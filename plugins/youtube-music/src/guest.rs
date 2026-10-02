//! Capability dispatch plus `playback.resolve`: walk the pinned client
//! ladder until a rung yields plain audio, then decorate + probe the
//! minted stream URL before reporting it. Guest-side of ABI 0.3.0 over
//! the vendored SDK.
//!
//! Per-rung state lives in the host KV namespace: `visitor/<rung-key>`
//! replays that client's last `responseContext.visitorData`,
//! `backoff/<edge>/<video-id>/<rung-key>` skips a rung that recently
//! failed on that serving edge, `ladder/last-good` leads the next
//! resolve's attempt order with the rung that finished the last one,
//! and `ladder/last-edge` opens the next resolve on the edge that
//! served it. `kv_set` stages writes the host commits only on `done`
//! — a failed resolve rolls its staged visitors/backoffs back by
//! contract. KV is advisory: transient store errors degrade to the
//! empty-store behavior, never a wedge.

use auqw_guest_sdk::{
    http_request, kv_get, kv_set, log, now_ms, pot_token, GuestError, GuestFuture, HttpResponse,
    Invocation, LogLevel,
};
use serde_json::{json, Map, Value};

use crate::parse::{
    classify_playability, format_outcome, pick_audio, visitor_data, visitor_token, FormatOutcome,
    PickOptions, Playability,
};
use crate::rungs::{
    append_pot, is_googlevideo, player_request, probe_request, LADDER, PROBE_FALLBACK_BYTES,
    PROBE_FALLBACK_START, PROBE_TAIL_BYTES,
};

/// Backoff windows staged for a failed rung, keyed by reason.
const BOT_BACKOFF_MS: u64 = 45_000;
const RATE_LIMIT_BACKOFF_MS: u64 = 60_000;
const TRANSPORT_BACKOFF_MS: u64 = 5_000;
const CAPPED_BACKOFF_MS: u64 = 5_000;

/// `ladder/last-good` — the `kv_key` of the rung that finished the
/// last `done` resolve. A pure attempt-order hint: see `last_good`.
const LAST_GOOD_KEY: &str = "ladder/last-good";
/// `pot/aside` records the ms timestamp until which the poToken mint
/// is skipped: a provider that just failed decorates nothing when it
/// degrades anyway, so the next resolve skips the serial call — one
/// retry per window keeps the attested path open when the sidecar
/// comes back. Provider-side weather (denied, unreachable, 5xx, a
/// malformed 200) is remembered globally; a refusal that can bind to
/// a video (4xx) goes under `pot/aside/<video>` so one video's
/// refusal never suppresses another's mint.
const POT_ASIDE_KEY: &str = "pot/aside";
const POT_ASIDE_MS: u64 = 60_000;

/// `ladder/last-edge` — which serving edge finished the last `done`
/// resolve: "b" marks the redraw host as the one to open with. The
/// hint self-corrects on the next success either way, so a stale
/// value costs at most one wrongly-ordered resolve.
const LAST_EDGE_KEY: &str = "ladder/last-edge";

/// The host's per-invocation HTTP budget (`max_http_calls` in
/// `plugin-host/src/budgets.rs`). A rung attempt's worst chain is
/// three calls — player, probe, one redirect re-probe — so a remedy
/// pass emits a request only while that chain still fits; past the
/// bound the rung would die mid-draw `budget-exceeded` either way.
/// In-flight re-asks (401 token-drop, walled-visitor drop) hold the
/// same bound: they run only while the probe chain behind them still
/// has room.
const HTTP_CALL_BUDGET: u32 = 32;
const RUNG_CHAIN_CALLS: u32 = 3;

/// What one rung attempt produced; recorded per rung for the final
/// `fail` kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RungOutcome {
    Bot,
    SignIn,
    Age,
    Unavailable,
    SabrOnly,
    CipheredOnly,
    NoAudio,
    RateLimited,
    Transport,
    /// The rung's minted URL refused the boundary probe — a capped mint.
    Capped,
}

pub fn dispatch(inv: Invocation) -> GuestFuture {
    Box::pin(async move {
        match inv.capability.as_str() {
            "playback.resolve" => resolve(&inv.payload).await,
            "playback.candidates" => crate::candidates::candidates(&inv.payload).await,
            "radio.seed" => crate::radio::radio_seed(&inv.payload).await,
            "catalog.suggest" => crate::suggest::suggest(&inv.payload).await,
            other => Err(failed(
                "not-applicable",
                format!("capability {other} not supported"),
            )),
        }
    })
}

pub(crate) fn failed(kind: &str, message: String) -> GuestError {
    GuestError::Failed {
        kind: kind.into(),
        message,
    }
}

pub(crate) fn bad_payload(m: &str) -> GuestError {
    failed("invalid-response", format!("payload: {m}"))
}

/// The payload object must contain only `allowed` keys and every key in
/// `required` — missing required fields or extras are
/// `invalid-response`.
pub(crate) fn payload_keys<'a>(
    payload: &'a Value,
    allowed: &[&str],
    required: &[&str],
) -> Result<&'a Map<String, Value>, GuestError> {
    let obj = payload
        .as_object()
        .ok_or_else(|| bad_payload("must be an object"))?;
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(bad_payload("unexpected key"));
        }
    }
    for k in required {
        if !obj.contains_key(*k) {
            return Err(bad_payload("missing key"));
        }
    }
    Ok(obj)
}

/// A YouTube video id is exactly 11 `[A-Za-z0-9_-]` ASCII characters.
pub(crate) fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A validated `playback.resolve` payload.
struct ResolvePayload {
    video_id: String,
    target_bitrate_kbps: u32,
    prefer: Vec<String>,
    pin_itag: Option<u32>,
    resume_offset: Option<u64>,
    access_token: Option<String>,
}

fn parse_resolve_payload(payload: &Value) -> Result<ResolvePayload, GuestError> {
    let obj = payload_keys(
        payload,
        &[
            "source_ref",
            "target_bitrate_kbps",
            "prefer",
            "pin_itag",
            "resume_offset",
            "access_token",
        ],
        &["source_ref"],
    )?;
    let video_id = match &obj["source_ref"] {
        Value::String(s) => {
            if s.is_empty() {
                return Err(bad_payload("source_ref must be nonempty"));
            }
            s.clone()
        }
        Value::Object(_) => {
            let o = payload_keys(
                &obj["source_ref"],
                &["provider", "kind", "id"],
                &["provider", "kind", "id"],
            )?;
            let provider = o["provider"]
                .as_str()
                .ok_or_else(|| bad_payload("ref.provider must be a string"))?;
            let kind = o["kind"]
                .as_str()
                .ok_or_else(|| bad_payload("ref.kind must be a string"))?;
            let id = o["id"]
                .as_str()
                .ok_or_else(|| bad_payload("ref.id must be a string"))?;
            if provider != "youtube-music" || kind != "track" {
                return Err(failed(
                    "not-applicable",
                    "ref is not a youtube-music track ref".into(),
                ));
            }
            id.to_string()
        }
        _ => return Err(bad_payload("source_ref must be a string or a sourceRef")),
    };
    if !is_video_id(&video_id) {
        return Err(bad_payload(
            "source_ref id must be an 11-character video id",
        ));
    }
    let target_bitrate_kbps = match obj.get("target_bitrate_kbps") {
        None => 128,
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=512).contains(n))
            .ok_or_else(|| bad_payload("target_bitrate_kbps must be an integer 1..512"))?,
    };
    let prefer = match obj.get("prefer") {
        None => vec!["audio/mp4".to_string(), "audio/webm".to_string()],
        Some(Value::Array(list)) => {
            if list.len() > 2 {
                return Err(bad_payload("prefer accepts at most 2 entries"));
            }
            let mut seen = Vec::with_capacity(list.len());
            for v in list {
                let Some(s) = v.as_str() else {
                    return Err(bad_payload("prefer entries must be strings"));
                };
                if s != "audio/mp4" && s != "audio/webm" {
                    return Err(bad_payload(
                        "prefer entries must be audio/mp4 or audio/webm",
                    ));
                }
                if seen.iter().any(|p| p == s) {
                    return Err(bad_payload("prefer entries must be unique"));
                }
                seen.push(s.to_string());
            }
            seen
        }
        Some(_) => return Err(bad_payload("prefer must be an array")),
    };
    let pin_itag = match obj.get("pin_itag") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| bad_payload("pin_itag must be a u32 or null"))?,
        ),
    };
    let resume_offset = match obj.get("resume_offset") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| bad_payload("resume_offset must be a u64 or null"))?,
        ),
    };
    // The app-held OAuth access token. Bounded like a header value;
    // empty is treated as absent so a cleared app token degrades to
    // the anonymous ladder rather than sending `Bearer `.
    let access_token = match obj.get("access_token") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 8192 => Some(s.clone()),
        Some(_) => return Err(bad_payload("access_token must be a string 1..8192 chars")),
    };
    Ok(ResolvePayload {
        video_id,
        target_bitrate_kbps,
        prefer,
        pin_itag,
        resume_offset,
        access_token,
    })
}

/// The host-error kinds that must abort the work in flight:
/// `cancelled` is the abort signal, `permission-denied` and
/// `invalid-response` are contract violations. Anything else is
/// weather — retryable by nature.
fn is_terminal_kind(kind: &str) -> bool {
    matches!(kind, "cancelled" | "permission-denied" | "invalid-response")
}

/// Propagate terminal host errors; fold retryable ones into a rung
/// outcome. `rate-limit` keeps its taxonomy (its own backoff
/// reason and fail kind); anything else is weather.
fn terminal_or_transport(e: GuestError) -> Result<RungOutcome, GuestError> {
    match e {
        GuestError::Host { kind, message } => match kind.as_str() {
            k if is_terminal_kind(k) => Err(GuestError::Host { kind, message }),
            "rate-limit" => Ok(RungOutcome::RateLimited),
            _ => Ok(RungOutcome::Transport),
        },
        other => Err(other),
    }
}

/// A sanitized warning — never carries bodies, URLs, query text, or
/// video ids. Diagnostics are never a wedge: the terminal kinds still
/// propagate (`cancelled` is the abort signal; `permission-denied` and
/// `invalid-response` are contract failures a log call can equally
/// surface), but any other host error on the log channel is swallowed
/// — a diagnostics path that is down must not fail the resolve it was
/// annotating.
pub(crate) async fn warn(message: &str) -> Result<(), GuestError> {
    match log(LogLevel::Warn, message).await {
        Err(GuestError::Host { kind, message }) if is_terminal_kind(&kind) => {
            Err(GuestError::Host { kind, message })
        }
        Err(GuestError::Host { .. }) => Ok(()),
        other => other,
    }
}

/// KV is advisory state — visitors, backoffs, and the last-good hint
/// are optimizations the resolve re-derives when absent. A read that
/// fails with non-terminal weather degrades to the empty-store answer
/// with a warning rather than failing user work; the terminal kinds
/// still propagate.
async fn kv_get_soft(key: &str) -> Result<Option<Vec<u8>>, GuestError> {
    match kv_get(key).await {
        Err(GuestError::Host { kind, .. }) if !is_terminal_kind(&kind) => {
            warn("ignoring a transient KV read failure").await?;
            Ok(None)
        }
        other => other,
    }
}

/// Same rule for staged writes: a dropped write is state the next
/// resolve re-derives, so transient store weather is warned-and-skipped
/// — it must never sink a finished result.
pub(crate) async fn kv_set_soft(key: &str, value: Option<&[u8]>) -> Result<(), GuestError> {
    match kv_set(key, value).await {
        Err(GuestError::Host { kind, .. }) if !is_terminal_kind(&kind) => {
            warn("a transient KV write was dropped").await
        }
        other => other,
    }
}

/// Load a persisted visitor: nonempty visible-ASCII values only;
/// anything else is ignored with a sanitized warning and never reaches
/// a header.
pub(crate) async fn load_visitor(key: &str) -> Result<Option<String>, GuestError> {
    match kv_get_soft(key).await? {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(s) if visitor_token(&s).is_some() => Ok(Some(s)),
            _ => {
                warn("ignoring malformed visitor KV value").await?;
                Ok(None)
            }
        },
        None => Ok(None),
    }
}

/// Stored backoff reasons — the only values a `{until_ms,reason}`
/// record may carry.
const BACKOFF_REASONS: &[&str] = &["bot-check", "rate-limit", "transport", "capped"];

/// What a `backoff/<edge>/<video>/<rung>` KV read found. A record
/// that exists but is not in force — expired, or bytes that fail the
/// record shape — is inert state a finishing rung still collects, so
/// it marks the key dirty where a truly absent key does not.
enum StoredBackoff {
    /// Nothing stored under the key.
    Absent,
    /// A record exists but is expired or malformed (the latter warned
    /// on read): garbage the next `Done` deletes.
    Stale,
    /// An in-force skip — the stored reason for the taxonomy.
    Active(String),
}

/// Load a stored backoff: the bytes must be exactly
/// `{until_ms: u64, reason: <known reason>}` and still ahead of
/// `now` — anything else reads `Stale` (malformed values warn; expired
/// ones do not), and an absent key reads `Absent`.
async fn load_backoff(key: &str, now: u64) -> Result<StoredBackoff, GuestError> {
    match kv_get_soft(key).await? {
        Some(bytes) => {
            let parsed = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
                let o = v.as_object()?;
                if o.len() != 2 {
                    return None;
                }
                let until = o.get("until_ms")?.as_u64()?;
                let reason = o
                    .get("reason")?
                    .as_str()
                    .filter(|r| BACKOFF_REASONS.contains(r))?;
                Some((until, reason.to_string()))
            });
            match parsed {
                Some((until, reason)) if until > now => Ok(StoredBackoff::Active(reason)),
                Some(_) => Ok(StoredBackoff::Stale),
                None => {
                    warn("ignoring malformed backoff KV value").await?;
                    Ok(StoredBackoff::Stale)
                }
            }
        }
        None => Ok(StoredBackoff::Absent),
    }
}

async fn stage_backoff(key: &str, until_ms: u64, reason: &str) -> Result<(), GuestError> {
    let v =
        serde_json::to_vec(&json!({ "until_ms": until_ms, "reason": reason })).unwrap_or_default();
    kv_set_soft(key, Some(&v)).await
}

/// Read `ladder/last-good`: the key of the rung that completed the
/// last resolve. The hint only permutes attempt order — a stale entry
/// costs nothing the static order wouldn't have paid (the hinted rung
/// is attempted exactly once either way) and the first success
/// rewrites it, so the order converges per-network. On a flagged
/// carrier IP where only one client serves bare, the second and later
/// resolves open with that client — one player POST instead of a walk
/// — while a clean network keeps the static VISIONOS-first order. A
/// value naming no current rung is a stale build's artifact or a
/// foreign write: ignored with a warning, never a wedge.
async fn last_good() -> Result<Option<String>, GuestError> {
    match kv_get_soft(LAST_GOOD_KEY).await? {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(key) if LADDER.iter().any(|r| r.kv_key() == key) => Ok(Some(key)),
            _ => {
                warn("ignoring a stale last-good rung KV value").await?;
                Ok(None)
            }
        },
        None => Ok(None),
    }
}

/// The edge the previous successful resolve finished on — "a" the
/// primary InnerTube host, "b" the redraw host. Anything else is a
/// foreign write: ignored, never a wedge.
async fn last_edge() -> Result<Option<String>, GuestError> {
    match kv_get_soft(LAST_EDGE_KEY).await? {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(tag) if tag == "a" || tag == "b" => Ok(Some(tag)),
            _ => {
                warn("ignoring a stale last-edge KV value").await?;
                Ok(None)
            }
        },
        None => Ok(None),
    }
}

/// Map a stored backoff reason back onto the rung-outcome taxonomy —
/// a skipped rung still participates in the final failure kind.
fn outcome_for_reason(reason: &str) -> RungOutcome {
    match reason {
        "rate-limit" => RungOutcome::RateLimited,
        "bot-check" => RungOutcome::Bot,
        "capped" => RungOutcome::Capped,
        _ => RungOutcome::Transport,
    }
}

