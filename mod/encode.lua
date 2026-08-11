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
  -- Space Age's own mobile enemies, which "unit" above does not cover
  -- because they were given prototype types of their own. Gleba's stompers
  -- and strafers are "spider-unit" and their legs "spider-leg"; Vulcanus's
  -- demolishers are a "segmented-unit" head trailing "segment" bodies.
  -- Only Gleba's wrigglers are plain "unit".
  --
  -- Found by reading a real capture rather than by reasoning: a Gleba
  -- timelapse held small-stomper-pentapod and small-strafer-pentapod
  -- alongside both their leg prototypes, every one of them logged as though
  -- somebody had built it. They roam, so the auto-follow camera stretched
  -- to cover wherever they had wandered and the factory rendered as a
  -- smudge in the middle of untouched jungle.
  --
  -- "spider-unit" cannot catch a player entity: Spidertron is
  -- "spider-vehicle", a separate type (base/prototypes/entity/entities.lua).
  -- "spider-leg" does also cover Spidertron's own legs, which is correct
  -- for the same reason everything else here is excluded.
  "spider-unit", "spider-leg", "segmented-unit", "segment",
  -- Vehicles and rolling stock. The rule that put every one of the above
  -- here applies to these just as squarely, and they were simply missed:
  -- "car" (cars and tanks), "spider-vehicle" (Spidertron) and the four
  -- rolling stock types all move, and this format cannot say that anything
  -- moved.
  --
  -- Trains are the worst of them, because they never stop. A from-saves
  -- export catches them somewhere different in every save, so they blink
  -- around the rail network frame by frame; a live capture logs one add
  -- where the locomotive was placed and then shows it parked there for the
  -- rest of the playthrough. Rails, signals and stations stay, being the
  -- stationary infrastructure that actually shows the network growing,
  -- exactly as roboports stay while the robots do not.
  "car", "spider-vehicle",
  "locomotive", "cargo-wagon", "fluid-wagon", "artillery-wagon",
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
  -- Asteroid chunks. Same reasoning as biters and bots, and for the same two
  -- reasons: they drift, so a format that records placement and removal but
  -- never movement would pin one wherever it was first seen while the real
  -- one moved on, and a platform collects them continuously.
  --
  -- The volume is the part that actually forced this. A chunk is never
  -- *built*, only collected, so every one produces a removal for something
  -- the replay never had: on a real capture with five platforms running,
  -- 6,101 of 6,259 logged events were exactly that, and the replay warned
  -- that almost nothing it read did anything. Collectors, silos and the rest
  -- of a platform stay, since those are the structures worth watching go up.
  "asteroid-chunk",
}

--- Scenery types, given the two settings that decide which of them are
--- recorded at all. Entities the map generated rather than anybody placing,
--- so they sit on every generated chunk regardless of where the factory is.
---
--- The point of naming them is that they are captured *near the factory*
--- rather than across the whole surface, exactly like the natural ground
--- they stand on. See `export.lua`'s bounded pass.
---
--- Disjoint from `EXCLUDED_TYPES` and from the conditional part of
--- `export.excluded_types()` by construction: each entry below is gated on
--- the same setting that would otherwise have excluded it outright, so a
--- type is either never recorded or recorded near the base, never both.
---
--- Worms are absent and cannot be added: they share the "turret" type with
--- player turrets (see `EXCLUDED_TYPES`), so bounding by type here would
--- bound real defences too.
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
  -- Aquilo freezes placed floor into a `frozen-` twin of the same tile.
  -- These are still floor the player laid, placeable and blueprintable, so
  -- without them here an entire Aquilo base's paving is invisible to live
  -- capture: it would only ever appear if terrain capture happened to be on,
  -- and then only in the baseline, never as it was built.
  --
  -- Seven of them, not one per floor type: the game only generates frozen
  -- variants for stone path, concrete, refined concrete and the two hazard
  -- pairs, not for the coloured refined concretes.
  "frozen-stone-path", "frozen-concrete",
  "frozen-hazard-concrete-left", "frozen-hazard-concrete-right",
  "frozen-refined-concrete",
  "frozen-refined-hazard-concrete-left", "frozen-refined-hazard-concrete-right",
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

