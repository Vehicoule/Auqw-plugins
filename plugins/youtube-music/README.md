# youtube-music

Minimal `playback.resolve` provider for Auqw (ABI v0).

## What it does

Resolves a video ID to a direct, fetchable audio URL using anonymous
InnerTube `player` calls against `music.youtube.com`. It walks a
five-rung, version-pinned client ladder, in order:

1. `VISIONOS` 1.02
2. `IOS` 20.10.4
3. `ANDROID_VR` 1.61.48
4. `ANDROID_VR` 1.60.19
5. `ANDROID_VR` 1.43.32

For each rung it POSTs `youtubei/v1/player?prettyPrint=false` with the
rung's client identity (`User-Agent`, `X-YouTube-Client-Name`/`Version`,
`X-Origin`/`Referer: https://music.youtube.com`), then scans
`streamingData.adaptiveFormats` for `audio/*` entries with a **plain
`url` field**. Entries carrying `signatureCipher`/`cipher` are dropped,
never deciphered; non-https URLs are skipped — the host only serves
https destinations. The best format is chosen by distance from 128 kbps
with `audio/mp4` preferred over `audio/webm` on ties.

`responseContext.visitorData` from each response is replayed as
`X-Goog-Visitor-Id` on later rungs within the same invocation. Visitor
IDs are invocation-scoped — persistence is deferred until the KV host
call lands in Slice 1.

## Minted URLs are verified, not trusted

A picked URL is probed before it is returned: a `Range` request on the
file's last 64 KiB (derived from `contentLength`; a fixed window past
the ~1 MiB horizon when the length is unknown). A 206 proves this mint
serves the whole file; a refusal marks the rung **capped** and advances
the ladder. The probe rides the exact URL the downloader will fetch.

Once per resolve, on the first pick, the guest emits a `pot_token` host
request — lazy, bound to the `visitorData` the ladder collected (video
id fallback). The host refuses it unless the manifest declares the
`pot-provider` permission and a provider endpoint is configured, in
which case the refusal arrives as `host_error` and the URL is probed
bare. A minted token decorates the googlevideo URL as `pot=`
(percent-encoded). Measured 2026-09-19: `pot=` did not lift a capped
IOS mint — web BotGuard tokens cannot attest non-web clients — but the
path stays wired for future web-context rungs.

Caps are stochastic per-mint, not per-client: GVS enforcement refused
windows past a ~1 MiB served horizon on some mints and served the whole
file on others, on VISIONOS and ANDROID_VR alike. Mid-stream recovery
is the downloader's job — re-resolve for a fresh mint and resume at the
written offset — so no rung carries a cap flag and there is no
`prefix_limited` result field.

## Why these versions

Client versions are load-bearing. Live probes (Phases A–B, then
2026-09-19 re-verification) showed the InnerTube `player` response shape
and the googlevideo behaviour both change with the client pin:

- `VISIONOS` resolves nearly everywhere and rarely bot-checks from
  residential IPs, so it leads.
- `IOS` 20.10.4 resolves nearly everywhere and returns plain-URL audio
  fetchable without a PO token. `IOS` 21.26.4 (the version yt-dlp
  currently tracks) is served **SABR-only**: audio formats carry
  neither `url` nor `signatureCipher`, only `serverAbrStreamingUrl`.
- `ANDROID_VR` 1.61.48 / 1.60.19 / 1.43.32 URLs serve the **full
  stream** anonymously — any `Range: bytes=` chunk is honoured fast —
  but the rung itself is the most bot-checked (`LOGIN_REQUIRED` / "Sign
  in to confirm you're not a bot") on residential IPs. Leading with
  them created the bot-checks, so they run last.

The versions are pinned deliberately. Do not bump them to track
upstream — newer clients are served strictly worse responses. If every
playable rung serves SABR-only, the guest fails `unsupported` with
message `sabr-only`; cipher-only formats fail `ciphered-only`; the
distinction is preserved for the host.

## Failure mapping

Per rung: non-2xx advances the ladder (429 is remembered), bot-check /
sign-in / age / unavailable playability advances, SABR / ciphered /
no-audio advances, and a refused tail probe advances as `capped`. A
response whose `id` is not the outstanding request's fails
`invalid-response` — a host protocol violation, on every leg. When the
ladder is exhausted: any 429 → `rate-limit`; every playable rung
SABR/ciphered → `unsupported` (`sabr-only` or `ciphered-only`, first
playable rung decides); every rung capped → `transient`
(`streams-capped`); otherwise the last rung's bucket decides —
bot-check → `transient` (`bot-check`), sign-in/age → `auth-required`,
unavailable → `no-result`.

## Slice 0 exception

This guest implements the ABI by hand (raw `alloc`/`handle` exports and
JSON step messages). The SDK inversion shim — which will let guests be
written as ordinary capability functions — arrives in Slice 1. Explicitly
out of scope for Slice 0: cookies, auth, KV persistence, backoff,
signature deciphering, WEB_REMIX, search, radio, candidates.

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
