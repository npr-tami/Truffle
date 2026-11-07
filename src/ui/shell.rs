//! The window's life outside the UI proper: Windows integration (media overlay, tray, Discord)
//! through [`Platform`], keeping running in the background, starting hidden, following Windows'
//! light or dark mode, the mini player's window geometry, and the Windows-setting jobs (start with
//! Windows, Start-menu shortcut) run off the UI thread. Hiding the window, the mini player and
//! quitting close Settings (its own window too).
//!
//! Everything here also runs while the window is hidden or minimized, from `App::logic`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use egui::{ViewportCommand, WindowLevel};

use super::window::{
    FULL_MIN_SIZE, MINI_SIZE, STAGE_MIN_SIZE, STAGE_SIZE, full_size_or_default, moved, plausible_position, point_on_a_monitor,
    stage_size_or_default,
};
use super::{AuthState, TruffleApp, theme};
use crate::config::{AppIcon, ThemeMode};
use crate::icon::{self, IconStyle};
use crate::platform::{self, Platform, PlatformAction};

/// Command-line flag used by the "start with Windows" entry: start hidden in the tray.
pub const START_MINIMIZED_FLAG: &str = "--minimized";
/// Frames to wait after a window change before trusting the reported geometry again.
const SETTLE_FRAMES: u8 = 3;
/// Tray "Mini player" on a minimized window waits at most this many passes for it to be restored.
const RESTORE_WAIT_PASSES: u8 = 60;
/// "Same as Windows" reads Windows' mode again at most this often while frames are drawn anyway.
const WINDOWS_MODE_RECHECK: Duration = Duration::from_secs(3);

/// Results of Windows-setting jobs that run on the blocking pool.
pub enum ShellJob {
    /// Whether the Run entry exists (checked once at startup).
    StartsWithWindows(bool),
    StartWithWindowsSet { enabled: bool, result: Result<(), String> },
    Shortcut(Result<PathBuf, String>),
}

/// The full window's geometry, kept while the mini player is shown.
#[derive(Clone, Copy, Debug)]
pub(super) struct FullGeometry {
    /// Outer top-left corner, physical pixels.
    pos: Option<[f32; 2]>,
    /// Inner size, logical pixels.
    size: [f32; 2],
    maximized: bool,
}

pub struct Shell {
    pub platform: Platform,
    /// Hidden in the tray (by us). eframe keeps calling `ui` for a hidden window, so the UI skips
    /// drawing while this is set.
    pub hidden: bool,
    pub mini: bool,
    /// "Quit" (the tray's) was chosen: the next close request is not turned into hiding.
    quitting: bool,
    started: bool,
    start_minimized: bool,
    full: Option<FullGeometry>,
    /// The window as it was before the Now playing view gave it its own size: where it goes back to.
    before_stage: Option<FullGeometry>,
    /// The window switched between full and mini during this pass: it still has the old size
    /// until the pass ends, so nothing is laid out in it (panels would store sizes clamped to it).
    pub switched: bool,
    /// Tray "Mini player" chosen while the window was minimized: switch once it is restored (its
    /// full-size geometry can't be read before), within this many more passes.
    pending_mini: Option<u8>,
    /// The zoom the mini player's size was last set at (it is given in points).
    mini_zoom: f32,
    /// Frames left before window geometry is tracked again (after a size/mode change).
    settle: u8,
    /// Ask for focus once more on the next frame (showing a window and focusing it in the same
    /// frame is not always honoured).
    focus_again: bool,
    /// Explain once per session, when the window comes back, that closing kept Truffle running.
    background_hint: BackgroundHint,
    /// "Same as Windows": when Windows' mode was last read, the theme winit last reported and
    /// whether the window had the focus (a change in either is a reason to read it again at once).
    mode_read: Option<Instant>,
    reported_theme: Option<egui::Theme>,
    focused: bool,
    /// The app icon's style on show, and for "Automatic", when the taskbar's mode was last read
    /// and what it was.
    pub icon_shown: Option<IconStyle>,
    taskbar_read: Option<Instant>,
    taskbar_dark: bool,
    pub jobs_tx: Sender<ShellJob>,
    jobs_rx: Receiver<ShellJob>,
    /// The "start with Windows" state being applied right now.
    pub start_with_windows_pending: Option<bool>,
    pub shortcut_busy: bool,
    /// The main window's handle, once found (for its placement).
    hwnd: Option<isize>,
    /// It was maximized when Truffle last closed: maximized again once it is first shown.
    pub maximize_when_shown: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BackgroundHint {
    NotYet,
    Pending,
    Shown,
}

impl Shell {
    pub fn new(platform: Platform) -> Self {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
        Self {
            platform,
            hidden: false,
            mini: false,
            quitting: false,
            started: false,
            start_minimized: std::env::args().any(|a| a == START_MINIMIZED_FLAG),
            full: None,
            before_stage: None,
            switched: false,
            pending_mini: None,
            mini_zoom: 1.0,
            settle: SETTLE_FRAMES,
            focus_again: false,
            background_hint: BackgroundHint::NotYet,
            mode_read: None,
            icon_shown: None,
            taskbar_read: None,
            taskbar_dark: true,
            reported_theme: None,
            focused: false,
            jobs_tx,
            jobs_rx,
            start_with_windows_pending: None,
            shortcut_busy: false,
            hwnd: None,
            maximize_when_shown: false,
        }
    }
}

fn root_cmd(ctx: &egui::Context, cmd: ViewportCommand) {
    ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, cmd);
}

