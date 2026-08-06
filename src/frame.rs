//! The exported frame format written by the mod (see mod/control.lua) and
//! consumed by the viewer. Kept in the lib so both can share it.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Frame {
    pub tick: u64,
    pub surface: String,
    pub entities: Vec<Entity>,
    pub count: usize,
    /// Absent in frames captured before tile export existed, hence the
    /// default rather than requiring it.
    #[serde(default)]
    pub tiles: Vec<Tile>,
}

#[derive(Debug, Deserialize)]
pub struct Entity {
    pub n: String,
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub d: u8,
    /// Tile footprint. Absent in frames captured before footprint export
    /// existed, and omitted by the encoder for the common 1x1 case, so both
    /// default to 1 rather than 0 (plain `#[serde(default)]` would give 0,
    /// which would draw as invisible instead of the single tile it always was).
    #[serde(default = "one")]
    pub w: u32,
    #[serde(default = "one")]
    pub h: u32,
}

fn one() -> u32 {
    1
}

/// Unlike entities, tiles are corner positioned and integer aligned: a tile
/// named at (x,y) occupies world space [x,x+1) x [y,y+1).
#[derive(Debug, Deserialize)]
pub struct Tile {
    pub n: String,
    pub x: i32,
    pub y: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_fixture(name: &str) -> Frame {
        let path = format!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames/{}"), name);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {path}: {e}"))
    }

    /// Known counts from tests/fixtures/README.md -- real captured frames,
    /// so this also guards against the format silently drifting.
    #[test]
    fn real_fixtures_parse_with_known_entity_counts() {
        let expected = [
            ("frame_0000.json", 240),
            ("frame_0001.json", 589),
            ("frame_0002.json", 3043),
            ("frame_0003.json", 10234),
            ("frame_0004.json", 22971),
        ];
        for (name, count) in expected {
            let frame = load_fixture(name);
            assert_eq!(frame.count, count, "{name}: count field");
            assert_eq!(frame.entities.len(), count, "{name}: entities.len()");
        }
    }

    /// These fixtures predate tile export, so `tiles` must default to
    /// empty rather than the missing key being a parse error.
    #[test]
    fn real_fixtures_predate_tile_export_and_default_to_no_tiles() {
        let frame = load_fixture("frame_0004.json");
        assert!(frame.tiles.is_empty());
    }

    #[test]
    fn tiles_array_parses_including_negative_coordinates() {
        let json = r#"{"tick":1,"surface":"nauvis","entities":[],"count":0,
            "tiles":[{"n":"concrete","x":-5,"y":-12}],"tile_count":1}"#;
        let frame: Frame = serde_json::from_str(json).unwrap();
        assert_eq!(frame.tiles.len(), 1);
        assert_eq!((frame.tiles[0].n.as_str(), frame.tiles[0].x, frame.tiles[0].y), ("concrete", -5, -12));
    }

    #[test]
    fn entity_direction_defaults_to_zero_when_absent_and_decodes_when_present() {
        let without: Entity = serde_json::from_str(r#"{"n":"pipe","x":1.0,"y":2.0}"#).unwrap();
        assert_eq!(without.d, 0);

        let with: Entity = serde_json::from_str(r#"{"n":"pipe","x":1.0,"y":2.0,"d":4}"#).unwrap();
        assert_eq!(with.d, 4);
    }

    /// w/h default to 1, not 0 (0 would draw as invisible instead of the
    /// single tile every entity always occupied before footprint export).
    #[test]
    fn entity_footprint_defaults_to_one_by_one_when_absent_and_decodes_when_present() {
        let without: Entity = serde_json::from_str(r#"{"n":"inserter","x":1.0,"y":2.0}"#).unwrap();
        assert_eq!((without.w, without.h), (1, 1));

        let with: Entity =
            serde_json::from_str(r#"{"n":"assembling-machine-1","x":1.0,"y":2.0,"w":3,"h":3}"#).unwrap();
        assert_eq!((with.w, with.h), (3, 3));
    }
}
