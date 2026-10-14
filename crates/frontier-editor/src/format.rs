// SPDX-License-Identifier: Apache-2.0
//! Formatting on demand for JSON, YAML and TOML.
//!
//! JSON is re-laid out from its tokens: every string, number and key is kept as it is
//! written, in the order it is written; only the whitespace between them changes.
//!
//! YAML and TOML carry comments and layout a person chose, so their formatter is
//! deliberately small: spacing around the key separator, trailing whitespace, runs of
//! blank lines and the final newline. It does not re-indent and does not re-flow. Its
//! result is checked before it is returned: the formatted text must parse to the same
//! value and hold the same comments, or nothing is changed and the reason is given.
//!
//! Every formatter here is idempotent: formatting its own output changes nothing.

use crate::syntax::Language;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOptions {
    /// One level of indentation (JSON).
    pub indent: String,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            indent: "  ".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The text does not parse; the message says where.
    Syntax(String),
    /// No formatter for this language.
    Unsupported(Language),
    /// The formatter's result failed its own check (the value or a comment would change).
    Unsafe,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(m) => write!(f, "Format unavailable: the text has a syntax error ({m})"),
            Self::Unsupported(l) => write!(
                f,
                "Format unavailable: there is no formatter for {} here",
                language_name(*l)
            ),
            Self::Unsafe => f.write_str(
                "Format unavailable: this text cannot be formatted without changing a value or a comment",
            ),
        }
    }
}

impl std::error::Error for FormatError {}

fn language_name(l: Language) -> &'static str {
    match l {
        Language::Json => "JSON",
        Language::Yaml => "YAML",
        Language::Toml => "TOML",
        Language::Python => "Python",
        Language::Markdown => "Markdown",
        Language::Rust => "Rust",
        Language::Shell => "shell scripts",
        Language::Java => "Java",
        Language::Matlab => "MATLAB",
        Language::Tcl => "Tcl",
        Language::Log => "logs",
        Language::Plain => "plain text",
    }
}

/// Whether [`format`] can format this language at all.
pub fn supported(language: Language) -> bool {
    language == Language::Json
        || (cfg!(feature = "schema") && matches!(language, Language::Yaml | Language::Toml))
}

/// Formats `text`. `Ok` holds the formatted text (equal to `text` when it was already
/// formatted); `Err` says why nothing was done.
pub fn format(text: &str, language: Language, opts: &FormatOptions) -> Result<String, FormatError> {
    match language {
        Language::Json => json(text, opts),
        #[cfg(feature = "schema")]
        Language::Yaml => checked(text, language, yaml(text)),
        #[cfg(feature = "schema")]
        Language::Toml => checked(text, language, toml(text)),
        other => Err(FormatError::Unsupported(other)),
    }
}

// ---- JSON ------------------------------------------------------------------------------------

fn json(text: &str, opts: &FormatOptions) -> Result<String, FormatError> {
    if text.trim().is_empty() {
        return Ok(text.to_string());
    }
    serde_json::from_str::<serde::de::IgnoredAny>(text)
        .map_err(|e| FormatError::Syntax(format!("line {}: {e}", e.line())))?;
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut depth = 0usize;
    let mut chars = text.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(&opts.indent);
        }
    };
    // an opening bracket whose first member has not been written yet
    let mut just_opened = false;
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '{' | '[' => {
                if just_opened {
                    newline(&mut out, depth);
                }
                out.push(c);
                depth += 1;
                just_opened = true;
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if !just_opened {
                    newline(&mut out, depth);
                }
                out.push(c);
                just_opened = false;
            }
            ',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            _ => {
                if just_opened {
                    newline(&mut out, depth);
                    just_opened = false;
                }
                out.push(c);
                if c == '"' {
                    while let Some(d) = chars.next() {
                        out.push(d);
                        match d {
                            '\\' => {
                                if let Some(e) = chars.next() {
                                    out.push(e);
                                }
                            }
                            '"' => break,
                            _ => {}
                        }
                    }
                } else {
                    while let Some(d) = chars.peek() {
                        if d.is_whitespace() || matches!(d, ',' | ':' | '{' | '}' | '[' | ']') {
                            break;
                        }
                        out.push(*d);
                        chars.next();
                    }
                }
            }
        }
    }
    out.push('\n');
    Ok(out)
}

// ---- shared line helpers ---------------------------------------------------------------------

