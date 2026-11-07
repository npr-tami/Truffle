//! Background job plumbing: the messages spawned tasks send back to the controller loop, and the
//! longer task bodies (login, reconnect, cover upload).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use librespot_core::Session;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::UiLink;
use crate::config::{Paths, Settings};
use crate::library::Library;
use crate::model::{Event, PlaylistSummary, Source, TrackInfo, UserProfile};
use crate::queue::SavedQueue;
use crate::session;
use crate::webapi::WebApi;

/// At most ~10 progress events per second per load.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Safety net around `connect_cached` (which gives up by itself after about 75 s).
const CACHED_LOGIN_TIMEOUT: Duration = Duration::from_secs(90);
const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);
/// Waits between reconnect attempts; the last one repeats until the connection is back.
const RECONNECT_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// A successful login: the connected session plus what is needed to announce it.
pub(super) struct LoginOk {
    pub session: Session,
    pub library: Library,
    pub profile: UserProfile,
}

/// Why a track list was requested; decides what happens with the result.
pub(super) enum TracksPurpose {
    /// Show it in the UI. `shown` is the cached copy already sent (if any).
    Open { shown: Option<Arc<Vec<TrackInfo>>> },
    /// Start playing it.
    Play { start_index: Option<usize>, shuffle: bool },
    /// Rebuild the queue saved at the last shutdown (paused).
    Restore { saved: SavedQueue },
}

pub(super) enum PlaylistEdit {
    Details { name: Option<String>, description: Option<String> },
    Cover,
}

/// Results of background work. `epoch` is the session epoch at spawn time: results from a
/// session that has since been logged out are dropped.
pub(super) enum Job {
    Login { id: u64, interactive: bool, result: Result<Option<LoginOk>> },
    Reconnected { id: u64, session: Option<Session> },
    Playlists { epoch: u64, id: u64, user: bool, result: Result<Vec<PlaylistSummary>> },
    Liked { epoch: u64, id: u64, result: Result<HashSet<String>> },
    Tracks { epoch: u64, id: u64, source: Source, purpose: TracksPurpose, result: Result<Arc<Vec<TrackInfo>>> },
    /// What the like queue's sender says (see `library::likes`).
    Likes { epoch: u64, report: crate::library::likes::Report },
    /// `user`: the Spotify account the new client is signed in to, if it could be read.
    WebApi { id: u64, client_id: String, interactive: bool, result: Result<Option<WebApi>>, user: Option<String> },
    /// Which account the Web API client (of request `id`) is signed in to.
    WebApiUser { id: u64, result: Result<String> },
    /// Whether Spotify answered a reachability check.
    Reachability { epoch: u64, id: u64, reachable: bool },
    /// A song that failed to load, looked up again on Spotify (see `Library::recheck`).
    Recheck { epoch: u64, id: u64, uri: String, result: Result<TrackInfo> },
    PlaylistEdited { epoch: u64, uri: String, edit: PlaylistEdit, result: Result<Option<PlaylistSummary>> },
    /// A Last.fm call finished (independent of the Spotify session).
    Lastfm(crate::lastfm::Report),
    /// Songs or playlists were changed (see `browse.rs`).
    Browse { epoch: u64, result: Result<super::browse::BrowseDone> },
    /// A playlist was read for "Find duplicates".
    Duplicates { epoch: u64, uri: String, result: Result<Arc<Vec<TrackInfo>>> },
    /// Listen-along work finished (see `listen.rs`).
    Listen { epoch: u64, done: super::listen::Done },
}

pub(super) fn spawn_login(id: u64, interactive: bool, paths: Paths, settings: Settings, tx: UnboundedSender<Job>) {
    tokio::spawn(async move {
        let result = login(interactive, &paths, &settings).await;
        let _ = tx.send(Job::Login { id, interactive, result });
    });
}

