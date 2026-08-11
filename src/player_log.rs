//! Reading the player-position log the mod writes (`players.jsonl`, or the
//! session-tagged name inside a live capture directory before
//! `save-timelapse.exe` relocates it).
//!
//! Newline-delimited JSON rather than a binary format: a sample happens at
//! most every few seconds. The mod writes and this reads the same shape, so
//! the file is relocated rather than rewritten.
//!
//! ```text
//! {"tick":123456,"players":[{"name":"Alice","surface":"nauvis","x":10.5,"y":-3.2}]}
//! ```

use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

/// One player's position at a given tick, already flattened out of the
/// per-line `players` array: callers want "every position Alice was ever
/// at" far more often than "what did this one line say."
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSample {
    pub tick: u64,
    pub name: String,
    pub surface: String,
    pub x: f32,
    pub y: f32,
}

#[derive(Deserialize)]
struct Line {
    tick: u64,
    players: Vec<LinePlayer>,
}

#[derive(Deserialize)]
struct LinePlayer {
    name: String,
    surface: String,
    x: f32,
    y: f32,
}

/// Reads every sample in file order; callers that need tick order sort. A
/// malformed line is skipped rather than failing the file, the log being
/// appended to during play so a killed process leaves a partial last line.
pub fn read_jsonl(path: &Path) -> io::Result<Vec<PlayerSample>> {
    let text = fs::read_to_string(path)?;
    let mut samples = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Line>(line) else { continue };
        for p in parsed.players {
            samples.push(PlayerSample { tick: parsed.tick, name: p.name, surface: p.surface, x: p.x, y: p.y });
        }
    }
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_sample_from_every_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("players.jsonl");
        fs::write(
            &path,
            "{\"tick\":100,\"players\":[{\"name\":\"Alice\",\"surface\":\"nauvis\",\"x\":1.5,\"y\":2.5}]}\n\
             {\"tick\":200,\"players\":[{\"name\":\"Alice\",\"surface\":\"nauvis\",\"x\":3.0,\"y\":4.0},\
             {\"name\":\"Bob\",\"surface\":\"vulcanus\",\"x\":-1.0,\"y\":-2.0}]}\n",
        )
        .unwrap();

        let samples = read_jsonl(&path).unwrap();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0], PlayerSample { tick: 100, name: "Alice".into(), surface: "nauvis".into(), x: 1.5, y: 2.5 });
        assert_eq!(samples[2].name, "Bob");
    }

    #[test]
    fn a_malformed_line_is_skipped_not_a_hard_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("players.jsonl");
        fs::write(
            &path,
            "{\"tick\":100,\"players\":[{\"name\":\"Alice\",\"surface\":\"nauvis\",\"x\":1.0,\"y\":2.0}]}\n\
             not even json\n\
             {\"tick\":300,\"players\":[{\"name\":\"Alice\",\"surface\":\"nauvis\",\"x\":9.0,\"y\":9.0}]}\n",
        )
        .unwrap();

        let samples = read_jsonl(&path).unwrap();
        assert_eq!(samples.len(), 2, "the malformed middle line should be skipped, not fatal");
        assert_eq!(samples[1].tick, 300);
    }

    #[test]
    fn an_empty_players_list_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("players.jsonl");
        fs::write(&path, "{\"tick\":1,\"players\":[]}\n").unwrap();

        assert!(read_jsonl(&path).unwrap().is_empty());
    }

    #[test]
    fn a_missing_file_is_a_real_error_not_an_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_jsonl(&dir.path().join("nope.jsonl")).is_err());
    }
}
