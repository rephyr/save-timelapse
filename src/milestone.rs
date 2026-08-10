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

    fn as_str(&self) -> &str {
        match self {
            Kind::Science => "science",
            Kind::Rocket => "rocket",
            Kind::Planet => "planet",
            Kind::Other(other) => other,
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
            Ok(raw) => milestones.push(Milestone { tick: raw.tick, kind: Kind::parse(&raw.kind), id: raw.id }),
            Err(e) => eprintln!("warning: skipping malformed milestone line: {e}"),
        }
    }

    // The mod appends in the order things happen, so this is almost always
    // already sorted; it is cheap insurance against a capture that was
    // resumed, or two surfaces recording in one flush.
    milestones.sort_by_key(|m| m.tick);
    Ok(milestones)
}

/// What one save knows about milestones, read out of its export manifest
/// (see `mod/export.lua`'s `milestone_state`).
///
/// State, not events. A save records that a science pack has been produced,
/// never when it first was, so a single one of these cannot place a marker on
/// a timeline. [`from_saves`] recovers the timing by comparing consecutive
/// saves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub tick: u64,
    /// Every science pack produced at least once by this point, sorted.
    pub science: Vec<String>,
    /// Every planet reached by this point, sorted. "Reached" means the mod
    /// found the planet's surface inhabited, which is also the condition for
    /// it appearing in frames at all.
    pub planets: Vec<String>,
    /// Rockets launched by this point. A count rather than a flag so a diff
    /// can tell the first launch from launches that were already happening.
    pub rockets: u64,
}

#[derive(Deserialize)]
struct RawManifest {
    tick: u64,
    milestones: Option<RawState>,
}

#[derive(Deserialize)]
struct RawState {
    #[serde(default)]
    science: Vec<String>,
    #[serde(default)]
    planets: Vec<String>,
    #[serde(default)]
    rockets: u64,
}

impl State {
    /// Reads one save's state from its export manifest.
    ///
    /// `Ok(None)` means the manifest predates milestone state, which is the
    /// ordinary case for a manifest written by an older mod. That is a
    /// timelapse without markers, not a failure, exactly as a live capture
    /// recorded before the feature existed has none.
    pub fn from_manifest(path: &Path) -> io::Result<Option<State>> {
        let text = fs::read_to_string(path)?;
        let raw: RawManifest = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
        Ok(raw.milestones.map(|m| State { tick: raw.tick, science: m.science, planets: m.planets, rockets: m.rockets }))
    }
}

/// Recovers milestone timings by comparing what consecutive saves know.
///
/// A save carries totals, so the earliest tick at which something can be
/// *proved* to have happened is the tick of the first save that reports it.
/// That is the tick used. The practical consequence is worth being clear
/// about: **from-saves milestones are only as precise as your save cadence.**
/// A pack first produced an hour before the save that first mentions it is
/// marked at that save, not an hour earlier. Live capture, which watches it
/// happen, is exact; this cannot be, and pretending otherwise by interpolating
/// between saves would invent a moment that no evidence supports.
///
/// Everything already true in the earliest save is emitted at that save's
/// tick. It genuinely happened at some point at or before then, and this
/// matches what live capture does when it is switched on mid-playthrough: the
/// first poll records every science pack already produced (see
/// `mod/milestones.lua`). A timelapse built from an established base
/// therefore opens with a cluster of markers, which is accurate rather than
/// tidy.
///
/// Nothing is ever un-marked. A planet whose surface stops being inhabited
/// (everything on it mined) was still reached.
pub fn from_saves(mut states: Vec<State>) -> Vec<Milestone> {
    // By tick rather than by the order saves were picked or exported: the
    // caller may have selected them in any order, and this comparison is only
    // meaningful chronologically.
    states.sort_by_key(|s| s.tick);

    let mut milestones = Vec::new();
    let mut seen_science: std::collections::BTreeSet<String> = Default::default();
    let mut seen_planets: std::collections::BTreeSet<String> = Default::default();
    let mut seen_rocket = false;

    for state in states {
        for name in &state.science {
            if seen_science.insert(name.clone()) {
                milestones.push(Milestone { tick: state.tick, kind: Kind::Science, id: name.clone() });
            }
        }
        for name in &state.planets {
            if seen_planets.insert(name.clone()) {
                milestones.push(Milestone { tick: state.tick, kind: Kind::Planet, id: name.clone() });
            }
        }
        if state.rockets > 0 && !seen_rocket {
            seen_rocket = true;
            milestones.push(Milestone { tick: state.tick, kind: Kind::Rocket, id: "rocket-launched".to_string() });
        }
    }

    milestones
}