impl TruffleApp {
    /// Creates the platform integration for the main window.
    pub(super) fn create_platform(ctx: &egui::Context, settings: &crate::config::Settings, runtime: tokio::runtime::Handle) -> Platform {
        let hwnd = super::window::main_window_handle();
        if hwnd.is_none() {
            log::warn!("main window handle not found; the Windows media overlay may be unavailable");
        }
        Platform::new(ctx, hwnd, settings, runtime)
    }

    /// Runs every frame from `App::logic` (also while hidden or minimized).
    pub(super) fn shell_logic(&mut self, ctx: &egui::Context) {
        if !self.shell.started {
            self.shell.started = true;
            self.check_start_with_windows();
            if self.shell.start_minimized {
                if self.tray_shown() {
                    log::info!("starting hidden in the tray");
                    self.hide_window(ctx, false);
                } else {
                    // `main.rs` already hid the window if the tray icon is switched on; if the
                    // icon then couldn't be shown, bring the window back as a taskbar button,
                    // or there would be no way to reach it.
                    root_cmd(ctx, ViewportCommand::Visible(true));
                    root_cmd(ctx, ViewportCommand::Minimized(true));
                }
            }
        }
        if crate::take_activation_request() {
            // Another launch asked this one to come to the front (the window was told to show
            // itself already): leave the tray-hidden state too.
            self.show_window(ctx);
        }
        self.handle_shell_jobs();
        self.follow_windows_mode(ctx);
        self.follow_icon_style(ctx);
        self.shell.platform.update(&self.playback);
        for action in self.shell.platform.poll() {
            self.handle_platform_action(ctx, action);
        }
        if self.shell.mini && self.auth != AuthState::LoggedIn {
            // Logged out or the login expired: the login screen doesn't fit in the mini player
            // (and it has no way back of its own then).
            self.leave_mini(ctx);
        }
        if self.stage.open && self.shell.before_stage.is_some() && self.auth != AuthState::LoggedIn && !self.shot_flag("stage-window") {
            // The same for the Now playing view's window (pictures open the view without one).
            self.close_stage(ctx);
        }
        self.enter_mini_when_restored(ctx);
        self.handle_close_request(ctx);
    }

    fn handle_platform_action(&mut self, ctx: &egui::Context, action: PlatformAction) {
        match action {
            PlatformAction::Command(command) => self.send(command),
            PlatformAction::ShowWindow => self.show_window(ctx),
            PlatformAction::ToggleMiniPlayer => {
                // A minimized window reports no position and not whether it is maximized, so
                // the full window couldn't be put back as it was: switch once it is restored.
                let minimized = ctx.input(|i| {
                    let v = i.viewport();
                    v.minimized == Some(true) || v.outer_rect.is_none()
                });
                self.show_window(ctx);
                if !self.shell.mini && minimized {
                    if self.auth == AuthState::LoggedIn {
                        self.shell.pending_mini = Some(RESTORE_WAIT_PASSES);
                    }
                } else {
                    self.toggle_mini(ctx);
                }
            }
            PlatformAction::Quit => self.quit(ctx),
        }
    }

