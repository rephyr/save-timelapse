//! Helpers shared by more than one module's tests.

use std::path::Path;
use std::time::SystemTime;

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
