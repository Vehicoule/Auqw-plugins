//! `catalog.search` / `catalog.entity` over the WEB_REMIX InnerTube
//! client — the anonymous catalog surface of `playback.candidates`
//! and `catalog.suggest`.
//!
//! An unfiltered `search` answers a mixed ask in one request: the
//! top-result card plus per-kind shelves land under `top_hit`,
//! `entities`, and `items`. A `kinds`-scoped ask re-queries under
//! that kind's `params` — one request per kind, section-weather
//! tolerated per rail exactly like the deezer/itunes guests.
//! `catalog.entity` is `browse` on the browseId minted into entity
//! refs (`MPREb_` albums, `UC` artists, `VL` playlists); album and
//! playlist pages carry their tracklist, artist pages carry top songs
//! plus carousel rails under `related`.
//!
//! First page only: InnerTube continuation mechanics differ between
//! `search` and `browse` — `continuation` is always null and a
//! supplied one is a foreign token (`invalid-response`).

use serde_json::{json, Map, Value};

use auqw_guest_sdk::{http_request, GuestError, HttpRequest};

use crate::candidates::{
    best_artwork_obj, browse, collect_renderers, column_runs, duration_ms_of, get_path,
    is_furniture, run_text, runs_text, text_of, video_id_of, web_remix_context, web_remix_request,
    MAX_DEPTH, MAX_NODES, SEARCH_URL, SONGS_PARAMS, VISITOR_KEY,
};
use crate::guest::{bad_payload, failed, is_terminal_kind, kv_set_soft, load_visitor, warn};
use crate::parse::{visitor_data, visitor_token};

/// InnerTube `params` selecting each kind's filter — the songs one
/// already serves `playback.candidates`.
const ALBUMS_PARAMS: &str = "EgWKAQIYAWoMEA4QChADEAQQCRAF";
const ARTISTS_PARAMS: &str = "EgWKAQIgAWoMEA4QChADEAQQCRAF";
const PLAYLISTS_PARAMS: &str = "EgWKAQIoAWoMEA4QChADEAQQCRAF";

const BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false";

/// Kinds this provider serves, in fetch order — the track backbone
/// first, then the entity rails.
const SEARCH_KINDS: &[&str] = &["track", "album", "artist", "playlist"];

/// A browseId is opaque but bounded: visible ASCII id chars, sane
/// length — the same alphabet as video ids at playlist length.
fn is_browse_id(s: &str) -> bool {
    (2..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn kind_params(kind: &str) -> Option<&'static str> {
    match kind {
        "track" => Some(SONGS_PARAMS),
        "album" => Some(ALBUMS_PARAMS),
        "artist" => Some(ARTISTS_PARAMS),
        "playlist" => Some(PLAYLISTS_PARAMS),
        _ => None,
    }
}

/// `MUSIC_PAGE_TYPE_*` → the contract entity kind; other page types
/// (user channels, podcasts, episodes) aren't servable kinds.
fn kind_of_page_type(pt: &str) -> Option<&'static str> {
    match pt {
        "MUSIC_PAGE_TYPE_ALBUM" => Some("album"),
        "MUSIC_PAGE_TYPE_ARTIST" => Some("artist"),
        "MUSIC_PAGE_TYPE_PLAYLIST" => Some("playlist"),
        _ => None,
    }
}

/// A `browseEndpoint` object → `(kind, browseId)` when both decode.
fn endpoint_of(b: &Map<String, Value>) -> Option<(String, String)> {
    let id = b.get("browseId")?.as_str()?;
    let pt = get_path(
        b,
        &[
            "browseEndpointContextSupportedConfigs",
            "browseEndpointContextMusicConfig",
            "pageType",
        ],
    )?
    .as_str()?;
    let kind = kind_of_page_type(pt)?;
    if !is_browse_id(id) {
        return None;
    }
    Some((kind.into(), id.into()))
}

/// Joined run text of a byline column — type labels stay (they are
/// the descriptor), bare separators and whitespace drop.
fn byline(col: &Value) -> Option<String> {
    let runs = column_runs(col);
    if runs.is_empty() {
        return None;
    }
    let joined: String = runs
        .iter()
        .filter_map(|r| run_text(r))
        .filter(|t| !t.trim().is_empty() && t.trim() != "•")
        .collect::<Vec<_>>()
        .join(" • ");
    let t = joined.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.to_string())
}

/// One search/browse track row → `trackMetadata`. Rows are either
/// `musicResponsiveListItemRenderer` (search shelves, shelf pages) or
/// `playlistPanelVideoRenderer` (playlist/album/artist listings) —
/// both funnel through the same metadata shape.
fn track_row_of(r: &Map<String, Value>) -> Option<Value> {
    if r.contains_key("playlistPanelVideoRenderer") {
        return r
            .get("playlistPanelVideoRenderer")
            .and_then(Value::as_object)
            .and_then(panel_row_of);
    }
    let video_id = video_id_of(r)?;
    let flex = r
        .get("flexColumns")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let title = flex.first().and_then(text_of)?;
    if title.chars().count() > 512 {
        return None;
    }
    let mut artist: Option<String> = None;
    let mut artist_ref: Option<String> = None;
    let mut album: Option<String> = None;
    let mut album_ref: Option<String> = None;
    let mut second_col_text: Option<String> = None;
    let mut duration_ms: Option<u64> = None;
    let fixed = r.get("fixedColumns").and_then(Value::as_array);
    let columns = flex.iter().skip(1).chain(fixed.into_iter().flatten());
    for (ci, col) in columns.enumerate() {
        for run in column_runs(col) {
            let Some(text) = run_text(run).map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some((kind, id)) = browse(run).and_then(endpoint_of) {
                match kind.as_str() {
                    "artist" if artist_ref.is_none() => {
                        artist_ref = Some(id);
                        artist = Some(text.to_string());
                    }
                    "album" if album_ref.is_none() => {
                        album_ref = Some(id);
                        album = Some(text.to_string());
                    }
                    _ => {}
                }
            }
            if duration_ms.is_none() {
                duration_ms = duration_ms_of(text);
            }
            // The artist fallback keeps names and artist links —
            // a validated album endpoint must not report itself as
            // the track's artist.
            if ci == 0
                && second_col_text.is_none()
                && !is_furniture(text)
                && browse(run)
                    .and_then(endpoint_of)
                    .is_none_or(|(kind, _)| kind != "album")
            {
                second_col_text = Some(text.to_string());
            }
        }
    }
    let artist = artist.or(second_col_text);
    let artwork = best_artwork_obj(r).into_iter().collect::<Vec<Value>>();
    Some(json!({
        "source_ref": { "provider": "youtube-music", "kind": "track", "id": video_id },
        "title": title,
        "artist": artist,
        "album": album,
        "artist_ref": artist_ref
            .map(|id| json!({"provider": "youtube-music", "kind": "artist", "id": id})),
        "album_ref": album_ref
            .map(|id| json!({"provider": "youtube-music", "kind": "album", "id": id})),
        "duration_ms": duration_ms,
        "release_year": null,
        "artwork": artwork,
        "explicit": null,
        "genre": null,
        "storefront": null,
    }))
}

