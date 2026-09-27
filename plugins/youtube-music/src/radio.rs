//! `radio.seed`: a track-seeded automix over the WEB_REMIX InnerTube
//! `next` endpoint — the same metadata-only client
//! `playback.candidates` uses. A `{source_ref}` payload seeds the
//! `RDAMVM<videoId>` queue; a `{continuation}` payload fetches the next
//! page through the opaque token a previous result carried. One `next`
//! call per invocation — the guest never loops upstream, and a page
//! without a `continuations` block is the honest terminal state
//! (`continuation: null`).

use std::collections::BTreeSet;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;

use auqw_guest_sdk::{http_request, kv_set, GuestError};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::candidates::{
    duration_ms_of, is_furniture, web_remix_context, web_remix_request, VISITOR_KEY,
};
use crate::guest::{bad_payload, failed, is_video_id, load_visitor, payload_keys, warn};
use crate::parse::{has_whole_word_age, visitor_token, Playability};

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
// every field parses through a visitor that collapses a wrong-typed
// value to absent (draining unexpected maps/sequences so the stream
// stays aligned), and every list element that is not an object drops
// out — a malformed row is skipped, never fatal to the page.

/// `as_str`: a string decodes, anything else — scalars, objects, arrays
/// — is absent. Sequences and maps are drained to keep the parser
/// aligned for the next field.
fn opt_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<String>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a string")
        }
        fn visit_str<E>(self, v: &str) -> Result<Option<String>, E> {
            Ok(Some(v.to_owned()))
        }
        fn visit_string<E>(self, v: String) -> Result<Option<String>, E> {
            Ok(Some(v))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Option<String>, A::Error> {
            while s.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Option<String>, A::Error> {
            while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_bool<E>(self, _v: bool) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_i64<E>(self, _v: i64) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_u64<E>(self, _v: u64) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_f64<E>(self, _v: f64) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_char<E>(self, _v: char) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_bytes<E>(self, _v: &[u8]) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_byte_buf<E>(self, _v: Vec<u8>) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_none<E>(self) -> Result<Option<String>, E> {
            Ok(None)
        }
        fn visit_some<D2: serde::Deserializer<'de>>(
            self,
            d: D2,
        ) -> Result<Option<String>, D2::Error> {
            d.deserialize_any(self)
        }
    }
    d.deserialize_any(V)
}

/// `as_u64` on a lazily read field.
fn opt_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<u64>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a u64")
        }
        fn visit_u64<E>(self, v: u64) -> Result<Option<u64>, E> {
            Ok(Some(v))
        }
        fn visit_i64<E>(self, v: i64) -> Result<Option<u64>, E> {
            Ok(u64::try_from(v).ok())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Option<u64>, A::Error> {
            while s.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Option<u64>, A::Error> {
            while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_bool<E>(self, _v: bool) -> Result<Option<u64>, E> {
            Ok(None)
        }
        fn visit_f64<E>(self, _v: f64) -> Result<Option<u64>, E> {
            Ok(None)
        }
        fn visit_str<E>(self, _v: &str) -> Result<Option<u64>, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Option<u64>, E> {
            Ok(None)
        }
        fn visit_none<E>(self) -> Result<Option<u64>, E> {
            Ok(None)
        }
        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Option<u64>, D2::Error> {
            d.deserialize_any(self)
        }
    }
    d.deserialize_any(V)
}

/// `as_object` + shape parse: a map becomes `Some(T)`, anything else is
/// absent. `T`'s own fields are all opt_*, so a struct parse only fails
/// on malformed JSON — which the upfront DOM parse would reject too.
fn opt_obj<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct V<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Option<T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<A: MapAccess<'de>>(self, acc: A) -> Result<Option<T>, A::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(acc)).map(Some)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Option<T>, A::Error> {
            while s.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_bool<E>(self, _v: bool) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_i64<E>(self, _v: i64) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_u64<E>(self, _v: u64) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_f64<E>(self, _v: f64) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_str<E>(self, _v: &str) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_unit<E>(self) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_none<E>(self) -> Result<Option<T>, E> {
            Ok(None)
        }
        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Option<T>, D2::Error> {
            d.deserialize_any(self)
        }
    }
    d.deserialize_any(V(PhantomData))
}

