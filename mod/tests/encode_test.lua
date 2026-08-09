-- Unit tests for mod/encode.lua: the pure binary-encoding logic shared by
-- snapshot export and live capture. No Factorio required, run with:
--
--   lua mod/tests/encode_test.lua
--
-- Not wired into `cargo test`: that command's promise of needing nothing
-- beyond the Rust toolchain shouldn't start silently depending on `lua`.

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
  "STF1" .. bytes(1) .. encode.u64le(100) .. encode.str("nauvis"))

do
  local dict = encode.new_dictionary()
  local record = encode.frame_entity_record(dict, {
    name = "transport-belt",
    position = { x = -80.5, y = 28.5 },
    direction = 4,
  })
  local expected = bytes(0) .. encode.str("transport-belt") -- DefineName, first use
    .. bytes(1) .. encode.u16le(0) .. encode.i32le(-805) .. encode.i32le(285) .. bytes(4) .. bytes(1) .. bytes(1)
  check("frame_entity_record: direction present, defaults w/h to 1x1", record, expected)
end

do
  local dict = encode.new_dictionary()
  encode.dictionary_id(dict, "assembling-machine-1", 0) -- pre-seed so this record doesn't redefine it
  local record = encode.frame_entity_record(dict, {
    name = "assembling-machine-1",
    position = { x = 5, y = 5 },
    direction = 0,
    tile_width = 3,
    tile_height = 3,
  })
  local expected = bytes(1) .. encode.u16le(0) .. encode.i32le(50) .. encode.i32le(50) .. bytes(0) .. bytes(3) .. bytes(3)
  check("frame_entity_record: an already defined name is not redefined", record, expected)
end

do
  local dict = encode.new_dictionary()
  local record = encode.frame_tile_record(dict, { name = "concrete", position = { x = -5, y = -12 } })
  local expected = bytes(0) .. encode.str("concrete")
    .. bytes(2) .. encode.u16le(0) .. encode.i32le(-5) .. encode.i32le(-12)
  check("frame_tile_record: integer coordinates, no rounding", record, expected)
end

check("frame_end_entities: tag 9, no payload", encode.frame_end_entities(), bytes(9))

-- live capture event format

check("event_header: magic and version", encode.event_header(), "STE1" .. bytes(1))
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

-- Position is always sent on removal, even when id is available too: an
-- entity that already existed when the baseline was taken carries its real
-- (pre-existing) unit_number when Factorio reports it removed, but replay's
-- world state never learned that number from a snapshot, which records no
-- ids. Sending id alone, as the JSON format once did, made every such
-- removal an unresolvable no-op. See mod/capture.lua's request_baseline/
-- perform_baseline for the other half of this.
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

-- is_capture_rollback

check("is_capture_rollback: normal forward play is not a rollback",
  encode.is_capture_rollback(5000, 5010),
  false)

check("is_capture_rollback: resuming exactly where recording left off is not a rollback",
  encode.is_capture_rollback(5000, 5000),
  false)

check("is_capture_rollback: loading an older save is a rollback",
  encode.is_capture_rollback(5000, 3000),
  true)

-- Reloading the same save twice in a row resumes at a tick the current
-- segment already starts at, so this has to report a rollback on the tick
-- comparison alone. Reading it back off the segment start would find them
-- equal and append the second attempt into the first attempt's file.
check("is_capture_rollback: replaying from the same save again is still a rollback",
  encode.is_capture_rollback(21000, 20000),
  true)

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

check("capture_segment_name: an explicit seq of 0 still gets the plain name",
  encode.capture_segment_name(0x1a2b3c, 22760790, 0),
  "001a2b3c/events_22760790.stev")

-- Reloading the same save twice: same start tick, so only the seq keeps the
-- second attempt out of the first attempt's file.
check("capture_segment_name: a rollover past the first is distinguished by seq",
  encode.capture_segment_name(0x1a2b3c, 22760790, 2),
  "001a2b3c/events_22760790_2.stev")

check("capture_segment_basename: no session folder, same naming rule",
  encode.capture_segment_basename(22760790, 1),
  "events_22760790_1.stev")

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

-- The empty case itself is never actually written: export.lua's callers
-- skip the write_file call entirely when nobody was sampled. Still worth
-- pinning here as a value, since something downstream could start calling
-- this with an empty list directly.
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
-- Enemies are wildlife, not factory, and their deaths would otherwise flood
-- live capture with removals unrelated to construction. Regression: a real
-- capture showed ~6% of exported entities were biters/spitters/spawners
-- before these were added.
check("EXCLUDED_TYPES: contains unit (biters, spitters)", excluded["unit"], true)
check("EXCLUDED_TYPES: contains unit-spawner", excluded["unit-spawner"], true)

local floor = assert_no_duplicates(encode.PLACED_FLOOR_TILES, "PLACED_FLOOR_TILES")
check("PLACED_FLOOR_TILES: contains concrete", floor["concrete"], true)
check("PLACED_FLOOR_TILES: contains landfill", floor["landfill"], true)
check("PLACED_FLOOR_TILES: contains stone-path", floor["stone-path"], true)

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
