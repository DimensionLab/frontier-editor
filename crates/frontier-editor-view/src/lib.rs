// SPDX-License-Identifier: Apache-2.0
//! GPU text view for `frontier-editor`: virtualised rendering of documents, diffs
//! and multi-million-line logs with wgpu, drawn into the host's own render pass.
//!
//! The core owns every piece of editor state; this crate turns an [`Editor`] (or a
//! `ChunkedDocument` for logs) plus a viewport into instanced quads, and turns
//! pointer and keyboard input into core commands. Text input goes through a hidden
//! platform text field (see [`input::HostInput`]) so IME, clipboard and screen
//! readers behave natively on the web, in the desktop WebView and under winit.
//!
//! Host API (what the app wires):
//!
//! ```ignore
//! let mut view = EditorView::new(ViewConfig::default(), Theme::dark());
//! view.resize(logical_w, logical_h, dpr);
//! view.open("configs/run.yaml", text);                 // or open_editor / open_log
//! for req in view.handle(InputEvent::Text("x".into()), now_ms) { /* Save, Copy, ... */ }
//! view.prepare(&device, &queue, surface_format, 1, &Placement::fill(w, h), now_ms);
//! // inside the host's render pass, after the 3D viewers:
//! view.render(&mut pass);
//! ```

#![forbid(unsafe_code)]

pub mod diff_view;
pub mod frame;
pub mod input;
pub mod render;
pub mod text;
pub mod theme;
pub mod view;
#[cfg(all(target_arch = "wasm32", feature = "web"))]
pub mod web;

pub use frontier_editor::{self as core, Editor};
pub use input::{HostInput, HostRequest, InputEvent, Key, KeyCode, Modifiers};
pub use render::{Instance, Placement, Renderer};
pub use text::{GlyphAtlas, Metrics, TextSystem};
pub use theme::Theme;
pub use view::{
    Content, CursorStyle, DiffMode, DiffSummary, EditorView, FrameStats, Geometry, Hit,
    SharedEditor, ViewConfig,
};
