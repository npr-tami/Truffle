//! App directories and persisted settings (`%APPDATA%\truffle\...`).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{RepeatMode, ShuffleRules};

#[derive(Debug, Clone)]
pub struct Paths {
    /// Roaming config dir: settings.json, tokens.
    pub config: PathBuf,
    /// Local cache dir: playlist track caches, librespot audio cache.
    pub cache: PathBuf,
}

/// Where release builds write their log: `%LOCALAPPDATA%\truffle\logs\truffle.log`, beside the
/// cache folder. Debug builds log to the console instead.
pub fn log_file() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "truffle")?;
    Some(dirs.cache_dir().with_file_name("logs").join("truffle.log"))
}

/// Debug builds only: keep all data (`config\`, `cache\`) under this folder instead of the user's
/// profile, so a development build can run next to an installed Truffle without sharing its
/// login, settings or play history.
#[cfg(debug_assertions)]
pub const DEV_DATA_DIR_ENV: &str = "TRUFFLE_DEV_DATA_DIR";

/// Tests that keep files (YouTube downloads, the sample song, speed reports) put them under
/// `TRUFFLE_TEST_DIR` when it is set, else under the temp folder.
#[cfg(test)]
pub fn test_root() -> PathBuf {
    std::env::var_os("TRUFFLE_TEST_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

impl Paths {
    pub fn new() -> Result<Self> {
        #[cfg(debug_assertions)]
        if let Some(root) = std::env::var_os(DEV_DATA_DIR_ENV).filter(|v| !v.is_empty()) {
            let root = PathBuf::from(root);
            log::warn!("{DEV_DATA_DIR_ENV} is set: using data in {}", root.display());
            let paths = Self { config: root.join("config"), cache: root.join("cache") };
            std::fs::create_dir_all(&paths.config)?;
            std::fs::create_dir_all(&paths.cache)?;
            return Ok(paths);
        }
        let dirs = directories::ProjectDirs::from("", "", "truffle")
            .context("could not find a home directory")?;
        let paths = Self {
            config: dirs.config_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
        };
        std::fs::create_dir_all(&paths.config)?;
        std::fs::create_dir_all(&paths.cache)?;
        Ok(paths)
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }
    /// Listen-along links (one per friend), kicks and removed links. Not in settings.json: an
    /// older Truffle saves that whole file at every exit and drops what it doesn't know, which
    /// would let every kicked friend back in.
    pub fn listen_links_file(&self) -> PathBuf {
        self.config.join("listen_links.json")
    }
    /// Likes and unlikes not yet sent to Spotify, per account (see `library::likes`).
    pub fn like_queue_file(&self) -> PathBuf {
        self.config.join("like_queue.json")
    }
    /// librespot credential cache directory (credentials.json lives here).
    pub fn librespot_dir(&self) -> PathBuf {
        self.config.join("librespot")
    }
    /// Web API OAuth tokens (refresh token).
    pub fn webapi_token_file(&self) -> PathBuf {
        self.config.join("webapi_token.json")
    }
    /// Last.fm session key + user name.
    pub fn lastfm_session_file(&self) -> PathBuf {
        self.config.join("lastfm_session.json")
    }
    /// Scrobbles not yet accepted by Last.fm (survives restarts and offline periods).
    pub fn lastfm_queue_file(&self) -> PathBuf {
        self.config.join("lastfm_queue.json")
    }
    /// Queue position, play counts, blocked tracks.
    pub fn state_file(&self) -> PathBuf {
        self.config.join("state.json")
    }
    /// Per-source track list caches.
    pub fn tracks_cache_dir(&self) -> PathBuf {
        self.cache.join("tracks")
    }
    /// librespot audio file cache.
    pub fn audio_cache_dir(&self) -> PathBuf {
        self.cache.join("audio")
    }
    /// Downloaded album/playlist art.
    pub fn image_cache_dir(&self) -> PathBuf {
        self.cache.join("images")
    }
    /// Truffle's data folder (`%LOCALAPPDATA%\truffle`): the cache folder's parent, which also has
    /// the logs (and may have yt-dlp).
    pub fn data_dir(&self) -> PathBuf {
        self.cache.parent().map_or_else(|| self.cache.clone(), Path::to_path_buf)
    }

    /// Delete temp files that [`write_json`] left behind when Truffle was killed between writing
    /// and renaming one. Call once at startup.
    pub fn remove_stale_temp_files(&self) {
        for dir in [self.config.clone(), self.cache.clone(), self.tracks_cache_dir()] {
            remove_stale_temp_files(&dir);
        }
    }
}

/// Light or dark. Saved by name, so files from before "Same as Windows" (only "Dark" or "Light")
/// still load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    /// Same as Windows' mode for apps (Settings > Personalisation > Colours).
    System,
}

/// How the left sidebar shows playlists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LibraryView {
    /// Rows with a small cover, name and owner.
    List,
    /// Record sleeves, two across, with the vinyl peeking out.
    #[default]
    Sleeves,
}