/// A field whose presence — not validity — drives behavior: `Absent`
/// only when the key is missing, `Present` for every supplied value
/// including `null`.
#[derive(Default)]
enum Presence {
    #[default]
    Absent,
    Present(Value),
}

impl<'de> Deserialize<'de> for Presence {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Value::deserialize(d).map(Presence::Present)
    }
}

/// `opt_obj` as a type, for `MapAccess::next_value` targets.
struct OptObj<T>(Option<T>);

impl<T> Default for OptObj<T> {
    fn default() -> Self {
        OptObj(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OptObj<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        opt_obj(d).map(OptObj)
    }
}

impl<T> Deref for OptObj<T> {
    type Target = Option<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// `opt_str` as a type, for `MapAccess::next_value` targets. `Deref`
/// makes it read as `Option<String>` at use sites.
#[derive(Default)]
struct OptStr(Option<String>);

impl<'de> Deserialize<'de> for OptStr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        opt_str(d).map(OptStr)
    }
}

impl Deref for OptStr {
    type Target = Option<String>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// `opt_u64` as a type.
#[derive(Default)]
struct OptU64(Option<u64>);

impl<'de> Deserialize<'de> for OptU64 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        opt_u64(d).map(OptU64)
    }
}

impl Deref for OptU64 {
    type Target = Option<u64>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// `opt_vec` as a type.
struct OptVec<T>(Vec<T>);

impl<T> Default for OptVec<T> {
    fn default() -> Self {
        OptVec(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OptVec<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        opt_vec(d).map(OptVec)
    }
}

impl<T> Deref for OptVec<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
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

impl<T: HasArt> ArtCarrier for OptObj<T> {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        self.0.as_ref().and_then(|t| t.art_out())
    }
}

impl<T: HasArt> ArtCarrier for OptVec<T> {
    fn art_out(&self) -> Option<(u64, Thumb)> {
        let mut best = None;
        for t in &self.0 {
            merge_art(&mut best, t.art_out());
        }
        best
    }
}

/// `art` merge for a `+art` field — a plain decode type carries none.
macro_rules! bubble_art {
    ($art:ident, $field:ident) => {};
    ($art:ident, $field:ident, art) => {
        merge_art(&mut $art, ArtCarrier::art_out(&$field))
    };
}

/// `Deserialize` for an object inside the row renderer: named keys
/// claim typed fields through tolerant wrappers, a `thumbnails` key
/// collects candidates, and every other key is walked for nested art —
/// the old whole-renderer `best_artwork` walk in a single pass, no DOM.
/// Fields marked `+art` bubble their subtree's best thumbnail upward.
/// The generated type gains an `art: Option<(u64, Thumb)>` field and a
/// `HasArt` impl so a parent can collect it.
macro_rules! scanned_obj {
    ($name:ident { $( $field:ident : $key:literal => $(+ $artflag:tt)? $dec:ty ),* $(,)? }) => {
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
                        let mut art = None;
                        $( let mut $field = <$dec>::default(); )*
                        while let Some(k) = m.next_key::<&str>()? {
                            match k {
                                "thumbnails" => {
                                    merge_art(&mut art, m.next_value::<ThumbSet>()?.0)
                                }
                                $( $key => {
                                    $field = m.next_value::<$dec>()?;
                                    bubble_art!(art, $field $(, $artflag)?);
                                } )*
                                _ => merge_art(&mut art, m.next_value::<ArtworkScan>()?.0),
                            }
                        }
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

/// An array element that may be any JSON value: a map parses as `T`,
/// anything else is consumed and dropped — the old `as_array` +
/// per-element `as_object` skip.
struct Tolerant<T>(Option<T>);

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
                    Ok(t) => Ok(Tolerant(Some(t))),
                    Err(_) => {
                        // Element-level rot (e.g. a scalar field type
                        // T cannot tolerate): drain the rest of the
                        // map so the parent array stays aligned, then
                        // drop the element — a dead row, not a dead
                        // page.
                        while acc.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                        Ok(Tolerant(None))
                    }
                }
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Tolerant<T>, A::Error> {
                while s.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Tolerant(None))
            }
            fn visit_bool<E>(self, v: bool) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::BoolDeserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                ))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::I64Deserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                ))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::U64Deserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                ))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::F64Deserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                ))
            }
            fn visit_str<E>(self, v: &str) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::StrDeserializer::<serde::de::value::Error>::new(v),
                    )
                    .ok(),
                ))
            }
            fn visit_string<E>(self, v: String) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(serde::de::value::StringDeserializer::<
                        serde::de::value::Error,
                    >::new(v))
                    .ok(),
                ))
            }
            fn visit_unit<E>(self) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(
                    T::deserialize(
                        serde::de::value::UnitDeserializer::<serde::de::value::Error>::new(),
                    )
                    .ok(),
                ))
            }
            fn visit_none<E>(self) -> Result<Tolerant<T>, E> {
                Ok(Tolerant(None))
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

/// `as_array` + the per-element skip: a non-array field is empty, and
/// each element tolerates any JSON shape.
fn opt_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct V<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut acc: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::new();
            while let Some(Tolerant(t)) = acc.next_element::<Tolerant<T>>()? {
                if let Some(t) = t {
                    out.push(t);
                }
            }
            Ok(out)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Vec<T>, A::Error> {
            while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(Vec::new())
        }
        fn visit_bool<E>(self, _v: bool) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_i64<E>(self, _v: i64) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_u64<E>(self, _v: u64) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_f64<E>(self, _v: f64) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_str<E>(self, _v: &str) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_unit<E>(self) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_none<E>(self) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Vec<T>, D2::Error> {
            d.deserialize_any(self)
        }
    }
    d.deserialize_any(V(PhantomData))
}

