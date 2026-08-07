//! Rebuild a timelapse from a live-capture directory.
//!
//! The other half of the live-capture flow: the mod snapshots a save once and
//! then logs only placements and removals, and this walks that log forward
//! over the baseline to produce ordinary frames, the same `frame_NNNN.stfr`
//! the viewer already reads, so nothing downstream needs to know a timeline
//! came from events rather than from a folder of saves.
//!
//!     save-timelapse-replay --capture "%APPDATA%/Factorio/script-output/save-timelapse" --out frames
//!
//! Add `--all-surfaces` to render every surface instead of picking one, so
//! the viewer's tab key has more than one world to switch between.

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use save_timelapse::frame;
use save_timelapse::replay::{self, Options};
use save_timelapse::world::World;

#[derive(Parser)]
#[command(name = "save-timelapse-replay", version, about)]
struct Args {
    /// The mod's output directory: baseline.json, its frame_*.json, and the
    /// events_*.jsonl segments.
    #[arg(long)]
    capture: PathBuf,

    /// Where to write the reconstructed frames.
    #[arg(long, default_value = "frames")]
    out: PathBuf,

    /// Game ticks between frames. Factorio runs at 60 ticks a second, so the
    /// default is one frame per minute of game time.
    #[arg(long, default_value_t = 3600)]
    interval: u64,

    /// Stop after this many frames.
    #[arg(long, default_value_t = 100_000)]
    max_frames: usize,

    /// Which surface to render. Defaults to the one with the most entities,
    /// which on a Space Age save is usually the planet you built on.
    #[arg(long, conflicts_with = "all_surfaces")]
    surface: Option<String>,

    /// Render every surface instead of just one, writing
    /// `frame_<index>_<surface>.stfr` files the same way the mod's own
    /// baseline does. A surface with nothing on it at a given tick is
    /// skipped for that tick rather than writing an empty file.
    #[arg(long)]
    all_surfaces: bool,
}

/// Picks the surface with the most entities, falling back to "nauvis" if
/// somehow none are loaded (`load_baseline` already errors before this can
/// happen, so this is a name for `unwrap_or` rather than a real code path).
fn busiest_surface(world: &World) -> String {
    world
        .surface_names()
        .into_iter()
        .max_by_key(|name| world.surface(name).map(|s| s.entity_count()).unwrap_or(0))
        .unwrap_or("nauvis")
        .to_string()
}

fn run(args: Args) -> std::io::Result<()> {
    let mut replay = replay::load_baseline(&args.capture)?;

    println!(
        "baseline tick {} ({} entities, {} tiles)",
        replay.baseline.tick,
        replay.world.entity_count(),
        replay.world.tile_count()
    );
    println!("surfaces: {}", replay.world.surface_names().join(", "));

    std::fs::create_dir_all(&args.out)?;

    let options = Options { interval: args.interval, max_frames: args.max_frames };
    let out = args.out.clone();
    let mut written = 0usize;
    let mut error: Option<std::io::Error> = None;

    let emitted = if args.all_surfaces {
        println!("rendering every surface, one frame per {} tick(s)\n", args.interval);
        replay::run(&mut replay, &args.capture, &options, |world, tick| {
            if error.is_some() {
                return;
            }
            for surface in world.surface_names() {
                let frame = world.to_frame(surface, tick);
                if frame.entities.is_empty() && frame.tiles.is_empty() {
                    continue;
                }
                let path = out.join(format!("frame_{written:04}_{surface}.stfr"));
                if let Err(e) = std::fs::write(&path, frame::write_binary(&frame.as_out())) {
                    error = Some(e);
                    return;
                }
            }
            written += 1;
            if written % 25 == 0 {
                print!("\r{written} frames");
                std::io::stdout().flush().ok();
            }
        })?
    } else {
        let target = args.surface.unwrap_or_else(|| busiest_surface(&replay.world));
        println!("rendering surface {target}, one frame per {} tick(s)\n", args.interval);
        replay::run(&mut replay, &args.capture, &options, |world, tick| {
            if error.is_some() {
                return;
            }
            let frame = world.to_frame(&target, tick);
            let path = out.join(format!("frame_{written:04}.stfr"));
            if let Err(e) = std::fs::write(&path, frame::write_binary(&frame.as_out())) {
                error = Some(e);
                return;
            }
            written += 1;
            if written % 25 == 0 {
                print!("\r{written} frames");
                std::io::stdout().flush().ok();
            }
        })?
    };

    if let Some(e) = error {
        return Err(e);
    }

    println!("\r{emitted} frames written to {}", args.out.display());
    println!(
        "{} events applied, {} changed nothing",
        replay.applied_events, replay.no_op_events
    );
    if replay.no_op_events > replay.applied_events {
        println!(
            "\nnote: most events changed nothing, which usually means the log \
             and the baseline came from different playthroughs."
        );
    }
    Ok(())
}

fn main() {
    if let Err(err) = run(Args::parse()) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}
