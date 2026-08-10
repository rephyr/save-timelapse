//! Pan/zoom camera and the timeline scrub bar. Geometry and hit-testing
//! only; drawing and input polling stay in `main.rs`.

use macroquad::math::Vec2;

use crate::render_frame::RenderFrame;

pub const BASE_PIXELS_PER_TILE: f32 = 32.0;

/// How much smaller than a bare edge-to-edge fit `fit_frames`'s initial,
/// static whole-sequence view zooms. Leaves visible breathing room around
/// the base instead of touching the window edges. Auto-follow uses its own,
/// tighter margin (see `AUTO_FOLLOW_FIT_MARGIN` in `main.rs`); this one is
/// only for the one-time opening view.
const FIT_MARGIN: f32 = 0.75;

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
    /// frame given (plus `terrain`'s own tiles, if a terrain layer is
    /// loaded), and pick a zoom that fits it on screen. Fitting the whole
    /// sequence rather than just the current frame means scrubbing through a
    /// growing base doesn't jump-recenter every step. Terrain is folded in
    /// too so the opening view shows the factory sitting on real ground
    /// rather than framing just the built area and cropping the terrain
    /// captured around it. Real bases are almost never near world origin, so
    /// an empty/degenerate input falls back to a sane default rather than
    /// opening on empty space.
    ///
    /// The whole-run fit, over a sequence rather than a slice of frames,
    /// since frames are no longer all resident at once (see `FrameSequence`).
    /// Every frame is materialized once into a shared scratch buffer as this
    /// walks, which is why it takes corners as it goes rather than collecting
    /// them: the frames do not outlive the walk.
    pub fn fit_sequence(
        frames: &crate::render_frame::FrameSequence,
        terrain: Option<&RenderFrame>,
        screen_width: f32,
        screen_height: f32,
    ) -> Camera {
        let mut bounds: Option<(Vec2, Vec2)> = None;
        let mut take = |point: Vec2| {
            bounds = Some(match bounds {
                Some((min, max)) => (min.min(point), max.max(point)),
                None => (point, point),
            });
        };

        frames.for_each_frame(|_, frame| {
            for entity in &frame.entities {
                let half = Vec2::new(entity.w as f32, entity.h as f32) / 2.0;
                let center = Vec2::new(entity.x, entity.y);
                take(center - half);
                take(center + half);
            }
            for tile in &frame.tiles {
                take(Vec2::new(tile.x as f32, tile.y as f32));
                take(Vec2::new(tile.x as f32 + 1.0, tile.y as f32 + 1.0));
            }
        });
        if let Some(terrain) = terrain {
            for tile in &terrain.tiles {
                take(Vec2::new(tile.x as f32, tile.y as f32));
                take(Vec2::new(tile.x as f32 + 1.0, tile.y as f32 + 1.0));
            }
        }

        match bounds {
            Some((min, max)) => Self::fit_bounds((min + max) / 2.0, max - min, screen_width, screen_height, 1.0, FIT_MARGIN),
            None => Camera { offset: Vec2::ZERO, zoom: 1.0 },
        }
    }

    pub fn fit_frames(frames: &[RenderFrame], terrain: Option<&RenderFrame>, screen_width: f32, screen_height: f32) -> Camera {
        // Each entity contributes its two footprint corners, not just its
        // center point. For a small cluster of large buildings, ignoring
        // footprint here would zoom in as if they were 1x1, and a real
        // multi-tile entity would then render bigger than the whole window.
        let mut points = frames
            .iter()
            .flat_map(|f| {
                let entity_corners = f.entities.iter().flat_map(|e| {
                    let half = Vec2::new(e.w as f32, e.h as f32) / 2.0;
                    let center = Vec2::new(e.x, e.y);
                    [center - half, center + half]
                });
                let tile_points = f.tiles.iter().map(|t| Vec2::new(t.x as f32, t.y as f32));
                entity_corners.chain(tile_points)
            })
            .chain(terrain.into_iter().flat_map(|t| t.tiles.iter().map(|t| Vec2::new(t.x as f32, t.y as f32))));
        let Some(first) = points.next() else {
            return Camera { offset: Vec2::ZERO, zoom: 1.0 };
        };
        let mut min = first;
        let mut max = first;
        for p in points {
            min = min.min(p);
            max = max.max(p);
        }
        Self::fit_bounds((min + max) / 2.0, max - min, screen_width, screen_height, 1.0, FIT_MARGIN)
    }

    /// Center on an arbitrary world-space box and pick a zoom that fits it on
    /// screen. `min_size_tiles` floors how tight this can zoom in either
    /// axis: framing a single small placement without a floor would zoom in
    /// far enough to fill the screen with one tile, which reads as broken
    /// rather than "focused." `margin` is how much smaller than a bare
    /// edge-to-edge fit to zoom (1.0 = box edges touch the window border,
    /// smaller = more breathing room), a caller's choice rather than a
    /// fixed constant, since a one-time static view and a continuously
    /// re-fitting auto-follow camera want different amounts of it (see
    /// `fit_frames` and `AUTO_FOLLOW_FIT_MARGIN` in `main.rs`).
    pub fn fit_bounds(
        center: Vec2,
        size: Vec2,
        screen_width: f32,
        screen_height: f32,
        min_size_tiles: f32,
        margin: f32,
    ) -> Camera {
        let size = size.max(Vec2::splat(min_size_tiles));
        let zoom = (screen_width / (size.x * BASE_PIXELS_PER_TILE)).min(screen_height / (size.y * BASE_PIXELS_PER_TILE)) * margin;
        Camera { offset: center, zoom: zoom.clamp(0.01, 50.0) }
    }
}

