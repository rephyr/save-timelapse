//! The exported frame format written by the mod and read by the viewer.
//!
//! Wire format (`frame_<tick>_<surface>.stfr`), all integers little endian:
//!
//! ```text
//! magic     4 bytes, "STF1"
//! version   u8, must equal CURRENT_VERSION
//! tick      u64
//! surface   string (u16 length, then that many UTF-8 bytes)
//! entity section, a sequence of:
//!   tag 0     DefineName  string name, u8 w, u8 h
//!   tag 1     EntityRun   varint name_id, varint count, u8 flags,
//!                         then per item varint dx, varint dy, and a u8
//!                         direction when flags has bit 0; then, when flags
//!                         has bit 1, varint len and that many bytes
//!   tag >=128 Extension   varint len, then that many bytes
//! tag 9  EndEntities (no payload), marking the start of the tile section
//! tile section, a sequence of:
//!   tag 0     DefineName  string name, u8 w, u8 h
//!   tag 2     TileRun     varint name_id, varint count, then per item
//!                         varint dx, varint dy
//!   tag >=128 Extension   varint len, then that many bytes
//! checksum  u32, djb2 of every byte before it, magic and version included
//! ```
//! Coordinates within a run are zigzag varint deltas against the previous
//! item, from the origin. Version 1 predates runs and has its own reader.
//!
//! # The extension contract
//!
//! From version 3 on, additions are extension records: tag 128 or above, a
//! varint length, then that many bytes, which an older reader skips exactly.
//! Factorio updates mods on its own while the desktop tool does not, so the
//! mod being newer than the tool is the normal state, not an edge case.
//!
//! Two rules keep it working:
//!
//! - Core tags stay below 128, so the two kinds never collide.
//! - Extension payloads never interleave with the data they annotate.
//!   `RUN_FLAG_DIRECTIONS` does, which is why an unknown column of that shape
//!   is unskippable: a reader cannot find the run's end.
//!
//! A length running past the end of the file is an error: not understanding a
//! record is fine, one that does not fit means damage.
//!
//! No item count anywhere, the incremental exporter spreading one export over
//! many ticks while play continues. Entity coordinates are position times ten,
//! the fixed point scale `world.rs::pos_key` uses. One `DefineName` dictionary
//! is shared by both sections.

use std::io;
use std::sync::Arc;

use crate::wire::{ByteReader, ByteWriter};

const MAGIC: &[u8; 4] = b"STF1";
/// Version 2 groups records into per-name runs with zigzag varint deltas, 4.7x
/// smaller than version 1, which is still read and has its own reader. Version
/// 3 is version 2's body byte for byte and only declares that extension
/// records may appear.
const CURRENT_VERSION: u8 = 3;
const MIN_SUPPORTED_VERSION: u8 = 1;
const TRAILER_LEN: usize = 4;
const TAG_DEFINE_NAME: u8 = 0;
const TAG_ENTITY: u8 = 1;
const TAG_TILE: u8 = 2;
const TAG_END_ENTITIES: u8 = 9;
/// Tags from here up are extension records: a varint byte length, then that
/// many bytes this reader is free not to understand. Core tags stay below it,
/// so the two kinds can never collide as the format grows.
const TAG_EXTENSION_MIN: u8 = 128;

/// djb2, in one pass, this side always holding the whole file. `u32` wrapping
/// is the Lua side's `% 2^32`. Chosen for being implementable identically
/// without a bitwise primitive, not for strength.
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

/// `n` is `Arc<str>` rather than `String`: a base has a few dozen distinct
/// names against hundreds of thousands of entries, and the wire format already
/// carries that dictionary, so resolving a name is a refcount bump rather than
/// a per-record allocation.
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
    write_binary_grouped(frame)
}

