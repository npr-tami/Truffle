//! Account side of the controller: login / logout / reconnect, the library (playlists, liked
//! songs, track lists) and the optional Web API used for editing.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use librespot_core::Session;

use super::jobs::{self, Job, LoginOk, PlaylistEdit, TracksPurpose};
use super::{Controller, Listen, drop_in_background, lock, persist, user_error};
use crate::library::likes::{self, Report};
use crate::library::{self, Library};
use crate::model::{Event, LikesWaiting, PlayStatus, PlaybackState, PlaylistSummary, Source, TrackInfo};
use crate::session;
use crate::ui::{fmt_song_count, fmt_thousands};
use crate::webapi::{self, WebApi};

/// While a login is running, a new browser login is only started after this long (guards
/// against double clicks; a newer browser login replaces the older one).
const INTERACTIVE_RELOGIN_AFTER: Duration = Duration::from_secs(5);
/// While a login is running, a silent (cached) login request is ignored for this long.
const CACHED_RELOGIN_AFTER: Duration = Duration::from_secs(120);

/// Start reminding the user this many days before Spotify ends the Web API sign-in (refresh
/// tokens stop working about 6 months after the browser sign-in).
const WEBAPI_REMINDER_DAYS: u64 = 14;

/// A like or unlike Spotify took that a fetched Liked Songs list may not contain yet.
pub(super) struct LikeChange {
    liked: bool,
    /// Request-id clock when Spotify took it. A list fetched after that contains it.
    confirmed: u64,
}

/// A like or unlike of many songs at once, until all of it went through.
pub(super) struct BigLike {
    liked: bool,
    total: usize,
    /// Its songs Spotify doesn't have yet (not ones changed the other way since).
    left: HashSet<String>,
    /// How far it got was said when the sender had to wait.
    told: bool,
}

impl BigLike {
    fn news(&self) -> String {
        like_news(self.liked, self.total - self.left.len(), self.left.len())
    }
}

/// "2,000 liked, 8,000 waiting" ("removed, … to remove" for unlikes) while some wait, then
/// "10,000 songs added to Liked Songs".
fn like_news(liked: bool, done: usize, left: usize) -> String {
    match (liked, left) {
        (true, 0) => format!("{} added to Liked Songs", fmt_song_count(done)),
        (false, 0) => format!("{} removed from Liked Songs", fmt_song_count(done)),
        (true, _) => format!("{} liked, {} waiting", fmt_thousands(done), fmt_thousands(left)),
        (false, _) => format!("{} removed, {} to remove", fmt_thousands(done), fmt_thousands(left)),
    }
}

/// Fields of a playlist changed through the Web API, re-applied over playlist lists whose fetch
/// started before the change was confirmed (they may still have the old name or picture).
#[derive(Default)]
pub(super) struct PlaylistPatch {
    name: Option<String>,
    description: Option<String>,
    image_url: Option<String>,
    /// Request-id clock when the edit was confirmed.
    confirmed: u64,
}

impl PlaylistPatch {
    fn apply(&self, playlist: &mut PlaylistSummary) {
        if let Some(name) = &self.name {
            playlist.name.clone_from(name);
        }
        if let Some(description) = &self.description {
            playlist.description.clone_from(description);
        }
        if let Some(image_url) = &self.image_url {
            playlist.image_url = Some(image_url.clone());
        }
    }
}

/// How the connected Web API client relates to the Spotify account logged in through librespot.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WebApiFit {
    /// Not connected.
    Missing,
    /// Connected, but which account it is signed in to isn't known yet.
    Unverified,
    /// Signed in to another Spotify account (its user id).
    OtherAccount(String),
    /// Signed in to the logged-in account (or nobody is logged in to compare with).
    Matches,
}

impl Controller {
    // ------------------------------------------------------------ login / logout

    pub(super) fn begin_login(&mut self, interactive: bool) {
        if self.session.is_some() {
            if let Some(profile) = &self.profile {
                self.ui.send(Event::LoggedIn(profile.clone()));
            }
            return;
        }
        let wait = if interactive { INTERACTIVE_RELOGIN_AFTER } else { CACHED_RELOGIN_AFTER };
        if let Some((_, started)) = self.login
            && started.elapsed() < wait
        {
            let message = if interactive {
                "Finish logging in in the browser window that Truffle opened."
            } else {
                "Still connecting to Spotify…"
            };
            self.ui.send(Event::Status(message.into()));
            return;
        }
        let id = self.new_id();
        self.login = Some((id, Instant::now()));
        self.ui.send(Event::LoggingIn);
        if interactive {
            self.ui.send(Event::Status("Opening your browser so you can log in to Spotify…".into()));
        }
        jobs::spawn_login(id, interactive, self.paths.clone(), self.settings_snapshot(), self.jobs.clone());
    }

    pub(super) fn on_login(&mut self, id: u64, interactive: bool, result: Result<Option<LoginOk>>) {
        let current = self.login.is_some_and(|(latest, _)| latest == id);
        // A superseded attempt that still succeeded is welcome, as long as a login is wanted.
        let wanted = self.session.is_none() && self.login.is_some();
        if current {
            self.login = None;
        }
        match result {
            Ok(Some(ok)) => {
                if wanted {
                    self.on_session(ok);
                } else {
                    ok.session.shutdown();
                }
            }
            Ok(None) => {
                if current && self.session.is_none() {
                    self.ui.send(Event::NeedLogin);
                }
            }
            Err(e) => {
                if current && self.session.is_none() {
                    let lead = if interactive {
                        "Logging in to Spotify didn't work. Please try again."
                    } else {
                        "Truffle couldn't connect to Spotify. Check your internet connection, then log in again."
                    };
                    self.ui.send(Event::Error(user_error(lead, &e)));
                    self.ui.send(Event::NeedLogin);
                }
            }
        }
    }

    fn on_session(&mut self, ok: LoginOk) {
        let LoginOk { session, library, profile } = ok;
        log::info!("logged in as {}", profile.username);
        self.session = Some(session);
        self.library = Some(library);
        self.profile = Some(profile.clone());
        self.ensure_engine();
        self.ui.send(Event::LoggedIn(profile.clone()));
        // Editing only works with the account it was connected for.
        if self.webapi.is_some() {
            self.check_webapi_account(None);
        }

        // Show the last known library at once, then refresh it in the background.
        if let Some(playlists) = self.library.as_ref().and_then(|l| l.cached_playlists()) {
            self.playlists = playlists;
            self.ui.send(Event::Library(self.playlists_for_ui()));
        }
        if let Some(liked) = persist::load_liked(&self.paths, &profile.username) {
            self.liked = liked;
            // Likes still waiting from before (a restart, a logout) keep their hearts.
            self.likes.apply(&profile.username, &mut self.liked);
            self.ui.send(Event::LikedSet(Arc::new(self.liked.clone())));
        }
        self.start_like_sender();
        self.request_library(false);
        self.request_liked();
        self.send_blocked();
        self.listen_logged_in();
        // How far each list's round has got (from the play history loaded at start), sent anew.
        self.last_round = None;
        self.round_dirty = true;
        self.sync_queue_info();
        self.mark_playback();
        self.restore_queue();
        if let Some((source, force_refresh)) = self.pending_open.take() {
            self.cmd_open_source(source, force_refresh);
        }
        if std::mem::take(&mut self.pending_collection) {
            self.cmd_collection();
        }
    }

