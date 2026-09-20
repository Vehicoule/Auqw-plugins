//! Strict mapping of LRCLIB bodies to records, search-hit selection,
//! LRC line parsing, and the title/query cleanups the later waterfall
//! tiers apply.

use auqw_guest_sdk::GuestError;
use serde_json::{json, Map, Value};

use crate::encode::ascii_fold;

/// One accepted upstream lyrics record. `plain`/`synced` are the raw
/// `plainLyrics`/`syncedLyrics` strings, `None` when absent or empty.
pub struct Record {
    /// `trackName`, guaranteed nonempty.
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// `duration` seconds carried as milliseconds.
    pub duration_ms: Option<u64>,
    pub instrumental: bool,
    pub plain: Option<String>,
    pub synced: Option<String>,
}

impl Record {
    /// The `lyricsMatched` wire value — the upstream metadata the
    /// application scores for acceptance.
    pub fn matched(&self) -> Value {
        json!({
            "title": self.title,
            "artist": self.artist,
            "album": self.album,
            "duration_ms": self.duration_ms,
        })
    }
}

fn bad(m: &str) -> GuestError {
    GuestError::Failed {
        kind: "invalid-response".into(),
        message: format!("lrclib: {m}"),
    }
}

/// Parse an `/api/get` body: a single record object. Non-JSON, a
/// non-object, or an unusable record is `invalid-response`.
///
/// # Errors
/// [`GuestError::Failed`] kind `invalid-response` on malformed input.
pub fn parse_get(body: &[u8]) -> Result<Record, GuestError> {
    let v: Value = serde_json::from_slice(body).map_err(|_| bad("body is not JSON"))?;
    record_of(&v).ok_or_else(|| bad("record is missing or unusable"))
}

/// Parse an `/api/search` body: an array of records. Entries that are
/// not usable records drop out; a non-JSON or non-array body is
/// `invalid-response`.
///
/// # Errors
/// [`GuestError::Failed`] kind `invalid-response` on malformed input.
pub fn parse_search(body: &[u8]) -> Result<Vec<Record>, GuestError> {
    let v: Value = serde_json::from_slice(body).map_err(|_| bad("body is not JSON"))?;
    let rows = v
        .as_array()
        .ok_or_else(|| bad("search result is not an array"))?;
    Ok(rows.iter().filter_map(record_of).collect())
}

/// A record is usable only with a nonempty `trackName` — `matched`
/// cannot carry an empty title. An `instrumental` field present but
/// not a boolean makes the record unusable: guessing the flag wrong
/// could send lyrics for an instrumental track, a dishonest answer.
/// Other fields are lenient: present-but-wrong-typed degrades to
/// absent rather than failing the row.
fn record_of(v: &Value) -> Option<Record> {
    let o = v.as_object()?;
    let title = o
        .get("trackName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let instrumental = match o.get("instrumental") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return None,
    };
    Some(Record {
        title,
        artist: str_field(o, "artistName"),
        album: str_field(o, "albumName"),
        duration_ms: o
            .get("duration")
            .and_then(Value::as_f64)
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| (d * 1000.0).round() as u64),
        instrumental,
        plain: str_field(o, "plainLyrics"),
        synced: str_field(o, "syncedLyrics"),
    })
}

fn str_field(o: &Map<String, Value>, key: &str) -> Option<String> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Pick the record a search tier answers with: prefer an exact
/// normalized title+artist match, then exact title, then the cleaned
/// query title, else the first usable row. Provider order is never
/// re-ranked beyond this evidence preference.
pub fn pick<'a>(records: &'a [Record], title: &str, artist: Option<&str>) -> Option<&'a Record> {
    let want_title = norm(title);
    let want_cleaned = norm(&clean_title(title));
    let want_artist = artist.map(norm);
    let artist_ok = |r: &&'a Record| match (&want_artist, &r.artist) {
        (None, _) => true,
        (Some(want), Some(got)) => *want == norm(got),
        (Some(_), None) => false,
    };
    records
        .iter()
        .find(|r| norm(&r.title) == want_title && artist_ok(r))
        .or_else(|| records.iter().find(|r| norm(&r.title) == want_title))
        .or_else(|| {
            if want_cleaned != want_title {
                records.iter().find(|r| norm(&r.title) == want_cleaned)
            } else {
                None
            }
        })
        .or_else(|| records.first())
}

/// Comparison key: ASCII-folded lowercase alphanumerics only — case,
/// punctuation, and spacing never decide a match.
fn norm(s: &str) -> String {
    ascii_fold(s)
        .unwrap_or_default()
        .to_lowercase()
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric())
        .map(char::from)
        .collect()
}

