//! Everything that doesn't touch macroquad's window/input globals, split out
//! so it's unit testable: `main.rs` is thin glue over this.

use std::io;
use std::path::Path;

use macroquad::color::Color;
use macroquad::math::Vec2;
use save_timelapse::frame::{Entity, Frame, Tile};

pub const BASE_PIXELS_PER_TILE: f32 = 32.0;

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
    pub fn fit_frames(frames: &[Frame], screen_width: f32, screen_height: f32) -> Camera {
        let mut points = frames.iter().flat_map(|f| {
            f.entities
                .iter()
                .map(|e| Vec2::new(e.x, e.y))
                .chain(f.tiles.iter().map(|t| Vec2::new(t.x as f32, t.y as f32)))
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
                n: NAMES[i % NAMES.len()].to_string(),
                x: ix as f32 * spacing,
                y: iy as f32 * spacing,
                d: 0,
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
            Tile { n: "concrete".to_string(), x: ix as i32, y: iy as i32 }
        })
        .collect()
}

/// A loaded sequence of frames with a current position. Always non-empty:
/// construction from zero frames is rejected rather than leaving every
/// accessor to guard against it.
pub struct FrameSequence {
    frames: Vec<Frame>,
    index: usize,
}

impl FrameSequence {
    pub fn new(frames: Vec<Frame>) -> Option<Self> {
        if frames.is_empty() {
            return None;
        }
        Some(Self { frames, index: 0 })
    }

    pub fn current(&self) -> &Frame {
        &self.frames[self.index]
    }

    pub fn frames(&self) -> &[Frame] {
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

/// A directory of `frame_*.json` (sorted by filename -- matches the CLI's
/// own zero-padded `frame_NNNN.json` output, so plain lexicographic sort is
/// enough) or a single frame file.
pub fn load_sequence(path: &Path) -> io::Result<Vec<Frame>> {
    let paths: Vec<std::path::PathBuf> = if path.is_dir() {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(path)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("json")
                    && p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.starts_with("frame_"))
            })
            .collect();
        entries.sort();
        entries
    } else {
        vec![path.to_path_buf()]
    };

    paths
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p)?;
            serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame(tick: u64) -> Frame {
        Frame { tick, surface: "nauvis".to_string(), count: 0, entities: Vec::new(), tiles: Vec::new() }
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
        let frame = Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 2,
            entities: vec![
                Entity { n: "a".to_string(), x: 0.0, y: 0.0, d: 0 },
                Entity { n: "b".to_string(), x: 10.0, y: 10.0, d: 0 },
            ],
            tiles: Vec::new(),
        };
        let camera = Camera::fit_frames(std::slice::from_ref(&frame), 800.0, 600.0);
        assert_eq!(camera.offset, Vec2::new(5.0, 5.0));
        assert!(camera.zoom.is_finite() && camera.zoom > 0.0);
    }

    #[test]
    fn fit_frames_handles_a_single_point_without_dividing_by_zero() {
        let frame = Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 1,
            entities: vec![Entity { n: "a".to_string(), x: 3.0, y: 3.0, d: 0 }],
            tiles: Vec::new(),
        };
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
        assert!(tiles.iter().all(|t| t.n == "concrete"));
        assert_eq!((tiles[0].x, tiles[0].y), (0, 0));
        assert_eq!((tiles[3].x, tiles[3].y), (0, 1));
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

    #[test]
    fn load_sequence_sorts_a_directory_regardless_of_iteration_order() {
        let dir = tempfile::tempdir().unwrap();
        for (name, tick) in [("frame_0002.json", 2u64), ("frame_0000.json", 0), ("frame_0001.json", 1)] {
            let json = format!(r#"{{"tick":{tick},"surface":"nauvis","entities":[],"count":0}}"#);
            std::fs::write(dir.path().join(name), json).unwrap();
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
}
