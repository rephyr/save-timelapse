//! Reading the live-capture event log written by mod/control.lua.
//!
//! One JSON object per line in `events_<start_tick>.jsonl`. The log is
//! append-only and is flushed periodically, so its last line can be a partial
//! write if the game was killed; a line that does not parse is skipped rather
//! than failing the whole log.
//!
//! See `mod/encode.lua::encode_event` for the writer.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The wire form. Every field past `t`/`op`/`k` is conditional on the event
/// shape, so they are all optional here and validated in [`Event::from_raw`].
#[derive(Debug, Deserialize)]
struct RawEvent {
    t: u64,
    op: String,
    k: String,
    #[serde(default)]
    s: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
    #[serde(default)]
    d: u8,
    #[serde(default)]
    w: Option<u32>,
    #[serde(default)]
    h: Option<u32>,
    #[serde(default)]
    id: Option<u64>,
}

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
    /// Removal keyed by `unit_number`, which is unique across the whole game,
    /// so it carries no surface and no position.
    RemoveEntityById { id: u64 },
    /// Removal of an entity with no `unit_number`, located by position.
    RemoveEntityAt { x: f32, y: f32 },
    AddTile { name: String, x: i32, y: i32 },
    RemoveTile { x: i32, y: i32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoggedEvent {
    pub tick: u64,
    /// Absent on `RemoveEntityById`, and on logs written before events
    /// carried a surface at all.
    pub surface: Option<String>,
    pub event: Event,
}

impl Event {
    fn from_raw(raw: RawEvent) -> Option<Event> {
        let adding = raw.op == "+";
        let entity = raw.k == "e";

        match (adding, entity) {
            (true, true) => Some(Event::AddEntity {
                name: raw.n?,
                x: raw.x? as f32,
                y: raw.y? as f32,
                d: raw.d,
                // Omitted at 1x1 by the encoder, same convention as frames.
                w: raw.w.unwrap_or(1).max(1),
                h: raw.h.unwrap_or(1).max(1),
                id: raw.id,
            }),
            (false, true) => match raw.id {
                Some(id) => Some(Event::RemoveEntityById { id }),
                None => Some(Event::RemoveEntityAt { x: raw.x? as f32, y: raw.y? as f32 }),
            },
            (true, false) => Some(Event::AddTile {
                name: raw.n?,
                x: raw.x? as i32,
                y: raw.y? as i32,
            }),
            (false, false) => Some(Event::RemoveTile { x: raw.x? as i32, y: raw.y? as i32 }),
        }
    }
}

/// Parse one log line. `None` covers both a malformed line and one whose
/// fields don't add up to any event shape -- neither is worth aborting a
/// whole replay over.
pub fn parse_line(line: &str) -> Option<LoggedEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let raw: RawEvent = serde_json::from_str(line).ok()?;
    let tick = raw.t;
    let surface = raw.s.clone();
    Event::from_raw(raw).map(|event| LoggedEvent { tick, surface, event })
}

/// Stream a log rather than reading it whole: these grow without bound over a
/// long playthrough, and replay only ever walks forward through them.
pub fn stream_log(path: &Path) -> io::Result<impl Iterator<Item = LoggedEvent>> {
    let reader = BufReader::new(File::open(path)?);
    Ok(reader.lines().map_while(Result::ok).filter_map(|line| parse_line(&line)))
}

/// Every `events_<tick>.jsonl` in a capture directory, ordered by start tick.
///
/// Ordered numerically, not lexicographically: segment names carry a raw game
/// tick with no zero padding, so `events_9000` would otherwise sort after
/// `events_10000`.
pub fn log_paths(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut segments: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter_map(|path| Some((segment_start_tick(&path)?, path)))
        .collect();
    segments.sort();
    Ok(segments.into_iter().map(|(_, path)| path).collect())
}

