// SPDX-License-Identifier: Apache-2.0
//! The editor: one document, its selections, history, syntax, diagnostics, folds,
//! locks, search state and (optionally) a proposal under review. Hosts feed it
//! commands and pointer positions and draw [`ViewState`]s.

use crate::brackets;
use crate::command::{Command, Key, Keymap};
use crate::decoration::{Decoration, DecorationKind, Diagnostic, Severity};
use crate::diff::{ChangeSet, HunkKind};
use crate::document::{Document, Position, Range};
use crate::fold::{FoldRange, fold_range_at, fold_ranges};
use crate::history::{History, UndoGroup};
use crate::kind::FileKind;
use crate::layout::LineLayout;
use crate::plugin::Registry;
use crate::provenance::{ApprovalEvent, Author, EditEvent, content_hash};
use crate::readonly::{ReadOnlyRange, map_locks, violated};
use crate::search::{Search, SearchOptions};
use crate::selection::{Selection, SelectionSet};
use crate::syntax::{Highlighter, Language};
use crate::transaction::{Change, EditKind, Transaction};
use crate::view::{
    FoldState, GutterMark, RowHighlight, RowWidget, ViewLine, ViewState, Viewport, clip_spans,
};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EditorError {
    #[error("document is read-only")]
    ReadOnly,
    #[error("range is locked: {reason}")]
    Locked { reason: String },
    #[error("transaction is malformed (overlapping changes)")]
    Malformed,
    #[error("no proposal under review")]
    NotInReview,
    #[error("unknown hunk {0}")]
    UnknownHunk(String),
    #[error("search: {0}")]
    Search(String),
    /// Why the text was not formatted, in words a person can read.
    #[error("{0}")]
    Format(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Edit,
    ReadOnly,
    /// A proposal is under review: the document is the base, hunks come from
    /// [`Editor::changeset`].
    Review,
}

/// What a command asks the host to do after the editor handled its part.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Outcome {
    #[default]
    None,
    /// Call [`Editor::save`] with an author and persist the event.
    Save,
    Apply,
    Leave,
    Custom(String),
}

/// Host-tunable behaviour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub tab_size: usize,
    pub insert_spaces: bool,
    /// Rows per page for PageUp/PageDown; the view sets it from its height.
    pub page_rows: usize,
    pub run_lints: bool,
    pub max_search_matches: usize,
    /// Typing an opening bracket or quote inserts its pair; typing the closing one
    /// over an existing one moves past it; Backspace inside an empty pair removes both.
    pub auto_close: bool,
    /// Soft wrap: rows this many cells wide (`None`: lines are as long as they are).
    /// The view sets it from its width.
    pub wrap_cols: Option<usize>,
    /// Schema validation and lints run this long after the last edit instead of
    /// inside it (0 = inside the edit). The host calls [`Editor::poll`].
    pub analysis_delay_ms: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            tab_size: 2,
            insert_spaces: true,
            page_rows: 40,
            run_lints: true,
            max_search_matches: 10_000,
            auto_close: true,
            wrap_cols: None,
            analysis_delay_ms: 0,
        }
    }
}

pub struct Editor {
    path: String,
    kind: FileKind,
    language: Language,
    doc: Document,
    selections: SelectionSet,
    history: History,
    highlighter: Highlighter,
    mode: Mode,
    locks: Vec<ReadOnlyRange>,
    /// Diagnostics by source (schema, plugins, host-pushed).
    diagnostics: BTreeMap<String, Vec<Diagnostic>>,
    decorations: Vec<Decoration>,
    /// Folded ranges, header line -> last hidden line.
    folded: BTreeMap<usize, usize>,
    search: Option<(Search, Vec<Range>)>,
    registry: Registry,
    keymap: Keymap,
    review: Option<(ChangeSet, BTreeSet<String>)>,
    saved_text: String,
    now_ms: u64,
    group: Option<UndoGroup>,
    /// Counts every change of the text (edits, undo, redo).
    revision: u64,
    /// When deferred analysis has to run (host clock).
    analysis_due: Option<u64>,
    /// Rows per line under soft wrap, and the folds it was measured with.
    rowmap: Option<(crate::rows::RowMap, BTreeMap<usize, usize>)>,
    pub options: Options,
    #[cfg(feature = "schema")]
    schema: Option<std::sync::Arc<crate::schema::SchemaEntry>>,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("mode", &self.mode)
            .field("doc", &self.doc)
            .field("selections", &self.selections.len())
            .finish()
    }
}

impl Editor {
    /// Opens `text` as `path`. Kind and language are detected from the path and the
    /// first line; override with [`Editor::set_kind`].
    pub fn open(path: &str, text: &str) -> Self {
        let head: String = text.lines().next().unwrap_or("").to_string();
        let kind = FileKind::detect(path, &head);
        let language = match kind.language() {
            // a kind without a language of its own: by the file's extension (scripts)
            Language::Plain if kind == FileKind::Text => {
                let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
                match name.rsplit_once('.') {
                    Some((_, ext)) => Language::from_extension(ext),
                    None if head.starts_with("#!") && head.contains("sh") => Language::Shell,
                    None => Language::Plain,
                }
            }
            l => l,
        };
        let doc = Document::from_str(text);
        let mut highlighter = Self::highlighter_for(language, &doc);
        highlighter.set_text(&doc);
        let mut ed = Self {
            path: path.to_string(),
            kind,
            language,
            doc,
            selections: SelectionSet::default(),
            history: History::new(),
            highlighter,
            mode: if kind.is_editable() {
                Mode::Edit
            } else {
                Mode::ReadOnly
            },
            locks: Vec::new(),
            diagnostics: BTreeMap::new(),
            decorations: Vec::new(),
            folded: BTreeMap::new(),
            search: None,
            registry: Registry::new(),
            keymap: Keymap::default(),
            review: None,
            saved_text: text.to_string(),
            now_ms: 0,
            group: None,
            revision: 0,
            analysis_due: None,
            rowmap: None,
            options: Options::default(),
            #[cfg(feature = "schema")]
            schema: None,
        };
        ed.refresh_analysis();
        ed
    }

    /// Documents above this size skip schema validation (they are viewers, not
    /// editable kinds; the app's editor budget is 4 MB).
    pub const ANALYSIS_BUDGET_BYTES: usize = 8 * 1024 * 1024;
    /// Documents above this size are highlighted by the line lexer instead of a
    /// tree-sitter parse. The incremental re-parse after a keystroke grows with the
    /// file: on a 10,000-line (290 kB) YAML file it took 11 to 20 ms of every key in
    /// an optimised build, while the lexer only touches visible lines.
    pub const TREE_SITTER_BUDGET_BYTES: usize = 64 * 1024;

    fn highlighter_for(language: Language, doc: &Document) -> Highlighter {
        if doc.len_bytes() > Self::TREE_SITTER_BUDGET_BYTES {
            Highlighter::new_lexer(language)
        } else {
            Highlighter::new(language)
        }
    }

