//! The ABI step machine: `invoke` starts the ladder; each `http_response`
//! or `host_error` advances it until a rung yields a playable format or
//! the ladder is exhausted.

use base64::Engine as _;
use core::cell::RefCell;
use serde_json::{json, Value};

use crate::parse::{pick_format, triage, PickOutcome, Triage};
use crate::rungs::{player_request, rung, RUNG_COUNT};

const DEFAULT_VIDEO_ID: &str = "dQw4w9WgXcQ";

/// Why a rung failed; recorded per rung for the final `fail` kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RungOutcome {
    AuthRequired,
    NoResult,
    Transient,
    RateLimit,
    CipheredOnly,
}

impl RungOutcome {
    fn kind(self) -> &'static str {
        match self {
            Self::AuthRequired => "auth-required",
            Self::NoResult => "no-result",
            Self::Transient => "transient",
            Self::RateLimit => "rate-limit",
            Self::CipheredOnly => "unsupported",
        }
    }
}

struct State {
    video_id: String,
    rung: usize,
    next_id: u32,
    outcomes: Vec<RungOutcome>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// One ABI step: consume the step message bytes, produce the response
/// message bytes.
pub fn step(input: &[u8]) -> Vec<u8> {
    let msg: Value = match serde_json::from_slice(input) {
        Ok(v) => v,
        Err(_) => return fail("invalid-response", "step input is not JSON"),
    };
    match msg.get("type").and_then(Value::as_str) {
        Some("invoke") => on_invoke(&msg),
        Some("http_response") => STATE.with(|s| {
            let mut s = s.borrow_mut();
            match s.as_mut() {
                Some(state) => on_http_response(&msg, state),
                None => fail("invalid-response", "http_response before invoke"),
            }
        }),
        Some("host_error") => STATE.with(|s| {
            let mut s = s.borrow_mut();
            match s.as_mut() {
                Some(state) => on_host_error(&msg, state),
                None => fail("invalid-response", "host_error before invoke"),
            }
        }),
        _ => fail("invalid-response", "unknown step message type"),
    }
}

fn on_invoke(msg: &Value) -> Vec<u8> {
    let video_id = msg
        .get("payload")
        .and_then(|p| p.get("source_ref"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_VIDEO_ID)
        .to_string();
    let mut state = State {
        video_id,
        rung: 0,
        next_id: 1,
        outcomes: Vec::new(),
    };
    let req = issue_request(&mut state);
    STATE.with(|s| *s.borrow_mut() = Some(state));
    req
}

/// Emit the current rung's player request, or the terminal `fail` when
/// the ladder is exhausted.
fn issue_request(state: &mut State) -> Vec<u8> {
    if state.rung >= RUNG_COUNT {
        return ladder_failed(&state.outcomes);
    }
    let id = state.next_id;
    state.next_id += 1;
    player_request(state.rung, &state.video_id, id)
}

/// Advance to the next rung, or fail if the ladder is done.
fn advance(state: &mut State, outcome: RungOutcome) -> Vec<u8> {
    state.outcomes.push(outcome);
    state.rung += 1;
    issue_request(state)
}

fn ladder_failed(outcomes: &[RungOutcome]) -> Vec<u8> {
    // The Slice 0 signal: every rung produced only ciphered formats.
    if !outcomes.is_empty() && outcomes.iter().all(|o| *o == RungOutcome::CipheredOnly) {
        return fail("unsupported", "ciphered-only");
    }
    let kind = outcomes.last().map_or("no-result", |o| o.kind());
    fail(kind, "all ladder rungs failed")
}

fn on_http_response(msg: &Value, state: &mut State) -> Vec<u8> {
    let status = msg
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(0);
    let body: Value = match msg.get("body").and_then(Value::as_str) {
        Some(b64) => match base64::engine::general_purpose::STANDARD
            .decode(b64)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(v) => v,
            None => return advance(state, RungOutcome::Transient),
        },
        None => return advance(state, RungOutcome::Transient),
    };
    match triage(status, &body) {
        Triage::Playable => match pick_format(&body) {
            PickOutcome::Picked(p) => done(&json!({
                "url": p.url,
                "mime": p.mime,
                "bitrate_kbps": p.bitrate_kbps,
                "expires_at_ms": p.expires_at_ms,
                "client": rung(state.rung).map_or("unknown", |r| r.name),
            })),
            PickOutcome::CipheredOnly => advance(state, RungOutcome::CipheredOnly),
            PickOutcome::NoAudio => advance(state, RungOutcome::NoResult),
        },
        Triage::AuthRequired => advance(state, RungOutcome::AuthRequired),
        Triage::NoResult => advance(state, RungOutcome::NoResult),
        Triage::Transient => advance(state, RungOutcome::Transient),
        Triage::RateLimit => advance(state, RungOutcome::RateLimit),
    }
}

fn on_host_error(msg: &Value, state: &mut State) -> Vec<u8> {
    let outcome = match msg
        .get("error")
        .and_then(|e| e.get("kind"))
        .and_then(Value::as_str)
    {
        Some("permission-denied") => RungOutcome::Transient,
        Some("rate-limit") => RungOutcome::RateLimit,
        _ => RungOutcome::Transient,
    };
    advance(state, outcome)
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

    fn rung_of(out: &[u8]) -> usize {
        let msg = parse(out);
        let headers = msg["payload"]["headers"]
            .as_array()
            .unwrap_or(&Vec::new())
            .clone();
        let name = headers
            .iter()
            .find(|h| h[0] == "X-Youtube-Client-Name")
            .and_then(|h| h[1].as_str().map(str::to_string))
            .unwrap_or_default();
        match name.as_str() {
            "5" => 0,
            "28" => 1,
            _ => panic!("unexpected rung in output: {msg}"),
        }
    }

    #[test]
    fn ladder_walks_both_rungs_then_done() {
        let out = step(&invoke_msg("vid12345678"));
        let req = parse(&out);
        assert_eq!(req["type"], "host_request");
        assert_eq!(rung_of(&out), 0);
        let body = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(req["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert!(body.contains("\"videoId\":\"vid12345678\""));
        assert_eq!(
            req["payload"]["url"].as_str().unwrap_or(""),
            "https://www.youtube.com/youtubei/v1/player?prettyPrint=false"
        );

        // rung 0 hits the login wall -> rung 1
        let out = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-login-required.json"),
        ));
        assert_eq!(rung_of(&out), 1);

