//! Deezer catalog guest: `catalog.search`, `catalog.metadata`, and
//! `catalog.entity` over the public keyless JSON API (ABI 0.3.0).
//!
//! The guest returns raw provider metadata as `trackMetadata` —
//! version labels and candidate scoring are the application's job.
//! `catalog.search` serves the track backbone plus `entities` rails
//! (artists, albums, playlists) with a tagged `top_hit` hero and an
//! offset-paging `continuation` token. `catalog.entity` returns
//! composite pages: album and playlist pages carry their track
//! listings; an artist page carries top tracks plus `related` rails
//! (discography and related artists) — entity rows never ride `items`.
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
/// Result kinds the guest serves, in fetch order — the track backbone
/// leads, then the entity rails.
const SEARCH_KINDS: &[&str] = &["track", "artist", "album", "playlist"];
/// Entity rails cap in a mixed query; explicit `kinds` asks get the
/// request's own `limit`.
const ENTITY_MIX_LIMIT: u64 = 10;
/// Page sizes for the artist composite's sections.
const ARTIST_TOP_LIMIT: u64 = 50;
const ARTIST_ALBUMS_LIMIT: u64 = 50;
const ARTIST_RELATED_LIMIT: u64 = 25;
/// `catalog.metadata` fetches one page per ref and the host admits
/// 32 HTTP calls per invocation — cap the batch at 30 so it fits the
/// call budget with headroom instead of dying `budget-exceeded` with
/// nothing fetched. A larger batch is rejected outright: returning a
/// fetched prefix would report truncated data as if the tail were
/// genuinely absent upstream.
const METADATA_FETCH_MAX: usize = 30;

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

