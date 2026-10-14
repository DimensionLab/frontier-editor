// SPDX-License-Identifier: Apache-2.0
//! Handing frames to a host that cannot present a GPU surface (a WebView that shows a
//! 2D canvas): only what changed since the frame the host has.
//!
//! The host pulls. [`FrameStore::put`] takes each rendered frame; [`FrameStore::take`]
//! answers "what do I do to the picture I have to make it the newest one": move it up
//! or down by so many rows (a scroll is a blit), then replace a few rectangles (the rows
//! the scroll exposed, the scrollbar, a line that was typed in). Frames rendered between
//! two pulls are never sent. The cost of a pull is in the size of what changed, not of
//! the view.

/// Rows are compared in bands this high; a changed band is sent from its first to its
/// last changed row and column.
const BAND: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// What turns the picture the host has into the newest frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Delta {
    pub width: u32,
    pub height: u32,
    /// Move the picture up by this many rows first (negative: down).
    pub shift: i32,
    pub rects: Vec<Rect>,
    /// The frame this leads to.
    pub seq: u64,
}

impl Delta {
    pub fn pixels(&self) -> u64 {
        self.rects.iter().map(|r| r.w as u64 * r.h as u64).sum()
    }
}

#[derive(Debug, Default)]
pub struct FrameStore {
    w: u32,
    h: u32,
    /// The picture the host has (after every delta it took).
    shown: Vec<u8>,
    shown_scroll: i64,
    /// The newest frame, when the host has not taken it yet.
    latest: Option<Vec<u8>>,
    latest_scroll: i64,
    seq: u64,
}

impl FrameStore {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// A frame is waiting to be taken.
    pub fn has_news(&self) -> bool {
        self.latest.is_some()
    }

    /// Takes a rendered frame (RGBA, `w * h * 4` bytes) and the view's scroll position
    /// in device pixels when it was built. Returns the frame's number.
    pub fn put(&mut self, w: u32, h: u32, data: Vec<u8>, scroll_px: i64) -> u64 {
        debug_assert_eq!(data.len(), (w as usize) * (h as usize) * 4);
        if w != self.w || h != self.h {
            self.w = w;
            self.h = h;
            self.shown.clear();
        }
        self.latest = Some(data);
        self.latest_scroll = scroll_px;
        self.seq += 1;
        self.seq
    }

    /// The delta from what the host has to the newest frame, and the pixels of its
    /// rectangles appended to `out` (RGBA, rectangle after rectangle, row by row).
    /// `None` when the host is up to date.
    pub fn take(&mut self, out: &mut Vec<u8>) -> Option<Delta> {
        let new = self.latest.take()?;
        let (w, h) = (self.w as usize, self.h as usize);
        let row = w * 4;
        let mut delta = Delta {
            width: self.w,
            height: self.h,
            shift: 0,
            rects: Vec::new(),
            seq: self.seq,
        };
        if self.shown.len() != new.len() || w == 0 || h == 0 {
            // nothing to build on (first frame, a resize)
            delta.rects.push(Rect {
                x: 0,
                y: 0,
                w: self.w,
                h: self.h,
            });
            out.extend_from_slice(&new);
            self.shown = new;
            self.shown_scroll = self.latest_scroll;
            return Some(delta);
        }
        // a scroll moves the picture: compare against the old one moved the same way
        let shift = self.latest_scroll - self.shown_scroll;
        if shift != 0 && shift.unsigned_abs() < h as u64 * 3 / 4 {
            let s = shift.unsigned_abs() as usize * row;
            if shift > 0 {
                self.shown.copy_within(s.., 0);
            } else {
                let n = self.shown.len();
                self.shown.copy_within(..n - s, s);
            }
            delta.shift = shift as i32;
        }
        let old = &self.shown;
        let mut y = 0;
        while y < h {
            let y_end = (y + BAND).min(h);
            // first and last changed row of the band
            let first =
                (y..y_end).find(|r| old[r * row..(r + 1) * row] != new[r * row..(r + 1) * row]);
            if let Some(y0) = first {
                let y1 = (y0..y_end)
                    .rev()
                    .find(|r| old[r * row..(r + 1) * row] != new[r * row..(r + 1) * row])
                    .unwrap_or(y0)
                    + 1;
                // first and last changed column over those rows
                let (mut x0, mut x1) = (w, 0usize);
                for r in y0..y1 {
                    let a = &old[r * row..(r + 1) * row];
                    let b = &new[r * row..(r + 1) * row];
                    if a == b {
                        continue;
                    }
                    let lead = a
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .zip(b.as_chunks::<4>().0)
                        .take(x0)
                        .position(|(p, q)| p != q)
                        .unwrap_or(x0);
                    x0 = x0.min(lead);
                    let tail = a
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .zip(b.as_chunks::<4>().0)
                        .rposition(|(p, q)| p != q)
                        .map_or(0, |i| i + 1);
                    x1 = x1.max(tail);
                    if x0 == 0 && x1 == w {
                        break;
                    }
                }
                if x1 > x0 {
                    let rect = Rect {
                        x: x0 as u32,
                        y: y0 as u32,
                        w: (x1 - x0) as u32,
                        h: (y1 - y0) as u32,
                    };
                    // bands that touch and span the same columns are one rectangle
                    match delta.rects.last_mut() {
                        Some(last)
                            if last.x == rect.x
                                && last.w == rect.w
                                && last.y + last.h == rect.y =>
                        {
                            last.h += rect.h;
                        }
                        _ => delta.rects.push(rect),
                    }
                }
            }
            y = y_end;
        }
        for r in &delta.rects {
            for yy in r.y as usize..(r.y + r.h) as usize {
                let a = yy * row + r.x as usize * 4;
                out.extend_from_slice(&new[a..a + r.w as usize * 4]);
            }
        }
        self.shown = new;
        self.shown_scroll = self.latest_scroll;
        Some(delta)
    }
}

