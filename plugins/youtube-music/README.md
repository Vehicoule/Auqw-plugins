# youtube-music

`playback.resolve` + `playback.candidates` + `radio.seed` provider for
Auqw (ABI 0.3.0), built on the vendored `auqw-guest-sdk` async
dispatch.

## playback.resolve

Resolves a video ID to a direct, fetchable audio URL using anonymous
InnerTube `player` calls against `music.youtube.com`. `source_ref`
accepts the legacy 11-character video-id string or a
`{provider:"youtube-music",kind:"track",id}` ref; foreign refs are
`not-applicable` before any host call. Optional payload inputs:

- `target_bitrate_kbps` (default 128, range 1..512): pick the audio
  format closest to the target.
- `prefer` (default `["audio/mp4","audio/webm"]`): container
  preference order — a listed base outranks bitrate distance, then
  bitrate distance decides inside the preferred container.
- `pin_itag` (null/absent or u32): constrain selection to exactly that
  itag — never a silent container switch. If no rung serves it, the
  resolve fails `expired-resource`/`pinned-itag-unavailable`.
- `resume_offset` (null/absent or u64): validated seam input for the
  Slice 1.5 re-mint path; byte pumping itself is Slice 1.5-owned.

It walks a five-rung, version-pinned client ladder, in order:

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
https destinations.

## KV visitors and backoff

Each rung has a stable KV key (`VISIONOS`, `IOS`,
`ANDROID_VR@<version>`):

- `visitor/<rung-key>` — the last `responseContext.visitorData` that
  rung returned, replayed as `X-Goog-Visitor-Id`. A fresher visitor
  minted earlier in the same invocation is replayed ahead of the
  persisted one; non-UTF-8/empty values are ignored with a sanitized
  warning.
- `backoff/<video-id>/<rung-key>` — `{until_ms, reason}` JSON. A rung
  whose backoff is still in force is skipped (no player call) but its
  reason still participates in the final failure taxonomy — a stored
  `rate-limit` answers `rate-limit`. Failed rungs stage backoffs:
  bot-check 45 s, rate-limit 60 s, transport 5 s, capped probe 5 s; a
  successful rung clears its own key.

`kv_set` writes are **staged**: the host commits them only when the
invocation ends `done`. Failed-rung backoffs and early visitors persist
when a later rung succeeds; an all-failed invocation rolls every staged
write back by contract — there is no partial-commit escape hatch.

## playback.candidates

Recording search over the WEB_REMIX metadata client (name id `67`,
version `1.20260114.01.00`, desktop UA) — metadata-only: WEB_REMIX is
never added to the playback ladder because signature deciphering is
out of scope. Payload is `{query:{title,artist,album,duration_ms,
version_labels,isrc},limit}` with strict key sets; `limit` clamps
1..50. Search text is artist + title + version labels + album joined
with single spaces — nulls and the ISRC are never serialized into it.

The guest POSTs `youtubei/v1/search` with the songs filter, walks the
response for `musicResponsiveListItemRenderer` rows (bounded: depth 64,
10 k nodes), keeps upstream order, dedups first-video-id-wins, and maps
each row to `trackMetadata` with honest nulls where upstream is silent.
Artwork is the largest HTTPS thumbnail only. The WEB_REMIX visitor is
persisted under `visitor/web-remix` with the same staging semantics.

## radio.seed

Track-seeded automix over the WEB_REMIX `next` endpoint (same client
identity, headers, and `visitor/web-remix` KV as search). The payload
is the ABI 0.3.0 dual envelope: `{source_ref}` seeds a new mix,
`{continuation}` fetches the next page — exactly one of the two keys.

A seed must be a `youtube-music`/`track` ref with a video id (foreign
and non-track refs are `not-applicable` before any host call). The
request POSTs `youtubei/v1/next` with `videoId` +
`playlistId: RDAMVM<videoId>` and the automix queue fields
(`isAudioOnly`, `enablePersistentPlaylistPanel`,
`tunerSettingValue: AUTOMIX_SETTING_NORMAL`); a continuation request
carries only `context` + the opaque token. The guest never loops — one
`next` call per invocation.

The seed response's `playabilityStatus` is classified by the same
taxonomy as `player`: bot-check → `transient` (`bot-check`), sign-in /
age → `auth-required`, unavailable → `no-result`, so an unavailable
seed fails rather than serving a substituted queue — and a panel whose
`playlistId` names a different queue is `no-result` for the same
reason. Items come from the queue panel's `playlistPanelVideoRenderer`
rows (unwrapping `playlistPanelVideoWrapperRenderer` primaries),
mapped to the same `trackMetadata` shape as search results; upstream
order is kept, first video id wins. `continuation` is the panel's
`nextRadioContinuationData`/`nextContinuationData` token verbatim;
when upstream yields none the result is `continuation: null` — the
honest end of the mix.

