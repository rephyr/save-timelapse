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
//!
//! Enemy nests and worm turrets are excluded for that same reason a third
//! time (see `registry::is_enemy`). They are kept in the capture because
//! they are stationary, so the format represents them honestly and clearing
//! them is worth watching, but a spawner sits wherever the map generated it,
//! exactly like a tree or an ore patch. Counting them was the worst case of
//! the three: nests cover every generated chunk in every direction, so the
//! tracked box spanned the whole explored map rather than the factory, and
//! since the box's midpoint is what the camera centers on, it centered on
//! the middle of the revealed map instead of on anything built.
//!
//! ## Not everything built is worth aiming at
//!
//! Excluding what the map generated is necessary and turned out not to be
//! sufficient, because the remaining offenders are genuinely player built.
//! Measured on a real 860k entity megabase, the four entities defining the
//! box were a gun turret, two stone walls and a rail chain signal: the
//! *defended perimeter and the rail outposts*, enclosing a great deal of
//! land nobody built anything on. The factory filled well under half of the
//! resulting frame, and no amount of filtering by prototype fixes that,
//! since a wall is exactly as player built as an assembling machine.
//!
//! So the box is taken over where the buildings actually *are*, not over
//! their extremes. Entity counts go into a per-axis histogram of chunk-sized
//! bins, and each end is walked inward while the entities given up stay
//! inside a small budget. That is deliberately not a percentile filter with
//! extra steps: the point is that **empty bins are free**, so the walk
//! crosses the whole gap between a perimeter and the factory at no cost and
//! halts the instant it meets real density. A thin wall line is affordable;
//! the first genuine factory bin never is.
//!
//! Being per axis rather than per cell is what keeps a real second base
//! safe. A cell-by-cell density threshold would have to decide whether a
//! remote outpost is dense enough to keep, and would get it wrong in both
//! directions. Projecting onto each axis asks only how much is standing
//! beyond a given line, and a second base is far too much to give up
//! whatever its shape.

use macroquad::math::Vec2;

use crate::registry::TypeRegistry;
use crate::render_frame::{FrameSequence, RenderEntity, RenderFrame};

/// Side of one bin in the density histograms, in tiles. A Factorio chunk,
/// the granularity the rest of the viewer already thinks in (see
/// `LOD_CELL_TILES`) and comfortably larger than any single building, so no
/// one machine straddles enough bins to matter.
const DENSITY_BIN_TILES: f32 = 32.0;

/// How much of what is standing may be trimmed off each end of each axis
/// before the box is taken, as a fraction of everything counted.
///
/// Half a percent per side sounds timid and is not, because **empty bins
/// cost nothing to trim**. The walk inward stops at the first bin it cannot
/// afford, so it crosses the entire empty gap between a perimeter and the
/// factory for free and halts the moment it reaches real density. What the
/// budget actually has to cover is only the thin outlying structure itself.
///
/// Measured against a real 860k entity megabase: its wall line holds about
/// 3,100 entities against a 4,300 budget, so the perimeter is trimmed and
/// the first genuine factory bin (thousands in one bin) stops the walk
/// immediately. Raising this much further would start eating factory, and
/// there is no need: the gap, not the budget, is what does the work.
const DENSITY_TRIM: f32 = 0.005;

/// A world-space box: a center and half-extent, the shape `Camera::fit_bounds`
/// wants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowingBounds {
    pub center: Vec2,
    pub half_extent: Vec2,
}

/// Everything in one frame that counts as construction: not terrain
/// scatter, not a resource deposit, not an enemy structure (see the module
/// doc comment). Filtering per *run* rather than per entity is why this is
/// cheap enough to walk twice below: a run is a whole contiguous span of one
/// prototype, so the name test happens tens of times per frame rather than
/// hundreds of thousands.
fn counted<'a>(frame: &'a RenderFrame, registry: &'a TypeRegistry) -> impl Iterator<Item = &'a RenderEntity> + 'a {
    frame
        .entity_runs
        .iter()
        .filter(move |run| {
            let id = run.type_id;
            !registry.is_terrain_scatter(id) && !registry.is_resource(id) && !registry.is_enemy(id) && !registry.is_vehicle(id)
        })
        .flat_map(move |run| frame.entities[run.range()].iter())
}

/// First and last surviving bin, after trimming up to `budget` items off
/// each end. Stops at the first bin it cannot afford, so a run of empty
/// bins is crossed for free and dense ones halt it immediately.
///
/// The two walks share a budget each rather than one between them, since
/// they are answering the same question independently at opposite ends, and
/// neither may cross the other: a frame holding nothing but outliers keeps
/// its whole range rather than inverting into an empty box.
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

/// The box worth aiming a camera at for one frame, which is not the same as
/// the box containing everything built (see the module doc comment). `None`
/// for a frame with nothing built yet.
///
/// Two passes over the frame's entities: the raw extent has to be known
/// before there is anywhere to put bins, and the total has to be known
/// before a fraction of it means anything. A hash map keyed by bin index
/// would fold them into one pass and be slower, since hashing every entity
/// costs more than reading them a second time in order.
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
    frames.for_each_frame(|_, frame, repeat| {
        // A repeat holds exactly what the previous frame held, so its box is
        // the box already running and unioning it in cannot move it. Skipping
        // the scan is the whole saving: on a long capture most frames are
        // repeats, and each one otherwise walks every entity on the surface
        // to rediscover a box it already has.
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

    /// With terrain capture on, tiles include natural ground covering a
    /// margin around the base, which is map revealed for context, not
    /// something the player built. Letting that count would hold the camera
    /// on a wide, mostly-empty view instead of tracking the actual buildings.
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
        let frames = vec![render(vec![entity("a", 0.0, 0.0), entity("crude-oil", -151.0, -195.0)], Vec::new(), &mut registry)];
        let bounds = growing_bounds_per_frame(&FrameSequence::new(frames).unwrap(), &registry)[0].unwrap();
        assert_eq!(bounds.center, Vec2::new(0.0, 0.0), "the distant oil deposit must not affect the box");
    }

    /// The worst of the three, and the one this file missed for longest:
    /// nests and worms are kept in the capture on purpose, so they are the
    /// only unbuilt thing left that the box still counted. They cover every
    /// generated chunk in every direction, so the box spanned the explored
    /// map and the camera centered on the middle of that rather than on the
    /// factory.
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

    /// A captive biter spawner is player placed (Space Age captivity), so it
    /// is construction like anything else and has to keep counting. This is
    /// the one name `is_enemy` deliberately excepts, and the exception is
    /// only worth anything if this file honours it too.
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
