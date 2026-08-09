-- save-timelapse: the shortcut-triggered panel for controlling live
-- capture in-game, instead of only through the mod settings menu and
-- console commands. Everything GUI-specific lives here; capture.lua owns
-- the actual capture state this just reads and writes.

local capture = require("capture")

local M = {}

local FRAME_NAME = "save-timelapse-panel"
local SHORTCUT_NAME = "save-timelapse-panel"
local LIVE_CAPTURE_CHECKBOX = "save-timelapse-toggle-live-capture"
local CLOSE_BUTTON = "save-timelapse-panel-close"
local GENERATE_BUTTON = "save-timelapse-generate-baseline"
local RESET_BUTTON = "save-timelapse-reset-capture"

local CONFIRM_FRAME_NAME = "save-timelapse-confirm-reset"
local CONFIRM_YES_BUTTON = "save-timelapse-confirm-reset-yes"
local CONFIRM_NO_BUTTON = "save-timelapse-confirm-reset-no"

--- Every planet, whether or not it has been visited yet, plus every other
--- existing surface that isn't a planet's own (space platforms and the
--- like). Planets come from `game.planets`, not `game.surfaces`: a planet
--- prototype exists from game start regardless of whether its surface has
--- ever been created (`LuaPlanet.surface` is nil until then), which is
--- what lets an unvisited planet already show up here. Platforms have no
--- such fixed prototype list and can only be listed once they exist.
---
--- Rebuilt fresh only when the panel opens, not kept live-updated while
--- open: a new platform appearing while the panel happens to already be
--- open won't show until it's reopened. Planets are unaffected by this
--- since they're always fully known up front.
local function collect_surface_rows()
  local planet_surface_names = {}
  local planets = {}
  for planet_name, planet in pairs(game.planets) do
    local surface_name = planet.surface and planet.surface.name or planet_name
    planets[#planets + 1] = surface_name
    if planet.surface then
      planet_surface_names[planet.surface.name] = true
    end
  end
  table.sort(planets)

  local others = {}
  for _, surface in pairs(game.surfaces) do
    if not planet_surface_names[surface.name] then
      others[#others + 1] = surface.name
    end
  end
  table.sort(others)

  return planets, others
end

--- One checkbox row per surface. The row's own `name` only needs to be
--- unique among its siblings (Factorio requires that); which surface it
--- means travels via `tags` instead, read back in the event handlers,
--- since a surface name can be any string and isn't safe to pack into an
--- element name and parse back out.
local function add_surface_row(parent, surface_name, key)
  parent.add({
    type = "checkbox",
    name = "save-timelapse-surface-checkbox-" .. key,
    caption = surface_name,
    state = not capture.is_surface_excluded(surface_name),
    tags = { surface = surface_name },
  })
end

local function is_open(player)
  return player.gui.screen[FRAME_NAME] ~= nil
end

--- Multiplayer gate for the two actions that have a real vanilla
--- precedent for being admin-only: changing the live-capture runtime
--- setting (Factorio itself restricts changing a runtime-global setting
--- via its native menu to admins in multiplayer) and resetting capture (a
--- shared, disruptive, server-wide action). Per-surface exclusion has no
--- such precedent and is low-stakes/reversible, so it is left ungated.
local function player_is_allowed_admin_action(player)
  if not game.is_multiplayer() or player.admin then
    return true
  end
  player.print("[save-timelapse] only admins can change this in multiplayer")
  return false
end

local function build_panel(player)
  local outer = player.gui.screen.add({ type = "frame", name = FRAME_NAME, direction = "vertical" })
  player.opened = outer

  local titlebar = outer.add({ type = "flow", name = "save-timelapse-panel-titlebar", direction = "horizontal" })
  titlebar.drag_target = outer
  titlebar.add({
    type = "label",
    caption = "Save Timelapse",
    style = "frame_title",
    ignored_by_interaction = true,
  })
  local spacer = titlebar.add({
    type = "empty-widget",
    style = "draggable_space_header",
    ignored_by_interaction = true,
  })
  spacer.style.horizontally_stretchable = true
  titlebar.add({
    type = "sprite-button",
    name = CLOSE_BUTTON,
    sprite = "utility/close",
    style = "frame_action_button",
  })

  local body = outer.add({
    type = "frame",
    name = "save-timelapse-panel-body",
    direction = "vertical",
    style = "inside_shallow_frame_with_padding",
  })

  body.add({
    type = "checkbox",
    name = LIVE_CAPTURE_CHECKBOX,
    caption = "Enable live capture",
    state = settings.global["save-timelapse-live-capture"].value,
  })

  body.add({ type = "line", direction = "horizontal" })
  body.add({ type = "label", caption = "Recorded surfaces", style = "caption_label" })

  local scroll = body.add({
    type = "scroll-pane",
    name = "save-timelapse-surface-scroll",
    direction = "vertical",
    vertical_scroll_policy = "auto",
  })
  scroll.style.maximal_height = 300

  local planets, others = collect_surface_rows()

  scroll.add({ type = "label", caption = "Planets" })
  for i, name in ipairs(planets) do
    add_surface_row(scroll, name, "planet-" .. i)
  end

  scroll.add({ type = "label", caption = "Platforms / other surfaces" })
  if #others == 0 then
    scroll.add({ type = "label", caption = "(none yet)" })
  end
  for i, name in ipairs(others) do
    add_surface_row(scroll, name, "other-" .. i)
  end

  -- Checking a box only ever changes what's recorded going forward or
  -- marks a surface as newly wanted; it never immediately runs the
  -- (freezing) export a newly-included surface needs, on its own. Generate
  -- is the explicit, separate step for that, so checking several surfaces
  -- in a row batches into one warning and one freeze instead of one per
  -- box, and doesn't force a decision to actually pay that cost right at
  -- the moment of ticking a box.
  body.add({ type = "button", name = GENERATE_BUTTON, caption = "Generate" })

  local footer = body.add({ type = "flow", name = "save-timelapse-panel-footer", direction = "horizontal" })
  local footer_spacer = footer.add({ type = "empty-widget", ignored_by_interaction = true })
  footer_spacer.style.horizontally_stretchable = true
  footer.add({ type = "button", name = RESET_BUTTON, caption = "Reset capture", style = "red_button" })

  player.set_shortcut_toggled(SHORTCUT_NAME, true)
end

--- A separate small screen frame rather than swapping the main panel's own
--- footer in place: deleting capture data is permanent (see
--- capture.reset_capture), and a distinct dialog the player has to
--- explicitly dismiss is a stronger guard against an accidental click than
--- a button that just changes what it does. `auto_center` and a fresh
--- `player.opened` mean it behaves like a normal Factorio confirmation
--- popup: centered, and closable with Escape (M.on_gui_closed already
--- handles cancelling it that way, same as clicking Cancel).
local function build_confirm_reset_dialog(player)
  if player.gui.screen[CONFIRM_FRAME_NAME] then
    return
  end

  local outer = player.gui.screen.add({ type = "frame", name = CONFIRM_FRAME_NAME, direction = "vertical" })
  outer.auto_center = true
  player.opened = outer

  local titlebar = outer.add({ type = "flow", direction = "horizontal" })
  titlebar.drag_target = outer
  titlebar.add({ type = "label", caption = "Reset capture?", style = "frame_title", ignored_by_interaction = true })
  local spacer = titlebar.add({
    type = "empty-widget",
    style = "draggable_space_header",
    ignored_by_interaction = true,
  })
  spacer.style.horizontally_stretchable = true

  local body = outer.add({ type = "frame", direction = "vertical", style = "inside_shallow_frame_with_padding" })
  local warning = body.add({
    type = "label",
    caption = "This permanently deletes every recorded frame and event for this playthrough. This cannot be undone.",
  })
  warning.style.single_line = false
  warning.style.maximal_width = 300

  local buttons = body.add({ type = "flow", direction = "horizontal" })
  buttons.style.top_margin = 8
  local button_spacer = buttons.add({ type = "empty-widget", ignored_by_interaction = true })
  button_spacer.style.horizontally_stretchable = true
  buttons.add({ type = "button", name = CONFIRM_NO_BUTTON, caption = "Cancel" })
  buttons.add({ type = "button", name = CONFIRM_YES_BUTTON, caption = "Reset", style = "red_button" })
end

--- Destroying the dialog doesn't hand `player.opened` back to the main
--- panel on its own (there's no stack, just a single current target), so
--- without this, Escape/E would stop closing anything after the first time
--- the confirm dialog was opened and dismissed, even with the panel still
--- visibly open behind it.
local function close_confirm_reset_dialog(player)
  if player.gui.screen[CONFIRM_FRAME_NAME] then
    player.gui.screen[CONFIRM_FRAME_NAME].destroy()
  end
  if player.gui.screen[FRAME_NAME] then
    player.opened = player.gui.screen[FRAME_NAME]
  end
end

function M.close(player_index)
  local player = game.get_player(player_index)
  if not player then
    return
  end
  if player.gui.screen[FRAME_NAME] then
    player.gui.screen[FRAME_NAME].destroy()
  end
  player.set_shortcut_toggled(SHORTCUT_NAME, false)
end

function M.open(player_index)
  local player = game.get_player(player_index)
  if not player or is_open(player) then
    return
  end
  build_panel(player)
end

function M.toggle(player_index)
  local player = game.get_player(player_index)
  if not player then
    return
  end
  if is_open(player) then
    M.close(player_index)
  else
    M.open(player_index)
  end
end

function M.on_gui_checked_state_changed(event)
  local element = event.element
  if not element or not element.valid then
    return
  end

  if element.name == LIVE_CAPTURE_CHECKBOX then
    local player = game.get_player(event.player_index)
    if not player_is_allowed_admin_action(player) then
      element.state = settings.global["save-timelapse-live-capture"].value
      return
    end
    settings.global["save-timelapse-live-capture"] = { value = element.state }
    return
  end

  local tags = element.tags
  if tags and tags.surface then
    -- element.state == true means the checkbox is now checked, i.e.
    -- included (matches add_surface_row's own `state = not
    -- capture.is_surface_excluded(...)`). Just records the choice; the
    -- Generate button is the separate, explicit step that actually takes
    -- a newly-included surface's catch-up baseline (see its own comment).
    capture.set_surface_excluded(tags.surface, not element.state)
  end
end

function M.on_gui_click(event)
  local element = event.element
  if not element or not element.valid then
    return
  end

  if element.name == CLOSE_BUTTON then
    M.close(event.player_index)
  elseif element.name == GENERATE_BUTTON then
    -- Admin-gated the same as reset: unlike a checkbox flip, this actually
    -- triggers the (shared, disruptive) freeze, not just a preference
    -- change.
    if not player_is_allowed_admin_action(game.get_player(event.player_index)) then
      return
    end
    capture.generate_pending_baselines(game.tick)
  elseif element.name == RESET_BUTTON then
    local player = game.get_player(event.player_index)
    if not player_is_allowed_admin_action(player) then
      return
    end
    build_confirm_reset_dialog(player)
  elseif element.name == CONFIRM_YES_BUTTON then
    local player = game.get_player(event.player_index)
    close_confirm_reset_dialog(player)
    -- Re-checked here too: the confirm dialog can sit open for a while,
    -- long enough for admin status to have changed under it.
    if not player_is_allowed_admin_action(player) then
      return
    end
    capture.reset_capture(player)
  elseif element.name == CONFIRM_NO_BUTTON then
    close_confirm_reset_dialog(game.get_player(event.player_index))
  end
end

function M.on_gui_closed(event)
  local element = event.element
  if not element or not element.valid then
    return
  end
  if element.name == FRAME_NAME then
    M.close(event.player_index)
  elseif element.name == CONFIRM_FRAME_NAME then
    close_confirm_reset_dialog(game.get_player(event.player_index))
  end
end

return M
