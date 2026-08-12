-- save-timelapse: one-shot "export everything, right now" snapshot logic,
-- shared by /timelapse-export, the headless-scan startup setting, and live
-- capture's baseline. Also owns the pcall-wrapped write helpers every capture
-- write in this mod goes through.

local encode = require("encode")

local M = {}

M.EXPORT_DIR = "save-timelapse/"
local FLUSH_EVERY = 2000
--- The least ground to capture past the built area, so the factory reads as
--- sitting on real land rather than stopping at an edge. A floor, not the
--- whole answer: `encode.terrain_margin` widens it to what a fitted 16:9
--- frame actually shows, which on anything large is far more.
local TERRAIN_MARGIN_TILES = 32

--- Set at load when the CLI's startup flag is on, and acted on by
--- `M.run_pending_tick_work` rather than by registering on_tick here.
--- Factorio keeps one handler per event, so a second registration would
--- silently replace control.lua's.
local headless_scan_pending = false

-- Unattended path. The CLI enables the startup flag, loads the save under
-- --benchmark, and the export runs on the first tick so the run reaches its
-- tick limit and exits.
if settings.startup["save-timelapse-headless-scan"].value then
  headless_scan_pending = true
end

--- Set the same way, for the ground-only pass. Separate from the scan above
--- rather than a mode of it because the two run against different saves: the
--- frames come from every save in the set, the ground from one.
local terrain_scan_pending = false
if settings.startup["save-timelapse-terrain-scan"].value then
  terrain_scan_pending = true
end

