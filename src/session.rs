//! librespot login and session management.
//!
//! Login uses librespot-oauth with librespot's default (keymaster) client id, then caches the
//! reusable credentials Spotify hands back under `Paths::librespot_dir()`, so the user only sees
//! the browser once. Later launches reconnect silently with [`connect_cached`].
//!
//! Notes for callers:
//! - Every `Session` is single-use: after it becomes invalid (see `Session::is_invalid`), connect a
//!   new one with [`connect_cached`] and hand it to `Engine::set_session`.
//! - Free accounts: the vendored librespot-core only logs the account type (upstream closes the
//!   whole program); their songs play from YouTube (see `crate::youtube`).

use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use librespot_core::Session;
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::error::ErrorKind;
use librespot_oauth::{OAuthClientBuilder, OAuthError, OAuthToken};

use crate::config::{Paths, Settings};

/// Loopback port for the OAuth redirect. It must match a redirect URI registered for the
/// keymaster client id, so it cannot be changed freely.
const REDIRECT_PORT: u16 = 8898;
const REDIRECT_URI: &str = "http://127.0.0.1:8898/login";

/// Scopes requested for the playback login. `streaming` is what playback needs; the rest are
/// standard read/modify scopes so the session's token is useful for library features too.
const OAUTH_SCOPES: &[&str] = &[
    "streaming",
    "user-read-private",
    "user-read-email",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "ugc-image-upload",
    "user-library-read",
    "user-library-modify",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
];

/// How long the user gets to finish logging in inside the browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Upper bound for `Session::connect` (librespot tries up to 6 access points internally).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(75);

/// File librespot's `Cache` keeps the reusable credentials in (inside `librespot_dir`).
const CREDENTIALS_FILE: &str = "credentials.json";
/// Stable per-install device id, so Spotify sees one device instead of a new one every launch.
const DEVICE_ID_FILE: &str = "device_id";

/// Page shown in the browser tab after the redirect. Plain ASCII: the listener sends no charset.
const LOGIN_DONE_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Truffle</title></head>\
<body style=\"font-family:'Segoe UI',system-ui,sans-serif;background:#121212;color:#f0f0f0;\
text-align:center;padding-top:18vh\"><h1 style=\"font-weight:600\">Logged in &mdash; you can close this tab \
and return to Truffle</h1></body></html>";

/// Number of browser-login threads still running (possibly abandoned after a timeout).
static LOGIN_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Connect using cached reusable credentials. Returns `Ok(None)` if there are none, or if Spotify
/// rejected them (they are deleted in that case, so the next step is an interactive login).
/// Network problems are returned as `Err` with a plain-language message.
/// Must be called from within the tokio runtime.
pub async fn connect_cached(paths: &Paths, settings: &Settings) -> Result<Option<Session>> {
    let cache = build_cache(paths, settings)?;
    let Some(credentials) = cache.credentials() else {
        log::info!("no saved Spotify login");
        return Ok(None);
    };

    let session = Session::new(session_config(paths), Some(cache));
    match connect_session(&session, credentials).await {
        Ok(()) => {
            log::info!("connected to Spotify as {}", session.username());
            Ok(Some(session))
        }
        Err(ConnectError::Rejected(reason)) => {
            log::warn!("Spotify rejected the saved login ({reason}); a new login is needed");
            if let Err(e) = forget_credentials(paths) {
                log::warn!("{e:#}");
            }
            Ok(None)
        }
        Err(other) => Err(other.into_user_error()),
    }
}

