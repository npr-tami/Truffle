//! Windows integration driven from the UI thread: media overlay + media keys (SMTC),
//! tray icon, Discord "Listening to" status, start-with-Windows and Start-menu shortcut.
//! The UI owns a `Platform`, feeds it playback state every frame
//! (cheap: it only acts on changes) and handles the `PlatformAction`s it returns.
//!
//! Threads: everything here runs on the UI thread except Discord (its own thread, see
//! `discord.rs`). Media-key presses arrive on a Windows thread-pool thread and tray clicks inside
//! the window message loop; both go through one channel and wake the UI with `request_repaint`.

mod discord;
mod media;
mod startup;
mod state;
mod tray;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, PoisonError};

use anyhow::Result;
use souvlaki::{MediaControlEvent, MediaPosition, SeekDirection};

use self::discord::{DiscordHandle, DiscordState, Presence, validate_app_id};
use self::media::Media;
use self::state::{Status, Tracker, seek_target, timestamps, unix_now_ms};
use self::tray::Tray;
use crate::config::Settings;
use crate::icon::IconStyle;
use crate::model::{Command, PlaybackState};

/// Something the user did through a platform surface that the UI must handle.
#[derive(Debug, Clone)]
pub enum PlatformAction {
    /// Forward to the controller (play/pause/next/previous/seek from media keys, overlay or tray).
    Command(Command),
    /// Bring the main window back (tray click / tray menu "Show Truffle").
    ShowWindow,
    /// Switch between the full window and the mini player.
    ToggleMiniPlayer,
    /// Quit for real (tray menu "Quit").
    Quit,
}

/// Events from the platform callbacks to `Platform::poll`.
enum Internal {
    Action(PlatformAction),
    /// Media keys / overlay buttons; mapped in `poll`, where the playback state is known.
    Media(MediaControlEvent),
}

/// Where the process-wide tray/menu handlers deliver events: the current `Platform`.
struct Sink {
    owner: u64,
    tx: Sender<Internal>,
    ctx: egui::Context,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn forward(event: Internal) {
    let sink = SINK.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(sink) = sink.as_ref()
        && sink.tx.send(event).is_ok()
    {
        sink.ctx.request_repaint();
    }
}

/// Seek step for the overlay's fast-forward / rewind buttons.
const SEEK_STEP_MS: i64 = 10_000;

enum DiscordSlot {
    Off,
    /// Switched on but can't run (no or bad application id, or the thread didn't start).
    Blocked(DiscordState),
    Running(DiscordHandle),
}

/// The settings `apply_settings` acts on, to skip work (and failed retries) when none changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Applied {
    media_controls: bool,
    tray_icon: bool,
    discord_status: bool,
    discord_app_id: Option<String>,
}

impl Applied {
    fn from(settings: &Settings) -> Self {
        Self {
            media_controls: settings.media_controls,
            tray_icon: settings.tray_icon,
            discord_status: settings.discord_status,
            discord_app_id: settings.discord_app_id.clone(),
        }
    }
}

/// Truffle's own Discord application (its name and picture show on the status). An application
/// ID is public; users only set their own to show a different name.
pub const TRUFFLE_DISCORD_APP_ID: &str = "1558921242042568854";

pub struct Platform {
    owner: u64,
    ctx: egui::Context,
    hwnd: Option<isize>,
    tx: Sender<Internal>,
    rx: Receiver<Internal>,
    tracker: Tracker,
    media: Option<Media>,
    tray: Option<Tray>,
    discord: DiscordSlot,
    applied: Option<Applied>,
    /// The tray icon's style (a new tray icon starts in it).
    icon_style: IconStyle,
}

impl Platform {
    /// `hwnd` is the main window handle (needed for the Windows media overlay).
    /// `ctx` is used to wake the UI when a platform event arrives. Never fails: features that
    /// cannot start are logged and left off.
    pub fn new(ctx: &egui::Context, hwnd: Option<isize>, settings: &Settings, runtime: tokio::runtime::Handle) -> Self {
        // Everything here is either synchronous on the UI thread or on its own std thread.
        drop(runtime);
        let (tx, rx) = channel();
        let owner = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
        *SINK.lock().unwrap_or_else(PoisonError::into_inner) = Some(Sink { owner, tx: tx.clone(), ctx: ctx.clone() });
        let mut platform = Self {
            owner,
            ctx: ctx.clone(),
            hwnd,
            tx,
            rx,
            tracker: Tracker::default(),
            media: None,
            tray: None,
            discord: DiscordSlot::Off,
            applied: None,
            icon_style: crate::icon::style_for(settings.app_icon, windows_taskbar_dark().unwrap_or(true)),
        };
        platform.apply_settings(settings);
        platform
    }

