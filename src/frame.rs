//! The exported frame format written by the mod (see mod/control.lua) and
//! consumed by the viewer. Kept in the lib so both can share it.
//!
//! Wire format (`frame_<tick>_<surface>.stfr`), all integers little endian:
//!
//! ```text
//! magic     4 bytes, "STF1"
//! version   u8, must equal CURRENT_VERSION
//! tick      u64
//! surface   string (u16 length, then that many UTF-8 bytes)
//! entity section, a sequence of:
//!   tag 0  DefineName    string
//!   tag 1  EntityRecord  u16 name_id, i32 x10, i32 y10, u8 d, u8 w, u8 h
//! tag 9  EndEntities (no payload), marking the start of the tile section
//! tile section, a sequence of:
//!   tag 0  DefineName    string
//!   tag 2  TileRecord    u16 name_id, i32 x, i32 y
//! checksum  u32, djb2 (see `checksum` below) of every byte before it,
//!           magic and version included
//! ```
//!
//! The version byte lets a reader tell "this is a format I don't understand"
//! apart from a generic parse failure, which matters here specifically:
//! this project has already changed this format more than once, each time
//! with no way for an older build to say anything clearer than a confusing
//! parse error about a newer file. The checksum catches a narrower,
//! different problem the tag-based structure alone can't: silent bit-level
//! corruption that still happens to decode as plausible-looking records.
//! Both are new as of this version; a file from before it has neither and
//! will not parse, consistent with this project's existing precedent of
//! clean breaks over carrying old formats forward at this alpha stage (see
//! the session-tagging change earlier).
//!
//! There is no entity or tile count anywhere in the file. `find_entities_filtered`
//! and `find_tiles_filtered` both return a full array, so a count would be
//! free to compute for the single tick synchronous export path, but the
//! periodic incremental exporter (`snapshot_step` in control.lua) spreads one
//! export across many ticks specifically so no single tick has to do the
//! whole thing, and real play keeps running in between: an entity a batch
//! has not reached yet can be mined by the player before its turn comes.
//! Scanning the whole list once at the start just to learn a count would
//! reintroduce the stall the incremental exporter exists to avoid, and the
//! count could still be wrong by the time writing finishes. `EndEntities`
//! sidesteps needing a count at all: each section is a plain forward stream,
//! and the tile section simply runs until the file ends.
//!
//! Entity coordinates are stored as position times ten, rounded to the
//! nearest integer: the same fixed point scale `world.rs::pos_key` already
//! keys positions by, and exactly the precision the mod's entities are
//! aligned to. Tile coordinates are already integers.
//!
//! `DefineName` writes a prototype name the first time it is used and gives
//! it the next sequential id; every later reference to that name is just the
//! two byte id. One dictionary is shared by both the entity and tile
//! sections of a file (a name only needs defining once), which is why the
//! tile section can still reference a name defined during the entity
//! section. `d`, `w` and `h` are always present now: once a record is this
//! compact, a variable width encoding to omit a default value costs more
//! complexity than the bytes it would save.

use std::io;
use std::sync::Arc;

use crate::wire::{ByteReader, ByteWriter};

const MAGIC: &[u8; 4] = b"STF1";
const CURRENT_VERSION: u8 = 1;
const TRAILER_LEN: usize = 4;
const TAG_DEFINE_NAME: u8 = 0;
const TAG_ENTITY: u8 = 1;
const TAG_TILE: u8 = 2;
const TAG_END_ENTITIES: u8 = 9;

/// djb2, computed in one pass since `read_binary`/`write_binary` always hold
/// the whole file in memory already, unlike `mod/encode.lua`'s incremental
/// version of the same hash, which folds each chunk in as it's streamed to
/// disk. `u32` wrapping arithmetic here is exactly the Lua side's `% 2^32`.
/// Chosen for being trivial to implement identically on both sides without
/// a bitwise primitive (Factorio's Lua 5.2 has none), not for cryptographic
/// strength: it only needs to catch accidental corruption.
fn checksum(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 5381;
    for &b in bytes {
        hash = hash.wrapping_mul(33).wrapping_add(b as u32);
    }
    hash
}

#[derive(Debug)]
pub struct Frame {
    pub tick: u64,
    pub surface: String,
    pub entities: Vec<Entity>,
    pub count: usize,
    pub tiles: Vec<Tile>,
}

