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

Inside the game, `/timelapse-export` writes a single frame for the current save
to `script-output/save-timelapse/`.

## Building

The mod is plain Lua and needs no build step. The tool is Rust:

```
cargo build --release
cargo test
```

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
