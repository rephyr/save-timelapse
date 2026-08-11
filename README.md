# Save Timelapse

[![CI](https://github.com/rephyr/save-timelapse/actions/workflows/ci.yml/badge.svg)](https://github.com/rephyr/save-timelapse/actions/workflows/ci.yml)

**The factory must grow. Unlike your files.**

Save Timelapse is an interactive Factorio timelapse tool built for large factories and long-running saves. Its live capture records your factory as you play using an initial snapshot followed by incremental changes, avoiding repeated full-world exports.

The result is an interactive replay you can pan, zoom, scrub, and explore. It is designed to stay efficient even as your factory reaches hundreds of thousands of entities, with optimized binary storage, parallel loading, memory-efficient replay data, and a renderer built for megabase-scale factories.

Already have a factory? Save Timelapse can also reconstruct a timelapse from your existing Factorio save files, even if you never had the mod installed while playing.

![Interactive Factorio timelapse viewer replaying a megabase](assets/overview.gif)


Zoomed in, it draws with Factorio's own artwork, so belts weave and turn, splitters and underground belts read the right way round, and pipe runs join up. 

Reworked ui and belt rotations from v0.7.0:

![The Save Timelapse viewer, showing a factory drawn with Factorio's own belt, splitter and pipe artwork](assets/sprites.PNG)

> ⚠️ **Alpha.** The core pipeline works, but the project is still under active development. Expect bugs and changes between releases.

---

## Why Save Timelapse?

### 🎮 Live capture with low overhead

Enable live capture and Save Timelapse takes one initial snapshot of your factory. After that, it records only the changes that happen as you play instead of repeatedly exporting the entire factory.

This makes it practical for long-running games and large factories where repeatedly scanning hundreds of thousands of entities would be expensive.

### ⚡ Built for megabase-scale performance

Large Factorio factories create a lot of data. Save Timelapse is designed around that problem rather than treating it as an afterthought.

- Custom binary formats instead of JSON for high-volume data
- Incremental event recording during live capture
- Compact frame storage
- Parallel frame loading across CPU cores
- Memory-efficient replay representation
- Chunk-based rendering and level of detail
- GPU draw-call batching
- World-space culling before rendering

The result is a viewer that can handle captures containing hundreds of thousands of entities without loading a full duplicate of every frame into memory.

### 📦 Use saves you already have

Save Timelapse does not require you to have been recording beforehand.

If you have autosaves or milestone saves from an existing factory, the companion tool can use those saves to reconstruct its history. It runs Factorio in an isolated environment, exports the selected saves, and builds them into the same replay format used by live capture.

---

## Install

Save Timelapse has two parts: a desktop companion tool and an optional Factorio mod.

### Build a timelapse from existing saves

You only need the companion tool.

1. Download the latest [release](https://github.com/rephyr/save-timelapse/releases).
2. Unzip `save-timelapse.exe` and `viewer.exe` into the same folder.
3. Run `save-timelapse.exe`.

The tool automatically finds your Factorio installation and saves folder and remembers your settings for future runs.

It asks whether to include the natural ground, meaning the grass, water and trees around your factory. It looks considerably better with it. The ground is read in one extra pass over your most recent save rather than recorded into every frame, so it costs one more Factorio run and very little size.

### Record while you play

Install the Save Timelapse mod from the [Factorio Mod Portal](https://mods.factorio.com/mod/save-timelapse) or through **Settings > Mods > Install mods**.

Enable the `save-timelapse-live-capture` runtime setting.

The mod takes an initial snapshot and then records changes as you play.

Press **Control+Shift+T** in game, or use the toolbar button, to open the live capture control panel.

From the panel you can:

- Start and stop live capture
- Choose which planets and space platforms to record
- Add previously skipped surfaces and generate catch-up baselines
- Reset the current capture

> **The initial snapshot pauses the game.** It reads your entire factory in one go, which on a very large base takes a few tens of seconds. The mod tells you how many entities it is about to read and gives you a moment before it starts. This happens once, and everything afterwards is incremental, so it has no ongoing effect on your game. Surfaces you exclude in the panel are skipped entirely, so you never pay for a base you did not want recorded.

> **Want ground in a live capture?** You are asked once the timelapse is built, not before you start recording. Ground never changes during a playthrough, so it is read afterwards from one of your saves rather than recorded while you play. It costs nothing during the game, and it can be added to a capture you already have.

---

## Interactive viewer

The replay is designed to be explored rather than simply watched.

- 🗺️ Pan and zoom around your factory
- ⏱️ Scrub through the entire timeline
- ▶️ Play at 0.25x to 8x speed
- 🏁 Jump between milestones
- 📊 See construction activity over time
- 🔥 View a construction heatmap
- 🔖 Add bookmarks
- 🌍 Switch between planets and space platforms
- 👤 Track player position, in timelapses recorded with live capture

### Viewer controls

| Key | Action |
|---|---|
| Drag / scroll | Pan and zoom |
| `Space` | Play / pause |
| `-` / `=` | Decrease / increase playback speed |
| `←` / `→` or `,` / `.` | Step one frame |
| `Home` / `End` | Jump to start / end |
| `Tab` | Switch planet or platform |
| `M` | Next milestone or bookmark |
| `Shift+M` | Previous milestone or bookmark |
| `C` | Next busy construction period |
| `Shift+C` | Previous busy construction period |
| `B` | Add or clear bookmark |
| `F` | Toggle camera auto-follow |
| `H` | Toggle construction heatmap |
| `?` | Show every control |
| `F3` | Renderer diagnostics |

Playback, the planet switcher and reframing are also clickable, so the viewer can be driven without knowing any of these.

---

## Export a video

Exporting is done from the companion tool rather than from inside the viewer. Run `save-timelapse.exe`, choose the video option, and pick a timelapse, a resolution and a frame rate. The finished file is written to a `videos` folder next to the program.

No FFmpeg or other video software is needed. You can also export a numbered image per frame instead, for editing the result yourself.

---

## Try it without Factorio

The repository includes real captured frames from a 100-hour Space Age factory.

```bash
cargo run -p viewer --release --bin viewer -- tests/fixtures/frames
```

The included capture grows from 240 to 22,971 entities, allowing the renderer to be tested without a Factorio installation.

---

## Known limitations

- **Moving entities are not recorded.** Biters, pentapods, demolishers, flying robots, vehicles, Spidertrons, and trains are excluded rather than being shown frozen in place.
- **Live capture starts when enabled.** Earlier factory history requires existing save files.
- **Save-based milestones depend on save frequency.** Live capture records milestone timing precisely, while existing saves can only identify the first save that shows an event.
- **Bot construction is less visible in the heatmap.** Automated construction is spread across multiple frames, while manual building can create many entities in a single frame.
- **Entity rotation is limited.** Belts, underground belts, splitters and pipes are drawn from Factorio's own in-world sprites, so they show their real direction, corners and connections. Most other entities remain unrotated because their inventory icons are not designed to rotate convincingly.

- **A belt that changed direction without being rebuilt may face the wrong way.** Rotating a belt by hand is recorded. A belt whose direction changes because the game connected it up for you, rather than because you rotated it yourself, is not, so it keeps the facing it had when it was first placed. It shows up as an occasional corner drawn as a straight belt. Recording a fresh baseline corrects everything built so far.

---

## Roadmap

### Shipped

- **v0.1:** Save export, live capture, interactive replay, timeline scrubbing, chunked renderer, sprites, LOD
- **v0.2:** Checksums, format versioning, playback speed, terrain rendering, player tracking, camera auto-follow
- **v0.3:** Terrain optimization, entity rotation, capture recovery, in-game control panel, surface selection
- **v0.4:** Timeline timestamps, activity graph, heatmap, milestones, capture management
- **v0.5:** Stable capture format, save-based milestones, 90% smaller exports, tile reverts, bookmarks, Linux builds
- **v0.6:** Video export, improved camera framing, moving entities excluded, ground scanned from a save, better scenery cutoff

### v0.7

- [x] Clean viewer interface: surface switcher, clickable playback controls, keyboard panel
- [x] Belts, underground belts, splitters and pipes drawn with Factorio's own artwork
- [x] Belt rotations recorded, so corners stop drawing as straight belts
- [x] The companion tool stays open and speaks plainly, and failures return to the menu
- [ ] Refreshed screenshots and demo recordings

### v1.0

- [ ] Broader modded-game support
- [ ] Smarter camera auto-follow
- [ ] Camera keyframes and cinematic controls
- [ ] MP4 export
- [ ] Single-binary distribution
- [ ] Stable capture format with migration

---

## For developers

```bash
cargo build --release
cargo test --workspace
make test-lua LUA=lua52
```

Documentation:

- [Architecture](docs/ARCHITECTURE.md)
- [Performance](docs/PERFORMANCE.md)
- [Testing](docs/TESTING.md)
- [Test fixtures](tests/fixtures/README.md)

The exporter can also be developed and tested without owning Factorio.

---

## Contributing

Issues, bug reports, performance reports, and feature suggestions are welcome.

---

## License

MIT
