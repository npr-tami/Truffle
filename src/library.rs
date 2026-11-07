//! Reading the user's library through the librespot session (works for owned AND followed
//! playlists, unlike the 2026 Web API), with an on-disk cache keyed by playlist revision.
//!
//! # librespot 0.8.0 APIs used (checked against the crate sources)
//!
//! All requests go through `Session::spclient()` (`librespot-core-0.8.0/src/spclient.rs`),
//! which authenticates with the session's login5 token and already retries network errors
//! (`RequestStrategy::TryTimes(10)`); `fetch::retry` adds backoff for 429/5xx on top.
//!
//! | What | Call | Format |
//! |---|---|---|
//! | Rootlist (sidebar order, folders) | `SpClient::get_rootlist(from, None)` → `/playlist/v2/user/{user}/rootlist?decorate=revision,attributes,length,owner,capabilities,status_code&from=&length=` | `playlist4_external::SelectedListContent`; `contents.items` are playlist URIs plus `spotify:start-group:` / `spotify:end-group:` folder markers, `contents.meta_items` the per-item decorations |
//! | Playlist details fallback | `SpClient::request(GET, "/playlist/v2/playlist/{id}?decorate=…&from=0&length=1")` | `SelectedListContent` |
//! | Playlist items | `SpClient::get_playlist(&SpotifyId)` → `/playlist/v2/playlist/{id}`, then `?from=&length=` pages if truncated | `SelectedListContent` (`revision`, `length`, `contents.items[].uri`) |
//! | Track / episode metadata | `SpClient::get_extended_metadata(BatchedEntityRequest)` → `POST /extended-metadata/v0/extended-metadata`, extension kinds `TRACK_V4` / `EPISODE_V4`, 500 entities per request | `extended_metadata::BatchedExtensionResponse` → `Any.value` = `metadata::Track` / `metadata::Episode` |
//! | Profile / owner names | `SpClient::get_user_profile(user, Some(0), Some(0))` → `/user-profile-view/v3/profile/{user}` | JSON `{name, image_url}` |
//! | Liked Songs (read) | `SpClient::request(POST, "/collection/v2/delta", …)` with the last sync token, else `POST /collection/v2/paging` (300 a page); fallback `SpClient::get_context("spotify:user:{user}:collection")` + `SpClient::get_next_page(url)` | collection2v2 protobuf (hand-coded in `wire.rs`); `context::Context` (protobuf parsed by librespot) / JSON pages |
//! | Liked Songs (write) | `SpClient::request(POST, "/collection/v2/write", …)`, sent by the like queue (`likes.rs`) | collection2v2 `WriteRequest`, set `"collection"` |
//! | Market for playability | `Session::country()`, `Session::get_user_attribute("catalogue")`, `Session::filter_explicit_content()` | same rules as librespot's player (`AudioItem::get_file`) |
//!
//! `librespot_metadata::Playlist::get` / `Track::get` are not used: their conversions fail the
//! whole playlist on a single odd item (local files, unusual dates), and `Track::get` is one
//! request per track. Raw protobuf messages are read with their default-returning accessors.
//!
//! Liked Songs are read through the collection service first: context-resolve has the server
//! resolve the whole collection in its first request, which times out (504) at tens of thousands
//! of songs, while the collection service pages with a size Truffle picks, retries a page on its
//! own, gives the dates songs were liked, and a sync token. With that token kept beside the saved
//! copy, a later read asks only for the changes since (one request when nothing changed).
//!
//! When the session route for Liked Songs fails, the optional Web API client (`set_webapi`) is
//! used instead. That route has no revision, so one cheap request (the total plus the newest
//! entry) tells whether the saved copy is still current; when it isn't, the newest pages are read
//! until they meet the saved copy and the rest of it is kept, and concurrent callers share one
//! download.

mod cache;
mod convert;
mod editing;
mod edits;
mod fetch;
mod friends;
pub mod likes;
mod pathfinder;
mod profiles;
pub mod selftest;
mod wire;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use futures::stream::{self, StreamExt};
use librespot_core::{Error as CoreError, Session, error::ErrorKind};
use librespot_protocol::playlist4_external::{ListAttributes, SelectedListContent};

use crate::config::Paths;
use crate::model::{PlaylistSummary, Source, TrackInfo, UserProfile};
use crate::webapi::WebApi;

use cache::CachedList;
use convert::Market;
pub use editing::Added;
use fetch::LikedSet;

/// Progress callback: (tracks loaded so far, total if known).
pub type Progress<'a> = &'a (dyn Fn(usize, Option<usize>) + Send + Sync);

/// Concurrent playlist-detail and profile lookups.
const DETAIL_CONCURRENCY: usize = 6;
/// `liked_uris()` and `tracks(LikedSongs)` share one fetch of the URI list for this long.
const LIKED_MEMO_TTL: Duration = Duration::from_secs(30);
/// A reachability check that gets no answer within this long counts as "unreachable".
const REACHABILITY_TIMEOUT: Duration = Duration::from_secs(8);

const NEED_WEBAPI_LIKED: &str = "Connect the editing feature in Settings to see Liked Songs.";

#[derive(Clone)]
pub struct Library {
    inner: Arc<Inner>,
}

struct Inner {
    paths: Paths,
    session: RwLock<Session>,
    webapi: RwLock<Option<WebApi>>,
    state: Mutex<State>,
    /// Serializes cache file writes (each target shares one temp file name).
    write_lock: Arc<Mutex<()>>,
    /// Liked Songs from the Web API fallback. Async lock: concurrent callers (`liked_uris` at login
    /// and `tracks(LikedSongs)` reopening the list) wait for one download instead of each paging
    /// through the whole library.
    webapi_liked: tokio::sync::Mutex<Option<WebLiked>>,
    /// Liked Songs reads through the session take turns, for the same reason.
    liked_read: tokio::sync::Mutex<()>,
    /// Header details of the album and artist pages loaded so far.
    browse_info: Mutex<HashMap<Source, crate::model::SourceInfo>>,
    /// Times each song was played (the controller's count), for Truffle's mixes.
    play_counts: Mutex<Arc<HashMap<String, u32>>>,
    /// Playlist changes go out one at a time (two quick adds would otherwise clash).
    write_gate: tokio::sync::Mutex<()>,
    /// Each listen-along link's writes take turns (see `write_channel`).
    channel_gates: Gates,
}

/// One async lock per key, made when first asked for and let go of once nobody holds or waits
/// for it: writes for the same key take turns, writes for different keys don't wait.
#[derive(Default)]
struct Gates(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl Gates {
    fn gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = lock(&self.0);
        // Only the map holds these: nobody writes to them now.
        gates.retain(|_, g| Arc::strong_count(g) > 1);
        gates.entry(key.to_owned()).or_default().clone()
    }

    async fn turn(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.gate(key).lock_owned().await
    }
}

/// The last Liked Songs list read through the Web API.
struct WebLiked {
    /// [`WebApi::liked_fingerprint`] the list was read under (also its cache revision).
    revision: String,
    checked: Instant,
    /// `State::liked_writes` when it was read.
    writes: u64,
    tracks: Arc<Vec<TrackInfo>>,
}

#[derive(Default)]
struct State {
    /// Account the in-memory state belongs to.
    username: String,
    lru: cache::Lru,
    /// Normalized playlist URI -> latest known revision (hex).
    revisions: HashMap<String, String>,
    /// Owner username -> display name.
    owners: HashMap<String, String>,
    /// Owners whose display name could not be fetched this run (not retried until restart).
    owners_failed: HashSet<String>,
    liked: Option<(Instant, Arc<LikedSet>)>,
    /// Likes and unlikes that went through. A Liked Songs read that overlapped one may or may not
    /// contain it, so it is not memoized.
    liked_writes: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn same_user(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn require_username(session: &Session) -> Result<String> {
    let username = session.username();
    if username.is_empty() {
        bail!("Truffle isn't connected to Spotify yet. Please log in first.");
    }
    Ok(username)
}

fn reason(err: &CoreError, session: &Session) -> &'static str {
    if session.is_invalid() {
        return "The connection to Spotify was lost. Truffle will reconnect; please try again in a moment.";
    }
    match err.kind {
        ErrorKind::Unavailable | ErrorKind::DeadlineExceeded | ErrorKind::Unknown => {
            "Spotify could not be reached. Check your internet connection and try again."
        }
        ErrorKind::ResourceExhausted => "Spotify asked Truffle to slow down. Please wait a minute and try again.",
        ErrorKind::Aborted => "It changed while it was loading. Please try again.",
        ErrorKind::Unauthenticated => "Spotify did not accept your login. Please log out and log in again.",
        ErrorKind::PermissionDenied => "Spotify says your account doesn't have access to it.",
        ErrorKind::NotFound => "Spotify could not find it; it may have been deleted.",
        _ => "Spotify sent an answer Truffle didn't understand. Please try again later.",
    }
}

/// Plain-language error for the UI, keeping the technical cause in the chain for logs.
fn friendly(action: &str, err: CoreError, session: &Session) -> anyhow::Error {
    let msg = format!("{action} {}", reason(&err, session));
    anyhow::Error::new(err).context(msg)
}

/// Network trouble (as opposed to "this route isn't supported").
fn is_network(err: &CoreError, session: &Session) -> bool {
    session.is_invalid() || fetch::is_transient(err.kind)
}

fn normalized(uri: &str) -> String {
    convert::normalize_playlist_uri(uri).unwrap_or_else(|| uri.to_owned())
}

struct Draft {
    summary: PlaylistSummary,
    owner: String,
}

fn draft(uri: &str, attrs: &ListAttributes, length: Option<i32>, revision: &[u8], owner: &str, me: &str) -> Draft {
    let name = attrs.name().trim();
    Draft {
        summary: PlaylistSummary {
            uri: uri.to_owned(),
            name: if name.is_empty() { "Untitled playlist".to_owned() } else { name.to_owned() },
            description: convert::clean_description(attrs.description()),
            owner_name: owner.to_owned(),
            image_url: convert::playlist_image(attrs),
            track_count: length.map(|l| l.max(0) as u32),
            revision: convert::hex(revision),
            editable: (!owner.is_empty() && same_user(owner, me)) || attrs.collaborative(),
            owned: !owner.is_empty() && same_user(owner, me),
            owner_id: owner.to_owned(),
        },
        owner: owner.to_owned(),
    }
}

fn draft_from_list(uri: &str, msg: &SelectedListContent, me: &str) -> Draft {
    draft(
        uri,
        msg.attributes.get_or_default(),
        msg.has_length().then(|| msg.length()),
        msg.revision(),
        msg.owner_username(),
        me,
    )
}

impl Library {
    pub fn new(session: Session, paths: Paths) -> Self {
        if let Err(e) = std::fs::create_dir_all(paths.tracks_cache_dir()) {
            log::warn!("could not create {}: {e}", paths.tracks_cache_dir().display());
        }
        let username = session.username();
        Self {
            inner: Arc::new(Inner {
                paths,
                session: RwLock::new(session),
                webapi: RwLock::new(None),
                state: Mutex::new(State { username, ..Default::default() }),
                write_lock: Arc::new(Mutex::new(())),
                webapi_liked: tokio::sync::Mutex::new(None),
                liked_read: tokio::sync::Mutex::new(()),
                browse_info: Mutex::new(HashMap::new()),
                play_counts: Mutex::new(Arc::new(HashMap::new())),
                write_gate: tokio::sync::Mutex::new(()),
                channel_gates: Gates::default(),
            }),
        }
    }

