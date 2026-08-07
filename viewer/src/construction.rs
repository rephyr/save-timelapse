//! Detecting where a base is actively being built, so the camera can follow
//! it: diffing consecutive frames for newly-appeared entities, grouping
//! those into spatially separate build sites, chaining each site's
//! per-frame appearances into an episode spanning its whole active
//! lifetime, and picking which episode to lock onto when more than one is
//! active at once.
//!
//! The episode step matters because the viewer has the entire frame
//! sequence in hand before playback ever starts -- unlike a live game
//! camera, this doesn't have to react frame by frame and guess how much
//! bigger a build is going to get. Chaining first means the camera can lock
//! onto a site already knowing its full eventual extent, so it eases to the
//! right distance once and holds, instead of creeping wider every time
//! another piece gets added to the same project.

use std::collections::{BTreeMap, HashSet};

use macroquad::math::Vec2;

use crate::registry::TypeId;
use crate::render_frame::RenderFrame;

/// One spatially distinct group of newly built entities: a box the camera
/// can frame, plus a weight to prefer among several when there's no player
/// position to break the tie with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuildSite {
    pub center: Vec2,
    pub half_extent: Vec2,
    pub new_entity_count: u32,
}

/// Identity for diffing across frames. There's no stable entity id in the
/// format, so this stands in for one: `(type, x, y)`, quantized to tenths of
/// a tile to match the precision `src/frame.rs` already rounds positions to
/// before writing them, so this is an exact integer comparison rather than a
/// fragile float one.
type EntityKey = (TypeId, i32, i32);

fn key(type_id: TypeId, x: f32, y: f32) -> EntityKey {
    (type_id, (x * 10.0).round() as i32, (y * 10.0).round() as i32)
}

/// Positions (center, half footprint) of entities in `curr` that weren't in
/// `prev` under the same type at the same spot. Tiles are deliberately left
/// out: paving and landfill are high volume and not what "building" means
/// here.
fn new_entities(prev: &RenderFrame, curr: &RenderFrame) -> Vec<(Vec2, Vec2)> {
    let mut seen: HashSet<EntityKey> = HashSet::with_capacity(prev.entities.len());
    for run in &prev.entity_runs {
        for e in &prev.entities[run.range()] {
            seen.insert(key(run.type_id, e.x, e.y));
        }
    }

    let mut added = Vec::new();
    for run in &curr.entity_runs {
        for e in &curr.entities[run.range()] {
            if !seen.contains(&key(run.type_id, e.x, e.y)) {
                added.push((Vec2::new(e.x, e.y), Vec2::new(e.w as f32, e.h as f32) / 2.0));
            }
        }
    }
    added
}

/// Groups newly built entities into spatially separate sites: bucket each
/// one's center into a `cell_tiles`-square grid cell, then flood fill over
/// occupied neighboring cells (8-connected) to merge everything that's part
/// of one contiguous build into a single site. Single pass, no iterative
/// convergence, good enough for "is this the same construction project or a
/// different one."
pub fn cluster_new_construction(prev: &RenderFrame, curr: &RenderFrame, cell_tiles: f32) -> Vec<BuildSite> {
    let entities = new_entities(prev, curr);
    if entities.is_empty() {
        return Vec::new();
    }

    let cell_of = |p: Vec2| -> (i32, i32) { ((p.x / cell_tiles).floor() as i32, (p.y / cell_tiles).floor() as i32) };

    let mut buckets: BTreeMap<(i32, i32), Vec<usize>> = BTreeMap::new();
    for (i, (pos, _)) in entities.iter().enumerate() {
        buckets.entry(cell_of(*pos)).or_default().push(i);
    }

    let mut visited: HashSet<(i32, i32)> = HashSet::new();
    let mut sites = Vec::new();
    for &start in buckets.keys() {
        if visited.contains(&start) {
            continue;
        }
        let mut stack = vec![start];
        let mut component = Vec::new();
        visited.insert(start);
        while let Some(cell) = stack.pop() {
            if let Some(indices) = buckets.get(&cell) {
                component.extend(indices.iter().copied());
            }
            for dx in -1..=1 {
                for dy in -1..=1 {
                    let neighbor = (cell.0 + dx, cell.1 + dy);
                    if (dx != 0 || dy != 0) && buckets.contains_key(&neighbor) && visited.insert(neighbor) {
                        stack.push(neighbor);
                    }
                }
            }
        }

        let mut min = entities[component[0]].0 - entities[component[0]].1;
        let mut max = entities[component[0]].0 + entities[component[0]].1;
        for &i in &component {
            let (pos, half) = entities[i];
            min = min.min(pos - half);
            max = max.max(pos + half);
        }
        sites.push(BuildSite {
            center: (min + max) / 2.0,
            half_extent: (max - min) / 2.0,
            new_entity_count: component.len() as u32,
        });
    }
    sites
}

