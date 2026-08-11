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
        // Entities fall back to a shade of the map view's friendly blue;
        // tiles keep the full-hue hash. A name is one or the other in
        // practice, and an unrecognised *floor* has no reason to be blue: the
        // blue is what the game uses for structures specifically.
        self.entity_colors.push(color.unwrap_or_else(|| friendly_shade(name)));
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
/// mod/control.lua), not an exhaustive prototype list. Factorio and Space
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

    // Player-built structures, following Factorio's own map view.
    //
    // The game paints the chart from each prototype's chart colour (see
    // `core/prototypes/utility-constants.lua`): rails grey, the belt family
    // yellow, a handful of types with their own colour, and everything else
    // one friendly blue. That palette is already in a player's head from
    // looking at the map screen, so matching it makes a timelapse read the
    // way the game does instead of like a hash function's opinion.
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

    // Space Age planets.
    //
    // Taken from each tile prototype's own `map_color`, the value Factorio
    // draws that tile with in map view, rather than picked by eye. That is
    // the palette a player already has in their head from looking at the
    // in-game map, and it means a timelapse of Vulcanus looks like Vulcanus
    // rather than like a hash function's opinion of one. Read out of
    // `data/space-age/prototypes/tile/tiles-<planet>.lua`.
    //
    // Grouped by prefix rather than listed tile by tile, matching how the
    // Nauvis terrain above is handled: there are around 75 of these across
    // the three planets and most of a family shares one colour anyway, so
    // naming each individually would be a table to maintain for no visible
    // gain.

    // Vulcanus: near-black rock and ash, with lava the only bright thing on
    // the planet. The contrast is the point, and it is what makes a lava flow
    // read at a glance against a base built on the rock beside it.
    if name == "lava-hot" {
        return Some(rgb(255, 138, 57));
    }
    if name.starts_with("lava") {
        return Some(rgb(150, 49, 30));
    }
    if name.starts_with("volcanic") {
        // The "hot" and "warm" variants are the ground immediately around
        // lava and are tinted towards it in game, so they keep that warmth
        // rather than flattening into the general grey.
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

    // Aquilo: pale snow at the top of the range, mid blue ice below it, and
    // near-black ammoniacal ocean at the bottom. The whole planet is one cold
    // ramp, so what matters is keeping those three bands apart.
    //
    // The ocean values here are the *uncommented* ones. The file also carries
    // an earlier `{5, 15, 25}` commented out directly above the live
    // `{15, 13, 25}`, which is an easy thing to read off by mistake.
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
    // through: flat is fresh cover, patchy is nearly bare. The game builds
    // these by interpolating between two colours, so these are the computed
    // ends and midpoints rather than literals in the file.
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

    // Resource deposits: colored to match Factorio's own map-view resource
    // palette (the little colored blobs shown in chart/map mode), since
    // that's the mental model a player already has for "what color is
    // iron" from staring at the map screen, not the ore chunk's in-world
    // sprite.
    //
    // Space Age's are here too now, taken from their prototypes' own
    // `map_color` rather than picked by eye. Leaving them out was survivable
    // while unrecognised names got a random hue, but once structures became
    // blue it meant Vulcanus's calcite and Fulgora's scrap rendered as
    // buildings, which is exactly the opposite of what a resource patch is.
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
        // Dark enough that entities sitting on it (colored via the bright
        // hash palette below) stay readable against it. A mid grey was
        // too close in brightness to blend into rather than contrast with.
        return Some(rgb(58, 58, 60));
    }
    if name == "stone-path" {
        return Some(rgb(146, 126, 104));
    }
    if name == "landfill" {
        return Some(rgb(107, 84, 60));
    }

    // Enemies, red so clearing a nest reads at a glance against a base
    // that is otherwise a rainbow of hashed hues. Worms are the lighter
    // shade purely so a nest and the worms around it stay distinguishable
    // when they sit in one cluster, which is how they usually generate.
    if is_enemy(name) {
        // Checked before the nest shade below, so a name that is somehow
        // both lands on one deterministically rather than by branch order
        // being read the other way round later.
        if name.ends_with("-worm-turret") {
            return Some(rgb(220, 74, 60));
        }
        return Some(rgb(168, 34, 30));
    }

    // Terrain scatter: cliffs read as bare rock; live trees green, with
    // "dead"/"dry" variants (including desert trees) a dead-wood brown
    // rather than green, since that's the biggest visual distinction
    // between tree variants, not the specific species.
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
        // Vulcanus lichen is ash-grey scrub on black rock, not forest. Nauvis
        // green here would dot a volcanic planet with something that reads as
        // a different world's vegetation.
        if name.starts_with("ashland-lichen") {
            return Some(rgb(74, 78, 62));
        }
        return Some(rgb(53, 89, 42));
    }

    None
}

