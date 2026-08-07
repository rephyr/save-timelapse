//! Pan/zoom camera and the timeline scrub bar. Geometry and hit-testing
//! only -- drawing and input polling stay in `main.rs`.

use macroquad::math::Vec2;

use crate::render_frame::RenderFrame;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TypeRegistry;
    use save_timelapse::frame::{Entity, Frame};

    fn entity(n: &str, x: f32, y: f32) -> Entity {
        Entity { n: n.into(), x, y, d: 0, w: 1, h: 1 }
    }

    fn render(frame: Frame) -> RenderFrame {
        RenderFrame::from_frame(frame, &mut TypeRegistry::new())
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
}
