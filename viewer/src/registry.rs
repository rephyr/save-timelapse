//! Interning entity/tile prototype names and choosing a color for each one.

use std::collections::HashMap;

use macroquad::color::Color;

/// Dense index into [`TypeRegistry`]. A real base has tens of distinct
/// prototype names against hundreds of thousands of entities, so the name is
/// worth storing once and referring to by number everywhere else.
pub type TypeId = u16;

/// Interns entity/tile prototype names, and resolves each one's color once.
///
/// The pre-registry renderer called `color_for` (an FNV hash over the name)
/// and `sprites.get(&e.n)` (a SipHash over the name) for every entity on
/// every rendered frame. Both are pure functions of the name, and there are
/// only ~58 distinct names in the real fixtures, so both collapse into an
/// array index once names are interned.
#[derive(Default)]
pub struct TypeRegistry {
    names: Vec<String>,
    ids: HashMap<String, TypeId>,
    entity_colors: Vec<Color>,
    tile_colors: Vec<Color>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Both color variants are precomputed rather than one per registered
    /// kind: a name is realistically either an entity or a tile type, but
    /// nothing in the format guarantees it, and two `Color`s per *type* is
    /// negligible next to one hash per *entity per frame*.
    pub fn intern(&mut self, name: &str) -> TypeId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = TypeId::try_from(self.names.len()).expect("more than u16::MAX distinct type names");
        self.names.push(name.to_string());
        let color = known_color(name);
        self.entity_colors.push(color.unwrap_or_else(|| color_for(name, 0.55, 0.85)));
        self.tile_colors.push(color.unwrap_or_else(|| color_for(name, 0.35, 0.5)));
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
}

/// A curated color for terrain and terrain-scatter names, approximating how
/// they actually look in Factorio, checked before falling back to
/// `color_for`'s hash. Best-effort and pattern-matched against the real
/// names a live capture actually produced (see the terrain-capture work in
/// mod/control.lua), not an exhaustive prototype list -- Factorio and Space
/// Age have dozens of terrain names across four planets, and hashing is a
/// perfectly good fallback for whatever this doesn't recognize. Without
/// this, ordinary factory infrastructure (concrete, a hazard floor) got the
/// same rainbow-hash treatment as everything else, which is exactly backward
/// for the handful of names a player sees constantly and has strong
/// expectations for the color of.
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

    // Resource deposits: colored to match Factorio's own map-view resource
    // palette (the little colored blobs shown in chart/map mode), since
    // that's the mental model a player already has for "what color is
    // iron" from staring at the map screen, not the ore chunk's in-world
    // sprite. Only the vanilla set is curated -- the handful of Space Age
    // additions in `is_resource` (tungsten ore, calcite, scrap) don't have
    // an established look here yet, so they fall through to the hash
    // palette below like any other unrecognized name.
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

    // Placed infrastructure common enough to have a strong expected color.
    if name.starts_with("refined-hazard-concrete") || name.starts_with("hazard-concrete") {
        return Some(rgb(196, 160, 40));
    }
    if name == "concrete" || name == "refined-concrete" {
        // Dark enough that entities sitting on it (colored via the bright
        // hash palette below) stay readable against it -- a mid grey was
        // too close in brightness to blend into rather than contrast with.
        return Some(rgb(58, 58, 60));
    }
    if name == "stone-path" {
        return Some(rgb(146, 126, 104));
    }
    if name == "landfill" {
        return Some(rgb(107, 84, 60));
    }

    // Terrain scatter: cliffs read as bare rock; live trees green, with
    // "dead"/"dry" variants (including desert trees) a dead-wood brown
    // rather than green, since that's the biggest visual distinction
    // between tree variants, not the specific species.
    if is_terrain_scatter(name) {
        if name == "cliff" {
            return Some(rgb(96, 92, 88));
        }
        if name.starts_with("dead-") || name.starts_with("dry-") {
            return Some(rgb(107, 92, 66));
        }
        return Some(rgb(53, 89, 42));
    }

    None
}

