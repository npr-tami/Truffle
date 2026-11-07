//! The backend brain: owns the session, engine, library, Web API client, play queue and play
//! history; turns UI `Command`s into actions and pushes `Event`s back.
//!
//! One async loop (`run`) multiplexes four inputs: UI commands, engine events, results of
//! background jobs, and a 1 s housekeeping tick. Anything that can take a while (logging in,
//! loading track lists, Web API calls, reconnecting) runs in a spawned task and reports back
//! through the internal job channel, so commands are always handled promptly. Every job carries
//! a request id (and, for session-bound work, the session epoch) so results that a newer request
//! has made obsolete are dropped.
//!
//! Submodules: `playback` (queue + engine), `account` (login, library, Web API, editing),
//! `extras` (equalizer, crossfade, sleep timer), `scrobble` (Last.fm), `listen` (listen along),
//! `links` (its links, one per friend), `jobs` (background job plumbing), `persist` (files owned
//! by the controller).

mod account;
mod browse;
mod extras;
mod jobs;
mod links;
mod listen;
mod persist;
mod playback;
mod scrobble;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use librespot_core::Session;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::config::{Paths, Settings};
use crate::engine::{Engine, EngineEvent, MAX_CROSSFADE_SECS};
use crate::lastfm::Scrobbler;
use crate::library::Library;
use crate::library::likes::LikeQueue;
use crate::listen::links::LinkBook;
use crate::model::{
    Command, Event, LikesWaiting, PlayStatus, PlaybackState, PlaylistSummary, RepeatMode, ShuffleRules, Source,
    TrackInfo, UserProfile,
};
use crate::queue::{PlayHistory, PlayQueue, SavedQueue};
use crate::webapi::WebApi;

use self::account::{BigLike, LikeChange, PlaylistPatch};
use self::extras::SleepPlan;
use self::jobs::Job;
use self::playback::ProbeKind;

pub type SharedSettings = Arc<Mutex<Settings>>;

/// Sends events to the UI and wakes it up.
#[derive(Clone)]
pub struct UiLink {
    pub events: std::sync::mpsc::Sender<Event>,
    pub ctx: egui::Context,
}

impl UiLink {
    pub fn send(&self, event: Event) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }
}

/// Housekeeping interval (play counting, session health, debounced saves).
const TICK: Duration = Duration::from_secs(1);
/// A tick this late means the computer was asleep (Windows counts sleep in `Instant`), so the time
/// in between was not playback.
const SUSPEND_GAP: Duration = Duration::from_secs(5);
/// Settings are written this long after the last change (a volume drag produces many changes).
const SETTINGS_SAVE_DELAY: Duration = Duration::from_secs(2);
/// state.json is written at most this often while something changed (or music is playing).
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(30);

/// Runs until `Command::Shutdown` or the command channel closes.
pub async fn run(paths: Paths, settings: SharedSettings, mut commands: UnboundedReceiver<Command>, ui: UiLink) {
    let (jobs_tx, mut jobs_rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let mut ctl = Controller::new(paths, settings, ui, jobs_tx);
    ctl.start();
    ctl.flush_playback();

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        // Crossfade start and sleep timer: recomputed after everything that could move them.
        let deadline = ctl.next_deadline();
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Command::Shutdown) | None => {
                        // Listeners hear that the music stopped (before the session goes).
                        ctl.share_stop().await;
                        ctl.shutdown();
                        // Only a scrobble batch already on its way is still waited for (briefly).
                        ctl.finish_lastfm_send(&mut jobs_rx).await;
                        break;
                    }
                    Some(command) => ctl.handle_command(command),
                }
            }
            event = recv_opt(&mut ctl.engine_rx) => {
                match event {
                    Some(event) => ctl.handle_engine_event(event),
                    None => ctl.engine_events_closed(),
                }
            }
            Some(job) = jobs_rx.recv() => ctl.handle_job(job),
            _ = tick.tick() => ctl.on_tick(),
            _ = sleep_until_opt(deadline) => ctl.on_deadline(),
        }
        ctl.flush_playback();
    }
    ctl.flush_playback();
    log::info!("controller stopped");
}

