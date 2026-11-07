//! Last.fm web service calls: request signing, transport, error classes, and the four methods
//! Truffle uses (`auth.getToken`, `auth.getSession`, `track.updateNowPlaying`, `track.scrobble`).
//! See https://www.last.fm/api for the calls and their errors.
//!
//! Every call is a signed `POST` with a form body and `format=json`. The shared secret and the
//! session key only ever go into the body and the signature; they are never logged (the `Debug`
//! output of [`Credentials`] and [`Session`] hides them).

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::tracker::{PendingScrobble, ScrobbleTrack};

pub const API_ROOT: &str = "https://ws.audioscrobbler.com/2.0/";
pub const AUTH_PAGE: &str = "https://www.last.fm/api/auth/";
/// Last.fm takes at most 50 scrobbles per request.
pub const MAX_BATCH: usize = 50;

/// "Please use an identifiable User-Agent header on all requests" (last.fm/api/intro).
const USER_AGENT: &str = concat!("Truffle/", env!("CARGO_PKG_VERSION"), " (Windows; personal scrobbler)");
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The user's own API key and shared secret.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub api_key: String,
    pub secret: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { api_key: <set>, secret: <hidden> }")
    }
}

/// A Last.fm session: the user it belongs to and its key (no expiry; revoked by the user).
/// Stored as `lastfm_session.json`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub username: String,
    pub key: String,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("username", &self.username).field("key", &"<hidden>").finish()
    }
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LfmError {
    /// Last.fm answered `{"error": code, "message": ...}` (often with a non-200 status).
    Api { code: u32, message: String },
    /// Offline, DNS, TLS, timeout.
    Net(String),
    /// An error status with a body that isn't Last.fm's error JSON (for example a 502 page).
    Status(u16),
    /// A success status with a body that can't be understood.
    Parse(String),
}

impl LfmError {
    pub fn code(&self) -> Option<u32> {
        match self {
            LfmError::Api { code, .. } => Some(*code),
            _ => None,
        }
    }
}

impl fmt::Display for LfmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LfmError::Api { code, message } => write!(f, "Last.fm error {code}: {message}"),
            LfmError::Net(e) => write!(f, "network error: {e}"),
            LfmError::Status(status) => write!(f, "HTTP status {status}"),
            LfmError::Parse(e) => write!(f, "unexpected response: {e}"),
        }
    }
}

/// What to do about a failed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Temporary (11, 16, network, 5xx): keep everything and try again later.
    Retry,
    /// Rate limited (29): try again, but not for at least 5 minutes.
    RetrySlow,
    /// The session key is no longer valid (9): forget it; the user must connect again.
    Reauth,
    /// The key or secret is wrong or suspended (4, 10, 13, 26): stop and tell the user.
    Config,
    /// The request itself was bad (2, 3, 5, 6, 7, 8, ...): retrying it as is can't help.
    Drop,
}

pub fn classify(e: &LfmError) -> ErrorClass {
    match e {
        LfmError::Api { code: 9, .. } => ErrorClass::Reauth,
        LfmError::Api { code: 11 | 16, .. } => ErrorClass::Retry,
        LfmError::Api { code: 29, .. } => ErrorClass::RetrySlow,
        LfmError::Api { code: 4 | 10 | 13 | 26, .. } => ErrorClass::Config,
        LfmError::Api { .. } => ErrorClass::Drop,
        LfmError::Net(_) | LfmError::Status(_) | LfmError::Parse(_) => ErrorClass::Retry,
    }
}

/// Lowercase hex of `bytes` (digest 0.11 output has no `LowerHex`).
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// `api_sig`: every parameter except `format` and `callback`, sorted by name in byte order (a
/// `BTreeMap` of `String`s iterates exactly so, e.g. `artist[10]` before `artist[1]`), name and
/// raw value concatenated without separators, the shared secret appended, MD5 as lowercase hex.
pub fn sign(params: &BTreeMap<String, String>, secret: &str) -> String {
    let mut hasher = Md5::new();
    for (name, value) in params.iter().filter(|(name, _)| !matches!(name.as_str(), "format" | "callback" | "api_sig")) {
        hasher.update(name.as_bytes());
        hasher.update(value.as_bytes());
    }
    hasher.update(secret.as_bytes());
    hex(&hasher.finalize())
}