/// One entry per frame: `result[0]` is always empty (no prior frame to diff
/// against), `result[i]` is what's new in frame `i` versus frame `i - 1`.
/// Precomputed once here, across the whole sequence, rather than during
/// playback, for the same reason the chunk-LOD pass in `render_frame.rs` is:
/// redoing an O(entities) diff on every step of fast autoplay would cost as
/// much as loading the sequence itself, once a second, for as long as it plays.
pub fn build_sites_per_frame(frames: &[RenderFrame], cell_tiles: f32) -> Vec<Vec<BuildSite>> {
    let mut result = Vec::with_capacity(frames.len());
    if !frames.is_empty() {
        result.push(Vec::new());
    }
    for pair in frames.windows(2) {
        result.push(cluster_new_construction(&pair[0], &pair[1], cell_tiles));
    }
    result
}

/// One construction project's whole active lifetime: the frame index range
/// it was growing across, and its final, fully-grown bounding box. `end` is
/// the last frame it actually grew on, not padded by however much quiet
/// gap it took to notice it was done -- that gap only controls *chaining*
/// (see `build_episodes`), it isn't part of the result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuildEpisode {
    pub start_index: usize,
    pub end_index: usize,
    pub site: BuildSite,
}

impl BuildEpisode {
    fn is_active_at(&self, index: usize) -> bool {
        self.start_index <= index && index <= self.end_index
    }
}

/// Whether a newly-appeared site is close enough to an in-progress episode's
/// current box to be the same ongoing project: an axis-aligned overlap test
/// after inflating the episode's box by `margin` in every direction, so a
/// base's next addition merges even when it lands just outside the box built
/// so far rather than only when it overlaps exactly.
fn close_enough(episode: BuildSite, addition: BuildSite, margin: f32) -> bool {
    let e_min = episode.center - episode.half_extent - Vec2::splat(margin);
    let e_max = episode.center + episode.half_extent + Vec2::splat(margin);
    let a_min = addition.center - addition.half_extent;
    let a_max = addition.center + addition.half_extent;
    e_min.x <= a_max.x && e_max.x >= a_min.x && e_min.y <= a_max.y && e_max.y >= a_min.y
}

fn union(a: BuildSite, b: BuildSite) -> BuildSite {
    let min = (a.center - a.half_extent).min(b.center - b.half_extent);
    let max = (a.center + a.half_extent).max(b.center + b.half_extent);
    BuildSite { center: (min + max) / 2.0, half_extent: (max - min) / 2.0, new_entity_count: a.new_entity_count + b.new_entity_count }
}

/// Chains each frame's build sites (from [`build_sites_per_frame`]) into
/// episodes spanning their whole active lifetime. A new site merges into
/// whichever in-progress episode it's `close_enough` to (growing that
/// episode's box); one that doesn't match any starts a new episode. An
/// episode stops accepting merges once `quiet_frames` have passed with
/// nothing new nearby -- so resuming work at the same spot much later
/// starts a fresh episode rather than reopening a long-finished one.
pub fn build_episodes(per_frame_sites: &[Vec<BuildSite>], merge_margin_tiles: f32, quiet_frames: usize) -> Vec<BuildEpisode> {
    struct Thread {
        start_index: usize,
        last_active_index: usize,
        site: BuildSite,
    }

    let mut active: Vec<Thread> = Vec::new();
    let mut finished: Vec<BuildEpisode> = Vec::new();

    for (index, sites) in per_frame_sites.iter().enumerate() {
        for &new_site in sites {
            match active.iter_mut().find(|t| close_enough(t.site, new_site, merge_margin_tiles)) {
                Some(thread) => {
                    thread.site = union(thread.site, new_site);
                    thread.last_active_index = index;
                }
                None => active.push(Thread { start_index: index, last_active_index: index, site: new_site }),
            }
        }

        let (still_active, done): (Vec<_>, Vec<_>) =
            active.into_iter().partition(|t| index - t.last_active_index < quiet_frames);
        active = still_active;
        finished.extend(
            done.into_iter().map(|t| BuildEpisode { start_index: t.start_index, end_index: t.last_active_index, site: t.site }),
        );
    }
    finished.extend(
        active.into_iter().map(|t| BuildEpisode { start_index: t.start_index, end_index: t.last_active_index, site: t.site }),
    );
    finished.sort_by_key(|e| e.start_index);
    finished
}

