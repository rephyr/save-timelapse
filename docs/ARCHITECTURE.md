# Architecture

## Components

    mod/     Lua mod loaded by Factorio. Exports entity data in a custom binary format.
    src/     Rust CLI. Drives Factorio, collects exports, renders output.

The two never communicate directly. Factorio's Lua sandbox cannot read files,
open sockets, or spawn processes, so the mod's only output channel is writing
into `script-output`. The CLI reads that directory after the process exits.

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
binary format, not JSON. The mod exports during real gameplay (the baseline
snapshot alone has been measured at tens of seconds of frozen play on a
~375k-entity base), so this is the one file whose write cost genuinely
mattered: JSON text formatting and punctuation for every single entity was
the actual CPU and I/O cost of a big export, not something incidental to it.

Wire format, all integers little endian:

    magic     4 bytes, "STF1"
    tick      u64
    surface   string (u16 length, then that many UTF-8 bytes)
    entity section, a sequence of:
      tag 0  DefineName    string
      tag 1  EntityRecord  u16 name_id, i32 x10, i32 y10, u8 d, u8 w, u8 h
    tag 9  EndEntities (no payload), marking the start of the tile section
    tile section, a sequence of, running to end of file:
      tag 0  DefineName    string
      tag 2  TileRecord    u16 name_id, i32 x, i32 y

`DefineName` writes a prototype name the first time it is used and gives it
the next sequential id; every later reference is just the two byte id, which
is what lets a base with hundreds of thousands of entities but only a few
dozen distinct prototype names avoid repeating `"transport-belt"` per entity.
One dictionary is shared by the entity and tile sections of a file, since a
name only needs defining once.

There is deliberately no entity or tile count anywhere in this format. Both
counts would be free to compute upfront for the single tick synchronous
export path (`find_entities_filtered` already returns a full array), but the
periodic incremental exporter (`snapshot_step` in control.lua) spreads one
export across many ticks specifically so no single tick has to do the whole
thing, with real play still running in between: an entity a batch has not
reached yet can be mined by the player before its turn comes, so a count
taken upfront could still be wrong by the time writing finishes, and scanning
the whole list once just to learn it would reintroduce the very stall the
incremental exporter exists to avoid. `EndEntities` sidesteps needing a count
at all: each section is a plain forward stream, and the tile section simply
runs until the file ends.

Entity coordinates are stored as position times ten, rounded to the nearest
integer (the same fixed point scale `world.rs::pos_key` keys positions by on
the Rust side, and exactly the precision entities are aligned to). Tile
coordinates are already integers, stored as is: a tile named at `(x, y)`
occupies world space `[x, x+1) x [y, y+1)`, corner rather than center
anchored like entities. `d`, `w` and `h` are always present now rather than
omitted at their default (0 direction, 1x1 footprint): once a record is this
compact, a variable width encoding to skip a default value costs more
complexity than the bytes it would save.

`tiles` covers placed floor (concrete, stone path, hazard/refined concrete
variants, landfill), a short, stable include list, the opposite of entity
filtering's exclude list, since natural terrain vastly outnumbers placed
floor types.

A surface is exported when it is nauvis or contains at least one entity owned
by the player force. A manifest listing exported surfaces accompanies each
set. Unlike the frame body, the manifest (`frame_<tick>_manifest.json`) and
`baseline.json` stay plain JSON: they hold one record per *surface*, not per
entity, so they're tiny, written once, and worth keeping human readable for
debugging the handshake between the mod and the Rust side.

## Entity filtering

Filtering happens in the `find_entities_filtered` query rather than in Lua, so
excluded types are never returned across the API boundary.

Excluded by default: characters, corpses, particles, projectiles, trees, rocks,
cliffs, fish, fire, smoke, explosions, ghosts, dropped items, combat robots,
streams, stickers and beams. Also excluded: biters, spitters and their
spawners (`unit`, `unit-spawner`)  wildlife rather than factory, and without
this a live-capture log fills with combat-death removals indistinguishable
from the player mining something. Confirmed against a real capture, where
these types were ~6% of exported entities. Worm turrets are left in: they
share Factorio's `turret` type with player-built turrets, and filtering them
out would mean matching by name instead of type, which risks catching a real
player entity by pattern rather than excluding by what it actually is.

Resource entities are excluded unless `save-timelapse-include-resources` is set.
Every ore tile is a separate entity and they typically outnumber built entities
while carrying no information about factory growth.

## Write batching

Encoded entity records are accumulated in a Lua table and flushed to
`helpers.write_file` in blocks. Each call is a file append, so one call per
entity makes export time scale with syscall count rather than entity count.

