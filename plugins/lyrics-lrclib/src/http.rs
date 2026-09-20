//! HTTPS fetch and status mapping for the LRCLIB API. Never logs
//! response bodies or full query URLs.

use auqw_guest_sdk::{http_request, log, GuestError, HttpRequest, HttpResponse, LogLevel};

/// What an upstream response means to the waterfall.
pub enum Outcome {
    /// A 2xx body.
    Body(Vec<u8>),
    /// A 404 — a tier miss; the waterfall advances.
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
/// `rate-limit` on 403/429 — a parsed `Retry-After` goes to the
/// redacted diagnostic log only, and the failure is terminal: the
/// guest fails closed, never retries. `transient` on every other
/// non-2xx; host and transport failures propagate with their
/// `host_error` kind.
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
        200..=299 => Ok(Outcome::Body(resp.body)),
        404 => Ok(Outcome::NotFound),
        403 | 429 => {
            match retry_after_secs(&resp) {
                Some(r) => {
                    log(
                        LogLevel::Warn,
                        &format!("lrclib rate-limited retry_after={r}"),
                    )
                    .await?;
                }
                None => log(LogLevel::Warn, "lrclib rate-limited").await?,
            }
            Err(failed(
                "rate-limit",
                format!("lrclib status {}", resp.status),
            ))
        }
        s => Err(failed("transient", format!("lrclib status {s}"))),
    }
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
