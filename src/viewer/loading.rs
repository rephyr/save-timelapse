//! Discovering, reading, and grouping raw `save_timelapse::frame::Frame`
//! data from disk (or synthesizing it for load testing). Everything here
//! stays at that level, never touching `TypeRegistry`/`RenderFrame`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use save_timelapse::frame::{Entity, Frame, Tile};

/// Grid of fabricated entities, cycling through a handful of type names, for
/// load-testing at counts the real fixtures don't reach.
pub fn synthetic_frame(count: usize) -> Frame {
    const NAMES: &[&str] = &["transport-belt", "assembling-machine-1", "electric-pole", "inserter", "pipe", "splitter"];
    let side = (count as f32).sqrt().ceil() as i64;
    let spacing = 2.0;
    let entities = (0..count)
        .map(|i| {
            let ix = (i as i64) % side;
            let iy = (i as i64) / side;
            Entity { n: NAMES[i % NAMES.len()].into(), x: ix as f32 * spacing, y: iy as f32 * spacing, d: 0, w: 1, h: 1 }
        })
        .collect();
    Frame {
        tick: 0,
        surface: "synthetic".to_string(),
        count,
        entities,
        tiles: Vec::new(),
        floor_unchanged: false,
        ..Default::default()
    }
}

/// Filled grid of concrete tiles, for load-testing the case a fully-paved
/// megabase produces: far more tile cells than entities.
pub fn synthetic_tiles(count: usize) -> Vec<Tile> {
    let side = (count as f32).sqrt().ceil() as i64;
    (0..count)
        .map(|i| {
            let ix = (i as i64) % side;
            let iy = (i as i64) / side;
            Tile { n: "concrete".into(), x: ix as i32, y: iy as i32 }
        })
        .collect()
}

/// A directory of `frame_*.stfr` (sorted by filename, matching the CLI's own
/// zero-padded `frame_NNNN.stfr` output, so plain lexicographic sort is
/// enough) or a single frame file.
fn frame_is_candidate(path: &Path) -> bool {
    // Extension first. Live capture writes a `.stfr.done` marker beside each
    // finished snapshot, whose stem passes every check below, so without this
    // every marker reads as a frame that fails to parse.
    if path.extension().and_then(|e| e.to_str()) != Some("stfr") {
        return false;
    }

    let stem = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return false,
    };
    if !stem.starts_with("frame_") {
        return false;
    }

    let rest = &stem[6..];
    let mut parts = rest.splitn(2, '_');
    let tick_part = parts.next().unwrap_or("");
    let surface_part = parts.next().unwrap_or("");

    if surface_part == "manifest" {
        return false;
    }

    tick_part.parse::<u64>().is_ok()
}

/// The frame files a path refers to, in order. Enumerating separately from
/// parsing is what lets the caller show a bar with a real total instead of an
/// indeterminate spinner.
pub fn frame_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(path)?.filter_map(Result::ok).map(|e| e.path()).filter(|p| frame_is_candidate(p)).collect();
    entries.sort();
    Ok(entries)
}

/// Parse one frame file, or warn and yield `None`. A snapshot being written
/// incrementally by the mod is a half-file until it is finished, so an
/// unparseable frame is an expected transient rather than an error.
pub fn load_frame(path: &Path) -> Option<Frame> {
    if !path.exists() {
        return None;
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("warning: skipping unreadable frame {}: {}", path.display(), e);
            return None;
        }
    };
    match save_timelapse::frame::read_binary(&bytes) {
        Ok(frame) => Some(frame),
        Err(e) => {
            eprintln!("warning: skipping invalid frame {}: {}", path.display(), e);
            None
        }
    }
}

/// Where `surface`'s one-time terrain snapshot lives, beside its frame files.
/// Never matched by `frame_is_candidate`, even for a surface named "frame" or
/// "terrain", so it can share a directory without being picked up.
pub fn terrain_path(dir: &Path, surface: &str) -> PathBuf {
    dir.join(format!("terrain_{surface}.stfr"))
}

/// `surface`'s terrain layer, if this capture has one. Terrain capture being
/// off, or an older capture predating this file, are the same "nothing to
/// show" case `load_frame`'s missing-path check already handles, so this is
/// a thin wrapper rather than a second error path.
pub fn load_terrain(dir: &Path, surface: &str) -> Option<Frame> {
    load_frame(&terrain_path(dir, surface))
}

