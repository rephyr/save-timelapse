//! Tracking how much of the base has ever been built, so the camera can
//! follow it: a monotonically growing bounding box over every entity that
//! has appeared in any frame so far, extended (never shrunk) as the base
//! grows.
//!
//! This mirrors TLBE (the most-downloaded Factorio timelapse mod)'s own
//! "base" tracker, which extends its tracked area on every new entity built
//! and never shrinks it back down when something is later removed, so the
//! camera gradually zooms out to keep the whole factory in frame as it
//! grows, rather than chasing individual build sites around the map. A
//! smarter, more targeted follow mode (favoring wherever's currently being
//! built, or wherever the player is) is a real possibility for later, but
//! this simpler whole-base version is the one that actually matches the
//! reference this was asked to match.
//!
//! Entities only, deliberately unlike `Camera::fit_frames`'s initial static
//! view, which also considers tiles: TLBE's own tracker reacts only to
//! entities being built, never to tiles, and tiles here include natural
//! terrain (when terrain capture is on) covering a margin around the base,
//! rendered for context, not something the player built. Including it would
//! make "how much has been built" track how much of the map has been
//! revealed instead, holding the camera on a wide, mostly-empty view instead
//! of the actual buildings.
//!
//! Trees and cliffs are excluded for the same reason, even though they're
//! entities, not tiles: with terrain capture on they're decorative scatter
//! covering that same margin, naturally spread across a wide area
//! independent of anything the player placed (see
//! `registry::is_terrain_scatter`). Resource deposits (ore, oil) are
//! excluded for the same reason again: with include-resources on, a
//! resource sits wherever the map generated it, and a distant oil field can
//! pull the tracked area out toward it just as easily as a distant tree (see
//! `registry::is_resource`).

use macroquad::math::Vec2;

use crate::registry::{is_resource, is_terrain_scatter, TypeRegistry};
use crate::render_frame::{FrameSequence, RenderFrame};

/// A world-space box: a center and half-extent, the shape `Camera::fit_bounds`
/// wants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowingBounds {
    pub center: Vec2,
    pub half_extent: Vec2,
}

/// Corner-to-corner bounding box of every entity's footprint in one frame,
/// skipping terrain scatter and resource deposits (see the module doc
/// comment). `None` for a frame with nothing else built yet.
fn frame_bounds(frame: &RenderFrame, registry: &TypeRegistry) -> Option<(Vec2, Vec2)> {
    let mut points = frame
        .entity_runs
        .iter()
        .filter(|run| {
            let name = registry.name(run.type_id);
            !is_terrain_scatter(name) && !is_resource(name)
        })
        .flat_map(|run| frame.entities[run.range()].iter())
        .flat_map(|e| {
            let half = Vec2::new(e.w as f32, e.h as f32) / 2.0;
            let center = Vec2::new(e.x, e.y);
            [center - half, center + half]
        });

    let first = points.next()?;
    let (mut min, mut max) = (first, first);
    for p in points {
        min = min.min(p);
        max = max.max(p);
    }
    Some((min, max))
}

fn union_min_max(a: (Vec2, Vec2), b: (Vec2, Vec2)) -> (Vec2, Vec2) {
    (a.0.min(b.0), a.1.max(b.1))
}

