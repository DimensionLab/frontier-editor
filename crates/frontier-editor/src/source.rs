// SPDX-License-Identifier: Apache-2.0
//! Chunked, range-read documents for files too large to hold as a rope (logs).
//!
//! A [`DocumentSource`] serves byte ranges; [`ChunkedDocument`] keeps a sparse line
//! index (one offset per `BLOCK_LINES` lines) and a small cache of chunks, so a
//! multi-million-line log costs a few megabytes however long it is. Reads that miss
//! the cache return `None` and are reported through [`ChunkedDocument::missing`] so
//! an async host (the app fetching `Range:` requests from the server) can fill them.

use std::collections::VecDeque;
use std::fmt;
use thiserror::Error;

/// Lines per sparse-index entry.
pub const BLOCK_LINES: usize = 256;
/// Bytes per cached chunk.
pub const CHUNK_BYTES: usize = 1 << 20;
/// Documents above this size go through `ChunkedDocument` instead of a rope.
pub const CHUNKED_THRESHOLD: u64 = 50 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("range {start}+{len} is not loaded yet")]
    NotLoaded { start: u64, len: usize },
    #[error("{0}")]
    Io(String),
}

/// Random-access bytes. Implementations: an in-memory buffer, a file, an HTTP
/// range cache owned by the host, a synthetic generator for benchmarks.
pub trait DocumentSource {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Bytes `[start, start+len)` clamped to the end. `Err(NotLoaded)` when the host
    /// has not fetched them yet.
    fn read(&mut self, start: u64, len: usize) -> Result<Vec<u8>, SourceError>;
    /// A line index the source already knows (fixed-width synthetic lines, a
    /// server-side index). `None` means the document scans for newlines.
    fn known_line_index(&self) -> Option<LineIndex> {
        None
    }
}

impl fmt::Debug for dyn DocumentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DocumentSource({} bytes)", self.len())
    }
}

#[derive(Debug, Clone)]
pub struct MemorySource(pub Vec<u8>);

impl DocumentSource for MemorySource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&mut self, start: u64, len: usize) -> Result<Vec<u8>, SourceError> {
        let s = (start as usize).min(self.0.len());
        let e = (s + len).min(self.0.len());
        Ok(self.0[s..e].to_vec())
    }
}

/// Deterministic synthetic log for benchmarks and the demo: `line_count` lines,
/// each `"<n:08> <level> step=<n> loss=<f>\n"`.
#[derive(Debug, Clone)]
pub struct SyntheticLog {
    pub line_count: u64,
}

impl SyntheticLog {
    pub fn line(&self, i: u64) -> String {
        let level = match i % 97 {
            0 => "ERROR",
            n if n % 13 == 0 => "WARN",
            _ => "INFO",
        };
        format!(
            "{i:08} {level:<5} step={i:08} loss={:.6}",
            1.0 / (1.0 + (i as f64) * 1e-4)
        )
    }
    fn line_bytes(&self) -> usize {
        // every line has the same width by construction
        self.line(0).len() + 1
    }
}

impl DocumentSource for SyntheticLog {
    fn len(&self) -> u64 {
        self.line_count * self.line_bytes() as u64
    }
    fn read(&mut self, start: u64, len: usize) -> Result<Vec<u8>, SourceError> {
        let w = self.line_bytes() as u64;
        let end = (start + len as u64).min(self.len());
        let mut out = Vec::with_capacity(len);
        let mut line = start / w;
        let mut pos = line * w;
        while pos < end {
            let text = self.line(line);
            let bytes = text.as_bytes();
            let line_start = pos;
            for (k, b) in bytes.iter().chain(std::iter::once(&b'\n')).enumerate() {
                let p = line_start + k as u64;
                if p >= start && p < end {
                    out.push(*b);
                }
            }
            line += 1;
            pos = line * w;
        }
        Ok(out)
    }
    fn known_line_index(&self) -> Option<LineIndex> {
        let w = self.line_bytes() as u64;
        let blocks = self.line_count.div_ceil(BLOCK_LINES as u64);
        Some(LineIndex {
            block_offsets: (0..=blocks)
                .map(|b| (b * BLOCK_LINES as u64 * w).min(self.len()))
                .collect(),
            line_count: self.line_count,
            total_bytes: self.len(),
        })
    }
}

