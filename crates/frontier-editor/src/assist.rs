// SPDX-License-Identifier: Apache-2.0
//! Completion, hover and quick fixes for JSON, YAML and TOML documents, driven by the
//! JSON Schema of the file's kind, and word completion for everything else.
//!
//! Nothing here needs the document to be valid: where the caret is (which object, a key
//! or a value) is read from the text around it, and the values already written are read
//! from a lenient parse, so the schema's conditional parts (`if` / `then`, `anyOf`)
//! follow what the document says while it is being typed.

use crate::document::Range;
use crate::schema::{self, Located, SchemaEntry};
use crate::syntax::Language;
use serde_json::{Map, Value};

/// Documents above this size get no schema completion or hover (they are viewers).
pub const ASSIST_BUDGET_CHARS: usize = 512 * 1024;
const MAX_ITEMS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// A key the schema allows here.
    Key,
    /// A value the schema allows here.
    Value,
    /// Several keys at once (the required ones that are missing).
    Snippet,
    /// A word that occurs in this file.
    Word,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
    /// Type and whether the key is required; `default` for a default value.
    pub detail: String,
    /// The schema's description.
    pub documentation: String,
    /// What is replaced (char range) and by what.
    pub replace: Range,
    pub insert: String,
    /// The selection afterwards, in chars from the start of `insert`.
    pub select: (usize, usize),
    /// Further edits the insertion needs (a comma after the previous member in JSON).
    pub also: Vec<(Range, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionList {
    pub items: Vec<Completion>,
    /// What the list is made from when it is not the schema ("Words from this file ...").
    pub note: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    /// The key or value the text is about.
    pub range: Range,
    /// Dotted path of the value (`editor.font_size`, `steps[0].op`).
    pub path: String,
    pub title: String,
    pub type_name: String,
    pub description: String,
    /// The default, as it is written in this language.
    pub default: Option<String>,
    /// Allowed values, as they are written in this language.
    pub values: Vec<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickFix {
    pub title: String,
    /// The text the problem is on (the diagnostic's range).
    pub range: Range,
    /// JSON pointer of the value the problem is on.
    pub pointer: String,
    pub edits: Vec<(Range, String)>,
}

pub const NOTE_WORDS_PYTHON: &str = "Words from this file. Python has no language server here.";
pub const NOTE_WORDS_NO_SCHEMA: &str = "Words from this file. This kind of file has no schema.";
pub const NOTE_WORDS: &str = "Words from this file.";

// ---- schema navigation ------------------------------------------------------------------

struct Nav<'a> {
    root: &'a Value,
}

fn type_matches(t: &Value, inst: &Value) -> bool {
    let one = |name: &str| match name {
        "object" => inst.is_object(),
        "array" => inst.is_array(),
        "string" => inst.is_string(),
        "boolean" => inst.is_boolean(),
        "null" => inst.is_null(),
        "integer" => inst.is_i64() || inst.is_u64(),
        "number" => inst.is_number(),
        _ => true,
    };
    match t {
        Value::String(s) => one(s),
        Value::Array(a) => a.iter().filter_map(Value::as_str).any(one),
        _ => true,
    }
}

impl<'a> Nav<'a> {
    fn deref(&self, node: &'a Value) -> &'a Value {
        let mut n = node;
        for _ in 0..16 {
            match n.get("$ref").and_then(Value::as_str) {
                Some(r) if r.starts_with("#/") || r == "#" => match self.root.pointer(&r[1..]) {
                    Some(t) => n = t,
                    None => break,
                },
                _ => break,
            }
        }
        n
    }

    /// Whether `inst` satisfies the parts of `cond` an `if` of our schemas uses: the
    /// type, `required`, and `const` / `enum` of properties.
    fn matches(&self, cond: &'a Value, inst: &Value) -> bool {
        let cond = self.deref(cond);
        if let Some(t) = cond.get("type")
            && !type_matches(t, inst)
        {
            return false;
        }
        if let Some(c) = cond.get("const")
            && c != inst
        {
            return false;
        }
        if let Some(req) = cond.get("required").and_then(Value::as_array) {
            for r in req.iter().filter_map(Value::as_str) {
                if inst.get(r).is_none() {
                    return false;
                }
            }
        }
        if let Some(props) = cond.get("properties").and_then(Value::as_object) {
            for (k, s) in props {
                let Some(v) = inst.get(k) else { continue };
                let s = self.deref(s);
                if let Some(c) = s.get("const")
                    && c != v
                {
                    return false;
                }
                if let Some(e) = s.get("enum").and_then(Value::as_array)
                    && !e.contains(v)
                {
                    return false;
                }
            }
        }
        true
    }

    /// Every schema object that applies at `node` for the value `inst` (when known):
    /// the node, what it refers to, `allOf` members, the `anyOf` / `oneOf` members the
    /// value can be, and the branch of `if` the value takes.
    fn expand(&self, node: &'a Value, inst: Option<&Value>, out: &mut Vec<&'a Value>, depth: u8) {
        if depth > 12 || !node.is_object() {
            return;
        }
        // keywords next to a `$ref` (a description, a default) apply too
        let target = self.deref(node);
        for n in [node, target] {
            if out.iter().any(|x| std::ptr::eq(*x, n)) {
                continue;
            }
            out.push(n);
            if let Some(all) = n.get("allOf").and_then(Value::as_array) {
                for m in all {
                    self.expand(m, inst, out, depth + 1);
                }
            }
            for key in ["anyOf", "oneOf"] {
                let Some(members) = n.get(key).and_then(Value::as_array) else {
                    continue;
                };
                let fits: Vec<&'a Value> = match inst {
                    Some(i) if !i.is_null() => {
                        members.iter().filter(|m| self.matches(m, i)).collect()
                    }
                    _ => Vec::new(),
                };
                let chosen: Vec<&'a Value> = if fits.is_empty() {
                    members.iter().collect()
                } else {
                    fits
                };
                for m in chosen {
                    self.expand(m, inst, out, depth + 1);
                }
            }
            if let Some(cond) = n.get("if") {
                let pass = inst.is_some_and(|i| self.matches(cond, i));
                if let Some(branch) = n.get(if pass { "then" } else { "else" }) {
                    self.expand(branch, inst, out, depth + 1);
                }
            }
        }
    }

    fn child(&self, nodes: &[&'a Value], seg: &str, inst: Option<&Value>) -> Vec<&'a Value> {
        let mut out = Vec::new();
        let index = seg.parse::<usize>().ok();
        for n in nodes {
            let mut named = false;
            if let Some(p) = n.get("properties").and_then(|p| p.get(seg)) {
                self.expand(p, inst, &mut out, 0);
                named = true;
            }
            if let Some(i) = index {
                if let Some(p) = n
                    .get("prefixItems")
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(i))
                {
                    self.expand(p, inst, &mut out, 0);
                } else if let Some(items) = n.get("items") {
                    self.expand(items, inst, &mut out, 0);
                }
            }
            if !named
                && index.is_none()
                && let Some(extra) = n.get("additionalProperties")
                && extra.is_object()
            {
                self.expand(extra, inst, &mut out, 0);
            }
        }
        out
    }

    /// The schemas of the value at `path`, following `inst_root` for the conditions.
    fn at(&self, path: &[String], inst_root: Option<&Value>) -> Vec<&'a Value> {
        let mut inst = inst_root;
        let mut nodes = Vec::new();
        self.expand(self.root, inst, &mut nodes, 0);
        for seg in path {
            let next = inst.and_then(|i| step(i, seg));
            nodes = self.child(&nodes, seg, next);
            inst = next;
            if nodes.is_empty() {
                break;
            }
        }
        nodes
    }
}

fn step<'v>(v: &'v Value, seg: &str) -> Option<&'v Value> {
    match v {
        Value::Object(m) => m.get(seg),
        Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    }
}

fn value_at<'v>(root: &'v Value, path: &[String]) -> Option<&'v Value> {
    let mut v = root;
    for seg in path {
        v = step(v, seg)?;
    }
    Some(v)
}

#[derive(Debug, Default, Clone)]
struct Info {
    title: String,
    description: String,
    types: Vec<String>,
    default: Option<Value>,
    values: Vec<Value>,
}

