//! Turning a loaded recording into a timelapse on disk.
//!
//! This is the long half of the work and the only part with nothing to ask.
//! It is split from whatever asked, because the two want different things: a
//! console prints a line and blocks until it is done, and a window has to keep
//! drawing, so the work has to be callable from a thread that is not the one
//! painting.
//!
//! What that costs is one callback and one flag, both of which a console front
//! end can ignore. What it buys is that neither front end owns the writing
//! rules: which places get named files, when a frame is a picture and when it
//! is a delta, and what order ground and frames are written in.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::export::{self, ExportConfig, MOD_NAME};
use crate::frame;
use crate::milestone;
use crate::replay::{self, Options, Replay};

/// How a caller watches a job and stops one.
///
/// `on` is called as each unit of work lands rather than on a schedule: a
/// small job can finish before any timer would fire, and a caller that wants
/// less than everything can filter for itself.
///
/// `cancel` is checked at the same points, so stopping takes effect within one
/// unit rather than at the end. Nothing already written is removed: a
/// cancelled build leaves a shorter timelapse, which is a real thing somebody
/// may have wanted, rather than nothing at all.
///
/// Generic in what gets reported, because the jobs genuinely differ: building
/// counts frames, and exporting saves has a name and an outcome per save.
pub struct Watch<'a, P> {
    pub on: &'a mut dyn FnMut(P),
    pub cancel: &'a AtomicBool,
}

impl<P> Watch<'_, P> {
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// What to build, decided before any of it starts.
pub struct Plan {
    /// Which places to include. Empty means every one the recording has.
    ///
    /// Exactly one is the case that changes the output rather than filtering
    /// it: the frames are named for nothing, which is what a single-surface
    /// timelapse has always been on disk and what the viewer reads as "one
    /// world, unnamed".
    pub surfaces: Vec<String>,
    pub options: Options,
}

/// What a finished build did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Built {
    /// Frames the replay reached, which is what the timeline covers.
    pub emitted: usize,
    /// Frames actually written. Lower than `emitted` only if a build stopped.
    pub written: usize,
    pub cancelled: bool,
}

/// Ground, sidecars and frames into `out`, in that order.
///
/// Ground first because it is fixed the instant the baseline loads, and
/// because a later ground scan is meant to overwrite this: the scan covers the
/// factory's final extent, and this only finds anything at all for a recording
/// made before ground moved out of the baseline.
pub fn timelapse(
    replay: &mut Replay,
    session_dir: &Path,
    out: &Path,
    plan: &Plan,
    watch: &mut Watch<'_, usize>,
) -> io::Result<Built> {
    // Before anything is written, so the nests somebody cleared stand in the
    // first frame like everything else that was there at the start.
    let _ = restore_cleared_nests(replay, session_dir);

    let tick = replay.baseline.tick;
    match plan.surfaces.as_slice() {
        [one] => replay::write_terrain(&replay.world, one, tick, out)?,
        many => replay::write_terrain_of(&replay.world, tick, out, many)?,
    }
    copy_sidecars(session_dir, out)?;

    let mut written = 0usize;
    let mut failed: Option<io::Error> = None;
    // Last revision written per surface, carried across the run so a surface
    // nothing has touched can be skipped.
    let mut revisions: std::collections::HashMap<String, u64> = Default::default();

    let single = match plan.surfaces.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    };

    let emitted = replay::run(replay, session_dir, &plan.options, |world, tick| {
        if failed.is_some() || watch.stopped() {
            return;
        }
        let result = match &single {
            // The first frame is the picture; every one after it is what
            // changed. A real megabase changed by about 200 items a frame out
            // of 4.2 million, so a snapshot spent 8.8 MB restating what the
            // reader already had.
            Some(name) => {
                let frame = match written == 0 {
                    true => {
                        world.clear_changes(name);
                        world.to_frame(name, tick)
                    }
                    false => world.to_frame_delta(name, tick),
                };
                std::fs::write(out.join(format!("frame_{written:04}.stfr")), frame::write_binary(&frame.as_out())).map(|()| 1)
            }
            None => replay::write_surfaces(world, tick, out, written, &mut revisions, &plan.surfaces),
        };
        match result {
            Ok(_) => {
                written += 1;
                (watch.on)(written);
            }
            Err(e) => failed = Some(e),
        }
    })?;

    if let Some(e) = failed {
        return Err(e);
    }
    Ok(Built { emitted, written, cancelled: watch.stopped() })
}