    /// Swap in a reconnected session. Clones of this `Library` share the change. If a different
    /// account logged in, the in-memory caches are dropped.
    pub fn set_session(&mut self, session: Session) {
        let username = session.username();
        {
            let mut st = lock(&self.inner.state);
            if !username.is_empty() {
                if !st.username.is_empty() && !same_user(&st.username, &username) {
                    log::info!("library: account changed, clearing in-memory caches");
                    st.lru.clear();
                    st.revisions.clear();
                    st.owners_failed.clear();
                    st.liked = None;
                    if let Ok(mut memo) = self.inner.webapi_liked.try_lock() {
                        *memo = None;
                    }
                }
                st.username = username;
            }
        }
        *self.inner.session.write().unwrap_or_else(|e| e.into_inner()) = session;
    }

    /// Optional Web API client, used as a fallback where the session route is unavailable.
    pub fn set_webapi(&mut self, api: Option<WebApi>) {
        *self.inner.webapi.write().unwrap_or_else(|e| e.into_inner()) = api;
    }

    fn session(&self) -> Session {
        self.inner.session.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn webapi(&self) -> Option<WebApi> {
        self.inner.webapi.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether Spotify's servers answer right now: one small request through the session's HTTP
    /// client, the same route track loads take. librespot only notices a dead connection when its
    /// keep-alive times out (20 to 80 s), while track loads fail at once; this tells a network
    /// outage apart from a track that can't be played.
    pub async fn spotify_reachable(&self) -> bool {
        let session = self.session();
        if session.is_invalid() {
            return false;
        }
        let username = session.username();
        let probe = session.spclient().get_user_profile(&username, Some(0), Some(0));
        match tokio::time::timeout(REACHABILITY_TIMEOUT, probe).await {
            Ok(Ok(_)) => true,
            // Any answer from Spotify, even an error status, means the network works.
            Ok(Err(e)) => {
                let down = session.is_invalid() || matches!(e.kind, ErrorKind::Unavailable | ErrorKind::DeadlineExceeded);
                log::info!("reachability check: {e} ({})", if down { "unreachable" } else { "reachable" });
                !down
            }
            Err(_) => {
                log::info!("reachability check: no answer within {REACHABILITY_TIMEOUT:?}");
                false
            }
        }
    }

    /// The Spotify username of the session.
    pub fn username(&self) -> String {
        self.session().username()
    }

    pub async fn profile(&self) -> Result<UserProfile> {
        let session = self.session();
        let username = require_username(&session)?;
        let mut profile = UserProfile { username: username.clone(), display_name: username.clone(), image_url: None, premium: None };
        match fetch::user_profile(&session, &username).await {
            Ok(p) => {
                if let Some(name) = p.name {
                    profile.display_name = name;
                }
                profile.image_url = p.image_url;
            }
            Err(e) => {
                log::info!("profile through the session unavailable: {e}");
                if let Some(api) = self.webapi() {
                    match api.me().await {
                        Ok(me) => {
                            if !me.display_name.trim().is_empty() {
                                profile.display_name = me.display_name;
                            }
                            profile.image_url = me.image_url;
                        }
                        Err(e) => log::info!("profile through the Web API unavailable: {e:#}"),
                    }
                }
            }
        }
        lock(&self.inner.state).owners.insert(username, profile.display_name.clone());
        Ok(profile)
    }

    /// Last saved playlist list (`<cache>/playlists.json`) for the logged-in account, without
    /// network, for an instant sidebar at startup. `None` if nothing is cached or the cache
    /// belongs to another account. Synchronous file read. Also seeds the owner-name map.
    pub fn cached_playlists(&self) -> Option<Vec<PlaylistSummary>> {
        let username = self.session().username();
        let file = cache::read_playlists_cache(&cache::playlists_cache_path(&self.inner.paths.cache))?;
        if !username.is_empty() && !same_user(&file.username, &username) {
            return None;
        }
        let mut st = lock(&self.inner.state);
        for (k, v) in &file.owners {
            st.owners.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Some(file.playlists)
    }

    /// All playlists in the user's library (owned and followed), in the user's sidebar order.
    pub async fn playlists(&self) -> Result<Vec<PlaylistSummary>> {
        let session = self.session();
        let username = require_username(&session)?;
        let cache_path = cache::playlists_cache_path(&self.inner.paths.cache);

        let root = fetch::rootlist(&session)
            .await
            .map_err(|e| friendly("Couldn't load your playlists.", e, &session))?;

        // Last saved list (also seeds the owner-name map): fallback for entries whose details
        // can't be fetched right now.
        let previous: Vec<PlaylistSummary> = {
            let this = self.clone();
            tokio::task::spawn_blocking(move || this.cached_playlists()).await.ok().flatten().unwrap_or_default()
        };

        let mut drafts: Vec<Option<Draft>> = root
            .entries
            .iter()
            .map(|e| {
                e.meta.as_ref().map(|m| {
                    draft(
                        &e.uri,
                        m.attributes.get_or_default(),
                        m.has_length().then(|| m.length()),
                        m.revision(),
                        m.owner_username(),
                        &username,
                    )
                })
            })
            .collect();

        // Rootlist entries without decorations: fetch their details, a few at a time.
        let missing: Vec<usize> = (0..drafts.len()).filter(|&i| drafts[i].is_none()).collect();
        if !missing.is_empty() {
            log::info!("fetching details for {} playlists individually", missing.len());
            let session_ref = &session;
            let entries = &root.entries;
            let results: Vec<(usize, Result<SelectedListContent, CoreError>)> = stream::iter(missing)
                .map(|i| async move {
                    let result = match convert::playlist_id(&entries[i].uri) {
                        Some(id) => fetch::playlist_header(session_ref, &id).await,
                        None => Err(CoreError::invalid_argument("not a playlist")),
                    };
                    (i, result)
                })
                .buffer_unordered(DETAIL_CONCURRENCY)
                .collect()
                .await;
            for (i, result) in results {
                let uri = &root.entries[i].uri;
                drafts[i] = Some(match result {
                    Ok(msg) => draft_from_list(uri, &msg, &username),
                    Err(e) => {
                        log::warn!("details for {uri} unavailable: {e}");
                        match previous.iter().find(|p| &p.uri == uri) {
                            Some(p) => Draft { summary: p.clone(), owner: String::new() },
                            None => draft(uri, &ListAttributes::new(), None, &[], "", &username),
                        }
                    }
                });
            }
        }
        let drafts: Vec<Draft> = drafts.into_iter().flatten().collect();

        drop(previous);

        // Owner display names: from memory (seeded from the cache file), else the profile service.
        let unknown: Vec<String> = {
            let st = lock(&self.inner.state);
            let mut seen = HashSet::new();
            drafts
                .iter()
                .map(|d| d.owner.as_str())
                .filter(|o| !o.is_empty() && *o != "spotify")
                .filter(|o| !st.owners.contains_key(*o) && !st.owners_failed.contains(*o))
                .filter(|o| seen.insert(o.to_string()))
                .map(str::to_owned)
                .collect()
        };
        if !unknown.is_empty() {
            let session_ref = &session;
            let names: Vec<(String, Result<fetch::ProfileInfo, CoreError>)> = stream::iter(unknown)
                .map(|owner| async move {
                    let r = fetch::user_profile(session_ref, &owner).await;
                    (owner, r)
                })
                .buffer_unordered(DETAIL_CONCURRENCY)
                .collect()
                .await;
            let mut st = lock(&self.inner.state);
            for (owner, r) in names {
                match r {
                    Ok(fetch::ProfileInfo { name: Some(name), .. }) => {
                        st.owners.insert(owner, name);
                    }
                    Ok(_) => {
                        st.owners.insert(owner.clone(), owner);
                    }
                    Err(e) => {
                        log::debug!("no display name for {owner}: {e}");
                        st.owners_failed.insert(owner);
                    }
                }
            }
        }

        let (summaries, owners) = {
            let mut st = lock(&self.inner.state);
            let summaries: Vec<PlaylistSummary> = drafts
                .into_iter()
                .map(|d| {
                    let mut s = d.summary;
                    if !d.owner.is_empty() {
                        s.owner_name = match st.owners.get(&d.owner) {
                            Some(name) => name.clone(),
                            None if d.owner == "spotify" => "Spotify".to_owned(),
                            None => d.owner.clone(),
                        };
                    }
                    s
                })
                .collect();
            st.revisions = summaries
                .iter()
                .filter(|s| !s.revision.is_empty())
                .map(|s| (s.uri.clone(), s.revision.clone()))
                .collect();
            (summaries, st.owners.clone())
        };

        let file = cache::PlaylistsCacheFile {
            username,
            revision: convert::hex(&root.revision),
            saved_at: cache::now_secs(),
            owners,
            playlists: summaries.clone(),
        };
        let write_lock = self.inner.write_lock.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = lock(&write_lock);
            if let Err(e) = cache::write_playlists_cache(&cache_path, &file) {
                log::warn!("could not save the playlist list: {e:#}");
            }
        });

        Ok(summaries)
    }

    fn lru_lookup(&self, source: &Source, username: &str) -> Option<Option<CachedList>> {
        let entry = lock(&self.inner.state).lru.get(&source.cache_key())?;
        Some(Self::belongs_to(&entry, source, username).then_some(entry))
    }

    fn belongs_to(entry: &CachedList, source: &Source, username: &str) -> bool {
        // Playlist contents are the same for everyone; Liked Songs are personal.
        !matches!(source, Source::LikedSongs) || username.is_empty() || same_user(&entry.username, username)
    }

    /// LRU, then disk (synchronous).
    fn cached_entry(&self, source: &Source) -> Option<CachedList> {
        let username = self.session().username();
        if let Some(hit) = self.lru_lookup(source, &username) {
            return hit;
        }
        let path = cache::track_cache_path(&self.inner.paths.tracks_cache_dir(), source);
        let file = cache::read_track_cache(&path)?;
        let entry = CachedList {
            key: source.cache_key(),
            revision: file.revision,
            username: file.username,
            sync_token: file.sync_token,
            tracks: Arc::new(file.tracks),
        };
        if !Self::belongs_to(&entry, source, &username) {
            return None;
        }
        lock(&self.inner.state).lru.put(entry.clone());
        Some(entry)
    }

    /// Like `cached_entry`, with the disk read off the async worker threads.
    async fn cached_entry_async(&self, source: &Source) -> Option<CachedList> {
        if let Some(hit) = self.lru_lookup(source, &self.session().username()) {
            return hit;
        }
        let this = self.clone();
        let source = source.clone();
        tokio::task::spawn_blocking(move || this.cached_entry(&source)).await.ok().flatten()
    }

    /// A song's details from the track lists kept in memory, without network.
    pub fn cached_track(&self, uri: &str) -> Option<TrackInfo> {
        lock(&self.inner.state).lru.find_track(uri)
    }

    /// Songs' details by uri, in the same order: from the track lists kept in memory, the rest
    /// fetched (one batched request). One Spotify can't describe has no artists.
    async fn tracks_known_first(&self, session: &Session, uris: &[String]) -> Result<Vec<TrackInfo>> {
        let known = lock(&self.inner.state).lru.find_tracks(uris);
        let reuse = known.iter().map(|t| (t.uri.as_str(), t)).collect();
        build_tracks(session, uris, &reuse, &|_, _| {}).await.map_err(|e| friendly("Couldn't look up the songs.", e, session))
    }

    /// Instant read of the last cached track list (may be stale), without network.
    pub fn cached_tracks(&self, source: &Source) -> Option<Arc<Vec<TrackInfo>>> {
        self.cached_entry(source).map(|e| e.tracks)
    }

    /// Remember a freshly built list: LRU, revision map, and disk (in the background).
    fn store(&self, source: &Source, revision: String, sync_token: String, username: String, tracks: Arc<Vec<TrackInfo>>) {
        {
            let mut st = lock(&self.inner.state);
            st.lru.put(CachedList {
                key: source.cache_key(),
                revision: revision.clone(),
                username: username.clone(),
                sync_token: sync_token.clone(),
                tracks: tracks.clone(),
            });
            if let Source::Playlist(uri) = source
                && !revision.is_empty()
            {
                st.revisions.insert(normalized(uri), revision.clone());
            }
        }
        let path = cache::track_cache_path(&self.inner.paths.tracks_cache_dir(), source);
        let write_lock = self.inner.write_lock.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = lock(&write_lock);
            if let Err(e) = cache::write_track_cache(&path, &revision, &username, &sync_token, &tracks) {
                log::warn!("could not save the track list cache {}: {e:#}", path.display());
            }
        });
    }