/// A `playlistPanelVideoRenderer` row — the entity-page track listing
/// shape: byline columns carry artist (short) and album (long).
fn panel_row_of(r: &Map<String, Value>) -> Option<Value> {
    let video_id = r
        .get("videoId")
        .and_then(Value::as_str)
        .filter(|s| crate::guest::is_video_id(s))
        .map(str::to_string)
        .or_else(|| video_id_of(r))?;
    let title = runs_text(r.get("title")?)?;
    if title.chars().count() > 512 {
        return None;
    }
    let byline_of = |key: &str| -> (Option<String>, Option<String>) {
        let mut name = None;
        let mut ref_id = None;
        for run in r
            .get(key)
            .and_then(|b| b.get("runs"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let text = run_text(run).map(str::trim).unwrap_or("");
            if text.is_empty() || text == "•" {
                continue;
            }
            if let Some((_, id)) = browse(run).and_then(endpoint_of) {
                if ref_id.is_none() {
                    ref_id = Some(id);
                }
            }
            if name.is_none() {
                name = Some(text.to_string());
            }
        }
        (name, ref_id)
    };
    let (artist, artist_ref) = byline_of("shortBylineText");
    let (album, album_ref) = byline_of("longBylineText");
    let duration_ms = r
        .get("lengthText")
        .and_then(runs_text)
        .and_then(|t| duration_ms_of(&t));
    let artwork = best_artwork_obj(r).into_iter().collect::<Vec<Value>>();
    Some(json!({
        "source_ref": { "provider": "youtube-music", "kind": "track", "id": video_id },
        "title": title,
        "artist": artist,
        "album": album,
        "artist_ref": artist_ref
            .map(|id| json!({"provider": "youtube-music", "kind": "artist", "id": id})),
        "album_ref": album_ref
            .map(|id| json!({"provider": "youtube-music", "kind": "album", "id": id})),
        "duration_ms": duration_ms,
        "release_year": null,
        "artwork": artwork,
        "explicit": null,
        "genre": null,
        "storefront": null,
    }))
}

/// One `musicResponsiveListItemRenderer` (or a two-row carousel cell)
/// → `entityMetadata` when its endpoint is a servable page type.
/// `group` tags the rail this row belongs to on entity pages.
fn entity_row_of(r: &Map<String, Value>, group: Option<&str>) -> Option<Value> {
    let nav = if let Some(c) = r.get("musicTwoRowItemRenderer").and_then(Value::as_object) {
        c
    } else {
        r
    };
    let (kind, id) = nav
        .get("navigationEndpoint")
        .and_then(|n| n.get("browseEndpoint"))
        .and_then(Value::as_object)
        .and_then(endpoint_of)
        .or_else(|| {
            // Carousel cells can put the endpoint on the title run.
            nav.get("title")
                .and_then(|t| t.get("runs"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
                .find_map(|run| browse(run).and_then(endpoint_of))
        })?;
    let title = nav
        .get("flexColumns")
        .and_then(Value::as_array)
        .and_then(|f| f.first())
        .and_then(text_of)
        .or_else(|| nav.get("title").and_then(runs_text))?;
    if title.chars().count() > 512 {
        return None;
    }
    let subtitle = nav
        .get("flexColumns")
        .and_then(Value::as_array)
        .and_then(|f| f.get(1))
        .and_then(byline)
        .or_else(|| nav.get("subtitle").and_then(runs_text));
    let artwork = best_artwork_obj(nav).into_iter().collect::<Vec<Value>>();
    let mut m = json!({
        "source_ref": { "provider": "youtube-music", "kind": kind, "id": id },
        "kind": kind,
        "title": title,
        "subtitle": subtitle,
        "artwork": artwork,
    });
    if let Some(g) = group {
        m["group"] = json!(g);
    }
    Some(m)
}

/// Bounded first-match walk for a key — entity cards nest their
/// endpoints at arbitrary depth.
fn find_key<'a>(v: &'a Value, key: &str, depth: usize, nodes: &mut usize) -> Option<&'a Value> {
    if depth > MAX_DEPTH || *nodes >= MAX_NODES {
        return None;
    }
    *nodes += 1;
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                if k == key {
                    return Some(child);
                }
                if let Some(found) = find_key(child, key, depth + 1, nodes) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(|c| find_key(c, key, depth + 1, nodes)),
        _ => None,
    }
}

/// The `musicCardShelfRenderer` top result: its item is an
/// `musicResponsiveListItemRenderer` row — an entity when its
/// endpoint is a servable page type, a track on a watch id.
fn card_hit(card: &Map<String, Value>) -> Value {
    for c in card
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(r) = c
            .get("musicResponsiveListItemRenderer")
            .and_then(Value::as_object)
        else {
            continue;
        };
        if let Some(e) = entity_row_of(r, None) {
            return json!({ "type": "entity", "item": e });
        }
        if let Some(t) = track_row_of(r) {
            return json!({ "type": "track", "item": t });
        }
    }
    Value::Null
}

