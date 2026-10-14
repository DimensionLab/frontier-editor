// SPDX-License-Identifier: Apache-2.0
//! Commands and the keymap. Keys are platform-neutral: `Modifiers::PRIMARY` is Cmd on
//! macOS and Ctrl elsewhere, resolved by the host when it builds a [`Key`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    /// Cmd on macOS, Ctrl elsewhere.
    pub primary: bool,
    /// Ctrl on macOS (the non-primary control key); same as `primary` elsewhere.
    pub ctrl: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        shift: false,
        alt: false,
        primary: false,
        ctrl: false,
    };
    pub const SHIFT: Self = Self {
        shift: true,
        ..Self::NONE
    };
    pub const ALT: Self = Self {
        alt: true,
        ..Self::NONE
    };
    pub const PRIMARY: Self = Self {
        primary: true,
        ..Self::NONE
    };
    pub const PRIMARY_SHIFT: Self = Self {
        primary: true,
        shift: true,
        ..Self::NONE
    };
    pub const ALT_SHIFT: Self = Self {
        alt: true,
        shift: true,
        ..Self::NONE
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Key {
    pub code: KeyCode,
    pub mods: Modifiers,
}

impl Key {
    pub fn new(code: KeyCode, mods: Modifiers) -> Self {
        Self { code, mods }
    }
    pub fn ch(c: char, mods: Modifiers) -> Self {
        Self::new(KeyCode::Char(c.to_ascii_lowercase()), mods)
    }

    /// Parses `"primary-shift-z"`, `"alt-up"`, `"escape"`, `"f3"`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut mods = Modifiers::NONE;
        let parts: Vec<&str> = s.split('-').collect();
        let (last, prefix) = parts.split_last()?;
        for p in prefix {
            match p.to_ascii_lowercase().as_str() {
                "shift" => mods.shift = true,
                "alt" | "option" => mods.alt = true,
                "primary" | "mod" | "cmd" => mods.primary = true,
                "ctrl" | "control" => mods.ctrl = true,
                _ => return None,
            }
        }
        let code = match last.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "delete" => KeyCode::Delete,
            "escape" | "esc" => KeyCode::Escape,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "space" => KeyCode::Char(' '),
            f if f.len() > 1
                && f.starts_with('f')
                && f[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                KeyCode::F(f[1..].parse().ok()?)
            }
            c => {
                let mut it = c.chars();
                let ch = it.next()?;
                if it.next().is_some() {
                    return None;
                }
                KeyCode::Char(ch)
            }
        };
        Some(Self { code, mods })
    }
}

/// Everything the editor can be asked to do. Hosts add their own through
/// [`Command::Custom`] and the plugin registry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case", tag = "cmd", content = "arg")]
pub enum Command {
    // movement; `bool` = extend selection
    MoveLeft(bool),
    MoveRight(bool),
    MoveUp(bool),
    MoveDown(bool),
    MoveWordLeft(bool),
    MoveWordRight(bool),
    MoveLineStart(bool),
    MoveLineEnd(bool),
    MoveDocStart(bool),
    MoveDocEnd(bool),
    PageUp(bool),
    PageDown(bool),
    // selection
    SelectAll,
    SelectLine,
    SelectWord,
    /// Add a cursor at the next occurrence of the selected word (Cmd+D).
    SelectNextOccurrence,
    AddCursorAbove,
    AddCursorBelow,
    CollapseSelections,
    // editing
    InsertText(String),
    InsertNewline,
    InsertTab,
    Backspace,
    Delete,
    DeleteWordLeft,
    DeleteWordRight,
    DeleteLine,
    DuplicateLine,
    MoveLineUp,
    MoveLineDown,
    ToggleComment,
    Indent,
    Outdent,
    Undo,
    Redo,
    // search
    FindNext,
    FindPrevious,
    // structure
    ToggleFold,
    FoldAll,
    UnfoldAll,
    GoToMatchingBracket,
    /// Move the caret to the start of this line (1-based, clamped).
    GoToLine(usize),
    // host-level
    Save,
    /// Kind-specific apply (recipe *Plan*, settings *Validate*).
    Apply,
    /// Leave the editor (the host asks when dirty).
    Leave,
    /// Accept / reject a hunk by id.
    AcceptHunk(String),
    RejectHunk(String),
    Custom(String),
}

