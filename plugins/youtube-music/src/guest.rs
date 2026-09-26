//! Capability dispatch plus `playback.resolve`: walk the pinned client
//! ladder until a rung yields plain audio, then decorate + probe the
//! minted stream URL before reporting it. Guest-side of ABI 0.3.0 over
//! the vendored SDK.
//!
//! Per-rung state lives in the host KV namespace: `visitor/<rung-key>`
//! replays that client's last `responseContext.visitorData`, and
//! `backoff/<video-id>/<rung-key>` skips a rung that recently failed.
//! `kv_set` stages writes the host commits only on `done` — a failed
//! resolve rolls its staged visitors/backoffs back by contract.

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
    append_pot, player_request, probe_request, LADDER, PROBE_FALLBACK_START, PROBE_TAIL_BYTES,
};

/// Backoff windows staged for a failed rung, keyed by reason.
const BOT_BACKOFF_MS: u64 = 45_000;
const RATE_LIMIT_BACKOFF_MS: u64 = 60_000;
const TRANSPORT_BACKOFF_MS: u64 = 5_000;
const CAPPED_BACKOFF_MS: u64 = 5_000;

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

/// Propagate terminal host errors; fold retryable ones into a rung
/// outcome. `cancelled`, `permission-denied`, and `invalid-response`
/// are terminal; `rate-limit` keeps its taxonomy (its own backoff
/// reason and fail kind); anything else is weather.
fn terminal_or_transport(e: GuestError) -> Result<RungOutcome, GuestError> {
    match e {
        GuestError::Host { kind, message } => match kind.as_str() {
            "cancelled" | "permission-denied" | "invalid-response" => {
                Err(GuestError::Host { kind, message })
            }
            "rate-limit" => Ok(RungOutcome::RateLimited),
            _ => Ok(RungOutcome::Transport),
        },
        other => Err(other),
    }
}

/// A sanitized warning — never carries bodies, URLs, query text, or
/// video ids.
pub(crate) async fn warn(message: &str) -> Result<(), GuestError> {
    log(LogLevel::Warn, message).await
}

/// Load a persisted visitor: nonempty visible-ASCII values only;
/// anything else is ignored with a sanitized warning and never reaches
/// a header.
pub(crate) async fn load_visitor(key: &str) -> Result<Option<String>, GuestError> {
    match kv_get(key).await? {
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

/// Load a stored backoff: the bytes must be exactly
/// `{until_ms: u64, reason: <known reason>}` — anything else is
/// ignored with a sanitized warning.
async fn load_backoff(key: &str) -> Result<Option<(u64, String)>, GuestError> {
    match kv_get(key).await? {
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
                Some(b) => Ok(Some(b)),
                None => {
                    warn("ignoring malformed backoff KV value").await?;
                    Ok(None)
                }
            }
        }
        None => Ok(None),
    }
}

