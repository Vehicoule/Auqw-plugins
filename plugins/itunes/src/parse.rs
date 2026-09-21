//! Strict mapping of iTunes `search`/`lookup` bodies to track rows,
//! track rows to `trackMetadata` wire values, and first-seen-wins
//! dedup of compilation floods.

use auqw_guest_sdk::GuestError;
use serde_json::{Map, Value};

/// One accepted upstream song row.
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub release_year: Option<u64>,
    /// Verified-https `artworkUrl100` before size substitution.
    pub artwork100: Option<String>,
    pub explicit: Option<bool>,
    pub genre: Option<String>,
}

fn bad(m: &str) -> GuestError {
    GuestError::Failed {
        kind: "invalid-response".into(),
        message: format!("itunes: {m}"),
    }
}

/// Contract cap on emitted text fields (`title`/`artist`/`album`/
/// `genre`) — an overlong upstream string truncates instead of
/// failing a whole page downstream.
const TEXT_MAX_CHARS: usize = 512;

/// `artworkRef.url` caps at 2048 characters — an overlong upstream
/// URL is dropped, not truncated to a broken link.
const URL_MAX_CHARS: usize = 2048;

/// Largest value a contract integer field may carry downstream —
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_MS: u64 = 9_007_199_254_740_991;

/// Parse the `results` array of an iTunes body. Non-song rows and
/// rows with a zero/missing id or empty name drop out; a non-JSON,
/// non-object, or `results`-less body is `invalid-response`.
///
/// # Errors
/// [`GuestError::Failed`] kind `invalid-response` on malformed input.
pub fn parse_tracks(body: &[u8]) -> Result<Vec<Track>, GuestError> {
    let v: Value = serde_json::from_slice(body).map_err(|_| bad("body is not JSON"))?;
    let results = v
        .as_object()
        .and_then(|o| o.get("results"))
        .and_then(Value::as_array)
        .ok_or_else(|| bad("results missing or not an array"))?;
    Ok(results.iter().filter_map(track_of).collect())
}

fn track_of(row: &Value) -> Option<Track> {
    let o = row.as_object()?;
    if let Some(w) = o.get("wrapperType").and_then(Value::as_str) {
        if w != "track" {
            return None;
        }
    }
    if o.get("kind").and_then(Value::as_str)? != "song" {
        return None;
    }
    let id = track_id(o)?;
    let title = o
        .get("trackName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(TEXT_MAX_CHARS).collect())?;
    let explicit = match o.get("trackExplicitness").and_then(Value::as_str) {
        Some("explicit") => Some(true),
        Some("cleaned") | Some("notExplicit") => Some(false),
        _ => None,
    };
    let release_year = o
        .get("releaseDate")
        .and_then(Value::as_str)
        .and_then(|s| s.get(..4))
        .and_then(|y| y.parse::<u64>().ok());
    let artwork100 = o
        .get("artworkUrl100")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("https://") && u.chars().count() <= URL_MAX_CHARS)
        .map(str::to_string);
    Some(Track {
        id,
        title,
        artist: str_field(o, "artistName"),
        album: str_field(o, "collectionName"),
        duration_ms: o
            .get("trackTimeMillis")
            .and_then(Value::as_u64)
            .map(|ms| ms.min(MAX_SAFE_MS)),
        release_year,
        artwork100,
        explicit,
        genre: str_field(o, "primaryGenreName"),
    })
}

fn str_field(o: &Map<String, Value>, key: &str) -> Option<String> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(TEXT_MAX_CHARS).collect())
}

fn track_id(o: &Map<String, Value>) -> Option<String> {
    let v = o.get("trackId")?;
    let id = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))?;
    (id > 0).then(|| id.to_string())
}

/// An `artworkRef` at `size`px, or `None` when the row has no https
/// artwork. The final `100x100bb` size segment is rewritten; an
/// unrecognized URL shape keeps the URL with honest null dimensions.
pub fn artwork_ref(track: &Track, size: u64) -> Option<Value> {
    let url = track.artwork100.as_ref()?;
    let (url, dim) = match url.rfind("100x100bb") {
        Some(i) => (
            format!(
                "{}{size}x{size}bb{}",
                &url[..i],
                &url[i + "100x100bb".len()..]
            ),
            Value::from(size),
        ),
        None => (url.clone(), Value::Null),
    };
    Some(serde_json::json!({
        "url": url,
        "width": dim,
        "height": dim,
    }))
}

/// The `trackMetadata` wire value for one track.
pub fn to_metadata(track: &Track, storefront: Option<&str>, artwork_size: u64) -> Value {
    let artwork: Vec<Value> = artwork_ref(track, artwork_size).into_iter().collect();
    serde_json::json!({
        "source_ref": {
            "provider": "itunes",
            "kind": "track",
            "id": track.id,
        },
        "title": track.title,
        "artist": track.artist,
        "album": track.album,
        "duration_ms": track.duration_ms,
        "release_year": track.release_year,
        "artwork": artwork,
        "explicit": track.explicit,
        "genre": track.genre,
        "storefront": storefront,
    })
}

/// First-seen-wins dedup. A later row is a duplicate iff it repeats an
/// emitted row's upstream track id, or its normalized title, normalized
/// artist, and explicitness match and both durations exist within
/// 2,000 ms of each other. Rows without a duration never collapse
/// unless the id matches; bracket/parenthetical/version labels stay in
/// the title, so "(Live)"/"(Remix)"/"(Remastered)" variants survive.
pub fn dedup(tracks: Vec<Track>) -> Vec<Track> {
    let mut ids = std::collections::HashSet::new();
    let mut keys: Vec<(String, String, Option<bool>, u64)> = Vec::new();
    let mut out = Vec::with_capacity(tracks.len());
    for t in tracks {
        if !ids.insert(t.id.clone()) {
            continue;
        }
        let title = normalize(&t.title);
        let artist = t.artist.as_deref().map(normalize).unwrap_or_default();
        let dupe = t.duration_ms.is_some_and(|d| {
            keys.iter().any(|(kt, ka, ke, kd)| {
                *kt == title && *ka == artist && *ke == t.explicit && kd.abs_diff(d) <= 2_000
            })
        });
        if dupe {
            continue;
        }
        if let Some(d) = t.duration_ms {
            keys.push((title, artist, t.explicit, d));
        }
        out.push(t);
    }
    out
}

fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, title: &str, duration_ms: Option<u64>, explicit: Option<bool>) -> Track {
        Track {
            id: id.into(),
            title: title.into(),
            artist: Some("Artist".into()),
            album: None,
            duration_ms,
            release_year: None,
            artwork100: None,
            explicit,
            genre: None,
        }
    }

    #[test]
    fn dedup_collapses_true_dupes_keeps_variants() {
        let out = dedup(vec![
            track("1", "Song", Some(240_000), Some(false)),
            // Compilation reissue: same key, +1,500 ms → duplicate.
            track("2", "  SONG ", Some(241_500), Some(false)),
            track("3", "Song (Live)", Some(250_000), Some(false)),
            // Explicit differs → a distinct row, not a duplicate.
            track("4", "Song", Some(270_000), Some(true)),
            // No duration on either side → never collapses.
            track("5", "Song", None, Some(false)),
            track("6", "Song", None, Some(false)),
            // Same upstream id collapses regardless of other fields.
            track("1", "Other Title", None, None),
        ]);
        let ids: Vec<&str> = out.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["1", "3", "4", "5", "6"]);
    }
}