/// Unicode escapes hide marker whitespace: a reason whose spaces
/// arrive as `\u0020` never matches the byte-level phrase scan until
/// the escapes decode. Only the `\uXXXX` form matters here; `\"` and
/// friends stay escaped so they can't fake structure.
fn unescape_json_unicode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'u') {
            chars.next();
            let mut code = 0u32;
            let mut ok = true;
            for _ in 0..4 {
                match chars.next().and_then(|h| h.to_digit(16)) {
                    Some(d) => code = code * 16 + d,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            match ok.then(|| char::from_u32(code)).flatten() {
                // Structural JSON stays escaped: a `\u0022` inside a
                // string VALUE must never forge a `"status":"ok"`
                // the closed-span veto then reads as real structure.
                // Whitespace and letters still decode — that's what
                // the phrase scan needs.
                Some(decoded)
                    if !matches!(decoded, '"' | '\\' | ':' | '{' | '}' | '[' | ']' | ',') =>
                {
                    out.push(decoded)
                }
                Some(decoded) => {
                    out.push('\\');
                    out.push('u');
                    out.extend(format!("{:04x}", decoded as u32).chars());
                }
                None => {
                    out.push('\\');
                    out.push('u');
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Bot-check recovery for a JSON-shaped 403 body that failed to parse:
/// the wall truncated mid-envelope still carries a recognizable partial
/// `playabilityStatus` — a non-OK status plus the same reason markers
/// `classify_playability` keys on. Anything else (an `error` envelope,
/// an ambiguous prefix) is an API refusal, not the wall.
pub(crate) fn truncated_bot_check(body: &[u8]) -> bool {
    let blob = String::from_utf8_lossy(body).to_lowercase();
    // Anchor on the quoted, colon-bound KEY — a bare `playabilitystatus`
    // substring inside a string value or a longer key (`xPlayabilityStatus`)
    // would misplace the whole span scan.
    let mut search = 0usize;
    let open = loop {
        let Some(found) = blob[search..].find("\"playabilitystatus\"") else {
            return false;
        };
        let after = search + found + "\"playabilitystatus\"".len();
        let mut i = after;
        while blob
            .as_bytes()
            .get(i)
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            i += 1;
        }
        if blob.as_bytes().get(i) != Some(&b':') {
            search = after;
            continue;
        }
        match blob.as_bytes()[i + 1..].iter().position(|b| *b == b'{') {
            Some(o) => break i + 1 + o,
            None => return false,
        }
    };
    let rest = &blob.as_bytes()[open..];
    // The `playabilityStatus` object's own span — braces inside string
    // values don't count, and a truncated object runs to the end. A
    // `"status":"ok"` veto is only trustworthy on a CLOSED span: in an
    // unclosed tail it can belong to a later sibling field, and
    // vetoing a genuine wall marker books the wall as Transport.
    let mut depth = 0i32;
    let mut end = rest.len();
    let mut closed = false;
    let mut in_str = false;
    let mut escaped = false;
    for (i, b) in rest.iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                in_str = false;
            }
            continue;
        }
        match *b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    closed = true;
                    break;
                }
            }
            _ => {}
        }
    }
    let span = unescape_json_unicode(&String::from_utf8_lossy(&rest[..end])).to_lowercase();
    // Whitespace INSIDE a phrase is flexible (JSON pretty-printing
    // varies) but the phrase's word boundaries are not — `"notabot"`
    // is not `"not a bot"`. The compact form only checks structural
    // JSON (`"status":"ok"`), where no word boundary can be lost.
    let compact: String = span.chars().filter(|c| !c.is_whitespace()).collect();
    if closed && compact.contains("\"status\":\"ok\"") {
        return false;
    }
    let collapsed = span.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.contains("not a bot") || collapsed.contains("unusual traffic")
}

/// Does the body (sans a UTF-8 BOM — some stacks' emitters prepend
/// one, and it must not make a valid envelope look non-JSON) begin
/// like a JSON value?
pub(crate) fn looks_json(body: &[u8]) -> bool {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    body.iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'{' || *b == b'[')
}

/// The outcome a non-2xx player response books for its rung. A
/// flagged IP's wall arrives as a bare 403 — Google's abuse edge
/// answers with an HTML "automated queries" interstitial, never a JSON
/// body. That is the bot wall in transport form: the attested replay
/// is its remedy, so it books exactly like a body-classified BotCheck
/// (position, backoff, replay). A 403 that does carry a JSON envelope
/// is classified by its playabilityStatus — a bot-check inside is
/// still a wall; anything else is an API-level refusal for that
/// client, not the edge. A body shaped like JSON that fails to parse
/// (a truncated refusal) books Transport — unless its surviving prefix
/// still carries a bot-check marker, which is the wall truncated, not
/// a refusal. Only a non-JSON body books Bot outright.
fn refusal_outcome(resp: &HttpResponse) -> RungOutcome {
    if resp.status == 403 {
        let body = resp
            .body
            .strip_prefix(b"\xEF\xBB\xBF")
            .unwrap_or(&resp.body);
        let json_shaped = looks_json(body);
        match (
            json_shaped,
            serde_json::from_slice::<Value>(body)
                .ok()
                .map(|b| classify_playability(&b).0),
        ) {
            (false, _) | (_, Some(Playability::BotCheck)) => RungOutcome::Bot,
            (_, Some(Playability::SignInRequired)) => RungOutcome::SignIn,
            (_, Some(Playability::AgeRestricted)) => RungOutcome::Age,
            (_, Some(Playability::Unavailable)) => RungOutcome::Unavailable,
            (true, None) => {
                if truncated_bot_check(body) {
                    RungOutcome::Bot
                } else {
                    RungOutcome::Transport
                }
            }
            (_, Some(Playability::Ok)) => RungOutcome::Transport,
        }
    } else if resp.status == 429 {
        RungOutcome::RateLimited
    } else {
        RungOutcome::Transport
    }
}

/// The outcome a 2xx response whose body is not a readable player
/// envelope books for its rung. Markup or text is the abuse edge's
/// interstitial or a consent page answering OK — the wall in transport
/// form, symmetric with the bare 403, so it books `Bot` and the
/// attested replay is its remedy. A JSON-shaped prefix that fails to
/// parse is a truncated envelope — the surviving wall markers decide
/// `Bot` vs `Transport`, exactly like a truncated 403. An empty or
/// whitespace-only body is no envelope and no wall evidence either —
/// plain transport weather.
fn unreadable_body_outcome(body: &[u8]) -> RungOutcome {
    if looks_json(body) {
        if truncated_bot_check(body) {
            RungOutcome::Bot
        } else {
            RungOutcome::Transport
        }
    } else if body.iter().all(|b| b.is_ascii_whitespace()) {
        RungOutcome::Transport
    } else {
        RungOutcome::Bot
    }
}

/// A player response classified once — the visitor-drop wall check
/// and the rung's booked outcome both read this verdict, so a 2xx
/// pays one `serde_json::Value` DOM parse and a refusal one
/// `refusal_outcome` per response, never two inside the shared
/// per-entry fuel budget.
enum PlayerVerdict {
    /// A 2xx carrying a readable player envelope.
    Envelope(Value),
    /// The outcome the response books — a non-2xx refusal
    /// (`refusal_outcome`) or a 2xx that is not a readable envelope
    /// (`unreadable_body_outcome` over the BOM-stripped body, so a
    /// BOM-only or whitespace body reads Transport exactly like the
    /// wall check sees it).
    Outcome(RungOutcome),
}

/// Classify a player response once for both the wall check and the
/// post-loop booking: a 2xx is BOM-stripped and parsed — `Envelope`
/// on success, its unreadable-body `Outcome` on failure — and a
/// non-2xx is its `refusal_outcome`.
fn classify_response(resp: &HttpResponse) -> PlayerVerdict {
    if (200..300).contains(&resp.status) {
        let stripped = resp
            .body
            .strip_prefix(b"\xEF\xBB\xBF")
            .unwrap_or(&resp.body);
        match serde_json::from_slice::<Value>(stripped) {
            Ok(b) => PlayerVerdict::Envelope(b),
            Err(_) => PlayerVerdict::Outcome(unreadable_body_outcome(stripped)),
        }
    } else {
        PlayerVerdict::Outcome(refusal_outcome(resp))
    }
}

/// Is this response the bot wall — in any of its three shapes: a
/// non-2xx refusal (`refusal_outcome`), a 2xx JSON envelope whose
/// `playabilityStatus` is a bot-check, or a 2xx body that is not a
/// readable envelope at all (`unreadable_body_outcome`)? Used to
/// decide whether a replayed visitor is worth dropping for a bare
/// re-ask — replayed state is suspect under every wall shape.
fn walled_by_bot(verdict: &PlayerVerdict) -> bool {
    match verdict {
        PlayerVerdict::Envelope(b) => classify_playability(b).0 == Playability::BotCheck,
        PlayerVerdict::Outcome(o) => *o == RungOutcome::Bot,
    }
}

/// The backoff reason + window staged for a rung outcome; `None` for
/// deterministic content answers that are not weather.
fn backoff_for(outcome: RungOutcome) -> Option<(&'static str, u64)> {
    match outcome {
        RungOutcome::Bot => Some(("bot-check", BOT_BACKOFF_MS)),
        RungOutcome::RateLimited => Some(("rate-limit", RATE_LIMIT_BACKOFF_MS)),
        RungOutcome::Capped => Some(("capped", CAPPED_BACKOFF_MS)),
        RungOutcome::Transport => Some(("transport", TRANSPORT_BACKOFF_MS)),
        _ => None,
    }
}

/// Is this rung's verdict worth a second draw on the alternate edge?
/// Every redrawn outcome is per-request stochastic — the wall, the 429
/// window, the mint cap, plain transport weather — where a different
/// serving edge is an independent draw. Deterministic verdicts
/// (sign-in, age, unavailable, restricted formats, a proven
/// no-audio answer) are edge-agnostic: redrawing them would be a
/// provably wasted request.
fn redraw_worthy(outcome: Option<RungOutcome>) -> bool {
    matches!(
        outcome,
        Some(RungOutcome::Bot)
            | Some(RungOutcome::RateLimited)
            | Some(RungOutcome::Capped)
            | Some(RungOutcome::Transport)
    )
}

/// The end of a pick: `Done` resolves, `Advance` records the outcome
/// and the rung continues.
enum PickOutcome {
    Done(Value),
    Advance(RungOutcome),
}

