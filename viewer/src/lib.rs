//! Everything that doesn't touch macroquad's window/input globals, split out
//! so it's unit testable: `main.rs` is thin glue over this.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use macroquad::color::Color;
use macroquad::math::{Rect, Vec2};
use save_timelapse::frame::{Entity, Frame, Tile};

pub const BASE_PIXELS_PER_TILE: f32 = 32.0;

/// Dense index into [`TypeRegistry`]. A real base has tens of distinct
/// prototype names against hundreds of thousands of entities, so the name is
/// worth storing once and referring to by number everywhere else.
pub type TypeId = u16;

/// Interns entity/tile prototype names, and resolves each one's color once.
///
/// The pre-registry renderer called `color_for` (an FNV hash over the name)
/// and `sprites.get(&e.n)` (a SipHash over the name) for every entity on
/// every rendered frame. Both are pure functions of the name, and there are
/// only ~58 distinct names in the real fixtures, so both collapse into an
/// array index once names are interned.
#[derive(Default)]
pub struct TypeRegistry {
    names: Vec<String>,
    ids: HashMap<String, TypeId>,
    entity_colors: Vec<Color>,
    tile_colors: Vec<Color>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Both color variants are precomputed rather than one per registered
    /// kind: a name is realistically either an entity or a tile type, but
    /// nothing in the format guarantees it, and two `Color`s per *type* is
    /// negligible next to one hash per *entity per frame*.
    pub fn intern(&mut self, name: &str) -> TypeId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = TypeId::try_from(self.names.len()).expect("more than u16::MAX distinct type names");
        self.names.push(name.to_string());
        self.entity_colors.push(color_for(name, 0.55, 0.85));
        self.tile_colors.push(color_for(name, 0.35, 0.5));
        self.ids.insert(name.to_string(), id);
        id
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn name(&self, id: TypeId) -> &str {
        &self.names[id as usize]
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn entity_color(&self, id: TypeId) -> Color {
        self.entity_colors[id as usize]
    }

    pub fn tile_color(&self, id: TypeId) -> Color {
        self.tile_colors[id as usize]
    }
}

/// A contiguous span of one type within a [`RenderFrame`]'s entity or tile
/// array. Draws iterate runs rather than individual items, so the texture is
/// bound once per type instead of being re-decided per entity -- which is
/// what keeps macroquad from breaking the batch. See [`DrawCallCounter`].
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

/// Mirrors macroquad 0.4's batching rule so the viewer can report the draw
/// calls it actually costs.
///
/// `quad_gl.rs::geometry` merges new geometry only into the *immediately
/// preceding* draw call, and starts a fresh one when the bound texture
/// differs or the index buffer would overflow. macroquad's own
/// `telemetry::drawcalls` can't be used for a running count: `track_drawcall`
/// allocates a 128x128 render texture per call, so counting thousands of them
/// would cost more than the thing being measured.
pub struct DrawCallCounter {
    max_indices: usize,
    current: Option<TypeId>,
    started: bool,
    indices: usize,
    pub calls: usize,
    pub quads: usize,
}

impl DrawCallCounter {
    pub const INDICES_PER_QUAD: usize = 6;

    pub fn new(max_indices: usize) -> Self {
        DrawCallCounter {
            max_indices,
            current: None,
            started: false,
            indices: 0,
            calls: 0,
            quads: 0,
        }
    }

    pub fn reset(&mut self) {
        self.current = None;
        self.started = false;
        self.indices = 0;
        self.calls = 0;
        self.quads = 0;
    }

    /// Record one quad bound to `texture` (`None` for an untextured rect,
    /// which macroquad treats as its own distinct texture state).
    pub fn quad(&mut self, texture: Option<TypeId>) {
        let would_overflow = self.indices + Self::INDICES_PER_QUAD > self.max_indices;
        if !self.started || self.current != texture || would_overflow {
            self.calls += 1;
            self.indices = 0;
            self.current = texture;
            self.started = true;
        }
        self.indices += Self::INDICES_PER_QUAD;
        self.quads += 1;
    }

