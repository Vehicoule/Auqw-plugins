//! The Slice 0 client ladder: `VISIONOS`, `IOS`, three `ANDROID_VR`
//! pins. Client versions are load-bearing: newer IOS builds are served
//! SABR-only. Pin, don't track upstream.

use base64::Engine as _;
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

/// Bytes requested by a minted-URL probe: the file's last 64 KiB.
/// Strict-mode mints carry a served horizon H (~1 MiB, varies per
/// mint): windows must end at or below H. A tail window ends at the
/// file's last byte, so a 206 proves this mint serves the whole file;
/// a 403 marks the rung capped. When `contentLength` is unknown the
/// probe falls back to a window past the most-observed ~1 MiB horizon
/// — a weaker guarantee (a higher H can still pass and cap later).
const PROBE_TAIL_BYTES: u64 = 65536;
const PROBE_FALLBACK_RANGE: &str = "bytes=1048576-1114111";

/// The Range header value for a probe over `content_length` bytes.
fn probe_range(content_length: Option<u64>) -> String {
    match content_length {
        Some(len) => {
            let start = len.saturating_sub(PROBE_TAIL_BYTES);
            format!("bytes={start}-{}", len - 1)
        }
        None => PROBE_FALLBACK_RANGE.to_string(),
    }
}

/// Build the tail-probe `host_request` for a minted stream URL.
pub fn probe_request(
    rung: &Rung,
    url: &str,
    request_id: u32,
    content_length: Option<u64>,
) -> Vec<u8> {
    let msg = json!({
        "type": "host_request",
        "id": request_id,
        "kind": "http_request",
        "payload": {
            "method": "GET",
            "url": url,
            "headers": [
                ["User-Agent", rung.user_agent],
                ["Range", probe_range(content_length)],
            ],
        }
    });
    serde_json::to_vec(&msg).unwrap_or_else(|_| fail("internal", "serialize"))
}

/// Build the `host_request` step message for one rung's player call.
pub fn player_request(
    rung: &Rung,
    video_id: &str,
    request_id: u32,
    visitor_id: Option<&str>,
) -> Vec<u8> {
    let mut client = serde_json::Map::new();
    client.insert("clientName".into(), json!(rung.innertube_name()));
    client.insert("clientVersion".into(), json!(rung.client_version));
    client.insert("userAgent".into(), json!(rung.user_agent));
    if let Value::Object(extra) = (rung.context)() {
        client.extend(extra);
    }
    let body = json!({
        "context": { "client": Value::Object(client) },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let body_bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let mut headers = vec![
        json!(["Content-Type", "application/json"]),
        json!(["User-Agent", rung.user_agent]),
        json!(["X-Goog-Api-Format-Version", "1"]),
        json!(["X-YouTube-Client-Name", rung.client_name_id]),
        json!(["X-YouTube-Client-Version", rung.client_version]),
        json!(["X-Origin", "https://music.youtube.com"]),
        json!(["Referer", "https://music.youtube.com"]),
    ];
    if let Some(visitor) = visitor_id {
        headers.push(json!(["X-Goog-Visitor-Id", visitor]));
    }
    let msg = json!({
        "type": "host_request",
        "id": request_id,
        "kind": "http_request",
        "payload": {
            "method": "POST",
            "url": PLAYER_URL,
            "headers": headers,
            "body": base64::engine::general_purpose::STANDARD.encode(body_bytes),
        }
    });
    serde_json::to_vec(&msg).unwrap_or_else(|_| fail("internal", "serialize"))
}

fn fail(kind: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "type": "fail",
        "error": { "kind": kind, "message": message },
    }))
    .unwrap_or_default()
}
