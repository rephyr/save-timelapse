-- Unit tests for mod/gui.lua, run outside Factorio against the fake in
-- fake_factorio.lua:
--
--   lua mod/tests/gui_test.lua
--
-- The panel is mostly layout, which is not worth asserting on. What is worth
-- asserting on is the part with a rule behind it: which actions a non-admin
-- may take in multiplayer, and that a refused one leaves nothing changed
-- rather than half changed.

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

local PANEL = "save-timelapse-panel"
local LIVE_CAPTURE_CHECKBOX = "save-timelapse-toggle-live-capture"
local CLOSE_BUTTON = "save-timelapse-panel-close"
local GENERATE_BUTTON = "save-timelapse-generate-baseline"
local GENERATE_NOTE = "save-timelapse-generate-note"
local STATUS_TEXT = "save-timelapse-status-text"
local ROW_STATE = "save-timelapse-row-state"

local function load_gui()
  for _, name in ipairs({ "gui", "capture", "export", "encode", "milestones" }) do
    package.loaded[name] = nil
  end
  return require("gui")
end

--- A game with one player in it, single player unless `multiplayer`.
local function with_player(fields)
  local player = fake.player(fields)
  _G.game.get_player = function() return player end
  _G.game.is_multiplayer = function() return fields and fields.multiplayer or false end
  _G.game.planets = {}
  _G.game.surfaces = { nauvis = fake.surface("nauvis") }
  return player
end

-- Opening and closing ----------------------------------------------------------

fake.reset(1000)
local gui = load_gui()
local player = with_player()

check("no panel to begin with", player.gui.screen[PANEL], nil)
gui.toggle(1)
check_true("toggling opens one", player.gui.screen[PANEL] ~= nil)
gui.toggle(1)
check("toggling again closes it", player.gui.screen[PANEL], nil)

gui.open(1)
check_true("opening works on its own", player.gui.screen[PANEL] ~= nil)
gui.open(1)
check_true("and opening an already open panel leaves one, not two", player.gui.screen[PANEL] ~= nil)

-- Closing by the button is the same as closing any other way.
fake.reset(1000)
gui = load_gui()
player = with_player()
gui.open(1)
local close = fake.find(player.gui.screen, CLOSE_BUTTON)
check_true("the panel has a close button", close ~= nil)
gui.on_gui_click({ player_index = 1, element = close })
check("clicking it closes the panel", player.gui.screen[PANEL], nil)

-- The admin gate -----------------------------------------------------------------

-- Single player: there is nobody to be an admin over, so nothing is gated.
fake.reset(1000)
gui = load_gui()
player = with_player({ admin = false })
_G.settings.global["save-timelapse-live-capture"] = { value = false }
gui.open(1)
local checkbox = fake.find(player.gui.screen, LIVE_CAPTURE_CHECKBOX)
checkbox.state = true
gui.on_gui_checked_state_changed({ player_index = 1, element = checkbox })
check("in single player anybody may turn capture on", _G.settings.global["save-timelapse-live-capture"].value, true)

-- Multiplayer, not an admin: refused, and told why.
fake.reset(1000)
gui = load_gui()
player = with_player({ admin = false, multiplayer = true })
_G.settings.global["save-timelapse-live-capture"] = { value = false }
gui.open(1)
checkbox = fake.find(player.gui.screen, LIVE_CAPTURE_CHECKBOX)
checkbox.state = true
gui.on_gui_checked_state_changed({ player_index = 1, element = checkbox })

check("a non-admin cannot change it in multiplayer", _G.settings.global["save-timelapse-live-capture"].value, false)
check(
  "and the checkbox is put back, so it does not show a state the game is not in",
  checkbox.state,
  false
)
check_true("with a reason, rather than silently doing nothing", player.printed[1]:match("only admins") ~= nil)

-- Multiplayer admin: allowed.
fake.reset(1000)
gui = load_gui()
player = with_player({ admin = true, multiplayer = true })
_G.settings.global["save-timelapse-live-capture"] = { value = false }
gui.open(1)
checkbox = fake.find(player.gui.screen, LIVE_CAPTURE_CHECKBOX)
checkbox.state = true
gui.on_gui_checked_state_changed({ player_index = 1, element = checkbox })
check("an admin may", _G.settings.global["save-timelapse-live-capture"].value, true)

