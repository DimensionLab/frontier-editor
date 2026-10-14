# frontier-editor-view

wgpu text view for `frontier-editor`. It draws documents, proposals with per-hunk accept and
reject, and multi-million-line logs into a render pass the host already began. Apache-2.0.

```rust
use frontier_editor_view::{EditorView, ViewConfig, Theme, InputEvent, HostRequest, Placement};

let mut view = EditorView::new(ViewConfig::default(), Theme::dark());
view.resize(logical_w, logical_h, dpr);
view.warm_up(&device, &queue, surface_format, 1);        // at mount: pipeline + ASCII atlas
view.open("configs/train.yaml", text);                    // or open_editor / open_log
for req in view.handle(InputEvent::Text("x".into()), now_ms) {
    match req { HostRequest::Save => { /* editor_mut().save(author) */ } _ => {} }
}
view.prepare(&device, &queue, surface_format, 1, &Placement { scale: board_zoom, x, y, target_w, target_h, clip: None }, now_ms);
// inside the host's render pass:
view.render(&mut pass);
```



## Host contract

| Topic | Behaviour |
|-------|-----------|
| Rendering | One pipeline and one instanced draw per view. `render` sets a scissor to the view's rectangle and draws into the host's pass, so the view shares the device with other viewers. |
| Virtualisation | Only the rows in the viewport are shaped and drawn. Documents come from `Editor::view_state`, logs from the core's `ChunkedDocument`. |
| Frames | `prepare(now_ms)` drives smooth scrolling and cursor blink. `needs_redraw(now)` says when another frame is wanted. |
| Zoom | `Placement::scale` re-rasterises glyphs at device pixel ratio × zoom; bitmaps are not scaled. |
| Text input | `InputEvent::Text` and `InputEvent::Composition*` come from a platform text field: a hidden `<textarea>` on the web, IME events natively. Keys handled by the core keymap arrive as `InputEvent::Key`. `HostInput` documents the overlay contract. |
| Review | `Editor::review(proposed)` switches to review mode. The view shows inline or side-by-side hunks with accept and reject regions. `Cmd/Ctrl+Enter` returns `HostRequest::Apply`; the host calls `finish_review(approver)` and receives the `ApprovalEvent`. |
| Minimap | A per-line summary with diagnostics and hunks as marks. Clicks and drags scroll. |
| Web | Feature `web` exports `WebEditor` for `wasm32-unknown-unknown`. |

Text is shaped and rasterised with `cosmic-text` (MIT/Apache-2.0) into a glyph atlas with
subpixel positioning. The embedded font is DejaVu Sans Mono (`assets/`, Bitstream Vera
licence, `assets/LICENSE_DEJAVU`), so web and native builds shape text identically.

## Demos

| Demo | Command |
|------|---------|
| Native | `cargo run -p frontier-editor-view --release --example native -- [file \| --log \| --review]` |
| Web (WebGPU) | `demo/build.sh --serve`, then <http://localhost:8765/> |

Native keys: `F2` theme, `F3` side-by-side diff, `F5` host zoom, `Cmd/Ctrl+S` prints the
`EditEvent`, `Cmd/Ctrl+Enter` applies accepted hunks.

## Development

```sh
pnpm nx run frontier-editor-view:test
pnpm nx run frontier-editor-view:lint
cargo check -p frontier-editor-view --target wasm32-unknown-unknown --features web
cargo bench -p frontier-editor-view
cargo run -p frontier-editor-view --release --example native -- --bench
```