    /// Record `n` consecutive quads sharing one texture -- the batched case,
    /// without looping per quad.
    pub fn quads(&mut self, texture: Option<TypeId>, n: usize) {
        if n == 0 {
            return;
        }
        self.quad(texture);
        let remaining = n - 1;
        let per_call = self.max_indices / Self::INDICES_PER_QUAD;
        let room = per_call - self.indices / Self::INDICES_PER_QUAD;
        let after_fill = remaining.saturating_sub(room);
        self.calls += after_fill.div_ceil(per_call);
        self.indices = if after_fill == 0 {
            self.indices + remaining * Self::INDICES_PER_QUAD
        } else {
            let tail = after_fill % per_call;
            let quads_in_last = if tail == 0 { per_call } else { tail };
            quads_in_last * Self::INDICES_PER_QUAD
        };
        self.quads += remaining;
    }
}

/// Progress through the startup load, for the on-screen bar.
///
/// Loading is blocking work in front of a window that is already open, so
/// without this the viewer shows an empty frame for as long as it takes to
/// parse every frame file and load every sprite -- which on a real save set
/// is many seconds with no indication anything is happening.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadProgress {
    pub phase: &'static str,
    pub detail: String,
    pub done: usize,
    pub total: usize,
}

impl LoadProgress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
    }
}

/// Geometry for the loading bar. Split from drawing for the same reason as
/// [`Timeline`]: this part is testable, macroquad calls are not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgressBar {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl ProgressBar {
    pub const HEIGHT: f32 = 18.0;

    pub fn centered(screen_width: f32, screen_height: f32) -> Self {
        let width = (screen_width * 0.5).max(1.0);
        ProgressBar {
            left: (screen_width - width) / 2.0,
            top: screen_height / 2.0 - Self::HEIGHT / 2.0,
            width,
            height: Self::HEIGHT,
        }
    }

    pub fn filled_width(&self, progress: &LoadProgress) -> f32 {
        self.width * progress.fraction()
    }
}

#[derive(Clone, Copy)]
pub struct Camera {
    pub offset: Vec2,
    pub zoom: f32,
}

impl Camera {
    pub fn pixels_per_tile(&self) -> f32 {
        BASE_PIXELS_PER_TILE * self.zoom
    }

    pub fn world_to_screen(&self, world: Vec2, screen_center: Vec2) -> Vec2 {
        screen_center + (world - self.offset) * self.pixels_per_tile()
    }

    pub fn screen_to_world(&self, screen: Vec2, screen_center: Vec2) -> Vec2 {
        self.offset + (screen - screen_center) / self.pixels_per_tile()
    }

    /// Center on the bounding box of every entity and tile across every
    /// frame given, and pick a zoom that fits it on screen. Fitting the
    /// whole sequence rather than just the current frame means scrubbing
    /// through a growing base doesn't jump-recenter every step. Real bases
    /// are almost never near world origin, so an empty/degenerate input
    /// falls back to a sane default rather than opening on empty space.
    pub fn fit_frames(frames: &[RenderFrame], screen_width: f32, screen_height: f32) -> Camera {
        // Each entity contributes its two footprint corners, not just its
        // center point -- for a small cluster of large buildings, ignoring
        // footprint here would zoom in as if they were 1x1, and a real
        // multi-tile entity would then render bigger than the whole window.
        let mut points = frames.iter().flat_map(|f| {
            let entity_corners = f.entities.iter().flat_map(|e| {
                let half = Vec2::new(e.w as f32, e.h as f32) / 2.0;
                let center = Vec2::new(e.x, e.y);
                [center - half, center + half]
            });
            let tile_points = f.tiles.iter().map(|t| Vec2::new(t.x as f32, t.y as f32));
            entity_corners.chain(tile_points)
        });
        let Some(first) = points.next() else {
            return Camera { offset: Vec2::ZERO, zoom: 1.0 };
        };
        let mut min = first;
        let mut max = first;
        for p in points {
            min = min.min(p);
            max = max.max(p);
        }
        let center = (min + max) / 2.0;
        let size = (max - min).max(Vec2::splat(1.0));
        let zoom = (screen_width / (size.x * BASE_PIXELS_PER_TILE))
            .min(screen_height / (size.y * BASE_PIXELS_PER_TILE))
            * 0.9;
        Camera { offset: center, zoom: zoom.clamp(0.01, 50.0) }
    }
}