/// Key → command table. VS Code / Zed defaults for the basics.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<Key, Command>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut k = Self {
            bindings: HashMap::new(),
        };
        use Command::*;
        let table: &[(&str, Command)] = &[
            ("left", MoveLeft(false)),
            ("shift-left", MoveLeft(true)),
            ("right", MoveRight(false)),
            ("shift-right", MoveRight(true)),
            ("up", MoveUp(false)),
            ("shift-up", MoveUp(true)),
            ("down", MoveDown(false)),
            ("shift-down", MoveDown(true)),
            ("alt-left", MoveWordLeft(false)),
            ("alt-shift-left", MoveWordLeft(true)),
            ("alt-right", MoveWordRight(false)),
            ("alt-shift-right", MoveWordRight(true)),
            ("home", MoveLineStart(false)),
            ("shift-home", MoveLineStart(true)),
            ("end", MoveLineEnd(false)),
            ("shift-end", MoveLineEnd(true)),
            ("primary-left", MoveLineStart(false)),
            ("primary-shift-left", MoveLineStart(true)),
            ("primary-right", MoveLineEnd(false)),
            ("primary-shift-right", MoveLineEnd(true)),
            ("primary-up", MoveDocStart(false)),
            ("primary-shift-up", MoveDocStart(true)),
            ("primary-down", MoveDocEnd(false)),
            ("primary-shift-down", MoveDocEnd(true)),
            ("primary-home", MoveDocStart(false)),
            ("primary-end", MoveDocEnd(false)),
            ("pageup", PageUp(false)),
            ("shift-pageup", PageUp(true)),
            ("pagedown", PageDown(false)),
            ("shift-pagedown", PageDown(true)),
            ("primary-a", SelectAll),
            ("primary-l", SelectLine),
            ("primary-d", SelectNextOccurrence),
            ("alt-shift-up", AddCursorAbove),
            ("alt-shift-down", AddCursorBelow),
            ("escape", CollapseSelections),
            ("enter", InsertNewline),
            ("tab", InsertTab),
            ("shift-tab", Outdent),
            ("backspace", Backspace),
            ("delete", Delete),
            ("alt-backspace", DeleteWordLeft),
            ("alt-delete", DeleteWordRight),
            ("primary-shift-k", DeleteLine),
            ("primary-shift-d", DuplicateLine),
            ("alt-up", MoveLineUp),
            ("alt-down", MoveLineDown),
            ("primary-/", ToggleComment),
            ("primary-]", Indent),
            ("primary-[", Outdent),
            ("primary-z", Undo),
            ("primary-shift-z", Redo),
            ("primary-y", Redo),
            ("primary-g", FindNext),
            ("primary-shift-g", FindPrevious),
            ("f3", FindNext),
            ("shift-f3", FindPrevious),
            ("primary-alt-[", ToggleFold),
            ("primary-shift-m", GoToMatchingBracket),
            ("primary-s", Save),
            ("primary-enter", Apply),
        ];
        for (ks, cmd) in table {
            if let Some(key) = Key::parse(ks) {
                k.bindings.insert(key, cmd.clone());
            }
        }
        k
    }
}

impl Keymap {
    pub fn empty() -> Self {
        Self {
            bindings: HashMap::new(),
        }
    }

    pub fn bind(&mut self, key: &str, cmd: Command) -> bool {
        match Key::parse(key) {
            Some(k) => {
                self.bindings.insert(k, cmd);
                true
            }
            None => false,
        }
    }

    pub fn unbind(&mut self, key: &str) {
        if let Some(k) = Key::parse(key) {
            self.bindings.remove(&k);
        }
    }

    /// Resolves a key. Unmodified printable chars with no binding become
    /// `InsertText`; `Escape` without a binding is `Leave`.
    pub fn resolve(&self, key: &Key) -> Option<Command> {
        if let Some(c) = self.bindings.get(key) {
            return Some(c.clone());
        }
        // Shift+char: look up without shift (the host already produced the shifted char).
        if key.mods.shift
            && let KeyCode::Char(_) = key.code
        {
            let unshifted = Key::new(
                key.code.clone(),
                Modifiers {
                    shift: false,
                    ..key.mods
                },
            );
            if let Some(c) = self.bindings.get(&unshifted) {
                return Some(c.clone());
            }
        }
        match key.code {
            KeyCode::Char(c)
                if !key.mods.primary && !key.mods.ctrl && !key.mods.alt && !c.is_control() =>
            {
                Some(Command::InsertText(c.to_string()))
            }
            KeyCode::Escape => Some(Command::Leave),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_resolve() {
        let km = Keymap::default();
        assert_eq!(
            km.resolve(&Key::parse("primary-z").expect("k")),
            Some(Command::Undo)
        );
        assert_eq!(
            km.resolve(&Key::parse("primary-shift-z").expect("k")),
            Some(Command::Redo)
        );
        assert_eq!(
            km.resolve(&Key::ch('x', Modifiers::NONE)),
            Some(Command::InsertText("x".into()))
        );
        assert_eq!(km.resolve(&Key::ch('x', Modifiers::PRIMARY)), None);
        assert_eq!(
            km.resolve(&Key::parse("f3").expect("k")),
            Some(Command::FindNext)
        );
        assert_eq!(
            km.resolve(&Key::parse("escape").expect("k")),
            Some(Command::CollapseSelections)
        );
        assert!(Key::parse("bogus-x").is_none());
    }
}
