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
use crate::rungs::{player_request, probe_request, LADDER};

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

struct State {
    video_id: String,
    rung: usize,
    next_id: u32,
    /// `responseContext.visitorData` from the most recent rung response;
    /// invocation-scoped, never persisted.
    visitor_id: Option<String>,
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

/// Advance to the next rung, or fail if the ladder is done.
fn advance(state: &mut State, outcome: RungOutcome) -> Vec<u8> {
    state.outcomes.push(outcome);
    state.rung += 1;
    issue_request(state)
}

fn on_http_step(msg: &Value, state: &mut State) -> Vec<u8> {
    // A probe response is lock-step: while `pending_probe` is set, this
    // step is the probe verdict, not a player response.
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
            Some(picked) => issue_probe(state, picked),
            None => advance(state, RungOutcome::NoAudio),
        },
        FormatOutcome::SabrOnly => advance(state, RungOutcome::SabrOnly),
        FormatOutcome::CipheredOnly => advance(state, RungOutcome::CipheredOnly),
        FormatOutcome::NoAudio => advance(state, RungOutcome::NoAudio),
    }
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

    fn parse(out: &[u8]) -> Value {
        match serde_json::from_slice(out) {
            Ok(v) => v,
            Err(e) => panic!("guest output is not JSON: {e}"),
        }
    }

    /// The rung index a `host_request` is for, read off its
    /// `X-YouTube-Client-Name` + version headers. Only meaningful for
    /// POST (player) requests — probes are GET and carry no client
    /// headers.
    fn rung_of(out: &[u8]) -> usize {
        let msg = parse(out);
        assert_eq!(msg["type"], "host_request");
        assert_eq!(msg["payload"]["method"], "POST");
        let headers = msg["payload"]["headers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let header = |name: &str| {
            headers
                .iter()
                .find(|h| h[0].as_str() == Some(name))
                .and_then(|h| h[1].as_str())
                .unwrap_or("")
                .to_string()
        };
        match (
            header("X-YouTube-Client-Name").as_str(),
            header("X-YouTube-Client-Version").as_str(),
        ) {
            ("101", "1.02") => 0,
            ("5", "20.10.4") => 1,
            ("28", "1.61.48") => 2,
            ("28", "1.60.19") => 3,
            ("28", "1.43.32") => 4,
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
        u32::try_from(msg["id"].as_u64().unwrap_or(0)).unwrap_or(0)
    }

    fn header_of(out: &[u8], name: &str) -> Option<String> {
        let msg = parse(out);
        msg["payload"]["headers"]
            .as_array()?
            .iter()
            .find(|h| h[0].as_str() == Some(name))
            .and_then(|h| h[1].as_str().map(str::to_string))
    }

    #[test]
    fn sabr_then_plain_succeeds_on_rung_two() {
        let out = step(&invoke_msg("vid12345678"));
        assert_eq!(rung_of(&out), 0);
        assert_eq!(
            parse(&out)["payload"]["url"],
            "https://music.youtube.com/youtubei/v1/player?prettyPrint=false"
        );
        // rungs 1+2 serve SABR-only -> advance to the first VR rung.
        for id in 1..=2u32 {
            let out = step(&http_response(
                id,
                200,
                include_str!("../fixtures/player-sabr-only.json"),
            ));
            assert_eq!(rung_of(&out), id as usize);
        }
        // rung 3 (first ANDROID_VR pin) yields plain URLs -> probe.
        let out = step(&http_response(
            3,
            200,
            include_str!("../fixtures/player-ok-plain-urls.json"),
        ));
        let probe_id = probe_of(&out);
        // Probe serves the boundary window -> done.
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
        let _ = step(&invoke_msg("vid12345678"));
        // The first four rungs serve SABR; the last VR pin yields plain
        // URLs and reports its own client name.
        for id in 1..=4u32 {
            let out = step(&http_response(
                id,
                200,
                include_str!("../fixtures/player-sabr-only.json"),
            ));
            assert_eq!(rung_of(&out), id as usize);
        }
        let out = step(&http_response(
            5,
            200,
            include_str!("../fixtures/player-ok-plain-urls.json"),
        ));
        let probe_id = probe_of(&out);
        let out = step(&http_response(probe_id, 206, ""));
        let msg = parse(&out);
        assert_eq!(msg["type"], "done");
        assert_eq!(msg["result"]["client"], "ANDROID_VR@1.43.32");
    }

    #[test]
    fn probe_403_advances_to_next_rung() {
        let _ = step(&invoke_msg("vid12345678"));
        // rung 0 yields plain URLs but its mint refuses the boundary.
        let out = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-ok-plain-urls.json"),
        ));
        let probe_id = probe_of(&out);
        let out = step(&http_response(probe_id, 403, ""));
        // The capped rung is skipped: next request is rung 1's player.
        assert_eq!(rung_of(&out), 1);
    }

    #[test]
    fn probe_416_means_short_file_done() {
        let _ = step(&invoke_msg("vid12345678"));
        let out = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-ok-plain-urls.json"),
        ));
        let probe_id = probe_of(&out);
        // File shorter than the probe boundary cannot cap -> done.
        let out = step(&http_response(probe_id, 416, ""));
        let msg = parse(&out);
        assert_eq!(msg["type"], "done");
        assert_eq!(msg["result"]["client"], "VISIONOS");
    }

    #[test]
    fn all_capped_fails_streams_capped() {
        let _ = step(&invoke_msg("vid12345678"));
        let mut out = Vec::new();
        for id in 1..=5u32 {
            out = step(&http_response(
                id,
                200,
                include_str!("../fixtures/player-ok-plain-urls.json"),
            ));
            let probe_id = probe_of(&out);
            out = step(&http_response(probe_id, 403, ""));
        }
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "transient");
        assert_eq!(msg["error"]["message"], "streams-capped");
    }

    #[test]
    fn probe_id_mismatch_fails() {
        let _ = step(&invoke_msg("vid12345678"));
        let out = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-ok-plain-urls.json"),
        ));
        let _ = probe_of(&out);
        // A response with an id that is not the probe's is a host
        // protocol violation.
        let out = step(&http_response(99, 206, ""));
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "invalid-message");
    }

    #[test]
    fn all_bot_checks_fail_transient() {
        let _ = step(&invoke_msg("vid12345678"));
        let mut out = Vec::new();
        for id in 1..=5u32 {
            out = step(&http_response(
                id,
                200,
                include_str!("../fixtures/player-bot-check.json"),
            ));
        }
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "transient");
        assert_eq!(msg["error"]["message"], "bot-check");
    }

    #[test]
    fn visitor_id_propagates_to_next_rung() {
        let out = step(&invoke_msg("vid12345678"));
        assert!(header_of(&out, "X-Goog-Visitor-Id").is_none());
        // rung 1 response carries visitorData -> rung 2 request headers.
        let out = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-sabr-only.json"),
        ));
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D")
        );
    }

    #[test]
    fn request_shape_and_headers() {
        let out = step(&invoke_msg("vid12345678"));
        let msg = parse(&out);
        assert_eq!(msg["payload"]["method"], "POST");
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
                .decode(msg["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert!(body.contains("\"videoId\":\"vid12345678\""));
        assert!(body.contains("\"contentCheckOk\":true"));
        assert!(body.contains("\"clientName\":\"VISIONOS\""));
    }

    #[test]
    fn all_sabr_fails_unsupported_sabr() {
        let _ = step(&invoke_msg("vid12345678"));
        let mut out = Vec::new();
        for id in 1..=5u32 {
            out = step(&http_response(
                id,
                200,
                include_str!("../fixtures/player-sabr-only.json"),
            ));
        }
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "unsupported");
        assert_eq!(msg["error"]["message"], "sabr-only");
    }

    #[test]
    fn all_ciphered_fails_unsupported_ciphered() {
        let _ = step(&invoke_msg("vid12345678"));
        let mut out = Vec::new();
        for id in 1..=5u32 {
            out = step(&http_response(
                id,
                200,
                include_str!("../fixtures/player-ciphered-only.json"),
            ));
        }
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "unsupported");
        assert_eq!(msg["error"]["message"], "ciphered-only");
    }

    #[test]
    fn rate_limit_wins_over_last_bucket() {
        let _ = step(&invoke_msg("vid12345678"));
        let _ = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-unplayable.json"),
        ));
        let _ = step(&http_response(2, 429, "{}"));
        let _ = step(&http_response(
            3,
            200,
            include_str!("../fixtures/player-bot-check.json"),
        ));
        let _ = step(&http_response(
            4,
            200,
            include_str!("../fixtures/player-bot-check.json"),
        ));
        let out = step(&http_response(
            5,
            200,
            include_str!("../fixtures/player-bot-check.json"),
        ));
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "rate-limit");
    }

    #[test]
    fn unavailable_ladder_fails_no_result() {
        let _ = step(&invoke_msg("vid12345678"));
        let mut out = Vec::new();
        for id in 1..=5u32 {
            out = step(&http_response(
                id,
                200,
                include_str!("../fixtures/player-unplayable.json"),
            ));
        }
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "no-result");
    }

    #[test]
    fn host_error_advances_rung() {
        let _ = step(&invoke_msg("vid12345678"));
        let err = serde_json::to_vec(&json!({
            "type": "host_error",
            "id": 1,
            "error": { "kind": "transient", "message": "conn reset" },
        }))
        .unwrap_or_default();
        let out = step(&err);
        assert_eq!(rung_of(&out), 1);
    }
}
