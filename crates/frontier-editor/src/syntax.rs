// SPDX-License-Identifier: Apache-2.0
//! Syntax highlighting.
//!
//! Natively each enabled grammar runs through tree-sitter with incremental re-parse
//! and queries restricted to the visible byte range. On `wasm32` and for languages
//! without a permissive grammar (Tcl, logs) a line lexer produces the same [`Span`]s,
//! so views never special-case the platform.
//!
//! Why the browser gets the lexer: the published grammar crates compile their C with
//! the host's headers, which `wasm32-unknown-unknown` does not have (only the
//! tree-sitter runtime itself brings a libc subset for that target), and Apple's clang
//! has no WebAssembly backend. Compiled out of tree with LLVM's clang the runtime plus
//! the Rust and Bash grammars are about 2.65 MB of wasm (0.38 MB gzipped), mostly parse
//! tables.

use crate::document::{Document, Range};
use crate::transaction::Transaction;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Plain,
    Log,
    Json,
    Yaml,
    Toml,
    Python,
    Markdown,
    Rust,
    Java,
    Matlab,
    Tcl,
    Shell,
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "json" | "json5" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "py" | "pyi" => Self::Python,
            "md" | "markdown" => Self::Markdown,
            "rs" => Self::Rust,
            "java" => Self::Java,
            "m" => Self::Matlab,
            "tcl" | "jou" => Self::Tcl,
            "sh" | "bash" | "zsh" | "ksh" => Self::Shell,
            "log" | "out" | "err" => Self::Log,
            _ => Self::Plain,
        }
    }

    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Yaml | Self::Toml | Self::Python | Self::Tcl | Self::Shell => Some("#"),
            Self::Rust | Self::Java => Some("//"),
            Self::Matlab => Some("%"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Keyword,
    String,
    Number,
    Comment,
    /// Object key / mapping key / table header.
    Property,
    Type,
    Function,
    Variable,
    Operator,
    Punctuation,
    /// `true`, `false`, `null`, builtin constants.
    Constant,
    Escape,
    Heading,
    Emphasis,
    Link,
    /// Log levels: ERROR / FATAL.
    Error,
    /// Log levels: WARN.
    Warning,
    Text,
}

/// A highlighted char range (absolute document offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Span {
    pub range: Range,
    pub kind: TokenKind,
}

/// Owns parser state for one document.
pub struct Highlighter {
    language: Language,
    #[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
    ts: Option<ts::State>,
}

impl fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Highlighter")
            .field("language", &self.language)
            .field("tree_sitter", &self.is_tree_sitter())
            .finish()
    }
}

impl Highlighter {
    pub fn new(language: Language) -> Self {
        Self {
            language,
            #[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
            ts: ts::State::new(language),
        }
    }

