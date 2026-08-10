# Testing

Five things, from cheapest to most involved. The first two run in seconds and
catch most mistakes; the rest exist because some questions can only be
answered by the game or by your eyes.

## 1. Before every commit

These are exactly what CI runs, so a green run here means a green run there:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
make test-lua LUA=E:/lua52/lua52.exe
```

`make test-lua` does two things. It compiles **every** shipped Lua file, since
a syntax error in `control.lua` or `capture.lua` is not a degraded mod but one
Factorio refuses to load at all, and then it runs the encoder unit suite.

Lua **5.2** specifically, the version Factorio's API is. The suite passes under
5.1 through 5.5 alike, so a newer interpreter looks healthy while accepting
syntax and library functions Factorio will not run. That is not hypothetical:
CI ran 5.3 for months and was green for the wrong reason.

## 2. Did my change help or hurt?

```bash
make stress-save     # record the current code as the baseline
# ...make your change...
make stress          # current vs baseline, with deltas
```

**Sizes and counts are exact.** If `write_bytes` moves, your change moved it,
even by one byte. **Timings are not.** Anything under 10% is marked `noise` and
means nothing; a single run on a desktop swings that much on its own.

A baseline is only comparable against the same shape, so re-record after
changing `--surfaces`, `--entities`, `--frames` or `--built`. Always
`--release`; a debug build is roughly ten times slower and the ratios between
stages differ.

To model a specific base, pass its real numbers:

```bash
make stress STRESS_ARGS="--surfaces 9 --entities 126000 --frames 1000"
```

## 3. Does it work in the actual game?

Nothing above touches Factorio, so this is the part only you can do.

```bash
make install-mod
```

Then restart Factorio, since Lua is read at load. A run through everything
worth checking:

- **Live capture.** Play a few minutes with capture on, then build a timelapse
  across every surface. Watch for the "Reading the baseline snapshot..." line
  rather than a silent pause.
- **Multiple surfaces.** In the viewer, `tab` through them all. Each must
  scrub over the same range and show the right state at the same playhead
  position. This is where skipping unchanged surfaces would show a bug.
- **Bookmarks.** `b` on a few frames, `m` and `shift+m` to walk them, `c` for
  busy stretches. Close and reopen the timelapse: they should still be there.
- **Reopening.** Quit, relaunch, choose option 1. Straight to the viewer with
  no rebuild.
- **Settings.** On that second run the folder line should say "remembered from
  last time" rather than "found at".
- **Tile reverts.** Needs terrain capture **on**. Fill water with landfill,
  let a frame pass, mine it back out. The water should return.

Anything touching capture needs **new play after installing**, since the mod
only records what happens while it is running.

## 4. What is actually in my capture?

When the tool warns about a capture, or something is missing that should be
there, read the event log rather than guessing. It is a plain tagged binary
format documented at the top of `src/event.rs`, and a short script that decodes
it will tell you the event counts by kind and by surface.

That is how the asteroid bug was found: the tool warned that almost nothing it
read did anything, and decoding the log showed 6,101 of 6,259 events were
removals on space platforms, which is what led to asteroid chunks never being
built by anyone.

The two measurement harnesses read a real capture the same way and are driven
by an environment variable so no local path is ever committed:

```bash
SAVE_TIMELAPSE_CAPTURE='<...>/script-output/save-timelapse/<session>' \
  cargo test --release --lib measure_unchanged_frames -- --ignored --nocapture
SAVE_TIMELAPSE_CAPTURE='<...>' \
  cargo test --release --lib measure_export_size -- --ignored --nocapture
```

## 5. Does it look right?

Some bugs are only visible. The image export is the cheapest way to look at a
frame without sitting in the viewer:

```bash
cargo run -p viewer --release --bin viewer -- tests/fixtures/frames \
  --export out/ --width 640 --height 360
```

The committed fixtures are real captured frames, so this works with no
Factorio and no capture of your own. It is how the export framing bug was
caught: the numbers were all fine and the picture was obviously wrong.