/// Whether `name` is an enemy structure: a nest or a worm turret.
///
/// Matched by name because that is all the wire format carries. The mod
/// knows each entity's prototype *type* while capturing (`unit-spawner`,
/// `turret`) but never writes it, since a type is worth nothing to the
/// renderer for the thousands of ordinary entities that make up a base, and
/// this is the one place it would have helped.
///
/// Substring matching rather than an exact list of the vanilla worms and
/// nests: modded enemies conventionally follow the same `spawner` naming, and
/// getting a modded nest colored as an enemy for free is worth more than the
/// precision of a list that would silently miss it. Substring rather than
/// suffix specifically because Space Age ships `gleba-spawner-small`, so even
/// within vanilla the word is not always last.
///
/// The exception is why this needs to be a function at all.
/// `captive-biter-spawner` contains `spawner` but is a Space Age
/// *assembling-machine*, a factory building the player crafts and places to
/// make biter eggs, so painting it enemy red would mislabel part of the
/// player's own base as something to be cleared. Checked against the real
/// prototype in `space-age/prototypes/entity/entities.lua`.
///
/// The other names carrying `spawner` (`biter-spawner-corpse`,
/// `captive-spawner-explosion-1`, `guts-entrails-particle-spawner`) need no
/// exception: every one is a corpse, explosion, or particle source, all of
/// which `EXCLUDED_TYPES` already keeps out of a capture entirely, so they
/// never reach a color lookup.
///
/// Individual biters and spitters are deliberately absent, and not because
/// of color: they are excluded from capture entirely (`mod/encode.lua`'s
/// `EXCLUDED_TYPES`), since the event log records construction and
/// destruction but never movement, so a captured biter would sit frozen
/// wherever it was first logged while the real one walked away.
///
/// Space Age's own mobile enemies, Gleba's pentapods and Vulcanus's
/// demolishers, *should* be absent for that same reason and are not in any
/// capture recorded so far: `EXCLUDED_TYPES` kept out Factorio's `unit` type,
/// and Space Age gave its new enemies types of their own (`spider-unit`,
/// `spider-leg`, `segmented-unit`, `segment`), so every one of them landed in
/// captures as though somebody had built it. Found by reading a real Gleba
/// capture, which holds `small-stomper-pentapod`, `small-strafer-pentapod`
/// and both of their `-leg` prototypes.
///
/// `mod/encode.lua` excludes those types now, which fixes it at the source
/// and helps no capture already on disk. Naming them here is what those
/// captures get: it stops them counting as construction, which matters
/// because they roam. On that same capture they held the auto-follow box
/// open across the whole explored map, so the factory rendered as a smudge
/// in the middle of untouched jungle.
///
/// Matching is by substring, which is safe here in the way
/// `is_terrain_scatter`'s exact lists are not: no player-craftable entity
/// carries either word. Pentapod eggs are an *item*, and items are not
/// entities. `demolisher` covers the head and its `-segment` bodies at all
/// three sizes together, the same way `pentapod` covers the size prefixes
/// and `-leg` suffixes.
pub fn is_enemy(name: &str) -> bool {
    if name == "captive-biter-spawner" {
        return false;
    }
    name.contains("spawner") || name.ends_with("-worm-turret") || name.contains("pentapod") || name.contains("demolisher")
}

