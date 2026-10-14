// SPDX-License-Identifier: Apache-2.0
//! Bracket matching around a cursor. Skips double- and single-quoted strings on the
//! same line; the scan is bounded so a stray bracket in a 100 MB file stays cheap.

use crate::document::{Document, Range};

const SCAN_LIMIT: usize = 200_000;

fn pair(c: char) -> Option<(char, bool)> {
    Some(match c {
        '(' => (')', true),
        '[' => (']', true),
        '{' => ('}', true),
        ')' => ('(', false),
        ']' => ('[', false),
        '}' => ('{', false),
        _ => return None,
    })
}

/// The bracket at or just before `offset` and its match, as two one-char ranges.
pub fn matching(doc: &Document, offset: usize) -> Option<(Range, Range)> {
    let candidates = [Some(offset), offset.checked_sub(1)];
    for at in candidates.into_iter().flatten() {
        if let Some(c) = doc.char_at(at)
            && let Some((other, forward)) = pair(c)
            && let Some(m) = scan(doc, at, c, other, forward)
        {
            return Some((Range::new(at, at + 1), Range::new(m, m + 1)));
        }
    }
    None
}

fn scan(doc: &Document, at: usize, open: char, close: char, forward: bool) -> Option<usize> {
    let n = doc.len_chars();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut steps = 0usize;
    let mut i = at;
    loop {
        let c = doc.char_at(i)?;
        if let Some(q) = quote {
            if c == q || c == '\n' {
                quote = None;
            }
        } else {
            match c {
                '"' | '\'' if i != at => quote = Some(c),
                _ if c == open => depth += 1,
                _ if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        steps += 1;
        if steps > SCAN_LIMIT {
            return None;
        }
        if forward {
            i += 1;
            if i >= n {
                return None;
            }
        } else {
            i = i.checked_sub(1)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_nested_and_skips_strings() {
        let doc = Document::from_str("{\"a\": [1, \"]\", {}]}");
        assert_eq!(
            matching(&doc, 0),
            Some((Range::new(0, 1), Range::new(18, 19)))
        );
        assert_eq!(
            matching(&doc, 6),
            Some((Range::new(6, 7), Range::new(17, 18)))
        );
        assert_eq!(
            matching(&doc, 19),
            Some((Range::new(18, 19), Range::new(0, 1)))
        );
        assert_eq!(matching(&doc, 2), None);
    }
}