async fn stage_backoff(key: &str, until_ms: u64, reason: &str) -> Result<(), GuestError> {
    let v =
        serde_json::to_vec(&json!({ "until_ms": until_ms, "reason": reason })).unwrap_or_default();
    kv_set(key, Some(&v)).await
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
                Some(decoded) => out.push(decoded),
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
fn truncated_bot_check(body: &[u8]) -> bool {
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
    let now = now_ms().await?;
    let mut outcomes: Vec<RungOutcome> = Vec::new();
    // Ladder indices that produced `Bot`, paired with the slot that
    // verdict occupies in `outcomes` — the attested pass replays only
    // the attestable ones and overwrites the slot, so a superseded
    // bot-check can't skew the ladder summary. Includes backoff-derived
    // entries: a staged bot-backoff is exactly what attestation is for.
    let mut bot_positions: Vec<(usize, usize)> = Vec::new();
    let mut pin_seen = false;
    let mut pin_missing = false;
    let mut mint_attempted = false;
    let mut pot: Option<String> = None;
    // The freshest visitorData seen this invocation — replayed on later
    // rungs ahead of their persisted KV value, matching the Slice 0
    // cross-rung propagation.
    let mut fresh_visitor: Option<String> = None;
    // The session-trust token rides every rung until a 401 proves it
    // dead — then the remaining ladder runs bare rather than failing
    // authenticated requests repeatedly.
    let mut access_token = p.access_token.clone();

    // Two passes over the ladder. Pass 1 runs bare — free on a clean
    // IP. Pass 2 replays only the bot-checked rungs with the shared
    // video-bound poToken in `serviceIntegrityDimensions`, the wall's
    // documented remedy; it fires only when a provider minted, so a
    // guest with no POT provider pays one locally-denied `pot_token`
    // call and keeps today's terminal behavior.
    for pass in 0..2u8 {
        let attested = pass == 1;
        for (i, rung) in LADDER.iter().enumerate() {
            // Pass 2 replays only rungs attestation can lift — a
            // non-attestable client's bot-check is permanent (its wall
            // needs DroidGuard, which a BotGuard mint never produces),
            // so replaying it would be a provably wasted request.
            if attested && (!rung.attestable || !bot_positions.iter().any(|(rung, _)| *rung == i)) {
                continue;
            }
            let visitor_key = format!("visitor/{}", rung.kv_key());
            let kv_visitor = load_visitor(&visitor_key).await?;
            let mut rung_visitor = fresh_visitor.clone().or(kv_visitor);

            let backoff_key = format!("backoff/{}/{}", p.video_id, rung.kv_key());
            // The attested pass deliberately ignores backoffs — a staged
            // bot-backoff is the thing attestation exists to break.
            if !attested {
                if let Some((until, reason)) = load_backoff(&backoff_key).await? {
                    if until > now {
                        let outcome = outcome_for_reason(&reason);
                        if outcome == RungOutcome::Bot {
                            bot_positions.push((i, outcomes.len()));
                        }
                        outcomes.push(outcome);
                        continue;
                    }
                }
            }

            let mut resp = match http_request(player_request(
                rung,
                &p.video_id,
                rung_visitor.as_deref(),
                if attested { pot.as_deref() } else { None },
                access_token.as_deref(),
            ))
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    let outcome = terminal_or_transport(e)?;
                    if let Some((reason, ms)) = backoff_for(outcome) {
                        stage_backoff(&backoff_key, now.saturating_add(ms), reason).await?;
                    }
                    record(&mut outcomes, attested, i, &bot_positions, outcome);
                    continue;
                }
            };
            // A 401 against a request that carried the token proves the
            // token dead, not the rung: drop it for the rest of the
            // ladder and re-ask this rung bare once before recording an
            // outcome — `take` makes the re-ask single-shot, and the
            // retried response flows through the same status handling
            // below, so a bare 401 is then the rung's own refusal.
            if resp.status == 401 && access_token.take().is_some() {
                resp = match http_request(player_request(
                    rung,
                    &p.video_id,
                    rung_visitor.as_deref(),
                    if attested { pot.as_deref() } else { None },
                    None,
                ))
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let outcome = terminal_or_transport(e)?;
                        if let Some((reason, ms)) = backoff_for(outcome) {
                            stage_backoff(&backoff_key, now.saturating_add(ms), reason).await?;
                        }
                        record(&mut outcomes, attested, i, &bot_positions, outcome);
                        continue;
                    }
                };
            }
            if !(200..300).contains(&resp.status) {
                // A flagged IP's wall arrives as a bare 403 — Google's
                // abuse edge answers with an HTML "automated queries"
                // interstitial, never a JSON body. That is the bot
                // wall in transport form: the attested replay is its
                // remedy, so it books exactly like a body-classified
                // BotCheck (position, backoff, replay). A 403 that
                // does carry a JSON envelope is classified by its
                // playabilityStatus — a bot-check inside is still a
                // wall; anything else is an API-level refusal for that
                // client, not the edge. A body shaped like JSON that
                // fails to parse (a truncated refusal) books Transport
                // — unless its surviving prefix still carries a
                // bot-check marker, which is the wall truncated, not a
                // refusal. Only a non-JSON body books Bot outright.
                let outcome = if resp.status == 403 {
                    // A UTF-8 BOM precedes some stacks' JSON emitters —
                    // strip it or a valid envelope looks non-JSON and
                    // books Bot on shape alone.
                    let body = resp
                        .body
                        .strip_prefix(b"\xEF\xBB\xBF")
                        .unwrap_or(&resp.body);
                    let looks_json = body
                        .iter()
                        .find(|b| !b.is_ascii_whitespace())
                        .is_some_and(|b| *b == b'{' || *b == b'[');
                    match (
                        looks_json,
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
                };
                // A 401 that carried the token was already re-asked
                // bare above — every refusal reaching here is the
                // rung's own and stages its backoff.
                if let Some((reason, ms)) = backoff_for(outcome) {
                    stage_backoff(&backoff_key, now.saturating_add(ms), reason).await?;
                }
                record(&mut outcomes, attested, i, &bot_positions, outcome);
                if !attested && outcome == RungOutcome::Bot {
                    bot_positions.push((i, outcomes.len() - 1));
                }
                continue;
            }
            // A 2xx player response must be a JSON envelope carrying
            // `playabilityStatus.status` — anything else (an HTML
            // interstitial, an empty body, a shape the parser predates) is
            // upstream breakage, not a rung outcome.
            let body: Value = serde_json::from_slice(&resp.body).map_err(|_| {
                failed(
                    "invalid-response",
                    "player response body is not JSON".into(),
                )
            })?;
            if body
                .pointer("/playabilityStatus/status")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(failed(
                    "invalid-response",
                    "player response lacks playabilityStatus".into(),
                ));
            }
            if let Some(raw) = visitor_data(&body) {
                if let Some(visitor) = visitor_token(&raw) {
                    let visitor = visitor.to_string();
                    kv_set(&visitor_key, Some(visitor.as_bytes())).await?;
                    rung_visitor = Some(visitor.clone());
                    fresh_visitor = Some(visitor);
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
                record(
                    &mut outcomes,
                    attested,
                    i,
                    &bot_positions,
                    RungOutcome::Unavailable,
                );
                continue;
            }
            match classify_playability(&body).0 {
                Playability::Ok => {}
                Playability::BotCheck => {
                    stage_backoff(
                        &backoff_key,
                        now.saturating_add(BOT_BACKOFF_MS),
                        "bot-check",
                    )
                    .await?;
                    record(&mut outcomes, attested, i, &bot_positions, RungOutcome::Bot);
                    if !attested {
                        bot_positions.push((i, outcomes.len() - 1));
                    }
                    continue;
                }
                Playability::AgeRestricted => {
                    record(&mut outcomes, attested, i, &bot_positions, RungOutcome::Age);
                    continue;
                }
                Playability::SignInRequired => {
                    record(
                        &mut outcomes,
                        attested,
                        i,
                        &bot_positions,
                        RungOutcome::SignIn,
                    );
                    continue;
                }
                Playability::Unavailable => {
                    record(
                        &mut outcomes,
                        attested,
                        i,
                        &bot_positions,
                        RungOutcome::Unavailable,
                    );
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
                            )
                            .await?
                            {
                                PickOutcome::Done(result) => {
                                    kv_set(&backoff_key, None).await?;
                                    return Ok(result);
                                }
                                PickOutcome::Advance(outcome) => {
                                    if let Some((reason, ms)) = backoff_for(outcome) {
                                        stage_backoff(&backoff_key, now.saturating_add(ms), reason)
                                            .await?;
                                    }
                                    record(&mut outcomes, attested, i, &bot_positions, outcome);
                                }
                            }
                        }
                        None => {
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
                                    },
                                )
                                .is_some()
                            {
                                pin_missing = true;
                            }
                            record(
                                &mut outcomes,
                                attested,
                                i,
                                &bot_positions,
                                RungOutcome::NoAudio,
                            );
                        }
                    }
                }
                FormatOutcome::SabrOnly => record(
                    &mut outcomes,
                    attested,
                    i,
                    &bot_positions,
                    RungOutcome::SabrOnly,
                ),
                FormatOutcome::CipheredOnly => record(
                    &mut outcomes,
                    attested,
                    i,
                    &bot_positions,
                    RungOutcome::CipheredOnly,
                ),
                FormatOutcome::NoAudio => record(
                    &mut outcomes,
                    attested,
                    i,
                    &bot_positions,
                    RungOutcome::NoAudio,
                ),
            }
        }
        // Escalation: bot-checks accumulate a remedy pass. One shared
        // mint — the same video-bound token also decorates picked URLs
        // via `finish_pick`. A denied/failed mint keeps today's
        // terminal outcome.
        if attested
            || !bot_positions
                .iter()
                .any(|(rung, _)| LADDER[*rung].attestable)
        {
            break;
        }
        if !mint_once(&mut mint_attempted, &mut pot, &p.video_id).await? {
            break;
        }
    }
    Err(ladder_error(&outcomes, pin_missing, pin_seen))
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
    match pot_token(video_id).await {
        Ok(r) if r.status == 200 => {
            *pot = serde_json::from_slice::<Value>(&r.body).ok().and_then(|j| {
                ["poToken", "po_token", "token"]
                    .iter()
                    .find_map(|key| j.get(key).and_then(Value::as_str))
                    .filter(|token| !token.is_empty())
                    .map(str::to_string)
            });
            Ok(pot.is_some())
        }
        Ok(_) => Ok(false),
        Err(GuestError::Host { kind, message }) => {
            if kind == "cancelled" {
                Err(GuestError::Host { kind, message })
            } else {
                // unsupported / permission-denied / transient mint
                // failures degrade to no token.
                Ok(false)
            }
        }
        Err(e) => Err(e),
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
) -> Result<PickOutcome, GuestError> {
    let _ = mint_once(mint_attempted, pot, video_id).await?;
    if let Some(token) = pot.as_deref() {
        picked.url = append_pot(&picked.url, token);
    }
    let mut resp = match http_request(probe_request(rung, &picked.url, picked.content_length)).await
    {
        Ok(r) => r,
        Err(e) => return Ok(PickOutcome::Advance(terminal_or_transport(e)?)),
    };
    // The host never follows redirects — destination policy is enforced
    // on each request — so a 3xx is re-requested through the normal
    // authorized path. googlevideo edge-balances minted URLs this way;
    // the verdict runs on wherever the chain lands. One hop: a second
    // redirect is serving weather, not a chain to chase.
    if resp.status / 100 == 3 {
        let target = header_value(&resp.headers, "location").map(str::to_owned);
        if let Some(target) = target.filter(|t| t.starts_with("https://")) {
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
                None => reached_eof || end == start_asked + PROBE_TAIL_BYTES - 1,
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

/// Append `outcome` — or, on the attested pass, overwrite the slot the
/// rung's bare `Bot` verdict occupied. The replay's verdict is the
/// rung's real answer: keeping the superseded `Bot` would report a
/// bot-check a successful attestation already answered.
fn record(
    outcomes: &mut Vec<RungOutcome>,
    attested: bool,
    rung: usize,
    bot_positions: &[(usize, usize)],
    outcome: RungOutcome,
) {
    if attested {
        if let Some(&(_, slot)) = bot_positions.iter().find(|(r, _)| *r == rung) {
            outcomes[slot] = outcome;
            return;
        }
    }
    outcomes.push(outcome);
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
        RungOutcome::Bot => failed("transient", "bot-check".into()),
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
            (Some("3"), Some("19.09.37")) => 4,
            (Some("28"), Some("1.61.48")) => 5,
            (Some("28"), Some("1.60.19")) => 6,
            (Some("28"), Some("1.43.32")) => 7,
            other => panic!("unexpected rung headers {other:?} in {out}"),
        }
    }

    /// Assert `out` is a GET tail probe for the OK fixture's pick
    /// (`contentLength` 4,557,665 → last 64 KiB) and return `out`.
    fn probe_of(out: &Value) {
        assert_eq!(out["type"], "host_request");
        assert_eq!(out["payload"]["method"], "GET");
        assert_eq!(
            header_of(out, "Range").as_deref(),
            Some("bytes=4492129-4557664")
        );
    }

    /// The honest 206 for the OK fixture's pick: tail
    /// `4492129-4557664`, span 64 KiB.
    fn answer_probe_206(h: &mut Harness, out: &Value) -> Value {
        h.answer_headers(
            out,
            206,
            &[("Content-Range", "bytes 4492129-4557664/4557665")],
            65536,
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
    }

    #[test]
    fn last_rung_success_reports_client() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for rung in 1..=7usize {
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
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 4492129-4557663/4557665")],
            65535,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_206_truncated_body_is_capped() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = feed(&mut h, &out, OK);
        probe_of(&out);
        let out = h.answer_headers(
            &out,
            206,
            &[("Content-Range", "bytes 4492129-4557664/4557665")],
            1024,
        );
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_429_reports_rate_limit() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..8 {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
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
        for _ in 0..8 {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 503, "");
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "transport".to_string())
        );
    }

    #[test]
    fn player_body_not_json_is_invalid_response() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer(&out, 200, "<html>oops</html>");
        assert_eq!(fail_kind(&out).0, "invalid-response");
    }

    #[test]
    fn player_missing_playability_status_is_invalid_response() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        let out = h.answer(&out, 200, &json!({ "videoDetails": {} }).to_string());
        assert_eq!(fail_kind(&out).0, "invalid-response");
    }

    #[test]
    fn all_capped_fails_streams_capped() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..8 {
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
        // No early exit: the bare pass walks every rung, so a fully
        // walled IP costs the whole ladder before failing.
        for _ in 0..LADDER.len() {
            out = feed(&mut h, &out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "bot-check".to_string())
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
        // now → kv(visitor) → kv(backoff) → player.
        for _ in 0..3 {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            out = match out["kind"].as_str().unwrap_or("") {
                "now_ms" => step(&json!({"type":"now_response","id":id,"now_ms":NOW})),
                "kv_get" => step(&json!({"type":"kv_response","id":id,"value":null})),
                other => panic!("unexpected kind {other}"),
            };
        }
        assert_eq!(out["kind"], "http_request");
        let id = out["id"].as_u64().unwrap_or(u64::MAX);
        let out = step(&http_response(id, 200, OK));
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
        for _ in 0..5 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        // The mint fires and pass 2 replays rung 0 attested (rung 1's
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
        assert!(!h.committed.contains_key(&format!("backoff/{VID}/VISIONOS")));
        assert!(h.committed.contains_key(&format!("backoff/{VID}/IOS")));
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
        // Two truncated refusals must not trip the 2-strike bot
        // budget — the ladder continues bare to rung 2.
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
        for _ in 0..5 {
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
        for _ in 0..5 {
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
        for _ in 0..5 {
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
        for _ in 0..5 {
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
        for _ in 0..5 {
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
        assert!(!h.committed.contains_key(&format!("backoff/{VID}/VISIONOS")));
        assert!(h.committed.contains_key(&format!("backoff/{VID}/IOS")));
    }

    #[test]
    fn non_attestable_bot_checks_never_starve_later_rungs() {
        // A bot-check on a rung attestation cannot lift proves nothing
        // about later clients — it must not spend the bare budget.
        // SABR on the Apple rungs + walls on the plain-UA ANDROID_VR
        // rungs still leaves the Oculus pin reachable.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = feed(&mut h, &out, SABR); // VISIONOS
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.57.29 — no budget spend
        out = feed(&mut h, &out, SABR); // IOS
        out = feed(&mut h, &out, BOT); // ANDROID_VR@1.61.29 — no budget spend
        out = feed(&mut h, &out, UNPLAYABLE); // ANDROID
        assert_eq!(rung_of(&out), 5);
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
        for _ in 0..7 {
            out = feed(&mut h, &out, BOT);
        }
        // Attested replay of rung 0 then rung 2 (non-attestable walls
        // need DroidGuard — never replayed) — still walled.
        assert_eq!(rung_of(&out), 0);
        let out = feed(&mut h, &out, BOT);
        assert_eq!(rung_of(&out), 2);
        let out = feed(&mut h, &out, BOT);
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "bot-check".to_string())
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
        for _ in 0..5 {
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
        // No POT provider: the second bare bot-check still ends the
        // resolve with the same typed failure — one locally-denied
        // `pot_token` call is the only added cost.
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..LADDER.len() {
            out = feed(&mut h, &out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "bot-check".to_string())
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
            format!("backoff/{VID}/VISIONOS"),
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
        for _ in 0..5 {
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
        assert!(!h.committed.contains_key(&format!("backoff/{VID}/VISIONOS")));
    }

    #[test]
    fn all_sabr_fails_unsupported_sabr() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..8 {
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
        for _ in 0..8 {
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
        for _ in 0..7 {
            out = feed(&mut h, &out, SABR);
        }
        // The single bare bot-check does not end the pass, so the
        // ladder ran to exhaustion; the denied mint ends attestation.
        assert_eq!(h.pot_calls, 1);
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "bot-check".to_string())
        );
    }

    #[test]
    fn transport_mixed_with_sabr_is_transient_not_unsupported() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        out = h.answer(&out, 500, "{}");
        for _ in 0..7 {
            out = feed(&mut h, &out, SABR);
        }
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
        for _ in 0..6 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(
            fail_kind(&out),
            ("rate-limit".to_string(), "rate-limit".to_string())
        );
    }

    #[test]
    fn unavailable_ladder_fails_no_result() {
        let mut h = Harness::new();
        let mut out = begin(&mut h);
        for _ in 0..8 {
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
                .get(&format!("backoff/{VID}/VISIONOS"))
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
        for _ in 0..8 {
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
        for _ in 0..8 {
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
        // The staged write committed under rung 0's key.
        assert_eq!(
            h.committed.get("visitor/VISIONOS").map(Vec::as_slice),
            Some(b"Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D".as_slice())
        );
        // A fresh invocation reads it back through KV.
        let out = begin(&mut h);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D")
        );
    }

    #[test]
    fn stored_backoff_skips_rung_without_player_call() {
        let mut h = Harness::new();
        h.committed.insert(
            format!("backoff/{VID}/VISIONOS"),
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
            "ANDROID",
            "ANDROID_VR@1.61.48",
            "ANDROID_VR@1.60.19",
            "ANDROID_VR@1.43.32",
        ] {
            h.committed.insert(
                format!("backoff/{VID}/{rung}"),
                json!({ "until_ms": NOW + 60_000, "reason": "rate-limit" })
                    .to_string()
                    .into_bytes(),
            );
        }
        // Every rung skipped -> fail without a single HTTP call.
        let out = h.invoke(json!({ "source_ref": VID }));
        assert_eq!(
            fail_kind(&out),
            ("rate-limit".to_string(), "rate-limit".to_string())
        );
    }

    #[test]
    fn expired_backoff_does_not_skip() {
        let mut h = Harness::new();
        h.committed.insert(
            format!("backoff/{VID}/VISIONOS"),
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
        let Some(stored) = h.committed.get(&format!("backoff/{VID}/VISIONOS")) else {
            panic!("rung-0 backoff must commit on done");
        };
        let stored: Value = serde_json::from_slice(stored).unwrap_or_default();
        assert_eq!(stored["reason"], "rate-limit");
        assert_eq!(stored["until_ms"], NOW + 60_000);
        // The successful rung's own backoff key was cleared.
        assert!(!h.committed.contains_key(&format!("backoff/{VID}/IOS")));
    }

    #[test]
    fn all_failed_rolls_back_staged_backoff() {
        let mut h = Harness::new();
        let out = begin(&mut h);
        // rung 0: 429 stages a backoff; the rest of the ladder serves
        // unplayable -> `fail` discards the staged write by contract.
        let mut out = h.answer(&out, 429, "{}");
        for _ in 0..7 {
            out = feed(&mut h, &out, UNPLAYABLE);
        }
        assert_eq!(fail_kind(&out).0, "rate-limit");
        assert!(!h.committed.contains_key(&format!("backoff/{VID}/VISIONOS")));
        assert!(h.staged.is_empty());
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
        for _ in 0..8 {
            out = h.answer_host_error(&out, "transient");
        }
        assert_eq!(fail_kind(&out).0, "transient");
    }

    #[test]
    fn pinned_resolve_preserves_uninspectable_player_outcomes() {
        for (body, attempts, expected) in [
            (BOT, 8, "transient"),
            (
                r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED"}}"#,
                8,
                "auth-required",
            ),
            (SABR, 8, "unsupported"),
            (CIPHERED, 8, "unsupported"),
            (UNPLAYABLE, 8, "no-result"),
            (
                r#"{"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[{"itag":140,"mimeType":"audio/mp4","url":"http://example.test/audio"}]}}"#,
                8,
                "no-result",
            ),
        ] {
            let mut h = Harness::new();
            let mut out = h.invoke(json!({ "source_ref": VID, "pin_itag": 251 }));
            for _ in 0..attempts {
                out = feed(&mut h, &out, body);
            }
            assert_eq!(fail_kind(&out).0, expected, "{body}");
        }
    }

    #[test]
    fn pin_itag_missing_everywhere_is_expired_resource() {
        let mut h = Harness::new();
        let mut out = h.invoke(json!({ "source_ref": VID, "pin_itag": 774 }));
        for _ in 0..8 {
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
        for rung in 1..8usize {
            out = feed(&mut h, &out, OK);
            probe_of(&out);
            out = h.answer(&out, 403, "");
            if rung < 7 {
                assert_eq!(rung_of(&out), rung + 1);
            }
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
        for _ in 0..8 {
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
                .insert(format!("backoff/{VID}/VISIONOS"), value.into_bytes());
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
                .get(&format!("backoff/{VID}/VISIONOS"))
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
        assert!(!h.committed.contains_key(&format!("backoff/{VID}/VISIONOS")));
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
                .get(&format!("backoff/{VID}/VISIONOS"))
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
                .get(&format!("backoff/{VID}/VISIONOS"))
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
}
