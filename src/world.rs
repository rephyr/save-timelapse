//! Mutable world state: a baseline snapshot advanced by replaying the event
//! log.
//!
//! Application is forgiving on purpose. The baseline is written across many
//! ticks while events are logged, so replay sees adds for things already
//! present and removes for things it never saw. Both are no-ops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::event::Event;
use crate::frame::{Entity, Frame, Tile};
use crate::names::{NameId, NameTable};

/// Integer position key, so lookups never hash a float.
///
/// Scaled by ten, not two: half-tile keying collapsed a
/// `logistic-train-stop-lamp-control` at x=326.9 onto its stop at x=327.0.
/// Computed in f64, an f32 being unable to round the scaled value at the map
/// limit.
type PosKey = (i32, i32);

pub fn pos_key(x: f32, y: f32) -> PosKey {
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

/// Whether `name` is floor somebody laid: what the capture said, or the list
/// below when it said nothing. A free function so it borrows one field and can
/// be called while `load_baseline` holds a surface.
fn is_floor(floor: &HashSet<String>, name: &str) -> bool {
    if floor.is_empty() {
        return is_placed_floor(name);
    }
    floor.contains(name)
}

/// Placed floor, for a capture that did not say which tiles it treated that
/// way. Everything else in a baseline is natural terrain, which has no events
/// and so needs only this one-time split. Wube's names only, which is why a
/// capture now says for itself (see `World::set_floor`).
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
            // Aquilo's frozen twins, placed by the player like the unfrozen
            // ones. The game generates these seven only.
            | "frozen-stone-path"
            | "frozen-concrete"
            | "frozen-hazard-concrete-left"
            | "frozen-hazard-concrete-right"
            | "frozen-refined-concrete"
            | "frozen-refined-hazard-concrete-left"
            | "frozen-refined-hazard-concrete-right"
    )
}

/// One surface's contents. Entities live in a slab with free-list reuse, so
/// ids stay stable and a churn-heavy replay reuses slots.
#[derive(Debug, Default)]
pub struct Surface {
    slots: Vec<Option<WorldEntity>>,
    free: Vec<usize>,
    by_pos: HashMap<PosKey, usize>,
    /// What a position held before something was built on top of it.
    ///
    /// Factorio lets a resource and a machine share a position. Keying by
    /// position alone let the add evict the ore and the later remove clear the
    /// tile, so building across a patch ate it. A second layer rather than a
    /// list per position: the depth needed is two, and a `Vec` each would
    /// allocate per entity. Nothing here knows what a resource is.
    under: HashMap<PosKey, usize>,
    by_id: HashMap<u64, usize>,
    /// Placed floor: seeded from the baseline, then kept current by
    /// `AddTile`/`RemoveTile` events for as long as replay runs.
    tiles: HashMap<PosKey, NameId>,
    /// Natural terrain: seeded from the baseline and never touched again.
    /// Separate from `tiles` so `to_frame`, called once per emitted frame,
    /// never re-serializes it.
    terrain: HashMap<PosKey, NameId>,
    /// Bumped by every mutation that actually changes this surface, which is
    /// what `replay::write_all_surfaces` compares to decide whether to write at
    /// all: on a nine-surface save 86% of files were identical to the previous
    /// one. A counter rather than a hash, the point being never to materialise
    /// the frame. A spurious bump costs a duplicate file, which is why `insert`
    /// checks for an unchanged re-add.
    revision: u64,
}

