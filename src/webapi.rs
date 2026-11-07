//! Optional official Spotify Web API client (user's own developer client id, PKCE login with
//! loopback redirect `http://127.0.0.1:8899/callback`). Used for editing: playlist name and
//! description, cover upload, liked songs.
//!
//! Layout: `auth` (PKCE sign-in, token file, refresh), `http` (rate limiting and retries),
//! `cover` (image preparation), `types` (JSON shapes and mapping to `model`).

mod auth;
mod cover;
mod http;
mod types;

#[cfg(test)]
mod mock_tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::prelude::{BASE64_STANDARD, Engine as _};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Method;

use crate::config::Paths;
use crate::model::{TrackInfo, UserProfile};

use self::http::ReqBody;
use self::types::{ApiPage, ApiSavedTrack, ApiUser};

pub use self::auth::{SCOPES, cancel_pending_login};
pub use self::cover::{MAX_COVER_BASE64_LEN, prepare_cover_jpeg};

pub const REDIRECT_URI: &str = "http://127.0.0.1:8899/callback";

/// Max URIs per `PUT/DELETE /me/library` request.
const LIBRARY_BATCH: usize = 40;
/// Max page size of `GET /me/tracks`.
const LIKED_PAGE: usize = 50;
/// Spotify only accepts one cover upload at a time per app (custom rate limit).
static COVER_UPLOAD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The stored sign-in no longer works (refresh token expired after ~6 months, revoked, or the
/// client id was deleted). The token file has been removed; the user must click Connect again.
/// Check with [`is_reconnect_required`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectRequired {
    message: String,
}

impl ReconnectRequired {
    fn expired() -> Self {
        Self::new(
            "Your Spotify sign-in for playlist editing has expired (Spotify ends these sign-ins after \
             about 6 months). Open Settings and click Connect to sign in again.",
        )
    }

    fn new(message: &str) -> Self {
        Self { message: message.to_owned() }
    }
}

impl std::fmt::Display for ReconnectRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ReconnectRequired {}

/// True when `err` means the user has to sign in to the Web API again.
pub fn is_reconnect_required(err: &anyhow::Error) -> bool {
    err.is::<ReconnectRequired>()
}

/// Extract the base62 playlist id from a `spotify:playlist:` URI, an open.spotify.com link or a
/// bare id. `None` if it doesn't look like a playlist id.
pub fn playlist_id_from_uri(input: &str) -> Option<&str> {
    let s = input.trim();
    let id = if let Some(pos) = s.rfind("playlist:") {
        &s[pos + "playlist:".len()..]
    } else if let Some(pos) = s.find("/playlist/") {
        s[pos + "/playlist/".len()..].split(['?', '/', '#']).next().unwrap_or("")
    } else {
        s
    };
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())).then_some(id)
}

fn parse_playlist_id(input: &str) -> Result<&str> {
    playlist_id_from_uri(input).ok_or_else(|| {
        log::warn!("not a playlist id: {input:?}");
        anyhow!("That isn't a Spotify playlist Truffle can edit.")
    })
}

/// Trim and sanity-check a pasted client id.
fn normalize_client_id(client_id: &str) -> Result<String> {
    let id = client_id.trim();
    if id.is_empty() {
        bail!("Please paste the Client ID of your Spotify developer app first.");
    }
    if id.len() != 32 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "That doesn't look like a Spotify Client ID. Copy the 32-character Client ID (not the Client \
             Secret) from your app's Settings page on developer.spotify.com."
        );
    }
    Ok(id.to_owned())
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("Truffle/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(2)
        .build()
        .map_err(|e| {
            log::error!("could not build the HTTP client: {e}");
            anyhow!("Truffle couldn't set up its connection to Spotify ({e}).")
        })
}

/// Where requests go: Spotify in the app, a local mock server in tests.
#[derive(Debug, Clone)]
struct Endpoints {
    api_base: String,
    token_url: String,
}

