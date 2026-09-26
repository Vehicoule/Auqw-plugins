//! `catalog.suggest`: keystroke-time query completions over the
//! WEB_REMIX `music/get_search_suggestions` endpoint — the same
//! metadata-only client `playback.candidates` and `radio.seed` use.
//! One request per invocation, no upstream looping; the contract is a
//! flat string list (`{suggestions: [...]}`), ordered as served.

use serde_json::{json, Map, Value};

use auqw_guest_sdk::{http_request, kv_set, GuestError, HttpRequest};

use crate::candidates::{web_remix_context, web_remix_request, VISITOR_KEY};
use crate::guest::{bad_payload, failed, load_visitor, payload_keys, warn};
use crate::parse::{visitor_data, visitor_token};

const SUGGEST_URL: &str = "https://music.youtube.com/youtubei/v1/music/get_search_suggestions?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false";

/// Input bounds: completions never need more than a line of text.
const MAX_INPUT_CHARS: usize = 256;

/// Served suggestions are capped — a pathological renderer list is a
/// malformed response, not a completion set.
const MAX_SUGGESTIONS: usize = 20;

/// A validated payload: exactly `input`, optionally `limit`.
struct SuggestPayload {
    input: String,
    limit: usize,
}

fn parse_suggest_payload(payload: &Value) -> Result<SuggestPayload, GuestError> {
    let obj = payload_keys(payload, &["input", "limit"], &["input"])?;
    let input = match &obj["input"] {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() || t.chars().count() > MAX_INPUT_CHARS {
                return Err(bad_payload("input must be nonempty and at most 256 chars"));
            }
            t.to_string()
        }
        _ => return Err(bad_payload("input must be a nonempty string")),
    };
    let limit = match obj.get("limit") {
        None | Some(Value::Null) => 10,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) if v >= 1 => (v as usize).min(MAX_SUGGESTIONS),
            _ => return Err(bad_payload("limit must be a positive integer")),
        },
        _ => return Err(bad_payload("limit must be a positive integer")),
    };
    Ok(SuggestPayload { input, limit })
}

/// The WEB_REMIX `music/get_search_suggestions` call.
fn suggest_request(input: &str, visitor: Option<&str>) -> HttpRequest {
    let body = json!({
        "context": web_remix_context(),
        "input": input,
    });
    web_remix_request(SUGGEST_URL, body, visitor, None)
}

