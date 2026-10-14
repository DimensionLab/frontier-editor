// SPDX-License-Identifier: Apache-2.0
//! Completion, hover, quick fixes and formatting through the editor, on the schemas the
//! crate ships.

use frontier_editor::assist::{CompletionKind, NOTE_WORDS_PYTHON};
use frontier_editor::schema::SchemaRegistry;
use frontier_editor::{Command, Editor, EditorError, Selection, SelectionSet};

fn open(path: &str, text_with_caret: &str) -> Editor {
    let at = text_with_caret.chars().position(|c| c == '|');
    let text: String = text_with_caret.chars().filter(|c| *c != '|').collect();
    let mut e = Editor::open(path, &text);
    e.use_registry(&SchemaRegistry::builtin());
    if let Some(at) = at {
        e.selections_set(SelectionSet::single(Selection::caret(at)));
    }
    e
}

fn labels(e: &Editor) -> Vec<String> {
    e.completions()
        .items
        .iter()
        .map(|c| c.label.clone())
        .collect()
}

fn accept(e: &mut Editor, label: &str) {
    let list = e.completions();
    let c = list
        .items
        .iter()
        .find(|c| c.label == label)
        .unwrap_or_else(|| panic!("no completion {label:?} in {:?}", labels(e)));
    e.accept_completion(c).expect("accepted");
}

#[test]
fn a_training_config_offers_its_keys_and_values() {
    let mut e = open("packages/frontier-train/configs/a.yaml", "|");
    let l = labels(&e);
    assert_eq!(l[0], "data", "the required key first: {l:?}");
    assert!(l.contains(&"task".to_string()) && l.contains(&"model".to_string()));
    accept(&mut e, "task");
    assert_eq!(e.text(), "task: surface");
    assert_eq!(
        e.selections().primary().range().len(),
        7,
        "the default is selected to type over"
    );
    // the values of `task`, the default marked
    let e = open("packages/frontier-train/configs/a.yaml", "task: |");
    let list = e.completions();
    assert_eq!(list.items[0].label, "surface");
    assert_eq!(list.items[0].detail, "default");
    assert!(list.items.iter().all(|c| c.kind == CompletionKind::Value));
    assert!(list.note.is_none());
    // the part of the schema a transient task switches on
    let e = open(
        "packages/frontier-train/configs/a.yaml",
        "task: transient\ndata:\n  |",
    );
    let l = labels(&e);
    assert_eq!(l[0], "path");
    assert!(l.contains(&"history".to_string()), "{l:?}");
    let e = open(
        "packages/frontier-train/configs/a.yaml",
        "task: surface\ndata:\n  |",
    );
    assert!(!labels(&e).contains(&"history".to_string()));
}

#[test]
fn a_recipe_offers_ops_and_the_parameters_of_the_op_written() {
    let e = open(
        "ds/recipes/clean.json",
        "{\"name\": \"r\", \"steps\": [{\"op\": \"prep.fields.re|\"}]}",
    );
    assert_eq!(labels(&e), vec!["prep.fields.rename"]);
    let mut e = open(
        "ds/recipes/clean.json",
        "{\n  \"name\": \"r\",\n  \"steps\": [\n    {\"op\": \"prep.fields.rename\", \"params\": {|}}\n  ]\n}",
    );
    let l = labels(&e);
    assert_eq!(l, vec!["map", "missing", "units"]);
    accept(&mut e, "missing");
    assert!(
        e.text().contains("\"params\": {\"missing\": \"skip\"}"),
        "{}",
        e.text()
    );
    e.execute(Command::Undo).expect("undo");
    assert!(e.text().contains("\"params\": {}"), "one undo step");
}

#[test]
fn the_settings_file_offers_tables_keys_and_values() {
    let mut e = open("frontier.toml", "[ed|");
    assert_eq!(labels(&e), vec!["editor"]);
    accept(&mut e, "editor");
    assert_eq!(e.text(), "[editor");
    let mut e = open("frontier.toml", "[editor]\ncur|\n");
    assert_eq!(
        labels(&e),
        vec!["current_line", "cursor_blink", "cursor_style"]
    );
    accept(&mut e, "cursor_style");
    assert_eq!(e.text(), "[editor]\ncursor_style = \"bar\"\n");
    let e = open("frontier.toml", "[editor]\ncursor_style = |\n");
    assert_eq!(labels(&e), vec!["bar", "block", "underline"]);
}

#[test]
fn a_pipeline_offers_its_required_keys_in_one_step() {
    let mut e = open("a.pipeline.json", "{\n  |\n}");
    let list = e.completions();
    assert_eq!(list.items[0].kind, CompletionKind::Snippet);
    assert_eq!(list.items[0].detail, "format, version, name, nodes, edges");
    e.accept_completion(&list.items[0]).expect("accepted");
    assert_eq!(
        e.text(),
        "{\n  \"format\": \"frontier-pipeline\",\n  \"version\": 1,\n  \"name\": \"\",\n  \"nodes\": [],\n  \"edges\": []\n}"
    );
    assert!(e.diagnostics().is_empty(), "{:?}", e.diagnostics());
}

