//! Guest-side SDK over the Auqw plugin step ABI (0.2.0).
//!
//! A plugin crate writes an ordinary `async` dispatch function and
//! exports it with [`export_plugin!`]; the shim below inverts the ABI's
//! step loop into futures, so host services read as plain awaits. All
//! host contact happens through `host_request` step messages — the SDK
//! itself has no network or filesystem access.

use std::alloc::Layout;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::{json, Value};

/// One plugin invocation handed to the dispatch function.
pub struct Invocation {
    /// Host-generated request id.
    pub request_id: String,
    /// Capability the host invoked (declared in the manifest).
    pub capability: String,
    /// Caller-supplied capability payload.
    pub payload: Value,
}

/// The future a guest dispatch function returns.
pub type GuestFuture = Pin<Box<dyn Future<Output = Result<Value, GuestError>>>>;

/// Signature of the guest's dispatch function, wired by
/// [`export_plugin!`].
pub type DispatchFn = fn(Invocation) -> GuestFuture;

/// Terminal error channel for dispatch: becomes the guest's `fail`
/// step message.
#[derive(Debug)]
pub enum GuestError {
    /// The host answered a `host_request` with `host_error`; `kind` is
    /// the ABI error kind verbatim (`permission-denied`,
    /// `unsupported`, ...).
    Host {
        /// ABI error kind from the host.
        kind: String,
        /// Host-provided detail.
        message: String,
    },
    /// The host violated the step protocol: a response id that matches
    /// no outstanding request, a wrong-shaped reply, or off-contract
    /// bytes. Surfaces as `invalid-response`.
    InvalidResponse(String),
    /// Guest-declared failure; `kind` must be a guest-visible ABI
    /// error kind.
    Failed {
        /// ABI error kind.
        kind: String,
        /// Detail text.
        message: String,
    },
}

impl std::fmt::Display for GuestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host { kind, message } | Self::Failed { kind, message } => {
                write!(f, "{kind}: {message}")
            }
            Self::InvalidResponse(m) => write!(f, "invalid-response: {m}"),
        }
    }
}

impl std::error::Error for GuestError {}

/// An outbound request for [`http_request`].
pub struct HttpRequest {
    /// `GET` or `POST`.
    pub method: String,
    /// `https://` destination the manifest permits.
    pub url: String,
    /// Header pairs.
    pub headers: Vec<(String, String)>,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
}

/// A host-relayed HTTP response.
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Header pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// Severity for [`log`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Debug detail.
    Debug,
    /// Informational.
    Info,
    /// Warning.
    Warn,
    /// Error.
    Error,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// The `errorKind` vocabulary from `messages.schema.json`: the only
/// kinds a `host_error` may carry and the only kinds a guest `fail`
/// may emit.
const ERROR_KINDS: &[&str] = &[
    "no-result",
    "not-applicable",
    "unsupported",
    "auth-required",
    "auth-expired",
    "rate-limit",
    "transient",
    "expired-resource",
    "permission-denied",
    "invalid-response",
    "timeout",
    "cancelled",
];

/// Shim state kept between `handle` entries. Guests are single-
/// threaded, so thread-local storage is the whole synchronization.
/// The future and the protocol state live in separate cells: polls
/// re-enter the state cell, so a borrow may never span a poll.
#[derive(Default)]
struct Inner {
    /// The request a parked future is waiting on, ready to emit.
    pending: Option<PendingRequest>,
    /// A host reply held for the awaiting request's next poll, keyed
    /// by the request id it answers.
    response: Option<(u32, Value)>,
    /// The outstanding request id awaiting a host reply.
    awaiting: Option<u32>,
    /// Next host-request id.
    next_id: u32,
}

struct PendingRequest {
    id: u32,
    msg: Value,
}

thread_local! {
    /// In-flight dispatch future; `None` before `invoke` and after a
    /// terminal result.
    static FUTURE: RefCell<Option<GuestFuture>> = const { RefCell::new(None) };
    static INNER: RefCell<Inner> = RefCell::new(Inner::default());
    static DISPATCH: RefCell<Option<DispatchFn>> = const { RefCell::new(None) };
}

