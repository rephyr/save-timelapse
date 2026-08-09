//! The renderer-facing frame representation: items grouped into contiguous
//! per-type runs with names interned away, plus the chunk-level
//! level-of-detail data computed alongside it, and a sequence of these to
//! scrub through.

use std::collections::HashMap;

use macroquad::math::Vec2;
use save_timelapse::frame::Frame;

use crate::registry::{TypeId, TypeRegistry};
use crate::spans::{SpanBuilder, SpanSet};

/// A contiguous span of one type within a [`RenderFrame`]'s entity or tile
/// array. Draws iterate runs rather than individual items, so the texture is
/// bound once per type instead of being re-decided per entity, which is
/// what keeps macroquad from breaking the batch. See [`crate::DrawCallCounter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub type_id: TypeId,
    pub start: u32,
    pub end: u32,
}

impl Run {
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }

    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

/// An entity stripped to what drawing actually reads. The name is gone (the
/// enclosing [`Run`] carries it), so this is 12 bytes against roughly 80 for
/// a `frame::Entity`: 48 for the struct plus a heap allocation for a name
/// that was one of a few dozen repeated strings.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderEntity {
    pub x: f32,
    pub y: f32,
    /// Tile footprint, saturated into a byte: the format allows u32, but
    /// Factorio's largest prototypes are far under 255 tiles across.
    pub w: u8,
    pub h: u8,
    /// Factorio's raw 16-way direction byte (0 = north, clockwise in 22.5
    /// degree steps), used to rotate square-footprint entities on screen.
    /// See `entity_rotation_radians` in `camera.rs`.
    pub d: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderTile {
    pub x: i32,
    pub y: i32,
}

/// Granularity of the level-of-detail pass below: an `LOD_CELL_TILES`^2
/// square of the world collapses to one flat-colored quad, showing whichever
/// type is most common in it and discarding the rest.
///
/// Independent of Factorio's own 32-tile chunk size, despite starting out
/// equal to it: that was borrowed as a convenient existing grid, not
/// because rendering needs to align with it. 32 turned out too coarse:
/// aggregating 1,024 tiles into one dominant-type color loses so much that a
/// paved area with belts and machines running through it just reads as a
/// solid gray block. Smaller cells keep more structure recognizable, at the
/// cost of more (but still vastly fewer than full-detail) quads submitted,
/// halved again from 8 to 4 once 8 measured with FPS to spare.
pub const LOD_CELL_TILES: i32 = 4;

/// Below this on-screen tile size (in pixels), draw chunk-aggregated
/// [`LodCell`]s instead of individual items. At this scale a 1x1 entity
/// spans a fraction of a pixel anyway, so individual items are already
/// imperceptible, and the only question is whether the CPU still pays to
/// transform and submit each one. Comfortably below `SPRITE_MIN_PIXELS`, so
/// sprites are never in play once LOD is.
///
/// A *tile's* pixel size, not a *chunk's*: a chunk is 32 tiles across, so
/// gating on the chunk's on-screen size instead would only trigger LOD 32x
/// later than intended: a real base at the zoom level that motivated this
/// (0.32 px/tile, individual tiles long since imperceptible) measured 3.27M
/// quads submitted and 7 fps with that version of the check, because a
/// 10px chunk still looked "big enough" even though every tile inside it was
/// a third of a pixel.
pub const LOD_MAX_TILE_PIXELS: f32 = 2.0;

pub fn use_chunk_lod(pixels_per_tile: f32) -> bool {
    pixels_per_tile <= LOD_MAX_TILE_PIXELS
}

/// One `LOD_CELL_TILES`-square chunk of the world, drawn as a single
/// flat-colored quad. Only ever produced for the level-of-detail pass:
/// full-detail rendering uses [`RenderEntity`]/[`RenderTile`] instead.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LodCell {
    pub cx: i32,
    pub cy: i32,
}

impl LodCell {
    /// World-space top-left corner of this chunk.
    pub fn world_origin(&self) -> Vec2 {
        Vec2::new((self.cx * LOD_CELL_TILES) as f32, (self.cy * LOD_CELL_TILES) as f32)
    }
}