    /// Turn features on/off after a settings change (media controls, tray, Discord, app id).
    /// Cheap when none of those settings changed, so it may be called after every change.
    pub fn apply_settings(&mut self, settings: &Settings) {
        let wanted = Applied::from(settings);
        if self.applied.as_ref() == Some(&wanted) {
            return;
        }

        if wanted.media_controls && self.media.is_none() {
            self.media = self.start_media();
            if let Some(media) = self.media.as_mut() {
                media.set_track(self.tracker.track.as_deref());
                media.set_status(self.tracker.status, self.tracker.latest_position_ms);
                log::info!("media overlay and media keys on");
            }
        } else if !wanted.media_controls && self.media.take().is_some() {
            log::info!("media overlay and media keys off");
        }

        if wanted.tray_icon && self.tray.is_none() {
            match Tray::new(self.icon_style) {
                Ok(mut tray) => {
                    tray.update(self.tracker.track.as_deref(), self.tracker.status);
                    self.tray = Some(tray);
                    log::info!("tray icon on");
                }
                Err(e) => log::warn!("couldn't show the tray icon: {e}"),
            }
        } else if !wanted.tray_icon && self.tray.take().is_some() {
            log::info!("tray icon off");
        }

        self.apply_discord(wanted.discord_status, wanted.discord_app_id.as_deref());
        self.applied = Some(wanted);
    }

    /// Shows the tray icon in `style` (and starts a new one in it).
    pub fn set_icon_style(&mut self, style: IconStyle) {
        if style == self.icon_style {
            return;
        }
        self.icon_style = style;
        if let Some(tray) = self.tray.as_mut() {
            tray.set_style(style);
        }
    }

    /// Push the current playback state. Cheap to call every frame; only changes are sent on.
    pub fn update(&mut self, playback: &PlaybackState) {
        let changes = self.tracker.observe(playback, unix_now_ms());
        if !changes.any() {
            return;
        }
        let status = self.tracker.status;
        if let Some(media) = self.media.as_mut() {
            if changes.track {
                media.set_track(self.tracker.track.as_deref());
            }
            media.set_status(status, self.tracker.anchor_position_ms);
        }
        if (changes.track || changes.status)
            && let Some(tray) = self.tray.as_mut()
        {
            tray.update(self.tracker.track.as_deref(), status);
        }
        if let DiscordSlot::Running(discord) = &self.discord {
            discord.show(self.presence());
        }
    }

    /// Drain user actions that arrived since the last call.
    pub fn poll(&mut self) -> Vec<PlatformAction> {
        let mut actions = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Internal::Action(action) => actions.push(action),
                Internal::Media(event) => {
                    let duration = self.tracker.track.as_ref().map_or(0, |t| t.duration_ms);
                    let playing = self.tracker.status == Status::Playing;
                    if let Some(action) = media_action(event, playing, self.tracker.latest_position_ms, duration) {
                        actions.push(action);
                    }
                }
            }
        }
        actions
    }

    /// Plain-language status of the Discord connection for the settings screen
    /// (e.g. "Connected", "Discord isn't running", "Off").
    pub fn discord_status(&self) -> String {
        match &self.discord {
            DiscordSlot::Off => DiscordState::Off.describe(),
            DiscordSlot::Blocked(state) => state.describe(),
            DiscordSlot::Running(handle) => handle.state().describe(),
        }
        .to_owned()
    }

    /// Whether the tray icon is actually showing. The UI should only hide the window to the
    /// tray when this is true, or the user would have no way back.
    pub fn tray_available(&self) -> bool {
        self.tray.is_some()
    }

    fn start_media(&self) -> Option<Media> {
        let Some(hwnd) = self.hwnd.filter(|h| *h != 0) else {
            log::warn!("media overlay unavailable: no window handle");
            return None;
        };
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let on_event = move |event: MediaControlEvent| {
            if tx.send(Internal::Media(event)).is_ok() {
                ctx.request_repaint();
            }
        };
        match Media::new(hwnd, on_event) {
            Ok(media) => Some(media),
            Err(e) => {
                log::warn!("couldn't connect to the Windows media controls: {e}");
                None
            }
        }
    }

    /// `app_id`: the user's own Discord application, if they set one; else Truffle's.
    fn apply_discord(&mut self, enabled: bool, app_id: Option<&str>) {
        if !enabled {
            if matches!(self.discord, DiscordSlot::Running(_)) {
                log::info!("Discord status off");
            }
            self.discord = DiscordSlot::Off;
            return;
        }
        let app_id = app_id.map(str::trim).filter(|id| !id.is_empty()).unwrap_or(TRUFFLE_DISCORD_APP_ID);
        let id = match validate_app_id(app_id) {
            Ok(id) => id,
            Err(state) => {
                self.discord = DiscordSlot::Blocked(state);
                return;
            }
        };
        if let DiscordSlot::Running(handle) = &self.discord
            && handle.app_id() == id
        {
            return;
        }
        // Stop the old connection (it clears its status) before starting the new one.
        self.discord = DiscordSlot::Off;
        self.discord = match DiscordHandle::start(id, self.ctx.clone(), self.presence()) {
            Ok(handle) => DiscordSlot::Running(handle),
            Err(e) => {
                log::warn!("{e}");
                DiscordSlot::Blocked(DiscordState::Failed)
            }
        };
    }

    /// What Discord should show now: the song while playing, nothing when paused or stopped.
    fn presence(&self) -> Option<Presence> {
        if self.tracker.status != Status::Playing {
            return None;
        }
        let track = self.tracker.track.as_ref()?;
        let (start_ms, end_ms) = timestamps(self.tracker.anchor_started_unix_ms, track.duration_ms);
        Some(Presence {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            cover: track.cover.clone(),
            start_ms,
            end_ms,
        })
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        let mut sink = SINK.lock().unwrap_or_else(PoisonError::into_inner);
        if sink.as_ref().is_some_and(|s| s.owner == self.owner) {
            *sink = None;
        }
    }
}

