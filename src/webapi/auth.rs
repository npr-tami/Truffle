//! PKCE sign-in (system browser + loopback listener), token storage and token refresh.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use super::{REDIRECT_URI, ReconnectRequired, WebApi, http};

pub(super) const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
pub(super) const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

/// Scopes Truffle asks for.
pub const SCOPES: &[&str] = &[
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-public",
    "playlist-modify-private",
    "ugc-image-upload",
    "user-library-read",
    "user-library-modify",
    "user-read-private",
];

const CALLBACK_PORT: u16 = 8899;
const CALLBACK_PATH: &str = "/callback";
/// How long the browser sign-in may take before Truffle gives up.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long the browser tab waits for Truffle to say how the sign-in ended.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// Deadline for exchanging the code for tokens, kept below `REPLY_TIMEOUT` so the browser shows
/// the real outcome (each token request may take up to 30 s and is retried).
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(45);
/// Refresh the access token this long before it actually expires.
pub(super) const REFRESH_MARGIN_SECS: u64 = 5 * 60;
/// Used when Spotify omits `expires_in` (it is normally 3600).
const DEFAULT_EXPIRES_IN_SECS: u64 = 3600;
/// Spotify refresh tokens die 6 months after the original browser sign-in.
pub(super) const REFRESH_TOKEN_LIFETIME_SECS: u64 = 182 * 24 * 3600;
/// Largest HTTP request head accepted on the loopback listener.
const MAX_REQUEST_HEAD: usize = 16 * 1024;

pub(super) fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Stored token
// ---------------------------------------------------------------------------------------------

/// Contents of `webapi_token.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredToken {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds at which `access_token` expires.
    pub expires_at: u64,
    /// Unix seconds of the original browser sign-in (refresh tokens expire ~6 months later).
    #[serde(default)]
    pub authorized_at: u64,
    /// Space-separated scopes Spotify granted; empty when unknown.
    #[serde(default)]
    pub scope: String,
}

impl StoredToken {
    /// Build a token from the response to the authorization-code exchange.
    pub fn from_exchange(client_id: &str, resp: TokenResponse, now: u64) -> Result<Self> {
        let refresh_token = resp.refresh_token.unwrap_or_default();
        if refresh_token.is_empty() {
            bail!("Spotify didn't return a lasting sign-in. Please click Connect and try again.");
        }
        Ok(Self {
            client_id: client_id.to_owned(),
            access_token: resp.access_token,
            refresh_token,
            expires_at: now.saturating_add(effective_expires_in(resp.expires_in)),
            authorized_at: now,
            scope: resp.scope.unwrap_or_default(),
        })
    }

    /// True when the access token is expired or expires within the refresh margin.
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.access_token.is_empty() || self.expires_at <= now.saturating_add(REFRESH_MARGIN_SECS)
    }

    /// Apply a refresh response. Keeps the old refresh token when Spotify doesn't send a new one.
    pub fn apply_refresh(&mut self, resp: TokenResponse, now: u64) {
        self.access_token = resp.access_token;
        self.expires_at = now.saturating_add(effective_expires_in(resp.expires_in));
        if let Some(rt) = resp.refresh_token.filter(|rt| !rt.is_empty()) {
            self.refresh_token = rt;
        }
        if let Some(scope) = resp.scope.filter(|s| !s.trim().is_empty()) {
            self.scope = scope;
        }
    }

    /// Whether every required scope was granted. An unknown (empty) scope list counts as yes.
    pub fn has_scopes(&self, required: &[&str]) -> bool {
        if self.scope.trim().is_empty() {
            return true;
        }
        let granted: HashSet<&str> = self.scope.split_whitespace().collect();
        required.iter().all(|s| granted.contains(s))
    }
}

fn effective_expires_in(expires_in: Option<u64>) -> u64 {
    match expires_in {
        Some(secs) if secs > 0 => secs,
        _ => DEFAULT_EXPIRES_IN_SECS,
    }
}

pub(super) fn load_token(path: &Path) -> Option<StoredToken> {
    crate::config::read_json(path)
}

pub(super) fn save_token(path: &Path, token: &StoredToken) -> Result<()> {
    crate::config::write_json(path, token)
        .context("Truffle couldn't save the Spotify Web API sign-in to disk")
}

