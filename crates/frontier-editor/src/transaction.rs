// SPDX-License-Identifier: Apache-2.0
//! Transactions: ordered, non-overlapping replacements applied atomically, with an
//! inverse so the undo tree can walk both ways.

use crate::document::{Document, Range};
use crate::selection::SelectionSet;
use serde::{Deserialize, Serialize};

/// Replace `range` (char offsets in the document *before* the transaction) with `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Change {
    pub range: Range,
    pub text: String,
}

impl Change {
    pub fn insert(offset: usize, text: impl Into<String>) -> Self {
        Self {
            range: Range::new(offset, offset),
            text: text.into(),
        }
    }
    pub fn delete(range: Range) -> Self {
        Self {
            range,
            text: String::new(),
        }
    }
    pub fn replace(range: Range, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
        }
    }
    pub fn inserted_len(&self) -> usize {
        self.text.chars().count()
    }
}

/// Why a transaction exists; drives undo grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum EditKind {
    Typing,
    Deletion,
    Paste,
    Replace,
    Format,
    Hunk,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Transaction {
    /// Sorted by `range.start`, non-overlapping, all relative to the pre-transaction text.
    pub changes: Vec<Change>,
    pub kind: EditKind,
    /// Selections to restore when this transaction is undone.
    pub selection_before: Option<SelectionSet>,
    /// Selections after the transaction (set by the editor when it applies it).
    pub selection_after: Option<SelectionSet>,
}

impl Transaction {
    pub fn new(mut changes: Vec<Change>, kind: EditKind) -> Self {
        changes.sort_by_key(|c| (c.range.start, c.range.end));
        Self {
            changes,
            kind,
            selection_before: None,
            selection_after: None,
        }
    }

    pub fn single(change: Change, kind: EditKind) -> Self {
        Self::new(vec![change], kind)
    }

    pub fn is_empty(&self) -> bool {
        self.changes
            .iter()
            .all(|c| c.range.is_empty() && c.text.is_empty())
    }

    /// True when no two changes overlap (touching is allowed).
    pub fn is_well_formed(&self) -> bool {
        self.changes
            .windows(2)
            .all(|w| w[0].range.end <= w[1].range.start)
    }

    /// Applies the changes to `doc` and returns the inverse transaction.
    pub fn apply(&self, doc: &mut Document) -> Transaction {
        // Apply from the back so earlier offsets stay valid; collect inverses in forward
        // order expressed against the *post* text.
        let mut inverse = Vec::with_capacity(self.changes.len());
        let mut delta: isize = 0;
        for c in &self.changes {
            let removed = doc.slice(c.range);
            let new_start = (c.range.start as isize + delta) as usize;
            let new_end = new_start + c.inserted_len();
            inverse.push(Change::replace(Range::new(new_start, new_end), removed));
            delta += c.inserted_len() as isize - c.range.len() as isize;
        }
        for c in self.changes.iter().rev() {
            doc.remove(c.range);
            doc.insert(c.range.start, &c.text);
        }
        Transaction {
            changes: inverse,
            kind: self.kind,
            selection_before: self.selection_after.clone(),
            selection_after: self.selection_before.clone(),
        }
    }

    /// Maps a pre-transaction offset to the post-transaction text. Offsets inside a
    /// replaced range collapse to the end of the inserted text (`assoc_after`) or its
    /// start.
    pub fn map_offset(&self, offset: usize, assoc_after: bool) -> usize {
        let mut delta: isize = 0;
        for c in &self.changes {
            if c.range.end < offset || (c.range.end == offset && c.range.is_empty() && assoc_after)
            {
                delta += c.inserted_len() as isize - c.range.len() as isize;
            } else if c.range.start < offset
                || (c.range.start == offset
                    && !c.range.is_empty()
                    && assoc_after
                    && c.range.end == offset)
            {
                // inside the replaced range
                let base = (c.range.start as isize + delta) as usize;
                return if assoc_after {
                    base + c.inserted_len()
                } else {
                    base
                };
            } else {
                break;
            }
        }
        (offset as isize + delta).max(0) as usize
    }

