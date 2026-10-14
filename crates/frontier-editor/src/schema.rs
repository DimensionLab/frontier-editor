// SPDX-License-Identifier: Apache-2.0
//! JSON-Schema validation of JSON, YAML and TOML documents with diagnostics mapped
//! to text ranges, and a registry keyed by file kind and path pattern.
//!
//! Each format is parsed into a `serde_json::Value` together with a map from JSON
//! pointer to char range, so every `jsonschema` error lands on the value it is about.

use crate::decoration::{Diagnostic, Severity};
use crate::document::Range;
use crate::kind::FileKind;
use crate::syntax::Language;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

pub const SOURCE: &str = "schema";

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("schema is not valid JSON: {0}")]
    Json(String),
    #[error("schema is not a valid JSON Schema: {0}")]
    Invalid(String),
    #[error("invalid glob pattern: {0}")]
    Pattern(String),
}

/// Parsed document plus the location of every value, keyed by JSON pointer.
#[derive(Debug, Clone)]
pub struct Located {
    pub value: serde_json::Value,
    pub spans: HashMap<String, Range>,
}

impl Located {
    /// The span for `pointer`, falling back to the nearest ancestor, then the root.
    pub fn span_for(&self, pointer: &str) -> Range {
        let mut p = pointer.to_string();
        loop {
            if let Some(r) = self.spans.get(&p) {
                return *r;
            }
            match p.rfind('/') {
                Some(i) => p.truncate(i),
                None => return self.spans.get("").copied().unwrap_or_default(),
            }
        }
    }
}

/// Outcome of parsing: a located value or a syntax diagnostic.
pub fn parse(text: &str, language: Language) -> Result<Located, Diagnostic> {
    match language {
        Language::Json => json::parse(text),
        Language::Yaml => yaml::parse(text),
        Language::Toml => toml::parse(text),
        _ => Err(Diagnostic::new(
            Range::new(0, 0),
            Severity::Info,
            SOURCE,
            format!("no structured parser for {language:?}"),
        )),
    }
}

pub struct SchemaEntry {
    pub id: String,
    pub kinds: Vec<FileKind>,
    pub patterns: Vec<glob::Pattern>,
    pub validator: jsonschema::Validator,
    pub schema: serde_json::Value,
}

impl fmt::Debug for SchemaEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaEntry")
            .field("id", &self.id)
            .field("kinds", &self.kinds)
            .finish()
    }
}

#[derive(Debug, Default)]
pub struct SchemaRegistry {
    entries: Vec<Arc<SchemaEntry>>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The published Frontier spec schemas, embedded at build time.
    pub fn builtin() -> Self {
        let mut r = Self::new();
        for (id, text, kinds, patterns) in builtin::ALL {
            if let Err(e) = r.register(id, text, kinds, patterns) {
                tracing::warn!(id, error = %e, "builtin schema skipped");
            }
        }
        r
    }

    pub fn register(
        &mut self,
        id: &str,
        schema_json: &str,
        kinds: &[FileKind],
        patterns: &[&str],
    ) -> Result<(), SchemaError> {
        let schema: serde_json::Value =
            serde_json::from_str(schema_json).map_err(|e| SchemaError::Json(e.to_string()))?;
        let validator =
            jsonschema::validator_for(&schema).map_err(|e| SchemaError::Invalid(e.to_string()))?;
        let patterns = patterns
            .iter()
            .map(|p| glob::Pattern::new(p).map_err(|e| SchemaError::Pattern(e.to_string())))
            .collect::<Result<Vec<_>, _>>()?;
        self.entries.retain(|e| e.id != id);
        self.entries.push(Arc::new(SchemaEntry {
            id: id.to_string(),
            kinds: kinds.to_vec(),
            patterns,
            validator,
            schema,
        }));
        Ok(())
    }

    pub fn ids(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.id.as_str()).collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<SchemaEntry>> {
        self.entries.iter().find(|e| e.id == id).cloned()
    }

    /// Path patterns win over kinds, kinds over nothing.
    pub fn resolve(&self, path: &str, kind: FileKind) -> Option<Arc<SchemaEntry>> {
        self.entries
            .iter()
            .find(|e| {
                e.patterns
                    .iter()
                    .any(|p| p.matches(path) || p.matches(path.rsplit('/').next().unwrap_or(path)))
            })
            .or_else(|| self.entries.iter().find(|e| e.kinds.contains(&kind)))
            .cloned()
    }

