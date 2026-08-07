//! Reassembling a timeline from a baseline snapshot plus the event log.
//!
//! The capture directory the mod writes looks like this:
//!
//! ```text
//! baseline.json                  tick + surfaces the baseline covers
//! frame_<tick>_<surface>.stfr    one per surface, the baseline itself
//! events_<start_tick>.stev       append-only, one segment per timeline
//! ```
//!
//! `baseline.json` is written last, so its presence means the baseline
//! finished. Replay loads it, seeds a [`World`], then walks the event
//! segments forward, emitting a frame whenever enough ticks have passed.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::event::{self, LoggedEvent};
use crate::frame;
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
        dir.join(format!("frame_{}_{}.stfr", self.tick, surface))
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
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("warning: baseline surface {} unreadable: {e}", path.display());
                continue;
            }
        };
        match frame::read_binary(&bytes) {
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

/// Writes every surface `world` has at `tick`, one `.stfr` file each, named
/// `frame_<index>_<surface>.stfr` -- the same shape the mod's own baseline
/// output uses, and what `viewer::group_by_surface` expects in order to show
/// more than one world. A surface with nothing on it at this tick (not yet
/// built, or already abandoned) is skipped rather than writing an empty
/// file.
///
/// Shared by `save-timelapse-replay --all-surfaces` and
/// `save-timelapse-watch`, which both want every surface rather than picking
/// one busiest.
pub fn write_all_surfaces(world: &World, tick: u64, out: &Path, index: usize) -> io::Result<()> {
    for surface in world.surface_names() {
        let frame = world.to_frame(surface, tick);
        if frame.entities.is_empty() && frame.tiles.is_empty() {
            continue;
        }
        let path = out.join(format!("frame_{index:04}_{surface}.stfr"));
        std::fs::write(&path, frame::write_binary(&frame.as_out()))?;
    }
    Ok(())
}

/// Apply one tick's events together, then clear the buffer for reuse rather
/// than allocating a new one per tick.
fn apply_batch(replay: &mut Replay, pending: &mut Vec<LoggedEvent>) {
    for logged in pending.iter() {
        if replay.world.apply(Some(logged.surface.as_str()), &logged.event) {
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
    use crate::wire::ByteWriter;
    use std::collections::HashMap;
    use std::fs;

    fn capture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("baseline.json"),
            r#"{"tick":100,"entities":1,"tiles":0,"surfaces":["nauvis"]}"#,
        )
        .unwrap();

        let entities = vec![frame::Entity { n: "pipe".into(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1 }];
        let out = frame::FrameOut { tick: 100, surface: "nauvis", entities: &entities, tiles: &[] };
        fs::write(dir.path().join("frame_100_nauvis.stfr"), frame::write_binary(&out)).unwrap();

        dir
    }

    /// Builds one `events_<tick>.stev` segment's bytes, tracking its name and
    /// surface dictionaries so a test reads as a sequence of events rather
    /// than a manual byte layout, the way the real writer builds it while
    /// logging as play happens.
    struct TestLog {
        w: ByteWriter,
        names: HashMap<String, u16>,
        surfaces: HashMap<String, u16>,
    }

    impl TestLog {
        fn new() -> Self {
            let mut w = ByteWriter::new();
            w.magic(b"STE1");
            TestLog { w, names: HashMap::new(), surfaces: HashMap::new() }
        }

        fn name_id(&mut self, name: &str) -> u16 {
            if let Some(&id) = self.names.get(name) {
                return id;
            }
            let id = self.names.len() as u16;
            self.names.insert(name.to_string(), id);
            self.w.u8(0).string(name);
            id
        }

        fn surface_id(&mut self, surface: &str) -> u16 {
            if let Some(&id) = self.surfaces.get(surface) {
                return id;
            }
            let id = self.surfaces.len() as u16;
            self.surfaces.insert(surface.to_string(), id);
            self.w.u8(1).string(surface);
            id
        }

        fn tick(&mut self, tick: u64) -> &mut Self {
            self.w.u8(2).u64(tick);
            self
        }

        /// `id` of 0 means the add carries no unit_number, matching the wire
        /// sentinel `event.rs` decodes.
        fn add_entity(&mut self, surface: &str, name: &str, x: f32, y: f32, id: u64) -> &mut Self {
            let surface_id = self.surface_id(surface);
            let name_id = self.name_id(name);
            self.w
                .u8(3)
                .u16(name_id)
                .i32((x * 10.0).round() as i32)
                .i32((y * 10.0).round() as i32)
                .u8(0)
                .u8(1)
                .u8(1)
                .u64(id)
                .u16(surface_id);
            self
        }

        fn remove_entity(&mut self, surface: &str, x: f32, y: f32) -> &mut Self {
            let surface_id = self.surface_id(surface);
            self.w.u8(4).i32((x * 10.0).round() as i32).i32((y * 10.0).round() as i32).u64(0).u16(surface_id);
            self
        }

        fn write(self, dir: &Path, name: &str) {
            fs::write(dir.join(name), self.w.into_vec()).unwrap();
        }
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
        let mut log = TestLog::new();
        log.tick(100).add_entity("nauvis", "transport-belt", 1.5, 0.5, 0);
        log.tick(160).add_entity("nauvis", "transport-belt", 2.5, 0.5, 0);
        log.tick(220).remove_entity("nauvis", 0.5, 0.5);
        log.write(dir.path(), "events_100.stev");

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
        // (1 baseline plus 2 belts, minus 1 pipe).
        assert_eq!(seen.last().unwrap().1, 2);
        assert!(seen.len() >= 3, "expected several frames, got {seen:?}");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "ticks must not go backwards");
    }

    /// Everything in one tick must land together: a frame boundary inside a
    /// blueprint would show half of it.
    #[test]
    fn events_sharing_a_tick_are_applied_as_one_batch() {
        let dir = capture_dir();
        let mut log = TestLog::new();
        log.tick(500);
        for i in 0..50 {
            log.add_entity("nauvis", "pipe", i as f32 + 0.5, 9.5, 0);
        }
        log.write(dir.path(), "events_100.stev");

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
        let mut log = TestLog::new();
        log.tick(5).remove_entity("nauvis", 0.5, 0.5);
        log.write(dir.path(), "events_1.stev");

        let mut replay = load_baseline(dir.path()).unwrap();
        run(&mut replay, dir.path(), &Options::default(), |_, _| {}).unwrap();
        assert_eq!(replay.world.entity_count(), 1, "the pre-baseline removal was skipped");
    }

    #[test]
    fn segments_replay_in_tick_order() {
        let dir = capture_dir();
        let mut later = TestLog::new();
        later.tick(9000).remove_entity("nauvis", 1.5, 0.5);
        later.write(dir.path(), "events_9000.stev");

        let mut earlier = TestLog::new();
        earlier.tick(200).add_entity("nauvis", "pipe", 1.5, 0.5, 0);
        earlier.write(dir.path(), "events_200.stev");

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
        let mut log = TestLog::new();
        log.tick(100_000).add_entity("nauvis", "pipe", 5.5, 5.5, 0);
        log.write(dir.path(), "events_100.stev");

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

    #[test]
    fn write_all_surfaces_skips_an_empty_surface_and_names_the_rest() {
        let mut world = crate::world::World::new();
        world.load_baseline(&crate::frame::Frame {
            tick: 100,
            surface: "nauvis".to_string(),
            entities: vec![crate::frame::Entity { n: "pipe".into(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1 }],
            count: 1,
            tiles: Vec::new(),
        });
        // vulcanus exists (an event referenced it) but has nothing on it yet,
        // e.g. a platform not built out this early in the timeline.
        world.load_baseline(&crate::frame::Frame {
            tick: 100,
            surface: "vulcanus".to_string(),
            entities: Vec::new(),
            count: 0,
            tiles: Vec::new(),
        });

        let dir = tempfile::tempdir().unwrap();
        write_all_surfaces(&world, 100, dir.path(), 7).unwrap();

        let written: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(written, vec!["frame_0007_nauvis.stfr"], "the empty vulcanus surface must not get a file");
    }
}
