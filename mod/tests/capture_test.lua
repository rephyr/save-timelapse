-- Unit tests for mod/capture.lua's segment lifecycle, run outside Factorio
-- against the fake in fake_factorio.lua:
--
--   lua mod/tests/capture_test.lua
--
-- Every bug this file guards against shipped. A capture that recorded nothing
-- after a reset, a segment file with no header, a playthrough whose two
-- branches merged into one factory: all of them were in code with no test
-- because the code touched `game`, and the fake is what removes that excuse.

package.path = (arg[0]:match("(.*/)") or "./") .. "../?.lua;" .. package.path
local fake = dofile((arg[0]:match("(.*/)") or "./") .. "fake_factorio.lua")

local failures = 0

local function check(name, actual, expected)
  if actual == expected then
    print("ok   " .. name)
  else
    failures = failures + 1
    print("FAIL " .. name)
    print("     expected: " .. tostring(expected))
    print("     actual:   " .. tostring(actual))
  end
end

local function check_true(name, actual)
  check(name, not not actual, true)
end

--- A load: Factorio re-runs every module's top-level code, which is how the
--- mod knows a load happened at all. Module locals reset; `storage` does not,
--- having come from the save.
local function load_mod()
  for _, name in ipairs({ "capture", "encode", "export", "milestones" }) do
    package.loaded[name] = nil
  end
  return require("capture")
end

local function copy(value)
  if type(value) ~= "table" then
    return value
  end
  local out = {}
  for k, v in pairs(value) do
    out[k] = copy(v)
  end
  return out
end

--- A save file, as far as this mod is concerned: `storage` as it stood when
--- the save was written. Loading it puts that back, which is the whole
--- mechanism behind segment lineage.
local function save_game()
  return copy(_G.storage)
end

--- Copied on the way in as well as out. A save can be loaded more than once,
--- and playing on mutates `storage`, so handing the same table over would let
--- the second load see what the first one did.
local function load_save(saved, tick)
  _G.storage = copy(saved)
  _G.game.tick = tick
  return load_mod()
end