--- The frame shape `M.terrain_margin` assumes a timelapse gets watched at.
--- 16:9 is save-timelapse.exe's default export size and every resolution
--- preset it offers, and it is what a video gets played back in regardless.
local TERRAIN_VIEW_ASPECT = 16 / 9

--- Matches `AUTO_FOLLOW_FIT_MARGIN` in `viewer/src/main.rs`: how much
--- smaller than edge to edge the export camera fits the base, so the
--- buildings have visible breathing room rather than touching the frame
--- border. Named here because the margin below is derived from what that
--- fit exposes, so the two have to agree to mean anything.
local TERRAIN_VIEW_FIT = 0.92

--- Ceiling on the region terrain is captured over, in tiles.
---
--- Needed because the margin below is driven by the base's *shape*, and a
--- long thin one asks for enormous amounts: a 100x5000 rail corridor wants
--- a 4800 tile margin, which is 140M tiles of ground, most of it nowhere
--- near anything.
---
--- 4M is a 2000x2000 region, which covers an ordinary base's full 16:9
--- framing without ever engaging.
---
--- A floor rather than the whole budget, because as a fixed ceiling it
--- inverted on anything larger than itself: a real 3070x3113 megabase has a
--- 9.6M tile footprint, so a 4M *total* left nothing for a margin, the
--- affordable width came out negative, and the base fell back to the 32 tile
--- floor. The bigger the factory, the less ground it got, which is exactly
--- backwards.
local TERRAIN_MAX_TILES = 4000000

--- ...so past that size the budget scales with the factory instead: a base
--- may always spend four times its own footprint.
---
--- Four specifically, because that is what a square base needs. Solving the
--- area cap for a square gives a margin of `(sqrt(k) - 1) / 2` per side, so
--- k=4 yields exactly half the base's width, which is what a 16:9 frame
--- exposes around a square factory (see `M.terrain_margin`). Anything less
--- caps a normally shaped base below what it can actually see, and the whole
--- point of the aspect calculation is to fill the frame.
---
--- Elongated bases stay bounded regardless, since the cap is on area: the
--- 100x5000 corridor below still gets 306 rather than the 4,781 it asks for.
local TERRAIN_BUDGET_MULTIPLE = 4

--- How far past `bbox` to capture natural ground, in tiles, never less than
--- `base_margin`.
---
--- `base_margin` alone was the whole rule when this only had to serve a
--- window somebody pans and zooms freely, where "roughly a chunk of
--- context" is a complete answer. Video export fits the base into a frame
--- of a fixed shape instead, and a fit leaves the difference between the
--- two shapes as empty world on whichever axis does not bind: a square base
--- in a 16:9 frame occupies 52% of its width, so nearly half the picture is
--- beyond the box, and 32 tiles of that is nothing on a base a thousand
--- tiles across.
---
--- So the margin is what the fit actually exposes rather than a guess.
--- `fit_bounds` takes the smaller of the two axis zooms and backs off by
--- `TERRAIN_VIEW_FIT`, so the visible region is the box grown to the
--- frame's aspect and then by 1/fit. Each of the first two candidates below
--- is that growth on one axis, and each goes negative when its axis is the
--- one that binds, so taking the largest picks the right one with no
--- branch. The third is the fit's own slack, which applies either way.
---
--- Deliberately *not* a fraction of the larger dimension, which is the
--- obvious rule and is wrong in both directions, because how much empty
--- space a fit leaves depends on the base's shape against the frame's, not
--- on its size: a 2:1 base needs 0.06 of its long side and a 1:2 base needs
--- 1.4 of it, so any single fraction is an order of magnitude out on one of
--- them.
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

