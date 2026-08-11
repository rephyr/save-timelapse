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

-- Reload handling deliberately has no test here, because the mod deliberately
-- does not attempt it. Every value that could reveal a reload lives in
-- `storage`, which Factorio saves inside the save file and therefore rewinds
-- along with it, so a comparison against the resumed tick is always false.
-- See the note above `event_reset_dictionaries` in encode.lua; reloads are
-- resolved on the reading side by `event::segment_run_bounds`.

-- milestones

check("milestone_line: tick, kind and id as one JSON line",
  encode.milestone_line(1234, "science", "logistic-science-pack"),
  '{"tick":1234,"kind":"science","id":"logistic-science-pack"}\n')

check("is_science_pack: matches on the suffix", encode.is_science_pack("logistic-science-pack"), true)
check("is_science_pack: a modded pack is picked up for free",
  encode.is_science_pack("se-deep-space-science-pack"), true)
check("is_science_pack: an ordinary item is not one", encode.is_science_pack("iron-plate"), false)

-- The from-saves half of milestones. A save knows only totals, never when
-- something first became true, so the mod reports state here and src/milestone.rs
-- recovers the timings by comparing consecutive saves. Rockets is a count
-- rather than a flag so that diff can tell a first launch from launches that
-- had been happening all along.
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

  -- The two that show why a fraction of the larger dimension cannot work.
  -- Both have a 2:1 side ratio and each needs an order of magnitude
  -- different padding, because what a fit exposes depends on the base's
  -- shape against the frame's, not on how big it is.
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

  -- The bug this guards against, and it was in the budget rather than the
  -- margin: as a flat ceiling, any base bigger than the ceiling itself had
  -- nothing left to spend, the affordable width came out negative, and it
  -- fell back to the 32 tile floor. The largest factories got the smallest
  -- margins, which is precisely backwards. A 5000x5000 base has a 25M tile
  -- footprint and must still be given what its shape asks for.
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
-- Every flying robot type. These are the highest-volume mobile entity in the
-- game: a megabase mid construction job has tens of thousands airborne, each
-- one a record in the baseline and in every from-saves frame, pinned wherever
-- it happened to be since the format cannot update a position.
check("EXCLUDED_TYPES: contains construction-robot", excluded["construction-robot"], true)
check("EXCLUDED_TYPES: contains logistic-robot", excluded["logistic-robot"], true)
check("EXCLUDED_TYPES: contains combat-robot", excluded["combat-robot"], true)
-- The stationary infrastructure that flies them stays: it is the part that
-- actually shows the factory growing.
check("EXCLUDED_TYPES: does not contain roboport", excluded["roboport"], nil)
-- Mobile enemies stay excluded: their combat deaths would flood live capture
-- with removals unrelated to construction, and since this format records
-- construction and destruction but never movement, a captured biter would sit
-- frozen wherever it was first logged. Regression: a real capture showed ~6%
-- of exported entities were biters/spitters/spawners before these were added,
-- and that bulk was the mobile units rather than the nests that spawn them.
check("EXCLUDED_TYPES: contains unit (biters, spitters)", excluded["unit"], true)
-- Asteroid chunks drift and are collected continuously, and are never built,
-- so every one logs a removal for something replay never had. On a real
-- five-platform capture that was 6,101 of 6,259 events.
check("EXCLUDED_TYPES: contains asteroid-chunk", excluded["asteroid-chunk"], true)
-- ...but the collector is a structure worth watching go up.
check("EXCLUDED_TYPES: does not contain asteroid-collector", excluded["asteroid-collector"], nil)
-- Nests are deliberately captured despite being enemies: stationary, so the
-- format represents them honestly, few enough to cost little, and watching
-- them get cleared is how expansion actually reads in a timelapse. The viewer
-- colors them red (see viewer/src/registry.rs's is_enemy).
check("EXCLUDED_TYPES: does not contain unit-spawner", excluded["unit-spawner"], nil)
-- Space Age gave its own mobile enemies prototype types of their own, so
-- "unit" above never covered them and every one landed in captures as though
-- somebody had built it. Found in a real Gleba capture holding
-- small-stomper-pentapod, small-strafer-pentapod and both their leg
-- prototypes; because they roam, the auto-follow camera stretched to wherever
-- they had wandered. Read from the game's own prototypes rather than guessed:
-- space-age/prototypes/entity/enemies.lua.
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
-- Asked of the game rather than listed, because the list is Wube's names and
-- was the last place a mod could not be seen. `space-platform-foundation` is
-- the case that proves it and is not even modded: a platform's own floor was
-- missing, so the tiles a player lays to grow a platform were recorded as
-- natural ground and the platform appeared fully formed rather than growing.
--
-- Two properties unioned, because on a real 69 mod game neither covers it
-- alone: nothing places a coloured refined concrete and nothing reports it
-- minable, while the stated list knows nothing of any mod.

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
-- apart by whether anything is above 1, so this has to as well. Assuming
-- floats put an already-byte-ranged prototype colour out of byte range (61
-- became 15555), which the reader could not parse at all: on an Alien Biomes
-- save 357 of 364 tiles were unreadable, and a single one of them was enough
-- to make the desktop side discard the whole file and colour the playthrough
-- from its built-in table.

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
-- preferring it painted an entire factory, belts and walls and radars alike,
-- in biter red.
--
-- The type of every entity goes out verbatim, which is what lets the desktop
-- side recognise a modded belt as a belt without this file, or that one,
-- naming it. Reach is asked of underground belts and pipes and nothing else.

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
    "prototypes_json: colours, types and reach, each sorted",
    encode.prototypes_json(),
    '{"tiles":{"cerys-refined-concrete":[100,100,100],"grass-1":[55,53,11],"water":[51,83,95]},'
      .. '"entities":{"biter-spawner":[255,25,25],"kr-advanced-underground-belt":[0,93,147],'
      .. '"radar":[0,93,147],"small-worm-turret":[255,25,25],"transport-belt":[204,161,71]},'
      .. '"types":{"biter-spawner":"unit-spawner","colourless":"container",'
      .. '"kr-advanced-underground-belt":"underground-belt","radar":"radar",'
      .. '"small-worm-turret":"turret","transport-belt":"transport-belt"},'
      .. '"reach":{"kr-advanced-underground-belt":30},'
      -- Named so the desktop side splits a baseline's tiles the way this
      -- capture recorded them, rather than from a list of its own that would
      -- disagree the moment a mod adds a floor.
      .. '"floor":["cerys-refined-concrete"]}'
  )
  prototypes = nil
end

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