    /// A song's details read again from Spotify, past every saved copy: whether it can be played
    /// now (one Spotify no longer has audio for comes back unplayable).
    pub async fn recheck(&self, uri: &str) -> Result<TrackInfo> {
        let session = self.session();
        let uris = [uri.to_owned()];
        let tracks = build_tracks(&session, &uris, &HashMap::new(), &|_, _| {})
            .await
            .map_err(|e| friendly("Couldn't look up the song again.", e, &session))?;
        tracks.into_iter().next().ok_or_else(|| anyhow!("Spotify sent no details for {uri}"))
    }

    /// `uri` can't be played any more: greyed out in `source`'s saved list (memory and disk), so
    /// the list shows it greyed out and playing skips it. A list read again after it changed
    /// looks it up afresh, as it does every unplayable song. The new list, if the song was
    /// playable in it.
    pub fn grey_out(&self, source: &Source, uri: &str) -> Option<Arc<Vec<TrackInfo>>> {
        let entry = self.cached_entry(source)?;
        let tracks = Arc::new(greyed_out(&entry.tracks, uri)?);
        self.store(source, entry.revision, entry.sync_token, entry.username, tracks.clone());
        Some(tracks)
    }

    /// Full track list for a source. Uses the cache when the revision is unchanged unless
    /// `force_refresh`. Fetches metadata in large batches; reports progress.
    pub async fn tracks(&self, source: &Source, force_refresh: bool, progress: Progress<'_>) -> Result<Arc<Vec<TrackInfo>>> {
        match source {
            Source::Playlist(uri) => self.playlist_tracks(source, uri, force_refresh, true, progress).await,
            Source::LikedSongs => self.liked_tracks(force_refresh, progress).await,
            Source::Album(uri) => self.album_tracks(source, uri, force_refresh, progress).await,
            Source::Artist(uri) => self.artist_tracks(source, uri, force_refresh, progress).await,
            Source::Mix(id) => {
                let liked = self.liked_tracks(false, progress).await?;
                let counts = lock(&self.inner.play_counts).clone();
                Ok(Arc::new(mix_of(id, &liked, &counts)))
            }
        }
    }

    /// The play counts Truffle's mixes are made with.
    pub fn set_play_counts(&self, counts: HashMap<String, u32>) {
        *lock(&self.inner.play_counts) = Arc::new(counts);
    }

    /// Truffle's mixes that have songs, for Home: the fixed ones, then one for each of `artists`
    /// (uri, name).
    pub async fn mixes(&self, artists: &[(String, String)]) -> Vec<crate::model::MixInfo> {
        let quiet = |_: usize, _: Option<usize>| {};
        let Ok(liked) = self.tracks(&Source::LikedSongs, false, &quiet).await else { return Vec::new() };
        let counts = lock(&self.inner.play_counts).clone();
        let fixed = [
            (MIX_ON_REPEAT.to_owned(), "On repeat".to_owned(), "The songs you play most".to_owned()),
            (MIX_FRESH.to_owned(), "Fresh likes".to_owned(), "The songs you liked last".to_owned()),
            (MIX_FORGOTTEN.to_owned(), "Forgotten favourites".to_owned(), "Liked a while ago, not played since".to_owned()),
        ];
        let by_artist = artists.iter().map(|(uri, name)| (format!("{MIX_ARTIST}{uri}"), format!("{name} mix"), format!("Your liked songs by {name}")));
        fixed
            .into_iter()
            .chain(by_artist)
            .filter_map(|(id, name, description)| {
                let songs = mix_of(&id, &liked, &counts);
                (!songs.is_empty()).then(|| crate::model::MixInfo {
                    image: songs.iter().find_map(|t| t.cover_large.clone().or_else(|| t.cover_small.clone())),
                    songs: songs.len(),
                    id,
                    name,
                    description,
                })
            })
            .collect()
    }

    /// The albums and artists of Liked Songs, newest first: what the Albums and Artists pages show
    /// (Spotify's saved albums and followed artists would need more permissions).
    pub async fn collection(&self) -> Result<crate::model::Collection> {
        let quiet = |_: usize, _: Option<usize>| {};
        let tracks = self.tracks(&Source::LikedSongs, false, &quiet).await?;
        Ok(collect(&tracks))
    }

