//! `playback.candidates`: recording search over the WEB_REMIX
//! metadata client. WEB_REMIX is metadata-only — it stays out of the
//! playback ladder permanently because signature deciphering is out
//! of scope.

use std::collections::BTreeSet;

use auqw_guest_sdk::{http_request, kv_get, kv_set, GuestError, HttpRequest};
use serde_json::{json, Map, Value};

use crate::guest::{bad_payload, failed, is_video_id, payload_keys, warn};
use crate::parse::{visitor_data, visitor_token};

const SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
const CLIENT_NAME_ID: &str = "67";
const CLIENT_VERSION: &str = "1.20260114.01.00";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// InnerTube `params` selecting the songs filter.
const SONGS_PARAMS: &str = "EgWKAQIIAWoMEA4QChADEAQQCRAF";
const VISITOR_KEY: &str = "visitor/web-remix";
/// Response-walk bounds — upstream trees are deep but a pathological
/// response must not spin the guest.
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 10_000;

/// A validated `playback.candidates` payload.
struct CandidatesPayload {
    search_text: String,
    limit: usize,
}

/// An optional query string: null or a nonempty string after trimming;
/// empty strings are `invalid-response`, never serialized as text.
fn opt_string(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, GuestError> {
    match &obj[key] {
        Value::Null => Ok(None),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Err(bad_payload("query strings must be nonempty or null"))
            } else {
                Ok(Some(t.to_string()))
            }
        }
        _ => Err(bad_payload("query strings must be nonempty or null")),
    }
}

fn parse_candidates_payload(payload: &Value) -> Result<CandidatesPayload, GuestError> {
    let obj = payload_keys(payload, &["query", "limit"], &["query", "limit"])?;
    let q = payload_keys(
        &obj["query"],
        &[
            "title",
            "artist",
            "album",
            "duration_ms",
            "version_labels",
            "isrc",
        ],
        &[
            "title",
            "artist",
            "album",
            "duration_ms",
            "version_labels",
            "isrc",
        ],
    )?;
    let title = match &q["title"] {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() || t.chars().count() > 512 {
                return Err(bad_payload(
                    "query.title must be nonempty and at most 512 chars",
                ));
            }
            t.to_string()
        }
        _ => return Err(bad_payload("query.title must be a nonempty string")),
    };
    let artist = opt_string(q, "artist")?;
    let album = opt_string(q, "album")?;
    let isrc = opt_string(q, "isrc")?;
    // ISRC is a filter input for the application, never query text.
    let _ = isrc;
    match &q["duration_ms"] {
        Value::Null => {}
        v if v.as_u64().is_some() => {}
        _ => {
            return Err(bad_payload(
                "query.duration_ms must be a nonnegative integer or null",
            ))
        }
    }
    let labels = match &q["version_labels"] {
        Value::Array(list) => {
            if list.len() > 16 {
                return Err(bad_payload("query.version_labels is limited to 16 entries"));
            }
            let mut seen = BTreeSet::new();
            let mut out = Vec::with_capacity(list.len());
            for v in list {
                let Some(s) = v.as_str() else {
                    return Err(bad_payload("version labels must be strings"));
                };
                if s.is_empty() || s.chars().count() > 64 {
                    return Err(bad_payload("version labels must be 1..64 chars"));
                }
                if !seen.insert(s.to_string()) {
                    return Err(bad_payload("version labels must be unique"));
                }
                out.push(s.to_string());
            }
            out
        }
        _ => return Err(bad_payload("query.version_labels must be an array")),
    };
    let limit = obj["limit"]
        .as_u64()
        .ok_or_else(|| bad_payload("limit must be an integer"))?
        .clamp(1, 50) as usize;
    // Search text order: artist, title, version labels, album.
    let mut parts: Vec<String> = Vec::new();
    if let Some(a) = artist {
        parts.push(a);
    }
    parts.push(title);
    parts.extend(labels);
    if let Some(a) = album {
        parts.push(a);
    }
    Ok(CandidatesPayload {
        search_text: parts.join(" "),
        limit,
    })
}