/// `opt_vec` keeping every slot as `Option<T>` — callers that join
/// elements need positions, not just survivors.
fn opt_vec_opt<'de, D, T>(d: D) -> Result<Vec<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct V<T>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
        type Value = Vec<Option<T>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut acc: A) -> Result<Vec<Option<T>>, A::Error> {
            let mut out = Vec::new();
            while let Some(Tolerant(t)) = acc.next_element::<Tolerant<T>>()? {
                out.push(t);
            }
            Ok(out)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Vec<Option<T>>, A::Error> {
            while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(Vec::new())
        }
        fn visit_bool<E>(self, _v: bool) -> Result<Vec<Option<T>>, E> {
            Ok(Vec::new())
        }
        fn visit_i64<E>(self, _v: i64) -> Result<Vec<Option<T>>, E> {
            Ok(Vec::new())
        }
        fn visit_u64<E>(self, _v: u64) -> Result<Vec<Option<T>>, E> {
            Ok(Vec::new())
        }
        fn visit_f64<E>(self, _v: f64) -> Result<Vec<Option<T>>, E> {
            Ok(Vec::new())
        }
        fn visit_str<E>(self, _v: &str) -> Result<Vec<Option<T>>, E> {
            Ok(Vec::new())
        }
        fn visit_unit<E>(self) -> Result<Vec<Option<T>>, E> {
            Ok(Vec::new())
        }
        fn visit_none<E>(self) -> Result<Vec<Option<T>>, E> {
            Ok(Vec::new())
        }
        fn visit_some<D2: serde::Deserializer<'de>>(
            self,
            d: D2,
        ) -> Result<Vec<Option<T>>, D2::Error> {
            d.deserialize_any(self)
        }
    }
    d.deserialize_any(V(PhantomData))
}

#[derive(Deserialize)]
struct NextBody {
    #[serde(rename = "playabilityStatus", default, deserialize_with = "opt_obj")]
    playability: Option<NextPlayability>,
    #[serde(rename = "responseContext", default, deserialize_with = "opt_obj")]
    response_context: Option<NextContext>,
    #[serde(default, deserialize_with = "opt_obj")]
    contents: Option<NextContents>,
    #[serde(rename = "continuationContents", default, deserialize_with = "opt_obj")]
    continuation_contents: Option<NextContinuation>,
}

