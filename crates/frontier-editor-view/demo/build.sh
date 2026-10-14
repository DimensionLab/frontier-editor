#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Builds the wasm demo into demo/pkg and serves it (needs wasm-bindgen-cli matching Cargo.lock).
#   crates/frontier-editor-view/demo/build.sh [--serve]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/dist/target}"
cd "$ROOT"
# Size-optimised release without touching the workspace profile (the app's own wasm
# build has its own settings): opt-level z, fat LTO, one codegen unit, abort on panic.
CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_LTO=fat \
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_PANIC=abort \
cargo build -p frontier-editor-view --release --target wasm32-unknown-unknown --features web
wasm-bindgen --target web --out-dir "$HERE/pkg" --no-typescript \
  "$TARGET/wasm32-unknown-unknown/release/frontier_editor_view.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz -o "$HERE/pkg/frontier_editor_view_bg.wasm" "$HERE/pkg/frontier_editor_view_bg.wasm"
fi
ls -la "$HERE/pkg" | sed -n '1,10p'
gzip -9 -c "$HERE/pkg/frontier_editor_view_bg.wasm" | wc -c | awk '{printf "wasm gzipped: %.0f kB\n", $1/1024}'
if [[ "${1:-}" == "--serve" ]]; then
  echo "serving http://localhost:8765/ (WebGPU needs Chrome/Edge/Safari 26+)"
  python3 -m http.server 8765 --directory "$HERE"
fi
