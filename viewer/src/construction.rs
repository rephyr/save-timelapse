//! Tracking how much of the base has ever been built, so the camera can follow
//! it: a bounding box over every entity seen so far, extended and never shrunk.
//!
//! Mirrors TLBE's own "base" tracker, so the camera zooms out to keep the
//! factory in frame rather than chasing individual build sites.
//!
//! Entities only, unlike `Camera::fit_frames`'s initial view: tiles include
//! natural terrain covering a margin around the base, so counting them would
//! track how much of the map has been revealed. Trees, cliffs, resources,
//! nests and worms are excluded for the same reason despite being entities:
//! all of them sit wherever the map generated them, and nests cover every
//! generated chunk in every direction.
//!
//! ## Not everything built is worth aiming at
//!
//! Excluding what the map generated is not sufficient, because the remaining
//! offenders are genuinely player built. On a real 860k entity megabase the
//! four entities defining the box were a gun turret, two stone walls and a
//! rail chain signal: the defended perimeter and rail outposts, enclosing a
//! great deal of empty land. No filtering by prototype fixes that, a wall
//! being exactly as player built as an assembler.
//!
//! So the box is taken over where the buildings are, not over their extremes.
//! Counts go into a per-axis histogram of chunk-sized bins, and each end walks
//! inward while what it gives up stays inside a small budget. Not a percentile
//! filter with extra steps: **empty bins are free**, so the walk crosses the
//! gap between a perimeter and the factory at no cost and halts at the first
//! real density.
//!
//! Per axis rather than per cell is what keeps a real second base safe. A
//! per-cell threshold would have to decide whether a remote outpost is dense
//! enough to keep; projecting onto each axis only asks how much is standing
//! beyond a line, and a second base is far too much to give up.

use macroquad::math::Vec2;

use crate::registry::TypeRegistry;
use crate::render_frame::{FrameSequence, RenderEntity, RenderFrame};

/// Side of one histogram bin, in tiles. A Factorio chunk, which the rest of
/// the viewer already thinks in and is comfortably larger than any single
/// building.
const DENSITY_BIN_TILES: f32 = 32.0;

/// How much of what is standing may be trimmed off each end of each axis,
/// as a fraction of everything counted.
///
/// Half a percent per side is not timid, because empty bins cost nothing to
/// trim: the walk crosses the entire gap between a perimeter and the factory
/// for free, so the budget only has to cover the thin outlying structure.
///
/// On a real 860k entity megabase the wall line holds about 3,100 entities
/// against a 4,300 budget, and the first genuine factory bin holds thousands
/// in one bin, which stops the walk immediately.
const DENSITY_TRIM: f32 = 0.005;

/// A world-space box: a center and half-extent, the shape `Camera::fit_bounds`
/// wants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowingBounds {
    pub center: Vec2,
    pub half_extent: Vec2,
}

/// Everything in one frame that counts as construction. Filtered per run
/// rather than per entity, so the test happens tens of times per frame rather
/// than hundreds of thousands, which is what makes walking twice affordable.
fn counted<'a>(frame: &'a RenderFrame, registry: &'a TypeRegistry) -> impl Iterator<Item = &'a RenderEntity> + 'a {
    frame
        .entity_runs
        .iter()
        .filter(move |run| registry.is_built(run.type_id))
        .flat_map(move |run| frame.entities[run.range()].iter())
}

/// First and last surviving bin after trimming up to `budget` off each end.
/// Stops at the first bin it cannot afford.
///
/// The two walks get a budget each rather than sharing one, answering the same
/// question independently at opposite ends, and neither may cross the other: a
/// frame of nothing but outliers keeps its whole range rather than inverting.
fn trim_ends(bins: &[u32], budget: u32) -> (usize, usize) {
    let (mut lo, mut hi) = (0usize, bins.len() - 1);

    let mut spent = 0u32;
    while lo < hi && spent + bins[lo] <= budget {
        spent += bins[lo];
        lo += 1;
    }

    let mut spent = 0u32;
    while hi > lo && spent + bins[hi] <= budget {
        spent += bins[hi];
        hi -= 1;
    }

    (lo, hi)
}

