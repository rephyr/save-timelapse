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

/// What the header shrinks to when the choices need the room. Enough for the
/// screen's own title, which is the part that says where you are.
const HEADER_MIN: f32 = 56.0;

/// Kept clear below the last row, so a list that scrolls does not end flush
/// against the edge and look like it has more under it.
const BOTTOM_MARGIN: f32 = 20.0;

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
    /// How far the rows are shifted up, and how far they can be.
    ///
    /// Scrolling rather than shrinking, because rows that resize with the
    /// window are how a list becomes unreadable on a laptop. Zero whenever
    /// everything fits, so the common case is exactly as it was.
    pub scroll: f32,
    pub max_scroll: f32,
    /// Rows are only drawn and only hit between these. Without it a scrolled
    /// row would paint over the title and still answer to a click.
    pub view_top: f32,
    pub view_bottom: f32,
}

impl Column {
    /// `scroll` is how far the caller wants the rows shifted up. It is clamped
    /// here rather than trusted, so a wheel spun past the end stops at it.
    pub fn centered(screen_width: f32, screen_height: f32, rows: usize, row_height: f32, scroll: f32) -> Column {
        let width = (screen_width * COLUMN_SHARE).clamp(COLUMN_MIN, COLUMN_MAX).min(screen_width - 32.0).max(1.0);
        let gap = 10.0;
        let block = rows as f32 * row_height + rows.saturating_sub(1) as f32 * gap;
        let x = (screen_width - width) / 2.0;

        // Everything fits: centre the header and the choices together, so they
        // read as one thing rather than as a title with a list under it.
        let room = screen_height - HEADER_HEIGHT - BOTTOM_MARGIN;
        if block <= room {
            let top = (screen_height - block + HEADER_HEIGHT) / 2.0;
            return Column {
                x,
                width,
                top,
                row_height,
                gap,
                rows,
                scroll: 0.0,
                max_scroll: 0.0,
                view_top: 0.0,
                view_bottom: screen_height,
            };
        }

        // It does not fit. The header gives up its room first, and whatever is
        // still over hangs off the bottom and is reached by scrolling: a list
        // that cannot reach its own Back row is a screen with no way out.
        let view_top = HEADER_MIN;
        let view_bottom = screen_height - BOTTOM_MARGIN;
        let max_scroll = (block - (view_bottom - view_top)).max(0.0);
        let scroll = scroll.clamp(0.0, max_scroll);

        Column { x, width, top: view_top - scroll, row_height, gap, rows, scroll, max_scroll, view_top, view_bottom }
    }

    /// Whether `index` is far enough on screen to be worth drawing and safe to
    /// click. Partly visible counts: half a row is still a row somebody can
    /// see and aim at.
    pub fn visible(&self, index: usize) -> bool {
        let row = self.row(index);
        row.y + row.height > self.view_top && row.y < self.view_bottom
    }

    pub fn row(&self, index: usize) -> Rect {
        Rect { x: self.x, y: self.top + index as f32 * (self.row_height + self.gap), width: self.width, height: self.row_height }
    }