    // ----- accessors -------------------------------------------------------------

    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn kind(&self) -> FileKind {
        self.kind
    }
    pub fn language(&self) -> Language {
        self.language
    }
    pub fn document(&self) -> &Document {
        &self.doc
    }
    pub fn text(&self) -> String {
        self.doc.to_string()
    }
    pub fn selections(&self) -> &SelectionSet {
        &self.selections
    }
    /// Replaces the selections (clamped to the text).
    pub fn selections_set(&mut self, set: SelectionSet) {
        let n = self.doc.len_chars();
        self.selections = set.map(|s| Selection::new(s.anchor.min(n), s.head.min(n)));
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn is_dirty(&self) -> bool {
        self.history.is_dirty()
    }
    /// A counter that moves on every change of the text. Two readings that are equal
    /// mean the text is the same; views and stores compare it instead of the text.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// True while validation waits for typing to pause (see `Options::analysis_delay_ms`).
    pub fn analysis_pending(&self) -> bool {
        self.analysis_due.is_some()
    }
    /// Runs deferred analysis when it is due. Returns true when diagnostics changed.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        match self.analysis_due {
            Some(due) if now_ms >= due => {
                self.analysis_due = None;
                self.refresh_search();
                self.refresh_analysis();
                true
            }
            _ => false,
        }
    }
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }
    pub fn keymap_mut(&mut self) -> &mut Keymap {
        &mut self.keymap
    }
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub fn locks(&self) -> &[ReadOnlyRange] {
        &self.locks
    }
    pub fn highlighter(&self) -> &Highlighter {
        &self.highlighter
    }
    pub fn changeset(&self) -> Option<&ChangeSet> {
        self.review.as_ref().map(|(c, _)| c)
    }
    pub fn accepted_hunks(&self) -> Vec<String> {
        self.review
            .as_ref()
            .map(|(_, a)| a.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn search_matches(&self) -> &[Range] {
        self.search
            .as_ref()
            .map(|(_, m)| m.as_slice())
            .unwrap_or(&[])
    }
    pub fn folds(&self) -> &BTreeMap<usize, usize> {
        &self.folded
    }
    pub fn content_hash(&self) -> String {
        content_hash(&self.text())
    }

    /// All diagnostics, sorted by position.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut v: Vec<Diagnostic> = self.diagnostics.values().flatten().cloned().collect();
        v.sort_by_key(|d| (d.range.start, std::cmp::Reverse(d.severity)));
        v
    }

    // ----- configuration ---------------------------------------------------------

    /// Host clock, milliseconds. Drives undo grouping; call before each input.
    pub fn tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    pub fn set_kind(&mut self, kind: FileKind) {
        self.kind = kind;
        self.set_language(kind.language());
        self.refresh_analysis();
    }

    pub fn set_language(&mut self, language: Language) {
        if language != self.language || !self.highlighter.is_tree_sitter() {
            self.language = language;
            self.highlighter = Self::highlighter_for(language, &self.doc);
            self.highlighter.set_text(&self.doc);
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if mode != Mode::Review {
            self.review = None;
        }
        self.mode = mode;
    }

    pub fn set_locks(&mut self, locks: Vec<ReadOnlyRange>) {
        self.locks = locks;
    }

    pub fn register_plugin(&mut self, plugin: Box<dyn crate::plugin::Plugin>) {
        for (key, cmd) in plugin
            .commands()
            .into_iter()
            .filter_map(|c| c.key.map(|k| (k, Command::Custom(c.id))))
        {
            self.keymap.bind(&key, cmd);
        }
        self.registry.register(plugin);
        self.refresh_analysis();
    }

    /// Diagnostics pushed by the host (server-side validation, a language server).
    pub fn set_diagnostics(&mut self, source: &str, diags: Vec<Diagnostic>) {
        if diags.is_empty() {
            self.diagnostics.remove(source);
        } else {
            self.diagnostics.insert(source.to_string(), diags);
        }
    }

    pub fn set_decorations(&mut self, owner: &str, decos: Vec<Decoration>) {
        self.decorations.retain(|d| d.owner != owner);
        self.decorations.extend(decos);
    }

    #[cfg(feature = "schema")]
    pub fn set_schema(&mut self, entry: Option<std::sync::Arc<crate::schema::SchemaEntry>>) {
        self.schema = entry;
        self.refresh_analysis();
    }

    #[cfg(feature = "schema")]
    pub fn use_registry(&mut self, registry: &crate::schema::SchemaRegistry) {
        let entry = registry.resolve(&self.path, self.kind);
        self.set_schema(entry);
    }

    /// Starts a new undo group: edits until [`Editor::end_group`] become one step.
    pub fn begin_group(&mut self) {
        self.group = Some(UndoGroup(
            self.now_ms
                .wrapping_mul(31)
                .wrapping_add(self.history.len() as u64),
        ));
    }

    pub fn end_group(&mut self) {
        self.group = None;
        self.history.close_group();
    }

    // ----- transactions ----------------------------------------------------------

    /// Applies a transaction: lock check, document edit, selection mapping, syntax
    /// update, history, analysis. The selections after the edit are the mapped ones
    /// unless `after` is given.
    pub fn apply(
        &mut self,
        mut tx: Transaction,
        after: Option<SelectionSet>,
    ) -> Result<(), EditorError> {
        if self.mode == Mode::ReadOnly {
            return Err(EditorError::ReadOnly);
        }
        if !tx.is_well_formed() {
            return Err(EditorError::Malformed);
        }
        if tx.is_empty() {
            return Ok(());
        }
        if let Some(l) = violated(&self.locks, &tx) {
            return Err(EditorError::Locked {
                reason: l.reason.clone(),
            });
        }
        tx.selection_before = Some(self.selections.clone());
        let before = self.doc.clone();
        let mut inverse = tx.apply(&mut self.doc);
        let mapped = after.unwrap_or_else(|| tx.map_selections(&self.selections));
        // whatever a command computed, a selection never lies outside the text
        let n = self.doc.len_chars();
        let mapped = mapped.map(|s| {
            if s.anchor <= n && s.head <= n {
                s
            } else {
                Selection::new(s.anchor.min(n), s.head.min(n))
            }
        });
        tx.selection_after = Some(mapped.clone());
        inverse.selection_before = Some(mapped.clone());
        self.selections = mapped;
        self.highlighter.edit(&before, &self.doc, &tx);
        self.locks = map_locks(&self.locks, &tx);
        self.map_folds(&before, &tx);
        self.map_diagnostics(&tx);
        self.rows_edit(&before, &tx);
        self.history.push(tx, inverse, self.now_ms, self.group);
        self.after_edit();
        Ok(())
    }

    /// Moves folds through a transaction; folds the edit touched are opened.
    fn map_folds(&mut self, before: &Document, tx: &Transaction) {
        if self.folded.is_empty() {
            return;
        }
        let old = std::mem::take(&mut self.folded);
        for (start, end) in old {
            let s_off = before.line_start(start);
            let e_off = before.line_end(end);
            let touched = tx.changes.iter().any(|c| {
                c.range.overlaps(&Range::new(s_off, e_off + 1))
                    || (c.range.is_empty() && c.range.start > s_off && c.range.start <= e_off)
            });
            if touched {
                continue;
            }
            let ns = self
                .doc
                .offset_to_position(tx.map_offset(s_off, false))
                .line;
            let ne = self.doc.offset_to_position(tx.map_offset(e_off, true)).line;
            if ne > ns {
                self.folded.insert(ns, ne);
            }
        }
    }

    fn after_edit(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        if self.options.analysis_delay_ms == 0 {
            self.analysis_due = None;
            self.refresh_search();
            self.refresh_analysis();
        } else {
            self.analysis_due = Some(self.now_ms + self.options.analysis_delay_ms);
        }
    }

    fn refresh_search(&mut self) {
        if let Some((s, matches)) = &mut self.search {
            *matches = s.find_all(&self.doc, self.options.max_search_matches);
        }
    }

    /// Moves diagnostics through an edit so they stay on their text until the next
    /// analysis replaces them.
    fn map_diagnostics(&mut self, tx: &Transaction) {
        let n = self.doc.len_chars();
        if self.options.analysis_delay_ms > 0
            && let Some((_, matches)) = &mut self.search
        {
            for m in matches.iter_mut() {
                let a = tx.map_offset(m.start, false).min(n);
                let b = tx.map_offset(m.end, true).min(n).max(a);
                *m = Range::new(a, b);
            }
        }
        for list in self.diagnostics.values_mut() {
            for d in list.iter_mut() {
                let a = tx.map_offset(d.range.start, false).min(n);
                let b = tx.map_offset(d.range.end, true).min(n).max(a);
                d.range = Range::new(a, b);
            }
        }
    }

    /// Re-runs schema validation and plugin lints.
    pub fn refresh_analysis(&mut self) {
        #[cfg(feature = "schema")]
        {
            let diags = match &self.schema {
                Some(entry) if self.doc.len_bytes() <= Self::ANALYSIS_BUDGET_BYTES => {
                    crate::schema::SchemaRegistry::validate(entry, &self.text(), self.language)
                }
                _ => Vec::new(),
            };
            self.set_diagnostics(crate::schema::SOURCE, diags);
        }
        if self.options.run_lints {
            let ids: Vec<String> = self.registry.ids().into_iter().map(String::from).collect();
            let lints = self.registry.lint(&self.doc, self.kind);
            for id in &ids {
                self.diagnostics.remove(id);
            }
            for d in lints {
                self.diagnostics
                    .entry(d.source.clone())
                    .or_default()
                    .push(d);
            }
            let decos = self.registry.decorate(&self.doc, self.kind);
            self.decorations.retain(|d| !ids.contains(&d.owner));
            self.decorations.extend(decos);
        }
    }

    /// Replaces the whole text (formatter output, a newer version from the store) as one
    /// undo step. Only the part that differs is edited, so carets, folds and
    /// highlighting outside it stay where they are.
    pub fn set_text(&mut self, text: &str) -> Result<(), EditorError> {
        let Some((range, insert)) = diff_span(&self.doc, text) else {
            return Ok(());
        };
        let tx = Transaction::single(Change::replace(range, insert), EditKind::Replace);
        self.history.close_group();
        let r = self.apply(tx, None);
        self.history.close_group();
        r
    }

    // ----- language features -----------------------------------------------------

    /// Whether [`Editor::completions`] can answer from a schema (the cheap question a
    /// host asks before offering suggestions while typing).
    pub fn has_schema_completion(&self) -> bool {
        #[cfg(feature = "schema")]
        {
            self.schema.is_some()
                && matches!(
                    self.language,
                    Language::Json | Language::Yaml | Language::Toml
                )
                && self.doc.len_chars() <= crate::assist::ASSIST_BUDGET_CHARS
        }
        #[cfg(not(feature = "schema"))]
        {
            false
        }
    }

    /// What can be written at the caret: keys and values from the schema of this kind
    /// of file, or words from the file (the list's note says which). Empty with more
    /// than one caret or a selection.
    #[cfg(feature = "schema")]
    pub fn completions(&self) -> crate::assist::CompletionList {
        if self.mode != Mode::Edit || self.selections.len() != 1 {
            return Default::default();
        }
        let sel = self.selections.primary();
        if !sel.is_caret() {
            return Default::default();
        }
        crate::assist::completions(
            self.schema.as_deref(),
            &self.text(),
            self.language,
            sel.head,
        )
    }

    /// Inserts a completion as one undo step and leaves the selection it asks for.
    #[cfg(feature = "schema")]
    pub fn accept_completion(&mut self, c: &crate::assist::Completion) -> Result<(), EditorError> {
        let mut changes = vec![Change::replace(c.replace, c.insert.clone())];
        for (r, t) in &c.also {
            changes.push(Change::replace(*r, t.clone()));
        }
        let tx = Transaction::new(changes, EditKind::Replace);
        // where the inserted text starts once the edits before it are applied
        let start = (c.replace.start as isize
            + c.also
                .iter()
                .filter(|(r, _)| r.end <= c.replace.start)
                .map(|(r, t)| t.chars().count() as isize - r.len() as isize)
                .sum::<isize>())
        .max(0) as usize;
        let after = SelectionSet::single(Selection::new(start + c.select.0, start + c.select.1));
        self.history.close_group();
        let r = self.apply(tx, Some(after));
        self.history.close_group();
        r
    }

    /// What the schema says about the key or value at `offset`.
    #[cfg(feature = "schema")]
    pub fn hover(&self, offset: usize) -> Option<crate::assist::Hover> {
        let entry = self.schema.as_deref()?;
        crate::assist::hover(entry, &self.text(), self.language, offset)
    }

    /// The problems reported on the text at `offset`, most severe first.
    pub fn problems_at(&self, offset: usize) -> Vec<Diagnostic> {
        let mut out: Vec<Diagnostic> = self
            .diagnostics
            .values()
            .flatten()
            .filter(|d| d.range.start <= offset && offset <= d.range.end.max(d.range.start + 1))
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.range.len().cmp(&b.range.len()))
        });
        out
    }

    /// Fixes for the schema problems at `offset` (on its line when none is under it),
    /// the most specific first.
    #[cfg(feature = "schema")]
    pub fn quick_fixes(&self, offset: usize) -> Vec<crate::assist::QuickFix> {
        let Some(entry) = self.schema.as_deref() else {
            return Vec::new();
        };
        if self.mode != Mode::Edit {
            return Vec::new();
        }
        let all = crate::assist::quick_fixes(entry, &self.text(), self.language);
        let line = self
            .doc
            .line_range(self.doc.offset_to_position(offset).line);
        let mut under: Vec<_> = all
            .iter()
            .filter(|f| f.range.start <= offset && offset <= f.range.end)
            .cloned()
            .collect();
        if under.is_empty() {
            under = all
                .into_iter()
                .filter(|f| f.range.start <= line.end && line.start <= f.range.end)
                .collect();
        }
        under.sort_by_key(|f| f.range.len());
        under
    }

    /// Applies a quick fix as one undo step.
    #[cfg(feature = "schema")]
    pub fn apply_fix(&mut self, fix: &crate::assist::QuickFix) -> Result<(), EditorError> {
        let changes = fix
            .edits
            .iter()
            .map(|(r, t)| Change::replace(*r, t.clone()))
            .collect();
        self.history.close_group();
        let r = self.apply(Transaction::new(changes, EditKind::Replace), None);
        self.history.close_group();
        r
    }

    /// Whether [`Editor::format`] has a formatter for this document's language.
    pub fn can_format(&self) -> bool {
        crate::format::supported(self.language)
    }

    /// Formats the document as one undo step. `Ok(true)` when the text changed. An
    /// error says why nothing was done (a syntax error, no formatter for the
    /// language); the text is then untouched.
    pub fn format(&mut self) -> Result<bool, EditorError> {
        if self.mode != Mode::Edit {
            return Err(EditorError::ReadOnly);
        }
        let opts = crate::format::FormatOptions {
            indent: if self.options.insert_spaces {
                " ".repeat(self.options.tab_size.max(1))
            } else {
                "\t".into()
            },
        };
        let text = self.text();
        let formatted = crate::format::format(&text, self.language, &opts)
            .map_err(|e| EditorError::Format(e.to_string()))?;
        if formatted == text {
            return Ok(false);
        }
        // the caret stays on its line and column where the line still exists
        let at = self.doc.offset_to_position(self.selections.primary().head);
        self.set_text(&formatted)?;
        let line = at.line.min(self.doc.line_count().saturating_sub(1));
        let col = at.col.min(self.doc.line_len(line));
        let off = self.doc.position_to_offset(Position { line, col });
        self.selections = SelectionSet::single(Selection::caret(off));
        Ok(true)
    }

    /// Replaces the text whatever the mode and locks (the store changed under a
    /// read-only buffer). Not an undo step; history is cleared.
    pub fn force_text(&mut self, text: &str) {
        self.doc = Document::from_str(text);
        self.highlighter = Self::highlighter_for(self.language, &self.doc);
        self.highlighter.set_text(&self.doc);
        self.history = History::new();
        self.selections = SelectionSet::default();
        self.folded.clear();
        self.saved_text = text.to_string();
        self.revision = self.revision.wrapping_add(1);
        self.analysis_due = None;
        self.rowmap = None;
        if let Some((s, matches)) = &mut self.search {
            *matches = s.find_all(&self.doc, self.options.max_search_matches);
        }
        self.refresh_analysis();
    }

    // ----- commands --------------------------------------------------------------

    /// Resolves a key through the keymap and executes it. `Ok(None)` = unbound.
    pub fn key(&mut self, key: &Key) -> Result<Option<Outcome>, EditorError> {
        match self.keymap.resolve(key) {
            Some(cmd) => self.execute(cmd).map(Some),
            None => Ok(None),
        }
    }

    /// Text committed by the host's input method (typing, IME, paste).
    pub fn insert(&mut self, text: &str) -> Result<(), EditorError> {
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && self.options.auto_close
            && self.mode == Mode::Edit
            && let Some(done) = self.insert_paired(c)
        {
            return done;
        }
        let kind = if text.chars().count() > 1 {
            EditKind::Paste
        } else {
            EditKind::Typing
        };
        let changes = self
            .selections
            .iter()
            .map(|s| Change::replace(s.range(), text))
            .collect();
        self.apply(Transaction::new(changes, kind), None)
    }

    /// Bracket and quote pairs for one typed character: wrap the selections, step over
    /// a closer that is already there, or insert the pair. `None` = type it plainly.
    fn insert_paired(&mut self, c: char) -> Option<Result<(), EditorError>> {
        let closer = match c {
            '(' => Some(')'),
            '[' => Some(']'),
            '{' => Some('}'),
            '"' | '\'' | '`' => Some(c),
            _ => None,
        };
        let is_closer = matches!(c, ')' | ']' | '}' | '"' | '\'' | '`');
        let quote = matches!(c, '"' | '\'' | '`');
        if quote && c != '"' && !matches!(self.language, Language::Python | Language::Markdown) {
            // an apostrophe or a backtick is text in most of our formats
            if !(c == '\'' && matches!(self.language, Language::Yaml | Language::Toml)) {
                return None;
            }
        }
        let doc = &self.doc;
        let sels = self.selections.as_slice();
        // step over an existing closer
        if is_closer
            && sels
                .iter()
                .all(|s| s.is_caret() && doc.char_at(s.head) == Some(c))
        {
            // a quote only steps over when it closes something (an odd count before it)
            let closes = !quote
                || sels.iter().all(|s| {
                    let pos = doc.offset_to_position(s.head);
                    let before: String = doc.line(pos.line).chars().take(pos.col).collect();
                    before.chars().filter(|x| *x == c).count() % 2 == 1
                });
            if closes {
                self.selections = self.selections.map(|s| Selection::caret(s.head + 1));
                return Some(Ok(()));
            }
        }
        let closer = closer?;
        // wrap every non-empty selection
        if sels.iter().all(|s| !s.is_caret()) {
            let changes: Vec<Change> = sels
                .iter()
                .map(|s| {
                    let inner = doc.slice(s.range());
                    Change::replace(s.range(), format!("{c}{inner}{closer}"))
                })
                .collect();
            let tx = Transaction::new(changes, EditKind::Other);
            let after: Vec<Selection> = sels
                .iter()
                .map(|s| {
                    let a = tx.map_offset(s.start(), false) + 1;
                    Selection::new(a, a + s.range().len())
                })
                .collect();
            let primary = self.selections.primary_index();
            return Some(self.apply(tx, Some(SelectionSet::new(after, primary))));
        }
        // insert the pair where what follows is a closer, whitespace or the line end
        let fits = sels.iter().all(|s| {
            if !s.is_caret() {
                return false;
            }
            let next_ok = match doc.char_at(s.head) {
                None => true,
                Some(n) => n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';' | ':'),
            };
            let prev_ok = !quote
                || s.head == 0
                || doc
                    .char_at(s.head - 1)
                    .is_none_or(|p| !(p.is_alphanumeric() || p == '_' || p == c || p == '\\'));
            next_ok && prev_ok
        });
        if !fits {
            return None;
        }
        let changes: Vec<Change> = sels
            .iter()
            .map(|s| Change::insert(s.head, format!("{c}{closer}")))
            .collect();
        let tx = Transaction::new(changes, EditKind::Typing);
        let after: Vec<Selection> = sels
            .iter()
            .map(|s| Selection::caret(tx.map_offset(s.head, false) + 1))
            .collect();
        let primary = self.selections.primary_index();
        Some(self.apply(tx, Some(SelectionSet::new(after, primary))))
    }

    /// What Copy puts on the clipboard: the selections joined by newlines, or the
    /// whole line of each caret when nothing is selected.
    pub fn copy_text(&self) -> String {
        let doc = &self.doc;
        if self.selections.iter().all(|s| s.is_caret()) {
            let mut lines: Vec<usize> = self
                .selections
                .iter()
                .map(|s| doc.offset_to_position(s.head).line)
                .collect();
            lines.dedup();
            let mut out = String::new();
            for l in lines {
                out.push_str(&doc.line(l));
                out.push('\n');
            }
            return out;
        }
        self.selections
            .iter()
            .filter(|s| !s.is_caret())
            .map(|s| doc.slice(s.range()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Cut: returns [`Editor::copy_text`] and removes it (whole lines for carets).
    pub fn cut(&mut self) -> Result<String, EditorError> {
        let text = self.copy_text();
        if self.selections.iter().all(|s| s.is_caret()) {
            self.delete_lines()?;
        } else {
            self.delete_forward()?;
        }
        self.history.close_group();
        Ok(text)
    }

    /// Paste. Text copied from as many carets as there are now goes one line to each.
    pub fn paste(&mut self, text: &str) -> Result<(), EditorError> {
        let n = self.selections.len();
        let lines: Vec<&str> = text
            .strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .collect();
        self.history.close_group();
        let r = if n > 1 && lines.len() == n {
            let changes = self
                .selections
                .iter()
                .zip(&lines)
                .map(|(s, l)| Change::replace(s.range(), *l))
                .collect();
            self.apply(Transaction::new(changes, EditKind::Paste), None)
        } else {
            let changes = self
                .selections
                .iter()
                .map(|s| Change::replace(s.range(), text))
                .collect();
            self.apply(Transaction::new(changes, EditKind::Paste), None)
        };
        self.history.close_group();
        // the caret ends after what was pasted
        self.selections = self.selections.map(|s| Selection::caret(s.end()));
        r
    }

    pub fn execute(&mut self, cmd: Command) -> Result<Outcome, EditorError> {
        use Command::*;
        match cmd {
            MoveLeft(ext) => self.move_horizontal(-1, ext),
            MoveRight(ext) => self.move_horizontal(1, ext),
            MoveUp(ext) => self.move_vertical(-1, ext),
            MoveDown(ext) => self.move_vertical(1, ext),
            PageUp(ext) => self.move_vertical(-(self.options.page_rows as isize), ext),
            PageDown(ext) => self.move_vertical(self.options.page_rows as isize, ext),
            MoveWordLeft(ext) => self.map_heads(ext, |ed, h| ed.doc.prev_word_boundary(h)),
            MoveWordRight(ext) => self.map_heads(ext, |ed, h| ed.doc.next_word_boundary(h)),
            MoveLineStart(ext) => self.map_heads(ext, |ed, h| {
                let line = ed.doc.offset_to_position(h).line;
                let start = ed.doc.line_start(line);
                let indent = start
                    + ed.doc
                        .line(line)
                        .chars()
                        .take_while(|c| c.is_whitespace())
                        .count();
                if h == indent { start } else { indent }
            }),
            MoveLineEnd(ext) => self.map_heads(ext, |ed, h| {
                ed.doc.line_end(ed.doc.offset_to_position(h).line)
            }),
            MoveDocStart(ext) => self.map_heads(ext, |_, _| 0),
            MoveDocEnd(ext) => self.map_heads(ext, |ed, _| ed.doc.len_chars()),
            SelectAll => {
                self.selections = SelectionSet::single(Selection::new(0, self.doc.len_chars()));
                Ok(Outcome::None)
            }
            SelectLine => {
                let doc = &self.doc;
                self.selections = self.selections.map(|s| {
                    let a = doc.offset_to_position(s.start()).line;
                    let b = doc.offset_to_position(s.end()).line;
                    let end = if b + 1 < doc.line_count() {
                        doc.line_start(b + 1)
                    } else {
                        doc.len_chars()
                    };
                    Selection::new(doc.line_start(a), end)
                });
                Ok(Outcome::None)
            }
            SelectWord => {
                let doc = &self.doc;
                self.selections = self.selections.map(|s| {
                    let w = doc.word_at(s.head);
                    Selection::new(w.start, w.end)
                });
                Ok(Outcome::None)
            }
            SelectNextOccurrence => self.select_next_occurrence(),
            AddCursorAbove => self.add_cursor(-1),
            AddCursorBelow => self.add_cursor(1),
            CollapseSelections => {
                self.selections = if self.selections.len() > 1 {
                    self.selections.collapse_to_primary()
                } else {
                    SelectionSet::single(Selection::caret(self.selections.primary().head))
                };
                self.search = None;
                Ok(Outcome::None)
            }
            InsertText(t) => self.insert(&t).map(|_| Outcome::None),
            InsertNewline => self.insert_newline(),
            InsertTab => self.insert_tab(),
            Backspace => self.delete_backward(),
            Delete => self.delete_forward(),
            DeleteWordLeft => self.delete_to(|ed, h| ed.doc.prev_word_boundary(h)),
            DeleteWordRight => self.delete_to(|ed, h| ed.doc.next_word_boundary(h)),
            DeleteLine => self.delete_lines(),
            DuplicateLine => self.duplicate_lines(),
            MoveLineUp => self.move_lines(-1),
            MoveLineDown => self.move_lines(1),
            ToggleComment => self.toggle_comment(),
            Indent => self.indent_lines(true),
            Outdent => self.indent_lines(false),
            Undo => self.undo(),
            Redo => self.redo(),
            FindNext => self.find_step(true),
            FindPrevious => self.find_step(false),
            ToggleFold => {
                let line = self
                    .doc
                    .offset_to_position(self.selections.primary().head)
                    .line;
                self.toggle_fold(line);
                Ok(Outcome::None)
            }
            FoldAll => {
                self.folded = fold_ranges(&self.doc, self.language)
                    .into_iter()
                    .map(|f| (f.start_line, f.end_line))
                    .collect();
                self.unfold_at_cursors();
                Ok(Outcome::None)
            }
            UnfoldAll => {
                self.folded.clear();
                Ok(Outcome::None)
            }
            GoToMatchingBracket => {
                if let Some((_, m)) = brackets::matching(&self.doc, self.selections.primary().head)
                {
                    self.selections = self.selections.map(|s| s.moved(m.start, false));
                }
                Ok(Outcome::None)
            }
            GoToLine(n) => {
                let line = n
                    .saturating_sub(1)
                    .min(self.doc.line_count().saturating_sub(1));
                let off = self.doc.line_start(line);
                self.selections = SelectionSet::single(Selection::caret(off));
                self.unfold_at_cursors();
                Ok(Outcome::None)
            }
            Save => Ok(Outcome::Save),
            Apply => Ok(Outcome::Apply),
            Leave => Ok(Outcome::Leave),
            AcceptHunk(id) => self.set_hunk(&id, true).map(|_| Outcome::None),
            RejectHunk(id) => self.set_hunk(&id, false).map(|_| Outcome::None),
            Custom(id) => {
                if let Some(out) = self.registry.run(&id, &self.doc, self.kind) {
                    if let Some(t) = out.new_text {
                        self.set_text(&t)?;
                    }
                    Ok(Outcome::None)
                } else {
                    Ok(Outcome::Custom(id))
                }
            }
        }
    }

    // ----- pointer ----------------------------------------------------------------

    /// Pointer down at a document position. `extend` = shift, `add` = alt.
    pub fn click(&mut self, pos: Position, extend: bool, add: bool) {
        let off = self.doc.position_to_offset(pos);
        self.selections = if add {
            self.selections.with_added(Selection::caret(off))
        } else if extend {
            SelectionSet::single(self.selections.primary().moved(off, true))
        } else {
            SelectionSet::single(Selection::caret(off))
        };
        self.unfold_at_cursors();
    }

    /// Pointer drag: moves the primary head.
    pub fn drag(&mut self, pos: Position) {
        let off = self.doc.position_to_offset(pos);
        let p = self.selections.primary();
        let idx = self.selections.primary_index();
        let mut v = self.selections.as_slice().to_vec();
        v[idx] = Selection::new(p.anchor, off);
        self.selections = SelectionSet::new(v, idx);
    }

    pub fn double_click(&mut self, pos: Position) {
        let off = self.doc.position_to_offset(pos);
        let w = self.doc.word_at(off);
        self.selections = SelectionSet::single(Selection::new(w.start, w.end));
    }

    pub fn triple_click(&mut self, pos: Position) {
        let r = self.doc.line_range(pos.line);
        let end = if pos.line + 1 < self.doc.line_count() {
            self.doc.line_start(pos.line + 1)
        } else {
            r.end
        };
        self.selections = SelectionSet::single(Selection::new(r.start, end));
    }

    /// Column (block) selection between two positions: one selection per line, each
    /// from `anchor.col` to `head.col`, clamped to the line's length.
    pub fn column_select(&mut self, anchor: Position, head: Position) {
        let last = self.doc.line_count().saturating_sub(1);
        let (l0, l1) = (
            anchor.line.min(head.line),
            anchor.line.max(head.line).min(last),
        );
        let mut v = Vec::with_capacity(l1 - l0 + 1);
        for line in l0..=l1 {
            let start = self.doc.line_start(line);
            let len = self.doc.line_len(line);
            v.push(Selection::new(
                start + anchor.col.min(len),
                start + head.col.min(len),
            ));
        }
        let primary = if head.line >= anchor.line {
            v.len() - 1
        } else {
            0
        };
        self.selections = SelectionSet::new(v, primary);
    }

    /// Selects word by word while a double-click is dragged: from the word at
    /// `anchor` to the word at `pos`.
    pub fn drag_words(&mut self, anchor: Position, pos: Position) {
        let a = self.doc.word_at(self.doc.position_to_offset(anchor));
        let b = self.doc.word_at(self.doc.position_to_offset(pos));
        let sel = if b.start >= a.start {
            Selection::new(a.start, b.end.max(a.end))
        } else {
            Selection::new(a.end, b.start)
        };
        self.selections = SelectionSet::single(sel);
    }

    /// Selects line by line while a triple-click is dragged.
    pub fn drag_lines(&mut self, anchor_line: usize, line: usize) {
        let last = self.doc.line_count().saturating_sub(1);
        let (a, b) = (anchor_line.min(last), line.min(last));
        let end_of = |l: usize| {
            if l < last {
                self.doc.line_start(l + 1)
            } else {
                self.doc.line_end(l)
            }
        };
        let sel = if b >= a {
            Selection::new(self.doc.line_start(a), end_of(b))
        } else {
            Selection::new(end_of(a), self.doc.line_start(b))
        };
        self.selections = SelectionSet::single(sel);
    }

    // ----- search ----------------------------------------------------------------

    pub fn set_search(&mut self, query: &str, opts: &SearchOptions) -> Result<usize, EditorError> {
        if query.is_empty() {
            self.search = None;
            return Ok(0);
        }
        let s = Search::new(query, opts).map_err(|e| EditorError::Search(e.to_string()))?;
        let m = s.find_all(&self.doc, self.options.max_search_matches);
        let n = m.len();
        self.search = Some((s, m));
        Ok(n)
    }

    pub fn replace_all(
        &mut self,
        replacement: &str,
        regex_groups: bool,
    ) -> Result<usize, EditorError> {
        if self.analysis_due.is_some() {
            self.refresh_search();
        }
        let Some((s, m)) = &self.search else {
            return Ok(0);
        };
        let n = m.len();
        let tx = s.replace_all(&self.doc, replacement, regex_groups);
        self.history.close_group();
        self.apply(tx, None)?;
        self.history.close_group();
        self.refresh_search();
        Ok(n)
    }

    fn find_step(&mut self, forward: bool) -> Result<Outcome, EditorError> {
        if self.analysis_due.is_some() {
            self.refresh_search();
        }
        let Some((_, all)) = &self.search else {
            return Ok(Outcome::None);
        };
        let p = self.selections.primary();
        let next = if forward {
            let i = all.partition_point(|r| r.start < p.end());
            all.get(i).or(all.first()).copied()
        } else {
            let i = all.partition_point(|r| r.start < p.start());
            if i > 0 { all.get(i - 1) } else { all.last() }.copied()
        };
        if let Some(r) = next {
            self.selections = SelectionSet::single(Selection::new(r.start, r.end));
            self.unfold_at_cursors();
        }
        Ok(Outcome::None)
    }

    /// Index (0-based) of the match the primary selection is on, if any.
    pub fn search_index(&self) -> Option<usize> {
        let (_, all) = self.search.as_ref()?;
        let r = self.selections.primary().range();
        all.binary_search_by_key(&r.start, |m| m.start)
            .ok()
            .filter(|i| all[*i] == r)
    }

    /// Replaces the match under the primary selection and moves to the next one.
    pub fn replace_current(
        &mut self,
        replacement: &str,
        regex_groups: bool,
    ) -> Result<bool, EditorError> {
        if self.analysis_due.is_some() {
            self.refresh_search();
        }
        let Some(i) = self.search_index() else {
            self.find_step(true)?;
            return Ok(false);
        };
        let Some((s, all)) = &self.search else {
            return Ok(false);
        };
        let m = all[i];
        let text = s.expand(&self.doc, m, replacement, regex_groups);
        let end = m.start + text.chars().count();
        self.history.close_group();
        self.apply(
            Transaction::single(Change::replace(m, text), EditKind::Replace),
            Some(SelectionSet::single(Selection::caret(end))),
        )?;
        self.history.close_group();
        self.refresh_search();
        self.find_step(true)?;
        Ok(true)
    }

    // ----- folds -----------------------------------------------------------------

    /// The fold starting at `line`, if any (bounded scan; see `fold::MAX_FOLD_SCAN`).
    pub fn fold_at(&self, line: usize) -> Option<FoldRange> {
        fold_range_at(&self.doc, self.language, line)
    }

    pub fn toggle_fold(&mut self, line: usize) {
        if self.folded.remove(&line).is_some() {
            return;
        }
        // the innermost fold containing `line`: its own header first, then headers above
        let target = self.fold_at(line).or_else(|| {
            (line.saturating_sub(crate::fold::MAX_FOLD_SCAN)..line)
                .rev()
                .filter_map(|l| self.fold_at(l))
                .find(|f| f.end_line >= line)
        });
        if let Some(f) = target {
            self.folded.insert(f.start_line, f.end_line);
            let head = self.selections.primary().head;
            let hl = self.doc.offset_to_position(head).line;
            if hl > f.start_line && hl <= f.end_line {
                let off = self.doc.line_start(f.start_line);
                self.selections = SelectionSet::single(Selection::caret(off));
            }
        }
    }

    fn unfold_at_cursors(&mut self) {
        if self.folded.is_empty() {
            return;
        }
        let lines: Vec<usize> = self
            .selections
            .iter()
            .map(|s| self.doc.offset_to_position(s.head).line)
            .collect();
        self.folded
            .retain(|s, e| !lines.iter().any(|l| l > s && l <= e));
    }

    /// Folded ranges currently hiding lines, sorted and non-nested.
    fn active_folds(&self) -> Vec<FoldRange> {
        let mut out: Vec<FoldRange> = Vec::new();
        for (&s, &e) in &self.folded {
            if let Some(last) = out.last()
                && s <= last.end_line
            {
                continue;
            }
            out.push(FoldRange {
                start_line: s,
                end_line: e,
            });
        }
        out
    }

    // ----- rows: folding and soft wrap ---------------------------------------------

    /// Re-measures the rows of the lines an edit touched.
    fn rows_edit(&mut self, before: &Document, tx: &Transaction) {
        let Some((map, folds)) = &mut self.rowmap else {
            return;
        };
        if !folds.is_empty() || !self.folded.is_empty() || tx.changes.is_empty() {
            self.rowmap = None;
            return;
        }
        let first_off = tx.changes.iter().map(|c| c.range.start).min().unwrap_or(0);
        let last_off = tx.changes.iter().map(|c| c.range.end).max().unwrap_or(0);
        let first = before.offset_to_position(first_off).line;
        let old_last = before.offset_to_position(last_off).line;
        let grown = self.doc.line_count() as isize - before.line_count() as isize;
        let new_last = (old_last as isize + grown).max(first as isize) as usize;
        map.splice(&self.doc, first, old_last, new_last);
    }

    /// The row map, when soft wrap is on.
    fn rows(&mut self) -> Option<&crate::rows::RowMap> {
        let cols = self.options.wrap_cols?;
        let tab = self.options.tab_size;
        let stale = match &self.rowmap {
            Some((m, folds)) => m.cols != cols || m.tab != tab || *folds != self.folded,
            None => true,
        };
        if stale {
            let folds = self.active_folds();
            let hidden = |line: usize| {
                folds
                    .iter()
                    .any(|f| line > f.start_line && line <= f.end_line)
            };
            let map = crate::rows::RowMap::build(&self.doc, cols, tab, hidden);
            self.rowmap = Some((map, self.folded.clone()));
        }
        self.rowmap.as_ref().map(|(m, _)| m)
    }

    /// Turns soft wrap on (rows `cols` cells wide) or off.
    pub fn set_wrap(&mut self, cols: Option<usize>) {
        let cols = cols.map(|c| c.max(8));
        if self.options.wrap_cols != cols {
            self.options.wrap_cols = cols;
            self.rowmap = None;
        }
    }

    /// The text of a line, where its characters are, and the columns its rows start at.
    fn line_rows(&self, line: usize) -> (String, LineLayout, Vec<usize>) {
        let text = self.doc.line(line);
        let layout = LineLayout::new(&text, self.options.tab_size);
        let starts = match self.options.wrap_cols {
            Some(cols) => layout.wrap(&text, cols),
            None => vec![0],
        };
        (text, layout, starts)
    }

    /// Visible rows after folding and wrapping.
    pub fn row_count(&mut self) -> usize {
        if let Some(m) = self.rows() {
            return m.total();
        }
        let n = self.doc.line_count();
        let hidden: usize = self
            .active_folds()
            .iter()
            .map(|f| f.end_line - f.start_line)
            .sum();
        n - hidden
    }

    pub fn row_to_line(&mut self, row: usize) -> usize {
        if let Some(m) = self.rows() {
            return m.locate(row).0;
        }
        let folds = self.active_folds();
        let mut line = row;
        for f in folds {
            if f.start_line < line {
                line += f.end_line - f.start_line;
            } else {
                break;
            }
        }
        line.min(self.doc.line_count().saturating_sub(1))
    }

    /// The first row of a line.
    pub fn line_to_row(&mut self, line: usize) -> usize {
        if let Some(m) = self.rows() {
            return m.rows_before(line);
        }
        let folds = self.active_folds();
        let mut row = line;
        for f in folds {
            if f.end_line <= line {
                row -= f.end_line - f.start_line;
            } else if f.start_line < line {
                row -= line - f.start_line;
                break;
            }
        }
        row
    }

    pub fn cursor_row(&mut self) -> usize {
        let head = self.selections.primary().head;
        self.row_and_cell(head).0
    }

    /// The row a character is on and the cell it starts at within that row.
    pub fn row_and_cell(&mut self, offset: usize) -> (usize, u32) {
        let pos = self.doc.offset_to_position(offset);
        let (_, layout, starts) = self.line_rows(pos.line);
        let seg = starts.partition_point(|s| *s <= pos.col).max(1) - 1;
        let row = self.line_to_row(pos.line) + seg;
        (row, layout.x(pos.col) - layout.x(starts[seg]))
    }

    /// The position at cell `x` of a row. The column may lie past the end of the line
    /// (a block selection wants that); `Document::position_to_offset` clamps it.
    pub fn position_at(&mut self, row: usize, x: f32) -> Position {
        let (line, seg) = match self.rows() {
            Some(m) => m.locate(row),
            None => (self.row_to_line(row), 0),
        };
        let (_, layout, starts) = self.line_rows(line);
        let seg = seg.min(starts.len() - 1);
        let base = layout.x(starts[seg]) as f32;
        let mut col = layout.col_at(base + x).max(starts[seg]);
        if let Some(next) = starts.get(seg + 1) {
            // the end of a wrapped row is the start of the next: stay on this one
            col = col.min(layout.prev_boundary(*next));
        }
        Position::new(line, col)
    }

    /// The cluster boundary before / after an offset (an emoji sequence or a letter
    /// with its accents is one step).
    fn prev_grapheme(&self, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }
        if self.doc.char_at(offset - 1).is_some_and(|c| c.is_ascii()) {
            return offset - 1;
        }
        let pos = self.doc.offset_to_position(offset);
        if pos.col == 0 {
            return offset - 1;
        }
        let text = self.doc.line(pos.line);
        let layout = LineLayout::new(&text, self.options.tab_size);
        offset - pos.col + layout.prev_boundary(pos.col)
    }

    fn next_grapheme(&self, offset: usize) -> usize {
        let n = self.doc.len_chars();
        if offset >= n {
            return n;
        }
        let ascii = |o: usize| self.doc.char_at(o).is_none_or(|c| c.is_ascii());
        if ascii(offset) && ascii(offset + 1) {
            return offset + 1;
        }
        let pos = self.doc.offset_to_position(offset);
        let text = self.doc.line(pos.line);
        if pos.col >= text.chars().count() {
            return offset + 1;
        }
        let layout = LineLayout::new(&text, self.options.tab_size);
        offset - pos.col + layout.next_boundary(pos.col)
    }

    // ----- review (proposals) --------------------------------------------------------

    /// Puts a proposal under review. The document stays the base text.
    pub fn review(&mut self, proposed: &str) {
        let cs = ChangeSet::compute(&self.text(), proposed);
        self.review = Some((cs, BTreeSet::new()));
        self.mode = Mode::Review;
    }

    pub fn set_hunk(&mut self, id: &str, accepted: bool) -> Result<(), EditorError> {
        let Some((cs, acc)) = &mut self.review else {
            return Err(EditorError::NotInReview);
        };
        if cs.hunk(id).is_none() {
            return Err(EditorError::UnknownHunk(id.to_string()));
        }
        if accepted {
            acc.insert(id.to_string());
        } else {
            acc.remove(id);
        }
        Ok(())
    }

    pub fn accept_all_hunks(&mut self) -> Result<(), EditorError> {
        let Some((cs, acc)) = &mut self.review else {
            return Err(EditorError::NotInReview);
        };
        *acc = cs.hunks.iter().map(|h| h.id.clone()).collect();
        Ok(())
    }

    /// Applies the accepted hunks, leaves review mode and returns the approval event.
    pub fn finish_review(
        &mut self,
        approver: Author,
        note: Option<String>,
    ) -> Result<ApprovalEvent, EditorError> {
        let (cs, acc) = self.review.take().ok_or(EditorError::NotInReview)?;
        let accepted: Vec<&str> = acc.iter().map(String::as_str).collect();
        let tx = cs.transaction_for(&self.doc, &accepted);
        self.mode = Mode::Edit;
        if !tx.is_empty() {
            self.apply(tx, None)?;
        }
        let text = self.text();
        Ok(ApprovalEvent::from_changeset(
            &self.path,
            self.kind,
            &cs,
            &text,
            &accepted,
            approver,
            note,
            self.now_ms,
        ))
    }

    // ----- save --------------------------------------------------------------------

    /// Marks the current text as saved and returns the provenance event.
    pub fn save(&mut self, author: Author) -> EditEvent {
        let text = self.text();
        let cs = ChangeSet::compute(&self.saved_text, &text);
        let edit_count = self.history.len();
        let ev = EditEvent {
            path: self.path.clone(),
            kind: self.kind,
            before_hash: cs.base_hash.clone(),
            after_hash: cs.proposed_hash.clone(),
            hunks: cs.hunks,
            author,
            time_ms: self.now_ms,
            edit_count,
        };
        self.saved_text = text;
        self.history.mark_saved();
        ev
    }

    // ----- view ----------------------------------------------------------------------

    /// Rows `[viewport.first_row, +rows)` after folding, with everything a renderer
    /// needs. Highlighting and search matches are computed for this range only.
    pub fn view_state(&mut self, viewport: Viewport) -> ViewState {
        let total_rows = self.row_count();
        let total_lines = self.doc.line_count();
        let folds = self.active_folds();
        let first_row = viewport.first_row.min(total_rows.saturating_sub(1));
        let last_row = (first_row + viewport.rows).min(total_rows);

        // rows -> lines
        let mut lines: Vec<usize> = Vec::with_capacity(viewport.rows);
        // under soft wrap: rows of the first line that lie above the viewport
        let mut skip_rows = 0usize;
        let wrap = self.options.wrap_cols;
        if let Some(m) = self.rows() {
            let (mut line, seg) = m.locate(first_row);
            skip_rows = seg;
            let mut have = 0usize;
            while have < viewport.rows + seg && line < total_lines {
                let n = m.rows_of(line);
                if n > 0 {
                    lines.push(line);
                    have += n;
                }
                line += 1;
            }
        } else {
            let mut line = self.row_to_line(first_row);
            let mut row = first_row;
            while row < last_row && line < total_lines {
                lines.push(line);
                row += 1;
                line = match folds.iter().find(|f| f.start_line == line) {
                    Some(f) => f.end_line + 1,
                    None => line + 1,
                };
            }
        }
        let (Some(&first_line), Some(&last_line)) = (lines.first(), lines.last()) else {
            return ViewState {
                total_rows,
                total_lines,
                ..Default::default()
            };
        };
        let spans = self
            .highlighter
            .spans(&self.doc, Range::new(first_line, last_line + 1));
        let visible = Range::new(
            self.doc.line_start(first_line),
            self.doc.line_end(last_line),
        );
        let diags = self.diagnostics();
        let primary = self.selections.primary();
        let cur_line = self.doc.offset_to_position(primary.head).line;
        let bracket = brackets::matching(&self.doc, primary.head);
        let hunks: Vec<(Range, HunkKind)> = self
            .review
            .as_ref()
            .map(|(cs, _)| cs.hunks.iter().map(|h| (h.old_lines, h.kind)).collect())
            .unwrap_or_default();
        let search_in_view: Vec<Range> = self
            .search
            .as_ref()
            .map(|(_, m)| m.iter().filter(|r| r.overlaps(&visible)).copied().collect())
            .unwrap_or_default();
        let (mut errors, mut warnings) = (0, 0);
        for d in &diags {
            match d.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                _ => {}
            }
        }
        let mut rows: Vec<ViewLine> = Vec::with_capacity(lines.len());
        let mut max_cols = 0;
        for &line in &lines {
            let lr = self.doc.line_range(line);
            let text = self.doc.line(line);
            let len = text.chars().count();
            max_cols = max_cols.max(len);
            let rel = |o: usize| o.saturating_sub(lr.start).min(len);
            let mut highlights = Vec::new();
            let mut cursors = Vec::new();
            let mut primary_cursor = None;
            for (i, s) in self.selections.iter().enumerate() {
                let r = s.range();
                // selection spans the line break: include one extra column
                let line_full = Range::new(
                    lr.start,
                    if line + 1 < total_lines {
                        lr.end + 1
                    } else {
                        lr.end
                    },
                );
                if !s.is_caret()
                    && let Some(x) = r.intersect(&line_full)
                {
                    highlights.push(RowHighlight {
                        cols: Range::new(
                            rel(x.start),
                            if x.end > lr.end { len + 1 } else { rel(x.end) },
                        ),
                        class: "selection".into(),
                    });
                }
                if lr.contains(s.head) || s.head == lr.end {
                    cursors.push(rel(s.head));
                    if i == self.selections.primary_index() {
                        primary_cursor = Some(rel(s.head));
                    }
                }
            }
            for m in &search_in_view {
                if let Some(x) = m.intersect(&lr) {
                    highlights.push(RowHighlight {
                        cols: Range::new(rel(x.start), rel(x.end)),
                        class: "search".into(),
                    });
                }
            }
            if let Some((a, b)) = bracket {
                for r in [a, b] {
                    if lr.contains(r.start) {
                        highlights.push(RowHighlight {
                            cols: Range::new(rel(r.start), rel(r.end)),
                            class: "bracket".into(),
                        });
                    }
                }
            }
            let mut gutter = Vec::new();
            let mut widgets = Vec::new();
            let mut worst: Option<Severity> = None;
            for d in diags.iter().filter(|d| {
                d.range.overlaps(&Range::new(lr.start, lr.end + 1))
                    || d.range.start == lr.start && d.range.is_empty()
            }) {
                let x = d.range.intersect(&lr).unwrap_or(Range::new(
                    d.range.start.min(lr.end),
                    d.range.start.min(lr.end),
                ));
                highlights.push(RowHighlight {
                    cols: Range::new(
                        rel(x.start),
                        rel(x.end).max(rel(x.start) + 1).min(len.max(1)),
                    ),
                    class: format!("diagnostic_{}", severity_class(d.severity)),
                });
                if worst.is_none_or(|w| d.severity > w) {
                    worst = Some(d.severity);
                }
                if matches!(d.severity, Severity::Error | Severity::Warning) && widgets.is_empty() {
                    widgets.push(RowWidget {
                        col: len,
                        text: d.message.clone(),
                        class: format!("diagnostic_{}", severity_class(d.severity)),
                    });
                }
            }
            if let Some(w) = worst {
                gutter.push(GutterMark::Diagnostic { severity: w });
            }
            for (hr, kind) in &hunks {
                let touches =
                    (hr.start <= line && line < hr.end) || (hr.is_empty() && hr.start == line);
                if touches {
                    gutter.push(GutterMark::Hunk { kind: *kind });
                    highlights.push(RowHighlight {
                        cols: Range::new(0, len),
                        class: match kind {
                            HunkKind::Insert => "hunk_insert",
                            HunkKind::Delete => "hunk_delete",
                            HunkKind::Modify => "hunk_modify",
                        }
                        .into(),
                    });
                }
            }
            let locked = self
                .locks
                .iter()
                .any(|l| l.range.overlaps(&Range::new(lr.start, lr.end + 1)));
            if locked {
                gutter.push(GutterMark::Lock);
                if let Some(l) = self.locks.iter().find(|l| l.range.contains(lr.start)) {
                    widgets.push(RowWidget {
                        col: len,
                        text: l.reason.clone(),
                        class: "lock".into(),
                    });
                }
            }
            if !search_in_view.is_empty() && search_in_view.iter().any(|m| m.overlaps(&lr)) {
                gutter.push(GutterMark::SearchMatch);
            }
            for d in self
                .decorations
                .iter()
                .filter(|d| d.range.overlaps(&Range::new(lr.start, lr.end + 1)))
            {
                match &d.kind {
                    DecorationKind::Highlight { class } => {
                        if let Some(x) = d.range.intersect(&lr) {
                            highlights.push(RowHighlight {
                                cols: Range::new(rel(x.start), rel(x.end)),
                                class: class.clone(),
                            });
                        }
                    }
                    DecorationKind::InlineWidget { text, class } => widgets.push(RowWidget {
                        col: rel(d.range.end),
                        text: text.clone(),
                        class: class.clone(),
                    }),
                    DecorationKind::GutterMark { .. } | DecorationKind::ReadOnly { .. } => {}
                }
            }
            let fold = if self.folded.contains_key(&line) {
                FoldState::Folded
            } else if self.fold_at(line).is_some() {
                FoldState::Foldable
            } else {
                FoldState::None
            };
            let folded_lines = folds
                .iter()
                .find(|f| f.start_line == line)
                .map(|f| f.end_line - f.start_line)
                .unwrap_or(0);
            rows.push(ViewLine {
                line,
                spans: clip_spans(&spans, lr),
                text,
                highlights,
                cursors,
                primary_cursor,
                widgets,
                gutter,
                fold,
                folded_lines,
                locked,
                is_current: line == cur_line,
                col_start: 0,
                continuation: false,
            });
        }
        if let Some(cols) = wrap {
            let tab = self.options.tab_size;
            let mut wrapped = Vec::with_capacity(viewport.rows + 4);
            for row in rows {
                let starts = LineLayout::new(&row.text, tab).wrap(&row.text, cols);
                wrapped.extend(crate::view::split_row(row, &starts));
            }
            wrapped.drain(..skip_rows.min(wrapped.len()));
            wrapped.truncate(viewport.rows);
            rows = wrapped;
        }
        ViewState {
            rows,
            total_rows,
            total_lines,
            primary: self.doc.offset_to_position(primary.head),
            selection_count: self.selections.len(),
            dirty: self.is_dirty(),
            read_only: self.mode == Mode::ReadOnly,
            errors,
            warnings,
            search_matches: self.search.as_ref().map(|(_, m)| m.len()).unwrap_or(0),
            max_cols,
        }
    }

    // ----- movement helpers ---------------------------------------------------------

    fn map_heads(
        &mut self,
        extend: bool,
        f: impl Fn(&Editor, usize) -> usize,
    ) -> Result<Outcome, EditorError> {
        let new: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| s.moved(f(self, s.head), extend))
            .collect();
        self.selections = SelectionSet::new(new, self.selections.primary_index());
        self.unfold_at_cursors();
        Ok(Outcome::None)
    }

    fn move_horizontal(&mut self, dir: isize, extend: bool) -> Result<Outcome, EditorError> {
        let n = self.doc.len_chars();
        let new: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| {
                if !extend && !s.is_caret() {
                    return Selection::caret(if dir < 0 { s.start() } else { s.end() });
                }
                let mut h = s.head;
                if dir < 0 {
                    h = self.prev_grapheme(h);
                    if self.doc.char_at(h) == Some('\n')
                        && h > 0
                        && self.doc.char_at(h - 1) == Some('\r')
                    {
                        h -= 1;
                    }
                } else {
                    h = self.next_grapheme(h);
                    if self.doc.char_at(h) == Some('\n')
                        && h > 0
                        && self.doc.char_at(h - 1) == Some('\r')
                    {
                        h += 1;
                    }
                }
                s.moved(h.min(n), extend)
            })
            .collect();
        self.selections = SelectionSet::new(new, self.selections.primary_index());
        self.unfold_at_cursors();
        Ok(Outcome::None)
    }

    /// Up and down by visual rows (folds and soft wrap included), holding the cell the
    /// movement started at.
    fn move_vertical(&mut self, rows: isize, extend: bool) -> Result<Outcome, EditorError> {
        let total = self.row_count();
        let sels: Vec<Selection> = self.selections.as_slice().to_vec();
        let mut new = Vec::with_capacity(sels.len());
        for s in sels {
            let (row, x) = self.row_and_cell(s.head);
            let goal = s.goal_col.unwrap_or(x as usize);
            let target = (row as isize + rows).clamp(0, total as isize - 1) as usize;
            let off = if target == row {
                // already on the first or last row: to the very start or end
                if rows < 0 { 0 } else { self.doc.len_chars() }
            } else {
                let pos = self.position_at(target, goal as f32);
                self.doc.position_to_offset(pos)
            };
            let mut m = s.moved(off, extend);
            m.goal_col = Some(goal);
            new.push(m);
        }
        self.selections = SelectionSet::new(new, self.selections.primary_index());
        Ok(Outcome::None)
    }

    fn add_cursor(&mut self, dir: isize) -> Result<Outcome, EditorError> {
        let p = self.selections.primary();
        let pos = self.doc.offset_to_position(p.head);
        let goal = p.goal_col.unwrap_or(pos.col);
        // use the outermost cursor in that direction
        let line = self
            .selections
            .iter()
            .map(|s| self.doc.offset_to_position(s.head).line)
            .fold(
                pos.line,
                |acc, l| if dir < 0 { acc.min(l) } else { acc.max(l) },
            );
        let target = if dir < 0 {
            line.checked_sub(1)
        } else {
            (line + 1 < self.doc.line_count()).then_some(line + 1)
        };
        if let Some(t) = target {
            let off = self.doc.position_to_offset(Position::new(t, goal));
            let mut c = Selection::caret(off);
            c.goal_col = Some(goal);
            self.selections = self.selections.with_added(c);
        }
        Ok(Outcome::None)
    }

    fn select_next_occurrence(&mut self) -> Result<Outcome, EditorError> {
        let p = self.selections.primary();
        if p.is_caret() {
            return self.execute(Command::SelectWord);
        }
        let needle = self.doc.slice(p.range());
        let opts = SearchOptions {
            case_sensitive: true,
            ..Default::default()
        };
        let s = Search::new(&needle, &opts).map_err(|e| EditorError::Search(e.to_string()))?;
        let last_end = self.selections.iter().map(|s| s.end()).max().unwrap_or(0);
        let all = s.find_all(&self.doc, usize::MAX);
        let next = all.iter().find(|r| r.start >= last_end).or_else(|| {
            all.iter()
                .find(|r| !self.selections.iter().any(|s| s.range() == **r))
        });
        if let Some(r) = next {
            self.selections = self.selections.with_added(Selection::new(r.start, r.end));
        }
        Ok(Outcome::None)
    }

    // ----- editing helpers ----------------------------------------------------------

    fn indent_unit(&self) -> String {
        if self.options.insert_spaces {
            " ".repeat(self.options.tab_size)
        } else {
            "\t".into()
        }
    }

    fn insert_newline(&mut self) -> Result<Outcome, EditorError> {
        let doc = self.doc.clone();
        let unit = self.indent_unit();
        let lang = self.language;
        let changes: Vec<Change> = self
            .selections
            .iter()
            .map(|s| {
                let line_no = doc.offset_to_position(s.start()).line;
                let line = doc.line(line_no);
                let indent: String = line
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                let before: String = doc.slice(Range::new(doc.line_start(line_no), s.start()));
                let trimmed = before.trim_end();
                let opens = trimmed.ends_with(['{', '[', '('])
                    || (matches!(lang, Language::Yaml | Language::Python)
                        && trimmed.ends_with(':'))
                    || (lang == Language::Yaml
                        && trimmed.trim_start().starts_with("- ")
                        && trimmed.ends_with(':'));
                let mut text = String::from("\n");
                text.push_str(&indent);
                if opens {
                    text.push_str(&unit);
                }
                Change::replace(s.range(), text)
            })
            .collect();
        self.apply(Transaction::new(changes, EditKind::Typing), None)?;
        self.history.close_group();
        Ok(Outcome::None)
    }

    fn insert_tab(&mut self) -> Result<Outcome, EditorError> {
        if self.selections.iter().any(|s| {
            !s.is_caret()
                && self.doc.offset_to_position(s.start()).line
                    != self.doc.offset_to_position(s.end()).line
        }) {
            return self.indent_lines(true);
        }
        let doc = self.doc.clone();
        let (spaces, ts) = (self.options.insert_spaces, self.options.tab_size);
        let changes = self
            .selections
            .iter()
            .map(|s| {
                let col = doc.offset_to_position(s.start()).col;
                let text = if spaces {
                    " ".repeat(ts - col % ts)
                } else {
                    "\t".into()
                };
                Change::replace(s.range(), text)
            })
            .collect();
        self.apply(Transaction::new(changes, EditKind::Typing), None)
            .map(|_| Outcome::None)
    }

    fn delete_backward(&mut self) -> Result<Outcome, EditorError> {
        let doc = self.doc.clone();
        let ts = self.options.tab_size;
        let in_empty_pair = |s: &Selection| {
            s.is_caret()
                && s.head > 0
                && matches!(
                    (doc.char_at(s.head - 1), doc.char_at(s.head)),
                    (Some('('), Some(')'))
                        | (Some('['), Some(']'))
                        | (Some('{'), Some('}'))
                        | (Some('"'), Some('"'))
                )
        };
        if self.options.auto_close && self.selections.iter().all(in_empty_pair) {
            let changes: Vec<Change> = self
                .selections
                .iter()
                .map(|s| Change::delete(Range::new(s.head - 1, s.head + 1)))
                .collect();
            return self
                .apply(Transaction::new(changes, EditKind::Deletion), None)
                .map(|_| Outcome::None);
        }
        let changes: Vec<Change> = self
            .selections
            .iter()
            .filter_map(|s| {
                if !s.is_caret() {
                    return Some(Change::delete(s.range()));
                }
                if s.head == 0 {
                    return None;
                }
                let pos = doc.offset_to_position(s.head);
                let line = doc.line(pos.line);
                let before: String = line.chars().take(pos.col).collect();
                // dedent when only spaces precede the caret
                if !before.is_empty() && before.chars().all(|c| c == ' ') && ts > 1 {
                    let n = ((pos.col - 1) % ts) + 1;
                    return Some(Change::delete(Range::new(s.head - n, s.head)));
                }
                let mut start = self.prev_grapheme(s.head);
                if doc.char_at(start) == Some('\n')
                    && start > 0
                    && doc.char_at(start - 1) == Some('\r')
                {
                    start -= 1;
                }
                Some(Change::delete(Range::new(start, s.head)))
            })
            .collect();
        self.apply(Transaction::new(changes, EditKind::Deletion), None)
            .map(|_| Outcome::None)
    }

    fn delete_forward(&mut self) -> Result<Outcome, EditorError> {
        let n = self.doc.len_chars();
        let changes: Vec<Change> = self
            .selections
            .iter()
            .filter_map(|s| {
                if !s.is_caret() {
                    return Some(Change::delete(s.range()));
                }
                (s.head < n).then(|| Change::delete(Range::new(s.head, self.next_grapheme(s.head))))
            })
            .collect();
        self.apply(Transaction::new(changes, EditKind::Deletion), None)
            .map(|_| Outcome::None)
    }

    fn delete_to(&mut self, f: impl Fn(&Editor, usize) -> usize) -> Result<Outcome, EditorError> {
        let changes: Vec<Change> = self
            .selections
            .iter()
            .map(|s| {
                if !s.is_caret() {
                    return Change::delete(s.range());
                }
                let t = f(self, s.head);
                Change::delete(Range::new(t.min(s.head), t.max(s.head)))
            })
            .collect();
        self.apply(Transaction::new(changes, EditKind::Deletion), None)
            .map(|_| Outcome::None)
    }

    /// Line ranges covered by the selections, merged.
    fn selected_line_blocks(&self) -> Vec<(usize, usize)> {
        let mut blocks: Vec<(usize, usize)> = self
            .selections
            .iter()
            .map(|s| {
                let a = self.doc.offset_to_position(s.start()).line;
                let mut b = self.doc.offset_to_position(s.end()).line;
                if !s.is_caret() && b > a && self.doc.offset_to_position(s.end()).col == 0 {
                    b -= 1;
                }
                (a, b)
            })
            .collect();
        blocks.sort();
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (a, b) in blocks {
            if let Some(last) = out.last_mut()
                && a <= last.1 + 1
            {
                last.1 = last.1.max(b);
                continue;
            }
            out.push((a, b));
        }
        out
    }

    fn block_range(&self, a: usize, b: usize) -> Range {
        let start = self.doc.line_start(a);
        let end = if b + 1 < self.doc.line_count() {
            self.doc.line_start(b + 1)
        } else {
            self.doc.len_chars()
        };
        Range::new(start, end)
    }

    fn delete_lines(&mut self) -> Result<Outcome, EditorError> {
        let changes: Vec<Change> = self
            .selected_line_blocks()
            .into_iter()
            .map(|(a, b)| {
                let mut r = self.block_range(a, b);
                if r.end == self.doc.len_chars() && r.start > 0 {
                    r.start -= 1; // eat the previous newline when deleting the last line
                }
                Change::delete(r)
            })
            .collect();
        self.apply(Transaction::new(changes, EditKind::Other), None)
            .map(|_| Outcome::None)
    }

    fn duplicate_lines(&mut self) -> Result<Outcome, EditorError> {
        let changes: Vec<Change> = self
            .selected_line_blocks()
            .into_iter()
            .map(|(a, b)| {
                let r = self.block_range(a, b);
                let mut text = self.doc.slice(r);
                if !text.ends_with('\n') {
                    text.insert(0, '\n');
                }
                Change::insert(r.end, text)
            })
            .collect();
        self.apply(Transaction::new(changes, EditKind::Other), None)
            .map(|_| Outcome::None)
    }

    fn move_lines(&mut self, dir: isize) -> Result<Outcome, EditorError> {
        let blocks = self.selected_line_blocks();
        let n = self.doc.line_count();
        let mut changes = Vec::new();
        let mut sels = self.selections.as_slice().to_vec();
        for (a, b) in blocks {
            let (swap, block) = if dir < 0 {
                let Some(prev) = a.checked_sub(1) else {
                    continue;
                };
                (prev, (a, b))
            } else {
                if b + 1 >= n {
                    continue;
                }
                (b + 1, (a, b))
            };
            let block_r = self.block_range(block.0, block.1);
            let swap_r = self.block_range(swap, swap);
            let mut block_t = self.doc.slice(block_r);
            let mut swap_t = self.doc.slice(swap_r);
            if !block_t.ends_with('\n') {
                block_t.push('\n');
            }
            if !swap_t.ends_with('\n') {
                swap_t.push('\n');
            }
            let whole = Range::new(block_r.start.min(swap_r.start), block_r.end.max(swap_r.end));
            let mut new_text = if dir < 0 {
                format!("{block_t}{swap_t}")
            } else {
                format!("{swap_t}{block_t}")
            };
            if whole.end == self.doc.len_chars() && !self.doc.slice(whole).ends_with('\n') {
                new_text.pop();
            }
            let shift: isize = if dir < 0 {
                -(swap_t.chars().count() as isize)
            } else {
                swap_t.chars().count() as isize
            };
            for s in sels.iter_mut() {
                if block_r.contains(s.head) || s.head == block_r.end && dir > 0 {
                    s.head = (s.head as isize + shift) as usize;
                    s.anchor = (s.anchor as isize + shift) as usize;
                }
            }
            changes.push(Change::replace(whole, new_text));
        }
        if changes.is_empty() {
            return Ok(Outcome::None);
        }
        let after = SelectionSet::new(sels, self.selections.primary_index());
        self.apply(Transaction::new(changes, EditKind::Other), Some(after))
            .map(|_| Outcome::None)
    }

    fn toggle_comment(&mut self) -> Result<Outcome, EditorError> {
        let Some(token) = self.language.line_comment() else {
            return Ok(Outcome::None);
        };
        let blocks = self.selected_line_blocks();
        let lines: Vec<usize> = blocks.iter().flat_map(|(a, b)| *a..=*b).collect();
        let all_commented = lines.iter().all(|&l| {
            let t = self.doc.line(l);
            t.trim().is_empty() || t.trim_start().starts_with(token)
        });
        let min_indent = lines
            .iter()
            .filter(|&&l| !self.doc.line(l).trim().is_empty())
            .map(|&l| {
                self.doc
                    .line(l)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count()
            })
            .min()
            .unwrap_or(0);
        let mut changes = Vec::new();
        for l in lines {
            let text = self.doc.line(l);
            let start = self.doc.line_start(l);
            if all_commented {
                let ws = text.chars().take_while(|c| c.is_whitespace()).count();
                if text[ws..].starts_with(token) {
                    let extra = usize::from(text[ws + token.len()..].starts_with(' '));
                    changes.push(Change::delete(Range::new(
                        start + ws,
                        start + ws + token.len() + extra,
                    )));
                }
            } else if !text.trim().is_empty() {
                changes.push(Change::insert(start + min_indent, format!("{token} ")));
            }
        }
        self.apply(Transaction::new(changes, EditKind::Other), None)
            .map(|_| Outcome::None)
    }

    fn indent_lines(&mut self, indent: bool) -> Result<Outcome, EditorError> {
        let unit = self.indent_unit();
        let ts = self.options.tab_size;
        let mut changes = Vec::new();
        for (a, b) in self.selected_line_blocks() {
            for l in a..=b {
                let text = self.doc.line(l);
                let start = self.doc.line_start(l);
                if indent {
                    if !text.is_empty() {
                        changes.push(Change::insert(start, unit.clone()));
                    }
                } else {
                    let n = if text.starts_with('\t') {
                        1
                    } else {
                        text.chars().take_while(|c| *c == ' ').count().min(ts)
                    };
                    if n > 0 {
                        changes.push(Change::delete(Range::new(start, start + n)));
                    }
                }
            }
        }
        self.apply(Transaction::new(changes, EditKind::Format), None)
            .map(|_| Outcome::None)
    }

    fn undo(&mut self) -> Result<Outcome, EditorError> {
        if self.mode == Mode::ReadOnly {
            return Err(EditorError::ReadOnly);
        }
        if let Some(inv) = self.history.undo() {
            let before = self.doc.clone();
            inv.apply(&mut self.doc);
            self.highlighter.edit(&before, &self.doc, &inv);
            self.locks = map_locks(&self.locks, &inv);
            self.map_folds(&before, &inv);
            self.map_diagnostics(&inv);
            self.rows_edit(&before, &inv);
            self.selections = inv
                .selection_after
                .clone()
                .unwrap_or_else(|| inv.map_selections(&self.selections));
            self.after_edit();
        }
        Ok(Outcome::None)
    }

    fn redo(&mut self) -> Result<Outcome, EditorError> {
        if self.mode == Mode::ReadOnly {
            return Err(EditorError::ReadOnly);
        }
        if let Some(fwd) = self.history.redo() {
            let before = self.doc.clone();
            fwd.apply(&mut self.doc);
            self.highlighter.edit(&before, &self.doc, &fwd);
            self.locks = map_locks(&self.locks, &fwd);
            self.map_folds(&before, &fwd);
            self.map_diagnostics(&fwd);
            self.rows_edit(&before, &fwd);
            self.selections = fwd
                .selection_after
                .clone()
                .unwrap_or_else(|| fwd.map_selections(&self.selections));
            self.after_edit();
        }
        Ok(Outcome::None)
    }
}