    /// Albums and singles out in the last `within_days` by these artists, newest first. Each artist's
    /// newest few releases are looked at (Spotify lists them newest first).
    pub async fn new_releases(&self, artist_uris: Vec<String>, within_days: i64) -> Vec<crate::model::Release> {
        use librespot_metadata::{Album, Artist, Metadata};
        let session = self.session();
        let today = days_since_epoch_now();
        let ids: Vec<librespot_core::SpotifyUri> = stream::iter(artist_uris)
            .map(|uri| {
                let session = session.clone();
                async move {
                    let id = librespot_core::SpotifyUri::from_uri(&uri).ok()?;
                    let artist = Artist::get(&session, &id).await.ok()?;
                    let newest: Vec<librespot_core::SpotifyUri> =
                        artist.albums_current().take(2).chain(artist.singles_current().take(3)).cloned().collect();
                    Some(newest)
                }
            })
            .buffered(6)
            .filter_map(|x| async move { x })
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();
        let mut out: Vec<crate::model::Release> = stream::iter(ids)
            .map(|id| {
                let session = session.clone();
                async move {
                    let album = Album::get(&session, &id).await.ok()?;
                    let date = album.date.as_utc();
                    let days_ago = today - days_from_civil(i64::from(date.year()), i64::from(u8::from(date.month())), i64::from(date.day()));
                    if !(0..=within_days).contains(&days_ago) {
                        return None;
                    }
                    let kind = if album.tracks().count() <= 3 { "Single" } else { "Album" };
                    Some(crate::model::Release {
                        uri: id.to_uri().ok()?,
                        name: album.name.clone(),
                        artist: album.artists.first().map(|a| a.name.clone()).unwrap_or_default(),
                        image: album.covers.iter().min_by_key(|i| (i.width - 300).abs()).and_then(|i| convert::image_url(&i.id.0)),
                        kind,
                        days_ago,
                    })
                }
            })
            .buffered(6)
            .filter_map(|x| async move { x })
            .collect()
            .await;
        out.sort_by_key(|r| r.days_ago);
        out.dedup_by(|a, b| a.uri == b.uri);
        out
    }

    /// Pictures of these artists (by URI), where Spotify has one.
    pub async fn artist_pictures(&self, uris: Vec<String>) -> HashMap<String, String> {
        let session = self.session();
        fetch::artist_details(&session, &uris).await.into_iter().filter_map(|(uri, (_, image))| Some((uri, image?))).collect()
    }

    /// An artist's likes from the Liked Songs list as it was last read (loading it if it isn't in
    /// memory), with `liked` (the likes as they are now, confirmed or not) applied: a like shows
    /// at once, before Spotify's list has it.
    pub async fn artist_likes(&self, artist: &str, liked: &HashSet<String>) -> Result<crate::model::ArtistLikes> {
        let quiet = |_: usize, _: Option<usize>| {};
        let list = match self.cached_entry_async(&Source::LikedSongs).await {
            Some(c) => c.tracks,
            None => self.tracks(&Source::LikedSongs, false, &quiet).await?,
        };
        let (songs, _) = liked_now(&list, liked, |u| self.cached_track(u));
        Ok(artist_likes_of(artist, &songs))
    }

    /// An artist's likes without waiting: from the Liked Songs list in memory and `liked` as it
    /// is now. `None` when the list isn't in memory yet. The bool: every like was counted (false
    /// when a song liked since the list was read has no details in memory: it is read again).
    pub fn artist_likes_now(&self, artist: &str, liked: &HashSet<String>) -> Option<(crate::model::ArtistLikes, bool)> {
        let list = self.lru_lookup(&Source::LikedSongs, &self.session().username())??.tracks;
        let (songs, whole) = liked_now(&list, liked, |u| self.cached_track(u));
        Some((artist_likes_of(artist, &songs), whole))
    }

    /// One of Truffle's mixes without waiting: made from the Liked Songs list kept in memory or on
    /// disk, with `liked` (the likes as they are now) applied once it is known. `None` when there
    /// is no saved list yet, or `id` isn't a mix made from Liked Songs. Spotify's list can take
    /// minutes to read again for a big library, so an opened mix shows this first.
    pub fn mix_now(&self, id: &str, liked: &HashSet<String>) -> Option<Arc<Vec<TrackInfo>>> {
        if ![MIX_ON_REPEAT, MIX_FRESH, MIX_FORGOTTEN].contains(&id) && !id.starts_with(MIX_ARTIST) {
            return None;
        }
        let list = self.cached_entry(&Source::LikedSongs)?.tracks;
        let counts = lock(&self.inner.play_counts).clone();
        if liked.is_empty() {
            return Some(Arc::new(mix_of(id, &list, &counts)));
        }
        let (songs, _) = liked_now(&list, liked, |u| self.cached_track(u));
        Some(Arc::new(mix_of(id, &songs, &counts)))
    }

    /// Tests: `tracks` kept in memory as the list last read for `source`.
    #[cfg(test)]
    pub fn keep_in_memory(&self, source: &Source, tracks: Vec<TrackInfo>) {
        lock(&self.inner.state).lru.put(CachedList { key: source.cache_key(), revision: String::new(), username: String::new(), sync_token: String::new(), tracks: Arc::new(tracks) });
    }

