-- save-timelapse
-- Two independent ways to get data out of a save for timelapse rendering:
--
-- Snapshot export: writes every entity/tile on a surface, right now. Runs
-- either from the /timelapse-export command or, for unattended runs, from
-- the save-timelapse-headless-scan startup setting. Retroactive -- works on
-- saves that already exist -- but only as fine-grained as however often the
-- player saved.
--
-- Live capture: logs every construction event as it happens, for
-- frame-perfect playback. Only covers play from the moment it's turned on,
-- since Factorio keeps no placement history inside a save to recover
-- retroactively. Toggled via the save-timelapse-live-capture runtime setting.
--
-- Both share their JSON-encoding logic via encode.lua, which has no
-- Factorio dependency and is unit tested standalone (tests/encode_test.lua).

local encode = require("encode")

local EXPORT_DIR = "save-timelapse/"
local FLUSH_EVERY = 2000

--- Set at load when the CLI's startup flag is on, and acted on by the single
--- on_tick handler at the bottom of this file rather than by registering one
--- here. Factorio keeps one handler per event, so a second registration would
--- silently replace this one -- which is exactly what an incremental snapshot
--- wanting on_tick would do.
local headless_scan_pending = false

local function excluded_types()
  if settings.startup["save-timelapse-include-resources"].value then
    return encode.EXCLUDED_TYPES
  end
  local list = { "resource" }
  for _, t in pairs(encode.EXCLUDED_TYPES) do
    list[#list + 1] = t
  end
  return list
end

--- Write one surface to its own file. Returns entity and tile counts written.
local function export_surface(surface, tick)
  local path = string.format("%sframe_%d_%s.json", EXPORT_DIR, tick, surface.name)

  helpers.write_file(path, string.format('{"tick":%d,"surface":%s,"entities":[',
    tick, encode.quote(surface.name)), false)

  local pending, pending_count, written = {}, 0, 0

  for _, entity in pairs(surface.find_entities_filtered({
    type = excluded_types(),
    invert = true,
  })) do
    if entity.valid then
      pending_count = pending_count + 1
      written = written + 1
      pending[pending_count] = (written > 1 and "," or "") .. encode.encode_entity(entity)

      -- Each write_file call is a separate file append, so flushing per entity
      -- would make export time track syscalls rather than entity count.
      if pending_count >= FLUSH_EVERY then
        helpers.write_file(path, table.concat(pending), true)
        pending, pending_count = {}, 0
      end
    end
  end

  if pending_count > 0 then
    helpers.write_file(path, table.concat(pending), true)
  end

  helpers.write_file(path, string.format('],"count":%d,"tiles":[', written), true)

  pending, pending_count = {}, 0
  local tiles_written = 0

  for _, tile in pairs(surface.find_tiles_filtered({ name = encode.PLACED_FLOOR_TILES })) do
    pending_count = pending_count + 1
    tiles_written = tiles_written + 1
    pending[pending_count] = (tiles_written > 1 and "," or "") .. encode.encode_tile(tile)

    if pending_count >= FLUSH_EVERY then
      helpers.write_file(path, table.concat(pending), true)
      pending, pending_count = {}, 0
    end
  end

  if pending_count > 0 then
    helpers.write_file(path, table.concat(pending), true)
  end

  helpers.write_file(path, string.format('],"tile_count":%d}', tiles_written), true)

  return written, tiles_written
end

--- A surface is worth exporting if it is nauvis or the player built on it.
local function is_inhabited(surface)
  if surface.name == "nauvis" then
    return true
  end
  local ok, found = pcall(function()
    return surface.find_entities_filtered({ force = "player", limit = 1 })
  end)
  return ok and found ~= nil and #found > 0
end

--- Shared by the synchronous export below and the periodic test-snapshot
--- timer: both describe "everything exported at this tick" in the same shape,
--- so both write it through one function rather than two copies drifting.
local function periodic_manifest_path(tick)
  return string.format("%sframe_%d_manifest.json", EXPORT_DIR, tick)
end

--- Every surface, synchronously, in whatever tick this is called from. Used
--- for /timelapse-export, headless scan, and the once-per-save baseline --
--- three callers wanting the exact same "everything, right now" export,
--- differing only in what manifest path names the result.
local function export_all_to(tick, manifest_path)
  local names, total, tile_total = {}, 0, 0

  for _, surface in pairs(game.surfaces) do
    if is_inhabited(surface) then
      local entities, tiles = export_surface(surface, tick)
      total = total + entities
      tile_total = tile_total + tiles
      names[#names + 1] = encode.quote(surface.name)
    end
  end

  helpers.write_file(
    manifest_path,
    string.format('{"tick":%d,"entities":%d,"tiles":%d,"surfaces":[%s]}',
      tick, total, tile_total, table.concat(names, ",")),
    false)

  return total, tile_total, #names
end

local function export_all(tick)
  return export_all_to(tick, periodic_manifest_path(tick))
end

commands.add_command("timelapse-export",
  "Export this save's entities for timelapse rendering.",
  function(event)
    local total, tiles, surfaces = export_all(game.tick)
    local player = event.player_index and game.get_player(event.player_index)
    if player then
      player.print(string.format(
        "[save-timelapse] exported %d entities and %d tiles from %d surface(s) to script-output/%s",
        total, tiles, surfaces, EXPORT_DIR))
    end
  end)

-- Unattended path. The CLI enables the startup flag, loads the save under
-- --benchmark, and we export on the first tick so the run reaches its tick
-- limit and exits. Only the flag is set here; the export itself happens in
-- the shared on_tick handler at the bottom of this file, which is defined
-- after everything it calls.
if settings.startup["save-timelapse-headless-scan"].value then
  headless_scan_pending = true
end

-- ---------------------------------------------------------------------------
-- Live capture

local CAPTURE_FLUSH_EVERY = 200
local CAPTURE_FLUSH_TICKS = 600 -- ~10 real seconds, bounds data loss even when idle
--- Work items encoded per tick while the periodic test-snapshot runs, and how
--- many encoded strings accumulate before a file append. Deliberately small:
--- the point of spreading that export over ticks is that no single tick
--- stalls, and a big batch gives that back. The baseline does not use this --
--- see export_all_to -- since it runs at most once per save and a one-time
--- freeze was judged worth it there to avoid a background cost smeared across
--- several minutes of play. Separate from FLUSH_EVERY, which serves every
--- synchronous export path (`/timelapse-export`, headless scan, baseline)
--- where syscall count, not smoothness, is the cost.
local SNAPSHOT_BATCH_SIZE = 64
local SNAPSHOT_FLUSH_EVERY = 128

--- Written once, after the baseline snapshot finishes, naming the tick and
--- surfaces it covers. This is the handshake with the Rust side: it is the
--- last file written, so its presence means the baseline is complete, and it
--- says which `frame_<tick>_<surface>.json` files make up that baseline.
--- Everything after the baseline is reconstructed by replaying the event log.
local BASELINE_MANIFEST = EXPORT_DIR .. "baseline.json"

local capture_pending, capture_pending_count = {}, 0
local capture_path = nil
local capture_checked_rollover = false

local snapshot_state = nil

local excluded_type_set = nil
--- Same filter as snapshot export, so a captured event never logs something
--- a snapshot wouldn't have shown (biter deaths, tree fires, and so on).
--- Memoized: startup settings can't change during a session.
local function is_excluded_type(entity_type)
  if not excluded_type_set then
    excluded_type_set = {}
    for _, t in pairs(excluded_types()) do
      excluded_type_set[t] = true
    end
  end
  return excluded_type_set[entity_type]
end

local placed_floor_set = nil
local function is_placed_floor(tile_name)
  if not placed_floor_set then
    placed_floor_set = {}
    for _, n in pairs(encode.PLACED_FLOOR_TILES) do
      placed_floor_set[n] = true
    end
  end
  return placed_floor_set[tile_name]
end

local function capture_segment_path(start_tick)
  return string.format("%sevents_%d.jsonl", EXPORT_DIR, start_tick)
end

--- A player can load an older save than one already recorded past, which an
--- append-only log can't represent as a single timeline. Called lazily from
--- an event handler or the periodic flush below -- never from on_load
--- itself, since storage cannot be written there.
local function ensure_capture_segment()
  local state = storage.timelapse_capture
  if not state then
    state = { segment_start_tick = game.tick, last_tick = game.tick }
    storage.timelapse_capture = state
  else
    local next_start = encode.next_capture_segment(state.last_tick, game.tick, state.segment_start_tick)
    if next_start ~= state.segment_start_tick then
      state.segment_start_tick = next_start
      state.last_tick = game.tick
    end
  end
  capture_path = capture_segment_path(state.segment_start_tick)
end

local function snapshot_path(tick, surface_name)
  return string.format("%sframe_%d_%s.json", EXPORT_DIR, tick, surface_name)
end

local function snapshot_flush(state)
  if state.pending_count > 0 then
    helpers.write_file(state.path, table.concat(state.pending), true)
    state.pending, state.pending_count = {}, 0
  end
end

local function snapshot_begin_surface(state, surface)
  state.path = snapshot_path(state.tick, surface.name)
  state.surface_name = surface.name
  state.entities = surface.find_entities_filtered({
    type = excluded_types(),
    invert = true,
  })
  state.entity_index = 1
  state.written = 0
  state.tiles = nil
  state.tile_index = 1
  state.tiles_written = 0
  state.phase = "entities"
  helpers.write_file(state.path,
    string.format('{"tick":%d,"surface":%s,"entities":[', state.tick, encode.quote(surface.name)), false)
end

--- Runs when a snapshot finishes: writes its manifest. Written last, so its
--- presence is what tells a reader the snapshot is whole rather than still
--- in progress. Only the periodic test-snapshot timer goes through this path
--- now -- the baseline runs synchronously via export_all_to below -- so the
--- manifest is always the periodic shape.
local function snapshot_finish(s)
  local quoted = {}
  for i, name in pairs(s.surface_names) do
    quoted[i] = encode.quote(name)
  end

  helpers.write_file(periodic_manifest_path(s.tick), string.format(
    '{"tick":%d,"entities":%d,"tiles":%d,"surfaces":[%s]}',
    s.tick, s.total_entities, s.total_tiles, table.concat(quoted, ",")), false)
end

--- One tick's worth of export. Driven by the shared on_tick handler rather
--- than by a handler this function registers itself: Factorio keeps a single
--- handler per event, so registering one here would silently displace the
--- headless-scan export.
local function snapshot_step()
  local s = snapshot_state
  if not s then
    return
  end

  if not s.phase then
    local surface = game.surfaces[s.surface_names[s.surface_index]]
    if not surface then
      snapshot_state = nil
      return
    end
    snapshot_begin_surface(s, surface)
  end

  local surface = game.surfaces[s.surface_name]
  if not surface then
    snapshot_state = nil
    return
  end

  if s.phase == "entities" then
    local end_index = math.min(s.entity_index + SNAPSHOT_BATCH_SIZE - 1, #s.entities)
    for i = s.entity_index, end_index do
      local entity = s.entities[i]
      if entity.valid then
        s.written = s.written + 1
        s.pending_count = s.pending_count + 1
        s.pending[s.pending_count] = (s.written > 1 and "," or "") .. encode.encode_entity(entity)
        if s.pending_count >= SNAPSHOT_FLUSH_EVERY then
          snapshot_flush(s)
        end
      end
    end
    s.entity_index = end_index + 1
    if s.entity_index > #s.entities then
      snapshot_flush(s)
      helpers.write_file(s.path, string.format('],"count":%d,"tiles":[', s.written), true)
      s.phase = "tiles"
      s.tiles = surface.find_tiles_filtered({ name = encode.PLACED_FLOOR_TILES })
      s.tile_index = 1
    end
    return
  end

  if s.phase == "tiles" then
    local end_index = math.min(s.tile_index + SNAPSHOT_BATCH_SIZE - 1, #s.tiles)
    for i = s.tile_index, end_index do
      local tile = s.tiles[i]
      s.tiles_written = s.tiles_written + 1
      s.pending_count = s.pending_count + 1
      s.pending[s.pending_count] = (s.tiles_written > 1 and "," or "") .. encode.encode_tile(tile)
      if s.pending_count >= SNAPSHOT_FLUSH_EVERY then
        snapshot_flush(s)
      end
    end
    s.tile_index = end_index + 1
    if s.tile_index > #s.tiles then
      snapshot_flush(s)
      helpers.write_file(s.path, string.format('],"tile_count":%d}', s.tiles_written), true)
      s.total_entities = s.total_entities + s.written
      s.total_tiles = s.total_tiles + s.tiles_written
      s.phase = nil
      s.surface_index = s.surface_index + 1
      if s.surface_index > #s.surface_names then
        snapshot_finish(s)
        snapshot_state = nil
      end
    end
  end
end

--- Start an incremental, multi-tick export. The only caller left is the
--- periodic test-snapshot timer: the baseline used to share this (see
--- export_all_to for why it no longer does) and a stray old reference here
--- would be the kind of thing that quietly drifts back out of sync, so this
--- stays a single-purpose function rather than a generic one two callers
--- have to agree on.
local function snapshot_start(tick)
  if snapshot_state then
    return
  end

  local state = {
    tick = tick,
    surface_names = {},
    surface_index = 1,
    entities = nil,
    entity_index = 1,
    written = 0,
    tiles = nil,
    tile_index = 1,
    tiles_written = 0,
    total_entities = 0,
    total_tiles = 0,
    pending = {},
    pending_count = 0,
    path = nil,
    surface_name = nil,
    phase = nil,
  }

  for _, surface in pairs(game.surfaces) do
    if is_inhabited(surface) then
      state.surface_names[#state.surface_names + 1] = surface.name
    end
  end

  if #state.surface_names == 0 then
    return
  end

  snapshot_state = state
end

--- Take the baseline once per save, then never again: everything after it is
--- reconstructed by replaying the event log, so a second full snapshot would
--- be pure duplication -- at roughly 50 bytes per entity, a megabase snapshot
--- every 10 seconds was writing gigabytes an hour to say what the log
--- already said.
---
--- Runs synchronously in a single tick via export_all_to, unlike the
--- incremental snapshot_start/snapshot_step the periodic test-snapshot
--- setting uses. That incremental machinery exists specifically to avoid a
--- visible freeze on every run -- the right trade for something that repeats.
--- A baseline runs at most once per save, so the trade flips: a freeze
--- proportional to base size (measured on a ~375k entity base: tens of
--- seconds), once, beats a background cost smeared across the next several
--- minutes of play that a save or quit can interrupt and force to restart.
--- Factorio can only save or quit between ticks, never mid-tick, so a
--- single-tick export cannot itself be caught half-written by normal play --
--- only a killed process could, and `baseline_tick` is set below only after
--- the write succeeds, so even that just retries on next load rather than
--- trusting a partial file.
---
--- `baseline_tick` is recorded in `storage`, so it travels inside the save:
--- a save that has been baselined knows it, and a fresh one does not.
local function ensure_baseline(tick)
  ensure_capture_segment()
  local capture = storage.timelapse_capture
  if capture.baseline_tick then
    return
  end
  export_all_to(tick, BASELINE_MANIFEST)
  capture.baseline_tick = tick
end

--- The mod cannot detect that script-output/save-timelapse has been wiped
--- and retake the baseline on its own: `LuaHelpers` (checked against
--- Factorio's own runtime-api.json) exposes `write_file` and `remove_path`
--- and nothing else -- no read, no exists check, no directory listing. A
--- mod genuinely cannot tell whether a file it wrote is still there.
--- `baseline_tick` therefore has to be trusted as the source of truth for
--- "has this save already been baselined," which is correct for its actual
--- purpose (never repeat a multi-second export unnecessarily) but leaves no
--- automatic recovery if the output files are deleted out from under it.
---
--- This command is the manual equivalent of automatic detection: run it
--- after clearing script-output (or whenever a corrupted/incomplete capture
--- needs to be abandoned) and the next check retakes the baseline and starts
--- a fresh event segment, exactly as if this save had never been captured.
commands.add_command("timelapse-reset-capture",
  "Clear live-capture state so the baseline is retaken and a fresh event " ..
  "log starts. Use after deleting files from script-output/save-timelapse " ..
  "-- the mod cannot see that on its own, since Factorio gives it no way " ..
  "to read back what it already wrote.",
  function(event)
    storage.timelapse_capture = nil
    capture_checked_rollover = false
    local player = event.player_index and game.get_player(event.player_index)

    if settings.global["save-timelapse-live-capture"].value then
      ensure_baseline(game.tick)
      if player then
        player.print("[save-timelapse] capture state cleared, baseline retaken")
      end
    elseif player then
      player.print(
        "[save-timelapse] capture state cleared; enable save-timelapse-live-capture to start a new baseline")
    end
  end)

local function flush_capture()
  if capture_pending_count > 0 then
    helpers.write_file(capture_path, table.concat(capture_pending), true)
    capture_pending, capture_pending_count = {}, 0
  end
end

local function log_event(op, kind, name, x, y, direction, id, w, h, surface)
  if not capture_checked_rollover then
    ensure_capture_segment()
    capture_checked_rollover = true
  end
  storage.timelapse_capture.last_tick = game.tick

  capture_pending_count = capture_pending_count + 1
  capture_pending[capture_pending_count] =
    encode.encode_event(op, kind, game.tick, name, x, y, direction, id, w, h, surface) .. "\n"

  if capture_pending_count >= CAPTURE_FLUSH_EVERY then
    flush_capture()
  end
end

local function log_entity(op, entity)
  if not entity.valid or is_excluded_type(entity.type) then
    return
  end
  local pos = entity.position
  log_event(op, "e", op == "+" and entity.name or nil, pos.x, pos.y,
    entity.direction, entity.unit_number, entity.tile_width, entity.tile_height,
    entity.surface.name)
end

local function log_tile_change(op, event)
  -- Tile events carry a surface_index rather than the surface itself.
  local surface = game.surfaces[event.surface_index]
  local surface_name = surface and surface.name

  for _, change in pairs(event.tiles) do
    local pos = change.position
    if op == "+" then
      if is_placed_floor(event.tile.name) then
        log_event("+", "t", event.tile.name, pos.x, pos.y, nil, nil, nil, nil, surface_name)
      end
    elseif change.old_tile and is_placed_floor(change.old_tile.name) then
      log_event("-", "t", nil, pos.x, pos.y, nil, nil, nil, nil, surface_name)
    end
  end
end

local CAPTURE_HANDLERS = {
  [defines.events.on_built_entity] = function(e) log_entity("+", e.entity) end,
  [defines.events.on_robot_built_entity] = function(e) log_entity("+", e.entity) end,
  [defines.events.script_raised_built] = function(e) log_entity("+", e.entity) end,
  [defines.events.on_player_mined_entity] = function(e) log_entity("-", e.entity) end,
  [defines.events.on_robot_mined_entity] = function(e) log_entity("-", e.entity) end,
  [defines.events.on_entity_died] = function(e) log_entity("-", e.entity) end,
  [defines.events.script_raised_destroy] = function(e) log_entity("-", e.entity) end,
  [defines.events.on_player_built_tile] = function(e) log_tile_change("+", e) end,
  [defines.events.on_robot_built_tile] = function(e) log_tile_change("+", e) end,
  [defines.events.on_player_mined_tile] = function(e) log_tile_change("-", e) end,
  [defines.events.on_robot_mined_tile] = function(e) log_tile_change("-", e) end,
}

-- ---------------------------------------------------------------------------
-- Timers
--
-- Factorio keeps one handler per on_nth_tick interval, so a second feature
-- registering the same interval silently replaces the first rather than
-- erroring. CAPTURE_FLUSH_TICKS is 600 (10 real seconds), and the periodic
-- test-snapshot setting below is also given in seconds, so a user picking 10
-- there hits that interval by coincidence, not by doing anything unusual.
-- Anything wanting a periodic callback is therefore collected into one table
-- keyed by interval and chained, rather than each feature calling
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

--- Handlers are only subscribed while their setting is on -- not registered
--- but checking a flag on every call -- so there's zero hook cost when off.
local function sync_subscriptions()
  local capture_on = settings.global["save-timelapse-live-capture"].value

  for event_id, handler in pairs(CAPTURE_HANDLERS) do
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
    -- Also where an interrupted baseline gets retried: `on_load` cannot
    -- write storage or start one, so the first flush after a reload is what
    -- notices `baseline_tick` is still unset and restarts the export.
    want(CAPTURE_FLUSH_TICKS, function(event)
      capture_checked_rollover = true
      ensure_baseline(event.tick)
      flush_capture()
    end)
  end

  -- A snapshot on a timer, independent of live capture, for exercising the
  -- export path during real play without the freeze a synchronous export
  -- would repeat every interval -- unlike the baseline, this one runs over
  -- and over, so avoiding a stall on every run is worth the extra ticks it
  -- takes to finish each one.
  local test_seconds = settings.global["save-timelapse-snapshot-seconds"].value
  if test_seconds > 0 then
    want(test_seconds * 60, function(event)
      snapshot_start(event.tick)
    end)
  end

  set_interval_handlers(by_interval)
end

--- The one on_tick subscription in the whole mod, so nothing here can
--- collide with another feature wanting on_tick the way two on_nth_tick
--- features could collide above.
local function on_tick(event)
  if headless_scan_pending then
    export_all(event.tick)
    headless_scan_pending = false
  end
  if snapshot_state then
    snapshot_step()
  end
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
      capture_checked_rollover = true
      -- Turning capture on is the one moment we can baseline immediately
      -- rather than waiting up to CAPTURE_FLUSH_TICKS for the first flush.
      ensure_baseline(game.tick)
    end
  end
end)
