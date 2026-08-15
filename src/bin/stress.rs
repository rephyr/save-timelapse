//! Development benchmark: did the change I just made help or hurt?
//!
//! Runs the whole pipeline at megabase scale and compares every number
//! against a saved baseline, so the answer is a set of deltas rather than a
//! wall of absolutes nobody can hold in their head:
//!
//!     build a world -> write frames -> read them back -> analyse -> seek
//!
//! Workflow is two commands. Take a baseline on the code as it stands, make
//! the change, run again:
//!
//!     make stress-save     # or: cargo run -p viewer --release --bin stress -- --save
//!     ...edit...
//!     make stress          # prints current vs baseline, with deltas
//!
//! **File sizes and counts are exact and deterministic**, so any delta there
//! is a real consequence of the change. **Timings are not**: a few percent
//! swing between identical runs is ordinary, so anything under
//! `NOISE_THRESHOLD` is marked as such rather than read as a result. Mixing
//! those two up is the main way a benchmark like this misleads its owner.
//!
//! Defaults are shaped like a real Space Age megabase: nine surfaces, 900k
//! entities, 200 frames. Every dimension is overridable, because the
//! interesting failures are at the edges (one enormous surface, or many tiny
//! ones). A baseline is only comparable against the same shape, so changing
//! these means taking a new one:
//!
//!     cargo run -p viewer --release --bin stress -- --surfaces 1 --entities 2000000
//!     cargo run -p viewer --release --bin stress -- --frames 1000 --built 50
//!
//! Synthetic rather than a real capture on purpose: this has to run on any
//! machine, with no Factorio and no saved game, and it has to be identical
//! every run or the file sizes would not be comparable.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Timing swing between identical runs that should not be read as a result.
/// Deliberately generous: this is one run, not a statistical harness, and a
/// background process or a thermal step is worth several percent on its own.
const NOISE_THRESHOLD: f64 = 10.0;

/// What a number is, which decides whether a delta in it means anything.
#[derive(Clone, Copy, PartialEq)]
enum Unit {
    /// Deterministic. Any change is a real change.
    Count,
    /// Deterministic. Any change is a real change, and this is usually the
    /// one actually being chased.
    Bytes,
    Seconds,
    Millis,
}

impl Unit {
    fn exact(self) -> bool {
        matches!(self, Unit::Count | Unit::Bytes)
    }

    fn render(self, value: f64) -> String {
        match self {
            Unit::Count => format!("{}", value as u64),
            Unit::Bytes => format!("{:.1} MB", value / (1024.0 * 1024.0)),
            Unit::Seconds => format!("{value:.2}s"),
            Unit::Millis => format!("{value:.3} ms"),
        }
    }
}

struct Metric {
    key: &'static str,
    value: f64,
    unit: Unit,
}

/// Under `target/`, which is already gitignored: this is a local development
/// aid, and a baseline is only meaningful on the machine that took it.
fn baseline_path() -> PathBuf {
    // CARGO_MANIFEST_DIR is the viewer crate; the workspace target/ is beside
    // its parent.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|root| root.join("target"))
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("stress-baseline.tsv")
}

fn read_baseline() -> Vec<(String, f64)> {
    let Ok(text) = std::fs::read_to_string(baseline_path()) else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('\t')?;
            Some((key.to_string(), value.parse().ok()?))
        })
        .collect()
}

fn write_baseline(metrics: &[Metric]) {
    let body: String = metrics.iter().map(|m| format!("{}\t{}\n", m.key, m.value)).collect();
    let path = baseline_path();
    match std::fs::write(&path, body) {
        Ok(()) => println!("\n  baseline saved to {}", path.display()),
        Err(e) => eprintln!("\n  could not save the baseline to {}: {e}", path.display()),
    }
}

/// Prints each metric beside its baseline, with the delta and whether that
/// delta is meaningful.
fn report(metrics: &[Metric], saving: bool) {
    let baseline = read_baseline();
    let previous = |key: &str| baseline.iter().find(|(k, _)| k == key).map(|(_, v)| *v);

    if baseline.is_empty() {
        println!("  {:<18}{:>12}", "metric", "current");
        for m in metrics {
            println!("  {:<18}{:>12}", m.key, m.unit.render(m.value));
        }
        println!("\n  no baseline yet. Run with --save to record one, then compare against it.");
        if saving {
            write_baseline(metrics);
        }
        return;
    }

    println!("  {:<18}{:>12}{:>14}{:>16}", "metric", "current", "baseline", "delta");
    for m in metrics {
        let Some(was) = previous(m.key) else {
            println!("  {:<18}{:>12}{:>14}{:>16}", m.key, m.unit.render(m.value), "-", "new");
            continue;
        };

        let delta = if was == 0.0 { 0.0 } else { (m.value - was) / was * 100.0 };
        let note = if m.value == was {
            "same".to_string()
        } else if m.unit.exact() {
            format!("{delta:+.1}%")
        } else if delta.abs() < NOISE_THRESHOLD {
            format!("{delta:+.1}% noise")
        } else {
            format!("{delta:+.1}%")
        };
        println!("  {:<18}{:>12}{:>14}{:>16}", m.key, m.unit.render(m.value), m.unit.render(was), note);
    }

    println!("\n  Sizes and counts are exact: any delta is a real consequence of the change.");
    println!("  Timings swing a few percent between identical runs; under {NOISE_THRESHOLD:.0}% is marked noise.");
    println!("  baseline: {}", baseline_path().display());

    if saving {
        write_baseline(metrics);
    }
}

