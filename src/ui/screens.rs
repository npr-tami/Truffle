//! The interface size, per screen. Each screen keeps its own: the one set there (Ctrl + / Ctrl −,
//! or Settings), else one picked from the screen's height, so Truffle isn't tiny on a big screen.
//! The picked one also fits the full window: never bigger than the window has room for (at least
//! the room the default window has at 100%), never below 100%.
//! Ctrl 0 goes back to the picked one. "Grow with the window" makes it bigger on a wide window,
//! once the window stops changing size. Sizes are egui zoom factors.
//!
//! The window's screen is looked up only when the window moved or its screen changed (egui
//! reports both every pass), not every pass. The mini player's screen is the one under its top
//! left corner, since its size follows the interface size.

use std::time::{Duration, Instant};

use egui::gui_zoom::kb_shortcuts;

use super::TruffleApp;
use super::window::{self, Monitor};
use crate::config::Settings;

/// Smallest and largest interface size.
const MIN_SIZE: f32 = 0.6;
const MAX_SIZE: f32 = 2.5;
/// How much Ctrl + and Ctrl − change the size.
const STEP: f32 = 0.1;
/// The window the layout is made for at 100% (Windows' units): the default full window.
const ROOM: egui::Vec2 = window::FULL_DEFAULT_SIZE;
/// "Grow with the window" (and the auto size's fit to the window) follows a new size once it has
/// stayed this long.
const GROW_SETTLE: Duration = Duration::from_millis(300);
/// Shown once on each screen auto size made bigger.
const BIG_SCREEN_TIP: &str = "Big screen: Truffle made itself bigger. Ctrl + and Ctrl − change it.";

/// A screen: what tells it apart from the others (its size is kept by it) and its height in
/// Windows' own units (pixels divided by Windows' scale: egui points at zoom 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub key: String,
    pub height: f32,
}

impl Screen {
    /// `name` is Windows' name for the monitor; without one, its size alone tells it apart.
    /// `width` and `height` are in physical pixels, `scale` is Windows' (1.0 at 100%).
    pub fn new(name: Option<&str>, width: i32, height: i32, scale: f32) -> Self {
        let size = format!("{width}x{height}");
        let key = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => format!("{name} {size}"),
            None => size,
        };
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        Self { key, height: height as f32 / scale }
    }
}

impl From<Monitor> for Screen {
    fn from(m: Monitor) -> Self {
        Self::new(Some(&m.name), m.width, m.height, m.scale)
    }
}

/// Ctrl + (or Ctrl =), Ctrl − and Ctrl 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomKey {
    Bigger,
    Smaller,
    Auto,
}

/// Takes Ctrl +, Ctrl − or Ctrl 0 from the keys of the window being drawn. egui's own handling of
/// them is off: its Ctrl 0 would go to 1.0, not to the screen's auto size.
pub fn take_zoom_key(ctx: &egui::Context) -> Option<ZoomKey> {
    ctx.input_mut(|i| {
        if i.consume_shortcut(&kb_shortcuts::ZOOM_RESET) {
            Some(ZoomKey::Auto)
        } else if i.consume_shortcut(&kb_shortcuts::ZOOM_IN) || i.consume_shortcut(&kb_shortcuts::ZOOM_IN_SECONDARY) {
            Some(ZoomKey::Bigger)
        } else if i.consume_shortcut(&kb_shortcuts::ZOOM_OUT) {
            Some(ZoomKey::Smaller)
        } else {
            None
        }
    })
}

fn round(size: f32) -> f32 {
    (size * 20.0).round() / 20.0
}

/// A size rounded to 0.05 and kept within 0.6-2.5 (nonsense gives 1.0).
pub fn sanitize(size: f32) -> f32 {
    if size.is_finite() { round(size) } else { 1.0 }.clamp(MIN_SIZE, MAX_SIZE)
}

/// The size picked for a screen `height` tall (Windows' units): 1.0 at 1080 and 0.75 more for
/// each 1080 more, at most 2.5. A smaller screen is never made smaller than 1.0.
pub fn auto_size(height: f32) -> f32 {
    if !height.is_finite() {
        return 1.0;
    }
    round((1.0 + 0.75 * (height / 1080.0 - 1.0)).clamp(1.0, MAX_SIZE))
}

