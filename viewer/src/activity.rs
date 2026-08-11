//! How much got built at each point in the timelapse, for the activity graph
//! drawn along the scrub bar.
//!
//! Frames are snapshots, not change lists, so "what got built here" has to be
//! recovered by diffing consecutive frames rather than read off. Done once at
//! load, next to `construction::growing_bounds_per_frame`, which already
//! walks every entity of every frame for the camera.
//!
//! Diffed against the previous frame only, not against everything ever seen.
//! The two differ when something is torn down and rebuilt on the same spot,
//! and counting that again is right: rebuilding is activity, and the point of
//! this graph is to show when the player was working, not to count distinct
//! positions ever occupied.

use crate::registry::TypeRegistry;
use crate::render_frame::FrameSequence;

/// Entity positions are aligned to a tenth of a tile, the same fixed point
/// `save_timelapse::world::pos_key` keys by on the replay side. Comparing the
/// raw f32s instead would make a position that survived a format round trip
/// with a one-ulp difference read as a brand new entity.
///
/// Packed into one integer rather than kept as an `(i32, i32)` tuple, so a
/// frame's positions sort as plain numbers. The exact packing does not matter
/// as long as it is injective; only equality and a total order are ever used.
fn pos_key(x: f32, y: f32) -> u64 {
    pack(((x as f64) * 10.0).round() as i32, ((y as f64) * 10.0).round() as i32)
}

/// World tiles per heatmap cell. Deliberately coarser than
/// `render_frame::LOD_CELL_TILES`: that one exists to collapse chunks
/// without visibly changing the picture, where this one wants blobs big
/// enough to read as a region of activity rather than a scatter of
/// individual machines.
pub const HEAT_CELL_TILES: i32 = 8;

/// Tenths of a tile per cell, matching the fixed point `pos_key` packs.
const HEAT_CELL_TENTHS: i32 = HEAT_CELL_TILES * 10;

/// One cell of the construction heatmap: cell coordinates, not world ones,
/// so multiply by `HEAT_CELL_TILES` to place it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeatCell {
    pub x: i32,
    pub y: i32,
    pub built: u32,
}

/// Everything the activity pass produces: one walk over the frames answers
/// both "when was the player working" (the timeline graph) and "where"
/// (the map heatmap), since both are the same set of newly built entities
/// counted two different ways.
pub struct Activity {
    /// New entities per frame, index-aligned with the frames.
    pub counts: Vec<usize>,
    /// Where those went, binned per frame. Sized by distinct positions ever
    /// built rather than frames times entities, since a given spot is new
    /// exactly once (bar a rebuild), so this stays a few MB even on a
    /// megabase.
    pub cells: Vec<Vec<HeatCell>>,
    /// The busiest single cell of any single frame, so the heatmap can scale
    /// against a fixed reference. Scaling against whatever is on screen
    /// instead would make the same construction change brightness as
    /// unrelated activity elsewhere came and went.
    pub peak_cell: u32,
}