/// The plain-JSON sidecar logs a live capture writes, copied beside the frames.
///
/// A straight copy rather than a re-parse, the mod's logs and what the viewer
/// reads being the same shape by design. Each being absent is normal: a
/// recording made before a given log existed simply has none.
pub fn copy_sidecars(session_dir: &Path, out: &Path) -> io::Result<Vec<&'static str>> {
    let mut copied = Vec::new();
    for name in ["players.jsonl", "milestones.jsonl", "prototypes.json"] {
        let source = session_dir.join(name);
        if source.exists() {
            std::fs::copy(&source, out.join(name))?;
            copied.push(name);
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture_with_events;

    /// The arguments a request becomes, as strings, for asserting on.
    fn args_of(request: &VideoRequest) -> Vec<String> {
        video_args(request).iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn a_request() -> VideoRequest {
        VideoRequest {
            timelapse: PathBuf::from("in"),
            target: PathBuf::from("out"),
            width: 1920,
            height: 1080,
            surface: None,
            video: true,
            fps: 30,
            mp4: false,
            overlay_players: false,
            overlay_clock: false,
        }
    }

    /// Every one of these is a decision somebody made in a menu, and a flag
    /// silently not passed is a render that comes out wrong after minutes
    /// rather than an error.
    #[test]
    fn every_choice_reaches_the_renderer() {
        let full = VideoRequest {
            surface: Some("gleba".to_string()),
            mp4: true,
            overlay_players: true,
            overlay_clock: true,
            fps: 60,
            ..a_request()
        };
        assert_eq!(
            args_of(&full),
            [
                "in",
                "--export",
                "out",
                "--width",
                "1920",
                "--height",
                "1080",
                "--video",
                "--fps",
                "60",
                "--mp4",
                "--overlay-players",
                "--overlay-clock",
                "--surface",
                "gleba",
            ]
        );
    }

    /// Frame rate and overlays only mean anything to a video. An image
    /// sequence is frames on disk for somebody else's editor to time and
    /// label, and passing a frame rate with them would be a claim about
    /// timing this cannot make.
    #[test]
    fn an_image_sequence_carries_no_frame_rate_or_overlays() {
        let frames = VideoRequest { video: false, mp4: true, overlay_clock: true, ..a_request() };
        let args = args_of(&frames);
        for unwanted in ["--video", "--fps", "--mp4", "--overlay-clock", "--overlay-players"] {
            assert!(!args.contains(&unwanted.to_string()), "{unwanted} reached an image sequence: {args:?}");
        }
    }

    /// No surface is the busiest one, which the renderer picks for itself, so
    /// the flag has to be absent rather than passed empty.
    #[test]
    fn no_chosen_place_passes_no_surface_at_all() {
        assert!(!args_of(&a_request()).contains(&"--surface".to_string()));
    }

    /// A build of the fixture capture into a fresh folder, plus the names it
    /// wrote, sorted so a test can assert on them.
    fn built(surfaces: &[&str], cancel_after: Option<usize>) -> (Built, Vec<String>, Vec<usize>) {
        let (_capture, session_dir) = capture_with_events(40);
        let out = tempfile::tempdir().unwrap();
        let mut replay = replay::load_baseline(&session_dir.join("baseline.json")).unwrap();

        let plan = Plan {
            surfaces: surfaces.iter().map(|s| s.to_string()).collect(),
            options: Options { interval: 10, max_frames: 1000 },
        };

        let cancel = AtomicBool::new(false);
        let mut seen: Vec<usize> = Vec::new();
        let mut on_frame = |written: usize| {
            seen.push(written);
            if cancel_after.is_some_and(|at| written >= at) {
                cancel.store(true, Ordering::Relaxed);
            }
        };
        let result =
            timelapse(&mut replay, &session_dir, out.path(), &plan, &mut Watch { on: &mut on_frame, cancel: &cancel }).unwrap();

        let mut names: Vec<String> =
            std::fs::read_dir(out.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        (result, names, seen)
    }

    /// One place writes frames named for nothing, which is what the viewer
    /// reads as "one world, unnamed" and what this has always produced.
    #[test]
    fn a_single_place_writes_frames_named_for_nothing() {
        let (result, names, _) = built(&["nauvis"], None);
        assert!(result.written > 1, "the fixture builds something every tick: {result:?}");
        assert!(names.contains(&"frame_0000.stfr".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("_nauvis.stfr")), "{names:?}");
    }

    /// Everything, which is what an empty list means, names each file for its
    /// surface so more than one world can share a folder.
    #[test]
    fn every_place_writes_frames_named_for_their_surface() {
        let (_, names, _) = built(&[], None);
        assert!(names.contains(&"frame_0000_nauvis.stfr".to_string()), "{names:?}");
    }

    /// The count is the running total and arrives once per frame, so a caller
    /// can print every twenty-fifth or drive a bar without keeping its own.
    #[test]
    fn progress_counts_up_once_per_frame() {
        let (result, _, seen) = built(&["nauvis"], None);
        assert_eq!(seen.len(), result.written);
        assert_eq!(seen.first(), Some(&1));
        assert_eq!(seen.last(), Some(&result.written));
        assert!(seen.windows(2).all(|w| w[1] == w[0] + 1), "the total must not skip: {seen:?}");
    }

    /// The whole reason for the flag: a window has to be able to stop a build
    /// that is going to take minutes, and stopping has to take effect within a
    /// frame rather than at the end.
    #[test]
    fn cancelling_stops_within_a_frame_and_says_so() {
        let (stopped, _, seen) = built(&["nauvis"], Some(3));
        assert!(stopped.cancelled);
        assert_eq!(stopped.written, 3, "one more frame must not slip through: {seen:?}");

        let (whole, _, _) = built(&["nauvis"], None);
        assert!(!whole.cancelled);
        assert!(whole.written > stopped.written, "{whole:?} against {stopped:?}");
    }

    /// What a cancelled build leaves is a shorter timelapse, not nothing: the
    /// frames already written are correct and somebody may well have wanted
    /// exactly that.
    #[test]
    fn a_cancelled_build_keeps_what_it_already_wrote() {
        let (result, names, _) = built(&["nauvis"], Some(2));
        assert_eq!(result.written, 2);
        assert!(names.contains(&"frame_0000.stfr".to_string()), "{names:?}");
        assert!(names.contains(&"frame_0001.stfr".to_string()), "{names:?}");
        assert!(!names.contains(&"frame_0002.stfr".to_string()), "{names:?}");
    }
}

/// One save, as an export reports it.
///
/// Named rather than counted, because the interesting failure is one save out
/// of forty refusing while the rest work, and a bare count cannot say which.
pub enum SaveStep {
    Started { index: usize, total: usize, label: String },
    Exported { index: usize, label: String, bytes: u64, seconds: f64 },
    Failed { index: usize, label: String, error: String },
}

/// What exporting a set of saves produced.
#[derive(Debug, Default)]
pub struct Exported {
    /// The frame each successful save became, in the order they were given.
    pub frames: Vec<PathBuf>,
    /// What each save said about milestones, for `milestone::from_saves`,
    /// which needs consecutive saves to say when something first became true.
    pub milestones: Vec<milestone::State>,
    pub cancelled: bool,
}

/// Runs Factorio once per save, writing one frame each into `out`.
///
/// The slowest thing this tool does by a wide margin: every save is a full
/// game load, which on a modded megabase is tens of seconds before any of the
/// export happens. That is the whole reason this reports per save and takes a
/// cancel flag.
///
/// Cancellation is checked between saves, never during one. A running Factorio
/// is a child process minutes into loading a save, and killing it partway
/// would leave a staging tree nobody asked for; stopping before the next one
/// starts is both achievable and what somebody means by "stop".
///
/// A save that fails is reported and skipped rather than ending the run. One
/// unreadable save out of forty should cost that save.
pub fn from_saves(
    saves: &[PathBuf],
    out: &Path,
    workspace: &Path,
    config: &ExportConfig,
    watch: &mut Watch<'_, SaveStep>,
) -> io::Result<Exported> {
    let mut result = Exported::default();

    for (index, save) in saves.iter().enumerate() {
        if watch.stopped() {
            result.cancelled = true;
            break;
        }
        let label = save.file_name().unwrap_or_default().to_string_lossy().into_owned();
        (watch.on)(SaveStep::Started { index, total: saves.len(), label: label.clone() });

        let staged = workspace.join(format!("stage_{index}"));
        match export::export_save(save, &staged, config) {
            Ok(outcome) => {
                let target = out.join(format!("frame_{index:04}.stfr"));
                let primary = &outcome.frames[0];
                std::fs::rename(primary, &target).or_else(|_| std::fs::copy(primary, &target).map(drop))?;

                // Appended, not overwritten: each save contributes its own
                // one-shot sample at its own real tick.
                if let Some(log) = &outcome.players_log {
                    let mut combined = std::fs::OpenOptions::new().create(true).append(true).open(out.join("players.jsonl"))?;
                    std::io::Write::write_all(&mut combined, &std::fs::read(log)?)?;
                }
                if let Some(state) = outcome.milestones {
                    result.milestones.push(state);
                }
                let bytes = target.metadata().map(|m| m.len()).unwrap_or(0);
                result.frames.push(target);
                (watch.on)(SaveStep::Exported { index, label, bytes, seconds: outcome.seconds });
            }
            Err(error) => (watch.on)(SaveStep::Failed { index, label, error: error.to_string() }),
        }
        let _ = std::fs::remove_dir_all(&staged);
    }

    Ok(result)
}

/// Marks a re-execution of this program as the viewer rather than the menu.
///
/// Deliberately a flag rather than a bare path argument: a path alone would
/// have to be told apart from every option a headless build wants, and
/// guessing wrong means opening a window at somebody who asked for a file.
pub const VIEW_FLAG: &str = "--view";

/// This program again, told to be the viewer.
///
/// The viewer used to be a second executable. It is now a module of this one,
/// but it still runs in its own process rather than in place, because
/// macroquad allows a single window per process: opening one from the menu
/// would mean the menu never comes back, and opening one from a window that
/// already exists is not possible at all.
pub fn viewer_command() -> io::Result<std::process::Command> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg(VIEW_FLAG);
    Ok(command)
}

/// A video or image sequence to render, decided before any of it starts.
pub struct VideoRequest {
    /// The built timelapse to render.
    pub timelapse: PathBuf,
    /// Where it goes, without an extension: the renderer appends `.avi` or
    /// `.mp4` for a video and treats the path as a folder for an image
    /// sequence, so the same argument serves both.
    pub target: PathBuf,
    pub width: u32,
    pub height: u32,
    /// One place, `"all"` for one file each, or `None` for the busiest.
    pub surface: Option<String>,
    /// A video file rather than a numbered image per frame.
    pub video: bool,
    pub fps: u32,
    pub mp4: bool,
    pub overlay_players: bool,
    pub overlay_clock: bool,
}

/// The arguments `request` becomes, after the `--view` flag.
///
/// Split from running it so the translation can be tested: every one of these
/// is a decision somebody made in a menu, and a flag silently not being passed
/// is a render that comes out wrong after several minutes rather than an error.
fn video_args(request: &VideoRequest) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        request.timelapse.clone().into(),
        "--export".into(),
        request.target.clone().into(),
        "--width".into(),
        request.width.to_string().into(),
        "--height".into(),
        request.height.to_string().into(),
    ];
    // Frame rate and overlays only mean anything to a video. An image sequence
    // is frames on disk for somebody else's editor to time and label.
    if request.video {
        args.push("--video".into());
        args.push("--fps".into());
        args.push(request.fps.to_string().into());
        if request.mp4 {
            args.push("--mp4".into());
        }
        if request.overlay_players {
            args.push("--overlay-players".into());
        }
        if request.overlay_clock {
            args.push("--overlay-clock".into());
        }
    }
    if let Some(name) = &request.surface {
        args.push("--surface".into());
        args.push(name.clone().into());
    }
    args
}