/// Declare the plugin's entry points. The `dispatch` function must
/// have signature `fn(Invocation) -> GuestFuture`.
///
/// ```ignore
/// auqw_guest_sdk::export_plugin!(dispatch);
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($dispatch:ident) => {
        /// ABI-required allocator: hands out heap bytes for the host to
        /// fill with the next step message.
        #[no_mangle]
        pub extern "C" fn alloc(len: u32) -> u32 {
            $crate::__alloc(len)
        }

        /// ABI-required step entry.
        ///
        /// # Safety
        /// Relies on the ABI contract: `ptr`/`len` describe a readable
        /// buffer the host wrote into this guest's linear memory.
        #[no_mangle]
        pub extern "C" fn handle(ptr: u32, len: u32) -> u64 {
            $crate::__handle(ptr, len, $dispatch)
        }
    };
}

/// Backing for the macro-generated `alloc` export.
#[doc(hidden)]
#[must_use]
pub fn __alloc(len: u32) -> u32 {
    // `alloc` with a zero-sized layout is UB; a 0 request gets a
    // 1-byte buffer it never writes through.
    let Ok(layout) = Layout::from_size_align((len as usize).max(1), 1) else {
        return 0;
    };
    // SAFETY: `layout` has nonzero size by construction; the returned
    // pointer is a valid guest-owned buffer of `len` bytes.
    unsafe { std::alloc::alloc(layout) as u32 }
}

/// Backing for the macro-generated `handle` export.
///
/// # Safety
/// Relies on the ABI contract: `ptr`/`len` describe a readable buffer
/// the host wrote into this guest's linear memory.
#[doc(hidden)]
#[must_use]
pub fn __handle(ptr: u32, len: u32, dispatch: DispatchFn) -> u64 {
    DISPATCH.with(|d| *d.borrow_mut() = Some(dispatch));
    // SAFETY: per the ABI, `ptr..ptr+len` is initialized guest memory
    // owned by this module.
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let out = handle_step(input);
    let out_ptr = out.as_ptr() as u64;
    let out_len = out.len() as u64;
    // Leak the response buffer: it must stay valid until the next
    // `handle`/`alloc` call, and the instance dies with the invocation.
    std::mem::forget(out);
    (out_ptr << 32) | out_len
}

/// Feed one host step message into the shim; returns the guest's reply
/// bytes. Exposed for SDK tests and the generated `handle` export.
#[doc(hidden)]
#[must_use]
pub fn handle_step(input: &[u8]) -> Vec<u8> {
    let reply = match serde_json::from_slice::<Value>(input) {
        Err(e) => fail("invalid-response", &format!("step input not JSON: {e}")),
        Ok(msg) => step(msg),
    };
    serde_json::to_vec(&reply)
        .unwrap_or_else(|_| b"{\"type\":\"fail\",\"error\":{\"kind\":\"invalid-response\",\"message\":\"reply serialize\"}}".to_vec())
}

/// Register `dispatch` and feed one host step message into the shim;
/// returns the guest's reply bytes. Exposed so plugin-native fixture
/// tests can drive the exact artifact protocol without a WASM host.
#[doc(hidden)]
#[must_use]
pub fn dispatch_step(input: &[u8], dispatch: DispatchFn) -> Vec<u8> {
    DISPATCH.with(|d| *d.borrow_mut() = Some(dispatch));
    handle_step(input)
}

/// Drop all thread-local shim state — pending future, protocol state,
/// and the registered dispatch — so a native fixture test can start a
/// fresh invocation even when a previous one parked mid-flight.
#[doc(hidden)]
pub fn reset_for_testing() {
    clear_state();
    DISPATCH.with(|d| *d.borrow_mut() = None);
}