/// Map a media key / overlay button to an action, given the current playback state.
fn media_action(event: MediaControlEvent, playing: bool, position_ms: u32, duration_ms: u32) -> Option<PlatformAction> {
    let command = match event {
        MediaControlEvent::Play => (!playing).then_some(Command::PlayPause)?,
        MediaControlEvent::Pause | MediaControlEvent::Stop => playing.then_some(Command::PlayPause)?,
        MediaControlEvent::Toggle => Command::PlayPause,
        MediaControlEvent::Next => Command::Next,
        MediaControlEvent::Previous => Command::Previous,
        MediaControlEvent::Seek(direction) => {
            Command::SeekMs(seek_target(position_ms, signed(direction, SEEK_STEP_MS), duration_ms))
        }
        MediaControlEvent::SeekBy(direction, by) => {
            let by = i64::try_from(by.as_millis()).unwrap_or(i64::MAX);
            Command::SeekMs(seek_target(position_ms, signed(direction, by), duration_ms))
        }
        MediaControlEvent::SetPosition(MediaPosition(at)) => {
            let at = i64::try_from(at.as_millis()).unwrap_or(i64::MAX);
            Command::SeekMs(seek_target(0, at, duration_ms))
        }
        MediaControlEvent::SetVolume(volume) if volume.is_finite() => {
            Command::SetVolume((volume.clamp(0.0, 1.0) * 100.0).round() as u8)
        }
        MediaControlEvent::Raise => return Some(PlatformAction::ShowWindow),
        // Not sent on Windows; never let an outside surface quit or open things.
        MediaControlEvent::SetVolume(_) | MediaControlEvent::OpenUri(_) | MediaControlEvent::Quit => return None,
    };
    Some(PlatformAction::Command(command))
}

fn signed(direction: SeekDirection, amount: i64) -> i64 {
    match direction {
        SeekDirection::Forward => amount,
        SeekDirection::Backward => -amount,
    }
}

/// Add/remove Truffle from HKCU\Software\Microsoft\Windows\CurrentVersion\Run (starts with
/// `--minimized`, which the app treats as "start hidden in the tray").
pub fn set_start_with_windows(enabled: bool) -> Result<()> {
    startup::set_start_with_windows(enabled)
}

/// Whether the Run entry currently exists and points at this exe.
pub fn starts_with_windows() -> bool {
    startup::starts_with_windows()
}

/// Create (or refresh) a Start-menu shortcut to this exe. Returns the .lnk path.
pub fn create_start_menu_shortcut() -> Result<std::path::PathBuf> {
    startup::create_start_menu_shortcut()
}

/// Whether Windows' taskbar (and Start) is dark (Settings > Personalisation > Colours > "Choose
/// your Windows mode"), which can differ from the apps' mode. `None` when Windows doesn't say.
pub fn windows_taskbar_dark() -> Option<bool> {
    match startup::registry::get_dword(PERSONALIZE_KEY, "SystemUsesLightTheme") {
        Ok(light) => light.map(|v| v == 0),
        Err(e) => {
            log::debug!("could not read Windows' taskbar mode: {e}");
            None
        }
    }
}

