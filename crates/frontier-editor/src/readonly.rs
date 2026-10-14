// SPDX-License-Identifier: Apache-2.0
//! Read-only ranges (admin-locked settings keys, generated blocks).

use crate::document::{Document, Range};
use crate::transaction::Transaction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ReadOnlyRange {
    pub range: Range,
    pub reason: String,
}

impl ReadOnlyRange {
    pub fn new(range: Range, reason: impl Into<String>) -> Self {
        Self {
            range,
            reason: reason.into(),
        }
    }

    /// Whole lines `[first, last]` of `doc`, including the final line break.
    pub fn lines(doc: &Document, first: usize, last: usize, reason: impl Into<String>) -> Self {
        let start = doc.line_start(first);
        let end = if last + 1 < doc.line_count() {
            doc.line_start(last + 1)
        } else {
            doc.len_chars()
        };
        Self::new(Range::new(start, end), reason)
    }
}

/// Returns the first locked range a transaction would touch. Insertions exactly at a
/// locked range's boundary are allowed, except inside it.
pub fn violated<'a>(locks: &'a [ReadOnlyRange], tx: &Transaction) -> Option<&'a ReadOnlyRange> {
    for c in &tx.changes {
        for l in locks {
            let inside_insert =
                c.range.is_empty() && c.range.start > l.range.start && c.range.start < l.range.end;
            if c.range.overlaps(&l.range) || inside_insert {
                return Some(l);
            }
        }
    }
    None
}

/// Maps locks through a transaction so they follow the text they protect.
pub fn map_locks(locks: &[ReadOnlyRange], tx: &Transaction) -> Vec<ReadOnlyRange> {
    locks
        .iter()
        .map(|l| ReadOnlyRange {
            range: Range::new(
                tx.map_offset(l.range.start, false),
                tx.map_offset(l.range.end, true),
            ),
            reason: l.reason.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::{Change, EditKind};

    #[test]
    fn detects_overlap() {
        let locks = vec![ReadOnlyRange::new(Range::new(5, 10), "policy")];
        let ok = Transaction::single(Change::insert(10, "x"), EditKind::Typing);
        assert!(violated(&locks, &ok).is_none());
        let bad = Transaction::single(Change::insert(7, "x"), EditKind::Typing);
        assert!(violated(&locks, &bad).is_some());
        let bad2 = Transaction::single(Change::delete(Range::new(9, 12)), EditKind::Deletion);
        assert!(violated(&locks, &bad2).is_some());
    }
}
