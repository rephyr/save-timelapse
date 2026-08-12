//! Reassembling a timeline from a baseline snapshot plus the event log.
//!
//! ```text
//! <session>/baseline.json                 tick + surfaces the baseline covers
//! <session>/frame_<tick>_<surface>.stfr   the baseline itself, one per surface
//! <session>/events_<start_tick>.stev      append-only, one segment per timeline
//! <session>/players.jsonl                 optional, sampled player positions
//! ```
//!
//! `baseline.json` is written last, so its presence means the baseline
//! finished, and never changes after: a surface added to tracking later gets
//! its own frame file with no entry in it, found by scanning.

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

/// Parses a session's hex folder name. `None` for anything else, including a
/// `baseline.json` sitting directly in the shared capture folder, the shape
/// every capture used before playthroughs got their own folders.
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
        let mut baseline: Baseline = serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        baseline.session_id = path.parent().and_then(parse_session_dir_name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: expected a hex-named parent folder to read a session id from", path.display()),
            )
        })?;
        Ok(baseline)
    }

    /// The path of one of this baseline's per-surface snapshot files.
    /// Untagged, the session folder already scoping it.
    pub fn frame_path(&self, dir: &Path, surface: &str) -> PathBuf {
        dir.join(format!("frame_{}_{}.stfr", self.tick, surface))
    }
}

/// A surface added to tracking after the original baseline ran: an ordinary
/// frame file for a surface `baseline.json` does not name. That manifest never
/// changes, so this is how a session accumulates more than one baseline tick.
#[derive(Debug)]
struct CatchUpBaseline {
    tick: u64,
    surface: String,
    path: PathBuf,
}

impl CatchUpBaseline {
    /// Parses a session folder entry the way `viewer::loading::frame_is_candidate`
    /// does, duplicated because `viewer` depends on this crate. Split once on
    /// the first underscore after the index, so a surface name containing one
    /// survives.
    fn from_path(path: &Path) -> Option<CatchUpBaseline> {
        if path.extension().and_then(|e| e.to_str()) != Some("stfr") {
            return None;
        }
        let stem = path.file_stem()?.to_str()?;
        let rest = stem.strip_prefix("frame_")?;
        let mut parts = rest.splitn(2, '_');
        let tick_part = parts.next().unwrap_or("");
        let surface_part = parts.next().unwrap_or("");
        if surface_part.is_empty() || surface_part == "manifest" {
            return None;
        }
        Some(CatchUpBaseline { tick: tick_part.parse().ok()?, surface: surface_part.to_string(), path: path.to_path_buf() })
    }

    /// Reads and parses this catch-up's frame file, deferred until `run`
    /// reaches its tick: a session can accumulate several, each potentially as
    /// large as any other baseline surface.
    fn load(&self) -> io::Result<frame::Frame> {
        let bytes = std::fs::read(&self.path)?;
        frame::read_binary(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Every catch-up baseline in `dir`, ascending by tick. Filtered by exact
/// expected path rather than by re-deriving a (tick, surface) pair, so anything
/// matching the frame shape and not built by `baseline.frame_path` is
/// definitionally a catch-up.
fn discover_catch_up_baselines(dir: &Path, baseline: &Baseline) -> io::Result<Vec<CatchUpBaseline>> {
    let known: std::collections::HashSet<PathBuf> = baseline.surfaces.iter().map(|s| baseline.frame_path(dir, s)).collect();

    let mut catch_ups: Vec<CatchUpBaseline> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| !known.contains(p))
        .filter_map(|p| CatchUpBaseline::from_path(&p))
        .collect();
    catch_ups.sort_by_key(|c| c.tick);
    Ok(catch_ups)
}

/// One playthrough with a finished baseline, as found by [`discover_sessions`].
#[derive(Debug)]
pub struct Session {
    pub session_id: u32,
    /// This session's own subfolder: pass this, not the shared top-level
    /// capture directory, to [`run`] and [`event::log_segments`].
    pub session_dir: PathBuf,
    pub baseline_path: PathBuf,
    pub baseline: Baseline,
    pub last_modified: SystemTime,
}

impl Session {
    /// A name the user gave this playthrough, as a `label.txt` in the session
    /// folder so it travels with the capture: copied elsewhere it keeps its
    /// name, deleted it takes it along. Unreadable or empty is unset.
    pub fn label(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.session_dir.join("label.txt")).ok()?;
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Sets or clears the name. An empty label removes the file rather than
    /// leaving a blank one, so "unset" has one representation.
    pub fn set_label(&self, label: &str) -> io::Result<()> {
        let path = self.session_dir.join("label.txt");
        if label.trim().is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
        std::fs::write(
            path,
            format!(
                "{}
",
                label.trim()
            ),
        )
    }

    /// How much disk this capture occupies, for showing what deleting it would
    /// free. Unreadable entries count as nothing rather than failing the walk:
    /// a slightly low number beats refusing to list the capture.
    pub fn size_on_disk(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
            entries
                .filter_map(Result::ok)
                .map(|entry| match entry.file_type() {
                    Ok(kind) if kind.is_dir() => walk(&entry.path()),
                    _ => entry.metadata().map(|m| m.len()).unwrap_or(0),
                })
                .sum()
        }
        walk(&self.session_dir)
    }

    /// Permanently removes this playthrough's capture. Takes `self` by value,
    /// so holding a stale session is a compile error rather than a surprise.
    pub fn delete(self) -> io::Result<()> {
        std::fs::remove_dir_all(&self.session_dir)
    }
}

/// Every playthrough with a finished baseline among `dir`'s session subfolders,
/// newest first. "Newest" is the latest mtime among the baseline and every
/// segment, not the baseline alone, which is written once at the start and
/// would rank an actively played session below a quiet new one.
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