    /// Finishes a tray "Mini player" that came while the window was minimized.
    fn enter_mini_when_restored(&mut self, ctx: &egui::Context) {
        let Some(left) = self.shell.pending_mini else { return };
        if self.shell.mini || self.shell.hidden || self.auth != AuthState::LoggedIn {
            self.shell.pending_mini = None;
            return;
        }
        let restored = ctx.input(|i| {
            let v = i.viewport();
            v.minimized != Some(true) && v.outer_rect.is_some()
        });
        // `settle` counts drawn frames since `show_window`: by then the geometry is current.
        if (restored && self.shell.settle == 0) || left == 0 {
            self.shell.pending_mini = None;
            self.enter_mini(ctx);
        } else {
            self.shell.pending_mini = Some(left - 1);
            ctx.request_repaint();
        }
    }

    /// Quits for real, even with "Keep running in the background" on (the tray's Quit).
    pub(super) fn quit(&mut self, ctx: &egui::Context) {
        // Settings' own window goes first (it is owned by the main window).
        if self.settings_open {
            self.close_settings();
        }
        self.shell.quitting = true;
        root_cmd(ctx, ViewportCommand::Close);
        ctx.request_repaint();
    }

    /// Closing hides the window ("Keep running in the background"): only when asked for and there
    /// is a way back to it, the tray icon or opening Truffle again (`main.rs` then shows this one).
    pub(super) fn keeps_running(&self) -> bool {
        self.prefs.keep_running && (self.tray_shown() || crate::relaunch_shows_window())
    }

    /// Whether the tray icon is on and actually showing.
    pub(super) fn tray_shown(&self) -> bool {
        self.prefs.tray_icon && self.shell.platform.tray_available()
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.shell.quitting || !self.keeps_running() {
            log::info!("closing Truffle");
            self.remember_mini_position(ctx);
            return;
        }
        root_cmd(ctx, ViewportCommand::CancelClose);
        self.hide_window(ctx, true);
    }

    /// Hides the window: no window and no taskbar button. `hint`: tell the user about it once they
    /// come back.
    fn hide_window(&mut self, ctx: &egui::Context, hint: bool) {
        // Settings closes with the window, as in Glint (its own window would stay on screen).
        if self.settings_open {
            self.close_settings();
        }
        self.remember_mini_position(ctx);
        root_cmd(ctx, ViewportCommand::Visible(false));
        self.shell.hidden = true;
        if hint && self.shell.background_hint == BackgroundHint::NotYet {
            self.shell.background_hint = BackgroundHint::Pending;
        }
    }

    pub(super) fn show_window(&mut self, ctx: &egui::Context) {
        root_cmd(ctx, ViewportCommand::Visible(true));
        // Sent unconditionally: winit only acts on a change, so a maximized window stays
        // maximized, and the viewport info can lag behind for a window that was never painted
        // (the hidden start).
        root_cmd(ctx, ViewportCommand::Minimized(false));
        root_cmd(ctx, ViewportCommand::Focus);
        self.shell.hidden = false;
        self.shell.focus_again = true;
        self.shell.settle = SETTLE_FRAMES;
        if self.shell.background_hint == BackgroundHint::Pending {
            self.shell.background_hint = BackgroundHint::Shown;
            let quit = if self.tray_shown() { " (Quit is in the tray icon's menu)" } else { "" };
            self.toast(format!("Truffle kept running in the background{quit}"), false);
        }
        ctx.request_repaint();
    }

    /// Switches between the full window and the mini player.
    pub(super) fn toggle_mini(&mut self, ctx: &egui::Context) {
        if self.shell.mini {
            self.leave_mini(ctx);
        } else if self.auth == AuthState::LoggedIn {
            self.enter_mini(ctx);
        }
    }

