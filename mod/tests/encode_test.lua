-- Unit tests for mod/encode.lua, the pure encoding logic. No Factorio
-- required:
--
--   lua mod/tests/encode_test.lua
--
-- Not wired into `cargo test`, whose promise of needing nothing beyond the
-- Rust toolchain should not start depending on `lua`.

local script_dir = arg[0]:match("(.*/)") or "./"
local encode = dofile(script_dir .. "../encode.lua")

local failures = 0

local function bytes(...)
  return string.char(...)
end

local function check(name, actual, expected)
  if actual == expected then
    print("ok   " .. name)
  else
    failures = failures + 1
    print("FAIL " .. name)
    print("     expected: " .. tostring(expected))
    print("     actual:   " .. tostring(actual))
  end
end

-- low level byte packing

check("u8: a plain byte", encode.u8(200), bytes(200))
check("u16le: little endian order", encode.u16le(40000), bytes(64, 156))
check("u32le: little endian order", encode.u32le(3000000000), bytes(0, 94, 208, 178))
check("u64le: little endian order, high bytes zero for a small value", encode.u64le(1234), bytes(210, 4, 0, 0, 0, 0, 0, 0))

-- Lua's floor based %/math.floor produce correct two's complement bytes for
-- a negative number with no separate "add 2^32" step. Verified by hand:
-- -805 is 0xFFFFFCDB, little endian DB FC FF FF.
check("i32le: a negative value packs as two's complement", encode.i32le(-805), bytes(219, 252, 255, 255))
check("i32le: -1 is all set bits", encode.i32le(-1), bytes(255, 255, 255, 255))
check("i32le: zero", encode.i32le(0), bytes(0, 0, 0, 0))

check("str: length prefix then bytes", encode.str("nauvis"), bytes(6, 0) .. "nauvis")
check("str: empty string is just a zero length prefix", encode.str(""), bytes(0, 0))

-- round10

check("round10: a positive value already at one decimal", encode.round10(28.5), 285)
check("round10: a negative value already at one decimal", encode.round10(-80.5), -805)
check("round10: zero", encode.round10(0), 0)
check("round10: a whole number", encode.round10(327.0), 3270)

-- dictionary_id

do
  local dict = encode.new_dictionary()
  local id_a, define_a = encode.dictionary_id(dict, "transport-belt", 0)
  check("dictionary_id: first use of a name defines it at id 0", id_a, 0)
  check("dictionary_id: the define chunk uses the given tag", define_a, bytes(0) .. encode.str("transport-belt"))

  local id_b, define_b = encode.dictionary_id(dict, "transport-belt", 0)
  check("dictionary_id: a repeated name returns the same id", id_b, 0)
  check("dictionary_id: a repeated name defines nothing", define_b, "")

  local id_c, define_c = encode.dictionary_id(dict, "pipe", 0)
  check("dictionary_id: a second distinct name gets the next id", id_c, 1)
  check("dictionary_id: the second name is defined too", define_c, bytes(0) .. encode.str("pipe"))

  local surfaces = encode.new_dictionary()
  local _, define_surface = encode.dictionary_id(surfaces, "nauvis", 1)
  check("dictionary_id: a different tag is used verbatim (DefineSurface)", define_surface, bytes(1) .. encode.str("nauvis"))
end

-- frame format

check("frame_header: magic, version, tick, surface", encode.frame_header(100, "nauvis"),
  "STF1" .. bytes(3) .. encode.u64le(100) .. encode.str("nauvis"))

-- varints

check("varint: a small value is one byte", encode.varint(0), bytes(0))
check("varint: the largest one byte value", encode.varint(127), bytes(127))
check("varint: 128 rolls into two bytes", encode.varint(128), bytes(128, 1))
check("varint: seven bits per byte", encode.varint(300), bytes(172, 2))
check("varint: two byte maximum", encode.varint(16383), bytes(255, 127))
check("varint: 16384 rolls into three", encode.varint(16384), bytes(128, 128, 1))

-- Zigzag keeps a small negative small: without it, -1 would set every high
-- bit and take ten bytes. No shifts or xor needed, which matters because
-- Factorio's Lua 5.2 has neither.
check("zigzag: zero", encode.zigzag(0), 0)
check("zigzag: positives double", encode.zigzag(1), 2)
check("zigzag: negatives interleave", encode.zigzag(-1), 1)
check("zigzag: -64 still fits one varint byte", encode.zigzag(-64), 127)
check("zigzag: -65 needs a second", encode.zigzag(-65), 129)
check("varint_i32: a negative delta is one byte", encode.varint_i32(-1), bytes(1))

-- Runs replace the old per-entity records: the name id and count are written
-- once for a group, and each position is a delta from the one before it.