/// Whether `name` is something that drives, walks or rolls: a car, a tank, a
/// Spidertron, or a train.
///
/// Excluded from capture now (`mod/encode.lua`'s `EXCLUDED_TYPES`) under the
/// same rule as robots and biters, and named here for the captures written
/// before that, where they are already recorded. Not folded into `is_enemy`
/// despite both feeding the same two callers: a locomotive is not an enemy,
/// and colouring one enemy red on an existing capture would be a worse lie
/// than leaving it the shade it already has.
///
/// Trains are the ones that actually matter. A from-saves export catches
/// them somewhere different in every save, so they blink around the rail
/// network frame by frame, and every position they were ever caught in would
/// otherwise pull the camera. Rails, signals and stations are not here, being
/// stationary infrastructure rather than the thing moving over it.
pub fn is_vehicle(name: &str) -> bool {
    matches!(name, "car" | "tank" | "spidertron" | "locomotive" | "cargo-wagon" | "fluid-wagon" | "artillery-wagon")
        || name.starts_with("spidertron-leg-")
}

/// Whether `name` is a tree or cliff prototype: decorative terrain scatter
/// captured only when the terrain-capture setting is on (see
/// `mod/control.lua`'s `excluded_types`), naturally scattered across a wide
/// area independent of anything the player built. Shared with
/// `construction.rs`'s auto-follow bounds, which needs to exclude exactly
/// this set for the same reason it excludes tiles: it says nothing about how
/// the factory grew, and counting it would track how much of the map has
/// been revealed instead of where the buildings are.
pub fn is_terrain_scatter(name: &str) -> bool {
    // `cliff-` rather than just `cliff`: every planet has its own cliff
    // prototype (`cliff-vulcanus`, `cliff-fulgora`, `cliff-gleba`), and
    // matching the bare name alone left all three rendering as structures.
    //
    // A prefix here rather than the exact list used for rails and flora,
    // because unlike those there is nothing a `cliff-` entity could be except
    // a cliff, and a new planet should not need this file edited to stop its
    // cliffs looking like buildings.
    name == "cliff"
        || name.starts_with("cliff-")
        || name.starts_with("dead-")
        || name.starts_with("dry-")
        || name.starts_with("tree")
        || OFF_WORLD_FLORA.contains(&name)
}

/// Every flora prototype outside Nauvis, named exactly.
///
/// Nauvis names happen to share `tree`, `dead-` and `dry-` prefixes, so it
/// got away with prefix matching. Nothing on the other planets follows that
/// convention: Gleba's are called `jellystem`, `boompuff`, `stingfrond` and
/// so on, and Vulcanus has `ashland-lichen-tree`. Without them listed, a
/// Gleba forest counted as construction and dragged the auto-follow camera
/// out over untouched wilderness, and every one of them rendered as a
/// structure rather than as scenery.
///
/// Exact names rather than a pattern, for the reason `RAIL_TRACK` is:
/// `rocket-turret` contains "rock", and `cryogenic-plant` and
/// `electromagnetic-plant` are crafting machines despite the name. Taken from
/// the prototypes of type `tree` and `plant` in the game's own data.
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

/// Whether `name` is a resource deposit: ore, oil, and the like, captured
/// only when the include-resources setting is on. Like terrain scatter,
/// shared with `construction.rs`'s auto-follow bounds: a resource sits
/// wherever the map generated it, entirely independent of anything the
/// player built, so counting it can pull the tracked area out toward a
/// distant oil field or ore patch instead of hugging the actual buildings.
///
/// Named exactly, not pattern-matched: unlike tree/cliff names, resource
/// names share no common prefix. This is the vanilla set plus every Space
/// Age one, taken from the prototypes of type `resource` in the game's data
/// rather than from whichever ones happened to be noticed. A modded resource
/// still will not be caught, which costs a wandering camera rather than
/// anything incorrect.
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
            // The three that were missing entirely, so they counted as
            // construction and dragged the auto-follow camera toward them.
            | "sulfuric-acid-geyser"
            | "fluorine-vent"
            | "lithium-brine"
    )
}

/// Entity types confirmed safe to rotate: a flat, top-down icon (visibly
/// directional, like a belt's chevrons) rather than a stylized oblique-angle
/// render (a fixed 3D camera perspective, like a chest, lab, or drill's).
/// Rotating a flat icon conveys the entity's real facing; rotating an
/// oblique one just spins that fixed camera angle around and looks wrong
/// regardless of the angle used, confirmed by directly inspecting the icon
/// files for both kinds.
///
/// An allowlist rather than a denylist deliberately: most Factorio entity
/// icons are the oblique, don't-rotate kind (confirmed against everything
/// checked so far), so a denylist would need to name nearly everything,
/// while this only needs to grow one confirmed-good entry at a time. Add to
/// it once an entity's rotated icon is checked and looks right; anything not
/// listed renders unrotated by default, same as before this feature existed.
const ALWAYS_ROTATE: &[&str] = BELTS;

