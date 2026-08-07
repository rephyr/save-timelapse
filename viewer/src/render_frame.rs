//! The renderer-facing frame representation: items grouped into contiguous
//! per-type runs with names interned away, plus the chunk-level
//! level-of-detail data computed alongside it, and a sequence of these to
//! scrub through.

use std::collections::HashMap;

use macroquad::math::Vec2;
use save_timelapse::frame::Frame;

use crate::registry::{TypeId, TypeRegistry};

/// A contiguous span of one type within a [`RenderFrame`]'s entity or tile
/// array. Draws iterate runs rather than individual items, so the texture is
/// bound once per type instead of being re-decided per entity -- which is
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
/// a `frame::Entity` -- 48 for the struct plus a heap allocation for a name
/// that was one of a few dozen repeated strings.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderEntity {
    pub x: f32,
    pub y: f32,
    /// Tile footprint, saturated into a byte: the format allows u32, but
    /// Factorio's largest prototypes are far under 255 tiles across.
    pub w: u8,
    pub h: u8,
    /// Unused by the current flat-sprite drawing, kept because it costs
    /// nothing here (it lands in existing padding) and rotation-aware
    /// sprites would otherwise need a reload to recover it.
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
/// equal to it -- that was borrowed as a convenient existing grid, not
/// because rendering needs to align with it. 32 turned out too coarse:
/// aggregating 1,024 tiles into one dominant-type color loses so much that a
/// paved area with belts and machines running through it just reads as a
/// solid gray block. Smaller cells keep more structure recognizable, at the
/// cost of more (but still vastly fewer than full-detail) quads submitted --
/// halved again from 8 to 4 once 8 measured with FPS to spare.
pub const LOD_CELL_TILES: i32 = 4;

/// Below this on-screen tile size (in pixels), draw chunk-aggregated
/// [`LodCell`]s instead of individual items. At this scale a 1x1 entity
/// spans a fraction of a pixel anyway, so individual items are already
/// imperceptible -- the only question is whether the CPU still pays to
/// transform and submit each one. Comfortably below `SPRITE_MIN_PIXELS`, so
/// sprites are never in play once LOD is.
///
/// A *tile's* pixel size, not a *chunk's*: a chunk is 32 tiles across, so
/// gating on the chunk's on-screen size instead would only trigger LOD 32x
/// later than intended -- a real base at the zoom level that motivated this
/// (0.32 px/tile, individual tiles long since imperceptible) measured 3.27M
/// quads submitted and 7 fps with that version of the check, because a
/// 10px chunk still looked "big enough" even though every tile inside it was
/// a third of a pixel.
pub const LOD_MAX_TILE_PIXELS: f32 = 2.0;

pub fn use_chunk_lod(pixels_per_tile: f32) -> bool {
    pixels_per_tile <= LOD_MAX_TILE_PIXELS
}

/// One `LOD_CELL_TILES`-square chunk of the world, drawn as a single
/// flat-colored quad. Only ever produced for the level-of-detail pass --
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

/// Group `items` by type into contiguous runs, by counting sort.
///
/// Counting sort rather than `sort_by_key` because this is O(n) with one
/// pass to count and one to scatter, needs no comparisons, and avoids the
/// temporary `Vec<(TypeId, T)>` that sorting in place would require -- which
/// at megabase entity counts is the difference between a brief allocation
/// spike and none.
fn group_by_type<T: Copy + Default>(ids: &[TypeId], items: &[T], type_count: usize) -> (Vec<T>, Vec<Run>) {
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

/// Aggregate items into one dominant type per `LOD_CELL_TILES`-square chunk,
/// for the level-of-detail pass. `chunk_coords[i]` is the chunk containing
/// `ids[i]`/the item at the same index in whatever array these came from.
///
/// A per-chunk `Vec<(TypeId, count)>` rather than a `type_count`-sized array
/// per chunk: a chunk of floor tiles is realistically one or two types, and
/// a real base can have thousands of occupied chunks, so a dense per-chunk
/// array (tens of entries, nearly all zero) would waste far more than the
/// linear scan through a handful of real entries costs.
fn build_chunk_lod(ids: &[TypeId], chunk_coords: &[(i32, i32)], type_count: usize) -> (Vec<LodCell>, Vec<Run>) {
    let mut counts: HashMap<(i32, i32), Vec<(TypeId, u32)>> = HashMap::new();
    for (&coord, &id) in chunk_coords.iter().zip(ids) {
        let entry = counts.entry(coord).or_default();
        match entry.iter_mut().find(|(t, _)| *t == id) {
            Some((_, count)) => *count += 1,
            None => entry.push((id, 1)),
        }
    }

    let mut cell_ids = Vec::with_capacity(counts.len());
    let mut cells = Vec::with_capacity(counts.len());
    for ((cx, cy), type_counts) in counts {
        let dominant = type_counts.into_iter().max_by_key(|&(_, count)| count).map(|(t, _)| t).unwrap_or(0);
        cell_ids.push(dominant);
        cells.push(LodCell { cx, cy });
    }

    group_by_type(&cell_ids, &cells, type_count)
}

impl RenderFrame {
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

/// A loaded sequence of frames with a current position. Always non-empty:
/// construction from zero frames is rejected rather than leaving every
/// accessor to guard against it.
pub struct FrameSequence {
    frames: Vec<RenderFrame>,
    index: usize,
}

impl FrameSequence {
    pub fn new(frames: Vec<RenderFrame>) -> Option<Self> {
        if frames.is_empty() {
            return None;
        }
        Some(Self { frames, index: 0 })
    }

    pub fn current(&self) -> &RenderFrame {
        &self.frames[self.index]
    }

    pub fn frames(&self) -> &[RenderFrame] {
        &self.frames
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Clamps at the sequence's ends rather than wrapping.
    pub fn goto(&mut self, index: usize) {
        self.index = index.min(self.frames.len() - 1);
    }

    pub fn step_forward(&mut self) {
        self.goto(self.index + 1);
    }

    pub fn step_back(&mut self) {
        self.goto(self.index.saturating_sub(1));
    }
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
            // -0.5 floors to -1, landing in the chunk to the left of origin
            // -- exercises div_euclid rather than plain integer division,
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