        let mut last_modified = baseline_path.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if let Ok(segments) = event::log_segments(&session_dir) {
            for segment in segments {
                if let Ok(modified) = segment.path.metadata().and_then(|m| m.modified()) {
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
    /// Events that changed nothing. A steady trickle is the baseline smear
    /// plus removals of entities filtered out of snapshots; a large fraction
    /// suggests a log replayed against the wrong save.
    pub no_op_events: usize,
    pub applied_events: usize,
    /// Segments skipped for failing to open. Never happens for an
    /// uninterrupted capture; the documented cause is a stale-header segment
    /// recreated after `script-output` was deleted by hand.
    pub skipped_segments: usize,
    /// Batches whose tick was below the highest already applied, never
    /// legitimate within one continuous capture: a stale-header segment can
    /// land within a readable file, sharing tick territory with a real one.
    pub out_of_order_batches: usize,
    /// Events dropped for belonging to a timeline the player reloaded away
    /// from. Not a health signal: any nonzero value means the playthrough was
    /// reloaded, and dropping these is what makes the replay match reality.
    pub superseded_events: usize,
    /// Segments holding more than one append run: a capture predating the
    /// same-save-twice rollover fix. Its own counter because a segment
    /// corrupted by hand-deleting `script-output` looks identical.
    pub restarted_segments: usize,
    /// Records stepped over because their tag postdates this build. Not
    /// corruption: the mod writing the capture is newer than the tool reading
    /// it, and the replay is correct as far as it goes.
    pub unknown_extensions: usize,
    /// Catch-up baselines (see [`CatchUpBaseline`]) not yet reached by `run`'s
    /// tick-ordered walk, ascending by tick. Emptied out as `run` applies
    /// each one in turn; never re-populated after `load_baseline`.
    pending_catch_ups: Vec<CatchUpBaseline>,
    /// How many catch-up baselines `run` applied. Orthogonal to the event
    /// counters, loading a baseline not being an event.
    pub catch_ups_applied: usize,
}

/// Seed a world from the baseline at `baseline_path`.
///
/// Surfaces load largest-file-first so the busiest one becomes the default
/// that untagged events fall back to.
pub fn load_baseline(baseline_path: &Path) -> io::Result<Replay> {
    let baseline = Baseline::read_at(baseline_path)?;
    let dir = baseline_path.parent().unwrap_or_else(|| Path::new("."));

    let mut paths: Vec<PathBuf> = baseline.surfaces.iter().map(|s| baseline.frame_path(dir, s)).collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.metadata().map(|m| m.len()).unwrap_or(0)));

    let mut world = World::new();
    // Before any baseline, since both splits happen as the baseline arrives.
    // Absent for every capture older than the file, which falls back to the
    // built-in lists (see `world::is_placed_floor`, `world::is_known_resource`).
    if let Some(prototypes) = crate::prototypes::read(dir) {
        world.set_resources(prototypes.resource_names());
        world.set_floor(prototypes.floor);
    }
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

    let pending_catch_ups = discover_catch_up_baselines(dir, &baseline).unwrap_or_else(|e| {
        eprintln!("warning: could not scan {} for catch-up baselines: {e}", dir.display());
        Vec::new()
    });

    Ok(Replay {
        world,
        baseline,
        no_op_events: 0,
        applied_events: 0,
        skipped_segments: 0,
        out_of_order_batches: 0,
        superseded_events: 0,
        restarted_segments: 0,
        unknown_extensions: 0,
        pending_catch_ups,
        catch_ups_applied: 0,
    })
}

/// Every surface the baseline, a pending catch-up, or the event log names, for
/// offering a complete choice before the expensive replay. The baseline alone
/// is not enough: a planet first visited after capture started has no entry but
/// its events name it, and a surface with a catch-up and no events yet is
/// invisible to both.
pub fn discover_surfaces(session_dir: &Path, replay: &Replay) -> io::Result<Vec<String>> {
    let mut surfaces: std::collections::BTreeSet<String> = replay.world.surface_names().into_iter().map(String::from).collect();
    surfaces.extend(replay.pending_catch_ups.iter().map(|c| c.surface.clone()));

    // Bounded per append run rather than per segment, so a surface existing
    // only in a timeline the player reloaded away from is not offered. Reloads
    // land inside a segment, so the per-run bound is the one that fires.
    for segment in event::log_segments(session_dir)? {
        let run_bounds = event::segment_run_bounds(&segment.path, segment.end_tick).unwrap_or_default();
        if let Ok(stream) = event::stream_log(&segment.path) {
            let mut run = 0usize;
            let mut previous_tick: Option<u64> = None;
            for logged in stream {
                if previous_tick.is_some_and(|p| logged.tick < p) {
                    run += 1;
                }
                previous_tick = Some(logged.tick);
                if logged.tick < run_bounds.get(run).copied().unwrap_or(segment.end_tick) {
                    surfaces.insert(logged.surface);
                }
            }
        }
    }

    Ok(surfaces.into_iter().collect())
}

/// Walk the event log forward, calling `emit` at each frame boundary. Events
/// apply in whole-tick groups, so a blueprint landing 400 entities shows up
/// whole or not at all.
///
/// `emit` receives the tick rather than a materialised frame, so the caller
/// decides what to do with the world. `dir` must be one playthrough's session
/// folder.
pub fn run<F>(replay: &mut Replay, dir: &Path, options: &Options, mut emit: F) -> io::Result<usize>
where
    F: FnMut(&World, u64),
{
    let mut next_emit = replay.baseline.tick;
    let mut emitted = 0;

    let mut flush_until = |replay: &mut Replay, tick: u64, next: &mut u64, emitted: &mut usize| {
        while tick >= *next && *emitted < options.max_frames {
            apply_due_catch_ups(replay, *next);
            emit(&replay.world, *next);
            *emitted += 1;
            *next += options.interval;
        }
    };

    for segment in event::log_segments(dir)? {
        // A session can span several segments, and the mod cannot clean up one
        // orphaned by deleting capture files by hand. One bad segment losing
        // its own events beats it sinking the rest of the session.
        let mut stream = match event::stream_log(&segment.path) {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("warning: skipping unreadable event segment {}: {e}", segment.path.display());
                replay.skipped_segments += 1;
                continue;
            }
        };

        // One bound per append run: more than one means the file predates the
        // same-save-twice rollover fix. A failed scan leaves the segment on
        // its single bound rather than dropping it.
        let run_bounds = event::segment_run_bounds(&segment.path, segment.end_tick).unwrap_or_default();
        if run_bounds.len() > 1 {
            replay.restarted_segments += 1;
        }
        let mut run = 0usize;
        let mut previous_tick: Option<u64> = None;

        let mut pending: Vec<LoggedEvent> = Vec::new();
        let mut pending_tick = None;

        for logged in &mut stream {
            // Tracked over every record rather than only the kept ones: a run
            // boundary is a fact about append order, so skipping a record must
            // not move it.
            if previous_tick.is_some_and(|p| logged.tick < p) {
                run += 1;
            }
            previous_tick = Some(logged.tick);

            // Events at or past this run's bound describe a timeline reloaded
            // away from. Skipped one by one rather than breaking out: what
            // follows is the replacement, not the end of the useful data.
            let bound = run_bounds.get(run).copied().unwrap_or(segment.end_tick);
            if logged.tick >= bound {
                replay.superseded_events += 1;
                continue;
            }
            // Events before the baseline describe a world we did not capture.
            if logged.tick < replay.baseline.tick {
                continue;
            }
            // Same per surface: the mod logs a surface's events the instant it
            // is included, but its catch-up snapshot is not taken until
            // BASELINE_WARNING_DELAY_TICKS later, and already reflects them.
            if pending_catch_up_tick(&replay.pending_catch_ups, &logged.surface).is_some_and(|t| logged.tick < t) {
                continue;
            }

            // Left to the per-tick batching, a catch-up would only apply once
            // some later event triggered a flush, by which point that event is
            // in the world too and every frame across the gap shows it.
            while replay.pending_catch_ups.first().is_some_and(|c| c.tick <= logged.tick) {
                let catch_up_tick = replay.pending_catch_ups[0].tick;
                apply_batch(replay, &mut pending);
                pending_tick = None;
                flush_until(replay, catch_up_tick, &mut next_emit, &mut emitted);
                // `flush_until`'s own call only fires when a checkpoint lands
                // at or past this tick, which a coarse interval can overshoot.
                // Applying directly also keeps this loop from spinning.
                apply_due_catch_ups(replay, catch_up_tick);
            }

            if pending_tick != Some(logged.tick) {
                apply_batch(replay, &mut pending);
                // Boundaries strictly before this tick, not just up to the
                // batch applied: across a gap they would otherwise stay
                // unflushed until the event ending it was already in the
                // world, showing it as present from the gap's start.
                flush_until(replay, logged.tick.saturating_sub(1), &mut next_emit, &mut emitted);
                pending_tick = Some(logged.tick);
            }
            pending.push(logged);
        }

        // Asked after the walk, not during: the count is of what iteration
        // actually stepped over, so it is only complete once the stream is.
        replay.unknown_extensions += stream.unknown_extensions();

        apply_batch(replay, &mut pending);
        if let Some(tick) = pending_tick {
            flush_until(replay, tick, &mut next_emit, &mut emitted);
        }
    }

