//! Reassembling a timeline from a baseline snapshot plus the event log.
//!
//! The capture directory the mod writes looks like this:
//!
//! ```text
//! <session>/baseline.json                 tick + surfaces the original baseline covers
//! <session>/frame_<tick>_<surface>.stfr   one per surface, that baseline itself
//! <session>/events_<start_tick>.stev      append-only, one segment per timeline
//! <session>/players.jsonl                 optional, sampled player positions
//! ```
//!
//! A surface added to tracking after the original baseline already ran gets
//! its own later `frame_<tick>_<surface>.stfr`, at its own tick, with no
//! entry in `baseline.json` (which never changes after it's first written).
//! `discover_catch_up_baselines` finds these by scanning rather than reading
//! them out of the manifest; see [`CatchUpBaseline`].
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

/// A surface added to tracking after the playthrough's original baseline
/// already ran (see `mod/capture.lua`'s `M.on_surface_included`): an
/// ordinary `frame_<tick>_<surface>.stfr` file, just written later and for
/// a surface `baseline.json` doesn't name, with no manifest entry of its
/// own. `baseline.json` never changes after it's first written, so this is
/// how a session accumulates more than one baseline tick over its
/// lifetime: found by scanning the session folder rather than reading it
/// out of JSON.
#[derive(Debug)]
struct CatchUpBaseline {
    tick: u64,
    surface: String,
    path: PathBuf,
}

impl CatchUpBaseline {
    /// Parses a session folder entry the same way `viewer::loading::frame_is_candidate`
    /// recognizes a frame file (duplicated, not shared: `viewer` depends on
    /// this crate, not the other way around). Extension first, then a
    /// `frame_` prefix, then split once on the first remaining underscore so
    /// a surface name that itself contains one (a modded planet might)
    /// survives intact in the second half.
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

    /// Actually reads and parses this catch-up's frame file. Deferred by
    /// design until `run`'s tick-ordered walk actually reaches its tick,
    /// since a session can accumulate several of these, each potentially as
    /// large as any other baseline surface.
    fn load(&self) -> io::Result<frame::Frame> {
        let bytes = std::fs::read(&self.path)?;
        frame::read_binary(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Every catch-up baseline in `dir`, ascending by tick: every
/// `frame_<tick>_<surface>.stfr` file that isn't one of `baseline`'s own
/// (the original set `load_baseline` already loads eagerly). Filtering by
/// exact expected path, not by re-deriving and comparing a (tick, surface)
/// pair, sidesteps needing the original set to ever agree with a filename
/// parse at all: `baseline.frame_path` already builds each original path
/// deterministically, so anything else matching the general frame shape is
/// definitionally a catch-up.
fn discover_catch_up_baselines(dir: &Path, baseline: &Baseline) -> io::Result<Vec<CatchUpBaseline>> {
    let known: std::collections::HashSet<PathBuf> =
        baseline.surfaces.iter().map(|s| baseline.frame_path(dir, s)).collect();

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
    /// Events dropped for belonging to a timeline the player reloaded away
    /// from (see `event::Segment`'s `end_tick`). Unlike every other counter
    /// here this is not a health signal: any nonzero value just means the
    /// playthrough was reloaded from an earlier save at some point, and
    /// dropping these is what makes the replay match what actually happened.
    pub superseded_events: usize,
    /// Segments holding more than one append run (see
    /// `event::segment_run_bounds`): a capture recorded before the mod
    /// started rolling over on a same-save-twice reload, so one file holds
    /// two attempts at the same stretch of play. Split and bounded rather
    /// than replayed on top of each other. Kept as its own counter because
    /// this shape is also, in principle, what a segment corrupted by
    /// deleting `script-output` by hand would look like, and the two are
    /// indistinguishable from the file alone.
    pub restarted_segments: usize,
    /// Catch-up baselines (see [`CatchUpBaseline`]) not yet reached by `run`'s
    /// tick-ordered walk, ascending by tick. Emptied out as `run` applies
    /// each one in turn; never re-populated after `load_baseline`.
    pending_catch_ups: Vec<CatchUpBaseline>,
    /// How many catch-up baselines `run` actually applied. Orthogonal to
    /// `applied_events`/`no_op_events`/`out_of_order_batches`: loading a
    /// baseline is not an event, exactly like the original baseline's own
    /// load in `load_baseline` never touches those counters either.
    pub catch_ups_applied: usize,
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
        pending_catch_ups,
        catch_ups_applied: 0,
    })
}

/// Every surface the baseline, a pending catch-up baseline, or the event log
/// ever names, for offering a complete choice of surfaces before running the
/// (much more expensive) replay itself. The baseline alone is not enough: a
/// Space Age planet visited for the first time after capture already started
/// gets no baseline entry (the baseline is taken once, at the start of a
/// playthrough's capture), but its own construction events still name it, so
/// scanning the event log too is what makes it choosable at all instead of
/// only ever appearing already lumped into "every surface". A surface with a
/// catch-up baseline but no events of its own yet (included, then nothing
/// built there before capture stopped) would otherwise be invisible to both
/// checks, so `replay.pending_catch_ups` is checked directly too.
pub fn discover_surfaces(session_dir: &Path, replay: &Replay) -> io::Result<Vec<String>> {
    let mut surfaces: std::collections::BTreeSet<String> =
        replay.world.surface_names().into_iter().map(String::from).collect();
    surfaces.extend(replay.pending_catch_ups.iter().map(|c| c.surface.clone()));

    // Bounded the same way `run` bounds them, so a surface that exists only
    // in a timeline the player reloaded away from is not offered as a choice
    // that would then replay to an empty timelapse.
    for segment in event::log_segments(session_dir)? {
        if let Ok(stream) = event::stream_log(&segment.path) {
            surfaces.extend(stream.filter(|l| l.tick < segment.end_tick).map(|logged| logged.surface));
        }
    }

    Ok(surfaces.into_iter().collect())
}

/// Walk the event log forward, calling `emit` with the world at each frame
/// boundary. Events are applied in whole-tick groups, so a frame is never cut
/// halfway through a tick's changes: a blueprint landing 400 entities on one
/// tick shows up whole or not at all. Each frame shows the world as of its own
/// tick, including across a long gap with no events in it (see the
/// `flush_until` call below), and a segment superseded by a reload to an
/// earlier save is bounded rather than replayed (see [`event::Segment`]).
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

