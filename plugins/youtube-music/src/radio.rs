//! `radio.seed`: a track-seeded automix over the WEB_REMIX InnerTube
//! `next` endpoint — the same metadata-only client
//! `playback.candidates` uses. A `{source_ref}` payload seeds the
//! `RDAMVM<videoId>` queue; a `{continuation}` payload fetches the next
//! page through the opaque token a previous result carried. One `next`
//! call per invocation — the guest never loops upstream, and a page
//! without a `continuations` block is the honest terminal state
//! (`continuation: null`).

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;

use auqw_guest_sdk::{http_request, GuestError, HttpResponse};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::candidates::{
    duration_ms_of, is_furniture, web_remix_context, web_remix_request, VISITOR_KEY,
};
use crate::guest::{
    bad_payload, failed, is_video_id, kv_set_soft, load_visitor, looks_json, payload_keys,
    truncated_bot_check, warn,
};
use crate::parse::{classify_playability, has_whole_word_age, visitor_token, Playability};

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

// ---- Tolerant `next` view -----------------------------------------------
//
// A ~600 KB `next` body parsed into a `serde_json::Value` DOM costs
// ~340 M fuel — a whisker under the 200 M per-entry cap, and over it on
// marginally larger variants (the mobile `budget-exceeded: fuel`
// failure). The page is deserialized in a single pass into narrow
// structs whose fields all tolerate wrong shapes, so unknown subtrees
// are skipped at tokenize time rather than materialized.
//
// Tolerance mirrors the old `get(k).and_then(as_*)` chains exactly:
// every field parses through a wrapper that collapses a wrong-typed
// value to absent, every list element that is not an object drops out,
// and a repeated key overwrites — `Value` kept the last occurrence, so
// these visitors do too.

/// Old `best_artwork` walk bounds: 64 levels, 10 000 nodes. The scan
/// frames (`ArtworkScan`, `ThumbSet`, `ThumbNode`) share the same
/// budget, kept in thread locals because the recursion goes through
/// serde's `Deserialize` chain and cannot carry state.
///
/// One deliberate divergence: the DOM spent its node budget in *sorted*
/// key order; this scan spends it in document order — sorted-order
/// exhaustion is unrecoverable without buffering each map's values,
/// which is the DOM cost this parse exists to remove. Below the cap
/// output is identical (the sorted `merge_keyed` reproduces DOM merge
/// order); past it, artwork is dropped in document order while
/// metadata keeps decoding. Fuel remains the outer bound either way.
mod scan_depth {
    use std::cell::Cell;

    thread_local! {
        static DEPTH: Cell<u32> = const { Cell::new(0) };
        static NODES: Cell<u32> = const { Cell::new(0) };
    }

    /// A fresh budget for one renderer — called once when a `PanelRow`
    /// decode begins, so sibling fields share the walk's node count
    /// like the old `best_artwork` run did.
    pub(super) fn reset() {
        DEPTH.with(|d| d.set(0));
        NODES.with(|n| n.set(0));
    }

    /// One recursion level; the returned guard unwinds it on drop.
    /// `None` once the depth cap is hit — the caller drains the rest
    /// of that value without descending.
    pub(super) fn enter() -> Option<ScanGuard> {
        DEPTH.with(|d| {
            if d.get() >= 64 {
                None
            } else {
                d.set(d.get() + 1);
                Some(ScanGuard)
            }
        })
    }

    /// One visited key/element; `false` once the node cap is hit.
    pub(super) fn node() -> bool {
        NODES.with(|n| {
            if n.get() >= 10_000 {
                false
            } else {
                n.set(n.get() + 1);
                true
            }
        })
    }

    pub(super) struct ScanGuard;

    impl Drop for ScanGuard {
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
}

/// Drain the rest of a map, scanning each value for artwork —
/// a wrong-shaped field still contributes its `thumbnails`, like the
/// old whole-subtree walk.
fn drain_scan_map<'de, A: MapAccess<'de>>(m: &mut A) -> Result<Option<(u64, Thumb)>, A::Error> {
    // Per-key, last-wins, merged sorted — the DOM's map semantics.
    let mut entries = Vec::new();
    loop {
        if !scan_depth::node() {
            drain_skip_map(m)?;
            break;
        }
        let Some(k) = m.next_key::<Cow<str>>()? else {
            break;
        };
        if k == "thumbnails" {
            note_key(&mut entries, k, m.next_value::<ThumbSet>()?.0);
        } else {
            note_key(&mut entries, k, m.next_value::<ArtworkScan>()?.0);
        }
    }
    Ok(merge_keyed(entries))
}

/// Drain the rest of a sequence, scanning each element for artwork.
fn drain_scan_seq<'de, A: SeqAccess<'de>>(s: &mut A) -> Result<Option<(u64, Thumb)>, A::Error> {
    let mut best = None;
    loop {
        if !scan_depth::node() {
            drain_skip_seq(s)?;
            break;
        }
        match s.next_element::<ArtworkScan>()? {
            Some(ArtworkScan(a)) => merge_art(&mut best, a),
            None => break,
        }
    }
    Ok(best)
}

/// Scanned map entries: each key with the best art found under it.
/// Sparse — only art-bearing keys (and later repeats that must wipe
/// one) are recorded; absent entries contribute nothing anyway.
type KeyedArt<'de> = Vec<(Cow<'de, str>, Option<(u64, Thumb)>)>;

/// Record a key's scan result when it can change the outcome: real
/// art, or a repeat of a key that already yielded some (a last-wins
/// wipe). Artless first-seen keys are skipped — no entry, no alloc.
fn note_key<'de>(entries: &mut KeyedArt<'de>, k: Cow<'de, str>, a: Option<(u64, Thumb)>) {
    if a.is_some() || entries.iter().any(|(pk, _)| *pk == k) {
        entries.push((k, a));
    }
}

/// The same with each entry's occurrence index for last-wins order.
type IndexedKeyedArt<'de> = Vec<(usize, Cow<'de, str>, Option<(u64, Thumb)>)>;

/// Resolve collected `(key, art)` pairs the way the DOM did: a
/// repeated key keeps its last value's art, and distinct keys merge in
/// sorted order. Vec + one sort instead of a `BTreeMap` insert per key
/// — the map version cost ~70 M fuel on a live `next` body.
fn merge_keyed(entries: KeyedArt<'_>) -> Option<(u64, Thumb)> {
    let mut e: IndexedKeyedArt<'_> = entries
        .into_iter()
        .enumerate()
        .map(|(i, (k, a))| (i, k, a))
        .collect();
    // Key ascending, occurrence index descending — the first entry of
    // each equal-key run is the last occurrence.
    e.sort_unstable_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)));
    e.dedup_by(|cur, prev| cur.1 == prev.1);
    let mut best = None;
    for (_, _, a) in e {
        merge_art(&mut best, a);
    }
    best
}

/// Drain without scanning — envelope fields never carried artwork.
fn drain_skip_map<'de, A: MapAccess<'de>>(m: &mut A) -> Result<(), A::Error> {
    while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_skip_seq<'de, A: SeqAccess<'de>>(s: &mut A) -> Result<(), A::Error> {
    while s.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

/// `as_str` as a `next_value` target: a string decodes, anything else
/// is absent — and a map/seq's subtree is still scanned for artwork.
/// `Deref` makes it read as `Option<String>` at use sites.
#[derive(Default)]
struct OptStr {
    v: Option<String>,
    art: Option<(u64, Thumb)>,
}

impl Deref for OptStr {
    type Target = Option<String>;
    fn deref(&self) -> &Self::Target {
        &self.v
    }
}

impl ArtCarrier for OptStr {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        self.art.clone()
    }
}

impl<'de> Deserialize<'de> for OptStr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptStr;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E>(self, v: &str) -> Result<OptStr, E> {
                Ok(OptStr {
                    v: Some(v.to_owned()),
                    art: None,
                })
            }
            fn visit_string<E>(self, v: String) -> Result<OptStr, E> {
                Ok(OptStr {
                    v: Some(v),
                    art: None,
                })
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<OptStr, A::Error> {
                let art = drain_scan_seq(&mut s)?;
                Ok(OptStr { v: None, art })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<OptStr, A::Error> {
                let art = drain_scan_map(&mut m)?;
                Ok(OptStr { v: None, art })
            }
            fn visit_bool<E>(self, _v: bool) -> Result<OptStr, E> {
                Ok(OptStr { v: None, art: None })
            }
            fn visit_i64<E>(self, _v: i64) -> Result<OptStr, E> {
                Ok(OptStr { v: None, art: None })
            }
            fn visit_u64<E>(self, _v: u64) -> Result<OptStr, E> {
                Ok(OptStr { v: None, art: None })
            }
            fn visit_f64<E>(self, _v: f64) -> Result<OptStr, E> {
                Ok(OptStr { v: None, art: None })
            }
            fn visit_unit<E>(self) -> Result<OptStr, E> {
                Ok(OptStr { v: None, art: None })
            }
            fn visit_none<E>(self) -> Result<OptStr, E> {
                Ok(OptStr { v: None, art: None })
            }
            fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<OptStr, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V)
    }
}