do
  local dict = encode.new_dictionary()
  -- Parallel arrays, not a table per entity: see frame_entity_run on why.
  local run = encode.frame_entity_run(dict, "transport-belt", 1, 1,
    { -80.5, -79.5 }, { 28.5, 28.5 }, { 4, 6 }, 2)
  local expected = bytes(0) .. encode.str("transport-belt") .. bytes(1) .. bytes(1) -- DefineName carries the footprint
    .. bytes(1) .. encode.varint(0) .. encode.varint(2) .. bytes(1) -- run: id, count, directions flag
    .. encode.varint_i32(-805) .. encode.varint_i32(285) .. bytes(4) -- first item, delta from origin
    .. encode.varint_i32(10) .. encode.varint_i32(0) .. bytes(6) -- second, delta from the first
  check("frame_entity_run: footprint in the definition, positions as deltas", run, expected)
end

do
  local dict = encode.new_dictionary()
  -- Nothing in this run rotates, so the flag is clear and no direction
  -- bytes are written at all.
  local run = encode.frame_entity_run(dict, "wooden-chest", 1, 1,
    { 0.5, 1.5 }, { 0.5, 0.5 }, { 0, 0 }, 2)
  local expected = bytes(0) .. encode.str("wooden-chest") .. bytes(1) .. bytes(1)
    .. bytes(1) .. encode.varint(0) .. encode.varint(2) .. bytes(0)
    .. encode.varint_i32(5) .. encode.varint_i32(5)
    .. encode.varint_i32(10) .. encode.varint_i32(0)
  check("frame_entity_run: a run that never rotates spends nothing on direction", run, expected)
end

do
  local dict = encode.new_dictionary()
  encode.frame_define_name(dict, "concrete", 1, 1) -- pre-seed, so the run does not redefine it
  local run = encode.frame_tile_run(dict, "concrete", { -5, -4 }, { 12, 12 }, 2)
  local expected = bytes(2) .. encode.varint(0) .. encode.varint(2)
    .. encode.varint_i32(-5) .. encode.varint_i32(12)
    .. encode.varint_i32(1) .. encode.varint_i32(0)
  check("frame_tile_run: integer coordinates, also delta encoded", run, expected)
end

check("frame_end_entities: tag 9, no payload", encode.frame_end_entities(), bytes(9))

-- live capture event format

check("event_header: magic and version", encode.event_header(), "STE1" .. bytes(3))
-- Written when a load resumes a segment already on disk: Factorio has just
-- emptied the writer's dictionaries while the file still holds every name
-- defined before the load, so both sides restart their ids from 0 here.
check("event_reset_dictionaries: bare tag 7", encode.event_reset_dictionaries(), bytes(7))
check("event_set_tick: tag 2 then the tick", encode.event_set_tick(1234), bytes(2) .. encode.u64le(1234))

do
  local names, surfaces = encode.new_dictionary(), encode.new_dictionary()
  local record = encode.event_add_entity(names, surfaces, "nauvis", "transport-belt", 10.5, 20.5, 4, 8842, 1, 1)
  local expected = bytes(1) .. encode.str("nauvis") -- DefineSurface, first use
    .. bytes(0) .. encode.str("transport-belt") -- DefineName, first use
    .. bytes(3) .. encode.u16le(0)
    .. encode.i32le(105) .. encode.i32le(205)
    .. bytes(4) .. bytes(1) .. bytes(1)
    .. encode.u64le(8842) .. encode.u16le(0)
  check("event_add_entity: defines both surface and name on first use", record, expected)
end

do
  local names, surfaces = encode.new_dictionary(), encode.new_dictionary()
  encode.dictionary_id(surfaces, "nauvis", 1)
  local record = encode.event_add_entity(names, surfaces, "nauvis", "stone-furnace", 1, 2, 0, 8842, nil, nil)
  local expected = bytes(0) .. encode.str("stone-furnace") -- surface already known, only the name is new
    .. bytes(3) .. encode.u16le(0)
    .. encode.i32le(10) .. encode.i32le(20)
    .. bytes(0) .. bytes(1) .. bytes(1) -- direction/w/h default to 0/1/1
    .. encode.u64le(8842) .. encode.u16le(0)
  check("event_add_entity: an omitted direction and footprint default to 0 and 1x1", record, expected)
end

do
  local names, surfaces = encode.new_dictionary(), encode.new_dictionary()
  local record = encode.event_add_entity(names, surfaces, "nauvis", "inserter", 1, 2, 0, nil, 1, 1)
  local id_field = record:sub(-10, -3) -- the 8 id bytes, right before the trailing surface_id
  check("event_add_entity: a missing id encodes as the 0 sentinel", id_field, encode.u64le(0))
end

-- Dragging a belt line round a corner makes Factorio rotate the belt already
-- placed and raises no event, so capture re-logs the tile the drag came from.
-- Backwards here would re-log the tile ahead, which is the belt just placed,
-- and fix nothing.
do
  -- Factorio's y grows south, so behind a north-facing belt is south of it.
  local cases = {
    { name = "north", direction = 0, dx = 0, dy = 1 },
    { name = "east", direction = 4, dx = -1, dy = 0 },
    { name = "south", direction = 8, dx = 0, dy = -1 },
    { name = "west", direction = 12, dx = 1, dy = 0 },
  }
  for _, case in pairs(cases) do
    local dx, dy = encode.step_behind(case.direction)
    check("step_behind: " .. case.name .. " looks back the way it came", dx .. "," .. dy, case.dx .. "," .. case.dy)
  end

  -- A belt can only face a cardinal, so the twelve diagonals are not a facing
  -- to step back from and must not resolve to one.
  for _, direction in pairs({ 1, 2, 3, 5, 6, 7, 9, 10, 11, 13, 14, 15 }) do
    check("step_behind: direction " .. direction .. " is not a belt facing", encode.step_behind(direction), nil)
  end
