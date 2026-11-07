//! egui user interface.
//!
//! Layout: playlists (left), Home or a list's page with Songs / Up next / Blocked tabs and a status
//! line (middle), the record deck (right, Ctrl+D hides it), player bar (bottom); or the compact
//! mini player. The side panels keep the widths the user drags them to, also after a restart.
//! Settings opens in a window of its own docked beside the main one, as in Glint (or small in its
//! middle, or in place of the three columns: "Settings opens"; see `settings.rs`). The UI never
//! blocks: it sends [`Command`]s to the controller and drains [`Event`]s in `App::logic`, which
//! also runs while the window is hidden in the tray or minimized.

mod accent;
mod blur;
mod collection;
mod narrow;
mod nav;
mod profile;
mod deck;
mod dnd;
mod dupes;
mod eq;
mod fin;
mod frame;
mod friends;
mod panel;
mod home;
mod keys;
mod icon_svg;
mod images;
mod kit;
mod library;
mod links;
mod login;
mod mini;
mod motion;
mod player;
mod screens;
mod search;
mod searchpage;
mod songmenu;
mod settings;
mod shell;
mod side;
mod sidebar;
mod stage;
mod svgicon;
mod theme;
mod tracks;
mod util;
mod wallpaper;
mod wide;
mod window;
mod ytpick;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use egui::{Align2, CornerRadius, Frame, Id, Margin, RichText, Sense};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Paths, PlaylistFilter, Settings};
use crate::controller::SharedSettings;
use crate::model::{
    Command, Event, LastfmStatus, LikesWaiting, PlayStatus, PlaybackState, PlaylistSummary, ShuffleRules, SleepTimerState,
    Source, TrackInfo, UserProfile, DeckState, SourceInfo,
};
use crate::text::{contains_folded, fold};

use self::accent::AccentFade;
use self::images::{ImageCache, Swatch};
use self::shell::Shell;
use self::theme::{Palette, ScriptFonts};
use self::tracks::TrackView;
use self::wallpaper::WallpaperState;
/// The controller says how a big like went in the same words.
pub(crate) use self::util::{fmt_song_count, fmt_thousands};

/// Title of the main window. The UI (and `main.rs`, before the first frame) finds the native
/// window handle by this title, so `main.rs` passes it to `with_title`.
pub const WINDOW_TITLE: &str = window::WINDOW_TITLE;

/// Smallest inner size of the full window, in logical pixels.
pub const MIN_WINDOW_SIZE: egui::Vec2 = window::FULL_MIN_SIZE;

/// The size to open the full window at: where the user last left it (logical pixels), or the
/// default. For `ViewportBuilder::with_inner_size` in `main.rs`.
/// Puts the still hidden main window at its remembered place (left, top, right, bottom), if that
/// screen is still connected.
pub fn restore_window_place(hwnd: isize, rect: [i32; 4]) {
    let [left, top, right, bottom] = rect;
    if right > left && bottom > top {
        if window::restore_placement(hwnd, window::ScreenRect { left, top, right, bottom }) {
            log::info!("window back at its place ({left}, {top}, {right}, {bottom})");
        } else {
            log::info!("the window's remembered place is off every screen; opening at the default place");
        }
    }
}

/// A close sent from outside to winit's hidden helper window closes the main window instead.
pub fn forward_helper_close(main: isize) {
    window::forward_helper_close(main);
}

pub fn initial_window_size(settings: &Settings) -> egui::Vec2 {
    window::full_size_or_default(settings.main_window_size)
}

/// Playlist name from `--play "<name>"`, played once when the playlist list first arrives.
static AUTOPLAY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Ask the UI to start a playlist (by name) as soon as the library has loaded.
pub fn set_autoplay(name: String) {
    *AUTOPLAY.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name);
}

fn take_autoplay() -> Option<String> {
    AUTOPLAY.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
}

/// The playlist a `--play` name refers to: an exact name match (case and accents aside), preferring
/// the one spelled as typed, then the user's own playlists, else the only playlist whose name
/// contains it. Spelled as typed comes first there too, so "Mötl" is "Mötley", not "Motley Mix".
fn find_playlist_by_name<'a>(list: &'a [PlaylistSummary], name: &str) -> Option<&'a PlaylistSummary> {
    let typed = name.trim().to_lowercase();
    let needle = fold(&typed);
    if needle.is_empty() {
        return None;
    }
    let mut exact: Vec<&PlaylistSummary> = list.iter().filter(|p| fold(p.name.trim()) == needle).collect();
    exact.sort_by_key(|p| (p.name.trim().to_lowercase() != typed, !p.owned));
    if let Some(p) = exact.first() {
        return Some(p);
    }
    only(list.iter().filter(|p| p.name.to_lowercase().contains(&typed)))
        .or_else(|| only(list.iter().filter(|p| contains_folded(&p.name, &needle))))
}

/// The one playlist in `hits`, or `None` for none or several.
fn only<'a>(mut hits: impl Iterator<Item = &'a PlaylistSummary>) -> Option<&'a PlaylistSummary> {
    match (hits.next(), hits.next()) {
        (Some(p), None) => Some(p),
        _ => None,
    }
}

const SETTINGS_SAVE_DELAY: Duration = Duration::from_millis(800);
/// A sleep timer this far past its end without word from the controller is dropped from view.
const SLEEP_TIMER_STALE: Duration = Duration::from_secs(60);
pub(super) const STATUS_TOAST_TIME: Duration = Duration::from_secs(5);
const MAX_TOASTS: usize = 5;
const PLAYLIST_SEARCH_ID: &str = "playlist_search";
const TRACK_FILTER_ID: &str = "track_filter";
/// The side panels: their ids, and their widths before the user drags them (logical points).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AuthState {
    /// Waiting for the controller to say whether a cached login exists.
    Starting,
    NeedLogin,
    LoggingIn,
    LoggedIn,
}

struct Toast {
    text: String,
    error: bool,
    created: Instant,
    undo: Option<Undo>,
}

/// A message in the stack (`toasts_ui`); one going while `left` is set (egui's clock).
#[derive(Clone)]
struct Stacked {
    text: String,
    error: bool,
    left: Option<f64>,
}

/// What Undo on a note (or Ctrl+Z) puts back.
#[derive(Clone, Debug)]
pub(crate) enum Undo {
    Rules(ShuffleRules),
    /// Block (`true`) or allow (`false`) these songs again.
    Block(Vec<String>, bool),
    /// Put these banners back (list's cache key -> picture), on lists without one meanwhile.
    Banners(std::collections::BTreeMap<String, crate::config::Banner>),
}

/// The tabs above the middle list, and Home (the house button left of them).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MiddleTab {
    Home,
    Songs,
    Blocked,
    Search,
    Albums,
    Artists,
    Profile,
    /// Who a user follows, or who follows them (`people_view`).
    People,
    /// Narrow widths: the playlists (the sidebar's lists), and the playing song's page.
    Library,
    Playing,
}

/// The playlist-details editor (name + description).
struct EditDetails {
    uri: String,
    name: String,
    description: String,
    original_name: String,
    original_description: String,
}

/// Follows and unfollows of one artist made in Truffle: the state shown (the latest asked for), its
/// number, the state before the first of them, the state Spotify last confirmed, and the artist.
#[derive(Clone)]
struct FollowChange {
    follow: bool,
    generation: u64,
    before: bool,
    confirmed: Option<bool>,
    item: crate::model::CollectionItem,
}

pub struct TruffleApp {
    ctx: egui::Context,
    commands: UnboundedSender<Command>,
    events: Receiver<Event>,
    settings: SharedSettings,
    paths: Paths,
    runtime: tokio::runtime::Handle,
    /// Windows integration and the window's state (tray, mini player).
    shell: Shell,

    /// Local copy of the settings. The UI owns theme/accent/ui_scale/panels/last_source and the
    /// audio options; it writes those back to `settings` and saves (debounced).
    prefs: Settings,
    settings_dirty_at: Option<Instant>,
    palette: Palette,
    /// The accent actually shown: the user's colour, or the album art's when that is on.
    accent_fade: AccentFade,
    /// The shown accent comes from album art (changes to and from it fade; the user's own picks
    /// apply at once).
    accent_from_art: bool,
    /// The interface size: the window's screen and width (`screens.rs`).
    screen_zoom: screens::ScreenZoom,

    auth: AuthState,
    /// The user clicked "Log in" (browser flow) rather than a cached login.
    interactive_login: bool,
    login_started: Option<Instant>,
    /// The logo and wordmark on the startup screen, made the first time it shows.
    brand: Option<login::Brand>,
    profile: Option<UserProfile>,
    reopened_last_source: bool,

    playlists: Vec<PlaylistSummary>,
    library_loaded: bool,
    /// The playlist list couldn't be loaded and there is nothing to show (offer a retry).
    library_error: Option<String>,
    playlist_search: String,
    /// Indices into `playlists` matching `playlist_search`.
    playlist_filtered: Vec<usize>,
    /// The sidebar's Pinned grid in its edit mode (Edit / Done).
    pin_edit: sidebar::PinEdit,