/// How the side panel's Friends tab shows your links and your friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FriendsLayout {
    /// A card about sharing above the friends; your links open in it.
    #[default]
    Card,
    /// Your links, then your friends, in one list.
    List,
    /// A switch at the top: your friends, or your listeners.
    Switch,
}

/// A list page's banner: the picture, and how it sits (moved and zoomed by the user).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "BannerSaved")]
pub struct Banner {
    /// Full path of the picture.
    pub path: String,
    /// 1 shows as much as fits; up to [`Banner::MAX_ZOOM`] times closer.
    pub zoom: f32,
    /// The point of the picture at the middle of the banner, 0 to 1 across and down.
    pub x: f32,
    pub y: f32,
}

impl Banner {
    pub const MAX_ZOOM: f32 = 4.0;

    pub fn new(path: String) -> Self {
        Self { path, zoom: 1.0, x: 0.5, y: 0.5 }
    }
}

/// A banner as saved: older files kept only the picture's path.
#[derive(Deserialize)]
#[serde(untagged)]
enum BannerSaved {
    Path(String),
    Full {
        path: String,
        #[serde(default = "one")]
        zoom: f32,
        #[serde(default = "half")]
        x: f32,
        #[serde(default = "half")]
        y: f32,
    },
}

fn one() -> f32 {
    1.0
}

fn half() -> f32 {
    0.5
}

impl From<BannerSaved> for Banner {
    fn from(saved: BannerSaved) -> Self {
        match saved {
            BannerSaved::Path(path) => Banner::new(path),
            BannerSaved::Full { path, zoom, x, y } => {
                let ok = |v: f32, lo: f32, hi: f32, or: f32| if v.is_finite() { v.clamp(lo, hi) } else { or };
                Banner { path, zoom: ok(zoom, 1.0, Banner::MAX_ZOOM, 1.0), x: ok(x, 0.0, 1.0, 0.5), y: ok(y, 0.0, 1.0, 0.5) }
            }
        }
    }
}

/// Which app icon Truffle shows (window, taskbar, tray).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AppIcon {
    /// The one that stands out on the taskbar: silver on a dark one, black on a light one.
    #[default]
    Automatic,
    /// The silver turntable.
    Silver,
    /// The black turntable, framed in chrome.
    Black,
}

/// Where the sound of a song comes from. The library, likes and search always come from Spotify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SoundSource {
    /// Spotify plays the song itself (needs Premium).
    #[default]
    Spotify,
    /// The same song found on YouTube (free accounts too).
    YouTube,
}

/// How a new page shows when switching pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PageAnimation {
    None,
    /// The new page fades in as it lifts into place (it was a plain fade, Crossfade, before).
    #[default]
    #[serde(alias = "Crossfade")]
    Rise,
    /// The top of the page shows first, the rest follows down the page.
    TopFirst,
}

/// How fast things move and fade (Settings > Appearance > Motion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MotionSpeed {
    Snappy,
    #[default]
    Normal,
    Relaxed,
}

/// How pages use a very wide window (Settings > Appearance > Wide screens).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WideScreens {
    /// At most 1800 wide, in the middle.
    #[default]
    Centred,
    /// The whole width.
    Fill,
    /// Home, Profile and the song lists in two columns from 1900 wide.
    Split,
}

/// Height of the big header above a playlist's songs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HeroSize {
    #[default]
    Tall,
    Short,
    Off,
}

/// Which playlists the left sidebar lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlaylistFilter {
    #[default]
    All,
    /// Only playlists the user created.
    Mine,
    /// Only playlists made by others (followed or collaborative).
    Others,
}

/// How the wallpaper is made to fit its area (Glint's `wallpaper-fit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WallpaperFit {
    /// It covers the whole area, cut to fit.
    #[default]
    Fill,
    /// All of it shows, as big as fits, at its side.
    Whole,
    /// Pulled to the area's exact size, whatever its shape.
    Stretch,
    /// Repeated across the area at its own size (times the zoom).
    Tile,
    /// One copy at its own size (times the zoom).
    Actual,
}

/// The wallpaper and how it sits (Glint's `wallpaper-*` skin settings).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "WallpaperSaved", into = "WallpaperSaved")]
pub struct Wallpaper {
    /// Full path of the picture; empty for none.
    pub path: String,
    /// How strongly it shows, 0 to 100 ("Opacity").
    pub strength: u8,
    /// Behind the whole window instead of only the songs.
    pub whole_window: bool,
    pub fit: WallpaperFit,
    /// Which side stays in view: 0 left, 0.5 middle, 1 right.
    pub at_x: f32,
    /// 0 top, 0.5 middle, 1 bottom.
    pub at_y: f32,
    /// Zoom in (up to 500) or out (down to -500); 0 is its usual size, 100 twice as big.
    pub zoom: i16,
    /// How blurred it is, 0 (sharp) to 50, in the pixels of the picture kept at most 4K.
    pub blur: u8,
    /// How much of the area's width and height it fades out over at the area's edges, 0 (none)
    /// to 50 (half).
    pub soft_edges: u8,
}

impl Wallpaper {
    pub const MAX_ZOOM: i16 = 500;
    pub const MAX_BLUR: u8 = 50;
    pub const MAX_SOFT_EDGES: u8 = 50;
}

