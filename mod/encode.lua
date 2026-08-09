-- save-timelapse: pure binary-encoding helpers shared by snapshot export
-- (export.lua) and live event capture (capture.lua). Nothing here touches
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
  "character", "corpse", "fish",
  -- Flying robots, all of them. Same reasoning as the mobile enemies below:
  -- an airborne bot is an entity with a position that this format has no way
  -- to update, so it would be pinned wherever it happened to be at capture
  -- time while the real one flew on. Worse than the biter case for volume,
  -- though, and that is the main reason these are here: a megabase running a
  -- large construction job has tens of thousands of bots in the air at once,
  -- every one of them a record in the baseline snapshot and in every frame of
  -- a from-saves export, describing nothing about how the factory grew.
  --
  -- Roboports and the logistics network they form are of course kept: those
  -- are stationary infrastructure, and they are the part that actually shows
  -- the factory growing.
  "combat-robot", "construction-robot", "logistic-robot",
  -- Mobile enemies only. Biters and spitters are excluded for two reasons
  -- that both still hold: their combat deaths would flood live capture with
  -- removal events unrelated to construction, and this format records
  -- construction and destruction but never movement, so a captured biter
  -- would sit frozen wherever it was first logged while the real one walked
  -- away, which is worse than not drawing it at all.
  --
  -- Nests ("unit-spawner") are NOT excluded, despite being enemies too:
  -- they are stationary, so the format represents them honestly, and they
  -- are few enough to cost nothing. Clearing them is one of the things a
  -- player most wants to watch happen in a timelapse, since the front line
  -- moving outward is how expansion actually looks. Worm turrets are in for
  -- the same reason, and additionally could not be filtered safely anyway:
  -- they share the "turret" type with player turrets, so excluding them
  -- would mean name-sniffing and risking a real player entity.
  "unit",
  -- generic decorative/rock scatter, kept out for now: unlike trees and
  -- cliffs (rendered as ground context, see export.lua's terrain capture),
  -- this catch-all covers whatever else Space Age uses it for, which is
  -- harder to predict without the game's prototype data on hand.
  "simple-entity", "simple-entity-with-force", "simple-entity-with-owner",
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

-- Terrain capture bounding box
--
-- Natural terrain (grass, water, sand, ...) covers every generated tile on
-- a surface, not just where the player built, so capturing all of it would
-- dwarf everything else this mod exports. Instead, export.lua captures a
-- margin of ground around wherever entities and placed floor actually are,
-- tracked here as a running box grown one position at a time while those
-- are scanned, needing no second pass over the surface just to learn its
-- extent.

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

M.FRAME_MAGIC = "STF1"
M.EVENT_MAGIC = "STE1"

--- Independent per format, since the frame and event formats change on their
--- own schedules: a frame-only tweak has no reason to also bump the event
--- version, and vice versa.
--- Version 2 groups records into per-name runs and stores coordinates as
--- zigzag varint deltas, measured 4.7x smaller than version 1 on a real
--- frame. See src/frame.rs, which reads both.
M.FRAME_VERSION = 2
--- Version 2 adds the dictionary-reset record (tag 7, see
--- `event_reset_dictionaries`). A version 1 reader hitting that record stops
--- the stream rather than misreading it, since an unknown tag ends parsing,
--- so the bump is what turns "silently wrong from the reload onward" into a
--- refusal an older build can explain.
M.EVENT_VERSION = 2

--- JSON string quoting, still needed for the manifest files
--- (baseline.json, frame_<tick>_manifest.json): those stay JSON since
--- they're tiny, written once, and useful to read by eye, unlike the bulk
--- entity/tile/event data this file otherwise encodes as binary.
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

-- Name dictionaries
--
-- A name is written out in full the first time it is used (a "DefineName"
-- chunk) and given the next sequential id; every later reference to that
-- name is just the two byte id. This is what lets a frame or event segment
-- skip repeating "transport-belt" once per entity. The dictionary is a
-- plain table so the caller (export.lua, capture.lua, or snapshot.lua) can
-- decide its lifetime: one per frame file, or one per live-capture segment.

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

--- LEB128: seven bits per byte, high bit set while more follow, so a small
--- number costs one byte. Pure arithmetic, since Factorio's Lua 5.2 has no
--- bitwise operators, the same constraint u32le above works around.
function M.varint(n)
  local out = {}
  while n >= 128 do
    out[#out + 1] = string.char(n % 128 + 128)
    n = math.floor(n / 128)
  end
  out[#out + 1] = string.char(n)
  return table.concat(out)
end

--- Zigzag: maps small magnitudes to small unsigned values whichever side of
--- zero they are on, so a coordinate delta of -1 costs one byte rather than
--- ten. Needs no shifts or xor, which is just as well here: it is exactly
--- 2v for a non-negative v and -2v-1 otherwise.
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
--- Marks the end of the entity section and the start of the tile section.
--- There is no entity or tile count anywhere in this format: the periodic
--- incremental exporter writes the entity section across many ticks with
--- real play still running in between, so it cannot afford to also scan the
--- whole entity list upfront just to learn a count, and the count could
--- still be stale by the time writing finished anyway. A tile section needs
--- no equivalent marker: it is always the last thing in the file, so it
--- simply runs to the end.
--- Defines a name and the footprint every entity of that name shares.
---
--- Footprint belongs here rather than on each entity: it is a property of the
--- prototype, so an assembling machine repeating "3x3" on every one of
--- thousands of records was two bytes each spent restating a constant.
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

--- One run of same-named entities: the name id and count once for the group,
--- then each item's position as a delta from the one before it.
---
--- `items` are plain tables of `x`, `y` and `d` in world coordinates, already
--- read out of the API by the caller, so this stays pure and testable.
---
--- Deltas are against the previous item in the run rather than the origin,
--- and the caller's order is kept rather than sorted: a real export already
--- lays same-type entities out with enough locality for that to pay, and
--- sorting first measured only 0.3% better, which is not worth sorting every
--- entity mid-export.
---
--- The direction byte is carried per run, not per entity: whether a
--- prototype rotates is the same answer for every item in the group, so a run
--- of chests spends nothing on it.
function M.frame_entity_run(dict, name, w, h, items)
  local id, define = M.frame_define_name(dict, name, w, h)

  local directions = false
  for i = 1, #items do
    if (items[i].d or 0) ~= 0 then
      directions = true
      break
    end
  end

  local parts = { define, M.u8(1), M.varint(id), M.varint(#items), M.u8(directions and 1 or 0) }
  local px, py = 0, 0
  for i = 1, #items do
    local item = items[i]
    local x, y = M.round10(item.x), M.round10(item.y)
    parts[#parts + 1] = M.varint_i32(x - px) .. M.varint_i32(y - py)
    if directions then
      parts[#parts + 1] = M.u8(item.d or 0)
    end
    px, py = x, y
  end
  return table.concat(parts)
end

--- The tile section's equivalent. Tiles are integer aligned and always one
--- by one, but the footprint is still written into the definition so a name
--- record has one shape in both sections.
function M.frame_tile_run(dict, name, items)
  local id, define = M.frame_define_name(dict, name, 1, 1)
  local parts = { define, M.u8(2), M.varint(id), M.varint(#items) }
  local px, py = 0, 0
  for i = 1, #items do
    local item = items[i]
    parts[#parts + 1] = M.varint_i32(item.x - px) .. M.varint_i32(item.y - py)
    px, py = item.x, item.y
  end
  return table.concat(parts)
end

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

--- Tells a reader to forget every name and surface id defined so far in this
--- segment, so the ids that follow are read against a fresh dictionary.
---
--- Needed because Factorio re-runs the whole mod on every load, which resets
--- the writer's dictionaries to empty, while the segment file it is appending
--- to keeps every `DefineName` written before that point. Without this record
--- the writer hands out id 0 again for its next new name while the reader is
--- still counting up from the ids already in the file, and every entity and
--- tile logged after the reload decodes as whichever name happened to be
--- defined first. That was silent: nothing about the file looks damaged, the
--- names are simply wrong.
---
--- Cheaper than the alternatives. Persisting the dictionary in `storage` does
--- not work, since `storage` is saved inside the save file and so rewinds
--- with it, leaving it describing fewer names than the file already has.
--- Starting a fresh segment per load does not work either, for the same
--- reason: every counter available to name that segment lives in `storage`,
--- so loading one save twice would reuse a filename and overwrite the
--- sibling branch's history.
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

-- Detecting a reload from inside the mod is deliberately not attempted.
--
-- Every version of this file has had some form of "compare the tick play
-- resumed at against the last tick we recorded, and start a fresh segment if
-- it went backwards". That check can never fire. Both values come out of the
-- save being loaded: the recorded tick lives in `storage`, which Factorio
-- serializes into the save file, so a save made at tick T restores a recorded
-- tick no greater than T while `game.tick` is exactly T. The comparison is
-- always false, and the rollover it guarded never happened.
--
-- Nothing else in the Lua sandbox helps, since anything durable enough to
-- survive a load is also inside the save and rewinds with it.
--
-- So reloads are not handled here at all. They are handled where the evidence
-- actually exists, on the reading side: ticks that jump backwards inside one
-- segment mark where a reload happened, and `event::segment_run_bounds`
-- (src/event.rs) splits the segment there and discards the superseded
-- stretch. The one thing this side must still do is announce that its name
-- dictionaries were reset by the load, which `event_reset_dictionaries`
-- below covers.

-- Checksums
--
-- djb2, a simple multiplicative hash, computed with plain multiply/add/mod
-- rather than the usual bitwise XOR/shift form: Factorio's Lua 5.2 has no
-- bit32 library, the same reason u32le/i32le above pack integers by hand
-- instead of with bitwise ops. Not chosen for cryptographic strength, only
-- to catch accidental corruption, not resist tampering, but for being
-- trivial to implement identically on both this side and the Rust reader's.
-- `export.lua` (and `capture.lua`/`snapshot.lua` via it) threads the
-- running hash through every write for a frame file and appends it as a
-- trailer once the file is complete; nothing on the event log side uses
-- this, since an append-only segment that grows for as long as capture
-- stays on has no "finished" moment to checksum against.

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
-- game.tick restarts from 0 for every save, and script-output/save-timelapse/
-- is one folder shared by every save that ever turns capture on, so a raw
-- tick number cannot tell two playthroughs apart. session_id (the world's
-- map generation seed; see capture.lua) is stable across save/reload of one
-- playthrough and differs across different ones, so every playthrough this
-- mod ever captures gets its own subfolder, named after its session_id, of
-- otherwise plain (untagged) filenames. Factorio's write_file creates
-- whatever subfolders a path needs, so this needs nothing beyond naming the
-- path correctly. A folder per playthrough is easier to browse by hand than
-- the flat, hex-in-every-filename scheme this replaced, and removes the
-- need for anything reading this folder back to filter by session at all:
-- each folder only ever contains one playthrough's files to begin with.

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

function M.player_log_name(session_id)
  return M.session_dir(session_id) .. "players.jsonl"
end

--- Unlike the three names above, a baseline's per-surface frame files are
--- untagged even without a session_id (see export.lua's export_surface):
--- `/timelapse-export` and the headless scan share one private, per-run
--- script-output folder with nothing else, so there is nothing for their
--- output to collide with.
function M.frame_name(session_id, tick, surface)
  local name = string.format("frame_%d_%s.stfr", tick, surface)
  if not session_id then
    return name
  end
  return M.session_dir(session_id) .. name
end

-- Player position log
--
-- Deliberately plain newline-delimited JSON, not a tagged binary format
-- like the frame/event formats above: a position sample happens at most
-- once every several seconds by design (see export.lua), nowhere near
-- the per-tick construction volume that actually justified paying for a
-- binary format there. The same shape is both what the mod writes and
-- what the viewer reads directly (src/player_log.rs), so
-- save-timelapse.exe only ever relocates this file, never rewrites it.

--- One line: `{"tick":T,"players":[{"name":...,"surface":...,"x":...,"y":...}]}`.
--- `players` is a list of `{name=, surface=, x=, y=}` tables, already
--- resolved by the caller (export.lua has two: periodic sampling during
--- live capture, and a one-shot sample alongside every full export).
function M.player_log_line(tick, players)
  local entries = {}
  for i, p in pairs(players) do
    entries[i] = string.format('{"name":%s,"surface":%s,"x":%s,"y":%s}',
      M.quote(p.name), M.quote(p.surface), tostring(p.x), tostring(p.y))
  end
  return string.format('{"tick":%d,"players":[%s]}\n', tick, table.concat(entries, ","))
end

return M
