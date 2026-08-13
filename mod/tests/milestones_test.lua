-- Unit tests for mod/milestones.lua, run outside Factorio against the fake in
-- fake_factorio.lua:
--
--   lua mod/tests/milestones_test.lua
--
-- Milestones are polled rather than evented, the game exposing no "an
-- assembling machine finished an item" event, so the only thing keeping a
-- marker from being written on every single flush is the record of what has
-- already been seen. That record is what this tests.

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

local function load_milestones()
  for _, name in ipairs({ "milestones", "export", "encode" }) do
    package.loaded[name] = nil
  end
  return require("milestones")
end

--- Every milestone line written so far, newest last.
local function markers()
  local lines = {}
  for _, write in ipairs(fake.written) do
    if write.path:match("milestones%.jsonl$") then
      lines[#lines + 1] = write.data
    end
  end
  return lines
end

--- A force whose production statistics report `counts` on every surface.
local function force_producing(counts)
  return {
    name = "player",
    get_item_production_statistics = function()
      return { input_counts = counts }
    end,
  }
end

-- Recording once ---------------------------------------------------------------

fake.reset(600)
local milestones = load_milestones()

check("a milestone is recorded the first time", milestones.record(600, "science", "automation-science-pack", nil), true)
check("and written out", #markers(), 1)
check_true("carrying the tick it happened at", markers()[1]:match("600") ~= nil)

check("the same one again is a no-op", milestones.record(900, "science", "automation-science-pack", nil), false)
check("writing nothing", #markers(), 1)

check("a different milestone of the same kind is its own", milestones.record(900, "science", "logistic-science-pack", nil), true)
check("as is the same id under a different kind", milestones.record(900, "planet", "logistic-science-pack", nil), true)
check("so three lines in total", #markers(), 3)

-- What was already seen lives in `storage`, so it survives a save and rewinds
-- with one. Without that every load would rewrite every marker it had ever
-- written.
check_true("the record of what was seen is persisted", _G.storage.timelapse_milestones ~= nil)

fake.reset(600)
milestones = load_milestones()
check("a fresh game has seen nothing", milestones.record(600, "science", "automation-science-pack", nil), true)

-- Science ------------------------------------------------------------------------

fake.reset(600)
milestones = load_milestones()
_G.game.forces = { player = force_producing({ ["automation-science-pack"] = 12, ["iron-plate"] = 5000 }) }
_G.game.surfaces = { nauvis = fake.surface("nauvis") }

milestones.poll_science(600, nil)
check("a science pack that has been produced is a milestone", #markers(), 1)
check_true("named for the pack", markers()[1]:match("automation%-science%-pack") ~= nil)

milestones.poll_science(1200, nil)
check("polling again records nothing new, which is what makes polling safe", #markers(), 1)

-- Statistics are per surface in 2.0, so a pack produced only on Vulcanus still
-- counts. The union is the point.
fake.reset(600)
milestones = load_milestones()
local per_surface = {
  nauvis = { ["automation-science-pack"] = 10 },
  vulcanus = { ["metallurgic-science-pack"] = 3 },
}
_G.game.forces = {
  player = {
    name = "player",
    get_item_production_statistics = function(surface)
      return { input_counts = per_surface[surface.name] or {} }
    end,
  },
}
_G.game.surfaces = { nauvis = fake.surface("nauvis"), vulcanus = fake.surface("vulcanus") }
milestones.poll_science(600, nil)
check("science is unioned across surfaces, statistics being per surface", #markers(), 2)

-- Nothing produced yet is not a milestone, and neither is something that is
-- not a science pack.
fake.reset(600)
milestones = load_milestones()
_G.game.forces = { player = force_producing({ ["automation-science-pack"] = 0, ["iron-plate"] = 99999 }) }
_G.game.surfaces = { nauvis = fake.surface("nauvis") }
milestones.poll_science(600, nil)
check("a pack at zero has not been produced", #markers(), 0)

-- A force that will not report statistics costs a marker, never the game.
fake.reset(600)
milestones = load_milestones()
_G.game.forces = {
  player = { name = "player", get_item_production_statistics = function() error("no statistics on this surface") end },
}
_G.game.surfaces = { nauvis = fake.surface("nauvis") }
milestones.poll_science(600, nil)
check("statistics that refuse cost a marker rather than raising", #markers(), 0)

-- Planets ---------------------------------------------------------------------

fake.reset(600)
milestones = load_milestones()
_G.game.connected_players = {
  { name = "you", surface = { name = "vulcanus", planet = {} } },
}
milestones.poll_planets(600, nil)
check("standing on a planet is reaching it", #markers(), 1)
check_true("named for the planet", markers()[1]:match("vulcanus") ~= nil)

-- Checked against `surface.planet` rather than by name, so a platform in
-- transit is not somewhere you arrived.
fake.reset(600)
milestones = load_milestones()
_G.game.connected_players = {
  { name = "you", surface = { name = "platform-1", planet = nil } },
}
milestones.poll_planets(600, nil)
check("a space platform is not a planet, whatever it is called", #markers(), 0)

-- Rockets -----------------------------------------------------------------------

fake.reset(600)
milestones = load_milestones()
milestones.on_rocket_launched({ tick = 600 }, nil)
milestones.on_rocket_launched({ tick = 90000 }, nil)
check("only the first rocket is a milestone", #markers(), 1)
check_true("recorded at the tick it launched", markers()[1]:match("600") ~= nil)

-- Resetting ---------------------------------------------------------------------

fake.reset(600)
milestones = load_milestones()
milestones.record(600, "science", "automation-science-pack", nil)
milestones.reset()
check(
  "resetting forgets what was seen, or a new recording would never rewrite its markers",
  milestones.record(600, "science", "automation-science-pack", nil),
  true
)

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
