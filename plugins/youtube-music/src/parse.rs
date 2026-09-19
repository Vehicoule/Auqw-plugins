//! Player-response parsing: playability triage, format classification,
//! audio format picking, expiry extraction.

use serde_json::Value;

/// The playability bucket a rung landed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Playability {
    /// `playabilityStatus.status` is `OK` or absent.
    Ok,
    /// Bot check ("not a bot", "unusual traffic").
    BotCheck,
    /// Age-restricted content.
    AgeRestricted,
    /// Sign-in required for any other reason.
    SignInRequired,
    /// Anything else (unavailable, region, deleted, ...).
    Unavailable,
}

/// The audio-format shape of a playable rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatOutcome {
    /// At least one audio format carries a plain `url`.
    PlainAudio,
    /// Audio formats exist but carry neither `url` nor `signatureCipher`
    /// (SABR-only serving, often with `serverAbrStreamingUrl`).
    SabrOnly,
    /// Audio formats exist only behind `signatureCipher`/`cipher`.
    CipheredOnly,
    /// No audio formats at all.
    NoAudio,
}

/// A selected audio format.
pub struct Picked {
    /// Plain stream URL.
    pub url: String,
    /// MIME type (e.g. `audio/mp4`).
    pub mime: String,
    /// Reported bitrate in kbps.
    pub bitrate_kbps: Option<u64>,
    /// `expire=` query param converted to epoch milliseconds.
    pub expires_at_ms: Option<u64>,
    /// `contentLength` of the format in bytes, when reported.
    pub content_length: Option<u64>,
}

/// Classify `playabilityStatus`. `status` is checked first; when the
/// status is non-OK the lowercased `reason` + `messages` blob decides
/// the bucket.
pub fn classify_playability(body: &Value) -> (Playability, String) {
    let status = body.get("playabilityStatus").unwrap_or(&Value::Null);
    let status_str = status.get("status").and_then(Value::as_str).unwrap_or("");
    if status_str == "OK" || status_str.is_empty() {
        return (Playability::Ok, String::new());
    }
    let reason = status
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or(status_str)
        .to_string();
    let blob = playability_blob(status);
    if blob.contains("not a bot") || blob.contains("unusual traffic") {
        return (Playability::BotCheck, reason);
    }
    if has_whole_word_age(&blob) {
        return (Playability::AgeRestricted, reason);
    }
    if blob.contains("sign in") {
        return (Playability::SignInRequired, reason);
    }
    (Playability::Unavailable, reason)
}

fn playability_blob(status: &Value) -> String {
    let mut blob = status
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if let Some(messages) = status.get("messages").and_then(Value::as_array) {
        for message in messages {
            blob.push(' ');
            blob.push_str(&message.as_str().unwrap_or("").to_lowercase());
        }
    }
    blob
}

/// Whole-word `age` — word-boundary match so "managed"/"damage" do not
/// count as age restriction.
fn has_whole_word_age(blob: &str) -> bool {
    let bytes = blob.as_bytes();
    blob.match_indices("age").any(|(index, _)| {
        let before_ok = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
        let after = index + 3;
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        before_ok && after_ok
    })
}

/// Extract `responseContext.visitorData` (structural path, not
/// substring scanning).
pub fn visitor_data(body: &Value) -> Option<String> {
    body.pointer("/responseContext/visitorData")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Classify the audio-format shape of a playable rung.
pub fn format_outcome(body: &Value) -> FormatOutcome {
    let mut audio = 0usize;
    let mut plain = 0usize;
    let mut ciphered = 0usize;
    let formats = body
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for format in &formats {
        let mime = format.get("mimeType").and_then(Value::as_str).unwrap_or("");
        if !mime.starts_with("audio/") {
            continue;
        }
        audio += 1;
        if format.get("url").and_then(Value::as_str).is_some() {
            plain += 1;
        }
        if format.get("signatureCipher").is_some() || format.get("cipher").is_some() {
            ciphered += 1;
        }
    }
    if plain > 0 {
        return FormatOutcome::PlainAudio;
    }
    if audio == 0 {
        return FormatOutcome::NoAudio;
    }
    if ciphered > 0 {
        return FormatOutcome::CipheredOnly;
    }
    FormatOutcome::SabrOnly
}

/// Pick the best plain audio format: score by |bitrate − 128 kbps|,
/// prefer `audio/mp4` over `audio/webm` on equal distance.
pub fn pick_audio(body: &Value) -> Option<Picked> {
    let formats = body
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)?;
    let mut best: Option<(i64, u8, &Value)> = None;
    for format in formats {
        let mime = format.get("mimeType").and_then(Value::as_str).unwrap_or("");
        if !mime.starts_with("audio/") {
            continue;
        }
        if format.get("url").and_then(Value::as_str).is_none() {
            continue;
        }
        let bitrate = format.get("bitrate").and_then(Value::as_u64).unwrap_or(0);
        let distance = (i64::try_from(bitrate).unwrap_or(i64::MAX) - 128_000).abs();
        let codec_rank = if mime.contains("mp4") { 1 } else { 0 };
        let better = match &best {
            None => true,
            Some((d, r, _)) => distance < *d || (distance == *d && codec_rank > *r),
        };
        if better {
            best = Some((distance, codec_rank, format));
        }
    }
    let (_, _, format) = best?;
    let url = format.get("url").and_then(Value::as_str).unwrap_or("");
    let mime = format
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("");
    let bitrate_kbps = format
        .get("bitrate")
        .and_then(Value::as_u64)
        .map(|b| b / 1000);
    Some(Picked {
        url: url.to_string(),
        mime: mime.to_string(),
        bitrate_kbps,
        expires_at_ms: expire_ms(url),
        content_length: content_length(format),
    })
}