fn step(msg: Value) -> Value {
    match msg.get("type").and_then(Value::as_str) {
        Some("invoke") => {
            let busy = FUTURE.with(|f| f.borrow().is_some())
                || INNER.with(|i| i.borrow().awaiting.is_some());
            if busy {
                return fail("invalid-response", "invoke while one is in flight");
            }
            let dispatch = DISPATCH.with(|d| *d.borrow());
            let Some(dispatch) = dispatch else {
                return fail("invalid-response", "no dispatch registered");
            };
            let invocation = match parse_invocation(&msg) {
                Ok(i) => i,
                Err(e) => return fail("invalid-response", &e.to_string()),
            };
            FUTURE.with(|f| *f.borrow_mut() = Some(dispatch(invocation)));
            drive()
        }
        Some("http_response" | "kv_response" | "host_ok" | "now_response" | "host_error") => {
            if let Err(e) = validate_reply(&msg) {
                return fail("invalid-response", &e.to_string());
            }
            let accepted: Result<(), &'static str> = INNER.with(|i| {
                let mut inner = i.borrow_mut();
                match inner.awaiting.take() {
                    None => Err("response with no outstanding request"),
                    Some(awaiting) => {
                        let rid = msg
                            .get("id")
                            .and_then(Value::as_u64)
                            .and_then(|v| u32::try_from(v).ok());
                        if rid == Some(awaiting) {
                            inner.response = Some((awaiting, msg.clone()));
                            Ok(())
                        } else {
                            Err("response id mismatch")
                        }
                    }
                }
            });
            if let Err(e) = accepted {
                return fail("invalid-response", e);
            }
            drive()
        }
        _ => fail("invalid-response", "unknown host step message"),
    }
}

/// Reject a host reply that violates the schema: exact keys per type,
/// a `u32` id, and the field types the typed accessors rely on.
/// Anything off-contract is a protocol violation — the SDK never
/// exposes it to the dispatch future.
fn validate_reply(msg: &Value) -> Result<(), GuestError> {
    let bad = |m: String| GuestError::InvalidResponse(m);
    let obj = msg
        .as_object()
        .ok_or_else(|| bad("reply must be an object".into()))?;
    let Some(t) = msg.get("type").and_then(Value::as_str) else {
        return Err(bad("reply.type missing".into()));
    };
    let allowed: &[&str] = match t {
        "http_response" => &["type", "id", "status", "headers", "body"],
        "host_error" => &["type", "id", "error"],
        "kv_response" => &["type", "id", "value"],
        "host_ok" => &["type", "id"],
        "now_response" => &["type", "id", "now_ms"],
        other => return Err(bad(format!("reply.type {other:?} unknown"))),
    };
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(bad(format!("{t}.{key} is not in the ABI schema")));
        }
    }
    for key in allowed.iter().filter(|k| **k != "type") {
        if !obj.contains_key(*key) {
            return Err(bad(format!("{t}.{key} missing")));
        }
    }
    msg.get("id")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| bad(format!("{t}.id missing or out of range")))?;
    match t {
        "http_response" => {
            msg.get("status")
                .and_then(Value::as_u64)
                .and_then(|v| u16::try_from(v).ok())
                .ok_or_else(|| bad("http_response.status missing or out of range".into()))?;
            let headers = msg
                .get("headers")
                .and_then(Value::as_array)
                .ok_or_else(|| bad("http_response.headers missing".into()))?;
            for h in headers {
                let pair = h.as_array().filter(|p| p.len() == 2).ok_or_else(|| {
                    bad("http_response header must be a [name, value] pair".into())
                })?;
                if !pair.iter().all(Value::is_string) {
                    return Err(bad("http_response header entries must be strings".into()));
                }
            }
            msg.get("body")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("http_response.body missing".into()))?;
        }
        "host_error" => {
            let error = msg
                .get("error")
                .and_then(Value::as_object)
                .ok_or_else(|| bad("host_error.error missing".into()))?;
            for key in error.keys() {
                if !["kind", "message"].contains(&key.as_str()) {
                    return Err(bad(format!(
                        "host_error.error.{key} is not in the ABI schema"
                    )));
                }
            }
            let kind = error
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("host_error.error.kind missing".into()))?;
            if !ERROR_KINDS.contains(&kind) {
                return Err(bad(format!(
                    "host_error.error.kind {kind:?} is not in the ABI taxonomy"
                )));
            }
            error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("host_error.error.message missing".into()))?;
        }
        "kv_response" => match msg.get("value") {
            Some(Value::Null) | Some(Value::String(_)) => {}
            _ => {
                return Err(bad(
                    "kv_response.value must be a base64 string or null".into()
                ));
            }
        },
        "now_response" => {
            msg.get("now_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| bad("now_response.now_ms missing".into()))?;
        }
        // `host_ok` carries only type+id — checked above.
        _ => {}
    }
    Ok(())
}