fn info(nodes: &[&Value]) -> Info {
    let mut out = Info::default();
    let add_type = |out: &mut Info, t: &str| {
        if !out.types.iter().any(|x| x == t) {
            out.types.push(t.to_string());
        }
    };
    for n in nodes {
        if out.description.is_empty()
            && let Some(d) = n.get("description").and_then(Value::as_str)
        {
            out.description = d.to_string();
        }
        if out.title.is_empty()
            && let Some(t) = n.get("title").and_then(Value::as_str)
        {
            out.title = t.to_string();
        }
        if out.default.is_none()
            && let Some(d) = n.get("default")
        {
            out.default = Some(d.clone());
        }
        match n.get("type") {
            Some(Value::String(t)) => add_type(&mut out, t),
            Some(Value::Array(a)) => {
                for t in a.iter().filter_map(Value::as_str) {
                    add_type(&mut out, t);
                }
            }
            _ => {
                if n.get("properties").is_some() {
                    add_type(&mut out, "object");
                } else if n.get("items").is_some() {
                    add_type(&mut out, "array");
                }
            }
        }
        let mut add_value = |v: &Value| {
            if !out.values.contains(v) {
                out.values.push(v.clone());
            }
        };
        if let Some(e) = n.get("enum").and_then(Value::as_array) {
            e.iter().for_each(&mut add_value);
        }
        if let Some(c) = n.get("const") {
            add_value(c);
        }
    }
    // a member of an `anyOf` that only says "or nothing" is not a type worth naming first
    if out.types.len() > 1
        && let Some(i) = out.types.iter().position(|t| t == "null")
    {
        let null = out.types.remove(i);
        out.types.push(null);
    }
    out
}

impl Info {
    fn type_name(&self) -> String {
        if self.types.is_empty() {
            return match self.values.first() {
                Some(Value::String(_)) => "string".into(),
                Some(Value::Bool(_)) => "boolean".into(),
                Some(Value::Number(_)) => "number".into(),
                _ => String::new(),
            };
        }
        self.types.join(" | ")
    }
    fn is(&self, t: &str) -> bool {
        self.types.first().is_some_and(|x| x == t)
    }
}

/// The properties objects described by `nodes` may have, in schema order, with whether
/// each is required.
fn properties<'a>(nav: &Nav<'a>, nodes: &[&'a Value]) -> Vec<(String, Vec<&'a Value>, bool)> {
    let mut required: Vec<&str> = Vec::new();
    for n in nodes {
        if let Some(r) = n.get("required").and_then(Value::as_array) {
            required.extend(r.iter().filter_map(Value::as_str));
        }
    }
    let mut out: Vec<(String, Vec<&'a Value>, bool)> = Vec::new();
    for n in nodes {
        let Some(props) = n.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (name, schema) in props {
            let at = match out.iter().position(|(k, _, _)| k == name) {
                Some(i) => i,
                None => {
                    out.push((name.clone(), Vec::new(), required.contains(&name.as_str())));
                    out.len() - 1
                }
            };
            nav.expand(schema, None, &mut out[at].1, 0);
        }
    }
    out
}

// ---- how values are written in each language -------------------------------------------------

fn yaml_plain_ok(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-' | '+'))
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '_')
        && !matches!(
            s.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n"
        )
}

/// A JSON value written in `language`.
pub fn literal(v: &Value, language: Language) -> String {
    match (v, language) {
        (Value::String(s), Language::Yaml) if yaml_plain_ok(s) => s.clone(),
        (Value::Null, Language::Yaml) => "null".into(),
        (Value::Null, Language::Toml) => "\"\"".into(),
        (Value::Object(m), Language::Toml) => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{k} = {}", literal(v, language)))
                .collect();
            if inner.is_empty() {
                "{}".into()
            } else {
                format!("{{ {} }}", inner.join(", "))
            }
        }
        (Value::Array(a), Language::Toml) => {
            let inner: Vec<String> = a.iter().map(|v| literal(v, language)).collect();
            format!("[{}]", inner.join(", "))
        }
        _ => v.to_string(),
    }
}

/// What to write for a key the schema describes with `info` when nothing better is
/// known, and the part of it a person will want to type over (chars from its start).
fn skeleton(i: &Info, language: Language) -> (String, (usize, usize)) {
    let whole = |s: String| {
        let n = s.chars().count();
        (s, (0, n))
    };
    if let Some(d) = &i.default
        && !d.is_null()
        && !d.is_object()
        && !d.is_array()
    {
        let s = literal(d, language);
        return match d {
            // inside the quotes
            Value::String(_) if s.starts_with('"') => {
                let n = s.chars().count();
                (s, (1, n - 1))
            }
            _ => whole(s),
        };
    }
    if let Some(v) = i.values.first() {
        let s = literal(v, language);
        if matches!(v, Value::String(_)) && s.starts_with('"') {
            let n = s.chars().count();
            return (s, (1, n - 1));
        }
        return whole(s);
    }
    match i.types.first().map(String::as_str) {
        Some("string") => ("\"\"".into(), (1, 1)),
        Some("integer") | Some("number") => whole("0".into()),
        Some("boolean") => whole("false".into()),
        Some("array") => ("[]".into(), (1, 1)),
        Some("object") => ("{}".into(), (1, 1)),
        _ => (String::new(), (0, 0)),
    }
}

// ---- where the caret is ----------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Key,
    Value,
    /// Inside a TOML `[table]` header.
    Table,
}

#[derive(Debug, Clone)]
struct Ctx {
    /// Key slot: the object the key goes into. Value slot: the value itself.
    path: Vec<String>,
    slot: Slot,
    /// What a completion replaces.
    replace: Range,
    /// What is typed so far (without quotes).
    prefix: String,
    /// Key slot: a separator already follows the key.
    separated: bool,
    /// Whitespace that starts further members of the same object.
    indent: String,
    /// JSON: a member follows (a comma is needed after an inserted one) and a member
    /// precedes without a comma (offset where one is needed).
    comma_after: bool,
    comma_before: Option<usize>,
    /// The line holds nothing but what is replaced.
    alone_on_line: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn line_bounds(chars: &[char], at: usize) -> (usize, usize) {
    let at = at.min(chars.len());
    let start = chars[..at]
        .iter()
        .rposition(|c| *c == '\n')
        .map_or(0, |i| i + 1);
    let end = chars[at..]
        .iter()
        .position(|c| *c == '\n')
        .map_or(chars.len(), |i| at + i);
    (start, end)
}

fn json_ctx(chars: &[char], at: usize) -> Ctx {
    enum Fr {
        Obj {
            key: Option<String>,
            colon: bool,
            /// The member's value is written; a comma has not followed yet.
            done: bool,
        },
        Arr {
            idx: usize,
        },
    }
    let at = at.min(chars.len());
    let mut stack: Vec<Fr> = Vec::new();
    // end of the last complete value before the caret, and whether a comma followed it
    let mut last_value_end: Option<usize> = None;
    let mut i = 0;
    let mut inside: Option<(usize, usize)> = None;
    while i < at {
        match chars[i] {
            '"' => {
                let start = i;
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < chars.len() {
                    match chars[i] {
                        '\\' => {
                            if let Some(c) = chars.get(i + 1) {
                                s.push(*c);
                            }
                            i += 2;
                        }
                        '"' => {
                            closed = true;
                            i += 1;
                            break;
                        }
                        '\n' => break,
                        c => {
                            s.push(c);
                            i += 1;
                        }
                    }
                }
                let i_end = i.min(chars.len());
                if at > start && (at < i_end || (!closed && at <= i_end)) {
                    inside = Some((start, i_end));
                    break;
                }
                i = i_end;
                match stack.last_mut() {
                    Some(Fr::Obj { key, colon, .. }) if !*colon => *key = Some(s),
                    Some(Fr::Obj { done, .. }) => {
                        *done = true;
                        last_value_end = Some(i);
                    }
                    _ => last_value_end = Some(i),
                }
            }
            '{' => {
                stack.push(Fr::Obj {
                    key: None,
                    colon: false,
                    done: false,
                });
                last_value_end = None;
                i += 1;
            }
            '[' => {
                stack.push(Fr::Arr { idx: 0 });
                last_value_end = None;
                i += 1;
            }
            '}' | ']' => {
                stack.pop();
                i += 1;
                last_value_end = Some(i);
                if let Some(Fr::Obj { done, .. }) = stack.last_mut() {
                    *done = true;
                }
            }
            ':' => {
                if let Some(Fr::Obj { colon, .. }) = stack.last_mut() {
                    *colon = true;
                }
                i += 1;
            }
            ',' => {
                match stack.last_mut() {
                    Some(Fr::Obj { key, colon, done }) => {
                        *key = None;
                        *colon = false;
                        *done = false;
                    }
                    Some(Fr::Arr { idx }) => *idx += 1,
                    None => {}
                }
                last_value_end = None;
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                // a bare word or number
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], ',' | ':' | '{' | '}' | '[' | ']' | '"')
                {
                    i += 1;
                }
                if i < at {
                    last_value_end = Some(i);
                    if let Some(Fr::Obj {
                        colon: true, done, ..
                    }) = stack.last_mut()
                    {
                        *done = true;
                    }
                }
            }
        }
    }
    let (replace, prefix) = match inside {
        Some((start, end)) => (
            Range::new(start, end),
            chars[start + 1..at].iter().collect::<String>(),
        ),
        None => {
            let mut a = at;
            while a > 0 && (is_word(chars[a - 1]) || matches!(chars[a - 1], '-' | '.')) {
                a -= 1;
            }
            let mut b = at;
            while b < chars.len() && (is_word(chars[b]) || matches!(chars[b], '-' | '.')) {
                b += 1;
            }
            (Range::new(a, b), chars[a..at].iter().collect::<String>())
        }
    };
    let mut path: Vec<String> = Vec::new();
    let n = stack.len();
    let mut slot = Slot::Value;
    for (k, fr) in stack.iter().enumerate() {
        let top = k + 1 == n;
        match fr {
            Fr::Obj { key, colon, done } => {
                // after a member whose comma is not typed yet, the next key
                if top && (!*colon || (*done && inside.is_none())) {
                    slot = Slot::Key;
                } else {
                    path.push(key.clone().unwrap_or_default());
                }
            }
            Fr::Arr { idx } => path.push(idx.to_string()),
        }
    }
    let mut j = replace.end;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    let next = chars.get(j).copied();
    let (ls, le) = line_bounds(chars, replace.start);
    let indent: String = chars[ls..replace.start]
        .iter()
        .take_while(|c| c.is_whitespace())
        .collect();
    let alone = chars[ls..replace.start].iter().all(|c| c.is_whitespace())
        && chars[replace.end.min(le)..le]
            .iter()
            .all(|c| c.is_whitespace());
    let comma_before = match (slot, last_value_end) {
        // the previous value in this object ended without a comma
        (Slot::Key, Some(e)) if e <= replace.start => Some(e),
        _ => None,
    };
    Ctx {
        path,
        slot,
        replace,
        prefix,
        separated: next == Some(':'),
        indent,
        comma_after: slot == Slot::Key && next == Some('"'),
        comma_before,
        alone_on_line: alone,
    }
}

