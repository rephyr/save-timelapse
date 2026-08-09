-- save-timelapse: live capture. Logs every construction event as it
-- happens, for frame-perfect playback. Only covers play from the moment
-- it's turned on, since Factorio keeps no placement history inside a save
-- to recover retroactively. Toggled via the save-timelapse-live-capture
-- runtime setting.

local encode = require("encode")
local export = require("export")

local M = {}

local CAPTURE_FLUSH_EVERY = 200
M.CAPTURE_FLUSH_TICKS = 240 -- ~4 real seconds, bounds data loss even when idle

--- Written once, after the baseline snapshot finishes, naming the tick and
--- surfaces it covers. This is the handshake with the Rust side: it is the
--- last file written, so its presence means the baseline is complete, and it
--- says which `frame_<tick>_<surface>.stfr` files, alongside it in the same
--- session folder, make up that baseline. Everything after the baseline is
--- reconstructed by replaying the event log. Tagged by session_id (see
--- compute_session_id below) so each playthrough gets its own folder instead
--- of overwriting another one's.
---
--- A save whose capture state predates session_id existing has it as `nil`
--- (see ensure_capture_segment below, which only ever sets it while creating
--- fresh state): this keeps such a save on the untagged, folder-less name it
--- was already using rather than erroring, until /timelapse-reset-capture
--- clears its state and lets it start over with a real one.
local function baseline_manifest_path(session_id)
  if not session_id then
    return export.EXPORT_DIR .. "baseline.json"
  end
  return export.EXPORT_DIR .. encode.baseline_manifest_name(session_id)
end

local capture_pending, capture_pending_count = {}, 0
local capture_path = nil
local capture_checked_rollover = false

--- Name and surface dictionaries for the event log currently being written.
--- Plain module locals rather than anything persisted in `storage`: Factorio
--- re-runs this whole file's top level on every load, which resets these to
--- fresh and empty exactly when they need to be. A brand new segment needs
--- that (nothing has been defined in it yet); a segment being *continued*
--- after a reload also gets it, even though the physical file already has
--- earlier DefineName/DefineSurface records in it from before the reload,
--- because this session has no way to read those back (see
--- `ensure_capture_segment`'s doc comment on why that's harmless rather than
--- a corruption bug).
local capture_names = encode.new_dictionary()
local capture_surfaces = encode.new_dictionary()
--- The tick a SetTick record was last written for, so `log_event` only emits
--- one when the tick actually changes rather than once per event. Reset
--- alongside the dictionaries above.
local capture_last_written_tick = nil

local excluded_type_set = nil
--- Same filter as snapshot export, so a captured event never logs something
--- a snapshot wouldn't have shown (biter deaths, tree fires, and so on).
--- Memoized: startup settings can't change during a session.
local function is_excluded_type(entity_type)
  if not excluded_type_set then
    excluded_type_set = {}
    for _, t in pairs(export.excluded_types()) do
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

--- See baseline_manifest_path above for why a nil session_id (a save whose
--- capture state predates this feature) falls back to the untagged name
--- instead of erroring.
local function capture_segment_path(session_id, start_tick)
  if not session_id then
    return string.format("%sevents_%d.stev", export.EXPORT_DIR, start_tick)
  end
  return export.EXPORT_DIR .. encode.capture_segment_name(session_id, start_tick)
end

--- A playthrough's identity, for tagging files written into the shared,
--- persistent script-output folder (see encode.baseline_manifest_name's
--- comment). The map's terrain seed is deterministic across save/reload of
--- one playthrough, differs across different ones with overwhelming
--- probability, and needs no new in-game UI to collect, unlike a save name,
--- which mods have no API access to at all. Wrapped in pcall and falling
--- back to 0 the same defensive way is_inhabited does: that only degrades
--- to today's single shared bucket in the unlikely event nauvis or its
--- map_gen_settings are unavailable, never a crash.
local function compute_session_id()
  local ok, seed = pcall(function() return game.surfaces["nauvis"].map_gen_settings.seed end)
  if ok and seed then
    return seed
  end
  return 0
end

