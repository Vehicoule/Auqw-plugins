//! The Slice 0 client ladder: `VISIONOS`, `IOS`, three `ANDROID_VR`
//! pins. Client versions are load-bearing: newer IOS builds are served
//! SABR-only. Pin, don't track upstream.

use auqw_guest_sdk::HttpRequest;
use serde_json::{json, Value};

/// One ladder rung: an InnerTube client identity.
pub struct Rung {
    /// Label reported as `client` in the resolve result.
    pub name: &'static str,
    /// `X-YouTube-Client-Name` header value.
    pub client_name_id: &'static str,
    /// `X-YouTube-Client-Version` header value.
    pub client_version: &'static str,
    /// `User-Agent` header value (also embedded in `context.client`).
    pub user_agent: &'static str,
    /// Extra `context.client` fields (device, OS, locale).
    pub context: fn() -> Value,
}

impl Rung {
    /// The InnerTube `clientName` for `context.client`.
    fn innertube_name(&self) -> &str {
        self.name.split('@').next().unwrap_or(self.name)
    }

    /// The rung's KV namespace key (`visitor/<key>`,
    /// `backoff/<video>/<key>`): stable across guest releases.
    pub fn kv_key(&self) -> &'static str {
        self.name
    }
}

/// The ladder, in fallback order. `VISIONOS` runs first: it resolves
/// nearly everywhere and almost never bot-checks from residential IPs.
/// `ANDROID_VR` rungs are the fallback — their URLs serve full streams
/// but the rung itself is the most bot-checked from residential IPs, so
/// it runs only after the Apple clients fail. `IOS` sits between:
/// resolves widely, occasionally SABR-only on newer versions (hence the
/// 20.10.4 pin). `WEB_REMIX` is excluded permanently — it requires
/// signature deciphering, which is out of scope by contract.
///
/// Attestation: on a flagged IP the bare `player` call is answered
/// `LOGIN_REQUIRED`/bot-check. A video-bound BotGuard poToken carried
/// in `context.serviceIntegrityDimensions` lifts that wall for the
/// web-attestable rungs — live-verified 2026-09: VISIONOS and IOS
/// return full format lists attested where bare requests bot-check;
/// ANDROID_VR stays walled (VR needs DroidGuard, not BotGuard), and
/// MWEB fails `UNPLAYABLE` either way. The resolve therefore runs the
/// ladder bare first, then replays only the bot-checked rungs with
/// attestation — see `guest.rs`.
///
/// Stream caps: any minted URL may be GVS-capped to a ~1 MiB served
/// budget; enforcement is stochastic per-mint, not client-deterministic
/// (live-verified 2026-09: both VISIONOS and ANDROID_VR mints have been
/// observed capped and uncapped). Recovery is a downloader concern —
/// re-resolve for a fresh mint and resume at the written offset — so no
/// rung carries a cap flag.
pub const LADDER: &[Rung] = &[
    Rung {
        name: "VISIONOS",
        client_name_id: "101",
        client_version: "1.02",
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
        context: || {
            json!({
                "deviceMake": "Apple",
                "deviceModel": "RealityDevice17,1",
                "osName": "visionOS",
                "osVersion": "26.5.23O471",
                "hl": "en",
                "gl": "US",
            })
        },
    },
    Rung {
        name: "IOS",
        client_name_id: "5",
        client_version: "20.10.4",
        user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
        context: || {
            json!({
                "deviceMake": "Apple",
                "deviceModel": "iPhone16,2",
                "osName": "iPhone",
                "osVersion": "18.3.2.22F90",
                "hl": "en",
            })
        },
    },
    Rung {
        name: "ANDROID_VR@1.61.48",
        client_name_id: "28",
        client_version: "1.61.48",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.61.48 (Linux; U; Android 12; en_US; Quest 3; Build/SQ3A.220605.009.A1; Cronet/132.0.6808.3)",
        context: || {
            json!({
                "osName": "Android",
                "osVersion": "12",
                "deviceMake": "Oculus",
                "deviceModel": "Quest 3",
                "androidSdkVersion": "32",
                "gl": "US",
                "hl": "en",
            })
        },
    },
    Rung {
        name: "ANDROID_VR@1.60.19",
        client_name_id: "28",
        client_version: "1.60.19",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.60.19 (Linux; U; Android 12; en_US; Quest 3; Build/SQ3A.220605.009.A1; Cronet/107.0.5284.2)",
        context: || {
            json!({
                "osName": "Android",
                "osVersion": "12",
                "deviceMake": "Oculus",
                "deviceModel": "Quest 3",
                "androidSdkVersion": "32",
                "gl": "US",
                "hl": "en",
            })
        },
    },
    Rung {
        name: "ANDROID_VR@1.43.32",
        client_name_id: "28",
        client_version: "1.43.32",
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.43.32 (Linux; U; Android 12; en_US; Quest 3; Build/SQ3A.220605.009.A1; Cronet/107.0.5284.2)",
        context: || {
            json!({
                "osName": "Android",
                "osVersion": "12",
                "deviceMake": "Oculus",
                "deviceModel": "Quest 3",
                "androidSdkVersion": "32",
                "gl": "US",
                "hl": "en",
            })
        },
    },
];