/// How many entities appeared in each frame that were not in the one before
/// it, index-aligned with `frames`.
///
/// Frame 0 is always 0. Everything in it appeared before the capture was
/// watching: for a live capture that is the whole baseline snapshot of an
/// existing base, which would otherwise be a spike hundreds of times taller
/// than any real construction and flatten the entire rest of the graph.
///
/// Terrain scatter and resource deposits are skipped for the same reason
/// `construction.rs` skips them: with terrain capture on, trees and ore are
/// scattered across a wide margin around the base by map generation rather
/// than built by anyone, and a frame that merely revealed more map would
/// otherwise read as the busiest construction moment of the run.
///
/// Sorted vectors rather than the hash set this obviously wants, because
/// this runs on the load path against every entity of every frame. Measured
/// on a 150-frame, 400k-entity sequence (see `bench` below), where the bare
/// walk over those same entities costs 295ms in `growing_bounds_per_frame`:
/// a `HashSet` per frame took 2.7s, and swapping in a cheap multiply hasher
/// only brought that to 2.3s, because the cost was never the hash function.
/// It was 30 million random-access probes and inserts into a table far
/// larger than cache. Sorting and merging touches the same data almost
/// entirely sequentially, which the prefetcher handles, and needs no
/// allocation per frame once the two buffers have grown: 1.07s for the same
/// sequence, which is the sort itself and close enough to the unavoidable
/// walk to stop optimizing.
pub fn analyze_activity(frames: &FrameSequence, registry: &TypeRegistry) -> Activity {
    let mut counts = vec![0usize; frames.len()];
    let mut cells: Vec<Vec<HeatCell>> = vec![Vec::new(); frames.len()];
    let mut peak_cell = 0;

    let (mut previous, mut current): (Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new());
    let mut built: Vec<u64> = Vec::new();

    frames.for_each_frame(|index, frame, repeat| {
        // A repeat is identical to the frame before it, so nothing was built
        // between them: the count is zero and the heat is empty, both known
        // without looking. `previous` is deliberately left alone, since it
        // already holds this frame's positions too, which is what lets the
        // next real frame diff correctly across the whole gap.
        //
        // Everything below (materialise, sort, dedup, merge over every entity
        // on the surface) would otherwise run per repeat to reach the same
        // answer, and on a long capture repeats are most of the frames.
        if repeat {
            return;
        }
        current.clear();
        for run in &frame.entity_runs {
            if registry.is_terrain_scatter(run.type_id) || registry.is_resource(run.type_id) {
                continue;
            }
            current.extend(frame.entities[run.range()].iter().map(|e| pos_key(e.x, e.y)));
        }
        current.sort_unstable();
        // Two entities sharing a position should count once, matching how
        // the replay world keys entities by position in the first place.
        current.dedup();

        if index > 0 {
            built.clear();
            collect_absent_from(&current, &previous, &mut built);
            counts[index] = built.len();
            cells[index] = bin_into_cells(&built);
            peak_cell = peak_cell.max(cells[index].iter().map(|c| c.built).max().unwrap_or(0));
        }
        std::mem::swap(&mut previous, &mut current);
    });

    Activity { counts, cells, peak_cell }
}

/// Appends every element of `current` missing from `previous` to `out`, both
/// inputs sorted ascending.
///
/// One merge walk rather than a binary search per element: the two runs are
/// almost identical frame to frame, so `previous` advances in step with
/// `current` and the whole thing stays a linear scan over two sorted arrays.
fn collect_absent_from(current: &[u64], previous: &[u64], out: &mut Vec<u64>) {
    let mut p = 0;
    for &key in current {
        while p < previous.len() && previous[p] < key {
            p += 1;
        }
        if p < previous.len() && previous[p] == key {
            p += 1;
        } else {
            out.push(key);
        }
    }
}

/// Groups newly built positions into `HEAT_CELL_TILES`-square cells.
///
/// Sorted and run-length counted rather than accumulated into a map, for the
/// same reason the diff above is a merge: this runs per frame over every new
/// entity, and the input is already a sorted `Vec` sitting in cache.
fn bin_into_cells(built: &[u64]) -> Vec<HeatCell> {
    let mut keys: Vec<u64> = built
        .iter()
        .map(|&key| {
            let (qx, qy) = unpack(key);
            // Euclidean, not truncating: plain division rounds toward zero,
            // which would fold the cells either side of an axis into one and
            // put a seam through the middle of any base built around origin.
            pack(qx.div_euclid(HEAT_CELL_TENTHS), qy.div_euclid(HEAT_CELL_TENTHS))
        })
        .collect();
    keys.sort_unstable();

    let mut cells: Vec<HeatCell> = Vec::new();
    for key in keys {
        let (x, y) = unpack(key);
        match cells.last_mut() {
            Some(last) if last.x == x && last.y == y => last.built += 1,
            _ => cells.push(HeatCell { x, y, built: 1 }),
        }
    }
    cells
}