/// The WEB_REMIX InnerTube `search` call. Visitor replay comes from
/// `visitor/web-remix`, the same per-client KV pattern as the ladder.
fn search_request(query: &str, visitor: Option<&str>) -> HttpRequest {
    let body = json!({
        "context": {
            "client": {
                "clientName": "WEB_REMIX",
                "clientVersion": CLIENT_VERSION,
                "hl": "en",
                "gl": "US",
                "userAgent": USER_AGENT,
            }
        },
        "query": query,
        "params": SONGS_PARAMS,
    });
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        ("User-Agent".into(), USER_AGENT.into()),
        ("X-Goog-Api-Format-Version".into(), "1".into()),
        ("X-YouTube-Client-Name".into(), CLIENT_NAME_ID.into()),
        ("X-YouTube-Client-Version".into(), CLIENT_VERSION.into()),
        ("X-Origin".into(), "https://music.youtube.com".into()),
        ("Origin".into(), "https://music.youtube.com".into()),
        ("Referer".into(), "https://music.youtube.com".into()),
    ];
    if let Some(v) = visitor {
        headers.push(("X-Goog-Visitor-Id".into(), v.into()));
    }
    HttpRequest {
        method: "POST".into(),
        url: SEARCH_URL.into(),
        headers,
        body: Some(serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec())),
    }
}

pub async fn candidates(payload: &Value) -> Result<Value, GuestError> {
    let p = parse_candidates_payload(payload)?;
    let visitor = match kv_get(VISITOR_KEY).await? {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(s) if visitor_token(&s).is_some() => Some(s),
            _ => {
                warn("ignoring malformed visitor KV value").await?;
                None
            }
        },
        None => None,
    };
    let resp = match http_request(search_request(&p.search_text, visitor.as_deref())).await {
        Ok(r) => r,
        Err(GuestError::Host { kind, message }) => match kind.as_str() {
            "cancelled" | "permission-denied" | "invalid-response" => {
                return Err(GuestError::Host { kind, message });
            }
            _ => return Err(failed("transient", "search transport".into())),
        },
        Err(e) => return Err(e),
    };
    match resp.status {
        s if (200..300).contains(&s) => {}
        429 => return Err(failed("rate-limit", "rate-limit".into())),
        _ => return Err(failed("transient", "search transport".into())),
    }
    let body: Value = serde_json::from_slice(&resp.body)
        .ok()
        .filter(Value::is_object)
        .filter(|v| v.get("contents").is_some_and(Value::is_object))
        .ok_or_else(|| {
            failed(
                "invalid-response",
                "search body is not a search JSON object".into(),
            )
        })?;
    if let Some(raw) = visitor_data(&body) {
        if let Some(visitor) = visitor_token(&raw) {
            kv_set(VISITOR_KEY, Some(visitor.as_bytes())).await?;
        } else {
            warn("ignoring malformed visitor value").await?;
        }
    }
    let items = collect_items(&body, p.limit);
    Ok(json!({ "items": items }))
}

/// Walk the response collecting every object under
/// `musicResponsiveListItemRenderer` in traversal order — shelf,
/// item-section, and card shapes all nest them. Bounded so a
/// pathological body cannot spin the guest.
fn collect_renderers<'a>(
    v: &'a Value,
    out: &mut Vec<&'a Map<String, Value>>,
    depth: usize,
    nodes: &mut usize,
) {
    if depth > MAX_DEPTH || *nodes >= MAX_NODES {
        return;
    }
    *nodes += 1;
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                if k == "musicResponsiveListItemRenderer" {
                    if let Value::Object(r) = child {
                        out.push(r);
                    }
                }
                collect_renderers(child, out, depth + 1, nodes);
            }
        }
        Value::Array(a) => {
            for child in a {
                collect_renderers(child, out, depth + 1, nodes);
            }
        }
        _ => {}
    }
}

/// Nested `Map` lookup — borrows, never clones.
fn get_path<'a>(m: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    let mut v = m.get(*keys.first()?)?;
    for k in &keys[1..] {
        v = v.get(*k)?;
    }
    Some(v)
}