/// Receive from an optional channel; pends forever while there is none.
async fn recv_opt<T>(rx: &mut Option<UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Wait until `deadline`; pends forever without one.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// Listening time of the current track, for "count a play after 30 s".
#[derive(Default)]
struct Listen {
    /// Milliseconds heard before `since`.
    heard_ms: u64,
    /// Set while playing.
    since: Option<Instant>,
    /// A play was already recorded for this load.
    recorded: bool,
    /// The track was left without a counted play and marked as done for its shuffle cycle.
    seen: bool,
}

struct Controller {
    paths: Paths,
    settings: SharedSettings,
    ui: UiLink,
    jobs: UnboundedSender<Job>,
    next_id: u64,
    /// Bumped whenever the session is torn down; session-bound job results carry it.
    epoch: u64,

    // --- Session-bound ---
    session: Option<Session>,
    engine: Option<Engine>,
    engine_rx: Option<UnboundedReceiver<EngineEvent>>,
    library: Option<Library>,
    profile: Option<UserProfile>,
    playlists: Vec<PlaylistSummary>,
    /// Edits confirmed by Spotify, re-applied over playlist lists fetched before them.
    playlist_edits: HashMap<String, PlaylistPatch>,
    /// Playlists removed from the library, and when. Spotify's playlist list can still show them
    /// for a while, so they stay hidden until it stops (or for `REMOVED_HIDE_FOR`).
    removed_playlists: HashMap<String, Instant>,
    liked: HashSet<String>,
    /// Likes/unlikes Spotify took that a fetched Liked Songs list may not have yet (latest per
    /// URI), so older fetches can't flip the heart back. (The waiting ones are in `likes`.)
    like_changes: HashMap<String, LikeChange>,
    /// The task sending the logged-in account's likes and unlikes (see `library::likes`).
    like_sender: Option<JoinHandle<()>>,
    /// Likes and unlikes of many songs at once, oldest first, each until all of it went through.
    big_likes: Vec<BigLike>,
    /// The likes waiting as last sent to the UI (`None`: none shown).
    likes_shown: Option<LikesWaiting>,
    /// One queue per artist, so its follow and unfollow writes reach Spotify in the order clicked.
    follow_queues: HashMap<String, tokio::sync::mpsc::UnboundedSender<(Library, bool, u64)>>,

    // --- Web API (independent of the session) ---
    webapi: Option<WebApi>,
    /// Spotify account the Web API client is signed in to, once known.
    webapi_user: Option<String>,
    /// `webapi_req` of a running account check.
    webapi_verify: Option<u64>,
    webapi_req: u64,

    // --- Requests in flight (latest ids) ---
    login: Option<(u64, Instant)>,
    library_req: u64,
    liked_req: u64,
    open_req: u64,
    /// Latest PlaySource / queue restore; older track loads for playing are dropped.
    play_req: u64,
    reconnect: Option<(u64, JoinHandle<()>)>,
    /// An OpenSource that arrived while still logging in.
    pending_open: Option<(Source, bool)>,
    /// A Collection asked for while still logging in.
    pending_collection: bool,

    /// Source most recently opened in the UI, and the track list last sent for it.
    open_source: Option<Source>,
    shown: Option<(Source, Arc<Vec<TrackInfo>>)>,
    /// The most played song's details once found (its uri as asked, and the song), so Settings >
    /// About doesn't look it up again each time it shows.
    top_song: Option<(String, TrackInfo)>,

    // --- Playback ---
    queue: Option<PlayQueue>,
    /// Queue saved at the last shutdown that has not been restored yet.
    saved_queue: Option<SavedQueue>,
    history: PlayHistory,
    pb: PlaybackState,
    pb_dirty: bool,
    /// The "Up next" preview may have changed; recomputed and sent (if different) on flush.
    upnext_dirty: bool,
    last_upnext: Option<Arc<Vec<TrackInfo>>>,
    /// The record deck order last sent to the UI.
    last_deck: Option<Arc<crate::model::DeckState>>,
    /// The "played this cycle" sets may have changed; their sizes are sent (if different) on flush.
    round_dirty: bool,
    /// The round progress last sent to the UI.
    last_round: Option<Arc<HashMap<String, usize>>>,
    /// The last seek and when it arrived (repeats of it are dropped).
    last_seek: Option<(u32, Instant)>,
    /// Whether the user wants sound (as opposed to what the engine reports right now).
    want_play: bool,
    /// The engine does not have the current track loaded (deferred during a reconnect, or after
    /// the engine restarted): the next play/seek reloads it.
    pending_reload: bool,
    /// The queue ran out (repeat off); play starts it over.
    ended: bool,
    listen: Listen,
    end_handled: bool,
    preload_requested: bool,
    /// Consecutive tracks that failed to load; playback stops after a few to avoid looping.
    failures: u32,
    /// A reachability check after a failed or interrupted track (its id and why).
    probe: Option<(u64, ProbeKind)>,
    /// A fresh lookup of a track that failed on a working connection (its id and uri).
    recheck: Option<(u64, String)>,
    /// When Spotify last answered a reachability check.
    last_reachable: Option<Instant>,
    /// Times the current track stopped early and was resumed on a working connection.
    interruptions: u32,
    /// Reconnects waited for because of the current track.
    reconnect_waits: u32,

    // --- Audio extras (see `extras`) ---
    /// Crossfade length in milliseconds; 0 = off (gapless).
    crossfade_ms: u32,
    /// Finds and fetches songs on YouTube (the YouTube sound source); one for the whole run.
    youtube: crate::youtube::YouTube,
    /// Why the current song couldn't play from YouTube, until it is skipped.
    source_problem: Option<String>,
    /// The songs last given to YouTube to fetch ahead.
    fetching_ahead: Vec<String>,
    /// The current track's crossfade point has been dealt with (crossfaded, or decided not to).
    crossfade_checked: bool,
    sleep: Option<SleepPlan>,
    /// The sleep timer paused playback after fading the sound out: restore the fade gain once
    /// the pause has taken effect, or before anything plays again.
    fade_restore_pending: bool,

    /// When the last housekeeping tick ran (to notice the computer sleeping).
    last_tick: Instant,

    /// Listen along: following a host, and sharing what plays (see `listen`).
    along: listen::Along,
    /// The listen-along links, one per friend (see `links`). Not in `along`: they outlive a
    /// logout.
    links: LinkBook,
    /// Shown once at start: what became of `listen_links.json` if it couldn't be read.
    links_notice: Option<&'static str>,
    /// `listen_links.json` couldn't be opened at start: never saved over until it can be read
    /// (tried again at login).
    links_held: bool,
    /// Likes and unlikes on their way to Spotify, every account's (see `library::likes`). Not
    /// session-bound: they outlive a logout.
    likes: LikeQueue,

    // --- Last.fm (see `scrobble`) ---
    lastfm: Scrobbler,
    /// Changes for every new play of a track (see `begin_new_play`).
    play_id: u64,

    // --- Persistence ---
    state_dirty: bool,
    last_state_save: Instant,
    settings_changed_at: Option<Instant>,
    /// Shown once at start: state.json had to be recovered.
    state_notice: Option<String>,
    finished: bool,
}

impl Controller {
    fn new(paths: Paths, settings: SharedSettings, ui: UiLink, jobs: UnboundedSender<Job>) -> Self {
        let youtube = crate::youtube::YouTube::new(
            paths.cache.clone(),
            std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)),
        );
        youtube.set_disk_cache_mb(lock(&settings).audio_cache_mb);
        let persist::LoadedState { state, notice } = persist::load_state(&paths);
        // The listen-along links, with the channel older Truffles write to taken in (the one
        // channel from before links becomes the first link).
        let persist::LoadedLinks { book: mut links, notice: links_notice, held: links_held } = persist::load_links(&paths);
        let legacy = lock(&settings).listen_channel.clone();
        if links.take_in(legacy.as_deref()) {
            log::info!("listen along: took in the link older Truffles write to");
            if !links_held
                && let Err(e) = persist::save_links(&paths, &links)
            {
                log::warn!("could not save the listen-along links: {e:#}");
            }
        }
        let likes = LikeQueue::load(&paths.like_queue_file());
        let (volume, shuffle, repeat, crossfade_secs) = {
            let s = lock(&settings);
            (s.volume.min(100), s.shuffle, s.repeat, s.crossfade_secs.min(MAX_CROSSFADE_SECS))
        };
        let lastfm = {
            let snapshot = lock(&settings).clone();
            let (events, reports) = (ui.clone(), jobs.clone());
            Scrobbler::new(
                &paths,
                &snapshot,
                Arc::new(move |status| events.send(Event::Lastfm(status))),
                Arc::new(move |report| {
                    let _ = reports.send(Job::Lastfm(report));
                }),
            )
        };
        Self {
            paths,
            settings,
            ui,
            jobs,
            next_id: 0,
            epoch: 0,
            session: None,
            engine: None,
            engine_rx: None,
            library: None,
            profile: None,
            playlists: Vec::new(),
            playlist_edits: HashMap::new(),
            removed_playlists: HashMap::new(),
            liked: HashSet::new(),
            like_changes: HashMap::new(),
            like_sender: None,
            big_likes: Vec::new(),
            likes_shown: None,
            follow_queues: HashMap::new(),
            webapi: None,
            webapi_user: None,
            webapi_verify: None,
            webapi_req: 0,
            login: None,
            library_req: 0,
            liked_req: 0,
            open_req: 0,
            play_req: 0,
            reconnect: None,
            pending_open: None,
            pending_collection: false,
            open_source: None,
            shown: None,
            top_song: None,
            queue: None,
            saved_queue: state.queue,
            history: state.history,
            pb: PlaybackState { volume, shuffle, repeat, ..Default::default() },
            pb_dirty: true,
            upnext_dirty: false,
            last_upnext: None,
            last_deck: None,
            round_dirty: false,
            last_round: None,
            last_seek: None,
            want_play: false,
            pending_reload: false,
            ended: false,
            listen: Listen::default(),
            end_handled: false,
            preload_requested: false,
            failures: 0,
            probe: None,
            recheck: None,
            last_reachable: None,
            interruptions: 0,
            reconnect_waits: 0,
            crossfade_ms: u32::from(crossfade_secs) * 1000,
            youtube,
            source_problem: None,
            fetching_ahead: Vec::new(),
            crossfade_checked: false,
            sleep: None,
            fade_restore_pending: false,
            last_tick: Instant::now(),
            along: listen::Along::default(),
            links,
            links_notice,
            links_held,
            likes,
            lastfm,
            play_id: 0,
            state_dirty: false,
            last_state_save: Instant::now(),
            settings_changed_at: None,
            state_notice: notice,
            finished: false,
        }
    }

    fn start(&mut self) {
        if let Some(notice) = self.state_notice.take() {
            self.ui.send(Event::Error(notice));
        }
        if let Some(notice) = self.links_notice.take() {
            self.ui.send(Event::Status(notice.into()));
        }
        self.send_blocked();
        self.lastfm.start(Instant::now());
        let client_id = lock(&self.settings).webapi_client_id.clone();
        if let Some(client_id) = client_id.map(|c| c.trim().to_owned()).filter(|c| !c.is_empty()) {
            self.spawn_webapi_connect(client_id, false);
        }
        self.begin_login(false);
    }

    fn handle_command(&mut self, command: Command) {
        match &command {
            // Never log the Last.fm shared secret, or a friend's name.
            Command::ConnectLastfm { .. } => log::debug!("command: ConnectLastfm"),
            Command::ListenNewLink { .. } => log::debug!("command: ListenNewLink"),
            Command::ListenRenameLink { id, .. } => log::debug!("command: ListenRenameLink #{id}"),
            // (Not the whole playlist it carries.)
            Command::RemoveDuplicates { playlist_uri, rows, .. } => {
                log::debug!("command: RemoveDuplicates {playlist_uri} ({} rows)", rows.len())
            }
            command => log::debug!("command: {command:?}"),
        }
        if self.refused_while_listening(&command) {
            return;
        }
        match command {
            Command::Login { interactive } => self.begin_login(interactive),
            Command::Logout => self.cmd_logout(),
            Command::RefreshLibrary => {
                self.request_library(true);
                self.request_liked();
            }
            Command::OpenSource { source, force_refresh } => self.cmd_open_source(source, force_refresh),
            Command::PlaySource { source, start_index, shuffle } => self.cmd_play_source(source, start_index, shuffle),
            Command::NewRound { source } => self.cmd_new_round(source),
            Command::PlayPause => self.cmd_play_pause(),
            Command::Next => self.cmd_next(),
            Command::Previous => self.cmd_previous(),
            Command::SeekMs(position_ms) => self.cmd_seek(position_ms),
            Command::SetVolume(volume) => self.cmd_set_volume(volume),
            Command::SetShuffle(on) => self.cmd_set_shuffle(on),
            Command::SetRepeat(mode) => self.cmd_set_repeat(mode),
            Command::SetShuffleRules(rules) => self.cmd_set_shuffle_rules(rules),
            Command::Reshuffle => self.cmd_reshuffle(),
            Command::SetBlocked { uri, blocked } => self.cmd_set_blocked(uri, blocked),
            Command::SetLiked { uri, liked } => self.cmd_set_liked(uri, liked),
            Command::SetLikedMany { uris, liked } => self.cmd_set_liked_many(uris, liked),
            Command::Follow { uri, follow, generation } => self.cmd_follow(uri, follow, generation),
            Command::UserPage(username) => self.cmd_user_page(username),
            Command::ArtistLikes(artist) => self.cmd_artist_likes(artist),
            Command::FollowUser { username, follow } => self.cmd_follow_user(username, follow),
            Command::People { username, followers } => self.cmd_people(username, followers),
            Command::ConnectWebApi { client_id } => self.cmd_connect_webapi(client_id),
            Command::CancelWebApiConnect => self.cmd_cancel_webapi_connect(),
            Command::DisconnectWebApi => self.cmd_disconnect_webapi(),
            Command::UpdatePlaylistDetails { uri, name, description } => {
                self.cmd_update_playlist_details(uri, name, description)
            }
            Command::SetPlaylistCover { uri, image_path } => self.cmd_set_playlist_cover(uri, image_path),
            Command::SetEqualizer(eq) => self.cmd_set_equalizer(eq),
            Command::SetCrossfade(secs) => self.cmd_set_crossfade(secs),
            Command::SetSoundSource(source) => self.cmd_set_sound_source(source),
            Command::YouTubeMatches(track) => self.cmd_youtube_matches(*track),
            Command::ChooseYouTube { track, video_id } => self.cmd_choose_youtube(*track, video_id),
            Command::SetSleepTimer(timer) => self.cmd_set_sleep_timer(timer),
            Command::ConnectLastfm { api_key, api_secret } => self.cmd_connect_lastfm(api_key, api_secret),
            Command::CancelLastfmConnect => self.cmd_cancel_lastfm_connect(),
            Command::DisconnectLastfm => self.cmd_disconnect_lastfm(),
            Command::SetLastfmEnabled(on) => self.cmd_set_lastfm_enabled(on),
            Command::SendHeldLastfmScrobbles { to } => self.cmd_send_held_lastfm_scrobbles(to),
            Command::DiscardLastfmScrobbles { keep_for } => self.cmd_discard_lastfm_scrobbles(keep_for),
            Command::Search { query, offset } => self.cmd_search(query, offset),
            Command::SearchUsers { query, offset } => self.cmd_search_users(query, offset),
            Command::Lyrics(uri) => self.cmd_lyrics(uri),
            Command::Canvas(uri) => self.cmd_canvas(uri),
            Command::HeldBy(uris) => self.cmd_held_by(uris),
            Command::PlayStats => self.cmd_play_stats(),
            Command::Helpers => self.cmd_helpers(),
            Command::Collection => self.cmd_collection(),
            Command::Queue { track, next } => self.cmd_queue(*track, next),
            Command::Unqueue(at) => self.cmd_unqueue(at),
            Command::AddToPlaylist { playlist_uri, uris, again, question } => self.cmd_add_new(playlist_uri, uris, again, question),
            Command::RemoveFromPlaylist { playlist_uri, uris } => self.cmd_remove_items(playlist_uri, uris),
            Command::FindDuplicates(uri) => self.cmd_find_duplicates(uri),
            Command::RemoveDuplicates { playlist_uri, tracks, rows } => self.cmd_remove_duplicates(playlist_uri, tracks, rows),
            Command::CreatePlaylist { name } => self.cmd_create_playlist(name),
            Command::RemovePlaylist { uri } => self.cmd_remove_playlist(uri),
            Command::ListenAlong { how, name } => self.cmd_listen_along(how, name),
            Command::ListenStop => self.end_listening(None),
            Command::ListenShare(on) => self.cmd_listen_share(on),
            Command::ListenJoin(link) => self.cmd_listen_join(link),
            Command::ListenForget(uri) => self.cmd_listen_forget(uri),
            Command::ListenNewLink { name } => self.cmd_listen_new_link(name),
            Command::ListenRenameLink { id, name } => self.cmd_listen_rename_link(id, name),
            Command::ListenKick { id, kick } => self.cmd_listen_kick(id, kick),
            Command::ListenRemoveLink(id) => self.cmd_listen_remove_link(id),
            Command::ListenRemakeLink(id) => self.cmd_listen_remake_link(id),
            Command::Friends => self.cmd_friends(false),
            Command::FriendPeek(username) => self.cmd_friend_peek(username),
            Command::Shutdown => self.shutdown(),
        }
    }

    fn handle_job(&mut self, job: Job) {
        match job {
            Job::Login { id, interactive, result } => self.on_login(id, interactive, result),
            Job::Reconnected { id, session } => self.on_reconnected(id, session),
            Job::WebApi { id, client_id, interactive, result, user } => {
                self.on_webapi(id, client_id, interactive, result, user)
            }
            Job::WebApiUser { id, result } => self.on_webapi_user(id, result),
            Job::Reachability { epoch, id, reachable } => {
                if epoch == self.epoch {
                    self.on_reachability(id, reachable);
                }
            }
            Job::Recheck { epoch, id, uri, result } => {
                if epoch == self.epoch {
                    self.on_recheck(id, &uri, result);
                }
            }
            Job::Playlists { epoch, id, user, result } => {
                if epoch == self.epoch {
                    self.on_playlists(id, user, result);
                }
            }
            Job::Liked { epoch, id, result } => {
                if epoch == self.epoch {
                    self.on_liked(id, result);
                }
            }
            Job::Tracks { epoch, id, source, purpose, result } => {
                if epoch == self.epoch {
                    self.on_tracks(id, source, purpose, result);
                }
            }
            Job::Likes { epoch, report } => {
                if epoch == self.epoch {
                    self.on_likes_report(report);
                }
            }
            Job::PlaylistEdited { epoch, uri, edit, result } => {
                if epoch == self.epoch {
                    self.on_playlist_edited(uri, edit, result);
                }
            }
            Job::Lastfm(report) => self.on_lastfm_report(report),
            Job::Browse { epoch, result } => {
                if epoch == self.epoch {
                    self.on_browse_done(result);
                }
            }
            Job::Duplicates { epoch, uri, result } => {
                if epoch == self.epoch {
                    self.on_duplicates_read(uri, result);
                }
            }
            Job::Listen { epoch, done } => {
                if epoch == self.epoch {
                    self.on_listen_done(done);
                } else {
                    self.on_stale_listen_done(done);
                }
            }
        }
    }

    fn on_tick(&mut self) {
        let gap = self.last_tick.elapsed();
        self.last_tick = Instant::now();
        if gap > SUSPEND_GAP {
            self.discount_suspend(gap.saturating_sub(TICK));
        }
        self.record_if_heard();
        self.check_end_watchdog();
        self.poll_host();
        self.lastfm.on_tick(self.last_tick);
        if self.reconnect.is_none() && self.session.as_ref().is_some_and(|s| s.is_invalid()) {
            self.start_reconnect();
        }
        if self.settings_changed_at.is_some_and(|at| at.elapsed() >= SETTINGS_SAVE_DELAY) {
            self.save_settings();
        }
        // The audio cache size is changed in the UI; YouTube's files follow it at once.
        self.youtube.set_disk_cache_mb(lock(&self.settings).audio_cache_mb);
        self.check_selftest();
        // While playing, the saved position changes, so a periodic save keeps the resume point fresh.
        let playing = self.pb.status == PlayStatus::Playing;
        if (self.state_dirty || playing) && self.last_state_save.elapsed() >= STATE_SAVE_INTERVAL {
            self.save_state();
        }
    }

    /// A test aid: `truffle-selftest.request` beside truffle.exe ("search", or "edit" for the
    /// throwaway-playlist test too; "users <name>" for the people a search finds) runs
    /// `library::selftest` once Truffle is logged in; the report goes to `truffle-selftest.txt`
    /// there. "open <page>" only opens a page (see `Event::OpenPage`); "likes" writes the like
    /// queue's state there.
    fn check_selftest(&mut self) {
        let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf)) else { return };
        let request = dir.join("truffle-selftest.request");
        if !request.exists() {
            return;
        }
        let Some(library) = self.library.clone() else { return };
        let mode = std::fs::read_to_string(&request).unwrap_or_default().trim().to_owned();
        let writes = mode == "edit";
        // Gone before it runs, or it doesn't run (a file that stays would start it every second).
        if let Err(e) = std::fs::remove_file(&request) {
            log::warn!("self-test not started: couldn't remove its request file ({e})");
            return;
        }
        if let Some(args) = mode.strip_prefix("listen") {
            self.listen_selftest(args.trim(), &dir);
            return;
        }
        // "open home", "open profile", "open liked", "open likes <artist URI>" or "open <Spotify
        // URI>": the UI shows that page and says so; nothing plays.
        if let Some(page) = open_page(&mode) {
            log::info!("self-test: {mode}");
            self.ui.send(Event::OpenPage { page: page.to_owned(), report: dir.join("truffle-selftest.txt") });
            return;
        }
        if mode == "likes" {
            // Changes nothing: what waits for Spotify, what the sender does, and the last error.
            let user = self.profile.as_ref().map(|p| p.username.clone()).unwrap_or_default();
            let report = self.likes.report(&user);
            log::info!("self-test likes:\n{report}");
            let _ = std::fs::write(dir.join("truffle-selftest.txt"), report);
            return;
        }
        if let Some(rest) = mode.strip_prefix("canvas").filter(|rest| rest.is_empty() || rest.starts_with(' ')) {
            // "canvas" (the playing song and the next 20) or "canvas <uri> ...": which have a Canvas,
            // and what Spotify gives for it. Only reads.
            let mut uris: Vec<String> = rest.split_whitespace().map(str::to_owned).collect();
            if uris.is_empty() {
                uris.extend(self.pb.track.iter().map(|t| t.uri.clone()));
                let rules = self.rules();
                if let Some(queue) = &self.queue {
                    uris.extend(queue.upcoming_filtered(20, &rules, &self.history).into_iter().map(|t| t.uri.clone()));
                }
                uris.dedup();
            }
            let Some(session) = self.session.clone() else { return };
            log::info!("self-test: canvas for {} songs", uris.len());
            tokio::spawn(async move {
                let report = crate::canvas::report(&session, &uris).await;
                log::info!("self-test canvas:\n{report}");
                let _ = std::fs::write(dir.join("truffle-selftest.txt"), report);
            });
            return;
        }
        static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if writes && STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            log::warn!("self-test with changes already ran in this session; not again");
            return;
        }
        // Playback test aids: "source youtube|spotify", "next", "seek <seconds>", "playpause",
        // "status" (what plays, where, from which source; written to truffle-selftest.txt).
        let mut words = mode.split_whitespace();
        let control = match (words.next(), words.next()) {
            (Some("source"), Some("youtube")) => Some(Command::SetSoundSource(crate::config::SoundSource::YouTube)),
            (Some("source"), Some("spotify")) => Some(Command::SetSoundSource(crate::config::SoundSource::Spotify)),
            (Some("next"), _) => Some(Command::Next),
            (Some("playpause"), _) => Some(Command::PlayPause),
            (Some("seek"), Some(s)) => s.parse::<u32>().ok().map(|s| Command::SeekMs(s * 1000)),
            _ => None,
        };
        if let Some(command) = control {
            log::info!("self-test: {mode}");
            self.handle_command(command);
        }
        if control_word(&mode) {
            let track = self.pb.track.as_ref().map(|t| format!("{} - {} ({} s)", t.artist_line(), t.name, t.duration_ms / 1000)).unwrap_or_default();
            let status = format!(
                "{:?} | {track} | at {} s | sound from {:?}
",
                self.pb.status,
                self.pb.current_position_ms() / 1000,
                self.sound_source()
            );
            log::info!("self-test status: {status}");
            let _ = std::fs::write(dir.join("truffle-selftest.txt"), status);
            return;
        }
        if let Some(name) = mode.strip_prefix("users").filter(|rest| rest.is_empty() || rest.starts_with(' ')) {
            // "users <name>": how many people a search for them finds, and the first 20.
            let name = name.trim().to_owned();
            log::info!("self-test started (people found for \"{name}\")");
            tokio::spawn(async move {
                let report = crate::library::selftest::users(library, &name).await;
                log::info!("self-test:\n{report}");
                let _ = std::fs::write(dir.join("truffle-selftest.txt"), report);
            });
            return;
        }
        if let Some(rest) = mode.strip_prefix("youtube") {
            // "youtube <playlist name> <how many>": the first songs of that list, found and fetched.
            let mut words: Vec<&str> = rest.split_whitespace().collect();
            let count = words.last().and_then(|w| w.parse::<usize>().ok()).inspect(|_| {
                words.pop();
            });
            let name = words.join(" ");
            let source = self.playlists.iter().find(|p| p.name.eq_ignore_ascii_case(&name)).map(|p| Source::Playlist(p.uri.clone()));
            let youtube = self.youtube.clone();
            log::info!("self-test started (YouTube matches for \"{name}\")");
            tokio::spawn(async move {
                let report = youtube_test(library, youtube, source, count.unwrap_or(20)).await;
                log::info!("self-test:
{report}");
                let _ = std::fs::write(dir.join("truffle-selftest.txt"), report);
            });
            return;
        }
        let taken: Vec<String> = self.playlists.iter().map(|p| p.name.clone()).collect();
        log::info!("self-test started ({})", if writes { "search and playlist changes" } else { "search" });
        tokio::spawn(async move {
            let report = if mode == "presence" { crate::library::selftest::presence(library).await } else if mode == "follow" { crate::library::selftest::follow_test(library).await } else if mode == "probe" { crate::library::selftest::probe(library).await } else { crate::library::selftest::run(library, writes, taken).await };
            log::info!("self-test:
{report}");
            let _ = std::fs::write(dir.join("truffle-selftest.txt"), report);
        });
    }

    /// Save everything and release the audio device. Safe to call more than once.
    fn shutdown(&mut self) {
        if self.finished {
            return;
        }
        log::info!("shutting down");
        self.record_if_heard();
        // Finishes the current play (an eligible one is queued) and writes the scrobble queue.
        self.lastfm.shutdown(Instant::now());
        self.save_settings();
        self.save_state();
        self.finished = true;
        if let Some((_, task)) = self.reconnect.take() {
            task.abort();
        }
        // What it hadn't sent waits in its file for the next start.
        if let Some(task) = self.like_sender.take() {
            task.abort();
        }
        self.engine_rx = None;
        if let Some(engine) = self.engine.take() {
            engine.stop();
            drop_in_background(engine);
        }
        if let Some(session) = self.session.take() {
            session.shutdown();
        }
    }

    // ---------------------------------------------------------------- small helpers

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn settings_snapshot(&self) -> Settings {
        lock(&self.settings).clone()
    }

    /// The rules the shuffle follows now (with true shuffle on, only "skip blocked").
    fn rules(&self) -> ShuffleRules {
        lock(&self.settings).shuffle_rules.effective()
    }

    fn repeat(&self) -> RepeatMode {
        lock(&self.settings).repeat
    }

    /// Change controller-owned settings; the file is written shortly after the last change.
    /// The lock is held only for the closure.
    fn update_settings(&mut self, change: impl FnOnce(&mut Settings)) {
        let changed = {
            let mut settings = lock(&self.settings);
            let before = settings.clone();
            change(&mut settings);
            *settings != before
        };
        if changed {
            self.settings_changed_at = Some(Instant::now());
        }
    }

    fn save_settings(&mut self) {
        self.settings_changed_at = None;
        if let Err(e) = Settings::save_shared(&self.settings, &self.paths) {
            log::warn!("could not save settings: {e:#}");
        }
    }

    fn save_state(&mut self) {
        self.state_dirty = false;
        self.last_state_save = Instant::now();
        let live = self.saved_live_queue();
        let queue = live.as_ref().or(self.saved_queue.as_ref());
        if let Err(e) = persist::save_state(&self.paths, &self.history, queue) {
            log::warn!("could not save play history: {e:#}");
        }
    }

    fn send_blocked(&self) {
        self.ui.send(Event::BlockedSet(Arc::new(self.history.blocked.clone())));
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        // The runtime can be shut down before a Shutdown command is processed; save anyway.
        if !self.finished {
            self.finished = true;
            self.record_if_heard();
            self.lastfm.shutdown(Instant::now());
            self.save_settings();
            self.save_state();
        }
    }
}

