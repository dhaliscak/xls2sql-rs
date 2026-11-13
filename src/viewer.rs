//! Virtualized read-only text viewer for very large outputs.
//!
//! egui's `TextEdit` lays out the whole text every frame, which is unusable for
//! multi-megabyte SQL scripts. This viewer hard-wraps lines into fixed-width rows
//! and only lays out the rows currently visible in the scroll area.
//!
//! The text is kept as the generator's ordered parts (no line spans two parts), so
//! showing a result never needs a second, joined copy of a multi-hundred-MB script.

use eframe::egui;
use std::ops::Range;

pub struct OutputView {
    parts: Vec<String>,
    /// Logical lines (without line terminators): part index and byte range in it.
    lines: Vec<(usize, Range<usize>)>,
    /// Columns (bytes) per visual row the row index was built for.
    cols: usize,
    /// `row_starts[i]` = first visual row of line `i`; last element = total rows.
    row_starts: Vec<usize>,
}

impl OutputView {
    /// `parts` concatenated is the full text; a line must not span two parts.
    pub fn new(mut parts: Vec<String>) -> Self {
        if parts.is_empty() {
            parts.push(String::new());
        }
        let mut lines = Vec::new();
        for (p, text) in parts.iter().enumerate() {
            let bytes = text.as_bytes();
            let mut start = 0;
            for i in find_all(b'\n', bytes) {
                let end = if i > start && bytes[i - 1] == b'\r' { i - 1 } else { i };
                lines.push((p, start..end));
                start = i + 1;
            }
            if start < text.len() {
                lines.push((p, start..text.len()));
            }
        }
        // A text ending with a newline (or an empty one) still has a last, empty line.
        if parts.last().map_or(true, |t| t.is_empty() || t.ends_with('\n')) {
            let last = parts.len().saturating_sub(1);
            let end = parts.last().map_or(0, String::len);
            lines.push((last, end..end));
        }
        Self {
            parts,
            lines,
            cols: 0,
            row_starts: Vec::new(),
        }
    }

    /// Total length in bytes.
    pub fn len(&self) -> usize {
        self.parts.iter().map(String::len).sum()
    }

    /// The whole text as one string (only built on demand, e.g. for the clipboard).
    pub fn to_text(&self) -> String {
        self.parts.concat()
    }

    fn rebuild_rows(&mut self, cols: usize) {
        self.cols = cols;
        self.row_starts.clear();
        self.row_starts.reserve(self.lines.len() + 1);
        let mut total = 0;
        for (_, l) in &self.lines {
            self.row_starts.push(total);
            total += l.len().div_ceil(cols).max(1);
        }
        self.row_starts.push(total);
    }

    fn total_rows(&self) -> usize {
        *self.row_starts.last().unwrap_or(&0)
    }

    /// Text slice of visual row `row`.
    fn row_text(&self, row: usize) -> &str {
        let line_idx = self.row_starts.partition_point(|&s| s <= row) - 1;
        let (part, line) = &self.lines[line_idx];
        let text = &self.parts[*part];
        let k = row - self.row_starts[line_idx];
        let a = floor_char_boundary(text, (line.start + k * self.cols).min(line.end));
        let b = floor_char_boundary(text, (line.start + (k + 1) * self.cols).min(line.end));
        &text[a..b]
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let row_height = ui.fonts(|f| f.row_height(&font));
        let char_width = ui.fonts(|f| f.glyph_width(&font, 'M')).max(1.0);
        let scrollbar = ui.spacing().scroll.bar_width + ui.spacing().scroll.bar_inner_margin;
        let cols = (((ui.available_width() - scrollbar) / char_width) as usize).max(10);
        if cols != self.cols {
            self.rebuild_rows(cols);
        }

        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show_rows(ui, row_height, self.total_rows(), |ui, range| {
                for row in range {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(self.row_text(row)).font(font.clone()),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                    );
                }
            });
    }
}

fn floor_char_boundary(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Positions of `needle` in `hay`.
fn find_all(needle: u8, hay: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let i = pos + hay[pos..].iter().position(|&b| b == needle)?;
        pos = i + 1;
        Some(i)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(v: &OutputView) -> Vec<&str> {
        (0..v.total_rows()).map(|r| v.row_text(r)).collect()
    }

    #[test]
    fn splits_lines_and_wraps() {
        let mut v = OutputView::new(vec!["abcdef\r\n\r\nxy".into()]);
        v.rebuild_rows(4);
        assert_eq!(rows(&v), vec!["abcd", "ef", "", "xy"]);
    }

    #[test]
    fn lines_across_parts() {
        let mut v = OutputView::new(vec!["ab\r\n".into(), "cd\r\n\r\n".into(), "e".into()]);
        v.rebuild_rows(10);
        assert_eq!(rows(&v), vec!["ab", "cd", "", "e"]);
        assert_eq!(v.to_text(), "ab\r\ncd\r\n\r\ne");
        assert_eq!(v.len(), 11);
    }

    #[test]
    fn trailing_newline_and_empty() {
        let mut v = OutputView::new(vec!["a\n".into()]);
        v.rebuild_rows(10);
        assert_eq!(rows(&v), vec!["a", ""]);
        let mut v = OutputView::new(vec![]);
        v.rebuild_rows(10);
        assert_eq!(rows(&v), vec![""]);
    }

    #[test]
    fn respects_char_boundaries() {
        let mut v = OutputView::new(vec!["ššš".into()]); // 6 bytes
        v.rebuild_rows(3);
        assert_eq!(rows(&v).concat(), "ššš");
    }
}
