# Auqw Plugins

Provider plugins for Auqw: sandboxed WebAssembly guests that resolve
playback and metadata through the host's step ABI (v0).

The product, architecture, and slice plan live in
[../docs/README.md](../docs/README.md); the ABI contract lives in
`../auqw/sdk/contract/`.

## Setup

Requires Rust (see `rust-toolchain.toml`; `wasm32-unknown-unknown` is
pinned).

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Build a plugin artifact

```sh
./tooling/build.sh youtube-music
```

Builds the guest for `wasm32-unknown-unknown`, copies the artifact to
`plugins/<id>/dist/`, and runs the validator (`tooling/validate`), which
checks size/imports/exports/start-section and updates
`manifest.artifact.digest`.

## Layout

| Path | Contents |
| --- | --- |
| `plugins/youtube-music` | Slice 0 minimal `playback.resolve` guest |
| `tooling/validate` | Standalone artifact validator |
| `tooling/build.sh` | Build + stage + validate one plugin |