fn parse_invocation(msg: &Value) -> Result<Invocation, GuestError> {
    let bad = |m: String| GuestError::InvalidResponse(format!("invoke.{m}"));
    let obj = msg
        .as_object()
        .ok_or_else(|| bad("must be an object".into()))?;
    const KEYS: [&str; 4] = ["type", "request_id", "capability", "payload"];
    for key in obj.keys() {
        if !KEYS.contains(&key.as_str()) {
            return Err(bad(format!("unexpected key {key}")));
        }
    }
    for key in KEYS {
        if !obj.contains_key(key) {
            return Err(bad(format!("{key} missing")));
        }
    }
    let payload = obj["payload"].clone();
    if !payload.is_object() {
        return Err(bad("payload must be an object".into()));
    }
    Ok(Invocation {
        request_id: obj["request_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad("request_id missing".into()))?
            .to_string(),
        capability: obj["capability"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad("capability missing".into()))?
            .to_string(),
        payload,
    })
}

/// Poll the in-flight dispatch once and translate the outcome into the
/// next step output.
fn drive() -> Value {
    let Some(mut fut) = FUTURE.with(|f| f.borrow_mut().take()) else {
        return fail("invalid-response", "no in-flight invocation");
    };
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(result)) => {
            clear_state();
            json!({ "type": "done", "result": result })
        }
        Poll::Ready(Err(e)) => fail(error_kind(&e), &e.to_string()),
        Poll::Pending => {
            FUTURE.with(|f| *f.borrow_mut() = Some(fut));
            let req = INNER.with(|i| {
                let mut inner = i.borrow_mut();
                inner.pending.take().map(|req| {
                    inner.awaiting = Some(req.id);
                    req.msg
                })
            });
            match req {
                Some(msg) => msg,
                None => fail(
                    "invalid-response",
                    "dispatch pending without a host request",
                ),
            }
        }
    }
}

fn error_kind(e: &GuestError) -> &str {
    let kind = match e {
        GuestError::Host { kind, .. } | GuestError::Failed { kind, .. } => kind.as_str(),
        _ => return "invalid-response",
    };
    if ERROR_KINDS.contains(&kind) {
        kind
    } else {
        "invalid-response"
    }
}

fn clear_state() {
    FUTURE.with(|f| *f.borrow_mut() = None);
    INNER.with(|i| *i.borrow_mut() = Inner::default());
}

fn fail(kind: &str, message: &str) -> Value {
    clear_state();
    json!({ "type": "fail", "error": { "kind": kind, "message": message } })
}

/// The future backing every service call: parks the dispatch until the
/// host's reply arrives on a later `handle` entry.
struct HostCall {
    kind: &'static str,
    payload: Value,
    id: Option<u32>,
}