    pub(super) fn cmd_logout(&mut self) {
        self.record_if_heard();
        // Last.fm is a separate account and stays connected; only the current play ends.
        self.finalise_scrobble_play();
        self.login = None;
        self.teardown_session();
        self.save_state();
        if let Err(e) = session::forget_credentials(&self.paths) {
            self.ui.send(Event::Error(user_error(
                "You're logged out, but Truffle couldn't remove the saved login from this computer.",
                &e,
            )));
        }
        self.ui.send(Event::LoggedOut);
    }

    /// Stop playback and drop everything tied to the current session. The queue is kept as a
    /// saved queue (tagged with its account) so it can be restored after the next login.
    pub(super) fn teardown_session(&mut self) {
        self.epoch += 1;
        if let Some((_, task)) = self.reconnect.take() {
            task.abort();
        }
        if let Some(saved) = self.saved_live_queue() {
            self.saved_queue = Some(saved);
            self.queue = None;
            self.state_dirty = true;
        }
        // Listening along ends with the session (the user's own queue was just saved).
        self.reset_listening();
        self.engine_rx = None;
        if let Some(engine) = self.engine.take() {
            engine.stop();
            drop_in_background(engine);
        }
        // The Web API client stays connected but leaves with the library: the next login checks
        // that it belongs to that account before using it.
        self.library = None;
        if let Some(session) = self.session.take() {
            session.shutdown();
        }
        self.profile = None;
        self.playlists.clear();
        self.playlist_edits.clear();
        self.liked.clear();
        self.like_changes.clear();
        // The likes not sent yet wait in their account's queue for its next login.
        if let Some(task) = self.like_sender.take() {
            task.abort();
        }
        self.likes.stopped();
        self.big_likes.clear();
        if self.likes_shown.take().is_some() {
            self.ui.send(Event::LikesWaiting(LikesWaiting::default()));
        }
        self.pending_open = None;
        self.open_source = None;
        self.shown = None;
        self.youtube_search_ahead();
        self.want_play = false;
        self.pending_reload = false;
        self.ended = false;
        self.failures = 0;
        self.probe = None;
        self.recheck = None;
        self.last_reachable = None;
        self.interruptions = 0;
        self.reconnect_waits = 0;
        self.listen = Listen::default();
        self.end_handled = false;
        self.preload_requested = false;
        self.crossfade_checked = false;
        // A sleep timer belongs to the music that was playing: it must not pause whatever plays
        // after the next login (the UI stops showing it now).
        self.fade_restore_pending = false;
        if self.sleep.take().is_some() {
            log::info!("sleep timer: cancelled by logging out");
        }
        self.ui.send(Event::SleepTimer(None));
        self.pb = PlaybackState {
            volume: self.pb.volume,
            shuffle: self.pb.shuffle,
            repeat: self.pb.repeat,
            ..Default::default()
        };
        self.sync_queue_info();
        self.mark_playback();
    }

    // ------------------------------------------------------------ reconnect

    pub(super) fn start_reconnect(&mut self) {
        if self.reconnect.is_some() || self.session.is_none() {
            return;
        }
        log::warn!("Spotify session lost; reconnecting");
        let id = self.new_id();
        self.ui.send(Event::Status("Lost the connection to Spotify. Reconnecting…".into()));
        let task = jobs::spawn_reconnect(id, self.paths.clone(), self.settings_snapshot(), self.jobs.clone(), self.ui.clone());
        self.reconnect = Some((id, task));
    }

    pub(super) fn on_reconnected(&mut self, id: u64, session: Option<Session>) {
        if !self.reconnect.as_ref().is_some_and(|(latest, _)| *latest == id) {
            if let Some(session) = session {
                session.shutdown();
            }
            return;
        }
        self.reconnect = None;
        let Some(session) = session else {
            // The saved login is gone or was rejected.
            self.teardown_session();
            self.ui.send(Event::Error("Your Spotify login has expired. Please log in again.".into()));
            self.ui.send(Event::NeedLogin);
            return;
        };
        log::info!("reconnected to Spotify");
        if let Some(engine) = &self.engine {
            engine.set_session(session.clone());
        }
        if let Some(library) = self.library.as_mut() {
            library.set_session(session.clone());
        }
        if let Some(old) = self.session.replace(session) {
            old.shutdown();
        }
        self.ui.send(Event::Status("Reconnected to Spotify.".into()));
        // Removed listen-along links' playlists not taken out of the library while it was down
        // (one still on its way from before is asked for again if it fails).
        self.along.reconnects += 1;
        self.retry_deletes();
        // The internet is back: unsent scrobbles needn't wait out their retry delay, nor likes.
        self.lastfm.network_up(Instant::now());
        self.likes.network_up();
        self.refresh_premium();
        // Resume where we were (playing or paused) with the new session. YouTube sound went on
        // meanwhile: only a song left waiting is loaded.
        let spotify = self.sound_source() == crate::config::SoundSource::Spotify;
        if self.pb.track.is_some() && self.pb.status != PlayStatus::Stopped && (spotify || self.pending_reload) {
            let position = self.pb.current_position_ms();
            let play = self.want_play;
            self.load_current(play, position, false);
        }
    }

    // ------------------------------------------------------------ library

