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

## Vendored guest SDK

Guest crates depend on `auqw-guest-sdk` by path under
`vendor/auqw-guest-sdk-0.2.0/` — the `cargo package`d source of the
authoritative SDK in `../auqw/sdk/rust` (version 0.2.0), including its
GPL-3.0-only license text. Normal builds never reach into a sibling
checkout; this repo builds standalone.

To refresh the vendor copy after an authoritative SDK change, a
maintainer with both checkouts runs:

```sh
./tooling/vendor-sdk.sh
```

The script packages `../auqw/sdk/rust`, verifies the package is exactly
`auqw-guest-sdk` 0.2.0, and replaces the versioned vendor directory.

## Layout

| Path | Contents |
| --- | --- |
| `plugins/youtube-music` | `playback.resolve` + `playback.candidates` guest (ABI 0.2.0) |
| `plugins/itunes` | iTunes catalog guest (`catalog.search`/`metadata`/`artwork`) |
| `vendor/auqw-guest-sdk-0.2.0` | Packaged guest SDK 0.2.0 source |
| `tooling/validate` | Standalone artifact validator |
| `tooling/build.sh` | Build + stage + validate one plugin |
| `tooling/vendor-sdk.sh` | Refresh the vendored SDK (needs `../auqw`) |
