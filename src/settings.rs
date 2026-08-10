//! What the tool remembers between runs, so a repeat run stops re-asking
//! questions it already has the answer to.
//!
//! Deliberately small. Every field is something a user would otherwise retype
//! identically every single time: where Factorio is (twice, since the folder
//! and the executable are found separately), how much game time a frame
//! covers, whether to include natural terrain, and the size and frame rate of
//! an exported video.
//!
//! **Nothing here is authoritative.** Every field is an `Option`, absent
//! meaning "never answered", and every one is offered as a *default* that
//! Enter accepts rather than as a decision made on the user's behalf. That
//! matters most for the two paths: a remembered Factorio location that has
//! since moved must not turn into a confusing failure several steps later, so
//! it is validated before being trusted and quietly re-asked if it no longer
//! looks right.
//!
//! Plain JSON, in the user's own config directory rather than beside the
//! executable. Beside the executable would be lost every time the release zip
//! is replaced, which is exactly when someone least wants to redo their
//! setup. Readable by eye and safe to delete: deleting it returns the tool to
//! asking everything, which is also the fix for anything in it going stale.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Everything remembered between runs. Absent fields mean "not answered
/// yet", which is why nothing here has a default value baked in: the
/// difference between "the user chose 60" and "nobody has ever been asked"
/// is what decides whether the first run explains itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Factorio's user data folder, the one holding `mods` and `saves`.
    pub factorio_dir: Option<PathBuf>,
    /// The game executable, needed only by the from-saves flow.
    pub factorio_exe: Option<PathBuf>,
    /// Game seconds per emitted frame.
    pub frame_seconds: Option<u64>,
    /// Whether to include natural terrain when exporting from saves.
    pub capture_terrain: Option<bool>,
    /// Video resolution last chosen when exporting a timelapse. Both stored
    /// rather than a single quality level, since a custom size need not be
    /// 16:9 and deriving one from the other would silently change it.
    pub export_width: Option<u32>,
    pub export_height: Option<u32>,
    /// Frames per second last chosen for a video export.
    pub export_fps: Option<u32>,
}

/// Where the settings file lives, following each platform's own convention
/// rather than inventing one.
///
/// `None` only when the environment gives nothing to build a path from, in
/// which case the tool runs exactly as it did before this existed: asking
/// everything, every time.
pub fn settings_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)?
    } else if cfg!(target_os = "macos") {
        crate::locate::home_dir()?.join("Library/Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::locate::home_dir().unwrap_or_default().join(".config"))
    };
    Some(base.join("save-timelapse").join("settings.json"))
}

impl Settings {
    /// Reads the saved settings, or an empty set.
    ///
    /// A missing file is the first run and an ordinary state, not an error. A
    /// *corrupt* file is also not an error: it is worth one warning and then
    /// starting fresh, because the alternative is a tool that refuses to
    /// launch until the user finds and deletes a file they never knew about.
    pub fn load() -> Settings {
        let Some(path) = settings_path() else { return Settings::default() };
        let Ok(text) = std::fs::read_to_string(&path) else { return Settings::default() };
        match serde_json::from_str(&text) {
            Ok(settings) => settings,
            Err(e) => {
                eprintln!("warning: ignoring unreadable settings at {} ({e})", path.display());
                Settings::default()
            }
        }
    }

    /// The remembered export size, and only when *both* halves are there.
    ///
    /// A width with no height is not half an answer, it is no answer: there
    /// is no sensible way to complete it, and guessing would produce a video
    /// in a shape the user never picked.
    pub fn export_size(&self) -> Option<(u32, u32)> {
        Some((self.export_width?, self.export_height?))
    }

    /// Whether anything has ever been saved. What the first run keys off to
    /// explain itself once and then never again.
    pub fn is_first_run() -> bool {
        settings_path().is_some_and(|path| !path.exists())
    }