    pub(super) fn request_library(&mut self, user: bool) {
        let Some(library) = self.library.clone() else {
            if user {
                self.ui.send(Event::Status("Log in to Spotify to see your playlists.".into()));
            }
            return;
        };
        let id = self.new_id();
        self.library_req = id;
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = library.playlists().await;
            let _ = tx.send(Job::Playlists { epoch, id, user, result });
        });
        if user {
            self.ui.send(Event::Status("Refreshing your library…".into()));
        }
    }

    /// The playlists the UI shows: all but the listen-along links (they only carry what plays):
    /// the ones kept here or made in this session, the one older Truffles write to, and any of
    /// the user's own named or worded like one (made on another PC, or renamed in Spotify's app).
    /// Every list in the UI (sidebar, Home, Library, "Add to playlist", autoplay) reads this.
    pub(super) fn playlists_for_ui(&self) -> Vec<PlaylistSummary> {
        let channel = lock(&self.settings).listen_channel.clone();
        let link = |p: &PlaylistSummary| {
            self.links.knows(&p.uri)
                || self.along.made.contains(&p.uri)
                || channel.as_deref() == Some(p.uri.as_str())
                || (p.editable && (p.name == crate::listen::CHANNEL_NAME || crate::listen::is_channel(&p.description)))
        };
        self.playlists.iter().filter(|p| !link(p)).cloned().collect()
    }

    pub(super) fn on_playlists(&mut self, id: u64, user: bool, result: Result<Vec<PlaylistSummary>>) {
        if id != self.library_req {
            return;
        }
        match result {
            Ok(mut playlists) => {
                self.apply_playlist_edits(id, &mut playlists);
                self.hide_removed_playlists(&mut playlists);
                self.playlists = playlists;
                self.ui.send(Event::Library(self.playlists_for_ui()));
                // Hidden from every list, so nothing else would show one still there.
                self.delete_listed_links();
            }
            Err(e) if self.playlists.is_empty() => {
                // Nothing to show at all: the sidebar says so and offers to try again.
                let message = user_error("Couldn't load your playlists. Check your internet connection and try again.", &e);
                self.ui.send(Event::LibraryFailed { message });
            }
            Err(e) if user => {
                self.ui.send(Event::Error(user_error(
                    "Couldn't load your playlists. Check your internet connection and use Refresh to try again.",
                    &e,
                )));
            }
            Err(e) => {
                log::warn!("background playlist refresh failed: {e:#}");
                self.ui.send(Event::Status(
                    "Couldn't refresh your playlists right now. Showing the last saved list.".into(),
                ));
            }
        }
    }

    /// Re-apply confirmed edits that a list fetched before them can't contain yet. Edits confirmed
    /// before the fetch (`fetch_id`) started are in it and are forgotten.
    fn apply_playlist_edits(&mut self, fetch_id: u64, playlists: &mut [PlaylistSummary]) {
        self.playlist_edits.retain(|uri, patch| {
            if patch.confirmed < fetch_id {
                return false;
            }
            if let Some(playlist) = playlists.iter_mut().find(|p| &p.uri == uri) {
                patch.apply(playlist);
            }
            true
        });
    }

    pub(super) fn request_liked(&mut self) {
        let Some(library) = self.library.clone() else { return };
        let id = self.new_id();
        self.liked_req = id;
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = library.liked_uris().await;
            let _ = tx.send(Job::Liked { epoch, id, result });
        });
    }

    pub(super) fn on_liked(&mut self, id: u64, result: Result<HashSet<String>>) {
        if id != self.liked_req {
            return;
        }
        match result {
            Ok(mut liked) => {
                // Likes and unlikes Spotify took after this fetch started may be missing from it:
                // keep them. Once a fetch that started after one comes back, it is in it and is
                // forgotten. The ones still waiting to be sent go on top.
                self.like_changes.retain(|uri, change| {
                    let in_list = change.confirmed < id;
                    if !in_list {
                        if change.liked {
                            liked.insert(uri.clone());
                        } else {
                            liked.remove(uri);
                        }
                    }
                    !in_list
                });
                if let Some(profile) = &self.profile {
                    self.likes.apply(&profile.username, &mut liked);
                }
                if liked != self.liked {
                    self.liked = liked;
                    self.save_liked_snapshot();
                }
                self.ui.send(Event::LikedSet(Arc::new(self.liked.clone())));
            }
            Err(e) => {
                log::warn!("could not load liked songs: {e:#}");
                self.webapi_failed(&e);
            }
        }
    }

    /// Likes or unlikes one song (the heart, the menu, Ctrl+H): shown at once, and queued for
    /// Spotify (see `library::likes`).
    pub(super) fn cmd_set_liked(&mut self, uri: String, liked: bool) {
        if !likes::is_song(&uri) {
            self.ui.send(Event::Error("Only Spotify songs can be added to Liked Songs.".into()));
            return;
        }
        self.queue_likes(vec![uri], liked);
    }

    /// Likes (or unlikes) many songs: shown at once and queued. Says how it went once all of it
    /// went through, or as soon as some of it has to wait.
    pub(super) fn cmd_set_liked_many(&mut self, uris: Vec<String>, liked: bool) {
        let songs: Vec<String> = uris.into_iter().filter(|u| likes::is_song(u)).collect();
        if songs.is_empty() || !self.queue_likes(songs.clone(), liked) {
            return;
        }
        let left: HashSet<String> = songs.into_iter().collect();
        let mut big = BigLike { liked, total: left.len(), left, told: false };
        if matches!(self.likes.state(), likes::State::Waiting { .. }) {
            big.told = true;
            self.ui.send(Event::Status(big.news()));
        }
        self.big_likes.push(big);
    }

    /// Shows the change at once and puts it in the account's queue. False when nobody is logged in.
    fn queue_likes(&mut self, uris: Vec<String>, liked: bool) -> bool {
        let Some(user) = self.profile.as_ref().filter(|_| self.library.is_some()).map(|p| p.username.clone()) else {
            self.ui.send(Event::Error("Log in to Spotify to change your Liked Songs.".into()));
            return false;
        };
        for uri in &uris {
            if liked {
                self.liked.insert(uri.clone());
            } else {
                self.liked.remove(uri);
            }
        }
        // A song changed the other way before it went is no longer part of an earlier big change.
        for big in self.big_likes.iter_mut().filter(|b| b.liked != liked) {
            for uri in &uris {
                if big.left.remove(uri) {
                    big.total -= 1;
                }
            }
        }
        self.ui.send(Event::LikedSet(Arc::new(self.liked.clone())));
        self.likes.push(&user, &uris, liked);
        self.show_likes_waiting();
        true
    }

    /// Starts sending the logged-in account's waiting likes, in a task of its own: it goes on
    /// while the window is hidden, and through rate limits and lost connections.
    fn start_like_sender(&mut self) {
        let (Some(library), Some(profile)) = (self.library.clone(), self.profile.as_ref()) else { return };
        if let Some(task) = self.like_sender.take() {
            task.abort();
        }
        let user = profile.username.clone();
        let (epoch, jobs, account) = (self.epoch, self.jobs.clone(), user.clone());
        let send = move |uris: Vec<String>, liked: bool| {
            let (library, account) = (library.clone(), account.clone());
            async move { library.send_likes(&account, &uris, liked).await }
        };
        let report = move |report| {
            let _ = jobs.send(Job::Likes { epoch, report });
        };
        self.like_sender = Some(tokio::spawn(likes::run(self.likes.clone(), user, send, report)));
        self.show_likes_waiting();
    }

    /// What the like sender says: what Spotify took is kept over older reads, a refused song's
    /// heart goes back, and a big change says how it went.
    pub(super) fn on_likes_report(&mut self, report: Report) {
        let Some(user) = self.profile.as_ref().map(|p| p.username.clone()) else { return };
        match report {
            Report::Sent { uris, liked } => {
                let confirmed = self.new_id();
                for uri in &uris {
                    self.like_changes.insert(uri.clone(), LikeChange { liked, confirmed });
                }
                // A Liked Songs read that came back after the sender took these out of the queue
                // and before this report had neither them nor their change: their hearts are set
                // again, unless they were changed since.
                let mut changed = false;
                for uri in self.likes.not_pending(&user, &uris) {
                    changed |= if liked { self.liked.insert(uri.clone()) } else { self.liked.remove(uri) };
                }
                if changed {
                    self.ui.send(Event::LikedSet(Arc::new(self.liked.clone())));
                    self.save_liked_snapshot();
                }
                for big in self.big_likes.iter_mut().filter(|b| b.liked == liked) {
                    for uri in &uris {
                        big.left.remove(uri);
                    }
                }
            }
            Report::Dropped { uri, liked } => {
                // As it was, unless it was changed again since.
                if self.likes.pending(&user, &uri).is_none() {
                    self.like_changes.remove(&uri);
                    if liked {
                        self.liked.remove(&uri);
                    } else {
                        self.liked.insert(uri.clone());
                    }
                    self.ui.send(Event::LikedSet(Arc::new(self.liked.clone())));
                }
                for big in &mut self.big_likes {
                    if big.left.remove(&uri) {
                        big.total -= 1;
                    }
                }
            }
            Report::Waiting => {
                for big in self.big_likes.iter_mut().filter(|b| !b.told) {
                    big.told = true;
                    self.ui.send(Event::Status(big.news()));
                }
            }
        }
        let mut finished = false;
        self.big_likes.retain(|big| {
            let done = big.left.is_empty();
            if done {
                self.ui.send(Event::Status(big.news()));
                finished = true;
            }
            !done
        });
        if self.likes.waiting(&user).is_empty() {
            self.likes_all_sent(finished);
        }
        self.show_likes_waiting();
    }

    /// Nothing waits any more: the hearts are saved, and Liked Songs (the list in memory has every
    /// change) is read again where it shows, or after a big change, so its saved copy has them too.
    fn likes_all_sent(&mut self, big: bool) {
        self.save_liked_snapshot();
        let Some(library) = self.library.clone() else { return };
        let source = Source::LikedSongs;
        if self.open_source.as_ref() == Some(&source) {
            let shown = self.shown.as_ref().filter(|(s, _)| *s == source).map(|(_, tracks)| tracks.clone());
            self.open_req = self.spawn_tracks(library, source, false, TracksPurpose::Open { shown }, false);
        } else if big {
            tokio::spawn(async move {
                if let Err(e) = library.tracks(&source, false, &|_, _| {}).await {
                    log::warn!("could not read Liked Songs again after the likes went through: {e:#}");
                }
            });
        }
    }

    /// Tells the UI how many likes and unlikes wait: from when the sender is held up or a change
    /// is bigger than one write, until none is left (one that goes straight through isn't shown).
    fn show_likes_waiting(&mut self) {
        let waiting = self.profile.as_ref().map(|p| self.likes.waiting(&p.username)).unwrap_or_default();
        let held_up = matches!(self.likes.state(), likes::State::Waiting { .. }) || waiting.total() > likes::BATCH;
        let shown = (!waiting.is_empty() && (held_up || self.likes_shown.is_some())).then_some(waiting);
        if shown != self.likes_shown {
            self.likes_shown = shown;
            self.ui.send(Event::LikesWaiting(shown.unwrap_or_default()));
        }
    }

    fn save_liked_snapshot(&self) {
        let Some(profile) = &self.profile else { return };
        if let Err(e) = persist::save_liked(&self.paths, &profile.username, &self.liked) {
            log::warn!("could not save the liked songs snapshot: {e:#}");
        }
    }

    // ------------------------------------------------------------ track lists

    pub(super) fn cmd_open_source(&mut self, source: Source, force_refresh: bool) {
        let Some(library) = self.library.clone() else {
            if self.login.is_some() {
                // Still logging in: open it as soon as the session is ready.
                self.ui.send(Event::SourceLoading { source: source.clone(), loaded: 0, total: None });
                self.pending_open = Some((source, force_refresh));
            } else {
                self.ui.send(Event::SourceFailed { source, message: "Log in to Spotify to see your music.".into() });
            }
            return;
        };
        self.open_source = Some(source.clone());
        let cached = match &source {
            _ if force_refresh => None,
            // A mix is made from Liked Songs: shown from the list Truffle has, with the likes as
            // they are now, while Spotify's list is read again.
            Source::Mix(id) => {
                library.set_play_counts(self.history.play_counts.clone());
                library.mix_now(id, &self.liked)
            }
            _ => library.cached_tracks(&source),
        };
        match &cached {
            Some(tracks) => self.show_tracks(source.clone(), tracks.clone()),
            None => self.ui.send(Event::SourceLoading { source: source.clone(), loaded: 0, total: None }),
        }
        // With a cached copy on screen, check for changes quietly (no progress events).
        let report_progress = cached.is_none();
        self.open_req = self.spawn_tracks(library, source, force_refresh, TracksPurpose::Open { shown: cached }, report_progress);
    }

    /// Load a source's tracks in the background; the result comes back as `Job::Tracks`.
    pub(super) fn spawn_tracks(
        &mut self,
        library: Library,
        source: Source,
        force_refresh: bool,
        purpose: TracksPurpose,
        report_progress: bool,
    ) -> u64 {
        let id = self.new_id();
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        let ui = report_progress.then(|| self.ui.clone());
        if matches!(source, Source::Mix(_)) {
            // A mix is made with the play counts as they are now.
            library.set_play_counts(self.history.play_counts.clone());
        }
        tokio::spawn(async move {
            let progress = jobs::progress_reporter(ui, source.clone());
            let result = library.tracks(&source, force_refresh, &progress).await;
            let _ = tx.send(Job::Tracks { epoch, id, source, purpose, result });
        });
        id
    }

    /// Songs taken out of a list no longer count as heard in its round, so "47 / 312" and RESUME
    /// stay true after removing songs.
    fn forget_removed_from_round(&mut self, source: &Source, tracks: &[TrackInfo]) {
        // An empty answer is more likely a hiccup than an emptied list: keep the round.
        if tracks.is_empty() {
            return;
        }
        let Some(heard) = self.history.played_in_cycle.get_mut(&source.cache_key()) else { return };
        let before = heard.len();
        let present: HashSet<&str> = tracks.iter().map(|t| t.uri.as_str()).collect();
        heard.retain(|uri| present.contains(uri.as_str()));
        if heard.len() != before {
            self.state_dirty = true;
            self.round_dirty = true;
            self.upnext_dirty = true;
        }
    }

    pub(super) fn on_tracks(&mut self, id: u64, source: Source, purpose: TracksPurpose, result: Result<Arc<Vec<TrackInfo>>>) {
        match &result {
            // Whatever asked for it, a fresh list of the playing source keeps the queue current.
            Ok(tracks) => {
                self.forget_removed_from_round(&source, tracks);
                self.refresh_queue_tracks(&source, tracks);
            }
            // Liked Songs may have gone through the Web API.
            Err(e) => self.webapi_failed(e),
        }
        match purpose {
            TracksPurpose::Open { shown } => {
                if id != self.open_req {
                    return;
                }
                match result {
                    Ok(tracks) => {
                        let unchanged = shown.as_ref().is_some_and(|old| Arc::ptr_eq(old, &tracks) || **old == *tracks);
                        if !unchanged {
                            self.show_tracks(source, tracks);
                        } else if let Some(info) = self.library.as_ref().and_then(|l| l.source_info(&source)) {
                            // An album or artist shown from the saved copy gets its name and
                            // picture once they have been read.
                            self.ui.send(Event::SourceInfo(info));
                        }
                    }
                    Err(e) if shown.is_some() => {
                        log::warn!("could not refresh {source:?}: {e:#}");
                        self.ui.send(Event::Status(
                            "Couldn't check this list for changes. Showing the saved copy.".into(),
                        ));
                    }
                    Err(e) => {
                        let message = user_error("Couldn't load these tracks.", &e);
                        self.ui.send(Event::SourceFailed { source, message });
                    }
                }
            }
            TracksPurpose::Play { start_index, shuffle } => {
                if id != self.play_req {
                    return;
                }
                let on_screen = self.open_source.as_ref() == Some(&source);
                let shown = self.shown.as_ref().is_some_and(|(s, _)| *s == source);
                match result {
                    Ok(tracks) => {
                        if on_screen && !shown {
                            self.show_tracks(source.clone(), tracks.clone());
                        }
                        self.start_source(source, tracks, start_index, shuffle);
                    }
                    Err(e) => {
                        let message = user_error("Couldn't load the tracks to play.", &e);
                        if on_screen && !shown {
                            self.ui.send(Event::SourceFailed { source, message: message.clone() });
                        }
                        self.ui.send(Event::Error(message));
                    }
                }
            }
            TracksPurpose::Restore { saved } => {
                // Listening along, the queue is the host's song: the restored one goes aside.
                if id != self.play_req || (self.queue.is_some() && !self.listening()) {
                    return;
                }
                match result {
                    Ok(tracks) => self.apply_restore(&saved, tracks),
                    Err(e) if library::is_permanent(&e) => {
                        // Deleted, or not visible to this account: retrying at every login can't help.
                        log::info!("dropping the saved queue, its list is gone: {e:#}");
                        self.saved_queue = None;
                        self.state_dirty = true;
                    }
                    Err(e) => log::warn!("could not load the tracks of the saved queue: {e:#}"),
                }
            }
        }
    }

    pub(super) fn show_tracks(&mut self, source: Source, tracks: Arc<Vec<TrackInfo>>) {
        if let Some(info) = self.library.as_ref().and_then(|l| l.source_info(&source)) {
            self.ui.send(Event::SourceInfo(info));
        }
        self.shown = Some((source.clone(), tracks.clone()));
        self.ui.send(Event::SourceLoaded { source, tracks });
        self.youtube_search_ahead();
    }

    /// Bring back the queue saved at the last shutdown (cached tracks first, else fetch them),
    /// if it belongs to the account that just logged in.
    fn restore_queue(&mut self) {
        if self.queue.is_some() {
            return;
        }
        let Some(saved) = self.saved_queue.clone() else { return };
        let Some(library) = self.library.clone() else { return };
        let me = self.profile.as_ref().map(|p| p.username.as_str()).unwrap_or_default();
        if !saved.username.is_empty() && !saved.username.eq_ignore_ascii_case(me) {
            // Kept (and saved again) for when that account logs in here again.
            log::info!("the saved queue belongs to another Spotify account; not restoring it");
            return;
        }
        match library.cached_tracks(&saved.source) {
            Some(tracks) => self.apply_restore(&saved, tracks),
            None => {
                let source = saved.source.clone();
                self.play_req = self.spawn_tracks(library, source, false, TracksPurpose::Restore { saved }, false);
            }
        }
    }

    // ------------------------------------------------------------ Web API (editing)

    pub(super) fn cmd_connect_webapi(&mut self, client_id: String) {
        let client_id = client_id.trim().to_owned();
        if client_id.is_empty() {
            self.ui.send(Event::WebApi {
                connected: false,
                message: Some("Paste the Client ID of your Spotify developer app first.".into()),
            });
            return;
        }
        // The Web API module validates the id and explains problems in the WebApi event.
        self.ui.send(Event::Status(
            "Opening your browser so you can allow Truffle to edit your playlists…".into(),
        ));
        self.spawn_webapi_connect(client_id, true);
    }

    pub(super) fn spawn_webapi_connect(&mut self, client_id: String, interactive: bool) {
        let id = self.new_id();
        self.webapi_req = id;
        let (paths, tx) = (self.paths.clone(), self.jobs.clone());
        tokio::spawn(async move {
            let result = WebApi::connect(&client_id, &paths, interactive).await;
            // Which account it is signed in to, so it is never used for another one.
            let user = match &result {
                Ok(Some(api)) => api
                    .user_id()
                    .await
                    .map_err(|e| log::info!("could not read the Web API account yet: {e:#}"))
                    .ok(),
                _ => None,
            };
            let _ = tx.send(Job::WebApi { id, client_id, interactive, result, user });
        });
    }

    pub(super) fn on_webapi(
        &mut self,
        id: u64,
        client_id: String,
        interactive: bool,
        result: Result<Option<WebApi>>,
        user: Option<String>,
    ) {
        if id != self.webapi_req {
            return;
        }
        match result {
            Ok(Some(api)) => {
                // Remember the id only once it worked, so a typo is never saved.
                self.update_settings(|s| s.webapi_client_id = Some(client_id));
                let message = if interactive {
                    Some("Connected. You can now rename playlists and change their covers.".to_owned())
                } else {
                    api.days_until_reconnect().filter(|&days| days <= WEBAPI_REMINDER_DAYS).map(reconnect_reminder)
                };
                self.webapi = Some(api);
                self.webapi_user = user;
                self.webapi_verify = None;
                self.check_webapi_account(message);
            }
            Ok(None) => {
                // Non-interactive: a client id is saved but its sign-in is gone (expired after
                // about 6 months, or revoked).
                let message = if interactive {
                    "Spotify didn't finish connecting. Please try again."
                } else {
                    "Playlist editing needs to be connected again: open Settings and click Connect."
                };
                self.ui.send(Event::WebApi { connected: false, message: Some(message.to_owned()) });
            }
            Err(e) => {
                let lead = if interactive {
                    "Couldn't connect playlist editing."
                } else {
                    "Playlist editing couldn't be reconnected. Open Settings and click Connect to try again."
                };
                self.ui.send(Event::WebApi { connected: false, message: Some(user_error(lead, &e)) });
            }
        }
    }

    fn webapi_fit(&self) -> WebApiFit {
        if self.webapi.is_none() {
            return WebApiFit::Missing;
        }
        let Some(api_user) = &self.webapi_user else { return WebApiFit::Unverified };
        match self.session.as_ref().map(Session::username) {
            Some(me) if !me.is_empty() && !me.eq_ignore_ascii_case(api_user) => WebApiFit::OtherAccount(api_user.clone()),
            _ => WebApiFit::Matches,
        }
    }

    /// Give the library the Web API client only if it is signed in to the logged-in account, and
    /// tell the UI. `message` goes with a usable connection (e.g. "Connected").
    fn check_webapi_account(&mut self, message: Option<String>) {
        let fit = self.webapi_fit();
        let usable = (fit == WebApiFit::Matches).then(|| self.webapi.clone()).flatten();
        if let Some(library) = self.library.as_mut() {
            library.set_webapi(usable);
        }
        match fit {
            WebApiFit::Missing => {}
            WebApiFit::Unverified => {
                if self.session.is_some() {
                    self.spawn_webapi_verify();
                }
                if message.is_some() {
                    self.ui.send(Event::WebApi { connected: true, message });
                }
            }
            WebApiFit::OtherAccount(api_user) => {
                let message = self.other_account_message(&api_user);
                log::warn!("the Web API sign-in belongs to {api_user}, not the logged-in account");
                self.ui.send(Event::WebApi { connected: false, message: Some(message) });
            }
            WebApiFit::Matches => self.ui.send(Event::WebApi { connected: true, message }),
        }
    }

    fn other_account_message(&self, api_user: &str) -> String {
        let me = self.profile.as_ref().map(|p| p.display_name.as_str()).filter(|n| !n.is_empty()).unwrap_or("this account");
        format!(
            "Playlist editing is signed in to a different Spotify account ({api_user}) than the one you're \
             logged in with ({me}), so it is off. To use it, open Settings, click Connect and choose your \
             account in the browser."
        )
    }

    /// Ask Spotify which account the Web API client is signed in to.
    fn spawn_webapi_verify(&mut self) {
        let Some(api) = self.webapi.clone() else { return };
        let id = self.webapi_req;
        if self.webapi_verify == Some(id) {
            return;
        }
        self.webapi_verify = Some(id);
        let tx = self.jobs.clone();
        tokio::spawn(async move {
            let result = api.user_id().await;
            let _ = tx.send(Job::WebApiUser { id, result });
        });
    }

    pub(super) fn on_webapi_user(&mut self, id: u64, result: Result<String>) {
        if self.webapi_verify == Some(id) {
            self.webapi_verify = None;
        }
        if id != self.webapi_req || self.webapi.is_none() {
            return;
        }
        match result {
            Ok(user) => {
                self.webapi_user = Some(user);
                self.check_webapi_account(None);
            }
            Err(e) => {
                log::warn!("could not check which account playlist editing uses: {e:#}");
                self.webapi_failed(&e);
            }
        }
    }

    /// Any Web API call can find that the sign-in is gone (expired after about 6 months, revoked,
    /// rejected). Turn editing off and tell the UI, whichever feature ran into it.
    pub(super) fn webapi_failed(&mut self, e: &anyhow::Error) {
        if !webapi::is_reconnect_required(e) || self.webapi.is_none() {
            return;
        }
        self.drop_webapi();
        let message = user_error("Playlist editing needs to be connected again.", e);
        self.ui.send(Event::WebApi { connected: false, message: Some(message) });
    }

    fn drop_webapi(&mut self) {
        if let Some(library) = self.library.as_mut() {
            library.set_webapi(None);
        }
        self.webapi = None;
        self.webapi_user = None;
        self.webapi_verify = None;
    }

    pub(super) fn cmd_cancel_webapi_connect(&mut self) {
        // The running attempt's result is stale from here on; then stop its wait for the browser.
        self.webapi_req = self.new_id();
        self.webapi_verify = None;
        webapi::cancel_pending_login();
        let connected = matches!(self.webapi_fit(), WebApiFit::Matches | WebApiFit::Unverified);
        self.ui.send(Event::WebApi { connected, message: Some("Connecting was cancelled.".into()) });
    }

    pub(super) fn cmd_disconnect_webapi(&mut self) {
        // Also makes any connection attempt still running stale.
        self.webapi_req = self.new_id();
        self.drop_webapi();
        // Turned off on purpose: don't try (and complain) again at the next start.
        self.update_settings(|s| s.webapi_client_id = None);
        let message = match WebApi::forget(&self.paths) {
            Ok(()) => "Playlist editing is turned off.".to_owned(),
            Err(e) => user_error("Playlist editing is turned off, but Truffle couldn't delete its saved sign-in.", &e),
        };
        self.ui.send(Event::WebApi { connected: false, message: Some(message) });
    }

    /// The Web API client and playlist id for an edit, or an explanation for the user.
    /// The library to change a playlist through (the session: no Web API needed), and the
    /// playlist's uri; else tells the user why not.
    pub(super) fn edit_target(&mut self, uri: &str) -> Option<(Library, String)> {
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::Error("Log in to Spotify first.".into()));
            return None;
        };
        if webapi::playlist_id_from_uri(uri).is_none() {
            self.ui.send(Event::Error("Only playlists can be edited.".into()));
            return None;
        }
        if self.playlists.iter().any(|p| p.uri == uri && !p.editable) {
            self.ui.send(Event::Error("You can only change playlists that you own or collaborate on.".into()));
            return None;
        }
        Some((library, uri.to_owned()))
    }

    pub(super) fn cmd_update_playlist_details(&mut self, uri: String, name: Option<String>, description: Option<String>) {
        let Some((api, playlist)) = self.edit_target(&uri) else { return };
        let name = name.map(|n| n.trim().to_owned());
        if name.as_deref() == Some("") {
            self.ui.send(Event::Error("A playlist needs a name.".into()));
            return;
        }
        let description = description.map(|d| d.trim().to_owned());
        if name.is_none() && description.is_none() {
            return;
        }
        self.ui.send(Event::Status("Saving playlist details…".into()));
        let edit = PlaylistEdit::Details { name: name.clone(), description: description.clone() };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = jobs::update_details(api, playlist, name, description).await;
            let _ = tx.send(Job::PlaylistEdited { epoch, uri, edit, result });
        });
    }

    pub(super) fn cmd_set_playlist_cover(&mut self, uri: String, image_path: PathBuf) {
        let Some((api, playlist)) = self.edit_target(&uri) else { return };
        if !image_path.is_file() {
            self.ui.send(Event::Error(
                "Truffle couldn't find that image file. Pick a JPEG, PNG or WebP picture.".into(),
            ));
            return;
        }
        self.ui.send(Event::Status("Uploading the new cover…".into()));
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = jobs::upload_cover(api, playlist, image_path).await;
            let _ = tx.send(Job::PlaylistEdited { epoch, uri, edit: PlaylistEdit::Cover, result });
        });
    }

    pub(super) fn on_playlist_edited(&mut self, uri: String, edit: PlaylistEdit, result: Result<Option<PlaylistSummary>>) {
        let fetched = match result {
            Ok(fetched) => fetched,
            Err(e) => {
                let lead = match edit {
                    PlaylistEdit::Details { .. } => "Couldn't save the playlist's name or description.",
                    PlaylistEdit::Cover => "Couldn't change the playlist cover.",
                };
                // The sign-in expired or was revoked: editing is off until the user reconnects.
                self.webapi_failed(&e);
                self.ui.send(Event::Error(user_error(lead, &e)));
                return;
            }
        };
        let done = match edit {
            PlaylistEdit::Details { .. } => "Playlist details saved.",
            PlaylistEdit::Cover => "Cover changed. It can take a minute to show up everywhere.",
        };
        // What changed, kept so a playlist list fetched before this edit can't undo it.
        let mut patch = PlaylistPatch::default();
        match edit {
            PlaylistEdit::Details { name, description } => {
                patch.name = name;
                patch.description = description;
            }
            PlaylistEdit::Cover => {
                patch.image_url = fetched.as_ref().and_then(|f| f.image_url.clone());
                // Through the Web API backup the address isn't known yet: fetched again soon.
                if patch.image_url.is_none() {
                    self.request_library(true);
                }
            }
        }
        // Patch our copy (keeps the library's revision, owner and track count).
        let index = self.playlists.iter().position(|p| p.uri == uri);
        let mut summary = match (index, &fetched) {
            (Some(i), _) => self.playlists[i].clone(),
            (None, Some(fetched)) => fetched.clone(),
            (None, None) => {
                self.ui.send(Event::Status(done.into()));
                return;
            }
        };
        summary.uri.clone_from(&uri);
        patch.apply(&mut summary);
        if let Some(i) = index {
            self.playlists[i] = summary.clone();
        }
        patch.confirmed = self.new_id();
        let pending = self.playlist_edits.entry(uri).or_default();
        pending.confirmed = patch.confirmed;
        if patch.name.is_some() {
            pending.name = patch.name;
        }
        if patch.description.is_some() {
            pending.description = patch.description;
        }
        if patch.image_url.is_some() {
            pending.image_url = patch.image_url;
        }
        self.ui.send(Event::PlaylistUpdated(summary));
        self.ui.send(Event::Status(done.into()));
    }
}

