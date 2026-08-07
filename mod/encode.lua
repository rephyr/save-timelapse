-- save-timelapse: pure binary-encoding helpers shared by snapshot export
-- (control.lua) and live event capture. Nothing here touches
-- game/surface/settings/helpers, so this file loads and runs under a plain
-- `lua` interpreter with no Factorio present. See tests/encode_test.lua.
--
-- Factorio's modding Lua is version 5.2, which has no string.pack/unpack (a
-- 5.3 feature), so every integer here is packed by hand from string.char and
-- arithmetic. Lua's `%` and `math.floor` are floor based, which turns out to
-- produce the correct little endian two's complement bytes for a negative
-- number with no separate "add 2^32" step first: for example -805 packed as
-- a 4 byte integer below comes out as DB FC FF FF, matching a real signed
-- 32 bit two's complement encoding. See the "negative numbers" tests.
--
-- See docs/ARCHITECTURE.md for the full wire format this file implements.

local M = {}

-- Types with no bearing on how a factory grew. Passed to
-- find_entities_filtered with invert, so these never cross the API boundary.
M.EXCLUDED_TYPES = {
  -- actors and their remains
  "character", "corpse", "combat-robot", "fish",
  -- enemies: wildlife rather than factory, and their deaths in combat would
  -- otherwise flood live capture with removal events unrelated to
  -- construction. Worm turrets are left in: they share the "turret" type
  -- with player turrets, and are stationary and comparatively few, so
  -- filtering them would risk excluding a real player entity by name-sniffing
  -- instead of type.
  "unit", "unit-spawner",
  -- terrain scatter
  "tree", "simple-entity", "simple-entity-with-force", "simple-entity-with-owner",
  "cliff",
  -- transient visual effects
  "particle-source", "projectile", "explosion", "fire", "smoke",
  "smoke-with-trigger", "stream", "sticker", "beam",
  -- not yet real, or lying on the floor
  "entity-ghost", "tile-ghost", "item-entity",
}

-- Natural terrain (grass, water, sand, dirt, ...) vastly outnumbers placed
-- floor types, so this is an include list rather than an exclude list, the
-- opposite of EXCLUDED_TYPES above. Verified against base/prototypes/tile/tiles.lua.
M.PLACED_FLOOR_TILES = {
  "stone-path", "concrete",
  "hazard-concrete-left", "hazard-concrete-right",
  "refined-concrete", "refined-hazard-concrete-left", "refined-hazard-concrete-right",
  "landfill",
  -- Space Age colored refined-concrete variants
  "red-refined-concrete", "green-refined-concrete", "blue-refined-concrete",
  "orange-refined-concrete", "yellow-refined-concrete", "pink-refined-concrete",
  "purple-refined-concrete", "black-refined-concrete", "brown-refined-concrete",
  "cyan-refined-concrete", "acid-refined-concrete",
}

M.FRAME_MAGIC = "STF1"
M.EVENT_MAGIC = "STE1"

--- JSON string quoting, still needed for the manifest files
--- (baseline.json, frame_<tick>_manifest.json): those stay JSON since
--- they're tiny, written once, and useful to read by eye, unlike the bulk
--- entity/tile/event data this file otherwise encodes as binary.
function M.quote(text)
  return '"' .. text:gsub('[\\"]', '\\%0') .. '"'
end

-- ---------------------------------------------------------------------------
-- Low level byte packing

function M.u8(n)
  return string.char(n % 256)
end

function M.u16le(n)
  local b0 = n % 256
  n = math.floor(n / 256)
  local b1 = n % 256
  return string.char(b0, b1)
end

--- Also used for signed 32 bit values: bytes are bytes, and Lua's floor
--- based `%`/`math.floor` produce the right two's complement result for a
--- negative `n` without any extra handling. See the module comment above.
function M.u32le(n)
  local b0 = n % 256
  n = math.floor(n / 256)
  local b1 = n % 256
  n = math.floor(n / 256)
  local b2 = n % 256
  n = math.floor(n / 256)
  local b3 = n % 256
  return string.char(b0, b1, b2, b3)
end

M.i32le = M.u32le

function M.u64le(n)
  local bytes = {}
  for i = 1, 8 do
    bytes[i] = n % 256
    n = math.floor(n / 256)
  end
  return string.char(bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8])
end

