//! Reassembling a timeline from a baseline snapshot plus the event log.
//!
//! The capture directory the mod writes looks like this:
//!
//! ```text
//! <session>/baseline.json                 tick + surfaces one playthrough's baseline covers
//! <session>/frame_<tick>_<surface>.stfr   one per surface, that baseline itself
//! <session>/events_<start_tick>.stev      append-only, one segment per timeline
//! <session>/players.jsonl                 optional, sampled player positions
//! ```
//!
//! `<session>` (an 8-digit hex folder name) is which playthrough these files
//! belong to (see mod/control.lua's `compute_session_id`): the capture
//! folder is shared by every save that ever turns capture on, and
//! `game.tick` restarts from 0 for each one, so a bare tick cannot tell two
//! playthroughs' files apart, and a bare filename cannot tell two
//! playthroughs' files apart either, but two playthroughs no longer share
//! one directory to get confused in, since each gets its own.
//!
//! `<session>/baseline.json` is written last, so its presence means that
//! playthrough's baseline finished. Replay loads it, seeds a [`World`], then
//! walks that session folder's event segments forward, emitting a frame
//! whenever enough ticks have passed.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::event::{self, LoggedEvent};
use crate::frame;
use crate::world::World;

#[derive(Debug, Deserialize)]
pub struct Baseline {
    pub tick: u64,
    #[serde(default)]
    pub entities: usize,
    #[serde(default)]
    pub tiles: usize,
    pub surfaces: Vec<String>,
    /// Not part of the JSON body: derived from the filename by `read_at`,
    /// since that's also where the mod's writer gets it from.
    #[serde(skip)]
    pub session_id: u32,
}

/// Parses a session's hex folder name (e.g. `00000001` in
/// `save-timelapse/00000001/baseline.json`). `None` for anything else,
/// including a `baseline.json` sitting directly in the shared top-level
/// capture folder (every capture used that shape before playthroughs got
/// their own folder), which is what makes such a leftover invisible to
/// `discover_sessions` rather than ambiguous.
fn parse_session_dir_name(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    if name.len() != 8 {
        return None;
    }
    u32::from_str_radix(name, 16).ok()
}

impl Baseline {
    pub fn read_at(path: &Path) -> io::Result<Baseline> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot read {}: {e}. The mod writes it once a playthrough's baseline \
                     snapshot completes, so a missing one means capture was never enabled \
                     for it, or the export did not finish.",
                    path.display()
                ),
            )
        })?;
        let mut baseline: Baseline =
            serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        baseline.session_id = path.parent().and_then(parse_session_dir_name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: expected a hex-named parent folder to read a session id from", path.display()),
            )
        })?;
        Ok(baseline)
    }

    /// The path of one of this baseline's per-surface snapshot files,
    /// alongside it in the same session folder. Untagged: unlike the
    /// pre-folder scheme, this never needed the session id in the filename
    /// itself, since the folder already scopes it.
    pub fn frame_path(&self, dir: &Path, surface: &str) -> PathBuf {
        dir.join(format!("frame_{}_{}.stfr", self.tick, surface))
    }
}

/// One playthrough with a finished baseline, as found by [`discover_sessions`].
#[derive(Debug)]
pub struct Session {
    pub session_id: u32,
    /// This session's own subfolder: pass this, not the shared top-level
    /// capture directory, to [`run`] and [`event::log_paths`].
    pub session_dir: PathBuf,
    pub baseline_path: PathBuf,
    pub baseline: Baseline,
    pub last_modified: SystemTime,
}

