//! Points worth jumping to, and the jumping itself. Three kinds, answering
//! different questions: milestones come from the capture, bookmarks are
//! whatever the viewer decided was worth returning to, and busy stretches are
//! derived from the activity data.
//!
//! Pure and index-based so it can be tested without a window, apart from
//! bookmark persistence.

use std::path::{Path, PathBuf};

/// A frame worth jumping to, already resolved to an index in the sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub frame: usize,
}

/// The next mark strictly after `from`, if any.
///
/// Strictly after, not at or after, so holding the key walks forward instead
/// of sticking on whichever mark the playhead already sits on.
pub fn next_mark(marks: &[usize], from: usize) -> Option<usize> {
    marks.iter().copied().filter(|&m| m > from).min()
}

/// The previous mark strictly before `from`, if any.
pub fn previous_mark(marks: &[usize], from: usize) -> Option<usize> {
    marks.iter().copied().filter(|&m| m < from).max()
}

/// Fraction of the busiest frame a frame has to reach to count as busy.
///
/// Low on purpose. The peak is usually one blueprint landing thousands of
/// entities at once, so a high threshold would find only that single moment
/// and call everything else quiet, which is useless for navigating.
const BUSY_FRACTION: f64 = 0.25;

/// The busiest frame of each stretch of sustained construction.
///
/// Consecutive busy frames collapse to one mark: a five minute building
/// session is one place somebody wants to go, not thirty. The mark lands on
/// the busiest frame within the stretch rather than its start.
///
/// Empty for a capture with no construction, which is also what stops the
/// threshold below dividing by zero.
pub fn busy_stretches(counts: &[usize]) -> Vec<usize> {
    let peak = counts.iter().copied().max().unwrap_or(0);
    if peak == 0 {
        return Vec::new();
    }
    let threshold = (peak as f64 * BUSY_FRACTION).ceil() as usize;

    let mut marks = Vec::new();
    let mut best: Option<(usize, usize)> = None; // (frame, count) within the current stretch
    for (frame, &count) in counts.iter().enumerate() {
        if count >= threshold {
            if best.is_none_or(|(_, c)| count > c) {
                best = Some((frame, count));
            }
        } else if let Some((frame, _)) = best.take() {
            marks.push(frame);
        }
    }
    // A stretch running to the very end never sees a quiet frame to close it.
    if let Some((frame, _)) = best {
        marks.push(frame);
    }
    marks
}

/// Where a timelapse's bookmarks are kept, beside its frames.
///
/// One tick per line, plain text. A list of integers does not justify pulling
/// a JSON dependency into the viewer, which has none, and this stays readable
/// and editable by hand like the player and milestone logs beside it.
pub fn bookmarks_path(dir: &Path) -> PathBuf {
    dir.join("bookmarks.txt")
}

/// Bookmarks are stored as ticks, not frame indices: a rebuild at a different
/// seconds-per-frame renumbers every index, where a tick is a fact about the
/// playthrough and survives re-exporting. Unparseable lines are skipped, so a
/// hand-edited file with a stray blank line still works.
pub fn read_bookmarks(dir: &Path) -> Vec<u64> {
    let Ok(text) = std::fs::read_to_string(bookmarks_path(dir)) else { return Vec::new() };
    let mut ticks: Vec<u64> = text.lines().filter_map(|line| line.trim().parse().ok()).collect();
    ticks.sort_unstable();
    ticks.dedup();
    ticks
}

/// Writes the bookmarks, reporting failure without treating it as fatal:
/// losing a bookmark is not a reason to interrupt somebody watching a
/// timelapse.
pub fn write_bookmarks(dir: &Path, ticks: &[u64]) {
    let body: String = ticks.iter().map(|tick| format!("{tick}\n")).collect();
    if let Err(e) = std::fs::write(bookmarks_path(dir), body) {
        eprintln!("warning: could not save bookmarks: {e}");
    }
}

