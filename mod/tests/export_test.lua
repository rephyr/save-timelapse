-- Unit tests for mod/export.lua, run outside Factorio against the fake in
-- fake_factorio.lua:
--
--   lua mod/tests/export_test.lua
--
-- The largest module in the mod and, until this file, the least tested. It
-- holds the write path every other module goes through, the rail sampling that
-- was silently empty twice before it worked, and the rule deciding which
-- surfaces are worth exporting at all.

package.path = (arg[0]:match("(.*/)") or "./") .. "../?.lua;" .. package.path
local fake = dofile((arg[0]:match("(.*/)") or "./") .. "fake_factorio.lua")

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

local function check_true(name, actual)
  check(name, not not actual, true)
end

local function load_export()
  for _, name in ipairs({ "export", "encode", "milestones" }) do
    package.loaded[name] = nil
  end
  return require("export")
end

-- Writing --------------------------------------------------------------------

fake.reset(100)
local export = load_export()

check("a write reports success", export.safe_write_file("a.stfr", "data", false), true)
check("and passes the bytes through unchanged", fake.written[1].data, "data")
check("with the append flag it was given", fake.written[1].append, false)

-- A capture that half wrote is worse than one that stopped: a frame missing
-- its middle reads as a factory that lost everything in it. The first failure
-- latches so nothing further is attempted.
fake.reset(100)
export = load_export()
_G.helpers.write_file = function() error("disk full") end
check("a failed write reports failure rather than raising", export.safe_write_file("a.stfr", "data", false), false)
check_true("and says so once, where a player can find it", fake.logged[1]:match("capture write failed") ~= nil)

local attempted = 0
_G.helpers.write_file = function() attempted = attempted + 1 end
check("every write after a failure is refused", export.safe_write_file("b.stfr", "more", true), false)
check("and not even attempted", attempted, 0)

-- The latch is per session, so a reload clears it: a disk that was full an
-- hour ago need not stay full.
fake.reset(100)
export = load_export()
check("a later session starts clean", export.safe_write_file("c.stfr", "data", false), true)

-- A checksum is folded from the same bytes that were written, so a writer
-- cannot accumulate one over bytes it did not write.
fake.reset(100)
export = load_export()
local encode = require("encode")
local checksum = export.checksummed_write("d.stfr", "hello", false, 0)
check("a checksummed write folds in exactly what it wrote", checksum, encode.checksum_update(0, "hello"))
check("and still writes it", fake.written[1].data, "hello")

-- Which surfaces are worth exporting -----------------------------------------

fake.reset(100)
export = load_export()
check("nauvis counts as inhabited even with nothing on it, being where you start", export.is_inhabited(fake.surface("nauvis")), true)
check(
  "an empty platform does not",
  export.is_inhabited(fake.surface("platform-1")),
  false
)
check(
  "one with something the player built does",
  export.is_inhabited(fake.surface("vulcanus", { fake.entity({ name = "assembling-machine-1" }) })),
  true
)

-- A surface that refuses to be searched is not a reason to fail an export.
local hostile = fake.surface("gleba")
hostile.find_entities_filtered = function() error("surface deleted mid-scan") end
check("a surface that errors is treated as uninhabited rather than crashing", export.is_inhabited(hostile), false)

-- Rail geometry ----------------------------------------------------------------

-- Sampled by prototype name rather than by type, because
-- `find_entities_filtered` raises on a type this game does not have and one
-- bad entry takes the whole call with it. That failure is invisible: the
-- sampling is pcall'd, so it came back empty and looked exactly like a game
-- with no rails on it.
fake.reset(100)
export = load_export()
_G.prototypes.entity = {
  ["straight-rail"] = { type = "straight-rail" },
  ["curved-rail-a"] = { type = "curved-rail-a" },
  ["kr-fancy-rail"] = { type = "straight-rail" },
  ["transport-belt"] = { type = "transport-belt" },
}

