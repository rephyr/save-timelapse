-- Unit tests for mod/encode.lua: the pure JSON-encoding logic shared by
-- snapshot export and live capture. No Factorio required -- run with:
--
--   lua mod/tests/encode_test.lua
--
-- Not wired into `cargo test`: that command's promise of needing nothing
-- beyond the Rust toolchain shouldn't start silently depending on `lua`.

local script_dir = arg[0]:match("(.*/)") or "./"
local encode = dofile(script_dir .. "../encode.lua")

local failures = 0

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

-- quote --------------------------------------------------------------------

check("quote: plain string", encode.quote("transport-belt"), '"transport-belt"')
check("quote: embedded quote is escaped", encode.quote('has "quotes"'), '"has \\"quotes\\""')
check("quote: embedded backslash is escaped", encode.quote("back\\slash"), '"back\\\\slash"')

-- encode_entity --------------------------------------------------------------

check("encode_entity: direction present and nonzero",
  encode.encode_entity({ name = "transport-belt", position = { x = -80.5, y = 28.5 }, direction = 4 }),
  '{"n":"transport-belt","x":-80.5,"y":28.5,"d":4}')

check("encode_entity: direction zero is omitted",
  encode.encode_entity({ name = "stone-furnace", position = { x = 1, y = 2 }, direction = 0 }),
  '{"n":"stone-furnace","x":1.0,"y":2.0}')

check("encode_entity: direction absent (nil) is omitted",
  encode.encode_entity({ name = "stone-furnace", position = { x = 1, y = 2 } }),
  '{"n":"stone-furnace","x":1.0,"y":2.0}')

check("encode_entity: 1x1 footprint is omitted",
  encode.encode_entity({ name = "inserter", position = { x = 0, y = 0 }, tile_width = 1, tile_height = 1 }),
  '{"n":"inserter","x":0.0,"y":0.0}')

check("encode_entity: multi-tile footprint is included",
  encode.encode_entity({
    name = "assembling-machine-1", position = { x = 5, y = 5 }, direction = 0,
    tile_width = 3, tile_height = 3,
  }),
  '{"n":"assembling-machine-1","x":5.0,"y":5.0,"w":3,"h":3}')

-- encode_tile ----------------------------------------------------------------

check("encode_tile: positive coordinates",
  encode.encode_tile({ name = "concrete", position = { x = 10, y = 20 } }),
  '{"n":"concrete","x":10,"y":20}')

check("encode_tile: negative coordinates",
  encode.encode_tile({ name = "stone-path", position = { x = -5, y = -12 } }),
  '{"n":"stone-path","x":-5,"y":-12}')

-- encode_event -----------------------------------------------------------------

check("encode_event: entity add with id and direction",
  encode.encode_event("+", "e", 1234, "transport-belt", 10.5, 20.5, 4, 8842),
  '{"t":1234,"op":"+","k":"e","n":"transport-belt","x":10.5,"y":20.5,"d":4,"id":8842}')

check("encode_event: entity add with direction zero omits d",
  encode.encode_event("+", "e", 1234, "stone-furnace", 1, 2, 0, 8842),
  '{"t":1234,"op":"+","k":"e","n":"stone-furnace","x":1.0,"y":2.0,"id":8842}')

check("encode_event: entity add with multi-tile footprint",
  encode.encode_event("+", "e", 1234, "assembling-machine-1", 5, 5, 0, 8842, 3, 3),
  '{"t":1234,"op":"+","k":"e","n":"assembling-machine-1","x":5.0,"y":5.0,"w":3,"h":3,"id":8842}')

check("encode_event: entity add with 1x1 footprint omits w/h",
  encode.encode_event("+", "e", 1234, "inserter", 1, 2, 0, 8842, 1, 1),
  '{"t":1234,"op":"+","k":"e","n":"inserter","x":1.0,"y":2.0,"id":8842}')

check("encode_event: entity remove with id is the short form",
  encode.encode_event("-", "e", 1250, nil, nil, nil, nil, 8842),
  '{"t":1250,"op":"-","k":"e","id":8842}')

check("encode_event: entity remove without id falls back to position",
  encode.encode_event("-", "e", 1250, nil, 10.5, 20.5, nil, nil),
  '{"t":1250,"op":"-","k":"e","x":10.5,"y":20.5}')

check("encode_event: tile add",
  encode.encode_event("+", "t", 1300, "concrete", 10, 20, nil, nil),
  '{"t":1300,"op":"+","k":"t","n":"concrete","x":10,"y":20}')

check("encode_event: tile remove is always position keyed",
  encode.encode_event("-", "t", 1310, nil, 10, 20, nil, nil),
  '{"t":1310,"op":"-","k":"t","x":10,"y":20}')

-- next_capture_segment ---------------------------------------------------------

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
