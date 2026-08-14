//! How this program says a size, a place, a stretch of play and a recording.
//!
//! One vocabulary rather than one per front end: the console menu and the
//! window both describe the same things, and two of these drifting apart is
//! how the same recording comes to read differently depending on where you
//! looked at it.

use std::time::{Duration, SystemTime};

use crate::replay;
use crate::viewer::TICKS_PER_SECOND;
use crate::with_thousands;

/// A coarse "how long ago" label, good enough to recognise your own
/// playthrough in a list. Factorio gives mods no way to read a save name.
pub fn describe_age(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        return "just now".to_string();
    }
    if secs < 3600 {
        let minutes = secs / 60;
        return format!("{minutes} minute{} ago", if minutes == 1 { "" } else { "s" });
    }
    if secs < 86400 {
        let hours = secs / 3600;
        return format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" });
    }
    let days = secs / 86400;
    format!("{days} day{} ago", if days == 1 { "" } else { "s" })
}

/// Bytes as something a person can compare at a glance. Captures range from
/// a few hundred KiB to several GiB, so a single unit would either be
/// unreadably long or lose the distinction between the small ones.
pub fn describe_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

/// A raw surface name as a player would say it. Factorio's own names are
/// lowercase (`nauvis`, `platform-1`), which reads like a database key next to
/// prose.
pub fn pretty_place(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The places in a recording, named rather than listed exhaustively. A Space
/// Age playthrough reaches five planets and any number of platforms, which ran
/// to nine names on a real capture. Two and a count says the same thing.
pub fn describe_places(surfaces: &[String]) -> String {
    match surfaces {
        [] => "nothing yet".to_string(),
        [one] => pretty_place(one),
        [one, two] => format!("{} and {}", pretty_place(one), pretty_place(two)),
        [one, two, rest @ ..] => {
            format!("{}, {} and {} more", pretty_place(one), pretty_place(two), rest.len())
        }
    }
}

/// How much play time a built timelapse covers, from the snapshot it starts
/// at to the last thing replayed. Minutes are dropped once there are hours:
/// at that scale they are noise, and a round number is easier to hold on to.
pub fn describe_span(from_tick: u64, to_tick: u64) -> String {
    let minutes = to_tick.saturating_sub(from_tick) / TICKS_PER_SECOND / 60;
    match (minutes / 60, minutes) {
        (0, 0) => "less than a minute of play".to_string(),
        (0, 1) => "1 minute of play".to_string(),
        (0, m) => format!("{m} minutes of play"),
        (1, _) => "1 hour of play".to_string(),
        (h, _) => format!("{h} hours of play"),
    }
}

/// How far into a playthrough a tick is, in hours of play.
pub fn describe_play_time(tick: u64) -> String {
    let hours = tick / TICKS_PER_SECOND / 3600;
    match hours {
        0 => "under an hour in".to_string(),
        1 => "1 hour in".to_string(),
        n => format!("{n} hours in"),
    }
}

/// One line describing a recording, for the picker before a rebuild. Leads with
/// the name when there is one, the places when there is not: a hex session id
/// identifies a recording perfectly and tells the person choosing between two
/// of them nothing. Size belongs in the management screen, where the question
/// is what to delete.
pub fn describe_session(session: &replay::Session, now: SystemTime) -> String {
    let age = describe_age(now.duration_since(session.last_modified).unwrap_or_default());
    let places = describe_places(&session.baseline.surfaces);
    // Older captures wrote only a total, which counts the trees and ore a
    // capture keeps for context alongside what somebody built.
    let buildings = session.baseline.buildings.unwrap_or(session.baseline.entities);
    let scale = format!("{} buildings, {}", with_thousands(buildings as u64), describe_play_time(session.baseline.tick));
    match session.label() {
        Some(name) => format!("{name}  ({places})\n     {scale}, last played {age}"),
        None => format!("{places}\n     {scale}, last played {age}"),
    }
}

/// The same recording, plus what it costs on disk, for the management screen.
pub fn describe_session_with_size(session: &replay::Session, now: SystemTime) -> String {
    format!("{}, {}", describe_session(session, now), describe_size(session.size_on_disk()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_size_picks_a_unit_a_person_can_compare() {
        assert_eq!(describe_size(0), "0 B");
        assert_eq!(describe_size(512), "512 B");
        assert_eq!(describe_size(1024), "1.0 KiB");
        assert_eq!(describe_size(38 * (1 << 20)), "38.0 MiB");
        assert_eq!(describe_size(3 * (1 << 30) / 2), "1.5 GiB");
    }

    #[test]
    fn an_unnamed_recording_is_described_by_where_it_happened() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("0000002a");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("baseline.json"), r#"{"tick":100,"entities":7,"tiles":3,"surfaces":["nauvis"]}"#)
            .unwrap();

        let sessions = replay::discover_sessions(dir.path()).unwrap();
        let line = describe_session(&sessions[0], SystemTime::now());
        assert!(line.starts_with("Nauvis"), "leads with the place: {line}");
        assert!(line.contains("7 buildings"), "got: {line}");
        // The things it deliberately stopped saying, none of which help
        // anybody choose between two recordings.
        assert!(!line.contains("0000002a"), "no session id: {line}");
        assert!(!line.contains("tick"), "no raw tick: {line}");

        sessions[0].set_label("Vulcanus run").unwrap();
        let named = describe_session(&replay::discover_sessions(dir.path()).unwrap()[0], SystemTime::now());
        assert!(named.starts_with("Vulcanus run"), "a named recording leads with its name: {named}");
    }

    #[test]
    fn places_are_named_up_to_two_then_counted() {
        let of = |names: &[&str]| describe_places(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(of(&[]), "nothing yet");
        assert_eq!(of(&["nauvis"]), "Nauvis");
        assert_eq!(of(&["nauvis", "vulcanus"]), "Nauvis and Vulcanus");
        assert_eq!(of(&["nauvis", "vulcanus", "fulgora"]), "Nauvis, Vulcanus and 1 more");
        assert_eq!(of(&["nauvis", "platform-1", "a", "b", "c"]), "Nauvis, Platform-1 and 3 more");
    }

    #[test]
    fn a_built_span_is_rounded_to_something_sayable() {
        let hour = TICKS_PER_SECOND * 3600;
        assert_eq!(describe_span(0, 0), "less than a minute of play");
        assert_eq!(describe_span(0, TICKS_PER_SECOND * 60), "1 minute of play");
        assert_eq!(describe_span(0, TICKS_PER_SECOND * 60 * 19), "19 minutes of play");
        assert_eq!(describe_span(0, hour), "1 hour of play");
        assert_eq!(describe_span(hour, hour * 4), "3 hours of play");
    }

    #[test]
    fn describe_age_just_now_for_under_a_minute() {
        assert_eq!(describe_age(Duration::from_secs(30)), "just now");
    }

    #[test]
    fn describe_age_minutes() {
        assert_eq!(describe_age(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(describe_age(Duration::from_secs(60 * 5)), "5 minutes ago");
    }

    #[test]
    fn describe_age_hours() {
        assert_eq!(describe_age(Duration::from_secs(3600)), "1 hour ago");
        assert_eq!(describe_age(Duration::from_secs(3600 * 3)), "3 hours ago");
    }

    #[test]
    fn describe_age_days() {
        assert_eq!(describe_age(Duration::from_secs(86400)), "1 day ago");
        assert_eq!(describe_age(Duration::from_secs(86400 * 2)), "2 days ago");
    }
}