/// Last.fm JSON numbers arrive as numbers or as strings ("0").
pub fn num(v: &Value) -> Option<u32> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| v.as_str()?.trim().parse().ok())
}

/// An API key or shared secret: 32 hex characters.
pub fn is_valid_key(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The page where the user approves Truffle for request token `token`.
pub fn auth_url(api_key: &str, token: &str) -> String {
    match url::Url::parse_with_params(AUTH_PAGE, &[("api_key", api_key), ("token", token)]) {
        Ok(url) => url.into(),
        // AUTH_PAGE is a valid URL, so this can't happen; both values are hex anyway.
        Err(_) => format!("{AUTH_PAGE}?api_key={api_key}&token={token}"),
    }
}

/// `track.updateNowPlaying` parameters (without method/api_key/sk). Empty optional values are
/// left out entirely, since only what is sent may be signed.
pub fn now_playing_params(t: &ScrobbleTrack) -> BTreeMap<String, String> {
    let mut p = BTreeMap::new();
    p.insert("artist".to_owned(), t.artist.clone());
    p.insert("track".to_owned(), t.track.clone());
    if !t.album.is_empty() {
        p.insert("album".to_owned(), t.album.clone());
    }
    if t.duration_s > 0 {
        p.insert("duration".to_owned(), t.duration_s.to_string());
    }
    p
}

/// `track.scrobble` parameters for up to 50 items, always in indexed form (`artist[0]`, ...).
pub fn scrobble_params(batch: &[PendingScrobble]) -> BTreeMap<String, String> {
    let mut p = BTreeMap::new();
    for (i, s) in batch.iter().take(MAX_BATCH).enumerate() {
        p.insert(format!("artist[{i}]"), s.track.artist.clone());
        p.insert(format!("track[{i}]"), s.track.track.clone());
        p.insert(format!("timestamp[{i}]"), s.timestamp.to_string());
        if !s.track.album.is_empty() {
            p.insert(format!("album[{i}]"), s.track.album.clone());
        }
        if s.track.duration_s > 0 {
            p.insert(format!("duration[{i}]"), s.track.duration_s.to_string());
        }
    }
    p
}

/// What Last.fm said about one scrobble of a batch: `code` 0 = accepted, otherwise the
/// `ignoredMessage` code (1 artist ignored, 2 track ignored, 3 too old, 4 too new, 5 daily limit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemResult {
    pub code: u32,
    pub message: String,
}

/// Per-item results of a `track.scrobble` response, in request order. `scrobble` is an object
/// for one item and an array for several. An unexpected shape without an `error` counts as
/// accepted, so a batch Last.fm took is never sent twice.
pub fn item_results(v: &Value, n: usize) -> Vec<ItemResult> {
    let items: Vec<&Value> = match &v["scrobbles"]["scrobble"] {
        Value::Array(a) => a.iter().collect(),
        o @ Value::Object(_) => vec![o],
        _ => Vec::new(),
    };
    let mut results: Vec<ItemResult> = items
        .iter()
        .take(n)
        .map(|s| {
            let ignored = &s["ignoredMessage"];
            ItemResult {
                code: num(&ignored["code"]).unwrap_or(0),
                message: ignored["#text"].as_str().unwrap_or("").to_owned(),
            }
        })
        .collect();
    results.resize(n, ItemResult { code: 0, message: String::new() });
    results
}

/// The HTTP side: one client with Truffle's User-Agent and timeouts.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    root: String,
}

impl Client {
    pub fn new() -> Result<Self, String> {
        Self::with_root(API_ROOT)
    }

