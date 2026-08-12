//! Reading the live capture event log written by mod/control.lua.
//!
//! Wire format (`events_<start_tick>.stev`), all integers little endian. Each
//! playthrough's segments live in their own session subfolder, `game.tick`
//! restarting from 0 for every save:
//!
//! ```text
//! magic   4 bytes, "STE1", written once when the segment is created
//! version u8, MIN_SUPPORTED_VERSION through CURRENT_VERSION
//!
//! then a sequence of tagged records:
//!   tag 0  DefineName     string                    (id implicit, next in order)
//!   tag 1  DefineSurface  string                    (id implicit, next in order)
//!   tag 2  SetTick        u64 tick
//!   tag 3  AddEntity      u16 name_id, i32 x10, i32 y10, u8 d, u8 w, u8 h,
//!                         u64 id, u16 surface_id
//!   tag 4  RemoveEntity   i32 x10, i32 y10, u64 id, u16 surface_id
//!   tag 5  AddTile        u16 name_id, i32 x, i32 y, u16 surface_id
//!   tag 6  RemoveTile     i32 x, i32 y, u16 surface_id
//!   tag 7  ResetDictionaries (no payload, version 2 and later)
//!   tag 128 RemoveName    varint len, then varint name_id: names what the
//!                         next RemoveEntity is for (version 3 and later)
//!   tag >=128 Extension    varint len, then that many bytes
//! ```
//!
//! `SetTick` is emitted once per distinct tick that has events, many events
//! usually sharing one, and is always a fresh segment's first record. `id`
//! uses 0 to mean "no id".
//!
//! Append only and flushed periodically, so the last record can be a partial
//! write if the game was killed: one that does not fit ends the stream rather
//! than failing it. No trailing checksum, unlike the frame format: a segment
//! is abandoned rather than closed, so there is no finished moment.
//!
//! # The extension contract
//!
//! From version 3 on, additions are extension records: tag 128 or above, a
//! varint length, then that many bytes, which an older reader skips exactly.
//! Without it an unknown tag returned `None`, which an iterator reports as the
//! end of the stream, so a replay stopped partway with nothing to say about
//! why. Core tags stay below 128, so the two kinds never collide.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::wire::ByteReader;

