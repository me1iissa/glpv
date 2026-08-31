#!/usr/bin/env bash
# Builds the viewer's WebAssembly evaluator and embeds it as base64.
# Output: ui/eval-wasm.b64 (picked up by glpv-render at compile time; an
# empty file means "no wasm" and the viewer falls back to its JS evaluator).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -p glpv-wasm --target wasm32-unknown-unknown --profile wasm-release
base64 -w0 target/wasm32-unknown-unknown/wasm-release/glpv_wasm.wasm > ui/eval-wasm.b64
echo "ui/eval-wasm.b64: $(wc -c < ui/eval-wasm.b64) bytes"