/// The box worth aiming a camera at for one frame, which is not the box
/// containing everything built. `None` for a frame with nothing built.
///
/// Two passes: the raw extent has to be known before there is anywhere to put
/// bins, and the total before a fraction of it means anything. A hash map
/// keyed by bin would fold them into one and be slower, hashing every entity
/// costing more than a second ordered read.
fn frame_bounds(frame: &RenderFrame, registry: &TypeRegistry) -> Option<(Vec2, Vec2)> {
    let mut extent: Option<(Vec2, Vec2)> = None;
    let mut total: u32 = 0;
    for e in counted(frame, registry) {
        let half = Vec2::new(e.w as f32, e.h as f32) / 2.0;
        let center = Vec2::new(e.x, e.y);
        extent = Some(match extent {
            None => (center - half, center + half),
            Some((lo, hi)) => (lo.min(center - half), hi.max(center + half)),
        });
        total += 1;
    }
    let (lo, hi) = extent?;

    let bin_count = |span: f32| ((span / DENSITY_BIN_TILES).ceil() as usize).max(1);
    let (nx, ny) = (bin_count(hi.x - lo.x), bin_count(hi.y - lo.y));
    let (mut xs, mut ys) = (vec![0u32; nx], vec![0u32; ny]);
    for e in counted(frame, registry) {
        // Binned by center, while `lo`/`hi` come from footprint corners, so
        // the index is clamped rather than trusted to land in range.
        xs[(((e.x - lo.x) / DENSITY_BIN_TILES) as usize).min(nx - 1)] += 1;
        ys[(((e.y - lo.y) / DENSITY_BIN_TILES) as usize).min(ny - 1)] += 1;
    }

    // Rounds down, so a frame small enough for the budget to floor to zero
    // keeps exactly the extent it always had. Nothing that fits comfortably
    // on screen is worth second-guessing.
    let budget = (total as f32 * DENSITY_TRIM) as u32;
    let (x0, x1) = trim_ends(&xs, budget);
    let (y0, y1) = trim_ends(&ys, budget);

    // Clamped back to the real extent: the last bin on each axis is a
    // partial one, so its far edge generally overshoots what is standing.
    let at = |start: f32, bin: usize| start + bin as f32 * DENSITY_BIN_TILES;
    Some((
        Vec2::new(at(lo.x, x0).max(lo.x), at(lo.y, y0).max(lo.y)),
        Vec2::new(at(lo.x, x1 + 1).min(hi.x), at(lo.y, y1 + 1).min(hi.y)),
    ))
}

fn union_min_max(a: (Vec2, Vec2), b: (Vec2, Vec2)) -> (Vec2, Vec2) {
    (a.0.min(b.0), a.1.max(b.1))
}

