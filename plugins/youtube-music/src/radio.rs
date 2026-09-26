//! `radio.seed`: a track-seeded automix over the WEB_REMIX InnerTube
//! `next` endpoint — the same metadata-only client
//! `playback.candidates` uses. A `{source_ref}` payload seeds the
//! `RDAMVM<videoId>` queue; a `{continuation}` payload fetches the next
//! page through the opaque token a previous result carried. One `next`
//! call per invocation — the guest never loops upstream, and a page
//! without a `continuations` block is the honest terminal state
//! (`continuation: null`).

use std::collections::BTreeSet;

use auqw_guest_sdk::{http_request, kv_set, GuestError};
use serde_json::{json, Map, Value};

use crate::candidates::{
    best_artwork, browse, duration_ms_of, is_furniture, page_type, run_text, runs_text,
    web_remix_context, web_remix_request, VISITOR_KEY,
};
use crate::guest::{bad_payload, failed, is_video_id, load_visitor, payload_keys, warn};
use crate::parse::{classify_playability, visitor_data, visitor_token, Playability};

const NEXT_URL: &str = "https://music.youtube.com/youtubei/v1/next?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false";

/// A validated `radio.seed` payload: exactly one of the schema's two
/// envelope shapes.
enum RadioPayload {
    /// First page of the mix seeded by this video id.
    Seed(String),
    /// The next page, addressed by the previous page's opaque token.
    Continuation(String),
}

fn parse_radio_payload(payload: &Value) -> Result<(RadioPayload, Option<String>), GuestError> {
    let obj = payload_keys(
        payload,
        &["source_ref", "continuation", "access_token"],
        &[],
    )?;
    // The app-held OAuth access token — same contract as
    // `playback.resolve`: nonempty, bounded, absent means anonymous.
    let access_token = match obj.get("access_token") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 8192 => Some(s.clone()),
        Some(_) => return Err(bad_payload("access_token must be a string 1..8192 chars")),
    };
    let shaped = match (obj.get("source_ref"), obj.get("continuation")) {
        (Some(_), None) => {
            let o = payload_keys(
                &obj["source_ref"],
                &["provider", "kind", "id"],
                &["provider", "kind", "id"],
            )?;
            let provider = o["provider"]
                .as_str()
                .ok_or_else(|| bad_payload("ref.provider must be a string"))?;
            let kind = o["kind"]
                .as_str()
                .ok_or_else(|| bad_payload("ref.kind must be a string"))?;
            let id = o["id"]
                .as_str()
                .ok_or_else(|| bad_payload("ref.id must be a string"))?;
            if provider != "youtube-music" || kind != "track" {
                return Err(failed(
                    "not-applicable",
                    "seed ref is not a youtube-music track ref".into(),
                ));
            }
            if !is_video_id(id) {
                return Err(bad_payload(
                    "source_ref id must be an 11-character video id",
                ));
            }
            Ok(RadioPayload::Seed(id.to_string()))
        }
        (None, Some(token)) => {
            let token = token
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad_payload("continuation must be a nonempty string"))?;
            Ok(RadioPayload::Continuation(token.to_string()))
        }
        _ => Err(bad_payload(
            "exactly one of source_ref or continuation is required",
        )),
    }?;
    Ok((shaped, access_token))
}

/// The `next` body for each payload shape. The seed asks for the
/// video's `RDAMVM` automix queue with a persistent panel; the
/// continuation carries only the opaque token.
fn next_body(p: &RadioPayload) -> Value {
    match p {
        RadioPayload::Seed(video_id) => json!({
            "context": web_remix_context(),
            "videoId": video_id,
            "playlistId": format!("RDAMVM{video_id}"),
            "isAudioOnly": true,
            "enablePersistentPlaylistPanel": true,
            "tunerSettingValue": "AUTOMIX_SETTING_NORMAL",
        }),
        RadioPayload::Continuation(token) => json!({
            "context": web_remix_context(),
            "continuation": token,
        }),
    }
}