/// Every playthrough with a finished baseline among `dir`'s session
/// subfolders, newest first.
///
/// "Newest" is the latest mtime among the baseline file and every one of
/// that session's event segments, not just the baseline's own: the baseline
/// is written once, at the very start of a playthrough's capture, so going
/// by it alone would rank an old, still actively played session below a
/// brand new one that simply hasn't logged much yet.
pub fn discover_sessions(dir: &Path) -> io::Result<Vec<Session>> {
    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(dir)?.filter_map(Result::ok) {
        let session_dir = entry.path();
        if !session_dir.is_dir() {
            continue;
        }
        let Some(session_id) = parse_session_dir_name(&session_dir) else { continue };

        let baseline_path = session_dir.join("baseline.json");
        if !baseline_path.exists() {
            // Capture started (the folder exists) but the baseline hasn't
            // finished yet, or never will. Not ready to show, same as no
            // folder at all.
            continue;
        }
        let baseline = match Baseline::read_at(&baseline_path) {
            Ok(baseline) => baseline,
            Err(e) => {
                eprintln!("warning: skipping unreadable baseline: {e}");
                continue;
            }
        };

        let mut last_modified =
            baseline_path.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if let Ok(segments) = event::log_paths(&session_dir) {
            for segment in segments {
                if let Ok(modified) = segment.metadata().and_then(|m| m.modified()) {
                    last_modified = last_modified.max(modified);
                }
            }
        }

        sessions.push(Session { session_id, session_dir, baseline_path, baseline, last_modified });
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
    Ok(sessions)
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
    /// Events that changed nothing. A steady trickle is normal: it is the
    /// baseline smear (see `world`) plus removals of entities filtered out of
    /// snapshots. A large fraction suggests events and baseline disagree,
    /// e.g. a log replayed against the wrong save.
    pub no_op_events: usize,
    pub applied_events: usize,
    /// Segments skipped for failing to open (bad magic, unreadable). See the
    /// warning already printed at the skip site in `run`. Never happens for
    /// an uninterrupted capture; the documented cause is a stale-header
    /// segment recreated after `script-output` files were deleted by hand
    /// without running `/timelapse-reset-capture` first.
    pub skipped_segments: usize,
    /// Batches whose tick was less than the highest tick already applied.
    /// Never legitimate within one continuous capture: the same stale-header
    /// segment that causes `skipped_segments` can also land within a
    /// readable file, sharing tick territory with a real one.
    pub out_of_order_batches: usize,
}

/// Seed a world from the baseline at `baseline_path`.
///
/// Surfaces load largest-file-first so the busiest one becomes the default
/// that untagged events fall back to.
pub fn load_baseline(baseline_path: &Path) -> io::Result<Replay> {
    let baseline = Baseline::read_at(baseline_path)?;
    let dir = baseline_path.parent().unwrap_or_else(|| Path::new("."));

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
                "{} names {} surface(s) but none could be loaded. If this session's files were \
                 ever deleted from script-output by hand, run /timelapse-reset-capture in-game \
                 before your next capture.",
                baseline_path.display(),
                baseline.surfaces.len()
            ),
        ));
    }

    world.tick = baseline.tick;
    Ok(Replay { world, baseline, no_op_events: 0, applied_events: 0, skipped_segments: 0, out_of_order_batches: 0 })
}