/// A title's matchable form: lowercase, Latin diacritics folded to
/// base letters, non-alphanumeric runs collapsed to single spaces, a
/// leading "the" dropped. "Beyoncé" and "beyonce", "THE  BEATLES"
/// and "the-beatles" all compare alike.
fn normalize_title(s: &str) -> String {
    fn fold(c: char) -> &'static str {
        match c {
            'à'..='å' | 'ā' | 'ă' | 'ą' => "a",
            'æ' => "ae",
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
            'ď' | 'đ' | 'ð' => "d",
            'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
            'ĥ' | 'ħ' => "h",
            'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
            'ĵ' => "j",
            'ķ' | 'ĸ' => "k",
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
            'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' => "n",
            'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
            'œ' => "oe",
            'ŕ' | 'ŗ' | 'ř' => "r",
            'ś' | 'ŝ' | 'ş' | 'š' => "s",
            'ß' => "ss",
            'ţ' | 'ť' | 'ŧ' => "t",
            'þ' => "th",
            'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
            'ŵ' => "w",
            'ý' | 'ÿ' => "y",
            'ź' | 'ż' | 'ž' => "z",
            _ => "",
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.trim().chars() {
        let folded = fold(c);
        if folded.is_empty() {
            for m in c.to_lowercase() {
                if m.is_alphanumeric() {
                    if pending_space && !out.is_empty() {
                        out.push(' ');
                    }
                    pending_space = false;
                    out.push(m);
                } else {
                    pending_space = true;
                }
            }
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push_str(folded);
        }
    }
    match out.strip_prefix("the ") {
        Some(rest) => rest.to_string(),
        None => out,
    }
}

/// The title with trailing `(…)`/`[…]` groups dropped — "Roads (2009
/// Remaster)" still names "roads". Nested/leading groups keep the
/// full title: a cut only counts when plain text precedes it.
fn core_title(s: &str) -> &str {
    match s.find(['(', '[']) {
        Some(i) if s[..i].trim().is_empty() => s,
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

/// Hero match strength, strongest first: 2 = normalized equal, 1 =
/// the title starts with the query, 0 = the title contains it. The
/// loose tiers only fire on a ≥3-char normalized query — a one- or
/// two-letter ask matching by substring is noise, not a top hit.
fn title_match(query: &str, title: &str) -> Option<u8> {
    let q = normalize_title(query);
    let t = normalize_title(title);
    let core = normalize_title(core_title(title));
    if t == q || core == q {
        return Some(2);
    }
    if q.chars().count() < 3 {
        return None;
    }
    if t.starts_with(&q) || core.starts_with(&q) {
        return Some(1);
    }
    if t.contains(&q) || core.contains(&q) {
        return Some(0);
    }
    None
}

/// A shelf's title → the contract kind its rows are; `None` for
/// non-contract sections (videos, profiles, podcasts, episodes).
fn kind_of_shelf_title(t: &str) -> Option<&'static str> {
    match t {
        "Songs" | "Top songs" => Some("track"),
        "Albums" => Some("album"),
        "Artists" => Some("artist"),
        "Playlists" | "Community playlists" | "Featured playlists" => Some("playlist"),
        _ => None,
    }
}

/// The `sectionListRenderer.contents` array wherever the response
/// nests it (tab wrapper shapes differ between search and browse).
/// `None` means the body carried no recognizable results container
/// at all — a present-but-empty array is a legitimate empty page.
fn section_list(body: &Value) -> Option<Vec<&Value>> {
    let mut nodes = 0usize;
    find_key(body, "sectionListRenderer", 0, &mut nodes)
        .and_then(|s| s.get("contents"))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
}

/// Every `musicShelfRenderer` / `musicCarouselShelfRenderer` in the
/// section list, as `(title, contents)` — carousels title via
/// `musicCarouselShelfBasicHeaderRenderer`.
fn shelves_of<'a>(sections: &[&'a Value]) -> Vec<(String, Vec<&'a Value>)> {
    let mut out = Vec::new();
    for s in sections {
        for key in [
            "musicShelfRenderer",
            "musicCarouselShelfRenderer",
            "musicPlaylistShelfRenderer",
            "itemSectionRenderer",
        ] {
            let Some(shelf) = s.get(key).and_then(Value::as_object) else {
                continue;
            };
            // The shelf's own title — a bounded walk would find a
            // row's title first.
            let title = shelf
                .get("title")
                .and_then(runs_text)
                .or_else(|| {
                    shelf
                        .get("header")
                        .and_then(|h| h.get("musicCarouselShelfBasicHeaderRenderer"))
                        .and_then(|h| h.get("title"))
                        .and_then(runs_text)
                })
                .unwrap_or_default();
            let contents = shelf
                .get("contents")
                .and_then(Value::as_array)
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            out.push((title, contents));
        }
    }
    out
}

/// Extract the item renderers from one shelf's contents — each entry
/// is `{<rendererName>: <renderer>}`; entity rows map through
/// `entity_row_of` (two-row cells included), track rows through
/// `track_row_of` (list items and panels).
fn rows_of<'a>(contents: &[&'a Value]) -> Vec<&'a Map<String, Value>> {
    let mut renderers = Vec::new();
    let mut nodes = 0usize;
    for c in contents {
        collect_renderers(c, &mut renderers, 0, &mut nodes);
        // Panels and two-row cells don't nest under
        // musicResponsiveListItemRenderer — collect them at the top
        // level of each content entry.
        if let Value::Object(o) = c {
            for (k, v) in o {
                if (k == "playlistPanelVideoRenderer" || k == "musicTwoRowItemRenderer")
                    && v.is_object()
                {
                    renderers.push(o);
                }
            }
        }
    }
    renderers
}

/// What one section fetch returned: a parseable 2xx body, or the
/// section is unavailable — carrying the typed failure so a lone
/// section's verdict (e.g. `rate-limit`) survives as the
/// invocation's when every fetched section fails.
enum Section {
    Body(Value),
    Degraded(GuestError),
}

