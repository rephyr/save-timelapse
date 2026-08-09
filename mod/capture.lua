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

--- How long to hold off the actual (synchronous, freezing) baseline export
--- after printing the warning about it. A single tick is one rendered
--- frame at best, often too brief to actually read a message in before the
--- freeze hits; this gives it real, perceptible time on screen first.
local BASELINE_WARNING_DELAY_TICKS = 120 -- ~2 real seconds

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

--- Whether this session has already reconciled its (empty, freshly re-run)
--- dictionaries with whatever the segment file on disk already contains.
---
--- A plain module local specifically because Factorio resets these on every
--- load, which is exactly the event that needs detecting. Nothing in
--- `storage` can serve here: `storage` is saved inside the save file, so it
--- rewinds along with it and can never tell "this save was just loaded" from
--- "this save has been running a while".
local capture_dictionaries_synced = false

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

--- Per-surface opt-OUT list: presence as a key means excluded from live
--- capture. A surface absent here is recorded, which is what lets a brand
--- new planet or platform, created after this table already has entries,
--- keep recording with zero special-casing, the same "capture everything
--- unless told otherwise" default this mod has always had, now per-surface.
---
--- Its own top-level storage key, sibling to storage.timelapse_capture
--- rather than nested inside it: M.reset_capture below wipes
--- storage.timelapse_capture wholesale to force a fresh baseline, and a
--- player's choice of which surfaces to record is a separate, persistent
--- preference reset should not also throw away.
local function excluded_surfaces()
  storage.timelapse_excluded_surfaces = storage.timelapse_excluded_surfaces or {}
  return storage.timelapse_excluded_surfaces
end

function M.is_surface_excluded(surface_name)
  return excluded_surfaces()[surface_name] == true
end

function M.set_surface_excluded(surface_name, excluded)
  if excluded then
    excluded_surfaces()[surface_name] = true
  else
    excluded_surfaces()[surface_name] = nil
  end
end

