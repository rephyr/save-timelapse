-- save-timelapse: milestones, the handful of moments in a playthrough worth
-- marking on the timeline rather than watching for.
--
-- Three of them, all first-time-only: each science pack the first time it is
-- produced, the first rocket launched, and each planet the first time it is
-- reached. Anything already visible in the frames themselves (a rocket silo
-- being built, a base spreading) is deliberately not here: those are what the
-- timelapse is, and repeating them as markers would only add noise.
--
-- Written as plain newline-delimited JSON. A whole playthrough produces on
-- the order of a dozen lines, nowhere near the volume that justified a binary
-- format for frames and events, and being readable by eye is worth more here
-- than the few hundred bytes packing it would save.
--
-- Nothing here is on a hot path. A rocket launches once in a while, a planet
-- is reached a handful of times ever, and the science check runs on the
-- capture flush that already ticks every few seconds. That is why it can
-- afford to be as simple as it is.

local encode = require("encode")
local export = require("export")

local M = {}

--- Which milestones have already been recorded, so each fires once.
---
--- Its own top-level storage key, sibling to `storage.timelapse_capture`
--- rather than nested inside it: capture reset (see capture.lua's
--- `M.reset_capture`) wipes that key wholesale to force a fresh baseline, and
--- it should wipe this too, since the milestone file is deleted along with
--- the rest of the session folder. Keeping them separate would leave this
--- believing every milestone had already fired while the file recording them
--- was gone, and none would ever be written again.
local function seen()
  storage.timelapse_milestones = storage.timelapse_milestones or {}
  return storage.timelapse_milestones
end

function M.reset()
  storage.timelapse_milestones = nil
end

local function milestone_path(session_id)
  if not session_id then
    return export.EXPORT_DIR .. "milestones.jsonl"
  end
  return export.EXPORT_DIR .. encode.milestone_name(session_id)
end

--- Records `kind`/`id` once, at `tick`. A repeat is a no-op, which is what
--- makes every caller below able to fire bluntly without checking first.
---
--- `pcall`'d like every other capture write in this mod: a failed write
--- degrades to a missing marker, never a crash mid-game.
local function record(tick, kind, id, session_id)
  local key = kind .. ":" .. id
  local already = seen()
  if already[key] then
    return false
  end
  already[key] = tick
  pcall(helpers.write_file, milestone_path(session_id), encode.milestone_line(tick, kind, id), true)
  return true
end

M.record = record

--- The first time each science pack is produced.
---
--- Polled rather than driven by an event, because there is no event for "an
--- assembling machine finished an item": `on_player_crafted_item` only covers
--- hand crafting, which is not how science gets made past the first hour.
--- Production statistics are the only place the game exposes it, and
--- `input_counts` hands back every item ever produced in one table, so the
--- check is a scan of a few hundred entries against a set.
---
--- Statistics are per surface in 2.0, so this unions across them: a pack
--- first assembled on Vulcanus counts as first produced.
---
--- Called from the capture flush (every few seconds), not on_tick. A science
--- pack marker being a few seconds late is invisible against a timelapse
--- where one frame is a minute of game time.
function M.poll_science(tick, session_id)
  local force = game.forces["player"]
  if not force then
    return
  end

  for _, surface in pairs(game.surfaces) do
    local ok, counts = pcall(function()
      return force.get_item_production_statistics(surface).input_counts
    end)
    if ok and counts then
      for name, count in pairs(counts) do
        if count > 0 and encode.is_science_pack(name) then
          record(tick, "science", name, session_id)
        end
      end
    end
  end
end

--- Every planet reached, the first time a player is on it.
---
--- Checked against `surface.planet` rather than by name, so a space platform
--- passing through does not count as arriving somewhere: a platform is a
--- surface too, and only a planet has that field set.
---
--- Swept over connected players rather than hooked to
--- `on_player_changed_surface` alone, so the planet a capture *starts* on is
--- recorded too. That event only fires on a change, and nobody changes
--- surface to arrive on Nauvis at the beginning.
function M.poll_planets(tick, session_id)
  for _, player in pairs(game.connected_players) do
    local ok, planet = pcall(function()
      return player.surface.planet and player.surface.name
    end)
    if ok and planet then
      record(tick, "planet", planet, session_id)
    end
  end
end

--- The first rocket. Later ones are not recorded: on a base that launches
--- continuously they would bury every other marker, and "the first rocket"
--- is the moment anyone actually wants to find again.
function M.on_rocket_launched(event, session_id)
  record(event.tick, "rocket", "rocket-launched", session_id)
end

--- Everything that runs on the capture flush, so control.lua has one call to
--- make rather than tracking which milestone needs which cadence.
function M.poll(tick, session_id)
  M.poll_science(tick, session_id)
  M.poll_planets(tick, session_id)
end

return M
