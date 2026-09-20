//! Strict mapping of Deezer bodies to normalized rows and the
//! `trackMetadata`/`entityMetadata` wire values built from them.
//!
//! Deezer durations are whole seconds — `duration_ms` is emitted in
//! milliseconds as the schema dictates. `isrc` is carried through when
//! the upstream object provides it; `preview` URLs are never read.

use std::collections::HashSet;

use auqw_guest_sdk::GuestError;
use serde_json::{Map, Value};

/// Edge length of the `cover_xl`/`picture_xl` artwork Deezer serves.
pub const ARTWORK_XL: u64 = 1000;

fn bad(m: &str) -> GuestError {
    GuestError::Failed {
        kind: "invalid-response".into(),
        message: format!("deezer: {m}"),
    }
}

/// One accepted upstream row normalized for `trackMetadata` emission —
/// a track, or an album/artist entity row (the widened `sourceRef`
/// kinds let catalog items carry entity refs).
pub struct Row {
    /// `source_ref.kind`.
    pub kind: &'static str,
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub release_year: Option<u64>,
    /// Verified-https `cover_xl`/`picture_xl` URL.
    pub artwork_xl: Option<String>,
    pub explicit: Option<bool>,
    pub genre: Option<String>,
    /// `artist_ref` target id; `None` when the row is the artist.
    pub artist_id: Option<String>,
    /// `album_ref` target id; `None` when the row is the album.
    pub album_id: Option<String>,
    pub isrc: Option<String>,
}

/// A parsed composite page: the `entityMetadata`, its `trackMetadata`
/// items, and the completeness verdict.
pub struct EntityPage {
    pub entity: Value,
    pub items: Vec<Value>,
    /// `false` when a page section is truncated or failed to parse.
    pub complete: bool,
}

/// The `data` array of a Deezer list envelope; a missing or non-array
/// `data` is `invalid-response`.
pub fn data_list(v: &Value) -> Result<&Vec<Value>, GuestError> {
    v.as_object()
        .and_then(|o| o.get("data"))
        .and_then(Value::as_array)
        .ok_or_else(|| bad("data missing or not an array"))
}