/// The bounding box of everything built by each frame, unioned with every
/// prior frame's so it only grows. One entry per frame; `None` only for a
/// leading run with nothing built.
///
/// Precomputed across the whole sequence at load rather than during playback:
/// O(total entities) once, not redone on every step.
pub fn growing_bounds_per_frame(frames: &FrameSequence, registry: &TypeRegistry) -> Vec<Option<GrowingBounds>> {
    let mut result = Vec::with_capacity(frames.len());
    let mut running: Option<(Vec2, Vec2)> = None;
    frames.for_each_frame(|_, frame, repeat| {
        // A repeat holds what the previous frame held, so its box cannot move
        // the one already running. On a long capture most frames are repeats,
        // and each would otherwise walk every entity to rediscover a box it
        // already has.
        if !repeat {
            running = match (running, frame_bounds(frame, registry)) {
                (Some(r), Some(f)) => Some(union_min_max(r, f)),
                (Some(r), None) => Some(r),
                (None, other) => other,
            };
        }
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
        let frames = vec![render(vec![entity("a", 0.0, 0.0), entity("b", 10.0, 10.0)], Vec::new(), &mut registry)];
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

    /// With terrain capture on, tiles include ground covering a margin around
    /// the base, which would hold the camera on a wide empty view.
    #[test]
    fn growing_bounds_ignores_tiles_entirely() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(vec![entity("a", 0.0, 0.0)], vec![Tile { n: "grass".into(), x: 5000, y: 5000 }], &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "the far away terrain tile must not affect the box");
    }

    #[test]
    fn growing_bounds_is_none_for_a_frame_with_only_tiles_and_no_entities() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(Vec::new(), vec![Tile { n: "concrete".into(), x: 50, y: 50 }], &mut registry)];
        assert!(growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].is_none());
    }

    /// With terrain capture on, trees and cliffs are captured as entities
    /// scattered across that same margin, which made the tracked area "how
    /// much of the map has been revealed".
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

    /// With include-resources on, a distant crude oil deposit pulled the box
    /// towards it even on a fresh save with one compact starter cluster.
    #[test]
    fn growing_bounds_ignores_resource_deposits() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(vec![entity("a", 0.0, 0.0), entity("crude-oil", -151.0, -195.0)], Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "the distant oil deposit must not affect the box");
    }

    /// Nests and worms are kept in the capture on purpose, so they were the
    /// last unbuilt thing the box counted. They cover every generated chunk in
    /// every direction, so the box spanned the explored map.
    #[test]
    fn growing_bounds_ignores_enemy_nests_and_worms() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(
            vec![entity("a", 0.0, 0.0), entity("biter-spawner", 4000.0, 4000.0), entity("medium-worm-turret", -4000.0, -4000.0)],
            Vec::new(),
            &mut registry,
        )];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "distant nests and worms must not affect the box");
    }

    /// A captive biter spawner is player placed, so it counts as construction.
    /// The one name `is_enemy` excepts, which is only worth anything if this
    /// file honours it too.
    #[test]
    fn growing_bounds_counts_a_captive_biter_spawner() {
        let mut registry = TypeRegistry::new();
        let frames =
            vec![render(vec![entity("a", 0.0, 0.0), entity("captive-biter-spawner", 100.0, 0.0)], Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(50.0, 0.0), "a captive spawner is built, so it must count");
    }

    /// A compact block of `count` 1x1 entities, one per tile, filling as
    /// square a patch as it can from `(ox, oy)`. Dense enough that the trim
    /// cannot afford to touch it.
    fn cluster(name: &str, count: usize, ox: f32, oy: f32) -> Vec<Entity> {
        let side = (count as f32).sqrt().ceil() as usize;
        (0..count).map(|i| entity(name, ox + (i % side) as f32, oy + (i / side) as f32)).collect()
    }

    /// The measured case: on a real megabase the box was defined by a gun
    /// turret, two stone walls and a rail signal, none of which is scenery
    /// and all of which sat far outside the factory.
    #[test]
    fn growing_bounds_is_not_dragged_out_by_a_thin_outlying_structure() {
        let mut registry = TypeRegistry::new();
        let mut entities = cluster("assembling-machine-1", 2000, 0.0, 0.0);
        entities.push(entity("gun-turret", 4000.0, 0.0));
        entities.push(entity("rail-chain-signal", 0.0, 4000.0));
        let frames = vec![render(entities, Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert!(bounds.half_extent.x < 200.0, "the distant turret must not set the box, got {:?}", bounds);
        assert!(bounds.half_extent.y < 200.0, "the distant rail signal must not set the box, got {:?}", bounds);
    }

    /// The other half of the same rule, and the one that makes it safe: a
    /// genuine second base is far too much to give up, however far away it
    /// is, so the box has to stretch to reach it.
    #[test]
    fn growing_bounds_still_reaches_a_real_second_base() {
        let mut registry = TypeRegistry::new();
        let mut entities = cluster("assembling-machine-1", 500, 0.0, 0.0);
        entities.extend(cluster("electric-furnace", 500, 4000.0, 0.0));
        let frames = vec![render(entities, Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert!(bounds.half_extent.x > 1900.0, "both bases must be framed, got {:?}", bounds);
    }

    /// A parked train is wherever it stopped, not where anything was built,
    /// and in a from-saves export that is somewhere different in every save.
    #[test]
    fn growing_bounds_ignores_trains_and_vehicles() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(
            vec![
                entity("a", 0.0, 0.0),
                entity("locomotive", 3000.0, 0.0),
                entity("cargo-wagon", 3007.0, 0.0),
                entity("spidertron", 0.0, -3000.0),
            ],
            Vec::new(),
            &mut registry,
        )];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "a train parked far away must not set the box");
    }

    /// The rails themselves are exactly what a timelapse should follow out
    /// to a new outpost, so the vehicle filter must not touch them.
    #[test]
    fn growing_bounds_still_follows_the_rail_network() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(vec![entity("a", 0.0, 0.0), entity("straight-rail", 1000.0, 0.0)], Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert!(bounds.half_extent.x > 400.0, "rail out to an outpost is construction, got {:?}", bounds);
    }

    /// The trim budget rounds down, so anything small enough to frame
    /// comfortably keeps the exact extent it always had. Without this, every
    /// existing expectation about small inputs would quietly shift.
    #[test]
    fn growing_bounds_leaves_a_small_base_exactly_as_it_was() {
        let mut registry = TypeRegistry::new();
        let frames = vec![render(vec![entity("a", 0.0, 0.0), entity("b", 40.0, 40.0)], Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(20.0, 20.0), "two entities cannot afford any trim at all");
    }

    #[test]
    fn growing_bounds_becomes_some_once_something_is_finally_built() {
        let mut registry = TypeRegistry::new();
        let frames =
            vec![render(Vec::new(), Vec::new(), &mut registry), render(vec![entity("a", 1.0, 1.0)], Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry);
        assert!(bounds[0].is_none());
        assert!(bounds[1].is_some());
    }
}