--- A player can load an older save than one already recorded past, which an
--- append-only log can't represent as a single timeline. Called lazily from
--- an event handler or the periodic flush below, never from on_load
--- itself, since storage cannot be written there.
---
--- Also where a fresh segment's magic header gets written, and where this
--- session's event dictionaries and tick tracking reset: both only need to
--- happen when `capture_path` points at a file nothing has confirmed
--- initializing yet, tracked by the persisted `state.segment_initialized`
--- flag rather than inferred from whether `segment_start_tick` changed.
--- Those aren't the same question: a save whose `storage.timelapse_capture`
--- was written by a mod version that named segments differently (this
--- actually happened going from the JSON format to this one, mid save, and
--- is exactly the kind of thing a future format change could repeat) would
--- keep the same `segment_start_tick` while `capture_segment_path` now
--- points at a file that has never been created, and inferring from the
--- tick alone would silently skip the magic header and start appending
--- records into a file whose first bytes were never written. A save that
--- predates `segment_initialized` existing has it as `nil`, which is
--- correctly falsy here, so upgrading mid save self-heals on the very next
--- check rather than producing a header-less file a reader can't recognize.
local function ensure_capture_segment()
  local state = storage.timelapse_capture

  if not state then
    state = {
      segment_start_tick = game.tick,
      last_tick = game.tick,
      segment_initialized = false,
      session_id = compute_session_id(),
    }
    storage.timelapse_capture = state
  else
    local next_start = encode.next_capture_segment(state.last_tick, game.tick, state.segment_start_tick)
    if next_start ~= state.segment_start_tick then
      state.segment_start_tick = next_start
      state.last_tick = game.tick
      state.segment_initialized = false
    end
  end

  capture_path = capture_segment_path(state.session_id, state.segment_start_tick)

  if not state.segment_initialized then
    capture_names = encode.new_dictionary()
    capture_surfaces = encode.new_dictionary()
    capture_last_written_tick = nil
    export.safe_write_file(capture_path, encode.event_header(), false)
    state.segment_initialized = true
  end
end

--- Take the baseline once per save, then never again: everything after it is
--- reconstructed by replaying the event log, so a second full snapshot would
--- be pure duplication, at roughly 50 bytes per entity, a megabase snapshot
--- every 10 seconds was writing gigabytes an hour to say what the log
--- already said.
---
--- Runs synchronously in a single tick via export.export_all_to, unlike the
--- incremental snapshot.lua machinery the periodic test-snapshot setting
--- uses. That incremental machinery exists specifically to avoid a visible
--- freeze on every run, the right trade for something that repeats. A
--- baseline runs at most once per save, so the trade flips: a freeze
--- proportional to base size (measured on a ~375k entity base: tens of
--- seconds), once, beats a background cost smeared across the next several
--- minutes of play that a save or quit can interrupt and force to restart.
--- Factorio can only save or quit between ticks, never mid-tick, so a
--- single-tick export cannot itself be caught half-written by normal play,
--- only a killed process could, and `baseline_tick` is set below only after
--- the write succeeds, so even that just retries on next load rather than
--- trusting a partial file.
---
--- `baseline_tick` is recorded in `storage`, so it travels inside the save:
--- a save that has been baselined knows it, and a fresh one does not.
---
--- Split into a request/perform pair, `request_baseline`/`perform_baseline`,
--- rather than one function that just does the export: nothing renders
--- between two calls made within the same tick, so a warning printed right
--- before the export in the same handler would only reach the screen at the
--- same moment as the freeze itself ends, alongside the "finished" message,
--- telling the player nothing they didn't already know from the freeze
--- ending. Queuing the actual export for the *next* tick gives Factorio one
--- rendered frame to show the warning on first.
local baseline_pending = false

--- Warns the player a freeze is coming, with a real entity count rather
--- than a vague "this might take a while": `count_entities_filtered` gives
--- that without paying to materialise the array `export_surface` needs.
--- There is no way to also promise a number of seconds, though: Factorio's
--- Lua sandbox has no wall clock (ticks are logical, not real time, kept
--- deterministic for multiplayer, and the export that follows runs inside a
--- single one of them anyway), so this can only size the job, not time it.
local function request_baseline(tick)
  ensure_capture_segment()
  local capture = storage.timelapse_capture
  if capture.baseline_tick or baseline_pending then
    return
  end

  local total = 0
  for _, surface in pairs(game.surfaces) do
    if export.is_inhabited(surface) then
      total = total + surface.count_entities_filtered({ type = export.excluded_types(), invert = true })
    end
  end

  game.print(string.format(
    "[save-timelapse] exporting a %d entity baseline starting next tick, the game will be " ..
    "unresponsive until it finishes (measured at tens of seconds for a ~375k entity base)",
    total))

  baseline_pending = true
end