/// A sanitized warning — never carries bodies, URLs, or upstream
/// text. The diagnostic is advisory: weather on the log channel is
/// swallowed while terminal kinds still propagate, mirroring
/// `rate_warn` in http.rs.
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
        .clamp(1, SEARCH_LIMIT_MAX);
    // Deezer has no storefront scoping; the parameter is validated for
    // shape and the result is honestly reported as global (`null`).
    let _storefront = storefront_of(obj)?;

    // `kinds` absent asks for everything; a present array is validated
    // against the contract enum and deduped into fetch order.
    let kinds: Vec<&str> = match obj.get("kinds") {
        None => SEARCH_KINDS.to_vec(),
        Some(Value::Array(list)) if !list.is_empty() => {
            let mut wanted = Vec::with_capacity(list.len());
            for k in list {
                let k = k
                    .as_str()
                    .ok_or_else(|| bad_payload("kinds entries must be strings"))?;
                if !SEARCH_KINDS.contains(&k) {
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
    // In a mixed ask the entity rails cap at ENTITY_MIX_LIMIT; an
    // explicit `kinds` ask hands every kind the request's own limit.
    let scoped = obj.contains_key("kinds");

    // The continuation token is this guest's own JSON map of
    // kind → next `index` for offset paging — `null` marks a kind that
    // exhausted, a missing entry restarts that kind at 0. Foreign or
    // malformed tokens are payload errors.
    let offsets: Map<String, Value> = match obj.get("continuation") {
        None => Map::new(),
        Some(Value::String(token)) => {
            let parsed: Value = serde_json::from_str(token)
                .map_err(|_| bad_payload("continuation is not a valid token"))?;
            let m = parsed
                .as_object()
                .ok_or_else(|| bad_payload("continuation is not a valid token"))?;
            if !m
                .iter()
                .all(|(k, v)| SEARCH_KINDS.contains(&k.as_str()) && (v.is_u64() || v.is_null()))
            {
                return Err(bad_payload("continuation is not a valid token"));
            }
            m.clone()
        }
        Some(_) => return Err(bad_payload("continuation must be a string")),
    };

    let mut items: Vec<Value> = Vec::new();
    let mut entities: Vec<Value> = Vec::new();
    // Done markers carry across pages — a kind that exhausted stays
    // exhausted and never refetches to repeat its first page beside
    // deeper rails.
    let mut more: Map<String, Value> = offsets
        .iter()
        .filter(|(_, v)| v.is_null())
        .map(|(k, _)| (k.clone(), Value::Null))
        .collect();
    let mut first_failure: Option<GuestError> = None;
    let mut fetched = 0usize;
    let mut failures = 0usize;
    let q = encode::percent_encode(&query);

    for kind in &kinds {
        // A done kind keeps its marker and never refetches.
        if offsets.get(*kind).is_some_and(Value::is_null) {
            continue;
        }
        fetched += 1;
        let off = offsets.get(*kind).and_then(Value::as_u64).unwrap_or(0);
        let page = if *kind == "track" || scoped {
            limit
        } else {
            ENTITY_MIX_LIMIT
        };
        // `index` is Deezer's offset paging — absent on page one.
        let index = if off == 0 {
            String::new()
        } else {
            format!("&index={off}")
        };
        let url = match *kind {
            "track" => format!("{API}/search?q={q}&limit={page}{index}"),
            other => format!("{API}/search/{other}?q={q}&limit={page}{index}"),
        };
        let rows = match http::get_json(&url).await {
            Ok(http::Outcome::Body(v)) => match *kind {
                "track" => parse::search_items(&v).map(|mut r| items.append(&mut r)),
                "artist" => {
                    parse::entity_items(&v, parse::artist_hit).map(|mut r| entities.append(&mut r))
                }
                "album" => parse::entity_items(&v, |row| parse::album_hit(row, None))
                    .map(|mut r| entities.append(&mut r)),
                _ => parse::entity_items(&v, parse::playlist_hit)
                    .map(|mut r| entities.append(&mut r)),
            }
            .map(|()| {
                // The link's own `index` is the next offset — a short
                // page advances by fewer rows than `limit`, so `off +
                // page` would skip rows. No `next` marks the kind done.
                match parse::next_index(&v, off) {
                    Some(idx) => more.insert(kind.to_string(), json!(idx)),
                    None => more.insert(kind.to_string(), Value::Null),
                };
            }),
            Ok(http::Outcome::NotFound) => Err(failed(
                "transient",
                format!("deezer {kind} search status 404"),
            )),
            Err(e) => Err(e),
        };
        if let Err(e) = rows {
            if terminal(&e) {
                return Err(e);
            }
            // Section weather: the kind keeps its offset in the
            // continuation — the next page retries it rather than
            // silently losing the rail.
            failures += 1;
            if first_failure.is_none() {
                first_failure = Some(e);
            }
            more.insert(kind.to_string(), json!(off));
            warn("deezer search section unavailable").await?;
        }
    }
    // Every fetched kind failing is a failure, not an empty page.
    if fetched > 0 && failures == fetched {
        return Err(first_failure.unwrap_or_else(|| failed("transient", "deezer search".into())));
    }

    Ok(json!({
        "items": items,
        "entities": entities,
        "top_hit": top_hit(&query, &entities, &items),
        // Only a pending offset keeps the page alive — a token of pure
        // done-markers is the honest end, same as an empty one.
        "continuation": match more.values().any(Value::is_u64) {
            true => json!(serde_json::to_string(&more).unwrap_or_default()),
            false => Value::Null,
        },
        "storefront": Value::Null,
    }))
}

/// The hero card: the first rail entity whose title is an exact
/// case-folded match of the query — artists first by fetch order —
/// else a track that matches, else `null`. Deezer orders each list by
/// relevance, so the first exact match is the best hit.
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

async fn metadata(payload: &Value) -> Result<Value, GuestError> {
    let obj = payload_obj(payload, &["refs"])?;
    let refs = obj["refs"]
        .as_array()
        .ok_or_else(|| bad_payload("refs must be an array"))?;
    if refs.len() > METADATA_FETCH_MAX {
        return Err(bad_payload(&format!(
            "refs is limited to {METADATA_FETCH_MAX} entries per invocation"
        )));
    }
    // Validate every ref before any request — one malformed ref
    // rejects the whole batch.
    let validated = refs
        .iter()
        .map(|r| deezer_ref(r, &["track", "album", "artist"]))
        .collect::<Result<Vec<_>, _>>()?;
    let mut items = Vec::with_capacity(validated.len());
    // Deezer has no batch lookup — one request per ref, in input
    // order. A ref whose resource is absent upstream is omitted, the
    // same way itunes drops genuinely-missing ids.
    for (kind, id) in validated {
        let url = format!("{API}/{kind}/{id}");
        match http::get_json(&url).await? {
            http::Outcome::NotFound => continue,
            http::Outcome::Body(v) => {
                // One malformed upstream row drops out of the batch
                // rather than failing every ref it rode in with —
                // same omission as a genuinely-absent resource, plus
                // a sanitized diagnostic.
                let Some(o) = v.as_object() else {
                    warn("deezer: catalog row body is not an object").await?;
                    continue;
                };
                let same = match parse::id_matches(o, &id) {
                    Ok(same) => same,
                    Err(_) => {
                        warn("deezer: catalog row id malformed").await?;
                        continue;
                    }
                };
                // A body naming a different id is not this ref's
                // resource — the ref resolves to nothing.
                if !same {
                    continue;
                }
                let row = match kind.as_str() {
                    "track" => parse::track_row(&v, None, None),
                    "album" => parse::album_row(&v, None),
                    _ => parse::artist_row(&v),
                };
                match row {
                    Some(row) => items.push(parse::to_metadata(&row)),
                    None => warn("deezer: malformed catalog row dropped").await?,
                }
            }
        }
    }
    Ok(json!({ "items": items }))
}

async fn entity(payload: &Value) -> Result<Value, GuestError> {
    let obj = payload_obj(payload, &["ref"])?;
    let (kind, id) = deezer_ref(&obj["ref"], &["album", "artist", "playlist"])?;
    match kind.as_str() {
        "album" => album_entity(&id).await,
        "playlist" => playlist_entity(&id).await,
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
    let mut related: Vec<Value> = Vec::new();
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

    // Discography rail: `/artist/{id}/albums` rows carry no artist
    // sub-object, so they inherit the page artist's name as their
    // subtitle. Entity rows ride `related`, never `items`.
    let albums_url = format!("{API}/artist/{id}/albums?limit={ARTIST_ALBUMS_LIMIT}");
    match section(&albums_url).await? {
        Section::Body(v) => match parse::grouped_entity_items(&v, "discography", |row| {
            parse::album_hit(row, Some(name.as_str()))
        }) {
            Ok(mut list) => {
                if parse::has_next(&v) {
                    complete = false;
                }
                related.append(&mut list);
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

    // Related-artists rail.
    let related_url = format!("{API}/artist/{id}/related?limit={ARTIST_RELATED_LIMIT}");
    match section(&related_url).await? {
        Section::Body(v) => match parse::grouped_entity_items(&v, "related", parse::artist_hit) {
            Ok(mut list) => {
                if parse::has_next(&v) {
                    complete = false;
                }
                related.append(&mut list);
            }
            Err(_) => {
                complete = false;
                warn("deezer artist related section unavailable").await?;
            }
        },
        Section::Degraded => {
            complete = false;
            warn("deezer artist related section unavailable").await?;
        }
    }

    Ok(json!({
        "entity": entity,
        "items": items,
        "related": related,
        "complete": complete,
    }))
}

async fn playlist_entity(id: &str) -> Result<Value, GuestError> {
    let v = match http::get_json(&format!("{API}/playlist/{id}")).await? {
        http::Outcome::NotFound => {
            return Err(failed(
                "no-result",
                format!("deezer playlist {id} not found"),
            ));
        }
        http::Outcome::Body(v) => v,
    };
    match parse::playlist_page(&v, id)? {
        Some(page) => Ok(json!({
            "entity": page.entity,
            "items": page.items,
            "complete": page.complete,
        })),
        None => Err(failed(
            "no-result",
            format!("deezer playlist {id} not found"),
        )),
    }
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
    const SEARCH_ARTIST: &str = include_str!("../fixtures/search-artist.json");
    const SEARCH_ALBUM: &str = include_str!("../fixtures/search-album.json");
    const SEARCH_PLAYLIST: &str = include_str!("../fixtures/search-playlist.json");
    const EMPTY: &str = include_str!("../fixtures/search-empty.json");
    const MALFORMED: &str = include_str!("../fixtures/search-malformed.json");
    const QUOTA: &str = include_str!("../fixtures/error-quota.json");
    const NO_DATA: &str = include_str!("../fixtures/error-no-data.json");
    const TRACK: &str = include_str!("../fixtures/track.json");
    const ALBUM: &str = include_str!("../fixtures/album.json");
    const ARTIST: &str = include_str!("../fixtures/artist.json");
    const ARTIST_TOP: &str = include_str!("../fixtures/artist-top.json");
    const ARTIST_ALBUMS: &str = include_str!("../fixtures/artist-albums.json");
    const ARTIST_RELATED: &str = include_str!("../fixtures/artist-related.json");
    const PLAYLIST: &str = include_str!("../fixtures/playlist.json");
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

    /// Feed `respond(request)` to every emitted `http_request` until a
    /// terminal output (`done`/`fail`), acking `log` calls and
    /// collecting their messages. Bounded by the step budget so a
    /// guest that never settles fails the test instead of hanging it.
    fn drive(out: Value, respond: impl Fn(&Value) -> Value) -> (Value, Vec<String>) {
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
        if url.contains("/search/artist") {
            SEARCH_ARTIST
        } else if url.contains("/search/album") {
            SEARCH_ALBUM
        } else {
            SEARCH_PLAYLIST
        }
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
        // A mixed ask fans out to the three typed entity endpoints.
        let (out, _) = drive(out, |req| http_ok(req_id(req), search_section(req)));
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

        // Entity rails: two artists, one album, one playlist — in
        // fetch order, each carrying its entityMetadata shape.
        let entities = entities_of(&out);
        assert_eq!(entities.len(), 4, "{entities:?}");
        assert_eq!(entities[0]["kind"], "artist");
        assert_eq!(entities[0]["title"], "Portishead");
        assert_eq!(entities[0]["subtitle"], "15 albums");
        assert_eq!(entities[0]["source_ref"]["id"], "1069");
        assert_eq!(entities[1]["title"], "Portis");
        assert_eq!(entities[2]["kind"], "album");
        assert_eq!(entities[2]["title"], "Dummy");
        assert_eq!(entities[2]["subtitle"], "Portishead");
        assert_eq!(entities[3]["kind"], "playlist");
        assert_eq!(entities[3]["title"], "Trip-Hop Classics");
        assert_eq!(entities[3]["subtitle"], "Deezer");
        // No exact title match → no hero.
        assert_eq!(out["result"]["top_hit"], Value::Null);
        // The artist fixture's own `next` advertises index 2 — the
        // offset the token carries, not `off + limit`. Kinds that
        // exhausted ride as done markers so they never refetch.
        let token: Value =
            serde_json::from_str(out["result"]["continuation"].as_str().unwrap_or_default())
                .unwrap_or_else(|_| panic!("continuation is not JSON: {out}"));
        assert_eq!(token["artist"], 2, "{token}");
        for kind in ["track", "album", "playlist"] {
            assert_eq!(token[kind], Value::Null, "{kind}: {token}");
        }
    }

    /// `kinds` scopes the ask: one endpoint, the request's own limit,
    /// and entity rails only for the kinds asked for.
    #[test]
    fn search_kinds_scopes_to_one_call() {
        let out = invoke(
            "catalog.search",
            json!({"query": "x", "limit": 7, "storefront": null, "kinds": ["album"]}),
        );
        assert_eq!(out["kind"], "http_request", "{out}");
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(
            url, "https://api.deezer.com/search/album?q=x&limit=7",
            "{url}"
        );
        let out = step(&http_ok(req_id(&out), SEARCH_ALBUM));
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
        let entities = entities_of(&out);
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0]["kind"], "album");
        assert_eq!(out["result"]["continuation"], Value::Null);
    }

    /// A `track`-only kinds ask keeps the legacy single-call shape —
    /// no entity endpoints are touched.
    #[test]
    fn search_track_only_is_the_legacy_call() {
        let out = invoke(
            "catalog.search",
            json!({"query": "x", "limit": 5, "storefront": null, "kinds": ["track"]}),
        );
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/search?q=x&limit=5", "{url}");
        let out = step(&http_ok(req_id(&out), EMPTY));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(entities_of(&out).len(), 0);
    }

    /// An exact title match on an entity rail wins the hero slot.
    #[test]
    fn search_exact_artist_is_top_hit() {
        let req = search_request(json!({"query": "portishead", "limit": 5, "storefront": null}));
        let (out, _) = drive(step(&http_ok(req_id(&req), SEARCH)), |req| {
            http_ok(req_id(req), search_section(req))
        });
        assert_eq!(out["type"], "done", "{out}");
        let hit = &out["result"]["top_hit"];
        assert_eq!(hit["type"], "entity", "{out}");
        assert_eq!(hit["item"]["kind"], "artist");
        assert_eq!(hit["item"]["source_ref"]["id"], "1069");
    }

    /// No entity match but an exact track title takes the hero slot.
    #[test]
    fn search_exact_track_is_top_hit() {
        let req = search_request(json!({"query": "roads", "limit": 5, "storefront": null}));
        let (out, _) = drive(step(&http_ok(req_id(&req), SEARCH)), |req| {
            http_ok(req_id(req), search_section(req))
        });
        assert_eq!(out["type"], "done", "{out}");
        let hit = &out["result"]["top_hit"];
        assert_eq!(hit["type"], "track", "{out}");
        assert_eq!(hit["item"]["source_ref"]["kind"], "track");
        assert_eq!(hit["item"]["title"], "Roads");
    }

    /// Section weather on one rail degrades it, keeping its offset —
    /// the page still reports every kind that answered.
    #[test]
    fn search_section_failure_keeps_its_offset() {
        let req = search_request(json!({"query": "x", "limit": 5, "storefront": null}));
        let (out, logs) = drive(step(&http_ok(req_id(&req), SEARCH)), |req| {
            let url = req["payload"]["url"].as_str().unwrap_or_default();
            if url.contains("/search/album") {
                http_status(req_id(req), 500, &[])
            } else {
                http_ok(req_id(req), search_section(req))
            }
        });
        assert_eq!(out["type"], "done", "{out}");
        let entities = entities_of(&out);
        assert!(
            entities.iter().all(|e| e["kind"] != "album"),
            "{entities:?}"
        );
        assert!(entities.iter().any(|e| e["kind"] == "playlist"));
        // The failed rail retries at the offset it never advanced.
        let token: Value =
            serde_json::from_str(out["result"]["continuation"].as_str().unwrap_or_default())
                .unwrap_or_else(|_| panic!("continuation is not JSON: {out}"));
        assert_eq!(token["album"], 0, "{out}");
        assert!(logs.iter().any(|m| m.contains("section unavailable")));
    }

    /// A continuation token pages its kinds forward on `index`.
    #[test]
    fn search_continuation_pages_forward() {
        let token = r#"{"track":10,"album":5}"#;
        let out = invoke(
            "catalog.search",
            json!({"query": "x", "limit": 10, "storefront": null,
                   "kinds": ["track", "album"], "continuation": token}),
        );
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(
            url.contains("/search?") && url.contains("index=10"),
            "{url}"
        );
        let out = step(&http_ok(req_id(&out), EMPTY));
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert!(
            url.contains("/search/album?") && url.contains("index=5"),
            "{url}"
        );
        let out = step(&http_ok(req_id(&out), EMPTY));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["continuation"], Value::Null);
    }

    /// A `null` entry marks a kind done: it never refetches on later
    /// pages, and when nothing stays pending the page ends the
    /// continuation honestly.
    #[test]
    fn search_continuation_skips_done_kinds() {
        let token = r#"{"track":null,"artist":2}"#;
        let out = invoke(
            "catalog.search",
            json!({"query": "x", "limit": 10, "storefront": null,
                   "kinds": ["track", "artist"], "continuation": token}),
        );
        // The one and only call is the artist rail at its offset —
        // track's done marker skips its fetch entirely.
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(
            url, "https://api.deezer.com/search/artist?q=x&limit=10&index=2",
            "{url}"
        );
        let out = step(&http_ok(req_id(&out), EMPTY));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["continuation"], Value::Null);
    }

    /// Done markers carry across pages — a kind exhausted on an
    /// earlier page stays marked while a deeper rail keeps paging.
    #[test]
    fn search_continuation_carries_done_markers() {
        let token = r#"{"track":null,"artist":2}"#;
        let out = invoke(
            "catalog.search",
            json!({"query": "x", "limit": 10, "storefront": null,
                   "kinds": ["track", "artist"], "continuation": token}),
        );
        // An advancing next keeps the rail pending; track's marker
        // rides alongside unchanged.
        let mut page: Value = serde_json::from_str(SEARCH_ARTIST)
            .unwrap_or_else(|_| panic!("SEARCH_ARTIST fixture is not JSON"));
        page["next"] = json!("https://api.deezer.com/search/artist?q=x&index=4");
        let body = serde_json::to_string(&page).unwrap_or_default();
        let out = step(&http_ok(req_id(&out), &body));
        assert_eq!(out["type"], "done", "{out}");
        let next: Value =
            serde_json::from_str(out["result"]["continuation"].as_str().unwrap_or_default())
                .unwrap_or_else(|_| panic!("continuation is not JSON: {out}"));
        assert_eq!(next["artist"], 4, "{next}");
        assert_eq!(next["track"], Value::Null, "{next}");
    }

    /// A `next` that can't move the offset forward ends the rail
    /// rather than emitting a self-referential token that refetches
    /// the same page forever.
    #[test]
    fn search_stuck_next_ends_the_rail() {
        let out = invoke(
            "catalog.search",
            json!({"query": "x", "limit": 10, "storefront": null,
                   "kinds": ["artist"], "continuation": "{\"artist\":10}"}),
        );
        // Empty data and a next link with no index — the fallback
        // offset equals the current one, so the rail is done.
        let body = r#"{"data":[],"next":"https://api.deezer.com/search/artist?q=x"}"#;
        let out = step(&http_ok(req_id(&out), body));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["continuation"], Value::Null);
    }

    /// Foreign or malformed tokens are payload errors, never a
    /// request.
    #[test]
    fn search_bad_continuation_is_invalid_response() {
        for token in [
            "not json",
            r#"{"bogus":3}"#,
            r#"{"track":"3"}"#,
            r#"{"track":-1}"#,
            "[]",
        ] {
            let out = invoke(
                "catalog.search",
                json!({"query": "x", "limit": 5, "storefront": null, "continuation": token}),
            );
            assert_eq!(out["type"], "fail", "{token}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{token}");
        }
    }

    /// `kinds` is the contract enum — empty, wrong-typed, or
    /// out-of-enum asks are payload errors before any request.
    #[test]
    fn search_bad_kinds_is_invalid_response() {
        for kinds in [
            json!([]),
            json!("album"),
            json!(["album", "bogus"]),
            json!([1]),
            Value::Null,
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
    fn search_empty_is_done_with_no_items() {
        let req = search_request(json!({"query": "x", "limit": 5, "storefront": null}));
        let (out, _) = drive(step(&http_ok(req_id(&req), EMPTY)), |req| {
            http_ok(req_id(req), EMPTY)
        });
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
        assert!(entities_of(&out).is_empty());
        assert_eq!(out["result"]["top_hit"], Value::Null);
        assert_eq!(out["result"]["continuation"], Value::Null);
    }

    #[test]
    fn malformed_body_is_invalid_response() {
        for body in [MALFORMED, "not json {"] {
            let req = search_request(json!({"query": "x", "limit": 5, "storefront": null}));
            // Every kind degrades on the malformed body — all-failed
            // propagates the first failure.
            let (out, _) = drive(step(&http_ok(req_id(&req), body)), |req| {
                http_ok(req_id(req), body)
            });
            assert_eq!(out["type"], "fail", "{body}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{body}");
        }
    }

    #[test]
    fn rate_limit_status_logs_then_fails() {
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        // Every kind hits the same quota — all-failed propagates the
        // typed verdict.
        let (out, logs) = drive(
            step(&http_status(req_id(&req), 429, &[("Retry-After", "30")])),
            |req| http_status(req_id(req), 429, &[("Retry-After", "30")]),
        );
        // The retry hint rides both the diagnostic log and the fail
        // message — the message is the only channel back to the app.
        assert!(
            logs.iter().any(|m| m.contains("retry_after=30")),
            "{logs:?}"
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "rate-limit");
        assert_eq!(
            out["error"]["message"].as_str(),
            Some("rate-limit: deezer status 429 retry_after=30")
        );
    }

    #[test]
    fn quota_envelope_is_rate_limit() {
        // Deezer answers quota exhaustion as 200 + an error envelope.
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let (out, _) = drive(step(&http_ok(req_id(&req), QUOTA)), |req| {
            http_ok(req_id(req), QUOTA)
        });
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "rate-limit");
    }

    #[test]
    fn rate_limit_survives_a_failed_log_call() {
        // The log channel's own failure must never displace the typed
        // verdict: a warn request answered `host_error` still leaves
        // `rate-limit`, not the channel's `transient`.
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let mut out = step(&http_status(req_id(&req), 429, &[("Retry-After", "30")]));
        // Every log call answers `transient`; every request 429s.
        for _ in 0..32 {
            match (out["type"].as_str(), out["kind"].as_str()) {
                (Some("host_request"), Some("log")) => {
                    out = step(&json!({
                        "type": "host_error", "id": req_id(&out),
                        "error": {"kind": "transient", "message": "log channel down"},
                    }));
                }
                (Some("host_request"), Some("http_request")) => {
                    out = step(&http_status(req_id(&out), 429, &[]));
                }
                _ => break,
            }
        }
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "rate-limit", "{out}");
    }

    #[test]
    fn cancelled_log_call_propagates_over_rate_limit() {
        // Terminal kinds outrank the typed verdict: `cancelled` is the
        // abort signal, never a rate-limit.
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
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
            let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
            let (out, _) = drive(step(&http_status(req_id(&req), status, &[])), |req| {
                http_status(req_id(req), status, &[])
            });
            assert_eq!(out["type"], "fail", "{status}");
            assert_eq!(out["error"]["kind"], "transient", "{status}");
        }
    }

    #[test]
    fn unknown_error_envelope_is_transient() {
        let req = search_request(json!({"query": "x", "limit": 1, "storefront": null}));
        let (out, _) = drive(
            step(&http_ok(
                req_id(&req),
                r#"{"error":{"type":"ParameterException","message":"bad parameter","code":501}}"#,
            )),
            |req| {
                http_ok(
                    req_id(req),
                    r#"{"error":{"type":"ParameterException","message":"bad parameter","code":501}}"#,
                )
            },
        );
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
        let (out, _) = drive(step(&http_status(req_id(&req), 404, &[])), |req| {
            http_status(req_id(req), 404, &[])
        });
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

    /// A malformed upstream row drops out of the batch — the
    /// surviving refs still resolve, each drop carrying a sanitized
    /// diagnostic.
    #[test]
    fn metadata_drops_malformed_rows_keeps_rest() {
        let out = invoke(
            "catalog.metadata",
            json!({"refs": [
                {"provider": "deezer", "kind": "track", "id": "982668"},
                {"provider": "deezer", "kind": "track", "id": "42"},
                {"provider": "deezer", "kind": "track", "id": "43"},
                {"provider": "deezer", "kind": "album", "id": "109301"},
            ]}),
        );
        // A good track.
        let out = feed(&out, TRACK);
        // A JSON body that is not a resource object.
        let out = feed(&out, r#"["not","an","object"]"#);
        // A track object whose title is unusable.
        let out = feed(&out, r#"{"id":43,"type":"track","title":"  "}"#);
        // A good album.
        let out = feed(&out, ALBUM);
        assert_eq!(out["type"], "done", "{out}");
        let items = items_of(&out);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["source_ref"]["id"], "982668");
        assert_eq!(items[1]["source_ref"]["id"], "109301");
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

    /// The host admits 32 HTTP calls per invocation, so a batch
    /// bigger than `METADATA_FETCH_MAX` cannot complete — it is
    /// rejected before any request rather than returning a silently
    /// truncated prefix.
    #[test]
    fn metadata_over_fetch_max_rejected_before_http() {
        let refs: Vec<Value> = (1..=35_u64)
            .map(|i| json!({"provider": "deezer", "kind": "track", "id": i.to_string()}))
            .collect();
        let out = invoke("catalog.metadata", json!({"refs": refs}));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "invalid-response");
    }

    /// A malformed ref rejects the batch — validation runs before any
    /// request is issued.
    #[test]
    fn metadata_malformed_ref_fails() {
        let refs: Vec<Value> = vec![
            json!({"provider": "deezer", "kind": "track", "id": "1"}),
            json!({"provider": "deezer", "kind": "track", "id": "abc"}),
        ];
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
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(
            url,
            format!("https://api.deezer.com/artist/1069/related?limit={ARTIST_RELATED_LIMIT}"),
            "{url}"
        );

        let out = step(&http_ok(req_id(&out), ARTIST_RELATED));
        assert_eq!(out["type"], "done", "{out}");

        let entity = &out["result"]["entity"];
        assert_eq!(entity["kind"], "artist");
        assert_eq!(entity["title"], "Portishead");
        assert_eq!(entity["subtitle"], "15 albums");
        assert_eq!(entity["artwork"][0]["width"], 1000);

        let items = items_of(&out);
        // `items` carries the top tracks only — entity rows ride
        // `related`, never the track listing.
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["source_ref"]["kind"], "track");
        assert_eq!(items[0]["title"], "Glory Box");
        assert_eq!(items[1]["source_ref"]["kind"], "track");

        let related = related_of(&out);
        assert_eq!(related.len(), 4, "{related:?}");
        assert_eq!(related[0]["kind"], "album");
        assert_eq!(related[0]["group"], "discography");
        assert_eq!(related[0]["title"], "Third");
        assert_eq!(related[0]["subtitle"], "Portishead");
        assert_eq!(related[0]["source_ref"]["id"], "455045");
        assert_eq!(related[1]["kind"], "album");
        assert_eq!(related[1]["group"], "discography");
        assert_eq!(related[2]["kind"], "artist");
        assert_eq!(related[2]["group"], "related");
        assert_eq!(related[2]["title"], "Massive Attack");
        assert_eq!(related[3]["title"], "Tricky");
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
        let out = step(&http_ok(req_id(&out), ARTIST_RELATED));
        assert_eq!(out["type"], "done", "{out}");
        assert!(items_of(&out).is_empty());
        let related = related_of(&out);
        assert_eq!(related.len(), 4, "{related:?}");
        assert_eq!(related[0]["group"], "discography");
        assert_eq!(related[2]["group"], "related");
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
        let out = feed(&out, ARTIST_RELATED);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert_eq!(related_of(&out).len(), 4);
        assert!(items_of(&out).is_empty());
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
        let out = feed(&out, ARTIST_RELATED);
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert_eq!(items_of(&out).len(), 1);
        assert_eq!(related_of(&out).len(), 4);
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
        let out = feed(&out, ARTIST_RELATED);
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
    fn entity_playlist_carries_tracks() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "playlist", "id": "908622995"}}),
        );
        let url = out["payload"]["url"].as_str().unwrap_or_default();
        assert_eq!(url, "https://api.deezer.com/playlist/908622995", "{url}");
        let out = step(&http_ok(req_id(&out), PLAYLIST));
        assert_eq!(out["type"], "done", "{out}");

        let entity = &out["result"]["entity"];
        assert_eq!(entity["kind"], "playlist");
        assert_eq!(entity["title"], "Trip-Hop Classics");
        // The curator is the playlist's subtitle — the line that
        // marks it from an album.
        assert_eq!(entity["subtitle"], "Deezer");
        assert_eq!(entity["source_ref"]["kind"], "playlist");

        let items = items_of(&out);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["title"], "Roads");
        assert_eq!(items[0]["source_ref"]["kind"], "track");
        assert_eq!(out["result"]["complete"], true);
    }

    #[test]
    fn entity_playlist_missing_is_no_result() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "playlist", "id": "42"}}),
        );
        let out = step(&http_ok(req_id(&out), NO_DATA));
        assert_eq!(out["type"], "fail", "{out}");
        assert_eq!(out["error"]["kind"], "no-result");
    }

    /// A playlist body whose `tracks` section is short flags
    /// `complete:false`, same as the album page.
    #[test]
    fn entity_playlist_short_tracks_incomplete() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "playlist", "id": "908622995"}}),
        );
        let body = r#"{"id":908622995,"type":"playlist","title":"T","nb_tracks":50,
            "tracks":{"data":[{"id":1,"type":"track","title":"Only One"}]}}"#;
        let out = step(&http_ok(req_id(&out), body));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert_eq!(items_of(&out).len(), 1);
    }

    /// `tracks.next` marks the listing truncated even when `nb_tracks`
    /// agrees with the rows delivered.
    #[test]
    fn entity_playlist_tracks_next_incomplete() {
        let out = invoke(
            "catalog.entity",
            json!({"ref": {"provider": "deezer", "kind": "playlist", "id": "908622995"}}),
        );
        let body = r#"{"id":908622995,"type":"playlist","title":"T","nb_tracks":1,
            "tracks":{"data":[{"id":1,"type":"track","title":"Only One"}],
            "next":"https://api.deezer.com/playlist/908622995/tracks?index=1"}}"#;
        let out = step(&http_ok(req_id(&out), body));
        assert_eq!(out["type"], "done", "{out}");
        assert_eq!(out["result"]["complete"], false);
        assert_eq!(items_of(&out).len(), 1);
    }

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
        let (out, _) = drive(step(&http_ok(req_id(&req), EMPTY)), |req| {
            http_ok(req_id(req), EMPTY)
        });
        assert_eq!(out["type"], "done");
    }

    /// Only a delta-seconds `Retry-After` reaches the diagnostic log and
    /// the fail message; oversized, malformed, and control-containing
    /// values are dropped while the invocation still fails `rate-limit`.
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
            let (out, logs) = drive(
                step(&http_status(req_id(&req), 429, &[("Retry-After", value)])),
                |req| http_status(req_id(req), 429, &[("Retry-After", value)]),
            );
            assert!(logs.iter().any(|m| m == want), "{value}: {logs:?}");
            assert_eq!(out["error"]["kind"], "rate-limit", "{value}");
            let want_fail = if want.contains("retry_after") {
                format!("rate-limit: deezer status 429 retry_after={value}")
            } else {
                "rate-limit: deezer status 429".to_string()
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