impl Endpoints {
    fn spotify() -> Self {
        Self { api_base: http::API_BASE.to_owned(), token_url: auth::TOKEN_URL.to_owned() }
    }
}

#[derive(Clone)]
pub struct WebApi {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    /// `https://api.spotify.com/v1` (a local mock in tests).
    api_base: String,
    /// `https://accounts.spotify.com/api/token` (a local mock in tests).
    token_url: String,
    client_id: String,
    token_path: PathBuf,
    token: tokio::sync::Mutex<auth::StoredToken>,
    /// Unix seconds of the browser sign-in (0 if unknown).
    authorized_at: u64,
    /// Set once Spotify rejected the refresh token; every call then fails fast.
    revoked: AtomicBool,
    /// The signed-in user's id, fetched once (needed to tell which playlists are editable).
    my_id: tokio::sync::OnceCell<String>,
}

impl std::fmt::Debug for WebApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebApi").field("client_id", &self.inner.client_id).finish_non_exhaustive()
    }
}

impl WebApi {
    /// Use a stored refresh token if present; otherwise, if `interactive`, run the PKCE browser
    /// login. Returns `Ok(None)` when not interactive and no usable token is stored.
    ///
    /// When not interactive and a token *was* stored but Spotify no longer accepts it (expired
    /// after ~6 months, revoked, missing a permission), this returns an error for which
    /// [`is_reconnect_required`] is true, so the user can be told to click Connect again.
    /// Network trouble while refreshing is not an error: the token is refreshed on first use.
    pub async fn connect(client_id: &str, paths: &Paths, interactive: bool) -> Result<Option<Self>> {
        Self::connect_with(client_id, paths, interactive, Endpoints::spotify()).await
    }

    async fn connect_with(
        client_id: &str,
        paths: &Paths,
        interactive: bool,
        endpoints: Endpoints,
    ) -> Result<Option<Self>> {
        let client_id = normalize_client_id(client_id)?;
        let token_path = paths.webapi_token_file();
        let http = build_http_client()?;
        let mut unusable: Option<anyhow::Error> = None;

        if let Some(stored) = auth::load_token(&token_path) {
            if stored.client_id != client_id {
                log::info!("stored Web API sign-in belongs to another client id; ignoring it");
            } else if !stored.has_scopes(SCOPES) {
                log::info!("stored Web API sign-in lacks permissions Truffle now needs; a new sign-in is required");
                unusable = Some(
                    ReconnectRequired::new(
                        "Playlist editing needs an extra permission from Spotify. Open Settings and click \
                         Connect to sign in again.",
                    )
                    .into(),
                );
            } else if stored.refresh_token.is_empty() {
                log::info!("stored Web API sign-in has no refresh token; a new sign-in is required");
                unusable = Some(ReconnectRequired::expired().into());
            } else {
                let api = Self::from_parts(http.clone(), token_path.clone(), stored, endpoints.clone());
                match api.access_token().await {
                    Ok(_) => return Ok(Some(api)),
                    Err(e) if is_reconnect_required(&e) => {
                        // The token file is already deleted; a fresh sign-in is needed.
                        log::warn!("stored Web API sign-in no longer works: {e}");
                        unusable = Some(e);
                    }
                    Err(e) => {
                        // Offline or Spotify trouble: keep the sign-in and refresh on first use.
                        log::warn!("could not refresh the Web API token yet ({e}); will retry on first use");
                        return Ok(Some(api));
                    }
                }
            }
        }

        if !interactive {
            return match unusable {
                Some(e) => Err(e),
                None => Ok(None),
            };
        }
        let token = auth::interactive_login(&http, &client_id).await?;
        if let Err(e) = auth::save_token(&token_path, &token) {
            // Still usable for this session; the user will just have to sign in again next time.
            log::warn!("{e:#}");
        }
        log::info!("Web API connected");
        Ok(Some(Self::from_parts(http, token_path, token, endpoints)))
    }