/// `contentLength` arrives as a JSON string on live responses; accept a
/// number too so fixtures can use either shape.
fn content_length(format: &Value) -> Option<u64> {
    let v = format.get("contentLength")?;
    v.as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .or_else(|| v.as_u64())
        .filter(|len| *len > 0)
}

/// Pull `expire=<unix seconds>` out of the URL and convert to ms.
fn expire_ms(url: &str) -> Option<u64> {
    for marker in ["?expire=", "&expire="] {
        if let Some((_, rest)) = url.split_once(marker) {
            let value = rest.split('&').next().unwrap_or(rest);
            let secs = value.parse::<u64>().ok()?;
            return secs.checked_mul(1000);
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
            "sabr" => include_str!("../fixtures/player-sabr-only.json"),
            "ciphered" => include_str!("../fixtures/player-ciphered-only.json"),
            "bot" => include_str!("../fixtures/player-bot-check.json"),
            "age" => include_str!("../fixtures/player-age-restricted.json"),
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
    fn playability_buckets() {
        assert_eq!(classify_playability(&fixture("ok")).0, Playability::Ok);
        assert_eq!(
            classify_playability(&fixture("bot")).0,
            Playability::BotCheck
        );
        assert_eq!(
            classify_playability(&fixture("age")).0,
            Playability::AgeRestricted
        );
        assert_eq!(
            classify_playability(&fixture("login")).0,
            Playability::SignInRequired
        );
        assert_eq!(
            classify_playability(&fixture("unplayable")).0,
            Playability::Unavailable
        );
    }

    #[test]
    fn missing_status_is_ok() {
        // Legacy shape: no playabilityStatus on a 200 counts as playable.
        assert_eq!(
            classify_playability(&serde_json::json!({})).0,
            Playability::Ok
        );
    }

    #[test]
    fn whole_word_age_only() {
        // "managed"/"damage" must not trip the age bucket.
        let body = serde_json::json!({
            "playabilityStatus": {
                "status": "ERROR",
                "reason": "Playback managed by your administrator"
            }
        });
        assert_ne!(classify_playability(&body).0, Playability::AgeRestricted);
        let body = serde_json::json!({
            "playabilityStatus": {
                "status": "LOGIN_REQUIRED",
                "reason": "This video is age-restricted"
            }
        });
        assert_eq!(classify_playability(&body).0, Playability::AgeRestricted);
    }

    #[test]
    fn unusual_traffic_is_bot() {
        let body = serde_json::json!({
            "playabilityStatus": {
                "status": "LOGIN_REQUIRED",
                "reason": "Unusual traffic from your network"
            }
        });
        assert_eq!(classify_playability(&body).0, Playability::BotCheck);
    }

    #[test]
    fn messages_join_reason_blob() {
        let body = serde_json::json!({
            "playabilityStatus": {
                "status": "ERROR",
                "reason": "Unavailable",
                "messages": ["Please sign in to continue"]
            }
        });
        assert_eq!(classify_playability(&body).0, Playability::SignInRequired);
    }

    #[test]
    fn visitor_data_path() {
        assert_eq!(
            visitor_data(&fixture("sabr")),
            Some("Cgt0ZXN0LXZpc2l0b3ItaWQtMDAxEgB6Zg%3D%3D".to_string())
        );
        assert_eq!(visitor_data(&fixture("ok")), None);
        assert_eq!(visitor_data(&serde_json::json!({})), None);
    }

    #[test]
    fn format_shapes() {
        assert_eq!(format_outcome(&fixture("ok")), FormatOutcome::PlainAudio);
        assert_eq!(format_outcome(&fixture("sabr")), FormatOutcome::SabrOnly);
        assert_eq!(
            format_outcome(&fixture("ciphered")),
            FormatOutcome::CipheredOnly
        );
        let no_audio = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "video/mp4", "bitrate": 1, "url": "https://x/v"}
            ]}
        });
        assert_eq!(format_outcome(&no_audio), FormatOutcome::NoAudio);
        assert_eq!(
            format_outcome(&serde_json::json!({})),
            FormatOutcome::NoAudio
        );
        assert_eq!(
            format_outcome(&fixture("malformed")),
            FormatOutcome::NoAudio
        );
    }

    #[test]
    fn picks_best_plain_audio() {
        let Some(p) = pick_audio(&fixture("ok")) else {
            panic!("expected a pick");
        };
        // mp4 at 130k (dist 2k) beats webm at 131k/132k and webm 72k.
        assert_eq!(p.mime, "audio/mp4");
        assert_eq!(p.bitrate_kbps, Some(130));
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
        let Some(p) = pick_audio(&body) else {
            panic!("expected a pick");
        };
        assert_eq!(p.mime, "audio/mp4");
    }

    #[test]
    fn missing_expire_is_none() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "bitrate": 128000, "url": "https://x/v?foo=1"}
            ]}
        });
        let Some(p) = pick_audio(&body) else {
            panic!("expected a pick");
        };
        assert_eq!(p.expires_at_ms, None);
    }

    #[test]
    fn absurd_expire_is_none_not_wrapped() {
        // u64::MAX seconds cannot become milliseconds — the expiry is
        // unknown rather than a wrapped timestamp in the distant past.
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "bitrate": 128000,
                 "url": format!("https://x/v?expire={}", u64::MAX)}
            ]}
        });
        let Some(p) = pick_audio(&body) else {
            panic!("expected a pick");
        };
        assert_eq!(p.expires_at_ms, None);
    }

    #[test]
    fn no_pick_without_plain_url() {
        assert!(pick_audio(&fixture("sabr")).is_none());
        assert!(pick_audio(&fixture("ciphered")).is_none());
    }
}