impl Surface {
    /// Counted from the slab rather than from `by_pos`, which holds only what
    /// is on top of each position and so undercounts anything covered.
    pub fn entity_count(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// Only meaningful compared against a previously observed value from the
    /// same surface.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Both layers together, which is what a baseline's "tiles" count means to
    /// a reader. See `floor_tile_count`/`terrain_tile_count` for the split.
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

        if let Some(&slot) = self.by_pos.get(&key) {
            // An unchanged re-add must not bump `revision`: the baseline
            // smear produces them by design, and a bump costs a whole file.
            if self.slots[slot] == Some(entity) {
                return;
            }
            // Something different covers what was there rather than replacing
            // it. Only one thing can be covered; a third arrival displaces
            // whatever the second was hiding.
            if let Some(buried) = self.under.insert(key, slot) {
                self.free_slot(buried);
            }
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

    /// Return a slot to the free list. Touches neither position index and does
    /// not bump `revision`; the caller owns both.
    fn free_slot(&mut self, slot: usize) {
        if let Some(entity) = self.slots[slot].take() {
            if let Some(id) = entity.id {
                self.by_id.remove(&id);
            }
        }
        self.free.push(slot);
    }

    fn remove_slot(&mut self, slot: usize) {
        let Some(entity) = self.slots[slot].take() else { return };
        let key = pos_key(entity.x, entity.y);

        // Uncovering: what this was covering takes the position back.
        if self.by_pos.get(&key) == Some(&slot) {
            match self.under.remove(&key) {
                Some(buried) => self.by_pos.insert(key, buried),
                None => self.by_pos.remove(&key),
            };
        } else if self.under.get(&key) == Some(&slot) {
            // Removed by id from underneath whatever stands on it.
            self.under.remove(&key);
        }

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
    /// Surface events fall back to when they name none: logs predating
    /// per-event surfaces, and removals keyed by id.
    default_surface: Option<String>,
    /// Which tiles this capture called placed floor. Empty falls back to
    /// `is_placed_floor`.
    floor: HashSet<String>,
    pub tick: u64,
}

impl World {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt the capture's own list of placed floor, from `prototypes.json`.
    /// Must be set before any baseline loads, the split happening as tiles
    /// arrive. Only the mod can be right about a modded floor, and floor filed
    /// as terrain can never be removed again.
    pub fn set_floor(&mut self, floor: HashSet<String>) {
        debug_assert!(self.surfaces.is_empty(), "floor must be set before loading a baseline");
        self.floor = floor;
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

    /// Seed a surface from one baseline frame. The first loaded becomes the
    /// default surface, and the CLI loads the largest first, so untagged events
    /// land on the busiest one.
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
                // only ever be removed by position.
                id: None,
            });
        }
        for tile in &frame.tiles {
            let name = self.names.intern(&tile.n);
            let key = (tile.x, tile.y);
            if is_floor(&self.floor, &tile.n) {
                surface.tiles.insert(key, name);
            } else {
                surface.terrain.insert(key, name);
            }
        }

        // Once for the whole load and unconditionally: a catch-up baseline is
        // a change however much of it matches, and the entity loop's bumps do
        // not cover a baseline that is only tiles.
        surface.revision += 1;

        self.tick = self.tick.max(frame.tick);
    }

    fn target(&mut self, surface: Option<&str>) -> Option<&mut Surface> {
        let key = surface.map(str::to_string).or_else(|| self.default_surface.clone())?;
        Some(self.surfaces.entry(key).or_default())
    }