    fn from_parts(http: reqwest::Client, token_path: PathBuf, token: auth::StoredToken, endpoints: Endpoints) -> Self {
        Self {
            inner: Arc::new(Inner {
                http,
                api_base: endpoints.api_base,
                token_url: endpoints.token_url,
                client_id: token.client_id.clone(),
                authorized_at: token.authorized_at,
                token_path,
                token: tokio::sync::Mutex::new(token),
                revoked: AtomicBool::new(false),
                my_id: tokio::sync::OnceCell::new(),
            }),
        }
    }

    /// Delete the stored token.
    pub fn forget(paths: &Paths) -> Result<()> {
        cancel_pending_login();
        auth::delete_token(&paths.webapi_token_file())
    }

    /// Whole days until Spotify ends this sign-in (refresh tokens expire about 6 months after the
    /// browser sign-in). `None` when the sign-in date is unknown. The controller uses it for a
    /// "reconnect soon" reminder at startup.
    pub fn days_until_reconnect(&self) -> Option<u64> {
        if self.inner.authorized_at == 0 {
            return None;
        }
        let ends = self.inner.authorized_at.saturating_add(auth::REFRESH_TOKEN_LIFETIME_SECS);
        Some(ends.saturating_sub(auth::now_unix()) / 86_400)
    }

    pub async fn me(&self) -> Result<UserProfile> {
        let user: ApiUser = self.get_json("/me", &[], "load your Spotify profile").await?;
        if !user.id.is_empty() {
            // Ignore "already set": another task may have fetched it concurrently.
            let _ = self.inner.my_id.set(user.id.clone());
        }
        Ok(types::user_profile(user))
    }

    /// The Spotify user id this sign-in belongs to (fetched once, then remembered). The controller
    /// compares it with the account logged in through librespot: editing must never act on
    /// another account.
    pub async fn user_id(&self) -> Result<String> {
        self.my_id().await.map(str::to_owned)
    }

    async fn my_id(&self) -> Result<&str> {
        let id = self
            .inner
            .my_id
            .get_or_try_init(|| async {
                let user: ApiUser = self.get_json("/me", &[], "load your Spotify profile").await?;
                if user.id.is_empty() {
                    bail!("Couldn't load your Spotify profile: Spotify didn't say who is signed in.");
                }
                Ok(user.id)
            })
            .await?;
        Ok(id.as_str())
    }

    pub async fn update_playlist_details(&self, playlist_id: &str, name: Option<&str>, description: Option<&str>) -> Result<()> {
        let id = parse_playlist_id(playlist_id)?;
        let mut body = serde_json::Map::new();
        if let Some(name) = name {
            let name = name.trim();
            if name.is_empty() {
                bail!("A playlist name can't be empty.");
            }
            body.insert("name".to_owned(), name.into());
        }
        if let Some(description) = description {
            // Spotify playlist descriptions are a single line.
            let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
            body.insert("description".to_owned(), description.into());
        }
        if body.is_empty() {
            return Ok(());
        }
        self.call(
            Method::PUT,
            &format!("/playlists/{id}"),
            &[],
            &ReqBody::Json(serde_json::Value::Object(body)),
            "change the playlist details",
        )
        .await?;
        Ok(())
    }