/// Leading sequence dashes of a YAML line (their columns) and the column its content
/// starts at.
fn yaml_lead(line: &[char]) -> (Vec<usize>, usize) {
    let mut i = 0;
    let mut dashes = Vec::new();
    loop {
        while i < line.len() && line[i] == ' ' {
            i += 1;
        }
        if i < line.len() && line[i] == '-' && (i + 1 == line.len() || line[i + 1] == ' ') {
            dashes.push(i);
            i += 1;
        } else {
            break;
        }
    }
    (dashes, i)
}

/// Column of the `:` that ends the key of a YAML line whose content starts at `from`.
fn yaml_colon(line: &[char], from: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut i = from;
    while i < line.len() {
        let c = line[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' if i == from => quote = Some(c),
                '#' if i > from && line[i - 1] == ' ' => return None,
                '{' | '[' if i == from => return None,
                ':' if i + 1 == line.len() || line[i + 1] == ' ' => return Some(i),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// End of a YAML or TOML value that starts at `from` on a line: before a trailing
/// comment and trailing spaces.
fn value_end(line: &[char], from: usize) -> usize {
    let mut end = line.len();
    let mut quote: Option<char> = None;
    for (i, c) in line.iter().enumerate().skip(from) {
        match quote {
            Some(q) => {
                if *c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(*c),
                '#' if i == from || line[i - 1] == ' ' => {
                    end = i;
                    break;
                }
                _ => {}
            },
        }
    }
    while end > from && line[end - 1] == ' ' {
        end -= 1;
    }
    end
}

fn yaml_ctx(chars: &[char], at: usize) -> Ctx {
    let at = at.min(chars.len());
    let (ls, le) = line_bounds(chars, at);
    let line = &chars[ls..le];
    let col = at - ls;
    let (mut dashes, mut ccol) = yaml_lead(line);
    if col < ccol {
        // the caret is in the indentation: a key would start here
        dashes.retain(|d| *d < col);
        ccol = col;
    }
    let colon = yaml_colon(line, ccol);
    let mut tail: Option<String> = None;
    let (slot, replace, prefix, separated) = match colon {
        Some(p) if col > p => {
            let mut vs = p + 1;
            while vs < line.len() && line[vs] == ' ' {
                vs += 1;
            }
            let vs = vs.min(col.max(p + 1));
            let ve = value_end(line, vs).max(col);
            tail = Some(unquote(&line[ccol..p].iter().collect::<String>()));
            let typed: String = line[vs..col].iter().collect();
            (
                Slot::Value,
                Range::new(ls + vs, ls + ve),
                typed.trim_start_matches(['"', '\'']).to_string(),
                true,
            )
        }
        Some(p) => (
            Slot::Key,
            Range::new(ls + ccol, ls + p),
            line[ccol..col.max(ccol)].iter().collect(),
            true,
        ),
        None => {
            let end = value_end(line, ccol).max(col);
            (
                Slot::Key,
                Range::new(ls + ccol, ls + end),
                line[ccol..col.max(ccol)].iter().collect(),
                false,
            )
        }
    };
    // the keys and sequence items the line is inside of, innermost first
    let mut segs: Vec<String> = Vec::new();
    let mut limit = ccol;
    let mut pending: Option<(usize, usize)> = None;
    let dash = |d: usize,
                limit: &mut usize,
                pending: &mut Option<(usize, usize)>,
                segs: &mut Vec<String>| {
        match pending {
            Some((pd, n)) => {
                if d == *pd {
                    *n += 1;
                } else if d < *pd {
                    segs.push(n.to_string());
                    *pending = Some((d, 0));
                    *limit = d;
                }
            }
            None => {
                if d < *limit {
                    *pending = Some((d, 0));
                    *limit = d;
                }
            }
        }
    };
    for d in dashes.iter().rev() {
        dash(*d, &mut limit, &mut pending, &mut segs);
    }
    let mut end = ls;
    while end > 0 && (limit > 0 || pending.is_some()) {
        let (s, e) = line_bounds(chars, end - 1);
        end = s;
        let l = &chars[s..e];
        let (ds, c) = yaml_lead(l);
        if c >= l.len() && ds.is_empty() {
            continue;
        }
        if l.get(c) == Some(&'#') {
            continue;
        }
        if let Some(p) = yaml_colon(l, c) {
            let key = unquote(&l[c..p].iter().collect::<String>());
            match pending {
                Some((pd, n)) => {
                    if c <= pd {
                        segs.push(n.to_string());
                        segs.push(key);
                        pending = None;
                        limit = c;
                    }
                }
                None => {
                    if c < limit {
                        segs.push(key);
                        limit = c;
                    }
                }
            }
        }
        for d in ds.iter().rev() {
            dash(*d, &mut limit, &mut pending, &mut segs);
        }
    }
    if let Some((_, n)) = pending {
        segs.push(n.to_string());
    }
    segs.reverse();
    if let Some(k) = tail {
        segs.push(k);
    }
    let alone = line[..ccol.min(line.len())].iter().all(|c| *c == ' ')
        && replace.end >= ls + value_end(line, ccol.min(line.len()));
    Ctx {
        path: segs,
        slot,
        replace,
        prefix,
        separated,
        indent: " ".repeat(ccol),
        comma_after: false,
        comma_before: None,
        alone_on_line: alone && dashes.is_empty(),
    }
}

/// Splits a dotted TOML key (`a."b.c".d`).
fn toml_dotted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None => match c {
                '"' | '\'' => quote = Some(c),
                '.' => out.push(std::mem::take(&mut cur).trim().to_string()),
                _ => cur.push(c),
            },
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// The table a TOML line belongs to: the path of the last header above `line_start`.
fn toml_table(chars: &[char], line_start: usize) -> Vec<String> {
    let mut headers: Vec<(Vec<String>, bool)> = Vec::new();
    let mut i = 0;
    while i < line_start {
        let (s, e) = line_bounds(chars, i);
        let l: String = chars[s..e].iter().collect();
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("[[") {
            if let Some(name) = rest.split("]]").next() {
                headers.push((toml_dotted(name), true));
            }
        } else if let Some(rest) = t.strip_prefix('[')
            && let Some(name) = rest.split(']').next()
        {
            headers.push((toml_dotted(name), false));
        }
        i = e + 1;
    }
    match headers.last() {
        None => Vec::new(),
        Some((name, false)) => name.clone(),
        Some((name, true)) => {
            let n = headers.iter().filter(|(h, a)| *a && h == name).count();
            let mut p = name.clone();
            p.push((n.saturating_sub(1)).to_string());
            p
        }
    }
}

fn toml_ctx(chars: &[char], at: usize) -> Ctx {
    let at = at.min(chars.len());
    let (ls, le) = line_bounds(chars, at);
    let line = &chars[ls..le];
    let col = at - ls;
    let mut ccol = line
        .iter()
        .take_while(|c| **c == ' ' || **c == '\t')
        .count();
    if col < ccol {
        ccol = col;
    }
    let base = |path: Vec<String>, slot, replace, prefix: String, separated, alone| Ctx {
        path,
        slot,
        replace,
        prefix,
        separated,
        indent: String::new(),
        comma_after: false,
        comma_before: None,
        alone_on_line: alone,
    };
    if line.get(ccol) == Some(&'[') {
        let mut a = ccol + 1;
        if line.get(a) == Some(&'[') {
            a += 1;
        }
        let b = line[a..]
            .iter()
            .position(|c| *c == ']')
            .map_or(line.len(), |i| a + i);
        let col = col.clamp(a, b);
        return base(
            Vec::new(),
            Slot::Table,
            Range::new(ls + a, ls + b),
            line[a..col].iter().collect(),
            true,
            false,
        );
    }
    let mut path = toml_table(chars, ls);
    // `=` outside quotes
    let mut eq = None;
    let mut quote: Option<char> = None;
    for (i, c) in line.iter().enumerate().skip(ccol) {
        match quote {
            Some(q) => {
                if *c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(*c),
                '=' => {
                    eq = Some(i);
                    break;
                }
                '#' => break,
                _ => {}
            },
        }
    }
    match eq {
        Some(p) if col > p => {
            let key: String = line[ccol..p].iter().collect();
            path.extend(toml_dotted(&key));
            let mut vs = p + 1;
            while vs < line.len() && line[vs] == ' ' {
                vs += 1;
            }
            let vs = vs.min(col.max(p + 1));
            let ve = value_end(line, vs).max(col);
            let typed: String = line[vs..col].iter().collect();
            base(
                path,
                Slot::Value,
                Range::new(ls + vs, ls + ve),
                typed.trim_start_matches(['"', '\'']).to_string(),
                true,
                false,
            )
        }
        _ => {
            let key_end = match eq {
                Some(p) => {
                    let mut e = p;
                    while e > ccol && line[e - 1] == ' ' {
                        e -= 1;
                    }
                    e
                }
                None => value_end(line, ccol).max(col),
            };
            let col = col.clamp(ccol, key_end);
            let typed: String = line[ccol..col].iter().collect();
            let mut parts = toml_dotted(&typed);
            let prefix = parts.pop().unwrap_or_default();
            let seg_start = col - prefix.chars().count();
            path.extend(parts);
            let alone = eq.is_none() && key_end == value_end(line, ccol).max(col);
            base(
                path,
                Slot::Key,
                Range::new(ls + seg_start.max(ccol), ls + key_end),
                prefix,
                eq.is_some(),
                alone,
            )
        }
    }
}

fn context(chars: &[char], language: Language, at: usize) -> Option<Ctx> {
    Some(match language {
        Language::Json => json_ctx(chars, at),
        Language::Yaml => yaml_ctx(chars, at),
        Language::Toml => toml_ctx(chars, at),
        _ => return None,
    })
}

// ---- what the document says so far -----------------------------------------------------------

/// JSON as far as it can be read: a missing value is `null`, a trailing comma and an
/// unclosed string, object or array are accepted.
fn json_lenient(chars: &[char]) -> Value {
    struct P<'a> {
        c: &'a [char],
        i: usize,
        depth: usize,
    }
    impl P<'_> {
        fn ws(&mut self) {
            while self.i < self.c.len() && self.c[self.i].is_whitespace() {
                self.i += 1;
            }
        }
        fn string(&mut self) -> String {
            let mut s = String::new();
            self.i += 1;
            while self.i < self.c.len() {
                match self.c[self.i] {
                    '\\' => {
                        if let Some(c) = self.c.get(self.i + 1) {
                            s.push(match c {
                                'n' => '\n',
                                't' => '\t',
                                other => *other,
                            });
                        }
                        self.i += 2;
                    }
                    '"' => {
                        self.i += 1;
                        break;
                    }
                    '\n' => break,
                    c => {
                        s.push(c);
                        self.i += 1;
                    }
                }
            }
            s
        }
        fn value(&mut self) -> Value {
            self.ws();
            if self.depth > 64 {
                self.i = self.c.len();
                return Value::Null;
            }
            match self.c.get(self.i) {
                Some('{') => {
                    self.i += 1;
                    self.depth += 1;
                    let mut m = Map::new();
                    loop {
                        self.ws();
                        match self.c.get(self.i) {
                            None => break,
                            Some('}') => {
                                self.i += 1;
                                break;
                            }
                            Some(',') => self.i += 1,
                            Some('"') => {
                                let k = self.string();
                                self.ws();
                                let v = if self.c.get(self.i) == Some(&':') {
                                    self.i += 1;
                                    self.value()
                                } else {
                                    Value::Null
                                };
                                m.insert(k, v);
                            }
                            Some(']') => break,
                            Some(_) => {
                                // a bare word where a key should be: skip it
                                let _ = self.value();
                            }
                        }
                    }
                    self.depth -= 1;
                    Value::Object(m)
                }
                Some('[') => {
                    self.i += 1;
                    self.depth += 1;
                    let mut a = Vec::new();
                    loop {
                        self.ws();
                        match self.c.get(self.i) {
                            None => break,
                            Some(']') => {
                                self.i += 1;
                                break;
                            }
                            Some(',') => self.i += 1,
                            Some('}') => break,
                            Some(_) => a.push(self.value()),
                        }
                    }
                    self.depth -= 1;
                    Value::Array(a)
                }
                Some('"') => Value::String(self.string()),
                Some(_) => {
                    let start = self.i;
                    while self.i < self.c.len()
                        && !self.c[self.i].is_whitespace()
                        && !matches!(self.c[self.i], ',' | ':' | '{' | '}' | '[' | ']' | '"')
                    {
                        self.i += 1;
                    }
                    if self.i == start {
                        // a stray `:` or closing bracket
                        self.i += 1;
                        return Value::Null;
                    }
                    let word: String = self.c[start..self.i].iter().collect();
                    serde_json::from_str(&word).unwrap_or(Value::Null)
                }
                None => Value::Null,
            }
        }
    }
    P {
        c: chars,
        i: 0,
        depth: 0,
    }
    .value()
}

/// The document as a value, as far as it can be read while the line of the caret is
/// being typed.
fn instance(text: &str, chars: &[char], language: Language, at: usize) -> Option<Value> {
    if language == Language::Json {
        return Some(json_lenient(chars));
    }
    if let Ok(l) = schema::parse(text, language) {
        return Some(l.value);
    }
    // without what is being typed on the caret's line
    let (ls, le) = line_bounds(chars, at);
    let line = &chars[ls..le];
    let keep = match language {
        Language::Yaml => {
            let (dashes, c) = yaml_lead(line);
            if dashes.is_empty() { 0 } else { c }
        }
        _ => 0,
    };
    let mut s: String = chars[..ls + keep].iter().collect();
    s.extend(chars[le..].iter());
    schema::parse(&s, language).ok().map(|l| l.value)
}

// ---- completion ----------------------------------------------------------------------------

fn rank(label: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(1);
    }
    let l = label.to_lowercase();
    let p = prefix.to_lowercase();
    if l.starts_with(&p) {
        Some(0)
    } else if l.contains(&p) {
        Some(2)
    } else {
        None
    }
}