    view: Option<TrackView>,
    /// Scroll to this track once its source is shown.
    pending_scroll_uri: Option<String>,

    playback: PlaybackState,
    have_playback: bool,
    rules: ShuffleRules,
    liked: Arc<HashSet<String>>,
    /// Likes and unlikes waiting for Spotify (shown on Liked Songs).
    likes_waiting: LikesWaiting,
    blocked: Arc<HashSet<String>>,
    up_next: Option<Arc<Vec<TrackInfo>>>,
    sleep_timer: Option<SleepTimerState>,
    /// The playing source's whole play order, for the record.
    deck: Option<Arc<DeckState>>,
    /// Songs heard in each list's current shuffle round, by `Source::cache_key`.
    round_progress: Arc<std::collections::HashMap<String, usize>>,
    middle_tab: MiddleTab,
    /// Lists opened this session, the latest first (Home's "Jump back in").
    recent_sources: Vec<Source>,
    home: home::HomeState,
    settings_page: settings::Page,
    /// What is typed in Settings' search box.
    settings_query: String,
    search: search::SearchState,
    /// Names and pictures of album and artist pages.
    source_info: std::collections::HashMap<Source, SourceInfo>,
    /// Play this song once this list has loaded (a song picked from search plays from its album).
    pending_play: Option<(Source, String)>,
    /// The name being typed for a new playlist (the box is open while `Some`).
    new_playlist: Option<String>,
    /// The playlist "Delete…" was clicked for: the question box is open.
    confirm_remove: Option<String>,
    /// The "Already in" questions: one for each add that found songs the playlist has, asked one
    /// at a time.
    already_in: library::AlreadyInState,
    /// The open "Find duplicates" box.
    dupes: Option<dupes::Dupes>,

    webapi_connected: bool,
    /// Waiting for the answer to Connect / Disconnect.
    webapi_busy: bool,
    /// The pending request is a Disconnect (its answer is not an error).
    webapi_disconnecting: bool,
    webapi_message: Option<String>,
    client_id_input: String,

    /// The latest Last.fm state from the controller (Connect / Cancel / Disconnect / the
    /// scrobbling switch also update it at once; the controller's next status confirms).
    lastfm: LastfmStatus,
    /// The Last.fm API key and shared secret being typed, pre-filled from the saved ones. Only
    /// sent with `Command::ConnectLastfm`; the controller saves them.
    lastfm_key_input: String,
    lastfm_secret_input: String,
    /// Show the shared secret instead of dots. Reset whenever Settings opens or closes.
    lastfm_show_secret: bool,
    /// "Discard…" was clicked: ask before deleting unsent plays.
    lastfm_confirm_discard: bool,

    seek_preview: Option<f32>,
    /// UI scale while its slider is being dragged (applied on release).
    scale_preview: Option<f32>,
    volume_ui: f32,
    volume_dirty_at: Option<Instant>,
    volume_dragging: bool,
    volume_before_mute: Option<u8>,
    /// The volume being typed in the box over the volume line (right-click it or the mute button).
    volume_box: Option<String>,
    /// Equalizer changes not sent yet (sent at most every `EQ_SEND_INTERVAL` while dragging).
    eq_dirty: bool,
    eq_last_sent: Option<Instant>,
    /// The Discord Application ID being typed, and when it was last edited (applied once it is
    /// valid and left alone for a moment).
    discord_id_input: String,
    discord_id_edited: Option<Instant>,

    /// Settings is open: in its own window (`settings_win`) or inside the main window, by
    /// "Settings opens".
    settings_open: bool,
    settings_win: settings::SettingsWindow,
    /// Settings → About: what it asked the controller for, and its record.
    about: settings::AboutState,
    edit_details: Option<EditDetails>,
    /// "Wrong song?": the YouTube videos that may be a song.
    yt_pick: Option<ytpick::YtPick>,
    cover_picker_open: bool,
    cover_tx: Sender<(String, Option<PathBuf>)>,
    cover_rx: Receiver<(String, Option<PathBuf>)>,
    /// The wallpaper's picture (the choices are in `prefs.wallpaper`).
    wallpaper: WallpaperState,

    toasts: Vec<Toast>,
    /// The Final design's frame: the sidebar opened over the page, the title bar's wordmark.
    frame: frame::FrameState,
    /// Lyrics fetched this run, by song (the side panel and the Playing page).
    lyrics: panel::LyricsCache,
    /// The Albums and Artists pages' contents, once asked for.
    collection: Option<Arc<crate::model::Collection>>,
    collection_asked: bool,
    /// How many of Up next's songs the user queued (they come first).
    queued: usize,
    /// Home's New releases, once looked up.
    new_releases: Option<Arc<Vec<crate::model::Release>>>,
    /// The profile shown on the Profile page: someone else's (their username), or `None` for one's own.
    user_view: Option<String>,
    /// Profile pages loaded so far (one's own too: its follower counts), by username.
    user_pages: std::collections::HashMap<String, Arc<crate::model::UserPage>>,
    /// Profiles asked for (so each is asked once), and the ones that couldn't be loaded.
    profiles_asked: HashSet<String>,
    profiles_failed: HashSet<String>,
    /// Each artist's songs in Liked Songs (for their page): asked again after a like, shown as
    /// they were until the answer.
    artist_likes: tracks::LikesByArtist,
    /// A text box had the keys at the start of this pass (a box gives them up on Enter or Esc
    /// before the list sees the key).
    typing_at_start: bool,
    /// The people list shown: whose, and followers (true) or following.
    people_view: Option<(String, bool)>,
    people: std::collections::HashMap<(String, bool), Arc<crate::model::People>>,
    /// The artists the user follows, followed last first (`None` until known).
    following: Option<Arc<Vec<crate::model::CollectionItem>>>,
    /// Their uris, to tell quickly whether an artist is followed.
    followed: HashSet<String>,
    /// The names and pictures have come (an artist still unnamed then is one Spotify wouldn't
    /// describe).
    following_named: bool,
    /// Follows and unfollows made here (latest per artist): laid over every list from Spotify,
    /// which may have been read before them.
    follow_changes: std::collections::HashMap<String, FollowChange>,
    follow_generation: u64,
    /// The account the collection, mixes, New releases and follows were worked out for.
    collection_account: Option<String>,
    /// Truffle's own mixes (Home's For you).
    mixes: Arc<Vec<crate::model::MixInfo>>,
    /// Listen along: whom the user listens along with, the Friends list (and when it was asked
    /// for), sharing, the friends' links (the Friends tab) with the dialog open over them and the
    /// one copied last (and when), the link being typed in Join with a link, and whether the
    /// Switch layout shows Your listeners.
    listening: Option<crate::model::ListenStatus>,
    friends: Option<Arc<Vec<crate::model::Friend>>>,
    friends_asked: Option<Instant>,
    listen_share: bool,
    listen_links: Arc<Vec<crate::model::LinkView>>,
    link_dialog: Option<links::LinkDialog>,
    link_copied: Option<(u32, Instant)>,
    join_input: String,
    listeners_shown: bool,
    /// What other users play now (their profile's Listen along), and when it was asked for.
    friend_peek: std::collections::HashMap<String, (Instant, Option<crate::model::Friend>)>,
    /// The open right-click menu of a song.
    song_menu: Option<songmenu::SongMenu>,
    /// The Now playing view (Ctrl+I), over the whole window while it is open.
    stage: stage::Stage,
    /// The shortcuts window (Ctrl+H) is open.
    help_open: bool,
    coll: collection::CollState,
    nav: nav::Nav,
    images: ImageCache,
    fonts: ScriptFonts,
}