fn chunk_of(x: i32, y: i32) -> (i32, i32) {
    (x.div_euclid(LOD_CELL_TILES), y.div_euclid(LOD_CELL_TILES))
}

/// A frame in the layout the renderer wants: items grouped into contiguous
/// per-type runs, names interned away, dense and copyable.
pub struct RenderFrame {
    pub tick: u64,
    pub count: usize,
    pub entities: Vec<RenderEntity>,
    pub entity_runs: Vec<Run>,
    pub tiles: Vec<RenderTile>,
    pub tile_runs: Vec<Run>,
    /// Chunk-level level of detail, precomputed once at load rather than
    /// per rendered frame: at millions of tiles, even a cheap per-item
    /// binning pass is too slow to redo 60 times a second, but paid once
    /// while parsing it's the same order of cost as parsing itself.
    pub tile_lod: Vec<LodCell>,
    pub tile_lod_runs: Vec<Run>,
    pub entity_lod: Vec<LodCell>,
    pub entity_lod_runs: Vec<Run>,
}

/// Below this many items, `group_by_type`/`build_chunk_lod` just run
/// single-threaded: thread-spawn overhead would dwarf the actual work for
/// an ordinary per-tick frame, which is exactly the common case. Above it
/// (realistically only ever a large baseline or a terrain scan spanning
/// millions of tiles) they split across every available core instead.
const PARALLEL_THRESHOLD: usize = 10_000;

fn worker_count() -> usize {
    std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1)
}

/// Group `items` by type into contiguous runs, by counting sort.
///
/// Counting sort rather than `sort_by_key` because this is O(n) with one
/// pass to count and one to scatter, needs no comparisons, and avoids the
/// temporary `Vec<(TypeId, T)>` that sorting in place would require, which
/// at megabase entity counts is the difference between a brief allocation
/// spike and none.
fn group_by_type_sequential<T: Copy + Default>(ids: &[TypeId], items: &[T], type_count: usize) -> (Vec<T>, Vec<Run>) {
    let mut counts = vec![0u32; type_count + 1];
    for &id in ids {
        counts[id as usize + 1] += 1;
    }
    for i in 1..counts.len() {
        counts[i] += counts[i - 1];
    }

    let runs: Vec<Run> = (0..type_count)
        .filter(|&t| counts[t + 1] > counts[t])
        .map(|t| Run { type_id: t as TypeId, start: counts[t], end: counts[t + 1] })
        .collect();

    let mut cursors = counts;
    let mut grouped = vec![T::default(); items.len()];
    for (&id, &item) in ids.iter().zip(items) {
        let slot = &mut cursors[id as usize];
        grouped[*slot as usize] = item;
        *slot += 1;
    }

    (grouped, runs)
}

/// Same result as `group_by_type_sequential`, computed across every
/// available core once there's enough work to be worth it. Splits `ids`/
/// `items` into contiguous chunks (one per core) and runs the existing
/// sequential algorithm on each chunk in its own thread, reusing it as the
/// per-chunk worker rather than a separate parallel implementation, then
/// concatenates each type's slice across chunks *in chunk order*. Chunks
/// are contiguous slices of the original arrays in original order, so this
/// preserves the same stable within-type ordering the sequential version
/// produces (an item that came before another of the same type keeps
/// coming before it).
fn group_by_type<T: Copy + Default + Send + Sync>(ids: &[TypeId], items: &[T], type_count: usize) -> (Vec<T>, Vec<Run>) {
    let workers = worker_count();
    if ids.len() < PARALLEL_THRESHOLD || workers <= 1 {
        return group_by_type_sequential(ids, items, type_count);
    }

    let chunk_size = ids.len().div_ceil(workers);
    let partials: Vec<(Vec<T>, Vec<Run>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = ids
            .chunks(chunk_size)
            .zip(items.chunks(chunk_size))
            .map(|(id_chunk, item_chunk)| {
                scope.spawn(move || group_by_type_sequential(id_chunk, item_chunk, type_count))
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("group_by_type worker thread panicked")).collect()
    });

    let mut grouped = Vec::with_capacity(items.len());
    let mut runs = Vec::new();
    for type_id in 0..type_count as TypeId {
        let start = grouped.len() as u32;
        for (partial_grouped, partial_runs) in &partials {
            if let Some(run) = partial_runs.iter().find(|r| r.type_id == type_id) {
                grouped.extend_from_slice(&partial_grouped[run.range()]);
            }
        }
        let end = grouped.len() as u32;
        if end > start {
            runs.push(Run { type_id, start, end });
        }
    }

    (grouped, runs)
}