/// `true` when the list envelope advertises another page — the caller
/// fetched only the first, so the composite is truncated.
pub fn has_next(v: &Value) -> bool {
    v.as_object()
        .and_then(|o| o.get("next"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
}

/// The object's own `id` must equal `want` — a different id is
/// `Ok(false)` (upstream answered a different resource); a missing or
/// unparsable id is `invalid-response`.
pub fn id_matches(o: &Map<String, Value>, want: &str) -> Result<bool, GuestError> {
    match id_field(o, "id") {
        Some(got) => Ok(got == want),
        None => Err(bad("id missing or malformed")),
    }
}

/// A track object → a `track` row, or `None` when the object is not a
/// track or lacks a usable id/title. `genre`/`release_year` context is
/// inherited from the enclosing album page when the track doesn't
/// carry its own.
pub fn track_row(v: &Value, genre: Option<&str>, release_year: Option<u64>) -> Option<Row> {
    let o = v.as_object()?;
    if let Some(t) = o.get("type").and_then(Value::as_str) {
        if t != "track" {
            return None;
        }
    }
    let artist = sub(o, "artist");
    let album = sub(o, "album");
    Some(Row {
        kind: "track",
        id: id_field(o, "id")?,
        title: str_field(o, "title").or_else(|| str_field(o, "title_short"))?,
        artist: artist.and_then(|a| str_field(a, "name")),
        album: album.and_then(|a| str_field(a, "title")),
        duration_ms: u64_field(o, "duration").map(|s| s.saturating_mul(1000)),
        release_year: str_field(o, "release_date")
            .and_then(|d| year_of(&d))
            .or_else(|| {
                album
                    .and_then(|a| str_field(a, "release_date"))
                    .and_then(|d| year_of(&d))
            })
            .or(release_year),
        artwork_xl: album.and_then(|a| https_field(a, "cover_xl")),
        explicit: bool_field(o, "explicit_lyrics"),
        genre: genre.map(str::to_string),
        artist_id: artist.and_then(|a| id_field(a, "id")),
        album_id: album.and_then(|a| id_field(a, "id")),
        isrc: str_field(o, "isrc"),
    })
}

/// An album object → an `album` entity row. `ctx_artist` supplies the
/// artist `(name, id)` for `/artist/{id}/albums` rows, which carry no
/// artist sub-object of their own.
pub fn album_row(v: &Value, ctx_artist: Option<(&str, &str)>) -> Option<Row> {
    let o = v.as_object()?;
    if let Some(t) = o.get("type").and_then(Value::as_str) {
        if t != "album" {
            return None;
        }
    }
    let (artist, artist_id) = match sub(o, "artist") {
        Some(a) => (str_field(a, "name"), id_field(a, "id")),
        None => (
            ctx_artist.map(|(n, _)| n.to_string()),
            ctx_artist.map(|(_, i)| i.to_string()),
        ),
    };
    Some(Row {
        kind: "album",
        id: id_field(o, "id")?,
        title: str_field(o, "title")?,
        artist,
        album: None,
        duration_ms: u64_field(o, "duration").map(|s| s.saturating_mul(1000)),
        release_year: str_field(o, "release_date").and_then(|d| year_of(&d)),
        artwork_xl: https_field(o, "cover_xl"),
        explicit: bool_field(o, "explicit_lyrics"),
        genre: genre_name(o),
        artist_id,
        album_id: None,
        isrc: None,
    })
}

/// An artist object → an `artist` entity row.
pub fn artist_row(v: &Value) -> Option<Row> {
    let o = v.as_object()?;
    if let Some(t) = o.get("type").and_then(Value::as_str) {
        if t != "artist" {
            return None;
        }
    }
    Some(Row {
        kind: "artist",
        id: id_field(o, "id")?,
        title: str_field(o, "name")?,
        artist: None,
        album: None,
        duration_ms: None,
        release_year: None,
        artwork_xl: https_field(o, "picture_xl"),
        explicit: None,
        genre: None,
        artist_id: None,
        album_id: None,
        isrc: None,
    })
}

/// The `trackMetadata` wire value for one row. `storefront` is `null`:
/// Deezer serves one global catalog and the search `storefront`
/// parameter is validated but not applied upstream.
pub fn to_metadata(row: &Row) -> Value {
    let artwork: Vec<Value> = row
        .artwork_xl
        .iter()
        .map(|u| serde_json::json!({"url": u, "width": ARTWORK_XL, "height": ARTWORK_XL}))
        .collect();
    serde_json::json!({
        "source_ref": {
            "provider": "deezer",
            "kind": row.kind,
            "id": row.id,
        },
        "title": row.title,
        "artist": row.artist,
        "album": row.album,
        "duration_ms": row.duration_ms,
        "release_year": row.release_year,
        "artwork": artwork,
        "explicit": row.explicit,
        "genre": row.genre,
        "storefront": Value::Null,
        "artist_ref": entity_ref("artist", row.artist_id.as_deref()),
        "album_ref": entity_ref("album", row.album_id.as_deref()),
        "isrc": row.isrc,
    })
}

/// Search items: `data` track rows mapped through [`track_row`],
/// collapsed first-seen-wins by upstream id.
///
/// # Errors
/// `invalid-response` when `data` is missing or not an array.
pub fn search_items(v: &Value) -> Result<Vec<Value>, GuestError> {
    let mut seen = HashSet::new();
    Ok(data_list(v)?
        .iter()
        .filter_map(|r| track_row(r, None, None))
        .filter(|r| seen.insert(r.id.clone()))
        .map(|r| to_metadata(&r))
        .collect())
}

/// A `{data:[..]}` page of tracks → `trackMetadata` items.
///
/// # Errors
/// `invalid-response` when `data` is missing or not an array.
pub fn track_items(v: &Value) -> Result<Vec<Value>, GuestError> {
    Ok(data_list(v)?
        .iter()
        .filter_map(|r| track_row(r, None, None))
        .map(|r| to_metadata(&r))
        .collect())
}

/// A `{data:[..]}` page of albums → `album` entity rows; `ctx_artist`
/// is the page artist's `(name, id)`.
///
/// # Errors
/// `invalid-response` when `data` is missing or not an array.
pub fn album_items(v: &Value, ctx_artist: Option<(&str, &str)>) -> Result<Vec<Value>, GuestError> {
    Ok(data_list(v)?
        .iter()
        .filter_map(|r| album_row(r, ctx_artist))
        .map(|r| to_metadata(&r))
        .collect())
}

/// The album entity page. Required fields gate the page itself — a
/// wrong-id body is `Ok(None)` (`no-result` upstream), a malformed
/// album object is `invalid-response`. A missing or short `tracks`
/// section degrades to `complete:false` without failing the page.
///
/// # Errors
/// `invalid-response` when the body is not a usable album object.
pub fn album_page(v: &Value, want_id: &str) -> Result<Option<EntityPage>, GuestError> {
    let o = v
        .as_object()
        .ok_or_else(|| bad("album body is not an object"))?;
    if !id_matches(o, want_id)? {
        return Ok(None);
    }
    let title = str_field(o, "title").ok_or_else(|| bad("album title missing"))?;
    let genre = genre_name(o);
    let release_year = str_field(o, "release_date").and_then(|d| year_of(&d));
    let entity = entity_metadata(
        "album",
        want_id,
        &title,
        sub(o, "artist")
            .and_then(|a| str_field(a, "name"))
            .as_deref(),
        https_field(o, "cover_xl"),
    );

    let mut complete = true;
    let mut items = Vec::new();
    match o
        .get("tracks")
        .and_then(Value::as_object)
        .and_then(|t| t.get("data"))
        .and_then(Value::as_array)
    {
        Some(rows) => {
            items = rows
                .iter()
                .filter_map(|r| track_row(r, genre.as_deref(), release_year))
                .map(|r| to_metadata(&r))
                .collect();
            if u64_field(o, "nb_tracks").is_some_and(|nb| (rows.len() as u64) < nb) {
                complete = false;
            }
        }
        None => complete = false,
    }
    Ok(Some(EntityPage {
        entity,
        items,
        complete,
    }))
}

/// The artist `entityMetadata` plus the artist's name — the
/// `(name, id)` ctx its album rows inherit. `Ok(None)` when the body
/// names a different artist.
///
/// # Errors
/// `invalid-response` when the body is not a usable artist object.
pub fn artist_meta(v: &Value, want_id: &str) -> Result<Option<(Value, String)>, GuestError> {
    let o = v
        .as_object()
        .ok_or_else(|| bad("artist body is not an object"))?;
    if !id_matches(o, want_id)? {
        return Ok(None);
    }
    let name = str_field(o, "name").ok_or_else(|| bad("artist name missing"))?;
    let subtitle = u64_field(o, "nb_album").map(|n| {
        if n == 1 {
            "1 album".to_string()
        } else {
            format!("{n} albums")
        }
    });
    let entity = entity_metadata(
        "artist",
        want_id,
        &name,
        subtitle.as_deref(),
        https_field(o, "picture_xl"),
    );
    Ok(Some((entity, name)))
}

fn entity_metadata(
    kind: &str,
    id: &str,
    title: &str,
    subtitle: Option<&str>,
    artwork_xl: Option<String>,
) -> Value {
    let artwork: Vec<Value> = artwork_xl
        .iter()
        .map(|u| serde_json::json!({"url": u, "width": ARTWORK_XL, "height": ARTWORK_XL}))
        .collect();
    serde_json::json!({
        "source_ref": {
            "provider": "deezer",
            "kind": kind,
            "id": id,
        },
        "kind": kind,
        "title": title,
        "subtitle": subtitle,
        "artwork": artwork,
    })
}

fn entity_ref(kind: &str, id: Option<&str>) -> Value {
    match id {
        Some(id) => serde_json::json!({"provider": "deezer", "kind": kind, "id": id}),
        None => Value::Null,
    }
}

fn sub<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Map<String, Value>> {
    o.get(key).and_then(Value::as_object)
}

fn str_field(o: &Map<String, Value>, key: &str) -> Option<String> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A Deezer id: a JSON number, or a digit string, in u64 range and
/// nonzero. The id is interpolated into a URL path — nothing else is
/// accepted.
fn id_field(o: &Map<String, Value>, key: &str) -> Option<String> {
    let v = o.get(key)?;
    let id = v.as_u64().or_else(|| {
        v.as_str().and_then(|s| {
            if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                s.parse::<u64>().ok()
            } else {
                None
            }
        })
    })?;
    (id > 0).then(|| id.to_string())
}

fn u64_field(o: &Map<String, Value>, key: &str) -> Option<u64> {
    o.get(key).and_then(Value::as_u64)
}

fn bool_field(o: &Map<String, Value>, key: &str) -> Option<bool> {
    o.get(key).and_then(Value::as_bool)
}

fn https_field(o: &Map<String, Value>, key: &str) -> Option<String> {
    o.get(key)
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("https://"))
        .map(str::to_string)
}