/// Whether `name` is a tree or cliff prototype -- decorative terrain scatter
/// captured only when the terrain-capture setting is on (see
/// `mod/control.lua`'s `excluded_types`), naturally scattered across a wide
/// area independent of anything the player built. Shared with
/// `construction.rs`'s auto-follow bounds, which needs to exclude exactly
/// this set for the same reason it excludes tiles: it says nothing about how
/// the factory grew, and counting it would track how much of the map has
/// been revealed instead of where the buildings are.
pub fn is_terrain_scatter(name: &str) -> bool {
    name == "cliff" || name.starts_with("dead-") || name.starts_with("dry-") || name.starts_with("tree")
}

/// Whether `name` is a resource deposit -- ore, oil, and the like, captured
/// only when the include-resources setting is on. Like terrain scatter,
/// shared with `construction.rs`'s auto-follow bounds: a resource sits
/// wherever the map generated it, entirely independent of anything the
/// player built, so counting it can pull the tracked area out toward a
/// distant oil field or ore patch instead of hugging the actual buildings.
///
/// Named exactly, not pattern-matched: unlike tree/cliff names, resource
/// names share no common prefix, so this is the vanilla set plus what's
/// confirmed from Space Age (`tungsten-ore`, `calcite` on Vulcanus,
/// `scrap` on Fulgora) -- best-effort the same way `is_terrain_scatter` is;
/// a modded or as-yet-unseen resource name just won't be caught.
pub fn is_resource(name: &str) -> bool {
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
    )
}

/// Deterministic name -> color, so a given entity type is always the same
/// color across runs with nothing to curate as new Factorio types show up.
pub fn color_for(name: &str, saturation: f32, value: f32) -> Color {
    let mut hash: u32 = 2166136261;
    for b in name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    let hue = (hash % 360) as f32 / 360.0;
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

    /// Every one of these is a real name a live capture actually produced
    /// (see the terrain-capture work in mod/control.lua); pinning that
    /// they resolve to a curated color, not the hash fallback, is what
    /// guards against a substring check silently stopping matching one of
    /// them after some future edit.
    #[test]
    fn known_color_recognizes_real_captured_terrain_names() {
        for name in [
            "water", "deepwater", "grass-1", "grass-4", "dirt-3", "dry-dirt", "sand-2", "red-desert-1",
            "out-of-map", "concrete", "refined-concrete", "hazard-concrete-left",
            "refined-hazard-concrete-right", "stone-path", "landfill", "cliff", "tree-01", "tree-09-brown",
            "dead-tree-desert", "dry-hairy-tree",
        ] {
            assert!(known_color(name).is_some(), "{name} should have a curated color");
        }
    }

    /// Distinct colors, not just "present": pinning that iron and copper in
    /// particular don't end up looking alike guards against the kind of
    /// mistake that's easy to make copy-pasting six similar `if` blocks.
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
    fn known_color_falls_back_to_none_for_ordinary_factory_entities() {
        for name in ["transport-belt", "assembling-machine-1", "electric-furnace"] {
            assert!(known_color(name).is_none(), "{name} should use the hash fallback, not a curated color");
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

    /// The registry has to agree with the function it replaces, or interning
    /// would silently recolor every entity in the viewer.
    #[test]
    fn registry_colors_match_the_underlying_hash() {
        let mut registry = TypeRegistry::new();
        let id = registry.intern("assembling-machine-1");
        let entity = color_for("assembling-machine-1", 0.55, 0.85);
        let tile = color_for("assembling-machine-1", 0.35, 0.5);
        assert_eq!(
            (registry.entity_color(id).r, registry.entity_color(id).g, registry.entity_color(id).b),
            (entity.r, entity.g, entity.b)
        );
        assert_eq!(
            (registry.tile_color(id).r, registry.tile_color(id).g, registry.tile_color(id).b),
            (tile.r, tile.g, tile.b)
        );
    }
}
