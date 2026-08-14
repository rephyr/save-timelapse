//! A camera move per frame, smoothed offline over the whole sequence.
//!
//! Fitting each frame's bounds directly is what makes an export snap: the
//! tracked box jumps the moment something is built far from everything else,
//! and the camera teleports with it. Interactive playback hides that behind
//! `CameraTransition`, which glides over wall-clock seconds, but an export has
//! no wall clock and advances as fast as the disk allows.
//!
//! An export does have something playback does not: the entire bounds sequence
//! up front, already precomputed by `construction::growing_bounds_per_frame`.
//! So rather than simulating a camera that chases the box, this filters the
//! path non-causally. The camera starts easing out *before* the far outpost
//! appears, which is what reads as a planned move rather than a reaction.
//!
//! Boxes are smoothed, not cameras. The aspect ratio only enters at
//! `Camera::fit_bounds`, so one path serves any output size and the
//! interactive window could share it.

use macroquad::math::Vec2;

use crate::camera::{Camera, Framing};
use crate::construction::GrowingBounds;

/// How many extremum-then-blur passes make up the filter. One box blur has a
/// corner in its velocity; chaining three approximates a Gaussian closely
/// enough that neither velocity nor acceleration visibly steps.
const PASSES: usize = 3;

/// Where the camera should sit on each frame, already smoothed.
pub struct CameraPath {
    boxes: Vec<GrowingBounds>,
}

impl CameraPath {
    /// Smooths `bounds` into one box per frame.
    ///
    /// `radius` is the half-width of a single pass, in frames; see
    /// `smoothing_radius`. Zero leaves the requirement untouched, which is how the
    /// old snapping behaviour is asked for.
    ///
    /// `opening` fills the leading frames where nothing has been built yet,
    /// where `growing_bounds_per_frame` has no box to give. The whole-sequence
    /// fit belongs there: it contains every frame by definition, so the filter
    /// turns it into a wide establishing shot that eases down into the first
    /// build site instead of hard cutting to it. That descent trails the first
    /// entity rather than anticipating it, unlike every later move: coming in
    /// early is precisely what `window_extremum` refuses to allow.
    pub fn smooth(bounds: &[Option<GrowingBounds>], opening: Option<GrowingBounds>, radius: usize) -> CameraPath {
        // Nothing was ever built and there is no opening box either, so there
        // is no box to put anywhere. The caller keeps whatever camera it had.
        let Some(fallback) = opening.or_else(|| bounds.iter().flatten().copied().next()) else {
            return CameraPath { boxes: Vec::new() };
        };

        let (mut lo, mut hi): (Vec<Vec2>, Vec<Vec2>) =
            bounds.iter().map(|b| b.unwrap_or(fallback)).map(|b| (b.min(), b.max())).unzip();
        for _ in 0..PASSES {
            if radius == 0 {
                break;
            }
            lo = blur(&window_extremum(&lo, radius, Corner::Min), radius);
            hi = blur(&window_extremum(&hi, radius, Corner::Max), radius);
        }

        CameraPath { boxes: lo.iter().zip(&hi).map(|(&min, &max)| GrowingBounds::from_min_max(min, max)).collect() }
    }

    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// The smoothed box for a frame. Resolution independent, so a caller that
    /// wants to draw it (a preview of the export's framing, say) can.
    pub fn box_at(&self, index: usize) -> Option<GrowingBounds> {
        self.boxes.get(index).copied()
    }

    /// The camera for a frame, fitted to the output being rendered.
    /// `framing` is the caller's, matching whatever auto-follow uses, so a
    /// smoothed export frames a moment the same way browsing to it does.
    pub fn camera_at(&self, index: usize, screen_width: f32, screen_height: f32, framing: Framing) -> Option<Camera> {
        self.box_at(index).map(|b| Camera::fit_framed(b.center, b.half_extent * 2.0, screen_width, screen_height, framing))
    }
}

/// The half-width of one pass, from a duration in seconds.
///
/// Expressed against the export's own frame rate so the feel is the same at
/// any of them: a radius fixed in frames would make a 60 fps render twice as
/// tight as a 30 fps one. `PASSES` chained passes reach `PASSES * radius`
/// frames to each side, so the requested window is split between them.
pub fn smoothing_radius(smooth_secs: f32, fps: u32) -> usize {
    if smooth_secs <= 0.0 {
        return 0;
    }
    // At least one frame: a smoothing duration shorter than a couple of frames
    // was still a request for smoothing, and rounding it away to a snap is not
    // what anyone asking for it meant.
    (((smooth_secs * fps as f32) / (2.0 * PASSES as f32)).round() as usize).max(1)
}

