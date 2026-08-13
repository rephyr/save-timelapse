-- Unit tests for mod/control.lua, run outside Factorio against the fake in
-- fake_factorio.lua:
--
--   lua mod/tests/control_test.lua
--
-- control.lua is mostly wiring, and the one piece of logic in it guards a trap
-- in Factorio's own API: `on_nth_tick` keeps exactly one handler per interval
-- and silently replaces any earlier one. The capture flush runs every 600
-- ticks and the snapshot interval is given in seconds, so setting that to 10
-- collides by coincidence and one feature would stop working with no error.

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

local function load_control()
  for _, name in ipairs({ "control", "capture", "export", "encode", "milestones", "snapshot", "gui" }) do
    package.loaded[name] = nil
  end
  dofile((arg[0]:match("(.*/)") or "./") .. "../control.lua")
end

local function intervals()
  local found = {}
  for interval, handler in pairs(_G.script.nth_ticks) do
    if handler then
      found[#found + 1] = interval
    end
  end
  table.sort(found)
  return table.concat(found, ",")
end

-- With capture on and no snapshot timer ------------------------------------

fake.reset(1000)
_G.settings.global["save-timelapse-live-capture"] = { value = true }
_G.settings.global["save-timelapse-snapshot-seconds"] = { value = 0 }
load_control()
_G.script.loaded()

check("the capture flush is the only timer", intervals(), "600")

-- Capture off ----------------------------------------------------------------

fake.reset(1000)
_G.settings.global["save-timelapse-live-capture"] = { value = false }
_G.settings.global["save-timelapse-snapshot-seconds"] = { value = 0 }
load_control()
_G.script.loaded()
check("nothing is subscribed while capture is off, so an idle mod costs nothing", intervals(), "")

-- The collision ----------------------------------------------------------------

-- 10 seconds is 600 ticks, which is also the capture flush interval. Factorio
-- would keep whichever was registered second.
fake.reset(1000)
_G.settings.global["save-timelapse-live-capture"] = { value = true }
_G.settings.global["save-timelapse-snapshot-seconds"] = { value = 10 }
load_control()
_G.script.loaded()

check("two features wanting one interval subscribe once", intervals(), "600")

-- and the one handler has to do both jobs. Driving it is the only way to tell
-- a chained handler from one that replaced the other.
_G.game.surfaces = { nauvis = fake.surface("nauvis", { fake.entity({ name = "assembling-machine-1" }) }) }
_G.script.nth_ticks[600]({ tick = 1200 })

local wrote_capture, started_snapshot = false, false
for _, path in ipairs(fake.paths()) do
  if path:match("%.stev$") then wrote_capture = true end
end
-- The snapshot writes nothing on the tick it starts, spreading its work, so
-- the evidence it ran is that a later step produces a frame. Stepped through
-- the same module instance control.lua loaded: requiring a fresh one would
-- discard the very state the handler just created.
local snapshot = require("snapshot")
for tick = 1201, 1400 do
  snapshot.run_pending_tick_work(tick)
end
for _, path in ipairs(fake.paths()) do
  if path:match("%.stfr$") then started_snapshot = true end
end

check("the capture flush still ran", wrote_capture, true)
check("and so did the snapshot, rather than one silently replacing the other", started_snapshot, true)

-- Releasing --------------------------------------------------------------------

fake.reset(1000)
_G.settings.global["save-timelapse-live-capture"] = { value = true }
_G.settings.global["save-timelapse-snapshot-seconds"] = { value = 30 }
load_control()
_G.script.loaded()
check("two different intervals are both subscribed", intervals(), "600,1800")

_G.settings.global["save-timelapse-snapshot-seconds"] = { value = 0 }
_G.script.handlers[_G.defines.events.on_runtime_mod_setting_changed]({ setting = "save-timelapse-snapshot-seconds" })
check("turning one off releases its interval rather than leaving it firing", intervals(), "600")

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