/// `as_u64` as a `next_value` target.
#[derive(Default)]
struct OptU64 {
    v: Option<u64>,
    art: Option<(u64, Thumb)>,
}

impl Deref for OptU64 {
    type Target = Option<u64>;
    fn deref(&self) -> &Self::Target {
        &self.v
    }
}

impl ArtCarrier for OptU64 {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        self.art.clone()
    }
}

impl<'de> Deserialize<'de> for OptU64 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OptU64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a u64")
            }
            fn visit_u64<E>(self, v: u64) -> Result<OptU64, E> {
                Ok(OptU64 {
                    v: Some(v),
                    art: None,
                })
            }
            fn visit_i64<E>(self, v: i64) -> Result<OptU64, E> {
                Ok(OptU64 {
                    v: u64::try_from(v).ok(),
                    art: None,
                })
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<OptU64, A::Error> {
                let art = drain_scan_seq(&mut s)?;
                Ok(OptU64 { v: None, art })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<OptU64, A::Error> {
                let art = drain_scan_map(&mut m)?;
                Ok(OptU64 { v: None, art })
            }
            fn visit_bool<E>(self, _v: bool) -> Result<OptU64, E> {
                Ok(OptU64 { v: None, art: None })
            }
            fn visit_f64<E>(self, _v: f64) -> Result<OptU64, E> {
                Ok(OptU64 { v: None, art: None })
            }
            fn visit_str<E>(self, _v: &str) -> Result<OptU64, E> {
                Ok(OptU64 { v: None, art: None })
            }
            fn visit_unit<E>(self) -> Result<OptU64, E> {
                Ok(OptU64 { v: None, art: None })
            }
            fn visit_none<E>(self) -> Result<OptU64, E> {
                Ok(OptU64 { v: None, art: None })
            }
            fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<OptU64, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V)
    }
}

/// `as_object` + shape parse as a `next_value` target: a map becomes
/// `Some(T)`, anything else is absent. A wrong-shaped subtree is still
/// scanned for artwork; a `T` that fails mid-map drains its remaining
/// entries the same way.
struct OptObj<T> {
    v: Option<T>,
    art: Option<(u64, Thumb)>,
}

impl<T> Default for OptObj<T> {
    fn default() -> Self {
        OptObj { v: None, art: None }
    }
}

impl<T> Deref for OptObj<T> {
    type Target = Option<T>;
    fn deref(&self) -> &Self::Target {
        &self.v
    }
}

impl<T: HasArt> ArtCarrier for OptObj<T> {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        let mut best = self.art.clone();
        if let Some(t) = &self.v {
            merge_art(&mut best, t.art_out());
        }
        best
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OptObj<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = OptObj<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut acc: A) -> Result<OptObj<T>, A::Error> {
                match T::deserialize(serde::de::value::MapAccessDeserializer::new(&mut acc)) {
                    Ok(t) => Ok(OptObj {
                        v: Some(t),
                        art: None,
                    }),
                    Err(_) => {
                        let art = drain_scan_map(&mut acc)?;
                        Ok(OptObj { v: None, art })
                    }
                }
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<OptObj<T>, A::Error> {
                let art = drain_scan_seq(&mut s)?;
                Ok(OptObj { v: None, art })
            }
            fn visit_bool<E>(self, _v: bool) -> Result<OptObj<T>, E> {
                Ok(OptObj { v: None, art: None })
            }
            fn visit_i64<E>(self, _v: i64) -> Result<OptObj<T>, E> {
                Ok(OptObj { v: None, art: None })
            }
            fn visit_u64<E>(self, _v: u64) -> Result<OptObj<T>, E> {
                Ok(OptObj { v: None, art: None })
            }
            fn visit_f64<E>(self, _v: f64) -> Result<OptObj<T>, E> {
                Ok(OptObj { v: None, art: None })
            }
            fn visit_str<E>(self, _v: &str) -> Result<OptObj<T>, E> {
                Ok(OptObj { v: None, art: None })
            }
            fn visit_unit<E>(self) -> Result<OptObj<T>, E> {
                Ok(OptObj { v: None, art: None })
            }
            fn visit_none<E>(self) -> Result<OptObj<T>, E> {
                Ok(OptObj { v: None, art: None })
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<OptObj<T>, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// `as_array` + the per-element skip as a `next_value` target: a
/// non-array field is empty (its subtree still scanned for art) and
/// each element tolerates any JSON shape.
struct OptVec<T> {
    v: Vec<T>,
    art: Option<(u64, Thumb)>,
}

impl<T> Default for OptVec<T> {
    fn default() -> Self {
        OptVec {
            v: Vec::new(),
            art: None,
        }
    }
}

impl<T> Deref for OptVec<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Self::Target {
        &self.v
    }
}

impl<T: HasArt> ArtCarrier for OptVec<T> {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        let mut best = self.art.clone();
        for t in &self.v {
            merge_art(&mut best, t.art_out());
        }
        best
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OptVec<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = OptVec<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<OptVec<T>, A::Error> {
                let mut out = Vec::new();
                let mut art = None;
                while let Some(Tolerant(t, a)) = s.next_element::<Tolerant<T>>()? {
                    merge_art(&mut art, a);
                    if let Some(t) = t {
                        out.push(t);
                    }
                }
                Ok(OptVec { v: out, art })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<OptVec<T>, A::Error> {
                let art = drain_scan_map(&mut m)?;
                Ok(OptVec { v: Vec::new(), art })
            }
            fn visit_bool<E>(self, _v: bool) -> Result<OptVec<T>, E> {
                Ok(OptVec {
                    v: Vec::new(),
                    art: None,
                })
            }
            fn visit_i64<E>(self, _v: i64) -> Result<OptVec<T>, E> {
                Ok(OptVec {
                    v: Vec::new(),
                    art: None,
                })
            }
            fn visit_u64<E>(self, _v: u64) -> Result<OptVec<T>, E> {
                Ok(OptVec {
                    v: Vec::new(),
                    art: None,
                })
            }
            fn visit_f64<E>(self, _v: f64) -> Result<OptVec<T>, E> {
                Ok(OptVec {
                    v: Vec::new(),
                    art: None,
                })
            }
            fn visit_str<E>(self, _v: &str) -> Result<OptVec<T>, E> {
                Ok(OptVec {
                    v: Vec::new(),
                    art: None,
                })
            }
            fn visit_unit<E>(self) -> Result<OptVec<T>, E> {
                Ok(OptVec {
                    v: Vec::new(),
                    art: None,
                })
            }
            fn visit_none<E>(self) -> Result<OptVec<T>, E> {
                Ok(OptVec {
                    v: Vec::new(),
                    art: None,
                })
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<OptVec<T>, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// An array element that may be any JSON value, carrying any artwork
/// found while draining a failed `T`.
struct Tolerant<T>(Option<T>, Option<(u64, Thumb)>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Tolerant<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = Tolerant<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut acc: A) -> Result<Tolerant<T>, A::Error> {
                match T::deserialize(serde::de::value::MapAccessDeserializer::new(&mut acc)) {
                    Ok(t) => Ok(Tolerant(Some(t), None)),
                    Err(_) => {
                        // Element-level rot: drain the rest of the map
                        // (scanning for art) so the parent array stays
                        // aligned, then drop the element — a dead row,
                        // not a dead page.
                        let art = drain_scan_map(&mut acc)?;
                        Ok(Tolerant(None, art))
                    }
                }
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Tolerant<T>, A::Error> {
                let art = drain_scan_seq(&mut s)?;
                Ok(Tolerant(None, art))
            }
            fn visit_bool<E>(self, v: bool) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::BoolDeserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                    None,
                ))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::I64Deserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                    None,
                ))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::U64Deserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                    None,
                ))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::F64Deserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                    None,
                ))
            }
            fn visit_str<E>(self, v: &str) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                    None,
                ))
            }
            fn visit_string<E>(self, v: String) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(serde::de::value::StringDeserializer::<
                        serde::de::value::Error,
                    >::new(v))
                    .ok(),
                    None,
                ))
            }
            fn visit_unit<E>(self) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::UnitDeserializer::<serde::de::value::Error>::new(),
                    )
                    .ok(),
                    None,
                ))
            }
            fn visit_none<E>(self) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(None, None))
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<Tolerant<T>, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// `as_array` keeping every slot as `Option<T>` — callers that join
/// elements need positions, not just survivors.
struct OptVecOpt<T>(Vec<Option<T>>);

