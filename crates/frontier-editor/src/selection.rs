// SPDX-License-Identifier: Apache-2.0
//! Multi-cursor selections.

use crate::document::Range;
use serde::{Deserialize, Serialize};

/// One selection. `anchor == head` is a caret. `head` is where the cursor is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Preferred column kept across vertical moves (`None` = use the head column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_col: Option<usize>,
}

impl Selection {
    pub const fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
            goal_col: None,
        }
    }
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal_col: None,
        }
    }
    pub fn is_caret(&self) -> bool {
        self.anchor == self.head
    }
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }
    pub fn range(&self) -> Range {
        Range::new(self.start(), self.end())
    }
    pub fn is_reversed(&self) -> bool {
        self.head < self.anchor
    }
    /// A caret at `offset`, or this selection with the head moved (shift-move).
    pub fn moved(&self, offset: usize, extend: bool) -> Self {
        if extend {
            Self {
                anchor: self.anchor,
                head: offset,
                goal_col: None,
            }
        } else {
            Self::caret(offset)
        }
    }
}

/// A sorted set of non-overlapping selections with one primary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SelectionSet {
    selections: Vec<Selection>,
    primary: usize,
}

impl Default for SelectionSet {
    fn default() -> Self {
        Self::single(Selection::caret(0))
    }
}

impl SelectionSet {
    pub fn single(sel: Selection) -> Self {
        Self {
            selections: vec![sel],
            primary: 0,
        }
    }

    /// Builds a normalised set: sorted by start, overlapping or touching-with-content
    /// selections merged, duplicate carets removed. `primary` tracks the given index.
    pub fn new(mut selections: Vec<Selection>, primary: usize) -> Self {
        if selections.is_empty() {
            return Self::default();
        }
        let primary_sel = selections[primary.min(selections.len() - 1)];
        selections.sort_by_key(|s| (s.start(), s.end()));
        let mut merged: Vec<Selection> = Vec::with_capacity(selections.len());
        for s in selections {
            if let Some(last) = merged.last_mut() {
                let overlap = s.start() < last.end()
                    || (s.start() == last.end() && s.is_caret() && last.is_caret());
                if overlap {
                    let start = last.start().min(s.start());
                    let end = last.end().max(s.end());
                    let reversed = s.is_reversed();
                    *last = if reversed {
                        Selection::new(end, start)
                    } else {
                        Selection::new(start, end)
                    };
                    continue;
                }
            }
            merged.push(s);
        }
        let primary = merged
            .iter()
            .position(|s| s.range().contains(primary_sel.head) || s.head == primary_sel.head)
            .unwrap_or(merged.len() - 1);
        Self {
            selections: merged,
            primary,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Selection> {
        self.selections.iter()
    }
    pub fn len(&self) -> usize {
        self.selections.len()
    }
    pub fn is_empty(&self) -> bool {
        self.selections.is_empty()
    }
    pub fn primary(&self) -> Selection {
        self.selections[self.primary]
    }
    pub fn primary_index(&self) -> usize {
        self.primary
    }
    pub fn as_slice(&self) -> &[Selection] {
        &self.selections
    }

    /// Applies `f` to every selection and renormalises.
    pub fn map(&self, mut f: impl FnMut(Selection) -> Selection) -> Self {
        Self::new(
            self.selections.iter().map(|s| f(*s)).collect(),
            self.primary,
        )
    }

    pub fn with_added(&self, sel: Selection) -> Self {
        let mut v = self.selections.clone();
        v.push(sel);
        let primary = v.len() - 1;
        Self::new(v, primary)
    }

    /// Keeps only the primary selection.
    pub fn collapse_to_primary(&self) -> Self {
        Self::single(self.primary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_overlaps_and_duplicates() {
        let s = SelectionSet::new(
            vec![
                Selection::new(5, 10),
                Selection::new(8, 12),
                Selection::caret(1),
                Selection::caret(1),
            ],
            0,
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s.as_slice()[0], Selection::caret(1));
        assert_eq!(s.as_slice()[1].range(), Range::new(5, 12));
        assert_eq!(s.primary().range(), Range::new(5, 12));
    }

    #[test]
    fn adjacent_ranges_stay_separate() {
        let s = SelectionSet::new(vec![Selection::new(0, 2), Selection::new(2, 4)], 0);
        assert_eq!(s.len(), 2);
    }
}