use save_timelapse::event::Event;
use save_timelapse::frame::{Entity, Frame, Tile};
use save_timelapse::replay::write_all_surfaces;
use save_timelapse::viewer::{analyze_activity, growing_bounds_per_frame, FrameSequence, RenderFrame, TypeRegistry};
use save_timelapse::world::World;

struct Config {
    surfaces: usize,
    entities: usize,
    tiles: usize,
    frames: usize,
    built: usize,
    save: bool,
}

impl Default for Config {
    fn default() -> Self {
        // Shaped like the real capture this was built against: nine surfaces
        // (Nauvis, three planets, five platforms), 100k entities each, and a
        // couple of hundred emitted frames.
        Config { surfaces: 9, entities: 100_000, tiles: 20_000, frames: 200, built: 500, save: false }
    }
}

fn parse_args() -> Config {
    let mut config = Config::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--save" {
            config.save = true;
            i += 1;
            continue;
        }
        let Some(raw) = args.get(i + 1) else {
            eprintln!("stress: {} needs a value", args[i]);
            std::process::exit(2);
        };
        let value: usize = raw.parse().unwrap_or_else(|_| {
            eprintln!("stress: {} needs a number, got {raw}", args[i]);
            std::process::exit(2);
        });
        match args[i].as_str() {
            "--surfaces" => config.surfaces = value.max(1),
            "--entities" => config.entities = value,
            "--tiles" => config.tiles = value,
            "--frames" => config.frames = value.max(1),
            "--built" => config.built = value,
            other => {
                eprintln!("stress: unknown option {other}");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    config
}

fn surface_name(index: usize) -> String {
    match index {
        0 => "nauvis".to_string(),
        1 => "vulcanus".to_string(),
        2 => "gleba".to_string(),
        3 => "fulgora".to_string(),
        other => format!("platform-{}", other - 3),
    }
}

/// A grid of entities cycling through a realistic number of prototypes. Forty
/// is close to what a real base uses, and the count matters: it is the number
/// of per-type runs every frame carries, and therefore how much of the
/// grouping and draw-batching work is real rather than degenerate.
fn baseline_frame(surface: &str, entities: usize, tiles: usize) -> Frame {
    let names: Vec<String> = (0..40).map(|i| format!("prototype-{i}")).collect();
    let side = (entities as f64).sqrt().ceil() as i32;

    let built = (0..entities)
        .map(|i| {
            let (x, y) = ((i as i32 % side) as f32, (i as i32 / side) as f32);
            Entity { n: names[i % names.len()].as_str().into(), x, y, d: 0, w: 1, h: 1 }
        })
        .collect();

    // Placed floor, not natural terrain: terrain is written once and never
    // re-serialized, so including it here would inflate the numbers with work
    // the real pipeline does exactly once.
    let tile_side = (tiles as f64).sqrt().ceil().max(1.0) as i32;
    let floor = (0..tiles).map(|i| Tile { n: "concrete".into(), x: i as i32 % tile_side, y: i as i32 / tile_side }).collect();

    Frame {
        tick: 0,
        surface: surface.to_string(),
        count: entities,
        entities: built,
        tiles: floor,
        floor_unchanged: false,
        ..Default::default()
    }
}

fn main() {
    let config = parse_args();
    let names: Vec<String> = (0..config.surfaces).map(surface_name).collect();

    println!(
        "stress: {} surfaces x {} entities ({} total), {} tiles each, {} frames, {} built per frame\n",
        config.surfaces,
        config.entities,
        config.surfaces * config.entities,
        config.tiles,
        config.frames,
        config.built
    );

    // Build the world
    let start = Instant::now();
    let mut world = World::new();
    for name in &names {
        world.load_baseline(&baseline_frame(name, config.entities, config.tiles));
    }
    let build = start.elapsed();

    // Write frames, exercising the skip
    //
    // One surface is built on per frame, rotating, which is what a real
    // playthrough looks like: you are somewhere, and everywhere else is idle.
    // That is exactly the shape `write_all_surfaces` exists to exploit, so a
    // stress test that built on every surface every frame would measure a
    // case that never happens and report no saving.
    let out = std::env::temp_dir().join(format!("save-timelapse-stress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("create the scratch directory");

    let start = Instant::now();
    let mut revisions = std::collections::HashMap::new();
    let mut files = 0usize;
    let mut next_id = 1u64;
    for frame in 0..config.frames {
        let active = &names[frame % names.len()];
        for i in 0..config.built {
            let offset = (frame * config.built + i) as f32;
            world.apply(
                Some(active),
                &Event::AddEntity {
                    name: "prototype-0".to_string(),
                    x: -1.0 - offset % 500.0,
                    y: -1.0 - offset / 500.0,
                    d: 0,
                    w: 1,
                    h: 1,
                    id: Some(next_id),
                },
            );
            next_id += 1;
        }
        files += write_all_surfaces(&mut world, frame as u64 * 3600, &out, frame, &mut revisions).expect("write");
    }
    let write = start.elapsed();

    let bytes: usize = std::fs::read_dir(&out)
        .expect("read the scratch directory")
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len() as usize)
        .sum();
    let unskipped = config.frames * config.surfaces;

    // Read them back, filling the gaps the skip left
    //
    // Mirrors the viewer's own loading loop (see `viewer/src/main.rs`), minus
    // the progress bar it interleaves. Kept in step by hand, which is a real
    // cost: if that loop changes, this measures something the viewer no
    // longer does.
    let start = Instant::now();
    let paths = save_timelapse::viewer::frame_paths(&out).expect("enumerate");
    let grouped = save_timelapse::viewer::group_paths_by_surface(paths);
    let timeline = save_timelapse::viewer::timeline_ticks(&grouped);

    let mut registry = TypeRegistry::new();
    let mut sequences: Vec<(String, FrameSequence)> = Vec::new();
    let mut spans_total = 0usize;
    for (name, entries) in grouped {
        let mut builder = FrameSequence::builder();
        let mut filled = 0usize;
        for (_, path) in &entries {
            let Some(frame) = save_timelapse::viewer::load_frame(path) else { continue };
            if let Some(offset) = timeline[filled..].iter().position(|&t| t == frame.tick) {
                builder.push_repeats(&timeline[filled..filled + offset]);
                filled += offset + 1;
            }
            builder.push(&RenderFrame::from_frame(frame, &mut registry));
        }
        builder.push_repeats(&timeline[filled..]);
        if let Some(sequence) = builder.finish(&registry) {
            spans_total += sequence.span_estimate();
            sequences.push((name, sequence));
        }
    }
    let load = start.elapsed();
    let restored: usize = sequences.iter().map(|(_, s)| s.len()).sum();

    // The load-time analysis passes
    // `black_box` on both results, not decoration: neither value is used
    // afterwards, and without it the optimizer is entitled to notice that and
    // delete the call, leaving a timing of nothing at all. A benchmark that
    // silently measures an elided function is worse than no benchmark.
    let start = Instant::now();
    for (_, sequence) in &sequences {
        std::hint::black_box(growing_bounds_per_frame(sequence, &registry));
    }
    let bounds = start.elapsed();

    let start = Instant::now();
    for (_, sequence) in &sequences {
        std::hint::black_box(analyze_activity(sequence, &registry));
    }
    let activity = start.elapsed();

    // Seeking, which is what scrubbing actually costs
    let start = Instant::now();
    let mut seeks = 0usize;
    for (_, sequence) in &mut sequences {
        let len = sequence.len();
        // Deliberately jumping around rather than walking forward: dragging
        // the scrub bar lands anywhere, and materializing a frame is the same
        // work wherever it is, so a sequential walk would flatter the cache.
        for step in 0..len {
            sequence.goto((step * 7 + step / 3) % len);
            seeks += 1;
        }
    }
    let seeking = start.elapsed();
    let per_seek = if seeks == 0 { 0.0 } else { seeking.as_secs_f64() * 1000.0 / seeks as f64 };
    let _ = std::fs::remove_dir_all(&out);

    let total = build + write + load + bounds + activity + seeking;
    let s = |d: Duration| d.as_secs_f64();

    // Ordered so the two lines a size change shows up in sit together and
    // near the top: `write_files` and `write_bytes` are the deterministic
    // ones, and they are usually what a change is actually being judged on.
    let metrics = vec![
        Metric { key: "write_files", value: files as f64, unit: Unit::Count },
        Metric { key: "write_bytes", value: bytes as f64, unit: Unit::Bytes },
        Metric { key: "files_skipped", value: (unskipped - files) as f64, unit: Unit::Count },
        Metric { key: "frames_restored", value: restored as f64, unit: Unit::Count },
        Metric { key: "spans", value: spans_total as f64, unit: Unit::Count },
        Metric { key: "build_world", value: s(build), unit: Unit::Seconds },
        Metric { key: "write_frames", value: s(write), unit: Unit::Seconds },
        Metric { key: "load_and_fill", value: s(load), unit: Unit::Seconds },
        Metric { key: "growing_bounds", value: s(bounds), unit: Unit::Seconds },
        Metric { key: "activity_heat", value: s(activity), unit: Unit::Seconds },
        Metric { key: "seek_each", value: per_seek, unit: Unit::Millis },
        Metric { key: "total", value: s(total), unit: Unit::Seconds },
    ];

    report(&metrics, config.save);
}