/// Writes milestones in the same newline-delimited JSON the mod writes and
/// [`read`] parses, so a from-saves timelapse and a live capture produce a
/// file the viewer cannot tell apart.
pub fn write_jsonl(path: &Path, milestones: &[Milestone]) -> io::Result<()> {
    let mut body = String::new();
    for milestone in milestones {
        body.push_str(&format!(
            "{{\"tick\":{},\"kind\":{},\"id\":{}}}\n",
            milestone.tick,
            serde_json::to_string(milestone.kind.as_str()).expect("a str always serializes"),
            serde_json::to_string(&milestone.id).expect("a str always serializes"),
        ));
    }
    fs::write(path, body)
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
        let path = write(dir.path(), "{\"tick\":1,\"kind\":\"science\",\"id\":\"automation-science-pack\"}\n{\"tick\":2,\"kin");
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

    /// No file is the ordinary case for a capture recorded before milestones
    /// existed, so it must not read as an error.
    #[test]
    fn a_missing_file_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("nope.jsonl")).unwrap().is_empty());
    }

    fn state(tick: u64, science: &[&str], planets: &[&str], rockets: u64) -> State {
        State {
            tick,
            science: science.iter().map(|s| s.to_string()).collect(),
            planets: planets.iter().map(|s| s.to_string()).collect(),
            rockets,
        }
    }

    fn ids(milestones: &[Milestone]) -> Vec<(u64, String)> {
        milestones.iter().map(|m| (m.tick, m.id.clone())).collect()
    }

    /// The core of the from-saves recovery: something absent from one save
    /// and present in the next happened in between, and is marked at the
    /// later one, the earliest tick at which it can be proved to have
    /// happened.
    #[test]
    fn a_thing_first_appearing_in_a_later_save_is_marked_at_that_save() {
        let found = from_saves(vec![
            state(100, &["automation-science-pack"], &["nauvis"], 0),
            state(200, &["automation-science-pack", "logistic-science-pack"], &["nauvis"], 0),
        ]);
        assert_eq!(
            ids(&found),
            [
                (100, "automation-science-pack".to_string()),
                (100, "nauvis".to_string()),
                (200, "logistic-science-pack".to_string())
            ]
        );
    }

    /// Everything already true in the earliest save is emitted there. It did
    /// happen at or before that point, and this is what live capture does
    /// when switched on mid-playthrough (see mod/milestones.lua, whose first
    /// poll records every pack already produced).
    #[test]
    fn what_the_first_save_already_knows_is_marked_at_the_first_save() {
        let found = from_saves(vec![state(500, &["chemical-science-pack"], &["nauvis", "vulcanus"], 3)]);
        assert_eq!(
            ids(&found),
            [
                (500, "chemical-science-pack".to_string()),
                (500, "nauvis".to_string()),
                (500, "vulcanus".to_string()),
                (500, "rocket-launched".to_string()),
            ]
        );
    }

    #[test]
    fn nothing_is_marked_twice_however_many_saves_repeat_it() {
        let found = from_saves(vec![
            state(1, &["automation-science-pack"], &["nauvis"], 0),
            state(2, &["automation-science-pack"], &["nauvis"], 0),
            state(3, &["automation-science-pack"], &["nauvis"], 0),
        ]);
        assert_eq!(found.len(), 2, "one science pack and one planet, once each");
    }

    /// Rockets are a count rather than a flag precisely so this case works:
    /// launches were already happening before the first save, so the first
    /// one cannot be dated any earlier than that save.
    #[test]
    fn the_rocket_marker_fires_on_the_first_save_with_any_launches() {
        let found =
            from_saves(vec![state(10, &[], &[], 0), state(20, &[], &[], 0), state(30, &[], &[], 2), state(40, &[], &[], 9)]);
        assert_eq!(ids(&found), [(30, "rocket-launched".to_string())]);
    }

    /// The caller may hand over saves in whatever order they were picked or
    /// finished exporting in, and comparing them is only meaningful
    /// chronologically.
    #[test]
    fn saves_are_compared_in_tick_order_not_the_order_given() {
        let found =
            from_saves(vec![state(300, &["logistic-science-pack"], &[], 0), state(100, &["automation-science-pack"], &[], 0)]);
        assert_eq!(ids(&found), [(100, "automation-science-pack".to_string()), (300, "logistic-science-pack".to_string())]);
    }

    /// A planet counts as reached once its surface is inhabited, and a player
    /// who mines everything back up has still been there.
    #[test]
    fn a_planet_that_stops_being_inhabited_stays_marked_as_reached() {
        let found = from_saves(vec![state(10, &[], &["vulcanus"], 0), state(20, &[], &[], 0)]);
        assert_eq!(ids(&found), [(10, "vulcanus".to_string())]);
    }

    #[test]
    fn no_saves_at_all_is_an_empty_list_rather_than_a_panic() {
        assert!(from_saves(Vec::new()).is_empty());
    }

    /// The point of writing the same format the mod writes: a from-saves
    /// timelapse and a live capture must produce a file the viewer cannot
    /// tell apart, including for a modded pack name or a surface with
    /// characters worth escaping.
    #[test]
    fn written_milestones_read_back_identically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("milestones.jsonl");
        let original = from_saves(vec![state(
            42,
            &["automation-science-pack", "se-deep-space-science-pack"],
            &["nauvis", "a \"quoted\" moon"],
            1,
        )]);

        write_jsonl(&path, &original).unwrap();
        assert_eq!(read(&path).unwrap(), original);
    }

    #[test]
    fn state_is_read_out_of_an_export_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame_900_manifest.json");
        fs::write(
            &path,
            r#"{"tick":900,"entities":5,"tiles":2,"surfaces":["nauvis"],
                "milestones":{"science":["automation-science-pack"],"planets":["nauvis"],"rockets":4}}"#,
        )
        .unwrap();

        let found = State::from_manifest(&path).unwrap().expect("this manifest carries state");
        assert_eq!(found, state(900, &["automation-science-pack"], &["nauvis"], 4));
    }

    /// A manifest from a mod predating milestone state is a timelapse without
    /// markers, not a broken export.
    #[test]
    fn a_manifest_without_milestone_state_is_none_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame_900_manifest.json");
        fs::write(&path, r#"{"tick":900,"entities":5,"tiles":2,"surfaces":["nauvis"]}"#).unwrap();
        assert_eq!(State::from_manifest(&path).unwrap(), None);
    }
}
