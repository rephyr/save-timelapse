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

## Deliberately not optimized

**The player position log and the milestone log are plain JSON lines.** A
whole playthrough produces a few thousand position samples and about a dozen
milestones, nowhere near the per-tick construction volume that justified a
binary format for frames and events. Being readable by eye is worth more than
the few hundred bytes packing would save.

**`PlayerTrack` lookups are a linear scan.** Sample counts are tiny next to
entity counts, so a binary search would be complexity spent where no time is.