/// Open the system browser for Spotify login (OAuth on a blocking thread, loopback redirect),
/// then connect and store reusable credentials. Must be called from within the tokio runtime.
///
/// The browser step waits at most 5 minutes. librespot's redirect listener cannot be cancelled,
/// so on timeout (or if this future is dropped) the blocking thread may linger briefly; Truffle
/// unblocks it by connecting to the listener once, which makes it give up and free the port.
pub async fn connect_interactive(paths: &Paths, settings: &Settings) -> Result<Session> {
    let config = session_config(paths);
    let cache = build_cache(paths, settings)?;

    ensure_login_port_free().await?;

    let client_id = config.client_id.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let unblocker = ListenerUnblocker { finished: finished.clone() };
    // Created here and moved in, so the count stays right even if the closure never runs.
    let mark = LoginThreadMark::new(finished);
    let login = tokio::task::spawn_blocking(move || {
        let _mark = mark;
        browser_login(&client_id)
    });

    log::info!("waiting for the Spotify login in the browser");
    let token = match tokio::time::timeout(LOGIN_TIMEOUT, login).await {
        Err(_) => {
            drop(unblocker);
            bail!("The Spotify login timed out after 5 minutes. Click Log in to try again.");
        }
        Ok(Err(join_error)) => {
            log::error!("browser login thread failed: {join_error}");
            bail!("The Spotify login stopped unexpectedly. Please try again.");
        }
        Ok(Ok(Err(oauth_error))) => return Err(oauth_user_error(oauth_error)),
        Ok(Ok(Ok(token))) => token,
    };
    drop(unblocker);

    let session = Session::new(config, Some(cache));
    let credentials = Credentials::with_access_token(token.access_token);
    match connect_session(&session, credentials).await {
        Ok(()) => {
            log::info!("logged in to Spotify as {}", session.username());
            Ok(session)
        }
        Err(ConnectError::Rejected(reason)) => {
            log::warn!("Spotify rejected the fresh login: {reason}");
            bail!("Spotify didn't accept the login. Please try again.")
        }
        Err(other) => Err(other.into_user_error()),
    }
}

/// Delete cached credentials (logout).
pub fn forget_credentials(paths: &Paths) -> Result<()> {
    let file = paths.librespot_dir().join(CREDENTIALS_FILE);
    match std::fs::remove_file(&file) {
        Ok(()) => {
            log::info!("removed the saved Spotify login");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!(
            "Truffle couldn't delete the saved Spotify login ({}): {e}",
            file.display()
        )),
    }
}

/// The logged-in username (canonical Spotify user id).
pub fn username(session: &Session) -> String {
    session.username()
}

// ---------------------------------------------------------------------------------------------
// Session setup

/// librespot's default config (keymaster client id) with a device id that stays the same
/// across launches.
fn session_config(paths: &Paths) -> SessionConfig {
    let mut config = SessionConfig::default();
    config.device_id = stable_device_id(paths, &config.device_id);
    config
}

fn stable_device_id(paths: &Paths, fresh: &str) -> String {
    let dir = paths.librespot_dir();
    let file = dir.join(DEVICE_ID_FILE);
    if let Ok(text) = std::fs::read_to_string(&file) {
        let id = text.trim();
        if is_valid_device_id(id) {
            return id.to_owned();
        }
        log::warn!("ignoring malformed device id in {}", file.display());
    }
    let saved = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, fresh));
    if let Err(e) = saved {
        log::warn!("could not save the device id to {}: {e}", file.display());
    }
    fresh.to_owned()
}

fn is_valid_device_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Credential cache in `librespot_dir`, plus librespot's audio file cache when enabled.
/// Volume is not cached here: the controller persists it in `Settings`.
fn build_cache(paths: &Paths, settings: &Settings) -> Result<Cache> {
    let credentials_dir = paths.librespot_dir();
    if settings.audio_cache_mb > 0 {
        let limit = settings.audio_cache_mb.saturating_mul(1024 * 1024);
        match Cache::new(
            Some(credentials_dir.clone()),
            None,
            Some(paths.audio_cache_dir()),
            Some(limit),
        ) {
            Ok(cache) => return Ok(cache),
            Err(e) => log::warn!("audio cache unavailable, continuing without it: {e}"),
        }
    }
    Cache::new(Some(credentials_dir.clone()), None, None, None).map_err(|e| {
        anyhow!(
            "Truffle couldn't open its settings folder ({}): {e}",
            credentials_dir.display()
        )
    })
}

// ---------------------------------------------------------------------------------------------
// Connecting

/// Why `Session::connect` failed, sorted by what the caller should do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectError {
    /// Spotify refused the credentials themselves (wrong, expired or revoked): log in again.
    Rejected(String),
    /// The account is not Premium.
    PremiumRequired,
    /// Spotify refused the login for another reason (travel restriction, banned client, ...).
    Refused(String),
    /// Spotify could not be reached or the connection broke.
    Network,
    /// Connecting took longer than `CONNECT_TIMEOUT`.
    TimedOut,
}

