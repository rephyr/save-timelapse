//! Interning entity/tile prototype names and choosing a color for each one.

use std::collections::HashMap;

use macroquad::color::Color;

/// Dense index into [`TypeRegistry`]: a base has tens of distinct names
/// against hundreds of thousands of entities.
pub type TypeId = u16;

/// Interns prototype names and resolves each one's colour and kind once, so
/// drawing never hashes a name.
#[derive(Default)]
pub struct TypeRegistry {
    names: Vec<String>,
    ids: HashMap<String, TypeId>,
    entity_colors: Vec<Color>,
    tile_colors: Vec<Color>,
    kinds: Vec<Kind>,
    /// What the running game said its own prototypes are, when the capture
    /// shipped with an answer. Consulted before every built-in table below.
    prototypes: Option<save_timelapse::prototypes::Prototypes>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Must be set before anything is interned: a name is resolved once, at
    /// intern time, so a description arriving later would apply to nothing.
    pub fn set_prototypes(&mut self, prototypes: save_timelapse::prototypes::Prototypes) {
        debug_assert!(self.names.is_empty(), "prototypes must be set before interning");
        self.prototypes = Some(prototypes);
    }

    /// Both variants precomputed: nothing guarantees a name is only one, and
    /// two `Color`s per type is nothing next to a hash per entity per frame.
    pub fn intern(&mut self, name: &str) -> TypeId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = TypeId::try_from(self.names.len()).expect("more than u16::MAX distinct type names");
        self.names.push(name.to_string());
        let from_game = self.prototypes.as_ref();
        let rgb = |c: &[u8; 3]| Color::new(c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, 1.0);
        let game_entity = from_game.and_then(|p| p.entities.get(name)).map(rgb);
        let game_tile = from_game.and_then(|p| p.tiles.get(name)).map(rgb);
        let color = known_color(name);
        // Entities take a shade of the map view's friendly blue, tiles the
        // full-hue hash: an unrecognised floor has no reason to be blue.
        self.entity_colors.push(game_entity.or(color).unwrap_or_else(|| friendly_shade(name)));
        self.tile_colors.push(game_tile.or(color).unwrap_or_else(|| color_for(name, 0.35, 0.5)));
        // Resolved here for the same reason the colours are: once per name
        // beats once per entity per frame.
        self.kinds.push(match from_game.and_then(|p| p.kind(name)) {
            Some(kind) => Kind::from_prototype_type(kind, name, from_game.and_then(|p| p.reach.get(name)).copied()),
            None => Kind::from_name(name),
        });
        self.ids.insert(name.to_string(), id);
        id
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn name(&self, id: TypeId) -> &str {
        &self.names[id as usize]
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn entity_color(&self, id: TypeId) -> Color {
        self.entity_colors[id as usize]
    }

    pub fn tile_color(&self, id: TypeId) -> Color {
        self.tile_colors[id as usize]
    }

    pub fn is_belt(&self, id: TypeId) -> bool {
        self.kinds[id as usize].belt
    }

    pub fn is_splitter(&self, id: TypeId) -> bool {
        self.kinds[id as usize].splitter
    }

    pub fn is_pipe(&self, id: TypeId) -> bool {
        self.kinds[id as usize].pipe
    }

    pub fn is_pipe_to_ground(&self, id: TypeId) -> bool {
        self.kinds[id as usize].pipe_to_ground
    }

    pub fn is_resource(&self, id: TypeId) -> bool {
        self.kinds[id as usize].resource
    }

    pub fn is_terrain_scatter(&self, id: TypeId) -> bool {
        self.kinds[id as usize].scatter
    }

    pub fn is_vehicle(&self, id: TypeId) -> bool {
        self.kinds[id as usize].vehicle
    }

    pub fn is_enemy(&self, id: TypeId) -> bool {
        self.kinds[id as usize].enemy
    }

    pub fn is_rotation_allowed(&self, id: TypeId) -> bool {
        self.kinds[id as usize].rotates
    }

    /// Whether somebody placed this rather than the map generating it. One
    /// definition for the two questions that need it: what the auto-follow
    /// camera aims at, and what the on-screen count counts. A capture keeps
    /// trees, ore and nests for context, and on a wooded map they outnumber
    /// the factory ten to one.
    pub fn is_built(&self, id: TypeId) -> bool {
        let kind = &self.kinds[id as usize];
        !kind.scatter && !kind.resource && !kind.enemy && !kind.vehicle
    }

    /// How far an underground belt of this type reaches, or `None` if it is
    /// not one. The reach is what pairs an entrance with its exit.
    pub fn underground_reach(&self, id: TypeId) -> Option<i32> {
        self.kinds[id as usize].reach
    }
}

/// What one prototype name is, as far as drawing cares. The name lists it
/// replaced survive as `from_name`, for captures the mod never described.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Kind {
    belt: bool,
    splitter: bool,
    pipe: bool,
    pipe_to_ground: bool,
    resource: bool,
    scatter: bool,
    vehicle: bool,
    enemy: bool,
    rotates: bool,
    reach: Option<i32>,
}

impl Kind {
    /// From the game's own prototype type.
    ///
    /// A splitter can also be a `lane-splitter` and an `infinity-pipe` is a
    /// pipe, while a `heat-pipe` is not. Worms are the bare `turret` type and
    /// the player's defences are not, which is what makes typing enemies safe.
    fn from_prototype_type(kind: &str, name: &str, reach: Option<i32>) -> Self {
        let belt = kind == "transport-belt";
        Self {
            belt,
            splitter: matches!(kind, "splitter" | "lane-splitter"),
            pipe: matches!(kind, "pipe" | "infinity-pipe"),
            pipe_to_ground: kind == "pipe-to-ground",
            resource: kind == "resource",
            scatter: matches!(kind, "tree" | "plant" | "cliff"),
            vehicle: matches!(
                kind,
                "car" | "spider-vehicle" | "spider-leg" | "locomotive" | "cargo-wagon" | "fluid-wagon" | "artillery-wagon"
            ),
            // The one prototype whose type lies about its side: a captive
            // biter spawner is `unit-spawner` and player built.
            enemy: matches!(kind, "unit" | "unit-spawner" | "turret" | "spider-unit" | "segmented-unit" | "segment")
                && name != "captive-biter-spawner",
            // Belts only: chevrons are a flat top-down icon that rotates
            // honestly, where an oblique render just spins its camera angle.
            rotates: belt,
            reach: (kind == "underground-belt").then_some(reach).flatten(),
        }
    }

