# Architecture

## In short

Factorio's Lua sandbox cannot read files, open sockets, or spawn processes. A
mod's only output is writing into `script-output`, and it can never read back
what it wrote, so the mod and the desktop tool never communicate: the mod
appends bytes, the tool reads the directory afterwards. The mod states what it
saw, and every question that needs more than one moment's evidence is answered
on the Rust side.

Two ways in, one way out:

    existing saves ──> CLI drives headless Factorio, mod exports each save ──┐
                                                                             ├──> frames ──> viewer ──> video
    live play ──────> mod snapshots once, then logs only what changes ──────┘

A save file carries no history of its own, so the two paths know different
things and converge on the same on-disk shape.

| Decision | Because |
|---|---|
| Custom binary formats, not JSON | The mod writes during play on bases of ~900k entities. Runs, delta varints and a name dictionary took one megabase surface from 200 MB to 38 MB |
| Unknown records are skipped, not fatal | Factorio auto-updates mods; the desktop tool does not update itself, so "mod newer than tool" is the normal state |
| Log changes, don't re-snapshot | At ~50 bytes per entity, snapshotting a megabase every ten seconds writes gigabytes an hour |
| Reloads are resolved when reading | The mod cannot detect a reloaded save: every value durable enough to survive a load rewinds with it |
| Nothing under your Factorio install is touched | Exports run against a staged tree the CLI owns and deletes |

Time inside Factorio and time in the desktop tool are not worth the same. A
player feels a stutter; nobody minds an external tool taking a minute longer.
So work moves out of the game wherever it can, even when that costs more work
overall: scanning ground from a save afterwards reads a whole surface again,
which is strictly more than capturing it during the baseline.

Three rules the sections below assume:

1. **The format records that something was built or destroyed, never that it
   moved.** That is the whole entity filter: robots, biters, pentapods, trains
   and cars would be pinned wherever they were captured.
2. **Being worth drawing and being worth aiming at are different questions.**
   Nests are kept and drawn; they do not move the camera.
3. **Core format layout is frozen at version 3.** Anything new is an extension
   record with a length prefix, so an older reader skips exactly what it does
   not understand.

## Where to read what

| If you want | Read |
|---|---|
| How a save becomes frames | Pipeline, Staging model, Export trigger |
| The file formats | Frame format, Event format, Format stability |
| Why captures survive version changes | Format stability and the extension contract |
| What gets recorded and what does not | Entity filtering, What counts as the factory |
| How a modded game describes itself | What the game says about itself |
| How live capture reassembles history | Live capture and replay, Replay tolerates events it cannot apply |
| What happens when a player reloads | Reloading an earlier save |
| Why the viewer is fast | Rendering, Loading, Only writing surfaces that changed |
| What is on screen and why | Viewer chrome |
| How a video gets made | Exporting a video |

## Components

    mod/     Lua mod loaded by Factorio. Exports entity data in a custom binary format.
    src/     Rust CLI. Drives Factorio, collects exports, renders output.
    viewer/  Rust macroquad app. Pans, zooms, scrubs and exports the result.

The mod and the CLI never communicate directly. Factorio's Lua sandbox cannot
read files, open sockets, or spawn processes, so the mod's only output channel
is writing into `script-output`. The CLI reads that directory after the process
exits.

The viewer is a separate binary rather than a mode of the CLI, because it is
the thing a user keeps open and comes back to. The CLI builds a timelapse and
launches it; closing the window does not end a job.