/// The biggest auto size a window `width` x `height` (Windows' units, so the size itself doesn't
/// change it) has room for: as much room as [`ROOM`] at 100%, rounded down to 0.05, never below
/// 1.0. A 1400 x 790 window: 1.0.
pub fn window_fit(width: f32, height: f32) -> f32 {
    // f32::min would drop a NaN side, so check both.
    let fit = (width / ROOM.x).min(height / ROOM.y);
    if !width.is_finite() || !height.is_finite() || !fit.is_finite() {
        return MAX_SIZE;
    }
    // Down, not to the nearest: the room is at least what the layout asks for (the tiny bit is
    // for float noise on an exact step).
    ((fit * 20.0 + 1e-3).floor() / 20.0).clamp(1.0, MAX_SIZE)
}

/// The auto size on a screen `height` tall in a window that `fit`s ([`window_fit`]): the smaller
/// of the two, never below 1.0.
pub fn fitted_auto_size(height: f32, fit: f32) -> f32 {
    let fit = if fit.is_finite() { fit.max(1.0) } else { MAX_SIZE };
    auto_size(height).min(round(fit))
}

/// How much "Grow with the window" makes the interface bigger on a window `width` wide (Windows'
/// units, so the size itself doesn't change it): not up to 1400, 1.5 times from 2100.
pub fn growth(width: f32) -> f32 {
    if !width.is_finite() {
        return 1.0;
    }
    round((width / 1400.0).clamp(1.0, 1.5))
}

/// The size on `screen`: the one set there, else its auto size. Without a known screen, the size
/// kept from before each screen had its own.
pub fn size_on(prefs: &Settings, screen: Option<&Screen>) -> f32 {
    match screen {
        Some(s) => prefs.screen_sizes.get(&s.key).map_or_else(|| auto_size(s.height), |&size| sanitize(size)),
        None => sanitize(prefs.ui_scale),
    }
}

/// Whether the size on `screen` was set (else it is the auto size).
pub fn is_set(prefs: &Settings, screen: Option<&Screen>) -> bool {
    match screen {
        Some(s) => prefs.screen_sizes.contains_key(&s.key),
        None => (prefs.ui_scale - 1.0).abs() > 0.001,
    }
}

/// Sets the size on `screen`, or with `None` forgets it (back to the auto size). Whether the
/// settings changed.
pub fn set_size(prefs: &mut Settings, screen: Option<&Screen>, size: Option<f32>) -> bool {
    match (screen, size) {
        (Some(s), Some(size)) => {
            let size = sanitize(size);
            prefs.screen_sizes.insert(s.key.clone(), size).is_none_or(|old| (old - size).abs() > 0.001)
        }
        (Some(s), None) => prefs.screen_sizes.remove(&s.key).is_some(),
        (None, size) => {
            let size = size.map_or(1.0, sanitize);
            let changed = (prefs.ui_scale - size).abs() > 0.001;
            prefs.ui_scale = size;
            changed
        }
    }
}

/// Ctrl + / Ctrl − change the size on `screen` (now `size`, the one that shows), Ctrl 0 forgets
/// it. Whether the settings changed.
pub fn press(prefs: &mut Settings, screen: Option<&Screen>, size: f32, key: ZoomKey) -> bool {
    match key {
        ZoomKey::Bigger => set_size(prefs, screen, Some(size + STEP)),
        ZoomKey::Smaller => set_size(prefs, screen, Some(size - STEP)),
        ZoomKey::Auto => set_size(prefs, screen, None),
    }
}

/// What arriving on a screen did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Arrival {
    /// The settings changed.
    pub changed: bool,
    /// Truffle made itself bigger here for the first time: say so.
    pub tip: bool,
}

/// Truffle opened on `screen`, or its window moved there. A size from before each screen had its
/// own was chosen by the user: it becomes this screen's (if it has none yet).
pub fn arrive(prefs: &mut Settings, screen: &Screen) -> Arrival {
    let mut arrival = Arrival::default();
    if is_set(prefs, None) {
        let old = sanitize(prefs.ui_scale);
        prefs.screen_sizes.entry(screen.key.clone()).or_insert(old);
        prefs.ui_scale = 1.0;
        arrival.changed = true;
    }
    if !is_set(prefs, Some(screen)) && auto_size(screen.height) > 1.0 && prefs.big_screen_tips.insert(screen.key.clone()) {
        arrival = Arrival { changed: true, tip: true };
    }
    arrival
}

