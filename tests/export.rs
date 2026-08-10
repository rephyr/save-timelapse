//! End-to-end exporter tests that never touch a real Factorio install.
//!
//! They run the `fake-factorio` binary in place of the game. It decodes the
//! staged `mod-settings.dat` and only emits a frame when the export trigger is
//! genuinely set, so these tests verify the settings staging for real rather
//! than assuming it worked.

use std::fs;
use std::path::{Path, PathBuf};

use save_timelapse::export::{self, ExportConfig};
use save_timelapse::settings_dat::{self, Entry, Payload};

/// Path to a binary built alongside the test harness.
fn built_binary(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().expect("test exe path");
    dir.pop(); // the test binary itself
    if dir.ends_with("deps") {
        dir.pop();
    }
    let candidate = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        candidate.exists(),
        "{} not built. Run `cargo test` (which builds all bins) rather than \
         invoking the test binary directly.",
        candidate.display()
    );
    candidate
}

/// A Factorio install laid out the way the exporter expects, with the fake
/// binary standing in for the real one at bin/x64.
fn fake_install(root: &Path) -> PathBuf {
    let bin = root.join("factorio").join("bin").join("x64");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(root.join("factorio").join("data")).unwrap();

    let exe = bin.join(format!("factorio{}", std::env::consts::EXE_SUFFIX));
    fs::copy(built_binary("fake-factorio"), &exe).unwrap();
    make_executable(&exe);
    exe
}

/// `fs::copy` doesn't reliably carry the execute bit over on every platform,
/// and `Command::new` needs it set to actually run the copy. Windows has no
/// such bit at all (a `.exe` is executable by extension alone), which is
/// exactly why this was never needed to make this test pass on a Windows dev
/// machine, only surfacing on Linux CI.
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(path, perms).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// A mods folder holding an unrelated mod and its settings, so tests can prove
/// those survive staging.
fn user_mods(root: &Path) -> PathBuf {
    let mods = root.join("mods");
    fs::create_dir_all(&mods).unwrap();
    fs::write(mods.join("SomeOtherMod_1.2.3.zip"), b"not really a zip").unwrap();
    fs::write(mods.join("mod-list.json"), br#"{"mods":[{"name":"base","enabled":true},{"name":"SomeOtherMod","enabled":true}]}"#)
        .unwrap();

    let mut settings = settings_dat::SettingsFile::blank([2, 0, 77, 0]);
    settings.put("startup", "some-other-mod-option", Entry::flag(true));
    settings.put("runtime-global", "another-option", Entry::flag(false));
    fs::write(mods.join("mod-settings.dat"), settings_dat::encode(&settings)).unwrap();

    mods
}

fn config_for(root: &Path) -> ExportConfig {
    ExportConfig {
        factorio: fake_install(root),
        user_mods: user_mods(root),
        mod_source: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mod"),
        include_resources: false,
        capture_terrain: false,
    }
}

#[test]
fn version_is_read_from_the_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = fake_install(tmp.path());

    // Reports what the process actually did when this fails, rather than only
    // that the answer was `None`. Every way this can break (the copy not
    // being executable, the binary refusing to start, the banner arriving on
    // a stream nobody read) produces the identical `None`, and a bare
    // assertion on it sends whoever hits it back to guessing. That is not
    // hypothetical: it failed exactly this way the first time CI ran on
    // Linux, having always passed on Windows.
    let ran = std::process::Command::new(&exe).arg("--version").output();
    let detail = match &ran {
        Ok(out) => format!(
            "exit {:?}\n  stdout: {:?}\n  stderr: {:?}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => format!("could not run {} at all: {e}", exe.display()),
    };

    assert_eq!(export::factorio_version(&exe), Some([2, 0, 77, 0]), "running --version gave:\n  {detail}");
}

#[test]
fn exporting_produces_a_frame() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config_for(tmp.path());

    let save = tmp.path().join("MyBase.zip");
    fs::write(&save, b"pretend save").unwrap();

    let outcome = export::export_save(&save, &tmp.path().join("staged"), &config).expect("export should produce a frame");

    assert_eq!(outcome.frames.len(), 1);
    let frame = save_timelapse::frame::read_binary(&fs::read(&outcome.frames[0]).unwrap()).unwrap();
    assert_eq!(frame.surface, "nauvis");
    assert_eq!(frame.count, 4);
}

