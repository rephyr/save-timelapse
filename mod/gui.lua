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

local BODY_NAME = "save-timelapse-panel-body"
local STATUS_FRAME = "save-timelapse-panel-status"
local WELL_NAME = "save-timelapse-surface-well"
local SCROLL_NAME = "save-timelapse-surface-scroll"
local GENERATE_NOTE = "save-timelapse-generate-note"

-- Prefixed like everything else here, and for a harder reason than tidiness:
-- a child's name has to clear `LuaGuiElement`'s own properties as well as its
-- siblings, so the obvious `text` and `state` are both rejected outright.
local STATUS_TEXT = "save-timelapse-status-text"
local STATUS_DETAIL = "save-timelapse-status-detail"
local ROW_CHECK = "save-timelapse-row-check"
local ROW_STATE = "save-timelapse-row-state"

local CONFIRM_FRAME_NAME = "save-timelapse-confirm-reset"
local CONFIRM_YES_BUTTON = "save-timelapse-confirm-reset-yes"
local CONFIRM_NO_BUTTON = "save-timelapse-confirm-reset-no"

--- Wide enough that a row's name and its state never crowd each other, and
--- fixed so the panel does not resize under the player as places change state.
local PANEL_WIDTH = 420

--- How often an open panel re-reads capture state. A second rather than a
--- tick because `capture.panel_status` runs an entity query per surface, and
--- nothing it shows changes fast enough to be worth more.
local REFRESH_TICKS = 60

local GREEN = { r = 0.45, g = 0.9, b = 0.45 }
local AMBER = { r = 1.0, g = 0.78, b = 0.3 }
local FADED = { r = 0.65, g = 0.65, b = 0.65 }

local next_refresh = 0

--- Surface names are lowercase internally, which is fine in a file path and
--- looks like a bug in a caption. Parenthesised so `gsub`'s replacement count
--- does not leak out as a second return value.
local function pretty_place(name)
  return (name:gsub("^%l", string.upper))
end

--- Ticks as something a player reads at a glance. Minutes are the smallest
--- unit worth showing: this counts a play session, not a benchmark.
local function spent(ticks)
  local minutes = math.floor(ticks / 3600)
  if minutes < 60 then
    return string.format("%dm", minutes)
  end
  return string.format("%dh %dm", math.floor(minutes / 60), minutes % 60)
end

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

--- What one place is doing, in the words a player can act on.
---
--- The four states are the four real ones, and the order they are tested in
--- is what keeps the list honest against the button below it: anything that
--- reads "needs a snapshot" is exactly what pressing the button would take.
--- A ticked place with nothing to snapshot yet says so rather than looking
--- broken, which covers both an unvisited planet and one nobody has built on.
local function row_state(surface_name, status, pending)
  if capture.is_surface_excluded(surface_name) then
    return "not recorded", FADED
  end
  if status.baselined[surface_name] then
    return "recording", GREEN
  end
  if pending[surface_name] then
    return "needs a snapshot", AMBER
  end
  return "nothing built yet", FADED
end

--- One row per surface: a checkbox on the left, what it is doing on the
--- right. The row's own `name` only needs to be unique among its siblings
--- (Factorio requires that); which surface it means travels via `tags`
--- instead, read back in the event handlers, since a surface name can be any
--- string and isn't safe to pack into an element name and parse back out.
local function add_surface_row(parent, surface_name, key, status, pending)
  local row = parent.add({
    type = "flow",
    name = "save-timelapse-row-" .. key,
    direction = "horizontal",
    tags = { surface = surface_name },
  })
  row.style.vertical_align = "center"
  row.style.horizontally_stretchable = true

  row.add({
    type = "checkbox",
    name = ROW_CHECK,
    caption = pretty_place(surface_name),
    state = not capture.is_surface_excluded(surface_name),
    tags = { surface = surface_name },
  })

  local spacer = row.add({ type = "empty-widget", ignored_by_interaction = true })
  spacer.style.horizontally_stretchable = true

  local caption, colour = row_state(surface_name, status, pending)
  local state = row.add({ type = "label", name = ROW_STATE, caption = caption })
  state.style.font_color = colour
