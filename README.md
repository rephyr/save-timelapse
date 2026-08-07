# Save Timelapse

Build a timelapse of your Factorio factory from the save files you already have.

Other timelapse tools only start working the day you install them. This one
reads the saves sitting in your saves folder right now, including autosaves, so
you can make a timelapse of a base you finished months ago.

## Status

Early. The mod exports; the renderer is not written yet.

## How it works

Point the tool at your saves folder. For each save it launches Factorio
headless, has the mod export every entity to JSON, and collects the results as
frames. Each export runs against a private throwaway copy of your mods folder,
so your Factorio installation and settings are never modified.

## Requirements

- Factorio 2.0 or later, the full game rather than the headless server build,
  since exporting drives the normal executable
- Save files. More saves means more frames. Milestone saves and rolling
  autosaves both work.
- A Rust toolchain to build the tool, from <https://rustup.rs>

## Usage

```
save-timelapse --saves "%APPDATA%/Factorio/saves" --out frames
```

Useful flags:

| Flag | Meaning |
|---|---|
| `--factorio <path>` | Path to `factorio.exe` if it is not auto-detected |
| `--mods <path>` | Mods folder if it is not auto-detected |
| `--limit <n>` | Export only the first N saves, for a quick check |
| `--include-resources` | Include ore tiles. Every tile is a separate entity, so this multiplies frame size. |
| `--match-name <text>` | Export only saves whose filename contains this text, case insensitive |

`--saves` also accepts a single `.zip` directly, not just a folder.

Inside the game, `/timelapse-export` writes a single frame for the current save
to `script-output/save-timelapse/`.

## Live capture

The other way to build a timelapse, and the smoother one: instead of one frame
per save, you get one frame per however often you like.

Enable the runtime setting `save-timelapse-live-capture`. The mod takes **one**
full snapshot of the save — the game freezes for its duration, proportional to
base size (tens of seconds on a large base) — then logs nothing but
placements and removals as you play. When you're done, replay the log over
that baseline:

```
save-timelapse-replay --capture "%APPDATA%/Factorio/script-output/save-timelapse" --out frames
```

That writes ordinary `frame_NNNN.json` files, the same ones the viewer reads,
so nothing downstream needs to know the timeline came from events.

| Flag | Meaning |
|---|---|
| `--interval <ticks>` | Ticks between frames. Factorio runs at 60/s, so the default 3600 is one frame per minute of game time. |
| `--surface <name>` | Which surface to render. Defaults to whichever has the most entities. |
| `--max-frames <n>` | Stop after this many frames. |

The baseline is taken once per save and recorded inside the save file, so it
never repeats — and a game saved partway through the export just retakes it on
the next load. Unlike the snapshot flow this only covers play from the moment
you turn it on, since Factorio keeps no placement history to recover
retroactively.

If you delete files from `script-output/save-timelapse`, the mod has no way
to notice: Factorio only lets it write files, never read or list them back.
It will keep assuming the baseline it already took is still there. Run
`/timelapse-reset-capture` to clear that assumption and retake the baseline
immediately, along with starting a fresh event log.

A separate runtime setting, `save-timelapse-snapshot-seconds`, takes a full
snapshot on a repeating timer instead — independent of live capture, for
exercising the export path during real play. 0 disables it (the default).
Unlike the baseline, this one runs incrementally across many ticks rather
than freezing the game, since paying that cost on every repeat (rather than
once) isn't worth it; a snapshot can still be running when the next one comes
due, in which case that tick is silently skipped.

The viewer ignores incomplete or malformed frame files while loading, so a
snapshot still being written won't crash it.

## Viewer

```
cargo run -p viewer --release -- frames
```

Drag to pan, scroll to zoom, left/right to step, space to play, home/end to
jump, `s` to toggle sprites, and drag the bar at the bottom to scrub. A
progress bar covers loading, which on a large save set takes a while.

The second HUD line reports draw calls against quads submitted, so a
regression in batching is visible rather than something you have to profile
for. `s` is the A/B: sprites off draws the same geometry as flat rects.

To measure without opening a window:

```
cargo run -p viewer --bin drawcalls --release            # the real fixtures
cargo run -p viewer --bin drawcalls --release -- --synthetic 500000
```

## Building

The mod is plain Lua and needs no build step. The tool is Rust:

```
cargo build --release
cargo test --workspace
```

`cargo test` alone skips the viewer, which is a separate workspace member.

## Developing without Factorio

The whole project can be built, tested and run on a machine with no Factorio
installed. `cargo test` needs nothing beyond the toolchain.

A `fake-factorio` binary implements enough of Factorio's command line to
exercise the exporter end to end. It decodes the staged `mod-settings.dat` and
emits a frame only when the export trigger is genuinely set, so it catches
staging bugs rather than papering over them.

To drive the real CLI with it, put it where the executable is expected:

```
mkdir -p /tmp/fake/factorio/bin/x64 /tmp/fake/factorio/data /tmp/fake/saves
cp target/debug/fake-factorio /tmp/fake/factorio/bin/x64/factorio
for n in 1 2 3 10 20; do echo save > /tmp/fake/saves/base$n.zip; done

FAKE_FACTORIO_FRAME=tests/fixtures/frames/frame_0003.json \
cargo run --bin save-timelapse -- \
    --saves /tmp/fake/saves \
    --mods  /tmp/fake/mods \
    --factorio /tmp/fake/factorio/bin/x64/factorio \
    --out /tmp/fake/out
```

`tests/fixtures/frames/` holds five real frames, from 240 to 22,971 entities
across 58 entity types, for building the renderer against realistic data.
See [tests/fixtures/README.md](tests/fixtures/README.md).

## Design

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License

MIT
