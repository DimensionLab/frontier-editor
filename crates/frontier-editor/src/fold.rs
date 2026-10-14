// SPDX-License-Identifier: Apache-2.0
//! Folding ranges. Indentation-based (YAML, Python, Markdown sections, plain text)
//! with bracket-based ranges for brace languages (JSON, Rust, Java). Both work
//! without a parse tree so results are identical natively and on wasm.

use crate::document::Document;
use crate::syntax::Language;
use serde::{Deserialize, Serialize};

/// Lines `(start, end]` fold away; `start` stays visible as the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct FoldRange {
    pub start_line: usize,
    pub end_line: usize,
}

/// Lines examined past a header before giving up (keeps `fold_range_at` O(1) on
/// multi-million-line files).
pub const MAX_FOLD_SCAN: usize = 4_000;

/// The fold starting at `line`, if any, found by a bounded forward scan.
pub fn fold_range_at(doc: &Document, language: Language, line: usize) -> Option<FoldRange> {
    let n = doc.line_count();
    if line >= n {
        return None;
    }
    let limit = (line + MAX_FOLD_SCAN).min(n);
    match language {
        Language::Json | Language::Rust | Language::Java => bracket_range_at(doc, line, limit),
        Language::Markdown => {
            let text = doc.line(line);
            let lv = text.chars().take_while(|c| *c == '#').count();
            if lv == 0 || !text.chars().nth(lv).is_some_and(|c| c == ' ') {
                return None;
            }
            let mut end = limit - 1;
            for j in line + 1..limit {
                let l2 = doc.line(j);
                let lv2 = l2.chars().take_while(|c| *c == '#').count();
                if lv2 > 0 && lv2 <= lv && l2.chars().nth(lv2).is_some_and(|c| c == ' ') {
                    end = j - 1;
                    break;
                }
            }
            while end > line && doc.line(end).trim().is_empty() {
                end -= 1;
            }
            (end > line).then_some(FoldRange {
                start_line: line,
                end_line: end,
            })
        }
        _ => {
            let ind = indent_of(&doc.line(line))?;
            let mut end = line;
            for j in line + 1..limit {
                match indent_of(&doc.line(j)) {
                    None => {}
                    Some(k) if k > ind => end = j,
                    Some(_) => break,
                }
            }
            (end > line).then_some(FoldRange {
                start_line: line,
                end_line: end,
            })
        }
    }
}

