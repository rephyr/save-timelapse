data:extend({
  {
    type = "custom-input",
    name = "save-timelapse-toggle-panel",
    key_sequence = "CONTROL + SHIFT + T",
    consuming = "none",
  },
  {
    type = "shortcut",
    name = "save-timelapse-panel",
    action = "lua",
    icon = "__save-timelapse__/graphics/shortcut-x32.png",
    icon_size = 32,
    small_icon = "__save-timelapse__/graphics/shortcut-x24.png",
    small_icon_size = 24,
    associated_control_input = "save-timelapse-toggle-panel",
    toggleable = true,
  },
})
