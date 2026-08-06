//! Reassembling a timeline from a baseline snapshot plus the event log.
//!
//! The capture directory the mod writes looks like this:
//!
//! ```text
//! baseline.json                  tick + surfaces the baseline covers
//! frame_<tick>_<surface>.json    one per surface, the baseline itself
//! events_<start_tick>.jsonl      append-only, one segment per timeline
//! ```
//!
//! `baseline.json` is written last, so its presence means the baseline
//! finished. Replay loads it, seeds a [`World`], then walks the event
//! segments forward, emitting a frame whenever enough ticks have passed.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::event::{self, LoggedEvent};
use crate::frame::Frame;
use crate::world::World;

pub const BASELINE_MANIFEST: &str = "baseline.json";

#[derive(Debug, Deserialize)]
pub struct Baseline {
    pub tick: u64,
    #[serde(default)]
    pub entities: usize,
    #[serde(default)]
    pub tiles: usize,
    pub surfaces: Vec<String>,
}

impl Baseline {
    pub fn read(dir: &Path) -> io::Result<Baseline> {
        let path = dir.join(BASELINE_MANIFEST);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot read {}: {e}. The mod writes it once the baseline \
                     snapshot completes, so an absent one means capture was \
                     never enabled, or the export did not finish.",
                    path.display()
                ),
            )
        })?;
        serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn frame_path(&self, dir: &Path, surface: &str) -> PathBuf {
        dir.join(format!("frame_{}_{}.json", self.tick, surface))
    }
}

/// How often replay emits a frame, and how many it will emit at most.
pub struct Options {
    /// Game ticks between emitted frames. 3600 is one minute of game time.
    pub interval: u64,
    pub max_frames: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { interval: 3600, max_frames: 100_000 }
    }
}

#[derive(Debug)]
pub struct Replay {
    pub world: World,
    pub baseline: Baseline,
    /// Events that changed nothing. A steady trickle is normal -- it is the
    /// baseline smear (see `world`) plus removals of entities filtered out of
    /// snapshots. A large fraction suggests events and baseline disagree,
    /// e.g. a log replayed against the wrong save.
    pub no_op_events: usize,
    pub applied_events: usize,
}

/// Seed a world from the baseline the manifest names.
///
/// Surfaces load largest-file-first so the busiest one becomes the default
/// that untagged events fall back to.
pub fn load_baseline(dir: &Path) -> io::Result<Replay> {
    let baseline = Baseline::read(dir)?;

    let mut paths: Vec<PathBuf> =
        baseline.surfaces.iter().map(|s| baseline.frame_path(dir, s)).collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.metadata().map(|m| m.len()).unwrap_or(0)));

    let mut world = World::new();
    let mut loaded = 0;
    for path in &paths {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("warning: baseline surface {} unreadable: {e}", path.display());
                continue;
            }
        };
        match serde_json::from_str::<Frame>(&text) {
            Ok(frame) => {
                world.load_baseline(&frame);
                loaded += 1;
            }
            Err(e) => eprintln!("warning: baseline surface {} unparseable: {e}", path.display()),
        }
    }

    if loaded == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{BASELINE_MANIFEST} names {} surface(s) but none could be loaded",
                baseline.surfaces.len()
            ),
        ));
    }

    world.tick = baseline.tick;
    Ok(Replay { world, baseline, no_op_events: 0, applied_events: 0 })
}

