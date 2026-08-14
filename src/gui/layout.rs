//! Where things go, worked out without drawing any of them.
//!
//! Split from the drawing for the same reason `chrome::Chrome` is: hit testing
//! and painting have to agree about every rectangle, and the only way to be
//! sure they do is for both to read the same one. It also makes the arithmetic
//! testable, which matters more here than in the viewer's chrome, because a
//! menu is nothing but arithmetic.

/// A rectangle, in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// The baseline for text vertically centred in this rectangle, given the
    /// font size. Text is drawn from its baseline rather than its top, and
    /// getting this wrong is the difference between a button that reads as
    /// deliberate and one that looks a pixel out.
    pub fn text_baseline(&self, size: f32) -> f32 {
        self.y + (self.height + size * 0.7) / 2.0
    }
}

/// How wide the column of choices is allowed to get, as a share of the window
/// and as hard limits.
///
/// Bounded at both ends because neither extreme reads: full width on a wide
/// monitor puts the label and its note so far apart that they stop looking
/// related, and an unbounded minimum makes a narrow window clip its own text.
const COLUMN_SHARE: f32 = 0.44;
const COLUMN_MIN: f32 = 320.0;
const COLUMN_MAX: f32 = 620.0;

/// Space above the column, for the logo and the name.
const HEADER_HEIGHT: f32 = 132.0;

/// A centred stack of equal rows: the shape every screen here has.
///
/// Centred on the window rather than laid out from a corner, because that is
/// the one arrangement that looks deliberate at any size and needs no
/// breakpoints to get there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Column {
    pub x: f32,
    pub width: f32,
    pub top: f32,
    pub row_height: f32,
    pub gap: f32,
    pub rows: usize,
}

impl Column {
    pub fn centered(screen_width: f32, screen_height: f32, rows: usize, row_height: f32) -> Column {
        let width = (screen_width * COLUMN_SHARE).clamp(COLUMN_MIN, COLUMN_MAX).min(screen_width - 32.0).max(1.0);
        let gap = 10.0;
        let block = rows as f32 * row_height + rows.saturating_sub(1) as f32 * gap;

        // The header is part of what gets centred, so the logo and the choices
        // read as one thing rather than as a title with a list under it. Never
        // above the top edge: on a short window the block wins and the header
        // is what gets clipped, a menu you cannot click being worse than one
        // whose logo is cut off.
        let top = ((screen_height - block + HEADER_HEIGHT) / 2.0).max(HEADER_HEIGHT.min(screen_height * 0.25));

        Column { x: (screen_width - width) / 2.0, width, top, row_height, gap, rows }
    }

    pub fn row(&self, index: usize) -> Rect {
        Rect { x: self.x, y: self.top + index as f32 * (self.row_height + self.gap), width: self.width, height: self.row_height }
    }

    /// Which row `point` is over, or `None` for the gaps and everywhere else.
    /// Gaps deliberately hit nothing: a click that lands between two choices
    /// should do neither rather than guess.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        (0..self.rows).find(|&index| self.row(index).contains(x, y))
    }

    /// The centre of the space above the column, where a logo goes.
    pub fn header_center(&self, screen_width: f32) -> (f32, f32) {
        (screen_width / 2.0, self.top - HEADER_HEIGHT / 2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_is_centred_horizontally_at_any_width() {
        for screen in [640.0, 1280.0, 1920.0, 3840.0] {
            let column = Column::centered(screen, 900.0, 5, 44.0);
            let left = column.x;
            let right = screen - (column.x + column.width);
            assert!((left - right).abs() < 0.01, "off centre at {screen}: {left} against {right}");
        }
    }

    /// Neither extreme reads: full width pulls a label away from its note, and
    /// no minimum clips the text on a narrow window.
    #[test]
    fn the_column_width_is_bounded_at_both_ends() {
        assert_eq!(Column::centered(3840.0, 900.0, 5, 44.0).width, COLUMN_MAX);
        assert_eq!(Column::centered(600.0, 900.0, 5, 44.0).width, COLUMN_MIN);
    }

    /// A window narrower than the minimum column still has to fit inside
    /// itself, margin included, rather than running off both edges.
    #[test]
    fn a_window_narrower_than_the_minimum_still_fits() {
        let column = Column::centered(200.0, 900.0, 3, 44.0);
        assert!(column.x >= 0.0, "{column:?}");
        assert!(column.x + column.width <= 200.0, "{column:?}");
    }

    #[test]
    fn rows_are_evenly_spaced_and_do_not_overlap() {
        let column = Column::centered(1280.0, 900.0, 4, 44.0);
        for index in 1..4 {
            let previous = column.row(index - 1);
            let current = column.row(index);
            assert_eq!(current.y - (previous.y + previous.height), column.gap);
        }
    }

    /// The whole reason layout and drawing share one rectangle: what is hit is
    /// what was painted.
    #[test]
    fn every_row_is_hit_at_its_own_centre() {
        let column = Column::centered(1280.0, 900.0, 6, 44.0);
        for index in 0..6 {
            let row = column.row(index);
            assert_eq!(column.hit(row.x + row.width / 2.0, row.y + row.height / 2.0), Some(index));
        }
    }

    /// A click between two choices should do neither rather than guess.
    #[test]
    fn the_gap_between_rows_hits_nothing() {
        let column = Column::centered(1280.0, 900.0, 3, 44.0);
        let first = column.row(0);
        assert_eq!(column.hit(first.x + 10.0, first.y + first.height + column.gap / 2.0), None);
    }

    #[test]
    fn everything_outside_the_column_hits_nothing() {
        let column = Column::centered(1280.0, 900.0, 3, 44.0);
        let row = column.row(0);
        assert_eq!(column.hit(column.x - 1.0, row.y + 5.0), None, "left of it");
        assert_eq!(column.hit(column.x + column.width + 1.0, row.y + 5.0), None, "right of it");
        assert_eq!(column.hit(row.x + 5.0, column.top - 1.0), None, "above it");
        let last = column.row(2);
        assert_eq!(column.hit(row.x + 5.0, last.y + last.height + 1.0), None, "below it");
    }

    /// A window too short for everything gives up the header before it gives
    /// up the top of the list: a menu whose logo is cut off still works, one
    /// that has scrolled its first choice off the top does not.
    ///
    /// More rows than fit still overflow the bottom, which is honest for now
    /// and wants scrolling rather than shrinking, since rows that resize with
    /// the window are how a list becomes unreadable on a laptop.
    #[test]
    fn a_short_window_keeps_the_top_of_the_list_on_screen() {
        let column = Column::centered(1280.0, 300.0, 6, 44.0);
        assert!(column.top > 0.0, "{column:?}");
        assert!(column.top < 300.0 * 0.4, "the header must give way first: {column:?}");
        let first = column.row(0);
        assert!(first.y + first.height <= 300.0, "the first choice must be fully visible: {column:?}");
    }

    #[test]
    fn the_header_sits_above_the_column_and_is_horizontally_centred() {
        let column = Column::centered(1280.0, 900.0, 5, 44.0);
        let (x, y) = column.header_center(1280.0);
        assert_eq!(x, 640.0);
        assert!(y < column.top, "the header is above the choices: {y} against {}", column.top);
    }
}
