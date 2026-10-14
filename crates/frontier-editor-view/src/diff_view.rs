// SPDX-License-Identifier: Apache-2.0
//! Row model of a comparison of two texts (a proposal under review, two versions of a
//! file): the rows a diff shows, for the inline layout (one column, both line numbers)
//! and the side-by-side layout (old left, new right, changed lines paired). Unchanged
//! stretches away from a change collapse into one row.

use frontier_editor::diff::{ChangeSet, LineOp};
use frontier_editor::document::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Context,
    Delete,
    Insert,
}

/// One side of a row: a line of the old or of the new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// 0-based line number in its own text.
    pub line: usize,
    pub kind: CellKind,
    /// The text of a changed line; `None` for context (read from the base document).
    pub text: Option<String>,
    /// Character ranges that differ from the paired line.
    pub emphasis: Vec<Range>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    Line {
        /// The old text's side (context or a removed line).
        left: Option<Cell>,
        /// The new text's side (context or an added line).
        right: Option<Cell>,
        /// Index of the hunk this row belongs to.
        hunk: Option<usize>,
        /// First row of its hunk: the hunk's actions are drawn here.
        first: bool,
    },
    /// `count` unchanged lines starting at base line `line`, hidden.
    Collapsed { line: usize, count: usize },
}

impl DiffRow {
    /// The base line a caret can be put on from this row.
    pub fn base_line(&self) -> Option<usize> {
        match self {
            DiffRow::Line { left: Some(c), .. } => Some(c.line),
            DiffRow::Collapsed { line, .. } => Some(*line),
            _ => None,
        }
    }

    pub fn hunk(&self) -> Option<usize> {
        match self {
            DiffRow::Line { hunk, .. } => *hunk,
            DiffRow::Collapsed { .. } => None,
        }
    }
}

/// The rows of a comparison and where each hunk starts in them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffRows {
    pub rows: Vec<DiffRow>,
    /// Row index of the first row of each hunk.
    pub hunk_rows: Vec<usize>,
    /// Lines added and removed in total.
    pub added: usize,
    pub removed: usize,
}