impl Default for Wallpaper {
    fn default() -> Self {
        Self {
            path: String::new(),
            strength: 40,
            whole_window: false,
            fit: WallpaperFit::Fill,
            at_x: 1.0,
            at_y: 0.0,
            zoom: 0,
            blur: 0,
            soft_edges: 0,
        }
    }
}

/// The wallpaper as saved. Files from before the fit choices had only `fit_whole` (all of the
/// picture, or filling the area): it is still written, so an older Truffle shows the same fit.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct WallpaperSaved {
    path: String,
    strength: u8,
    whole_window: bool,
    fit_whole: bool,
    /// Not in older files: `fit_whole` says the fit then.
    fit: Option<WallpaperFit>,
    at_x: f32,
    at_y: f32,
    zoom: i16,
    blur: u8,
    soft_edges: u8,
}

impl Default for WallpaperSaved {
    /// What a value missing from the file means: a new wallpaper's, but no fit (so `fit_whole`
    /// says it).
    fn default() -> Self {
        WallpaperSaved { fit: None, ..Wallpaper::default().into() }
    }
}

impl From<WallpaperSaved> for Wallpaper {
    fn from(s: WallpaperSaved) -> Self {
        let fit = s.fit.unwrap_or(if s.fit_whole { WallpaperFit::Whole } else { WallpaperFit::Fill });
        // Kept within their ranges (a value from a hand-edited file).
        let side = |v: f32, or: f32| if v.is_finite() { v.clamp(0.0, 1.0) } else { or };
        let d = Wallpaper::default();
        Wallpaper {
            path: s.path,
            strength: s.strength.min(100),
            whole_window: s.whole_window,
            fit,
            at_x: side(s.at_x, d.at_x),
            at_y: side(s.at_y, d.at_y),
            zoom: s.zoom.clamp(-Wallpaper::MAX_ZOOM, Wallpaper::MAX_ZOOM),
            blur: s.blur.min(Wallpaper::MAX_BLUR),
            soft_edges: s.soft_edges.min(Wallpaper::MAX_SOFT_EDGES),
        }
    }
}

