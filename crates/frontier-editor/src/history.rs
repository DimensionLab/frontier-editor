// SPDX-License-Identifier: Apache-2.0
//! Undo tree with grouping.
//!
//! Every applied transaction becomes a node whose parent is the state it was applied
//! to. Undo walks to the parent; redo walks to the most recently visited child, so
//! branches are never lost. Consecutive typing within `group_window_ms` (or sharing an
//! explicit [`UndoGroup`]) composes into one node when the transactions can be merged.

use crate::transaction::{EditKind, Transaction};

/// Opaque grouping token: transactions carrying the same group merge into one undo step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UndoGroup(pub u64);

#[derive(Debug, Clone)]
struct Node {
    parent: Option<usize>,
    children: Vec<usize>,
    /// Child to take on redo.
    preferred_child: Option<usize>,
    /// Applied transaction (forward) and its inverse.
    forward: Transaction,
    inverse: Transaction,
    time_ms: u64,
    group: Option<UndoGroup>,
}

#[derive(Debug, Clone)]
pub struct History {
    nodes: Vec<Node>,
    /// Current node; `None` is the root (original document).
    current: Option<usize>,
    /// Index of the node that matches the on-disk/saved text.
    saved: Option<usize>,
    pub group_window_ms: u64,
    pub max_nodes: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            current: None,
            saved: None,
            group_window_ms: 300,
            max_nodes: 10_000,
        }
    }
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn can_undo(&self) -> bool {
        self.current.is_some()
    }

    pub fn can_redo(&self) -> bool {
        match self.current {
            Some(i) => self.nodes[i].preferred_child.is_some(),
            None => {
                self.nodes.iter().any(|n| n.parent.is_none()) && self.root_preferred().is_some()
            }
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.current != self.saved
    }

    pub fn mark_saved(&mut self) {
        self.saved = self.current;
    }

    fn root_preferred(&self) -> Option<usize> {
        // The most recently created root child is the redo target from the root.
        self.nodes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, n)| n.parent.is_none())
            .map(|(i, _)| i)
    }

    /// Records an applied transaction (`forward`) and its inverse. Returns true when
    /// it was merged into the current node instead of creating a new one.
    pub fn push(
        &mut self,
        forward: Transaction,
        inverse: Transaction,
        time_ms: u64,
        group: Option<UndoGroup>,
    ) -> bool {
        if let Some(cur) = self.current {
            let node = &self.nodes[cur];
            let same_group = group.is_some() && group == node.group;
            let mergeable_kind = matches!(
                (node.forward.kind, forward.kind),
                (EditKind::Typing, EditKind::Typing) | (EditKind::Deletion, EditKind::Deletion)
            );
            let within_window = time_ms.saturating_sub(node.time_ms) <= self.group_window_ms;
            let is_leaf = node.children.is_empty();
            if is_leaf && (same_group || (mergeable_kind && within_window)) {
                let composed = node.forward.compose(&forward);
                // The inverse of (old then new) is (new⁻¹ then old⁻¹).
                let inv_composed = inverse.compose(&node.inverse);
                if !composed.changes.is_empty() && !inv_composed.changes.is_empty() {
                    let node = &mut self.nodes[cur];
                    node.forward = composed;
                    node.inverse = inv_composed;
                    node.time_ms = time_ms;
                    return true;
                }
            }
        }
        self.push_new(forward, inverse, time_ms, group)
    }

    fn push_new(
        &mut self,
        forward: Transaction,
        inverse: Transaction,
        time_ms: u64,
        group: Option<UndoGroup>,
    ) -> bool {
        let idx = self.nodes.len();
        self.nodes.push(Node {
            parent: self.current,
            children: Vec::new(),
            preferred_child: None,
            forward,
            inverse,
            time_ms,
            group,
        });
        if let Some(cur) = self.current {
            self.nodes[cur].children.push(idx);
            self.nodes[cur].preferred_child = Some(idx);
        }
        self.current = Some(idx);
        if self.nodes.len() > self.max_nodes {
            self.prune();
        }
        false
    }

    /// Drops the oldest root chain when the tree grows past `max_nodes`. Nodes are only
    /// ever appended, so index 0 is always the oldest; we rebase its first child.
    fn prune(&mut self) {
        // Simple strategy: when over budget, forget everything that is not on the path
        // from the root to `current` and renumber. Rare in practice.
        let mut keep = Vec::new();
        let mut i = self.current;
        while let Some(n) = i {
            keep.push(n);
            i = self.nodes[n].parent;
        }
        keep.reverse();
        let drop_count = keep.len().saturating_sub(self.max_nodes / 2);
        let keep = &keep[drop_count..];
        let mut new_nodes: Vec<Node> = Vec::with_capacity(keep.len());
        let mut prev: Option<usize> = None;
        for &old in keep {
            let mut n = self.nodes[old].clone();
            n.parent = prev;
            n.children.clear();
            n.preferred_child = None;
            if let Some(p) = prev {
                new_nodes[p].children = vec![new_nodes.len()];
                new_nodes[p].preferred_child = Some(new_nodes.len());
            }
            prev = Some(new_nodes.len());
            new_nodes.push(n);
        }
        self.saved = self.saved.and_then(|s| keep.iter().position(|&k| k == s));
        self.nodes = new_nodes;
        self.current = prev;
    }

    /// Returns the inverse transaction to apply, and moves to the parent.
    pub fn undo(&mut self) -> Option<Transaction> {
        let cur = self.current?;
        let node = &self.nodes[cur];
        let inv = node.inverse.clone();
        self.current = node.parent;
        if let Some(p) = self.current {
            self.nodes[p].preferred_child = Some(cur);
        }
        Some(inv)
    }

    /// Returns the forward transaction to re-apply, and moves to the preferred child.
    pub fn redo(&mut self) -> Option<Transaction> {
        let next = match self.current {
            Some(i) => self.nodes[i].preferred_child,
            None => self.root_preferred(),
        }?;
        self.current = Some(next);
        Some(self.nodes[next].forward.clone())
    }

    /// Breaks the current grouping so the next edit starts a new undo step.
    pub fn close_group(&mut self) {
        if let Some(cur) = self.current {
            self.nodes[cur].time_ms = 0;
            self.nodes[cur].group = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, Range};
    use crate::transaction::Change;

    fn apply(doc: &mut Document, h: &mut History, t: Transaction, time: u64) {
        let inv = t.apply(doc);
        h.push(t, inv, time, None);
    }

    #[test]
    fn undo_redo_and_branching() {
        let mut d = Document::from_str("");
        let mut h = History::new();
        apply(
            &mut d,
            &mut h,
            Transaction::single(Change::insert(0, "a"), EditKind::Typing),
            0,
        );
        apply(
            &mut d,
            &mut h,
            Transaction::single(Change::insert(1, "b"), EditKind::Typing),
            10,
        );
        assert_eq!(h.len(), 1, "typing within the window merges");
        apply(
            &mut d,
            &mut h,
            Transaction::single(Change::insert(2, "c"), EditKind::Typing),
            5000,
        );
        assert_eq!(h.len(), 2);
        assert_eq!(d.to_string(), "abc");
        h.undo().expect("undo").apply(&mut d);
        assert_eq!(d.to_string(), "ab");
        // branch
        apply(
            &mut d,
            &mut h,
            Transaction::single(Change::insert(2, "X"), EditKind::Paste),
            9000,
        );
        assert_eq!(d.to_string(), "abX");
        h.undo().expect("undo").apply(&mut d);
        assert_eq!(d.to_string(), "ab");
        h.redo().expect("redo").apply(&mut d);
        assert_eq!(d.to_string(), "abX", "redo follows the newest branch");
        h.undo().expect("undo").apply(&mut d);
        h.undo().expect("undo").apply(&mut d);
        assert_eq!(d.to_string(), "");
        assert!(!h.can_undo());
        h.redo().expect("redo").apply(&mut d);
        assert_eq!(d.to_string(), "ab");
    }

    #[test]
    fn dirty_tracking() {
        let mut d = Document::from_str("x");
        let mut h = History::new();
        assert!(!h.is_dirty());
        apply(
            &mut d,
            &mut h,
            Transaction::single(Change::delete(Range::new(0, 1)), EditKind::Deletion),
            0,
        );
        assert!(h.is_dirty());
        h.mark_saved();
        assert!(!h.is_dirty());
        h.undo().expect("undo").apply(&mut d);
        assert!(h.is_dirty());
    }
}