pub fn is_rotation_allowed(name: &str) -> bool {
    ALWAYS_ROTATE.contains(&name)
}

/// The four transport belt tiers, which are the entities drawn from Factorio's
/// own in-world sheet rather than from a rotated inventory icon.
///
/// Only these four. Underground belts and splitters move items too, but they
/// have no curved form to get wrong: an underground entrance is one fixed
/// picture and a splitter is drawn flat, so both are served fine by their icon.
const BELTS: &[&str] = &["transport-belt", "fast-transport-belt", "express-transport-belt", "turbo-transport-belt"];

pub fn is_belt(name: &str) -> bool {
    BELTS.contains(&name)
}

/// Underground belt tiers and how far each one reaches, in tiles, taken from
/// `max_distance` in the game's own prototypes.
///
/// The reach is what pairs an entrance with its exit. Two underground belts
/// facing the same way on the same line belong together only if they are close
/// enough to actually connect, and a factory routinely has several separate
/// crossings in a row along one line.
const UNDERGROUNDS: &[(&str, i32)] =
    &[("underground-belt", 5), ("fast-underground-belt", 7), ("express-underground-belt", 9), ("turbo-underground-belt", 11)];

pub fn underground_reach(name: &str) -> Option<i32> {
    UNDERGROUNDS.iter().find(|(tier, _)| *tier == name).map(|(_, reach)| *reach)
}

/// Splitter tiers, which draw from four separate per-facing files rather than
/// from one sheet.
const SPLITTERS: &[&str] = &["splitter", "fast-splitter", "express-splitter", "turbo-splitter"];

/// Plain pipes, whose whole appearance comes from which sides join onto them.
/// `pipe-to-ground` is deliberately absent: it has its own fixed pictures and
/// does not change shape with its neighbours.
pub fn is_pipe(name: &str) -> bool {
    name == "pipe"
}

/// Underground pipes, which draw one of four fixed pictures chosen by facing.
/// Unlike an underground belt, the two ends of a run carry different
/// directions of their own, so nothing has to be paired up to tell them apart.
pub fn is_pipe_to_ground(name: &str) -> bool {
    name == "pipe-to-ground"
}

pub fn is_splitter(name: &str) -> bool {
    SPLITTERS.contains(&name)
}

/// Every prototype that is rail track, and therefore should look like rail
/// track rather than like twelve unrelated things.
///
/// Factorio 2.0 split what used to be two rail prototypes into a family:
/// straight, two curve halves, half-diagonal, the elevated version of each,
/// ramps and supports, plus `legacy-` variants kept for saves made before the
/// split. Hashing each name separately gave every one its own hue, so a
/// single rail line rendered as a rainbow and a pre-2.0 save's track was a
/// different colour again from track laid after it.
///
/// Named explicitly rather than matched on "rail" as a substring, which would
/// wrongly swallow `rail-signal`, `rail-chain-signal`, `gate-over-rail` and
/// `railgun-turret`. Those are genuinely different things and keep their own
/// colours. Taken from the game's own `entity-name` locale entries rather
/// than from memory.
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