async fn resolve(payload: &Value) -> Result<Value, GuestError> {
    let p = parse_resolve_payload(payload)?;
    // Validated for the Slice 1.5 seam re-mint calls; byte pumping
    // itself is Slice 1.5-owned.
    let _ = p.resume_offset;
    // The wall clock is the one call a resolve cannot degrade —
    // backoff arithmetic is meaningless without it, so its failure is
    // the resolve's failure.
    let now = now_ms().await?;
    // One slot per rung, indexed by static LADDER position: `order`
    // permutes request scheduling only — the failure summary still
    // reads the ladder in static order, so a last-good hint can never
    // change which failure `ladder_error` selects. The attested pass
    // overwrites a replayed rung's slot with its real verdict, so a
    // superseded bot-check can't skew the summary either.
    let mut outcomes: Vec<Option<RungOutcome>> = vec![None; LADDER.len()];
    // Ladder indices that produced `Bot` — the attested pass replays
    // only the attestable ones. Includes backoff-derived entries: a
    // staged bot-backoff is exactly what attestation is for.
    let mut bot_positions: Vec<usize> = Vec::new();
    // Visitor keys this invocation dropped as walled — their staged
    // deletes don't read back through `load_visitor`, so the redraw
    // pass consults this set instead of replaying a poisoned value.
    let mut dropped_keys: Vec<String> = Vec::new();
    let mut pin_seen = false;
    let mut pin_missing = false;
    let mut mint_attempted = false;
    let mut pot: Option<String> = None;
    // The freshest visitorData seen this invocation — replayed on later
    // rungs ahead of their persisted KV value, matching the Slice 0
    // cross-rung propagation — plus the KV key it was harvested under,
    // so dropping a burned fresh value can erase its staged write too.
    let mut fresh_visitor: Option<String> = None;
    let mut fresh_visitor_key: Option<String> = None;
    // The session-trust token rides every rung until a 401 proves it
    // dead — then the remaining ladder runs bare rather than failing
    // authenticated requests repeatedly.
    let mut access_token = p.access_token.clone();

    // Up to three passes over the ladder. Pass 0 runs bare — free on
    // a clean IP. Pass 1 replays only the bot-checked rungs with the
    // shared video-bound poToken in `serviceIntegrityDimensions`, the
    // wall's documented remedy; it fires only when a provider minted.
    // Pass 2 is the no-attestation remedy: every rung whose verdict is
    // still per-request weather (bot wall, 429, capped mint, transport)
    // is redrawn bare on `REDRAW_PLAYER_URL` — a different serving edge
    // with independent wall decisions, carrying the freshest visitor
    // the walls themselves minted.
    //
    // Attempt order is the `ladder/last-good` hint — the rung that
    // finished the previous resolve — followed by the rest of the
    // static ladder: a pure permutation, so a stale hint never costs
    // more than the static order (every rung still gets its bare
    // shot), and a failed resolve never writes it. This is what lets a
    // flagged carrier IP converge on the one client that serves it
    // bare instead of re-walking dead rungs on every resolve.
    let last_good = last_good().await?;
    // Which edge finished the previous resolve — a "b" hint opens the
    // walk on the redraw host, where a sustained wall already proved
    // edge A dead. The hint is advisory only: whatever it picks, the
    // first success rewrites it, so it can never wedge.
    let last_edge = last_edge().await?;
    let primary_b = last_edge.as_deref() == Some("b");
    let hint = last_good
        .as_deref()
        .and_then(|key| LADDER.iter().position(|r| r.kv_key() == key));
    let mut order: Vec<usize> = Vec::with_capacity(LADDER.len());
    if let Some(i) = hint {
        order.push(i);
    }
    order.extend((0..LADDER.len()).filter(|i| hint != Some(*i)));
    // Rungs whose `backoff/<edge>` key the resolve touched — an
    // in-force record it skipped past or one it staged. A finishing
    // rung clears its key only in those cases; a clean run needs no
    // write.
    let mut dirty_backoffs = vec![false; LADDER.len()];
    // Player + probe emissions this resolve, counted against the
    // host's HTTP budget so a remedy pass never opens a rung chain
    // that can't finish inside it.
    let mut http_calls = 0u32;

    let mut pass = 0u8;
    'passes: loop {
        let attested = pass == 1;
        let redraw = pass == 2;
        // The edge this pass's requests ride: passes 0/1 stay on one
        // host (attestation replays the rung on the edge that walled
        // it) and the redraw rides the other. "b" primary means the
        // previous resolve's redraw winner opens first this time.
        let edge = if redraw != primary_b { "b" } else { "a" };
        'rung: for &i in &order {
            let rung = &LADDER[i];
            // Pass 1 replays only rungs attestation can lift — a
            // non-attestable client's bot-check is permanent (its wall
            // needs DroidGuard, which a BotGuard mint never produces),
            // so replaying it would be a provably wasted request.
            if attested && (!rung.attestable || !bot_positions.contains(&i)) {
                continue;
            }
            // Pass 2 redraws whatever still reads as weather — walls
            // attestation never replayed or failed to lift, 429s,
            // capped mints, transport misses. Deterministic verdicts
            // are edge-agnostic and never redrawn.
            if redraw && !redraw_worthy(outcomes[i]) {
                continue;
            }
            // A remedy request needs its whole chain inside the host's
            // 32-call HTTP budget — pass 0's worst case (9 player +
            // 9 probes + re-probes) can leave so little that a redrawn
            // rung would never reach its probe. Emit only what fits;
            // the skipped rungs keep their pass-0 verdict.
            if (attested || redraw) && http_calls + RUNG_CHAIN_CALLS > HTTP_CALL_BUDGET {
                continue;
            }
            // Backoffs are edge-scoped: a redraw cooldown must never
            // shadow the primary edge's record — the other edge can
            // recover long before it expires.
            let backoff_key = format!("backoff/{}/{}/{}", edge, p.video_id, rung.kv_key());
            // Backoff before the visitor read: a skipped rung costs one
            // KV read, not two. Each edge honors only its own records —
            // the redraw's 429 must not extend the primary edge's skip,
            // and the attested pass deliberately ignores them all: a
            // staged bot-backoff is the thing attestation exists to
            // break.
            if !attested {
                match load_backoff(&backoff_key, now).await? {
                    StoredBackoff::Active(reason) => {
                        dirty_backoffs[i] = true;
                        let outcome = outcome_for_reason(&reason);
                        if pass == 0 && outcome == RungOutcome::Bot {
                            bot_positions.push(i);
                        }
                        outcomes[i] = Some(outcome);
                        continue;
                    }
                    // An inert record (malformed or expired) still
                    // marks the key dirty — it is garbage a `Done`
                    // collects, where `Absent` needs no write.
                    StoredBackoff::Stale => dirty_backoffs[i] = true,
                    StoredBackoff::Absent => {}
                }
            }
            let visitor_key = format!("visitor/{}", rung.kv_key());
            // A fresh cross-rung visitor shadows the stored value —
            // skip the read entirely when one exists. A key this
            // invocation already dropped as walled is poisoned: its
            // staged delete can't be read back yet, so skip the read
            // rather than replay the burned value on the new edge.
            let kv_visitor = if fresh_visitor.is_some() || dropped_keys.contains(&visitor_key) {
                None
            } else {
                load_visitor(&visitor_key).await?
            };
            let mut rung_visitor = fresh_visitor.clone().or(kv_visitor);

            // The rung's sends live in one loop because two one-shot
            // re-asks can stack: a 401 against a carried trust token
            // drops it and re-asks bare (`take` makes it single-shot —
            // a bare 401 falls through as the rung's own refusal), and
            // a bot wall on a request that replayed a visitor re-asks
            // once without it — replayed state is itself suspect on a
            // wall, and nothing else can heal a poisoned persisted
            // visitor: a failed resolve rolls its staged writes back,
            // so the value would wall this rung on every later resolve
            // too, and the attested pass replays the same one.
            let mut dropped_visitor = false;
            let verdict = 'request: loop {
                http_calls += 1;
                let sent = http_request(player_request(
                    rung,
                    &p.video_id,
                    rung_visitor.as_deref(),
                    if attested { pot.as_deref() } else { None },
                    access_token.as_deref(),
                    if edge == "b" {
                        crate::rungs::REDRAW_PLAYER_URL
                    } else {
                        crate::rungs::PLAYER_URL
                    },
                ))
                .await;
                let r = match sent {
                    Ok(r) => r,
                    Err(e) => {
                        let outcome = terminal_or_transport(e)?;
                        if let Some((reason, ms)) = backoff_for(outcome) {
                            dirty_backoffs[i] = true;
                            stage_backoff(&backoff_key, now.saturating_add(ms), reason).await?;
                        }
                        outcomes[i] = Some(outcome);
                        continue 'rung;
                    }
                };
                // Re-asks consume from the same budget — one only runs
                // while the probe chain behind it still fits, else the
                // rung keeps this verdict rather than dying mid-draw.
                if r.status == 401
                    && access_token.is_some()
                    && http_calls + RUNG_CHAIN_CALLS <= HTTP_CALL_BUDGET
                {
                    access_token.take();
                    continue 'request;
                }
                let verdict = classify_response(&r);
                if rung_visitor.is_some() && !dropped_visitor && walled_by_bot(&verdict) {
                    dropped_visitor = true;
                    // Drop the suspect state everywhere it can persist —
                    // all deletions are staged and commit on `done`, so
                    // the poisoned value is gone for later resolves
                    // instead of replayed and dropped again every time:
                    // * this rung's KV slot, whichever value was sent —
                    //   when a fresh visitor shadowed it the stored value
                    //   went untested, but a rung that walls under any
                    //   replayed state can't trust what it would replay
                    //   next;
                    // * the fresh cross-rung value's home slot when it
                    //   was the one sent (fresh shadows KV, so a set
                    //   `fresh` is always the replayed one) — otherwise a
                    //   committed resolve would re-persist the burned
                    //   token under its source rung's key and the next
                    //   invocation would replay it there.
                    kv_set_soft(&visitor_key, None).await?;
                    dropped_keys.push(visitor_key.clone());
                    if fresh_visitor.take().is_some() {
                        if let Some(home) = fresh_visitor_key.take() {
                            if home != visitor_key {
                                kv_set_soft(&home, None).await?;
                                dropped_keys.push(home);
                            }
                        }
                    }
                    rung_visitor = None;
                    // Same budget bound as the 401 re-ask: the bare
                    // re-ask runs only while the probe chain behind it
                    // still fits — the drop bookkeeping commits either
                    // way.
                    if http_calls + RUNG_CHAIN_CALLS <= HTTP_CALL_BUDGET {
                        continue 'request;
                    }
                }
                break 'request verdict;
            };
            // A non-envelope verdict is this rung's own answer — the
            // wall in transport form (a bare refusal, a markup
            // interstitial or consent page answering OK, a truncated
            // envelope) or plain transport weather — never a reason to
            // starve the rungs behind it. A 401 that carried the token
            // was already re-asked bare above, so every outcome
            // reaching here is the rung's own and stages its backoff:
            // Bot also books the attested-replay slot.
            let body: Value = match verdict {
                PlayerVerdict::Outcome(outcome) => {
                    if let Some((reason, ms)) = backoff_for(outcome) {
                        dirty_backoffs[i] = true;
                        stage_backoff(&backoff_key, now.saturating_add(ms), reason).await?;
                    }
                    outcomes[i] = Some(outcome);
                    if pass == 0 && outcome == RungOutcome::Bot {
                        bot_positions.push(i);
                    }
                    continue;
                }
                PlayerVerdict::Envelope(b) => b,
            };
            // A parseable body is classified by whatever it carries —
            // an absent `playabilityStatus` is the parser's documented
            // "legacy playable" shape and `format_outcome` reports a
            // format-less answer as no-audio honestly. No JSON shape
            // the envelope could take is worth aborting the ladder over.
            if let Some(raw) = visitor_data(&body) {
                if let Some(visitor) = visitor_token(&raw) {
                    let visitor = visitor.to_string();
                    kv_set_soft(&visitor_key, Some(visitor.as_bytes())).await?;
                    rung_visitor = Some(visitor.clone());
                    fresh_visitor = Some(visitor);
                    fresh_visitor_key = Some(visitor_key.clone());
                } else {
                    // A malformed visitorData is dropped before it can reach
                    // a header, a KV value, or the PO-token binding.
                    warn("ignoring malformed visitor value").await?;
                }
            }
            // A player response naming a different video is not this
            // resolve's resource — the rung answered, just not what was
            // asked. Its stream URL must never reach the picker; the rung
            // counts as unavailable.
            if body
                .pointer("/videoDetails/videoId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != p.video_id)
            {
                outcomes[i] = Some(RungOutcome::Unavailable);
                continue;
            }
            match classify_playability(&body).0 {
                Playability::Ok => {}
                Playability::BotCheck => {
                    dirty_backoffs[i] = true;
                    stage_backoff(
                        &backoff_key,
                        now.saturating_add(BOT_BACKOFF_MS),
                        "bot-check",
                    )
                    .await?;
                    outcomes[i] = Some(RungOutcome::Bot);
                    if pass == 0 {
                        bot_positions.push(i);
                    }
                    continue;
                }
                Playability::AgeRestricted => {
                    outcomes[i] = Some(RungOutcome::Age);
                    continue;
                }
                Playability::SignInRequired => {
                    outcomes[i] = Some(RungOutcome::SignIn);
                    continue;
                }
                Playability::Unavailable => {
                    outcomes[i] = Some(RungOutcome::Unavailable);
                    continue;
                }
            }
            match format_outcome(&body) {
                FormatOutcome::PlainAudio => {
                    let prefer: Vec<&str> = p.prefer.iter().map(String::as_str).collect();
                    match pick_audio(
                        &body,
                        PickOptions {
                            target_bitrate_kbps: p.target_bitrate_kbps,
                            prefer: &prefer,
                            pin_itag: p.pin_itag,
                            // Rank only what the sandbox can serve —
                            // a foreign-host top pick must not starve
                            // the googlevideo formats behind it.
                            url_ok: Some(is_googlevideo),
                        },
                    ) {
                        Some(picked) => {
                            // The pinned itag exists on this rung — serving
                            // trouble from here on is capped/transport
                            // weather, not a missing resource.
                            if p.pin_itag.is_some() {
                                pin_seen = true;
                            }
                            match finish_pick(
                                rung,
                                picked,
                                &mut mint_attempted,
                                &mut pot,
                                &p.video_id,
                                &mut http_calls,
                            )
                            .await?
                            {
                                PickOutcome::Done(result) => {
                                    // Clear the rung's backoff key only
                                    // when the resolve touched it — an
                                    // in-force record skipped past or
                                    // one staged this invocation; a
                                    // clean key needs no write.
                                    if dirty_backoffs[i] {
                                        kv_set_soft(&backoff_key, None).await?;
                                    }
                                    // The winning rung leads the next
                                    // resolve — rewrite the hint only
                                    // when it changed.
                                    if last_good.as_deref() != Some(rung.kv_key()) {
                                        kv_set_soft(LAST_GOOD_KEY, Some(rung.kv_key().as_bytes()))
                                            .await?;
                                    }
                                    // Same for the serving edge: a
                                    // redraw winner opens the next
                                    // resolve's primary pass so a
                                    // sustained wall doesn't cost the
                                    // dead edge's walk every time.
                                    if last_edge.as_deref() != Some(edge) {
                                        kv_set_soft(LAST_EDGE_KEY, Some(edge.as_bytes())).await?;
                                    }
                                    return Ok(result);
                                }
                                PickOutcome::Advance(outcome) => {
                                    if let Some((reason, ms)) = backoff_for(outcome) {
                                        dirty_backoffs[i] = true;
                                        stage_backoff(&backoff_key, now.saturating_add(ms), reason)
                                            .await?;
                                    }
                                    outcomes[i] = Some(outcome);
                                }
                            }
                        }
                        None => {
                            // Nothing servable ranked — was the
                            // response bare of plain audio, or was its
                            // audio all on hosts the sandbox can never
                            // serve? Re-pick without the host bar to
                            // tell them apart: all-foreign books the
                            // capped mint it is, not a no-audio rung.
                            if pick_audio(
                                &body,
                                PickOptions {
                                    target_bitrate_kbps: p.target_bitrate_kbps,
                                    prefer: &prefer,
                                    pin_itag: p.pin_itag,
                                    url_ok: None,
                                },
                            )
                            .is_some()
                            {
                                // The pinned itag exists but its mint
                                // is unreachable — seen, not missing.
                                if p.pin_itag.is_some() {
                                    pin_seen = true;
                                }
                                outcomes[i] = Some(RungOutcome::Capped);
                            } else {
                                // A failed player request cannot establish that
                                // the pin disappeared. Require a usable format
                                // from this response when the pin is removed.
                                if p.pin_itag.is_some()
                                    && pick_audio(
                                        &body,
                                        PickOptions {
                                            target_bitrate_kbps: p.target_bitrate_kbps,
                                            prefer: &prefer,
                                            pin_itag: None,
                                            url_ok: None,
                                        },
                                    )
                                    .is_some()
                                {
                                    pin_missing = true;
                                }
                                outcomes[i] = Some(RungOutcome::NoAudio);
                            }
                        }
                    }
                }
                FormatOutcome::SabrOnly => outcomes[i] = Some(RungOutcome::SabrOnly),
                FormatOutcome::CipheredOnly => outcomes[i] = Some(RungOutcome::CipheredOnly),
                FormatOutcome::NoAudio => outcomes[i] = Some(RungOutcome::NoAudio),
            }
        }
        // Escalation: bot-checks accumulate the attested pass first —
        // one shared mint, the same video-bound token also decorates
        // picked URLs via `finish_pick`. When attestation cannot run
        // (no attestable wall, denied/failed mint) or ran and walls
        // still stand, the redraw pass gives every weather outcome a
        // second draw on the alternate edge before the resolve turns
        // terminal.
        pass = match pass {
            0 => {
                if bot_positions.iter().any(|&r| LADDER[r].attestable)
                    && mint_once(&mut mint_attempted, &mut pot, &p.video_id).await?
                {
                    1
                } else if outcomes.iter().any(|o| redraw_worthy(*o)) {
                    2
                } else {
                    break 'passes;
                }
            }
            1 if outcomes.iter().any(|o| redraw_worthy(*o)) => 2,
            _ => break 'passes,
        };
    }
    // The taxonomy reads the ladder in static rung order — `order`
    // scheduled requests only.
    let seen: Vec<RungOutcome> = outcomes.iter().flatten().copied().collect();
    Err(ladder_error(&seen, pin_missing, pin_seen))
}

/// The shared PO token: one video-bound mint per resolve serves both
/// consumers — `serviceIntegrityDimensions` attestation on the
/// player-replay pass and `pot=` decoration on googlevideo URLs.
/// Video-bound matches the current upstream binding for both contexts
/// (live-verified: a video-bound `pot=` serves the full span). A
/// denied/failed mint degrades to `None`; `cancelled` propagates.
async fn mint_once(
    mint_attempted: &mut bool,
    pot: &mut Option<String>,
    video_id: &str,
) -> Result<bool, GuestError> {
    if *mint_attempted {
        return Ok(pot.is_some());
    }
    *mint_attempted = true;
    // A still-warm aside at either scope short-circuits the call —
    // stale or malformed records fall through and earn a fresh probe.
    if pot_aside_active(POT_ASIDE_KEY).await?
        || pot_aside_active(&format!("{POT_ASIDE_KEY}/{video_id}")).await?
    {
        return Ok(false);
    }
    let (outcome, miss) = match pot_token(video_id).await {
        Ok(r) if r.status == 200 => {
            *pot = serde_json::from_slice::<Value>(&r.body).ok().and_then(|j| {
                ["poToken", "po_token", "token"]
                    .iter()
                    .find_map(|key| j.get(key).and_then(Value::as_str))
                    .filter(|token| !token.is_empty())
                    .map(str::to_string)
            });
            // A 200 with no usable token is the provider's own
            // contract shape — it can't be video-specific.
            (pot.is_some(), PotMiss::Global)
        }
        // Server-side weather can't bind to a video; a client refusal
        // can — scope it so the next video's mint is unaffected.
        Ok(r) => (
            false,
            if r.status >= 500 {
                PotMiss::Global
            } else {
                PotMiss::Video
            },
        ),
        Err(GuestError::Host { kind, message }) => {
            if kind == "cancelled" {
                return Err(GuestError::Host { kind, message });
            }
            // denied / unsupported / transient mint failures degrade
            // to no token — the sidecar never saw a binding, so the
            // miss can only be provider-wide.
            (false, PotMiss::Global)
        }
        Err(e) => return Err(e),
    };
    if outcome {
        // A mint that landed clears any stale aside a failed resolve
        // wrote — at both scopes it can see.
        kv_set_soft(POT_ASIDE_KEY, None).await?;
        kv_set_soft(&format!("{POT_ASIDE_KEY}/{video_id}"), None).await?;
    } else {
        let key = match miss {
            PotMiss::Global => POT_ASIDE_KEY.to_string(),
            PotMiss::Video => format!("{POT_ASIDE_KEY}/{video_id}"),
        };
        let until = now_ms().await?.saturating_add(POT_ASIDE_MS);
        let record = serde_json::to_vec(&json!({ "until_ms": until })).unwrap_or_default();
        kv_set_soft(&key, Some(&record)).await?;
    }
    Ok(outcome)
}

/// Where a degraded mint books its aside.
enum PotMiss {
    /// Provider-side weather — suppresses mints for every video.
    Global,
    /// A refusal that can bind to the asked video — suppresses only
    /// that video's mints.
    Video,
}

/// Whether a `pot/aside` record at `key` is still in force. The record
/// must be exactly `{"until_ms": u64}` — extra fields, wrong types,
/// or malformed bytes are warned-and-ignored (the provider is probed
/// fresh) rather than letting foreign state suppress mints.
async fn pot_aside_active(key: &str) -> Result<bool, GuestError> {
    match kv_get_soft(key).await? {
        Some(bytes) => {
            let until = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
                let o = v.as_object()?;
                if o.len() != 1 {
                    return None;
                }
                o.get("until_ms")?.as_u64()
            });
            match until {
                Some(u) => Ok(u > now_ms().await?),
                None => {
                    warn("ignoring malformed pot-aside KV value").await?;
                    Ok(false)
                }
            }
        }
        None => Ok(false),
    }
}

/// A candidate URL is decorated with `pot=` before it is probed. The
/// mint is lazy — it fires once per resolve, shared with the
/// attestation pass. Then the strict tail probe runs over the final
/// decorated URL.
async fn finish_pick(
    rung: &crate::rungs::Rung,
    mut picked: crate::parse::Picked,
    mint_attempted: &mut bool,
    pot: &mut Option<String>,
    video_id: &str,
    http_calls: &mut u32,
) -> Result<PickOutcome, GuestError> {
    let _ = mint_once(mint_attempted, pot, video_id).await?;
    if let Some(token) = pot.as_deref() {
        picked.url = append_pot(&picked.url, token);
    }
    // A picked URL the manifest allowlist can never admit
    // (`*.googlevideo.com` only) is a dead mint, not weather: probing
    // it wastes a request that ends in permission-denied. Book it
    // capped like a refused mint and let the next rung try.
    if !is_googlevideo(&picked.url) {
        return Ok(PickOutcome::Advance(RungOutcome::Capped));
    }
    *http_calls += 1;
    let mut resp = match http_request(probe_request(rung, &picked.url, picked.content_length)).await
    {
        Ok(r) => r,
        // A host denial past the local allowlist check is no longer a
        // foreign destination — `permission-denied` is a contract
        // failure and stays terminal, never a capped mint.
        Err(e) => return Ok(PickOutcome::Advance(terminal_or_transport(e)?)),
    };
    // The host never follows redirects — destination policy is enforced
    // on each request — so a 3xx is re-requested through the normal
    // authorized path. googlevideo edge-balances minted URLs this way;
    // the verdict runs on wherever the chain lands. One hop, and only
    // to a host the allowlist admits — an edge balancing off
    // googlevideo is a mint we cannot serve, so a second redirect or
    // a foreign target is serving weather, not a chain to chase.
    if resp.status / 100 == 3 {
        let target = header_value(&resp.headers, "location").map(str::to_owned);
        if let Some(target) = target.filter(|t| t.starts_with("https://") && is_googlevideo(t)) {
            *http_calls += 1;
            match http_request(probe_request(rung, &target, picked.content_length)).await {
                Ok(r) => resp = r,
                Err(e) => return Ok(PickOutcome::Advance(terminal_or_transport(e)?)),
            }
        }
    }
    match probe_verdict(&resp, picked.content_length) {
        None => Ok(PickOutcome::Done(json!({
            "url": picked.url,
            "mime": picked.mime,
            "bitrate_kbps": picked.bitrate_kbps,
            "expires_at_ms": picked.expires_at_ms,
            "content_length": picked.content_length,
            "client": rung.name,
            "itag": picked.itag,
            // The mint's fetch identity: the URL was minted (and just
            // probed) as this client — the host must serve the stream
            // fetch with the same UA or the edge answers as bot
            // traffic.
            "headers": {"user-agent": rung.user_agent},
        }))),
        Some(outcome) => Ok(PickOutcome::Advance(outcome)),
    }
}

/// A parsed `Content-Range` value: `bytes <start>-<end>/<total>` on a
/// 206, `bytes */<total>` on a 416. `total` is `None` when the server
/// sends `*`.
enum ContentRange {
    Range {
        start: u64,
        end: u64,
        total: Option<u64>,
    },
    Unsatisfiable {
        total: Option<u64>,
    },
}

/// Case-insensitive header lookup over response header pairs.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|(k, v)| k.eq_ignore_ascii_case(name).then_some(v.as_str()))
}

fn parse_content_range(resp: &HttpResponse) -> Option<ContentRange> {
    let value = header_value(&resp.headers, "content-range")?
        .trim()
        .strip_prefix("bytes ")?;
    if let Some(total) = value.strip_prefix("*/") {
        return Some(ContentRange::Unsatisfiable {
            total: total.trim().parse().ok(),
        });
    }
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.trim().split_once('-')?;
    Some(ContentRange::Range {
        start: start.trim().parse().ok()?,
        end: end.trim().parse().ok()?,
        total: total.trim().parse().ok(),
    })
}

/// The byte offset the probe asked for: the tail of a known-length
/// file, or the fixed fallback window start.
fn probe_start(content_length: Option<u64>) -> u64 {
    content_length
        .map(|len| len.saturating_sub(PROBE_TAIL_BYTES))
        .unwrap_or(PROBE_FALLBACK_START)
}