/// The per-item counting pass `build_chunk_lod` splits out from its own
/// finalization, so a large scan can run this half in parallel: see
/// `build_chunk_lod`'s own doc comment for why merging partial results
/// needs to sum rather than concatenate.
///
/// A per-chunk `Vec<(TypeId, count)>` rather than a `type_count`-sized array
/// per chunk: a chunk of floor tiles is realistically one or two types, and
/// a real base can have thousands of occupied chunks, so a dense per-chunk
/// array (tens of entries, nearly all zero) would waste far more than the
/// linear scan through a handful of real entries costs.
/// Per-chunk counts keyed by chunk coordinate, each value a list of
/// `(type, count)` pairs actually seen in that chunk (see `chunk_lod_counts`'s
/// own doc comment for why a short list beats a dense per-type array here).
type ChunkCounts = HashMap<(i32, i32), Vec<(TypeId, u32)>>;

fn chunk_lod_counts(ids: &[TypeId], chunk_coords: &[(i32, i32)]) -> ChunkCounts {
    let mut counts: ChunkCounts = HashMap::new();
    for (&coord, &id) in chunk_coords.iter().zip(ids) {
        let entry = counts.entry(coord).or_default();
        match entry.iter_mut().find(|(t, _)| *t == id) {
            Some((_, count)) => *count += 1,
            None => entry.push((id, 1)),
        }
    }
    counts
}

/// The other half of `build_chunk_lod`: picks the dominant type per chunk
/// from already-finished counts, then groups the resulting cells into
/// per-type runs the same way entities/tiles are.
fn finalize_chunk_lod(counts: ChunkCounts, type_count: usize) -> (Vec<LodCell>, Vec<Run>) {
    let mut cell_ids = Vec::with_capacity(counts.len());
    let mut cells = Vec::with_capacity(counts.len());
    for ((cx, cy), type_counts) in counts {
        let dominant = type_counts.into_iter().max_by_key(|&(_, count)| count).map(|(t, _)| t).unwrap_or(0);
        cell_ids.push(dominant);
        cells.push(LodCell { cx, cy });
    }

    group_by_type(&cell_ids, &cells, type_count)
}

/// Aggregate items into one dominant type per `LOD_CELL_TILES`-square chunk,
/// for the level-of-detail pass. `chunk_coords[i]` is the chunk containing
/// `ids[i]`/the item at the same index in whatever array these came from.
///
/// Below `PARALLEL_THRESHOLD` this is just `chunk_lod_counts` then
/// `finalize_chunk_lod`, unchanged from before the split. Above it, each
/// core computes its own *partial* counts on a slice of the input (chunks
/// split by item index, not by chunk coordinate, so the same `(cx, cy)`
/// can and does land in more than one thread's slice), and the partials
/// are merged by summing matching `(coord, type)` entries rather than
/// concatenating: summing is commutative, so correctness never depends on
/// which thread happened to see which tiles. That merge is bounded by
/// however many distinct chunks exist times the worker count, not by item
/// count, so it stays cheap even done single-threaded.
fn build_chunk_lod(ids: &[TypeId], chunk_coords: &[(i32, i32)], type_count: usize) -> (Vec<LodCell>, Vec<Run>) {
    let workers = worker_count();
    if ids.len() < PARALLEL_THRESHOLD || workers <= 1 {
        return finalize_chunk_lod(chunk_lod_counts(ids, chunk_coords), type_count);
    }

    let chunk_size = ids.len().div_ceil(workers);
    let partials: Vec<ChunkCounts> = std::thread::scope(|scope| {
        let handles: Vec<_> = ids
            .chunks(chunk_size)
            .zip(chunk_coords.chunks(chunk_size))
            .map(|(id_chunk, coord_chunk)| scope.spawn(move || chunk_lod_counts(id_chunk, coord_chunk)))
            .collect();
        handles.into_iter().map(|h| h.join().expect("build_chunk_lod worker thread panicked")).collect()
    });

    let mut merged: ChunkCounts = HashMap::new();
    for partial in partials {
        for (coord, type_counts) in partial {
            let entry = merged.entry(coord).or_default();
            for (type_id, count) in type_counts {
                match entry.iter_mut().find(|(t, _)| *t == type_id) {
                    Some((_, total)) => *total += count,
                    None => entry.push((type_id, count)),
                }
            }
        }
    }

    finalize_chunk_lod(merged, type_count)
}

