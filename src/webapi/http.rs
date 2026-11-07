//! HTTP layer for `api.spotify.com`: bearer auth with refresh, a global limit of two requests in
//! flight, a global pace of about three requests per second (the
//! Development Mode quota is per developer account and easily used up by paging), a shared
//! back-off gate for 429s, and retries for server errors.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use bytes::Bytes;
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use super::{ReconnectRequired, WebApi};

pub(super) const API_BASE: &str = "https://api.spotify.com/v1";

/// Never sleep longer than this for a 429; longer waits become an error for the user.
pub(super) const MAX_RATE_WAIT: Duration = Duration::from_secs(60);
/// Used when a 429 has no `Retry-After` header.
const DEFAULT_RETRY_AFTER_SECS: u64 = 5;
/// Assumed cool-down for a quota 429 without `Retry-After`.
const DEFAULT_QUOTA_BLOCK_SECS: u64 = 3600;
const MAX_SERVER_RETRIES: u32 = 2;
const MAX_RATE_RETRIES: u32 = 3;

/// At most two Web API requests in flight across the whole app.
static PERMITS: Semaphore = Semaphore::const_new(2);
/// While set and in the future, no request is sent (shared by every request after a 429).
static BLOCKED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
/// Sustained request rate: one request per interval (about 3 per second)...
#[cfg(not(test))]
const PACE_INTERVAL: Duration = Duration::from_millis(334);
#[cfg(test)]
const PACE_INTERVAL: Duration = Duration::from_millis(1);
/// ...after a burst of this many requests.
const PACE_BURST: u32 = 6;
static PACE: Mutex<Pacer> = Mutex::new(Pacer { next_free: None });

/// Spaces requests out (a token bucket in "virtual scheduling" form): each request reserves the
/// next slot of a schedule running at `interval`, and only waits when it is more than `burst`
/// slots ahead of now.
#[derive(Debug)]
pub(super) struct Pacer {
    /// When the schedule frees up.
    next_free: Option<Instant>,
}

impl Pacer {
    /// Reserve a slot and return how long to wait before sending.
    pub(super) fn reserve(&mut self, now: Instant, interval: Duration, burst: u32) -> Duration {
        let start = self.next_free.map_or(now, |t| t.max(now));
        let next = start + interval;
        self.next_free = Some(next);
        (next - now).saturating_sub(interval * burst)
    }
}

pub(super) enum ReqBody {
    Empty,
    Json(serde_json::Value),
    /// Base64 text of a JPEG, sent with `Content-Type: image/jpeg`.
    Jpeg(Bytes),
}

#[derive(Debug, Default, Deserialize)]
struct ApiErrorEnvelope {
    #[serde(default)]
    error: Option<ApiErrorDetail>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiErrorDetail {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// `(message, reason)` from a Web API error body, both possibly empty.
pub(super) fn parse_api_error(body: &[u8]) -> (String, String) {
    let detail = serde_json::from_slice::<ApiErrorEnvelope>(body)
        .ok()
        .and_then(|e| e.error)
        .unwrap_or_default();
    (detail.message.unwrap_or_default(), detail.reason.unwrap_or_default())
}

pub(super) fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    headers.get(RETRY_AFTER)?.to_str().ok()?.trim().parse::<u64>().ok()
}

/// Exponential back-off for retry `attempt` (1-based): 1 s, 2 s, 4 s ...
pub(super) fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5))
}

pub(super) fn network_error() -> anyhow::Error {
    anyhow!("Truffle couldn't reach Spotify. Check your internet connection and try again.")
}

fn network_error_for(what: &str) -> anyhow::Error {
    anyhow!("Couldn't {what}: Truffle couldn't reach Spotify. Check your internet connection and try again.")
}

/// "about 3 minutes", "about 2 hours", ...
pub(super) fn friendly_duration(d: Duration) -> String {
    let secs = d.as_secs().max(1);
    if secs < 90 {
        format!("about {secs} seconds")
    } else if secs < 90 * 60 {
        format!("about {} minutes", secs.div_ceil(60))
    } else {
        let hours = secs.div_ceil(3600);
        if hours == 1 { "about an hour".to_owned() } else { format!("about {hours} hours") }
    }
}

fn block_for(wait: Duration) {
    let until = Instant::now() + wait;
    let mut blocked = BLOCKED_UNTIL.lock().unwrap_or_else(|p| p.into_inner());
    if blocked.is_none_or(|b| b < until) {
        *blocked = Some(until);
    }
}

/// How long to wait before sending, or an error when Spotify asked for a long pause.
fn gate_wait() -> Result<Option<Duration>> {
    let mut blocked = BLOCKED_UNTIL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(until) = *blocked else { return Ok(None) };
    let now = Instant::now();
    if until <= now {
        *blocked = None;
        return Ok(None);
    }
    let remaining = until - now;
    if remaining > MAX_RATE_WAIT {
        return Err(anyhow!(
            "Spotify has paused requests from your developer app for a while. Editing features will work \
             again in {}.",
            friendly_duration(remaining)
        ));
    }
    Ok(Some(remaining))
}

