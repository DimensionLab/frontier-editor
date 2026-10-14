// SPDX-License-Identifier: Apache-2.0
//! CPU-side view benchmarks (no GPU): frame build for a 1 MB config, a keystroke
//! to a rebuilt frame, and 5M-line log windows. GPU frame times come from
//! `cargo run --release --example native -- --bench`.
//!
//!     cargo bench -p frontier-editor-view

use frontier_editor::source::SyntheticLog;
use frontier_editor::syntax::Language;
use frontier_editor_view::{EditorView, InputEvent, Modifiers, Theme, ViewConfig};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Counts allocations, so a frame's are a number this benchmark reports.
struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

/// Time per call (p50, p95) and allocations per call of `f` over `n` calls.
fn measure(name: &str, n: u64, mut f: impl FnMut(u64)) {
    let mut v = Vec::with_capacity(n as usize);
    let a0 = ALLOCS.load(Ordering::Relaxed);
    for i in 0..n {
        let t = Instant::now();
        f(i);
        v.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let allocs = (ALLOCS.load(Ordering::Relaxed) - a0) as f64 / n as f64;
    println!(
        "{name:<48} {:>8.3} ms p50 {:>8.3} ms p95 {allocs:>8.1} allocations",
        median(v.clone()),
        p95(v)
    );
}

/// A visible page of a node on the board (620 x 420 CSS px at 2x) over a 10,000-line
/// YAML file: what one frame costs while it scrolls and while keys arrive.
fn visible_page() {
    let text: String = (0..10_000)
        .map(|i| format!("key_{i}: value number {i}  # note {i}\n"))
        .collect();
    let mut view = EditorView::new(ViewConfig::default(), Theme::dark());
    view.resize(620.0, 420.0, 2.0);
    view.open("big.yaml", &text);
    view.handle(InputEvent::FocusIn, 0);
    view.build_frame(0);
    measure("page: frame build, nothing changed", 300, |i| {
        view.build_frame(16 + i * 16);
    });
    measure("page: wheel (34 px) -> frame build", 600, |i| {
        let t = 10_000 + i * 16;
        view.handle(
            InputEvent::Wheel {
                dx: 0.0,
                dy: if (i / 150) % 2 == 0 { 34.0 } else { -34.0 },
                mods: Modifiers::NONE,
            },
            t,
        );
        view.build_frame(t);
    });
    view.scroll_to_row(5000.0, true);
    view.build_frame(30_000);
    view.handle(
        InputEvent::PointerDown {
            x: 200.0,
            y: 100.0,
            button: 0,
            clicks: 1,
            mods: Modifiers::NONE,
        },
        30_000,
    );
    view.handle(InputEvent::PointerUp { x: 200.0, y: 100.0 }, 30_000);
    measure("page: key -> frame build", 300, |i| {
        let t = 40_000 + i * 16;
        view.handle(
            InputEvent::Text("abc def "[(i % 8) as usize..][..1].into()),
            t,
        );
        view.build_frame(t);
    });
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("ord"));
    v[v.len() / 2]
}

fn p95(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("ord"));
    v[v.len() * 95 / 100]
}

fn main() {
    visible_page();
    let mut cfg = String::new();
    for i in 0..28_000 {
        if i % 10 == 0 {
            cfg.push_str(&format!("section_{i}:\n"));
        } else {
            cfg.push_str(&format!("  key_{i}: value_{i}  # comment {i}\n"));
        }
    }
    let t = Instant::now();
    let mut view = EditorView::new(ViewConfig::default(), Theme::dark());
    view.resize(1100.0, 720.0, 2.0);
    view.open("bench.yaml", &cfg);
    view.handle(InputEvent::FocusIn, 0);
    println!(
        "{:<48} {:>8.2} ms",
        "open 1 MB YAML in the view",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    view.build_frame(0);
    println!(
        "{:<48} {:>8.2} ms ({} instances, {} glyphs)",
        "first frame build (cold shape + atlas)",
        t.elapsed().as_secs_f64() * 1e3,
        view.stats().instances,
        view.stats().glyphs
    );
    let mut v = Vec::new();
    for i in 1..200 {
        let t = Instant::now();
        view.build_frame(i * 16);
        v.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{:<48} {:>8.3} ms p50 {:>8.3} ms p95",
        "steady frame build (warm)",
        median(v.clone()),
        p95(v)
    );
    view.handle(
        InputEvent::PointerDown {
            x: 300.0,
            y: 40.0,
            button: 0,
            clicks: 1,
            mods: Modifiers::NONE,
        },
        4000,
    );
    let mut v = Vec::new();
    for i in 0..200 {
        let t = Instant::now();
        view.handle(InputEvent::Text("x".into()), 5000 + i * 16);
        view.build_frame(5000 + i * 16);
        v.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{:<48} {:>8.3} ms p50 {:>8.3} ms p95",
        "keystroke -> frame build (core + view)",
        median(v.clone()),
        p95(v)
    );
    let mut v = Vec::new();
    for i in 0..300 {
        let t = Instant::now();
        view.handle(
            InputEvent::Wheel {
                dx: 0.0,
                dy: 200.0,
                mods: Modifiers::NONE,
            },
            9000 + i * 16,
        );
        view.build_frame(9000 + i * 16);
        v.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{:<48} {:>8.3} ms p50 {:>8.3} ms p95",
        "wheel -> frame build (smooth scroll)",
        median(v.clone()),
        p95(v)
    );
    view.set_zoom(1.7);
    let mut v = Vec::new();
    for i in 0..60 {
        let t = Instant::now();
        view.build_frame(20000 + i * 16);
        v.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{:<48} {:>8.3} ms p50 {:>8.3} ms p95",
        "frame build at host zoom 1.7 (re-rasterised)",
        median(v.clone()),
        p95(v)
    );

    let t = Instant::now();
    let mut log = EditorView::new(ViewConfig::default(), Theme::dark());
    log.resize(1100.0, 720.0, 2.0);
    log.open_log(
        Box::new(SyntheticLog {
            line_count: 5_000_000,
        }),
        Language::Log,
    )
    .expect("log");
    println!(
        "{:<48} {:>8.2} ms",
        "open 5M-line log",
        t.elapsed().as_secs_f64() * 1e3
    );
    let mut v = Vec::new();
    for i in 0..300 {
        log.scroll_to_row(i as f64 * 4.0, true);
        let t = Instant::now();
        log.build_frame(i * 16);
        v.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{:<48} {:>8.3} ms p50 {:>8.3} ms p95",
        "log frame build, sequential scroll",
        median(v.clone()),
        p95(v)
    );
    let mut v = Vec::new();
    for i in 0..100u64 {
        log.scroll_to_row(((i * 2_654_435_761) % 4_999_000) as f64, true);
        let t = Instant::now();
        log.build_frame(i * 16);
        v.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "{:<48} {:>8.3} ms p50 {:>8.3} ms p95",
        "log frame build, random jumps (chunk reads)",
        median(v.clone()),
        p95(v)
    );
    println!(
        "{:<48} {:>8.1} MB",
        "log resident memory (index + chunks)",
        log.log().expect("log").memory_bytes() as f64 / 1e6
    );
}