impl RenderFrame {
    /// A frame with nothing in it, used as the buffer `FrameSequence`
    /// materializes into and reuses, so seeking allocates nothing once the
    /// vectors have grown to the largest frame seen.
    pub fn empty() -> RenderFrame {
        RenderFrame {
            tick: 0,
            count: 0,
            entities: Vec::new(),
            entity_runs: Vec::new(),
            tiles: Vec::new(),
            tile_runs: Vec::new(),
            tile_lod: Vec::new(),
            tile_lod_runs: Vec::new(),
            entity_lod: Vec::new(),
            entity_lod_runs: Vec::new(),
        }
    }

    /// Consumes the parsed frame: keeping both representations alive would
    /// defeat the point, since the `frame::Frame` is the expensive one.
    pub fn from_frame(frame: Frame, registry: &mut TypeRegistry) -> RenderFrame {
        let entity_ids: Vec<TypeId> = frame.entities.iter().map(|e| registry.intern(&e.n)).collect();
        let entities: Vec<RenderEntity> = frame
            .entities
            .iter()
            .map(|e| RenderEntity {
                x: e.x,
                y: e.y,
                w: e.w.clamp(1, u8::MAX as u32) as u8,
                h: e.h.clamp(1, u8::MAX as u32) as u8,
                d: e.d,
            })
            .collect();

        let tile_ids: Vec<TypeId> = frame.tiles.iter().map(|t| registry.intern(&t.n)).collect();
        let tiles: Vec<RenderTile> = frame.tiles.iter().map(|t| RenderTile { x: t.x, y: t.y }).collect();

        let type_count = registry.len();

        // Computed from the pre-grouped ids/positions, so this doesn't need
        // the full-detail grouping to have happened first.
        let entity_chunks: Vec<(i32, i32)> =
            entities.iter().map(|e| chunk_of(e.x.floor() as i32, e.y.floor() as i32)).collect();
        let (entity_lod, entity_lod_runs) = build_chunk_lod(&entity_ids, &entity_chunks, type_count);

        let tile_chunks: Vec<(i32, i32)> = tiles.iter().map(|t| chunk_of(t.x, t.y)).collect();
        let (tile_lod, tile_lod_runs) = build_chunk_lod(&tile_ids, &tile_chunks, type_count);

        let (entities, entity_runs) = group_by_type(&entity_ids, &entities, type_count);
        let (tiles, tile_runs) = group_by_type(&tile_ids, &tiles, type_count);

        RenderFrame {
            tick: frame.tick,
            count: frame.count,
            entities,
            entity_runs,
            tiles,
            tile_runs,
            tile_lod,
            tile_lod_runs,
            entity_lod,
            entity_lod_runs,
        }
    }
}

/// Position key for span identity, matching the tenth-of-a-tile grid
/// entities are aligned to and `save_timelapse::world::pos_key` keys by.
fn span_key(x: f32, y: f32) -> u64 {
    let qx = ((x as f64) * 10.0).round() as i32;
    let qy = ((y as f64) * 10.0).round() as i32;
    ((qx as u32 as u64) << 32) | (qy as u32 as u64)
}

