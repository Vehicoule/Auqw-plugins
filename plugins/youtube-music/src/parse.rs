//! Player-response parsing: playability triage, format classification,
//! audio format picking, expiry extraction.

use serde_json::Value;

/// A visitor string safe to replay as `X-Goog-Visitor-Id` or persist
/// under a `visitor/` KV key: nonempty, at most 1024 bytes, every byte
/// visible ASCII (`0x21..=0x7e`) — no controls, space, or DEL, so a
/// poisoned persisted/upstream value can never become header syntax.
/// Input `&str` is already UTF-8 by construction.
pub fn visitor_token(s: &str) -> Option<&str> {
    if s.is_empty() || s.len() > 1024 || !s.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return None;
    }
    Some(s)
}

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
    /// The format's itag, when upstream reports it.
    pub itag: Option<u32>,
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
    if blob.contains("sign in") || status_str == "LOGIN_REQUIRED" {
        // `LOGIN_REQUIRED` is the canonical sign-in status even with no
        // reason text — but bot-check reasons under it were caught
        // above, so this arm only fires on a real sign-in wall.
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
        // Only a real cipher string counts — an explicit `null` (or any
        // other non-string value) under the key is not a ciphered format.
        if format.get("signatureCipher").is_some_and(Value::is_string)
            || format.get("cipher").is_some_and(Value::is_string)
        {
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

/// Selection inputs for [`pick_audio`].
pub struct PickOptions<'a> {
    /// Bitrate to land nearest, in kbps.
    pub target_bitrate_kbps: u32,
    /// MIME bases in preference order (`audio/mp4`, `audio/webm`);
    /// unlisted bases rank after all listed ones.
    pub prefer: &'a [&'a str],
    /// Exact itag requirement — `Some` picks only that itag, never a
    /// fallback.
    pub pin_itag: Option<u32>,
}

/// A format's itag as `u32`, from either the numeric or the
/// numeric-string shape upstream uses.
fn itag_of(format: &Value) -> Option<u32> {
    let v = format.get("itag")?;
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
}