impl From<Wallpaper> for WallpaperSaved {
    fn from(w: Wallpaper) -> Self {
        WallpaperSaved {
            path: w.path,
            strength: w.strength,
            whole_window: w.whole_window,
            fit_whole: w.fit == WallpaperFit::Whole,
            fit: Some(w.fit),
            at_x: w.at_x,
            at_y: w.at_y,
            zoom: w.zoom,
            blur: w.blur,
            soft_edges: w.soft_edges,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// 0..=100
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub shuffle_rules: ShuffleRules,
    pub theme: ThemeMode,
    /// Accent colour as RGB.
    pub accent: [u8; 3],
    /// The interface size from before each screen had its own (1.0 = none). It becomes the size of
    /// the screen Truffle next opens on, and is 1.0 from then on (`ui/screens.rs`).
    pub ui_scale: f32,
    /// Optional Spotify developer app client id for Web API editing features.
    pub webapi_client_id: Option<String>,
    /// Audio quality: "low" (96), "normal" (160), "high" (320) kbps.
    pub bitrate: String,
    /// Volume normalisation.
    pub normalisation: bool,
    /// Size limit for librespot's audio cache, in megabytes (0 = no audio cache).
    pub audio_cache_mb: u64,
    pub show_queue_panel: bool,
    pub show_rules_panel: bool,
    /// Last opened source, reopened on start.
    pub last_source: Option<crate::model::Source>,
    pub playlist_filter: PlaylistFilter,
    /// Playlists pinned to the top of the list, in pin order (URIs).
    pub pinned_playlists: Vec<String>,

    // --- Audio extras (controller-owned: changed through Commands) ---
    pub equalizer: crate::model::EqSettings,
    /// Crossfade seconds between tracks, 0 = off (gapless).
    pub crossfade_secs: u8,
    /// Where songs' sound comes from (a free account always uses YouTube).
    pub sound_source: SoundSource,

    // --- Listen along (controller-owned: changed through Commands) ---
    /// Friends may listen along: what plays is written to their links (`listen_links.json`).
    pub listen_share: bool,
    /// The link older Truffles write to (`spotify:playlist:...`): always the first link that
    /// isn't kicked. One they make is taken in as a link at the next start.
    pub listen_channel: Option<String>,
    /// Truffle hosts joined with a link: (name, channel uri).
    pub listen_hosts: Vec<(String, String)>,

    // --- Windows integration (UI-owned) ---
    /// Show the current song in Windows' media overlay / lock screen and react to media keys.
    pub media_controls: bool,
    /// Show a tray icon with playback controls.
    pub tray_icon: bool,
    /// "Keep running in the background": closing the window hides it instead of quitting (the
    /// tray's Quit still quits).
    pub keep_running: bool,
    /// "Close to the tray", in files from before `keep_running`: it only acted while the tray icon
    /// showed. Read to carry the choice over (see [`Settings::load`]), never saved again.
    #[serde(skip_serializing)]
    pub close_to_tray: Option<bool>,
    /// Start Truffle when Windows starts (HKCU Run key), hidden in the tray (minimised without it).
    pub start_with_windows: bool,
    /// Discord "Listening to" status. No join/party features are ever sent.
    pub discord_status: bool,
    /// Discord application id (from discord.com/developers/applications). Discord shows
    /// "Listening to <song>" and the app's name on the profile.
    pub discord_app_id: Option<String>,

    // --- Last.fm (controller-owned: changed through Commands) ---
    /// Scrobble plays and show "now playing" on Last.fm.
    pub lastfm_enabled: bool,
    /// The user's own Last.fm API key and shared secret (last.fm/api/account/create).
    pub lastfm_api_key: Option<String>,
    pub lastfm_api_secret: Option<String>,

    // --- Look and feel (UI-owned) ---
    /// Take the accent colour from the current album art.
    pub dynamic_accent: bool,
    /// Denser rows and smaller paddings.
    pub compact: bool,
    /// The interface size set on each screen (Ctrl + / Ctrl −, or Settings), by the screen's key
    /// (`ui/screens.rs`). A screen without one gets a size from its height.
    pub screen_sizes: std::collections::BTreeMap<String, f32>,
    /// "Grow with the window": the interface gets bigger on wide windows (up to 1.5 times).
    pub grow_with_window: bool,
    /// Screens Truffle told it made itself bigger on (once each).
    pub big_screen_tips: std::collections::BTreeSet<String>,
    /// Optional full custom colour scheme on top of the dark/light base.
    pub custom_colors: Option<CustomColors>,
    /// A picture behind the songs (or the whole window), as in Glint.
    pub wallpaper: Wallpaper,
    /// Playlists as rows or as record sleeves.
    pub library_view: LibraryView,
    /// Height of the playlist header.
    pub hero_size: HeroSize,
    /// Widths of the playlists panel and the record panel the user last dragged them to
    /// (logical points); `None` until first resized.
    pub library_width: Option<f32>,
    pub deck_width: Option<f32>,
    /// Banner pictures for list pages, shown only in Truffle, by `Source::cache_key`.
    pub banners: std::collections::BTreeMap<String, Banner>,
    /// The Settings window's size (points), once resized; it opens over the middle of Truffle.
    pub settings_size: Option<[f32; 2]>,
    /// The app icon's style.
    pub app_icon: AppIcon,
    /// At wide widths the sidebar is folded to the 56 strip (Ctrl+B).
    pub sidebar_folded: bool,
    /// At medium widths (760-1199) the side panel is open (Ctrl+P).
    pub panel_medium: bool,
    /// The side panel shows the turntable (Ctrl+D).
    pub panel_record: bool,
    /// Liked Songs sits in the sidebar's Pinned section (else first under Playlists).
    pub liked_pinned: bool,
    /// How the Friends tab shows your links and your friends (files from before it: Card).
    pub friends_layout: FriendsLayout,
    /// The Friends tab's card shows your links (its MANAGE LINKS is open).
    pub friends_links_open: bool,
    /// Lists opened lately, newest first (Home's tiles and Recently played).
    pub recent_lists: Vec<crate::model::Source>,
    /// Searches used lately (newest first), for the Search page.
    pub recent_searches: Vec<String>,
    /// The record turns while music plays (redraws the record about 20 times a second meanwhile).
    pub spin_record: bool,
    /// How a new page shows when switching pages.
    pub page_animation: PageAnimation,
    /// How fast things move and fade.
    pub motion_speed: MotionSpeed,
    /// How big songs in lists are: 0 default, 1 medium, 2 big.
    pub song_size: u8,
    /// Artist and Date added columns in song lists (on wide windows).
    pub more_columns: bool,
    /// Bigger cards on the Profile page.
    pub fill_cards: bool,
    /// How pages use a very wide window.
    pub wide_screens: WideScreens,
    /// Mini player: window size/position and always-on-top.
    pub mini_player_on_top: bool,
    pub mini_player_pos: Option<[f32; 2]>,
    pub main_window_size: Option<[f32; 2]>,
    /// Where the full window goes back to (Windows' "normal" place: left, top, right, bottom in
    /// pixels), and whether it was maximized: it opens there again, on the same screen.
    pub main_window_rect: Option<[i32; 4]>,
    pub main_window_maximized: bool,
    /// The Now playing view's window size as it was left (logical pixels).
    pub stage_window_size: Option<[f32; 2]>,
}

/// User-chosen colours (RGB). Any `None` keeps the base theme's colour.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomColors {
    pub background: Option<[u8; 3]>,
    pub panel: Option<[u8; 3]>,
    pub text: Option<[u8; 3]>,
    pub text_weak: Option<[u8; 3]>,
    pub hover: Option<[u8; 3]>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 70,
            shuffle: true,
            repeat: RepeatMode::Off,
            shuffle_rules: ShuffleRules::default(),
            theme: ThemeMode::Dark,
            accent: [227, 38, 45],
            ui_scale: 1.0,
            webapi_client_id: None,
            bitrate: "high".to_owned(),
            normalisation: true,
            audio_cache_mb: 1024,
            show_queue_panel: false,
            show_rules_panel: true,
            last_source: None,
            playlist_filter: PlaylistFilter::All,
            pinned_playlists: Vec::new(),
            equalizer: crate::model::EqSettings::default(),
            crossfade_secs: 0,
            sound_source: SoundSource::default(),
            listen_share: false,
            listen_channel: None,
            listen_hosts: Vec::new(),
            media_controls: true,
            tray_icon: true,
            keep_running: false,
            close_to_tray: None,
            start_with_windows: false,
            discord_status: false,
            discord_app_id: None,
            lastfm_enabled: true,
            lastfm_api_key: None,
            lastfm_api_secret: None,
            dynamic_accent: true,
            compact: false,
            screen_sizes: std::collections::BTreeMap::new(),
            grow_with_window: false,
            big_screen_tips: std::collections::BTreeSet::new(),
            custom_colors: None,
            wallpaper: Wallpaper::default(),
            library_view: LibraryView::default(),
            hero_size: HeroSize::default(),
            spin_record: true,
            page_animation: PageAnimation::default(),
            motion_speed: MotionSpeed::default(),
            song_size: 0,
            more_columns: false,
            fill_cards: false,
            wide_screens: WideScreens::default(),
            library_width: None,
            deck_width: None,
            banners: std::collections::BTreeMap::new(),
            settings_size: None,
            app_icon: AppIcon::default(),
            sidebar_folded: false,
            panel_medium: false,
            panel_record: true,
            liked_pinned: true,
            friends_layout: FriendsLayout::default(),
            friends_links_open: false,
            recent_lists: Vec::new(),
            recent_searches: Vec::new(),
            mini_player_on_top: true,
            mini_player_pos: None,
            main_window_size: None,
            main_window_rect: None,
            main_window_maximized: false,
            stage_window_size: None,
        }
    }
}