--- Which segment files a run wrote, in the order they were first written.
local function segments()
  local found = {}
  for _, path in ipairs(fake.paths()) do
    if path:match("%.stev$") then
      found[#found + 1] = path:match("([^/]+)$")
    end
  end
  return table.concat(found, " ")
end

-- A capture's first segment ----------------------------------------------

fake.reset(1000)
local capture = load_mod()
capture.periodic_flush(1000)

check("a capture's first segment is named for the tick it starts at", segments(), "events_1000.stev")
check(
  "and has no parent, nothing having come before it",
  segments():match("_%d+_%d+") == nil,
  true
)

local header = fake.written[1]
check("the header is written rather than appended, so the file starts empty", header.append, false)
check("and it is the event magic", header.data:sub(1, 4), "STE1")

-- Lineage ------------------------------------------------------------------

-- Play on, then save. The save carries the segment it was written during.
local branch_a = save_game()

fake.reset(3000)
local reloaded = load_save(branch_a, 3000)
reloaded.periodic_flush(3000)

check(
  "a segment names the one its save was made during",
  segments(),
  "events_3000_1000.stev"
)

-- Forward, back, forward: the sequence that used to merge two branches.
--
-- Save during segment 1000, play into a second branch, then load that same
-- save again and carry on. Both later segments descend from 1000, and the
-- reading side keeps only the chain the newest one is on.
fake.reset(5000)
local branch_b = load_save(branch_a, 5000)
branch_b.periodic_flush(5000)
check("the abandoned branch names its own parent", segments(), "events_5000_1000.stev")

fake.reset(7000)
local returned = load_save(branch_a, 7000)
returned.periodic_flush(7000)
check(
  "and returning to the first branch names that one too, not the branch just left",
  segments(),
  "events_7000_1000.stev"
)

-- Reloading the same save twice ---------------------------------------------

fake.reset(9000)
local first_attempt = load_save(branch_a, 9000)
first_attempt.periodic_flush(9000)
local name_once = segments()

local again = load_save(branch_a, 9000)
again.periodic_flush(9000)

check("loading one save twice resumes at the same tick from the same parent", segments(), name_once)
check(
  "so it writes the same file, and writing a header truncates the attempt it replaces",
  fake.written[1].append,
  false
)

-- Resetting a capture --------------------------------------------------------

fake.reset(11000)
local running = load_mod()
running.periodic_flush(11000)
check_true("a running capture has written a segment", #fake.written > 0)

local before_reset = #fake.written
running.reset_capture(nil)
check("resetting deletes this playthrough's folder", #fake.removed, 1)
check_true("named for the session", fake.removed[1]:match("save%-timelapse/%x+/") ~= nil)

-- The capture state is deliberately rebuilt rather than left nil: with live
-- capture still on, a reset means "start again", not "stop". What must not
-- survive is anything describing the recording just deleted, which is the bug
-- that made a reset capture record nothing at all until the save was reloaded.
check(
  "a fresh segment header is written for the new recording",
  fake.written[before_reset + 1] and fake.written[before_reset + 1].data:sub(1, 4),
  "STE1"
)
check("truncating rather than appending, the old file being gone", fake.written[before_reset + 1].append, false)
check(
  "and nothing carries over saying this game was already described",
  _G.storage.timelapse_capture.prototypes_stamp,
  nil
)

-- The prototype description --------------------------------------------------

fake.reset(13000)
local described = load_mod()
described.periodic_flush(13000)
local wrote_prototypes = false
for _, write in ipairs(fake.written) do
  if write.path:match("prototypes%.json$") then
    wrote_prototypes = true
  end
end
check_true("a fresh capture describes this game's prototypes", wrote_prototypes)

local before = #fake.written
described.periodic_flush(13060)
local rewrote = false
for i = before + 1, #fake.written do
  if fake.written[i].path:match("prototypes%.json$") then
    rewrote = true
  end
end
check(
  "and does not rewrite that description on every flush, it being hundreds of kilobytes",
  rewrote,
  false
)

-- An exhausted ore patch ----------------------------------------------------
--
-- The bug this exists for: ore is not mined away a removal at a time. The
-- resource entity stays while its amount falls, and the game destroys it on
-- reaching zero without raising any of the removal events the capture listens
-- for, so every patch a factory ever ate stood there full for the whole
-- timelapse while the drills on top of it kept working.

--- Everything written to a segment file, joined: a segment is appended in
--- pieces, and a record's name lives in whichever piece first mentioned it.
local function segment_bytes()
  local parts = {}
  for _, write in ipairs(fake.written) do
    if write.path:match("%.stev$") then
      parts[#parts + 1] = write.data
    end
  end
  return table.concat(parts)
end

fake.reset(20000)
_G.settings.startup["save-timelapse-include-resources"] = { value = true }
local depleting = load_mod()
local patch = fake.entity({ name = "iron-ore", type = "resource", x = 12, y = -34 })
depleting.CAPTURE_HANDLERS[_G.defines.events.on_resource_depleted]({ entity = patch })
depleting.periodic_flush(20000)

check_true("a depleted patch is recorded as removed", segment_bytes():find("iron%-ore") ~= nil)

-- The `RemoveName` record: tag 128, a one byte payload, naming dictionary
-- entry 0. Asserted exactly, because a removal carrying only a position
-- resolves to whatever stands on the ore, which on an exhausted patch is the
-- drill that exhausted it.
check_true(
  "and named, so the removal reaches the ore rather than the drill on top of it",
  segment_bytes():find("\128\1\0", 1, true) ~= nil
)

-- Turning resources off is a statement that this capture has no ore in it, so
-- there is nothing to deplete either. The removal would otherwise be logged
-- against a patch no frame ever showed.
fake.reset(20000)
_G.settings.startup["save-timelapse-include-resources"] = { value = false }
local uninterested = load_mod()
uninterested.CAPTURE_HANDLERS[_G.defines.events.on_resource_depleted]({ entity = patch })
uninterested.periodic_flush(20000)

check("with resources not captured, depletion is not recorded either", segment_bytes():find("iron%-ore"), nil)

-- An entity the game destroyed before the event reached the mod. Costs that
-- one removal rather than raising, which would take the whole capture down.
fake.reset(20000)
_G.settings.startup["save-timelapse-include-resources"] = { value = true }
local gone = load_mod()
local invalid = fake.entity({ name = "copper-ore", type = "resource", x = 1, y = 2 })
invalid.valid = false
gone.CAPTURE_HANDLERS[_G.defines.events.on_resource_depleted]({ entity = invalid })
gone.periodic_flush(20000)

check("an already destroyed patch is skipped rather than raising", segment_bytes():find("copper%-ore"), nil)

-- Nests cleared ---------------------------------------------------------------
--
-- A nest is stationary and worth watching get cleared, unlike the biters that
-- come out of it, so it is recorded and its death is an event like any other.
-- These pin that down: it is one absence from `EXCLUDED_TYPES` away from being
-- silently dropped, and nothing else would say so.

fake.reset(30000)
local clearing = load_mod()
local nest = fake.entity({ name = "biter-spawner", type = "unit-spawner", x = 400, y = -120, unit_number = 77 })
clearing.CAPTURE_HANDLERS[_G.defines.events.on_entity_died]({ entity = nest })
clearing.periodic_flush(30000)

check_true("a nest destroyed is recorded as removed", segment_bytes():find(string.char(4), 1, true) ~= nil)

-- The biters themselves are not. They move, and this format cannot say
-- anything moved, so a captured one sits frozen wherever it was first logged
-- while their combat deaths flood the log with removals of things replay never
-- had.
fake.reset(30000)
local swarm = load_mod()
local biter = fake.entity({ name = "small-biter", type = "unit", x = 401, y = -121, unit_number = 78 })
swarm.CAPTURE_HANDLERS[_G.defines.events.on_entity_died]({ entity = biter })
swarm.periodic_flush(30000)

check("a biter dying is not", segment_bytes():find(string.char(4), 1, true), nil)

-- The other half of the same war. Biters expanding is the only thing in the
-- game that builds without a player or a bot doing it, so none of the ordinary
-- build events fire for it: a nest that appeared mid playthrough was recorded
-- only if a later baseline happened to catch it, while a nest cleared was
-- recorded the moment it died.
fake.reset(30000)
local expanding = load_mod()
local built = fake.entity({ name = "biter-spawner", type = "unit-spawner", x = 900, y = 40, unit_number = 80 })
expanding.CAPTURE_HANDLERS[_G.defines.events.on_biter_base_built]({ entity = built })
expanding.periodic_flush(30000)

check_true("a nest built by expansion is recorded", segment_bytes():find("biter%-spawner") ~= nil)
check_true("as something arriving rather than leaving", segment_bytes():find(string.char(3), 1, true) ~= nil)

-- A worm is stationary too, and shares the "turret" type with the player's
-- own, which is why it cannot be named in the scenery list and has to be
-- recorded the same way anything built is.
fake.reset(30000)
local worms = load_mod()
local worm = fake.entity({ name = "small-worm-turret", type = "turret", x = 402, y = -122, unit_number = 79 })
worms.CAPTURE_HANDLERS[_G.defines.events.on_entity_died]({ entity = worm })
worms.periodic_flush(30000)

check_true("and neither is a worm dropped", segment_bytes():find(string.char(4), 1, true) ~= nil)

-- Which playthrough a recording belongs to -------------------------------------

-- The id was the map seed, which identifies a map and not a playthrough: two
-- games rolled from one seed wrote into one folder, and the second one's
-- baseline manifest overwrote the first's. The tests that matter most here are
-- the two at the bottom, which are the recordings that already exist.

local function with_seed(tick, seed)
  fake.reset(tick)
  _G.game.surfaces.nauvis = fake.surface("nauvis")
  _G.game.surfaces.nauvis.map_gen_settings.seed = seed
  return load_mod()
end

local first = with_seed(1000, 12345)
first.periodic_flush(1000)
local first_id = _G.storage.timelapse_capture.session_id

-- A new game on the same map: identical seed, empty `storage`, and that second
-- part is the whole of what the old id could not see.
local second = with_seed(4000, 12345)
second.periodic_flush(4000)
local second_id = _G.storage.timelapse_capture.session_id

check_true("two playthroughs on one seed no longer share an id", first_id ~= second_id)
check_true(
  "and both fit the eight hex digits the folder name is",
  first_id >= 0 and first_id < 0x100000000 and second_id >= 0 and second_id < 0x100000000
)

-- Minted once, not per load: a reload that re-minted would start a new
-- recording every time the player came back to the game.
local saved = save_game()
fake.reset(6000)
_G.game.surfaces.nauvis = fake.surface("nauvis")
_G.game.surfaces.nauvis.map_gen_settings.seed = 12345
local again = load_save(saved, 6000)
again.periodic_flush(6000)
check("a reload keeps the id its capture was minted with", _G.storage.timelapse_capture.session_id, second_id)

-- An older save, whose id is its seed because that is what the mod stored back
-- then. It has to go on writing exactly where it always did.
fake.reset(8000)
_G.game.surfaces.nauvis = fake.surface("nauvis")
_G.game.surfaces.nauvis.map_gen_settings.seed = 777
_G.storage.timelapse_capture =
  { segment_start_tick = 100, last_tick = 100, segment_initialized = true, session_id = 777 }
local older = load_mod()
check("an older recording keeps the id it already had", older.compute_session_id(), 777)

-- A save that never captured, which is every build-from-saves run. There is
-- nothing to have stored an id on, so the seed still answers.
fake.reset(9000)
_G.game.surfaces.nauvis = fake.surface("nauvis")
_G.game.surfaces.nauvis.map_gen_settings.seed = 4242
local never_captured = load_mod()
check("a save that never captured still resolves to its seed", never_captured.compute_session_id(), 4242)

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
