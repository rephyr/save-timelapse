//! Rebuild a timelapse from a live-capture directory.
//!
//! The other half of the live-capture flow: the mod snapshots a save once and
//! then logs only placements and removals, and this walks that log forward
//! over the baseline to produce ordinary frames, the same `frame_NNNN.stfr`
//! the viewer already reads, so nothing downstream needs to know a timeline
//! came from events rather than from a folder of saves.
//!
//!     save-timelapse-replay --capture "%APPDATA%/Factorio/script-output/save-timelapse" --out frames

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use save_timelapse::replay::{self, Options};

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
    #[arg(long)]
    surface: Option<String>,
}

fn run(args: Args) -> std::io::Result<()> {
    let mut replay = replay::load_baseline(&args.capture)?;

    let surface = match args.surface {
        Some(name) => name,
        None => replay
            .world
            .surface_names()
            .into_iter()
            .max_by_key(|name| {
                replay.world.surface(name).map(|s| s.entity_count()).unwrap_or(0)
            })
            .unwrap_or("nauvis")
            .to_string(),
    };

    println!("baseline tick {} ({} entities, {} tiles)",
        replay.baseline.tick, replay.world.entity_count(), replay.world.tile_count());
    println!("surfaces: {}", replay.world.surface_names().join(", "));
    println!("rendering surface {surface}, one frame per {} tick(s)\n", args.interval);

    std::fs::create_dir_all(&args.out)?;

    let options = Options { interval: args.interval, max_frames: args.max_frames };
    let out = args.out.clone();
    let target = surface.clone();
    let mut written = 0usize;
    let mut error: Option<std::io::Error> = None;

    let emitted = replay::run(&mut replay, &args.capture, &options, |world, tick| {
        if error.is_some() {
            return;
        }
        let frame = world.to_frame(&target, tick);
        let path = out.join(format!("frame_{written:04}.stfr"));
        if let Err(e) = std::fs::write(&path, save_timelapse::frame::write_binary(&frame.as_out())) {
            error = Some(e);
            return;
        }
        written += 1;
        if written % 25 == 0 {
            print!("\r{written} frames");
            std::io::stdout().flush().ok();
        }
    })?;

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