/// Every `terrain_<surface>.stfr` directly in `dir`, for loading a capture's
/// terrain in one batch. Deliberately independent of which surfaces the frames
/// turn out to have, which is what lets terrain loading start immediately
/// rather than waiting to learn the surface list from the frame files.
pub fn terrain_paths(dir: &Path) -> io::Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("stfr")
                && p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with("terrain_"))
        })
        .collect();
    entries.sort();
    Ok(entries)
}

/// Loads many frame files across every core rather than one at a time.
///
/// Reading and parsing is pure independent work with nothing shared until
/// frames become `RenderFrame`s, which need one `TypeRegistry` handing out
/// consistent ids. On a real megabase capture parsing was the dominant cost of
/// opening the viewer, roughly halved on an 8 core machine.
///
/// Runs on a fresh thread rather than blocking, so a caller driving macroquad's
/// render loop can keep drawing a progress bar. `poll` collects the result.
pub struct ParallelFrameLoad {
    progress: Arc<AtomicUsize>,
    total: usize,
    result: mpsc::Receiver<Vec<Frame>>,
}

impl ParallelFrameLoad {
    pub fn start(paths: Vec<PathBuf>) -> Self {
        let total = paths.len();
        let progress = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();

        let worker_progress = Arc::clone(&progress);
        std::thread::spawn(move || {
            let _ = tx.send(load_all(&paths, &worker_progress));
        });

        ParallelFrameLoad { progress, total, result: rx }
    }

    pub fn done(&self) -> usize {
        self.progress.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// `None` until loading finishes; the result is only ever sent once.
    pub fn poll(&self) -> Option<Vec<Frame>> {
        self.result.try_recv().ok()
    }
}

/// One worker per available core, each claiming the next unclaimed path from
/// a shared cursor. Simple work stealing rather than a fixed split, so an
/// early baseline sized frame next to much smaller ones doesn't leave the
/// worker that drew it running long after the others are idle.
fn load_all(paths: &[PathBuf], progress: &AtomicUsize) -> Vec<Frame> {
    let workers = std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1);
    let next_index = AtomicUsize::new(0);

    let mut found: Vec<(usize, Frame)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers.min(paths.len().max(1)))
            .map(|_| {
                scope.spawn(|| {
                    let mut loaded = Vec::new();
                    loop {
                        let i = next_index.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = paths.get(i) else { break };
                        if let Some(frame) = load_frame(path) {
                            loaded.push((i, frame));
                        }
                        progress.fetch_add(1, Ordering::Relaxed);
                    }
                    loaded
                })
            })
            .collect();

        handles.into_iter().flat_map(|h| h.join().expect("frame loading thread panicked")).collect()
    });

    // Workers finish in claim order, not path order, so restore the order
    // `frame_paths` produced before handing frames back to the caller.
    found.sort_by_key(|(i, _)| *i);
    found.into_iter().map(|(_, frame)| frame).collect()
}

/// Order a loaded sequence by in-game tick, keeping one surface per tick.
///
/// Filenames cannot be trusted: the CLI writes zero-padded names where
/// lexicographic sort works, but the mod writes a raw unpadded tick, so
/// sorting names puts 1200 and 12600 before 600. The parsed tick is the one
/// thing both carry.
///
/// When several surfaces share a tick the busiest wins, showing them in
/// sequence would make the camera jump between planets.
pub fn order_by_tick<T>(frames: &mut Vec<T>, tick: impl Fn(&T) -> u64, count: impl Fn(&T) -> usize) {
    frames.sort_by(|a, b| tick(a).cmp(&tick(b)).then(count(b).cmp(&count(a))));

    let mut seen = None;
    frames.retain(|frame| {
        let keep = seen != Some(tick(frame));
        seen = Some(tick(frame));
        keep
    });
}