--- The other half of `request_baseline`, run from `M.run_pending_tick_work`
--- one tick after the warning so that message gets a chance to actually
--- render first.
local function perform_baseline(tick)
  baseline_pending = false
  local capture = storage.timelapse_capture
  if capture.baseline_tick then
    return
  end

  local total, tiles, surfaces =
    export.export_all_to(tick, baseline_manifest_path(capture.session_id), capture.session_id)
  capture.baseline_tick = tick

  game.print(string.format(
    "[save-timelapse] baseline export finished: %d entities and %d tiles across %d surface(s)",
    total, tiles, surfaces))
end

--- The mod cannot detect that script-output/save-timelapse has been wiped
--- and retake the baseline on its own: `LuaHelpers` (checked against
--- Factorio's own runtime-api.json) exposes `write_file` and `remove_path`
--- and nothing else, no read, no exists check, no directory listing. A
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
  "log starts. Use after deleting files from script-output/save-timelapse, " ..
  "the mod cannot see that on its own, since Factorio gives it no way " ..
  "to read back what it already wrote.",
  function(event)
    storage.timelapse_capture = nil
    capture_checked_rollover = false
    local player = event.player_index and game.get_player(event.player_index)

    if settings.global["save-timelapse-live-capture"].value then
      if player then
        player.print("[save-timelapse] capture state cleared, retaking the baseline")
      end
      request_baseline(game.tick)
    elseif player then
      player.print(
        "[save-timelapse] capture state cleared; enable save-timelapse-live-capture to start a new baseline")
    end
  end)

local function flush_capture()
  if capture_pending_count > 0 then
    export.safe_write_file(capture_path, table.concat(capture_pending), true)
    capture_pending, capture_pending_count = {}, 0
  end
end

--- Picks the right encode.lua function for this event's op/kind. Split out
--- from log_event below so that function reads as "manage the pending
--- buffer and tick bookkeeping" without also being a four-way dispatch.
local function encode_capture_event(op, kind, name, x, y, direction, id, w, h, surface)
  if kind == "e" then
    if op == "+" then
      return encode.event_add_entity(capture_names, capture_surfaces, surface, name, x, y, direction, id, w, h)
    end
    return encode.event_remove_entity(capture_surfaces, surface, x, y, id)
  end
  if op == "+" then
    return encode.event_add_tile(capture_names, capture_surfaces, surface, name, x, y)
  end
  return encode.event_remove_tile(capture_surfaces, surface, x, y)
end

local function log_event(op, kind, name, x, y, direction, id, w, h, surface)
  if not capture_checked_rollover then
    ensure_capture_segment()
    capture_checked_rollover = true
  end
  storage.timelapse_capture.last_tick = game.tick

  -- Emitted once per distinct tick that has at least one event, rather than
  -- on every record: many events (a blueprint landing hundreds of entities)
  -- usually share a tick.
  if capture_last_written_tick ~= game.tick then
    capture_pending_count = capture_pending_count + 1
    capture_pending[capture_pending_count] = encode.event_set_tick(game.tick)
    capture_last_written_tick = game.tick
  end

  capture_pending_count = capture_pending_count + 1
  capture_pending[capture_pending_count] = encode_capture_event(op, kind, name, x, y, direction, id, w, h, surface)

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

M.CAPTURE_HANDLERS = {
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

--- The CAPTURE_FLUSH_TICKS periodic callback body, run from control.lua's
--- timer multiplexer while live capture is on. Also where an interrupted
--- baseline gets retried: `on_load` cannot write storage or start one, so
--- the first flush after a reload is what notices `baseline_tick` is still
--- unset and restarts the export.
function M.periodic_flush(tick)
  capture_checked_rollover = true
  request_baseline(tick)
  flush_capture()
  -- After request_baseline, which guarantees storage.timelapse_capture
  -- exists (see ensure_capture_segment): session_id may still be nil for a
  -- save whose capture state predates it, and player_log_path already
  -- falls back to the untagged name for that case.
  export.sample_connected_players(tick, storage.timelapse_capture.session_id)
end

--- Run when the save-timelapse-live-capture setting is turned on: baselines
--- immediately rather than waiting up to CAPTURE_FLUSH_TICKS for the first
--- flush. Setting `capture_checked_rollover` here (not `M.periodic_flush`'s
--- job, since this doesn't also flush or sample players) means the very
--- next `log_event` doesn't redundantly call `ensure_capture_segment` again
--- right after `request_baseline` already did.
function M.on_capture_enabled(tick)
  capture_checked_rollover = true
  request_baseline(tick)
end

--- Runs the baseline export if one is pending; no-ops otherwise. Called
--- unconditionally from control.lua's on_tick, alongside export.lua's and
--- snapshot.lua's own pending-work checks.
function M.run_pending_tick_work(tick)
  if baseline_pending then
    perform_baseline(tick)
  end
end

return M