-- Per-surface exclusion is deliberately ungated: low stakes, reversible, and
-- with no vanilla precedent for restricting it.
fake.reset(1000)
gui = load_gui()
player = with_player({ admin = false, multiplayer = true })
local capture = require("capture")
gui.open(1)
gui.on_gui_checked_state_changed({
  player_index = 1,
  element = { valid = true, name = "surface-row", tags = { surface = "vulcanus" }, state = false },
})
check("a non-admin may still exclude a surface", capture.is_surface_excluded("vulcanus"), true)
gui.on_gui_checked_state_changed({
  player_index = 1,
  element = { valid = true, name = "surface-row", tags = { surface = "vulcanus" }, state = true },
})
check("and include it again", capture.is_surface_excluded("vulcanus"), false)

-- What the panel says ----------------------------------------------------------

-- The button used to read "Generate", which named nothing it made and did
-- nothing at all in two of its three states. Each state is asserted on here
-- because each one used to be an identical, silent button.

fake.reset(1000)
gui = load_gui()
player = with_player()
_G.settings.global["save-timelapse-live-capture"] = { value = false }
gui.open(1)
check("with recording off there is nothing to snapshot", fake.find(player.gui.screen, GENERATE_BUTTON).enabled, false)
check("and the panel says why", fake.find(player.gui.screen, GENERATE_NOTE).caption, "Turn recording on first.")
check("the status line leads with it", fake.find(player.gui.screen, STATUS_TEXT).caption, "Not recording")

-- A place that has never been snapshotted is what the button is for, and
-- naming it on the button is the whole difference from "Generate".
gui.close(1)
_G.settings.global["save-timelapse-live-capture"] = { value = true }
gui.open(1)
check("a place waiting for one names itself", fake.find(player.gui.screen, GENERATE_BUTTON).caption, "Snapshot Nauvis")
check_true("and the button can be pressed", fake.find(player.gui.screen, GENERATE_BUTTON).enabled)
check("the row agrees with the button", fake.find(player.gui.screen, ROW_STATE).caption, "needs a snapshot")
check("and the status line has flipped", fake.find(player.gui.screen, STATUS_TEXT).caption, "Recording")

-- Once it has a baseline the button has nothing left to do, and says that
-- rather than staying pressable and doing nothing.
gui.close(1)
_G.storage.timelapse_capture = { segment_start_tick = 0, session_id = "s", baselined_surfaces = { nauvis = 1 } }
gui.open(1)
check("nothing left to snapshot", fake.find(player.gui.screen, GENERATE_BUTTON).enabled, false)
check("and the row says so", fake.find(player.gui.screen, ROW_STATE).caption, "recording")

-- Unticking a place updates the row without waiting for the next refresh,
-- which is what makes the click look like it landed.
gui.on_gui_checked_state_changed({
  player_index = 1,
  element = { valid = true, name = "check", tags = { surface = "nauvis" }, state = false },
})
check("an unticked place stops claiming to record", fake.find(player.gui.screen, ROW_STATE).caption, "not recorded")

-- Events about elements that are gone ----------------------------------------------

-- A click can arrive for an element destroyed in the same tick, which reads as
-- invalid rather than as an error.
fake.reset(1000)
gui = load_gui()
player = with_player()
check_true("a click on nothing is ignored", (function()
  return pcall(function() gui.on_gui_click({ player_index = 1, element = nil }) end)
end)())
check_true("as is one on an element that has been destroyed", (function()
  return pcall(function()
    gui.on_gui_click({ player_index = 1, element = { valid = false, name = CLOSE_BUTTON } })
  end)
end)())
check_true("and a checkbox change on one", (function()
  return pcall(function()
    gui.on_gui_checked_state_changed({ player_index = 1, element = { valid = false, name = LIVE_CAPTURE_CHECKBOX } })
  end)
end)())

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
