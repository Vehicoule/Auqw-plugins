//! Deezer catalog guest: `catalog.search`, `catalog.metadata`, and
//! `catalog.entity` over the public keyless JSON API (ABI 0.3.0).
//!
//! The guest returns raw provider metadata as `trackMetadata` —
//! version labels and candidate scoring are the application's job.
//! `catalog.entity` returns composite pages: an album page carries its
//! track listing; an artist page carries the artist's top tracks plus
//! album list, all as `items` rows under their own `source_ref` kind.
//! `complete` is `false` whenever a page section is truncated or a
//! section request failed; a failed section never fabricates rows.
//! `preview` URLs are never emitted: this guest declares no
//! `playback.*` capability.

mod encode;
mod http;
mod parse;

use auqw_guest_sdk::{export_plugin, log, GuestError, GuestFuture, Invocation, LogLevel};
use serde_json::{json, Map, Value};

const API: &str = "https://api.deezer.com";
/// Deezer caps search pages at 25 rows — larger asks are clamped, not
/// rejected.
const SEARCH_LIMIT_MAX: u64 = 25;
/// Page sizes for the artist composite's two sections.
const ARTIST_TOP_LIMIT: u64 = 50;
const ARTIST_ALBUMS_LIMIT: u64 = 50;

fn dispatch(inv: Invocation) -> GuestFuture {
    Box::pin(async move {
        match inv.capability.as_str() {
            "catalog.search" => search(&inv.payload).await,
            "catalog.metadata" => metadata(&inv.payload).await,
            "catalog.entity" => entity(&inv.payload).await,
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

/// A sanitized warning — never carries bodies, URLs, or upstream text.
async fn warn(message: &str) -> Result<(), GuestError> {
    log(LogLevel::Warn, message).await
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

/// A well-formed ref for this provider, or a typed rejection: the
/// provider must be `deezer` and `kind` one of `kinds`, else
/// `not-applicable`; a malformed object or id is `invalid-response`.
/// The returned id is the validated digit string.
fn deezer_ref(v: &Value, kinds: &[&str]) -> Result<(String, String), GuestError> {
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
    if provider != "deezer" || !kinds.contains(&kind) {
        return Err(failed(
            "not-applicable",
            format!("ref is not a deezer {} ref", kinds.join("/")),
        ));
    }
    // The id becomes a URL path segment: ASCII digits only, in u64
    // range, nonzero — anything else is `invalid-response`, never a
    // request.
    let parsed = if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        id.parse::<u64>().ok()
    } else {
        None
    };
    match parsed {
        Some(n) if n > 0 => Ok((kind.to_string(), id.to_string())),
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
        .clamp(1, SEARCH_LIMIT_MAX);
    // Deezer has no storefront scoping; the parameter is validated for
    // shape and the result is honestly reported as global (`null`).
    let _storefront = storefront_of(obj)?;

    let url = format!(
        "{API}/search?q={}&limit={limit}",
        encode::percent_encode(&query)
    );
    match http::get_json(&url).await? {
        // A missing search endpoint is upstream breakage, not an
        // empty catalog.
        http::Outcome::NotFound => Err(failed("transient", "deezer search status 404".into())),
        http::Outcome::Body(v) => Ok(json!({
            "items": parse::search_items(&v)?,
            "storefront": Value::Null,
        })),
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
    let mut items = Vec::with_capacity(refs.len());
    // Deezer has no batch lookup — one request per ref, in input
    // order. A ref whose resource is absent upstream is omitted, the
    // same way itunes drops genuinely-missing ids.
    for r in refs {
        let (kind, id) = deezer_ref(r, &["track", "album", "artist"])?;
        let url = format!("{API}/{kind}/{id}");
        match http::get_json(&url).await? {
            http::Outcome::NotFound => continue,
            http::Outcome::Body(v) => {
                let o = v.as_object().ok_or_else(|| {
                    failed("invalid-response", "deezer: body is not an object".into())
                })?;
                // A body naming a different id is not this ref's
                // resource — the ref resolves to nothing.
                if !parse::id_matches(o, &id)? {
                    continue;
                }
                let row = match kind.as_str() {
                    "track" => parse::track_row(&v, None, None),
                    "album" => parse::album_row(&v, None),
                    _ => parse::artist_row(&v),
                };
                match row {
                    Some(row) => items.push(parse::to_metadata(&row)),
                    None => {
                        return Err(failed(
                            "invalid-response",
                            format!("deezer: malformed {kind} object"),
                        ));
                    }
                }
            }
        }
    }
    Ok(json!({ "items": items }))
}

async fn entity(payload: &Value) -> Result<Value, GuestError> {
    let obj = payload_obj(payload, &["ref"])?;
    let (kind, id) = deezer_ref(&obj["ref"], &["album", "artist"])?;
    match kind.as_str() {
        "album" => album_entity(&id).await,
        _ => artist_entity(&id).await,
    }
}

async fn album_entity(id: &str) -> Result<Value, GuestError> {
    let v = match http::get_json(&format!("{API}/album/{id}")).await? {
        http::Outcome::NotFound => {
            return Err(failed("no-result", format!("deezer album {id} not found")));
        }
        http::Outcome::Body(v) => v,
    };
    match parse::album_page(&v, id)? {
        Some(page) => Ok(json!({
            "entity": page.entity,
            "items": page.items,
            "complete": page.complete,
        })),
        None => Err(failed("no-result", format!("deezer album {id} not found"))),
    }
}

async fn artist_entity(id: &str) -> Result<Value, GuestError> {
    let v = match http::get_json(&format!("{API}/artist/{id}")).await? {
        http::Outcome::NotFound => {
            return Err(failed("no-result", format!("deezer artist {id} not found")));
        }
        http::Outcome::Body(v) => v,
    };
    let (entity, name) = match parse::artist_meta(&v, id)? {
        Some(m) => m,
        None => {
            return Err(failed("no-result", format!("deezer artist {id} not found")));
        }
    };

    let mut items: Vec<Value> = Vec::new();
    let mut complete = true;

    // Top-tracks section. A section that fails degrades the page to
    // `complete:false` — the rows it would have carried are left
    // empty, never fabricated.
    let top_url = format!("{API}/artist/{id}/top?limit={ARTIST_TOP_LIMIT}");
    match section(&top_url).await? {
        Section::Body(v) => match parse::track_items(&v) {
            Ok(mut list) => {
                if parse::has_next(&v) {
                    complete = false;
                }
                items.append(&mut list);
            }
            Err(_) => {
                complete = false;
                warn("deezer artist top section unavailable").await?;
            }
        },
        Section::Degraded => {
            complete = false;
            warn("deezer artist top section unavailable").await?;
        }
    }

    // Album-list section: `/artist/{id}/albums` rows carry no artist
    // sub-object, so they inherit the page artist's name and id.
    let albums_url = format!("{API}/artist/{id}/albums?limit={ARTIST_ALBUMS_LIMIT}");
    match section(&albums_url).await? {
        Section::Body(v) => match parse::album_items(&v, Some((name.as_str(), id))) {
            Ok(mut list) => {
                if parse::has_next(&v) {
                    complete = false;
                }
                items.append(&mut list);
            }
            Err(_) => {
                complete = false;
                warn("deezer artist albums section unavailable").await?;
            }
        },
        Section::Degraded => {
            complete = false;
            warn("deezer artist albums section unavailable").await?;
        }
    }

    Ok(json!({
        "entity": entity,
        "items": items,
        "complete": complete,
    }))
}

/// The result of a composite-page section fetch.
enum Section {
    /// A parseable 2xx body (not necessarily well-shaped inside).
    Body(Value),
    /// The section is unavailable: not-found, upstream weather, or a
    /// malformed/empty response.
    Degraded,
}

/// Fetch one page section. Terminal host failures
/// (`cancelled`/`permission-denied`/`invalid-response`) and step
/// protocol violations propagate; every other failure — including a
/// guest-side `rate-limit`/`transient`/`invalid-response` verdict — is
/// a degraded section the caller flags `complete:false`.
async fn section(url: &str) -> Result<Section, GuestError> {
    match http::get_json(url).await {
        Ok(http::Outcome::Body(v)) => Ok(Section::Body(v)),
        Ok(http::Outcome::NotFound) => Ok(Section::Degraded),
        Err(e) if terminal(&e) => Err(e),
        Err(_) => Ok(Section::Degraded),
    }
}

/// Errors a partial page must not swallow: host-declared terminal
/// kinds and step-protocol violations. Everything else is weather on
/// one section of a composite page.
fn terminal(e: &GuestError) -> bool {
    match e {
        GuestError::Host { kind, .. } => matches!(
            kind.as_str(),
            "cancelled" | "permission-denied" | "invalid-response"
        ),
        GuestError::InvalidResponse(_) => true,
        GuestError::Failed { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    const SEARCH: &str = include_str!("../fixtures/search.json");
    const EMPTY: &str = include_str!("../fixtures/search-empty.json");
    const MALFORMED: &str = include_str!("../fixtures/search-malformed.json");
    const QUOTA: &str = include_str!("../fixtures/error-quota.json");
    const NO_DATA: &str = include_str!("../fixtures/error-no-data.json");
    const TRACK: &str = include_str!("../fixtures/track.json");
    const ALBUM: &str = include_str!("../fixtures/album.json");
    const ARTIST: &str = include_str!("../fixtures/artist.json");
    const ARTIST_TOP: &str = include_str!("../fixtures/artist-top.json");
    const ARTIST_ALBUMS: &str = include_str!("../fixtures/artist-albums.json");
    const TOP_MALFORMED: &str = include_str!("../fixtures/artist-top-malformed.json");

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

    /// Ack a `log` request and return the next step output.
    fn ack_log(out: &Value) -> Value {
        assert_eq!(out["kind"], "log", "{out}");
        step(&json!({"type": "host_ok", "id": req_id(out)}))
    }

    /// Drive one `host_request` → `http_ok(body)` → follow-on output,
    /// transparently acking any `log` requests in between. Returns the
    /// first non-host_request output after the response is fed.
    fn feed(req: &Value, body: &str) -> Value {
        let mut out = step(&http_ok(req_id(req), body));
        while out["type"] == "host_request" && out["kind"] == "log" {
            out = step(&json!({"type": "host_ok", "id": req_id(&out)}));
        }
        out
    }

    #[test]
    fn search_request_is_encoded_and_bounded() {
        let out = search_request(json!({
            "query": "Roads & 夜", "limit": 500, "storefront": "us",
        }));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(url.starts_with("https://api.deezer.com/search?"), "{url}");
        assert!(url.contains("q=Roads%20%26%20%E5%A4%9C"), "{url}");
        assert!(url.contains("limit=25"), "{url}");
        assert_eq!(out["payload"]["method"], "GET");
        let headers = out["payload"]["headers"].to_string();
        assert!(headers.contains("Auqw/0.1"), "{headers}");
    }

    #[test]
    fn search_limit_floor() {
        let out = search_request(json!({"query": "x", "limit": 0, "storefront": null}));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(url.contains("limit=1"), "{url}");
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
    fn search_fixture_maps_rows() {
        let req = search_request(json!({"query": "x", "limit": 10, "storefront": "US"}));
        let out = step(&http_ok(req_id(&req), SEARCH));
        assert_eq!(out["type"], "done", "{out}");
        let items = items_of(&out);
        // Bad rows dropped: zero-id, non-track type, title-less, the
        // string literal, and the duplicate id collapse.
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(out["result"]["storefront"], Value::Null);

        let first = &items[0];
        assert_eq!(first["title"], "Roads");
        assert_eq!(first["artist"], "Portishead");
        assert_eq!(first["album"], "Dummy");
        assert_eq!(first["duration_ms"], 303_000);
        // Search rows carry no release_date — honest null.
        assert_eq!(first["release_year"], Value::Null);
        assert_eq!(first["explicit"], false);
        assert_eq!(first["isrc"], "GBAQT9400064");
        assert_eq!(first["source_ref"]["provider"], "deezer");
        assert_eq!(first["source_ref"]["kind"], "track");
        assert_eq!(first["source_ref"]["id"], "982668");
        assert_eq!(first["artist_ref"]["kind"], "artist");
        assert_eq!(first["artist_ref"]["id"], "1069");
        assert_eq!(first["album_ref"]["kind"], "album");
        assert_eq!(first["album_ref"]["id"], "109301");
        let art = &first["artwork"][0];
        assert_eq!(
            art["url"].as_str().unwrap_or_default(),
            "https://cdn-images.dzcdn.net/images/cover/5942b88996f33c82790023d5d99395d3/1000x1000-000000-80-0-0.jpg"
        );
        assert_eq!(art["width"], 1000);
        assert_eq!(art["height"], 1000);

        let live = items
            .iter()
            .find(|i| i["title"] == "Roads (Live)")
            .unwrap_or_else(|| panic!("missing live item"));
        assert_eq!(live["title_version"], Value::Null, "no leak of raw keys");
        let explicit = items
            .iter()
            .find(|i| i["title"] == "Explicit Cut")
            .unwrap_or_else(|| panic!("missing explicit item"));
        assert_eq!(explicit["explicit"], true);
        let no_art = items
            .iter()
            .find(|i| i["title"] == "Quiet Row")
            .unwrap_or_else(|| panic!("missing no-art item"));
        assert_eq!(no_art["artwork"].as_array().map(Vec::len), Some(0));
        assert_eq!(no_art["isrc"], Value::Null);

        // Preview URLs never cross into the result.
        let s = out["result"].to_string();
        assert!(!s.contains("preview") && !s.contains("cdnt-preview"), "{s}");
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
    fn rate_limit_status_logs_then_fails() {
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let out = step(&http_status(req_id(&req), 429, &[("Retry-After", "30")]));
        // The retry hint goes to the diagnostic log, not the result.
        assert_eq!(out["kind"], "log", "{out}");
        let msg = out["payload"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("retry_after=30"), "{msg}");
        let out = ack_log(&out);
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "rate-limit");
    }

    #[test]
    fn quota_envelope_is_rate_limit() {
        // Deezer answers quota exhaustion as 200 + an error envelope.
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let out = step(&http_ok(req_id(&req), QUOTA));
        let out = ack_log(&out);
        assert_eq!(out["type"], "fail", "{out}");
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
    fn unknown_error_envelope_is_transient() {
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let out = step(&http_ok(
            req_id(&req),
            r#"{"error":{"type":"ParameterException","message":"bad parameter","code":501}}"#,
        ));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "transient");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("501"),
            "{out}"
        );
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
    fn metadata_dispatches_per_kind_in_order() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [
                {"provider": "deezer", "kind": "track", "id": "982668"},
                {"provider": "deezer", "kind": "album", "id": "109301"},
                {"provider": "deezer", "kind": "artist", "id": "1069"},
                {"provider": "deezer", "kind": "track", "id": "404404"},
                {"provider": "deezer", "kind": "track", "id": "982668"},
            ]}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/track/982668", "{url}");

        let out = step(&http_ok(req_id(&out), TRACK));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/album/109301", "{url}");

        let out = step(&http_ok(req_id(&out), ALBUM));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/artist/1069", "{url}");

        let out = step(&http_ok(req_id(&out), ARTIST));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/track/404404", "{url}");

        // The missing ref resolves to a DataException → omitted.
        let out = step(&http_ok(req_id(&out), NO_DATA));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/track/982668", "{url}");

        let out = step(&http_ok(req_id(&out), TRACK));
        assert_eq!(out["type"], "done", "{out}");
        let items = items_of(&out);
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(items[0]["source_ref"]["kind"], "track");
        assert_eq!(items[0]["isrc"], "GBAQT9400064");
        assert_eq!(items[0]["release_year"], 1994);
        assert_eq!(items[1]["source_ref"]["kind"], "album");
        assert_eq!(items[1]["title"], "Dummy");
        assert_eq!(items[1]["genre"], "Rock");
        assert_eq!(items[2]["source_ref"]["kind"], "artist");
        assert_eq!(items[2]["title"], "Portishead");
        assert_eq!(items[3]["source_ref"]["id"], "982668");
    }

    #[test]
    fn metadata_foreign_ref_fails_before_http() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "itunes", "kind": "track", "id": "1"}]}),
        );
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "not-applicable");
    }

    #[test]
    fn metadata_wrong_kind_fails_not_applicable() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "deezer", "kind": "episode", "id": "1"}]}),
        );
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "not-applicable");
    }

    /// The ref id is interpolated into a URL path: anything that is
    /// not ASCII digits in u64 range is rejected before any request.
    #[test]
    fn ref_id_injection_and_bounds_rejected() {
        for id in [
            "1/tracks",
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
                json!({"refs": [{"provider": "deezer", "kind": "track", "id": id}]}),
            );
            assert_eq!(out["type"], "fail", "{id}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{id}");
        }
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "deezer", "kind": "track", "id": "18446744073709551615"}]}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        assert!(
            out["payload"]["url"]
                .as_str()
                .unwrap_or_default()
                .ends_with("/track/18446744073709551615"),
            "{out}"
        );
    }

    #[test]
    fn metadata_over_200_refs_rejected_before_http() {
        let refs: Vec<Value> = (1..=201_u64)
            .map(|i| json!({"provider": "deezer", "kind": "track", "id": i.to_string()}))
            .collect();
        let out = invoke("catalog.metadata", json!({"refs": refs}));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "invalid-response");
    }

    #[test]
    fn metadata_id_mismatch_omits_ref() {
        // Upstream answered a different resource: the ref resolves to
        // nothing rather than trusting the foreign body.
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [{"provider": "deezer", "kind": "track", "id": "111"}]}),
        );
        let out = step(&http_ok(req_id(&out), TRACK));
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
    }

    #[test]
    fn entity_album_carries_tracks() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "album", "id": "109301"}}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/album/109301", "{url}");
        let out = step(&http_ok(req_id(&out), ALBUM));
        assert_eq!(out["type"], "done", "{out}");

        let entity = &out["result"]["entity"];
        assert_eq!(entity["kind"], "album");
        assert_eq!(entity["title"], "Dummy");
        assert_eq!(entity["subtitle"], "Portishead");
        assert_eq!(entity["source_ref"]["kind"], "album");
        assert_eq!(entity["artwork"][0]["width"], 1000);

        let items = items_of(&out);
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[0]["title"], "Mysterons");
        assert_eq!(items[0]["genre"], "Rock");
        assert_eq!(items[0]["release_year"], 1994);
        assert_eq!(items[0]["album_ref"]["id"], "109301");
        assert_eq!(items[0]["artist_ref"]["id"], "1069");
        assert_eq!(out["result"]["complete"], true);
    }

    #[test]
    fn entity_artist_carries_top_and_albums() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "artist", "id": "1069"}}),
        );
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/artist/1069", "{url}");

        let out = step(&http_ok(req_id(&out), ARTIST));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(
            url,
            format!("https://api.deezer.com/artist/1069/top?limit={ARTIST_TOP_LIMIT}"),
            "{url}"
        );

        let out = step(&http_ok(req_id(&out), ARTIST_TOP));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(
            url,
            format!("https://api.deezer.com/artist/1069/albums?limit={ARTIST_ALBUMS_LIMIT}"),
            "{url}"
        );

        let out = step(&http_ok(req_id(&out), ARTIST_ALBUMS));
        assert_eq!(out["type"], "done", "{out}");

        let entity = &out["result"]["entity"];
        assert_eq!(entity["kind"], "artist");
        assert_eq!(entity["title"], "Portishead");
        assert_eq!(entity["subtitle"], "15 albums");
        assert_eq!(entity["artwork"][0]["width"], 1000);

        let items = items_of(&out);
        // Top tracks first, then the album list — each under its own
        // source_ref kind.
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(items[0]["source_ref"]["kind"], "track");
        assert_eq!(items[0]["title"], "Glory Box");
        assert_eq!(items[1]["source_ref"]["kind"], "track");
        assert_eq!(items[2]["source_ref"]["kind"], "album");
        assert_eq!(items[2]["title"], "Third");
        assert_eq!(items[2]["artist"], "Portishead");
        assert_eq!(items[2]["artist_ref"]["id"], "1069");
        assert_eq!(items[3]["source_ref"]["kind"], "album");
        assert_eq!(out["result"]["complete"], true);
    }

    /// A failed page section degrades the composite to
    /// `complete:false`; the surviving sections still report — the
    /// missing one is empty, never fabricated.
    #[test]
    fn entity_artist_partial_composite_degrades() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "artist", "id": "1069"}}),
        );
        let out = feed(&out, ARTIST);
        // Top tracks answer a quota error envelope → degraded, and
        // the albums request still goes out.
        let out = step(&http_ok(req_id(&out), QUOTA));
        let out = ack_log(&out); // rate-limit diagnostic
        let out = ack_log(&out); // section-unavailable diagnostic
        assert_eq!(out["kind"], "http_request", "{out}");
        assert!(
            out["payload"]["url"]
                .as_str()
                .unwrap_or_default()
                .contains("/albums?"),
            "{out}"
        );
        let out = step(&http_ok(req_id(&out), ARTIST_ALBUMS));
        assert_eq!(out["type"], "done", "{out}");
        let items = items_of(&out);
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(items.iter().all(|i| i["source_ref"]["kind"] == "album"));
        assert_eq!(out["result"]["complete"], false);
    }

    /// A malformed section body degrades the same way as a failed one.
    #[test]
    fn entity_artist_malformed_section_degrades() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "artist", "id": "1069"}}),
        );
        let out = feed(&out, ARTIST);
        let out = feed(&out, TOP_MALFORMED);
        // Malformed top → warn logged, albums still fetched.
        assert_eq!(out["kind"], "http_request", "{out}");
        let out = feed(&out, ARTIST_ALBUMS);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert_eq!(items_of(&out).len(), 2);
    }

    /// A `next` page marker truncates the composite — flagged, never
    /// silently partial.
    #[test]
    fn entity_artist_next_marks_incomplete() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "artist", "id": "1069"}}),
        );
        let out = feed(&out, ARTIST);
        let out = feed(
            &out,
            r#"{"data":[{"id":1,"type":"track","title":"T","artist":{"id":1069,"name":"Portishead"}}],"total":99,"next":"https://api.deezer.com/artist/1069/top?index=1"}"#,
        );
        let out = feed(&out, ARTIST_ALBUMS);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert_eq!(items_of(&out).len(), 3);
    }

    /// Terminal host errors are not section weather — a
    /// `permission-denied` on a section fetch fails the invocation.
    #[test]
    fn entity_artist_terminal_host_error_propagates() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "artist", "id": "1069"}}),
        );
        let out = feed(&out, ARTIST);
        let out = step(&json!({
            "type": "host_error", "id": req_id(&out),
            "error": {"kind": "permission-denied", "message": "denied"},
        }));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "permission-denied");
    }

    /// Non-terminal host weather on a section degrades instead.
    #[test]
    fn entity_artist_host_weather_degrades() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "artist", "id": "1069"}}),
        );
        let out = feed(&out, ARTIST);
        let out = step(&json!({
            "type": "host_error", "id": req_id(&out),
            "error": {"kind": "timeout", "message": "slow"},
        }));
        let out = ack_log(&out);
        assert_eq!(out["kind"], "http_request", "{out}");
        let out = feed(&out, ARTIST_ALBUMS);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
    }

    #[test]
    fn entity_album_missing_is_no_result() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "album", "id": "42"}}),
        );
        let out = step(&http_ok(req_id(&out), NO_DATA));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "no-result");
    }

    #[test]
    fn entity_artist_missing_is_no_result() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "artist", "id": "42"}}),
        );
        let out = step(&http_ok(req_id(&out), NO_DATA));
        assert_eq!(out["error"]["kind"], "no-result");
    }

    /// A truncated album track listing flags `complete:false`.
    #[test]
    fn entity_album_short_tracks_incomplete() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "album", "id": "109301"}}),
        );
        let body = r#"{"id":109301,"type":"album","title":"Dummy","nb_tracks":11,
            "tracks":{"data":[{"id":1,"type":"track","title":"Only One"}]}}"#;
        let out = step(&http_ok(req_id(&out), body));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert_eq!(items_of(&out).len(), 1);
    }

    /// An album body with no `tracks` section still yields the entity
    /// — degraded, not fabricated.
    #[test]
    fn entity_album_missing_tracks_degrades() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "album", "id": "109301"}}),
        );
        let body = r#"{"id":109301,"type":"album","title":"Dummy","nb_tracks":11}"#;
        let out = step(&http_ok(req_id(&out), body));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert!(items_of(&out).is_empty());
    }

    /// Track refs are valid `sourceRef`s but not `entityRef`s —
    /// `catalog.entity` cannot serve them.
    #[test]
    fn entity_track_ref_is_not_applicable() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "track", "id": "982668"}}),
        );
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "not-applicable");
    }

    #[test]
    fn entity_foreign_ref_is_not_applicable() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "itunes", "kind": "album", "id": "1"}}),
        );
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "not-applicable");
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
            json!({"refs": [{"provider": "deezer", "kind": "track", "id": "1", "x": 1}]}),
        ] {
            let out = invoke("catalog.metadata", payload.clone());
            assert_eq!(out["type"], "fail", "{payload}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{payload}");
        }
        for payload in [
            json!({}),
            json!({"ref": {"provider": "deezer", "kind": "album", "id": "1"}, "x": 1}),
            json!({"ref": {"provider": "deezer", "kind": "album", "id": "1", "x": 1}}),
        ] {
            let out = invoke("catalog.entity", payload.clone());
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
        // 512 scalars is the cap, counted in characters — a 512-CJK-
        // character query is legal.
        let req =
            search_request(json!({"query": "曲".repeat(512), "limit": 5, "storefront": null}));
        let out = step(&http_ok(req_id(&req), EMPTY));
        assert_eq!(out["type"], "done");
    }

    /// Only a delta-seconds `Retry-After` reaches the diagnostic log;
    /// oversized, malformed, and control-containing values are dropped
    /// while the invocation still fails `rate-limit`.
    #[test]
    fn rate_limit_retry_after_is_sanitized() {
        for (value, want) in [
            ("30", "deezer rate-limited retry_after=30"),
            ("99999999999", "deezer rate-limited"),
            ("-5", "deezer rate-limited"),
            ("30\r\nX-Inject: 1", "deezer rate-limited"),
            ("tomorrow", "deezer rate-limited"),
            ("", "deezer rate-limited"),
        ] {
            let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
            let out = step(&http_status(req_id(&req), 429, &[("Retry-After", value)]));
            assert_eq!(out["kind"], "log", "{value}");
            assert_eq!(out["payload"]["message"].as_str(), Some(want), "{value}");
            let out = ack_log(&out);
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
