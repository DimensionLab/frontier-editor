// SPDX-License-Identifier: Apache-2.0
//! Visual rows under soft wrap: how many rows each line takes, kept up to date by
//! re-measuring only the lines an edit touched.

use crate::document::Document;
use crate::layout::LineLayout;

const BLOCK: usize = 512;

#[derive(Debug, Clone)]
pub(crate) struct RowMap {
    pub cols: usize,
    pub tab: usize,
    /// Rows of each line (0 for a line hidden by a fold).
    counts: Vec<u16>,
    /// Rows of each block of `BLOCK` lines.
    blocks: Vec<u32>,
    total: usize,
}

impl RowMap {
    /// `hidden(line)` is true for lines inside a fold.
    pub fn build(doc: &Document, cols: usize, tab: usize, hidden: impl Fn(usize) -> bool) -> Self {
        let n = doc.line_count();
        let mut counts = Vec::with_capacity(n);
        for line in 0..n {
            counts.push(if hidden(line) {
                0
            } else {
                Self::measure(doc, line, cols, tab)
            });
        }
        let mut m = Self {
            cols,
            tab,
            counts,
            blocks: Vec::new(),
            total: 0,
        };
        m.reindex();
        m
    }

    fn measure(doc: &Document, line: usize, cols: usize, tab: usize) -> u16 {
        // a character is at most two cells: short lines need no look at their text
        if doc.line_len(line) * 2 <= cols {
            return 1;
        }
        let text = doc.line(line);
        LineLayout::new(&text, tab)
            .wrap(&text, cols)
            .len()
            .min(u16::MAX as usize) as u16
    }

    fn reindex(&mut self) {
        self.blocks.clear();
        self.total = 0;
        for chunk in self.counts.chunks(BLOCK) {
            let s: u32 = chunk.iter().map(|c| *c as u32).sum();
            self.blocks.push(s);
            self.total += s as usize;
        }
    }

    pub fn total(&self) -> usize {
        self.total.max(1)
    }

    pub fn rows_of(&self, line: usize) -> usize {
        self.counts.get(line).copied().unwrap_or(1) as usize
    }

    /// Rows above `line`.
    pub fn rows_before(&self, line: usize) -> usize {
        let line = line.min(self.counts.len());
        let b = line / BLOCK;
        let mut rows: usize = self.blocks[..b.min(self.blocks.len())]
            .iter()
            .map(|v| *v as usize)
            .sum();
        rows += self.counts[b * BLOCK..line]
            .iter()
            .map(|c| *c as usize)
            .sum::<usize>();
        rows
    }

    /// The line a row belongs to and which of the line's rows it is.
    pub fn locate(&self, row: usize) -> (usize, usize) {
        let mut left = row.min(self.total.saturating_sub(1));
        let mut line = 0;
        for b in &self.blocks {
            if left < *b as usize {
                break;
            }
            left -= *b as usize;
            line += BLOCK;
        }
        while line < self.counts.len() {
            let c = self.counts[line] as usize;
            if left < c {
                return (line, left);
            }
            left -= c;
            line += 1;
        }
        (self.counts.len().saturating_sub(1), 0)
    }

    /// Lines `first..=old_last` were replaced by lines `first..=new_last` of `doc`.
    pub fn splice(&mut self, doc: &Document, first: usize, old_last: usize, new_last: usize) {
        let n = doc.line_count();
        let first = first.min(self.counts.len());
        let old_end = (old_last + 1).min(self.counts.len()).max(first);
        let new_end = (new_last + 1).min(n).max(first);
        let (cols, tab) = (self.cols, self.tab);
        let fresh: Vec<u16> = (first..new_end)
            .map(|l| Self::measure(doc, l, cols, tab))
            .collect();
        self.counts.splice(first..old_end, fresh);
        // whatever happened, one count per line
        self.counts.resize(n, 1);
        self.reindex();
    }
}
