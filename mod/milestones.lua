-- save-timelapse: milestones, the few moments in a playthrough worth marking
-- on the timeline: each science pack the first time it is produced, the first
-- rocket, and each planet the first time it is reached. Anything already
-- visible in the frames is deliberately absent.
--
-- Newline-delimited JSON. A playthrough produces about a dozen lines, and
-- nothing here is on a hot path.

local encode = require("encode")
local export = require("export")

local M = {}

--- Which milestones have already been recorded, so each fires once.
---
--- Its own storage key rather than nested in `storage.timelapse_capture`,
--- which a capture reset wipes: the milestone file goes with the session
--- folder, so this has to go with it. Kept separate, this would believe every
--- milestone had fired while the file recording them was gone.
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

--- Records `kind`/`id` once, at `tick`; a repeat is a no-op, which is what
--- lets every caller fire bluntly. `pcall`'d like every capture write, so a
--- failure costs a marker rather than the game.
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
--- Polled rather than evented: there is no "an assembling machine finished an
--- item" event, and `on_player_crafted_item` covers hand crafting only.
--- Production statistics are the only place the game exposes it, and
--- `input_counts` hands back everything ever produced in one table.
---
--- Statistics are per surface in 2.0, so this unions across them. Called from
--- the capture flush, so a marker can be a few seconds late, which is
--- invisible against frames a minute of game time apart.
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
--- passing through does not count as arriving. Swept over connected players
--- rather than hooked to `on_player_changed_surface`, which only fires on a
--- change and so would miss the planet a capture starts on.
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
