//! Reading the live capture event log written by mod/control.lua.
//!
//! Wire format (`events_<start_tick>.stev`), all integers little endian.
//! Every playthrough's segments live in their own subfolder, named after
//! its session id (the map's terrain seed; see mod/control.lua's
//! `compute_session_id`), since `script-output/save-timelapse/` is shared by
//! every save that ever turns capture on and `game.tick` restarts from 0 for
//! each one, so a raw tick alone cannot tell two playthroughs' segments
//! apart, but two playthroughs no longer share a directory to get confused
//! in to begin with:
//!
//! ```text
//! magic   4 bytes, "STE1", written once when the segment is created
//! version u8, MIN_SUPPORTED_VERSION through CURRENT_VERSION; written right
//!         after the magic
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
//!   tag >=128 Extension    varint len, then that many bytes
//! ```
//!
//! `DefineName`/`DefineSurface` give the next sequential id to a not yet seen
//! string, so later records only spend two bytes on what would otherwise be
//! a repeated name. `SetTick` is emitted once per distinct tick that has at
//! least one event rather than on every record, since many events (a
//! blueprint landing hundreds of entities) usually share a tick.
//! `control.lua` always writes a `SetTick` as the very first record of a
//! fresh segment, right after the magic, so a data record is never expected
//! before the first `SetTick`.
//!
//! `id` on `AddEntity`/`RemoveEntity` uses 0 to mean "no id": Factorio's
//! `unit_number` starts at 1, and `control.lua` already tolerates a missing
//! one (some entity kinds have none).
//!
//! The log is append only and flushed periodically, so its last record can
//! be a partial write if the game was killed. A record that does not fit in
//! the remaining bytes ends the stream instead of failing it, the same
//! tolerance the JSON line format used to give a truncated last line.
//!
//! Unlike the frame format (see `frame.rs`), a segment has no trailing
//! checksum: it grows for as long as capture stays on and is simply
//! abandoned, not closed, when a reset starts a new one, so there is no
//! "this segment is finished" moment to checksum against. The version byte
//! still lets a reader tell "this is a format I don't understand" apart
//! from a generic parse failure, same as the frame format.
//!
//! # The extension contract
//!
//! Version 3 is where this format stops changing shape. Anything added after
//! it is an extension record: a tag of 128 or above, a varint byte length,
//! then that many bytes. A reader that does not know the tag skips exactly
//! that many bytes and keeps going, so a segment written by a newer mod still
//! replays in an older tool, minus whatever the new record described.
//!
//! Skipping matters more here than the symmetry with `frame.rs` suggests.
//! Factorio updates mods from the portal by itself and the desktop tool does
//! not update itself, so the mod being ahead of the tool is the normal state
//! of anyone who installed once and kept playing. Without a skippable record
//! that combination has only ever had two outcomes, both bad: a hard refusal
//! of the whole segment, or, for an unknown tag inside an accepted version,
//! `next` returning `None` and the replay simply stopping early with no
//! indication it had. `EventStream::unknown_extensions` reports what was
//! stepped over so the difference is visible rather than silent.
//!
//! Core tags stay below 128 so the two kinds never collide.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::wire::ByteReader;

const MAGIC: &[u8; 4] = b"STE1";
/// Versions this reader understands. Version 1 predates the
/// dictionary-reset record (tag 7) and is still accepted, since captures in
/// that format exist and are readable; they simply carry the mislabeling bug
/// that record was added to fix, which nothing on this side can undo.
///
/// Version 3 writes exactly the same records as version 2. It exists only to
/// declare "this file may contain extension records", so a build predating
/// them refuses it with a clear message rather than stopping dead at the
/// first one. See the extension contract in this module's header: from
/// version 3 onward additions go in extension records, so this is meant to be
/// the last time this constant moves.
const CURRENT_VERSION: u8 = 3;
const MIN_SUPPORTED_VERSION: u8 = 1;
/// Tags from here up carry their own length and may be skipped. Core record
/// tags stay below it so the two can never collide as the format grows.
const TAG_EXTENSION_MIN: u8 = 128;

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
    /// Removal of an entity. `id` (`unit_number`) is a fast, unambiguous
    /// match when replay's world state has it registered: true for anything
    /// built after capture began, since its `AddEntity` carried the same id.
    /// `pos` is what actually resolves an entity that already existed when
    /// the baseline was taken, since a snapshot records no ids. Both are
    /// always present in this format (see `control.lua::log_entity`, which
    /// always has an entity's position and passes its `unit_number` whether
    /// or not the entity has one).
    RemoveEntity { id: Option<u64>, pos: (f32, f32) },
    AddTile { name: String, x: i32, y: i32 },
    RemoveTile { x: i32, y: i32 },
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
    unknown_extensions: usize,
}

