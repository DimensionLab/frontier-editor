// SPDX-License-Identifier: Apache-2.0
//! Core benchmarks (plain timing, no harness): open / edit / undo on 1 MB and
//! 100 MB documents, highlight a visible range, validate a 5k-line YAML.
//!
//!     cargo bench -p frontier-editor
//!     FRONTIER_BENCH_BIG=0 cargo bench -p frontier-editor   # skip the 100 MB case

use frontier_editor::command::Command;
use frontier_editor::document::{Position, Range};
use frontier_editor::view::Viewport;
use frontier_editor::{Editor, Highlighter, Language};
use std::time::{Duration, Instant};

fn yaml_doc(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 40);
    for i in 0..lines {
        if i % 10 == 0 {
            s.push_str(&format!("section_{i}:\n"));
        } else {
            s.push_str(&format!("  key_{i}: value_{i}  # comment {i}\n"));
        }
    }
    s
}

fn time<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    println!("{label:<48} {:>10.3} ms", t.elapsed().as_secs_f64() * 1e3);
    r
}

fn median(mut v: Vec<Duration>) -> f64 {
    v.sort();
    v[v.len() / 2].as_secs_f64() * 1e3
}

fn bench_doc(label: &str, text: String) {
    println!(
        "--- {label}: {:.1} MB, {} lines",
        text.len() as f64 / 1e6,
        text.lines().count()
    );
    let mut ed = time("open (rope + full tree-sitter parse)", || {
        Editor::open("bench.yaml", &text)
    });
    let last = ed.document().line_count() - 1;
    // inside a value: the tree stays valid, tree-sitter reuses almost everything
    ed.click(Position::new(last / 2, 12), false, false);
    let mut samples = Vec::new();
    for i in 0..20 {
        ed.tick(i * 1000);
        let t = Instant::now();
        ed.insert("x").expect("insert");
        samples.push(t.elapsed());
    }
    println!(
        "{:<48} {:>10.3} ms (median of 20)",
        "insert one char inside a value (valid tree)",
        median(samples)
    );
    // at a line start: breaks the mapping, tree-sitter error recovery over the flat file
    ed.click(Position::new(last / 2, 0), false, false);
    let mut samples = Vec::new();
    for i in 0..20 {
        ed.tick(100_000 + i * 1000);
        let t = Instant::now();
        ed.insert("x").expect("insert");
        samples.push(t.elapsed());
    }
    println!(
        "{:<48} {:>10.3} ms (median of 20)",
        "insert one char at line start (error recovery)",
        median(samples)
    );
    let mut samples = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        ed.execute(Command::Undo).expect("undo");
        samples.push(t.elapsed());
    }
    println!("{:<48} {:>10.3} ms (median of 20)", "undo", median(samples));
    let mut samples = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let _ = ed.view_state(Viewport {
            first_row: last / 2,
            rows: 60,
        });
        samples.push(t.elapsed());
    }
    println!(
        "{:<48} {:>10.3} ms (median of 20)",
        "view_state 60 rows (highlight + decorations)",
        median(samples)
    );
    let mut h = Highlighter::new(Language::Yaml);
    h.set_text(ed.document());
    let doc = ed.document().clone();
    let mut samples = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let _ = h.spans(&doc, Range::new(last / 2, last / 2 + 60));
        samples.push(t.elapsed());
    }
    println!(
        "{:<48} {:>10.3} ms (median of 20)",
        "highlight 60 visible lines",
        median(samples)
    );
}

fn main() {
    bench_doc("256 KB YAML (tree-sitter)", yaml_doc(7_000));
    bench_doc(
        "1 MB YAML (lexer: above TREE_SITTER_BUDGET_BYTES)",
        yaml_doc(28_000),
    );
    if std::env::var("FRONTIER_BENCH_BIG")
        .map(|v| v != "0")
        .unwrap_or(true)
    {
        bench_doc("100 MB YAML", yaml_doc(2_800_000));
    }
    #[cfg(feature = "schema")]
    {
        use frontier_editor::schema::SchemaRegistry;
        let text = yaml_doc(5_000);
        let schema = r#"{"type":"object","additionalProperties":{"anyOf":[{"type":"null"},{"type":"object","additionalProperties":{"type":"string"}}]}}"#;
        let mut reg = SchemaRegistry::new();
        reg.register("bench", schema, &[], &[]).expect("schema");
        let entry = reg.get("bench").expect("entry");
        println!("--- schema");
        let mut samples = Vec::new();
        for _ in 0..10 {
            let t = Instant::now();
            let _ = SchemaRegistry::validate(&entry, &text, Language::Yaml);
            samples.push(t.elapsed());
        }
        println!(
            "{:<48} {:>10.3} ms (median of 10)",
            "validate 5k-line YAML (parse + schema)",
            median(samples)
        );
    }
    println!("--- chunked log");
    use frontier_editor::source::{ChunkedDocument, SyntheticLog};
    let mut doc = time("open 5M-line synthetic log (index)", || {
        ChunkedDocument::open(Box::new(SyntheticLog {
            line_count: 5_000_000,
        }))
        .expect("open")
    });
    let mut samples = Vec::new();
    for i in 0..50u64 {
        let t = Instant::now();
        let _ = doc.lines(i * 99_991 % 4_999_000, 60);
        samples.push(t.elapsed());
    }
    println!(
        "{:<48} {:>10.3} ms (median of 50)",
        "random 60-line window",
        median(samples)
    );
    println!(
        "{:<48} {:>10.1} MB",
        "memory (index + chunk cache)",
        doc.memory_bytes() as f64 / 1e6
    );
}