/// A save's milestone state comes back out of the manifest the mod writes
/// beside the frames, which is the only route it has: no single save knows
/// when anything first happened, so `milestone::from_saves` needs one of
/// these per save to compare.
#[test]
fn exporting_reads_the_saves_milestone_state_from_its_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config_for(tmp.path());

    let save = tmp.path().join("MyBase.zip");
    fs::write(&save, b"pretend save").unwrap();

    let outcome = export::export_save(&save, &tmp.path().join("staged"), &config).expect("export should produce a frame");

    let state = outcome.milestones.expect("the manifest carries milestone state");
    assert_eq!(state.tick, 216_000);
    assert_eq!(state.science, ["automation-science-pack"]);
    assert_eq!(state.planets, ["nauvis"]);
    assert_eq!(state.rockets, 1);
}

#[test]
fn staging_preserves_other_mods_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config_for(tmp.path());
    let save = tmp.path().join("MyBase.zip");
    fs::write(&save, b"pretend save").unwrap();

    let staged = tmp.path().join("staged");
    export::export_save(&save, &staged, &config).expect("export");

    let listing = settings_dat::decode(&fs::read(staged.join("mods").join("mod-settings.dat")).unwrap()).unwrap().listing();

    // The unrelated mod's settings must survive untouched.
    assert_eq!(
        listing.get(&("startup".into(), "some-other-mod-option".into())),
        Some(&Payload::Flag(true)),
        "staging destroyed another mod's startup setting"
    );
    assert_eq!(
        listing.get(&("runtime-global".into(), "another-option".into())),
        Some(&Payload::Flag(false)),
        "staging destroyed another mod's runtime-global setting"
    );
    // And ours must have been added.
    assert_eq!(listing.get(&("startup".into(), "save-timelapse-headless-scan".into())), Some(&Payload::Flag(true)));
}

#[test]
fn the_users_mods_folder_is_never_modified() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config_for(tmp.path());
    let save = tmp.path().join("MyBase.zip");
    fs::write(&save, b"pretend save").unwrap();

    let list_path = config.user_mods.join("mod-list.json");
    let settings_path = config.user_mods.join("mod-settings.dat");
    let list_before = fs::read(&list_path).unwrap();
    let settings_before = fs::read(&settings_path).unwrap();

    export::export_save(&save, &tmp.path().join("staged"), &config).expect("export");

    assert_eq!(fs::read(&list_path).unwrap(), list_before, "mod-list.json was modified");
    assert_eq!(fs::read(&settings_path).unwrap(), settings_before, "the user's mod-settings.dat was modified");
}

#[test]
fn our_mod_is_enabled_in_the_staged_list() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config_for(tmp.path());
    let save = tmp.path().join("MyBase.zip");
    fs::write(&save, b"pretend save").unwrap();

    let staged = tmp.path().join("staged");
    export::export_save(&save, &staged, &config).expect("export");

    let list: serde_json::Value = serde_json::from_slice(&fs::read(staged.join("mods").join("mod-list.json")).unwrap()).unwrap();
    let entry =
        list["mods"].as_array().unwrap().iter().find(|m| m["name"] == export::MOD_NAME).expect("our mod should be listed");
    assert_eq!(entry["enabled"], true);

    // The pre-existing entries must still be there.
    assert!(list["mods"].as_array().unwrap().iter().any(|m| m["name"] == "SomeOtherMod"));
}

#[test]
fn a_missing_executable_fails_rather_than_reporting_success() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = config_for(tmp.path());
    config.factorio = tmp.path().join("absent").join("bin").join("x64").join("factorio");

    let save = tmp.path().join("MyBase.zip");
    fs::write(&save, b"pretend save").unwrap();

    export::export_save(&save, &tmp.path().join("staged"), &config).expect_err("a missing executable must be an error");
}

/// The failure this project exists to prevent: the game runs, exits cleanly,
/// and writes nothing because the trigger never took effect. It must surface as
/// an error rather than a silent empty result.
#[test]
fn a_clean_run_that_writes_nothing_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config_for(tmp.path());

    // The fake game exits cleanly and writes nothing for this name.
    let save = tmp.path().join("MyBase-silent.zip");
    fs::write(&save, b"pretend save").unwrap();

    let err = export::export_save(&save, &tmp.path().join("staged"), &config).expect_err("no frame written must be an error");
    assert!(err.to_string().contains("startup setting"), "the error should explain the startup-setting requirement, got: {err}");
}
