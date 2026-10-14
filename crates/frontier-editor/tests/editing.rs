// SPDX-License-Identifier: Apache-2.0
//! Randomised and large-file checks of the core: edits against a plain-string model,
//! undo back to the start, selections that stay valid, multi-cursor, find/replace,
//! pairs, column selection, and 100k-line operations inside a time budget.

use frontier_editor::command::Command;
use frontier_editor::document::{Position, Range};
use frontier_editor::editor::{Editor, diff_span};
use frontier_editor::search::SearchOptions;
use frontier_editor::selection::{Selection, SelectionSet};
use frontier_editor::transaction::{Change, EditKind, Transaction};
use std::time::Instant;

/// xorshift: a seed gives the same run on every machine.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

const ALPHABET: &[&str] = &["a", "b", " ", "\n", "é", "漢", "{", "}", ":", "x1", "-\n  "];

fn plain(text: &str) -> Editor {
    let mut e = Editor::open("notes.txt", text);
    e.options.auto_close = false;
    e
}

fn check_selections(e: &Editor) {
    let n = e.document().len_chars();
    let mut last_end = 0;
    for (i, s) in e.selections().iter().enumerate() {
        assert!(s.anchor <= n && s.head <= n, "selection outside the text");
        if i > 0 {
            assert!(s.start() >= last_end, "selections overlap");
        }
        last_end = s.end();
    }
}

#[test]
fn random_edits_match_a_string_model_and_undo_returns_to_the_start() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let start = "alpha: 1\nbeta:\n  - x\n  - y\n";
        let mut e = plain(start);
        let mut model: Vec<char> = start.chars().collect();
        let mut clock = 0u64;
        for _ in 0..120 {
            clock += 1000; // every edit its own undo step
            e.tick(clock);
            let n = model.len();
            let a = rng.below(n + 1);
            let b = (a + rng.below(4)).min(n);
            let text = ALPHABET[rng.below(ALPHABET.len())];
            let tx = Transaction::single(Change::replace(Range::new(a, b), text), EditKind::Other);
            e.apply(tx, None).expect("apply");
            model.splice(a..b, text.chars());
            assert_eq!(e.text(), model.iter().collect::<String>(), "seed {seed}");
            check_selections(&e);
        }
        let end = e.text();
        let mut undone = 0;
        while e.can_undo() {
            e.execute(Command::Undo).expect("undo");
            check_selections(&e);
            undone += 1;
            assert!(undone <= 200);
        }
        assert_eq!(
            e.text(),
            start,
            "seed {seed}: undo did not return to the start"
        );
        while e.can_redo() {
            e.execute(Command::Redo).expect("redo");
        }
        assert_eq!(e.text(), end, "seed {seed}: redo did not return to the end");
    }
}

#[test]
fn random_commands_never_break_the_invariants() {
    use Command::*;
    let cmds = [
        MoveLeft(false),
        MoveRight(true),
        MoveUp(false),
        MoveDown(true),
        MoveWordLeft(true),
        MoveWordRight(false),
        MoveLineStart(true),
        MoveLineEnd(false),
        SelectLine,
        SelectWord,
        SelectNextOccurrence,
        AddCursorAbove,
        AddCursorBelow,
        CollapseSelections,
        InsertNewline,
        InsertTab,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteLine,
        DuplicateLine,
        MoveLineUp,
        MoveLineDown,
        ToggleComment,
        Indent,
        Outdent,
        Undo,
        Redo,
        SelectAll,
        ToggleFold,
        GoToMatchingBracket,
        GoToLine(3),
        MoveDocStart(false),
        MoveDocEnd(true),
    ];
    for seed in 1..=30u64 {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let mut e = Editor::open("c.yaml", "a:\n  b: [1, 2]\n  c: {d: \"e\"}\nf: g\n");
        for step in 0..200 {
            e.tick(step * 40);
            let before = format!("{:?} in {:?}", e.selections().as_slice(), e.text());
            let did;
            if rng.below(3) == 0 {
                let t = ALPHABET[rng.below(ALPHABET.len())];
                e.insert(t).expect("insert");
                did = format!("insert {t:?}");
            } else {
                let c = cmds[rng.below(cmds.len())].clone();
                did = format!("{c:?}");
                e.execute(c).expect("command");
            }
            let n = e.document().len_chars();
            assert!(
                e.selections().iter().all(|s| s.anchor <= n && s.head <= n),
                "seed {seed} step {step}: {did} left {:?} outside {n} chars; before: {before}",
                e.selections().as_slice()
            );
            check_selections(&e);
            // the view of any window never panics and stays inside the text
            let vs = e.view_state(frontier_editor::Viewport {
                first_row: rng.below(8),
                rows: 12,
            });
            assert!(vs.rows.len() <= 12);
        }
    }
}