/// The probe verdict: `None` means the mint demonstrably serves the
/// probed span — resolve `done`. On the tail probe (known
/// `content_length`) a 206 proves that only when its `Content-Range`
/// starts where the probe asked, reaches the file's last byte, and
/// the body carried the whole advertised span — an empty body or an
/// absent/foreign range verifies nothing. On the fixed fallback
/// window (unknown `content_length`) `end + 1 == total` is
/// unreachable for any file longer than the window, so there the bar
/// is a matching start plus either the whole asked span or a reported
/// EOF — the window only exists to prove the mint serves past the
/// ~1 MiB cap horizon. A 416 is a pass only for the fallback range
/// *and* only with a `bytes */N` showing the file ends before the
/// probe start — a cap can wear a bare 416. A 429 keeps its taxonomy:
/// rate-limiting is not evidence of a truncated mint. A 5xx is
/// `Transport`, not `Capped` — server weather teaches nothing about
/// serving. Anything else marks the mint capped and advances the
/// ladder.
fn probe_verdict(resp: &HttpResponse, content_length: Option<u64>) -> Option<RungOutcome> {
    let start_asked = probe_start(content_length);
    match resp.status {
        206 => {
            let Some(ContentRange::Range { start, end, total }) = parse_content_range(resp) else {
                return Some(RungOutcome::Capped);
            };
            let reached_eof = match total {
                Some(t) => end.checked_add(1) == Some(t),
                None => content_length.is_some_and(|l| end.checked_add(1) == Some(l)),
            };
            let served_span = match content_length {
                // The tail window ends at the file's last byte — only
                // reaching EOF proves the whole file serves.
                Some(_) => reached_eof,
                // The fallback window ends past the ~1 MiB horizon, so
                // the whole asked span — or an earlier reported EOF —
                // proves the mint serves beyond it.
                None => reached_eof || end == start_asked + PROBE_FALLBACK_BYTES - 1,
            };
            let span_carried = end
                .checked_sub(start)
                .is_some_and(|span| span + 1 == resp.body.len() as u64);
            if start == start_asked && served_span && span_carried {
                None
            } else {
                Some(RungOutcome::Capped)
            }
        }
        416 if content_length.is_none() => match parse_content_range(resp) {
            Some(ContentRange::Unsatisfiable { total: Some(total) }) if total <= start_asked => {
                None
            }
            _ => Some(RungOutcome::Capped),
        },
        429 => Some(RungOutcome::RateLimited),
        500..=599 => Some(RungOutcome::Transport),
        _ => Some(RungOutcome::Capped),
    }
}