/// Pick the best plain audio format. Audio-only, plain-HTTPS-URL
/// formats are eligible; ciphered rows never are. When `pin_itag` is
/// set only that exact itag can pick. Ranking: preferred MIME base
/// first (unlisted bases after all listed), then absolute bitrate
/// distance to the target, then stable upstream order.
pub fn pick_audio(body: &Value, options: PickOptions<'_>) -> Option<Picked> {
    let formats = body
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)?;
    let target = i64::from(options.target_bitrate_kbps) * 1000;
    let mut best: Option<(usize, i64, &Value)> = None;
    for format in formats {
        let mime = format.get("mimeType").and_then(Value::as_str).unwrap_or("");
        if !mime.starts_with("audio/") {
            continue;
        }
        if format
            .get("url")
            .and_then(Value::as_str)
            .is_none_or(|u| !u.starts_with("https://"))
        {
            // The host only serves https:// destinations — a non-https
            // format is unusable, not a reason to kill the resolve.
            continue;
        }
        if let Some(pin) = options.pin_itag {
            if itag_of(format) != Some(pin) {
                continue;
            }
        }
        let base = mime.split(';').next().unwrap_or("");
        let pref = options
            .prefer
            .iter()
            .position(|p| *p == base)
            .unwrap_or(options.prefer.len());
        let bitrate = format.get("bitrate").and_then(Value::as_u64).unwrap_or(0);
        let distance = (i64::try_from(bitrate).unwrap_or(i64::MAX) - target).abs();
        // Strictly-better replacement keeps upstream order stable on
        // ties (first seen wins).
        let better = match &best {
            None => true,
            Some((p, d, _)) => (pref, distance) < (*p, *d),
        };
        if better {
            best = Some((pref, distance, format));
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
    // `bitrate_kbps` is capped at u32::MAX by the capabilities schema
    // — a pathological upstream bitrate saturates rather than
    // invalidating an otherwise-good resolve.
    let bitrate_kbps = format
        .get("bitrate")
        .and_then(Value::as_u64)
        .map(|b| (b / 1000).min(u64::from(u32::MAX)));
    Some(Picked {
        url: url.to_string(),
        mime: mime.to_string(),
        bitrate_kbps,
        expires_at_ms: expire_ms(url),
        content_length: content_length(format),
        itag: itag_of(format),
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
    fn bare_login_status_is_sign_in() {
        // `LOGIN_REQUIRED` with no reason or messages is still a
        // sign-in wall, not generic unavailability.
        let body = serde_json::json!({
            "playabilityStatus": { "status": "LOGIN_REQUIRED" }
        });
        assert_eq!(classify_playability(&body).0, Playability::SignInRequired);
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
    fn null_cipher_keys_are_not_ciphered() {
        // Key presence alone is not a cipher: an explicit `null` under
        // `signatureCipher`/`cipher` leaves the format unciphered —
        // with no url and audio present, that's SABR, not ciphered.
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "signatureCipher": null, "cipher": null}
            ]}
        });
        assert_eq!(format_outcome(&body), FormatOutcome::SabrOnly);
        // Non-string cipher values don't count either.
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "signatureCipher": {"s": "x"}}
            ]}
        });
        assert_eq!(format_outcome(&body), FormatOutcome::SabrOnly);
        // A real cipher string does.
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "signatureCipher": "s=abc&sp=sig"}
            ]}
        });
        assert_eq!(format_outcome(&body), FormatOutcome::CipheredOnly);
    }

    fn default_opts<'a>() -> PickOptions<'a> {
        PickOptions {
            target_bitrate_kbps: 128,
            prefer: &["audio/mp4", "audio/webm"],
            pin_itag: None,
        }
    }

    #[test]
    fn picks_best_plain_audio() {
        let Some(p) = pick_audio(&fixture("ok"), default_opts()) else {
            panic!("expected a pick");
        };
        // mp4 at 130k (dist 2k) beats webm at 131k/132k and webm 72k.
        assert_eq!(p.mime, "audio/mp4");
        assert_eq!(p.bitrate_kbps, Some(130));
        assert!(p.url.contains("itag=140"));
        assert_eq!(p.itag, Some(140));
        assert_eq!(p.expires_at_ms, Some(1_893_456_000_000));
    }

    #[test]
    fn prefer_webm_outranks_closer_mp4() {
        // `prefer` order outranks bitrate distance: a webm-first
        // caller gets the webm format even though the mp4 sits closer
        // to the target bitrate.
        let Some(p) = pick_audio(
            &fixture("ok"),
            PickOptions {
                prefer: &["audio/webm", "audio/mp4"],
                ..default_opts()
            },
        ) else {
            panic!("expected a pick");
        };
        assert_eq!(p.mime, "audio/webm");
        assert_eq!(p.itag, Some(251));
    }

    #[test]
    fn mime_rank_breaks_distance_ties() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 128000, "url": "https://x/w"},
                {"mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000, "url": "https://x/m"}
            ]}
        });
        let Some(p) = pick_audio(&body, default_opts()) else {
            panic!("expected a pick");
        };
        assert_eq!(p.mime, "audio/mp4");
    }

    #[test]
    fn target_bitrate_selects_within_container() {
        // Within the preferred container the closest bitrate wins.
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"itag": 139, "mimeType": "audio/mp4", "bitrate": 50000, "url": "https://x/l"},
                {"itag": 140, "mimeType": "audio/mp4", "bitrate": 130000, "url": "https://x/h"}
            ]}
        });
        let Some(p) = pick_audio(
            &body,
            PickOptions {
                target_bitrate_kbps: 64,
                ..default_opts()
            },
        ) else {
            panic!("expected a pick");
        };
        assert_eq!(p.itag, Some(139));
    }

    #[test]
    fn pin_itag_picks_exactly() {
        let Some(p) = pick_audio(
            &fixture("ok"),
            PickOptions {
                pin_itag: Some(251),
                ..default_opts()
            },
        ) else {
            panic!("expected a pick");
        };
        assert_eq!(p.itag, Some(251));
        assert_eq!(p.mime, "audio/webm");
    }

    #[test]
    fn pin_itag_accepts_string_shaped_itag() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"itag": "140", "mimeType": "audio/mp4", "bitrate": 130000, "url": "https://x/m"}
            ]}
        });
        let Some(p) = pick_audio(
            &body,
            PickOptions {
                pin_itag: Some(140),
                ..default_opts()
            },
        ) else {
            panic!("expected a pick");
        };
        assert_eq!(p.itag, Some(140));
    }

    #[test]
    fn pin_itag_missing_picks_nothing() {
        assert!(pick_audio(
            &fixture("ok"),
            PickOptions {
                pin_itag: Some(774),
                ..default_opts()
            },
        )
        .is_none());
    }

    #[test]
    fn malformed_itag_never_matches_pin() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"itag": "abc", "mimeType": "audio/mp4", "bitrate": 130000, "url": "https://x/m"}
            ]}
        });
        assert!(pick_audio(
            &body,
            PickOptions {
                pin_itag: Some(140),
                ..default_opts()
            },
        )
        .is_none());
    }

    #[test]
    fn absent_itag_surfaces_null_unpinned() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "bitrate": 130000, "url": "https://x/m"}
            ]}
        });
        let Some(p) = pick_audio(&body, default_opts()) else {
            panic!("expected a pick");
        };
        assert_eq!(p.itag, None);
    }

    #[test]
    fn bitrate_saturates_at_schema_cap() {
        // `bitrate_kbps` is u32-capped in the capabilities schema: a
        // pathological upstream `bitrate` saturates the emitted value
        // instead of dropping the format or invalidating the resolve.
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "bitrate": u64::MAX, "url": "https://x/v"}
            ]}
        });
        let Some(p) = pick_audio(&body, default_opts()) else {
            panic!("expected a pick");
        };
        assert_eq!(p.bitrate_kbps, Some(u64::from(u32::MAX)));
    }

    #[test]
    fn missing_expire_is_none() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "bitrate": 128000, "url": "https://x/v?foo=1"}
            ]}
        });
        let Some(p) = pick_audio(&body, default_opts()) else {
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
        let Some(p) = pick_audio(&body, default_opts()) else {
            panic!("expected a pick");
        };
        assert_eq!(p.expires_at_ms, None);
    }

    #[test]
    fn no_pick_without_plain_url() {
        assert!(pick_audio(&fixture("sabr"), default_opts()).is_none());
        assert!(pick_audio(&fixture("ciphered"), default_opts()).is_none());
    }

    #[test]
    fn non_https_url_is_unpickable() {
        let body = serde_json::json!({
            "streamingData": { "adaptiveFormats": [
                {"mimeType": "audio/mp4", "bitrate": 128000, "url": "http://x/v"}
            ]}
        });
        assert!(pick_audio(&body, default_opts()).is_none());
    }
}
