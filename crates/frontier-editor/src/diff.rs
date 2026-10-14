// SPDX-License-Identifier: Apache-2.0
//! Line diff with intra-line refinement, grouped into hunks with stable ids, and
//! per-hunk accept/reject that produces a new text (and a transaction).

use crate::document::{Document, Range};
use crate::transaction::{Change, EditKind, Transaction};
use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, DiffOp, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HunkKind {
    Insert,
    Delete,
    Modify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LineOp {
    Equal,
    Delete,
    Insert,
}

/// One line inside a hunk, with intra-line char ranges that differ (for `Modify`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct HunkLine {
    pub op: LineOp,
    pub text: String,
    /// Char ranges within `text` that changed (empty for `Equal`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emphasis: Vec<Range>,
}

/// A contiguous block of changed lines. `old_lines` / `new_lines` are half-open line
/// ranges in the base and proposed texts. The id is a content hash so the same
/// change keeps its id across re-diffs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Hunk {
    pub id: String,
    pub kind: HunkKind,
    pub old_lines: Range,
    pub new_lines: Range,
    pub lines: Vec<HunkLine>,
    /// Optional alignment key (a recipe step index, a settings key, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Hunk {
    pub fn old_text(&self) -> String {
        join_lines(
            self.lines
                .iter()
                .filter(|l| l.op != LineOp::Insert)
                .map(|l| l.text.as_str()),
        )
    }
    pub fn new_text(&self) -> String {
        join_lines(
            self.lines
                .iter()
                .filter(|l| l.op != LineOp::Delete)
                .map(|l| l.text.as_str()),
        )
    }
}