impl Settings {
    /// Load settings.json. A file that is briefly locked (another Truffle finishing its save, a
    /// virus scanner) is retried, and a file with one bad value keeps every other value instead
    /// of silently falling back to all defaults (which the next save would then write to disk).
    pub fn load(paths: &Paths) -> Self {
        let path = paths.settings_file();
        let Some(bytes) = read_with_retry(&path) else { return Self::default() };
        let mut settings = match serde_json::from_slice::<Settings>(&bytes) {
            Ok(settings) => settings,
            Err(e) => {
                log::warn!("settings.json is partly unreadable ({e}); keeping every value that can be read");
                let backup = path.with_extension(format!("json.unreadable-{}", unix_secs()));
                if let Err(e) = std::fs::copy(&path, &backup) {
                    log::warn!("could not keep a copy of the unreadable settings: {e}");
                }
                Self::from_partial(&bytes)
            }
        };
        settings.carry_over();
        settings
    }

    /// Carries choices in an older file over to the settings that replaced them.
    fn carry_over(&mut self) {
        // "Close to the tray" without the tray icon meant closing quits. "Keep running in the
        // background" hides without it too, so it is only on where closing used to hide.
        if let Some(close_to_tray) = self.close_to_tray.take() {
            self.keep_running |= close_to_tray && self.tray_icon;
        }
    }

    /// Defaults, overwritten by every top-level value of `bytes` that is valid on its own.
    fn from_partial(bytes: &[u8]) -> Self {
        let Ok(serde_json::Value::Object(file)) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return Self::default();
        };
        let Ok(serde_json::Value::Object(mut merged)) = serde_json::to_value(Self::default()) else {
            return Self::default();
        };
        for (key, value) in file {
            let mut candidate = merged.clone();
            candidate.insert(key.clone(), value.clone());
            if serde_json::from_value::<Settings>(serde_json::Value::Object(candidate)).is_ok() {
                merged.insert(key, value);
            } else {
                log::warn!("ignoring the unreadable setting {key:?}");
            }
        }
        serde_json::from_value(serde_json::Value::Object(merged)).unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_json(&paths.settings_file(), self)
    }

    /// Save the settings shared by the UI and the controller without holding their lock during
    /// the disk write (the UI thread takes that lock on every change, so a slow disk must never
    /// stall it). The snapshot and a sequence number are taken together under the lock; writes
    /// are serialized on a lock of their own and a snapshot older than the one already written to
    /// the same file is skipped, so concurrent saves can never leave an older snapshot on disk.
    /// (Per file: a newer save of another settings file must not make this one's skip.)
    pub fn save_shared(shared: &Mutex<Settings>, paths: &Paths) -> Result<()> {
        static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
        static WRITTEN: Mutex<BTreeMap<PathBuf, u64>> = Mutex::new(BTreeMap::new());
        let (snapshot, seq) = {
            let settings = shared.lock().unwrap_or_else(PoisonError::into_inner);
            (settings.clone(), SNAPSHOTS.fetch_add(1, Ordering::Relaxed) + 1)
        };
        let file = paths.settings_file();
        let mut written = WRITTEN.lock().unwrap_or_else(PoisonError::into_inner);
        if written.get(&file).is_some_and(|&w| w > seq) {
            return Ok(());
        }
        snapshot.save(paths)?;
        written.insert(file, seq);
        Ok(())
    }
}

