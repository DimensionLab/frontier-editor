// SPDX-License-Identifier: Apache-2.0
//! Plugin interface: languages, lints, commands and decorations contributed by
//! in-process plugins. Registered once per editor; the editor calls them on load,
//! after every transaction (lints, decorations) and when a `Command::Custom` fires.

use crate::command::Command;
use crate::decoration::{Decoration, Diagnostic};
use crate::document::Document;
use crate::kind::FileKind;
use crate::syntax::Language;
use std::fmt;

/// A language contributed by a plugin: an extension mapping plus a line comment
/// token. Highlighting for plugin languages uses the fallback lexer with the
/// plugin's keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageContribution {
    pub extensions: Vec<String>,
    pub language: Language,
}

/// A command contributed by a plugin, reachable as `Command::Custom(id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandContribution {
    pub id: String,
    pub title: String,
    /// Default key, e.g. `"primary-shift-p"`.
    pub key: Option<String>,
}

/// Outcome of a plugin command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutcome {
    /// Replacement for the whole text, if the command rewrites it (formatters).
    pub new_text: Option<String>,
    pub message: Option<String>,
}

pub trait Plugin {
    fn id(&self) -> &str;

    fn languages(&self) -> Vec<LanguageContribution> {
        Vec::new()
    }

    fn commands(&self) -> Vec<CommandContribution> {
        Vec::new()
    }

    /// Diagnostics for the current text. Called after each transaction, debounced by
    /// the host. Keep it fast or return early for kinds you do not handle.
    fn lint(&self, _doc: &Document, _kind: FileKind) -> Vec<Diagnostic> {
        Vec::new()
    }

    /// Extra decorations (e.g. inline hints) for the current text.
    fn decorate(&self, _doc: &Document, _kind: FileKind) -> Vec<Decoration> {
        Vec::new()
    }

    fn run(&self, _id: &str, _doc: &Document, _kind: FileKind) -> Option<CommandOutcome> {
        None
    }
}

/// In-crate plugin registry.
#[derive(Default)]
pub struct Registry {
    plugins: Vec<Box<dyn Plugin>>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.plugins.iter().map(|p| p.id()))
            .finish()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.retain(|p| p.id() != plugin.id());
        self.plugins.push(plugin);
    }

    pub fn ids(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.id()).collect()
    }

    pub fn language_for_extension(&self, ext: &str) -> Option<Language> {
        self.plugins
            .iter()
            .flat_map(|p| p.languages())
            .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .map(|l| l.language)
    }

    pub fn commands(&self) -> Vec<CommandContribution> {
        self.plugins.iter().flat_map(|p| p.commands()).collect()
    }

    pub fn lint(&self, doc: &Document, kind: FileKind) -> Vec<Diagnostic> {
        self.plugins
            .iter()
            .flat_map(|p| p.lint(doc, kind))
            .collect()
    }

    pub fn decorate(&self, doc: &Document, kind: FileKind) -> Vec<Decoration> {
        self.plugins
            .iter()
            .flat_map(|p| p.decorate(doc, kind))
            .collect()
    }

    pub fn run(&self, id: &str, doc: &Document, kind: FileKind) -> Option<CommandOutcome> {
        self.plugins.iter().find_map(|p| p.run(id, doc, kind))
    }

    /// Keys declared by plugin commands, for the host to merge into its keymap.
    pub fn default_bindings(&self) -> Vec<(String, Command)> {
        self.commands()
            .into_iter()
            .filter_map(|c| c.key.map(|k| (k, Command::Custom(c.id))))
            .collect()
    }
}

/// Built-in lint: trailing whitespace and tabs in YAML.
#[derive(Debug, Default)]
pub struct WhitespaceLint;

impl Plugin for WhitespaceLint {
    fn id(&self) -> &str {
        "core.whitespace"
    }

    fn lint(&self, doc: &Document, kind: FileKind) -> Vec<Diagnostic> {
        use crate::decoration::Severity;
        use crate::document::Range;
        let mut out = Vec::new();
        let yaml = kind.language() == Language::Yaml;
        for i in 0..doc.line_count().min(200_000) {
            let line = doc.line(i);
            let base = doc.line_start(i);
            if yaml && line.starts_with('\t') {
                out.push(Diagnostic::new(
                    Range::new(base, base + 1),
                    Severity::Warning,
                    self.id(),
                    "YAML indentation must use spaces",
                ));
            }
            let trimmed = line.trim_end_matches([' ', '\t']);
            if trimmed.len() < line.len() {
                let s = base + trimmed.chars().count();
                out.push(Diagnostic::new(
                    Range::new(s, base + line.chars().count()),
                    Severity::Hint,
                    self.id(),
                    "trailing whitespace",
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_runs_lints() {
        let mut r = Registry::new();
        r.register(Box::new(WhitespaceLint));
        let doc = Document::from_str("a: 1  \n\tb: 2\n");
        let d = r.lint(&doc, FileKind::Yaml);
        assert_eq!(d.len(), 2);
        assert_eq!(r.ids(), vec!["core.whitespace"]);
    }
}