/// The bounding box of everything built by each frame, unioned with every
/// prior frame's box so it only ever grows. See the module doc comment.
/// One entry per frame; `None` only for a leading run of frames with
/// nothing built yet.
///
/// Precomputed once here, across the whole sequence, rather than during
/// playback, for the same reason the chunk-LOD pass in `render_frame.rs` is:
/// this is O(total entities across the whole sequence) done once at load,
/// not redone on every step of playback.
pub fn growing_bounds_per_frame(frames: &FrameSequence, registry: &TypeRegistry) -> Vec<Option<GrowingBounds>> {
    let mut result = Vec::with_capacity(frames.len());
    let mut running: Option<(Vec2, Vec2)> = None;
    frames.for_each_frame(|_, frame| {
        running = match (running, frame_bounds(frame, registry)) {
            (Some(r), Some(f)) => Some(union_min_max(r, f)),
            (Some(r), None) => Some(r),
            (None, other) => other,
        };
        result.push(running.map(|(min, max)| GrowingBounds { center: (min + max) / 2.0, half_extent: (max - min) / 2.0 }));
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TypeRegistry;
    use save_timelapse::frame::{Entity, Frame, Tile};

    fn entity(n: &str, x: f32, y: f32) -> Entity {
        Entity { n: n.into(), x, y, d: 0, w: 1, h: 1 }
    }

    fn render(entities: Vec<Entity>, tiles: Vec<Tile>, registry: &mut TypeRegistry) -> RenderFrame {
        RenderFrame::from_frame(
            Frame { tick: 0, surface: "nauvis".to_string(), count: entities.len(), entities, tiles },
            registry,
        )
    }

    #[test]
    fn growing_bounds_is_none_while_nothing_has_been_built() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(Vec::new(), Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry);
        assert_eq!(bounds, vec![None]);
    }

    #[test]
    fn growing_bounds_covers_the_first_frames_entities() {
        let mut registry = TypeRegistry::new();
        let frames =
            vec![render(vec![entity("a", 0.0, 0.0), entity("b", 10.0, 10.0)], Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(5.0, 5.0));
    }

    #[test]
    fn growing_bounds_extends_to_cover_a_later_addition() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            render(vec![entity("a", 0.0, 0.0)], Vec::new(), &mut registry),
            render(vec![entity("a", 0.0, 0.0), entity("b", 100.0, 0.0)], Vec::new(), &mut registry),
        ];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry);
        assert!(bounds[1].unwrap().half_extent.x > bounds[0].unwrap().half_extent.x, "the box must grow");
    }

    /// The whole point: this never shrinks back down, even though the frame
    /// itself lost the entity that had pushed the box out that far.
    #[test]
    fn growing_bounds_does_not_shrink_when_something_is_removed() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            render(vec![entity("a", 0.0, 0.0), entity("b", 100.0, 0.0)], Vec::new(), &mut registry),
            render(vec![entity("a", 0.0, 0.0)], Vec::new(), &mut registry),
        ];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry);
        assert_eq!(bounds[0], bounds[1], "removing b must not shrink the tracked area");
    }

    /// With terrain capture on, tiles include natural ground covering a
    /// margin around the base, which is map revealed for context, not
    /// something the player built. Letting that count would hold the camera
    /// on a wide, mostly-empty view instead of tracking the actual buildings.
    #[test]
    fn growing_bounds_ignores_tiles_entirely() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(
            vec![entity("a", 0.0, 0.0)],
            vec![Tile { n: "grass".into(), x: 5000, y: 5000 }],
            &mut registry,
        )];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "the far away terrain tile must not affect the box");
    }

    #[test]
    fn growing_bounds_is_none_for_a_frame_with_only_tiles_and_no_entities() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(Vec::new(), vec![Tile { n: "concrete".into(), x: 50, y: 50 }], &mut registry)];
        assert!(growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].is_none());
    }

    /// The bug this guards against: with terrain capture on, trees and
    /// cliffs are captured as entities (not tiles), scattered across that
    /// same margin independent of anything the player placed. Without this
    /// exclusion the tracked area became "how much of the map has been
    /// revealed" instead of "where the buildings are".
    #[test]
    fn growing_bounds_ignores_trees_and_cliffs() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(
            vec![entity("a", 0.0, 0.0), entity("tree-01", 5000.0, 5000.0), entity("cliff", -5000.0, -5000.0)],
            Vec::new(),
            &mut registry,
        )];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "the distant tree and cliff must not affect the box");
    }

    #[test]
    fn growing_bounds_is_none_for_a_frame_with_only_terrain_scatter() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(vec![entity("tree-01", 0.0, 0.0)], Vec::new(), &mut registry)];
        assert!(growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].is_none());
    }

    /// The real bug this guards against: with include-resources on, a
    /// distant crude oil deposit pulled the tracked box out towards it even
    /// on a fresh save with a single compact starter cluster and nothing
    /// else built anywhere near the oil field.
    #[test]
    fn growing_bounds_ignores_resource_deposits() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(
            vec![entity("a", 0.0, 0.0), entity("crude-oil", -151.0, -195.0)],
            Vec::new(),
            &mut registry,
        )];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "the distant oil deposit must not affect the box");
    }

    #[test]
    fn growing_bounds_becomes_some_once_something_is_finally_built() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            render(Vec::new(), Vec::new(), &mut registry),
            render(vec![entity("a", 1.0, 1.0)], Vec::new(), &mut registry),
        ];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry);
        assert!(bounds[0].is_none());
        assert!(bounds[1].is_some());
    }
}
