//! Reading the live capture event log written by mod/control.lua.
//!
//! Wire format (`events_<session_id>_<start_tick>.stev`), all integers
//! little endian. `session_id` is a per-playthrough tag (the map's terrain
//! seed; see mod/control.lua's `compute_session_id`), needed because
//! `script-output/save-timelapse/` is one folder shared by every save that
//! ever turns capture on, and `game.tick` restarts from 0 for each one, so a
//! raw tick alone cannot tell two playthroughs' segments apart:
//!
//! ```text
//! magic 4 bytes, "STE1", written once when the segment is created
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

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::wire::ByteReader;

const MAGIC: &[u8; 4] = b"STE1";

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
struct EventStream {
    bytes: Vec<u8>,
    pos: usize,
    names: Vec<String>,
    surfaces: Vec<String>,
    current_tick: Option<u64>,
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
pub fn stream_log(path: &Path) -> io::Result<impl Iterator<Item = LoggedEvent>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;

    if bytes.get(0..4) != Some(&MAGIC[..]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: not an event log (bad magic)", path.display()),
        ));
    }

    Ok(EventStream { bytes, pos: 4, names: Vec::new(), surfaces: Vec::new(), current_tick: None })
}

/// Every `events_<session>_<tick>.stev` belonging to `session` in a capture
/// directory, ordered by start tick.
///
/// Ordered numerically, not lexicographically: segment names carry a raw
/// game tick with no zero padding, so `events_..._9000` would otherwise sort
/// after `events_..._10000`.
pub fn log_paths(dir: &Path, session: u32) -> io::Result<Vec<PathBuf>> {
    let mut segments: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter_map(|path| {
            let (found, tick) = segment_id_and_tick(&path)?;
            (found == session).then_some((tick, path))
        })
        .collect();
    segments.sort();
    Ok(segments.into_iter().map(|(_, path)| path).collect())
}

/// Parses the hex session tag and decimal tick out of
/// `events_<session>_<tick>.stev`. A bare `events_<tick>.stev` -- every
/// segment written before playthroughs were tagged -- has no session to
/// parse and returns `None`, which is what makes such a leftover file
/// invisible, and therefore harmless, rather than a "bad magic" crash.
fn segment_id_and_tick(path: &Path) -> Option<(u32, u64)> {
    if path.extension().and_then(|e| e.to_str()) != Some("stev") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.strip_prefix("events_")?;
    let (session_hex, tick_str) = stem.split_once('_')?;
    let session = u32::from_str_radix(session_hex, 16).ok()?;
    let tick = tick_str.parse().ok()?;
    Some((session, tick))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::ByteWriter;

    /// Builds one segment's bytes with the given records, magic included, so
    /// tests read close to the format description above instead of hiding
    /// the byte layout behind a builder.
    fn segment(records: impl FnOnce(&mut ByteWriter)) -> Vec<u8> {
        let mut w = ByteWriter::new();
        w.magic(MAGIC);
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

    #[test]
    fn a_file_with_the_wrong_magic_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_to(dir.path(), "events_0.stev", b"nope");
        assert!(stream_log(&path).is_err());
    }

    #[test]
    fn segments_order_by_tick_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["events_00000001_10000.stev", "events_00000001_9000.stev", "events_00000001_100.stev"] {
            std::fs::write(dir.path().join(name), MAGIC).unwrap();
        }
        // Files that are not segments must not be picked up.
        std::fs::write(dir.path().join("frame_1_nauvis.stfr"), "").unwrap();
        std::fs::write(dir.path().join("baseline_00000001.json"), "{}").unwrap();

        let names: Vec<String> = log_paths(dir.path(), 1)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["events_00000001_100.stev", "events_00000001_9000.stev", "events_00000001_10000.stev"]
        );
    }

    #[test]
    fn log_paths_only_returns_the_requested_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("events_00000001_100.stev"), MAGIC).unwrap();
        std::fs::write(dir.path().join("events_00000002_200.stev"), MAGIC).unwrap();

        let names: Vec<String> = log_paths(dir.path(), 2)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["events_00000002_200.stev"]);
    }

    /// A segment written before playthroughs were tagged has no session id
    /// in its name at all, so it must not be mistaken for a match against
    /// any particular session -- this is what makes such a leftover file
    /// inert rather than crashing replay (see the real bug this fixed:
    /// `events_22630200.stev` from before this feature choked replay of an
    /// otherwise unrelated, correctly tagged capture).
    #[test]
    fn an_untagged_legacy_segment_is_ignored_by_every_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("events_22630200.stev"), MAGIC).unwrap();
        std::fs::write(dir.path().join("events_00000001_100.stev"), MAGIC).unwrap();

        let names: Vec<String> = log_paths(dir.path(), 1)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["events_00000001_100.stev"]);
    }
}