fn key_text(name: &str, language: Language) -> String {
    match language {
        Language::Json => Value::String(name.to_string()).to_string(),
        Language::Yaml if yaml_plain_ok(name) => name.to_string(),
        Language::Toml
            if name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && !name.is_empty() =>
        {
            name.to_string()
        }
        _ => Value::String(name.to_string()).to_string(),
    }
}

fn separator(language: Language) -> &'static str {
    match language {
        Language::Toml => " = ",
        _ => ": ",
    }
}

/// `key<sep>value` for a member the schema describes, and the selection inside it.
fn member(name: &str, i: &Info, language: Language) -> (String, (usize, usize)) {
    let key = key_text(name, language);
    let (mut value, (a, b)) = skeleton(i, language);
    // a YAML mapping or sequence is written on the lines below its key
    let mut sep = separator(language);
    let no_default = i.default.as_ref().is_none_or(Value::is_null);
    if language == Language::Yaml && matches!(value.as_str(), "{}" | "[]") && no_default {
        value.clear();
        sep = ":";
    }
    let head = key.chars().count() + sep.chars().count();
    let sel = if value.is_empty() {
        (head, head)
    } else {
        (head + a, head + b)
    };
    (format!("{key}{sep}{value}"), sel)
}

fn schema_completions(
    entry: &SchemaEntry,
    text: &str,
    chars: &[char],
    language: Language,
    at: usize,
) -> Vec<Completion> {
    let Some(ctx) = context(chars, language, at) else {
        return Vec::new();
    };
    let nav = Nav {
        root: &entry.schema,
    };
    let inst = instance(text, chars, language, at);
    let mut out: Vec<(u8, bool, Completion)> = Vec::new();
    match ctx.slot {
        Slot::Key => {
            let nodes = nav.at(&ctx.path, inst.as_ref());
            let here = inst.as_ref().and_then(|v| value_at(v, &ctx.path));
            let has = |name: &str| {
                name != ctx.prefix
                    && here
                        .and_then(Value::as_object)
                        .is_some_and(|m| m.contains_key(name))
            };
            let props = properties(&nav, &nodes);
            let mut missing: Vec<(String, Info)> = Vec::new();
            let order: Vec<&str> = nodes
                .iter()
                .filter_map(|n| n.get("required").and_then(Value::as_array))
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            for (name, schemas, required) in &props {
                if has(name) {
                    continue;
                }
                let i = info(schemas);
                if *required {
                    missing.push((name.clone(), i.clone()));
                }
                let Some(r) = rank(name, &ctx.prefix) else {
                    continue;
                };
                // a table of its own in TOML when the line holds nothing else
                let table = language == Language::Toml && i.is("object") && i.default.is_none();
                let (insert, select) = if ctx.separated {
                    let k = key_text(name, language);
                    let n = k.chars().count();
                    (k, (n, n))
                } else if table {
                    if !ctx.alone_on_line {
                        continue;
                    }
                    let mut full = ctx.path.clone();
                    full.push(name.clone());
                    let head = format!(
                        "[{}]\n",
                        full.iter()
                            .map(|s| key_text(s, language))
                            .collect::<Vec<_>>()
                            .join(".")
                    );
                    let n = head.chars().count();
                    (head, (n, n))
                } else {
                    member(name, &i, language)
                };
                let mut c = Completion {
                    label: name.clone(),
                    kind: CompletionKind::Key,
                    detail: detail(&i, *required),
                    documentation: i.description.clone(),
                    replace: ctx.replace,
                    insert,
                    select,
                    also: Vec::new(),
                };
                if table && !ctx.separated {
                    // the header replaces the line's key, wherever the dotted prefix began
                    let (ls, _) = line_bounds(chars, ctx.replace.start);
                    c.replace = Range::new(ls, ctx.replace.end);
                }
                json_commas(&mut c, &ctx, language);
                out.push((r, !*required, c));
            }
            missing.sort_by_key(|(n, _)| order.iter().position(|o| o == n));
            // the required keys that are missing, in one step
            if missing.len() > 1 && ctx.prefix.is_empty() && !ctx.separated {
                let join = match language {
                    Language::Json => format!(",\n{}", ctx.indent),
                    _ => format!("\n{}", ctx.indent),
                };
                let mut insert = String::new();
                let mut select = (0, 0);
                let mut ok = true;
                for (n, (name, i)) in missing.iter().enumerate() {
                    if language == Language::Toml && i.is("object") {
                        ok = false;
                        break;
                    }
                    let (m, sel) = member(name, i, language);
                    if n == 0 {
                        select = sel;
                    } else {
                        insert.push_str(&join);
                    }
                    insert.push_str(&m);
                }
                if ok {
                    let names: Vec<&str> = missing.iter().map(|(n, _)| n.as_str()).collect();
                    let mut c = Completion {
                        label: "Required keys".into(),
                        kind: CompletionKind::Snippet,
                        detail: names.join(", "),
                        documentation: "Adds every required key that is missing here.".into(),
                        replace: ctx.replace,
                        insert,
                        select,
                        also: Vec::new(),
                    };
                    json_commas(&mut c, &ctx, language);
                    out.push((0, false, c));
                }
            }
        }
        Slot::Value => {
            let nodes = nav.at(&ctx.path, inst.as_ref());
            let i = info(&nodes);
            let mut values: Vec<(Value, &str)> = i.values.iter().map(|v| (v.clone(), "")).collect();
            if i.types.iter().any(|t| t == "boolean") && values.is_empty() {
                values.push((Value::Bool(true), ""));
                values.push((Value::Bool(false), ""));
            }
            if let Some(d) = &i.default
                && !d.is_null()
                && !d.is_object()
                && !d.is_array()
            {
                match values.iter_mut().find(|(v, _)| v == d) {
                    Some(v) => v.1 = "default",
                    None => values.push((d.clone(), "default")),
                }
            }
            for (v, note) in values {
                let label = match &v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let Some(r) = rank(&label, &ctx.prefix) else {
                    continue;
                };
                let insert = literal(&v, language);
                let n = insert.chars().count();
                out.push((
                    r,
                    note != "default",
                    Completion {
                        label,
                        kind: CompletionKind::Value,
                        detail: note.to_string(),
                        documentation: i.description.clone(),
                        replace: ctx.replace,
                        insert,
                        select: (n, n),
                        also: Vec::new(),
                    },
                ));
            }
        }
        Slot::Table => {
            // tables the schema describes, by their dotted path
            let mut found: Vec<(String, Info)> = Vec::new();
            let mut nodes = Vec::new();
            nav.expand(nav.root, None, &mut nodes, 0);
            tables(&nav, &nodes, String::new(), 0, &mut found);
            for (path, i) in found {
                let Some(r) = rank(&path, &ctx.prefix) else {
                    continue;
                };
                let n = path.chars().count();
                out.push((
                    r,
                    false,
                    Completion {
                        label: path.clone(),
                        kind: CompletionKind::Key,
                        detail: "table".into(),
                        documentation: i.description,
                        replace: ctx.replace,
                        insert: path,
                        select: (n, n),
                        also: Vec::new(),
                    },
                ));
            }
        }
    }
    out.sort_by_key(|a| (a.0, a.1));
    out.into_iter().map(|(_, _, c)| c).take(MAX_ITEMS).collect()
}

