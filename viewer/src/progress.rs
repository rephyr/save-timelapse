//! On-screen progress reporting for the startup load.

/// Progress through the startup load, for the on-screen bar.
///
/// Loading is blocking work in front of a window that is already open, so
/// without this the viewer shows an empty frame for as long as it takes to
/// parse every frame file and load every sprite, which on a real save set
/// is many seconds with no indication anything is happening.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadProgress {
    pub phase: &'static str,
    pub detail: String,
    pub done: usize,
    pub total: usize,
}

impl LoadProgress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
    }
}

/// Geometry for the loading bar. Split from drawing for the same reason as
/// `Timeline`: this part is testable, macroquad calls are not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgressBar {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl ProgressBar {
    pub const HEIGHT: f32 = 18.0;

    pub fn centered(screen_width: f32, screen_height: f32) -> Self {
        let width = (screen_width * 0.5).max(1.0);
        ProgressBar {
            left: (screen_width - width) / 2.0,
            top: screen_height / 2.0 - Self::HEIGHT / 2.0,
            width,
            height: Self::HEIGHT,
        }
    }

    pub fn filled_width(&self, progress: &LoadProgress) -> f32 {
        self.width * progress.fraction()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_progress_fraction_is_bounded_and_safe_at_zero_total() {
        let at = |done, total| LoadProgress {
            phase: "frames",
            detail: String::new(),
            done,
            total,
        }
        .fraction();
        assert_eq!(at(0, 0), 0.0, "an empty job must not divide by zero");
        assert_eq!(at(0, 4), 0.0);
        assert_eq!(at(2, 4), 0.5);
        assert_eq!(at(4, 4), 1.0);
        assert_eq!(at(9, 4), 1.0, "overshoot clamps rather than overflowing the bar");
    }

    #[test]
    fn progress_bar_is_centered_and_fills_proportionally() {
        let bar = ProgressBar::centered(1000.0, 600.0);
        assert_eq!(bar.left + bar.width / 2.0, 500.0);
        assert_eq!(bar.top + bar.height / 2.0, 300.0);

        let half = LoadProgress { phase: "frames", detail: String::new(), done: 1, total: 2 };
        assert_eq!(bar.filled_width(&half), bar.width / 2.0);
    }
}
