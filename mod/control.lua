-- save-timelapse
-- Two independent ways to get data out of a save for timelapse rendering:
--
-- Snapshot export: writes every entity/tile on a surface, right now. Runs
-- either from the /timelapse-export command or, for unattended runs, from
-- the save-timelapse-headless-scan startup setting. Retroactive, works on
-- saves that already exist, but only as fine grained as however often the
-- player saved.
--
-- Live capture: logs every construction event as it happens, for
-- frame-perfect playback. Only covers play from the moment it's turned on,
-- since Factorio keeps no placement history inside a save to recover
-- retroactively. Toggled via the save-timelapse-live-capture runtime setting.
--
-- Both share their binary-encoding logic via encode.lua, which has no
-- Factorio dependency and is unit tested standalone (tests/encode_test.lua).

local encode = require("encode")

local EXPORT_DIR = "save-timelapse/"
local FLUSH_EVERY = 2000
--- How far past the entities/placed-floor bounding box to also capture
--- natural terrain, so the factory reads as sitting on real ground rather
--- than stopping at a hard edge. Roughly a chunk; not exposed as a setting
--- since nothing has asked for this to be tunable yet.
local TERRAIN_MARGIN_TILES = 32

--- Set at load when the CLI's startup flag is on, and acted on by the single
--- on_tick handler at the bottom of this file rather than by registering one
--- here. Factorio keeps one handler per event, so a second registration would
--- silently replace this one, which is exactly what an incremental snapshot
--- wanting on_tick would do.
local headless_scan_pending = false