    /// Validates `text` against `entry`. A syntax error yields one diagnostic.
    pub fn validate(entry: &SchemaEntry, text: &str, language: Language) -> Vec<Diagnostic> {
        let located = match parse(text, language) {
            Ok(l) => l,
            Err(d) => return vec![d],
        };
        let mut out: Vec<Diagnostic> = entry
            .validator
            .iter_errors(&located.value)
            .map(|e| {
                let pointer = e.instance_path().as_str().to_string();
                Diagnostic::new(
                    located.span_for(&pointer),
                    Severity::Error,
                    SOURCE,
                    e.to_string(),
                )
                .with_code(pointer)
            })
            .collect();
        out.sort_by_key(|d| (d.range.start, d.range.end));
        out.dedup_by(|a, b| a.range == b.range && a.message == b.message);
        out
    }
}

/// Builds a char-offset lookup for byte offsets (identity for ASCII).
struct ByteToChar {
    table: Option<Vec<usize>>,
}

impl ByteToChar {
    fn new(text: &str) -> Self {
        if text.is_ascii() {
            return Self { table: None };
        }
        let mut t = Vec::with_capacity(text.len() + 1);
        let mut ci = 0;
        for (bi, c) in text.char_indices() {
            while t.len() < bi {
                t.push(ci);
            }
            t.push(ci);
            ci += 1;
            let _ = c;
        }
        while t.len() <= text.len() {
            t.push(ci);
        }
        Self { table: Some(t) }
    }
    fn get(&self, b: usize) -> usize {
        match &self.table {
            None => b,
            Some(t) => t[b.min(t.len() - 1)],
        }
    }
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

mod json {
    use super::*;
    use serde_json::{Map, Value};

    struct P<'a> {
        s: &'a [u8],
        i: usize,
        spans: HashMap<String, Range>,
        b2c: ByteToChar,
    }

    pub fn parse(text: &str) -> Result<Located, Diagnostic> {
        let mut p = P {
            s: text.as_bytes(),
            i: 0,
            spans: HashMap::new(),
            b2c: ByteToChar::new(text),
        };
        p.ws();
        let value = p.value("")?;
        p.ws();
        if p.i < p.s.len() {
            return Err(p.err("trailing characters"));
        }
        Ok(Located {
            value,
            spans: p.spans,
        })
    }