impl<T> Default for OptVecOpt<T> {
    fn default() -> Self {
        OptVecOpt(Vec::new())
    }
}

impl<T> Deref for OptVecOpt<T> {
    type Target = Vec<Option<T>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OptVecOpt<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = OptVecOpt<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<OptVecOpt<T>, A::Error> {
                let mut out = Vec::new();
                while let Some(Tolerant(t, _)) = s.next_element::<Tolerant<T>>()? {
                    out.push(t);
                }
                Ok(OptVecOpt(out))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<OptVecOpt<T>, A::Error> {
                drain_skip_map(&mut m)?;
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_bool<E>(self, _v: bool) -> Result<OptVecOpt<T>, E> {
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_i64<E>(self, _v: i64) -> Result<OptVecOpt<T>, E> {
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_u64<E>(self, _v: u64) -> Result<OptVecOpt<T>, E> {
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_f64<E>(self, _v: f64) -> Result<OptVecOpt<T>, E> {
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_str<E>(self, _v: &str) -> Result<OptVecOpt<T>, E> {
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_unit<E>(self) -> Result<OptVecOpt<T>, E> {
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_none<E>(self) -> Result<OptVecOpt<T>, E> {
                Ok(OptVecOpt(Vec::new()))
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<OptVecOpt<T>, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

/// A field whose presence — not validity — drives behavior: `Absent`
/// only when the key is missing. `Present` keeps the value's contents
/// when it is a string (`None` for objects, arrays, `null`, …) — the
/// `Value`-shape check without building a DOM — plus whatever art a
/// wrong-shaped subtree carried.
#[derive(Default)]
enum Presence {
    #[default]
    Absent,
    Present(Option<String>, Option<(u64, Thumb)>),
}

impl Presence {
    /// `Value::as_str`: `Some` only for a present string.
    fn as_str(&self) -> Option<&str> {
        match self {
            Presence::Absent => None,
            Presence::Present(v, _) => v.as_deref(),
        }
    }

    /// `Map::get().is_some()`: the key was supplied, whatever its shape.
    fn is_present(&self) -> bool {
        matches!(self, Presence::Present(..))
    }
}

impl<'de> Deserialize<'de> for Presence {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Presence;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_str<E>(self, v: &str) -> Result<Presence, E> {
                Ok(Presence::Present(Some(v.to_owned()), None))
            }
            fn visit_string<E>(self, v: String) -> Result<Presence, E> {
                Ok(Presence::Present(Some(v), None))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Presence, A::Error> {
                let art = drain_scan_map(&mut m)?;
                Ok(Presence::Present(None, art))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Presence, A::Error> {
                let art = drain_scan_seq(&mut s)?;
                Ok(Presence::Present(None, art))
            }
            fn visit_bool<E>(self, _v: bool) -> Result<Presence, E> {
                Ok(Presence::Present(None, None))
            }
            fn visit_i64<E>(self, _v: i64) -> Result<Presence, E> {
                Ok(Presence::Present(None, None))
            }
            fn visit_u64<E>(self, _v: u64) -> Result<Presence, E> {
                Ok(Presence::Present(None, None))
            }
            fn visit_f64<E>(self, _v: f64) -> Result<Presence, E> {
                Ok(Presence::Present(None, None))
            }
            fn visit_unit<E>(self) -> Result<Presence, E> {
                Ok(Presence::Present(None, None))
            }
            fn visit_none<E>(self) -> Result<Presence, E> {
                Ok(Presence::Present(None, None))
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<Presence, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V)
    }
}

impl ArtCarrier for Presence {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        match self {
            Presence::Absent => None,
            Presence::Present(_, art) => art.clone(),
        }
    }
}

/// A decoded subtree that found artwork while scanning unknown keys.
trait HasArt {
    fn art_out(&self) -> Option<(u64, Thumb)>;
}

/// A field-level decode wrapper whose parsed subtree may carry art.
trait ArtCarrier {
    fn art_out(&self) -> Option<(u64, Thumb)>;
}

/// `Deserialize` for an envelope object between the body and the row:
/// named keys claim tolerant wrappers, a repeated key overwrites
/// (`Value` kept the last occurrence), unknown keys are skipped.
macro_rules! lenient_obj {
    ($name:ident { $( $field:ident : $key:literal => $dec:ty ),* $(,)? }) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str(concat!(stringify!($name), " object"))
                    }
                    fn visit_map<A: MapAccess<'de>>(
                        self,
                        mut m: A,
                    ) -> Result<$name, A::Error> {
                        $( let mut $field = <$dec>::default(); )*
                        while let Some(k) = m.next_key::<Cow<str>>()? {
                            match k.as_ref() {
                                $( $key => $field = m.next_value::<$dec>()?, )*
                                _ => {
                                    m.next_value::<IgnoredAny>()?;
                                }
                            }
                        }
                        Ok($name { $($field),* })
                    }
                }
                d.deserialize_any(V)
            }
        }
    };
}

/// Emits `scan_depth::reset()` when the caller flags a row boundary.
macro_rules! row_reset {
    () => {};
    (row) => {
        scan_depth::reset();
    };
}

/// `Deserialize` for an object inside the row renderer: named keys
/// claim typed fields, a `thumbnails` key collects candidates, and
/// every other key is walked for nested art — the old whole-renderer
/// `best_artwork` walk in a single pass, no DOM. Art is kept per key
/// and merged in sorted-key order last-wins, exactly like the `Value`
/// DOM the old walk ran over. `@row` marks the renderer root so the
/// shared depth/node budget resets once per row.
macro_rules! scanned_obj {
    ($name:ident $(@ $flag:ident)? { $( $field:ident : $key:literal => $dec:ty ),* $(,)? }) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str(concat!(stringify!($name), " object"))
                    }
                    fn visit_map<A: MapAccess<'de>>(
                        self,
                        mut m: A,
                    ) -> Result<$name, A::Error> {
                        row_reset!($($flag)?);
                        let mut entries = Vec::new();
                        $( let mut $field = <$dec>::default(); )*
                        loop {
                            // Keys keep decoding past the art budget —
                            // the DOM always parsed them; only scans stop.
                            let scan = scan_depth::node();
                            let Some(k) = m.next_key::<Cow<str>>()? else { break };
                            match k.as_ref() {
                                "thumbnails" => {
                                    if scan {
                                        note_key(
                                            &mut entries,
                                            k,
                                            m.next_value::<ThumbSet>()?.0,
                                        );
                                    } else {
                                        m.next_value::<IgnoredAny>()?;
                                        note_key(&mut entries, k, None);
                                    }
                                }
                                $( $key => {
                                    $field = m.next_value::<$dec>()?;
                                    note_key(
                                        &mut entries,
                                        Cow::Borrowed($key),
                                        $field.art_out(),
                                    );
                                } )*
                                _ => {
                                    if scan {
                                        note_key(
                                            &mut entries,
                                            k,
                                            m.next_value::<ArtworkScan>()?.0,
                                        );
                                    } else {
                                        m.next_value::<IgnoredAny>()?;
                                        note_key(&mut entries, k, None);
                                    }
                                }
                            }
                        }
                        let art = merge_keyed(entries);
                        Ok($name { $($field,)* art })
                    }
                }
                d.deserialize_any(V)
            }
        }
        impl HasArt for $name {
            fn art_out(&self) -> Option<(u64, Thumb)> {
                self.art.clone()
            }
        }
    };
}