/// `genres.data[0].name` — Deezer hangs genres off album objects only.
fn genre_name(o: &Map<String, Value>) -> Option<String> {
    o.get("genres")
        .and_then(Value::as_object)
        .and_then(|g| g.get("data"))
        .and_then(Value::as_array)
        .and_then(|d| d.first())
        .and_then(Value::as_object)
        .and_then(|g| str_field(g, "name"))
}

/// The four-digit year prefix of a `YYYY-MM-DD` release date.
fn year_of(s: &str) -> Option<u64> {
    let y = s.get(..4)?;
    if y.bytes().all(|b| b.is_ascii_digit()) {
        y.parse::<u64>().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn track_row_seconds_to_ms_and_isrc() {
        let row = track_row(
            &json!({
                "id": 982668, "type": "track", "title": "Roads",
                "duration": 303, "isrc": "GBAQT9400064",
                "explicit_lyrics": false,
                "artist": {"id": 1069, "name": "Portishead"},
                "album": {"id": 109301, "title": "Dummy",
                          "cover_xl": "https://cdn-images.dzcdn.net/x.jpg",
                          "release_date": "1994-01-01"},
            }),
            None,
            None,
        )
        .unwrap_or_else(|| panic!("track_row dropped a valid track"));
        assert_eq!(row.duration_ms, Some(303_000));
        assert_eq!(row.isrc.as_deref(), Some("GBAQT9400064"));
        assert_eq!(row.release_year, Some(1994));
        assert_eq!(row.artist_id.as_deref(), Some("1069"));
        assert_eq!(row.album_id.as_deref(), Some("109301"));
    }

    #[test]
    fn track_row_context_fallback_and_drops() {
        // Album-page track: no own release_date → inherits ctx year.
        let row = track_row(&json!({"id": 1, "title": "T"}), Some("Rock"), Some(1994));
        assert_eq!(row.map(|r| r.release_year), Some(Some(1994)));
        // No id, no title, wrong type, non-object → dropped.
        for v in [
            json!({"title": "x"}),
            json!({"id": 0, "title": "x"}),
            json!({"id": 7, "type": "episode", "title": "x"}),
            json!("a string is not a row"),
        ] {
            assert!(track_row(&v, None, None).is_none(), "{v}");
        }
    }

    #[test]
    fn album_row_uses_ctx_artist() {
        let row = album_row(
            &json!({"id": 455045, "type": "album", "title": "Third",
                    "release_date": "2008-04-28",
                    "cover_xl": "https://cdn-images.dzcdn.net/y.jpg"}),
            Some(("Portishead", "1069")),
        )
        .unwrap_or_else(|| panic!("album_row dropped a valid album"));
        assert_eq!(row.artist.as_deref(), Some("Portishead"));
        assert_eq!(row.artist_id.as_deref(), Some("1069"));
        assert_eq!(row.release_year, Some(2008));
        assert_eq!(row.genre, None);
    }

    #[test]
    fn id_field_accepts_digits_only() {
        let o = json!({"a": 7, "b": "42", "c": "0", "d": "1x", "e": -3, "f": 1.5});
        let m = o.as_object().unwrap_or_else(|| panic!("not an object"));
        assert_eq!(id_field(m, "a").as_deref(), Some("7"));
        assert_eq!(id_field(m, "b").as_deref(), Some("42"));
        for k in ["c", "d", "e", "f", "missing"] {
            assert!(id_field(m, k).is_none(), "{k}");
        }
    }

    #[test]
    fn year_of_parses_date_prefix() {
        assert_eq!(year_of("1994-08-22"), Some(1994));
        assert_eq!(year_of("1994"), Some(1994));
        assert_eq!(year_of("xx94-01"), None);
        assert_eq!(year_of("19"), None);
    }

    #[test]
    fn metadata_emits_schema_keys() {
        let row = Row {
            kind: "track",
            id: "1".into(),
            title: "T".into(),
            artist: Some("A".into()),
            album: Some("B".into()),
            duration_ms: Some(1000),
            release_year: None,
            artwork_xl: None,
            explicit: Some(true),
            genre: None,
            artist_id: Some("9".into()),
            album_id: None,
            isrc: None,
        };
        let m = to_metadata(&row);
        for key in [
            "source_ref",
            "title",
            "artist",
            "album",
            "duration_ms",
            "release_year",
            "artwork",
            "explicit",
            "genre",
            "storefront",
            "artist_ref",
            "album_ref",
            "isrc",
        ] {
            assert!(m.as_object().is_some_and(|o| o.contains_key(key)), "{key}");
        }
        assert_eq!(m["artist_ref"]["kind"], "artist");
        assert_eq!(m["album_ref"], Value::Null);
    }
}