/// Builds the rows. `side_by_side` pairs removed and added lines of a hunk on one row;
/// `context` keeps that many unchanged lines around each change and collapses the rest
/// (`None` shows every line).
pub fn build_rows(
    cs: &ChangeSet,
    base_line_count: usize,
    side_by_side: bool,
    context: Option<usize>,
) -> DiffRows {
    let mut out = DiffRows::default();
    let mut line = 0usize;
    // new line number of an unchanged base line = base line + shift
    let mut shift = 0isize;
    let ctx_row = |line: usize, shift: isize| {
        let cell = |l: usize| Cell {
            line: l,
            kind: CellKind::Context,
            text: None,
            emphasis: Vec::new(),
        };
        DiffRow::Line {
            left: Some(cell(line)),
            right: Some(cell((line as isize + shift).max(0) as usize)),
            hunk: None,
            first: false,
        }
    };
    // unchanged lines [from, to): all of them, or the edges with the middle collapsed
    let push_context =
        |out: &mut DiffRows, from: usize, to: usize, shift: isize, lead: bool, trail: bool| {
            let n = to.saturating_sub(from);
            let Some(keep) = context else {
                out.rows.extend((from..to).map(|l| ctx_row(l, shift)));
                return;
            };
            let head = if lead { keep.min(n) } else { 0 };
            let tail = if trail { keep.min(n - head) } else { 0 };
            let hidden = n - head - tail;
            // hiding one or two lines saves nothing
            if hidden <= 2 {
                out.rows.extend((from..to).map(|l| ctx_row(l, shift)));
                return;
            }
            out.rows
                .extend((from..from + head).map(|l| ctx_row(l, shift)));
            out.rows.push(DiffRow::Collapsed {
                line: from + head,
                count: hidden,
            });
            out.rows.extend((to - tail..to).map(|l| ctx_row(l, shift)));
        };
    for (i, h) in cs.hunks.iter().enumerate() {
        let start = h.old_lines.start.min(base_line_count);
        push_context(&mut out, line, start, shift, i > 0, true);
        out.hunk_rows.push(out.rows.len());
        let mut dels = Vec::new();
        let mut ins = Vec::new();
        let (mut d, mut n) = (h.old_lines.start, h.new_lines.start);
        for l in &h.lines {
            match l.op {
                LineOp::Delete => {
                    dels.push(Cell {
                        line: d,
                        kind: CellKind::Delete,
                        text: Some(l.text.clone()),
                        emphasis: l.emphasis.clone(),
                    });
                    d += 1;
                }
                LineOp::Insert => {
                    ins.push(Cell {
                        line: n,
                        kind: CellKind::Insert,
                        text: Some(l.text.clone()),
                        emphasis: l.emphasis.clone(),
                    });
                    n += 1;
                }
                LineOp::Equal => {}
            }
        }
        out.removed += dels.len();
        out.added += ins.len();
        let mut first = true;
        let mut push = |out: &mut DiffRows, left: Option<Cell>, right: Option<Cell>| {
            out.rows.push(DiffRow::Line {
                left,
                right,
                hunk: Some(i),
                first,
            });
            first = false;
        };
        if side_by_side {
            let rows = dels.len().max(ins.len());
            let (mut d, mut s) = (dels.into_iter(), ins.into_iter());
            for _ in 0..rows {
                push(&mut out, d.next(), s.next());
            }
        } else {
            for c in dels {
                push(&mut out, Some(c), None);
            }
            for c in ins {
                push(&mut out, None, Some(c));
            }
        }
        line = h.old_lines.end.max(line);
        shift = h.new_lines.end as isize - h.old_lines.end as isize;
    }
    push_context(
        &mut out,
        line,
        base_line_count,
        shift,
        !cs.hunks.is_empty(),
        false,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(rows: &[DiffRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                DiffRow::Collapsed { count, .. } => format!("…{count}"),
                DiffRow::Line { left, right, .. } => {
                    let k = |c: &Option<Cell>| match c.as_ref().map(|c| c.kind) {
                        Some(CellKind::Context) => "=",
                        Some(CellKind::Delete) => "-",
                        Some(CellKind::Insert) => "+",
                        None => "_",
                    };
                    format!("{}{}", k(left), k(right))
                }
            })
            .collect()
    }

    #[test]
    fn inline_and_side_by_side_rows() {
        let cs = ChangeSet::compute("a\nb\nc\n", "a\nB\nc\nd\n");
        let inline = build_rows(&cs, 3, false, None);
        assert_eq!(kinds(&inline.rows), ["==", "-_", "_+", "==", "_+"]);
        assert_eq!(inline.hunk_rows, [1, 4]);
        assert_eq!((inline.added, inline.removed), (2, 1));
        let sbs = build_rows(&cs, 3, true, None);
        assert_eq!(kinds(&sbs.rows), ["==", "-+", "==", "_+"]);
        assert_eq!(sbs.hunk_rows, [1, 3]);
        // line numbers of each side
        match &sbs.rows[2] {
            DiffRow::Line {
                left: Some(l),
                right: Some(r),
                ..
            } => assert_eq!((l.line, r.line), (2, 2)),
            other => panic!("{other:?}"),
        }
        match &sbs.rows[3] {
            DiffRow::Line {
                left: None,
                right: Some(r),
                first: true,
                hunk: Some(1),
            } => assert_eq!(r.line, 3),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unchanged_stretches_collapse_around_the_changes() {
        let base: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let new = base
            .replace("line 10\n", "line ten\n")
            .replace("line 30\n", "");
        let cs = ChangeSet::compute(&base, &new);
        let r = build_rows(&cs, 40, false, Some(3));
        let k = kinds(&r.rows);
        // 7 hidden, 3 context, change, 3 context, 13 hidden, 3 context, removal, 3 context, 6 hidden
        assert_eq!(k[0], "…7");
        assert_eq!(&k[1..4], ["==", "==", "=="]);
        assert_eq!(&k[4..6], ["-_", "_+"]);
        assert!(k.contains(&"…13".to_string()));
        assert_eq!(k.last().map(String::as_str), Some("…6"));
        // context after a removal carries the shifted new line number
        match r
            .rows
            .iter()
            .rev()
            .find(|x| matches!(x, DiffRow::Line { .. }))
        {
            Some(DiffRow::Line {
                left: Some(l),
                right: Some(n),
                ..
            }) => assert_eq!(l.line, n.line + 1),
            other => panic!("{other:?}"),
        }
        assert_eq!(r.rows[r.hunk_rows[1]].hunk(), Some(1));
    }
}