    /// From the name alone, for a capture whose mod never described its
    /// prototypes. Defers to the free functions so the two cannot drift.
    fn from_name(name: &str) -> Self {
        Self {
            belt: is_belt(name),
            splitter: is_splitter(name),
            pipe: is_pipe(name),
            pipe_to_ground: is_pipe_to_ground(name),
            resource: is_resource(name),
            scatter: is_terrain_scatter(name),
            vehicle: is_vehicle(name),
            enemy: is_enemy(name),
            rotates: is_rotation_allowed(name),
            reach: underground_reach(name),
        }
    }
}

/// A curated colour for terrain and scatter names, checked before the hash.
/// Best-effort: hashing is a fine fallback for anything unrecognised.
fn known_color(name: &str) -> Option<Color> {
    let rgb = |r: u8, g: u8, b: u8| Color::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0);

    // Natural terrain.
    if name == "water" || name.starts_with("water-") {
        return Some(rgb(29, 78, 130));
    }
    if name == "deepwater" || name.starts_with("deepwater-") {
        return Some(rgb(16, 52, 92));
    }
    if name.starts_with("grass") {
        return Some(rgb(76, 104, 46));
    }
    if name.starts_with("dirt") || name == "dry-dirt" {
        return Some(rgb(107, 84, 60));
    }
    if name.starts_with("sand") {
        return Some(rgb(184, 165, 116));
    }
    if name.starts_with("red-desert") {
        return Some(rgb(140, 92, 68));
    }
    if name == "out-of-map" {
        return Some(rgb(8, 8, 8));
    }

    // Player-built structures, following the game's own chart colours: rails
    // grey, belts yellow, everything else one friendly blue.
    if RAIL_TRACK.contains(&name) {
        // The game lightens elevated rail and ramps so a raised line is
        // distinguishable from the ground it crosses, which is worth keeping.
        if name.starts_with("elevated-") {
            return Some(rgb(186, 186, 186));
        }
        if name == "rail-ramp" {
            return Some(rgb(166, 166, 166));
        }
        return Some(rgb(140, 140, 140));
    }
    if name.ends_with("splitter") {
        return Some(rgb(255, 209, 0));
    }
    if name.ends_with("underground-belt") {
        return Some(rgb(112, 92, 0));
    }
    if name.ends_with("transport-belt") {
        return Some(rgb(204, 161, 71));
    }
    if name == "heat-pipe" {
        return Some(rgb(58, 130, 172));
    }
    if name == "pipe-to-ground" {
        return Some(rgb(25, 103, 150));
    }
    if name == "pipe" {
        return Some(rgb(69, 130, 165));
    }
    if name == "storage-tank" {
        return Some(rgb(131, 166, 188));
    }
    if name.ends_with("-wall") {
        return Some(rgb(204, 217, 204));
    }
    if name == "gate" {
        return Some(rgb(128, 128, 128));
    }
    if name.ends_with("-turret") {
        return Some(rgb(202, 167, 24));
    }
    if name == "roboport" {
        return Some(rgb(211, 207, 136));
    }
    if name == "solar-panel" {
        return Some(rgb(31, 33, 36));
    }
    if name == "accumulator" {
        return Some(rgb(122, 122, 122));
    }
    if name == "beacon" {
        return Some(rgb(7, 68, 104));
    }

    // Space Age planets, from each tile prototype's own `map_color`. Grouped
    // by prefix, most of a family sharing one colour.

    // Vulcanus: near-black rock and ash, lava the only bright thing on it.
    if name == "lava-hot" {
        return Some(rgb(255, 138, 57));
    }
    if name.starts_with("lava") {
        return Some(rgb(150, 49, 30));
    }
    if name.starts_with("volcanic") {
        // "hot" and "warm" are the ground around lava and are tinted towards
        // it in game.
        if name.contains("hot") || name.contains("warm") {
            return Some(rgb(33, 13, 10));
        }
        if name.contains("ash-cracks") {
            return Some(rgb(39, 39, 39));
        }
        if name.contains("soil") {
            return Some(rgb(24, 21, 15));
        }
        return Some(rgb(25, 25, 25));
    }

    // Fulgora: brown throughout, with the oil oceans darker than the islands
    // and the deep ocean darker still.
    if name == "oil-ocean-deep" {
        return Some(rgb(56, 36, 40));
    }
    if name.starts_with("oil-ocean") {
        return Some(rgb(74, 42, 43));
    }
    if name.starts_with("fulgoran") {
        if name.contains("paving") {
            return Some(rgb(120, 94, 67));
        }
        // The ruins: conduit and machinery read greyer than the sand around
        // them, which is what distinguishes a ruin field from open ground.
        if name.contains("machinery") || name.contains("conduit") {
            return Some(rgb(93, 79, 68));
        }
        if name.contains("rock") {
            return Some(rgb(131, 85, 66));
        }
        return Some(rgb(120, 70, 58));
    }

    // Gleba: brown and olive ground with strongly coloured biomes over it.
    if name.starts_with("gleba-deep-lake") {
        return Some(rgb(18, 37, 51));
    }
    if name == "pit-rock" {
        return Some(rgb(22, 22, 30));
    }
    if name.contains("yumako") {
        return Some(rgb(204, 183, 6));
    }
    if name.contains("jellynut") {
        return Some(rgb(204, 6, 183));
    }
    if name.starts_with("wetland") {
        if name.contains("green-slime") {
            return Some(rgb(28, 56, 28));
        }
        if name.contains("tentacle") {
            return Some(rgb(58, 17, 28));
        }
        if name.contains("blue-slime") {
            return Some(rgb(25, 49, 58));
        }
        return Some(rgb(48, 47, 53));
    }
    if name.starts_with("lowland") {
        if name.contains("red-vein") || name.contains("red-infection") {
            return Some(rgb(115, 53, 66));
        }
        if name.contains("cream") || name.contains("dead-skin") {
            return Some(rgb(95, 93, 88));
        }
        return Some(rgb(66, 82, 11));
    }
    if name.starts_with("midland") {
        if name.contains("turquoise-bark") {
            return Some(rgb(46, 68, 48));
        }
        if name.contains("yellow-crust") {
            return Some(rgb(114, 86, 40));
        }
        return Some(rgb(75, 71, 41));
    }
    if name.starts_with("highland") {
        return Some(rgb(52, 55, 48));
    }

    // Aquilo: pale snow, mid blue ice, near-black ammoniacal ocean. Keeping
    // the three bands apart is what matters.
    if name.starts_with("ammoniacal-ocean") {
        return Some(rgb(16, 14, 27));
    }
    if name.starts_with("brash-ice") {
        return Some(rgb(21, 42, 56));
    }
    if name == "ice-platform" {
        return Some(rgb(95, 122, 156));
    }
    if name.starts_with("ice-") {
        return Some(rgb(100, 135, 177));
    }
    // Snow and dust share one palette, graded by how much ground shows
    // through. The game interpolates these, so these are the computed ends.
    if name.starts_with("snow-") || name.starts_with("dust-") {
        if name.ends_with("patchy") {
            return Some(rgb(156, 166, 181));
        }
        if name.ends_with("lumpy") {
            return Some(rgb(166, 174, 186));
        }
        if name.ends_with("crests") {
            return Some(rgb(180, 186, 192));
        }
        return Some(rgb(190, 194, 197));
    }

    // Resource deposits, matching the game's map-view resource palette rather
    // than the in-world ore sprite. Without Space Age's, calcite and scrap
    // rendered as buildings once structures went blue.
    if name == "iron-ore" {
        return Some(rgb(140, 165, 200));
    }
    if name == "copper-ore" {
        return Some(rgb(203, 106, 54));
    }
    if name == "coal" {
        return Some(rgb(40, 40, 40));
    }
    if name == "stone" {
        return Some(rgb(178, 158, 130));
    }
    if name == "uranium-ore" {
        return Some(rgb(140, 168, 60));
    }
    if name == "crude-oil" {
        return Some(rgb(126, 44, 96));
    }
    if name == "tungsten-ore" {
        return Some(rgb(98, 86, 150));
    }
    if name == "calcite" {
        return Some(rgb(204, 179, 179));
    }
    if name == "scrap" {
        return Some(rgb(230, 230, 230));
    }
    if name == "sulfuric-acid-geyser" {
        return Some(rgb(199, 199, 26));
    }
    if name == "fluorine-vent" {
        return Some(rgb(179, 255, 153));
    }
    if name == "lithium-brine" {
        return Some(rgb(0, 204, 255));
    }

    // Placed infrastructure common enough to have a strong expected color.
    if name.starts_with("refined-hazard-concrete") || name.starts_with("hazard-concrete") {
        return Some(rgb(196, 160, 40));
    }
    if name == "concrete" || name == "refined-concrete" {
        // Dark enough that entities drawn from the bright hash palette stay
        // readable against it.
        return Some(rgb(58, 58, 60));
    }
    if name == "stone-path" {
        return Some(rgb(146, 126, 104));
    }
    if name == "landfill" {
        return Some(rgb(107, 84, 60));
    }

    // Enemies, red so clearing a nest reads at a glance. Worms take the
    // lighter shade so a nest and the worms around it stay distinguishable.
    if is_enemy(name) {
        // Before the nest shade, so a name that is somehow both lands
        // deterministically.
        if name.ends_with("-worm-turret") {
            return Some(rgb(220, 74, 60));
        }
        return Some(rgb(168, 34, 30));
    }

    // Terrain scatter: cliffs read as bare rock, live trees green, and
    // "dead"/"dry" variants dead-wood brown.
    if is_terrain_scatter(name) {
        // Each planet's cliffs are its own rock, so they take that planet's
        // stone rather than one shared grey that would look imported.
        if name == "cliff-vulcanus" {
            return Some(rgb(45, 42, 40));
        }
        if name == "cliff-fulgora" {
            return Some(rgb(96, 70, 58));
        }
        if name == "cliff-gleba" {
            return Some(rgb(78, 80, 62));
        }
        if name.starts_with("cliff") {
            return Some(rgb(96, 92, 88));
        }
        if name.starts_with("dead-") || name.starts_with("dry-") {
            return Some(rgb(107, 92, 66));
        }
        // Vulcanus lichen is ash-grey scrub on black rock, not forest.
        if name.starts_with("ashland-lichen") {
            return Some(rgb(74, 78, 62));
        }
        return Some(rgb(53, 89, 42));
    }

    None
}

