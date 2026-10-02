#!/bin/sh
# Build one plugin guest for wasm32, stage it into dist/, and validate.
# Usage: tooling/build.sh <plugin-id>   (run from the repo root)
set -eu

plugin="${1:?usage: tooling/build.sh <plugin-id>}"
crate="auqw-${plugin}"
wasm_name="auqw_$(printf '%s' "$plugin" | tr '-' '_').wasm"

# Normalize embedded cargo and toolchain/sysroot paths so the artifact
# digest is identical on every machine — the same export lives in
# .github/workflows/ci.yml. The rust-src remap must come FIRST: with the
# rust-src component installed, std item paths embed the local
# <sysroot>/lib/rustlib/src/rust tree; CI has no rust-src, so its std
# paths stay at the abstract /rustc/<commit-hash> prefix. Mapping ours
# onto the same prefix keeps local release builds byte-identical to CI's.
export RUSTFLAGS="--remap-path-prefix=$(rustc --print sysroot)/lib/rustlib/src/rust=/rustc/$(rustc -vV | awk '/commit-hash/{print $2}') --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$(rustc --print sysroot)=/sysroot ${RUSTFLAGS:-}"

cargo build --locked --release --target wasm32-unknown-unknown -p "$crate"

mkdir -p "plugins/${plugin}/dist"
cp "target/wasm32-unknown-unknown/release/${wasm_name}" \
   "plugins/${plugin}/dist/${plugin}.wasm"

cargo run -q --locked -p auqw-validate -- \
    "plugins/${plugin}/dist/${plugin}.wasm" \
    "plugins/${plugin}/manifest.json" --update-digest
