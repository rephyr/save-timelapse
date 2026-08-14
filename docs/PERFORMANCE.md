# Performance

Every number here was measured, and every one can be re-measured. The
commands are given so nothing has to be taken on trust, and the harnesses
that produced them live in the repository rather than in a commit message.

## How these are measured

The measurement harnesses are `#[ignore]`d tests. They are not run by
`cargo test`, because they need either several seconds and a few hundred
megabytes or a real capture on disk, but they are kept next to the code they
measure so a change can be re-checked instead of assumed.

Two of them read a real capture folder, which no fixture can stand in for:
the answers are properties of how somebody actually played, not of the code.
Those take the folder from an environment variable so no local path is ever
committed.

## What they were measured on

    CPU      AMD Ryzen 9 5900X, 12 cores/24 threads
    RAM      32 GB DDR4-3200
    GPU      NVIDIA GeForce RTX 4080, 16 GB, 
    Storage  Samsung 970 EVO Plus 2 TB NVMe 
    OS       Windows 10 

Stated because three of the numbers below are properties of this machine as
much as of the code, and reading them without it invites the wrong conclusion.

**Thread count changes the load times.** `ParallelFrameLoad` and the grouping
passes split across `available_parallelism`, which is 24 here. The 47s to 20s
load figure is that split plus the `Arc<str>` change together; on four cores the
parallel half of it is worth proportionally less.

**Disk speed changes what skipping unchanged surfaces is worth.** Not writing
86% of the files saves more on a slow disk than on this one, so that measurement
is a floor rather than a typical result.

**The GPU barely matters for the draw-call numbers and matters a lot for
export.** Draw calls are a CPU submission cost, which is the whole reason
grouping by type is worth doing, and it would look much the same on weaker
hardware. Export is the opposite: a 4x supersampled 1080p frame is a 132 MB
readback, and that is where a slower card would show.

The mod's own cost is the exception. It is measured in Factorio's F4 time usage,
which is a share of a 60 Hz tick budget rather than wall clock, so it is the one
figure here that travels between machines reasonably well.

## Results

### Frame format: entity runs and delta-encoded positions

Grouping entities by prototype and storing positions as zigzag varint deltas
against the previous entity in the run, with the footprint carried once per
prototype instead of once per entity.

| | before | after |
|---|---|---|
| a real 22,971-entity frame | 14 bytes/entity | under 3.5 bytes/entity |
| a megabase Nauvis export | 200 MB | 38 MB |

The ratio is asserted rather than described, so it cannot quietly regress:

```bash
cargo test --lib version_2_is_several_times_smaller_than_version_1
```

It is also *faster* to write than the format it replaced, which is the part
that mattered most: the mod exports during live gameplay, so this cost is
paid as frozen frames in someone's game.

### Export: only writing surfaces that changed

A playthrough only builds on one surface at a time, but an export used to
write every surface at every frame. Measured on a real nine-surface Space Age
capture over 13 minutes of play:

| frame interval | before | after | |
|---|---|---|---|
| 30s | 243 files, 237.8 MB | 34 files, 15.6 MB | 93.4% smaller |
| 60s | 126 files, 123.3 MB | 22 files, 12.3 MB | 90.0% smaller |

The mechanism is visible per surface. With the player on Gleba, Gleba showed
3.7% duplicate frames while the other eight surfaces sat at 96.3% each; Nauvis
alone wrote 219.7 MB of which 211.5 MB was byte-identical.

```bash
SAVE_TIMELAPSE_CAPTURE='<...>/script-output/save-timelapse/<session>' \
  cargo test --release --lib measure_unchanged_frames -- --ignored --nocapture
SAVE_TIMELAPSE_CAPTURE='<...>' \
  cargo test --release --lib measure_export_size -- --ignored --nocapture
```

The first models the waste, the second runs the real writer into a scratch
directory and weighs it. They agree, which is the point of having both.

### Viewer memory: span storage

A factory is overwhelmingly the same factory from one frame to the next, so
frames are stored as spans (an item plus the half-open frame range it exists
over) rather than as a copy per frame. On a sequence shaped like a real
capture, a base growing to 400k entities over 150 frames:

| | |
|---|---|
| per-frame item copies | 30,199,950 |
| distinct spans | 400,000 |
| memory before | 362.4 MB |
| memory after | 9.6 MB |
| **reduction** | **37.7x** |

Seeking stays cheap despite materialising on demand: 425µs per seek, and a
full walk of all 150 frames in 67ms.

```bash
cargo test --release -p viewer --lib gains -- --ignored --nocapture
```

### Load time

