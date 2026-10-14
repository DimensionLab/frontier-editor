// SPDX-License-Identifier: Apache-2.0
//! Search and replace, literal or regex, over the rope.

use crate::document::{Document, Range};
use crate::transaction::{Change, EditKind, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SearchOptions {
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("invalid pattern: {0}")]
    Pattern(String),
}

#[derive(Debug)]
pub struct Search {
    re: regex::Regex,
    multiline: bool,
}

impl Search {
    pub fn new(query: &str, opts: &SearchOptions) -> Result<Self, SearchError> {
        if query.is_empty() {
            return Err(SearchError::Pattern("empty query".into()));
        }
        let mut pat = if opts.regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        if opts.whole_word {
            pat = format!(r"\b(?:{pat})\b");
        }
        let re = regex::RegexBuilder::new(&pat)
            .case_insensitive(!opts.case_sensitive)
            .multi_line(true)
            .size_limit(1 << 24)
            .build()
            .map_err(|e| SearchError::Pattern(e.to_string()))?;
        let multiline =
            query.contains("\\n") || query.contains('\n') || (opts.regex && query.contains("(?s"));
        Ok(Self { re, multiline })
    }

    /// Matches within `lines` (half-open line range), at most `limit`.
    pub fn find_in_lines(&self, doc: &Document, lines: Range, limit: usize) -> Vec<Range> {
        let mut out = Vec::new();
        if self.multiline {
            let text = doc.to_string();
            let start_b = doc.char_to_byte(doc.line_start(lines.start));
            for m in self.re.find_iter(&text) {
                if m.start() < start_b {
                    continue;
                }
                let r = Range::new(doc.byte_to_char(m.start()), doc.byte_to_char(m.end()));
                if doc.offset_to_position(r.start).line >= lines.end {
                    break;
                }
                out.push(r);
                if out.len() >= limit {
                    break;
                }
            }
            return out;
        }
        // One pass over the rope: no per-line offset lookups, one reused buffer.
        let rope = doc.rope();
        let end = lines.end.min(doc.line_count());
        if lines.start >= end {
            return out;
        }
        let mut base = rope.line_to_char(lines.start);
        let mut text = String::new();
        for line in rope.lines_at(lines.start).take(end - lines.start) {
            text.clear();
            for chunk in line.chunks() {
                text.push_str(chunk);
            }
            let line_chars = line.len_chars();
            while text.ends_with(['\n', '\r']) {
                text.pop();
            }
            // byte -> char, advanced match by match (matches come in order)
            let (mut at_b, mut at_c) = (0usize, 0usize);
            for m in self.re.find_iter(&text) {
                if m.is_empty() {
                    continue;
                }
                at_c += text[at_b..m.start()].chars().count();
                let s = at_c;
                at_c += text[m.start()..m.end()].chars().count();
                at_b = m.end();
                out.push(Range::new(base + s, base + at_c));
                if out.len() >= limit {
                    return out;
                }
            }
            base += line_chars;
        }
        out
    }

    pub fn find_all(&self, doc: &Document, limit: usize) -> Vec<Range> {
        self.find_in_lines(doc, Range::new(0, doc.line_count()), limit)
    }

    /// First match at or after `from`, wrapping around.
    pub fn find_next(&self, doc: &Document, from: usize) -> Option<Range> {
        let all = self.find_all(doc, usize::MAX);
        all.iter()
            .find(|r| r.start >= from)
            .or(all.first())
            .copied()
    }

    pub fn find_prev(&self, doc: &Document, before: usize) -> Option<Range> {
        let all = self.find_all(doc, usize::MAX);
        all.iter()
            .rev()
            .find(|r| r.start < before)
            .or(all.last())
            .copied()
    }

    /// Replacement text for one match (`$1` style groups when regex).
    pub fn expand(
        &self,
        doc: &Document,
        m: Range,
        replacement: &str,
        regex_groups: bool,
    ) -> String {
        if !regex_groups {
            return replacement.to_string();
        }
        let hay = doc.slice(m);
        let mut out = String::new();
        if let Some(caps) = self.re.captures(&hay) {
            caps.expand(replacement, &mut out);
            out
        } else {
            replacement.to_string()
        }
    }

    pub fn replace_all(
        &self,
        doc: &Document,
        replacement: &str,
        regex_groups: bool,
    ) -> Transaction {
        let changes = self
            .find_all(doc, usize::MAX)
            .into_iter()
            .map(|m| Change::replace(m, self.expand(doc, m, replacement, regex_groups)))
            .collect();
        Transaction::new(changes, EditKind::Replace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_regex() {
        let doc = Document::from_str("foo bar\nFoo baz\nfoobar\n");
        let s = Search::new("foo", &SearchOptions::default()).expect("ok");
        assert_eq!(s.find_all(&doc, 10).len(), 3);
        let s = Search::new(
            "foo",
            &SearchOptions {
                whole_word: true,
                case_sensitive: true,
                regex: false,
            },
        )
        .expect("ok");
        assert_eq!(s.find_all(&doc, 10), vec![Range::new(0, 3)]);
        let s = Search::new(
            r"ba(\w)",
            &SearchOptions {
                regex: true,
                ..Default::default()
            },
        )
        .expect("ok");
        let mut d = doc.clone();
        s.replace_all(&doc, "B$1", true).apply(&mut d);
        assert_eq!(d.to_string(), "foo Br\nFoo Bz\nfooBr\n");
        assert_eq!(s.find_next(&doc, 100), Some(Range::new(4, 7)));
    }

    #[test]
    fn unicode_offsets() {
        let doc = Document::from_str("héllo wörld");
        let s = Search::new("wörld", &SearchOptions::default()).expect("ok");
        assert_eq!(s.find_all(&doc, 1), vec![Range::new(6, 11)]);
    }
}
