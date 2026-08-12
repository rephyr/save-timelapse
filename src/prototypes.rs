//! What this game's prototypes are, as the game itself reports them: each
//! one's colour, each entity's type, how far an underground belt reaches, and
//! which tiles count as placed floor. None of it exists outside the running
//! game, since a mod ships as a zip, so the mod writes it beside a capture
//! (see `encode.prototypes_json`) and this reads it back.
//!
//! Absent is normal: every capture older than the file has none, and the
//! viewer falls back to its own built-in names.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One prototype's colour, as bytes. The mod reduces Factorio's colours to
/// these before writing, so there is nothing to interpret here.
pub type Rgb = [u8; 3];

/// Tile and entity colours are kept apart because a name can be both, and
/// they are asked for in different places.
#[derive(Debug, Default, Clone)]
pub struct Prototypes {
    pub tiles: HashMap<String, Rgb>,
    pub entities: HashMap<String, Rgb>,
    /// Each entity prototype's own type, verbatim: `transport-belt`, `pipe`,
    /// `resource`, and so on.
    pub types: HashMap<String, String>,
    /// How far an underground belt or pipe reaches, in tiles. Only the two
    /// types that have one appear here.
    pub reach: HashMap<String, i32>,
    /// Which tiles this capture treated as placed floor rather than generated
    /// ground, the split `world.rs` has to reproduce. Empty falls back there.
    pub floor: HashSet<String>,
}

impl Prototypes {
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty() && self.entities.is_empty() && self.types.is_empty() && self.floor.is_empty()
    }

    pub fn kind(&self, name: &str) -> Option<&str> {
        self.types.get(name).map(String::as_str)
    }

    /// Every deposit this game has, for `World::set_resources`. Empty when the
    /// capture described no types, which falls back to the built-in list.
    pub fn resource_names(&self) -> HashSet<String> {
        self.types.iter().filter(|(_, kind)| *kind == "resource").map(|(name, _)| name.clone()).collect()
    }

    /// Whether somebody placed this rather than the map generating it. An
    /// undescribed name counts as built, which is what a capture with no
    /// types at all reports for everything.
    pub fn is_built(&self, name: &str) -> bool {
        !self.kind(name).is_some_and(|kind| NOT_BUILT.contains(&kind))
    }
}

/// Prototype types nobody placed. `turret` is the worms': the player's three
/// defences are `ammo-turret`, `electric-turret` and `fluid-turret`, which is
/// what makes excluding the bare type safe.
const NOT_BUILT: [&str; 8] = ["resource", "tree", "plant", "cliff", "fish", "simple-entity", "unit-spawner", "turret"];

/// Reads `prototypes.json` from a built timelapse, or `None` if it has none.
/// Every failure folds into `None`: a missing file is the normal state of an
/// older capture, and a malformed one must not stop somebody opening their
/// timelapse.
pub fn read(dir: &Path) -> Option<Prototypes> {
    let text = std::fs::read_to_string(dir.join("prototypes.json")).ok()?;
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    let prototypes = Prototypes {
        tiles: section(&root, "tiles", rgb),
        entities: section(&root, "entities", rgb),
        types: section(&root, "types", |value| Some(value.as_str()?.to_string())),
        reach: section(&root, "reach", |value| i32::try_from(value.as_i64()?).ok()),
        floor: names(&root, "floor"),
    };
    (!prototypes.is_empty()).then_some(prototypes)
}

/// One `{name: value}` section, keeping the entries `parse` understands.
///
/// Entry by entry rather than all or nothing: deserializing straight into the
/// struct meant one colour written out of range took the other three hundred
/// with it. A missing section is the same at a larger scale, which is what
/// lets this read a file written by a mod older than the section.
fn section<T>(root: &serde_json::Value, key: &str, parse: fn(&serde_json::Value) -> Option<T>) -> HashMap<String, T> {
    let Some(map) = root.get(key).and_then(|v| v.as_object()) else {
        return HashMap::new();
    };
    map.iter().filter_map(|(name, value)| Some((name.clone(), parse(value)?))).collect()
}

/// One `[name, name, ...]` section. Unlike `section` above this says nothing
/// per name, only which names are in the set, so it reads as a list.
fn names(root: &serde_json::Value, key: &str) -> HashSet<String> {
    let Some(list) = root.get(key).and_then(|v| v.as_array()) else {
        return HashSet::new();
    };
    list.iter().filter_map(|v| Some(v.as_str()?.to_string())).collect()
}