impl Future for HostCall {
    type Output = Result<Value, GuestError>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        INNER.with(|i| {
            let mut inner = i.borrow_mut();
            if let Some((rid, response)) = inner.response.take() {
                // The reply belongs to exactly one request; consuming a
                // response minted for a different id is a violation.
                return if Some(rid) == self.id {
                    Poll::Ready(Ok(response))
                } else {
                    Poll::Ready(Err(GuestError::InvalidResponse(
                        "response id mismatch".into(),
                    )))
                };
            }
            match self.id {
                None => {
                    // The ABI is sequential: one host request in flight
                    // at a time. A second call polled while another
                    // request is pending/awaiting is a violation.
                    if inner.pending.is_some() || inner.awaiting.is_some() {
                        return Poll::Ready(Err(GuestError::InvalidResponse(
                            "concurrent host calls are not supported".into(),
                        )));
                    }
                    let id = inner.next_id;
                    inner.next_id = inner.next_id.wrapping_add(1);
                    self.id = Some(id);
                    inner.pending = Some(PendingRequest {
                        id,
                        msg: json!({
                            "type": "host_request",
                            "id": id,
                            "kind": self.kind,
                            "payload": self.payload,
                        }),
                    });
                }
                Some(id)
                    if inner.awaiting.is_some_and(|a| a != id)
                        || inner.pending.as_ref().is_some_and(|p| p.id != id) =>
                {
                    return Poll::Ready(Err(GuestError::InvalidResponse(
                        "concurrent host calls are not supported".into(),
                    )));
                }
                Some(_) => {}
            }
            Poll::Pending
        })
    }
}

fn host_call(kind: &'static str, payload: Value) -> HostCall {
    HostCall {
        kind,
        payload,
        id: None,
    }
}

/// Pull the `error` object out of a `host_error` reply, or accept the
/// reply when it has `want_type`.
fn expect_type(resp: Value, want_type: &str) -> Result<Value, GuestError> {
    match resp.get("type").and_then(Value::as_str) {
        Some("host_error") => {
            let error = &resp["error"];
            Err(GuestError::Host {
                kind: error
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("transient")
                    .to_string(),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        }
        Some(t) if t == want_type => Ok(resp),
        _ => Err(GuestError::InvalidResponse(format!(
            "expected {want_type} reply"
        ))),
    }
}

fn parse_http_response(resp: Value) -> Result<HttpResponse, GuestError> {
    let resp = expect_type(resp, "http_response")?;
    let bad = |m: &str| GuestError::InvalidResponse(format!("http_response.{m}"));
    let status = resp
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| bad("status missing"))?;
    let raw_headers = resp
        .get("headers")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("headers missing"))?;
    let mut headers = Vec::with_capacity(raw_headers.len());
    for h in raw_headers {
        let pair = h
            .as_array()
            .filter(|p| p.len() == 2)
            .ok_or_else(|| bad("header must be a [name, value] pair"))?;
        let name = pair[0]
            .as_str()
            .ok_or_else(|| bad("header name must be a string"))?;
        let value = pair[1]
            .as_str()
            .ok_or_else(|| bad("header value must be a string"))?;
        headers.push((name.to_string(), value.to_string()));
    }
    let body = resp
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("body missing"))
        .and_then(|s| B64.decode(s).map_err(|_| bad("body is not base64")))?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Perform an authorized HTTPS request through the host.
///
/// # Errors
/// [`GuestError::Host`] when the host replies `host_error`;
/// [`GuestError::InvalidResponse`] on a protocol violation.
pub async fn http_request(req: HttpRequest) -> Result<HttpResponse, GuestError> {
    let resp = host_call(
        "http_request",
        json!({
            "method": req.method,
            "url": req.url,
            "headers": req.headers,
            "body": req.body.map(|b| B64.encode(b)),
        }),
    )
    .await?;
    parse_http_response(resp)
}

/// Ask the host to mint a PO token for `content_binding` against its
/// configured provider. The reply is the provider's HTTP response
/// verbatim.
///
/// # Errors
/// [`GuestError::Host`] on `host_error` (`permission-denied` /
/// `unsupported`); [`GuestError::InvalidResponse`] on a protocol
/// violation.
pub async fn pot_token(content_binding: &str) -> Result<HttpResponse, GuestError> {
    let resp = host_call("pot_token", json!({ "content_binding": content_binding })).await?;
    parse_http_response(resp)
}