/// Walk the event log forward, calling `emit` with the world at each frame
/// boundary. Events are applied in whole-tick groups, so a frame is never cut
/// halfway through a tick's changes -- a blueprint landing 400 entities on one
/// tick shows up whole or not at all.
///
/// `emit` receives the tick rather than a materialised frame so the caller
/// decides what to do with the world: write every surface, one surface, or
/// just measure.
pub fn run<F>(replay: &mut Replay, dir: &Path, options: &Options, mut emit: F) -> io::Result<usize>
where
    F: FnMut(&World, u64),
{
    let mut next_emit = replay.baseline.tick;
    let mut emitted = 0;

    let mut flush_until = |world: &World, tick: u64, next: &mut u64, emitted: &mut usize| {
        while tick >= *next && *emitted < options.max_frames {
            emit(world, *next);
            *emitted += 1;
            *next += options.interval;
        }
    };

    for segment in event::log_paths(dir)? {
        let mut pending: Vec<LoggedEvent> = Vec::new();
        let mut pending_tick = None;

        for logged in event::stream_log(&segment)? {
            // Events before the baseline describe a world we did not capture.
            if logged.tick < replay.baseline.tick {
                continue;
            }

            if pending_tick != Some(logged.tick) {
                apply_batch(replay, &mut pending);
                if let Some(tick) = pending_tick {
                    flush_until(&replay.world, tick, &mut next_emit, &mut emitted);
                }
                pending_tick = Some(logged.tick);
            }
            pending.push(logged);
        }

        apply_batch(replay, &mut pending);
        if let Some(tick) = pending_tick {
            flush_until(&replay.world, tick, &mut next_emit, &mut emitted);
        }
    }

    // Always land a final frame on the finished world, so the timelapse ends
    // on the base as it actually was rather than at the last interval
    // boundary that happened to fall before the last event.
    if emitted < options.max_frames {
        emit(&replay.world, replay.world.tick.max(next_emit.saturating_sub(options.interval)));
        emitted += 1;
    }

    Ok(emitted)
}