fn rgb(value: &serde_json::Value) -> Option<Rgb> {
    let [r, g, b] = value.as_array()?.as_slice() else {
        return None;
    };
    Some([byte(r)?, byte(g)?, byte(b)?])
}

fn byte(value: &serde_json::Value) -> Option<u8> {
    u8::try_from(value.as_u64()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prototypes.json"), body).unwrap();
        dir
    }

    #[test]
    fn reads_colours_types_and_reach() {
        let dir = written(
            r#"{"tiles":{"grass-1":[76,104,46]},"entities":{"transport-belt":[204,161,71]},
                "types":{"kr-advanced-transport-belt":"transport-belt"},
                "reach":{"kr-advanced-underground-belt":30}}"#,
        );

        let read = read(dir.path()).expect("a description");
        assert_eq!(read.tiles["grass-1"], [76, 104, 46]);
        assert_eq!(read.entities["transport-belt"], [204, 161, 71]);
        assert_eq!(read.kind("kr-advanced-transport-belt"), Some("transport-belt"));
        assert_eq!(read.reach["kr-advanced-underground-belt"], 30);
        assert_eq!(read.kind("never-mentioned"), None);
    }

    /// What the "N buildings" counts mean by a building. A capture keeps ore,
    /// trees and nests for context, and counting those reports the map rather
    /// than the factory.
    #[test]
    fn only_what_somebody_placed_counts_as_built() {
        let dir = written(
            r#"{"types":{"assembling-machine-2":"assembling-machine","kr-rare-metal-ore":"resource",
                "tree-01":"tree","big-rock":"simple-entity","biter-spawner":"unit-spawner",
                "small-worm-turret":"turret","gun-turret":"ammo-turret","laser-turret":"electric-turret"}}"#,
        );
        let read = read(dir.path()).expect("a description");

        for name in ["assembling-machine-2", "gun-turret", "laser-turret"] {
            assert!(read.is_built(name), "{name} is something somebody placed");
        }
        for name in ["kr-rare-metal-ore", "tree-01", "big-rock", "biter-spawner", "small-worm-turret"] {
            assert!(!read.is_built(name), "{name} is not a building");
        }
        assert!(read.is_built("never-mentioned"), "an undescribed name counts, as it always did");
    }

    /// No file, unreadable content, and a well-formed file describing
    /// nothing, all of which are ordinary.
    #[test]
    fn anything_unusable_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_none(), "no file");

        assert!(read(written("not json at all").path()).is_none(), "malformed");
        assert!(read(written(r#"{"tiles":{},"entities":{},"types":{}}"#).path()).is_none(), "empty");
    }

    /// Mod 0.7.0 scaled colours already in 0..255 by 255 again, and
    /// deserializing into the struct threw away every good colour with the
    /// first bad one. Only the entry itself may be lost.
    #[test]
    fn one_unusable_entry_costs_only_itself() {
        let dir = written(
            r#"{"tiles":{"grass-1":[76,104,46],"mineral-red-dirt-1":[31620,16320,13005],
                "short":[1,2],"wordy":"blue","negative":[-1,0,0]},"entities":{},
                "types":{"pipe":"pipe","numeric":7}}"#,
        );

        let read = read(dir.path()).expect("a description");
        assert_eq!(read.tiles["grass-1"], [76, 104, 46]);
        assert_eq!(read.tiles.len(), 1, "every malformed colour dropped, and nothing else");
        assert_eq!(read.kind("pipe"), Some("pipe"));
        assert_eq!(read.types.len(), 1, "a type that is not a string is dropped the same way");
    }

    /// A newer mod may carry sections this build has never heard of, and an
    /// older one is missing ones it expects. Neither may cost the rest.
    #[test]
    fn unknown_and_absent_sections_are_both_survivable() {
        let dir = written(r#"{"tiles":{"lava":[150,49,30]},"fluids":{"water":[0,0,255]}}"#);

        let read = read(dir.path()).expect("a description");
        assert_eq!(read.tiles["lava"], [150, 49, 30]);
        assert!(read.types.is_empty(), "a file predating types still reads");
        assert!(read.reach.is_empty());
    }
}