    fn enter_mini(&mut self, ctx: &egui::Context) {
        // Settings belongs to the full window (docked beside it, or inside it).
        if self.settings_open {
            self.close_settings();
        }
        // Out of the Now playing view too: the window it came from is the one to come back to. A
        // fullscreen window (its F11) has no size of its own to come back to: it comes back as a
        // maximised one does, at the saved size.
        let before_stage = if self.stage.open { self.drop_stage() } else { None };
        let zoom = ctx.zoom_factor();
        let (info, ppp) = ctx.input(|i| (i.viewport().clone(), i.pixels_per_point()));
        let maximized = info.maximized.unwrap_or(false);
        let fullscreen = info.fullscreen.unwrap_or(false);
        let size = match info.inner_rect {
            Some(r) if !maximized && !fullscreen => [r.width() * zoom, r.height() * zoom],
            _ => full_size_or_default(self.prefs.main_window_size).into(),
        };
        let pos = info.outer_rect.filter(|_| !maximized && !fullscreen).map(|r| [r.min.x * ppp, r.min.y * ppp]);
        self.shell.full = Some(before_stage.unwrap_or(FullGeometry { pos, size, maximized }));

        if fullscreen {
            root_cmd(ctx, ViewportCommand::Fullscreen(false));
        }
        if maximized {
            root_cmd(ctx, ViewportCommand::Maximized(false));
        }
        root_cmd(ctx, ViewportCommand::Decorations(false));
        root_cmd(ctx, ViewportCommand::Resizable(false));
        root_cmd(ctx, ViewportCommand::MinInnerSize(MINI_SIZE));
        root_cmd(ctx, ViewportCommand::InnerSize(MINI_SIZE));
        root_cmd(ctx, ViewportCommand::WindowLevel(self.mini_window_level()));
        if let Some([x, y]) = plausible_position(self.prefs.mini_player_pos)
            && point_on_a_monitor(x + 24.0, y + 12.0)
            && ppp > 0.0
        {
            root_cmd(ctx, ViewportCommand::OuterPosition(egui::pos2(x / ppp, y / ppp)));
        }
        self.shell.mini = true;
        self.shell.mini_zoom = zoom;
        self.shell.switched = true;
        self.shell.settle = SETTLE_FRAMES;
        log::debug!("mini player on");
        ctx.request_repaint();
    }

    /// The mini player's size is in points, so it follows the interface size: after a zoom
    /// change (Ctrl +/-, or Settings) it is set again.
    pub(super) fn keep_mini_size(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        if !self.shell.mini || (zoom - self.shell.mini_zoom).abs() < 0.001 {
            return;
        }
        self.shell.mini_zoom = zoom;
        root_cmd(ctx, ViewportCommand::MinInnerSize(MINI_SIZE));
        root_cmd(ctx, ViewportCommand::InnerSize(MINI_SIZE));
        self.shell.switched = true;
        self.shell.settle = SETTLE_FRAMES;
        ctx.request_repaint();
    }

    fn leave_mini(&mut self, ctx: &egui::Context) {
        self.remember_mini_position(ctx);
        let ppp = ctx.input(|i| i.pixels_per_point());
        let zoom = ctx.zoom_factor();
        let zoom = if zoom > 0.0 { zoom } else { 1.0 };
        let full = self.shell.full.take();
        let size = full.map_or_else(|| full_size_or_default(self.prefs.main_window_size), |f| full_size_or_default(Some(f.size)));

        root_cmd(ctx, ViewportCommand::WindowLevel(WindowLevel::Normal));
        // Truffle draws its own title bar; the full window has no Windows frame either.
        root_cmd(ctx, ViewportCommand::Decorations(false));
        root_cmd(ctx, ViewportCommand::Resizable(true));
        root_cmd(ctx, ViewportCommand::MinInnerSize(FULL_MIN_SIZE / zoom));
        root_cmd(ctx, ViewportCommand::InnerSize(size / zoom));
        if let Some([x, y]) = full.and_then(|f| plausible_position(f.pos))
            && point_on_a_monitor(x + 24.0, y + 12.0)
            && ppp > 0.0
        {
            root_cmd(ctx, ViewportCommand::OuterPosition(egui::pos2(x / ppp, y / ppp)));
        }
        if full.is_some_and(|f| f.maximized) {
            root_cmd(ctx, ViewportCommand::Maximized(true));
        }
        self.shell.mini = false;
        self.shell.switched = true;
        self.shell.settle = SETTLE_FRAMES;
        log::debug!("mini player off");
        ctx.request_repaint();
    }

