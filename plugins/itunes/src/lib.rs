//! iTunes catalog guest: `catalog.search`, `catalog.metadata`, and
//! `catalog.artwork` over the public keyless JSON API (ABI 0.2.0).
//!
//! The guest returns raw provider metadata as `trackMetadata` —
//! version labels and candidate scoring are the application's job.
//! `previewUrl` is never emitted: a 30-second preview is not the
//! recording.

mod encode;
mod http;
mod parse;

use std::collections::BTreeMap;

use auqw_guest_sdk::{export_plugin, GuestError, GuestFuture, Invocation};
use serde_json::{json, Map, Value};

const API: &str = "https://itunes.apple.com";
const SEARCH_ARTWORK_SIZE: u64 = 1200;

fn dispatch(inv: Invocation) -> GuestFuture {
    Box::pin(async move {
        match inv.capability.as_str() {
            "catalog.search" => search(&inv.payload).await,
            "catalog.metadata" => metadata(&inv.payload).await,
            "catalog.artwork" => artwork(&inv.payload).await,
            other => Err(failed(
                "not-applicable",
                format!("capability {other} not supported"),
            )),
        }
    })
}

export_plugin!(dispatch);

fn failed(kind: &str, message: String) -> GuestError {
    GuestError::Failed {
        kind: kind.into(),
        message,
    }
}

fn bad_payload(m: &str) -> GuestError {
    failed("invalid-response", format!("payload: {m}"))
}

/// The payload/ref object must contain exactly `keys` — missing
/// fields or extras are `invalid-response`.
fn payload_obj<'a>(
    payload: &'a Value,
    keys: &[&str],
) -> Result<&'a Map<String, Value>, GuestError> {
    let obj = payload
        .as_object()
        .ok_or_else(|| bad_payload("must be an object"))?;
    for k in obj.keys() {
        if !keys.contains(&k.as_str()) {
            return Err(bad_payload("unexpected key"));
        }
    }
    for k in keys {
        if !obj.contains_key(*k) {
            return Err(bad_payload("missing key"));
        }
    }
    Ok(obj)
}

/// A well-formed `sourceRef` for this provider, or a typed rejection:
/// foreign-but-shaped refs are `not-applicable`, malformed ones are
/// `invalid-response`. Returned id is the validated `trackId` digits.
fn itunes_ref(v: &Value) -> Result<String, GuestError> {
    let o = payload_obj(v, &["provider", "kind", "id"])?;
    let provider = o["provider"]
        .as_str()
        .ok_or_else(|| bad_payload("ref.provider must be a string"))?;
    let kind = o["kind"]
        .as_str()
        .ok_or_else(|| bad_payload("ref.kind must be a string"))?;
    let id = o["id"]
        .as_str()
        .ok_or_else(|| bad_payload("ref.id must be a string"))?;
    if provider != "itunes" || kind != "track" {
        return Err(failed(
            "not-applicable",
            "ref is not an itunes track ref".into(),
        ));
    }
    // The id becomes a URL query segment: ASCII digits only, in u64
    // range, nonzero — anything else is `invalid-response`, never a
    // request.
    let parsed = if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        id.parse::<u64>().ok()
    } else {
        None
    };
    match parsed {
        Some(n) if n > 0 => Ok(id.to_string()),
        _ => Err(bad_payload("ref.id must be ASCII digits > 0")),
    }
}

fn storefront_of(obj: &Map<String, Value>) -> Result<Option<String>, GuestError> {
    match obj.get("storefront") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic()) => {
            Ok(Some(s.to_uppercase()))
        }
        _ => Err(bad_payload("storefront must be two ASCII letters or null")),
    }
}