impl TruffleApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        commands: UnboundedSender<Command>,
        events: Receiver<Event>,
        settings: SharedSettings,
        paths: Paths,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::install_fonts(&ctx);
        let mut prefs = settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
        eq::sanitize(&mut prefs.equalizer);
        let accent = rgb(prefs.accent);
        let palette = Palette::build(prefs.theme, accent, prefs.custom_colors.as_ref());
        theme::apply_style(&ctx, &palette, prefs.compact);
        motion::set_speed(&ctx, prefs.motion_speed);
        motion::read_windows();
        // Ctrl + / Ctrl − / Ctrl 0 change the size on the window's screen (`screens.rs`), not egui's.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let screen_zoom = screens::ScreenZoom::opening(&ctx, &mut prefs);

        let images = ImageCache::new(&ctx, runtime.clone(), paths.image_cache_dir());
        let (cover_tx, cover_rx) = std::sync::mpsc::channel();
        let mut shell = Shell::new(Self::create_platform(&ctx, &prefs, runtime.clone()));
        shell.maximize_when_shown = prefs.main_window_maximized;

        // The controller tries the cached login on its own at startup and answers with
        // NeedLogin / LoggingIn / LoggedIn; until then the login screen shows "Starting…".
        Self {
            ctx: ctx.clone(),
            commands,
            events,
            settings,
            paths,
            runtime,
            shell,
            settings_dirty_at: None,
            palette,
            accent_fade: AccentFade::new(accent),
            accent_from_art: false,
            screen_zoom,
            auth: AuthState::Starting,
            interactive_login: false,
            login_started: None,
            brand: None,
            profile: None,
            reopened_last_source: false,
            playlists: Vec::new(),
            library_loaded: false,
            library_error: None,
            playlist_search: String::new(),
            playlist_filtered: Vec::new(),
            pin_edit: sidebar::PinEdit::default(),
            view: None,
            pending_scroll_uri: None,
            playback: PlaybackState::default(),
            have_playback: false,
            rules: prefs.shuffle_rules.clone(),
            liked: Arc::default(),
            likes_waiting: LikesWaiting::default(),
            blocked: Arc::default(),
            up_next: None,
            sleep_timer: None,
            deck: None,
            round_progress: Arc::default(),
            middle_tab: MiddleTab::Songs,
            recent_sources: prefs.recent_lists.clone(),
            home: home::HomeState::default(),
            settings_page: settings::Page::Audio,
            settings_query: String::new(),
            search: search::SearchState::default(),
            source_info: std::collections::HashMap::new(),
            pending_play: None,
            new_playlist: None,
            confirm_remove: None,
            already_in: library::AlreadyInState::default(),
            dupes: None,
            webapi_connected: false,
            webapi_busy: false,
            webapi_disconnecting: false,
            webapi_message: None,
            client_id_input: prefs.webapi_client_id.clone().unwrap_or_default(),
            lastfm: LastfmStatus { enabled: prefs.lastfm_enabled, ..LastfmStatus::default() },
            lastfm_key_input: prefs.lastfm_api_key.clone().unwrap_or_default(),
            lastfm_secret_input: prefs.lastfm_api_secret.clone().unwrap_or_default(),
            lastfm_show_secret: false,
            lastfm_confirm_discard: false,
            seek_preview: None,
            scale_preview: None,
            volume_ui: prefs.volume as f32,
            volume_dirty_at: None,
            volume_dragging: false,
            volume_before_mute: None,
            volume_box: None,
            eq_dirty: false,
            eq_last_sent: None,
            discord_id_input: prefs.discord_app_id.clone().unwrap_or_default(),
            discord_id_edited: None,
            settings_open: false,
            settings_win: settings::SettingsWindow::default(),
            about: settings::AboutState::default(),
            edit_details: None,
            yt_pick: None,
            cover_picker_open: false,
            cover_tx,
            cover_rx,
            wallpaper: WallpaperState::default(),
            toasts: Vec::new(),
            frame: frame::FrameState::default(),
            lyrics: panel::LyricsCache::new(),
            collection: None,
            collection_asked: false,
            queued: 0,
            new_releases: None,
            user_view: None,
            user_pages: std::collections::HashMap::new(),
            profiles_asked: HashSet::new(),
            profiles_failed: HashSet::new(),
            people_view: None,
            people: std::collections::HashMap::new(),
            artist_likes: tracks::LikesByArtist::default(),
            typing_at_start: false,
            following: None,
            followed: HashSet::new(),
            following_named: false,
            follow_changes: std::collections::HashMap::new(),
            follow_generation: 0,
            collection_account: None,
            mixes: Arc::new(Vec::new()),
            listening: None,
            friends: None,
            friends_asked: None,
            listen_share: prefs.listen_share,
            listen_links: Arc::default(),
            link_dialog: None,
            link_copied: None,
            join_input: String::new(),
            listeners_shown: false,
            friend_peek: std::collections::HashMap::new(),
            song_menu: None,
            stage: stage::Stage::default(),
            help_open: false,
            coll: collection::CollState::default(),
            nav: nav::Nav::default(),
            images,
            fonts: ScriptFonts::new(),
            prefs,
        }
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::error!("controller stopped; command dropped");
        }
    }

    /// Follows (or unfollows) the artist at once, and asks Spotify to. `name` and `image` may be
    /// empty while its page loads: the list from Spotify (or Liked Songs) fills them in.
    pub(super) fn set_follow(&mut self, uri: &str, name: &str, image: Option<String>, follow: bool) {
        // The artist as listed, if it is; else as this page shows it.
        let known = self.following.as_ref().and_then(|l| l.iter().find(|a| a.uri == uri).cloned());
        let item = known.filter(|a| !a.name.is_empty()).unwrap_or_else(|| crate::model::CollectionItem {
            uri: uri.to_owned(),
            name: name.to_owned(),
            image,
            ..Default::default()
        });
        self.follow_generation += 1;
        let generation = self.follow_generation;
        let was = self.followed.contains(uri);
        let change = self.follow_changes.entry(uri.to_owned()).or_insert_with(|| FollowChange {
            follow,
            generation,
            before: was,
            confirmed: None,
            item: item.clone(),
        });
        change.follow = follow;
        change.generation = generation;
        if !item.name.is_empty() {
            change.item = item.clone();
        }
        self.apply_follow(&item, follow);
        let text = match (follow, item.name.is_empty()) {
            (true, false) => format!("Following {}", item.name),
            (true, true) => "Following this artist".to_owned(),
            (false, false) => format!("Unfollowed {}", item.name),
            (false, true) => "Unfollowed this artist".to_owned(),
        };
        self.toast(text, false);
        self.send(Command::Follow { uri: uri.to_owned(), follow, generation });
        // One's own following count and list are read again when next shown.
        if let Some(me) = self.profile.as_ref().map(|p| p.username.clone()) {
            self.people.remove(&(me.clone(), false));
            self.profiles_asked.remove(&me);
        }
    }

    /// Puts `item` in the artists followed (first, as followed last) or takes it out.
    fn apply_follow(&mut self, item: &crate::model::CollectionItem, follow: bool) {
        let list = Arc::make_mut(self.following.get_or_insert_with(Default::default));
        let had = list.iter().position(|a| a.uri == item.uri);
        if follow {
            self.followed.insert(item.uri.clone());
            if had.is_none() {
                for a in list.iter_mut() {
                    a.newest += 1;
                }
                list.insert(0, crate::model::CollectionItem { newest: 0, ..item.clone() });
            }
        } else {
            self.followed.remove(&item.uri);
            if let Some(k) = had {
                list.remove(k);
            }
        }
    }

    /// A follow (or unfollow) is through (they come back in the order sent). Kept: that is what
    /// Spotify has now. Refused, and with no newer change since: the artist goes back to what
    /// Spotify last confirmed (or had before).
    fn follow_done(&mut self, uri: &str, follow: bool, generation: u64, ok: bool) {
        let Some(change) = self.follow_changes.get_mut(uri) else { return };
        if ok {
            change.confirmed = Some(follow);
            return;
        }
        if change.generation != generation {
            return;
        }
        let back = change.confirmed.unwrap_or(change.before);
        change.follow = back;
        let item = change.item.clone();
        self.apply_follow(&item, back);
    }

    /// What belongs to the account logged in: worked out again for the next one.
    fn forget_account_data(&mut self) {
        self.collection_asked = false;
        self.collection = None;
        self.new_releases = None;
        self.mixes = Arc::default();
        self.following = None;
        self.followed.clear();
        self.following_named = false;
        self.follow_changes.clear();
        self.already_in.clear();
        // A playlist of the account's own, being looked through.
        self.dupes = None;
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        // Collapse repeats of the same message.
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(Toast { text, error, created: Instant::now(), undo: None });
        if self.toasts.len() > MAX_TOASTS {
            let idx = toast_to_drop(&self.toasts);
            self.toasts.remove(idx);
        }
    }

    /// A status-line note, optionally with UNDO.
    fn note(&mut self, text: impl Into<String>, undo: Option<Undo>) {
        self.toast(text, false);
        if let Some(t) = self.toasts.last_mut() {
            t.undo = undo;
        }
    }

    fn apply_undo(&mut self, undo: Undo) {
        match undo {
            Undo::Rules(rules) => {
                self.set_rules(rules);
                self.toast("Rules put back", false);
            }
            Undo::Block(uris, block) => {
                let set = Arc::make_mut(&mut self.blocked);
                for uri in &uris {
                    if block {
                        set.insert(uri.clone());
                    } else {
                        set.remove(uri);
                    }
                }
                for uri in uris {
                    self.send(Command::SetBlocked { uri, blocked: block });
                }
                self.toast(if block { "Blocked again" } else { "Allowed again" }, false);
            }
            Undo::Banners(banners) => self.put_banners_back(banners),
        }
    }

    fn shared_settings(&self) -> MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies the UI-owned settings into the shared settings and schedules a save.
    fn prefs_changed(&mut self) {
        {
            let p = &self.prefs;
            let mut s = self.shared_settings();
            s.theme = p.theme;
            s.accent = p.accent;
            s.ui_scale = p.ui_scale;
            s.show_queue_panel = p.show_queue_panel;
            s.show_rules_panel = p.show_rules_panel;
            s.last_source = p.last_source.clone();
            s.bitrate = p.bitrate.clone();
            s.normalisation = p.normalisation;
            s.audio_cache_mb = p.audio_cache_mb;
            s.playlist_filter = p.playlist_filter;
            s.pinned_playlists = p.pinned_playlists.clone();
            s.media_controls = p.media_controls;
            s.tray_icon = p.tray_icon;
            s.keep_running = p.keep_running;
            s.start_with_windows = p.start_with_windows;
            s.discord_status = p.discord_status;
            s.discord_app_id = p.discord_app_id.clone();
            s.dynamic_accent = p.dynamic_accent;
            s.compact = p.compact;
            s.screen_sizes = p.screen_sizes.clone();
            s.grow_with_window = p.grow_with_window;
            s.big_screen_tips = p.big_screen_tips.clone();
            s.custom_colors = p.custom_colors.clone();
            s.wallpaper = p.wallpaper.clone();
            s.library_view = p.library_view;
            s.hero_size = p.hero_size;
            s.spin_record = p.spin_record;
            s.library_width = p.library_width;
            s.deck_width = p.deck_width;
            s.banners = p.banners.clone();
            s.settings_size = p.settings_size;
            s.app_icon = p.app_icon;
            s.sidebar_folded = p.sidebar_folded;
            s.panel_medium = p.panel_medium;
            s.panel_record = p.panel_record;
            s.liked_pinned = p.liked_pinned;
            s.friends_layout = p.friends_layout;
            s.friends_links_open = p.friends_links_open;
            s.recent_lists = p.recent_lists.clone();
            s.recent_searches = p.recent_searches.clone();
            s.mini_player_on_top = p.mini_player_on_top;
            s.mini_player_pos = p.mini_player_pos;
            s.main_window_size = p.main_window_size;
            s.main_window_rect = p.main_window_rect;
            s.main_window_maximized = p.main_window_maximized;
            s.stage_window_size = p.stage_window_size;
            s.page_animation = p.page_animation;
            s.motion_speed = p.motion_speed;
            s.song_size = p.song_size;
            s.more_columns = p.more_columns;
            s.fill_cards = p.fill_cards;
            s.wide_screens = p.wide_screens;
        }
        self.settings_dirty_at = Some(Instant::now());
    }

    fn save_settings_now(&self) {
        if let Err(e) = Settings::save_shared(&self.settings, &self.paths) {
            log::warn!("could not save settings: {e:#}");
        }
    }

    fn flush_settings_if_due(&mut self, ctx: &egui::Context) {
        let Some(at) = self.settings_dirty_at else { return };
        let elapsed = at.elapsed();
        if elapsed < SETTINGS_SAVE_DELAY {
            ctx.request_repaint_after(SETTINGS_SAVE_DELAY - elapsed);
            return;
        }
        self.settings_dirty_at = None;
        let settings = self.settings.clone();
        let paths = self.paths.clone();
        self.runtime.spawn_blocking(move || {
            // Snapshots under the lock and writes outside it, so the UI never waits for the disk;
            // a concurrent save can still never leave an older snapshot on disk.
            if let Err(e) = Settings::save_shared(&settings, &paths) {
                log::warn!("could not save settings: {e:#}");
            }
        });
    }

    /// Rebuilds the palette (theme, shown accent, custom colours, density) and applies it.
    fn restyle(&mut self, ctx: &egui::Context) {
        let accent = self.accent_fade.value(Instant::now());
        self.palette = Palette::build(self.prefs.theme, accent, self.prefs.custom_colors.as_ref());
        theme::apply_style(ctx, &self.palette, self.prefs.compact);
    }

    /// Moves the shown accent towards the album art's colour (dynamic accent) or the user's.
    fn update_accent(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let user = rgb(self.prefs.accent);
        let (target, from_art) = if self.prefs.dynamic_accent {
            let cover = self.playback.track.as_ref().and_then(|t| t.cover_large.as_deref().or(t.cover_small.as_deref()));
            match cover.map(|url| self.images.swatch(url)) {
                // Readable everywhere the accent marks something: the lists, the side panels
                // (the playing playlist) and the player bar (shuffle/repeat on, liked, timer).
                Some(Swatch::Color(c)) => {
                    let p = &self.palette;
                    (accent::adapt_accent(c, &[p.main_bg, p.sidebar_bg, p.bar_bg]), true)
                }
                // Keep the current colour until the new cover is decoded, instead of flashing.
                Some(Swatch::Pending) => (self.accent_fade.target(), self.accent_from_art),
                Some(Swatch::None) | None => (user, false),
            }
        } else {
            (user, false)
        };
        if from_art || self.accent_from_art {
            self.accent_fade.retarget(target, now);
        } else if target != self.accent_fade.target() {
            self.accent_fade.jump(target);
        }
        self.accent_from_art = from_art;
        if self.accent_fade.value(now) != self.palette.accent {
            self.restyle(ctx);
        }
        if self.accent_fade.animating(now) {
            ctx.request_repaint();
        }
    }

    /// Start the playlist named on the command line (`--play`), once.
    fn autoplay(&mut self) {
        let Some(name) = take_autoplay() else { return };
        match find_playlist_by_name(&self.playlists, &name).map(|p| p.uri.clone()) {
            Some(uri) => {
                log::info!("playing playlist {name:?} from the command line");
                let source = Source::Playlist(uri);
                self.open_source(source.clone(), false);
                let shuffle = self.current_shuffle();
                self.send(Command::PlaySource { source, start_index: None, shuffle });
            }
            None => {
                log::warn!("--play: no playlist called {name:?}");
                self.toast(format!("Couldn't find a playlist called \"{name}\"."), true);
            }
        }
    }

    fn current_shuffle(&self) -> bool {
        if self.have_playback { self.playback.shuffle } else { self.prefs.shuffle }
    }

    fn is_playing(&self) -> bool {
        matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading)
    }

    fn playlist(&self, uri: &str) -> Option<&PlaylistSummary> {
        self.playlists.iter().find(|p| p.uri == uri)
    }

    /// A dialog box is open over the window (its keys are its own).
    fn dialog_open(&self) -> bool {
        self.new_playlist.is_some()
            || self.confirm_remove.is_some()
            || self.edit_details.is_some()
            || self.yt_pick.is_some()
            || self.wallpaper.banners.adjusting.is_some()
            || self.dupes.is_some()
            || self.link_dialog.is_some()
    }

    /// Shows a source in the centre view and asks the controller for its tracks.
    fn open_source(&mut self, source: Source, force_refresh: bool) {
        // Opening a list always shows it, whichever tab was open.
        self.middle_tab = MiddleTab::Songs;
        if self.recent_sources.first() != Some(&source) {
            self.recent_sources.retain(|s| *s != source);
            self.recent_sources.insert(0, source.clone());
            self.recent_sources.truncate(home::RECENT_MAX);
            self.prefs.recent_lists = self.recent_sources.clone();
            self.prefs_changed();
        }
        let same = self.view.as_ref().is_some_and(|v| v.source == source);
        if same && !force_refresh {
            return;
        }
        if same {
            if let Some(view) = self.view.as_mut() {
                view.loading = Some((0, None));
                view.error = None;
            }
        } else {
            self.view = Some(TrackView::new(source.clone()));
        }
        self.send(Command::OpenSource { source: source.clone(), force_refresh });
        if self.prefs.last_source.as_ref() != Some(&source) {
            self.prefs.last_source = Some(source);
            self.prefs_changed();
        }
    }

    /// Whether the logged-in user created this playlist. Falls back to comparing the owner name
    /// for playlist lists cached before `owned` existed.
    fn is_own_playlist(&self, p: &PlaylistSummary) -> bool {
        if p.owned {
            return true;
        }
        let owner = p.owner_name.trim();
        !owner.is_empty()
            && self.profile.as_ref().is_some_and(|me| {
                owner.eq_ignore_ascii_case(me.username.trim()) || owner.eq_ignore_ascii_case(me.display_name.trim())
            })
    }

    fn refilter_playlists(&mut self) {
        let needle = fold(self.playlist_search.trim());
        let filter = self.prefs.playlist_filter;
        self.playlist_filtered = self
            .playlists
            .iter()
            .enumerate()
            .filter(|(_, p)| match filter {
                PlaylistFilter::All => true,
                PlaylistFilter::Mine => self.is_own_playlist(p),
                PlaylistFilter::Others => !self.is_own_playlist(p),
            })
            .filter(|(_, p)| {
                contains_folded(&p.name, &needle) || contains_folded(&p.owner_name, &needle)
            })
            .map(|(i, _)| i)
            .collect();
        // Pinned playlists first, in the order they were pinned.
        let pinned = &self.prefs.pinned_playlists;
        let rank = |i: &usize| pinned.iter().position(|u| *u == self.playlists[*i].uri).unwrap_or(usize::MAX);
        self.playlist_filtered.sort_by_key(rank);
    }

    /// Every name the controller sends (releases, cards, lyrics, friends, ...) can need a font for
    /// its script (Japanese, Korean, ...): looked through once each, so none shows as squares.
    /// Song lists and the sets of liked or blocked songs are looked through where they arrive
    /// (they are big, and only their names and artists matter).
    fn scan_event_text(&mut self, ctx: &egui::Context, event: &Event) {
        if self.fonts.all_requested() {
            return;
        }
        match event {
            Event::SourceLoaded { .. } | Event::LikedSet(_) | Event::LikesWaiting(_) | Event::BlockedSet(_) | Event::RoundProgress(_) | Event::Duplicates { .. } | Event::UpNext(_) => {}
            Event::ForAccount { event, .. } => self.scan_event_text(ctx, event),
            other => self.fonts.scan(ctx, &format!("{other:?}")),
        }
    }

    fn handle_event(&mut self, ctx: &egui::Context, event: Event) {
        self.scan_event_text(ctx, &event);
        match event {
            Event::PlayStats(stats) => self.on_play_stats(ctx, stats),
            Event::Helpers(helpers) => self.on_helpers(ctx, helpers),
            Event::Collection(c) => self.collection = Some(c),
            Event::Queued(n) => self.queued = n,
            Event::NewReleases(r) => self.new_releases = Some(r),
            Event::ForAccount { account, event } => {
                if self.collection_account.as_deref() == Some(account.as_str()) {
                    self.handle_event(ctx, *event);
                }
            }
            Event::Following { list, named } => {
                self.followed = list.iter().map(|a| a.uri.clone()).collect();
                self.following = Some(list);
                self.following_named = named;
                // Oldest first, so the newest follow ends up first.
                let mut changes: Vec<FollowChange> = self.follow_changes.values().cloned().collect();
                changes.sort_by_key(|c| c.generation);
                for c in changes {
                    self.apply_follow(&c.item, c.follow);
                }
            }
            Event::FollowDone { uri, follow, generation, ok } => self.follow_done(&uri, follow, generation, ok),
            Event::UserPage(page) => {
                self.profiles_failed.remove(&page.username);
                self.user_pages.insert(page.username.clone(), page);
            }
            Event::People(people) => {
                self.profiles_failed.remove(&people.username);
                self.people.insert((people.username.clone(), people.followers), people);
            }
            Event::ProfileFailed { username, message } => {
                self.profiles_failed.insert(username);
                self.toast(message, true);
            }
            Event::Mixes(m) => self.mixes = m,
            Event::Lyrics { uri, lines, synced } => {
                let l = match lines {
                    Some(lines) => panel::Lyrics::Lines { lines, synced },
                    None => panel::Lyrics::None,
                };
                self.lyrics.insert(uri, l);
            }
            Event::Canvas { uri, video, picture } => self.stage.canvas_came(uri, video, picture),
            Event::HeldBy { uris, playlists } => self.held_came(uris, playlists),
            Event::Status(text) => self.toast(text, false),
            Event::Error(text) => self.toast(text, true),
            Event::NeedLogin => {
                self.auth = AuthState::NeedLogin;
                self.interactive_login = false;
                self.dupes = None;
            }
            Event::LoggingIn => {
                if self.auth != AuthState::LoggingIn {
                    self.login_started = Some(Instant::now());
                }
                self.auth = AuthState::LoggingIn;
            }
            Event::LoggedIn(profile) => {
                self.fonts.scan(ctx, &profile.display_name);
                self.profile = Some(profile);
                self.refilter_playlists();
                self.auth = AuthState::LoggedIn;
                self.interactive_login = false;
                // Another account than the one they were worked out for (after the login expired):
                // they are worked out again.
                let account = self.profile.as_ref().map(|p| p.username.clone());
                if self.collection_account != account {
                    self.forget_account_data();
                    self.collection_account = account;
                }
                // The Albums and Artists pages, Truffle's mixes and New releases are worked out
                // once, in the background, right after logging in.
                if !self.collection_asked {
                    self.collection_asked = true;
                    self.send(Command::Collection);
                }
                if !self.reopened_last_source {
                    self.reopened_last_source = true;
                    if let Some(source) = self.prefs.last_source.clone() {
                        self.open_source(source, false);
                    }
                }
            }
            Event::LoggedOut => {
                if self.shell.mini {
                    self.toggle_mini(ctx);
                }
                self.sleep_timer = None;
                self.auth = AuthState::NeedLogin;
                self.profile = None;
                self.reopened_last_source = false;
                self.playlists.clear();
                self.playlist_filtered.clear();
                self.recent_sources.clear();
                self.library_loaded = false;
                self.library_error = None;
                self.view = None;
                self.playback = PlaybackState::default();
                self.have_playback = false;
                self.liked = Arc::default();
                self.likes_waiting = LikesWaiting::default();
                self.artist_likes = tracks::LikesByArtist::default();
                self.blocked = Arc::default();
                self.up_next = None;
                self.deck = None;
                self.round_progress = Arc::default();
                self.settings_open = false;
                self.edit_details = None;
                self.forget_account_data();
                self.collection_account = None;
                self.listening = None;
                self.friends = None;
                self.friends_asked = None;
                self.friend_peek.clear();
                self.listen_links = Arc::default();
                self.link_dialog = None;
            }
            Event::Library(list) => {
                self.fonts.scan_all(ctx, list.iter().flat_map(|p| [p.name.as_str(), p.owner_name.as_str()]));
                // A question about a playlist that went goes too (one being removed that is still
                // there wasn't removed: it is asked after all).
                self.already_in.listed(|uri| list.iter().any(|p| p.uri == uri));
                self.playlists = list;
                self.library_loaded = true;
                self.library_error = None;
                self.refilter_playlists();
                self.autoplay();
            }
            Event::LibraryFailed { message } => self.library_error = Some(message),
            Event::PlaylistUpdated(p) => {
                self.fonts.scan(ctx, &p.name);
                match self.playlists.iter_mut().find(|x| x.uri == p.uri) {
                    Some(slot) => *slot = p,
                    None => self.playlists.push(p),
                }
                self.refilter_playlists();
            }
            Event::SourceLoading { source, loaded, total } => {
                if let Some(view) = self.view.as_mut().filter(|v| v.source == source) {
                    view.loading = Some((loaded, total));
                    view.error = None;
                }
            }
            Event::SourceLoaded { source, tracks } => {
                if self.view.as_ref().is_some_and(|v| v.source == source) {
                    self.fonts.scan_all(
                        ctx,
                        tracks.iter().flat_map(|t| {
                            [t.name.as_str(), t.album.as_str()].into_iter().chain(t.artists.iter().map(|a| a.name.as_str()))
                        }),
                    );
                    if let Some(view) = self.view.as_mut() {
                        view.set_tracks(tracks);
                        if let Some(uri) = self.pending_scroll_uri.take() {
                            view.scroll_to_uri(&uri);
                        }
                    }
                    self.start_pending_play();
                }
            }
            Event::SourceFailed { source, message } => {
                if let Some(view) = self.view.as_mut().filter(|v| v.source == source) {
                    view.loading = None;
                    view.error = Some(message);
                }
            }
            Event::Playback(state) => {
                if let Some(t) = &state.track {
                    self.fonts.scan(ctx, &t.name);
                    self.fonts.scan(ctx, &t.artist_line());
                }
                if self.volume_dirty_at.is_none() && !self.volume_dragging {
                    self.volume_ui = state.volume as f32;
                }
                self.playback = state;
                self.have_playback = true;
            }
            Event::BlockedSet(set) => self.blocked = set,
            Event::LikedSet(set) => {
                if set != self.liked {
                    self.artist_likes.stale();
                }
                self.liked = set;
            }
            Event::LikesWaiting(waiting) => self.likes_waiting = waiting,
            Event::YouTubeMatches { uri, current, result } => self.on_yt_matches(&uri, current, result),
            Event::SoundSource(source) => self.prefs.sound_source = source,
            Event::FollowUserDone { username, follow, ok } => {
                if !ok {
                    // Put back as it was (the count too).
                    self.set_user_followed(&username, !follow);
                }
            }
            Event::ArtistLikes(likes) => self.artist_likes.answer(likes),
            Event::WebApi { connected, message } => {
                let failed = !connected && !self.webapi_disconnecting;
                self.webapi_connected = connected;
                self.webapi_busy = false;
                self.webapi_disconnecting = false;
                if let Some(m) = &message {
                    self.toast(m.clone(), failed);
                }
                self.webapi_message = message;
            }
            Event::SleepTimer(state) => self.sleep_timer = state,
            // Shown in Settings → Last.fm (anything urgent also comes as Status / Error).
            Event::Lastfm(status) => self.lastfm = status,
            Event::Deck(deck) => self.deck = deck,
            Event::RoundProgress(progress) => self.round_progress = progress,
            Event::SearchResults(results) => {
                self.fonts.scan_all(ctx, results.tracks.iter().map(|t| t.name.as_str()));
                self.search.receive(results);
                if self.search.play_top && self.search.results.is_some() {
                    self.play_top();
                }
            }
            Event::SearchFailed { query, message } => {
                if query == self.search.sent {
                    self.search.loading = false;
                    self.search.error = Some(message);
                }
            }
            Event::SearchUsers(results) => self.search.receive_users(&results),
            Event::SearchUsersFailed { query, message } => {
                if query == self.search.users.query {
                    self.search.users.loading = false;
                    self.search.users.error = Some(message);
                }
            }
            Event::SourceInfo(info) => {
                self.fonts.scan(ctx, &info.title);
                // An artist followed before its page had loaded gets its name and picture now.
                if let Source::Artist(uri) = &info.source {
                    if let Some(c) = self.follow_changes.get_mut(uri).filter(|c| c.item.name.is_empty()) {
                        c.item.name = info.title.clone();
                        c.item.image = info.image.clone();
                    }
                    if let Some(a) = self.following.as_mut().and_then(|l| Arc::make_mut(l).iter_mut().find(|a| a.uri == *uri && a.name.is_empty())) {
                        a.name = info.title.clone();
                        a.image = info.image.clone();
                    }
                }
                self.source_info.insert(info.source.clone(), info);
            }
            Event::Listening(status) => self.listening = status,
            // (Its link is the one older Truffles write to: the Friends tab shows every link instead.)
            Event::ListenShare { on, .. } => self.listen_share = on,
            Event::ListenLinks(list) => {
                self.fonts.scan_all(ctx, list.iter().map(|l| l.name.as_str()));
                // A dialog about a link that went (removed, or another account's now) goes too.
                if self.link_dialog.as_ref().and_then(|d| d.link()).is_some_and(|id| !list.iter().any(|l| l.id == id)) {
                    self.link_dialog = None;
                }
                self.listen_links = list;
            }
            Event::Friends(list) => {
                self.fonts.scan_all(ctx, list.iter().flat_map(|f| [f.name.as_str(), f.song.as_str(), f.artist.as_str()]));
                self.friends = Some(list);
            }
            Event::FriendPeek { username, friend } => {
                let asked = self.friend_peek.get(&username).map_or_else(Instant::now, |(at, _)| *at);
                self.friend_peek.insert(username, (asked, friend));
            }
            Event::OpenPage { page, report } => {
                let page = page.split_whitespace().collect::<Vec<_>>().join(" ");
                let said = match nav::place_of(&page) {
                    Some(place) => {
                        self.show_place(&place);
                        format!("opened {page}")
                    }
                    None => format!("couldn't open \"{page}\": home, profile, liked, likes <artist URI> or a playlist's, album's or artist's URI"),
                };
                log::info!("self-test: {said}");
                if let Err(e) = std::fs::write(&report, format!("{said}\n")) {
                    log::warn!("self-test: couldn't write {} ({e})", report.display());
                }
            }
            Event::Duplicates { uri, tracks } => self.on_duplicates(ctx, &uri, tracks),
            Event::PlaylistRemoved { uri } => {
                self.playlists.retain(|p| p.uri != uri);
                self.already_in.drop_about(|u| u == uri);
                if self.prefs.pinned_playlists.contains(&uri) {
                    self.prefs.pinned_playlists.retain(|u| *u != uri);
                    self.prefs_changed();
                }
                if self.view.as_ref().is_some_and(|v| v.source == Source::Playlist(uri.clone())) {
                    self.view = None;
                }
                self.refilter_playlists();
            }
            // Asked one at a time, in the order they come.
            Event::AlreadyInPlaylist(already) => self.already_in.found(already),
            Event::UpNext(list) => {
                // The upcoming tracks may be in scripts (CJK, Thai, ...) whose fonts nothing else
                // has loaded yet, e.g. a restored queue of a list that isn't on screen.
                self.fonts.scan_all(
                    ctx,
                    list.iter().flat_map(|t| {
                        [t.name.as_str(), t.album.as_str()].into_iter().chain(t.artists.iter().map(|a| a.name.as_str()))
                    }),
                );
                self.up_next = Some(list);
            }
        }
    }

    /// Global keyboard shortcuts (the list: `keys::GROUPS`). Runs before any widget so keys are
    /// consumed exactly once.
    fn handle_shortcuts(&mut self, ui: &egui::Ui) {
        use egui::{Key, Modifiers};
        if self.auth != AuthState::LoggedIn {
            return;
        }
        // The shortcuts window owns the keys while it is open: Esc and Ctrl+H close it, and nothing
        // underneath sees them.
        if self.help_open {
            if ui.input_mut(|i| keys::take(i, Modifiers::NONE, Key::Escape) || keys::take(i, Modifiers::CTRL, Key::H)) {
                self.help_open = false;
            }
            return;
        }
        let mini = self.shell.mini;
        let (focus_filter, focus_search, toggle_mini, toggle_deck, show_playing, open_settings) = ui.input_mut(|i| {
            (
                keys::take(i, Modifiers::CTRL, Key::F),
                keys::take(i, Modifiers::CTRL, Key::L),
                keys::take(i, Modifiers::CTRL, Key::M),
                keys::take(i, Modifiers::CTRL, Key::D),
                keys::take(i, Modifiers::CTRL, Key::J),
                keys::take(i, Modifiers::CTRL, Key::Comma),
            )
        });
        if !mini && ui.input_mut(|i| keys::take(i, Modifiers::CTRL, Key::K)) {
            // Out of the Now playing view, at the width the window comes back to.
            let mut width = ui.ctx().content_rect().width();
            if self.stage.open {
                width = self.stage_return_width(ui.ctx()).unwrap_or(width);
                self.close_stage(ui.ctx());
            }
            // Narrow: the search box is on the Search tab.
            if width < frame::NARROW {
                self.middle_tab = MiddleTab::Search;
            }
            ui.memory_mut(|m| m.request_focus(Id::new(search::SEARCH_BOX_ID)));
        }
        if open_settings && !mini {
            // Settings' window opens or comes to the front (Ctrl+, in that window closes it).
            self.open_settings();
            return;
        }
        // Dialogs are not drawn in the mini player, so they don't own the keys there. Settings has
        // a window of its own with its own keys: this window's still work.
        if !mini && (self.edit_details.is_some() || self.link_dialog.is_some() || self.already_in_up(ui.ctx())) {
            // Keys belong to the open dialog. "Already in" pops up by itself, maybe as the user
            // types in a text box, which it takes the keys from: the rest isn't Space, R or Home.
            return;
        }
        if toggle_mini {
            let ctx = ui.ctx().clone();
            self.toggle_mini(&ctx);
            return;
        }
        if mini {
            // The mini player has no text fields: only the playback keys apply.
            self.playback_shortcuts(ui);
            return;
        }
        // The Now playing view: Ctrl+I opens and closes it. While it is open F11 makes it fullscreen,
        // and Esc closes it once no menu is open.
        let ctx = ui.ctx().clone();
        let (toggle_stage, fullscreen) = ui.input_mut(|i| (keys::take(i, Modifiers::CTRL, Key::I), self.stage.open && keys::take(i, Modifiers::NONE, Key::F11)));
        if toggle_stage {
            self.toggle_stage(&ctx);
            return;
        }
        if fullscreen {
            self.stage_fullscreen(&ctx);
        }
        let menus = self.song_menu.is_some() || egui::Popup::is_any_open(&ctx) || ctx.text_edit_focused();
        if self.stage.open && !menus && ui.input_mut(|i| keys::take(i, Modifiers::NONE, Key::Escape)) {
            self.close_stage(&ctx);
            return;
        }
        // The Final design's keys: Ctrl+B folds or opens the sidebar, Ctrl+P (and Ctrl+D, until the record
        // gets its own switch) opens or closes the side panel, Ctrl+Z undoes the newest note's change.
        let width = ui.ctx().content_rect().width();
        let typing = ui.ctx().text_edit_focused();
        let (toggle_side, toggle_panel, undo, help) = ui.input_mut(|i| {
            (
                keys::take(i, Modifiers::CTRL, Key::B),
                keys::take(i, Modifiers::CTRL, Key::P),
                // While typing, Ctrl+Z (and Ctrl+Shift+Z) belong to the text box.
                !typing && keys::take(i, Modifiers::CTRL, Key::Z),
                keys::take(i, Modifiers::CTRL, Key::H),
            )
        });
        if help {
            self.toggle_help(ui.ctx());
            return;
        }
        // Ctrl+1, 2, 3: Home, Albums, Artists.
        let page = ui.input_mut(|i| {
            if keys::take(i, Modifiers::CTRL, Key::Num1) {
                Some(MiddleTab::Home)
            } else if keys::take(i, Modifiers::CTRL, Key::Num2) {
                Some(MiddleTab::Albums)
            } else if keys::take(i, Modifiers::CTRL, Key::Num3) {
                Some(MiddleTab::Artists)
            } else {
                None
            }
        });
        match page {
            Some(MiddleTab::Home) => self.show_home(),
            Some(tab) => self.show_collection(tab),
            None => {}
        }
        self.song_shortcuts(ui);
        // Back and forward: Alt+Left / Alt+Right and the mouse's side buttons.
        let (back, forward) = ui.input_mut(|i| {
            let back = keys::take(i, Modifiers::ALT, Key::ArrowLeft) || i.pointer.button_pressed(egui::PointerButton::Extra1);
            let forward = keys::take(i, Modifiers::ALT, Key::ArrowRight) || i.pointer.button_pressed(egui::PointerButton::Extra2);
            (back, forward)
        });
        if back {
            self.go_back();
        } else if forward {
            self.go_forward();
        }
        // Keys that go to a page or a box close the Now playing view, which would hide it. They act on
        // the window as it comes back, not the view's.
        let mut width = width;
        if self.stage.open && (page.is_some() || back || forward || toggle_side || toggle_panel || show_playing || focus_filter || focus_search) {
            width = self.stage_return_width(&ctx).unwrap_or(width);
            self.close_stage(&ctx);
        }
        let narrow = width < frame::NARROW;
        if toggle_side {
            if narrow {
                self.middle_tab = MiddleTab::Library;
            } else {
                self.cycle_sidebar(width);
            }
        }
        if toggle_panel {
            if narrow {
                self.middle_tab = MiddleTab::Playing;
            } else {
                self.toggle_side_panel(width);
            }
        }
        if toggle_deck {
            self.toggle_panel_record();
        }
        if undo {
            self.undo_newest();
        }
        if show_playing {
            self.locate_current_track();
        }
        if focus_filter && self.view.is_some() {
            self.middle_tab = MiddleTab::Songs;
            ui.memory_mut(|m| m.request_focus(Id::new(TRACK_FILTER_ID)));
        }
        if focus_search {
            // Narrow: the playlist filter is on the Library tab.
            if narrow {
                self.middle_tab = MiddleTab::Library;
            }
            ui.memory_mut(|m| m.request_focus(Id::new(PLAYLIST_SEARCH_ID)));
        }
        if ui.text_edit_focused() {
            return;
        }
        if ui.input_mut(|i| keys::take(i, Modifiers::NONE, Key::R)) && self.playback.track.is_some() && self.current_shuffle() {
            self.reshuffle();
        }
        if ui.input_mut(|i| keys::take(i, Modifiers::NONE, Key::Home)) {
            if self.stage.open {
                self.close_stage(ui.ctx());
            }
            self.show_home();
        }
        self.playback_shortcuts(ui);
    }

    /// Space, Ctrl+→ and Ctrl+←, Ctrl+↑ and Ctrl+↓ (volume).
    fn playback_shortcuts(&mut self, ui: &egui::Ui) {
        use egui::{Key, Modifiers};
        let (toggle, next, prev, louder, quieter) = ui.input_mut(|i| {
            (
                keys::take(i, Modifiers::NONE, Key::Space),
                keys::take(i, Modifiers::CTRL, Key::ArrowRight),
                keys::take(i, Modifiers::CTRL, Key::ArrowLeft),
                keys::take(i, Modifiers::CTRL, Key::ArrowUp),
                keys::take(i, Modifiers::CTRL, Key::ArrowDown),
            )
        });
        if louder {
            self.nudge_volume(5.0);
        } else if quieter {
            self.nudge_volume(-5.0);
        }
        if toggle {
            self.send(Command::PlayPause);
        }
        if next {
            self.send(Command::Next);
        }
        if prev {
            self.send(Command::Previous);
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.toasts.retain(|t| t.error || now.duration_since(t.created) < STATUS_TOAST_TIME);
        // The messages as the stack shows them: one that went (it ran out, or was clicked away)
        // stays a moment to close up as it fades. Kept while shown in every pass.
        let (clock, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
        let leave = f64::from(motion::secs(motion::LEAVE).max(0.001));
        let stack_id = Id::new("toast-stack");
        let mut stack = ctx.data(|d| d.get_temp::<(u64, Vec<Stacked>)>(stack_id)).filter(|(at, _)| at + 1 == pass).map(|(_, s)| s).unwrap_or_default();
        for s in &mut stack {
            if s.left.is_none() && !self.toasts.iter().any(|t| t.text == s.text) {
                s.left = Some(clock);
            }
        }
        stack.retain(|s| s.left.is_none_or(|at| clock - at < leave));
        for t in &self.toasts {
            if !stack.iter().any(|s| s.left.is_none() && s.text == t.text) {
                stack.push(Stacked { text: t.text.clone(), error: t.error, left: None });
            }
        }
        ctx.data_mut(|d| d.insert_temp(stack_id, (pass, stack.clone())));
        if stack.is_empty() {
            return;
        }
        if let Some(next_expiry) = self
            .toasts
            .iter()
            .filter(|t| !t.error)
            .map(|t| STATUS_TOAST_TIME.saturating_sub(now.duration_since(t.created)))
            .min()
        {
            ctx.request_repaint_after(next_expiry + Duration::from_millis(20));
        }
        let p = self.palette;
        let bottom = if self.auth == AuthState::LoggedIn { player::BAR_HEIGHT + 12.0 } else { 16.0 };
        let mut dismiss = None;
        let shown = egui::Area::new(Id::new("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -bottom))
            .order(egui::Order::Foreground)
            .fade_in(false)
            .show(ctx, |ui| {
                ui.set_max_width(380.0);
                for (i, t) in stack.iter().enumerate() {
                    let accent = if t.error { p.error } else { p.accent };
                    let frame = Frame::new()
                        .fill(p.raised_bg)
                        .stroke(egui::Stroke::new(1.0, accent.gamma_multiply(0.6)))
                        .corner_radius(CornerRadius::same(kit::RADIUS))
                        .inner_margin(Margin::symmetric(12, 9))
                        .shadow(ui.visuals().popup_shadow);
                    // Each rises into place as it fades in and its room opens, so the others move
                    // up; one going sinks as it fades out and its room closes.
                    let shows = match t.left {
                        None => motion::arrive(ctx, Id::new(("toast", &t.text)), motion::MESSAGE_IN),
                        Some(at) => {
                            ctx.request_repaint();
                            1.0 - motion::ease_in(((clock - at) / leave) as f32)
                        }
                    };
                    let drift = egui::vec2(0.0, motion::offset(motion::MESSAGE_RISE, shows, motion::moves()));
                    let place = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.max_rect().width(), f32::INFINITY));
                    let mut one = ui.new_child(egui::UiBuilder::new().max_rect(place));
                    one.multiply_opacity(shows);
                    let drawn = motion::moved(&mut one, drift, |ui| {
                        frame.show(ui, |ui| {
                            ui.set_width(340.0);
                            ui.add(egui::Label::new(RichText::new(&t.text).color(p.text)).wrap());
                            if t.error {
                                ui.label(RichText::new("Click to dismiss").small().color(p.text_weak));
                            }
                        })
                    });
                    let rect = drawn.response.rect;
                    ui.allocate_exact_size(egui::vec2(rect.width(), (rect.height() + 6.0) * motion::room(shows, motion::moves())), Sense::hover());
                    if t.left.is_none() && ui.interact(rect, Id::new(("toast-click", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        dismiss = Some(t.text.clone());
                    }
                }
            });
        // Once the last one goes, they sink back as they fade out.
        motion::fade_out_later(ctx, shown.response.layer_id, Id::new("toasts"), egui::vec2(0.0, motion::MESSAGE_RISE));
        if let Some(text) = dismiss {
            self.toasts.retain(|t| t.text != text);
        }
    }

    fn handle_cover_picks(&mut self) {
        while let Ok((uri, path)) = self.cover_rx.try_recv() {
            self.cover_picker_open = false;
            if let Some(image_path) = path {
                self.toast("Uploading the new cover…", false);
                self.send(Command::SetPlaylistCover { uri, image_path });
            }
        }
    }

    /// Opens the native file picker on a background thread (it blocks until closed).
    fn pick_cover(&mut self, ctx: &egui::Context, uri: String) {
        if self.cover_picker_open {
            return;
        }
        self.cover_picker_open = true;
        let tx = self.cover_tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("truffle-file-dialog".into()).spawn(move || {
            let path = rfd::FileDialog::new()
                .set_title("Choose a cover image")
                .add_filter("Images", &["jpg", "jpeg", "png", "webp"])
                .pick_file();
            let _ = tx.send((uri, path));
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            log::warn!("could not open the file picker: {e}");
            self.cover_picker_open = false;
            self.toast("Could not open the file picker.", true);
        }
    }
}

/// Which toast to drop when there are too many: the oldest status message other than the one just
/// added (the last), else the oldest one. Errors stay until dismissed, so with a full set of
/// errors the oldest error goes; never the new message.
fn toast_to_drop(toasts: &[Toast]) -> usize {
    let older = &toasts[..toasts.len().saturating_sub(1)];
    older.iter().position(|t| !t.error).unwrap_or(0)
}

fn rgb(c: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(c[0], c[1], c[2])
}

/// How long until a countdown that ends at `until` shows the next whole second.
fn until_next_second(until: Instant, now: Instant) -> Duration {
    let left = until.saturating_duration_since(now);
    if left.is_zero() {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(u64::from(left.subsec_millis()) + 15)
    }
}

impl TruffleApp {
    /// Periodic repaints while something on screen moves on its own. Only called for drawn
    /// frames, so a hidden or minimized window never ticks.
    fn schedule_repaint(&self, ctx: &egui::Context) {
        if self.is_playing() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if let Some(SleepTimerState::Until(at)) = self.sleep_timer {
            ctx.request_repaint_after(until_next_second(at, Instant::now()));
        }
    }
}

impl eframe::App for TruffleApp {
    /// Runs every frame, also while the window is hidden in the tray or minimized (then without
    /// `ui`): the one place controller events and platform actions are handled.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(ctx, event);
        }
        self.images.poll();
        self.fonts.poll(ctx);
        self.handle_cover_picks();
        self.shell_logic(ctx);
        self.flush_settings_if_due(ctx);
        if let Some(SleepTimerState::Until(at)) = self.sleep_timer
            && Instant::now().saturating_duration_since(at) > SLEEP_TIMER_STALE
        {
            // The controller reports the timer firing; if that never came, stop showing "0:00".
            self.sleep_timer = None;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Every pass, drawn or not: the zoom this pass's window sizes were measured with.
        let measured_zoom = self.screen_zoom.next_pass(ctx.zoom_factor(), ctx.current_pass_index() > 0);
        self.memory_probe(&ctx);
        self.frame.self_shot(&ctx, self.shell.hidden, self.shell.switched, self.shell.mini);
        if self.shell.hidden {
            // eframe still runs passes for a hidden window; there is nothing to draw.
            return;
        }
        if ctx.input(|i| i.viewport().minimized == Some(true)) {
            // Only while Settings has a window of its own (eframe counts it as shown, though
            // Windows hid it with this one): nothing to draw, and that window goes until this one
            // is back, so minimised Truffle stops drawing again.
            self.park_settings_window();
            return;
        }
        self.images.begin_frame();
        // Here, not in `logic`: before the first drawn frame egui doesn't know the graphics card's
        // largest texture yet (a hidden start), and the picture is only needed once it shows.
        self.wallpaper_logic(&ctx);
        self.track_window(&ctx);
        self.handle_shortcuts(ui);
        self.sync_zoom(&ctx, measured_zoom);
        self.keep_mini_size(&ctx);
        self.update_accent(&ctx);
        motion::follow_windows(&ctx);

        if std::mem::take(&mut self.shell.switched) {
            // The window changed mode (or the mini player its size) in this pass, but keeps its
            // old size until the pass ends: draw nothing in it, the next pass has the new size.
            ctx.request_repaint();
        } else if (self.shell.mini || self.shot_flag("mini")) && self.auth == AuthState::LoggedIn {
            self.mini_player(ui);
        } else {
            match self.auth {
                _ if self.shot_flag("stage-demo") => self.stage_demo(ui),
                AuthState::LoggedIn if !self.shot_flag("login") => self.main_ui(ui),
                _ => self.login_screen(ui),
            }
            if self.auth == AuthState::LoggedIn {
                // Before `settings_logic`, which sends what was changed in it this pass.
                self.settings_window(&ctx);
                self.settings_logic(&ctx);
                self.edit_details_modal(&ctx);
                self.yt_pick_modal(&ctx);
                self.new_playlist_modal(&ctx);
                self.banner_adjust_modal(&ctx);
                self.remove_playlist_modal(&ctx);
                self.already_in_modal(&ctx);
                self.dupes_modal(&ctx);
                self.link_dialog_modal(&ctx);
            }
            if self.auth != AuthState::LoggedIn {
                self.toasts_ui(&ctx);
            }
        }
        // A switch made while drawing (the mini player's own buttons) happens at the very end of
        // the pass, after everything was laid out: nothing to skip next time.
        self.shell.switched = false;
        motion::end_pass(&ctx);
        self.images.end_frame();
        self.schedule_repaint(&ctx);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.take_middle_button(ctx, raw_input);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.palette.main_bg.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.settings_dirty_at.take().is_some() {
            self.save_settings_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every setting is saved by someone: the controller (changed through commands) or the UI's
    /// `prefs_changed`. A new UI setting left out of `prefs_changed` would be lost on restart.
    #[test]
    fn every_ui_setting_is_saved() {
        const CONTROLLER_OWNED: &[&str] = &[
            "volume", "shuffle", "repeat", "shuffle_rules", "webapi_client_id", "equalizer", "crossfade_secs",
            "sound_source", "listen_share", "listen_channel", "listen_hosts", "close_to_tray", "lastfm_enabled",
            "lastfm_api_key", "lastfm_api_secret",
        ];
        let config = include_str!("../config.rs");
        let start = config.find("pub struct Settings {").expect("Settings");
        let body = &config[start..start + config[start..].find("\n}").expect("end of Settings")];
        let ui = include_str!("mod.rs");
        let from = ui.find("fn prefs_changed").expect("prefs_changed");
        let saver = &ui[from..from + ui[from..].find("settings_dirty_at = Some").expect("end of prefs_changed")];
        let mut checked = 0;
        for line in body.lines().skip(1) {
            let Some(name) = line.trim().strip_prefix("pub ").and_then(|l| l.split(':').next()) else { continue };
            if !CONTROLLER_OWNED.contains(&name) {
                assert!(saver.contains(&format!("s.{name} = p.{name}")), "the UI never saves the setting {name:?}");
                checked += 1;
            }
        }
        assert!(checked > 30, "only {checked} settings found: the check can't read config.rs any more");
    }

    fn playlist(name: &str, owned: bool) -> PlaylistSummary {
        PlaylistSummary {
            uri: format!("spotify:playlist:{name}{owned}"),
            name: name.into(),
            description: String::new(),
            owner_name: String::new(),
            image_url: None,
            track_count: None,
            revision: String::new(),
            editable: owned,
            owned,
            owner_id: String::new(),
        }
    }

    #[test]
    fn autoplay_finds_playlists_by_name() {
        let list = [playlist("Moonlight", false), playlist("moonlight", true), playlist("Gym mix", true), playlist("Gym old", true), playlist("Mötley Crüe", true)];
        // Exact match ignores case and prefers the user's own playlist.
        assert_eq!(find_playlist_by_name(&list, " MOONLIGHT ").map(|p| p.owned), Some(true));
        // A unique partial match is accepted, an ambiguous one is not.
        assert_eq!(find_playlist_by_name(&list, "mix").map(|p| p.name.as_str()), Some("Gym mix"));
        assert!(find_playlist_by_name(&list, "gym").is_none());
        assert!(find_playlist_by_name(&list, "jazz").is_none());
        // Accents aside, either way.
        assert_eq!(find_playlist_by_name(&list, "motley").map(|p| p.name.as_str()), Some("Mötley Crüe"));
        assert_eq!(find_playlist_by_name(&list, "moonlíght").map(|p| p.owned), Some(true));
        assert!(find_playlist_by_name(&list, "  ").is_none());
    }

    #[test]
    fn autoplay_prefers_the_name_spelled_as_typed() {
        let list = [playlist("Cafe", true), playlist("Café", true), playlist("Motley Mix", true), playlist("Mötley", true)];
        // Exact: the spelling typed wins, whatever the list order and ownership.
        assert_eq!(find_playlist_by_name(&list, "café").map(|p| p.name.as_str()), Some("Café"));
        assert_eq!(find_playlist_by_name(&list, "CAFE").map(|p| p.name.as_str()), Some("Cafe"));
        let others = [playlist("Café", true), playlist("Cafe", false)];
        assert_eq!(find_playlist_by_name(&others, "cafe").map(|p| p.name.as_str()), Some("Cafe"));
        // Partial: one hit spelled as typed beats more hits accents aside.
        assert_eq!(find_playlist_by_name(&list, "Mötl").map(|p| p.name.as_str()), Some("Mötley"));
        assert_eq!(find_playlist_by_name(&list, "motl").map(|p| p.name.as_str()), Some("Motley Mix"));
        assert!(find_playlist_by_name(&list, "tley").is_none());
        // With no spelled hit, accents aside still finds it.
        assert_eq!(find_playlist_by_name(&list[3..], "motl").map(|p| p.name.as_str()), Some("Mötley"));
    }

    fn toasts(errors: &[bool]) -> Vec<Toast> {
        errors.iter().enumerate().map(|(i, &error)| Toast { text: i.to_string(), error, created: Instant::now(), undo: None }).collect()
    }

    #[test]
    fn countdown_ticks_on_whole_seconds() {
        let now = Instant::now();
        assert_eq!(until_next_second(now + Duration::from_millis(2_300), now), Duration::from_millis(315));
        assert_eq!(until_next_second(now, now + Duration::from_secs(5)), Duration::from_secs(1));
    }

    #[test]
    fn a_new_status_survives_a_screen_full_of_errors() {
        // Five sticky errors plus a new status: the oldest error goes, the status stays.
        assert_eq!(toast_to_drop(&toasts(&[true, true, true, true, true, false])), 0);
        // Otherwise the oldest earlier status goes first.
        assert_eq!(toast_to_drop(&toasts(&[true, false, true, false, true, false])), 1);
        assert_eq!(toast_to_drop(&toasts(&[true, true, true, true, true, true])), 0);
    }
}