/// Renders `request`, blocking until the render finishes.
///
/// Blocking on purpose, and not reporting either: the renderer opens its own
/// window and shows the frames as it writes them, so progress is already in
/// front of whoever asked. What a caller with a window of its own needs is to
/// run this off the thread doing the painting, which is what makes it a job
/// rather than something the menu does inline.
pub fn video(request: &VideoRequest) -> io::Result<()> {
    let status = viewer_command()?.args(video_args(request)).status()?;
    if status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!("the renderer exited with {status} without finishing the export")))
}

/// Where a mode writes its output: beside the running exe, so the result is
/// easy to find wherever Factorio's user data lives. Falls back to the current
/// directory only if the exe's path cannot be determined.
pub fn output_dir_next_to_exe(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from(".")).join(name)
}

/// Where built timelapses are kept, one subfolder each. A single fixed folder
/// that every run rebuilt meant reopening yesterday's timelapse was
/// impossible: the only way to see one was to make it again.
pub fn timelapses_root() -> PathBuf {
    output_dir_next_to_exe("timelapses")
}

/// Turns a playthrough name or save name into something safe to use as a
/// folder name, since both come from the user and can hold anything.
pub fn as_folder_name(raw: &str) -> String {
    // Dots are kept, a name like "v1.2 run" being ordinary, and trimmed from
    // the ends, since "." and ".." name directories.
    let cleaned: String =
        raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') { c } else { '_' }).collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "timelapse".to_string()
    } else {
        trimmed.to_string()
    }
}