/// A loaded sequence of frames with a current position. Always non-empty:
/// construction from zero frames is rejected rather than leaving every
/// accessor to guard against it.
///
/// Stores the whole run as spans (see `spans`) rather than one materialized
/// `RenderFrame` per frame, since consecutive frames of a real capture are
/// nearly identical and keeping every copy is what put a ceiling on how long
/// a capture could be. The frame being displayed is materialized into
/// `current`, and only when the position actually moves: rendering reads it
/// once per drawn frame, seeking rebuilds it a few times a second.
pub struct FrameSequence {
    entities: SpanSet<RenderEntity>,
    tiles: SpanSet<RenderTile>,
    entity_lod: SpanSet<LodCell>,
    tile_lod: SpanSet<LodCell>,
    /// Per frame, and tiny next to the item data, so these stay plain vecs.
    ticks: Vec<u64>,
    counts: Vec<usize>,
    /// The frame at `index`, rebuilt by `goto`. Handed out by `current` so
    /// the renderer keeps taking an ordinary `&RenderFrame`.
    current: RenderFrame,
    index: usize,
}

impl FrameSequence {
    /// Folds `frames` into spans, dropping each one as it goes.
    ///
    /// Takes them all at once for the convenience of tests and of callers
    /// that already have a vec; a loader wanting the memory win at load time
    /// too should fold frame by frame as it parses, since this still sees
    /// every frame alive at once.
    pub fn new(frames: Vec<RenderFrame>) -> Option<Self> {
        if frames.is_empty() {
            return None;
        }

        let mut entities = SpanBuilder::new();
        let mut tiles = SpanBuilder::new();
        let mut entity_lod = SpanBuilder::new();
        let mut tile_lod = SpanBuilder::new();
        let mut ticks = Vec::with_capacity(frames.len());
        let mut counts = Vec::with_capacity(frames.len());

        for frame in frames {
            ticks.push(frame.tick);
            counts.push(frame.count);
            entities.push_frame(runs_with_items(&frame.entity_runs, &frame.entities, &|e: &RenderEntity| span_key(e.x, e.y)));
            tiles.push_frame(runs_with_items(&frame.tile_runs, &frame.tiles, &|t: &RenderTile| span_key(t.x as f32, t.y as f32)));
            entity_lod.push_frame(runs_with_items(&frame.entity_lod_runs, &frame.entity_lod, &cell_key));
            tile_lod.push_frame(runs_with_items(&frame.tile_lod_runs, &frame.tile_lod, &cell_key));
        }

        let mut sequence = Self {
            entities: entities.finish(),
            tiles: tiles.finish(),
            entity_lod: entity_lod.finish(),
            tile_lod: tile_lod.finish(),
            ticks,
            counts,
            current: RenderFrame::empty(),
            index: 0,
        };
        sequence.rebuild_current();
        Some(sequence)
    }

    fn rebuild_current(&mut self) {
        let frame = &mut self.current;
        frame.tick = self.ticks[self.index];
        frame.count = self.counts[self.index];
        self.entities.materialize(self.index, &mut frame.entities, &mut frame.entity_runs);
        self.tiles.materialize(self.index, &mut frame.tiles, &mut frame.tile_runs);
        self.entity_lod.materialize(self.index, &mut frame.entity_lod, &mut frame.entity_lod_runs);
        self.tile_lod.materialize(self.index, &mut frame.tile_lod, &mut frame.tile_lod_runs);
    }

    pub fn current(&self) -> &RenderFrame {
        &self.current
    }

    /// The in-game tick of any frame, without materializing it: the timeline
    /// labels every position on the bar and needs nothing else about them.
    pub fn tick_at(&self, index: usize) -> Option<u64> {
        self.ticks.get(index).copied()
    }