/// The version 1 layout, kept only to generate fixtures for the reader's
/// backward-compatibility test. Nothing writes it in anger.
#[cfg(test)]
fn write_binary_v1(frame: &FrameOut<'_>) -> Vec<u8> {
    let mut w = ByteWriter::new();
    w.magic(MAGIC).u8(1).u64(frame.tick).string(frame.surface);

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

/// Steps over one extension record's payload, its tag already read. A record
/// running off the end is an error: accepting it would turn corruption into a
/// silently short frame.
fn skip_extension(r: &mut ByteReader<'_>) -> io::Result<()> {
    let len = r.varint().ok_or_else(truncated)? as usize;
    r.skip(len).ok_or_else(truncated)
}

/// The tick and surface from a frame file without reading the rest, so a
/// loader can group and order a capture before parsing any of it. Reads a
/// bounded prefix, and deliberately skips the checksum, which would mean
/// reading everything; the real parse still verifies it.
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
    if !(MIN_SUPPORTED_VERSION..=CURRENT_VERSION).contains(&version) {
        return Err(invalid(format!(
            "{}: unsupported frame format version {version} (this build understands versions              {MIN_SUPPORTED_VERSION} through {CURRENT_VERSION})",
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
    if !(MIN_SUPPORTED_VERSION..=CURRENT_VERSION).contains(&version) {
        return Err(invalid(format!(
            "unsupported frame format version {version} (this build understands versions              {MIN_SUPPORTED_VERSION} through {CURRENT_VERSION})"
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

    if version >= 2 {
        return read_grouped_body(&mut r, payload_end, tick, surface);
    }

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

/// Bit 0 of an entity run's flags: every item in the run carries a direction
/// byte. Set per run rather than per entity because it is a property of the
/// prototype (a belt rotates, a chest does not), so within one run it is the
/// same answer every time, and a run of chests then pays nothing at all.
const RUN_FLAG_DIRECTIONS: u8 = 1;

/// Bit 1: a varint length and that many bytes of extension payload follow the
/// run's coordinates. The cheaper home for a future per-entity column, a flag
/// bit rather than a fresh dictionary and coordinate list. The payload trails
/// the run and never interleaves, which is what makes it skippable.
const RUN_FLAG_EXTENSION: u8 = 2;

/// Groups items by name, preserving both the order names appear and the order
/// within each name. Scan order is kept: coordinates are delta encoded against
/// the previous item, and a real export already has that locality.
fn group_by_name<'a, T>(items: &'a [T], name_of: impl Fn(&'a T) -> &'a str) -> Vec<(&'a str, Vec<&'a T>)> {
    let mut order: Vec<&str> = Vec::new();
    let mut groups: std::collections::HashMap<&str, Vec<&T>> = std::collections::HashMap::new();
    for item in items {
        let name = name_of(item);
        groups.entry(name).or_insert_with(|| {
            order.push(name);
            Vec::new()
        });
        groups.get_mut(name).expect("just inserted").push(item);
    }
    order.into_iter().map(|name| (name, groups.remove(name).expect("in order"))).collect()
}

fn write_binary_grouped(frame: &FrameOut<'_>) -> Vec<u8> {
    let mut w = ByteWriter::new();
    w.magic(MAGIC).u8(CURRENT_VERSION).u64(frame.tick).string(frame.surface);

    let mut names = NameDict::new();
    for (name, group) in group_by_name(frame.entities, |e| &e.n) {
        let (name_id, is_new) = names.id_for(name);
        if is_new {
            // Footprint lives with the name, not the entity: it is a property
            // of the prototype, so every assembling machine repeating "3x3"
            // was two bytes per entity spent restating a constant.
            let first = group[0];
            w.u8(TAG_DEFINE_NAME)
                .string(name)
                .u8(first.w.clamp(1, u8::MAX as u32) as u8)
                .u8(first.h.clamp(1, u8::MAX as u32) as u8);
        }

        let directions = group.iter().any(|e| e.d != 0);
        let flags = if directions { RUN_FLAG_DIRECTIONS } else { 0 };
        w.u8(TAG_ENTITY).varint(name_id as u64).varint(group.len() as u64).u8(flags);

        let (mut px, mut py) = (0i32, 0i32);
        for entity in group {
            let (x, y) = (round10(entity.x), round10(entity.y));
            w.varint_i32(x - px).varint_i32(y - py);
            if directions {
                w.u8(entity.d);
            }
            (px, py) = (x, y);
        }
    }
    w.u8(TAG_END_ENTITIES);

    for (name, group) in group_by_name(frame.tiles, |t| &t.n) {
        let (name_id, is_new) = names.id_for(name);
        if is_new {
            // Tiles are always one by one, but the footprint is written
            // anyway so a name definition has one shape in both sections and
            // the shared dictionary stays a single record type.
            w.u8(TAG_DEFINE_NAME).string(name).u8(1).u8(1);
        }
        w.u8(TAG_TILE).varint(name_id as u64).varint(group.len() as u64);

        let (mut px, mut py) = (0i32, 0i32);
        for tile in group {
            w.varint_i32(tile.x - px).varint_i32(tile.y - py);
            (px, py) = (tile.x, tile.y);
        }
    }

    let mut bytes = w.into_vec();
    let trailer = checksum(&bytes);
    bytes.extend_from_slice(&trailer.to_le_bytes());
    bytes
}

fn read_grouped_body(r: &mut ByteReader<'_>, payload_end: usize, tick: u64, surface: String) -> io::Result<Frame> {
    // Name, then the footprint every entity of that name shares.
    let mut names: Vec<(Arc<str>, u32, u32)> = Vec::new();
    let resolve = |names: &[(Arc<str>, u32, u32)], id: usize| {
        names.get(id).cloned().ok_or_else(|| invalid("record references an undefined name id"))
    };

    let mut entities = Vec::new();
    loop {
        match r.tag().ok_or_else(truncated)? {
            TAG_DEFINE_NAME => {
                let name = Arc::from(r.string().ok_or_else(truncated)?);
                let w = r.u8().ok_or_else(truncated)? as u32;
                let h = r.u8().ok_or_else(truncated)? as u32;
                names.push((name, w, h));
            }
            TAG_ENTITY => {
                let name_id = r.varint().ok_or_else(truncated)? as usize;
                let count = r.varint().ok_or_else(truncated)? as usize;
                let flags = r.u8().ok_or_else(truncated)?;
                let (name, w, h) = resolve(&names, name_id)?;
                let directions = flags & RUN_FLAG_DIRECTIONS != 0;

                entities.reserve(count);
                let (mut px, mut py) = (0i32, 0i32);
                for _ in 0..count {
                    px += r.varint_i32().ok_or_else(truncated)?;
                    py += r.varint_i32().ok_or_else(truncated)?;
                    let d = if directions { r.u8().ok_or_else(truncated)? } else { 0 };
                    entities.push(Entity { n: Arc::clone(&name), x: round10_back(px), y: round10_back(py), d, w, h });
                }

                if flags & RUN_FLAG_EXTENSION != 0 {
                    skip_extension(r)?;
                }
            }
            TAG_END_ENTITIES => break,
            other if other >= TAG_EXTENSION_MIN => skip_extension(r)?,
            other => return Err(invalid(format!("unexpected tag {other} in entity section"))),
        }
    }

    let mut tiles = Vec::new();
    while r.consumed() < payload_end {
        match r.tag().ok_or_else(truncated)? {
            TAG_DEFINE_NAME => {
                let name = Arc::from(r.string().ok_or_else(truncated)?);
                let w = r.u8().ok_or_else(truncated)? as u32;
                let h = r.u8().ok_or_else(truncated)? as u32;
                names.push((name, w, h));
            }
            TAG_TILE => {
                let name_id = r.varint().ok_or_else(truncated)? as usize;
                let count = r.varint().ok_or_else(truncated)? as usize;
                let (name, _, _) = resolve(&names, name_id)?;

                tiles.reserve(count);
                let (mut px, mut py) = (0i32, 0i32);
                for _ in 0..count {
                    px += r.varint_i32().ok_or_else(truncated)?;
                    py += r.varint_i32().ok_or_else(truncated)?;
                    tiles.push(Tile { n: Arc::clone(&name), x: px, y: py });
                }
            }
            other if other >= TAG_EXTENSION_MIN => skip_extension(r)?,
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

    /// Pins the byte layout by hand, so a change to field order, width or tag
    /// shows up here rather than as a round trip agreeing with itself.
    #[test]
    fn a_version_1_frame_still_reads_identically() {
        let entities = vec![
            entity("transport-belt", -80.5, 28.5, 4, 1, 1),
            entity("assembling-machine-1", 12.0, -3.5, 0, 3, 3),
            entity("transport-belt", -79.5, 28.5, 6, 1, 1),
        ];
        let tiles = vec![Tile { n: "concrete".into(), x: -5, y: 12 }];
        let out = FrameOut { tick: 4242, surface: "nauvis", entities: &entities, tiles: &tiles };

        let old = read_binary(&write_binary_v1(&out)).expect("v1 must still parse");
        let new = read_binary(&write_binary(&out)).expect("v2 must parse");

        assert_eq!(old.tick, new.tick);
        assert_eq!(old.surface, new.surface);
        assert_eq!(old.tiles, new.tiles);
        // Grouping reorders entities by name, so compare as multisets: what
        // matters is that every entity survives intact, not its position in
        // the array.
        let key = |e: &Entity| (e.n.to_string(), round10(e.x), round10(e.y), e.d, e.w, e.h);
        let mut old_keys: Vec<_> = old.entities.iter().map(key).collect();
        let mut new_keys: Vec<_> = new.entities.iter().map(key).collect();
        old_keys.sort();
        new_keys.sort();
        assert_eq!(old_keys, new_keys);
    }

    /// The gain, asserted rather than described, on the same real frame the
    /// format study measured.
    #[test]
    fn version_2_is_several_times_smaller_than_version_1() {
        let frame = load_fixture("frame_0004.stfr");
        let out = frame.as_out();
        let v1 = write_binary_v1(&out).len();
        let v2 = write_binary(&out).len();
        let ratio = v1 as f64 / v2 as f64;
        assert!(ratio > 4.0, "expected v2 to be over 4x smaller, got {ratio:.2}x ({v1} vs {v2})");
        let per_entity = v2 as f64 / frame.entities.len() as f64;
        assert!(per_entity < 3.5, "expected under 3.5 bytes per entity, got {per_entity:.2}");
    }

    #[test]
    fn a_single_entity_frame_matches_its_documented_byte_layout() {
        let entities = vec![entity("pipe", -80.5, 28.5, 4, 1, 1)];
        let out = FrameOut { tick: 100, surface: "nauvis", entities: &entities, tiles: &[] };
        let bytes = write_binary(&out);

        let mut expected = ByteWriter::new();
        expected
            .magic(b"STF1")
            .u8(3) // version
            .u64(100)
            .string("nauvis")
            // DefineName carries the prototype's footprint, so entities do not.
            .u8(0)
            .string("pipe")
            .u8(1)
            .u8(1)
            // EntityRun: name id, count, flags (this one rotates, so each
            // item carries a direction byte).
            .u8(1)
            .varint(0)
            .varint(1)
            .u8(RUN_FLAG_DIRECTIONS)
            // First item's coordinates are deltas from the origin.
            .varint_i32(-805)
            .varint_i32(285)
            .u8(4)
            .u8(9); // EndEntities, no tiles follow
        let payload = expected.into_vec();

        assert_eq!(&bytes[..bytes.len() - 4], &payload[..], "payload before the trailer");
        assert_eq!(
            &bytes[bytes.len() - 4..],
            &checksum(&payload).to_le_bytes(),
            "trailer is the checksum of everything before it"
        );
    }

    /// Finishes a hand written payload the way the writer does, so a test can
    /// spell out a byte layout without also restating the trailer rule.
    fn sealed(payload: ByteWriter) -> Vec<u8> {
        let mut bytes = payload.into_vec();
        let trailer = checksum(&bytes);
        bytes.extend_from_slice(&trailer.to_le_bytes());
        bytes
    }

    /// A record this build has no meaning for costs nothing but its bytes.
    /// Covers both section loops and both ends of a section, which are
    /// different points in the loop.
    #[test]
    fn unknown_extension_records_are_skipped_rather_than_failing_the_parse() {
        let mut w = ByteWriter::new();
        w.magic(MAGIC)
            .u8(CURRENT_VERSION)
            .u64(7)
            .string("nauvis")
            .u8(TAG_EXTENSION_MIN)
            .varint(3)
            .u8(0xAA)
            .u8(0xBB)
            .u8(0xCC)
            .u8(TAG_DEFINE_NAME)
            .string("pipe")
            .u8(1)
            .u8(1)
            .u8(TAG_ENTITY)
            .varint(0)
            .varint(1)
            .u8(0)
            .varint_i32(15)
            .varint_i32(25)
            // A different extension tag, and an empty one, between the last
            // run and the end of the section.
            .u8(TAG_EXTENSION_MIN + 40)
            .varint(0)
            .u8(TAG_END_ENTITIES)
            // The tile section runs its own loop, so it needs its own.
            .u8(TAG_EXTENSION_MIN)
            .varint(2)
            .u8(1)
            .u8(2)
            .u8(TAG_DEFINE_NAME)
            .string("concrete")
            .u8(1)
            .u8(1)
            .u8(TAG_TILE)
            .varint(1)
            .varint(1)
            .varint_i32(-5)
            .varint_i32(12);

        let frame = read_binary(&sealed(w)).expect("an unknown extension must not fail the parse");
        assert_eq!(frame.entities.len(), 1);
        assert_eq!((frame.entities[0].x, frame.entities[0].y), (1.5, 2.5));
        assert_eq!(frame.tiles, vec![Tile { n: "concrete".into(), x: -5, y: 12 }]);
    }

    /// Written alongside `RUN_FLAG_DIRECTIONS` on purpose: directions
    /// interleave and the extension block trails, and a reader has to get both
    /// right to land on the next tag.
    #[test]
    fn a_run_extension_payload_is_skipped_and_the_run_still_decodes() {
        let mut w = ByteWriter::new();
        w.magic(MAGIC)
            .u8(CURRENT_VERSION)
            .u64(1)
            .string("nauvis")
            .u8(TAG_DEFINE_NAME)
            .string("transport-belt")
            .u8(1)
            .u8(1)
            .u8(TAG_ENTITY)
            .varint(0)
            .varint(2)
            .u8(RUN_FLAG_DIRECTIONS | RUN_FLAG_EXTENSION)
            .varint_i32(10)
            .varint_i32(0)
            .u8(4)
            .varint_i32(10)
            .varint_i32(0)
            .u8(6)
            // One byte per entity, of something this build has never heard of.
            .varint(2)
            .u8(0)
            .u8(1)
            .u8(TAG_END_ENTITIES);

        let frame = read_binary(&sealed(w)).expect("a run extension must not fail the parse");
        assert_eq!(frame.entities.len(), 2);
        assert_eq!((frame.entities[0].x, frame.entities[0].d), (1.0, 4));
        assert_eq!((frame.entities[1].x, frame.entities[1].d), (2.0, 6));
    }

    /// Skipping an unfamiliar record is tolerance; accepting a length that
    /// points past the last byte would be swallowing corruption. The checksum
    /// passes here, so only the length check can catch this.
    #[test]
    fn an_extension_claiming_more_bytes_than_the_file_holds_is_an_error() {
        let mut w = ByteWriter::new();
        w.magic(MAGIC).u8(CURRENT_VERSION).u64(1).string("nauvis").u8(TAG_EXTENSION_MIN).varint(9999).u8(TAG_END_ENTITIES);

        let err = read_binary(&sealed(w)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// Asserted by relabelling a fresh file as version 2 and getting the same
    /// parse. If the bodies diverge, the promise that an old capture still
    /// loads quietly stops holding.
    #[test]
    fn version_3_writes_the_same_body_as_version_2() {
        let entities = vec![entity("transport-belt", -80.5, 28.5, 4, 1, 1), entity("assembling-machine-1", 5.0, 5.0, 0, 3, 3)];
        let tiles = vec![Tile { n: "concrete".into(), x: -5, y: -12 }];
        let out = FrameOut { tick: 4242, surface: "nauvis", entities: &entities, tiles: &tiles };

        let current = write_binary(&out);
        assert_eq!(current[4], 3, "the writer stamps the current version");

        let mut relabelled = current.clone();
        relabelled[4] = 2;
        // The trailer covers the version byte, so restamping it means
        // recomputing the checksum rather than carrying the old one over.
        let end = relabelled.len() - TRAILER_LEN;
        let trailer = checksum(&relabelled[..end]);
        relabelled[end..].copy_from_slice(&trailer.to_le_bytes());

        let as_v2 = read_binary(&relabelled).expect("v2 must still read");
        let as_v3 = read_binary(&current).expect("v3 must read");
        assert_eq!(as_v2.tick, as_v3.tick);
        assert_eq!(as_v2.entities, as_v3.entities);
        assert_eq!(as_v2.tiles, as_v3.tiles);
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

    /// Regenerates the fixtures `tests/format_compatibility.rs` reads. Ignored
    /// because it writes into the source tree.
    ///
    /// All three hold the same real captured contents, read out of a version 1
    /// fixture, so the compatibility test compares three encodings of one frame
    /// rather than three synthetic shapes. Only run for a genuine format
    /// change: regenerating to make a failing test pass deletes the evidence.
    #[test]
    #[ignore]
    fn regenerate_compatibility_fixtures() {
        let source = load_fixture("frame_0001.stfr");
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames"));

        // No v1 fixture is written, `frame_0001.stfr` already being real mod
        // output. Asserted rather than assumed, since it is what makes the v2
        // and v3 fixtures trustworthy.
        assert_eq!(
            write_binary_v1(&source.as_out()),
            std::fs::read(dir.join("frame_0001.stfr")).unwrap(),
            "the v1 writer must still reproduce the real captured frame it stands in for"
        );

        let v3 = write_binary(&source.as_out());
        std::fs::write(dir.join("compat_v3.stfr"), &v3).unwrap();

        // Version 2's body is version 3's, byte for byte (see
        // `version_3_writes_the_same_body_as_version_2`), so the v2 fixture
        // is the same payload restamped, with the trailer recomputed because
        // it covers the version byte.
        let mut v2 = v3;
        v2[4] = 2;
        let end = v2.len() - TRAILER_LEN;
        let trailer = checksum(&v2[..end]);
        v2[end..].copy_from_slice(&trailer.to_le_bytes());
        std::fs::write(dir.join("compat_v2.stfr"), &v2).unwrap();
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
        // Contrived, but it proves the dictionary is shared rather than one
        // per section: a tile section defining its own "landfill" would land
        // at a different id.
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

#[cfg(test)]
mod format_study {
    use super::*;

    fn varint_len(mut n: u64) -> usize {
        let mut len = 1;
        while n >= 0x80 {
            n >>= 7;
            len += 1;
        }
        len
    }

    fn zigzag(v: i32) -> u64 {
        ((v << 1) ^ (v >> 31)) as u32 as u64
    }

    /// What the candidate v2 encodings would actually cost on a real capture,
    /// so the format is chosen from measurements rather than from a guess
    /// about how well coordinates delta-encode.
    #[test]
    #[ignore]
    fn measure_candidate_encodings() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames/frame_0004.stfr");
        let frame = read_binary(&std::fs::read(path).unwrap()).unwrap();
        let n = frame.entities.len();

        // Group by name, as every candidate does.
        let mut by_name: std::collections::HashMap<&str, Vec<&Entity>> = std::collections::HashMap::new();
        for e in &frame.entities {
            by_name.entry(&e.n).or_default().push(e);
        }

        let current = n * 14;

        let mut absolute = 0usize;
        let mut delta_scan = 0usize;
        let mut delta_sorted = 0usize;
        for group in by_name.values() {
            // name_id + count, once per run rather than per entity.
            let header = 1 + varint_len(group.len() as u64) + varint_len(64);
            absolute += header;
            delta_scan += header;
            delta_sorted += header;

            for e in group.iter() {
                absolute += varint_len(zigzag(round10(e.x))) + varint_len(zigzag(round10(e.y)));
            }

            let mut prev = (0i32, 0i32);
            for e in group.iter() {
                let (x, y) = (round10(e.x), round10(e.y));
                delta_scan += varint_len(zigzag(x - prev.0)) + varint_len(zigzag(y - prev.1));
                prev = (x, y);
            }

            let mut sorted: Vec<(i32, i32)> = group.iter().map(|e| (round10(e.y), round10(e.x))).collect();
            sorted.sort_unstable();
            let mut prev = (0i32, 0i32);
            for &(y, x) in &sorted {
                delta_sorted += varint_len(zigzag(x - prev.1)) + varint_len(zigzag(y - prev.0));
                prev = (y, x);
            }
        }

        // Direction: only entities whose prototype can rotate need it. Every
        // candidate carries it the same way, so it is added once here.
        let rotatable = frame.entities.iter().filter(|e| e.d != 0).count();
        let report = |label: &str, bytes: usize| {
            let total = bytes + rotatable;
            println!(
                "STUDY {label:<22} {:>8} bytes  {:>5.2} B/entity  {:>5.2}x smaller",
                total,
                total as f64 / n as f64,
                current as f64 / total as f64
            );
        };
        println!("STUDY entities={n} rotatable={rotatable} names={}", by_name.len());
        println!("STUDY {:<22} {:>8} bytes  {:>5.2} B/entity", "current v1", current, 14.0);
        report("runs+varint absolute", absolute);
        report("runs+varint delta scan", delta_scan);
        report("runs+varint delta sorted", delta_sorted);
    }
}