fn tables<'a>(
    nav: &Nav<'a>,
    nodes: &[&'a Value],
    prefix: String,
    depth: u8,
    out: &mut Vec<(String, Info)>,
) {
    if depth > 3 {
        return;
    }
    for (name, schemas, _) in properties(nav, nodes) {
        let i = info(&schemas);
        if !i.is("object") {
            continue;
        }
        let path = if prefix.is_empty() {
            key_text(&name, Language::Toml)
        } else {
            format!("{prefix}.{}", key_text(&name, Language::Toml))
        };
        out.push((path.clone(), i));
        tables(nav, &schemas, path, depth + 1, out);
    }
}

fn detail(i: &Info, required: bool) -> String {
    let t = i.type_name();
    match (t.is_empty(), required) {
        (true, true) => "required".into(),
        (true, false) => String::new(),
        (false, true) => format!("{t} · required"),
        (false, false) => t,
    }
}

/// JSON members are separated by commas: one after the inserted member when another
/// follows, one after the member before when it has none.
fn json_commas(c: &mut Completion, ctx: &Ctx, language: Language) {
    if language != Language::Json {
        return;
    }
    if ctx.comma_after && !ctx.separated {
        c.insert.push(',');
    }
    if let Some(at) = ctx.comma_before {
        c.also.push((Range::new(at, at), ",".into()));
    }
}

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "self", "print", "range", "len", "dict", "list", "str", "int", "float",
];

/// Words of the document that start with (or contain) what is typed before `at`,
/// nearest first.
fn word_completions(chars: &[char], language: Language, at: usize) -> Vec<Completion> {
    let at = at.min(chars.len());
    let mut a = at;
    while a > 0 && is_word(chars[a - 1]) {
        a -= 1;
    }
    let mut b = at;
    while b < chars.len() && is_word(chars[b]) {
        b += 1;
    }
    let prefix: String = chars[a..at].iter().collect();
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_word(chars[i]) {
            i += 1;
            continue;
        }
        let s = i;
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        if s == a || i - s < 3 || chars[s].is_numeric() {
            continue;
        }
        let w: String = chars[s..i].iter().collect();
        let dist = s.abs_diff(a);
        match index.get(&w) {
            Some(&k) => seen[k].1 = seen[k].1.min(dist),
            None => {
                index.insert(w.clone(), seen.len());
                seen.push((w, dist));
            }
        }
    }
    if language == Language::Python {
        for k in PYTHON_KEYWORDS {
            if !index.contains_key(*k) {
                seen.push((k.to_string(), usize::MAX));
            }
        }
    }
    let mut ranked: Vec<(u8, usize, String)> = seen
        .into_iter()
        .filter(|(w, _)| *w != prefix)
        .filter_map(|(w, d)| rank(&w, &prefix).map(|r| (r, d, w)))
        .collect();
    ranked.sort();
    ranked
        .into_iter()
        .take(MAX_ITEMS)
        .map(|(_, _, w)| {
            let n = w.chars().count();
            Completion {
                label: w.clone(),
                kind: CompletionKind::Word,
                detail: String::new(),
                documentation: String::new(),
                replace: Range::new(a, b),
                insert: w,
                select: (n, n),
                also: Vec::new(),
            }
        })
        .collect()
}