    /// Apply one event. Returns whether it changed anything, which is how
    /// replay decides a chunk is dirty.
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
            // Id first: unique game-wide, so this searches every surface, and
            // O(1) for anything built after capture began. Position is the
            // only thing that can resolve a baseline entity, which has no id.
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
            // Clears the position rather than reverting it: this record cannot
            // say what was underneath, and a baseline taken while landfill was
            // down never saw the water. The mod does the revert instead,
            // logging an ordinary `AddTile` immediately after this one, and
            // only when terrain capture is on.
            Event::RemoveTile { x, y } => self.target(surface).is_some_and(|s| {
                let changed = s.tiles.remove(&(*x, *y)).is_some();
                if changed {
                    s.revision += 1;
                }
                changed
            }),
        }
    }

    /// Materialise one surface as a `Frame`, so replay produces ordinary
    /// frames rather than a second format the viewer must understand. Placed
    /// floor only: terrain is a separate unchanging layer, and this runs once
    /// per emitted frame. Use `terrain_frame` for that layer, once.
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

    /// The natural-terrain layer as a `Frame` (`entities` always empty).
    /// Terrain never changes after the baseline, so unlike `to_frame` this is
    /// called once per surface rather than once per replayed frame.
    pub fn terrain_frame(&self, surface_name: &str, tick: u64) -> Frame {
        let Some(surface) = self.surfaces.get(surface_name) else {
            return Frame { tick, surface: surface_name.to_string(), entities: Vec::new(), count: 0, tiles: Vec::new() };
        };

        let names = self.name_table();
        let tiles = Self::materialize_tiles(&surface.terrain, &names);
        Frame { tick, surface: surface_name.to_string(), count: 0, entities: Vec::new(), tiles }
    }

    /// Resolved once per call rather than per item: a surface has a few dozen
    /// names against hundreds of thousands of entities.
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

    /// Mining landfill has to put the water back, not leave a hole.
    ///
    /// The mod logs the removal and then an add for what it uncovered, because
    /// only it can see that: a baseline taken while the landfill was down never
    /// saw the water. This pins the pair applying in order to the right result,
    /// which is behaviour the two halves only have together.
    #[test]
    fn a_removed_tile_reverts_to_whatever_the_mod_says_was_uncovered() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), vec![Tile { n: "landfill".into(), x: 5, y: 5 }]));
        assert_eq!(world.to_frame("nauvis", 0).tiles.len(), 1);

        assert!(world.apply(Some("nauvis"), &Event::RemoveTile { x: 5, y: 5 }));
        assert!(world.to_frame("nauvis", 0).tiles.is_empty(), "the landfill is gone");

        assert!(world.apply(Some("nauvis"), &Event::AddTile { name: "water".to_string(), x: 5, y: 5 }));
        let tiles = world.to_frame("nauvis", 0).tiles;
        assert_eq!(tiles.len(), 1);
        assert_eq!((tiles[0].n.as_ref(), tiles[0].x, tiles[0].y), ("water", 5, 5));
    }

    /// The revert is a real change to the surface, so it has to mark the
    /// surface dirty. Otherwise `write_all_surfaces` would skip the very
    /// frame that shows the water coming back.
    #[test]
    fn revealing_a_tile_bumps_the_surface_revision() {
        let mut world = World::new();
        world.load_baseline(&baseline(Vec::new(), vec![Tile { n: "landfill".into(), x: 5, y: 5 }]));
        let before = world.surface("nauvis").unwrap().revision();

        world.apply(Some("nauvis"), &Event::RemoveTile { x: 5, y: 5 });
        world.apply(Some("nauvis"), &Event::AddTile { name: "water".to_string(), x: 5, y: 5 });

        assert!(world.surface("nauvis").unwrap().revision() > before, "the surface changed twice over");
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

    /// An entity from the baseline carries no id in replay's world state, but
    /// Factorio still reports its real unit_number when it is later mined, so
    /// the removal carries an id lookup can never resolve alongside the
    /// position that can. Sending id alone made such a removal a silent no-op
    /// forever.
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

    /// Building on an ore patch and mining the building back off. Factorio
    /// lets both share a tile, and keying entities by position alone meant the
    /// belt evicted the ore and the removal then cleared the tile, so building
    /// across a patch ate it a tile at a time and nothing brought it back.
    #[test]
    fn building_on_something_covers_it_and_removing_uncovers_it() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("iron-ore", 1.5, 2.5)], Vec::new()));

        world.apply(Some("nauvis"), &add("transport-belt", 1.5, 2.5, Some(9)));
        assert_eq!(world.entity_count(), 2, "the ore is still there, under the belt");

        let frame = world.to_frame("nauvis", 200);
        let names: Vec<&str> = frame.entities.iter().map(|e| &*e.n).collect();
        assert!(names.contains(&"transport-belt") && names.contains(&"iron-ore"), "got {names:?}");

        // Reachable by the id the add carried, and taking it away gives the
        // tile back to the ore rather than emptying it.
        assert!(world.apply(None, &remove_by_id(9, 1.5, 2.5)));
        assert_eq!(world.entity_count(), 1);
        let frame = world.to_frame("nauvis", 300);
        assert_eq!(frame.entities[0].n, Arc::from("iron-ore"), "the ore is back");

        // And the uncovered ore is reachable by position again, which is the
        // only way a baseline entity can be reached at all.
        assert!(world.apply(Some("nauvis"), &remove_at(1.5, 2.5)));
        assert_eq!(world.entity_count(), 0);
    }

    /// A re-add of the same thing must not bury a copy of it: the baseline
    /// smear produces these by design, and a buried duplicate would outlive the
    /// visible one.
    #[test]
    fn re_adding_the_same_entity_covers_nothing() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("transport-belt", 1.5, 2.5)], Vec::new()));

        world.apply(Some("nauvis"), &add("transport-belt", 1.5, 2.5, None));
        assert_eq!(world.entity_count(), 1);
        assert!(world.apply(Some("nauvis"), &remove_at(1.5, 2.5)));
        assert_eq!(world.entity_count(), 0, "nothing left buried under it");
    }

    /// Only one thing can be covered. A third arrival displaces whatever the
    /// second was hiding rather than growing a stack, which is the old
    /// behaviour applied one level down.
    #[test]
    fn covering_is_two_deep_and_does_not_leak() {
        let mut world = World::new();
        world.load_baseline(&baseline(vec![entity("iron-ore", 1.5, 2.5)], Vec::new()));

        world.apply(Some("nauvis"), &add("transport-belt", 1.5, 2.5, Some(1)));
        world.apply(Some("nauvis"), &add("pipe", 1.5, 2.5, Some(2)));
        assert_eq!(world.entity_count(), 2, "the ore fell out, the belt did not");

        assert!(world.apply(None, &remove_by_id(2, 1.5, 2.5)));
        assert!(world.apply(None, &remove_by_id(1, 1.5, 2.5)));
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
            // Aquilo's frozen twins. Without these an entire Aquilo base's
            // paving is invisible to live capture, since the mod would never
            // log it as a tile the player placed.
            "frozen-stone-path",
            "frozen-concrete",
            "frozen-hazard-concrete-left",
            "frozen-hazard-concrete-right",
            "frozen-refined-concrete",
            "frozen-refined-hazard-concrete-left",
            "frozen-refined-hazard-concrete-right",
        ] {
            assert!(is_placed_floor(name), "{name} should be placed floor");
        }

        // Aquilo's own natural ground, which must not be mistaken for floor
        // just because everything on that planet is frozen.
        for name in ["grass-1", "water", "sand-1", "deepwater", "dirt-3", "snow-flat", "ice-smooth", "ammoniacal-ocean"] {
            assert!(!is_placed_floor(name), "{name} should be natural terrain");
        }
    }

    /// What the capture says outranks the list above: the mod works its answer
    /// out from the loaded prototypes, so it is the only side that can know a
    /// platform's foundation is something somebody laid.
    ///
    /// Not cosmetic. Terrain is seeded once and never touched again, so floor
    /// filed as terrain can never be removed: mining it up would leave it on
    /// screen for the rest of the timelapse.
    #[test]
    fn the_captures_own_floor_list_decides_the_split() {
        let tiles = vec![
            crate::frame::Tile { n: Arc::from("space-platform-foundation"), x: 0, y: 0 },
            crate::frame::Tile { n: Arc::from("vegetation-green-grass-1"), x: 1, y: 0 },
        ];

        let mut told = World::new();
        told.set_floor(["space-platform-foundation".to_string()].into_iter().collect());
        told.load_baseline(&baseline(Vec::new(), tiles.clone()));
        let surface = told.surface("nauvis").expect("a surface");
        assert_eq!(surface.floor_tile_count(), 1, "the platform's own foundation is floor somebody laid");
        assert_eq!(surface.terrain_tile_count(), 1, "and the modded grass is not");

        // A capture that said nothing gets the built-in list, which knows
        // neither name, so both read as ground. That is the old behaviour and
        // has to stay it: every capture recorded so far is this one.
        let mut silent = World::new();
        silent.load_baseline(&baseline(Vec::new(), tiles));
        let surface = silent.surface("nauvis").expect("a surface");
        assert_eq!(surface.floor_tile_count(), 0);
        assert_eq!(surface.terrain_tile_count(), 2);
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