/// Collapses runs of blank lines to one, drops blank lines at the start and the end,
/// and ends the text with one newline. `keep[i]` marks lines that must stay as they
/// are (inside a multi-line value).
#[cfg(feature = "schema")]
fn finish(lines: Vec<(String, bool)>) -> String {
    let mut out = String::new();
    let mut blank_run = 0usize;
    let mut started = false;
    let last_content = lines
        .iter()
        .rposition(|(l, keep)| *keep || !l.trim().is_empty());
    for (i, (line, keep)) in lines.iter().enumerate() {
        if last_content.is_none_or(|last| i > last) {
            break;
        }
        if !*keep && line.trim().is_empty() {
            blank_run += 1;
            if !started || blank_run > 1 {
                continue;
            }
            out.push('\n');
            continue;
        }
        blank_run = 0;
        started = true;
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The formatted text is returned only when it means the same and keeps every comment.
#[cfg(feature = "schema")]
fn checked(
    original: &str,
    language: Language,
    formatted: Result<String, FormatError>,
) -> Result<String, FormatError> {
    let formatted = formatted?;
    if formatted == original {
        return Ok(formatted);
    }
    let before = crate::schema::parse(original, language)
        .map_err(|d| FormatError::Syntax(d.message.clone()))?;
    let Ok(after) = crate::schema::parse(&formatted, language) else {
        return Err(FormatError::Unsafe);
    };
    if before.value != after.value || comments(original, language) != comments(&formatted, language)
    {
        return Err(FormatError::Unsafe);
    }
    Ok(formatted)
}

/// The comments of a YAML or TOML text, in order, without the whitespace around them.
#[cfg(feature = "schema")]
pub fn comments(text: &str, language: Language) -> Vec<String> {
    let mut out = Vec::new();
    match language {
        Language::Toml => {
            let mut st = TomlState::default();
            for line in text.lines() {
                if let Some(at) = st.scan(line).comment {
                    out.push(line[at..].trim_end().to_string());
                }
            }
        }
        Language::Yaml => {
            let mut block: Option<usize> = None;
            for line in text.lines() {
                let indent = line.len() - line.trim_start_matches(' ').len();
                if let Some(b) = block {
                    if line.trim().is_empty() || indent > b {
                        continue;
                    }
                    block = None;
                }
                let (comment, opens) = yaml_scan(line);
                if let Some(at) = comment {
                    out.push(line[at..].trim_end().to_string());
                }
                if opens {
                    block = Some(indent);
                }
            }
        }
        _ => {}
    }
    out
}

// ---- TOML ------------------------------------------------------------------------------------

#[cfg(feature = "schema")]
#[derive(Default)]
struct TomlState {
    /// Inside a `"""` or `'''` string: its quote.
    multi: Option<char>,
    /// Open `[` of a value that continues on the next line.
    depth: usize,
}

#[cfg(feature = "schema")]
struct TomlLine {
    /// The line began outside any multi-line value.
    top: bool,
    /// Byte offset of the `=` of a `key = value` line.
    eq: Option<usize>,
    /// Byte offset of a comment.
    comment: Option<usize>,
    /// The line began inside a multi-line string.
    in_string: bool,
}

#[cfg(feature = "schema")]
impl TomlState {
    fn scan(&mut self, line: &str) -> TomlLine {
        let top = self.multi.is_none() && self.depth == 0;
        let in_string = self.multi.is_some();
        let header = top && line.trim_start().starts_with('[');
        let b = line.as_bytes();
        let mut eq = None;
        let mut comment = None;
        let mut i = 0;
        // a one-line string: its quote
        let mut single: Option<u8> = None;
        while i < b.len() {
            let c = b[i];
            if let Some(q) = self.multi {
                if c == b'\\' && q == '"' {
                    i += 2;
                    continue;
                }
                if c == q as u8 && b[i..].starts_with(&[c, c, c]) {
                    self.multi = None;
                    i += 3;
                    continue;
                }
                i += 1;
                continue;
            }
            if let Some(q) = single {
                if c == b'\\' && q == b'"' {
                    i += 2;
                    continue;
                }
                if c == q {
                    single = None;
                }
                i += 1;
                continue;
            }
            match c {
                b'"' | b'\'' => {
                    if b[i..].starts_with(&[c, c, c]) {
                        self.multi = Some(c as char);
                        i += 3;
                        continue;
                    }
                    single = Some(c);
                }
                b'#' => {
                    comment = Some(i);
                    break;
                }
                b'=' if top && eq.is_none() && !header => eq = Some(i),
                b'[' if !header && (eq.is_some() || !top) => self.depth += 1,
                b']' if !header && (eq.is_some() || !top) => {
                    self.depth = self.depth.saturating_sub(1)
                }
                _ => {}
            }
            i += 1;
        }
        TomlLine {
            top,
            eq,
            comment,
            in_string,
        }
    }
}

#[cfg(feature = "schema")]
fn toml(text: &str) -> Result<String, FormatError> {
    crate::schema::parse(text, Language::Toml).map_err(|d| FormatError::Syntax(d.message))?;
    let mut st = TomlState::default();
    let mut lines: Vec<(String, bool)> = Vec::new();
    for raw in text.lines() {
        let info = st.scan(raw);
        if info.in_string {
            lines.push((raw.to_string(), true));
            continue;
        }
        if !info.top {
            // inside a multi-line array: only the end of the line
            lines.push((raw.trim_end().to_string(), true));
            continue;
        }
        let line = raw.trim_end();
        let lead = line.len() - line.trim_start().len();
        let mut s = line[lead..].to_string();
        if let Some(eq) = info.eq
            && eq >= lead
        {
            let key = line[lead..eq].trim_end();
            let value = line[eq + 1..].trim_start();
            s = format!("{key} = {value}");
        }
        // a multi-line string that opens on this line keeps its own line ends
        lines.push((s, st.multi.is_some()));
    }
    Ok(finish(lines))
}

// ---- YAML ------------------------------------------------------------------------------------

/// Byte offset of a comment on a YAML line, and whether the line opens a block scalar
/// (`key: |`, `- >-`).
#[cfg(feature = "schema")]
fn yaml_scan(line: &str) -> (Option<usize>, bool) {
    let b = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut comment = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(b'"') if c == b'\\' => {
                i += 2;
                continue;
            }
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'"' | b'\'' => {
                    // a quote opens a scalar only where a scalar can start
                    let before = line[..i].trim_end();
                    if before.is_empty()
                        || before.ends_with(':')
                        || before.ends_with('-')
                        || before.ends_with(',')
                        || before.ends_with('[')
                        || before.ends_with('{')
                    {
                        quote = Some(c);
                    }
                }
                b'#' if i == 0 || b[i - 1] == b' ' || b[i - 1] == b'\t' => {
                    comment = Some(i);
                    break;
                }
                _ => {}
            },
        }
        i += 1;
    }
    let content = line[..comment.unwrap_or(line.len())].trim_end();
    let indicator = content
        .rsplit([' ', '\t'])
        .next()
        .unwrap_or("")
        .trim_end_matches(|c: char| c == '+' || c == '-' || c.is_ascii_digit());
    let head = content[..content
        .len()
        .saturating_sub(content.rsplit([' ', '\t']).next().map_or(0, str::len))]
        .trim_end();
    let opens = matches!(indicator, "|" | ">")
        && (head.ends_with(':') || head.ends_with('-') || head.is_empty());
    (comment, opens)
}