/// What can be written at char offset `at`. With a schema: the keys valid there, the
/// values it allows, the required keys in one step. Otherwise words from the file, and
/// the list says so.
pub fn completions(
    entry: Option<&SchemaEntry>,
    text: &str,
    language: Language,
    at: usize,
) -> CompletionList {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > ASSIST_BUDGET_CHARS {
        return CompletionList::default();
    }
    let structured = matches!(language, Language::Json | Language::Yaml | Language::Toml);
    if let (Some(entry), true) = (entry, structured) {
        let items = schema_completions(entry, text, &chars, language, at);
        if !items.is_empty() {
            return CompletionList { items, note: None };
        }
    }
    let items = word_completions(&chars, language, at);
    let note = if items.is_empty() {
        None
    } else if language == Language::Python {
        Some(NOTE_WORDS_PYTHON)
    } else if structured && entry.is_none() {
        Some(NOTE_WORDS_NO_SCHEMA)
    } else {
        Some(NOTE_WORDS)
    };
    CompletionList { items, note }
}

// ---- hover -----------------------------------------------------------------------------------

fn pointer_path(pointer: &str) -> Vec<String> {
    pointer
        .split('/')
        .skip(1)
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect()
}

fn dotted(path: &[String]) -> String {
    let mut out = String::new();
    for seg in path {
        if seg.parse::<usize>().is_ok() {
            out.push_str(&format!("[{seg}]"));
        } else {
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(seg);
        }
    }
    out
}

/// The innermost located value (or key) that contains `at`.
fn pointer_at(located: &Located, at: usize) -> Option<(String, Range)> {
    located
        .spans
        .iter()
        .filter(|(p, r)| !p.is_empty() && r.start <= at && at <= r.end)
        .min_by_key(|(p, r)| (r.len(), usize::MAX - p.len()))
        .map(|(p, r)| (p.trim_end_matches("#key").to_string(), *r))
}