    // A catch-up still outstanding has no later event to trigger it, but a
    // completed file is genuinely part of the base as it was when capture
    // stopped.
    apply_due_catch_ups(replay, u64::MAX);

    // Always land a final frame on the finished world, so the timelapse ends
    // on the base as it actually was rather than at the last interval
    // boundary that happened to fall before the last event.
    if emitted < options.max_frames {
        emit(&replay.world, replay.world.tick.max(next_emit.saturating_sub(options.interval)));
        emitted += 1;
    }

    Ok(emitted)
}

/// The tick `surface`'s catch-up baseline is scheduled for, if still
/// outstanding. Only answers "is there a tick this surface's events must not
/// predate"; whether it has been applied yet does not matter here.
fn pending_catch_up_tick(pending: &[CatchUpBaseline], surface: &str) -> Option<u64> {
    pending.iter().find(|c| c.surface == surface).map(|c| c.tick)
}

/// Applies every pending catch-up baseline whose own tick is at most `tick`,
/// removing each from `replay.pending_catch_ups` as it lands (the list stays
/// sorted ascending, see `discover_catch_up_baselines`).
fn apply_due_catch_ups(replay: &mut Replay, tick: u64) {
    while replay.pending_catch_ups.first().is_some_and(|c| c.tick <= tick) {
        let catch_up = replay.pending_catch_ups.remove(0);
        match catch_up.load() {
            Ok(frame) => {
                if frame.tick < replay.world.tick {
                    eprintln!(
                        "warning: catch-up baseline {} claims tick {} but replay has already \
                         reached tick {}; loading it anyway (a baseline load never moves the \
                         clock backward), but this file's tick looks wrong",
                        catch_up.path.display(),
                        frame.tick,
                        replay.world.tick
                    );
                }
                replay.world.load_baseline(&frame);
                replay.catch_ups_applied += 1;
            }
            Err(e) => eprintln!(
                "warning: catch-up baseline {} unreadable or unparseable: {e}. That surface's \
                 state as of when it was added to tracking will be missing from this replay; \
                 anything built on it afterward should still show up.",
                catch_up.path.display()
            ),
        }
    }
}