    let mut flush_until = |replay: &mut Replay, tick: u64, next: &mut u64, emitted: &mut usize| {
        while tick >= *next && *emitted < options.max_frames {
            apply_due_catch_ups(replay, *next);
            emit(&replay.world, *next);
            *emitted += 1;
            *next += options.interval;
        }
    };

    for segment in event::log_segments(dir)? {
        // A session can legitimately span more than one segment (a save
        // reload rolls over to a fresh one; see `is_capture_rollback`), and
        // the mod has no way to notice or clean up an orphaned segment left
        // behind by deleting capture files without running
        // `/timelapse-reset-capture` first (its next flush recreates one via
        // a plain append, with no magic header, under the same session tag).
        // One bad segment losing its own events is a much smaller problem
        // than it sinking replay of an otherwise complete session.
        let stream = match event::stream_log(&segment.path) {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("warning: skipping unreadable event segment {}: {e}", segment.path.display());
                replay.skipped_segments += 1;
                continue;
            }
        };

        // One bound per append run (see `event::segment_run_bounds`). A
        // capture written by the current mod always has exactly one run per
        // segment; more than one means this file was recorded before the
        // same-save-twice rollover fix and holds two attempts at the same
        // stretch of play. A scan that fails leaves the segment on its own
        // single bound, which is what it had before runs were considered at
        // all, rather than dropping the segment.
        let run_bounds = event::segment_run_bounds(&segment.path, segment.end_tick).unwrap_or_default();
        if run_bounds.len() > 1 {
            replay.restarted_segments += 1;
        }
        let mut run = 0usize;
        let mut previous_tick: Option<u64> = None;

        let mut pending: Vec<LoggedEvent> = Vec::new();
        let mut pending_tick = None;

