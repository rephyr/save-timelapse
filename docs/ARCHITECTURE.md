# Architecture

## Components

    mod/     Lua mod loaded by Factorio. Exports entity data as JSON.
    src/     Rust CLI. Drives Factorio, collects exports, renders output.

The two never communicate directly. Factorio's Lua sandbox cannot read files,
open sockets, or spawn processes, so the mod's only output channel is writing
into `script-output`. The CLI reads that directory after the process exits.

## Pipeline

    saves/*.zip
        |
        |  CLI launches: factorio --benchmark <save> --benchmark-ticks 3
        |                         --config <staged> --mod-directory <staged>
        v
    Factorio loads the save, mod exports on first tick, process exits
        |
        v
    <staged>/script-output/save-timelapse/frame_<tick>_<surface>.json
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
from `factorio.exe --version`.

## Frame format

    {
      "tick": 22630009,
      "surface": "nauvis",
      "entities": [ {"n": "transport-belt", "x": -80.5, "y": 28.5, "d": 4} ],
      "count": 517934,
      "tiles": [ {"n": "concrete", "x": -80, "y": 28} ],
      "tile_count": 812004
    }

Keys are shortened because entity count dominates file size. `d` is omitted
when direction is zero. Coordinates are fixed to one decimal, matching
Factorio's half-tile entity alignment.

`tiles` covers placed floor (concrete, stone path, hazard/refined concrete
variants, landfill) — a short, stable include list, the opposite of entity
filtering's exclude list, since natural terrain vastly outnumbers placed
floor types. Tile positions are integers: a tile named at `(x, y)` occupies
world space `[x, x+1) x [y, y+1)`, corner rather than center anchored like
entities. `tiles` is absent from frames captured before tile export existed;
readers should treat a missing `tiles`/`tile_count` as empty rather than
requiring it.

A surface is exported when it is nauvis or contains at least one entity owned
by the player force. A manifest listing exported surfaces accompanies each set,
and now also reports the tile total alongside the entity total.

## Entity filtering

Filtering happens in the `find_entities_filtered` query rather than in Lua, so
excluded types are never returned across the API boundary.

Excluded by default: characters, corpses, particles, projectiles, trees, rocks,
cliffs, fish, fire, smoke, explosions, ghosts, dropped items, combat robots,
streams, stickers and beams.

Resource entities are excluded unless `save-timelapse-include-resources` is set.
Every ore tile is a separate entity and they typically outnumber built entities
while carrying no information about factory growth.

## Write batching

Entity JSON is accumulated in a Lua table and flushed to
`helpers.write_file` in blocks. Each call is a file append, so one call per
entity makes export time scale with syscall count rather than entity count.

## Live capture and replay

The second way to build a timelapse, and the finer-grained one. The mod
snapshots a save **once**, then logs only what changes; the Rust side
reassembles any moment by replaying that log over the baseline.

    <script-output>/save-timelapse/
        baseline.json                  tick + surfaces the baseline covers
        frame_<tick>_<surface>.json    the baseline itself, one per surface
        events_<start_tick>.jsonl      append-only, one segment per timeline

`baseline.json` is written last, so its existence means the baseline finished.
It is the handshake: replay reads it to learn which frame files to seed from.

The baseline is taken once per save, not periodically. `baseline_tick` lives
in `storage`, so it travels inside the save file — a save that has been
baselined knows it. It is recorded only on *completion*, so a game saved and
reloaded midway simply starts over rather than leaving a truncated baseline
that replay would trust. Abandoned partial files are orphans no manifest
names, so they are ignored rather than harmful.

Snapshotting periodically instead would be pure duplication of what the log
already says: at roughly 50 bytes per entity, a megabase snapshot every ten
seconds writes gigabytes an hour.

### Event format

One JSON object per line. `op` is `+`/`-`, `k` is `e`/`t`.

    {"t":1234,"op":"+","k":"e","s":"nauvis","n":"transport-belt","x":10.5,"y":20.5,"d":4,"id":8842}
    {"t":1250,"op":"-","k":"e","id":8842}

`d`/`w`/`h` are omitted at their defaults, as in the frame format. `s` is the
surface, and without it a Space Age save's planets collapse into one
coordinate space, since positions repeat across surfaces. It is omitted on
removals keyed by `id`: `unit_number` is unique across the whole game rather
than per surface, so the id alone locates the target.

### Why replay is forgiving

The baseline is written across many ticks and so is not an atomic picture of
one instant — something built while it runs may or may not appear, depending
on whether its surface had already been flushed. Events are logged throughout
that window too, so replay can see an add for something already present, or a
remove for something it never saw.

Both are no-ops rather than errors. That turns an unavoidable smear into a
non-problem, instead of something the mod would have to freeze the game to
prevent. `save-timelapse-replay` reports the no-op count: a trickle is normal,
but a large fraction means the log and baseline came from different
playthroughs.

Events are applied in whole-tick batches, so a frame is never cut halfway
through a tick — a blueprint landing 400 entities appears whole or not at all.

### World state

Entities live in a slab with free-list reuse, indexed by position and by
`unit_number`. Baseline entities have no `unit_number` (snapshots don't record
one), so they can only be removed by position, which is why the mod always
emits the position form when no id is available.

Position keys are scaled by **ten**, not two. Half-tile alignment covers most
entities but not all: `frame_0000.json` holds a
`logistic-train-stop-lamp-control` at x=326.9 beside its `logistic-train-stop`
at x=327.0. Keying on half tiles merged them and silently dropped five of that
frame's 240 entities. One decimal is exactly the precision the mod writes.

## Rendering

The viewer converts each parsed `Frame` into a `RenderFrame` at load time and
drops the parsed form. Two things happen in that conversion.

**Names are interned.** A real base has tens of distinct prototype names
against hundreds of thousands of entities, so `Frame`'s `n: String` is one
heap allocation per entity for one of ~58 repeated strings. `TypeRegistry`
maps each name to a `u16` once and resolves its color at the same time.
Drawing then never hashes a name: the pre-registry loop called `color_for`
(FNV over the name) and `sprites.get(&e.n)` (SipHash over the name) for every
entity on every rendered frame.

**Items are grouped into per-type runs**, by counting sort, so all entities of
one type sit contiguously and a `Run` names the span. This is what keeps the
GPU batch intact. macroquad merges geometry only into the *immediately
preceding* draw call, and starts a new one whenever the bound texture changes
(`quad_gl.rs::geometry`), so drawing in export order — which interleaves types
— costs close to one draw call per entity. Untextured rects count as their own
texture state, so mixing shapes and sprites breaks the batch the same way.

Measured on `tests/fixtures/frames` with `cargo run -p viewer --bin drawcalls`,
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
covers both phases, frame parsing and sprite loading, since sprites are loaded
once up front rather than on first use — otherwise scrubbing stutters the
first time a not-yet-seen type appears.

## Concurrency

Saves are independent. Each export gets its own staged directory and Factorio
process, so scanning parallelises across saves with no shared state.

Note that the CLI does not yet exploit this: `main.rs` exports saves in a
sequential loop, and each iteration launches Factorio and waits for it.