/// One timelapse already built and sitting on disk.
pub struct BuiltTimelapse {
    pub name: String,
    pub path: PathBuf,
    pub frames: usize,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// Every built timelapse, newest first. Counts frames and weighs the folder,
/// since which of two somebody wants is usually answered by how big and how
/// recent it is, neither visible from a name.
pub fn list_timelapses() -> Vec<BuiltTimelapse> {
    list_timelapses_in(&timelapses_root())
}

/// Split from [`list_timelapses`] only so it can be tested: the real root is
/// derived from the running executable's own location, which no test can
/// arrange.
pub fn list_timelapses_in(root: &Path) -> Vec<BuiltTimelapse> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };

    let mut found: Vec<BuiltTimelapse> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let path = entry.path();
            let files: Vec<_> = std::fs::read_dir(&path).ok()?.filter_map(Result::ok).collect();
            let frames = files.iter().filter(|f| f.path().extension().and_then(|e| e.to_str()) == Some("stfr")).count();
            // A folder with no frames in it is a half-finished or interrupted
            // build, not something worth offering to open.
            if frames == 0 {
                return None;
            }
            let bytes = files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some(BuiltTimelapse { name: entry.file_name().to_string_lossy().into_owned(), path, frames, bytes, modified })
        })
        .collect();

    found.sort_by_key(|t| std::cmp::Reverse(t.modified));
    found
}

/// Where finished videos and image sequences go: one folder next to the
/// executable, like `timelapses/`, so "where did it go" has one answer.
pub fn videos_root() -> PathBuf {
    output_dir_next_to_exe("videos")
}