    impl P<'_> {
        fn err(&self, msg: &str) -> Diagnostic {
            let c = self.b2c.get(self.i);
            Diagnostic::new(
                Range::new(c, c + 1),
                Severity::Error,
                SOURCE,
                format!("JSON syntax: {msg}"),
            )
        }
        fn ws(&mut self) {
            while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            }
        }
        fn peek(&self) -> Option<u8> {
            self.s.get(self.i).copied()
        }
        fn expect(&mut self, b: u8) -> Result<(), Diagnostic> {
            if self.peek() == Some(b) {
                self.i += 1;
                Ok(())
            } else {
                Err(self.err(&format!("expected '{}'", b as char)))
            }
        }
        fn value(&mut self, ptr: &str) -> Result<Value, Diagnostic> {
            let start = self.i;
            let v = match self.peek() {
                Some(b'{') => self.object(ptr)?,
                Some(b'[') => self.array(ptr)?,
                Some(b'"') => Value::String(self.string()?),
                Some(b't') => self.lit("true", Value::Bool(true))?,
                Some(b'f') => self.lit("false", Value::Bool(false))?,
                Some(b'n') => self.lit("null", Value::Null)?,
                Some(b) if b == b'-' || b.is_ascii_digit() => self.number()?,
                Some(_) => return Err(self.err("unexpected character")),
                None => return Err(self.err("unexpected end of input")),
            };
            self.spans.insert(
                ptr.to_string(),
                Range::new(self.b2c.get(start), self.b2c.get(self.i)),
            );
            Ok(v)
        }
        fn lit(&mut self, word: &str, v: Value) -> Result<Value, Diagnostic> {
            if self.s[self.i..].starts_with(word.as_bytes()) {
                self.i += word.len();
                Ok(v)
            } else {
                Err(self.err("invalid literal"))
            }
        }
        fn number(&mut self) -> Result<Value, Diagnostic> {
            let start = self.i;
            while self.i < self.s.len()
                && matches!(
                    self.s[self.i],
                    b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
                )
            {
                self.i += 1;
            }
            let txt =
                std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err("bad number"))?;
            serde_json::from_str::<Value>(txt).map_err(|_| self.err("invalid number"))
        }
        fn string(&mut self) -> Result<String, Diagnostic> {
            let start = self.i;
            self.i += 1;
            while self.i < self.s.len() {
                match self.s[self.i] {
                    b'\\' => self.i += 2,
                    b'"' => {
                        self.i += 1;
                        let raw = std::str::from_utf8(&self.s[start..self.i])
                            .map_err(|_| self.err("bad utf-8"))?;
                        return serde_json::from_str::<String>(raw)
                            .map_err(|_| self.err("invalid string"));
                    }
                    _ => self.i += 1,
                }
            }
            Err(self.err("unterminated string"))
        }
        fn object(&mut self, ptr: &str) -> Result<Value, Diagnostic> {
            self.expect(b'{')?;
            let mut map = Map::new();
            self.ws();
            if self.peek() == Some(b'}') {
                self.i += 1;
                return Ok(Value::Object(map));
            }
            loop {
                self.ws();
                if self.peek() != Some(b'"') {
                    return Err(self.err("expected a string key"));
                }
                let kstart = self.i;
                let key = self.string()?;
                let child = format!("{ptr}/{}", escape_pointer(&key));
                self.spans.insert(
                    format!("{child}#key"),
                    Range::new(self.b2c.get(kstart), self.b2c.get(self.i)),
                );
                self.ws();
                self.expect(b':')?;
                self.ws();
                let v = self.value(&child)?;
                map.insert(key, v);
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {
                        self.i += 1;
                        return Ok(Value::Object(map));
                    }
                    _ => return Err(self.err("expected ',' or '}'")),
                }
            }
        }
        fn array(&mut self, ptr: &str) -> Result<Value, Diagnostic> {
            self.expect(b'[')?;
            let mut items = Vec::new();
            self.ws();
            if self.peek() == Some(b']') {
                self.i += 1;
                return Ok(Value::Array(items));
            }
            loop {
                self.ws();
                let child = format!("{ptr}/{}", items.len());
                items.push(self.value(&child)?);
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b']') => {
                        self.i += 1;
                        return Ok(Value::Array(items));
                    }
                    _ => return Err(self.err("expected ',' or ']'")),
                }
            }
        }
    }
}

mod yaml {
    use super::*;
    use serde_json::{Map, Value};
    use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
    use yaml_rust2::scanner::{Marker, TScalarStyle};

    enum Frame {
        Map {
            ptr: String,
            map: Map<String, Value>,
            key: Option<String>,
            start: usize,
        },
        Seq {
            ptr: String,
            items: Vec<Value>,
            start: usize,
        },
    }

    #[derive(Default)]
    struct R {
        stack: Vec<Frame>,
        root: Option<Value>,
        spans: HashMap<String, Range>,
        anchors: HashMap<usize, Value>,
    }

    impl R {
        fn child_ptr(&self) -> String {
            match self.stack.last() {
                Some(Frame::Map { ptr, key, .. }) => {
                    format!("{ptr}/{}", escape_pointer(key.as_deref().unwrap_or("")))
                }
                Some(Frame::Seq { ptr, items, .. }) => format!("{ptr}/{}", items.len()),
                None => String::new(),
            }
        }
        fn push_value(&mut self, v: Value, aid: usize) {
            if aid != 0 {
                self.anchors.insert(aid, v.clone());
            }
            match self.stack.last_mut() {
                Some(Frame::Map { map, key, .. }) => match key.take() {
                    None => {
                        *key = Some(match &v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    }
                    Some(k) => {
                        map.insert(k, v);
                    }
                },
                Some(Frame::Seq { items, .. }) => items.push(v),
                None => self.root = Some(v),
            }
        }
        fn is_key_position(&self) -> bool {
            matches!(self.stack.last(), Some(Frame::Map { key: None, .. }))
        }
    }