/// Splits a loaded batch into one timeline per surface, each ordered and
/// deduplicated by tick. Several surfaces sharing a tick, which is the mod's
/// raw baseline output, is exactly the case that needs separating rather than
/// collapsing.
///
/// A `Frame`'s surface comes from its parsed content rather than its filename.
/// Surfaces come back busiest first, so a caller that only shows the first
/// gets the surface loading used to always pick.
/// Groups frame paths by surface and orders each group by tick, reading only
/// each file's header.
///
/// The path-based twin of [`group_by_surface`], and what makes a streaming
/// load possible: a loader has to know surface and order before it can fold
/// frames in one at a time, and grouping parsed frames means every frame is
/// resident at exactly the moment that is being avoided. Headers are a bounded
/// read per file.
///
/// Busiest first, matched to `group_by_surface` so the default world is the
/// same either way. Busiest is by file size here, since counting entities
/// would mean parsing, and the two agree closely.
///
/// A file whose header will not read is dropped with a warning, matching how
/// the full parse treats one.
pub fn group_paths_by_surface(paths: Vec<PathBuf>) -> Vec<(String, Vec<(u64, PathBuf)>)> {
    /// One frame file as this needs it before parsing: when it is, how big
    /// it is (standing in for how busy), and where it lives.
    type Candidate = (u64, u64, PathBuf);

    let mut by_surface: HashMap<String, Vec<Candidate>> = HashMap::new();
    for path in paths {
        match save_timelapse::frame::read_header(&path) {
            Ok((tick, surface)) => {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                by_surface.entry(surface).or_default().push((tick, size, path));
            }
            Err(e) => eprintln!("warning: skipping unreadable frame {}: {e}", path.display()),
        }
    }

    let mut surfaces: Vec<(String, Vec<Candidate>)> = by_surface.into_iter().collect();
    for (_, group) in &mut surfaces {
        // Same rule as `order_by_tick`: ascending tick, and where two files
        // claim one tick for a surface, the larger wins as the busier.
        group.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        group.dedup_by_key(|(tick, _, _)| *tick);
    }
    surfaces.sort_by_key(|(_, group)| std::cmp::Reverse(group.iter().map(|(_, size, _)| *size).max().unwrap_or(0)));

    // The tick is kept because the loader needs it to put back the frames an
    // export omitted: a surface is not written at a moment nothing on it
    // changed, so the gaps have to be filled against the union of every
    // surface's ticks.
    surfaces.into_iter().map(|(name, group)| (name, group.into_iter().map(|(tick, _, path)| (tick, path)).collect())).collect()
}

/// Parses `paths` in parallel and returns the frames in the same order,
/// skipping any that fail. Unlike [`ParallelFrameLoad`], which loads a whole
/// capture at once, this is meant to be handed one bounded batch at a time so
/// peak memory stays at the batch rather than the capture.
pub fn load_batch(paths: &[PathBuf]) -> Vec<Frame> {
    let ignored = AtomicUsize::new(0);
    load_all(paths, &ignored)
}

pub fn group_by_surface(frames: Vec<Frame>) -> Vec<(String, Vec<Frame>)> {
    let mut by_surface: HashMap<String, Vec<Frame>> = HashMap::new();
    for frame in frames {
        by_surface.entry(frame.surface.clone()).or_default().push(frame);
    }

    let mut surfaces: Vec<(String, Vec<Frame>)> = by_surface.into_iter().collect();
    for (_, group) in &mut surfaces {
        order_by_tick(group, |f| f.tick, |f| f.entities.len());
    }
    surfaces.sort_by_key(|(_, group)| std::cmp::Reverse(group.iter().map(|f| f.entities.len()).max().unwrap_or(0)));
    surfaces
}

/// Every moment an export covers, ascending and deduplicated: the union of the
/// ticks each surface has a frame at.
///
/// It cannot be taken from any one surface, each being written only when
/// something on it changed and so holding an arbitrary subset. The union is
/// what each surface's gaps are filled against.
pub fn timeline_ticks(surfaces: &[(String, Vec<(u64, PathBuf)>)]) -> Vec<u64> {
    let mut ticks: Vec<u64> = surfaces.iter().flat_map(|(_, group)| group.iter().map(|&(tick, _)| tick)).collect();
    ticks.sort_unstable();
    ticks.dedup();
    ticks
}

