//! A stand-in for factorio.exe, so the exporter can be developed on a machine
//! with no Factorio installed.
//!
//! It implements just enough of the real command line to exercise the whole
//! staging and collection path:
//!
//!   --version                 prints a version banner in Factorio's format
//!   --benchmark <save> ...    behaves like a headless run
//!
//! Under `--benchmark` it reads `write-data` out of the `--config` file and
//! decodes the staged `mod-settings.dat`, and only emits a frame when the
//! export trigger is actually switched on. That means a test using this binary
//! genuinely verifies the settings were written correctly, rather than assuming
//! it.
//!
//! Set `FAKE_FACTORIO_FRAME` to a `.stfr` file to control what gets emitted.
//!
//! A save whose name ends in `-silent.zip` makes it exit cleanly having written
//! nothing, reproducing the failure mode where the game runs fine but the mod
//! never fires.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use save_timelapse::frame::{self, Entity, FrameOut};
use save_timelapse::settings_dat::{self, Payload};

const VERSION_BANNER: &str = "Version: 2.0.77 (build 84539, linux64, steam)\n\
                              Version: 64\n\
                              Map input version: 1.0.0-0\n\
                              Map output version: 2.0.77-0";

const TRIGGER: &str = "save-timelapse-headless-scan";
const RESOURCES: &str = "save-timelapse-include-resources";

/// Encoded on demand rather than kept as a byte literal: a `Frame` value is
/// clearer to read and edit than the bytes its binary encoding would produce.
fn default_frame_bytes() -> Vec<u8> {
    let entities = vec![
        Entity { n: "transport-belt".into(), x: -4.5, y: 2.5, d: 4, w: 1, h: 1 },
        Entity { n: "assembling-machine-1".into(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1 },
        Entity { n: "stone-furnace".into(), x: 6.5, y: -1.5, d: 0, w: 1, h: 1 },
        Entity { n: "inserter".into(), x: 3.5, y: 0.5, d: 2, w: 1, h: 1 },
    ];
    frame::write_binary(&FrameOut { tick: 216_000, surface: "nauvis", entities: &entities, tiles: &[] })
}

fn flag_value(file: &settings_dat::SettingsFile, name: &str) -> bool {
    matches!(file.listing().get(&("startup".to_string(), name.to_string())), Some(Payload::Flag(true)))
}

/// Pull `write-data` out of a Factorio config.ini.
fn write_data_dir(config: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config).ok()?;
    text.lines().find_map(|line| line.strip_prefix("write-data=").map(|v| PathBuf::from(v.trim())))
}

fn arg_after(args: &[String], flag: &str) -> Option<PathBuf> {
    let pos = args.iter().position(|a| a == flag)?;
    args.get(pos + 1).map(PathBuf::from)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version") {
        println!("{VERSION_BANNER}");
        return ExitCode::SUCCESS;
    }

    if !args.iter().any(|a| a == "--benchmark") {
        eprintln!("fake-factorio: expected --version or --benchmark");
        return ExitCode::FAILURE;
    }

    let Some(config) = arg_after(&args, "--config") else {
        eprintln!("fake-factorio: --config is required");
        return ExitCode::FAILURE;
    };
    let Some(write_data) = write_data_dir(&config) else {
        eprintln!("fake-factorio: no write-data in {}", config.display());
        return ExitCode::FAILURE;
    };
    let Some(mods) = arg_after(&args, "--mod-directory") else {
        eprintln!("fake-factorio: --mod-directory is required");
        return ExitCode::FAILURE;
    };

    // The real game silently does nothing when the trigger is off. Reproducing
    // that is the whole point: it is the failure this project exists to avoid.
    let settings = match std::fs::read(mods.join("mod-settings.dat")) {
        Ok(bytes) => match settings_dat::decode(&bytes) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("fake-factorio: mod-settings.dat is not decodable: {err}");
                return ExitCode::FAILURE;
            }
        },
        Err(err) => {
            eprintln!("fake-factorio: cannot read mod-settings.dat: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("Loading mods from {}", mods.display());

    let silent = arg_after(&args, "--benchmark")
        .and_then(|save| save.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|name| name.ends_with("-silent.zip"));

    if silent || !flag_value(&settings, TRIGGER) {
        // Exits cleanly having written nothing, exactly like the real game.
        println!("Running update 0");
        println!("  Performed 3 updates");
        return ExitCode::SUCCESS;
    }

    let body = match std::env::var("FAKE_FACTORIO_FRAME") {
        Ok(path) => std::fs::read(&path).unwrap_or_else(|err| {
            eprintln!("fake-factorio: cannot read {path}: {err}");
            default_frame_bytes()
        }),
        Err(_) => default_frame_bytes(),
    };

    let out_dir = write_data.join("script-output").join("save-timelapse");
    if let Err(err) = std::fs::create_dir_all(&out_dir) {
        eprintln!("fake-factorio: cannot create {}: {err}", out_dir.display());
        return ExitCode::FAILURE;
    }

    let tick = 216_000;
    if let Err(err) = std::fs::write(out_dir.join(format!("frame_{tick}_nauvis.stfr")), &body) {
        eprintln!("fake-factorio: cannot write frame: {err}");
        return ExitCode::FAILURE;
    }
    // Milestone state included because the real mod includes it (see
    // mod/export.lua's `export_all_to`), and it is the only thing on the
    // from-saves path that a single save cannot answer for itself: the Rust
    // side recovers timings by comparing consecutive saves, so the pickup
    // being exercised here is what makes that possible at all.
    let _ = std::fs::write(
        out_dir.join(format!("frame_{tick}_manifest.json")),
        format!(
            r#"{{"tick":{tick},"entities":4,"surfaces":["nauvis"],"milestones":{{"science":["automation-science-pack"],"planets":["nauvis"],"rockets":1}}}}"#
        ),
    );

    if flag_value(&settings, RESOURCES) {
        println!("including resource entities");
    }
    println!("Running update 0");
    println!("  Performed 3 updates");
    ExitCode::SUCCESS
}