fn join_lines<'a>(it: impl Iterator<Item = &'a str>) -> String {
    let mut s = String::new();
    for l in it {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// A diff between a base text and a proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ChangeSet {
    pub base_hash: String,
    pub proposed_hash: String,
    pub hunks: Vec<Hunk>,
    #[serde(default = "yes")]
    pub base_trailing_newline: bool,
    #[serde(default = "yes")]
    pub proposed_trailing_newline: bool,
}

fn yes() -> bool {
    true
}

impl ChangeSet {
    /// Diffs `base` against `proposed` line by line (Myers), then refines modified
    /// blocks character by character for emphasis ranges.
    pub fn compute(base: &str, proposed: &str) -> Self {
        let diff = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .diff_lines(base, proposed);
        let old_lines: Vec<&str> = split_keep_newlines(base);
        let new_lines: Vec<&str> = split_keep_newlines(proposed);
        let mut hunks = Vec::new();
        let mut pending: Option<(usize, usize, usize, usize)> = None; // old_s, old_e, new_s, new_e
        let flush = |p: &mut Option<(usize, usize, usize, usize)>, hunks: &mut Vec<Hunk>| {
            if let Some((os, oe, ns, ne)) = p.take() {
                hunks.push(build_hunk(&old_lines[os..oe], &new_lines[ns..ne], os, ns));
            }
        };
        for op in diff.ops() {
            match *op {
                DiffOp::Equal { .. } => flush(&mut pending, &mut hunks),
                DiffOp::Delete {
                    old_index,
                    old_len,
                    new_index,
                } => extend(
                    &mut pending,
                    old_index,
                    old_index + old_len,
                    new_index,
                    new_index,
                ),
                DiffOp::Insert {
                    old_index,
                    new_index,
                    new_len,
                } => extend(
                    &mut pending,
                    old_index,
                    old_index,
                    new_index,
                    new_index + new_len,
                ),
                DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => extend(
                    &mut pending,
                    old_index,
                    old_index + old_len,
                    new_index,
                    new_index + new_len,
                ),
            }
        }
        flush(&mut pending, &mut hunks);
        Self {
            base_hash: crate::provenance::content_hash(base),
            proposed_hash: crate::provenance::content_hash(proposed),
            hunks,
            base_trailing_newline: base.ends_with('\n') || base.is_empty(),
            proposed_trailing_newline: proposed.ends_with('\n') || proposed.is_empty(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn hunk(&self, id: &str) -> Option<&Hunk> {
        self.hunks.iter().find(|h| h.id == id)
    }

    /// Applies the hunks whose ids are in `accepted` to `base`, leaving the others as
    /// they are in the base. Unknown ids are ignored.
    pub fn apply_accepted(&self, base: &str, accepted: &[&str]) -> String {
        let old: Vec<&str> = split_keep_newlines(base);
        let mut out = String::with_capacity(base.len());
        let mut cursor = 0usize;
        for h in &self.hunks {
            out.extend(old[cursor..h.old_lines.start].iter().copied());
            if accepted.contains(&h.id.as_str()) {
                for l in h.lines.iter().filter(|l| l.op != LineOp::Delete) {
                    out.push_str(&l.text);
                    out.push('\n');
                }
            } else {
                out.extend(old[h.old_lines.start..h.old_lines.end].iter().copied());
            }
            cursor = h.old_lines.end;
        }
        out.extend(old[cursor..].iter().copied());
        // The last line keeps the trailing-newline convention of the text it came from.
        let last_from_proposal = self.hunks.last().is_some_and(|h| {
            h.old_lines.end >= old.len()
                && accepted.contains(&h.id.as_str())
                && h.kind != HunkKind::Delete
        });
        let want_newline = if last_from_proposal {
            self.proposed_trailing_newline
        } else {
            self.base_trailing_newline
        };
        if !want_newline && out.ends_with('\n') {
            out.pop();
        }
        out
    }

    /// A transaction turning `doc` (which must equal the base) into the text with the
    /// accepted hunks applied. One change per accepted hunk.
    pub fn transaction_for(&self, doc: &Document, accepted: &[&str]) -> Transaction {
        let mut changes = Vec::new();
        for h in self
            .hunks
            .iter()
            .filter(|h| accepted.contains(&h.id.as_str()))
        {
            let start = if h.old_lines.start < doc.line_count() {
                doc.line_start(h.old_lines.start)
            } else {
                doc.len_chars()
            };
            let end = if h.old_lines.end == 0 {
                start
            } else if h.old_lines.end < doc.line_count() {
                doc.line_start(h.old_lines.end)
            } else {
                doc.len_chars()
            };
            let mut text = h.new_text();
            if end == doc.len_chars() && !self.proposed_trailing_newline && text.ends_with('\n') {
                text.pop();
            }
            changes.push(Change::replace(Range::new(start, end), text));
        }
        Transaction::new(changes, EditKind::Hunk)
    }
}

fn extend(
    p: &mut Option<(usize, usize, usize, usize)>,
    os: usize,
    oe: usize,
    ns: usize,
    ne: usize,
) {
    match p {
        Some(cur) => {
            cur.1 = oe;
            cur.3 = ne;
        }
        None => *p = Some((os, oe, ns, ne)),
    }
}

fn split_keep_newlines(s: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(&s[start..=i]);
            start = i + 1;
        }
    }
    if start < s.len() {
        v.push(&s[start..]);
    }
    v
}

fn strip_nl(s: &str) -> &str {
    s.strip_suffix('\n')
        .map(|t| t.strip_suffix('\r').unwrap_or(t))
        .unwrap_or(s)
}

fn build_hunk(old: &[&str], new: &[&str], old_start: usize, new_start: usize) -> Hunk {
    let kind = match (old.is_empty(), new.is_empty()) {
        (true, false) => HunkKind::Insert,
        (false, true) => HunkKind::Delete,
        _ => HunkKind::Modify,
    };
    let mut lines = Vec::with_capacity(old.len() + new.len());
    if kind == HunkKind::Modify && old.len() == new.len() {
        for (o, n) in old.iter().zip(new.iter()) {
            let (eo, en) = intra_line(strip_nl(o), strip_nl(n));
            lines.push(HunkLine {
                op: LineOp::Delete,
                text: strip_nl(o).to_string(),
                emphasis: eo,
            });
            lines.push(HunkLine {
                op: LineOp::Insert,
                text: strip_nl(n).to_string(),
                emphasis: en,
            });
        }
    } else {
        for o in old {
            lines.push(HunkLine {
                op: LineOp::Delete,
                text: strip_nl(o).to_string(),
                emphasis: Vec::new(),
            });
        }
        for n in new {
            lines.push(HunkLine {
                op: LineOp::Insert,
                text: strip_nl(n).to_string(),
                emphasis: Vec::new(),
            });
        }
    }
    let mut hasher = String::new();
    for l in &lines {
        hasher.push(match l.op {
            LineOp::Delete => '-',
            LineOp::Insert => '+',
            LineOp::Equal => ' ',
        });
        hasher.push_str(&l.text);
        hasher.push('\n');
    }
    let id = crate::provenance::content_hash(&hasher)[..12].to_string();
    Hunk {
        id,
        kind,
        old_lines: Range::new(old_start, old_start + old.len()),
        new_lines: Range::new(new_start, new_start + new.len()),
        lines,
        label: None,
    }
}

/// Char ranges that differ between two lines (old ranges, new ranges).
fn intra_line(old: &str, new: &str) -> (Vec<Range>, Vec<Range>) {
    let diff = TextDiff::from_chars(old, new);
    let (mut eo, mut en) = (Vec::new(), Vec::new());
    let (mut oi, mut ni) = (0usize, 0usize);
    for c in diff.iter_all_changes() {
        match c.tag() {
            ChangeTag::Equal => {
                oi += 1;
                ni += 1;
            }
            ChangeTag::Delete => {
                push_range(&mut eo, oi);
                oi += 1;
            }
            ChangeTag::Insert => {
                push_range(&mut en, ni);
                ni += 1;
            }
        }
    }
    (eo, en)
}

fn push_range(v: &mut Vec<Range>, i: usize) {
    if let Some(last) = v.last_mut()
        && last.end == i
    {
        last.end = i + 1;
        return;
    }
    v.push(Range::new(i, i + 1));
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "a: 1\nb: 2\nc: 3\nd: 4\n";
    const PROP: &str = "a: 1\nb: 20\nc: 3\nd: 4\ne: 5\n";

    #[test]
    fn hunks_and_partial_accept() {
        let cs = ChangeSet::compute(BASE, PROP);
        assert_eq!(cs.hunks.len(), 2);
        assert_eq!(cs.hunks[0].kind, HunkKind::Modify);
        assert_eq!(cs.hunks[0].old_lines, Range::new(1, 2));
        assert_eq!(cs.hunks[0].lines[1].emphasis, vec![Range::new(4, 5)]);
        assert_eq!(cs.hunks[1].kind, HunkKind::Insert);
        let all: Vec<&str> = cs.hunks.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(cs.apply_accepted(BASE, &all), PROP);
        assert_eq!(cs.apply_accepted(BASE, &[]), BASE);
        assert_eq!(
            cs.apply_accepted(BASE, &[all[1]]),
            "a: 1\nb: 2\nc: 3\nd: 4\ne: 5\n"
        );
        let doc = Document::from_str(BASE);
        let mut d2 = doc.clone();
        cs.transaction_for(&doc, &[all[0]]).apply(&mut d2);
        assert_eq!(d2.to_string(), "a: 1\nb: 20\nc: 3\nd: 4\n");
        let mut d3 = doc.clone();
        cs.transaction_for(&doc, &all).apply(&mut d3);
        assert_eq!(d3.to_string(), PROP);
    }

    #[test]
    fn ids_are_stable() {
        let a = ChangeSet::compute(BASE, PROP);
        let b = ChangeSet::compute("x\n".to_string().as_str(), "x\n");
        assert!(b.is_empty());
        let c = ChangeSet::compute(&format!("zzz\n{BASE}"), &format!("zzz\n{PROP}"));
        assert_eq!(a.hunks[0].id, c.hunks[0].id);
    }

    #[test]
    fn no_trailing_newline() {
        let cs = ChangeSet::compute("a\nb", "a\nc");
        let ids: Vec<&str> = cs.hunks.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(cs.apply_accepted("a\nb", &ids), "a\nc");
        let doc = Document::from_str("a\nb");
        let mut d = doc.clone();
        cs.transaction_for(&doc, &ids).apply(&mut d);
        assert_eq!(d.to_string(), "a\nc");
    }
}
