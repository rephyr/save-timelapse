//! Mutable world state, built from a baseline snapshot and advanced by
//! replaying the event log.
//!
//! The flow this serves: the mod snapshots a save once, then logs only
//! placements and removals. Reconstructing any moment means taking the
//! baseline and applying every event up to that tick.
//!
//! ## Why application is forgiving
//!
//! The baseline is written incrementally across many ticks, so it is not an
//! atomic picture of one instant: something built while it is being written
//! may or may not appear, depending on whether its surface had already been
//! flushed. Events are logged throughout that window too, so replay can see
//! an add for something already present, or a remove for something it never
//! saw. Both are treated as no-ops rather than errors, which turns that
//! unavoidable smear into a non-problem instead of something the mod would
//! have to freeze the game to prevent.

use std::collections::HashMap;
use std::sync::Arc;

use crate::event::Event;
use crate::frame::{Entity, Frame, Tile};
use crate::names::{NameId, NameTable};

/// Integer position key, so lookups never hash a float or compare with an
/// epsilon.
///
/// Scaled by ten, not two. Half-tile alignment covers most entities but not
/// all: `tests/fixtures/frames/frame_0000.stfr` has a
/// `logistic-train-stop-lamp-control` at x=326.9 sitting beside its
/// `logistic-train-stop` at x=327.0. Keying on half tiles collapsed the two
/// onto one slot and silently dropped one of them, five entities out of
/// that frame's 240. One decimal is exactly the precision entity positions
/// are stored at (see `frame.rs`), so scaling by ten is both lossless and
/// collision free.
///
/// Computed in f64: at Factorio's ±1,000,000 map limit an f32 has too few
/// mantissa bits left to round the scaled value reliably.
type PosKey = (i32, i32);

