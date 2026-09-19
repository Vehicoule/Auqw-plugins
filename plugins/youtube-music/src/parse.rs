//! Pure response handling: playability triage, audio format selection,
//! and URL expiry parsing. No ABI or I/O here — everything is unit
//! testable on the native target.

use serde_json::Value;

/// Target bitrate for selection scoring (128 kbps AAC-ish).
const TARGET_BITRATE: u64 = 128_000;

/// Playability triage for one rung's player response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Triage {
    /// `playabilityStatus.status == "OK"` — parse formats.
    Playable,
    /// `LOGIN_REQUIRED` — sign-in or bot-check wall.
    AuthRequired,
    /// `UNPLAYABLE`/`ERROR` — the video cannot play.
    NoResult,
    /// Anything else: unknown statuses, non-2xx, missing fields.
    Transient,
    /// HTTP 429.
    RateLimit,
}

/// Triage a player response by status code and `playabilityStatus`.
pub fn triage(status: u16, body: &Value) -> Triage {
    if status == 429 {
        return Triage::RateLimit;
    }
    if !(200..300).contains(&(status as i32)) {
        return Triage::Transient;
    }
    match body
        .get("playabilityStatus")
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str)
    {
        Some("OK") => Triage::Playable,
        Some("LOGIN_REQUIRED") => Triage::AuthRequired,
        Some("UNPLAYABLE") | Some("ERROR") => Triage::NoResult,
        // Unknown status or malformed body: do not trust it, do not fail
        // the whole resolve — let the next rung try.
        _ => Triage::Transient,
    }
}

/// A selected audio format.
#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    /// Direct media URL (plain `url` field; never deciphered).
    pub url: String,
    /// MIME type without codec parameters, e.g. `audio/mp4`.
    pub mime: String,
    /// `averageBitrate`/`bitrate` in kbps.
    pub bitrate_kbps: Option<u32>,
    /// `expire=` query parameter as epoch milliseconds.
    pub expires_at_ms: Option<u64>,
}

/// Outcome of scanning `streamingData.adaptiveFormats`.
#[derive(Debug, Clone, PartialEq)]
pub enum PickOutcome {
    /// A plain-URL audio format was selected.
    Picked(Picked),
    /// Audio formats exist but every one is ciphered.
    CipheredOnly,
    /// No audio formats at all.
    NoAudio,
}

/// Scan `streamingData.adaptiveFormats` for playable audio.
///
/// Only entries with a plain `url` are candidates; entries carrying
/// `signatureCipher`/`cipher` are counted but never used — deciphering is
/// out of scope. Selection scores by `|bitrate − 128 kbps|` and prefers
/// `audio/mp4` (AAC) over `audio/webm` (opus) on ties.
pub fn pick_format(body: &Value) -> PickOutcome {
    let Some(formats) = body
        .get("streamingData")
        .and_then(|s| s.get("adaptiveFormats"))
        .and_then(Value::as_array)
    else {
        return PickOutcome::NoAudio;
    };
    let mut ciphered = 0usize;
    let mut best: Option<(u64, u8, Picked)> = None;
    for f in formats {
        let Some(mime_full) = f.get("mimeType").and_then(Value::as_str) else {
            continue;
        };
        if !mime_full.starts_with("audio/") {
            continue;
        }
        if f.get("signatureCipher").is_some() || f.get("cipher").is_some() {
            ciphered += 1;
            continue;
        }
        let Some(url) = f.get("url").and_then(Value::as_str) else {
            continue;
        };
        let bitrate = f
            .get("averageBitrate")
            .or_else(|| f.get("bitrate"))
            .and_then(Value::as_u64);
        let dist = bitrate.map_or(u64::MAX, |b| b.abs_diff(TARGET_BITRATE));
        let rank = if mime_full.starts_with("audio/mp4") {
            0u8
        } else if mime_full.starts_with("audio/webm") {
            1
        } else {
            2
        };
        let picked = Picked {
            url: url.to_string(),
            mime: mime_full.split(';').next().unwrap_or(mime_full).to_string(),
            bitrate_kbps: bitrate.map(|b| (b / 1000) as u32),
            expires_at_ms: expire_ms(url),
        };
        let replace = match &best {
            None => true,
            Some((d, r, _)) => (dist, rank) < (*d, *r),
        };
        if replace {
            best = Some((dist, rank, picked));
        }
    }
    match best {
        Some((_, _, picked)) => PickOutcome::Picked(picked),
        None if ciphered > 0 => PickOutcome::CipheredOnly,
        None => PickOutcome::NoAudio,
    }
}

/// `expire=` query parameter (epoch seconds) as milliseconds.
fn expire_ms(url: &str) -> Option<u64> {
    let query = url.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("expire=") {
            return v.parse::<u64>().ok().map(|s| s * 1000);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let text = match name {
            "ok" => include_str!("../fixtures/player-ok-plain-urls.json"),
            "ciphered" => include_str!("../fixtures/player-ciphered-only.json"),
            "login" => include_str!("../fixtures/player-login-required.json"),
            "unplayable" => include_str!("../fixtures/player-unplayable.json"),
            "malformed" => include_str!("../fixtures/player-malformed.json"),
            _ => panic!("no fixture {name}"),
        };
        match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => panic!("fixture {name}: {e}"),
        }
    }

    #[test]
    fn triage_status_mapping() {
        assert_eq!(triage(200, &fixture("ok")), Triage::Playable);
        assert_eq!(triage(200, &fixture("login")), Triage::AuthRequired);
        assert_eq!(triage(200, &fixture("unplayable")), Triage::NoResult);
        assert_eq!(triage(200, &fixture("malformed")), Triage::Transient);
        assert_eq!(triage(429, &fixture("ok")), Triage::RateLimit);
        assert_eq!(triage(500, &fixture("ok")), Triage::Transient);
        assert_eq!(triage(200, &serde_json::json!({})), Triage::Transient);
    }

    #[test]
    fn picks_best_plain_audio() {
        let PickOutcome::Picked(p) = pick_format(&fixture("ok")) else {
            panic!("expected a pick");
        };
        // mp4 at 129.5k (dist 1.5k) beats webm at 131k (dist 3k)
        assert_eq!(p.mime, "audio/mp4");
        assert_eq!(p.bitrate_kbps, Some(129));
        assert!(p.url.contains("itag=140"));
        assert_eq!(p.expires_at_ms, Some(1_893_456_000_000));
    }

    #[test]
    fn mime_rank_breaks_distance_ties() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 128000, "url": "https://x/w"},
                {"mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000, "url": "https://x/m"}
            ]}
        });
        let PickOutcome::Picked(p) = pick_format(&body) else {
            panic!("expected a pick");
        };
        assert_eq!(p.mime, "audio/mp4");
    }

    #[test]
    fn ciphered_only_is_distinct() {
        assert_eq!(pick_format(&fixture("ciphered")), PickOutcome::CipheredOnly);
    }

    #[test]
    fn no_audio_formats() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "video/mp4", "bitrate": 1, "url": "https://x/v"}
            ]}
        });
        assert_eq!(pick_format(&body), PickOutcome::NoAudio);
        assert_eq!(pick_format(&serde_json::json!({})), PickOutcome::NoAudio);
    }

    #[test]
    fn missing_expire_is_none() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "bitrate": 128000, "url": "https://x/v?foo=1"}
            ]}
        });
        let PickOutcome::Picked(p) = pick_format(&body) else {
            panic!("expected a pick");
        };
        assert_eq!(p.expires_at_ms, None);
    }
}