/// Continue a fetch at a byte offset through the host's `resume` step
/// (ABI 0.3.0): the host issues `GET url` with a `Range` header it
/// builds itself and verifies a `206` response's `Content-Range`
/// against the request. Any other status passes through as the
/// upstream's real answer (`200` = range ignored, `416` = past EOF).
///
/// # Errors
/// [`GuestError::Host`] on `host_error` (`permission-denied` for an
/// unlisted destination, `invalid-response` for a `Content-Range`
/// mismatch); [`GuestError::InvalidResponse`] on a protocol violation.
pub async fn resume(
    url: &str,
    offset: u64,
    length: Option<u64>,
) -> Result<HttpResponse, GuestError> {
    let payload = match length {
        Some(l) => json!({ "url": url, "offset": offset, "length": l }),
        None => json!({ "url": url, "offset": offset }),
    };
    let resp = host_call("resume", payload).await?;
    parse_http_response(resp)
}

/// Read `key` from this plugin's KV namespace; `None` when absent.
///
/// # Errors
/// [`GuestError::Host`] on `host_error` (`permission-denied`);
/// [`GuestError::InvalidResponse`] on a protocol violation.
pub async fn kv_get(key: &str) -> Result<Option<Vec<u8>>, GuestError> {
    let resp = expect_type(
        host_call("kv_get", json!({ "key": key })).await?,
        "kv_response",
    )?;
    match resp.get("value") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => B64
            .decode(s)
            .map(Some)
            .map_err(|_| GuestError::InvalidResponse("kv_response.value is not base64".into())),
        _ => Err(GuestError::InvalidResponse(
            "kv_response.value missing".into(),
        )),
    }
}

/// Stage a write into this plugin's KV namespace; `None` deletes the
/// key. Staged writes commit only if the invocation ends `done`.
///
/// # Errors
/// [`GuestError::Host`] on `host_error` (`permission-denied`,
/// `invalid-response` for size caps);
/// [`GuestError::InvalidResponse`] on a protocol violation.
pub async fn kv_set(key: &str, value: Option<&[u8]>) -> Result<(), GuestError> {
    expect_type(
        host_call(
            "kv_set",
            json!({ "key": key, "value": value.map(|v| B64.encode(v)) }),
        )
        .await?,
        "host_ok",
    )?;
    Ok(())
}

/// Append a redacted entry to the invocation's diagnostics.
///
/// # Errors
/// [`GuestError::InvalidResponse`] on a protocol violation.
pub async fn log(level: LogLevel, message: &str) -> Result<(), GuestError> {
    expect_type(
        host_call(
            "log",
            json!({ "level": level.as_str(), "message": message }),
        )
        .await?,
        "host_ok",
    )?;
    Ok(())
}