/// Writes every surface `world` has at `tick`, one file each, in the shape the
/// mod's own baseline output uses. A surface with nothing on it is skipped.
///
/// A surface unchanged since its last written frame is skipped too, which is
/// where nearly all of a multi-surface export's output went. Callers keep
/// `written`, mapping surface to last written revision, across the run.
///
/// The gaps that leaves are not new, the "nothing on it" skip having always
/// produced them, and `viewer::loading::group_by_surface` expands them back
/// out. The gap is the record, so there is no sidecar to keep in sync.
pub fn write_all_surfaces(
    world: &World,
    tick: u64,
    out: &Path,
    index: usize,
    written: &mut std::collections::HashMap<String, u64>,
) -> io::Result<usize> {
    let mut files = 0;
    for surface in world.surface_names() {
        let revision = match world.surface(surface) {
            Some(s) => s.revision(),
            None => continue,
        };

        // On a real nine-surface capture, re-writing unchanged surfaces was
        // 93% of the bytes. Against this surface's own last revision, not a
        // global one: "did anything change anywhere" is near always true.
        // Checked before `to_frame`, materialising a frame to discover it is
        // unchanged being the expensive half.
        if written.get(surface) == Some(&revision) {
            continue;
        }

        let frame = world.to_frame(surface, tick);
        if frame.entities.is_empty() && frame.tiles.is_empty() {
            continue;
        }
        let path = out.join(format!("frame_{index:04}_{surface}.stfr"));
        std::fs::write(&path, frame::write_binary(&frame.as_out()))?;
        written.insert(surface.to_string(), revision);
        files += 1;
    }
    Ok(files)
}

/// Writes `surface_name`'s terrain layer, once: terrain is fixed the instant
/// the baseline loads. Skipped rather than an error when there is none, so the
/// file's presence tells the viewer whether terrain is available.
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

/// Sizing the "skip unchanged frames" idea against a real capture, which no
/// fixture can stand in for: the whole answer is a property of how somebody
/// actually played, not of the code.
#[cfg(test)]
mod idle_study {
    use super::*;

    /// Reports how much of an export is frames identical to the one before.
    ///
    /// ```text
    /// SAVE_TIMELAPSE_CAPTURE='<...>/save-timelapse/<session>' \
    ///   cargo test --lib measure_unchanged_frames -- --ignored --nocapture
    /// ```
    ///
    /// `SAVE_TIMELAPSE_FRAME_SECONDS` overrides the 60 seconds per frame, worth
    /// sweeping: a coarse interval gives each frame more chance of containing a
    /// change. Measured per surface, a global check being near always true.
    #[test]
    #[ignore]
    fn measure_unchanged_frames() {
        let dir = std::env::var("SAVE_TIMELAPSE_CAPTURE")
            .expect("set SAVE_TIMELAPSE_CAPTURE to one session folder under script-output");
        let dir = Path::new(&dir);
        let frame_seconds: u64 = std::env::var("SAVE_TIMELAPSE_FRAME_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);

        let mut replay = load_baseline(&dir.join("baseline.json")).expect("baseline must load");
        let surfaces: Vec<String> = replay.world.surface_names().iter().map(|s| s.to_string()).collect();
        assert!(!surfaces.is_empty(), "a capture with no surfaces has nothing to measure");

        let options = Options { interval: frame_seconds * 60, max_frames: 100_000 };

        // Every surface, because that is what an export writes; measuring one
        // understates it badly. Serialized for real rather than estimated, the
        // question being bytes actually written. The comparison skips the tick
        // and checksum, which is "same contents, different moment".
        let mut totals = vec![(0usize, 0usize, 0usize); surfaces.len()]; // files, bytes, duplicate files
        let mut duplicate_bytes = vec![0usize; surfaces.len()];
        let mut previous: Vec<Option<Vec<u8>>> = vec![None; surfaces.len()];

        let emitted = run(&mut replay, dir, &options, |world, tick| {
            for (i, surface) in surfaces.iter().enumerate() {
                let bytes = crate::frame::write_binary(&world.to_frame(surface, tick).as_out());
                let body = |b: &[u8]| b[13..b.len() - 4].to_vec();
                if previous[i].as_deref().map(|p| body(p) == body(&bytes)).unwrap_or(false) {
                    totals[i].2 += 1;
                    duplicate_bytes[i] += bytes.len();
                }
                totals[i].0 += 1;
                totals[i].1 += bytes.len();
                previous[i] = Some(bytes);
            }
        })
        .expect("replay must run");

        let pct = |part: usize, whole: usize| if whole == 0 { 0.0 } else { part as f64 * 100.0 / whole as f64 };
        let mb = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);

        println!("\nIDLE STUDY  {frame_seconds}s per frame, {emitted} frames, {} surfaces", surfaces.len());
        println!("  {:<14} {:>7} {:>9} {:>10} {:>9}", "surface", "files", "dup", "written", "wasted");
        for (i, surface) in surfaces.iter().enumerate() {
            let (files, bytes, dups) = totals[i];
            println!(
                "  {:<14} {:>7} {:>8.1}% {:>8.1} MB {:>6.1} MB",
                surface,
                files,
                pct(dups, files),
                mb(bytes),
                mb(duplicate_bytes[i])
            );
        }
        let files: usize = totals.iter().map(|t| t.0).sum();
        let bytes: usize = totals.iter().map(|t| t.1).sum();
        let dups: usize = totals.iter().map(|t| t.2).sum();
        let wasted: usize = duplicate_bytes.iter().sum();
        println!("  {:<14} {:>7} {:>8.1}% {:>8.1} MB {:>6.1} MB", "TOTAL", files, pct(dups, files), mb(bytes), mb(wasted));
        println!("  bytes wasted: {:.1}%", pct(wasted, bytes));
        println!("  applied events {}  no-op {}", replay.applied_events, replay.no_op_events);
    }