/// `n` is `Arc<str>` rather than `String`: a real base has a few dozen
/// distinct prototype names against hundreds of thousands (or, for tiles on
/// a fully paved base, millions) of entries, and the wire format already
/// carries that small deduplicated dictionary (see `read_binary` below), so
/// resolving a record's name only needs a cheap refcount bump, not a fresh
/// heap allocation and copy of the same handful of strings repeated per
/// entity. That distinction showed up directly: on a real ~300k-entity,
/// 3.1M-tile capture, `String::clone` per record was the dominant cost of
/// loading a frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub n: Arc<str>,
    pub x: f32,
    pub y: f32,
    pub d: u8,
    pub w: u32,
    pub h: u32,
}

/// Unlike entities, tiles are corner positioned and integer aligned: a tile
/// named at (x,y) occupies world space [x,x+1) x [y,y+1).
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub n: Arc<str>,
    pub x: i32,
    pub y: i32,
}

/// The write side view of a frame. Separate from `Frame` because `Frame::count`
/// is derived from `entities`: making the write side take that same derived
/// value as an input field would let it drift from the array it describes.
pub struct FrameOut<'a> {
    pub tick: u64,
    pub surface: &'a str,
    pub entities: &'a [Entity],
    pub tiles: &'a [Tile],
}

impl Frame {
    pub fn as_out(&self) -> FrameOut<'_> {
        FrameOut { tick: self.tick, surface: &self.surface, entities: &self.entities, tiles: &self.tiles }
    }
}

/// Assigns sequential ids to names in first use order, so the writer and the
/// reader agree on what each id means without ever exchanging the table up
/// front.
struct NameDict<'a> {
    ids: std::collections::HashMap<&'a str, u16>,
}

impl<'a> NameDict<'a> {
    fn new() -> Self {
        NameDict { ids: std::collections::HashMap::new() }
    }

    /// Returns the id for `name`, and whether it needed to be defined just
    /// now (the caller must then write a `DefineName` record before this
    /// one).
    fn id_for(&mut self, name: &'a str) -> (u16, bool) {
        if let Some(&id) = self.ids.get(name) {
            return (id, false);
        }
        let id = self.ids.len() as u16;
        self.ids.insert(name, id);
        (id, true)
    }
}

fn round10(v: f32) -> i32 {
    (v * 10.0).round() as i32
}

fn round10_back(scaled: i32) -> f32 {
    scaled as f32 / 10.0
}