Error mapping mirrors `playback.candidates`: 429 → `rate-limit`, other
non-2xx and transport host errors → `transient`,
`cancelled`/`permission-denied`/`invalid-response` propagate, a
non-object 2xx body or a missing queue panel → `invalid-response`.

A picked URL is probed before it is returned: a `Range` request on the
file's last 64 KiB (derived from `contentLength`; a fixed window past
the ~1 MiB horizon when the length is unknown). On the tail window a
206 that reaches the file's last byte proves this mint serves the whole
file; on the fallback window a 206 carrying the asked span — or a
reported EOF inside it — proves the mint serves past the horizon. A
refusal marks the rung **capped** and advances the ladder. The probe
rides the exact URL the downloader will fetch.

Once per resolve the guest may emit a `pot_token` host request — lazy,
bound to the video id (the current upstream binding for both player
and GVS token contexts). A denied or failed mint degrades to the bare
URL; `cancelled` propagates. The one token serves two consumers:

- `pot=` on the googlevideo stream URL (percent-encoded) before the
  tail probe.
- `context.serviceIntegrityDimensions.poToken` on the **attested
  replay**: when bare `player` calls bot-check, the second pass
  replays only those rungs with the token in the request body.
  Live-verified 2026-09: VISIONOS and IOS return full format lists
  attested where bare requests answer `LOGIN_REQUIRED`; ANDROID_VR
  stays walled (VR needs DroidGuard, not BotGuard). With no POT
  provider configured the replay never runs and a second bot-check is
  terminal as before — one locally-denied `pot_token` call is the only
  added cost.

Measured 2026-09-19: `pot=` did not lift a capped IOS mint — serving
caps are enforced independently of attestation — but the same token
lifts the player-level `LOGIN_REQUIRED` wall on web-attestable rungs.

Caps are stochastic per-mint, not per-client: GVS enforcement refused
windows past a ~1 MiB served horizon on some mints and served the whole
file on others, on VISIONOS and ANDROID_VR alike. Mid-stream recovery
is the downloader's job — re-resolve for a fresh mint and resume at the
written offset (`resume_offset` is the seam for it) — so no rung
carries a cap flag and there is no `prefix_limited` result field.

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
recorded rung outcome is a restricted-format verdict, the guest fails
`unsupported` — `sabr-only` or `ciphered-only`, the first such rung
deciding which; a bot-checked or transport-failed rung demonstrated
nothing about plain audio and blocks the claim. The distinction is
preserved for the host.

## Failure mapping

Per rung: non-2xx advances the ladder (429 is remembered), bot-check /
sign-in / age / unavailable playability advances, SABR / ciphered /
no-audio advances, and a refused tail probe advances as `capped`. A
3xx probe is re-requested once against its `Location` — the host
enforces the destination allowlist on every request — before the
verdict lands. A
second bot-check inside one invocation ends the bare pass — the
rungs then get one attested replay each when a POT provider minted,
and `transient` (`bot-check`) only when the wall holds anyway.
`cancelled`
propagates immediately; `permission-denied`/`invalid-response` host
errors are terminal; a `rate-limit` host error stages the rate-limit
backoff and reports as `rate-limit`; other host errors are transport
weather. When the
ladder is exhausted: any 429 or stored rate-limit → `rate-limit`; a
requested pin no rung served → `expired-resource`
(`pinned-itag-unavailable`); every recorded outcome SABR/ciphered →
`unsupported` (`sabr-only` or `ciphered-only`, first such rung
decides); every rung capped → `transient` (`streams-capped`); otherwise
the last rung's bucket outside the restricted-format set decides —
bot-check → `transient` (`bot-check`), sign-in/age → `auth-required`,
unavailable → `no-result`.

For `playback.candidates`: 429 → `rate-limit`, other non-2xx and
transport host errors → `transient`, `cancelled`/`permission-denied`/
`invalid-response` propagate, a non-object 2xx body → `invalid-response`.

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

Runs natively through the SDK harness (`dispatch_step` +
`reset_for_testing`): playability/format classification, pick ranking
and pinning, probe verdicts, the full ladder walk with simulated
KV/backoff/PO-token host, and candidate parsing over the fixtures in
`fixtures/` (hand-authored minimal shapes).