/// One rendered video or image sequence in `videos/`. The viewer writes a file
/// for a video and a folder of numbered frames for a sequence, so both shapes
/// are listed the same way and weighed the same way.
pub struct BuiltVideo {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// Bytes under `path`, whether it is one file or a folder of frames. Best
/// effort: anything unreadable counts as nothing rather than failing a listing
/// somebody is only trying to read.
pub fn size_on_disk(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else { return 0 };
    if meta.is_file() {
        return meta.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    entries.filter_map(Result::ok).map(|entry| size_on_disk(&entry.path())).sum()
}

pub fn list_videos() -> Vec<BuiltVideo> {
    list_videos_in(&videos_root())
}

/// Split from [`list_videos`] for the same reason as [`list_timelapses_in`]:
/// the real root is derived from the running executable's own location.
pub fn list_videos_in(root: &Path) -> Vec<BuiltVideo> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut found: Vec<BuiltVideo> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some(BuiltVideo {
                name: entry.file_name().to_string_lossy().into_owned(),
                bytes: size_on_disk(&path),
                path,
                modified,
            })
        })
        .collect();
    found.sort_by_key(|v| std::cmp::Reverse(v.modified));
    found
}

/// Deletes `path`, file or folder, so a video and an image sequence are the
/// same operation to the caller.
pub fn delete_path(path: &Path) -> io::Result<()> {
    match std::fs::metadata(path)?.is_dir() {
        true => std::fs::remove_dir_all(path),
        false => std::fs::remove_file(path),
    }
}

/// frame after it along with it.
pub fn write_as_delta_chain(frames: &[PathBuf]) -> io::Result<(u64, u64)> {
    let size = |path: &PathBuf| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let before: u64 = frames.iter().map(size).sum();

    let mut ordered: Vec<(u64, &PathBuf)> = Vec::new();
    for path in frames {
        match frame::read_header(path) {
            Ok((tick, _)) => ordered.push((tick, path)),
            // Unreadable here means unreadable to the viewer as well, so it is
            // removed rather than left to reset the sequence at load.
            Err(_) => drop(std::fs::remove_file(path)),
        }
    }
    ordered.sort_by_key(|&(tick, _)| tick);

    let mut chain: Vec<&PathBuf> = Vec::new();
    let mut last_tick: Option<u64> = None;
    for (tick, path) in ordered {
        match last_tick == Some(tick) {
            true => drop(std::fs::remove_file(path)),
            false => {
                chain.push(path);
                last_tick = Some(tick);
            }
        }
    }

    let mut previous: Option<frame::Frame> = None;
    for path in &chain {
        let current = frame::read_binary(&std::fs::read(path)?)?;
        // Read before it is written, so rewriting in place is safe: the folder
        // shrinks as it goes rather than needing room for both forms at once.
        if let Some(prev) = &previous {
            std::fs::write(path, frame::write_binary(&crate::world::delta_between(prev, &current).as_out()))?;
        }
        previous = Some(current);
    }

    Ok((before, chain.iter().copied().map(size).sum()))
}
/// of target/release), then the current folder.
pub fn mod_source_dir() -> io::Result<PathBuf> {
    let exe_sibling = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("mod")));
    if let Some(dir) = &exe_sibling {
        if dir.is_dir() {
            return Ok(dir.clone());
        }
    }
    let manifest_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mod");
    if manifest_candidate.is_dir() {
        return Ok(manifest_candidate);
    }
    let cwd_candidate = PathBuf::from("mod");
    if cwd_candidate.is_dir() {
        return Ok(cwd_candidate);
    }
    Err(io::Error::other(
        "Could not find the mod/ folder needed to export from saves. It should sit next to \
         this program (or in the current folder, if running from source).",
    ))
}

/// Ground already read for this playthrough, newest first.
///
/// Kept beside the capture rather than only in the timelapse, which is deleted
/// and rebuilt every time. Natural ground does not change, so reading it again
/// means launching Factorio to be told the same thing, which on a megabase is
/// most of what a rebuild costs.
///
/// Ground cached before the scan collected scenery would be reused forever,
/// leaving out the trees and ore this exists to supply, so a set where no file
/// holds an entity counts as nothing cached. A capture with no scenery
/// anywhere rescans, which is the safe way to be wrong.
pub fn cached_ground(user_dir: &Path, session_id: u32) -> Vec<PathBuf> {
    let dir = user_dir.join("script-output").join(MOD_NAME).join(format!("{session_id:08x}"));
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("terrain_") && n.ends_with(".stfr")))
        .collect();
    found.sort();

    match found.iter().any(|p| crate::frame::read_has_entities(p).unwrap_or(false)) {
        true => found,
        false => Vec::new(),
    }
}

/// Copies cached ground beside the frames. Returns how many files landed.
pub fn reuse_ground(cached: &[PathBuf], out: &Path) -> usize {
    cached.iter().filter(|file| file.file_name().is_some_and(|name| std::fs::copy(file, out.join(name)).is_ok())).count()
}

