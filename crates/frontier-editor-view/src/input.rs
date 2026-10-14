// SPDX-License-Identifier: Apache-2.0
//! Input to the view and requests back to the host.
//!
//! Text input never comes from raw key codes: the host owns a hidden, focused text
//! field (a `<textarea>` on the web and in the Dioxus desktop WebView, winit's IME
//! events natively) and forwards what the platform produced, so IME composition,
//! dead keys, clipboard and screen readers work the way the OS expects. See
//! [`HostInput`] for the contract that overlay implements.

pub use frontier_editor::command::{Key, KeyCode, Modifiers};
use frontier_editor::provenance::ApprovalEvent;

/// Pointer and keyboard events in the view's own logical-pixel coordinates
/// (origin at the view's top-left, before the host transform).
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// A key the overlay did not turn into text (arrows, shortcuts, Enter, Tab, ...).
    Key(Key),
    /// Committed text: typing, IME commit.
    Text(String),
    /// Text from the clipboard (one undo step; spread over the carets when it has as
    /// many lines as there are carets).
    Paste(String),
    CompositionStart,
    CompositionUpdate(String),
    CompositionEnd(String),
    PointerDown {
        x: f32,
        y: f32,
        button: u8,
        clicks: u8,
        mods: Modifiers,
    },
    PointerMove {
        x: f32,
        y: f32,
    },
    PointerUp {
        x: f32,
        y: f32,
    },
    /// Scroll deltas in logical pixels (already multiplied for line/page modes).
    Wheel {
        dx: f32,
        dy: f32,
        mods: Modifiers,
    },
    /// Pinch / ctrl+wheel zoom factor (1.0 = unchanged).
    Zoom(f32),
    FocusIn,
    FocusOut,
}

/// What the view needs the host to do after handling an event.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum HostRequest {
    /// `Cmd/Ctrl+S`: call `editor_mut().save(author)` and persist the event.
    Save,
    /// `Cmd/Ctrl+Enter`: kind-specific apply (recipe *Plan*, settings *Validate*).
    Apply,
    /// `Esc`: leave the editor (ask first when `editor().is_dirty()`).
    Leave,
    /// Put this text on the clipboard.
    Copy(String),
    /// Ask the clipboard for text and feed it back as `InputEvent::Text`.
    Paste,
    /// A plugin command the registry did not handle.
    Custom(String),
    /// The reviewer finished a proposal; persist the approval.
    Approval(ApprovalEvent),
    /// The caret moved: place the IME candidate window at this logical rect.
    CaretMoved { x: f32, y: f32, w: f32, h: f32 },
    /// The view wants a redraw soon (smooth scroll, blink).
    Redraw,
}

/// The contract of the hidden input overlay. Implemented by the web host
/// (`web::Overlay`, a transparent `<textarea>`), by the Dioxus desktop host the same
/// way, and by a winit host with its IME API. The view calls these; the overlay
/// turns platform events into [`InputEvent`]s.
pub trait HostInput {
    /// Give the overlay keyboard focus.
    fn focus(&mut self);
    /// Position the (invisible) field so IME candidate windows open at the caret.
    fn set_caret_rect(&mut self, x: f32, y: f32, w: f32, h: f32);
    /// Mirror the selected text for screen readers and native copy.
    fn set_selection_text(&mut self, text: &str);
    /// Clipboard write; the overlay may instead rely on the platform `copy` event.
    fn write_clipboard(&mut self, text: &str);
}

/// Tracks a pointer drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Drag {
    Text,
    /// A double-click being dragged: word by word from the word at `anchor`.
    Words {
        anchor: frontier_editor::document::Position,
    },
    /// A triple-click (or a gutter click) being dragged: line by line.
    Lines {
        anchor: usize,
    },
    /// Alt held: a block of columns from `anchor`.
    Column {
        anchor: frontier_editor::document::Position,
    },
    Scrollbar {
        grab_offset: f32,
    },
    Minimap,
}