/// Whether `name` is an enemy structure, for captures with no
/// `prototypes.json`.
///
/// Substring rather than a list, so a modded nest is caught for free, and not
/// a suffix because Space Age ships `gleba-spawner-small`.
/// `captive-biter-spawner` is the exception: player-crafted despite the name.
///
/// Pentapods and demolishers are here for captures recorded before
/// `EXCLUDED_TYPES` covered Space Age's enemy types.
fn is_enemy(name: &str) -> bool {
    if name == "captive-biter-spawner" {
        return false;
    }
    name.contains("spawner") || name.ends_with("-worm-turret") || name.contains("pentapod") || name.contains("demolisher")
}

/// Whether `name` drives, walks or rolls. Excluded from capture now, named
/// here for captures written before that.
fn is_vehicle(name: &str) -> bool {
    matches!(name, "car" | "tank" | "spidertron" | "locomotive" | "cargo-wagon" | "fluid-wagon" | "artillery-wagon")
        || name.starts_with("spidertron-leg-")
}

/// Whether `name` is a tree or cliff. Shared with `construction.rs`'s
/// auto-follow bounds, which must exclude scatter the map generated.
fn is_terrain_scatter(name: &str) -> bool {
    // `cliff-` rather than `cliff`: every planet has its own, and nothing
    // else can be one, so a new planet needs no edit here.
    name == "cliff"
        || name.starts_with("cliff-")
        || name.starts_with("dead-")
        || name.starts_with("dry-")
        || name.starts_with("tree")
        || OFF_WORLD_FLORA.contains(&name)
}