/// The on-screen size of an entity's footprint, in pixels. Most entities are
/// 1x1, but assemblers, furnaces and the like span several tiles -- sizing
/// every entity to a fixed 1-tile square (the pre-footprint behavior) is why
/// multi-tile buildings used to render undersized and visually disconnected
/// from whatever was actually touching them.
pub fn entity_footprint_size(pixels_per_tile: f32, w: u32, h: u32) -> Vec2 {
    Vec2::new((w as f32 * pixels_per_tile).max(1.0), (h as f32 * pixels_per_tile).max(1.0))
}

/// Vanilla/Space-Age icons follow a predictable path under the Factorio
/// install's data directory, keyed by prototype name -- verified against
/// Wube's own base-game source (e.g. `__base__/graphics/icons/stone-furnace.png`
/// resolves to `<data_dir>/base/graphics/icons/stone-furnace.png`). There's no
/// runtime API a mod could use to export the exact path (checked before
/// building this), and no reliable convention for third-party mod icons --
/// a lookup miss just means falling back to a colored shape, never an error.
pub fn icon_candidates(data_dir: &Path, name: &str) -> Vec<PathBuf> {
    ["base", "space-age"]
        .iter()
        .map(|group| data_dir.join(group).join("graphics/icons").join(format!("{name}.png")))
        .collect()
}

pub fn icon_path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    icon_candidates(data_dir, name).into_iter().find(|candidate| candidate.exists())
}

/// Vanilla and Space Age icon files are a horizontal mipmap strip, not a
/// single image: the primary icon at full size, then progressively smaller
/// copies laid out to its right, all sharing the strip's height (verified
/// against real files -- e.g. 64+32+16+8=120 wide, 64 tall). Drawing the
/// whole file stretched into one entity's box renders all of them squashed
/// together, which is what "sprites have 4 sprites in them" was.
///
/// The primary icon is always the leftmost square, sized to the image's
/// height. A single-icon file with no strip (width == height) falls out of
/// the same rule as a no-op crop, so callers don't need to special-case it.
pub fn icon_source_rect(width: f32, height: f32) -> Rect {
    let size = width.min(height);
    Rect::new(0.0, 0.0, size, size)
}

/// A horizontal scrub bar, centered near the bottom of the window, mapping
/// between screen-x and frame index. Geometry and hit-testing only -- drawing
/// and input polling stay in main.rs, same split as Camera.
#[derive(Clone, Copy)]
pub struct Timeline {
    pub left: f32,
    pub width: f32,
    pub y: f32,
}

impl Timeline {
    /// Vertical distance from the bar within which a click/drag counts as
    /// grabbing it, rather than falling through to camera pan.
    pub const HIT_HEIGHT: f32 = 14.0;

    pub fn for_screen(screen_width: f32, screen_height: f32) -> Self {
        let width = screen_width * 0.6;
        Timeline { left: (screen_width - width) / 2.0, width, y: screen_height - 40.0 }
    }

    /// Where a frame index sits along the bar.
    pub fn x_for_index(&self, index: usize, frame_count: usize) -> f32 {
        if frame_count <= 1 {
            return self.left;
        }
        self.left + self.width * (index as f32 / (frame_count - 1) as f32)
    }

    /// The nearest frame index to an x position, clamped to the bar's ends
    /// rather than requiring the point land exactly on it.
    pub fn index_for_x(&self, x: f32, frame_count: usize) -> usize {
        if frame_count <= 1 {
            return 0;
        }
        let t = ((x - self.left) / self.width).clamp(0.0, 1.0);
        (t * (frame_count - 1) as f32).round() as usize
    }

    /// Whether a point (typically the mouse) is close enough to the bar,
    /// horizontally within its ends and vertically within `HIT_HEIGHT`, to
    /// count as interacting with it rather than the view behind it.
    pub fn contains(&self, point: Vec2) -> bool {
        point.x >= self.left && point.x <= self.left + self.width && (point.y - self.y).abs() <= Self::HIT_HEIGHT
    }
}