/// "Grow with the window": the growth for the window's width, following a new width once it has
/// stayed for [`GROW_SETTLE`] (so the size doesn't jump while the window is dragged bigger).
#[derive(Clone, Copy, Debug)]
pub struct Grow {
    /// The width last seen (Windows' units), and since when.
    width: Option<(f32, Instant)>,
    /// The growth in use.
    pub factor: f32,
}

impl Default for Grow {
    fn default() -> Self {
        Self { width: None, factor: 1.0 }
    }
}

impl Grow {
    /// The window is `width` wide (the first width counts at once). While a new growth waits for
    /// the width to stay, how long it still waits.
    pub fn follow(&mut self, width: f32, now: Instant) -> Option<Duration> {
        let since = match self.width {
            Some((w, since)) if (w - width).abs() <= 0.5 => since,
            Some(_) => now,
            None => {
                self.factor = growth(width);
                now
            }
        };
        self.width = Some((width, since));
        let target = growth(width);
        let waited = now.saturating_duration_since(since);
        if (target - self.factor).abs() < 0.001 {
            None
        } else if waited >= GROW_SETTLE {
            self.factor = target;
            None
        } else {
            Some(GROW_SETTLE - waited)
        }
    }
}

/// The auto size's fit to the window ([`window_fit`]), following a new window size once it has
/// stayed for [`GROW_SETTLE`] (so the size doesn't jump while the window is dragged).
#[derive(Clone, Copy, Debug)]
pub struct Fit {
    /// The window size last seen (Windows' units), and since when.
    size: Option<([f32; 2], Instant)>,
    /// The fit in use (until the window is known: no limit).
    pub fit: f32,
}

impl Default for Fit {
    fn default() -> Self {
        Self { size: None, fit: MAX_SIZE }
    }
}

impl Fit {
    /// The window is `width` x `height` (the first size counts at once). While a new fit waits
    /// for the size to stay, how long it still waits.
    pub fn follow(&mut self, width: f32, height: f32, now: Instant) -> Option<Duration> {
        let since = match self.size {
            Some(([w, h], since)) if (w - width).abs() <= 0.5 && (h - height).abs() <= 0.5 => since,
            Some(_) => now,
            None => {
                self.fit = window_fit(width, height);
                now
            }
        };
        self.size = Some(([width, height], since));
        let target = window_fit(width, height);
        let waited = now.saturating_duration_since(since);
        if (target - self.fit).abs() < 0.001 {
            None
        } else if waited >= GROW_SETTLE {
            self.fit = target;
            None
        } else {
            Some(GROW_SETTLE - waited)
        }
    }
}

/// What the UI keeps to follow the window's screen and width.
#[derive(Default)]
pub struct ScreenZoom {
    /// The screen the window is on, once known.
    pub screen: Option<Screen>,
    /// The window's place, its screen's size (physical pixels), Windows' scale (%) and whether it
    /// is the mini player when the screen was last looked up.
    seen: Option<[i32; 6]>,
    /// Opening on a screen (looked up before the first pass), acted on in the first drawn pass.
    arrived: Option<Arrival>,
    grow: Grow,
    fit: Fit,
    /// The zoom of the last pass, and the one this frame's window sizes were measured with.
    last_zoom: f32,
    measured_zoom: f32,
    /// Ctrl + / Ctrl − / Ctrl 0 pressed in Settings' own window, for the main window's next pass.
    pub key: Option<ZoomKey>,
}

impl ScreenZoom {
    /// The screen the (still hidden) main window opens on and its width there, so the first
    /// frame has their size (with its growth).
    pub fn opening(ctx: &egui::Context, prefs: &mut Settings) -> Self {
        let hwnd = window::main_window_handle();
        let monitor = hwnd.and_then(window::monitor_of);
        let mut grow = Grow::default();
        let mut fit = Fit::default();
        // Opening maximized, it soon has its screen's size.
        let rect = hwnd.and_then(|h| if prefs.main_window_maximized { window::work_area(h) } else { window::visible_rect(h) });
        if let (Some(r), Some(m)) = (rect, &monitor) {
            let (w, h, now) = (r.width() as f32 / m.scale, r.height() as f32 / m.scale, Instant::now());
            grow.follow(w, now);
            fit.follow(w, h, now);
        }
        let screen = monitor.map(Screen::from);
        let arrived = screen.as_ref().map(|s| arrive(prefs, s));
        let now = ctx.zoom_factor();
        let zoom = Self { screen, arrived, grow, fit, last_zoom: now, measured_zoom: now, ..Self::default() };
        ctx.set_zoom_factor(zoom.zoom(prefs, false));
        zoom
    }

