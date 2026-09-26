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

## Sign a release

```sh
node tooling/sign.mjs sign plugins/<id>
node tooling/sign.mjs verify releases/<id>/<version>
```

Stages the artifact + manifest into `releases/<id>/<version>/` with
provenance and an ed25519 signature. Signing keys live outside every
repo — layout, payload format, and key custody are documented in
[releases/README.md](releases/README.md).

## Vendored guest SDK

Guest crates depend on `auqw-guest-sdk` by path under a versioned
`vendor/auqw-guest-sdk-<version>/` directory — the `cargo package`d
source of the authoritative SDK in `../auqw/sdk/rust`, including its
GPL-3.0-only license text. Guests currently build on 0.3.0; a vendor dir
persists only while a guest still builds against it (0.2.0 was dropped
once every guest moved to 0.3.0). Normal builds never reach
into a sibling checkout; this repo builds standalone.

To refresh the vendor copy after an authoritative SDK change, a
maintainer with both checkouts runs:

```sh
./tooling/vendor-sdk.sh
```

The script packages `../auqw/sdk/rust`, verifies the package is exactly
`auqw-guest-sdk` at the SDK's own version, and replaces that version's
vendor directory.

## Layout

| Path | Contents |
| --- | --- |
| `plugins/youtube-music` | `playback.resolve` + `playback.candidates` + `radio.seed` + `catalog.suggest` guest (ABI 0.3.0) |
| `plugins/itunes` | iTunes catalog guest (`catalog.search`/`metadata`/`artwork`) |
| `plugins/deezer` | Deezer catalog guest (`catalog.search`/`metadata`/`entity`, ABI 0.3.0) |
| `vendor/auqw-guest-sdk-0.3.0` | Packaged guest SDK 0.3.0 source |
| `tooling/validate` | Standalone artifact validator |
| `tooling/build.sh` | Build + stage + validate one plugin |
| `tooling/sign.mjs` | ed25519 release signing: keygen/sign/verify/pubkey |
| `tooling/vendor-sdk.sh` | Refresh the vendored SDK (needs `../auqw`) |
| `releases/<id>/<version>` | Signed immutable release artifacts (see `releases/README.md`) |
