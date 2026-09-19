//! The ABI step machine: `invoke` starts the ladder; each
//! `http_response`/`host_error` advances it until a rung yields plain
//! audio or the ladder is exhausted.

use base64::Engine as _;
use core::cell::RefCell;
use serde_json::{json, Value};

use crate::parse::{
    classify_playability, format_outcome, pick_audio, visitor_data, FormatOutcome, Picked,
    Playability,
};
use crate::rungs::{append_pot, player_request, pot_mint_request, probe_request, LADDER};

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

/// A picked format awaiting its boundary-probe verdict.
struct PendingProbe {
    request_id: u32,
    picked: Picked,
}

/// A picked format paused while a `pot_token` mint is in flight. The
/// mint is lazy — issued when the first rung yields a stream URL — so
/// the token can bind to the visitor the ladder already collected.
struct PendingMint {
    request_id: u32,
    picked: Picked,
}

struct State {
    video_id: String,
    rung: usize,
    next_id: u32,
    /// `responseContext.visitorData` from the most recent rung response;
    /// invocation-scoped, never persisted.
    visitor_id: Option<String>,
    /// Minted PO token for this resolve, once the mint step answered.
    pot_token: Option<String>,
    /// A mint was already attempted this resolve — at most one.
    mint_attempted: bool,
    /// `pot_token` mint in flight, holding the picked URL it decorates.
    pending_mint: Option<PendingMint>,
    outcomes: Vec<RungOutcome>,
    /// Probe in flight for a candidate URL, if any.
    pending_probe: Option<PendingProbe>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// One ABI step: consume the step message bytes, produce the response
/// message bytes.
pub fn step(input: &[u8]) -> Vec<u8> {
    let msg: Value = match serde_json::from_slice(input) {
        Ok(v) => v,
        Err(_) => return fail("invalid-message", "step input is not JSON"),
    };
    match msg.get("type").and_then(Value::as_str) {
        Some("invoke") => on_invoke(&msg),
        Some("http_response") | Some("host_error") => STATE.with(|s| {
            let mut s = s.borrow_mut();
            match s.as_mut() {
                Some(state) => on_http_step(&msg, state),
                None => fail("invalid-message", "http step before invoke"),
            }
        }),
        _ => fail("invalid-message", "unknown step message type"),
    }
}

fn on_invoke(msg: &Value) -> Vec<u8> {
    let capability = msg.get("capability").and_then(Value::as_str).unwrap_or("");
    if capability != "playback.resolve" {
        return fail("not-applicable", "unsupported capability");
    }
    let video_id = msg
        .get("payload")
        .and_then(|p| p.get("source_ref"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if video_id.is_empty() {
        return fail("invalid-response", "missing source_ref");
    }
    let mut state = State {
        video_id,
        rung: 0,
        next_id: 1,
        visitor_id: None,
        pot_token: None,
        mint_attempted: false,
        pending_mint: None,
        outcomes: Vec::new(),
        pending_probe: None,
    };
    let req = issue_request(&mut state);
    STATE.with(|s| *s.borrow_mut() = Some(state));
    req
}

/// Emit the current rung's player request, or the terminal `fail` when
/// the ladder is exhausted.
fn issue_request(state: &mut State) -> Vec<u8> {
    let Some(rung) = LADDER.get(state.rung) else {
        return ladder_failed(&state.outcomes);
    };
    let id = state.next_id;
    state.next_id += 1;
    player_request(rung, &state.video_id, id, state.visitor_id.as_deref())
}

/// The mint verdict: a 200 body carrying `poToken` arms stream-URL
/// decoration; `host_error` or a non-200 degrades to the bare URL, and
/// a mismatched response id is a protocol violation. Otherwise the
/// paused pick goes to its tail probe.
fn on_mint_step(msg: &Value, mint: PendingMint, state: &mut State) -> Vec<u8> {
    if msg.get("type").and_then(Value::as_str) == Some("host_error") {
        return issue_probe(state, mint.picked);
    }
    if msg.get("id").and_then(Value::as_u64) != Some(u64::from(mint.request_id)) {
        return fail("invalid-message", "mint response id mismatch");
    }
    if msg.get("status").and_then(Value::as_u64) == Some(200) {
        state.pot_token = msg
            .get("body")
            .and_then(Value::as_str)
            .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|json| {
                ["poToken", "po_token", "token"]
                    .iter()
                    .find_map(|key| json.get(key).and_then(Value::as_str))
                    .filter(|token| !token.is_empty())
                    .map(str::to_string)
            });
    }
    issue_probe(state, mint.picked)
}

/// Advance to the next rung, or fail if the ladder is done.
fn advance(state: &mut State, outcome: RungOutcome) -> Vec<u8> {
    state.outcomes.push(outcome);
    state.rung += 1;
    issue_request(state)
}

fn on_http_step(msg: &Value, state: &mut State) -> Vec<u8> {
    // Mint and probe are lock-step: while either is pending, this step
    // is that request's verdict, not a player response.
    if let Some(mint) = state.pending_mint.take() {
        return on_mint_step(msg, mint, state);
    }
    if let Some(probe) = state.pending_probe.take() {
        return on_probe_step(msg, probe, state);
    }
    if msg.get("type").and_then(Value::as_str) == Some("host_error") {
        return advance(state, RungOutcome::Transport);
    }
    let status = msg.get("status").and_then(Value::as_u64).unwrap_or(0);
    if !(200..300).contains(&status) {
        return advance(
            state,
            if status == 429 {
                RungOutcome::RateLimited
            } else {
                RungOutcome::Transport
            },
        );
    }
    let body: Value = msg
        .get("body")
        .and_then(Value::as_str)
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    if body.is_null() {
        return advance(state, RungOutcome::Transport);
    }
    if let Some(visitor) = visitor_data(&body) {
        state.visitor_id = Some(visitor);
    }
    match classify_playability(&body) {
        (Playability::Ok, _) => on_ok_rung(&body, state),
        (Playability::BotCheck, _) => advance(state, RungOutcome::Bot),
        (Playability::AgeRestricted, _) => advance(state, RungOutcome::Age),
        (Playability::SignInRequired, _) => advance(state, RungOutcome::SignIn),
        (Playability::Unavailable, _) => advance(state, RungOutcome::Unavailable),
    }
}

fn on_ok_rung(body: &Value, state: &mut State) -> Vec<u8> {
    match format_outcome(body) {
        FormatOutcome::PlainAudio => match pick_audio(body) {
            Some(picked) => issue_probe_or_mint(state, picked),
            None => advance(state, RungOutcome::NoAudio),
        },
        FormatOutcome::SabrOnly => advance(state, RungOutcome::SabrOnly),
        FormatOutcome::CipheredOnly => advance(state, RungOutcome::CipheredOnly),
        FormatOutcome::NoAudio => advance(state, RungOutcome::NoAudio),
    }
}

/// A candidate URL is decorated with `pot=` before it is probed. The
/// mint is lazy — it fires once, on the first pick of the resolve, bound
/// to the visitor the ladder collected (the video id stands in when no
/// rung yielded one). No provider configured -> `host_error` -> the URL
/// is probed bare.
fn issue_probe_or_mint(state: &mut State, picked: Picked) -> Vec<u8> {
    if state.mint_attempted {
        return issue_probe(state, picked);
    }
    state.mint_attempted = true;
    let binding = state
        .visitor_id
        .clone()
        .unwrap_or_else(|| state.video_id.clone());
    let id = state.next_id;
    state.next_id += 1;
    state.pending_mint = Some(PendingMint {
        request_id: id,
        picked,
    });
    pot_mint_request(&binding, id)
}

/// A candidate URL is verified before it is returned: probe the file's
/// tail so a capped mint advances the ladder instead of handing the app
/// a URL that cannot serve the track.
fn issue_probe(state: &mut State, picked: Picked) -> Vec<u8> {
    let Some(rung) = LADDER.get(state.rung) else {
        return fail("internal", "probe without rung");
    };
    let id = state.next_id;
    state.next_id += 1;
    // The probe must hit the URL the downloader will fetch: decorated
    // with `pot=` when the mint succeeded. The decorated URL is also
    // what `done` reports.
    let mut picked = picked;
    if let Some(token) = &state.pot_token {
        picked.url = append_pot(&picked.url, token);
    }
    let req = probe_request(rung, &picked.url, id, picked.content_length);
    state.pending_probe = Some(PendingProbe {
        request_id: id,
        picked,
    });
    req
}

/// The probe verdict: 206 serves the file's tail, 416 is a defensive
/// pass (a tail probe can't legitimately 416) — both return the picked
/// resource. Anything else marks the mint capped and advances the
/// ladder. A probe transport failure is `Transport`, not `Capped`:
/// nothing about serving was learned.
fn on_probe_step(msg: &Value, probe: PendingProbe, state: &mut State) -> Vec<u8> {
    if msg.get("type").and_then(Value::as_str) == Some("host_error") {
        return advance(state, RungOutcome::Transport);
    }
    if msg.get("id").and_then(Value::as_u64) != Some(u64::from(probe.request_id)) {
        return fail("invalid-message", "probe response id mismatch");
    }
    match msg.get("status").and_then(Value::as_u64).unwrap_or(0) {
        206 | 416 => {
            let rung = LADDER.get(state.rung);
            let picked = probe.picked;
            done(&json!({
                "url": picked.url,
                "mime": picked.mime,
                "bitrate_kbps": picked.bitrate_kbps,
                "expires_at_ms": picked.expires_at_ms,
                "content_length": picked.content_length,
                "client": rung.map_or("unknown", |r| r.name),
            }))
        }
        _ => advance(state, RungOutcome::Capped),
    }
}

/// Map the accumulated outcomes to a taxonomy `fail`.
fn ladder_failed(outcomes: &[RungOutcome]) -> Vec<u8> {
    if outcomes.contains(&RungOutcome::RateLimited) {
        return fail("rate-limit", "rate-limit");
    }
    let ok_outcomes: Vec<RungOutcome> = outcomes
        .iter()
        .copied()
        .filter(|o| {
            matches!(
                o,
                RungOutcome::SabrOnly | RungOutcome::CipheredOnly | RungOutcome::NoAudio
            )
        })
        .collect();
    if !ok_outcomes.is_empty()
        && ok_outcomes
            .iter()
            .all(|o| matches!(o, RungOutcome::SabrOnly | RungOutcome::CipheredOnly))
    {
        return fail(
            "unsupported",
            if ok_outcomes[0] == RungOutcome::SabrOnly {
                "sabr-only"
            } else {
                "ciphered-only"
            },
        );
    }
    // Every rung resolved but every mint refused the boundary probe:
    // provider serving is restricted right now — retryable weather.
    if !outcomes.is_empty() && outcomes.iter().all(|o| *o == RungOutcome::Capped) {
        return fail("transient", "streams-capped");
    }
    match outcomes.last().copied().unwrap_or(RungOutcome::Transport) {
        RungOutcome::Bot => fail("transient", "bot-check"),
        RungOutcome::SignIn | RungOutcome::Age => fail("auth-required", "sign-in-required"),
        RungOutcome::Unavailable | RungOutcome::NoAudio => fail("no-result", "unavailable"),
        RungOutcome::SabrOnly => fail("unsupported", "sabr-only"),
        RungOutcome::CipheredOnly => fail("unsupported", "ciphered-only"),
        RungOutcome::Capped => fail("transient", "streams-capped"),
        RungOutcome::RateLimited | RungOutcome::Transport => fail("transient", "transport"),
    }
}

fn done(result: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({ "type": "done", "result": result })).unwrap_or_default()
}

fn fail(kind: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "type": "fail",
        "error": { "kind": kind, "message": message },
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = include_str!("../fixtures/player-ok-plain-urls.json");
    const SABR: &str = include_str!("../fixtures/player-sabr-only.json");
    const BOT: &str = include_str!("../fixtures/player-bot-check.json");
    const CIPHERED: &str = include_str!("../fixtures/player-ciphered-only.json");
    const UNPLAYABLE: &str = include_str!("../fixtures/player-unplayable.json");

    fn invoke_msg(video_id: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": "invoke",
            "request_id": "t1",
            "capability": "playback.resolve",
            "payload": { "source_ref": video_id },
        }))
        .unwrap_or_default()
    }

    fn http_response(id: u32, status: u16, body: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": "http_response",
            "id": id,
            "status": status,
            "headers": [],
            "body": base64::engine::general_purpose::STANDARD.encode(body),
        }))
        .unwrap_or_default()
    }

    fn host_error(id: u32) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": "host_error",
            "id": id,
            "error": { "kind": "permission-denied", "message": "no provider" },
        }))
        .unwrap_or_default()
    }

    fn parse(out: &[u8]) -> Value {
        match serde_json::from_slice(out) {
            Ok(v) => v,
            Err(e) => panic!("guest output is not JSON: {e}"),
        }
    }

    /// Invoke and return the rung-0 player request.
    fn begin(video_id: &str) -> Vec<u8> {
        let out = step(&invoke_msg(video_id));
        assert_eq!(rung_of(&out), 0);
        out
    }

    fn req_id_of(out: &[u8]) -> u32 {
        u32::try_from(parse(out)["id"].as_u64().unwrap_or(0)).unwrap_or(0)
    }

    /// `Some(id)` when `out` is a `pot_token` mint request.
    fn mint_id_of(out: &[u8]) -> Option<u32> {
        let msg = parse(out);
        (msg["type"] == "host_request" && msg["kind"] == "pot_token").then(|| req_id_of(out))
    }

    /// Feed `body` (status 200) to the pending request in `out`; if a
    /// mint follows, deny it as the host would without a provider, and
    /// return the next emitted request.
    fn feed(out: &[u8], body: &str) -> Vec<u8> {
        let mut next = step(&http_response(req_id_of(out), 200, body));
        if let Some(mint_id) = mint_id_of(&next) {
            next = step(&host_error(mint_id));
        }
        next
    }

    /// The rung index a `host_request` is for, read off its
    /// `X-YouTube-Client-Name` + version headers. Only meaningful for
    /// POST (player) requests — probes are GET and carry no client
    /// headers.
    fn rung_of(out: &[u8]) -> usize {
        let msg = parse(out);
        assert_eq!(msg["type"], "host_request");
        assert_eq!(msg["payload"]["method"], "POST");
        match (
            header_of(out, "X-YouTube-Client-Name").as_deref(),
            header_of(out, "X-YouTube-Client-Version").as_deref(),
        ) {
            (Some("101"), Some("1.02")) => 0,
            (Some("5"), Some("20.10.4")) => 1,
            (Some("28"), Some("1.61.48")) => 2,
            (Some("28"), Some("1.60.19")) => 3,
            (Some("28"), Some("1.43.32")) => 4,
            other => panic!("unexpected rung headers {other:?} in {msg}"),
        }
    }

    /// Assert `out` is a GET tail probe and return its id. The fixture's
    /// picked format reports contentLength 4557665, so the probe asks
    /// for its last 64 KiB.
    fn probe_of(out: &[u8]) -> u32 {
        let msg = parse(out);
        assert_eq!(msg["type"], "host_request");
        assert_eq!(msg["payload"]["method"], "GET");
        assert_eq!(
            header_of(out, "Range").as_deref(),
            Some("bytes=4492129-4557664")
        );
        req_id_of(out)
    }

    fn header_of(out: &[u8], name: &str) -> Option<String> {
        let msg = parse(out);
        msg["payload"]["headers"]
            .as_array()?
            .iter()
            .find(|h| h[0].as_str() == Some(name))
            .and_then(|h| h[1].as_str().map(str::to_string))
    }

    /// The `url` a `host_request` targets.
    fn url_of(out: &[u8]) -> String {
        parse(out)["payload"]["url"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn fail_kind(out: &[u8]) -> (String, String) {
        let msg = parse(out);
        assert_eq!(msg["type"], "fail");
        (
            msg["error"]["kind"].as_str().unwrap_or("").to_string(),
            msg["error"]["message"].as_str().unwrap_or("").to_string(),
        )
    }

    #[test]
    fn sabr_then_plain_succeeds_on_rung_two() {
        let out = begin("vid12345678");
        assert_eq!(
            url_of(&out),
            "https://music.youtube.com/youtubei/v1/player?prettyPrint=false"
        );
        // rungs 0+1 serve SABR-only -> advance to the first VR rung.
        let out = feed(&out, SABR);
        assert_eq!(rung_of(&out), 1);
        let out = feed(&out, SABR);
        assert_eq!(rung_of(&out), 2);
        // rung 2 (first ANDROID_VR pin) yields plain URLs -> mint+probe.
        let out = feed(&out, OK);
        let probe_id = probe_of(&out);
        let out = step(&http_response(probe_id, 206, ""));
        let msg = parse(&out);
        assert_eq!(msg["type"], "done");
        assert_eq!(msg["result"]["client"], "ANDROID_VR@1.61.48");
        assert_eq!(msg["result"]["mime"], "audio/mp4");
        assert_eq!(msg["result"]["bitrate_kbps"], 130);
        assert_eq!(msg["result"]["expires_at_ms"], 1_893_456_000_000u64);
        assert!(msg["result"]["url"]
            .as_str()
            .unwrap_or("")
            .starts_with("https://"));
    }

    #[test]
    fn last_rung_success_reports_client() {
        let mut out = begin("vid12345678");
        // The first four rungs serve SABR; the last VR pin yields plain
        // URLs and reports its own client name.
        for rung in 1..=4usize {
            out = feed(&out, SABR);
            assert_eq!(rung_of(&out), rung);
        }
        let out = feed(&out, OK);
        let probe_id = probe_of(&out);
        let out = step(&http_response(probe_id, 206, ""));
        let msg = parse(&out);
        assert_eq!(msg["type"], "done");
        assert_eq!(msg["result"]["client"], "ANDROID_VR@1.43.32");
    }

    #[test]
    fn probe_403_advances_to_next_rung() {
        let out = begin("vid12345678");
        // rung 0 yields plain URLs but its mint refuses the tail.
        let out = feed(&out, OK);
        let probe_id = probe_of(&out);
        let out = step(&http_response(probe_id, 403, ""));
        // The capped rung is skipped: next request is rung 1's player.
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_416_means_short_file_done() {
        let out = begin("vid12345678");
        let out = feed(&out, OK);
        let probe_id = probe_of(&out);
        // File shorter than the probe window cannot cap -> done.
        let out = step(&http_response(probe_id, 416, ""));
        let msg = parse(&out);
        assert_eq!(msg["type"], "done");
        assert_eq!(msg["result"]["client"], "VISIONOS");
    }

    #[test]
    fn all_capped_fails_streams_capped() {
        let mut out = begin("vid12345678");
        for _ in 0..5 {
            out = feed(&out, OK);
            let probe_id = probe_of(&out);
            out = step(&http_response(probe_id, 403, ""));
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "streams-capped".to_string())
        );
    }

    #[test]
    fn probe_id_mismatch_fails() {
        let out = begin("vid12345678");
        let out = feed(&out, OK);
        let _ = probe_of(&out);
        // A response with an id that is not the probe's is a host
        // protocol violation.
        let out = step(&http_response(99, 206, ""));
        assert_eq!(
            fail_kind(&out),
            (
                "invalid-message".to_string(),
                "probe response id mismatch".to_string()
            )
        );
    }

    #[test]
    fn mint_id_mismatch_fails() {
        let out = begin("vid12345678");
        // rung 0 yields plain URLs -> the lazy mint request follows.
        let out = step(&http_response(req_id_of(&out), 200, OK));
        assert!(mint_id_of(&out).is_some());
        // A response with an id that is not the mint's is a host
        // protocol violation.
        let out = step(&http_response(
            99,
            200,
            &json!({"poToken": "tok"}).to_string(),
        ));
        assert_eq!(
            fail_kind(&out),
            (
                "invalid-message".to_string(),
                "mint response id mismatch".to_string()
            )
        );
    }

    #[test]
    fn all_bot_checks_fail_transient() {
        let mut out = begin("vid12345678");
        for _ in 0..5 {
            out = feed(&out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("transient".to_string(), "bot-check".to_string())
        );
    }

    #[test]
    fn visitor_id_propagates_to_next_rung() {
        let out = begin("vid12345678");
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        // rung 0 response carries visitorData -> rung 1 request headers.
        let out = feed(&out, SABR);
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D")
        );
    }

    #[test]
    fn request_shape_and_headers() {
        let out = begin("vid12345678");
        assert_eq!(parse(&out)["payload"]["method"], "POST");
        assert_eq!(
            header_of(&out, "X-Origin").as_deref(),
            Some("https://music.youtube.com")
        );
        assert_eq!(
            header_of(&out, "Referer").as_deref(),
            Some("https://music.youtube.com")
        );
        assert_eq!(
            header_of(&out, "X-Goog-Api-Format-Version").as_deref(),
            Some("1")
        );
        assert!(header_of(&out, "Origin").is_none());
        let body = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(parse(&out)["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert!(body.contains("\"videoId\":\"vid12345678\""));
        assert!(body.contains("\"contentCheckOk\":true"));
        assert!(body.contains("\"clientName\":\"VISIONOS\""));
        // PO tokens never ride player bodies — web BotGuard tokens
        // cannot attest non-web clients.
        assert!(!body.contains("serviceIntegrityDimensions"));
    }

    #[test]
    fn mint_binds_video_id_without_visitor() {
        let out = begin("vid12345678");
        let out = step(&http_response(req_id_of(&out), 200, OK));
        let msg = parse(&out);
        assert_eq!(msg["kind"], "pot_token");
        assert_eq!(msg["payload"]["content_binding"], "vid12345678");
    }

    #[test]
    fn mint_binds_visitor_when_response_carries_one() {
        let out = begin("vid12345678");
        let mut body: Value = serde_json::from_str(OK).unwrap_or_default();
        body["responseContext"] = json!({ "visitorData": "visitor-xyz" });
        let out = step(&http_response(req_id_of(&out), 200, &body.to_string()));
        let msg = parse(&out);
        assert_eq!(msg["kind"], "pot_token");
        assert_eq!(msg["payload"]["content_binding"], "visitor-xyz");
    }

    #[test]
    fn mint_denied_probes_bare_url() {
        let out = begin("vid12345678");
        let out = step(&http_response(req_id_of(&out), 200, OK));
        let mint_id = mint_id_of(&out).unwrap_or_else(|| panic!("mint expected"));
        let out = step(&host_error(mint_id));
        let _ = probe_of(&out);
        assert!(!url_of(&out).contains("pot="));
    }

    #[test]
    fn mint_ok_decorates_probe_and_result() {
        let out = begin("vid12345678");
        let out = step(&http_response(req_id_of(&out), 200, OK));
        let mint_id = mint_id_of(&out).unwrap_or_else(|| panic!("mint expected"));
        let out = step(&http_response(
            mint_id,
            200,
            &json!({ "poToken": "tok-abc" }).to_string(),
        ));
        let probe_id = probe_of(&out);
        assert!(url_of(&out).contains("pot=tok-abc"));
        let out = step(&http_response(probe_id, 206, ""));
        let msg = parse(&out);
        assert_eq!(msg["type"], "done");
        assert!(msg["result"]["url"]
            .as_str()
            .unwrap_or("")
            .contains("pot=tok-abc"));
    }

    #[test]
    fn mint_fires_once_per_resolve() {
        let mut out = begin("vid12345678");
        // rung 0 pick -> mint (denied) -> probe 403 -> rung 1.
        out = step(&http_response(req_id_of(&out), 200, OK));
        let mint_id = mint_id_of(&out).unwrap_or_else(|| panic!("mint expected"));
        out = step(&host_error(mint_id));
        let probe_id = probe_of(&out);
        out = step(&http_response(probe_id, 403, ""));
        assert_eq!(rung_of(&out), 1);
        // rung 1 pick -> straight to probe; no second mint.
        out = step(&http_response(req_id_of(&out), 200, OK));
        assert!(mint_id_of(&out).is_none());
        let _ = probe_of(&out);
    }

    #[test]
    fn all_sabr_fails_unsupported_sabr() {
        let mut out = begin("vid12345678");
        for _ in 0..5 {
            out = feed(&out, SABR);
        }
        assert_eq!(
            fail_kind(&out),
            ("unsupported".to_string(), "sabr-only".to_string())
        );
    }

    #[test]
    fn all_ciphered_fails_unsupported_ciphered() {
        let mut out = begin("vid12345678");
        for _ in 0..5 {
            out = feed(&out, CIPHERED);
        }
        assert_eq!(
            fail_kind(&out),
            ("unsupported".to_string(), "ciphered-only".to_string())
        );
    }

    #[test]
    fn rate_limit_wins_over_last_bucket() {
        let mut out = begin("vid12345678");
        out = feed(&out, UNPLAYABLE);
        let id = req_id_of(&out);
        out = step(&http_response(id, 429, "{}"));
        for _ in 0..3 {
            out = feed(&out, BOT);
        }
        assert_eq!(
            fail_kind(&out),
            ("rate-limit".to_string(), "rate-limit".to_string())
        );
    }

    #[test]
    fn unavailable_ladder_fails_no_result() {
        let mut out = begin("vid12345678");
        for _ in 0..5 {
            out = feed(&out, UNPLAYABLE);
        }
        assert_eq!(
            fail_kind(&out),
            ("no-result".to_string(), "unavailable".to_string())
        );
    }

    #[test]
    fn host_error_advances_rung() {
        let out = begin("vid12345678");
        let out = step(&host_error(req_id_of(&out)));
        assert_eq!(rung_of(&out), 1);
    }
}
