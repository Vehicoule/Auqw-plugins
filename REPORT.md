# playback.resolve probe: cutting cold-resolve cost

Task: overlap the resolve's range probe with handoff — cut cold-resolve
RTT in the youtube-music guest. Branch `devin/pb-probe`, base
`origin/main`.

## What the probe actually asserts

`probe_verdict` (`plugins/youtube-music/src/guest.rs`) is not a URL
liveness check. GVS mints are capped per-mint, stochastically: a
strict-mode mint serves only up to a horizon H (~1 MiB, varies per
mint). To prove a mint is uncapped the guest must observe bytes served
*past* H:

- **Known `contentLength`:** a `Range` ask ending at the file's last
  byte. A 206 passes only when `Content-Range` starts at the asked
  offset AND `end + 1 == total` (reaches EOF) AND `body.len()` equals
  the advertised span. The evidence is the range's END at EOF — the
  width transferred carries no evidence.
- **Unknown length:** a fixed 64 KiB window at 1 MiB
  (`bytes=1048576-1114111`). A 206 must carry the whole asked span (or
  report an EOF inside it via `end + 1 == total`). Width *is* the
  evidence here: the window must extend past H, so its 64 KiB span is
  load-bearing.
- **Taxonomy:** 429 → `RateLimited`; 5xx → `Transport`; everything
  else (403, other non-2xx, malformed/foreign `Content-Range`, span
  mismatch) → `Capped`, which stages a 5 s rung backoff and advances
  the ladder. A bare-URL fallback probe that 416s passes only when
  `Content-Range: bytes */N` shows the file ends before the ask.

Probe failure advances the **ladder** (same-rung retry, then next
rung), not the resolve — `PickOutcome::Advance(RungOutcome::Capped)`.
The classification is the ladder's input, so it is load-bearing: a
probe that merely checked "URL alive" would pass capped mints and the
caller would mint sessions against URLs that die ~1 MiB in.

## Option taken

**Shrink the known-length tail ask from 64 KiB to 1 byte at EOF**
(`PROBE_TAIL_BYTES = 1`); keep the synchronous probe and every verdict
arm byte-identical.

- `rungs.rs`: `PROBE_TAIL_BYTES: u64 = 1`; new
  `PROBE_FALLBACK_BYTES: u64 = 65536` so the unknown-length window keeps
  its cap-horizon-crossing width (it previously reused
  `PROBE_TAIL_BYTES` — splitting the constants was required, else the
  fallback would have silently shrunk to `bytes=1048576-1048576` and
  lost its evidence).
- `guest.rs`: the fallback arm of `probe_verdict` now references
  `PROBE_FALLBACK_BYTES`; the tail arm is unchanged (`end + 1 == total`
  still keys on EOF, not width).
- Verdict equivalence: for a known-length file the passing ask is
  exactly `{len-1}-{len-1}` — `start == start_asked` and
  `end + 1 == len == total` are the same proof the 64 KiB tail
  produced. Refusals map identically.
- `tooling/journeys/ytm-resolve.json` and
  `fixtures/probe-tail.bin` updated to the 1-byte ask/answer.

This is a transfer-time cut, not an RTT cut: the probe still costs one
serial round trip between the player response and `done`. What it
removes is the 64 KiB body the old tail pulled on every cold resolve —
on a slow mobile link the gap between "headers arrive" and "64 KiB
drained" is real (and counted in the 10 s request budget). It also
shrinks the *failure* cost symmetrically: a capped mint now refuses a
1-byte ask instead of a 64 KiB one.

No device/network measurement was possible in this environment — the
RTT-vs-transfer split of the saving is asserted from the request shape,
not measured. Evidence below is harness-level (provisional).

## Why not the alternatives

### Deferred/async probe (options 1 and 3) — ABI-infeasible

The step ABI forbids it, three independent ways:

- **Sequential host calls.** `HostCall::poll` returns
  `concurrent host calls are not supported` on a second outstanding
  call (`vendor/auqw-guest-sdk-0.3.0/src/lib.rs:538,559`). The probe
  cannot overlap *anything* else inside the guest, let alone the host's
  session mint, which happens after `done` anyway.
- **Terminal result.** `done`/`fail` call `clear_state()`
  (`lib.rs:261,496,502`) and the host creates **a fresh Wasmi instance
  per invocation** (`crates/plugin-host/src/invoke.rs:274`, sibling
  repo). A guest-spawned probe cannot outlive `done`; there is no task
  executor, no deferred-verdict channel, no continuation.
