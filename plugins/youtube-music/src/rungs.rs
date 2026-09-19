//! The Slice 0 client ladder: exactly two rungs, `IOS` then `ANDROID_VR`.
//!
//! Client names, versions, device fields, and user agents are taken from
//! yt-dlp's public `INNERTUBE_CLIENTS` table
//! (`yt_dlp/extractor/youtube/_base.py`), the canonical public reference.
//! Neither client defines `INNERTUBE_HOST`, so the player endpoint is
//! `www.youtube.com`, not `music.youtube.com`.

use base64::Engine as _;
use serde_json::{json, Value};

/// One ladder rung: an InnerTube client identity.
pub struct Rung {
    /// Name reported as `client` in the resolve result.
    pub name: &'static str,
    /// Numeric `X-Youtube-Client-Name` header value.
    pub client_name_id: u32,
    /// `X-Youtube-Client-Version` header value.
    pub client_version: &'static str,
    /// `User-Agent` header value.
    pub user_agent: &'static str,
    /// Extra fields merged into `context.client` (device, OS).
    pub context_extra: Value,
}

/// The ladder, in order. Do not add rungs (Slice 0 scope).
pub const RUNG_COUNT: usize = 2;

/// Rung metadata by index.
pub fn rung(index: usize) -> Option<Rung> {
    match index {
        0 => Some(Rung {
            name: "IOS",
            client_name_id: 5,
            client_version: "21.26.4",
            user_agent: "com.google.ios.youtube/21.26.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
            context_extra: json!({
                "deviceMake": "Apple",
                "deviceModel": "iPhone16,2",
                "osName": "iPhone",
                "osVersion": "18.3.2.22D82",
            }),
        }),
        1 => Some(Rung {
            name: "ANDROID_VR",
            client_name_id: 28,
            client_version: "1.65.10",
            user_agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
            context_extra: json!({
                "deviceMake": "Oculus",
                "deviceModel": "Quest 3",
                "androidSdkVersion": 32,
                "osName": "Android",
                "osVersion": "12L",
            }),
        }),
        _ => None,
    }
}

/// The InnerTube player endpoint for these clients (`www.youtube.com`;
/// `music.youtube.com` is only the `WEB_REMIX` host).
pub const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";

/// Build the `host_request` step message for one rung's player call.
pub fn player_request(rung_index: usize, video_id: &str, request_id: u32) -> Vec<u8> {
    let Some(r) = rung(rung_index) else {
        return fail("internal", "rung index out of range");
    };
    let mut client = serde_json::Map::new();
    client.insert("clientName".into(), json!(r.name));
    client.insert("clientVersion".into(), json!(r.client_version));
    client.insert("userAgent".into(), json!(r.user_agent));
    if let Value::Object(extra) = r.context_extra {
        client.extend(extra);
    }
    let body = json!({
        "context": { "client": Value::Object(client) },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let body_bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let msg = json!({
        "type": "host_request",
        "id": request_id,
        "kind": "http_request",
        "payload": {
            "method": "POST",
            "url": PLAYER_URL,
            "headers": [
                ["Content-Type", "application/json"],
                ["User-Agent", r.user_agent],
                ["X-Youtube-Client-Name", r.client_name_id.to_string()],
                ["X-Youtube-Client-Version", r.client_version],
                ["Origin", "https://music.youtube.com"],
            ],
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
