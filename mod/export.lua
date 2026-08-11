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
--- The *least* ground to capture past the built area, so the factory reads
--- as sitting on real land rather than stopping at a hard edge. Roughly a
--- chunk; not exposed as a setting since nothing has asked for this to be
--- tunable yet.
---
--- A floor rather than the whole answer: `encode.terrain_margin` widens it
--- to cover what a fitted 16:9 frame actually shows around a base this
--- shape, which on anything large is far more than a chunk. This value is
--- what a base small enough for that not to matter still gets.
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

--- Set the same way, for the ground-only pass. Separate from the scan above
--- rather than a mode of it because the two run against different saves: the
--- frames come from every save in the set, the ground from one.
local terrain_scan_pending = false
if settings.startup["save-timelapse-terrain-scan"].value then
  terrain_scan_pending = true
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
    -- Gleba's flora is type "plant", not "tree": yumako trees, jellystem and
    -- the rest. Without this line, turning terrain capture off silenced
    -- Nauvis's forests while Gleba's kept being recorded, which is both
    -- surprising and the more expensive half on a planet that is mostly
    -- wilderness.
    list[#list + 1] = "plant"
  end
  return list
end

--- Scenery: entities the map generated rather than anybody placing, which
--- therefore sit on every generated chunk regardless of where the factory
--- is. Recorded, but only near the factory, exactly like the natural ground
--- they stand on.
---
--- These used to come from the whole surface while the ground was capped to
--- a margin around the base, which is the same reasoning applied to one of
--- them and not the other. Measured on a real megabase capture, trees,
--- resources and nests were **69% of every frame** and covered an area 2.3x
--- larger than the ground beneath them, so most of them rendered on empty
--- black. Worse, Factorio generates chunks ahead of the player, so a capture
--- included forests and ore fields from chunks nobody had ever visited.
---
--- Worms are missing from this list and cannot join it: they share the
--- "turret" type with player turrets (see `encode.EXCLUDED_TYPES`), so
--- bounding by type would bound real defences too. They were 0.9% of that
--- same capture alongside the nests, so the leak is small.
---
--- Disjoint from `M.excluded_types()` by construction: each entry is gated
--- on the setting that would otherwise have excluded it outright, so no type
--- is ever named by both. The list itself lives in `encode.lua`, which has
--- no Factorio dependency and so can be unit tested; this only reads the
--- settings for it.
function M.context_types()
  return encode.context_types(
    settings.startup["save-timelapse-include-resources"].value,
    settings.startup["save-timelapse-capture-terrain"].value
  )
end

--- What the unbounded entity pass skips: everything never recorded at all,
--- plus the scenery the bounded pass handles instead.
---
--- Not folded into `M.excluded_types()`, which `capture.lua` uses to decide
--- what a live event may log: scenery is genuinely recorded, so an event
--- touching it is not something to drop.
local function unbounded_excludes()
  local list = M.excluded_types()
  for _, t in pairs(M.context_types()) do
    list[#list + 1] = t
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

--- Whether somebody built `entity`, as opposed to the map having generated
--- it. `"player"` is the same force name `M.is_inhabited` and
--- `M.milestone_state` already treat as "the player's" everywhere else in
--- this file.
local function is_player_built(entity)
  local force = entity.force
  return force ~= nil and force.name == "player"
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
  --
  -- Only what somebody actually built grows it. Everything else exported
  -- here sits wherever the map generated it: with terrain capture on that
  -- is every tree on every generated chunk, and enemy nests and worms are
  -- there in every direction regardless of the setting. Letting those in
  -- made this "the explored map" rather than "the factory", which is not a
  -- margin around anything, and then multiplied the terrain pass below by
  -- the whole difference.
  local bbox = encode.new_bbox()

  -- Which prototype names somebody built, asked once per distinct name
  -- instead of once per entity. `entity.force.name` is two crossings of the
  -- mod/game boundary and this loop runs per entity on bases that reach
  -- hundreds of thousands of them, so the difference is a few dozen
  -- questions against ~900k.
  --
  -- Per name does mean a prototype standing on two forces at once is judged
  -- by whichever was seen first. That is fine for what this feeds: the box
  -- only decides how far past the factory to capture ground, so being one
  -- building wrong at its edge changes nothing anybody can see.
  local player_built = {}

  local pending_count, written = 0, 0

  -- Records are grouped by name into runs (see encode.frame_entity_run), so
  -- entities are collected by name as they are scanned and written out when
  -- the batch fills.
  --
  -- Grouped per batch and not per frame deliberately: buffering a whole
  -- megabase to group it would reintroduce exactly the stall the incremental
  -- exporter exists to avoid. A batch of FLUSH_EVERY across the few dozen
  -- distinct names on a surface still leaves runs long enough for the name id
  -- and count to amortize, which is where the saving is.
  --
  -- Straight into parallel flat arrays, never a table per entity: measured at
  -- 900k entities, a table each made encoding 1.26x slower than the format
  -- this replaced, against 0.60x this way.
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

  -- Unbounded, because a factory reaches wherever somebody took it: a mining
  -- outpost or a rail terminus thousands of tiles out is still theirs and
  -- still belongs in the timelapse. Scenery is the opposite case and is
  -- handled by the bounded pass below.
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

  -- One area for the scenery pass and the natural ground pass both, computed
  -- once here from what somebody built. The two describing the same region is
  -- the whole point: a tree drawn outside the ground it grows on is what this
  -- fixes.
  --
  -- Taken from entities alone, unlike before, because it has to be final
  -- before the scenery pass writes and placed floor is not read until the
  -- tile section further down. Measured on a real megabase the paving sat
  -- inside the entity box anyway (2873x2863 against 3070x3113), and
  -- `encode.terrain_margin` adds far more slack than the difference, so
  -- nothing is lost that the margin does not already cover.
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

  for _, tile in pairs(surface.find_tiles_filtered({ name = encode.PLACED_FLOOR_TILES })) do
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

  -- No natural ground here, deliberately, and this is the one place it would
  -- obviously belong. Ground is the only part of a capture that does not
  -- change: entities need a record per placement, tiles you lay down need
  -- one, but grass is grass for the whole playthrough. Putting it in a frame
  -- meant paying for it once per frame in a from-saves export, and paying for
  -- it inside somebody's game during a live baseline, to describe something
  -- that was the same every time.
  --
  -- `M.export_terrain` writes it once instead, from an unattended run against
  -- a single save. See its comment for what that costs and what it gives up.

  -- Not itself folded into the checksum: nothing needs a checksum of the
  -- checksum, and the reader already knows the trailer's fixed size.
  M.safe_write_file(path, encode.u32le(checksum), true)

  return written, tiles_written
end

--- Natural ground over `area` as a `terrain_<surface>.stfr`: a frame file
--- with an empty entity section and nothing but tiles after it.
---
--- Placed floor is excluded because the frames already carry it, with the
--- history of when each piece went down that this file cannot express.
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
    name = encode.PLACED_FLOOR_TILES,
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
--- player force owns on it, or `nil` if they own nothing.
---
--- Asks for player-force entities directly rather than reusing
--- `export_surface`'s scan, which is only affordable because this runs
--- unattended: nothing here has a frame rate to protect, so bringing robots
--- and characters across the API boundary costs time nobody is waiting on.
--- They inflate the box by a rounding error and are not worth filtering.
--- How much further the ground reaches than anything that stands on it.
---
--- Scenery is recorded into the frames while playing, from a box measured
--- then; ground is scanned later, from a box measured then. Two boxes from
--- two moments never agree exactly, and on a real capture the scenery
--- overhung the ground by 33 tiles on every side. The viewer now clips
--- scenery to whatever ground exists, so the edge is exact either way, but
--- clipping throws away trees somebody paid to capture. Reaching further
--- means there is usually nothing to throw away.
---
--- Affordable now in a way it never was before: ground used to be written
--- into every frame, so widening it multiplied by the frame count. It is one
--- file per surface now, so a wider margin costs once.
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

--- Write every inhabited surface's natural ground, one
--- `terrain_<surface>.stfr` each, and nothing else.
---
--- Runs unattended, against one save, after the playthrough it describes.
--- That ordering is the whole point and buys three things at once: no ground
--- cost inside anybody's game, no ground repeated in every frame of a
--- from-saves export, and an area chosen knowing how far the factory
--- eventually reached rather than guessing from how far it had reached when
--- recording started.
---
--- What it gives up is ground that has since been built over. The query asks
--- for everything that is not placed floor, so water somebody landfilled at
--- hour three reads as landfill at hour ten and its water is never recorded.
--- Replayed from the beginning that lake is a hole until the tick the
--- landfill was placed. A pass during the baseline would have caught it while
--- it was still visible, which is the trade being made here.
---
--- Named `terrain_<surface>.stfr` because that is what the viewer already
--- looks for, discovered straight from the directory and independent of the
--- frame files, so nothing downstream needed changing to accept ground from
--- a different producer.
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

--- Everything this save can say about milestones, for the from-saves path:
--- which science packs have ever been produced, which planets have been
--- reached, and how many rockets have launched.
---
--- Lives here rather than in milestones.lua, which is the obvious home,
--- because that module already requires this one (for `EXPORT_DIR`) and Lua
--- handles a require cycle badly. The two are doing different jobs anyway:
--- milestones.lua watches for transitions during live play, while this
--- snapshots totals for a save that has no history to watch.
---
--- "Planets reached" is a planet surface that is *inhabited*, not merely one
--- that exists, since the game creates a planet's surface before anybody goes
--- there. Reusing `is_inhabited` also keeps the marker honest against the
--- timelapse it annotates: a surface only appears in frames once it is
--- inhabited, so a planet is marked reached exactly when it starts being
--- shown.
---
--- Every read is `pcall`'d like the rest of this file: a statistics call
--- failing should cost one marker, never the whole export.
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

--- Untagged or session-tagged exactly like the player log above.
local function palette_path(session_id)
  if not session_id then
    return M.EXPORT_DIR .. "palette.json"
  end
  return M.EXPORT_DIR .. encode.palette_name(session_id)
end

--- Writes the prototype colour table, once, beside everything else a capture
--- produces.
---
--- Overwrites rather than appends: it is a snapshot of what this game's
--- prototypes are, and rewriting it on a later run is how a capture picks up
--- colours for mods added since it started. `pcall` because it is a nicety, and
--- a colour table that failed to write must never take a capture down with it:
--- the desktop side falls back to its own palette when this is missing.
function M.write_palette(session_id)
  pcall(function()
    M.safe_write_file(palette_path(session_id), encode.palette_json(), false)
  end)
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
  -- One choke point for both paths that produce frames, the live baseline and
  -- the headless save export, so neither can end up without a palette.
  M.write_palette(session_id)
  local names, total, tile_total = {}, 0, 0

  for _, surface in pairs(game.surfaces) do
    if M.is_inhabited(surface) and not (is_excluded and is_excluded(surface.name)) then
      local entities, tiles = export_surface(surface, tick, session_id)
      total = total + entities
      tile_total = tile_total + tiles
      names[#names + 1] = encode.quote(surface.name)
    end
  end

  -- Milestone state rides in the manifest rather than in a file of its own:
  -- it describes the same instant the manifest already describes, and every
  -- consumer that wants one wants the other. Being JSON, an older reader
  -- simply ignores the field, which is why this can be added to a file
  -- `baseline.json` also uses without disturbing live capture.
  local science, planets, rockets = M.milestone_state()

  helpers.write_file(
    manifest_path,
    string.format('{"tick":%d,"entities":%d,"tiles":%d,"surfaces":[%s],"milestones":%s}',
      tick, total, tile_total, table.concat(names, ","),
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

--- Runs the headless scan if the startup setting requested one, and this is
--- the first tick since load; no-ops every other tick. Called unconditionally
--- from control.lua's on_tick, alongside capture.lua's and snapshot.lua's
--- own pending-work checks.
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