pub(super) fn delete_token(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            log::warn!("could not delete {}: {e}", path.display());
            Err(anyhow!("Truffle couldn't remove the saved Spotify Web API sign-in ({e})."))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// PKCE + authorize URL
// ---------------------------------------------------------------------------------------------

/// `BASE64URL_NOPAD(SHA256(verifier))` (RFC 7636, S256).
pub(super) fn pkce_challenge(verifier: &str) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()).as_slice())
}

/// Returns `(code_verifier, code_challenge)`. 48 random bytes -> 64 URL-safe characters.
pub(super) fn new_pkce_pair() -> (String, String) {
    let mut raw = [0u8; 48];
    rand::fill(&mut raw[..]);
    let verifier = BASE64_URL_SAFE_NO_PAD.encode(raw);
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

/// Random CSRF `state` value (128 bits, URL-safe).
pub(super) fn random_state() -> String {
    let mut raw = [0u8; 16];
    rand::fill(&mut raw[..]);
    BASE64_URL_SAFE_NO_PAD.encode(raw)
}

pub(super) fn authorize_url(client_id: &str, code_challenge: &str, state: &str) -> String {
    let scope = SCOPES.join(" ");
    let params = [
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("code_challenge_method", "S256"),
        ("code_challenge", code_challenge),
        ("state", state),
        ("scope", scope.as_str()),
        // Always show Spotify's consent page (with its "Not you?" link) instead of silently
        // approving whichever account the browser happens to be signed in to.
        ("show_dialog", "true"),
    ];
    match url::Url::parse_with_params(AUTHORIZE_URL, params) {
        Ok(url) => url.into(),
        // AUTHORIZE_URL is a valid constant, so this cannot fail; keep a sane fallback anyway.
        Err(_) => {
            let query: String = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(params)
                .finish();
            format!("{AUTHORIZE_URL}?{query}")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug)]
pub(super) enum TokenError {
    /// Code or refresh token expired/revoked (`invalid_grant`).
    InvalidGrant(String),
    /// Spotify doesn't know this client id (`invalid_client`).
    InvalidClient(String),
    Other(anyhow::Error),
}

const INVALID_CLIENT_MESSAGE: &str = "Spotify didn't accept this Client ID. Copy it again from your app's \
     Settings page on developer.spotify.com (it is 32 letters and numbers) and click Connect.";

/// POST a form to the token endpoint, retrying network errors and server errors twice.
pub(super) async fn request_token(
    client: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, TokenError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let can_retry = attempt <= 2;
        let resp = match client.post(token_url).form(form).send().await {
            Ok(resp) => resp,
            Err(e) => {
                log::warn!("token request failed (attempt {attempt}): {e}");
                if can_retry {
                    tokio::time::sleep(http::backoff(attempt)).await;
                    continue;
                }
                return Err(TokenError::Other(http::network_error()));
            }
        };
        let status = resp.status();
        let retry_after = http::retry_after_secs(resp.headers());
        let body = match resp.bytes().await {
            Ok(body) => body,
            Err(e) => {
                log::warn!("token response read failed (attempt {attempt}): {e}");
                if can_retry {
                    tokio::time::sleep(http::backoff(attempt)).await;
                    continue;
                }
                return Err(TokenError::Other(http::network_error()));
            }
        };

        if status.is_success() {
            return match serde_json::from_slice::<TokenResponse>(&body) {
                Ok(t) if !t.access_token.is_empty() => Ok(t),
                Ok(_) => Err(TokenError::Other(anyhow!(
                    "Spotify sent an empty sign-in response. Please try again."
                ))),
                Err(e) => {
                    log::warn!("unparseable token response: {e}");
                    Err(TokenError::Other(anyhow!(
                        "Spotify sent an unexpected sign-in response. Please try again."
                    )))
                }
            };
        }
        if status.is_server_error() && can_retry {
            log::warn!("token endpoint returned {status} (attempt {attempt}), retrying");
            tokio::time::sleep(http::backoff(attempt)).await;
            continue;
        }
        if status.as_u16() == 429 {
            let wait = retry_after.unwrap_or(5);
            if can_retry && wait <= http::MAX_RATE_WAIT.as_secs() {
                log::warn!("token endpoint rate limited, waiting {wait} s");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            return Err(TokenError::Other(anyhow!(
                "Spotify's sign-in service asked Truffle to slow down. Please try again in {}.",
                http::friendly_duration(Duration::from_secs(wait))
            )));
        }
        if status.is_server_error() {
            return Err(TokenError::Other(anyhow!(
                "Spotify's sign-in service is having trouble right now. Please try again in a few minutes."
            )));
        }

        let parsed: Option<OAuthErrorBody> = serde_json::from_slice(&body).ok();
        let (code, description) = match parsed {
            Some(p) => (p.error, p.error_description.unwrap_or_default()),
            None => (String::new(), String::from_utf8_lossy(&body).chars().take(200).collect()),
        };
        log::warn!("token endpoint returned {status}: {code} {description}");
        return Err(match code.as_str() {
            "invalid_grant" => TokenError::InvalidGrant(description),
            "invalid_client" | "unauthorized_client" => TokenError::InvalidClient(description),
            _ => {
                let detail = if description.is_empty() { status.to_string() } else { description };
                TokenError::Other(anyhow!(
                    "Spotify's sign-in service refused the request ({detail}). Please try connecting again."
                ))
            }
        });
    }
}

async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    code: &str,
    verifier: &str,
) -> Result<StoredToken> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    let now = now_unix();
    match request_token(client, TOKEN_URL, &form).await {
        Ok(resp) => StoredToken::from_exchange(client_id, resp, now),
        Err(TokenError::InvalidGrant(desc)) => {
            if desc.to_ascii_lowercase().contains("redirect") {
                bail!(
                    "Spotify rejected the sign-in because the Redirect URI doesn't match. In your app's \
                     Settings on developer.spotify.com, add exactly {REDIRECT_URI} and try again."
                )
            }
            bail!("The sign-in code from Spotify had already expired. Please click Connect again.")
        }
        Err(TokenError::InvalidClient(_)) => bail!(INVALID_CLIENT_MESSAGE),
        Err(TokenError::Other(e)) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------------
// Access token management for API calls
// ---------------------------------------------------------------------------------------------

impl WebApi {
    /// A valid access token, refreshing it first when it is about to expire.
    pub(super) async fn access_token(&self) -> Result<String> {
        self.ensure_not_revoked()?;
        let mut token = self.inner.token.lock().await;
        if token.needs_refresh(now_unix()) {
            self.refresh_locked(&mut token).await?;
        }
        Ok(token.access_token.clone())
    }

    /// Called after a 401: refresh unless another request already replaced `rejected`.
    pub(super) async fn refresh_after_rejection(&self, rejected: &str) -> Result<()> {
        self.ensure_not_revoked()?;
        let mut token = self.inner.token.lock().await;
        if token.access_token == rejected {
            self.refresh_locked(&mut token).await?;
        }
        Ok(())
    }

    fn ensure_not_revoked(&self) -> Result<()> {
        if self.inner.revoked.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ReconnectRequired::expired().into());
        }
        Ok(())
    }

    async fn refresh_locked(&self, token: &mut StoredToken) -> Result<()> {
        if token.refresh_token.is_empty() {
            return Err(self.revoke(token, ReconnectRequired::expired()));
        }
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", token.refresh_token.as_str()),
            ("client_id", token.client_id.as_str()),
        ];
        match request_token(&self.inner.http, &self.inner.token_url, &form).await {
            Ok(resp) => {
                let previous_refresh_token = token.refresh_token.clone();
                token.apply_refresh(resp, now_unix());
                self.persist_refreshed(&previous_refresh_token, token);
                log::debug!("Web API access token refreshed");
                Ok(())
            }
            Err(TokenError::InvalidGrant(desc)) => {
                log::warn!("Web API refresh token rejected (invalid_grant: {desc})");
                Err(self.revoke(token, ReconnectRequired::expired()))
            }
            Err(TokenError::InvalidClient(desc)) => {
                log::warn!("Web API client id rejected during refresh (invalid_client: {desc})");
                Err(self.revoke(token, ReconnectRequired::new(INVALID_CLIENT_MESSAGE)))
            }
            Err(TokenError::Other(e)) => Err(e),
        }
    }

    /// Whether the token file on disk still holds this connection's sign-in. After a Disconnect
    /// (file deleted) or a newer sign-in (file replaced), an old `WebApi` must not touch it.
    fn file_is_ours(&self, refresh_token: &str, client_id: &str) -> bool {
        load_token(&self.inner.token_path)
            .is_some_and(|on_disk| on_disk.refresh_token == refresh_token && on_disk.client_id == client_id)
    }

    fn persist_refreshed(&self, previous_refresh_token: &str, token: &StoredToken) {
        if !self.file_is_ours(previous_refresh_token, &token.client_id) {
            log::info!("Web API token file was removed or replaced; not saving the refreshed token");
            return;
        }
        if let Err(e) = save_token(&self.inner.token_path, token) {
            // The new token still works in memory; it is just not persisted.
            log::warn!("{e:#}");
        }
    }

    /// Mark the sign-in dead, delete its token file (if still ours) and return the error to report.
    fn revoke(&self, token: &StoredToken, err: ReconnectRequired) -> anyhow::Error {
        self.inner.revoked.store(true, std::sync::atomic::Ordering::Release);
        if self.file_is_ours(&token.refresh_token, &token.client_id)
            && let Err(e) = delete_token(&self.inner.token_path)
        {
            log::warn!("{e:#}");
        }
        err.into()
    }
}