/// The smallest single replacement that turns `doc` into `text`: the char range of
/// `doc` to replace and what to put there. `None` when they are equal.
pub fn diff_span(doc: &Document, text: &str) -> Option<(Range, String)> {
    let rope = doc.rope();
    let old_len = rope.len_chars();
    let mut prefix = 0usize;
    let mut prefix_bytes = 0usize;
    for (a, b) in rope.chars().zip(text.chars()) {
        if a != b {
            break;
        }
        prefix += 1;
        prefix_bytes += b.len_utf8();
    }
    let rest = &text[prefix_bytes..];
    let new_rest_len = rest.chars().count();
    if prefix == old_len && new_rest_len == 0 {
        return None;
    }
    let max_suffix = (old_len - prefix).min(new_rest_len);
    let mut suffix = 0usize;
    let mut suffix_bytes = 0usize;
    let mut old_rev = rope.chars_at(old_len);
    for b in rest.chars().rev() {
        if suffix == max_suffix {
            break;
        }
        match old_rev.prev() {
            Some(a) if a == b => {
                suffix += 1;
                suffix_bytes += b.len_utf8();
            }
            _ => break,
        }
    }
    let insert = &rest[..rest.len() - suffix_bytes];
    Some((Range::new(prefix, old_len - suffix), insert.to_string()))
}

