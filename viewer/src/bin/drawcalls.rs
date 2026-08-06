//! Headless draw-call profiler. Reports what a frame costs to render before
//! and after per-type grouping, and at macroquad's default batch capacity
//! versus the viewer's raised one.
//!
//! Headless on purpose: it models macroquad's batching rule (see
//! `DrawCallCounter`) rather than driving a GPU, so it runs in CI, on a
//! machine with no Factorio, and without a window. Numbers are draw calls
//! submitted for a fully-visible frame -- the zoomed-out worst case, before
//! culling removes anything.
//!
//!     cargo run -p viewer --bin drawcalls --release [-- <frames dir or file>]

use std::path::Path;

use save_timelapse::frame::Frame;
use viewer::{DrawCallCounter, RenderFrame, TypeId, TypeRegistry};

/// macroquad's defaults (`conf::Conf`): 5,000 indices at 6 per quad.
const DEFAULT_INDEX_CAPACITY: usize = 5_000;
/// What the viewer now asks for.
const RAISED_INDEX_CAPACITY: usize = 4096 * 6;

fn calls(order: &[Option<TypeId>], max_indices: usize) -> usize {
    let mut counter = DrawCallCounter::new(max_indices);
    for &texture in order {
        counter.quad(texture);
    }
    counter.calls
}

/// Roughly what the parsed frame occupies: struct size plus one heap
/// allocation per name string. Approximate -- it ignores allocator overhead,
/// which makes it an underestimate of the real cost, not an overestimate.
fn parsed_bytes(frame: &Frame) -> usize {
    let entities: usize = frame
        .entities
        .iter()
        .map(|e| std::mem::size_of::<save_timelapse::frame::Entity>() + e.n.len())
        .sum();
    let tiles: usize = frame
        .tiles
        .iter()
        .map(|t| std::mem::size_of::<save_timelapse::frame::Tile>() + t.n.len())
        .sum();
    entities + tiles
}

fn grouped_bytes(frame: &RenderFrame) -> usize {
    frame.entities.len() * std::mem::size_of::<viewer::RenderEntity>()
        + frame.tiles.len() * std::mem::size_of::<viewer::RenderTile>()
        + (frame.entity_runs.len() + frame.tile_runs.len()) * std::mem::size_of::<viewer::Run>()
}

/// The real fixtures top out at 23k entities, where most per-type runs still
/// fit in one default-capacity draw call. `--synthetic N` reaches the scale
/// where the raised capacity is what actually matters.
fn synthetic_report(count: usize) {
    let frame = viewer::synthetic_frame(count);
    let mut registry = TypeRegistry::new();
    let rendered = RenderFrame::from_frame(frame, &mut registry);
    let order: Vec<Option<TypeId>> = rendered
        .entity_runs
        .iter()
        .flat_map(|run| std::iter::repeat(Some(run.type_id)).take(run.len()))
        .collect();

    println!("synthetic: {count} entities across {} types\n", registry.len());
    println!("  grouped, default capacity (833 quads/call) : {:>7} draw calls", calls(&order, DEFAULT_INDEX_CAPACITY));
    println!("  grouped, raised capacity (4096 quads/call) : {:>7} draw calls", calls(&order, RAISED_INDEX_CAPACITY));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--synthetic") {
        synthetic_report(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(500_000));
        return;
    }

    let path = args
        .first()
        .map(String::as_str)
        .unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frames"));

    let frames = match viewer::load_sequence(Path::new(path)) {
        Ok(frames) => frames,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    println!("draw calls for a fully-visible frame, sprites on (one texture per type)\n");
    println!(
        "{:>8}  {:>7}  {:>6}  {:>12}  {:>12}  {:>12}  {:>9}  {:>9}",
        "items", "types", "runs", "file/5k", "grouped/5k", "grouped/24k", "parsed", "grouped"
    );

    let mut totals = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    for path in viewer::frame_paths(Path::new(path)).unwrap_or_default() {
        let Some(frame) = viewer::load_frame(&path) else { continue };

        // Type ids in the order the exporter wrote them -- what the viewer
        // used to iterate, and the batching worst case.
        let mut registry = TypeRegistry::new();
        let mut file_order: Vec<Option<TypeId>> = Vec::with_capacity(frame.tiles.len() + frame.entities.len());
        // Tiles then entities, the order the viewer draws in.
        file_order.extend(frame.tiles.iter().map(|t| Some(registry.intern(&t.n))));
        file_order.extend(frame.entities.iter().map(|e| Some(registry.intern(&e.n))));

        let parsed = parsed_bytes(&frame);
        let items = file_order.len();
        let types = registry.len();

        let mut registry = TypeRegistry::new();
        let rendered = RenderFrame::from_frame(frame, &mut registry);
        let grouped_order: Vec<Option<TypeId>> = rendered
            .tile_runs
            .iter()
            .chain(rendered.entity_runs.iter())
            .flat_map(|run| std::iter::repeat(Some(run.type_id)).take(run.len()))
            .collect();

        let runs = rendered.tile_runs.len() + rendered.entity_runs.len();
        let before = calls(&file_order, DEFAULT_INDEX_CAPACITY);
        let after_default = calls(&grouped_order, DEFAULT_INDEX_CAPACITY);
        let after_raised = calls(&grouped_order, RAISED_INDEX_CAPACITY);
        let grouped = grouped_bytes(&rendered);

        println!(
            "{items:>8}  {types:>7}  {runs:>6}  {before:>12}  {after_default:>12}  \
             {after_raised:>12}  {:>8} KiB  {:>8} KiB",
            parsed / 1024,
            grouped / 1024
        );

        totals.0 += items;
        totals.1 += before;
        totals.2 += after_default;
        totals.3 += after_raised;
        totals.4 += parsed;
        totals.5 += grouped;
    }

    if totals.0 == 0 {
        return;
    }
    println!("\ntotals across {} frames, {} items", frames.len(), totals.0);
    println!("  file order, default capacity : {:>8} draw calls", totals.1);
    println!("  grouped,    default capacity : {:>8} draw calls", totals.2);
    println!(
        "  grouped,    raised capacity  : {:>8} draw calls   ({:.0}x fewer than file order)",
        totals.3,
        totals.1 as f64 / totals.3.max(1) as f64
    );
    println!(
        "  memory: {} KiB parsed -> {} KiB grouped ({:.1}x smaller)",
        totals.4 / 1024,
        totals.5 / 1024,
        totals.4 as f64 / totals.5.max(1) as f64
    );
}