// ---------------------------------------------------------------------------------------------
// Interactive browser sign-in
// ---------------------------------------------------------------------------------------------

/// The one sign-in attempt that may own the loopback port. A newer attempt cancels the older one.
static PENDING_LOGIN: Mutex<Option<(u64, oneshot::Sender<()>)>> = Mutex::new(None);
static LOGIN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Clears this attempt's slot in `PENDING_LOGIN` when the attempt ends (or its future is dropped).
struct LoginRegistration {
    id: u64,
}

impl LoginRegistration {
    fn register() -> (Self, oneshot::Receiver<()>) {
        let id = LOGIN_SEQ.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let previous = {
            let mut slot = PENDING_LOGIN.lock().unwrap_or_else(|p| p.into_inner());
            slot.replace((id, tx))
        };
        if let Some((_, old)) = previous {
            log::info!("cancelling an older Web API sign-in attempt");
            let _ = old.send(());
        }
        (Self { id }, rx)
    }
}

impl Drop for LoginRegistration {
    fn drop(&mut self) {
        let mut slot = PENDING_LOGIN.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_some_and(|(id, _)| *id == self.id) {
            slot.take();
        }
    }
}

/// Stop a browser sign-in that is waiting for the user. Returns true if one was pending.
pub fn cancel_pending_login() -> bool {
    let pending = PENDING_LOGIN.lock().unwrap_or_else(|p| p.into_inner()).take();
    match pending {
        Some((_, tx)) => {
            let _ = tx.send(());
            true
        }
        None => false,
    }
}