        // rung 1 returns plain urls -> done
        let out = step(&http_response(
            2,
            200,
            include_str!("../fixtures/player-ok-plain-urls.json"),
        ));
        let msg = parse(&out);
        assert_eq!(msg["type"], "done");
        assert_eq!(msg["result"]["client"], "ANDROID_VR");
        assert_eq!(msg["result"]["mime"], "audio/mp4");
        assert_eq!(msg["result"]["bitrate_kbps"], 129);
        assert_eq!(msg["result"]["expires_at_ms"], 1_893_456_000_000u64);
        assert!(msg["result"]["url"]
            .as_str()
            .unwrap_or("")
            .starts_with("https://"));
    }

    #[test]
    fn ciphered_on_both_rungs_fails_unsupported() {
        let _ = step(&invoke_msg("vid12345678"));
        let _ = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-ciphered-only.json"),
        ));
        let out = step(&http_response(
            2,
            200,
            include_str!("../fixtures/player-ciphered-only.json"),
        ));
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "unsupported");
        assert_eq!(msg["error"]["message"], "ciphered-only");
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

    #[test]
    fn exhausted_ladder_reports_last_kind() {
        let _ = step(&invoke_msg("vid12345678"));
        let _ = step(&http_response(
            1,
            200,
            include_str!("../fixtures/player-unplayable.json"),
        ));
        let out = step(&http_response(2, 429, "{}"));
        let msg = parse(&out);
        assert_eq!(msg["type"], "fail");
        assert_eq!(msg["error"]["kind"], "rate-limit");
    }
}
