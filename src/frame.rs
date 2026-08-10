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
//! Coordinates within a run are zigzag varint deltas against the previous
//! item, starting from the origin. Version 1 predates runs entirely, writing
//! one fixed width record per entity or tile, and `read_binary` keeps a
//! separate function for it.
//!
//! # The extension contract
//!
//! Version 3 is where this format stops changing shape. Everything added
//! after it goes in an extension record: a tag of 128 or above, a varint byte
//! length, then that many bytes. A reader that does not recognise the tag
//! skips exactly that many bytes and carries on, so a capture written by a
//! newer mod still loads in an older tool, minus whatever the new record
//! described.
//!
//! That property is the point, because of how this project is actually
//! installed. Factorio updates mods from the portal on its own; the desktop
//! tool does not update itself. The mod being newer than the tool is
//! therefore the normal state of anyone who installed once and kept playing,
//! not an edge case, and before extension records that combination could only
//! produce a hard refusal.
//!
//! Two rules keep it working:
//!
//! - Core tags stay below 128, so the two kinds never collide.
//! - Extension payloads are never interleaved with the data they annotate.
//!   `RUN_FLAG_DIRECTIONS` is interleaved, and that is precisely why an
//!   unknown column of that shape is unskippable: without knowing the column
//!   is there, a reader cannot find where the run ends. A trailing,
//!   length prefixed block has no such problem.
//!
//! A length that runs past the end of the file is still an error. Not
//! understanding a record is fine; a record that does not fit means the file
//! is damaged.
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
/// Version 2 groups records into per-name runs and encodes coordinates as
/// zigzag varint deltas, which measured 4.7x smaller than version 1 on a real
/// frame (see `format_study`). Version 1 is still read: a capture written by
/// an older mod is worth keeping openable, and the shape is different enough
/// that the two bodies are simply separate functions.
///
/// Version 3 is version 2's body byte for byte. It changes nothing about how
/// entities and tiles are written and exists only to declare "this file may
/// contain extension records", so that a build predating them refuses it up
/// front with a clear message instead of desynchronising on the first one it
/// cannot skip. See the extension contract in this module's header: from
/// version 3 onward additions go in extension records, so this constant is
/// intended never to rise again.
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

/// Steps over one extension record's payload, its tag having already been
/// read.
///
/// A record that runs off the end of the file is still an error: not
/// understanding a feature is fine, but a length that points past the last
/// byte means the file is damaged, and quietly accepting it would turn
/// corruption into a silently short frame.
fn skip_extension(r: &mut ByteReader<'_>) -> io::Result<()> {
    let len = r.varint().ok_or_else(truncated)? as usize;
    r.skip(len).ok_or_else(truncated)
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
/// run's coordinates.
///
/// The top level extension record can already express anything, so this is
/// purely the cheaper home for the likeliest kind of future addition: one more
/// column alongside the existing per-entity ones (quality, say, or health).
/// Putting it here costs a single flag bit on runs that do not use it, rather
/// than a fresh dictionary and coordinate list to re-associate a top level
/// record with the entities it describes.
///
/// The payload goes *after* the run, never interleaved with the coordinates.
/// That is the whole reason it is skippable: `RUN_FLAG_DIRECTIONS` is
/// interleaved, which is why an old reader meeting an unknown interleaved
/// column could not find where the run ended. Anything added here must keep
/// to a trailing block for the same reason.
const RUN_FLAG_EXTENSION: u8 = 2;

/// Groups items by name, preserving both the order names first appear and the
/// order of items within each name.
///
/// Scan order is kept deliberately. Coordinates are delta encoded against the
/// previous item in the run, so this only pays off if consecutive items are
/// near each other, and measuring a real frame showed the export order
/// already has that locality: players and blueprints lay same-type entities
/// out in rows and the scan preserves it. Sorting spatially first measured
/// 0.3% better, which is not worth a sort of every entity during a live
/// export.
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
                    entities.push(Entity {
                        n: Arc::clone(&name),
                        x: round10_back(px),
                        y: round10_back(py),
                        d,
                        w,
                        h,
                    });
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

    /// Pins the exact byte layout by hand, so a change to field order,
    /// width, or tag value shows up here rather than only as a round trip
    /// still agreeing with itself. The trailing checksum is verified via
    /// the same `checksum` function under test elsewhere, rather than a
    /// hand computed constant that would just be a second copy of the
    /// algorithm to keep in sync.
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
            .u8(0).string("pipe").u8(1).u8(1)
            // EntityRun: name id, count, flags (this one rotates, so each
            // item carries a direction byte).
            .u8(1).varint(0).varint(1).u8(RUN_FLAG_DIRECTIONS)
            // First item's coordinates are deltas from the origin.
            .varint_i32(-805).varint_i32(285).u8(4)
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

    /// The whole point of the extension contract: a record this build has no
    /// meaning for costs it nothing but the bytes it occupies. Covers both
    /// section loops, since they are separate code paths, and both ends of a
    /// section, since a record before the first run and one after the last
    /// exercise different points in the loop.
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

    /// The per-run extension point, which is the one a future per-entity
    /// column would use. Written alongside `RUN_FLAG_DIRECTIONS` on purpose:
    /// directions are interleaved and the extension block trails the run, and
    /// a reader has to get both right in the same pass to land on the next
    /// tag.
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

    /// Version 3 claims to have changed nothing except declaring that
    /// extension records may appear. Asserted by relabelling a freshly
    /// written file as version 2 and getting the same parse out: if the
    /// bodies ever diverge, the promise that an old capture still loads (and
    /// that an old tool still reads a new capture using no new features)
    /// quietly stops holding.
    #[test]
    fn version_3_writes_the_same_body_as_version_2() {
        let entities = vec![
            entity("transport-belt", -80.5, 28.5, 4, 1, 1),
            entity("assembling-machine-1", 5.0, 5.0, 0, 3, 3),
        ];
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

    /// Regenerates the committed compatibility fixtures that
    /// `tests/format_compatibility.rs` reads. Ignored because it writes into
    /// the source tree; run it deliberately with
    /// `cargo test --lib regenerate_compatibility_fixtures -- --ignored`
    /// and commit whatever changes.
    ///
    /// All three hold the same real captured entities and tiles, read back
    /// out of a version 1 fixture, so what the compatibility test compares is
    /// three encodings of one frame rather than three synthetic shapes that
    /// each happen to round trip.
    ///
    /// Only ever run again if a genuine format change lands. Regenerating
    /// these to make a failing test pass would be deleting the evidence: the
    /// whole point is that they are bytes an older build wrote and this one
    /// must keep reading.
    #[test]
    #[ignore]
    fn regenerate_compatibility_fixtures() {
        let source = load_fixture("frame_0001.stfr");
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/frames"));

        // No v1 fixture is written: `frame_0001.stfr` is already one, and it
        // is real mod output rather than anything reconstructed here. Asserted
        // rather than assumed, because it is also what makes the v2 and v3
        // fixtures below trustworthy: they carry that same real frame's
        // contents, so if this writer ever stopped reproducing what the old
        // mod actually wrote, they would silently stop representing a real
        // capture too.
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