/// Run the full PKCE flow: open the browser, wait for the redirect, exchange the code.
pub(super) async fn interactive_login(client: &reqwest::Client, client_id: &str) -> Result<StoredToken> {
    let (_registration, cancelled) = LoginRegistration::register();
    let listener = bind_callback_listener().await?;

    let (verifier, challenge) = new_pkce_pair();
    let state = random_state();
    let url = authorize_url(client_id, &challenge, &state);
    log::info!("opening the browser for Web API sign-in");
    open_browser(&url).await?;

    let callback = wait_for_callback(listener, &state, cancelled).await?;
    match callback.result {
        CallbackResult::Code(code) => {
            let result = match tokio::time::timeout(EXCHANGE_TIMEOUT, exchange_code(client, client_id, &code, &verifier)).await {
                Ok(result) => result,
                Err(_) => Err(anyhow!(
                    "Spotify's sign-in service didn't answer in time. Check your internet connection and click \
                     Connect to try again."
                )),
            };
            let page = match &result {
                Ok(_) => Page::success(
                    "Truffle is connected",
                    "Playlist editing is now switched on. You can close this tab and go back to Truffle.",
                ),
                Err(e) => Page::failure("Sign-in didn't finish", &e.to_string()),
            };
            let _ = callback.reply.send(page);
            result
        }
        CallbackResult::Denied(error) => {
            let message = if error == "access_denied" {
                "You chose not to give Truffle access, so playlist editing stays off. You can connect \
                 again any time from Truffle's settings."
                    .to_owned()
            } else {
                format!("Spotify reported a sign-in problem ({error}). Please go back to Truffle and try again.")
            };
            let _ = callback.reply.send(Page::failure("Sign-in cancelled", &message));
            Err(anyhow!(message))
        }
    }
}