async fn fetch_json(req: HttpRequest, label: &str) -> Result<Section, GuestError> {
    let resp = match http_request(req).await {
        Ok(r) => r,
        Err(GuestError::Host { kind, message }) => {
            if is_terminal_kind(&kind) {
                return Err(GuestError::Host { kind, message });
            }
            return Ok(Section::Degraded(failed(
                "transient",
                "section transport".into(),
            )));
        }
        Err(e) => return Err(e),
    };
    match resp.status {
        s if (200..300).contains(&s) => {}
        429 => return Ok(Section::Degraded(failed("rate-limit", "rate-limit".into()))),
        s => {
            return Ok(Section::Degraded(failed(
                "transient",
                format!("section status {s}"),
            )))
        }
    }
    match serde_json::from_slice::<Value>(&resp.body) {
        Ok(v) if v.is_object() => {
            if let Some(raw) = visitor_data(&v) {
                if let Some(vtok) = visitor_token(&raw) {
                    kv_set_soft(VISITOR_KEY, Some(vtok.as_bytes())).await?;
                } else {
                    warn("ignoring malformed visitor value").await?;
                }
            }
            Ok(Section::Body(v))
        }
        _ => {
            warn(label).await?;
            Ok(Section::Degraded(failed(
                "invalid-response",
                "section body".into(),
            )))
        }
    }
}

/// The WEB_REMIX `search` call; `params` selects a kind filter, `None`
/// asks for the unfiltered mixed page.
fn search_request(query: &str, params: Option<&str>, visitor: Option<&str>) -> HttpRequest {
    let mut body = json!({
        "context": web_remix_context(),
        "query": query,
    });
    if let Some(p) = params {
        body["params"] = json!(p);
    }
    web_remix_request(SEARCH_URL, body, visitor, None)
}

/// Fold one shelf's rows into the accumulators. Rows discriminate
/// themselves: a servable browse endpoint lands in `entities`, a
/// watch id in `items`, anything else drops. `want` scopes the ask
/// — a scoped "album" query keeps album rows only; `None` is the
/// unfiltered page (everything it served).
fn fold_rows(
    contents: &[&Value],
    want: Option<&str>,
    items: &mut Vec<Value>,
    entities: &mut Vec<Value>,
) {
    for r in rows_of(contents) {
        if let Some(e) = entity_row_of(r, None) {
            if want.is_none_or(|w| e["kind"].as_str() == Some(w)) {
                entities.push(e);
            }
            continue;
        }
        if want.is_none_or(|w| w == "track") {
            if let Some(t) = track_row_of(r) {
                items.push(t);
            }
        }
    }
}