/// The wire form of a delta: little-endian `u32`s `[width, height, shift (as i32),
/// rectangles, seq (low 32 bits), extra...]` (`HEADER` words), then `[x, y, w, h]` per
/// rectangle, then the pixels.
pub const HEADER: usize = 12;

pub fn encode(delta: &Delta, extra: &[u32], pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER * 4 + delta.rects.len() * 16 + pixels.len());
    let mut head = [0u32; HEADER];
    head[0] = delta.width;
    head[1] = delta.height;
    head[2] = delta.shift as u32;
    head[3] = delta.rects.len() as u32;
    head[4] = delta.seq as u32;
    for (slot, v) in head[5..].iter_mut().zip(extra) {
        *slot = *v;
    }
    for v in head {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for r in &delta.rects {
        for v in [r.x, r.y, r.w, r.h] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out.extend_from_slice(pixels);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture whose row `y` holds the value `base + y` in every pixel.
    fn rows(w: u32, h: u32, base: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for _ in 0..w {
                v.extend_from_slice(&(base + y).to_le_bytes());
            }
        }
        v
    }

    /// Applies a delta to `picture` the way the page does.
    fn apply(picture: &mut Vec<u8>, d: &Delta, pixels: &[u8]) {
        let (w, h) = (d.width as usize, d.height as usize);
        let row = w * 4;
        if picture.len() != w * h * 4 {
            *picture = vec![0; w * h * 4];
        }
        if d.shift > 0 {
            picture.copy_within(d.shift as usize * row.., 0);
        } else if d.shift < 0 {
            let n = picture.len();
            picture.copy_within(..n - (-d.shift) as usize * row, (-d.shift) as usize * row);
        }
        let mut at = 0;
        for r in &d.rects {
            for y in r.y as usize..(r.y + r.h) as usize {
                let a = y * row + r.x as usize * 4;
                picture[a..a + r.w as usize * 4]
                    .copy_from_slice(&pixels[at..at + r.w as usize * 4]);
                at += r.w as usize * 4;
            }
        }
        assert_eq!(at, pixels.len());
    }

    #[test]
    fn the_first_frame_is_sent_whole_and_an_equal_one_not_at_all() {
        let mut s = FrameStore::default();
        let (w, h) = (40, 100);
        s.put(w, h, rows(w, h, 0), 0);
        let mut px = Vec::new();
        let d = s.take(&mut px).expect("first");
        assert_eq!(d.rects, vec![Rect { x: 0, y: 0, w, h }]);
        assert_eq!(px.len(), (w * h * 4) as usize);
        assert!(s.take(&mut px).is_none(), "nothing new");
        s.put(w, h, rows(w, h, 0), 0);
        px.clear();
        let d = s.take(&mut px).expect("a frame was put");
        assert!(d.rects.is_empty() && d.shift == 0 && px.is_empty());
    }

    #[test]
    fn a_typed_line_is_one_small_rectangle() {
        let mut s = FrameStore::default();
        let (w, h) = (200, 300);
        let base = rows(w, h, 0);
        s.put(w, h, base.clone(), 0);
        let mut picture = Vec::new();
        let mut px = Vec::new();
        let d = s.take(&mut px).expect("first");
        apply(&mut picture, &d, &px);
        let mut next = base.clone();
        for y in 100..118usize {
            for x in 50..90usize {
                next[(y * w as usize + x) * 4] ^= 0xff;
            }
        }
        s.put(w, h, next.clone(), 0);
        px.clear();
        let d = s.take(&mut px).expect("changed");
        assert_eq!(d.shift, 0);
        assert_eq!(
            d.rects,
            vec![Rect {
                x: 50,
                y: 100,
                w: 40,
                h: 18
            }]
        );
        assert_eq!(d.pixels(), 40 * 18);
        apply(&mut picture, &d, &px);
        assert_eq!(picture, next);
    }

    #[test]
    fn a_scroll_is_a_move_plus_the_rows_it_exposed_and_what_stood_still() {
        let mut s = FrameStore::default();
        let (w, h) = (120u32, 240u32);
        // content scrolls; a 10-pixel strip on the right (a scrollbar) does not
        let frame = |scroll: u32| {
            let mut f = rows(w, h, scroll);
            for y in 0..h as usize {
                for x in 110..120usize {
                    f[(y * w as usize + x) * 4..][..4].copy_from_slice(&7u32.to_le_bytes());
                }
            }
            f
        };
        let mut picture = Vec::new();
        let mut px = Vec::new();
        s.put(w, h, frame(1000), 1000);
        let d = s.take(&mut px).expect("first");
        apply(&mut picture, &d, &px);
        for (from, to) in [(1000u32, 1034u32), (1034, 1000), (1000, 1003)] {
            let _ = from;
            s.put(w, h, frame(to), to as i64);
            px.clear();
            let d = s.take(&mut px).expect("scrolled");
            assert_eq!(d.shift, to as i32 - from as i32);
            let moved = (to as i64 - from as i64).unsigned_abs();
            assert!(
                d.pixels() <= moved * w as u64 + 10 * h as u64,
                "{} pixels for a scroll by {moved}: {:?}",
                d.pixels(),
                d.rects
            );
            apply(&mut picture, &d, &px);
            assert_eq!(picture, frame(to));
        }
        // frames nobody took are skipped: one delta covers them
        s.put(w, h, frame(1010), 1010);
        s.put(w, h, frame(1020), 1020);
        px.clear();
        let d = s.take(&mut px).expect("two frames, one delta");
        assert_eq!(d.shift, 17);
        apply(&mut picture, &d, &px);
        assert_eq!(picture, frame(1020));
        // a jump further than the view is sent whole
        s.put(w, h, frame(9000), 9000);
        px.clear();
        let d = s.take(&mut px).expect("jump");
        assert_eq!(d.shift, 0);
        apply(&mut picture, &d, &px);
        assert_eq!(picture, frame(9000));
    }

    #[test]
    fn a_resize_starts_over_and_the_wire_form_has_its_header() {
        let mut s = FrameStore::default();
        s.put(8, 8, rows(8, 8, 0), 0);
        let mut px = Vec::new();
        s.take(&mut px);
        s.put(16, 8, rows(16, 8, 0), 0);
        px.clear();
        let d = s.take(&mut px).expect("resized");
        assert_eq!(
            d.rects,
            vec![Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 8
            }]
        );
        let wire = encode(&d, &[42, 7], &px);
        assert_eq!(wire.len(), HEADER * 4 + 16 + 16 * 8 * 4);
        let word = |i: usize| {
            u32::from_le_bytes([
                wire[i * 4],
                wire[i * 4 + 1],
                wire[i * 4 + 2],
                wire[i * 4 + 3],
            ])
        };
        assert_eq!((word(0), word(1), word(2), word(3)), (16, 8, 0, 1));
        assert_eq!((word(5), word(6)), (42, 7));
        assert_eq!((word(HEADER), word(HEADER + 2)), (0, 16));
    }
}