    fn scalar(v: &str, style: TScalarStyle) -> Value {
        if style != TScalarStyle::Plain {
            return Value::String(v.to_string());
        }
        match v {
            "" | "~" | "null" | "Null" | "NULL" => Value::Null,
            "true" | "True" | "TRUE" => Value::Bool(true),
            "false" | "False" | "FALSE" => Value::Bool(false),
            _ => {
                if let Ok(i) = v.parse::<i64>() {
                    return Value::from(i);
                }
                if v.starts_with("0x") || v.starts_with("0o") {
                    return Value::String(v.to_string());
                }
                if let Ok(f) = v.parse::<f64>()
                    && !matches!(v, ".inf" | "-.inf" | ".nan")
                    && v.chars().any(|c| c.is_ascii_digit())
                {
                    return serde_json::Number::from_f64(f)
                        .map(Value::Number)
                        .unwrap_or(Value::String(v.to_string()));
                }
                Value::String(v.to_string())
            }
        }
    }

    impl MarkedEventReceiver for R {
        fn on_event(&mut self, ev: Event, mark: Marker) {
            let at = mark.index();
            match ev {
                Event::Scalar(v, style, aid, _) => {
                    let key_pos = self.is_key_position();
                    let ptr = if key_pos {
                        match self.stack.last() {
                            Some(Frame::Map { ptr, .. }) => {
                                format!("{ptr}/{}#key", escape_pointer(&v))
                            }
                            _ => String::new(),
                        }
                    } else {
                        self.child_ptr()
                    };
                    let len = v.chars().count() + if style == TScalarStyle::Plain { 0 } else { 2 };
                    self.spans.insert(ptr, Range::new(at, at + len.max(1)));
                    if let Some(Frame::Map { start, .. } | Frame::Seq { start, .. }) =
                        self.stack.last_mut()
                        && at < *start
                    {
                        *start = at;
                    }
                    let val = if key_pos {
                        Value::String(v)
                    } else {
                        scalar(&v, style)
                    };
                    self.push_value(val, aid);
                }
                Event::Alias(aid) => {
                    let v = self.anchors.get(&aid).cloned().unwrap_or(Value::Null);
                    let ptr = self.child_ptr();
                    self.spans.insert(ptr, Range::new(at, at + 1));
                    self.push_value(v, 0);
                }
                Event::MappingStart(_, _) => {
                    let ptr = self.child_ptr();
                    self.stack.push(Frame::Map {
                        ptr,
                        map: Map::new(),
                        key: None,
                        start: at,
                    });
                }
                Event::MappingEnd => {
                    if let Some(Frame::Map {
                        ptr, map, start, ..
                    }) = self.stack.pop()
                    {
                        self.spans.insert(ptr, Range::new(start, at.max(start + 1)));
                        self.push_value(Value::Object(map), 0);
                    }
                }
                Event::SequenceStart(_, _) => {
                    let ptr = self.child_ptr();
                    self.stack.push(Frame::Seq {
                        ptr,
                        items: Vec::new(),
                        start: at,
                    });
                }
                Event::SequenceEnd => {
                    if let Some(Frame::Seq { ptr, items, start }) = self.stack.pop() {
                        self.spans.insert(ptr, Range::new(start, at.max(start + 1)));
                        self.push_value(Value::Array(items), 0);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn parse(text: &str) -> Result<Located, Diagnostic> {
        let mut r = R::default();
        let mut parser = Parser::new_from_str(text);
        if let Err(e) = parser.load(&mut r, false) {
            let m = e.marker();
            let at = m.index();
            return Err(Diagnostic::new(
                Range::new(at, at + 1),
                Severity::Error,
                SOURCE,
                format!("YAML syntax: {}", e.info()),
            ));
        }
        let value = r.root.unwrap_or(Value::Null);
        r.spans
            .entry(String::new())
            .or_insert(Range::new(0, text.chars().count()));
        Ok(Located {
            value,
            spans: r.spans,
        })
    }
}

mod toml {
    use super::*;
    use serde_json::{Map, Value};
    use toml_edit::{Document, Item, Table, Value as TV};

    struct W<'a> {
        spans: HashMap<String, Range>,
        b2c: &'a ByteToChar,
    }

    impl W<'_> {
        fn span(&mut self, ptr: &str, s: Option<std::ops::Range<usize>>) {
            if let Some(s) = s {
                self.spans.insert(
                    ptr.to_string(),
                    Range::new(self.b2c.get(s.start), self.b2c.get(s.end)),
                );
            }
        }
        fn table(&mut self, ptr: &str, t: &Table) -> Value {
            let mut map = Map::new();
            for (k, item) in t.iter() {
                let child = format!("{ptr}/{}", escape_pointer(k));
                if let Some((key, _)) = t.get_key_value(k) {
                    self.span(&format!("{child}#key"), key.span());
                }
                map.insert(k.to_string(), self.item(&child, item));
            }
            Value::Object(map)
        }
        fn item(&mut self, ptr: &str, item: &Item) -> Value {
            match item {
                Item::None => Value::Null,
                Item::Value(v) => self.value(ptr, v),
                Item::Table(t) => {
                    self.span(ptr, t.span());
                    self.table(ptr, t)
                }
                Item::ArrayOfTables(a) => {
                    self.span(ptr, a.span());
                    Value::Array(
                        a.iter()
                            .enumerate()
                            .map(|(i, t)| {
                                let child = format!("{ptr}/{i}");
                                self.span(&child, t.span());
                                self.table(&child, t)
                            })
                            .collect(),
                    )
                }
            }
        }
        fn value(&mut self, ptr: &str, v: &TV) -> Value {
            self.span(ptr, v.span());
            match v {
                TV::String(s) => Value::String(s.value().clone()),
                TV::Integer(i) => Value::from(*i.value()),
                TV::Float(f) => serde_json::Number::from_f64(*f.value())
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                TV::Boolean(b) => Value::Bool(*b.value()),
                TV::Datetime(d) => Value::String(d.value().to_string()),
                TV::Array(a) => Value::Array(
                    a.iter()
                        .enumerate()
                        .map(|(i, x)| self.value(&format!("{ptr}/{i}"), x))
                        .collect(),
                ),
                TV::InlineTable(t) => {
                    let mut map = Map::new();
                    for (k, x) in t.iter() {
                        let child = format!("{ptr}/{}", escape_pointer(k));
                        map.insert(k.to_string(), self.value(&child, x));
                    }
                    Value::Object(map)
                }
            }
        }
    }

    pub fn parse(text: &str) -> Result<Located, Diagnostic> {
        let b2c = ByteToChar::new(text);
        let doc: Document<&str> = match Document::parse(text) {
            Ok(d) => d,
            Err(e) => {
                let r = e
                    .span()
                    .map(|s| Range::new(b2c.get(s.start), b2c.get(s.end).max(b2c.get(s.start) + 1)))
                    .unwrap_or_default();
                return Err(Diagnostic::new(
                    r,
                    Severity::Error,
                    SOURCE,
                    format!("TOML syntax: {}", e.message()),
                ));
            }
        };
        let mut w = W {
            spans: HashMap::new(),
            b2c: &b2c,
        };
        let value = w.table("", doc.as_table());
        w.spans
            .entry(String::new())
            .or_insert(Range::new(0, text.chars().count()));
        Ok(Located {
            value,
            spans: w.spans,
        })
    }
}

/// Schemas embedded from `schemas/` next to this crate: a generated copy of the
/// published spec schemas (the monorepo's exporter writes both and a drift test keeps
/// them identical), so the crate builds standalone.
pub mod builtin {
    use crate::kind::FileKind;

    macro_rules! schema {
        ($name:literal) => {
            include_str!(concat!("../schemas/", $name, ".schema.json"))
        };
    }

    pub type Entry = (
        &'static str,
        &'static str,
        &'static [FileKind],
        &'static [&'static str],
    );

    pub const ALL: &[Entry] = &[
        (
            "frontier-train-config",
            schema!("frontier-train-config"),
            &[FileKind::TrainConfig],
            &[],
        ),
        (
            "runspec",
            schema!("runspec"),
            &[FileKind::RunSpec],
            &["runspec.yaml", "runspec.yml", "runspec.json", "*.run.yaml"],
        ),
        (
            "jobspec",
            schema!("jobspec"),
            &[FileKind::JobSpec],
            &["jobspec.yaml", "*.job.yaml", "*.job.json"],
        ),
        (
            "recipe",
            schema!("recipe"),
            &[FileKind::Recipe],
            &["*.recipe.json"],
        ),
        (
            "pipeline-v1",
            schema!("pipeline-v1"),
            &[FileKind::Pipeline],
            &["*.pipeline.json"],
        ),
        (
            "hub-entry",
            schema!("hub-entry"),
            &[FileKind::HubEntry],
            &[],
        ),
        (
            "benchmark-spec",
            schema!("benchmark-spec"),
            &[FileKind::BenchmarkSpec],
            &[],
        ),
        (
            "frontier-config",
            schema!("frontier-config"),
            &[FileKind::Settings],
            &["frontier.toml"],
        ),
        (
            "plugin-manifest",
            schema!("plugin-manifest"),
            &[FileKind::PluginManifest],
            &["plugin.toml"],
        ),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"{
        "type": "object",
        "required": ["name", "count"],
        "properties": {
            "name": {"type": "string"},
            "count": {"type": "integer", "minimum": 1},
            "tags": {"type": "array", "items": {"type": "string"}},
            "nested": {"type": "object", "properties": {"x": {"type": "number"}}}
        },
        "additionalProperties": false
    }"#;

    fn reg() -> SchemaRegistry {
        let mut r = SchemaRegistry::new();
        r.register(
            "t",
            SCHEMA,
            &[FileKind::Yaml, FileKind::Json, FileKind::Toml],
            &["*.t.yaml"],
        )
        .expect("schema");
        r
    }

    #[test]
    fn json_errors_land_on_values() {
        let r = reg();
        let e = r.get("t").expect("entry");
        let text = "{\n  \"name\": 5,\n  \"count\": 0,\n  \"tags\": [\"a\", 2]\n}";
        let d = SchemaRegistry::validate(&e, text, Language::Json);
        let codes: Vec<&str> = d.iter().filter_map(|d| d.code.as_deref()).collect();
        assert_eq!(codes, vec!["/name", "/count", "/tags/1"]);
        assert_eq!(d[0].range, Range::new(12, 13));
        assert_eq!(d[2].range, Range::new(45, 46));
    }

    #[test]
    fn json_syntax_error() {
        let r = reg();
        let e = r.get("t").expect("entry");
        let d = SchemaRegistry::validate(&e, "{\"a\": }", Language::Json);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("JSON syntax"));
        assert_eq!(d[0].range.start, 6);
    }