/// The watch surface's queue panel for a seed response: the first tab
/// carrying a `musicQueueRenderer` — the Up-next tab's automix list.
fn seed_panel(body: &Value) -> Result<&Map<String, Value>, GuestError> {
    body.pointer(
        "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer\
         /watchNextTabbedResultsRenderer/tabs",
    )
    .and_then(Value::as_array)
    .and_then(|tabs| {
        tabs.iter().find_map(|tab| {
            tab.pointer("/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer")
                .and_then(Value::as_object)
        })
    })
    .ok_or_else(|| {
        failed(
            "invalid-response",
            "next response lacks the queue panel".into(),
        )
    })
}

/// The continuation page's queue panel.
fn continuation_panel(body: &Value) -> Result<&Map<String, Value>, GuestError> {
    body.get("continuationContents")
        .and_then(|c| c.get("playlistPanelContinuation"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            failed(
                "invalid-response",
                "continuation response lacks the queue panel".into(),
            )
        })
}

/// The queue row's renderer: a bare `playlistPanelVideoRenderer` or the
/// primary video inside a `playlistPanelVideoWrapperRenderer` — the
/// wrapper's secondary renderer is decoration, never a queue entry.
fn panel_renderer(entry: &Value) -> Option<&Map<String, Value>> {
    if let Some(r) = entry
        .get("playlistPanelVideoRenderer")
        .and_then(Value::as_object)
    {
        return Some(r);
    }
    entry
        .get("playlistPanelVideoWrapperRenderer")
        .and_then(|w| w.get("primaryRenderer"))
        .and_then(|p| p.get("playlistPanelVideoRenderer"))
        .and_then(Value::as_object)
}

/// The row's video id: the renderer's own `videoId`, else its watch
/// endpoint's.
fn panel_video_id(r: &Map<String, Value>) -> Option<String> {
    r.get("videoId")
        .or_else(|| {
            r.get("navigationEndpoint")
                .and_then(|e| e.get("watchEndpoint"))
                .and_then(|e| e.get("videoId"))
        })
        .and_then(Value::as_str)
        .filter(|s| is_video_id(s))
        .map(str::to_string)
}

/// The `runs` of a byline-style text node (`longBylineText`,
/// `shortBylineText`, `lengthText`).
fn text_runs<'a>(r: &'a Map<String, Value>, key: &str) -> Vec<&'a Map<String, Value>> {
    r.get(key)
        .and_then(|t| t.get("runs"))
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