/// The InnerTube `player` endpoint. `music.youtube.com` is the canonical
/// host for this provider.
pub const PLAYER_URL: &str = "https://music.youtube.com/youtubei/v1/player?prettyPrint=false";

/// Append `pot=<token>` to a googlevideo stream URL. Non-googlevideo
/// URLs and URLs already carrying `pot=` pass through unchanged. The
/// token is percent-encoded: providers return URL-safe base64 today,
/// but a `+`, `&`, or `%` would otherwise corrupt the query.
pub fn append_pot(url: &str, token: &str) -> String {
    if !is_googlevideo(url) || url.contains("?pot=") || url.contains("&pot=") {
        return url.to_string();
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}pot={}", url_query_value(token))
}

/// `url` is HTTPS with host `googlevideo.com` or a subdomain — the same
/// shape the manifest's `*.googlevideo.com` allowlist admits. The host
/// still validates the `done` URL; this just avoids leaking the token
/// into a URL shaped like googlevideo that isn't.
fn is_googlevideo(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "googlevideo.com" || host.ends_with(".googlevideo.com")
}

/// RFC 3986 unreserved characters pass through; everything else is
/// percent-encoded (UTF-8, though tokens are ASCII in practice).
fn url_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Bytes requested by a minted-URL probe: the file's last 64 KiB.
/// Strict-mode mints carry a served horizon H (~1 MiB, varies per
/// mint): windows must end at or below H. A tail window ends at the
/// file's last byte, so a 206 proves this mint serves the whole file;
/// a 403 marks the rung capped. When `contentLength` is unknown the
/// probe falls back to a window past the most-observed ~1 MiB horizon
/// — a weaker guarantee (a higher H can still pass and cap later).
pub const PROBE_TAIL_BYTES: u64 = 65536;
/// First byte of the fallback probe window (1 MiB in).
pub const PROBE_FALLBACK_START: u64 = 1_048_576;

/// The Range header value for a probe over `content_length` bytes.
fn probe_range(content_length: Option<u64>) -> String {
    match content_length {
        Some(len) => {
            let start = len.saturating_sub(PROBE_TAIL_BYTES);
            format!("bytes={start}-{}", len.saturating_sub(1))
        }
        None => format!(
            "bytes={PROBE_FALLBACK_START}-{}",
            PROBE_FALLBACK_START + PROBE_TAIL_BYTES - 1
        ),
    }
}

/// Build the tail-probe request for a minted stream URL.
pub fn probe_request(rung: &Rung, url: &str, content_length: Option<u64>) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url: url.into(),
        headers: vec![
            ("User-Agent".into(), rung.user_agent.into()),
            ("Range".into(), probe_range(content_length)),
        ],
        body: None,
    }
}

/// Build one rung's InnerTube `player` call. `pot` is the shared
/// video-bound BotGuard token: when present it rides
/// `context.serviceIntegrityDimensions.poToken`, attesting the player
/// request itself on the rungs that accept web attestation. The same
/// token decorates googlevideo stream URLs via [`append_pot`] — one
/// mint serves both contexts.
pub fn player_request(
    rung: &Rung,
    video_id: &str,
    visitor_id: Option<&str>,
    pot: Option<&str>,
) -> HttpRequest {
    let mut client = serde_json::Map::new();
    client.insert("clientName".into(), json!(rung.innertube_name()));
    client.insert("clientVersion".into(), json!(rung.client_version));
    client.insert("userAgent".into(), json!(rung.user_agent));
    if let Value::Object(extra) = (rung.context)() {
        client.extend(extra);
    }
    let mut context = serde_json::Map::new();
    context.insert("client".into(), Value::Object(client));
    if let Some(token) = pot {
        context.insert(
            "serviceIntegrityDimensions".into(),
            json!({ "poToken": token }),
        );
    }
    let body = json!({
        "context": Value::Object(context),
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        ("User-Agent".into(), rung.user_agent.into()),
        ("X-Goog-Api-Format-Version".into(), "1".into()),
        ("X-YouTube-Client-Name".into(), rung.client_name_id.into()),
        (
            "X-YouTube-Client-Version".into(),
            rung.client_version.into(),
        ),
        ("X-Origin".into(), "https://music.youtube.com".into()),
        ("Referer".into(), "https://music.youtube.com".into()),
    ];
    if let Some(visitor) = visitor_id {
        headers.push(("X-Goog-Visitor-Id".into(), visitor.into()));
    }
    HttpRequest {
        method: "POST".into(),
        url: PLAYER_URL.into(),
        headers,
        body: Some(serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pot_appended_url_encoded() {
        assert_eq!(
            append_pot("https://rr1---sn.googlevideo.com/v?x=1", "tok+en&=%"),
            "https://rr1---sn.googlevideo.com/v?x=1&pot=tok%2Ben%26%3D%25"
        );
    }

    #[test]
    fn pot_skipped_for_foreign_or_decorated_urls() {
        // The token never lands on a non-googlevideo host, even when the
        // URL merely contains the string.
        for url in [
            "https://example.com/v?redir=googlevideo.com",
            "http://rr1---sn.googlevideo.com/v",
            "https://googlevideo.com.evil.com/v",
        ] {
            assert_eq!(append_pot(url, "t"), url);
        }
        let done = "https://rr1---sn.googlevideo.com/v?pot=old";
        assert_eq!(append_pot(done, "new"), done);
    }
}