    /// Walks every frame in order, materializing each into a scratch buffer
    /// that is reused throughout, for the load-time passes that need to see
    /// the whole run once (camera fit, growing bounds, activity).
    ///
    /// A callback rather than an iterator because each frame is a temporary:
    /// there is no per-frame storage left to hand out a borrow of, which is
    /// the entire point.
    pub fn for_each_frame(&self, mut visit: impl FnMut(usize, &RenderFrame)) {
        let mut scratch = RenderFrame::empty();
        for index in 0..self.len() {
            scratch.tick = self.ticks[index];
            scratch.count = self.counts[index];
            self.entities.materialize(index, &mut scratch.entities, &mut scratch.entity_runs);
            self.tiles.materialize(index, &mut scratch.tiles, &mut scratch.tile_runs);
            self.entity_lod.materialize(index, &mut scratch.entity_lod, &mut scratch.entity_lod_runs);
            self.tile_lod.materialize(index, &mut scratch.tile_lod, &mut scratch.tile_lod_runs);
            visit(index, &scratch);
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Total spans across all four sets, for measuring what the layout costs.
    pub fn span_estimate(&self) -> usize {
        self.entities.span_count()
            + self.tiles.span_count()
            + self.entity_lod.span_count()
            + self.tile_lod.span_count()
    }

    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Clamps at the sequence's ends rather than wrapping.
    pub fn goto(&mut self, index: usize) {
        let index = index.min(self.len() - 1);
        if index != self.index {
            self.index = index;
            self.rebuild_current();
        }
    }

    pub fn step_forward(&mut self) {
        self.goto(self.index + 1);
    }

    pub fn step_back(&mut self) {
        self.goto(self.index.saturating_sub(1));
    }
}

/// Pairs each item with the type of the run it belongs to, which is how the
/// span builder wants a frame: runs carry the type, items carry the rest.
fn runs_with_items<'a, T: Copy + 'a>(
    runs: &'a [Run],
    items: &'a [T],
    key: &'a impl Fn(&T) -> u64,
) -> impl Iterator<Item = (u64, TypeId, T)> + 'a {
    // `key` is taken by reference so the inner closure can capture a Copy
    // shared borrow: capturing the closure itself by move would have it
    // escape the outer `FnMut`, which `flat_map` will not allow.
    runs.iter()
        .flat_map(move |run| items[run.range()].iter().map(move |item| (key(item), run.type_id, *item)))
}

fn cell_key(cell: &LodCell) -> u64 {
    ((cell.cx as u32 as u64) << 32) | (cell.cy as u32 as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use save_timelapse::frame::{Entity, Tile};

    fn entity(n: &str, x: f32, y: f32) -> Entity {
        Entity { n: n.into(), x, y, d: 0, w: 1, h: 1 }
    }

    fn render(frame: Frame) -> RenderFrame {
        RenderFrame::from_frame(frame, &mut TypeRegistry::new())
    }

    fn sample_frame(tick: u64) -> RenderFrame {
        render(Frame { tick, surface: "nauvis".to_string(), count: 0, entities: Vec::new(), tiles: Vec::new() })
    }

    #[test]
    fn render_frame_groups_entities_into_contiguous_runs_per_type() {
        let mut registry = TypeRegistry::new();
        let frame = Frame {
            tick: 7,
            surface: "nauvis".to_string(),
            count: 5,
            // Deliberately interleaved, the order a real export produces.
            entities: vec![
                entity("belt", 0.0, 0.0),
                entity("pipe", 1.0, 0.0),
                entity("belt", 2.0, 0.0),
                entity("pole", 3.0, 0.0),
                entity("pipe", 4.0, 0.0),
            ],
            tiles: Vec::new(),
        };
        let rendered = RenderFrame::from_frame(frame, &mut registry);

        assert_eq!(rendered.entities.len(), 5);
        assert_eq!(rendered.entity_runs.len(), 3, "one run per distinct type");
        assert_eq!(rendered.entity_runs.iter().map(Run::len).sum::<usize>(), 5);

        // Runs must tile the array end to end with no gap or overlap.
        let mut cursor = 0;
        for run in &rendered.entity_runs {
            assert_eq!(run.start as usize, cursor, "runs must be contiguous");
            cursor = run.end as usize;
        }
        assert_eq!(cursor, 5);

        // Every item inside a run really is that type: the belts sit
        // together, and they kept their positions through the regrouping.
        let belt = registry.intern("belt");
        let belt_run = rendered.entity_runs.iter().find(|r| r.type_id == belt).unwrap();
        let xs: Vec<f32> = rendered.entities[belt_run.range()].iter().map(|e| e.x).collect();
        assert_eq!(xs, vec![0.0, 2.0]);
    }

    #[test]
    fn render_frame_groups_tiles_and_preserves_coordinates() {
        let mut registry = TypeRegistry::new();
        let frame = Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 0,
            entities: Vec::new(),
            tiles: vec![
                Tile { n: "concrete".into(), x: -5, y: 1 },
                Tile { n: "stone-path".into(), x: 2, y: 2 },
                Tile { n: "concrete".into(), x: -6, y: 3 },
            ],
        };
        let rendered = RenderFrame::from_frame(frame, &mut registry);
        assert_eq!(rendered.tile_runs.len(), 2);

        let concrete = registry.intern("concrete");
        let run = rendered.tile_runs.iter().find(|r| r.type_id == concrete).unwrap();
        let mut coords: Vec<(i32, i32)> = rendered.tiles[run.range()].iter().map(|t| (t.x, t.y)).collect();
        coords.sort();
        assert_eq!(coords, vec![(-6, 3), (-5, 1)]);
    }

    /// The existing tests above all use small item counts, so none of them
    /// ever cross `PARALLEL_THRESHOLD` and exercise the parallel path at
    /// all. This is the one that actually does, checking it against the
    /// known-correct sequential implementation directly rather than just
    /// checking it doesn't crash.
    #[test]
    fn group_by_type_parallel_path_matches_sequential_at_scale() {
        let type_count = 5;
        let n = PARALLEL_THRESHOLD * 3;
        let ids: Vec<TypeId> = (0..n).map(|i| (i % type_count) as TypeId).collect();
        let items: Vec<u32> = (0..n as u32).collect();

        let (parallel_grouped, parallel_runs) = group_by_type(&ids, &items, type_count);
        let (sequential_grouped, sequential_runs) = group_by_type_sequential(&ids, &items, type_count);

        assert_eq!(parallel_grouped, sequential_grouped, "must keep the same stable within-type order");
        assert_eq!(parallel_runs, sequential_runs);
    }

    /// Same idea for `build_chunk_lod`, with chunk coordinates deliberately
    /// cycled across the whole array (not clustered) so any index-based
    /// worker split still has more than one worker see the same chunk
    /// coordinate, exercising the summed merge the parallel path needs
    /// (see `build_chunk_lod`'s own doc comment for why). Compared by
    /// content rather than position: `finalize_chunk_lod` iterates a
    /// `HashMap`, whose order isn't guaranteed stable between the
    /// sequential and parallel paths' separately-constructed maps even for
    /// identical input.
    #[test]
    fn build_chunk_lod_parallel_path_matches_sequential_at_scale() {
        let type_count = 4;
        let n = PARALLEL_THRESHOLD * 3;
        let ids: Vec<TypeId> = (0..n).map(|i| (i % type_count) as TypeId).collect();
        let chunk_coords: Vec<(i32, i32)> = (0..n).map(|i| ((i % 3) as i32, 0)).collect();

        let (parallel_cells, parallel_runs) = build_chunk_lod(&ids, &chunk_coords, type_count);
        let (sequential_cells, sequential_runs) =
            finalize_chunk_lod(chunk_lod_counts(&ids, &chunk_coords), type_count);

        let content = |cells: &[LodCell], runs: &[Run]| {
            let mut pairs: Vec<(TypeId, i32, i32)> = runs
                .iter()
                .flat_map(|r| cells[r.range()].iter().map(move |c| (r.type_id, c.cx, c.cy)))
                .collect();
            pairs.sort();
            pairs
        };

        assert_eq!(content(&parallel_cells, &parallel_runs), content(&sequential_cells, &sequential_runs));
    }

    #[test]
    fn render_frame_saturates_oversized_footprints_into_a_byte() {
        let mut registry = TypeRegistry::new();
        let frame = Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 1,
            entities: vec![Entity { n: "huge".into(), x: 0.0, y: 0.0, d: 0, w: 100_000, h: 0 }],
            tiles: Vec::new(),
        };
        let rendered = RenderFrame::from_frame(frame, &mut registry);
        assert_eq!(rendered.entities[0].w, u8::MAX);
        assert_eq!(rendered.entities[0].h, 1, "a zero footprint must not become invisible");
    }

