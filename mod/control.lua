-- save-timelapse
-- Exports a surface's entities as JSON so an external renderer can build a
-- timelapse from save files.
--
-- Triggered either by the /timelapse-export command or, for unattended runs,
-- by the save-timelapse-headless-scan startup setting.

local EXPORT_DIR = "save-timelapse/"
local FLUSH_EVERY = 2000

-- Types with no bearing on how a factory grew. Passed to
-- find_entities_filtered with invert, so these never cross the API boundary.
local EXCLUDED_TYPES = {
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

local function excluded_types()
  if settings.startup["save-timelapse-include-resources"].value then
    return EXCLUDED_TYPES
  end
  local list = { "resource" }
  for _, t in pairs(EXCLUDED_TYPES) do
    list[#list + 1] = t
  end
  return list
end

local function quote(text)
  return '"' .. text:gsub('[\\"]', '\\%0') .. '"'
end

local function encode_entity(entity)
  local pos = entity.position
  local fields = string.format('{"n":%s,"x":%.1f,"y":%.1f',
    quote(entity.name), pos.x, pos.y)
  local facing = entity.direction
  if facing and facing ~= 0 then
    fields = fields .. ',"d":' .. facing
  end
  return fields .. "}"
end

--- Write one surface to its own file. Returns how many entities were written.
local function export_surface(surface, tick)
  local path = string.format("%sframe_%d_%s.json", EXPORT_DIR, tick, surface.name)

  helpers.write_file(path, string.format('{"tick":%d,"surface":%s,"entities":[',
    tick, quote(surface.name)), false)

  local pending, pending_count, written = {}, 0, 0

  for _, entity in pairs(surface.find_entities_filtered({
    type = excluded_types(),
    invert = true,
  })) do
    if entity.valid then
      pending_count = pending_count + 1
      written = written + 1
      pending[pending_count] = (written > 1 and "," or "") .. encode_entity(entity)

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

  helpers.write_file(path, string.format('],"count":%d}', written), true)
  return written
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
  local names, total = {}, 0

  for _, surface in pairs(game.surfaces) do
    if is_inhabited(surface) then
      total = total + export_surface(surface, tick)
      names[#names + 1] = quote(surface.name)
    end
  end

  helpers.write_file(
    string.format("%sframe_%d_manifest.json", EXPORT_DIR, tick),
    string.format('{"tick":%d,"entities":%d,"surfaces":[%s]}',
      tick, total, table.concat(names, ",")),
    false)

  return total, #names
end

commands.add_command("timelapse-export",
  "Export this save's entities for timelapse rendering.",
  function(event)
    local total, surfaces = export_all(game.tick)
    local player = event.player_index and game.get_player(event.player_index)
    if player then
      player.print(string.format(
        "[save-timelapse] exported %d entities from %d surface(s) to script-output/%s",
        total, surfaces, EXPORT_DIR))
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