end

do
  local types = encode.DRAGGABLE_CARRIER_TYPES
  check("DRAGGABLE_CARRIER_TYPES: contains transport-belt", types["transport-belt"], true)
  check("DRAGGABLE_CARRIER_TYPES: contains underground-belt", types["underground-belt"], true)
  check("DRAGGABLE_CARRIER_TYPES: contains splitter", types["splitter"], true)
  check("DRAGGABLE_CARRIER_TYPES: contains lane-splitter", types["lane-splitter"], true)
  -- Pipes connect by adjacency rather than by facing, so nothing rotates them
  -- and looking behind one would be a lookup per pipe placed for nothing.
  check("DRAGGABLE_CARRIER_TYPES: does not contain pipe", types["pipe"], nil)
  check("DRAGGABLE_CARRIER_TYPES: does not contain inserter", types["inserter"], nil)
end

-- Names what the next removal is for, as an extension record so a tool older
-- than the field steps over it by its own length. Written for a deposit only:
-- it is the one thing that can sit under something else, and a removal
-- carrying just a position resolves to whatever stands on top instead.
do
  local names = encode.new_dictionary()
  local record = encode.event_remove_name(names, "iron-ore")
  local payload = encode.varint(0)
  local expected = bytes(0) .. encode.str("iron-ore") -- DefineName, first use
    .. bytes(128) .. encode.varint(#payload) .. payload
  check("event_remove_name: defines the name, then tag 128 with its own length", record, expected)

  -- Second use shares the dictionary entry, so only the record itself repeats.
  local again = encode.event_remove_name(names, "iron-ore")
  check("event_remove_name: a known name costs only the record", again,
    bytes(128) .. encode.varint(#payload) .. payload)
end

do
  -- The length is what an older reader skips by, so it must count the payload
  -- and nothing else.
  local names = encode.new_dictionary()
  for i = 1, 200 do
    encode.dictionary_id(names, "filler-" .. i, 0)
  end
  local record = encode.event_remove_name(names, "iron-ore")
  local body = record:sub(#(bytes(0) .. encode.str("iron-ore")) + 1)
  local declared = body:byte(2)
  check("event_remove_name: a two-byte name id declares length 2", declared, 2)
  check("event_remove_name: and the record is exactly that long", #body, 2 + declared)
end

-- Position is sent on removal even when id is available: an entity that
-- existed when the baseline was taken carries a real unit_number replay never
-- learned, a snapshot recording no ids, so id alone made every such removal an
-- unresolvable no-op.
do
  local surfaces = encode.new_dictionary()
  local record = encode.event_remove_entity(surfaces, "nauvis", 10.5, 20.5, 8842)
  local expected = bytes(1) .. encode.str("nauvis")
    .. bytes(4) .. encode.i32le(105) .. encode.i32le(205) .. encode.u64le(8842) .. encode.u16le(0)
  check("event_remove_entity: carries position and id together", record, expected)
end

do
  local surfaces = encode.new_dictionary()
  local record = encode.event_remove_entity(surfaces, "nauvis", 10.5, 20.5, nil)
  local expected = bytes(1) .. encode.str("nauvis")
    .. bytes(4) .. encode.i32le(105) .. encode.i32le(205) .. encode.u64le(0) .. encode.u16le(0)
  check("event_remove_entity: no id is position keyed, sentinel id of 0", record, expected)
end

do
  local names, surfaces = encode.new_dictionary(), encode.new_dictionary()
  local record = encode.event_add_tile(names, surfaces, "fulgora", "concrete", 10, 20)
  local expected = bytes(1) .. encode.str("fulgora")
    .. bytes(0) .. encode.str("concrete")
    .. bytes(5) .. encode.u16le(0) .. encode.i32le(10) .. encode.i32le(20) .. encode.u16le(0)
  check("event_add_tile: carries its surface, integer coordinates", record, expected)
end

do
  local surfaces = encode.new_dictionary()
  local record = encode.event_remove_tile(surfaces, "fulgora", 10, 20)
  local expected = bytes(1) .. encode.str("fulgora")
    .. bytes(6) .. encode.i32le(10) .. encode.i32le(20) .. encode.u16le(0)
  check("event_remove_tile: carries its surface, no name", record, expected)
end

do
  -- vulcanus is pre-seeded as the second surface (id 1); concrete is still
  -- new here, so the record should be DefineName("concrete") then the tile
  -- record referencing surface id 1 without redefining vulcanus.
  local names, surfaces = encode.new_dictionary(), encode.new_dictionary()
  encode.dictionary_id(surfaces, "nauvis", 1)
  encode.dictionary_id(surfaces, "vulcanus", 1)
  local record = encode.event_add_tile(names, surfaces, "vulcanus", "concrete", 1, 1)
  local expected = bytes(0) .. encode.str("concrete")
    .. bytes(5) .. encode.u16le(0) .. encode.i32le(1) .. encode.i32le(1) .. encode.u16le(1)
  check("event_add_tile: a second, already defined surface is referenced by its id", record, expected)
end

-- Reload handling has no test here because the mod does not attempt it: every
-- value that could reveal a reload lives in `storage`, which rewinds with the
-- save. Reloads are resolved on the reading side by
-- `event::segment_run_bounds`.

-- milestones

check("milestone_line: tick, kind and id as one JSON line",
  encode.milestone_line(1234, "science", "logistic-science-pack"),
  '{"tick":1234,"kind":"science","id":"logistic-science-pack"}\n')

check("is_science_pack: matches on the suffix", encode.is_science_pack("logistic-science-pack"), true)
check("is_science_pack: a modded pack is picked up for free",
  encode.is_science_pack("se-deep-space-science-pack"), true)
check("is_science_pack: an ordinary item is not one", encode.is_science_pack("iron-plate"), false)

-- The from-saves half of milestones. A save knows only totals, so the mod
-- reports state and src/milestone.rs recovers timings by comparing consecutive
-- saves. Rockets is a count rather than a flag so that diff can tell a first
-- launch from launches that had been happening all along.
check("milestone_state: both lists and the rocket count as one JSON object",
  encode.milestone_state({ "automation-science-pack", "logistic-science-pack" }, { "nauvis" }, 3),
  '{"science":["automation-science-pack","logistic-science-pack"],"planets":["nauvis"],"rockets":3}')

check("milestone_state: a save with nothing reached yet is still well formed",
  encode.milestone_state({}, {}, 0),
  '{"science":[],"planets":[],"rockets":0}')

-- Goes through encode.quote, so a name needing escaping stays valid JSON
-- rather than terminating the string early.
check("milestone_state: a name needing escaping is quoted properly",
  encode.milestone_state({}, { 'a "quoted" moon' }, 0),
  '{"science":[],"planets":["a \\"quoted\\" moon"],"rockets":0}')

-- checksums

check("checksum_init: the djb2 seed", encode.checksum_init(), 5381)

do
  local hash = encode.checksum_init()
  hash = encode.checksum_update(hash, "ab")
  -- Hand computed: 5381*33+97=177670, then 177670*33+98=5863208. Also
  -- asserted equal in src/frame.rs's checksum tests, so the two
  -- implementations are checked against the same known vector.
  check("checksum_update: byte by byte djb2 over \"ab\"", hash, 5863208)
end

do
  local hash = encode.checksum_init()
  hash = encode.checksum_update(hash, "a")
  hash = encode.checksum_update(hash, "b")
  check("checksum_update: splitting the input across calls gives the same result", hash, 5863208)
end

-- terrain capture bounding box

check("new_bbox: starts with nothing seen", encode.new_bbox().min_x, nil)

do
  local bbox = encode.new_bbox()
  encode.grow_bbox(bbox, 10, 20)
  encode.grow_bbox(bbox, -5, 30)
  encode.grow_bbox(bbox, 7, -2)
  check("grow_bbox: min_x tracks the smallest x seen", bbox.min_x, -5)
  check("grow_bbox: max_x tracks the largest x seen", bbox.max_x, 10)
  check("grow_bbox: min_y tracks the smallest y seen", bbox.min_y, -2)
  check("grow_bbox: max_y tracks the largest y seen", bbox.max_y, 30)
end

check("expand_bbox: nil for a box that never saw a position",
  encode.expand_bbox(encode.new_bbox(), 32), nil)

do
  local bbox = encode.new_bbox()
  encode.grow_bbox(bbox, 10, 20)
  encode.grow_bbox(bbox, -5, 30)
  -- Compared field by field rather than the returned table as a whole:
  -- Lua's == on tables is identity, not structural, so two freshly built
  -- tables with identical contents are never equal.
  local area = encode.expand_bbox(bbox, 4)
  check("expand_bbox: left_top x is min_x minus the margin", area[1][1], -9)
  check("expand_bbox: left_top y is min_y minus the margin", area[1][2], 16)
  check("expand_bbox: right_bottom x is max_x plus the margin", area[2][1], 14)
  check("expand_bbox: right_bottom y is max_y plus the margin", area[2][2], 34)
end

-- scenery captured near the factory rather than across the whole surface

do
  local function set(list)
    local s = {}
    for _, t in pairs(list) do
      if s[t] then
        error("duplicate type " .. t)
      end
      s[t] = true
    end
    return s
  end

  local both_off = set(encode.context_types(false, false))
  local both_on = set(encode.context_types(true, true))

  check("context_types: nests are scenery whatever the settings say", both_off["unit-spawner"], true)
  check("context_types: resources only when they are being recorded", both_off["resource"], nil)
  check("context_types: resources when include-resources is on", both_on["resource"], true)
  check("context_types: no flora when terrain capture is off", both_off["tree"], nil)
  for _, t in pairs({ "tree", "cliff", "plant" }) do
    check("context_types: " .. t .. " is scenery when terrain capture is on", both_on[t], true)
  end

  -- Worms share the "turret" type with player defences, so bounding this
  -- type would bound real turrets to the factory box as well.
  check("context_types: never bounds turrets, which would take player ones with them", both_on["turret"], nil)

  -- The property the whole split rests on. A type named by both lists would
  -- either be dropped from the unbounded pass and never picked up by the
  -- bounded one, or captured twice.
  local excluded = set(encode.EXCLUDED_TYPES)
  for _, t in pairs(encode.context_types(true, true)) do
    check("context_types: " .. t .. " is not also always-excluded", excluded[t], nil)
  end
end

-- how far past the built area to capture ground

do
  local function box(w, h)
    local bbox = encode.new_bbox()
    encode.grow_bbox(bbox, 0, 0)
    encode.grow_bbox(bbox, w, h)
    return bbox
  end

  check("terrain_margin: an untouched surface just gets the floor",
    encode.terrain_margin(encode.new_bbox(), 32), 32)

  check("terrain_margin: a base small enough not to matter gets the floor",
    encode.terrain_margin(box(20, 20), 32), 32)

  -- A square base fills 52% of a 16:9 frame's width, so nearly half the
  -- picture is world past the box and the margin has to reach that far.
  check("terrain_margin: a square base is padded to fill the frame's width",
    encode.terrain_margin(box(1000, 1000), 32), 466)

  -- Why a fraction of the larger dimension cannot work: both have a 2:1 side
  -- ratio and each needs an order of magnitude different padding, what a fit
  -- exposes depending on shape against the frame rather than on size.
  check("terrain_margin: a base wider than the frame needs very little",
    encode.terrain_margin(box(2000, 1000), 32), 111)

  check("terrain_margin: a base taller than the frame needs more than its own width",
    encode.terrain_margin(box(200, 400), 32), 286)

  -- The corridor named in `terrain_margin`'s own comment: it asks for a
  -- 4780 tile margin, which would be 140M tiles of ground.
  check("terrain_margin: a long thin base is capped well below what it asks for",
    encode.terrain_margin(box(100, 5000), 32), 306)

  do
    local m = encode.terrain_margin(box(100, 5000), 32)
    check("terrain_margin: the cap keeps the captured region inside the budget",
      (100 + 2 * m) * (5000 + 2 * m) <= 4000000, true)
  end

  -- The bug was in the budget rather than the margin: as a flat ceiling, any
  -- base bigger than the ceiling had nothing left to spend, so the affordable
  -- width came out negative and it fell back to the 32 tile floor. The largest
  -- factories got the smallest margins.
  check("terrain_margin: a base far larger than the flat budget still gets a real margin",
    encode.terrain_margin(box(5000, 5000), 32), 2330)

  -- The property rather than one number: the allowance has to grow with the
  -- factory, or the same inversion comes back at whatever the next fixed
  -- limit happens to be.
  do
    local small = encode.terrain_margin(box(3000, 3000), 32)
    local large = encode.terrain_margin(box(6000, 6000), 32)
    check("terrain_margin: a bigger base is given a bigger margin, not a smaller one", large > small, true)
  end

  check("terrain_margin: always a whole number of tiles",
    encode.terrain_margin(box(1000, 1000), 32) % 1, 0)
end

-- per-playthrough file naming

check("session_dir: session id as zero padded 8 digit hex",
  encode.session_dir(0x1a2b3c),
  "001a2b3c/")

check("session_dir: a small session id still gets full width padding",
  encode.session_dir(0),
  "00000000/")

check("baseline_manifest_name: lives inside the session's own folder",
  encode.baseline_manifest_name(0x1a2b3c),
  "001a2b3c/baseline.json")

check("capture_segment_name: tick only, folder already scopes it to one session",
  encode.capture_segment_name(0x1a2b3c, 22760790),
  "001a2b3c/events_22760790.stev")

check("capture_segment_basename: no session folder, same naming rule",
  encode.capture_segment_basename(22760790),
  "events_22760790.stev")

-- The reader tells a branch that was left behind from the history leading to
-- now by walking these parents, so the name has to carry one whenever there is
-- one to carry.
check("capture_segment_name: a segment names the one its save was made during",
  encode.capture_segment_name(0x1a2b3c, 22760790, 22000000),
  "001a2b3c/events_22760790_22000000.stev")

check("capture_segment_basename: a capture's first segment has no parent",
  encode.capture_segment_basename(22760790, nil),
  "events_22760790.stev")

check("player_log_name: lives inside the session's own folder",
  encode.player_log_name(0x1a2b3c),
  "001a2b3c/players.jsonl")

check("frame_name: tagged, lives inside the session's own folder",
  encode.frame_name(0x1a2b3c, 100, "nauvis"),
  "001a2b3c/frame_100_nauvis.stfr")

check("frame_name: untagged (timelapse-export / headless scan) stays flat",
  encode.frame_name(nil, 100, "nauvis"),
  "frame_100_nauvis.stfr")

-- player position log

check("player_log_line: one player, tick and fields in order",
  encode.player_log_line(100, { { name = "Alice", surface = "nauvis", x = 10.5, y = -3.2 } }),
  '{"tick":100,"players":[{"name":"Alice","surface":"nauvis","x":10.5,"y":-3.2}]}\n')

check("player_log_line: multiple players are comma separated",
  encode.player_log_line(1, {
    { name = "Alice", surface = "nauvis", x = 1, y = 2 },
    { name = "Bob", surface = "vulcanus", x = 3, y = 4 },
  }),
  '{"tick":1,"players":[{"name":"Alice","surface":"nauvis","x":1,"y":2},' ..
    '{"name":"Bob","surface":"vulcanus","x":3,"y":4}]}\n')

-- Never actually written: export.lua's callers skip the write entirely when
-- nobody was sampled. Pinned anyway, since something could start calling this
-- with an empty list.
check("player_log_line: an empty player list is an empty JSON array",
  encode.player_log_line(1, {}),
  '{"tick":1,"players":[]}\n')

-- list sanity

local function assert_no_duplicates(list, label)
  local seen = {}
  for _, item in pairs(list) do
    if seen[item] then
      failures = failures + 1
      print("FAIL " .. label .. ": duplicate entry " .. item)
    end
    seen[item] = true
  end
  return seen
end

local excluded = assert_no_duplicates(encode.EXCLUDED_TYPES, "EXCLUDED_TYPES")
check("EXCLUDED_TYPES: contains character", excluded["character"], true)
-- Rendered as ground context now (see export.lua's terrain capture), not
-- excluded like other terrain scatter.
check("EXCLUDED_TYPES: does not contain tree", excluded["tree"], nil)
check("EXCLUDED_TYPES: does not contain cliff", excluded["cliff"], nil)
check("EXCLUDED_TYPES: still contains the generic decorative scatter type", excluded["simple-entity"], true)
-- Every flying robot type, the highest-volume mobile entity in the game: a
-- megabase mid construction job has tens of thousands airborne, each pinned
-- wherever it happened to be since the format cannot update a position.
check("EXCLUDED_TYPES: contains construction-robot", excluded["construction-robot"], true)
check("EXCLUDED_TYPES: contains logistic-robot", excluded["logistic-robot"], true)
check("EXCLUDED_TYPES: contains combat-robot", excluded["combat-robot"], true)
-- The stationary infrastructure that flies them stays: it is the part that
-- actually shows the factory growing.
check("EXCLUDED_TYPES: does not contain roboport", excluded["roboport"], nil)
-- Mobile enemies stay excluded: their combat deaths would flood capture with
-- removals unrelated to construction, and a captured biter would sit frozen
-- where it was first logged. On a real capture ~6% of exported entities were
-- biters, spitters and spawners before this.
check("EXCLUDED_TYPES: contains unit (biters, spitters)", excluded["unit"], true)
-- Asteroid chunks drift and are collected continuously, and are never built,
-- so every one logs a removal for something replay never had. On a real
-- five-platform capture that was 6,101 of 6,259 events.
check("EXCLUDED_TYPES: contains asteroid-chunk", excluded["asteroid-chunk"], true)
-- ...but the collector is a structure worth watching go up.
check("EXCLUDED_TYPES: does not contain asteroid-collector", excluded["asteroid-collector"], nil)
-- Nests are captured despite being enemies: stationary, few, and watching them
-- get cleared is how expansion reads in a timelapse.
check("EXCLUDED_TYPES: does not contain unit-spawner", excluded["unit-spawner"], nil)
-- Space Age gave its mobile enemies their own prototype types, so "unit" never
-- covered them and every one landed in captures as though somebody had built
-- it. Found in a real Gleba capture; because they roam, the auto-follow camera
-- stretched to wherever they had wandered.
check("EXCLUDED_TYPES: contains spider-unit (Gleba stompers and strafers)", excluded["spider-unit"], true)
check("EXCLUDED_TYPES: contains spider-leg (their legs, and Spidertron's)", excluded["spider-leg"], true)
check("EXCLUDED_TYPES: contains segmented-unit (Vulcanus demolisher heads)", excluded["segmented-unit"], true)
check("EXCLUDED_TYPES: contains segment (demolisher bodies)", excluded["segment"], true)
-- Vehicles and rolling stock, excluded under the same rule and simply missed
-- until now. Trains are the worst case: a from-saves export catches them
-- somewhere different in every save, so they blink around the network.
check("EXCLUDED_TYPES: contains car (cars and tanks)", excluded["car"], true)
check("EXCLUDED_TYPES: contains spider-vehicle (Spidertron)", excluded["spider-vehicle"], true)
check("EXCLUDED_TYPES: contains locomotive", excluded["locomotive"], true)
check("EXCLUDED_TYPES: contains cargo-wagon", excluded["cargo-wagon"], true)
check("EXCLUDED_TYPES: contains fluid-wagon", excluded["fluid-wagon"], true)
check("EXCLUDED_TYPES: contains artillery-wagon", excluded["artillery-wagon"], true)
-- ...but the track they run on is stationary infrastructure and is exactly
-- what shows a rail network growing, the same way roboports stay while the
-- robots do not.
check("EXCLUDED_TYPES: does not contain straight-rail", excluded["straight-rail"], nil)
check("EXCLUDED_TYPES: does not contain rail-signal", excluded["rail-signal"], nil)
check("EXCLUDED_TYPES: does not contain train-stop", excluded["train-stop"], nil)

local known_floor = assert_no_duplicates(encode.KNOWN_PLACED_FLOOR_TILES, "KNOWN_PLACED_FLOOR_TILES")
check("KNOWN_PLACED_FLOOR_TILES: contains concrete", known_floor["concrete"], true)
check("KNOWN_PLACED_FLOOR_TILES: contains landfill", known_floor["landfill"], true)
check("KNOWN_PLACED_FLOOR_TILES: contains stone-path", known_floor["stone-path"], true)

-- placed_floor_tiles
--
-- Asked of the game rather than listed, the list being Wube's names and the
-- last place a mod could not be seen. `space-platform-foundation` proves it and
-- is not even modded: a platform's own floor was missing, so the tiles a player
-- lays were recorded as natural ground and the platform appeared fully formed.
--
-- Two properties unioned, neither covering it alone on a real 69 mod game.

do
  local tile = function(props) return props end
  prototypes = {
    tile = {
      -- Placed by an item. The one that was missing.
      ["space-platform-foundation"] = tile({
        items_to_place_this = { { name = "space-platform-foundation" } },
        mineable_properties = { minable = false },
      }),
      -- Minable but placed by no item of its own, which is how most modded
      -- floor reads.
      ["cerys-refined-concrete"] = tile({ mineable_properties = { minable = true } }),
      -- Neither, and floor anyway. Exactly why the stated list survives.
      ["acid-refined-concrete"] = tile({ mineable_properties = { minable = false } }),
      -- Ground. An empty item list must not count as an item.
      ["vegetation-green-grass-1"] = tile({ items_to_place_this = {}, mineable_properties = { minable = false } }),
      ["water"] = tile({ mineable_properties = { minable = false } }),
    },
  }

  check("placed_floor_tiles: sorted, and natural ground left out", table.concat(encode.placed_floor_tiles(), ","),
    "acid-refined-concrete,cerys-refined-concrete,space-platform-foundation")
  -- These names are handed to find_tiles_filtered, which will not be asked
  -- about a tile no loaded mod defines.
  check("placed_floor_tiles: a stated name this game lacks is left out",
    table.concat(encode.placed_floor_tiles(), ","):find("frozen%-concrete"), nil)
  prototypes = nil
end

-- color_bytes
--
-- Factorio writes a Color as 0..1 floats or as 0..255 values and tells them
-- apart by whether anything is above 1. Assuming floats put an already
-- byte-ranged colour out of range (61 became 15555), which the reader could not
-- parse: on an Alien Biomes save 357 of 364 tiles were unreadable, and one was
-- enough to make the desktop side discard the whole file.

local function color(c)
  return string.format("%d,%d,%d", encode.color_bytes(c))
end

check("color_bytes: 0..1 floats scale up", color({ r = 0.2, g = 0.4, b = 1 }), "51,102,255")
check("color_bytes: 0..255 values pass through", color({ r = 55, g = 53, b = 11 }), "55,53,11")
check("color_bytes: one component above 1 means the whole colour is 0..255", color({ r = 218, g = 1, b = 0 }), "218,1,0")
check("color_bytes: black is black whichever form it is in", color({ r = 0, g = 0, b = 0 }), "0,0,0")
check("color_bytes: white as floats", color({ r = 1, g = 1, b = 1 }), "255,255,255")
check("color_bytes: a missing component is zero", color({ r = 0.5 }), "128,0,0")
-- The range rule is a convention, not something the game enforces on a mod.
check("color_bytes: clamped above", color({ r = 300, g = 260, b = 4 }), "255,255,4")
check("color_bytes: clamped below", color({ r = -5, g = 5, b = 400 }), "0,5,255")

-- prototypes_json
--
-- Reads the `prototypes` global the game provides, which the test stands in
-- for. Nests and worms take the enemy colour and everything else does not:
-- `enemy_map_color` is set on most prototypes whatever side they are on, so
-- preferring it painted belts, walls and radars alike in biter red.
--
-- Every entity's type goes out verbatim, which is what lets the desktop side
-- recognise a modded belt without naming it. Reach is asked of underground
-- belts and pipes only.

do
  -- `mineable_properties` is never nil on a real tile prototype, so the
  -- stubs carry it too rather than letting the reader be defensive about a
  -- shape the game does not produce.
  local tile = function(c, minable)
    return { map_color = c, mineable_properties = { minable = minable or false } }
  end
  prototypes = {
    tile = {
      ["grass-1"] = tile({ r = 55, g = 53, b = 11 }),
      ["water"] = tile({ r = 51, g = 83, b = 95 }),
      -- Minable, so it is floor somebody laid, and no list here names it.
      ["cerys-refined-concrete"] = tile({ r = 100, g = 100, b = 100 }, true),
    },
    entity = {
      ["transport-belt"] = {
        type = "transport-belt",
        map_color = { r = 204, g = 161, b = 71 },
        enemy_map_color = { r = 255, g = 25, b = 25 },
      },
      ["radar"] = {
        type = "radar",
        friendly_map_color = { r = 0, g = 93, b = 147 },
        enemy_map_color = { r = 255, g = 25, b = 25 },
      },
      ["biter-spawner"] = {
        type = "unit-spawner",
        friendly_map_color = { r = 0, g = 93, b = 147 },
        enemy_map_color = { r = 255, g = 25, b = 25 },
        -- The one thing a removal cannot say. A nest cleared before any save
        -- was scanned is rebuilt from its name alone, and without this it
        -- would be rebuilt one tile across.
        tile_width = 3,
        tile_height = 3,
      },
      ["small-worm-turret"] = {
        type = "turret",
        enemy_map_color = { r = 255, g = 25, b = 25 },
      },
      -- A modded tier, which is the whole point: nothing here or on the
      -- desktop side knows this name, and it still comes out a belt.
      ["kr-advanced-underground-belt"] = {
        type = "underground-belt",
        friendly_map_color = { r = 0, g = 93, b = 147 },
        max_underground_distance = 30,
      },
      -- Nothing to say about itself, so it says nothing and the viewer
      -- falls back for it, exactly as for a name this file never mentions.
      ["colourless"] = { type = "container" },
    },
  }

  check(
    "prototypes_json: colours, types, reach and footprints, each sorted",
    encode.prototypes_json(),
    '{"tiles":{"cerys-refined-concrete":[100,100,100],"grass-1":[55,53,11],"water":[51,83,95]},'
      .. '"entities":{"biter-spawner":[255,25,25],"kr-advanced-underground-belt":[0,93,147],'
      .. '"radar":[0,93,147],"small-worm-turret":[255,25,25],"transport-belt":[204,161,71]},'
      .. '"types":{"biter-spawner":"unit-spawner","colourless":"container",'
      .. '"kr-advanced-underground-belt":"underground-belt","radar":"radar",'
      .. '"small-worm-turret":"turret","transport-belt":"transport-belt"},'
      .. '"reach":{"kr-advanced-underground-belt":30},'
      -- Only what is not one tile across, so absent means 1x1 and the common
      -- case costs nothing. The nest is here because a removal carries no
      -- footprint, and a nest cleared before any save was scanned is rebuilt
      -- from its name alone.
      .. '"size":{"biter-spawner":"3x3"},'
      -- Empty for a game with no rails down, and for one whose rail API this
      -- mod could not read. The desktop side falls back to its own geometry
      -- for both, so they need not be told apart.
      .. '"rails":[],'
      -- Named so the desktop side splits a baseline's tiles the way this
      -- capture recorded them, rather than from a list of its own that would
      -- disagree the moment a mod adds a floor.
      .. '"floor":["cerys-refined-concrete"]}'
  )

  -- Rail geometry is not in any prototype, so what gets written is which
  -- rails connect to which and the desktop side works the shape out from
  -- there. Sorted by name then facing, so two runs of one save agree.
  check(
    "prototypes_json: rails carry their neighbours, sorted by name then facing",
    encode.prototypes_json({
      { n = "curved-rail-b", d = 0, links = { { n = "straight-rail", d = 0, x = 2, y = 0 } } },
      { n = "curved-rail-a", d = 4, links = {
        { n = "straight-rail", d = 4, x = -3, y = 0 },
        { n = "curved-rail-b", d = 4, x = 1.5, y = -2.5 },
      } },
      { n = "curved-rail-a", d = 0, links = { { n = "straight-rail", d = 0, x = 0, y = 3 } } },
    }):match('("rails":%[.*%]),"floor"'),
    '"rails":['
      .. '{"n":"curved-rail-a","d":0,"links":[{"n":"straight-rail","d":0,"x":0.0,"y":3.0}]},'
      .. '{"n":"curved-rail-a","d":4,"links":[{"n":"straight-rail","d":4,"x":-3.0,"y":0.0},'
      .. '{"n":"curved-rail-b","d":4,"x":1.5,"y":-2.5}]},'
      .. '{"n":"curved-rail-b","d":0,"links":[{"n":"straight-rail","d":0,"x":2.0,"y":0.0}]}'
      .. ']'
  )
  prototypes = nil
end

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