    /// A user's profile page: name, picture, follower counts, public playlists (up to 50) and
    /// recently played artists.
    pub async fn user_page(&self, username: &str) -> Result<crate::model::UserPage> {
        let session = self.session();
        require_username(&session)?;
        let bytes = fetch::retry("profile", || {
            let session = session.clone();
            async move { session.spclient().get_user_profile(username, Some(50), Some(10)).await }
        })
        .await
        .map_err(|e| friendly("Couldn't load the profile.", e, &session))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| anyhow!("Spotify sent a profile Truffle didn't understand."))?;
        let mut page = profiles::user_page(username, &v);
        // Not in the profile: asked the way the web player asks.
        if page.is_following.is_none() && !same_user(username, &session.username()) {
            page.is_following = self.follows_user(username).await.ok().flatten();
        }
        Ok(page)
    }

    /// Whether the logged-in user follows this user (`None`: Spotify didn't say).
    pub async fn follows_user(&self, username: &str) -> Result<Option<bool>> {
        let session = self.session();
        let uri = format!("spotify:user:{username}");
        let answer = fetch::pathfinder(&session, "isFollowingUsers", pathfinder::FOLLOW_USERS_HASH, serde_json::json!({ "uris": [uri] }))
            .await
            .map_err(|e| friendly("Couldn't check the follow.", e, &session))?;
        Ok(answer.and_then(|a| pathfinder::is_following_user(&a, &uri)))
    }

    /// Follows (or unfollows) a user, as Spotify's own apps do.
    pub async fn set_following_user(&self, username: &str, follow: bool) -> Result<()> {
        let session = self.session();
        require_username(&session)?;
        let operation = if follow { "followUsers" } else { "unfollowUsers" };
        let what = if follow { "Couldn't follow them." } else { "Couldn't unfollow them." };
        let answer = fetch::pathfinder(&session, operation, pathfinder::FOLLOW_USERS_HASH, serde_json::json!({ "usernames": [username] }))
            .await
            .map_err(|e| friendly(what, e, &session))?
            .ok_or_else(|| anyhow!("{what} Spotify changed how this works."))?;
        if pathfinder::follow_users_ok(&answer, operation, username) {
            Ok(())
        } else {
            log::warn!("{operation} {username}: {}", answer["errors"]);
            bail!("{what}")
        }
    }

    /// The people (and artists) a user follows, or who follow them.
    pub async fn people(&self, username: &str, followers: bool) -> Result<crate::model::People> {
        let session = self.session();
        require_username(&session)?;
        let bytes = fetch::retry("people", || {
            let session = session.clone();
            async move {
                if followers { session.spclient().get_user_followers(username).await } else { session.spclient().get_user_following(username).await }
            }
        })
        .await
        .map_err(|e| friendly(if followers { "Couldn't load the followers." } else { "Couldn't load who they follow." }, e, &session))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| anyhow!("Spotify sent a list Truffle didn't understand."))?;
        Ok(crate::model::People { username: username.to_owned(), followers, list: profiles::people(&v) })
    }

    /// The artists the user follows, followed last first.
    pub async fn followed_artists(&self) -> Result<Vec<String>> {
        let session = self.session();
        require_username(&session)?;
        fetch::followed_artist_uris(&session).await.map_err(|e| friendly("Couldn't load the artists you follow.", e, &session))
    }

    /// One item per artist, in the same order: its name and picture (left empty when Spotify won't
    /// describe it), `newest` its place in the list.
    pub async fn artist_briefs(&self, uris: Vec<String>) -> Vec<crate::model::CollectionItem> {
        let session = self.session();
        let mut details = fetch::artist_details(&session, &uris).await;
        uris.into_iter()
            .enumerate()
            .map(|(k, uri)| {
                let (name, image) = details.remove(&uri).unwrap_or_default();
                crate::model::CollectionItem { uri, name, image, newest: k, ..Default::default() }
            })
            .collect()
    }

    /// Follows (or unfollows) an artist.
    pub async fn set_following(&self, uri: &str, follow: bool) -> Result<()> {
        if !uri.starts_with("spotify:artist:") {
            bail!("Only artists can be followed.");
        }
        let session = self.session();
        require_username(&session)?;
        let what = if follow { "Couldn't follow this artist." } else { "Couldn't unfollow this artist." };
        fetch::follow_write(&session, uri, follow).await.map_err(|e| friendly(what, e, &session))
    }

    /// Header details of an album or artist page loaded with [`Self::tracks`].
    pub fn source_info(&self, source: &Source) -> Option<crate::model::SourceInfo> {
        lock(&self.inner.browse_info).get(source).cloned()
    }

    async fn album_tracks(&self, source: &Source, uri: &str, force: bool, progress: Progress<'_>) -> Result<Arc<Vec<TrackInfo>>> {
        use librespot_metadata::{Album, Metadata};
        let session = self.session();
        let username = require_username(&session)?;
        let id = librespot_core::SpotifyUri::from_uri(uri).map_err(|_| anyhow!("This doesn't look like a Spotify album."))?;
        let album = Album::get(&session, &id).await.map_err(|e| friendly("Couldn't open this album.", e, &session))?;
        let uris: Vec<String> = album.tracks().filter_map(|t| t.to_uri().ok()).collect();
        let artist = album.artists.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ");
        let year = album.date.as_utc().year();
        let subtitle = if artist.is_empty() { format!("Album · {year}") } else { format!("{artist} · {year}") };
        let image = album.covers.iter().max_by_key(|i| i.width).and_then(|i| convert::image_url(&i.id.0));
        lock(&self.inner.browse_info).insert(
            source.clone(),
            crate::model::SourceInfo {
                source: source.clone(),
                title: album.name.clone(),
                subtitle,
                image,
                albums: Vec::new(),
                artists: album.artists.0.iter().map(|a| crate::model::ArtistRef { name: a.name.clone(), uri: a.id.to_uri().unwrap_or_default() }).collect(),
            },
        );
        self.browse_list(source, &session, username, &uris, force, progress).await
    }

    async fn artist_tracks(&self, source: &Source, uri: &str, force: bool, progress: Progress<'_>) -> Result<Arc<Vec<TrackInfo>>> {
        use librespot_metadata::{Album, Artist, Metadata};
        let session = self.session();
        let username = require_username(&session)?;
        let id = librespot_core::SpotifyUri::from_uri(uri).map_err(|_| anyhow!("This doesn't look like a Spotify artist."))?;
        let artist = Artist::get(&session, &id).await.map_err(|e| friendly("Couldn't open this artist.", e, &session))?;
        let uris: Vec<String> = artist.top_tracks.for_country(&session.country()).iter().filter_map(|t| t.to_uri().ok()).collect();
        let image = artist
            .portraits
            .iter()
            .chain(artist.portrait_group.iter())
            .max_by_key(|i| i.width)
            .and_then(|i| convert::image_url(&i.id.0));

        // Their releases, newest first as Spotify lists them: albums, then singles.
        let release_ids: Vec<librespot_core::SpotifyUri> =
            artist.albums_current().chain(artist.singles_current()).take(24).cloned().collect();
        let albums: Vec<crate::model::Card> = stream::iter(release_ids)
            .map(|rid| {
                let session = session.clone();
                async move {
                    let album = Album::get(&session, &rid).await.ok()?;
                    let uri = rid.to_uri().ok()?;
                    let kind = if album.tracks().count() <= 3 { "Single" } else { "Album" };
                    Some(crate::model::Card {
                        uri,
                        name: album.name.clone(),
                        subtitle: format!("{kind} · {}", album.date.as_utc().year()),
                        image: album.covers.iter().min_by_key(|i| (i.width - 300).abs()).and_then(|i| convert::image_url(&i.id.0)),
                    })
                }
            })
            .buffered(6)
            .filter_map(|c| async move { c })
            .collect()
            .await;
        lock(&self.inner.browse_info).insert(
            source.clone(),
            crate::model::SourceInfo { source: source.clone(), title: artist.name.clone(), subtitle: "Artist · top songs".to_owned(), image, albums, artists: Vec::new() },
        );
        self.browse_list(source, &session, username, &uris, force, progress).await
    }

    /// Song details for an album or artist page, reusing the saved copy's.
    async fn browse_list(
        &self,
        source: &Source,
        session: &Session,
        username: String,
        uris: &[String],
        force: bool,
        progress: Progress<'_>,
    ) -> Result<Arc<Vec<TrackInfo>>> {
        let cached = if force { None } else { self.cached_entry_async(source).await };
        let revision = convert::fingerprint(uris.iter().map(String::as_str));
        if let Some(c) = cached.as_ref().filter(|c| c.revision == revision) {
            progress(c.tracks.len(), Some(c.tracks.len()));
            return Ok(c.tracks.clone());
        }
        let reuse = reusable(cached.as_ref());
        let tracks = build_tracks(session, uris, &reuse, progress)
            .await
            .map_err(|e| friendly("Couldn't load these songs.", e, session))?;
        drop(reuse);
        let tracks = Arc::new(tracks);
        self.store(source, revision, String::new(), username, tracks.clone());
        Ok(tracks)
    }

    /// A playlist's songs as Spotify has them now, in order with their added dates: like
    /// [`Self::tracks`], but always asking Spotify which revision it is at (the playlist list's
    /// may be older). The saved copy is used when it is that revision.
    pub async fn playlist_now(&self, uri: &str, progress: Progress<'_>) -> Result<Arc<Vec<TrackInfo>>> {
        self.playlist_tracks(&Source::Playlist(uri.to_owned()), uri, false, false, progress).await
    }

    /// `trust_known`: a saved copy at the revision the playlist list gave is used without asking.
    async fn playlist_tracks(&self, source: &Source, uri: &str, force: bool, trust_known: bool, progress: Progress<'_>) -> Result<Arc<Vec<TrackInfo>>> {
        let session = self.session();
        let username = require_username(&session)?;
        let id = convert::playlist_id(uri).ok_or_else(|| anyhow!("This doesn't look like a Spotify playlist."))?;
        let cached = if force { None } else { self.cached_entry_async(source).await };

        if trust_known && let Some(c) = &cached {
            let known = lock(&self.inner.state).revisions.get(&normalized(uri)).cloned();
            if known.is_some_and(|k| !k.is_empty() && k == c.revision) && has_dates(&c.tracks) {
                progress(c.tracks.len(), Some(c.tracks.len()));
                return Ok(c.tracks.clone());
            }
        }

        let items = fetch::playlist_items(&session, &id)
            .await
            .map_err(|e| friendly("Couldn't open this playlist.", e, &session))?;
        self.tracks_of_items(&session, username, uri, cached, &items, progress).await
    }

    /// A playlist's songs with their details, from its rows just read (`items`), as
    /// [`Self::playlist_now`] has them: the saved copy when it is that revision, else built with
    /// only what the saved copy lacks fetched, and saved.
    async fn playlist_details(&self, session: &Session, uri: &str, items: &fetch::PlaylistItems) -> Result<Arc<Vec<TrackInfo>>> {
        let username = require_username(session)?;
        let source = Source::Playlist(uri.to_owned());
        let cached = self.cached_entry_async(&source).await;
        self.tracks_of_items(session, username, uri, cached, items, &|_, _| {}).await
    }

    /// A playlist's songs from its rows just read: the saved copy (`cached`) when it is that
    /// revision, else built reusing its details, and saved.
    async fn tracks_of_items(
        &self,
        session: &Session,
        username: String,
        uri: &str,
        cached: Option<CachedList>,
        items: &fetch::PlaylistItems,
        progress: Progress<'_>,
    ) -> Result<Arc<Vec<TrackInfo>>> {
        let revision = convert::hex(&items.revision);
        if let Some(c) = &cached
            && !revision.is_empty()
            && c.revision == revision
            && has_dates(&c.tracks)
        {
            lock(&self.inner.state).revisions.insert(normalized(uri), revision);
            progress(c.tracks.len(), Some(c.tracks.len()));
            return Ok(c.tracks.clone());
        }

        let reuse = reusable(cached.as_ref());
        let mut tracks = build_tracks(session, &items.uris, &reuse, progress)
            .await
            .map_err(|e| friendly("Couldn't load the songs in this playlist.", e, session))?;
        drop(reuse);
        if tracks.len() == items.added.len() {
            for (t, &at) in tracks.iter_mut().zip(&items.added) {
                t.added_at = at;
            }
        }
        let tracks = Arc::new(tracks);
        self.store(&Source::Playlist(uri.to_owned()), revision, String::new(), username, tracks.clone());
        Ok(tracks)
    }

    /// The Liked Songs list from the last read through the session, while it is fresh.
    fn fresh_liked(&self) -> Option<Arc<LikedSet>> {
        let st = lock(&self.inner.state);
        st.liked.as_ref().filter(|(at, _)| at.elapsed() < LIKED_MEMO_TTL).map(|(_, set)| set.clone())
    }

    /// Liked Songs through the session, newest first (memoized briefly). Only the changes since the
    /// list last read (in memory, else saved) are asked for when the collection service can list
    /// them; `force` reads every page.
    async fn liked_list(&self, session: &Session, force: bool) -> Result<Arc<LikedSet>, CoreError> {
        if !force && let Some(set) = self.fresh_liked() {
            return Ok(set);
        }
        // One read at a time: callers that waited get the list the read before them made.
        let _turn = self.inner.liked_read.lock().await;
        if !force && let Some(set) = self.fresh_liked() {
            return Ok(set);
        }
        let (writes_before, memo) = {
            let st = lock(&self.inner.state);
            (st.liked_writes, st.liked.as_ref().map(|(_, set)| set.clone()))
        };
        let known = match memo.filter(|m| !m.sync_token.is_empty()) {
            _ if force => None,
            Some(memo) => Some(memo),
            None => self.cached_entry_async(&Source::LikedSongs).await.filter(|c| !c.sync_token.is_empty()).map(|c| {
                let songs = c.tracks.iter().map(|t| (t.uri.clone(), t.added_at)).collect();
                Arc::new(LikedSet { songs, sync_token: c.sync_token })
            }),
        };
        let started = Instant::now();
        let read = match fetch::liked_sync(session, known.as_deref()).await {
            Ok(read) if !read.set.songs.is_empty() || known.is_some() => read,
            // None at all: make sure through context-resolve.
            Ok(read) => match fetch::liked_uris_context(session).await {
                Ok(other) if !other.set.songs.is_empty() => fetch::LikedRead { requests: read.requests + other.requests, ..other },
                _ => read,
            },
            Err(e) if is_network(&e, session) => return Err(e),
            Err(e) => {
                log::info!("Liked Songs through the collection service failed ({e}); trying context-resolve");
                fetch::liked_uris_context(session).await.map_err(|e2| {
                    log::info!("Liked Songs through context-resolve failed too ({e2})");
                    if is_network(&e2, session) { e2 } else { e }
                })?
            }
        };
        log_liked_read(read.set.songs.len(), read.route, read.requests, started);
        let set = Arc::new(read.set);
        let mut st = lock(&self.inner.state);
        // A like or unlike that finished meanwhile may be missing from this list: don't serve it to
        // later readers (`set_liked` only patches a memo that already exists).
        if st.liked_writes == writes_before {
            st.liked = Some((Instant::now(), set.clone()));
        }
        Ok(set)
    }

    /// Liked Songs through the Web API. Paging through the whole list costs one request per 50
    /// songs against the developer app's quota, so it is only read again when Spotify's fingerprint
    /// of the list (total plus newest entry, one request) differs from the saved copy's, and then
    /// only down to where it meets the saved copy when the rest adds up.
    async fn webapi_liked(&self, api: &WebApi, username: &str, progress: Progress<'_>) -> Result<Arc<Vec<TrackInfo>>> {
        let mut memo = self.inner.webapi_liked.lock().await;
        let writes = lock(&self.inner.state).liked_writes;
        if let Some(m) = memo.as_ref()
            && m.writes == writes
            && m.checked.elapsed() < LIKED_MEMO_TTL
        {
            progress(m.tracks.len(), Some(m.tracks.len()));
            return Ok(m.tracks.clone());
        }
        let started = Instant::now();
        let revision = format!("webapi:{}", api.liked_fingerprint().await?);
        // The list as last read: in memory, else saved.
        let last = match memo.take() {
            Some(m) => Some((m.revision, m.tracks)),
            None => self.cached_entry_async(&Source::LikedSongs).await.map(|c| (c.revision, c.tracks)),
        };
        let (tracks, requests) = match last {
            Some((last_revision, tracks)) if last_revision == revision => {
                progress(tracks.len(), Some(tracks.len()));
                (tracks, 1)
            }
            last => {
                let (tracks, requests) = match last.filter(|(_, tracks)| tracks.iter().any(|t| t.added_at != 0)) {
                    Some((last_revision, known)) => {
                        log::info!("Liked Songs changed since the saved copy; reading the new ones through the Web API");
                        api.liked_tracks_since(&known, unlisted(&last_revision, known.len()), progress).await?
                    }
                    None => {
                        log::info!("Liked Songs changed since the saved copy; reading them through the Web API");
                        api.liked_tracks(progress).await?
                    }
                };
                let tracks = Arc::new(tracks);
                self.store(&Source::LikedSongs, revision.clone(), String::new(), username.to_owned(), tracks.clone());
                (tracks, requests + 1)
            }
        };
        log_liked_read(tracks.len(), "Web API", requests, started);
        *memo = Some(WebLiked { revision, checked: Instant::now(), writes, tracks: tracks.clone() });
        Ok(tracks)
    }

    async fn liked_tracks(&self, force: bool, progress: Progress<'_>) -> Result<Arc<Vec<TrackInfo>>> {
        let session = self.session();
        let username = require_username(&session)?;
        let source = Source::LikedSongs;
        let cached = if force { None } else { self.cached_entry_async(&source).await };

        match self.liked_list(&session, force).await {
            Ok(set) => {
                let uris: Vec<String> = set.songs.iter().map(|(uri, _)| uri.clone()).collect();
                let revision = convert::fingerprint(uris.iter().map(String::as_str));
                if let Some(c) = &cached
                    && c.revision == revision
                    && has_dates(&c.tracks)
                {
                    // Saved before Truffle kept the sync token: keep it now, so the next start only
                    // asks for the changes.
                    if c.sync_token.is_empty() && !set.sync_token.is_empty() {
                        self.store(&source, revision, set.sync_token.clone(), username, c.tracks.clone());
                    }
                    progress(c.tracks.len(), Some(c.tracks.len()));
                    return Ok(c.tracks.clone());
                }
                let reuse = reusable(cached.as_ref());
                let mut tracks = build_tracks(&session, &uris, &reuse, progress)
                    .await
                    .map_err(|e| friendly("Couldn't load Liked Songs.", e, &session))?;
                drop(reuse);
                // The dates (for the Date added column), when Spotify gave them.
                for (t, (_, at)) in tracks.iter_mut().zip(&set.songs) {
                    t.added_at = *at;
                }
                let tracks = Arc::new(tracks);
                self.store(&source, revision, set.sync_token.clone(), username, tracks.clone());
                Ok(tracks)
            }
            Err(e) => {
                if let Some(api) = self.webapi() {
                    log::info!("Liked Songs through the session failed ({e}); using the Web API");
                    return self.webapi_liked(&api, &username, progress).await;
                }
                if is_network(&e, &session) {
                    return Err(friendly("Couldn't load Liked Songs.", e, &session));
                }
                Err(anyhow::Error::new(e).context(NEED_WEBAPI_LIKED))
            }
        }
    }

    /// URIs of the user's liked songs (for heart icons).
    pub async fn liked_uris(&self) -> Result<HashSet<String>> {
        let session = self.session();
        let username = require_username(&session)?;
        match self.liked_list(&session, false).await {
            Ok(set) => Ok(set.songs.iter().map(|(uri, _)| uri.clone()).collect()),
            Err(e) => {
                if let Some(api) = self.webapi() {
                    log::info!("Liked Songs through the session failed ({e}); using the Web API");
                    let tracks = self.webapi_liked(&api, &username, &|_: usize, _: Option<usize>| {}).await?;
                    return Ok(tracks.iter().map(|t| t.uri.clone()).collect());
                }
                if is_network(&e, &session) {
                    return Err(friendly("Couldn't load Liked Songs.", e, &session));
                }
                Err(anyhow::Error::new(e).context(NEED_WEBAPI_LIKED))
            }
        }
    }

    /// One write of the like queue (see `likes`): `uris` liked or unliked for `user` through the
    /// session, sent once (the queue decides when to try again), or through the Web API when the
    /// session refuses the write itself (Spotify's "slow down" isn't sent there: the Web API has a
    /// small daily allowance).
    pub async fn send_likes(&self, user: &str, uris: &[String], liked: bool) -> Result<(), likes::SendError> {
        let session = self.session();
        if session.is_invalid() || !same_user(&session.username(), user) {
            return Err(likes::SendError::Network(format!("not connected to Spotify as {user}")));
        }
        if let Err(e) = fetch::collection_write_many(&session, uris, liked).await {
            let error = if session.is_invalid() { likes::SendError::Network(e.to_string()) } else { like_error(&e) };
            let likes::SendError::Other(why) = error else { return Err(error) };
            let Some(api) = self.webapi() else { return Err(likes::SendError::Other(why)) };
            log::info!("likes: the session refused the write ({why}); using the Web API");
            api.set_liked(uris, liked).await.map_err(|e| likes::SendError::Other(format!("{e:#}")))?;
        }
        // Keep the memoized list in step so the next read doesn't undo the change. (The Web API
        // copy is re-checked against Spotify's fingerprint on its next read.)
        let mut st = lock(&self.inner.state);
        st.liked_writes += 1;
        if let Some((at, set)) = st.liked.take() {
            st.liked = Some((at, Arc::new(with_likes(&set, uris, liked))));
        }
        Ok(())
    }
}