- **Closed result schema.** `resolve_resource_from` rejects
  `additionalProperties`
  (`crates/host-surface/src/lib.rs:1082-1096`): a "pending probe"
  verdict field cannot ride the existing result. Surfacing a late
  verdict would need an ABI bump plus a host/consumer protocol — a
  decision-log event, not a guest-side change.

The session head-fill (`auqw-stream`: head 3 MiB, read_ahead 4 MiB,
stall 10 s, `probe_bytes` 64 KiB, remint on 403/qualifying 416) does
re-validate the URL at attach — but its failure surfaces *mid-attach*
to the player, whereas the probe's `Capped` verdict is what walks the
ladder to a better rung *before* any session exists. Dropping the
guest probe entirely would turn per-rung cap knowledge into per-session
error recovery, which is exactly the contract the taxonomy protects.

### `bytes=0-0` / HEAD-equivalent (option 2, small end) — weakens cap detection

A 1-byte ask at offset 0 is served by *every* mint, capped or not —
the horizon only refuses ranges *ending past* H. The EOF assertion is
the load-bearing part; `bytes=0-0` deletes it. The chosen shape keeps
the assertion and moves the ask to where the proof is: the file's last
byte.

### Probe-skip / spec cache (option 4) — unsafe and policy-blocked

- In-guest memory does not survive invocations (fresh Wasmi instance),
  so only KV could carry a cache.
- Caps are **per-mint stochastic**: "probed-ok" on URL A says nothing
  about freshly-minted URL B for the same videoId. A sound cache would
  have to re-serve the *same* minted URL — i.e. persist the resolve
  spec, which embeds signed-URL material (client IP, signature,
  `pot=`).
- Host KV policy: credentials never belong in the store, and the
  host's own convention hashes signed URLs rather than persisting them
  (`dev_prepare_url`). Value caps (64 KiB/value, 256 KiB/namespace)
  turn an unbounded spec cache into a terminal `invalid-response` at
  overflow — a resolve that fails *because of its own cache* is worse
  than the probe.

No cache was added.

## Deliberately unchanged

- Every `probe_verdict` arm: pass conditions, `Capped`/`RateLimited`/
  `Transport` mapping, the 416-until-EOF fallback carve-out, the
  googlevideo-allowlist pre-check, the one-redirect re-ask.
- `finish_pick`/`mint_once` ordering: `pot_token` mint still precedes
  the probe (the probe rides the final decorated URL), the probe still
  precedes `done`.
- Unknown-length fallback range `bytes=1048576-1114111` and its full
  64 KiB span check.
- poToken stays optional: no provider → bare pass, no behavior change.
- Request budgets, per-invocation HTTP bounds, staged-KV semantics.
- Error taxonomy byte-identical: `bot-check`, `streams-capped`,
  `auth-required`, `rate-limit`, `expired-resource`, `unsupported`,
  `no-result`.

## Tests

`cargo test -p auqw-youtube-music` — 195 passing:

- `probe_of`/`answer_probe_206` harness now drives the 1-byte ask
  (`bytes=4557664-4557664`) with a 1-byte answer.
- Verdict edges at the new ask: wrong `total` (range doesn't reach the
  real EOF), empty body, oversized body → `Capped` (rung advances).
- Fallback arms unchanged (`bytes=1048576-1114111`, 416 short-file
  pass, bare/large-total 416 → `Capped`, short body → `Capped`).
- Full ladder walk, KV/backoff, pot-mint and concurrent-resolve harness
  tests untouched and green.

`tooling/journeys/ytm-resolve.json` updated to the new pinned range and
1-byte body. Not executed here: the integration runner verifies signed
releases and the release key lives outside the repos — harness evidence
is provisional per the repo's evidence rules.

## Gates

```
cargo fmt --check                                          pass
cargo test -p auqw-youtube-music                           195 passed, 0 failed
cargo clippy --all-targets -- -D warnings                  pass
cargo build --release --target wasm32-unknown-unknown      pass
```

## Diffstat

`git diff --stat origin/main...HEAD`:

```
 REPORT.md                                     | 183 ++++++++++++++++++++++++++
 plugins/youtube-music/README.md               |   4 +-
 plugins/youtube-music/fixtures/probe-tail.bin | Bin 65536 -> 1 bytes
 plugins/youtube-music/src/guest.rs            |  49 ++++++--
 plugins/youtube-music/src/rungs.rs            |  26 ++--
 tooling/journeys/ytm-resolve.json             |   6 +-
 6 files changed, 241 insertions(+), 27 deletions(-)
```
