// SPDX-License-Identifier: Apache-2.0
//! What a view draws: a pure projection of editor state for a viewport. Every range
//! here is **line-relative in chars** so a renderer never needs the document.

use crate::decoration::Severity;
use crate::diff::HunkKind;
use crate::document::{Position, Range};
use crate::syntax::{Span, TokenKind};
use serde::{Deserialize, Serialize};

/// Which *visible* rows to produce. Rows count folded blocks as one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Viewport {
    pub first_row: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum GutterMark {
    Diagnostic { severity: Severity },
    Hunk { kind: HunkKind },
    Lock,
    SearchMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FoldState {
    None,
    Foldable,
    Folded,
}

/// A highlighted token on one row, line-relative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RowSpan {
    pub cols: Range,
    pub kind: TokenKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RowHighlight {
    pub cols: Range,
    /// `selection`, `search`, `bracket`, `hunk_insert`, `hunk_delete`, `diagnostic_error`, ...
    pub class: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct RowWidget {
    pub col: usize,
    pub text: String,
    pub class: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ViewLine {
    /// Document line number (zero-based).
    pub line: usize,
    pub text: String,
    pub spans: Vec<RowSpan>,
    pub highlights: Vec<RowHighlight>,
    /// Caret columns on this row.
    pub cursors: Vec<usize>,
    pub primary_cursor: Option<usize>,
    pub widgets: Vec<RowWidget>,
    pub gutter: Vec<GutterMark>,
    pub fold: FoldState,
    /// Hidden lines after this row when folded.
    pub folded_lines: usize,
    pub locked: bool,
    pub is_current: bool,
    /// Column of the line where this row's text starts (soft wrap), and whether the
    /// row continues a line begun on a row above.
    #[serde(default)]
    pub col_start: usize,
    #[serde(default)]
    pub continuation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ViewState {
    pub rows: Vec<ViewLine>,
    /// Rows after folding (for the scrollbar).
    pub total_rows: usize,
    pub total_lines: usize,
    pub primary: Position,
    pub selection_count: usize,
    pub dirty: bool,
    pub read_only: bool,
    pub errors: usize,
    pub warnings: usize,
    pub search_matches: usize,
    /// Longest visible line in chars (for horizontal scroll extent).
    pub max_cols: usize,
}

/// Converts absolute spans to row-relative ones, clipped to the line.
pub(crate) fn clip_spans(spans: &[Span], line_range: Range) -> Vec<RowSpan> {
    spans
        .iter()
        .filter_map(|s| {
            let r = s.range.intersect(&line_range)?;
            Some(RowSpan {
                cols: Range::new(r.start - line_range.start, r.end - line_range.start),
                kind: s.kind,
            })
        })
        .collect()
}

/// Splits the row of a whole line into its soft-wrap rows; `starts` are the columns
/// where each begins (the first is 0).
pub(crate) fn split_row(row: ViewLine, starts: &[usize]) -> Vec<ViewLine> {
    if starts.len() <= 1 {
        return vec![row];
    }
    let chars: Vec<char> = row.text.chars().collect();
    let len = chars.len();
    let n = starts.len();
    let mut out = Vec::with_capacity(n);
    for (i, &a) in starts.iter().enumerate() {
        let last = i + 1 == n;
        let b = if last { len } else { starts[i + 1] };
        // the last row also owns what lies past the text (the line break, widgets)
        let reach = if last { usize::MAX } else { b };
        let clip = |r: Range| -> Option<Range> {
            let (s, e) = (r.start.max(a), r.end.min(reach));
            (s < e || (r.is_empty() && r.start >= a && r.start < reach))
                .then(|| Range::new(s - a, e.max(s) - a))
        };
        let on_row = |c: usize| c >= a && (c < b || last);
        out.push(ViewLine {
            line: row.line,
            text: chars[a.min(len)..b.min(len)].iter().collect(),
            spans: row
                .spans
                .iter()
                .filter_map(|s| clip(s.cols).map(|cols| RowSpan { cols, kind: s.kind }))
                .collect(),
            highlights: row
                .highlights
                .iter()
                .filter_map(|h| {
                    clip(h.cols).map(|cols| RowHighlight {
                        cols,
                        class: h.class.clone(),
                    })
                })
                .collect(),
            cursors: row
                .cursors
                .iter()
                .filter(|c| on_row(**c))
                .map(|c| c - a)
                .collect(),
            primary_cursor: row.primary_cursor.filter(|c| on_row(*c)).map(|c| c - a),
            widgets: if last {
                row.widgets
                    .iter()
                    .map(|w| RowWidget {
                        col: w.col.saturating_sub(a),
                        text: w.text.clone(),
                        class: w.class.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            },
            gutter: if i == 0 {
                row.gutter.clone()
            } else {
                Vec::new()
            },
            fold: if i == 0 { row.fold } else { FoldState::None },
            folded_lines: if last { row.folded_lines } else { 0 },
            locked: row.locked,
            is_current: row.is_current,
            col_start: a,
            continuation: i > 0,
        });
    }
    out
}