impl EventStream {
    /// Extension records stepped over so far because this build does not
    /// recognise their tag, which only means the capture was written by a
    /// newer mod than the tool reading it.
    ///
    /// Only meaningful once the stream has been walked, since it counts what
    /// iteration actually passed over.
    pub fn unknown_extensions(&self) -> usize {
        self.unknown_extensions
    }
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
                // Tag 7, ResetDictionaries. Written when the mod resumes a
                // segment across a save load, at which point Factorio has
                // re-run the mod and emptied the writer's dictionaries while
                // this file kept every name defined before that point. Ids
                // after it start from 0 again on both sides.
                //
                // Version 1 has no such record, so a version 1 file that was
                // ever resumed silently mislabels everything logged after the
                // resume. That cannot be repaired from this side: the record
                // that would have said where it happened is exactly what is
                // missing.
                7 => {
                    self.names.clear();
                    self.surfaces.clear();
                }
                3 => {
                    let name_id = r.u16()? as usize;
                    let x = r.i32()?;
                    let y = r.i32()?;
                    let d = r.u8()?;
                    let w = r.u8()?;
                    let h = r.u8()?;
                    let id = r.u64()?;
                    let surface_id = r.u16()? as usize;
                    let event = match (self.current_tick, self.names.get(name_id), self.surfaces.get(surface_id)) {
                        (Some(tick), Some(name), Some(surface)) => Some(LoggedEvent {
                            tick,
                            surface: surface.clone(),
                            event: Event::AddEntity {
                                name: name.clone(),
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
                    let event = match (self.current_tick, self.surfaces.get(surface_id)) {
                        (Some(tick), Some(surface)) => Some(LoggedEvent {
                            tick,
                            surface: surface.clone(),
                            event: Event::RemoveEntity {
                                id: (id != 0).then_some(id),
                                pos: (x as f32 / 10.0, y as f32 / 10.0),
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
                    let event = match (self.current_tick, self.names.get(name_id), self.surfaces.get(surface_id)) {
                        (Some(tick), Some(name), Some(surface)) => Some(LoggedEvent {
                            tick,
                            surface: surface.clone(),
                            event: Event::AddTile { name: name.clone(), x, y },
                        }),
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
                    let event = match (self.current_tick, self.surfaces.get(surface_id)) {
                        (Some(tick), Some(surface)) => {
                            Some(LoggedEvent { tick, surface: surface.clone(), event: Event::RemoveTile { x, y } })
                        }
                        _ => None,
                    };
                    self.pos += r.consumed();
                    if let Some(logged) = event {
                        return Some(logged);
                    }
                    continue;
                }
                // An extension record: a tag this build has no meaning for,
                // carrying its own length precisely so it can be stepped over
                // rather than ending the stream. See the extension contract in
                // this module's header.
                //
                // A length running past the end of the file falls out as
                // `None` from `skip`, which ends the stream, the same
                // treatment every other record gets when the game was killed
                // mid write.
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

/// Stream a log rather than reading it whole into a `Vec<LoggedEvent>`: it
/// still has to be read into memory once (unlike the old line by line
/// reader), but replay only ever walks forward through it, so there is no
/// reason to also materialise every decoded event up front.
///
/// Returns the concrete [`EventStream`] rather than `impl Iterator` so a
/// caller that wants [`EventStream::unknown_extensions`] afterwards can ask
/// for it; iterate through `&mut` to keep the stream alive that long.
pub fn stream_log(path: &Path) -> io::Result<EventStream> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;

    if bytes.get(0..4) != Some(&MAGIC[..]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: not an event log (bad magic)", path.display()),
        ));
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
        None => {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{}: truncated event log", path.display())))
        }
    }

    Ok(EventStream {
        bytes,
        pos: 5,
        names: Vec::new(),
        surfaces: Vec::new(),
        current_tick: None,
        unknown_extensions: 0,
    })
}

/// One segment file, plus the half-open tick range replay should actually
/// take from it.
///
/// `end_tick` is what makes reloading an earlier save replayable. The mod
/// starts a fresh segment whenever play resumes at a tick it has already
/// recorded past, but it
/// cannot delete or truncate the segment it just abandoned: Factorio's Lua
/// sandbox offers `write_file` and `remove_path` and nothing finer, and
/// removing the file outright would also throw away the part of it that is
/// still real history. So the abandoned file keeps its records for a future
/// the player reloaded away from, and bounding it is this side's job.
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

/// Every `events_<tick>.stev` segment in `dir`, in the order play actually
/// happened, each bounded at the tick a later reload superseded it.
///
/// `dir` is expected to already be one playthrough's own session folder (see
/// `replay::discover_sessions`); there is no session to filter by here
/// anymore, since two playthroughs can no longer share a directory to get
/// confused in.
///
/// Ordered by mtime rather than by the tick in the filename, because start
/// tick is not chronological once a playthrough reloads more than once.
/// Reload from tick 5000 back to 3000, play to 8000, then reload again back
/// to 1000, and the segments were created in the order 0, 3000, 1000: sorting
/// by tick would replay the second reload's log before the first reload's,
/// which is backwards. mtime recovers the real order, since a segment is
/// appended to for exactly as long as it is the live one and never touched
/// again after a rollover abandons it, so segments finish being written in
/// the same order they were created.
///
/// Given that order, each segment ends where the next one to be created
/// begins: a reload rewinds the world to the tick the new segment starts at,
/// so every record at or past that tick, in every earlier segment, describes
/// a timeline that no longer happened. `end_tick` is therefore the smallest
/// start tick among all segments created later, computed as a suffix minimum
/// below (the smallest, not simply the next one's, so a second reload
/// reaching further back also invalidates what the first reload's segment
/// recorded past that point).
///
/// That bound is also what keeps the returned segments in ascending tick
/// order despite being sorted by mtime: a segment's events all fall below its
/// `end_tick`, which is at most the following segment's `start_tick`, so no
/// segment can contribute an event later than the next one's first.
///
/// Two segments with the same mtime fall back to ascending start tick, then
/// to the rollover sequence in the filename (see `segment_tick`). That is the
/// degenerate case (a copied capture folder whose timestamps were all
/// flattened to the copy time, or a filesystem too coarse to separate two
/// rollovers), and it degrades to stitching the segments together in tick
/// order with overlaps trimmed, which is still no worse than replaying the
/// overlaps twice.
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

    let mut segments: Vec<Segment> = found
        .into_iter()
        .map(|(_, start_tick, _, path)| Segment { path, start_tick, end_tick: u64::MAX })
        .collect();

    let mut superseded_at = u64::MAX;
    for segment in segments.iter_mut().rev() {
        segment.end_tick = superseded_at;
        superseded_at = superseded_at.min(segment.start_tick);
    }

    Ok(segments)
}

/// The exclusive tick bound for each *append run* inside one segment file,
/// given the bound the segment as a whole already carries.
///
/// A run is a stretch of records whose ticks do not go backwards. A single
/// segment normally holds exactly one, and every capture the current mod
/// writes always does, since a rollback now always rolls over to a new file
/// Reloads land inside a segment rather than starting a new one, since the
/// mod cannot detect a reload at all (see `mod/encode.lua`). Captures
/// fix can hold more than one: reloading the same save twice in a row resumed
/// at exactly the tick the live segment had started at, which the old
/// rollover check read as "no rollback", so the second attempt was appended
/// straight onto the first attempt's records. Nothing separates the two
/// attempts except that the ticks jump backwards where the second begins.
///
/// Bounding runs is the same problem as bounding segments, one level down,
/// and gets the same answer: records within a file are in append order, which
/// is real chronological order, so a run ends at the smallest start tick
/// among the runs appended after it (and at the segment's own bound, which is
/// where the suffix minimum below starts from). A capture with one run per
/// segment gets a single bound equal to the segment's own, which is why this
/// costs nothing but the scan for everything the fixed mod writes.
///
/// Streams the file a second time rather than buffering the first pass: a
/// bound cannot be known until every later run's start tick has been seen, so
/// something has to be held until the end, and holding a tick per run is
/// bounded by how many times a save was reloaded, where holding decoded
/// events is bounded by how much was built.
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
/// tick and rollover sequence, `seq` defaulting to 0 for the plain form.
/// `None` for anything else (a differently-named file sharing this session's
/// folder, or the wrong extension), so a stray file is silently ignored
/// rather than crashing discovery.
///
/// `seq` exists because a start tick alone does not name a segment uniquely:
/// reloading the same save twice in a row resumes at the same tick both
/// times, and without it the mod would append the second attempt into the
/// first attempt's file (see `mod/encode.lua`'s `capture_segment_basename`).
/// It is only ever a tiebreak for ordering, never the primary key: mtime
/// stays that, since it is the one signal that also orders segments written
/// before `seq` existed, and segments left behind by a reset whose file
/// deletion failed (which restarts `seq` from 0 while older files keep
/// theirs).
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
            w.u8(3).u16(0).i32(-805).i32(285).u8(4).u8(3).u8(3).u64(1234).u16(0); // AddEntity
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tick, 42);
        assert_eq!(events[0].surface, "nauvis");
        assert_eq!(
            events[0].event,
            Event::AddEntity {
                name: "assembling-machine-1".to_string(),
                x: -80.5,
                y: 28.5,
                d: 4,
                w: 3,
                h: 3,
                id: Some(1234),
            }
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
        assert_eq!(logged.event, Event::RemoveEntity { id: Some(99), pos: (-3.5, 4.5) });
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
        assert_eq!(events[0].event, Event::AddEntity {
            name: "pipe".to_string(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1, id: Some(1),
        });
    }

    /// What a plain quit-and-reload does to a segment that stays open across
    /// it.
    ///
    /// Factorio re-runs `capture.lua`'s top level on every load, which resets
    /// the writer's name and surface dictionaries to empty, but the segment
    /// file is appended to rather than restarted. The writer's next new name
    /// therefore gets id 0 again, while this reader has been assigning ids by
    /// encounter order and is already past 0. Every record written after the
    /// reload then names the wrong prototype.
    ///
    /// `docs/ARCHITECTURE.md` claims this is harmless on the grounds that a
    /// name defined twice "just gets two ids that both resolve to the same
    /// string". That holds only if the writer keeps counting; once it resets,
    /// the two sides disagree about what id 0 means.
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
            Event::AddEntity { name, .. } => assert_eq!(
                name, "transport-belt",
                "the post-reload record must decode as what the writer meant"
            ),
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

    /// The failure this record shape exists to prevent. Before it, an
    /// unrecognised tag returned `None`, which an iterator reports as "the
    /// stream ended", so a segment written by a newer mod replayed as a
    /// timelapse that simply stopped partway with nothing to say about why.
    /// Now the events after it still arrive.
    #[test]
    fn an_unknown_extension_record_is_skipped_and_the_stream_continues() {
        let bytes = segment(|w| {
            w.u8(2).u64(10); // SetTick
            w.u8(1).string("nauvis"); // DefineSurface
            w.u8(0).string("pipe"); // DefineName
            w.u8(3).u16(0).i32(5).i32(5).u8(0).u8(1).u8(1).u64(1).u16(0); // AddEntity
            // Something a later mod version writes and this build has never
            // heard of, sitting between two records it does understand.
            w.u8(TAG_EXTENSION_MIN).varint(4).u32(0xDEADBEEF);
            w.u8(3).u16(0).i32(15).i32(5).u8(0).u8(1).u8(1).u64(2).u16(0); // AddEntity
        });

        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", &bytes);
        let mut stream = stream_log(&path).unwrap();
        let events: Vec<LoggedEvent> = (&mut stream).collect();

        assert_eq!(events.len(), 2, "the record after the extension must still arrive");
        assert_eq!(events[1].event, Event::AddEntity {
            name: "pipe".to_string(), x: 1.5, y: 0.5, d: 0, w: 1, h: 1, id: Some(2),
        });
        assert_eq!(stream.unknown_extensions(), 1);
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

    /// Stamps `name`'s mtime, which is how `log_segments` recovers the order
    /// segments were created in: higher `rank` means created later, and an
    /// equal `rank` means an exact tie.
    ///
    /// Anchored to a fixed instant rather than `SystemTime::now()` so a tie
    /// is genuinely a tie (two `now()` calls moments apart are not equal, and
    /// a test asking for one would quietly get an order instead) and so
    /// nothing here depends on the wall clock at all. Writing the files back
    /// to back and letting the filesystem timestamp them would not work
    /// either: Windows' clock granularity is coarser than the gap between two
    /// consecutive writes, so their order would be a coin flip.
    fn set_mtime_rank(dir: &Path, name: &str, rank: u64) {
        let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + rank);
        std::fs::OpenOptions::new().write(true).open(dir.join(name)).unwrap().set_modified(when).unwrap();
    }

    fn segment_names(dir: &Path) -> Vec<String> {
        log_segments(dir)
            .unwrap()
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
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

        assert_eq!(
            segment_names(dir.path()),
            vec!["events_100.stev", "events_9000.stev", "events_10000.stev"]
        );
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

        assert_eq!(
            segment_names(dir.path()),
            vec!["events_0.stev", "events_1000.stev", "events_500.stev"]
        );
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

        let bounds: Vec<(u64, u64)> =
            log_segments(dir.path()).unwrap().iter().map(|s| (s.start_tick, s.end_tick)).collect();
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

        let bounds: Vec<(u64, u64)> =
            log_segments(dir.path()).unwrap().iter().map(|s| (s.start_tick, s.end_tick)).collect();
        assert_eq!(bounds, vec![(100, 900), (900, 5000), (5000, u64::MAX)]);
    }
}