/// One suggestion row: the canonical query it commits is
/// `navigationEndpoint.searchEndpoint.query` — display runs can drop
/// word separators at bold splits, so joined runs are only the
/// fallback for rows that carry no navigation endpoint. Empty results
/// drop, duplicates collapse.
fn suggestion_text(renderer: &Map<String, Value>) -> Option<String> {
    if let Some(query) = renderer
        .get("navigationEndpoint")
        .and_then(|n| n.get("searchEndpoint"))
        .and_then(|s| s.get("query"))
        .and_then(Value::as_str)
    {
        let trimmed = query.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let runs = renderer.get("suggestion")?.get("runs")?.as_array()?;
    let mut text = String::new();
    for run in runs {
        text.push_str(run.get("text")?.as_str()?);
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub async fn suggest(payload: &Value) -> Result<Value, GuestError> {
    let p = parse_suggest_payload(payload)?;
    let visitor = load_visitor(VISITOR_KEY).await?;
    let resp = match http_request(suggest_request(&p.input, visitor.as_deref())).await {
        Ok(r) => r,
        Err(GuestError::Host { kind, message }) => match kind.as_str() {
            "cancelled" | "permission-denied" | "invalid-response" => {
                return Err(GuestError::Host { kind, message });
            }
            _ => return Err(failed("transient", "suggest transport".into())),
        },
        Err(e) => return Err(e),
    };
    match resp.status {
        s if (200..300).contains(&s) => {}
        429 => return Err(failed("rate-limit", "rate-limit".into())),
        _ => return Err(failed("transient", "suggest transport".into())),
    }
    let body: Value = serde_json::from_slice(&resp.body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| {
            failed(
                "invalid-response",
                "suggest body is not a JSON object".into(),
            )
        })?;
    if let Some(raw) = visitor_data(&body) {
        if let Some(v) = visitor_token(&raw) {
            kv_set(VISITOR_KEY, Some(v.as_bytes())).await?;
        } else {
            warn("ignoring malformed visitor value").await?;
        }
    }
    // The suggestion section must exist: an upstream error body or an
    // unexpected shape is `invalid-response`, not an empty completion
    // set — while a present-but-empty contents array is the honest
    // "no completions" signal.
    let contents = body
        .get("contents")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|s| s.get("searchSuggestionsSectionRenderer"))
        .and_then(|s| s.get("contents"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            failed(
                "invalid-response",
                "suggest body carries no suggestion section".into(),
            )
        })?;
    let mut out: Vec<String> = Vec::new();
    // Scan bound: a pathological renderer list is bounded regardless
    // of how many rows skip or dedupe.
    for item in contents.iter().take(MAX_SUGGESTIONS * 4) {
        let renderer = match item
            .get("searchSuggestionRenderer")
            .and_then(Value::as_object)
        {
            Some(r) => r,
            None => continue,
        };
        if let Some(text) = suggestion_text(renderer) {
            if !out.contains(&text) {
                out.push(text);
            }
            if out.len() >= p.limit.min(MAX_SUGGESTIONS) {
                break;
            }
        }
    }
    Ok(json!({ "suggestions": out }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use auqw_guest_sdk::{dispatch_step, reset_for_testing};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use std::collections::BTreeMap;

    const SUGGESTIONS: &str = include_str!("../fixtures/suggest-songs.json");

    fn step(input: &Value) -> Value {
        let out = dispatch_step(
            &serde_json::to_vec(input).unwrap_or_default(),
            crate::guest::dispatch,
        );
        serde_json::from_slice(&out).unwrap_or_else(|e| panic!("guest output is not JSON: {e}"))
    }

    struct Harness {
        committed: BTreeMap<String, Vec<u8>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                committed: BTreeMap::new(),
            }
        }

        fn drive(&mut self, mut out: Value) -> Value {
            loop {
                if out["type"] != "host_request" || out["kind"] == "http_request" {
                    return out;
                }
                let id = out["id"].as_u64().unwrap_or(u64::MAX);
                out = match out["kind"].as_str().unwrap_or("") {
                    "kv_get" => {
                        let value = self
                            .committed
                            .get(out["payload"]["key"].as_str().unwrap_or(""))
                            .map(|v| json!(B64.encode(v)))
                            .unwrap_or(Value::Null);
                        step(&json!({"type":"kv_response","id":id,"value":value}))
                    }
                    "kv_set" => {
                        if let Some(v) = out["payload"]["value"]
                            .as_str()
                            .and_then(|s| B64.decode(s).ok())
                        {
                            self.committed.insert(
                                out["payload"]["key"].as_str().unwrap_or("").to_string(),
                                v,
                            );
                        }
                        step(&json!({"type":"host_ok","id":id}))
                    }
                    "log" => step(&json!({"type":"host_ok","id":id})),
                    other => panic!("unexpected kind {other}"),
                };
            }
        }

        fn invoke(&mut self, payload: Value) -> Value {
            reset_for_testing();
            let out = step(&json!({
                "type": "invoke", "request_id": "t", "capability": "catalog.suggest",
                "payload": payload,
            }));
            self.drive(out)
        }

        fn answer(&mut self, out: &Value, status: u16, body: &str) -> Value {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            let next = step(&json!({
                "type": "http_response", "id": id, "status": status, "headers": [],
                "body": B64.encode(body),
            }));
            self.drive(next)
        }
    }

    #[test]
    fn suggest_fixture_yields_deduped_suggestions() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "input": "awa " }));
        assert_eq!(out["kind"], "http_request");
        assert_eq!(
            out["payload"]["url"],
            "https://music.youtube.com/youtubei/v1/music/get_search_suggestions?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false"
        );
        let out = h.answer(&out, 200, SUGGESTIONS);
        assert_eq!(out["type"], "done");
        let suggestions = out["result"]["suggestions"]
            .as_array()
            .unwrap_or_else(|| panic!("suggestions array"));
        // Canonical `searchEndpoint.query` wins over joined runs (row
        // 2 would join to "awa 2lacrim"); history renderers skip; the
        // exact duplicate of row 1 collapses; whitespace-only drops.
        assert_eq!(
            suggestions,
            &json!(["awa lacrim", "awa 2 lacrim", "awa imani"])
                .as_array()
                .unwrap_or_else(|| panic!("expected array"))[..]
        );
        // The response's visitorData persisted for the next call.
        assert!(h.committed.contains_key(VISITOR_KEY));
    }

    #[test]
    fn suggest_blank_input_rejects_before_http() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "input": "   " }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
    }

    #[test]
    fn suggest_oversized_input_rejects_before_http() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "input": "x".repeat(300) }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
    }

    #[test]
    fn suggest_429_is_rate_limit() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "input": "awa" }));
        let out = h.answer(&out, 429, "{}");
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "rate-limit");
    }
}
