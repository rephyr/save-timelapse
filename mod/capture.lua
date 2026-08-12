-- save-timelapse: live capture. Logs every construction event as it happens.
-- Only covers play from the moment it is turned on, Factorio keeping no
-- placement history inside a save to recover retroactively.

local encode = require("encode")
local export = require("export")
local milestones = require("milestones")

local M = {}

local CAPTURE_FLUSH_EVERY = 200
--- The floor on how often a flush happens, not the rate: a busy factory
--- flushes on volume at every CAPTURE_FLUSH_EVERY pending events. This bounds
--- how much a crash can lose while idle, and how coarse the two samplers
--- riding on it are.
M.CAPTURE_FLUSH_TICKS = 600

--- How long to hold off the synchronous, freezing baseline export after
--- warning about it, so the warning gets perceptible time on screen.
local BASELINE_WARNING_DELAY_TICKS = 120 -- ~2 real seconds

--- Written last, so its presence means the baseline is complete. Names the
--- tick and surfaces it covers, which is how the Rust side knows which frame
--- files make it up. Tagged by session_id so playthroughs do not overwrite
--- each other; a save predating session_id keeps the untagged name until
--- /timelapse-reset-capture.
local function baseline_manifest_path(session_id)
  if not session_id then
    return export.EXPORT_DIR .. "baseline.json"
  end
  return export.EXPORT_DIR .. encode.baseline_manifest_name(session_id)
end

local capture_pending, capture_pending_count = {}, 0
local capture_path = nil
local capture_checked_rollover = false

--- Name and surface dictionaries for the segment currently being written.
--- Module locals rather than `storage`, because Factorio re-runs this file on
--- every load and resets them, which is what `ensure_capture_segment` relies
--- on to know a reload happened.
local capture_names = encode.new_dictionary()
local capture_surfaces = encode.new_dictionary()
--- The tick a SetTick record was last written for, so `log_event` only emits
--- one when the tick actually changes rather than once per event. Reset
--- alongside the dictionaries above.
local capture_last_written_tick = nil

--- Whether this session has reconciled its freshly reset dictionaries with
--- what the segment file on disk already holds. A module local because a load
--- resetting it is exactly the event that needs detecting; `storage` rewinds
--- with the save and cannot tell a fresh load from a long-running session.
local capture_dictionaries_synced = false