struct NextBody {
    playability: OptObj<NextPlayability>,
    response_context: OptObj<NextContext>,
    contents: OptObj<NextContents>,
    continuation_contents: OptObj<NextContinuation>,
}

lenient_obj!(NextBody {
    playability: "playabilityStatus" => OptObj<NextPlayability>,
    response_context: "responseContext" => OptObj<NextContext>,
    contents: "contents" => OptObj<NextContents>,
    continuation_contents: "continuationContents" => OptObj<NextContinuation>,
});

struct NextPlayability {
    status: OptStr,
    reason: OptStr,
    // Positions preserved: the old blob appended one space per
    // element, non-strings included, so a dropped entry still
    // separates its neighbours (`["not a",42,"bot"]` stays
    // "not a  bot").
    messages: OptVecOpt<String>,
}

lenient_obj!(NextPlayability {
    status: "status" => OptStr,
    reason: "reason" => OptStr,
    messages: "messages" => OptVecOpt<String>,
});

struct NextContext {
    visitor_data: OptStr,
}

lenient_obj!(NextContext {
    visitor_data: "visitorData" => OptStr,
});

struct NextContents {
    single_column: OptObj<SingleColumn>,
}

lenient_obj!(NextContents {
    single_column: "singleColumnMusicWatchNextResultsRenderer" => OptObj<SingleColumn>,
});

struct SingleColumn {
    tabbed: OptObj<Tabbed>,
}

lenient_obj!(SingleColumn {
    tabbed: "tabbedRenderer" => OptObj<Tabbed>,
});

struct Tabbed {
    watch_next: OptObj<WatchNext>,
}

lenient_obj!(Tabbed {
    watch_next: "watchNextTabbedResultsRenderer" => OptObj<WatchNext>,
});

struct WatchNext {
    tabs: OptVec<WatchTab>,
}

lenient_obj!(WatchNext {
    tabs: "tabs" => OptVec<WatchTab>,
});

struct WatchTab {
    renderer: OptObj<TabRenderer>,
}

lenient_obj!(WatchTab {
    renderer: "tabRenderer" => OptObj<TabRenderer>,
});

struct TabRenderer {
    content: OptObj<TabContent>,
}

lenient_obj!(TabRenderer {
    content: "content" => OptObj<TabContent>,
});

struct TabContent {
    queue: OptObj<QueueRenderer>,
}

lenient_obj!(TabContent {
    queue: "musicQueueRenderer" => OptObj<QueueRenderer>,
});

struct QueueRenderer {
    content: OptObj<QueueContent>,
}

lenient_obj!(QueueRenderer {
    content: "content" => OptObj<QueueContent>,
});

struct QueueContent {
    panel: OptObj<Panel>,
}

lenient_obj!(QueueContent {
    panel: "playlistPanelRenderer" => OptObj<Panel>,
});

struct NextContinuation {
    panel: OptObj<Panel>,
}

lenient_obj!(NextContinuation {
    panel: "playlistPanelContinuation" => OptObj<Panel>,
});

struct Panel {
    playlist_id: OptStr,
    contents: OptVec<RowEntry>,
    continuations: OptVec<ContinuationWrap>,
}

lenient_obj!(Panel {
    playlist_id: "playlistId" => OptStr,
    contents: "contents" => OptVec<RowEntry>,
    continuations: "continuations" => OptVec<ContinuationWrap>,
});

struct RowEntry {
    video: OptObj<PanelRow>,
    wrapper: OptObj<RowWrapper>,
}

lenient_obj!(RowEntry {
    video: "playlistPanelVideoRenderer" => OptObj<PanelRow>,
    wrapper: "playlistPanelVideoWrapperRenderer" => OptObj<RowWrapper>,
});

struct RowWrapper {
    primary: OptObj<PrimaryRenderer>,
}

lenient_obj!(RowWrapper {
    primary: "primaryRenderer" => OptObj<PrimaryRenderer>,
});

struct PrimaryRenderer {
    video: OptObj<PanelRow>,
}

lenient_obj!(PrimaryRenderer {
    video: "playlistPanelVideoRenderer" => OptObj<PanelRow>,
});

/// The queue row's renderer — decoded via `scanned_obj!` like every
/// object inside it: typed keys claim their fields, `thumbnails` keys
/// collect candidates, and every other key is walked for nested
/// artwork. Together these reproduce the old whole-renderer
/// `best_artwork` walk exactly, in one pass, with no `Value` DOM.
struct PanelRow {
    /// `Absent` only when the key is missing — present-but-invalid
    /// (`null`, `42`) rejects the row without consulting the watch
    /// endpoint, like `get().and_then(as_str)`.
    video_id: Presence,
    navigation: OptObj<WatchNav>,
    title: OptObj<TextRuns>,
    long_byline: OptObj<TextRuns>,
    short_byline: OptObj<TextRuns>,
    length: OptObj<TextRuns>,
    /// Largest valid thumbnail found anywhere in the renderer subtree.
    art: Option<(u64, Thumb)>,
}

scanned_obj!(PanelRow @row {
    video_id: "videoId" => Presence,
    navigation: "navigationEndpoint" => OptObj<WatchNav>,
    title: "title" => OptObj<TextRuns>,
    long_byline: "longBylineText" => OptObj<TextRuns>,
    short_byline: "shortBylineText" => OptObj<TextRuns>,
    length: "lengthText" => OptObj<TextRuns>,
});

/// A `thumbnails` candidate — url plus optional dims.
#[derive(Clone)]
struct Thumb {
    url: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
}

/// The value under a `thumbnails` key: an array collects candidates
/// (each element may itself carry nested `thumbnails`); a non-array
/// value is still walked — the old traversal recursed into it.
struct ThumbSet(Option<(u64, Thumb)>);

impl ArtCarrier for ThumbSet {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        self.0.clone()
    }
}

impl<'de> Deserialize<'de> for ThumbSet {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ThumbSet;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a thumbnails array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ThumbSet, A::Error> {
                let Some(_guard) = scan_depth::enter() else {
                    drain_skip_seq(&mut s)?;
                    return Ok(ThumbSet(None));
                };
                let mut best = None;
                loop {
                    if !scan_depth::node() {
                        drain_skip_seq(&mut s)?;
                        break;
                    }
                    match s.next_element::<ThumbNode>()? {
                        Some(ThumbNode(node)) => merge_art(&mut best, node),
                        None => break,
                    }
                }
                Ok(ThumbSet(best))
            }
            fn visit_map<A: MapAccess<'de>>(self, m: A) -> Result<ThumbSet, A::Error> {
                ArtworkScan::deserialize(serde::de::value::MapAccessDeserializer::new(m))
                    .map(|s| ThumbSet(s.0))
            }
            fn visit_bool<E>(self, _v: bool) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_i64<E>(self, _v: i64) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_u64<E>(self, _v: u64) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_f64<E>(self, _v: f64) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_str<E>(self, _v: &str) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_string<E>(self, _v: String) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_unit<E>(self) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_none<E>(self) -> Result<ThumbSet, E> {
                Ok(ThumbSet(None))
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<ThumbSet, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V)
    }
}

/// One element of a `thumbnails` array: `url`/`width`/`height` make it
/// a candidate, and its remaining keys — including nested
/// `thumbnails` — are still walked for deeper art.
struct ThumbNode(Option<(u64, Thumb)>);