fn jitter() -> Duration {
    Duration::from_millis(rand::random_range(0..500u64))
}

/// Plain-language error for a final, non-success response.
pub(super) fn describe_failure(status: StatusCode, body: &[u8], what: &str) -> anyhow::Error {
    let (message, _) = parse_api_error(body);
    let lower = message.to_ascii_lowercase();
    let reason = match status.as_u16() {
        401 => "Spotify no longer accepts Truffle's sign-in. Open Settings and click Connect to sign in again."
            .to_owned(),
        403 if lower.contains("not registered") || lower.contains("may not be registered") => {
            "your Spotify account isn't on your developer app's user list. On developer.spotify.com open \
             your app, go to Settings > User Management and add the email address of your Spotify account."
                .to_owned()
        }
        403 if lower.contains("scope") => {
            "Truffle's sign-in is missing a permission. Open Settings, disconnect playlist editing and click \
             Connect to sign in again."
                .to_owned()
        }
        403 => "Spotify refused. You can only change playlists you own or collaborate on, and the owner of \
                your developer app needs an active Premium subscription."
            .to_owned(),
        404 => "Spotify couldn't find it. It may have been deleted.".to_owned(),
        413 => "the data was too large for Spotify.".to_owned(),
        400 | 422 if !message.is_empty() => format!("Spotify rejected the request ({message})."),
        s if s >= 500 => {
            "Spotify's servers are having trouble right now. Please try again in a few minutes.".to_owned()
        }
        _ if !message.is_empty() => format!("Spotify answered \"{message}\" ({status})."),
        _ => format!("Spotify answered with an error ({status})."),
    };
    anyhow!("Couldn't {what}: {reason}")
}