/// Map the accumulated outcomes to the terminal taxonomy. Precedence:
/// rate-limit, a demonstrably missing pinned itag (absent from a usable
/// response and never seen — seen-but-capped is capped/transport weather), the
/// all-unsupported SABR/cipher analysis, all-capped, then the last
/// bucket (bot/auth/no-result/transport).
fn ladder_error(outcomes: &[RungOutcome], pin_missing: bool, pin_seen: bool) -> GuestError {
    if outcomes.contains(&RungOutcome::RateLimited) {
        return failed("rate-limit", "rate-limit".into());
    }
    if pin_missing && !pin_seen {
        return failed("expired-resource", "pinned-itag-unavailable".into());
    }
    // `unsupported` is earned only when EVERY recorded outcome is a
    // restricted-format verdict: a bot-checked or transport-failed
    // rung demonstrated nothing about plain audio, so a mixed ladder
    // falls through to the weather it actually saw.
    if !outcomes.is_empty()
        && outcomes
            .iter()
            .all(|o| matches!(o, RungOutcome::SabrOnly | RungOutcome::CipheredOnly))
    {
        return failed(
            "unsupported",
            if outcomes[0] == RungOutcome::SabrOnly {
                "sabr-only".into()
            } else {
                "ciphered-only".into()
            },
        );
    }
    // Every rung resolved but every mint refused the boundary probe:
    // provider serving is restricted right now — retryable weather.
    if !outcomes.is_empty() && outcomes.iter().all(|o| *o == RungOutcome::Capped) {
        return failed("transient", "streams-capped".into());
    }
    // Ranking fallback: the last outcome that is not a restricted-
    // format verdict decides — mixed into weather, a SABR/ciphered
    // rung proved nothing about the ladder as a whole, so the recorded
    // weather/auth/no-result outcome carries the failure instead.
    match outcomes
        .iter()
        .copied()
        .rfind(|o| !matches!(o, RungOutcome::SabrOnly | RungOutcome::CipheredOnly))
        .unwrap_or(RungOutcome::Transport)
    {
        // `provider-wall` (ABI taxonomy since host-side 0.3.x): the wall
        // is a verdict on this visitor/IP, not transport weather — the
        // dedicated kind keeps it terminal + row-preserving without any
        // message sniffing on the app side. But a wall is only certain
        // when every rung met a verdict: a rung that failed on retryable
        // weather (Transport — never resolved; Capped — the mint refused
        // the probe) can succeed on retry before the walled rung is even
        // reached, so a mixed ladder keeps the retryable kind and the
        // wall detail rides the message for classification.
        RungOutcome::Bot => {
            if outcomes
                .iter()
                .any(|o| matches!(o, RungOutcome::Transport | RungOutcome::Capped))
            {
                failed("transient", "bot-check".into())
            } else {
                failed("provider-wall", "bot-check".into())
            }
        }
        RungOutcome::SignIn | RungOutcome::Age => {
            failed("auth-required", "sign-in-required".into())
        }
        RungOutcome::Unavailable | RungOutcome::NoAudio => {
            failed("no-result", "unavailable".into())
        }
        RungOutcome::SabrOnly
        | RungOutcome::CipheredOnly
        | RungOutcome::RateLimited
        | RungOutcome::Transport => failed("transient", "transport".into()),
        RungOutcome::Capped => failed("transient", "streams-capped".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auqw_guest_sdk::{dispatch_step, reset_for_testing};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use std::collections::BTreeMap;

    const OK: &str = include_str!("../fixtures/player-ok-plain-urls.json");
    const SABR: &str = include_str!("../fixtures/player-sabr-only.json");
    const BOT: &str = include_str!("../fixtures/player-bot-check.json");
    const CIPHERED: &str = include_str!("../fixtures/player-ciphered-only.json");
    const UNPLAYABLE: &str = include_str!("../fixtures/player-unplayable.json");
    const VID: &str = "vid12345678";
    const NOW: u64 = 1_800_000_000_000;

    fn step(input: &Value) -> Value {
        let out = dispatch_step(&serde_json::to_vec(input).unwrap_or_default(), dispatch);
        serde_json::from_slice(&out).unwrap_or_else(|e| panic!("guest output is not JSON: {e}"))
    }

    /// How the harness answers a `pot_token` mint request.
    enum Pot {
        /// `host_error` permission-denied, like a host with no provider.
        Deny,
        /// A 200 carrying `{"poToken": ...}`.
        Token(&'static str),
        /// `host_error` cancelled — must propagate.
        Cancelled,
        /// A non-200 HTTP status with an empty body.
        Status(u16),
    }

    /// A fake host: answers now/kv/log/pot itself and stops at every
    /// `http_request` so the test can reply. `kv_set` lands in `staged`;
    /// `done` merges it into `committed`, `fail` discards it — the
    /// host's commit-on-done semantics.
    struct Harness {
        committed: BTreeMap<String, Vec<u8>>,
        staged: BTreeMap<String, Option<Vec<u8>>>,
        pot: Pot,
        pot_calls: u32,
        logs: Vec<String>,
        /// Value `now_ms` replies with; `NOW` unless a test overrides.
        now: u64,
        /// When set, every KV call answers `host_error` with this kind —
        /// the store is advisory, so non-terminal kinds must degrade
        /// rather than wedge, and terminal kinds must still abort.
        kv_error: Option<&'static str>,
        /// Same for the log channel: `warn` must swallow weather but
        /// never the terminal kinds.
        log_error: Option<&'static str>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                committed: BTreeMap::new(),
                staged: BTreeMap::new(),
                pot: Pot::Deny,
                pot_calls: 0,
                logs: Vec::new(),
                now: NOW,
                kv_error: None,
                log_error: None,
            }
        }

        fn invoke(&mut self, payload: Value) -> Value {
            // Tests share worker threads; clear any state a previous
            // test left parked before starting a fresh invocation.
            reset_for_testing();
            self.staged.clear();
            let out = step(&json!({
                "type": "invoke",
                "request_id": "t1",
                "capability": "playback.resolve",
                "payload": payload,
            }));
            self.drive(out)
        }

        /// Answer non-HTTP host calls until the guest emits an
        /// `http_request` or a terminal message.
        fn drive(&mut self, mut out: Value) -> Value {
            loop {
                match out["type"].as_str().unwrap_or("") {
                    "done" => {
                        for (k, v) in core::mem::take(&mut self.staged) {
                            match v {
                                Some(v) => {
                                    self.committed.insert(k, v);
                                }
                                None => {
                                    self.committed.remove(&k);
                                }
                            }
                        }
                        return out;
                    }
                    "fail" => {
                        self.staged.clear();
                        return out;
                    }
                    "host_request" => {
                        let id = out["id"].as_u64().unwrap_or(u64::MAX);
                        match out["kind"].as_str().unwrap_or("") {
                            "now_ms" => {
                                out = step(&json!({
                                    "type": "now_response", "id": id, "now_ms": self.now,
                                }));
                            }
                            "kv_get" | "kv_set" if self.kv_error.is_some() => {
                                out = step(&host_error(id, self.kv_error.unwrap_or("transient")));
                            }
                            "kv_get" => {
                                let key = out["payload"]["key"].as_str().unwrap_or("").to_string();
                                let value = self
                                    .committed
                                    .get(&key)
                                    .map(|v| Value::String(B64.encode(v)))
                                    .unwrap_or(Value::Null);
                                out = step(&json!({
                                    "type": "kv_response", "id": id, "value": value,
                                }));
                            }
                            "kv_set" => {
                                let key = out["payload"]["key"].as_str().unwrap_or("").to_string();
                                let value = out["payload"]["value"]
                                    .as_str()
                                    .and_then(|s| B64.decode(s).ok());
                                self.staged.insert(key, value);
                                out = step(&json!({ "type": "host_ok", "id": id }));
                            }
                            "log" if self.log_error.is_some() => {
                                out = step(&host_error(id, self.log_error.unwrap_or("transient")));
                            }
                            "log" => {
                                self.logs.push(
                                    out["payload"]["message"].as_str().unwrap_or("").to_string(),
                                );
                                out = step(&json!({ "type": "host_ok", "id": id }));
                            }
                            "pot_token" => {
                                self.pot_calls += 1;
                                out = match self.pot {
                                    Pot::Deny => step(&host_error(id, "permission-denied")),
                                    Pot::Cancelled => step(&host_error(id, "cancelled")),
                                    Pot::Status(s) => step(&http_response(id, s, "")),
                                    Pot::Token(t) => step(&http_response(
                                        id,
                                        200,
                                        &json!({ "poToken": t }).to_string(),
                                    )),
                                };
                            }
                            "http_request" => return out,
                            other => panic!("unexpected host request kind {other}"),
                        }
                    }
                    _ => return out,
                }
            }
        }

        /// Reply to the pending `http_request` and keep driving.
        fn answer(&mut self, out: &Value, status: u16, body: &str) -> Value {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            let next = step(&http_response(id, status, body));
            self.drive(next)
        }

        fn answer_headers(
            &mut self,
            out: &Value,
            status: u16,
            headers: &[(&str, &str)],
            body_len: usize,
        ) -> Value {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            let next = step(&json!({
                "type": "http_response",
                "id": id,
                "status": status,
                "headers": headers,
                "body": B64.encode(vec![b'x'; body_len]),
            }));
            self.drive(next)
        }

        fn answer_host_error(&mut self, out: &Value, kind: &str) -> Value {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            self.drive(step(&host_error(id, kind)))
        }
    }

    fn host_error(id: u64, kind: &str) -> Value {
        json!({
            "type": "host_error",
            "id": id,
            "error": { "kind": kind, "message": "host said no" },
        })
    }

    fn http_response(id: u64, status: u16, body: &str) -> Value {
        json!({
            "type": "http_response",
            "id": id,
            "status": status,
            "headers": [],
            "body": B64.encode(body),
        })
    }

    /// Invoke the default resolve and return rung 0's player request.
    fn begin(h: &mut Harness) -> Value {
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 0);
        out
    }

    fn header_of(out: &Value, name: &str) -> Option<String> {
        out["payload"]["headers"]
            .as_array()?
            .iter()
            .find(|h| h[0].as_str() == Some(name))
            .and_then(|h| h[1].as_str().map(str::to_string))
    }

    /// The rung index a player `http_request` is for, read off its
    /// client headers. Only meaningful for POSTs — probes are GET and
    /// carry no client headers.
    fn rung_of(out: &Value) -> usize {
        assert_eq!(out["type"], "host_request");
        assert_eq!(out["payload"]["method"], "POST");
        match (
            header_of(out, "X-YouTube-Client-Name").as_deref(),
            header_of(out, "X-YouTube-Client-Version").as_deref(),
        ) {
            (Some("101"), Some("1.02")) => 0,
            (Some("28"), Some("1.57.29")) => 1,
            (Some("5"), Some("20.10.4")) => 2,
            (Some("28"), Some("1.61.29")) => 3,
            (Some("28"), Some("1.62.27")) => 4,
            (Some("3"), Some("20.19.36")) => 5,
            (Some("28"), Some("1.61.48")) => 6,
            (Some("28"), Some("1.60.19")) => 7,
            (Some("28"), Some("1.43.32")) => 8,
            other => panic!("unexpected rung headers {other:?} in {out}"),
        }
    }

    /// Assert `out` is a GET tail probe for the OK fixture's pick
    /// (`contentLength` 4,557,665 → the file's last byte) and return
    /// `out`.
    fn probe_of(out: &Value) {
        assert_eq!(out["type"], "host_request");
        assert_eq!(out["payload"]["method"], "GET");
        assert_eq!(
            header_of(out, "Range").as_deref(),
            Some("bytes=4557664-4557664")
        );
    }

    /// The honest 206 for the OK fixture's pick: the last byte
    /// `4557664-4557664`, span 1.
    fn answer_probe_206(h: &mut Harness, out: &Value) -> Value {
        h.answer_headers(
            out,
            206,
            &[("Content-Range", "bytes 4557664-4557664/4557665")],
            1,
        )
    }

    fn url_of(out: &Value) -> String {
        out["payload"]["url"].as_str().unwrap_or("").to_string()
    }

    /// `(kind, message)` with the SDK's `"<kind>: "` Display prefix
    /// stripped back off the message.
    fn fail_kind(out: &Value) -> (String, String) {
        assert_eq!(out["type"], "fail");
        let kind = out["error"]["kind"].as_str().unwrap_or("").to_string();
        let message = out["error"]["message"]
            .as_str()
            .unwrap_or("")
            .strip_prefix(&format!("{kind}: "))
            .unwrap_or(out["error"]["message"].as_str().unwrap_or(""))
            .to_string();
        (kind, message)
    }

    /// `feed` the pending player request a 200 `body` and keep driving
    /// — mint (denied by default), probe, and the next rung's player.
    fn feed(h: &mut Harness, out: &Value, body: &str) -> Value {
        h.answer(out, 200, body)
    }

    #[test]
    fn sabr_then_plain_succeeds_on_rung_two() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        assert_eq!(
            url_of(&out),
            "https://music.youtube.com/youtubei/v1/player?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false"
        );
        let out = feed(&mut h, &out, SABR);
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, SABR);
        assert_eq!(rung_of(&out), 2);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "IOS");
        assert_eq!(out["result"]["mime"], "audio/mp4");
        assert_eq!(out["result"]["bitrate_kbps"], 130);
        assert_eq!(out["result"]["itag"], 140);
        assert_eq!(out["result"]["expires_at_ms"], 1_893_456_000_000u64);
        assert!(out["result"]["url"]
            .as_str()
            .unwrap_or("")
            .starts_with("https://"));
        // The minted URL is served by the edge under this rung's
        // client identity — the fetch must ride the same UA.
        assert_eq!(
            out["result"]["headers"]["user-agent"],
            "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)"
        );
    }

    #[test]
    fn last_rung_success_reports_client() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for rung in 1..=8usize {
            out = feed(&mut h, &out, SABR);
            assert_eq!(rung_of(&out), rung);
        }
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "ANDROID_VR@1.43.32");
    }

    #[test]
    fn probe_403_advances_to_next_rung() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = h.answer(&out, 403, "");
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_redirect_follows_location_to_done() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        // Edge-balance 302: the verdict runs on the Location target,
        // re-requested through the authorized path.
        let out = h.answer_headers(
            &out,
            302,
            &[(
                "Location",
                "https://rr1---sn-edge.googlevideo.com/videoplayback?rn=1",
            )],
            0,
        );
        probe_of(&out);
        assert_eq!(
            url_of(&out),
            "https://rr1---sn-edge.googlevideo.com/videoplayback?rn=1"
        );
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
    }

    #[test]
    fn probe_second_redirect_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = h.answer_headers(
            &out,
            302,
            &[(
                "Location",
                "https://rr1---sn-edge.googlevideo.com/videoplayback?rn=1",
            )],
            0,
        );
        probe_of(&out);
        let out = h.answer_headers(
            &out,
            302,
            &[(
                "Location",
                "https://rr2---sn-edge.googlevideo.com/videoplayback?rn=2",
            )],
            0,
        );
        // One hop is the bound: the second 3xx caps the mint and the
        // ladder moves on.
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_416_with_known_length_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = h.answer(&out, 416, "");
        assert_eq!(rung_of(&out), 1);
    }

    /// Strip `contentLength` so the probe falls back to a fixed window.
    fn ok_without_length() -> String {
        let mut body: Value = serde_json::from_str(OK).unwrap_or_default();
        let Some(fmts) = body
            .pointer_mut("/streamingData/adaptiveFormats")
            .and_then(Value::as_array_mut)
        else {
            panic!("fixture has no adaptiveFormats");
        };
        for f in fmts {
            if let Some(o) = f.as_object_mut() {
                o.remove("contentLength");
            }
        }
        body.to_string()
    }

    #[test]
    fn probe_416_without_length_means_short_file_done() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &ok_without_length());
        assert_eq!(out["payload"]["method"], "GET");
        assert_eq!(
            header_of(&out, "Range").as_deref(),
            Some("bytes=1048576-1114111")
        );
        // `bytes */900000` is the range evidence: the file ends before
        // the probe start (1,048,576), so the mint serves the file.
        let out = h.answer_headers(&out, 416, &[("Content-Range", "bytes */900000")], 0);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
    }

    #[test]
    fn probe_416_fallback_with_large_total_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &ok_without_length());
        // `bytes */2000000` claims the file reaches past the probe start
        // yet refused the in-range window — a cap wearing a 416.
        let out = h.answer_headers(&out, 416, &[("Content-Range", "bytes */2000000")], 0);
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_416_fallback_without_range_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &ok_without_length());
        // A bare 416 carries no evidence the file is short — the cap
        // horizon sits in the same window, so it cannot pass.
        let out = h.answer(&out, 416, "");
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_on_fallback_window_is_done() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &ok_without_length());
        assert_eq!(out["payload"]["method"], "GET");
        assert_eq!(
            header_of(&out, "Range").as_deref(),
            Some("bytes=1048576-1114111")
        );
        // The file (3,000,000) runs past the window, so `end + 1 ==
        // total` can never hold here — the fallback pass bar is the
        // whole asked span from a matching start.
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 1048576-1114111/3000000")],
            65536,
        );
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        assert_eq!(out["result"]["content_length"], Value::Null);
    }

    #[test]
    fn probe_206_fallback_short_of_window_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &ok_without_length());
        // Starts at the window but ends before both the window's last
        // byte and the file's EOF — a cap cutting mid-window.
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 1048576-1090000/3000000")],
            41425,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_fallback_truncated_at_eof_is_done() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &ok_without_length());
        // The file ends inside the window: the advertised span runs
        // short of the ask but `end + 1 == total` holds.
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 1048576-1099999/1100000")],
            51424,
        );
        assert_eq!(out["type"], "done");
    }

    #[test]
    fn probe_200_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        // A 200 means the edge ignored the Range ask and streams from
        // byte 0 — it proves nothing about serving past the horizon,
        // so the mint is judged capped, not transport weather.
        let out = h.answer(&out, 200, "x");
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_without_content_range_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        // A 206 with no echoed range — even with a full body — verifies
        // nothing about the tail.
        let out = h.answer_headers(&out, 206, &[], 65536);
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_wrong_start_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 0-65535/4557665")],
            65536,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_stopping_before_eof_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        // Right start, but the reported total runs one byte past the
        // manifest's length — the range never reaches the file's real
        // last byte, so it proves nothing about serving past a horizon.
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 4557664-4557664/4557666")],
            1,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_truncated_body_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        // The echoed range claims the last byte but the body is empty —
        // a truncated answer carries no evidence.
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 4557664-4557664/4557665")],
            0,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_oversized_body_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        // A body wider than the asked span is a mismatch too — the
        // verdict keys on the exact span, not "at least the span".
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 4557664-4557664/4557665")],
            2,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_429_reports_rate_limit() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 429, "");
        }
        // Rate-limited mints are weather too — the redraw pass re-asks
        // each on the second edge.
        for _ in 0..9 {
            out = h.answer(&out, 429, "");
        }
        assert_eq!(
            fail_kind(&out),
            ("rate-limit".to_string(), "rate-limit".to_string())
        );
    }

    #[test]
    fn probe_5xx_is_transport() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 503, "");
        }
        for _ in 0..9 {
            out = h.answer(&out, 503, "");
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "transport".to_string())
        );
    }

    #[test]
    fn non_json_2xx_is_the_wall_not_an_abort() {
        // A 200 carrying markup is the edge's interstitial answering
        // OK — the rung books the wall (the attested replay it earns is
        // covered by `markup_2xx_earns_an_attested_replay`) and the
        // ladder walks on; it must not die `invalid-response` with
        // seven rungs untried.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer(&out, 200, "<html>oops</html>");
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
    }

    #[test]
    fn missing_envelope_status_advances_as_no_audio() {
        // A JSON body without `playabilityStatus` is the parser's
        // legacy "playable" shape — `format_outcome` reports no-audio
        // honestly and the rung advances. An envelope shape the parser
        // predates can no longer starve the rungs behind it.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer(&out, 200, &json!({ "videoDetails": {} }).to_string());
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "ANDROID_VR@1.57.29");
    }

    #[test]
    fn truncated_2xx_body_is_weather_not_abort() {
        // A JSON-shaped 2xx that fails to parse is a truncated envelope:
        // the same marker scan a truncated 403 gets — no wall marker
        // here, so Transport, and the ladder advances.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer(
            &out,
            200,
            "{\"playabilityStatus\":{\"status\":\"OK\",\"streami",
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn markup_2xx_under_visitor_drops_and_reasks() {
        // The wall in transport form carries the same replayed-state
        // rule as the 403 wall: a 200-HTML answer under a persisted
        // visitor drops it and re-asks once bare.
        let mut h = Harness::new();
        h.committed
            .insert("visitor/VISIONOS".into(), b"persisted-visitor".to_vec());
        let out = begin(&mut h);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("persisted-visitor")
        );
        let out = h.answer(&out, 200, "<html><body>sorry</body></html>");
        // Same rung re-asked bare — not advanced.
        assert_eq!(rung_of(&out), 0);
        assert_eq!(header_of(&out, "X-Goog-Visitor-Id"), None);
    }

    #[test]
    fn markup_2xx_earns_an_attested_replay() {
        // The 200-wall is bot taxonomy: with a provider the rung gets
        // its attested replay like any bot-check — the wall's remedy
        // must reach every shape the wall takes.
        let mut h = Harness::new();
        h.pot = Pot::Token("pot-1");
        let out = begin(&mut h);
        let out = h.answer(&out, 200, "<html><body>sorry</body></html>");
        // The rest of the bare ladder walls too.
        let mut out = out;
        for _ in 1..LADDER.len() {
            out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        }
        // Pass 1 replays only attestable walled rungs — VISIONOS (0)
        // and IOS (2) — carrying the PoT.
        for expected in [0usize, 2] {
            assert_eq!(rung_of(&out), expected);
            let body = String::from_utf8(
                B64.decode(out["payload"]["body"].as_str().unwrap_or(""))
                    .unwrap_or_default(),
            )
            .unwrap_or_default();
            assert!(body.contains("serviceIntegrityDimensions"));
            out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        }
        // Attestation failed to lift the wall — the redraw pass re-asks
        // every walled rung bare on the second edge.
        for _ in 0..LADDER.len() {
            assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
            out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        }
        assert_eq!(
            fail_kind(&out),
            ("provider-wall".to_string(), "bot-check".to_string())
        );
    }

    #[test]
    fn all_capped_fails_streams_capped() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 403, "");
        }
        // Capped mints are per-mint stochastic — each earns a redraw on
        // the second edge, mint and probe again. Pass 0 already spent
        // 18 calls of the 32-call HTTP budget, so only the redraws
        // whose full chain (player + probe + re-probe) still fits are
        // emitted: six of nine.
        for _ in 0..6 {
            assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 403, "");
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "streams-capped".to_string())
        );
    }

    #[test]
    fn response_id_mismatch_is_invalid_response() {
        let mut h = Harness::new();
        let _player = begin(&mut h);
        // A response whose id is not the outstanding request's is a
        // host protocol violation — the SDK rejects it before dispatch.
        let out = step(&http_response(9_999, 200, OK));
        assert_eq!(fail_kind(&out).0, "invalid-response");
    }

    #[test]
    fn host_error_id_mismatch_is_invalid_response() {
        let mut h = Harness::new();
        let _out = begin(&mut h);
        let out = step(&host_error(9_999, "transient"));
        assert_eq!(fail_kind(&out).0, "invalid-response");
    }

    #[test]
    fn bot_check_on_every_rung_is_terminal() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        // No early exit: the bare pass walks every rung, then the
        // no-mint redraw re-asks each wall on the second edge — a
        // fully walled IP costs the whole ladder twice before
        // failing.
        for _ in 0..LADDER.len() {
            out = feed(&mut h, &out, BOT);
        }
        for _ in 0..LADDER.len() {
            assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
            out = feed(&mut h, &out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("provider-wall".to_string(), "bot-check".to_string())
        );
    }

    #[test]
    fn visitor_id_propagates_to_next_rung() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        // rung 0 response carries visitorData -> rung 1 request headers.
        let out = feed(&mut h, &out, SABR);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D")
        );
    }

    #[test]
    fn request_shape_and_headers() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        assert_eq!(out["payload"]["method"], "POST");
        // Native identities send no web-origin headers and use api
        // format version 2.
        assert!(header_of(&out, "X-Origin").is_none());
        assert!(header_of(&out, "Referer").is_none());
        assert_eq!(
            header_of(&out, "X-Goog-Api-Format-Version").as_deref(),
            Some("2")
        );
        assert!(header_of(&out, "Origin").is_none());
        let body = String::from_utf8(
            B64.decode(out["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert!(body.contains("\"videoId\":\"vid12345678\""));
        assert!(body.contains("\"contentCheckOk\":true"));
        assert!(body.contains("\"clientName\":\"VISIONOS\""));
        // The anonymous-user object rides every native request.
        assert!(body.contains("\"user\":{}"));
        // The bare pass carries no attestation — the shared token only
        // enters `serviceIntegrityDimensions` on the bot-replay pass.
        assert!(!body.contains("serviceIntegrityDimensions"));
    }

    #[test]
    fn mint_binds_video_id_without_visitor() {
        // Drive manually so the pot_token request is visible.
        reset_for_testing();
        let mut out = step(&json!({
            "type": "invoke", "request_id": "t1", "capability": "playback.resolve",
            "payload": { "source_ref": VID },
        }));
        // now → kv(last-good) → kv(last-edge) → kv(backoff) → kv(visitor) → player.
        for _ in 0..5 {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            out = match out["kind"].as_str().unwrap_or("") {
                "now_ms" => step(&json!({"type":"now_response","id":id,"now_ms":NOW})),
                "kv_get" => step(&json!({"type":"kv_response","id":id,"value":null})),
                other => panic!("unexpected kind {other}"),
            };
        }
        assert_eq!(out["kind"], "http_request");
        let id = out["id"].as_u64().unwrap_or(u64::MAX);
        let mut out = step(&http_response(id, 200, OK));
        // The pot/aside KV read rides between the pick and the mint.
        loop {
            match out["kind"].as_str().unwrap_or("") {
                "kv_get" => {
                    let id = out["id"].as_u64().unwrap_or(u64::MAX);
                    out = step(&json!({"type":"kv_response","id":id,"value":null}));
                }
                "pot_token" => break,
                other => panic!("unexpected kind {other}"),
            }
        }
        assert_eq!(out["kind"], "pot_token");
        assert_eq!(out["payload"]["content_binding"], VID);
    }

    #[test]
    fn mint_binds_video_id_even_when_response_carries_visitor() {
        // The shared token is video-bound: it attests player requests
        // (player-context tokens bind to the video id) and decorates
        // GVS URLs, where upstream now expects video binding too —
        // never the visitor.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let mut body: Value = serde_json::from_str(OK).unwrap_or_default();
        body["responseContext"] = json!({ "visitorData": "visitor-xyz" });
        // Answer manually to see the mint request.
        let id = out["id"].as_u64().unwrap_or(u64::MAX);
        let mut out = step(&http_response(id, 200, &body.to_string()));
        // The staged visitor kv_set comes first.
        assert_eq!(out["kind"], "kv_set");
        let id = out["id"].as_u64().unwrap_or(u64::MAX);
        out = step(&json!({ "type": "host_ok", "id": id }));
        // Then the pot/aside KV reads (global, then video) before the
        // mint call.
        loop {
            match out["kind"].as_str().unwrap_or("") {
                "kv_get" => {
                    let id = out["id"].as_u64().unwrap_or(u64::MAX);
                    out = step(&json!({"type":"kv_response","id":id,"value":null}));
                }
                "pot_token" => break,
                other => panic!("unexpected kind {other}"),
            }
        }
        assert_eq!(out["kind"], "pot_token");
        assert_eq!(out["payload"]["content_binding"], VID);
    }

    #[test]
    fn mint_denied_probes_bare_url() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        assert!(!url_of(&out).contains("pot="));
    }

    #[test]
    fn mint_ok_decorates_probe_and_result() {
        let mut h = Harness {
            pot: Pot::Token("tok-abc"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        assert!(url_of(&out).contains("pot=tok-abc"));
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert!(out["result"]["url"]
            .as_str()
            .unwrap_or("")
            .contains("pot=tok-abc"));
    }

    #[test]
    fn mint_cancelled_propagates() {
        let mut h = Harness {
            pot: Pot::Cancelled,
            ..Harness::new()
        };
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        assert_eq!(fail_kind(&out).0, "cancelled");
    }

    #[test]
    fn mint_fires_once_per_resolve() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        // rung 0 pick -> denied mint -> probe 403 -> rung 1.
        out = feed(&mut h, &out, OK);
        probe_of(&out);
        out = h.answer(&out, 403, "");
        assert_eq!(rung_of(&out), 1);
        // rung 1 pick -> straight to probe; no second mint.
        out = feed(&mut h, &out, OK);
        probe_of(&out);
        assert_eq!(h.pot_calls, 1);
    }

    /// The decoded JSON body of the pending player request.
    fn body_of(out: &Value) -> Value {
        serde_json::from_slice(
            &B64.decode(out["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default()
    }

    #[test]
    fn attested_replay_lifts_the_bot_wall() {
        // Live-verified shape: bare VISIONOS+IOS bot-check on a flagged
        // IP; the same rungs return full formats once the request
        // carries a video-bound poToken in serviceIntegrityDimensions.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let mut out = begin(&mut h);
        // Pass 1 runs bare: no attestation on the wire.
        assert!(body_of(&out)["context"]["serviceIntegrityDimensions"].is_null());
        // Pass 1 runs bare and walks the whole ladder: walls on the
        // Apple rungs never starve the bare-only Android rungs.
        out = feed(&mut h, &out, BOT); // VISIONOS
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.57.29
        out = feed(&mut h, &out, BOT); // IOS
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // The mint fires and pass 1 replays rung 0 attested (rung 1's
        // wall needs DroidGuard — never replayed).
        assert_eq!(rung_of(&out), 0);
        let body = body_of(&out);
        assert_eq!(
            body["context"]["serviceIntegrityDimensions"]["poToken"],
            "tok-xyz"
        );
        // The attested rung resolves: pick -> probe -> done.
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        assert!(url_of(&out).contains("pot=tok-xyz"));
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        // One mint served both attestation and the URL decoration.
        assert_eq!(h.pot_calls, 1);
        // The recovered rung's staged bot-backoff was cleared; the
        // still-walled rung's persists.
        assert!(!h
            .committed
            .contains_key(&format!("backoff/a/{VID}/VISIONOS")));
        assert!(h.committed.contains_key(&format!("backoff/a/{VID}/IOS")));
    }

    #[test]
    fn forbidden_json_body_is_transport_not_a_bot_wall() {
        // A 403 carrying a JSON error envelope is an API-level refusal
        // for that client — not the abuse-edge interstitial. It must
        // not consume the bot-check budget, and no mint fires.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let out = h.answer(&out, 403, "{\"error\":{\"code\":403}}");
        // Rung 0 recorded Transport → the ladder continues to rung 1.
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "ANDROID_VR@1.57.29");
        // The one mint is the finish_pick decoration — the JSON-403
        // rung booked Transport, so no attested replay ever ran.
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn forbidden_truncated_json_is_transport_not_a_bot_wall() {
        // A 403 carrying a JSON-shaped body that fails to parse is a
        // truncated API refusal — not the abuse-edge interstitial.
        // Booking it Bot would end the bare pass early and replay
        // only the walled rungs, skipping a later playable client.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        // A marker phrase outside `playabilityStatus` (here inside an
        // `error` envelope) is not the wall — it must not book Bot.
        let out = h.answer(&out, 403, "{\"error\":{\"message\":\"not a bot\",");
        let out = h.answer(&out, 403, "{\"error\":{\"code\":403,");
        // Two truncated refusals must not trip anything — the
        // ladder continues bare to rung 2.
        assert_eq!(rung_of(&out), 2);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "IOS");
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn truncated_compact_reason_is_transport_not_a_bot_wall() {
        // `"notabot"` as one word is not the bot-check phrase — word
        // boundaries must survive the whitespace normalization, so a
        // compact unrelated reason stays a truncated refusal.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let body = "{\"playabilityStatus\":{\"status\":\"LOGIN_REQUIRED\",\"reason\":\"notabot\",";
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        assert_eq!(rung_of(&out), 2);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "IOS");
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn truncated_bot_check_with_json_spacing_books_bot() {
        // Pretty-printed JSON can stretch the phrase's whitespace —
        // `"not   a    bot"` is still the bot-check reason.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let body =
            "{\"playabilityStatus\":{\"status\":\"LOGIN_REQUIRED\",\"reason\":\"not   a    bot";
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        // Non-attestable walls never end the bare pass — the
        // ladder is walked out, then rung 0 replays attested.
        let mut out = out;
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(rung_of(&out), 0);
        let body = body_of(&out);
        assert_eq!(
            body["context"]["serviceIntegrityDimensions"]["poToken"],
            "tok-xyz"
        );
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn forbidden_truncated_bot_check_gets_an_attested_replay() {
        // A 403 cut short mid-envelope cannot be parsed, but when the
        // surviving prefix still carries the bot-check marker it is
        // the wall truncated, not a refusal — it must book Bot and
        // enter the attested replay like the complete body does.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let body =
            "{\"playabilityStatus\":{\"status\":\"LOGIN_REQUIRED\",\"reason\":\"Sign in to confirm you're not a bot";
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        // Non-attestable walls never end the bare pass — the
        // ladder is walked out, then rung 0 replays attested.
        let mut out = out;
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(rung_of(&out), 0);
        let body = body_of(&out);
        assert_eq!(
            body["context"]["serviceIntegrityDimensions"]["poToken"],
            "tok-xyz"
        );
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn truncated_bot_check_ignores_an_unrelated_later_ok_status() {
        // An unrelated `"status":"OK"` after the playabilityStatus
        // object must not veto its bot-check reason — the marker scan
        // is bounded to that object's own span.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let body = "{\"playabilityStatus\":{\"status\":\"LOGIN_REQUIRED\",\"reason\":\"not a bot\"},\"metadata\":{\"status\":\"OK\"},";
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        // Non-attestable walls never end the bare pass — the
        // ladder is walked out, then rung 0 replays attested.
        let mut out = out;
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(rung_of(&out), 0);
        let body = body_of(&out);
        assert_eq!(
            body["context"]["serviceIntegrityDimensions"]["poToken"],
            "tok-xyz"
        );
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn truncated_bot_check_does_not_decode_structural_escapes() {
        // A `\u0022` inside a string VALUE must stay escaped — decoded
        // it would forge a `"status":"ok"` and the closed-span veto
        // would book a genuine wall as a transport failure.
        let forged = "{\"playabilityStatus\":{\"status\":\"LOGIN_REQUIRED\",\"reason\":\"Sign in to confirm you're not a bot\",\"x\":\"\\u0022status\\u0022:\\u0022ok\\u0022\"}}";
        assert!(truncated_bot_check(forged.as_bytes()));
    }

    #[test]
    fn forbidden_json_bot_check_gets_an_attested_replay() {
        // A JSON envelope can still carry the wall: a 403 whose
        // playabilityStatus is a bot-check books like the HTML
        // interstitial — attested replay, not a transport shrug.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let body =
            "{\"playabilityStatus\":{\"status\":\"LOGIN_REQUIRED\",\"reason\":\"Sign in to confirm you're not a bot\"}}";
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        let out = h.answer(&out, 403, body);
        // Non-attestable walls never end the bare pass — the
        // ladder is walked out, then rung 0 replays attested.
        let mut out = out;
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(rung_of(&out), 0);
        let body = body_of(&out);
        assert_eq!(
            body["context"]["serviceIntegrityDimensions"]["poToken"],
            "tok-xyz"
        );
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn forbidden_player_response_gets_an_attested_replay() {
        // The wall's transport shape: Google's abuse edge answers a
        // flagged IP's player request with a bare 403 and an HTML
        // interstitial — never JSON. It books like a BotCheck so the
        // attested replay fires (a Transport verdict never did).
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        assert!(body_of(&out)["context"]["serviceIntegrityDimensions"].is_null());
        let out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        let out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        let out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        // Walls never end the bare pass — the rest of the ladder still
        // runs, then pass 2 replays rung 0 attested after the mint.
        let mut out = out;
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(rung_of(&out), 0);
        let body = body_of(&out);
        assert_eq!(
            body["context"]["serviceIntegrityDimensions"]["poToken"],
            "tok-xyz"
        );
        // The attested rung resolves: pick -> probe -> done.
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        assert!(url_of(&out).contains("pot=tok-xyz"));
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        assert_eq!(h.pot_calls, 1);
        // The recovered rung's staged bot-backoff was cleared; the
        // still-walled rung's persists.
        assert!(!h
            .committed
            .contains_key(&format!("backoff/a/{VID}/VISIONOS")));
        assert!(h.committed.contains_key(&format!("backoff/a/{VID}/IOS")));
    }

    #[test]
    fn non_attestable_bot_checks_never_starve_later_rungs() {
        // A bot-check on a rung attestation cannot lift proves nothing
        // about later clients — it must not spend the bare budget.
        // SABR on the Apple rungs + walls on the plain-UA ANDROID_VR
        // rungs still leaves the Oculus pin reachable.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = feed(&mut h, &out, SABR); // VISIONOS — harvests a fresh visitor
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.57.29 — no budget spend
                                       // The wall fired while replaying the fresh visitor: the rung is
                                       // re-asked once bare before the outcome is recorded.
        assert_eq!(rung_of(&out), 1);
        assert_eq!(header_of(&out, "X-Goog-Visitor-Id"), None);
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.57.29 bare — no budget spend
        out = feed(&mut h, &out, SABR); // IOS — re-harvests the visitor
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.61.29 — no budget spend
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.61.29 bare — no budget spend
        out = feed(&mut h, &out, UNPLAYABLE); // ANDROID_VR@1.62.27
        out = feed(&mut h, &out, UNPLAYABLE); // ANDROID
        assert_eq!(rung_of(&out), 6);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "ANDROID_VR@1.61.48");
        // No attestable rung bot-checked -> no mint, no replay pass.
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn attested_replay_also_walled_is_terminal() {
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let out = begin(&mut h);
        let mut out = feed(&mut h, &out, BOT);
        for _ in 0..8 {
            out = feed(&mut h, &out, BOT);
        }
        // Attested replay of rung 0 then rung 2 (non-attestable walls
        // need DroidGuard — never replayed) — still walled.
        assert_eq!(rung_of(&out), 0);
        let out = feed(&mut h, &out, BOT);
        assert_eq!(rung_of(&out), 2);
        let mut out = feed(&mut h, &out, BOT);
        // Attestation spent, walls stand: the last remedy is the bare
        // redraw of every walled rung on the second edge.
        for _ in 0..9 {
            assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
            out = feed(&mut h, &out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("provider-wall".to_string(), "bot-check".to_string())
        );
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn attested_replay_supersedes_bot_for_unsupported() {
        // Bare rungs bot-check; the attested replays then prove the
        // rungs' formats are all SABR. The superseded Bot records must
        // not poison the all-restricted read — the replay's verdict is
        // the rung's real answer.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        let first = begin(&mut h);
        let mut out = feed(&mut h, &first, BOT);
        out = feed(&mut h, &out, SABR);
        out = feed(&mut h, &out, BOT);
        // The rung-2 wall replayed the fresh visitor SABR harvested —
        // it is re-asked once bare before the outcome is recorded.
        out = feed(&mut h, &out, BOT);
        for _ in 0..6 {
            out = feed(&mut h, &out, SABR);
        }
        assert_eq!(rung_of(&out), 0);
        out = feed(&mut h, &out, SABR);
        assert_eq!(rung_of(&out), 2);
        let out = feed(&mut h, &out, SABR);
        assert_eq!(
            fail_kind(&out),
            ("unsupported".to_string(), "sabr-only".to_string())
        );
    }

    #[test]
    fn bot_wall_without_provider_keeps_terminal_shape() {
        // No POT provider: attestation is skipped and the wall still
        // ends the resolve with the same typed failure — but only
        // after the second-edge redraw spent its draws. One
        // locally-denied `pot_token` call plus the redraw are the only
        // added costs.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..LADDER.len() {
            out = feed(&mut h, &out, BOT);
        }
        for _ in 0..LADDER.len() {
            assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
            out = feed(&mut h, &out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("provider-wall".to_string(), "bot-check".to_string())
        );
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn stored_bot_backoff_gets_an_attested_replay() {
        // A persisted bot-backoff skips the bare request — but it is
        // exactly what attestation exists to break, so the rung still
        // gets an attested retry in pass 2.
        let mut h = Harness {
            pot: Pot::Token("tok-xyz"),
            ..Harness::new()
        };
        h.committed.insert(
            format!("backoff/a/{VID}/VISIONOS"),
            json!({ "until_ms": NOW + 60_000, "reason": "bot-check" })
                .to_string()
                .into_bytes(),
        );
        // Pass 1: rung 0 is backoff-skipped; rung 1's live bot-check
        // is non-attestable so it never spends the budget — the pass
        // walks the rest of the ladder before attestation.
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 1);
        let mut out = feed(&mut h, &out, BOT);
        out = feed(&mut h, &out, BOT);
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // Pass 2 replays rung 0 first despite its stored backoff.
        assert_eq!(rung_of(&out), 0);
        assert_eq!(
            body_of(&out)["context"]["serviceIntegrityDimensions"]["poToken"],
            "tok-xyz"
        );
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        // Recovery erased the stale backoff.
        assert!(!h
            .committed
            .contains_key(&format!("backoff/a/{VID}/VISIONOS")));
    }

    #[test]
    fn all_sabr_fails_unsupported_sabr() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = feed(&mut h, &out, SABR);
        }
        assert_eq!(
            fail_kind(&out),
            ("unsupported".to_string(), "sabr-only".to_string())
        );
    }

    #[test]
    fn all_ciphered_fails_unsupported_ciphered() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = feed(&mut h, &out, CIPHERED);
        }
        assert_eq!(
            fail_kind(&out),
            ("unsupported".to_string(), "ciphered-only".to_string())
        );
    }

    #[test]
    fn bot_mixed_with_sabr_is_transient_not_unsupported() {
        // `unsupported` claims every rung proved plain audio absent —
        // a bot-checked rung demonstrated nothing, so a ladder mixing
        // bot-checks with sabr-only answers is weather, not a
        // restriction verdict.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = feed(&mut h, &out, BOT);
        for _ in 0..8 {
            out = feed(&mut h, &out, SABR);
        }
        // The single bare bot-check does not end the pass, so the
        // ladder ran to exhaustion; the denied mint ends attestation
        // and hands the wall to the second-edge redraw.
        assert_eq!(h.pot_calls, 1);
        assert_eq!(rung_of(&out), 0);
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        // The redraw rides SABR's fresh visitor — the wall drops it
        // and re-asks once bare, the same healing the primary edge
        // gets.
        let out = feed(&mut h, &out, BOT);
        let out = feed(&mut h, &out, BOT);
        assert_eq!(
            fail_kind(&out),
            ("provider-wall".to_string(), "bot-check".to_string())
        );
    }

    #[test]
    fn transport_mixed_with_sabr_is_transient_not_unsupported() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = h.answer(&out, 500, "{}");
        for _ in 0..8 {
            out = feed(&mut h, &out, SABR);
        }
        // Transport is weather too — the rung redraws on the second edge.
        assert_eq!(rung_of(&out), 0);
        let out = h.answer(&out, 500, "{}");
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "transport".to_string())
        );
    }

    #[test]
    fn rate_limit_wins_over_last_bucket() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = feed(&mut h, &out, UNPLAYABLE);
        out = h.answer(&out, 429, "{}");
        for _ in 0..7 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // The 429 earns a second-edge redraw before the verdict lands.
        assert_eq!(rung_of(&out), 1);
        let out = h.answer(&out, 429, "{}");
        assert_eq!(
            fail_kind(&out),
            ("rate-limit".to_string(), "rate-limit".to_string())
        );
    }

    #[test]
    fn unavailable_ladder_fails_no_result() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(
            fail_kind(&out),
            ("no-result".to_string(), "unavailable".to_string())
        );
    }

    #[test]
    fn host_error_advances_rung() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer_host_error(&out, "transient");
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn host_rate_limit_keeps_taxonomy_and_backoff() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        // A host-side `rate-limit` on the player call is not generic
        // transport weather: it stages the 60 s rate-limit backoff.
        let out = h.answer_host_error(&out, "rate-limit");
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        let stored: Value = serde_json::from_slice(
            h.committed
                .get(&format!("backoff/a/{VID}/VISIONOS"))
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(stored["reason"], "rate-limit");
        assert_eq!(stored["until_ms"], NOW + 60_000);
    }

    #[test]
    fn host_rate_limit_on_every_rung_fails_rate_limit() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = h.answer_host_error(&out, "rate-limit");
        }
        for _ in 0..9 {
            out = h.answer_host_error(&out, "rate-limit");
        }
        assert_eq!(
            fail_kind(&out),
            ("rate-limit".to_string(), "rate-limit".to_string())
        );
    }

    #[test]
    fn cancelled_host_error_propagates() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer_host_error(&out, "cancelled");
        assert_eq!(fail_kind(&out).0, "cancelled");
    }

    #[test]
    fn permission_denied_is_terminal() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer_host_error(&out, "permission-denied");
        assert_eq!(fail_kind(&out).0, "permission-denied");
    }

    /// A player response whose `videoDetails.videoId` is not the
    /// requested id never reaches the picker — its stream URL would be
    /// the wrong song. The rung counts as unavailable and the ladder
    /// advances; a response carrying no `videoDetails` at all is not
    /// penalized (some rungs omit it).
    #[test]
    fn wrong_video_id_never_reaches_picker() {
        let mut wrong: Value = serde_json::from_str(OK).unwrap_or_default();
        wrong["videoDetails"] = json!({ "videoId": "a-different-video" });
        let wrong = wrong.to_string();
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &wrong);
        assert_eq!(rung_of(&out), 1);
        // Every rung answering the wrong video -> honest no-result.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..9 {
            out = feed(&mut h, &out, &wrong);
        }
        assert_eq!(
            fail_kind(&out),
            ("no-result".to_string(), "unavailable".to_string())
        );
    }

    // ---- KV visitors, backoff, pinning ------------------------------

    #[test]
    fn persisted_visitor_is_replayed_on_first_player_call() {
        let mut h = Harness::new();
        h.committed
            .insert("visitor/VISIONOS".into(), b"persisted-visitor".to_vec());
        let out = begin(&mut h);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("persisted-visitor")
        );
    }

    #[test]
    fn malformed_visitor_kv_is_ignored_with_warn() {
        let mut h = Harness::new();
        h.committed
            .insert("visitor/VISIONOS".into(), vec![0xff, 0xfe]);
        let out = begin(&mut h);
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        assert!(h.logs.iter().any(|m| m.contains("malformed visitor")));
    }

    #[test]
    fn response_visitor_commits_for_next_invocation() {
        let mut h = Harness::new();
        // rung 0 serves SABR + visitorData (staged), rung 1 resolves.
        let out = begin(&mut h);
        let out = feed(&mut h, &out, SABR);
        assert_eq!(rung_of(&out), 1);
        // rung 1 already replays the fresh visitor in-invocation.
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D")
        );
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        // The staged write committed under rung 0's key, and the
        // finishing rung left the `ladder/last-good` hint pointing at
        // rung 1.
        assert_eq!(
            h.committed.get("visitor/VISIONOS").map(Vec::as_slice),
            Some(b"Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D".as_slice())
        );
        assert_eq!(
            h.committed.get("ladder/last-good").map(Vec::as_slice),
            Some(b"ANDROID_VR@1.57.29".as_slice())
        );
        // A fresh invocation leads with the hinted rung; VISIONOS
        // follows in static order and replays the committed value
        // through KV — the hinted rung's transport failure keeps the
        // walk honest.
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 1);
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        let out = h.answer_host_error(&out, "transient");
        assert_eq!(rung_of(&out), 0);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D")
        );
    }

    #[test]
    fn walled_replayed_visitor_reasks_bare_once_and_heals() {
        // A persisted visitor is suspect when the rung walls: re-ask
        // once without it, and stage the KV delete so a poisoned value
        // is gone once the resolve commits.
        let mut h = Harness::new();
        h.committed
            .insert("visitor/VISIONOS".into(), b"persisted-visitor".to_vec());
        let out = begin(&mut h);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("persisted-visitor")
        );
        let out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        // Same rung re-asked bare — not advanced.
        assert_eq!(rung_of(&out), 0);
        assert_eq!(header_of(&out, "X-Goog-Visitor-Id"), None);
        // The bare re-ask serves: fresh visitor heals the KV slot on
        // commit; a later invocation replays the healed value.
        let out = feed(&mut h, &out, SABR);
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(
            h.committed.get("visitor/VISIONOS").map(Vec::as_slice),
            Some(b"Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D".as_slice())
        );
    }

    #[test]
    fn visitor_wall_reask_is_single_shot() {
        let mut h = Harness::new();
        h.committed
            .insert("visitor/VISIONOS".into(), b"persisted-visitor".to_vec());
        let mut out = begin(&mut h);
        out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        assert_eq!(rung_of(&out), 0);
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        // Still walled bare: the rung's outcome records and the ladder
        // advances — no second re-ask.
        out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn walled_fresh_visitor_clears_cross_rung_propagation() {
        // SABR's response visitor rides rung 1; when the wall fires the
        // fresh value is dropped entirely — the re-ask and every later
        // rung send no visitor — and its staged write is erased, so a
        // committed resolve cannot re-persist the burned token under its
        // source rung's key. Rung 1's own stored visitor went untested
        // under the shadow but is dropped too: a wall under replayed
        // state can't trust what the rung would replay next.
        let mut h = Harness::new();
        h.committed.insert(
            "visitor/ANDROID_VR@1.57.29".into(),
            b"shadowed-stored".to_vec(),
        );
        let mut out = begin(&mut h);
        out = feed(&mut h, &out, SABR);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D")
        );
        out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        assert_eq!(rung_of(&out), 1);
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        assert_eq!(rung_of(&out), 2);
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        // Both suspect slots healed: the shadowed stored visitor and the
        // fresh value's staged write under VISIONOS's key.
        assert!(!h.committed.contains_key("visitor/ANDROID_VR@1.57.29"));
        assert!(!h.committed.contains_key("visitor/VISIONOS"));
    }

    #[test]
    fn bare_wall_never_reasks() {
        // No replayed visitor -> a wall is the rung's own verdict and
        // the ladder advances without a re-ask.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn stored_backoff_skips_rung_without_player_call() {
        let mut h = Harness::new();
        h.committed.insert(
            format!("backoff/a/{VID}/VISIONOS"),
            json!({ "until_ms": NOW + 60_000, "reason": "rate-limit" })
                .to_string()
                .into_bytes(),
        );
        // rung 0 is skipped: the first player request is rung 1's.
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn stored_rate_limit_backoff_participates_in_taxonomy() {
        let mut h = Harness::new();
        for rung in [
            "VISIONOS",
            "IOS",
            "ANDROID_VR@1.57.29",
            "ANDROID_VR@1.61.29",
            "ANDROID_VR@1.62.27",
            "ANDROID",
            "ANDROID_VR@1.61.48",
            "ANDROID_VR@1.60.19",
            "ANDROID_VR@1.43.32",
        ] {
            h.committed.insert(
                format!("backoff/a/{VID}/{rung}"),
                json!({ "until_ms": NOW + 60_000, "reason": "rate-limit" })
                    .to_string()
                    .into_bytes(),
            );
        }
        // Every rung is backoff-skipped on the primary edge — but a
        // stored backoff never suppresses the second-edge redraw, so
        // the failure lands only after each rung's googleapis draw
        // rate-limits too.
        let mut out = h.invoke(json!({ "source_ref": VID }));
        for _ in 0..9 {
            out = h.answer_host_error(&out, "rate-limit");
        }
        assert_eq!(
            fail_kind(&out),
            ("rate-limit".to_string(), "rate-limit".to_string())
        );
    }

    #[test]
    fn expired_backoff_does_not_skip() {
        let mut h = Harness::new();
        h.committed.insert(
            format!("backoff/a/{VID}/VISIONOS"),
            json!({ "until_ms": NOW - 1, "reason": "rate-limit" })
                .to_string()
                .into_bytes(),
        );
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 0);
    }

    #[test]
    fn failed_rung_backoff_persists_after_later_success() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        // rung 0: 429 -> stage 60s rate-limit backoff; rung 1 resolves.
        let out = h.answer(&out, 429, "{}");
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        let Some(stored) = h.committed.get(&format!("backoff/a/{VID}/VISIONOS")) else {
            panic!("rung-0 backoff must commit on done");
        };
        let stored: Value = serde_json::from_slice(stored).unwrap_or_default();
        assert_eq!(stored["reason"], "rate-limit");
        assert_eq!(stored["until_ms"], NOW + 60_000);
        // The successful rung's own backoff key was cleared.
        assert!(!h.committed.contains_key(&format!("backoff/a/{VID}/IOS")));
    }

    #[test]
    fn all_failed_rolls_back_staged_backoff() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        // rung 0: 429 stages a backoff; the rest of the ladder serves
        // unplayable -> `fail` discards the staged write by contract.
        let mut out = h.answer(&out, 429, "{}");
        for _ in 0..8 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // The 429 redraws on the second edge; the same verdict stands.
        assert_eq!(rung_of(&out), 0);
        let out = h.answer(&out, 429, "{}");
        assert_eq!(fail_kind(&out).0, "rate-limit");
        assert!(!h
            .committed
            .contains_key(&format!("backoff/a/{VID}/VISIONOS")));
        assert!(h.staged.is_empty());
    }

    #[test]
    fn last_good_hint_leads_the_attempt_order() {
        // The rung that finished last time leads this resolve — the
        // flagged-IP convergence: second and later resolves open with
        // the client that serves bare, not a fresh walk.
        let mut h = Harness::new();
        h.committed
            .insert("ladder/last-good".into(), b"IOS".to_vec());
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 2);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        // The winner already held the hint — no rewrite needed.
        assert_eq!(
            h.committed.get("ladder/last-good").map(Vec::as_slice),
            Some(b"IOS".as_slice())
        );
    }

    #[test]
    fn last_good_hint_rewrites_to_the_new_winner() {
        // A hinted rung that walls costs the same bare POST the static
        // order would have paid; the winner takes the hint so the next
        // resolve converges on it.
        let mut h = Harness::new();
        h.committed
            .insert("ladder/last-good".into(), b"IOS".to_vec());
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 2);
        let out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        assert_eq!(rung_of(&out), 0);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(
            h.committed.get("ladder/last-good").map(Vec::as_slice),
            Some(b"VISIONOS".as_slice())
        );
    }

    #[test]
    fn stale_last_good_is_ignored_not_a_wedge() {
        // A hint naming no current rung is a stale build's artifact or
        // a foreign write — warned, ignored, static order.
        let mut h = Harness::new();
        h.committed
            .insert("ladder/last-good".into(), b"DEAD_RUNG".to_vec());
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 0);
        assert!(h.logs.iter().any(|m| m.contains("last-good")));
    }

    #[test]
    fn hinted_rung_backoff_skips_to_static_order() {
        // The hint permutes, it doesn't exempt: a hinted rung under an
        // in-force backoff is skipped without a request and the static
        // order takes over.
        let mut h = Harness::new();
        h.committed
            .insert("ladder/last-good".into(), b"ANDROID_VR@1.57.29".to_vec());
        h.committed.insert(
            format!("backoff/a/{VID}/ANDROID_VR@1.57.29"),
            json!({ "until_ms": NOW + 60_000, "reason": "bot-check" })
                .to_string()
                .into_bytes(),
        );
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 0);
    }

    #[test]
    fn hint_order_still_walks_every_rung() {
        // A permutation can never starve: with the last rung hinted,
        // every rung still gets exactly one bare shot.
        let mut h = Harness::new();
        h.committed
            .insert("ladder/last-good".into(), b"ANDROID_VR@1.43.32".to_vec());
        let mut out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 8);
        for expected in [0usize, 1, 2, 3, 4, 5, 6, 7] {
            out = feed(&mut h, &out, SABR);
            assert_eq!(rung_of(&out), expected);
        }
        out = feed(&mut h, &out, SABR);
        assert_eq!(
            fail_kind(&out),
            ("unsupported".to_string(), "sabr-only".to_string())
        );
    }

    #[test]
    fn hinted_order_keeps_the_static_failure_taxonomy() {
        // The hint permutes requests, never the verdict: rung 7 walled
        // and every other rung unavailable reads exactly like the cold
        // order — the last STATIC outcome (rung 7's bot-check), not the
        // last attempt-order one.
        let mut h = Harness::new();
        h.committed
            .insert("ladder/last-good".into(), b"ANDROID_VR@1.43.32".to_vec());
        let mut out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 8);
        out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        for expected in 0..8usize {
            assert_eq!(rung_of(&out), expected);
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // The hinted rung's wall earns its second-edge redraw.
        assert_eq!(rung_of(&out), 8);
        let out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        assert_eq!(
            fail_kind(&out),
            ("provider-wall".to_string(), "bot-check".to_string())
        );
    }

    #[test]
    fn hinted_restricted_rung_keeps_the_static_reason() {
        // Same rule for the all-restricted summary: the message names
        // the first STATIC rung's verdict, not the hinted rung's —
        // sabr-only either way, hinted or cold.
        let mut h = Harness::new();
        h.committed
            .insert("ladder/last-good".into(), b"ANDROID_VR@1.43.32".to_vec());
        let mut out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 8);
        out = feed(&mut h, &out, CIPHERED);
        for expected in 0..8usize {
            assert_eq!(rung_of(&out), expected);
            out = feed(&mut h, &out, SABR);
        }
        assert_eq!(
            fail_kind(&out),
            ("unsupported".to_string(), "sabr-only".to_string())
        );
    }

    #[test]
    fn transient_kv_failures_never_wedge() {
        // Every KV call answering non-terminal weather degrades the
        // advisory namespace to empty-store behavior: the resolve still
        // walks, mints, probes, and reports — a logging call per drop
        // but never a wedge.
        let mut h = Harness::new();
        h.kv_error = Some("transient");
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
    }

    #[test]
    fn cancelled_kv_still_aborts() {
        // `cancelled` is the abort signal, not weather — it must
        // propagate even out of an advisory read.
        let mut h = Harness::new();
        h.kv_error = Some("cancelled");
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(fail_kind(&out).0, "cancelled");
    }

    #[test]
    fn warn_channel_swallows_weather_not_terminal_kinds() {
        // Transient log weather degrades to swallowed diagnostics —
        // the stale-hint warning is dropped and the resolve walks on.
        let mut h = Harness::new();
        h.log_error = Some("transient");
        h.committed
            .insert("ladder/last-good".into(), b"DEAD_RUNG".to_vec());
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(rung_of(&out), 0);
        // A contract violation on the same channel stays terminal — a
        // log path answering `invalid-response` must still abort.
        let mut h = Harness::new();
        h.log_error = Some("invalid-response");
        h.committed
            .insert("ladder/last-good".into(), b"DEAD_RUNG".to_vec());
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(fail_kind(&out).0, "invalid-response");
    }

    #[test]
    fn foreign_top_pick_falls_back_to_servable_format() {
        // The best-ranked format sits off the allowlist but other
        // googlevideo audio remains in the same response — the rung
        // must try it rather than abandoning the response as capped.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let mut body: Value = serde_json::from_str(OK).unwrap_or_default();
        body["streamingData"]["adaptiveFormats"][3]["url"] =
            json!("https://cdn.example.com/v.mp4?expire=1893456000&itag=140");
        let out = feed(&mut h, &out, &body.to_string());
        // The probe goes to the next servable format (itag 251) — no
        // capped advance, no request spent on the foreign URL.
        assert_eq!(out["type"], "host_request");
        assert_eq!(out["payload"]["method"], "GET");
        assert!(url_of(&out).contains("itag=251"), "{}", url_of(&out));
    }

    #[test]
    fn foreign_pick_url_is_capped_not_probed() {
        // Every minted URL outside the `*.googlevideo.com` allowlist
        // can never be served — an all-foreign response books the rung
        // capped without spending a doomed probe request, and advances
        // to the next rung.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let mut body: Value = serde_json::from_str(OK).unwrap_or_default();
        let Some(fmts) = body["streamingData"]["adaptiveFormats"].as_array_mut() else {
            panic!("fixture has no adaptiveFormats");
        };
        for f in fmts {
            f["url"] = json!("https://cdn.example.com/v.mp4?expire=1893456000&itag=140");
        }
        let out = feed(&mut h, &out, &body.to_string());
        // No GET probe was emitted — the next request is rung 1's POST.
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_permission_denied_is_terminal() {
        // The picked URL already passed the local allowlist check, so
        // a host `permission-denied` is a contract failure — the
        // resolve reports it rather than booking a capped mint and
        // walking on to `streams-capped`.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = h.answer_host_error(&out, "permission-denied");
        assert_eq!(fail_kind(&out).0, "permission-denied");
    }

    #[test]
    fn redirect_to_foreign_host_is_capped() {
        // An edge redirect off the allowlist is a mint we cannot
        // serve — no re-request is made and the rung advances.
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = h.answer_headers(
            &out,
            302,
            &[("Location", "https://cdn.example.com/elsewhere")],
            0,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn pin_itag_resolves_pinned_format() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "source_ref": VID, "pin_itag": 251 }));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["itag"], 251);
        assert_eq!(out["result"]["mime"], "audio/webm");
    }

    #[test]
    fn pinned_resolve_preserves_transport_failures() {
        let mut h = Harness::new();
        let mut out = h.invoke(json!({ "source_ref": VID, "pin_itag": 251 }));
        for _ in 0..9 {
            out = h.answer_host_error(&out, "transient");
        }
        for _ in 0..9 {
            out = h.answer_host_error(&out, "transient");
        }
        assert_eq!(fail_kind(&out).0, "transient");
    }

    #[test]
    fn pinned_resolve_preserves_uninspectable_player_outcomes() {
        for (body, attempts, redraws, expected) in [
            (BOT, 9, 9, "provider-wall"),
            (
                r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED"}}"#,
                9,
                0,
                "auth-required",
            ),
            (SABR, 9, 0, "unsupported"),
            (CIPHERED, 9, 0, "unsupported"),
            (UNPLAYABLE, 9, 0, "no-result"),
            (
                r#"{"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[{"itag":140,"mimeType":"audio/mp4","url":"http://example.test/audio"}]}}"#,
                9,
                0,
                "no-result",
            ),
        ] {
            let mut h = Harness::new();
            let mut out = h.invoke(json!({ "source_ref": VID, "pin_itag": 251 }));
            for _ in 0..attempts {
                out = feed(&mut h, &out, body);
            }
            // Weather outcomes redraw once each on the second edge.
            for _ in 0..redraws {
                out = feed(&mut h, &out, body);
            }
            assert_eq!(fail_kind(&out).0, expected, "{body}");
        }
    }

    #[test]
    fn redraw_on_second_edge_lifts_the_wall() {
        // The wall is a per-edge verdict: every rung bot-checks on
        // music.youtube.com, no provider mints, and the redraw pass
        // re-asks each wall bare on youtubei.googleapis.com — where
        // rung 0's draw resolves.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..LADDER.len() {
            out = feed(&mut h, &out, BOT);
        }
        // The denied mint paid once; the redraw leads with last-good
        // order's first rung on the second edge.
        assert_eq!(h.pot_calls, 1);
        assert_eq!(rung_of(&out), 0);
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        assert!(header_of(&out, "X-Goog-Api-Format-Version").is_some());
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        // The recovering rung takes the last-good hint.
        assert_eq!(
            h.committed.get("ladder/last-good").map(Vec::as_slice),
            Some(b"VISIONOS".as_slice())
        );
    }

    #[test]
    fn redraw_skips_deterministic_verdicts() {
        // Only weather earns the second draw: the walled rung is
        // redrawn while its sign-in/unavailable neighbours stay
        // settled.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = feed(&mut h, &out, UNPLAYABLE); // VISIONOS — deterministic
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.57.29 — weather
        for _ in 0..7 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // Exactly one redrawn rung — the walled one.
        assert_eq!(rung_of(&out), 1);
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "ANDROID_VR@1.57.29");
        // The wall's verdict survives nowhere — the redraw's pick is
        // the rung's real answer, and no mint was spent (no attestable
        // wall ever stood).
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn redraw_does_not_replay_a_dropped_visitor() {
        // A visitor already burned on edge A must not ride edge B: the
        // dropped key is read as absent for the redraw, not replayed
        // from the pre-commit store.
        let mut h = Harness::new();
        h.committed
            .insert("visitor/VISIONOS".into(), b"persisted-visitor".to_vec());
        let mut out = begin(&mut h);
        // Rung 0 walls under the replayed visitor -> bare re-ask ->
        // still walled; the rest of the ladder is unplayable.
        out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        out = h.answer(&out, 403, "<html><body>sorry</body></html>");
        for _ in 0..8 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // The redraw of rung 0 carries no visitor header.
        assert_eq!(rung_of(&out), 0);
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        assert_eq!(header_of(&out, "X-Goog-Visitor-Id"), None);
    }

    #[test]
    fn redraw_win_persists_the_winning_edge() {
        // A sustained wall must not cost the dead edge's walk on every
        // later resolve: the redraw winner is written back so the next
        // resolve opens on the serving edge.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..LADDER.len() {
            out = feed(&mut h, &out, BOT);
        }
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(
            h.committed.get("ladder/last-edge").map(Vec::as_slice),
            Some(b"b".as_slice())
        );
    }

    #[test]
    fn last_edge_hint_opens_on_the_redraw_host() {
        // The "b" hint moves the redraw host into the primary passes'
        // slot: rung 0's first request goes to youtubei.googleapis.com
        // and the attested pass would replay on that same edge.
        let mut h = Harness::new();
        h.committed.insert("ladder/last-edge".into(), b"b".to_vec());
        let out = begin(&mut h);
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        // The hint is already correct — no rewrite.
        assert_eq!(
            h.committed.get("ladder/last-edge").map(Vec::as_slice),
            Some(b"b".as_slice())
        );
    }

    #[test]
    fn a_wall_on_the_hinted_edge_redraws_the_primary_host() {
        // With "b" primary, the redraw pass is the mirror image: walls
        // on the redraw host are re-asked on music.youtube.com.
        let mut h = Harness::new();
        h.committed.insert("ladder/last-edge".into(), b"b".to_vec());
        let mut out = begin(&mut h);
        for _ in 0..LADDER.len() {
            out = feed(&mut h, &out, BOT);
        }
        // Denied mint skips attestation; the redraw rides edge A.
        assert!(url_of(&out).starts_with("https://music.youtube.com/"));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(
            h.committed.get("ladder/last-edge").map(Vec::as_slice),
            Some(b"a".as_slice())
        );
    }

    #[test]
    fn edge_b_backoff_never_skips_edge_a() {
        // The reviewer's scenario: a cooldown staged on the second
        // edge must not suppress the same rung's bare shot on the
        // primary edge — the edges cool independently.
        let mut h = Harness::new();
        h.committed.insert(
            format!("backoff/b/{VID}/VISIONOS"),
            json!({ "until_ms": NOW + 60_000, "reason": "rate-limit" })
                .to_string()
                .into_bytes(),
        );
        let out = begin(&mut h);
        assert!(url_of(&out).starts_with("https://music.youtube.com/"));
    }

    #[test]
    fn each_edge_stages_its_own_backoff() {
        // A transport miss on each edge stages two independent records:
        // edge A's under `backoff/a/...`, the redraw's under
        // `backoff/b/...` — neither shadows the other. Staged writes
        // commit only on `done`, so the second redraw rung wins.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = h.answer(&out, 403, "{\"error\":{\"code\":403}}"); // rung 0, edge A
        for _ in 0..8 {
            out = feed(&mut h, &out, BOT);
        }
        // Denied mint skips attestation; the redraw re-asks rung 0's
        // transport miss on edge B first.
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        out = h.answer(&out, 403, "{\"error\":{\"code\":403}}"); // rung 0, edge B
        assert!(url_of(&out).starts_with("https://youtubei.googleapis.com/"));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert!(h
            .committed
            .contains_key(&format!("backoff/a/{VID}/VISIONOS")));
        assert!(h
            .committed
            .contains_key(&format!("backoff/b/{VID}/VISIONOS")));
        assert_eq!(
            h.committed.get("ladder/last-edge").map(Vec::as_slice),
            Some(b"b".as_slice())
        );
    }

    #[test]
    fn redraw_honors_its_own_edges_backoff() {
        // A live edge-B cooldown suppresses only the redraw's re-ask:
        // the rung walks edge A bare, then stays parked on the second
        // edge instead of re-hammering a cooled rung.
        let mut h = Harness::new();
        h.committed.insert(
            format!("backoff/b/{VID}/ANDROID_VR@1.57.29"),
            json!({ "until_ms": NOW + 60_000, "reason": "bot-check" })
                .to_string()
                .into_bytes(),
        );
        let mut out = begin(&mut h);
        out = feed(&mut h, &out, UNPLAYABLE); // rung 0 — deterministic
        out = feed(&mut h, &out, BOT); // rung 1 — wall on edge A
        for _ in 0..7 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // Denied mint skips attestation; the redraw reads the stored
        // edge-B record and ends the resolve instead of re-asking.
        assert_eq!(out["type"], "fail");
    }

    #[test]
    fn mixed_transport_then_wall_stays_retryable() {
        // Rung 0 books Transport (a JSON-403 the parser can't read as a
        // playability verdict), rungs 1-7 all bot-check. The ladder's
        // last verdict is the wall — but a retry may recover rung 0
        // before the walled rung is reached, so the kind stays
        // `transient` and `bot-check` rides the message.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = h.answer(&out, 403, "{\"error\":{\"code\":403}}");
        assert_eq!(rung_of(&out), 1);
        for _ in 0..8 {
            out = feed(&mut h, &out, BOT);
        }
        // The denied mint skips attestation; the redraw pass re-asks
        // rung 0's transport miss then every wall on the second edge.
        out = h.answer(&out, 403, "{\"error\":{\"code\":403}}");
        for _ in 0..8 {
            out = feed(&mut h, &out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "bot-check".to_string())
        );
    }

    #[test]
    fn pin_itag_missing_everywhere_is_expired_resource() {
        let mut h = Harness::new();
        let mut out = h.invoke(json!({ "source_ref": VID, "pin_itag": 774 }));
        for _ in 0..9 {
            out = feed(&mut h, &out, OK);
            if out["type"] == "host_request" && out["payload"]["method"] == "GET" {
                panic!("a missing pin must never reach the probe");
            }
        }
        assert_eq!(
            fail_kind(&out),
            (
                "expired-resource".to_string(),
                "pinned-itag-unavailable".to_string()
            )
        );
    }

    #[test]
    fn prefer_webm_picks_webm() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "source_ref": VID, "prefer": ["audio/webm"] }));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["mime"], "audio/webm");
        assert_eq!(out["result"]["itag"], 251);
    }

    #[test]
    fn resume_offset_is_accepted() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "source_ref": VID, "resume_offset": 1_048_576 }));
        assert_eq!(rung_of(&out), 0);
    }

    #[test]
    fn object_source_ref_resolves() {
        let mut h = Harness::new();
        let out = h.invoke(json!({
            "source_ref": { "provider": "youtube-music", "kind": "track", "id": VID },
        }));
        assert_eq!(rung_of(&out), 0);
        let body = String::from_utf8(
            B64.decode(out["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert!(body.contains("\"videoId\":\"vid12345678\""));
    }

    #[test]
    fn foreign_source_ref_is_not_applicable() {
        let mut h = Harness::new();
        let out = h.invoke(json!({
            "source_ref": { "provider": "itunes", "kind": "track", "id": "12345" },
        }));
        assert_eq!(fail_kind(&out).0, "not-applicable");
    }

    #[test]
    fn malformed_payloads_are_invalid_response() {
        let cases = [
            json!({}),                                // missing source_ref
            json!({ "source_ref": "" }),              // empty legacy ref
            json!({ "source_ref": "short" }),         // bad video id
            json!({ "source_ref": VID, "extra": 1 }), // unknown key
            json!({ "source_ref": VID, "target_bitrate_kbps": 0 }),
            json!({ "source_ref": VID, "target_bitrate_kbps": 513 }),
            json!({ "source_ref": VID, "target_bitrate_kbps": "128" }),
            json!({ "source_ref": VID, "prefer": ["audio/mp4", "audio/mp4"] }),
            json!({ "source_ref": VID, "prefer": ["video/mp4"] }),
            json!({ "source_ref": VID, "prefer": "audio/mp4" }),
            json!({ "source_ref": VID, "prefer": ["audio/mp4", "audio/webm", "audio/mp4"] }),
            json!({ "source_ref": VID, "pin_itag": -1 }),
            json!({ "source_ref": VID, "pin_itag": "140" }),
            json!({ "source_ref": VID, "resume_offset": "12" }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track" } }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track", "id": VID, "x": 1 } }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track", "id": "bad" } }),
        ];
        for payload in cases {
            let mut h = Harness::new();
            let out = h.invoke(payload.clone());
            assert_eq!(fail_kind(&out).0, "invalid-response", "{payload}");
        }
    }

    #[test]
    fn unsupported_capability_is_not_applicable() {
        reset_for_testing();
        let out = step(&json!({
            "type": "invoke", "request_id": "t1", "capability": "radio.start",
            "payload": {},
        }));
        assert_eq!(fail_kind(&out).0, "not-applicable");
    }

    /// An OK body whose itag-251 row is removed — a rung that does not
    /// carry the pinned format.
    fn ok_without_itag(itag: u64) -> String {
        let mut body: Value = serde_json::from_str(OK).unwrap_or_default();
        let Some(fmts) = body
            .pointer_mut("/streamingData/adaptiveFormats")
            .and_then(Value::as_array_mut)
        else {
            panic!("fixture has no adaptiveFormats");
        };
        fmts.retain(|f| f.get("itag").and_then(Value::as_u64).unwrap_or(u64::MAX) != itag);
        body.to_string()
    }

    #[test]
    fn pin_seen_then_capped_is_not_pinned_unavailable() {
        let mut h = Harness::new();
        let mut out = h.invoke(json!({ "source_ref": VID, "pin_itag": 251 }));
        // rung 0 lacks itag 251 -> advances; rungs 1-7 provide it but
        // every probe refuses -> capped weather, not a missing resource.
        out = feed(&mut h, &out, &ok_without_itag(251));
        assert_eq!(rung_of(&out), 1);
        for rung in 1..9usize {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 403, "");
            if rung < 8 {
                assert_eq!(rung_of(&out), rung + 1);
            }
        }
        // The capped mints are weather: each redraws on the second
        // edge — new mint, same refusal. Seventeen calls spent on
        // pass 0 leaves room inside the 32-call budget for seven of
        // the eight redraws (each needs its three-call chain).
        for _ in 1..8usize {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 403, "");
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "streams-capped".to_string())
        );
    }

    #[test]
    fn pin_unseen_everywhere_is_pinned_unavailable() {
        let mut h = Harness::new();
        let mut out = h.invoke(json!({ "source_ref": VID, "pin_itag": 251 }));
        // No rung carries itag 251 -> the requested pin is unavailable.
        for _ in 0..9 {
            out = feed(&mut h, &out, &ok_without_itag(251));
        }
        assert_eq!(
            fail_kind(&out),
            (
                "expired-resource".to_string(),
                "pinned-itag-unavailable".to_string()
            )
        );
    }

    #[test]
    fn malformed_persisted_visitors_never_reach_headers() {
        for (name, value) in [
            ("crlf", b"vis\r\nX-Inject: 1".as_slice()),
            ("space", b"vis itor".as_slice()),
            ("control", b"vis\x07itor".as_slice()),
            ("del", b"vis\x7fitor".as_slice()),
            ("oversized", vec![b'v'; 1025].as_slice()),
            ("non-utf8", vec![0xff, 0xfe].as_slice()),
        ] {
            let mut h = Harness::new();
            h.committed
                .insert("visitor/VISIONOS".into(), value.to_vec());
            let out = begin(&mut h);
            assert_eq!(header_of(&out, "X-Goog-Visitor-Id"), None, "{name}");
            assert!(
                h.logs.iter().any(|m| m.contains("malformed visitor")),
                "{name}"
            );
        }
    }

    #[test]
    fn boundary_length_visitor_replays() {
        let mut h = Harness::new();
        let visitor = "v".repeat(1024);
        h.committed
            .insert("visitor/VISIONOS".into(), visitor.clone().into_bytes());
        let out = begin(&mut h);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some(visitor.as_str())
        );
    }

    #[test]
    fn malformed_response_visitor_is_dropped() {
        let mut h = Harness::new();
        let mut body: Value = serde_json::from_str(SABR).unwrap_or_default();
        body["responseContext"] = json!({ "visitorData": "bad\r\nvisitor" });
        let out = begin(&mut h);
        let out = feed(&mut h, &out, &body.to_string());
        // Not replayed on rung 1, not staged for commit.
        assert_eq!(rung_of(&out), 1);
        assert_eq!(header_of(&out, "X-Goog-Visitor-Id"), None);
        assert!(h.logs.iter().any(|m| m.contains("malformed visitor")));
        assert!(!h.staged.contains_key("visitor/VISIONOS"));
    }

    #[test]
    fn malformed_backoff_values_are_ignored() {
        for (name, value) in [
            (
                "extra-key",
                json!({"until_ms": NOW + 60_000, "reason": "rate-limit", "x": 1}).to_string(),
            ),
            (
                "unknown-reason",
                json!({"until_ms": NOW + 60_000, "reason": "weird"}).to_string(),
            ),
            (
                "wrong-type",
                json!({"until_ms": "soon", "reason": "transport"}).to_string(),
            ),
            ("missing-key", json!({"until_ms": NOW + 60_000}).to_string()),
            ("not-json", "garbage".to_string()),
            ("not-object", "[1,2]".to_string()),
        ] {
            let mut h = Harness::new();
            h.committed
                .insert(format!("backoff/a/{VID}/VISIONOS"), value.into_bytes());
            // Ignored backoff -> rung 0 still gets its player call.
            let out = begin(&mut h);
            assert_eq!(rung_of(&out), 0, "{name}");
            assert!(
                h.logs.iter().any(|m| m.contains("malformed backoff")),
                "{name}"
            );
        }
    }

    #[test]
    fn backoff_staging_saturates_at_u64_max() {
        let mut h = Harness {
            now: u64::MAX,
            ..Harness::new()
        };
        let out = begin(&mut h);
        // 429 -> stage until_ms = u64::MAX (saturating, never wrapped).
        let out = h.answer(&out, 429, "{}");
        assert_eq!(rung_of(&out), 1);
        let staged: Value = serde_json::from_slice(
            h.staged
                .get(&format!("backoff/a/{VID}/VISIONOS"))
                .and_then(|v| v.as_deref())
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(staged["until_ms"], u64::MAX);
        assert_eq!(staged["reason"], "rate-limit");
    }

    // ---- Session trust (access_token) -------------------------------

    #[test]
    fn access_token_rides_authorization_header() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "source_ref": VID, "access_token": "tok-abc" }));
        assert_eq!(
            header_of(&out, "Authorization").as_deref(),
            Some("Bearer tok-abc")
        );
        let out = feed(&mut h, &out, SABR);
        // The token rides every rung while it stays valid.
        assert_eq!(
            header_of(&out, "Authorization").as_deref(),
            Some("Bearer tok-abc")
        );
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        // But never the googlevideo probe — Bearer is innertube-only.
        assert_eq!(header_of(&out, "Authorization"), None);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        // The token never reaches a guest log line.
        assert!(h.logs.iter().all(|m| !m.contains("tok-abc")));
    }

    #[test]
    fn absent_access_token_sends_no_authorization() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        assert_eq!(header_of(&out, "Authorization"), None);
    }

    #[test]
    fn empty_access_token_is_anonymous() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "source_ref": VID, "access_token": "" }));
        assert_eq!(header_of(&out, "Authorization"), None);
    }

    #[test]
    fn unauthorized_token_reasks_the_rung_bare() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "source_ref": VID, "access_token": "dead-tok" }));
        assert_eq!(
            header_of(&out, "Authorization").as_deref(),
            Some("Bearer dead-tok")
        );
        // rung 0 answers the authed request 401: the token is dead —
        // the SAME rung is re-asked bare once before any outcome is
        // recorded, so a stale token can't waste the rung it died on.
        let out = h.answer(&out, 401, "{}");
        assert_eq!(rung_of(&out), 0);
        assert_eq!(header_of(&out, "Authorization"), None);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "VISIONOS");
        // The 401 blamed the token, not the rung — no backoff was
        // staged against `backoff/<vid>/VISIONOS`.
        assert!(!h
            .committed
            .contains_key(&format!("backoff/a/{VID}/VISIONOS")));
    }

    #[test]
    fn unauthorized_bare_reask_refusal_is_the_rungs() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "source_ref": VID, "access_token": "dead-tok" }));
        // The authed request gets a 401 → bare re-ask of rung 0.
        let out = h.answer(&out, 401, "{}");
        assert_eq!(rung_of(&out), 0);
        assert_eq!(header_of(&out, "Authorization"), None);
        // The bare re-ask 401s too — that is the rung's own refusal:
        // it stages the transport backoff and the ladder advances,
        // still bare for every remaining rung.
        let out = h.answer(&out, 401, "{}");
        assert_eq!(rung_of(&out), 1);
        assert_eq!(header_of(&out, "Authorization"), None);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["client"], "ANDROID_VR@1.57.29");
        let stored: Value = serde_json::from_slice(
            h.committed
                .get(&format!("backoff/a/{VID}/VISIONOS"))
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(stored["reason"], "transport");
    }

    #[test]
    fn bare_unauthorized_still_stages_transport_backoff() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        // No token was sent, so this 401 is the rung's refusal — it
        // stages the transport backoff like any other failure.
        let out = h.answer(&out, 401, "{}");
        assert_eq!(rung_of(&out), 1);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        let stored: Value = serde_json::from_slice(
            h.committed
                .get(&format!("backoff/a/{VID}/VISIONOS"))
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(stored["reason"], "transport");
    }

    #[test]
    fn malformed_access_token_is_invalid_response() {
        for payload in [
            json!({ "source_ref": VID, "access_token": 42 }),
            json!({ "source_ref": VID, "access_token": "x".repeat(8193) }),
        ] {
            let mut h = Harness::new();
            let out = h.invoke(payload.clone());
            assert_eq!(fail_kind(&out).0, "invalid-response", "{payload}");
        }
    }

    #[test]
    fn failed_mint_writes_pot_aside_and_next_resolve_skips_the_call() {
        let mut h = Harness::new(); // Pot::Deny default
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 1);
        // The denied mint is remembered for the window.
        let aside = h.committed.get(POT_ASIDE_KEY).cloned().unwrap_or_default();
        let v: Value = serde_json::from_slice(&aside).unwrap_or_default();
        assert_eq!(v["until_ms"].as_u64(), Some(NOW + POT_ASIDE_MS));

        // A second resolve inside the window pays no mint call —
        // the aside short-circuits it before the provider is asked.
        let out = h.invoke(json!({ "source_ref": VID }));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 1);
        // A resolve that never minted leaves the aside in force.
        assert!(h.committed.contains_key(POT_ASIDE_KEY));

        // Past the window the provider earns a fresh probe.
        h.now = NOW + POT_ASIDE_MS + 1;
        let out = h.invoke(json!({ "source_ref": VID }));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 2);
    }

    #[test]
    fn expired_aside_retries_and_a_successful_mint_clears_it() {
        let mut h = Harness::new();
        h.committed.insert(
            POT_ASIDE_KEY.to_string(),
            serde_json::to_vec(&json!({ "until_ms": NOW - 1 })).unwrap_or_default(),
        );
        h.pot = Pot::Token("tok-abc");
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 1);
        assert!(!h.committed.contains_key(POT_ASIDE_KEY));
    }

    #[test]
    fn malformed_aside_record_is_ignored() {
        let mut h = Harness::new();
        h.committed
            .insert(POT_ASIDE_KEY.to_string(), b"{not json".to_vec());
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        // The foreign write wedges nothing: the provider still earns
        // its probe (and overwrites the record with a fresh aside).
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn extra_fields_make_the_aside_record_inert() {
        // A record carrying anything beyond `until_ms` is a foreign
        // write — it must not suppress mints.
        let mut h = Harness::new();
        h.committed.insert(
            POT_ASIDE_KEY.to_string(),
            serde_json::to_vec(&json!({
                "until_ms": NOW + POT_ASIDE_MS,
                "unexpected": true,
            }))
            .unwrap_or_default(),
        );
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 1);
    }

    #[test]
    fn video_refusal_marks_only_its_own_videos_aside() {
        let mut h = Harness::new();
        h.pot = Pot::Status(400);
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 1);
        // A 4xx can bind to the asked video — it books the scoped key
        // and never the global one.
        assert!(!h.committed.contains_key(POT_ASIDE_KEY));
        assert!(h.committed.contains_key(&format!("{POT_ASIDE_KEY}/{VID}")));

        // The same video inside the window skips its mint…
        let out = h.invoke(json!({ "source_ref": VID }));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 1);

        // …but another video's mint is unaffected — its own refusal
        // is the only thing that can suppress it.
        h.pot = Pot::Token("tok-2");
        let out = h.invoke(json!({ "source_ref": "othervideo1" }));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 2);

        // And the first video's aside still holds after a foreign
        // success — a mint clears only the scopes it can see.
        let out = h.invoke(json!({ "source_ref": VID }));
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = answer_probe_206(&mut h, &out);
        assert_eq!(out["type"], "done");
        assert_eq!(h.pot_calls, 2);
    }
}