async fn bind_callback_listener() -> Result<TcpListener> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, CALLBACK_PORT));
    // An older, just-cancelled attempt may still be releasing the port.
    let give_up_at = Instant::now() + Duration::from_secs(3);
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < give_up_at => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                log::error!("cannot bind {addr}: {e}");
                bail!(
                    "Truffle couldn't start its sign-in helper on port {CALLBACK_PORT} ({e}). Another program, \
                     or another copy of Truffle, may be using it. Close it and try again."
                );
            }
        }
    }
}

async fn open_browser(url: &str) -> Result<()> {
    log::debug!("authorize URL: {url}");
    let target = url.to_owned();
    let outcome = tokio::task::spawn_blocking(move || open::that_detached(target)).await;
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            log::error!("could not open the browser: {e}");
            bail!(
                "Truffle couldn't open your web browser for the Spotify sign-in. Make sure Windows has a \
                 default browser set, then try again."
            )
        }
        Err(e) => {
            log::error!("browser launcher task failed: {e}");
            bail!("Truffle couldn't open your web browser for the Spotify sign-in. Please try again.")
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CallbackResult {
    Code(String),
    /// Value of the `error` parameter, e.g. `access_denied`.
    Denied(String),
}

struct Callback {
    result: CallbackResult,
    /// The page to show in the browser once Truffle knows how the sign-in ended.
    reply: oneshot::Sender<Page>,
}

async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    mut cancelled: oneshot::Receiver<()>,
) -> Result<Callback> {
    let (tx, mut rx) = mpsc::channel::<Callback>(1);
    let deadline = tokio::time::sleep(LOGIN_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = &mut cancelled => {
                bail!("The Spotify sign-in was cancelled (a newer attempt may have replaced it).");
            }
            Some(callback) = rx.recv() => return Ok(callback),
            _ = &mut deadline => {
                bail!(
                    "The Spotify sign-in wasn't completed within 5 minutes, so Truffle stopped waiting. \
                     Click Connect to try again. If the browser showed \"INVALID_CLIENT: Invalid redirect \
                     URI\", first add {REDIRECT_URI} to your app's Redirect URIs on developer.spotify.com."
                );
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    tokio::spawn(serve_callback_connection(stream, expected_state.to_owned(), tx.clone()));
                }
                Err(e) => {
                    log::warn!("sign-in listener accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
        }
    }
}

enum CallbackRequest {
    /// Some other path (favicon etc.).
    NotCallback,
    /// A `/callback` request that can't be used; show this page and keep waiting.
    Rejected(Page),
    Accepted(CallbackResult),
}

fn classify_callback(target: &str, expected_state: &str) -> CallbackRequest {
    if !target.starts_with('/') {
        return CallbackRequest::NotCallback;
    }
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1:{CALLBACK_PORT}{target}")) else {
        return CallbackRequest::NotCallback;
    };
    if url.path() != CALLBACK_PATH {
        return CallbackRequest::NotCallback;
    }
    let (mut code, mut state, mut error) = (None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return CallbackRequest::Rejected(Page::failure(
            "This sign-in link is out of date",
            "Go back to Truffle and click Connect again.",
        ));
    }
    if let Some(error) = error {
        return CallbackRequest::Accepted(CallbackResult::Denied(error));
    }
    match code {
        Some(code) if !code.is_empty() => CallbackRequest::Accepted(CallbackResult::Code(code)),
        _ => CallbackRequest::Rejected(Page::failure(
            "Sign-in incomplete",
            "Spotify didn't send a sign-in code. Go back to Truffle and click Connect again.",
        )),
    }
}