        for logged in stream {
            // Which append run this record belongs to, tracked over every
            // record the file holds rather than only the kept ones: a run
            // boundary is a fact about append order, so skipping a record
            // must not change where the next boundary is seen.
            if previous_tick.is_some_and(|p| logged.tick < p) {
                run += 1;
            }
            previous_tick = Some(logged.tick);

            // Events at or past this run's bound describe a timeline the
            // player reloaded away from (see `event::Segment` for the reload
            // that starts a new file, `event::segment_run_bounds` for the one
            // that used to continue the same file). Skipped one by one rather
            // than breaking out of the stream, since what follows a
            // superseded stretch is the replacement for it, not the end of
            // the useful data.
            let bound = run_bounds.get(run).copied().unwrap_or(segment.end_tick);
            if logged.tick >= bound {
                replay.superseded_events += 1;
                continue;
            }
            // Events before the baseline describe a world we did not capture.
            if logged.tick < replay.baseline.tick {
                continue;
            }
            // Same idea, per surface: the mod starts logging a surface's
            // events the instant it's included, but its catch-up snapshot
            // itself isn't captured until BASELINE_WARNING_DELAY_TICKS later
            // (mod/capture.lua), so a handful of real events can be logged
            // for a tick earlier than the surface's own baseline. Whatever
            // they did is already reflected in that later snapshot, so they
            // must not also apply, or it would double count.
            if pending_catch_up_tick(&replay.pending_catch_ups, &logged.surface).is_some_and(|t| logged.tick < t) {
                continue;
            }

            // A surface can go long stretches with no events at all before a
            // catch-up (that's the whole point: it was excluded, so nothing
            // was being logged for it). Left to the ordinary per-tick
            // batching below, a catch-up would only ever get applied (via
            // flush_until's own internal apply_due_catch_ups) once some
            // *later* event's batch happens to trigger a flush, by which
            // point that later event has already been applied to the world
            // too, making every frame across that whole gap retroactively
            // show it, including ones chronologically before the catch-up's
            // own tick. Explicitly flushing up through each due catch-up's
            // tick first, before this event ever gets folded into `pending`,
            // is what keeps frames before it clean.
            while replay.pending_catch_ups.first().is_some_and(|c| c.tick <= logged.tick) {
                let catch_up_tick = replay.pending_catch_ups[0].tick;
                apply_batch(replay, &mut pending);
                pending_tick = None;
                flush_until(replay, catch_up_tick, &mut next_emit, &mut emitted);
                // flush_until's own internal apply_due_catch_ups call only
                // fires when an interval checkpoint lands at or past this
                // tick, and a coarse interval can overshoot it in a single
                // jump, never landing on or after it within *this* call.
                // Applying it directly guarantees it actually happens
                // (a no-op if flush_until's own call already did), which is
                // also what keeps this loop from spinning forever on a
                // catch-up flush_until never gets around to.
                apply_due_catch_ups(replay, catch_up_tick);
            }

            if pending_tick != Some(logged.tick) {
                apply_batch(replay, &mut pending);
                // Flush every boundary strictly before this new tick, not
                // just up to the batch just applied. Those two only look
                // equivalent when events are dense: across a gap (a long
                // stretch of research or walking with nothing built) the
                // boundaries inside it would otherwise stay unflushed until
                // whatever event ends the gap had already been folded into
                // the world, and each one would then render with that
                // event's changes already visible, showing something built
                // at the end of the gap as though it had been there from the
                // start of it. Bounding the flush by the incoming tick
                // instead pins every frame in the gap to the world as it
                // stood while the gap was happening.
                flush_until(replay, logged.tick.saturating_sub(1), &mut next_emit, &mut emitted);
                pending_tick = Some(logged.tick);
            }
            pending.push(logged);
        }

        apply_batch(replay, &mut pending);
        if let Some(tick) = pending_tick {
            flush_until(replay, tick, &mut next_emit, &mut emitted);
        }
    }

    // Every catch-up still outstanding at this point has no later event to
    // trigger it, but a completed catch-up file is genuinely part of "the
    // base as it actually was" by the time capture stopped, so it still
    // belongs in the final frame below.
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

