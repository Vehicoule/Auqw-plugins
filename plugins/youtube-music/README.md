# youtube-music

Minimal `playback.resolve` provider for Auqw (ABI v0).

## What it does

Resolves a video ID to a direct, fetchable audio URL using anonymous
InnerTube `player` calls. It tries exactly two client rungs, in order:

1. `IOS`
2. `ANDROID_VR`

For each rung it POSTs to the InnerTube `player` endpoint
(`www.youtube.com` — these clients do not use the `music.youtube.com`
host) with the rung's client context, then scans
`streamingData.adaptiveFormats` for `audio/*` entries with a **plain
`url` field**. Entries carrying `signatureCipher`/`cipher` are dropped,
never deciphered. The best format is chosen by distance from 128 kbps
with `audio/mp4` preferred over `audio/webm` on ties.

If every rung yields only ciphered formats, the guest fails with kind
`unsupported` and message `ciphered-only` — a distinct signal from
`no-result`.

## Slice 0 exception

This guest implements the ABI by hand (raw `alloc`/`handle` exports and
JSON step messages). The SDK inversion shim — which will let guests be
written as ordinary capability functions — arrives in Slice 1. Explicitly
out of scope for Slice 0: cookies, signature deciphering, KV, PO tokens,
search, radio, candidates, and any further ladder rungs.

## Build

```sh
../../tooling/build.sh youtube-music
```

Produces `dist/youtube-music.wasm` and updates `manifest.artifact.digest`
via the validator.

## Tests

```sh
cargo test -p auqw-youtube-music
```

Runs natively: parser/triage/selection/expiry unit tests plus step-machine
tests over the fixtures in `fixtures/` (hand-authored minimal shapes).
