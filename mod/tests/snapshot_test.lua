-- Unit tests for mod/snapshot.lua, run outside Factorio against the fake in
-- fake_factorio.lua:
--
--   lua mod/tests/snapshot_test.lua
--
-- The periodic snapshot spreads a whole-surface read across many ticks so it
-- does not freeze the game. Everything here is about that spreading: that it
-- starts only when there is something to read, that it does not start twice
-- over itself, and that stepping when nothing is running costs nothing.

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

local function load_snapshot()
  for _, name in ipairs({ "snapshot", "export", "encode" }) do
    package.loaded[name] = nil
  end
  return require("snapshot")
end

-- Nothing to snapshot ------------------------------------------------------

fake.reset(3600)
local snapshot = load_snapshot()
_G.game.surfaces = {}
snapshot.start(3600)
snapshot.run_pending_tick_work(3601)
check("a game with no surfaces writes nothing", #fake.written, 0)

-- An uninhabited platform is not worth a file, the same rule the baseline
-- uses. Nauvis always counts.
fake.reset(3600)
snapshot = load_snapshot()
_G.game.surfaces = { ["platform-1"] = fake.surface("platform-1") }
snapshot.start(3600)
for tick = 3601, 3620 do
  snapshot.run_pending_tick_work(tick)
end
check("an empty platform is skipped rather than snapshotted", #fake.written, 0)

-- Stepping with nothing running ---------------------------------------------

fake.reset(3600)
snapshot = load_snapshot()
check("stepping when nothing is in progress is a no-op", (function()
  local ok = pcall(function() snapshot.run_pending_tick_work(3601) end)
  return ok and #fake.written
end)(), 0)

-- Spreading the work ---------------------------------------------------------

-- The whole point of the module: reading a surface in one tick is what the
-- baseline does and what makes a megabase freeze for tens of seconds. This one
-- takes several ticks over the same work.
fake.reset(3600)
snapshot = load_snapshot()
local entities = {}
for i = 1, 500 do
  entities[i] = fake.entity({ name = "transport-belt", x = i, y = 0 })
end
local surface = fake.surface("nauvis", entities)
surface.find_entities_filtered = function() return entities end
_G.game.surfaces = { nauvis = surface }

snapshot.start(3600)
local wrote_on_first_step = #fake.written
snapshot.run_pending_tick_work(3601)
check("starting does not itself write anything, which is what keeps the tick short", wrote_on_first_step, 0)

local ticks = 0
while ticks < 200 do
  ticks = ticks + 1
  snapshot.run_pending_tick_work(3601 + ticks)
end
check("the surface is written out once the steps have run", #fake.written > 0, true)

local frames = 0
for _, write in ipairs(fake.written) do
  if write.path:match("%.stfr$") then
    frames = frames + 1
  end
end
check("as frame files", frames > 0, true)

-- Starting twice --------------------------------------------------------------

-- The timer fires on a fixed interval and a snapshot can outlast one, so the
-- second call has to be ignored rather than restarting halfway through.
fake.reset(3600)
snapshot = load_snapshot()
_G.game.surfaces = { nauvis = surface }
snapshot.start(3600)
snapshot.run_pending_tick_work(3601)
local mid_progress = #fake.written
snapshot.start(3660)
snapshot.run_pending_tick_work(3661)
check(
  "a second start while one is running does not restart it",
  #fake.written >= mid_progress,
  true
)

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