## Pipeline

    saves/*.zip
        |
        |  CLI launches: factorio benchmark <save> benchmark-ticks 3
        |                         config <staged> mod-directory <staged>
        v
    Factorio loads the save, mod exports on first tick, process exits
        |
        v
    <staged>/script-output/save-timelapse/frame_<tick>_<surface>.stfr
        |
        |  CLI collects, orders by save sequence
        v
    frames/ -> renderer -> video or viewer bundle

One save produces one frame. Frame count equals the number of saves supplied.

## Staging model

Every export runs against a staged directory tree that the CLI owns and deletes
afterwards. Nothing under the user's Factorio installation is modified.

    <staged>/
        config.ini              read-data -> real install, write-data -> staged
        mods/
            *.zip               hardlinked from the user's mods folder
            mod-list.json       copied, not hardlinked
            mod-settings.dat    rewritten with the export flag enabled
            save-timelapse/     this mod, as a loose directory
        script-output/          where the mod writes

`mod-list.json` is copied rather than hardlinked because Factorio rewrites it
during load; a hardlink would propagate that write back into the user's real
mods folder. Mod archives are only ever read, so sharing an inode is safe.

The mod is staged from `mod/` rather than from whatever is installed, so an
export never depends on the user's installed version.

## Export trigger

The mod exports when the `save-timelapse-headless-scan` startup setting is true.

This setting must be startup scope. Factorio stores runtime-global setting
values inside each save file, and loading a save restores that save's stored
values in preference to `mod-settings.dat`. A runtime-global flag therefore
cannot be set from outside for an existing save. Startup settings are read from
`mod-settings.dat` regardless of save contents.

## mod-settings.dat

A Factorio PropertyTree:

    u16 x4        version (main, major, minor, developer)
    u8            flag
    PropertyTree  root dictionary

    PropertyTree := u8 type, u8 any_type, payload
    type: 0 none, 1 bool, 2 f64, 3 string, 4 list, 5 dict, 6 i64, 7 u64
    string  := u8 empty_flag, [u8 len | 0xFF u32 len], bytes
    list    := u32 count, then count * (string key, PropertyTree)
    dict    := as list

The CLI parses the full tree, sets one entry, and reserializes. Writing a
minimal file instead would discard every other mod's settings.

Two encoding details are required for byte-exact round-trips: empty strings are
written as `empty_flag=0` followed by a zero length rather than `empty_flag=1`,
and dictionary insertion order is preserved.

The version header is taken from the source file when one exists, otherwise
from `factorio.exe version`.

## Frame format

Every per-surface export (`frame_<tick>_<surface>.stfr`) is a small custom
binary format, not JSON. The mod writes it during real gameplay (a baseline
snapshot alone measures tens of seconds of frozen play on a ~375k entity base),
and JSON text formatting for every entity is most of that cost.

Wire format, all integers little endian:

    magic     4 bytes, "STF1"
    version   u8, MIN_SUPPORTED_VERSION through CURRENT_VERSION
    tick      u64
    surface   string (u16 length, then that many UTF-8 bytes)
    entity section, a sequence of:
      tag 0     DefineName  string name, u8 w, u8 h
      tag 1     EntityRun   varint name_id, varint count, u8 flags,
                            then per item varint dx, varint dy, and a u8
                            direction when flags has bit 0; then, when flags
                            has bit 1, varint len and that many bytes
      tag >=128 Extension   varint len, then that many bytes
    tag 9  EndEntities (no payload), marking the start of the tile section
    tile section, a sequence of:
      tag 0     DefineName  string name, u8 w, u8 h
      tag 2     TileRun     varint name_id, varint count, then per item
                            varint dx, varint dy
      tag >=128 Extension   varint len, then that many bytes
    checksum  u32, djb2 of every byte before it (magic and version included)

Coordinates within a run are zigzag varint deltas against the previous item,
starting from the origin. Grouping entities into per-name runs and delta
encoding their positions is what took a real megabase surface export from
200 MB to 38 MB. Version 1 predates runs entirely, writing one fixed width
record per entity or tile, and is still read by a separate function.

The version byte separates "this is a format I don't understand" from a
generic parse failure. The checksum catches what the tag structure cannot:
bit-level corruption that still decodes as plausible records.

`mod/control.lua` accumulates the checksum incrementally (`checksummed_write`,
beside every `helpers.write_file` call for a frame), a file running to hundreds
of megabytes and never being held whole on that side; the Rust reader hashes
the payload in one pass. Both implementations (`encode.checksum_update` in Lua,
`frame::checksum` in Rust) use a djb2 variant built from multiply, add and mod
alone, Factorio's Lua 5.2 having neither `string.pack` nor `bit32`, the same
constraint `u32le`/`i32le` work around by hand. A test in each language asserts
they agree on one known input.

A file older than the version byte has neither it nor the trailer and does not
parse under the current reader.

## Format stability and the extension contract

Version 3 of both formats is the last clean break. From it onward the core
layout does not change; anything added is an extension record, and both
formats use the same rule: **a tag of 128 or above carries a varint byte
length, then that many bytes.** A reader that does not recognise the tag skips
exactly that many bytes and carries on, so a capture written by a newer mod
still loads in an older tool, minus whatever the new record described. Core
tags stay below 128 so the two kinds can never collide.

Entity runs have a second, cheaper extension point: bit 1 of a run's flags
means a length prefixed block follows the run's coordinates. That is the
natural home for a future per-entity column (quality, say, or health), since a
top level record would have to restate a dictionary and a coordinate list to
re-associate itself with the entities it describes.

Extension payloads are never interleaved with the data they annotate.
`RUN_FLAG_DIRECTIONS` is, which is why an unknown column of that shape is
unskippable: a reader cannot find where the run ends. A trailing, length
prefixed block can always be stepped over, and anything added later has to keep
to that shape.

A length running past the end of the file is an error. Not understanding a
record is fine; one that does not fit means damage.

`Replay::unknown_extensions` counts what was stepped over, which the CLI turns
into a message saying the tool is behind the mod.

### What holds it in place

`tests/format_compatibility.rs` opens one real frame in all three released
encodings (v1 through v0.3, v2 in v0.4, v3 from v0.5) and asserts they agree.
The fixtures are committed bytes rather than generated at test time, so the
check is against what older builds actually wrote. A failure there means
somebody's existing capture stopped loading.

`DefineName` writes a prototype name the first time it is used and gives it the
next sequential id; every later reference is the two byte id. One dictionary is
shared by the entity and tile sections, a name only needing defining once.

There is no entity or tile count anywhere in the format. The incremental
exporter spreads one export across many ticks with play continuing in between,
so a count taken upfront can be wrong by the time writing finishes, and
scanning for one would reintroduce the stall that exporter avoids.
`EndEntities` removes the need: each section is a forward stream, and the tile
section runs until the 4 byte checksum trailer.

Entity coordinates are position times ten, rounded, the same fixed point scale
`world.rs::pos_key` uses. Tile coordinates are already integers: a tile named
at `(x, y)` occupies `[x, x+1) x [y, y+1)`, corner anchored where entities are
centre anchored. `d`, `w` and `h` are always present, a variable width encoding
to skip a default costing more complexity than the bytes it saves.

`tiles` covers placed floor (concrete, stone path, hazard and refined concrete
variants, landfill, a platform's foundation), an include list rather than
entity filtering's exclude list, since natural terrain vastly outnumbers placed
floor types. The list is asked of the game rather than stated: a tile counts if
an item places it or it can be mined (see What the game says about itself).

A surface is exported when it is nauvis or contains at least one entity owned
by the player force. A manifest listing exported surfaces accompanies each
set. Unlike the frame body, the manifest (`frame_<tick>_manifest.json`) and
the live-capture baseline manifest (`<session>/baseline.json`, see "Live
capture and replay" below) stay plain JSON: they hold one record per
*surface*, not per entity, so they're tiny, written once, and worth keeping
human readable for debugging the handshake between the mod and the Rust side.

## Entity filtering

Filtering happens in the `find_entities_filtered` query rather than in Lua, so
excluded types are never returned across the API boundary.

Excluded by default: characters, corpses, particles, projectiles, fish, fire,
smoke, explosions, ghosts, dropped items, streams, stickers and beams. Trees
and cliffs are excluded only while `save-timelapse-capture-terrain` is off,
since with it on they are wanted as ground context. Rocks are not part of that
exception: they are `simple-entity`, which stays excluded either way, since
that type is a catch-all covering whatever else Space Age puts in it rather
than rocks specifically.

The rest of the exclusions are all one idea: **this format records that
something was built or destroyed, never that it moved.** A frame carries a
position per entity and the event log carries add and remove records, and
neither has any way to say "the thing you already know about is now
somewhere else". Anything mobile is therefore pinned wherever it happened to
be when it was captured, while the real one carries on, so drawing it is
worse than leaving it out.

- **Flying robots** (`construction-robot`, `logistic-robot`, `combat-robot`).
  The highest-volume case by far: a megabase running a large construction job
  has tens of thousands airborne at once, each one a record in the baseline
  and in every frame of a from-saves export. Roboports stay, being the
  stationary infrastructure that actually shows the network growing.
- **Biters and spitters** (`unit`). Additionally, their combat deaths would
  fill a live-capture log with removals indistinguishable from the player
  mining something. Confirmed against a real capture, where enemies were ~6%
  of exported entities.
- **Space Age's own mobile enemies** (`spider-unit`, `spider-leg`,
  `segmented-unit`, `segment`). Gleba's stompers and strafers, the legs they
  walk on, and Vulcanus's demolishers with their trailing body segments. Only
  Gleba's wrigglers are plain `unit`.
- **Vehicles and rolling stock** (`car`, `spider-vehicle`, `locomotive`,
  `cargo-wagon`, `fluid-wagon`, `artillery-wagon`). Rails, signals and
  stations stay, being the stationary infrastructure that shows the network
  growing, exactly as roboports stay while the robots do not.

`unit` is one prototype type rather than "enemies", which is why Space Age's
own mobile enemies need naming separately. Trains are the worst case of the
rule: a from-saves export catches them somewhere different in every save, so
they blink around the network frame by frame, while a live capture shows one
parked where it was placed for the rest of the playthrough.

Types here are read out of the game's own
`space-age/prototypes/entity/enemies.lua` and `base/prototypes/entity/` rather
than inferred from names: `spider-unit` sounds like it would catch Spidertron
and does not, Spidertron being `spider-vehicle`.

A capture already on disk still holds these entities, so
`viewer/src/registry.rs` recognises them too, from the game's own prototype
types when the capture carries them and from its fallback name lists when it
does not. That cannot un-capture them, but it stops them counting as
construction for the auto-follow camera.

Nests (`unit-spawner`) and worm turrets are deliberately **kept**, despite
being enemies, because they are stationary and so the format represents them
honestly. Watching the front line move outward as nests are cleared is a real
part of how expansion looks, and the viewer colors both red so it reads at a
glance. Worms additionally could not be filtered safely even if wanted: they
share Factorio's `turret` type with player-built turrets, so excluding them
would mean matching by name rather than by what the entity actually is,
risking a real player entity. Note that this cuts the other way for the
viewer, where the bare `turret` type is precisely what identifies a worm,
because the player's own turrets are three separate types.

Resource entities are excluded unless `save-timelapse-include-resources` is set.
Every ore tile is a separate entity and they typically outnumber built entities
while carrying no information about factory growth.

## What counts as the factory

Being worth drawing and being worth *aiming at* are separate questions. Two
things point themselves at the base: the auto-follow camera
(`viewer/src/construction.rs`) and the terrain margin (`mod/export.lua`). Both
need "where the buildings are" rather than "where anything the capture kept
is".

Trees, cliffs, resource deposits, nests and worms are all excluded from that
box. Every one of them sits wherever the map generated it, and nests cover
every generated chunk in every direction, so counting them makes the box span
the explored map; since the camera centres on the box's midpoint, it then
centres on the middle of the revealed map. The same exclusion feeds the mod's
terrain margin, so "32 tiles around the factory" means the factory.

Both sides ask the prototype what it is rather than matching names: the mod
directly, once per distinct name rather than once per entity, and the viewer by
reading what the game said about its own prototypes (see What the game says
about itself). The name lists survive only for captures recorded before the mod
started saying, and are private to `registry.rs`.

### How much ground to capture

`encode.terrain_margin` decides, and it is derived rather than picked. A fitted
view leaves the difference between the base's shape and the frame's as empty
world on whichever axis does not bind: a square base in 16:9 fills 52% of the
width, so nearly half the picture is beyond the box. The margin is exactly that
exposed region, computed from the output aspect and `AUTO_FOLLOW_FIT_MARGIN`.

A single fraction of the base's larger dimension is the obvious rule and is
wrong by an order of magnitude in both directions, because what a fit exposes
depends on shape, not size: a 2:1 base needs 0.06 of its long side, a 1:2 base
needs 1.4 of it.

Shape-driven also means a long thin base asks for enormous amounts (a 100x5000
corridor wants 4,800 tiles, which is 140M tiles of ground), so the result is
bounded by an area budget: at least 4M tiles, and at least four times the
factory's own footprint. Four, because solving the area cap for a square gives
a margin of `(sqrt(k) - 1) / 2` per side, so k=4 yields exactly the half-width
a 16:9 frame exposes. As a flat ceiling it inverted on any base larger than
itself, leaving nothing to spend and falling back to the 32 tile floor, so the
biggest factories got the smallest margins.

## What the game says about itself

A capture records prototype *names*, and a name means nothing on its own. The
viewer has to know what colour `vegetation-turquoise-grass-2` is, and whether
`kr-advanced-transport-belt` is a belt, and it cannot work either out. Both
answers exist only inside the running game: a mod ships as a zip in the mods
folder, so its prototypes are Lua the desktop side never executes.

So the mod writes them down. One file, `prototypes.json`, beside everything
else a capture produces:

    {
      "tiles":    { "grass-1": [55, 53, 11], ... },        map_color, as bytes
      "entities": { "transport-belt": [204, 161, 71], ... },
      "types":    { "kr-advanced-transport-belt": "transport-belt", ... },
      "reach":    { "kr-advanced-underground-belt": 30, ... }
    }

Colours are the ones Factorio paints its own map view with, which is the
palette a player already has in their head. Types are each entity prototype's
own `type`, verbatim and unfiltered: deciding mod-side which types are
interesting would only move the curated list from one side of the file to the
other, and the viewer is where the answer is wanted. `reach` is
`max_underground_distance`, asked only of the two types that have one.

Without it, supporting a mod means transcribing its prototypes into tables in
`registry.rs`, once per mod: Alien Biomes alone adds a couple of hundred tiles,
and Krastorio2 adds belt tiers, ores and pipes a viewer built around Wube's
names cannot see are belts, ore or pipes.

**Absent is normal.** Every capture older than the file has none, so the reader
folds every failure into "no file" and the viewer falls back on its built-in
colours and name lists. A missing section works the same way, which is what
lets a file from an older mod be used for what it does say. Unusable entries
are dropped one at a time rather than taking the file with them: one colour out
of range would otherwise discard all 364 good ones.

**Colours are 0..255 or 0..1, and the game does not say which.** Factorio
accepts either and distinguishes them by rule: if any component exceeds 1, the
whole colour is in 0..255. Prototypes mostly use the second form, base's own
tiles included (`grass-1` is `{55, 53, 11}`), and the runtime returns them as
written. `encode.color_bytes` applies that rule and clamps, the rule being a
convention the game does not enforce on a mod.

**An entity's colour depends on whose it is, which a prototype cannot know.**
`map_color` is what charting uses "if a friendly or enemy color isn't defined",
and the two are mutually exclusive per prototype: the ones defining `map_color`
(rails, trees, cliffs) leave the pair nil, and the rest carry it. Force belongs
to an entity rather than to the prototype this file is keyed by, so enemies are
picked out by type, which for a capture is exact: the only ones surviving
entity filtering are nests and worms.

**Rewritten when the loaded mods change**, not once per capture: a baseline
runs once per save and never again, so a file written only there cannot pick up
a mod added since. The condition is `script.active_mods`, stamped into
`storage` alongside the rest of the capture's state, prototypes being fixed at
load time. This mod's own version is in that stamp, which is what makes a
capture heal itself when a build that wrote the file wrongly is replaced.

`storage` rather than a module local, because Factorio re-runs the control
stage on every load: a local reading "already done this session" means once per
load, and this rebuilds a couple of hundred kilobytes of JSON in one tick, on
the tick already flushing events and sampling players.

**Which tiles are floor is asked the same way**, in the mod rather than written
down for the viewer, since it decides what a capture records rather than how it
draws. `encode.placed_floor_tiles` unions two properties, neither covering it
alone on a real 69 mod game: `items_to_place_this` finds 18 tiles,
`mineable_properties.minable` finds 41, and together they miss only the eleven
coloured refined concretes, which no item places and which report as not
minable. Those eleven are stated, along with the rest of the old list, as a
floor guaranteeing a capture cannot lose paving it used to record.

`space-platform-foundation` is why this matters and is not modded at all:
missing from the stated list, the tiles a player lays to grow a platform were
recorded as natural ground by the scan rather than as built, so a platform
appeared fully formed from its first frame. Aquilo's `foundation` had the same
problem.

The viewer resolves both halves at intern time, next to where it already
resolves colour, so a name costs one lookup rather than one per entity per
frame. Factorio's type vocabulary is fixed and small, so matching on it in
`registry.rs` is not the thing the name lists had to be replaced for, but it
does need care: a splitter can also be a `lane-splitter`, an `infinity-pipe` is
a pipe while a `heat-pipe` is not, and worms are the bare `turret` type while
the player's defences are `ammo-turret`, `electric-turret` and `fluid-turret`,
which is what makes classifying enemies by type safe. Exactly one name is still
worth knowing: `captive-biter-spawner` is a `unit-spawner` the player built.

## Ground is scanned, not captured

Natural ground is the one part of a capture that does not change. Entities need
a record per placement and placed floor needs one, but grass is grass for the
whole playthrough. So it is not recorded during play at all: a separate
unattended pass reads it from **one** save, afterwards, and writes one
`terrain_<surface>.stfr` per surface.

That ordering buys three things at once:

- **Nothing during play.** Ground was the expensive half of the baseline, the
  freeze a player actually feels. It is now entirely outside the game.
- **Nothing per frame.** A from-saves export wrote the same ground into every
  frame, since unlike entities it cannot be skipped as unchanged, so its cost
  was multiplied by the frame count.
- **The right area.** Running afterwards means the box can be drawn knowing how
  far the factory eventually reached, instead of guessing from how far it had
  reached when recording started.

Ground captured at baseline time covers wherever the factory stood then, so a
capture enabled on a starter base and played for hours leaves buildings
standing on nothing beyond that first box. Topping it up as the base grows
would need the covered area tracked in `storage`, a threshold to batch on, ring
geometry so each top-up writes only what is new, and a stall when one fires.

**What it gives up is ground since built over.** The query asks for everything
that is not placed floor, so water landfilled at hour three reads as landfill
at hour ten and its water is never recorded: replayed from the beginning, that
lake is a hole until the tick the landfill was laid.

The scan writes into the session folder named for the map seed, which is what
verifies it: a save from a different playthrough lands under a different
session and is refused rather than laying an unrelated landscape under
somebody's factory. The from-saves flow skips that check, the saves there being
the playthrough.

`terrain_<surface>.stfr` is what the viewer already looked for, discovered from
the directory independently of the frame files. Captures made before this carry
ground in their baseline and `replay::write_all_terrain` still projects it; a
scan overwrites that result.

## Write batching

Encoded entity records are accumulated in a Lua table and flushed to
`helpers.write_file` in blocks. Each call is a file append, so one call per
entity makes export time scale with syscall count rather than entity count.

## Live capture and replay

The second way to build a timelapse, and the finer-grained one. The mod
snapshots a save **once**, then logs only what changes; the Rust side
reassembles any moment by replaying that log over the baseline.

    <script-output>/save-timelapse/<session>/
        baseline.json                 tick + surfaces the baseline covers
        frame_<tick>_<surface>.stfr   the baseline itself, one per surface
        events_<start_tick>.stev      append-only, one segment per timeline
        players.jsonl                 optional, sampled player positions
        milestones.jsonl              optional, when each milestone was reached
        prototypes.json               this game's colours and prototype types

`<session>/baseline.json` is written last, so its existence means that
playthrough's baseline finished. It is the handshake: replay reads it to
learn which frame files to seed from.

`<session>` (an 8 digit hex folder name) identifies which playthrough these
files belong to. `script-output/save-timelapse/` is shared by every save that
ever turns capture on and `game.tick` restarts from 0 for each, so a bare tick
cannot tell two playthroughs' files apart. `control.lua`'s
`compute_session_id` hashes the map's terrain seed (`map_gen_settings.seed`),
which is stable across save and reload of one playthrough, differs across
playthroughs, and needs no new in-game UI to collect. A save name would, mods
having no API access to it.

`replay::discover_sessions` lists every session folder with a finished
baseline. `save-timelapse.exe` auto-picks the only one when there is just one
and otherwise asks which playthrough to build from.

A capture whose state predates the folder-per-session scheme has no session id
(`nil` in `storage.timelapse_capture`) and keeps writing the old untagged names
flat in the shared top-level folder until `/timelapse-reset-capture` clears its
state. The Rust side descends only into session subfolders, so a leftover flat
file sits inert.

The baseline is taken once per save, not periodically, and **synchronously** in
a single tick: the game freezes for its duration, tens of seconds on a ~375k
entity base. A repeating snapshot would have to avoid a stall on every run,
where a one-time freeze can simply take one. Factorio saves and quits only
between ticks, so the export cannot be caught half written by normal play; a
killed process retries on next load.

`baseline_tick` lives in `storage`, so it travels inside the save file and a
baselined save knows it. It is recorded only on completion, so a game saved and
reloaded midway starts over rather than leaving a truncated baseline that
replay would trust.

Nothing renders while a tick's Lua is still running, so a freeze inside one
tick cannot show progress. `request_baseline`/`perform_baseline` split across
two moments so the warning can be read: `request_baseline` prints an entity
count (`LuaSurface::count_entities_filtered`, cheap because it never builds the
array `export_surface` needs) and schedules `perform_baseline`
`BASELINE_WARNING_DELAY_TICKS` later, about two seconds, one tick being one
rendered frame and too brief to read.

Deleting `script-output` while keeping the save leaves `storage` claiming a
baseline that no longer exists, and the mod cannot check: `LuaHelpers` exposes
`write_file` and `remove_path` and nothing else, no read and no directory
listing. It can delete a path it knows it wrote, which is what `M.reset_capture`
(the `/timelapse-reset-capture` command and the panel's reset button) does,
removing this playthrough's session folder via `encode.session_dir` before
clearing `storage.timelapse_capture`.

### Per-surface exclusion and catch-up baselines

The in-game panel (`mod/gui.lua`, opened via a toolbar shortcut or
Control+Shift+T) excludes individual surfaces from recording, including planets
not yet visited: `game.planets` covers every planet prototype whether or not
its surface exists, alongside whatever is already in `game.surfaces`.

`storage.timelapse_excluded_surfaces` is an opt-out set, presence as a key
meaning excluded, so a new planet or platform records automatically the moment
it is created with no special casing. `log_event` checks it before anything
else, and `request_baseline`/`perform_baseline` skip an excluded surface, so
exclusion also skips that surface's baseline cost.

Un-excluding a surface that already has something on it needs a baseline of its
own at that moment, or its timelapse starts from empty.
`storage.timelapse_capture.baselined_surfaces` tracks which surfaces have ever
had one and at what tick, so `request_baseline`/`perform_baseline` mean
"baseline whatever included surfaces are not in this table", which covers the
first baseline, a reset and a later catch-up identically. A catch-up
(`M.on_surface_included`, when a checkbox flips from excluded to included)
exports only the newly eligible surfaces through `export.export_surfaces_to`,
one ordinary `frame_<tick>_<surface>.stfr` each and **no manifest write**:
`baseline.json` never changes after it is first written.

The Rust side finds a catch-up by scanning the session folder for
`frame_<tick>_<surface>.stfr` files `baseline.json` does not name
(`replay::discover_catch_up_baselines`), nothing else in a session folder being
named that way. `replay::run`'s tick-ordered walk applies one
(`World::load_baseline`) at the point its own tick is reached rather than
eagerly, so the surface is absent from every frame before that tick and fully
formed from it onward. Events logged for that surface before its catch-up tick
(logging starts the instant it is included, the snapshot landing
`BASELINE_WARNING_DELAY_TICKS` later) are dropped rather than deferred, the
snapshot taken after them already reflecting what they did.

Snapshotting periodically instead duplicates what the log already says: at
roughly 50 bytes per entity, a megabase snapshot every ten seconds writes
gigabytes an hour. The `save-timelapse-snapshot-seconds` runtime setting does
exactly that anyway, independent of live capture, for exercising the export
path during real play. It uses the incremental exporter rather than the
baseline's synchronous one, a freeze that is fine once not being fine every
interval.

### Event format

Also a custom binary format (`<session>/events_<start_tick>.stev`), written
incrementally as the player plays, so text formatting per construction event
would be a cost paid during real gameplay.

There is no upfront count of anything: the log is append only and grows for as
long as capture stays on, a plain forward stream of tagged records from the
magic to whatever the last flush wrote.

    magic   4 bytes, "STE1", written once when the segment is created
    version u8, MIN_SUPPORTED_VERSION through CURRENT_VERSION

    then a sequence of tagged records:
      tag 0     DefineName        string
      tag 1     DefineSurface     string
      tag 2     SetTick           u64 tick
      tag 3     AddEntity         u16 name_id, i32 x10, i32 y10, u8 d,
                                  u8 w, u8 h, u64 id, u16 surface_id
      tag 4     RemoveEntity      i32 x10, i32 y10, u64 id, u16 surface_id
      tag 5     AddTile           u16 name_id, i32 x, i32 y, u16 surface_id
      tag 6     RemoveTile        i32 x, i32 y, u16 surface_id
      tag 7     ResetDictionaries (no payload, version 2 and later)
      tag 128   RemoveName        varint len, then varint name_id
      tag >=128 Extension         varint len, then that many bytes

`DefineName`/`DefineSurface` work like the frame format's dictionaries, as two
separate dictionaries sharing one tagged stream. Surfaces get their own tag, a
save holding a handful of planets and platforms against a much larger set of
prototype names. `SetTick` is written once per distinct tick that has an event
rather than per record, a blueprint landing hundreds of entities in one tick;
`control.lua` writes one as the first record of a fresh segment, so a reader
never meets a data record before a tick.

There is no checksum trailer here, unlike the frame format. A segment is
abandoned rather than closed when a rollover starts a new one (see below), so
there is no "finished" moment to checksum against. `replay::run` skips and
warns on a segment that fails to open, which covers the same ground.

Extension records work as in the frame format, under the same rule. An
unrecognised tag used to fall through to `next` returning `None`, which an
iterator reports as end of stream, so a segment from a newer mod stopped the
replay partway through silently. `EventStream::unknown_extensions` counts what
was stepped over and `replay::run` sums it across segments.

**A removal can name what it is for.** A position holds at most a deposit and
the thing standing on it, and a removal carrying only a position resolves to
whatever is on top, which is the structure. Hand-mining the ore out from under
a machine therefore took the machine. `RemoveName` (tag 128) names the entity
the next `RemoveEntity` is for, written immediately before it.

An extension record rather than a field on `RemoveEntity`, the core layout
being frozen at version 3, so a tool older than the field skips it by its own
length and resolves the removal exactly as it always did. The mod writes it for
a resource only: nothing else can be the buried one, and `log_entity` already
reads the entity's type, so every other removal still reads only what it
writes. A name the capture never mentioned is a no-op rather than a fallback,
falling back to the top being the bug this closes.

`id` on `AddEntity`/`RemoveEntity` uses `0` for "no id", Factorio's
`unit_number` starting at 1. **`RemoveEntity` always carries position, even
with an id.** An entity present at baseline time has no id in replay's world
state, a snapshot recording no `unit_number`s, but Factorio still reports its
real id when it is later mined, so an id-only removal resolves nowhere and
silently no-ops, leaving the entity in the replayed timeline forever. Replay
tries the id first and falls back to position (see "World state" below), and no
shape of `RemoveEntity` omits position.

**A load starts a new segment.** Factorio re-runs all of `capture.lua`'s
top-level code on every load, which is how the mod knows a load happened at
all: `storage` rewinds with the save and cannot tell a fresh load from a long
session.

Resuming the segment a save names was tried and could not be made safe, for two
reasons. The file may not exist: deleting a capture cannot reach the saves that
describe it, so loading a save from before a reset appended to a file that was
gone and recreated it with no header, which a reader could only refuse. And the
name stopped being true, since a segment named for when it was first created
went on to hold events from a different branch entirely, while `log_segments`
bounds abandoned branches using exactly that number.

Starting fresh costs one file per load and makes both hold: every file gets its
header from whoever created it, and every filename is the tick its records
really begin at. A header is written rather than appended, so loading the same
save twice truncates the first attempt, which is a branch the player abandoned
by loading back to it.

Version 2 added a `ResetDictionaries` record (tag 7, no payload) for the
resuming case, where the writer's dictionaries restarted at id 0 while the file
already held every earlier define, so **every entity logged after a reload
decoded as whichever name was defined first**. Captures carrying it are still
read; nothing writes it now.

A segment with no header at all is accepted if it reads as events all the way
to the end, which recovers recordings damaged by the resuming behaviour before
it was removed. `Replay::headerless_segments` counts them.

Persisting the dictionary in `storage` would not work, for the same reason the
mod cannot detect a reload at all (below): `storage` rewinds with the save and
would describe fewer names than the file holds. A fresh segment per load would
not either, every counter available to name it also living in `storage`, so
loading one save twice reuses a filename and overwrites a sibling branch.

### Replay tolerates events it cannot apply

An add for something already present, or a remove for something never seen, are
no-ops rather than errors. The baseline runs inside one tick but is not atomic
against every other handler Factorio invokes in that tick, so a robot finishing
construction in exactly that tick can be both logged and already reflected in
what the baseline read.

`Replay::no_op_events` counts them: a trickle is normal, a large fraction means
the log and baseline came from different playthroughs.

Events are applied in whole-tick batches, so a frame is never cut halfway
through a tick and a blueprint landing 400 entities appears whole or not at
all.

A frame is pinned to its own tick rather than to whenever the next event
arrives. The two differ across a gap, a long stretch of research or walking
with nothing built: every frame boundary inside the gap has to be emitted
before the event that ends it is folded into the world, or all of them render
with that event's changes visible, showing something built at the end of a gap
as though it stood there from the start. `replay::run` flushes up to the tick
*before* each incoming event, not up to the batch it just applied.

### Reloading an earlier save

Going back in time is ordinary play: a player reloads to undo a mistake or
recover from a breach.

**The mod cannot detect it.** Both values a check would compare come out of the
save being loaded, the recorded tick living in `storage`, which Factorio
serializes into the save file: a save made at tick T restores a recorded tick
no greater than T while `game.tick` is exactly T. Anything durable enough to
survive a load rewinds with it, so the whole question is resolved on the
reading side.

The mod also cannot delete or truncate the segment it abandoned, the Lua
sandbox offering `write_file` and `remove_path` and nothing finer, and removing
the file would throw away the part that is still real history. So the abandoned
file keeps records for a future that never happened, and bounding it is the
Rust side's job.

`event::log_segments` does that. Each segment carries an `end_tick`, and events
at or past it are dropped, counted as `Replay::superseded_events` and reported
rather than warned about: a nonzero count is what a reloaded playthrough looks
like replaying correctly.

Two things make the bound correct.

Segments are ordered by **mtime, not by the tick in the filename**, start tick
not being chronological once a playthrough reloads twice. A segment is appended
to for exactly as long as it is the live one and never touched again after a
rollover abandons it, so segments finish being written in creation order.

A segment ends at the **smallest** start tick among all segments created after
it, not the next one's, so a second reload reaching further back also
invalidates part of the first reload's segment. A suffix minimum over the
creation order expresses that, and also keeps the result in ascending tick
order despite the mtime sort, since a segment's events all fall below its
`end_tick`.

Two segments sharing an mtime (a copied capture folder, or a filesystem too
coarse to separate two rollovers) fall back to ascending start tick, then to
the rollover sequence in the filename, stitching them together with overlaps
trimmed.

#### Reloading the *same* save twice

Loading the same save again resumes at exactly the tick the live segment
started at, so nothing distinguishes the second attempt from the first except
that the ticks jump backwards where it begins. `capture_segment_name` takes a
rollover sequence so two segments starting at the same tick get different
filenames (`events_<tick>_<seq>.stev`, the suffix omitted at 0).

Captures recorded before that have both attempts in one file, so
`event::segment_run_bounds` splits a segment into **append runs** wherever its
ticks jump backwards and bounds them by the same suffix minimum used across
segments: records within a file are in append order, which is chronological
order. A capture the current mod writes has exactly one run per segment, so the
two mechanisms compose rather than overlapping.

`Replay::restarted_segments` counts files holding more than one run, kept
separate from `superseded_events` because a segment corrupted by deleting
`script-output` by hand looks the same from the file alone. In practice that
corruption produces a header-less file that fails to open outright, counted as
`skipped_segments`.

A session spans several segment files by design, one per load, and a segment
that fails to open is treated the way a bad baseline surface is, a warning and a
skip, so one broken file costs only its own events.

#### Returning to a branch already left

Superseding is one directional, and deliberately so. Load an earlier save and
the branch you left is truncated, which is what keeps a timelapse showing one
coherent history. Load a save from that abandoned branch later and its history
is already gone: the timelapse jumps, and events after the return land on a
world built from the branch that replaced it.

Fixing it means never truncating, keeping every branch, and deciding at read
time which one the newest save descends from. Nothing in a save says which save
it came from. A tick is not enough, since two branches share every tick between
the divergence and the point they were abandoned at, and the mod cannot mark a
save with a lineage it would have to invent before knowing a branch existed.

So the cost is a permanently growing log, plus a guess at the one thing that
would make it usable. Truncating loses a branch somebody may come back to;
keeping everything loses the ability to say which history is the current one,
which is the whole promise. The first is the better trade, and a fresh baseline
from where the player actually is repairs it in one step.

### Timer handlers share one on_nth_tick per interval

Factorio keeps a single handler per `on_nth_tick` interval; registering a
second for the same interval silently replaces the first rather than erroring.
The capture flush runs every 600 ticks (10 seconds) and the independent
`save-timelapse-snapshot-seconds` setting is also given in seconds, so a value
of 10 there collides by coincidence. `control.lua` collects every interval a
setting wants into one table and chains handlers sharing an interval, rather
than each feature calling `on_nth_tick` for itself: see `sync_subscriptions`
and `set_interval_handlers`.

`on_tick` is outside that scheme. The mod registers exactly one, driving both
the headless-scan export and the periodic snapshot's incremental stepper
(`snapshot_step`) off two field checks, nothing else in the mod wanting
`on_tick`.

`snapshot_start`/`snapshot_step`, the incremental multi-tick exporter, has one
caller: the periodic `save-timelapse-snapshot-seconds` setting. The baseline
runs synchronously through `export_all_to` instead, the same function
`/timelapse-export` and the headless scan use.

### World state

Entities live in a slab with free-list reuse, indexed by position and by
`unit_number`. Baseline entities are loaded with no `unit_number`, a snapshot
recording none, so they are reachable only by position whatever id Factorio
later reports removing them by. Removal tries `id` first, an O(1) hit for
anything built after capture began, then falls back to position.

Position keys are scaled by **ten**, not two. Half-tile alignment covers most
entities but not all: `frame_0000.stfr` holds a
`logistic-train-stop-lamp-control` at x=326.9 beside its `logistic-train-stop`
at x=327.0, and keying on half tiles merges them, dropping five of that frame's
240 entities. One decimal is the precision positions are stored at on the wire
(see "Frame format" above).

**A position holds two entities, not one.** Factorio lets two things occupy one
position, and the pair that happens is a resource with something built on top.
Keying by position alone means an `AddEntity` evicts the ore and the following
`RemoveEntity` clears the position, so building across a patch eats it a tile
at a time and mining the building back never returns it. `Surface::under` holds
what a position had before something covered it, and removal promotes it back.

A second layer rather than a list per position, the depth needed being two: a
`Vec` per position allocates once per entity across hundreds of thousands of
them to express something that almost never happens. A third arrival displaces
whatever the second was hiding. Nothing in it knows what a resource is;
covering and uncovering is the behaviour whatever the two things are.

Only an exact key collision covers anything, so an even-sized building sits on
a tile corner and covers nothing while an odd-sized one covers the single tile
under its middle.

## What the tool remembers

Four things, in plain JSON under the user's own config directory
(`%APPDATA%\save-timelapse\settings.json` on Windows, the platform equivalent
elsewhere): where Factorio's folder is, where its executable is, seconds per
emitted frame, and whether to include natural terrain.

Not beside the executable, which is the obvious home for a tool shipped as a
zip and is exactly wrong: replacing the zip on update wipes it.

Three rules keep it from becoming a liability:

**Absent means never asked, not a default.** Every field is an `Option`, which
is what lets the first run explain itself once and never again, and why no
field carries a baked-in value.

**Remembered paths are validated, never trusted.** A Factorio folder that has
moved, been renamed, or lives on a drive that is not plugged in falls through
to auto-detection rather than failing confusingly downstream.

**Nothing it does is fatal.** A missing file is the first run. A corrupt file
is one warning and a fresh start, a tool that refuses to launch until you
delete a file you never knew about being worse than one that forgets your
preferences. A failed write is a warning too, costing one prompt next time.

Surface choice is deliberately **not** remembered. Which surfaces a capture has
changes as a playthrough reaches new planets, so a remembered answer picks a
stale one. The terrain choice is remembered but still asked every time, being a
cost decision rather than a preference; what is remembered is which way Enter
goes.

## Tile reverts

Removing a placed tile has to restore what it was covering. Mining landfill
should put the water back.

`RemoveTile` carries only a position, and nothing on the reading side can
recover what was underneath: a baseline taken while the landfill was down never
saw the water. Extending `RemoveTile` to carry the uncovered name is a core
layout change and off the table after the version 3 freeze.

The mod does it instead, with no new record type. `on_player_mined_tile` and
`on_robot_mined_tile` fire **after** the tiles are replaced, so `capture.lua`
reads `surface.get_tile` at that position and logs an ordinary `AddTile` for
what is now there, immediately after the `RemoveTile`. Applied in order, the
position ends up holding the revealed ground. The reader has never cared
whether an `AddTile` names placed floor or natural ground, so this needs no
format change and an older tool replaying a newer capture gets it for free.

Gated on the terrain capture setting: with terrain off the timelapse shows no
natural ground, and uncovering water under a removed tile would put some back.

## Only writing surfaces that changed

A playthrough builds on one surface at a time, so writing every surface at
every frame re-serializes the rest byte for byte unchanged. Measured on a real
nine-surface Space Age capture over 13 minutes of play at the 60s default,
**86% of files written were byte-identical to that surface's previous file, and
93% of the bytes were**; nauvis alone accounted for 219.7 MB of which 211.5 MB
was duplication, the player being on Gleba throughout.

`World` keeps a **per-surface revision counter**, bumped by every mutation that
changes that surface and nothing else, and `replay::write_all_surfaces` skips a
surface whose revision matches the one it last wrote. A counter rather than a
hash of the frame, the point being never to materialise the frame: hashing to
detect the duplicate means doing the expensive half of the work and discarding
it.

A spurious bump costs a whole redundant file, so `Surface::insert` checks
whether an add lands on exactly what is already there and leaves the revision
alone if so. Those are not rare: the baseline smear, a snapshot taken slightly
after the events describing the same construction, produces them by design.

**The floor is written once, then only when it moves.** A frame is a full
snapshot, so on a paved base it carries the entire floor every time. Measured on
a real 660 frame Space Age capture: 6.3 MB of floor against 2.1 MB of entities
per frame, and across the whole run the tile count changed by 3.3% and the
entity count by 0.4%. That is 5.5 GB of which roughly 96% was the same data
written again.

`Surface` therefore keeps a second counter for the placed-floor layer alone, and
a frame whose floor has not changed writes a `FloorUnchanged` record in place of
its tile section. The reader carries the previous floor forward, which costs one
pass over open spans rather than a walk over millions of tiles: `SpanBuilder`
already had `push_repeats` for restoring skipped frames, and this is the same
primitive applied to one layer.

Frames that omit a floor declare format version 4, and only those frames do. A
build that never omits one stays readable by an older viewer. The version rather
than a skippable extension, because an older reader stepping over the marker
would see an empty tile section and draw a factory standing on nothing, which
looks like a bug rather than a missing feature. The mod never writes it, so
captures stay at version 3 and every recording already on disk is untouched.

**The gap in the numbering is the record.** Files stay named
`frame_<index>_<surface>.stfr` against a global frame index, so a surface that
did not change has no file at that index: no format change, no naming change,
no sidecar to keep in sync. `write_all_surfaces` has always skipped a surface
with nothing on it, so the viewer already groups by surface into independently
ordered timelines.

The viewer puts the omitted frames back. `loading::timeline_ticks` takes the
union of every surface's ticks, the set of moments the export covers and not
readable off any single surface, and the loader fills each surface's gaps
against it with `SequenceBuilder::push_repeats`. `Timeline` is
index-addressed, so every surface has to agree on how many moments there were
or switching surfaces would scrub at a different rate.

Restoring a gap costs **one pass over what is standing per gap, not per
frame**. Nothing changed across the gap by definition, so every span open when
it started is still open when it ends and each one's `last` jumps straight to
the far side: on a megabase surface idling through a long stretch, one walk
over ~900k spans instead of dozens. The frame itself was never written, read or
parsed.

`render_frame.rs` asserts the equivalence this rests on: a sequence built with
gaps and repeats is identical, index for index and tick for tick, to one built
from an export that wrote every frame.

## Milestones

Three moments worth marking on a timeline: the first time each science pack is
produced, the first rocket launch, and each planet reached. Both capture paths
produce the same `milestones.jsonl` (`{"tick":T,"kind":K,"id":I}` per line), so
the viewer cannot tell which one built a timelapse. They arrive by different
routes, the two paths having different evidence available.

### Live capture watches them happen

`mod/milestones.lua` polls on the capture flush that already runs every few
seconds. Science is polled rather than evented, the game exposing no "an
assembling machine finished an item" event: `on_player_crafted_item` covers
hand crafting only, which is not how science gets made past the first hour, so
production statistics are the only source. A planet counts as reached when a
player stands on a surface with `planet` set, swept over connected players
rather than hooked to `on_player_changed_surface`, since nobody changes surface
to arrive on Nauvis at the start.

Every milestone fires once, tracked in `storage.timelapse_milestones`, a
sibling of `storage.timelapse_capture` rather than nested inside it so a
capture reset wipes both together. The reset deletes the file recording them,
and a surviving key would believe every milestone had already fired.

Ticks from this path are exact.

### From saves, they are recovered by comparison

A save knows that a science pack has been produced, never when it first was, so
the mod reports **state** rather than events: `export.milestone_state` collects
the science packs with nonzero production, the inhabited planet surfaces, and
`force.rockets_launched`, and `export_all_to` writes them into the per-save
manifest. They ride in the manifest rather than a file of their own, describing
the same instant it describes; being JSON, an older reader ignores the field,
which is what lets `baseline.json` carry it too without disturbing live
capture.

`milestone::from_saves` sorts the per-save states by tick and walks them,
emitting a milestone the first time each id appears. Rockets are carried as a
count rather than a flag so that walk can tell a first launch from launches
already happening before the earliest save.

Two consequences, both inherent:

**Precision is bounded by save cadence.** The earliest tick at which something
can be proved to have happened is the tick of the first save reporting it, so
that is the tick used.

**An established base opens with a cluster.** Everything already true in the
earliest save is emitted at that save's tick, having genuinely happened at or
before then. Live capture switched on mid-playthrough does the same, its first
poll recording every pack already produced.

"Planet reached" uses `is_inhabited` rather than surface existence, the game
creating a planet's surface before anybody goes there. That also keeps the
marker aligned with the timelapse: a surface appears in frames once inhabited,
so a planet is marked reached exactly when it starts being shown.

## Rendering

The viewer converts each parsed `Frame` into a `RenderFrame` at load time and
drops the parsed form. Two things happen in that conversion.

**Names are interned.** A real base has tens of distinct prototype names
against hundreds of thousands of entities, or millions of tiles on a fully
paved one, so `TypeRegistry` maps each name to a `u16` once and resolves both
its colour and what it is at the same time (see What the game says about
itself). Drawing then never hashes a name, where the pre-registry loop called
`color_for` (FNV over the name) and `sprites.get(&e.n)` (SipHash over the name)
per entity per frame. `Frame`'s `n` field is `Arc<str>` rather than `String` for
the same reason one level earlier: the wire format's name dictionary means
resolving a record's name during parsing is a refcount bump rather than a fresh
allocation and copy of one of ~58 repeated strings, which dominated the cost of
parsing a megabase frame.

**Items are grouped into per-type runs** by counting sort, so all entities of
one type sit contiguously and a `Run` names the span. That is what keeps the
GPU batch intact: macroquad merges geometry only into the *immediately
preceding* draw call and starts a new one whenever the bound texture changes
(`quad_gl.rs::geometry`), so drawing in export order, which interleaves types,
costs close to one draw call per entity. Untextured rects count as their own
texture state, so mixing shapes and sprites breaks the batch the same way.

Measured on `tests/fixtures/frames` with `cargo run -p viewer bin drawcalls`,
for fully-visible frames:

    items    types    export order    grouped    grouped, raised capacity
    22,971      58          10,427         72                          59
    37,077 (all five)       17,868        205                         187

The second lever is macroquad's batch capacity. Its default
`draw_call_index_capacity` of 5,000 caps a draw call at 833 quads, so even
perfectly sorted output pays a draw call per 833 items. The viewer raises it
via `Conf`: barely visible at fixture scale, where most runs are under 833, but
at 500,000 entities it is 606 draw calls against 126.

Capacity cannot go far higher. Indices are `u16` and get offset by the running
vertex count, so vertex capacity above 65,536 corrupts geometry, and macroquad
allocates one GPU buffer of this size per draw call it has ever used.

`DrawCallCounter` models the batching rule above so the viewer can report its
real draw-call count in the diagnostics overlay behind `F3` (see Viewer
chrome). macroquad's own `telemetry::drawcalls` is not usable for this:
`track_drawcall` allocates a 128x128 render texture per call.

Culling happens in world space, before the world-to-screen transform, so a
culled item costs two comparisons rather than a transform plus a screen-bounds
test.

## Viewer chrome

Everything drawn on top of the world lives in `viewer/src/chrome.rs`. One rule
decides what is allowed there: **an element earns its place by answering where
am I, when am I, or what can I do.** Anything answering *how is the renderer
doing* goes behind `F3`, which is where both diagnostic readouts live, along
with `s` (sprites off) and `l` (LOD off): those are A/B tests for texture
binding and per-item CPU cost, and pressing either by accident makes the
factory render wrongly with no visible reason why.

### Geometry is computed once and read twice

`Chrome::layout` positions every chip and button once per frame. `draw` and
`hit` both read those rects and neither recomputes anything, which is what
keeps a control from drifting away from the region that activates it.

Text width and window width are the only things layout needs from macroquad, so
`Chrome::layout_with` takes them as arguments and `Chrome::layout` is a wrapper
supplying them from the `Ui` and the window. That is what lets a test lay the
chrome out and assert every rect answers a click at its centre.

Layout runs *before* input is polled. Afterwards would test this frame's click
against last frame's rects, which is invisible except on the frame a window is
resized.

### The font

macroquad bundles exactly one face, ProggyClean, a bitmap font that works and
looks it. Anything else has to be loaded, so `font_candidates` walks a chain:
`ui-font.ttf` beside the executable, then the platform's own UI font (Segoe UI,
or DejaVu and Liberation on Linux), then `None` for the built-in one.

No font is committed. The system face is already on every machine and carries
no redistribution obligation, and a release wanting one exact face can ship
`ui-font.ttf` beside the binary without this code changing.

### The active surface is a fill, not a brighter label

Chrome is painted onto the rendered world, which is dark grass in one place and
a white space platform in the next, so brightness is not a reliable signal: the
gap between white and a dimmed label disappears over bright terrain. A filled
pill paints its own ground and reads identically on both. Brightness is left to
carry hover, where being wrong for one frame costs nothing.

That also sets the weight of the inactive chips, the only dim thing painted
onto the factory with no fill behind it: at the 55% used elsewhere they
disappear over a screen of machine icons, so they sit at 72%, as bright as they
can be while still losing to the active chip.

Chips keep their natural order, reordering on every switch would rearrange the
row under the cursor. Surfaces that do not fit collapse into a `+N more` chip,
which matters beyond the five planets because a player can have arbitrarily
many space platforms. The exception is an active surface that did not fit,
swapped in for the last visible chip.

### Transport controls sit in the gutters

The bar is 60% of the window and centred (`Timeline::for_screen`), so the
gutters either side are dead space. The column above the bar is not: the
activity graph stands on it, the playhead label clears the graph, and the hover
tooltip clears the label, all derived from each other so they move together.

On a narrow window the left cluster degrades in tiers rather than overflowing:
speed pill first, then the step buttons, leaving play alone. Everything dropped
still has a keyboard equivalent, listed in the `?` panel.

### Two ordering constraints in the draw loop

**Surface switches take effect one frame later.** A click on a chip arrives
while `worlds[current]` is still mutably borrowed by the loop body, so
`apply_chrome_click` returns the index and the loop applies it at the top of
the next iteration. That is 16ms and invisible.

**A press on chrome is latched, not tested continuously.** `on_chrome` is set
when the button goes down and read for the rest of the drag, so a drag starting
on a control and wandering off it does not become a camera pan halfway through.

### The controls panel shows itself once

`first_run` writes a `seen-controls` marker beside the tool's own
`settings.json`, and the `?` panel opens by itself the first time the viewer is
run. Everything else here is on request, but a `?` in the corner only helps
somebody who already suspects there is something to find.

The marker is a sibling file rather than a field in `Settings`, which holds
only answers a user would otherwise retype, and deleting it is an obvious way
to get the panel back. A failed write leaves the panel appearing again next
launch: one that reappears is a nuisance, one that never appears leaves a
first-time viewer with nothing to go on.

## Loading

Frames are parsed with the window already open, yielding to draw a progress bar
roughly every 33ms. Without that the viewer shows an empty window for as long
as parsing takes, many seconds on a real save set. The bar covers three phases:
reading frames, converting them, and loading sprites. Sprites are loaded once
up front rather than on first use, which would stutter scrubbing the first time
a not-yet-seen type appears.

Reading and parsing each `.stfr` file is independent work, nothing being
shared until conversion, which needs one consistently numbered `TypeRegistry`
across every frame, so `ParallelFrameLoad` spreads it across every available
CPU core. It runs on its own OS thread rather than blocking the caller, so the
async render loop keeps polling and drawing the progress bar. Measured on a
real ~300k entity, 3.1M tile, 55 frame capture: parallel reading plus switching
`Entity`/`Tile` names from `String` to `Arc<str>` (see "Rendering" above) took
total load time from 47s to 20s.

**Multiple surfaces load as separate worlds.** `group_by_surface` splits a
loaded batch by each frame's parsed `surface` field, not its filename, into one
independently ordered timeline per surface rather than collapsing to whichever
is busiest. That is what the viewer's `tab` key switches between. Each world
keeps its own `Camera`, fitted to its own frames at load time, so switching
away and back does not disturb either one's pan and zoom.

## Exporting a video

    growing bounds ─> camera fit ─> draw offscreen at (w·N, h·N)
                                          │
                          get_texture_data()   RGBA, bottom up
                                          │
                          downsample()         box average N·N ─> w x h
                                          │
                          encode_jpeg()        flip rows, RGB, quality 85
                                          │
                          AviWriter::add_jpeg()    one "00dc" chunk
                                          │
                          finish()             index, then patch four sizes

Each frame is encoded and appended as it is drawn, so memory stays at one
frame however long the timelapse runs.

### Motion JPEG in an AVI, written by hand

`viewer/src/avi.rs` is a few hundred lines of laying out headers, which is the
point: the promise is that you run the executable and that is it, nothing to
install. Linking or shelling out to ffmpeg would break that for the one
feature most likely to be used by somebody who just wants something to post.

MJPEG is "every frame is a complete JPEG", so there is no inter-frame
prediction, no bitrate control and no encoder state to get wrong, and every
mainstream player reads it. The cost is size, since the file is roughly the
sum of its frames. That is only the right trade because a timelapse is flat
colour on a dark background, which JPEG compresses hard.

Four fields cannot be known until the last frame lands: the RIFF size, the
`movi` list size, and the frame count, which appears in **two** separate
headers. They are written as zero and patched by seeking back, which is why
`AviWriter::create` takes a path rather than a writer.

Three things that break a file quietly rather than loudly, each with a test:
RIFF chunks are word aligned, so an unpadded odd-length frame shifts
everything after it and the file stops parsing partway; index offsets are
relative to the `movi` fourcc, and getting that wrong still plays but breaks
seeking, the one thing an index is for; and `dwSuggestedBufferSize` is
nominally advisory but some decoders size their input buffer from it and
truncate anything larger.

The tests parse the file the way a player does, following chunk sizes, rather
than reading fixed offsets. A size wrong by one is exactly the bug worth
catching, and a fixed-offset assertion sails straight past it.

### Which way up

Row order has to be declared in two places and they have to agree; either one
alone just moves the flip.

A render target reads back bottom up, the way OpenGL stores it. macroquad's
`Image::export_png` undoes that itself, so the PNG sequence is right without
help and the JPEG path, which touches `bytes` directly, reverses rows as it
encodes. The other half is `biHeight` in the `BITMAPINFOHEADER`, which carries
row order **in its sign**: positive means bottom up, the DIB convention;
negative means top down, which is what these frames are. Declaring the truth is
also the more compatible choice, a player that ignores the sign treating MJPEG
as top down anyway.

### Full detail, supersampled

An export renders at `N` times the requested size and averages down.

At 1080p a 2,900 tile base puts about three tiles behind every output pixel, so
at one sample per pixel whichever entity a pixel lands on wins it outright and
the other two tiles are gone, and which one wins changes frame to frame as the
camera creeps. A box filter is the correct kernel rather than the cheap one:
the samples cover exactly one output pixel's area, so their unweighted mean is
that pixel's coverage. Bicubic would reconstruct a continuous signal, and the
source is flat quads with hard edges.

That is also what makes **chunk LOD wrong for an export**. LOD keeps a live
frame rate up by not submitting items too small to perceive, and an export has
no frame rate to protect. A cell keeps only its dominant type, so at these
zooms a paved area swallows every belt running through it. Full detail is sound
only because of supersampling: at one sample per pixel the items it restores
are sub-pixel and alias into speckle.

Measured on a real 860k entity base, 41 frames at 1080p: 4x supersampling costs
about 3% more wall clock than 2x, the bottleneck being entity submission rather
than fill rate or the downsample. The default is 2, a 7680x4320 readback being
132 MB per frame.

### Culling is against the render surface, not the window

`view_bounds` derives the surface size from `screen_center * 2.0` rather than
from `screen_width()`/`screen_height()`, which are the **window's**
dimensions. Those are the same thing only while drawing to the window: an
export renders into an offscreen target of whatever size was asked for, so
culling to the window throws away everything outside a window-sized corner of
it, cropping a 1080p export to its top-left two thirds at a 1280x800 window.

Every caller already builds `screen_center` as exactly half the surface it is
drawing to, so it is the same number by a route that cannot go stale, and it
takes the last window global out of the draw path.

## Concurrency

Saves are independent. Each export gets its own staged directory and Factorio
process, so scanning parallelises across saves with no shared state.

Note that the CLI does not yet exploit this: `main.rs` exports saves in a
sequential loop, and each iteration launches Factorio and waits for it.
