// SPDX-License-Identifier: Apache-2.0
//! `EditorView`: turns core state + a viewport into quads, and pointer/keys into
//! core commands. Only visible rows are touched each frame; text comes from the
//! core's `view_state` (documents) or a `ChunkedDocument` (logs).

use crate::diff_view::{Cell, CellKind, DiffRow, DiffRows, build_rows};
use crate::input::{Drag, HostRequest, InputEvent, Key, KeyCode, Modifiers};
use crate::render::{Instance, Placement, Renderer};
use crate::text::{AtlasFull, Metrics, TextSystem};
use crate::theme::{Rgba, Theme, alpha};
use frontier_editor::command::Command;
use frontier_editor::decoration::Severity;
use frontier_editor::document::{Position, Range};
use frontier_editor::editor::{Mode, Outcome};
use frontier_editor::layout::LineLayout;
use frontier_editor::source::{ChunkedDocument, DocumentSource};
use frontier_editor::syntax::{Language, Span, TokenKind, fallback};
use frontier_editor::view::{FoldState, GutterMark, ViewLine, ViewState, Viewport};
use frontier_editor::{Editor, Highlighter};
use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::rc::Rc;

/// A buffer several views can show at once: the node's editor and its focus mode, a
/// file open on two pages. Selections, undo history and folds belong to the buffer;
/// size, scroll position and pointer state belong to each view.
pub type SharedEditor = Rc<RefCell<Editor>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Inline,
    SideBySide,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ViewConfig {
    pub font_size: f32,
    pub line_numbers: bool,
    pub minimap: bool,
    pub diff_mode: DiffMode,
    /// Smooth-scroll factor per 16 ms frame (1 = instant).
    pub scroll_smoothing: f32,
    pub blink_ms: u64,
    /// Row height as a multiple of the font size.
    pub line_height: f32,
    /// Tint the row of the caret.
    pub current_line: bool,
    pub cursor: CursorStyle,
    pub cursor_blink: bool,
    /// A comparison offers Accept / Reject on each change (a proposal under review).
    /// Off: two texts are compared and nothing can be decided.
    /// Soft wrap: long lines continue on the next row instead of scrolling sideways.
    pub wrap: bool,
    pub review_actions: bool,
    /// Unchanged lines kept around each change of a comparison; the rest collapse.
    pub diff_context: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorStyle {
    #[default]
    Bar,
    Block,
    Underline,
}

impl Default for ViewConfig {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            line_numbers: true,
            minimap: true,
            diff_mode: DiffMode::Inline,
            scroll_smoothing: 0.45,
            blink_ms: 530,
            line_height: 1.45,
            current_line: true,
            cursor: CursorStyle::Bar,
            cursor_blink: true,
            wrap: false,
            review_actions: true,
            diff_context: 3,
        }
    }
}

/// What the view is showing.
#[allow(clippy::large_enum_variant)]
pub enum Content {
    Empty,
    Editor(SharedEditor),
    Log {
        doc: ChunkedDocument,
        highlighter: Highlighter,
    },
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Content::Empty => write!(f, "Empty"),
            Content::Editor(e) => match e.try_borrow() {
                Ok(e) => write!(f, "Editor({})", e.path()),
                Err(_) => write!(f, "Editor(in use)"),
            },
            Content::Log { doc, .. } => write!(f, "Log({} lines)", doc.line_count()),
        }
    }
}

/// Per-frame numbers for HUDs and benchmarks.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameStats {
    pub rows: usize,
    pub instances: usize,
    pub glyphs: usize,
    pub build_ms: f64,
    pub atlas_glyphs: usize,
    pub atlas_bytes: usize,
    pub shape_cache: usize,
}

/// Geometry of one frame in device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub metrics: Metrics,
    pub width: f32,
    pub height: f32,
    pub gutter_w: f32,
    pub fold_x: f32,
    pub text_x: f32,
    pub text_w: f32,
    pub minimap_x: f32,
    pub minimap_w: f32,
    pub scrollbar_x: f32,
    pub rows_visible: usize,
}

/// What a comparison holds, for the host's bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffSummary {
    pub changes: usize,
    pub added: usize,
    pub removed: usize,
    pub accepted: usize,
    pub rejected: usize,
    /// The change the keyboard is on, 1-based.
    pub at: Option<usize>,
}

/// One sampled line in the minimap: visible length and dominant token.
type MinimapRow = (u16, Option<TokenKind>);

#[derive(Debug, Clone, Copy, PartialEq)]
enum HitKind {
    Fold(usize),
    HunkAccept(usize),
    HunkReject(usize),
    /// A collapsed stretch of unchanged lines: show them.
    Expand,
    Minimap,
    Scrollbar,
}

#[derive(Debug, Clone, Copy)]
struct HitRegion {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    kind: HitKind,
}

/// Result of a hit test, logical coordinates in.
#[derive(Debug, Clone, PartialEq)]
pub enum Hit {
    Text(Position),
    Gutter(usize),
    FoldToggle(usize),
    HunkAccept(String),
    HunkReject(String),
    /// A collapsed stretch of unchanged lines of a comparison.
    ExpandContext,
    Minimap(f32),
    Scrollbar,
    Outside,
}

pub struct EditorView {
    pub text: TextSystem,
    renderer: Option<Renderer>,
    theme: Theme,
    pub config: ViewConfig,
    content: Content,
    /// Logical size and device pixel ratio.
    width: f32,
    height: f32,
    dpr: f32,
    /// Extra zoom from the host (canvas scale); glyphs are rasterised at dpr × zoom.
    zoom: f32,
    scroll_row: f64,
    scroll_target: f64,
    scroll_x: f32,
    drag: Option<Drag>,
    composition: Option<String>,
    focused: bool,
    blink_origin_ms: u64,
    last_frame_ms: u64,
    instances: Vec<Instance>,
    hits: Vec<HitRegion>,
    geometry: Option<Geometry>,
    diff: DiffRows,
    diff_key: u64,
    diff_hunk_ids: Vec<String>,
    /// Hunks the reviewer turned down (the core only knows the accepted ones).
    diff_rejected: std::collections::BTreeSet<String>,
    /// The hunk the keyboard is on (next / previous change).
    diff_focus: Option<usize>,
    diff_expanded: bool,
    /// Width of one line-number column of a comparison, device px.
    review_numw: f32,
    /// Tab stops of the buffer on show, in cells.
    tab: usize,
    minimap_cache: Option<(u64, Vec<MinimapRow>)>,
    minimap_dirty_at: u64,
    stats: FrameStats,
    animating: bool,
}

impl std::fmt::Debug for EditorView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorView")
            .field("content", &self.content)
            .field("size", &(self.width, self.height, self.dpr))
            .field("scroll_row", &self.scroll_row)
            .finish()
    }
}

const GUTTER_PAD: f32 = 10.0;
const FOLD_COL_W: f32 = 14.0;
const MINIMAP_W: f32 = 84.0;
const MINIMAP_MIN_VIEW_W: f32 = 420.0;
const SCROLLBAR_W: f32 = 8.0;
const CURSOR_W: f32 = 2.0;
const REVIEW_BAR_W: f32 = 4.0;
/// The lane beside the scrollbar where a comparison marks its changes.
const CHANGE_LANE_W: f32 = 6.0;
const MINIMAP_ROW_H: f32 = 2.0;

impl EditorView {
    pub fn new(config: ViewConfig, theme: Theme) -> Self {
        Self {
            text: TextSystem::new(),
            renderer: None,
            theme,
            config,
            content: Content::Empty,
            width: 640.0,
            height: 480.0,
            dpr: 1.0,
            zoom: 1.0,
            scroll_row: 0.0,
            scroll_target: 0.0,
            scroll_x: 0.0,
            drag: None,
            composition: None,
            focused: false,
            blink_origin_ms: 0,
            last_frame_ms: 0,
            instances: Vec::new(),
            hits: Vec::new(),
            geometry: None,
            diff: DiffRows::default(),
            diff_key: 0,
            diff_hunk_ids: Vec::new(),
            diff_rejected: Default::default(),
            diff_focus: None,
            diff_expanded: false,
            review_numw: 0.0,
            tab: 4,
            minimap_cache: None,
            minimap_dirty_at: 0,
            stats: FrameStats::default(),
            animating: false,
        }
    }

    // ----- content -----------------------------------------------------------------

    pub fn open(&mut self, path: &str, text: &str) {
        self.open_editor(Editor::open(path, text));
    }

    pub fn open_editor(&mut self, editor: Editor) {
        self.open_shared(Rc::new(RefCell::new(editor)));
    }

    /// Shows a buffer other views may show too. Scroll position and pointer state are
    /// this view's own; the caret, the selections and the undo history are the buffer's.
    pub fn open_shared(&mut self, editor: SharedEditor) {
        self.content = Content::Editor(editor);
        self.reset_scroll();
    }

    /// The buffer this view shows, to open in another view.
    pub fn shared(&self) -> Option<SharedEditor> {
        match &self.content {
            Content::Editor(e) => Some(e.clone()),
            _ => None,
        }
    }

    /// Opens a large read-only document through a range-reading source.
    pub fn open_log(
        &mut self,
        source: Box<dyn DocumentSource>,
        language: Language,
    ) -> Result<(), frontier_editor::source::SourceError> {
        let doc = ChunkedDocument::open(source)?;
        self.content = Content::Log {
            doc,
            highlighter: Highlighter::new_lexer(language),
        };
        self.reset_scroll();
        Ok(())
    }

    fn reset_scroll(&mut self) {
        self.scroll_row = 0.0;
        self.scroll_target = 0.0;
        self.scroll_x = 0.0;
        self.minimap_cache = None;
        self.diff = DiffRows::default();
        self.diff_key = 0;
        self.diff_focus = None;
        self.diff_rejected.clear();
    }

    pub fn content(&self) -> &Content {
        &self.content
    }