    pub fn map_selections(&self, sels: &SelectionSet) -> SelectionSet {
        sels.map(|s| {
            let mut m = s;
            m.anchor = self.map_offset(s.anchor, true);
            m.head = self.map_offset(s.head, true);
            m.goal_col = None;
            m
        })
    }

    /// Net change in char length.
    pub fn delta(&self) -> isize {
        self.changes
            .iter()
            .map(|c| c.inserted_len() as isize - c.range.len() as isize)
            .sum()
    }

    /// Lowest changed offset (pre-text), if any.
    pub fn first_offset(&self) -> Option<usize> {
        self.changes.first().map(|c| c.range.start)
    }

    /// Composes `self` then `next` into one transaction (used for undo grouping).
    /// `next`'s ranges are relative to the text after `self`.
    pub fn compose(&self, next: &Transaction) -> Transaction {
        // General composition is involved; the editor only groups transactions that
        // are adjacent single-change edits, which this handles exactly, and falls
        // back to keeping both when it cannot.
        if let ([a], [b]) = (self.changes.as_slice(), next.changes.as_slice()) {
            let a_end_post = a.range.start + a.inserted_len();
            if b.range.start >= a.range.start && b.range.end <= a_end_post {
                // `b` edits inside what `a` inserted.
                let mut text: Vec<char> = a.text.chars().collect();
                let lo = b.range.start - a.range.start;
                let hi = b.range.end - a.range.start;
                let tail: Vec<char> = text.split_off(hi);
                text.truncate(lo);
                text.extend(b.text.chars());
                text.extend(tail);
                return Transaction {
                    changes: vec![Change::replace(
                        a.range,
                        text.into_iter().collect::<String>(),
                    )],
                    kind: self.kind,
                    selection_before: self.selection_before.clone(),
                    selection_after: next.selection_after.clone(),
                };
            }
            if b.text.is_empty() && b.range.end == a.range.start && a.text.is_empty() {
                // backspacing leftwards: two deletions that touch
                return Transaction {
                    changes: vec![Change::delete(Range::new(b.range.start, a.range.end))],
                    kind: self.kind,
                    selection_before: self.selection_before.clone(),
                    selection_after: next.selection_after.clone(),
                };
            }
        }
        // Fallback: sequence both, expressing `next` against the pre-`self` text is
        // not always possible, so signal the caller to keep separate entries.
        let mut t = self.clone();
        t.changes.clear();
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_and_invert() {
        let mut d = Document::from_str("hello world");
        let t = Transaction::new(
            vec![
                Change::replace(Range::new(0, 5), "goodbye"),
                Change::insert(11, "!"),
            ],
            EditKind::Other,
        );
        let inv = t.apply(&mut d);
        assert_eq!(d.to_string(), "goodbye world!");
        inv.apply(&mut d);
        assert_eq!(d.to_string(), "hello world");
    }

    #[test]
    fn map_offsets() {
        let t = Transaction::single(Change::replace(Range::new(2, 4), "xyz"), EditKind::Other);
        assert_eq!(t.map_offset(1, true), 1);
        assert_eq!(t.map_offset(3, true), 5);
        assert_eq!(t.map_offset(3, false), 2);
        assert_eq!(t.map_offset(6, true), 7);
        let ins = Transaction::single(Change::insert(2, "ab"), EditKind::Typing);
        assert_eq!(ins.map_offset(2, true), 4);
        assert_eq!(ins.map_offset(2, false), 2);
    }

    #[test]
    fn compose_typing() {
        let a = Transaction::single(Change::insert(3, "a"), EditKind::Typing);
        let b = Transaction::single(Change::insert(4, "b"), EditKind::Typing);
        let c = a.compose(&b);
        assert_eq!(c.changes, vec![Change::insert(3, "ab")]);
        let d1 = Transaction::single(Change::delete(Range::new(4, 5)), EditKind::Deletion);
        let d2 = Transaction::single(Change::delete(Range::new(3, 4)), EditKind::Deletion);
        assert_eq!(
            d1.compose(&d2).changes,
            vec![Change::delete(Range::new(3, 5))]
        );
    }
}