    #[test]
    fn use_chunk_lod_switches_on_below_the_pixel_threshold() {
        assert!(!use_chunk_lod(5.0), "5px tiles are still individually visible");
        assert!(use_chunk_lod(0.5), "0.5px tiles are already sub-pixel");
    }

    /// Regression: gating on the *chunk's* on-screen size (chunk being 32
    /// tiles) instead of the tile's meant LOD only engaged 32x further out
    /// than intended. 0.32 px/tile is a real measurement from a base that
    /// stayed in full detail (3.27M quads, 7 fps) under that version.
    #[test]
    fn use_chunk_lod_engages_well_before_a_tile_is_sub_pixel() {
        assert!(use_chunk_lod(0.32));
    }

    #[test]
    fn chunk_lod_aggregates_a_dense_area_into_one_cell_per_chunk() {
        let mut registry = TypeRegistry::new();
        // A strip of concrete spanning one chunk plus one tile into the
        // next, so this must produce exactly two cells, not one per tile.
        // The spill must stay under one cell's width or this would land in
        // three-plus chunks instead of two, regardless of LOD_CELL_TILES.
        let tiles: Vec<Tile> = (0..LOD_CELL_TILES + 1).map(|x| Tile { n: "concrete".into(), x, y: 0 }).collect();
        let frame = Frame { tick: 0, surface: "nauvis".to_string(), count: 0, entities: Vec::new(), tiles };
        let rendered = RenderFrame::from_frame(frame, &mut registry);

        assert_eq!(rendered.tile_lod.len(), 2, "one cell per occupied chunk, not per tile");
        let mut coords: Vec<(i32, i32)> = rendered.tile_lod.iter().map(|c| (c.cx, c.cy)).collect();
        coords.sort();
        assert_eq!(coords, vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn chunk_lod_picks_the_dominant_type_when_a_chunk_has_several() {
        let mut registry = TypeRegistry::new();
        let mut tiles = vec![Tile { n: "concrete".into(), x: 0, y: 0 }; 5];
        tiles.push(Tile { n: "stone-path".into(), x: 1, y: 0 });
        let frame = Frame { tick: 0, surface: "nauvis".to_string(), count: 0, entities: Vec::new(), tiles };
        let rendered = RenderFrame::from_frame(frame, &mut registry);

        assert_eq!(rendered.tile_lod.len(), 1, "still one chunk");
        let concrete = registry.intern("concrete");
        assert_eq!(rendered.tile_lod_runs[0].type_id, concrete, "5 concrete outnumbers 1 stone-path");
    }

    #[test]
    fn chunk_lod_covers_entities_too_keyed_by_their_floored_position() {
        let mut registry = TypeRegistry::new();
        let frame = Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 2,
            // -0.5 floors to -1, landing in the chunk to the left of origin.
            // Exercises div_euclid rather than plain integer division,
            // which would incorrectly floor a negative toward zero. The
            // second entity sits in the last column of chunk (0,0).
            entities: vec![entity("pipe", -0.5, -0.5), entity("pipe", (LOD_CELL_TILES - 1) as f32 + 0.5, 0.5)],
            tiles: Vec::new(),
        };
        let rendered = RenderFrame::from_frame(frame, &mut registry);

        assert_eq!(rendered.entity_lod.len(), 2);
        let mut coords: Vec<(i32, i32)> = rendered.entity_lod.iter().map(|c| (c.cx, c.cy)).collect();
        coords.sort();
        assert_eq!(coords, vec![(-1, -1), (0, 0)]);
    }

    #[test]
    fn lod_cell_world_origin_is_the_chunks_top_left_corner() {
        let cell = LodCell { cx: -2, cy: 3 };
        assert_eq!(cell.world_origin(), Vec2::new((-2 * LOD_CELL_TILES) as f32, (3 * LOD_CELL_TILES) as f32));
    }

    #[test]
    fn frame_sequence_new_rejects_empty() {
        assert!(FrameSequence::new(Vec::new()).is_none());
    }

    #[test]
    fn frame_sequence_stepping_clamps_at_both_ends() {
        let mut seq = FrameSequence::new(vec![sample_frame(0), sample_frame(1), sample_frame(2)]).unwrap();
        assert_eq!(seq.index(), 0);
        seq.step_back();
        assert_eq!(seq.index(), 0, "stepping back at the start should clamp, not wrap");

        seq.step_forward();
        seq.step_forward();
        seq.step_forward();
        assert_eq!(seq.index(), 2, "stepping past the end should clamp at the last frame");

        seq.goto(100);
        assert_eq!(seq.index(), 2);
    }
}