/// Walk the event log forward, calling `emit` with the world at each frame
/// boundary. Events are applied in whole-tick groups, so a frame is never cut
/// halfway through a tick's changes: a blueprint landing 400 entities on one
/// tick shows up whole or not at all.
///
/// `emit` receives the tick rather than a materialised frame so the caller
/// decides what to do with the world: write every surface, one surface, or
/// just measure. `dir` must be one playthrough's own session folder (a
/// [`Session`]'s `session_dir`), not the shared top-level capture directory:
/// every segment in it is walked, with nothing left to scope by session.
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
        // A session can legitimately span more than one segment (a save
        // reload rolls over to a fresh one; see `next_capture_segment`), and
        // the mod has no way to notice or clean up an orphaned segment left
        // behind by deleting capture files without running
        // `/timelapse-reset-capture` first (its next flush recreates one via
        // a plain append, with no magic header, under the same session tag).
        // One bad segment losing its own events is a much smaller problem
        // than it sinking replay of an otherwise complete session.
        let stream = match event::stream_log(&segment) {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("warning: skipping unreadable event segment {}: {e}", segment.display());
                replay.skipped_segments += 1;
                continue;
            }
        };

        let mut pending: Vec<LoggedEvent> = Vec::new();
        let mut pending_tick = None;

        for logged in stream {
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
/// `frame_<index>_<surface>.stfr`, the same shape the mod's own baseline
/// output uses, and what `viewer::group_by_surface` expects in order to show
/// more than one world. A surface with nothing on it at this tick (not yet
/// built, or already abandoned) is skipped rather than writing an empty
/// file.
///
/// Used by `save-timelapse.exe`'s live-capture flow when the user asks for
/// every surface rather than picking one busiest.
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

/// Writes `surface_name`'s natural-terrain layer to `terrain_<surface>.stfr`
/// in `out`, once. Unlike `write_all_surfaces`, this is not called per
/// emitted frame: terrain is fixed the instant the baseline loads (see
/// `World::terrain_frame`), so one call right after loading the baseline
/// covers the whole replay. Skipped, not an error, when there is no terrain
/// to write (terrain capture was off, or this came from an older capture),
/// so `terrain_<surface>.stfr`'s mere presence tells the viewer whether
/// terrain is available.
pub fn write_terrain(world: &World, surface_name: &str, tick: u64, out: &Path) -> io::Result<()> {
    let frame = world.terrain_frame(surface_name, tick);
    if frame.tiles.is_empty() {
        return Ok(());
    }
    let path = out.join(format!("terrain_{surface_name}.stfr"));
    std::fs::write(&path, frame::write_binary(&frame.as_out()))
}

/// `write_terrain` for every surface `world` has.
pub fn write_all_terrain(world: &World, tick: u64, out: &Path) -> io::Result<()> {
    for surface in world.surface_names() {
        write_terrain(world, surface, tick, out)?;
    }
    Ok(())
}

/// Apply one tick's events together, then clear the buffer for reuse rather
/// than allocating a new one per tick.
fn apply_batch(replay: &mut Replay, pending: &mut Vec<LoggedEvent>) {
    // All events in one batch share a tick (see `run`'s grouping), so the
    // first is representative. Less than the running max is never
    // legitimate within one continuous capture: see `Replay::out_of_order_batches`.
    if let Some(first) = pending.first() {
        if first.tick < replay.world.tick {
            replay.out_of_order_batches += 1;
        }
    }
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

    /// The session id every test capture uses, as both the raw value and the
    /// hex name its session folder carries.
    const TEST_SESSION: u32 = 1;
    const TEST_SESSION_HEX: &str = "00000001";

    /// Builds a capture directory containing one session subfolder, plus the
    /// path to its baseline manifest inside that folder, since `load_baseline`
    /// takes that path directly rather than a directory to search. Callers
    /// needing the session folder itself (to pass to `run`, or to write more
    /// segments into) get it via `baseline_path.parent()`.
    fn capture_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join(TEST_SESSION_HEX);
        fs::create_dir_all(&session_dir).unwrap();

        let baseline_path = session_dir.join("baseline.json");
        fs::write(&baseline_path, r#"{"tick":100,"entities":1,"tiles":0,"surfaces":["nauvis"]}"#).unwrap();

        let entities = vec![frame::Entity { n: "pipe".into(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1 }];
        let out = frame::FrameOut { tick: 100, surface: "nauvis", entities: &entities, tiles: &[] };
        fs::write(session_dir.join("frame_100_nauvis.stfr"), frame::write_binary(&out)).unwrap();

        (dir, baseline_path)
    }

    /// Builds one `events_<session>_<tick>.stev` segment's bytes, tracking
    /// its name and surface dictionaries so a test reads as a sequence of
    /// events rather than a manual byte layout, the way the real writer
    /// builds it while logging as play happens.
    struct TestLog {
        w: ByteWriter,
        names: HashMap<String, u16>,
        surfaces: HashMap<String, u16>,
    }

    impl TestLog {
        fn new() -> Self {
            let mut w = ByteWriter::new();
            w.magic(b"STE1").u8(1); // magic, then the event format's current version
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
        let path = dir.path().join(TEST_SESSION_HEX).join("baseline.json");
        let err = load_baseline(&path).unwrap_err();
        assert!(err.to_string().contains("baseline.json"), "got: {err}");
    }

    #[test]
    fn the_baseline_seeds_the_world() {
        let (_dir, baseline_path) = capture_dir();
        let replay = load_baseline(&baseline_path).unwrap();
        assert_eq!(replay.world.entity_count(), 1);
        assert_eq!(replay.baseline.tick, 100);
        assert_eq!(replay.baseline.session_id, TEST_SESSION);
    }

    #[test]
    fn a_manifest_naming_no_loadable_surface_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join(TEST_SESSION_HEX);
        fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("baseline.json");
        fs::write(&path, r#"{"tick":5,"surfaces":["gone"]}"#).unwrap();
        let err = load_baseline(&path).unwrap_err();
        assert!(err.to_string().contains("/timelapse-reset-capture"), "got: {err}");
    }

    #[test]
    fn replay_applies_events_and_emits_frames_on_the_interval() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(100).add_entity("nauvis", "transport-belt", 1.5, 0.5, 0);
        log.tick(160).add_entity("nauvis", "transport-belt", 2.5, 0.5, 0);
        log.tick(220).remove_entity("nauvis", 0.5, 0.5);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        let mut seen: Vec<(u64, usize)> = Vec::new();
        let options = Options { interval: 50, max_frames: 100 };
        run(&mut replay, session_dir, &options, |world, tick| {
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
        let _dir = dir; // kept alive for the duration of the test
    }

    /// Everything in one tick must land together: a frame boundary inside a
    /// blueprint would show half of it.
    #[test]
    fn events_sharing_a_tick_are_applied_as_one_batch() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(500);
        for i in 0..50 {
            log.add_entity("nauvis", "pipe", i as f32 + 0.5, 9.5, 0);
        }
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        let mut counts = Vec::new();
        let options = Options { interval: 100, max_frames: 100 };
        run(&mut replay, session_dir, &options, |world, _| counts.push(world.entity_count())).unwrap();

        // No frame may show a partial tick: counts jump from 1 to 51.
        assert!(
            counts.iter().all(|&c| c == 1 || c == 51),
            "a frame caught mid-tick: {counts:?}"
        );
        assert_eq!(*counts.last().unwrap(), 51);
        let _dir = dir;
    }

    #[test]
    fn events_before_the_baseline_are_ignored() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(5).remove_entity("nauvis", 0.5, 0.5);
        log.write(session_dir, "events_1.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();
        assert_eq!(replay.world.entity_count(), 1, "the pre-baseline removal was skipped");
        let _dir = dir;
    }

    #[test]
    fn segments_replay_in_tick_order() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut later = TestLog::new();
        later.tick(9000).remove_entity("nauvis", 1.5, 0.5);
        later.write(session_dir, "events_9000.stev");

        let mut earlier = TestLog::new();
        earlier.tick(200).add_entity("nauvis", "pipe", 1.5, 0.5, 0);
        earlier.write(session_dir, "events_200.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();
        // Add at tick 200 then remove at 9000 nets out; replayed the other
        // way round the remove would no-op and the add would survive.
        assert_eq!(replay.world.entity_count(), 1);
        assert_eq!(replay.no_op_events, 0);
        let _dir = dir;
    }

    #[test]
    fn max_frames_caps_output() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(100_000).add_entity("nauvis", "pipe", 5.5, 5.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        let options = Options { interval: 1, max_frames: 10 };
        let mut count = 0;
        let emitted = run(&mut replay, session_dir, &options, |_, _| count += 1).unwrap();
        assert_eq!(emitted, 10);
        assert_eq!(count, 10);
        let _dir = dir;
    }

    #[test]
    fn a_capture_with_no_events_still_emits_the_baseline() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut replay = load_baseline(&baseline_path).unwrap();
        let mut count = 0;
        let emitted = run(&mut replay, session_dir, &Options::default(), |_, _| count += 1).unwrap();
        assert_eq!(emitted, 1);
        assert_eq!(count, 1);
        let _dir = dir;
    }

    #[test]
    fn discover_sessions_ignores_a_leftover_flat_top_level_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join(TEST_SESSION_HEX);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("baseline.json"), r#"{"tick":100,"entities":1,"tiles":0,"surfaces":["nauvis"]}"#)
            .unwrap();
        // The shape every capture used before playthroughs got their own
        // folder: a plain file, not a directory, sitting at the top level.
        fs::write(dir.path().join("baseline.json"), r#"{"tick":1,"surfaces":[]}"#).unwrap();

        let sessions = discover_sessions(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1, "the flat top-level leftover must not be picked up");
        assert_eq!(sessions[0].session_id, TEST_SESSION);
    }

    #[test]
    fn discover_sessions_ignores_a_session_folder_with_no_finished_baseline() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(TEST_SESSION_HEX)).unwrap();
        assert!(discover_sessions(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn discover_sessions_orders_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        for hex in ["00000001", "00000002"] {
            fs::create_dir_all(dir.path().join(hex)).unwrap();
        }
        fs::write(
            dir.path().join("00000001").join("baseline.json"),
            r#"{"tick":100,"entities":1,"tiles":0,"surfaces":["nauvis"]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("00000002").join("baseline.json"),
            r#"{"tick":200,"entities":2,"tiles":0,"surfaces":["nauvis"]}"#,
        )
        .unwrap();

        let older = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("00000001").join("baseline.json"))
            .unwrap()
            .set_modified(older)
            .unwrap();

        let sessions = discover_sessions(dir.path()).unwrap();
        assert_eq!(sessions[0].session_id, 2, "the more recently modified session sorts first");
    }

    /// A real failure mode: clearing script-output by hand without running
    /// `/timelapse-reset-capture` first leaves the mod believing its current
    /// segment is already initialized, so the next flush recreates it via a
    /// plain append with no magic header, alongside whatever segment the
    /// mod starts fresh with afterward, in the same session folder. One
    /// orphaned, unreadable segment must not sink replay of the rest of that
    /// session.
    #[test]
    fn run_skips_an_unreadable_segment_and_still_replays_the_rest_of_the_session() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        fs::write(session_dir.join("events_50.stev"), b"not a real segment").unwrap();

        let mut log = TestLog::new();
        log.tick(150).add_entity("nauvis", "pipe", 5.5, 5.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        let emitted = run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();

        assert!(emitted >= 1, "replay must still complete rather than aborting on the bad segment");
        assert_eq!(replay.world.entity_count(), 2, "the good segment's event must still apply");
        assert_eq!(replay.skipped_segments, 1);
        let _dir = dir;
    }

    /// The documented failure this guards: a stale-header segment recreated
    /// after files were deleted by hand can land within a readable file,
    /// sharing tick territory with a real one, rather than always failing
    /// to open outright.
    #[test]
    fn run_counts_a_batch_whose_tick_regresses_relative_to_the_running_max() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(500).add_entity("nauvis", "pipe", 5.5, 5.5, 0);
        log.tick(200).add_entity("nauvis", "transport-belt", 6.5, 5.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();

        assert_eq!(replay.out_of_order_batches, 1);
        let _dir = dir;
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

    #[test]
    fn write_all_terrain_writes_one_file_per_surface_with_terrain_and_skips_the_rest() {
        let mut world = crate::world::World::new();
        world.load_baseline(&crate::frame::Frame {
            tick: 100,
            surface: "nauvis".to_string(),
            entities: Vec::new(),
            count: 0,
            tiles: vec![
                crate::frame::Tile { n: "grass-1".into(), x: 0, y: 0 },
                crate::frame::Tile { n: "concrete".into(), x: 1, y: 0 },
            ],
        });
        // vulcanus has placed floor but no natural terrain, e.g. terrain
        // capture was off when this baseline was taken.
        world.load_baseline(&crate::frame::Frame {
            tick: 100,
            surface: "vulcanus".to_string(),
            entities: Vec::new(),
            count: 0,
            tiles: vec![crate::frame::Tile { n: "concrete".into(), x: 0, y: 0 }],
        });

        let dir = tempfile::tempdir().unwrap();
        write_all_terrain(&world, 100, dir.path()).unwrap();

        let written: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(written, vec!["terrain_nauvis.stfr"], "vulcanus has no terrain, so it gets no file");

        let bytes = fs::read(dir.path().join("terrain_nauvis.stfr")).unwrap();
        let frame = frame::read_binary(&bytes).unwrap();
        assert_eq!(frame.tiles.len(), 1, "concrete is placed floor, not terrain");
        assert_eq!(frame.tiles[0].n, "grass-1".into());
        assert!(frame.entities.is_empty());
    }

    #[test]
    fn write_terrain_is_a_no_op_when_the_surface_has_no_terrain() {
        let mut world = crate::world::World::new();
        world.load_baseline(&crate::frame::Frame {
            tick: 100,
            surface: "nauvis".to_string(),
            entities: Vec::new(),
            count: 0,
            tiles: vec![crate::frame::Tile { n: "concrete".into(), x: 0, y: 0 }],
        });

        let dir = tempfile::tempdir().unwrap();
        write_terrain(&world, "nauvis", 100, dir.path()).unwrap();

        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0, "no terrain means no file");
    }
}
