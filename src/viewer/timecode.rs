//! Turning game ticks into readable elapsed time. A tick is the only clock a
//! capture carries, so a frame can say how far into the save it is and never
//! when the player was sitting there, which is the right thing to show
//! anyway.

/// Factorio's update rate. Exact by definition rather than measured: ticks
/// are logical, kept deterministic for multiplayer, so a save that ran at
/// half speed on a struggling machine still advanced 60 ticks per in-game
/// second. Dividing by this converts to game time with no fudge factor.
pub const TICKS_PER_SECOND: u64 = 60;

const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;

/// `tick` as elapsed in-game time: `"4h 12m"`, or `"42m"` before the first
/// hour.
///
/// Seconds are dropped: frames are typically minutes of game time apart, so a
/// seconds field would change every frame while telling the viewer nothing.
///
/// Hours are not wrapped into days for the opposite reason, staying directly
/// comparable: `"312h 05m"` against `"104h 30m"` is roughly three times, where
/// `"13d 00h"` against `"4d 08h"` makes somebody stop and multiply.
pub fn format_game_time(tick: u64) -> String {
    let total_minutes = tick / TICKS_PER_SECOND / SECONDS_PER_MINUTE;
    let hours = total_minutes / MINUTES_PER_HOUR;
    let minutes = total_minutes % MINUTES_PER_HOUR;
    if hours == 0 {
        return format!("{minutes}m");
    }
    // Zero-padded only once there is an hours field in front of it, so the
    // minutes read as part of one duration rather than as a separate number.
    format!("{hours}h {minutes:02}m")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = TICKS_PER_SECOND * SECONDS_PER_MINUTE;
    const HOUR: u64 = MINUTE * MINUTES_PER_HOUR;

    #[test]
    fn a_fresh_save_reads_as_zero_rather_than_empty() {
        assert_eq!(format_game_time(0), "0m");
    }

    /// Everything below a minute collapses to "0m" rather than rounding up
    /// to "1m": a frame at tick 59 has not reached the first minute, and
    /// saying it has would put the first two frames of a capture on the same
    /// label for the wrong reason.
    #[test]
    fn part_of_a_minute_truncates_rather_than_rounding() {
        assert_eq!(format_game_time(1), "0m");
        assert_eq!(format_game_time(MINUTE - 1), "0m");
        assert_eq!(format_game_time(MINUTE), "1m");
    }

    #[test]
    fn under_an_hour_omits_the_hours_field_entirely() {
        assert_eq!(format_game_time(42 * MINUTE), "42m");
        assert_eq!(format_game_time(HOUR - 1), "59m");
    }

    #[test]
    fn an_exact_hour_still_shows_padded_minutes() {
        assert_eq!(format_game_time(HOUR), "1h 00m");
    }

    #[test]
    fn hours_and_minutes_combine() {
        assert_eq!(format_game_time(4 * HOUR + 12 * MINUTE), "4h 12m");
        assert_eq!(format_game_time(HOUR + 5 * MINUTE), "1h 05m");
    }

    /// Hours accumulate past a day instead of rolling over into one, so two
    /// long captures stay comparable by reading the hours field alone.
    #[test]
    fn hours_run_past_a_day_without_wrapping() {
        assert_eq!(format_game_time(312 * HOUR + 5 * MINUTE), "312h 05m");
    }

    /// The label is drawn every frame for several positions on the bar, so
    /// no tick may panic or wrap. The exact string is not meaningful here
    /// (it is some 85 trillion hours); that it divides down cleanly instead
    /// of overflowing is the point.
    #[test]
    fn an_absurd_tick_still_formats() {
        assert_eq!(format_game_time(u64::MAX), "85401592933840h 31m");
    }
}
