//! The Slice 0 client ladder: `IOS`, two `ANDROID_VR` pins, `VISIONOS`.
//!
//! Client versions are load-bearing: newer IOS builds are served
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

/// The ladder, in evidence order. Do not add rungs (Slice 0 scope);
/// `WEB_REMIX` is excluded permanently — it requires signature
/// deciphering, which is out of scope by contract.
pub const LADDER: &[Rung] = &[
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
];

/// The InnerTube `player` endpoint. `music.youtube.com` is the canonical
/// host for this provider.
pub const PLAYER_URL: &str = "https://music.youtube.com/youtubei/v1/player?prettyPrint=false";

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