#[test]
fn python_completes_words_and_says_so() {
    let mut e = Editor::open("train.py", "learning_rate = 1e-3\nprint(lea");
    e.set_mode(frontier_editor::Mode::Edit);
    e.selections_set(SelectionSet::single(Selection::caret(
        e.document().len_chars(),
    )));
    let list = e.completions();
    assert_eq!(list.note, Some(NOTE_WORDS_PYTHON));
    assert_eq!(list.items[0].label, "learning_rate");
    e.accept_completion(&list.items[0]).expect("accepted");
    assert_eq!(e.text(), "learning_rate = 1e-3\nprint(learning_rate");
}

#[test]
fn hover_reads_the_schema() {
    let e = open("frontier.toml", "[editor]\nfont_size = 13\n");
    let h = e.hover(12).expect("hover");
    assert_eq!(h.path, "editor.font_size");
    assert_eq!(h.title, "Font size (px)");
    assert_eq!(h.type_name, "integer");
    assert_eq!(h.default.as_deref(), Some("13"));
    assert!(h.description.starts_with("Size of the editor's text"));
    let e = open("packages/frontier-train/configs/a.yaml", "task: surface\n");
    let h = e.hover(8).expect("hover on the value");
    assert_eq!(h.values, vec!["surface", "finetune", "finetune-compare"]);
    assert!(Editor::open("notes.txt", "task").hover(1).is_none());
}

#[test]
fn quick_fixes_add_a_required_key_and_replace_an_unknown_value() {
    let mut e = open("ds/recipes/clean.json", "{\n  \"steps\": []\n}");
    assert_eq!(e.diagnostics().len(), 1);
    let fixes = e.quick_fixes(0);
    assert_eq!(fixes[0].title, "Add \"name\"");
    e.apply_fix(&fixes[0]).expect("applied");
    assert_eq!(e.text(), "{\n  \"name\": \"\",\n  \"steps\": []\n}");
    assert!(e.diagnostics().is_empty());
    e.execute(Command::Undo).expect("undo");
    assert_eq!(e.text(), "{\n  \"steps\": []\n}");

    let mut e = open("frontier.toml", "[editor]\ncursor_style = \"blok\"\n");
    assert_eq!(e.diagnostics().len(), 1);
    let at = e.text().find("blok").expect("value");
    assert!(!e.problems_at(at).is_empty());
    let fixes = e.quick_fixes(at);
    assert_eq!(fixes[0].title, "Replace with \"block\"");
    e.apply_fix(&fixes[0]).expect("applied");
    assert_eq!(e.text(), "[editor]\ncursor_style = \"block\"\n");
    assert!(e.diagnostics().is_empty());
    // from anywhere on the line
    let e = open("frontier.toml", "[editor]\ncursor_style = \"blok\"\n");
    assert_eq!(e.quick_fixes(10).len(), 3);
}

#[test]
fn format_is_one_undo_step_and_says_why_it_cannot() {
    let mut e = open("ds/recipes/clean.json", "{\"name\":\"r\",\"steps\":[]}");
    assert!(e.can_format());
    assert_eq!(e.format(), Ok(true));
    assert_eq!(e.text(), "{\n  \"name\": \"r\",\n  \"steps\": []\n}\n");
    assert_eq!(e.format(), Ok(false), "already formatted");
    e.execute(Command::Undo).expect("undo");
    assert_eq!(e.text(), "{\"name\":\"r\",\"steps\":[]}");
    e.options.tab_size = 4;
    assert_eq!(e.format(), Ok(true));
    assert!(e.text().starts_with("{\n    \"name\""));

    let mut e = open("frontier.toml", "# mine\n[editor]\nfont_size=14   # big\n");
    assert_eq!(e.format(), Ok(true));
    assert_eq!(e.text(), "# mine\n[editor]\nfont_size = 14   # big\n");

    let mut e = open("ds/recipes/clean.json", "{\"name\": }");
    match e.format() {
        Err(EditorError::Format(m)) => assert!(
            m.starts_with("Format unavailable: the text has a syntax error"),
            "{m}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(e.text(), "{\"name\": }");

    let mut e = Editor::open("train.py", "x=1\n");
    e.set_mode(frontier_editor::Mode::Edit);
    assert!(!e.can_format());
    assert_eq!(
        e.format(),
        Err(EditorError::Format(
            "Format unavailable: there is no formatter for Python here".into()
        ))
    );
}

#[test]
fn scripts_are_highlighted_by_their_extension() {
    use frontier_editor::Language;
    assert_eq!(
        Editor::open("tools/build.sh", "echo hi\n").language(),
        Language::Shell
    );
    assert_eq!(
        Editor::open("src/main.rs", "fn main() {}\n").language(),
        Language::Rust
    );
    assert_eq!(
        Editor::open("run", "#!/bin/sh\necho hi\n").language(),
        Language::Shell
    );
    assert_eq!(
        Editor::open("notes.txt", "hello\n").language(),
        Language::Plain
    );
}