/// The name a prototype is coloured by, which is its own unless it belongs to
/// a family the eye reads as one thing.
///
/// Deliberately the only grouping so far. A curated list of "these look alike"
/// is exactly the maintenance burden `color_for` was designed to avoid, so
/// this earns its place only where the alternative is visibly wrong, and rail
/// is that case: track is a continuous line, and a line changing colour along
/// its length reads as a different structure rather than as the same one.
fn color_group(name: &str) -> &str {
    if RAIL_TRACK.contains(&name) {
        return "rail";
    }
    // Aquilo freezes placed floor into a `frozen-` twin of the same tile,
    // deep-copied from it in the game's own data. It is the same path the
    // player laid, so it must not change colour because the weather did, and
    // a path crossing from a warm surface to a cold one must not appear to
    // change material halfway.
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

/// A shade of Factorio's map-view friendly blue, for any structure without a
/// colour of its own.
///
/// The game paints all of these with exactly one blue
/// (`default_friendly_color`), which is why a Factorio map reads as a blue
/// city. Copying that literally would make a furnace, an assembler and a
/// chest indistinguishable here, and telling machines apart is most of what
/// this renderer is for.
///
/// So the hash picks a shade *within* the blue rather than anywhere on the
/// wheel: the hue stays in a narrow band around the game's own 200 degrees,
/// and lightness and saturation carry the rest of the variation. Everything
/// still reads as blue at a glance, while two neighbouring machine types stay
/// visibly different up close, which is the compromise the plain hash got
/// backwards and a single flat blue would get backwards the other way.
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
        /// A registry holding exactly one name, so a test can ask what colour
        /// that name actually resolves to through the real path rather than
        /// reimplementing the fallback chain.
        fn new_with(name: &str) -> TypeRegistry {
            let mut registry = TypeRegistry::new();
            registry.intern(name);
            registry
        }
    }

    /// A rail line is one continuous structure and has to read as one. Before
    /// this, 2.0's split into straight, curved, half-diagonal and elevated
    /// variants gave each its own hue, and a save from before the split
    /// coloured differently again from track laid after it.
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

    /// Every Space Age tile a capture can contain resolves to a curated
    /// colour rather than falling through to the hash.
    ///
    /// The names are the real prototype names, read out of the game's own
    /// tile definitions. A tile falling through here would not look broken,
    /// it would just look like an arbitrary hue in the middle of a planet
    /// whose palette is otherwise deliberate, which is exactly the kind of
    /// thing nobody notices until they see a screenshot of it.
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

    /// Aquilo is one cold ramp from pale snow down to near-black ocean, and
    /// the three bands staying apart is the only thing that makes its terrain
    /// readable at a glance.
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

    /// Lava has to stay obviously brighter than the rock it sits in, which is
    /// the whole reason Vulcanus reads at a glance. Asserted rather than
    /// eyeballed, because a later tweak to the greys could quietly close the
    /// gap.
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

    /// Flora on the other planets is scenery, exactly like a Nauvis forest.
    ///
    /// This matters well beyond colour. `construction.rs` and `activity.rs`
    /// both skip terrain scatter, so while these went unrecognised a Gleba
    /// forest counted as construction and dragged the auto-follow camera out
    /// over untouched wilderness.
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

    /// Every resource is a deposit, on every planet.
    ///
    /// Two separate failures were possible here and both bit. A resource
    /// missing from `is_resource` counts as construction and drags the
    /// auto-follow camera toward it; one missing from `known_color` renders
    /// as a structure, which since structures went blue meant Vulcanus's
    /// calcite and Fulgora's scrap looked like buildings.
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

    /// Every one of these is a real name a live capture actually produced
    /// (see the terrain-capture work in mod/control.lua); pinning that
    /// they resolve to a curated color, not the hash fallback, is what
    /// guards against a substring check silently stopping matching one of
    /// them after some future edit.
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

    /// A machine with no colour of its own in the game's chart palette has
    /// none here either, and picks up a shade of the friendly blue instead.
    ///
    /// Belts and rails deliberately no longer qualify: the game gives those
    /// their own chart colours, so this build does too.
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

    /// Every one of these was found in a real Gleba capture, having slipped
    /// past `EXCLUDED_TYPES`'s `unit` filter because Space Age gave its new
    /// enemies their own prototype types. They roam, so counting them as
    /// construction held the auto-follow box open across the whole explored
    /// map.
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

    /// Vulcanus's demolishers, the other half of the same miss: a
    /// `segmented-unit` head trailing `segment` bodies, neither of them the
    /// `unit` type that was being filtered.
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

    /// The infrastructure a train runs on is stationary and stays. Getting
    /// this wrong would erase the rail network, which is one of the things
    /// most worth watching a base grow.
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

    /// The trap this exists for: a Space Age assembling-machine the player
    /// crafts and places, which matches the `-spawner` suffix but is part of
    /// their own base, not something to clear.
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

    /// The registry has to agree with the functions it caches, or interning
    /// would silently recolor every entity in the viewer.
    ///
    /// The two lists deliberately disagree with each other for an
    /// uncurated name: as an entity it is a structure and gets a blue shade,
    /// as a tile it is floor and keeps the full-hue hash. Asserting both is
    /// what stops one of them quietly being wired to the other.
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
}
