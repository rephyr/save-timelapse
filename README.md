# Save Timelapse

[![CI](https://github.com/rephyr/save-timelapse/actions/workflows/ci.yml/badge.svg)](https://github.com/rephyr/save-timelapse/actions/workflows/ci.yml)

**The factory must grow. Unlike your files.**

Save Timelapse is an interactive Factorio timelapse tool built for large factories and long-running saves. Its live capture records your factory as you play using an initial snapshot followed by incremental changes, avoiding repeated full-world exports.

The result is an interactive replay you can pan, zoom, scrub, and explore. It is designed to stay efficient even as your factory reaches hundreds of thousands of entities, with optimized binary storage, parallel loading, memory-efficient replay data, and a renderer built for megabase-scale factories.

Already have a factory? Save Timelapse can also reconstruct a timelapse from your existing Factorio save files, even if you never had the mod installed while playing.

![Interactive Factorio timelapse viewer replaying a megabase](assets/overview.gif)

A factory recorded and rendered end to end. Click to watch:

[![Factorio Save Timelapse: a factory recorded and rendered end to end](https://img.youtube.com/vi/CCscPpznCfo/maxresdefault.jpg)](https://youtu.be/CCscPpznCfo)

Captured by the mod while playing, rendered to video by the viewer. No external video editing software was used at any point.


Zoomed in, it draws with Factorio's own artwork, so belts weave and turn, splitters and underground belts read the right way round, and pipe runs join up. 

Reworked ui and belt rotations from v0.7.0:

![The Save Timelapse viewer, showing a factory drawn with Factorio's own belt, splitter and pipe artwork](assets/sprites.PNG)

> **Beta.** The recording format is frozen, so a capture you make today keeps working as the tool updates, and every released format is checked against real recorded bytes on every build. There are a few places that need ironing out but all the hard stuff is done and current version seemed stable during testing period.

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

### 🧩 Works with the mods you play

Save Timelapse does not carry a list of the things it knows how to draw. It records what your game says its own prototypes are, so a modded playthrough replays as itself: terrain and buildings take the colours Factorio paints its own map view with, and a modded belt is still a belt, a modded ore patch is still ore, and a modded underground belt still pairs up over its real distance.

That means an Alien Biomes world keeps its own terrain, a Krastorio2 belt curves at corners, and an ore field from a mod nobody has ever heard of does not pull the camera off your factory. There is nothing to configure, and a mod added partway through a playthrough is picked up the next time you load the save.

Modded buildings are drawn with their own artwork too. A mod's icons cannot be found by guessing, since the file need not be named after the building and is often several layers the game composites, so Factorio is asked to draw them all once per modpack and the result is kept with your timelapse. It takes about a minute the first time you build with a given set of mods and nothing after that, and an unmodded playthrough skips it altogether.

---

## Install

Save Timelapse has two parts: a desktop companion tool and an optional Factorio mod.

### Build a timelapse from existing saves

You only need the companion tool.

1. Download the latest [release](https://github.com/rephyr/save-timelapse/releases).
2. Unzip `save-timelapse.exe` into a folder of its own.
3. Run `save-timelapse.exe`.

One file, and the viewer is part of it. There is nothing to keep beside it except the `mod` folder it unzips with, which it needs to read your saves.

### Prefer a window?

Run it with `--gui` and you get one instead of the text menu:

```bash
save-timelapse --gui
```

Everything the menu does is there: watching a timelapse, building one from a recording or from your saves, saving a video, and managing what is on disk. It opens your timelapse the moment it has built it, rather than sending you back to pick it.

It is opt in for this release while it gets some use. The text menu is unchanged and still what you get by double-clicking.

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
| `P` | Toggle player markers |
| `?` | Show every control |
| `F3` | Renderer diagnostics |

Playback, the planet switcher and reframing are also clickable, so the viewer can be driven without knowing any of these.

---

## Export a video

Exporting is done from the companion tool rather than from inside the viewer. Run `save-timelapse.exe`, choose the video option, and pick a timelapse, a resolution and a frame rate. The finished file is written to a `videos` folder next to the program.

No FFmpeg or other video software is needed. You can also export a numbered image per frame instead, for editing the result yourself.

You are also asked what to put on top of the video: the in-game clock, so the footage says how long the factory took, and a marker showing where you were, for captures recorded with live capture. Both are burned into the frames, so they are chosen before the render rather than switched on afterwards. Neither is on unless you ask, apart from the clock, which most timelapses want.

If you happen to have FFmpeg on your PATH, the tool offers an MP4 as well. It is roughly fifteen times smaller than the AVI and is what sharing sites accept, since many will not upload or preview an AVI at all. It is only ever offered when FFmpeg is already there, and nothing asks you to install it.

---

## Try it without Factorio

The repository includes real captured frames from a 100-hour Space Age factory.

```bash
cargo run --release --bin save-timelapse -- --view tests/fixtures/frames
```

The included capture grows from 240 to 22,971 entities, allowing the renderer to be tested without a Factorio installation.

---

## Known limitations

- **Moving entities are not recorded.** Biters, pentapods, demolishers, flying robots, vehicles, Spidertrons, and trains are excluded rather than being shown frozen in place.
- **Live capture starts when enabled.** Earlier factory history requires existing save files.
- **Save-based milestones depend on save frequency.** Live capture records milestone timing precisely, while existing saves can only identify the first save that shows an event.
- **Bot construction is less visible in the heatmap.** Automated construction is spread across multiple frames, while manual building can create many entities in a single frame.
- **Entity rotation is limited.** Belts, underground belts, splitters, pipes and rail are drawn from Factorio's own artwork and show their real direction and connections. Most other entities remain unrotated because their inventory icons are not designed to rotate convincingly.

- **A belt that changed direction without being rebuilt may face the wrong way.** Rotating a belt by hand is recorded, and so is the rotation Factorio applies to the belt you dragged from when a line turns a corner. What is still missing is any other way the game changes a facing without an event. Recording a fresh baseline corrects everything built so far.

- **Only branches recorded by this version can be returned to.** Loading an older save and carrying on tells the recording that the play you left behind was replaced, and it is dropped, which is what keeps the timelapse showing one coherent history rather than several contradicting ones. Come back to a branch you left and the recording follows you, because every save made from 0.8.0 on knows which recording it belongs to. Saves made before that do not, so a branch you left with an older version is gone for good, and so is one whose files you deleted by hand. Recording a fresh baseline from where you are now repairs either.

- **A nest you walked up to is shown from the start.** Where the nests are comes from the same one-off scan the ground does, and that scan reads a finished save, so it cannot say when any of them appeared. Nests the biters built are timed correctly, because your recording says when, and the scan hands those back to it. What is left is a nest that was always sitting there in a part of the map you had not explored yet: nothing anywhere knows when it came into view, so it is drawn from the beginning.

- **Two playthroughs on the same map seed are one recording.** A recording is filed under your map's terrain seed, which is what lets the tool recognise your playthrough across saves and refuse ground scanned from somebody else's game. It also means a second game started from the same seed is indistinguishable from carrying on with the first: both write into the same recording, and one of them is treated as a branch you left behind and dropped. Rolling a new seed, which is what generating a new map normally does, keeps them apart.

- **The landscape from before your recording is the finished one.** Ground, ore, trees and cliffs are scanned once from a single save after the fact, which is what keeps them out of your game entirely. A forest you cleared before that recording began is in no save it can read and was never written down anywhere else, so it was never there to find. Cleared while recording, it replays as it happened, and so does a patch your drills have mined out. With terrain capture switched off there is deliberately nothing to uncover at all.

---

## Roadmap

### Shipped

- **v0.1:** Save export, live capture, interactive replay, timeline scrubbing, chunked renderer, sprites, LOD
- **v0.2:** Checksums, format versioning, playback speed, terrain rendering, player tracking, camera auto-follow
- **v0.3:** Terrain optimization, entity rotation, capture recovery, in-game control panel, surface selection
- **v0.4:** Timeline timestamps, activity graph, heatmap, milestones, capture management
- **v0.5:** Stable capture format, save-based milestones, 90% smaller exports, tile reverts, bookmarks, Linux builds
- **v0.6:** Video export, improved camera framing, moving entities excluded, ground scanned from a save, better scenery cutoff
- **v0.7:** Modded games recorded as themselves, Factorio's own belt and pipe artwork, reworked viewer, belt rotations recorded
- **v0.8, the beta:** Incremental frames, 98% smaller timelapses, ground read once per playthrough, MP4 export, video overlays, recording recovery, saves compared against each other, rail drawn along the track
- **v0.8.1:** Smoothed export camera, framing clear of the scrub bar, modded building artwork
- **v0.8.2:** Scenery scanned from the save rather than bounded to the starting factory, rail corners drawn as curves
- **v0.9:** One binary instead of two, a window alongside the text menu, biter expansion and nest clearing recorded, ground under paving recovered, exhausted ore emptied out, pipes joined to underground runs

### Upcomming

- **v1.0:** Currently gated behind testing and finding edge-cases/missed bugs

---

## For developers

```bash
cargo build --release
cargo test --workspace
make test-lua LUA=lua52
```

Documentation:

- [Requirements](docs/REQUIREMENTS.MD)
- [Architecture](docs/ARCHITECTURE.md)
- [Performance](docs/PERFORMANCE.md)
- [Test fixtures](tests/fixtures/README.md)

The exporter can also be developed and tested without owning Factorio.

---

## Contributing

Issues, bug reports, performance reports, and feature suggestions are welcome.

---

## License

MIT

The window sets its name in [Full Automation](https://fonts.google.com/) by Sharkshock, used under the SIL Open Font License. The face is compiled into the binary; its licence ships beside it and lives in `fonts/OFL.txt`.