Parallel frame parsing plus switching entity and tile names from `String` to
`Arc<str>`, on a real ~300k-entity, 3.1M-tile, 55-frame capture: **47s to
20s**. A capture has a few dozen distinct prototype names against hundreds of
thousands of entities, so cloning a name per record was the dominant cost of
loading a frame.

### Activity analysis: sorted merge over hash sets

Finding what is new in each frame runs on the load path against every entity
of every frame. On a 150-frame, 400k-entity sequence:

| approach | time |
|---|---|
| `HashSet` per frame | 2.7s |
| same, with a cheap multiply hasher | 2.3s |
| sorted vectors, merged | **1.07s** |

The hash function was never the cost. It was 30 million random-access probes
into a table far larger than cache. Sorting and merging touches the same data
almost entirely sequentially and allocates nothing per frame once the buffers
have grown.

```bash
cargo test --release -p viewer --lib measure_cost -- --ignored --nocapture
```

### Export: aggregating the ground

An export deliberately keeps full detail, because a chunk cell holds only its
dominant type and a paved area would swallow the belts running through it. That
reasoning is about entities and placed floor. It is not about grass.

Measured on a real Space Age megabase, per frame:

| layer | quads | share |
|---|---|---|
| natural ground | 19,732,883 | 82% |
| placed floor | 3,354,339 | 14% |
| entities | 863,862 | 4% |

The ground alone was four fifths of the drawing, and at roughly 2 seconds a
frame a 660 frame export took 22 minutes. Binned into the same 4x4 cells the
interactive view uses, that ground is **1,240,391 cells, 15.9x fewer**, and
**85% of those cells hold a single ground type**, so collapsing them loses
nothing whatsoever. Only the 15% straddling a boundary lose anything, at a scale
supersampling is already averaging away.

Items keep full detail exactly as before, so the belts the original decision
protected are untouched. Total drawing falls about 4.4x, which should take that
export from 22 minutes to roughly 5.

```bash
SAVE_TIMELAPSE_TERRAIN='<...>/timelapses/<name>/terrain_nauvis.stfr'   cargo test --release -p viewer --lib measure_terrain_lod -- --ignored --nocapture
```

### Folding a frame that changes most of the factory

`SpanBuilder::open` is everything standing, as a sorted vector. Folding a delta
in used to binary search it per changed item and splice, and `Vec::remove` and
`Vec::insert` move everything past the position they touch, so the cost was
standing multiplied by changed.

That is invisible at what an ordinary base does. The extreme is not: on a real
4000 hour gigabase, one frame took the factory from **3.7 million buildings to
500 thousand**, so a single delta carried about 3.2 million removals against a
list averaging around 2 million entries. Of the order of 3e12 element moves.

| approach | a frame that clears most of the factory |
|---|---|
| binary search and splice per item | did not complete, at either scale below |
| both sides sorted, merged in one pass | 0.25s at a million standing |

The two rows are not the same measurement and cannot be: the splice version has
no completion time to report. It was abandoned on the real capture, and again on
the million-entity test below.

The interesting part is what it looked like. Correct code that never returns
gives no error, no partial output and no progress, so it is indistinguishable
from a deadlock. It was also being misattributed: the viewer set its phase label
before the work and painted after it, so the screen named the previous step for
the whole of it, which pointed diagnosis at the wrong function.

```bash
cargo test --manifest-path viewer/Cargo.toml a_frame_that_clears_most
```

## Measured and rejected

The measurements that changed nothing are worth as much as the ones that did.

**Spatially sorting entities before delta encoding.** In theory this should
shrink coordinate deltas. Measured on a real frame it was **0.3%** better,
which is not worth sorting every entity during a live export. The export scan
order already has the locality that matters, because players and blueprints
lay same-type entities out in rows.

**A body hash to deduplicate identical frames.** Would have bought the
load-time saving only, and load time was already fine. Skipping the write
entirely was strictly better, so the hash was never built.

**A global "did anything change" counter** as a cheap proxy for duplicate
detection. It agreed exactly with byte comparison on a single-surface capture
and then disagreed wildly on a nine-surface one: it asks whether anything
changed *anywhere*, which is near always true, and reported almost no
duplication where 86% of files were duplicates. It was removed rather than
kept as a misleading number.

### Reading the ground faster

The ground scan is the slowest part of building a timelapse. On a 10 hour
Krastorio save it was 30s of a 38s Factorio run, the save load being 5.7s of
that. `encode.terrain_margin` gives the box a floor of `TERRAIN_MAX_TILES` per
surface, so the factory's own size barely enters into it: a small base and a
megabase both read about four million tiles, at roughly 3.8 us each.

Two changes were measured against that, both reverted.