pub fn write_binary(frame: &FrameOut<'_>) -> Vec<u8> {
    let mut w = ByteWriter::new();
    w.magic(MAGIC).u8(CURRENT_VERSION).u64(frame.tick).string(frame.surface);

    let mut names = NameDict::new();
    for entity in frame.entities {
        let (name_id, is_new) = names.id_for(&entity.n);
        if is_new {
            w.u8(TAG_DEFINE_NAME).string(&entity.n);
        }
        w.u8(TAG_ENTITY)
            .u16(name_id)
            .i32(round10(entity.x))
            .i32(round10(entity.y))
            .u8(entity.d)
            .u8(entity.w.min(255) as u8)
            .u8(entity.h.min(255) as u8);
    }
    w.u8(TAG_END_ENTITIES);

    for tile in frame.tiles {
        let (name_id, is_new) = names.id_for(&tile.n);
        if is_new {
            w.u8(TAG_DEFINE_NAME).string(&tile.n);
        }
        w.u8(TAG_TILE).u16(name_id).i32(tile.x).i32(tile.y);
    }

    let mut bytes = w.into_vec();
    let trailer = checksum(&bytes);
    bytes.extend_from_slice(&trailer.to_le_bytes());
    bytes
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame file")
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// The tick and surface from a frame file, without reading the rest of it.
///
/// Exists so a loader can group and order a whole capture before parsing any
/// of it. Grouping used to require every frame parsed and resident first,
/// which is the peak memory a streaming loader is trying to avoid: it needs
/// to know which surface each file belongs to and what order they go in
/// *before* it can fold them in one at a time and drop them.
///
/// Reads a bounded prefix rather than the file, so this costs one small read
/// per file regardless of how large the frames are. It deliberately does not
/// verify the checksum, which would mean reading everything and defeat the
/// point; the real parse still does, so a corrupt file is caught there.
pub fn read_header(path: &std::path::Path) -> io::Result<(u64, String)> {
    use std::io::Read;

    // Magic, version, tick, then a u16-prefixed surface name. Prototype and
    // surface names are short by construction (see `ByteWriter::string`), so
    // this prefix always covers the whole header.
    let mut prefix = [0u8; 512];
    let read = {
        let mut file = std::fs::File::open(path)?;
        let mut filled = 0;
        loop {
            match file.read(&mut prefix[filled..])? {
                0 => break filled,
                n => filled += n,
            }
            if filled == prefix.len() {
                break filled;
            }
        }
    };

    let mut r = ByteReader::new(&prefix[..read]);
    r.magic(MAGIC).ok_or_else(|| invalid(format!("{}: not a frame file (bad magic)", path.display())))?;
    let version = r.u8().ok_or_else(truncated)?;
    if version != CURRENT_VERSION {
        return Err(invalid(format!(
            "{}: unsupported frame format version {version} (this build only understands version {CURRENT_VERSION})",
            path.display()
        )));
    }
    let tick = r.u64().ok_or_else(truncated)?;
    let surface = r.string().ok_or_else(truncated)?;
    Ok((tick, surface))
}

pub fn read_binary(bytes: &[u8]) -> io::Result<Frame> {
    // Magic + version + at least an empty trailer: anything shorter cannot
    // possibly be a complete file, whatever else is wrong with it.
    if bytes.len() < 4 + 1 + TRAILER_LEN {
        return Err(truncated());
    }

    let mut r = ByteReader::new(bytes);
    r.magic(MAGIC).ok_or_else(|| invalid("not a frame file (bad magic)"))?;
    let version = r.u8().ok_or_else(truncated)?;
    if version != CURRENT_VERSION {
        return Err(invalid(format!(
            "unsupported frame format version {version} (this build only understands version {CURRENT_VERSION})"
        )));
    }

    let payload_end = bytes.len() - TRAILER_LEN;
    let expected = u32::from_le_bytes(bytes[payload_end..].try_into().unwrap());
    let actual = checksum(&bytes[..payload_end]);
    if actual != expected {
        return Err(invalid("checksum mismatch (corrupted or truncated frame file)"));
    }

    let tick = r.u64().ok_or_else(truncated)?;
    let surface = r.string().ok_or_else(truncated)?;

    // Arc<str>, not String: a name is defined once here but referenced by
    // every entity/tile that uses it, potentially millions of times on a
    // real base, and cloning an Arc is a refcount bump rather than a fresh
    // allocation and copy of the same handful of short strings.
    let mut names: Vec<Arc<str>> = Vec::new();

    let mut entities = Vec::new();
    loop {
        match r.tag().ok_or_else(truncated)? {
            TAG_DEFINE_NAME => names.push(Arc::from(r.string().ok_or_else(truncated)?)),
            TAG_ENTITY => {
                let name_id = r.u16().ok_or_else(truncated)? as usize;
                let x = round10_back(r.i32().ok_or_else(truncated)?);
                let y = round10_back(r.i32().ok_or_else(truncated)?);
                let d = r.u8().ok_or_else(truncated)?;
                let w = r.u8().ok_or_else(truncated)? as u32;
                let h = r.u8().ok_or_else(truncated)? as u32;
                let name = names.get(name_id).ok_or_else(|| invalid("entity references an undefined name id"))?;
                entities.push(Entity { n: Arc::clone(name), x, y, d, w, h });
            }
            TAG_END_ENTITIES => break,
            other => return Err(invalid(format!("unexpected tag {other} in entity section"))),
        }
    }

    let mut tiles = Vec::new();
    while r.consumed() < payload_end {
        match r.tag().ok_or_else(truncated)? {
            TAG_DEFINE_NAME => names.push(Arc::from(r.string().ok_or_else(truncated)?)),
            TAG_TILE => {
                let name_id = r.u16().ok_or_else(truncated)? as usize;
                let x = r.i32().ok_or_else(truncated)?;
                let y = r.i32().ok_or_else(truncated)?;
                let name = names.get(name_id).ok_or_else(|| invalid("tile references an undefined name id"))?;
                tiles.push(Tile { n: Arc::clone(name), x, y });
            }
            other => return Err(invalid(format!("unexpected tag {other} in tile section"))),
        }
    }

    Ok(Frame { tick, surface, count: entities.len(), entities, tiles })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_fixture(name: &str) -> Frame {
        let path = format!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames/{}"), name);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
        read_binary(&bytes).unwrap_or_else(|e| panic!("parsing {path}: {e}"))
    }

    /// Known counts from tests/fixtures/README.md, real captured frames, so
    /// this also guards against the format silently drifting.
    #[test]
    fn read_header_matches_a_full_parse_without_reading_the_file() {
        for name in ["frame_0000.stfr", "frame_0004.stfr"] {
            let path = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames")).join(name);
            let (tick, surface) = read_header(&path).unwrap();
            let full = load_fixture(name);
            assert_eq!(tick, full.tick, "{name}: tick");
            assert_eq!(surface, full.surface, "{name}: surface");
        }
    }

    #[test]
    fn read_header_rejects_a_file_that_is_not_a_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.stfr");
        std::fs::write(&path, b"not a frame at all").unwrap();
        assert!(read_header(&path).is_err());
    }

    #[test]
    fn real_fixtures_parse_with_known_entity_counts() {
        let expected = [
            ("frame_0000.stfr", 240),
            ("frame_0001.stfr", 589),
            ("frame_0002.stfr", 3043),
            ("frame_0003.stfr", 10234),
            ("frame_0004.stfr", 22971),
        ];
        for (name, count) in expected {
            let frame = load_fixture(name);
            assert_eq!(frame.count, count, "{name}: count field");
            assert_eq!(frame.entities.len(), count, "{name}: entities.len()");
        }
    }

    fn entity(n: &str, x: f32, y: f32, d: u8, w: u32, h: u32) -> Entity {
        Entity { n: n.into(), x, y, d, w, h }
    }

    #[test]
    fn a_frame_round_trips_through_write_and_read() {
        let entities = vec![
            entity("transport-belt", -80.5, 28.5, 4, 1, 1),
            entity("assembling-machine-1", 5.0, 5.0, 0, 3, 3),
            entity("stone-furnace", 1.0, 2.0, 0, 1, 1),
        ];
        let tiles = vec![Tile { n: "concrete".into(), x: -5, y: -12 }, Tile { n: "concrete".into(), x: -4, y: -12 }];
        let out = FrameOut { tick: 22630009, surface: "nauvis", entities: &entities, tiles: &tiles };

        let bytes = write_binary(&out);
        let frame = read_binary(&bytes).unwrap();

        assert_eq!(frame.tick, 22630009);
        assert_eq!(frame.surface, "nauvis");
        assert_eq!(frame.entities, entities);
        assert_eq!(frame.tiles, tiles);
        assert_eq!(frame.count, 3);
    }

    /// Pins the exact byte layout by hand, so a change to field order,
    /// width, or tag value shows up here rather than only as a round trip
    /// still agreeing with itself. The trailing checksum is verified via
    /// the same `checksum` function under test elsewhere, rather than a
    /// hand computed constant that would just be a second copy of the
    /// algorithm to keep in sync.
    #[test]
    fn a_single_entity_frame_matches_its_documented_byte_layout() {
        let entities = vec![entity("pipe", -80.5, 28.5, 4, 1, 1)];
        let out = FrameOut { tick: 100, surface: "nauvis", entities: &entities, tiles: &[] };
        let bytes = write_binary(&out);

        let mut expected = ByteWriter::new();
        expected
            .magic(b"STF1")
            .u8(1) // version
            .u64(100)
            .string("nauvis")
            .u8(0).string("pipe") // DefineName
            .u8(1).u16(0).i32(-805).i32(285).u8(4).u8(1).u8(1) // EntityRecord
            .u8(9); // EndEntities, no tiles follow
        let payload = expected.into_vec();

        assert_eq!(&bytes[..bytes.len() - 4], &payload[..], "payload before the trailer");
        assert_eq!(
            &bytes[bytes.len() - 4..],
            &checksum(&payload).to_le_bytes(),
            "trailer is the checksum of everything before it"
        );
    }

    #[test]
    fn a_wrong_version_is_a_distinct_error_from_a_parse_failure() {
        let entities = vec![entity("pipe", 1.0, 2.0, 0, 1, 1)];
        let out = FrameOut { tick: 1, surface: "nauvis", entities: &entities, tiles: &[] };
        let mut bytes = write_binary(&out);
        bytes[4] = 99; // the version byte, right after the 4 byte magic

        let err = read_binary(&bytes).unwrap_err();
        assert!(err.to_string().contains("version 99"), "got: {err}");
    }

    #[test]
    fn a_corrupted_byte_is_caught_by_the_checksum() {
        let entities = vec![entity("pipe", 1.0, 2.0, 0, 1, 1)];
        let out = FrameOut { tick: 1, surface: "nauvis", entities: &entities, tiles: &[] };
        let mut bytes = write_binary(&out);
        let last = bytes.len() - 1 - 4; // a byte inside the payload, not the trailer
        bytes[last] ^= 0xFF;

        let err = read_binary(&bytes).unwrap_err();
        assert!(err.to_string().contains("checksum"), "got: {err}");
    }

    /// The Rust and Lua implementations of this hash must agree, since one
    /// writes what the other reads. Also checked against the same input in
    /// mod/tests/encode_test.lua via lupa.
    #[test]
    fn checksum_matches_the_lua_side_known_vector() {
        assert_eq!(checksum(b"ab"), 5863208);
    }

    #[test]
    fn tiles_parse_including_negative_coordinates() {
        let tiles = vec![Tile { n: "concrete".into(), x: -5, y: -12 }];
        let out = FrameOut { tick: 1, surface: "nauvis", entities: &[], tiles: &tiles };
        let frame = read_binary(&write_binary(&out)).unwrap();

        assert_eq!(frame.tiles.len(), 1);
        assert_eq!((frame.tiles[0].n.as_ref(), frame.tiles[0].x, frame.tiles[0].y), ("concrete", -5, -12));
    }

    #[test]
    fn a_name_used_by_both_an_entity_and_a_tile_is_only_defined_once() {
        // Contrived (real prototypes never share a name across the two kinds
        // of thing), but it is the case that proves the dictionary is shared
        // rather than one per section: if the tile section defined its own
        // copy of "landfill" it would land at a different id than the
        // entity section's, and this frame has no way to tell two different
        // ids called "landfill" apart from just one.
        let entities = vec![entity("landfill", 0.5, 0.5, 0, 1, 1)];
        let tiles = vec![Tile { n: "landfill".into(), x: 1, y: 1 }];
        let out = FrameOut { tick: 1, surface: "nauvis", entities: &entities, tiles: &tiles };

        let frame = read_binary(&write_binary(&out)).unwrap();
        assert_eq!(frame.entities[0].n, "landfill".into());
        assert_eq!(frame.tiles[0].n, "landfill".into());
    }

    #[test]
    fn a_frame_with_no_tiles_at_all_still_parses() {
        let entities = vec![entity("pipe", 1.0, 2.0, 0, 1, 1)];
        let out = FrameOut { tick: 1, surface: "nauvis", entities: &entities, tiles: &[] };
        let frame = read_binary(&write_binary(&out)).unwrap();
        assert!(frame.tiles.is_empty());
    }

    #[test]
    fn an_unrecognised_magic_is_an_error() {
        let err = read_binary(b"nope, not a frame").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Cutting bytes off the end lands inside the trailer here (it's only 4
    /// bytes), which the checksum catches as a mismatch rather than the
    /// reader running out of bytes mid tag, still an error either way,
    /// just a different `io::ErrorKind` than a cut mid-payload would give.
    #[test]
    fn a_truncated_file_is_an_error_rather_than_a_panic() {
        let entities = vec![entity("pipe", 1.0, 2.0, 0, 1, 1)];
        let out = FrameOut { tick: 1, surface: "nauvis", entities: &entities, tiles: &[] };
        let bytes = write_binary(&out);

        let err = read_binary(&bytes[..bytes.len() - 3]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("checksum"), "got: {err}");
    }

    /// Cutting deep enough to remove real payload, not just the trailer,
    /// still must not panic: the length guard at the top of `read_binary`
    /// exists specifically for inputs shorter than a header and trailer
    /// combined.
    #[test]
    fn a_file_shorter_than_a_header_and_trailer_is_truncated_not_a_panic() {
        let err = read_binary(b"STF1\x01").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A file that ends exactly at `EndEntities` (no tile section at all,
    /// not even zero bytes of one) is complete, not truncated.
    #[test]
    fn a_file_ending_right_after_end_entities_is_not_truncated() {
        let entities = vec![entity("pipe", 1.0, 2.0, 0, 1, 1)];
        let out = FrameOut { tick: 1, surface: "nauvis", entities: &entities, tiles: &[] };
        let frame = read_binary(&write_binary(&out)).unwrap();
        assert_eq!(frame.entities.len(), 1);
        assert!(frame.tiles.is_empty());
    }
}
