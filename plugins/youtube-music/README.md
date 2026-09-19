# youtube-music

Minimal `playback.resolve` provider for Auqw (ABI v0).

## What it does

Resolves a video ID to a direct, fetchable audio URL using anonymous
InnerTube `player` calls against `music.youtube.com`. It walks a
four-rung, version-pinned client ladder, in order:

1. `ANDROID_VR` 1.61.48
2. `ANDROID_VR` 1.43.32
3. `VISIONOS` 1.02
4. `IOS` 20.10.4

For each rung it POSTs `youtubei/v1/player?prettyPrint=false` with the
rung's client identity (`User-Agent`, `X-YouTube-Client-Name`/`Version`,
`X-Origin`/`Referer: https://music.youtube.com`), then scans
`streamingData.adaptiveFormats` for `audio/*` entries with a **plain
`url` field**. Entries carrying `signatureCipher`/`cipher` are dropped,
never deciphered. The best format is chosen by distance from 128 kbps
with `audio/mp4` preferred over `audio/webm` on ties.

`responseContext.visitorData` from each response is replayed as
`X-Goog-Visitor-Id` on later rungs within the same invocation. Visitor
IDs are invocation-scoped — persistence is deferred until the KV host
call lands in Slice 1.

## Why these versions

Client versions are load-bearing. Live probes (Phases A–B) showed the
InnerTube `player` response shape and the googlevideo behaviour both
change with the client pin:

- `ANDROID_VR` 1.61.48 / 1.43.32 URLs serve the **full stream**
  anonymously — any `Range: bytes=` chunk is honoured fast; a plain
  GET succeeds but is throttled. They run first. They are bot-checked
  (`LOGIN_REQUIRED` / "Sign in to confirm you're not a bot") on some
  IP reputations — that check is per-IP, so they are kept rather than
  dropped.
- `VISIONOS` is likewise often bot-checked but kept as a fallback for
  IPs where it passes.
- `IOS` 20.10.4 resolves nearly everywhere but its URLs are
  **prefix-capped**: GVS requires a PO token for the `ios` client, so
  only the first ~1 MiB is served before 403s. The result carries
  `prefix_limited: true` so hosts can label the stream honestly
  instead of discovering the cap mid-playback.
- `IOS` 21.26.4 (the version yt-dlp currently tracks) is served
  **SABR-only**: audio formats carry neither `url` nor
  `signatureCipher`, only `serverAbrStreamingUrl`.

The versions are pinned deliberately. Do not bump them to track
upstream — newer clients are served strictly worse responses. If every
playable rung serves SABR-only, the guest fails `unsupported` with
message `sabr-only`; cipher-only formats fail `ciphered-only`; the
distinction is preserved for the host.

## Failure mapping

Per rung: non-2xx advances the ladder (429 is remembered), bot-check /
sign-in / age / unavailable playability advances, and SABR / ciphered /
no-audio advances. When the ladder is exhausted: any 429 → `rate-limit`;
every playable rung SABR/ciphered → `unsupported` (`sabr-only` or
`ciphered-only`, first playable rung decides); otherwise the last rung's
bucket decides — bot-check → `transient` (`bot-check`), sign-in/age →
`auth-required`, unavailable → `no-result`.

## Slice 0 exception

This guest implements the ABI by hand (raw `alloc`/`handle` exports and
JSON step messages). The SDK inversion shim — which will let guests be
written as ordinary capability functions — arrives in Slice 1. Explicitly
out of scope for Slice 0: cookies, auth, PO tokens, KV persistence,
backoff, signature deciphering, WEB_REMIX, search, radio, candidates.

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

Runs natively: playability/format classification, scoring, expiry, and
ladder-walk tests over the fixtures in `fixtures/` (hand-authored
minimal shapes).