/// Every flora prototype outside Nauvis, named exactly. Only Nauvis shares
/// `tree`, `dead-` and `dry-` prefixes, and `cryogenic-plant` and
/// `electromagnetic-plant` are crafting machines.
const OFF_WORLD_FLORA: &[&str] = &[
    // Vulcanus
    "ashland-lichen-tree",
    "ashland-lichen-tree-flaming",
    // Gleba
    "boompuff",
    "cuttlepop",
    "funneltrunk",
    "hairyclubnub",
    "jellystem",
    "lickmaw",
    "slipstack",
    "stingfrond",
    "sunnycomb",
    "teflilly",
    "water-cane",
    "yumako-tree",
];

/// Whether `name` is a resource deposit. Shared with `construction.rs`'s
/// auto-follow bounds, since a resource sits where the map generated it.
/// Named exactly, resource names sharing no prefix.
fn is_resource(name: &str) -> bool {
    matches!(
        name,
        "iron-ore"
            | "copper-ore"
            | "coal"
            | "stone"
            | "uranium-ore"
            | "crude-oil"
            | "tungsten-ore"
            | "calcite"
            | "scrap"
            // The three that were missing entirely, so they counted as
            // construction and dragged the auto-follow camera toward them.
            | "sulfuric-acid-geyser"
            | "fluorine-vent"
            | "lithium-brine"
    )
}

/// Entity names safe to rotate: a flat top-down icon rather than an oblique
/// render, which just spins its fixed camera angle.
const ALWAYS_ROTATE: &[&str] = BELTS;

fn is_rotation_allowed(name: &str) -> bool {
    ALWAYS_ROTATE.contains(&name)
}

/// The four belt tiers, drawn from the in-world sheet rather than an icon.
/// Only these have a curved form to get wrong.
const BELTS: &[&str] = &["transport-belt", "fast-transport-belt", "express-transport-belt", "turbo-transport-belt"];

fn is_belt(name: &str) -> bool {
    BELTS.contains(&name)
}

/// Underground belt tiers and their reach in tiles, from `max_distance` in the
/// game's prototypes. The reach is what pairs an entrance with its exit.
const UNDERGROUNDS: &[(&str, i32)] =
    &[("underground-belt", 5), ("fast-underground-belt", 7), ("express-underground-belt", 9), ("turbo-underground-belt", 11)];

fn underground_reach(name: &str) -> Option<i32> {
    UNDERGROUNDS.iter().find(|(tier, _)| *tier == name).map(|(_, reach)| *reach)
}

/// Splitter tiers, which draw from four separate per-facing files rather than
/// from one sheet.
const SPLITTERS: &[&str] = &["splitter", "fast-splitter", "express-splitter", "turbo-splitter"];

/// Plain pipes, whose appearance comes from which sides join onto them.
/// `pipe-to-ground` is absent: it has fixed pictures and does not change.
fn is_pipe(name: &str) -> bool {
    name == "pipe"
}

/// Underground pipes, drawing one of four fixed pictures chosen by facing.
/// Unlike belts, the two ends carry different directions, so nothing pairs.
fn is_pipe_to_ground(name: &str) -> bool {
    name == "pipe-to-ground"
}

fn is_splitter(name: &str) -> bool {
    SPLITTERS.contains(&name)
}

/// Every prototype that is rail track. 2.0 split two into a family, and
/// hashing each made one line render as a rainbow. Named explicitly rather
/// than matched on "rail", which would swallow `rail-signal` and `gate-over-rail`.
const RAIL_TRACK: &[&str] = &[
    "straight-rail",
    "curved-rail-a",
    "curved-rail-b",
    "half-diagonal-rail",
    "legacy-straight-rail",
    "legacy-curved-rail",
    "elevated-straight-rail",
    "elevated-curved-rail-a",
    "elevated-curved-rail-b",
    "elevated-half-diagonal-rail",
    "rail-ramp",
    "rail-support",
];

/// The name a prototype is coloured by: its own, unless it belongs to a
/// family the eye reads as one thing. Rail is the only case so far.
fn color_group(name: &str) -> &str {
    if RAIL_TRACK.contains(&name) {
        return "rail";
    }
    // Aquilo's `frozen-` twin is the same path the player laid, so it must
    // not change colour because the weather did.
    if let Some(base) = name.strip_prefix("frozen-") {
        return base;
    }
    name
}