#[cfg(feature = "schema")]
fn yaml(text: &str) -> Result<String, FormatError> {
    crate::schema::parse(text, Language::Yaml).map_err(|d| FormatError::Syntax(d.message))?;
    let mut lines: Vec<(String, bool)> = Vec::new();
    let mut block: Option<usize> = None;
    for raw in text.lines() {
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        if let Some(b) = block {
            if raw.trim().is_empty() || indent > b {
                // the content of a block scalar is the value itself
                lines.push((raw.to_string(), true));
                continue;
            }
            block = None;
        }
        let (comment, opens) = yaml_scan(raw);
        if opens {
            block = Some(indent);
        }
        let mut s = raw.trim_end().to_string();
        // `key:    value` → `key: value` for a plain key at the start of the line
        let body_start = {
            let t = s.trim_start_matches(' ');
            let mut at = s.len() - t.len();
            while s[at..].starts_with("- ") {
                at += 2;
                at += s[at..].len() - s[at..].trim_start_matches(' ').len();
            }
            at
        };
        let body = &s[body_start..];
        let plain_key = body
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if plain_key
            && let Some(colon) = body.find(": ")
            && comment.is_none_or(|c| c > body_start + colon)
            && !body[..colon].contains(['"', '\'', '{', '[', '#'])
        {
            let after = &body[colon + 1..];
            let value = after.trim_start_matches(' ');
            if !value.is_empty() && !value.starts_with('#') && after.len() - value.len() > 1 {
                s = format!("{}{}: {}", &s[..body_start], &body[..colon], value);
            }
        }
        lines.push((s, false));
    }
    Ok(finish(lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(text: &str, l: Language) -> String {
        let out = format(text, l, &FormatOptions::default()).expect("formats");
        assert_eq!(
            format(&out, l, &FormatOptions::default()).as_deref(),
            Ok(out.as_str()),
            "formatting again changes nothing"
        );
        out
    }

    #[test]
    fn json_is_laid_out_again_and_keeps_every_token() {
        let text = "{\"b\":1.0,\"a\":[1,2,{\"x\":\"a, {b}: \\\"c\\\"\"}],\"e\":{},\"n\":[ ],\"big\":12345678901234567890}";
        assert_eq!(
            fmt(text, Language::Json),
            "{\n  \"b\": 1.0,\n  \"a\": [\n    1,\n    2,\n    {\n      \"x\": \"a, {b}: \\\"c\\\"\"\n    }\n  ],\n  \"e\": {},\n  \"n\": [],\n  \"big\": 12345678901234567890\n}\n"
        );
        let four = format(
            "{\"a\":[true]}",
            Language::Json,
            &FormatOptions {
                indent: "\t".into(),
            },
        );
        assert_eq!(four.as_deref(), Ok("{\n\t\"a\": [\n\t\ttrue\n\t]\n}\n"));
        assert_eq!(fmt("[]", Language::Json), "[]\n");
        assert_eq!(fmt("", Language::Json), "");
    }

    #[test]
    fn a_syntax_error_formats_nothing_and_says_where() {
        let e =
            format("{\n\"a\": }", Language::Json, &FormatOptions::default()).expect_err("refused");
        assert!(matches!(e, FormatError::Syntax(_)));
        assert!(
            e.to_string()
                .starts_with("Format unavailable: the text has a syntax error (line 2")
        );
        assert!(matches!(
            format("a: [1,\n", Language::Yaml, &FormatOptions::default()),
            Err(FormatError::Syntax(_))
        ));
        assert!(matches!(
            format("a = \n", Language::Toml, &FormatOptions::default()),
            Err(FormatError::Syntax(_))
        ));
    }

    #[test]
    fn other_languages_say_there_is_no_formatter() {
        let e = format("x=1", Language::Python, &FormatOptions::default()).expect_err("none");
        assert_eq!(
            e.to_string(),
            "Format unavailable: there is no formatter for Python here"
        );
        assert!(!supported(Language::Python) && supported(Language::Json));
    }

    #[test]
    fn toml_keeps_comments_and_values() {
        let text = "\n\n# Frontier settings   \n  [server]   \nbind=\"0.0.0.0:8787\"   # where\nport   =   8787\n\n\n\n[editor]\n\tfont_size= 14 #px\nlist = [\n  1,   \n  2, # two\n]\ntext = \"\"\"\n  kept   \n\n\n  as is = yes\n\"\"\"\nurl = \"a=b#c\"\n\n\n";
        let out = fmt(text, Language::Toml);
        assert_eq!(
            out,
            "# Frontier settings\n[server]\nbind = \"0.0.0.0:8787\"   # where\nport = 8787\n\n[editor]\nfont_size = 14 #px\nlist = [\n  1,\n  2, # two\n]\ntext = \"\"\"\n  kept   \n\n\n  as is = yes\n\"\"\"\nurl = \"a=b#c\"\n"
        );
        assert_eq!(
            comments(&out, Language::Toml),
            vec!["# Frontier settings", "# where", "#px", "# two"]
        );
        assert_eq!(
            comments(text, Language::Toml),
            comments(&out, Language::Toml)
        );
    }

    #[test]
    fn yaml_keeps_comments_block_scalars_and_indentation() {
        let text = "\n# training   \nmodel:     transolver   # the model\ndata:\n    batch:   4\n    notes: |\n      first   \n\n\n      key:    spaced stays\n    after:  1\n\n\n\nsteps:\n  -   op:   cut\n      at: 2\n  - \"quoted:   key\":   1\nurl: http://a/b#c   \n";
        let out = fmt(text, Language::Yaml);
        assert_eq!(
            out,
            "# training\nmodel: transolver   # the model\ndata:\n    batch: 4\n    notes: |\n      first   \n\n\n      key:    spaced stays\n    after: 1\n\nsteps:\n  -   op: cut\n      at: 2\n  - \"quoted:   key\":   1\nurl: http://a/b#c\n"
        );
        assert_eq!(
            comments(&out, Language::Yaml),
            vec!["# training", "# the model"]
        );
    }

    #[test]
    fn formatted_text_is_returned_unchanged() {
        for (t, l) in [
            ("a: 1\nb:\n  - x\n", Language::Yaml),
            ("a = 1\n\n[t]\nb = \"x\"\n", Language::Toml),
            ("{\n  \"a\": 1\n}\n", Language::Json),
        ] {
            assert_eq!(fmt(t, l), t);
        }
    }
}