fn pack(x: i32, y: i32) -> u64 {
    ((x as u32 as u64) << 32) | (y as u32 as u64)
}

fn unpack(key: u64) -> (i32, i32) {
    ((key >> 32) as u32 as i32, key as u32 as i32)
}

/// The heat visible at `index`, as `(cell x, cell y, intensity)` with
/// intensity already scaled to 0..1.
///
/// Two things happen here that the raw per-frame cells do not do on their own.
///
/// **A window with falloff.** The last `window` frames contribute, weighted
/// down by age, so the glow trails the construction front and dies out behind
/// it rather than accumulating into a map of everywhere you have ever built.
///
/// **Spatial spread.** Each cell bleeds into its neighbours out to `spread`
/// cells, falling off with distance. Without this the overlay is a scatter of
/// isolated lit squares wherever individual machines happen to sit, which
/// reads as noise rather than as a region being worked on: what a player
/// means by "I am building over there" is an area, not a set of points. The
/// kernel deliberately does not normalize to a sum of 1. A cell surrounded by
/// other construction therefore collects from all of its neighbours and
/// saturates, while an isolated machine out on its own stays dim, which is
/// what makes density read as heat instead of merely spreading a fixed amount
/// of ink more thinly.
///
/// `view` bounds the work to what is on screen (in cell coordinates, inclusive,
/// already grown by `spread` at the caller). Skipping it is correct but does
/// the spreading for the whole base every rendered frame, which on a megabase
/// mid-blueprint is tens of thousands of cells against a screenful of a few
/// hundred.
pub fn recent_heat(
    per_frame: &[Vec<HeatCell>],
    index: usize,
    window: usize,
    spread: i32,
    peak: u32,
    view: Option<(i32, i32, i32, i32)>,
) -> Vec<(i32, i32, f32)> {
    if peak == 0 || window == 0 {
        return Vec::new();
    }

    let visible = |x: i32, y: i32| match view {
        Some((min_x, min_y, max_x, max_y)) => x >= min_x && x <= max_x && y >= min_y && y <= max_y,
        None => true,
    };

    let mut source: std::collections::HashMap<(i32, i32), f32> = std::collections::HashMap::new();
    for age in 0..window {
        let Some(frame) = index.checked_sub(age).and_then(|i| per_frame.get(i)) else { break };
        let weight = 1.0 - (age as f32 / window as f32);
        for cell in frame {
            if !visible(cell.x, cell.y) {
                continue;
            }
            *source.entry((cell.x, cell.y)).or_insert(0.0) += cell.built as f32 * weight;
        }
    }

    let reach = spread.max(0);
    let mut field: std::collections::HashMap<(i32, i32), f32> = std::collections::HashMap::new();
    for (&(cx, cy), &value) in &source {
        for dy in -reach..=reach {
            for dx in -reach..=reach {
                let distance = ((dx * dx + dy * dy) as f32).sqrt();
                if distance > reach as f32 {
                    continue; // round blobs, not squares
                }
                let falloff = 1.0 - distance / (reach as f32 + 1.0);
                *field.entry((cx + dx, cy + dy)).or_insert(0.0) += value * falloff;
            }
        }
    }

    // Rooted, and against the busiest single cell of the whole run rather
    // than of this moment: scaling to whatever is currently on screen would
    // make identical construction change brightness as unrelated activity
    // elsewhere scrolled in and out of view.
    let scale = (peak as f32).sqrt();
    field
        .into_iter()
        .map(|((x, y), value)| (x, y, (value.sqrt() / scale).clamp(0.0, 1.0)))
        .filter(|&(_, _, intensity)| intensity > 0.0)
        .collect()
}