fn name_hash(name: &str) -> u32 {
    let mut hash: u32 = 2166136261;
    for b in name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

/// Deterministic name -> color, so a given entity type is always the same
/// color across runs with nothing to curate as new Factorio types show up.
pub fn color_for(name: &str, saturation: f32, value: f32) -> Color {
    let hue = (name_hash(color_group(name)) % 360) as f32 / 360.0;
    let (r, g, b) = hsv_to_rgb(hue, saturation, value);
    Color::new(r, g, b, 1.0)
}

/// A shade of the map view's friendly blue, for a structure with no colour of
/// its own. The game paints them all one blue, which would make a furnace and
/// a chest indistinguishable, so the hash varies lightness and saturation
/// within a narrow hue band instead.
fn friendly_shade(name: &str) -> Color {
    let hash = name_hash(color_group(name));
    let hue = (188 + hash % 26) as f32 / 360.0;
    let saturation = 0.55 + ((hash >> 8) % 35) as f32 / 100.0;
    let value = 0.42 + ((hash >> 16) % 48) as f32 / 100.0;
    let (r, g, b) = hsv_to_rgb(hue, saturation, value);
    Color::new(r, g, b, 1.0)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl TypeRegistry {
        /// A registry holding one name, so a test can ask what it resolves to
        /// through the real path.
        fn new_with(name: &str) -> TypeRegistry {
            let mut registry = TypeRegistry::new();
            registry.intern(name);
            registry
        }
    }

    /// A rail line is one structure and has to read as one: 2.0's split gave
    /// each variant its own hue.
    #[test]
    fn every_kind_of_rail_track_shares_one_color() {
        let reference = color_for("straight-rail", 0.6, 0.9);
        for name in RAIL_TRACK {
            assert_eq!(color_for(name, 0.6, 0.9), reference, "{name} must colour as rail");
        }
    }

    /// The reason the list is explicit rather than a substring match on
    /// "rail": all four of these contain it and none of them are track.
    #[test]
    fn things_that_merely_have_rail_in_the_name_keep_their_own_color() {
        let rail = color_for("straight-rail", 0.6, 0.9);
        for name in ["rail-signal", "rail-chain-signal", "gate-over-rail", "railgun-turret"] {
            assert_ne!(color_for(name, 0.6, 0.9), rail, "{name} is not track and must not colour as it");
        }
    }

    /// Every Space Age tile a capture can contain resolves to a curated colour
    /// rather than the hash.
    #[test]
    fn every_space_age_tile_family_has_a_curated_color() {
        for name in [
            // Vulcanus
            "lava",
            "lava-hot",
            "volcanic-jagged-ground",
            "volcanic-cracks-hot",
            "volcanic-ash-light",
            "volcanic-soil-dark",
            "volcanic-smooth-stone",
            // Fulgora
            "fulgoran-dust",
            "fulgoran-rock",
            "fulgoran-paving",
            "fulgoran-machinery",
            "oil-ocean-shallow",
            "oil-ocean-deep",
            // Gleba
            "natural-yumako-soil",
            "wetland-jellynut",
            "lowland-red-vein",
            "lowland-dead-skin",
            "lowland-olive-blubber",
            "midland-turquoise-bark",
            "midland-yellow-crust",
            "midland-cracked-lichen",
            "highland-dark-rock",
            "wetland-green-slime",
            "wetland-blue-slime",
            "gleba-deep-lake",
            "pit-rock",
            // Aquilo
            "ammoniacal-ocean",
            "ammoniacal-ocean-2",
            "brash-ice",
            "brash-ice-2",
            "ice-smooth",
            "ice-rough",
            "ice-platform",
            "snow-flat",
            "snow-crests",
            "snow-lumpy",
            "snow-patchy",
            "dust-flat",
            "dust-patchy",
        ] {
            assert!(known_color(name).is_some(), "{name} should have a curated color");
        }
    }

    /// Aquilo is one cold ramp, and the three bands staying apart is what
    /// makes its terrain readable.
    #[test]
    fn aquilo_reads_as_snow_then_ice_then_ocean() {
        let brightness = |c: Color| c.r + c.g + c.b;
        let snow = brightness(known_color("snow-flat").unwrap());
        let ice = brightness(known_color("ice-smooth").unwrap());
        let ocean = brightness(known_color("ammoniacal-ocean").unwrap());
        assert!(snow > ice, "snow must be brighter than ice");
        assert!(ice > ocean, "ice must be brighter than the ammoniacal ocean");
    }

    /// A path does not become a different material because the weather
    /// changed, so Aquilo's frozen floors colour as the floor they are.
    #[test]
    fn frozen_floor_colours_as_the_floor_it_is() {
        for base in ["concrete", "stone-path", "refined-concrete", "hazard-concrete-left"] {
            assert_eq!(
                color_for(&format!("frozen-{base}"), 0.6, 0.9),
                color_for(base, 0.6, 0.9),
                "frozen-{base} must match {base}"
            );
        }
    }

    /// Lava has to stay obviously brighter than the rock it sits in, which a
    /// later tweak to the greys could quietly close.
    #[test]
    fn lava_is_far_brighter_than_volcanic_rock() {
        let brightness = |c: Color| c.r + c.g + c.b;
        let rock = brightness(known_color("volcanic-ash-light").unwrap());
        assert!(brightness(known_color("lava").unwrap()) > rock * 2.0, "lava must stand out against rock");
        assert!(brightness(known_color("lava-hot").unwrap()) > rock * 2.0);
    }

    /// Fulgora's oil oceans have to be darker than its islands, and the deep
    /// ocean darker than the shallow, or the shape of the terrain is lost.
    #[test]
    fn fulgoras_oceans_are_darker_than_its_islands() {
        let brightness = |c: Color| c.r + c.g + c.b;
        let island = brightness(known_color("fulgoran-rock").unwrap());
        let shallow = brightness(known_color("oil-ocean-shallow").unwrap());
        let deep = brightness(known_color("oil-ocean-deep").unwrap());
        assert!(shallow < island, "shallow ocean must be darker than the islands");
        assert!(deep < shallow, "deep ocean must be darker than shallow");
    }

    /// The three families Factorio's map view makes instantly recognisable:
    /// grey rails, yellow belts, blue everything else.
    #[test]
    fn structures_follow_the_games_map_palette() {
        let grey = |c: Color| (c.r - c.g).abs() < 0.02 && (c.g - c.b).abs() < 0.02;
        for rail in ["straight-rail", "curved-rail-a", "legacy-straight-rail"] {
            assert!(grey(known_color(rail).unwrap()), "{rail} should be grey");
        }

        // Yellow: red and green high, blue low.
        for belt in ["transport-belt", "fast-transport-belt", "turbo-transport-belt", "express-splitter"] {
            let c = known_color(belt).unwrap();
            assert!(c.r > 0.4 && c.g > 0.3 && c.b < c.g, "{belt} should read yellow, got {c:?}");
        }
    }

    /// Everything without a colour of its own is a shade of blue, the way the
    /// map view paints every player structure it has no specific colour for.
    #[test]
    fn ordinary_machines_are_shades_of_blue() {
        for name in ["assembling-machine-1", "stone-furnace", "iron-chest", "lab", "steel-furnace"] {
            let c = TypeRegistry::new_with(name).entity_color(0);
            assert!(c.b > c.r && c.b > c.g, "{name} should be blue-dominant, got {c:?}");
        }
    }

    /// ...but not the *same* blue, or a base becomes one undifferentiated
    /// smear and the renderer stops saying anything about what is where.
    #[test]
    fn different_machines_are_different_shades() {
        let a = TypeRegistry::new_with("assembling-machine-1").entity_color(0);
        let b = TypeRegistry::new_with("stone-furnace").entity_color(0);
        let apart = (a.r - b.r).abs() + (a.g - b.g).abs() + (a.b - b.b).abs();
        assert!(apart > 0.08, "two machine types should be tellable apart, got {apart}");
    }

    /// Flora on the other planets is scenery like a Nauvis forest.
    /// `construction.rs` and `activity.rs` both skip scatter, so while these
    /// went unrecognised a Gleba forest dragged the auto-follow camera.
    #[test]
    fn flora_on_every_planet_counts_as_scenery() {
        for name in [
            "tree-01",
            "dead-grey-trunk",
            "cliff",
            "yumako-tree",
            "jellystem",
            "boompuff",
            "stingfrond",
            "water-cane",
            "ashland-lichen-tree",
        ] {
            assert!(is_terrain_scatter(name), "{name} should be scenery, not a structure");
            assert!(known_color(name).is_some(), "{name} should have a curated colour");
        }
    }

    /// Every resource is a deposit, on every planet. Missing from
    /// `is_resource` it drags the camera; missing from `known_color` it
    /// renders as a building.
    #[test]
    fn every_resource_on_every_planet_is_a_deposit() {
        for name in [
            "iron-ore",
            "copper-ore",
            "coal",
            "stone",
            "uranium-ore",
            "crude-oil",
            "tungsten-ore",
            "calcite",
            "scrap",
            "sulfuric-acid-geyser",
            "fluorine-vent",
            "lithium-brine",
        ] {
            assert!(is_resource(name), "{name} should be a resource");
            assert!(known_color(name).is_some(), "{name} should have a curated colour, not a blue shade");
        }
    }

    /// Every planet has its own cliff prototype. Matching the bare name
    /// alone left the other three rendering as structures.
    #[test]
    fn every_planets_cliffs_are_scenery_in_that_planets_stone() {
        let nauvis = known_color("cliff").unwrap();
        for name in ["cliff", "cliff-vulcanus", "cliff-fulgora", "cliff-gleba"] {
            assert!(is_terrain_scatter(name), "{name} should be scenery");
            assert!(known_color(name).is_some(), "{name} should have a curated colour");
        }
        // Each planet's own rock, not one shared grey that would look
        // imported from Nauvis.
        for name in ["cliff-vulcanus", "cliff-fulgora", "cliff-gleba"] {
            assert_ne!(known_color(name).unwrap(), nauvis, "{name} should not be Nauvis grey");
        }
    }

    /// The reason the flora list is exact: all three of these read as flora
    /// to a substring match and are in fact machines and a turret.
    #[test]
    fn machines_that_sound_like_flora_are_not_scenery() {
        for name in ["cryogenic-plant", "electromagnetic-plant", "rocket-turret"] {
            assert!(!is_terrain_scatter(name), "{name} is a structure, not scenery");
        }
    }

    /// Vulcanus lichen must not be Nauvis forest green, or a volcanic planet
    /// ends up dotted with another world's vegetation.
    #[test]
    fn vulcanus_lichen_is_not_forest_green() {
        assert_ne!(known_color("ashland-lichen-tree"), known_color("tree-01"));
    }

    /// Grouping must not leak: two unrelated prototypes still get their own
    /// colours, which is the property the hash exists for.
    #[test]
    fn unrelated_types_still_differ() {
        assert_ne!(color_for("transport-belt", 0.6, 0.9), color_for("stone-furnace", 0.6, 0.9));
    }

    /// Real names a live capture produced. Pinning that they resolve to a
    /// curated colour guards against a substring check silently stopping.
    #[test]
    fn known_color_recognizes_real_captured_terrain_names() {
        for name in [
            "water",
            "deepwater",
            "grass-1",
            "grass-4",
            "dirt-3",
            "dry-dirt",
            "sand-2",
            "red-desert-1",
            "out-of-map",
            "concrete",
            "refined-concrete",
            "hazard-concrete-left",
            "refined-hazard-concrete-right",
            "stone-path",
            "landfill",
            "cliff",
            "tree-01",
            "tree-09-brown",
            "dead-tree-desert",
            "dry-hairy-tree",
        ] {
            assert!(known_color(name).is_some(), "{name} should have a curated color");
        }
    }

    /// Distinct colours, not just present: iron and copper looking alike is
    /// the easy mistake when copy-pasting six similar `if` blocks.
    #[test]
    fn known_color_recognizes_resource_deposits_distinctly() {
        let iron = known_color("iron-ore").unwrap();
        let copper = known_color("copper-ore").unwrap();
        let coal = known_color("coal").unwrap();
        for name in ["iron-ore", "copper-ore", "coal", "stone", "uranium-ore", "crude-oil"] {
            assert!(known_color(name).is_some(), "{name} should have a curated color");
        }
        assert_ne!((iron.r, iron.g, iron.b), (copper.r, copper.g, copper.b));
        assert!(coal.r < 0.3 && coal.g < 0.3 && coal.b < 0.3, "coal should read as near-black");
    }

    #[test]
    fn is_terrain_scatter_recognizes_trees_and_cliffs_not_ordinary_entities() {
        for name in ["cliff", "tree-01", "tree-09-brown", "dead-tree-desert", "dry-hairy-tree"] {
            assert!(is_terrain_scatter(name), "{name} should be recognized as terrain scatter");
        }
        for name in ["transport-belt", "assembling-machine-1", "stone-furnace"] {
            assert!(!is_terrain_scatter(name), "{name} is a real building, not terrain scatter");
        }
    }

    #[test]
    fn is_resource_recognizes_ore_and_oil_not_ordinary_entities() {
        for name in ["iron-ore", "copper-ore", "coal", "stone", "uranium-ore", "crude-oil"] {
            assert!(is_resource(name), "{name} should be recognized as a resource");
        }
        for name in ["transport-belt", "assembling-machine-1", "pumpjack"] {
            assert!(!is_resource(name), "{name} is a real building, not a resource deposit");
        }
    }

    #[test]
    fn is_rotation_allowed_recognizes_flat_icon_belts_not_oblique_ones() {
        for name in ["transport-belt", "fast-transport-belt", "express-transport-belt", "turbo-transport-belt"] {
            assert!(is_rotation_allowed(name), "{name}'s icon is flat and top-down, confirmed safe to rotate");
        }
        for name in [
            "inserter",
            "assembling-machine-1",
            "chemical-plant",
            "stone-furnace",
            "iron-chest",
            "lab",
            "electric-mining-drill",
            "pumpjack",
        ] {
            assert!(!is_rotation_allowed(name), "{name} is not on the curated allowlist");
        }
    }

    /// A machine with no chart colour of its own picks up a shade of the
    /// friendly blue. Belts and rails no longer qualify, the game giving those
    /// their own.
    #[test]
    fn known_color_falls_back_to_none_for_ordinary_factory_entities() {
        for name in ["assembling-machine-1", "electric-furnace", "iron-chest"] {
            assert!(known_color(name).is_none(), "{name} should fall back to a blue shade, not a curated color");
        }
    }

    /// Every vanilla nest and worm, taken from the real prototype names in
    /// base/ and space-age/ rather than from memory.
    #[test]
    fn every_vanilla_nest_and_worm_is_recognized_as_an_enemy() {
        for name in [
            "biter-spawner",
            "spitter-spawner",
            "gleba-spawner",
            "gleba-spawner-small",
            "small-worm-turret",
            "medium-worm-turret",
            "big-worm-turret",
            "behemoth-worm-turret",
        ] {
            assert!(is_enemy(name), "{name} should be an enemy");
            assert!(known_color(name).is_some(), "{name} should be colored red, not hashed");
        }
    }

    /// All found in a real Gleba capture, having slipped past the `unit`
    /// filter because Space Age gave its enemies their own types. They roam,
    /// so counting them held the auto-follow box open.
    #[test]
    fn glebas_pentapods_are_enemies_despite_not_being_units() {
        for name in [
            "small-stomper-pentapod",
            "small-stomper-pentapod-leg",
            "small-strafer-pentapod",
            "small-strafer-pentapod-leg",
            "medium-wriggler-pentapod",
            "big-stomper-pentapod",
        ] {
            assert!(is_enemy(name), "{name} should be an enemy");
        }
    }

    /// Demolishers, the other half of the same miss: a `segmented-unit` head
    /// trailing `segment` bodies.
    #[test]
    fn vulcanus_demolishers_are_enemies() {
        for name in ["small-demolisher", "medium-demolisher", "big-demolisher", "small-demolisher-segment"] {
            assert!(is_enemy(name), "{name} should be an enemy");
        }
    }

    #[test]
    fn vehicles_and_rolling_stock_are_recognized() {
        for name in ["car", "tank", "spidertron", "spidertron-leg-1", "locomotive", "cargo-wagon", "fluid-wagon"] {
            assert!(is_vehicle(name), "{name} moves and should be recognized as a vehicle");
        }
    }

    /// The track a train runs on is stationary and stays. Getting this wrong
    /// would erase the rail network.
    #[test]
    fn rails_and_stations_are_not_vehicles() {
        for name in [
            "straight-rail",
            "curved-rail-a",
            "rail-signal",
            "rail-chain-signal",
            "train-stop",
            "artillery-turret",
            "car-battery",
        ] {
            assert!(!is_vehicle(name), "{name} does not move and must be kept");
        }
    }

    /// The trap this exists for: a player-crafted assembling machine that
    /// matches the `-spawner` suffix.
    #[test]
    fn the_player_built_captive_biter_spawner_is_not_an_enemy() {
        assert!(!is_enemy("captive-biter-spawner"));
        assert!(
            known_color("captive-biter-spawner").is_none(),
            "it must fall through to the ordinary hash palette like any other building"
        );
    }

    #[test]
    fn ordinary_buildings_and_terrain_are_not_enemies() {
        for name in ["transport-belt", "assembling-machine-1", "gun-turret", "laser-turret", "tree-01", "water"] {
            assert!(!is_enemy(name), "{name} must not be an enemy");
        }
    }

    /// Nests and worms are told apart by shade, since they generally
    /// generate together in one cluster.
    #[test]
    fn nests_and_worms_are_distinguishable_reds() {
        let nest = known_color("biter-spawner").unwrap();
        let worm = known_color("big-worm-turret").unwrap();
        assert_ne!((nest.r, nest.g, nest.b), (worm.r, worm.g, worm.b));
        for c in [nest, worm] {
            assert!(c.r > c.g && c.r > c.b, "an enemy color must read as red");
        }
    }

    #[test]
    fn hsv_to_rgb_known_values() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), (1.0, 0.0, 0.0));
        let (r, g, b) = hsv_to_rgb(0.5, 0.0, 0.7);
        assert!((r - 0.7).abs() < 1e-6 && (g - 0.7).abs() < 1e-6 && (b - 0.7).abs() < 1e-6);
    }

    #[test]
    fn color_for_is_deterministic() {
        let a = color_for("transport-belt", 0.55, 0.85);
        let b = color_for("transport-belt", 0.55, 0.85);
        assert_eq!((a.r, a.g, a.b), (b.r, b.g, b.b));
    }

    #[test]
    fn registry_interns_repeated_names_to_one_id() {
        let mut registry = TypeRegistry::new();
        let belt = registry.intern("transport-belt");
        let pipe = registry.intern("pipe");
        assert_eq!(registry.intern("transport-belt"), belt);
        assert_ne!(belt, pipe);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.name(belt), "transport-belt");
    }

    /// The registry has to agree with the functions it caches. The two lists
    /// deliberately disagree for an uncurated name, as an entity gets a blue
    /// shade and as a tile keeps the hash, so both are asserted.
    #[test]
    fn registry_colors_match_the_functions_behind_them() {
        let name = "assembling-machine-1";
        let mut registry = TypeRegistry::new();
        let id = registry.intern(name);

        let entity = friendly_shade(name);
        let tile = color_for(name, 0.35, 0.5);
        let rgb = |c: Color| (c.r, c.g, c.b);
        assert_eq!(rgb(registry.entity_color(id)), rgb(entity), "entities use the friendly blue shade");
        assert_eq!(rgb(registry.tile_color(id)), rgb(tile), "tiles keep the hash");
    }

    /// A curated name uses its curated colour in *both* lists, so a belt is
    /// the same yellow whichever list happens to be consulted.
    #[test]
    fn a_curated_name_uses_its_curated_color_in_both_lists() {
        let mut registry = TypeRegistry::new();
        let id = registry.intern("transport-belt");
        let curated = known_color("transport-belt").expect("belts are curated");
        assert_eq!(registry.entity_color(id).r, curated.r);
        assert_eq!(registry.tile_color(id).r, curated.r);
    }

    fn typed(pairs: &[(&str, &str)], reach: &[(&str, i32)]) -> TypeRegistry {
        let mut registry = TypeRegistry::new();
        registry.set_prototypes(save_timelapse::prototypes::Prototypes {
            types: pairs.iter().map(|(n, t)| ((*n).to_string(), (*t).to_string())).collect(),
            reach: reach.iter().map(|(n, r)| ((*n).to_string(), *r)).collect(),
            ..Default::default()
        });
        registry
    }

    /// Names no list here has heard of, recognised because the game said so.
    /// All four are real Krastorio2 prototypes, and all four were
    /// unrecognised under the name matching this replaced.
    #[test]
    fn a_modded_prototype_is_recognised_from_its_type() {
        let mut registry = typed(
            &[
                ("kr-advanced-transport-belt", "transport-belt"),
                ("kr-advanced-underground-belt", "underground-belt"),
                ("kr-fluid-pipe", "pipe"),
                ("imersite", "resource"),
            ],
            &[("kr-advanced-underground-belt", 30)],
        );

        let belt = registry.intern("kr-advanced-transport-belt");
        assert!(registry.is_belt(belt), "a modded belt is a belt");
        assert!(registry.is_rotation_allowed(belt), "and so turns with its chevrons");

        let underground = registry.intern("kr-advanced-underground-belt");
        assert_eq!(registry.underground_reach(underground), Some(30), "the game's reach, not a vanilla tier's");
        assert!(!registry.is_belt(underground));

        let pipe = registry.intern("kr-fluid-pipe");
        assert!(registry.is_pipe(pipe));
        let ore = registry.intern("imersite");
        assert!(registry.is_resource(ore), "modded ore must not count as construction");
    }

    /// Types the vanilla names never had to tell apart, and one that must not
    /// be folded in: `heat-pipe` is its own artwork entirely.
    #[test]
    fn related_types_are_told_apart() {
        let mut registry = typed(
            &[
                ("lane-splitter", "lane-splitter"),
                ("infinity-pipe", "infinity-pipe"),
                ("heat-pipe", "heat-pipe"),
                ("gun-turret", "ammo-turret"),
                ("small-worm-turret", "turret"),
            ],
            &[],
        );

        let (lane, infinity, heat) =
            (registry.intern("lane-splitter"), registry.intern("infinity-pipe"), registry.intern("heat-pipe"));
        assert!(registry.is_splitter(lane));
        assert!(registry.is_pipe(infinity));
        assert!(!registry.is_pipe(heat), "a heat pipe is not a fluid pipe");

        let (gun, worm) = (registry.intern("gun-turret"), registry.intern("small-worm-turret"));
        assert!(!registry.is_enemy(gun), "the player's own defences are their own types");
        assert!(registry.is_enemy(worm), "a worm is the bare turret type");
    }

    /// A captive spawner is `unit-spawner` and player built, so type alone
    /// would stop it counting as construction.
    #[test]
    fn a_captive_spawner_is_not_an_enemy_despite_its_type() {
        let mut registry = typed(&[("captive-biter-spawner", "unit-spawner"), ("biter-spawner", "unit-spawner")], &[]);
        let (captive, wild) = (registry.intern("captive-biter-spawner"), registry.intern("biter-spawner"));
        assert!(!registry.is_enemy(captive));
        assert!(registry.is_enemy(wild));
    }

    /// Every capture that exists today has no such file, so the name lists
    /// have to keep answering for them exactly as they did before.
    #[test]
    fn without_the_game_talking_names_still_answer() {
        let mut registry = TypeRegistry::new();
        let ids: Vec<TypeId> = [
            "transport-belt",
            "fast-underground-belt",
            "pipe",
            "iron-ore",
            "tree-01",
            "biter-spawner",
            "kr-advanced-transport-belt",
        ]
        .iter()
        .map(|n| registry.intern(n))
        .collect();
        assert!(registry.is_belt(ids[0]));
        assert_eq!(registry.underground_reach(ids[1]), Some(7));
        assert!(registry.is_pipe(ids[2]));
        assert!(registry.is_resource(ids[3]));
        assert!(registry.is_terrain_scatter(ids[4]));
        assert!(registry.is_enemy(ids[5]));
        assert!(!registry.is_belt(ids[6]), "and know nothing of a mod, as before");
    }

    /// Names the file leaves out must fall back rather than come back blank:
    /// a modded capture still holds vanilla belts.
    #[test]
    fn a_name_the_file_omits_falls_back_to_its_own_answer() {
        let mut registry = typed(&[("kr-advanced-transport-belt", "transport-belt")], &[]);
        let (belt, underground) = (registry.intern("transport-belt"), registry.intern("underground-belt"));
        assert!(registry.is_belt(belt), "unmentioned, so answered by name");
        assert_eq!(registry.underground_reach(underground), Some(5));
    }
}