fn pos_key(x: f32, y: f32) -> PosKey {
    (((x as f64) * 10.0).round() as i32, ((y as f64) * 10.0).round() as i32)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldEntity {
    pub name: NameId,
    pub x: f32,
    pub y: f32,
    pub d: u8,
    pub w: u32,
    pub h: u32,
    pub id: Option<u64>,
}

/// Whether `name` is one of the placed-floor prototypes the mod tracks
/// incrementally (concrete, landfill, and the like), mirroring
/// `mod/encode.lua`'s `PLACED_FLOOR_TILES` list exactly. Anything else that
/// shows up in a baseline's tiles is natural terrain (grass, water, sand,
/// ...), captured once by `save-timelapse-capture-terrain` and never again:
/// Factorio has no event for terrain changing (nobody "builds" grass), so
/// unlike placed floor it needs no ongoing tracking, just a one-time split
/// at baseline load time. See `Surface::terrain`.
fn is_placed_floor(name: &str) -> bool {
    matches!(
        name,
        "stone-path"
            | "concrete"
            | "hazard-concrete-left"
            | "hazard-concrete-right"
            | "refined-concrete"
            | "refined-hazard-concrete-left"
            | "refined-hazard-concrete-right"
            | "landfill"
            | "red-refined-concrete"
            | "green-refined-concrete"
            | "blue-refined-concrete"
            | "orange-refined-concrete"
            | "yellow-refined-concrete"
            | "pink-refined-concrete"
            | "purple-refined-concrete"
            | "black-refined-concrete"
            | "brown-refined-concrete"
            | "cyan-refined-concrete"
            | "acid-refined-concrete"
    )
}

/// One surface's contents.
///
/// Entities live in a slab with free-list reuse, so ids stay stable while
/// entities come and go, iteration stays dense and cache-friendly, and a
/// churn-heavy replay reuses slots instead of growing forever.
#[derive(Debug, Default)]
pub struct Surface {
    slots: Vec<Option<WorldEntity>>,
    free: Vec<usize>,
    by_pos: HashMap<PosKey, usize>,
    by_id: HashMap<u64, usize>,
    /// Placed floor: seeded from the baseline, then kept current by
    /// `AddTile`/`RemoveTile` events for as long as replay runs.
    tiles: HashMap<PosKey, NameId>,
    /// Natural terrain: seeded from the baseline and never touched again
    /// (see `is_placed_floor`). Kept separate from `tiles` specifically so
    /// `to_frame` (called once per emitted replay frame, potentially
    /// hundreds of times) never has to re-serialize it: a real capture's
    /// terrain can be large, and doing that on every single frame is what
    /// made every emitted frame file redundantly balloon to the same huge,
    /// unchanging size.
    terrain: HashMap<PosKey, NameId>,
    /// Bumped by every mutation that actually changes this surface, and by
    /// nothing else.
    ///
    /// What `replay::write_all_surfaces` compares against to decide whether a
    /// surface needs writing at all. A counter rather than a hash of the
    /// frame, because the entire point is to never materialise the frame: on
    /// a nine-surface save, measured over 13 minutes of real play, 86% of the
    /// files written were byte-identical to that surface's previous one and
    /// 93% of the bytes were, since you can only build on one surface at a
    /// time but every surface was written every frame. Hashing to detect that
    /// would mean doing the expensive work first and then throwing it away.
    ///
    /// Precision matters more than it looks: a spurious bump costs a whole
    /// duplicate file, which is why `insert` below checks for a genuinely
    /// unchanged re-add rather than blindly overwriting.
    revision: u64,
}

impl Surface {
    pub fn entity_count(&self) -> usize {
        self.by_pos.len()
    }

    /// See the field's own comment. Only ever compared against a previously
    /// observed value from the same surface; the absolute number means
    /// nothing.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Both layers together: what a baseline's "tiles" count means to
    /// someone reading it, before they have any reason to know the two
    /// layers exist internally. See `floor_tile_count`/`terrain_tile_count`
    /// for the split.
    pub fn tile_count(&self) -> usize {
        self.tiles.len() + self.terrain.len()
    }

    pub fn floor_tile_count(&self) -> usize {
        self.tiles.len()
    }

    pub fn terrain_tile_count(&self) -> usize {
        self.terrain.len()
    }

    pub fn entities(&self) -> impl Iterator<Item = &WorldEntity> {
        self.slots.iter().filter_map(Option::as_ref)
    }

    fn insert(&mut self, entity: WorldEntity) {
        let key = pos_key(entity.x, entity.y);

        // Idempotent: an add for a position already occupied updates it in
        // place rather than creating a second entity on the same tile.
        if let Some(&slot) = self.by_pos.get(&key) {
            // An add landing on exactly what is already there changed
            // nothing, so it must not bump `revision`. Checked rather than
            // assumed because a spurious bump costs a whole redundant frame
            // file, and re-adds are not rare: the baseline smear (a snapshot
            // taken slightly after the events describing the same
            // construction) produces them by design.
            if self.slots[slot] == Some(entity) {
                return;
            }
            if let Some(existing) = self.slots[slot].take() {
                if let Some(id) = existing.id {
                    self.by_id.remove(&id);
                }
            }
            if let Some(id) = entity.id {
                self.by_id.insert(id, slot);
            }
            self.slots[slot] = Some(entity);
            self.revision += 1;
            return;
        }

        let slot = match self.free.pop() {
            Some(slot) => {
                self.slots[slot] = Some(entity);
                slot
            }
            None => {
                self.slots.push(Some(entity));
                self.slots.len() - 1
            }
        };
        self.by_pos.insert(key, slot);
        if let Some(id) = entity.id {
            self.by_id.insert(id, slot);
        }
        self.revision += 1;
    }

    fn remove_slot(&mut self, slot: usize) {
        let Some(entity) = self.slots[slot].take() else { return };
        self.by_pos.remove(&pos_key(entity.x, entity.y));
        if let Some(id) = entity.id {
            self.by_id.remove(&id);
        }
        self.free.push(slot);
        self.revision += 1;
    }

    fn remove_by_id(&mut self, id: u64) -> bool {
        match self.by_id.get(&id).copied() {
            Some(slot) => {
                self.remove_slot(slot);
                true
            }
            None => false,
        }
    }

    fn remove_at(&mut self, x: f32, y: f32) -> bool {
        match self.by_pos.get(&pos_key(x, y)).copied() {
            Some(slot) => {
                self.remove_slot(slot);
                true
            }
            None => false,
        }
    }
}

/// Every surface, plus the name table they share.
#[derive(Debug, Default)]
pub struct World {
    names: NameTable,
    surfaces: HashMap<String, Surface>,
    /// The surface events fall back to when they name none: logs written
    /// before events carried a surface, and removals keyed by id.
    default_surface: Option<String>,
    pub tick: u64,
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn names(&self) -> &NameTable {
        &self.names
    }

    pub fn surface(&self, name: &str) -> Option<&Surface> {
        self.surfaces.get(name)
    }

    pub fn surface_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.surfaces.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    pub fn entity_count(&self) -> usize {
        self.surfaces.values().map(Surface::entity_count).sum()
    }

    pub fn tile_count(&self) -> usize {
        self.surfaces.values().map(Surface::tile_count).sum()
    }

    /// Seed a surface from one baseline frame. The first frame loaded becomes
    /// the default surface, and the CLI loads the largest first, so untagged
    /// events land on the busiest surface rather than an arbitrary one.
    pub fn load_baseline(&mut self, frame: &Frame) {
        let surface = self.surfaces.entry(frame.surface.clone()).or_default();
        if self.default_surface.is_none() {
            self.default_surface = Some(frame.surface.clone());
        }

        for entity in &frame.entities {
            let name = self.names.intern(&entity.n);
            surface.insert(WorldEntity {
                name,
                x: entity.x,
                y: entity.y,
                d: entity.d,
                w: entity.w,
                h: entity.h,
                // Snapshots carry no unit_number, so a baseline entity can
                // only ever be removed by position. That is why the mod
                // prefers id on removal but always has the position path.
                id: None,
            });
        }
        for tile in &frame.tiles {
            let name = self.names.intern(&tile.n);
            let key = (tile.x, tile.y);
            if is_placed_floor(&tile.n) {
                surface.tiles.insert(key, name);
            } else {
                surface.terrain.insert(key, name);
            }
        }

        // Bumped once for the whole load rather than per tile above, and
        // unconditionally: a catch-up baseline landing mid-replay is a change
        // to this surface however much of it happens to match what was
        // already there, and the entity loop's own bumps do not cover a
        // baseline that is only tiles.
        surface.revision += 1;

        self.tick = self.tick.max(frame.tick);
    }

    fn target(&mut self, surface: Option<&str>) -> Option<&mut Surface> {
        let key = surface.map(str::to_string).or_else(|| self.default_surface.clone())?;
        Some(self.surfaces.entry(key).or_default())
    }

    /// Apply one event. Returns whether it changed anything, which is what
    /// the replay uses to decide a chunk is dirty, and what makes the
    /// baseline smear visible as a count rather than silently absorbed.
    pub fn apply(&mut self, surface: Option<&str>, event: &Event) -> bool {
        match event {
            Event::AddEntity { name, x, y, d, w, h, id } => {
                let name = self.names.intern(name);
                let entity = WorldEntity { name, x: *x, y: *y, d: *d, w: *w, h: *h, id: *id };
                match self.target(surface) {
                    Some(s) => {
                        s.insert(entity);
                        true
                    }
                    None => false,
                }
            }
            // Id first: unit_number is unique game-wide (so this searches
            // every surface, not just `surface`) and resolves in O(1) for
            // anything replay already has registered, which is anything
            // built after capture began, since its AddEntity carried the
            // same id. Position is the fallback, and the only thing that
            // can resolve an entity that already existed when the baseline
            // was taken: a snapshot records no ids, so no id lookup can ever
            // find a baseline-original entity no matter what id Factorio
            // reports it removed by.
            Event::RemoveEntity { id, pos } => {
                if let Some(id) = id {
                    if self.surfaces.values_mut().any(|s| s.remove_by_id(*id)) {
                        return true;
                    }
                }
                let (x, y) = *pos;
                self.target(surface).is_some_and(|s| s.remove_at(x, y))
            }
            Event::AddTile { name, x, y } => {
                let name = self.names.intern(name);
                match self.target(surface) {
                    Some(s) => {
                        let changed = s.tiles.insert((*x, *y), name) != Some(name);
                        if changed {
                            s.revision += 1;
                        }
                        changed
                    }
                    None => false,
                }
            }
            // Known gap, not fixed here: removing landfill fires this the
            // same as any other tile removal, but `tiles` only ever holds
            // placed floor now (see `Surface::terrain`) and still has no
            // idea what was there before the removed tile, e.g. the water a
            // baseline captured underneath it. The position just goes empty
            // instead of reverting to water. A real fix needs the mod to
            // capture what a removed placed-floor tile is replacing at
            // removal time, which this event does not carry.
            Event::RemoveTile { x, y } => self.target(surface).is_some_and(|s| {
                let changed = s.tiles.remove(&(*x, *y)).is_some();
                if changed {
                    s.revision += 1;
                }
                changed
            }),
        }
    }

    /// Materialise one surface as a `Frame`, the format the viewer already
    /// reads, so replay is an offline step that produces ordinary frames
    /// rather than a second thing the viewer has to understand. Placed
    /// floor only: natural terrain lives in a separate, unchanging layer
    /// (see `Surface::terrain`) that this deliberately never touches, since
    /// this gets called once per emitted replay frame (potentially
    /// hundreds of times) and terrain hasn't changed since the baseline
    /// loaded it. Use `terrain_frame` to get that layer, once.
    pub fn to_frame(&self, surface_name: &str, tick: u64) -> Frame {
        let Some(surface) = self.surfaces.get(surface_name) else {
            return Frame { tick, surface: surface_name.to_string(), entities: Vec::new(), count: 0, tiles: Vec::new() };
        };

        let names = self.name_table();

        let entities: Vec<Entity> = surface
            .entities()
            .map(|e| Entity { n: Arc::clone(&names[e.name as usize]), x: e.x, y: e.y, d: e.d, w: e.w, h: e.h })
            .collect();

        let tiles = Self::materialize_tiles(&surface.tiles, &names);

        Frame { tick, surface: surface_name.to_string(), count: entities.len(), entities, tiles }
    }

    /// Materialise one surface's natural-terrain layer as a `Frame`-shaped
    /// snapshot in the same format `to_frame` uses (`entities` always
    /// empty). Terrain never changes after the baseline loads it (see
    /// `Surface::terrain`), so unlike `to_frame` this only ever needs
    /// calling once per surface, right after loading the baseline, not once
    /// per replayed frame.
    pub fn terrain_frame(&self, surface_name: &str, tick: u64) -> Frame {
        let Some(surface) = self.surfaces.get(surface_name) else {
            return Frame { tick, surface: surface_name.to_string(), entities: Vec::new(), count: 0, tiles: Vec::new() };
        };

        let names = self.name_table();
        let tiles = Self::materialize_tiles(&surface.terrain, &names);
        Frame { tick, surface: surface_name.to_string(), count: 0, entities: Vec::new(), tiles }
    }

    /// Resolved once per call rather than once per entity/tile: a real
    /// surface has a few dozen distinct names against hundreds of thousands
    /// of entities (or millions of tiles), so re-allocating the same name
    /// string on every entity/tile of every frame is pure waste `Arc::clone`
    /// skips.
    fn name_table(&self) -> Vec<Arc<str>> {
        (0..self.names.len()).map(|id| Arc::from(self.names.name(id as NameId))).collect()
    }

    /// Sorted so a materialised frame is byte-stable across runs: a
    /// `HashMap` iterates in an order that depends on allocation and
    /// hashing rather than on its contents.
    fn materialize_tiles(tiles: &HashMap<PosKey, NameId>, names: &[Arc<str>]) -> Vec<Tile> {
        let mut out: Vec<Tile> =
            tiles.iter().map(|(&(x, y), &name)| Tile { n: Arc::clone(&names[name as usize]), x, y }).collect();
        out.sort_by_key(|t| (t.y, t.x));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(entities: Vec<Entity>, tiles: Vec<Tile>) -> Frame {
        Frame { tick: 100, surface: "nauvis".to_string(), count: entities.len(), entities, tiles }
    }

    fn entity(n: &str, x: f32, y: f32) -> Entity {
        Entity { n: n.into(), x, y, d: 0, w: 1, h: 1 }
    }

    fn add(name: &str, x: f32, y: f32, id: Option<u64>) -> Event {
        Event::AddEntity { name: name.to_string(), x, y, d: 0, w: 1, h: 1, id }
    }

    /// Position is always present on the wire now, so a "removal by id" test
    /// helper still needs one, even for the id lookup fast path that never
    /// reads it.
    fn remove_by_id(id: u64, x: f32, y: f32) -> Event {
        Event::RemoveEntity { id: Some(id), pos: (x, y) }
    }

    fn remove_at(x: f32, y: f32) -> Event {
        Event::RemoveEntity { id: None, pos: (x, y) }
    }

    /// Regression: keying positions by half-tile merged these two real
    /// entities into one and lost five of `frame_0000.stfr`'s 240.
    #[test]
    fn entities_a_tenth_of_a_tile_apart_stay_distinct() {
        let mut world = World::new();
        world.load_baseline(&baseline(
            vec![entity("logistic-train-stop-lamp-control", 326.9, -843.0), entity("logistic-train-stop", 327.0, -843.0)],
            Vec::new(),
        ));
        assert_eq!(world.entity_count(), 2);

        // ...and each is individually addressable.
        assert!(world.apply(Some("nauvis"), &remove_at(326.9, -843.0)));
        assert_eq!(world.entity_count(), 1);
        assert_eq!(world.to_frame("nauvis", 0).entities[0].n, "logistic-train-stop".into());
    }

    /// The real baseline must survive a round trip intact, which is how the
    /// half-tile bug was caught in the first place.
    #[test]
    fn a_real_exported_frame_loads_without_losing_entities() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames/frame_0000.stfr");
        let bytes = std::fs::read(path).unwrap();
        let frame = crate::frame::read_binary(&bytes).unwrap();
        let expected = frame.entities.len();

        let mut world = World::new();
        world.load_baseline(&frame);
        assert_eq!(world.entity_count(), expected, "entities lost loading a real frame");
        assert_eq!(world.to_frame("nauvis", 0).count, expected);
    }

    #[test]
    fn a_baseline_loads_entities_and_tiles() {
        let mut world = World::new();
        world.load_baseline(&baseline(
            vec![entity("pipe", 1.5, 2.5), entity("transport-belt", 3.5, 4.5)],
            vec![Tile { n: "concrete".into(), x: 0, y: 0 }],
        ));
        assert_eq!(world.entity_count(), 2);
        assert_eq!(world.tile_count(), 1);
        assert_eq!(world.surface_names(), vec!["nauvis"]);
    }

    #[test]
    fn adding_then_removing_by_id_round_trips() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), Vec::new()));

        assert!(world.apply(Some("nauvis"), &add("pipe", 1.5, 2.5, Some(7))));
        assert_eq!(world.entity_count(), 1);

        assert!(world.apply(None, &remove_by_id(7, 1.5, 2.5)));
        assert_eq!(world.entity_count(), 0);
    }

    #[test]
    fn removing_by_position_works_for_baseline_entities_that_have_no_id() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("pipe", -3.5, 4.5)], Vec::new()));

        assert!(world.apply(Some("nauvis"), &remove_at(-3.5, 4.5)));
        assert_eq!(world.entity_count(), 0);
    }

    /// The bug this fixes: an entity from the baseline carries no id in
    /// replay's world state, but Factorio still reports its *real*
    /// unit_number (assigned whenever it was originally built) when it's
    /// later mined. The removal event therefore carries an id lookup can
    /// never resolve, alongside the position that can. Before the fix, the
    /// mod sent id alone whenever one was available and this removal was a
    /// silent no-op forever: the entity never disappeared from the
    /// replayed timeline.
    #[test]
    fn removing_a_baseline_entity_by_an_id_replay_never_registered_falls_back_to_position() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("pipe", -3.5, 4.5)], Vec::new()));

        // A real unit_number Factorio assigned long before capture started,
        // so replay's by_id map was never told about it.
        let unrecognized_id = 999_999;
        assert!(world.apply(Some("nauvis"), &Event::RemoveEntity { id: Some(unrecognized_id), pos: (-3.5, 4.5) }));
        assert_eq!(world.entity_count(), 0, "position must resolve it even though the id can't");
    }

    /// The baseline is written over many ticks while events are logged, so an
    /// add for something already captured is expected, not a bug.
    #[test]
    fn adding_over_an_existing_position_replaces_rather_than_duplicating() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("pipe", 1.5, 2.5)], Vec::new()));

        world.apply(Some("nauvis"), &add("transport-belt", 1.5, 2.5, Some(9)));
        assert_eq!(world.entity_count(), 1, "still one entity on that tile");

        let frame = world.to_frame("nauvis", 200);
        assert_eq!(frame.entities[0].n, Arc::from("transport-belt"), "the later add wins");

        // ...and it is now reachable by the id the add carried.
        assert!(world.apply(None, &remove_by_id(9, 1.5, 2.5)));
        assert_eq!(world.entity_count(), 0);
    }

    /// The other half of the smear: a remove for something the baseline never
    /// captured must be a no-op, not a panic or a corrupt index.
    #[test]
    fn removing_something_absent_is_a_harmless_no_op() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("pipe", 1.5, 2.5)], Vec::new()));

        assert!(!world.apply(None, &remove_by_id(404, 500.0, 500.0)));
        assert!(!world.apply(Some("nauvis"), &remove_at(99.5, 99.5)));
        assert!(!world.apply(Some("nauvis"), &Event::RemoveTile { x: 5, y: 5 }));
        assert_eq!(world.entity_count(), 1, "the real entity is untouched");
    }

    #[test]
    fn tiles_add_replace_and_remove_by_position() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), Vec::new()));

        assert!(world.apply(Some("nauvis"), &Event::AddTile { name: "concrete".into(), x: -5, y: 12 }));
        assert!(
            !world.apply(Some("nauvis"), &Event::AddTile { name: "concrete".into(), x: -5, y: 12 }),
            "re-adding the same tile changes nothing"
        );
        assert!(
            world.apply(Some("nauvis"), &Event::AddTile { name: "stone-path".into(), x: -5, y: 12 }),
            "a different tile on the same spot is a change"
        );
        assert_eq!(world.tile_count(), 1);

        assert!(world.apply(Some("nauvis"), &Event::RemoveTile { x: -5, y: 12 }));
        assert_eq!(world.tile_count(), 0);
    }

    /// Positions only repeat across surfaces, so without routing, two planets
    /// would overwrite each other at the same coordinates.
    #[test]
    fn surfaces_are_independent_at_identical_coordinates() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("pipe", 1.5, 2.5)], Vec::new()));
        world.load_baseline(&Frame {
            tick: 100,
            surface: "vulcanus".to_string(),
            count: 1,
            entities: vec![entity("transport-belt", 1.5, 2.5)],
            tiles: Vec::new(),
        });

        assert_eq!(world.entity_count(), 2);
        world.apply(Some("vulcanus"), &remove_at(1.5, 2.5));

        assert_eq!(world.surface("nauvis").unwrap().entity_count(), 1);
        assert_eq!(world.surface("vulcanus").unwrap().entity_count(), 0);
    }

    /// Logs written before events carried a surface must still replay, onto
    /// the first baseline loaded.
    #[test]
    fn untagged_events_fall_back_to_the_first_baseline_surface() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), Vec::new()));

        assert!(world.apply(None, &add("pipe", 1.5, 2.5, None)));
        assert_eq!(world.surface("nauvis").unwrap().entity_count(), 1);
    }

    #[test]
    fn freed_slots_are_reused_rather_than_growing_the_slab() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), Vec::new()));

        for i in 0..100 {
            world.apply(Some("nauvis"), &add("pipe", i as f32 + 0.5, 0.5, Some(i)));
            world.apply(None, &remove_by_id(i, i as f32 + 0.5, 0.5));
        }
        assert_eq!(world.entity_count(), 0);
        assert_eq!(world.surface("nauvis").unwrap().slots.len(), 1, "one slot, reused 100 times");
    }

    #[test]
    fn to_frame_is_stable_across_runs() {
        let build = || {
            let mut world = World::new();
            world.load_baseline(&baseline(
                vec![entity("pipe", 1.5, 2.5)],
                vec![
                    Tile { n: "concrete".into(), x: 3, y: 1 },
                    Tile { n: "concrete".into(), x: 0, y: 1 },
                    Tile { n: "stone-path".into(), x: 2, y: 0 },
                ],
            ));
            world.to_frame("nauvis", 500)
        };
        let a = build();
        let b = build();
        let coords = |f: &Frame| f.tiles.iter().map(|t| (t.x, t.y)).collect::<Vec<_>>();
        assert_eq!(coords(&a), coords(&b));
        assert_eq!(coords(&a), vec![(2, 0), (0, 1), (3, 1)], "row-major order");
        assert_eq!(a.tick, 500);
        assert_eq!(a.count, 1);
    }

    #[test]
    fn an_unknown_surface_materialises_as_an_empty_frame() {
        let world = World::new();
        let frame = world.to_frame("nowhere", 12);
        assert_eq!(frame.count, 0);
        assert!(frame.entities.is_empty() && frame.tiles.is_empty());
        assert_eq!(frame.surface, "nowhere");
    }

    #[test]
    fn is_placed_floor_matches_the_mods_placed_floor_list() {
        for name in [
            "stone-path",
            "concrete",
            "hazard-concrete-left",
            "hazard-concrete-right",
            "refined-concrete",
            "refined-hazard-concrete-left",
            "refined-hazard-concrete-right",
            "landfill",
            "red-refined-concrete",
            "green-refined-concrete",
            "blue-refined-concrete",
            "orange-refined-concrete",
            "yellow-refined-concrete",
            "pink-refined-concrete",
            "purple-refined-concrete",
            "black-refined-concrete",
            "brown-refined-concrete",
            "cyan-refined-concrete",
            "acid-refined-concrete",
        ] {
            assert!(is_placed_floor(name), "{name} should be placed floor");
        }

        for name in ["grass-1", "water", "sand-1", "deepwater", "dirt-3"] {
            assert!(!is_placed_floor(name), "{name} should be natural terrain");
        }
    }

    /// A baseline mixes both kinds of tile; loading must sort each into the
    /// layer `is_placed_floor` says it belongs in, not lump them together.
    #[test]
    fn a_baseline_routes_tiles_to_the_right_layer() {
        let mut world = World::new();
        world.load_baseline(&baseline(
            Vec::new(),
            vec![
                Tile { n: "concrete".into(), x: 0, y: 0 },
                Tile { n: "grass-1".into(), x: 1, y: 0 },
                Tile { n: "landfill".into(), x: 2, y: 0 },
                Tile { n: "water".into(), x: 3, y: 0 },
            ],
        ));

        let surface = world.surface("nauvis").unwrap();
        assert_eq!(surface.floor_tile_count(), 2, "concrete and landfill");
        assert_eq!(surface.terrain_tile_count(), 2, "grass and water");
        assert_eq!(surface.tile_count(), 4, "both layers together");
    }

    /// `to_frame` runs once per emitted replay frame, so it must only ever
    /// carry placed floor: including terrain here is exactly the bug this
    /// fix removes.
    #[test]
    fn to_frame_never_includes_terrain() {
        let mut world = World::new();
        world.load_baseline(&baseline(
            Vec::new(),
            vec![Tile { n: "concrete".into(), x: 0, y: 0 }, Tile { n: "grass-1".into(), x: 1, y: 0 }],
        ));

        let frame = world.to_frame("nauvis", 10);
        assert_eq!(frame.tiles.len(), 1);
        assert_eq!(frame.tiles[0].n, Arc::from("concrete"));
    }

    /// The counterpart to `to_frame_never_includes_terrain`: the terrain
    /// layer materialises through its own method instead, with no entities
    /// since natural terrain never has any.
    #[test]
    fn terrain_frame_carries_only_the_terrain_layer() {
        let mut world = World::new();
        world.load_baseline(&baseline(
            vec![entity("pipe", 1.5, 2.5)],
            vec![
                Tile { n: "concrete".into(), x: 0, y: 0 },
                Tile { n: "grass-1".into(), x: 1, y: 0 },
                Tile { n: "water".into(), x: 2, y: 0 },
            ],
        ));

        let frame = world.terrain_frame("nauvis", 10);
        assert_eq!(frame.count, 0);
        assert!(frame.entities.is_empty());
        let names: Vec<Arc<str>> = frame.tiles.iter().map(|t| t.n.clone()).collect();
        assert_eq!(names, vec![Arc::from("grass-1"), Arc::from("water")]);
    }

    #[test]
    fn terrain_frame_is_empty_when_the_baseline_had_no_terrain() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), vec![Tile { n: "concrete".into(), x: 0, y: 0 }]));

        let frame = world.terrain_frame("nauvis", 10);
        assert!(frame.tiles.is_empty());
    }

    #[test]
    fn terrain_frame_for_an_unknown_surface_is_empty() {
        let world = World::new();
        let frame = world.terrain_frame("nowhere", 12);
        assert_eq!(frame.count, 0);
        assert!(frame.entities.is_empty() && frame.tiles.is_empty());
        assert_eq!(frame.surface, "nowhere");
    }
}