    #[test]
    fn yaml_errors_and_missing_required() {
        let r = reg();
        let e = r.get("t").expect("entry");
        let text = "name: ahmed\ncount: zero\nnested:\n  x: no\n";
        let d = SchemaRegistry::validate(&e, text, Language::Yaml);
        let codes: Vec<&str> = d.iter().filter_map(|d| d.code.as_deref()).collect();
        assert_eq!(codes, vec!["/count", "/nested/x"]);
        assert_eq!(d[0].range, Range::new(19, 23));
        let d = SchemaRegistry::validate(&e, "name: x\n", Language::Yaml);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("count"));
        assert_eq!(
            d[0].range.start, 0,
            "missing key points at the parent mapping"
        );
    }

    #[test]
    fn yaml_syntax_error() {
        let r = reg();
        let e = r.get("t").expect("entry");
        let d = SchemaRegistry::validate(&e, "a: [1, 2\nb: 3\n", Language::Yaml);
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("YAML syntax"));
    }

    #[test]
    fn toml_errors() {
        let r = reg();
        let e = r.get("t").expect("entry");
        let text = "name = \"a\"\ncount = -1\nextra = true\n[nested]\nx = \"s\"\n";
        let d = SchemaRegistry::validate(&e, text, Language::Toml);
        let codes: Vec<&str> = d.iter().filter_map(|d| d.code.as_deref()).collect();
        assert!(codes.contains(&"/count"));
        assert!(codes.contains(&"/nested/x"));
        let count = d
            .iter()
            .find(|d| d.code.as_deref() == Some("/count"))
            .expect("count");
        assert_eq!(count.range, Range::new(19, 21));
        let d = SchemaRegistry::validate(&e, "a = \n", Language::Toml);
        assert!(d[0].message.contains("TOML syntax"));
    }

    #[test]
    fn resolve_by_pattern_then_kind() {
        let r = reg();
        assert!(r.resolve("cfg/run.t.yaml", FileKind::Log).is_some());
        assert!(r.resolve("x.yaml", FileKind::Yaml).is_some());
        assert!(r.resolve("x.log", FileKind::Log).is_none());
    }

    #[test]
    fn unicode_offsets_in_json() {
        let r = reg();
        let e = r.get("t").expect("entry");
        let text = "{\"name\": \"héllo\", \"count\": \"x\"}";
        let d = SchemaRegistry::validate(&e, text, Language::Json);
        assert_eq!(d[0].range, Range::new(27, 30));
    }
}