/// A camera move from wherever it currently is to a new target, over a
/// fixed real-time duration, the same model TLBE (the most-downloaded
/// Factorio timelapse mod) uses for its own camera transitions: plain
/// linear interpolation of both position and zoom against elapsed time,
/// not an easing curve. A fixed duration is what makes a transition read as
/// deliberate and gradual regardless of how far it's travelling, rather
/// than an exponential approach that (without extra clamping of its own)
/// moves fastest the instant a distant target first appears.
#[derive(Clone, Copy)]
pub struct CameraTransition {
    start: Camera,
    end: Camera,
    elapsed_secs: f32,
    duration_secs: f32,
}

impl CameraTransition {
    /// Starts from `start` (the camera's live position, not the previous
    /// target) so retargeting mid-transition (the point of interest
    /// moving again before the camera arrived) glides on from wherever
    /// the camera actually is, with no discontinuity, rather than jumping
    /// back to resume an old transition's endpoint.
    pub fn new(start: Camera, end: Camera, duration_secs: f32) -> Self {
        CameraTransition { start, end, elapsed_secs: 0.0, duration_secs }
    }

    /// Advances the transition by `dt_secs` and returns the camera at the
    /// new elapsed time. Zoom is interpolated linearly, matching TLBE,
    /// rather than in log space: a "technically nicer" curve isn't the
    /// point here, matching the reference mod's own feel is.
    pub fn step(&mut self, dt_secs: f32) -> Camera {
        self.elapsed_secs = (self.elapsed_secs + dt_secs).min(self.duration_secs);
        let t = if self.duration_secs <= 0.0 { 1.0 } else { self.elapsed_secs / self.duration_secs };
        Camera {
            offset: self.start.offset + (self.end.offset - self.start.offset) * t,
            zoom: self.start.zoom + (self.end.zoom - self.start.zoom) * t,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.elapsed_secs >= self.duration_secs
    }
}

/// The on-screen size of an entity's footprint, in pixels. Most entities are
/// 1x1, but assemblers, furnaces and the like span several tiles, so sizing
/// every entity to a fixed 1-tile square (the pre-footprint behavior) is why
/// multi-tile buildings used to render undersized and visually disconnected
/// from whatever was actually touching them.
pub fn entity_footprint_size(pixels_per_tile: f32, w: u32, h: u32) -> Vec2 {
    Vec2::new((w as f32 * pixels_per_tile).max(1.0), (h as f32 * pixels_per_tile).max(1.0))
}

/// Whether an entity's footprint is safe to rotate at all. `is_rotation_allowed`
/// is the curated `registry::is_rotation_allowed` allowlist result: only
/// entities confirmed to have a flat, top-down icon go through (this also
/// covers terrain scatter for free, since trees/cliffs are never added to
/// that allowlist). Separately, a non-square footprint's `w`/`h` already
/// reflect Factorio's own post-rotation swap (see `entity_rotation_radians`),
/// so rotating the already-swapped quad again would double count it. A
/// square footprint has no such swap (it's a no-op regardless of
/// direction), so it's the only footprint shape safe to rotate.
fn is_rotatable_square(w: u32, h: u32, is_rotation_allowed: bool) -> bool {
    is_rotation_allowed && w == h
}

/// An entity's on-screen rotation, in radians, from Factorio's raw 16-way
/// `direction` byte (0 = north, clockwise in 22.5 degree steps). Zero for
/// anything not safe to rotate: Factorio's own `tile_width`/`tile_height`
/// (our `w`/`h`) are read straight off the live, already-rotated entity,
/// so for a rotated rectangular entity they already reflect the
/// post-rotation footprint swap. Rotating the drawn quad again on top of
/// that would misalign it from the entity's real world-space footprint,
/// and there is no native, unrotated size captured anywhere to undo the
/// swap correctly. See `is_rotatable_square`.
///
/// The `- 4.0`: confirmed against a real capture that a belt set to face
/// east (`d = 4`) rendered facing south instead, for every direction, not
/// just that one. This project's icons are the generic square inventory
/// icons (not Factorio's real in-world sprites, see `sprites.rs`), and the
/// belt icon's own native, unrotated artwork already depicts it facing
/// east, not north as first assumed. Solving the reported symptom directly
/// (observed south at `d = 4`, wanted east) gives a fixed reference-angle
/// correction of one quarter turn, not a sign flip.
pub fn entity_rotation_radians(w: u32, h: u32, d: u8, is_rotation_allowed: bool) -> f32 {
    if !is_rotatable_square(w, h, is_rotation_allowed) {
        return 0.0;
    }
    (d as f32 - 4.0) * (std::f32::consts::PI / 8.0)
}

/// Half-extents to use for view-frustum culling, in world-space tiles.
/// Plain `w/2, h/2` for everything except a rotatable square at a
/// non-cardinal direction (`d` not a multiple of 4): a rotated square's
/// on-screen bounding box grows by up to sqrt(2) on each axis, maximal
/// exactly at a 45 degree diagonal rather than at a cardinal angle, so
/// culling against the unrotated box there would occasionally clip an
/// entity a few pixels early at the view's edge. Gated on `!d.is_multiple_of(4)`
/// (a plain integer check, no trig) so the common case, a cardinally
/// placed square building, pays nothing extra.
pub fn entity_cull_half_extents(w: u32, h: u32, d: u8, is_rotation_allowed: bool) -> Vec2 {
    let half = Vec2::new(w as f32, h as f32) / 2.0;
    if is_rotatable_square(w, h, is_rotation_allowed) && !d.is_multiple_of(4) {
        half * std::f32::consts::SQRT_2
    } else {
        half
    }
}

/// A horizontal scrub bar, centered near the bottom of the window, mapping
/// between screen-x and frame index. Geometry and hit-testing only; drawing
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