async fn serve_callback_connection(mut stream: TcpStream, expected_state: String, tx: mpsc::Sender<Callback>) {
    let target = match tokio::time::timeout(Duration::from_secs(15), read_request_target(&mut stream)).await {
        Ok(Ok(Some(target))) => target,
        Ok(Ok(None)) => {
            let page = Page::failure("Bad request", "Truffle didn't understand this request.");
            let _ = write_html(&mut stream, "400 Bad Request", &page.render()).await;
            return;
        }
        // Idle pre-connections and broken sockets: just drop them.
        _ => return,
    };

    let page = match classify_callback(&target, &expected_state) {
        CallbackRequest::NotCallback => {
            let page = Page::failure("Not found", "This address is only used for signing in to Truffle.");
            let _ = write_html(&mut stream, "404 Not Found", &page.render()).await;
            return;
        }
        CallbackRequest::Rejected(page) => page,
        CallbackRequest::Accepted(result) => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if tx.send(Callback { result, reply: reply_tx }).await.is_err() {
                Page::failure(
                    "Truffle isn't waiting for this sign-in",
                    "This sign-in has already finished or was cancelled. Go back to Truffle and click \
                     Connect if you want to try again.",
                )
            } else {
                match tokio::time::timeout(REPLY_TIMEOUT, reply_rx).await {
                    Ok(Ok(page)) => page,
                    // Truffle hasn't said how it ended: don't claim success.
                    _ => Page::info(
                        "Finishing in Truffle",
                        "Truffle received your sign-in and is still finishing it. Go back to Truffle to see \
                         whether playlist editing is connected; you can close this tab.",
                    ),
                }
            }
        }
    };
    if let Err(e) = write_html(&mut stream, "200 OK", &page.render()).await {
        log::debug!("could not write the sign-in page: {e}");
    }
}

/// Read the request head and return the request target of a `GET`. `None` if the peer closed the
/// connection early or sent something that isn't a GET request.
async fn read_request_target(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let search_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        // Read the whole head before answering: closing a socket with unread data makes Windows
        // send a reset, and the browser would show an error instead of our page.
        if buf[search_from..].windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_HEAD {
            break;
        }
    }
    Ok(parse_request_target(&buf))
}

fn parse_request_target(head: &[u8]) -> Option<String> {
    let line_end = head.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&head[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method != "GET" || !version.starts_with("HTTP/") {
        return None;
    }
    Some(target.to_owned())
}

async fn write_html(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

/// How a sign-in page looks: done, failed, or "see Truffle" (outcome unknown here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageKind {
    Success,
    Failure,
    Info,
}

/// A small HTML page shown in the browser after the redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Page {
    title: String,
    message: String,
    kind: PageKind,
}

impl Page {
    fn new(kind: PageKind, title: &str, message: &str) -> Self {
        Self { title: title.to_owned(), message: message.to_owned(), kind }
    }

    fn success(title: &str, message: &str) -> Self {
        Self::new(PageKind::Success, title, message)
    }

    fn failure(title: &str, message: &str) -> Self {
        Self::new(PageKind::Failure, title, message)
    }

    fn info(title: &str, message: &str) -> Self {
        Self::new(PageKind::Info, title, message)
    }

