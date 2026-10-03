//! iTunes catalog guest: `catalog.search`, `catalog.metadata`,
//! `catalog.entity`, and `catalog.artwork` over the public keyless
//! JSON API (ABI 0.2.0).
//!
//! The guest returns raw provider metadata as `trackMetadata` —
//! version labels and candidate scoring are the application's job.
//! `previewUrl` is never emitted: a 30-second preview is not the
//! recording. Search emits `entities` rails (artist/album rows) and a
//! tagged `top_hit`; entity pages come from `lookup` — an album page
//! is its tracklist, an artist page carries top songs plus a
//! discography rail under `related`. The public API serves no
//! playlists and no paging — `continuation` is always null and a
//! playlist ref is `not-applicable`.

mod encode;
mod http;
mod parse;

use std::collections::BTreeMap;

use auqw_guest_sdk::{export_plugin, log, GuestError, GuestFuture, Invocation, LogLevel};
use serde_json::{json, Map, Value};

const API: &str = "https://itunes.apple.com";
const SEARCH_ARTWORK_SIZE: u64 = 1200;
/// Result kinds the guest serves, in fetch order — the track backbone
/// leads, then the entity rails. `playlist` is contract-valid but the
/// public API serves none, so it filters to an honest zero rows.
const SEARCH_KINDS: &[&str] = &["track", "artist", "album"];
/// Entity rails cap in a mixed query; explicit `kinds` asks get the
/// request's own `limit`.
const ENTITY_MIX_LIMIT: u64 = 10;
/// Section sizes for the artist composite page.
const ARTIST_SONGS_LIMIT: u64 = 50;
const ARTIST_ALBUMS_LIMIT: u64 = 200;
/// Album pages ask for the whole tracklist in one lookup.
const ALBUM_TRACKS_LIMIT: u64 = 200;

fn dispatch(inv: Invocation) -> GuestFuture {
    Box::pin(async move {
        match inv.capability.as_str() {
            "catalog.search" => search(&inv.payload).await,
            "catalog.metadata" => metadata(&inv.payload).await,
            "catalog.artwork" => artwork(&inv.payload).await,
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

/// Log channel failure is weather — `warn` swallows Host kinds and
/// still propagates terminal errors, mirroring `rate_warn` in http.rs.
async fn warn(message: &str) -> Result<(), GuestError> {
    match log(LogLevel::Warn, message).await {
        Err(e) if terminal(&e) => Err(e),
        Err(GuestError::Host { .. }) => Ok(()),
        other => other,
    }
}

/// The payload/ref object must contain exactly `keys` — missing
/// fields or extras are `invalid-response`.
fn payload_obj<'a>(
    payload: &'a Value,
    keys: &[&str],
) -> Result<&'a Map<String, Value>, GuestError> {
    payload_obj_opt(payload, keys, &[])
}

/// Like `payload_obj`, but `optional` keys may be absent; keys outside
/// the union still reject. The host never emits an optional key as
/// `null` — a present-but-null optional is `invalid-response` in the
/// same spirit as an unanticipated key.
fn payload_obj_opt<'a>(
    payload: &'a Value,
    keys: &[&str],
    optional: &[&str],
) -> Result<&'a Map<String, Value>, GuestError> {
    let obj = payload
        .as_object()
        .ok_or_else(|| bad_payload("must be an object"))?;
    for k in obj.keys() {
        if !keys.contains(&k.as_str()) && !optional.contains(&k.as_str()) {
            return Err(bad_payload("unexpected key"));
        }
    }
    for k in keys {
        if !obj.contains_key(*k) {
            return Err(bad_payload("missing key"));
        }
    }
    for k in optional {
        if obj.get(*k) == Some(&Value::Null) {
            return Err(bad_payload("optional key must be absent, not null"));
        }
    }
    Ok(obj)
}

/// A well-formed track `sourceRef` for this provider — the validated
/// `trackId` digits. Foreign-but-shaped refs are `not-applicable`,
/// malformed ones `invalid-response`.
fn itunes_ref(v: &Value) -> Result<String, GuestError> {
    Ok(itunes_ref_kind(v, &["track"])?.1)
}

/// A well-formed `entityRef` for `catalog.entity` — `(kind, id)` for
/// an `album`/`artist` ref. A `playlist` ref is `not-applicable`: the
/// public API serves no playlist entity.
fn entity_ref(v: &Value) -> Result<(String, String), GuestError> {
    itunes_ref_kind(v, &["album", "artist"])
}