    pub(super) fn mini_window_level(&self) -> WindowLevel {
        if self.prefs.mini_player_on_top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal }
    }

    /// Keeps "always on top" in line with the setting while the mini player is shown.
    pub(super) fn set_mini_on_top(&mut self, ctx: &egui::Context, on_top: bool) {
        if self.prefs.mini_player_on_top != on_top {
            self.prefs.mini_player_on_top = on_top;
            self.prefs_changed();
        }
        if self.shell.mini {
            root_cmd(ctx, ViewportCommand::WindowLevel(self.mini_window_level()));
        }
    }

    fn remember_mini_position(&mut self, ctx: &egui::Context) {
        // Right after a switch the reported position may still be the full window's.
        if !self.shell.mini || self.shell.settle > 0 {
            return;
        }
        let (outer, ppp) = ctx.input(|i| (i.viewport().outer_rect, i.pixels_per_point()));
        if let Some(r) = outer {
            let pos = [r.min.x * ppp, r.min.y * ppp];
            if plausible_position(Some(pos)).is_some() && moved(self.prefs.mini_player_pos, pos) {
                self.prefs.mini_player_pos = Some(pos);
                self.prefs_changed();
            }
        }
    }

    /// Runs at the start of every drawn frame: re-focuses after showing, and remembers the full
    /// window's size and the mini player's position as the user changes them.
    pub(super) fn track_window(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.shell.focus_again) {
            root_cmd(ctx, ViewportCommand::Focus);
        }
        if self.shell.settle > 0 {
            self.shell.settle -= 1;
            return;
        }
        if self.shell.mini {
            self.remember_mini_position(ctx);
            return;
        }
        if self.stage.open && self.shell.before_stage.is_some() {
            self.remember_stage_size(ctx);
            return;
        }
        self.remember_placement(ctx);
        let zoom = ctx.zoom_factor();
        let (normal, inner) = ctx.input(|i| {
            let v = i.viewport();
            (v.maximized == Some(false) && v.minimized != Some(true) && v.fullscreen != Some(true), v.inner_rect)
        });
        if let (true, Some(r)) = (normal, inner) {
            let size = [(r.width() * zoom).round(), (r.height() * zoom).round()];
            let plausible = size[0] >= FULL_MIN_SIZE.x - 1.0 && size[1] >= FULL_MIN_SIZE.y - 1.0;
            if plausible && moved(self.prefs.main_window_size, size) {
                self.prefs.main_window_size = Some(size);
                self.prefs_changed();
            }
        }
    }

    /// The Now playing view opens in a window of its own: 16:10, at [`STAGE_SIZE`] the first time and
    /// then as it was left, centred where the window was. Only its corners size it, and it keeps its
    /// shape ([`super::window::keep_shape`]). The window as it was is kept to go back to.
    pub(super) fn open_stage_window(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        let (info, ppp) = ctx.input(|i| (i.viewport().clone(), i.pixels_per_point()));
        let maximized = info.maximized.unwrap_or(false);
        let fullscreen = info.fullscreen.unwrap_or(false);
        let size = match info.inner_rect {
            Some(r) if !maximized && !fullscreen => [r.width() * zoom, r.height() * zoom],
            _ => full_size_or_default(self.prefs.main_window_size).into(),
        };
        // Maximized (or fullscreen), its normal place is Windows' record of it: the window goes back
        // there before it is maximized again, so un-maximizing it later still works as before.
        let normal = if maximized || fullscreen { self.main_hwnd().and_then(super::window::placement).map(|(r, _)| r) } else { None };
        let pos = match normal {
            Some(r) => Some([r.left as f32, r.top as f32]),
            None => info.outer_rect.filter(|_| !maximized && !fullscreen).map(|r| [r.min.x * ppp, r.min.y * ppp]),
        };
        self.shell.before_stage = Some(FullGeometry { pos, size, maximized });

        if fullscreen {
            root_cmd(ctx, ViewportCommand::Fullscreen(false));
        }
        if maximized {
            root_cmd(ctx, ViewportCommand::Maximized(false));
        }
        let view = stage_size_or_default(self.prefs.stage_window_size, info.monitor_size.map(|m| m * zoom));
        root_cmd(ctx, ViewportCommand::MinInnerSize(STAGE_MIN_SIZE / zoom));
        root_cmd(ctx, ViewportCommand::InnerSize(view / zoom));
        // Centred over the window as it was, and on its screen.
        let native = if zoom > 0.0 { ppp / zoom } else { ppp };
        let hwnd = self.main_hwnd();
        let screen = hwnd.and_then(|h| Some((super::window::visible_rect(h)?, super::window::work_area(h)?)));
        if let (Some((was, work)), true) = (screen, ppp > 0.0) {
            let r = super::window::centred_over(was, work, (view.x * native).round() as i32, (view.y * native).round() as i32);
            root_cmd(ctx, ViewportCommand::OuterPosition(egui::pos2(r.left as f32 / ppp, r.top as f32 / ppp)));
        } else if let Some(r) = info.outer_rect {
            root_cmd(ctx, ViewportCommand::OuterPosition(r.center() - view / zoom / 2.0));
        }
        if let Some(hwnd) = hwnd {
            super::window::keep_shape(hwnd, Some(STAGE_SIZE.x / STAGE_SIZE.y), STAGE_MIN_SIZE.x as i32);
        }
        log::info!("Now playing window: {} x {} (16:10), from {:?}{}", view.x, view.y, size, if maximized { ", maximized" } else { "" });
        self.shell.settle = SETTLE_FRAMES;
        ctx.request_repaint();
    }

    /// Out of the Now playing view: its size is kept for next time and the window goes back as it was
    /// (maximized again if it was).
    pub(super) fn close_stage_window(&mut self, ctx: &egui::Context) {
        self.let_stage_shape_go();
        // A view without a window of its own (pictures) has nothing to put back.
        let Some(back) = self.shell.before_stage else { return };
        self.remember_stage_size(ctx);
        self.shell.before_stage = None;
        let zoom = ctx.zoom_factor();
        let (info, ppp) = ctx.input(|i| (i.viewport().clone(), i.pixels_per_point()));
        let zoom = if zoom > 0.0 { zoom } else { 1.0 };
        if info.fullscreen == Some(true) {
            root_cmd(ctx, ViewportCommand::Fullscreen(false));
        }
        // Out of a maximized view first: the size and place below are the normal ones.
        if info.maximized == Some(true) {
            root_cmd(ctx, ViewportCommand::Maximized(false));
        }
        root_cmd(ctx, ViewportCommand::MinInnerSize(FULL_MIN_SIZE / zoom));
        root_cmd(ctx, ViewportCommand::InnerSize(full_size_or_default(Some(back.size)) / zoom));
        if let Some([x, y]) = plausible_position(back.pos)
            && point_on_a_monitor(x + 24.0, y + 12.0)
            && ppp > 0.0
        {
            root_cmd(ctx, ViewportCommand::OuterPosition(egui::pos2(x / ppp, y / ppp)));
        }
        if back.maximized {
            root_cmd(ctx, ViewportCommand::Maximized(true));
        }
        log::info!("Now playing window closed: back to {:?}{}", back.size, if back.maximized { ", maximized" } else { "" });
        self.shell.settle = SETTLE_FRAMES;
        ctx.request_repaint();
    }

    /// How wide (points) the window is once the Now playing view gives it back, if the view has its
    /// own window: keys that close the view act on that width, not the view's.
    pub(super) fn stage_return_width(&self, ctx: &egui::Context) -> Option<f32> {
        let back = self.shell.before_stage?;
        let zoom = ctx.zoom_factor();
        let zoom = if zoom > 0.0 { zoom } else { 1.0 };
        if back.maximized {
            ctx.input(|i| i.viewport().monitor_size).map(|m| m.x)
        } else {
            Some(full_size_or_default(Some(back.size)).x / zoom)
        }
    }

    /// The window the Now playing view came from, the view now going without putting it back (the mini
    /// player takes over), its shape let go.
    pub(super) fn take_stage_window(&mut self) -> Option<FullGeometry> {
        self.let_stage_shape_go();
        self.shell.before_stage.take()
    }

    fn let_stage_shape_go(&mut self) {
        if let Some(hwnd) = self.main_hwnd() {
            super::window::keep_shape(hwnd, None, 0);
        }
    }

    /// The Now playing view's window size, as the user sizes it (not maximized, fullscreen, minimized or
    /// snapped to a side of the screen out of its shape).
    fn remember_stage_size(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        let (normal, inner) = ctx.input(|i| {
            let v = i.viewport();
            (v.maximized != Some(true) && v.minimized != Some(true) && v.fullscreen != Some(true), v.inner_rect)
        });
        if let (true, Some(r)) = (normal, inner) {
            let size = [(r.width() * zoom).round(), (r.height() * zoom).round()];
            let shaped = (size[1] - size[0] * STAGE_SIZE.y / STAGE_SIZE.x).abs() <= 2.0;
            if shaped && size[0] >= STAGE_MIN_SIZE.x - 1.0 && moved(self.prefs.stage_window_size, size) {
                self.prefs.stage_window_size = Some(size);
                self.prefs_changed();
            }
        }
    }

    /// Remembers where the full window goes back to and whether it is maximized (Windows' own
    /// record of it, so it opens on the same screen); maximizes it once if it closed maximized.
    fn remember_placement(&mut self, ctx: &egui::Context) {
        let visible = ctx.input(|i| i.viewport().minimized != Some(true));
        if std::mem::take(&mut self.shell.maximize_when_shown) {
            if visible {
                log::info!("maximized again, as when Truffle closed");
                root_cmd(ctx, ViewportCommand::Maximized(true));
                self.shell.settle = SETTLE_FRAMES;
                return;
            }
            self.shell.maximize_when_shown = true;
        }
        let Some((normal, maximized)) = self.main_hwnd().and_then(super::window::placement) else { return };
        let rect = [normal.left, normal.top, normal.right, normal.bottom];
        if self.prefs.main_window_rect != Some(rect) || self.prefs.main_window_maximized != maximized {
            self.prefs.main_window_rect = Some(rect);
            self.prefs.main_window_maximized = maximized;
            self.prefs_changed();
        }
    }

    /// The main window's handle, found once.
    pub(super) fn main_hwnd(&mut self) -> Option<isize> {
        if self.shell.hwnd.is_none() {
            self.shell.hwnd = super::window::main_window_handle();
        }
        self.shell.hwnd
    }

    /// Reads the real "start with Windows" state once (the Run entry can be removed elsewhere).
    fn check_start_with_windows(&mut self) {
        let tx = self.shell.jobs_tx.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn_blocking(move || {
            let on = platform::starts_with_windows();
            let _ = tx.send(ShellJob::StartsWithWindows(on));
            ctx.request_repaint();
        });
    }

    pub(super) fn set_start_with_windows(&mut self, enabled: bool) {
        if self.shell.start_with_windows_pending.is_some() {
            return;
        }
        self.shell.start_with_windows_pending = Some(enabled);
        let tx = self.shell.jobs_tx.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn_blocking(move || {
            let result = platform::set_start_with_windows(enabled).map_err(|e| format!("{e:#}"));
            let _ = tx.send(ShellJob::StartWithWindowsSet { enabled, result });
            ctx.request_repaint();
        });
    }

    pub(super) fn create_start_menu_shortcut(&mut self) {
        if self.shell.shortcut_busy {
            return;
        }
        self.shell.shortcut_busy = true;
        let tx = self.shell.jobs_tx.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn_blocking(move || {
            let result = platform::create_start_menu_shortcut().map_err(|e| format!("{e:#}"));
            let _ = tx.send(ShellJob::Shortcut(result));
            ctx.request_repaint();
        });
    }

    fn handle_shell_jobs(&mut self) {
        while let Ok(job) = self.shell.jobs_rx.try_recv() {
            match job {
                ShellJob::StartsWithWindows(on) => {
                    if self.shell.start_with_windows_pending.is_none() && self.prefs.start_with_windows != on {
                        log::info!("start with Windows is {}", if on { "on" } else { "off" });
                        self.prefs.start_with_windows = on;
                        self.prefs_changed();
                    }
                }
                ShellJob::StartWithWindowsSet { enabled, result } => {
                    self.shell.start_with_windows_pending = None;
                    match result {
                        Ok(()) => {
                            self.prefs.start_with_windows = enabled;
                            self.prefs_changed();
                            let text = if enabled {
                                "Truffle will start when you sign in to Windows."
                            } else {
                                "Truffle won't start with Windows any more."
                            };
                            self.toast(text, false);
                        }
                        Err(e) => {
                            log::warn!("could not change start with Windows: {e}");
                            self.toast(format!("Couldn't change whether Truffle starts with Windows: {e}"), true);
                        }
                    }
                }
                ShellJob::Shortcut(result) => {
                    self.shell.shortcut_busy = false;
                    match result {
                        Ok(path) => {
                            log::info!("start menu shortcut at {}", path.display());
                            self.toast("Truffle is now in the Start menu.", false);
                        }
                        Err(e) => {
                            log::warn!("could not create the start menu shortcut: {e}");
                            self.toast(format!("Couldn't add Truffle to the Start menu: {e}"), true);
                        }
                    }
                }
            }
        }
    }

    /// The app icon (window, taskbar, tray) in the style chosen. "Automatic" follows the taskbar:
    /// its mode is read again as often as Windows' app mode is (never asking for a frame of its
    /// own), so the icon changes soon after Windows does.
    fn follow_icon_style(&mut self, ctx: &egui::Context) {
        if self.prefs.app_icon == AppIcon::Automatic
            && self.shell.taskbar_read.is_none_or(|at| at.elapsed() >= WINDOWS_MODE_RECHECK)
        {
            self.shell.taskbar_read = Some(Instant::now());
            self.shell.taskbar_dark = platform::windows_taskbar_dark().unwrap_or(true);
        }
        let style = icon::style_for(self.prefs.app_icon, self.shell.taskbar_dark);
        if self.shell.icon_shown == Some(style) {
            return;
        }
        self.shell.icon_shown = Some(style);
        root_cmd(ctx, ViewportCommand::Icon(Some(std::sync::Arc::new(icon::window_icon(style)))));
        self.shell.platform.set_icon_style(style);
    }

    /// "Same as Windows": reads Windows' light or dark mode again when it may have changed (winit
    /// reported a new theme, or the window got the focus back), else at most every few seconds of
    /// frames that are drawn anyway. Never asks for a frame of its own.
    fn follow_windows_mode(&mut self, ctx: &egui::Context) {
        if self.prefs.theme != ThemeMode::System {
            self.shell.mode_read = None;
            return;
        }
        let (focused, reported) = (ctx.input(|i| i.focused), ctx.system_theme());
        let got_focus = focused && !std::mem::replace(&mut self.shell.focused, focused);
        let told = reported != std::mem::replace(&mut self.shell.reported_theme, reported);
        let due = self.shell.mode_read.is_none_or(|at| at.elapsed() >= WINDOWS_MODE_RECHECK);
        if !(due || got_focus || told) {
            return;
        }
        self.shell.mode_read = Some(Instant::now());
        if theme::reread_windows_mode() {
            log::info!("Windows switched to {} mode", if theme::shows_dark(ThemeMode::System) { "dark" } else { "light" });
            self.restyle(ctx);
        }
    }

    /// Re-applies the Windows-integration settings (media controls, tray, Discord).
    pub(super) fn apply_platform_settings(&mut self) {
        self.shell.platform.apply_settings(&self.prefs);
    }
}