--- What was loaded, as one string, so the prototype description is rewritten
--- when the answer could have changed and never otherwise. `script.active_mods`
--- decides it, prototypes being fixed at load time. This mod is in it too,
--- which is what makes a capture heal itself when a version that wrote the file
--- wrongly is replaced.
local loaded_mods_stamp = nil
local function loaded_mods()
  if not loaded_mods_stamp then
    local parts = {}
    for name, version in pairs(script.active_mods) do
      parts[#parts + 1] = name .. " " .. version
    end
    table.sort(parts)
    loaded_mods_stamp = table.concat(parts, ",")
  end
  return loaded_mods_stamp
end

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
    for _, n in pairs(encode.placed_floor_tiles()) do
      placed_floor_set[n] = true
    end
  end
  return placed_floor_set[tile_name]
end

--- Per-surface opt-OUT: presence means excluded, so a planet created later
--- needs no special casing. Its own storage key rather than nested in
--- `storage.timelapse_capture`, which `M.reset_capture` wipes: which surfaces
--- to record is a preference a reset should not throw away.
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

--- Every surface that has had a baseline frame, keyed by name, valued by tick.
--- `request_baseline` diffs this against what is wanted, which lets one
--- function serve the first baseline, a reset, and any catch-up. Nested inside
--- `storage.timelapse_capture`, this being capture progress a reset throws
--- away.
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

--- A playthrough's identity, for tagging files in the shared script-output
--- folder. The map's terrain seed is stable across save/reload and differs
--- across playthroughs, and needs no in-game UI to collect, unlike a save name
--- which mods cannot read. Falls back to 0 rather than crashing.
function M.compute_session_id()
  local ok, seed = pcall(function() return game.surfaces["nauvis"].map_gen_settings.seed end)
  if ok and seed then
    return seed
  end
  return 0
end

--- A player can load an older save than one already recorded past, which an
--- append-only log cannot represent as one timeline. Called lazily from an
--- event handler or the flush, never from on_load, where storage is read only.
---
--- Also where a fresh segment's magic header is written, tracked by the
--- persisted `segment_initialized` flag rather than inferred from
--- `segment_start_tick`: a mod version naming segments differently would keep
--- the same tick while pointing at a file that never existed.
---
--- `state.last_tick` is the last tick an event was logged at, not the last
--- played, which keeps the rollback check neither trigger-happy nor blind.
local function ensure_capture_segment()
  local state = storage.timelapse_capture

  if not state then
    state = {
      segment_start_tick = game.tick,
      last_tick = game.tick,
      segment_initialized = false,
      session_id = M.compute_session_id(),
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
    -- Resuming a segment this save was already writing. The module locals
    -- above were reset by the load while the file still holds every name
    -- defined before it, so the two sides disagree about what id 0 means.
    -- See encode.event_reset_dictionaries.
    export.safe_write_file(capture_path, encode.event_reset_dictionaries(), true)
  end

  -- Set after both branches, so the very first call of a session takes one
  -- of them and every later call in the same session takes neither.
  capture_dictionaries_synced = true
end

--- Take the baseline once per save, then never again: everything after is
--- reconstructed from the event log, and at roughly 50 bytes per entity a
--- megabase snapshot every 10 seconds wrote gigabytes an hour.
---
--- Runs synchronously in one tick, unlike snapshot.lua's incremental path,
--- which exists to avoid a freeze on something that repeats. A baseline runs at
--- most once per save, so tens of seconds once beats a background cost a save
--- or quit can interrupt. `baseline_tick` is set only after the write succeeds.
---
--- Split into `request_baseline`/`perform_baseline` because nothing renders
--- between two calls in one tick, so a warning printed just before the export
--- would reach the screen as the freeze ended.
local baseline_pending_tick = nil

--- Which currently-included, inhabited surfaces have never had a baseline,
--- sorted by name. Shared by `request_baseline` and `perform_baseline` rather
--- than either trusting the other: exclusion can change during the delay
--- between them, so re-scanning keeps the export matching reality.
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

--- Warns that a freeze is coming, with a real entity count:
--- `count_entities_filtered` gives one without materialising the array
--- `export_surface` needs. It cannot promise seconds, the Lua sandbox having no
--- wall clock. Covers all three callers, so checking several boxes before
--- pressing Generate coalesces into one freeze.
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

--- The other half of `request_baseline`, run once the warning delay has
--- passed. The first baseline a save ever takes writes `baseline.json` via
--- `export.export_all_to`; every later call is provably a catch-up and goes
--- through `export.export_surfaces_to`, which writes no manifest at all.
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
    -- export_all_to describes the prototypes itself, so record what it
    -- described and the next flush has nothing to do. First baseline only:
    -- a catch-up writes no description of its own.
    capture.prototypes_stamp = loaded_mods()
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

--- The mod cannot detect that script-output has been wiped and retake the
--- baseline: `LuaHelpers` exposes `write_file` and `remove_path` and nothing
--- else, so `baseline_tick` has to be trusted for "already baselined". This
--- command and the GUI's reset button are the recovery, deleting this
--- playthrough's own files being something `remove_path` can do.
function M.reset_capture(player)
  local old_session_id = storage.timelapse_capture and storage.timelapse_capture.session_id
  if old_session_id then
    -- remove_path's failure behaviour is undocumented, so this is pcall'd
    -- like every other capture write: a failure leaves stale files beside
    -- the new ones rather than becoming a new failure mode.
    pcall(helpers.remove_path, export.EXPORT_DIR .. encode.session_dir(old_session_id))
  end

  storage.timelapse_capture = nil
  capture_checked_rollover = false
  -- The milestone file goes with the session folder, so the record of which
  -- milestones already fired has to go too, or none would ever be rewritten.
  milestones.reset()

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
    -- `name` reaches here only for a resource, whose removal a position alone
    -- would resolve to whatever stands on it instead.
    local names_it = name and encode.event_remove_name(capture_names, name) or ""
    return names_it .. encode.event_remove_entity(capture_surfaces, surface, x, y, id)
  end
  if op == "+" then
    return encode.event_add_tile(capture_names, capture_surfaces, surface, name, x, y)
  end
  return encode.event_remove_tile(capture_surfaces, surface, x, y)
end

local function log_event(op, kind, name, x, y, direction, id, w, h, surface)
  -- A nil surface (log_tile_change's can be nil for an unresolvable
  -- surface_index) is never excluded: a nil key read is legal and never
  -- equals true.
  if M.is_surface_excluded(surface) then
    return
  end

  if not capture_checked_rollover then
    ensure_capture_segment()
    capture_checked_rollover = true
  end

  -- Read once. `game.tick` is a property read across the Lua/C++ boundary
  -- like any other, and this asked for the same answer three times per event.
  local tick = game.tick
  storage.timelapse_capture.last_tick = tick

  -- Emitted once per distinct tick that has at least one event, rather than
  -- on every record: many events (a blueprint landing hundreds of entities)
  -- usually share a tick.
  if capture_last_written_tick ~= tick then
    capture_pending_count = capture_pending_count + 1
    capture_pending[capture_pending_count] = encode.event_set_tick(tick)
    capture_last_written_tick = tick
  end

  capture_pending_count = capture_pending_count + 1
  capture_pending[capture_pending_count] = encode_capture_event(op, kind, name, x, y, direction, id, w, h, surface)

  if capture_pending_count >= CAPTURE_FLUSH_EVERY then
    flush_capture()
  end
end

--- The type list `find_entities_filtered` wants, built once from the same set
--- `encode` states, so the two cannot drift.
local draggable_carrier_types = nil
local function draggable_carrier_type_list()
  if not draggable_carrier_types then
    draggable_carrier_types = {}
    for name in pairs(encode.DRAGGABLE_CARRIER_TYPES) do
      draggable_carrier_types[#draggable_carrier_types + 1] = name
    end
  end
  return draggable_carrier_types
end

--- Dragging a belt line around a corner makes Factorio rotate the belt already
--- placed, and raises no event for it, so the capture kept the facing that belt
--- went down with and every dragged corner drew straight.
---
--- The rotated one is the tile behind the new belt, opposite the way it faces,
--- so one lookup per belt placed catches it. Re-logging it as an ordinary add
--- costs the reader nothing when nothing changed: `World::insert` updates an
--- occupied position in place and skips the revision bump.
local function relog_belt_behind(surface, pos, direction)
  local dx, dy = encode.step_behind(direction)
  if not dx then
    return
  end

  local found = surface.find_entities_filtered({
    position = { x = pos.x + dx, y = pos.y + dy },
    type = draggable_carrier_type_list(),
  })[1]
  if not found then
    return
  end

  local back = found.position
  log_event("+", "e", found.name, back.x, back.y,
    found.direction, found.unit_number, found.tile_width, found.tile_height,
    surface.name)
end

--- Every field read here crosses the Lua/C++ boundary once per property, and
--- on a busy tick those crossings are most of what capture costs, so a removal
--- reads only what a removal record holds. Two calls rather than conditionals,
--- so what each kind of record needs is visible at the call site.
local function log_entity(op, entity)
  if not entity.valid or is_excluded_type(entity.type) then
    return
  end
  local pos = entity.position

  if op ~= "+" then
    -- Named only when it is a deposit, which is the one thing that can be
    -- buried under something else and so the one case a position alone cannot
    -- resolve (see `encode.event_remove_name`). The type was read above
    -- already, so every other removal still reads exactly what it writes.
    local buried = entity.type == "resource" and entity.name or nil
    log_event(op, "e", buried, pos.x, pos.y, nil, entity.unit_number, nil, nil, entity.surface.name)
    return
  end

  local direction = entity.direction
  local surface = entity.surface
  log_event(op, "e", entity.name, pos.x, pos.y,
    direction, entity.unit_number, entity.tile_width, entity.tile_height,
    surface.name)

  if encode.DRAGGABLE_CARRIER_TYPES[entity.type] then
    relog_belt_behind(surface, pos, direction)
  end
end

--- Whether natural ground is being captured at all. Memoized: a startup
--- setting cannot change during a session.
local capture_terrain = nil
local function terrain_captured()
  if capture_terrain == nil then
    capture_terrain = settings.startup["save-timelapse-capture-terrain"].value and true or false
  end
  return capture_terrain
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

      -- What the removal uncovered, logged as an ordinary add so the position
      -- holds it rather than going empty: mining landfill reveals water no
      -- baseline ever saw, and without this a filled lake un-fills into a hole.
      -- Readable only because these events fire after the tiles are replaced,
      -- and gated on terrain capture, which it would otherwise violate.
      if surface and terrain_captured() then
        local ok, revealed = pcall(function()
          return surface.get_tile(pos.x, pos.y).name
        end)
        if ok and revealed then
          log_event("+", "t", revealed, pos.x, pos.y, nil, nil, nil, nil, surface_name)
        end
      end
    end
  end
end

M.CAPTURE_HANDLERS = {
  [defines.events.on_built_entity] = function(e) log_entity("+", e.entity) end,
  [defines.events.on_robot_built_entity] = function(e) log_entity("+", e.entity) end,
  [defines.events.script_raised_built] = function(e) log_entity("+", e.entity) end,
  -- Rotating raises neither a build nor a removal, so without this an entity
  -- keeps the facing it was placed with for the rest of the playthrough.
  --
  -- Logged as an add, which is what an add already means: `World::insert`
  -- updates an occupied position in place and skips the revision bump when
  -- nothing changed. Covers rotating by hand only, a belt the game turns for
  -- you raising no event this listens for.
  [defines.events.on_player_rotated_entity] = function(e) log_entity("+", e.entity) end,
  [defines.events.on_player_mined_entity] = function(e) log_entity("-", e.entity) end,
  [defines.events.on_robot_mined_entity] = function(e) log_entity("-", e.entity) end,
  [defines.events.on_entity_died] = function(e) log_entity("-", e.entity) end,
  [defines.events.script_raised_destroy] = function(e) log_entity("-", e.entity) end,
  [defines.events.on_player_built_tile] = function(e) log_tile_change("+", e) end,
  [defines.events.on_robot_built_tile] = function(e) log_tile_change("+", e) end,
  [defines.events.on_player_mined_tile] = function(e) log_tile_change("-", e) end,
  [defines.events.on_robot_mined_tile] = function(e) log_tile_change("-", e) end,
}

--- Adds a handler only if this Factorio build defines the event. Indexing a
--- table with a nil key is a hard error in Lua rather than a skipped entry, so
--- a build missing one of these would fail to load the mod at all.
local function capture_handler(event_name, handler)
  local id = defines.events[event_name]
  if id then
    M.CAPTURE_HANDLERS[id] = handler
  end
end

-- Space platforms are a separate event family, and everything on one is placed
-- by platform construction bots, so without these a platform appears fully
-- formed and never changes. The payloads match their robot equivalents, which
-- is why these reuse the same two handlers.
capture_handler("on_space_platform_built_entity", function(e) log_entity("+", e.entity) end)
capture_handler("on_space_platform_mined_entity", function(e) log_entity("-", e.entity) end)
capture_handler("on_space_platform_built_tile", function(e) log_tile_change("+", e) end)
capture_handler("on_space_platform_mined_tile", function(e) log_tile_change("-", e) end)

-- A ghost revived by a script rather than by a bot: the path mods use.
-- Without it a modded construction aid places entities capture never sees.
capture_handler("script_raised_revive", function(e) log_entity("+", e.entity) end)

--- The periodic flush body, run from control.lua's timer multiplexer. Calls
--- `ensure_capture_segment` directly because a save that starts with capture
--- already on may reach here before anything is built.
---
--- Deliberately does not call `request_baseline`: a baseline is only taken in
--- response to an explicit player action, so an interrupted one does not
--- silently retry after a reload.
function M.periodic_flush(tick)
  ensure_capture_segment()
  capture_checked_rollover = true
  flush_capture()
  export.sample_connected_players(tick, storage.timelapse_capture.session_id)
  milestones.poll(tick, storage.timelapse_capture.session_id)
  -- Rewritten only when the loaded mods differ from what the file was written
  -- for, and that answer lives in `storage`. A module local meant "already
  -- done this session", which a load resets, so this rebuilt a couple of
  -- hundred kilobytes of JSON in one tick far more often than intended.
  local stamp = loaded_mods()
  if storage.timelapse_capture.prototypes_stamp ~= stamp then
    export.write_prototypes(storage.timelapse_capture.session_id)
    storage.timelapse_capture.prototypes_stamp = stamp
  end
end

--- Run when live capture is turned on: baselines immediately rather than
--- waiting for the first flush. Sets `capture_checked_rollover` so the next
--- `log_event` does not redundantly re-check the segment.
function M.on_capture_enabled(tick)
  capture_checked_rollover = true
  request_baseline(tick)
end

--- Run when the panel's Generate button is clicked. Checking a box only records
--- that a surface is wanted; its pre-existing state was never snapshotted, so
--- something has to take the catch-up baseline, and keeping it a separate step
--- batches several boxes into one freeze. A no-op while capture is off.
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