async fn search(payload: &Value) -> Result<Value, GuestError> {
    let obj = payload_obj(payload, &["query", "limit", "storefront"])?;
    let query = obj["query"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    // The schema caps `query` at 512 characters (Unicode scalars), not
    // bytes — a 512-CJK-character query is legal and stays bounded by
    // the request budgets.
    if query.is_empty() || query.chars().count() > 512 {
        return Err(bad_payload(
            "query must be a nonempty string of at most 512 characters",
        ));
    }
    let limit = obj["limit"]
        .as_u64()
        .ok_or_else(|| bad_payload("limit must be an integer"))?
        .clamp(1, 200);
    let storefront = storefront_of(obj)?;

    let mut url = format!(
        "{API}/search?term={}&media=music&entity=song&limit={limit}",
        encode::percent_encode(&query)
    );
    if let Some(sf) = &storefront {
        url.push_str(&format!("&country={sf}"));
    }

    match http::get_json(&url).await? {
        http::Outcome::NotFound => Err(failed("transient", "itunes search status 404".into())),
        http::Outcome::Body(body) => {
            let items: Vec<Value> = parse::dedup(parse::parse_tracks(&body)?)
                .iter()
                .map(|t| parse::to_metadata(t, storefront.as_deref(), SEARCH_ARTWORK_SIZE))
                .collect();
            Ok(json!({ "items": items, "storefront": storefront }))
        }
    }
}

async fn metadata(payload: &Value) -> Result<Value, GuestError> {
    let obj = payload_obj(payload, &["refs"])?;
    let refs = obj["refs"]
        .as_array()
        .ok_or_else(|| bad_payload("refs must be an array"))?;
    if refs.len() > 200 {
        return Err(bad_payload("refs is limited to 200 entries"));
    }
    let mut ids = Vec::with_capacity(refs.len());
    for r in refs {
        ids.push(itunes_ref(r)?);
    }

    let mut by_id: BTreeMap<String, parse::Track> = BTreeMap::new();
    // refs ≤ 200 per the check above — one lookup call covers them all.
    if !ids.is_empty() {
        let url = format!("{API}/lookup?id={}", ids.join(","));
        if let http::Outcome::Body(body) = http::get_json(&url).await? {
            for t in parse::parse_tracks(&body)? {
                by_id.entry(t.id.clone()).or_insert(t);
            }
        }
    }
    let items: Vec<Value> = ids
        .iter()
        .filter_map(|id| by_id.get(id))
        .map(|t| parse::to_metadata(t, None, SEARCH_ARTWORK_SIZE))
        .collect();
    Ok(json!({ "items": items }))
}

async fn artwork(payload: &Value) -> Result<Value, GuestError> {
    let obj = payload_obj(payload, &["ref", "size"])?;
    let id = itunes_ref(&obj["ref"])?;
    let size = match obj["size"].as_u64() {
        Some(600) => 600_u64,
        Some(1200) => 1200_u64,
        _ => return Err(bad_payload("size must be 600 or 1200")),
    };

    let url = format!("{API}/lookup?id={id}");
    let items: Vec<Value> = match http::get_json(&url).await? {
        http::Outcome::NotFound => Vec::new(),
        http::Outcome::Body(body) => parse::parse_tracks(&body)?
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| parse::artwork_ref(t, size))
            .into_iter()
            .collect(),
    };
    Ok(json!({
        "source_ref": { "provider": "itunes", "kind": "track", "id": id },
        "items": items,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    const MIXED: &str = include_str!("../fixtures/search-mixed.json");
    const DUPLICATES: &str = include_str!("../fixtures/search-duplicates.json");
    const EMPTY: &str = include_str!("../fixtures/search-empty.json");
    const MALFORMED: &str = include_str!("../fixtures/search-malformed.json");
    const LOOKUP: &str = include_str!("../fixtures/lookup.json");

    fn step(input: &Value) -> Value {
        let out =
            auqw_guest_sdk::dispatch_step(&serde_json::to_vec(input).unwrap_or_default(), dispatch);
        serde_json::from_slice(&out).unwrap_or_else(|e| panic!("guest output is not JSON: {e}"))
    }

    fn invoke(cap: &str, payload: Value) -> Value {
        // Tests share worker threads; clear any state a previous test
        // left parked before starting a fresh invocation.
        auqw_guest_sdk::reset_for_testing();
        step(&json!({
            "type": "invoke", "request_id": "t", "capability": cap, "payload": payload,
        }))
    }

    fn req_id(out: &Value) -> u64 {
        out["id"].as_u64().unwrap_or(u64::MAX)
    }

    fn http_ok(id: u64, body: &str) -> Value {
        json!({
            "type": "http_response", "id": id, "status": 200,
            "headers": [], "body": B64.encode(body),
        })
    }

    fn http_status(id: u64, status: u16, headers: &[(&str, &str)]) -> Value {
        let pairs: Vec<Value> = headers.iter().map(|(n, v)| json!([n, v])).collect();
        json!({
            "type": "http_response", "id": id, "status": status,
            "headers": pairs, "body": "",
        })
    }

    fn items_of(out: &Value) -> &Vec<Value> {
        out["result"]["items"]
            .as_array()
            .unwrap_or_else(|| panic!("result.items not an array: {out}"))
    }

    /// Invoke `catalog.search` and return the emitted `http_request`.
    fn search_request(payload: Value) -> Value {
        let out = invoke("catalog.search", payload);
        assert_eq!(out["type"], "host_request", "{out}");
        assert_eq!(out["kind"], "http_request", "{out}");
        out
    }

    #[test]
    fn search_request_is_encoded_and_bounded() {
        let out = search_request(json!({
            "query": "Roads & 夜", "limit": 500, "storefront": "us",
        }));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(url.starts_with("https://itunes.apple.com/search?"), "{url}");
        assert!(url.contains("term=Roads%20%26%20%E5%A4%9C"), "{url}");
        assert!(
            url.contains("media=music") && url.contains("entity=song"),
            "{url}"
        );
        assert!(url.contains("limit=200"), "{url}");
        assert!(url.contains("country=US"), "{url}");
        assert_eq!(out["payload"]["method"], "GET");
        let headers = out["payload"]["headers"].to_string();
        assert!(headers.contains("Auqw/0.1"), "{headers}");
    }

    #[test]
    fn search_limit_floor_and_null_storefront() {
        let out = search_request(json!({"query": "x", "limit": 0, "storefront": null}));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(url.contains("limit=1"), "{url}");
        assert!(!url.contains("country="), "{url}");
    }

    #[test]
    fn invalid_storefront_is_invalid_response() {
        for sf in ["USA", "u1", "us "] {
            let out = invoke(
                "catalog.search",
                json!({"query": "x", "limit": 5, "storefront": sf}),
            );
            assert_eq!(out["type"], "fail", "{sf}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{sf}");
        }
    }

    #[test]
    fn empty_query_is_invalid_response() {
        let out = invoke(
            "catalog.search",
            json!({"query": "   ", "limit": 5, "storefront": null}),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
    }

    #[test]
    fn search_mixed_fixture_filters_and_maps() {
        let req = search_request(json!({"query": "x", "limit": 10, "storefront": "US"}));
        let out = step(&http_ok(req_id(&req), MIXED));
        assert_eq!(out["type"], "done", "{out}");
        let items = items_of(&out);
        // music-video and zero-id rows dropped; four songs survive.
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(out["result"]["storefront"], "US");

        let first = &items[0];
        assert_eq!(first["title"], "Roads");
        assert_eq!(first["artist"], "Portishead");
        assert_eq!(first["album"], "Dummy");
        assert_eq!(first["duration_ms"], 307_013);
        assert_eq!(first["release_year"], 1994);
        assert_eq!(first["explicit"], false);
        assert_eq!(first["genre"], "Alternative");
        assert_eq!(first["source_ref"]["provider"], "itunes");
        assert_eq!(first["source_ref"]["kind"], "track");
        assert_eq!(first["source_ref"]["id"], "1440761789");
        let art = &first["artwork"][0];
        assert!(
            art["url"]
                .as_str()
                .unwrap_or_default()
                .contains("1200x1200bb"),
            "{art}"
        );
        assert_eq!(art["width"], 1200);
        assert_eq!(art["height"], 1200);

        let no_art = items
            .iter()
            .find(|i| i["title"] == "No Art Song")
            .unwrap_or_else(|| panic!("missing no-art item"));
        assert_eq!(no_art["artwork"].as_array().map(Vec::len), Some(0));
        assert_eq!(no_art["explicit"], false, "cleaned maps to false");

        assert!(items.iter().any(|i| i["title"] == "夜の歌"));
        let explicit = items
            .iter()
            .find(|i| i["title"] == "Explicit Song")
            .unwrap_or_else(|| panic!("missing explicit item"));
        assert_eq!(explicit["explicit"], true);

        // Preview URLs never cross into the result.
        let s = out["result"].to_string();
        assert!(!s.contains("previewUrl") && !s.contains("audio-ssl"), "{s}");
    }

    #[test]
    fn search_duplicates_collapse_only_true_dupes() {
        let req = search_request(json!({"query": "song", "limit": 50, "storefront": null}));
        let out = step(&http_ok(req_id(&req), DUPLICATES));
        assert_eq!(out["type"], "done", "{out}");
        let items = items_of(&out);
        let titles: Vec<&str> = items.iter().filter_map(|i| i["title"].as_str()).collect();
        // The +1.5 s compilation duplicate collapses; labelled variants
        // and the explicit/clean pair (different explicitness) survive.
        assert_eq!(
            titles,
            [
                "Song",
                "Song",
                "Song",
                "Song (Live)",
                "Song (Remix)",
                "Song (Remastered)"
            ]
        );
        // First-seen wins: the surviving plain "Song" is the studio row.
        assert_eq!(items[0]["source_ref"]["id"], "100");
        assert_eq!(items[0]["explicit"], false);
        // The cleaned row (explicit=false, +30 s) and explicit row
        // (explicit=true) both survive the plain duplicate rule.
        assert_eq!(items[1]["source_ref"]["id"], "600");
        assert_eq!(items[1]["explicit"], false);
        assert_eq!(items[2]["source_ref"]["id"], "700");
        assert_eq!(items[2]["explicit"], true);
    }

    #[test]
    fn search_empty_is_done_with_no_items() {
        let req = search_request(json!({"query": "x", "limit": 5, "storefront": null}));
        let out = step(&http_ok(req_id(&req), EMPTY));
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
    }

    #[test]
    fn malformed_body_is_invalid_response() {
        for body in [MALFORMED, "not json {"] {
            let req = search_request(json!({"query": "x", "limit": 5, "storefront": null}));
            let out = step(&http_ok(req_id(&req), body));
            assert_eq!(out["type"], "fail", "{body}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{body}");
        }
    }

    #[test]
    fn rate_limit_logs_then_fails() {
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let out = step(&http_status(req_id(&req), 429, &[("Retry-After", "30")]));
        // The retry hint goes to the diagnostic log, not the result.
        assert_eq!(out["type"], "host_request", "{out}");
        assert_eq!(out["kind"], "log", "{out}");
        let msg = out["payload"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("retry_after=30"), "{msg}");
        assert!(!msg.contains("http"), "{msg}");
        let out = step(&json!({"type": "host_ok", "id": req_id(&out)}));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "rate-limit");
    }

    #[test]
    fn server_and_other_errors_are_transient() {
        for status in [500_u16, 503, 418] {
            let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
            let out = step(&http_status(req_id(&req), status, &[]));
            assert_eq!(out["type"], "fail", "{status}");
            assert_eq!(out["error"]["kind"], "transient", "{status}");
        }
    }

    #[test]
    fn search_404_is_transient() {
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let out = step(&http_status(req_id(&req), 404, &[]));
        assert_eq!(out["error"]["kind"], "transient");
    }

    #[test]
    fn host_error_kind_propagates() {
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let out = step(&json!({
            "type": "host_error", "id": req_id(&req),
            "error": {"kind": "permission-denied", "message": "no network"},
        }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "permission-denied");
    }

    #[test]
    fn metadata_orders_by_input_and_repeats_dupes() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [
                {"provider": "itunes", "kind": "track", "id": "900004"},
                {"provider": "itunes", "kind": "track", "id": "1440761789"},
                {"provider": "itunes", "kind": "track", "id": "1440761789"},
                {"provider": "itunes", "kind": "track", "id": "999999"}
            ]}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(
            url.starts_with("https://itunes.apple.com/lookup?id="),
            "{url}"
        );
        let out = step(&http_ok(req_id(&out), LOOKUP));
        assert_eq!(out["type"], "done", "{out}");
        // Input-ref order; dup ref emits twice; genuinely-missing id omitted.
        let ids: Vec<&str> = items_of(&out)
            .iter()
            .filter_map(|i| i["source_ref"]["id"].as_str())
            .collect();
        assert_eq!(ids, ["900004", "1440761789", "1440761789"], "{ids:?}");
        assert_eq!(items_of(&out)[0]["storefront"], Value::Null);
    }

    #[test]
    fn metadata_foreign_ref_fails_before_http() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "youtube", "kind": "track", "id": "x"}]}),
        );
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "not-applicable");
    }

    #[test]
    fn metadata_lookup_404_is_empty_result() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "itunes", "kind": "track", "id": "1"}]}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        let out = step(&http_status(req_id(&out), 404, &[]));
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
    }

    #[test]
    fn artwork_sizes_and_missing_art() {
        for (size, marker) in [(600_u64, "600x600bb"), (1200, "1200x1200bb")] {
            let out = invoke(
                "catalog.artwork",
                json!({
                    "ref": {"provider": "itunes", "kind": "track", "id": "1440761789"},
                    "size": size,
                }),
            );
            assert_eq!(out["kind"], "http_request", "{out}");
            let url = out["payload"]["url"].as_str().unwrap_or_default();
            assert!(url.contains("/lookup?id=1440761789"), "{url}");
            let out = step(&http_ok(req_id(&out), LOOKUP));
            assert_eq!(out["type"], "done", "{out}");
            assert_eq!(out["result"]["source_ref"]["id"], "1440761789");
            let items = items_of(&out);
            assert_eq!(items.len(), 1, "{items:?}");
            assert!(
                items[0]["url"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(marker),
                "{items:?}"
            );
        }

        // A row without artwork yields an honest empty list.
        let out = invoke(
            "catalog.artwork",
            json!({
                "ref": {"provider": "itunes", "kind": "track", "id": "900001"},
                "size": 1200,
            }),
        );
        let out = step(&http_ok(req_id(&out), LOOKUP));
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
    }

    #[test]
    fn artwork_rejects_foreign_ref_and_bad_size_before_http() {
        let out = invoke(
            "catalog.artwork",
            json!({
                "ref": {"provider": "youtube", "kind": "track", "id": "x"},
                "size": 600,
            }),
        );
        assert_eq!(out["error"]["kind"], "not-applicable", "{out}");
        let out = invoke(
            "catalog.artwork",
            json!({
                "ref": {"provider": "itunes", "kind": "track", "id": "1"},
                "size": 300,
            }),
        );
        assert_eq!(out["error"]["kind"], "invalid-response", "{out}");
    }

    /// Every payload object must carry exactly its contract keys —
    /// extras and missing fields are both `invalid-response`.
    #[test]
    fn payload_key_strictness() {
        for payload in [
            json!({"query": "x", "limit": 5, "storefront": null, "extra": 1}),
            json!({"query": "x", "limit": 5}),
            json!({"limit": 5, "storefront": null}),
        ] {
            let out = invoke("catalog.search", payload.clone());
            assert_eq!(out["type"], "fail", "{payload}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{payload}");
        }
        for payload in [
            json!({"refs": [], "x": 1}),
            json!({}),
            json!({"refs": [{"provider": "itunes", "kind": "track", "id": "1", "x": 1}]}),
        ] {
            let out = invoke("catalog.metadata", payload.clone());
            assert_eq!(out["type"], "fail", "{payload}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{payload}");
        }
        for payload in [
            json!({"ref": {"provider": "itunes", "kind": "track", "id": "1"}}),
            json!({"ref": {"provider": "itunes", "kind": "track", "id": "1", "x": 1}, "size": 600}),
            json!({"ref": {"provider": "itunes", "kind": "track", "id": "1"}, "size": 600, "x": 1}),
        ] {
            let out = invoke("catalog.artwork", payload.clone());
            assert_eq!(out["type"], "fail", "{payload}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{payload}");
        }
    }

    #[test]
    fn query_char_cap_is_enforced() {
        let out = invoke(
            "catalog.search",
            json!({"query": "a".repeat(513), "limit": 5, "storefront": null}),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        // 512 characters is the schema cap, counted in scalars — a
        // 512-CJK-character query (1,536 UTF-8 bytes) is legal.
        let cjk = "曲".repeat(512);
        let req = search_request(json!({"query": cjk, "limit": 5, "storefront": null}));
        let out = step(&http_ok(req_id(&req), EMPTY));
        assert_eq!(out["type"], "done");
        let out = invoke(
            "catalog.search",
            json!({"query": "曲".repeat(513), "limit": 5, "storefront": null}),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        // The 512-ASCII boundary still emits a request.
        let req = search_request(json!({"query": "a".repeat(512), "limit": 5, "storefront": null}));
        let out = step(&http_ok(req_id(&req), EMPTY));
        assert_eq!(out["type"], "done");
    }

    /// The ref id is interpolated into a URL: anything that is not
    /// ASCII digits in u64 range is rejected before any request.
    #[test]
    fn ref_id_injection_and_bounds_rejected() {
        for id in [
            "1&country=XX",
            "abc",
            "0",
            "18446744073709551616",
            "12 3",
            "-5",
            "1.5",
            "",
            "007?",
        ] {
            let out = invoke(
                "catalog.metadata",
                json!({"refs": [{"provider": "itunes", "kind": "track", "id": id}]}),
            );
            assert_eq!(out["type"], "fail", "{id}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{id}");
        }
        // u64::MAX is a legal nonzero id and does reach HTTP.
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "itunes", "kind": "track", "id": "18446744073709551615"}]}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        assert!(
            out["payload"]["url"]
                .as_str()
                .unwrap_or_default()
                .contains("id=18446744073709551615"),
            "{out}"
        );
    }

    #[test]
    fn metadata_over_200_refs_rejected_before_http() {
        let refs: Vec<Value> = (1..=201_u64)
            .map(|i| json!({"provider": "itunes", "kind": "track", "id": i.to_string()}))
            .collect();
        let out = invoke("catalog.metadata", json!({"refs": refs}));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "invalid-response");

        let refs: Vec<Value> = (1..=200_u64)
            .map(|i| json!({"provider": "itunes", "kind": "track", "id": i.to_string()}))
            .collect();
        let out = invoke("catalog.metadata", json!({"refs": refs}));
        assert_eq!(out["kind"], "http_request", "{out}");
    }

    #[test]
    fn metadata_blank_upstream_fields_map_to_null() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "itunes", "kind": "track", "id": "900005"}]}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        let out = step(&http_ok(req_id(&out), LOOKUP));
        assert_eq!(out["type"], "done", "{out}");
        let item = &items_of(&out)[0];
        assert_eq!(item["title"], "Blank Fields");
        assert_eq!(item["artist"], Value::Null);
        assert_eq!(item["album"], Value::Null);
        assert_eq!(item["genre"], Value::Null);
    }

    /// Only a delta-seconds `Retry-After` reaches the diagnostic log;
    /// oversized, malformed, and control-containing values are dropped
    /// while the invocation still fails `rate-limit`.
    #[test]
    fn rate_limit_retry_after_is_sanitized() {
        for (value, want) in [
            ("30", "itunes rate-limited retry_after=30"),
            ("99999999999", "itunes rate-limited"),
            ("-5", "itunes rate-limited"),
            ("30\r\nX-Inject: 1", "itunes rate-limited"),
            ("tomorrow", "itunes rate-limited"),
            ("", "itunes rate-limited"),
        ] {
            let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
            let out = step(&http_status(req_id(&req), 429, &[("Retry-After", value)]));
            assert_eq!(out["kind"], "log", "{value}");
            assert_eq!(out["payload"]["message"].as_str(), Some(want), "{value}");
            let out = step(&json!({"type": "host_ok", "id": req_id(&out)}));
            assert_eq!(out["error"]["kind"], "rate-limit", "{value}");
        }
    }

    #[test]
    fn unsupported_capability_is_not_applicable() {
        let out = invoke("playback.resolve", json!({}));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "not-applicable");
    }
}