#[test]
fn multi_cursor_typing_is_one_undo_step_per_burst() {
    let mut e = plain("one\ntwo\nthree\n");
    e.execute(Command::AddCursorBelow).unwrap();
    e.execute(Command::AddCursorBelow).unwrap();
    assert_eq!(e.selections().len(), 3);
    e.tick(10);
    e.insert("> ").unwrap();
    assert_eq!(e.text(), "> one\n> two\n> three\n");
    e.tick(20);
    e.insert("!").unwrap();
    assert_eq!(e.text(), "> !one\n> !two\n> !three\n");
    e.execute(Command::Undo).unwrap();
    assert_eq!(
        e.text(),
        "> one\n> two\n> three\n",
        "a paste-sized insert is its own step"
    );
    e.execute(Command::Undo).unwrap();
    assert_eq!(e.text(), "one\ntwo\nthree\n");
}

#[test]
fn typing_groups_break_on_a_pause_and_on_a_newline() {
    let mut e = plain("");
    for (i, c) in "hello".chars().enumerate() {
        e.tick(i as u64 * 80);
        e.insert(&c.to_string()).unwrap();
    }
    e.tick(5_000);
    for (i, c) in " world".chars().enumerate() {
        e.tick(5_000 + i as u64 * 80);
        e.insert(&c.to_string()).unwrap();
    }
    e.execute(Command::Undo).unwrap();
    assert_eq!(e.text(), "hello");
    e.execute(Command::Undo).unwrap();
    assert_eq!(e.text(), "");
    e.execute(Command::Redo).unwrap();
    e.execute(Command::Redo).unwrap();
    assert_eq!(e.text(), "hello world");
}

#[test]
fn pairs_wrap_step_over_and_delete_together() {
    let mut e = Editor::open("a.json", "");
    e.insert("{").unwrap();
    assert_eq!(e.text(), "{}");
    e.insert("\"").unwrap();
    assert_eq!(e.text(), "{\"\"}");
    e.insert("k").unwrap();
    e.insert("\"").unwrap(); // steps over the closing quote
    assert_eq!(e.text(), "{\"k\"}");
    assert_eq!(e.selections().primary().head, 4);
    e.insert("}").unwrap(); // steps over the closing brace
    assert_eq!(e.text(), "{\"k\"}");
    assert_eq!(e.selections().primary().head, 5);
    // Backspace inside an empty pair removes both
    let mut e = Editor::open("a.json", "");
    e.insert("[").unwrap();
    e.execute(Command::Backspace).unwrap();
    assert_eq!(e.text(), "");
    // a selection is wrapped and stays selected
    let mut e = Editor::open("a.json", "abc");
    e.execute(Command::SelectAll).unwrap();
    e.insert("(").unwrap();
    assert_eq!(e.text(), "(abc)");
    assert_eq!(e.selections().primary().range(), Range::new(1, 4));
    // no pair in front of a word
    let mut e = Editor::open("a.json", "abc");
    e.insert("(").unwrap();
    assert_eq!(e.text(), "(abc");
}

#[test]
fn column_selection_and_word_and_line_drags() {
    let mut e = plain("abcdef\nab\nabcdefgh\n");
    e.column_select(Position::new(0, 2), Position::new(2, 5));
    let sels: Vec<(usize, usize)> = e
        .selections()
        .iter()
        .map(|s| (s.start(), s.end()))
        .collect();
    assert_eq!(
        sels,
        vec![(2, 5), (9, 9), (12, 15)],
        "short lines get a caret at their end"
    );
    e.insert("X").unwrap();
    assert_eq!(e.text(), "abXf\nabX\nabXfgh\n");
    let mut e = plain("one two three\nfour five\n");
    e.drag_words(Position::new(0, 5), Position::new(1, 6));
    assert_eq!(
        e.document().slice(e.selections().primary().range()),
        "two three\nfour five"
    );
    e.drag_lines(1, 0);
    assert_eq!(e.selections().primary().range(), Range::new(0, 24));
    assert!(e.selections().primary().is_reversed());
}