/// Parse LRC text into `(t_ms, text)` pairs: `[mm:ss.xx]` and
/// `[mm:ss.xxx]` timestamps (fraction digits scale to milliseconds,
/// `[mm:ss]` counts as whole seconds). Metadata tags (`[ti:]`, `[ar:]`,
/// …) drop; the first `[offset:±ms]` tag shifts every timestamp,
/// clamped at zero. A line with several time tags emits one pair per
/// tag; lines with no time tag contribute nothing. Output is stable-
/// sorted by `t_ms` — the wire shape needs monotonic timestamps —
/// and each text is capped at the schema's 1024 characters.
pub fn parse_lrc(input: &str) -> Vec<(u64, String)> {
    let mut offset_ms: i64 = 0;
    let mut out: Vec<(u64, String)> = Vec::new();
    for line in input.lines() {
        let mut rest = line;
        let mut stamps: Vec<u64> = Vec::new();
        while let Some(tail) = rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
            let (tag, after) = tail;
            let tag = tag.trim();
            if tag.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
                if let Some(ms) = parse_timestamp(tag) {
                    stamps.push(ms);
                }
            } else if let Some(v) = tag
                .strip_prefix("offset:")
                .or_else(|| tag.strip_prefix("Offset:"))
                .or_else(|| tag.strip_prefix("OFFSET:"))
            {
                if let Ok(n) = v.trim().parse::<i64>() {
                    offset_ms = n;
                }
            }
            rest = after;
        }
        if stamps.is_empty() {
            continue;
        }
        let text: String = rest.trim().chars().take(1024).collect();
        for stamp in stamps {
            let t = stamp as i64 + offset_ms;
            out.push((t.max(0) as u64, text.clone()));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

/// `mm:ss[.frac]` → milliseconds. Fraction digits scale: `.5` is
/// 500 ms, `.50` is 500 ms, `.500` is 500 ms; longer fractions
/// truncate at milliseconds. Returns `None` on any non-numeric part.
fn parse_timestamp(tag: &str) -> Option<u64> {
    let (m, rest) = tag.split_once(':')?;
    let minutes: u64 = m.trim().parse().ok()?;
    let (secs, frac) = match rest.split_once('.') {
        Some((s, f)) => (s, f),
        None => (rest, ""),
    };
    let seconds: u64 = secs.trim().parse().ok()?;
    let frac = frac.trim();
    let ms = if frac.is_empty() {
        0
    } else {
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut d: Vec<u8> = frac.bytes().take(3).collect();
        while d.len() < 3 {
            d.push(b'0');
        }
        std::str::from_utf8(&d).ok()?.parse::<u64>().ok()?
    };
    Some(minutes * 60_000 + seconds * 1_000 + ms)
}

/// Version-suffix marker words for [`clean_title`]: exact words for
/// the short forms, prefix stems (≥4 chars) for the compounds —
/// "remastered"/"remastering" share `remaster`, "deluxe" lives under
/// `delux`.
const MARKER_STEMS: &[&str] = &[
    "remaster",
    "remix",
    "delux",
    "editi",
    "expand",
    "annivers",
    "featur",
    "versio",
    "reissue",
    "rerecord",
    "instrumen",
    "karaoke",
    "acoust",
    "unplug",
    "orchestr",
    "symphon",
];
const MARKER_EXACT: &[&str] = &[
    "live", "edit", "mono", "stereo", "bonus", "demo", "mix", "dub", "ep", "lp", "feat", "ft",
    "clean", "explicit", "single", "radio", "cover", "session", "sessions", "mtv",
];

/// Byte index of the first ASCII-case-insensitive `pat` in `hay` —
/// byte-preserving, so the offset slices `hay` safely.
fn find_ascii_ci(hay: &str, pat: &str) -> Option<usize> {
    hay.as_bytes()
        .windows(pat.len())
        .position(|w| w.eq_ignore_ascii_case(pat.as_bytes()))
}

fn has_marker(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| {
            let w = w.to_lowercase();
            MARKER_EXACT.contains(&w.as_str()) || MARKER_STEMS.iter().any(|s| w.starts_with(s))
        })
}