/// Ref validation shared by track and entity refs: the provider must
/// be `itunes` and `kind` one of `kinds`, else `not-applicable`; a
/// malformed object or id is `invalid-response`. The returned id is
/// the validated digit string.
fn itunes_ref_kind(v: &Value, kinds: &[&str]) -> Result<(String, String), GuestError> {
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
    if provider != "itunes" || !kinds.contains(&kind) {
        return Err(failed(
            "not-applicable",
            format!("ref is not an itunes {} ref", kinds.join("/")),
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
    let obj = payload_obj_opt(
        payload,
        &["query", "limit", "storefront"],
        &["kinds", "continuation"],
    )?;
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

    // `kinds` absent asks for everything the provider serves; a
    // present array is validated against the contract enum — the
    // public API serves no `playlist`, so the kind fetches nothing.
    let kinds: Vec<&str> = match obj.get("kinds") {
        None => SEARCH_KINDS.to_vec(),
        Some(Value::Array(list)) if !list.is_empty() => {
            let mut wanted = Vec::with_capacity(list.len());
            for k in list {
                let k = k
                    .as_str()
                    .ok_or_else(|| bad_payload("kinds entries must be strings"))?;
                if !["track", "artist", "album", "playlist"].contains(&k) {
                    return Err(bad_payload("kinds entry is not a search kind"));
                }
                if !wanted.contains(&k) {
                    wanted.push(k);
                }
            }
            SEARCH_KINDS
                .iter()
                .copied()
                .filter(|k| wanted.contains(k))
                .collect()
        }
        Some(_) => return Err(bad_payload("kinds must be a nonempty array")),
    };
    // iTunes search has no paging — this guest never emits a
    // continuation, so a present one is a foreign token.
    if let Some(v) = obj.get("continuation") {
        if !v.is_string() {
            return Err(bad_payload("continuation must be a string"));
        }
        return Err(bad_payload("continuation is not a valid token"));
    }
    // In a mixed ask the entity rails cap at ENTITY_MIX_LIMIT; an
    // explicit `kinds` ask hands every kind the request's own limit.
    let scoped = obj.contains_key("kinds");

    let mut items: Vec<Value> = Vec::new();
    let mut entities: Vec<Value> = Vec::new();
    let mut first_failure: Option<GuestError> = None;
    let mut fetched = 0usize;
    let mut failures = 0usize;
    let q = encode::percent_encode(&query);
    let country = storefront
        .as_deref()
        .map(|sf| format!("&country={sf}"))
        .unwrap_or_default();

    for kind in &kinds {
        fetched += 1;
        let page = if *kind == "track" || scoped {
            limit
        } else {
            ENTITY_MIX_LIMIT
        };
        let entity = match *kind {
            "track" => "song",
            "artist" => "musicArtist",
            _ => "album",
        };
        let url =
            format!("{API}/search?term={q}&media=music&entity={entity}&limit={page}{country}");
        let rows = match http::get_json(&url).await {
            Ok(http::Outcome::Body(body)) => match parse::parse_rows(&body) {
                Ok(rows) => {
                    match *kind {
                        "track" => {
                            let tracks =
                                parse::dedup(rows.iter().filter_map(parse::track_of).collect());
                            items.extend(tracks.iter().map(|t| {
                                parse::to_metadata(t, storefront.as_deref(), SEARCH_ARTWORK_SIZE)
                            }));
                        }
                        "artist" => {
                            entities.extend(rows.iter().filter_map(|r| parse::artist_of(r, None)))
                        }
                        _ => entities
                            .extend(rows.iter().filter_map(|r| parse::collection_of(r, None))),
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            },
            Ok(http::Outcome::NotFound) => Err(failed(
                "transient",
                format!("itunes {kind} search status 404"),
            )),
            Err(e) => Err(e),
        };
        if let Err(e) = rows {
            if terminal(&e) {
                return Err(e);
            }
            // Section weather: the rail is simply absent — iTunes has
            // no continuation to retry it through.
            failures += 1;
            if first_failure.is_none() {
                first_failure = Some(e);
            }
            warn("itunes search section unavailable").await?;
        }
    }
    // Every fetched kind failing is a failure, not an empty page.
    if fetched > 0 && failures == fetched {
        return Err(first_failure.unwrap_or_else(|| failed("transient", "itunes search".into())));
    }

    Ok(json!({
        "items": items,
        "entities": entities,
        "top_hit": top_hit(&query, &entities, &items),
        "continuation": Value::Null,
        "storefront": storefront,
    }))
}

/// The hero card: the first rail entity whose title is an exact
/// case-folded match of the query — artists first by fetch order —
/// else a track that matches, else `null`.
fn top_hit(query: &str, entities: &[Value], items: &[Value]) -> Value {
    let q = query.trim().to_lowercase();
    if let Some(e) = entities
        .iter()
        .find(|e| e["title"].as_str().is_some_and(|t| t.to_lowercase() == q))
    {
        return json!({ "type": "entity", "item": e });
    }
    items
        .iter()
        .find(|t| t["title"].as_str().is_some_and(|t| t.to_lowercase() == q))
        .map(|t| json!({ "type": "track", "item": t }))
        .unwrap_or(Value::Null)
}

async fn entity(payload: &Value) -> Result<Value, GuestError> {
    let obj = payload_obj(payload, &["ref"])?;
    let (kind, id) = entity_ref(&obj["ref"])?;
    match kind.as_str() {
        "album" => album_entity(&id).await,
        _ => artist_entity(&id).await,
    }
}

/// `lookup?id=<collection>&entity=song` returns the collection row
/// first, then every song row — no continuation exists.
async fn album_entity(id: &str) -> Result<Value, GuestError> {
    let url = format!("{API}/lookup?id={id}&entity=song&limit={ALBUM_TRACKS_LIMIT}");
    let body = match http::get_json(&url).await? {
        http::Outcome::NotFound => {
            return Err(failed("no-result", format!("itunes album {id} not found")));
        }
        http::Outcome::Body(body) => body,
    };
    let rows = parse::parse_rows(&body)?;
    let entity = match rows.iter().find_map(|r| parse::collection_of(r, None)) {
        Some(e) if e["source_ref"]["id"].as_str() == Some(id) => e,
        _ => {
            return Err(failed("no-result", format!("itunes album {id} not found")));
        }
    };
    let tracks: Vec<parse::Track> = rows.iter().filter_map(parse::track_of).collect();
    let items: Vec<Value> = tracks
        .iter()
        .map(|t| parse::to_metadata(t, None, SEARCH_ARTWORK_SIZE))
        .collect();
    let complete = section_complete(rows.len(), &body, ALBUM_TRACKS_LIMIT);
    Ok(json!({
        "entity": entity,
        "items": items,
        "complete": complete,
    }))
}

/// The artist composite: `lookup?id=<artist>` carries the artist row;
/// `entity=song`/`entity=album` lookups carry the page's sections.
/// A section that fails degrades `complete` to `false` — its rows are
/// left empty, never fabricated.
async fn artist_entity(id: &str) -> Result<Value, GuestError> {
    let url = format!("{API}/lookup?id={id}");
    let body = match http::get_json(&url).await? {
        http::Outcome::NotFound => {
            return Err(failed("no-result", format!("itunes artist {id} not found")));
        }
        http::Outcome::Body(body) => body,
    };
    let rows = parse::parse_rows(&body)?;
    let entity = match rows.iter().find_map(|r| parse::artist_of(r, None)) {
        Some(e) if e["source_ref"]["id"].as_str() == Some(id) => e,
        _ => {
            return Err(failed("no-result", format!("itunes artist {id} not found")));
        }
    };

    let mut items: Vec<Value> = Vec::new();
    let mut related: Vec<Value> = Vec::new();
    let mut complete = true;

    // Top-tracks section: the lookup's leading artist row is skipped
    // by `track_of` on its own.
    let songs = format!("{API}/lookup?id={id}&entity=song&limit={ARTIST_SONGS_LIMIT}");
    match section(&songs).await? {
        Section::Body(body) => match parse::parse_rows(&body) {
            Ok(rows) => {
                items.extend(
                    rows.iter()
                        .filter_map(parse::track_of)
                        .collect::<Vec<_>>()
                        .iter()
                        .map(|t| parse::to_metadata(t, None, SEARCH_ARTWORK_SIZE)),
                );
                if !section_complete(rows.len(), &body, ARTIST_SONGS_LIMIT) {
                    complete = false;
                }
            }
            Err(_) => {
                complete = false;
                warn("itunes artist songs section unavailable").await?;
            }
        },
        Section::Degraded => {
            complete = false;
            warn("itunes artist songs section unavailable").await?;
        }
    }

    // Discography rail — collection rows ride `related`, never items.
    let albums = format!("{API}/lookup?id={id}&entity=album&limit={ARTIST_ALBUMS_LIMIT}");
    match section(&albums).await? {
        Section::Body(body) => match parse::parse_rows(&body) {
            Ok(rows) => {
                related.extend(
                    rows.iter()
                        .filter_map(|r| parse::collection_of(r, Some("discography"))),
                );
                if !section_complete(rows.len(), &body, ARTIST_ALBUMS_LIMIT) {
                    complete = false;
                }
            }
            Err(_) => {
                complete = false;
                warn("itunes artist albums section unavailable").await?;
            }
        },
        Section::Degraded => {
            complete = false;
            warn("itunes artist albums section unavailable").await?;
        }
    }

    Ok(json!({
        "entity": entity,
        "items": items,
        "related": related,
        "complete": complete,
    }))
}

/// One section's honesty check: `complete` only when the body came in
/// under the request's own limit AND no declared `resultCount` says
/// more rows existed than arrived — at-cap may have more upstream,
/// and a declared count past the delivered rows is missing data.
fn section_complete(rows: usize, body: &[u8], limit: u64) -> bool {
    rows < limit as usize && parse::result_count(body).is_none_or(|c| c <= rows as u64)
}

/// The result of a composite-page section fetch.
enum Section {
    /// A parseable 2xx body (not necessarily well-shaped inside).
    Body(Vec<u8>),
    /// The section is unavailable: not-found, upstream weather, or a
    /// malformed/empty response.
    Degraded,
}

/// Fetch one page section. Terminal host failures
/// (`cancelled`/`permission-denied`/`invalid-response`) and step
/// protocol violations propagate; every other failure — including a
/// guest-side `rate-limit`/`transient` verdict — is a degraded
/// section the caller flags `complete:false`.
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
    const SEARCH_ARTIST: &str = include_str!("../fixtures/search-artist.json");
    const SEARCH_ALBUM: &str = include_str!("../fixtures/search-album.json");
    const LOOKUP_ALBUM: &str = include_str!("../fixtures/lookup-album.json");
    const LOOKUP_ARTIST: &str = include_str!("../fixtures/lookup-artist.json");
    const LOOKUP_ARTIST_SONGS: &str = include_str!("../fixtures/lookup-artist-songs.json");
    const LOOKUP_ARTIST_ALBUMS: &str = include_str!("../fixtures/lookup-artist-albums.json");

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

    fn entities_of(out: &Value) -> &Vec<Value> {
        out["result"]["entities"]
            .as_array()
            .unwrap_or_else(|| panic!("result.entities not an array: {out}"))
    }

    fn related_of(out: &Value) -> &Vec<Value> {
        out["result"]["related"]
            .as_array()
            .unwrap_or_else(|| panic!("result.related not an array: {out}"))
    }

    /// Invoke `catalog.search` and return the emitted `http_request`.
    fn search_request(payload: Value) -> Value {
        let out = invoke("catalog.search", payload);
        assert_eq!(out["type"], "host_request", "{out}");
        assert_eq!(out["kind"], "http_request", "{out}");
        out
    }

    /// Feed `respond(request)` to every emitted `http_request` until a
    /// terminal output (`done`/`fail`), acking `log` calls and
    /// collecting their messages. Bounded so a guest that never
    /// settles fails the test instead of hanging it.
    fn drive(out: Value, mut respond: impl FnMut(&Value) -> Value) -> (Value, Vec<String>) {
        let mut logs = Vec::new();
        let mut out = out;
        for _ in 0..32 {
            match (out["type"].as_str(), out["kind"].as_str()) {
                (Some("host_request"), Some("http_request")) => {
                    out = step(&respond(&out));
                }
                (Some("host_request"), Some("log")) => {
                    if let Some(m) = out["payload"]["message"].as_str() {
                        logs.push(m.to_string());
                    }
                    out = step(&json!({"type": "host_ok", "id": req_id(&out)}));
                }
                _ => return (out, logs),
            }
        }
        panic!("drive exceeded its step budget: {out}")
    }

    /// The search fixture a typed entity endpoint answers with.
    fn search_section(req: &Value) -> &'static str {
        let url = req["payload"]["url"].as_str().unwrap_or_default();
        if url.contains("entity=musicArtist") {
            SEARCH_ARTIST
        } else if url.contains("entity=album") {
            SEARCH_ALBUM
        } else {
            MIXED
        }
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

    /// A kinds-absent ask fetches track + artist + album rails in one
    /// pass; entity rows land under `entities`, an exact title match
    /// is the tagged hero, and track rows carry their entity refs.
    #[test]
    fn search_mixed_fixture_filters_and_maps() {
        let req = search_request(json!({"query": "portishead", "limit": 10, "storefront": "US"}));
        let (out, _) = drive(step(&http_ok(req_id(&req), MIXED)), |req| {
            http_ok(req_id(req), search_section(req))
        });
        assert_eq!(out["type"], "done", "{out}");
        let items = items_of(&out);
        // music-video and zero-id rows dropped; four songs survive.
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(out["result"]["storefront"], "US");

        // Rails: artists first by fetch order, then albums.
        let entities = entities_of(&out);
        assert_eq!(entities.len(), 6, "{entities:?}");
        assert_eq!(entities[0]["kind"], "artist");
        assert_eq!(entities[0]["title"], "Portishead");
        assert_eq!(entities[0]["source_ref"]["id"], "2893557");
        assert_eq!(entities[0]["artwork"], json!([]));
        assert_eq!(entities[3]["kind"], "album");
        assert_eq!(entities[3]["title"], "Dummy");
        assert_eq!(entities[3]["subtitle"], "Portishead");
        assert!(
            entities[3]["artwork"][0]["url"]
                .as_str()
                .unwrap_or_default()
                .contains("600x600bb"),
            "{entities:?}"
        );
        // The exact-title artist is the tagged hero.
        let hit = &out["result"]["top_hit"];
        assert_eq!(hit["type"], "entity");
        assert_eq!(hit["item"]["source_ref"]["id"], "2893557");
        assert_eq!(out["result"]["continuation"], Value::Null);

        let first = &items[0];
        assert_eq!(first["title"], "Roads");
        assert_eq!(first["artist"], "Portishead");
        assert_eq!(first["album"], "Dummy");
        // Upstream ids mint the entity refs.
        assert_eq!(first["artist_ref"]["kind"], "artist");
        assert_eq!(first["artist_ref"]["id"], "2893557");
        assert_eq!(first["album_ref"]["kind"], "album");
        assert_eq!(first["album_ref"]["id"], "1440760837");
        // Rows without upstream ids emit honest null refs.
        assert_eq!(items[1]["artist_ref"], Value::Null, "{items:?}");
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
        let req = search_request(
            json!({"query": "song", "limit": 50, "storefront": null, "kinds": ["track"]}),
        );
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
        let req = search_request(
            json!({"query": "x", "limit": 5, "storefront": null, "kinds": ["track"]}),
        );
        let out = step(&http_ok(req_id(&req), EMPTY));
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
    }

    /// A `track`-only kinds ask keeps the single-call shape; a
    /// malformed body is the one section's failure — all sections
    /// failing is the invocation's `invalid-response`.
    #[test]
    fn malformed_body_is_invalid_response() {
        for body in [MALFORMED, "not json {"] {
            let req = search_request(
                json!({"query": "x", "limit": 5, "storefront": null, "kinds": ["track"]}),
            );
            // The section warn rides before the fail.
            let (out, logs) = drive(step(&http_ok(req_id(&req), body)), |_| {
                panic!("no further request expected")
            });
            assert_eq!(out["type"], "fail", "{body}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{body}");
            assert!(
                logs.iter().any(|m| m.contains("section unavailable")),
                "{logs:?}"
            );
        }
    }

    /// Under a mixed ask one rail's weather never sinks the page —
    /// the failed section is simply absent and the rest delivers.
    #[test]
    fn search_section_weather_degrades_a_rail_not_the_page() {
        let req = search_request(json!({"query": "x", "limit": 5, "storefront": null}));
        let mut calls = 0usize;
        let (out, logs) = drive(step(&http_status(req_id(&req), 500, &[])), |req| {
            calls += 1;
            http_ok(req_id(req), search_section(req))
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(calls, 2, "the two healthy sections still fetch");
        assert!(items_of(&out).is_empty());
        assert_eq!(entities_of(&out).len(), 6);
        assert!(
            logs.iter().any(|m| m.contains("section unavailable")),
            "{logs:?}"
        );
    }

    /// Every fetched section failing is a failure, not an empty page.
    #[test]
    fn search_all_sections_fail_is_typed_failure() {
        let mut out = search_request(json!({"query": "x", "limit": 5, "storefront": null}));
        for _ in 0..3 {
            assert_eq!(out["kind"], "http_request", "{out}");
            out = step(&http_status(req_id(&out), 500, &[]));
            // Acks the section-warn log between requests.
            assert_eq!(out["kind"], "log", "{out}");
            out = step(&json!({"type": "host_ok", "id": req_id(&out)}));
        }
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "transient", "{out}");
    }

    /// `kinds` scopes the ask: one endpoint, the request's own limit.
    #[test]
    fn search_kinds_scopes_to_one_call() {
        let req = search_request(
            json!({"query": "portishead", "limit": 7, "storefront": "us", "kinds": ["album"]}),
        );
        let url = req["payload"]["url"].as_str().unwrap_or_default();
        assert!(url.contains("entity=album"), "{url}");
        assert!(url.contains("limit=7"), "{url}");
        assert!(url.contains("country=US"), "{url}");
        let (out, _) = drive(step(&http_ok(req_id(&req), SEARCH_ALBUM)), |_| {
            panic!("no further request expected")
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(entities_of(&out).len(), 3);
        assert!(entities_of(&out).iter().all(|e| e["kind"] == "album"));
        assert!(items_of(&out).is_empty());
    }

    /// `playlist` is contract-valid but unservable — it fetches
    /// nothing and delivers an honest empty page.
    #[test]
    fn search_playlist_kind_is_honest_empty() {
        let out = invoke(
            "catalog.search",
            json!({"query": "x", "limit": 5, "storefront": null, "kinds": ["playlist"]}),
        );
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
        assert!(entities_of(&out).is_empty());
        assert_eq!(out["result"]["top_hit"], Value::Null);
    }

    /// iTunes never emits a continuation — a present one is a foreign
    /// token, and a non-string one is malformed.
    #[test]
    fn search_continuation_is_invalid_response() {
        for cont in [json!("tok"), json!(5), json!({"a":1})] {
            let out = invoke(
                "catalog.search",
                json!({"query": "x", "limit": 5, "storefront": null, "continuation": cont}),
            );
            assert_eq!(out["type"], "fail", "{cont}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{cont}");
        }
    }

    /// `kinds` shape errors are payload errors before any request.
    #[test]
    fn search_kinds_shape_is_validated() {
        for kinds in [
            json!([]),
            json!("album"),
            json!([5]),
            json!(["genre"]),
            json!(["album", "bogus"]),
        ] {
            let out = invoke(
                "catalog.search",
                json!({"query": "x", "limit": 5, "storefront": null, "kinds": kinds}),
            );
            assert_eq!(out["type"], "fail", "{kinds}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{kinds}");
        }
    }

    #[test]
    fn rate_limit_logs_then_fails() {
        let req = search_request(
            json!({"query": "x", "limit": 1, "storefront": null, "kinds": ["track"]}),
        );
        let out = step(&http_status(req_id(&req), 429, &[("Retry-After", "30")]));
        // The retry hint rides both the diagnostic log and the fail
        // message — the message is the only channel back to the app.
        assert_eq!(out["type"], "host_request", "{out}");
        assert_eq!(out["kind"], "log", "{out}");
        let msg = out["payload"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("retry_after=30"), "{msg}");
        assert!(!msg.contains("http"), "{msg}");
        // The lone section's rate-limit is the invocation's failure.
        let (out, _) = drive(
            step(&json!({"type": "host_ok", "id": req_id(&out)})),
            |_| panic!("no further request expected"),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "rate-limit");
        assert_eq!(
            out["error"]["message"].as_str(),
            Some("rate-limit: itunes status 429 retry_after=30")
        );
    }

    #[test]
    fn rate_limit_survives_a_failed_log_call() {
        // The log channel's own failure must never displace the typed
        // verdict: the warn request answered `host_error` still leaves
        // `rate-limit`, not the channel's `transient`.
        let req = search_request(
            json!({"query": "x", "limit": 1, "storefront": null, "kinds": ["track"]}),
        );
        let out = step(&http_status(req_id(&req), 429, &[("Retry-After", "30")]));
        assert_eq!(out["kind"], "log", "{out}");
        let out = step(&json!({
            "type": "host_error", "id": req_id(&out),
            "error": {"kind": "transient", "message": "log channel down"},
        }));
        // The section warn rides before the verdict.
        let (out, _) = drive(out, |_| panic!("no further request expected"));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "rate-limit", "{out}");
    }

    #[test]
    fn cancelled_log_call_propagates_over_rate_limit() {
        // Terminal kinds outrank the typed verdict: `cancelled` is the
        // abort signal, never a rate-limit.
        let req = search_request(
            json!({"query": "x", "limit": 1, "storefront": null, "kinds": ["track"]}),
        );
        let out = step(&http_status(req_id(&req), 429, &[("Retry-After", "30")]));
        assert_eq!(out["kind"], "log", "{out}");
        let out = step(&json!({
            "type": "host_error", "id": req_id(&out),
            "error": {"kind": "cancelled", "message": "invocation stopped"},
        }));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "cancelled", "{out}");
    }

    #[test]
    fn server_and_other_errors_are_transient() {
        for status in [500_u16, 503, 418] {
            let req = search_request(
                json!({"query": "x", "limit": 1, "storefront": null, "kinds": ["track"]}),
            );
            let (out, _) = drive(step(&http_status(req_id(&req), status, &[])), |_| {
                panic!("no further request expected")
            });
            assert_eq!(out["type"], "fail", "{status}");
            assert_eq!(out["error"]["kind"], "transient", "{status}");
        }
    }

    #[test]
    fn search_404_is_transient() {
        let req = search_request(
            json!({"query": "x", "limit": 1, "storefront": null, "kinds": ["track"]}),
        );
        let (out, _) = drive(step(&http_status(req_id(&req), 404, &[])), |_| {
            panic!("no further request expected")
        });
        assert_eq!(out["error"]["kind"], "transient");
    }

    /// An album page is a `lookup` whose leading collection row is
    /// the entity and whose track rows are the items.
    #[test]
    fn entity_album_page_is_lookup_backed() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "itunes", "kind": "album", "id": "1440760837"}}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(url.starts_with("https://itunes.apple.com/lookup?"), "{url}");
        assert!(url.contains("id=1440760837"), "{url}");
        assert!(url.contains("entity=song"), "{url}");

        let (out, _) = drive(step(&http_ok(req_id(&out), LOOKUP_ALBUM)), |_| {
            panic!("no further request expected")
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["entity"]["kind"], "album");
        assert_eq!(out["result"]["entity"]["title"], "Dummy");
        assert_eq!(out["result"]["entity"]["subtitle"], "Portishead");
        assert!(
            out["result"]["entity"]["artwork"][0]["url"]
                .as_str()
                .unwrap_or_default()
                .contains("600x600bb"),
            "{out}"
        );
        let items = items_of(&out);
        assert_eq!(items.len(), 4, "{items:?}");
        assert_eq!(items[0]["title"], "Mysterons");
        assert_eq!(items[0]["album_ref"]["id"], "1440760837");
        assert_eq!(items[0]["artist_ref"]["id"], "2893557");
        // resultCount 5 = collection row + 4 songs — complete.
        assert_eq!(out["result"]["complete"], true);
        assert_eq!(out["result"]["continuation"], Value::Null);
    }

    /// An artist page is three lookups: the artist row plus top-songs
    /// and discography sections riding `related`.
    #[test]
    fn entity_artist_page_carries_discography_rail() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "itunes", "kind": "artist", "id": "2893557"}}),
        );
        let mut seen = Vec::new();
        let (out, _) = drive(out, |req| {
            let url = req["payload"]["url"].as_str().unwrap_or_default();
            let body = if url.contains("entity=song") {
                seen.push("songs");
                LOOKUP_ARTIST_SONGS
            } else if url.contains("entity=album") {
                seen.push("albums");
                LOOKUP_ARTIST_ALBUMS
            } else {
                seen.push("artist");
                LOOKUP_ARTIST
            };
            http_ok(req_id(req), body)
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(seen, vec!["artist", "songs", "albums"]);

        assert_eq!(out["result"]["entity"]["kind"], "artist");
        assert_eq!(out["result"]["entity"]["title"], "Portishead");
        assert_eq!(out["result"]["entity"]["subtitle"], "Electronic");
        let items = items_of(&out);
        assert_eq!(items.len(), 3, "{items:?}");
        assert_eq!(items[0]["title"], "Roads");
        let related = related_of(&out);
        assert_eq!(related.len(), 3, "{related:?}");
        assert!(
            related
                .iter()
                .all(|e| e["kind"] == "album" && e["group"] == "discography"),
            "{related:?}"
        );
        assert_eq!(out["result"]["complete"], true);
    }

    /// A failed songs section degrades the page honestly — `complete`
    /// flips false, the rest still delivers.
    #[test]
    fn entity_artist_partial_section_is_incomplete_not_failed() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "itunes", "kind": "artist", "id": "2893557"}}),
        );
        let mut n = 0usize;
        let (out, logs) = drive(out, |req| {
            n += 1;
            match n {
                1 => http_ok(req_id(req), LOOKUP_ARTIST),
                2 => http_status(req_id(req), 500, &[]),
                _ => http_ok(req_id(req), LOOKUP_ARTIST_ALBUMS),
            }
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false, "{out}");
        assert!(items_of(&out).is_empty());
        assert_eq!(related_of(&out).len(), 3);
        assert!(
            logs.iter().any(|m| m.contains("section unavailable")),
            "{logs:?}"
        );
    }

    /// A lookup that returns no matching entity row is `no-result`.
    #[test]
    fn entity_lookup_miss_is_no_result() {
        for kind in ["album", "artist"] {
            let out = invoke(
                "catalog.entity",
                json!({"ref": {"provider": "itunes", "kind": kind, "id": "42"}}),
            );
            let out = step(&http_ok(req_id(&out), EMPTY));
            assert_eq!(out["type"], "fail", "{kind}");
            assert_eq!(out["error"]["kind"], "no-result", "{kind}");
        }
    }

    /// A lookup that fills its request limit may have more upstream —
    /// at-cap pages report `complete:false` since no count proves the
    /// rest is absent.
    #[test]
    fn entity_capped_lookup_is_incomplete() {
        fn body_of(rows: Vec<Value>) -> String {
            json!({"resultCount": rows.len(), "results": rows}).to_string()
        }
        let song = |i: u64| {
            json!({"wrapperType": "track", "kind": "song", "trackId": i,
                   "artistId": 2893557, "collectionId": 1440760837,
                   "trackName": "s", "artistName": "a", "trackTimeMillis": 1})
        };

        // Album at cap: collection + 199 songs fills limit=200.
        let mut rows = vec![json!({"wrapperType": "collection",
            "collectionId": 1440760837, "collectionName": "Dummy",
            "artistName": "Portishead"})];
        rows.extend((1..200).map(song));
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "itunes", "kind": "album", "id": "1440760837"}}),
        );
        let (out, _) = drive(step(&http_ok(req_id(&out), &body_of(rows))), |_| {
            panic!("no further request expected")
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false, "{out}");

        // Artist songs at cap: artist + 49 songs fills limit=50 — the
        // discography still delivers while the page degrades honest.
        let mut songs = vec![json!({"wrapperType": "artist", "artistId": 2893557,
            "artistName": "Portishead"})];
        songs.extend((1..50).map(song));
        let mut n = 0usize;
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "itunes", "kind": "artist", "id": "2893557"}}),
        );
        let (out, _) = drive(out, |req| {
            n += 1;
            match n {
                1 => http_ok(req_id(req), LOOKUP_ARTIST),
                2 => http_ok(req_id(req), &body_of(songs.clone())),
                _ => http_ok(req_id(req), LOOKUP_ARTIST_ALBUMS),
            }
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false, "{out}");
        assert_eq!(items_of(&out).len(), 49);
        assert_eq!(related_of(&out).len(), 3);

        // A declared resultCount past the delivered rows is missing
        // data even under the limit — collection + 2 songs while the
        // header claims 5 rows.
        let short = json!({"resultCount": 5, "results": [
            {"wrapperType": "collection", "collectionId": 1440760837,
             "collectionName": "Dummy", "artistName": "Portishead"},
            song(1),
            song(2),
        ]});
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "itunes", "kind": "album", "id": "1440760837"}}),
        );
        let (out, _) = drive(step(&http_ok(req_id(&out), &short.to_string())), |_| {
            panic!("no further request expected")
        });
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(items_of(&out).len(), 2);
        assert_eq!(out["result"]["complete"], false, "{out}");
    }

    /// iTunes serves no playlist entity; a foreign provider ref is
    /// not ours to serve. Both die before any request.
    #[test]
    fn entity_unservable_refs_are_not_applicable() {
        for (kind, provider) in [
            ("playlist", "itunes"),
            ("track", "itunes"),
            ("album", "deezer"),
        ] {
            let out = invoke(
                "catalog.entity",
                json!({"ref": {"provider": provider, "kind": kind, "id": "42"}}),
            );
            assert_eq!(out["type"], "fail", "{kind}:{provider}");
            assert_eq!(out["error"]["kind"], "not-applicable", "{kind}:{provider}");
        }
    }

    /// The entity payload takes only `ref`; anything else is strict.
    #[test]
    fn entity_payload_strictness() {
        for payload in [
            json!({}),
            json!({"ref": {"provider": "itunes", "kind": "album", "id": "42"}, "extra": 1}),
        ] {
            let out = invoke("catalog.entity", payload.clone());
            assert_eq!(out["type"], "fail", "{payload}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{payload}");
        }
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
        let req = search_request(
            json!({"query": cjk, "limit": 5, "storefront": null, "kinds": ["track"]}),
        );
        let out = step(&http_ok(req_id(&req), EMPTY));
        assert_eq!(out["type"], "done");
        let out = invoke(
            "catalog.search",
            json!({"query": "曲".repeat(513), "limit": 5, "storefront": null}),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        // The 512-ASCII boundary still emits a request.
        let req = search_request(
            json!({"query": "a".repeat(512), "limit": 5, "storefront": null, "kinds": ["track"]}),
        );
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

    /// Only a delta-seconds `Retry-After` reaches the diagnostic log and
    /// the fail message; oversized, malformed, and control-containing
    /// values are dropped while the invocation still fails `rate-limit`.
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
            let req = search_request(
                json!({"query": "x", "limit": 1, "storefront": null, "kinds": ["track"]}),
            );
            let out = step(&http_status(req_id(&req), 429, &[("Retry-After", value)]));
            assert_eq!(out["kind"], "log", "{value}");
            assert_eq!(out["payload"]["message"].as_str(), Some(want), "{value}");
            let (out, _) = drive(
                step(&json!({"type": "host_ok", "id": req_id(&out)})),
                |_| panic!("no further request expected"),
            );
            assert_eq!(out["error"]["kind"], "rate-limit", "{value}");
            let want_fail = if want.contains("retry_after") {
                format!("rate-limit: itunes status 429 retry_after={value}")
            } else {
                "rate-limit: itunes status 429".to_string()
            };
            assert_eq!(
                out["error"]["message"].as_str(),
                Some(want_fail.as_str()),
                "{value}"
            );
        }
    }

    #[test]
    fn unsupported_capability_is_not_applicable() {
        let out = invoke("playback.resolve", json!({}));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "not-applicable");
    }
}