pub async fn search(payload: &Value) -> Result<Value, GuestError> {
    let obj = crate::guest::payload_keys(
        payload,
        &["query", "limit", "storefront", "kinds", "continuation"],
        &["query", "limit", "storefront"],
    )?;
    for opt in ["kinds", "continuation"] {
        if obj.get(opt) == Some(&Value::Null) {
            return Err(bad_payload("optional key must be absent, not null"));
        }
    }
    let query = obj["query"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if query.is_empty() || query.chars().count() > 512 {
        return Err(bad_payload(
            "query must be a nonempty string of at most 512 characters",
        ));
    }
    let limit = obj["limit"]
        .as_u64()
        .ok_or_else(|| bad_payload("limit must be an integer"))?
        .clamp(1, 200) as usize;
    // `storefront` validates like the catalog peers even though this
    // provider serves one global catalog — the value isn't
    // forwarded upstream, but a malformed payload is still
    // `invalid-response`.
    match &obj["storefront"] {
        Value::Null => {}
        Value::String(sf) if sf.len() == 2 && sf.bytes().all(|b| b.is_ascii_alphabetic()) => {}
        _ => {
            return Err(bad_payload(
                "storefront must be null or a two-letter country",
            ))
        }
    }
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
    // This phase never emits a continuation — a present one is a
    // foreign token.
    if let Some(v) = obj.get("continuation") {
        if !v.is_string() {
            return Err(bad_payload("continuation must be a string"));
        }
        return Err(bad_payload("continuation is not a valid token"));
    }
    let scoped = obj.contains_key("kinds");
    let visitor = load_visitor(VISITOR_KEY).await?;

    let mut items: Vec<Value> = Vec::new();
    let mut entities: Vec<Value> = Vec::new();
    let mut top_hit = Value::Null;
    let mut first_failure: Option<GuestError> = None;
    let mut fetched = 0usize;
    let mut failures = 0usize;

    if !scoped {
        // One unfiltered request: the card plus every kind's shelf.
        fetched = 1;
        match fetch_json(
            search_request(&query, None, visitor.as_deref()),
            "ytm search body is not a JSON object",
        )
        .await?
        {
            Section::Body(body) => match section_list(&body) {
                Some(sections) => {
                    for s in sections {
                        if let Some(card) =
                            s.get("musicCardShelfRenderer").and_then(Value::as_object)
                        {
                            if top_hit.is_null() {
                                top_hit = card_hit(card);
                            }
                            continue;
                        }
                        for (_title, contents) in shelves_of(&[s]) {
                            fold_rows(&contents, None, &mut items, &mut entities);
                        }
                    }
                }
                // A 200 body that isn't a results page — `{}` or an
                // error envelope is invalid-response, never a clean
                // zero-result search.
                None => {
                    failures = 1;
                    first_failure = Some(failed(
                        "invalid-response",
                        "search body carried no results section".into(),
                    ));
                    warn("ytm search body carried no results section").await?;
                }
            },
            Section::Degraded(e) => {
                failures = 1;
                first_failure = Some(e);
                warn("ytm search section unavailable").await?;
            }
        }
    } else {
        for kind in &kinds {
            fetched += 1;
            let Some(params) = kind_params(kind) else {
                continue;
            };
            match fetch_json(
                search_request(&query, Some(params), visitor.as_deref()),
                "ytm search body is not a JSON object",
            )
            .await?
            {
                Section::Body(body) => match section_list(&body) {
                    Some(sections) => {
                        // Each scoped ask caps at the request's own
                        // limit — a shared cap would let an earlier
                        // kind starve the later ones.
                        let mut kind_items = Vec::new();
                        let mut kind_entities = Vec::new();
                        for s in sections {
                            for (_title, contents) in shelves_of(&[s]) {
                                fold_rows(
                                    &contents,
                                    Some(kind),
                                    &mut kind_items,
                                    &mut kind_entities,
                                );
                            }
                        }
                        kind_items.truncate(limit);
                        kind_entities.truncate(limit);
                        items.extend(kind_items);
                        entities.extend(kind_entities);
                    }
                    None => {
                        failures += 1;
                        if first_failure.is_none() {
                            first_failure = Some(failed(
                                "invalid-response",
                                "search body carried no results section".into(),
                            ));
                        }
                        warn("ytm search body carried no results section").await?;
                    }
                },
                Section::Degraded(e) => {
                    failures += 1;
                    if first_failure.is_none() {
                        first_failure = Some(e);
                    }
                    warn("ytm search section unavailable").await?;
                }
            }
        }
    }
    // The failed rail is absent; every fetched section failing is the
    // invocation's failure.
    if fetched > 0 && failures == fetched {
        return Err(first_failure
            .take()
            .unwrap_or_else(|| failed("transient", "ytm search".into())));
    }
    // Unfiltered caps the mixed page at the request limit; scoped
    // asks already capped each kind inside its own section loop.
    if !scoped {
        items.truncate(limit);
        entities.truncate(limit);
    }
    // Fallback hero when the card was absent or unservable: strongest
    // normalized title match — entities before tracks at each tier.
    if top_hit.is_null() {
        for tier in [2u8, 1, 0] {
            if let Some(e) = entities.iter().find(|e| {
                e["title"]
                    .as_str()
                    .is_some_and(|t| title_match(&query, t) == Some(tier))
            }) {
                top_hit = json!({ "type": "entity", "item": e });
                break;
            }
            if let Some(t) = items.iter().find(|t| {
                t["title"]
                    .as_str()
                    .is_some_and(|t| title_match(&query, t) == Some(tier))
            }) {
                top_hit = json!({ "type": "track", "item": t });
                break;
            }
        }
    }
    Ok(json!({
        "items": items,
        "entities": entities,
        "top_hit": top_hit,
        "continuation": Value::Null,
        "storefront": null,
    }))
}

/// A `browse` call for one entity page.
fn browse_request(browse_id: &str, visitor: Option<&str>) -> HttpRequest {
    let body = json!({
        "context": web_remix_context(),
        "browseId": browse_id,
    });
    web_remix_request(BROWSE_URL, body, visitor, None)
}

/// The page header wherever it nests — album/playlist detail headers
/// and the artist's immersive header all carry `title` + `subtitle`.
fn header_of(body: &Value) -> Option<(String, Option<String>, Vec<Value>)> {
    for key in [
        "musicDetailHeaderRenderer",
        "musicImmersiveHeaderRenderer",
        "musicEditablePlaylistDetailHeaderRenderer",
    ] {
        let mut nodes = 0usize;
        let Some(h) = find_key(body, key, 0, &mut nodes).and_then(Value::as_object) else {
            continue;
        };
        let title = h.get("title").and_then(runs_text).or_else(|| {
            h.get("title")
                .and_then(|t| t.get("musicCardShelfTitleBasicRenderer"))
                .and_then(|t| t.get("title"))
                .and_then(runs_text)
        })?;
        if title.chars().count() > 512 {
            return None;
        }
        let subtitle = h.get("subtitle").and_then(|s| {
            let runs = s.get("runs")?.as_array()?;
            let joined: String = runs
                .iter()
                .filter_map(|r| r.get("text").and_then(Value::as_str))
                .filter(|t| {
                    let t = t.trim();
                    !t.is_empty() && t != "•" && !is_furniture(t)
                })
                .collect::<Vec<_>>()
                .join(" • ");
            let t = joined.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        });
        let artwork = best_artwork_obj(h).into_iter().collect::<Vec<Value>>();
        return Some((title, subtitle, artwork));
    }
    None
}

/// Carousel shelf title → the `related` group its rows ride under.
fn group_of_carousel_title(t: &str) -> Option<&'static str> {
    match t {
        "Albums" | "Singles" | "EPs" | "Singles & EPs" | "Singles and EPs" => Some("discography"),
        "Featured on" | "Appears on" | "Appears On" => Some("appears-on"),
        "Fans might also like" | "Fans may also like" | "Related artists" => Some("related"),
        _ => None,
    }
}

