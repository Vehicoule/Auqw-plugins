#!/bin/sh
# Build one plugin guest for wasm32, stage it into dist/, and validate.
# Usage: tooling/build.sh <plugin-id>   (run from the repo root)
set -eu

plugin="${1:?usage: tooling/build.sh <plugin-id>}"
crate="auqw-${plugin}"
wasm_name="auqw_$(printf '%s' "$plugin" | tr '-' '_').wasm"

cargo build --release --target wasm32-unknown-unknown -p "$crate"

mkdir -p "plugins/${plugin}/dist"
cp "target/wasm32-unknown-unknown/release/${wasm_name}" \
   "plugins/${plugin}/dist/${plugin}.wasm"

cargo run -q -p auqw-validate -- \
    "plugins/${plugin}/dist/${plugin}.wasm" \
    "plugins/${plugin}/manifest.json" --update-digest
