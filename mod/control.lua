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

local function export_all(tick)
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
    string.format("%sframe_%d_manifest.json", EXPORT_DIR, tick),
    string.format('{"tick":%d,"entities":%d,"tiles":%d,"surfaces":[%s]}',
      tick, total, tile_total, table.concat(names, ",")),
    false)

  return total, tile_total, #names
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
-- --benchmark, and we export on the first tick then unsubscribe so the run
-- reaches its tick limit and exits.
if settings.startup["save-timelapse-headless-scan"].value then
  script.on_event(defines.events.on_tick, function(event)
    export_all(event.tick)
    script.on_event(defines.events.on_tick, nil)
  end)
end

-- ---------------------------------------------------------------------------
-- Live capture

local CAPTURE_FLUSH_EVERY = 200
local CAPTURE_FLUSH_TICKS = 600 -- ~10 real seconds, bounds data loss even when idle
local SNAPSHOT_BATCH_SIZE = 1000 -- work items processed per tick while exporting

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

--- Runs when the baseline finishes: writes the manifest that tells the Rust
--- side the baseline is complete and what it covers, then hands back the
--- tick so the caller can record that this save now has one.
local function snapshot_finish(s)
  local quoted = {}
  for i, name in pairs(s.surface_names) do
    quoted[i] = encode.quote(name)
  end

  helpers.write_file(BASELINE_MANIFEST, string.format(
    '{"tick":%d,"entities":%d,"tiles":%d,"surfaces":[%s]}',
    s.tick, s.total_entities, s.total_tiles, table.concat(quoted, ",")), false)

  if s.on_complete then
    s.on_complete(s.tick)
  end
end

local function snapshot_start(tick, on_complete)
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
    on_complete = on_complete,
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
  script.on_event(defines.events.on_tick, function(event)
    if not snapshot_state then
      script.on_event(defines.events.on_tick, nil)
      return
    end

    local s = snapshot_state
    if not s.phase then
      local surface = game.surfaces[s.surface_names[s.surface_index]]
      if not surface then
        snapshot_state = nil
        script.on_event(defines.events.on_tick, nil)
        return
      end
      snapshot_begin_surface(s, surface)
    end

    local surface = game.surfaces[s.surface_name]
    if not surface then
      snapshot_state = nil
      script.on_event(defines.events.on_tick, nil)
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
          if s.pending_count >= FLUSH_EVERY then
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
    end

    if s.phase == "tiles" then
      local end_index = math.min(s.tile_index + SNAPSHOT_BATCH_SIZE - 1, #s.tiles)
      for i = s.tile_index, end_index do
        local tile = s.tiles[i]
        s.tiles_written = s.tiles_written + 1
        s.pending_count = s.pending_count + 1
        s.pending[s.pending_count] = (s.tiles_written > 1 and "," or "") .. encode.encode_tile(tile)
        if s.pending_count >= FLUSH_EVERY then
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
          script.on_event(defines.events.on_tick, nil)
          return
        end
      end
    end
  end)
end

--- Take the baseline once per save, then never again: everything after it is
--- reconstructed by replaying the event log, so a second full snapshot would
--- be pure duplication -- at roughly 50 bytes per entity, a megabase snapshot
--- every 10 seconds was writing gigabytes an hour to say what the log
--- already said.
---
--- `baseline_tick` is recorded in `storage`, so it travels inside the save:
--- a save that has been baselined knows it, and a fresh one does not. It is
--- set only on *completion*, so a game saved and reloaded midway through the
--- export simply starts over rather than leaving the Rust side with a
--- truncated baseline it believes is whole. The abandoned partial files are
--- orphans that no manifest names, so they are ignored rather than harmful.
---
--- Events are logged during the export too. The snapshot is spread over many
--- ticks and so is not atomic: something built while it runs may or may not
--- land in the baseline depending on whether its surface was already written.
--- Replay absorbs that by applying adds idempotently and tolerating removes
--- of things it never saw, which makes the smear harmless rather than
--- something the mod has to prevent.
local function ensure_baseline(tick)
  ensure_capture_segment()
  if storage.timelapse_capture.baseline_tick or snapshot_state then
    return
  end
  snapshot_start(tick, function(completed_tick)
    storage.timelapse_capture.baseline_tick = completed_tick
  end)
end

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

--- Handlers are only subscribed while capture is on -- not registered but
--- checking a flag on every call -- so there's zero hook cost when it's off.
local function sync_capture_subscriptions()
  local on = settings.global["save-timelapse-live-capture"].value

  for event_id, handler in pairs(CAPTURE_HANDLERS) do
    script.on_event(event_id, on and handler or nil)
  end

  -- Also where an interrupted baseline gets retried: `on_load` cannot write
  -- storage or start one, so the first flush after a reload is what notices
  -- `baseline_tick` is still unset and restarts the export.
  script.on_nth_tick(CAPTURE_FLUSH_TICKS, on and function(event)
    capture_checked_rollover = true
    ensure_baseline(event.tick)
    flush_capture()
  end or nil)
end

script.on_init(sync_capture_subscriptions)
script.on_load(sync_capture_subscriptions)
script.on_event(defines.events.on_runtime_mod_setting_changed, function(event)
  if event.setting == "save-timelapse-live-capture" then
    sync_capture_subscriptions()
    if settings.global["save-timelapse-live-capture"].value then
      capture_checked_rollover = true
      -- Turning capture on is the one moment we can baseline immediately
      -- rather than waiting up to CAPTURE_FLUSH_TICKS for the first flush.
      ensure_baseline(game.tick)
    end
  end
end)
