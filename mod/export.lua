-- save-timelapse: one-shot "export everything, right now" snapshot logic,
-- shared by /timelapse-export, the save-timelapse-headless-scan startup
-- setting, and live capture's own baseline (see capture.lua, which calls
-- M.export_all_to directly). Also owns the pcall-wrapped write helpers
-- every capture write in this mod (this file's and capture.lua's) goes
-- through, since a write failure is the same "degrade, don't crash"
-- concern regardless of which feature triggered it.

local encode = require("encode")

local M = {}

M.EXPORT_DIR = "save-timelapse/"
local FLUSH_EVERY = 2000
--- How far past the entities/placed-floor bounding box to also capture
--- natural terrain, so the factory reads as sitting on real ground rather
--- than stopping at a hard edge. Roughly a chunk; not exposed as a setting
--- since nothing has asked for this to be tunable yet.
local TERRAIN_MARGIN_TILES = 32

--- Set at load when the CLI's startup flag is on, and acted on by
--- `M.run_pending_tick_work` below rather than by registering an on_tick
--- handler here. Factorio keeps one handler per event, so a second
--- registration would silently replace control.lua's own, which is exactly
--- what an incremental snapshot wanting on_tick would do.
local headless_scan_pending = false

-- Unattended path. The CLI enables the startup flag, loads the save under
-- --benchmark, and the export runs on the first tick so the run reaches its
-- tick limit and exits.
if settings.startup["save-timelapse-headless-scan"].value then
  headless_scan_pending = true
end

--- Trees and cliffs are excluded here, on top of `encode.EXCLUDED_TYPES`'s
--- always-excluded set, when terrain capture is off: one setting
--- controls all of it (them plus the natural-ground tile pass further
--- down) rather than scatter entities always showing regardless of the
--- toggle someone just turned off because of its cost.
function M.excluded_types()
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

--- Whether a capture write has already failed this session (disk full,
--- permissions, a program locking the file, the kind of thing a
--- long-running live capture is exactly the workload to eventually hit).
--- Plain module local, not `storage`: Factorio re-runs this file's top
--- level on every load, so a transient failure doesn't wrongly disable
--- capture forever across a reload, only for the rest of the session it
--- actually happened in.
local capture_write_failed = false

--- Wraps helpers.write_file so a capture write that throws degrades the
--- capture instead of crashing the whole game with an uncaught mod error.
--- Once one write fails, every later capture write no-ops for the rest of
--- this session rather than retrying (and re-warning about) the same
--- failure on every flush.
function M.safe_write_file(path, data, append)
  if capture_write_failed then
    return false
  end
  local ok, err = pcall(helpers.write_file, path, data, append)
  if not ok then
    capture_write_failed = true
    game.print("[save-timelapse] capture write failed, capture stopped for this session: " .. tostring(err))
  end
  return ok
end