/// "Reconnect soon" text for a Web API sign-in that Spotify will end in `days` days.
fn reconnect_reminder(days: u64) -> String {
    let when = match days {
        0 => "today".to_owned(),
        1 => "tomorrow".to_owned(),
        n => format!("in {n} days"),
    };
    format!(
        "Spotify ends Truffle's playlist-editing sign-in {when}. To renew it, open Settings, click Disconnect, then Connect again."
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::Receiver;

    use super::*;
    use super::{PlaylistPatch, reconnect_reminder};
    use crate::config::{Paths, Settings};
    use crate::controller::UiLink;
    use crate::library::likes::SendError;
    use crate::model::{PlaylistSummary, UserProfile};

    /// A controller logged in as `user` (its session never connects) with its files in `paths`,
    /// and what it sends the UI.
    fn controller_at(paths: Paths, user: &str) -> (Controller, Receiver<Event>) {
        let (events, rx) = std::sync::mpsc::channel();
        let ui = UiLink { events, ctx: egui::Context::default() };
        let (jobs, _) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Controller::new(paths.clone(), Arc::new(Mutex::new(Settings::default())), ui, jobs);
        let session = librespot_core::Session::new(librespot_core::SessionConfig::default(), None);
        c.library = Some(Library::new(session, paths));
        c.profile = Some(UserProfile { username: user.into(), display_name: user.into(), image_url: None, premium: Some(true) });
        (c, rx)
    }

    fn controller(name: &str, user: &str) -> (Controller, Receiver<Event>) {
        let dir = std::env::temp_dir().join(format!("truffle-account-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        controller_at(Paths { config: dir.join("config"), cache: dir.join("cache") }, user)
    }

    fn song(n: usize) -> String {
        format!("spotify:track:{n:0>22}")
    }

    /// Runs the like sender with `send` as Spotify, handing the controller what it says, until
    /// nothing waits and a big change has said how it went.
    async fn send_likes<F, Fut>(c: &mut Controller, send: F)
    where
        F: FnMut(Vec<String>, bool) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), SendError>> + Send + 'static,
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(likes::run(c.likes.clone(), "alex".into(), send, move |r| {
            let _ = tx.send(r);
        }));
        while !(c.likes.waiting("alex").is_empty() && c.big_likes.is_empty()) {
            let report = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("the sender reports").expect("it runs");
            c.on_likes_report(report);
        }
        task.abort();
    }

    fn statuses(said: &[Event]) -> Vec<&str> {
        said.iter().filter_map(|e| if let Event::Status(s) = e { Some(s.as_str()) } else { None }).collect()
    }

    #[test]
    fn a_big_like_says_what_happened() {
        assert_eq!(like_news(true, 2_000, 8_000), "2,000 liked, 8,000 waiting");
        assert_eq!(like_news(true, 0, 10_000), "0 liked, 10,000 waiting");
        assert_eq!(like_news(false, 1, 12), "1 removed, 12 to remove");
        assert_eq!(like_news(true, 10_000, 0), "10,000 songs added to Liked Songs");
        assert_eq!(like_news(false, 1, 0), "1 song removed from Liked Songs");
    }

    #[tokio::test]
    async fn a_big_like_that_has_to_wait_keeps_its_hearts_and_says_how_it_went() {
        let (mut c, events) = controller("big-like", "alex");
        let songs: Vec<String> = (0..500).map(song).collect();
        let older = song(9_999);
        c.cmd_set_liked_many(songs.clone(), true);
        // Spotify's list, read before any of them went: their hearts stay, and they count.
        let before = c.new_id();
        c.liked_req = before;
        c.on_liked(before, Ok(HashSet::from([older.clone()])));
        assert_eq!(c.liked.len(), 501);
        // Spotify takes the first 200, asks to wait, then takes the rest.
        let calls = Arc::new(AtomicUsize::new(0));
        send_likes(&mut c, move |_, _| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move { if n == 1 { Err(SendError::Slow(Some(Duration::from_millis(5)))) } else { Ok(()) } }
        })
        .await;
        let said: Vec<Event> = events.try_iter().collect();
        assert_eq!(statuses(&said), ["200 liked, 300 waiting", "500 songs added to Liked Songs"]);
        assert!(!said.iter().any(|e| matches!(e, Event::Error(_))), "no error while they only wait");
        let shown: Vec<LikesWaiting> = said.iter().filter_map(|e| if let Event::LikesWaiting(w) = e { Some(*w) } else { None }).collect();
        assert_eq!(shown.first(), Some(&LikesWaiting { likes: 500, unlikes: 0 }), "more than one write: shown at once");
        assert!(shown.windows(2).all(|w| w[1].likes < w[0].likes), "counts down");
        assert_eq!(shown.last(), Some(&LikesWaiting::default()), "gone at none");
        assert_eq!(c.liked.len(), 501);
        // A list read before Spotify took them doesn't undo them; one read after has them.
        c.on_liked(before, Ok(HashSet::from([older.clone()])));
        assert_eq!(c.liked.len(), 501);
        let after = c.new_id();
        c.liked_req = after;
        c.on_liked(after, Ok(songs.into_iter().chain([older]).collect()));
        assert_eq!(c.liked.len(), 501);
        assert!(c.like_changes.is_empty());
    }

    #[tokio::test]
    async fn a_read_between_a_write_and_its_report_keeps_the_hearts() {
        let (mut c, events) = controller("read-between", "alex");
        let older = song(9_999);
        let songs: Vec<String> = (0..3).map(song).collect();
        c.cmd_set_liked_many(songs.clone(), true);
        // A read of Liked Songs that started before they went.
        let read = c.new_id();
        c.liked_req = read;
        // The sender takes them out of the queue; its report is still on its way.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(likes::run(c.likes.clone(), "alex".into(), |_, _| async { Ok::<(), SendError>(()) }, move |r| {
            let _ = tx.send(r);
        }));
        let report = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("the sender reports").expect("it runs");
        task.abort();
        assert!(c.likes.waiting("alex").is_empty());
        // One of them unliked again meanwhile; then the read comes back, without any of them.
        c.cmd_set_liked(songs[0].clone(), false);
        c.on_liked(read, Ok(HashSet::from([older.clone()])));
        assert_eq!(c.liked, HashSet::from([older.clone()]));
        c.on_likes_report(report);
        let want = HashSet::from([songs[1].clone(), songs[2].clone(), older]);
        assert_eq!(c.liked, want);
        assert_eq!(persist::load_liked(&c.paths, "alex"), Some(want.clone()), "saved with them");
        let last = events.try_iter().filter_map(|e| if let Event::LikedSet(s) = e { Some(s) } else { None }).last();
        assert_eq!(last.as_deref(), Some(&want));
        // The same report again changes nothing and tells the UI nothing.
        c.on_likes_report(Report::Sent { uris: songs.clone(), liked: true });
        assert_eq!(c.liked, want);
        assert!(!events.try_iter().any(|e| matches!(e, Event::LikedSet(_))));
    }

    #[tokio::test]
    async fn a_big_like_finishes_when_some_of_it_changes_before_it_goes() {
        let (mut c, events) = controller("big-changes", "alex");
        let first: Vec<String> = (0..500).map(song).collect();
        let second: Vec<String> = (1_000..1_300).map(song).collect();
        c.cmd_set_liked_many(first.clone(), true);
        // One of them unliked before it went, a second big like and a big unlike of some of both,
        // all while the first waits.
        c.cmd_set_liked(first[450].clone(), false);
        c.cmd_set_liked_many(second.clone(), true);
        let unlike: Vec<String> = first[100..200].iter().chain(&second[..50]).cloned().collect();
        c.cmd_set_liked_many(unlike.clone(), false);
        send_likes(&mut c, |_, _| async { Ok::<(), SendError>(()) }).await;
        let said: Vec<Event> = events.try_iter().collect();
        assert_eq!(
            statuses(&said),
            ["399 songs added to Liked Songs", "250 songs added to Liked Songs", "150 songs removed from Liked Songs"]
        );
        assert_eq!(c.liked.len(), 399 + 250);
        assert!(!c.liked.contains(&first[450]) && !unlike.iter().any(|u| c.liked.contains(u)));
        assert!(c.big_likes.is_empty());
    }

    #[tokio::test]
    async fn a_song_spotify_refuses_gets_its_heart_back_and_no_error() {
        let (mut c, events) = controller("refused", "alex");
        let (bad, good) = (song(1), song(2));
        c.cmd_set_liked_many(vec![bad.clone(), good.clone()], true);
        assert_eq!(c.liked, HashSet::from([bad.clone(), good.clone()]));
        send_likes(&mut c, move |uris, _| {
            let refused = uris.contains(&bad);
            async move { if refused { Err(SendError::Refused("400 invalid id".into())) } else { Ok(()) } }
        })
        .await;
        assert_eq!(c.liked, HashSet::from([good]));
        let said: Vec<Event> = events.try_iter().collect();
        assert_eq!(statuses(&said), ["1 song added to Liked Songs"]);
        assert!(!said.iter().any(|e| matches!(e, Event::Error(_) | Event::LikesWaiting(_))), "nothing to say while it went straight through");
    }

    #[tokio::test]
    async fn waiting_likes_belong_to_their_account_and_outlast_a_restart() {
        let (mut c, _events) = controller("accounts", "alex");
        c.cmd_set_liked(song(1), true);
        c.cmd_set_liked("spotify:local:Artist:Album:Song:180".into(), true);
        assert_eq!(c.liked, HashSet::from([song(1)]), "only Spotify songs");
        let paths = c.paths.clone();
        drop(c);
        // Back as sam: alex's like isn't sam's.
        let (mut c, _events) = controller_at(paths.clone(), "sam");
        c.liked_req = 3;
        c.on_liked(3, Ok(HashSet::new()));
        assert!(c.liked.is_empty());
        assert_eq!(c.likes.waiting("alex").likes, 1);
        drop(c);
        // Back as alex: Spotify's list doesn't have it yet, and its heart stays.
        let (mut c, _events) = controller_at(paths, "alex");
        c.liked_req = 3;
        c.on_liked(3, Ok(HashSet::from([song(2)])));
        assert_eq!(c.liked, HashSet::from([song(1), song(2)]));
    }

    #[test]
    fn reminder_wording() {
        assert!(reconnect_reminder(0).contains(" today."));
        assert!(reconnect_reminder(1).contains(" tomorrow."));
        assert!(reconnect_reminder(9).contains(" in 9 days."));
        assert!(reconnect_reminder(9).ends_with("then Connect again."));
    }

    #[test]
    fn playlist_patches_only_touch_edited_fields() {
        let mut p = PlaylistSummary {
            uri: "spotify:playlist:a".into(),
            name: "Old".into(),
            description: "desc".into(),
            owner_name: "me".into(),
            image_url: Some("old.jpg".into()),
            track_count: Some(3),
            revision: "r".into(),
            editable: true,
            owned: true,
            owner_id: String::new(),
        };
        let patch = PlaylistPatch { name: Some("Gym".into()), ..PlaylistPatch::default() };
        patch.apply(&mut p);
        assert_eq!((p.name.as_str(), p.description.as_str(), p.image_url.as_deref()), ("Gym", "desc", Some("old.jpg")));
    }
}