const MAGIC: &[u8; 4] = b"STE1";
/// Versions this reader understands. Version 1 predates the dictionary-reset
/// record and is still accepted, carrying the mislabeling bug that record
/// fixes, which nothing on this side can undo. Version 3 writes the same
/// records as version 2 and only declares that extension records may appear.
const CURRENT_VERSION: u8 = 3;
const MIN_SUPPORTED_VERSION: u8 = 1;
/// Tags from here up carry their own length and may be skipped. Core record
/// tags stay below it so the two can never collide as the format grows.
const TAG_EXTENSION_MIN: u8 = 128;
/// Names the entity the next `RemoveEntity` is for. An extension rather than a
/// field on that record, the core layout being frozen at version 3, so a tool
/// older than this steps over it and resolves the removal the way it always
/// did. See `encode.event_remove_name`.
const TAG_REMOVE_NAME: u8 = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    AddEntity {
        name: String,
        x: f32,
        y: f32,
        d: u8,
        w: u32,
        h: u32,
        id: Option<u64>,
    },
    /// Removal of an entity. `id` matches anything built after capture began;
    /// `pos` is what resolves an entity that existed when the baseline was
    /// taken, a snapshot recording no ids. Both are always present.
    RemoveEntity {
        id: Option<u64>,
        pos: (f32, f32),
        /// Which of the things sharing this position was mined, when the mod
        /// said. Only ever set for a resource, the one thing that can be
        /// buried, so `None` means "whatever is on top" as it always did.
        name: Option<String>,
    },
    AddTile {
        name: String,
        x: i32,
        y: i32,
    },
    RemoveTile {
        x: i32,
        y: i32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoggedEvent {
    pub tick: u64,
    pub surface: String,
    pub event: Event,
}

/// Streams one segment's records, rebuilding the name and surface
/// dictionaries as `DefineName`/`DefineSurface` records are encountered, the
/// same way the writer built them while logging.
pub struct EventStream {
    bytes: Vec<u8>,
    pos: usize,
    names: Vec<String>,
    surfaces: Vec<String>,
    current_tick: Option<u64>,
    /// Set by a `TAG_REMOVE_NAME` record and consumed by the removal that
    /// follows it. A `DefineSurface` may sit between the two, the writer
    /// emitting one with the removal itself, so only an add or a dictionary
    /// reset clears it.
    pending_remove_name: Option<String>,
    unknown_extensions: usize,
    undefined_references: usize,
}

impl EventStream {
    /// Extension records stepped over because this build does not recognise
    /// their tag, which only means the capture is newer than the tool. Only
    /// meaningful once the stream has been walked.
    pub fn unknown_extensions(&self) -> usize {
        self.unknown_extensions
    }

    /// Records dropped because they named a dictionary entry this segment
    /// never defined, which is damage rather than a version difference.
    ///
    /// Counted because it used to be silent: a capture reset left the mod's
    /// buffer holding records encoded against the deleted segment's
    /// dictionary, they were flushed into a fresh segment that defined none of
    /// those names, and every one vanished here without a word. A whole
    /// playthrough recorded nothing and looked like it had simply not been
    /// played.
    pub fn undefined_references(&self) -> usize {
        self.undefined_references
    }
}

/// A record's name and surface, counting a miss in `undefined`. Resolved to
/// owned strings so the caller is not left holding a borrow of the
/// dictionaries, and every caller cloned them anyway.
///
/// Free functions taking one field each because `EventStream::next` holds a
/// reader over `bytes` for the whole loop body, so nothing there can take
/// `&mut self`.
fn resolve(
    names: &[String],
    surfaces: &[String],
    undefined: &mut usize,
    name_id: usize,
    surface_id: usize,
) -> (Option<String>, Option<String>) {
    let name = names.get(name_id).cloned();
    let surface = surfaces.get(surface_id).cloned();
    if name.is_none() || surface.is_none() {
        *undefined += 1;
    }
    (name, surface)
}

fn resolve_surface(surfaces: &[String], undefined: &mut usize, surface_id: usize) -> Option<String> {
    let surface = surfaces.get(surface_id).cloned();
    if surface.is_none() {
        *undefined += 1;
    }
    surface
}

impl Iterator for EventStream {
    type Item = LoggedEvent;

    fn next(&mut self) -> Option<LoggedEvent> {
        loop {
            let mut r = ByteReader::new(&self.bytes[self.pos..]);
            let tag = r.tag()?;

            match tag {
                0 => {
                    let name = r.string()?;
                    self.names.push(name);
                }
                1 => {
                    let name = r.string()?;
                    self.surfaces.push(name);
                }
                2 => {
                    self.current_tick = Some(r.u64()?);
                }
                // Written when the mod resumes a segment across a save load:
                // Factorio has emptied the writer's dictionaries while this
                // file kept every earlier define, so ids restart on both
                // sides. Version 1 has no such record, so a version 1 file
                // that was resumed mislabels everything after it.
                7 => {
                    self.names.clear();
                    self.surfaces.clear();
                    self.pending_remove_name = None;
                }
                3 => {
                    self.pending_remove_name = None;
                    let name_id = r.u16()? as usize;
                    let x = r.i32()?;
                    let y = r.i32()?;
                    let d = r.u8()?;
                    let w = r.u8()?;
                    let h = r.u8()?;
                    let id = r.u64()?;
                    let surface_id = r.u16()? as usize;
                    let (name, surface) =
                        resolve(&self.names, &self.surfaces, &mut self.undefined_references, name_id, surface_id);
                    let event = match (self.current_tick, name, surface) {
                        (Some(tick), Some(name), Some(surface)) => Some(LoggedEvent {
                            tick,
                            surface,
                            event: Event::AddEntity {
                                name,
                                x: x as f32 / 10.0,
                                y: y as f32 / 10.0,
                                d,
                                w: w as u32,
                                h: h as u32,
                                id: (id != 0).then_some(id),
                            },
                        }),
                        _ => None,
                    };
                    self.pos += r.consumed();
                    if let Some(logged) = event {
                        return Some(logged);
                    }
                    continue;
                }
                4 => {
                    let x = r.i32()?;
                    let y = r.i32()?;
                    let id = r.u64()?;
                    let surface_id = r.u16()? as usize;
                    let surface = resolve_surface(&self.surfaces, &mut self.undefined_references, surface_id);
                    let event = match (self.current_tick, surface) {
                        (Some(tick), Some(surface)) => Some(LoggedEvent {
                            tick,
                            surface,
                            event: Event::RemoveEntity {
                                id: (id != 0).then_some(id),
                                pos: (x as f32 / 10.0, y as f32 / 10.0),
                                name: self.pending_remove_name.take(),
                            },
                        }),
                        _ => None,
                    };
                    self.pos += r.consumed();
                    if let Some(logged) = event {
                        return Some(logged);
                    }
                    continue;
                }
                5 => {
                    let name_id = r.u16()? as usize;
                    let x = r.i32()?;
                    let y = r.i32()?;
                    let surface_id = r.u16()? as usize;
                    let (name, surface) =
                        resolve(&self.names, &self.surfaces, &mut self.undefined_references, name_id, surface_id);
                    let event = match (self.current_tick, name, surface) {
                        (Some(tick), Some(name), Some(surface)) => {
                            Some(LoggedEvent { tick, surface, event: Event::AddTile { name, x, y } })
                        }
                        _ => None,
                    };
                    self.pos += r.consumed();
                    if let Some(logged) = event {
                        return Some(logged);
                    }
                    continue;
                }
                6 => {
                    let x = r.i32()?;
                    let y = r.i32()?;
                    let surface_id = r.u16()? as usize;
                    let surface = resolve_surface(&self.surfaces, &mut self.undefined_references, surface_id);
                    let event = match (self.current_tick, surface) {
                        (Some(tick), Some(surface)) => Some(LoggedEvent { tick, surface, event: Event::RemoveTile { x, y } }),
                        _ => None,
                    };
                    self.pos += r.consumed();
                    if let Some(logged) = event {
                        return Some(logged);
                    }
                    continue;
                }
                // A tag this build has no meaning for, carrying its own length
                // so it can be stepped over. A length past the end of the file
                // falls out as `None` from `skip`, the same treatment every
                // record gets when the game was killed mid write.
                // Names what the next removal is for. Read out of its own
                // length rather than off the stream, so a payload that grows
                // later is still stepped over exactly.
                TAG_REMOVE_NAME => {
                    let len = r.varint()? as usize;
                    let payload = r.bytes(len)?;
                    self.pending_remove_name =
                        ByteReader::new(payload).varint().and_then(|id| self.names.get(id as usize)).cloned();
                    self.pos += r.consumed();
                    continue;
                }
                t if t >= TAG_EXTENSION_MIN => {
                    let len = r.varint()? as usize;
                    r.skip(len)?;
                    self.unknown_extensions += 1;
                }
                // A core tag below the extension range that this build does
                // not know can only come from a version whose layout differs,
                // and `stream_log`'s version check has already refused those.
                // Reaching here means a damaged file, so stop.
                _ => return None,
            }

            self.pos += r.consumed();
        }
    }
}

/// Streams rather than collecting into a `Vec<LoggedEvent>`: replay only walks
/// forward. Returns the concrete [`EventStream`] so a caller can ask for
/// [`EventStream::unknown_extensions`] afterwards.
pub fn stream_log(path: &Path) -> io::Result<EventStream> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;

    if bytes.get(0..4) != Some(&MAGIC[..]) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: not an event log (bad magic)", path.display())));
    }
    match bytes.get(4) {
        Some(&v) if (MIN_SUPPORTED_VERSION..=CURRENT_VERSION).contains(&v) => {}
        Some(&v) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: unsupported event log version {v} (this build understands                      versions {MIN_SUPPORTED_VERSION} through {CURRENT_VERSION})",
                    path.display()
                ),
            ));
        }
        None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{}: truncated event log", path.display()))),
    }

    Ok(EventStream {
        bytes,
        pos: 5,
        names: Vec::new(),
        surfaces: Vec::new(),
        current_tick: None,
        pending_remove_name: None,
        unknown_extensions: 0,
        undefined_references: 0,
    })
}