/// Which episode the camera should be locked onto at `index`, given
/// wherever it's currently locked. Sticky: if the current lock is still
/// growing (active at `index`), it's kept even when another episode is
/// active too -- that's what makes the camera hold on one build instead of
/// chasing whatever the player touches next. A lock that's no longer active
/// is only replaced once something else actually is; with nothing active at
/// all, the current lock (possibly `None`) just holds, on a finished
/// project's final framing rather than snapping back to a full-base fit.
pub fn choose_lock(episodes: &[BuildEpisode], index: usize, players: &[Vec2], current: Option<usize>) -> Option<usize> {
    if let Some(lock) = current {
        if episodes.get(lock).is_some_and(|e| e.is_active_at(index)) {
            return current;
        }
    }
    let active: Vec<usize> = (0..episodes.len()).filter(|&i| episodes[i].is_active_at(index)).collect();
    if active.is_empty() {
        return current;
    }
    if players.is_empty() {
        return active.into_iter().max_by_key(|&i| episodes[i].site.new_entity_count);
    }
    active.into_iter().min_by(|&a, &b| {
        nearest_player_dist_sq(&episodes[a].site, players).total_cmp(&nearest_player_dist_sq(&episodes[b].site, players))
    })
}

fn nearest_player_dist_sq(site: &BuildSite, players: &[Vec2]) -> f32 {
    players.iter().map(|&p| (p - site.center).length_squared()).fold(f32::INFINITY, f32::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TypeRegistry;
    use save_timelapse::frame::{Entity, Frame};

    fn entity(n: &str, x: f32, y: f32) -> Entity {
        Entity { n: n.into(), x, y, d: 0, w: 1, h: 1 }
    }

    fn render(entities: Vec<Entity>, registry: &mut TypeRegistry) -> RenderFrame {
        RenderFrame::from_frame(
            Frame { tick: 0, surface: "nauvis".to_string(), count: entities.len(), entities, tiles: Vec::new() },
            registry,
        )
    }

    #[test]
    fn new_entities_finds_only_additions() {
        let mut registry = TypeRegistry::new();
        let prev = render(vec![entity("belt", 0.0, 0.0)], &mut registry);
        let curr = render(vec![entity("belt", 0.0, 0.0), entity("belt", 5.0, 0.0)], &mut registry);
        let added = new_entities(&prev, &curr);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].0, Vec2::new(5.0, 0.0));
    }

    #[test]
    fn new_entities_ignores_removals() {
        let mut registry = TypeRegistry::new();
        let prev = render(vec![entity("belt", 0.0, 0.0), entity("belt", 5.0, 0.0)], &mut registry);
        let curr = render(vec![entity("belt", 0.0, 0.0)], &mut registry);
        assert!(new_entities(&prev, &curr).is_empty());
    }

    #[test]
    fn new_entities_treats_a_type_change_at_the_same_spot_as_new() {
        let mut registry = TypeRegistry::new();
        let prev = render(vec![entity("belt", 0.0, 0.0)], &mut registry);
        let curr = render(vec![entity("pipe", 0.0, 0.0)], &mut registry);
        let added = new_entities(&prev, &curr);
        assert_eq!(added.len(), 1, "a different type at the same position is a real change, not persistence");
    }

    #[test]
    fn cluster_merges_nearby_new_entities_into_one_site() {
        let mut registry = TypeRegistry::new();
        let prev = render(Vec::new(), &mut registry);
        let curr = render(vec![entity("belt", 0.0, 0.0), entity("belt", 2.0, 0.0)], &mut registry);
        let sites = cluster_new_construction(&prev, &curr, 32.0);
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].new_entity_count, 2);
    }

    #[test]
    fn cluster_splits_distant_new_entities_into_separate_sites() {
        let mut registry = TypeRegistry::new();
        let prev = render(Vec::new(), &mut registry);
        let curr = render(vec![entity("belt", 0.0, 0.0), entity("belt", 500.0, 500.0)], &mut registry);
        let sites = cluster_new_construction(&prev, &curr, 32.0);
        assert_eq!(sites.len(), 2);
    }

    #[test]
    fn cluster_returns_empty_when_nothing_is_new() {
        let mut registry = TypeRegistry::new();
        let prev = render(vec![entity("belt", 0.0, 0.0)], &mut registry);
        let curr = render(vec![entity("belt", 0.0, 0.0)], &mut registry);
        assert!(cluster_new_construction(&prev, &curr, 32.0).is_empty());
    }

    #[test]
    fn build_sites_per_frame_has_one_entry_per_frame_with_the_first_empty() {
        let mut registry = TypeRegistry::new();
        let frames =
            vec![render(Vec::new(), &mut registry), render(vec![entity("belt", 0.0, 0.0)], &mut registry)];
        let sites = build_sites_per_frame(&frames, 32.0);
        assert_eq!(sites.len(), 2);
        assert!(sites[0].is_empty());
        assert_eq!(sites[1].len(), 1);
    }

    fn site(center: Vec2, count: u32) -> BuildSite {
        BuildSite { center, half_extent: Vec2::splat(1.0), new_entity_count: count }
    }

    #[test]
    fn build_episodes_chains_nearby_additions_across_frames_into_one_growing_episode() {
        // Same neighborhood, three frames running: this should read as one
        // continuous project, not three separate blips.
        let per_frame = vec![
            vec![site(Vec2::new(0.0, 0.0), 1)],
            vec![site(Vec2::new(5.0, 0.0), 1)],
            vec![site(Vec2::new(10.0, 0.0), 1)],
        ];
        let episodes = build_episodes(&per_frame, 32.0, 2);
        assert_eq!(episodes.len(), 1);
        assert_eq!((episodes[0].start_index, episodes[0].end_index), (0, 2));
        assert_eq!(episodes[0].site.new_entity_count, 3, "the box's growth should accumulate every addition");
        // The final box must cover every addition, not just the latest one.
        assert!(episodes[0].site.half_extent.x >= 5.0, "box should span from x=0 to x=10");
    }

    #[test]
    fn build_episodes_splits_a_long_quiet_gap_into_separate_episodes() {
        let per_frame = vec![
            vec![site(Vec2::new(0.0, 0.0), 1)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![site(Vec2::new(0.0, 0.0), 1)],
        ];
        let episodes = build_episodes(&per_frame, 32.0, 2);
        assert_eq!(episodes.len(), 2, "a 3-frame gap exceeds quiet_frames=2, so this must not chain");
    }

    #[test]
    fn build_episodes_splits_simultaneous_distant_activity_into_separate_episodes() {
        let per_frame = vec![vec![site(Vec2::new(0.0, 0.0), 1), site(Vec2::new(1000.0, 1000.0), 1)]];
        let episodes = build_episodes(&per_frame, 32.0, 2);
        assert_eq!(episodes.len(), 2);
    }

    #[test]
    fn build_episodes_on_all_quiet_frames_returns_nothing() {
        let per_frame = vec![Vec::new(), Vec::new()];
        assert!(build_episodes(&per_frame, 32.0, 2).is_empty());
    }

    fn episode(start: usize, end: usize, center: Vec2, count: u32) -> BuildEpisode {
        BuildEpisode { start_index: start, end_index: end, site: site(center, count) }
    }

    #[test]
    fn choose_lock_stays_on_the_current_episode_while_it_is_still_active() {
        let near_player = episode(0, 5, Vec2::new(0.0, 0.0), 1);
        let bigger_elsewhere = episode(0, 5, Vec2::new(1000.0, 1000.0), 50);
        let episodes = [near_player, bigger_elsewhere];
        // Locked onto index 0 (the smaller one); a bigger, equally active
        // episode existing at the same time must not steal the lock.
        assert_eq!(choose_lock(&episodes, 3, &[], Some(0)), Some(0));
    }

    #[test]
    fn choose_lock_picks_the_player_nearest_active_episode_when_unlocked() {
        let near = episode(0, 5, Vec2::new(0.0, 0.0), 1);
        let far_but_bigger = episode(0, 5, Vec2::new(1000.0, 1000.0), 50);
        let episodes = [near, far_but_bigger];
        let player = Vec2::new(1.0, 1.0);
        assert_eq!(choose_lock(&episodes, 0, &[player], None), Some(0));
    }

    #[test]
    fn choose_lock_falls_back_to_the_biggest_active_episode_with_no_players() {
        let small = episode(0, 5, Vec2::new(0.0, 0.0), 1);
        let big = episode(0, 5, Vec2::new(1000.0, 1000.0), 50);
        let episodes = [small, big];
        assert_eq!(choose_lock(&episodes, 0, &[], None), Some(1));
    }

    #[test]
    fn choose_lock_holds_the_finished_lock_when_nothing_else_is_active() {
        let only = episode(0, 2, Vec2::ZERO, 1);
        // Past index 2, `only` is no longer active and nothing replaces it.
        assert_eq!(choose_lock(&[only], 5, &[], Some(0)), Some(0));
    }

    #[test]
    fn choose_lock_switches_once_the_old_lock_ends_and_something_new_starts() {
        let first = episode(0, 2, Vec2::ZERO, 1);
        let second = episode(3, 5, Vec2::new(50.0, 50.0), 1);
        let episodes = [first, second];
        assert_eq!(choose_lock(&episodes, 4, &[], Some(0)), Some(1));
    }
}
