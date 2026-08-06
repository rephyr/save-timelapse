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

## Design

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## License

MIT