impl WebApi {
    /// Send one API request with auth, the in-flight limit and the retry policy. Returns the
    /// response body of a 2xx response.
    pub(super) async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: &ReqBody,
        what: &str,
    ) -> Result<Bytes> {
        let url = format!("{}{path}", self.inner.api_base);
        let mut auth_retried = false;
        let mut server_retries = 0u32;
        let mut rate_retries = 0u32;
        loop {
            while let Some(wait) = gate_wait()? {
                tokio::time::sleep(wait).await;
            }
            let pace = PACE.lock().unwrap_or_else(|p| p.into_inner()).reserve(Instant::now(), PACE_INTERVAL, PACE_BURST);
            if !pace.is_zero() {
                tokio::time::sleep(pace).await;
            }
            let token = self.access_token().await?;
            let permit = PERMITS
                .acquire()
                .await
                .map_err(|_| anyhow!("Couldn't {what}: Truffle is shutting down."))?;

            let mut request = self.inner.http.request(method.clone(), &url).bearer_auth(&token);
            if !query.is_empty() {
                request = request.query(query);
            }
            request = match body {
                // Spotify answers 411 to a body-less PUT/DELETE without Content-Length.
                ReqBody::Empty if method != Method::GET => request.header(CONTENT_LENGTH, "0"),
                ReqBody::Empty => request,
                ReqBody::Json(value) => request.json(value),
                ReqBody::Jpeg(data) => request.header(CONTENT_TYPE, "image/jpeg").body(data.clone()),
            };

            let sent = request.send().await;
            let response = match sent {
                Ok(response) => response,
                Err(e) => {
                    drop(permit);
                    log::warn!("{method} {path} failed: {e}");
                    if server_retries < MAX_SERVER_RETRIES {
                        server_retries += 1;
                        tokio::time::sleep(backoff(server_retries)).await;
                        continue;
                    }
                    return Err(network_error_for(what));
                }
            };
            let status = response.status();
            let retry_after = retry_after_secs(response.headers());
            let bytes = response.bytes().await;
            drop(permit);
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(e) => {
                    log::warn!("{method} {path}: reading the response failed: {e}");
                    if server_retries < MAX_SERVER_RETRIES {
                        server_retries += 1;
                        tokio::time::sleep(backoff(server_retries)).await;
                        continue;
                    }
                    return Err(network_error_for(what));
                }
            };

            if status.is_success() {
                return Ok(bytes);
            }

            match status.as_u16() {
                401 if !auth_retried => {
                    log::info!("{method} {path}: 401, refreshing the access token");
                    auth_retried = true;
                    self.refresh_after_rejection(&token).await?;
                }
                401 => {
                    // Rejected again with a freshly refreshed token: the sign-in itself is no good.
                    log::warn!("{method} {path}: 401 after a token refresh");
                    return Err(ReconnectRequired::new(&format!(
                        "Couldn't {what}: Spotify no longer accepts Truffle's sign-in for playlist editing. \
                         Open Settings and click Connect to sign in again."
                    ))
                    .into());
                }
                429 => {
                    let (message, reason) = parse_api_error(&bytes);
                    log::warn!(
                        "{method} {path}: 429 (Retry-After {retry_after:?}, reason {reason:?}, message {message:?})"
                    );
                    if reason.eq_ignore_ascii_case("QUOTA_EXCEEDED") {
                        let wait = Duration::from_secs(retry_after.unwrap_or(DEFAULT_QUOTA_BLOCK_SECS));
                        block_for(wait);
                        return Err(anyhow!(
                            "Couldn't {what}: your Spotify developer app has used up its request quota. \
                             Editing features will work again in {}.",
                            friendly_duration(wait)
                        ));
                    }
                    let wait = Duration::from_secs(retry_after.unwrap_or(DEFAULT_RETRY_AFTER_SECS));
                    block_for(wait);
                    if wait > MAX_RATE_WAIT {
                        return Err(anyhow!(
                            "Couldn't {what}: Spotify asked Truffle to slow down. Please try again in {}.",
                            friendly_duration(wait)
                        ));
                    }
                    if rate_retries >= MAX_RATE_RETRIES {
                        return Err(anyhow!(
                            "Couldn't {what}: Spotify is limiting requests right now. Please try again in a minute."
                        ));
                    }
                    rate_retries += 1;
                    tokio::time::sleep(jitter()).await;
                }
                500..=599 if server_retries < MAX_SERVER_RETRIES => {
                    server_retries += 1;
                    log::warn!("{method} {path}: {status}, retry {server_retries}");
                    tokio::time::sleep(backoff(server_retries)).await;
                }
                _ => {
                    log::warn!(
                        "{method} {path}: {status}: {}",
                        String::from_utf8_lossy(&bytes[..bytes.len().min(300)])
                    );
                    return Err(describe_failure(status, &bytes, what));
                }
            }
        }
    }

    /// GET a JSON document.
    pub(super) async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        what: &str,
    ) -> Result<T> {
        let bytes = self.call(Method::GET, path, query, &ReqBody::Empty, what).await?;
        serde_json::from_slice(&bytes).map_err(|e| {
            log::warn!("GET {path}: unexpected JSON: {e}");
            anyhow!("Couldn't {what}: Spotify sent data Truffle didn't understand.")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn parses_quota_error_body() {
        let body = br#"{"error":{"status":429,"message":"Too many requests","reason":"QUOTA_EXCEEDED"}}"#;
        assert_eq!(parse_api_error(body), ("Too many requests".to_owned(), "QUOTA_EXCEEDED".to_owned()));
        assert_eq!(parse_api_error(b"not json"), (String::new(), String::new()));
        assert_eq!(parse_api_error(br#"{"error":{"status":404}}"#), (String::new(), String::new()));
    }

    #[test]
    fn retry_after_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after_secs(&headers), None);
        headers.insert(RETRY_AFTER, HeaderValue::from_static("17"));
        assert_eq!(retry_after_secs(&headers), Some(17));
        headers.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert_eq!(retry_after_secs(&headers), None);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(friendly_duration(Duration::from_secs(30)), "about 30 seconds");
        assert_eq!(friendly_duration(Duration::from_secs(300)), "about 5 minutes");
        assert_eq!(friendly_duration(Duration::from_secs(3600)), "about 60 minutes");
        assert_eq!(friendly_duration(Duration::from_secs(5400)), "about 2 hours");
        assert_eq!(friendly_duration(Duration::from_secs(50_000)), "about 14 hours");
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
    }

    #[test]
    fn requests_are_paced_after_a_burst() {
        let interval = Duration::from_millis(100);
        let mut pacer = Pacer { next_free: None };
        let now = Instant::now();
        let waits: Vec<Duration> = (0..8).map(|_| pacer.reserve(now, interval, 3)).collect();
        assert_eq!(&waits[..3], &[Duration::ZERO; 3], "a burst goes out at once");
        assert_eq!(waits[3], interval);
        assert_eq!(waits[7], interval * 5);
        // After a quiet spell the burst is available again.
        let later = now + Duration::from_secs(10);
        assert_eq!(pacer.reserve(later, interval, 3), Duration::ZERO);
    }

    #[test]
    fn failures_are_plain_language() {
        let e = describe_failure(
            StatusCode::FORBIDDEN,
            br#"{"error":{"status":403,"message":"User not registered in the Developer Dashboard"}}"#,
            "rename the playlist",
        );
        let text = e.to_string();
        assert!(text.starts_with("Couldn't rename the playlist:"));
        assert!(text.contains("User Management"));
        let e = describe_failure(StatusCode::NOT_FOUND, b"", "load the playlist");
        assert!(e.to_string().contains("couldn't find"));
    }
}