/// Lock the shared settings, recovering from a poisoned lock instead of panicking.
fn lock(settings: &SharedSettings) -> MutexGuard<'_, Settings> {
    settings.lock().unwrap_or_else(|e| e.into_inner())
}

/// A plain-language error line for the UI. The other modules already word their errors for
/// people as full sentences; those are shown as they are. Anything else (a raw technical message)
/// is put in brackets after `lead`. The full error chain goes to the log.
fn user_error(lead: &str, err: &anyhow::Error) -> String {
    log::warn!("{lead} {err:#}");
    let detail = err.to_string();
    let detail = detail.trim();
    if detail.is_empty() {
        lead.to_owned()
    } else if detail.ends_with(['.', '!', '?']) {
        detail.to_owned()
    } else {
        format!("{lead} ({detail})")
    }
}

/// Drop something whose destructor may block (the engine joins its player thread) off the
/// async worker threads.
fn drop_in_background<T: Send + 'static>(value: T) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            // Fire and forget: nothing waits for the drop to finish.
            drop(handle.spawn_blocking(move || drop(value)));
        }
        Err(_) => drop(value),
    }
}

/// A test aid: finds and fetches the first `count` songs of a list on YouTube, one at a time, and
/// reports each match (its length beside Spotify's) and how long it took.
async fn youtube_test(library: Library, youtube: crate::youtube::YouTube, source: Option<Source>, count: usize) -> String {
    use std::fmt::Write as _;
    let Some(source) = source else { return "SOME FAILED
no playlist by that name".to_owned() };
    let tracks = match library.tracks(&source, false, &|_: usize, _: Option<usize>| {}).await {
        Ok(t) => t,
        Err(e) => return format!("SOME FAILED
couldn't load the list: {e:#}"),
    };
    let mut out = String::new();
    let (mut ok, mut far, mut failed) = (0, 0, 0);
    for t in tracks.iter().filter(|t| !t.is_local).take(count) {
        let started = Instant::now();
        match youtube.audio(t).await {
            Ok(a) => {
                let yt = a.duration_ms.unwrap_or(0);
                let diff = (i64::from(yt) - i64::from(t.duration_ms)).abs() / 1000;
                if diff > 5 {
                    far += 1;
                } else {
                    ok += 1;
                }
                let _ = writeln!(
                    out,
                    "{} {} - {} | {} s vs {} s | \"{}\" ({}) | {} KB | {:.1} s",
                    if diff > 5 { "FAR " } else { "OK  " },
                    t.artist_line(),
                    t.name,
                    t.duration_ms / 1000,
                    yt / 1000,
                    a.title,
                    a.video_id,
                    a.bytes.len() / 1024,
                    started.elapsed().as_secs_f32()
                );
            }
            Err(e) => {
                failed += 1;
                let _ = writeln!(out, "FAIL {} - {} | {e:#}", t.artist_line(), t.name);
            }
        }
    }
    let head = if failed == 0 { "ALL PASSED" } else { "SOME FAILED" };
    format!("{head}
{ok} matched, {far} off by more than 5 s, {failed} failed
{out}")
}

/// Whether a self-test request is one of the playback test aids.
fn control_word(mode: &str) -> bool {
    matches!(mode.split_whitespace().next(), Some("source" | "next" | "seek" | "playpause" | "status"))
}

/// The page a self-test's "open <page>" asks for ("" when it names none).
fn open_page(mode: &str) -> Option<&str> {
    mode.strip_prefix("open").filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace)).map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::{control_word, open_page, user_error};

    #[test]
    fn a_self_test_open_names_its_page_and_nothing_else_opens() {
        assert_eq!(open_page("open home"), Some("home"));
        assert_eq!(open_page("open\tlikes spotify:artist:x  "), Some("likes spotify:artist:x"));
        assert_eq!(open_page("open"), Some(""));
        // Other requests: not an open, and an open is no playback aid.
        for mode in ["opener", "openhome", "listen", "status", "search", ""] {
            assert_eq!(open_page(mode), None, "{mode:?}");
        }
        assert!(!control_word("open liked"));
    }

    #[test]
    fn user_errors() {
        let plain = anyhow::anyhow!("Spotify could not be reached. Check your internet connection.");
        assert_eq!(user_error("Couldn't load.", &plain), "Spotify could not be reached. Check your internet connection.");
        let technical = anyhow::anyhow!("error sending request for url");
        assert_eq!(user_error("Couldn't load.", &technical), "Couldn't load. (error sending request for url)");
    }
}