impl ConnectError {
    fn into_user_error(self) -> anyhow::Error {
        match self {
            ConnectError::Rejected(_) => {
                anyhow!("Spotify didn't accept the saved login. Please log in again.")
            }
            ConnectError::PremiumRequired => anyhow!(
                "Truffle needs a Spotify Premium account. Free accounts can't play music in Truffle."
            ),
            ConnectError::Refused(reason) if reason.to_ascii_lowercase().contains("travel") => {
                anyhow!("Spotify says your account can't be used from this country right now.")
            }
            ConnectError::Refused(reason) => {
                anyhow!("Spotify refused the login ({reason}). Please try again later.")
            }
            ConnectError::Network => anyhow!(
                "Couldn't connect to Spotify. Check your internet connection and try again."
            ),
            ConnectError::TimedOut => anyhow!(
                "Connecting to Spotify took too long. Check your internet connection and try again."
            ),
        }
    }
}

async fn connect_session(session: &Session, credentials: Credentials) -> Result<(), ConnectError> {
    match tokio::time::timeout(CONNECT_TIMEOUT, session.connect(credentials, true)).await {
        Err(_) => {
            log::warn!("connecting to Spotify timed out");
            Err(ConnectError::TimedOut)
        }
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            log::warn!("connecting to Spotify failed: {e}");
            Err(classify_connect_error(e.kind, &e.error.to_string()))
        }
    }
}

/// librespot maps every access-point login refusal to `PermissionDenied` with the message
/// "Login failed with reason: <reason>" (its `AuthenticationError` type is private, so the
/// message is the only way to tell the reasons apart). Everything else is a transport problem.
fn classify_connect_error(kind: ErrorKind, detail: &str) -> ConnectError {
    const LOGIN_FAILED: &str = "Login failed with reason:";
    if kind != ErrorKind::PermissionDenied {
        return ConnectError::Network;
    }
    let Some(reason) = detail.trim().strip_prefix(LOGIN_FAILED) else {
        return ConnectError::Network;
    };
    let reason = reason.trim().to_owned();
    let lower = reason.to_ascii_lowercase();
    if lower.contains("premium") {
        ConnectError::PremiumRequired
    } else if lower.contains("credentials") || lower.contains("verification") {
        ConnectError::Rejected(reason)
    } else if lower.contains("access point") {
        ConnectError::Network
    } else {
        ConnectError::Refused(reason)
    }
}

// ---------------------------------------------------------------------------------------------
// Browser login

/// Blocking: opens the browser and waits on librespot's loopback listener for the redirect.
fn browser_login(client_id: &str) -> Result<OAuthToken, OAuthError> {
    let client = OAuthClientBuilder::new(client_id, REDIRECT_URI, OAUTH_SCOPES.to_vec())
        .open_in_browser()
        .with_custom_message(LOGIN_DONE_PAGE)
        .build()?;
    client.get_access_token()
}

fn oauth_user_error(error: OAuthError) -> anyhow::Error {
    log::warn!("Spotify browser login failed: {error}");
    match error {
        OAuthError::AuthCodeListenerBind { .. } => port_busy_error(),
        OAuthError::AuthCodeNotFound { .. }
        | OAuthError::AuthCodeListenerParse
        | OAuthError::AuthCodeListenerRead
        | OAuthError::AuthCodeListenerTerminated => {
            anyhow!("The Spotify login was cancelled or didn't finish. Please try again.")
        }
        OAuthError::AuthCodeListenerWrite => {
            anyhow!("The login page was closed too early. Please try again.")
        }
        OAuthError::ExchangeCode { .. } | OAuthError::Recv => anyhow!(
            "Spotify didn't finish the login. Check your internet connection and try again."
        ),
        _ => anyhow!("The Spotify login failed. Please try again."),
    }
}

fn port_busy_error() -> anyhow::Error {
    anyhow!(
        "Truffle couldn't start its login helper because another program is using port {REDIRECT_PORT}. \
         Close other Spotify tools (or another Truffle window) and try again."
    )
}

fn login_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], REDIRECT_PORT))
}

fn login_port_free() -> bool {
    TcpListener::bind(login_addr()).is_ok()
}

