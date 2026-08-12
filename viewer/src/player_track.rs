//! Looking up where a tracked player was at a given tick, from the flat
//! sample list `player_log::read_jsonl` produces.

use std::collections::HashMap;

use save_timelapse::player_log::PlayerSample;

/// Where every tracked player was, as of whatever tick is displayed. Built once
/// at load; a linear scan per lookup is plenty, sample counts being tiny next
/// to entity counts.
pub struct PlayerTrack {
    /// Sorted by tick, ascending, once at construction rather than on every
    /// lookup.
    samples: Vec<PlayerSample>,
}

impl PlayerTrack {
    pub fn new(mut samples: Vec<PlayerSample>) -> Self {
        samples.sort_by_key(|s| s.tick);
        PlayerTrack { samples }
    }

    /// Each player's latest known position at or before `tick`, filtered to
    /// the surface they were last seen on: the same nearest-preceding
    /// semantics `World` uses for entity state. A player not yet sampled, or
    /// last seen elsewhere, is not returned.
    pub fn positions_at(&self, surface: &str, tick: u64) -> Vec<(&str, f32, f32)> {
        let mut latest: HashMap<&str, &PlayerSample> = HashMap::new();
        for sample in &self.samples {
            if sample.tick > tick {
                break;
            }
            latest.insert(&sample.name, sample);
        }
        latest.into_values().filter(|s| s.surface == surface).map(|s| (s.name.as_str(), s.x, s.y)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player_sample(tick: u64, name: &str, surface: &str, x: f32, y: f32) -> PlayerSample {
        PlayerSample { tick, name: name.to_string(), surface: surface.to_string(), x, y }
    }

    #[test]
    fn player_track_finds_no_one_before_the_first_sample() {
        let track = PlayerTrack::new(vec![player_sample(100, "Alice", "nauvis", 1.0, 2.0)]);
        assert!(track.positions_at("nauvis", 50).is_empty());
    }

    #[test]
    fn player_track_returns_the_exact_match_at_that_tick() {
        let track = PlayerTrack::new(vec![player_sample(100, "Alice", "nauvis", 1.0, 2.0)]);
        assert_eq!(track.positions_at("nauvis", 100), vec![("Alice", 1.0, 2.0)]);
    }

    #[test]
    fn player_track_uses_the_latest_sample_at_or_before_the_tick_not_a_later_one() {
        let track = PlayerTrack::new(vec![
            player_sample(100, "Alice", "nauvis", 1.0, 2.0),
            player_sample(200, "Alice", "nauvis", 9.0, 9.0),
        ]);
        assert_eq!(track.positions_at("nauvis", 150), vec![("Alice", 1.0, 2.0)], "150 is before the 200 sample");
    }

    #[test]
    fn player_track_only_shows_a_player_on_their_last_known_surface() {
        let track = PlayerTrack::new(vec![
            player_sample(100, "Alice", "nauvis", 1.0, 2.0),
            player_sample(200, "Alice", "vulcanus", 3.0, 4.0),
        ]);
        assert!(track.positions_at("nauvis", 250).is_empty(), "Alice last seen on vulcanus by tick 250");
        assert_eq!(track.positions_at("vulcanus", 250), vec![("Alice", 3.0, 4.0)]);
    }

    #[test]
    fn player_track_tracks_multiple_players_independently() {
        let track = PlayerTrack::new(vec![
            player_sample(100, "Alice", "nauvis", 1.0, 2.0),
            player_sample(100, "Bob", "nauvis", 5.0, 6.0),
        ]);
        let mut positions = track.positions_at("nauvis", 100);
        positions.sort_by_key(|(name, ..)| *name);
        assert_eq!(positions, vec![("Alice", 1.0, 2.0), ("Bob", 5.0, 6.0)]);
    }
}
