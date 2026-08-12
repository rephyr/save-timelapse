-- save-timelapse: binary encoding shared by export.lua and capture.lua.
-- Touches no Factorio API, so it runs under a plain Lua 5.2 interpreter, which
-- has no string.pack and no bitwise operators: every integer is packed by hand.
-- See docs/ARCHITECTURE.md for the wire format.

local M = {}

-- Types with no bearing on how a factory grew. Passed to
-- find_entities_filtered with invert, so these never cross the API boundary.
M.EXCLUDED_TYPES = {
  -- actors and their remains
  "character", "corpse", "fish",
  -- Flying robots. They move, and this format cannot say anything moved, so a
  -- captured bot would sit frozen wherever it was first logged. A megabase
  -- has tens of thousands airborne at once. Roboports stay.
  "combat-robot", "construction-robot", "logistic-robot",
  -- Biters and spitters: they move, and their combat deaths would flood the
  -- log with removals unrelated to construction. Nests ("unit-spawner") and
  -- worms are kept, being stationary and worth watching get cleared.
  "unit",
  -- Space Age's own mobile enemies, which "unit" does not cover: Gleba's
  -- stompers and strafers are "spider-unit" with "spider-leg" legs, and
  -- Vulcanus's demolishers a "segmented-unit" head trailing "segment"
  -- bodies. Spidertron is "spider-vehicle", so none of these can catch it.
  "spider-unit", "spider-leg", "segmented-unit", "segment",
  -- Vehicles and rolling stock, under the same rule. Trains are the worst
  -- case, since they never stop. Rails, signals and stations stay, being the
  -- stationary part that shows a network growing.
  "car", "spider-vehicle",
  "locomotive", "cargo-wagon", "fluid-wagon", "artillery-wagon",
  -- Generic decorative and rock scatter, unlike trees and cliffs, which are
  -- captured as ground context (see export.lua's terrain capture).
  "simple-entity", "simple-entity-with-force", "simple-entity-with-owner",
  -- transient visual effects
  "particle-source", "projectile", "explosion", "fire", "smoke",
  "smoke-with-trigger", "stream", "sticker", "beam",
  -- not yet real, or lying on the floor
  "entity-ghost", "tile-ghost", "item-entity",
  -- Asteroid chunks. They drift, and are collected rather than built, so
  -- every one logs a removal for something replay never had: 6,101 of 6,259
  -- events on a real five-platform capture.
  "asteroid-chunk",
}

--- Scenery types: entities the map generated rather than anybody placing, so
--- they sit on every generated chunk and are captured near the factory rather
--- than across the whole surface. Disjoint from `EXCLUDED_TYPES`, each entry
--- being gated on the setting that would otherwise exclude it. Worms cannot be
--- added, sharing the "turret" type with player turrets.
function M.context_types(include_resources, capture_terrain)
  local list = { "unit-spawner" }
  if include_resources then
    list[#list + 1] = "resource"
  end
  if capture_terrain then
    list[#list + 1] = "tree"
    list[#list + 1] = "cliff"
    -- Gleba's flora is type "plant", not "tree", the same split
    -- `export.excluded_types()` has to make.
    list[#list + 1] = "plant"
  end
  return list
end

-- Floor somebody laid, as opposed to ground the map generated. An include
-- list, natural terrain vastly outnumbering it.
--
-- Only names the game reports as neither placeable nor minable, which
-- `M.placed_floor_tiles` cannot find: the eleven coloured refined concretes.
-- The rest stay stated so a capture cannot silently lose floor it records.
M.KNOWN_PLACED_FLOOR_TILES = {
  "stone-path", "concrete",
  "hazard-concrete-left", "hazard-concrete-right",
  "refined-concrete", "refined-hazard-concrete-left", "refined-hazard-concrete-right",
  "landfill",
  "red-refined-concrete", "green-refined-concrete", "blue-refined-concrete",
  "orange-refined-concrete", "yellow-refined-concrete", "pink-refined-concrete",
  "purple-refined-concrete", "black-refined-concrete", "brown-refined-concrete",
  "cyan-refined-concrete", "acid-refined-concrete",
  "frozen-stone-path", "frozen-concrete",
  "frozen-hazard-concrete-left", "frozen-hazard-concrete-right",
  "frozen-refined-concrete",
  "frozen-refined-hazard-concrete-left", "frozen-refined-hazard-concrete-right",
}

--- Every tile this game counts as placed floor: placeable by an item, minable,
--- or named above. Neither property covers it alone, and the list alone was
--- Wube's names, so a platform's own foundation was recorded as natural ground
--- rather than as something built.
---
--- Only names this game has, these going to `find_tiles_filtered`.
function M.placed_floor_tiles()
  local known = {}
  for _, name in ipairs(M.KNOWN_PLACED_FLOOR_TILES) do
    known[name] = true
  end

  local found = {}
  for name, proto in pairs(prototypes.tile) do
    local items = proto.items_to_place_this
    local placeable = items ~= nil and #items > 0
    -- `mineable_properties` is always a table (unlike `items_to_place_this`,
    -- which is nil for a tile no item places), so `minable` reads directly.
    local minable = proto.mineable_properties.minable
    if placeable or minable or known[name] then
      found[#found + 1] = name
    end
  end
  -- Sorted so the list does not depend on `pairs` order, which Lua does not
  -- promise and which would otherwise vary between two runs of one save.
  table.sort(found)
  return found
end

-- Terrain capture bounding box
--
-- Ground covers every generated tile, so export.lua captures only a margin
-- around wherever entities and placed floor are. The box grows one position at
-- a time while those are scanned, needing no second pass.

--- `nil` fields mean "nothing seen yet": an untouched surface has no
--- factory to show context around, distinct from a box that happens to be
--- a single point.
function M.new_bbox()
  return { min_x = nil, min_y = nil, max_x = nil, max_y = nil }
end

--- Mutates `bbox` in place (a plain table, not a value like the checksum
--- above) and returns it, so a caller can chain `grow_bbox(grow_bbox(...))`
--- if that reads better at a given call site.
function M.grow_bbox(bbox, x, y)
  if not bbox.min_x or x < bbox.min_x then bbox.min_x = x end
  if not bbox.max_x or x > bbox.max_x then bbox.max_x = x end
  if not bbox.min_y or y < bbox.min_y then bbox.min_y = y end
  if not bbox.max_y or y > bbox.max_y then bbox.max_y = y end
  return bbox
end

--- `nil` if `bbox` never saw a position. Otherwise a Factorio BoundingBox
--- (the shape `LuaSurface.find_tiles_filtered`'s `area` argument expects)
--- padded by `margin` tiles on every side.
function M.expand_bbox(bbox, margin)
  if not bbox.min_x then
    return nil
  end
  return {
    { bbox.min_x - margin, bbox.min_y - margin },
    { bbox.max_x + margin, bbox.max_y + margin },
  }
end

--- The frame shape `M.terrain_margin` assumes a timelapse gets watched at.
--- 16:9 is save-timelapse.exe's default export size and every resolution
--- preset it offers, and it is what a video gets played back in regardless.
local TERRAIN_VIEW_ASPECT = 16 / 9

--- Matches `AUTO_FOLLOW_FIT_MARGIN` in `viewer/src/main.rs`: how much smaller
--- than edge to edge the export camera fits the base. The margin below is
--- derived from what that fit exposes, so the two have to agree.
local TERRAIN_VIEW_FIT = 0.92

--- Ceiling on the region ground is captured over, in tiles. 4M is 2000x2000,
--- covering an ordinary base's 16:9 framing without engaging. A floor rather
--- than the whole budget: as a fixed ceiling it inverted on any base larger
--- than itself, leaving nothing to spend.
local TERRAIN_MAX_TILES = 4000000

--- Past that size the budget scales with the factory: four times its own
--- footprint, which is what a square base needs. Solving the area cap for a
--- square gives `(sqrt(k) - 1) / 2` per side, so k=4 is the half width a 16:9
--- frame exposes. Elongated bases stay bounded, the cap being on area.
local TERRAIN_BUDGET_MULTIPLE = 4

--- How far past `bbox` to capture ground, in tiles, never less than
--- `base_margin`.
---
--- A fit leaves the difference between the base's shape and the frame's as
--- empty world on whichever axis does not bind, so the margin is what the fit
--- exposes rather than a fraction of the base, which is an order of magnitude
--- out on anything not square. The first two candidates go negative when their
--- axis binds, so the largest picks the right one.
function M.terrain_margin(bbox, base_margin)
  if not bbox.min_x then
    return base_margin
  end
  local w = bbox.max_x - bbox.min_x
  local h = bbox.max_y - bbox.min_y
  local slack = 1 / TERRAIN_VIEW_FIT

  local wanted = math.max(
    base_margin,
    (h * TERRAIN_VIEW_ASPECT * slack - w) / 2,
    (w * slack / TERRAIN_VIEW_ASPECT - h) / 2,
    math.max(w, h) * (slack - 1) / 2
  )

  -- At least the flat budget, and at least a multiple of what the factory
  -- itself occupies, so the allowance grows with the base rather than being
  -- swallowed by it.
  local budget = math.max(TERRAIN_MAX_TILES, TERRAIN_BUDGET_MULTIPLE * w * h)

  -- Largest margin keeping (w + 2m)(h + 2m) within that budget, i.e. the
  -- positive root of 4m^2 + 2m(w + h) + wh - budget = 0.
  local affordable = (math.sqrt((w - h) * (w - h) + 4 * budget) - (w + h)) / 4

  return math.floor(math.max(base_margin, math.min(wanted, affordable)))
end

M.FRAME_MAGIC = "STF1"
M.EVENT_MAGIC = "STE1"

--- Independent per format, the two changing on their own schedules.
--- Version 2 groups records into per-name runs with zigzag varint deltas, 4.7x
--- smaller than version 1. Version 3 writes byte for byte what 2 does and only
--- declares that extension records may appear, so a tool predating them
--- refuses the file rather than desynchronising.
M.FRAME_VERSION = 3
--- Version 2 adds the dictionary-reset record (tag 7, see
--- `event_reset_dictionaries`), which a version 1 reader would end the stream
--- on rather than misread. Version 3 declares extension records, as above.
M.EVENT_VERSION = 3

--- JSON string quoting, for the manifests (baseline.json,
--- frame_<tick>_manifest.json). Those stay JSON: tiny, written once, and
--- useful to read by eye.
function M.quote(text)
  return '"' .. text:gsub('[\\"]', '\\%0') .. '"'
end

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

--- Position times ten, rounded away from zero: entities align to a tenth of a
--- tile (see world.rs::pos_key), and this is the fixed point form the wire
--- stores it in.
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

-- Name dictionaries
--
-- A name is written in full the first time it is used and given the next
-- sequential id; later references are just the id. A plain table, so the
-- caller decides the lifetime: one per frame file, or one per segment.

function M.new_dictionary()
  return { ids = {}, count = 0 }
end

--- Returns the id for `name`, plus a define chunk to prepend on first use.
--- Concatenate both onto the record that follows: the dictionary is a stream
--- position, not something written separately. `define_tag` is 0 for names and
--- 1 for the event format's surface dictionary.
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

--- LEB128: seven bits per byte, high bit set while more follow, so a small
--- number costs one byte. Pure arithmetic, since Factorio's Lua 5.2 has no
--- bitwise operators, the same constraint u32le above works around.
function M.varint(n)
  -- One and two byte values are almost everything this sees, and spelling
  -- them out avoids building and concatenating a table per call.
  if n < 128 then
    return string.char(n)
  end
  if n < 16384 then
    return string.char(n % 128 + 128, math.floor(n / 128))
  end

  local out = {}
  while n >= 128 do
    out[#out + 1] = string.char(n % 128 + 128)
    n = math.floor(n / 128)
  end
  out[#out + 1] = string.char(n)
  return table.concat(out)
end

--- Zigzag: maps small magnitudes to small unsigned values on either side of
--- zero, so a delta of -1 costs one byte. 2v for non-negative v, else -2v-1.
function M.zigzag(v)
  if v >= 0 then
    return 2 * v
  end
  return -2 * v - 1
end

function M.varint_i32(v)
  return M.varint(M.zigzag(v))
end

-- Frame format (frame_<tick>_<surface>.stfr)

function M.frame_header(tick, surface)
  return M.FRAME_MAGIC .. M.u8(M.FRAME_VERSION) .. M.u64le(tick) .. M.str(surface)
end

--- Defines a name and the footprint every entity of that name shares.
--- Footprint is a property of the prototype, not of each record.
function M.frame_define_name(dict, name, w, h)
  local id = dict.ids[name]
  if id then
    return id, ""
  end
  id = dict.count
  dict.count = id + 1
  dict.ids[name] = id
  return id, M.u8(0) .. M.str(name) .. M.u8(clamp_u8(w or 1)) .. M.u8(clamp_u8(h or 1))
end

--- One run of same-named entities: name id and count once, then each position
--- as a delta from the one before.
---
--- Parallel arrays rather than a table per entity, 1.26x slower at 900k
--- entities against 0.60x here. Caller order is kept, a real export already
--- having the locality. The direction byte is per run.
function M.frame_entity_run(dict, name, w, h, xs, ys, ds, count)
  local id, define = M.frame_define_name(dict, name, w, h)

  local directions = false
  for i = 1, count do
    if ds[i] ~= 0 then
      directions = true
      break
    end
  end

  local parts = { define, M.u8(1), M.varint(id), M.varint(count), M.u8(directions and 1 or 0) }
  local n = 5
  local px, py = 0, 0
  for i = 1, count do
    local x, y = M.round10(xs[i]), M.round10(ys[i])
    -- Appended separately rather than concatenated into one string: that
    -- intermediate was one more allocation per entity, and table.concat at
    -- the end joins them just as well.
    parts[n + 1] = M.varint_i32(x - px)
    parts[n + 2] = M.varint_i32(y - py)
    n = n + 2
    if directions then
      n = n + 1
      parts[n] = M.u8(ds[i])
    end
    px, py = x, y
  end
  return table.concat(parts)
end

--- The tile section's equivalent. Tiles are integer aligned and always one
--- by one, but the footprint is still written into the definition so a name
--- record has one shape in both sections.
function M.frame_tile_run(dict, name, xs, ys, count)
  local id, define = M.frame_define_name(dict, name, 1, 1)
  local parts = { define, M.u8(2), M.varint(id), M.varint(count) }
  local n = 4
  local px, py = 0, 0
  for i = 1, count do
    local x, y = xs[i], ys[i]
    parts[n + 1] = M.varint_i32(x - px)
    parts[n + 2] = M.varint_i32(y - py)
    n = n + 2
    px, py = x, y
  end
  return table.concat(parts)
end

--- Marks the end of the entity section and the start of the tile section.
--- Neither carries a count: the incremental exporter writes entities across
--- many ticks and cannot scan for one first. The tile section is last, so it
--- simply runs to the end of the file.
function M.frame_end_entities()
  return M.u8(9)
end

-- Live capture event format (events_<start_tick>.stev)

function M.event_header()
  return M.EVENT_MAGIC .. M.u8(M.EVENT_VERSION)
end

--- Emitted once per distinct tick that has at least one event, rather than
--- on every record, since many events (a blueprint landing hundreds of
--- entities) usually share a tick.
function M.event_set_tick(tick)
  return M.u8(2) .. M.u64le(tick)
end

--- Tells a reader to forget every id defined so far in this segment.
---
--- Factorio re-runs the mod on every load, resetting the writer's dictionaries
--- while the file keeps every define already in it, so without this the writer
--- reissues id 0 while the reader keeps counting and everything after the
--- reload decodes as the wrong name, silently. `storage` cannot hold the
--- dictionary instead, rewinding with the save.
function M.event_reset_dictionaries()
  return M.u8(7)
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

--- Names the entity a following removal is for, as an extension record so the
--- frozen core layout is untouched and an older tool steps over it.
---
--- Only ever written for a resource. A position holds at most a deposit and
--- the thing standing on it, and a removal carrying only a position resolves
--- to whatever is on top, which is the structure; without this, hand-mining
--- the ore under a machine took the machine instead. Nothing else can be the
--- buried one, so nothing else needs saying.
function M.event_remove_name(names, name)
  local name_id, define_name = M.dictionary_id(names, name, 0)
  local payload = M.varint(name_id)
  return define_name .. M.u8(128) .. M.varint(#payload) .. payload
end

--- Position is sent even when `id` is available: a baseline entity has no
--- recorded id, so `id` alone would make its removal an unresolvable no-op.
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

-- Reloads are not detected here, and cannot be: every durable value lives in
-- `storage`, which rewinds with the save, so any "did the tick go backwards"
-- check compares two values from the same save. They are handled on the
-- reading side by `event::segment_run_bounds`; this side only announces that
-- its dictionaries were reset.

-- Checksums
--
-- djb2, with multiply/add/mod rather than XOR/shift, Lua 5.2 having no bit32.
-- For accidental corruption, not tampering. Threaded through every write of a
-- frame file and appended as a trailer; the event log has no finished moment.

--- Pure: plain values in and out, so these are testable the same way as
--- event_reset_dictionaries above.
function M.checksum_init()
  return 5381
end

--- `data` is hashed byte by byte and folded into `hash`, wrapping at 2^32
--- the same way Rust's `u32::wrapping_mul`/`wrapping_add` do, so the two
--- sides agree on every input without either needing the other's runtime.
function M.checksum_update(hash, data)
  for i = 1, #data do
    hash = (hash * 33 + string.byte(data, i)) % 4294967296
  end
  return hash
end

-- Per-playthrough file naming
--
-- game.tick restarts from 0 for every save and script-output/save-timelapse/ is
-- shared by every save that turns capture on, so a tick cannot tell two
-- playthroughs apart. Each gets a subfolder named after its session_id.

--- Pure: plain values in and out, so these are testable the same way as
--- event_reset_dictionaries above, with no save/load cycle to trigger them.
function M.session_dir(session_id)
  return string.format("%08x/", session_id)
end

function M.baseline_manifest_name(session_id)
  return M.session_dir(session_id) .. "baseline.json"
end

function M.capture_segment_name(session_id, start_tick)
  return M.session_dir(session_id) .. M.capture_segment_basename(start_tick)
end

--- Split out from `capture_segment_name` so a save with no session_id (one
--- whose capture state predates session folders) can build the same name
--- without one. See capture.lua's `capture_segment_path`.
function M.capture_segment_basename(start_tick)
  return string.format("events_%d.stev", start_tick)
end

function M.milestone_name(session_id)
  return M.session_dir(session_id) .. "milestones.jsonl"
end

function M.player_log_name(session_id)
  return M.session_dir(session_id) .. "players.jsonl"
end

function M.prototypes_name(session_id)
  return M.session_dir(session_id) .. "prototypes.json"
end

--- Prototype types that are enemies whatever force they end up on, and so take
--- `enemy_map_color`.
---
--- Force belongs to an entity, not a prototype, and this file is keyed by
--- prototype name. Type is the closest the game comes and for a capture it is
--- exact. `turret` is the type worms use; the player's defences are
--- `ammo-turret`, `electric-turret` and `fluid-turret`.
local ENEMY_TYPES = {
  ["unit"] = true,
  ["unit-spawner"] = true,
  ["turret"] = true,
  ["spider-unit"] = true,
  ["spider-leg"] = true,
  ["segmented-unit"] = true,
  ["segment"] = true,
}

--- Rounds to a whole byte and holds it there. Unlike `clamp_u8`, which guards
--- a footprint that is a positive integer already, this takes an arbitrary
--- float off a prototype and has both ends to defend.
local function clamp_byte(v)
  v = math.floor(v + 0.5)
  if v < 0 then
    return 0
  end
  if v > 255 then
    return 255
  end
  return v
end

--- One prototype colour as three bytes.
---
--- Factorio writes a Color as 0..1 floats or 0..255 values and tells them apart
--- by whether any component exceeds 1. Prototypes mostly use the second form,
--- base's own tiles included (`grass-1` is {55, 53, 11}), and the runtime
--- returns them as written, so assuming floats puts every colour out of byte
--- range and the file becomes unreadable rather than merely wrong. Clamped as
--- well as rounded, the range rule being a convention.
function M.color_bytes(color)
  local r, g, b = color.r or 0, color.g or 0, color.b or 0
  local scale = 255
  if r > 1 or g > 1 or b > 1 then
    scale = 1
  end
  return clamp_byte(r * scale), clamp_byte(g * scale), clamp_byte(b * scale)
end

--- The types that have an underground reach to report.
--- `max_underground_distance` is documented optional and returns nil for
--- everything else, but this file is written inside a `pcall` that turns any
--- raise into no file at all, so only the two types that have one are asked.
local REACH_TYPES = {
  ["underground-belt"] = true,
  ["pipe-to-ground"] = true,
}

--- Everything the desktop side needs to know about this game's prototypes, so
--- it never has to recognise one by name: colours, each entity's type, and how
--- far an underground belt reaches. None of it exists outside the running game,
--- a mod shipping as a zip. Rewritten only when the loaded mods change (see
--- capture.lua's `loaded_mods`).
---
--- Entities take `map_color` and fall back to `friendly_map_color`, with nests
--- taking `enemy_map_color`. The two are mutually exclusive per prototype,
--- `map_color` being what charting uses "if a friendly or enemy color isn't
--- defined".
function M.prototypes_json()
  local parts = {}
  local function add(text)
    parts[#parts + 1] = text
  end
  local function add_color(name, color)
    if not color then
      return
    end
    local r, g, b = M.color_bytes(color)
    add(string.format('%q:[%d,%d,%d]', name, r, g, b))
  end

  local tiles = {}
  for name, proto in pairs(prototypes.tile) do
    tiles[#tiles + 1] = { name = name, color = proto.map_color }
  end
  table.sort(tiles, function(a, b) return a.name < b.name end)

  local entities = {}
  for name, proto in pairs(prototypes.entity) do
    local color
    if ENEMY_TYPES[proto.type] then
      color = proto.enemy_map_color or proto.map_color
    else
      color = proto.map_color or proto.friendly_map_color
    end
    entities[#entities + 1] = {
      name = name,
      color = color,
      -- The prototype's own type, verbatim. Filtering here would only move
      -- the curated list to the other side of the file.
      kind = proto.type,
      reach = REACH_TYPES[proto.type] and proto.max_underground_distance or nil,
    }
  end
  table.sort(entities, function(a, b) return a.name < b.name end)

  -- Each section's entries are built into the shared `parts` buffer and then
  -- joined out of it by range, so a section costs one concat rather than a
  -- table of its own.
  local out = {}
  local function section(prefix, items, emit)
    out[#out + 1] = prefix
    local first = #parts
    for _, item in ipairs(items) do
      emit(item)
    end
    out[#out + 1] = table.concat(parts, ",", first + 1, #parts)
  end

  section('{"tiles":{', tiles, function(t) add_color(t.name, t.color) end)
  section('},"entities":{', entities, function(e) add_color(e.name, e.color) end)
  section('},"types":{', entities, function(e)
    if e.kind then
      add(string.format('%q:%q', e.name, e.kind))
    end
  end)
  section('},"reach":{', entities, function(e)
    if e.reach then
      add(string.format('%q:%d', e.name, e.reach))
    end
  end)

  -- Which tiles this capture treats as placed floor, so the desktop side
  -- splits a baseline's tiles the way this mod recorded them. It kept a copy
  -- of the old list for that, which cannot agree once this side works the
  -- answer out per game. An array, since this says nothing per name.
  out[#out + 1] = '},"floor":['
  local first = #parts
  for _, name in ipairs(M.placed_floor_tiles()) do
    add(string.format('%q', name))
  end
  out[#out + 1] = table.concat(parts, ",", first + 1, #parts)
  out[#out + 1] = ']}'
  return table.concat(out)
end

--- Untagged without a session_id: `/timelapse-export` and the headless scan
--- each own a private script-output folder with nothing to collide with.
function M.frame_name(session_id, tick, surface)
  local name = string.format("frame_%d_%s.stfr", tick, surface)
  if not session_id then
    return name
  end
  return M.session_dir(session_id) .. name
end

--- One file per surface for a whole capture rather than one per tick, so no
--- tick in the name. Session tagged like everything else, which is how the
--- desktop tool tells whether the save it scanned was the right one.
function M.terrain_name(session_id, surface)
  local name = string.format("terrain_%s.stfr", surface)
  if not session_id then
    return name
  end
  return M.session_dir(session_id) .. name
end

-- Milestones
--
-- Moments worth marking on the timeline: the first of each science pack, the
-- first rocket, the first visit to each planet. Newline-delimited JSON, a
-- playthrough producing about a dozen.

--- One milestone: `{"tick":T,"kind":K,"id":I}`. Kind and id rather than a
--- prebaked sentence, so the viewer decides the wording and can filter.
function M.milestone_line(tick, kind, id)
  return string.format('{"tick":%d,"kind":%s,"id":%s}\n', tick, M.quote(kind), M.quote(id))
end

--- Whether `name` is a science pack, by suffix rather than a fixed list, so a
--- modded pack is picked up for free. Only ever asked about item names, which
--- is what makes the suffix safe: the `science-pack` subgroup and the
--- `signal-science-pack` signal are not items.
function M.is_science_pack(name)
  return name:sub(-13) == "-science-pack"
end

--- What one save can say about milestones, for the export manifest:
--- `{"science":[...],"planets":[...],"rockets":N}`.
---
--- State, not events: a save knows a pack has been produced, never when.
--- src/milestone.rs recovers timing by diffing consecutive saves, and rockets
--- is a count rather than a flag so that diff can tell the first from the
--- hundredth.
function M.milestone_state(science, planets, rockets)
  local quoted_science, quoted_planets = {}, {}
  for i, name in ipairs(science) do
    quoted_science[i] = M.quote(name)
  end
  for i, name in ipairs(planets) do
    quoted_planets[i] = M.quote(name)
  end
  return string.format('{"science":[%s],"planets":[%s],"rockets":%d}',
    table.concat(quoted_science, ","), table.concat(quoted_planets, ","), rockets)
end

-- Player position log
--
-- Newline-delimited JSON like milestones, a sample happening at most every few
-- seconds. The same shape the viewer reads, so save-timelapse.exe relocates
-- the file rather than converting it.

--- One line: `{"tick":T,"players":[{"name":...,"surface":...,"x":...,"y":...}]}`,
--- with `players` already resolved by the caller.
function M.player_log_line(tick, players)
  local entries = {}
  for i, p in pairs(players) do
    entries[i] = string.format('{"name":%s,"surface":%s,"x":%s,"y":%s}',
      M.quote(p.name), M.quote(p.surface), tostring(p.x), tostring(p.y))
  end
  return string.format('{"tick":%d,"players":[%s]}\n', tick, table.concat(entries, ","))
end

return M