    /// A highlighter that always uses the line lexer (large files, plugin languages).
    pub fn new_lexer(language: Language) -> Self {
        Self {
            language,
            #[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
            ts: None,
        }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// True when a tree-sitter grammar backs this highlighter.
    pub fn is_tree_sitter(&self) -> bool {
        #[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
        {
            self.ts.is_some()
        }
        #[cfg(not(all(feature = "tree-sitter", not(target_arch = "wasm32"))))]
        {
            false
        }
    }

    /// Full parse. Call after loading a document.
    pub fn set_text(&mut self, doc: &Document) {
        #[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
        if let Some(ts) = &mut self.ts {
            ts.reparse(doc);
        }
        #[cfg(not(all(feature = "tree-sitter", not(target_arch = "wasm32"))))]
        let _ = doc;
    }

    /// Incremental update after `tx` turned `before` into `after`.
    pub fn edit(&mut self, before: &Document, after: &Document, tx: &Transaction) {
        #[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
        if let Some(ts) = &mut self.ts {
            ts.edit(before, after, tx);
        }
        #[cfg(not(all(feature = "tree-sitter", not(target_arch = "wasm32"))))]
        let _ = (before, after, tx);
    }

    /// Spans intersecting the line range `[lines.start, lines.end)`, sorted and
    /// non-overlapping. Only the requested range is examined.
    pub fn spans(&mut self, doc: &Document, lines: Range) -> Vec<Span> {
        let lines = Range::new(
            lines.start.min(doc.line_count()),
            lines.end.min(doc.line_count()),
        );
        if lines.is_empty() {
            return Vec::new();
        }
        #[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
        if let Some(ts) = &mut self.ts {
            return ts.spans(doc, lines);
        }
        fallback::spans(self.language, doc, lines)
    }
}

/// Maps a highlight capture name to a token kind.
pub fn kind_for_capture(name: &str) -> Option<TokenKind> {
    if name.ends_with(".key") || name == "property" {
        return Some(TokenKind::Property);
    }
    let head = name.split('.').next().unwrap_or(name);
    Some(match head {
        "keyword" | "conditional" | "repeat" | "include" | "storageclass" => TokenKind::Keyword,
        "string" | "character" => TokenKind::String,
        "number" | "float" => TokenKind::Number,
        "comment" => TokenKind::Comment,
        "property" | "field" | "label" | "attribute" | "tag" => TokenKind::Property,
        "type" | "constructor" => TokenKind::Type,
        "function" | "method" => TokenKind::Function,
        "variable" | "parameter" => TokenKind::Variable,
        "operator" => TokenKind::Operator,
        "punctuation" => TokenKind::Punctuation,
        "constant" | "boolean" | "null" => TokenKind::Constant,
        "escape" | "string_escape" => TokenKind::Escape,
        "text" | "markup" => match name {
            n if n.contains("title") || n.contains("heading") => TokenKind::Heading,
            n if n.contains("emphasis") || n.contains("strong") => TokenKind::Emphasis,
            n if n.contains("uri") || n.contains("link") => TokenKind::Link,
            n if n.contains("literal") || n.contains("raw") => TokenKind::String,
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(all(feature = "tree-sitter", not(target_arch = "wasm32")))]
mod ts {
    use super::*;
    use tree_sitter::{InputEdit, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

    pub struct State {
        parser: Parser,
        query: Query,
        capture_kinds: Vec<Option<TokenKind>>,
        tree: Option<Tree>,
    }

    fn grammar(language: Language) -> Option<(tree_sitter::Language, &'static str)> {
        match language {
            #[cfg(feature = "lang-json")]
            Language::Json => Some((
                tree_sitter_json::LANGUAGE.into(),
                tree_sitter_json::HIGHLIGHTS_QUERY,
            )),
            #[cfg(feature = "lang-yaml")]
            Language::Yaml => Some((
                tree_sitter_yaml::LANGUAGE.into(),
                tree_sitter_yaml::HIGHLIGHTS_QUERY,
            )),
            #[cfg(feature = "lang-toml")]
            Language::Toml => Some((
                tree_sitter_toml_ng::LANGUAGE.into(),
                tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            )),
            #[cfg(feature = "lang-python")]
            Language::Python => Some((
                tree_sitter_python::LANGUAGE.into(),
                tree_sitter_python::HIGHLIGHTS_QUERY,
            )),
            #[cfg(feature = "lang-markdown")]
            Language::Markdown => Some((
                tree_sitter_md::LANGUAGE.into(),
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            )),
            #[cfg(feature = "lang-rust")]
            Language::Rust => Some((
                tree_sitter_rust::LANGUAGE.into(),
                tree_sitter_rust::HIGHLIGHTS_QUERY,
            )),
            #[cfg(feature = "lang-shell")]
            Language::Shell => Some((
                tree_sitter_bash::LANGUAGE.into(),
                tree_sitter_bash::HIGHLIGHT_QUERY,
            )),
            #[cfg(feature = "lang-java")]
            Language::Java => Some((
                tree_sitter_java::LANGUAGE.into(),
                tree_sitter_java::HIGHLIGHTS_QUERY,
            )),
            #[cfg(feature = "lang-matlab")]
            Language::Matlab => Some((
                tree_sitter_matlab::LANGUAGE.into(),
                tree_sitter_matlab::HIGHLIGHTS_QUERY,
            )),
            _ => None,
        }
    }

    impl State {
        pub fn new(language: Language) -> Option<Self> {
            let (lang, query_src) = grammar(language)?;
            let mut parser = Parser::new();
            parser.set_language(&lang).ok()?;
            let query = match Query::new(&lang, query_src) {
                Ok(q) => q,
                Err(e) => {
                    tracing::warn!(?language, error = %e, "highlight query failed to compile");
                    return None;
                }
            };
            let capture_kinds = query
                .capture_names()
                .iter()
                .map(|n| kind_for_capture(n))
                .collect();
            Some(Self {
                parser,
                query,
                capture_kinds,
                tree: None,
            })
        }

        fn parse(&mut self, doc: &Document, old: Option<&Tree>) -> Option<Tree> {
            let rope = doc.rope();
            self.parser.parse_with_options(
                &mut |byte, _pos| {
                    if byte >= rope.len_bytes() {
                        return &[] as &[u8];
                    }
                    let (chunk, start, _, _) = rope.chunk_at_byte(byte);
                    &chunk.as_bytes()[byte - start..]
                },
                old,
                None,
            )
        }

        pub fn reparse(&mut self, doc: &Document) {
            self.tree = self.parse(doc, None);
        }

        pub fn edit(&mut self, before: &Document, after: &Document, tx: &Transaction) {
            let Some(tree) = &mut self.tree else {
                self.reparse(after);
                return;
            };
            for c in tx.changes.iter().rev() {
                let start_byte = before.char_to_byte(c.range.start);
                let old_end_byte = before.char_to_byte(c.range.end);
                let new_end_byte = start_byte + c.text.len();
                let (sr, sc) = before.byte_point(start_byte);
                let (oer, oec) = before.byte_point(old_end_byte);
                let (ner, nec) = match c.text.rfind('\n') {
                    Some(i) => (sr + c.text.matches('\n').count(), c.text.len() - i - 1),
                    None => (sr, sc + c.text.len()),
                };
                tree.edit(&InputEdit {
                    start_byte,
                    old_end_byte,
                    new_end_byte,
                    start_position: Point::new(sr, sc),
                    old_end_position: Point::new(oer, oec),
                    new_end_position: Point::new(ner, nec),
                });
            }
            let old = self.tree.take();
            self.tree = self.parse(after, old.as_ref());
        }

        pub fn spans(&mut self, doc: &Document, lines: Range) -> Vec<Span> {
            let Some(tree) = &self.tree else {
                return Vec::new();
            };
            let start_byte = doc.char_to_byte(doc.line_start(lines.start));
            let end_byte = if lines.end >= doc.line_count() {
                doc.len_bytes()
            } else {
                doc.char_to_byte(doc.line_start(lines.end))
            };
            let mut cursor = QueryCursor::new();
            cursor.set_byte_range(start_byte..end_byte);
            let rope = doc.rope();
            let provider = RopeText(rope);
            let mut out: Vec<Span> = Vec::new();
            let mut matches = cursor.matches(&self.query, tree.root_node(), provider);
            while let Some(m) = matches.next() {
                for cap in m.captures() {
                    let Some(kind) = self.capture_kinds[cap.index as usize] else {
                        continue;
                    };
                    let node = cap.node;
                    let r = Range::new(
                        doc.byte_to_char(node.start_byte()),
                        doc.byte_to_char(node.end_byte()),
                    );
                    if r.is_empty() {
                        continue;
                    }
                    // Earlier (more specific) patterns win on overlap, like tree-sitter-highlight.
                    if let Some(last) = out.last()
                        && r.start < last.range.end
                    {
                        continue;
                    }
                    out.push(Span { range: r, kind });
                }
            }
            out.sort_by_key(|s| s.range.start);
            out.dedup_by(|b, a| b.range.start < a.range.end);
            out
        }
    }

    struct RopeText<'a>(&'a ropey::Rope);

    impl<'a> tree_sitter::TextProvider<&'a [u8]> for RopeText<'a> {
        type I = std::iter::Map<ropey::iter::Chunks<'a>, fn(&'a str) -> &'a [u8]>;
        fn text(&mut self, node: tree_sitter::Node) -> Self::I {
            self.0
                .byte_slice(node.start_byte()..node.end_byte())
                .chunks()
                .map(str::as_bytes)
        }
    }
}

/// Lexer-based highlighting: one line at a time, no cross-line state.
pub mod fallback {
    use super::*;

    const PY_KEYWORDS: &[&str] = &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
        "yield",
    ];
    const RUST_KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "type",
        "unsafe", "use", "where", "while",
    ];
    const SHELL_KEYWORDS: &[&str] = &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
        "in", "function", "select", "time", "return", "exit", "break", "continue", "local",
        "export", "readonly", "declare", "unset", "shift", "source", "set", "trap", "eval", "exec",
        "echo", "printf", "read", "cd", "test",
    ];
    const JAVA_KEYWORDS: &[&str] = &[
        "abstract",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "float",
        "for",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "new",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "switch",
        "this",
        "throw",
        "throws",
        "try",
        "void",
        "while",
    ];
    const MATLAB_KEYWORDS: &[&str] = &[
        "break",
        "case",
        "catch",
        "classdef",
        "continue",
        "else",
        "elseif",
        "end",
        "for",
        "function",
        "global",
        "if",
        "otherwise",
        "parfor",
        "persistent",
        "return",
        "switch",
        "try",
        "while",
    ];
    const TCL_KEYWORDS: &[&str] = &[
        "proc", "set", "if", "else", "elseif", "foreach", "for", "while", "return", "puts", "expr",
        "lappend", "list", "string", "switch", "break", "continue", "source", "global", "upvar",
    ];

    pub fn spans(language: Language, doc: &Document, lines: Range) -> Vec<Span> {
        let mut out = Vec::new();
        for line in lines.start..lines.end {
            let text = doc.line(line);
            let base = doc.line_start(line);
            lex_line(language, &text, base, &mut out);
        }
        out
    }

    fn keywords(language: Language) -> &'static [&'static str] {
        match language {
            Language::Python => PY_KEYWORDS,
            Language::Rust => RUST_KEYWORDS,
            Language::Java => JAVA_KEYWORDS,
            Language::Matlab => MATLAB_KEYWORDS,
            Language::Tcl => TCL_KEYWORDS,
            Language::Shell => SHELL_KEYWORDS,
            _ => &[],
        }
    }

    pub fn lex_line(language: Language, text: &str, base: usize, out: &mut Vec<Span>) {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let push = |out: &mut Vec<Span>, s: usize, e: usize, k: TokenKind| {
            if e > s {
                out.push(Span {
                    range: Range::new(base + s, base + e),
                    kind: k,
                });
            }
        };
        match language {
            Language::Plain => return,
            Language::Log => {
                lex_log(&chars, base, out);
                return;
            }
            Language::Markdown => {
                lex_markdown(&chars, base, out);
                return;
            }
            _ => {}
        }
        let comment = language.line_comment();
        let kws = keywords(language);
        let mut i = 0;
        while i < n {
            let c = chars[i];
            if let Some(cm) = comment
                && chars[i..].iter().take(cm.len()).collect::<String>() == cm
                // in a shell script `#` opens a comment only where a word could start
                && (language != Language::Shell
                    || i == 0
                    || chars[i - 1].is_whitespace()
                    || chars[i - 1] == ';')
            {
                push(out, i, n, TokenKind::Comment);
                return;
            }
            if language == Language::Rust {
                // a lifetime (`'a`) is not the start of a character literal
                if c == '\''
                    && i + 1 < n
                    && (chars[i + 1].is_alphabetic() || chars[i + 1] == '_')
                    && chars.get(i + 2) != Some(&'\'')
                {
                    let s = i;
                    i += 1;
                    while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    push(out, s, i, TokenKind::Variable);
                    continue;
                }
                // an attribute
                if c == '#' && matches!(chars.get(i + 1), Some('[') | Some('!')) {
                    let s = i;
                    while i < n && chars[i] != ']' {
                        i += 1;
                    }
                    i = (i + 1).min(n);
                    push(out, s, i, TokenKind::Keyword);
                    continue;
                }
            }
            if language == Language::Shell && c == '$' && i + 1 < n {
                // `$name`, `${name}`, `$1`, `$@`
                let s = i;
                i += 1;
                if chars[i] == '{' {
                    while i < n && chars[i] != '}' {
                        i += 1;
                    }
                    i = (i + 1).min(n);
                } else if chars[i].is_alphanumeric() || chars[i] == '_' {
                    while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                } else if matches!(chars[i], '@' | '*' | '#' | '?' | '!' | '$') {
                    i += 1;
                } else {
                    continue;
                }
                push(out, s, i, TokenKind::Variable);
                continue;
            }
            if c == '"' || c == '\'' {
                let q = c;
                let s = i;
                i += 1;
                while i < n && chars[i] != q {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(n);
                // JSON / TOML key or YAML quoted key?
                let mut j = i;
                while j < n && chars[j] == ' ' {
                    j += 1;
                }
                let is_key = j < n
                    && ((chars[j] == ':' && matches!(language, Language::Json | Language::Yaml))
                        || (chars[j] == '=' && language == Language::Toml));
                push(
                    out,
                    s,
                    i,
                    if is_key {
                        TokenKind::Property
                    } else {
                        TokenKind::String
                    },
                );
                continue;
            }
            if c.is_ascii_digit() || (c == '-' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
                let s = i;
                i += 1;
                while i < n
                    && (chars[i].is_ascii_alphanumeric()
                        || chars[i] == '.'
                        || chars[i] == '_'
                        || chars[i] == '-'
                        || chars[i] == '+')
                {
                    i += 1;
                }
                push(out, s, i, TokenKind::Number);
                continue;
            }
            if c.is_alphabetic() || c == '_' {
                let s = i;
                while i < n
                    && (chars[i].is_alphanumeric()
                        || chars[i] == '_'
                        || chars[i] == '-'
                        || chars[i] == '.')
                {
                    i += 1;
                }
                let word: String = chars[s..i].iter().collect();
                let mut j = i;
                while j < n && chars[j] == ' ' {
                    j += 1;
                }
                let kind = if matches!(
                    word.as_str(),
                    "true" | "false" | "null" | "None" | "True" | "False" | "~"
                ) {
                    TokenKind::Constant
                } else if kws.contains(&word.as_str()) {
                    TokenKind::Keyword
                } else if j < n
                    && ((chars[j] == ':'
                        && matches!(language, Language::Yaml)
                        && (j + 1 >= n || chars[j + 1] == ' '))
                        || (chars[j] == '=' && language == Language::Toml))
                {
                    TokenKind::Property
                } else if j < n
                    && chars[j] == '('
                    && matches!(
                        language,
                        Language::Python | Language::Rust | Language::Java | Language::Matlab
                    )
                {
                    TokenKind::Function
                } else if language == Language::Rust && j < n && chars[j] == '!' && j == i {
                    // a macro
                    TokenKind::Function
                } else if language == Language::Rust
                    && word.chars().next().is_some_and(char::is_uppercase)
                {
                    TokenKind::Type
                } else {
                    i = i.max(s + 1);
                    continue;
                };
                push(out, s, i, kind);
                continue;
            }
            if language == Language::Toml && c == '[' && i == 0 {
                push(out, 0, n, TokenKind::Property);
                return;
            }
            if matches!(c, '{' | '}' | '[' | ']' | ',' | ':' | '=') {
                push(out, i, i + 1, TokenKind::Punctuation);
            }
            i += 1;
        }
    }

    fn lex_log(chars: &[char], base: usize, out: &mut Vec<Span>) {
        let text: String = chars.iter().collect();
        let upper = text.to_ascii_uppercase();
        for (needle, kind) in [
            ("ERROR", TokenKind::Error),
            ("FATAL", TokenKind::Error),
            ("TRACEBACK", TokenKind::Error),
            ("WARN", TokenKind::Warning),
        ] {
            if let Some(pos) = upper.find(needle) {
                let s = upper[..pos].chars().count();
                out.push(Span {
                    range: Range::new(base + s, base + s + needle.len()),
                    kind,
                });
            }
        }
        // leading timestamp / bracketed prefix
        if chars.first() == Some(&'[') {
            if let Some(e) = chars.iter().position(|&c| c == ']') {
                out.push(Span {
                    range: Range::new(base, base + e + 1),
                    kind: TokenKind::Comment,
                });
            }
        } else if chars.len() > 10
            && chars[..10]
                .iter()
                .all(|c| c.is_ascii_digit() || *c == '-' || *c == ':')
        {
            let e = chars.iter().position(|&c| c == ' ').unwrap_or(chars.len());
            out.push(Span {
                range: Range::new(base, base + e),
                kind: TokenKind::Comment,
            });
        }
        out.sort_by_key(|s| s.range.start);
        out.dedup_by(|b, a| b.range.start < a.range.end);
    }

    fn lex_markdown(chars: &[char], base: usize, out: &mut Vec<Span>) {
        let n = chars.len();
        if chars.first() == Some(&'#') {
            out.push(Span {
                range: Range::new(base, base + n),
                kind: TokenKind::Heading,
            });
            return;
        }
        let text: String = chars.iter().collect();
        if text.trim_start().starts_with("```") {
            out.push(Span {
                range: Range::new(base, base + n),
                kind: TokenKind::Punctuation,
            });
            return;
        }
        let mut i = 0;
        while i < n {
            let c = chars[i];
            if c == '`' {
                let s = i;
                i += 1;
                while i < n && chars[i] != '`' {
                    i += 1;
                }
                i = (i + 1).min(n);
                out.push(Span {
                    range: Range::new(base + s, base + i),
                    kind: TokenKind::String,
                });
                continue;
            }
            if c == '*' || c == '_' {
                let s = i;
                let q = c;
                i += 1;
                while i < n && chars[i] == q {
                    i += 1;
                }
                let close = chars[i..].iter().position(|&x| x == q);
                if let Some(off) = close {
                    i += off;
                    while i < n && chars[i] == q {
                        i += 1;
                    }
                    out.push(Span {
                        range: Range::new(base + s, base + i),
                        kind: TokenKind::Emphasis,
                    });
                    continue;
                }
            }
            if c == '['
                && let Some(off) = chars[i..].iter().position(|&x| x == ')')
            {
                out.push(Span {
                    range: Range::new(base + i, base + i + off + 1),
                    kind: TokenKind::Link,
                });
                i += off + 1;
                continue;
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_yaml() {
        let doc = Document::from_str("name: ahmed # note\ncount: 42\nflag: true\n");
        let spans = fallback::spans(Language::Yaml, &doc, Range::new(0, 3));
        let kinds: Vec<TokenKind> = spans.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Property,
                TokenKind::Punctuation,
                TokenKind::Comment,
                TokenKind::Property,
                TokenKind::Punctuation,
                TokenKind::Number,
                TokenKind::Property,
                TokenKind::Punctuation,
                TokenKind::Constant
            ]
        );
    }

    #[test]
    fn fallback_rust_and_shell() {
        let kinds = |language, text: &str| -> Vec<(String, TokenKind)> {
            let doc = Document::from_str(text);
            fallback::spans(language, &doc, Range::new(0, doc.line_count()))
                .into_iter()
                .map(|s| (doc.slice(s.range), s.kind))
                .collect()
        };
        let rust = kinds(
            Language::Rust,
            "#[derive(Debug)]\npub fn run<'a>(x: &'a str) -> Vec<u8> { println!(\"hi {}\", 'c'); } // done\n",
        );
        let has = |list: &[(String, TokenKind)], text: &str, kind| {
            list.iter().any(|(t, k)| t == text && *k == kind)
        };
        assert!(
            has(&rust, "#[derive(Debug)]", TokenKind::Keyword),
            "{rust:?}"
        );
        assert!(has(&rust, "fn", TokenKind::Keyword));
        assert!(has(&rust, "run", TokenKind::Function) || has(&rust, "'a", TokenKind::Variable));
        assert!(
            has(&rust, "'a", TokenKind::Variable),
            "a lifetime is not a string: {rust:?}"
        );
        assert!(has(&rust, "Vec", TokenKind::Type));
        assert!(has(&rust, "println", TokenKind::Function));
        assert!(has(&rust, "\"hi {}\"", TokenKind::String));
        assert!(has(&rust, "'c'", TokenKind::String));
        assert!(has(&rust, "// done", TokenKind::Comment));
        let sh = kinds(
            Language::Shell,
            "#!/bin/sh\nfor f in \"$@\"; do echo ${f#x} $HOME; done # all\n",
        );
        assert!(has(&sh, "#!/bin/sh", TokenKind::Comment), "{sh:?}");
        assert!(has(&sh, "for", TokenKind::Keyword) && has(&sh, "done", TokenKind::Keyword));
        assert!(
            has(&sh, "${f#x}", TokenKind::Variable),
            "`#` inside a word is not a comment: {sh:?}"
        );
        assert!(has(&sh, "$HOME", TokenKind::Variable));
        assert!(has(&sh, "# all", TokenKind::Comment));
    }

    #[test]
    fn fallback_log() {
        let doc = Document::from_str("2026-10-11T10:00:00 WARN disk\n[step 3] ERROR nan loss\n");
        let spans = fallback::spans(Language::Log, &doc, Range::new(0, 2));
        assert!(spans.iter().any(|s| s.kind == TokenKind::Warning));
        assert!(spans.iter().any(|s| s.kind == TokenKind::Error));
        assert_eq!(
            spans
                .iter()
                .filter(|s| s.kind == TokenKind::Comment)
                .count(),
            2
        );
    }

    #[cfg(all(feature = "lang-json", not(target_arch = "wasm32")))]
    #[test]
    fn tree_sitter_json_incremental() {
        use crate::transaction::{Change, EditKind};
        let mut doc = Document::from_str("{\"a\": 1, \"b\": [true, null]}");
        let mut h = Highlighter::new(Language::Json);
        assert!(h.is_tree_sitter());
        h.set_text(&doc);
        let spans = h.spans(&doc, Range::new(0, 1));
        assert!(
            spans
                .iter()
                .any(|s| s.kind == TokenKind::Property && s.range == Range::new(1, 4))
        );
        assert!(spans.iter().any(|s| s.kind == TokenKind::Number));
        assert!(spans.iter().any(|s| s.kind == TokenKind::Constant));
        let before = doc.clone();
        let tx = Transaction::single(Change::replace(Range::new(6, 7), "\"x\""), EditKind::Typing);
        tx.apply(&mut doc);
        h.edit(&before, &doc, &tx);
        let spans = h.spans(&doc, Range::new(0, 1));
        assert!(
            spans
                .iter()
                .any(|s| s.kind == TokenKind::String && s.range == Range::new(6, 9))
        );
    }

    #[cfg(all(feature = "lang-yaml", not(target_arch = "wasm32")))]
    #[test]
    fn tree_sitter_yaml_visible_range_only() {
        let text: String = (0..2000).map(|i| format!("key{i}: value{i}\n")).collect();
        let doc = Document::from_str(&text);
        let mut h = Highlighter::new(Language::Yaml);
        h.set_text(&doc);
        let spans = h.spans(&doc, Range::new(1000, 1010));
        assert!(!spans.is_empty());
        assert!(
            spans
                .iter()
                .all(|s| s.range.start >= doc.line_start(1000)
                    && s.range.end <= doc.line_start(1010))
        );
    }
}