fn bracket_range_at(doc: &Document, line: usize, limit: usize) -> Option<FoldRange> {
    // first unmatched opener on `line`, matched forward
    let mut depth = 0usize;
    let mut opened = false;
    let mut in_str = false;
    let mut escape = false;
    for l in line..limit {
        for c in doc.line(l).chars() {
            if in_str {
                if escape {
                    escape = false;
                } else if c == '\\' {
                    escape = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' | '[' | '(' => {
                    depth += 1;
                    opened = true;
                }
                '}' | ']' | ')' => {
                    if depth > 0 {
                        depth -= 1;
                        if depth == 0 && opened {
                            return (l > line).then_some(FoldRange {
                                start_line: line,
                                end_line: l,
                            });
                        }
                    } else if l == line {
                        // a closer before any opener on this line: not a header
                        opened = false;
                    }
                }
                _ => {}
            }
        }
        if l == line && !opened {
            return None;
        }
        in_str = false;
        escape = false;
    }
    None
}

/// Every fold range in the document (used by *Fold all*; capped for huge files).
pub fn fold_ranges(doc: &Document, language: Language) -> Vec<FoldRange> {
    let mut out = match language {
        Language::Json | Language::Rust | Language::Java => bracket_ranges(doc),
        Language::Markdown => heading_ranges(doc),
        _ => indent_ranges(doc),
    };
    out.sort_by_key(|f| (f.start_line, f.end_line));
    out.dedup();
    out
}

fn indent_of(line: &str) -> Option<usize> {
    if line.trim().is_empty() {
        return None;
    }
    Some(line.chars().take_while(|c| *c == ' ' || *c == '\t').count())
}

fn indent_ranges(doc: &Document) -> Vec<FoldRange> {
    let n = doc.line_count();
    let indents: Vec<Option<usize>> = (0..n).map(|i| indent_of(&doc.line(i))).collect();
    let mut out = Vec::new();
    for i in 0..n {
        let Some(ind) = indents[i] else { continue };
        // find the last following line with greater indentation (skipping blanks)
        let mut end = i;
        let mut j = i + 1;
        while j < n {
            match indents[j] {
                None => {}
                Some(k) if k > ind => end = j,
                Some(_) => break,
            }
            j += 1;
        }
        if end > i {
            out.push(FoldRange {
                start_line: i,
                end_line: end,
            });
        }
    }
    out
}

fn heading_ranges(doc: &Document) -> Vec<FoldRange> {
    let n = doc.line_count();
    let level = |l: &str| l.chars().take_while(|c| *c == '#').count();
    let mut out = Vec::new();
    for i in 0..n {
        let line = doc.line(i);
        let lv = level(&line);
        if lv == 0 || !line.chars().nth(lv).is_some_and(|c| c == ' ') {
            continue;
        }
        let mut end = n - 1;
        for j in i + 1..n {
            let l2 = doc.line(j);
            let lv2 = level(&l2);
            if lv2 > 0 && lv2 <= lv && l2.chars().nth(lv2).is_some_and(|c| c == ' ') {
                end = j - 1;
                break;
            }
        }
        while end > i && doc.line(end).trim().is_empty() {
            end -= 1;
        }
        if end > i {
            out.push(FoldRange {
                start_line: i,
                end_line: end,
            });
        }
    }
    out
}

fn bracket_ranges(doc: &Document) -> Vec<FoldRange> {
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut out = Vec::new();
    let mut in_str: Option<char> = None;
    let mut escape = false;
    for line_no in 0..doc.line_count() {
        for c in doc.line(line_no).chars() {
            if let Some(q) = in_str {
                if escape {
                    escape = false;
                } else if c == '\\' {
                    escape = true;
                } else if c == q {
                    in_str = None;
                }
                continue;
            }
            match c {
                '"' => in_str = Some('"'),
                '{' | '[' | '(' => stack.push((c, line_no)),
                '}' | ']' | ')' => {
                    if let Some((_, start)) = stack.pop()
                        && line_no > start
                    {
                        out.push(FoldRange {
                            start_line: start,
                            end_line: line_no,
                        });
                    }
                }
                _ => {}
            }
        }
        // strings in these languages do not span lines in the folding model
        in_str = None;
        escape = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_indentation() {
        let doc = Document::from_str("a:\n  b: 1\n  c:\n    d: 2\ne: 3\n");
        let f = fold_ranges(&doc, Language::Yaml);
        assert_eq!(
            f,
            vec![
                FoldRange {
                    start_line: 0,
                    end_line: 3
                },
                FoldRange {
                    start_line: 2,
                    end_line: 3
                }
            ]
        );
    }

    #[test]
    fn json_brackets() {
        let doc = Document::from_str("{\n  \"a\": [\n    1\n  ],\n  \"s\": \"{\"\n}\n");
        let f = fold_ranges(&doc, Language::Json);
        assert_eq!(
            f,
            vec![
                FoldRange {
                    start_line: 0,
                    end_line: 5
                },
                FoldRange {
                    start_line: 1,
                    end_line: 3
                }
            ]
        );
    }

    #[test]
    fn single_range_matches_full_scan() {
        let yaml = Document::from_str("a:\n  b: 1\n  c:\n    d: 2\ne: 3\n");
        let json = Document::from_str("{\n  \"a\": [\n    1\n  ],\n  \"s\": \"{\"\n}\n");
        let md = Document::from_str("# A\ntext\n## B\nmore\n# C\nend\n");
        for (doc, lang) in [
            (&yaml, Language::Yaml),
            (&json, Language::Json),
            (&md, Language::Markdown),
        ] {
            let all = fold_ranges(doc, lang);
            for line in 0..doc.line_count() {
                let expect = all.iter().find(|f| f.start_line == line).copied();
                assert_eq!(
                    fold_range_at(doc, lang, line),
                    expect,
                    "{lang:?} line {line}"
                );
            }
        }
    }

    #[test]
    fn markdown_headings() {
        let doc = Document::from_str("# A\ntext\n## B\nmore\n# C\nend\n");
        let f = fold_ranges(&doc, Language::Markdown);
        assert_eq!(
            f,
            vec![
                FoldRange {
                    start_line: 0,
                    end_line: 3
                },
                FoldRange {
                    start_line: 2,
                    end_line: 3
                },
                FoldRange {
                    start_line: 4,
                    end_line: 5
                }
            ]
        );
    }
}