/// The tier-3 cleanup: strip trailing parenthesized/bracketed groups
/// whose words carry a version marker ("(Remastered 2011)",
/// "[Deluxe Edition]", "(feat. X)"), a bare `feat.`/`ft.`/`featuring`
/// tail, and a `- <marker text>` tail segment. Plain parenthetical
/// title text survives — "(Everything I Do) I Do It for You" keeps
/// its leading group. An empty result falls back to the original.
pub fn clean_title(title: &str) -> String {
    let mut cur = title.trim().to_string();
    loop {
        let before = cur.clone();
        // A trailing `(...)`/`[...]` group with a marker word inside.
        if cur.ends_with(')') || cur.ends_with(']') {
            let (open, close) = if cur.ends_with(')') {
                ('(', ')')
            } else {
                ('[', ']')
            };
            if let Some(start) = cur.rfind(open) {
                let inner = &cur[start + 1..cur.len() - close.len_utf8()];
                if has_marker(inner) {
                    cur = cur[..start].trim_end().to_string();
                }
            }
        }
        // A bare "feat. X"/"ft. X"/"featuring X" tail.
        for pat in [" feat. ", " feat ", " ft. ", " ft ", " featuring "] {
            if let Some(i) = find_ascii_ci(&cur, pat) {
                cur = cur[..i].trim_end().to_string();
                break;
            }
        }
        // A "- <marker text>" tail segment.
        if let Some(i) = cur.rfind(" - ") {
            if has_marker(&cur[i + 3..]) {
                cur = cur[..i].trim_end().to_string();
            }
        }
        if cur == before {
            break;
        }
    }
    let collapsed = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        title.to_string()
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc_timestamps_and_fraction_digits() {
        let lines = parse_lrc("[00:17.12] a\n[01:02.345] b\n[02:03] c\n[00:00.5] d");
        assert_eq!(
            lines,
            [
                (500, "d".to_string()),
                (17_120, "a".to_string()),
                (62_345, "b".to_string()),
                (123_000, "c".to_string())
            ]
        );
    }

    #[test]
    fn lrc_metadata_tags_drop_and_offset_applies() {
        let lrc =
            "[ti:Song]\n[ar:Artist]\n[offset:+250]\n[00:10.00] hi\n[offset:-50]\n[00:20.00] yo";
        let lines = parse_lrc(lrc);
        // Each `[offset:]` tag shifts the lines that follow it.
        assert_eq!(lines[0], (10_250, "hi".to_string()));
        assert_eq!(lines[1], (19_950, "yo".to_string()));
    }

    #[test]
    fn lrc_multi_stamp_line_emits_each() {
        let lines = parse_lrc("[00:10.00][00:20.00] again");
        assert_eq!(
            lines,
            [(10_000, "again".to_string()), (20_000, "again".to_string())]
        );
    }

    #[test]
    fn lrc_untimed_and_tagonly_lines_contribute_nothing() {
        let lines = parse_lrc("plain text line\n[03:25.72] \n[la:eng]");
        assert_eq!(lines, [(205_720, String::new())]);
    }

    #[test]
    fn lrc_out_of_order_input_sorts() {
        let lines = parse_lrc("[00:30.00] b\n[00:10.00] a");
        assert_eq!(lines[0].1, "a");
        assert_eq!(lines[1].1, "b");
    }

    #[test]
    fn lrc_negative_offset_clamps_at_zero() {
        let lines = parse_lrc("[offset:-60000]\n[00:10.00] x");
        assert_eq!(lines, [(0, "x".to_string())]);
    }

    #[test]
    fn lrc_text_capped_at_1024_chars() {
        let long = "x".repeat(2_000);
        let lines = parse_lrc(&format!("[00:01.00] {long}"));
        assert_eq!(lines[0].1.chars().count(), 1024);
    }

    #[test]
    fn clean_strips_version_suffixes() {
        for (given, want) in [
            ("Roads (Remastered 2011)", "Roads"),
            ("Roads [Deluxe Edition]", "Roads"),
            ("Roads (feat. Someone)", "Roads"),
            ("Roads feat. Someone", "Roads"),
            ("Roads ft. Someone", "Roads"),
            ("Roads - Live at Wembley", "Roads"),
            ("Roads (Live) [Remastered]", "Roads"),
            ("Roads (2009 Remaster)", "Roads"),
        ] {
            assert_eq!(clean_title(given), want, "{given}");
        }
    }

    #[test]
    fn clean_keeps_real_title_text() {
        for given in [
            "Roads",
            "(Everything I Do) I Do It for You",
            "Song (About You)",
            "Live at the Club",
            "The Mix-Up",
        ] {
            assert_eq!(clean_title(given), given, "{given}");
        }
    }

    #[test]
    fn clean_empty_falls_back_to_original() {
        assert_eq!(clean_title("(Remastered)"), "(Remastered)");
    }

    #[test]
    fn norm_folds_case_punct_and_marks() {
        assert_eq!(norm("  Roads (Remastered) "), norm("roads remastered"));
        assert_eq!(norm("Sigur Rós"), norm("sigur ros"));
        assert_eq!(norm("L'été"), norm("lete"));
    }
}