--- A u16 length prefix followed by the string's bytes. Prototype and surface
--- names are always short, so a u16 length leaves plenty of headroom without
--- spending 4 bytes on every single one.
function M.str(s)
  return M.u16le(#s) .. s
end

--- Position times ten, rounded to the nearest integer: entities are aligned
--- to a tenth of a tile (see world.rs::pos_key on the Rust side), and this
--- is the fixed point form that alignment is stored in on the wire.
--- Round-half-away-from-zero, matching what the old "%.1f" text formatting
--- produced when read back as a number.
function M.round10(v)
  if v >= 0 then
    return math.floor(v * 10 + 0.5)
  end
  return -math.floor(-v * 10 + 0.5)
end

local function clamp_u8(n)
  if n > 255 then
    return 255
  end
  return n
end

-- ---------------------------------------------------------------------------
-- Name dictionaries
--
-- A name is written out in full the first time it is used (a "DefineName"
-- chunk) and given the next sequential id; every later reference to that
-- name is just the two byte id. This is what lets a frame or event segment
-- skip repeating "transport-belt" once per entity. The dictionary is a
-- plain table so the caller (control.lua) can decide its lifetime: one per
-- frame file, or one per live-capture segment.

function M.new_dictionary()
  return { ids = {}, count = 0 }
end

--- Returns the id for `name` in `dict`, plus a define chunk to prepend if
--- this is the first time `dict` has seen it, or "" otherwise. Concatenate
--- the two returned pieces directly onto whatever record follows: the
--- dictionary is a stream position, not something written separately.
---
--- `define_tag` is the tag byte the define chunk uses: 0 (DefineName) for
--- both the frame format's shared dictionary and the event format's name
--- dictionary, but 1 (DefineSurface) for the event format's surface
--- dictionary, since those are two different tags on that stream. The
--- dictionary itself doesn't know which one it is; the caller does.
function M.dictionary_id(dict, name, define_tag)
  local id = dict.ids[name]
  if id then
    return id, ""
  end
  id = dict.count
  dict.count = id + 1
  dict.ids[name] = id
  return id, M.u8(define_tag) .. M.str(name)
end

-- ---------------------------------------------------------------------------
-- Frame format (frame_<tick>_<surface>.stfr)

function M.frame_header(tick, surface)
  return M.FRAME_MAGIC .. M.u64le(tick) .. M.str(surface)
end

--- One entity, as a "DefineName" chunk (if needed) followed by tag 1 and its
--- fixed size record. `direction`/`tile_width`/`tile_height` are always
--- written now rather than omitted at their default: once a record is this
--- compact, a variable width encoding to skip a default value costs more
--- complexity than the bytes it would save.
function M.frame_entity_record(dict, entity)
  local id, define = M.dictionary_id(dict, entity.name, 0)
  local pos = entity.position
  local direction = entity.direction or 0
  local w = clamp_u8(entity.tile_width or 1)
  local h = clamp_u8(entity.tile_height or 1)
  return define
    .. M.u8(1)
    .. M.u16le(id)
    .. M.i32le(M.round10(pos.x))
    .. M.i32le(M.round10(pos.y))
    .. M.u8(direction)
    .. M.u8(w)
    .. M.u8(h)
end

--- Tiles are corner positioned and integer aligned, unlike entities.
function M.frame_tile_record(dict, tile)
  local id, define = M.dictionary_id(dict, tile.name, 0)
  local pos = tile.position
  return define .. M.u8(2) .. M.u16le(id) .. M.i32le(pos.x) .. M.i32le(pos.y)
end

--- Marks the end of the entity section and the start of the tile section.
--- There is no entity or tile count anywhere in this format: the periodic
--- incremental exporter writes the entity section across many ticks with
--- real play still running in between, so it cannot afford to also scan the
--- whole entity list upfront just to learn a count, and the count could
--- still be stale by the time writing finished anyway. A tile section needs
--- no equivalent marker: it is always the last thing in the file, so it
--- simply runs to the end.
function M.frame_end_entities()
  return M.u8(9)
end

-- ---------------------------------------------------------------------------
-- Live capture event format (events_<start_tick>.stev)

function M.event_header()
  return M.EVENT_MAGIC
end

--- Emitted once per distinct tick that has at least one event, rather than
--- on every record, since many events (a blueprint landing hundreds of
--- entities) usually share a tick.
function M.event_set_tick(tick)
  return M.u8(2) .. M.u64le(tick)
end

--- `id` of nil or 0 means the add carries no unit_number (some entity kinds
--- have none); `w`/`h` default to 1 and `direction` to 0, the same as the
--- frame format.
function M.event_add_entity(names, surfaces, surface, name, x, y, direction, id, w, h)
  local surface_id, define_surface = M.dictionary_id(surfaces, surface, 1)
  local name_id, define_name = M.dictionary_id(names, name, 0)
  return define_surface
    .. define_name
    .. M.u8(3)
    .. M.u16le(name_id)
    .. M.i32le(M.round10(x))
    .. M.i32le(M.round10(y))
    .. M.u8(direction or 0)
    .. M.u8(clamp_u8(w or 1))
    .. M.u8(clamp_u8(h or 1))
    .. M.u64le(id or 0)
    .. M.u16le(surface_id)
end

--- Position is always sent, even when `id` is also available: an entity that
--- already existed when the baseline was taken carries its real
--- unit_number when Factorio later reports it mined or destroyed, but a
--- snapshot records no ids, so replay's world state never learned that
--- number belongs to that entity. `id` alone would make every such removal
--- an unresolvable no-op; position is what actually finds it.
function M.event_remove_entity(surfaces, surface, x, y, id)
  local surface_id, define_surface = M.dictionary_id(surfaces, surface, 1)
  return define_surface
    .. M.u8(4)
    .. M.i32le(M.round10(x))
    .. M.i32le(M.round10(y))
    .. M.u64le(id or 0)
    .. M.u16le(surface_id)
end

function M.event_add_tile(names, surfaces, surface, name, x, y)
  local surface_id, define_surface = M.dictionary_id(surfaces, surface, 1)
  local name_id, define_name = M.dictionary_id(names, name, 0)
  return define_surface .. define_name .. M.u8(5) .. M.u16le(name_id) .. M.i32le(x) .. M.i32le(y) .. M.u16le(surface_id)
end

function M.event_remove_tile(surfaces, surface, x, y)
  local surface_id, define_surface = M.dictionary_id(surfaces, surface, 1)
  return define_surface .. M.u8(6) .. M.i32le(x) .. M.i32le(y) .. M.u16le(surface_id)
end

--- Given the tick already recorded up to and the tick play resumed at,
--- decide whether this is a reload of an older save, something an
--- append-only log can't represent as one timeline, and if so, what
--- segment to start. Pure: plain values in and out, no Factorio state, so
--- the decision is testable without a save/load cycle to actually trigger.
function M.next_capture_segment(last_tick, resumed_tick, current_segment_start)
  if resumed_tick < last_tick then
    return resumed_tick -- rolled back: start a fresh segment here
  end
  return current_segment_start -- keep appending to the existing segment
end

return M