**Asking for the box a block at a time, and a numeric loop instead of `pairs`.**
The whole box in one `find_tiles_filtered` builds a Lua table of four million
entries before the first tile is written; 256 tile blocks hold 65k. Together
these bought **7%**, 3.82 us a tile down to 3.56.

**Reading one tile per 8x8 square outside the fitted view**, written out as the
whole square, which removes three quarters of the reads. It bought a further
**9%**, 3.56 down to 3.24.

Nine percent for three quarters of the reads is the entire finding: the two
Lua/C++ crossings per tile are about a tenth of what a tile costs. The rest is
the per-tile *write* path, all of it Lua, being `record`'s table stores,
`frame_tile_run`'s position encoding, and `checksum_update` folding ten
megabytes a byte at a time.

The sampling worked and was not lost ground: every generated chunk in the
result held exactly 1024 tiles, so coverage was complete. It was still reverted,
because 9% is a poor price for a blocky shoreline beyond the fitted view. It is
worth revisiting only together with a frame format that can say "this 8x8
square is grass" in one record rather than 64, which would cut the writes, the
checksum and the file by the same factor. That is a change to a frozen format,
not an optimisation.

**Measuring this again.** Nothing outside Factorio can separate the scan from
the save load, and the tool's own stopwatch covers both. Factorio's log
timestamps every line from process start, so two `log()` calls bracketing the
scan in `M.export_terrain` give the split exactly. The log lives in the staging
directory `add_terrain` builds, which is removed on the way out whether the
scan succeeded or failed, so keep it or read it before then.

## Deliberately not optimized

**The player position log and the milestone log are plain JSON lines.** A
whole playthrough produces a few thousand position samples and about a dozen
milestones, nowhere near the per-tick construction volume that justified a
binary format for frames and events. Being readable by eye is worth more than
the few hundred bytes packing would save.

**`PlayerTrack` lookups are a linear scan.** Sample counts are tiny next to
entity counts, so a binary search would be complexity spent where no time is.

### Live capture's per-event cost

Unlike every other number in this file, this one comes from the game rather
than from a harness, because nothing outside Factorio can produce it: read off
`F4 > show-time-usage` during real play on a 69 mod save. The mod sits near
0.02 ms per tick idle and reaches about 0.2 ms while building hard, roughly 1%
of a 16.67 ms tick. Re-check it the same way; there is no command to give.

What follows is known, costs a little, and is left alone on purpose. Each item
says what it would buy and why that is not worth buying.

**Every field read off a `LuaEntity` crosses the Lua/C++ boundary once.** That
is what the per-event cost mostly is, so the only reductions worth making are
in how many properties are read at all. Two were made: a removal reads only
what a removal record holds (`log_entity`), and `game.tick` is read once per
event rather than three times. The rest below were considered and declined.

**`entity.surface.name` is two crossings and materialises a `LuaSurface`.**
`entity.surface_index` would be one crossing returning an integer, with a
memoized index to name table behind it. Declined on correctness, not on
effort: the game's own documentation for `LuaSurface.index` states that
"indexes of deleted surfaces can be reused", and Space Age creates and
destroys space platforms routinely, so a cached table would eventually file a
new platform's events under a dead platform's name. Keeping it correct means
subscribing to `on_surface_deleted` to invalidate, which registers a permanent
handler to save a property read. A silently mis-attributed surface is not
discovered until somebody builds the timelapse hours later, which is the wrong
trade for 1% of a tick.

**`is_surface_excluded` writes to `storage` on every event.**
`excluded_surfaces()` does `storage.x = storage.x or {}` unconditionally, so
the read path performs a write. Cheap (an ordinary Lua table store) and
removing it means the read path grows its own nil check while the write path
keeps creating the table. Left as is because the lazy init is why no caller has
to nil check, and that is worth more than the store.

**Each record allocates.** Varints are built through `string.char` and joined
with `table.concat`, so every event produces short-lived strings and the
collection that follows is charged to the mod. Short strings are interned in
Lua 5.2, so the repeated one-byte pieces mostly cost a hash lookup rather than
garbage, and the alternative is a hand-rolled buffer scheme whose complexity is
not repayable at this volume.

**Broader placed-floor detection logs tile events that used to be skipped.**
Working the floor list out from the loaded prototypes (see ARCHITECTURE's
"What the game says about itself") means a space platform's foundation and
every modded floor now record as built. That is the point, and it is genuinely
more work while paving or growing a platform. It is proportional to what the
player is doing rather than a standing cost.

**The flush tick does synchronous file I/O.** Amortised over 200 pending
events or ten seconds, whichever comes first, so it is a spike rather than an
average. Worth watching for a perceptible hitch, since a single long tick is
what a player feels; a raised average is not.