## Live capture and replay

The second way to build a timelapse, and the finer-grained one. The mod
snapshots a save **once**, then logs only what changes; the Rust side
reassembles any moment by replaying that log over the baseline.

    <script-output>/save-timelapse/
        baseline.json                  tick + surfaces the baseline covers
        frame_<tick>_<surface>.stfr    the baseline itself, one per surface
        events_<start_tick>.stev       append-only, one segment per timeline

`baseline.json` is written last, so its existence means the baseline finished.
It is the handshake: replay reads it to learn which frame files to seed from.

The baseline is taken once per save, not periodically, and **synchronously**
in a single tick — the game visibly freezes for its duration (measured on a
~375k entity base: tens of seconds). That trade is deliberate: a repeating
snapshot would need to avoid a stall on every run, but a baseline runs at most
once per save, so a one-time freeze beats a background cost smeared across
the next several minutes of play that a save or quit could interrupt and
force to restart. Factorio can only save or quit between ticks, never
mid-tick, so the export cannot itself be caught half-written by normal
play — only a killed process could, and that just retries on next load (see
below).

`baseline_tick` lives in `storage`, so it travels inside the save file — a
save that has been baselined knows it. It is recorded only on *completion*,
so a game saved and reloaded midway simply starts over rather than leaving a
truncated baseline that replay would trust.

Because nothing renders while a tick's Lua is still running, there is no way
to show a live progress bar for a freeze that happens entirely inside one
tick. `request_baseline`/`perform_baseline` split the work across two ticks
purely so a warning can be seen at all: `request_baseline` prints an entity
count (`LuaSurface::count_entities_filtered`, cheap since it never builds the
array `export_surface` needs) and sets a flag, then `on_tick` runs
`perform_baseline` the *next* tick. Printing and freezing in the same tick
would put the warning and the "finished" message on screen at the same
moment, after the freeze already ended.

That same persistence is a trap if the *output* is deleted rather than the
save: `storage` survives independently of `script-output`, and the mod has
no way to notice the mismatch. Checked against Factorio's own
`runtime-api.json`: `LuaHelpers` exposes `write_file` and `remove_path` and
nothing else, no read and no directory listing, so a mod genuinely cannot
ask "is the file I wrote still there." Deleting `script-output/save-timelapse`
by hand leaves `baseline_tick` set, `request_baseline` keeps returning early,
and nothing gets rewritten — the reported symptom was live capture producing
an `events_*.stev` (which doesn't check `baseline_tick` at all) with no
baseline files alongside it. `/timelapse-reset-capture` is the manual
equivalent of the detection the mod cannot do automatically: it clears
`storage.timelapse_capture` outright, so the next check retakes the baseline
and starts a fresh event segment.

Snapshotting periodically instead would be pure duplication of what the log
already says: at roughly 50 bytes per entity, a megabase snapshot every ten
seconds writes gigabytes an hour. A separate `save-timelapse-snapshot-seconds`
runtime setting does exactly that anyway, independent of live capture, for
exercising the export path during real play — but for a *repeating* export
the freeze that's fine to accept once is not fine to accept every interval,
so that path stays incremental (see below) rather than sharing the baseline's
synchronous one.

### Event format

Also a custom binary format (`events_<start_tick>.stev`), for the same reason
as the frame format: this is written incrementally, live, as the player
plays, so the cost of formatting and re-parsing text for every single
construction event is a cost paid during real gameplay, not just at export
time.

Unlike the frame format there is no upfront count of anything: the log is
append only and grows for as long as capture stays on, so it is a plain
forward stream of tagged records from the magic to whatever the last flush
wrote.

    magic 4 bytes, "STE1", written once when the segment is created

    then a sequence of tagged records:
      tag 0  DefineName    string
      tag 1  DefineSurface string
      tag 2  SetTick       u64 tick
      tag 3  AddEntity     u16 name_id, i32 x10, i32 y10, u8 d, u8 w, u8 h,
                           u64 id, u16 surface_id
      tag 4  RemoveEntity  i32 x10, i32 y10, u64 id, u16 surface_id
      tag 5  AddTile       u16 name_id, i32 x, i32 y, u16 surface_id
      tag 6  RemoveTile    i32 x, i32 y, u16 surface_id

`DefineName`/`DefineSurface` name dictionaries work the same way as the frame
format's, just as two separate dictionaries sharing the same tagged stream
(surfaces get their own tag, `1`, since a save has only a handful of planets
and platforms, not worth spending a `DefineName`-sized dictionary entry
alongside the much larger set of prototype names). `SetTick` is written once
per distinct tick that has at least one event, rather than on every record,
since many events (a blueprint landing hundreds of entities) usually share a
tick; `control.lua` always writes one as the very first record of a fresh
segment, so a reader never has to handle a data record before any tick is
known.

`id` on `AddEntity`/`RemoveEntity` uses `0` to mean "no id" (Factorio's
`unit_number` is documented to start at 1, and `control.lua` already
tolerates an entity with none). `RemoveEntity` always carries position, even
when `id` is present too. The JSON-era format used to send id alone whenever
one was available, on the reasoning that `unit_number` is unique game-wide
and unambiguously locates the target. That reasoning had a hole: an entity
that already existed when the baseline was taken has no id in replay's world
state (a snapshot records no `unit_number`s), but Factorio still reports its
*real* id, the one it was assigned whenever it was originally built, when
that entity is later mined or destroyed. Replay had never registered that id
anywhere, so the removal resolved nowhere and silently no-opped. The entity
never disappeared from the replayed timeline. `id` is kept alongside position
now as a fast-path hint rather than the only key: replay tries it first, and
falls back to position when the id isn't one it recognizes (see "World state"
below). The wire format now makes this structurally impossible to get wrong
in the old way: there is no shape a `RemoveEntity` record can take that omits
position, unlike the old JSON line where `id` alone was a valid message.