end

--- What the button says, whether it can be pressed, and the line under it.
---
--- Naming the places it would snapshot is the whole point: the button used to
--- read "Generate", which said nothing about what it made, and did nothing at
--- all in the two states below where it is now disabled with a reason.
local function generate_state(status)
  if not status.on then
    return "Snapshot new places", false, "Turn recording on first."
  end
  if #status.pending == 0 then
    return "Snapshot new places", false, "Everything ticked above is already recording."
  end
  local names = {}
  for i, name in ipairs(status.pending) do
    names[i] = pretty_place(name)
  end
  return "Snapshot " .. table.concat(names, ", "), true,
    "Catches these up so they can start recording. Freezes the game for a moment."
end

local function is_open(player)
  return player.gui.screen[FRAME_NAME] ~= nil
end

--- Everything that changes while the panel sits open, written into a panel
--- that already exists. A rebuild would do the same job and throw away the
--- scroll position and any half-made click along with it.
local function refresh_panel(frame, status)
  local body = frame[BODY_NAME]
  if not body then
    return
  end

  local pending = {}
  for _, name in ipairs(status.pending) do
    pending[name] = true
  end

  local strip = body[STATUS_FRAME]
  if strip then
    local recording = 0
    for name in pairs(status.baselined) do
      if not capture.is_surface_excluded(name) then
        recording = recording + 1
      end
    end

    local text = strip[STATUS_TEXT]
    text.caption = status.on and "Recording" or "Not recording"
    text.style.font_color = status.on and GREEN or FADED

    local detail = strip[STATUS_DETAIL]
    if not status.on then
      detail.caption = ""
    elseif status.since_tick then
      detail.caption = string.format("%s, %s since you loaded", recording == 1 and "1 place" or
        string.format("%d places", recording), spent(game.tick - status.since_tick))
    else
      detail.caption = "waiting for the first event"
    end
  end

  -- Through the well rather than straight off the body: the list sits in its
  -- own inset frame, so the scroll pane is a grandchild here.
  local well = body[WELL_NAME]
  local scroll = well and well[SCROLL_NAME]
  if scroll then
    for _, row in pairs(scroll.children) do
      local name = row.tags and row.tags.surface
      if name and row[ROW_STATE] then
        local caption, colour = row_state(name, status, pending)
        row[ROW_STATE].caption = caption
        row[ROW_STATE].style.font_color = colour
      end
    end
  end

  local button = body[GENERATE_BUTTON]
  local note = body[GENERATE_NOTE]
  if button and note then
    local caption, enabled, says = generate_state(status)
    button.caption = caption
    button.enabled = enabled
    note.caption = says
  end
end

