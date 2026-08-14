-- Enough of Factorio's API to load and drive the mod outside the game.
--
-- Everything except encode.lua talks to the game, which is why everything
-- except encode.lua was untested. The API surface this mod actually uses is
-- small and almost entirely inert: it reads a tick, writes a file, and stores
-- a table. So the parts that matter are recorded rather than emulated, and a
-- test asserts on what the mod tried to do.
--
-- Deliberately not a simulator. Nothing here models entities, surfaces or
-- events firing on their own; a test builds exactly the situation it is about
-- and calls the mod directly.

local M = {}

--- Every file the mod wrote, in order, as `{ path, data, append }`. The mod's
--- only output channel, so this is what most assertions are about.
M.written = {}

--- Paths passed to `helpers.remove_path`, which is how a capture is deleted.
M.removed = {}

--- Lines the mod logged. Its only way of reporting something that is worth
--- knowing but must not stop a capture.
M.logged = {}

--- Reset every global to a fresh game at `tick`, with no capture in progress.
---
--- Called between tests rather than once, since `storage` surviving from one
--- test into the next is exactly the confusion this is meant to rule out.
function M.reset(tick)
  M.written = {}
  M.removed = {}
  M.logged = {}

  -- `storage` is Factorio's own persisted table, and rewinds with a save. A
  -- test that loads an older save assigns a copy of the earlier value back.
  _G.storage = {}

  _G.game = {
    tick = tick or 0,
    surfaces = {},
    forces = { player = { rockets_launched = 0 } },
    -- Empty rather than absent: the mod iterates this every flush, and a nil
    -- would be a fake-only crash rather than anything the game does.
    connected_players = {},
    players = {},
    get_player = function() return nil end,
    print = function() end,
  }

  _G.helpers = {
    write_file = function(path, data, append)
      M.written[#M.written + 1] = { path = path, data = data, append = append or false }
    end,
    remove_path = function(path)
      M.removed[#M.removed + 1] = path
    end,
  }

  _G.log = function(message)
    M.logged[#M.logged + 1] = message
  end

  -- Handlers are kept rather than dispatched: a test calls the one it means.
  _G.script = {
    active_mods = { ["save-timelapse"] = "0.8.0" },
    handlers = {},
    on_event = function(id, handler) _G.script.handlers[id] = handler end,
    -- Kept per interval, not as one handler. Factorio allows exactly one
    -- per interval and silently replaces it, which is the collision
    -- control.lua's multiplexer exists to prevent, so a fake that kept only
    -- the last one could not tell the two apart. `nil` releases, as it does
    -- in the game.
    nth_ticks = {},
    on_nth_tick = function(interval, handler) _G.script.nth_ticks[interval] = handler end,
    on_load = function(handler) _G.script.loaded = handler end,
    on_init = function(handler) _G.script.inited = handler end,
    on_configuration_changed = function(handler) _G.script.reconfigured = handler end,
  }

  _G.commands = {
    commands = {},
    add_command = function(name, _, handler) _G.commands.commands[name] = handler end,
  }

  -- Every setting the mod reads, at its default. A test that cares overwrites
  -- the one it cares about.
  _G.settings = {
    global = {
      ["save-timelapse-live-capture"] = { value = true },
      ["save-timelapse-snapshot-seconds"] = { value = 0 },
    },
    startup = {
      ["save-timelapse-headless-scan"] = { value = false },
      ["save-timelapse-terrain-scan"] = { value = false },
      ["save-timelapse-include-resources"] = { value = false },
      ["save-timelapse-capture-terrain"] = { value = false },
    },
  }

  _G.prototypes = { entity = {}, tile = {} }

  -- Only the members the mod names. A missing one would read as nil and pass
  -- silently, so they are spelled out rather than generated.
  _G.defines = {
    events = {},
    direction = { north = 0, northeast = 2, east = 4, southeast = 6, south = 8, southwest = 10, west = 12, northwest = 14 },
    rail_direction = { front = 0, back = 1 },
    -- A rail is asked one end and one branch at a time. `none` is in here
    -- because the game has it, and asking about it is exactly the combination
    -- that may be rejected, which a test needs to be able to reproduce.
    rail_connection_direction = { straight = 0, left = 1, right = 2, none = 3 },
    inventory = {},
  }
  local event_names = {
    "on_built_entity", "on_robot_built_entity", "script_raised_built", "script_raised_revive", "on_entity_cloned",
    "on_player_rotated_entity", "on_player_mined_entity", "on_robot_mined_entity", "on_entity_died", "script_raised_destroy",
    "on_player_built_tile", "on_robot_built_tile", "on_player_mined_tile", "on_robot_mined_tile",
    "on_resource_depleted",
    "on_tick", "on_rocket_launched", "on_research_finished", "on_player_created", "on_gui_click",
    "on_gui_checked_state_changed", "on_gui_closed", "on_lua_shortcut", "on_runtime_mod_setting_changed",
    "on_surface_created", "on_surface_deleted", "on_pre_surface_deleted", "on_player_changed_surface",
  }
  for i, name in ipairs(event_names) do
    _G.defines.events[name] = i
  end
end

--- The single file written to `path`, or nil. Fails the caller's assertion
--- rather than silently picking one when a path was written more than once,
--- which for a segment header would mean the file was recreated.
function M.written_to(path)
  local found = nil
  for _, write in ipairs(M.written) do
    if write.path == path then
      found = write
    end
  end
  return found
end

--- How many writes went to `path`.
function M.write_count(path)
  local n = 0
  for _, write in ipairs(M.written) do
    if write.path == path then
      n = n + 1
    end
  end
  return n
end

--- Every distinct path written, in first-write order.
function M.paths()
  local seen, order = {}, {}
  for _, write in ipairs(M.written) do
    if not seen[write.path] then
      seen[write.path] = true
      order[#order + 1] = write.path
    end
  end
  return order
end

--- A surface with just enough on it to be found and named.
---
--- `find_entities_filtered` honours `type` and nothing else, and
--- `find_tiles_filtered` honours `name`. The terrain scan asks for scenery
--- types over an area it computed from the player's own entities, so a fake
--- that returned everything to both queries could not tell the two apart, and a
--- test of what the scan picks up would pass on a scan that picked up the wrong
--- things. `force` and `area` stay ignored: no fake entity has a force, and
--- every test that cares about bounds asserts on the bounds directly.
function M.surface(name, entities, tiles)
  return {
    name = name,
    valid = true,
    index = 1,
    find_entities_filtered = function(filter)
      local all = entities or {}
      local wanted = filter and filter.type
      if not wanted then
        return all
      end
      if type(wanted) == "string" then
        wanted = { wanted }
      end
      local allowed, kept = {}, {}
      for _, t in ipairs(wanted) do
        allowed[t] = true
      end
      for _, entity in ipairs(all) do
        if allowed[entity.type] then
          kept[#kept + 1] = entity
        end
      end
      return kept
    end,
    find_tiles_filtered = function(filter)
      local all = tiles or {}
      local wanted = filter and filter.name
      if not wanted then
        return all
      end
      if type(wanted) == "string" then
        wanted = { wanted }
      end
      local allowed, kept = {}, {}
      for _, n in ipairs(wanted) do
        allowed[n] = true
      end
      for _, tile in ipairs(all) do
        if allowed[tile.name] then
          kept[#kept + 1] = tile
        end
      end
      return kept
    end,
    get_tile = function() return { name = "grass-1", valid = true } end,
  }
end

--- One tile as a scan reads it. `hidden` is the ground Factorio kept under
--- placed floor, and `deep` the layer under that.
function M.tile(fields)
  return {
    valid = true,
    name = fields.name,
    position = { x = fields.x or 0, y = fields.y or 0 },
    hidden_tile = fields.hidden,
    double_hidden_tile = fields.deep,
  }
end

--- One entity as the mod reads it: every field it touches and nothing else.
function M.entity(fields)
  return {
    valid = true,
    name = fields.name,
    type = fields.type or fields.name,
    position = { x = fields.x or 0, y = fields.y or 0 },
    direction = fields.direction or 0,
    unit_number = fields.unit_number,
    tile_width = fields.tile_width or 1,
    tile_height = fields.tile_height or 1,
    surface = fields.surface or { name = fields.surface_name or "nauvis" },
    -- Keyed "<end>:<branch>", matching how the mod asks: one end and one
    -- branch per call. A missing key is nothing attached there, which is the
    -- ordinary answer for most of the eight combinations.
    get_connected_rail = fields.get_connected_rail or function(opts)
      return (fields.connections or {})[opts.rail_direction .. ":" .. opts.rail_connection_direction]
    end,
  }
end

--- One GUI element, as much of `LuaGuiElement` as the panel uses.
---
--- Children are reachable both by name (`screen["save-timelapse-panel"]`,
--- which is how the mod asks whether its panel is open) and as a list, which
--- is how a test finds the row it wants to click.
function M.element(spec)
  local element = {
    type = spec.type or "flow",
    name = spec.name,
    caption = spec.caption,
    tags = spec.tags,
    state = spec.state,
    valid = true,
    children = {},
    -- Present but inert. The panel sets stretch, margins and maximal heights
    -- on almost every element it builds; none of it changes behaviour, and a
    -- missing table would fail the build rather than the layout.
    style = {},
  }

  element.add = function(child_spec)
    local child = M.element(child_spec)
    child.parent = element
    element.children[#element.children + 1] = child
    if child.name then
      element[child.name] = child
    end
    return child
  end

  element.destroy = function()
    element.valid = false
    local parent = element.parent
    if not parent then
      return
    end
    if element.name then
      parent[element.name] = nil
    end
    for i, sibling in ipairs(parent.children) do
      if sibling == element then
        table.remove(parent.children, i)
        break
      end
    end
  end

  element.clear = function()
    element.children = {}
  end

  return element
end

--- Finds a descendant by name, for a test that wants to click something the
--- panel built rather than restating the tree it built it in.
function M.find(element, name)
  if element.name == name then
    return element
  end
  for _, child in ipairs(element.children) do
    local found = M.find(child, name)
    if found then
      return found
    end
  end
  return nil
end

--- A player with a GUI. `printed` collects what the mod told them, which for
--- the admin gate is the entire observable effect.
function M.player(fields)
  fields = fields or {}
  local player = {
    index = fields.index or 1,
    name = fields.name or "you",
    admin = fields.admin or false,
    valid = true,
    printed = {},
    gui = { screen = M.element({ type = "screen" }) },
    surface = fields.surface or { name = "nauvis", planet = {} },
    position = { x = 0, y = 0 },
  }
  player.print = function(message)
    player.printed[#player.printed + 1] = message
  end
  -- The toolbar shortcut's pressed look, which the panel keeps in step with
  -- whether it is open. Recorded rather than ignored: a shortcut left lit
  -- with no panel behind it is a real, if small, bug.
  player.shortcut_toggled = {}
  player.set_shortcut_toggled = function(name, toggled)
    player.shortcut_toggled[name] = toggled
  end
  return player
end

return M