A segment file can be resumed across a save reload: Factorio re-runs all of
`control.lua`'s top-level code on every load, which resets the in-memory name
and surface dictionaries to empty. If play resumes appending to a segment
file that already has earlier `DefineName`/`DefineSurface` records in it from
before the reload, this session has no way to read those back (see "That same
persistence is a trap" above for why not), so it may redefine a
handful of names it cannot know were already defined. That's harmless, not a
corruption: the reader always assigns ids purely by encounter order in the
file, so a name defined twice just gets two ids that both resolve to the same
string, at the cost of a few dozen redundant bytes, not a wrong replay.

### Why replay is forgiving

The baseline runs inside one tick, but that is not quite the same as being
atomic with respect to every other event handler Factorio invokes during that
same tick — a robot completing construction or a biter dying to a turret in
the exact tick the baseline runs could in principle be logged as an event
while also already reflected (or not) in what the baseline read. This window
is now a single tick wide rather than the multi-minute one an incremental
baseline would leave open, so in practice it is vanishingly rare, but replay
does not depend on it never happening: an add for something already present,
or a remove for something it never saw, are both no-ops rather than errors.

That costs nothing and removes an entire class of edge case for free, so it
stays even though the baseline it was originally built to cover no longer
smears. `save-timelapse-replay` reports the no-op count: a trickle is normal,
but a large fraction means the log and baseline came from different
playthroughs.

Events are applied in whole-tick batches, so a frame is never cut halfway
through a tick — a blueprint landing 400 entities appears whole or not at all.

### Timer handlers share one on_nth_tick per interval

Factorio keeps a single handler per `on_nth_tick` interval; registering a
second one for the same interval silently replaces the first rather than
erroring. The capture flush runs every 600 ticks (10 real seconds), and the
independent `save-timelapse-snapshot-seconds` test setting is also given in
seconds, so a value of 10 there collides with it by coincidence, not by doing
anything unusual. `control.lua` collects every interval a setting wants into
one table and chains handlers that share an interval, rather than each
feature calling `on_nth_tick` for itself — see `sync_subscriptions` and
`set_interval_handlers`.

`on_tick` itself is not part of that scheme: the mod registers exactly one
`on_tick` handler, unconditionally, which drives both the headless-scan export
and the periodic test-snapshot's incremental stepper (`snapshot_step`) off two
field checks. There is nothing to collide with, since nothing else in the mod
wants `on_tick`.

`snapshot_start`/`snapshot_step` (the incremental, multi-tick exporter) now
has exactly one caller: the periodic `save-timelapse-snapshot-seconds` test
setting. The baseline used to share it, but runs synchronously instead (see
"Live capture and replay" above) — a single-tick export via `export_all_to`,
the same function `/timelapse-export` and headless scan use.

### World state

Entities live in a slab with free-list reuse, indexed by position and by
`unit_number`. Baseline entities are loaded with no `unit_number` (a snapshot
records none), so they are never reachable by id no matter what id Factorio
later reports removing them by — only position resolves them. Replay's
removal handling reflects this: try `id` first (an O(1) hit for anything
built after capture began, since its add event registered the same id),
then fall back to position, which is what makes a baseline-original entity's
removal work at all.

Position keys are scaled by **ten**, not two. Half-tile alignment covers most
entities but not all: `frame_0000.stfr` holds a
`logistic-train-stop-lamp-control` at x=326.9 beside its `logistic-train-stop`
at x=327.0. Keying on half tiles merged them and silently dropped five of that
frame's 240 entities. One decimal is exactly the precision positions are
stored at on the wire (see "Frame format" above).

## Rendering

The viewer converts each parsed `Frame` into a `RenderFrame` at load time and
drops the parsed form. Two things happen in that conversion.

**Names are interned.** A real base has tens of distinct prototype names
against hundreds of thousands of entities (or millions of tiles on a fully
paved one), so `TypeRegistry` maps each name to a `u16` once and resolves its
color at the same time. Drawing then never hashes a name: the pre-registry
loop called `color_for` (FNV over the name) and `sprites.get(&e.n)` (SipHash
over the name) for every entity on every rendered frame. `Frame`'s `n` field
is `Arc<str>` rather than `String` for the same reason, one level earlier:
the wire format's own name dictionary (see `frame.rs`) means resolving a
record's name during parsing only needs a refcount bump, not a fresh
allocation and copy of one of those same ~58 repeated strings, which was the
dominant cost of parsing a real megabase frame before it was changed.

**Items are grouped into per-type runs**, by counting sort, so all entities of
one type sit contiguously and a `Run` names the span. This is what keeps the
GPU batch intact. macroquad merges geometry only into the *immediately
preceding* draw call, and starts a new one whenever the bound texture changes
(`quad_gl.rs::geometry`), so drawing in export order — which interleaves types
— costs close to one draw call per entity. Untextured rects count as their own
texture state, so mixing shapes and sprites breaks the batch the same way.

Measured on `tests/fixtures/frames` with `cargo run -p viewer bin drawcalls`,
for fully-visible frames:

    items    types    export order    grouped    grouped, raised capacity
    22,971      58          10,427         72                          59
    37,077 (all five)       17,868        205                         187

The second lever is macroquad's batch capacity. Its default
`draw_call_index_capacity` of 5,000 caps a draw call at 833 quads, so even
perfectly sorted output pays a draw call per 833 items. The viewer raises it
via `Conf`. That barely shows at fixture scale, where most runs are under 833
anyway, but at 500,000 entities it is 606 draw calls against 126.

Capacity cannot go far higher: indices are `u16` and get offset by the running
vertex count, so vertex capacity above 65,536 corrupts geometry, and macroquad
allocates one GPU buffer of this size per draw call it has ever used.

`DrawCallCounter` models the batching rule above so the viewer can report its
real draw-call count in the HUD. macroquad's own `telemetry::drawcalls` is not
usable for this: `track_drawcall` allocates a 128x128 render texture per call.

Culling happens in world space, before the world-to-screen transform, so a
culled item costs two comparisons rather than a transform plus a screen-bounds
test.

## Loading

Frames are parsed with the window already open, yielding to draw a progress
bar roughly every 33ms. Without that the viewer shows an empty window for as
long as parsing takes, which on a real save set is many seconds. The bar
covers three phases: reading frames, converting them, and loading sprites,
since sprites are loaded once up front rather than on first use — otherwise
scrubbing stutters the first time a not-yet-seen type appears.

Reading and parsing each `.stfr` file is independent work with nothing shared
until conversion (which needs one consistently numbered `TypeRegistry` across
every frame), so `ParallelFrameLoad` spreads it across every available CPU
core instead of one file at a time. It runs on its own OS thread rather than
blocking the caller, so the async render loop can keep polling and drawing
the progress bar while it proceeds. Measured on a real ~300k-entity,
3.1M-tile, 55-frame capture: parallel reading plus switching `Entity`/`Tile`
names from `String` to `Arc<str>` (see "Rendering" above) took total load
time from 47s to 20s.

**Multiple surfaces load as separate worlds.** `group_by_surface` splits a
loaded batch by each frame's parsed `surface` field (not its filename) into
one independently ordered timeline per surface, rather than collapsing to
whichever is busiest the way a single sequence has to. This is what lets the
viewer's `tab` key switch between worlds: the mod's raw baseline output
already writes every surface at one tick, and `save-timelapse-replay
all-surfaces` does the same across a whole timelapse. Each world keeps its
own `Camera`, fitted to its own frames at load time, so switching to another
world and back doesn't disturb either one's pan/zoom.

## Concurrency

Saves are independent. Each export gets its own staged directory and Factorio
process, so scanning parallelises across saves with no shared state.

Note that the CLI does not yet exploit this: `main.rs` exports saves in a
sequential loop, and each iteration launches Factorio and waits for it.
