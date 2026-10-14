// SPDX-License-Identifier: Apache-2.0
//! Frontier editor core.
//!
//! Everything an editor needs except pixels: a rope document with stable
//! line/column and byte coordinates, multi-cursor selections, transactions with an
//! undo tree, a hunk model with per-hunk accept/reject, syntax highlighting
//! (tree-sitter natively, a lexer fallback on wasm), folding, bracket matching,
//! regex search/replace, JSON-Schema diagnostics for JSON/YAML/TOML, read-only
//! ranges, decorations, a command table with a keymap, a plugin registry and
//! provenance events on save.
//!
//! The view is a pure function of [`Editor`] state and a [`Viewport`]: call
//! [`Editor::view_state`] and draw what comes back. Nothing here touches a window,
//! a DOM or a GPU; `frontier-editor-view` does that with wgpu.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

#[cfg(feature = "schema")]
pub mod assist;
pub mod brackets;
pub mod command;
pub mod decoration;
pub mod diff;
pub mod document;
pub mod editor;
pub mod fold;
pub mod format;
pub mod history;
pub mod kind;
pub mod layout;
pub mod plugin;
pub mod provenance;
pub mod readonly;
mod rows;
#[cfg(feature = "schema")]
pub mod schema;
pub mod search;
pub mod selection;
pub mod source;
pub mod sync;
pub mod syntax;
pub mod transaction;
pub mod view;

pub use command::{Command, Key, KeyCode, Keymap, Modifiers};
pub use decoration::{Decoration, DecorationKind, Diagnostic, Severity};
pub use diff::{ChangeSet, Hunk, HunkKind, HunkLine, LineOp};
pub use document::{Document, Position, Range};
pub use editor::{Editor, EditorError, Mode};
pub use history::{History, UndoGroup};
pub use kind::FileKind;
pub use plugin::{Plugin, Registry};
pub use provenance::{ApprovalEvent, Author, EditEvent, content_hash};
pub use readonly::ReadOnlyRange;
pub use selection::{Selection, SelectionSet};
pub use source::{ChunkedDocument, DocumentSource, LineIndex, MemorySource};
pub use sync::{Binding, Incoming};
pub use syntax::{Highlighter, Language, Span, TokenKind};
pub use transaction::{Change, Transaction};
pub use view::{GutterMark, ViewLine, ViewState, Viewport};