/// The host clock's epoch milliseconds.
///
/// # Errors
/// [`GuestError::InvalidResponse`] on a protocol violation.
pub async fn now_ms() -> Result<u64, GuestError> {
    let resp = expect_type(host_call("now_ms", json!({})).await?, "now_response")?;
    resp.get("now_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| GuestError::InvalidResponse("now_response.now_ms missing".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset() {
        reset_for_testing();
    }

    fn dispatch_register(d: DispatchFn) {
        DISPATCH.with(|slot| *slot.borrow_mut() = Some(d));
    }

    fn step_json(input: &Value) -> Value {
        serde_json::from_slice(&handle_step(&serde_json::to_vec(input).unwrap_or_default()))
            .unwrap_or_else(|_| json!({"unparseable": true}))
    }

    fn kv_dispatch(inv: Invocation) -> GuestFuture {
        Box::pin(async move {
            let existing = kv_get("visitor").await?;
            kv_set("visitor", Some(b"v2")).await?;
            let read = kv_get("visitor").await?;
            let now = now_ms().await?;
            log(LogLevel::Info, "hello https://a.b/x?sig=SECRET").await?;
            Ok(json!({
                "cap": inv.capability,
                "existing_null": existing.is_none(),
                "read_len": read.map_or(0, |v| v.len()),
                "now": now,
            }))
        })
    }

    #[test]
    fn shim_drives_kv_log_and_clock() {
        reset();
        dispatch_register(kv_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r1", "capability": "playback.resolve",
            "payload": {},
        }));
        assert_eq!(out["type"], "host_request");
        assert_eq!(out["kind"], "kv_get");
        let id = out["id"].as_u64().unwrap_or(u64::MAX);

        let out = step_json(&json!({"type": "kv_response", "id": id, "value": null}));
        assert_eq!(out["kind"], "kv_set");

        let out = step_json(&json!({"type": "host_ok", "id": out["id"]}));
        assert_eq!(out["kind"], "kv_get");

        let out = step_json(&json!({
            "type": "kv_response", "id": out["id"], "value": B64.encode(b"v2"),
        }));
        assert_eq!(out["kind"], "now_ms");

        let out = step_json(&json!({"type": "now_response", "id": out["id"], "now_ms": 42}));
        assert_eq!(out["kind"], "log");

        let out = step_json(&json!({"type": "host_ok", "id": out["id"]}));
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["existing_null"], true);
        assert_eq!(out["result"]["read_len"], 2);
        assert_eq!(out["result"]["now"], 42);
        reset();
    }

    fn done_dispatch(_inv: Invocation) -> GuestFuture {
        Box::pin(async move { Ok(json!({"ok": true})) })
    }

    #[test]
    fn done_without_requests() {
        reset();
        dispatch_register(done_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["ok"], true);
        reset();
    }

    /// `dispatch_step` is the native-test entry: it registers the
    /// dispatch and drives the same strict protocol as `handle`.
    #[test]
    fn dispatch_step_drives_protocol() {
        reset();
        let out: Value = serde_json::from_slice(&dispatch_step(
            &serde_json::to_vec(&json!({
                "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
            }))
            .unwrap_or_default(),
            done_dispatch,
        ))
        .unwrap_or_else(|_| json!({"unparseable": true}));
        assert_eq!(out["type"], "done");
        assert_eq!(out["result"]["ok"], true);
        reset();
    }

    /// `reset_for_testing` clears a parked invocation: after it, a new
    /// `invoke` is accepted instead of "invoke while one is in flight".
    #[test]
    fn reset_for_testing_clears_pending_invocation() {
        reset();
        dispatch_register(kv_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["kind"], "kv_get");
        reset_for_testing();
        let out: Value = serde_json::from_slice(&dispatch_step(
            &serde_json::to_vec(&json!({
                "type": "invoke", "request_id": "r2", "capability": "x", "payload": {},
            }))
            .unwrap_or_default(),
            done_dispatch,
        ))
        .unwrap_or_else(|_| json!({"unparseable": true}));
        assert_eq!(out["type"], "done");
        reset();
    }

    #[test]
    fn response_id_mismatch_is_invalid_response() {
        reset();
        dispatch_register(kv_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r1", "capability": "x", "payload": {},
        }));
        assert_eq!(out["kind"], "kv_get");
        let out = step_json(&json!({"type": "kv_response", "id": 9999, "value": null}));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        reset();
    }

    #[test]
    fn response_with_no_request_is_invalid_response() {
        reset();
        dispatch_register(done_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["type"], "done");
        let out = step_json(&json!({"type": "host_ok", "id": 0}));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        reset();
    }

    fn fail_dispatch(_inv: Invocation) -> GuestFuture {
        Box::pin(async move {
            Err(GuestError::Failed {
                kind: "transient".into(),
                message: "nope".into(),
            })
        })
    }

    #[test]
    fn dispatch_error_becomes_fail() {
        reset();
        dispatch_register(fail_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "transient");
        assert_eq!(out["error"]["message"], "transient: nope");
        reset();
    }

    fn http_dispatch(_inv: Invocation) -> GuestFuture {
        Box::pin(async move {
            let resp = http_request(HttpRequest {
                method: "GET".into(),
                url: "https://a.test/".into(),
                headers: vec![],
                body: None,
            })
            .await?;
            Ok(json!({ "status": resp.status }))
        })
    }

    /// A response header that is not a `[name, value]` pair is a
    /// protocol violation, not a silently dropped entry.
    #[test]
    fn malformed_header_tuple_is_invalid_response() {
        reset();
        dispatch_register(http_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["kind"], "http_request");
        let id = out["id"].as_u64().unwrap_or(u64::MAX);
        let out = step_json(&json!({
            "type": "http_response", "id": id, "status": 200,
            "headers": [["only-name"]], "body": "",
        }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        reset();
    }

    /// A `host_error` must be exactly `{kind, message}` with a kind
    /// from the ABI taxonomy — extra keys or an unknown kind are
    /// protocol violations.
    #[test]
    fn malformed_host_error_is_invalid_response() {
        for error in [
            json!({"kind": "transient", "message": "x", "extra": 1}),
            json!({"kind": "budget-exceeded", "message": "x"}),
            json!({"kind": "transient"}),
        ] {
            reset();
            dispatch_register(http_dispatch);
            let out = step_json(&json!({
                "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
            }));
            let id = out["id"].as_u64().unwrap_or(u64::MAX);
            let out = step_json(&json!({"type": "host_error", "id": id, "error": error}));
            assert_eq!(out["type"], "fail", "{error}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{error}");
            reset();
        }
    }

    /// `invoke` requires exactly `type`/`request_id`/`capability`/
    /// `payload` with an object payload — a missing or non-object
    /// payload never reaches the dispatch function.
    #[test]
    fn malformed_invoke_is_invalid_response() {
        for input in [
            json!({"type": "invoke", "request_id": "r", "capability": "x"}),
            json!({"type": "invoke", "request_id": "r", "capability": "x", "payload": "s"}),
            json!({"type": "invoke", "request_id": "r", "capability": "x", "payload": {}, "extra": 1}),
            json!({"type": "invoke", "request_id": "", "capability": "x", "payload": {}}),
        ] {
            reset();
            dispatch_register(done_dispatch);
            let out = step_json(&input);
            assert_eq!(out["type"], "fail", "{input}");
            assert_eq!(out["error"]["kind"], "invalid-response", "{input}");
            reset();
        }
    }

    fn bad_kind_dispatch(_inv: Invocation) -> GuestFuture {
        Box::pin(async move {
            Err(GuestError::Failed {
                kind: "budget-exceeded".into(),
                message: "not my word".into(),
            })
        })
    }

    /// A guest-declared `fail` kind outside the ABI taxonomy collapses
    /// to `invalid-response` — the host-only kinds are not a guest
    /// vocabulary.
    #[test]
    fn invalid_failure_kind_becomes_invalid_response() {
        reset();
        dispatch_register(bad_kind_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        reset();
    }

    fn bad_host_kind_dispatch(_inv: Invocation) -> GuestFuture {
        Box::pin(async move {
            Err(GuestError::Host {
                kind: "budget-exceeded".into(),
                message: "not my word".into(),
            })
        })
    }

    /// A guest-constructed `Host` error gets the same taxonomy clamp as
    /// `Failed` — a kind outside the guest vocabulary collapses to
    /// `invalid-response`.
    #[test]
    fn guest_host_kind_becomes_invalid_response() {
        reset();
        dispatch_register(bad_host_kind_dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        reset();
    }

    /// The ABI is sequential: polling a second host call while the
    /// first request is pending is a protocol violation.
    #[test]
    fn concurrent_host_calls_are_invalid_response() {
        fn dispatch(_inv: Invocation) -> GuestFuture {
            Box::pin(async move {
                let mut a = host_call("now_ms", json!({}));
                let mut b = host_call("now_ms", json!({}));
                let mut cx = Context::from_waker(Waker::noop());
                let _ = Pin::new(&mut a).poll(&mut cx);
                match Pin::new(&mut b).poll(&mut cx) {
                    Poll::Ready(Err(e)) => Err(e),
                    _ => Ok(json!("second call unexpectedly accepted")),
                }
            })
        }
        reset();
        dispatch_register(dispatch);
        let out = step_json(&json!({
            "type": "invoke", "request_id": "r", "capability": "x", "payload": {},
        }));
        assert_eq!(out["type"], "fail");
        assert_eq!(out["error"]["kind"], "invalid-response");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("concurrent host calls"),
            "{}",
            out["error"]["message"]
        );
        reset();
    }
}
