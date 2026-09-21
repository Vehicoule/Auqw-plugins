//! HTTPS fetch and status/envelope mapping for the Deezer API. Never
//! logs response bodies or full query URLs.
//!
//! Deezer answers many upstream failures as `200` with an error
//! envelope (`{"error":{"type":..,"message":..,"code":..}}`), so a 2xx
//! body is classified before it reaches the capability layer.

use auqw_guest_sdk::{http_request, log, GuestError, HttpRequest, HttpResponse, LogLevel};
use serde_json::Value;

/// What an upstream response means to the capability layer.
pub enum Outcome {
    /// A 2xx JSON body that is not an error envelope.
    Body(Value),
    /// A 404, or a `DataException`/`code:800` envelope — the resource
    /// does not exist upstream. Callers decide whether that is an
    /// empty result or an error.
    NotFound,
}

fn failed(kind: &str, message: String) -> GuestError {
    GuestError::Failed {
        kind: kind.into(),
        message,
    }
}

/// GET `url` through the host with bounded, non-identifying headers.
///
/// # Errors
/// `rate-limit` on 403/429 or a quota error envelope (a parsed
/// `Retry-After` goes to the redacted diagnostic log and rides the
/// fail message — the only channel back to the app),
/// `transient` on every other non-2xx and on unrecognized error
/// envelopes, `invalid-response` on a non-JSON 2xx body; host and
/// transport failures propagate with their `host_error` kind.
pub async fn get_json(url: &str) -> Result<Outcome, GuestError> {
    let resp = http_request(HttpRequest {
        method: "GET".into(),
        url: url.into(),
        headers: vec![
            ("User-Agent".into(), "Auqw/0.1".into()),
            ("Accept".into(), "application/json".into()),
        ],
        body: None,
    })
    .await?;
    match resp.status {
        200..=299 => {
            let body: Value = serde_json::from_slice(&resp.body)
                .map_err(|_| failed("invalid-response", "deezer: body is not JSON".into()))?;
            match envelope_kind(&body) {
                Envelope::Data => Ok(Outcome::Body(body)),
                Envelope::NotFound => Ok(Outcome::NotFound),
                Envelope::Quota => Err(rate_limited().await?),
                Envelope::Other(code) => Err(failed(
                    "transient",
                    match code {
                        Some(c) => format!("deezer api error code {c}"),
                        None => "deezer api error".into(),
                    },
                )),
            }
        }
        404 => Ok(Outcome::NotFound),
        403 | 429 => Err(rate_limited_with_hint(&resp).await?),
        s => Err(failed("transient", format!("deezer status {s}"))),
    }
}

/// Classification of a parsed 2xx body.
enum Envelope {
    /// A normal payload (no `error` object).
    Data,
    /// `DataException` or `code:800` — the resource does not exist.
    NotFound,
    /// `QuotaException` or `code:4` — upstream rate limit.
    Quota,
    /// Any other error envelope; carries `error.code` when numeric.
    Other(Option<u64>),
}

/// Inspect a 2xx body's `error` member. Only an `error` object with a
/// string `message` counts as a refusal — a catalog object that merely
/// carries an `error` key of another shape is data, not an error.
fn envelope_kind(body: &Value) -> Envelope {
    let Some(error) = body
        .as_object()
        .and_then(|o| o.get("error"))
        .and_then(Value::as_object)
    else {
        return Envelope::Data;
    };
    if !error.get("message").is_some_and(Value::is_string) {
        return Envelope::Data;
    }
    let code = error.get("code").and_then(Value::as_u64);
    let kind = error.get("type").and_then(Value::as_str);
    if code == Some(800) || kind == Some("DataException") {
        Envelope::NotFound
    } else if code == Some(4) || kind == Some("QuotaException") {
        Envelope::Quota
    } else {
        Envelope::Other(code)
    }
}

/// Emit the sanitized rate-limit diagnostic, then produce the typed
/// error. The message never carries upstream text, bodies, or URLs.
async fn rate_limited() -> Result<GuestError, GuestError> {
    log(LogLevel::Warn, "deezer rate-limited").await?;
    Ok(failed("rate-limit", "deezer api quota exceeded".into()))
}

/// Rate-limit the same way, but mine `Retry-After` for the log and
/// the fail message first — the message is the only channel back to
/// the app, so a parsed hint rides it.
async fn rate_limited_with_hint(resp: &HttpResponse) -> Result<GuestError, GuestError> {
    let hint = retry_after_secs(resp);
    match hint {
        Some(r) => {
            log(
                LogLevel::Warn,
                &format!("deezer rate-limited retry_after={r}"),
            )
            .await?;
        }
        None => log(LogLevel::Warn, "deezer rate-limited").await?,
    }
    Ok(failed(
        "rate-limit",
        match hint {
            Some(r) => format!("deezer status {} retry_after={r}", resp.status),
            None => format!("deezer status {}", resp.status),
        },
    ))
}

/// A `Retry-After` hint only when it is a delta-seconds value of 1..10
/// ASCII digits — malformed, oversized, or control-containing values
/// are ignored. A raw response header never reaches the guest log.
fn retry_after_secs(resp: &HttpResponse) -> Option<&str> {
    let v = header(resp, "retry-after")?.trim();
    if (1..=10).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_digit()) {
        Some(v)
    } else {
        None
    }
}

fn header<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
    resp.headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}