    /// A client for another API root (a local mock server in tests).
    pub fn with_root(root: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { http, root: root.to_owned() })
    }

    /// One signed call. `sk` is the session key for write methods.
    async fn call(
        &self,
        creds: &Credentials,
        method: &str,
        mut params: BTreeMap<String, String>,
        sk: Option<&str>,
    ) -> Result<Value, LfmError> {
        params.insert("method".to_owned(), method.to_owned());
        params.insert("api_key".to_owned(), creds.api_key.clone());
        if let Some(sk) = sk {
            params.insert("sk".to_owned(), sk.to_owned());
        }
        let sig = sign(&params, &creds.secret);
        params.insert("api_sig".to_owned(), sig);
        // Never signed.
        params.insert("format".to_owned(), "json".to_owned());
        // `without_url`: the URL holds nothing secret, but keep error text short.
        let resp = self
            .http
            .post(&self.root)
            .form(&params)
            .send()
            .await
            .map_err(|e| LfmError::Net(e.without_url().to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| LfmError::Net(e.without_url().to_string()))?;
        parse_response(status.as_u16(), &body)
    }

    /// `auth.getToken`: an unauthorised request token (valid 60 minutes, usable once).
    pub async fn get_token(&self, creds: &Credentials) -> Result<String, LfmError> {
        let v = self.call(creds, "auth.getToken", BTreeMap::new(), None).await?;
        match v["token"].as_str() {
            Some(token) if !token.is_empty() => Ok(token.to_owned()),
            _ => Err(LfmError::Parse("no token in the response".into())),
        }
    }

    /// `auth.getSession`: the session once the user approved `token` (error 14 until then).
    pub async fn get_session(&self, creds: &Credentials, token: &str) -> Result<Session, LfmError> {
        let params = BTreeMap::from([("token".to_owned(), token.to_owned())]);
        let v = self.call(creds, "auth.getSession", params, None).await?;
        let s = &v["session"];
        match (s["name"].as_str(), s["key"].as_str()) {
            (Some(name), Some(key)) if !key.is_empty() => Ok(Session { username: name.to_owned(), key: key.to_owned() }),
            _ => Err(LfmError::Parse("no session in the response".into())),
        }
    }

    /// `track.updateNowPlaying` (best effort; never retried).
    pub async fn now_playing(&self, creds: &Credentials, sk: &str, track: &ScrobbleTrack) -> Result<(), LfmError> {
        self.call(creds, "track.updateNowPlaying", now_playing_params(track), Some(sk)).await.map(|_| ())
    }

    /// `track.scrobble` for up to 50 items; per-item results in request order.
    pub async fn scrobble(&self, creds: &Credentials, sk: &str, batch: &[PendingScrobble]) -> Result<Vec<ItemResult>, LfmError> {
        let n = batch.len().min(MAX_BATCH);
        let v = self.call(creds, "track.scrobble", scrobble_params(batch), Some(sk)).await?;
        Ok(item_results(&v, n))
    }
}

