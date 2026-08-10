//! Codec tests against a settings file genuinely written by Factorio.
//!
//! The unit tests in `settings_dat` only prove the encoder agrees with its own
//! decoder. These prove it agrees with Factorio.

use std::path::PathBuf;

use save_timelapse::settings_dat::{self, Entry, Payload};

fn fixture() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("real-mod-settings.dat");
    std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn a_real_settings_file_re_encodes_byte_for_byte() {
    let original = fixture();
    let decoded = settings_dat::decode(&original).expect("real file must decode");
    assert_eq!(settings_dat::encode(&decoded), original, "re-encoding a Factorio-written file must reproduce it exactly");
}

#[test]
fn a_real_settings_file_has_the_expected_shape() {
    let decoded = settings_dat::decode(&fixture()).expect("decodes");

    assert_eq!(decoded.version[0], 2, "expected a Factorio 2.x settings file");

    let listing = decoded.listing();
    assert!(listing.len() > 100, "expected a populated file, got {}", listing.len());

    for section in ["startup", "runtime-global", "runtime-per-user"] {
        assert!(listing.keys().any(|(s, _)| s == section), "missing section {section}");
    }
}

#[test]
fn adding_our_trigger_leaves_every_other_setting_untouched() {
    let original = fixture();
    let before = settings_dat::decode(&original).expect("decodes").listing();

    let mut file = settings_dat::decode(&original).expect("decodes");
    file.put("startup", "save-timelapse-headless-scan", Entry::flag(true));
    let after = settings_dat::decode(&settings_dat::encode(&file)).expect("re-decodes").listing();

    assert_eq!(after.len(), before.len() + 1, "exactly one setting should have been added");
    assert_eq!(after.get(&("startup".into(), "save-timelapse-headless-scan".into())), Some(&Payload::Flag(true)));

    for (key, value) in &before {
        assert_eq!(after.get(key), Some(value), "setting {key:?} was altered");
    }
}

#[test]
fn a_corrupt_file_is_rejected_rather_than_misread() {
    let mut damaged = fixture();
    damaged.truncate(damaged.len() / 2);
    assert!(settings_dat::decode(&damaged).is_err(), "a truncated file must fail rather than decode into nonsense");
}