    /// Writes the settings, creating the directory if needed.
    ///
    /// Returns the error rather than swallowing it so a caller can say so,
    /// but no caller should treat it as fatal: failing to remember an answer
    /// costs one prompt next time, and refusing to build somebody's timelapse
    /// over it would be absurd.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = settings_path() else {
            return Err(io::Error::other("no writable configuration directory on this system"));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&path, body)
    }

    /// The remembered Factorio user folder, if it still looks like one.
    ///
    /// Validated rather than trusted: a folder that has since been moved,
    /// renamed, or lived on a drive that is not plugged in would otherwise
    /// resurface as a confusing failure much later, and the whole point of
    /// remembering it is to save the user work, not to create a new way to
    /// waste it.
    pub fn valid_factorio_dir(&self) -> Option<&Path> {
        let dir = self.factorio_dir.as_deref()?;
        dir.join("mods").is_dir().then_some(dir)
    }

    /// The remembered game executable, if it is still there.
    pub fn valid_factorio_exe(&self) -> Option<&Path> {
        let exe = self.factorio_exe.as_deref()?;
        exe.is_file().then_some(exe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_settings_file_round_trips() {
        let settings = Settings::default();
        let text = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn every_field_round_trips() {
        let settings = Settings {
            factorio_dir: Some(PathBuf::from("/tmp/factorio")),
            factorio_exe: Some(PathBuf::from("/tmp/factorio/bin/x64/factorio")),
            frame_seconds: Some(30),
            capture_terrain: Some(true),
            export_width: Some(2560),
            export_height: Some(1440),
            export_fps: Some(24),
        };
        let text = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&text).unwrap(), settings);
    }

    /// A file written by an older build has fewer fields than this one knows
    /// about, and a file from a newer build has more. Neither is a reason to
    /// throw away everything the user already answered, which `#[serde(default)]`
    /// plus serde's default of ignoring unknown fields is what buys.
    #[test]
    fn a_file_with_missing_or_unknown_fields_still_loads() {
        let older: Settings = serde_json::from_str(r#"{"frame_seconds":45}"#).unwrap();
        assert_eq!(older.frame_seconds, Some(45));
        assert_eq!(older.capture_terrain, None);

        let newer: Settings = serde_json::from_str(r#"{"frame_seconds":45,"something_from_the_future":true}"#).unwrap();
        assert_eq!(newer.frame_seconds, Some(45));
    }

    /// The reason paths are validated rather than trusted: a remembered
    /// folder that has since gone away must read as "ask again", not as a
    /// path to hand downstream.
    #[test]
    fn a_factorio_folder_that_is_no_longer_there_is_not_offered() {
        let settings = Settings { factorio_dir: Some(PathBuf::from("/definitely/not/here")), ..Default::default() };
        assert!(settings.valid_factorio_dir().is_none());
    }

    #[test]
    fn a_factorio_folder_is_offered_only_when_it_holds_mods() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings { factorio_dir: Some(dir.path().to_path_buf()), ..Default::default() };
        assert!(settings.valid_factorio_dir().is_none(), "no mods folder yet");

        std::fs::create_dir(dir.path().join("mods")).unwrap();
        assert_eq!(settings.valid_factorio_dir(), Some(dir.path()));
    }

    #[test]
    fn a_missing_executable_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("factorio.exe");
        let settings = Settings { factorio_exe: Some(exe.clone()), ..Default::default() };
        assert!(settings.valid_factorio_exe().is_none());

        std::fs::write(&exe, b"pretend").unwrap();
        assert_eq!(settings.valid_factorio_exe(), Some(exe.as_path()));
    }

    /// Corrupt is not fatal. A tool that will not start until the user finds
    /// and deletes a file they never knew existed is worse than one that
    /// forgets their preferences.
    #[test]
    fn malformed_json_loads_as_empty_rather_than_failing() {
        let parsed = serde_json::from_str::<Settings>("{not json at all");
        assert!(parsed.is_err(), "the parse itself fails, and `load` turns that into a default");
    }
}