/// The row's video id: navigation watchEndpoint, overlay
/// playNavigationEndpoint, or `playlistItemData` — first match with a
/// valid id wins.
fn video_id_of(r: &Map<String, Value>) -> Option<String> {
    const PATHS: &[&[&str]] = &[
        &["navigationEndpoint", "watchEndpoint", "videoId"],
        &[
            "overlay",
            "musicItemThumbnailOverlayRenderer",
            "content",
            "musicPlayButtonRenderer",
            "playNavigationEndpoint",
            "watchEndpoint",
            "videoId",
        ],
        &["playlistItemData", "videoId"],
    ];
    PATHS.iter().find_map(|p| {
        get_path(r, p)
            .and_then(Value::as_str)
            .filter(|s| is_video_id(s))
            .map(str::to_string)
    })
}

/// A `{runs:[{text}..]}`-or-`{simpleText:..}` text node → its trimmed
/// text, `None` when empty.
fn runs_text(text: &Value) -> Option<String> {
    if let Some(runs) = text.get("runs").and_then(Value::as_array) {
        let joined: String = runs
            .iter()
            .filter_map(|r| r.get("text").and_then(Value::as_str))
            .collect();
        let t = joined.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    text.get("simpleText")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The text node of a flex/fixed column entry.
fn text_of(col: &Value) -> Option<String> {
    let renderer = col
        .get("musicResponsiveListItemFlexColumnRenderer")
        .or_else(|| col.get("musicResponsiveListItemFixedColumnRenderer"))?;
    runs_text(renderer.get("text")?)
}

/// Every `{text:{runs:[..]}}` inside a column entry.
fn column_runs(col: &Value) -> Vec<&Map<String, Value>> {
    let mut runs = Vec::new();
    for key in [
        "musicResponsiveListItemFlexColumnRenderer",
        "musicResponsiveListItemFixedColumnRenderer",
    ] {
        if let Some(list) = col
            .get(key)
            .and_then(|c| c.get("text"))
            .and_then(|t| t.get("runs"))
            .and_then(Value::as_array)
        {
            for r in list {
                if let Value::Object(o) = r {
                    runs.push(o);
                }
            }
        }
    }
    runs
}

fn run_text(run: &Map<String, Value>) -> Option<&str> {
    run.get("text").and_then(Value::as_str)
}

fn browse(run: &Map<String, Value>) -> Option<&Map<String, Value>> {
    run.get("navigationEndpoint")?
        .get("browseEndpoint")?
        .as_object()
}

fn page_type(run: &Map<String, Value>) -> Option<&str> {
    browse(run)?
        .get("browseEndpointContextSupportedConfigs")?
        .get("browseEndpointContextMusicConfig")?
        .get("pageType")?
        .as_str()
}

/// `M:SS` / `H:MM:SS` run text → milliseconds. Seconds (and minutes in
/// the hours form) must be under 60; all arithmetic is checked so a
/// pathological timestamp is `None`, not a wrap.
fn duration_ms_of(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split(':').collect();
    if !parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let num = |p: &str| p.parse::<u64>().ok();
    let secs = match parts.as_slice() {
        [m, s] if s.len() == 2 => {
            let (m, s) = (num(m)?, num(s)?);
            if s >= 60 {
                return None;
            }
            m.checked_mul(60)?.checked_add(s)?
        }
        [h, m, s] if m.len() == 2 && s.len() == 2 => {
            let (h, m, s) = (num(h)?, num(m)?, num(s)?);
            if m >= 60 || s >= 60 {
                return None;
            }
            h.checked_mul(3600)?
                .checked_add(m.checked_mul(60)?)?
                .checked_add(s)?
        }
        _ => return None,
    };
    secs.checked_mul(1000)
}

/// Row furniture that must never surface as an artist fallback: type
/// labels, separators, durations, and play-count text.
fn is_furniture(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t == "•" {
        return true;
    }
    if duration_ms_of(t).is_some() {
        return true;
    }
    const LABELS: &[&str] = &[
        "song", "video", "single", "ep", "album", "playlist", "artist", "profile",
    ];
    if LABELS.iter().any(|l| t.eq_ignore_ascii_case(l)) {
        return true;
    }
    ["views", "plays"].iter().any(|suffix| {
        t.get(t.len().saturating_sub(suffix.len())..)
            .is_some_and(|s| s.eq_ignore_ascii_case(suffix))
    })
}

/// Largest HTTPS thumbnail in the renderer by width×height: contract-
/// legal URLs only (≤2048 chars), and dimensions serialize as null
/// rather than schema-invalid zeros.
fn best_artwork(v: &Value) -> Option<Value> {
    let mut best: Option<(u64, Value)> = None;
    let mut nodes = 0usize;
    fn walk(v: &Value, depth: usize, nodes: &mut usize, best: &mut Option<(u64, Value)>) {
        if depth > MAX_DEPTH || *nodes >= MAX_NODES {
            return;
        }
        *nodes += 1;
        match v {
            Value::Object(o) => {
                for (k, child) in o {
                    if k == "thumbnails" {
                        if let Value::Array(list) = child {
                            for t in list {
                                let url = t.get("url").and_then(Value::as_str).unwrap_or("");
                                if !url.starts_with("https://") || url.len() > 2048 {
                                    continue;
                                }
                                let w = t.get("width").and_then(Value::as_u64).unwrap_or(0);
                                let h = t.get("height").and_then(Value::as_u64).unwrap_or(0);
                                let area = w.saturating_mul(h);
                                if best.as_ref().is_none_or(|(a, _)| area > *a) {
                                    *best = Some((
                                        area,
                                        json!({
                                            "url": url,
                                            "width": t
                                                .get("width")
                                                .and_then(Value::as_u64)
                                                .filter(|w| *w > 0),
                                            "height": t
                                                .get("height")
                                                .and_then(Value::as_u64)
                                                .filter(|h| *h > 0),
                                        }),
                                    ));
                                }
                            }
                        }
                    }
                    walk(child, depth + 1, nodes, best);
                }
            }
            Value::Array(a) => {
                for child in a {
                    walk(child, depth + 1, nodes, best);
                }
            }
            _ => {}
        }
    }
    walk(v, 0, &mut nodes, &mut best);
    best.map(|(_, art)| art)
}

/// Map one renderer to a `trackMetadata` candidate; rows without a
/// usable video id or a contract-legal title are not candidates.
fn item_of(r: &Map<String, Value>) -> Option<Value> {
    let video_id = video_id_of(r)?;
    let flex = r
        .get("flexColumns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let title = flex.first().and_then(text_of)?;
    // `trackMetadata.title` caps at 512 scalars — drop the row rather
    // than emit a contract-invalid result.
    if title.chars().count() > 512 {
        return None;
    }

    let mut artist: Option<String> = None;
    let mut second_col_text: Option<String> = None;
    let mut album: Option<String> = None;
    let mut duration_ms: Option<u64> = None;
    let mut columns: Vec<Value> = flex.iter().skip(1).cloned().collect();
    if let Some(fixed) = r.get("fixedColumns").and_then(Value::as_array) {
        columns.extend(fixed.iter().cloned());
    }
    for (ci, col) in columns.iter().enumerate() {
        for run in column_runs(col) {
            let Some(text) = run_text(run).map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some(b) = browse(run) {
                if let Some(id) = b.get("browseId").and_then(Value::as_str) {
                    if id.starts_with("UC") && artist.is_none() {
                        artist = Some(text.to_string());
                    }
                }
                if page_type(run) == Some("MUSIC_PAGE_TYPE_ARTIST") && artist.is_none() {
                    artist = Some(text.to_string());
                }
                if page_type(run) == Some("MUSIC_PAGE_TYPE_ALBUM") && album.is_none() {
                    album = Some(text.to_string());
                }
            }
            if duration_ms.is_none() {
                duration_ms = duration_ms_of(text);
            }
            // Artist fallback: first useful text in the second flex
            // column — never a type label, album-page run, duration,
            // separator, or play count.
            if ci == 0
                && second_col_text.is_none()
                && !is_furniture(text)
                && page_type(run) != Some("MUSIC_PAGE_TYPE_ALBUM")
            {
                second_col_text = Some(text.to_string());
            }
        }
    }
    let artist = artist.or(second_col_text);
    let artwork = best_artwork(&Value::Object(r.clone()))
        .into_iter()
        .collect::<Vec<Value>>();
    Some(json!({
        "source_ref": { "provider": "youtube-music", "kind": "track", "id": video_id },
        "title": title,
        "artist": artist,
        "album": album,
        "duration_ms": duration_ms,
        "release_year": null,
        "artwork": artwork,
        "explicit": null,
        "genre": null,
        "storefront": null,
    }))
}

/// Upstream traversal order, first video id wins, capped at `limit`.
fn collect_items(body: &Value, limit: usize) -> Vec<Value> {
    let mut renderers = Vec::new();
    let mut nodes = 0usize;
    collect_renderers(body, &mut renderers, 0, &mut nodes);
    let mut seen = BTreeSet::new();
    let mut items = Vec::new();
    for r in renderers {
        if items.len() >= limit {
            break;
        }
        let Some(video_id) = video_id_of(r) else {
            continue;
        };
        if seen.contains(&video_id) {
            continue;
        }
        // A row that cannot produce a candidate (no usable title) does
        // not claim the id — a later complete row still wins.
        if let Some(item) = item_of(r) {
            seen.insert(video_id);
            items.push(item);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use auqw_guest_sdk::{dispatch_step, reset_for_testing};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use std::collections::BTreeMap;

    const SONGS: &str = include_str!("../fixtures/search-songs.json");
    const MIXED: &str = include_str!("../fixtures/search-mixed.json");
    const EMPTY: &str = include_str!("../fixtures/search-empty.json");
    const MALFORMED: &str = include_str!("../fixtures/search-malformed.json");

    fn step(input: &Value) -> Value {
        let out = dispatch_step(
            &serde_json::to_vec(input).unwrap_or_default(),
            crate::guest::dispatch,
        );
        serde_json::from_slice(&out).unwrap_or_else(|e| panic!("guest output is not JSON: {e}"))
    }

    fn query() -> Value {
        json!({
            "title": "Never Gonna Give You Up",
            "artist": "Rick Astley",
            "album": null,
            "duration_ms": null,
            "version_labels": [],
            "isrc": null,
        })
    }

    /// Drive until the search `http_request` or a terminal message,
    /// answering kv/log like the resolve harness does. `kv_set` lands
    /// in `committed` directly — a successful search is the only
    /// staging path under test.
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
                        let key = out["payload"]["key"].as_str().unwrap_or("").to_string();
                        let value = self
                            .committed
                            .get(&key)
                            .map(|v| Value::String(B64.encode(v)))
                            .unwrap_or(Value::Null);
                        step(&json!({"type":"kv_response","id":id,"value":value}))
                    }
                    "kv_set" => {
                        let key = out["payload"]["key"].as_str().unwrap_or("").to_string();
                        if let Some(v) = out["payload"]["value"]
                            .as_str()
                            .and_then(|s| B64.decode(s).ok())
                        {
                            self.committed.insert(key, v);
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
                "type": "invoke", "request_id": "t", "capability": "playback.candidates",
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

        fn answer_host_error(&mut self, out: &Value, kind: &str) -> Value {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            self.drive(step(&json!({
                "type": "host_error", "id": id,
                "error": { "kind": kind, "message": "host said no" },
            })))
        }
    }

    fn header_of(out: &Value, name: &str) -> Option<String> {
        out["payload"]["headers"]
            .as_array()?
            .iter()
            .find(|h| h[0].as_str() == Some(name))
            .and_then(|h| h[1].as_str().map(str::to_string))
    }

    fn fail_kind(out: &Value) -> (String, String) {
        assert_eq!(out["type"], "fail");
        (
            out["error"]["kind"].as_str().unwrap_or("").to_string(),
            out["error"]["message"].as_str().unwrap_or("").to_string(),
        )
    }

    fn payload(limit: u64) -> Value {
        json!({ "query": query(), "limit": limit })
    }

    #[test]
    fn songs_fixture_yields_ordered_candidates() {
        let mut h = Harness::new();
        let out = h.invoke(payload(5));
        assert_eq!(out["kind"], "http_request");
        assert_eq!(
            out["payload"]["url"],
            "https://music.youtube.com/youtubei/v1/search?prettyPrint=false"
        );
        let out = h.answer(&out, 200, SONGS);
        assert_eq!(out["type"], "done");
        let Some(items) = out["result"]["items"].as_array() else {
            panic!("items array");
        };
        assert_eq!(items.len(), 3);
        // Upstream traversal order preserved: shelf row, second shelf
        // row, then the item-section row.
        assert_eq!(
            items[0]["source_ref"],
            json!({"provider":"youtube-music","kind":"track","id":"dQw4w9WgXcQ"})
        );
        assert_eq!(items[0]["title"], "Never Gonna Give You Up");
        assert_eq!(items[0]["artist"], "Rick Astley");
        assert_eq!(items[0]["album"], "Whenever You Need Somebody");
        assert_eq!(items[0]["duration_ms"], 213_000);
        assert_eq!(items[0]["release_year"], Value::Null);
        assert_eq!(items[0]["explicit"], Value::Null);
        assert_eq!(items[0]["genre"], Value::Null);
        assert_eq!(items[0]["storefront"], Value::Null);
        // Largest HTTPS thumbnail wins; the bigger http URL is ignored.
        let art = &items[0]["artwork"][0];
        assert_eq!(art["width"], 544);
        assert_eq!(art["height"], 544);
        assert!(art["url"].as_str().unwrap_or("").starts_with("https://"));
        assert_eq!(items[1]["source_ref"]["id"], "oHg5SJYRHA0");
        assert_eq!(items[1]["duration_ms"], 4_820_000); // "1:20:20"
        assert_eq!(items[2]["source_ref"]["id"], "abcDEF123_-");
    }

    #[test]
    fn mixed_fixture_dedups_and_tolerates() {
        let mut h = Harness::new();
        let out = h.invoke(payload(10));
        let out = h.answer(&out, 200, MIXED);
        assert_eq!(out["type"], "done");
        let Some(items) = out["result"]["items"].as_array() else {
            panic!("items array");
        };
        // Card-nested row, the duplicate's first occurrence, the CJK
        // row, the missing-metadata row, and the no-browse fallback
        // row — non-track, titleless, and >512-char-title rows dropped.
        let ids: Vec<&str> = items
            .iter()
            .map(|i| i["source_ref"]["id"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            ids,
            [
                "cardVideoId",
                "dupVideoId1",
                "cjkVideoId1",
                "bareVideoId",
                "fallbackId1"
            ]
        );
        assert_eq!(items[2]["title"], "夜に駆ける");
        assert_eq!(items[2]["artist"], "YOASOBI");
        // Missing metadata surfaces honest nulls and empty artwork.
        assert_eq!(items[3]["title"], "Barebones");
        assert_eq!(items[3]["artist"], Value::Null);
        assert_eq!(items[3]["album"], Value::Null);
        assert_eq!(items[3]["duration_ms"], Value::Null);
        assert_eq!(items[3]["artwork"].as_array().map(Vec::len), Some(0));
        // No browse endpoints: the second-column furniture ("Song" type
        // label, separators, duration) is skipped for the real artist.
        assert_eq!(items[4]["title"], "Fallback Title");
        assert_eq!(items[4]["artist"], "Artist Name");
        assert_eq!(items[4]["duration_ms"], 213_000);
    }

    #[test]
    fn limit_bounds_output() {
        let mut h = Harness::new();
        let out = h.invoke(payload(2));
        let out = h.answer(&out, 200, SONGS);
        assert_eq!(out["result"]["items"].as_array().map(Vec::len), Some(2));
        let mut h = Harness::new();
        // Over-cap limits clamp to 50, not an error.
        let out = h.invoke(payload(500));
        assert_eq!(out["kind"], "http_request");
    }

    #[test]
    fn empty_search_is_done_not_failure() {
        let mut h = Harness::new();
        let out = h.invoke(payload(5));
        let out = h.answer(&out, 200, EMPTY);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["items"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn malformed_search_body_is_invalid_response() {
        // A 2xx body must be a JSON object carrying a `contents`
        // object — anything less is upstream breakage.
        for body in [MALFORMED, "{}", "{\"error\":{}}", "<html>oops</html>"] {
            let mut h = Harness::new();
            let out = h.invoke(payload(5));
            let out = h.answer(&out, 200, body);
            assert_eq!(fail_kind(&out).0, "invalid-response", "{body}");
        }
    }

    #[test]
    fn duration_forms_are_strict() {
        assert_eq!(duration_ms_of("3:33"), Some(213_000));
        assert_eq!(duration_ms_of("1:20:20"), Some(4_820_000));
        assert_eq!(duration_ms_of("120:33"), Some(7_233_000));
        // Seconds and (in the hours form) minutes stay under 60.
        assert_eq!(duration_ms_of("1:99"), None);
        assert_eq!(duration_ms_of("1:60:00"), None);
        assert_eq!(duration_ms_of("1:2"), None);
        // Pathological magnitudes are None, never wrapped — parse
        // failure, multiplication overflow, and add overflow all exit.
        assert_eq!(duration_ms_of("99999999999999999999:59"), None);
        assert_eq!(duration_ms_of("18446744073709551615:00"), None);
        assert_eq!(duration_ms_of("18446744073709551615:59:59"), None);
        // A representable-but-absurd length still parses honestly.
        assert_eq!(
            duration_ms_of("12345678901:12:34"),
            Some(44_444_444_044_354_000)
        );
        assert_eq!(duration_ms_of("4:35 pm"), None);
    }

    #[test]
    fn artwork_bounds_and_null_dimensions() {
        let long_url = format!("https://x/{}", "u".repeat(2048));
        let renderer = json!({
            "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                { "url": long_url, "width": 9999, "height": 9999 },
                { "url": "https://ok/small", "width": 0, "height": 0 },
                { "url": "https://ok/big", "width": 226, "height": 226 }
            ]}}}
        });
        // The >2048-char URL is ignored despite its area; zero
        // dimensions serialize as null.
        let small = best_artwork(&json!({
            "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                { "url": "https://ok/small", "width": 0, "height": 0 }
            ]}}}
        }))
        .unwrap_or_default();
        assert_eq!(small["width"], Value::Null);
        assert_eq!(small["height"], Value::Null);
        let art = best_artwork(&renderer).unwrap_or_default();
        assert_eq!(art["url"], "https://ok/big");
        assert_eq!(art["width"], 226);
    }

    #[test]
    fn overlong_title_row_is_dropped() {
        let r = json!({
            "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "t".repeat(513)}]}}}],
            "navigationEndpoint": {"watchEndpoint": {"videoId": "abcdefghijk"}},
        });
        let Value::Object(map) = r else {
            panic!("object");
        };
        assert!(item_of(&map).is_none());
    }

    #[test]
    fn furniture_never_becomes_artist() {
        for (runs, want) in [
            (
                vec!["Song", " • ", "Real Artist", " • ", "3:33"],
                "Real Artist",
            ),
            (vec!["Video", "12M views", "Channel Name"], "Channel Name"),
            (vec!["Single", " • ", "5K plays", "Person"], "Person"),
            (vec!["3:33"], ""), // only furniture -> no fallback
        ] {
            let run_objs: Vec<Value> = runs.iter().map(|t| json!({"text": t})).collect();
            let r = json!({
                "flexColumns": [
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Title"}]}}},
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": run_objs}}},
                ],
                "navigationEndpoint": {"watchEndpoint": {"videoId": "abcdefghijk"}},
            });
            let Value::Object(map) = r else {
                panic!("object");
            };
            let item = item_of(&map).unwrap_or_default();
            if want.is_empty() {
                assert_eq!(item["artist"], Value::Null, "{runs:?}");
            } else {
                assert_eq!(item["artist"], want, "{runs:?}");
            }
        }
    }

    #[test]
    fn status_taxonomy() {
        for (status, kind) in [
            (429u16, "rate-limit"),
            (503u16, "transient"),
            (404u16, "transient"),
        ] {
            let mut h = Harness::new();
            let out = h.invoke(payload(5));
            let out = h.answer(&out, status, "{}");
            assert_eq!(fail_kind(&out).0, kind, "status {status}");
        }
        // Terminal host errors propagate; weather maps to transient.
        let mut h = Harness::new();
        let out = h.invoke(payload(5));
        let out = h.answer_host_error(&out, "cancelled");
        assert_eq!(fail_kind(&out).0, "cancelled");
        let mut h = Harness::new();
        let out = h.invoke(payload(5));
        let out = h.answer_host_error(&out, "transient");
        assert_eq!(fail_kind(&out).0, "transient");
        let mut h = Harness::new();
        let out = h.invoke(payload(5));
        let out = h.answer_host_error(&out, "permission-denied");
        assert_eq!(fail_kind(&out).0, "permission-denied");
    }

    #[test]
    fn persisted_visitor_replays_and_response_visitor_stores() {
        let mut h = Harness::new();
        h.committed
            .insert("visitor/web-remix".into(), b"persisted-wr".to_vec());
        let out = h.invoke(payload(5));
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("persisted-wr")
        );
        let out = h.answer(&out, 200, SONGS);
        assert_eq!(out["type"], "done");
        // The response's visitorData is staged for the next call.
        assert_eq!(
            h.committed.get("visitor/web-remix").map(Vec::as_slice),
            Some(b"visitor-wr-001".as_slice())
        );
    }

    #[test]
    fn search_text_order_and_headers() {
        let mut h = Harness::new();
        let mut q = query();
        q["album"] = json!("Album Name");
        q["version_labels"] = json!(["Live", "Remastered"]);
        let out = h.invoke(json!({ "query": q, "limit": 5 }));
        assert_eq!(out["kind"], "http_request");
        let body: Value = serde_json::from_slice(
            &B64.decode(out["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        // artist + title + labels + album, single-space joined.
        assert_eq!(
            body["query"],
            "Rick Astley Never Gonna Give You Up Live Remastered Album Name"
        );
        assert_eq!(body["params"], SONGS_PARAMS);
        assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        assert_eq!(
            header_of(&out, "X-YouTube-Client-Name").as_deref(),
            Some("67")
        );
    }

    #[test]
    fn malformed_payloads_are_invalid_response() {
        let cases = [
            json!({}),
            json!({ "query": query() }),
            json!({ "query": query(), "limit": 5, "extra": 1 }),
            json!({ "query": { "title": "x" }, "limit": 5 }),
            json!({ "query": { "title": "", "artist": null, "album": null, "duration_ms": null, "version_labels": [], "isrc": null }, "limit": 5 }),
            json!({ "query": { "title": "  ", "artist": null, "album": null, "duration_ms": null, "version_labels": [], "isrc": null }, "limit": 5 }),
            json!({ "query": { "title": "x".repeat(513), "artist": null, "album": null, "duration_ms": null, "version_labels": [], "isrc": null }, "limit": 5 }),
            json!({ "query": { "title": "t", "artist": " ", "album": null, "duration_ms": null, "version_labels": [], "isrc": null }, "limit": 5 }),
            json!({ "query": { "title": "t", "artist": null, "album": null, "duration_ms": -1, "version_labels": [], "isrc": null }, "limit": 5 }),
            json!({ "query": { "title": "t", "artist": null, "album": null, "duration_ms": null, "version_labels": ["a", "a"], "isrc": null }, "limit": 5 }),
            json!({ "query": { "title": "t", "artist": null, "album": null, "duration_ms": null, "version_labels": [""], "isrc": null }, "limit": 5 }),
            json!({ "query": query(), "limit": "5" }),
        ];
        for p in cases {
            let mut h = Harness::new();
            let out = h.invoke(p.clone());
            assert_eq!(fail_kind(&out).0, "invalid-response", "{p}");
        }
        // >16 labels rejected.
        let mut q = query();
        q["version_labels"] = json!(vec!["l"; 17]);
        let mut h = Harness::new();
        let out = h.invoke(json!({ "query": q, "limit": 5 }));
        assert_eq!(fail_kind(&out).0, "invalid-response");
        // >64-char label rejected.
        let mut q = query();
        q["version_labels"] = json!(["x".repeat(65)]);
        let mut h = Harness::new();
        let out = h.invoke(json!({ "query": q, "limit": 5 }));
        assert_eq!(fail_kind(&out).0, "invalid-response");
    }
}
