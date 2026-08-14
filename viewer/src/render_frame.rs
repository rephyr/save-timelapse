//! The renderer-facing frame: items grouped into per-type runs with names
//! interned away, the chunk-level LOD data computed alongside, and a sequence
//! of these to scrub through.

use std::collections::HashMap;

use macroquad::math::Vec2;
use save_timelapse::frame::Frame;

use crate::registry::{TypeId, TypeRegistry};
use crate::spans::{Span, SpanBuilder, SpanSet};

/// A contiguous span of one type within a [`RenderFrame`]'s array. Draws
/// iterate runs rather than items, so the texture is bound once per type,
/// which is what keeps macroquad from breaking the batch.
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

/// An entity stripped to what drawing reads. The name is gone, the enclosing
/// [`Run`] carrying it, so this is 12 bytes against roughly 80 for a
/// `frame::Entity`.
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
    /// Which way a belt bends, as a `BeltShape` byte. Worked out from the
    /// neighbours after a frame is read (see `belts::infer_shapes`) rather
    /// than stored, so it is correct on captures older than the field. Free:
    /// the two `f32`s already forced a spare byte here.
    pub shape: u8,
}

impl RenderEntity {
    /// The tile this entity's own position sits in. Belts and pipes both work
    /// out their shape from what occupies the neighbouring tiles.
    pub fn tile(&self) -> (i32, i32) {
        (self.x.floor() as i32, self.y.floor() as i32)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderTile {
    pub x: i32,
    pub y: i32,
}

/// Granularity of the LOD pass: an `LOD_CELL_TILES`^2 square collapses to one
/// flat quad showing its dominant type. Independent of Factorio's 32-tile
/// chunk, which was only borrowed as a grid and is far too coarse: 1,024 tiles
/// per cell made a paved area with belts through it read as a grey block.
pub const LOD_CELL_TILES: i32 = 4;

/// Below this on-screen tile size, draw aggregated [`LodCell`]s instead of
/// items, a 1x1 entity being a fraction of a pixel by then. Comfortably below
/// `SPRITE_MIN_PIXELS`, so sprites are never in play at the same time.
///
/// A *tile's* pixel size, not a *chunk's*: gating on the chunk engages LOD 32x
/// further out, where a base at 0.32 px/tile measured 3.27M quads and 7 fps.
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
#[derive(Default)]
pub struct RenderFrame {
    pub tick: u64,
    pub count: usize,
    pub entities: Vec<RenderEntity>,
    pub entity_runs: Vec<Run>,
    pub tiles: Vec<RenderTile>,
    pub tile_runs: Vec<Run>,
    /// Chunk-level LOD, precomputed at load: at millions of tiles even a
    /// cheap per-item binning pass is too slow to redo 60 times a second.
    pub tile_lod: Vec<LodCell>,
    pub tile_lod_runs: Vec<Run>,
    pub entity_lod: Vec<LodCell>,
    pub entity_lod_runs: Vec<Run>,
    /// Corner-to-corner extent of this frame's tiles. Computed once, because
    /// the only consumer asks for it on a terrain layer of millions of tiles
    /// on every rendered frame: `draw_world` culls scenery against the ground
    /// so trees stop where the grass does.
    pub tile_bounds: Option<(Vec2, Vec2)>,
    /// This frame carried no floor because the surface's floor had not changed.
    /// `tiles` is empty and the previous frame's floor still stands.
    pub floor_unchanged: bool,
    /// `entities` and `tiles` are what arrived since the previous frame rather
    /// than everything standing, and the two removed lists are what left.
    pub delta: bool,
    pub removed_entities: Vec<u64>,
    pub removed_tiles: Vec<u64>,
}

/// Below this many items the grouping passes run single-threaded, thread-spawn
/// overhead dwarfing the work for an ordinary frame. Above it (a large
/// baseline or a terrain scan) they split across every core.
const PARALLEL_THRESHOLD: usize = 10_000;

fn worker_count() -> usize {
    std::thread::available_parallelism().map(std::num::NonZeroUsize::get).unwrap_or(1)
}

/// Group `items` by type into contiguous runs, by counting sort: O(n) with no
/// comparisons and no temporary `Vec<(TypeId, T)>`, which at megabase counts
/// is the difference between an allocation spike and none.
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

/// Same result as `group_by_type_sequential`, across every core once there is
/// enough work. Splits into contiguous chunks, runs the sequential algorithm
/// on each, and concatenates each type's slice in chunk order, which preserves
/// the same stable within-type ordering.
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
            .map(|(id_chunk, item_chunk)| scope.spawn(move || group_by_type_sequential(id_chunk, item_chunk, type_count)))
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

/// The per-item counting pass, split out so a large scan can run this half in
/// parallel. A per-chunk `Vec<(TypeId, count)>` rather than a dense
/// `type_count`-sized array: a chunk is realistically one or two types, and a
/// base has thousands of occupied chunks.
/// Per-chunk counts keyed by chunk coordinate.
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

/// Which single type stands for a cell: the most common, except that a
/// resource never speaks for a cell holding anything else. By count ore wins
/// easily, a 4x4 cell on a patch being sixteen ore against an outpost's two or
/// three entities. Same rule the full-detail draw order states in
/// `draw_world`. A cell of nothing but ore still reads as ore, which is why
/// this falls back rather than filtering.
fn dominant_type(counts: &[(TypeId, u32)], registry: &TypeRegistry) -> TypeId {
    let most_common = |resource: bool| {
        counts
            .iter()
            .filter(|&&(kind, _)| registry.is_resource(kind) == resource)
            .max_by_key(|&&(_, count)| count)
            .map(|&(kind, _)| kind)
    };
    most_common(false).or_else(|| most_common(true)).unwrap_or(0)
}

/// The other half of `build_chunk_lod`: picks the dominant type per chunk
/// from already-finished counts, then groups the resulting cells into
/// per-type runs the same way entities/tiles are.
fn finalize_chunk_lod(counts: ChunkCounts, type_count: usize, registry: &TypeRegistry) -> (Vec<LodCell>, Vec<Run>) {
    let mut cell_ids = Vec::with_capacity(counts.len());
    let mut cells = Vec::with_capacity(counts.len());
    for ((cx, cy), type_counts) in counts {
        let dominant = dominant_type(&type_counts, registry);
        cell_ids.push(dominant);
        cells.push(LodCell { cx, cy });
    }

    group_by_type(&cell_ids, &cells, type_count)
}

/// Aggregate items into one dominant type per chunk. `chunk_coords[i]` is the
/// chunk containing `ids[i]`.
///
/// Above `PARALLEL_THRESHOLD` each core counts a slice split by item index
/// rather than chunk coordinate, so the same `(cx, cy)` lands in more than one
/// slice. Partials merge by summing, which is commutative, so correctness
/// never depends on which thread saw which tiles.
fn build_chunk_lod(
    ids: &[TypeId],
    chunk_coords: &[(i32, i32)],
    type_count: usize,
    registry: &TypeRegistry,
) -> (Vec<LodCell>, Vec<Run>) {
    let workers = worker_count();
    if ids.len() < PARALLEL_THRESHOLD || workers <= 1 {
        return finalize_chunk_lod(chunk_lod_counts(ids, chunk_coords), type_count, registry);
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

    finalize_chunk_lod(merged, type_count, registry)
}

impl RenderFrame {
    /// How many of these somebody built, as opposed to `count`, which is
    /// everything the frame holds. Summed over runs rather than entities, a
    /// run knowing its own length, so this is tens of tests per frame rather
    /// than hundreds of thousands.
    pub fn building_count(&self, registry: &TypeRegistry) -> usize {
        self.entity_runs.iter().filter(|run| registry.is_built(run.type_id)).map(Run::len).sum()
    }

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
            // Only ever read off a terrain layer, and this buffer is the
            // per-tick one, so it never has ground to bound.
            tile_bounds: None,
            floor_unchanged: false,
            delta: false,
            removed_entities: Vec::new(),
            removed_tiles: Vec::new(),
        }
    }

    /// Consumes the parsed frame: keeping both representations alive would
    /// defeat the point, since the `frame::Frame` is the expensive one.
    pub fn from_frame(frame: Frame, registry: &mut TypeRegistry) -> RenderFrame {
        let entity_ids: Vec<TypeId> = frame.entities.iter().map(|e| registry.intern(&e.n)).collect();
        let mut entities: Vec<RenderEntity> = frame
            .entities
            .iter()
            .map(|e| RenderEntity {
                x: e.x,
                y: e.y,
                w: e.w.clamp(1, u8::MAX as u32) as u8,
                h: e.h.clamp(1, u8::MAX as u32) as u8,
                d: e.d,
                shape: 0,
            })
            .collect();

        // Before grouping, while entities are still in scan order and their ids
        // line up: a belt's shape depends on its neighbours, so this is the
        // only place with every belt in one list. Undergrounds share the
        // `shape` byte and nothing is both. This runs first because the far
        // end of a crossing feeds the belt in front of it and can bend it.
        let underground_kinds: Vec<Option<(TypeId, i32)>> =
            entity_ids.iter().map(|&id| registry.underground_reach(id).map(|reach| (id, reach))).collect();
        crate::belts::infer_underground_ends(&mut entities, &underground_kinds);

        let carriers: Vec<Option<crate::belts::Carrier>> = entity_ids
            .iter()
            .map(|&id| {
                if registry.is_belt(id) {
                    Some(crate::belts::Carrier::Belt)
                } else if registry.is_splitter(id) {
                    Some(crate::belts::Carrier::Splitter)
                } else if registry.underground_reach(id).is_some() {
                    Some(crate::belts::Carrier::Underground)
                } else {
                    None
                }
            })
            .collect();
        crate::belts::infer_shapes(&mut entities, &carriers);

        // Pipes reuse the same `shape` byte again, holding a four bit mask of
        // which sides join onto them. Nothing is both a pipe and a belt, so
        // the three meanings never collide on one entity.
        let pipe_flags: Vec<bool> = entity_ids.iter().map(|&id| registry.is_pipe(id)).collect();
        let to_ground_flags: Vec<bool> = entity_ids.iter().map(|&id| registry.is_pipe_to_ground(id)).collect();
        crate::pipes::infer_connections(&mut entities, &pipe_flags, &to_ground_flags);

        let tile_ids: Vec<TypeId> = frame.tiles.iter().map(|t| registry.intern(&t.n)).collect();
        let tiles: Vec<RenderTile> = frame.tiles.iter().map(|t| RenderTile { x: t.x, y: t.y }).collect();

        let type_count = registry.len();

        // Computed from the pre-grouped ids/positions, so this doesn't need
        // the full-detail grouping to have happened first.
        let entity_chunks: Vec<(i32, i32)> = entities.iter().map(|e| chunk_of(e.x.floor() as i32, e.y.floor() as i32)).collect();
        let (entity_lod, entity_lod_runs) = build_chunk_lod(&entity_ids, &entity_chunks, type_count, registry);

        let tile_chunks: Vec<(i32, i32)> = tiles.iter().map(|t| chunk_of(t.x, t.y)).collect();
        let (tile_lod, tile_lod_runs) = build_chunk_lod(&tile_ids, &tile_chunks, type_count, registry);

        let (entities, entity_runs) = group_by_type(&entity_ids, &entities, type_count);
        let (tiles, tile_runs) = group_by_type(&tile_ids, &tiles, type_count);

        // Corner to corner, so a tile at (x, y) contributes the whole square
        // it occupies rather than just its own corner: tiles are corner
        // anchored, unlike entities.
        let tile_bounds = tiles.iter().fold(None, |acc: Option<(Vec2, Vec2)>, t| {
            let (lo, hi) = (Vec2::new(t.x as f32, t.y as f32), Vec2::new(t.x as f32 + 1.0, t.y as f32 + 1.0));
            Some(match acc {
                None => (lo, hi),
                Some((min, max)) => (min.min(lo), max.max(hi)),
            })
        });

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
            tile_bounds,
            floor_unchanged: frame.floor_unchanged,
            delta: frame.delta,
            // Turned into span keys here, the same way the items themselves
            // are, so the builder compares like with like.
            removed_entities: frame.removed_entities.iter().map(|&(x, y)| span_key(x as f32 / 10.0, y as f32 / 10.0)).collect(),
            removed_tiles: frame.removed_tiles.iter().map(|&(x, y)| span_key(x as f32, y as f32)).collect(),
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

/// A loaded sequence of frames with a current position. Always non-empty.
///
/// Stores the run as spans rather than a materialized `RenderFrame` each,
/// consecutive frames being nearly identical, which is what put a ceiling on
/// capture length. The displayed frame is materialized only when it moves.
pub struct FrameSequence {
    entities: SpanSet<RenderEntity>,
    tiles: SpanSet<RenderTile>,
    entity_lod: SpanSet<LodCell>,
    tile_lod: SpanSet<LodCell>,
    /// Per frame, and tiny next to the item data, so these stay plain vecs.
    ticks: Vec<u64>,
    counts: Vec<usize>,
    /// Which frames the loader reconstructed rather than read (see
    /// `is_repeat`). A `Vec<bool>` rather than a bitset: one byte per frame
    /// against megabytes of item data is not where this pays attention.
    repeats: Vec<bool>,
    /// The frame at `index`, rebuilt by `goto`. Handed out by `current` so
    /// the renderer keeps taking an ordinary `&RenderFrame`.
    current: RenderFrame,
    index: usize,
}

impl FrameSequence {
    /// Folds `frames` into spans, dropping each as it goes. Takes them all at
    /// once for tests and callers that already have a vec; a loader wanting
    /// the memory win should fold frame by frame as it parses.
    pub fn new(frames: Vec<RenderFrame>, registry: &TypeRegistry) -> Option<Self> {
        let mut builder = SequenceBuilder::new();
        for frame in frames {
            builder.push(&frame);
        }
        builder.finish(registry)
    }

    /// Folds frames in one at a time, so a loader never has to hold more than
    /// the batch it is currently parsing.
    pub fn builder() -> SequenceBuilder {
        SequenceBuilder::new()
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

    /// Spans across all four layers, which is what this sequence's memory is
    /// proportional to: an item standing through a thousand frames is one span.
    /// Whether this frame was reconstructed rather than read, the export having
    /// omitted it because nothing on this surface changed.
    ///
    /// Carried forward rather than recomputed, being the premise the load-time
    /// passes short-circuit on: a frame identical to the one before cannot have
    /// new construction or extend a bounding box.
    pub fn is_repeat(&self, index: usize) -> bool {
        self.repeats.get(index).copied().unwrap_or(false)
    }

    /// The in-game tick of any frame, without materializing it: the timeline
    /// labels every position on the bar and needs nothing else about them.
    pub fn tick_at(&self, index: usize) -> Option<u64> {
        self.ticks.get(index).copied()
    }

    /// Walks every frame in order into a reused scratch buffer, for the
    /// load-time passes that need one look at the whole run. A callback rather
    /// than an iterator, each frame being a temporary with no storage to hand
    /// out a borrow of.
    ///
    /// A repeat frame is not re-materialized: the scratch buffer already holds
    /// the previous frame and a repeat is identical to it. The third argument
    /// says which kind it is, so a caller whose answer is also known in advance
    /// can skip its own work.
    pub fn for_each_frame(&self, mut visit: impl FnMut(usize, &RenderFrame, bool)) {
        let mut scratch = RenderFrame::empty();
        for index in 0..self.len() {
            let repeat = self.is_repeat(index);
            scratch.tick = self.ticks[index];
            scratch.count = self.counts[index];
            if !repeat {
                self.entities.materialize(index, &mut scratch.entities, &mut scratch.entity_runs);
                self.tiles.materialize(index, &mut scratch.tiles, &mut scratch.tile_runs);
                self.entity_lod.materialize(index, &mut scratch.entity_lod, &mut scratch.entity_lod_runs);
                self.tile_lod.materialize(index, &mut scratch.tile_lod, &mut scratch.tile_lod_runs);
            }
            visit(index, &scratch, repeat);
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Total spans across all four sets, for measuring what the layout costs.
    pub fn span_estimate(&self) -> usize {
        self.entities.span_count() + self.tiles.span_count() + self.entity_lod.span_count() + self.tile_lod.span_count()
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

/// Builds a [`FrameSequence`] frame by frame. `FrameSequence::new` taking a
/// `Vec<RenderFrame>` gives away the whole point of the span layout at the
/// moment it matters, every frame being alive at once just before folding.
/// Parsing in batches and pushing here keeps peak memory at one batch.
#[derive(Default)]
pub struct SequenceBuilder {
    entities: SpanBuilder<RenderEntity>,
    tiles: SpanBuilder<RenderTile>,
    ticks: Vec<u64>,
    counts: Vec<usize>,
    /// Which frames were reconstructed rather than read. Recorded here because
    /// this is the only place that knows: once a `FrameSequence` exists, a
    /// repeat is indistinguishable from a frame that happened to match.
    repeats: Vec<bool>,
}

impl SequenceBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the frame by reference and copies what it needs, so the caller
    /// can drop it immediately after.
    pub fn push(&mut self, frame: &RenderFrame) {
        self.ticks.push(frame.tick);
        // A delta says how many arrived, not how many are standing, so the
        // running total is carried rather than taken from the frame.
        //
        // Saturating on the total rather than clamping the removals against
        // the arrivals: a frame that mines more than it builds is the ordinary
        // case for a teardown, and clamping made the count stop falling
        // whenever it happened.
        let standing = match frame.delta {
            true => (self.counts.last().copied().unwrap_or(0) + frame.count).saturating_sub(frame.removed_entities.len()),
            false => frame.count,
        };
        self.counts.push(standing);
        self.repeats.push(false);

        if frame.delta {
            self.entities.push_delta(
                runs_with_items(&frame.entity_runs, &frame.entities, &|e: &RenderEntity| span_key(e.x, e.y)),
                frame.removed_entities.iter().copied(),
            );
            self.tiles.push_delta(
                runs_with_items(&frame.tile_runs, &frame.tiles, &|t: &RenderTile| span_key(t.x as f32, t.y as f32)),
                frame.removed_tiles.iter().copied(),
            );
            return;
        }

        self.entities.push_frame(runs_with_items(&frame.entity_runs, &frame.entities, &|e: &RenderEntity| span_key(e.x, e.y)));

        // A frame that says its floor is unchanged extends the spans already
        // open rather than folding the same millions of tiles in again, which
        // is the whole point: on a paved base the floor is most of a frame and
        // barely moves. One pass over open spans, not over tiles.
        //
        // Before the first frame carries a floor there is nothing to extend,
        // and `push_repeats` is a no-op then, which is correct: a surface whose
        // very first frame claims an unchanged floor has no floor.
        if frame.floor_unchanged {
            self.tiles.push_repeats(1);
            return;
        }
        self.tiles
            .push_frame(runs_with_items(&frame.tile_runs, &frame.tiles, &|t: &RenderTile| span_key(t.x as f32, t.y as f32)));
    }

    /// Repeats the frame just pushed at each of `ticks`, without that frame
    /// being read, parsed or folded in again.
    ///
    /// An export omits a surface's file when nothing on it changed, but the
    /// timeline is index-addressed and every surface has to agree on how many
    /// moments there were. Restoring costs one pass per gap rather than per
    /// frame. A no-op before any frame has been pushed.
    pub fn push_repeats(&mut self, ticks: &[u64]) {
        let Some(&count) = self.counts.last() else { return };
        if ticks.is_empty() {
            return;
        }
        self.ticks.extend_from_slice(ticks);
        self.counts.extend(std::iter::repeat_n(count, ticks.len()));
        self.repeats.extend(std::iter::repeat_n(true, ticks.len()));
        self.entities.push_repeats(ticks.len());
        self.tiles.push_repeats(ticks.len());
    }

    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ticks.is_empty()
    }

    /// `None` for a capture with no frames in it, matching `FrameSequence`'s
    /// promise of always being non-empty.
    pub fn finish(self, registry: &TypeRegistry) -> Option<FrameSequence> {
        if self.ticks.is_empty() {
            return None;
        }
        let frames = self.ticks.len();
        let entities = self.entities.finish();
        let tiles = self.tiles.finish();
        let entity_lod = derive_lod(&entities, |e: &RenderEntity| chunk_of(e.tile().0, e.tile().1), frames, registry);
        let tile_lod = derive_lod(&tiles, |t: &RenderTile| chunk_of(t.x, t.y), frames, registry);
        let mut sequence = FrameSequence {
            entities,
            tiles,
            entity_lod,
            tile_lod,
            ticks: self.ticks,
            counts: self.counts,
            repeats: self.repeats,
            current: RenderFrame::empty(),
            index: 0,
        };
        sequence.rebuild_current();
        Some(sequence)
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
    runs.iter().flat_map(move |run| items[run.range()].iter().map(move |item| (key(item), run.type_id, *item)))
}

/// When a cell's count for one type moves, and by how much. One entry per span
/// end, which is what makes deriving the aggregate cost spans rather than
/// frames times items.
type CellEvents = HashMap<(i32, i32), Vec<(u32, TypeId, i32)>>;

/// The aggregated layer for one item layer, derived from its spans rather than
/// maintained alongside them.
///
/// Maintaining it per frame cannot work with deltas: a cell shows its dominant
/// type, which depends on everything in it, not on what just changed. Deriving
/// is also cheaper than it was. Every span contributes `+1` to its cell's count
/// for its type over `[first, last)`, so one pass over the spans gives every
/// cell the frames at which its counts move, and the dominant type only has to
/// be recomputed at those. Cost is the number of spans, not frames times items.
fn derive_lod<T: Copy>(
    items: &SpanSet<T>,
    cell_of: impl Fn(&T) -> (i32, i32),
    frames: usize,
    registry: &TypeRegistry,
) -> SpanSet<LodCell> {
    // Per cell, when its count for a type changes and by how much.
    let mut events: CellEvents = HashMap::new();
    for span in items.iter() {
        let cell = cell_of(&span.item);
        let at = events.entry(cell).or_default();
        at.push((span.first, span.type_id, 1));
        at.push((span.last, span.type_id, -1));
    }

    let mut out: Vec<Span<LodCell>> = Vec::with_capacity(events.len());
    let mut counts: Vec<(TypeId, i32)> = Vec::new();
    for ((cx, cy), mut at) in events {
        at.sort_unstable_by_key(|&(frame, _, _)| frame);
        counts.clear();

        let (mut showing, mut since) = (None::<TypeId>, 0u32);
        let mut i = 0;
        while i < at.len() {
            let frame = at[i].0;
            while i < at.len() && at[i].0 == frame {
                let (_, type_id, delta) = at[i];
                match counts.iter_mut().find(|(t, _)| *t == type_id) {
                    Some((_, n)) => *n += delta,
                    None => counts.push((type_id, delta)),
                }
                i += 1;
            }

            let present: Vec<(TypeId, u32)> = counts.iter().filter(|&&(_, n)| n > 0).map(|&(t, n)| (t, n as u32)).collect();
            let now = (!present.is_empty()).then(|| dominant_type(&present, registry));
            if now != showing {
                if let Some(was) = showing {
                    out.push(Span { item: LodCell { cx, cy }, type_id: was, first: since, last: frame });
                }
                showing = now;
                since = frame;
            }
        }
        // Still showing something when the capture ended.
        if let Some(was) = showing {
            out.push(Span { item: LodCell { cx, cy }, type_id: was, first: since, last: frames as u32 });
        }
    }

    SpanSet::from_spans(out, frames)
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
        render(Frame {
            tick,
            surface: "nauvis".to_string(),
            count: 0,
            entities: Vec::new(),
            tiles: Vec::new(),
            floor_unchanged: false,
            ..Default::default()
        })
    }

    /// The on-screen count is what somebody built, not what the frame holds.
    /// Measured on a real capture, one frame held 13,492 entities of which
    /// 1,174 were buildings: trees alone were 9,793 of the rest.
    #[test]
    fn a_frame_counts_buildings_apart_from_what_the_map_generated() {
        let mut registry = TypeRegistry::new();
        registry.set_prototypes(save_timelapse::prototypes::Prototypes {
            types: [
                ("assembling-machine-2", "assembling-machine"),
                ("transport-belt", "transport-belt"),
                ("gun-turret", "ammo-turret"),
                ("tree-grassland-k", "tree"),
                ("iron-ore", "resource"),
                ("cliff", "cliff"),
                ("biter-spawner", "unit-spawner"),
                ("small-worm-turret", "turret"),
                ("locomotive", "locomotive"),
            ]
            .iter()
            .map(|(n, k)| (n.to_string(), k.to_string()))
            .collect(),
            ..Default::default()
        });

        let built = ["assembling-machine-2", "transport-belt", "gun-turret"];
        let scenery = ["tree-grassland-k", "iron-ore", "cliff", "biter-spawner", "small-worm-turret", "locomotive"];
        let entities: Vec<Entity> = built.iter().chain(&scenery).enumerate().map(|(i, n)| entity(n, i as f32, 0.0)).collect();

        let frame = RenderFrame::from_frame(
            Frame {
                tick: 1,
                surface: "nauvis".to_string(),
                count: entities.len(),
                entities,
                tiles: Vec::new(),
                floor_unchanged: false,
                ..Default::default()
            },
            &mut registry,
        );

        assert_eq!(frame.count, 9, "the frame holds everything");
        assert_eq!(frame.building_count(&registry), 3, "only what somebody placed is a building");
    }

    /// The running count a delta carries has to follow what is standing, and
    /// what is standing goes down as well as up. Clearing a base mines far
    /// more than it builds, which is exactly where the count used to stick:
    /// removals were clamped against arrivals, so a frame that added one and
    /// mined ninety reported no change at all.
    #[test]
    fn a_delta_that_mines_more_than_it_builds_still_counts_down() {
        let mut registry = TypeRegistry::new();

        let standing: Vec<Entity> = (0..100).map(|i| entity("pipe", i as f32 + 0.5, 0.5)).collect();
        let full =
            Frame { tick: 0, surface: "nauvis".to_string(), count: standing.len(), entities: standing, ..Default::default() };
        // Ninety mined and one built, at the tenth-of-a-tile scale removals
        // are recorded in.
        let torn_down = Frame {
            tick: 1,
            surface: "nauvis".to_string(),
            count: 1,
            entities: vec![entity("pipe", 500.5, 0.5)],
            delta: true,
            removed_entities: (0..90).map(|i| (i * 10 + 5, 5)).collect(),
            ..Default::default()
        };

        let mut builder = FrameSequence::builder();
        builder.push(&RenderFrame::from_frame(full, &mut registry));
        builder.push(&RenderFrame::from_frame(torn_down, &mut registry));
        let mut sequence = builder.finish(&registry).expect("frames");

        sequence.goto(1);
        let frame = sequence.current();
        assert_eq!(frame.entities.len(), 11, "eleven really are left");
        assert_eq!(frame.count, frame.entities.len(), "and the count has to say so");
    }

    /// What aggregating the ground is worth, over a real terrain layer rather
    /// than a fixture: the answer is a property of how a map generated, not of
    /// the code, so no synthetic input can stand in for it.
    ///
    /// Ignored, and takes the path from the environment so no local one is
    /// committed.
    ///
    /// ```text
    /// SAVE_TIMELAPSE_TERRAIN='<...>/timelapses/<name>/terrain_nauvis.stfr'     ///   cargo test --release -p viewer --lib measure_terrain_lod -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_terrain_lod_on_a_real_ground_layer() {
        let path = std::env::var("SAVE_TIMELAPSE_TERRAIN")
            .expect("set SAVE_TIMELAPSE_TERRAIN to a terrain_<surface>.stfr from a built timelapse");
        let bytes = std::fs::read(&path).expect("reading the terrain layer");
        let frame = save_timelapse::frame::read_binary(&bytes).expect("parsing the terrain layer");

        let tiles = frame.tiles.len();
        let mut registry = TypeRegistry::new();
        let rendered = RenderFrame::from_frame(frame, &mut registry);
        let cells = rendered.tile_lod.len();

        println!(
            "
{path}"
        );
        println!("  tiles      {tiles:>12}");
        println!("  LOD cells  {cells:>12}");
        println!("  reduction  {:>11.1}x", tiles as f64 / cells.max(1) as f64);

        assert!(cells > 0, "a ground layer must produce cells");
        assert!(cells * 4 < tiles, "aggregating ground has to be worth doing: {cells} cells from {tiles} tiles");
    }

    /// The whole scheme in one check: a timelapse built from deltas has to
    /// show, frame for frame, exactly what one built from snapshots shows.
    ///
    /// Driven through the real world model and the real wire format rather
    /// than hand-built frames, so it covers what `to_frame_delta` decides,
    /// what the encoder writes, what the reader gets back and what the span
    /// builder does with it.
    #[test]
    fn a_timelapse_built_from_deltas_shows_the_same_thing_as_one_built_from_snapshots() {
        use save_timelapse::event::Event;
        use save_timelapse::world::World;

        let add = |name: &str, x: f32, y: f32, id: u64| Event::AddEntity {
            name: name.to_string(),
            x,
            y,
            d: 0,
            w: 1,
            h: 1,
            id: Some(id),
        };
        // Build, pave, rotate, mine, repave: every shape of change a frame can
        // carry, including a position that is emptied and one reused.
        let steps: Vec<Vec<Event>> = vec![
            vec![add("pipe", 1.5, 1.5, 1), add("pipe", 2.5, 1.5, 2)],
            vec![
                Event::AddTile { name: "concrete".to_string(), x: 0, y: 0 },
                Event::AddTile { name: "concrete".to_string(), x: 1, y: 0 },
            ],
            vec![Event::RemoveEntity { id: Some(1), pos: (1.5, 1.5), name: None }],
            vec![add("transport-belt", 1.5, 1.5, 3), Event::RemoveTile { x: 0, y: 0 }],
            vec![Event::AddTile { name: "stone-path".to_string(), x: 1, y: 0 }],
            vec![],
        ];

        let materialise = |deltas: bool| {
            let mut world = World::new();
            world.load_baseline(&save_timelapse::frame::Frame {
                tick: 0,
                surface: "nauvis".to_string(),
                count: 1,
                entities: vec![Entity { n: "inserter".into(), x: 9.5, y: 9.5, d: 0, w: 1, h: 1 }],
                tiles: vec![Tile { n: "concrete".into(), x: 9, y: 9 }],
                ..Default::default()
            });

            let mut registry = TypeRegistry::new();
            let mut builder = FrameSequence::builder();
            for (i, events) in steps.iter().enumerate() {
                for event in events {
                    world.apply(Some("nauvis"), event);
                }
                let frame = match deltas && i > 0 {
                    true => world.to_frame_delta("nauvis", i as u64),
                    false => {
                        world.clear_changes("nauvis");
                        world.to_frame("nauvis", i as u64)
                    }
                };
                // Through the wire format, so the encoder and reader are in the
                // loop rather than assumed.
                let bytes = save_timelapse::frame::write_binary(&frame.as_out());
                let read = save_timelapse::frame::read_binary(&bytes).expect("a frame must read back");
                builder.push(&RenderFrame::from_frame(read, &mut registry));
            }
            let mut sequence = builder.finish(&registry).expect("frames");

            let mut shown = Vec::new();
            for i in 0..steps.len() {
                sequence.goto(i);
                let frame = sequence.current();
                let mut entities: Vec<(i32, i32)> =
                    frame.entities.iter().map(|e| ((e.x * 10.0) as i32, (e.y * 10.0) as i32)).collect();
                let mut tiles: Vec<(i32, i32)> = frame.tiles.iter().map(|t| (t.x, t.y)).collect();
                entities.sort();
                tiles.sort();
                shown.push((entities, tiles));
            }
            shown
        };

        let snapshots = materialise(false);
        let deltas = materialise(true);
        for (i, (a, b)) in snapshots.iter().zip(&deltas).enumerate() {
            assert_eq!(a, b, "frame {i} differs between a snapshot build and a delta build");
        }
    }

    /// The saving is only real if the floor is still there. A frame that leaves
    /// its floor out must show exactly the same floor as the frame before it,
    /// and must cost one pass over open spans rather than a walk over millions
    /// of tiles.
    #[test]
    fn a_frame_that_omits_its_floor_still_shows_it() {
        let mut registry = TypeRegistry::new();
        let floor: Vec<Tile> = (0..50).map(|i| Tile { n: "concrete".into(), x: i, y: 0 }).collect();

        let carried = RenderFrame::from_frame(
            Frame {
                tick: 10,
                surface: "nauvis".to_string(),
                count: 1,
                entities: vec![entity("pipe", 1.0, 1.0)],
                tiles: floor,
                floor_unchanged: false,
                ..Default::default()
            },
            &mut registry,
        );
        // What the writer produces for an unchanged floor: no tiles, and the
        // flag saying the previous one still stands.
        let omitted = RenderFrame::from_frame(
            Frame {
                tick: 20,
                surface: "nauvis".to_string(),
                count: 2,
                entities: vec![entity("pipe", 1.0, 1.0), entity("pipe", 2.0, 2.0)],
                tiles: Vec::new(),
                floor_unchanged: true,
                ..Default::default()
            },
            &mut registry,
        );

        let mut builder = FrameSequence::builder();
        builder.push(&carried);
        builder.push(&omitted);
        let mut sequence = builder.finish(&registry).expect("two frames");

        sequence.goto(0);
        let first: Vec<RenderTile> = sequence.current().tiles.clone();
        sequence.goto(1);
        let second: Vec<RenderTile> = sequence.current().tiles.clone();

        assert_eq!(first.len(), 50, "the frame that carried the floor shows it");
        assert_eq!(second, first, "and the frame that left it out shows exactly the same floor");
        assert_eq!(sequence.current().entities.len(), 2, "while its own entities are its own");
    }

    /// The equivalence the skip-unchanged-frames scheme rests on: the restored
    /// sequence must be indistinguishable from what an export writing every
    /// frame would have produced, index for index. Otherwise the saving is
    /// paid for with a subtly different timelapse.
    #[test]
    fn repeats_produce_the_same_sequence_as_writing_every_frame() {
        // Frame contents at each moment: one entity until tick 40, two after.
        let at = |tick: u64, wide: bool| {
            let mut entities = vec![entity("pipe", 1.0, 2.0)];
            if wide {
                entities.push(entity("belt", 3.0, 2.0));
            }
            Frame {
                tick,
                surface: "nauvis".to_string(),
                count: entities.len(),
                entities,
                tiles: Vec::new(),
                floor_unchanged: false,
                ..Default::default()
            }
        };
        let ticks = [10u64, 20, 30, 40];

        // What an export that wrote every surface every frame produced.
        let mut every = FrameSequence::builder();
        for &tick in &ticks {
            every.push(&render(at(tick, tick >= 40)));
        }
        let every = every.finish(&TypeRegistry::new()).unwrap();

        // What an export that skips unchanged surfaces produces: files only
        // at ticks 10 and 40, with 20 and 30 restored by the loader.
        let mut skipped = FrameSequence::builder();
        skipped.push(&render(at(10, false)));
        skipped.push_repeats(&[20, 30]);
        skipped.push(&render(at(40, true)));
        let mut skipped = skipped.finish(&TypeRegistry::new()).unwrap();
        let mut every = every;

        assert_eq!(skipped.len(), every.len(), "same number of moments");
        for index in 0..every.len() {
            every.goto(index);
            skipped.goto(index);
            let (a, b) = (every.current(), skipped.current());
            assert_eq!(a.tick, b.tick, "frame {index}: tick");
            assert_eq!(a.count, b.count, "frame {index}: count");
            assert_eq!(a.entities, b.entities, "frame {index}: entities");
            assert_eq!(a.entity_runs, b.entity_runs, "frame {index}: runs");
        }
    }

    /// A gap that runs to the end of the capture, which is the common case:
    /// a surface stops changing and the playthrough carries on elsewhere.
    #[test]
    fn trailing_repeats_keep_a_surface_present_for_the_rest_of_the_capture() {
        let mut builder = FrameSequence::builder();
        builder.push(&sample_frame(10));
        builder.push_repeats(&[20, 30, 40]);
        let sequence = builder.finish(&TypeRegistry::new()).unwrap();

        assert_eq!(sequence.len(), 4);
        assert_eq!(sequence.tick_at(3), Some(40), "the last moment is the timeline's, not the surface's");
    }

    /// Nothing to repeat before a first frame exists. A surface's own first
    /// frame is always written, so this is a guard rather than a real case.
    #[test]
    fn repeats_before_any_frame_are_ignored() {
        let mut builder = FrameSequence::builder();
        builder.push_repeats(&[1, 2, 3]);
        assert!(builder.is_empty());
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
            floor_unchanged: false,
            ..Default::default()
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
            floor_unchanged: false,
            ..Default::default()
        };
        let rendered = RenderFrame::from_frame(frame, &mut registry);
        assert_eq!(rendered.tile_runs.len(), 2);

        let concrete = registry.intern("concrete");
        let run = rendered.tile_runs.iter().find(|r| r.type_id == concrete).unwrap();
        let mut coords: Vec<(i32, i32)> = rendered.tiles[run.range()].iter().map(|t| (t.x, t.y)).collect();
        coords.sort();
        assert_eq!(coords, vec![(-6, 3), (-5, 1)]);
    }

    /// The tests above all stay under `PARALLEL_THRESHOLD`, so this is the one
    /// that exercises the parallel path, against the sequential result.
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

    /// Same for `build_chunk_lod`, with chunk coordinates cycled rather than
    /// clustered so more than one worker sees the same coordinate, exercising
    /// the summed merge. Compared by content, `finalize_chunk_lod` iterating a
    /// `HashMap`.
    /// A mining outpost stands in an ore patch, so by count the ore wins.
    #[test]
    fn ore_never_speaks_for_a_cell_that_holds_something_built() {
        let mut registry = TypeRegistry::new();
        let ore = registry.intern("iron-ore");
        let drill = registry.intern("electric-mining-drill");

        // Fifteen tiles of ore against a single drill, in one cell.
        let mut ids = vec![ore; 15];
        ids.push(drill);
        let coords = vec![(0, 0); ids.len()];

        let (cells, runs) = build_chunk_lod(&ids, &coords, registry.len(), &registry);
        assert_eq!(cells.len(), 1, "one cell");
        assert_eq!(runs[0].type_id, drill, "the drill speaks for it, outnumbered fifteen to one");

        // ...but a cell with nothing built in it is still ore, which is why
        // this prefers rather than filters.
        let bare = vec![ore; 4];
        let (_, runs) = build_chunk_lod(&bare, &[(0, 0); 4], registry.len(), &registry);
        assert_eq!(runs[0].type_id, ore);
    }

    #[test]
    fn build_chunk_lod_parallel_path_matches_sequential_at_scale() {
        let type_count = 4;
        let n = PARALLEL_THRESHOLD * 3;
        let ids: Vec<TypeId> = (0..n).map(|i| (i % type_count) as TypeId).collect();
        let chunk_coords: Vec<(i32, i32)> = (0..n).map(|i| ((i % 3) as i32, 0)).collect();

        let mut registry = TypeRegistry::new();
        for i in 0..type_count {
            registry.intern(&format!("type-{i}"));
        }

        let (parallel_cells, parallel_runs) = build_chunk_lod(&ids, &chunk_coords, type_count, &registry);
        let (sequential_cells, sequential_runs) =
            finalize_chunk_lod(chunk_lod_counts(&ids, &chunk_coords), type_count, &registry);

        let content = |cells: &[LodCell], runs: &[Run]| {
            let mut pairs: Vec<(TypeId, i32, i32)> =
                runs.iter().flat_map(|r| cells[r.range()].iter().map(move |c| (r.type_id, c.cx, c.cy))).collect();
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
            floor_unchanged: false,
            ..Default::default()
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

    /// Regression: gating on the chunk's on-screen size rather than the
    /// tile's engaged LOD 32x too far out. 0.32 px/tile is a real measurement
    /// from a base that stayed in full detail at 3.27M quads and 7 fps.
    #[test]
    fn use_chunk_lod_engages_well_before_a_tile_is_sub_pixel() {
        assert!(use_chunk_lod(0.32));
    }

    #[test]
    fn chunk_lod_aggregates_a_dense_area_into_one_cell_per_chunk() {
        let mut registry = TypeRegistry::new();
        // A strip spanning one chunk plus one tile, so this must produce two
        // cells. The spill stays under one cell's width, or it would land in
        // three chunks regardless of LOD_CELL_TILES.
        let tiles: Vec<Tile> = (0..LOD_CELL_TILES + 1).map(|x| Tile { n: "concrete".into(), x, y: 0 }).collect();
        let frame = Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 0,
            entities: Vec::new(),
            tiles,
            floor_unchanged: false,
            ..Default::default()
        };
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
        let frame = Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 0,
            entities: Vec::new(),
            tiles,
            floor_unchanged: false,
            ..Default::default()
        };
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
            // -0.5 floors to -1, exercising div_euclid rather than integer
            // division, which would floor a negative toward zero.
            entities: vec![entity("pipe", -0.5, -0.5), entity("pipe", (LOD_CELL_TILES - 1) as f32 + 0.5, 0.5)],
            tiles: Vec::new(),
            floor_unchanged: false,
            ..Default::default()
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
        assert!(FrameSequence::new(Vec::new(), &TypeRegistry::new()).is_none());
    }

    #[test]
    fn frame_sequence_stepping_clamps_at_both_ends() {
        let mut seq = FrameSequence::new(vec![sample_frame(0), sample_frame(1), sample_frame(2)], &TypeRegistry::new()).unwrap();
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