/// What a like write that failed means to the like queue.
fn like_error(e: &CoreError) -> likes::SendError {
    match e.kind {
        ErrorKind::ResourceExhausted => likes::SendError::Slow(rate_limit_wait(e)),
        ErrorKind::InvalidArgument => likes::SendError::Refused(e.to_string()),
        k if fetch::is_transient(k) || k == ErrorKind::Unauthenticated => likes::SendError::Network(e.to_string()),
        _ => likes::SendError::Other(e.to_string()),
    }
}

/// The wait librespot's own limit names ("rate limited for at least another 7 seconds"). Spotify's
/// Retry-After up to 10 s is waited inside librespot; a longer one doesn't reach Truffle.
fn rate_limit_wait(e: &CoreError) -> Option<Duration> {
    let text = e.error.to_string();
    let secs: u64 = text.split("another ").nth(1)?.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_secs(secs.max(1)))
}

/// True when a library error means the item is gone or off-limits for this account, so trying
/// again won't help (as opposed to network trouble).
pub fn is_permanent(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|e| e.downcast_ref::<CoreError>())
        .any(|e| matches!(e.kind, ErrorKind::NotFound | ErrorKind::PermissionDenied | ErrorKind::InvalidArgument))
}

/// True when Spotify (or librespot's own limit, shared by all of Truffle's calls) said to slow
/// down.
pub fn is_rate_limited(err: &anyhow::Error) -> bool {
    err.chain().filter_map(|e| e.downcast_ref::<CoreError>()).any(|e| e.kind == ErrorKind::ResourceExhausted)
}