    /// What the export writes against what it would have written before a
    /// surface could be skipped. Runs the real `write_all_surfaces` into a
    /// scratch directory, so this measures the shipped path.
    #[test]
    #[ignore]
    fn measure_export_size() {
        let dir = std::env::var("SAVE_TIMELAPSE_CAPTURE")
            .expect("set SAVE_TIMELAPSE_CAPTURE to one session folder under script-output");
        let dir = Path::new(&dir);
        let frame_seconds: u64 = std::env::var("SAVE_TIMELAPSE_FRAME_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);

        let mut replay = load_baseline(&dir.join("baseline.json")).expect("baseline must load");
        let out = tempfile::tempdir().unwrap();
        let options = Options { interval: frame_seconds * 60, max_frames: 100_000 };

        let mut revisions = std::collections::HashMap::new();
        let (mut index, mut files) = (0usize, 0usize);
        run(&mut replay, dir, &options, |world, tick| {
            files += write_all_surfaces(world, tick, out.path(), index, &mut revisions).expect("write");
            index += 1;
        })
        .expect("replay must run");

        let bytes: u64 = std::fs::read_dir(out.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();

        let surfaces = replay.world.surface_names().len();
        let mb = |b: f64| b / (1024.0 * 1024.0);
        println!("\nEXPORT SIZE  {frame_seconds}s per frame");
        println!("  frames             {index}");
        println!("  surfaces           {surfaces}");
        println!("  files written      {files}   (every surface every frame would be {})", index * surfaces);
        println!("  bytes on disk      {:.1} MB", mb(bytes as f64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::set_mtime_rank;
    use crate::wire::ByteWriter;
    use std::collections::HashMap;
    use std::fs;

    /// The session id every test capture uses, as both the raw value and the
    /// hex name its session folder carries.
    const TEST_SESSION: u32 = 1;
    const TEST_SESSION_HEX: &str = "00000001";

    /// A capture directory with one session subfolder, plus the path to its
    /// baseline manifest, `load_baseline` taking that path rather than a
    /// directory. Callers needing the folder use `baseline_path.parent()`.
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

    /// Builds one segment's bytes, tracking its dictionaries so a test reads as
    /// a sequence of events rather than a manual byte layout.
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

    /// A planet first visited after capture started has no baseline entry, so
    /// the surface list shown before the replay must check the event log.
    #[test]
    fn discover_surfaces_includes_a_surface_that_only_appears_in_events() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(150).add_entity("gleba", "assembling-machine-1", 5.5, 5.5, 42);
        log.write(session_dir, "events_100.stev");

        let replay = load_baseline(&baseline_path).unwrap();
        let surfaces = discover_surfaces(session_dir, &replay).unwrap();

        assert_eq!(surfaces, vec!["gleba".to_string(), "nauvis".to_string()]);
        let _dir = dir;
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
        assert!(counts.iter().all(|&c| c == 1 || c == 51), "a frame caught mid-tick: {counts:?}");
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

        // Written newest first but played in the other order. Stamped rather
        // than inferred, write order asserting the opposite of the truth.
        set_mtime_rank(session_dir, "events_200.stev", 0);
        set_mtime_rank(session_dir, "events_9000.stev", 1);

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
    fn a_capture_starts_unnamed_and_keeps_a_name_once_given_one() {
        let (dir, baseline_path) = capture_dir();
        let session = &discover_sessions(dir.path()).unwrap()[0];

        assert_eq!(session.label(), None, "a fresh capture has no name");
        session.set_label("Gleba run").unwrap();
        assert_eq!(session.label(), Some("Gleba run".to_string()));

        // Re-discovered from disk, since the name has to outlive the process
        // that set it.
        let again = &discover_sessions(dir.path()).unwrap()[0];
        assert_eq!(again.label(), Some("Gleba run".to_string()));
        let _ = baseline_path;
    }

    #[test]
    fn an_empty_name_clears_it_rather_than_storing_a_blank() {
        let (dir, _) = capture_dir();
        let session = &discover_sessions(dir.path()).unwrap()[0];
        session.set_label("temporary").unwrap();
        session.set_label("   ").unwrap();
        assert_eq!(session.label(), None);
        // Clearing an already-unset name is not an error.
        session.set_label("").unwrap();
    }

    /// The label file sits inside the session folder, so it must not be
    /// mistaken for capture data by anything scanning it.
    #[test]
    fn a_named_capture_still_discovers_and_replays_normally() {
        let (dir, baseline_path) = capture_dir();
        discover_sessions(dir.path()).unwrap()[0].set_label("Named").unwrap();

        let sessions = discover_sessions(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, &sessions[0].session_dir, &Options::default(), |_, _| {}).unwrap();
        assert_eq!(replay.world.entity_count(), 1, "the label must not disturb replay");
    }

    #[test]
    fn size_on_disk_counts_the_whole_capture() {
        let (dir, _) = capture_dir();
        let session = &discover_sessions(dir.path()).unwrap()[0];
        let before = session.size_on_disk();
        assert!(before > 0, "a capture with a baseline is not zero bytes");

        std::fs::write(session.session_dir.join("players.jsonl"), vec![b'x'; 512]).unwrap();
        assert!(session.size_on_disk() >= before + 512, "a new sidecar must be counted");
    }

    #[test]
    fn deleting_a_capture_removes_it_from_discovery() {
        let (dir, _) = capture_dir();
        let mut sessions = discover_sessions(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);

        sessions.remove(0).delete().unwrap();
        assert!(discover_sessions(dir.path()).unwrap().is_empty(), "the capture is gone");
    }

    #[test]
    fn discover_sessions_ignores_a_leftover_flat_top_level_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join(TEST_SESSION_HEX);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("baseline.json"), r#"{"tick":100,"entities":1,"tiles":0,"surfaces":["nauvis"]}"#).unwrap();
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

    /// Clearing script-output by hand leaves the mod believing its segment is
    /// initialized, so the next flush recreates it via a plain append with no
    /// magic header. One orphaned segment must not sink the rest.
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

    /// Ticks going backwards inside one segment, which is what a capture from
    /// before the same-save-twice rollover fix looks like: the second attempt
    /// was appended onto the first.
    ///
    /// Read as a run boundary rather than damage, so only the second attempt
    /// survives. A segment corrupted by hand-deleting `script-output` could
    /// look the same, which is what `restarted_segments` stays visible for.
    #[test]
    fn ticks_regressing_inside_one_segment_supersede_the_attempt_before_them() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(500).add_entity("nauvis", "pipe", 5.5, 5.5, 0);
        log.tick(200).add_entity("nauvis", "transport-belt", 6.5, 5.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();

        assert_eq!(replay.restarted_segments, 1);
        assert_eq!(replay.superseded_events, 1, "the tick-500 pipe, from the abandoned attempt");
        assert_eq!(replay.applied_events, 1, "the tick-200 belt, from the attempt that stuck");
        assert_eq!(replay.out_of_order_batches, 0, "the split means nothing regresses at apply time");
        assert_eq!(replay.world.entity_count(), 2, "the baseline's pipe plus the surviving belt");
        let _dir = dir;
    }

    /// The second attempt replaces the first. Both build at the same tick and
    /// spot, so a replay applying both would still look fine; the position
    /// built only in the abandoned attempt is what makes it observable.
    #[test]
    fn a_same_save_reload_inside_one_segment_does_not_leak_the_abandoned_attempt() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        // First attempt from tick 1000: built a belt, then a furnace.
        log.tick(1000).add_entity("nauvis", "transport-belt", 5.5, 5.5, 10);
        log.tick(1200).add_entity("nauvis", "stone-furnace", 40.5, 40.5, 11);
        // Reloaded the same save and tried again: same belt, no furnace.
        log.tick(1000).add_entity("nauvis", "transport-belt", 5.5, 5.5, 20);
        log.tick(1300).add_entity("nauvis", "chemical-plant", 60.5, 60.5, 21);
        log.write(session_dir, "events_1000.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();

        let names: std::collections::BTreeSet<String> =
            replay.world.surface("nauvis").unwrap().entities().map(|e| replay.world.names().name(e.name).to_string()).collect();
        assert_eq!(
            names,
            ["pipe", "transport-belt", "chemical-plant"].iter().map(|s| s.to_string()).collect(),
            "the abandoned attempt's furnace must not survive"
        );
        assert_eq!(replay.restarted_segments, 1);
        let _dir = dir;
    }

    /// The cross-segment half of reload handling: a fresh segment starts at
    /// the resumed tick while the old one is abandoned on disk, still holding
    /// events for a future that never happened. The assembling machine must
    /// never show, at any tick.
    #[test]
    fn a_reload_to_an_earlier_save_must_not_leak_the_abandoned_futures_events() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();

        let mut old = TestLog::new();
        old.tick(200).add_entity("nauvis", "transport-belt", 2.5, 0.5, 10);
        old.tick(4000).add_entity("nauvis", "assembling-machine-1", 9.5, 9.5, 20);
        old.write(session_dir, "events_100.stev");

        let mut new = TestLog::new();
        new.tick(3500).add_entity("nauvis", "transport-belt", 5.5, 0.5, 30);
        new.write(session_dir, "events_3000.stev");

        // The reload's segment was created second, so it is the newer file.
        set_mtime_rank(session_dir, "events_100.stev", 0);
        set_mtime_rank(session_dir, "events_3000.stev", 1);

        let mut replay = load_baseline(&baseline_path).unwrap();
        let options = Options { interval: 100, max_frames: 1000 };
        let mut seen: Vec<(u64, usize)> = Vec::new();
        run(&mut replay, session_dir, &options, |world, tick| {
            seen.push((tick, world.entity_count()));
        })
        .unwrap();

        // The real timeline: the baseline's pipe until the belt lands at 200,
        // both until the post-reload belt at 3500, all three after. The
        // erased assembling machine has no tick at which it may appear.
        for &(tick, count) in &seen {
            let expected = if tick < 200 {
                1
            } else if tick < 3500 {
                2
            } else {
                3
            };
            assert_eq!(count, expected, "tick {tick} shows {count} entities");
        }

        let abandoned_survived = replay.world.surface("nauvis").unwrap().entities().any(|e| e.x == 9.5 && e.y == 9.5);
        assert!(!abandoned_survived, "the abandoned future's entity must not survive the reload");
        // baseline's pipe + the tick-200 belt (before the reload point) + the
        // real tick-3500 belt (after it) = 3; never 4, which would mean the
        // erased assembling machine is still there.
        assert_eq!(replay.world.entity_count(), 3);
        let _dir = dir;
    }

    /// Two reloads, the second reaching further back, which start-tick ordering
    /// cannot express: the segment created last has the middle start tick. Only
    /// the belt and the chemical plant are real.
    #[test]
    fn a_second_reload_reaching_further_back_supersedes_the_first_reloads_events() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();

        let mut first = TestLog::new();
        first.tick(300).add_entity("nauvis", "transport-belt", 3.5, 0.5, 10);
        first.tick(2000).add_entity("nauvis", "stone-furnace", 20.5, 0.5, 11);
        first.write(session_dir, "events_0.stev");

        let mut second = TestLog::new();
        second.tick(1500).add_entity("nauvis", "assembling-machine-1", 30.5, 0.5, 20);
        second.write(session_dir, "events_1000.stev");

        let mut third = TestLog::new();
        third.tick(800).add_entity("nauvis", "chemical-plant", 40.5, 0.5, 30);
        third.write(session_dir, "events_500.stev");

        set_mtime_rank(session_dir, "events_0.stev", 0);
        set_mtime_rank(session_dir, "events_1000.stev", 1);
        set_mtime_rank(session_dir, "events_500.stev", 2);

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();

        let surviving: std::collections::BTreeSet<String> =
            replay.world.surface("nauvis").unwrap().entities().map(|e| replay.world.names().name(e.name).to_string()).collect();
        assert_eq!(
            surviving,
            ["pipe", "transport-belt", "chemical-plant"].iter().map(|s| s.to_string()).collect(),
            "only the baseline's pipe, the pre-reload belt, and the post-reload chemical plant are real"
        );
        // The tick-2000 furnace and the tick-1500 assembling machine.
        assert_eq!(replay.superseded_events, 2);
        let _dir = dir;
    }

    /// Nothing to do with reloads: a forward capture with a long idle gap.
    /// Every boundary in that gap must show the world as it stood then, not as
    /// it stood once the gap ended.
    #[test]
    fn frames_in_a_gap_between_events_show_the_state_at_their_own_tick() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        let mut log = TestLog::new();
        log.tick(200).add_entity("nauvis", "transport-belt", 2.5, 0.5, 10);
        log.tick(5000).add_entity("nauvis", "assembling-machine-1", 9.5, 9.5, 20);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        let options = Options { interval: 100, max_frames: 1000 };
        let mut seen: Vec<(u64, usize)> = Vec::new();
        run(&mut replay, session_dir, &options, |world, tick| {
            seen.push((tick, world.entity_count()));
        })
        .unwrap();

        // Between tick 200 and tick 5000 the world holds exactly the
        // baseline's pipe plus the tick-200 belt.
        for &(tick, count) in &seen {
            if (200..5000).contains(&tick) {
                assert_eq!(count, 2, "tick {tick} sits in the idle gap but shows {count} entities");
            }
        }
        let _dir = dir;
    }

    /// Writes a hand-built catch-up baseline into a session folder, the shape
    /// `M.export_surfaces_to` produces: an ordinary frame file with no manifest
    /// entry.
    fn write_catch_up_frame(session_dir: &Path, tick: u64, surface: &str, entities: Vec<frame::Entity>) {
        let out = frame::FrameOut { tick, surface, entities: &entities, tiles: &[] };
        fs::write(session_dir.join(format!("frame_{tick}_{surface}.stfr")), frame::write_binary(&out)).unwrap();
    }

    fn vulcanus_entity(x: f32, y: f32) -> frame::Entity {
        frame::Entity { n: "pipe".into(), x, y, d: 0, w: 1, h: 1 }
    }

    /// The core scenario this whole feature exists for: a surface included
    /// after the original baseline already ran gets its own later baseline,
    /// and must not appear in any frame before that baseline's own tick.
    #[test]
    fn catch_up_baseline_surface_appears_only_from_its_own_tick_onward() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        write_catch_up_frame(session_dir, 500, "vulcanus", vec![vulcanus_entity(1.5, 1.5), vulcanus_entity(2.5, 1.5)]);

        let mut log = TestLog::new();
        // Before the catch-up: logged (the mod starts logging the instant a
        // surface is included) but must not apply, since it predates the
        // snapshot taken after it.
        log.tick(300).add_entity("vulcanus", "transport-belt", 9.5, 9.5, 0);
        // After the catch-up: a real, applicable change.
        log.tick(600).add_entity("vulcanus", "transport-belt", 3.5, 1.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        let mut seen: Vec<(u64, usize)> = Vec::new();
        let options = Options { interval: 50, max_frames: 1000 };
        run(&mut replay, session_dir, &options, |world, tick| {
            seen.push((tick, world.surface("vulcanus").map(|s| s.entity_count()).unwrap_or(0)));
        })
        .unwrap();

        for &(tick, count) in &seen {
            if tick < 500 {
                assert_eq!(count, 0, "vulcanus must not exist before its own catch-up tick {tick}");
            }
        }
        assert_eq!(
            seen.iter().find(|(tick, _)| *tick == 500).unwrap().1,
            2,
            "the catch-up snapshot's own 2 entities, nothing from the dropped tick-300 event"
        );
        assert_eq!(seen.last().unwrap().1, 3, "the snapshot's 2 plus the tick-600 event's 1");
        assert_eq!(replay.catch_ups_applied, 1);
        let _dir = dir;
    }

    /// A pre-catch-up event for that surface is dropped outright, not
    /// queued to apply once the catch-up lands: its distinct position would
    /// show up in the final count if it were.
    #[test]
    fn events_for_a_pending_catch_up_surface_before_its_tick_are_dropped_not_deferred() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        write_catch_up_frame(session_dir, 500, "vulcanus", vec![vulcanus_entity(1.5, 1.5)]);

        let mut log = TestLog::new();
        log.tick(300).add_entity("vulcanus", "transport-belt", 40.5, 40.5, 0); // distinct position, must never show up
        log.tick(600).add_entity("vulcanus", "transport-belt", 3.5, 1.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();

        assert_eq!(
            replay.world.surface("vulcanus").unwrap().entity_count(),
            2,
            "only the snapshot's entity plus the tick-600 add; the tick-300 add must be gone entirely"
        );
        let _dir = dir;
    }

    /// The documented failure this guards against, applied to catch-ups: a
    /// catch-up baseline with a tick behind ticks already processed (a
    /// corrupt or misnamed file) must never rewind the replayed clock.
    #[test]
    fn a_catch_up_with_a_stale_filename_tick_does_not_move_world_tick_backward() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        // Behind the session's own baseline tick of 100.
        write_catch_up_frame(session_dir, 50, "vulcanus", vec![vulcanus_entity(1.5, 1.5)]);

        let mut log = TestLog::new();
        log.tick(5000).add_entity("nauvis", "pipe", 9.5, 9.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        let mut ticks: Vec<u64> = Vec::new();
        run(&mut replay, session_dir, &Options::default(), |_, tick| ticks.push(tick)).unwrap();

        assert!(ticks.windows(2).all(|w| w[0] <= w[1]), "emitted ticks must never go backward: {ticks:?}");
        assert_eq!(replay.world.tick, 5000);
        let _dir = dir;
    }

    /// Loading a catch-up baseline is not an event: it must not be folded
    /// into the counters that describe the event log's own health.
    #[test]
    fn catch_up_baselines_do_not_affect_the_event_or_batch_counters() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        write_catch_up_frame(session_dir, 500, "vulcanus", vec![vulcanus_entity(1.5, 1.5)]);

        let mut log = TestLog::new();
        log.tick(300).add_entity("vulcanus", "transport-belt", 40.5, 40.5, 0);
        log.tick(600).add_entity("vulcanus", "transport-belt", 3.5, 1.5, 0);
        log.write(session_dir, "events_100.stev");

        let mut replay = load_baseline(&baseline_path).unwrap();
        run(&mut replay, session_dir, &Options::default(), |_, _| {}).unwrap();

        assert_eq!(replay.applied_events, 1, "only the tick-600 event; tick-300 is dropped before it ever reaches apply_batch");
        assert_eq!(replay.no_op_events, 0);
        assert_eq!(replay.out_of_order_batches, 0);
        assert_eq!(replay.catch_ups_applied, 1);
        let _dir = dir;
    }

    #[test]
    fn discover_surfaces_includes_a_pending_catch_up_surface_with_no_events_of_its_own() {
        let (dir, baseline_path) = capture_dir();
        let session_dir = baseline_path.parent().unwrap();
        write_catch_up_frame(session_dir, 500, "vulcanus", vec![vulcanus_entity(1.5, 1.5)]);

        let replay = load_baseline(&baseline_path).unwrap();
        let surfaces = discover_surfaces(session_dir, &replay).unwrap();

        assert_eq!(surfaces, vec!["nauvis".to_string(), "vulcanus".to_string()]);
        let _dir = dir;
    }

    #[test]
    fn catch_up_filename_parsing_keeps_an_underscore_containing_surface_name_intact() {
        let path = Path::new("frame_500_my_modded_planet.stfr");
        let parsed = CatchUpBaseline::from_path(path).unwrap();
        assert_eq!(parsed.tick, 500);
        assert_eq!(parsed.surface, "my_modded_planet");
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
        write_all_surfaces(&world, 100, dir.path(), 7, &mut Default::default()).unwrap();

        let written: Vec<String> =
            fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(written, vec!["frame_0007_nauvis.stfr"], "the empty vulcanus surface must not get a file");
    }

    /// The saving itself: on a real nine-surface capture 93% of the bytes were
    /// a surface re-serialized unchanged. The gap left in that surface's
    /// indices is the record that it did not change.
    #[test]
    fn an_unchanged_surface_is_not_written_again() {
        let entity = |x: f32, y: f32| crate::frame::Entity { n: "pipe".into(), x, y, d: 0, w: 1, h: 1 };
        let baseline = |surface: &str, x: f32| crate::frame::Frame {
            tick: 100,
            surface: surface.to_string(),
            entities: vec![entity(x, 2.0)],
            count: 1,
            tiles: Vec::new(),
        };

        let mut world = crate::world::World::new();
        world.load_baseline(&baseline("nauvis", 1.0));
        world.load_baseline(&baseline("gleba", 50.0));

        let dir = tempfile::tempdir().unwrap();
        let mut revisions = std::collections::HashMap::new();

        assert_eq!(
            write_all_surfaces(&world, 100, dir.path(), 0, &mut revisions).unwrap(),
            2,
            "both surfaces are new at the first frame"
        );
        assert_eq!(
            write_all_surfaces(&world, 200, dir.path(), 1, &mut revisions).unwrap(),
            0,
            "a world where nothing happened writes nothing at all"
        );

        world.apply(
            Some("gleba"),
            &crate::event::Event::AddEntity { name: "inserter".to_string(), x: 51.0, y: 2.0, d: 0, w: 1, h: 1, id: Some(1) },
        );
        assert_eq!(
            write_all_surfaces(&world, 300, dir.path(), 2, &mut revisions).unwrap(),
            1,
            "only the surface that actually changed"
        );

        let mut files: Vec<String> =
            fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        files.sort();
        assert_eq!(
            files,
            ["frame_0000_gleba.stfr", "frame_0000_nauvis.stfr", "frame_0002_gleba.stfr"],
            "nauvis stops at index 0 and gleba skips index 1"
        );
    }

    /// A re-add of exactly what is there must not count as a change: the
    /// baseline smear produces these by design, so treating one as a change
    /// would write a duplicate file for every surface every time.
    #[test]
    fn re_adding_an_identical_entity_does_not_force_a_write() {
        let mut world = crate::world::World::new();
        world.load_baseline(&crate::frame::Frame {
            tick: 100,
            surface: "nauvis".to_string(),
            entities: vec![crate::frame::Entity { n: "pipe".into(), x: 1.0, y: 2.0, d: 0, w: 1, h: 1 }],
            count: 1,
            tiles: Vec::new(),
        });

        let dir = tempfile::tempdir().unwrap();
        let mut revisions = std::collections::HashMap::new();
        write_all_surfaces(&world, 100, dir.path(), 0, &mut revisions).unwrap();

        world.apply(
            Some("nauvis"),
            &crate::event::Event::AddEntity { name: "pipe".to_string(), x: 1.0, y: 2.0, d: 0, w: 1, h: 1, id: None },
        );

        assert_eq!(
            write_all_surfaces(&world, 200, dir.path(), 1, &mut revisions).unwrap(),
            0,
            "re-adding what was already there changed nothing"
        );
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

        let written: Vec<String> =
            fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
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