--- Trees and cliffs are excluded here, on top of `encode.EXCLUDED_TYPES`'s
--- always-excluded set, when terrain capture is off -- one setting
--- controlling all of it (them plus the natural-ground tile pass further
--- down) rather than scatter entities always showing regardless of the
--- toggle someone just turned off because of its cost.
local function excluded_types()
  local list = {}
  for _, t in pairs(encode.EXCLUDED_TYPES) do
    list[#list + 1] = t
  end
  if not settings.startup["save-timelapse-include-resources"].value then
    list[#list + 1] = "resource"
  end
  if not settings.startup["save-timelapse-capture-terrain"].value then
    list[#list + 1] = "tree"
    list[#list + 1] = "cliff"
  end
  return list
end

--- Pairs a write with folding the same bytes into a running checksum, so
--- every frame-file writer accumulates one the same way rather than each
--- repeating the two calls side by side. Returns the updated checksum,
--- Lua-style, since there is no reference to update in place.
local function checksummed_write(path, data, append, checksum)
  helpers.write_file(path, data, append)
  return encode.checksum_update(checksum, data)
end

--- Write one surface to its own file. Returns entity and tile counts written.
---
--- `session_id`, when given, tags the path so a baseline written into the
--- shared, persistent script-output folder can't collide with another
--- playthrough's (see encode.baseline_manifest_name's comment for why).
--- `/timelapse-export` and the headless scan call this with no session id:
--- both run against a private, per-run script-output folder that nothing
--- else ever writes into, so there is nothing for them to collide with.
local function export_surface(surface, tick, session_id)
  local tag = session_id and string.format("%08x_", session_id) or ""
  local path = string.format("%sframe_%s%d_%s.stfr", EXPORT_DIR, tag, tick, surface.name)
  local dict = encode.new_dictionary()

  local checksum = encode.checksum_init()
  checksum = checksummed_write(path, encode.frame_header(tick, surface.name), false, checksum)

  -- Grown as entities and placed floor below are scanned, so the terrain
  -- pass after them knows what area to cover without a separate scan of
  -- the whole surface just to learn its extent.
  local bbox = encode.new_bbox()

  local pending, pending_count, written = {}, 0, 0

  for _, entity in pairs(surface.find_entities_filtered({
    type = excluded_types(),
    invert = true,
  })) do
    if entity.valid then
      pending_count = pending_count + 1
      written = written + 1
      pending[pending_count] = encode.frame_entity_record(dict, entity)
      encode.grow_bbox(bbox, entity.position.x, entity.position.y)

      -- Each write_file call is a separate file append, so flushing per entity
      -- would make export time track syscalls rather than entity count.
      if pending_count >= FLUSH_EVERY then
        checksum = checksummed_write(path, table.concat(pending), true, checksum)
        pending, pending_count = {}, 0
      end
    end
  end

  if pending_count > 0 then
    checksum = checksummed_write(path, table.concat(pending), true, checksum)
  end

  checksum = checksummed_write(path, encode.frame_end_entities(), true, checksum)

  pending, pending_count = {}, 0
  local tiles_written = 0

  for _, tile in pairs(surface.find_tiles_filtered({ name = encode.PLACED_FLOOR_TILES })) do
    pending_count = pending_count + 1
    tiles_written = tiles_written + 1
    pending[pending_count] = encode.frame_tile_record(dict, tile)
    encode.grow_bbox(bbox, tile.position.x, tile.position.y)

    if pending_count >= FLUSH_EVERY then
      checksum = checksummed_write(path, table.concat(pending), true, checksum)
      pending, pending_count = {}, 0
    end
  end

  if pending_count > 0 then
    checksum = checksummed_write(path, table.concat(pending), true, checksum)
  end

  -- Natural terrain (grass, water, sand, ...) covers every generated tile,
  -- not just where the player built, so it is capped to a margin around
  -- the factory rather than the whole surface -- otherwise it would dwarf
  -- everything else exported here. Off by default (see settings.lua): it
  -- roughly 5x'd export size and time in testing, so opting in is a real
  -- decision, not a free improvement. `nil` when nothing was seen above
  -- (an untouched surface has no factory to show context around) also
  -- skips it, same as the setting being off.
  local terrain_area = settings.startup["save-timelapse-capture-terrain"].value
    and encode.expand_bbox(bbox, TERRAIN_MARGIN_TILES)
  if terrain_area then
    for _, tile in pairs(surface.find_tiles_filtered({
      area = terrain_area,
      name = encode.PLACED_FLOOR_TILES,
      invert = true,
    })) do
      pending_count = pending_count + 1
      tiles_written = tiles_written + 1
      pending[pending_count] = encode.frame_tile_record(dict, tile)

      if pending_count >= FLUSH_EVERY then
        checksum = checksummed_write(path, table.concat(pending), true, checksum)
        pending, pending_count = {}, 0
      end
    end

    if pending_count > 0 then
      checksum = checksummed_write(path, table.concat(pending), true, checksum)
    end
  end

  -- Not itself folded into the checksum: nothing needs a checksum of the
  -- checksum, and the reader already knows the trailer's fixed size.
  helpers.write_file(path, encode.u32le(checksum), true)

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

-- ---------------------------------------------------------------------------
-- Player position tracking
--
-- A separate, deliberately simple newline-delimited JSON log (not the
-- binary formats above): a sample happens at most once every several
-- seconds by design, nowhere near the per-tick construction volume that
-- actually justified going binary for frames and events, so there is
-- nothing here for a text format's formatting/parsing cost to be a problem
-- for. The same shape is both what this mod writes and what the viewer
-- reads (see src/player_log.rs) -- save-timelapse.exe just relocates the
-- file into its output directory, no conversion step.

--- Untagged for /timelapse-export and headless scan, tagged by session_id
--- for live capture, exactly like `baseline_manifest_path`.
local function player_log_path(session_id)
  if not session_id then
    return EXPORT_DIR .. "players.jsonl"
  end
  return EXPORT_DIR .. encode.player_log_name(session_id)
end

--- Periodic, for live capture: only players actually connected right now.
--- Wrapped in `pcall` per player, the same defensive style as
--- `compute_session_id`/`is_inhabited`: a player with no valid position
--- right now (e.g. true spectator state) is skipped rather than raising.
local function sample_connected_players(tick, session_id)
  local players = {}
  for _, player in pairs(game.connected_players) do
    local ok, name, surface, x, y = pcall(function()
      return player.name, player.surface.name, player.position.x, player.position.y
    end)
    if ok then
      players[#players + 1] = { name = name, surface = surface, x = x, y = y }
    end
  end
  if #players > 0 then
    helpers.write_file(player_log_path(session_id), encode.player_log_line(tick, players), true)
  end
end

--- One-shot, for /timelapse-export, headless scan, and the baseline: reads
--- every player who has ever played this save via `game.players`, not
--- `game.connected_players`, which would just be empty in headless mode
--- (nobody is technically "connected" to run a benchmark). A player whose
--- character does not currently exist (never spawned, or dead) is skipped.
local function sample_all_players(tick, session_id)
  local players = {}
  for _, player in pairs(game.players) do
    local ok, name, surface, x, y = pcall(function()
      return player.name, player.character.surface.name, player.character.position.x, player.character.position.y
    end)
    if ok then
      players[#players + 1] = { name = name, surface = surface, x = x, y = y }
    end
  end
  if #players > 0 then
    helpers.write_file(player_log_path(session_id), encode.player_log_line(tick, players), true)
  end
end

--- Every surface, synchronously, in whatever tick this is called from. Used
--- for /timelapse-export, headless scan, and the once-per-save baseline,
--- three callers wanting the exact same "everything, right now" export,
--- differing only in what manifest path names the result and, for the
--- baseline, in tagging its output with the playthrough's session_id. Also
--- where all three record where the player(s) were, one line, alongside
--- the entities and tiles.
local function export_all_to(tick, manifest_path, session_id)
  sample_all_players(tick, session_id)
  local names, total, tile_total = {}, 0, 0

  for _, surface in pairs(game.surfaces) do
    if is_inhabited(surface) then
      local entities, tiles = export_surface(surface, tick, session_id)
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
local CAPTURE_FLUSH_TICKS = 240 -- ~4 real seconds, bounds data loss even when idle
--- Work items encoded per tick while the periodic test-snapshot runs, and how
--- many encoded strings accumulate before a file append. Deliberately small:
--- the point of spreading that export over ticks is that no single tick
--- stalls, and a big batch gives that back. The baseline does not use this,
--- see export_all_to, since it runs at most once per save and a one-time
--- freeze was judged worth it there to avoid a background cost smeared across
--- the next several minutes of play. Separate from FLUSH_EVERY, which serves
--- every synchronous export path (`/timelapse-export`, headless scan,
--- baseline) where syscall count, not smoothness, is the cost.
local SNAPSHOT_BATCH_SIZE = 64
local SNAPSHOT_FLUSH_EVERY = 128

--- Written once, after the baseline snapshot finishes, naming the tick and
--- surfaces it covers. This is the handshake with the Rust side: it is the
--- last file written, so its presence means the baseline is complete, and it
--- says which `frame_<session_id>_<tick>_<surface>.stfr` files make up that
--- baseline. Everything after the baseline is reconstructed by replaying the
--- event log. Tagged by session_id (see compute_session_id below) so each
--- playthrough gets its own manifest instead of overwriting another one's.
---
--- A save whose capture state predates session_id existing has it as `nil`
--- (see ensure_capture_segment below, which only ever sets it while creating
--- fresh state): this keeps such a save on the untagged name it was already
--- using rather than erroring, until /timelapse-reset-capture clears its
--- state and lets it start over with a real one.
local function baseline_manifest_path(session_id)
  if not session_id then
    return EXPORT_DIR .. "baseline.json"
  end
  return EXPORT_DIR .. encode.baseline_manifest_name(session_id)
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

--- See baseline_manifest_path above for why a nil session_id (a save whose
--- capture state predates this feature) falls back to the untagged name
--- instead of erroring.
local function capture_segment_path(session_id, start_tick)
  if not session_id then
    return string.format("%sevents_%d.stev", EXPORT_DIR, start_tick)
  end
  return EXPORT_DIR .. encode.capture_segment_name(session_id, start_tick)
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
    helpers.write_file(capture_path, encode.event_header(), false)
    state.segment_initialized = true
  end
end

local function snapshot_path(tick, surface_name)
  return string.format("%sframe_%d_%s.stfr", EXPORT_DIR, tick, surface_name)
end

local function snapshot_flush(state)
  if state.pending_count > 0 then
    state.checksum = checksummed_write(state.path, table.concat(state.pending), true, state.checksum)
    state.pending, state.pending_count = {}, 0
  end
end

local function snapshot_begin_surface(state, surface)
  state.path = snapshot_path(state.tick, surface.name)
  state.surface_name = surface.name
  state.dict = encode.new_dictionary()
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
  state.checksum = encode.checksum_init()
  state.checksum = checksummed_write(state.path, encode.frame_header(state.tick, surface.name), false, state.checksum)
end

--- Runs when a snapshot finishes: writes its manifest. Written last, so its
--- presence is what tells a reader the snapshot is whole rather than still
--- in progress. Only the periodic test-snapshot timer goes through this path
--- now, the baseline runs synchronously via export_all_to below, so the
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
        s.pending[s.pending_count] = encode.frame_entity_record(s.dict, entity)
        if s.pending_count >= SNAPSHOT_FLUSH_EVERY then
          snapshot_flush(s)
        end
      end
    end
    s.entity_index = end_index + 1
    if s.entity_index > #s.entities then
      snapshot_flush(s)
      s.checksum = checksummed_write(s.path, encode.frame_end_entities(), true, s.checksum)
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
      s.pending[s.pending_count] = encode.frame_tile_record(s.dict, tile)
      if s.pending_count >= SNAPSHOT_FLUSH_EVERY then
        snapshot_flush(s)
      end
    end
    s.tile_index = end_index + 1
    if s.tile_index > #s.tiles then
      snapshot_flush(s)
      -- Not itself folded into the checksum, same as export_surface's
      -- trailer: nothing needs a checksum of the checksum.
      helpers.write_file(s.path, encode.u32le(s.checksum), true)
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
    dict = nil,
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
--- be pure duplication, at roughly 50 bytes per entity, a megabase snapshot
--- every 10 seconds was writing gigabytes an hour to say what the log
--- already said.
---
--- Runs synchronously in a single tick via export_all_to, unlike the
--- incremental snapshot_start/snapshot_step the periodic test-snapshot
--- setting uses. That incremental machinery exists specifically to avoid a
--- visible freeze on every run, the right trade for something that repeats.
--- A baseline runs at most once per save, so the trade flips: a freeze
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
    if is_inhabited(surface) then
      total = total + surface.count_entities_filtered({ type = excluded_types(), invert = true })
    end
  end

  game.print(string.format(
    "[save-timelapse] exporting a %d entity baseline starting next tick, the game will be " ..
    "unresponsive until it finishes (measured at tens of seconds for a ~375k entity base)",
    total))

  baseline_pending = true
end

--- The other half of `request_baseline`, run from `on_tick` one tick after
--- the warning so that message gets a chance to actually render first.
local function perform_baseline(tick)
  baseline_pending = false
  local capture = storage.timelapse_capture
  if capture.baseline_tick then
    return
  end

  local total, tiles, surfaces =
    export_all_to(tick, baseline_manifest_path(capture.session_id), capture.session_id)
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
    helpers.write_file(capture_path, table.concat(capture_pending), true)
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
-- erroring. CAPTURE_FLUSH_TICKS is 240 (4 real seconds), and the periodic
-- test-snapshot setting below is also given in seconds, so a user picking 4
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

--- Handlers are only subscribed while their setting is on, not registered
--- but checking a flag on every call, so there's zero hook cost when off.
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
      request_baseline(event.tick)
      flush_capture()
      -- After request_baseline, which guarantees storage.timelapse_capture
      -- exists (see ensure_capture_segment): session_id may still be nil
      -- for a save whose capture state predates it, and player_log_path
      -- already falls back to the untagged name for that case.
      sample_connected_players(event.tick, storage.timelapse_capture.session_id)
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
  if baseline_pending then
    perform_baseline(event.tick)
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
      request_baseline(game.tick)
    end
  end
end)