    fn render(&self) -> String {
        let (accent, mark) = match self.kind {
            PageKind::Success => ("#1db954", "&#10003;"),
            PageKind::Failure => ("#e8a33d", "!"),
            PageKind::Info => ("#8a8a8a", "i"),
        };
        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
             <title>Truffle - {title}</title><style>\
             body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;\
             background:#121212;color:#eaeaea;font-family:'Segoe UI',system-ui,sans-serif}}\
             main{{max-width:30rem;padding:2rem;text-align:center}}\
             .mark{{width:3.5rem;height:3.5rem;margin:0 auto 1rem;border-radius:50%;background:{accent};\
             color:#121212;font-size:2rem;font-weight:700;line-height:3.5rem}}\
             h1{{font-size:1.4rem;margin:0 0 .75rem}}p{{color:#b3b3b3;line-height:1.5;margin:0}}\
             </style></head><body><main><div class=\"mark\">{mark}</div><h1>{title}</h1><p>{message}</p>\
             </main></body></html>",
            title = html_escape(&self.title),
            message = html_escape(&self.message),
        )
    }
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_pair_is_well_formed_and_random() {
        let (verifier, challenge) = new_pkce_pair();
        assert_eq!(verifier.len(), 64);
        assert!((43..=128).contains(&verifier.len()));
        assert!(verifier.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
        assert_eq!(challenge, pkce_challenge(&verifier));
        assert_eq!(challenge.len(), 43);
        let (other, _) = new_pkce_pair();
        assert_ne!(verifier, other);
        assert_ne!(random_state(), random_state());
    }

    #[test]
    fn authorize_url_has_all_parameters() {
        let url = authorize_url("0123456789abcdef0123456789abcdef", "CHALLENGE", "STATE-1");
        assert!(url.starts_with("https://accounts.spotify.com/authorize?"));
        let parsed = url::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:8899/callback");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["code_challenge"], "CHALLENGE");
        assert_eq!(pairs["state"], "STATE-1");
        assert_eq!(pairs["show_dialog"], "true");
        let scopes: Vec<&str> = pairs["scope"].split(' ').collect();
        assert_eq!(scopes, SCOPES);
        // The redirect URI must be percent-encoded, not sent raw.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8899%2Fcallback"));
    }

    #[test]
    fn redirect_uri_matches_listener() {
        let url = url::Url::parse(REDIRECT_URI).unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(CALLBACK_PORT));
        assert_eq!(url.path(), CALLBACK_PATH);
    }

    fn token(expires_at: u64) -> StoredToken {
        StoredToken {
            client_id: "cid".into(),
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at,
            authorized_at: 1_000,
            scope: SCOPES.join(" "),
        }
    }

    #[test]
    fn token_expiry_uses_refresh_margin() {
        let now = 1_000_000;
        assert!(!token(now + REFRESH_MARGIN_SECS + 1).needs_refresh(now));
        assert!(token(now + REFRESH_MARGIN_SECS).needs_refresh(now));
        assert!(token(now + 10).needs_refresh(now));
        assert!(token(now - 10).needs_refresh(now));
        assert!(token(0).needs_refresh(now));
        let mut empty = token(now + 3600);
        empty.access_token.clear();
        assert!(empty.needs_refresh(now));
    }

    #[test]
    fn refresh_keeps_old_refresh_token_when_omitted() {
        let now = 50_000;
        let mut t = token(now - 1);
        t.apply_refresh(
            TokenResponse { access_token: "new".into(), expires_in: Some(3600), refresh_token: None, scope: None },
            now,
        );
        assert_eq!(t.access_token, "new");
        assert_eq!(t.refresh_token, "refresh");
        assert_eq!(t.expires_at, now + 3600);
        assert_eq!(t.authorized_at, 1_000);
        assert!(!t.needs_refresh(now));

        t.apply_refresh(
            TokenResponse { access_token: "newer".into(), expires_in: None, refresh_token: Some(String::new()), scope: None },
            now,
        );
        assert_eq!(t.refresh_token, "refresh");
        assert_eq!(t.expires_at, now + DEFAULT_EXPIRES_IN_SECS);

        t.apply_refresh(
            TokenResponse {
                access_token: "newest".into(),
                expires_in: Some(1800),
                refresh_token: Some("rotated".into()),
                scope: Some("user-read-private".into()),
            },
            now,
        );
        assert_eq!(t.refresh_token, "rotated");
        assert_eq!(t.expires_at, now + 1800);
        assert!(!t.has_scopes(SCOPES));
    }

    #[test]
    fn exchange_requires_refresh_token() {
        let resp = TokenResponse { access_token: "a".into(), expires_in: Some(3600), refresh_token: None, scope: None };
        assert!(StoredToken::from_exchange("cid", resp, 10).is_err());
        let resp = TokenResponse {
            access_token: "a".into(),
            expires_in: Some(3600),
            refresh_token: Some("r".into()),
            scope: Some(SCOPES.join(" ")),
        };
        let t = StoredToken::from_exchange("cid", resp, 10).unwrap();
        assert_eq!((t.expires_at, t.authorized_at), (3610, 10));
        assert!(t.has_scopes(SCOPES));
    }

    #[test]
    fn scope_check() {
        let mut t = token(0);
        assert!(t.has_scopes(SCOPES));
        t.scope = "playlist-read-private user-read-private".into();
        assert!(!t.has_scopes(SCOPES));
        t.scope.clear();
        assert!(t.has_scopes(SCOPES), "unknown scope list is accepted");
    }

    #[test]
    fn stored_token_json_roundtrip_and_old_format() {
        let t = token(123);
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<StoredToken>(&json).unwrap(), t);
        let minimal = r#"{"client_id":"c","access_token":"a","refresh_token":"r","expires_at":5}"#;
        let parsed: StoredToken = serde_json::from_str(minimal).unwrap();
        assert_eq!(parsed.authorized_at, 0);
        assert!(parsed.scope.is_empty());
    }

    #[test]
    fn request_line_parsing() {
        let head = b"GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1:8899\r\n\r\n";
        assert_eq!(parse_request_target(head).as_deref(), Some("/callback?code=abc&state=xyz"));
        assert_eq!(parse_request_target(b"POST /callback HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_request_target(b"GET /callback"), None);
        assert_eq!(parse_request_target(b""), None);
    }

    #[test]
    fn callback_classification() {
        let state = "s1";
        assert!(matches!(classify_callback("/favicon.ico", state), CallbackRequest::NotCallback));
        assert!(matches!(
            classify_callback("/callback?code=c&state=wrong", state),
            CallbackRequest::Rejected(_)
        ));
        assert!(matches!(classify_callback("/callback?code=c", state), CallbackRequest::Rejected(_)));
        assert!(matches!(classify_callback("/callback?state=s1", state), CallbackRequest::Rejected(_)));
        match classify_callback("/callback?code=AQB%2Dx_1&state=s1", state) {
            CallbackRequest::Accepted(r) => assert_eq!(r, CallbackResult::Code("AQB-x_1".into())),
            _ => panic!("expected code"),
        }
        match classify_callback("/callback?error=access_denied&state=s1", state) {
            CallbackRequest::Accepted(r) => assert_eq!(r, CallbackResult::Denied("access_denied".into())),
            _ => panic!("expected denial"),
        }
    }

    #[test]
    fn pages_escape_html() {
        let html = Page::failure("<b>", "a & \"b\"").render();
        assert!(html.contains("&lt;b&gt;"));
        assert!(html.contains("a &amp; &quot;b&quot;"));
        assert!(!html.contains("<b>"));
    }

    #[test]
    fn only_a_finished_sign_in_looks_successful() {
        assert!(Page::success("t", "m").render().contains("&#10003;"));
        let pending = Page::info("t", "m").render();
        assert!(!pending.contains("&#10003;") && !pending.contains("#1db954"));
        assert!(EXCHANGE_TIMEOUT < REPLY_TIMEOUT);
    }

    #[tokio::test]
    async fn loopback_listener_serves_pages_and_returns_code() {
        async fn get(addr: SocketAddr, target: &str) -> String {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/html\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut out = String::new();
            stream.read_to_string(&mut out).await.unwrap();
            out
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        let waiter = tokio::spawn(async move { wait_for_callback(listener, "st", cancel_rx).await });

        // An idle pre-connection (browsers open these) must not block the real request.
        let _idle = TcpStream::connect(addr).await.unwrap();
        assert!(get(addr, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        let stale = get(addr, "/callback?code=x&state=other").await;
        assert!(stale.starts_with("HTTP/1.1 200") && stale.contains("out of date"), "{stale}");

        let browser = tokio::spawn(get(addr, "/callback?code=the-code&state=st"));
        let callback = waiter.await.unwrap().unwrap();
        assert_eq!(callback.result, CallbackResult::Code("the-code".into()));
        callback.reply.send(Page::success("Done", "All good")).unwrap();
        let page = browser.await.unwrap();
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("All good"), "{page}");
    }

    #[tokio::test]
    async fn waiting_for_callback_can_be_cancelled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        cancel_tx.send(()).unwrap();
        let err = wait_for_callback(listener, "st", cancel_rx).await.err().unwrap();
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn newer_login_cancels_older() {
        let (first, mut first_rx) = LoginRegistration::register();
        let (second, mut second_rx) = LoginRegistration::register();
        assert!(first_rx.try_recv().is_ok(), "older attempt is cancelled");
        assert!(second_rx.try_recv().is_err(), "newer attempt keeps running");
        drop(first); // must not clear the newer attempt's slot
        assert!(cancel_pending_login());
        assert!(second_rx.try_recv().is_ok());
        drop(second);
        assert!(!cancel_pending_login());
    }
}
