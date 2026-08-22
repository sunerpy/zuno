//! Application-owned text selection shared by scrollable text surfaces.
//!
//! Coordinates are content-relative rather than frame-relative. A selection therefore
//! survives scrolling and can be painted by any surface that knows its current viewport.

/// One terminal cell in scrollable content coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TextPoint {
    pub(crate) row: usize,
    pub(crate) column: u16,
}

/// A drag selection with an anchor and a moving head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextSelection {
    pub(crate) anchor: TextPoint,
    pub(crate) head: TextPoint,
}

impl TextSelection {
    pub(crate) fn ordered(self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Selected columns on `row`, with an exclusive end.
    pub(crate) fn columns(self, row: usize, width: u16) -> Option<(u16, u16)> {
        let (start, end) = self.ordered();
        if row < start.row || row > end.row || width == 0 {
            return None;
        }
        let last = width.saturating_sub(1);
        let start_column = start.column.min(last);
        let end_column = end.column.min(last);
        let (left, right) = if start.row == end.row {
            (start_column, end_column.saturating_add(1))
        } else if row == start.row {
            (start_column, width)
        } else if row == end.row {
            (0, end_column.saturating_add(1))
        } else {
            (0, width)
        };
        (left < right).then_some((left, right.min(width)))
    }
}

/// The characters whose terminal cells overlap `[left, right)`.
pub(crate) fn slice_columns(text: &str, left: u16, right: u16) -> String {
    let left = usize::from(left);
    let right = usize::from(right);
    let mut column = 0usize;
    let mut out = String::new();
    let mut selected_previous = false;
    for character in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            if selected_previous {
                out.push(character);
            }
            continue;
        }
        let end = column.saturating_add(width);
        selected_previous = column < right && end > left;
        if selected_previous {
            out.push(character);
        }
        column = end;
        if column >= right {
            break;
        }
    }
    out
}
