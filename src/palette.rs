//! Prototype colours, as the game itself reports them.
//!
//! Factorio paints its own map view from each prototype's `map_color`, and
//! that value only exists inside the running game: a mod ships as a zip in the
//! mods folder, not as anything this tool can read. So the mod writes them out
//! once beside a capture (see `encode.palette_json`) and this reads them back.
//!
//! The point is to stop naming things. Before this, supporting a mod's terrain
//! meant hand-transcribing its colours into a table here, once per mod,
//! forever, and Alien Biomes alone adds a couple of hundred tiles. With it, any
//! mod's colours arrive automatically and match what the player already sees on
//! their map screen.
//!
//! Absent is normal, not an error: every capture recorded before this existed
//! has no palette, and the viewer keeps its own built-in colours for those.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// One prototype's colour, as bytes. The mod rounds Factorio's 0..1 floats
/// before writing, so there is nothing to interpret here.
pub type Rgb = [u8; 3];

/// Tiles and entities are kept apart because a name can legitimately be both,
/// and because they are asked for in different places: `tile_color` for the
/// floor, `entity_color` for what stands on it.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Palette {
    #[serde(default)]
    pub tiles: HashMap<String, Rgb>,
    #[serde(default)]
    pub entities: HashMap<String, Rgb>,
}

impl Palette {
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty() && self.entities.is_empty()
    }
}

/// Reads `palette.json` from a built timelapse, or `None` if it has none.
///
/// Every failure folds into `None` rather than surfacing: a missing palette is
/// the normal state of any capture older than this feature, and a malformed one
/// is a cosmetic problem that must not stop somebody opening their timelapse.
/// The built-in colours are a complete fallback either way.
pub fn read(dir: &Path) -> Option<Palette> {
    let text = std::fs::read_to_string(dir.join("palette.json")).ok()?;
    let palette: Palette = serde_json::from_str(&text).ok()?;
    (!palette.is_empty()).then_some(palette)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tiles_and_entities() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("palette.json"),
            r#"{"tiles":{"grass-1":[76,104,46]},"entities":{"transport-belt":[204,161,71]}}"#,
        )
        .unwrap();

        let palette = read(dir.path()).expect("a palette");
        assert_eq!(palette.tiles["grass-1"], [76, 104, 46]);
        assert_eq!(palette.entities["transport-belt"], [204, 161, 71]);
    }

    /// The three ways there is nothing usable, all of which are ordinary
    /// rather than exceptional: no file at all, unreadable content, and a
    /// well-formed file describing nothing.
    #[test]
    fn anything_unusable_reads_as_no_palette() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_none(), "no file");

        std::fs::write(dir.path().join("palette.json"), "not json at all").unwrap();
        assert!(read(dir.path()).is_none(), "malformed");

        std::fs::write(dir.path().join("palette.json"), r#"{"tiles":{},"entities":{}}"#).unwrap();
        assert!(read(dir.path()).is_none(), "empty");
    }

    /// A palette written by a newer mod than this build may carry sections
    /// this does not know about, which must not stop the ones it does.
    #[test]
    fn unknown_sections_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("palette.json"),
            r#"{"tiles":{"lava":[150,49,30]},"entities":{},"fluids":{"water":[0,0,255]}}"#,
        )
        .unwrap();

        let palette = read(dir.path()).expect("a palette");
        assert_eq!(palette.tiles["lava"], [150, 49, 30]);
    }
}
