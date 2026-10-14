# frontier-editor

Host-agnostic editor core for Frontier: the state of a text editor without rendering.
`frontier-editor-view` draws it with wgpu. Apache-2.0.

```rust
use frontier_editor::{Editor, Viewport, Command, provenance::Author, schema::SchemaRegistry};

let mut ed = Editor::open("configs/train.yaml", text);      // kind + language detected
ed.use_registry(&SchemaRegistry::builtin());                 // JSON-Schema diagnostics
ed.tick(now_ms);                                             // host clock (undo grouping)
ed.insert("x")?;                                             // typing / IME / paste
ed.execute(Command::Undo)?;
let view = ed.view_state(Viewport { first_row: 0, rows: 40 }); // what a view draws
let event = ed.save(Author::User { id: "ada".into() });      // EditEvent for provenance
```

`Editor::view_state` is the only call a renderer needs.

## Modules

| Area | Types | Notes |
|------|-------|-------|
| Document | `Document`, `Position`, `Range` | Rope; char offsets everywhere, byte / UTF-16 / line-col conversions, word boundaries |
| Selections | `Selection`, `SelectionSet` | Multi-cursor, normalised (sorted, merged), goal column for vertical moves |
| Edits | `Change`, `Transaction`, `EditKind` | Sorted non-overlapping replacements, inverse on apply, offset mapping |
| History | `History`, `UndoGroup` | Undo tree (branches are kept, redo follows the latest), typing groups by time or explicit group, dirty tracking |
| Diff | `ChangeSet`, `Hunk`, `HunkLine`, `LineOp` | Line diff with intra-line emphasis; hunk ids are content hashes; `apply_accepted` / `transaction_for` for per-hunk accept and reject |
| Syntax | `Highlighter`, `Language`, `TokenKind`, `Span` | tree-sitter with incremental re-parse, queried for the visible byte range only; a line lexer elsewhere (see below) |
| Structure | `fold`, `brackets` | Indentation, bracket and heading folds found on demand (`fold_range_at`); bracket matching that skips strings |
| Search | `Search`, `SearchOptions` | Literal, regex, whole word; per-line or multi-line; `$1` groups in replacements |
| Schema | `SchemaRegistry`, `SchemaEntry`, `schema::parse` | JSON, YAML and TOML parsed with a JSON-pointer to range map and validated against JSON Schema; a syntax error is one diagnostic |
| Assist | `assist::completions`, `assist::hover`, `assist::quick_fixes`; `Editor::completions`, `accept_completion`, `hover`, `quick_fixes`, `apply_fix` | Keys and values valid at the caret, hover text and fixes from the JSON Schema of the file's kind; reads the caret's place from the text, so it works while the document does not parse. Words of the file where there is no schema |
| Format | `format::format`, `Editor::format` | JSON re-laid out from its tokens; YAML and TOML spacing and blank lines only, returned only when the value and every comment are unchanged; one undo step |
| Locks | `ReadOnlyRange` | A transaction touching a locked range is refused with the reason; locks follow the text through edits |
| Decorations | `Diagnostic`, `Severity`, `Decoration`, `DecorationKind` | Per-source diagnostics (schema, plugins, host), highlights, gutter marks, inline widgets |
| Commands | `Command`, `Key`, `Keymap` | Movement, word operations, multi-cursor (`Cmd+D`, `Alt+Shift+Up/Down`), line operations, comment, indent, undo tree, find, fold, bracket jump, save / apply / leave |
| Plugins | `Plugin`, `Registry` | Languages, commands with default keys, lints, decorations; `WhitespaceLint` built in |
| Provenance | `EditEvent`, `ApprovalEvent`, `Author`, `content_hash` | `save()` diffs against the last saved text; `finish_review()` records accepted and rejected hunk ids and the result hash |
| Large files | `DocumentSource`, `ChunkedDocument`, `LineIndex` | Range-read documents with a sparse newline index (256 lines per entry) and a 16-chunk cache; missing ranges are reported for the host to fetch |
| View | `ViewState`, `ViewLine`, `Viewport`, `GutterMark`, `FoldState` | Visible rows after folding, with spans, highlights, cursors, widgets and gutter marks |

## Features

| Feature | Default | Adds |
|---------|---------|------|
| `schema` | yes | JSON-Schema diagnostics for JSON, YAML and TOML |
| `lang-json`, `lang-yaml`, `lang-toml`, `lang-python`, `lang-markdown` | yes | tree-sitter grammars |
| `lang-rust`, `lang-shell`, `lang-java`, `lang-matlab` | no | tree-sitter grammars; `all-languages` enables every grammar |

The line lexer is used instead of tree-sitter on `wasm32`, for Tcl and logs, and for files
larger than `Editor::TREE_SITTER_BUDGET_BYTES` (512 KB).

## Schemas

`SchemaRegistry::builtin()` embeds the schemas in `schemas/`, a generated copy of
[docs/spec/schemas](../../docs/spec/schemas):

`frontier-train-config`, `runspec`, `jobspec`, `recipe`, `pipeline-v1`, `hub-entry`,
`benchmark-spec`, `frontier-config` (`frontier.toml`), `plugin-manifest` (`plugin.toml`).

## Development

```sh
pnpm nx run frontier-editor:test
pnpm nx run frontier-editor:lint
cargo check --target wasm32-unknown-unknown -p frontier-editor
cargo bench -p frontier-editor
pnpm nx run schemas:export        # regenerate schemas/
```