/// Read a file, retrying for about a second while it is locked (Windows refuses reads while
/// another process holds it open without sharing, e.g. during a save or a virus scan).
/// `None` if it doesn't exist or stays unreadable.
pub fn read_with_retry(path: &Path) -> Option<Vec<u8>> {
    const ATTEMPTS: u32 = 6;
    for attempt in 1..=ATTEMPTS {
        match std::fs::read(path) {
            Ok(bytes) => return Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) if attempt < ATTEMPTS => {
                log::debug!("{} is busy ({e}); retrying", path.display());
                std::thread::sleep(std::time::Duration::from_millis(50 * u64::from(attempt)));
            }
            Err(e) => {
                log::warn!("could not read {}: {e}", path.display());
                return None;
            }
        }
    }
    None
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = read_with_retry(path)?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("ignoring unreadable {}: {e}", path.display());
            None
        }
    }
}

/// Durable atomic write: the data goes to a temp file that is flushed to disk before it is
/// renamed over the target. Without the flush, a power cut or crash shortly after a save can
/// leave the target renamed but its content never written (a file full of zero bytes).
///
/// Every call uses its own temp file name, so two threads saving the same file at once (the UI
/// and the controller both save settings) can't trip over each other's half-written temp file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}-{n}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        rename_with_retry(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    if written.is_ok() {
        // The first save of each file in a run is logged (where things go, without flooding the log).
        static LOGGED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
        let mut logged = LOGGED.lock().unwrap_or_else(PoisonError::into_inner);
        if !logged.iter().any(|p| p == path) {
            logged.push(path.to_path_buf());
            log::info!("saved {}", path.display());
        }
    }
    written.with_context(|| format!("could not write {}", path.display()))
}

/// Renames `from` over `to`, retrying for about a second while Windows refuses (a virus scan or a
/// reader holding the file open). Saves in this copy of Truffle take turns, so two of them renaming
/// over the same file at once can't refuse each other.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 6;
    static RENAMING: Mutex<()> = Mutex::new(());
    let _turn = RENAMING.lock().unwrap_or_else(PoisonError::into_inner);
    let mut attempt = 1;
    loop {
        match std::fs::rename(from, to) {
            Err(e) if attempt < ATTEMPTS && is_busy(&e) => {
                log::debug!("{} is busy ({e}); retrying", to.display());
                std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Windows' "access denied", "in use by another process" and "locked" errors: another program has
/// the file open right now.
fn is_busy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(32 | 33))
}

/// [`write_atomic`] of a value as JSON.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    write_atomic(path, &bytes)
}

/// Move a file that can't be read out of the way (`<name>.corrupt-<unix seconds>`), so it is
/// kept for inspection instead of being overwritten. Returns the new path.
pub fn quarantine(path: &Path) -> Option<PathBuf> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut name = path.file_name()?.to_os_string();
    name.push(format!(".corrupt-{secs}"));
    let target = path.with_file_name(name);
    match std::fs::rename(path, &target) {
        Ok(()) => Some(target),
        Err(e) => {
            log::warn!("could not set {} aside: {e}", path.display());
            None
        }
    }
}

/// Temp files of a write that never finished are deleted once they are this old (younger ones
/// may belong to another copy of Truffle writing right now).
const STALE_TEMP_AGE: Duration = Duration::from_secs(60);

