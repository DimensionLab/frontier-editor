// SPDX-License-Identifier: Apache-2.0
//! Where the characters of a line are: display columns (cells) of a monospace grid,
//! grapheme clusters, tab stops and soft-wrap segments.
//!
//! A CJK character or an emoji takes two cells, a combining mark none; a caret never
//! stands inside a cluster (a base letter and its accents, an emoji sequence). Lines of
//! plain ASCII without tabs take a fast path that allocates nothing.
//!
//! Right-to-left runs are laid out left to right in logical order: bidirectional text
//! is not supported.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The cells of one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineLayout {
    len: usize,
    /// `(char index, cell)` of every cluster start, then `(len, width)`. `None`: every
    /// character is one cell (ASCII without tabs).
    stops: Option<Vec<(u32, u32)>>,
}

impl LineLayout {
    pub fn new(text: &str, tab: usize) -> Self {
        if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return Self {
                len: text.len(),
                stops: None,
            };
        }
        let tab = tab.max(1) as u32;
        let mut stops = Vec::with_capacity(text.len() / 2 + 2);
        let (mut col, mut x) = (0u32, 0u32);
        for g in text.graphemes(true) {
            stops.push((col, x));
            let w = if g == "\t" {
                tab - x % tab
            } else {
                (UnicodeWidthStr::width(g) as u32).clamp(1, 2)
            };
            x += w;
            col += g.chars().count() as u32;
        }
        stops.push((col, x));
        Self {
            len: col as usize,
            stops: Some(stops),
        }
    }

    pub fn len_chars(&self) -> usize {
        self.len
    }

    /// Cells the whole line takes.
    pub fn width(&self) -> u32 {
        match &self.stops {
            None => self.len as u32,
            Some(s) => s.last().map(|p| p.1).unwrap_or(0),
        }
    }

    /// The cell where the character at `col` starts. Columns past the end continue one
    /// cell each (a block selection reaches beyond short lines).
    pub fn x(&self, col: usize) -> u32 {
        if col >= self.len {
            return self.width() + (col - self.len) as u32;
        }
        match &self.stops {
            None => col as u32,
            Some(s) => {
                let i = s.partition_point(|p| p.0 as usize <= col) - 1;
                s[i].1
            }
        }
    }

    /// The caret column nearest to cell `x`: always a cluster boundary. Past the end
    /// of the line it keeps counting (callers clamp when they need a real position).
    pub fn col_at(&self, x: f32) -> usize {
        let x = x.max(0.0);
        let w = self.width() as f32;
        if x >= w {
            return self.len + (x - w).round() as usize;
        }
        match &self.stops {
            None => x.round() as usize,
            Some(s) => {
                let i = s.partition_point(|p| (p.1 as f32) <= x).max(1) - 1;
                let (a, b) = (s[i], s[(i + 1).min(s.len() - 1)]);
                if x - a.1 as f32 <= b.1 as f32 - x {
                    a.0 as usize
                } else {
                    b.0 as usize
                }
            }
        }
    }

    /// The cluster boundary before `col` (0 at the start).
    pub fn prev_boundary(&self, col: usize) -> usize {
        match &self.stops {
            None => col.min(self.len).saturating_sub(1),
            Some(s) => {
                let i = s.partition_point(|p| (p.0 as usize) < col.min(self.len));
                if i == 0 { 0 } else { s[i - 1].0 as usize }
            }
        }
    }

    /// The cluster boundary after `col` (the length at the end).
    pub fn next_boundary(&self, col: usize) -> usize {
        match &self.stops {
            None => (col + 1).min(self.len),
            Some(s) => s
                .iter()
                .map(|p| p.0 as usize)
                .find(|c| *c > col)
                .unwrap_or(self.len),
        }
    }

    /// Soft-wrap: the columns where each visual row of the line starts (the first is
    /// 0), for rows `cols` cells wide. A row breaks after the last space that fits,
    /// else where the cells run out; never inside a cluster.
    pub fn wrap(&self, text: &str, cols: usize) -> Vec<usize> {
        let cols = cols.max(4) as u32;
        let mut starts = vec![0usize];
        if self.width() <= cols {
            return starts;
        }
        // cluster boundaries with their cells, the same for both representations
        let ascii: Vec<(u32, u32)>;
        let stops: &[(u32, u32)] = match &self.stops {
            Some(s) => s,
            None => {
                ascii = (0..=self.len as u32).map(|i| (i, i)).collect();
                &ascii
            }
        };
        let chars: Vec<char> = text.chars().collect();
        let mut row_x = 0u32;
        // boundary index after the last space of the current row
        let mut last_space: Option<usize> = None;
        let mut i = 1;
        let mut row_first = 0usize;
        while i < stops.len() {
            let end_x = stops[i].1;
            if end_x - row_x > cols && i - 1 > row_first {
                let at = match last_space {
                    Some(b) if b > row_first => b,
                    _ => i - 1,
                };
                starts.push(stops[at].0 as usize);
                row_x = stops[at].1;
                row_first = at;
                last_space = None;
                i = at + 1;
                continue;
            }
            let c = chars.get(stops[i - 1].0 as usize).copied().unwrap_or(' ');
            if c == ' ' || c == '\t' {
                last_space = Some(i);
            }
            i += 1;
        }
        starts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_one_cell_each() {
        let l = LineLayout::new("key: value", 4);
        assert_eq!(
            (l.width(), l.x(4), l.col_at(4.4), l.col_at(4.6)),
            (10, 4, 4, 5)
        );
        assert_eq!(
            (l.prev_boundary(3), l.next_boundary(3), l.next_boundary(10)),
            (2, 4, 10)
        );
        // past the end: virtual columns
        assert_eq!((l.x(13), l.col_at(13.0)), (13, 13));
    }

    #[test]
    fn wide_and_combining_characters_take_their_cells() {
        // 漢 and 字 are two cells; e + U+0301 is one cluster of one cell
        let l = LineLayout::new("a漢字e\u{301}b", 4);
        assert_eq!(l.len_chars(), 6);
        assert_eq!(l.width(), 1 + 2 + 2 + 1 + 1);
        assert_eq!([l.x(0), l.x(1), l.x(2), l.x(3), l.x(5)], [0, 1, 3, 5, 6]);
        // the accent is not a place for a caret
        assert_eq!(l.x(4), 5);
        assert_eq!((l.next_boundary(3), l.prev_boundary(5)), (5, 3));
        // a press on the right half of a wide character lands after it
        assert_eq!((l.col_at(1.4), l.col_at(2.4)), (1, 2));
        assert_eq!(l.col_at(5.4), 3);
    }

    #[test]
    fn emoji_sequences_are_one_cluster_of_two_cells() {
        let family = "👨\u{200d}👩\u{200d}👧";
        let text = format!("a{family}b");
        let l = LineLayout::new(&text, 4);
        assert_eq!(l.width(), 4);
        assert_eq!(l.next_boundary(1), 1 + family.chars().count());
        assert_eq!(l.prev_boundary(1 + family.chars().count()), 1);
        let flag = LineLayout::new("🇨🇿x", 4);
        assert_eq!((flag.width(), flag.next_boundary(0)), (3, 2));
    }

    #[test]
    fn tabs_go_to_the_next_stop() {
        let l = LineLayout::new("a\tb\t\tc", 4);
        assert_eq!(
            [l.x(0), l.x(1), l.x(2), l.x(3), l.x(4), l.x(5)],
            [0, 1, 4, 5, 8, 12]
        );
    }

    #[test]
    fn wrap_breaks_after_spaces_and_never_inside_a_cluster() {
        let text = "alpha beta gamma delta";
        let l = LineLayout::new(text, 4);
        assert_eq!(l.wrap(text, 80), [0]);
        assert_eq!(l.wrap(text, 11), [0, 11]);
        assert_eq!(l.wrap(text, 8), [0, 6, 11, 17]);
        // a word longer than the row is cut where the cells run out
        let long = "x".repeat(25);
        assert_eq!(LineLayout::new(&long, 4).wrap(&long, 10), [0, 10, 20]);
        // wide characters: five per ten cells
        let cjk = "漢".repeat(12);
        assert_eq!(LineLayout::new(&cjk, 4).wrap(&cjk, 10), [0, 5, 10]);
        // every start is a boundary
        let mixed = format!("ab {} cd e\u{301}f", "👨\u{200d}👩\u{200d}👧".repeat(4));
        let lm = LineLayout::new(&mixed, 4);
        for s in lm.wrap(&mixed, 6) {
            assert_eq!(
                lm.next_boundary(lm.prev_boundary(s).min(s)).max(s),
                s.max(lm.next_boundary(lm.prev_boundary(s)))
            );
            assert!(s == 0 || lm.x(s) > lm.x(lm.prev_boundary(s)));
        }
    }
}