#[test]
fn copy_cut_paste_follow_the_usual_rules() {
    let mut e = plain("one\ntwo\nthree\n");
    // nothing selected: the line
    e.execute(Command::MoveDown(false)).unwrap();
    assert_eq!(e.copy_text(), "two\n");
    assert_eq!(e.cut().unwrap(), "two\n");
    assert_eq!(e.text(), "one\nthree\n");
    e.execute(Command::Undo).unwrap();
    assert_eq!(e.text(), "one\ntwo\nthree\n");
    // as many lines as carets: one each
    let mut e = plain("a\nb\n");
    e.execute(Command::AddCursorBelow).unwrap();
    e.paste("1\n2").unwrap();
    assert_eq!(e.text(), "1a\n2b\n");
    // otherwise all of it at every caret, as one undo step
    e.paste("x").unwrap();
    assert_eq!(e.text(), "1xa\n2xb\n");
    e.execute(Command::Undo).unwrap();
    assert_eq!(e.text(), "1a\n2b\n");
}

#[test]
fn find_and_replace_with_regex_case_and_whole_word() {
    let mut e = plain("Cat cat concat\ncat-nap CAT\n");
    let n = |e: &mut Editor, q: &str, o: SearchOptions| e.set_search(q, &o).unwrap();
    assert_eq!(n(&mut e, "cat", SearchOptions::default()), 5);
    assert_eq!(
        n(
            &mut e,
            "cat",
            SearchOptions {
                case_sensitive: true,
                ..Default::default()
            }
        ),
        3
    );
    assert_eq!(
        n(
            &mut e,
            "cat",
            SearchOptions {
                whole_word: true,
                ..Default::default()
            }
        ),
        4
    );
    assert_eq!(
        n(
            &mut e,
            r"c(a)t\b",
            SearchOptions {
                regex: true,
                case_sensitive: true,
                ..Default::default()
            }
        ),
        3
    );
    assert_eq!(e.replace_all("d${1}g", true).unwrap(), 3);
    assert_eq!(e.text(), "Cat dag condag\ndag-nap CAT\n");
    e.execute(Command::Undo).unwrap();
    assert_eq!(
        e.text(),
        "Cat cat concat\ncat-nap CAT\n",
        "replace all is one undo step"
    );
    // stepping wraps around
    n(
        &mut e,
        "cat",
        SearchOptions {
            whole_word: true,
            case_sensitive: true,
            ..Default::default()
        },
    );
    e.execute(Command::MoveDocStart(false)).unwrap();
    e.execute(Command::FindNext).unwrap();
    assert_eq!(e.selections().primary().range(), Range::new(4, 7));
    e.execute(Command::FindNext).unwrap();
    assert_eq!(e.selections().primary().range(), Range::new(15, 18));
    e.execute(Command::FindNext).unwrap();
    assert_eq!(e.selections().primary().range(), Range::new(4, 7));
    assert!(
        e.set_search(
            "(",
            &SearchOptions {
                regex: true,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn set_text_edits_only_what_differs() {
    let e = plain("line one\nline two\nline three\n");
    let (range, insert) = diff_span(e.document(), "line one\nline 2\nline three\n").unwrap();
    assert_eq!((range, insert.as_str()), (Range::new(14, 17), "2"));
    assert!(diff_span(e.document(), "line one\nline two\nline three\n").is_none());
    // a caret after the change stays on its text
    let mut e = plain("aaa\nbbb\nccc\n");
    e.execute(Command::MoveDocEnd(false)).unwrap();
    e.execute(Command::MoveLeft(false)).unwrap();
    let head = e.selections().primary().head;
    e.set_text("aaa\nB\nccc\n").unwrap();
    assert_eq!(e.selections().primary().head, head - 2);
    // repeated characters around the edit
    for (a, b) in [
        ("aaaa", "aaa"),
        ("aaa", "aaaa"),
        ("", "x"),
        ("x", ""),
        ("abab", "abcab"),
        ("é漢", "é字漢"),
    ] {
        let mut e = plain(a);
        e.set_text(b).unwrap();
        assert_eq!(e.text(), b);
        e.execute(Command::Undo).unwrap();
        assert_eq!(e.text(), a);
    }
}

#[test]
fn deferred_analysis_runs_when_due_and_diagnostics_follow_edits() {
    let reg = frontier_editor::schema::SchemaRegistry::builtin();
    let mut e = Editor::open("demo.recipe.json", "{\n  \"name\": 3\n}\n");
    e.use_registry(&reg);
    let before = e.diagnostics();
    assert!(
        !before.is_empty(),
        "the built-in recipe schema flags a numeric name"
    );
    e.options.analysis_delay_ms = 150;
    e.tick(1_000);
    e.execute(Command::MoveDocStart(false)).unwrap();
    e.insert("\n").unwrap();
    assert!(e.analysis_pending());
    // not re-validated yet, but moved with the text
    let moved = e.diagnostics();
    assert_eq!(moved.len(), before.len());
    assert_eq!(moved[0].range.start, before[0].range.start + 1);
    assert!(!e.poll(1_100));
    assert!(e.poll(1_150));
    assert!(!e.analysis_pending());
}

fn big(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 28);
    for i in 0..lines {
        s.push_str(&format!("key_{i}: value number {i}\n"));
    }
    s
}

#[test]
fn a_hundred_thousand_lines_stay_interactive() {
    let text = big(100_000);
    let t = Instant::now();
    let mut e = Editor::open("big.yaml", &text);
    e.options.analysis_delay_ms = 150;
    let open_ms = t.elapsed().as_secs_f64() * 1e3;
    // typing in the middle
    e.execute(Command::GoToLine(50_000)).unwrap();
    let mut worst = 0f64;
    for i in 0..200u64 {
        e.tick(i * 30);
        let t = Instant::now();
        e.insert("x").unwrap();
        let _ = e.view_state(frontier_editor::Viewport {
            first_row: 49_990,
            rows: 60,
        });
        worst = worst.max(t.elapsed().as_secs_f64() * 1e3);
    }
    // select all + copy size, multi-cursor on a page, find, undo
    let t = Instant::now();
    e.execute(Command::SelectAll).unwrap();
    assert!(e.copy_text().len() > 2_000_000);
    e.execute(Command::CollapseSelections).unwrap();
    eprintln!(
        "select all + copy {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    let n = e
        .set_search("number 9999", &SearchOptions::default())
        .unwrap();
    assert_eq!(n, 11);
    eprintln!("+ search {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    e.selections_set(SelectionSet::new(
        (0..500)
            .map(|l| Selection::caret(e.document().line_start(l)))
            .collect(),
        0,
    ));
    e.insert("#").unwrap();
    eprintln!("+ 500 carets {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    e.execute(Command::Undo).unwrap();
    let ops_ms = t.elapsed().as_secs_f64() * 1e3;
    eprintln!(
        "100k lines: open {open_ms:.1} ms, worst keystroke+view {worst:.2} ms, bulk ops {ops_ms:.1} ms"
    );
    // generous bounds (debug builds on a loaded machine); the release numbers are in the README
    assert!(worst < 250.0, "keystroke took {worst:.1} ms");
    assert!(open_ms < 3_000.0 && ops_ms < 3_000.0);
}

#[test]
fn wide_characters_and_clusters_move_and_delete_as_one() {
    let family = "👨\u{200d}👩\u{200d}👧";
    let mut e = plain(&format!("a{family}e\u{301}漢b"));
    let n = family.chars().count();
    e.execute(Command::MoveRight(false)).unwrap();
    e.execute(Command::MoveRight(false)).unwrap();
    assert_eq!(
        e.selections().primary().head,
        1 + n,
        "over the emoji sequence in one step"
    );
    e.execute(Command::MoveRight(false)).unwrap();
    assert_eq!(
        e.selections().primary().head,
        1 + n + 2,
        "over the letter and its accent"
    );
    e.execute(Command::MoveLeft(false)).unwrap();
    assert_eq!(e.selections().primary().head, 1 + n);
    e.execute(Command::Backspace).unwrap();
    assert_eq!(
        e.text(),
        "ae\u{301}漢b",
        "Backspace removes the whole sequence"
    );
    e.execute(Command::Delete).unwrap();
    assert_eq!(e.text(), "a漢b");
    // the cell a caret is at: after a wide character it is two cells further
    let end = e.document().len_chars();
    assert_eq!(e.row_and_cell(end), (0, 4));
    assert_eq!(
        e.position_at(0, 2.6),
        Position::new(0, 2),
        "a press on its right half lands after it"
    );
}

#[test]
fn soft_wrap_rows_follow_edits_and_the_caret_moves_by_visual_row() {
    let mut e = plain("short\nalpha beta gamma delta epsilon\nend\n");
    e.set_wrap(Some(12));
    // line 1 wraps to three rows: "alpha beta " / "gamma delta " / "epsilon"
    assert_eq!(e.row_count(), 1 + 3 + 1 + 1);
    assert_eq!(
        (
            e.row_to_line(0),
            e.row_to_line(1),
            e.row_to_line(3),
            e.row_to_line(4)
        ),
        (0, 1, 1, 2)
    );
    assert_eq!(e.line_to_row(2), 4);
    let vs = e.view_state(frontier_editor::Viewport {
        first_row: 2,
        rows: 3,
    });
    let texts: Vec<(&str, usize, bool)> = vs
        .rows
        .iter()
        .map(|r| (r.text.as_str(), r.col_start, r.continuation))
        .collect();
    assert_eq!(
        texts,
        [
            ("gamma delta ", 11, true),
            ("epsilon", 23, true),
            ("end", 0, false)
        ]
    );
    assert_eq!(vs.total_rows, 6);
    // Down from "al|pha" goes to the row below in the same line, same cell
    e.selections_set(SelectionSet::single(Selection::caret(6 + 2)));
    e.execute(Command::MoveDown(false)).unwrap();
    assert_eq!(e.selections().primary().head, 6 + 11 + 2);
    assert_eq!(e.row_and_cell(e.selections().primary().head), (2, 2));
    e.execute(Command::MoveDown(false)).unwrap();
    e.execute(Command::MoveDown(false)).unwrap();
    assert_eq!(
        e.document()
            .offset_to_position(e.selections().primary().head),
        Position::new(2, 2)
    );
    e.execute(Command::MoveUp(true)).unwrap();
    assert_eq!(e.row_and_cell(e.selections().primary().head), (3, 2));
    // a press on a wrapped row lands in its part of the line; the end of a row stays on it
    assert_eq!(e.position_at(2, 3.0), Position::new(1, 14));
    assert_eq!(e.position_at(1, 40.0), Position::new(1, 10));
    // typing re-measures only the edited line; the count follows
    e.selections_set(SelectionSet::single(Selection::caret(5)));
    e.insert(" and now this line is long too").unwrap();
    assert!(e.row_count() > 6);
    e.execute(Command::Undo).unwrap();
    assert_eq!(e.row_count(), 6);
    // a wider view unwraps; a fold hides its rows
    e.set_wrap(Some(80));
    assert_eq!(e.row_count(), 4);
    e.set_wrap(None);
    assert_eq!(e.row_count(), 4);
}

#[test]
fn wrapped_rows_of_ten_thousand_lines_stay_cheap_to_edit() {
    let mut text = String::new();
    for i in 0..10_000 {
        text.push_str(&format!("key_{i}: a value that is long enough to wrap at sixty cells, number {i} of ten thousand\n"));
    }
    let mut e = Editor::open("big.yaml", &text);
    e.options.analysis_delay_ms = 150;
    e.set_wrap(Some(60));
    let rows = e.row_count();
    assert_eq!(rows, 20_001);
    e.execute(Command::GoToLine(5_000)).unwrap();
    let mut times = Vec::new();
    for i in 0..100u64 {
        e.tick(i * 30);
        let t = Instant::now();
        e.insert("x").unwrap();
        let row = e.cursor_row();
        let _ = e.view_state(frontier_editor::Viewport {
            first_row: row.saturating_sub(10),
            rows: 40,
        });
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(|a, b| a.total_cmp(b));
    eprintln!(
        "wrap, 10k lines: keystroke+view median {:.2} ms, p95 {:.2} ms",
        times[50], times[95]
    );
    // the rows kept up to date edit by edit are the rows a fresh measure gives
    let mut fresh = Editor::open("big.yaml", &e.text());
    fresh.set_wrap(Some(60));
    assert_eq!(e.row_count(), fresh.row_count());
    assert!(e.row_count() > rows);
    assert!(times[50] < 20.0, "median {:.1} ms", times[50]);
}