/// What the schema says about the key or value at char offset `at`.
pub fn hover(entry: &SchemaEntry, text: &str, language: Language, at: usize) -> Option<Hover> {
    if !matches!(language, Language::Json | Language::Yaml | Language::Toml) {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > ASSIST_BUDGET_CHARS {
        return None;
    }
    let nav = Nav {
        root: &entry.schema,
    };
    let (path, range, inst) = match schema::parse(text, language) {
        Ok(located) => {
            let (pointer, range) = pointer_at(&located, at)?;
            (pointer_path(&pointer), range, Some(located.value))
        }
        Err(_) => {
            // the text does not parse: read the place from the text around it
            let mut end = at.min(chars.len());
            while end < chars.len() && is_word(chars[end]) {
                end += 1;
            }
            let ctx = context(&chars, language, end)?;
            let mut path = ctx.path.clone();
            match ctx.slot {
                Slot::Key if !ctx.prefix.is_empty() => path.push(ctx.prefix.clone()),
                Slot::Value => {}
                _ => return None,
            }
            (path, ctx.replace, instance(text, &chars, language, at))
        }
    };
    if path.is_empty() {
        return None;
    }
    let nodes = nav.at(&path, inst.as_ref());
    if nodes.is_empty() {
        return None;
    }
    let i = info(&nodes);
    let parent = nav.at(&path[..path.len() - 1], inst.as_ref());
    let key = &path[path.len() - 1];
    let required = parent.iter().any(|n| {
        n.get("required")
            .and_then(Value::as_array)
            .is_some_and(|r| r.iter().any(|x| x.as_str() == Some(key)))
    });
    if i.description.is_empty() && i.type_name().is_empty() && i.title.is_empty() {
        return None;
    }
    Some(Hover {
        range,
        path: dotted(&path),
        type_name: i.type_name(),
        title: i.title.clone(),
        description: i.description.clone(),
        default: i.default.as_ref().map(|d| literal(d, language)),
        values: i.values.iter().map(|v| literal(v, language)).collect(),
        required,
    })
}

// ---- quick fixes -----------------------------------------------------------------------------

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(!ca.eq_ignore_ascii_case(cb));
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Start of the first key of the object at `pointer`.
fn first_key(located: &Located, pointer: &str) -> Option<usize> {
    let head = format!("{pointer}/");
    located
        .spans
        .iter()
        .filter(|(p, _)| {
            p.strip_prefix(&head)
                .and_then(|rest| rest.strip_suffix("#key"))
                .is_some_and(|k| !k.contains('/'))
        })
        .map(|(_, r)| r.start)
        .min()
}

fn add_key_edit(
    located: &Located,
    chars: &[char],
    language: Language,
    pointer: &str,
    name: &str,
    i: &Info,
) -> Option<(Range, String)> {
    let (member_text, _) = {
        let key = key_text(name, language);
        let (value, _) = skeleton(i, language);
        let value = if value.is_empty() {
            match language {
                Language::Json => "null".to_string(),
                _ => "\"\"".to_string(),
            }
        } else {
            value
        };
        (format!("{key}{}{value}", separator(language)), ())
    };
    let first = first_key(located, pointer);
    match language {
        Language::Json => {
            let span = located.spans.get(pointer)?;
            match first {
                Some(at) => {
                    let (ls, _) = line_bounds(chars, at);
                    let lead = &chars[ls..at];
                    if lead.iter().all(|c| c.is_whitespace()) {
                        let indent: String = lead.iter().collect();
                        Some((Range::new(at, at), format!("{member_text},\n{indent}")))
                    } else {
                        Some((Range::new(at, at), format!("{member_text}, ")))
                    }
                }
                None => {
                    // an empty object: `{}` or `{ }`
                    let (ls, _) = line_bounds(chars, span.start);
                    let indent: String = chars[ls..span.start]
                        .iter()
                        .take_while(|c| c.is_whitespace())
                        .collect();
                    if span.len() < 2 {
                        return None;
                    }
                    Some((
                        Range::new(span.start + 1, span.end - 1),
                        format!("\n{indent}  {member_text}\n{indent}"),
                    ))
                }
            }
        }
        Language::Yaml => match first {
            Some(at) => {
                let (ls, _) = line_bounds(chars, at);
                Some((
                    Range::new(at, at),
                    format!("{member_text}\n{}", " ".repeat(at - ls)),
                ))
            }
            None if pointer.is_empty() && chars.iter().all(|c| c.is_whitespace()) => {
                Some((Range::new(0, 0), format!("{member_text}\n")))
            }
            None => None,
        },
        Language::Toml => {
            let path = pointer_path(pointer);
            let header = path
                .iter()
                .map(|s| key_text(s, language))
                .collect::<Vec<_>>()
                .join(".");
            let end = chars.len();
            let nl = if chars.last().is_none_or(|c| *c == '\n') {
                ""
            } else {
                "\n"
            };
            if i.is("object") && i.default.is_none() {
                let full = if header.is_empty() {
                    key_text(name, language)
                } else {
                    format!("{header}.{}", key_text(name, language))
                };
                return Some((Range::new(end, end), format!("{nl}\n[{full}]\n")));
            }
            if path.is_empty() {
                // root keys go before the first table; after the comment that opens the file
                let mut at = 0;
                while at < chars.len() {
                    let (s, e) = line_bounds(chars, at);
                    let l: String = chars[s..e].iter().collect();
                    if l.trim_start().starts_with('#') {
                        at = (e + 1).min(chars.len());
                        if e == chars.len() {
                            return Some((Range::new(e, e), format!("\n{member_text}\n")));
                        }
                    } else {
                        break;
                    }
                }
                return Some((Range::new(at, at), format!("{member_text}\n")));
            }
            // after the table's header line
            let mut at = 0;
            while at < chars.len() {
                let (s, e) = line_bounds(chars, at);
                let l: String = chars[s..e].iter().collect();
                let t = l.trim();
                if let Some(rest) = t.strip_prefix('[')
                    && !t.starts_with("[[")
                    && let Some(n) = rest.split(']').next()
                    && toml_dotted(n) == path
                {
                    let after = (e + 1).min(chars.len());
                    let lead = if e == chars.len() { "\n" } else { "" };
                    return Some((Range::new(after, after), format!("{lead}{member_text}\n")));
                }
                at = e + 1;
            }
            Some((
                Range::new(end, end),
                format!("{nl}\n[{header}]\n{member_text}\n"),
            ))
        }
        _ => None,
    }
}

/// Fixes for the problems the schema finds in `text`: a required key that is missing
/// is added with its default, a value that is not one of the allowed ones is replaced
/// by the nearest allowed ones.
pub fn quick_fixes(entry: &SchemaEntry, text: &str, language: Language) -> Vec<QuickFix> {
    use jsonschema::error::ValidationErrorKind as K;
    let Ok(located) = schema::parse(text, language) else {
        return Vec::new();
    };
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > ASSIST_BUDGET_CHARS {
        return Vec::new();
    }
    let nav = Nav {
        root: &entry.schema,
    };
    let mut out = Vec::new();
    for e in entry.validator.iter_errors(&located.value) {
        let pointer = e.instance_path().as_str().to_string();
        let range = located.span_for(&pointer);
        let path = pointer_path(&pointer);
        match e.kind() {
            K::Required { property } => {
                let Some(name) = property.as_str() else {
                    continue;
                };
                let mut child = path.clone();
                child.push(name.to_string());
                let i = info(&nav.at(&child, Some(&located.value)));
                if let Some(edit) = add_key_edit(&located, &chars, language, &pointer, name, &i) {
                    out.push(QuickFix {
                        title: format!("Add \"{name}\""),
                        range,
                        pointer: pointer.clone(),
                        edits: vec![edit],
                    });
                }
            }
            K::Enum { options } => {
                let Some(options) = options.as_array() else {
                    continue;
                };
                let Some(span) = located.spans.get(&pointer) else {
                    continue;
                };
                let current = match value_at(&located.value, &path) {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                let mut ranked: Vec<(usize, &Value)> = options
                    .iter()
                    .filter(|o| !o.is_null())
                    .map(|o| {
                        let s = match o {
                            Value::String(s) => s.clone(),
                            v => v.to_string(),
                        };
                        (edit_distance(&current, &s), o)
                    })
                    .collect();
                ranked.sort_by_key(|(d, _)| *d);
                for (_, o) in ranked.into_iter().take(3) {
                    let lit = literal(o, language);
                    out.push(QuickFix {
                        title: format!("Replace with {lit}"),
                        range,
                        pointer: pointer.clone(),
                        edits: vec![(*span, lit)],
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::FileKind;
    use crate::schema::SchemaRegistry;

    const SCHEMA: &str = r##"{
        "type": "object",
        "required": ["name", "mode"],
        "properties": {
            "name": {"type": "string", "description": "What it is called."},
            "mode": {"type": "string", "enum": ["fast", "exact", "off"], "default": "fast", "description": "How it runs."},
            "count": {"type": "integer", "default": 3, "description": "How many."},
            "verbose": {"type": "boolean"},
            "limits": {"$ref": "#/$defs/Limits", "description": "Bounds."},
            "steps": {"type": "array", "items": {"$ref": "#/$defs/Step"}}
        },
        "$defs": {
            "Limits": {"type": "object", "properties": {"max": {"type": "number", "description": "Upper bound."}, "min": {"type": "number"}}, "required": ["max"]},
            "Step": {
                "type": "object", "required": ["op"],
                "properties": {"op": {"type": "string", "enum": ["cut", "join"]}, "params": {"type": "object"}},
                "allOf": [
                    {"if": {"properties": {"op": {"const": "cut"}}, "required": ["op"]}, "then": {"properties": {"params": {"type": "object", "properties": {"at": {"type": "number", "description": "Where to cut."}}}}}},
                    {"if": {"properties": {"op": {"const": "join"}}, "required": ["op"]}, "then": {"properties": {"params": {"type": "object", "properties": {"with": {"type": "string"}}}}}}
                ]
            }
        }
    }"##;

    fn entry() -> std::sync::Arc<SchemaEntry> {
        let mut r = SchemaRegistry::new();
        r.register("t", SCHEMA, &[FileKind::Json], &[])
            .expect("schema");
        r.get("t").expect("entry")
    }

    /// Completions at the `|` of `text`.
    fn at(text: &str, language: Language) -> (Vec<Completion>, String, usize) {
        let pos = text.chars().position(|c| c == '|').expect("caret");
        let clean: String = text.chars().filter(|c| *c != '|').collect();
        let e = entry();
        (
            completions(Some(&e), &clean, language, pos).items,
            clean,
            pos,
        )
    }

    fn labels(items: &[Completion]) -> Vec<&str> {
        items.iter().map(|c| c.label.as_str()).collect()
    }

    fn apply(text: &str, c: &Completion) -> String {
        let mut edits: Vec<(Range, String)> = c.also.clone();
        edits.push((c.replace, c.insert.clone()));
        edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
        let mut chars: Vec<char> = text.chars().collect();
        for (r, s) in edits {
            chars.splice(r.start..r.end, s.chars());
        }
        chars.into_iter().collect()
    }

    #[test]
    fn yaml_keys_at_the_root_exclude_the_ones_present() {
        let (items, text, _) = at("name: a\n|\n", Language::Yaml);
        let l = labels(&items);
        assert!(!l.contains(&"name"));
        assert_eq!(l[0], "mode", "the required key comes first: {l:?}");
        assert!(l.contains(&"count") && l.contains(&"limits"));
        let mode = &items[0];
        assert_eq!(mode.detail, "string · required");
        assert_eq!(mode.documentation, "How it runs.");
        assert_eq!(apply(&text, mode), "name: a\nmode: fast\n");
        let limits = items.iter().find(|c| c.label == "limits").expect("limits");
        assert_eq!(apply(&text, limits), "name: a\nlimits:\n");
    }

    #[test]
    fn yaml_keys_follow_what_is_typed() {
        let (items, text, _) = at("name: a\nco|\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["count"]);
        assert_eq!(apply(&text, &items[0]), "name: a\ncount: 3\n");
        assert_eq!(items[0].select, (7, 8), "the default is selected");
    }

    #[test]
    fn yaml_nested_keys_and_values() {
        let (items, _, _) = at("name: a\nlimits:\n  m|\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["max", "min"]);
        let (items, text, _) = at("name: a\nmode: |\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["fast", "exact", "off"]);
        assert_eq!(items[0].detail, "default");
        assert_eq!(apply(&text, &items[1]), "name: a\nmode: exact\n");
        let (items, text, _) = at("mode: ex|act\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["exact"]);
        assert_eq!(apply(&text, &items[0]), "mode: exact\n");
        let (items, _, _) = at("verbose: |\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["true", "false"]);
    }

    #[test]
    fn yaml_sequence_items_take_the_branch_their_values_choose() {
        let text = "steps:\n  - op: join\n  - op: cut\n    params:\n      |\n";
        let (items, _, _) = at(text, Language::Yaml);
        assert_eq!(labels(&items), vec!["at"]);
        let text = "steps:\n- op: join\n  params:\n    |\n- op: cut\n";
        let (items, _, _) = at(text, Language::Yaml);
        assert_eq!(labels(&items), vec!["with"]);
        // a key next to the first key of an item, and on the dash line itself
        let (items, _, _) = at("steps:\n  - op: cut\n    |\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["params"]);
        let (items, _, _) = at("steps:\n  - |\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["op", "params"]);
        let (items, _, _) = at("steps:\n  - op: |\n", Language::Yaml);
        assert_eq!(labels(&items), vec!["cut", "join"]);
    }

    #[test]
    fn json_keys_values_and_commas() {
        let (items, text, _) = at("{\n  \"name\": \"a\",\n  |\n}", Language::Json);
        assert_eq!(labels(&items)[0], "mode");
        assert_eq!(
            apply(&text, &items[0]),
            "{\n  \"name\": \"a\",\n  \"mode\": \"fast\"\n}"
        );
        // typed inside quotes, with a member after and none before
        let (items, text, _) = at("{\n  \"co|\"\n  \"name\": \"a\"\n}", Language::Json);
        assert_eq!(labels(&items), vec!["count"]);
        assert_eq!(
            apply(&text, &items[0]),
            "{\n  \"count\": 3,\n  \"name\": \"a\"\n}"
        );
        // the member before has no comma yet
        let (items, text, _) = at("{\n  \"name\": \"a\"\n  |\n}", Language::Json);
        let count = items.iter().find(|c| c.label == "count").expect("count");
        assert_eq!(
            apply(&text, count),
            "{\n  \"name\": \"a\",\n  \"count\": 3\n}"
        );
        let (items, text, _) = at("{\"mode\": \"|\"}", Language::Json);
        assert_eq!(labels(&items), vec!["fast", "exact", "off"]);
        assert_eq!(apply(&text, &items[2]), "{\"mode\": \"off\"}");
        let (items, text, _) = at("{\"mode\": |}", Language::Json);
        assert_eq!(apply(&text, &items[1]), "{\"mode\": \"exact\"}");
    }

    #[test]
    fn json_conditional_keys_while_the_text_is_incomplete() {
        let text = "{\"steps\": [{\"op\": \"cut\", \"params\": {\"|";
        let (items, _, _) = at(text, Language::Json);
        assert_eq!(labels(&items), vec!["at"]);
        let text = "{\"steps\": [{\"op\": \"join\"}, {\"op\": \"join\", \"params\": {|}}]}";
        let (items, _, _) = at(text, Language::Json);
        assert_eq!(labels(&items), vec!["with"]);
    }

    #[test]
    fn required_keys_in_one_step() {
        let (items, text, _) = at("|", Language::Yaml);
        assert_eq!(items[0].label, "Required keys");
        assert_eq!(items[0].kind, CompletionKind::Snippet);
        assert_eq!(items[0].detail, "name, mode");
        assert_eq!(apply(&text, &items[0]), "name: \"\"\nmode: fast");
        let (items, text, _) = at("{\n  |\n}", Language::Json);
        assert_eq!(
            apply(&text, &items[0]),
            "{\n  \"name\": \"\",\n  \"mode\": \"fast\"\n}"
        );
        let (items, text, _) = at("|", Language::Toml);
        assert_eq!(apply(&text, &items[0]), "name = \"\"\nmode = \"fast\"");
    }

    #[test]
    fn toml_keys_values_tables_and_headers() {
        let (items, text, _) = at("name = \"a\"\nmo|\n", Language::Toml);
        assert_eq!(labels(&items), vec!["mode"]);
        assert_eq!(apply(&text, &items[0]), "name = \"a\"\nmode = \"fast\"\n");
        let (items, text, _) = at("mode = |\n", Language::Toml);
        assert_eq!(apply(&text, &items[1]), "mode = \"exact\"\n");
        let (items, _, _) = at("[limits]\nm|\n", Language::Toml);
        assert_eq!(labels(&items), vec!["max", "min"]);
        let (items, _, _) = at("limits.m|\n", Language::Toml);
        assert_eq!(labels(&items), vec!["max", "min"]);
        // a table of its own when the line holds nothing else
        let (items, text, _) = at("name = \"a\"\nlim|\n", Language::Toml);
        assert_eq!(apply(&text, &items[0]), "name = \"a\"\n[limits]\n\n");
        let (items, text, _) = at("[li|]\n", Language::Toml);
        assert_eq!(labels(&items), vec!["limits"]);
        assert_eq!(apply(&text, &items[0]), "[limits]\n");
    }

    #[test]
    fn words_when_there_is_no_schema_and_the_list_says_so() {
        let text = "def total_count(items):\n    return len(items)\n\ntot";
        let list = completions(None, text, Language::Python, text.chars().count());
        assert_eq!(list.items[0].label, "total_count");
        assert_eq!(list.note, Some(NOTE_WORDS_PYTHON));
        assert_eq!(list.items[0].kind, CompletionKind::Word);
        let list = completions(None, "imp", Language::Python, 3);
        assert_eq!(labels(&list.items), vec!["import"]);
        let list = completions(None, "alpha: 1\nal", Language::Yaml, 11);
        assert_eq!(list.note, Some(NOTE_WORDS_NO_SCHEMA));
        assert!(
            completions(None, "x = 1\n", Language::Python, 6)
                .items
                .is_empty()
        );
    }

    #[test]
    fn hover_says_what_the_schema_says() {
        let e = entry();
        let text = "name: a\nmode: exact\nlimits:\n  max: 2\n";
        let h = hover(&e, text, Language::Yaml, 9).expect("hover on the key");
        assert_eq!(h.path, "mode");
        assert_eq!(h.type_name, "string");
        assert_eq!(h.description, "How it runs.");
        assert_eq!(h.default.as_deref(), Some("fast"));
        assert_eq!(h.values, vec!["fast", "exact", "\"off\""]);
        assert!(h.required);
        assert_eq!(h.range, Range::new(8, 12));
        let h = hover(&e, text, Language::Yaml, 16).expect("hover on the value");
        assert_eq!(h.path, "mode");
        let h = hover(&e, text, Language::Yaml, 31).expect("nested");
        assert_eq!(h.path, "limits.max");
        assert_eq!(h.description, "Upper bound.");
        assert!(h.required);
        let h = hover(&e, "{\"count\": 4}", Language::Json, 3).expect("json");
        assert_eq!(
            (h.type_name.as_str(), h.default.as_deref()),
            ("integer", Some("3"))
        );
        let h = hover(&e, "[limits]\nmax = 1\n", Language::Toml, 10).expect("toml");
        assert_eq!(h.path, "limits.max");
        assert!(hover(&e, "unknown: 1\n", Language::Yaml, 2).is_none());
        // the text does not parse: the place is read from the text
        let h = hover(&e, "name: a\nmode: [\n", Language::Yaml, 10).expect("broken");
        assert_eq!(h.path, "mode");
        // conditional parts follow the document
        let text = "steps:\n  - op: cut\n    params:\n      at: 1\n";
        let h = hover(&e, text, Language::Yaml, 38).expect("conditional");
        assert_eq!(
            (h.path.as_str(), h.description.as_str()),
            ("steps[0].params.at", "Where to cut.")
        );
    }

    fn fix(text: &str, language: Language, title: &str) -> String {
        let e = entry();
        let fixes = quick_fixes(&e, text, language);
        let f = fixes.iter().find(|f| f.title == title).unwrap_or_else(|| {
            panic!(
                "no fix {title:?} in {:?}",
                fixes.iter().map(|f| &f.title).collect::<Vec<_>>()
            )
        });
        let mut chars: Vec<char> = text.chars().collect();
        for (r, s) in &f.edits {
            chars.splice(r.start..r.end, s.chars());
        }
        let out: String = chars.into_iter().collect();
        assert!(
            schema::parse(&out, language).is_ok(),
            "the fixed text parses: {out:?}"
        );
        out
    }

    #[test]
    fn a_missing_required_key_is_added() {
        assert_eq!(
            fix("name: a\n", Language::Yaml, "Add \"mode\""),
            "mode: fast\nname: a\n"
        );
        assert_eq!(
            fix("{\n  \"name\": \"a\"\n}", Language::Json, "Add \"mode\""),
            "{\n  \"mode\": \"fast\",\n  \"name\": \"a\"\n}"
        );
        assert_eq!(
            fix("{\"name\": \"a\"}", Language::Json, "Add \"mode\""),
            "{\"mode\": \"fast\", \"name\": \"a\"}"
        );
        assert_eq!(
            fix("# settings\nname = \"a\"\n", Language::Toml, "Add \"mode\""),
            "# settings\nmode = \"fast\"\nname = \"a\"\n"
        );
        // in a nested object, a sequence item, a table
        assert_eq!(
            fix(
                "name: a\nmode: off\nlimits:\n  min: 1\n",
                Language::Yaml,
                "Add \"max\""
            ),
            "name: a\nmode: off\nlimits:\n  max: 0\n  min: 1\n"
        );
        assert_eq!(
            fix(
                "name: a\nmode: off\nsteps:\n  - params: {}\n",
                Language::Yaml,
                "Add \"op\""
            ),
            "name: a\nmode: off\nsteps:\n  - op: cut\n    params: {}\n"
        );
        assert_eq!(
            fix(
                "name = \"a\"\nmode = \"off\"\n\n[limits]\nmin = 1\n",
                Language::Toml,
                "Add \"max\""
            ),
            "name = \"a\"\nmode = \"off\"\n\n[limits]\nmax = 0\nmin = 1\n"
        );
        assert_eq!(
            fix(
                "{\"name\": \"a\", \"mode\": \"off\", \"limits\": {}}",
                Language::Json,
                "Add \"max\""
            ),
            "{\"name\": \"a\", \"mode\": \"off\", \"limits\": {\n  \"max\": 0\n}}"
        );
    }

    #[test]
    fn an_unknown_value_is_replaced_by_the_nearest_allowed_one() {
        let e = entry();
        let text = "name: a\nmode: exatc\n";
        let fixes = quick_fixes(&e, text, Language::Yaml);
        let titles: Vec<&str> = fixes.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles[0], "Replace with exact", "nearest first: {titles:?}");
        assert_eq!(fixes[0].range, Range::new(14, 19));
        assert_eq!(
            fix(text, Language::Yaml, "Replace with exact"),
            "name: a\nmode: exact\n"
        );
        assert_eq!(
            fix(
                "{\"name\": \"a\", \"mode\": \"of\"}",
                Language::Json,
                "Replace with \"off\""
            ),
            "{\"name\": \"a\", \"mode\": \"off\"}"
        );
        assert_eq!(
            fix(
                "name = \"a\"\nmode = \"fats\"\n",
                Language::Toml,
                "Replace with \"fast\""
            ),
            "name = \"a\"\nmode = \"fast\"\n"
        );
    }
}