/// Deterministic name -> color, so a given entity type is always the same
/// color across runs with nothing to curate as new Factorio types show up.
pub fn color_for(name: &str, saturation: f32, value: f32) -> Color {
    let mut hash: u32 = 2166136261;
    for b in name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    let hue = (hash % 360) as f32 / 360.0;
    let (r, g, b) = hsv_to_rgb(hue, saturation, value);
    Color::new(r, g, b, 1.0)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Grid of fabricated entities, cycling through a handful of type names, for
/// load-testing at counts the real fixtures don't reach.
pub fn synthetic_frame(count: usize) -> Frame {
    const NAMES: &[&str] = &[
        "transport-belt",
        "assembling-machine-1",
        "electric-pole",
        "inserter",
        "pipe",
        "splitter",
    ];
    let side = (count as f32).sqrt().ceil() as i64;
    let spacing = 2.0;
    let entities = (0..count)
        .map(|i| {
            let ix = (i as i64) % side;
            let iy = (i as i64) / side;
            Entity {
                n: NAMES[i % NAMES.len()].into(),
                x: ix as f32 * spacing,
                y: iy as f32 * spacing,
                d: 0,
                w: 1,
                h: 1,
            }
        })
        .collect();
    Frame { tick: 0, surface: "synthetic".to_string(), count, entities, tiles: Vec::new() }
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

/// A directory of `frame_*.stfr` (sorted by filename, matching the CLI's own
/// zero-padded `frame_NNNN.stfr` output, so plain lexicographic sort is
/// enough) or a single frame file.
fn frame_is_candidate(path: &Path) -> bool {
    // Extension first. Live capture writes a `frame_<tick>_<surface>.stfr.done`
    // marker beside each finished snapshot, and its *stem* is
    // `frame_<tick>_<surface>.stfr`, which passes every check below, so
    // without this the viewer treats every marker as a frame and warns about
    // failing to parse an empty file.
    if path.extension().and_then(|e| e.to_str()) != Some("stfr") {
        return false
    }

    let stem = match path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return false,
    };
    if !stem.starts_with("frame_") {
        return false
    }

    let rest = &stem[6..];
    let mut parts = rest.splitn(2, '_');
    let tick_part = parts.next().unwrap_or("");
    let surface_part = parts.next().unwrap_or("");

    if surface_part == "manifest" {
        return false
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
    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| frame_is_candidate(p))
        .collect();
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

/// Loads many frame files across every available CPU core instead of one at
/// a time. Reading and parsing a `.stfr` file is pure, independent work with
/// nothing shared until frames become `RenderFrame`s (which need a single
/// `TypeRegistry` handing out consistent type ids across the whole
/// sequence), so parsing is what parallelises: on a real megabase capture
/// (millions of tiles per frame, dozens of frames), it was the dominant cost
/// of opening the viewer, roughly halved by this on an 8 core machine.
///
/// Runs on a fresh OS thread rather than blocking the caller, so a caller
/// driving macroquad's `next_frame`-based render loop can keep drawing a
/// progress bar while this proceeds. `done`/`total` report progress as files
/// finish; `poll` is how the caller collects the result once ready.
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
/// Filenames cannot be trusted for ordering. The CLI writes zero-padded
/// `frame_0000.stfr` in save order, where lexicographic sort happens to work,
/// but the mod writes `frame_<tick>_<surface>.stfr` with a raw unpadded tick,
/// so sorting names puts tick 1200 and 12600 before 600. The parsed `tick`
/// is the one thing both schemes carry.
///
/// When several surfaces were exported at the same tick, the busiest wins:
/// showing them in sequence would make the camera jump between planets.
pub fn order_by_tick<T>(frames: &mut Vec<T>, tick: impl Fn(&T) -> u64, count: impl Fn(&T) -> usize) {
    frames.sort_by(|a, b| tick(a).cmp(&tick(b)).then(count(b).cmp(&count(a))));

    let mut seen = None;
    frames.retain(|frame| {
        let keep = seen != Some(tick(frame));
        seen = Some(tick(frame));
        keep
    });
}

pub fn load_sequence(path: &Path) -> io::Result<Vec<Frame>> {
    let mut frames: Vec<Frame> = frame_paths(path)?.iter().filter_map(|p| load_frame(p)).collect();

    if frames.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no valid frame files found in {}", path.display()),
        ));
    }

    order_by_tick(&mut frames, |f| f.tick, |f| f.entities.len());
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    fn sample_frame(tick: u64) -> RenderFrame {
        render(Frame {
            tick,
            surface: "nauvis".to_string(),
            count: 0,
            entities: Vec::new(),
            tiles: Vec::new(),
        })
    }

    fn render(frame: Frame) -> RenderFrame {
        RenderFrame::from_frame(frame, &mut TypeRegistry::new())
    }

    fn entity(n: &str, x: f32, y: f32) -> Entity {
        Entity { n: n.into(), x, y, d: 0, w: 1, h: 1 }
    }

    #[test]
    fn screen_and_world_conversion_round_trips() {
        let camera = Camera { offset: Vec2::new(12.0, -34.0), zoom: 2.0 };
        let screen_center = Vec2::new(400.0, 300.0);
        let world = Vec2::new(5.0, 6.0);
        let back = camera.screen_to_world(camera.world_to_screen(world, screen_center), screen_center);
        assert!((back - world).length() < 1e-3, "expected {world:?}, got {back:?}");
    }

    #[test]
    fn fit_frames_centers_on_the_bounding_box() {
        let frame = render(Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 2,
            entities: vec![entity("a", 0.0, 0.0), entity("b", 10.0, 10.0)],
            tiles: Vec::new(),
        });
        let camera = Camera::fit_frames(std::slice::from_ref(&frame), 800.0, 600.0);
        assert_eq!(camera.offset, Vec2::new(5.0, 5.0));
        assert!(camera.zoom.is_finite() && camera.zoom > 0.0);
    }

    #[test]
    fn icon_path_prefers_the_first_existing_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let base_icon_dir = dir.path().join("base").join("graphics/icons");
        let space_age_icon_dir = dir.path().join("space-age").join("graphics/icons");
        std::fs::create_dir_all(&base_icon_dir).unwrap();
        std::fs::create_dir_all(&space_age_icon_dir).unwrap();
        std::fs::write(base_icon_dir.join("stone-furnace.png"), b"icon").unwrap();
        std::fs::write(space_age_icon_dir.join("stone-furnace.png"), b"other").unwrap();

        let path = icon_path(dir.path(), "stone-furnace").unwrap();
        assert_eq!(path, base_icon_dir.join("stone-furnace.png"));
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
        });
        std::fs::write(&valid, bytes).unwrap();
        std::fs::write(&invalid, b"not a frame").unwrap();

        let frames = load_sequence(dir.path()).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].tick, 0);
    }

    #[test]
    fn fit_frames_handles_a_single_point_without_dividing_by_zero() {
        let frame = render(Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 1,
            entities: vec![entity("a", 3.0, 3.0)],
            tiles: Vec::new(),
        });
        let camera = Camera::fit_frames(std::slice::from_ref(&frame), 800.0, 600.0);
        assert!(camera.zoom.is_finite() && camera.zoom > 0.0);
    }

    #[test]
    fn fit_frames_on_empty_input_returns_a_sane_default() {
        let camera = Camera::fit_frames(&[], 800.0, 600.0);
        assert_eq!(camera.offset, Vec2::ZERO);
        assert!(camera.zoom.is_finite() && camera.zoom > 0.0);
    }

    #[test]
    fn hsv_to_rgb_known_values() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), (1.0, 0.0, 0.0));
        let (r, g, b) = hsv_to_rgb(0.5, 0.0, 0.7);
        assert!((r - 0.7).abs() < 1e-6 && (g - 0.7).abs() < 1e-6 && (b - 0.7).abs() < 1e-6);
    }

    #[test]
    fn color_for_is_deterministic() {
        let a = color_for("transport-belt", 0.55, 0.85);
        let b = color_for("transport-belt", 0.55, 0.85);
        assert_eq!((a.r, a.g, a.b), (b.r, b.g, b.b));
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
    fn entity_footprint_size_scales_by_tile_count() {
        assert_eq!(entity_footprint_size(32.0, 1, 1), Vec2::new(32.0, 32.0));
        assert_eq!(entity_footprint_size(32.0, 3, 2), Vec2::new(96.0, 64.0));
    }

    #[test]
    fn entity_footprint_size_never_collapses_to_zero_at_tiny_zoom() {
        let size = entity_footprint_size(0.001, 1, 1);
        assert!(size.x >= 1.0 && size.y >= 1.0);
    }

    #[test]
    fn icon_candidates_checks_base_then_space_age() {
        let candidates = icon_candidates(Path::new("/data"), "stone-furnace");
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/data/base/graphics/icons/stone-furnace.png"),
                PathBuf::from("/data/space-age/graphics/icons/stone-furnace.png"),
            ]
        );
    }

    /// Real vanilla/Space Age icon files measured directly: 64+32+16+8=120
    /// wide, 64 tall. The bug this guards against is drawing that whole
    /// strip stretched into one entity's box instead of cropping to the
    /// first (primary) icon.
    #[test]
    fn icon_source_rect_crops_a_real_mipmap_strip_to_the_primary_icon() {
        assert_eq!(icon_source_rect(120.0, 64.0), Rect::new(0.0, 0.0, 64.0, 64.0));
    }

    #[test]
    fn icon_source_rect_is_a_no_op_for_a_single_square_icon() {
        assert_eq!(icon_source_rect(64.0, 64.0), Rect::new(0.0, 0.0, 64.0, 64.0));
    }

    #[test]
    fn timeline_index_and_x_are_inverses() {
        let timeline = Timeline::for_screen(1000.0, 600.0);
        for index in [0, 3, 7, 12] {
            let x = timeline.x_for_index(index, 13);
            assert_eq!(timeline.index_for_x(x, 13), index, "index {index}");
        }
    }

    #[test]
    fn timeline_index_for_x_clamps_beyond_the_bar_ends() {
        let timeline = Timeline::for_screen(1000.0, 600.0);
        assert_eq!(timeline.index_for_x(timeline.left - 500.0, 10), 0);
        assert_eq!(timeline.index_for_x(timeline.left + timeline.width + 500.0, 10), 9);
    }

    #[test]
    fn timeline_with_a_single_frame_never_divides_by_zero() {
        let timeline = Timeline::for_screen(1000.0, 600.0);
        assert_eq!(timeline.index_for_x(timeline.left + 999.0, 1), 0);
        assert_eq!(timeline.x_for_index(0, 1), timeline.left);
    }

    #[test]
    fn timeline_contains_checks_both_axes() {
        let timeline = Timeline::for_screen(1000.0, 600.0);
        let mid = Vec2::new(timeline.left + timeline.width / 2.0, timeline.y);
        assert!(timeline.contains(mid));
        assert!(!timeline.contains(Vec2::new(timeline.left - 50.0, timeline.y)), "left of the bar");
        assert!(!timeline.contains(Vec2::new(mid.x, timeline.y - 200.0)), "far above the bar");
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

    /// Writes a `.stfr` frame with `entity_count` fabricated entities, for
    /// tests that only care about tick/surface ordering, not real content.
    fn write_stub_frame(dir: &Path, name: &str, tick: u64, surface: &str, entity_count: usize) {
        let entities = synthetic_frame(entity_count).entities;
        let out = save_timelapse::frame::FrameOut { tick, surface, entities: &entities, tiles: &[] };
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

    #[test]
    fn registry_interns_repeated_names_to_one_id() {
        let mut registry = TypeRegistry::new();
        let belt = registry.intern("transport-belt");
        let pipe = registry.intern("pipe");
        assert_eq!(registry.intern("transport-belt"), belt);
        assert_ne!(belt, pipe);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.name(belt), "transport-belt");
    }

    /// The registry has to agree with the function it replaces, or interning
    /// would silently recolor every entity in the viewer.
    #[test]
    fn registry_colors_match_the_underlying_hash() {
        let mut registry = TypeRegistry::new();
        let id = registry.intern("assembling-machine-1");
        let entity = color_for("assembling-machine-1", 0.55, 0.85);
        let tile = color_for("assembling-machine-1", 0.35, 0.5);
        assert_eq!(
            (registry.entity_color(id).r, registry.entity_color(id).g, registry.entity_color(id).b),
            (entity.r, entity.g, entity.b)
        );
        assert_eq!(
            (registry.tile_color(id).r, registry.tile_color(id).g, registry.tile_color(id).b),
            (tile.r, tile.g, tile.b)
        );
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
        let tiles: Vec<Tile> =
            (0..LOD_CELL_TILES + 1).map(|x| Tile { n: "concrete".into(), x, y: 0 }).collect();
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

    /// The whole point of grouping: one texture switch per type instead of
    /// one per entity. macroquad only merges into the immediately preceding
    /// draw call, so interleaved types cost a draw call each.
    #[test]
    fn grouping_collapses_draw_calls_against_interleaved_order() {
        let types: Vec<TypeId> = (0..600).map(|i| (i % 3) as TypeId).collect();
        let huge_buffer = usize::MAX / 2;

        let mut interleaved = DrawCallCounter::new(huge_buffer);
        for &t in &types {
            interleaved.quad(Some(t));
        }
        assert_eq!(interleaved.calls, 600, "every switch starts a new draw call");

        let mut grouped = DrawCallCounter::new(huge_buffer);
        let mut sorted = types.clone();
        sorted.sort();
        for &t in &sorted {
            grouped.quad(Some(t));
        }
        assert_eq!(grouped.calls, 3, "one per distinct type");
    }

    /// Untextured rects are their own texture state, so interleaving shapes
    /// and sprites breaks the batch exactly like two different sprites do.
    #[test]
    fn untextured_quads_break_the_batch_like_a_texture_change() {
        let mut counter = DrawCallCounter::new(usize::MAX / 2);
        counter.quad(Some(0));
        counter.quad(None);
        counter.quad(Some(0));
        assert_eq!(counter.calls, 3);
    }

    /// Even perfectly grouped, macroquad's index buffer caps a draw call at
    /// `max_indices / 6` quads -- the ceiling that made raising the capacity
    /// worth doing alongside the sorting.
    #[test]
    fn a_full_index_buffer_splits_one_texture_across_draw_calls() {
        let max_indices = 5000; // macroquad's default
        let per_call = max_indices / DrawCallCounter::INDICES_PER_QUAD; // 833
        let mut counter = DrawCallCounter::new(max_indices);
        for _ in 0..per_call * 2 {
            counter.quad(Some(0));
        }
        assert_eq!(counter.calls, 2);

        counter.quad(Some(0));
        assert_eq!(counter.calls, 3, "one past a full buffer starts another call");
    }

    /// The bulk helper is only useful if it counts identically to the
    /// per-quad path it replaces, across buffer boundaries.
    #[test]
    fn bulk_quads_match_counting_one_at_a_time() {
        for n in [0usize, 1, 5, 832, 833, 834, 1666, 1667, 5000] {
            let mut one_by_one = DrawCallCounter::new(5000);
            for _ in 0..n {
                one_by_one.quad(Some(1));
            }
            let mut bulk = DrawCallCounter::new(5000);
            bulk.quads(Some(1), n);
            assert_eq!(bulk.calls, one_by_one.calls, "calls for n={n}");
            assert_eq!(bulk.quads, one_by_one.quads, "quads for n={n}");
        }
    }

    #[test]
    fn bulk_quads_then_a_switch_still_counts_the_switch() {
        let mut counter = DrawCallCounter::new(5000);
        counter.quads(Some(1), 10);
        counter.quads(Some(2), 10);
        counter.quads(Some(1), 10);
        assert_eq!(counter.calls, 3);
        assert_eq!(counter.quads, 30);
    }

    #[test]
    fn load_progress_fraction_is_bounded_and_safe_at_zero_total() {
        let at = |done, total| LoadProgress {
            phase: "frames",
            detail: String::new(),
            done,
            total,
        }
        .fraction();
        assert_eq!(at(0, 0), 0.0, "an empty job must not divide by zero");
        assert_eq!(at(0, 4), 0.0);
        assert_eq!(at(2, 4), 0.5);
        assert_eq!(at(4, 4), 1.0);
        assert_eq!(at(9, 4), 1.0, "overshoot clamps rather than overflowing the bar");
    }

    #[test]
    fn progress_bar_is_centered_and_fills_proportionally() {
        let bar = ProgressBar::centered(1000.0, 600.0);
        assert_eq!(bar.left + bar.width / 2.0, 500.0);
        assert_eq!(bar.top + bar.height / 2.0, 300.0);

        let half = LoadProgress { phase: "frames", detail: String::new(), done: 1, total: 2 };
        assert_eq!(bar.filled_width(&half), bar.width / 2.0);
    }
}
