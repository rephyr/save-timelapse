-- save-timelapse: glue. Wires one-shot export (export.lua), live capture
-- (capture.lua), the periodic test snapshot (snapshot.lua) and the control
-- panel (gui.lua) into Factorio's event system, and owns the single on_tick
-- dispatcher every feature's pending work runs through.

local export = require("export")
local capture = require("capture")
local snapshot = require("snapshot")
local gui = require("gui")
local milestones = require("milestones")

-- Timers
--
-- Factorio keeps one handler per on_nth_tick interval, so a second feature
-- registering the same interval silently replaces the first. CAPTURE_FLUSH_TICKS
-- is 600 and the test-snapshot setting is given in seconds, so a value of 10
-- collides by coincidence. Everything wanting a periodic callback is therefore
-- collected by interval and chained.

--- Intervals currently subscribed, so a resync can release the ones no
--- longer wanted. Reset by on_load along with the rest of Lua state, which
--- is correct: a fresh state holds no subscriptions to release.
local active_intervals = {}

local function set_interval_handlers(by_interval)
  for interval in pairs(active_intervals) do
    if not by_interval[interval] then
      script.on_nth_tick(interval, nil)
    end
  end

  active_intervals = {}
  for interval, handler in pairs(by_interval) do
    script.on_nth_tick(interval, handler)
    active_intervals[interval] = true
  end
end

--- Handlers are only subscribed while their setting is on, not registered
--- but checking a flag on every call, so there's zero hook cost when off.
local function sync_subscriptions()
  local capture_on = settings.global["save-timelapse-live-capture"].value

  for event_id, handler in pairs(capture.CAPTURE_HANDLERS) do
    script.on_event(event_id, capture_on and handler or nil)
  end

  -- Milestones ride on the same setting as capture: they are markers on a
  -- capture's timeline, so there is nothing for them to annotate when one is
  -- not being recorded. Subscribed only while it is on, like the handlers
  -- above, so there is no hook cost when off.
  script.on_event(
    defines.events.on_rocket_launched,
    capture_on and function(event)
      local state = storage.timelapse_capture
      milestones.on_rocket_launched(event, state and state.session_id)
    end or nil
  )

  local by_interval = {}
  local function want(interval, handler)
    local existing = by_interval[interval]
    by_interval[interval] = existing
      and function(event) existing(event); handler(event) end
      or handler
  end

  if capture_on then
    want(capture.CAPTURE_FLUSH_TICKS, function(event)
      capture.periodic_flush(event.tick)
    end)
  end

  -- A snapshot on a timer, independent of live capture, for exercising the
  -- export path during play. Incremental rather than synchronous because
  -- unlike the baseline this repeats, so a stall on every run would not do.
  local test_seconds = settings.global["save-timelapse-snapshot-seconds"].value
  if test_seconds > 0 then
    want(test_seconds * 60, function(event)
      snapshot.start(event.tick)
    end)
  end

  set_interval_handlers(by_interval)
end

--- The one on_tick subscription in the mod, so nothing can collide the way
--- two on_nth_tick features could. Each module's `run_pending_tick_work`
--- no-ops when idle, keeping this a flat unconditional sequence.
local function on_tick(event)
  export.run_pending_tick_work(event.tick, capture.compute_session_id)
  capture.run_pending_tick_work(event.tick)
  snapshot.run_pending_tick_work(event.tick)
end
script.on_event(defines.events.on_tick, on_tick)

-- GUI
--
-- Registered unconditionally, unlike capture's handlers: the panel has to
-- work in order to turn live capture on in the first place, so gating it
-- behind the capture setting would be actively wrong.
script.on_event(defines.events.on_lua_shortcut, function(event)
  if event.prototype_name == "save-timelapse-panel" then
    gui.toggle(event.player_index)
  end
end)
script.on_event("save-timelapse-toggle-panel", function(event)
  gui.toggle(event.player_index)
end)
script.on_event(defines.events.on_gui_click, gui.on_gui_click)
script.on_event(defines.events.on_gui_checked_state_changed, gui.on_gui_checked_state_changed)
script.on_event(defines.events.on_gui_closed, gui.on_gui_closed)

script.on_init(sync_subscriptions)
script.on_load(sync_subscriptions)
script.on_event(defines.events.on_runtime_mod_setting_changed, function(event)
  if event.setting == "save-timelapse-live-capture"
    or event.setting == "save-timelapse-snapshot-seconds" then
    sync_subscriptions()
    if event.setting == "save-timelapse-live-capture"
      and settings.global["save-timelapse-live-capture"].value then
      capture.on_capture_enabled(game.tick)
    end
  end
end)