--- Independent per format, since the frame and event formats change on their
--- own schedules: a frame-only tweak has no reason to also bump the event
--- version, and vice versa.
--- Version 2 groups records into per-name runs and stores coordinates as
--- zigzag varint deltas, measured 4.7x smaller than version 1 on a real
--- frame. See src/frame.rs, which reads both.
---
--- Version 3 writes byte for byte what version 2 does. It exists only to
--- declare "this file may contain extension records" (see the extension
--- contract in src/frame.rs), so a tool predating them refuses it up front
--- with a clear message instead of desynchronising on the first record it
--- cannot skip. Additions from here on are extension records, not a fourth
--- version, so this is meant to be the last time this number moves.
M.FRAME_VERSION = 3
--- Version 2 adds the dictionary-reset record (tag 7, see
--- `event_reset_dictionaries`). A version 1 reader hitting that record stops
--- the stream rather than misreading it, since an unknown tag ends parsing,
--- so the bump is what turns "silently wrong from the reload onward" into a
--- refusal an older build can explain.
---
--- Version 3 is the same story as the frame format's: identical records, and
--- the bump only declares that extension records may appear. That record
--- shape is the standing fix for the problem the version 2 bump could only
--- paper over, since an unknown tag is now skippable rather than the end of
--- the stream.
M.EVENT_VERSION = 3

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
  -- One and two byte values are almost everything this ever sees: a
  -- coordinate delta between neighbouring entities, a name id, a run length.
  -- Spelling those out avoids building and concatenating a table per call,
  -- which at two calls per entity was the whole of this encoder's remaining
  -- cost over the old per-entity one.
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
--- Takes parallel arrays rather than an array of per-entity tables, and that
--- is a performance decision, not a style one. A table per entity was
--- measured at 900k entities under real Lua 5.2: it made encoding 1.26x
--- slower than the per-entity string records this format replaced, wiping
--- out the win. Grouping straight into flat arrays as entities are scanned
--- costs a few integer-keyed stores instead, and lands at 0.60x, so the
--- smaller format is also the faster one to produce.
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

function M.milestone_name(session_id)
  return M.session_dir(session_id) .. "milestones.jsonl"
end

function M.player_log_name(session_id)
  return M.session_dir(session_id) .. "players.jsonl"
end

function M.prototypes_name(session_id)
  return M.session_dir(session_id) .. "prototypes.json"
end

--- Which prototype types are enemies whatever else is true of them, and so
--- take `enemy_map_color` rather than the friendly one.
---
--- Force is a property of an entity, not of a prototype, and this file is
--- keyed by prototype name, so it has to pick one colour per name without
--- ever being told which side anything was on. Type is the closest the game
--- comes to answering that, and for a capture it answers it exactly: the only
--- enemies that survive `EXCLUDED_TYPES` are nests and worms.
---
--- `turret` is the plain type worms use. It cannot catch a player's defences,
--- which are `ammo-turret`, `electric-turret` and `fluid-turret`, three types
--- of their own (see the note on EXCLUDED_TYPES above, which makes the same
--- split for the same reason).
---
--- The mobile ones are listed even though nothing captures them, because this
--- file describes the game's prototypes rather than one capture's contents,
--- and a wriggler that is somehow in an old recording should still be red.
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

--- One prototype colour as three bytes, which is the only form the reader
--- accepts.
---
--- Factorio writes a Color either as 0..1 floats or as 0..255 values, and the
--- rule for telling them apart is the game's own: any component above 1 means
--- the whole colour is in 0..255. Prototypes overwhelmingly use the second
--- form, base's own tiles included (`grass-1` is {55, 53, 11}), and the
--- runtime hands a prototype's colour back exactly as it was written rather
--- than normalising it first.
---
--- Assuming floats here did not produce a wrong colour, it produced no colours
--- at all: an already-byte-ranged 61 scaled by 255 wrote 15555, too large for
--- the byte the reader expects, and one such number made the entire file
--- unreadable. A modded playthrough then fell back to the desktop side's
--- built-in table for every tile it had, which is the exact thing this file
--- exists to stop. Alien Biomes was where it showed: 357 of 364 tiles.
---
--- Clamped as well as rounded, because the range rule is a convention the game
--- does not enforce on a mod: nothing stops a prototype carrying a component
--- outside either range, and a colour that is merely wrong must never cost the
--- other few hundred their colours.
function M.color_bytes(color)
  local r, g, b = color.r or 0, color.g or 0, color.b or 0
  local scale = 255
  if r > 1 or g > 1 or b > 1 then
    scale = 1
  end
  return clamp_byte(r * scale), clamp_byte(g * scale), clamp_byte(b * scale)
end

--- The types that have an underground reach to report, so nothing else is
--- asked for a property it does not have.
---
--- `max_underground_distance` is documented optional and so returns nil rather
--- than raising for everything else (checked against the install's own
--- runtime-api.json, the same source `EXCLUDED_TYPES` was checked against), but
--- this file is written inside a `pcall` that turns any raise into no file at
--- all. Reading two types' worth of properties instead of sixteen hundred is
--- both cheaper and not a thing that can cost a capture its whole sidecar.
local REACH_TYPES = {
  ["underground-belt"] = true,
  ["pipe-to-ground"] = true,
}