--- The panel of whoever acted, brought up to date at once. Waiting for the
--- next tick refresh would leave a ticked box reading the state it had before
--- the tick, which is exactly long enough to look like the click missed.
local function refresh_for(player)
  if player and is_open(player) then
    refresh_panel(player.gui.screen[FRAME_NAME], capture.panel_status())
  end
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
  local status = capture.panel_status()
  local pending = {}
  for _, name in ipairs(status.pending) do
    pending[name] = true
  end

  local outer = player.gui.screen.add({ type = "frame", name = FRAME_NAME, direction = "vertical" })
  outer.auto_center = true
  outer.style.width = PANEL_WIDTH
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
    name = BODY_NAME,
    direction = "vertical",
    style = "inside_shallow_frame_with_padding",
  })

  -- Whether anything is being recorded, said first and said plainly. The panel
  -- used to open on a checkbox and a list of names, which left the one
  -- question anybody opens it to ask unanswered.
  local strip = body.add({ type = "frame", name = STATUS_FRAME, direction = "horizontal", style = "inside_deep_frame" })
  strip.style.padding = 8
  strip.style.horizontally_stretchable = true
  strip.add({ type = "label", name = STATUS_TEXT, style = "bold_label" })
  local strip_spacer = strip.add({ type = "empty-widget", ignored_by_interaction = true })
  strip_spacer.style.horizontally_stretchable = true
  local detail = strip.add({ type = "label", name = STATUS_DETAIL })
  detail.style.font_color = FADED

  local live = body.add({
    type = "checkbox",
    name = LIVE_CAPTURE_CHECKBOX,
    caption = "Record while I play",
    state = status.on,
  })
  live.style.top_margin = 8

  local places = body.add({ type = "label", caption = "Places to record", style = "caption_label" })
  places.style.top_margin = 8

  local well = body.add({ type = "frame", name = WELL_NAME, direction = "vertical", style = "inside_deep_frame" })
  well.style.horizontally_stretchable = true
  local scroll = well.add({
    type = "scroll-pane",
    name = SCROLL_NAME,
    direction = "vertical",
    vertical_scroll_policy = "auto",
  })
  scroll.style.maximal_height = 300
  scroll.style.padding = 8
  scroll.style.horizontally_stretchable = true

  local planets, others = collect_surface_rows()

  scroll.add({ type = "label", caption = "Planets", style = "caption_label" })
  for i, name in ipairs(planets) do
    add_surface_row(scroll, name, "planet-" .. i, status, pending)
  end

  local platforms = scroll.add({ type = "label", caption = "Space platforms", style = "caption_label" })
  platforms.style.top_margin = 8
  if #others == 0 then
    local none = scroll.add({ type = "label", caption = "none yet" })
    none.style.font_color = FADED
  end
  for i, name in ipairs(others) do
    add_surface_row(scroll, name, "other-" .. i, status, pending)
  end

  -- Checking a box only ever changes what's recorded going forward or
  -- marks a surface as newly wanted; it never immediately runs the
  -- (freezing) export a newly-included surface needs, on its own. This is
  -- the explicit, separate step for that, so checking several surfaces
  -- in a row batches into one warning and one freeze instead of one per
  -- box, and doesn't force a decision to actually pay that cost right at
  -- the moment of ticking a box.
  local caption, enabled, says = generate_state(status)
  local button = body.add({ type = "button", name = GENERATE_BUTTON, caption = caption })
  button.enabled = enabled
  button.style.top_margin = 8
  button.style.horizontally_stretchable = true

  local note = body.add({ type = "label", name = GENERATE_NOTE, caption = says })
  note.style.single_line = false
  note.style.maximal_width = PANEL_WIDTH - 48
  note.style.font_color = FADED

  local footer = body.add({ type = "flow", name = "save-timelapse-panel-footer", direction = "horizontal" })
  footer.style.top_margin = 12
  local footer_spacer = footer.add({ type = "empty-widget", ignored_by_interaction = true })
  footer_spacer.style.horizontally_stretchable = true
  footer.add({ type = "button", name = RESET_BUTTON, caption = "Reset capture", style = "red_button" })

  refresh_panel(outer, status)
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

--- Keeps every open panel current, the same way the other modules' pending
--- work is driven from control.lua's single on_tick. No-ops when nobody has
--- the panel up, which is the common case and the reason the entity queries
--- behind `panel_status` are safe to make on a timer at all.
function M.run_pending_tick_work(tick)
  if tick < next_refresh then
    return
  end
  next_refresh = tick + REFRESH_TICKS

  local open = {}
  for _, player in pairs(game.players) do
    if is_open(player) then
      open[#open + 1] = player
    end
  end
  if #open == 0 then
    return
  end

  local status = capture.panel_status()
  for _, player in ipairs(open) do
    refresh_panel(player.gui.screen[FRAME_NAME], status)
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
    refresh_for(player)
    return
  end

  local tags = element.tags
  if tags and tags.surface then
    -- element.state == true means the checkbox is now checked, i.e.
    -- included (matches add_surface_row's own `state = not
    -- capture.is_surface_excluded(...)`). Just records the choice; the
    -- button below is the separate, explicit step that actually takes
    -- a newly-included surface's catch-up baseline (see its own comment).
    capture.set_surface_excluded(tags.surface, not element.state)
    refresh_for(game.get_player(event.player_index))
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
    local player = game.get_player(event.player_index)
    if not player_is_allowed_admin_action(player) then
      return
    end
    capture.generate_pending_baselines(game.tick)
    refresh_for(player)
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
    refresh_for(player)
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