pub async fn entity(payload: &Value) -> Result<Value, GuestError> {
    let obj = crate::guest::payload_keys(payload, &["ref"], &["ref"])?;
    let r = payload_obj_ref(&obj["ref"])?;
    let visitor = load_visitor(VISITOR_KEY).await?;
    let resp = match http_request(browse_request(&r.1, visitor.as_deref())).await {
        Ok(r) => r,
        Err(GuestError::Host { kind, message }) => {
            return Err(GuestError::Host { kind, message });
        }
        Err(e) => return Err(e),
    };
    let body = match resp.status {
        s if (200..300).contains(&s) => serde_json::from_slice::<Value>(&resp.body)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| failed("invalid-response", "browse body is not JSON".into()))?,
        429 => return Err(failed("rate-limit", "rate-limit".into())),
        400 | 404 => return Err(failed("no-result", "entity not found".into())),
        _ => return Err(failed("transient", "browse transport".into())),
    };
    if let Some(raw) = visitor_data(&body) {
        if let Some(vtok) = visitor_token(&raw) {
            kv_set_soft(VISITOR_KEY, Some(vtok.as_bytes())).await?;
        } else {
            warn("ignoring malformed visitor value").await?;
        }
    }
    let (title, subtitle, artwork) = header_of(&body)
        .ok_or_else(|| failed("no-result", "entity page carried no header".into()))?;
    let entity = json!({
        "source_ref": { "provider": "youtube-music", "kind": r.0, "id": r.1 },
        "kind": r.0,
        "title": title,
        "subtitle": subtitle,
        "artwork": artwork,
    });

    let mut items: Vec<Value> = Vec::new();
    let mut related: Vec<Value> = Vec::new();
    for (title, contents) in shelves_of(&section_list(&body).unwrap_or_default()) {
        match r.0.as_str() {
            "artist" => {
                // Top-songs shelf → items; carousels → related rails.
                match kind_of_shelf_title(&title) {
                    Some("track") => {
                        for row in rows_of(&contents) {
                            if let Some(t) = track_row_of(row) {
                                items.push(t);
                            }
                        }
                    }
                    _ => {
                        if let Some(g) = group_of_carousel_title(&title) {
                            for row in rows_of(&contents) {
                                if let Some(e) = entity_row_of(row, Some(g)) {
                                    related.push(e);
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                // Album/playlist pages: every musicShelfRenderer row is
                // the tracklist; carousels (rare on albums) group under
                // their shelf title when it's a contract rail.
                for row in rows_of(&contents) {
                    if let Some(t) = track_row_of(row) {
                        items.push(t);
                    } else if let Some(g) = group_of_carousel_title(&title) {
                        if let Some(e) = entity_row_of(row, Some(g)) {
                            related.push(e);
                        }
                    }
                }
            }
        }
    }
    // `complete` claims the page delivered in full — a continuation
    // marker on any served section means a listing is paginated and
    // what came back isn't the whole tracklist.
    let complete = {
        let mut nodes = 0usize;
        find_key(&body, "continuationItemRenderer", 0, &mut nodes).is_none() && {
            let mut nodes = 0usize;
            find_key(&body, "nextContinuationData", 0, &mut nodes).is_none()
        }
    };
    Ok(json!({
        "entity": entity,
        "items": items,
        "related": related,
        "complete": complete,
    }))
}

/// `ref` must be a `youtube-music` entity ref of kind
/// album/artist/playlist with a browse-id id — anything else is
/// `not-applicable` (foreign provider) or `invalid-response`.
fn payload_obj_ref(v: &Value) -> Result<(String, String), GuestError> {
    let o =
        crate::guest::payload_keys(v, &["provider", "kind", "id"], &["provider", "kind", "id"])?;
    let provider = o["provider"]
        .as_str()
        .ok_or_else(|| bad_payload("ref.provider must be a string"))?;
    let kind = o["kind"]
        .as_str()
        .ok_or_else(|| bad_payload("ref.kind must be a string"))?;
    if provider != "youtube-music" || !matches!(kind, "album" | "artist" | "playlist") {
        return Err(failed(
            "not-applicable",
            "ref is not a youtube-music album/artist/playlist ref".into(),
        ));
    }
    let id = o["id"]
        .as_str()
        .filter(|s| is_browse_id(s))
        .ok_or_else(|| bad_payload("ref.id must be a browse id"))?;
    Ok((kind.to_string(), id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use auqw_guest_sdk::{dispatch_step, reset_for_testing};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use std::collections::BTreeMap;

    const SEARCH_MIXED: &str = include_str!("../fixtures/catalog-search-mixed.json");
    const SEARCH_EMPTY: &str = include_str!("../fixtures/search-empty.json");
    const SEARCH_ALBUMS: &str = include_str!("../fixtures/catalog-search-albums.json");
    const BROWSE_ALBUM: &str = include_str!("../fixtures/catalog-browse-album.json");
    const BROWSE_ARTIST: &str = include_str!("../fixtures/catalog-browse-artist.json");
    const BROWSE_PLAYLIST: &str = include_str!("../fixtures/catalog-browse-playlist.json");

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

        fn invoke(&mut self, capability: &str, payload: Value) -> Value {
            reset_for_testing();
            let out = step(&json!({
                "type": "invoke", "request_id": "t", "capability": capability,
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

    /// The matcher's tiers: normalized equal (diacritics, case,
    /// punctuation, leading "the", edition parentheticals), then
    /// prefix, then substring — both loose tiers gated on ≥3 chars.
    #[test]
    fn title_match_tiers() {
        assert_eq!(title_match("roads", "Roads"), Some(2));
        assert_eq!(title_match("roads", "Roads (2009 Remaster)"), Some(2));
        assert_eq!(title_match("beyonce", "Beyoncé"), Some(2));
        assert_eq!(title_match("beatles", "The Beatles"), Some(2));
        assert_eq!(title_match("trip-hop", "Trip-Hop Classics"), Some(1));
        assert_eq!(title_match("phonk", "Brazilian Phonk Mano"), Some(0));
        assert_eq!(title_match("ab", "Abbey Road"), None);
        assert_eq!(title_match("jazz", "Portishead"), None);
    }

    #[test]
    fn mixed_search_maps_card_shelves_and_top_hit() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "portishead", "limit": 20, "storefront": null }),
        );
        assert_eq!(out["kind"], "http_request");
        // One unfiltered request — no params filter.
        assert!(out["payload"]["body"]
            .as_str()
            .and_then(|b| B64.decode(b).ok())
            .is_some_and(|b| !String::from_utf8_lossy(&b).contains("params")));
        let out = h.answer(&out, 200, SEARCH_MIXED);
        assert_eq!(out["type"], "done");
        let result = &out["result"];
        // The card is the artist top result.
        assert_eq!(result["top_hit"]["type"], "entity");
        assert_eq!(result["top_hit"]["item"]["kind"], "artist");
        assert_eq!(result["top_hit"]["item"]["title"], "Portishead");
        assert_eq!(
            result["top_hit"]["item"]["source_ref"]["id"],
            "UCportishead9"
        );
        // Entities across the rails; the Videos shelf's row is a
        // playable track like the Songs shelf's.
        let Some(entities) = result["entities"].as_array() else {
            panic!("entities array");
        };
        assert_eq!(entities.len(), 3);
        assert_eq!(entities[0]["kind"], "album");
        assert_eq!(entities[0]["title"], "Dummy");
        assert_eq!(entities[0]["subtitle"], "Album • Portishead • 1994");
        assert_eq!(entities[1]["kind"], "artist");
        assert_eq!(entities[2]["kind"], "playlist");
        let Some(items) = result["items"].as_array() else {
            panic!("items array");
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["title"], "Roads");
        assert_eq!(
            items[0]["artist_ref"]["id"], "UCportishead9",
            "track rows carry artist/album refs"
        );
        assert_eq!(items[0]["album_ref"]["id"], "MPREb_dummy");
        assert_eq!(items[0]["duration_ms"], 302_000);
        assert_eq!(result["continuation"], Value::Null);
        assert!(h.committed.contains_key(VISITOR_KEY));
    }

    #[test]
    fn scoped_albums_search_emits_only_album_entities() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "dummy", "limit": 10, "storefront": null,
                    "kinds": ["album"] }),
        );
        assert_eq!(out["kind"], "http_request");
        let body = B64
            .decode(out["payload"]["body"].as_str().unwrap_or(""))
            .unwrap_or_default();
        assert!(String::from_utf8_lossy(&body).contains(ALBUMS_PARAMS));
        let out = h.answer(&out, 200, SEARCH_ALBUMS);
        let result = &out["result"];
        let Some(entities) = result["entities"].as_array() else {
            panic!("entities array");
        };
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0]["source_ref"]["id"], "MPREb_dummy");
        assert_eq!(entities[1]["source_ref"]["id"], "MPREb_third");
        assert_eq!(result["items"].as_array().map(Vec::len), Some(0));
        // Case-folded title match is the scoped hero.
        assert_eq!(result["top_hit"]["type"], "entity");
        assert_eq!(result["top_hit"]["item"]["title"], "Dummy");
    }

    #[test]
    fn scoped_track_search_drops_entity_rows() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "portishead", "limit": 10, "storefront": null,
                    "kinds": ["track"] }),
        );
        assert_eq!(out["kind"], "http_request");
        let body = B64
            .decode(out["payload"]["body"].as_str().unwrap_or(""))
            .unwrap_or_default();
        assert!(String::from_utf8_lossy(&body).contains(SONGS_PARAMS));
        let out = h.answer(&out, 200, SEARCH_ALBUMS);
        let result = &out["result"];
        assert_eq!(result["items"].as_array().map(Vec::len), Some(0));
        assert_eq!(result["entities"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn all_scoped_sections_failing_surfaces_first_failure() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": null,
                    "kinds": ["track", "album"] }),
        );
        let out = h.answer(&out, 429, "{}");
        assert_eq!(out["kind"], "http_request", "second kind still fetches");
        let out = h.answer(&out, 429, "{}");
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "rate-limit");
    }

    #[test]
    fn foreign_continuation_and_bad_kinds_are_rejected() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": null,
                    "continuation": "abc" }),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": null,
                    "kinds": ["genre"] }),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
    }

    #[test]
    fn album_entity_maps_header_and_tracklist() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.entity",
            json!({ "ref": { "provider": "youtube-music", "kind": "album",
                             "id": "MPREb_dummy" } }),
        );
        assert_eq!(out["kind"], "http_request");
        assert!(out["payload"]["url"]
            .as_str()
            .unwrap_or("")
            .contains("/browse?"));
        let body = B64
            .decode(out["payload"]["body"].as_str().unwrap_or(""))
            .unwrap_or_default();
        assert!(String::from_utf8_lossy(&body).contains("\"MPREb_dummy\""));
        let out = h.answer(&out, 200, BROWSE_ALBUM);
        assert_eq!(out["type"], "done");
        let result = &out["result"];
        assert_eq!(result["entity"]["kind"], "album");
        assert_eq!(result["entity"]["title"], "Dummy");
        assert_eq!(result["entity"]["subtitle"], "Portishead • 1994");
        let Some(items) = result["items"].as_array() else {
            panic!("items array");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "Mysterons");
        assert_eq!(items[0]["artist_ref"]["id"], "UCportishead9");
        assert_eq!(items[0]["album_ref"]["id"], "MPREb_dummy");
        assert_eq!(items[0]["duration_ms"], 302_000);
        assert_eq!(result["complete"], true);
    }

    #[test]
    fn artist_entity_maps_top_songs_and_related_rails() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.entity",
            json!({ "ref": { "provider": "youtube-music", "kind": "artist",
                             "id": "UCportishead9" } }),
        );
        let out = h.answer(&out, 200, BROWSE_ARTIST);
        assert_eq!(out["type"], "done");
        let result = &out["result"];
        assert_eq!(result["entity"]["kind"], "artist");
        assert_eq!(result["entity"]["title"], "Portishead");
        let Some(items) = result["items"].as_array() else {
            panic!("items array");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], "Roads");
        let Some(related) = result["related"].as_array() else {
            panic!("related array");
        };
        assert_eq!(related.len(), 3);
        assert_eq!(related[0]["kind"], "album");
        assert_eq!(related[0]["title"], "Dummy");
        assert_eq!(related[0]["group"], "discography");
        assert_eq!(related[1]["kind"], "playlist");
        assert_eq!(related[1]["group"], "appears-on");
        assert_eq!(related[2]["kind"], "artist");
        assert_eq!(related[2]["group"], "related");
        assert_eq!(result["complete"], true);
    }

    #[test]
    fn playlist_entity_maps_tracklist() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.entity",
            json!({ "ref": { "provider": "youtube-music", "kind": "playlist",
                             "id": "VLPLportisheadmix" } }),
        );
        let out = h.answer(&out, 200, BROWSE_PLAYLIST);
        assert_eq!(out["type"], "done");
        let result = &out["result"];
        assert_eq!(result["entity"]["kind"], "playlist");
        assert_eq!(result["entity"]["title"], "This Is Portishead");
        assert_eq!(result["items"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn foreign_entity_ref_is_not_applicable() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.entity",
            json!({ "ref": { "provider": "deezer", "kind": "album", "id": "1" } }),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "not-applicable");
        let out = h.invoke(
            "catalog.entity",
            json!({ "ref": { "provider": "youtube-music", "kind": "track",
                             "id": "roadsvideo0" } }),
        );
        assert_eq!(out["error"]["kind"], "not-applicable");
    }

    #[test]
    fn browse_404_is_no_result() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.entity",
            json!({ "ref": { "provider": "youtube-music", "kind": "album",
                             "id": "MPREb_gone" } }),
        );
        assert_eq!(out["kind"], "http_request");
        let out = h.answer(&out, 404, "{}");
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "no-result");
    }

    #[test]
    fn continuation_marker_marks_entity_incomplete() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.entity",
            json!({ "ref": { "provider": "youtube-music", "kind": "playlist",
                             "id": "VLPLportisheadmix" } }),
        );
        // A paginated playlist: the tracklist shelf carries a
        // continuationItemRenderer — the page isn't the whole truth.
        let mut body: Value =
            serde_json::from_str(BROWSE_PLAYLIST).unwrap_or_else(|e| panic!("fixture json: {e}"));
        let mut nodes = 0usize;
        let Some(shelf) = find_key(&body, "musicPlaylistShelfRenderer", 0, &mut nodes)
            .and_then(Value::as_object)
            .cloned()
        else {
            panic!("playlist shelf");
        };
        let mut shelf = shelf;
        let Some(contents) = shelf["contents"].as_array_mut() else {
            panic!("shelf contents");
        };
        contents.push(json!({
            "continuationItemRenderer": {
                "continuationEndpoint": {
                    "continuationCommand": { "token": "CAES" }
                }
            }
        }));
        // Walk to the shelf's slot and swap in the paginated copy.
        let mut nodes = 0usize;
        let target = find_key_mut(&mut body, "musicPlaylistShelfRenderer", 0, &mut nodes)
            .unwrap_or_else(|| panic!("shelf slot"));
        *target = json!(shelf);
        let out = h.answer(&out, 200, &body.to_string());
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["complete"], false);
        let Some(items) = out["result"]["items"].as_array() else {
            panic!("items array");
        };
        assert_eq!(items.len(), 2, "the delivered rows still surface");
    }

    /// find_key's mutable twin — test-only, rewrites one node.
    fn find_key_mut<'a>(
        v: &'a mut Value,
        key: &str,
        depth: usize,
        nodes: &mut usize,
    ) -> Option<&'a mut Value> {
        if depth > MAX_DEPTH || *nodes >= MAX_NODES {
            return None;
        }
        *nodes += 1;
        match v {
            Value::Object(o) => {
                if o.contains_key(key) {
                    return o.get_mut(key);
                }
                for (_k, child) in o.iter_mut() {
                    if matches!(child, Value::Object(_) | Value::Array(_)) {
                        if let Some(found) = find_key_mut(child, key, depth + 1, nodes) {
                            return Some(found);
                        }
                    }
                }
                None
            }
            Value::Array(a) => a
                .iter_mut()
                .find_map(|c| find_key_mut(c, key, depth + 1, nodes)),
            _ => None,
        }
    }

    #[test]
    fn empty_object_body_is_invalid_response_not_empty_results() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": null }),
        );
        let out = h.answer(&out, 200, "{}");
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
    }

    #[test]
    fn honest_empty_search_page_is_done_not_failure() {
        // A present-but-empty results container is a legitimate "no
        // matches" page — the same shape candidates' search-empty
        // fixture uses — not a malformed body.
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": null }),
        );
        let out = h.answer(&out, 200, SEARCH_EMPTY);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["items"], json!([]));
        assert_eq!(out["result"]["entities"], json!([]));
        // Scoped asks hold the same verdict — every kind's page was
        // valid, just empty, so nothing failed.
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": null,
                    "kinds": ["track"] }),
        );
        let out = h.answer(&out, 200, SEARCH_EMPTY);
        assert_eq!(out["type"], "done");
    }

    #[test]
    fn malformed_storefront_is_rejected() {
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": 123 }),
        );
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": "USA" }),
        );
        assert_eq!(out["error"]["kind"], "invalid-response");
        // A two-letter code validates even though this provider
        // ignores it upstream.
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "x", "limit": 10, "storefront": "US",
                    "kinds": ["track"] }),
        );
        assert_eq!(out["kind"], "http_request");
    }

    #[test]
    fn scoped_multi_kind_caps_per_kind() {
        // kinds album+artist at limit 1: each kind's own filtered
        // request caps at 1 — the artist survives the album's cap.
        let mut h = Harness::new();
        let out = h.invoke(
            "catalog.search",
            json!({ "query": "portishead", "limit": 1, "storefront": null,
                    "kinds": ["album", "artist"] }),
        );
        fn artist_row(id: &str, name: &str) -> Value {
            json!({
                "musicResponsiveListItemRenderer": {
                    "navigationEndpoint": {"browseEndpoint": {
                        "browseId": id,
                        "browseEndpointContextSupportedConfigs": {
                            "browseEndpointContextMusicConfig": {
                                "pageType": "MUSIC_PAGE_TYPE_ARTIST"}}}},
                    "flexColumns": [{
                        "musicResponsiveListItemFlexColumnRenderer": {
                            "text": {"runs": [{"text": name}]}}}]
                }
            })
        }
        let artists_body = json!({
            "contents": {
                "sectionListRenderer": {
                    "contents": [{
                        "musicShelfRenderer": {
                            "title": {"runs": [{"text": "Artists"}]},
                            "contents": [
                                artist_row("UCportishead9", "Portishead"),
                                artist_row("UCmassive001", "Massive Attack"),
                            ]
                        }
                    }]
                }
            }
        })
        .to_string();
        let out = h.answer(&out, 200, SEARCH_ALBUMS);
        assert_eq!(out["kind"], "http_request");
        let out = h.answer(&out, 200, &artists_body);
        assert_eq!(out["type"], "done");
        let Some(entities) = out["result"]["entities"].as_array() else {
            panic!("entities array");
        };
        let kinds: Vec<&str> = entities.iter().filter_map(|e| e["kind"].as_str()).collect();
        assert_eq!(kinds, ["album", "artist"], "each kind keeps its own cap");
    }

    #[test]
    fn album_run_never_becomes_the_track_artist() {
        // A second column holding only an album link leaves `artist`
        // null rather than naming the album twice.
        let row = json!({
            "navigationEndpoint": {"watchEndpoint": {"videoId": "roadsvideo0"}},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {
                    "text": {"runs": [{"text": "Roads"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {
                    "text": {"runs": [
                        {"text": "Dummy", "navigationEndpoint": {"browseEndpoint": {
                            "browseId": "MPREb_dummy",
                            "browseEndpointContextSupportedConfigs": {
                                "browseEndpointContextMusicConfig": {
                                    "pageType": "MUSIC_PAGE_TYPE_ALBUM"}}}}}
                    ]}}}
            ]
        });
        let Some(r) = row.as_object() else {
            panic!("row object");
        };
        let Some(t) = track_row_of(r) else {
            panic!("track row");
        };
        assert_eq!(t["album"], "Dummy");
        assert_eq!(t["artist"], Value::Null);
        assert_eq!(t["album_ref"]["id"], "MPREb_dummy");
    }
}