/// Always read the body: errors come as `{"error": N, "message": ...}`, often with a non-200
/// status, and a 200 doesn't guarantee success.
fn parse_response(status: u16, body: &str) -> Result<Value, LfmError> {
    let ok = (200..300).contains(&status);
    match serde_json::from_str::<Value>(body) {
        Ok(v) => match v.get("error").and_then(num) {
            Some(code) => Err(LfmError::Api { code, message: v["message"].as_str().unwrap_or("").to_owned() }),
            None if ok => Ok(v),
            None => Err(LfmError::Status(status)),
        },
        Err(_) if !ok => Err(LfmError::Status(status)),
        Err(e) => Err(LfmError::Parse(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn pending(artist: &str, track: &str, album: &str, duration_s: u32, timestamp: i64) -> PendingScrobble {
        PendingScrobble {
            track: ScrobbleTrack {
                artist: artist.into(),
                track: track.into(),
                album: album.into(),
                duration_s,
                uri: String::new(),
            },
            timestamp,
            attempts: 0,
            user: String::new(),
        }
    }

    #[test]
    fn md5_hex_matches_the_reference() {
        let mut h = Md5::new();
        h.update(b"hello world");
        assert_eq!(hex(&h.finalize()), "5eb63bbbe01eeed093cb22bb8f5acdc3");
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn signature_matches_the_authspec_test_vector() {
        let params = map(&[("api_key", "xxxxxxxxxx"), ("method", "auth.getSession"), ("token", "yyyyyy")]);
        assert_eq!(sign(&params, "ilovecher"), "b87d61da3cda91a8b6746c4aef55d6f8");
    }

    #[test]
    fn format_callback_and_api_sig_are_not_signed() {
        let base = map(&[("api_key", "xxxxxxxxxx"), ("method", "auth.getSession"), ("token", "yyyyyy")]);
        let mut extra = base.clone();
        extra.insert("format".into(), "json".into());
        extra.insert("callback".into(), "cb".into());
        extra.insert("api_sig".into(), "whatever".into());
        assert_eq!(sign(&extra, "ilovecher"), sign(&base, "ilovecher"));
    }

    #[test]
    fn parameters_are_signed_in_byte_order() {
        // artist[10] sorts before artist[1] ('0' < ']'), uppercase before lowercase.
        let names: Vec<&str> = ["artist[1]", "artist[10]", "sk", "albumArtist[0]", "album[0]", "api_key", "artist[2]", "method"]
            .into_iter()
            .collect();
        let params: BTreeMap<String, String> = names.iter().map(|n| ((*n).to_owned(), String::new())).collect();
        let order: Vec<&str> = params.keys().map(String::as_str).collect();
        assert_eq!(
            order,
            ["albumArtist[0]", "album[0]", "api_key", "artist[10]", "artist[1]", "artist[2]", "method", "sk"]
        );
        // And the signature is built in exactly that order.
        let params = map(&[("artist[1]", "B"), ("artist[10]", "A")]);
        let mut h = Md5::new();
        h.update(b"artist[10]Aartist[1]Bsecret");
        assert_eq!(sign(&params, "secret"), hex(&h.finalize()));
    }

    #[test]
    fn values_are_signed_raw_as_utf8() {
        let params = map(&[("artist", "Björk & Co"), ("track", "a=b?c")]);
        let mut h = Md5::new();
        h.update("artistBjörk & Cotracka=b?cs".as_bytes());
        assert_eq!(sign(&params, "s"), hex(&h.finalize()));
    }

    #[test]
    fn scrobble_params_are_indexed_and_skip_empty_values() {
        let batch = vec![pending("A", "One", "Album", 200, 1_700_000_000), pending("B", "Two", "", 0, 1_700_000_300)];
        let p = scrobble_params(&batch);
        assert_eq!(p["artist[0]"], "A");
        assert_eq!(p["track[0]"], "One");
        assert_eq!(p["album[0]"], "Album");
        assert_eq!(p["duration[0]"], "200");
        assert_eq!(p["timestamp[0]"], "1700000000");
        assert_eq!(p["artist[1]"], "B");
        assert_eq!(p["timestamp[1]"], "1700000300");
        assert!(!p.contains_key("album[1]") && !p.contains_key("duration[1]"));
        assert_eq!(p.len(), 8);

        let many: Vec<PendingScrobble> = (0..60).map(|i| pending("A", "T", "", 0, i)).collect();
        assert!(scrobble_params(&many).contains_key("artist[49]"));
        assert!(!scrobble_params(&many).contains_key("artist[50]"));
    }

    #[test]
    fn now_playing_params_skip_empty_values() {
        let t = pending("Artist", "Song", "", 0, 0).track;
        let p = now_playing_params(&t);
        assert_eq!(p, map(&[("artist", "Artist"), ("track", "Song")]));
        let t = pending("Artist", "Song", "Album", 181, 0).track;
        assert_eq!(now_playing_params(&t)["duration"], "181");
        assert_eq!(now_playing_params(&t)["album"], "Album");
    }

    #[test]
    fn item_results_handle_object_array_and_odd_shapes() {
        let one: Value = serde_json::from_str(
            r##"{"scrobbles":{"@attr":{"accepted":1,"ignored":0},"scrobble":{"ignoredMessage":{"code":"0","#text":""}}}}"##,
        )
        .unwrap();
        assert_eq!(item_results(&one, 1), vec![ItemResult { code: 0, message: String::new() }]);

        let many: Value = serde_json::from_str(
            r##"{"scrobbles":{"scrobble":[{"ignoredMessage":{"code":"0","#text":""}},
                {"ignoredMessage":{"code":3,"#text":"Timestamp too old"}},{"ignoredMessage":{"code":"5"}}]}}"##,
        )
        .unwrap();
        let codes: Vec<u32> = item_results(&many, 3).iter().map(|r| r.code).collect();
        assert_eq!(codes, [0, 3, 5]);
        assert_eq!(item_results(&many, 3)[1].message, "Timestamp too old");

        // No per-item details: everything counts as accepted (never re-sent as duplicates).
        let odd: Value = serde_json::from_str(r#"{"scrobbles":{}}"#).unwrap();
        assert_eq!(item_results(&odd, 2).len(), 2);
        assert!(item_results(&odd, 2).iter().all(|r| r.code == 0));
    }

    #[test]
    fn responses_and_errors_are_classified() {
        let err = parse_response(400, r#"{"error":6,"message":"Invalid parameters"}"#).unwrap_err();
        assert_eq!(err, LfmError::Api { code: 6, message: "Invalid parameters".into() });
        assert_eq!(classify(&err), ErrorClass::Drop);
        // An error inside a 200, with the code as a string.
        let err = parse_response(200, r#"{"error":"9","message":"Invalid session key"}"#).unwrap_err();
        assert_eq!(classify(&err), ErrorClass::Reauth);
        assert_eq!(parse_response(502, "<html>Bad gateway</html>").unwrap_err(), LfmError::Status(502));
        assert!(matches!(parse_response(200, "<lfm/>").unwrap_err(), LfmError::Parse(_)));
        assert!(parse_response(200, r#"{"token":"abc"}"#).is_ok());

        let api = |code| LfmError::Api { code, message: String::new() };
        assert_eq!(classify(&api(11)), ErrorClass::Retry);
        assert_eq!(classify(&api(16)), ErrorClass::Retry);
        assert_eq!(classify(&api(29)), ErrorClass::RetrySlow);
        for code in [4, 10, 13, 26] {
            assert_eq!(classify(&api(code)), ErrorClass::Config, "code {code}");
        }
        for code in [2, 3, 5, 6, 7, 8, 99] {
            assert_eq!(classify(&api(code)), ErrorClass::Drop, "code {code}");
        }
        assert_eq!(classify(&LfmError::Net("offline".into())), ErrorClass::Retry);
        assert_eq!(classify(&LfmError::Status(503)), ErrorClass::Retry);
    }

    #[test]
    fn keys_and_auth_url() {
        assert!(is_valid_key("0123456789abcdef0123456789ABCDEF"));
        assert!(!is_valid_key("0123456789abcdef0123456789abcde"));
        assert!(!is_valid_key("0123456789abcdef0123456789abcdeg"));
        assert!(!is_valid_key(""));
        assert_eq!(auth_url("abc123", "tok"), "https://www.last.fm/api/auth/?api_key=abc123&token=tok");
    }

    #[test]
    fn secrets_never_show_in_debug_output() {
        let creds = Credentials { api_key: "k".repeat(32), secret: "s".repeat(32) };
        let session = Session { username: "someone".into(), key: "q".repeat(32) };
        let text = format!("{creds:?} {session:?}");
        assert!(!text.contains(&"s".repeat(32)) && !text.contains(&"q".repeat(32)) && !text.contains(&"k".repeat(32)));
        assert!(text.contains("someone"));
    }

    #[test]
    fn numbers_as_strings_or_numbers() {
        assert_eq!(num(&serde_json::json!(5)), Some(5));
        assert_eq!(num(&serde_json::json!("5")), Some(5));
        assert_eq!(num(&serde_json::json!(" 7 ")), Some(7));
        assert_eq!(num(&serde_json::json!(null)), None);
        assert_eq!(num(&serde_json::json!("x")), None);
    }
}
