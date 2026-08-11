//! Drives Factorio headless to export a save's entities.
//!
//! Every export runs against a staged directory tree owned by this process, so
//! the user's Factorio installation and settings are never modified.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::settings_dat::{self, Version};

pub const MOD_NAME: &str = "save-timelapse";
const TRIGGER_SETTING: &str = "save-timelapse-headless-scan";
const RESOURCES_SETTING: &str = "save-timelapse-include-resources";
const TERRAIN_SETTING: &str = "save-timelapse-capture-terrain";
const TERRAIN_SCAN_SETTING: &str = "save-timelapse-terrain-scan";

#[derive(Clone)]
pub struct ExportConfig {
    pub factorio: PathBuf,
    pub user_mods: PathBuf,
    /// Directory holding this mod's Lua, staged in place of any installed copy.
    pub mod_source: PathBuf,
    pub include_resources: bool,
    /// Natural terrain (grass, water, trees, cliffs, ...) around the base,
    /// not just what the player built. Roughly 5x'd export size and time in
    /// testing, so this is a real opt-in rather than something to default on.
    pub capture_terrain: bool,
    /// Scan this save for natural ground and nothing else.
    ///
    /// Mutually exclusive with the frame export rather than an addition to
    /// it, because the two want different saves: frames come from every save
    /// in the set, ground from one. Ground does not change over a
    /// playthrough, so describing it once is enough, and doing it in its own
    /// pass keeps it out of every frame and out of anybody's game.
    pub terrain_scan: bool,
}

#[derive(Debug)]
pub struct ExportOutcome {
    /// Exported surface files, largest first.
    pub frames: Vec<PathBuf>,
    pub seconds: f64,
    /// The mod's one-shot player-position sample, if it found anyone with a
    /// valid position to record (see mod/control.lua's `sample_all_players`).
    pub players_log: Option<PathBuf>,
    /// What this save knows about milestones, read out of the manifest the
    /// mod writes beside the frames.
    ///
    /// `None` for a manifest written by a mod predating milestone state,
    /// which costs markers and nothing else. One save's state cannot place a
    /// marker by itself; see `milestone::from_saves`, which compares them.
    pub milestones: Option<crate::milestone::State>,
}

/// Read the version from the executable rather than assuming one.
///
/// Reads stdout and stderr together. Which stream a program writes a version
/// banner to is a per-build detail nobody should have to know, and searching
/// both costs one concatenation against silently returning `None` and
/// reporting no version at all.
pub fn factorio_version(exe: &Path) -> Option<Version> {
    let output = Command::new(exe).arg("--version").output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let field = text.lines().find(|line| line.contains("Version:"))?.split_whitespace().find(|token| token.contains('.'))?;

    let parts: Vec<u16> = field.split('.').filter_map(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [major, minor, patch, ..] => Some([*major, *minor, *patch, 0]),
        _ => None,
    }
}

/// Hardlink when the filesystem permits, otherwise copy. Mirroring a mods
/// folder per export should not cost disk space.
fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to).or_else(|_| std::fs::copy(from, to).map(drop))
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for item in std::fs::read_dir(from)? {
        let item = item?;
        let target = to.join(item.file_name());
        if item.file_type()?.is_dir() {
            copy_tree(&item.path(), &target)?;
        } else {
            std::fs::copy(item.path(), target)?;
        }
    }
    Ok(())
}

/// Factorio loads only mods marked enabled, so make sure ours is listed.
fn enable_in_mod_list(list: &Path) -> io::Result<()> {
    let mut doc: serde_json::Value = std::fs::read(list)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({ "mods": [] }));

    let entries = doc["mods"]
        .as_array_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mod-list.json has no mods array"))?;

    match entries.iter_mut().find(|m| m["name"].as_str() == Some(MOD_NAME)) {
        Some(found) => found["enabled"] = serde_json::Value::Bool(true),
        None => entries.push(serde_json::json!({ "name": MOD_NAME, "enabled": true })),
    }

    std::fs::write(list, serde_json::to_vec_pretty(&doc)?)
}