/// Apply one tick's events together, then clear the buffer for reuse rather
/// than allocating a new one per tick.
fn apply_batch(replay: &mut Replay, pending: &mut Vec<LoggedEvent>) {
    for logged in pending.iter() {
        if replay.world.apply(logged.surface.as_deref(), &logged.event) {
            replay.applied_events += 1;
        } else {
            replay.no_op_events += 1;
        }
        replay.world.tick = replay.world.tick.max(logged.tick);
    }
    pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn capture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("baseline.json"),
            r#"{"tick":100,"entities":1,"tiles":0,"surfaces":["nauvis"]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("frame_100_nauvis.json"),
            r#"{"tick":100,"surface":"nauvis","entities":[{"n":"pipe","x":0.5,"y":0.5}],"count":1,"tiles":[],"tile_count":0}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_missing_manifest_explains_what_it_means() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_baseline(dir.path()).unwrap_err();
        assert!(err.to_string().contains("baseline.json"), "got: {err}");
    }

    #[test]
    fn the_baseline_seeds_the_world() {
        let dir = capture_dir();
        let replay = load_baseline(dir.path()).unwrap();
        assert_eq!(replay.world.entity_count(), 1);
        assert_eq!(replay.baseline.tick, 100);
    }

    #[test]
    fn a_manifest_naming_no_loadable_surface_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("baseline.json"),
            r#"{"tick":5,"surfaces":["gone"]}"#,
        )
        .unwrap();
        assert!(load_baseline(dir.path()).is_err());
    }

    #[test]
    fn replay_applies_events_and_emits_frames_on_the_interval() {
        let dir = capture_dir();
        fs::write(
            dir.path().join("events_100.jsonl"),
            "{\"t\":100,\"op\":\"+\",\"k\":\"e\",\"s\":\"nauvis\",\"n\":\"transport-belt\",\"x\":1.5,\"y\":0.5}\n\
             {\"t\":160,\"op\":\"+\",\"k\":\"e\",\"s\":\"nauvis\",\"n\":\"transport-belt\",\"x\":2.5,\"y\":0.5}\n\
             {\"t\":220,\"op\":\"-\",\"k\":\"e\",\"s\":\"nauvis\",\"x\":0.5,\"y\":0.5}\n",
        )
        .unwrap();

        let mut replay = load_baseline(dir.path()).unwrap();
        let mut seen: Vec<(u64, usize)> = Vec::new();
        let options = Options { interval: 50, max_frames: 100 };
        run(&mut replay, dir.path(), &options, |world, tick| {
            seen.push((tick, world.entity_count()));
        })
        .unwrap();

        assert_eq!(replay.applied_events, 3);
        assert_eq!(replay.no_op_events, 0);
        // Grows to 2 as belts land, then back to 2 when the pipe is removed
        // (1 baseline + 2 belts - 1 pipe).
        assert_eq!(seen.last().unwrap().1, 2);
        assert!(seen.len() >= 3, "expected several frames, got {seen:?}");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "ticks must not go backwards");
    }

    /// Everything in one tick must land together: a frame boundary inside a
    /// blueprint would show half of it.
    #[test]
    fn events_sharing_a_tick_are_applied_as_one_batch() {
        let dir = capture_dir();
        let mut log = String::new();
        for i in 0..50 {
            log.push_str(&format!(
                "{{\"t\":500,\"op\":\"+\",\"k\":\"e\",\"s\":\"nauvis\",\"n\":\"pipe\",\"x\":{}.5,\"y\":9.5}}\n",
                i
            ));
        }
        fs::write(dir.path().join("events_100.jsonl"), log).unwrap();

        let mut replay = load_baseline(dir.path()).unwrap();
        let mut counts = Vec::new();
        let options = Options { interval: 100, max_frames: 100 };
        run(&mut replay, dir.path(), &options, |world, _| counts.push(world.entity_count()))
            .unwrap();

        // No frame may show a partial tick: counts jump from 1 to 51.
        assert!(
            counts.iter().all(|&c| c == 1 || c == 51),
            "a frame caught mid-tick: {counts:?}"
        );
        assert_eq!(*counts.last().unwrap(), 51);
    }

    #[test]
    fn events_before_the_baseline_are_ignored() {
        let dir = capture_dir();
        fs::write(
            dir.path().join("events_1.jsonl"),
            "{\"t\":5,\"op\":\"-\",\"k\":\"e\",\"s\":\"nauvis\",\"x\":0.5,\"y\":0.5}\n",
        )
        .unwrap();

        let mut replay = load_baseline(dir.path()).unwrap();
        run(&mut replay, dir.path(), &Options::default(), |_, _| {}).unwrap();
        assert_eq!(replay.world.entity_count(), 1, "the pre-baseline removal was skipped");
    }

    #[test]
    fn segments_replay_in_tick_order() {
        let dir = capture_dir();
        fs::write(
            dir.path().join("events_9000.jsonl"),
            "{\"t\":9000,\"op\":\"-\",\"k\":\"e\",\"s\":\"nauvis\",\"x\":1.5,\"y\":0.5}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("events_200.jsonl"),
            "{\"t\":200,\"op\":\"+\",\"k\":\"e\",\"s\":\"nauvis\",\"n\":\"pipe\",\"x\":1.5,\"y\":0.5}\n",
        )
        .unwrap();

        let mut replay = load_baseline(dir.path()).unwrap();
        run(&mut replay, dir.path(), &Options::default(), |_, _| {}).unwrap();
        // Add at tick 200 then remove at 9000 nets out; replayed the other
        // way round the remove would no-op and the add would survive.
        assert_eq!(replay.world.entity_count(), 1);
        assert_eq!(replay.no_op_events, 0);
    }

    #[test]
    fn max_frames_caps_output() {
        let dir = capture_dir();
        fs::write(
            dir.path().join("events_100.jsonl"),
            "{\"t\":100000,\"op\":\"+\",\"k\":\"e\",\"s\":\"nauvis\",\"n\":\"pipe\",\"x\":5.5,\"y\":5.5}\n",
        )
        .unwrap();

        let mut replay = load_baseline(dir.path()).unwrap();
        let options = Options { interval: 1, max_frames: 10 };
        let mut count = 0;
        let emitted = run(&mut replay, dir.path(), &options, |_, _| count += 1).unwrap();
        assert_eq!(emitted, 10);
        assert_eq!(count, 10);
    }

    #[test]
    fn a_capture_with_no_events_still_emits_the_baseline() {
        let dir = capture_dir();
        let mut replay = load_baseline(dir.path()).unwrap();
        let mut count = 0;
        let emitted = run(&mut replay, dir.path(), &Options::default(), |_, _| count += 1).unwrap();
        assert_eq!(emitted, 1);
        assert_eq!(count, 1);
    }
}