/// Map one `playlistPanelVideoRenderer` to a `trackMetadata` item;
/// rows without a contract-legal title are not items.
fn panel_item(r: &Map<String, Value>, video_id: String) -> Option<Value> {
    let title = r.get("title").and_then(runs_text)?;
    // `trackMetadata.title` caps at 512 scalars — drop the row rather
    // than emit a contract-invalid result.
    if title.chars().count() > 512 {
        return None;
    }
    let mut artist: Option<String> = None;
    let mut album: Option<String> = None;
    let mut byline_fallback: Option<String> = None;
    // `lengthText` is the authoritative duration; the byline scan below
    // is a fallback for rows that omit it.
    let mut duration_ms: Option<u64> = r
        .get("lengthText")
        .and_then(runs_text)
        .and_then(|t| duration_ms_of(&t));
    // Long byline carries "artist • album"; short carries the artist.
    for run in ["longBylineText", "shortBylineText"]
        .iter()
        .flat_map(|key| text_runs(r, key))
    {
        let Some(text) = run_text(run).map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if let Some(b) = browse(run) {
            if artist.is_none()
                && b.get("browseId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with("UC"))
            {
                artist = Some(text.to_string());
            }
            if artist.is_none() && page_type(run) == Some("MUSIC_PAGE_TYPE_ARTIST") {
                artist = Some(text.to_string());
            }
            if album.is_none() && page_type(run) == Some("MUSIC_PAGE_TYPE_ALBUM") {
                album = Some(text.to_string());
            }
        }
        if duration_ms.is_none() {
            duration_ms = duration_ms_of(text);
        }
        // Artist fallback: first useful byline text — never a type
        // label, separator, duration, or the album run.
        if byline_fallback.is_none()
            && !is_furniture(text)
            && page_type(run) != Some("MUSIC_PAGE_TYPE_ALBUM")
        {
            byline_fallback = Some(text.to_string());
        }
    }
    let artist = artist.or(byline_fallback);
    let artwork = best_artwork(&Value::Object(r.clone()))
        .into_iter()
        .collect::<Vec<Value>>();
    Some(json!({
        "source_ref": { "provider": "youtube-music", "kind": "track", "id": video_id },
        "title": title,
        "artist": artist,
        "album": album,
        "duration_ms": duration_ms,
        "release_year": null,
        "artwork": artwork,
        "explicit": null,
        "genre": null,
        "storefront": null,
    }))
}

/// Upstream queue order, first video id wins — a row that cannot
/// produce an item does not claim its id.
fn panel_items(panel: &Map<String, Value>) -> Vec<Value> {
    let mut items = Vec::new();
    let mut seen = BTreeSet::new();
    let Some(contents) = panel.get("contents").and_then(Value::as_array) else {
        return items;
    };
    for entry in contents {
        let Some(r) = panel_renderer(entry) else {
            continue;
        };
        let Some(video_id) = panel_video_id(r) else {
            continue;
        };
        if seen.contains(&video_id) {
            continue;
        }
        if let Some(item) = panel_item(r, video_id.clone()) {
            seen.insert(video_id);
            items.push(item);
        }
    }
    items
}

/// The panel's next-page token: `continuations` entries carry it under
/// `nextRadioContinuationData` for automix queues and
/// `nextContinuationData` elsewhere — first nonempty token wins; absent
/// is the honest terminal page.
fn next_continuation(panel: &Map<String, Value>) -> Option<String> {
    panel
        .get("continuations")?
        .as_array()?
        .iter()
        .find_map(|c| {
            ["nextRadioContinuationData", "nextContinuationData"]
                .iter()
                .find_map(|key| {
                    c.get(*key)
                        .and_then(|d| d.get("continuation"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
        })
}

/// One InnerTube `next` page. The seed asks for the video's automix
/// queue; a panel `playlistId` naming a different queue is not this
/// seed's radio — like the player path answering a foreign video id,
/// it counts as unavailable rather than a substituted mix.
pub async fn radio_seed(payload: &Value) -> Result<Value, GuestError> {
    let (p, access_token) = parse_radio_payload(payload)?;
    let visitor = load_visitor(VISITOR_KEY).await?;
    let mut resp = match http_request(web_remix_request(
        NEXT_URL,
        next_body(&p),
        visitor.as_deref(),
        access_token.as_deref(),
    ))
    .await
    {
        Ok(r) => r,
        Err(GuestError::Host { kind, message }) => match kind.as_str() {
            "cancelled" | "permission-denied" | "invalid-response" => {
                return Err(GuestError::Host { kind, message });
            }
            _ => return Err(failed("transient", "next transport".into())),
        },
        Err(e) => return Err(e),
    };
    // A 401 proves the token dead — retry once bare so a stale token
    // can't wall the seed, matching the ladder's drop rule.
    if resp.status == 401 && access_token.is_some() {
        resp = match http_request(web_remix_request(
            NEXT_URL,
            next_body(&p),
            visitor.as_deref(),
            None,
        ))
        .await
        {
            Ok(r) => r,
            Err(GuestError::Host { kind, message }) => match kind.as_str() {
                "cancelled" | "permission-denied" | "invalid-response" => {
                    return Err(GuestError::Host { kind, message });
                }
                _ => return Err(failed("transient", "next transport".into())),
            },
            Err(e) => return Err(e),
        };
    }
    match resp.status {
        s if (200..300).contains(&s) => {}
        429 => return Err(failed("rate-limit", "rate-limit".into())),
        _ => return Err(failed("transient", "next transport".into())),
    }
    // A 2xx `next` response must be a JSON envelope; the seed's
    // `playabilityStatus` (when upstream sends one) is classified by
    // the same taxonomy as the player path — a walled or unavailable
    // seed fails honestly, never with a substituted queue.
    let body: Value = serde_json::from_slice(&resp.body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| failed("invalid-response", "next body is not a JSON object".into()))?;
    match classify_playability(&body).0 {
        Playability::Ok => {}
        Playability::BotCheck => return Err(failed("transient", "bot-check".into())),
        Playability::SignInRequired | Playability::AgeRestricted => {
            return Err(failed("auth-required", "sign-in-required".into()));
        }
        Playability::Unavailable => return Err(failed("no-result", "unavailable".into())),
    }
    if let Some(raw) = visitor_data(&body) {
        if let Some(visitor) = visitor_token(&raw) {
            kv_set(VISITOR_KEY, Some(visitor.as_bytes())).await?;
        } else {
            warn("ignoring malformed visitor value").await?;
        }
    }
    let panel = match &p {
        RadioPayload::Seed(video_id) => {
            let panel = seed_panel(&body)?;
            if panel
                .get("playlistId")
                .and_then(Value::as_str)
                .is_some_and(|pid| pid != format!("RDAMVM{video_id}"))
            {
                return Err(failed("no-result", "unavailable".into()));
            }
            panel
        }
        RadioPayload::Continuation(_) => continuation_panel(&body)?,
    };
    Ok(json!({
        "items": panel_items(panel),
        "continuation": next_continuation(panel),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use auqw_guest_sdk::{dispatch_step, reset_for_testing};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use std::collections::BTreeMap;

    const SEED: &str = include_str!("../fixtures/next-automix-seed.json");
    const CONT: &str = include_str!("../fixtures/next-automix-continuation.json");
    const TERMINAL: &str = include_str!("../fixtures/next-automix-terminal.json");
    const UNAVAILABLE: &str = include_str!("../fixtures/next-unavailable-seed.json");
    const VID: &str = "dQw4w9WgXcQ";

    fn step(input: &Value) -> Value {
        let out = dispatch_step(
            &serde_json::to_vec(input).unwrap_or_default(),
            crate::guest::dispatch,
        );
        serde_json::from_slice(&out).unwrap_or_else(|e| panic!("guest output is not JSON: {e}"))
    }

    fn seed_payload() -> Value {
        json!({ "source_ref": { "provider": "youtube-music", "kind": "track", "id": VID } })
    }

    /// Drive until the `next` `http_request` or a terminal message,
    /// answering kv/log like the candidates harness does.
    struct Harness {
        committed: BTreeMap<String, Vec<u8>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                committed: BTreeMap::new(),
            }
        }

        fn drive(&mut self, mut out: Value) -> Value {
            loop {
                if out["type"] != "host_request" || out["kind"] == "http_request" {
                    return out;
                }
                let id = out["id"].as_u64().unwrap_or(u64::MAX);
                out = match out["kind"].as_str().unwrap_or("") {
                    "kv_get" => {
                        let key = out["payload"]["key"].as_str().unwrap_or("").to_string();
                        let value = self
                            .committed
                            .get(&key)
                            .map(|v| Value::String(B64.encode(v)))
                            .unwrap_or(Value::Null);
                        step(&json!({"type":"kv_response","id":id,"value":value}))
                    }
                    "kv_set" => {
                        let key = out["payload"]["key"].as_str().unwrap_or("").to_string();
                        if let Some(v) = out["payload"]["value"]
                            .as_str()
                            .and_then(|s| B64.decode(s).ok())
                        {
                            self.committed.insert(key, v);
                        }
                        step(&json!({"type":"host_ok","id":id}))
                    }
                    "log" => step(&json!({"type":"host_ok","id":id})),
                    other => panic!("unexpected kind {other}"),
                };
            }
        }

        fn invoke(&mut self, payload: Value) -> Value {
            reset_for_testing();
            let out = step(&json!({
                "type": "invoke", "request_id": "t", "capability": "radio.seed",
                "payload": payload,
            }));
            self.drive(out)
        }

        fn answer(&mut self, out: &Value, status: u16, body: &str) -> Value {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            let next = step(&json!({
                "type": "http_response", "id": id, "status": status, "headers": [],
                "body": B64.encode(body),
            }));
            self.drive(next)
        }

        fn answer_host_error(&mut self, out: &Value, kind: &str) -> Value {
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            self.drive(step(&json!({
                "type": "host_error", "id": id,
                "error": { "kind": kind, "message": "host said no" },
            })))
        }
    }

    fn header_of(out: &Value, name: &str) -> Option<String> {
        out["payload"]["headers"]
            .as_array()?
            .iter()
            .find(|h| h[0].as_str() == Some(name))
            .and_then(|h| h[1].as_str().map(str::to_string))
    }

    fn body_of(out: &Value) -> Value {
        serde_json::from_slice(
            &B64.decode(out["payload"]["body"].as_str().unwrap_or(""))
                .unwrap_or_default(),
        )
        .unwrap_or_default()
    }

    fn fail_kind(out: &Value) -> (String, String) {
        assert_eq!(out["type"], "fail");
        (
            out["error"]["kind"].as_str().unwrap_or("").to_string(),
            out["error"]["message"].as_str().unwrap_or("").to_string(),
        )
    }

    #[test]
    fn seed_request_shape_and_headers() {
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        assert_eq!(out["kind"], "http_request");
        assert_eq!(out["payload"]["method"], "POST");
        assert_eq!(
            out["payload"]["url"],
            "https://music.youtube.com/youtubei/v1/next?key=AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30&prettyPrint=false"
        );
        assert_eq!(
            header_of(&out, "X-YouTube-Client-Name").as_deref(),
            Some("67")
        );
        assert_eq!(
            header_of(&out, "Referer").as_deref(),
            Some("https://music.youtube.com")
        );
        let body = body_of(&out);
        // The seed asks for the video's automix queue — never a
        // continuation, never a different playlist.
        assert_eq!(body["videoId"], VID);
        assert_eq!(body["playlistId"], format!("RDAMVM{VID}"));
        assert_eq!(body["isAudioOnly"], true);
        assert_eq!(body["enablePersistentPlaylistPanel"], true);
        assert_eq!(body["tunerSettingValue"], "AUTOMIX_SETTING_NORMAL");
        assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        assert!(body.get("continuation").is_none());
    }

    #[test]
    fn continuation_request_passes_token_verbatim() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "continuation": "radio-cont-001" }));
        let body = body_of(&out);
        assert_eq!(body["continuation"], "radio-cont-001");
        assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        // A paged request names no seed — the token alone addresses the
        // next page.
        assert!(body.get("videoId").is_none());
        assert!(body.get("playlistId").is_none());
    }

    #[test]
    fn seed_fixture_yields_items_and_continuation() {
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let out = h.answer(&out, 200, SEED);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["continuation"], "radio-cont-001");
        let Some(items) = out["result"]["items"].as_array() else {
            panic!("items array");
        };
        // Seed row (upstream order is kept — the mix starts with the
        // seed), the wrapper's primary video, then the deduped row.
        // The secondary renderer, the duplicate, and the id-/title-less
        // rows never surface.
        let ids: Vec<&str> = items
            .iter()
            .map(|i| i["source_ref"]["id"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(ids, ["dQw4w9WgXcQ", "oHg5SJYRHA0", "dupVideoId1"]);
        assert_eq!(
            items[0]["source_ref"],
            json!({"provider":"youtube-music","kind":"track","id":"dQw4w9WgXcQ"})
        );
        assert_eq!(items[0]["title"], "Never Gonna Give You Up");
        assert_eq!(items[0]["artist"], "Rick Astley");
        assert_eq!(items[0]["album"], "Whenever You Need Somebody");
        assert_eq!(items[0]["duration_ms"], 213_000);
        assert_eq!(items[0]["release_year"], Value::Null);
        assert_eq!(items[0]["explicit"], Value::Null);
        let art = &items[0]["artwork"][0];
        assert_eq!(art["width"], 544);
        assert!(art["url"].as_str().unwrap_or("").starts_with("https://"));
        // The wrapped row parses through the primary renderer; its
        // duration comes from byline text when lengthText is absent.
        assert_eq!(items[1]["title"], "Wrapped Together");
        assert_eq!(items[1]["artist"], "Wrapper Artist");
        assert_eq!(items[1]["duration_ms"], 241_000);
        assert_eq!(items[2]["artist"], "Dup Artist");
    }

    #[test]
    fn continuation_fixture_yields_next_page() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "continuation": "radio-cont-001" }));
        let out = h.answer(&out, 200, CONT);
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["continuation"], "radio-cont-002");
        let Some(items) = out["result"]["items"].as_array() else {
            panic!("items array");
        };
        // The watch-endpoint-id row parses too; the cross-page-shaped
        // duplicate within this page is dropped.
        let ids: Vec<&str> = items
            .iter()
            .map(|i| i["source_ref"]["id"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(ids, ["abcDEF123_-", "bareVideoId"]);
        assert_eq!(items[0]["title"], "Page Two Song");
        assert_eq!(items[0]["artist"], "Page Artist");
        assert_eq!(items[0]["album"], "Page Album");
        assert_eq!(items[0]["duration_ms"], 4_820_000);
        // simpleText title + bare artist text byline.
        assert_eq!(items[1]["title"], "Endpoint Id Song");
        assert_eq!(items[1]["artist"], "Endpoint Artist");
        assert_eq!(items[1]["artwork"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn terminal_page_reports_null_continuation() {
        let mut h = Harness::new();
        let out = h.invoke(json!({ "continuation": "radio-cont-002" }));
        let out = h.answer(&out, 200, TERMINAL);
        assert_eq!(out["type"], "done");
        // No continuations block is the honest end — a done with the
        // page's items and an explicit null, never a fabricated token.
        assert_eq!(out["result"]["continuation"], Value::Null);
        assert_eq!(out["result"]["items"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn empty_panel_contents_is_done_not_failure() {
        // A playable seed whose queue upstream answers empty is an
        // honest empty page, not an error.
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let body = json!({
            "playabilityStatus": {"status": "OK"},
            "contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer":
                {"watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"content":
                    {"musicQueueRenderer": {"content": {"playlistPanelRenderer":
                        {"playlistId": format!("RDAMVM{VID}"), "contents": []}}}}}}]}}}},
        });
        let out = h.answer(&out, 200, &body.to_string());
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["items"].as_array().map(Vec::len), Some(0));
        assert_eq!(out["result"]["continuation"], Value::Null);
    }

    #[test]
    fn unavailable_seed_is_no_result() {
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let out = h.answer(&out, 200, UNAVAILABLE);
        assert_eq!(fail_kind(&out).0, "no-result");
    }

    #[test]
    fn foreign_queue_is_never_substituted() {
        // The panel names a different queue than the seed's RDAMVM —
        // serving it would substitute another radio for this seed.
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let mut body: Value = serde_json::from_str(SEED).unwrap_or_default();
        body["contents"]["singleColumnMusicWatchNextResultsRenderer"]["tabbedRenderer"]
            ["watchNextTabbedResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
            ["musicQueueRenderer"]["content"]["playlistPanelRenderer"]["playlistId"] =
            json!("RDAMVMother00000");
        let out = h.answer(&out, 200, &body.to_string());
        assert_eq!(fail_kind(&out).0, "no-result");
    }

    #[test]
    fn bot_check_is_transient_and_sign_in_is_auth() {
        for (status, reason, kind, message) in [
            (
                "LOGIN_REQUIRED",
                "Sign in to confirm you're not a bot",
                "transient",
                "transient: bot-check",
            ),
            (
                "LOGIN_REQUIRED",
                "Please sign in",
                "auth-required",
                "auth-required: sign-in-required",
            ),
        ] {
            let mut h = Harness::new();
            let out = h.invoke(seed_payload());
            let body = json!({ "playabilityStatus": { "status": status, "reason": reason } });
            let out = h.answer(&out, 200, &body.to_string());
            assert_eq!(
                fail_kind(&out),
                (kind.to_string(), message.to_string()),
                "{reason}"
            );
        }
    }

    #[test]
    fn status_taxonomy() {
        for (status, kind) in [
            (429u16, "rate-limit"),
            (503u16, "transient"),
            (404u16, "transient"),
        ] {
            let mut h = Harness::new();
            let out = h.invoke(seed_payload());
            let out = h.answer(&out, status, "{}");
            assert_eq!(fail_kind(&out).0, kind, "status {status}");
        }
        // Terminal host errors propagate; weather maps to transient.
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let out = h.answer_host_error(&out, "cancelled");
        assert_eq!(fail_kind(&out).0, "cancelled");
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let out = h.answer_host_error(&out, "transient");
        assert_eq!(fail_kind(&out).0, "transient");
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let out = h.answer_host_error(&out, "permission-denied");
        assert_eq!(fail_kind(&out).0, "permission-denied");
    }

    #[test]
    fn malformed_bodies_are_invalid_response() {
        // A 2xx `next` body must be a JSON object carrying the queue
        // panel — anything less is upstream breakage.
        for body in [
            "<html>oops</html>",
            "{}",
            "{\"contents\":{}}",
            "{\"playabilityStatus\":{\"status\":\"OK\"}}",
        ] {
            let mut h = Harness::new();
            let out = h.invoke(seed_payload());
            let out = h.answer(&out, 200, body);
            assert_eq!(fail_kind(&out).0, "invalid-response", "{body}");
        }
        // The continuation page must carry its panel too.
        let mut h = Harness::new();
        let out = h.invoke(json!({ "continuation": "radio-cont-001" }));
        let out = h.answer(&out, 200, "{\"continuationContents\":{}}");
        assert_eq!(fail_kind(&out).0, "invalid-response");
    }

    #[test]
    fn malformed_payloads_are_invalid_response() {
        let cases = [
            json!({}),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track", "id": VID }, "continuation": "x" }),
            json!({ "source_ref": VID }),
            json!({ "source_ref": null }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track" } }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track", "id": "short" } }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track", "id": VID, "extra": 1 } }),
            json!({ "continuation": "" }),
            json!({ "continuation": null }),
            json!({ "continuation": 5 }),
            json!({ "continuation": "x", "extra": 1 }),
        ];
        for p in cases {
            let mut h = Harness::new();
            let out = h.invoke(p.clone());
            assert_eq!(fail_kind(&out).0, "invalid-response", "{p}");
        }
    }

    #[test]
    fn foreign_and_nonstrack_seeds_are_not_applicable() {
        for p in [
            json!({ "source_ref": { "provider": "itunes", "kind": "track", "id": VID } }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "album", "id": VID } }),
            json!({ "source_ref": { "provider": "youtube-music", "kind": "artist", "id": VID } }),
        ] {
            let mut h = Harness::new();
            let out = h.invoke(p.clone());
            assert_eq!(fail_kind(&out).0, "not-applicable", "{p}");
        }
    }

    #[test]
    fn persisted_visitor_replays_and_response_visitor_stores() {
        let mut h = Harness::new();
        h.committed
            .insert("visitor/web-remix".into(), b"persisted-wr".to_vec());
        let out = h.invoke(seed_payload());
        assert_eq!(
            header_of(&out, "X-Goog-Visitor-Id").as_deref(),
            Some("persisted-wr")
        );
        let out = h.answer(&out, 200, SEED);
        assert_eq!(out["type"], "done");
        // The response's visitorData is staged for the next call.
        assert_eq!(
            h.committed.get("visitor/web-remix").map(Vec::as_slice),
            Some(b"visitor-wr-002".as_slice())
        );
    }

    // ---- Session trust (access_token) -------------------------------

    #[test]
    fn access_token_rides_next_request() {
        let mut h = Harness::new();
        let mut p = seed_payload();
        p["access_token"] = json!("tok-abc");
        let out = h.invoke(p);
        assert_eq!(
            header_of(&out, "Authorization").as_deref(),
            Some("Bearer tok-abc")
        );
        let out = h.answer(&out, 200, SEED);
        assert_eq!(out["type"], "done");
    }

    #[test]
    fn unauthorized_token_retries_bare() {
        let mut h = Harness::new();
        let mut p = seed_payload();
        p["access_token"] = json!("dead-tok");
        let out = h.invoke(p);
        assert_eq!(
            header_of(&out, "Authorization").as_deref(),
            Some("Bearer dead-tok")
        );
        // The 401 proves the token dead — the retry carries no
        // Authorization header rather than failing the seed.
        let out = h.answer(&out, 401, "{}");
        assert_eq!(out["kind"], "http_request");
        assert_eq!(header_of(&out, "Authorization"), None);
        let out = h.answer(&out, 200, SEED);
        assert_eq!(out["type"], "done");
    }

    #[test]
    fn malformed_access_token_is_invalid_response() {
        for p in [
            json!({ "source_ref": { "provider": "youtube-music", "kind": "track", "id": VID }, "access_token": 42 }),
            json!({ "continuation": "c", "access_token": "x".repeat(8193) }),
        ] {
            let mut h = Harness::new();
            let out = h.invoke(p.clone());
            assert_eq!(fail_kind(&out).0, "invalid-response", "{p}");
        }
    }
}