/// Assemble the staged mods directory: the user's mods, ours, and a settings
/// file with the export trigger enabled.
fn stage_mods(staged: &Path, config: &ExportConfig) -> io::Result<PathBuf> {
    let mods = staged.join("mods");
    std::fs::create_dir_all(&mods)?;

    for item in std::fs::read_dir(&config.user_mods)? {
        let item = item?;
        let name = item.file_name();
        let label = name.to_string_lossy();

        // Settings are rewritten below. Any installed build of this mod is
        // replaced by mod_source, so skip it to avoid loading two copies.
        if label == "mod-settings.dat" || label.starts_with(MOD_NAME) {
            continue;
        }

        let target = mods.join(&name);
        if target.exists() {
            continue;
        }

        if item.file_type()?.is_dir() {
            copy_tree(&item.path(), &target)?;
        } else if label == "mod-list.json" {
            // Copied, never hardlinked: Factorio rewrites this file during
            // load and a shared inode would reach the user's real folder.
            std::fs::copy(item.path(), &target)?;
        } else if label.ends_with(".zip") {
            clone_file(&item.path(), &target)?;
        }
    }

    copy_tree(&config.mod_source, &mods.join(MOD_NAME))?;
    enable_in_mod_list(&mods.join("mod-list.json"))?;

    settings_dat::apply_to_file(
        &config.user_mods.join("mod-settings.dat"),
        &mods.join("mod-settings.dat"),
        &[
            ("startup", TRIGGER_SETTING, !config.terrain_scan),
            ("startup", TERRAIN_SCAN_SETTING, config.terrain_scan),
            ("startup", RESOURCES_SETTING, config.include_resources),
            ("startup", TERRAIN_SETTING, config.capture_terrain),
        ],
        factorio_version(&config.factorio).unwrap_or([2, 0, 0, 0]),
    )?;

    Ok(mods)
}

/// Locate the install's `data` directory from the executable path.
///
/// The usual layout is `<root>/bin/x64/factorio`, but macOS app bundles put the
/// binary in `Contents/MacOS`, so walk up looking for a real `data` directory
/// rather than assuming a fixed depth.
pub fn install_data_dir(exe: &Path) -> Option<PathBuf> {
    let mut dir = exe.parent();
    for _ in 0..4 {
        let current = dir?;
        let candidate = current.join("data");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    // Nothing found: fall back to the conventional depth so the error surfaces
    // from Factorio itself, which reports it better than we can.
    exe.parent()?.parent()?.parent().map(|root| root.join("data"))
}

/// Export one save. `staged` is a directory this call owns and may delete.
/// Stage a private Factorio, run the save through it, and return the
/// directory the mod wrote into.
///
/// Shared by the frame export and the ground scan, which differ only in which
/// startup flag `stage_mods` sets: everything about locating the install,
/// writing a config that reads real data but writes staged, and reporting a
/// failed run is identical.
fn run_factorio(save: &Path, staged: &Path, config: &ExportConfig) -> io::Result<PathBuf> {
    let written_to = staged.join("script-output").join(MOD_NAME);
    std::fs::create_dir_all(&written_to)?;

    let mods = stage_mods(staged, config)?;

    let data = install_data_dir(&config.factorio)
        .ok_or_else(|| io::Error::other("cannot locate the Factorio data directory from the executable path"))?;

    let config_file = staged.join("config.ini");
    std::fs::write(&config_file, format!("[path]\nread-data={}\nwrite-data={}\n", data.display(), staged.display()))?;

    let run = Command::new(&config.factorio)
        .arg("--benchmark")
        .arg(save)
        .arg("--benchmark-ticks")
        .arg("3")
        .arg("--config")
        .arg(&config_file)
        .arg("--mod-directory")
        .arg(&mods)
        .arg("--disable-audio")
        .output()?;

    if !run.status.success() {
        let stdout = String::from_utf8_lossy(&run.stdout);
        let tail: Vec<&str> = stdout.lines().rev().take(3).collect();
        return Err(io::Error::other(format!(
            "factorio exited with {}: {} {}",
            run.status,
            tail.join(" | "),
            String::from_utf8_lossy(&run.stderr).trim()
        )));
    }

    Ok(written_to)
}

pub fn export_save(save: &Path, staged: &Path, config: &ExportConfig) -> io::Result<ExportOutcome> {
    let started = Instant::now();
    let written_to = run_factorio(save, staged, config)?;

    let mut frames: Vec<PathBuf> = std::fs::read_dir(&written_to)?
        .filter_map(Result::ok)
        .map(|item| item.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("frame_") && name.ends_with(".stfr") && !name.contains("manifest"))
        })
        .collect();

    // Largest first: the busiest surface is the one worth showing.
    frames.sort_by_key(|p| std::cmp::Reverse(p.metadata().map(|m| m.len()).unwrap_or(0)));

    if frames.is_empty() {
        return Err(io::Error::other(
            "no frame produced, so the mod did not run. The export trigger must \
             be a startup setting: runtime-global values are stored inside the \
             save and cannot be set from outside.",
        ));
    }

    let players_log = Some(written_to.join("players.jsonl")).filter(|p| p.exists());

    // The manifest the mod writes beside the frames, which is what carries
    // this save's milestone state. Deliberately best-effort: a missing or
    // unreadable manifest costs markers, never the export, since the frames
    // are the thing actually being asked for here.
    let milestones = std::fs::read_dir(&written_to)
        .ok()
        .and_then(|entries| {
            entries.filter_map(Result::ok).map(|item| item.path()).find(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.starts_with("frame_") && name.ends_with("_manifest.json"))
            })
        })
        .and_then(|path| match crate::milestone::State::from_manifest(&path) {
            Ok(state) => state,
            Err(e) => {
                eprintln!("warning: could not read milestone state from {}: {e}", path.display());
                None
            }
        });

    Ok(ExportOutcome { frames, seconds: started.elapsed().as_secs_f64(), players_log, milestones })
}

