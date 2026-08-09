-- save-timelapse: the periodic incremental test-snapshot debug feature,
-- independent of both one-shot export (export.lua) and live capture
-- (capture.lua). Exercises the export path during real play, spread over
-- many ticks so no single tick stalls, unlike a synchronous export.

local encode = require("encode")
local export = require("export")

local M = {}

--- Work items encoded per tick while the periodic test-snapshot runs, and how
--- many encoded strings accumulate before a file append. Deliberately small:
--- the point of spreading this export over ticks is that no single tick
--- stalls, and a big batch gives that back. The baseline does not use this,
--- see export.export_all_to, since it runs at most once per save and a
--- one-time freeze was judged worth it there to avoid a background cost
--- smeared across the next several minutes of play. Separate from
--- export.lua's own flush-batch size, which serves every synchronous export
--- path (`/timelapse-export`, headless scan, baseline) where syscall count,
--- not smoothness, is the cost.
local SNAPSHOT_BATCH_SIZE = 64
local SNAPSHOT_FLUSH_EVERY = 128

local snapshot_state = nil

local function snapshot_path(tick, surface_name)
  return string.format("%sframe_%d_%s.stfr", export.EXPORT_DIR, tick, surface_name)
end

--- Writes whatever has been grouped so far as runs.
---
--- Grouping happens as items are scanned, into the parallel flat arrays this
--- encodes, rather than into a table per entity: measured at 900k entities
--- under real Lua 5.2, a table each made encoding 1.26x slower than the
--- per-entity format this replaced, against 0.60x this way.
---
--- Grouped per flush rather than over the whole snapshot, which is the point
--- of this exporter: it spreads one export across many ticks so no single
--- tick does the lot, and buffering a megabase to group it would put that
--- stall straight back. A flush's worth across the few dozen distinct names
--- on a surface still leaves runs long enough for the name id and count to
--- amortize.
---
--- `state.phase` says which section is being written. Both final flushes
--- happen before the phase advances, so this is never asked to guess.
local function snapshot_flush(state)
  if state.pending_count == 0 then
    return
  end

  local parts = {}
  for i = 1, #state.order do
    local name = state.order[i]
    local g = state.groups[name]
    if state.phase == "tiles" then
      parts[i] = encode.frame_tile_run(state.dict, name, g.xs, g.ys, g.n)
    else
      parts[i] = encode.frame_entity_run(state.dict, name, g.w, g.h, g.xs, g.ys, g.ds, g.n)
    end
  end

  state.checksum = export.checksummed_write(state.path, table.concat(parts), true, state.checksum)
  state.order, state.groups, state.pending_count = {}, {}, 0
end

--- The run `name` is being collected into, created on first sight. `w`/`h`
--- are nil for tiles, which are always one by one.
local function snapshot_group(state, name, w, h)
  local g = state.groups[name]
  if not g then
    g = { w = w, h = h, n = 0, xs = {}, ys = {}, ds = {} }
    state.groups[name] = g
    state.order[#state.order + 1] = name
  end
  return g
end

local function snapshot_begin_surface(state, surface)
  state.path = snapshot_path(state.tick, surface.name)
  state.surface_name = surface.name
  state.dict = encode.new_dictionary()
  state.entities = surface.find_entities_filtered({
    type = export.excluded_types(),
    invert = true,
  })
  state.entity_index = 1
  state.written = 0
  state.tiles = nil
  state.tile_index = 1
  state.tiles_written = 0
  state.phase = "entities"
  state.checksum = encode.checksum_init()
  state.checksum = export.checksummed_write(state.path, encode.frame_header(state.tick, surface.name), false, state.checksum)
end

--- Runs when a snapshot finishes: writes its manifest. Written last, so its
--- presence is what tells a reader the snapshot is whole rather than still
--- in progress. Only this periodic timer goes through this path, the
--- baseline runs synchronously via export.export_all_to, so the manifest is
--- always this periodic shape.
local function snapshot_finish(s)
  local quoted = {}
  for i, name in pairs(s.surface_names) do
    quoted[i] = encode.quote(name)
  end

  helpers.write_file(export.periodic_manifest_path(s.tick), string.format(
    '{"tick":%d,"entities":%d,"tiles":%d,"surfaces":[%s]}',
    s.tick, s.total_entities, s.total_tiles, table.concat(quoted, ",")), false)
end

--- One tick's worth of export. Driven by `M.run_pending_tick_work` rather
--- than by a handler this function registers itself: Factorio keeps a
--- single handler per event, so registering one here would silently
--- displace another feature wanting on_tick.
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
        -- Each field crosses the API boundary, so each is read exactly once,
        -- straight into the run being built.
        local pos = entity.position
        local g = snapshot_group(s, entity.name, entity.tile_width, entity.tile_height)
        local k = g.n + 1
        g.n, g.xs[k], g.ys[k], g.ds[k] = k, pos.x, pos.y, entity.direction
        if s.pending_count >= SNAPSHOT_FLUSH_EVERY then
          snapshot_flush(s)
        end
      end
    end
    s.entity_index = end_index + 1
    if s.entity_index > #s.entities then
      snapshot_flush(s)
      s.checksum = export.checksummed_write(s.path, encode.frame_end_entities(), true, s.checksum)
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
      local pos = tile.position
      local g = snapshot_group(s, tile.name)
      local k = g.n + 1
      g.n, g.xs[k], g.ys[k] = k, pos.x, pos.y
      if s.pending_count >= SNAPSHOT_FLUSH_EVERY then
        snapshot_flush(s)
      end
    end
    s.tile_index = end_index + 1
    if s.tile_index > #s.tiles then
      snapshot_flush(s)
      -- Not itself folded into the checksum, same as export_surface's
      -- trailer: nothing needs a checksum of the checksum.
      export.safe_write_file(s.path, encode.u32le(s.checksum), true)
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

--- Start an incremental, multi-tick export. Single-purpose rather than a
--- generic function the baseline also shares (see export.export_all_to for
--- why it no longer does): a stray old reference here would be the kind of
--- thing that quietly drifts back out of sync.
function M.start(tick)
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
    order = {},
    groups = {},
    pending_count = 0,
    path = nil,
    surface_name = nil,
    phase = nil,
    dict = nil,
  }

  for _, surface in pairs(game.surfaces) do
    if export.is_inhabited(surface) then
      state.surface_names[#state.surface_names + 1] = surface.name
    end
  end

  if #state.surface_names == 0 then
    return
  end

  snapshot_state = state
end

--- Runs one step of the incremental snapshot if one is in progress; no-ops
--- otherwise. Called unconditionally from control.lua's on_tick, alongside
--- export.lua's and capture.lua's own pending-work checks.
function M.run_pending_tick_work(tick)
  if snapshot_state then
    snapshot_step()
  end
end

return M
