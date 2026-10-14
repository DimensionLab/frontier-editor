// SPDX-License-Identifier: Apache-2.0
//! Diagnostics and decorations: things drawn over or beside the text that are not
//! the text.

use crate::document::Range;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Hint,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub message: String,
    /// Who produced it: `schema`, `syntax`, a plugin id, ...
    pub source: String,
    /// Machine-readable code (a JSON pointer for schema errors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Diagnostic {
    pub fn new(range: Range, severity: Severity, source: &str, message: impl Into<String>) -> Self {
        Self {
            range,
            severity,
            message: message.into(),
            source: source.to_string(),
            code: None,
        }
    }
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum DecorationKind {
    /// Background highlight across the range (search match, bracket, hunk, ...).
    Highlight { class: String },
    /// A mark in the gutter of the line containing `range.start`.
    GutterMark {
        class: String,
        tooltip: Option<String>,
    },
    /// Text drawn after the range on the same line (inline diagnostic, lock reason).
    InlineWidget { text: String, class: String },
    /// A lock glyph plus dimmed text; the range cannot be edited.
    ReadOnly { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Decoration {
    pub range: Range,
    pub kind: DecorationKind,
    /// Owner: `core`, `search`, `brackets`, a plugin id, ...
    pub owner: String,
}

impl Decoration {
    pub fn highlight(range: Range, class: &str, owner: &str) -> Self {
        Self {
            range,
            kind: DecorationKind::Highlight {
                class: class.to_string(),
            },
            owner: owner.to_string(),
        }
    }
}