/// What one ground scan produced.
#[derive(Debug)]
pub struct TerrainScan {
    /// The playthrough the scanned save actually belongs to, taken from the
    /// folder the mod wrote into rather than from anything the caller
    /// supplied. That is the whole verification: a save from a different game
    /// lands under a different session and the caller can say so, instead of
    /// quietly laying a stranger's landscape under somebody's factory.
    pub session_id: u32,
    pub files: Vec<PathBuf>,
    pub seconds: f64,
    /// The game tick the scanned save was at.
    ///
    /// Worth carrying because it is the one thing that can make a scan wrong
    /// while succeeding: ground only exists in chunks the save had already
    /// generated, so a save older than the end of a capture cannot describe
    /// wherever the factory grew after it. The caller has the capture's own
    /// tick and can compare.
    pub tick: u64,
}

/// Read one save's natural ground, once, for a whole timelapse.
///
/// Ground is the only part of a capture that does not change, so it needs no
/// history: entities need a record per placement and placed floor needs one,
/// but grass is grass for the whole playthrough. Scanning it separately keeps
/// it out of every frame of a from-saves export and out of the live baseline
/// that freezes somebody's game, and lets the area be chosen knowing how far
/// the factory eventually reached.
///
/// The trade is ground that has since been built over: water landfilled at
/// hour three reads as landfill at hour ten and its water is never recorded,
/// so replaying from the start shows a hole there until the landfill is laid.
pub fn scan_terrain(save: &Path, staged: &Path, config: &ExportConfig) -> io::Result<TerrainScan> {
    let started = Instant::now();
    let scan = ExportConfig { terrain_scan: true, ..config.clone() };
    let written_to = run_factorio(save, staged, &scan)?;

    // One session folder per playthrough, and a scan only ever loads one
    // save, so anything else here would mean a stale staging directory.
    let mut found: Option<(u32, Vec<PathBuf>)> = None;
    for entry in std::fs::read_dir(&written_to)?.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(session_id) = path.file_name().and_then(|n| n.to_str()).and_then(|n| u32::from_str_radix(n, 16).ok()) else {
            continue;
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&path)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("terrain_") && n.ends_with(".stfr")))
            .collect();
        files.sort();
        if !files.is_empty() {
            found = Some((session_id, files));
        }
    }

    let (session_id, files) = found.ok_or_else(|| {
        io::Error::other(
            "the scan produced no ground. Either nothing on this save is owned \
             by the player force, or the mod did not run: the scan trigger must \
             be a startup setting, since runtime-global values live inside the \
             save and cannot be set from outside.",
        )
    })?;

    // From the file the mod just wrote, not from anything guessed about the
    // save: the header carries the tick the export ran at.
    let tick = files.first().and_then(|p| crate::frame::read_header(p).ok()).map(|(tick, _)| tick).unwrap_or(0);

    Ok(TerrainScan { session_id, files, seconds: started.elapsed().as_secs_f64(), tick })
}
