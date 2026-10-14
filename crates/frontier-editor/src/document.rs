// SPDX-License-Identifier: Apache-2.0
//! Rope document with line/column, char and byte coordinates.
//!
//! Offsets used across the crate are **char offsets** (Unicode scalar values), which
//! are what `ropey` indexes cheaply and what cursors move over. Byte offsets are
//! available for tree-sitter and for hashing; UTF-16 offsets for a DOM host.

use ropey::Rope;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Zero-based line and column. The column counts chars from the start of the line.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl Position {
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line + 1, self.col + 1)
    }
}

/// Half-open char range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Range {
    pub start: usize,
    pub end: usize,
}

impl Range {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
    pub fn contains(&self, offset: usize) -> bool {
        offset >= self.start && offset < self.end
    }
    pub fn overlaps(&self, other: &Range) -> bool {
        self.start < other.end && other.start < self.end
    }
    pub fn intersect(&self, other: &Range) -> Option<Range> {
        let r = Range::new(self.start.max(other.start), self.end.min(other.end));
        (!r.is_empty()).then_some(r)
    }
}

/// The text model. Cheap to clone (rope nodes are shared).
#[derive(Clone, Default)]
pub struct Document {
    rope: Rope,
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("lines", &self.line_count())
            .field("chars", &self.len_chars())
            .finish()
    }
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Self {
        Self {
            rope: Rope::from_str(text),
        }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Number of lines. An empty document has one line; a trailing newline adds one.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.rope.to_string()
    }

    /// Text of a line without its line break.
    pub fn line(&self, line: usize) -> String {
        if line >= self.line_count() {
            return String::new();
        }
        let s = self.rope.line(line);
        let mut out: String = s.chars().collect();
        while out.ends_with('\n') || out.ends_with('\r') {
            out.pop();
        }
        out
    }

    /// Char length of a line excluding its line break.
    pub fn line_len(&self, line: usize) -> usize {
        if line >= self.line_count() {
            return 0;
        }
        let s = self.rope.line(line);
        let mut n = s.len_chars();
        if n > 0 && s.char(n - 1) == '\n' {
            n -= 1;
        }
        if n > 0 && s.char(n - 1) == '\r' {
            n -= 1;
        }
        n
    }

    pub fn line_start(&self, line: usize) -> usize {
        let line = line.min(self.line_count().saturating_sub(1));
        self.rope.line_to_char(line)
    }

    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line_len(line)
    }

    pub fn line_range(&self, line: usize) -> Range {
        Range::new(self.line_start(line), self.line_end(line))
    }

    pub fn slice(&self, range: Range) -> String {
        let end = range.end.min(self.len_chars());
        let start = range.start.min(end);
        self.rope.slice(start..end).to_string()
    }

    pub fn char_at(&self, offset: usize) -> Option<char> {
        (offset < self.len_chars()).then(|| self.rope.char(offset))
    }

    // ----- coordinate conversions -------------------------------------------------

    pub fn offset_to_position(&self, offset: usize) -> Position {
        let offset = offset.min(self.len_chars());
        let line = self.rope.char_to_line(offset);
        Position::new(line, offset - self.rope.line_to_char(line))
    }

    /// Clamps the column to the line length.
    pub fn position_to_offset(&self, pos: Position) -> usize {
        let line = pos.line.min(self.line_count().saturating_sub(1));
        self.line_start(line) + pos.col.min(self.line_len(line))
    }

    pub fn char_to_byte(&self, offset: usize) -> usize {
        self.rope.char_to_byte(offset.min(self.len_chars()))
    }

    pub fn byte_to_char(&self, byte: usize) -> usize {
        self.rope.byte_to_char(byte.min(self.len_bytes()))
    }

    pub fn char_to_utf16(&self, offset: usize) -> usize {
        self.rope.char_to_utf16_cu(offset.min(self.len_chars()))
    }

    pub fn utf16_to_char(&self, cu: usize) -> usize {
        self.rope.utf16_cu_to_char(cu.min(self.rope.len_utf16_cu()))
    }

    /// Byte position (`row`, `column` in bytes) as tree-sitter wants it.
    pub fn byte_point(&self, byte: usize) -> (usize, usize) {
        let byte = byte.min(self.len_bytes());
        let line = self.rope.byte_to_line(byte);
        (line, byte - self.rope.line_to_byte(line))
    }

    // ----- primitive edits (used by `Transaction::apply`) --------------------------

    pub(crate) fn insert(&mut self, offset: usize, text: &str) {
        self.rope.insert(offset.min(self.len_chars()), text);
    }

    pub(crate) fn remove(&mut self, range: Range) {
        let end = range.end.min(self.len_chars());
        let start = range.start.min(end);
        self.rope.remove(start..end);
    }

    /// Word boundaries around `offset` (alphanumerics and `_` form words).
    pub fn word_at(&self, offset: usize) -> Range {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let n = self.len_chars();
        let offset = offset.min(n);
        let mut start = offset;
        while start > 0 && self.char_at(start - 1).is_some_and(is_word) {
            start -= 1;
        }
        let mut end = offset;
        while end < n && self.char_at(end).is_some_and(is_word) {
            end += 1;
        }
        Range::new(start, end)
    }

    /// Offset of the next word boundary to the right (for `Alt+Right` / `Ctrl+Delete`).
    pub fn next_word_boundary(&self, offset: usize) -> usize {
        let n = self.len_chars();
        let mut i = offset.min(n);
        let class = |c: char| {
            if c.is_alphanumeric() || c == '_' {
                1
            } else if c.is_whitespace() {
                0
            } else {
                2
            }
        };
        while i < n
            && self
                .char_at(i)
                .is_some_and(|c| c.is_whitespace() && c != '\n')
        {
            i += 1;
        }
        if i < n && self.char_at(i) == Some('\n') {
            return i + 1;
        }
        let Some(first) = self.char_at(i) else {
            return i;
        };
        let k = class(first);
        while i < n && self.char_at(i).is_some_and(|c| class(c) == k) {
            i += 1;
        }
        i
    }

    pub fn prev_word_boundary(&self, offset: usize) -> usize {
        let mut i = offset.min(self.len_chars());
        let class = |c: char| {
            if c.is_alphanumeric() || c == '_' {
                1
            } else if c.is_whitespace() {
                0
            } else {
                2
            }
        };
        while i > 0
            && self
                .char_at(i - 1)
                .is_some_and(|c| c.is_whitespace() && c != '\n')
        {
            i -= 1;
        }
        if i > 0 && self.char_at(i - 1) == Some('\n') {
            return i - 1;
        }
        let Some(last) = (i > 0).then(|| self.rope.char(i - 1)) else {
            return i;
        };
        let k = class(last);
        while i > 0 && self.char_at(i - 1).is_some_and(|c| class(c) == k) {
            i -= 1;
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_round_trip() {
        let d = Document::from_str("ab\ncdé\n\nx");
        assert_eq!(d.line_count(), 4);
        assert_eq!(d.line(1), "cdé");
        assert_eq!(d.line_len(1), 3);
        assert_eq!(d.offset_to_position(4), Position::new(1, 1));
        assert_eq!(d.position_to_offset(Position::new(1, 99)), 6);
        assert_eq!(d.position_to_offset(Position::new(3, 0)), 8);
        assert_eq!(d.char_to_byte(6), 7); // é is two bytes
        assert_eq!(d.byte_to_char(7), 6);
        assert_eq!(d.line(99), "");
    }

    #[test]
    fn crlf_lines() {
        let d = Document::from_str("a\r\nb\r\n");
        assert_eq!(d.line_len(0), 1);
        assert_eq!(d.line(0), "a");
        assert_eq!(d.line_count(), 3);
    }

    #[test]
    fn words() {
        let d = Document::from_str("foo_bar  baz.qux");
        assert_eq!(d.word_at(2), Range::new(0, 7));
        assert_eq!(d.next_word_boundary(0), 7);
        assert_eq!(d.next_word_boundary(7), 12);
        assert_eq!(d.prev_word_boundary(12), 9);
        assert_eq!(d.prev_word_boundary(9), 0);
    }
}