/// Activity scaled to 0..1 against the busiest frame, for drawing.
///
/// Square rooted rather than linear. One blueprint landing a few thousand
/// entities in a single frame is routine late in a playthrough, and against a
/// linear scale that one frame takes the full height while every ordinary
/// hour of hand building underneath it rounds to a flat line, which is
/// exactly backwards for a graph whose job is showing when you were working.
/// The root keeps the ordering intact (it is monotonic) while pulling the
/// outlier back into the same picture as everything else.
///
/// All zeros for a capture with no construction in it at all, rather than a
/// division by zero.
pub fn activity_heights(activity: &[usize]) -> Vec<f32> {
    let peak = activity.iter().copied().max().unwrap_or(0);
    if peak == 0 {
        return vec![0.0; activity.len()];
    }
    let scale = (peak as f32).sqrt();
    activity.iter().map(|&count| (count as f32).sqrt() / scale).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_frame::{FrameSequence, RenderEntity, RenderFrame, Run};

    /// Builds a frame holding `positions` of one type, which is all this
    /// module looks at.
    fn frame_of(registry: &mut TypeRegistry, name: &str, positions: &[(f32, f32)]) -> RenderFrame {
        let type_id = registry.intern(name);
        let entities: Vec<RenderEntity> =
            positions.iter().map(|&(x, y)| RenderEntity { x, y, w: 1, h: 1, d: 0, shape: 0 }).collect();
        RenderFrame {
            tick: 0,
            count: entities.len(),
            entity_runs: vec![Run { type_id, start: 0, end: entities.len() as u32 }],
            entities,
            tiles: Vec::new(),
            tile_runs: Vec::new(),
            tile_lod: Vec::new(),
            tile_lod_runs: Vec::new(),
            entity_lod: Vec::new(),
            entity_lod_runs: Vec::new(),
            tile_bounds: None,
        }
    }

    /// Wraps built frames into the sequence the pass now reads, which owns
    /// them as spans rather than keeping the vec.
    fn seq(frames: Vec<RenderFrame>) -> FrameSequence {
        FrameSequence::new(frames).expect("tests always build at least one frame")
    }

    #[test]
    fn the_first_frame_is_never_activity() {
        let mut registry = TypeRegistry::new();
        let frames = vec![frame_of(&mut registry, "transport-belt", &[(0.5, 0.5), (1.5, 0.5)])];
        assert_eq!(analyze_activity(&seq(frames), &registry).counts, vec![0]);
    }

    #[test]
    fn only_positions_absent_from_the_previous_frame_count() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5)]),
            // One carried over, two new.
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5), (1.5, 0.5), (2.5, 0.5)]),
        ];
        assert_eq!(analyze_activity(&seq(frames), &registry).counts, vec![0, 2]);
    }

    /// Tearing something down is not construction, so a frame that only lost
    /// entities is a quiet frame rather than a busy one.
    #[test]
    fn removals_do_not_register_as_activity() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5), (1.5, 0.5), (2.5, 0.5)]),
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5)]),
        ];
        assert_eq!(analyze_activity(&seq(frames), &registry).counts, vec![0, 0]);
    }

    /// Diffing against the previous frame rather than against everything ever
    /// seen: rebuilding on a cleared spot is work, and should read as work.
    #[test]
    fn rebuilding_on_a_previously_cleared_spot_counts_again() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5)]),
            frame_of(&mut registry, "transport-belt", &[]),
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5)]),
        ];
        assert_eq!(analyze_activity(&seq(frames), &registry).counts, vec![0, 0, 1]);
    }

    /// The bug this guards: with terrain capture on, a frame that merely
    /// revealed more map would otherwise be the busiest moment of the run.
    #[test]
    fn trees_and_ore_appearing_are_not_construction() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5)]),
            frame_of(&mut registry, "tree-01", &[(50.5, 50.5), (51.5, 50.5)]),
            frame_of(&mut registry, "iron-ore", &[(60.5, 60.5), (61.5, 60.5)]),
        ];
        assert_eq!(analyze_activity(&seq(frames), &registry).counts, vec![0, 0, 0]);
    }

    /// Everything built in one 8-tile square collapses to a single cell
    /// carrying the count, which is what makes the heatmap a few hundred
    /// quads instead of a few hundred thousand.
    #[test]
    fn nearby_construction_bins_into_one_cell() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            frame_of(&mut registry, "transport-belt", &[]),
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5), (1.5, 0.5), (7.5, 7.5)]),
        ];
        let activity = analyze_activity(&seq(frames), &registry);
        assert_eq!(activity.cells[1], vec![HeatCell { x: 0, y: 0, built: 3 }]);
        assert_eq!(activity.peak_cell, 3);
    }

    /// The seam this guards: truncating division rounds toward zero, so
    /// cells at -1 and +1 tiles would both land in cell 0 and every base
    /// built around the origin would show a cross-shaped artifact.
    #[test]
    fn cells_either_side_of_the_origin_stay_separate() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            frame_of(&mut registry, "transport-belt", &[]),
            frame_of(&mut registry, "transport-belt", &[(-0.5, -0.5), (0.5, 0.5)]),
        ];
        let cells = &analyze_activity(&seq(frames), &registry).cells[1];
        assert_eq!(cells.len(), 2, "got {cells:?}");
        assert!(cells.contains(&HeatCell { x: -1, y: -1, built: 1 }));
        assert!(cells.contains(&HeatCell { x: 0, y: 0, built: 1 }));
    }

    #[test]
    fn separate_regions_get_separate_cells() {
        let mut registry = TypeRegistry::new();
        let frames = vec![
            frame_of(&mut registry, "transport-belt", &[]),
            frame_of(&mut registry, "transport-belt", &[(0.5, 0.5), (100.5, 0.5)]),
        ];
        let cells = &analyze_activity(&seq(frames), &registry).cells[1];
        assert_eq!(cells.len(), 2, "got {cells:?}");
        assert!(cells.iter().all(|c| c.built == 1));
    }

    /// Frame 0 contributes no heat for the same reason it contributes no
    /// count: it is the starting state, not construction anyone watched.
    #[test]
    fn the_first_frame_contributes_no_heat() {
        let mut registry = TypeRegistry::new();
        let frames = vec![frame_of(&mut registry, "transport-belt", &[(0.5, 0.5), (9.5, 9.5)])];
        let activity = analyze_activity(&seq(frames), &registry);
        assert!(activity.cells[0].is_empty());
        assert_eq!(activity.peak_cell, 0);
    }

    /// Builds a per-frame heat list directly, so the spreading tests state
    /// their input rather than deriving it from frames.
    fn heat_of(frames: &[&[(i32, i32, u32)]]) -> Vec<Vec<HeatCell>> {
        frames.iter().map(|cells| cells.iter().map(|&(x, y, built)| HeatCell { x, y, built }).collect()).collect()
    }

    fn intensity_at(field: &[(i32, i32, f32)], x: i32, y: i32) -> f32 {
        field.iter().find(|&&(cx, cy, _)| cx == x && cy == y).map(|&(_, _, i)| i).unwrap_or(0.0)
    }

    /// The point of the spread: one machine built in one cell should light
    /// an area around it, not a single isolated square.
    #[test]
    fn heat_spreads_into_the_area_around_where_building_happened() {
        let heat = heat_of(&[&[], &[(0, 0, 4)]]);
        let field = recent_heat(&heat, 1, 4, 2, 4, None);

        assert!(intensity_at(&field, 0, 0) > 0.0, "the built cell itself must be lit");
        assert!(intensity_at(&field, 1, 0) > 0.0, "a neighbour must pick up heat");
        assert!(intensity_at(&field, 2, 0) > 0.0, "the far edge of the radius must too");
        assert_eq!(intensity_at(&field, 5, 0), 0.0, "well outside the radius stays cold");
    }

    #[test]
    fn heat_falls_off_with_distance_from_the_construction() {
        let heat = heat_of(&[&[], &[(0, 0, 9)]]);
        let field = recent_heat(&heat, 1, 4, 3, 9, None);
        let (core, near, far) = (intensity_at(&field, 0, 0), intensity_at(&field, 1, 0), intensity_at(&field, 3, 0));
        assert!(core > near && near > far, "expected a falloff, got {core} {near} {far}");
    }

    /// Density is the whole point of not normalizing the kernel: a cluster
    /// of construction should burn hotter than the same amount of building
    /// scattered thinly.
    ///
    /// `peak` is deliberately far above any single cell's count here. With a
    /// peak equal to one cell, both arrangements clamp to full brightness
    /// and the comparison proves nothing, which is exactly how this test
    /// first failed.
    #[test]
    fn clustered_construction_burns_hotter_than_the_same_amount_spread_thin() {
        let peak = 400;
        let clustered = heat_of(&[&[], &[(0, 0, 4), (1, 0, 4), (0, 1, 4), (1, 1, 4)]]);
        let scattered = heat_of(&[&[], &[(0, 0, 4), (40, 0, 4), (0, 40, 4), (40, 40, 4)]]);

        let hot = intensity_at(&recent_heat(&clustered, 1, 4, 2, peak, None), 0, 0);
        let thin = intensity_at(&recent_heat(&scattered, 1, 4, 2, peak, None), 0, 0);
        assert!(hot > thin, "clustered {hot} should beat scattered {thin}");
        assert!(hot < 1.0 && thin > 0.0, "neither should be clamped: {hot} {thin}");
    }

    /// Older frames contribute less, so the glow trails the front and dies
    /// out behind it rather than piling up forever.
    #[test]
    fn older_frames_fade_and_leave_the_window_entirely() {
        let heat = heat_of(&[&[], &[(0, 0, 4)], &[], &[], &[], &[], &[]]);
        let fresh = intensity_at(&recent_heat(&heat, 1, 3, 1, 4, None), 0, 0);
        let stale = intensity_at(&recent_heat(&heat, 2, 3, 1, 4, None), 0, 0);
        assert!(fresh > stale, "heat should dim with age: {fresh} then {stale}");
        assert_eq!(intensity_at(&recent_heat(&heat, 6, 3, 1, 4, None), 0, 0), 0.0, "past the window");
    }

    /// Culling is an optimization, so it must not change what is drawn
    /// inside the region asked for.
    #[test]
    fn view_culling_matches_the_unculled_field_inside_the_view() {
        let heat = heat_of(&[&[], &[(0, 0, 5), (30, 30, 5)]]);
        let full = recent_heat(&heat, 1, 4, 2, 5, None);
        let culled = recent_heat(&heat, 1, 4, 2, 5, Some((-4, -4, 4, 4)));

        assert_eq!(intensity_at(&full, 0, 0), intensity_at(&culled, 0, 0));
        assert_eq!(intensity_at(&full, 1, 1), intensity_at(&culled, 1, 1));
        assert!(intensity_at(&full, 30, 30) > 0.0, "the distant cluster exists uncalled");
        assert_eq!(intensity_at(&culled, 30, 30), 0.0, "and is skipped once culled away");
    }

    #[test]
    fn a_run_with_no_construction_produces_no_heat() {
        let heat = heat_of(&[&[], &[]]);
        assert!(recent_heat(&heat, 1, 4, 2, 0, None).is_empty(), "peak of 0 must not divide by zero");
    }

    #[test]
    fn heights_are_normalized_against_the_busiest_frame() {
        let heights = activity_heights(&[0, 25, 100]);
        assert_eq!(heights[0], 0.0);
        assert_eq!(heights[2], 1.0, "the peak fills the graph");
        // Square rooted: 25 is a quarter of 100 but reaches half height.
        assert!((heights[1] - 0.5).abs() < 1e-6, "got {}", heights[1]);
    }

    /// A single blueprint dwarfing everything else must not flatten the rest
    /// of the graph into an unreadable line.
    #[test]
    fn an_outlier_burst_leaves_ordinary_frames_visible() {
        let heights = activity_heights(&[100, 10_000]);
        assert_eq!(heights[1], 1.0);
        assert!(heights[0] > 0.05, "ordinary building rounded away to {}", heights[0]);
    }

    #[test]
    fn a_capture_with_no_construction_does_not_divide_by_zero() {
        assert_eq!(activity_heights(&[0, 0, 0]), vec![0.0, 0.0, 0.0]);
        assert!(activity_heights(&[]).is_empty());
    }

    /// The synthetic frames above prove the rules; this proves the whole
    /// thing actually finds construction in a real exported capture, which
    /// is the part that would make the graph a flat line while every unit
    /// test still passed.
    #[test]
    fn real_exported_fixtures_produce_a_readable_graph() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fixtures/frames");
        let frames = crate::loading::load_sequence(std::path::Path::new(dir)).unwrap();

        let mut registry = TypeRegistry::new();
        let render: Vec<RenderFrame> = frames.into_iter().map(|frame| RenderFrame::from_frame(frame, &mut registry)).collect();

        let activity = analyze_activity(&seq(render), &registry).counts;
        assert_eq!(activity[0], 0, "the first frame is the starting state, not construction");
        assert!(activity[1..].iter().any(|&count| count > 0), "no construction found across five real frames: {activity:?}");

        let heights = activity_heights(&activity);
        assert!(heights.iter().all(|h| (0.0..=1.0).contains(h)), "heights escaped 0..1: {heights:?}");
        assert!(heights.iter().any(|&h| h > 0.0), "a real capture must not draw as a flat line");
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::registry::TypeRegistry;
    use crate::render_frame::{FrameSequence, RenderEntity, RenderFrame, Run};

    /// Where the numbers quoted on `analyze_activity` come from, kept so
    /// they can be re-checked rather than trusted. Prints rather than
    /// asserts: a timing threshold would be flaky on shared or throttled
    /// hardware, and the useful signal is the ratio against
    /// `growing_bounds_per_frame`, which walks the same entities and is
    /// already on the load path.
    ///
    /// `#[ignore]`d because it builds ~30 million entities (roughly half a
    /// gigabyte) and takes a few seconds. Run it, in release, whenever this
    /// module's data structures change:
    ///
    /// ```text
    /// cargo test --release -p viewer --lib measure_cost -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn measure_cost_on_a_megabase_sized_sequence() {
        let per_frame = 400_000usize;
        let frames_n = 150usize;
        let mut registry = TypeRegistry::new();
        let type_id = registry.intern("transport-belt");

        let mut frames = Vec::with_capacity(frames_n);
        for f in 0..frames_n {
            // Each frame keeps everything before it and adds a slice more.
            let n = per_frame * (f + 1) / frames_n;
            let entities: Vec<RenderEntity> = (0..n)
                .map(|i| RenderEntity { x: (i % 2000) as f32 + 0.5, y: (i / 2000) as f32 + 0.5, w: 1, h: 1, d: 0, shape: 0 })
                .collect();
            frames.push(RenderFrame {
                tick: f as u64,
                count: entities.len(),
                entity_runs: vec![Run { type_id, start: 0, end: entities.len() as u32 }],
                entities,
                tiles: Vec::new(),
                tile_runs: Vec::new(),
                tile_lod: Vec::new(),
                tile_lod_runs: Vec::new(),
                entity_lod: Vec::new(),
                entity_lod_runs: Vec::new(),
                tile_bounds: None,
            });
        }
        let total: usize = frames.iter().map(|f| f.entities.len()).sum();
        let sequence = FrameSequence::new(frames).unwrap();
        let start = std::time::Instant::now();
        let activity = analyze_activity(&sequence, &registry).counts;
        let ours = start.elapsed();
        let start = std::time::Instant::now();
        let bounds = crate::construction::growing_bounds_per_frame(&sequence, &registry);
        let theirs = start.elapsed();
        println!("BENCH {frames_n} frames, {total} entity-visits");
        println!("BENCH analyze_activity:         {ours:?}");
        println!("BENCH growing_bounds_per_frame:{theirs:?}  (already on the load path)");
        println!("BENCH tail {:?} bounds {}", &activity[activity.len() - 3..], bounds.len());
    }
}