impl<'de> Deserialize<'de> for ThumbNode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ThumbNode;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a thumbnail object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<ThumbNode, A::Error> {
                let Some(_guard) = scan_depth::enter() else {
                    drain_skip_map(&mut m)?;
                    return Ok(ThumbNode(None));
                };
                let mut url = OptStr::default();
                let mut width = OptU64::default();
                let mut height = OptU64::default();
                let mut inner = Vec::new();
                loop {
                    if !scan_depth::node() {
                        drain_skip_map(&mut m)?;
                        break;
                    }
                    let Some(k) = m.next_key::<Cow<str>>()? else {
                        break;
                    };
                    match k.as_ref() {
                        "url" => url = m.next_value::<OptStr>()?,
                        "width" => width = m.next_value::<OptU64>()?,
                        "height" => height = m.next_value::<OptU64>()?,
                        "thumbnails" => {
                            note_key(&mut inner, k, m.next_value::<ThumbSet>()?.0);
                        }
                        _ => {
                            note_key(&mut inner, k, m.next_value::<ArtworkScan>()?.0);
                        }
                    }
                }
                // Wrong-shaped claims still contribute scanned art at
                // their key position, matching the sorted DOM walk.
                note_key(&mut inner, Cow::Borrowed("url"), url.art_out());
                note_key(&mut inner, Cow::Borrowed("width"), width.art_out());
                note_key(&mut inner, Cow::Borrowed("height"), height.art_out());
                // The old walk offered the element itself before
                // descending into it — on equal area the outer
                // candidate wins the tie.
                let mut art = None;
                offer_art(
                    &mut art,
                    Thumb {
                        url: url.v.clone(),
                        width: width.v,
                        height: height.v,
                    },
                );
                merge_art(&mut art, merge_keyed(inner));
                Ok(ThumbNode(art))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ThumbNode, A::Error> {
                let Some(_guard) = scan_depth::enter() else {
                    drain_skip_seq(&mut s)?;
                    return Ok(ThumbNode(None));
                };
                let mut best = None;
                loop {
                    if !scan_depth::node() {
                        drain_skip_seq(&mut s)?;
                        break;
                    }
                    match s.next_element::<ArtworkScan>()? {
                        Some(ArtworkScan(inner)) => merge_art(&mut best, inner),
                        None => break,
                    }
                }
                Ok(ThumbNode(best))
            }
            fn visit_bool<E>(self, _v: bool) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_i64<E>(self, _v: i64) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_u64<E>(self, _v: u64) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_f64<E>(self, _v: f64) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_str<E>(self, _v: &str) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_string<E>(self, _v: String) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_unit<E>(self) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_none<E>(self) -> Result<ThumbNode, E> {
                Ok(ThumbNode(None))
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<ThumbNode, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V)
    }
}

/// `best_artwork` as a zero-DOM recursive visitor: each value is walked
/// once — maps recurse per key, a `thumbnails` key collects candidates —
/// keeping the largest valid thumbnail (https+<=2048 URL, max
/// width*height) exactly as the old whole-renderer walk did.
struct ArtworkScan(Option<(u64, Thumb)>);

impl ArtCarrier for ArtworkScan {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        self.0.clone()
    }
}

/// One thumbnail candidate into the running best.
fn offer_art(best: &mut Option<(u64, Thumb)>, t: Thumb) {
    let Some(url) = t.url.as_deref() else {
        return;
    };
    if !url.starts_with("https://") || url.len() > 2048 {
        return;
    }
    let area = t.width.unwrap_or(0).saturating_mul(t.height.unwrap_or(0));
    if best.as_ref().is_none_or(|(a, _)| area > *a) {
        *best = Some((area, t));
    }
}

/// A finished subtree scan into the running best.
fn merge_art(best: &mut Option<(u64, Thumb)>, other: Option<(u64, Thumb)>) {
    if let Some((area, _)) = other {
        if best.as_ref().is_none_or(|(a, _)| area > *a) {
            *best = other;
        }
    }
}

impl<'de> Deserialize<'de> for ArtworkScan {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ArtworkScan;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<ArtworkScan, A::Error> {
                let Some(_guard) = scan_depth::enter() else {
                    drain_skip_map(&mut m)?;
                    return Ok(ArtworkScan(None));
                };
                let mut entries = Vec::new();
                loop {
                    if !scan_depth::node() {
                        drain_skip_map(&mut m)?;
                        break;
                    }
                    let Some(k) = m.next_key::<Cow<str>>()? else {
                        break;
                    };
                    if k == "thumbnails" {
                        note_key(&mut entries, k, m.next_value::<ThumbSet>()?.0);
                    } else {
                        note_key(&mut entries, k, m.next_value::<ArtworkScan>()?.0);
                    }
                }
                Ok(ArtworkScan(merge_keyed(entries)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ArtworkScan, A::Error> {
                let Some(_guard) = scan_depth::enter() else {
                    drain_skip_seq(&mut s)?;
                    return Ok(ArtworkScan(None));
                };
                let mut best = None;
                loop {
                    if !scan_depth::node() {
                        drain_skip_seq(&mut s)?;
                        break;
                    }
                    match s.next_element::<ArtworkScan>()? {
                        Some(ArtworkScan(inner)) => merge_art(&mut best, inner),
                        None => break,
                    }
                }
                Ok(ArtworkScan(best))
            }
            fn visit_bool<E>(self, _v: bool) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_i64<E>(self, _v: i64) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_u64<E>(self, _v: u64) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_f64<E>(self, _v: f64) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_str<E>(self, _v: &str) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_string<E>(self, _v: String) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_unit<E>(self) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_none<E>(self) -> Result<ArtworkScan, E> {
                Ok(ArtworkScan(None))
            }
            fn visit_some<D2: serde::Deserializer<'de>>(
                self,
                d: D2,
            ) -> Result<ArtworkScan, D2::Error> {
                d.deserialize_any(self)
            }
        }
        d.deserialize_any(V)
    }
}

