//! LRCLIB lyrics guest: `lyrics.plain` and `lyrics.synced` over the
//! keyless `https://lrclib.net` API (ABI 0.3.0).
//!
//! Seven-tier fallback waterfall (providers.md): exact `/api/get`
//! with duration → without duration → cleaned-metadata get → scoped
//! `/api/search` with album → scoped search without album → general
//! query → ASCII-normalized query. A 404 is a tier miss that advances
//! the waterfall; a record that lacks the requested flavor counts as
//! a miss too — a later tier may hold a usable record. A 429 fails
//! closed `rate-limit` (never a retry storm); a malformed upstream
//! body fails `invalid-response`.
//!
//! Synced results are parsed LRC `[mm:ss.xx]` lines; plain results
//! never carry timing; an `instrumental` record never carries text.
//! The `matched` field reports the upstream record's own metadata —
//! acceptance scoring is the application's job, not the guest's.

mod encode;
mod http;
mod parse;

use auqw_guest_sdk::{export_plugin, GuestError, GuestFuture, Invocation};
use serde_json::{json, Map, Value};

use parse::Record;

const API: &str = "https://lrclib.net";

fn dispatch(inv: Invocation) -> GuestFuture {
    Box::pin(async move {
        match inv.capability.as_str() {
            "lyrics.plain" => lyrics(&inv.payload, Flavor::Plain).await,
            "lyrics.synced" => lyrics(&inv.payload, Flavor::Synced).await,
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

/// Which result shape the invocation asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Plain,
    Synced,
}

/// A validated `lyricsQuery`: the recording the caller wants lyrics
/// for. `isrc` is validated then dropped — LRCLIB takes no ISRC
/// parameter.
struct Query {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<u64>,
}

fn parse_query(payload: &Value) -> Result<Query, GuestError> {
    let obj = payload_obj(payload, &["query"])?;
    let q = payload_obj(
        &obj["query"],
        &["title", "artist", "album", "duration_ms", "isrc"],
    )?;
    let title = q["title"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().count() <= 512)
        .ok_or_else(|| {
            bad_payload("query.title must be a nonempty string of at most 512 characters")
        })?
        .to_string();
    let artist = opt_str(q, "artist")?;
    let album = opt_str(q, "album")?;
    let duration_ms = match &q["duration_ms"] {
        Value::Null => None,
        v => Some(
            v.as_u64()
                .ok_or_else(|| bad_payload("query.duration_ms must be an integer or null"))?,
        ),
    };
    opt_str(q, "isrc")?;
    Ok(Query {
        title,
        artist,
        album,
        duration_ms,
    })
}

/// A nullable string field: `null` is absent, a blank string is a
/// malformed payload, anything else is trimmed and kept.
fn opt_str(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, GuestError> {
    match &obj[key] {
        Value::Null => Ok(None),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Err(bad_payload("query fields must be nonempty or null"))
            } else {
                Ok(Some(t.to_string()))
            }
        }
        _ => Err(bad_payload("query fields must be strings or null")),
    }
}

/// One waterfall request: `/api/get` tiers answer a single record,
/// `/api/search` tiers answer an array the picker chooses from.
enum TierKind {
    Get,
    Search,
}

struct Tier {
    kind: TierKind,
    url: String,
}

/// The seven tiers in order. Tiers whose parameters cannot be formed
/// (no artist for `/api/get`, no album for the album-scoped search)
/// are skipped, and a URL already issued is never repeated — a
/// duration-less query makes tier 1 identical to tier 2, an
/// ASCII-only query makes tier 7 identical to tier 6.
fn tiers(q: &Query) -> Vec<Tier> {
    let mut out: Vec<Tier> = Vec::new();
    let push = |kind: TierKind, url: String, out: &mut Vec<Tier>| {
        if !out.iter().any(|t: &Tier| t.url == url) {
            out.push(Tier { kind, url });
        }
    };
    let duration_secs = q
        .duration_ms
        .filter(|d| *d > 0)
        .map(|d| d.saturating_add(500) / 1000);
    if let Some(artist) = &q.artist {
        let mut exact = format!(
            "{API}/api/get?track_name={}&artist_name={}",
            encode::percent_encode(&q.title),
            encode::percent_encode(artist)
        );
        if let Some(album) = &q.album {
            exact.push_str(&format!("&album_name={}", encode::percent_encode(album)));
        }
        // 1. Exact get with duration.
        if let Some(d) = duration_secs {
            push(TierKind::Get, format!("{exact}&duration={d}"), &mut out);
        }
        // 2. Exact get without duration.
        push(TierKind::Get, exact.clone(), &mut out);
        // 3. Cleaned-metadata get: same signature, version-suffix-free
        //    title.
        let cleaned = parse::clean_title(&q.title);
        if cleaned != q.title {
            let mut url = format!(
                "{API}/api/get?track_name={}&artist_name={}",
                encode::percent_encode(&cleaned),
                encode::percent_encode(artist)
            );
            if let Some(album) = &q.album {
                url.push_str(&format!("&album_name={}", encode::percent_encode(album)));
            }
            if let Some(d) = duration_secs {
                url.push_str(&format!("&duration={d}"));
            }
            push(TierKind::Get, url, &mut out);
        }
    }
    // 4. Scoped search with album.
    if let Some(album) = &q.album {
        let mut url = format!(
            "{API}/api/search?track_name={}&album_name={}",
            encode::percent_encode(&q.title),
            encode::percent_encode(album)
        );
        if let Some(artist) = &q.artist {
            url.push_str(&format!("&artist_name={}", encode::percent_encode(artist)));
        }
        push(TierKind::Search, url, &mut out);
    }
    // 5. Scoped search without album.
    if let Some(artist) = &q.artist {
        push(
            TierKind::Search,
            format!(
                "{API}/api/search?track_name={}&artist_name={}",
                encode::percent_encode(&q.title),
                encode::percent_encode(artist)
            ),
            &mut out,
        );
    }
    // 6. General query.
    let general = match &q.artist {
        Some(artist) => format!("{} {artist}", q.title),
        None => q.title.clone(),
    };
    push(
        TierKind::Search,
        format!("{API}/api/search?q={}", encode::percent_encode(&general)),
        &mut out,
    );
    // 7. ASCII-normalized general query — only a new request when the
    //    fold actually changes the query text.
    if let Some(ascii) = encode::ascii_fold(&general) {
        if ascii != general {
            push(
                TierKind::Search,
                format!("{API}/api/search?q={}", encode::percent_encode(&ascii)),
                &mut out,
            );
        }
    }
    out
}

/// What one picked record means for the requested flavor: `Some` is
/// the terminal `done` result, `None` is a contentless record — the
/// waterfall keeps looking.
fn record_result(rec: &Record, flavor: Flavor, matched: &Value) -> Option<Value> {
    // The flag is authoritative: an instrumental record answers
    // honestly and carries no text, whatever lyrics fields it holds.
    if rec.instrumental {
        return Some(match flavor {
            Flavor::Plain => json!({"state": "instrumental", "text": null, "matched": matched}),
            Flavor::Synced => {
                json!({"state": "instrumental", "lines": null, "matched": matched})
            }
        });
    }
    match flavor {
        Flavor::Plain => rec
            .plain
            .clone()
            .or_else(|| rec.synced.as_deref().and_then(plain_from_synced))
            .map(|text| json!({"state": "plain", "text": text, "matched": matched})),
        Flavor::Synced => rec
            .synced
            .as_deref()
            .map(parse::parse_lrc)
            .filter(|lines| !lines.is_empty())
            .map(|lines| {
                json!({
                    "state": "synced",
                    "lines": lines
                        .iter()
                        .map(|(t, text)| json!({"t_ms": t, "text": text}))
                        .collect::<Vec<Value>>(),
                    "matched": matched,
                })
            }),
    }
}

/// Plain text derived from synced lyrics: the timestamped lines'
/// texts joined in order — the same words with the timing removed.
/// `None` when nothing timestamped parses.
fn plain_from_synced(synced: &str) -> Option<String> {
    let lines = parse::parse_lrc(synced);
    if lines.is_empty() {
        return None;
    }
    let text = lines
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

async fn lyrics(payload: &Value, flavor: Flavor) -> Result<Value, GuestError> {
    let q = parse_query(payload)?;
    // The first usable record seen — its `matched` rides the `absent`
    // result so the app can score a near-match that had no lyrics.
    let mut first_matched = Value::Null;
    for tier in tiers(&q) {
        let body = match http::get_json(&tier.url).await? {
            http::Outcome::NotFound => continue,
            http::Outcome::Body(body) => body,
        };
        let get_record;
        let search_records;
        let record: &Record = match tier.kind {
            TierKind::Get => {
                get_record = parse::parse_get(&body)?;
                &get_record
            }
            TierKind::Search => {
                search_records = parse::parse_search(&body)?;
                match parse::pick(&search_records, &q.title, q.artist.as_deref()) {
                    Some(r) => r,
                    None => continue,
                }
            }
        };
        if first_matched.is_null() {
            first_matched = record.matched();
        }
        if let Some(result) = record_result(record, flavor, &record.matched()) {
            return Ok(result);
        }
    }
    Ok(match flavor {
        Flavor::Plain => {
            json!({"state": "absent", "text": null, "matched": first_matched})
        }
        Flavor::Synced => {
            json!({"state": "absent", "lines": null, "matched": first_matched})
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;

    const GET_SYNCED: &str = include_str!("../fixtures/get-synced.json");
    const GET_PLAIN: &str = include_str!("../fixtures/get-plain.json");
    const GET_INSTRUMENTAL: &str = include_str!("../fixtures/get-instrumental.json");
    const GET_REMASTERED: &str = include_str!("../fixtures/get-remastered.json");
    const SEARCH_HIT: &str = include_str!("../fixtures/search-hit.json");
    const MISS_404: &str = include_str!("../fixtures/miss-404.json");
    const MALFORMED: &str = include_str!("../fixtures/malformed.json");

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

    fn url_of(out: &Value) -> String {
        assert_eq!(out["type"], "host_request", "{out}");
        assert_eq!(out["kind"], "http_request", "{out}");
        out["payload"]["url"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn http_status(id: u64, status: u16) -> Value {
        json!({
            "type": "http_response", "id": id, "status": status,
            "headers": [], "body": "",
        })
    }

    fn http_status_body(id: u64, status: u16, body: &str) -> Value {
        json!({
            "type": "http_response", "id": id, "status": status,
            "headers": [], "body": B64.encode(body),
        })
    }

    fn query() -> Value {
        json!({
            "title": "Roads",
            "artist": "Portishead",
            "album": "Dummy",
            "duration_ms": 307_000,
            "isrc": null,
        })
    }

    fn invoke_request(cap: &str, q: &Value) -> Value {
        let out = invoke(cap, json!({ "query": q }));
        assert_eq!(out["type"], "host_request", "{out}");
        out
    }

    /// Answer the pending request with `status`/`body` and return the
    /// guest's next output.
    fn answer(out: &Value, status: u16, body: &str) -> Value {
        if body.is_empty() {
            step(&http_status(req_id(out), status))
        } else {
            step(&http_status_body(req_id(out), status, body))
        }
    }

    /// Drive `404` misses through every tier, collecting the URLs the
    /// guest asked for in order; returns (urls, terminal output).
    fn miss_all(out: Value, limit: usize) -> (Vec<String>, Value) {
        let mut urls = vec![url_of(&out)];
        let mut cur = out;
        for _ in 0..limit {
            let next = answer(&cur, 404, MISS_404);
            if next["type"] != "host_request" {
                return (urls, next);
            }
            urls.push(url_of(&next));
            cur = next;
        }
        (urls, answer(&cur, 404, MISS_404))
    }

    #[test]
    fn synced_hit_parses_lrc_lines() {
        let req = invoke_request("lyrics.synced", &query());
        assert_eq!(
            url_of(&req),
            "https://lrclib.net/api/get?track_name=Roads&artist_name=Portishead\
             &album_name=Dummy&duration=307"
        );
        let out = answer(&req, 200, GET_SYNCED);
        assert_eq!(out["type"], "done", "{out}");
        let r = &out["result"];
        assert_eq!(r["state"], "synced");
        let lines = r["lines"].as_array().unwrap_or_else(|| panic!("{r}"));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["t_ms"], 17_120);
        assert_eq!(lines[0]["text"], "Oh, can't anybody see");
        assert_eq!(lines[1]["t_ms"], 30_500);
        assert_eq!(lines[2]["text"], "", "{lines:?}");
        let m = &r["matched"];
        assert_eq!(m["title"], "Roads");
        assert_eq!(m["artist"], "Portishead");
        assert_eq!(m["album"], "Dummy");
        assert_eq!(m["duration_ms"], 307_000);
    }

    #[test]
    fn plain_hit_returns_text() {
        let req = invoke_request("lyrics.plain", &query());
        let out = answer(&req, 200, GET_PLAIN);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["state"], "plain");
        let text = out["result"]["text"].as_str().unwrap_or_default();
        assert!(text.contains("wondering"), "{text}");
        assert!(!text.contains("[00:"), "{text}");
    }

    #[test]
    fn plain_derives_text_from_synced_when_no_plain() {
        // The record carries only syncedLyrics — a plain answer is the
        // same words with timing stripped, never a synced shape.
        let req = invoke_request("lyrics.plain", &query());
        let out = answer(&req, 200, GET_SYNCED);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["state"], "plain");
        let text = out["result"]["text"].as_str().unwrap_or_default();
        assert!(text.contains("Oh, can't anybody see"), "{text}");
        assert!(!text.contains('['), "{text}");
    }

    #[test]
    fn synced_request_absent_on_plain_only_record() {
        // A record without syncedLyrics is contentless for the synced
        // flavor — the waterfall keeps looking, then reports absent
        // with the near-match metadata still carried.
        let req = invoke_request("lyrics.synced", &query());
        let out = answer(&req, 200, GET_PLAIN);
        let (urls, out) = miss_all(out, 10);
        assert_eq!(urls.len(), 4, "{urls:?}");
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["state"], "absent");
        assert_eq!(out["result"]["lines"], Value::Null);
        // The plain-only record was the first usable hit: its metadata
        // rides the absent result.
        assert_eq!(out["result"]["matched"]["title"], "Roads");
    }

    #[test]
    fn instrumental_reports_no_text() {
        for (cap, key) in [("lyrics.plain", "text"), ("lyrics.synced", "lines")] {
            let req = invoke_request(cap, &query());
            let out = answer(&req, 200, GET_INSTRUMENTAL);
            assert_eq!(out["type"], "done", "{cap}: {out}");
            assert_eq!(out["result"]["state"], "instrumental", "{cap}");
            assert_eq!(out["result"][key], Value::Null, "{cap}");
            // The fixture carries lyrics text anyway — none of it may
            // leak into the result.
            let s = out["result"].to_string();
            assert!(!s.contains("violin"), "{cap}: {s}");
            assert_eq!(out["result"]["matched"]["title"], "Roads");
        }
    }

    #[test]
    fn remastered_title_falls_back_to_cleaned_get() {
        let mut q = query();
        q["title"] = json!("Roads (Remastered 2011)");
        let req = invoke_request("lyrics.synced", &q);
        assert!(
            url_of(&req).contains("track_name=Roads%20%28Remastered%202011%29"),
            "{req}"
        );
        let out = answer(&req, 404, MISS_404);
        assert!(
            url_of(&out).contains("track_name=Roads%20%28Remastered%202011%29"),
            "{out}"
        );
        let out = answer(&out, 404, MISS_404);
        // Tier 3 is the cleaned-metadata get.
        assert!(url_of(&out).contains("/api/get?"), "{out}");
        assert!(url_of(&out).contains("track_name=Roads&"), "{out}");
        let out = answer(&out, 200, GET_REMASTERED);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["state"], "synced");
        assert_eq!(out["result"]["matched"]["title"], "Roads");
    }

    #[test]
    fn waterfall_tier_order_full_query() {
        let mut q = query();
        q["artist"] = json!("Sigur Rós");
        let req = invoke_request("lyrics.plain", &q);
        let (urls, out) = miss_all(req, 10);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["state"], "absent");
        assert_eq!(out["result"]["matched"], Value::Null);
        assert_eq!(
            urls,
            [
                "https://lrclib.net/api/get?track_name=Roads&artist_name=Sigur%20R%C3%B3s&album_name=Dummy&duration=307",
                "https://lrclib.net/api/get?track_name=Roads&artist_name=Sigur%20R%C3%B3s&album_name=Dummy",
                "https://lrclib.net/api/search?track_name=Roads&album_name=Dummy&artist_name=Sigur%20R%C3%B3s",
                "https://lrclib.net/api/search?track_name=Roads&artist_name=Sigur%20R%C3%B3s",
                "https://lrclib.net/api/search?q=Roads%20Sigur%20R%C3%B3s",
                "https://lrclib.net/api/search?q=Roads%20Sigur%20Ros",
            ],
            "{urls:?}"
        );
    }

    #[test]
    fn waterfall_skips_unformable_tiers() {
        // No artist: the /api/get tiers cannot form (artist_name is
        // required upstream) and the artist-scoped search is gone —
        // album-scoped, general, and folded-general tiers remain.
        let req = invoke_request(
            "lyrics.plain",
            &json!({
                "title": "Rós", "artist": null, "album": "X",
                "duration_ms": 60_000, "isrc": null,
            }),
        );
        let (urls, out) = miss_all(req, 10);
        assert_eq!(out["result"]["state"], "absent");
        assert_eq!(
            urls,
            [
                "https://lrclib.net/api/search?track_name=R%C3%B3s&album_name=X",
                "https://lrclib.net/api/search?q=R%C3%B3s",
                "https://lrclib.net/api/search?q=Ros",
            ],
            "{urls:?}"
        );
    }

    #[test]
    fn waterfall_no_optional_fields_is_one_general_query() {
        let req = invoke_request(
            "lyrics.plain",
            &json!({
                "title": "Roads", "artist": null, "album": null,
                "duration_ms": null, "isrc": null,
            }),
        );
        assert_eq!(url_of(&req), "https://lrclib.net/api/search?q=Roads");
        let out = answer(&req, 404, MISS_404);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["state"], "absent");
        assert_eq!(out["result"]["matched"], Value::Null);
    }

    #[test]
    fn waterfall_no_duration_starts_without_duration_param() {
        let mut q = query();
        q["duration_ms"] = Value::Null;
        let req = invoke_request("lyrics.plain", &q);
        let url = url_of(&req);
        assert!(url.contains("/api/get?"), "{url}");
        assert!(!url.contains("duration="), "{url}");
    }

    #[test]
    fn search_tier_picks_best_record() {
        let mut q = query();
        q["duration_ms"] = Value::Null;
        let req = invoke_request("lyrics.synced", &q);
        let out = answer(&req, 404, MISS_404); // tier 2 miss
                                               // Tier 4: album-scoped search; fixture's first row is a
                                               // different song, the title match wins.
        let out = answer(&out, 200, SEARCH_HIT);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["state"], "synced");
        assert_eq!(out["result"]["matched"]["title"], "Roads");
        let lines = out["result"]["lines"]
            .as_array()
            .unwrap_or_else(|| panic!("lines"));
        assert!(!lines.is_empty());
    }

    #[test]
    fn malformed_get_body_is_invalid_response() {
        for body in [MALFORMED, "not json {"] {
            let req = invoke_request("lyrics.plain", &query());
            let out = answer(&req, 200, body);
            assert_eq!(out["type"], "fail", "{body}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{body}");
        }
    }

    #[test]
    fn malformed_search_body_is_invalid_response() {
        let mut q = query();
        q["duration_ms"] = Value::Null;
        let req = invoke_request("lyrics.plain", &q);
        let out = answer(&req, 404, MISS_404); // get miss → search tier
        let out = answer(&out, 200, MALFORMED);
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "invalid-response", "{out}");
    }

    #[test]
    fn rate_limit_logs_once_then_fails_closed() {
        let req = invoke_request("lyrics.plain", &query());
        let out = step(&json!({
            "type": "http_response", "id": req_id(&req), "status": 429,
            "headers": [["Retry-After", "30"]], "body": "",
        }));
        // One sanitized diagnostic, then the terminal fail — never a
        // retry loop.
        assert_eq!(out["type"], "host_request", "{out}");
        assert_eq!(out["kind"], "log", "{out}");
        let msg = out["payload"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("retry_after=30"), "{msg}");
        let out = step(&json!({"type": "host_ok", "id": req_id(&out)}));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "rate-limit");
    }

    #[test]
    fn server_errors_are_transient() {
        for status in [500_u16, 503, 418] {
            let req = invoke_request("lyrics.plain", &query());
            let out = answer(&req, status, "");
            assert_eq!(out["type"], "fail", "{status}");
            assert_eq!(out["error"]["kind"], "transient", "{status}");
        }
    }

    #[test]
    fn host_error_kind_propagates() {
        let req = invoke_request("lyrics.plain", &query());
        let out = step(&json!({
            "type": "host_error", "id": req_id(&req),
            "error": {"kind": "permission-denied", "message": "no network"},
        }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "permission-denied");
    }

    #[test]
    fn unsupported_capability_is_not_applicable() {
        let out = invoke("catalog.search", json!({ "query": query() }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "not-applicable");
    }

    #[test]
    fn malformed_payloads_fail_before_http() {
        for payload in [
            json!({}),
            json!({"query": {"title": "x"}}),
            json!({"query": {
                "title": " ", "artist": null, "album": null,
                "duration_ms": null, "isrc": null,
            }}),
            json!({"query": {
                "title": "x", "artist": null, "album": null,
                "duration_ms": null, "isrc": null,
            }, "extra": 1}),
            json!({"query": {
                "title": "x", "artist": "", "album": null,
                "duration_ms": null, "isrc": null,
            }}),
            json!({"query": {
                "title": "x", "artist": null, "album": null,
                "duration_ms": -5, "isrc": null,
            }}),
            json!({"query": {
                "title": "x", "artist": null, "album": null,
                "duration_ms": null, "isrc": null, "bogus": true,
            }}),
        ] {
            let out = invoke("lyrics.plain", payload.clone());
            assert_eq!(out["type"], "fail", "{payload}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{payload}");
        }
    }

    #[test]
    fn all_miss_is_absent_with_null_matched() {
        let req = invoke_request("lyrics.plain", &query());
        let (urls, out) = miss_all(req, 10);
        assert_eq!(urls.len(), 5, "{urls:?}");
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["state"], "absent");
        assert_eq!(out["result"]["text"], Value::Null);
        assert_eq!(out["result"]["matched"], Value::Null);
    }
}