#[derive(Clone, Copy)]
enum Corner {
    Min,
    Max,
}

/// The most outward value within `radius` frames either side.
///
/// This is what keeps the smoothed box outside the required one everywhere.
/// A blurred value is an average over a window, and every entry in that window
/// has already taken its extremum over a window that includes the center, so
/// every term of the average is at least as far out as the requirement there,
/// and so is the average. That holds for any input, monotone or not, and it
/// composes across passes, so all three chained still contain the box. A plain
/// blur has no such property: it undershoots mid-transition and clips whatever
/// was just built, which is the one thing an auto-following camera must not do.
fn window_extremum(values: &[Vec2], radius: usize, corner: Corner) -> Vec<Vec2> {
    (0..values.len())
        .map(|i| {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius).min(values.len() - 1);
            let window = values[lo..=hi].iter().copied();
            match corner {
                Corner::Min => window.reduce(Vec2::min),
                Corner::Max => window.reduce(Vec2::max),
            }
            .expect("the window always contains at least its own center")
        })
        .collect()
}

/// A normalized box blur of half-width `radius`.
///
/// Summed directly rather than through prefix sums: the window is small next
/// to the frame count, so this is the same O(n * radius) the extremum beside
/// it already costs, and a prefix sum over thousands of frames of world
/// coordinates loses tiles' worth of precision in `f32`.
fn blur(values: &[Vec2], radius: usize) -> Vec<Vec2> {
    let n = values.len() as isize;
    (0..values.len())
        .map(|i| {
            let (mut x, mut y) = (0.0f64, 0.0f64);
            for offset in -(radius as isize)..=(radius as isize) {
                // Clamp-extended at the ends rather than zero-padded, which
                // would drag the first and last seconds of the video toward
                // the origin.
                let j = (i as isize + offset).clamp(0, n - 1) as usize;
                x += values[j].x as f64;
                y += values[j].y as f64;
            }
            let count = (2 * radius + 1) as f64;
            Vec2::new((x / count) as f32, (y / count) as f32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(center: Vec2, half: f32) -> Option<GrowingBounds> {
        Some(GrowingBounds { center, half_extent: Vec2::splat(half) })
    }

    /// A base that grows only at one moment, the shape that snaps worst.
    fn step_sequence(len: usize, at: usize) -> Vec<Option<GrowingBounds>> {
        (0..len).map(|i| if i < at { bounds(Vec2::ZERO, 10.0) } else { bounds(Vec2::splat(300.0), 300.0) }).collect()
    }

    fn contains(outer: GrowingBounds, inner: GrowingBounds) -> bool {
        let slack = 1e-3;
        outer.min().x <= inner.min().x + slack
            && outer.min().y <= inner.min().y + slack
            && outer.max().x >= inner.max().x - slack
            && outer.max().y >= inner.max().y - slack
    }

    /// The whole point of the extremum pass. Checked against a deliberately
    /// non-monotone sequence, since the containment argument does not rest on
    /// the box only ever growing, and a future recency-windowed follow mode
    /// would hand this one that shrinks.
    #[test]
    fn every_smoothed_box_still_contains_what_it_has_to_show() {
        // A fixed LCG rather than a rand dependency: reproducible, and the
        // point is coverage of awkward shapes, not statistical quality.
        let mut seed = 0x2545F491u32;
        let mut next = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 24) as f32
        };
        let required: Vec<Option<GrowingBounds>> =
            (0..400).map(|_| bounds(Vec2::new(next() * 800.0 - 400.0, next() * 800.0 - 400.0), 5.0 + next() * 200.0)).collect();

        let path = CameraPath::smooth(&required, None, 9);
        for (i, want) in required.iter().enumerate() {
            let got = path.box_at(i).unwrap();
            assert!(contains(got, want.unwrap()), "frame {i}: {got:?} does not contain {:?}", want.unwrap());
        }
    }

    /// The symptom being fixed: one frame's worth of camera movement must
    /// never again be the entire move.
    #[test]
    fn a_step_is_spread_out_instead_of_teleporting() {
        let required = step_sequence(200, 100);
        let snapped = CameraPath::smooth(&required, None, 0);
        let smoothed = CameraPath::smooth(&required, None, 9);

        let biggest_jump = |path: &CameraPath| {
            (1..path.len())
                .map(|i| (path.box_at(i).unwrap().center - path.box_at(i - 1).unwrap().center).length())
                .fold(0.0f32, f32::max)
        };
        let (before, after) = (biggest_jump(&snapped), biggest_jump(&smoothed));
        assert!(before > 400.0, "the unsmoothed path should teleport, got {before}");
        assert!(after < before / 20.0, "the smoothed path still jumps {after} against {before}");
    }

    /// A camera that invents drift over a base nobody is extending would be
    /// worse than the snap it replaced.
    #[test]
    fn a_base_that_stops_growing_gets_a_still_camera() {
        let required: Vec<Option<GrowingBounds>> = (0..50).map(|_| bounds(Vec2::new(7.0, -3.0), 40.0)).collect();
        let path = CameraPath::smooth(&required, None, 6);
        for i in 0..required.len() {
            let got = path.box_at(i).unwrap();
            assert!((got.center - Vec2::new(7.0, -3.0)).length() < 1e-3, "frame {i} drifted to {:?}", got.center);
            assert!((got.half_extent.x - 40.0).abs() < 1e-3, "frame {i} rezoomed to {:?}", got.half_extent);
        }
    }

    /// Radius is in frames but the setting is in seconds, so the same move has
    /// to take the same time at any frame rate. Measured as when the zoom-out
    /// is nine tenths done, in seconds.
    #[test]
    fn the_move_takes_the_same_time_at_any_frame_rate() {
        let settle_secs = |fps: u32| {
            let required = step_sequence(20 * fps as usize, 10 * fps as usize);
            let path = CameraPath::smooth(&required, None, smoothing_radius(1.5, fps));
            let target = path.box_at(path.len() - 1).unwrap().half_extent.x;
            let settled = (0..path.len()).find(|&i| path.box_at(i).unwrap().half_extent.x >= target * 0.9).unwrap();
            settled as f32 / fps as f32
        };
        let (thirty, sixty) = (settle_secs(30), settle_secs(60));
        assert!((thirty - sixty).abs() < 0.1, "30 fps settled at {thirty}s, 60 fps at {sixty}s");
    }

    /// The frames before the first thing is built. They open on the
    /// whole-sequence box and should ease down from it, not sit there and then
    /// cut.
    ///
    /// Unlike a move outward, this one cannot start early: pulling in ahead of
    /// time is exactly what the extremum pass exists to forbid. So what is
    /// checked is that the descent is spread out, not that it is anticipated.
    #[test]
    fn leading_frames_open_on_the_establishing_box_and_ease_in() {
        let radius = 8;
        let opening = GrowingBounds { center: Vec2::ZERO, half_extent: Vec2::splat(2000.0) };
        let mut required = vec![None; 30];
        required.extend((0..170).map(|_| bounds(Vec2::splat(50.0), 20.0)));

        let path = CameraPath::smooth(&required, Some(opening), radius);
        assert!(path.box_at(0).unwrap().half_extent.x > 1500.0, "should open wide");
        assert!(path.box_at(199).unwrap().half_extent.x < 60.0, "should end on the base");

        // Spread over at least the filter's own reach, rather than dropping in
        // a handful of frames, which is what a cut would look like.
        let total = path.box_at(0).unwrap().half_extent.x - path.box_at(199).unwrap().half_extent.x;
        let moving = (1..path.len())
            .filter(|&i| (path.box_at(i - 1).unwrap().half_extent.x - path.box_at(i).unwrap().half_extent.x) > total * 0.001)
            .count();
        assert!(moving >= PASSES * radius, "the descent took only {moving} frames");
    }

    #[test]
    fn a_zero_radius_reproduces_the_unsmoothed_fit_exactly() {
        let required = step_sequence(20, 10);
        let path = CameraPath::smooth(&required, None, 0);
        for (i, want) in required.iter().enumerate() {
            assert_eq!(path.box_at(i).unwrap(), want.unwrap(), "frame {i}");
        }
    }

    #[test]
    fn nothing_built_and_nothing_to_open_on_gives_an_empty_path() {
        let path = CameraPath::smooth(&[None; 10], None, 5);
        assert!(path.is_empty());
        let framing = Framing { min_size_tiles: 6.0, margin: 0.92, bottom_inset: 0.0 };
        assert!(path.camera_at(0, 1920.0, 1080.0, framing).is_none());
    }

    #[test]
    fn an_empty_sequence_does_not_panic() {
        assert!(CameraPath::smooth(&[], None, 5).is_empty());
    }
}