    /// Resizes/re-encodes the image to a JPEG whose base64 is <= 256 KB, then uploads it.
    pub async fn upload_playlist_cover(&self, playlist_id: &str, image_path: &Path) -> Result<()> {
        let id = parse_playlist_id(playlist_id)?;
        let path = image_path.to_path_buf();
        let jpeg = tokio::task::spawn_blocking(move || prepare_cover_jpeg(&path))
            .await
            .map_err(|e| {
                log::error!("cover preparation task failed: {e}");
                anyhow!("Truffle couldn't process the picture. Please try a different one.")
            })??;
        let encoded = BASE64_STANDARD.encode(&jpeg);
        drop(jpeg);
        if encoded.len() > MAX_COVER_BASE64_LEN {
            bail!("The picture is still too large for Spotify after shrinking. Please try a different one.");
        }
        let _one_at_a_time = COVER_UPLOAD.lock().await;
        self.call(
            Method::PUT,
            &format!("/playlists/{id}/images"),
            &[],
            &ReqBody::Jpeg(Bytes::from(encoded)),
            "upload the playlist cover",
        )
        .await?;
        log::info!("cover uploaded for playlist {id}");
        Ok(())
    }

    /// One page of search results: up to 10 songs (from `offset`), and on the first page up to 10
    /// albums, artists and playlists too.
    pub async fn search(&self, query: &str, offset: u32) -> Result<crate::model::SearchResults> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(crate::model::SearchResults::default());
        }
        let kinds = if offset == 0 { "track,album,artist,playlist" } else { "track" };
        let offset_s = offset.to_string();
        let value: serde_json::Value = self
            .get_json(
                "/search",
                &[("q", query), ("type", kinds), ("limit", "10"), ("offset", offset_s.as_str()), ("market", "from_token")],
                "search Spotify",
            )
            .await?;
        Ok(types::search_results(query, offset, &value))
    }

    /// Removes songs from a playlist (every copy of each), 100 per request.
    pub async fn remove_items(&self, playlist_id: &str, uris: &[String]) -> Result<()> {
        let id = parse_playlist_id(playlist_id)?;
        for batch in uris.chunks(100) {
            let items: Vec<serde_json::Value> = batch.iter().map(|u| serde_json::json!({ "uri": u })).collect();
            let body = serde_json::json!({ "items": items, "tracks": items });
            self.call(Method::DELETE, &format!("/playlists/{id}/items"), &[], &ReqBody::Json(body), "remove songs from the playlist")
                .await?;
        }
        Ok(())
    }

    /// Removes a playlist from the user's library (unfollows it; for a playlist the user owns this
    /// is what Spotify calls deleting).
    pub async fn remove_playlist(&self, playlist_uri: &str) -> Result<()> {
        let id = parse_playlist_id(playlist_uri)?;
        let uri = format!("spotify:playlist:{id}");
        self.call(Method::DELETE, "/me/library", &[("uris", uri.as_str())], &ReqBody::Empty, "remove the playlist").await?;
        Ok(())
    }

    pub async fn set_liked(&self, uris: &[String], liked: bool) -> Result<()> {
        let saveable: Vec<&str> = uris
            .iter()
            .map(|u| u.trim())
            .filter(|u| u.starts_with("spotify:") && !u.starts_with("spotify:local:") && !u.contains(','))
            .collect();
        if saveable.len() < uris.len() {
            log::info!("set_liked: skipping {} local or invalid URIs", uris.len() - saveable.len());
        }
        if saveable.is_empty() {
            if uris.is_empty() {
                return Ok(());
            }
            bail!("Only Spotify songs can be liked. Local files can't be added to Liked Songs.");
        }
        let (method, what) = if liked {
            (Method::PUT, "add to Liked Songs")
        } else {
            (Method::DELETE, "remove from Liked Songs")
        };
        for batch in saveable.chunks(LIBRARY_BATCH) {
            let joined = batch.join(",");
            self.call(method.clone(), "/me/library", &[("uris", joined.as_str())], &ReqBody::Empty, what)
                .await?;
        }
        Ok(())
    }

    /// Every liked song, newest first, and the requests it took.
    pub async fn liked_tracks(&self, progress: &(dyn Fn(usize, Option<usize>) + Send + Sync)) -> Result<(Vec<TrackInfo>, usize)> {
        let mut collector = LikedCollector::default();
        let first = self.liked_page(0).await?;
        let mut requests = 1;
        let total = first.total.map(|t| t as usize);
        collector.tracks.reserve(total.unwrap_or(0).min(100_000));
        let mut last_len = collector.add(first);
        progress(collector.tracks.len(), total);

        match total {
            Some(total) => {
                // Offsets are known up front: fetch two pages at a time, in order.
                let offsets = (LIKED_PAGE..total).step_by(LIKED_PAGE);
                let mut pages = futures::stream::iter(offsets).map(|offset| self.liked_page(offset)).buffered(2);
                while let Some(page) = pages.next().await {
                    collector.add(page?);
                    requests += 1;
                    progress(collector.tracks.len(), Some(total));
                }
                if collector.entries < total {
                    log::warn!("Liked Songs: Spotify reported {total} entries but returned {}", collector.entries);
                }
            }
            None => {
                // No total in the response: follow pages until a short one.
                while last_len == LIKED_PAGE {
                    let page = self.liked_page(collector.entries).await?;
                    requests += 1;
                    last_len = collector.add(page);
                    progress(collector.tracks.len(), None);
                }
            }
        }
        if collector.skipped > 0 {
            log::info!("Liked Songs: skipped {} removed or unreadable entries", collector.skipped);
        }
        Ok((collector.tracks, requests))
    }

    /// Liked Songs read newest first only until they meet `known` (the list as last read, newest
    /// first, with when each song was liked): the songs liked since are read and the rest of
    /// `known` is kept. `unlisted` is how many entries Spotify counted that `known` leaves out.
    /// When Spotify's total shows songs further down went too, everything is read again. Returns
    /// the songs and the requests made.
    pub async fn liked_tracks_since(
        &self,
        known: &[TrackInfo],
        unlisted: usize,
        progress: &(dyn Fn(usize, Option<usize>) + Send + Sync),
    ) -> Result<(Vec<TrackInfo>, usize)> {
        // The same like on both sides: the same song, liked in the same second.
        let index: HashMap<(&str, i64), usize> =
            known.iter().enumerate().filter(|(_, t)| t.added_at != 0).map(|(i, t)| ((t.uri.as_str(), t.added_at), i)).collect();
        let mut new = LikedCollector::default();
        let mut requests = 0;
        loop {
            let offset = new.entries;
            let page = self.liked_page(offset).await?;
            requests += 1;
            let total = page.total.map(|t| t as usize);
            let len = page.items.len();
            let mut met = None;
            for (i, item) in page.items.into_iter().enumerate() {
                let song = saved_song(item);
                if let Some(&at) = song.as_ref().and_then(|t| index.get(&(t.uri.as_str(), t.added_at))) {
                    met = Some((offset + i, at));
                    break;
                }
                new.push(song);
            }
            progress(new.tracks.len(), total);
            match (met, total) {
                (Some((above, at)), Some(total)) => {
                    if let Some(songs) = line_up(new.tracks, above, at, known, total, unlisted) {
                        progress(songs.len(), Some(songs.len()));
                        return Ok((songs, requests));
                    }
                    log::info!("Liked Songs: the saved copy doesn't add up to Spotify's {total}; reading them all");
                    break;
                }
                // Nothing to check the rest against.
                (Some(_), None) => break,
                // Nothing in common with the saved copy: every song is new.
                (None, _) if len < LIKED_PAGE || total.is_some_and(|t| offset + len >= t) => return Ok((new.tracks, requests)),
                (None, _) => {}
            }
        }
        let (tracks, more) = self.liked_tracks(progress).await?;
        Ok((tracks, requests + more))
    }

    /// A cheap marker that changes whenever Liked Songs change (the endpoint has no snapshot id):
    /// the total plus the `added_at` and id of the newest entry, from a one-item page.
    pub async fn liked_fingerprint(&self) -> Result<String> {
        let page: ApiPage = self
            .get_json(
                "/me/tracks",
                &[("limit", "1"), ("offset", "0"), ("market", "from_token")],
                "check your Liked Songs for changes",
            )
            .await?;
        Ok(liked_fingerprint_of(&page))
    }

    async fn liked_page(&self, offset: usize) -> Result<ApiPage> {
        let offset = offset.to_string();
        let limit = LIKED_PAGE.to_string();
        self.get_json(
            "/me/tracks",
            &[("limit", limit.as_str()), ("offset", offset.as_str()), ("market", "from_token")],
            "load your Liked Songs",
        )
        .await
    }
}