/// Where Windows keeps its light or dark mode for apps.
const PERSONALIZE_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// Whether Windows shows apps dark (Settings > Personalisation > Colours > "Choose your app
/// mode"). A quick read of the user's own registry. `None` when Windows doesn't say (older than
/// Windows 10 1809).
pub fn windows_apps_dark() -> Option<bool> {
    match startup::registry::get_dword(PERSONALIZE_KEY, "AppsUseLightTheme") {
        Ok(light) => light.map(|v| v == 0),
        Err(e) => {
            log::debug!("could not read Windows' app mode: {e}");
            None
        }
    }
}

/// Where Windows keeps its own version.
const VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// Windows' version, for Settings > About's details ("Windows 11 25H2 (build 26200.6899)"): a quick
/// read of the registry.
pub fn windows_version() -> String {
    let read = |name: &str| startup::registry::machine_string(VERSION_KEY, name).ok().flatten();
    let build = read("CurrentBuildNumber").and_then(|b| b.trim().parse().ok());
    let release = read("DisplayVersion").or_else(|| read("ReleaseId"));
    let update = startup::registry::machine_dword(VERSION_KEY, "UBR").ok().flatten();
    windows_name(build, release.as_deref(), update)
}

/// "Windows 11 25H2 (build 26200.6899)" from the build number, the release ("25H2") and the
/// update number, each when known. Windows 11 still calls itself Windows 10 in the registry's
/// ProductName; its builds start at 22000.
fn windows_name(build: Option<u32>, release: Option<&str>, update: Option<u32>) -> String {
    let Some(build) = build else { return "Windows".to_owned() };
    let mut name = if build >= 22_000 { "Windows 11".to_owned() } else { "Windows 10".to_owned() };
    if let Some(release) = release.map(str::trim).filter(|r| !r.is_empty()) {
        name = format!("{name} {release}");
    }
    match update {
        Some(update) => format!("{name} (build {build}.{update})"),
        None => format!("{name} (build {build})"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn windows_is_named_by_its_build() {
        assert_eq!(windows_name(Some(26_200), Some("25H2"), Some(6899)), "Windows 11 25H2 (build 26200.6899)");
        assert_eq!(windows_name(Some(19_045), Some("22H2"), None), "Windows 10 22H2 (build 19045)");
        assert_eq!(windows_name(Some(22_000), None, Some(1)), "Windows 11 (build 22000.1)");
        assert_eq!(windows_name(None, Some("25H2"), Some(1)), "Windows");
        assert!(windows_version().starts_with("Windows"));
    }

    fn command(action: Option<PlatformAction>) -> Option<Command> {
        match action {
            Some(PlatformAction::Command(c)) => Some(c),
            _ => None,
        }
    }

    #[test]
    fn media_keys_respect_the_current_state() {
        assert!(matches!(command(media_action(MediaControlEvent::Play, false, 0, 0)), Some(Command::PlayPause)));
        assert!(media_action(MediaControlEvent::Play, true, 0, 0).is_none());
        assert!(matches!(command(media_action(MediaControlEvent::Pause, true, 0, 0)), Some(Command::PlayPause)));
        assert!(media_action(MediaControlEvent::Pause, false, 0, 0).is_none());
        assert!(media_action(MediaControlEvent::Stop, false, 0, 0).is_none());
        assert!(matches!(command(media_action(MediaControlEvent::Toggle, true, 0, 0)), Some(Command::PlayPause)));
        assert!(matches!(command(media_action(MediaControlEvent::Next, true, 0, 0)), Some(Command::Next)));
        assert!(matches!(command(media_action(MediaControlEvent::Previous, true, 0, 0)), Some(Command::Previous)));
        assert!(matches!(media_action(MediaControlEvent::Raise, true, 0, 0), Some(PlatformAction::ShowWindow)));
        assert!(media_action(MediaControlEvent::Quit, true, 0, 0).is_none());
    }

    #[test]
    fn media_seeks_are_clamped_to_the_track() {
        let seek = |e| match command(media_action(e, true, 5_000, 60_000)) {
            Some(Command::SeekMs(ms)) => ms,
            other => panic!("{other:?}"),
        };
        assert_eq!(seek(MediaControlEvent::Seek(SeekDirection::Forward)), 15_000);
        assert_eq!(seek(MediaControlEvent::Seek(SeekDirection::Backward)), 0);
        assert_eq!(seek(MediaControlEvent::SeekBy(SeekDirection::Forward, Duration::from_secs(90))), 60_000);
        assert_eq!(seek(MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs(42)))), 42_000);
        assert!(matches!(
            command(media_action(MediaControlEvent::SetVolume(0.5), true, 0, 0)),
            Some(Command::SetVolume(50))
        ));
        assert!(media_action(MediaControlEvent::SetVolume(f64::NAN), true, 0, 0).is_none());
    }
}