--- Pairs a write with folding the same bytes into a running checksum, so
--- every frame-file writer accumulates one the same way rather than each
--- repeating the two calls side by side. Returns the updated checksum,
--- Lua-style, since there is no reference to update in place.
function M.checksummed_write(path, data, append, checksum)
  M.safe_write_file(path, data, append)
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
  local path = M.EXPORT_DIR .. encode.frame_name(session_id, tick, surface.name)
  local dict = encode.new_dictionary()

  local checksum = encode.checksum_init()
  checksum = M.checksummed_write(path, encode.frame_header(tick, surface.name), false, checksum)

  -- Grown as entities and placed floor below are scanned, so the terrain
  -- pass after them knows what area to cover without a separate scan of
  -- the whole surface just to learn its extent.
  local bbox = encode.new_bbox()

  local pending, pending_count, written = {}, 0, 0

  for _, entity in pairs(surface.find_entities_filtered({
    type = M.excluded_types(),
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
        checksum = M.checksummed_write(path, table.concat(pending), true, checksum)
        pending, pending_count = {}, 0
      end
    end
  end

  if pending_count > 0 then
    checksum = M.checksummed_write(path, table.concat(pending), true, checksum)
  end

  checksum = M.checksummed_write(path, encode.frame_end_entities(), true, checksum)

  pending, pending_count = {}, 0
  local tiles_written = 0

  for _, tile in pairs(surface.find_tiles_filtered({ name = encode.PLACED_FLOOR_TILES })) do
    pending_count = pending_count + 1
    tiles_written = tiles_written + 1
    pending[pending_count] = encode.frame_tile_record(dict, tile)
    encode.grow_bbox(bbox, tile.position.x, tile.position.y)

    if pending_count >= FLUSH_EVERY then
      checksum = M.checksummed_write(path, table.concat(pending), true, checksum)
      pending, pending_count = {}, 0
    end
  end

  if pending_count > 0 then
    checksum = M.checksummed_write(path, table.concat(pending), true, checksum)
  end

  -- Natural terrain (grass, water, sand, ...) covers every generated tile,
  -- not just where the player built, so it is capped to a margin around
  -- the factory rather than the whole surface, since the whole surface
  -- would otherwise dwarf everything else exported here. Off by default
  -- (see settings.lua): it roughly 5x'd export size and time in testing,
  -- so opting in is a real decision, not a free improvement. `nil` when
  -- nothing was seen above (an untouched surface has no factory to show
  -- context around) also skips it, same as the setting being off.
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
        checksum = M.checksummed_write(path, table.concat(pending), true, checksum)
        pending, pending_count = {}, 0
      end
    end

    if pending_count > 0 then
      checksum = M.checksummed_write(path, table.concat(pending), true, checksum)
    end
  end

  -- Not itself folded into the checksum: nothing needs a checksum of the
  -- checksum, and the reader already knows the trailer's fixed size.
  M.safe_write_file(path, encode.u32le(checksum), true)

  return written, tiles_written
end

--- A surface is worth exporting if it is nauvis or the player built on it.
function M.is_inhabited(surface)
  if surface.name == "nauvis" then
    return true
  end
  local ok, found = pcall(function()
    return surface.find_entities_filtered({ force = "player", limit = 1 })
  end)
  return ok and found ~= nil and #found > 0
end

--- Shared by the synchronous export below and the periodic test-snapshot
--- timer (see snapshot.lua): both describe "everything exported at this
--- tick" in the same shape, so both write it through one function rather
--- than two copies drifting.
function M.periodic_manifest_path(tick)
  return string.format("%sframe_%d_manifest.json", M.EXPORT_DIR, tick)
end

-- Player position tracking
--
-- A separate, deliberately simple newline-delimited JSON log (not the
-- binary formats above): a sample happens at most once every several
-- seconds by design, nowhere near the per-tick construction volume that
-- actually justified going binary for frames and events, so there is
-- nothing here for a text format's formatting/parsing cost to be a problem
-- for. The same shape is both what this mod writes and what the viewer
-- reads (see src/player_log.rs), so save-timelapse.exe just relocates the
-- file into its output directory, no conversion step.

--- Untagged for /timelapse-export and headless scan, tagged by session_id
--- for live capture, exactly like `baseline_manifest_path` (capture.lua).
local function player_log_path(session_id)
  if not session_id then
    return M.EXPORT_DIR .. "players.jsonl"
  end
  return M.EXPORT_DIR .. encode.player_log_name(session_id)
end

--- Periodic, for live capture: only players actually connected right now.
--- Wrapped in `pcall` per player, the same defensive style as
--- `compute_session_id`/`is_inhabited`: a player with no valid position
--- right now (e.g. true spectator state) is skipped rather than raising.
function M.sample_connected_players(tick, session_id)
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

--- Exports exactly `surface_names` (already filtered by the caller: see
--- capture.lua's baseline-gap scan), each to its own
--- frame_<tick>_<surface>.stfr in the same shape `M.export_all_to`'s own
--- loop produces, but writes no manifest of any kind.
---
--- Why no manifest: `baseline.json`'s tick/surfaces describe the original,
--- once-per-session baseline. A catch-up covers a different surface at a
--- different (later) tick that has nothing to do with what `baseline.json`
--- already says about every surface it doesn't mention; overwriting it
--- here would corrupt that meaning for every surface it already covers.
--- Rust instead discovers a catch-up by finding an extra
--- frame_<tick>_<surface>.stfr file the manifest doesn't already account
--- for (see replay.rs's `discover_catch_up_baselines`), so there is
--- deliberately no separate manifest for catch-ups either: `baseline.json`
--- keeps its original, simple meaning intact no matter how many later
--- catch-ups a session accumulates.
---
--- Still samples players, the same as `M.export_all_to`: an accurate
--- "where was everyone" line for the tick this happened at is exactly as
--- useful here as for any other export.
---
--- Trusts `surface_names` outright, no inhabited/exclusion re-check: the
--- caller already computed exactly that gap a moment ago, in this same
--- tick.
function M.export_surfaces_to(tick, session_id, surface_names)
  sample_all_players(tick, session_id)
  local total, tile_total, exported = 0, 0, {}
  for _, name in ipairs(surface_names) do
    local surface = game.surfaces[name]
    if surface then
      local entities, tiles = export_surface(surface, tick, session_id)
      total = total + entities
      tile_total = tile_total + tiles
      exported[#exported + 1] = name
    end
  end
  return total, tile_total, exported
end

--- Every surface, synchronously, in whatever tick this is called from. Used
--- for /timelapse-export, headless scan, and the once-per-save baseline
--- (capture.lua), three callers wanting the exact same "everything, right
--- now" export, differing only in what manifest path names the result,
--- for the baseline in tagging its output with the playthrough's
--- session_id, and for the baseline alone in `is_excluded`. Also where all
--- three record where the player(s) were, one line, alongside the entities
--- and tiles.
---
--- `is_excluded`, when given, skips a surface entirely instead of exporting
--- it: a surface the player has opted out of recording (see capture.lua's
--- per-surface exclusion) shouldn't pay the baseline's cost either, which
--- is the single most expensive part of capture (tens of seconds on a
--- large base) and the whole reason exclusion exists in the first place,
--- not just something incremental events already skip after the fact.
--- `/timelapse-export` and the headless scan pass nothing (`nil`): neither
--- has any concept of exclusion, and both always mean "everything, right
--- now."
function M.export_all_to(tick, manifest_path, session_id, is_excluded)
  sample_all_players(tick, session_id)
  local names, total, tile_total = {}, 0, 0

  for _, surface in pairs(game.surfaces) do
    if M.is_inhabited(surface) and not (is_excluded and is_excluded(surface.name)) then
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
  return M.export_all_to(tick, M.periodic_manifest_path(tick))
end

commands.add_command("timelapse-export",
  "Export this save's entities for timelapse rendering.",
  function(event)
    local total, tiles, surfaces = export_all(game.tick)
    local player = event.player_index and game.get_player(event.player_index)
    if player then
      player.print(string.format(
        "[save-timelapse] exported %d entities and %d tiles from %d surface(s) to script-output/%s",
        total, tiles, surfaces, M.EXPORT_DIR))
    end
  end)

--- Runs the headless scan if the startup setting requested one, and this is
--- the first tick since load; no-ops every other tick. Called unconditionally
--- from control.lua's on_tick, alongside capture.lua's and snapshot.lua's
--- own pending-work checks.
function M.run_pending_tick_work(tick)
  if headless_scan_pending then
    export_all(tick)
    headless_scan_pending = false
  end
end

return M