#[derive(Deserialize)]
struct NextPlayability {
    #[serde(default, deserialize_with = "opt_str")]
    status: Option<String>,
    #[serde(default, deserialize_with = "opt_str")]
    reason: Option<String>,
    // Positions preserved: the old blob appended one space per
    // element, non-strings included, so a dropped entry still
    // separates its neighbours (`["not a",42,"bot"]` stays
    // "not a  bot").
    #[serde(default, deserialize_with = "opt_vec_opt")]
    messages: Vec<Option<String>>,
}

#[derive(Deserialize)]
struct NextContext {
    #[serde(rename = "visitorData", default, deserialize_with = "opt_str")]
    visitor_data: Option<String>,
}

#[derive(Deserialize)]
struct NextContents {
    #[serde(
        rename = "singleColumnMusicWatchNextResultsRenderer",
        default,
        deserialize_with = "opt_obj"
    )]
    single_column: Option<SingleColumn>,
}

#[derive(Deserialize)]
struct SingleColumn {
    #[serde(rename = "tabbedRenderer", default, deserialize_with = "opt_obj")]
    tabbed: Option<Tabbed>,
}

#[derive(Deserialize)]
struct Tabbed {
    #[serde(
        rename = "watchNextTabbedResultsRenderer",
        default,
        deserialize_with = "opt_obj"
    )]
    watch_next: Option<WatchNext>,
}

#[derive(Deserialize)]
struct WatchNext {
    #[serde(default, deserialize_with = "opt_vec")]
    tabs: Vec<WatchTab>,
}

#[derive(Deserialize)]
struct WatchTab {
    #[serde(rename = "tabRenderer", default, deserialize_with = "opt_obj")]
    renderer: Option<TabRenderer>,
}

#[derive(Deserialize)]
struct TabRenderer {
    #[serde(default, deserialize_with = "opt_obj")]
    content: Option<TabContent>,
}

#[derive(Deserialize)]
struct TabContent {
    #[serde(rename = "musicQueueRenderer", default, deserialize_with = "opt_obj")]
    queue: Option<QueueRenderer>,
}

#[derive(Deserialize)]
struct QueueRenderer {
    #[serde(default, deserialize_with = "opt_obj")]
    content: Option<QueueContent>,
}

#[derive(Deserialize)]
struct QueueContent {
    #[serde(
        rename = "playlistPanelRenderer",
        default,
        deserialize_with = "opt_obj"
    )]
    panel: Option<Panel>,
}

#[derive(Deserialize)]
struct NextContinuation {
    #[serde(
        rename = "playlistPanelContinuation",
        default,
        deserialize_with = "opt_obj"
    )]
    panel: Option<Panel>,
}

#[derive(Deserialize)]
struct Panel {
    #[serde(rename = "playlistId", default, deserialize_with = "opt_str")]
    playlist_id: Option<String>,
    #[serde(default, deserialize_with = "opt_vec")]
    contents: Vec<RowEntry>,
    #[serde(default, deserialize_with = "opt_vec")]
    continuations: Vec<ContinuationWrap>,
}

#[derive(Deserialize)]
struct RowEntry {
    #[serde(
        rename = "playlistPanelVideoRenderer",
        default,
        deserialize_with = "opt_obj"
    )]
    video: Option<PanelRow>,
    #[serde(
        rename = "playlistPanelVideoWrapperRenderer",
        default,
        deserialize_with = "opt_obj"
    )]
    wrapper: Option<RowWrapper>,
}

#[derive(Deserialize)]
struct RowWrapper {
    #[serde(rename = "primaryRenderer", default, deserialize_with = "opt_obj")]
    primary: Option<PrimaryRenderer>,
}

#[derive(Deserialize)]
struct PrimaryRenderer {
    #[serde(
        rename = "playlistPanelVideoRenderer",
        default,
        deserialize_with = "opt_obj"
    )]
    video: Option<PanelRow>,
}

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