pub fn load_sequence(path: &Path) -> io::Result<Vec<Frame>> {
    let mut frames: Vec<Frame> = frame_paths(path)?.iter().filter_map(|p| load_frame(p)).collect();

    if frames.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("no valid frame files found in {}", path.display())));
    }

    order_by_tick(&mut frames, |f| f.tick, |f| f.entities.len());
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a `.stfr` frame with `entity_count` fabricated entities, for
    /// tests that only care about tick/surface ordering, not real content.
    fn write_stub_frame(dir: &Path, name: &str, tick: u64, surface: &str, entity_count: usize) {
        let entities = synthetic_frame(entity_count).entities;
        let out = save_timelapse::frame::FrameOut {
            tick,
            surface,
            entities: &entities,
            tiles: &[],
            floor_unchanged: false,
            ..Default::default()
        };
        std::fs::write(dir.join(name), save_timelapse::frame::write_binary(&out)).unwrap();
    }

    /// Polls `load` to completion without a real render loop to yield
    /// through, standing in for what `main.rs`'s `redraw_progress` loop
    /// does between polls.
    fn block_on(load: &ParallelFrameLoad) -> Vec<Frame> {
        loop {
            if let Some(frames) = load.poll() {
                return frames;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn load_sequence_skips_invalid_frames() {
        let dir = tempfile::tempdir().unwrap();
        let valid = dir.path().join("frame_0000.stfr");
        let invalid = dir.path().join("frame_0001.stfr");
        let bytes = save_timelapse::frame::write_binary(&save_timelapse::frame::FrameOut {
            tick: 0,
            surface: "nauvis",
            entities: &[],
            tiles: &[],
            floor_unchanged: false,
            ..Default::default()
        });
        std::fs::write(&valid, bytes).unwrap();
        std::fs::write(&invalid, b"not a frame").unwrap();

        let frames = load_sequence(dir.path()).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].tick, 0);
    }

    #[test]
    fn synthetic_frame_produces_the_requested_count_on_a_grid() {
        let frame = synthetic_frame(9);
        assert_eq!(frame.entities.len(), 9);
        assert_eq!(frame.count, 9);
        assert_eq!((frame.entities[0].x, frame.entities[0].y), (0.0, 0.0));
        assert_eq!((frame.entities[1].x, frame.entities[1].y), (2.0, 0.0));
        assert_eq!((frame.entities[3].x, frame.entities[3].y), (0.0, 2.0));
    }

    #[test]
    fn synthetic_tiles_produces_the_requested_count_on_a_grid() {
        let tiles = synthetic_tiles(9);
        assert_eq!(tiles.len(), 9);
        assert!(tiles.iter().all(|t| &*t.n == "concrete"));
        assert_eq!((tiles[0].x, tiles[0].y), (0, 0));
        assert_eq!((tiles[3].x, tiles[3].y), (0, 1));
    }

    #[test]
    fn parallel_load_returns_frames_in_the_same_order_paths_were_given() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (name, tick) in [("a.stfr", 5u64), ("b.stfr", 1), ("c.stfr", 9)] {
            write_stub_frame(dir.path(), name, tick, "nauvis", 0);
            paths.push(dir.path().join(name));
        }

        let load = ParallelFrameLoad::start(paths.clone());
        let frames = block_on(&load);

        assert_eq!(frames.len(), paths.len());
        assert_eq!(frames.iter().map(|f| f.tick).collect::<Vec<_>>(), vec![5, 1, 9]);
        assert_eq!(load.total(), paths.len());
        assert_eq!(load.done(), paths.len());
    }

    #[test]
    fn parallel_load_skips_an_unreadable_path_rather_than_failing_the_whole_load() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_frame(dir.path(), "good.stfr", 1, "nauvis", 0);
        let missing = dir.path().join("missing.stfr");

        let load = ParallelFrameLoad::start(vec![missing, dir.path().join("good.stfr")]);
        let frames = block_on(&load);

        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].tick, 1);
    }

    #[test]
    fn parallel_load_of_an_empty_path_list_completes_with_no_frames() {
        let load = ParallelFrameLoad::start(Vec::new());
        assert!(block_on(&load).is_empty());
    }

    /// Real megabase captures are what motivated parallelising this at all,
    /// so the fixture with the most entities stands in for "one big file
    /// among the paths" rather than only ever testing uniform, tiny ones.
    #[test]
    fn parallel_load_matches_sequential_loading_on_the_real_fixtures() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frames");
        let paths = frame_paths(Path::new(dir)).unwrap();

        let sequential: Vec<u64> = paths.iter().filter_map(|p| load_frame(p)).map(|f| f.tick).collect();
        let load = ParallelFrameLoad::start(paths);
        let parallel: Vec<u64> = block_on(&load).iter().map(|f| f.tick).collect();

        assert_eq!(parallel, sequential);
    }

    #[test]
    fn load_sequence_sorts_a_directory_regardless_of_iteration_order() {
        let dir = tempfile::tempdir().unwrap();
        for (name, tick) in [("frame_0002.stfr", 2u64), ("frame_0000.stfr", 0), ("frame_0001.stfr", 1)] {
            write_stub_frame(dir.path(), name, tick, "nauvis", 0);
        }
        let frames = load_sequence(dir.path()).unwrap();
        assert_eq!(frames.iter().map(|f| f.tick).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn grouping_paths_by_header_matches_grouping_parsed_frames() {
        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frames"));
        let paths = frame_paths(dir).unwrap();

        let by_header = group_paths_by_surface(paths.clone());
        let by_parse = group_by_surface(load_batch(&paths));

        assert_eq!(by_header.len(), by_parse.len(), "same number of surfaces");
        for ((header_name, header_paths), (parse_name, parse_frames)) in by_header.iter().zip(&by_parse) {
            assert_eq!(header_name, parse_name, "surfaces must come back in the same order");
            assert_eq!(header_paths.len(), parse_frames.len(), "{header_name}: same frame count");
            // The orders have to agree file for file, since this decides the
            // sequence the spans are folded in.
            let header_ticks: Vec<u64> = header_paths.iter().map(|&(tick, _)| tick).collect();
            let parse_ticks: Vec<u64> = parse_frames.iter().map(|f| f.tick).collect();
            assert_eq!(header_ticks, parse_ticks, "{header_name}: tick order");
        }
    }

    #[test]
    fn grouping_paths_skips_a_file_it_cannot_read_a_header_from() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_frame(dir.path(), "frame_100_nauvis.stfr", 100, "nauvis", 1);
        std::fs::write(dir.path().join("frame_200_nauvis.stfr"), b"garbage").unwrap();

        let grouped = group_paths_by_surface(frame_paths(dir.path()).unwrap());
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].1.len(), 1, "the unreadable file must be dropped, not fail the load");
    }

    #[test]
    fn load_sequence_loads_the_real_fixtures_in_order() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frames");
        let frames = load_sequence(Path::new(dir)).unwrap();
        assert_eq!(frames.len(), 5);
        let ticks: Vec<u64> = frames.iter().map(|f| f.tick).collect();
        assert!(ticks.windows(2).all(|w| w[0] < w[1]), "expected strictly increasing ticks, got {ticks:?}");
    }

    /// The shape a snapshot timer produces: tick-named files whose
    /// lexicographic order is wrong, plus manifests that are not frames.
    #[test]
    fn load_sequence_orders_mod_written_snapshots_by_tick_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        for tick in [600u64, 1200, 12600, 216000] {
            write_stub_frame(dir.path(), &format!("frame_{tick}_nauvis.stfr"), tick, "nauvis", 0);
            // Written beside every snapshot; must not be read as a frame.
            std::fs::write(
                dir.path().join(format!("frame_{tick}_manifest.json")),
                format!(r#"{{"tick":{tick},"entities":0,"tiles":0,"surfaces":["nauvis"]}}"#),
            )
            .unwrap();
        }

        let frames = load_sequence(dir.path()).expect("manifests must not break loading");
        assert_eq!(
            frames.iter().map(|f| f.tick).collect::<Vec<_>>(),
            vec![600, 1200, 12600, 216000],
            "sorted by filename this would be 1200, 12600, 216000, 600"
        );
    }

    #[test]
    fn load_sequence_keeps_only_the_busiest_surface_per_tick() {
        let dir = tempfile::tempdir().unwrap();
        for (surface, count) in [("nauvis", 900usize), ("vulcanus", 12)] {
            write_stub_frame(dir.path(), &format!("frame_600_{surface}.stfr"), 600, surface, count);
        }

        let frames = load_sequence(dir.path()).unwrap();
        assert_eq!(frames.len(), 1, "one frame per tick, or the camera jumps between planets");
        assert_eq!(frames[0].surface, "nauvis");
    }

    /// The multi-surface counterpart of the busiest-per-tick test above: the
    /// same six-surfaces-one-tick shape, but nothing may be discarded.
    #[test]
    fn group_by_surface_keeps_every_surface_sharing_a_tick() {
        let dir = tempfile::tempdir().unwrap();
        for (surface, count) in [("nauvis", 900usize), ("vulcanus", 12), ("fulgora", 40)] {
            write_stub_frame(dir.path(), &format!("frame_600_{surface}.stfr"), 600, surface, count);
        }

        let frames: Vec<Frame> = frame_paths(dir.path()).unwrap().iter().filter_map(|p| load_frame(p)).collect();
        let grouped = group_by_surface(frames);

        let names: Vec<&str> = grouped.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["nauvis", "fulgora", "vulcanus"], "busiest surface first");
        assert!(grouped.iter().all(|(_, frames)| frames.len() == 1));
    }

    #[test]
    fn group_by_surface_orders_and_dedups_each_surfaces_own_timeline() {
        let dir = tempfile::tempdir().unwrap();
        // nauvis across three ticks, written out of order, plus one
        // vulcanus tick sharing tick 600 with a busier nauvis frame that
        // must not swallow it the way single-surface loading would.
        write_stub_frame(dir.path(), "frame_1200_nauvis.stfr", 1200, "nauvis", 5);
        write_stub_frame(dir.path(), "frame_0000_nauvis.stfr", 0, "nauvis", 1);
        write_stub_frame(dir.path(), "frame_0600_nauvis.stfr", 600, "nauvis", 3);
        write_stub_frame(dir.path(), "frame_0600_vulcanus.stfr", 600, "vulcanus", 2);

        let frames: Vec<Frame> = frame_paths(dir.path()).unwrap().iter().filter_map(|p| load_frame(p)).collect();
        let grouped = group_by_surface(frames);

        let nauvis = grouped.iter().find(|(name, _)| name == "nauvis").unwrap();
        assert_eq!(nauvis.1.iter().map(|f| f.tick).collect::<Vec<_>>(), vec![0, 600, 1200]);

        let vulcanus = grouped.iter().find(|(name, _)| name == "vulcanus").unwrap();
        assert_eq!(vulcanus.1.len(), 1);
        assert_eq!(vulcanus.1[0].tick, 600);
    }

    /// Live capture writes these beside every finished snapshot. Their stem
    /// ends in `.stfr`, so they look exactly like a frame to a stem-only
    /// check, and being empty, each one would produce a parse warning.
    #[test]
    fn done_markers_and_event_logs_are_not_mistaken_for_frames() {
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("frame_22630009_nauvis.stfr");
        write_stub_frame(dir.path(), "frame_22630009_nauvis.stfr", 22630009, "nauvis", 0);
        std::fs::write(dir.path().join("frame_22630009_nauvis.stfr.done"), "").unwrap();
        std::fs::write(dir.path().join("frame_22630009_manifest.json"), "{}").unwrap();
        std::fs::write(dir.path().join("events_22630009.stev"), "").unwrap();

        assert_eq!(frame_paths(dir.path()).unwrap(), vec![frame]);

        let frames = load_sequence(dir.path()).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].tick, 22630009);
    }

    /// `terrain_<surface>.stfr` sits in the same directory as every
    /// `frame_*.stfr`, so it must never be picked up as an extra frame, even
    /// for a surface literally named "frame".
    #[test]
    fn terrain_files_are_not_mistaken_for_frames() {
        let dir = tempfile::tempdir().unwrap();
        let frame = dir.path().join("frame_0000_nauvis.stfr");
        write_stub_frame(dir.path(), "frame_0000_nauvis.stfr", 0, "nauvis", 0);
        write_stub_frame(dir.path(), "terrain_nauvis.stfr", 0, "nauvis", 0);
        write_stub_frame(dir.path(), "terrain_frame.stfr", 0, "frame", 0);

        assert_eq!(frame_paths(dir.path()).unwrap(), vec![frame]);
    }

    #[test]
    fn terrain_paths_finds_every_terrain_file_and_ignores_regular_frames() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_frame(dir.path(), "frame_0000_nauvis.stfr", 0, "nauvis", 0);
        write_stub_frame(dir.path(), "terrain_nauvis.stfr", 0, "nauvis", 0);
        write_stub_frame(dir.path(), "terrain_vulcanus.stfr", 0, "vulcanus", 0);

        let found = terrain_paths(dir.path()).unwrap();
        assert_eq!(found, vec![dir.path().join("terrain_nauvis.stfr"), dir.path().join("terrain_vulcanus.stfr")]);
    }

    #[test]
    fn terrain_paths_is_empty_for_a_path_that_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not_a_directory.stfr");
        write_stub_frame(dir.path(), "not_a_directory.stfr", 0, "nauvis", 0);

        assert!(terrain_paths(&file).unwrap().is_empty());
    }

    #[test]
    fn load_terrain_reads_a_surfaces_terrain_file() {
        let dir = tempfile::tempdir().unwrap();
        write_stub_frame(dir.path(), "terrain_nauvis.stfr", 0, "nauvis", 3);

        let frame = load_terrain(dir.path(), "nauvis").unwrap();
        assert_eq!(frame.entities.len(), 3);
    }

    #[test]
    fn load_terrain_is_none_when_the_surface_has_no_terrain_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_terrain(dir.path(), "nauvis").is_none());
    }
}