async fn login(interactive: bool, paths: &Paths, settings: &Settings) -> Result<Option<LoginOk>> {
    let session = if interactive {
        // No timeout here: the user may take a while in the browser. The session module owns
        // the OAuth listener and its own cancellation.
        session::connect_interactive(paths, settings).await?
    } else {
        match tokio::time::timeout(CACHED_LOGIN_TIMEOUT, session::connect_cached(paths, settings)).await {
            Ok(result) => match result? {
                Some(session) => session,
                None => return Ok(None),
            },
            Err(_) => bail!("Spotify didn't respond in time"),
        }
    };

    let library = Library::new(session.clone(), paths.clone());
    let mut profile = match tokio::time::timeout(PROFILE_TIMEOUT, library.profile()).await {
        Ok(Ok(profile)) => profile,
        Ok(Err(e)) => {
            log::warn!("could not load the user profile: {e:#}");
            UserProfile::default()
        }
        Err(_) => {
            log::warn!("loading the user profile timed out");
            UserProfile::default()
        }
    };
    if profile.username.is_empty() {
        profile.username = session::username(&session);
    }
    if profile.display_name.is_empty() {
        profile.display_name = profile.username.clone();
    }
    profile.premium = account_premium(&session).await;
    Ok(Some(LoginOk { session, library, profile }))
}

/// Whether the account is Premium. Spotify says so a moment after connecting: asked for up to
/// three seconds (`None` if it never says).
async fn account_premium(session: &Session) -> Option<bool> {
    for _ in 0..15 {
        if let Some(kind) = session.get_user_attribute("type") {
            log::info!("account type: {kind}");
            return Some(kind == "premium");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

fn reconnect_delay(attempt: usize) -> Duration {
    RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)]
}

/// Retry `connect_cached` with backoff until it succeeds or there are no credentials left.
/// The returned handle is aborted when the user logs out or the app shuts down.
pub(super) fn spawn_reconnect(
    id: u64,
    paths: Paths,
    settings: Settings,
    tx: UnboundedSender<Job>,
    ui: UiLink,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut attempt = 0usize;
        loop {
            tokio::time::sleep(reconnect_delay(attempt)).await;
            attempt += 1;
            match tokio::time::timeout(CACHED_LOGIN_TIMEOUT, session::connect_cached(&paths, &settings)).await {
                Ok(Ok(session)) => {
                    let _ = tx.send(Job::Reconnected { id, session });
                    return;
                }
                Ok(Err(e)) => log::warn!("reconnect attempt {attempt} failed: {e:#}"),
                Err(_) => log::warn!("reconnect attempt {attempt} timed out"),
            }
            let wait = reconnect_delay(attempt).as_secs();
            ui.send(Event::Status(format!(
                "Still can't reach Spotify. Trying again in {wait} seconds…"
            )));
        }
    })
}

/// A throttled progress callback for `Library::tracks`. With `ui == None` it does nothing.
pub(super) fn progress_reporter(ui: Option<UiLink>, source: Source) -> impl Fn(usize, Option<usize>) + Send + Sync {
    let last_sent: Mutex<Option<Instant>> = Mutex::new(None);
    move |loaded, total| {
        let Some(ui) = &ui else { return };
        let now = Instant::now();
        {
            let mut last = last_sent.lock().unwrap_or_else(|e| e.into_inner());
            let previous: Option<Instant> = *last;
            let finished = total.is_some_and(|t| loaded >= t);
            if !finished && previous.is_some_and(|at| now.duration_since(at) < PROGRESS_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        ui.send(Event::SourceLoading { source: source.clone(), loaded, total });
    }
}

/// Rename / re-describe a playlist. Returns no summary: the controller patches its own copy,
/// which keeps the library's revision and track count intact.
pub(super) async fn update_details(
    library: Library,
    playlist_uri: String,
    name: Option<String>,
    description: Option<String>,
) -> Result<Option<PlaylistSummary>> {
    library.update_playlist_details(&playlist_uri, name.as_deref(), description.as_deref()).await?;
    Ok(None)
}

/// A new cover from an image file. Returns a summary carrying only the new picture's address
/// (when Spotify gave it), which the controller lays over its own copy.
pub(super) async fn upload_cover(library: Library, playlist_uri: String, image_path: PathBuf) -> Result<Option<PlaylistSummary>> {
    let image_url = library.set_playlist_cover(&playlist_uri, &image_path).await?;
    Ok(image_url.map(|url| PlaylistSummary {
        uri: playlist_uri,
        name: String::new(),
        description: String::new(),
        owner_name: String::new(),
        image_url: Some(url),
        track_count: None,
        revision: String::new(),
        editable: true,
        owned: true,
        owner_id: String::new(),
    }))
}