fn liked_fingerprint_of(page: &ApiPage) -> String {
    let first = page.items.first();
    let text = |v: Option<&serde_json::Value>| v.and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
    let added_at = text(first.and_then(|item| item.get("added_at")));
    let track = first.and_then(|item| item.get("track"));
    let id = text(track.and_then(|t| t.get("id").or_else(|| t.get("uri"))));
    let total = page.total.map_or_else(|| "?".to_owned(), |t| t.to_string());
    format!("{total}:{added_at}:{id}")
}

/// Liked Songs from those read down to where they met `known` (`new`, the `above` entries before
/// `known[at]`), then `known` from that song on, less songs liked again since (they are in
/// `new`). `None` unless that adds up to Spotify's `total`: songs further down went too.
fn line_up(mut new: Vec<TrackInfo>, above: usize, at: usize, known: &[TrackInfo], total: usize, unlisted: usize) -> Option<Vec<TrackInfo>> {
    // Songs liked in the same second may be listed in another order: keep the whole second.
    let second = known[at].added_at;
    let from = known[..at].iter().rposition(|t| t.added_at != second).map_or(0, |p| p + 1);
    let rest: Vec<TrackInfo> = {
        let fresh: HashSet<&str> = new.iter().map(|t| t.uri.as_str()).collect();
        known[from..].iter().filter(|t| !fresh.contains(t.uri.as_str())).cloned().collect()
    };
    if above + rest.len() + unlisted != total {
        return None;
    }
    new.extend(rest);
    Some(new)
}

