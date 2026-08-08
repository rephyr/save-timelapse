# Save Timelapse
Save timelapse is a mod for Factorio.

> Build an interactive timelapse of your Factorio factory  from existing saves or while you play.

**Save Timelapse works with saves you already have**. Point it at your save folder, and it reconstructs your factory's history into an interactive replay you can pan, zoom, scrub, and explore.

> ⚠️ **Alpha:** The core pipeline is functional, but the project is under active development. Expect bugs, missing features, and breaking changes between releases.

---

## Features

- 📦 Build timelapses from **existing save files**
- 🎮 Live capture mode with minimal performance impact
- 🗺 Interactive viewer with pan, zoom and timeline scrubbing
- 🌍 Multi-surface support (Nauvis, platforms, planets)
- ⚡ Chunked renderer with automatic level-of-detail rendering
- 🦀 Written in Rust for performance
- 🔧 No Python, command-line tools or FFmpeg required

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
![Tool overview](assets/save-timelapse-overview.PNG)

- Live capture, played back
![Demo](assets/demo.gif)

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
- keeps runtime overhead minimal
- tags every capture with which playthrough it belongs to, so saves from
  different games never get mixed into one timelapse

Whenever you want to view the replay, simply launch Save Timelapse. If more
than one playthrough has capture data waiting, it asks which one to build
the timelapse from.

> Upgrading from an older version? Run `/timelapse-reset-capture` once
> in-game so your current playthrough starts a freshly tagged capture.

Terrain works differently here, since save-timelapse.exe only reads a
baseline after the mod already took it, not before: enable the
`save-timelapse-capture-terrain` **startup** setting (off by default, same
reasoning as above) before your baseline is taken if you want it included
in a live capture.

> **Known limitation:** with terrain capture on, removing landfill during
> the tracked playthrough leaves an empty tile in the replay instead of
> reverting to the water underneath. Fixing this properly needs a two-layer
> tile model; tracked for a future release.

---

## Viewer

Current features:

- Pan
- Zoom
- Timeline scrubbing
- Play / Pause (`-`/`=` adjust speed, 0.25x-8x)
- Home / End navigation
- Surface switching
- Player position marker
- Camera auto-follow (on by default, `f` to toggle off): gradually pans and zooms out to keep the whole base in frame as it grows, the way TLBE's own camera does
- Sprite rendering
- Flat-color LOD rendering
- Parallel loading
- Progress indicator

---

## Building

```bash
cargo build --release
cargo test --workspace
```

The Lua mod requires no build step.

---

## Roadmap

### v0.1

- [x] Existing save export
- [x] Live capture
- [x] Interactive replay
- [x] Timeline scrubbing
- [x] Chunk renderer
- [x] Sprite rendering
- [x] LOD rendering

### v0.2

- [x] Checksums (corrupted frame/event files are detected, not silently misread)
- [x] Versioning (the binary formats carry a version byte)
- [x] Adjustable playback speed
- [x] Better documentation
- [x] Add screenshots and gifs to README.md of the working project

Bonus, not originally planned for v0.2 but delivered along the way:
- [x] Terrain and terrain-scatter rendering (grass, water, trees, cliffs), opt-in given its export cost
- [x] Player position tracking, shown as a marker in the viewer
- [x] Camera auto-follow: gradually pans and zooms out to keep the whole growing base in frame, with smooth transitions (originally planned for v1.0)

Video export moved out of this release see v1.0.

### v0.5

- [ ] One-click installer
- [ ] Save detection
- [ ] Performance improvements
- [ ] Polish

### v1.0

- [ ] Bookmarks
- [ ] Timeline markers
- [ ] Statistics
- [ ] Video export
- [ ] Better export

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

More details are available in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

---

## Contributing

Issues, bug reports and feature suggestions are welcome.

---

## License

MIT