--- Trees and cliffs are excluded on top of `encode.EXCLUDED_TYPES` when
--- terrain capture is off, so one setting controls all of it rather than
--- scatter entities showing regardless of the toggle just turned off.
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
    -- Gleba's flora is type "plant", not "tree". Without this, turning
    -- terrain capture off silenced Nauvis's forests while Gleba's, the more
    -- expensive half, kept being recorded.
    list[#list + 1] = "plant"
  end
  return list
end

--- Scenery: entities the map generated rather than anybody placing, so they sit
--- on every generated chunk. Recorded, but only near the factory, like the
--- ground they stand on: taken from the whole surface, trees, resources and
--- nests were 69% of every frame on a real megabase.
---
--- Worms cannot join the list, sharing the "turret" type with player turrets.
--- The list lives in `encode.lua`, which has no Factorio dependency and can be
--- unit tested.
function M.context_types()
  return encode.context_types(
    settings.startup["save-timelapse-include-resources"].value,
    settings.startup["save-timelapse-capture-terrain"].value
  )
end

--- What the unbounded entity pass skips: everything never recorded at all,
--- plus the scenery the bounded pass handles instead. Not folded into
--- `M.excluded_types()`, which decides what a live event may log: scenery is
--- genuinely recorded, so an event touching it is not one to drop.
local function unbounded_excludes()
  local list = M.excluded_types()
  for _, t in pairs(M.context_types()) do
    list[#list + 1] = t
  end
  return list
end

--- Whether a capture write has already failed this session (disk full,
--- permissions, a file lock). A module local rather than `storage`, so a
--- transient failure disables capture for the rest of this session only, not
--- forever across a reload.
local capture_write_failed = false

--- Wraps helpers.write_file so a failed capture write degrades the capture
--- instead of crashing the game. After one failure every later write no-ops,
--- rather than re-warning on every flush.
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
--- every frame-file writer accumulates one the same way. Returns the updated
--- checksum, there being no reference to update in place.
function M.checksummed_write(path, data, append, checksum)
  M.safe_write_file(path, data, append)
  return encode.checksum_update(checksum, data)
end

--- Whether somebody built `entity`, as opposed to the map generating it.
--- `"player"` is the same force name `M.is_inhabited` and
--- `M.milestone_state` treat as the player's.
local function is_player_built(entity)
  local force = entity.force
  return force ~= nil and force.name == "player"
end

--- Write one surface to its own file. Returns entity and tile counts written.
---
--- `session_id`, when given, tags the path so a baseline in the shared
--- script-output folder cannot collide with another playthrough's. The other
--- two callers run against a private per-run folder and pass none.
local function export_surface(surface, tick, session_id)
  local path = M.EXPORT_DIR .. encode.frame_name(session_id, tick, surface.name)
  local dict = encode.new_dictionary()

  local checksum = encode.checksum_init()
  checksum = M.checksummed_write(path, encode.frame_header(tick, surface.name), false, checksum)

  -- Grown as entities and placed floor are scanned, so the terrain pass knows
  -- its area without a second scan. Only what somebody built grows it:
  -- everything else sits wherever the map generated it, so letting those in
  -- made this the explored map rather than the factory.
  local bbox = encode.new_bbox()

  -- Asked once per distinct name rather than once per entity:
  -- `entity.force.name` is two boundary crossings and this loop runs per
  -- entity on bases holding hundreds of thousands. Per name means a prototype
  -- standing on two forces is judged by whichever was seen first, which shifts
  -- the ground margin by one building at the edge.
  local player_built = {}

  local pending_count, written = 0, 0

  -- Records are grouped by name into runs, so entities are collected by name as
  -- they are scanned and written when the batch fills. Per batch rather than
  -- per frame: buffering a megabase to group it would reintroduce the stall
  -- this exporter avoids. Straight into parallel flat arrays, a table per
  -- entity measuring 1.26x slower at 900k against 0.60x this way.
  local order, groups = {}, {}

  local function group_for(name, w, h)
    local group = groups[name]
    if not group then
      group = { w = w, h = h, n = 0, xs = {}, ys = {}, ds = {} }
      groups[name] = group
      order[#order + 1] = name
    end
    return group
  end

  local function flush_entities()
    if pending_count == 0 then
      return
    end
    local parts = {}
    for i = 1, #order do
      local name = order[i]
      local group = groups[name]
      parts[i] = encode.frame_entity_run(dict, name, group.w, group.h, group.xs, group.ys, group.ds, group.n)
    end
    checksum = M.checksummed_write(path, table.concat(parts), true, checksum)
    order, groups, pending_count = {}, {}, 0
  end

  -- Unbounded, because a factory reaches wherever somebody took it: an
  -- outpost thousands of tiles out still belongs in the timelapse. Scenery is
  -- the opposite case, handled by the bounded pass below.
  for _, entity in pairs(surface.find_entities_filtered({
    type = unbounded_excludes(),
    invert = true,
  })) do
    if entity.valid then
      -- Each field crosses the mod/game boundary, so each is read exactly
      -- once, straight into the run being built.
      local pos = entity.position
      local name = entity.name
      local group = group_for(name, entity.tile_width, entity.tile_height)
      local k = group.n + 1
      group.n, group.xs[k], group.ys[k], group.ds[k] = k, pos.x, pos.y, entity.direction
      written = written + 1
      pending_count = pending_count + 1

      local built = player_built[name]
      if built == nil then
        built = is_player_built(entity)
        player_built[name] = built
      end
      if built then
        encode.grow_bbox(bbox, pos.x, pos.y)
      end

      -- Each write_file call is a separate file append, so flushing per entity
      -- would make export time track syscalls rather than entity count.
      if pending_count >= FLUSH_EVERY then
        flush_entities()
      end
    end
  end

  flush_entities()

  -- One area for the scenery pass and the ground pass both: the two describing
  -- the same region is the point, a tree drawn outside the ground it grows on
  -- being what this fixes. From entities alone, having to be final before the
  -- scenery pass writes while placed floor is not read until the tile section.
  -- Everything so far came from the unbounded pass, which is exactly what
  -- somebody built. The scenery pass below adds trees, nests and ore, so this
  -- is the last moment the two are separable without classifying names.
  local built = written

  local context_area = encode.expand_bbox(bbox, encode.terrain_margin(bbox, TERRAIN_MARGIN_TILES))

  if context_area then
    for _, entity in pairs(surface.find_entities_filtered({
      area = context_area,
      type = M.context_types(),
    })) do
      if entity.valid then
        local pos = entity.position
        local group = group_for(entity.name, entity.tile_width, entity.tile_height)
        local k = group.n + 1
        group.n, group.xs[k], group.ys[k], group.ds[k] = k, pos.x, pos.y, entity.direction
        written = written + 1
        pending_count = pending_count + 1

        if pending_count >= FLUSH_EVERY then
          flush_entities()
        end
      end
    end

    flush_entities()
  end

  checksum = M.checksummed_write(path, encode.frame_end_entities(), true, checksum)

  local tiles_written = 0

  -- Same batching as the entity section above, for the same reason.
  local function tile_group_for(name)
    local group = groups[name]
    if not group then
      group = { n = 0, xs = {}, ys = {} }
      groups[name] = group
      order[#order + 1] = name
    end
    return group
  end

  local function flush_tiles()
    if pending_count == 0 then
      return
    end
    local parts = {}
    for i = 1, #order do
      local name = order[i]
      local group = groups[name]
      parts[i] = encode.frame_tile_run(dict, name, group.xs, group.ys, group.n)
    end
    checksum = M.checksummed_write(path, table.concat(parts), true, checksum)
    order, groups, pending_count = {}, {}, 0
  end

  for _, tile in pairs(surface.find_tiles_filtered({ name = encode.placed_floor_tiles() })) do
    local pos = tile.position
    local group = tile_group_for(tile.name)
    local k = group.n + 1
    group.n, group.xs[k], group.ys[k] = k, pos.x, pos.y
    tiles_written = tiles_written + 1
    pending_count = pending_count + 1

    if pending_count >= FLUSH_EVERY then
      flush_tiles()
    end
  end

  flush_tiles()

  -- No natural ground here, deliberately: it is the only part of a capture
  -- that does not change, so putting it in a frame meant paying for it once
  -- per frame, and inside somebody's game during a live baseline, to describe
  -- something identical every time. `M.export_terrain` writes it once.

  -- Not itself folded into the checksum: nothing needs a checksum of the
  -- checksum, and the reader already knows the trailer's fixed size.
  M.safe_write_file(path, encode.u32le(checksum), true)

  return written, tiles_written, built
end

--- Natural ground over `area` as a `terrain_<surface>.stfr`: a frame file
--- with an empty entity section and nothing but tiles after it. Placed floor
--- is excluded, the frames already carrying it with the history of when each
--- piece went down.
local function export_terrain_to(tick, session_id, surface, area)
  local path = M.EXPORT_DIR .. encode.terrain_name(session_id, surface.name)
  local dict = encode.new_dictionary()

  local checksum = encode.checksum_init()
  checksum = M.checksummed_write(path, encode.frame_header(tick, surface.name), false, checksum)
  checksum = M.checksummed_write(path, encode.frame_end_entities(), true, checksum)

  local order, groups, pending_count, written = {}, {}, 0, 0

  local function flush()
    if pending_count == 0 then
      return
    end
    local parts = {}
    for i = 1, #order do
      local name = order[i]
      local group = groups[name]
      parts[i] = encode.frame_tile_run(dict, name, group.xs, group.ys, group.n)
    end
    checksum = M.checksummed_write(path, table.concat(parts), true, checksum)
    order, groups, pending_count = {}, {}, 0
  end

  for _, tile in pairs(surface.find_tiles_filtered({
    area = area,
    name = encode.placed_floor_tiles(),
    invert = true,
  })) do
    local pos = tile.position
    local group = groups[tile.name]
    if not group then
      group = { n = 0, xs = {}, ys = {} }
      groups[tile.name] = group
      order[#order + 1] = tile.name
    end
    local k = group.n + 1
    group.n, group.xs[k], group.ys[k] = k, pos.x, pos.y
    written = written + 1
    pending_count = pending_count + 1

    if pending_count >= FLUSH_EVERY then
      flush()
    end
  end

  flush()
  M.safe_write_file(path, encode.u32le(checksum), true)
  return written
end

--- The area a surface's ground should cover: a margin around everything the
--- player force owns on it, or `nil` if they own nothing. Asks for player-force
--- entities directly rather than reusing `export_surface`'s scan, affordable
--- only because this runs unattended.
---
--- Reaches further than the scenery box, the two being measured at different
--- moments: on a real capture scenery overhung ground by 33 tiles a side. The
--- viewer clips scenery to the ground, but clipping throws away trees somebody
--- paid to capture.
local TERRAIN_SCAN_OVERSHOOT = 2.0

local function terrain_area_for(surface)
  local bbox = encode.new_bbox()
  for _, entity in pairs(surface.find_entities_filtered({ force = "player" })) do
    if entity.valid then
      local pos = entity.position
      encode.grow_bbox(bbox, pos.x, pos.y)
    end
  end
  local margin = encode.terrain_margin(bbox, TERRAIN_MARGIN_TILES) * TERRAIN_SCAN_OVERSHOOT
  return encode.expand_bbox(bbox, math.floor(margin))
end

--- Write every inhabited surface's natural ground, one file each.
---
--- Runs unattended against one save, after the playthrough it describes, which
--- buys three things: no ground cost inside anybody's game, no ground repeated
--- per frame, and an area chosen knowing how far the factory reached.
---
--- What it gives up is ground since built over: the query asks for everything
--- that is not placed floor, so water landfilled at hour three reads as
--- landfill at hour ten.
function M.export_terrain(tick, session_id)
  local written, surfaces = 0, 0
  for _, surface in pairs(game.surfaces) do
    if M.is_inhabited(surface) then
      local area = terrain_area_for(surface)
      if area then
        local count = export_terrain_to(tick, session_id, surface, area)
        if count > 0 then
          written = written + count
          surfaces = surfaces + 1
        end
      end
    end
  end
  return written, surfaces
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

--- Everything this save can say about milestones, for the from-saves path.
---
--- Lives here rather than in milestones.lua because that module already
--- requires this one and Lua handles a require cycle badly. The two do
--- different jobs: milestones.lua watches transitions during live play, this
--- snapshots totals for a save with no history.
---
--- A planet counts as reached when its surface is inhabited rather than when it
--- exists, the game creating it first, which keeps the marker honest against
--- the timelapse. Every read is `pcall`'d, a failing statistics call costing
--- one marker rather than the export.
function M.milestone_state()
  local science, planets, rockets = {}, {}, 0
  local force = game.forces["player"]

  if force then
    -- Statistics are per surface in 2.0, so this unions across them: a pack
    -- first assembled on Vulcanus still counts as produced.
    local seen = {}
    for _, surface in pairs(game.surfaces) do
      local ok, counts = pcall(function()
        return force.get_item_production_statistics(surface).input_counts
      end)
      if ok and counts then
        for name, count in pairs(counts) do
          if count > 0 and encode.is_science_pack(name) and not seen[name] then
            seen[name] = true
            science[#science + 1] = name
          end
        end
      end
    end

    local ok, launched = pcall(function() return force.rockets_launched end)
    if ok and launched then
      rockets = launched
    end
  end

  for _, surface in pairs(game.surfaces) do
    local ok, name = pcall(function()
      return surface.planet and M.is_inhabited(surface) and surface.name
    end)
    if ok and name then
      planets[#planets + 1] = name
    end
  end

  -- Sorted so a manifest is stable between runs of the same save, which
  -- keeps a diff of two saves free of spurious ordering churn.
  table.sort(science)
  table.sort(planets)
  return science, planets, rockets
end

--- Shared by the synchronous export and the periodic test-snapshot timer,
--- both describing "everything exported at this tick" in the same shape.
function M.periodic_manifest_path(tick)
  return string.format("%sframe_%d_manifest.json", M.EXPORT_DIR, tick)
end

-- Player position tracking
--
-- A newline-delimited JSON log rather than one of the binary formats above, a
-- sample happening at most every few seconds. The same shape the viewer reads,
-- so save-timelapse.exe relocates the file rather than converting it.

--- Untagged for /timelapse-export and headless scan, tagged by session_id
--- for live capture, exactly like `baseline_manifest_path` (capture.lua).
local function player_log_path(session_id)
  if not session_id then
    return M.EXPORT_DIR .. "players.jsonl"
  end
  return M.EXPORT_DIR .. encode.player_log_name(session_id)
end

--- Untagged or session-tagged exactly like the player log above.
local function prototypes_path(session_id)
  if not session_id then
    return M.EXPORT_DIR .. "prototypes.json"
  end
  return M.EXPORT_DIR .. encode.prototypes_name(session_id)
end

--- Writes what this game's prototypes are. Overwrites rather than appends,
--- which is how a capture picks up mods added since it started. `pcall`'d
--- because a description that failed to write must never take a capture down:
--- the desktop side falls back to its own built-in names.
function M.write_prototypes(session_id)
  pcall(function()
    M.safe_write_file(prototypes_path(session_id), encode.prototypes_json(), false)
  end)
end

--- Periodic, for live capture: only players connected right now. Wrapped in
--- `pcall` per player, so one with no valid position is skipped.
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

--- One-shot, for /timelapse-export, the headless scan and the baseline. Reads
--- `game.players` rather than `game.connected_players`, which is empty in
--- headless mode. A player whose character does not exist is skipped.
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

--- Exports exactly `surface_names`, already filtered by the caller, and writes
--- no manifest: `baseline.json` describes the original once-per-session
--- baseline, and a catch-up covers a different surface at a later tick, so
--- overwriting it would corrupt its meaning for every surface it covers. Rust
--- finds a catch-up by spotting a frame file the manifest does not account
--- for. Trusts `surface_names` outright, the caller having computed that gap in
--- this same tick.
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

--- Every surface, synchronously, in whatever tick this is called from. Serves
--- /timelapse-export, the headless scan and the once-per-save baseline, which
--- differ only in the manifest path, session tagging, and `is_excluded`.
---
--- `is_excluded` skips a surface entirely: one opted out of recording should
--- not pay the baseline's cost either, that being the most expensive part of
--- capture and the reason exclusion exists.
function M.export_all_to(tick, manifest_path, session_id, is_excluded)
  sample_all_players(tick, session_id)
  -- One choke point for both paths that produce frames, the live baseline and
  -- the headless save export, so neither can end up undescribed.
  M.write_prototypes(session_id)
  local names, total, tile_total, built_total = {}, 0, 0, 0

  for _, surface in pairs(game.surfaces) do
    if M.is_inhabited(surface) and not (is_excluded and is_excluded(surface.name)) then
      local entities, tiles, built = export_surface(surface, tick, session_id)
      total = total + entities
      tile_total = tile_total + tiles
      built_total = built_total + built
      names[#names + 1] = encode.quote(surface.name)
    end
  end

  -- Milestone state rides in the manifest rather than a file of its own: it
  -- describes the same instant, and every consumer wanting one wants the
  -- other. Being JSON, an older reader ignores the field.
  local science, planets, rockets = M.milestone_state()

  helpers.write_file(
    manifest_path,
    -- `buildings` counts only the unbounded pass, `entities` everything
    -- written. A reader older than this field falls back to `entities`, which
    -- is what it always showed.
    string.format('{"tick":%d,"entities":%d,"buildings":%d,"tiles":%d,"surfaces":[%s],"milestones":%s}',
      tick, total, built_total, tile_total, table.concat(names, ","),
      encode.milestone_state(science, planets, rockets)),
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

--- Runs the headless scan if the startup setting asked for one and this is
--- the first tick since load; no-ops otherwise. Called unconditionally from
--- control.lua's on_tick.
function M.run_pending_tick_work(tick, session_id_fn)
  if headless_scan_pending then
    export_all(tick)
    headless_scan_pending = false
  end
  if terrain_scan_pending then
    -- Asked for only now, and only if a scan is actually due: it reads
    -- nauvis's map settings, which is not work to repeat on every tick of
    -- every game just so this call site can read tidily.
    local tiles, surfaces = M.export_terrain(tick, session_id_fn and session_id_fn() or nil)
    log(string.format("[save-timelapse] terrain scan wrote %d tiles across %d surface(s)", tiles, surfaces))
    terrain_scan_pending = false
  end
end

return M