/// Sparse newline index: byte offset of line `k * BLOCK_LINES` for every k, plus the
/// end. 5 M lines ≈ 20 k entries ≈ 160 kB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    pub block_offsets: Vec<u64>,
    pub line_count: u64,
    pub total_bytes: u64,
}

impl LineIndex {
    /// Scans the whole source once. Cost: one pass over the bytes.
    pub fn scan(src: &mut dyn DocumentSource) -> Result<Self, SourceError> {
        let total = src.len();
        let mut block_offsets = vec![0u64];
        let mut lines: u64 = 0;
        let mut pos: u64 = 0;
        let mut last_byte_nl = true;
        while pos < total {
            let buf = src.read(pos, CHUNK_BYTES)?;
            if buf.is_empty() {
                break;
            }
            for (i, b) in buf.iter().enumerate() {
                if *b == b'\n' {
                    lines += 1;
                    if lines.is_multiple_of(BLOCK_LINES as u64) {
                        block_offsets.push(pos + i as u64 + 1);
                    }
                }
            }
            last_byte_nl = buf.last() == Some(&b'\n');
            pos += buf.len() as u64;
        }
        if !last_byte_nl && total > 0 {
            lines += 1;
        }
        Ok(Self {
            block_offsets,
            line_count: lines,
            total_bytes: total,
        })
    }

    pub fn memory_bytes(&self) -> usize {
        self.block_offsets.len() * std::mem::size_of::<u64>()
    }
}

#[derive(Debug)]
struct Chunk {
    start: u64,
    data: Vec<u8>,
}

/// A large document read through a [`DocumentSource`].
pub struct ChunkedDocument {
    source: Box<dyn DocumentSource>,
    index: LineIndex,
    cache: VecDeque<Chunk>,
    max_chunks: usize,
    missing: Vec<(u64, usize)>,
}

impl fmt::Debug for ChunkedDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkedDocument")
            .field("lines", &self.index.line_count)
            .field("bytes", &self.index.total_bytes)
            .field("cached_chunks", &self.cache.len())
            .finish()
    }
}

impl ChunkedDocument {
    /// Builds the line index (using the source's own when it has one).
    pub fn open(mut source: Box<dyn DocumentSource>) -> Result<Self, SourceError> {
        let index = match source.known_line_index() {
            Some(i) => i,
            None => LineIndex::scan(source.as_mut())?,
        };
        Ok(Self {
            source,
            index,
            cache: VecDeque::new(),
            max_chunks: 16,
            missing: Vec::new(),
        })
    }

    pub fn line_count(&self) -> u64 {
        self.index.line_count
    }

    pub fn len_bytes(&self) -> u64 {
        self.index.total_bytes
    }

    pub fn index(&self) -> &LineIndex {
        &self.index
    }

    /// Approximate memory held by the index and the chunk cache.
    pub fn memory_bytes(&self) -> usize {
        self.index.memory_bytes() + self.cache.iter().map(|c| c.data.len()).sum::<usize>()
    }

    /// Ranges a host must load (cleared on read).
    pub fn take_missing(&mut self) -> Vec<(u64, usize)> {
        std::mem::take(&mut self.missing)
    }