/// The tick `surface`'s own catch-up baseline is scheduled for, if it still
/// has one outstanding. See `run`'s use of this: whether `surface`'s entry
/// has already been applied to `world` by the time a given event is checked
/// does not matter here, this only answers "is there a tick this surface's
/// events must not predate."
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
    use std::time::SystemTime;

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

    /// Stamps a segment's mtime, which is how `event::log_segments` recovers
    /// the order segments were created in: higher `rank` means created later.
    /// Set it explicitly wherever a test depends on that order, since writing
    /// two files back to back does not reliably give them distinguishable
    /// mtimes. See `event::tests::set_mtime_rank`, which this mirrors.
    fn set_mtime_rank(dir: &Path, name: &str, rank: u64) {
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + rank);
        std::fs::OpenOptions::new().write(true).open(dir.join(name)).unwrap().set_modified(when).unwrap();
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

    /// The bug this guards against: a planet visited for the first time after
    /// capture already started has no baseline entry (the baseline is taken
    /// once, at the start of a playthrough's capture), so the surface list
    /// shown before running the replay must also check the event log, not
    /// just the baseline the world was seeded from.
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

        // Written to the directory newest first above, but played in the
        // other order: the tick-200 segment is the one capture started with,
        // and the tick-9000 one is where play resumed after a reload later
        // on. Stamped rather than inferred, since which segment came first is
        // exactly what ordering depends on and a bare write order would be
        // asserting the opposite of the truth here.
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

    /// Ticks going backwards inside one segment file, which is what a
    /// capture recorded before the mod's same-save-twice rollover fix looks
    /// like: reloading the same save resumed at exactly the tick the live
    /// segment started at, the old rollover check read that as "no
    /// rollback", and the second attempt was appended onto the first
    /// attempt's records in the same file.
    ///
    /// Read as a run boundary rather than as damage: the tick-500 pipe
    /// belongs to the attempt that was reloaded away from, so it is
    /// superseded, and only the tick-200 belt from the second attempt
    /// survives. This used to be counted as an out-of-order batch and
    /// applied anyway, which left both attempts' entities in the world at
    /// once.
    ///
    /// The same shape could in principle come from a segment corrupted by
    /// deleting `script-output` by hand, which is indistinguishable from the
    /// file alone (though that normally produces a header-less file that
    /// fails to open outright, counted as `skipped_segments` instead). That
    /// is what `restarted_segments` stays visible for.
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

    /// The whole point of splitting a segment into runs: the second attempt
    /// replaces the first rather than piling on top of it. Both attempts
    /// build at the same tick and the same spot, so a replay that applied
    /// both would still end with one entity there and look fine; the
    /// distinct position built only in the abandoned attempt is what makes
    /// the difference observable.
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

        let names: std::collections::BTreeSet<String> = replay
            .world
            .surface("nauvis")
            .unwrap()
            .entities()
            .map(|e| replay.world.names().name(e.name).to_string())
            .collect();
        assert_eq!(
            names,
            ["pipe", "transport-belt", "chemical-plant"].iter().map(|s| s.to_string()).collect(),
            "the abandoned attempt's furnace must not survive"
        );
        assert_eq!(replay.restarted_segments, 1);
        let _dir = dir;
    }

    /// The scenario `is_capture_rollback` (mod/encode.lua) exists for: the
    /// player reloads an older save mid playthrough. `ensure_capture_segment`
    /// reacts by starting a fresh segment at the tick play resumed from, but
    /// the OLD segment is simply abandoned on disk (control.lua has no API to
    /// delete or truncate it), so it still contains events for ticks beyond
    /// the reload point describing a future that, from the player's
    /// perspective, never actually happened.
    ///
    /// `events_100.stev` here plays out as if the player never reloaded:
    /// something real at tick 200, then an assembling machine at tick 4000
    /// that only exists in that abandoned future. `events_3000.stev` is what
    /// the player actually did after reloading their tick-3000 save: build a
    /// second belt at tick 3500. A correct replay must never show the
    /// assembling machine, at any tick, since the reload erased it before it
    /// was ever built for real.
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

        // The real timeline: the baseline's pipe alone until the belt lands
        // at tick 200, both of them until the post-reload belt lands at 3500,
        // all three after that. The assembling machine the reload erased has
        // no tick at which it may appear.
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

        let abandoned_survived =
            replay.world.surface("nauvis").unwrap().entities().any(|e| e.x == 9.5 && e.y == 9.5);
        assert!(!abandoned_survived, "the abandoned future's entity must not survive the reload");
        // baseline's pipe + the tick-200 belt (before the reload point) + the
        // real tick-3500 belt (after it) = 3; never 4, which would mean the
        // erased assembling machine is still there.
        assert_eq!(replay.world.entity_count(), 3);
        let _dir = dir;
    }

    /// Two reloads, the second reaching further back than the first, which is
    /// the case start-tick ordering cannot express: the segment created last
    /// (`events_500`) has the middle start tick of the three, so ordering by
    /// tick would replay it before `events_1000` and let the first reload's
    /// abandoned events land on top of the second reload's real ones.
    ///
    /// Played from tick 0 and built a belt at 300. Reloaded to tick 1000,
    /// built an assembling machine at 1500. Reloaded again, further back, to
    /// tick 500, and built a chemical plant at 800. Only the belt (before
    /// every reload point) and the chemical plant (after the last one) are
    /// real; the assembling machine belongs to a timeline erased by the
    /// second reload, and the original segment's own tick-2000 furnace was
    /// erased by the first.
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

        let surviving: std::collections::BTreeSet<String> = replay
            .world
            .surface("nauvis")
            .unwrap()
            .entities()
            .map(|e| replay.world.names().name(e.name).to_string())
            .collect();
        assert_eq!(
            surviving,
            ["pipe", "transport-belt", "chemical-plant"].iter().map(|s| s.to_string()).collect(),
            "only the baseline's pipe, the pre-reload belt, and the post-reload chemical plant are real"
        );
        // The tick-2000 furnace and the tick-1500 assembling machine.
        assert_eq!(replay.superseded_events, 2);
        let _dir = dir;
    }

    /// Nothing to do with reloads: a plain forward capture where the player
    /// built something, idled a long time (researching, running around), then
    /// built again. Every frame boundary in that gap must show the world as
    /// it stood at that boundary, not the world as it stood once the gap
    /// ended.
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

    /// Writes a hand-built catch-up baseline frame directly into a session
    /// folder, the same shape `M.export_surfaces_to` (mod/export.lua)
    /// produces: an ordinary frame_<tick>_<surface>.stfr with no manifest
    /// entry of its own.
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
        write_catch_up_frame(
            session_dir,
            500,
            "vulcanus",
            vec![vulcanus_entity(1.5, 1.5), vulcanus_entity(2.5, 1.5)],
        );

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
        assert_eq!(
            seen.last().unwrap().1,
            3,
            "the snapshot's 2 plus the tick-600 event's 1"
        );
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