fn segment_start_tick(path: &Path) -> Option<u64> {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    path.file_stem()
        .and_then(|s| s.to_str())?
        .strip_prefix("events_")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_entity_add_with_every_optional_field() {
        let line = r#"{"t":42,"op":"+","k":"e","s":"nauvis","n":"assembling-machine-1","x":-80.5,"y":28.5,"d":4,"w":3,"h":3,"id":1234}"#;
        let logged = parse_line(line).unwrap();
        assert_eq!(logged.tick, 42);
        assert_eq!(logged.surface.as_deref(), Some("nauvis"));
        assert_eq!(
            logged.event,
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

    /// The encoder omits d/w/h at their defaults, so the reader has to supply
    /// them -- and 1x1, not 0x0, or the entity would replay as zero-sized.
    #[test]
    fn omitted_direction_and_footprint_default_to_zero_and_one_by_one() {
        let line = r#"{"t":1,"op":"+","k":"e","s":"nauvis","n":"transport-belt","x":1.5,"y":2.5}"#;
        match parse_line(line).unwrap().event {
            Event::AddEntity { d, w, h, id, .. } => {
                assert_eq!((d, w, h), (0, 1, 1));
                assert_eq!(id, None);
            }
            other => panic!("expected an add, got {other:?}"),
        }
    }

    #[test]
    fn removal_by_id_carries_no_surface_or_position() {
        let logged = parse_line(r#"{"t":7,"op":"-","k":"e","id":99}"#).unwrap();
        assert_eq!(logged.surface, None);
        assert_eq!(logged.event, Event::RemoveEntityById { id: 99 });
    }

    #[test]
    fn removal_without_an_id_falls_back_to_position() {
        let logged = parse_line(r#"{"t":7,"op":"-","k":"e","s":"nauvis","x":-3.5,"y":4.5}"#).unwrap();
        assert_eq!(logged.event, Event::RemoveEntityAt { x: -3.5, y: 4.5 });
    }

    #[test]
    fn tile_events_use_integer_coordinates() {
        let add = parse_line(r#"{"t":3,"op":"+","k":"t","s":"nauvis","n":"concrete","x":-5,"y":12}"#).unwrap();
        assert_eq!(add.event, Event::AddTile { name: "concrete".to_string(), x: -5, y: 12 });

        let remove = parse_line(r#"{"t":4,"op":"-","k":"t","s":"nauvis","x":-5,"y":12}"#).unwrap();
        assert_eq!(remove.event, Event::RemoveTile { x: -5, y: 12 });
    }

    /// A log's last line can be a partial write if the game was killed
    /// mid-flush, and blank lines are harmless.
    #[test]
    fn malformed_and_empty_lines_are_skipped_not_fatal() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line(r#"{"t":1,"op":"+","k":"e","x":1.5"#).is_none(), "truncated");
        assert!(parse_line(r#"{"t":1,"op":"+","k":"e","x":1.5,"y":2.5}"#).is_none(), "add with no name");
        assert!(parse_line(r#"{"t":1,"op":"+","k":"t","n":"concrete"}"#).is_none(), "tile with no position");
    }

    #[test]
    fn logs_predating_the_surface_field_still_parse() {
        let logged = parse_line(r#"{"t":1,"op":"+","k":"e","n":"pipe","x":1.5,"y":2.5}"#).unwrap();
        assert_eq!(logged.surface, None);
    }

    #[test]
    fn streaming_a_log_yields_events_in_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events_100.jsonl");
        std::fs::write(
            &path,
            "{\"t\":100,\"op\":\"+\",\"k\":\"e\",\"s\":\"nauvis\",\"n\":\"pipe\",\"x\":0.5,\"y\":0.5}\n\
             garbage\n\
             {\"t\":101,\"op\":\"-\",\"k\":\"e\",\"id\":5}\n",
        )
        .unwrap();

        let events: Vec<LoggedEvent> = stream_log(&path).unwrap().collect();
        assert_eq!(events.len(), 2, "the unparseable middle line is skipped");
        assert_eq!(events[0].tick, 100);
        assert_eq!(events[1].event, Event::RemoveEntityById { id: 5 });
    }

    #[test]
    fn segments_order_by_tick_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["events_10000.jsonl", "events_9000.jsonl", "events_100.jsonl"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        // Files that are not segments must not be picked up.
        std::fs::write(dir.path().join("frame_1_nauvis.json"), "{}").unwrap();
        std::fs::write(dir.path().join("baseline.json"), "{}").unwrap();

        let names: Vec<String> = log_paths(dir.path())
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["events_100.jsonl", "events_9000.jsonl", "events_10000.jsonl"]);
    }
}