/// One `/me/tracks` entry as a song, with when it was liked. `None` when unreadable.
fn saved_song(item: serde_json::Value) -> Option<TrackInfo> {
    let saved = serde_json::from_value::<Option<ApiSavedTrack>>(item)
        .map_err(|e| log::debug!("unreadable Liked Songs entry: {e}"))
        .ok()
        .flatten()?;
    let mut track = types::track_info(saved.track?)?;
    track.added_at = saved.added_at.as_deref().and_then(types::unix_time).unwrap_or(0);
    Some(track)
}

#[derive(Default)]
struct LikedCollector {
    tracks: Vec<TrackInfo>,
    seen: HashSet<String>,
    /// Raw entries received (including null/unreadable ones).
    entries: usize,
    skipped: usize,
}

impl LikedCollector {
    /// Add one page; returns the number of raw entries in it.
    fn add(&mut self, page: ApiPage) -> usize {
        let len = page.items.len();
        for item in page.items {
            self.push(saved_song(item));
        }
        len
    }

    /// Add one raw entry.
    fn push(&mut self, song: Option<TrackInfo>) {
        self.entries += 1;
        match song {
            // Offsets can shift if the library changes mid-load; never list a song twice.
            Some(track) if self.seen.insert(track.uri.clone()) => self.tracks.push(track),
            Some(_) => {}
            None => self.skipped += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_ids_are_extracted() {
        assert_eq!(playlist_id_from_uri("spotify:playlist:3cEYpjA9oz9GiPac4AsH4n"), Some("3cEYpjA9oz9GiPac4AsH4n"));
        assert_eq!(playlist_id_from_uri("spotify:user:bob:playlist:abc123"), Some("abc123"));
        assert_eq!(
            playlist_id_from_uri("https://open.spotify.com/playlist/3cEYpjA9oz9GiPac4AsH4n?si=xyz"),
            Some("3cEYpjA9oz9GiPac4AsH4n")
        );
        assert_eq!(playlist_id_from_uri(" 3cEYpjA9oz9GiPac4AsH4n "), Some("3cEYpjA9oz9GiPac4AsH4n"));
        assert_eq!(playlist_id_from_uri("spotify:playlist:../../me"), None);
        assert_eq!(playlist_id_from_uri(""), None);
        assert!(parse_playlist_id("spotify:track:").is_err());
    }

    #[test]
    fn client_ids_are_validated() {
        assert_eq!(
            normalize_client_id("  0123456789abcdef0123456789ABCDEF \n").unwrap(),
            "0123456789abcdef0123456789ABCDEF"
        );
        assert!(normalize_client_id("").is_err());
        assert!(normalize_client_id("short").is_err());
        assert!(normalize_client_id("0123456789abcdef0123456789abcde!").is_err());
    }

    #[test]
    fn reconnect_errors_are_recognisable() {
        let err: anyhow::Error = ReconnectRequired::expired().into();
        assert!(is_reconnect_required(&err));
        assert!(err.to_string().contains("click Connect"));
        assert!(is_reconnect_required(&err.context("while loading")));
        assert!(!is_reconnect_required(&anyhow!("other")));
    }

    #[test]
    fn liked_fingerprint_changes_with_the_list() {
        let page = |json: &str| serde_json::from_str::<ApiPage>(json).unwrap();
        let a = page(r#"{"total":2,"items":[{"added_at":"2026-01-02T00:00:00Z","track":{"id":"x","uri":"spotify:track:x"}}]}"#);
        assert_eq!(liked_fingerprint_of(&a), "2:2026-01-02T00:00:00Z:x");
        let liked_again = page(r#"{"total":2,"items":[{"added_at":"2026-03-04T00:00:00Z","track":{"id":"x"}}]}"#);
        assert_ne!(liked_fingerprint_of(&a), liked_fingerprint_of(&liked_again));
        let removed = page(r#"{"total":1,"items":[{"added_at":"2026-01-02T00:00:00Z","track":{"id":"x"}}]}"#);
        assert_ne!(liked_fingerprint_of(&a), liked_fingerprint_of(&removed));
        assert_eq!(liked_fingerprint_of(&page(r#"{"items":[]}"#)), "?::");
    }

    #[test]
    fn liked_collector_dedupes_and_counts() {
        let page: ApiPage = serde_json::from_str(
            r#"{"total":3,"items":[
                {"added_at":"2026-01-02T00:00:00Z","track":{"uri":"spotify:track:a","name":"A"}},
                {"added_at":"not a time","track":{"uri":"spotify:track:a","name":"A again"}},
                {"track":null},
                42
            ]}"#,
        )
        .unwrap();
        let mut c = LikedCollector::default();
        assert_eq!(c.add(page), 4);
        assert_eq!(c.tracks.len(), 1);
        assert_eq!(c.entries, 4);
        assert_eq!(c.skipped, 2);
        assert_eq!(Some(c.tracks[0].added_at), types::unix_time("2026-01-02T00:00:00Z"));
    }

    /// Compile-time check: the public futures can be spawned on the tokio runtime.
    #[allow(dead_code, clippy::let_underscore_future)]
    fn futures_are_send(api: &WebApi, paths: &Paths, path: &Path) {
        fn is_send<T: Send>(_: T) {}
        is_send(WebApi::connect("x", paths, true));
        is_send(api.me());
        is_send(api.update_playlist_details("x", Some("n"), None));
        is_send(api.upload_playlist_cover("x", path));
        is_send(api.set_liked(&[], true));
        is_send(api.liked_tracks(&|_, _| {}));
        is_send(api.liked_tracks_since(&[], 0, &|_, _| {}));
        is_send(api.liked_fingerprint());
        is_send(api.user_id());
    }
}