/// Resolves ticks to frames that exist, a bookmarked tick needing no frame of
/// its own. Nearest rather than the next one after: a bookmark half a frame
/// past where somebody set it should return to the frame they were looking
/// at.
pub fn frames_for_ticks(ticks: &[u64], frame_ticks: &[u64]) -> Vec<usize> {
    if frame_ticks.is_empty() {
        return Vec::new();
    }
    let mut frames: Vec<usize> = ticks
        .iter()
        .map(|&tick| {
            frame_ticks
                .iter()
                .enumerate()
                .min_by_key(|(_, &frame_tick)| frame_tick.abs_diff(tick))
                .map(|(index, _)| index)
                .unwrap_or(0)
        })
        .collect();
    frames.sort_unstable();
    frames.dedup();
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jumping_moves_strictly_past_where_it_started() {
        let marks = [2usize, 5, 9];
        assert_eq!(next_mark(&marks, 0), Some(2));
        // Standing on a mark moves to the next one rather than sticking.
        assert_eq!(next_mark(&marks, 2), Some(5));
        assert_eq!(next_mark(&marks, 9), None, "nothing past the last");

        assert_eq!(previous_mark(&marks, 9), Some(5));
        assert_eq!(previous_mark(&marks, 2), None);
    }

    #[test]
    fn jumping_works_on_unsorted_marks() {
        let marks = [9usize, 2, 5];
        assert_eq!(next_mark(&marks, 3), Some(5));
        assert_eq!(previous_mark(&marks, 8), Some(5));
    }

    /// One building session is one place to go, not one per frame in it.
    #[test]
    fn a_run_of_busy_frames_collapses_to_a_single_mark() {
        let counts = [0usize, 0, 80, 100, 90, 0, 0];
        assert_eq!(busy_stretches(&counts), vec![3], "the busiest frame of the one stretch");
    }

    #[test]
    fn separate_sessions_get_separate_marks() {
        let counts = [100usize, 0, 0, 0, 80, 0];
        assert_eq!(busy_stretches(&counts), vec![0, 4]);
    }

    /// A stretch running to the last frame has no quiet frame after it to
    /// close it, which is exactly the off-by-one this guards.
    #[test]
    fn a_stretch_that_reaches_the_end_still_gets_a_mark() {
        let counts = [0usize, 0, 50, 100];
        assert_eq!(busy_stretches(&counts), vec![3]);
    }

    #[test]
    fn quiet_frames_are_not_marks() {
        // 10 is under a quarter of the 100 peak.
        let counts = [100usize, 10, 10, 10];
        assert_eq!(busy_stretches(&counts), vec![0]);
    }

    #[test]
    fn a_capture_with_no_construction_has_no_busy_stretches() {
        assert!(busy_stretches(&[0, 0, 0]).is_empty());
        assert!(busy_stretches(&[]).is_empty());
    }

    /// The reason bookmarks are stored as ticks: a rebuild at a different
    /// seconds-per-frame renumbers every index, and a bookmark has to keep
    /// meaning the same moment of the playthrough.
    #[test]
    fn bookmarked_ticks_resolve_to_the_nearest_frame_that_exists() {
        let frame_ticks = [0u64, 3600, 7200, 10800];
        // Exactly on a frame, between two, and past the end.
        assert_eq!(frames_for_ticks(&[3600], &frame_ticks), vec![1]);
        assert_eq!(frames_for_ticks(&[4000], &frame_ticks), vec![1], "nearest, not next");
        assert_eq!(frames_for_ticks(&[6000], &frame_ticks), vec![2]);
        assert_eq!(frames_for_ticks(&[99999], &frame_ticks), vec![3]);
    }

    #[test]
    fn two_bookmarks_landing_on_one_frame_become_one_mark() {
        let frame_ticks = [0u64, 3600];
        assert_eq!(frames_for_ticks(&[3500, 3600, 3700], &frame_ticks), vec![1]);
    }

    #[test]
    fn bookmarks_round_trip_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        write_bookmarks(dir.path(), &[7200, 3600]);
        assert_eq!(read_bookmarks(dir.path()), vec![3600, 7200], "sorted on the way back");
    }

    /// A timelapse with no bookmarks file is the ordinary case, and a corrupt
    /// one is not worth refusing to open a timelapse over.
    #[test]
    fn missing_or_unreadable_bookmarks_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_bookmarks(dir.path()).is_empty());

        std::fs::write(bookmarks_path(dir.path()), b"not a number\n\n").unwrap();
        assert!(read_bookmarks(dir.path()).is_empty());
    }

    /// Hand editable is part of the point of a plain text list, so a stray
    /// blank line or a note to self must not throw away the real entries.
    #[test]
    fn a_hand_edited_file_keeps_the_lines_that_are_ticks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(bookmarks_path(dir.path()), b"3600\n\n  7200  \nthe rocket\n").unwrap();
        assert_eq!(read_bookmarks(dir.path()), vec![3600, 7200]);
    }
}