/// Entries of an older list that can be reused when a source changed: playable catalog tracks
/// only (unplayable ones are looked up again in case they became available).
/// The one line the overnight check reads when a Liked Songs read finishes.
fn log_liked_read(songs: usize, route: &str, requests: usize, started: Instant) {
    log::info!("Liked Songs read: {songs} songs via {route}, {requests} requests, {:.1} s", started.elapsed().as_secs_f64());
}

/// `set` with `uris` liked (in front, liked now) or unliked, until the next read brings Spotify's
/// own list.
fn with_likes(set: &LikedSet, uris: &[String], liked: bool) -> LikedSet {
    let changed: HashSet<&str> = uris.iter().map(String::as_str).collect();
    let now = cache::now_secs() as i64;
    let mut seen = HashSet::new();
    let mut songs: Vec<(String, i64)> = if liked {
        uris.iter().filter(|u| seen.insert(u.as_str())).map(|u| (u.clone(), now)).collect()
    } else {
        Vec::new()
    };
    songs.extend(set.songs.iter().filter(|(u, _)| !changed.contains(u.as_str())).cloned());
    LikedSet { songs, sync_token: set.sync_token.clone() }
}

/// How many entries Spotify's total counted that a Web API read with `revision`
/// (`webapi:{total}:…`) left out of its `len` songs (unreadable ones). 0 for other reads.
fn unlisted(revision: &str, len: usize) -> usize {
    revision
        .strip_prefix("webapi:")
        .and_then(|r| r.split(':').next())
        .and_then(|total| total.parse::<usize>().ok())
        .map_or(0, |total| total.saturating_sub(len))
}

/// `tracks` with every copy of `uri` marked unplayable; `None` if none of them was playable.
pub fn greyed_out(tracks: &[TrackInfo], uri: &str) -> Option<Vec<TrackInfo>> {
    if !tracks.iter().any(|t| t.uri == uri && t.playable) {
        return None;
    }
    Some(tracks.iter().map(|t| if t.uri == uri { TrackInfo { playable: false, ..t.clone() } } else { t.clone() }).collect())
}

fn reusable(cached: Option<&CachedList>) -> HashMap<&str, &TrackInfo> {
    cached
        .map(|c| {
            c.tracks
                .iter()
                .filter(|t| t.playable && !t.is_local)
                .map(|t| (t.uri.as_str(), t))
                .collect()
        })
        .unwrap_or_default()
}

/// Turn an ordered URI list (duplicates kept) into track infos: local files and reused entries
/// locally, everything else through batched metadata requests.
/// Whether a saved list has its "added" dates (lists saved before Truffle kept them don't: those
/// are read again once).
fn has_dates(tracks: &[TrackInfo]) -> bool {
    tracks.is_empty() || tracks.iter().any(|t| t.added_at != 0 || t.is_local)
}

async fn build_tracks(
    session: &Session,
    uris: &[String],
    reuse: &HashMap<&str, &TrackInfo>,
    progress: Progress<'_>,
) -> Result<Vec<TrackInfo>, CoreError> {
    let total = uris.len();
    let mut known: HashMap<String, TrackInfo> = HashMap::with_capacity(total.min(16_384));
    let mut need: Vec<String> = Vec::new();
    let mut occurrences: HashMap<&str, usize> = HashMap::new();
    for uri in uris {
        let u = uri.as_str();
        if let Some(n) = occurrences.get_mut(u) {
            *n += 1;
            continue;
        }
        if known.contains_key(u) {
            continue;
        }
        if convert::is_local_uri(u) {
            known.insert(uri.clone(), convert::local_track_info(u));
        } else if let Some(t) = reuse.get(u) {
            known.insert(uri.clone(), (*t).clone());
        } else if convert::is_catalog_item(u) {
            occurrences.insert(u, 1);
            need.push(uri.clone());
        } else {
            known.insert(uri.clone(), convert::unavailable_info(u));
        }
    }

    let pending: usize = occurrences.values().sum();
    let mut loaded = total - pending;
    progress(loaded, Some(total));
    if !need.is_empty() {
        log::info!("fetching details for {} of {total} items ({} reused)", need.len(), known.len());
        let market = Market::from_session(session);
        let fetched = fetch::hydrate(session, need, &market, |done| {
            loaded += done.iter().map(|u| occurrences.get(u.as_str()).copied().unwrap_or(0)).sum::<usize>();
            progress(loaded.min(total), Some(total));
        })
        .await?;
        known.extend(fetched);
    }

    Ok(uris
        .iter()
        .map(|u| known.get(u).cloned().unwrap_or_else(|| convert::unavailable_info(u)))
        .collect())
}

/// Liked songs (newest first) grouped by album and by artist, each in the order of its newest song.
pub fn collect(tracks: &[TrackInfo]) -> crate::model::Collection {
    use crate::model::CollectionItem;
    let mut albums: Vec<CollectionItem> = Vec::new();
    let mut artists: Vec<CollectionItem> = Vec::new();
    let mut album_at: HashMap<String, usize> = HashMap::new();
    let mut artist_at: HashMap<String, usize> = HashMap::new();
    for (i, t) in tracks.iter().enumerate().filter(|(_, t)| !t.is_local) {
        if t.album_uri.starts_with("spotify:album:") {
            match album_at.get(&t.album_uri) {
                Some(&k) => albums[k].songs += 1,
                None => {
                    album_at.insert(t.album_uri.clone(), albums.len());
                    albums.push(CollectionItem {
                        uri: t.album_uri.clone(),
                        name: t.album.clone(),
                        sub: t.artists.first().map(|a| a.name.clone()).unwrap_or_default(),
                        image: t.cover_large.clone().or_else(|| t.cover_small.clone()),
                        songs: 1,
                        newest: i,
                    });
                }
            }
        }
        for a in t.artists.iter().filter(|a| a.uri.starts_with("spotify:artist:")) {
            match artist_at.get(&a.uri) {
                Some(&k) => artists[k].songs += 1,
                None => {
                    artist_at.insert(a.uri.clone(), artists.len());
                    artists.push(CollectionItem { uri: a.uri.clone(), name: a.name.clone(), sub: String::new(), image: None, songs: 1, newest: i });
                }
            }
        }
    }
    crate::model::Collection { albums, artists }
}

/// Days from 1970-01-01 to a calendar date (Howard Hinnant's `days_from_civil`).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days from 1970-01-01 to today (UTC).
fn days_since_epoch_now() -> i64 {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    (secs / 86_400) as i64
}

#[cfg(test)]
mod day_tests {
    use super::days_from_civil;

    #[test]
    fn calendar_days_count_from_1970() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 10, 22) - days_from_civil(2026, 10, 1), 21);
    }
}

#[cfg(test)]
mod gate_tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::Gates;

    const A: &str = "spotify:playlist:0000000000000000000001";
    const B: &str = "spotify:playlist:0000000000000000000002";

    #[tokio::test]
    async fn writes_to_one_link_take_turns_and_other_links_dont_wait() {
        let gates = Arc::new(Gates::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        // A "play" write is on its way to link A when quitting sends A its stop.
        let play = gates.turn(A).await;
        let (g, o) = (gates.clone(), order.clone());
        let stop = tokio::spawn(async move {
            let _turn = g.turn(A).await;
            o.lock().unwrap().push("stop");
        });
        // Link B doesn't wait for A.
        let other = tokio::time::timeout(Duration::from_millis(500), gates.turn(B)).await;
        assert!(other.is_ok(), "another link goes at once");
        drop(other);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(order.lock().unwrap().is_empty(), "the stop waits for the write on its way");
        order.lock().unwrap().push("play");
        drop(play);
        tokio::time::timeout(Duration::from_secs(5), stop).await.expect("its turn came").unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["play", "stop"], "the stop lands last");
        // Locks nobody holds are let go of.
        drop(gates.gate(B));
        assert_eq!(super::lock(&gates.0).len(), 1);
    }
}

/// Truffle's mixes: the songs played most, the newest likes, old likes not played, one artist's.
pub const MIX_ON_REPEAT: &str = "on-repeat";
pub const MIX_FRESH: &str = "fresh";
pub const MIX_FORGOTTEN: &str = "forgotten";
/// Followed by the artist's URI.
pub const MIX_ARTIST: &str = "artist:";
/// Songs in a mix (an artist's has all of theirs).
const MIX_LEN: usize = 50;

/// `list` (Liked Songs as last read) brought up to date with `liked` (the likes as they are now):
/// songs unliked since are left out, and songs liked since go in front with the details `known`
/// has for them. The bool is false when one had none: it is left out until the list is read
/// again.
fn liked_now(list: &[TrackInfo], liked: &HashSet<String>, known: impl Fn(&str) -> Option<TrackInfo>) -> (Vec<TrackInfo>, bool) {
    let had: HashSet<&str> = list.iter().map(|t| t.uri.as_str()).collect();
    let mut whole = true;
    let mut songs: Vec<TrackInfo> = Vec::new();
    for uri in liked.iter().filter(|u| !had.contains(u.as_str())) {
        match known(uri) {
            Some(t) => songs.push(t),
            None => whole = false,
        }
    }
    songs.extend(list.iter().filter(|t| liked.contains(&t.uri)).cloned());
    (songs, whole)
}

