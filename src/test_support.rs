//! Helpers shared by more than one module's tests.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A capture with a one-entity baseline on nauvis and a log that builds one
/// more thing per tick for `ticks` ticks, so a replay over it emits frames
/// rather than finishing with nothing to write.
///
/// Returns the session folder. Its parent is the capture directory, and its
/// baseline manifest is `baseline.json` inside it.
pub fn capture_with_events(ticks: u64) -> (tempfile::TempDir, PathBuf) {
    use crate::frame;
    use crate::wire::ByteWriter;

    let dir = tempfile::tempdir().unwrap();
    let session_dir = dir.path().join("00000001");
    std::fs::create_dir_all(&session_dir).unwrap();

    std::fs::write(session_dir.join("baseline.json"), r#"{"tick":0,"entities":1,"tiles":0,"surfaces":["nauvis"]}"#).unwrap();

    let entities = vec![frame::Entity { n: "pipe".into(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1 }];
    let baseline = frame::FrameOut { tick: 0, surface: "nauvis", entities: &entities, tiles: &[], ..Default::default() };
    std::fs::write(session_dir.join("frame_0_nauvis.stfr"), frame::write_binary(&baseline)).unwrap();

    // One name and one surface up front, then a tick marker and an add per
    // tick. Written by hand rather than through the mod, which is the point of
    // a fixture: the reader is what is under test.
    let mut w = ByteWriter::new();
    w.magic(b"STE1").u8(1);
    w.u8(0).string("pipe");
    w.u8(1).string("nauvis");
    for tick in 1..=ticks {
        w.u8(2).u64(tick);
        let at = (tick as i32) * 10;
        w.u8(3).u16(0).i32(at).i32(at).u8(0).u8(1).u8(1).u64(tick).u16(0);
    }
    std::fs::write(session_dir.join("events_0.stev"), w.into_vec()).unwrap();

    (dir, session_dir)
}

/// Stamps `name`'s mtime, which is how `event::log_segments` recovers the order
/// segments were created in: higher `rank` is later, equal `rank` an exact tie.
///
/// Anchored to a fixed instant rather than `now()` so a tie is genuinely a tie.
/// Letting the filesystem timestamp two back-to-back writes would not work:
/// Windows' granularity is coarser than the gap between them.
pub fn set_mtime_rank(dir: &Path, name: &str, rank: u64) {
    let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + rank);
    std::fs::OpenOptions::new().write(true).open(dir.join(name)).unwrap().set_modified(when).unwrap();
}
