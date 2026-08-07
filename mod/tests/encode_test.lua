-- Unit tests for mod/encode.lua: the pure binary-encoding logic shared by
-- snapshot export and live capture. No Factorio required -- run with:
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

-- low level byte packing -----------------------------------------------------

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

-- round10 ----------------------------------------------------------------------

check("round10: a positive value already at one decimal", encode.round10(28.5), 285)
check("round10: a negative value already at one decimal", encode.round10(-80.5), -805)
check("round10: zero", encode.round10(0), 0)
check("round10: a whole number", encode.round10(327.0), 3270)

-- dictionary_id ------------------------------------------------------------------

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

-- frame format -------------------------------------------------------------------

check("frame_header: magic, tick, surface", encode.frame_header(100, "nauvis"),
  "STF1" .. encode.u64le(100) .. encode.str("nauvis"))

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

-- live capture event format -------------------------------------------------------

check("event_header: just the magic", encode.event_header(), "STE1")
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
-- removal an unresolvable no-op. See mod/control.lua's ensure_baseline for
-- the other half of this.
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

-- next_capture_segment -----------------------------------------------------------

check("next_capture_segment: normal forward play keeps the segment",
  encode.next_capture_segment(5000, 5010, 100),
  100)

check("next_capture_segment: resuming exactly where recording left off keeps the segment",
  encode.next_capture_segment(5000, 5000, 100),
  100)

check("next_capture_segment: loading an older save starts a new segment at the resumed tick",
  encode.next_capture_segment(5000, 3000, 100),
  3000)

-- list sanity ------------------------------------------------------------------

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
check("EXCLUDED_TYPES: contains tree", excluded["tree"], true)
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

-- ------------------------------------------------------------------------------

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