    pub fn editor(&self) -> Option<Ref<'_, Editor>> {
        match &self.content {
            Content::Editor(e) => Some(e.borrow()),
            _ => None,
        }
    }

    pub fn editor_mut(&self) -> Option<RefMut<'_, Editor>> {
        match &self.content {
            Content::Editor(e) => Some(e.borrow_mut()),
            _ => None,
        }
    }

    pub fn log(&self) -> Option<&ChunkedDocument> {
        match &self.content {
            Content::Log { doc, .. } => Some(doc),
            _ => None,
        }
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Logical size of the view and the device pixel ratio.
    pub fn resize(&mut self, width: f32, height: f32, dpr: f32) {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
        if (dpr - self.dpr).abs() > f32::EPSILON {
            self.dpr = dpr.max(0.5);
            self.text.invalidate_shapes();
        }
    }

    /// Host zoom (canvas scale). Glyphs are re-rasterised at the effective scale so
    /// text stays crisp while a board node is zoomed. The layout does not change with
    /// it: every length of a frame is its logical length times `dpr × zoom`.
    pub fn set_zoom(&mut self, zoom: f32) {
        let z = zoom.clamp(0.1, 16.0);
        if (z - self.zoom).abs() > 1e-4 {
            self.zoom = z;
            self.text.invalidate_shapes();
        }
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Device pixels per logical pixel of this view.
    pub fn scale(&self) -> f32 {
        self.dpr * self.zoom
    }

    pub fn stats(&self) -> FrameStats {
        self.stats
    }

    pub fn geometry(&self) -> Option<Geometry> {
        self.geometry
    }

    pub fn scroll_row(&self) -> f64 {
        self.scroll_row
    }

    /// Where the view will rest once smooth scrolling ends.
    pub fn scroll_target(&self) -> f64 {
        self.scroll_target
    }

    /// Where the frame last built is scrolled to, in device pixels. Rows sit on whole
    /// pixels, so between two frames the text moves by exactly the difference.
    pub fn scroll_px(&self) -> i64 {
        let lh = self.geometry.map_or(1.0, |g| g.metrics.line_h) as f64;
        (self.scroll_row * lh).round() as i64
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Height of one row in logical pixels (the same at every zoom).
    pub fn row_height(&self) -> f32 {
        self.config.font_size * self.config.line_height
    }

    // ----- rows ---------------------------------------------------------------------

    pub fn total_rows(&mut self) -> usize {
        match &self.content {
            Content::Empty => 0,
            Content::Log { doc, .. } => doc.line_count() as usize,
            Content::Editor(e) => {
                let e = e.clone();
                if e.borrow().mode() == Mode::Review {
                    self.ensure_diff_rows();
                    self.diff.rows.len()
                } else {
                    e.borrow_mut().row_count()
                }
            }
        }
    }

    /// Side by side when asked for and the view is wide enough for two panes.
    fn side_by_side(&self) -> bool {
        self.config.diff_mode == DiffMode::SideBySide && self.width >= 560.0
    }

    fn in_review(&self) -> bool {
        match &self.content {
            Content::Editor(e) => e.borrow().mode() == Mode::Review,
            _ => false,
        }
    }

    fn ensure_diff_rows(&mut self) {
        let Content::Editor(e) = &self.content else {
            return;
        };
        let e = e.borrow();
        let Some(cs) = e.changeset() else {
            self.diff = DiffRows::default();
            self.diff_key = 0;
            return;
        };
        use std::hash::{Hash, Hasher};
        let sbs = self.config.diff_mode == DiffMode::SideBySide && self.width >= 560.0;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cs.base_hash.hash(&mut h);
        cs.proposed_hash.hash(&mut h);
        (sbs, self.diff_expanded, self.config.diff_context).hash(&mut h);
        let key = h.finish() | 1;
        if self.diff_key != key {
            self.diff_key = key;
            let context = (!self.diff_expanded).then_some(self.config.diff_context);
            self.diff = build_rows(cs, e.document().line_count(), sbs, context);
            self.diff_hunk_ids = cs.hunks.iter().map(|h| h.id.clone()).collect();
            let ids = &self.diff_hunk_ids;
            self.diff_rejected.retain(|id| ids.contains(id));
            if self.diff_focus.is_some_and(|f| f >= ids.len()) {
                self.diff_focus = None;
            }
        }
    }

    /// Numbers of the comparison on show: changes, lines added and removed, changes
    /// accepted and turned down, and the change the keyboard is on (1-based).
    pub fn diff_summary(&mut self) -> Option<DiffSummary> {
        if !self.in_review() {
            return None;
        }
        self.ensure_diff_rows();
        let accepted = self.editor().map(|e| e.accepted_hunks().len()).unwrap_or(0);
        Some(DiffSummary {
            changes: self.diff_hunk_ids.len(),
            added: self.diff.added,
            removed: self.diff.removed,
            accepted,
            rejected: self.diff_rejected.len(),
            at: self.diff_focus.map(|f| f + 1),
        })
    }

    /// Moves to the next or previous change and brings it into view.
    pub fn go_to_change(&mut self, forward: bool) -> Option<usize> {
        self.ensure_diff_rows();
        let n = self.diff.hunk_rows.len();
        if n == 0 {
            return None;
        }
        let next = match self.diff_focus {
            Some(f) if forward => (f + 1) % n,
            Some(f) => (f + n - 1) % n,
            None => {
                // the first change at or below the top of the view (or the last above it)
                let top = self.scroll_target.floor() as usize;
                let at = self.diff.hunk_rows.partition_point(|r| *r < top);
                if forward { at % n } else { (at + n - 1) % n }
            }
        };
        self.diff_focus = Some(next);
        let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
        let visible = (g.height / g.metrics.line_h) as f64;
        let row = self.diff.hunk_rows[next] as f64;
        if row < self.scroll_target + 1.0 || row > self.scroll_target + visible - 4.0 {
            self.scroll_to_row(row - (visible / 3.0).floor(), false);
        }
        Some(next)
    }

    /// Accepts or turns down a change by index (`None`: the one the keyboard is on).
    pub fn decide_change(&mut self, index: Option<usize>, accept: bool) {
        let Some(id) = index
            .or(self.diff_focus)
            .and_then(|i| self.diff_hunk_ids.get(i).cloned())
        else {
            return;
        };
        if let Some(mut e) = self.editor_mut() {
            let _ = e.set_hunk(&id, accept);
        }
        if accept {
            self.diff_rejected.remove(&id);
        } else {
            self.diff_rejected.insert(id);
        }
    }

    /// Accepts or turns down every change.
    pub fn decide_all(&mut self, accept: bool) {
        self.ensure_diff_rows();
        for i in 0..self.diff_hunk_ids.len() {
            self.decide_change(Some(i), accept);
        }
    }

    fn metrics(&mut self) -> Metrics {
        let scale = self.scale();
        let mut m = self.text.metrics(self.config.font_size, scale);
        // The row height is a setting, not the font's: the glyph box is centred in it.
        let line_h = (self.config.font_size * self.config.line_height * scale).max(m.line_h * 0.8);
        m.ascent = (m.ascent + (line_h - m.line_h) / 2.0).round();
        m.line_h = line_h;
        // one cell is the same share of the font size at every zoom, so a line holds
        // the same number of cells whatever the zoom
        m.cell_w = self.config.font_size * scale * self.text.cell_ratio();
        m
    }

    fn compute_geometry(&mut self) -> Geometry {
        let metrics = self.metrics();
        let s = self.scale();
        let width = self.width * s;
        let height = self.height * s;
        let total = self.total_rows().max(1);
        let digits = if self.config.line_numbers {
            (total.max(1) as f64).log10().floor() as usize + 1
        } else {
            0
        };
        let digits = digits.max(2);
        let review = self.in_review();
        let mut gutter_w = if self.config.line_numbers {
            GUTTER_PAD * s + digits as f32 * metrics.cell_w + GUTTER_PAD * s * 0.5 + FOLD_COL_W * s
        } else {
            FOLD_COL_W * s
        };
        if review {
            // a comparison: a state bar, then the old and the new line number (inline),
            // or one number per pane (side by side); the +/- marker sits before the text
            let lines = self
                .editor()
                .map(|e| e.document().line_count() + self.diff.added)
                .unwrap_or(1);
            let d = ((lines.max(1) as f64).log10().floor() as usize + 1).max(2);
            self.review_numw = d as f32 * metrics.cell_w + GUTTER_PAD * s;
            let columns = if self.side_by_side() { 1.0 } else { 2.0 };
            gutter_w = REVIEW_BAR_W * s + columns * self.review_numw + FOLD_COL_W * s;
        }
        let minimap_on = self.config.minimap
            && !review
            && self.width >= MINIMAP_MIN_VIEW_W
            && !matches!(self.content, Content::Log { .. });
        let minimap_w = if minimap_on { MINIMAP_W * s } else { 0.0 };
        let scrollbar_x = width - SCROLLBAR_W * s;
        let minimap_x = scrollbar_x - minimap_w;
        let text_x = gutter_w + GUTTER_PAD * s * 0.5;
        if let Content::Editor(e) = &self.content {
            let cols = (self.config.wrap && !review).then(|| {
                (((minimap_x - text_x) / metrics.cell_w).floor() as usize)
                    .saturating_sub(1)
                    .max(8)
            });
            let mut e = e.borrow_mut();
            e.set_wrap(cols);
            self.tab = e.options.tab_size;
        }
        Geometry {
            metrics,
            width,
            height,
            gutter_w,
            fold_x: gutter_w - FOLD_COL_W * s,
            text_x,
            text_w: (minimap_x - text_x).max(1.0),
            minimap_x,
            minimap_w,
            scrollbar_x,
            rows_visible: (height / metrics.line_h).ceil() as usize + 1,
        }
    }

    // ----- scrolling ------------------------------------------------------------------

    fn max_scroll(&mut self) -> f64 {
        let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
        let total = self.total_rows() as f64;
        (total - (g.height / g.metrics.line_h) as f64 + 1.0).max(0.0)
    }

    pub fn scroll_to_row(&mut self, row: f64, instant: bool) {
        let max = self.max_scroll();
        self.scroll_target = row.clamp(0.0, max);
        if instant {
            self.scroll_row = self.scroll_target;
        }
    }

    pub fn scroll_by_rows(&mut self, rows: f64) {
        let t = self.scroll_target + rows;
        self.scroll_to_row(t, false);
    }

    /// Scrolls so the primary cursor is visible (with a 2-row margin).
    pub fn scroll_to_cursor(&mut self) {
        self.scroll_to_cursor_within(2.0);
    }

    /// Scrolls so the primary cursor is visible with `margin` rows around it. A press
    /// of the pointer uses 0: the text under the pointer must not move away from it.
    fn scroll_to_cursor_within(&mut self, margin: f64) {
        let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
        let visible = (g.height / g.metrics.line_h).floor().max(1.0) as f64;
        let Some(row) = self.cursor_row() else { return };
        let row = row as f64;
        let top = self.scroll_target;
        let margin: f64 = if visible > 6.0 { margin } else { 0.0 };
        if row < top + margin.min(1.0) {
            self.scroll_to_row((row - margin).max(0.0), false);
        } else if row > top + visible - 1.0 - margin {
            self.scroll_to_row(row - visible + 1.0 + margin, false);
        }
        // horizontal
        if self.config.wrap {
            self.scroll_x = 0.0;
            return;
        }
        let col = self.editor_mut().map(|mut e| {
            let head = e.selections().primary().head;
            e.row_and_cell(head).1 as f32
        });
        if let Some(col) = col {
            let x = col * g.metrics.cell_w;
            let margin = g.metrics.cell_w * 4.0;
            if x < self.scroll_x + margin {
                self.scroll_x = (x - margin).max(0.0);
            } else if x > self.scroll_x + g.text_w - margin {
                self.scroll_x = x - g.text_w + margin;
            }
        }
    }

    fn cursor_row(&mut self) -> Option<usize> {
        let e = self.shared()?;
        if e.borrow().mode() == Mode::Review {
            let line = {
                let e = e.borrow();
                e.document()
                    .offset_to_position(e.selections().primary().head)
                    .line
            };
            self.ensure_diff_rows();
            self.diff
                .rows
                .iter()
                .position(|r| r.base_line() == Some(line))
        } else {
            Some(e.borrow_mut().cursor_row())
        }
    }

    /// Advances smooth scrolling; returns true while still moving.
    fn animate(&mut self, now_ms: u64) -> bool {
        let dt = now_ms.saturating_sub(self.last_frame_ms).min(100) as f32 / 16.0;
        self.last_frame_ms = now_ms;
        let diff = self.scroll_target - self.scroll_row;
        if diff.abs() < 0.002 {
            self.scroll_row = self.scroll_target;
            return false;
        }
        let k = (1.0 - (1.0 - self.config.scroll_smoothing).powf(dt.max(0.1))) as f64;
        self.scroll_row += diff * k;
        true
    }

    // ----- input --------------------------------------------------------------------

    /// Converts a logical point to a document position / control hit.
    pub fn hit_test(&mut self, x: f32, y: f32) -> Hit {
        let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
        let s = self.scale();
        let (dx, dy) = (x * s, y * s);
        if dx < 0.0 || dy < 0.0 || dx > g.width || dy > g.height {
            return Hit::Outside;
        }
        for h in &self.hits {
            if dx >= h.x && dx < h.x + h.w && dy >= h.y && dy < h.y + h.h {
                return match h.kind {
                    HitKind::Fold(line) => Hit::FoldToggle(line),
                    HitKind::HunkAccept(i) => {
                        Hit::HunkAccept(self.diff_hunk_ids.get(i).cloned().unwrap_or_default())
                    }
                    HitKind::HunkReject(i) => {
                        Hit::HunkReject(self.diff_hunk_ids.get(i).cloned().unwrap_or_default())
                    }
                    HitKind::Expand => Hit::ExpandContext,
                    HitKind::Minimap => Hit::Minimap((dy / g.height).clamp(0.0, 1.0)),
                    HitKind::Scrollbar => Hit::Scrollbar,
                };
            }
        }
        let row = ((dy / g.metrics.line_h) as f64 + self.scroll_row)
            .floor()
            .max(0.0) as usize;
        let col = ((dx - g.text_x + self.scroll_x) / g.metrics.cell_w)
            .round()
            .max(0.0) as usize;
        match &self.content {
            Content::Editor(e) => {
                let e = e.clone();
                let line = if e.borrow().mode() == Mode::Review {
                    self.ensure_diff_rows();
                    match self.diff.rows.get(row).and_then(|r| r.base_line()) {
                        Some(l) => l,
                        None => return Hit::Outside,
                    }
                } else {
                    // the buffer knows where its characters are: wide ones, clusters,
                    // tabs and wrapped rows
                    let cells = (dx - g.text_x + self.scroll_x) / g.metrics.cell_w;
                    let pos = e.borrow_mut().position_at(row, cells);
                    return if dx < g.gutter_w {
                        Hit::Gutter(pos.line)
                    } else {
                        Hit::Text(pos)
                    };
                };
                if dx < g.gutter_w {
                    Hit::Gutter(line)
                } else {
                    Hit::Text(Position::new(line, col))
                }
            }
            Content::Log { doc, .. } => {
                let line = row.min(doc.line_count().saturating_sub(1) as usize);
                if dx < g.gutter_w {
                    Hit::Gutter(line)
                } else {
                    Hit::Text(Position::new(line, col))
                }
            }
            Content::Empty => Hit::Outside,
        }
    }

    /// Caret rectangle in logical px (for the IME overlay), if a cursor is visible.
    pub fn caret_rect(&mut self) -> Option<(f32, f32, f32, f32)> {
        let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
        let s = self.scale();
        let row = self.cursor_row()? as f64;
        let col = {
            let mut e = self.editor_mut()?;
            let head = e.selections().primary().head;
            if e.mode() == Mode::Review {
                e.document().offset_to_position(head).col as f32
            } else {
                e.row_and_cell(head).1 as f32
            }
        };
        let x = (g.text_x + col * g.metrics.cell_w - self.scroll_x) / s;
        let y = ((row - self.scroll_row) as f32 * g.metrics.line_h) / s;
        Some((x, y, g.metrics.cell_w / s, g.metrics.line_h / s))
    }

    pub fn selected_text(&self) -> String {
        match self.editor() {
            Some(e) => {
                let doc = e.document();
                e.selections()
                    .iter()
                    .filter(|s| !s.is_caret())
                    .map(|s| doc.slice(s.range()))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            None => String::new(),
        }
    }

    /// What Copy puts on the clipboard (the line of the caret when nothing is selected).
    pub fn copy_text(&self) -> String {
        self.editor().map(|e| e.copy_text()).unwrap_or_default()
    }

    /// Cut: removes and returns what Copy would return.
    pub fn cut(&mut self, now_ms: u64) -> (String, Vec<HostRequest>) {
        let mut out = Vec::new();
        let text = match self.editor_mut() {
            Some(mut e) => {
                e.tick(now_ms);
                e.cut().unwrap_or_default()
            }
            None => String::new(),
        };
        self.minimap_dirty_at = now_ms;
        self.after_cursor_move(&mut out);
        (text, out)
    }

    /// Runs a command of the core and keeps the caret in view.
    pub fn run(&mut self, cmd: Command, now_ms: u64) -> Vec<HostRequest> {
        let mut out = Vec::new();
        if let Some(mut e) = self.editor_mut() {
            e.tick(now_ms);
            match e.execute(cmd) {
                Ok(Outcome::Save) => out.push(HostRequest::Save),
                Ok(Outcome::Apply) => out.push(HostRequest::Apply),
                Ok(Outcome::Leave) => out.push(HostRequest::Leave),
                Ok(Outcome::Custom(id)) => out.push(HostRequest::Custom(id)),
                Ok(Outcome::None) => {}
                Err(err) => tracing::debug!(%err, "command rejected"),
            }
        }
        self.minimap_dirty_at = now_ms;
        self.after_cursor_move(&mut out);
        out
    }

    pub fn handle(&mut self, ev: InputEvent, now_ms: u64) -> Vec<HostRequest> {
        let mut out = Vec::new();
        match ev {
            InputEvent::FocusIn => {
                self.focused = true;
                self.blink_origin_ms = now_ms;
            }
            InputEvent::FocusOut => {
                // A drag keeps going: the host holds the pointer until it is released,
                // wherever focus went meanwhile.
                self.focused = false;
                self.composition = None;
            }
            InputEvent::Wheel { dx, dy, mods: _ } => {
                let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
                let s = self.scale();
                let rows = (dy * s / g.metrics.line_h) as f64;
                // A trackpad sends small, frequent deltas that already carry momentum:
                // follow them exactly. A wheel notch is eased.
                if dy.abs() < 40.0 {
                    let t = self.scroll_target + rows;
                    self.scroll_to_row(t, true);
                } else {
                    self.scroll_by_rows(rows);
                }
                self.scroll_x = (self.scroll_x + dx * s).max(0.0);
                out.push(HostRequest::Redraw);
            }
            InputEvent::Zoom(f) => {
                self.config.font_size = (self.config.font_size * f).clamp(8.0, 40.0);
                self.geometry = None;
                out.push(HostRequest::Redraw);
            }
            InputEvent::PointerDown {
                x,
                y,
                button,
                clicks,
                mods,
            } => {
                if button != 0 {
                    return out;
                }
                self.blink_origin_ms = now_ms;
                match self.hit_test(x, y) {
                    Hit::Text(pos) => {
                        if let Some(mut e) = self.editor_mut() {
                            match clicks {
                                2 => e.double_click(pos),
                                3 => e.triple_click(pos),
                                _ if mods.alt && mods.shift => {
                                    let a = e.selections().primary().anchor;
                                    let anchor = e.document().offset_to_position(a);
                                    e.column_select(anchor, pos);
                                }
                                _ => e.click(pos, mods.shift, mods.alt || mods.primary),
                            }
                        }
                        self.drag = Some(match clicks {
                            2 => Drag::Words { anchor: pos },
                            3 => Drag::Lines { anchor: pos.line },
                            _ if mods.alt => Drag::Column { anchor: pos },
                            _ => Drag::Text,
                        });
                        self.blink_origin_ms = self.last_frame_ms;
                        self.scroll_to_cursor_within(0.0);
                        if let Some((x, y, w, h)) = self.caret_rect() {
                            out.push(HostRequest::CaretMoved { x, y, w, h });
                        }
                    }
                    Hit::Gutter(line) => {
                        if let Some(mut e) = self.editor_mut() {
                            e.triple_click(Position::new(line, 0));
                        }
                        self.drag = Some(Drag::Lines { anchor: line });
                    }
                    Hit::FoldToggle(line) => {
                        if let Some(mut e) = self.editor_mut() {
                            e.toggle_fold(line);
                        }
                    }
                    Hit::HunkAccept(id) => {
                        let index = self.diff_hunk_ids.iter().position(|h| *h == id);
                        let accepted = self
                            .editor()
                            .is_some_and(|e| e.accepted_hunks().contains(&id));
                        if accepted {
                            // pressing Accepted again takes the decision back
                            if let Some(mut e) = self.editor_mut() {
                                let _ = e.set_hunk(&id, false);
                            }
                        } else {
                            self.decide_change(index, true);
                        }
                        self.diff_focus = index;
                    }
                    Hit::HunkReject(id) => {
                        let index = self.diff_hunk_ids.iter().position(|h| *h == id);
                        if self.diff_rejected.contains(&id) {
                            self.diff_rejected.remove(&id);
                        } else {
                            self.decide_change(index, false);
                        }
                        self.diff_focus = index;
                    }
                    Hit::ExpandContext => {
                        self.diff_expanded = true;
                    }
                    Hit::Minimap(frac) => {
                        let total = self.total_rows() as f64;
                        self.scroll_to_row(frac as f64 * total - 10.0, false);
                        self.drag = Some(Drag::Minimap);
                    }
                    Hit::Scrollbar => {
                        let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
                        let total = self.total_rows().max(1) as f64;
                        let frac = (y * self.scale() / g.height) as f64;
                        let visible = (g.height / g.metrics.line_h) as f64;
                        self.scroll_to_row(frac * total - visible / 2.0, true);
                        self.drag = Some(Drag::Scrollbar { grab_offset: 0.0 });
                    }
                    Hit::Outside => {}
                }
                out.push(HostRequest::Redraw);
            }
            InputEvent::PointerMove { x, y } => match self.drag {
                Some(
                    Drag::Text | Drag::Words { .. } | Drag::Lines { .. } | Drag::Column { .. },
                ) => {
                    // the pointer may be outside the view (it is held by the host)
                    let cx = x.clamp(0.0, self.width - 1.0);
                    let cy = y.clamp(0.0, self.height - 1.0);
                    if let Hit::Text(pos) = self.hit_test(cx, cy).with_line_pos() {
                        if let Some(mut e) = self.editor_mut() {
                            match self.drag {
                                Some(Drag::Words { anchor }) => e.drag_words(anchor, pos),
                                Some(Drag::Lines { anchor }) => e.drag_lines(anchor, pos.line),
                                Some(Drag::Column { anchor }) => {
                                    if anchor != pos {
                                        e.column_select(anchor, pos);
                                    }
                                }
                                _ => e.drag(pos),
                            }
                        }
                        if let Some((cx, cy, w, h)) = self.caret_rect() {
                            out.push(HostRequest::CaretMoved { x: cx, y: cy, w, h });
                        }
                    }
                    // autoscroll when dragging past the edges
                    if y < 0.0 {
                        self.scroll_by_rows(-1.0 - (-y / 40.0) as f64);
                    } else if y > self.height {
                        self.scroll_by_rows(1.0 + ((y - self.height) / 40.0) as f64);
                    }
                    out.push(HostRequest::Redraw);
                }
                Some(Drag::Minimap) | Some(Drag::Scrollbar { .. }) => {
                    let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
                    let total = self.total_rows().max(1) as f64;
                    let frac = (y * self.scale() / g.height).clamp(0.0, 1.0) as f64;
                    let visible = (g.height / g.metrics.line_h) as f64;
                    self.scroll_to_row(frac * total - visible / 2.0, true);
                    out.push(HostRequest::Redraw);
                }
                None => {}
            },
            InputEvent::PointerUp { .. } => {
                self.drag = None;
            }
            InputEvent::Text(t) => {
                self.composition = None;
                if let Some(mut e) = self.editor_mut() {
                    e.tick(now_ms);
                    if let Err(err) = e.insert(&t) {
                        tracing::debug!(%err, "text rejected");
                    }
                }
                self.minimap_dirty_at = now_ms;
                self.after_cursor_move(&mut out);
            }
            InputEvent::Paste(t) => {
                self.composition = None;
                if let Some(mut e) = self.editor_mut() {
                    e.tick(now_ms);
                    if let Err(err) = e.paste(&t) {
                        tracing::debug!(%err, "paste rejected");
                    }
                }
                self.minimap_dirty_at = now_ms;
                self.after_cursor_move(&mut out);
            }
            InputEvent::CompositionStart => self.composition = Some(String::new()),
            InputEvent::CompositionUpdate(s) => {
                self.composition = Some(s);
                out.push(HostRequest::Redraw);
            }
            InputEvent::CompositionEnd(s) => {
                self.composition = None;
                if !s.is_empty()
                    && let Some(mut e) = self.editor_mut()
                {
                    e.tick(now_ms);
                    // composed text is text, never half of a bracket pair
                    let pairs = std::mem::replace(&mut e.options.auto_close, false);
                    let _ = e.insert(&s);
                    e.options.auto_close = pairs;
                }
                self.minimap_dirty_at = now_ms;
                self.after_cursor_move(&mut out);
            }
            InputEvent::Key(key) => {
                self.handle_key(key, now_ms, &mut out);
            }
        }
        out
    }

    fn handle_key(&mut self, key: Key, now_ms: u64, out: &mut Vec<HostRequest>) {
        // Hosts with a platform clipboard event (the web overlay) never send these
        // keys; hosts without one (winit) do.
        if key.mods.primary && !key.mods.alt && !key.mods.shift {
            match key.code {
                KeyCode::Char('c') => {
                    out.push(HostRequest::Copy(self.copy_text()));
                    return;
                }
                KeyCode::Char('x') => {
                    let (t, reqs) = self.cut(now_ms);
                    out.push(HostRequest::Copy(t));
                    out.extend(reqs);
                    return;
                }
                KeyCode::Char('v') => {
                    out.push(HostRequest::Paste);
                    return;
                }
                _ => {}
            }
        }
        if self.in_review() && key.code == KeyCode::F(7) && !key.mods.primary {
            self.go_to_change(!key.mods.shift);
            out.push(HostRequest::Redraw);
            return;
        }
        let g = self.geometry.unwrap_or_else(|| self.compute_geometry());
        let page = (g.height / g.metrics.line_h).floor().max(1.0) as f64;
        match &mut self.content {
            Content::Editor(e) => {
                {
                    let mut e = e.borrow_mut();
                    e.options.page_rows = (page as usize).saturating_sub(1).max(1);
                    e.tick(now_ms);
                    match e.key(&key) {
                        Ok(Some(Outcome::Save)) => out.push(HostRequest::Save),
                        Ok(Some(Outcome::Apply)) => out.push(HostRequest::Apply),
                        Ok(Some(Outcome::Leave)) => out.push(HostRequest::Leave),
                        Ok(Some(Outcome::Custom(id))) => out.push(HostRequest::Custom(id)),
                        Ok(_) => {}
                        Err(err) => tracing::debug!(%err, "key rejected"),
                    }
                }
                self.minimap_dirty_at = now_ms;
                self.after_cursor_move(out);
            }
            Content::Log { .. } => {
                let rows = match (&key.code, key.mods.primary) {
                    (KeyCode::Down, false) => 1.0,
                    (KeyCode::Up, false) => -1.0,
                    (KeyCode::PageDown, _) => page,
                    (KeyCode::PageUp, _) => -page,
                    (KeyCode::End, _) | (KeyCode::Down, true) => f64::MAX / 4.0,
                    (KeyCode::Home, _) | (KeyCode::Up, true) => f64::MIN / 4.0,
                    (KeyCode::Escape, _) => {
                        out.push(HostRequest::Leave);
                        return;
                    }
                    _ => 0.0,
                };
                if rows != 0.0 {
                    self.scroll_by_rows(rows);
                    out.push(HostRequest::Redraw);
                }
            }
            Content::Empty => {}
        }
    }

    fn after_cursor_move(&mut self, out: &mut Vec<HostRequest>) {
        self.blink_origin_ms = self.last_frame_ms;
        self.scroll_to_cursor();
        if let Some((x, y, w, h)) = self.caret_rect() {
            out.push(HostRequest::CaretMoved { x, y, w, h });
        }
        out.push(HostRequest::Redraw);
    }

    /// Brings the caret into view after the buffer was changed from outside this view
    /// (another view of the same buffer, the store).
    pub fn reveal_cursor(&mut self) {
        self.scroll_to_cursor();
    }

    /// True when a frame should be drawn again soon (blink, smooth scroll).
    pub fn needs_redraw(&self, now_ms: u64) -> bool {
        if self.animating {
            return true;
        }
        if self.focused && self.config.cursor_blink && matches!(self.content, Content::Editor(_)) {
            // redraw at blink edges only
            let phase = now_ms.saturating_sub(self.blink_origin_ms) % (self.config.blink_ms * 2);
            return phase < 40
                || (phase >= self.config.blink_ms && phase < self.config.blink_ms + 40);
        }
        false
    }

    /// True while a smooth scroll is still moving.
    pub fn is_animating(&self) -> bool {
        self.animating || (self.scroll_target - self.scroll_row).abs() > 0.002
    }

    /// Milliseconds until the caret's next blink edge, when it blinks.
    pub fn next_blink_in(&self, now_ms: u64) -> Option<u64> {
        if !(self.focused && self.config.cursor_blink && matches!(self.content, Content::Editor(_)))
        {
            return None;
        }
        let b = self.config.blink_ms.max(1);
        Some(b - now_ms.saturating_sub(self.blink_origin_ms) % b)
    }

    // ----- frame ------------------------------------------------------------------------

    /// Builds the instance list for this frame (CPU only). Hosts that render through
    /// `prepare`/`render` need not call it.
    pub fn build_frame(&mut self, now_ms: u64) -> &[Instance] {
        let t0 = now();
        self.animating = self.animate(now_ms);
        let g = self.compute_geometry();
        self.geometry = Some(g);
        let max = self.max_scroll();
        self.scroll_target = self.scroll_target.clamp(0.0, max);
        self.scroll_row = self.scroll_row.clamp(0.0, max);
        self.instances.clear();
        self.hits.clear();
        let th = self.theme.clone();
        let mut glyphs = 0usize;
        let mut rows_drawn = 0usize;
        // backgrounds
        self.instances
            .push(Instance::solid(0.0, 0.0, g.width, g.height, th.bg_1));
        self.instances
            .push(Instance::solid(0.0, 0.0, g.gutter_w, g.height, th.bg_0));
        let first_row = self.scroll_row.floor() as usize;
        // Rows sit on whole device pixels, so glyphs are never resampled. A row's place
        // is rounded from its place in the whole document, not from the first row on
        // screen: a scroll by n pixels then moves every row by exactly n, and a host that
        // presents frames as images can move the old one instead of sending it again.
        let lh = g.metrics.line_h as f64;
        let scroll_px = (self.scroll_row * lh).round();
        let y_of =
            |row_index: usize| (((first_row + row_index) as f64 * lh).round() - scroll_px) as f32;
        let blink_on = !self.config.cursor_blink
            || now_ms.saturating_sub(self.blink_origin_ms) % (self.config.blink_ms * 2)
                < self.config.blink_ms;
        let composition = self.composition.clone();

        match std::mem::replace(&mut self.content, Content::Empty) {
            Content::Empty => {}
            Content::Log {
                mut doc,
                mut highlighter,
            } => {
                let total = doc.line_count() as usize;
                let lines = doc.lines(
                    first_row as u64,
                    g.rows_visible.min(total.saturating_sub(first_row)),
                );
                let _ = doc.take_missing();
                for (i, line) in lines.into_iter().enumerate() {
                    let line_no = first_row + i;
                    let y = y_of(i);
                    let text = line.unwrap_or_else(|| "…".to_string());
                    let tmp = frontier_editor::Document::from_str(&text);
                    let spans = highlighter.spans(&tmp, Range::new(0, 1));
                    let row_spans: Vec<(Range, TokenKind)> =
                        spans.iter().map(|s| (s.range, s.kind)).collect();
                    glyphs += self.draw_text_row(&g, y, &text, &row_spans, th.fg_1);
                    self.draw_line_number(&g, y, line_no + 1, false);
                    rows_drawn += 1;
                }
                self.draw_scrollbar(&g, total);
                self.content = Content::Log { doc, highlighter };
            }
            Content::Editor(shared) => {
                self.content = Content::Editor(shared.clone());
                let review = shared.borrow().mode() == Mode::Review;
                if review {
                    self.ensure_diff_rows();
                    let mut e = shared.borrow_mut();
                    let (gl, rd) = self.draw_review(&g, &mut e, first_row, y_of, blink_on, &th);
                    glyphs += gl;
                    rows_drawn += rd;
                    drop(e);
                    self.draw_scrollbar(&g, self.diff.rows.len());
                } else {
                    let mut e = shared.borrow_mut();
                    e.poll(now_ms);
                    let vs = e.view_state(Viewport {
                        first_row,
                        rows: g.rows_visible,
                    });
                    let cur_line = vs.primary.line;
                    for (i, row) in vs.rows.iter().enumerate() {
                        let y = y_of(i);
                        glyphs +=
                            self.draw_view_line(&g, y, row, &th, blink_on, composition.as_deref());
                        if !row.continuation {
                            self.draw_line_number(&g, y, row.line + 1, row.line == cur_line);
                        }
                        rows_drawn += 1;
                    }
                    self.draw_minimap(&g, &mut e, &vs, now_ms, &th);
                    self.draw_scrollbar(&g, vs.total_rows);
                }
            }
        }
        self.stats = FrameStats {
            rows: rows_drawn,
            instances: self.instances.len(),
            glyphs,
            build_ms: (now() - t0) * 1e3,
            atlas_glyphs: self.text.atlas.len(),
            atlas_bytes: self.text.atlas.bytes_used(),
            shape_cache: self.text.shape_stats().2,
        };
        &self.instances
    }

    fn draw_line_number(&mut self, g: &Geometry, y: f32, n: usize, current: bool) {
        if !self.config.line_numbers {
            return;
        }
        let s = n.to_string();
        let w = s.chars().count() as f32 * g.metrics.cell_w;
        let x = g.fold_x - GUTTER_PAD * self.scale() * 0.5 - w;
        let color = if current {
            self.theme.fg_1
        } else {
            self.theme.fg_3
        };
        self.draw_glyph_run(g, x, y, &s, |_| color);
    }

    /// Draws `text` at (x, y) using `color_at(char index)`. Returns glyphs drawn.
    fn draw_glyph_run(
        &mut self,
        g: &Geometry,
        x: f32,
        y: f32,
        text: &str,
        color_at: impl Fn(usize) -> Rgba,
    ) -> usize {
        let px = g.metrics.font_size * g.metrics.scale;
        let cw = g.metrics.cell_w;
        let shaped = self.text.shape(text, px, cw);
        let baseline = y + g.metrics.ascent;
        // Plain ASCII sits where the shaper put it. Anything else (wide characters,
        // clusters, tabs) is placed on the cell grid, cluster by cluster.
        let plain = text.bytes().all(|b| (0x20..0x7f).contains(&b));
        let grid = (!plain).then(|| {
            let layout = LineLayout::new(text, self.tab);
            let mut char_of = vec![0u32; text.len() + 1];
            let mut ci = 0u32;
            for (bi, _) in text.char_indices() {
                char_of[bi] = ci;
                ci += 1;
            }
            char_of[text.len()] = ci;
            (layout, char_of)
        });
        let bytes = text.as_bytes();
        let mut n = 0;
        // the cluster being placed: its first column and the shaper's x of its first glyph
        let mut cluster: (usize, i32) = (usize::MAX, 0);
        let TextSystem {
            font_system,
            swash,
            atlas,
            ..
        } = &mut self.text;
        for glyph in &shaped.glyphs {
            if bytes.get(glyph.start) == Some(&b'\t') {
                continue;
            }
            let gx = match &grid {
                None => x + glyph.x as f32,
                Some((layout, char_of)) => {
                    let col = char_of.get(glyph.start).copied().unwrap_or(0) as usize;
                    // the boundary at or before this glyph's character
                    let start = layout.prev_boundary(col + 1);
                    if cluster.0 != start {
                        cluster = (start, glyph.x);
                    }
                    (x + layout.x(start) as f32 * cw).round() + (glyph.x - cluster.1) as f32
                }
            };
            if gx > g.width || gx + cw * 2.0 < 0.0 {
                continue;
            }
            let entry = match atlas.get(font_system, swash, glyph.key) {
                Ok(e) => e,
                Err(AtlasFull) => {
                    let size = (atlas.size * 2).min(4096);
                    atlas.clear(size);
                    atlas.get(font_system, swash, glyph.key).ok().flatten()
                }
            };
            let Some(a) = entry else { continue };
            let color = color_at(glyph.start);
            let mut inst = Instance::glyph(
                gx + a.left as f32,
                baseline - a.top as f32 + glyph.y as f32,
                a.w as f32,
                a.h as f32,
                a.uv,
                color,
            );
            if a.color {
                inst.kind = Instance::COLOR_GLYPH;
                inst.color = [1.0, 1.0, 1.0, color[3]];
            }
            self.instances.push(inst);
            n += 1;
        }
        n
    }

    /// Text row with syntax colours; returns glyphs drawn.
    fn draw_text_row(
        &mut self,
        g: &Geometry,
        y: f32,
        text: &str,
        spans: &[(Range, TokenKind)],
        default: Rgba,
    ) -> usize {
        let th = self.theme.clone();
        let x = g.text_x - self.scroll_x;
        // char index -> byte-ish char index used by shaping clusters (chars)
        let colors: Vec<Rgba> = {
            let n = text.chars().count();
            let mut v = vec![default; n];
            for (r, k) in spans {
                let c = th.token(*k);
                for slot in v.iter_mut().take(r.end.min(n)).skip(r.start) {
                    *slot = c;
                }
            }
            v
        };
        // shaping clusters report byte offsets; map bytes -> char index
        let byte_to_char: Vec<usize> = {
            let mut m = vec![0usize; text.len() + 1];
            let mut ci = 0;
            for (bi, _) in text.char_indices() {
                m[bi] = ci;
                ci += 1;
            }
            m[text.len()] = ci;
            m
        };
        self.draw_glyph_run(g, x, y, text, |start| {
            let ci = byte_to_char.get(start).copied().unwrap_or(0);
            colors.get(ci).copied().unwrap_or(default)
        })
    }

    fn draw_view_line(
        &mut self,
        g: &Geometry,
        y: f32,
        row: &ViewLine,
        th: &Theme,
        blink_on: bool,
        composition: Option<&str>,
    ) -> usize {
        let lh = g.metrics.line_h;
        let cw = g.metrics.cell_w;
        let x0 = g.text_x - self.scroll_x;
        let layout = LineLayout::new(&row.text, self.tab);
        let col_x = |c: usize| x0 + layout.x(c) as f32 * cw;
        if row.is_current && self.config.current_line {
            self.instances.push(Instance::solid(
                g.gutter_w,
                y,
                g.minimap_x - g.gutter_w,
                lh,
                th.line_1,
            ));
        }
        // hunk / diagnostic / selection / search backgrounds, in that order
        let mut sorted: Vec<&frontier_editor::view::RowHighlight> = row.highlights.iter().collect();
        sorted.sort_by_key(|h| match h.class.as_str() {
            c if c.starts_with("hunk") => 0,
            c if c.starts_with("diagnostic") => 1,
            "selection" => 2,
            "search" => 3,
            _ => 4,
        });
        for h in sorted {
            let Some(color) = th.highlight(&h.class) else {
                continue;
            };
            let (a, b) = (h.cols.start, h.cols.end.max(h.cols.start + 1));
            if h.class.starts_with("hunk") {
                self.instances.push(Instance::solid(
                    g.gutter_w,
                    y,
                    g.minimap_x - g.gutter_w,
                    lh,
                    color,
                ));
            } else if h.class.starts_with("diagnostic") {
                // underline
                self.instances.push(Instance::solid(
                    col_x(a),
                    y + lh - 2.0 * self.scale(),
                    col_x(b) - col_x(a),
                    2.0 * self.scale(),
                    alpha(color, 0.9),
                ));
            } else {
                self.instances
                    .push(Instance::solid(col_x(a), y, col_x(b) - col_x(a), lh, color));
            }
        }
        let spans: Vec<(Range, TokenKind)> = row.spans.iter().map(|s| (s.cols, s.kind)).collect();
        let default = if row.locked { th.fg_2 } else { th.fg_1 };
        let mut n = self.draw_text_row(g, y, &row.text, &spans, default);
        let len = row.text.chars().count();
        // folded placeholder
        if row.fold == FoldState::Folded {
            let label = format!("  ⋯ {} lines", row.folded_lines);
            self.instances.push(Instance::solid(
                col_x(len),
                y + 2.0,
                label.chars().count() as f32 * cw,
                lh - 4.0,
                th.bg_3,
            ));
            n += self.draw_glyph_run(g, col_x(len), y, &label, |_| th.fg_2);
        }
        // inline widgets (diagnostics, lock reasons)
        let mut wx = len + if row.fold == FoldState::Folded { 12 } else { 2 };
        for w in &row.widgets {
            let color = match w.class.as_str() {
                "diagnostic_error" => th.fail,
                "diagnostic_warning" => th.warn,
                "lock" => th.fg_3,
                _ => th.fg_2,
            };
            let text: String = w.text.chars().take(80).collect();
            n += self.draw_glyph_run(g, col_x(wx), y, &text, |_| alpha(color, 0.85));
            wx += text.chars().count() + 2;
        }
        // IME preedit at the primary cursor
        if let (Some(pre), Some(pc)) = (composition, row.primary_cursor)
            && !pre.is_empty()
        {
            let x = col_x(pc);
            self.instances.push(Instance::solid(
                x,
                y,
                pre.chars().count() as f32 * cw,
                lh,
                th.accent_bg,
            ));
            n += self.draw_glyph_run(g, x, y, pre, |_| th.fg_0);
            self.instances.push(Instance::solid(
                x,
                y + lh - 1.0 * self.scale(),
                pre.chars().count() as f32 * cw,
                self.scale(),
                th.accent,
            ));
        }
        // cursors
        if blink_on && self.focused {
            for c in &row.cursors {
                let is_primary = row.primary_cursor == Some(*c);
                let color = if is_primary {
                    th.accent
                } else {
                    alpha(th.accent, 0.6)
                };
                let shift = composition.map(|p| p.chars().count()).unwrap_or(0) as f32 * cw;
                let x = (col_x(*c) + shift).round();
                let bar = (CURSOR_W * self.scale()).round().max(1.0);
                let (cx, cy, w, h, color) = match self.config.cursor {
                    CursorStyle::Bar => (x - (bar / 2.0).floor(), y, bar, lh, color),
                    CursorStyle::Block => (x, y, cw, lh, alpha(color, 0.45)),
                    CursorStyle::Underline => (x, y + lh - bar, cw, bar, color),
                };
                self.instances.push(Instance::solid(cx, cy, w, h, color));
            }
        } else if !self.focused {
            // an unfocused view still shows where typing would go
            for c in row.primary_cursor.iter() {
                let bar = (self.scale()).round().max(1.0);
                self.instances.push(Instance::solid(
                    col_x(*c).round(),
                    y,
                    bar,
                    lh,
                    alpha(th.fg_3, 0.7),
                ));
            }
        }
        // gutter marks
        let gx = GUTTER_PAD * self.scale() * 0.3;
        for m in &row.gutter {
            match m {
                GutterMark::Diagnostic { severity } => {
                    let d = 5.0 * self.scale();
                    self.instances.push(Instance::solid(
                        gx,
                        y + (lh - d) / 2.0,
                        d,
                        d,
                        th.severity(*severity),
                    ));
                }
                GutterMark::Hunk { kind } => {
                    self.instances.push(Instance::solid(
                        0.0,
                        y,
                        3.0 * self.scale(),
                        lh,
                        th.hunk(*kind),
                    ));
                }
                GutterMark::Lock => {
                    let d = 4.0 * self.scale();
                    self.instances
                        .push(Instance::solid(gx, y + (lh - d) / 2.0, d, d, th.fg_3));
                }
                GutterMark::SearchMatch => {
                    self.instances.push(Instance::solid(
                        g.gutter_w - 2.0 * self.scale(),
                        y,
                        2.0 * self.scale(),
                        lh,
                        th.warn,
                    ));
                }
            }
        }
        // fold chevron
        if row.fold != FoldState::None {
            let glyph = if row.fold == FoldState::Folded {
                "▸"
            } else {
                "▾"
            };
            let color = if row.fold == FoldState::Folded {
                th.accent
            } else {
                th.fg_3
            };
            n += self.draw_glyph_run(g, g.fold_x + 2.0 * self.scale(), y, glyph, |_| color);
            self.hits.push(HitRegion {
                x: g.fold_x,
                y,
                w: FOLD_COL_W * self.scale(),
                h: lh,
                kind: HitKind::Fold(row.line),
            });
        }
        n
    }

    /// Text at an absolute x (a pane of a comparison), cut to `max_cols` columns.
    #[allow(clippy::too_many_arguments)]
    fn draw_text_at(
        &mut self,
        g: &Geometry,
        x: f32,
        y: f32,
        text: &str,
        max_cols: usize,
        hl: &mut Highlighter,
        color: Option<Rgba>,
        default: Rgba,
    ) -> usize {
        let cut: String;
        let text = if text.chars().count() > max_cols {
            cut = text.chars().take(max_cols).collect();
            cut.as_str()
        } else {
            text
        };
        let spans: Vec<(Range, TokenKind)> = match color {
            Some(_) => Vec::new(),
            None => {
                let tmp = frontier_editor::Document::from_str(text);
                hl.spans(&tmp, Range::new(0, 1))
                    .iter()
                    .map(|s: &Span| (s.range, s.kind))
                    .collect()
            }
        };
        let saved = self.scroll_x;
        self.scroll_x = -(x - g.text_x);
        let n = self.draw_text_row(g, y, text, &spans, color.unwrap_or(default));
        self.scroll_x = saved;
        n
    }

    /// A number whose last digit ends at `right`.
    fn draw_number_right(
        &mut self,
        g: &Geometry,
        right: f32,
        y: f32,
        n: usize,
        color: Rgba,
    ) -> usize {
        let s = n.to_string();
        let x = right - s.len() as f32 * g.metrics.cell_w;
        self.draw_glyph_run(g, x, y, &s, |_| color)
    }

    /// A comparison: the old and the new text, inline or side by side, with what
    /// changed inside a line marked, a state bar and (for a proposal) Accept / Reject
    /// on the first row of each change, and every change marked beside the scrollbar.
    fn draw_review(
        &mut self,
        g: &Geometry,
        e: &mut Editor,
        first_row: usize,
        y_of: impl Fn(usize) -> f32,
        _blink_on: bool,
        th: &Theme,
    ) -> (usize, usize) {
        let s = self.scale();
        let lh = g.metrics.line_h;
        let cw = g.metrics.cell_w;
        let sbs = self.side_by_side();
        let actions = self.config.review_actions;
        let doc = e.document().clone();
        let mut hl = Highlighter::new_lexer(e.language());
        let accepted = e.accepted_hunks();
        let rows: Vec<DiffRow> = self
            .diff
            .rows
            .iter()
            .skip(first_row)
            .take(g.rows_visible)
            .cloned()
            .collect();
        let bar = REVIEW_BAR_W * s;
        let numw = self.review_numw;
        let pad = GUTTER_PAD * s * 0.5;
        let lane_x = g.scrollbar_x - CHANGE_LANE_W * s;
        let right_edge = lane_x;
        // the gutter of a comparison has no fold column
        self.instances
            .push(Instance::solid(0.0, 0.0, g.width, g.height, th.bg_1));
        let pane_w = if sbs {
            ((right_edge - bar) / 2.0).floor()
        } else {
            right_edge - bar
        };
        // per pane: where its number column ends and its text starts
        let panes: [(f32, f32, f32); 2] = if sbs {
            let p1 = bar + pane_w;
            [
                (bar, bar + numw - pad, bar + numw + cw * 2.0),
                (p1, p1 + numw - pad, p1 + numw + cw * 2.0),
            ]
        } else {
            let t = bar + numw * 2.0 + cw * 2.0;
            [(bar, bar + numw - pad, t), (bar, bar + numw * 2.0 - pad, t)]
        };
        let number_bg_w = if sbs { numw } else { numw * 2.0 };
        self.instances.push(Instance::solid(
            0.0,
            0.0,
            bar + number_bg_w,
            g.height,
            th.bg_0,
        ));
        if sbs {
            self.instances
                .push(Instance::solid(panes[1].0, 0.0, numw, g.height, th.bg_0));
        }
        let mut glyphs = 0;
        let mut drawn = 0;
        for (i, row) in rows.iter().enumerate() {
            let y = y_of(i);
            drawn += 1;
            let (left, right, hunk, first) = match row {
                DiffRow::Collapsed { count, .. } => {
                    self.instances
                        .push(Instance::solid(0.0, y, right_edge, lh, th.bg_0));
                    self.instances.push(Instance::solid(
                        0.0,
                        y + lh - s.max(1.0),
                        right_edge,
                        s.max(1.0),
                        th.line_2,
                    ));
                    let label = format!(
                        "⋯ {count} unchanged {}",
                        if *count == 1 { "line" } else { "lines" }
                    );
                    let x = ((right_edge - label.chars().count() as f32 * cw) / 2.0).max(bar);
                    glyphs += self.draw_glyph_run(g, x, y, &label, |_| th.fg_2);
                    self.hits.push(HitRegion {
                        x: 0.0,
                        y,
                        w: right_edge,
                        h: lh,
                        kind: HitKind::Expand,
                    });
                    continue;
                }
                DiffRow::Line {
                    left,
                    right,
                    hunk,
                    first,
                } => (left, right, *hunk, *first),
            };
            let id = hunk.and_then(|h| self.diff_hunk_ids.get(h).cloned());
            let is_accepted = id.as_ref().is_some_and(|id| accepted.contains(id));
            let is_rejected = id
                .as_ref()
                .is_some_and(|id| self.diff_rejected.contains(id));
            let focused = hunk.is_some() && hunk == self.diff_focus;
            // one cell of the row in one pane
            let mut cell = |this: &mut Self, c: &Cell, pane: usize, glyphs: &mut usize| {
                let (px, num_right, tx) = panes[pane];
                let width = if sbs { pane_w } else { right_edge - bar };
                let (tint, mark, mark_color) = match c.kind {
                    CellKind::Context => (None, "", th.fg_3),
                    CellKind::Delete => (Some(th.fail), "−", th.fail),
                    CellKind::Insert => (Some(th.ok), "+", th.ok),
                };
                // a change that was turned down is shown without its colours; an accepted
                // one keeps the new text and quietens the old
                let quiet = match c.kind {
                    CellKind::Insert => is_rejected,
                    CellKind::Delete => is_accepted,
                    CellKind::Context => false,
                };
                if let Some(t) = tint {
                    let a = if quiet { 0.05 } else { 0.13 };
                    this.instances
                        .push(Instance::solid(px, y, width, lh, alpha(t, a)));
                    if !quiet {
                        for r in &c.emphasis {
                            this.instances.push(Instance::solid(
                                tx + r.start as f32 * cw,
                                y,
                                r.len() as f32 * cw,
                                lh,
                                alpha(t, 0.30),
                            ));
                        }
                    }
                    *glyphs += this.draw_glyph_run(g, tx - cw * 1.5, y, mark, |_| {
                        if quiet { th.fg_3 } else { mark_color }
                    });
                }
                let number_color = if tint.is_some() && !quiet {
                    th.fg_1
                } else {
                    th.fg_3
                };
                *glyphs += this.draw_number_right(g, num_right, y, c.line + 1, number_color);
                let base;
                let text = match &c.text {
                    Some(t) => t.as_str(),
                    None => {
                        // context: the same line in both texts; the base document has it
                        base = match (left, c.kind) {
                            (Some(l), CellKind::Context) => doc.line(l.line),
                            _ => String::new(),
                        };
                        base.as_str()
                    }
                };
                let max_cols = (((px + width) - tx) / cw).floor().max(0.0) as usize;
                let color = quiet.then_some(th.fg_3);
                *glyphs += this.draw_text_at(g, tx, y, text, max_cols, &mut hl, color, th.fg_1);
            };
            if sbs {
                for (pane, side) in [left, right].into_iter().enumerate() {
                    match side {
                        Some(c) => cell(self, c, pane, &mut glyphs),
                        None => {
                            // nothing on this side of the change
                            let px = panes[pane].0;
                            self.instances.push(Instance::solid(
                                px,
                                y,
                                pane_w,
                                lh,
                                alpha(th.bg_0, 0.9),
                            ));
                        }
                    }
                }
            } else {
                match (left, right) {
                    // context: one text, both numbers
                    (Some(l), Some(r)) => {
                        cell(self, l, 0, &mut glyphs);
                        glyphs += self.draw_number_right(g, panes[1].1, y, r.line + 1, th.fg_3);
                    }
                    (Some(l), None) => cell(self, l, 0, &mut glyphs),
                    (None, Some(r)) => cell(self, r, 1, &mut glyphs),
                    (None, None) => {}
                }
            }
            // the state of the change, down its left edge
            if hunk.is_some() {
                let color = if is_accepted {
                    th.ok
                } else if is_rejected {
                    th.fg_3
                } else if focused {
                    th.accent
                } else {
                    alpha(th.accent, 0.45)
                };
                self.instances.push(Instance::solid(0.0, y, bar, lh, color));
            }
            if first
                && actions
                && let Some(h) = hunk
            {
                let labels: [(&str, bool, Rgba); 2] = [
                    (
                        if is_accepted {
                            "✓ Accepted"
                        } else {
                            "Accept"
                        },
                        is_accepted,
                        th.ok,
                    ),
                    (
                        if is_rejected { "× Rejected" } else { "Reject" },
                        is_rejected,
                        th.fail,
                    ),
                ];
                let widths: Vec<f32> = labels
                    .iter()
                    .map(|(l, _, _)| (l.chars().count() + 2) as f32 * cw)
                    .collect();
                let gap = cw * 0.5;
                let total = widths.iter().sum::<f32>() + gap + cw;
                let mut x = (right_edge - total).max(bar);
                // clear what the buttons stand on
                self.instances.push(Instance::solid(
                    x - cw * 0.5,
                    y,
                    total + cw * 0.5,
                    lh,
                    th.bg_1,
                ));
                let inset = (2.0 * s).round();
                for (k, ((label, on, tone), w)) in labels.iter().zip(&widths).enumerate() {
                    let bg = if *on { alpha(*tone, 0.22) } else { th.bg_3 };
                    self.instances
                        .push(Instance::solid(x, y + inset, *w, lh - inset * 2.0, bg));
                    let fg = if *on { *tone } else { th.fg_1 };
                    glyphs += self.draw_glyph_run(g, x + cw, y, label, |_| fg);
                    self.hits.push(HitRegion {
                        x,
                        y,
                        w: *w,
                        h: lh,
                        kind: if k == 0 {
                            HitKind::HunkAccept(h)
                        } else {
                            HitKind::HunkReject(h)
                        },
                    });
                    x += w + gap;
                }
            }
        }
        if sbs {
            self.instances.push(Instance::solid(
                panes[1].0 - s.max(1.0),
                0.0,
                s.max(1.0),
                g.height,
                th.line_2,
            ));
        }
        // every change, where it is in the whole comparison
        self.instances.push(Instance::solid(
            lane_x,
            0.0,
            CHANGE_LANE_W * s,
            g.height,
            th.bg_0,
        ));
        let total = self.diff.rows.len().max(1) as f32;
        if let Some(cs) = e.changeset() {
            for (h, start) in self.diff.hunk_rows.iter().enumerate() {
                let end = self
                    .diff
                    .rows
                    .iter()
                    .skip(*start)
                    .take_while(|r| r.hunk() == Some(h))
                    .count();
                let y0 = *start as f32 / total * g.height;
                let hh = (end as f32 / total * g.height).max(3.0 * s);
                let color = cs
                    .hunks
                    .get(h)
                    .map(|x| th.hunk(x.kind))
                    .unwrap_or(th.accent);
                self.instances.push(Instance::solid(
                    lane_x + s,
                    y0,
                    CHANGE_LANE_W * s - 2.0 * s,
                    hh,
                    color,
                ));
            }
        }
        (glyphs, drawn)
    }

    fn draw_scrollbar(&mut self, g: &Geometry, total_rows: usize) {
        let visible = g.height / g.metrics.line_h;
        if total_rows as f32 <= visible {
            return;
        }
        let track_h = g.height;
        let thumb_h = (visible / total_rows as f32 * track_h).max(24.0 * self.scale());
        let thumb_y =
            (self.scroll_row as f32 / (total_rows as f32 - visible + 1.0)) * (track_h - thumb_h);
        let th = &self.theme;
        self.instances.push(Instance::solid(
            g.scrollbar_x,
            0.0,
            SCROLLBAR_W * self.scale(),
            track_h,
            th.bg_0,
        ));
        self.instances.push(Instance::solid(
            g.scrollbar_x + 2.0 * self.scale(),
            thumb_y,
            SCROLLBAR_W * self.scale() - 4.0 * self.scale(),
            thumb_h,
            alpha(th.fg_3, 0.6),
        ));
        self.hits.push(HitRegion {
            x: g.scrollbar_x,
            y: 0.0,
            w: SCROLLBAR_W * self.scale(),
            h: track_h,
            kind: HitKind::Scrollbar,
        });
    }

    /// Minimap: one 2 px row per document line (sampled when the document is taller
    /// than the strip), coloured by the dominant token; diagnostics and hunks as a
    /// heat-map on its right edge; the viewport as a translucent box.
    fn draw_minimap(
        &mut self,
        g: &Geometry,
        e: &mut Editor,
        vs: &ViewState,
        now_ms: u64,
        th: &Theme,
    ) {
        if g.minimap_w <= 0.0 {
            return;
        }
        let total = vs.total_lines.max(1);
        let rows_fit = (g.height / (MINIMAP_ROW_H * self.scale())).floor().max(1.0) as usize;
        let stride = total.div_ceil(rows_fit).max(1);
        // cache the per-line summary; rebuild at most 4×/s after edits
        let hash = e.content_hash_cheap();
        let stale = self.minimap_cache.as_ref().is_none_or(|(h, _)| *h != hash);
        if stale && now_ms.saturating_sub(self.minimap_dirty_at) >= 250
            || self.minimap_cache.is_none()
        {
            let doc = e.document();
            let lang = e.language();
            let mut summary = Vec::with_capacity(total / stride + 1);
            let mut line = 0;
            let mut spans = Vec::new();
            while line < total {
                let text = doc.line(line);
                spans.clear();
                fallback::lex_line(lang, &text, 0, &mut spans);
                let kind = spans.iter().max_by_key(|s| s.range.len()).map(|s| s.kind);
                summary.push((text.chars().count().min(200) as u16, kind));
                line += stride;
            }
            self.minimap_cache = Some((hash, summary));
        }
        self.instances.push(Instance::solid(
            g.minimap_x,
            0.0,
            g.minimap_w,
            g.height,
            th.bg_0,
        ));
        let px_per_char = (g.minimap_w - 10.0 * self.scale()) / 120.0;
        if let Some((_, summary)) = &self.minimap_cache {
            let row_h = MINIMAP_ROW_H * self.scale();
            for (i, (len, kind)) in summary.iter().enumerate() {
                if *len == 0 {
                    continue;
                }
                let y = i as f32 * row_h;
                if y > g.height {
                    break;
                }
                let w = (*len as f32).min(120.0) * px_per_char;
                self.instances.push(Instance::solid(
                    g.minimap_x + 4.0 * self.scale(),
                    y,
                    w,
                    (row_h - 0.5).max(1.0),
                    th.minimap(*kind),
                ));
            }
        }
        // heat-map: diagnostics and hunks
        let scale = g.height / total as f32;
        let hx = g.minimap_x + g.minimap_w - 4.0 * self.scale();
        for d in e.diagnostics() {
            let line = e.document().offset_to_position(d.range.start).line;
            let c = match d.severity {
                Severity::Error | Severity::Warning => th.severity(d.severity),
                _ => continue,
            };
            self.instances.push(Instance::solid(
                hx,
                line as f32 * scale,
                4.0 * self.scale(),
                (scale * 2.0).max(2.0 * self.scale()),
                c,
            ));
        }
        if let Some(cs) = e.changeset() {
            for h in &cs.hunks {
                self.instances.push(Instance::solid(
                    hx - 4.0 * self.scale(),
                    h.old_lines.start as f32 * scale,
                    4.0 * self.scale(),
                    (h.old_lines.len().max(1) as f32 * scale).max(2.0 * self.scale()),
                    th.hunk(h.kind),
                ));
            }
        }
        // viewport box (in row space; folds make this approximate)
        let first = vs.rows.first().map(|r| r.line).unwrap_or(0) as f32;
        let last = vs.rows.last().map(|r| r.line + 1).unwrap_or(1) as f32;
        self.instances.push(Instance::solid(
            g.minimap_x,
            first * scale,
            g.minimap_w,
            ((last - first) * scale).max(4.0 * self.scale()),
            alpha(th.fg_2, 0.12),
        ));
        self.hits.push(HitRegion {
            x: g.minimap_x,
            y: 0.0,
            w: g.minimap_w,
            h: g.height,
            kind: HitKind::Minimap,
        });
    }

    // ----- GPU ------------------------------------------------------------------------

    /// Creates the pipeline and rasterises printable ASCII at the current metrics so
    /// the first document paints without a cold atlas or a shader compile. Call it
    /// when the surface mounts, before the file arrives.
    pub fn warm_up(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        sample_count: u32,
    ) {
        if self.renderer.as_ref().is_none_or(|r| r.format != format) {
            self.renderer = Some(Renderer::new(device, format, sample_count));
        }
        let m = self.metrics();
        let ascii: String = (0x20u8..0x7f).map(|b| b as char).collect();
        let g = self.compute_geometry();
        self.geometry = Some(g);
        self.instances.clear();
        self.draw_glyph_run(&g, 0.0, 0.0, &ascii, |_| [0.0; 4]);
        for s in ["▸", "▾", "⋯", "✓"] {
            self.draw_glyph_run(&g, 0.0, 0.0, s, |_| [0.0; 4]);
        }
        let _ = m;
        let (w, h) = (
            (self.width * self.scale()).round() as u32,
            (self.height * self.scale()).round() as u32,
        );
        if let Some(r) = &mut self.renderer {
            r.prepare(
                device,
                queue,
                &mut self.text.atlas,
                &[],
                &Placement::fill(w.max(1), h.max(1)),
                w,
                h,
            );
        }
        self.instances.clear();
    }

    /// Builds the frame and uploads it. Call once per frame before the host begins
    /// the render pass that will contain [`EditorView::render`].
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        sample_count: u32,
        placement: &Placement,
        now_ms: u64,
    ) -> FrameStats {
        self.set_zoom(placement.scale);
        self.build_frame(now_ms);
        let needs_new = self.renderer.as_ref().is_none_or(|r| r.format != format);
        if needs_new {
            self.renderer = Some(Renderer::new(device, format, sample_count));
        }
        let (w, h) = (
            (self.width * self.scale()).round() as u32,
            (self.height * self.scale()).round() as u32,
        );
        // the frame was built at dpr × zoom; the host transform must not scale it twice
        let unscaled = Placement {
            scale: 1.0,
            ..*placement
        };
        if let Some(r) = &mut self.renderer {
            r.prepare(
                device,
                queue,
                &mut self.text.atlas,
                &self.instances,
                &unscaled,
                w,
                h,
            );
        }
        self.stats
    }

    /// Draws into the host's pass (same device, same pass list as the 3D viewers).
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(r) = &self.renderer {
            r.render(pass);
        }
    }

    pub fn instances(&self) -> &[Instance] {
        &self.instances
    }
}

trait HitExt {
    fn with_line_pos(self) -> Self;
}

impl HitExt for Hit {
    fn with_line_pos(self) -> Self {
        match self {
            Hit::Gutter(line) => Hit::Text(Position::new(line, 0)),
            other => other,
        }
    }
}

trait CheapHash {
    fn content_hash_cheap(&self) -> u64;
}

impl CheapHash for Editor {
    fn content_hash_cheap(&self) -> u64 {
        // length + line count + history length is enough to notice edits
        let d = self.document();
        (d.len_chars() as u64) ^ ((d.line_count() as u64) << 32) ^ ((self.can_undo() as u64) << 63)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn now() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

#[cfg(target_arch = "wasm32")]
fn now() -> f64 {
    js_sys::Date::now() / 1e3
}

// keep the unused-import lints quiet for optional pieces
#[allow(dead_code)]
fn _unused(_: Modifiers, _: HashMap<u8, u8>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(text: &str) -> EditorView {
        let mut v = EditorView::new(ViewConfig::default(), Theme::dark());
        v.resize(800.0, 300.0, 2.0);
        v.open("cfg.yaml", text);
        v.build_frame(0);
        v
    }

    #[test]
    fn hit_test_maps_pixels_to_positions() {
        let mut v = view("a: 1\nbb: 2\nccc: 3\n");
        let g = v.geometry().expect("geometry");
        let x = (g.text_x + 2.0 * g.metrics.cell_w) / 2.0;
        let y = (1.5 * g.metrics.line_h) / 2.0;
        assert_eq!(v.hit_test(x, y), Hit::Text(Position::new(1, 2)));
        assert_eq!(v.hit_test(1.0, y), Hit::Gutter(1));
        assert_eq!(v.hit_test(-5.0, y), Hit::Outside);
        // the fold chevron of line 0 is not present (no fold); clicking a foldable header toggles
        let mut v2 = view("a:\n  b: 1\n  c: 2\nd: 3\n");
        let g = v2.geometry().expect("geometry");
        let fx = (g.fold_x + 3.0) / 2.0;
        assert_eq!(v2.hit_test(fx, 2.0), Hit::FoldToggle(0));
        v2.handle(
            InputEvent::PointerDown {
                x: fx,
                y: 2.0,
                button: 0,
                clicks: 1,
                mods: Modifiers::NONE,
            },
            10,
        );
        assert_eq!(v2.total_rows(), 3);
    }

    #[test]
    fn click_drag_selects_and_builds_selection_quads() {
        let mut v = view("hello world\nsecond line\n");
        let g = v.geometry().expect("geometry");
        let cell = g.metrics.cell_w / 2.0;
        let lh = g.metrics.line_h / 2.0;
        let tx = g.text_x / 2.0;
        v.handle(
            InputEvent::PointerDown {
                x: tx + cell * 1.0,
                y: lh * 0.5,
                button: 0,
                clicks: 1,
                mods: Modifiers::NONE,
            },
            0,
        );
        v.handle(
            InputEvent::PointerMove {
                x: tx + cell * 3.0,
                y: lh * 1.5,
            },
            5,
        );
        v.handle(InputEvent::PointerUp { x: 0.0, y: 0.0 }, 6);
        assert_eq!(v.selected_text(), "ello world\nsec");
        let before = v.instances().len();
        v.build_frame(20);
        assert!(v.instances().len() >= before);
        let sel = Theme::dark().highlight("selection").expect("colour");
        assert!(
            v.instances().iter().filter(|i| i.color == sel).count() >= 2,
            "one quad per selected row"
        );
    }

    #[test]
    fn typing_and_keys_route_through_the_core() {
        let mut v = view("");
        v.handle(InputEvent::FocusIn, 0);
        v.handle(InputEvent::Text("ab".into()), 1);
        v.handle(InputEvent::Key(Key::parse("enter").expect("k")), 2);
        v.handle(InputEvent::CompositionStart, 3);
        v.handle(InputEvent::CompositionUpdate("に".into()), 4);
        v.build_frame(5);
        assert_eq!(v.editor().expect("e").text(), "ab\n");
        v.handle(InputEvent::CompositionEnd("日本".into()), 6);
        assert_eq!(v.editor().expect("e").text(), "ab\n日本");
        let req = v.handle(InputEvent::Key(Key::parse("primary-s").expect("k")), 7);
        assert!(req.contains(&HostRequest::Save));
        let req = v.handle(InputEvent::Key(Key::parse("primary-c").expect("k")), 8);
        assert!(matches!(req[0], HostRequest::Copy(_)));
    }

    #[test]
    fn scrolling_is_clamped_and_smooth() {
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let mut v = view(&text);
        v.handle(
            InputEvent::Wheel {
                dx: 0.0,
                dy: 100_000.0,
                mods: Modifiers::NONE,
            },
            0,
        );
        v.build_frame(16);
        assert!(v.scroll_row() > 0.0);
        for t in 1..60 {
            v.build_frame(16 * t);
        }
        let max = v.max_scroll();
        assert!(
            (v.scroll_row() - max).abs() < 0.01,
            "{} vs {max}",
            v.scroll_row()
        );
        let g = v.geometry().expect("g");
        let stats = v.stats();
        assert!(stats.rows <= g.rows_visible);
    }

    #[test]
    fn a_comparison_shows_changes_with_accept_and_reject() {
        let base: String = (0..40).map(|i| format!("k{i}: {i}\n")).collect();
        let new = base.replace("k10: 10\n", "k10: ten\n") + "tail: 1\n";
        let mut v = view(&base);
        v.editor_mut().expect("e").review(&new);
        v.build_frame(0);
        let sum = v.diff_summary().expect("in review");
        assert_eq!((sum.changes, sum.added, sum.removed), (2, 2, 1));
        // unchanged lines away from the changes are collapsed: far fewer rows than lines
        assert!(v.total_rows() < 25, "{} rows", v.total_rows());
        // the first change is on screen with its buttons; pressing Accept accepts it
        let hit = |v: &EditorView, want: HitKind| v.hits.iter().find(|h| h.kind == want).copied();
        let accept = hit(&v, HitKind::HunkAccept(0)).expect("accept button");
        let press = |v: &mut EditorView, h: HitRegion| {
            let s = v.scale();
            down(v, (h.x + 2.0) / s, (h.y + 2.0) / s, 1, Modifiers::NONE);
            v.build_frame(1);
        };
        press(&mut v, accept);
        assert_eq!(v.diff_summary().expect("s").accepted, 1);
        // Reject on the same change takes the acceptance back and marks it turned down
        let reject = hit(&v, HitKind::HunkReject(0)).expect("reject button");
        press(&mut v, reject);
        let sum = v.diff_summary().expect("s");
        assert_eq!((sum.accepted, sum.rejected), (0, 1));
        // next change: the second one is brought into view and can be decided from the keyboard
        assert_eq!(v.go_to_change(true), Some(1));
        v.decide_change(None, true);
        assert_eq!(v.go_to_change(true), Some(0), "wraps");
        assert_eq!(v.go_to_change(false), Some(1));
        v.build_frame(2);
        let sum = v.diff_summary().expect("s");
        assert_eq!((sum.accepted, sum.rejected, sum.at), (1, 1, Some(2)));
        // a collapsed stretch opens when pressed
        v.scroll_to_row(0.0, true);
        v.build_frame(3);
        let rows = v.total_rows();
        let fold = hit(&v, HitKind::Expand).expect("collapsed rows");
        press(&mut v, fold);
        assert!(v.total_rows() > rows + 10);
        // side by side pairs the changed line: one row fewer per modified line
        let inline = v.total_rows();
        v.config.diff_mode = DiffMode::SideBySide;
        v.resize(1200.0, 300.0, 2.0);
        v.build_frame(4);
        assert_eq!(v.total_rows(), inline - 1);
        assert!(v.stats().rows > 0);
        // a comparison without decisions has no buttons
        v.config.review_actions = false;
        v.scroll_to_row(0.0, true);
        v.build_frame(5);
        assert!(hit(&v, HitKind::HunkAccept(0)).is_none());
        // accepting everything and finishing applies the accepted changes to the buffer
        v.decide_all(true);
        let ev = v
            .editor_mut()
            .expect("e")
            .finish_review(frontier_editor::Author::User { id: "t".into() }, None)
            .expect("finish");
        assert_eq!(ev.accepted_hunks.len(), 2);
        assert_eq!(v.editor().expect("e").text(), new);
    }

    #[test]
    fn log_content_scrolls_through_chunked_source() {
        use frontier_editor::source::SyntheticLog;
        let mut v = EditorView::new(ViewConfig::default(), Theme::dark());
        v.resize(800.0, 400.0, 2.0);
        v.open_log(
            Box::new(SyntheticLog {
                line_count: 5_000_000,
            }),
            Language::Log,
        )
        .expect("open");
        v.build_frame(0);
        assert!(v.stats().glyphs > 0);
        v.scroll_to_row(4_999_000.0, true);
        v.build_frame(1);
        let g = v.geometry().expect("g");
        assert!(v.stats().rows <= g.rows_visible);
        assert!(
            matches!(v.hit_test(g.text_x / 2.0 + 10.0, 5.0), Hit::Text(Position { line, .. }) if line >= 4_999_000)
        );
        assert!(v.log().expect("log").memory_bytes() < 100 * 1024 * 1024);
    }

    fn down(v: &mut EditorView, x: f32, y: f32, clicks: u8, mods: Modifiers) {
        v.handle(
            InputEvent::PointerDown {
                x,
                y,
                button: 0,
                clicks,
                mods,
            },
            0,
        );
    }

    /// The logical point of a (line, column) of a view that is not scrolled.
    fn point(v: &mut EditorView, line: usize, col: usize) -> (f32, f32) {
        v.build_frame(0);
        let g = v.geometry().expect("geometry");
        let s = v.scale();
        (
            (g.text_x + col as f32 * g.metrics.cell_w) / s,
            ((line as f32 + 0.5) * g.metrics.line_h) / s,
        )
    }

    #[test]
    fn two_views_of_one_buffer_edit_the_same_text() {
        let mut a = view("one\ntwo\n");
        let mut b = EditorView::new(ViewConfig::default(), Theme::dark());
        b.resize(900.0, 600.0, 2.0);
        b.open_shared(a.shared().expect("shared"));
        // type in the small view, then in the big one, then in the small one again
        a.handle(InputEvent::Text("A".into()), 10);
        b.handle(InputEvent::Text("B".into()), 2000);
        a.handle(InputEvent::Text("C".into()), 4000);
        assert_eq!(a.editor().expect("e").text(), "ABCone\ntwo\n");
        assert_eq!(b.editor().expect("e").text(), "ABCone\ntwo\n");
        // one caret, one history: undo in either view steps back through all of it
        b.handle(InputEvent::Key(Key::ch('z', Modifiers::PRIMARY)), 6000);
        assert_eq!(a.editor().expect("e").text(), "ABone\ntwo\n");
        a.handle(InputEvent::Key(Key::ch('z', Modifiers::PRIMARY)), 8000);
        a.handle(InputEvent::Key(Key::ch('z', Modifiers::PRIMARY)), 9000);
        assert_eq!(b.editor().expect("e").text(), "one\ntwo\n");
        // each view keeps its own scroll position and both draw
        b.scroll_to_row(1.0, true);
        assert_eq!(a.scroll_row(), 0.0);
        assert!(!a.build_frame(0).is_empty() && !b.build_frame(0).is_empty());
    }

    #[test]
    fn zooming_changes_the_pixels_and_nothing_else() {
        let text: String = (0..80)
            .map(|i| format!("key_{i}: value number {i}\n"))
            .collect();
        let mut v = view(&text);
        v.config.minimap = true;
        v.resize(560.0, 260.0, 1.0);
        v.build_frame(0);
        let g1 = v.geometry().expect("g");
        let (px, py) = point(&mut v, 7, 12);
        let at_1 = v.hit_test(px, py);
        assert_eq!(at_1, Hit::Text(Position::new(7, 12)));
        for zoom in [0.6f32, 1.0, 1.37, 2.4, 3.0] {
            v.set_zoom(zoom);
            v.build_frame(0);
            let g = v.geometry().expect("g");
            // the frame fills the target at this zoom, and every part scales with it
            assert!((g.width - 560.0 * zoom).abs() < 0.01, "width at {zoom}");
            assert!((g.height - 260.0 * zoom).abs() < 0.01);
            for (a, b) in [
                (g.gutter_w, g1.gutter_w),
                (g.text_x, g1.text_x),
                (g.minimap_x, g1.minimap_x),
                (g.metrics.line_h, g1.metrics.line_h),
            ] {
                assert!(
                    (a / zoom - b).abs() < 1.0,
                    "layout moved at zoom {zoom}: {a} vs {b}"
                );
            }
            // the same rows are on screen and the same logical point is the same text
            assert_eq!(g.rows_visible, g1.rows_visible, "rows at {zoom}");
            assert_eq!(v.hit_test(px, py), at_1, "hit test at zoom {zoom}");
        }
    }

    #[test]
    fn a_drag_survives_losing_focus_and_leaving_the_view() {
        let mut v = view("alpha beta\ngamma delta\nepsilon\n");
        let (x0, y0) = point(&mut v, 0, 2);
        let (x1, y1) = point(&mut v, 1, 5);
        down(&mut v, x0, y0, 1, Modifiers::NONE);
        v.handle(InputEvent::FocusOut, 1);
        v.handle(InputEvent::PointerMove { x: x1, y: y1 }, 2);
        assert_eq!(v.selected_text(), "pha beta\ngamma");
        // outside the view on the right: clamped to the row, still selecting
        v.handle(InputEvent::PointerMove { x: 5000.0, y: y1 }, 3);
        assert_eq!(v.selected_text(), "pha beta\ngamma delta");
        v.handle(InputEvent::PointerUp { x: x1, y: y1 }, 4);
        v.handle(InputEvent::PointerMove { x: x0, y: y0 }, 5);
        assert_eq!(
            v.selected_text(),
            "pha beta\ngamma delta",
            "released: moves select nothing"
        );
    }

    #[test]
    fn double_and_triple_click_drags_and_shift_click() {
        let mut v = view("alpha beta\ngamma delta\nepsilon\n");
        let (x0, y0) = point(&mut v, 0, 7);
        let (x1, y1) = point(&mut v, 1, 1);
        down(&mut v, x0, y0, 2, Modifiers::NONE);
        assert_eq!(v.selected_text(), "beta");
        v.handle(InputEvent::PointerMove { x: x1, y: y1 }, 1);
        assert_eq!(v.selected_text(), "beta\ngamma");
        v.handle(InputEvent::PointerUp { x: x1, y: y1 }, 2);
        down(&mut v, x0, y0, 3, Modifiers::NONE);
        assert_eq!(v.selected_text(), "alpha beta\n");
        v.handle(InputEvent::PointerMove { x: x1, y: y1 }, 3);
        assert_eq!(v.selected_text(), "alpha beta\ngamma delta\n");
        v.handle(InputEvent::PointerUp { x: x1, y: y1 }, 4);
        // click, then shift-click extends from the first click
        down(&mut v, x0, y0, 1, Modifiers::NONE);
        v.handle(InputEvent::PointerUp { x: x0, y: y0 }, 5);
        down(&mut v, x1, y1, 1, Modifiers::SHIFT);
        assert_eq!(v.selected_text(), "eta\ng");
    }

    #[test]
    fn alt_drag_selects_a_block_and_alt_click_adds_a_caret() {
        let mut v = view("abcdef\nab\nabcdefgh\n");
        let (x0, y0) = point(&mut v, 0, 2);
        let (x1, y1) = point(&mut v, 2, 5);
        down(&mut v, x0, y0, 1, Modifiers::ALT);
        v.handle(InputEvent::PointerMove { x: x1, y: y1 }, 1);
        v.handle(InputEvent::PointerUp { x: x1, y: y1 }, 2);
        assert_eq!(v.selected_text(), "cde\ncde");
        assert_eq!(v.editor().expect("e").selections().len(), 3);
        // a plain click, then alt-click elsewhere: two carets, typing goes to both
        down(&mut v, x0, y0, 1, Modifiers::NONE);
        v.handle(InputEvent::PointerUp { x: x0, y: y0 }, 3);
        down(&mut v, x1, y1, 1, Modifiers::ALT);
        v.handle(InputEvent::PointerUp { x: x1, y: y1 }, 4);
        v.handle(InputEvent::Text("X".into()), 5);
        assert_eq!(v.editor().expect("e").text(), "abXcdef\nab\nabcdeXfgh\n");
    }

    #[test]
    fn copy_cut_and_paste_requests() {
        let mut v = view("one\ntwo\n");
        // nothing selected: the line
        let r = v.handle(InputEvent::Key(Key::ch('c', Modifiers::PRIMARY)), 0);
        assert!(r.contains(&HostRequest::Copy("one\n".into())));
        let (t, _) = v.cut(10);
        assert_eq!(t, "one\n");
        assert_eq!(v.editor().expect("e").text(), "two\n");
        v.handle(InputEvent::Paste("zero\n".into()), 20);
        assert_eq!(v.editor().expect("e").text(), "zero\ntwo\n");
        v.handle(InputEvent::Key(Key::ch('z', Modifiers::PRIMARY)), 30);
        assert_eq!(
            v.editor().expect("e").text(),
            "two\n",
            "a paste is one undo step"
        );
    }

    #[test]
    fn wide_characters_and_clusters_are_hit_and_drawn_on_their_cells() {
        let mut v = view("a漢字b\ne\u{301}x\n\tq\n");
        v.build_frame(0);
        let g = v.geometry().expect("g");
        let s = v.scale();
        let at = |cells: f32, line: usize| {
            (
                (g.text_x + cells * g.metrics.cell_w) / s,
                ((line as f32 + 0.5) * g.metrics.line_h) / s,
            )
        };
        // 漢 covers cells 1..3: a press on its left half is before it, on its right half after
        let (x, y) = at(1.4, 0);
        assert_eq!(v.hit_test(x, y), Hit::Text(Position::new(0, 1)));
        let (x, y) = at(2.6, 0);
        assert_eq!(v.hit_test(x, y), Hit::Text(Position::new(0, 2)));
        // b is at cell 5
        let (x, y) = at(5.0, 0);
        assert_eq!(v.hit_test(x, y), Hit::Text(Position::new(0, 3)));
        // the accent is no place for a caret: after "é" is column 2
        let (x, y) = at(1.0, 1);
        assert_eq!(v.hit_test(x, y), Hit::Text(Position::new(1, 2)));
        // a tab reaches the next stop (2 cells in YAML): q is at cell 2
        let (x, y) = at(2.0, 2);
        assert_eq!(v.hit_test(x, y), Hit::Text(Position::new(2, 1)));
        // the caret after 字 is drawn at cell 5
        down(&mut v, at(5.0, 0).0, at(5.0, 0).1, 1, Modifiers::NONE);
        let (cx, _, _, _) = v.caret_rect().expect("caret");
        assert!((cx - at(5.0, 0).0).abs() < 0.6, "{cx}");
        // every glyph of the wide line is on screen once
        v.build_frame(1);
        assert!(v.stats().glyphs >= 6);
    }

    #[test]
    fn soft_wrap_keeps_lines_inside_the_view_at_every_zoom() {
        let long = "word ".repeat(60);
        let mut v = view(&format!("first\n{long}\nlast\n"));
        v.config.wrap = true;
        v.config.minimap = false;
        v.resize(420.0, 300.0, 1.0);
        v.build_frame(0);
        v.build_frame(1);
        let rows = v.total_rows();
        assert!(rows > 8, "the long line takes several rows: {rows}");
        let g = v.geometry().expect("g");
        // no glyph is drawn past the text area
        let right = v
            .instances()
            .iter()
            .filter(|i| i.kind == Instance::GLYPH)
            .map(|i| i.rect[0] + i.rect[2])
            .fold(0.0f32, f32::max);
        assert!(right <= g.scrollbar_x + 1.0, "{right} > {}", g.scrollbar_x);
        // a press on the third row of the wrapped line lands in that part of the line
        let s = v.scale();
        let hit = v.hit_test(
            (g.text_x + 2.0 * g.metrics.cell_w) / s,
            (3.5 * g.metrics.line_h) / s,
        );
        match hit {
            Hit::Text(p) => assert!(p.line == 1 && p.col > 40, "{p:?}"),
            other => panic!("{other:?}"),
        }
        // the same rows at every zoom: zooming never reflows
        for zoom in [0.5f32, 0.77, 1.3, 2.0, 2.9] {
            v.set_zoom(zoom);
            v.build_frame(2);
            v.build_frame(3);
            assert_eq!(v.total_rows(), rows, "rows at zoom {zoom}");
        }
        // a narrower view wraps into more rows; turning wrap off gives one row per line
        v.set_zoom(1.0);
        v.resize(260.0, 300.0, 1.0);
        v.build_frame(4);
        v.build_frame(5);
        assert!(v.total_rows() > rows);
        v.config.wrap = false;
        v.build_frame(6);
        assert_eq!(v.total_rows(), 4);
    }
}
