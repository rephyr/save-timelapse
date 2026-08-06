-- save-timelapse: pure JSON-encoding helpers shared by snapshot export
-- (control.lua) and live event capture. Nothing here touches
-- game/surface/settings/helpers, so this file loads and runs under a plain
-- `lua` interpreter with no Factorio present. See tests/encode_test.lua.

local M = {}

-- Types with no bearing on how a factory grew. Passed to
-- find_entities_filtered with invert, so these never cross the API boundary.
M.EXCLUDED_TYPES = {
  -- actors and their remains
  "character", "corpse", "combat-robot", "fish",
  -- terrain scatter
  "tree", "simple-entity", "simple-entity-with-force", "simple-entity-with-owner",
  "cliff",
  -- transient visual effects
  "particle-source", "projectile", "explosion", "fire", "smoke",
  "smoke-with-trigger", "stream", "sticker", "beam",
  -- not yet real, or lying on the floor
  "entity-ghost", "tile-ghost", "item-entity",
}

-- Natural terrain (grass, water, sand, dirt, ...) vastly outnumbers placed
-- floor types, so this is an include list rather than an exclude list, the
-- opposite of EXCLUDED_TYPES above. Verified against base/prototypes/tile/tiles.lua.
M.PLACED_FLOOR_TILES = {
  "stone-path", "concrete",
  "hazard-concrete-left", "hazard-concrete-right",
  "refined-concrete", "refined-hazard-concrete-left", "refined-hazard-concrete-right",
  "landfill",
  -- Space Age colored refined-concrete variants
  "red-refined-concrete", "green-refined-concrete", "blue-refined-concrete",
  "orange-refined-concrete", "yellow-refined-concrete", "pink-refined-concrete",
  "purple-refined-concrete", "black-refined-concrete", "brown-refined-concrete",
  "cyan-refined-concrete", "acid-refined-concrete",
}

function M.quote(text)
  return '"' .. text:gsub('[\\"]', '\\%0') .. '"'
end

function M.encode_entity(entity)
  local pos = entity.position
  local fields = string.format('{"n":%s,"x":%.1f,"y":%.1f',
    M.quote(entity.name), pos.x, pos.y)
  local facing = entity.direction
  if facing and facing ~= 0 then
    fields = fields .. ',"d":' .. facing
  end
  -- Most entities are 1x1, so this omits w/h the same way d is omitted at 0
  -- -- otherwise every belt and pole would carry two redundant fields.
  local w, h = entity.tile_width, entity.tile_height
  if w and h and (w ~= 1 or h ~= 1) then
    fields = fields .. string.format(',"w":%d,"h":%d', w, h)
  end
  return fields .. "}"
end

--- Tiles are corner positioned and integer aligned, unlike entities.
function M.encode_tile(tile)
  local pos = tile.position
  return string.format('{"n":%s,"x":%d,"y":%d}', M.quote(tile.name), pos.x, pos.y)
end

--- One live-capture log line. `op` is "+" (add) or "-" (remove); `kind` is
--- "e" (entity) or "t" (tile). On remove, `id` (an entity's unit_number) is
--- preferred when available: just tick/op/kind/id, nothing else needed.
--- Otherwise x/y locate whatever currently occupies that position -- the
--- only option for tiles, which have no identity beyond their position.
--- `w`/`h` (entity footprint, add events only) follow the same omit-at-1x1
--- convention as encode_entity.
function M.encode_event(op, kind, tick, name, x, y, direction, id, w, h)
  if op == "-" and id then
    return string.format('{"t":%d,"op":"-","k":%s,"id":%d}', tick, M.quote(kind), id)
  end

  local coord_fmt = kind == "t" and '"x":%d,"y":%d' or '"x":%.1f,"y":%.1f'
  local fields = string.format('{"t":%d,"op":%s,"k":%s,', tick, M.quote(op), M.quote(kind))

  if name then
    fields = fields .. '"n":' .. M.quote(name) .. ","
  end

  fields = fields .. string.format(coord_fmt, x, y)

  if op == "+" and kind == "e" then
    if direction and direction ~= 0 then
      fields = fields .. ',"d":' .. direction
    end
    if w and h and (w ~= 1 or h ~= 1) then
      fields = fields .. string.format(',"w":%d,"h":%d', w, h)
    end
    if id then
      fields = fields .. ',"id":' .. id
    end
  end

  return fields .. "}"
end

--- Given the tick already recorded up to and the tick play resumed at,
--- decide whether this is a reload of an older save -- something an
--- append-only log can't represent as one timeline -- and if so, what
--- segment to start. Pure: plain values in and out, no Factorio state, so
--- the decision is testable without a save/load cycle to actually trigger.
function M.next_capture_segment(last_tick, resumed_tick, current_segment_start)
  if resumed_tick < last_tick then
    return resumed_tick -- rolled back: start a fresh segment here
  end
  return current_segment_start -- keep appending to the existing segment
end

return M