/// One segment file, plus the half-open tick range replay should take from it.
///
/// `end_tick` is what makes reloading an earlier save replayable: the mod
/// starts a fresh segment when play resumes at a tick it has recorded past,
/// but cannot delete or truncate the abandoned one, the Lua sandbox offering
/// only `write_file` and `remove_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub path: PathBuf,
    /// The tick in the filename: where capture began writing this file.
    pub start_tick: u64,
    /// Exclusive. Events at or past this tick were superseded by a later
    /// reload. `u64::MAX` for a segment nothing superseded, which in a
    /// capture that never reloaded is the only segment there is.
    pub end_tick: u64,
}

/// Every segment in `dir`, in the order play happened, each bounded at the tick
/// a later reload superseded it. `dir` is one playthrough's session folder.
///
/// Ordered by mtime rather than the tick in the filename, start tick not being
/// chronological once a playthrough reloads more than once: a segment is
/// appended to for exactly as long as it is live.
///
/// Each segment ends where the next created one begins, so `end_tick` is the
/// smallest start tick among all later segments, computed as a suffix minimum
/// so a second reload reaching further back also invalidates the first's. That
/// bound is also what keeps the result in ascending tick order.
///
/// Equal mtimes fall back to start tick, then rollover sequence, degrading to
/// stitching in tick order with overlaps trimmed.
pub fn log_segments(dir: &Path) -> io::Result<Vec<Segment>> {
    let mut found: Vec<(SystemTime, u64, u32, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let (start_tick, seq) = segment_tick(&path)?;
            // A segment whose mtime cannot be read sorts oldest, which puts
            // it before everything readable rather than silently last.
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            Some((modified, start_tick, seq, path))
        })
        .collect();
    found.sort();

    let mut segments: Vec<Segment> =
        found.into_iter().map(|(_, start_tick, _, path)| Segment { path, start_tick, end_tick: u64::MAX }).collect();

    let mut superseded_at = u64::MAX;
    for segment in segments.iter_mut().rev() {
        segment.end_tick = superseded_at;
        superseded_at = superseded_at.min(segment.start_tick);
    }

    Ok(segments)
}

