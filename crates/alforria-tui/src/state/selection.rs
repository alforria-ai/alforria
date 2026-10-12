//! Mouse text selection (`util/selection.ts` + OpenTUI's renderer
//! selection). A left-button drag selects screen cells in reading order,
//! like a terminal; releasing copies the text and clears the selection
//! (copy-on-select). With `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT`
//! the selection stays until ctrl+c / ctrl+y copies it, Esc or any other
//! key dismisses it, or a right click copies it.
//!
//! OpenTUI selects inside renderables; the port selects what is on
//! screen, which reads the same for transcript text but can span panes.

use ratatui::buffer::Buffer;

/// Screen cell `(column, row)`.
pub type Cell = (u16, u16);

#[derive(Debug, Default)]
pub struct Selection {
    anchor: Option<Cell>,
    focus: Option<Cell>,
    /// The frame on screen while a selection is possible — what a copy
    /// reads.
    pub screen: Option<Buffer>,
}

/// `Flag.OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT`.
pub fn copy_on_select_disabled_from_env() -> bool {
    std::env::var("OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "true" | "1"))
        .unwrap_or(false)
}

impl Selection {
    /// A left press: a possible selection starts here.
    pub fn begin(&mut self, cell: Cell) {
        self.anchor = Some(cell);
        self.focus = None;
    }

    /// A left drag.
    pub fn extend(&mut self, cell: Cell) {
        if self.anchor.is_some() {
            self.focus = Some(cell);
        }
    }

    /// Whether the frame should be kept for a copy.
    pub fn tracking(&self) -> bool {
        self.anchor.is_some()
    }

    /// A drag moved off the press cell: there is a selection.
    pub fn active(&self) -> bool {
        matches!((self.anchor, self.focus), (Some(anchor), Some(focus)) if anchor != focus)
    }

    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.screen = None;
    }

    /// The selected span, start before end in reading order.
    pub fn range(&self) -> Option<(Cell, Cell)> {
        if !self.active() {
            return None;
        }
        let (anchor, focus) = (self.anchor?, self.focus?);
        let key = |(column, row): Cell| (row, column);
        Some(if key(anchor) <= key(focus) {
            (anchor, focus)
        } else {
            (focus, anchor)
        })
    }

    /// Whether `(column, row)` is selected.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        let Some(((start_col, start_row), (end_col, end_row))) = self.range() else {
            return false;
        };
        if row < start_row || row > end_row {
            return false;
        }
        (row > start_row || column >= start_col) && (row < end_row || column <= end_col)
    }

    /// The selected text of the kept frame: each row's cells in order
    /// (wide glyphs once), trailing blanks trimmed, rows joined by `\n`.
    pub fn text(&self) -> Option<String> {
        let ((start_col, start_row), (end_col, end_row)) = self.range()?;
        let screen = self.screen.as_ref()?;
        let area = screen.area;
        let mut lines = Vec::new();
        for row in start_row..=end_row.min(area.bottom().saturating_sub(1)) {
            let first = if row == start_row {
                start_col
            } else {
                area.left()
            };
            let last = if row == end_row {
                end_col.min(area.right().saturating_sub(1))
            } else {
                area.right().saturating_sub(1)
            };
            let mut line = String::new();
            let mut column = first;
            while column <= last {
                let symbol = screen[(column, row)].symbol();
                line.push_str(symbol);
                let width = unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
                column = column.saturating_add(width);
            }
            lines.push(line.trim_end().to_string());
        }
        let text = lines.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    /// Paint the selection over a drawn frame (reversed cells).
    pub fn paint(&self, buffer: &mut Buffer) {
        if self.range().is_none() {
            return;
        }
        let area = buffer.area;
        for row in area.top()..area.bottom() {
            for column in area.left()..area.right() {
                if self.contains(column, row) {
                    let cell = &mut buffer[(column, row)];
                    cell.modifier.insert(ratatui::style::Modifier::REVERSED);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn screen(lines: &[&str]) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, lines.len() as u16));
        for (row, line) in lines.iter().enumerate() {
            buffer.set_string(0, row as u16, line, ratatui::style::Style::new());
        }
        buffer
    }

    #[test]
    fn a_drag_selects_in_reading_order() {
        let mut selection = Selection::default();
        selection.begin((6, 1));
        assert!(!selection.active(), "a press alone selects nothing");
        selection.extend((2, 0));
        selection.screen = Some(screen(&["hello world", "second line", "third"]));
        assert_eq!(selection.range(), Some(((2, 0), (6, 1))));
        assert_eq!(selection.text().as_deref(), Some("llo world\nsecond"));
        assert!(selection.contains(11, 0) && !selection.contains(7, 1));
    }

    #[test]
    fn wide_glyphs_copy_once() {
        let mut selection = Selection::default();
        selection.begin((0, 0));
        selection.extend((5, 0));
        selection.screen = Some(screen(&["思考 ok"]));
        assert_eq!(selection.text().as_deref(), Some("思考 o"));
    }
}