local asked_for = nil
local straight = fake.entity({ name = "straight-rail", x = 10, y = 23, direction = 0 })
local curve = fake.entity({
  name = "curved-rail-a",
  x = 10,
  y = 20,
  direction = 0,
  -- One neighbour, off one end, down one branch. The other seven combinations
  -- have nothing attached, which is the ordinary case.
  connections = { ["0:0"] = straight },
})

local surface = fake.surface("nauvis")
surface.find_entities_filtered = function(filter)
  asked_for = filter
  return { curve }
end
_G.game.surfaces = { nauvis = surface }

local samples = export.sample_rail_joints()

check_true("rails are asked for by name", asked_for.name ~= nil)
check("and never by type, which is what raised on a name this game lacks", asked_for.type, nil)
check("every rail prototype is asked for, whoever added it", #asked_for.name, 3)
check("a belt is not", (function()
  for _, name in ipairs(asked_for.name) do
    if name == "transport-belt" then return "asked for a belt" end
  end
  return "no"
end)(), "no")

check("one sample per rail with something attached", #samples, 1)
check("named for the piece it describes", samples[1].n, "curved-rail-a")
check("with its facing", samples[1].d, 0)
check("and its neighbour, positioned relative to it", samples[1].links[1].y, 3)
check("so the answer is the same wherever on the map it was read", samples[1].links[1].x, 0)

-- A branch that refuses is nothing attached there, not a rail that cannot be
-- read. `rail_connection_direction` includes `none`, and asking about it is
-- exactly the kind of combination the game may reject. Treating that as fatal
-- is what emptied this list the first two times it was tried.
fake.reset(100)
export = load_export()
_G.prototypes.entity = { ["straight-rail"] = { type = "straight-rail" } }
local partner = fake.entity({ name = "straight-rail", x = 0, y = 2 })
local touchy = fake.entity({
  name = "straight-rail",
  x = 0,
  y = 0,
  get_connected_rail = function(opts)
    if opts.rail_connection_direction == defines.rail_connection_direction.none then
      error("Entity is not rail-signal")
    end
    if opts.rail_direction == defines.rail_direction.front
      and opts.rail_connection_direction == defines.rail_connection_direction.straight then
      return partner
    end
    return nil
  end,
})
local touchy_surface = fake.surface("nauvis")
touchy_surface.find_entities_filtered = function() return { touchy } end
_G.game.surfaces = { nauvis = touchy_surface }

local survived = export.sample_rail_joints()
check("a refused branch does not lose the neighbours that did answer", #survived, 1)
check("and the one that answered is kept", survived[1].links[1].y, 2)
check_true("with the refusal reported once, not once per rail", (function()
  local refusals = 0
  for _, line in ipairs(fake.logged) do
    if line:match("rail branch refused") then refusals = refusals + 1 end
  end
  return refusals == 1
end)())

-- A piece with nothing attached says nothing about where its own ends are.
fake.reset(100)
export = load_export()
_G.prototypes.entity = { ["straight-rail"] = { type = "straight-rail" } }
local lonely = fake.entity({ name = "straight-rail", x = 0, y = 0 })
local empty_surface = fake.surface("nauvis")
empty_surface.find_entities_filtered = function() return { lonely } end
_G.game.surfaces = { nauvis = empty_surface }
check("a rail with nothing connected is not worth a sample", #export.sample_rail_joints(), 0)

-- A game with no rails at all, which is most space platforms.
fake.reset(100)
export = load_export()
_G.prototypes.entity = { ["transport-belt"] = { type = "transport-belt" } }
check("a game with no rail prototypes samples nothing", #export.sample_rail_joints(), 0)
check_true("and says why, rather than looking like a failure", fake.logged[1]:match("no rail prototypes") ~= nil)

-- A surface that refuses the scan costs only itself.
fake.reset(100)
export = load_export()
_G.prototypes.entity = { ["straight-rail"] = { type = "straight-rail" } }
local broken = fake.surface("nauvis")
broken.find_entities_filtered = function() error("no such name") end
local pair_b = fake.entity({ name = "straight-rail", x = 0, y = 2 })
local pair_a = fake.entity({ name = "straight-rail", x = 0, y = 0, connections = { ["0:0"] = pair_b } })
local working = fake.surface("vulcanus")
working.find_entities_filtered = function() return { pair_a } end
_G.game.surfaces = { nauvis = broken, vulcanus = working }

local mixed = export.sample_rail_joints()
check("a surface that refuses the scan does not empty the whole description", #mixed, 1)
check_true("and is named in the log", (function()
  for _, line in ipairs(fake.logged) do
    if line:match("rail scan failed on nauvis") then return true end
  end
  return false
end)())

-- The terrain scan's scenery pass ---------------------------------------------
--
-- The bug this exists for: the in-game pass bounds ore, trees and cliffs by the
-- factory's bounding box at the moment it runs, which for a live capture is the
-- baseline. A patch reached in hour ten was never inside any box, so it was
-- never recorded, and no later frame could rescue it. This pass runs against
-- the finished save instead, so it repairs recordings already made.

fake.reset(100)
export = load_export()
local ore = fake.entity({ name = "coal", type = "resource", x = 900, y = 900 })
local tree = fake.entity({ name = "tree-01", type = "tree", x = -400, y = 120 })
local nest = fake.entity({ name = "biter-spawner", type = "unit-spawner", x = -410, y = 130 })
local drill = fake.entity({ name = "electric-mining-drill", type = "mining-drill", x = 0, y = 0 })
_G.game.surfaces = { nauvis = fake.surface("nauvis", { drill, ore, tree, nest }) }

local tiles, surfaces, scenery = export.export_terrain(1, nil)
check("the scan reports the scenery it wrote", scenery, 3)
check("and still reports its surface, though the fake has no tiles to give", surfaces, 1)
check("tiles are counted separately from scenery", tiles, 0)

-- Joined rather than read from `written_to`, which keeps one write: a terrain
-- file is appended in pieces (header, scenery runs, the marker, tiles, then the
-- checksum), and the names live in whichever piece first mentioned them.
local function file_bytes(path)
  local parts = {}
  for _, write in ipairs(fake.written) do
    if write.path == path then
      parts[#parts + 1] = write.data
    end
  end
  return table.concat(parts)
end

local terrain_file = file_bytes("save-timelapse/terrain_nauvis.stfr")
check_true("ore far outside the baseline's box is in the file", terrain_file:find("coal", 1, true) ~= nil)
check_true("so are trees", terrain_file:find("tree-01", 1, true) ~= nil)
-- Scenery goes in the entity section; what the player built belongs to the
-- frames, which carry when each piece went down.
check("what the player built is not duplicated into it", terrain_file:find("electric-mining-drill", 1, true), nil)

-- Nests come from here and from nowhere else on a capture begun at the start.
-- A live baseline only sees chunks the game had generated by then, and
-- Factorio keeps nests out of the starting area, so leaving them to the
-- recording left a timelapse with no nests in it until biters expanded.
check_true("nests come from the scan, the baseline having none to give", terrain_file:find("biter%-spawner") ~= nil)

-- The ground under placed floor ----------------------------------------------
--
-- The bug this exists for: the scan reads the finished save, where concrete
-- laid at hour three is already concrete, and it wrote only what was not floor.
-- That left every paved position with nothing under it, so replayed from the
-- beginning a paved area was a hole until the tick the concrete went down,
-- rather than the grass it was laid on. Factorio keeps the covered tile so that
-- mining floor gives the ground back, so the scan asks for it.

fake.reset(100)
export = load_export()
local floor_proto = { items_to_place_this = { { name = "concrete" } }, mineable_properties = { minable = true } }
_G.prototypes.tile = {
  concrete = floor_proto,
  landfill = { items_to_place_this = { { name = "landfill" } }, mineable_properties = { minable = false } },
  ["grass-1"] = { mineable_properties = { minable = false } },
  water = { mineable_properties = { minable = false } },
}
local paver = fake.entity({ name = "electric-mining-drill", type = "mining-drill", x = 0, y = 0 })
_G.game.surfaces = {
  nauvis = fake.surface("nauvis", { paver }, {
    fake.tile({ name = "grass-1", x = 0, y = 0 }),
    fake.tile({ name = "concrete", x = 1, y = 0, hidden = "grass-1" }),
    -- Concrete over landfill over water: the deepest layer is what the lake
    -- was, and the one the replay should start from.
    fake.tile({ name = "concrete", x = 2, y = 0, hidden = "landfill", deep = "water" }),
    -- Paving with nothing kept under it. Skipped rather than guessed at.
    fake.tile({ name = "concrete", x = 3, y = 0 }),
  }),
}

check("ground is written for the bare tile and both recoverable paved ones", export.export_terrain(1, nil), 3)

terrain_file = file_bytes("save-timelapse/terrain_nauvis.stfr")
check_true("the grass under the concrete is in the file", terrain_file:find("grass-1", 1, true) ~= nil)
check_true("and the water under concrete over landfill", terrain_file:find("water", 1, true) ~= nil)
check("the floor itself is not: the frames carry when it went down", terrain_file:find("concrete", 1, true), nil)

-- A build with no such property raises on the read rather than answering nil,
-- which is why the read is probed. One failure must cost the paving it could
-- not see through and nothing else.

fake.reset(100)
export = load_export()
_G.prototypes.tile = { concrete = floor_proto, ["grass-1"] = { mineable_properties = { minable = false } } }
local opaque = setmetatable({ valid = true, name = "concrete", position = { x = 1, y = 0 } }, {
  __index = function(_, key) error("no property " .. key .. " on this build") end,
})
_G.game.surfaces = {
  nauvis = fake.surface("nauvis", { paver }, { fake.tile({ name = "grass-1", x = 0, y = 0 }), opaque }),
}

check("a build that cannot say what is under floor still writes the rest", export.export_terrain(1, nil), 1)

-- The scan describes the game again, for the rails ---------------------------
--
-- Rail corner shapes cannot be recovered from a recording, so they are sampled
-- from track that is actually placed. Live capture samples once, at the
-- baseline, and a playthrough with no track down yet samples nothing and never
-- looks again unless the modpack changes, so every corner built afterwards
-- draws as a square. The scanned save has the finished factory in it.

fake.reset(100)
_G.settings.startup["save-timelapse-terrain-scan"] = { value = true }
export = load_export()
_G.prototypes.entity = {
  ["straight-rail"] = { type = "straight-rail" },
  ["curved-rail-a"] = { type = "curved-rail-a" },
}
local joined = fake.entity({ name = "straight-rail", type = "straight-rail", x = 10, y = 23 })
local corner = fake.entity({
  name = "curved-rail-a",
  type = "curved-rail-a",
  x = 10,
  y = 20,
  connections = { ["0:0"] = joined },
})
_G.game.surfaces = { nauvis = fake.surface("nauvis", { corner, joined }) }

export.run_pending_tick_work(5, function() return 0x1234 end)

local described = nil
for _, write in ipairs(fake.written) do
  if write.path:find("prototypes.json", 1, true) then
    described = write.data
  end
end
check_true("the scan writes a description of its own", described ~= nil)
check_true("with the corner it found in it", described and described:find("curved%-rail%-a") ~= nil)

-- The per-surface cap is why this only ever half worked: the first N rails
-- found are whichever corner of the map is enumerated first, so orientations
-- used anywhere else were never sampled.
local asked_with = nil
local capped = fake.surface("nauvis", { corner, joined })
capped.find_entities_filtered = function(filter)
  asked_with = filter
  return { corner, joined }
end
_G.game.surfaces = { nauvis = capped }

export.sample_rail_joints(3000)
check("a live sample stays capped", asked_with.limit, 3000)
export.write_prototypes(0x1234, true)
check("the scan asks for every rail there is", asked_with.limit, nil)

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
