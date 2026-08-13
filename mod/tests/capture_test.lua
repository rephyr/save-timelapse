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

if failures > 0 then
  print(string.format("\n%d check(s) failed", failures))
  os.exit(1)
else
  print("\nall checks passed")
end
