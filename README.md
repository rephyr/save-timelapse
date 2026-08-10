# Save Timelapse

[![CI](https://github.com/rephyr/save-timelapse/actions/workflows/ci.yml/badge.svg)](https://github.com/rephyr/save-timelapse/actions/workflows/ci.yml)

Save Timelapse is a Factorio timelapse mod and companion desktop application for creating interactive, explorable timelapses of your Factorio factory.

> Watch your Factorio factory grow. Record it live as you play, or build a timelapse from Factorio saves you already have.

**Save Timelapse reconstructs your factory's history as an interactive replay**, not a fixed video. Pan, zoom, scrub through construction history, and watch your base grow from a small starter factory into a massive megabase.

The companion tool automatically detects your Factorio saves folder, so you can generate a timelapse from existing saves with no manual setup. For live capture, install the mod and enable the `save-timelapse-live-capture` setting to record your factory as you play.

> ⚠️ **Alpha:** The core pipeline is functional, but the project is under active development. Expect bugs, missing features, and breaking changes between releases.

---
## Features

- 📦 Build timelapses from **existing save files**
- 🎮 Live capture mode with minimal performance impact
- 🎛️ In-game panel to control live capture: start/stop, choose which surfaces are recorded, and reset
- 🗂 Capture management in the desktop tool: name each playthrough, see its size on disk, and delete ones you are finished with
- 📂 Built timelapses are kept, so you can close the tool and reopen one later instead of rebuilding it
- 🗺 Interactive viewer with pan, zoom and timeline scrubbing
- 🌍 Multi-surface support (Nauvis, platforms, planets)
- 🏁 Milestone markers on the timeline: first science packs, first rocket, planets reached, in both live capture and timelapses built from existing saves
- ⚡ Chunked renderer with automatic level-of-detail rendering
- 🦀 Written in Rust for performance
- 🔧 No Python, command-line tools or FFmpeg required

---

## Try it without owning Factorio

The repository ships five real exported frames, so the viewer can be run
against genuine captured data with nothing else installed:

```bash
cargo run -p viewer --release --bin viewer -- tests/fixtures/frames
```

A factory growing from 240 to 22,971 entities across 58 entity types,
captured from a real 100 hour Space Age save. Scrub the timeline, zoom in far
enough for sprites, press `h` for the construction heatmap.

The exporter can be developed and tested without the game too. `fake-factorio`
is a binary in this crate that implements enough of Factorio's command line to
exercise the real export path, including reading back the staged
`mod-settings.dat` so a test proves the settings staging actually worked
rather than assuming it. See [tests/fixtures/README.md](tests/fixtures/README.md).

---

## Why Save Timelapse?

Most Factorio timelapse tools have one major limitation:

> They only start recording after you install them.

Save Timelapse can reconstruct a timelapse from the saves already sitting in your saves folder. If you've kept autosaves or milestone saves, you can build a timelapse of a factory that was created weeks or months ago.

For ongoing factories, enable live capture once and the mod records only incremental changes instead of repeatedly exporting the whole world.

---

## Installation

Two separate downloads, depending on what you want to do.

**The tool** (`save-timelapse.exe` + `viewer.exe`): needed either way, since this is what builds and shows the timelapse.

1. Download the latest release from [GitHub Releases](https://github.com/rephyr/save-timelapse/releases)
2. Unzip `save-timelapse.exe` and `viewer.exe` into the same folder (the first launches the second, so they need to sit together)
3. Run `save-timelapse.exe`

**The mod**: only needed for live capture, not for building a timelapse from saves you already have.

- Get it from the [Factorio mod portal](https://mods.factorio.com/mod/save-timelapse), or install it in-game via Settings > Mods > Install mods
- Enable the `save-timelapse-live-capture` runtime setting to start recording

No changes are made to your Factorio installation or mods folder unless you install the mod yourself.

---

## Screenshots
- Overview of the current state of the tool
![Tool overview](assets/overview.gif)

- Timeline scrubbing
![Timeline scrubbing](assets/scrubbing.gif)

- Zoomed sprite rendering
![Sprite rendering](assets/sprites.PNG)

- Camera auto-follow gradually zooming out as the base grows
![Camera auto-follow](assets/camera-follow.gif)

---

## How it works

Save Timelapse supports two workflows.

### Existing saves

The application:

1. Finds your Factorio installation
2. Launches Factorio in headless mode
3. Exports each selected save
4. Reconstructs the world
5. Opens the interactive viewer

It also asks whether to include natural terrain (grass, water, trees,
cliffs) around the base. Worth it for how much more it looks like a real
place, but it's a real cost, not a free improvement: roughly 5x more export
time and file size in testing, so it's an explicit yes/no each run rather
than always on.

No changes are made to your real installation or mod folder.

---

### Live capture

Enable the runtime setting:

```
save-timelapse-live-capture
```

The mod:

- performs one initial snapshot
- records only entity additions/removals afterwards
- writes a compact binary format: entities are grouped by type and their
  positions stored as small deltas, which measured roughly 5x smaller than
  the previous format on a megabase (a 200 MB surface export became 38 MB)
  while also being faster to write than the format it replaced
- tags every capture with which playthrough it belongs to, so saves from
  different games never get mixed into one timelapse

Whenever you want to view the replay, simply launch Save Timelapse. If more
than one playthrough has capture data waiting, it asks which one to build
the timelapse from.

Click the Save Timelapse shortcut in the top toolbar (or press
Control+Shift+T) to open the in-game control panel: toggle live capture,
exclude individual surfaces from recording (including planets you haven't
visited yet), or reset capture. Excluding a surface skips its baseline
entirely, not just its ongoing events, so a huge base you don't care about
tracking (a sprawling Nauvis factory, say, while you only want a smaller
Gleba outpost recorded) doesn't cost anything to capture. Checking a box
only records the choice; press **Generate** to actually take a catch-up
baseline for whatever you just included, so ticking several surfaces first
batches into one freeze instead of one per box, without touching any other
surface's already-recorded history. Resetting (behind a confirmation
dialog, since it's permanent) deletes this playthrough's own capture files
and retakes the baseline, so files no longer need to be deleted by hand
first.

> Upgrading from an older version? Run `/timelapse-reset-capture` once
> in-game (or use the panel's reset button) so your current playthrough
> starts a freshly tagged capture.

Terrain works differently here, since save-timelapse.exe only reads a
baseline after the mod already took it, not before: enable the
`save-timelapse-capture-terrain` **startup** setting (off by default, same
reasoning as above) before your baseline is taken if you want it included
in a live capture (see [Known Limitations](#known-limitations)).

---

## Viewer

Current features:

- Pan
- Zoom
- Timeline scrubbing, labelled with elapsed in-game time at each end and at the playhead, and hovering the bar shows the time and frame number at that point before you commit to a seek
- Activity graph along the scrub bar: how much got built at each point in the run, so busy stretches and idle ones are visible at a glance without playing through them
- Construction heatmap (`h` to toggle, off by default): warm overlay showing where building happened over the last few frames, drawn under the factory so it never obscures what you built
- Milestone markers under the scrub bar: the first of each science pack, the first rocket launch, and each planet reached, coloured by science pack and labelled on hover
- Play / Pause (`-`/`=` adjust speed, 0.25x-8x)
- Jump between notable moments: `m` for the next milestone or bookmark, `c` for the next busy stretch of building, and hold shift with either to go backwards
- Bookmarks (`b` to set or clear one at the current frame), drawn as yellow ticks above the scrub bar and saved beside the frames, so they are still there next time you open that timelapse
- Home / End navigation
- Surface switching
- Player position marker
- Camera auto-follow (on by default, `f` to toggle off): gradually pans and zooms out to keep the whole base in frame as it grows, the way TLBE's own camera does
- Sprite rendering
- Entity rotation (belts only for now, see [Known Limitations](#known-limitations))
- Flat-color LOD rendering
- Parallel loading
- Progress indicator

---

## Building

```bash
cargo build --release
cargo test --workspace
```

The Lua mod requires no build step, but it has its own test suite, which
needs a Lua interpreter that `cargo test` deliberately does not depend on:

```bash
make test-lua                 # needs `lua` on PATH
make test-lua LUA=lua52       # or point it at a specific interpreter
```

Use **Lua 5.2**, the version Factorio's modding API is. The suite passes
under 5.1 through 5.5 alike, so a newer interpreter looks healthy while
accepting syntax (`//`, `&`, `~`) and library functions (`string.pack`,
`math.type`) that Factorio will not run.

---

## Roadmap

### v0.1

- [x] Existing save export
- [x] Live capture
- [x] Interactive replay
- [x] Timeline scrubbing
- [x] Chunked renderer
- [x] Sprite rendering
- [x] LOD rendering

### v0.2

- [x] Checksums and format versioning
- [x] Adjustable playback speed
- [x] Documentation, screenshots, and GIFs
- [x] Terrain and terrain-scatter rendering
- [x] Player position tracking
- [x] Camera auto-follow

### v0.3

- [x] Terrain capture optimization
- [x] Entity rotation tracking
- [x] Robust capture recovery and diagnostics
- [x] In-game live capture control panel
- [x] Safer capture reset
- [x] Per-surface capture selection
- [x] Mid-playthrough surface baselines
- [x] Improved baseline warnings

### v0.4

- [x] Timeline timestamps and hover information
- [x] Construction activity graph and map heatmap
- [x] Milestone markers (first of each science pack, first rocket, each planet reached)
- [x] Capture management: name your captures, see what they cost on disk, delete ones you are done with

### v0.5

- [x] Settled capture format: extension records make future additions skippable, so the mod and the tool no longer have to be updated in lockstep
- [x] Milestones for timelapses built from existing saves, recovered by comparing consecutive saves
- [x] Skip writing frames that changed nothing: a surface is only written at moments something on it changed, measured at 90% smaller on a real nine-surface megabase export
- [x] Complete tile change tracking: removing a placed tile restores what it was covering, so mining landfill puts the water back
- [x] Settings persistence and first-run setup: where Factorio is, seconds per frame, and the terrain choice are remembered between runs
- [x] Bookmarks and jumping between milestones and busy stretches
- [ ] Linux builds

### v1.0

- [ ] Broader modded-game compatibility
- [ ] Smarter camera auto-follow
- [ ] Camera keyframes and cinematic controls
- [ ] Export resolution and FPS controls
- [ ] MP4 video export
- [ ] Polished export workflow
- [ ] Single binary, so the tool and viewer cannot be separated
- [ ] Stable capture format and format migration

---

## Architecture

```
Factorio Save
        │
        ▼
 Lua Export Mod
        │
        ▼
 Binary Snapshot + Event Log
        │
        ▼
 Replay Engine (Rust)
        │
        ▼
 Interactive Renderer
```

More details are available in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md),
including the wire formats, the extension contract that keeps captures
readable across versions, and why the mod cannot detect a reloaded save.

[docs/PERFORMANCE.md](docs/PERFORMANCE.md) covers what was optimized, by how
much, and how to re-measure all of it: 5x smaller frames, 90% smaller exports,
37.7x less viewer memory. It also records what was measured and then
**rejected**, which is the more useful half.

---

## Known Limitations

- **Tile reverts need terrain capture on:** Removing a placed tile restores whatever it was covering, so mining landfill puts the water back rather than leaving a hole. With terrain capture off there is deliberately no natural ground in the timelapse, so the tile simply disappears instead. Only applies to tiles removed while capture is running: a tile already gone before capture started was never seen either way.
- **Entity rotation:** Rotation is currently limited to a small allowlist of confirmed entities, such as transport belts. Other icons use an oblique 3D-style perspective and do not rotate correctly. Rotation also only works for square-footprint entities.
- **From-saves milestones are only as precise as your save cadence:** A save records that a science pack has been produced, never when it first was, so a timelapse built from existing saves marks each milestone at the first save that shows it. A pack first produced an hour before the save that first mentions it is marked at that save, not an hour earlier. Live capture watches them happen and is exact. Building from an already established base also opens with a cluster of markers, since everything already done is reported by the earliest save; that is accurate rather than tidy, and live capture does the same thing when switched on mid-playthrough.
- **The construction heatmap barely registers bot building:** The overlay scales every cell against the busiest single cell of the whole run, and that peak is almost always a blueprint landing hundreds of entities in one frame. Construction robots place the same blueprint gradually over many frames instead, so each frame contributes a small fraction of what an instant placement does and the glow stays dim or invisible even while a large area is genuinely being built. The activity graph along the scrub bar has the same cause and shows bot work as a long low plateau rather than a spike. Separately, space platform construction was not recorded at all before v0.5, which looked like the same problem but was a different one and is fixed.
- **Nothing that moves is recorded:** The capture format records that something was built or destroyed, never that it moved, so biters, spitters and flying construction/logistics robots are deliberately excluded rather than drawn frozen wherever they happened to be. Stationary enemies are kept: nests and worms appear in red, so clearing them is visible as the front line moves outward.

---

## Contributing

Issues, bug reports and feature suggestions are welcome.

---

## License

MIT