fn severity_class(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Modifiers;

    fn ed(text: &str) -> Editor {
        Editor::open("x.yaml", text)
    }

    #[test]
    fn typing_undo_redo() {
        let mut e = ed("");
        e.tick(0);
        for c in "hello".chars() {
            e.key(&Key::ch(c, Modifiers::NONE)).expect("key");
        }
        assert_eq!(e.text(), "hello");
        e.tick(1000);
        e.insert(" world").expect("insert");
        assert_eq!(e.text(), "hello world");
        e.execute(Command::Undo).expect("undo");
        assert_eq!(e.text(), "hello");
        e.execute(Command::Undo).expect("undo");
        assert_eq!(e.text(), "");
        e.execute(Command::Redo).expect("redo");
        e.execute(Command::Redo).expect("redo");
        assert_eq!(e.text(), "hello world");
        assert_eq!(e.selections().primary().head, 11);
        assert!(e.is_dirty());
    }

    #[test]
    fn multi_cursor_insert_and_backspace() {
        let mut e = ed("a\nb\nc\n");
        e.execute(Command::AddCursorBelow).expect("c");
        e.execute(Command::AddCursorBelow).expect("c");
        assert_eq!(e.selections().len(), 3);
        e.insert("x").expect("i");
        assert_eq!(e.text(), "xa\nxb\nxc\n");
        e.execute(Command::Backspace).expect("b");
        assert_eq!(e.text(), "a\nb\nc\n");
        e.execute(Command::MoveLineEnd(false)).expect("m");
        e.execute(Command::InsertText("!".into())).expect("i");
        assert_eq!(e.text(), "a!\nb!\nc!\n");
    }

    #[test]
    fn select_next_occurrence_and_replace() {
        let mut e = ed("foo bar foo baz foo");
        e.execute(Command::SelectNextOccurrence).expect("w");
        assert_eq!(e.doc.slice(e.selections().primary().range()), "foo");
        e.execute(Command::SelectNextOccurrence).expect("n");
        e.execute(Command::SelectNextOccurrence).expect("n");
        assert_eq!(e.selections().len(), 3);
        e.insert("qux").expect("i");
        assert_eq!(e.text(), "qux bar qux baz qux");
    }

    #[test]
    fn vertical_movement_keeps_goal_column() {
        let mut e = ed("abcdef\nab\nabcdef\n");
        e.click(Position::new(0, 5), false, false);
        e.execute(Command::MoveDown(false)).expect("d");
        assert_eq!(
            e.doc.offset_to_position(e.selections().primary().head),
            Position::new(1, 2)
        );
        e.execute(Command::MoveDown(false)).expect("d");
        assert_eq!(
            e.doc.offset_to_position(e.selections().primary().head),
            Position::new(2, 5)
        );
    }

    #[test]
    fn locked_ranges_reject_edits() {
        let mut e = ed("a = 1\nb = 2\n");
        let lock = ReadOnlyRange::lines(e.document(), 1, 1, "set by policy");
        e.set_locks(vec![lock]);
        e.click(Position::new(1, 4), false, false);
        assert_eq!(
            e.insert("x"),
            Err(EditorError::Locked {
                reason: "set by policy".into()
            })
        );
        e.click(Position::new(0, 0), false, false);
        e.insert("# ").expect("edit above the lock");
        assert_eq!(e.locks()[0].range.start, e.document().line_start(1));
        let vs = e.view_state(Viewport {
            first_row: 0,
            rows: 5,
        });
        assert!(vs.rows[1].locked);
        assert_eq!(vs.rows[1].widgets[0].text, "set by policy");
    }

    #[test]
    fn newline_auto_indents() {
        let mut e = ed("data:\n  path: x");
        e.execute(Command::MoveDocEnd(false)).expect("m");
        e.execute(Command::InsertNewline).expect("n");
        assert_eq!(e.text(), "data:\n  path: x\n  ");
        e.execute(Command::MoveDocStart(false)).expect("m");
        e.execute(Command::MoveLineEnd(false)).expect("m");
        e.execute(Command::InsertNewline).expect("n");
        assert!(e.text().starts_with("data:\n  \n"));
    }

    #[test]
    fn line_commands() {
        let mut e = ed("one\ntwo\nthree");
        e.click(Position::new(1, 0), false, false);
        e.execute(Command::MoveLineDown).expect("m");
        assert_eq!(e.text(), "one\nthree\ntwo");
        assert_eq!(
            e.doc.offset_to_position(e.selections().primary().head).line,
            2
        );
        e.execute(Command::MoveLineUp).expect("m");
        assert_eq!(e.text(), "one\ntwo\nthree");
        e.execute(Command::DuplicateLine).expect("d");
        assert_eq!(e.text(), "one\ntwo\ntwo\nthree");
        e.execute(Command::DeleteLine).expect("d");
        assert_eq!(e.text(), "one\ntwo\nthree");
        e.execute(Command::ToggleComment).expect("c");
        assert_eq!(e.text(), "one\n# two\nthree");
        e.execute(Command::ToggleComment).expect("c");
        assert_eq!(e.text(), "one\ntwo\nthree");
        e.execute(Command::Indent).expect("i");
        assert_eq!(e.text(), "one\n  two\nthree");
        e.execute(Command::Outdent).expect("o");
        assert_eq!(e.text(), "one\ntwo\nthree");
    }

    #[test]
    fn folding_rows() {
        let mut e = ed("a:\n  b: 1\n  c: 2\nd: 3\n");
        assert_eq!(e.row_count(), 5);
        e.toggle_fold(0);
        assert_eq!(e.row_count(), 3);
        assert_eq!(e.row_to_line(1), 3);
        assert_eq!(e.line_to_row(3), 1);
        let vs = e.view_state(Viewport {
            first_row: 0,
            rows: 10,
        });
        assert_eq!(
            vs.rows.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![0, 3, 4]
        );
        assert_eq!(vs.rows[0].fold, FoldState::Folded);
        assert_eq!(vs.rows[0].folded_lines, 2);
        e.click(Position::new(2, 1), false, false);
        assert_eq!(e.row_count(), 5, "clicking into a fold opens it");
    }

    #[test]
    fn review_flow() {
        let mut e = ed("a: 1\nb: 2\nc: 3\n");
        e.review("a: 1\nb: 20\nc: 3\nd: 4\n");
        assert_eq!(e.mode(), Mode::Review);
        let ids: Vec<String> = e
            .changeset()
            .expect("cs")
            .hunks
            .iter()
            .map(|h| h.id.clone())
            .collect();
        assert_eq!(ids.len(), 2);
        let vs = e.view_state(Viewport {
            first_row: 0,
            rows: 10,
        });
        assert!(vs.rows[1].gutter.contains(&GutterMark::Hunk {
            kind: HunkKind::Modify
        }));
        e.execute(Command::AcceptHunk(ids[1].clone()))
            .expect("accept");
        assert_eq!(
            e.execute(Command::AcceptHunk("nope".into())),
            Err(EditorError::UnknownHunk("nope".into()))
        );
        let ev = e
            .finish_review(Author::User { id: "u".into() }, None)
            .expect("finish");
        assert_eq!(e.text(), "a: 1\nb: 2\nc: 3\nd: 4\n");
        assert_eq!(ev.accepted_hunks, vec![ids[1].clone()]);
        assert_eq!(ev.rejected_hunks, vec![ids[0].clone()]);
        assert_eq!(ev.result_hash, e.content_hash());
        assert_eq!(e.mode(), Mode::Edit);
    }

    #[test]
    fn save_event_has_hunks() {
        let mut e = ed("a: 1\n");
        e.tick(5);
        e.execute(Command::MoveDocEnd(false)).expect("m");
        e.insert("b: 2\n").expect("i");
        let ev = e.save(Author::Agent {
            id: "prep".into(),
            session: None,
        });
        assert_eq!(ev.hunks.len(), 1);
        assert_ne!(ev.before_hash, ev.after_hash);
        assert!(!e.is_dirty());
        assert_eq!(ev.time_ms, 5);
    }

    #[test]
    fn search_highlights_in_view() {
        let mut e = ed("x: foo\ny: foo\nz: bar\n");
        assert_eq!(
            e.set_search("foo", &SearchOptions::default()).expect("s"),
            2
        );
        e.execute(Command::FindNext).expect("f");
        assert_eq!(e.selections().primary().range(), Range::new(3, 6));
        e.execute(Command::FindNext).expect("f");
        assert_eq!(e.selections().primary().range(), Range::new(10, 13));
        let vs = e.view_state(Viewport {
            first_row: 0,
            rows: 3,
        });
        assert_eq!(vs.search_matches, 2);
        assert!(vs.rows[0].highlights.iter().any(|h| h.class == "search"));
        assert!(vs.rows[1].gutter.contains(&GutterMark::SearchMatch));
        assert_eq!(e.replace_all("qux", false).expect("r"), 2);
        assert_eq!(e.text(), "x: qux\ny: qux\nz: bar\n");
    }

    #[test]
    fn read_only_mode() {
        let mut e = Editor::open("job.log", "line\n");
        assert_eq!(e.mode(), Mode::ReadOnly);
        assert_eq!(e.insert("x"), Err(EditorError::ReadOnly));
        assert_eq!(e.execute(Command::Undo), Err(EditorError::ReadOnly));
        e.execute(Command::SelectAll).expect("selection is fine");
    }

    #[test]
    fn view_state_selection_and_cursor() {
        let mut e = ed("abc\ndef\n");
        e.click(Position::new(0, 1), false, false);
        e.drag(Position::new(1, 2));
        let vs = e.view_state(Viewport {
            first_row: 0,
            rows: 2,
        });
        assert_eq!(vs.rows[0].highlights[0].cols, Range::new(1, 4));
        assert_eq!(vs.rows[1].highlights[0].cols, Range::new(0, 2));
        assert_eq!(vs.rows[1].primary_cursor, Some(2));
        assert!(vs.rows[0].cursors.is_empty());
        assert_eq!(vs.primary, Position::new(1, 2));
    }

    #[cfg(feature = "schema")]
    #[test]
    fn schema_diagnostics_update_on_edit() {
        use crate::schema::SchemaRegistry;
        let mut reg = SchemaRegistry::new();
        reg.register(
            "t",
            r#"{"type":"object","properties":{"n":{"type":"integer"}}}"#,
            &[FileKind::Yaml],
            &[],
        )
        .expect("schema");
        let mut e = ed("n: 1\n");
        e.use_registry(&reg);
        assert!(e.diagnostics().is_empty());
        e.click(Position::new(0, 4), false, false);
        e.insert("x").expect("i");
        let d = e.diagnostics();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].range, Range::new(3, 5));
        let vs = e.view_state(Viewport {
            first_row: 0,
            rows: 1,
        });
        assert_eq!(vs.errors, 1);
        assert_eq!(
            vs.rows[0].gutter,
            vec![GutterMark::Diagnostic {
                severity: Severity::Error
            }]
        );
    }
}