    /// Which row `point` is over, or `None` for the gaps and everywhere else.
    /// Gaps deliberately hit nothing: a click that lands between two choices
    /// should do neither rather than guess.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        if y < self.view_top || y >= self.view_bottom {
            return None;
        }
        (0..self.rows).find(|&index| self.visible(index) && self.row(index).contains(x, y))
    }

    /// The centre of the space above the column, where a logo goes. Follows
    /// the header's real height, which shrinks when the choices need the room.
    pub fn header_center(&self, screen_width: f32) -> (f32, f32) {
        match self.max_scroll > 0.0 {
            true => (screen_width / 2.0, self.view_top / 2.0),
            false => (screen_width / 2.0, self.top - HEADER_HEIGHT / 2.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_is_centred_horizontally_at_any_width() {
        for screen in [640.0, 1280.0, 1920.0, 3840.0] {
            let column = Column::centered(screen, 900.0, 5, 44.0, 0.0);
            let left = column.x;
            let right = screen - (column.x + column.width);
            assert!((left - right).abs() < 0.01, "off centre at {screen}: {left} against {right}");
        }
    }

    /// Neither extreme reads: full width pulls a label away from its note, and
    /// no minimum clips the text on a narrow window.
    #[test]
    fn the_column_width_is_bounded_at_both_ends() {
        assert_eq!(Column::centered(3840.0, 900.0, 5, 44.0, 0.0).width, COLUMN_MAX);
        assert_eq!(Column::centered(600.0, 900.0, 5, 44.0, 0.0).width, COLUMN_MIN);
    }

    /// A window narrower than the minimum column still has to fit inside
    /// itself, margin included, rather than running off both edges.
    #[test]
    fn a_window_narrower_than_the_minimum_still_fits() {
        let column = Column::centered(200.0, 900.0, 3, 44.0, 0.0);
        assert!(column.x >= 0.0, "{column:?}");
        assert!(column.x + column.width <= 200.0, "{column:?}");
    }

    #[test]
    fn rows_are_evenly_spaced_and_do_not_overlap() {
        let column = Column::centered(1280.0, 900.0, 4, 44.0, 0.0);
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
        let column = Column::centered(1280.0, 900.0, 6, 44.0, 0.0);
        for index in 0..6 {
            let row = column.row(index);
            assert_eq!(column.hit(row.x + row.width / 2.0, row.y + row.height / 2.0), Some(index));
        }
    }

    /// A click between two choices should do neither rather than guess.
    #[test]
    fn the_gap_between_rows_hits_nothing() {
        let column = Column::centered(1280.0, 900.0, 3, 44.0, 0.0);
        let first = column.row(0);
        assert_eq!(column.hit(first.x + 10.0, first.y + first.height + column.gap / 2.0), None);
    }

    #[test]
    fn everything_outside_the_column_hits_nothing() {
        let column = Column::centered(1280.0, 900.0, 3, 44.0, 0.0);
        let row = column.row(0);
        assert_eq!(column.hit(column.x - 1.0, row.y + 5.0), None, "left of it");
        assert_eq!(column.hit(column.x + column.width + 1.0, row.y + 5.0), None, "right of it");
        assert_eq!(column.hit(row.x + 5.0, column.top - 1.0), None, "above it");
        let last = column.row(2);
        assert_eq!(column.hit(row.x + 5.0, last.y + last.height + 1.0), None, "below it");
    }

    /// A window too short for everything gives up the header before it gives
    /// up the top of the list.
    #[test]
    fn a_short_window_keeps_the_top_of_the_list_on_screen() {
        let column = Column::centered(1280.0, 300.0, 6, 44.0, 0.0);
        assert!(column.top > 0.0, "{column:?}");
        assert!(column.top < 300.0 * 0.4, "the header must give way first: {column:?}");
        let first = column.row(0);
        assert!(first.y + first.height <= 300.0, "the first choice must be fully visible: {column:?}");
    }

    /// The bug this exists for: a window too short for the list buried the
    /// last row, and the last row is Back. A screen with no way out is worse
    /// than one that has to be scrolled.
    #[test]
    fn the_last_row_can_always_be_reached_by_scrolling() {
        let rows = 12;
        let column = Column::centered(1280.0, 300.0, rows, 44.0, 0.0);
        assert!(column.max_scroll > 0.0, "this window cannot hold twelve rows: {column:?}");
        assert!(!column.visible(rows - 1), "the last row starts off screen, which is the problem");

        let scrolled = Column::centered(1280.0, 300.0, rows, 44.0, column.max_scroll);
        let last = scrolled.row(rows - 1);
        assert!(scrolled.visible(rows - 1), "scrolled to the end it must be there: {scrolled:?}");
        assert!(last.y + last.height <= 300.0, "and fully on screen: {last:?}");
        assert_eq!(scrolled.hit(last.x + 10.0, last.y + last.height / 2.0), Some(rows - 1), "and clickable");
    }

    /// Nothing to scroll when everything fits, so the ordinary screen behaves
    /// exactly as it did before scrolling existed.
    #[test]
    fn a_list_that_fits_does_not_scroll_at_all() {
        let column = Column::centered(1280.0, 900.0, 5, 44.0, 0.0);
        assert_eq!(column.max_scroll, 0.0);
        assert_eq!(column.scroll, 0.0);
        assert!((0..5).all(|index| column.visible(index)));

        // And asking to scroll one that fits changes nothing.
        let asked = Column::centered(1280.0, 900.0, 5, 44.0, 500.0);
        assert_eq!(asked.top, column.top);
    }

    /// Spinning the wheel past either end stops there rather than counting up
    /// invisibly and needing to be spun all the way back.
    #[test]
    fn scrolling_is_clamped_to_both_ends() {
        let column = Column::centered(1280.0, 300.0, 12, 44.0, 99_999.0);
        assert_eq!(column.scroll, column.max_scroll);
        let back = Column::centered(1280.0, 300.0, 12, 44.0, -500.0);
        assert_eq!(back.scroll, 0.0);
    }

    /// A row scrolled above the list would otherwise paint over the title and
    /// still answer to a click on it.
    #[test]
    fn a_row_scrolled_off_the_top_is_neither_drawn_nor_clickable() {
        let column = Column::centered(1280.0, 300.0, 12, 44.0, 200.0);
        assert!(!column.visible(0), "{column:?}");
        let first = column.row(0);
        assert_eq!(column.hit(first.x + 10.0, first.y + first.height / 2.0), None);
    }

    #[test]
    fn the_header_sits_above_the_column_and_is_horizontally_centred() {
        let column = Column::centered(1280.0, 900.0, 5, 44.0, 0.0);
        let (x, y) = column.header_center(1280.0);
        assert_eq!(x, 640.0);
        assert!(y < column.top, "the header is above the choices: {y} against {}", column.top);
    }
}