--- Every surface that has ever gotten its own baseline frame, keyed by
--- name, valued by the tick it happened at. The original whole-playthrough
--- baseline (`perform_baseline`'s first-ever run) records every surface it
--- covered here; a later catch-up (`M.generate_pending_baselines`) adds
--- just the one it covers. `request_baseline` computes what's still
--- missing by checking this table, which is what lets the exact same
--- function serve the first-ever baseline, a reset, and any later
--- catch-up with no special-casing between them.
---
--- Nested inside `storage.timelapse_capture`, sibling to `baseline_tick`,
--- NOT alongside `storage.timelapse_excluded_surfaces`: unlike a player's
--- exclusion choice (a persistent preference `M.reset_capture` should not
--- discard), this is capture progress, and `M.reset_capture` wiping
--- `storage.timelapse_capture` wholesale is supposed to throw it away so
--- everything gets a fresh baseline, exactly like `baseline_tick` already
--- does today. Assumes `storage.timelapse_capture` already exists: every
--- caller (`request_baseline`/`perform_baseline`) only ever runs after
--- `ensure_capture_segment` has already guaranteed that.
local function baselined_surfaces()
  local capture = storage.timelapse_capture
  capture.baselined_surfaces = capture.baselined_surfaces or {}
  return capture.baselined_surfaces
end

--- See baseline_manifest_path above for why a nil session_id (a save whose
--- capture state predates this feature) falls back to the untagged name
--- instead of erroring.
local function capture_segment_path(session_id, start_tick)
  if not session_id then
    return export.EXPORT_DIR .. encode.capture_segment_basename(start_tick)
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
---
--- Note that `state.last_tick` tracks the last tick an event was actually
--- logged at, not the last tick played, which is what makes the rollback
--- check below neither trigger-happy nor blind. Reloading past a stretch
--- where nothing was built discards no recorded history, so it correctly
--- reports no rollback and keeps one segment; reloading past anything that
--- was recorded correctly reports one.
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
  end

  capture_path = capture_segment_path(state.session_id, state.segment_start_tick)

  if not state.segment_initialized then
    capture_names = encode.new_dictionary()
    capture_surfaces = encode.new_dictionary()
    capture_last_written_tick = nil
    export.safe_write_file(capture_path, encode.event_header(), false)
    state.segment_initialized = true
  elseif not capture_dictionaries_synced then
    -- Resuming a segment this save was already writing before it was loaded.
    -- The module locals above were reset to empty by Factorio re-running
    -- this file, while the segment on disk still holds every name defined
    -- before the load, so the two sides now disagree about what id 0 means.
    -- The record says so explicitly; see encode.event_reset_dictionaries for
    -- why this cannot instead be solved by persisting the dictionary or by
    -- starting a fresh segment.
    export.safe_write_file(capture_path, encode.event_reset_dictionaries(), true)
  end

  -- Set after both branches, so the very first call of a session takes one
  -- of them and every later call in the same session takes neither.
  capture_dictionaries_synced = true
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
--- the write succeeds, so a save from mid-export never trusts a partial
--- file as done. It also does not silently retry itself (see
--- `M.periodic_flush`): the player has to explicitly re-trigger a baseline,
--- same as if one had never been requested.
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
--- ending. Queuing the actual export for `BASELINE_WARNING_DELAY_TICKS`
--- later, not just the next tick, gives Factorio real, perceptible time to
--- render the warning before the freeze hits, not just one frame that may
--- flash by unread.
local baseline_pending_tick = nil

--- Computes which currently-included, inhabited surfaces have never gotten
--- a baseline of their own, sorted by name for stable, readable output.
--- Shared by `request_baseline` (to warn about and size the coming
--- export) and `perform_baseline` (to actually run it), rather than either
--- trusting the other's answer: exclusion can change during
--- `BASELINE_WARNING_DELAY_TICKS`'s gap between the two, so re-scanning is
--- what keeps the export matching reality at the moment it actually runs.
local function surfaces_needing_baseline()
  local baselined = baselined_surfaces()
  local names = {}
  for _, surface in pairs(game.surfaces) do
    if export.is_inhabited(surface) and not M.is_surface_excluded(surface.name) and not baselined[surface.name] then
      names[#names + 1] = surface.name
    end
  end
  table.sort(names)
  return names
end

--- Warns the player a freeze is coming, with a real entity count alongside
--- the headline message: `count_entities_filtered` gives that without
--- paying to materialise the array `export_surface` needs. There is no way
--- to also promise a number of seconds, though: Factorio's Lua sandbox has
--- no wall clock (ticks are logical, not real time, kept deterministic for
--- multiplayer, and the export that follows runs inside a single one of
--- them anyway), so this can only size the job, not time it.
---
--- Generalized to cover three callers identically: the first-ever baseline
--- (`M.on_capture_enabled`), a reset (`M.reset_capture`), and the panel's
--- Generate button for whatever surfaces were checked since the last one
--- (`M.generate_pending_baselines`), since all three just mean "some
--- currently-wanted surface has never been baselined," which
--- `surfaces_needing_baseline` answers the same way regardless of why it's
--- being asked. Checking several surfaces before pressing Generate
--- coalesces into one warning/freeze instead of one per surface, since
--- `perform_baseline` re-scans everything eligible when it actually runs.
local function request_baseline(tick)
  ensure_capture_segment()
  if baseline_pending_tick then
    return
  end

  local names = surfaces_needing_baseline()
  if #names == 0 then
    return
  end

  local total = 0
  for _, name in ipairs(names) do
    total = total + game.surfaces[name].count_entities_filtered({ type = export.excluded_types(), invert = true })
  end

  game.print(string.format(
    "[save-timelapse] Loading baseline for %s, this might take a while " ..
    "(%d entities, the game will be unresponsive until it finishes)",
    table.concat(names, ", "), total))

  baseline_pending_tick = tick + BASELINE_WARNING_DELAY_TICKS
end

--- The other half of `request_baseline`, run from `M.run_pending_tick_work`
--- once `BASELINE_WARNING_DELAY_TICKS` have passed since the warning, so
--- that message gets real time on screen before the freeze it describes.
---
--- The very first baseline this save ever takes (`capture.baseline_tick`
--- still nil) writes `baseline.json` via `export.export_all_to`, exactly
--- as before. Every later call is provably a catch-up (baseline_tick is
--- already set), so it exports only the still-missing surfaces through
--- `export.export_surfaces_to`, which writes no manifest at all; see its
--- own doc comment for why `baseline.json` must never change after it's
--- first written.
local function perform_baseline(tick)
  baseline_pending_tick = nil
  local capture = storage.timelapse_capture

  local names = surfaces_needing_baseline()
  if #names == 0 then
    return
  end

  local total, tiles, count
  if not capture.baseline_tick then
    total, tiles, count =
      export.export_all_to(tick, baseline_manifest_path(capture.session_id), capture.session_id, M.is_surface_excluded)
    capture.baseline_tick = tick
    for _, name in ipairs(names) do
      baselined_surfaces()[name] = tick
    end
  else
    local exported
    total, tiles, exported = export.export_surfaces_to(tick, capture.session_id, names)
    count = #exported
    for _, name in ipairs(exported) do
      baselined_surfaces()[name] = tick
    end
  end

  game.print(string.format(
    "[save-timelapse] baseline export finished: %d entities and %d tiles across %d surface(s)",
    total, tiles, count))
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
--- This command (and the GUI's own reset button, see gui.lua) is what
--- recovers from that: unlike detecting an external deletion, actually
--- deleting this playthrough's own files is something `remove_path` can do,
--- since the mod already knows exactly what it wrote and where (every
--- playthrough gets its own session-tagged subfolder, see
--- encode.session_dir). So reset does the real thing itself instead of
--- assuming the player already cleared script-output by hand.
function M.reset_capture(player)
  local old_session_id = storage.timelapse_capture and storage.timelapse_capture.session_id
  if old_session_id then
    -- remove_path's failure behavior isn't documented, so this is pcall'd
    -- the same defensive way every other capture write in this mod is: a
    -- failure just degrades to the old behavior (stale files linger
    -- alongside the new ones), not a new failure mode.
    pcall(helpers.remove_path, export.EXPORT_DIR .. encode.session_dir(old_session_id))
  end

  storage.timelapse_capture = nil
  capture_checked_rollover = false

  if settings.global["save-timelapse-live-capture"].value then
    if player then
      player.print("[save-timelapse] cleared this playthrough's capture files, retaking the baseline")
    end
    request_baseline(game.tick)
  elseif player then
    player.print(
      "[save-timelapse] capture files cleared; enable save-timelapse-live-capture to start a new baseline")
  end
end

commands.add_command("timelapse-reset-capture",
  "Clear this playthrough's live-capture files and state so the baseline " ..
  "is retaken and a fresh event log starts.",
  function(event)
    M.reset_capture(event.player_index and game.get_player(event.player_index))
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
  -- A nil surface (log_tile_change's surface_name can be nil for an
  -- unresolvable surface_index) is never excluded: a nil table key read is
  -- legal and never equals true, so this is a no-op for that case, same
  -- as today.
  if M.is_surface_excluded(surface) then
    return
  end

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
--- timer multiplexer while live capture is on. Calls `ensure_capture_segment`
--- directly rather than `request_baseline`: a save that starts with live
--- capture already on from its very first tick (rather than it being
--- switched on mid-session) never runs `M.on_capture_enabled`, and with
--- nothing built or mined yet, `log_event`'s own lazy init hasn't run
--- either, so this can be the first thing to ever touch
--- `storage.timelapse_capture`, which still needs to exist for
--- `capture_checked_rollover = true` below to be a true statement, and for
--- `sample_connected_players` to read a real `session_id`. Deliberately
--- does NOT call `request_baseline` itself, though: a baseline is only
--- ever taken in direct response to an explicit player action (turning
--- live capture on, or the panel's reset button, both via
--- `M.on_capture_enabled`/`M.reset_capture`), never on an unattended
--- timer. An interrupted baseline (the process killed mid-export, before
--- `baseline_tick` got set) therefore does not silently retry itself on
--- the next flush after a reload; the player has to explicitly re-trigger
--- it, the same as if it had never started.
function M.periodic_flush(tick)
  ensure_capture_segment()
  capture_checked_rollover = true
  flush_capture()
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

--- Run when the panel's "Generate" button is clicked. Checking a surface's
--- box only records that it's now wanted (gui.lua); its pre-existing
--- state, built up while excluded or before capture ever started, was
--- never snapshotted, and its events only start logging from the moment
--- it's checked, so something still has to actually take the catch-up
--- baseline for whatever's newly included. Generate is that explicit,
--- separate step, deliberately not run automatically the instant a box is
--- checked, so checking several surfaces in a row batches into one warning
--- and one freeze instead of one per box, and ticking a box doesn't force
--- an immediate decision to pay that cost.
---
--- A no-op while live capture is off: nothing is being logged for this
--- save yet, and `M.on_capture_enabled`'s own `request_baseline` call will
--- see the exact same gap and cover it whenever/if the player turns
--- capture on.
function M.generate_pending_baselines(tick)
  if settings.global["save-timelapse-live-capture"].value then
    capture_checked_rollover = true
    request_baseline(tick)
  end
end

--- Runs the baseline export once its warning has had its delay to be read;
--- no-ops otherwise. Called unconditionally from control.lua's on_tick,
--- alongside export.lua's and snapshot.lua's own pending-work checks.
function M.run_pending_tick_work(tick)
  if baseline_pending_tick and tick >= baseline_pending_tick then
    perform_baseline(tick)
  end
end

return M