--- Everything the desktop side needs to know about this game's prototypes, as
--- JSON, so it never has to recognise one by name.
---
--- Two questions, one file, because both have the same answer source and the
--- same lifetime. What colour is it: the exact colours Factorio paints its own
--- map view with, which is the palette a player already has in their head. And
--- what *is* it: a belt, a pipe, an ore patch, a tree. Neither exists anywhere
--- but inside the running game, since a mod ships as a zip in the mods folder,
--- not as anything the desktop tool can read.
---
--- Without this, supporting a mod means transcribing its prototypes by hand,
--- once per mod, forever: Alien Biomes alone adds a couple of hundred tiles,
--- and Krastorio2 adds belt tiers, ore types and pipes that a viewer built
--- around the vanilla names cannot see are belts, ore or pipes at all.
---
--- Written once beside the baseline rather than per tick, and refreshed once
--- per load (see capture.lua's `prototypes_written`). None of it can change during
--- a playthrough: prototypes are fixed at load time.
---
--- Entities take `map_color` and fall back to `friendly_map_color`, with nests
--- and their kind taking `enemy_map_color`, which is the same split the game
--- makes when it draws them. The two are mutually exclusive per prototype:
--- `map_color` is documented as what charting uses "if a friendly or enemy
--- color isn't defined", and the prototypes that define one leave the other
--- nil.
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
      -- The prototype's own type, verbatim and unfiltered. Deciding here
      -- which types are worth reporting would just move the curated list
      -- from one side of the file to the other, and the desktop side is
      -- where the answer is actually wanted.
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
  out[#out + 1] = '}}'
  return table.concat(out)
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

--- Ground is one file per surface for a whole capture, not one per tick, so
--- unlike `M.frame_name` there is no tick in it. Session tagged for the same
--- reason everything else is: the shared script-output folder holds every
--- playthrough that ever recorded, and the desktop tool uses the folder it
--- lands in to tell whether the save it scanned was really the right one.
function M.terrain_name(session_id, surface)
  local name = string.format("terrain_%s.stfr", surface)
  if not session_id then
    return name
  end
  return M.session_dir(session_id) .. name
end

-- Milestones
--
-- Notable moments worth marking on the timeline: the first of each science
-- pack, the first rocket, the first visit to each planet. Plain
-- newline-delimited JSON for the same reason the player log below is: a
-- whole playthrough produces on the order of a dozen of these, nowhere near
-- the volume that justified a binary format for frames and events, and a
-- format that can be read by eye is worth more here than the few hundred
-- bytes packing it would save.

--- One milestone: `{"tick":T,"kind":K,"id":I}`.
---
--- `kind` says what sort of thing happened ("science", "rocket", "planet")
--- and `id` which one, rather than a prebaked sentence, so the viewer decides
--- the wording and can filter by kind without parsing prose.
function M.milestone_line(tick, kind, id)
  return string.format('{"tick":%d,"kind":%s,"id":%s}\n', tick, M.quote(kind), M.quote(id))
end

--- Whether `name` is a science pack, by suffix rather than by a fixed list,
--- so a modded pack is picked up for free.
---
--- Only ever asked about names that came out of *item* production
--- statistics, which is what makes the suffix safe: the two other
--- science-pack-ish prototype names in the game, the `science-pack` item
--- subgroup and the `signal-science-pack` virtual signal, are not items and
--- so can never appear there.
function M.is_science_pack(name)
  return name:sub(-13) == "-science-pack"
end

--- What one save can say about milestones, as a JSON object for the export
--- manifest: `{"science":[...],"planets":[...],"rockets":N}`.
---
--- State, not events, and that difference is the whole reason this exists.
--- Live capture watches milestones happen and can write the exact tick
--- (milestones.lua). A save file has no history of its own: it knows only
--- that a pack has been produced at some point, never when. So this reports
--- what is true as of this save, and recovering *when* each thing first
--- became true is left to the Rust side, which has every save's state and
--- can diff consecutive ones (see src/milestone.rs).
---
--- Rockets is a count rather than a flag so the diff can tell "the first
--- rocket flew between these two saves" from "some rockets flew, as they had
--- been all along."
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