    /// How close a tooltip is allowed to get to the window edge before it
    /// stops following the cursor and parks instead.
    pub const TOOLTIP_MARGIN: f32 = 8.0;

    /// Left edge for a hover tooltip of `width` that would ideally be
    /// centered on `center_x`, pushed back inside the window when centering
    /// would hang it off an edge.
    ///
    /// Needed because the tooltip tracks the cursor across the whole bar
    /// while the bar itself reaches to within 20% of each window edge (see
    /// `for_screen`). A label wide enough to be worth reading is therefore
    /// wider than the gap beside the bar's ends, so it would be clipped
    /// exactly at the first and last frame, which are two of the positions
    /// someone is most likely to go looking at.
    ///
    /// Clamping rather than flipping the tooltip to the other side of the
    /// cursor: flipping keeps the pointer relationship exact but makes the
    /// box jump sideways mid-drag, and a scrub bar is dragged along its
    /// whole length, so that jump would happen constantly.
    pub fn tooltip_left(center_x: f32, width: f32, screen_width: f32) -> f32 {
        let margin = Self::TOOLTIP_MARGIN;
        // A tooltip too wide for the window has no in-bounds placement at
        // all, so max_left would fall below margin and invert the clamp
        // range, which `f32::clamp` panics on. Pinning it left in that case
        // at least keeps the start of the text readable.
        let max_left = (screen_width - margin - width).max(margin);
        (center_x - width / 2.0).clamp(margin, max_left)
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
        let camera = Camera::fit_frames(std::slice::from_ref(&frame), None, 800.0, 600.0);
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
        let camera = Camera::fit_frames(std::slice::from_ref(&frame), None, 800.0, 600.0);
        assert!(camera.zoom.is_finite() && camera.zoom > 0.0);
    }

