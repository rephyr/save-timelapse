-- The first two settings are startup scope deliberately.
--
-- Factorio stores runtime-global setting values inside each save file, and a
-- loaded save restores its own stored values in preference to mod-settings.dat.
-- An external tool therefore cannot set a runtime-global flag for an existing
-- save. Startup values are read from mod-settings.dat regardless of the save,
-- which is what makes unattended export possible.
--
-- Live capture is the opposite case: it's the player choosing to record
-- during an active session, not the CLI forcing a flag on an existing save,
-- so it belongs as an ordinary runtime setting -- changeable live from the
-- in-game settings menu, the normal reason runtime settings exist.

data:extend({
  {
    type = "bool-setting",
    name = "save-timelapse-headless-scan",
    setting_type = "startup",
    default_value = false,
    order = "a",
  },
  {
    -- Every ore tile is a separate entity, so resources typically outnumber
    -- built entities while saying nothing about how the factory grew.
    type = "bool-setting",
    name = "save-timelapse-include-resources",
    setting_type = "startup",
    default_value = false,
    order = "b",
  },
  {
    type = "bool-setting",
    name = "save-timelapse-live-capture",
    setting_type = "runtime-global",
    default_value = false,
    order = "c",
  },
})
