// SPDX-License-Identifier: Apache-2.0
//! Keeping a buffer and the place its text is stored in step.
//!
//! The text of a document lives in one store (a parameter of a board node, a file, a
//! settings layer). The editor works on a buffer of it; every view of the document
//! shares that buffer. [`Binding`] is the only path between the two, in both
//! directions, and its one rule is that the store never overwrites the buffer with
//! text the buffer already left behind:
//!
//! * [`Binding::outgoing`] hands the buffer's text to the store when it changed since
//!   the last hand-over (the host calls it debounced, and when focus leaves).
//! * [`Binding::incoming`] is told what the store shows. Text the binding wrote itself,
//!   or already saw, is an echo and never touches the buffer, however many edits were
//!   typed meanwhile. Text that is new to it (the store's own undo, another person's
//!   save, an agent's write) is applied as one undoable edit of the part that differs,
//!   so carets stay where they are and the buffer's undo brings the typed text back.

use crate::editor::{Editor, EditorError};
use std::hash::{Hash, Hasher};

/// What [`Binding::incoming`] did with the store's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incoming {
    /// The store shows what the binding last wrote or saw: nothing to do.
    Echo,
    /// New to the binding, but the buffer already says the same.
    Unchanged,
    /// The store changed on its own; the buffer took the change as one edit.
    Applied,
    /// The store changed but the buffer refused the edit (read-only, a locked range).
    Refused,
}

/// One buffer's link to its store. Cheap: a hash and a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Hash of the text both sides last agreed on.
    synced: u64,
    /// Buffer revision at the last hand-over in either direction.
    flushed: u64,
}

fn hash(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl Binding {
    /// A binding for a buffer just opened from `text` of the store.
    pub fn new(editor: &Editor, text: &str) -> Self {
        Self {
            synced: hash(text),
            flushed: editor.revision(),
        }
    }

    /// True when the buffer has edits the store has not been handed yet.
    pub fn pending(&self, editor: &Editor) -> bool {
        editor.revision() != self.flushed
    }

    /// The store shows `text`.
    pub fn incoming(&mut self, editor: &mut Editor, text: &str) -> Incoming {
        let h = hash(text);
        if h == self.synced {
            return Incoming::Echo;
        }
        if *editor.document().rope() == text {
            self.synced = h;
            self.flushed = editor.revision();
            return Incoming::Unchanged;
        }
        match editor.set_text(text) {
            Ok(()) => {
                self.synced = h;
                self.flushed = editor.revision();
                Incoming::Applied
            }
            Err(EditorError::ReadOnly | EditorError::Locked { .. }) => {
                // a read-only buffer still shows what the store holds
                editor.force_text(text);
                self.synced = h;
                self.flushed = editor.revision();
                Incoming::Applied
            }
            Err(_) => Incoming::Refused,
        }
    }

    /// The text to write to the store, when the buffer changed since the last
    /// hand-over and no longer says what the store holds.
    pub fn outgoing(&mut self, editor: &Editor) -> Option<String> {
        if !self.pending(editor) {
            return None;
        }
        self.flushed = editor.revision();
        let text = editor.text();
        let h = hash(&text);
        if h == self.synced {
            return None;
        }
        self.synced = h;
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;

    fn ed(text: &str) -> Editor {
        Editor::open("a.yaml", text)
    }

    #[test]
    fn an_echo_never_overwrites_newer_typing() {
        let mut store = String::from("a: 1\n");
        let mut e = ed(&store);
        let mut b = Binding::new(&e, &store);
        e.execute(Command::MoveDocEnd(false)).unwrap();
        e.insert("b").unwrap();
        // hand-over
        store = b.outgoing(&e).expect("changed");
        assert_eq!(store, "a: 1\nb");
        // more typing before the store's render comes back
        e.insert(": 2").unwrap();
        assert_eq!(b.incoming(&mut e, &store), Incoming::Echo);
        assert_eq!(e.text(), "a: 1\nb: 2");
        // any number of re-renders with what we wrote change nothing
        for _ in 0..3 {
            assert_eq!(b.incoming(&mut e, &store), Incoming::Echo);
        }
        assert_eq!(b.outgoing(&e).as_deref(), Some("a: 1\nb: 2"));
        assert_eq!(b.outgoing(&e), None);
    }

    #[test]
    fn a_change_of_the_store_is_one_undoable_edit_that_keeps_the_caret() {
        let store = "name: x\nsteps: []\n";
        let mut e = ed(store);
        let mut b = Binding::new(&e, store);
        e.execute(Command::MoveDocEnd(false)).unwrap();
        e.insert("tail").unwrap();
        let typed = e.text();
        // the store was changed by someone else at the top
        let other = "name: renamed\nsteps: []\n";
        assert_eq!(b.incoming(&mut e, other), Incoming::Applied);
        assert_eq!(e.text(), other);
        assert!(!b.pending(&e));
        // nothing typed is lost for good: the buffer's undo brings it back
        e.execute(Command::Undo).unwrap();
        assert_eq!(e.text(), typed);
    }

    #[test]
    fn two_views_typing_into_one_buffer_lose_nothing() {
        // Two views share one buffer and one binding; each hands over and hears the
        // store's echo at its own pace, in every interleaving of a short script.
        let script = [
            "A", "B", "flush", "A", "echo", "B", "flush", "echo", "A", "B",
        ];
        for rotate in 0..script.len() {
            let mut store = String::new();
            let mut e = ed("");
            let mut b = Binding::new(&e, &store);
            let mut typed = String::new();
            for i in 0..script.len() {
                match script[(i + rotate) % script.len()] {
                    "A" => {
                        e.execute(Command::MoveDocEnd(false)).unwrap();
                        e.insert("a").unwrap();
                        typed.push('a');
                    }
                    "B" => {
                        e.execute(Command::MoveDocEnd(false)).unwrap();
                        e.insert("b").unwrap();
                        typed.push('b');
                    }
                    "flush" => {
                        if let Some(t) = b.outgoing(&e) {
                            store = t;
                        }
                    }
                    _ => {
                        let before = e.text();
                        assert_ne!(b.incoming(&mut e, &store), Incoming::Applied);
                        assert_eq!(e.text(), before, "an echo changed the buffer");
                    }
                }
                assert_eq!(e.text(), typed, "rotation {rotate}, step {i}");
            }
            if let Some(t) = b.outgoing(&e) {
                store = t;
            }
            assert_eq!(store, typed);
        }
    }

    #[test]
    fn a_read_only_buffer_follows_the_store() {
        let mut e = ed("a: 1\n");
        e.set_mode(crate::editor::Mode::ReadOnly);
        let mut b = Binding::new(&e, "a: 1\n");
        assert_eq!(b.incoming(&mut e, "a: 2\n"), Incoming::Applied);
        assert_eq!(e.text(), "a: 2\n");
        assert_eq!(b.outgoing(&e), None);
    }
}
