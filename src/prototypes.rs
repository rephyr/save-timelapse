//! What this game's prototypes are, as the game itself reports them.
//!
//! Two things live in one file because they have one source and one lifetime.
//! What colour is it: Factorio paints its own map view from each prototype's
//! `map_color`. And what *is* it: a belt, a pipe, an ore patch, a tree, and how
//! far an underground belt reaches. Neither exists anywhere but inside the
//! running game, since a mod ships as a zip in the mods folder rather than as
//! anything this tool can read, so the mod writes both out beside a capture
//! (see `encode.prototypes_json`) and this reads them back.
//!
//! The point is to stop naming things. Before this, supporting a mod meant
//! transcribing its prototypes into tables here, once per mod, forever: Alien
//! Biomes alone adds a couple of hundred tiles, and Krastorio2 adds belt tiers,
//! ores and pipes that a viewer built around the vanilla names cannot see are
//! belts, ore or pipes at all.
//!
//! Absent is normal, not an error: every capture recorded before this existed
//! has no such file, and the viewer keeps its own built-in names for those.

use std::collections::HashMap;
use std::path::Path;

/// One prototype's colour, as bytes. The mod reduces Factorio's colours to
/// these before writing (see `encode.color_bytes`), so there is nothing to
/// interpret here.
pub type Rgb = [u8; 3];

/// Tile and entity colours are kept apart because a name can legitimately be
/// both, and because they are asked for in different places: `tile_color` for
/// the floor, `entity_color` for what stands on it.
#[derive(Debug, Default, Clone)]
pub struct Prototypes {
    pub tiles: HashMap<String, Rgb>,
    pub entities: HashMap<String, Rgb>,
    /// Each entity prototype's own type, verbatim: `transport-belt`, `pipe`,
    /// `resource`, `tree`, and so on. What the viewer reads to know what a
    /// name it has never seen actually is.
    pub types: HashMap<String, String>,
    /// How far an underground belt or pipe reaches, in tiles. Only the two
    /// types that have one appear here.
    pub reach: HashMap<String, i32>,
}

impl Prototypes {
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty() && self.entities.is_empty() && self.types.is_empty()
    }

    pub fn kind(&self, name: &str) -> Option<&str> {
        self.types.get(name).map(String::as_str)
    }
}

/// Reads `prototypes.json` from a built timelapse, or `None` if it has none.
///
/// Every failure folds into `None` rather than surfacing: a missing file is the
/// normal state of any capture older than this feature, and a malformed one is
/// a cosmetic problem that must not stop somebody opening their timelapse. The
/// built-in names and colours are a complete fallback either way.
pub fn read(dir: &Path) -> Option<Prototypes> {
    let text = std::fs::read_to_string(dir.join("prototypes.json")).ok()?;
    let root: serde_json::Value = serde_json::from_str(&text).ok()?;
    let prototypes = Prototypes {
        tiles: section(&root, "tiles", rgb),
        entities: section(&root, "entities", rgb),
        types: section(&root, "types", |value| Some(value.as_str()?.to_string())),
        reach: section(&root, "reach", |value| i32::try_from(value.as_i64()?).ok()),
    };
    (!prototypes.is_empty()).then_some(prototypes)
}

/// One `{name: value}` section, keeping the entries `parse` understands and
/// dropping the ones it does not.
///
/// Entry by entry rather than all or nothing, which is not defensiveness for
/// its own sake: deserializing straight into the struct meant a single colour
/// the mod had written out of range took the other three hundred and sixty
/// with it, and a whole modded playthrough rendered from the built-in table
/// because of one number. A name that arrives unusable simply has nothing said
/// about it here and falls back on its own, exactly as a name the file never
/// mentioned does. A section that is missing entirely is the same thing at a
/// larger scale, which is what lets this read a file written by a mod older
/// than the section.
fn section<T>(root: &serde_json::Value, key: &str, parse: fn(&serde_json::Value) -> Option<T>) -> HashMap<String, T> {
    let Some(map) = root.get(key).and_then(|v| v.as_object()) else {
        return HashMap::new();
    };
    map.iter().filter_map(|(name, value)| Some((name.clone(), parse(value)?))).collect()
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

    /// The three ways there is nothing usable, all of which are ordinary
    /// rather than exceptional: no file at all, unreadable content, and a
    /// well-formed file describing nothing.
    #[test]
    fn anything_unusable_reads_as_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_none(), "no file");

        assert!(read(written("not json at all").path()).is_none(), "malformed");
        assert!(read(written(r#"{"tiles":{},"entities":{},"types":{}}"#).path()).is_none(), "empty");
    }

    /// The failure this reader was rewritten for: mod 0.7.0 scaled colours
    /// that were already in 0..255 by 255 again, and deserializing into the
    /// struct meant the first of those threw away every good colour in the
    /// file. Only the entry itself may be lost.
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

    /// Sections cut both ways: a file from a newer mod may carry ones this
    /// build has never heard of, and a file from an older mod is missing ones
    /// it expects. Neither may cost the sections that are actually there.
    #[test]
    fn unknown_and_absent_sections_are_both_survivable() {
        let dir = written(r#"{"tiles":{"lava":[150,49,30]},"fluids":{"water":[0,0,255]}}"#);

        let read = read(dir.path()).expect("a description");
        assert_eq!(read.tiles["lava"], [150, 49, 30]);
        assert!(read.types.is_empty(), "a file predating types still reads");
        assert!(read.reach.is_empty());
    }
}