    /// Runs at the start of every pass, drawn or not, with its zoom: returns the zoom egui's
    /// window and screen sizes in it were measured with. They are measured just before a frame,
    /// and a new zoom applies at the start of a pass, so in that pass they are still in the old
    /// one's points. `rerun`: egui runs the frame again (it discarded the pass), same sizes.
    pub fn next_pass(&mut self, zoom: f32, rerun: bool) -> f32 {
        let last = std::mem::replace(&mut self.last_zoom, zoom);
        if !rerun {
            self.measured_zoom = last;
        }
        self.measured_zoom
    }

    /// The size on the window's screen: the one set there, else its auto size, fitted to the full
    /// window (the mini player's size follows the interface size, so it keeps the screen's).
    pub fn size(&self, prefs: &Settings, mini: bool) -> f32 {
        match &self.screen {
            Some(s) if !mini && !is_set(prefs, Some(s)) => fitted_auto_size(s.height, self.fit.fit),
            screen => size_on(prefs, screen.as_ref()),
        }
    }

    /// The zoom to use: the size on the window's screen, times its growth with "Grow with the
    /// window" on (the full window only: the mini player keeps the screen's size).
    pub fn zoom(&self, prefs: &Settings, mini: bool) -> f32 {
        let size = self.size(prefs, mini);
        if prefs.grow_with_window && !mini { sanitize(size * self.grow.factor) } else { size }
    }
}

impl TruffleApp {
    /// Runs every drawn pass, before anything is drawn: follows the window to another screen,
    /// takes Ctrl + / Ctrl − / Ctrl 0, and applies the size on the window's screen, times its
    /// growth with "Grow with the window" on. `measured` is from [`ScreenZoom::next_pass`].
    pub(super) fn sync_zoom(&mut self, ctx: &egui::Context, measured: f32) {
        if let Some(arrival) = self.screen_zoom.arrived.take() {
            self.arrived(arrival);
        }
        self.follow_screen(ctx, measured);
        if let Some(key) = take_zoom_key(ctx).or_else(|| self.screen_zoom.key.take()) {
            let size = self.screen_zoom.size(&self.prefs, self.shell.mini);
            if press(&mut self.prefs, self.screen_zoom.screen.as_ref(), size, key) {
                self.prefs_changed();
            }
        }
        // The window's inner size in Windows' units (points times the zoom they were measured
        // with): a new zoom doesn't change it, so it can't feed back into the size.
        if !self.shell.mini
            && let Some(r) = ctx.input(|i| i.viewport().inner_rect)
        {
            let (w, h, now) = (r.width() * measured, r.height() * measured, Instant::now());
            let grow = self.screen_zoom.grow.follow(w, now).filter(|_| self.prefs.grow_with_window);
            let fit = self.screen_zoom.fit.follow(w, h, now);
            if let Some(wait) = [grow, fit].into_iter().flatten().min() {
                ctx.request_repaint_after(wait);
            }
        }
        let zoom = self.screen_zoom.zoom(&self.prefs, self.shell.mini);
        if (zoom - ctx.zoom_factor()).abs() > 0.001 {
            ctx.set_zoom_factor(zoom);
        }
    }