    fn chunk_containing(&mut self, offset: u64) -> Option<usize> {
        if let Some(i) = self
            .cache
            .iter()
            .position(|c| offset >= c.start && offset < c.start + c.data.len() as u64)
        {
            return Some(i);
        }
        let start = offset - offset % CHUNK_BYTES as u64;
        match self.source.read(start, CHUNK_BYTES) {
            Ok(data) if !data.is_empty() => {
                if self.cache.len() >= self.max_chunks {
                    self.cache.pop_front();
                }
                self.cache.push_back(Chunk { start, data });
                Some(self.cache.len() - 1)
            }
            Ok(_) => None,
            Err(SourceError::NotLoaded { start, len }) => {
                self.missing.push((start, len));
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "chunk read failed");
                None
            }
        }
    }

    /// Byte offset of the start of `line`, scanning from the block boundary.
    pub fn line_offset(&mut self, line: u64) -> Option<u64> {
        if line >= self.index.line_count {
            return None;
        }
        let block = (line / BLOCK_LINES as u64) as usize;
        let mut pos = *self.index.block_offsets.get(block)?;
        let mut remaining = line % BLOCK_LINES as u64;
        while remaining > 0 {
            let ci = self.chunk_containing(pos)?;
            let c = &self.cache[ci];
            let local = (pos - c.start) as usize;
            match c.data[local..].iter().position(|b| *b == b'\n') {
                Some(nl) => {
                    pos += nl as u64 + 1;
                    remaining -= 1;
                }
                None => pos = c.start + c.data.len() as u64,
            }
        }
        Some(pos)
    }

    /// Text of `line` without its newline; `None` when out of range or not loaded.
    pub fn line(&mut self, line: u64) -> Option<String> {
        let mut pos = self.line_offset(line)?;
        let mut bytes = Vec::new();
        loop {
            let ci = self.chunk_containing(pos)?;
            let c = &self.cache[ci];
            let local = (pos - c.start) as usize;
            match c.data[local..].iter().position(|b| *b == b'\n') {
                Some(nl) => {
                    bytes.extend_from_slice(&c.data[local..local + nl]);
                    break;
                }
                None => {
                    bytes.extend_from_slice(&c.data[local..]);
                    pos = c.start + c.data.len() as u64;
                    if pos >= self.index.total_bytes {
                        break;
                    }
                }
            }
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Lines `[first, first+count)`; `None` entries are not loaded.
    pub fn lines(&mut self, first: u64, count: usize) -> Vec<Option<String>> {
        (first..first + count as u64)
            .map(|l| self.line(l))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_lines() {
        let text: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let mut doc =
            ChunkedDocument::open(Box::new(MemorySource(text.into_bytes()))).expect("open");
        assert_eq!(doc.line_count(), 1000);
        assert_eq!(doc.index().block_offsets.len(), 1 + 1000 / BLOCK_LINES);
        assert_eq!(doc.line(0).as_deref(), Some("line 0"));
        assert_eq!(doc.line(257).as_deref(), Some("line 257"));
        assert_eq!(doc.line(999).as_deref(), Some("line 999"));
        assert_eq!(doc.line(1000), None);
    }

    #[test]
    fn no_trailing_newline_counts_last_line() {
        let mut doc =
            ChunkedDocument::open(Box::new(MemorySource(b"a\nb".to_vec()))).expect("open");
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line(1).as_deref(), Some("b"));
    }

    #[test]
    fn synthetic_five_million_lines_is_small() {
        let src = SyntheticLog {
            line_count: 5_000_000,
        };
        let mut doc = ChunkedDocument::open(Box::new(src.clone())).expect("open");
        assert_eq!(doc.line_count(), 5_000_000);
        assert_eq!(doc.line(4_999_999), Some(src.line(4_999_999)));
        assert_eq!(doc.line(1234567), Some(src.line(1234567)));
        assert!(
            doc.memory_bytes() < 4 * CHUNK_BYTES,
            "{}",
            doc.memory_bytes()
        );
        assert!(doc.index().memory_bytes() < 200_000);
    }

    #[test]
    fn not_loaded_is_reported() {
        struct Lazy;
        impl DocumentSource for Lazy {
            fn len(&self) -> u64 {
                100
            }
            fn read(&mut self, start: u64, len: usize) -> Result<Vec<u8>, SourceError> {
                Err(SourceError::NotLoaded { start, len })
            }
            fn known_line_index(&self) -> Option<LineIndex> {
                Some(LineIndex {
                    block_offsets: vec![0, 100],
                    line_count: 10,
                    total_bytes: 100,
                })
            }
        }
        let mut doc = ChunkedDocument::open(Box::new(Lazy)).expect("open");
        assert_eq!(doc.line(3), None);
        assert_eq!(doc.take_missing(), vec![(0, CHUNK_BYTES)]);
    }
}