struct WatchNav {
    watch: OptObj<WatchEndpoint>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(WatchNav {
    watch: "watchEndpoint" => OptObj<WatchEndpoint>,
});

struct WatchEndpoint {
    video_id: OptStr,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(WatchEndpoint {
    video_id: "videoId" => OptStr,
});

struct TextRuns {
    runs: OptVec<BylineRun>,
    simple: OptStr,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(TextRuns {
    runs: "runs" => OptVec<BylineRun>,
    simple: "simpleText" => OptStr,
});

struct BylineRun {
    text: OptStr,
    navigation: OptObj<BrowseNav>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BylineRun {
    text: "text" => OptStr,
    navigation: "navigationEndpoint" => OptObj<BrowseNav>,
});

struct BrowseNav {
    browse: OptObj<BrowseEndpoint>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseNav {
    browse: "browseEndpoint" => OptObj<BrowseEndpoint>,
});

struct BrowseEndpoint {
    id: OptStr,
    context_configs: OptObj<BrowseConfigs>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseEndpoint {
    id: "browseId" => OptStr,
    context_configs: "browseEndpointContextSupportedConfigs" => OptObj<BrowseConfigs>,
});

struct BrowseConfigs {
    music: OptObj<BrowseMusic>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseConfigs {
    music: "browseEndpointContextMusicConfig" => OptObj<BrowseMusic>,
});

struct BrowseMusic {
    page_type: OptStr,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseMusic {
    page_type: "pageType" => OptStr,
});

struct ContinuationWrap {
    radio: OptObj<ContinuationData>,
    next: OptObj<ContinuationData>,
}

lenient_obj!(ContinuationWrap {
    radio: "nextRadioContinuationData" => OptObj<ContinuationData>,
    next: "nextContinuationData" => OptObj<ContinuationData>,
});

struct ContinuationData {
    continuation: OptStr,
}

lenient_obj!(ContinuationData {
    continuation: "continuation" => OptStr,
});

/// The watch surface's queue panel for a seed response: the first tab
/// carrying a `musicQueueRenderer` — the Up-next tab's automix list.
fn seed_panel(body: &NextBody) -> Result<&Panel, GuestError> {
    body.contents
        .as_ref()
        .and_then(|c| c.single_column.as_ref())
        .and_then(|s| s.tabbed.as_ref())
        .and_then(|t| t.watch_next.as_ref())
        .and_then(|w| {
            w.tabs.iter().find_map(|tab| {
                tab.renderer
                    .as_ref()
                    .and_then(|r| r.content.as_ref())
                    .and_then(|c| c.queue.as_ref())
                    .and_then(|q| q.content.as_ref())
                    .and_then(|c| c.panel.as_ref())
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
fn continuation_panel(body: &NextBody) -> Result<&Panel, GuestError> {
    body.continuation_contents
        .as_ref()
        .and_then(|c| c.panel.as_ref())
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
fn panel_row(entry: &RowEntry) -> Option<&PanelRow> {
    entry
        .video
        .as_ref()
        .or_else(|| entry.wrapper.as_ref()?.primary.as_ref()?.video.as_ref())
}

/// The row's video id: the renderer's own `videoId`, else its watch
/// endpoint's — same short-circuit as the Value form: a present but
/// unusable `videoId` rejects the row without consulting the endpoint.
fn panel_video_id(r: &PanelRow) -> Option<&str> {
    let id = if r.video_id.is_present() {
        r.video_id.as_str()
    } else {
        r.navigation
            .as_ref()
            .and_then(|n| n.watch.as_ref())
            .and_then(|w| w.video_id.as_deref())
    };
    id.filter(|s| is_video_id(s))
}

/// A byline/text node's string: the `runs` join when it carries text,
/// else `simpleText` — `runs_text`'s contract verbatim.
fn text_string(t: &TextRuns) -> Option<String> {
    let joined: String = t.runs.iter().filter_map(|r| r.text.as_deref()).collect();
    if !joined.trim().is_empty() {
        return Some(joined.trim().to_string());
    }
    t.simple
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The run's `browseEndpoint` when it is an object — object presence is
/// the gate for artist/album attribution, like `browse()`'s `as_object`.
fn run_browse(run: &BylineRun) -> Option<&BrowseEndpoint> {
    run.navigation.as_ref().and_then(|n| n.browse.as_ref())
}

/// The run's browse endpoint page type.
fn run_page_type(b: Option<&BrowseEndpoint>) -> Option<&str> {
    b.and_then(|b| b.context_configs.as_ref())
        .and_then(|c| c.music.as_ref())
        .and_then(|m| m.page_type.as_deref())
}

/// The row's artwork: the single best thumbnail found anywhere in
/// the renderer, serialized with the dims-null rules `best_artwork`
/// used (`0` dims become `null`, never schema-invalid zeros).
fn row_artwork(r: &PanelRow) -> Vec<Value> {
    r.art
        .as_ref()
        .map(|(_, t)| {
            json!({
                "url": t.url,
                "width": t.width.filter(|w| *w > 0),
                "height": t.height.filter(|h| *h > 0),
            })
        })
        .into_iter()
        .collect()
}

/// Map one `playlistPanelVideoRenderer` to a `trackMetadata` item;
/// rows without a contract-legal title are not items.
fn panel_item(r: &PanelRow, video_id: &str) -> Option<Value> {
    let title = r.title.as_ref().and_then(text_string)?;
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
        .length
        .as_ref()
        .and_then(text_string)
        .and_then(|t| duration_ms_of(&t));
    // Long byline carries "artist • album"; short carries the artist.
    for run in [r.long_byline.as_ref(), r.short_byline.as_ref()]
        .into_iter()
        .flatten()
        .flat_map(|t| t.runs.iter())
    {
        let Some(text) = run.text.as_deref() else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let browse = run_browse(run);
        let page_type = run_page_type(browse);
        if let Some(b) = browse {
            if artist.is_none() && b.id.as_deref().is_some_and(|id| id.starts_with("UC")) {
                artist = Some(text.to_string());
            }
            if artist.is_none() && page_type == Some("MUSIC_PAGE_TYPE_ARTIST") {
                artist = Some(text.to_string());
            }
            if album.is_none() && page_type == Some("MUSIC_PAGE_TYPE_ALBUM") {
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
            && page_type != Some("MUSIC_PAGE_TYPE_ALBUM")
        {
            byline_fallback = Some(text.to_string());
        }
    }
    let artist = artist.or(byline_fallback);
    Some(json!({
        "source_ref": { "provider": "youtube-music", "kind": "track", "id": video_id },
        "title": title,
        "artist": artist,
        "album": album,
        "duration_ms": duration_ms,
        "release_year": null,
        "artwork": row_artwork(r),
        "explicit": null,
        "genre": null,
        "storefront": null,
    }))
}

/// Upstream queue order, first video id wins — a row that cannot
/// produce an item does not claim its id.
fn panel_items(panel: &Panel) -> Vec<Value> {
    let mut items = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in panel.contents.iter() {
        let Some(r) = panel_row(entry) else {
            continue;
        };
        let Some(video_id) = panel_video_id(r) else {
            continue;
        };
        if seen.contains(video_id) {
            continue;
        }
        if let Some(item) = panel_item(r, video_id) {
            seen.insert(video_id.to_string());
            items.push(item);
        }
    }
    items
}

/// The panel's next-page token: `continuations` entries carry it under
/// `nextRadioContinuationData` for automix queues and
/// `nextContinuationData` elsewhere — first nonempty token wins; absent
/// is the honest terminal page.
fn next_continuation(panel: &Panel) -> Option<String> {
    panel.continuations.iter().find_map(|c| {
        [c.radio.as_ref(), c.next.as_ref()]
            .into_iter()
            .find_map(|d| {
                d.and_then(|d| d.continuation.as_deref())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
    })
}

/// `playabilityStatus` over the typed view — the same bucket order as
/// `classify_playability`: OK-or-absent wins, then the reason+messages
/// blob decides bot-check vs age vs sign-in vs unavailable.
fn next_playability(status: Option<&NextPlayability>) -> Playability {
    let Some(status) = status else {
        return Playability::Ok;
    };
    let status_str = status.status.as_deref().unwrap_or("");
    if status_str == "OK" || status_str.is_empty() {
        return Playability::Ok;
    }
    let reason = status.reason.as_deref().unwrap_or("");
    let mut blob = reason.to_lowercase();
    for message in status.messages.iter() {
        blob.push(' ');
        blob.push_str(&message.as_deref().unwrap_or("").to_lowercase());
    }
    if blob.contains("not a bot") || blob.contains("unusual traffic") {
        return Playability::BotCheck;
    }
    if has_whole_word_age(&blob) {
        return Playability::AgeRestricted;
    }
    if blob.contains("sign in") || status_str == "LOGIN_REQUIRED" {
        // `LOGIN_REQUIRED` is the canonical sign-in status even with no
        // reason text — but bot-check reasons under it were caught
        // above, so this arm only fires on a real sign-in wall.
        return Playability::SignInRequired;
    }
    Playability::Unavailable
}

/// The outcome a non-2xx/non-429 `next` refusal books — the same wall
/// shapes `refusal_outcome` reads on the player path. A flagged IP's
/// bare 403 is the abuse edge's HTML interstitial: the bot wall in
/// transport form, terminal like its envelope twin, never transient
/// weather. A parseable JSON refusal body carries a real
/// `playabilityStatus` verdict — it books the same terminal outcome the
/// 2xx path would (`auth-required`, `no-result`, `provider-wall`), so a
/// sign-in wall is never retried as transport weather either.
fn next_refusal(resp: &HttpResponse) -> GuestError {
    if resp.status != 403 {
        return failed("transient", "next transport".into());
    }
    let body = resp
        .body
        .strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(&resp.body);
    if let Ok(b) = serde_json::from_slice::<Value>(body) {
        return match classify_playability(&b).0 {
            Playability::BotCheck => failed("provider-wall", "bot-check".into()),
            Playability::SignInRequired | Playability::AgeRestricted => {
                failed("auth-required", "sign-in-required".into())
            }
            Playability::Unavailable => failed("no-result", "unavailable".into()),
            Playability::Ok => failed("transient", "next transport".into()),
        };
    }
    // Not a JSON envelope: the HTML interstitial, or a truncated
    // JSON-looking body — both are the wall in transport form.
    if !looks_json(body) || truncated_bot_check(body) {
        failed("provider-wall", "bot-check".into())
    } else {
        failed("transient", "next transport".into())
    }
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
        _ => return Err(next_refusal(&resp)),
    }
    // A 2xx `next` response must be a JSON envelope; the seed's
    // `playabilityStatus` (when upstream sends one) is classified by
    // the same taxonomy as the player path — a walled or unavailable
    // seed fails honestly, never with a substituted queue.
    let body: NextBody = serde_json::from_slice(&resp.body)
        .map_err(|_| failed("invalid-response", "next body is not a JSON object".into()))?;
    match next_playability(body.playability.as_ref()) {
        Playability::Ok => {}
        Playability::BotCheck => {
            return Err(failed("provider-wall", "bot-check".into()));
        }
        Playability::SignInRequired | Playability::AgeRestricted => {
            return Err(failed("auth-required", "sign-in-required".into()));
        }
        Playability::Unavailable => return Err(failed("no-result", "unavailable".into())),
    }
    if let Some(raw) = body
        .response_context
        .as_ref()
        .and_then(|c| c.visitor_data.as_deref())
        .filter(|s| !s.is_empty())
    {
        if let Some(visitor) = visitor_token(raw) {
            kv_set_soft(VISITOR_KEY, Some(visitor.as_bytes())).await?;
        } else {
            warn("ignoring malformed visitor value").await?;
        }
    }
    let panel = match &p {
        RadioPayload::Seed(video_id) => {
            let panel = seed_panel(&body)?;
            if panel
                .playlist_id
                .as_deref()
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
    fn bot_check_is_provider_wall_and_sign_in_is_auth() {
        for (status, reason, kind, message) in [
            (
                "LOGIN_REQUIRED",
                "Sign in to confirm you're not a bot",
                "provider-wall",
                "provider-wall: bot-check",
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

    /// The wall in transport form: a flagged IP's `next` is answered
    /// with the abuse edge's interstitial before an envelope exists —
    /// a bare non-JSON 403 is the same provider wall the player path
    /// books, never retryable transport weather.
    #[test]
    fn transport_form_wall_is_provider_wall() {
        for body in [
            "<html><body>Our systems have detected unusual traffic</body></html>",
            "<html>oops</html>",
            "",
        ] {
            let mut h = Harness::new();
            let out = h.invoke(seed_payload());
            let out = h.answer(&out, 403, body);
            assert_eq!(
                fail_kind(&out),
                (
                    "provider-wall".to_string(),
                    "provider-wall: bot-check".to_string()
                ),
                "{body}"
            );
        }
        // A JSON refusal envelope classifies by its playabilityStatus —
        // the bot check inside is the same wall.
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let body = json!({
            "playabilityStatus": {
                "status": "LOGIN_REQUIRED",
                "reason": "Sign in to confirm you're not a bot"
            }
        });
        let out = h.answer(&out, 403, &body.to_string());
        assert_eq!(fail_kind(&out).0, "provider-wall");
        // A wall truncated mid-envelope still carries the marker.
        let mut h = Harness::new();
        let out = h.invoke(seed_payload());
        let out = h.answer(
            &out,
            403,
            "{\"playabilityStatus\":{\"status\":\"LOGIN_REQUIRED\",\"reason\":\"Sign in to confirm you're not a bot",
        );
        assert_eq!(fail_kind(&out).0, "provider-wall");
        // Non-wall refusals stay transient weather.
        for (status, body) in [(403u16, "{}"), (500u16, "<html>oops</html>")] {
            let mut h = Harness::new();
            let out = h.invoke(seed_payload());
            let out = h.answer(&out, status, body);
            assert_eq!(fail_kind(&out).0, "transient", "status {status}");
        }
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
    fn malformed_panel_entries_skip_not_abort() {
        // A wrong-typed renderer value or a non-object contents entry is
        // a dead row, not a dead page — siblings and the continuation
        // survive exactly like the Value lookup chains.
        let body: NextBody = serde_json::from_str(
            r#"{"contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {"watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"musicQueueRenderer": {"content": {"playlistPanelRenderer": {"playlistId": "RDAMVMdQw4w9WgXcQ", "contents": [{"playlistPanelVideoRenderer": 42}, "not-an-object", {"playlistPanelVideoWrapperRenderer": {"primaryRenderer": 5}}, {"playlistPanelVideoRenderer": {"videoId": "dQw4w9WgXcQ", "title": {"simpleText": "Song"}}}], "continuations": [{"nextRadioContinuationData": {"continuation": "NEXT"}}]}}}}}}]}}}}}"#,
        )
        .unwrap_or_else(|e| panic!("body is not the typed view: {e}"));
        let panel = seed_panel(&body).unwrap_or_else(|e| panic!("panel missing: {e}"));
        let items = panel_items(panel);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "Song");
        assert_eq!(next_continuation(panel).as_deref(), Some("NEXT"));
    }

    #[test]
    fn playability_reads_message_only_text() {
        // The blob is reason + messages — a wall explained only in
        // `messages` still classifies, and a bare non-OK status is
        // `Unavailable`, not auth-required.
        for (body, want) in [
            (
                r#"{"playabilityStatus":{"status":"UNPLAYABLE","messages":["Sign in to confirm you're not a bot"]}}"#,
                Playability::BotCheck,
            ),
            (
                r#"{"playabilityStatus":{"status":"ERROR","messages":[5,"content is age restricted",{"x":1}]}}"#,
                Playability::AgeRestricted,
            ),
            (
                r#"{"playabilityStatus":{"status":"AGE_RESTRICTED"}}"#,
                Playability::Unavailable,
            ),
            // A dropped non-string element still separates neighbours —
            // "not a  bot" never contains "not a bot".
            (
                r#"{"playabilityStatus":{"status":"UNPLAYABLE","messages":["not a",42,"bot"]}}"#,
                Playability::Unavailable,
            ),
        ] {
            let b: NextBody =
                serde_json::from_str(body).unwrap_or_else(|e| panic!("body parses: {e}"));
            assert_eq!(next_playability(b.playability.as_ref()), want, "{body}");
        }
    }

    #[test]
    fn null_video_id_rejects_row_without_endpoint_fallback() {
        // A present `videoId` short-circuits the endpoint even when it
        // is null — `get().and_then(as_str)` semantics.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":null,"navigationEndpoint":{"watchEndpoint":{"videoId":"dQw4w9WgXcQ"}},"title":{"simpleText":"Song"}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        assert_eq!(panel_video_id(row), None);
    }

    #[test]
    fn nested_thumbnail_renderer_supplies_artwork() {
        // `thumbnail.musicThumbnailRenderer.thumbnail.thumbnails` is
        // the real upstream shape — artwork is found under any
        // `thumbnails` key inside the row's thumbnail subtree.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"thumbnail":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":"https://example.com/a.jpg","width":226,"height":226}]}}}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        let art = row_artwork(row);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["url"], "https://example.com/a.jpg");
    }

    #[test]
    fn artwork_anywhere_in_renderer_wins_largest() {
        // The old whole-renderer walk: a `thumbnails` array under any
        // key — not only `thumbnail` — supplies artwork, largest wins.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"overlay":{"art":{"thumbnails":[{"url":"http://insecure.example.com/x.jpg","width":999,"height":999},{"url":"https://example.com/small.jpg","width":60,"height":60}]}},"thumbnail":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":"https://example.com/big.jpg","width":544,"height":544}]}}}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        let art = row_artwork(row);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["url"], "https://example.com/big.jpg");
    }

    #[test]
    fn non_array_thumbnails_keeps_row_and_still_walks() {
        // `thumbnails: null` is not artwork but never kills the row;
        // a non-array `thumbnails` object is still walked deeper —
        // both match the old traversal.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"overlay":{"thumbnails":null}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        assert_eq!(panel_video_id(row), Some("dQw4w9WgXcQ"));
        assert!(row_artwork(row).is_empty());

        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"overlay":{"thumbnails":{"nested":{"thumbnails":[{"url":"https://example.com/n.jpg","width":120,"height":120}]}}}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        let art = row_artwork(row);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["url"], "https://example.com/n.jpg");
    }

    #[test]
    fn artwork_inside_typed_fields_is_found() {
        // Typed fields still get scanned: `thumbnails` under `title`
        // or `navigationEndpoint` supplied artwork in the old
        // whole-renderer walk.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song","icon":{"thumbnails":[{"url":"https://example.com/icon.jpg","width":60,"height":60}]}},"navigationEndpoint":{"watchEndpoint":{"videoId":"dQw4w9WgXcQ"},"badge":{"thumbnails":[{"url":"https://example.com/nav.jpg","width":200,"height":200}]}}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        let art = row_artwork(row);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["url"], "https://example.com/nav.jpg");
    }

    #[test]
    fn repeated_envelope_key_keeps_last_value() {
        // `Value` retained the last occurrence of a repeated key; the
        // visitors overwrite the same way instead of erroring.
        let body: NextBody = serde_json::from_str(
            r#"{"contents":null,"contents":{"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {"watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"musicQueueRenderer": {"content": {"playlistPanelRenderer": {"contents": [{"playlistPanelVideoRenderer": {"videoId": "dQw4w9WgXcQ", "title": {"simpleText": "Song"}}}]}}}}}}]}}}}}"#,
        )
        .unwrap_or_else(|e| panic!("body parses: {e}"));
        let panel = seed_panel(&body).unwrap_or_else(|e| panic!("panel found: {e}"));
        assert_eq!(panel.contents.len(), 1);
    }

    #[test]
    fn wrong_shape_runs_still_yields_artwork() {
        // `runs` as an object is drained tolerantly — but the old walk
        // found thumbnails inside it, so the scan must too.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song","runs":{"thumbnails":[{"url":"https://example.com/r.jpg","width":80,"height":80}]}}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        assert!(row.title.as_ref().is_none_or(|t| t.runs.is_empty()));
        let art = row_artwork(row);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["url"], "https://example.com/r.jpg");
    }

    #[test]
    fn artwork_scan_respects_old_depth_bound() {
        // The old walk stopped at 64 levels — thumbnails buried deeper
        // must not be collected, and the row still survives.
        let mut v = String::from(r#"{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"x":"#);
        for _ in 0..70 {
            v.push_str(r#"{"a":"#);
        }
        v.push_str(
            r#"{"thumbnails":[{"url":"https://example.com/deep.jpg","width":10,"height":10}]}"#,
        );
        for _ in 0..70 {
            v.push('}');
        }
        v.push('}');
        let entry: RowEntry =
            serde_json::from_str(&format!(r#"{{"playlistPanelVideoRenderer":{v}}}"#))
                .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        assert_eq!(panel_video_id(row), Some("dQw4w9WgXcQ"));
        assert!(row_artwork(row).is_empty());
    }

    #[test]
    fn escaped_object_keys_do_not_reject_the_page() {
        // A `\u` escape in a key yields an owned string — the DOM had
        // owned keys, so decoding must tolerate them too.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"vid\u0065oId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"\u0065xtra":1}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        assert_eq!(panel_video_id(row), Some("dQw4w9WgXcQ"));
    }

    #[test]
    fn overwritten_field_drops_its_artwork() {
        // Last-wins applies to artwork too: art found under the first
        // `title` is discarded with that value.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Bad","thumbnails":[{"url":"https://example.com/a.jpg","width":500,"height":500}]},"title":{"simpleText":"Song"}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        assert_eq!(
            row.title.as_ref().and_then(|t| t.simple.as_deref()),
            Some("Song")
        );
        assert!(row_artwork(row).is_empty());
    }

    #[test]
    fn equal_area_ties_match_sorted_key_walk() {
        // Sorted-key traversal offered `a` before `z`, and an element
        // before its own descendants — both ties keep the earlier one.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"z":{"thumbnails":[{"url":"https://example.com/z.jpg","width":100,"height":100}]},"a":{"thumbnails":[{"url":"https://example.com/outer.jpg","width":100,"height":100,"nested":{"thumbnails":[{"url":"https://example.com/inner.jpg","width":100,"height":100}]}}]}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        let art = row_artwork(row);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["url"], "https://example.com/outer.jpg");
    }

    #[test]
    fn node_budget_is_shared_across_siblings_and_resets_per_row() {
        // Two fat siblings exhaust one renderer's node budget; the next
        // row gets a fresh walk.
        let mut x = String::from(r#""x":{"#);
        for i in 0..5000 {
            if i > 0 {
                x.push(',');
            }
            x.push_str(&format!(r#""k{i}":1"#));
        }
        x.push_str(r#"},"y":{"#);
        for i in 0..5000 {
            if i > 0 {
                x.push(',');
            }
            x.push_str(&format!(r#""k{i}":1"#));
        }
        // x+y burn the budget; `z`'s thumbnails arrive after the cap.
        x.push_str(r#"},"z":{"thumbnails":[{"url":"https://example.com/late.jpg","width":10,"height":10}]}"#);
        let r1: RowEntry = serde_json::from_str(&format!(
            "{{\"playlistPanelVideoRenderer\":{{\"videoId\":\"r1\",\"title\":{{\"simpleText\":\"A\"}},{x}}}}}"
        ))
        .unwrap_or_else(|e| panic!("row parses: {e}"));
        let r1 = panel_row(&r1).unwrap_or_else(|| panic!("row 1 present"));
        assert!(row_artwork(r1).is_empty());

        // The next row decodes with a fresh budget — `reset` runs at
        // the `PanelRow` boundary.
        let r2: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"r2","title":{"simpleText":"B"},"ok":{"thumbnails":[{"url":"https://example.com/fresh.jpg","width":10,"height":10}]}}}"#,
        )
        .unwrap_or_else(|e| panic!("row parses: {e}"));
        let r2 = panel_row(&r2).unwrap_or_else(|| panic!("row 2 present"));
        let art = row_artwork(r2);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0]["url"], "https://example.com/fresh.jpg");
    }

    #[test]
    fn escaped_key_inside_unknown_subtree_keeps_row() {
        // An escaped key nested inside an unknown field decodes owned —
        // the row's claims survive the scan of that subtree.
        let entry: RowEntry = serde_json::from_str(
            r#"{"playlistPanelVideoRenderer":{"videoId":"dQw4w9WgXcQ","title":{"simpleText":"Song"},"overlay":{"\u0061rt":1}}}"#,
        )
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row present"));
        assert_eq!(panel_video_id(row), Some("dQw4w9WgXcQ"));
    }

    #[test]
    fn exhausted_art_budget_still_claims_required_fields() {
        // The node cap bounds the artwork walk, never the field claims:
        // metadata keys after the cap still decode, art is empty.
        let mut fat = String::new();
        for i in 0..10_005 {
            if i > 0 {
                fat.push(',');
            }
            fat.push_str(&format!(r#""k{i}":1"#));
        }
        let entry: RowEntry = serde_json::from_str(&format!(
            r#"{{"playlistPanelVideoRenderer":{{"fat":{{{fat}}},"videoId":"dQw4w9WgXcQ","title":{{"simpleText":"Song"}},"thumbnails":[{{"url":"https://example.com/late.jpg","width":10,"height":10}}]}}}}"#
        ))
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row survives the cap"));
        assert_eq!(panel_video_id(row), Some("dQw4w9WgXcQ"));
        assert_eq!(
            row.title.as_ref().and_then(|t| t.simple.as_deref()),
            Some("Song")
        );
        assert!(row_artwork(row).is_empty());
    }

    #[test]
    fn artwork_before_the_cap_still_lands() {
        // Bounded scans run in document order (the old DOM walked sorted
        // keys — that ordering is unrecoverable without buffering, which
        // is the DOM cost this parse removed). Art seen before the cap
        // is kept; only what comes after loses it.
        let mut fat = String::new();
        for i in 0..10_005 {
            if i > 0 {
                fat.push(',');
            }
            fat.push_str(&format!(r#""k{i}":1"#));
        }
        let entry: RowEntry = serde_json::from_str(&format!(
            r#"{{"playlistPanelVideoRenderer":{{"videoId":"dQw4w9WgXcQ","title":{{"simpleText":"Song"}},"thumbnails":[{{"url":"https://example.com/early.jpg","width":10,"height":10}}],"fat":{{{fat}}}}}}}"#
        ))
        .unwrap_or_else(|e| panic!("entry parses: {e}"));
        let row = panel_row(&entry).unwrap_or_else(|| panic!("row survives"));
        assert_eq!(panel_video_id(row), Some("dQw4w9WgXcQ"));
        assert_eq!(
            row_artwork(row)[0].get("url"),
            Some(&json!("https://example.com/early.jpg"))
        );
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

    /// A 403 `next` refusal books by its body: the interstitial wall
    /// and envelope walls are terminal on their own kinds; only a
    /// verdict-less JSON or a non-403 refusal stays transport weather.
    #[test]
    fn next_refusal_books_by_body() {
        let refusal = |status: u16, body: &[u8]| match next_refusal(&HttpResponse {
            status,
            headers: vec![],
            body: body.to_vec(),
        }) {
            GuestError::Failed { kind, .. } | GuestError::Host { kind, .. } => kind,
            _ => "other".into(),
        };
        // HTML interstitial — the bot wall in transport form.
        assert_eq!(
            refusal(403, b"<html>unusual traffic</html>"),
            "provider-wall"
        );
        // JSON envelopes book their verdict's own terminal kind —
        // sign-in walls are never retried as transient.
        assert_eq!(
            refusal(
                403,
                br#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Please sign in"}}"#
            ),
            "auth-required"
        );
        assert_eq!(
            refusal(
                403,
                br#"{"playabilityStatus":{"status":"ERROR","reason":"Video unavailable"}}"#
            ),
            "no-result"
        );
        // A verdict-less parseable body stays weather.
        assert_eq!(refusal(403, br#"{"error":{}}"#), "transient");
        // Non-403 refusals stay weather whatever they carry.
        assert_eq!(refusal(500, b"<html></html>"), "transient");
    }
}
