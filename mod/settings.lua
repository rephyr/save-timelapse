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
    -- Natural terrain covers every generated tile around the base, not
    -- just where the player built, so this roughly 5x'd a real ~38MB/30s
    -- export to ~200MB/161s in testing. Off by default so enabling live
    -- capture doesn't silently sign up for a much longer baseline freeze;
    -- save-timelapse.exe's from-saves flow asks about this each run
    -- instead of assuming this setting.
    type = "bool-setting",
    name = "save-timelapse-capture-terrain",
    setting_type = "startup",
    default_value = false,
    order = "b2",
  },
  {
    type = "bool-setting",
    name = "save-timelapse-live-capture",
    setting_type = "runtime-global",
    default_value = false,
    order = "c",
  },
  {
    -- Snapshot the whole surface on a timer, for testing the export path
    -- during real play independent of live capture. 0 disables it.
    --
    -- Spread over many ticks the same way the live-capture baseline is
    -- (SNAPSHOT_BATCH_SIZE work items per tick in control.lua), so this does
    -- not stall the game the way one giant single-tick export would. The
    -- cost is elapsed time instead: on a large base a snapshot can still be
    -- running when the next one is due, in which case the timer's tick is
    -- silently skipped rather than overlapping it. Small intervals are only
    -- meaningful on small saves for that reason.
    type = "int-setting",
    name = "save-timelapse-snapshot-seconds",
    setting_type = "runtime-global",
    default_value = 0,
    minimum_value = 0,
    maximum_value = 3600,
    order = "d",
  },
})
