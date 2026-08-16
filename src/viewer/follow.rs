//! Which surface a following export shows at each moment.
//!
//! A timelapse can hold several surfaces, and until now they were separate
//! videos: one per planet, each running the whole length of the playthrough
//! with nothing on it for the hours nobody was there. Following instead makes
//! one video that goes where the player went.
//!
//! Kept apart from the render loop because the rule is the whole feature and
//! none of it needs a GPU to decide.

use crate::viewer::player_track::PlayerTrack;

/// The clock a following export runs on: every moment any surface has a frame
/// for, ascending and deduplicated.
///
/// The union rather than one surface's own ticks, because a surface is not
/// written at a moment nothing changed on it, so any single surface's list has
/// holes exactly where another one was busy.
pub fn shared_ticks(per_surface: &[&[u64]]) -> Vec<u64> {
    let mut ticks: Vec<u64> = per_surface.iter().flat_map(|s| s.iter().copied()).collect();
    ticks.sort_unstable();
    ticks.dedup();
    ticks
}

/// The frame at or before `tick`, for a surface whose own frames are sparser
/// than the shared clock.
///
/// Before a surface's first frame there is nothing to show, so its first frame
/// stands in. That only happens on a surface the player reached later, and its
/// first frame is the moment it had anything on it at all.
pub fn frame_at(ticks: &[u64], tick: u64) -> usize {
    match ticks.binary_search(&tick) {
        Ok(exact) => exact,
        Err(0) => 0,
        Err(after) => after - 1,
    }
}

/// Which surface to show at each moment of `ticks`, as indices into
/// `surfaces`.
///
/// The rule, in the order it is applied: show the surface the player is on;
/// if that surface is not in this timelapse, keep showing the last one that
/// was. So a trip to a planet nobody chose to record does not cut to black or
/// jump somewhere arbitrary, it simply stays where it was until they come
/// back.
///
/// `opening` is what to show before the player has been sampled anywhere,
/// which is the start of every recording and the whole of any that has no
/// player log at all.
pub fn schedule(ticks: &[u64], surfaces: &[String], track: &PlayerTrack, opening: usize) -> Vec<usize> {
    let mut showing = opening;
    ticks
        .iter()
        .map(|&tick| {
            // Only a surface this timelapse actually holds may move the
            // camera. Anything else leaves `showing` alone, which is the
            // "stay where you were" half of the rule.
            if let Some(name) = track.surface_at(tick) {
                if let Some(index) = surfaces.iter().position(|s| s == name) {
                    showing = index;
                }
            }
            showing
        })
        .collect()
}

/// Where the camera changes surface, as `(index into the schedule, surface)`.
///
/// Only the moves, not one entry per frame: this is for telling somebody what
/// their export is going to do before it spends ten minutes doing it.
pub fn moves(schedule: &[usize]) -> Vec<(usize, usize)> {
    let mut moves = Vec::new();
    let mut last = None;
    for (at, &surface) in schedule.iter().enumerate() {
        if last != Some(surface) {
            moves.push((at, surface));
            last = Some(surface);
        }
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player_log::PlayerSample;

    fn sample(tick: u64, name: &str, surface: &str) -> PlayerSample {
        PlayerSample { tick, name: name.to_string(), surface: surface.to_string(), x: 0.0, y: 0.0 }
    }

    fn places(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_shared_clock_is_every_moment_any_surface_has_one() {
        let nauvis: &[u64] = &[0, 10, 20];
        let vulcanus: &[u64] = &[10, 30];
        assert_eq!(shared_ticks(&[nauvis, vulcanus]), vec![0, 10, 20, 30]);
    }

    /// A surface is only written when something on it changed, so the shared
    /// clock asks for moments it has no frame of its own for.
    #[test]
    fn a_surface_holds_its_last_frame_between_its_own() {
        let ticks = [10, 20, 40];
        assert_eq!(frame_at(&ticks, 10), 0);
        assert_eq!(frame_at(&ticks, 15), 0);
        assert_eq!(frame_at(&ticks, 20), 1);
        assert_eq!(frame_at(&ticks, 39), 1);
        assert_eq!(frame_at(&ticks, 400), 2);
    }

    /// Before a surface exists at all there is nothing truthful to show, and
    /// its own first frame is the least wrong answer available.
    #[test]
    fn a_moment_before_a_surface_began_shows_its_first_frame() {
        assert_eq!(frame_at(&[100, 200], 5), 0);
    }

    #[test]
    fn the_camera_goes_where_the_player_goes() {
        let track = PlayerTrack::new(vec![sample(0, "Alice", "nauvis"), sample(100, "Alice", "vulcanus")]);
        let schedule = schedule(&[0, 50, 100, 150], &places(&["nauvis", "vulcanus"]), &track, 0);
        assert_eq!(schedule, vec![0, 0, 1, 1]);
    }

    /// The half of the rule that is easy to get wrong: a planet nobody chose
    /// to record must not move the camera, and must not blank it either.
    #[test]
    fn a_surface_left_out_of_the_timelapse_leaves_the_camera_where_it_was() {
        let track = PlayerTrack::new(vec![
            sample(0, "Alice", "nauvis"),
            sample(100, "Alice", "fulgora"),
            sample(200, "Alice", "vulcanus"),
        ]);
        // Fulgora was recorded but not chosen for this timelapse.
        let schedule = schedule(&[0, 100, 150, 200], &places(&["nauvis", "vulcanus"]), &track, 0);
        assert_eq!(schedule, vec![0, 0, 0, 1], "fulgora holds on nauvis, then vulcanus takes over");
    }

    #[test]
    fn coming_back_returns_the_camera() {
        let track = PlayerTrack::new(vec![
            sample(0, "Alice", "nauvis"),
            sample(100, "Alice", "vulcanus"),
            sample(200, "Alice", "nauvis"),
        ]);
        let schedule = schedule(&[0, 100, 200], &places(&["nauvis", "vulcanus"]), &track, 0);
        assert_eq!(schedule, vec![0, 1, 0]);
    }

    /// Every tick before the first sample, and every tick of a recording that
    /// has no player log at all, which is what a terrain-only capture is.
    #[test]
    fn nothing_known_about_the_player_shows_the_opening_surface() {
        let track = PlayerTrack::new(vec![sample(500, "Alice", "vulcanus")]);
        assert_eq!(schedule(&[0, 100, 500], &places(&["nauvis", "vulcanus"]), &track, 0), vec![0, 0, 1]);

        let empty = PlayerTrack::new(vec![]);
        assert_eq!(schedule(&[0, 100], &places(&["nauvis"]), &empty, 0), vec![0, 0]);
    }

    /// A camera can only be in one place. Following whoever appears first
    /// keeps it from swapping every time a different player changes planet.
    #[test]
    fn one_player_is_followed_rather_than_whoever_moved_last() {
        let track =
            PlayerTrack::new(vec![sample(0, "Alice", "nauvis"), sample(100, "Bob", "vulcanus"), sample(200, "Bob", "vulcanus")]);
        let schedule = schedule(&[0, 100, 200], &places(&["nauvis", "vulcanus"]), &track, 0);
        assert_eq!(schedule, vec![0, 0, 0], "Bob travelling does not move a camera following Alice");
    }

    #[test]
    fn the_moves_are_the_changes_not_every_frame() {
        assert_eq!(moves(&[0, 0, 1, 1, 1, 0]), vec![(0, 0), (2, 1), (5, 0)]);
        assert_eq!(moves(&[]), vec![]);
    }
}