/// Make sure the redirect port can be bound before the browser opens; otherwise the user would
/// log in only for the redirect to hit nothing. If one of our own earlier (abandoned) login
/// threads still holds it, unblock that thread first.
async fn ensure_login_port_free() -> Result<()> {
    for attempt in 0..10 {
        if login_port_free() {
            return Ok(());
        }
        if LOGIN_THREADS.load(Ordering::Acquire) == 0 {
            break;
        }
        if attempt == 0 {
            log::info!("closing an earlier, unfinished login attempt");
            poke_login_listener();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(port_busy_error())
}

/// Connect to librespot's redirect listener and hang up. The listener accepts exactly one
/// connection, so this makes a stuck `get_access_token` fail and release the port.
fn poke_login_listener() {
    match TcpStream::connect_timeout(&login_addr(), Duration::from_millis(500)) {
        Ok(stream) => {
            let _ = stream.shutdown(Shutdown::Both);
        }
        Err(e) => log::debug!("login listener not reachable (already closed?): {e}"),
    }
}

/// Dropped when `connect_interactive` stops waiting; unblocks the login thread if it is still
/// waiting for the browser.
struct ListenerUnblocker {
    finished: Arc<AtomicBool>,
}

impl Drop for ListenerUnblocker {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            log::info!("abandoning the unfinished browser login");
            poke_login_listener();
        }
    }
}

/// Lives on the blocking login thread; records that it has finished (even on panic).
struct LoginThreadMark {
    finished: Arc<AtomicBool>,
}

impl LoginThreadMark {
    fn new(finished: Arc<AtomicBool>) -> Self {
        LOGIN_THREADS.fetch_add(1, Ordering::AcqRel);
        Self { finished }
    }
}

impl Drop for LoginThreadMark {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        LOGIN_THREADS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_credentials_are_rejected() {
        let kind = ErrorKind::PermissionDenied;
        assert_eq!(
            classify_connect_error(kind, "Login failed with reason: Bad credentials"),
            ConnectError::Rejected("Bad credentials".into())
        );
        assert_eq!(
            classify_connect_error(kind, "Login failed with reason: Could not validate credentials"),
            ConnectError::Rejected("Could not validate credentials".into())
        );
    }

    #[test]
    fn premium_and_other_refusals_are_not_rejections() {
        let kind = ErrorKind::PermissionDenied;
        assert_eq!(
            classify_connect_error(kind, "Login failed with reason: Premium account required"),
            ConnectError::PremiumRequired
        );
        assert_eq!(
            classify_connect_error(kind, "Login failed with reason: Travel restriction"),
            ConnectError::Refused("Travel restriction".into())
        );
        assert_eq!(
            classify_connect_error(kind, "Login failed with reason: Try another access point"),
            ConnectError::Network
        );
    }

    #[test]
    fn transport_errors_are_network_errors() {
        // An OS-level "access denied" on the socket is also PermissionDenied, but not a login refusal.
        assert_eq!(
            classify_connect_error(ErrorKind::PermissionDenied, "An attempt was made to access a socket"),
            ConnectError::Network
        );
        assert_eq!(
            classify_connect_error(ErrorKind::Unavailable, "transport returned no data"),
            ConnectError::Network
        );
        assert_eq!(
            classify_connect_error(ErrorKind::DeadlineExceeded, "timed out"),
            ConnectError::Network
        );
    }

    #[test]
    fn device_id_validation() {
        assert!(is_valid_device_id("3f2504e0-4f89-41d3-9a0c-0305e82c3301"));
        assert!(!is_valid_device_id(""));
        assert!(!is_valid_device_id("has spaces"));
        assert!(!is_valid_device_id(&"a".repeat(65)));
    }

    #[test]
    fn login_futures_can_be_spawned() {
        fn assert_send<T: Send>(_: &T) {}
        let paths = Paths { config: "unused".into(), cache: "unused".into() };
        let settings = Settings::default();
        // Only constructs the futures; nothing runs.
        assert_send(&connect_cached(&paths, &settings));
        assert_send(&connect_interactive(&paths, &settings));
    }

    #[tokio::test]
    async fn no_saved_login_means_none() {
        let root = std::env::temp_dir().join(format!("truffle-session-test-{}", std::process::id()));
        let paths = Paths { config: root.join("config"), cache: root.join("cache") };
        let settings = Settings { audio_cache_mb: 1, ..Settings::default() };

        assert!(connect_cached(&paths, &settings).await.unwrap().is_none());
        assert!(paths.audio_cache_dir().is_dir(), "audio cache folder is created");
        // A stable device id is created once and then reused.
        let first = session_config(&paths).device_id;
        assert_eq!(session_config(&paths).device_id, first);
        // Logging out with nothing saved is fine.
        forget_credentials(&paths).unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn login_page_is_ascii() {
        // The loopback listener sends no charset header, so keep the page pure ASCII.
        assert!(LOGIN_DONE_PAGE.is_ascii());
    }
}
