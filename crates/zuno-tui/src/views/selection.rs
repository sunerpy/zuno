//! Application-owned text selection shared by scrollable text surfaces.
//!
//! Coordinates are content-relative rather than frame-relative. A selection therefore
//! survives scrolling and can be painted by any surface that knows its current viewport.

use unicode_segmentation::UnicodeSegmentation;

/// A row's selectable text, emitted together with its visible spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopyRow {
    pub content_start: u16,
    pub text: String,
    /// A separator crossed between visual rows, never prepended to a partial selection.
    pub join_before: String,
}

impl CopyRow {
    pub(crate) fn visible(text: String) -> Self {
        Self {
            content_start: 0,
            text,
            join_before: "\n".to_owned(),
        }
    }
}

/// Align already-rendered text with the renderer's logical text (not Markdown source).
/// Matching whole strings avoids assigning an invisible wrapping space to column zero
/// of the next row. Newline and space have the same layout width in inline prose.
pub(crate) fn copy_projections(logical: &str, rendered: &[String]) -> Vec<CopyRow> {
    let display = logical.replace('\n', " ");
    let mut cursor = 0;
    rendered
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if row.is_empty() {
                if logical[cursor..].starts_with('\n') {
                    cursor += 1;
                }
                return CopyRow {
                    content_start: 0,
                    text: "\n".to_owned(),
                    join_before: String::new(),
                };
            }
            let Some(relative) = display[cursor..].find(row) else {
                // Unmatched content is still copied as drawn, never guessed from source columns.
                return CopyRow::visible(row.clone());
            };
            let start = cursor + relative;
            let end = start + row.len();
            let gap = &logical[cursor..start];
            let join_before = if index == 0 {
                "\n".to_owned()
            } else if gap.chars().all(char::is_whitespace) {
                gap.to_owned()
            } else {
                "\n".to_owned()
            };
            cursor = end;
            CopyRow {
                content_start: 0,
                text: logical[start..end].to_owned(),
                join_before,
            }
        })
        .collect()
}

pub(crate) fn semantic_width(text: &str) -> usize {
    text.graphemes(true)
        .map(|cluster| {
            if cluster == "\n" {
                1
            } else {
                super::display_width(cluster)
            }
        })
        .sum()
}

/// The displayed columns covered by complete grapheme clusters.
pub(crate) fn covered_columns(text: &str, left: u16, right: u16) -> Option<(u16, u16)> {
    let mut column = 0usize;
    let mut covered: Option<(usize, usize)> = None;
    for cluster in text.graphemes(true) {
        let end = column
            + if cluster == "\n" {
                1
            } else {
                super::display_width(cluster)
            };
        if column < usize::from(right) && end > usize::from(left) {
            let range = covered.get_or_insert((column, end));
            range.1 = end;
        }
        column = end;
    }
    covered.map(|(left, right)| {
        (
            u16::try_from(left).unwrap_or(u16::MAX),
            u16::try_from(right).unwrap_or(u16::MAX),
        )
    })
}

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
    for cluster in text.graphemes(true) {
        let width = if cluster == "\n" {
            1
        } else {
            super::display_width(cluster)
        };
        if width == 0 {
            if selected_previous {
                out.push_str(cluster);
            }
            continue;
        }
        let end = column.saturating_add(width);
        selected_previous = column < right && end > left;
        if selected_previous {
            out.push_str(cluster);
        }
        column = end;
        if column >= right {
            break;
        }
    }
    out
}
