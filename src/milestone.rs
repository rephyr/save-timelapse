//! Reading the milestone log the mod writes (`milestones.jsonl`).
//!
//! Notable moments worth marking on the timeline rather than watching for:
//! the first of each science pack, the first rocket, the first arrival on
//! each planet. See `mod/milestones.lua` for how they are detected.
//!
//! Plain newline-delimited JSON for the same reason `player_log.rs` is: a
//! whole playthrough produces on the order of a dozen of these, nowhere near
//! the volume that justified a tagged binary format for frames and events,
//! and being readable by eye is worth more than the few hundred bytes
//! packing it would save.
//!
//! One line per milestone:
//! ```text
//! {"tick":1234567,"kind":"science","id":"logistic-science-pack"}
//! ```
//!
//! `kind` and `id` rather than a prebaked sentence, so wording is this side's
//! decision and a viewer can filter by kind without parsing prose.

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

/// What sort of moment a milestone marks. Unknown kinds are kept rather than
/// dropped: a capture written by a newer mod should still show its markers,
/// just without this build's nicer label for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A science pack produced for the first time. `id` is the item name.
    Science,
    /// The first rocket launched. `id` is always `rocket-launched`.
    Rocket,
    /// A planet reached for the first time. `id` is the surface name.
    Planet,
    Other(String),
}

impl Kind {
    fn parse(raw: &str) -> Kind {
        match raw {
            "science" => Kind::Science,
            "rocket" => Kind::Rocket,
            "planet" => Kind::Planet,
            other => Kind::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Milestone {
    pub tick: u64,
    pub kind: Kind,
    pub id: String,
}

impl Milestone {
    /// A short human label, since `id` is a prototype name and nobody wants
    /// to read `logistic-science-pack` on a timeline.
    ///
    /// Prototype names are the internal identifier, not a localised string,
    /// and a mod cannot hand out localised names outside the game anyway
    /// (`LocalisedString` only resolves in a running Factorio), so this
    /// tidies the raw name rather than pretending to translate it.
    pub fn label(&self) -> String {
        match &self.kind {
            // Named in full rather than trimmed to "First logistic": the
            // pack's name is the recognisable part, and dropping the suffix
            // reads as a sentence cut short.
            Kind::Science => format!("First {}", self.id.replace('-', " ")),
            Kind::Rocket => "First rocket launched".to_string(),
            Kind::Planet => format!("Reached {}", prettify(&self.id)),
            Kind::Other(kind) => format!("{}: {}", prettify(kind), prettify(&self.id)),
        }
    }
}

/// `logistic-science-pack` to `Logistic science pack`.
fn prettify(name: &str) -> String {
    let spaced = name.replace('-', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

#[derive(Deserialize)]
struct RawMilestone {
    tick: u64,
    kind: String,
    id: String,
}

/// Every milestone in `path`, ascending by tick.
///
/// A missing file is an empty list rather than an error: milestones only
/// exist for a live capture, and one recorded before this feature (or with
/// nothing notable yet reached) simply has no file. That is the same
/// "nothing to show" case as terrain capture being off, not a failure.
///
/// A malformed line is skipped with a warning rather than failing the read,
/// matching how the player log treats one: the file is appended to during
/// play and its last line can be a partial write if the game was killed.
pub fn read(path: &Path) -> io::Result<Vec<Milestone>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)?;

    let mut milestones: Vec<Milestone> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<RawMilestone>(line) {
            Ok(raw) => milestones.push(Milestone {
                tick: raw.tick,
                kind: Kind::parse(&raw.kind),
                id: raw.id,
            }),
            Err(e) => eprintln!("warning: skipping malformed milestone line: {e}"),
        }
    }

    // The mod appends in the order things happen, so this is almost always
    // already sorted; it is cheap insurance against a capture that was
    // resumed, or two surfaces recording in one flush.
    milestones.sort_by_key(|m| m.tick);
    Ok(milestones)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("milestones.jsonl");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn reads_every_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            r#"{"tick":100,"kind":"science","id":"automation-science-pack"}
{"tick":200,"kind":"planet","id":"vulcanus"}
{"tick":300,"kind":"rocket","id":"rocket-launched"}
"#,
        );
        let found = read(&path).unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].kind, Kind::Science);
        assert_eq!(found[1].kind, Kind::Planet);
        assert_eq!(found[2].kind, Kind::Rocket);
    }

    #[test]
    fn labels_read_as_english_rather_than_prototype_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            r#"{"tick":1,"kind":"science","id":"logistic-science-pack"}
{"tick":2,"kind":"planet","id":"vulcanus"}
{"tick":3,"kind":"rocket","id":"rocket-launched"}
"#,
        );
        let labels: Vec<String> = read(&path).unwrap().iter().map(Milestone::label).collect();
        assert_eq!(labels, ["First logistic science pack", "Reached Vulcanus", "First rocket launched"]);
    }

    /// A capture from a newer mod must still show its markers, even if this
    /// build has no idea what they mean.
    #[test]
    fn an_unknown_kind_is_kept_rather_than_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "{\"tick\":5,\"kind\":\"cargo-pod\",\"id\":\"first-ascent\"}\n");
        let found = read(&path).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, Kind::Other("cargo-pod".to_string()));
        assert_eq!(found[0].label(), "Cargo pod: First ascent");
    }

    /// The file is appended to during play, so a killed game can leave a
    /// half-written last line.
    #[test]
    fn a_truncated_last_line_does_not_lose_the_ones_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "{\"tick\":1,\"kind\":\"science\",\"id\":\"automation-science-pack\"}\n{\"tick\":2,\"kin",
        );
        assert_eq!(read(&path).unwrap().len(), 1);
    }

    #[test]
    fn milestones_come_back_in_tick_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            r#"{"tick":900,"kind":"planet","id":"gleba"}
{"tick":100,"kind":"science","id":"automation-science-pack"}
"#,
        );
        let ticks: Vec<u64> = read(&path).unwrap().iter().map(|m| m.tick).collect();
        assert_eq!(ticks, [100, 900]);
    }

    /// No file is the ordinary case for a from-saves timelapse, which has no
    /// milestones at all, so it must not read as an error.
    #[test]
    fn a_missing_file_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("nope.jsonl")).unwrap().is_empty());
    }
}
