-- save-timelapse
-- Two independent ways to get data out of a save for timelapse rendering,
-- each its own module: one-shot export (export.lua) and live capture
-- (capture.lua), plus a periodic test-snapshot debug feature (snapshot.lua).
-- This file is just the glue that wires all three into Factorio's event
-- system: which events/timers are subscribed and when, and the single
-- on_tick dispatcher every feature's pending work runs through.
--
-- Both export.lua and capture.lua share their binary-encoding logic via
-- encode.lua, which has no Factorio dependency and is unit tested
-- standalone (tests/encode_test.lua).

local export = require("export")
local capture = require("capture")
local snapshot = require("snapshot")

-- Timers
--
-- Factorio keeps one handler per on_nth_tick interval, so a second feature
-- registering the same interval silently replaces the first rather than
-- erroring. capture.CAPTURE_FLUSH_TICKS is 240 (4 real seconds), and the
-- periodic test-snapshot setting below is also given in seconds, so a user
-- picking 4 there hits that interval by coincidence, not by doing anything
-- unusual. Anything wanting a periodic callback is therefore collected into
-- one table keyed by interval and chained, rather than each feature calling
-- `script.on_nth_tick` for itself.

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
  -- export path during real play without the freeze a synchronous export
  -- would repeat every interval, unlike the baseline, this one runs over
  -- and over, so avoiding a stall on every run is worth the extra ticks it
  -- takes to finish each one.
  local test_seconds = settings.global["save-timelapse-snapshot-seconds"].value
  if test_seconds > 0 then
    want(test_seconds * 60, function(event)
      snapshot.start(event.tick)
    end)
  end

  set_interval_handlers(by_interval)
end

--- The one on_tick subscription in the whole mod, so nothing here can
--- collide with another feature wanting on_tick the way two on_nth_tick
--- features could collide above. Each module's own `run_pending_tick_work`
--- no-ops when it has nothing to do, so this stays a flat, unconditional
--- sequence: order matches the original headless-scan / baseline / snapshot
--- priority, though nothing today can actually make more than one of these
--- pending on the same tick.
local function on_tick(event)
  export.run_pending_tick_work(event.tick)
  capture.run_pending_tick_work(event.tick)
  snapshot.run_pending_tick_work(event.tick)
end
script.on_event(defines.events.on_tick, on_tick)

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