scanned_obj!(PanelRow {
    video_id: "videoId" => Presence,
    navigation: "navigationEndpoint" => +art OptObj<WatchNav>,
    title: "title" => +art OptObj<TextRuns>,
    long_byline: "longBylineText" => +art OptObj<TextRuns>,
    short_byline: "shortBylineText" => +art OptObj<TextRuns>,
    length: "lengthText" => +art OptObj<TextRuns>,
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
                let mut best = None;
                while let Some(ThumbNode(node)) = s.next_element::<ThumbNode>()? {
                    merge_art(&mut best, node);
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
                let mut url = None;
                let mut width = None;
                let mut height = None;
                let mut art = None;
                while let Some(k) = m.next_key::<&str>()? {
                    match k {
                        "url" => url = m.next_value::<OptStr>()?.0,
                        "width" => width = m.next_value::<OptU64>()?.0,
                        "height" => height = m.next_value::<OptU64>()?.0,
                        "thumbnails" => merge_art(&mut art, m.next_value::<ThumbSet>()?.0),
                        _ => merge_art(&mut art, m.next_value::<ArtworkScan>()?.0),
                    }
                }
                offer_art(&mut art, Thumb { url, width, height });
                Ok(ThumbNode(art))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ThumbNode, A::Error> {
                let mut best = None;
                while let Some(ArtworkScan(inner)) = s.next_element::<ArtworkScan>()? {
                    merge_art(&mut best, inner);
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
                let mut best = None;
                while let Some(k) = m.next_key::<&str>()? {
                    if k == "thumbnails" {
                        merge_art(&mut best, m.next_value::<ThumbSet>()?.0);
                    } else {
                        merge_art(&mut best, m.next_value::<ArtworkScan>()?.0);
                    }
                }
                Ok(ArtworkScan(best))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<ArtworkScan, A::Error> {
                let mut best = None;
                while let Some(ArtworkScan(inner)) = s.next_element::<ArtworkScan>()? {
                    merge_art(&mut best, inner);
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
    watch: "watchEndpoint" => +art OptObj<WatchEndpoint>,
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
    runs: "runs" => +art OptVec<BylineRun>,
    simple: "simpleText" => OptStr,
});

struct BylineRun {
    text: OptStr,
    navigation: OptObj<BrowseNav>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BylineRun {
    text: "text" => OptStr,
    navigation: "navigationEndpoint" => +art OptObj<BrowseNav>,
});

struct BrowseNav {
    browse: OptObj<BrowseEndpoint>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseNav {
    browse: "browseEndpoint" => +art OptObj<BrowseEndpoint>,
});

struct BrowseEndpoint {
    id: OptStr,
    context_configs: OptObj<BrowseConfigs>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseEndpoint {
    id: "browseId" => OptStr,
    context_configs: "browseEndpointContextSupportedConfigs" => +art OptObj<BrowseConfigs>,
});

struct BrowseConfigs {
    music: OptObj<BrowseMusic>,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseConfigs {
    music: "browseEndpointContextMusicConfig" => +art OptObj<BrowseMusic>,
});

struct BrowseMusic {
    page_type: OptStr,
    art: Option<(u64, Thumb)>,
}

scanned_obj!(BrowseMusic {
    page_type: "pageType" => OptStr,
});

#[derive(Deserialize)]
struct ContinuationWrap {
    #[serde(
        rename = "nextRadioContinuationData",
        default,
        deserialize_with = "opt_obj"
    )]
    radio: Option<ContinuationData>,
    #[serde(rename = "nextContinuationData", default, deserialize_with = "opt_obj")]
    next: Option<ContinuationData>,
}

#[derive(Deserialize)]
struct ContinuationData {
    #[serde(default, deserialize_with = "opt_str")]
    continuation: Option<String>,
}

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
    let id = match &r.video_id {
        Presence::Present(v) => v.as_str(),
        Presence::Absent => r
            .navigation
            .as_ref()
            .and_then(|n| n.watch.as_ref())
            .and_then(|w| w.video_id.as_deref()),
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
    for entry in &panel.contents {
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
    for message in &status.messages {
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
    let body: NextBody = serde_json::from_slice(&resp.body)
        .map_err(|_| failed("invalid-response", "next body is not a JSON object".into()))?;
    match next_playability(body.playability.as_ref()) {
        Playability::Ok => {}
        Playability::BotCheck => return Err(failed("transient", "bot-check".into())),
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
            kv_set(VISITOR_KEY, Some(visitor.as_bytes())).await?;
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