    /// Looks up the screen the window is on once it moved, or its screen changed size or scale.
    fn follow_screen(&mut self, ctx: &egui::Context, measured: f32) {
        let (outer, monitor, scale) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.monitor_size, v.native_pixels_per_point.unwrap_or(1.0))
        });
        // Minimised: on no screen.
        let Some(outer) = outer else { return };
        let px = |points: f32| (points * measured * scale).round() as i32;
        let [w, h] = monitor.map_or([0, 0], |m| [px(m.x), px(m.y)]);
        let (x, y, mini) = (px(outer.min.x), px(outer.min.y), self.shell.mini);
        let seen = [x, y, w, h, (scale * 100.0).round() as i32, i32::from(mini)];
        if self.screen_zoom.seen == Some(seen) {
            return;
        }
        self.screen_zoom.seen = Some(seen);
        let monitor_now = if mini {
            // The mini player's size follows the interface size, so the screen it is mostly on
            // could change with it, back and forth: its screen is the one under its top left
            // corner, which stays put.
            window::monitor_at(x + 24, y + 12)
        } else {
            self.main_hwnd().and_then(window::monitor_of)
        };
        let screen = monitor_now.map(Screen::from).or_else(|| monitor.map(|_| Screen::new(None, w, h, scale)));
        let Some(screen) = screen.filter(|s| self.screen_zoom.screen.as_ref() != Some(s)) else { return };
        let arrival = arrive(&mut self.prefs, &screen);
        log::info!("on the screen {} ({:.0} tall): interface size {:.2}", screen.key, screen.height, size_on(&self.prefs, Some(&screen)));
        self.screen_zoom.screen = Some(screen);
        self.arrived(arrival);
    }

    fn arrived(&mut self, arrival: Arrival) {
        if arrival.tip {
            self.toast(BIG_SCREEN_TIP, false);
        }
        if arrival.changed {
            self.prefs_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(key: &str, height: f32) -> Screen {
        Screen { key: key.into(), height }
    }

    #[test]
    fn a_screen_gets_a_size_from_its_height() {
        for (height, size) in [(1080.0, 1.0), (1200.0, 1.1), (1440.0, 1.25), (1600.0, 1.35), (2160.0, 1.75), (2880.0, 2.25)] {
            assert!((auto_size(height) - size).abs() < 1e-6, "{height}: {} not {size}", auto_size(height));
        }
        // Never smaller than 1.0, at most 2.5; nonsense gives 1.0.
        assert_eq!(auto_size(768.0), 1.0);
        assert_eq!(auto_size(0.0), 1.0);
        assert_eq!(auto_size(10_000.0), 2.5);
        assert_eq!(auto_size(f32::NAN), 1.0);
    }

    #[test]
    fn sizes_are_rounded_and_kept_in_range() {
        assert!((sanitize(1.27) - 1.25).abs() < 1e-6);
        assert!((sanitize(1.08) - 1.1).abs() < 1e-6);
        assert_eq!((sanitize(0.1), sanitize(9.0), sanitize(f32::INFINITY)), (0.6, 2.5, 1.0));
    }

    #[test]
    fn the_auto_size_fits_the_window() {
        // The default window has room for 100%, twice its size for 200%; rounded down.
        assert_eq!(window_fit(1180.0, 760.0), 1.0);
        assert_eq!(window_fit(2360.0, 1520.0), 2.0);
        assert!((window_fit(1475.0, 950.0) - 1.25).abs() < 1e-6);
        assert!((window_fit(1470.0, 2000.0) - 1.2).abs() < 1e-6);
        // The tighter side counts; never below 100%; nonsense counts as room enough.
        assert_eq!(window_fit(1400.0, 790.0), 1.0);
        assert_eq!(window_fit(5000.0, 400.0), 1.0);
        assert_eq!(window_fit(f32::NAN, 800.0), MAX_SIZE);
        // A 1440 screen picks 125%; a 1400 x 790 window on it gets 100%.
        assert_eq!(fitted_auto_size(1440.0, window_fit(1400.0, 790.0)), 1.0);
        // Maximized there: the screen's 125%.
        assert!((fitted_auto_size(1440.0, window_fit(2560.0, 1380.0)) - 1.25).abs() < 1e-6);
        // In between: what the window has room for.
        assert!((fitted_auto_size(2160.0, window_fit(1770.0, 1140.0)) - 1.5).abs() < 1e-6);
        // A small screen stays at 100% even in a big window.
        assert_eq!(fitted_auto_size(1080.0, 2.5), 1.0);
        assert!((fitted_auto_size(1440.0, f32::NAN) - 1.25).abs() < 1e-6);
    }

    #[test]
    fn the_fit_waits_until_the_window_stops_changing_size() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        let mut fit = Fit::default();
        // The first size counts at once.
        assert_eq!(fit.follow(2360.0, 1520.0, t), None);
        assert_eq!(fit.fit, 2.0);
        // Dragged smaller: nothing changes until it has stayed 0.3 s.
        assert_eq!(fit.follow(1400.0, 790.0, ms(100)), Some(GROW_SETTLE));
        assert_eq!(fit.follow(1400.0, 790.0, ms(300)), Some(Duration::from_millis(100)));
        assert_eq!(fit.fit, 2.0);
        assert_eq!(fit.follow(1400.0, 790.0, ms(400)), None);
        assert_eq!(fit.fit, 1.0);
        // A new size with the same fit has nothing to wait for.
        assert_eq!(fit.follow(1300.0, 780.0, ms(500)), None);
    }

    #[test]
    fn auto_fits_the_window_and_a_set_size_does_not() {
        let mut prefs = Settings::default();
        let mut zoom = ScreenZoom { screen: Some(screen("A 2560x1440", 1440.0)), ..ScreenZoom::default() };
        // The window not known yet: the screen's auto size.
        assert_eq!(zoom.size(&prefs, false), 1.25);
        zoom.fit.follow(1400.0, 790.0, Instant::now());
        assert_eq!(zoom.size(&prefs, false), 1.0);
        // The mini player keeps the screen's.
        assert_eq!(zoom.size(&prefs, true), 1.25);
        // Ctrl + goes up from what shows; a set size stays as set.
        let shown = zoom.size(&prefs, false);
        assert!(press(&mut prefs, zoom.screen.as_ref(), shown, ZoomKey::Bigger));
        assert!((zoom.size(&prefs, false) - 1.1).abs() < 1e-6);
        set_size(&mut prefs, zoom.screen.as_ref(), Some(1.5));
        assert_eq!(zoom.size(&prefs, false), 1.5);
    }

    #[test]
    fn a_wide_window_grows_up_to_half_again() {
        for (width, factor) in [(800.0, 1.0), (1400.0, 1.0), (1540.0, 1.1), (1750.0, 1.25), (2100.0, 1.5), (3440.0, 1.5)] {
            assert!((growth(width) - factor).abs() < 1e-6, "{width}: {} not {factor}", growth(width));
        }
        assert_eq!(growth(f32::NAN), 1.0);
    }

    #[test]
    fn the_growth_waits_until_the_window_stops_changing_size() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        let mut grow = Grow::default();
        // The first width counts at once.
        assert_eq!(grow.follow(2100.0, t), None);
        assert_eq!(grow.factor, 1.5);
        // Dragged narrower: nothing changes while it moves, nor until it has stayed 0.3 s.
        assert_eq!(grow.follow(1800.0, ms(100)), Some(GROW_SETTLE));
        assert_eq!(grow.follow(1400.0, ms(200)), Some(GROW_SETTLE));
        assert_eq!(grow.follow(1400.0, ms(400)), Some(Duration::from_millis(100)));
        assert_eq!(grow.factor, 1.5);
        assert_eq!(grow.follow(1400.0, ms(500)), None);
        assert_eq!(grow.factor, 1.0);
        // A new width with the same growth has nothing to wait for.
        assert_eq!(grow.follow(1200.0, ms(600)), None);
    }

    #[test]
    fn only_the_full_window_grows() {
        let mut prefs = Settings::default();
        let mut zoom = ScreenZoom { screen: Some(screen("A 2560x1440", 1440.0)), ..ScreenZoom::default() };
        zoom.grow.follow(2100.0, Instant::now());
        assert_eq!(zoom.zoom(&prefs, false), 1.25);
        prefs.grow_with_window = true;
        assert!((zoom.zoom(&prefs, false) - 1.9).abs() < 1e-6);
        assert_eq!(zoom.zoom(&prefs, true), 1.25);
    }

    #[test]
    fn sizes_are_measured_with_the_last_passs_zoom() {
        // A new zoom applies at the start of a pass, after its sizes were measured.
        let mut zoom = ScreenZoom { last_zoom: 1.0, measured_zoom: 1.0, ..ScreenZoom::default() };
        assert_eq!(zoom.next_pass(1.25, false), 1.0);
        // egui ran that frame again: the same sizes.
        assert_eq!(zoom.next_pass(1.25, true), 1.0);
        assert_eq!(zoom.next_pass(1.25, false), 1.25);
    }

    #[test]
    fn each_screen_keeps_its_own_size() {
        let mut prefs = Settings::default();
        let (big, laptop) = (screen(r"\\.\DISPLAY1 3840x2160", 2160.0), screen(r"\\.\DISPLAY2 1920x1080", 1080.0));
        // Auto until set.
        assert_eq!((size_on(&prefs, Some(&big)), size_on(&prefs, Some(&laptop))), (1.75, 1.0));
        // Ctrl + on the big screen: only its size changes.
        assert!(press(&mut prefs, Some(&big), 1.75, ZoomKey::Bigger));
        assert!((size_on(&prefs, Some(&big)) - 1.85).abs() < 1e-6);
        assert_eq!(size_on(&prefs, Some(&laptop)), 1.0);
        assert!(press(&mut prefs, Some(&laptop), 1.0, ZoomKey::Smaller));
        assert!((size_on(&prefs, Some(&laptop)) - 0.9).abs() < 1e-6);
        // Settings sets it; setting the same size again changes nothing.
        assert!(set_size(&mut prefs, Some(&big), Some(1.5)));
        assert!(!set_size(&mut prefs, Some(&big), Some(1.5)));
        assert_eq!(size_on(&prefs, Some(&big)), 1.5);
        // Ctrl 0 forgets it: back to auto, there only.
        assert!(press(&mut prefs, Some(&big), 1.5, ZoomKey::Auto));
        assert!(!press(&mut prefs, Some(&big), 1.75, ZoomKey::Auto));
        assert!(!is_set(&prefs, Some(&big)) && is_set(&prefs, Some(&laptop)));
        assert_eq!(size_on(&prefs, Some(&big)), 1.75);
        // At the ends, Ctrl + and Ctrl − stop.
        set_size(&mut prefs, Some(&big), Some(2.5));
        press(&mut prefs, Some(&big), 2.5, ZoomKey::Bigger);
        assert_eq!(size_on(&prefs, Some(&big)), 2.5);
    }

    #[test]
    fn screens_are_told_apart_by_name_and_size() {
        let s = Screen::new(Some(r"\\.\DISPLAY1"), 3840, 2160, 1.5);
        assert_eq!((s.key.as_str(), s.height), (r"\\.\DISPLAY1 3840x2160", 1440.0));
        // Without a name the size alone; a nonsense scale counts as 100%.
        let s = Screen::new(Some(" "), 2560, 1440, 0.0);
        assert_eq!((s.key.as_str(), s.height), ("2560x1440", 1440.0));
    }

    #[test]
    fn an_older_size_becomes_the_first_screens() {
        let (first, second) = (screen("A 3840x2160", 2160.0), screen("B 1920x1080", 1080.0));
        let mut prefs = Settings { ui_scale: 1.3, ..Settings::default() };
        // Before a screen is known, the older size is used.
        assert_eq!(size_on(&prefs, None), 1.3);
        // The screen Truffle opens on takes it, instead of its auto size, and no tip.
        assert_eq!(arrive(&mut prefs, &first), Arrival { changed: true, tip: false });
        assert_eq!((size_on(&prefs, Some(&first)), prefs.ui_scale), (1.3, 1.0));
        // Only once: the next screen has its own.
        assert_eq!(arrive(&mut prefs, &second), Arrival::default());
        assert_eq!(size_on(&prefs, Some(&second)), 1.0);
        // A file without an older size: nothing to carry over.
        let mut prefs = Settings::default();
        arrive(&mut prefs, &second);
        assert!(prefs.screen_sizes.is_empty());
    }

    #[test]
    fn the_big_screen_tip_shows_once_per_screen() {
        let (big, other_big, laptop) = (screen("A 3840x2160", 2160.0), screen("B 2560x1440", 1440.0), screen("C 1920x1080", 1080.0));
        let mut prefs = Settings::default();
        let tip = Arrival { changed: true, tip: true };
        assert_eq!(arrive(&mut prefs, &big), tip);
        assert_eq!(arrive(&mut prefs, &big), Arrival::default());
        assert_eq!(arrive(&mut prefs, &other_big), tip);
        // Not made bigger: nothing to say.
        assert_eq!(arrive(&mut prefs, &laptop), Arrival::default());
        // A size set there already: auto didn't make it bigger.
        let mut prefs = Settings::default();
        set_size(&mut prefs, Some(&big), Some(1.2));
        assert_eq!(arrive(&mut prefs, &big), Arrival::default());
        // Remembered with the settings.
        let mut prefs = Settings::default();
        arrive(&mut prefs, &big);
        let mut back: Settings = serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(arrive(&mut back, &big), Arrival::default());
    }
}