/// An artist's playable songs in Liked Songs, and the releases they are on (most liked first).
pub fn artist_likes_of(artist: &str, liked: &[TrackInfo]) -> crate::model::ArtistLikes {
    let mine: Vec<&TrackInfo> = liked.iter().filter(|t| t.playable && !t.is_local && t.artists.iter().any(|a| a.uri == artist)).collect();
    let mut albums: Vec<(crate::model::Card, usize)> = Vec::new();
    for t in &mine {
        if !t.album_uri.starts_with("spotify:album:") {
            continue;
        }
        match albums.iter_mut().find(|(c, _)| c.uri == t.album_uri) {
            Some((_, n)) => *n += 1,
            None => albums.push((
                crate::model::Card {
                    uri: t.album_uri.clone(),
                    name: t.album.clone(),
                    subtitle: "Album".to_owned(),
                    image: t.cover_large.clone().or_else(|| t.cover_small.clone()),
                },
                1,
            )),
        }
    }
    albums.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    crate::model::ArtistLikes { artist: artist.to_owned(), songs: mine.len(), albums }
}

/// The songs of a mix, from Liked Songs (newest first) and play counts.
pub fn mix_of(id: &str, liked: &[TrackInfo], counts: &HashMap<String, u32>) -> Vec<TrackInfo> {
    let plays = |t: &TrackInfo| counts.get(&t.uri).copied().unwrap_or(0);
    let playable = liked.iter().filter(|t| t.playable && !t.is_local);
    if id == MIX_ON_REPEAT {
        let mut v: Vec<&TrackInfo> = playable.filter(|t| plays(t) >= 2).collect();
        v.sort_by_key(|t| std::cmp::Reverse(plays(t)));
        v.into_iter().take(MIX_LEN).cloned().collect()
    } else if id == MIX_FRESH {
        playable.take(MIX_LEN).cloned().collect()
    } else if id == MIX_FORGOTTEN {
        // From the older half of the likes, never played in Truffle; spread over all of it.
        let old: Vec<&TrackInfo> = liked.iter().skip(liked.len() / 2).filter(|t| t.playable && !t.is_local && plays(t) == 0).collect();
        let step = (old.len() / MIX_LEN).max(1);
        old.into_iter().step_by(step).take(MIX_LEN).cloned().collect()
    } else if let Some(artist) = id.strip_prefix(MIX_ARTIST) {
        playable.filter(|t| t.artists.iter().any(|a| a.uri == artist)).cloned().collect()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod liked_tests {
    use super::*;

    fn set(uris: &[&str]) -> LikedSet {
        LikedSet { songs: uris.iter().enumerate().map(|(i, u)| (u.to_string(), 1_000 - i as i64)).collect(), sync_token: "s1".into() }
    }

    #[test]
    fn likes_made_here_patch_the_list_until_the_next_read() {
        let before = set(&["a", "b", "c"]);
        let liked = with_likes(&before, &["c".into(), "x".into(), "x".into()], true);
        let uris: Vec<&str> = liked.songs.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(uris, ["c", "x", "a", "b"], "liked again goes in front, nothing twice");
        assert!(liked.songs[0].1 > 1_000, "liked now");
        assert_eq!(liked.sync_token, "s1", "the next read asks for the changes since the last one");
        let unliked = with_likes(&before, &["b".into(), "not liked".into()], false);
        assert_eq!(unliked.songs, vec![("a".into(), 1_000), ("c".into(), 998)]);
        assert_eq!(with_likes(&LikedSet::default(), &[], true), LikedSet::default());
    }

    #[test]
    fn a_failed_like_write_waits_unless_it_is_about_the_songs() {
        use likes::SendError::{Network, Other, Refused, Slow};
        let limited = CoreError::resource_exhausted("rate limited for at least another 7 seconds");
        assert_eq!(like_error(&limited), Slow(Some(Duration::from_secs(7))));
        let now = CoreError::resource_exhausted("rate limited for at least another 0 seconds");
        assert_eq!(like_error(&now), Slow(Some(Duration::from_secs(1))), "never no wait at all");
        assert_eq!(like_error(&CoreError::resource_exhausted("429 Too Many Requests")), Slow(None));
        assert_eq!(like_error(&CoreError::resource_exhausted("rate limited for at least another")), Slow(None));
        for e in [CoreError::deadline_exceeded("timed out"), CoreError::unavailable("503"), CoreError::unauthenticated("401")] {
            assert!(matches!(like_error(&e), Network(_)), "{e}");
        }
        assert!(matches!(like_error(&CoreError::invalid_argument("400")), Refused(_)));
        assert!(matches!(like_error(&CoreError::permission_denied("403")), Other(_)));
        assert!(matches!(like_error(&CoreError::not_found("404")), Other(_)));
    }

    #[test]
    fn unreadable_entries_are_counted_from_a_web_api_revision() {
        assert_eq!(unlisted("webapi:120:2026-01-02T00:00:00Z:x", 119), 1);
        assert_eq!(unlisted("webapi:120:2026-01-02T00:00:00Z:x", 120), 0);
        assert_eq!(unlisted("webapi:5::", 9), 0, "never below none");
        assert_eq!(unlisted("webapi:?::", 3), 0);
        assert_eq!(unlisted("0123456789abcdef", 3), 0, "a session read lists every entry");
        assert_eq!(unlisted("", 0), 0);
    }
}

#[cfg(test)]
mod grey_tests {
    use super::*;

    fn song(id: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{id}"),
            name: id.to_owned(),
            artists: Vec::new(),
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 1000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    #[test]
    fn greying_out_marks_every_copy_of_that_song_only() {
        let list = vec![song("a"), song("b"), song("a")];
        let greyed = greyed_out(&list, "spotify:track:a").expect("a was playable");
        assert_eq!(greyed.iter().map(|t| t.playable).collect::<Vec<_>>(), [false, true, false]);
        assert_eq!(greyed.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["a", "b", "a"], "details kept");
        assert!(greyed_out(&greyed, "spotify:track:a").is_none(), "already greyed out: nothing to save");
        assert!(greyed_out(&list, "spotify:track:zz").is_none(), "not in the list");
    }
}

#[cfg(test)]
mod mix_tests {
    use super::*;
    use crate::model::ArtistRef;

    fn t(i: usize, artist: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{i}"),
            name: format!("Song {i}"),
            artists: vec![ArtistRef { name: artist.into(), uri: format!("spotify:artist:{artist}") }],
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 1000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    #[test]
    fn likes_count_at_once_from_the_list_last_read() {
        let list: Vec<TrackInfo> = (0..4).map(|i| t(i, "a")).collect();
        let liked: HashSet<String> = ["spotify:track:0", "spotify:track:1", "spotify:track:9", "spotify:track:8"].iter().map(|s| s.to_string()).collect();
        // 9 was liked since and its details are in memory; 8 was too, but nothing knows it yet.
        let known = |u: &str| (u == "spotify:track:9").then(|| t(9, "a"));
        let (songs, whole) = liked_now(&list, &liked, known);
        let names: Vec<&str> = songs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Song 9", "Song 0", "Song 1"], "unliked 2 and 3 are out, 9 goes in front");
        assert!(!whole, "8 is missing until the list is read again");
        assert_eq!(artist_likes_of("spotify:artist:a", &songs).songs, 3);
        let (_, whole) = liked_now(&list, &list.iter().take(2).map(|t| t.uri.clone()).collect(), |_| None);
        assert!(whole);
    }

    #[test]
    fn mixes_pick_their_songs() {
        let liked: Vec<TrackInfo> = (0..10).map(|i| t(i, if i % 2 == 0 { "a" } else { "b" })).collect();
        let mut counts = HashMap::new();
        counts.insert("spotify:track:3".to_owned(), 5);
        counts.insert("spotify:track:7".to_owned(), 2);
        counts.insert("spotify:track:8".to_owned(), 1);
        let names = |v: Vec<TrackInfo>| v.into_iter().map(|t| t.name).collect::<Vec<_>>();
        assert_eq!(names(mix_of(MIX_ON_REPEAT, &liked, &counts)), ["Song 3", "Song 7"]);
        assert_eq!(mix_of(MIX_FRESH, &liked, &counts).len(), 10);
        // The older half (5..10), not played: 5, 6, 9.
        assert_eq!(names(mix_of(MIX_FORGOTTEN, &liked, &counts)), ["Song 5", "Song 6", "Song 9"]);
        assert_eq!(mix_of(&format!("{MIX_ARTIST}spotify:artist:a"), &liked, &counts).len(), 5);
        // The same songs, counted per release for the artist's page.
        let mut on_albums = liked.clone();
        for (i, t) in on_albums.iter_mut().enumerate() {
            t.album_uri = format!("spotify:album:{}", i % 3);
        }
        on_albums[0].playable = false;
        let likes = artist_likes_of("spotify:artist:a", &on_albums);
        // Songs 2, 4, 6, 8 (0 can't be played), on albums 2, 1, 0, 2: album 2 first.
        assert_eq!(likes.songs, 4);
        let per: Vec<(&str, usize)> = likes.albums.iter().map(|(c, n)| (c.uri.as_str(), *n)).collect();
        assert_eq!(per, [("spotify:album:2", 2), ("spotify:album:1", 1), ("spotify:album:0", 1)]);
    }
}