/// The exclusive tick bound for each append run inside one segment, given the
/// bound the segment already carries.
///
/// A run is a stretch of records whose ticks do not go backwards. Every capture
/// the current mod writes holds one; older ones can hold two attempts at the
/// same stretch, separated only by the ticks jumping backwards.
///
/// Same problem as bounding segments one level down, and the same answer:
/// records are in append order, so a run ends at the smallest start tick among
/// those after it. Streams the file a second time rather than buffering, a tick
/// per run being bounded by reloads where decoded events are bounded by
/// how much was built.
pub fn segment_run_bounds(path: &Path, segment_end: u64) -> io::Result<Vec<u64>> {
    let mut starts: Vec<u64> = Vec::new();
    let mut previous: Option<u64> = None;
    for logged in stream_log(path)? {
        if previous.is_none_or(|p| logged.tick < p) {
            starts.push(logged.tick);
        }
        previous = Some(logged.tick);
    }

    let mut bounds = vec![segment_end; starts.len()];
    let mut superseded_at = segment_end;
    for (index, start) in starts.iter().enumerate().rev() {
        bounds[index] = superseded_at;
        superseded_at = superseded_at.min(*start);
    }
    Ok(bounds)
}

/// Parses `events_<tick>.stev` or `events_<tick>_<seq>.stev` into its start
/// tick and rollover sequence. `None` for anything else, so a stray file is
/// ignored rather than crashing discovery.
///
/// `seq` exists because reloading the same save twice resumes at the same tick
/// both times. Only a tiebreak, mtime staying the primary key since it also
/// orders segments written before `seq` existed.
fn segment_tick(path: &Path) -> Option<(u64, u32)> {
    if path.extension().and_then(|e| e.to_str()) != Some("stev") {
        return None;
    }
    let rest = path.file_stem()?.to_str()?.strip_prefix("events_")?;
    match rest.split_once('_') {
        Some((tick, seq)) => Some((tick.parse().ok()?, seq.parse().ok()?)),
        None => Some((rest.parse().ok()?, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::set_mtime_rank;
    use crate::wire::ByteWriter;

    /// Builds one segment's bytes with the given records, magic and version
    /// included, so tests read close to the format description above
    /// instead of hiding the byte layout behind a builder.
    fn segment(records: impl FnOnce(&mut ByteWriter)) -> Vec<u8> {
        let mut w = ByteWriter::new();
        w.magic(MAGIC).u8(CURRENT_VERSION);
        records(&mut w);
        w.into_vec()
    }

    fn write_to(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn an_entity_add_with_every_field_decodes() {
        let bytes = segment(|w| {
            w.u8(2).u64(42); // SetTick
            w.u8(1).string("nauvis"); // DefineSurface
            w.u8(0).string("assembling-machine-1"); // DefineName
            w.u8(3).u16(0).i32(-805).i32(285).u8(4).u8(3).u8(3).u64(1234).u16(0);
            // AddEntity
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tick, 42);
        assert_eq!(events[0].surface, "nauvis");
        assert_eq!(
            events[0].event,
            Event::AddEntity { name: "assembling-machine-1".to_string(), x: -80.5, y: 28.5, d: 4, w: 3, h: 3, id: Some(1234) }
        );
    }

    #[test]
    fn an_id_of_zero_decodes_as_no_id() {
        let bytes = segment(|w| {
            w.u8(2).u64(1);
            w.u8(1).string("nauvis");
            w.u8(0).string("transport-belt");
            w.u8(3).u16(0).i32(15).i32(25).u8(0).u8(1).u8(1).u64(0).u16(0);
        });
        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        match stream_log(&path).unwrap().next().unwrap().event {
            Event::AddEntity { id, .. } => assert_eq!(id, None),
            other => panic!("expected an add, got {other:?}"),
        }
    }

    #[test]
    fn a_removal_carries_position_and_optionally_an_id() {
        let bytes = segment(|w| {
            w.u8(2).u64(7);
            w.u8(1).string("vulcanus");
            w.u8(4).i32(-35).i32(45).u64(99).u16(0);
        });
        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let logged = stream_log(&path).unwrap().next().unwrap();

        assert_eq!(logged.surface, "vulcanus");
        assert_eq!(logged.event, Event::RemoveEntity { id: Some(99), pos: (-3.5, 4.5), name: None });
    }

    #[test]
    fn tile_events_use_integer_coordinates() {
        let bytes = segment(|w| {
            w.u8(2).u64(3);
            w.u8(1).string("nauvis");
            w.u8(0).string("concrete");
            w.u8(5).u16(0).i32(-5).i32(12).u16(0); // AddTile
            w.u8(2).u64(4);
            w.u8(6).i32(-5).i32(12).u16(0); // RemoveTile
        });
        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events[0].event, Event::AddTile { name: "concrete".to_string(), x: -5, y: 12 });
        assert_eq!(events[1].event, Event::RemoveTile { x: -5, y: 12 });
    }

    /// A log's last record can be a partial write if the game was killed
    /// mid flush. It should end the stream rather than fail it, and whatever
    /// came before it must still come through.
    #[test]
    fn a_truncated_final_record_ends_the_stream_without_losing_earlier_ones() {
        let full = segment(|w| {
            w.u8(2).u64(100);
            w.u8(1).string("nauvis");
            w.u8(0).string("pipe");
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0);
            w.u8(3).u16(0).i32(15).i32(15).u8(0).u8(1).u8(1).u64(2).u16(0);
        });
        let cut = &full[..full.len() - 4]; // slices into the second AddEntity

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", cut);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events.len(), 1, "only the complete record survives");
        assert_eq!(events[0].event, Event::AddEntity { name: "pipe".to_string(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1, id: Some(1) });
    }

    /// A plain quit-and-reload on a segment that stays open across it: the
    /// writer's dictionaries reset while the file is appended to, so its next
    /// new name gets id 0 again while this reader is already past 0.
    #[test]
    fn a_dictionary_reset_mid_segment_does_not_mislabel_later_records() {
        let bytes = segment(|w| {
            // Before the reload.
            w.u8(1).string("nauvis"); // DefineSurface -> writer id 0
            w.u8(0).string("iron-chest"); // DefineName -> writer id 0
            w.u8(2).u64(100); // SetTick
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0); // AddEntity id 0

            // Reload. The writer's dictionaries are empty again, so the next
            // new name and surface are handed id 0 a second time; the reset
            // record is what tells this reader to start counting from 0 too.
            w.u8(7); // ResetDictionaries
            w.u8(1).string("nauvis"); // DefineSurface -> writer id 0 again
            w.u8(0).string("transport-belt"); // DefineName -> writer id 0 again
            w.u8(2).u64(200); // SetTick
            w.u8(3).u16(0).i32(15).i32(5).u8(0).u8(1).u8(1).u64(2).u16(0); // AddEntity id 0
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events.len(), 2);
        match &events[1].event {
            Event::AddEntity { name, .. } => {
                assert_eq!(name, "transport-belt", "the post-reload record must decode as what the writer meant")
            }
            other => panic!("expected an add, got {other:?}"),
        }
    }

    /// Captures written before the reset record exist and must keep working:
    /// version 1 is still a supported read, just without the fix.
    #[test]
    fn a_version_1_segment_still_reads() {
        let mut bytes = segment(|w| {
            w.u8(2).u64(7);
            w.u8(1).string("nauvis");
            w.u8(0).string("pipe");
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0);
        });
        bytes[4] = 1; // the version byte, right after the 4 byte magic

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0].event, Event::AddEntity { name, .. } if name == "pipe"));
    }

    /// Before this record shape, an unrecognised tag returned `None`, which an
    /// iterator reports as the end of the stream, so a newer capture replayed
    /// as a timelapse that stopped partway.
    #[test]
    fn an_unknown_extension_record_is_skipped_and_the_stream_continues() {
        let bytes = segment(|w| {
            w.u8(2).u64(10); // SetTick
            w.u8(1).string("nauvis"); // DefineSurface
            w.u8(0).string("pipe"); // DefineName
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0); // AddEntity
                                                                          // Something a later mod version writes and this build has never
                                                                          // heard of, sitting between two records it does understand.
            w.u8(TAG_EXTENSION_MIN + 40).varint(4).u32(0xDEADBEEF);
            w.u8(3).u16(0).i32(15).i32(5).u8(0).u8(1).u8(1).u64(2).u16(0); // AddEntity
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let mut stream = stream_log(&path).unwrap();
        let events: Vec<LoggedEvent> = (&mut stream).collect();

        assert_eq!(events.len(), 2, "the record after the extension must still arrive");
        assert_eq!(events[1].event, Event::AddEntity { name: "pipe".to_string(), x: 1.5, y: 0.5, d: 0, w: 1, h: 1, id: Some(2) });
        assert_eq!(stream.unknown_extensions(), 1);
    }

    /// Records that name a dictionary entry their segment never defined are
    /// lost, and used to be lost in silence. A capture reset left the mod's
    /// buffer holding records encoded against the deleted segment's
    /// dictionary; they were flushed into a fresh segment defining none of
    /// those names, and a whole playthrough recorded nothing while looking
    /// like it had simply not been played.
    #[test]
    fn records_naming_something_the_segment_never_defined_are_counted() {
        let bytes = segment(|w| {
            w.u8(2).u64(10); // SetTick
                             // No DefineSurface and no DefineName, exactly as the reset bug wrote.
            w.u8(4).i32(15).i32(25).u64(7).u16(0); // RemoveEntity
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0); // AddEntity
            w.u8(5).u16(0).i32(1).i32(2).u16(0); // AddTile
            w.u8(6).i32(1).i32(2).u16(0); // RemoveTile
                                          // Then a properly defined pair, which must still come through.
            w.u8(1).string("nauvis");
            w.u8(0).string("pipe");
            w.u8(3).u16(0).i32(35).i32(45).u8(0).u8(1).u8(1).u64(2).u16(0);
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let mut stream = stream_log(&path).unwrap();
        let events: Vec<LoggedEvent> = (&mut stream).collect();

        assert_eq!(events.len(), 1, "only the record whose names were defined survives");
        assert_eq!(stream.undefined_references(), 4, "and the four that did not are counted, not silent");
        assert_eq!(stream.unknown_extensions(), 0, "this is damage, not a newer mod");
    }

    /// A removal that says which of two things at one position was mined. The
    /// annotation is its own record, so a tool older than it steps over it and
    /// resolves the removal the way it always did.
    #[test]
    fn a_removal_can_name_what_it_is_for() {
        let bytes = segment(|w| {
            w.u8(2).u64(10); // SetTick
            w.u8(1).string("nauvis"); // DefineSurface
            w.u8(0).string("iron-ore"); // DefineName
            w.u8(TAG_REMOVE_NAME).varint(1).varint(0); // RemoveName: iron-ore
            w.u8(4).i32(15).i32(25).u64(0).u16(0); // RemoveEntity
            w.u8(4).i32(35).i32(45).u64(7).u16(0); // RemoveEntity, unannotated
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let mut stream = stream_log(&path).unwrap();
        let events: Vec<LoggedEvent> = (&mut stream).collect();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, Event::RemoveEntity { id: None, pos: (1.5, 2.5), name: Some("iron-ore".to_string()) });
        assert_eq!(
            events[1].event,
            Event::RemoveEntity { id: Some(7), pos: (3.5, 4.5), name: None },
            "the annotation applies to one removal only"
        );
        assert_eq!(stream.unknown_extensions(), 0, "a record this build understands is not an unknown one");
    }

    /// A name written but never used, because the game was killed between the
    /// two records or an add landed in between, must not attach itself to a
    /// later removal.
    #[test]
    fn a_stale_removal_name_does_not_carry_over() {
        let bytes = segment(|w| {
            w.u8(2).u64(10); // SetTick
            w.u8(1).string("nauvis"); // DefineSurface
            w.u8(0).string("iron-ore"); // DefineName
            w.u8(TAG_REMOVE_NAME).varint(1).varint(0); // RemoveName, then no removal
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0); // AddEntity
            w.u8(4).i32(35).i32(45).u64(7).u16(0); // RemoveEntity
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events.len(), 2);
        assert_eq!(events[1].event, Event::RemoveEntity { id: Some(7), pos: (3.5, 4.5), name: None });
    }

    /// A segment is append only and can be cut off mid record by the game
    /// being killed, which every other record type treats as a clean end of
    /// stream rather than an error. An extension whose length runs past the
    /// end is the same situation and gets the same treatment.
    #[test]
    fn an_extension_running_past_the_end_ends_the_stream_like_any_partial_write() {
        let bytes = segment(|w| {
            w.u8(2).u64(10);
            w.u8(1).string("nauvis");
            w.u8(0).string("pipe");
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0);
            w.u8(TAG_EXTENSION_MIN).varint(500); // claims far more than follows
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events.len(), 1, "the complete record before it still survives");
    }

    #[test]
    fn a_file_with_the_wrong_magic_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", b"nope");
        assert!(stream_log(&path).is_err());
    }

    #[test]
    fn a_wrong_version_is_a_distinct_error_from_a_parse_failure() {
        let mut bytes = segment(|w| {
            w.u8(2).u64(1);
        });
        bytes[4] = 99; // the version byte, right after the 4 byte magic

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        // Not unwrap_err(): the Ok type is `impl Iterator`, which doesn't
        // implement Debug. err() discards it, needing only the Err side to.
        let err = stream_log(&path).err().unwrap();
        assert!(err.to_string().contains("version 99"), "got: {err}");
    }

    fn segment_names(dir: &Path) -> Vec<String> {
        log_segments(dir).unwrap().iter().map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    /// With no reload in the picture, mtime order and tick order agree, and
    /// the tick must be read as a number: `events_9000` would sort after
    /// `events_10000` lexicographically.
    #[test]
    fn segments_order_by_tick_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        for (rank, name) in ["events_100.stev", "events_9000.stev", "events_10000.stev"].iter().enumerate() {
            std::fs::write(dir.path().join(name), MAGIC).unwrap();
            set_mtime_rank(dir.path(), name, rank as u64);
        }
        // Files that are not segments must not be picked up.
        std::fs::write(dir.path().join("frame_1_nauvis.stfr"), "").unwrap();
        std::fs::write(dir.path().join("baseline.json"), "{}").unwrap();

        assert_eq!(segment_names(dir.path()), vec!["events_100.stev", "events_9000.stev", "events_10000.stev"]);
    }

    /// Two reloads, the second reaching further back than the first. Start
    /// tick alone cannot order these: the segment created last has the
    /// middle tick of the three.
    #[test]
    fn segments_order_by_creation_not_start_tick_when_reloads_chain() {
        let dir = tempfile::tempdir().unwrap();
        // Played from 0, reloaded back to 1000, then reloaded back to 500.
        for (rank, name) in ["events_0.stev", "events_1000.stev", "events_500.stev"].iter().enumerate() {
            std::fs::write(dir.path().join(name), MAGIC).unwrap();
            set_mtime_rank(dir.path(), name, rank as u64);
        }

        assert_eq!(segment_names(dir.path()), vec!["events_0.stev", "events_1000.stev", "events_500.stev"]);
    }

    /// A segment ends where the earliest later-created one begins, so the
    /// second reload above also invalidates what the first reload's segment
    /// recorded past tick 500, not just what the original segment did.
    #[test]
    fn each_segment_ends_at_the_earliest_start_tick_created_after_it() {
        let dir = tempfile::tempdir().unwrap();
        for (rank, name) in ["events_0.stev", "events_1000.stev", "events_500.stev"].iter().enumerate() {
            std::fs::write(dir.path().join(name), MAGIC).unwrap();
            set_mtime_rank(dir.path(), name, rank as u64);
        }

        let bounds: Vec<(u64, u64)> = log_segments(dir.path()).unwrap().iter().map(|s| (s.start_tick, s.end_tick)).collect();
        assert_eq!(bounds, vec![(0, 500), (1000, 500), (500, u64::MAX)]);
    }

    /// The name the mod gives a segment when a reload resumed at the tick the
    /// live segment already started at, which the plain `events_<tick>.stev`
    /// form cannot distinguish from the segment already there.
    #[test]
    fn a_sequence_suffixed_segment_parses_and_keeps_its_start_tick() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("events_20000.stev"), MAGIC).unwrap();
        std::fs::write(dir.path().join("events_20000_1.stev"), MAGIC).unwrap();
        set_mtime_rank(dir.path(), "events_20000.stev", 0);
        set_mtime_rank(dir.path(), "events_20000_1.stev", 1);

        let segments = log_segments(dir.path()).unwrap();
        assert_eq!(
            segments.iter().map(|s| (s.start_tick, s.end_tick)).collect::<Vec<_>>(),
            vec![(20000, 20000), (20000, u64::MAX)],
            "the first attempt is superseded from the tick the second one restarts at"
        );
    }

    /// A `.stev` whose name is neither form must be ignored rather than
    /// parsed into a bogus tick.
    #[test]
    fn a_segment_name_that_is_not_a_tick_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["events_.stev", "events_abc.stev", "events_100_x.stev", "notevents_100.stev"] {
            std::fs::write(dir.path().join(name), MAGIC).unwrap();
        }
        assert!(log_segments(dir.path()).unwrap().is_empty());
    }

    /// The ordinary case: one segment, never reloaded, so nothing bounds it.
    #[test]
    fn a_lone_segment_is_unbounded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("events_100.stev"), MAGIC).unwrap();

        let segments = log_segments(dir.path()).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_tick, 100);
        assert_eq!(segments[0].end_tick, u64::MAX);
    }

    /// The degenerate case (a copied capture folder whose mtimes all
    /// flattened to one value): ordering falls back to ascending start tick,
    /// which still trims the overlaps rather than replaying them twice.
    #[test]
    fn segments_sharing_an_mtime_fall_back_to_start_tick_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["events_5000.stev", "events_100.stev", "events_900.stev"] {
            std::fs::write(dir.path().join(name), MAGIC).unwrap();
            set_mtime_rank(dir.path(), name, 0);
        }

        let bounds: Vec<(u64, u64)> = log_segments(dir.path()).unwrap().iter().map(|s| (s.start_tick, s.end_tick)).collect();
        assert_eq!(bounds, vec![(100, 900), (900, 5000), (5000, u64::MAX)]);
    }
}