    #[test]
    fn fit_frames_on_empty_input_returns_a_sane_default() {
        let camera = Camera::fit_frames(&[], None, 800.0, 600.0);
        assert_eq!(camera.offset, Vec2::ZERO);
        assert!(camera.zoom.is_finite() && camera.zoom > 0.0);
    }

    /// The opening view is the whole point of turning terrain capture on: a
    /// wide establishing shot of the factory sitting on real ground. A
    /// terrain layer wider than the built area must expand the fit, not get
    /// ignored.
    #[test]
    fn fit_frames_folds_terrain_into_the_bounding_box() {
        let frame = render(Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 1,
            entities: vec![entity("a", 0.0, 0.0)],
            tiles: Vec::new(),
        });
        let terrain = render(Frame {
            tick: 0,
            surface: "nauvis".to_string(),
            count: 0,
            entities: Vec::new(),
            tiles: vec![
                save_timelapse::frame::Tile { n: "grass-1".into(), x: -50, y: -50 },
                save_timelapse::frame::Tile { n: "grass-1".into(), x: 50, y: 50 },
            ],
        });

        let without_terrain = Camera::fit_frames(std::slice::from_ref(&frame), None, 800.0, 600.0);
        let with_terrain = Camera::fit_frames(std::slice::from_ref(&frame), Some(&terrain), 800.0, 600.0);
        assert!(
            with_terrain.zoom < without_terrain.zoom,
            "a wider terrain box must zoom out further: {} vs {}",
            with_terrain.zoom,
            without_terrain.zoom
        );
    }

    /// `min_size_tiles = 1.0` must reproduce `fit_frames`'s own
    /// `.max(Vec2::splat(1.0))` floor exactly, or the refactor changed
    /// behavior instead of just relocating it.
    #[test]
    fn fit_bounds_matches_fit_frames_on_the_same_box() {
        let via_fit_frames = Camera::fit_frames(
            std::slice::from_ref(&render(Frame {
                tick: 0,
                surface: "nauvis".to_string(),
                count: 2,
                entities: vec![entity("a", 0.0, 0.0), entity("b", 10.0, 10.0)],
                tiles: Vec::new(),
            })),
            None,
            800.0,
            600.0,
        );
        let via_fit_bounds = Camera::fit_bounds(Vec2::new(5.0, 5.0), Vec2::new(11.0, 11.0), 800.0, 600.0, 1.0, FIT_MARGIN);
        assert_eq!(via_fit_frames.offset, via_fit_bounds.offset);
        assert!((via_fit_frames.zoom - via_fit_bounds.zoom).abs() < 1e-6);
    }

    #[test]
    fn fit_bounds_floors_a_tiny_box_to_the_minimum_size() {
        let tight = Camera::fit_bounds(Vec2::ZERO, Vec2::splat(0.1), 800.0, 600.0, 24.0, 1.0);
        let at_floor = Camera::fit_bounds(Vec2::ZERO, Vec2::splat(24.0), 800.0, 600.0, 24.0, 1.0);
        assert!(
            (tight.zoom - at_floor.zoom).abs() < 1e-6,
            "a box smaller than the floor should zoom as if it were exactly the floor size"
        );
    }

    #[test]
    fn fit_bounds_margin_scales_zoom_directly() {
        let tight = Camera::fit_bounds(Vec2::ZERO, Vec2::splat(10.0), 800.0, 600.0, 1.0, 1.0);
        let loose = Camera::fit_bounds(Vec2::ZERO, Vec2::splat(10.0), 800.0, 600.0, 1.0, 0.5);
        assert!((loose.zoom - tight.zoom * 0.5).abs() < 1e-4, "half the margin should mean half the zoom");
    }

    #[test]
    fn camera_transition_is_a_no_op_at_zero_elapsed() {
        let start = Camera { offset: Vec2::new(1.0, 2.0), zoom: 1.0 };
        let end = Camera { offset: Vec2::new(100.0, 200.0), zoom: 10.0 };
        let mut transition = CameraTransition::new(start, end, 2.0);
        let step = transition.step(0.0);
        assert_eq!(step.offset, start.offset);
        assert!((step.zoom - start.zoom).abs() < 1e-6);
    }

    #[test]
    fn camera_transition_is_halfway_at_half_the_duration() {
        let start = Camera { offset: Vec2::ZERO, zoom: 1.0 };
        let end = Camera { offset: Vec2::new(100.0, 0.0), zoom: 5.0 };
        let mut transition = CameraTransition::new(start, end, 2.0);
        let step = transition.step(1.0);
        assert!((step.offset.x - 50.0).abs() < 1e-4);
        assert!((step.zoom - 3.0).abs() < 1e-4, "zoom interpolates linearly, matching TLBE, not in log space");
    }

    #[test]
    fn camera_transition_reaches_exactly_the_end_and_then_reports_finished() {
        let start = Camera { offset: Vec2::ZERO, zoom: 1.0 };
        let end = Camera { offset: Vec2::new(100.0, -40.0), zoom: 8.0 };
        let mut transition = CameraTransition::new(start, end, 2.0);
        let step = transition.step(2.0);
        assert_eq!(step.offset, end.offset);
        assert_eq!(step.zoom, end.zoom);
        assert!(transition.is_finished());
    }

    /// Advancing past the end (e.g. one slow frame straddling the
    /// transition's finish) must clamp rather than overshoot into
    /// extrapolation, which a naive `t = elapsed / duration` without
    /// clamping elapsed would do.
    #[test]
    fn camera_transition_clamps_past_the_end_instead_of_overshooting() {
        let start = Camera { offset: Vec2::ZERO, zoom: 1.0 };
        let end = Camera { offset: Vec2::new(100.0, 0.0), zoom: 1.0 };
        let mut transition = CameraTransition::new(start, end, 1.0);
        let step = transition.step(5.0);
        assert_eq!(step.offset, end.offset);
        assert!(transition.is_finished());
    }

    /// Multiple small steps that add up to the full duration must land in
    /// the same place as one big step: the draw loop calls `step` once
    /// per rendered frame, so this is what makes the transition's total
    /// duration independent of frame rate.
    #[test]
    fn camera_transition_many_small_steps_match_one_big_step() {
        let start = Camera { offset: Vec2::ZERO, zoom: 1.0 };
        let end = Camera { offset: Vec2::new(100.0, 50.0), zoom: 4.0 };
        let mut stepped = CameraTransition::new(start, end, 2.0);
        let mut result = stepped.step(0.0);
        for _ in 0..120 {
            result = stepped.step(2.0 / 120.0);
        }
        assert!((result.offset - end.offset).length() < 1e-3);
        assert!((result.zoom - end.zoom).abs() < 1e-3);
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
    fn entity_rotation_radians_is_zero_for_non_square_footprints() {
        assert_eq!(entity_rotation_radians(1, 2, 4, true), 0.0, "already-swapped footprint, don't double-rotate");
    }

    #[test]
    fn entity_rotation_radians_is_zero_when_rotation_is_not_allowed() {
        assert_eq!(entity_rotation_radians(3, 3, 4, false), 0.0, "not on the curated allowlist");
    }

    /// The `d = 4` case is the regression check: a belt facing east must
    /// render at zero rotation (its icon's own native artwork already faces
    /// east), not `PI/2` as an uncorrected `d * PI/8` formula would give.
    #[test]
    fn entity_rotation_radians_scales_cardinal_directions_correctly() {
        use std::f32::consts::PI;
        assert!((entity_rotation_radians(3, 3, 0, true) - (-PI / 2.0)).abs() < 1e-6, "north");
        assert_eq!(entity_rotation_radians(3, 3, 4, true), 0.0, "east: the icon's own native facing");
        assert!((entity_rotation_radians(3, 3, 8, true) - PI / 2.0).abs() < 1e-6, "south");
        assert!((entity_rotation_radians(3, 3, 12, true) - PI).abs() < 1e-6, "west");
    }

    #[test]
    fn entity_cull_half_extents_only_widens_non_cardinal_square_entities() {
        use std::f32::consts::SQRT_2;
        assert_eq!(entity_cull_half_extents(2, 2, 0, true), Vec2::splat(1.0), "cardinal square: unchanged");
        assert_eq!(entity_cull_half_extents(2, 2, 2, true), Vec2::splat(SQRT_2), "diagonal square: widened");
        assert_eq!(entity_cull_half_extents(1, 2, 2, true), Vec2::new(0.5, 1.0), "non-square: never widened");
        assert_eq!(entity_cull_half_extents(2, 2, 2, false), Vec2::splat(1.0), "not allowed: never widened");
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
    fn a_tooltip_with_room_to_spare_is_centered_on_the_cursor() {
        assert_eq!(Timeline::tooltip_left(500.0, 120.0, 1000.0), 440.0);
    }

    /// The bar reaches to within 20% of each window edge, so a tooltip at
    /// the first or last frame is exactly where centering would clip it.
    #[test]
    fn a_tooltip_at_either_bar_end_is_pushed_back_on_screen() {
        let timeline = Timeline::for_screen(1000.0, 600.0);
        let margin = Timeline::TOOLTIP_MARGIN;
        let width = 300.0;

        let at_start = Timeline::tooltip_left(timeline.left, width, 1000.0);
        assert!(at_start >= margin, "ran off the left edge: {at_start}");

        let at_end = Timeline::tooltip_left(timeline.left + timeline.width, width, 1000.0);
        assert!(at_end + width <= 1000.0 - margin, "ran off the right edge: {at_end}");
    }

    /// `f32::clamp` panics when its range is inverted, which a tooltip wider
    /// than the window would otherwise produce.
    #[test]
    fn a_tooltip_wider_than_the_window_pins_left_instead_of_panicking() {
        assert_eq!(Timeline::tooltip_left(400.0, 5000.0, 800.0), Timeline::TOOLTIP_MARGIN);
    }
}