/// Delete `<name>.json.<pid>-<n>.tmp` files in `dir` left by an interrupted [`write_atomic`].
fn remove_stale_temp_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let own = format!(".json.{}-", std::process::id());
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".tmp") || !name.contains(".json.") || name.contains(&own) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_TEMP_AGE);
        if stale {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => log::info!("removed a leftover temp file {name}"),
                Err(e) => log::debug!("could not remove {name}: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("truffle-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths { config: root.join("config"), cache: root.join("cache") };
        std::fs::create_dir_all(&paths.config).unwrap();
        paths
    }

    #[test]
    fn one_bad_setting_keeps_all_the_others() {
        let paths = temp_paths("partial");
        std::fs::write(
            paths.settings_file(),
            br#"{"volume":"loud","shuffle":false,"lastfm_api_key":"k1","lastfm_api_secret":"s1","discord_app_id":"123"}"#,
        )
        .unwrap();
        let s = Settings::load(&paths);
        assert_eq!(s.volume, Settings::default().volume, "the bad value falls back to its default");
        assert!(!s.shuffle);
        assert_eq!(s.lastfm_api_key.as_deref(), Some("k1"));
        assert_eq!(s.lastfm_api_secret.as_deref(), Some("s1"));
        assert_eq!(s.discord_app_id.as_deref(), Some("123"));
        // The unreadable original is kept next to it.
        let kept = std::fs::read_dir(&paths.config)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("settings.json.unreadable-"));
        assert!(kept);
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn theme_modes_old_and_new_load() {
        for (json, mode) in [(r#""Dark""#, ThemeMode::Dark), (r#""Light""#, ThemeMode::Light), (r#""System""#, ThemeMode::System)] {
            assert_eq!(serde_json::from_str::<ThemeMode>(json).unwrap(), mode);
            assert_eq!(serde_json::to_string(&mode).unwrap(), json);
        }
        // A settings file from before "Same as Windows" keeps its choice.
        let old: Settings = serde_json::from_str(r#"{"theme":"Light","volume":40}"#).unwrap();
        assert_eq!((old.theme, old.volume), (ThemeMode::Light, 40));
    }

    #[test]
    fn wallpapers_saved_before_the_fit_choices_look_the_same() {
        let paths = temp_paths("wallpaper");
        // A file from before: the whole picture behind the whole window, at the left bottom, smaller.
        std::fs::write(
            paths.settings_file(),
            br#"{"volume":55,"wallpaper":{"path":"C:\\Pictures\\sakura.jpg","strength":25,"whole_window":true,"fit_whole":true,"at_x":0.0,"at_y":1.0,"zoom":-30}}"#,
        )
        .unwrap();
        let s = Settings::load(&paths);
        let w = &s.wallpaper;
        assert_eq!(s.volume, 55, "the rest of the file still reads");
        assert_eq!((w.path.as_str(), w.strength, w.whole_window), ("C:\\Pictures\\sakura.jpg", 25, true));
        assert_eq!((w.fit, w.at_x, w.at_y, w.zoom), (WallpaperFit::Whole, 0.0, 1.0, -30));
        // What's new changes nothing: no blur, no soft edges.
        assert_eq!((w.blur, w.soft_edges), (0, 0));
        // Filling, in a file from before, and in one without the switch at all.
        let fill: Wallpaper = serde_json::from_str(r#"{"path":"a.png","fit_whole":false,"at_x":0.5}"#).unwrap();
        assert_eq!((fill.fit, fill.at_x, fill.strength), (WallpaperFit::Fill, 0.5, 40));
        assert_eq!(serde_json::from_str::<Wallpaper>(r#"{"path":"a.png"}"#).unwrap().fit, WallpaperFit::Fill);

        // Saved again: the fit by name, and the old switch too, for an older Truffle.
        s.save(&paths).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(paths.settings_file()).unwrap()).unwrap();
        assert_eq!((saved["wallpaper"]["fit"].as_str(), saved["wallpaper"]["fit_whole"].as_bool()), (Some("Whole"), Some(true)));
        assert_eq!(saved["wallpaper"]["strength"].as_u64(), Some(25), "still saved as strength");
        assert_eq!(Settings::load(&paths).wallpaper, s.wallpaper, "read back the same");

        // A fit of its own wins over the old switch; numbers past their ends are kept to them.
        let tile: Wallpaper = serde_json::from_str(r#"{"fit":"Tile","fit_whole":true}"#).unwrap();
        assert_eq!(tile.fit, WallpaperFit::Tile);
        let wild: Wallpaper = serde_json::from_str(r#"{"strength":250,"zoom":900,"at_x":3.5,"at_y":-1,"blur":99,"soft_edges":80}"#).unwrap();
        assert_eq!((wild.strength, wild.zoom, wild.at_x, wild.at_y, wild.blur, wild.soft_edges), (100, 500, 1.0, 0.0, 50, 50));
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn close_to_tray_from_older_files_carries_over() {
        let paths = temp_paths("close-to-tray");
        for (json, keep_running) in [
            // It only acted while the tray icon showed: without the icon closing quit, and still does.
            (r#"{"close_to_tray":true,"tray_icon":false}"#, false),
            (r#"{"close_to_tray":true,"tray_icon":true}"#, true),
            (r#"{"close_to_tray":false,"tray_icon":true}"#, false),
            // A file saved since keeps its choice, also without the tray icon.
            (r#"{"keep_running":true,"tray_icon":false}"#, true),
        ] {
            std::fs::write(paths.settings_file(), json).unwrap();
            assert_eq!(Settings::load(&paths).keep_running, keep_running, "{json}");
        }
        // The old name is never saved again, so it is carried over only once.
        std::fs::write(paths.settings_file(), br#"{"close_to_tray":true,"tray_icon":true}"#).unwrap();
        Settings::load(&paths).save(&paths).unwrap();
        let saved = String::from_utf8(std::fs::read(paths.settings_file()).unwrap()).unwrap();
        assert!(!saved.contains("close_to_tray") && saved.contains(r#""keep_running":true"#));
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn the_friends_layout_is_card_in_older_files_and_kept() {
        // A file from before the layouts: Card, its links closed.
        let old = Settings::from_partial(br#"{"volume":40}"#);
        assert_eq!((old.friends_layout, old.friends_links_open, old.volume), (FriendsLayout::Card, false, 40));
        for layout in [FriendsLayout::Card, FriendsLayout::List, FriendsLayout::Switch] {
            let s = Settings { friends_layout: layout, friends_links_open: true, ..Settings::default() };
            let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(back, s);
        }
        // One this Truffle doesn't know (from a newer one): Card, and every other value kept.
        let newer = Settings::from_partial(br#"{"friends_layout":"Grid","friends_links_open":true}"#);
        assert_eq!((newer.friends_layout, newer.friends_links_open), (FriendsLayout::Card, true));
    }

    #[test]
    fn a_crossfade_page_animation_loads_as_rise() {
        let old = Settings::from_partial(br#"{"page_animation":"Crossfade"}"#);
        assert_eq!(old.page_animation, PageAnimation::Rise);
        assert_eq!(Settings::from_partial(br#"{"volume":40}"#).page_animation, PageAnimation::Rise);
        let top = Settings::from_partial(br#"{"page_animation":"TopFirst"}"#);
        assert_eq!(top.page_animation, PageAnimation::TopFirst);
    }

    #[test]
    fn the_motion_speed_is_normal_in_older_files_and_kept() {
        let old = Settings::from_partial(br#"{"volume":40}"#);
        assert_eq!((old.motion_speed, old.volume), (MotionSpeed::Normal, 40));
        for speed in [MotionSpeed::Snappy, MotionSpeed::Normal, MotionSpeed::Relaxed] {
            let s = Settings { motion_speed: speed, ..Settings::default() };
            let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn wide_screens_are_centred_in_older_files_and_kept() {
        let old = Settings::from_partial(br#"{"volume":40}"#);
        assert_eq!((old.wide_screens, old.volume), (WideScreens::Centred, 40));
        for wide in [WideScreens::Centred, WideScreens::Fill, WideScreens::Split] {
            let s = Settings { wide_screens: wide, ..Settings::default() };
            let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(back, s);
        }
        // One this Truffle doesn't know (from a newer one): Centred, and every other value kept.
        let newer = Settings::from_partial(br#"{"wide_screens":"Three","volume":40}"#);
        assert_eq!((newer.wide_screens, newer.volume), (WideScreens::Centred, 40));
    }

    #[test]
    fn sizes_per_screen_load_from_older_files_and_are_kept() {
        // A file from before sizes per screen: its size is kept for the first screen to take.
        let old: Settings = serde_json::from_str(r#"{"ui_scale":1.3,"volume":40}"#).unwrap();
        assert_eq!(old.ui_scale, 1.3);
        assert!(old.screen_sizes.is_empty() && old.big_screen_tips.is_empty() && !old.grow_with_window);
        let mut s = Settings { grow_with_window: true, ..Settings::default() };
        s.screen_sizes.insert(r"\\.\DISPLAY1 3840x2160".into(), 1.6);
        s.big_screen_tips.insert(r"\\.\DISPLAY1 3840x2160".into());
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn missing_or_garbage_settings_give_defaults() {
        let paths = temp_paths("garbage");
        assert_eq!(Settings::load(&paths), Settings::default());
        std::fs::write(paths.settings_file(), b"\0\0\0 not json").unwrap();
        assert_eq!(Settings::load(&paths), Settings::default());
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn concurrent_writes_to_one_file_all_succeed() {
        let dir = std::env::temp_dir().join(format!("truffle-config-test-{}", std::process::id()));
        let file = dir.join("settings.json");
        let writers: Vec<_> = (0..8u32)
            .map(|i| {
                let file = file.clone();
                std::thread::spawn(move || (0..25u32).map(|j| write_json(&file, &(i * 100 + j))).collect::<Vec<_>>())
            })
            .collect();
        for writer in writers {
            for result in writer.join().unwrap() {
                result.unwrap();
            }
        }
        // The file holds one complete value and no temp files are left behind.
        assert!(read_json::<u32>(&file).is_some());
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_files_are_set_aside_and_stale_temp_files_removed() {
        let dir = std::env::temp_dir().join(format!("truffle-config-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("state.json");
        std::fs::write(&file, [0u8; 64]).unwrap();
        let moved = quarantine(&file).unwrap();
        assert!(!file.exists());
        assert!(moved.file_name().unwrap().to_string_lossy().starts_with("state.json.corrupt-"));
        assert_eq!(std::fs::read(&moved).unwrap(), vec![0u8; 64]);

        // Someone else's temp file (another pid) that is old is removed; ours and others are kept.
        let foreign = dir.join("state.json.999999999-3.tmp");
        let own = dir.join(format!("state.json.{}-3.tmp", std::process::id()));
        let unrelated = dir.join("notes.tmp");
        for f in [&foreign, &own, &unrelated] {
            std::fs::write(f, b"x").unwrap();
        }
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&foreign).unwrap().set_modified(old).unwrap();
        remove_stale_temp_files(&dir);
        assert!(!foreign.exists());
        assert!(own.exists() && unrelated.exists() && moved.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_settings_are_saved_without_holding_their_lock() {
        let dir = std::env::temp_dir().join(format!("truffle-config-test3-{}", std::process::id()));
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        let shared = std::sync::Arc::new(Mutex::new(Settings::default()));
        let writers: Vec<_> = (0..4u8)
            .map(|i| {
                let (shared, paths) = (shared.clone(), paths.clone());
                std::thread::spawn(move || {
                    for v in 0..20u8 {
                        shared.lock().unwrap().volume = i * 20 + v;
                        Settings::save_shared(&shared, &paths).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        // The last snapshot taken is the one on disk.
        let on_disk: Settings = read_json(&paths.settings_file()).unwrap();
        assert_eq!(on_disk, *shared.lock().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
