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

use crate::event::Event;
use crate::frame::{Entity, Frame, Tile};
use crate::names::{NameId, NameTable};

/// Integer position key, so lookups never hash a float or compare with an
/// epsilon.
///
/// Scaled by ten, not two. Half-tile alignment covers most entities but not
/// all: `tests/fixtures/frames/frame_0000.json` has a
/// `logistic-train-stop-lamp-control` at x=326.9 sitting beside its
/// `logistic-train-stop` at x=327.0. Keying on half tiles collapsed the two
/// onto one slot and silently dropped one of them -- five entities out of
/// that frame's 240. One decimal is exactly the precision the mod writes
/// (`%.1f`), so scaling by ten is both lossless and collision-free.
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
    tiles: HashMap<PosKey, NameId>,
}

impl Surface {
    pub fn entity_count(&self) -> usize {
        self.by_pos.len()
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    pub fn entities(&self) -> impl Iterator<Item = &WorldEntity> {
        self.slots.iter().filter_map(Option::as_ref)
    }

    fn insert(&mut self, entity: WorldEntity) {
        let key = pos_key(entity.x, entity.y);

        // Idempotent: an add for a position already occupied updates it in
        // place rather than creating a second entity on the same tile.
        if let Some(&slot) = self.by_pos.get(&key) {
            if let Some(existing) = self.slots[slot].take() {
                if let Some(id) = existing.id {
                    self.by_id.remove(&id);
                }
            }
            if let Some(id) = entity.id {
                self.by_id.insert(id, slot);
            }
            self.slots[slot] = Some(entity);
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
    }

    fn remove_slot(&mut self, slot: usize) {
        let Some(entity) = self.slots[slot].take() else { return };
        self.by_pos.remove(&pos_key(entity.x, entity.y));
        if let Some(id) = entity.id {
            self.by_id.remove(&id);
        }
        self.free.push(slot);
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
    /// The surface events fall back to when they name none -- logs written
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
            surface.tiles.insert((tile.x, tile.y), name);
        }

        self.tick = self.tick.max(frame.tick);
    }

    fn target(&mut self, surface: Option<&str>) -> Option<&mut Surface> {
        let key = surface.map(str::to_string).or_else(|| self.default_surface.clone())?;
        Some(self.surfaces.entry(key).or_default())
    }

    /// Apply one event. Returns whether it changed anything, which is what
    /// the replay uses to decide a chunk is dirty -- and what makes the
    /// baseline smear visible as a count rather than silently absorbed.
    pub fn apply(&mut self, surface: Option<&str>, event: &Event) -> bool {
        match event {
            Event::AddEntity { name, x, y, d, w, h, id } => {
                let name = self.names.intern(name);
                let entity =
                    WorldEntity { name, x: *x, y: *y, d: *d, w: *w, h: *h, id: *id };
                match self.target(surface) {
                    Some(s) => {
                        s.insert(entity);
                        true
                    }
                    None => false,
                }
            }
            // An id-keyed removal names no surface, so it has to be looked
            // for everywhere. unit_number is unique game-wide, so at most one
            // surface can hold it.
            Event::RemoveEntityById { id } => {
                self.surfaces.values_mut().any(|s| s.remove_by_id(*id))
            }
            Event::RemoveEntityAt { x, y } => {
                self.target(surface).is_some_and(|s| s.remove_at(*x, *y))
            }
            Event::AddTile { name, x, y } => {
                let name = self.names.intern(name);
                match self.target(surface) {
                    Some(s) => s.tiles.insert((*x, *y), name) != Some(name),
                    None => false,
                }
            }
            Event::RemoveTile { x, y } => {
                self.target(surface).is_some_and(|s| s.tiles.remove(&(*x, *y)).is_some())
            }
        }
    }

    /// Materialise one surface as a `Frame`, the format the viewer already
    /// reads -- so replay is an offline step that produces ordinary frames
    /// rather than a second thing the viewer has to understand.
    pub fn to_frame(&self, surface_name: &str, tick: u64) -> Frame {
        let Some(surface) = self.surfaces.get(surface_name) else {
            return Frame {
                tick,
                surface: surface_name.to_string(),
                entities: Vec::new(),
                count: 0,
                tiles: Vec::new(),
            };
        };

        let entities: Vec<Entity> = surface
            .entities()
            .map(|e| Entity {
                n: self.names.name(e.name).to_string(),
                x: e.x,
                y: e.y,
                d: e.d,
                w: e.w,
                h: e.h,
            })
            .collect();

        // Sorted so a replayed frame is byte-stable across runs: both the
        // slab and the tile map iterate in an order that depends on
        // allocation and hashing rather than on the world's contents.
        let mut tiles: Vec<Tile> = surface
            .tiles
            .iter()
            .map(|(&(x, y), &name)| Tile { n: self.names.name(name).to_string(), x, y })
            .collect();
        tiles.sort_by_key(|t| (t.y, t.x));

        Frame { tick, surface: surface_name.to_string(), count: entities.len(), entities, tiles }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(entities: Vec<Entity>, tiles: Vec<Tile>) -> Frame {
        Frame {
            tick: 100,
            surface: "nauvis".to_string(),
            count: entities.len(),
            entities,
            tiles,
        }
    }

    fn entity(n: &str, x: f32, y: f32) -> Entity {
        Entity { n: n.to_string(), x, y, d: 0, w: 1, h: 1 }
    }

    fn add(name: &str, x: f32, y: f32, id: Option<u64>) -> Event {
        Event::AddEntity { name: name.to_string(), x, y, d: 0, w: 1, h: 1, id }
    }

    /// Regression: keying positions by half-tile merged these two real
    /// entities into one and lost five of `frame_0000.json`'s 240.
    #[test]
    fn entities_a_tenth_of_a_tile_apart_stay_distinct() {
        let mut world = World::new();
        world.load_baseline(&baseline(
            vec![
                entity("logistic-train-stop-lamp-control", 326.9, -843.0),
                entity("logistic-train-stop", 327.0, -843.0),
            ],
            Vec::new(),
        ));
        assert_eq!(world.entity_count(), 2);

        // ...and each is individually addressable.
        assert!(world.apply(Some("nauvis"), &Event::RemoveEntityAt { x: 326.9, y: -843.0 }));
        assert_eq!(world.entity_count(), 1);
        assert_eq!(world.to_frame("nauvis", 0).entities[0].n, "logistic-train-stop");
    }

    /// The real baseline must survive a round trip intact, which is how the
    /// half-tile bug was caught in the first place.
    #[test]
    fn a_real_exported_frame_loads_without_losing_entities() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames/frame_0000.json");
        let text = std::fs::read_to_string(path).unwrap();
        let frame: Frame = serde_json::from_str(&text).unwrap();
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
            vec![Tile { n: "concrete".to_string(), x: 0, y: 0 }],
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

        assert!(world.apply(None, &Event::RemoveEntityById { id: 7 }));
        assert_eq!(world.entity_count(), 0);
    }

    #[test]
    fn removing_by_position_works_for_baseline_entities_that_have_no_id() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("pipe", -3.5, 4.5)], Vec::new()));

        assert!(world.apply(Some("nauvis"), &Event::RemoveEntityAt { x: -3.5, y: 4.5 }));
        assert_eq!(world.entity_count(), 0);
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
        assert_eq!(frame.entities[0].n, "transport-belt", "the later add wins");

        // ...and it is now reachable by the id the add carried.
        assert!(world.apply(None, &Event::RemoveEntityById { id: 9 }));
        assert_eq!(world.entity_count(), 0);
    }

    /// The other half of the smear: a remove for something the baseline never
    /// captured must be a no-op, not a panic or a corrupt index.
    #[test]
    fn removing_something_absent_is_a_harmless_no_op() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("pipe", 1.5, 2.5)], Vec::new()));

        assert!(!world.apply(None, &Event::RemoveEntityById { id: 404 }));
        assert!(!world.apply(Some("nauvis"), &Event::RemoveEntityAt { x: 99.5, y: 99.5 }));
        assert!(!world.apply(Some("nauvis"), &Event::RemoveTile { x: 5, y: 5 }));
        assert_eq!(world.entity_count(), 1, "the real entity is untouched");
    }

    #[test]
    fn tiles_add_replace_and_remove_by_position() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), Vec::new()));

        assert!(world.apply(Some("nauvis"), &Event::AddTile { name: "concrete".into(), x: -5, y: 12 }));
        assert!(!world.apply(Some("nauvis"), &Event::AddTile { name: "concrete".into(), x: -5, y: 12 }),
            "re-adding the same tile changes nothing");
        assert!(world.apply(Some("nauvis"), &Event::AddTile { name: "stone-path".into(), x: -5, y: 12 }),
            "a different tile on the same spot is a change");
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
        world.apply(Some("vulcanus"), &Event::RemoveEntityAt { x: 1.5, y: 2.5 });

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
            world.apply(None, &Event::RemoveEntityById { id: i });
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
                    Tile { n: "concrete".to_string(), x: 3, y: 1 },
                    Tile { n: "concrete".to_string(), x: 0, y: 1 },
                    Tile { n: "stone-path".to_string(), x: 2, y: 0 },
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
}