/// Reads one save's ground and puts it beside the frames, keeping a copy for
/// next time.
///
/// `expect_session` is the playthrough the frames belong to. A save from a
/// different game lands under a different session id and is refused rather
/// than laying an unrelated landscape under somebody's factory; `None` skips
/// that check, which is right for the from-saves path where the saves *are*
/// the playthrough.
pub fn scan_ground(save: &Path, out: &Path, config: &ExportConfig, expect_session: Option<u32>) -> Result<usize, String> {
    let staged = std::env::temp_dir().join(format!("{MOD_NAME}-ground-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staged);

    let scanned = crate::export::scan_terrain(save, &staged, config);
    let copied = match scanned {
        Err(e) => Err(format!("Could not read the ground: {e}")),
        Ok(scan) => match expect_session {
            Some(want) if want != scan.session_id => {
                Err("That save is from a different playthrough, so its ground would not match.".to_string())
            }
            _ => {
                let keep = crate::locate::locate_factorio()
                    .map(|dir| dir.join("script-output").join(MOD_NAME).join(format!("{:08x}", scan.session_id)))
                    .filter(|dir| dir.is_dir());
                let mut copied = 0usize;
                for file in &scan.files {
                    let Some(name) = file.file_name() else { continue };
                    if std::fs::copy(file, out.join(name)).is_ok() {
                        copied += 1;
                    }
                    if let Some(dir) = &keep {
                        let _ = std::fs::copy(file, dir.join(name));
                    }
                }
                Ok(copied)
            }
        },
    };

    let _ = std::fs::remove_dir_all(&staged);
    copied
}

/// The surfaces a built timelapse holds, read off its filenames. A
/// single-surface build writes `frame_0000.stfr` with no name in it, so an
/// empty result means "one surface, unnamed" rather than "none". Split at the
/// first underscore after the index, so a name containing one survives.
pub fn surfaces_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("stfr"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            // Terrain files are named per surface too, so counting both would
            // list a name twice. Terrain alone is not a surface anyone can
            // export.
            let rest = stem.strip_prefix("frame_")?;
            let (_index, surface) = rest.split_once('_')?;
            (!surface.is_empty()).then(|| surface.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The type a nest is, which is what the scan cannot date and the log can.
const NEST: &str = "unit-spawner";

/// Puts back every nest the recording cleared, so clearing one can be watched.
///
/// The ground scan reads a finished save, so a nest somebody cleared at hour
/// five is not in it and never appears at all: the clearing, which is most of
/// what fighting biters looks like in a timelapse, is invisible. The log knows
/// it happened, because a removal names a nest, and a removal naming something
/// the recording never added is proof it was standing there before any of this
/// began.
///
/// So each one goes into the world at the baseline, and the removal that named
/// it takes it away again at the tick it fired. Nothing else changes: the
/// replay does the rest by itself.
///
/// Only nests. A tree or a rock cleared has the same shape of problem and no
/// name in its removal to work from, and giving every removal a name would put
/// one on the millions a factory generates.
pub fn restore_cleared_nests(replay: &mut Replay, session_dir: &Path) -> io::Result<usize> {
    let described = crate::prototypes::read(session_dir);
    let is_nest = |name: &str| match &described {
        Some(p) => p.kind(name) == Some(NEST),
        None => name.contains("spawner"),
    };

    // Everything the log ever added, so a nest built by expansion and then
    // cleared is not put back at the start as well: the event already places
    // that one at the tick it was built.
    let mut added: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    let mut cleared: Vec<(String, f32, f32)> = Vec::new();

    for segment in crate::event::log_segments(session_dir)? {
        let Ok(stream) = crate::event::stream_log(&segment.path) else { continue };
        for logged in stream {
            match &logged.event {
                crate::event::Event::AddEntity { x, y, .. } => {
                    added.insert(at(*x, *y));
                }
                crate::event::Event::RemoveEntity { pos, name: Some(name), .. } if is_nest(name) => {
                    cleared.push((name.clone(), pos.0, pos.1));
                }
                _ => {}
            }
        }
    }

    let mut put_back = 0usize;
    for (name, x, y) in cleared {
        if added.contains(&at(x, y)) {
            continue;
        }
        let (w, h) = described.as_ref().map_or((1, 1), |p| p.size_of(&name));
        // No id: a nest rebuilt from a removal has no history, exactly like one
        // that came from a baseline, and position is what resolves both.
        replay.world.apply(None, &crate::event::Event::AddEntity { name, x, y, d: 0, w, h, id: None });
        put_back += 1;
    }
    Ok(put_back)
}

/// One position, at the tenth of a tile the format stores, so a nest read from
/// a save and the same nest named by an event compare equal.
fn at(x: f32, y: f32) -> (i32, i32) {
    ((x * 10.0).round() as i32, (y * 10.0).round() as i32)
}

/// Drops from `out`'s ground layer every nest the recording says turned up
/// after it started.
///
/// The scan reads one finished save and cannot say when anything in it
/// appeared, so a nest the biters built at hour ten was laid down from the
/// first frame. The recording knows: `on_biter_base_built` names it at the
/// tick it was built. Taking those out of the static layer leaves the event to
/// put each one down when it actually arrived, and leaves every nest that was
/// always there exactly where it was.
///
/// Scanning the earliest save instead does not work and was measured: Factorio
/// generates the map as somebody explores, so an early save has almost no
/// nests in it and nothing ever adds the rest, revealing a chunk raising no
/// event this records.
///
/// Best effort by design. A log that cannot be read, or a ground file that
/// cannot be rewritten, leaves the ground as the scan produced it, which is
/// the behaviour this replaces rather than a failure.
pub fn drop_nests_that_arrived_later(out: &Path, session_dir: &Path) -> io::Result<usize> {
    let described = crate::prototypes::read(session_dir);
    let is_nest = |name: &str| match &described {
        Some(p) => p.kind(name) == Some(NEST),
        // A recording that never described its prototypes, which is every one
        // made before the mod started saying. The name is all there is.
        None => name.contains("spawner"),
    };

    let mut arrived: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    for segment in crate::event::log_segments(session_dir)? {
        let Ok(stream) = crate::event::stream_log(&segment.path) else { continue };
        for logged in stream {
            if let crate::event::Event::AddEntity { name, x, y, .. } = &logged.event {
                if is_nest(name) {
                    arrived.insert(at(*x, *y));
                }
            }
        }
    }
    if arrived.is_empty() {
        return Ok(0);
    }

    let mut dropped = 0usize;
    for entry in std::fs::read_dir(out)?.filter_map(Result::ok) {
        let path = entry.path();
        let is_ground =
            path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("terrain_") && n.ends_with(".stfr"));
        if !is_ground {
            continue;
        }
        let Ok(mut ground) = std::fs::read(&path).and_then(|bytes| frame::read_binary(&bytes)) else { continue };

        let before = ground.entities.len();
        ground.entities.retain(|e| !(is_nest(&e.n) && arrived.contains(&at(e.x, e.y))));
        if ground.entities.len() == before {
            continue;
        }
        ground.count = ground.entities.len();
        if std::fs::write(&path, frame::write_binary(&ground.as_out())).is_ok() {
            dropped += before - ground.entities.len();
        }
    }
    Ok(dropped)
}

#[cfg(test)]
mod ground_tests {
    use super::*;
    use crate::test_support::{capture_building, capture_with_events};

    /// A ground file holding two nests, one of which the log will claim.
    fn ground_with_nests(out: &Path, positions: &[(f32, f32)]) {
        let entities: Vec<frame::Entity> =
            positions.iter().map(|&(x, y)| frame::Entity { n: "biter-spawner".into(), x, y, d: 0, w: 3, h: 3 }).collect();
        let tiles = vec![frame::Tile { n: "grass-1".into(), x: 0, y: 0 }];
        let out_frame = frame::FrameOut { tick: 0, surface: "nauvis", entities: &entities, tiles: &tiles, ..Default::default() };
        std::fs::write(out.join("terrain_nauvis.stfr"), frame::write_binary(&out_frame)).unwrap();
    }

    fn nests_left(out: &Path) -> Vec<(f32, f32)> {
        let bytes = std::fs::read(out.join("terrain_nauvis.stfr")).unwrap();
        frame::read_binary(&bytes).unwrap().entities.iter().map(|e| (e.x, e.y)).collect()
    }

    /// The whole point: a nest the recording says was built stops being part
    /// of the landscape, so the event that built it decides when it appears.
    /// One that was always there is untouched.
    #[test]
    fn a_nest_the_log_built_is_dropped_and_the_rest_stay() {
        // The fixture writes tenths of a tile, so its one build at tick 1
        // lands at 1.0, 1.0.
        let (_capture, session_dir) = capture_building(1, "biter-spawner");
        let out = tempfile::tempdir().unwrap();
        ground_with_nests(out.path(), &[(1.0, 1.0), (500.0, 500.0)]);

        assert_eq!(drop_nests_that_arrived_later(out.path(), &session_dir).unwrap(), 1);
        assert_eq!(
            nests_left(out.path()),
            [(500.0, 500.0)],
            "the one the biters built goes, so its event decides when it appears; the one that was              always there stays"
        );
    }

    /// Only nests. Everything else in the ground layer is trees, ore and
    /// cliffs, none of which a playthrough builds, and a factory's own adds
    /// run to millions.
    #[test]
    fn something_else_the_log_built_takes_no_nest_with_it() {
        let (_capture, session_dir) = capture_building(1, "pipe");
        let out = tempfile::tempdir().unwrap();
        ground_with_nests(out.path(), &[(1.0, 1.0), (500.0, 500.0)]);

        assert_eq!(drop_nests_that_arrived_later(out.path(), &session_dir).unwrap(), 0);
        assert_eq!(nests_left(out.path()).len(), 2, "a pipe built where a nest stands is not that nest arriving");
    }

    /// A recording with nothing built leaves the ground exactly as scanned,
    /// which is what every from-saves build and every quiet playthrough gets.
    #[test]
    fn a_recording_that_built_no_nests_changes_nothing() {
        let (_capture, session_dir) = capture_with_events(0);
        let out = tempfile::tempdir().unwrap();
        ground_with_nests(out.path(), &[(10.0, 10.0), (500.0, 500.0)]);

        assert_eq!(drop_nests_that_arrived_later(out.path(), &session_dir).unwrap(), 0);
        assert_eq!(nests_left(out.path()).len(), 2);
    }

    /// A capture whose log clears a nest that nothing ever built. That is what
    /// a player finding a nest and destroying it looks like from the outside.
    fn capture_clearing_a_nest(at_tick: u64) -> (tempfile::TempDir, PathBuf) {
        use crate::wire::ByteWriter;
        let (dir, session_dir) = capture_building(0, "biter-spawner");

        let mut w = ByteWriter::new();
        w.magic(b"STE1").u8(1);
        w.u8(0).string("biter-spawner");
        w.u8(1).string("nauvis");
        w.u8(2).u64(at_tick);
        // A named removal: tag 128 says the next removal is for that name.
        w.u8(128).varint(1).varint(0);
        w.u8(4).i32(3000).i32(4000).u64(0).u16(0);
        std::fs::write(session_dir.join("events_0.stev"), w.into_vec()).unwrap();

        (dir, session_dir)
    }

    /// The whole point: a nest cleared during a playthrough is in no save the
    /// scan can read, so without this it never appears and the clearing cannot
    /// be watched.
    #[test]
    fn a_nest_the_recording_cleared_is_put_back_at_the_start() {
        let (_capture, session_dir) = capture_clearing_a_nest(50);
        let mut replay = replay::load_baseline(&session_dir.join("baseline.json")).unwrap();

        assert_eq!(restore_cleared_nests(&mut replay, &session_dir).unwrap(), 1);
        let frame = replay.world.to_frame("nauvis", 0);
        assert!(
            frame.entities.iter().any(|e| &*e.n == "biter-spawner" && e.x == 300.0 && e.y == 400.0),
            "it has to stand in the first frame: {:?}",
            frame.entities
        );
    }

    /// And it goes away again when the log says it did, which is the half that
    /// makes putting it back worth anything.
    #[test]
    fn and_the_removal_takes_it_away_again_at_its_own_tick() {
        let (_capture, session_dir) = capture_clearing_a_nest(50);
        let out = tempfile::tempdir().unwrap();
        let mut replay = replay::load_baseline(&session_dir.join("baseline.json")).unwrap();

        let plan = Plan { surfaces: vec!["nauvis".to_string()], options: Options { interval: 10, max_frames: 100 } };
        let mut ignored = |_: usize| {};
        let cancel = AtomicBool::new(false);
        timelapse(&mut replay, &session_dir, out.path(), &plan, &mut Watch { on: &mut ignored, cancel: &cancel }).unwrap();

        assert!(
            !replay.world.to_frame("nauvis", 100).entities.iter().any(|e| &*e.n == "biter-spawner"),
            "the nest must be gone once the clearing has replayed"
        );
    }

    /// A nest the biters built and somebody then cleared is already placed by
    /// its own event, so putting it back at the start would show it before the
    /// biters made it.
    #[test]
    fn a_nest_that_was_built_first_is_not_put_back_as_well() {
        let (_capture, session_dir) = capture_building(1, "biter-spawner");
        let mut replay = replay::load_baseline(&session_dir.join("baseline.json")).unwrap();
        assert_eq!(restore_cleared_nests(&mut replay, &session_dir).unwrap(), 0, "nothing was cleared here");
    }

    /// Positions are matched at the tenth of a tile the format stores, so a
    /// nest read from a save and the same nest named by an event compare
    /// equal rather than missing each other by rounding.
    #[test]
    fn positions_match_at_the_precision_the_format_keeps() {
        assert_eq!(at(10.0, 10.0), (100, 100));
        assert_eq!(at(10.05, -3.5), (101, -35), "a tenth apart is a different place");
        assert_eq!(at(10.04, 10.0), at(10.04, 10.0));
    }

    /// A session folder with no log at all is a from-saves build, and leaves
    /// the ground alone rather than failing the ground step.
    #[test]
    fn no_recording_leaves_the_ground_alone() {
        let empty = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        ground_with_nests(out.path(), &[(10.0, 10.0)]);

        assert_eq!(drop_nests_that_arrived_later(out.path(), empty.path()).unwrap(), 0);
        assert_eq!(nests_left(out.path()).len(), 1);
    }
}
