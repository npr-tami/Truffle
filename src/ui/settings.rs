//! Settings: a window of its own (never docked), opened over the middle of Truffle; it moves and
//! resizes like any window and keeps its size. Laid out like Windows 11's Settings, in the Final
//! design's colours: the sections down the left with their icons, a search box above them that finds
//! any setting in any section, and the chosen section's cards on the right. Every change applies at
//! once. Sections: Audio (quality, loudness, crossfade, equalizer, cache), Shuffle, Appearance
//! (`side.rs`), Accounts (Last.fm in `settings/lastfm.rs`; listen along only points to the
//! Friends tab), System, Shortcuts, About (`settings/about.rs`).
//!
//! The window is an egui immediate viewport owned by the main window: it is in front of it, minimised
//! with it (let go meanwhile, so nothing is drawn, and back with it), has no taskbar button, and goes
//! when the main window hides, switches to the mini player or quits. Esc, Ctrl+W and Ctrl+, close it.

mod about;
mod lastfm;

pub(super) use about::AboutState;

use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, Id, Key, Layout, Margin, Modifiers, Rect, RichText, ScrollArea,
    Sense, Stroke, TextEdit, Ui, Vec2, ViewportBuilder, ViewportCommand, ViewportId, pos2, vec2,
};

use super::eq::{self, CUSTOM, FLAT};
use super::fin::{self, Look};
use super::kit::{self, Kind};
use super::motion;
use super::svgicon;
use super::theme::{bold, heavy, medium, regular, text_field};
use super::util::{looks_like_client_id, looks_like_discord_id, open_in_browser};
use super::window::{self as win, ScreenRect};
use super::TruffleApp;
use crate::model::{Command, EQ_BANDS_HZ};
use crate::text::{contains_folded, fold};
use crate::webapi::REDIRECT_URI;

const DASHBOARD_URL: &str = "https://developer.spotify.com/dashboard";
const DISCORD_APPS_URL: &str = "https://discord.com/developers/applications";
/// While an equalizer slider is dragged, changes go out at most this often.
const EQ_SEND_INTERVAL: Duration = Duration::from_millis(80);
/// A typed Discord Application ID is applied once it has been left alone this long.
const DISCORD_ID_SETTLE: Duration = Duration::from_millis(700);
/// How often the Discord connection status is re-read while it is on screen.
const DISCORD_STATUS_REFRESH: Duration = Duration::from_secs(1);
const EQ_SLIDER_LENGTH: f32 = 110.0;
const EQ_COLUMN_WIDTH: f32 = 38.0;
/// The Settings window's title (Alt+Tab); it is also found by it.
const WINDOW_TITLE: &str = "Truffle settings";
/// Its size the first time, and the smallest it can be made (points).
const DEFAULT_SIZE: Vec2 = vec2(900.0, 640.0);
const MIN_SIZE: Vec2 = vec2(520.0, 420.0);
/// The head row (as tall as the main window's title bar), the section list, and the strip of
/// icons it becomes below `NARROW_W`.
const HEAD_H: f32 = 52.0;
const NAV_W: f32 = 232.0;
const STRIP_W: f32 = 56.0;
const NARROW_W: f32 = 700.0;
/// The cards are at most this wide, this far in from the section list.
const CONTENT_MAX_W: f32 = 760.0;
const CONTENT_PAD: f32 = 32.0;
/// The window's own resize edges (it has no Windows frame).
const EDGE: f32 = 5.0;
/// A place Windows didn't give the window exactly is asked for this many more times, then left.
const PLACE_RETRIES: u8 = 8;
const SEARCH_ID: &str = "settings-search";

/// The Settings window's viewport.
fn window_viewport() -> ViewportId {
    ViewportId::from_hash_of("truffle_settings_window")
}

/// Settings' window, while it is open.
#[derive(Default)]
pub(super) struct SettingsWindow {
    /// The main window (its owner) and the Settings window, once found.
    main: Option<isize>,
    hwnd: Option<isize>,
    /// Where it opens (over the middle of the main window), and how many times it was asked
    /// again because it didn't take. After that it is the user's to move.
    target: Option<ScreenRect>,
    retries: u8,
    /// Let go while the main window was minimised: made again once it is back, without taking
    /// the focus from it.
    parked: bool,
    /// The window is made hidden, put in its place, shown cloaked and drawn there, and only then
    /// uncloaked: it never flashes up somewhere else or white first.
    shown: bool,
    /// Drawn passes still to go before uncloaking (it needs a picture drawn while shown).
    cloaked: Option<u8>,
}

/// The sections, in the order of the list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Page {
    Audio,
    Shuffle,
    Appearance,
    Accounts,
    System,
    Shortcuts,
    About,
}

impl Page {
    const ALL: [Page; 7] = [Page::Audio, Page::Shuffle, Page::Appearance, Page::Accounts, Page::System, Page::Shortcuts, Page::About];

    fn title(self) -> &'static str {
        match self {
            Page::Audio => "Audio",
            Page::Shuffle => "Shuffle",
            Page::Appearance => "Appearance",
            Page::Accounts => "Accounts",
            Page::System => "System",
            Page::Shortcuts => "Shortcuts",
            Page::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Page::Audio => "vol",
            Page::Shuffle => "shuffle",
            Page::Appearance => "palette",
            Page::Accounts => "acct",
            Page::System => "monitor",
            Page::Shortcuts => "keys",
            Page::About => "info",
        }
    }

    /// A self-shot's word for a section ("audio", "accounts").
    pub(super) fn from_word(word: &str) -> Option<Page> {
        Page::ALL.into_iter().find(|p| p.title().eq_ignore_ascii_case(word))
    }
}

/// One setting (a row, or a block such as the equalizer). The search finds these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Item {
    SoundSource,
    Quality,
    Loudness,
    Crossfade,
    Equalizer,
    Cache,
    TrueShuffle,
    NoRepeats,
    MixArtists,
    RareFirst,
    SkipBlocked,
    Theme,
    MatchAccent,
    Accent,
    AppIcon,
    Compact,
    Size,
    Spin,
    Motion,
    PageAnim,
    SongSize,
    WideScreens,
    MoreColumns,
    FillCards,
    Banners,
    Wallpaper,
    Colours,
    Spotify,
    ListenAlong,
    Editing,
    Lastfm,
    Discord,
    KeepRunning,
    Tray,
    StartWithWindows,
    MediaKeys,
    MiniOnTop,
    StartMenu,
    Shortcuts,
    Version,
    Plays,
    Account,
    Helpers,
    Folder,
    Details,
    Credits,
}

impl Item {
    /// What the search looks in: the row's words and the words people may type for it.
    fn words(self) -> &'static str {
        match self {
            Item::SoundSource => "sound source youtube spotify free premium audio from",
            Item::Quality => "quality audio bitrate kbps high normal low streaming sound",
            Item::Loudness => "even out loudness normalisation normalization volume level",
            Item::Crossfade => "crossfade fade gap transition gapless",
            Item::Equalizer => "equalizer equaliser eq bass treble preamp preset sound",
            Item::Cache => "audio cache storage disk space mb youtube",
            Item::TrueShuffle => "true shuffle random order fair",
            Item::NoRepeats => "no repeats play every song once first",
            Item::MixArtists => "mix artists same artist twice in a row",
            Item::RareFirst => "rare first favour songs rarely played",
            Item::SkipBlocked => "skip blocked songs block",
            Item::Theme => "theme dark light mode",
            Item::MatchAccent => "match accent album art cover colour color",
            Item::Accent => "accent colour color",
            Item::AppIcon => "app icon taskbar tray silver black",
            Item::Compact => "compact lists rows density smaller",
            Item::Size => "interface size zoom scale bigger smaller text screen monitor auto grow window wide",
            Item::Spin => "spin the record turntable animation",
            Item::Motion => "motion animation speed snappy normal relaxed fast slow",
            Item::PageAnim => "page animation switch transition fade crossfade rise top first",
            Item::SongSize => "song size bigger rows songs list text medium big",
            Item::WideScreens => "wide screen ultrawide layout width centred centered fill split columns",
            Item::MoreColumns => "artist date added columns list",
            Item::FillCards => "cards fill row profile bigger pictures",
            Item::Banners => "banners header picture",
            Item::Wallpaper => {
                "wallpaper background picture image photo nearby folder fit fill whole stretch tile actual position place \
                 across down left right top bottom middle opacity strength see-through zoom blur soft edges fade behind"
            }
            Item::Colours => "custom colours colors background panels text hover",
            Item::Spotify => "spotify account log out logout sign",
            Item::ListenAlong => "listen along friends tab share sharing together what you play link links copy invite join kick remove let back in rename new",
            Item::Editing => "editing playlists web api client id developer rename covers connect spotify",
            Item::Lastfm => "last.fm lastfm scrobble scrobbling api key secret",
            Item::Discord => "discord status listening presence",
            Item::KeepRunning => "keep playing running background close window quit",
            Item::Tray => "tray icon notification area",
            Item::StartWithWindows => "start with windows startup sign in",
            Item::MediaKeys => "media keys controls overlay lock screen",
            Item::MiniOnTop => "mini player on top",
            Item::StartMenu => "start menu shortcut",
            Item::Shortcuts => "keyboard shortcuts keys hotkeys",
            Item::Version => "about version build date truffle record now spinning",
            Item::Plays => "your truffle plays played stats statistics songs different most played top",
            Item::Account => "account premium free sound from spotify youtube",
            Item::Helpers => "youtube helpers yt-dlp node.js deno javascript found version",
            Item::Folder => "folder data cache log logs open",
            Item::Details => "something wrong details copy bug report problem version windows",
            Item::Credits => "made with credits licences licenses open source prip rust egui librespot yt-dlp",
        }
    }
}

/// A card of settings in a section, with its heading.
struct Card {
    page: Page,
    title: Option<&'static str>,
    items: &'static [Item],
}

/// Every setting, by section and card, in the order shown.
const CARDS: &[Card] = &[
    Card { page: Page::Audio, title: Some("Playback"), items: &[Item::SoundSource, Item::Quality, Item::Loudness, Item::Crossfade] },
    Card { page: Page::Audio, title: Some("Equalizer"), items: &[Item::Equalizer] },
    Card { page: Page::Audio, title: Some("Storage"), items: &[Item::Cache] },
    Card {
        page: Page::Shuffle,
        title: Some("Rules"),
        items: &[Item::TrueShuffle, Item::NoRepeats, Item::MixArtists, Item::RareFirst, Item::SkipBlocked],
    },
    Card { page: Page::Appearance, title: Some("Colours"), items: &[Item::Theme, Item::MatchAccent, Item::Accent, Item::AppIcon] },
    Card { page: Page::Appearance, title: Some("Size"), items: &[Item::Size, Item::SongSize, Item::Compact] },
    Card { page: Page::Appearance, title: Some("Pages"), items: &[Item::WideScreens, Item::MoreColumns, Item::FillCards, Item::Banners] },
    Card { page: Page::Appearance, title: Some("Motion"), items: &[Item::Motion, Item::PageAnim, Item::Spin] },
    Card { page: Page::Appearance, title: Some("Wallpaper"), items: &[Item::Wallpaper] },
    Card { page: Page::Appearance, title: Some("Custom colours"), items: &[Item::Colours] },
    Card { page: Page::Accounts, title: Some("Spotify"), items: &[Item::Spotify] },
    Card { page: Page::Accounts, title: Some("Listen along"), items: &[Item::ListenAlong] },
    Card { page: Page::Accounts, title: Some("Developer key (optional)"), items: &[Item::Editing] },
    Card { page: Page::Accounts, title: Some("Last.fm"), items: &[Item::Lastfm] },
    Card { page: Page::Accounts, title: Some("Discord"), items: &[Item::Discord] },
    Card { page: Page::System, title: Some("Background"), items: &[Item::KeepRunning, Item::Tray, Item::StartWithWindows] },
    Card { page: Page::System, title: Some("Desktop"), items: &[Item::MediaKeys, Item::MiniOnTop, Item::StartMenu] },
    Card { page: Page::Shortcuts, title: None, items: &[Item::Shortcuts] },
    Card { page: Page::About, title: None, items: &[Item::Version] },
    Card { page: Page::About, title: Some("Your Truffle"), items: &[Item::Plays] },
    Card { page: Page::About, title: Some("This copy"), items: &[Item::Account, Item::Helpers, Item::Folder, Item::Details] },
    Card { page: Page::About, title: Some("Made with"), items: &[Item::Credits] },
];

/// Whether every word typed (`query`, folded) is in the setting's words, its card's heading or its
/// section's name.
fn found(query: &str, card: &Card, item: Item) -> bool {
    let hay = [item.words(), card.title.unwrap_or(""), card.page.title()];
    query.split_whitespace().all(|w| hay.iter().any(|h| contains_folded(h, w)))
}

fn step(ui: &mut Ui, n: u32, color: Color32, add: impl FnOnce(&mut Ui)) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(format!("{n}.")).strong().color(color));
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            add(ui);
        });
    });
    ui.add_space(4.0);
}

fn note(ui: &mut Ui, text: &str, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).wrap());
}

/// One equalizer band, `width` wide: value, vertical slider, label. Returns the slider's response.
fn eq_column(
    ui: &mut Ui,
    p: &super::theme::Palette,
    width: f32,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    label: &str,
) -> egui::Response {
    ui.allocate_ui_with_layout(vec2(width, EQ_SLIDER_LENGTH + 44.0), Layout::top_down(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        let shown = (*value * 2.0).round() / 2.0;
        let text = if shown.abs() < 0.05 { "0".to_owned() } else { format!("{shown:+}") };
        let on = ui.is_enabled();
        ui.label(RichText::new(text).font(regular(11.0)).color(if on { p.text } else { p.text_faint }));
        let resp = eq_slider(ui, p, value, range);
        ui.label(RichText::new(label).font(regular(11.0)).color(if on { p.text_weak } else { p.text_faint }));
        resp
    })
    .inner
}

/// A band's slider in the Final style: a 3 px line, the accent from 0 dB to the value, and a knob
/// (white while hovered or held). Half-dB steps. `changed()` while it moves.
fn eq_slider(ui: &mut Ui, p: &super::theme::Palette, value: &mut f32, range: std::ops::RangeInclusive<f32>) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(20.0, EQ_SLIDER_LENGTH), Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());
    let span = (hi - lo).max(f32::EPSILON);
    let y_of = |v: f32| rect.bottom() - (v - lo) / span * rect.height();
    if (resp.dragged() || resp.is_pointer_button_down_on())
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let t = ((rect.bottom() - pos.y) / rect.height()).clamp(0.0, 1.0);
        let v = ((lo + t * span) * 2.0).round() / 2.0;
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    let on = ui.is_enabled();
    let painter = ui.painter();
    let x = rect.center().x;
    painter.rect_filled(Rect::from_min_max(pos2(x - 1.5, rect.top()), pos2(x + 1.5, rect.bottom())), CornerRadius::same(2), p.border);
    let zero = y_of(0.0_f32.clamp(lo, hi));
    let at = y_of(value.clamp(lo, hi));
    let fill = if on { p.accent_ink } else { p.text_faint };
    painter.rect_filled(Rect::from_min_max(pos2(x - 1.5, at.min(zero)), pos2(x + 1.5, at.max(zero))), CornerRadius::same(2), fill);
    painter.hline((x - 5.0)..=(x + 5.0), zero, Stroke::new(1.0, if on { p.text_weak } else { p.text_faint }));
    let hot = on && (resp.hovered() || resp.dragged());
    painter.circle_filled(pos2(x, at), 5.5, if !on { p.text_faint } else if hot { p.text_strong } else { p.text });
    if on { resp.on_hover_cursor(CursorIcon::ResizeVertical) } else { resp }
}

/// The Shortcuts page: the same groups as the shortcuts window (Ctrl+H).
fn shortcuts_grid(ui: &mut Ui) {
    let left = ui.max_rect().left();
    let right = left + ui.available_width();
    let mut y = ui.cursor().top();
    for (name, rows) in super::keys::GROUPS {
        y = super::keys::paint_group(ui, left, right, y, name, rows) + 12.0;
    }
    ui.allocate_space(vec2(right - left, y - ui.cursor().top()));
}

/// A window's own resize edges, for a window without Windows' frame. `salt` keeps each window's
/// apart.
pub(super) fn resize_edges(ctx: &egui::Context, salt: &str, corners_only: bool) {
    use egui::viewport::ResizeDirection as D;
    let (maximized, fullscreen) = ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false)));
    if maximized || fullscreen {
        return;
    }
    let s = ctx.content_rect();
    let c = EDGE * 2.0;
    let zones = [
        (Rect::from_min_max(s.min, pos2(s.left() + c, s.top() + c)), D::NorthWest, CursorIcon::ResizeNwSe),
        (Rect::from_min_max(pos2(s.right() - c, s.top()), pos2(s.right(), s.top() + c)), D::NorthEast, CursorIcon::ResizeNeSw),
        (Rect::from_min_max(pos2(s.left(), s.bottom() - c), pos2(s.left() + c, s.bottom())), D::SouthWest, CursorIcon::ResizeNeSw),
        (Rect::from_min_max(pos2(s.right() - c, s.bottom() - c), s.max), D::SouthEast, CursorIcon::ResizeNwSe),
        (Rect::from_min_max(pos2(s.left() + c, s.top()), pos2(s.right() - c, s.top() + EDGE)), D::North, CursorIcon::ResizeVertical),
        (Rect::from_min_max(pos2(s.left() + c, s.bottom() - EDGE), pos2(s.right() - c, s.bottom())), D::South, CursorIcon::ResizeVertical),
        (Rect::from_min_max(pos2(s.left(), s.top() + c), pos2(s.left() + EDGE, s.bottom() - c)), D::West, CursorIcon::ResizeHorizontal),
        (Rect::from_min_max(pos2(s.right() - EDGE, s.top() + c), pos2(s.right(), s.bottom() - c)), D::East, CursorIcon::ResizeHorizontal),
    ];
    // The corners come first: with `corners_only` the sides don't size the window.
    for (i, (rect, dir, cursor)) in zones.into_iter().enumerate().take(if corners_only { 4 } else { 8 }) {
        egui::Area::new(Id::new((salt, "edge", i))).order(egui::Order::Tooltip).fixed_pos(rect.min).interactable(true).show(ctx, |ui| {
            let (_, resp) = ui.allocate_exact_size(rect.size(), Sense::drag());
            if resp.hovered() || resp.dragged() {
                ctx.set_cursor_icon(cursor);
            }
            if resp.drag_started() {
                ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
            }
        });
    }
}

impl TruffleApp {
    /// Open Settings, with nothing left revealed, half-confirmed or searched from the last time.
    /// Already open: its window comes to the front.
    pub(super) fn open_settings(&mut self) {
        if self.settings_open {
            self.ctx.send_viewport_cmd_to(window_viewport(), ViewportCommand::Focus);
            return;
        }
        self.settings_open = true;
        self.settings_query.clear();
        self.reset_settings_reveals();
        // The play counts are added up again each time About shows.
        self.about.stats_asked = false;
        self.ctx.request_repaint_of(ViewportId::ROOT);
    }

    /// Hide the Last.fm shared secret again and forget a pending "Discard…" confirmation.
    fn reset_settings_reveals(&mut self) {
        self.lastfm_show_secret = false;
        self.lastfm_confirm_discard = false;
    }

    pub(super) fn close_settings(&mut self) {
        // Closed from its own window (Esc, ×, Log out…): the main window gets the focus back, not
        // whatever Windows would pick.
        if self.settings_open && self.ctx.viewport_id() == window_viewport() {
            self.ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
        }
        self.settings_open = false;
        self.about.licences_open = false;
        // Not shown again by itself the next time Settings opens (e.g. while sharing the screen).
        self.reset_settings_reveals();
        // The main window's next pass takes the Settings window away.
        self.ctx.request_repaint_of(ViewportId::ROOT);
    }

    /// Runs every frame: applies what was still waiting once Settings closes.
    pub(super) fn settings_logic(&mut self, ctx: &egui::Context) {
        if self.settings_open {
            self.flush_equalizer(ctx, false);
            return;
        }
        self.flush_equalizer(ctx, true);
        if self.discord_id_edited.is_some() {
            self.commit_discord_id();
        }
    }

    /// Runs every drawn frame of the full window, after its contents and before `settings_logic`
    /// (which then sends what was changed in the window this frame): shows Settings' window while
    /// it is open, made over the middle of the main window.
    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            // Not shown this pass: eframe closes the window.
            self.settings_win = SettingsWindow::default();
            return;
        }
        let mut builder = ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_decorations(false)
            .with_resizable(true)
            .with_min_inner_size(MIN_SIZE)
            .with_taskbar(false)
            .with_visible(false);
        // eframe lists the windows it has; not there yet, the window is made in this pass.
        if !ctx.input(|i| i.raw.viewports.contains_key(&window_viewport())) {
            let parked = self.settings_win.parked;
            if parked {
                // Back with the main window, which keeps the focus.
                builder = builder.with_active(false);
            }
            let main = win::main_window_handle();
            let size = self.settings_size();
            let ppp = ctx.pixels_per_point();
            let target = self.settings_target(ctx, main);
            self.settings_win = SettingsWindow { main, parked, target, ..SettingsWindow::default() };
            // Made at its place, so it doesn't jump. Only then: a position or size in the builder
            // later would move it back each pass.
            builder = match target {
                Some(at) if ppp > 0.0 => builder
                    .with_position(pos2(at.left as f32 / ppp, at.top as f32 / ppp))
                    .with_inner_size(vec2(at.width() as f32 / ppp, at.height() as f32 / ppp)),
                _ => builder.with_inner_size(size),
            };
        }
        ctx.show_viewport_immediate(window_viewport(), builder, |ui, _| self.settings_window_ui(ui));
        if !self.settings_open {
            return;
        }
        // Looked up every pass (a few windows at most), so a window made again is never mistaken
        // for the one before it.
        let hwnd = win::find_window(WINDOW_TITLE);
        if hwnd != self.settings_win.hwnd {
            self.settings_win.hwnd = hwnd;
            self.settings_win.retries = 0;
            if let (Some(hwnd), Some(main)) = (hwnd, self.settings_win.main) {
                win::set_owner(hwnd, main);
            }
        }
        self.show_settings_window_once_placed(ctx);
    }

    /// The Settings window's size (points): as the user left it, else the default.
    fn settings_size(&self) -> Vec2 {
        self.prefs.settings_size.map_or(DEFAULT_SIZE, |[w, h]| vec2(w, h)).max(MIN_SIZE)
    }

    /// Where the Settings window opens: centred over the main window (screen pixels).
    fn settings_target(&self, ctx: &egui::Context, main: Option<isize>) -> Option<ScreenRect> {
        let m = main.filter(|m| !win::is_minimized(*m))?;
        let main_rect = win::visible_rect(m)?;
        let work = win::work_area(m).unwrap_or(main_rect);
        let size = self.settings_size();
        let ppp = ctx.pixels_per_point();
        let px = |points: f32| (points * ppp).round() as i32;
        Some(win::centred_over(main_rect, work, px(size.x), px(size.y)))
    }

    /// Shows the (hidden) Settings window once it sits in its place and has been drawn there.
    fn show_settings_window_once_placed(&mut self, ctx: &egui::Context) {
        // Not known when the window was made (the main window wasn't found yet): work it out now.
        if self.settings_win.target.is_none() && !self.settings_win.shown {
            let main = self.settings_win.main.or_else(win::main_window_handle);
            self.settings_win.main = main;
            self.settings_win.target = self.settings_target(ctx, main);
        }
        let w = &mut self.settings_win;
        if let (Some(hwnd), Some(left)) = (w.hwnd, w.cloaked) {
            if left == 0 {
                w.cloaked = None;
                win::uncloak(hwnd);
            } else {
                w.cloaked = Some(left - 1);
                ctx.request_repaint_of(window_viewport());
                ctx.request_repaint_of(ViewportId::ROOT);
            }
            return;
        }
        let Some(hwnd) = w.hwnd.filter(|_| !w.shown) else { return };
        let in_place = w.target.is_none_or(|to| win::visible_rect(hwnd) == Some(to));
        if in_place || w.retries >= PLACE_RETRIES {
            log::info!("settings window shown at {:?} (wanted {:?})", win::visible_rect(hwnd), w.target);
            w.shown = true;
            // Back from a minimised main window: shown without taking its focus.
            win::show_cloaked(hwnd, !w.parked);
            w.parked = false;
            w.cloaked = Some(2);
        } else if let Some(to) = w.target {
            w.retries += 1;
            win::place_visible(hwnd, to);
        }
        ctx.request_repaint_of(window_viewport());
        ctx.request_repaint_of(ViewportId::ROOT);
    }

    /// The main window is minimised, and Windows hid Settings' window with it. eframe still counts
    /// that window as shown, so it would keep drawing both: it is let go instead, and made again
    /// when the main window is back. Settings stays open meanwhile.
    pub(super) fn park_settings_window(&mut self) {
        if self.settings_open {
            self.settings_win = SettingsWindow { parked: true, ..SettingsWindow::default() };
        }
    }

    /// The Settings window's contents (its own pass, inside the main window's).
    fn settings_window_ui(&mut self, ui: &mut Ui) {
        let p = self.palette;
        // Under everything, so the window never shows black, even in the pass it closes in.
        ui.painter().rect_filled(ui.max_rect(), 0.0, p.main_bg);
        let searching = ui.memory(|m| m.has_focus(Id::new(SEARCH_ID)));
        let (close, find, mut esc) = ui.input_mut(|i| {
            let close = i.viewport().close_requested() || super::keys::take(i, Modifiers::CTRL, Key::W) || super::keys::take(i, Modifiers::CTRL, Key::Comma);
            (close, super::keys::take(i, Modifiers::CTRL, Key::F), super::keys::take(i, Modifiers::NONE, Key::Escape))
        });
        // Ctrl + / Ctrl − / Ctrl 0 here too: the main window applies them (`screens.rs`).
        if let Some(key) = super::screens::take_zoom_key(ui.ctx()) {
            self.screen_zoom.key = Some(key);
            ui.ctx().request_repaint_of(ViewportId::ROOT);
        }
        // Esc while a wallpaper slider or the pad is held puts back what it was, and nothing more;
        // with a menu or the licences open, it closes them (no dialog sees Esc: it is taken here
        // first).
        if esc && self.wallpaper_esc(ui.ctx()) {
            esc = false;
        }
        // An open menu (a drop-down) closes first, and Settings stays.
        if esc && egui::Popup::is_any_open(ui.ctx()) {
            egui::Popup::close_all(ui.ctx());
            esc = false;
        }
        if esc && self.about.licences_open {
            self.about.licences_open = false;
            esc = false;
        }
        let editing_elsewhere = ui.ctx().text_edit_focused() && !searching;
        if close || (esc && !editing_elsewhere && self.settings_query.is_empty()) {
            self.close_settings();
            return;
        }
        if esc && !self.settings_query.is_empty() {
            self.settings_query.clear();
        }
        if find {
            ui.memory_mut(|m| m.request_focus(Id::new(SEARCH_ID)));
        }
        // Its size is kept once it is on screen (not while it is still being placed).
        if self.settings_win.shown
            && let Some(r) = ui.ctx().input(|i| i.viewport().inner_rect)
            && r.width() >= MIN_SIZE.x - 1.0
            && win::moved(self.prefs.settings_size, [r.width(), r.height()])
        {
            self.prefs.settings_size = Some([r.width(), r.height()]);
            self.prefs_changed();
        }
        self.settings_contents(ui);
        self.licences_modal(ui.ctx());
        motion::end_pass(ui.ctx());
        // No Windows frame: a 1 px line keeps it apart from the window behind it.
        ui.painter().rect_stroke(ui.max_rect(), CornerRadius::ZERO, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        resize_edges(ui.ctx(), "settings", false);
    }

    /// Everything inside the Settings window, in `ui.max_rect()`: the section list on the left
    /// (icons only when narrow) and the chosen section, or what the search found, on the right.
    pub(super) fn settings_contents(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let full = ui.max_rect();
        let narrow = full.width() < NARROW_W;
        let nav = Rect::from_min_max(full.min, pos2(full.left() + if narrow { STRIP_W } else { NAV_W }, full.bottom()));
        let body = Rect::from_min_max(pos2(nav.right(), full.top()), full.max);
        let head = Rect::from_min_max(full.min, pos2(full.right(), full.top() + HEAD_H));

        // The head row drags the window (made first, so everything on it stays clickable);
        // double-click maximises, as on the main window.
        let drag = ui.interact(head, Id::new("settings-drag"), Sense::click_and_drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
        }

        let painter = ui.painter().clone();
        painter.rect_filled(nav, CornerRadius::ZERO, p.sidebar_bg);
        painter.vline(nav.right() - 0.5, nav.y_range(), Stroke::new(1.0, p.border));

        let searching = !self.settings_query.trim().is_empty();
        let mut y = full.top();
        if narrow {
            let r = Rect::from_center_size(pos2(nav.center().x, y + HEAD_H / 2.0), vec2(18.0, 18.0));
            svgicon::paint(&painter, r, "gear", p.text, svgicon::STROKE);
            y += HEAD_H;
            for page in Page::ALL {
                let r = Rect::from_center_size(pos2(nav.center().x, y + 19.0), vec2(36.0, 34.0));
                let look = if page == self.settings_page && !searching { Look::Sel } else { Look::Plain };
                if fin::ib(ui, &p, r, Id::new(("settings-strip", page.title())), page.icon(), look, page.title()).clicked() {
                    self.choose_settings_page(page);
                }
                y += 38.0;
            }
        } else {
            painter.text(pos2(nav.left() + 20.0, y + HEAD_H / 2.0), Align2::LEFT_CENTER, "Settings", bold(15.0), p.text_strong);
            y += HEAD_H;
            let sr = Rect::from_min_size(pos2(nav.left() + 12.0, y), vec2(nav.width() - 24.0, 30.0));
            fin::search_box(ui, &p, sr, Id::new(SEARCH_ID), &mut self.settings_query, "Find a setting", "Ctrl+F");
            y += 30.0 + 14.0;
            let first = y;
            let row_at = |k: usize| Rect::from_min_size(pos2(nav.left() + 8.0, first + k as f32 * 36.0), vec2(nav.width() - 16.0, 34.0));
            // The highlight slides to the page picked (and fades while searching).
            let picked = Page::ALL.iter().position(|&page| page == self.settings_page && !searching).map(|k| (k, row_at(k)));
            motion::marker(ui.ctx(), Id::new("settings-nav-mark"), nav.min, picked, motion::TAB, |row, a| {
                painter.rect_filled(row, CornerRadius::same(6), fin::acc(&p, 0.16).gamma_multiply(a));
                painter.rect_filled(Rect::from_min_size(row.min, vec2(3.0, row.height())), CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }, p.accent_ink.gamma_multiply(a));
            });
            for (k, page) in Page::ALL.into_iter().enumerate() {
                let row = row_at(k);
                let resp = ui.interact(row, Id::new(("settings-nav", page.title())), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                let chosen = page == self.settings_page && !searching;
                let hover = motion::color(ui.ctx(), resp.id, if !chosen && resp.hovered() { p.hover_bg } else { Color32::TRANSPARENT }, motion::HOVER);
                if hover.a() > 0 {
                    painter.rect_filled(row, CornerRadius::same(6), hover);
                }
                let icon = Rect::from_min_size(pos2(row.left() + 12.0, row.center().y - 8.0), vec2(16.0, 16.0));
                svgicon::paint(&painter, icon, page.icon(), motion::color(ui.ctx(), resp.id.with("icon"), if chosen { p.accent_ink } else { p.text_weak }, motion::HOVER), svgicon::STROKE);
                let font = if chosen { bold(12.5) } else { medium(12.5) };
                let text_c = motion::color(ui.ctx(), resp.id.with("text"), if chosen { p.text_strong } else { p.text }, motion::HOVER);
                painter.text(pos2(icon.right() + 12.0, row.center().y), Align2::LEFT_CENTER, page.title(), font, text_c);
                if resp.clicked() {
                    self.choose_settings_page(page);
                }
            }
        }

        // The section's title (and the search box, when narrow), and the window's close button.
        let close = Rect::from_min_size(pos2(full.right() - 12.0 - 30.0, full.top() + 11.0), vec2(30.0, 30.0));
        if fin::ib(ui, &p, close, Id::new("settings-close"), "x", Look::Plain, "Close (Esc)").clicked() {
            self.close_settings();
            return;
        }
        let pad = if narrow { 20.0 } else { CONTENT_PAD };
        let title = if searching { "Search" } else { self.settings_page.title() };
        let tx = body.left() + pad;
        painter.text(pos2(tx, full.top() + HEAD_H / 2.0 + 4.0), Align2::LEFT_CENTER, title, heavy(20.0), p.text_strong);
        if narrow {
            let w = (close.left() - 10.0 - (tx + 130.0)).clamp(110.0, 240.0);
            let sr = Rect::from_min_size(pos2(close.left() - 10.0 - w, full.top() + 11.0), vec2(w, 30.0));
            fin::search_box(ui, &p, sr, Id::new(SEARCH_ID), &mut self.settings_query, "Find a setting", "");
        }

        let list = Rect::from_min_max(pos2(body.left(), full.top() + HEAD_H + 10.0), body.max);
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("settings-body").max_rect(list).layout(Layout::top_down(Align::Min)));
        child.set_clip_rect(list.intersect(ui.clip_rect()));
        let width = (list.width() - pad * 2.0).min(CONTENT_MAX_W);
        let key = if searching { "search" } else { self.settings_page.title() };
        ScrollArea::vertical().id_salt(("settings-scroll", key)).auto_shrink(false).show(&mut child, |ui| {
            egui::Frame::new().inner_margin(Margin { left: pad as i8, right: pad as i8, top: 0, bottom: 24 }).show(ui, |ui| {
                ui.set_width(width);
                self.settings_cards(ui);
            });
        });
    }

    fn choose_settings_page(&mut self, page: Page) {
        self.settings_page = page;
        self.settings_query.clear();
        self.reset_settings_reveals();
        self.about.stats_asked = false;
    }

    /// The chosen section's cards, or every setting the search finds, under its section's name.
    fn settings_cards(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let query = fold(self.settings_query.trim());
        if query.is_empty() && self.settings_page == Page::About {
            // About keeps its mockup's own cards and titles.
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for card in CARDS.iter().filter(|c| c.page == Page::About) {
                    about::card(ui, card.title, |ui| {
                        for &item in card.items {
                            self.setting(ui, item);
                        }
                    });
                }
            });
            return;
        }
        if query.is_empty() {
            let page = self.settings_page;
            for card in CARDS.iter().filter(|c| c.page == page) {
                kit::card(ui, &p, card.title, |ui| {
                    for &item in card.items {
                        self.setting(ui, item);
                    }
                });
            }
            return;
        }
        let mut any = false;
        for card in CARDS {
            let hits: Vec<Item> = card.items.iter().copied().filter(|&item| found(&query, card, item)).collect();
            if hits.is_empty() {
                continue;
            }
            any = true;
            let heading = match card.title {
                Some(t) => format!("{}  ›  {t}", card.page.title()),
                None => card.page.title().to_owned(),
            };
            kit::card(ui, &p, Some(&heading), |ui| {
                for item in hits {
                    self.setting(ui, item);
                }
            });
        }
        if !any {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("Nothing found for \"{}\".", self.settings_query.trim())).font(regular(12.5)).color(p.text_weak));
        }
    }

    /// One setting's row (or block).
    fn setting(&mut self, ui: &mut Ui, item: Item) {
        let p = self.palette;
        // Quality and loudness are Spotify's own: nothing to change with YouTube sound.
        let spotify_sound = self.sound_source() == crate::config::SoundSource::Spotify;
        match item {
            Item::SoundSource => self.sound_source_row(ui),
            Item::Quality => self.quality_row(ui, spotify_sound),
            Item::Loudness => {
                if kit::switch_row(ui, &p, "Even out loudness", Some("Applies after a restart"), &mut self.prefs.normalisation, false, spotify_sound) {
                    self.prefs_changed();
                }
            }
            Item::Crossfade => self.crossfade_row(ui, spotify_sound),
            Item::Equalizer => kit::card_body(ui, &p, |ui| self.equalizer_body(ui)),
            Item::Cache => {
                let mut changed = false;
                // Spotify's cache takes a new size after a restart; YouTube songs' at once.
                let note = if spotify_sound { "Covers YouTube songs too. 0 turns it off. Applies after a restart" } else { "Covers YouTube songs too. 0 turns it off" };
                kit::row(ui, &p, "Audio cache", Some(note), false, true, 110.0, |ui| {
                    let resp = ui.add(egui::DragValue::new(&mut self.prefs.audio_cache_mb).range(0..=20_480).speed(16.0).suffix(" MB"));
                    changed |= resp.changed();
                });
                if changed {
                    self.prefs_changed();
                }
            }
            Item::TrueShuffle | Item::NoRepeats | Item::MixArtists | Item::RareFirst | Item::SkipBlocked => self.rule_row(ui, item),
            Item::Theme => self.theme_row(ui),
            Item::MatchAccent => self.match_accent_row(ui),
            Item::Accent => self.accent_row(ui),
            Item::AppIcon => self.app_icon_row(ui),
            Item::Compact => self.compact_row(ui),
            Item::Size => self.size_row(ui),
            Item::Spin => {
                if kit::switch_row(ui, &p, "Spin the record while music plays", None, &mut self.prefs.spin_record, false, true) {
                    self.prefs_changed();
                }
            }
            Item::Motion => self.motion_row(ui),
            Item::PageAnim => self.page_animation_row(ui),
            Item::SongSize => self.song_size_row(ui),
            Item::WideScreens => self.wide_screens_row(ui),
            Item::MoreColumns => {
                if kit::switch_row(ui, &p, "Artist and Date added columns", Some("In song lists, when the window is wide enough"), &mut self.prefs.more_columns, false, true) {
                    self.prefs_changed();
                }
            }
            Item::FillCards => {
                if kit::switch_row(ui, &p, "Bigger cards", Some("On your Profile"), &mut self.prefs.fill_cards, false, true) {
                    self.prefs_changed();
                }
            }
            Item::Banners => self.banners_row(ui),
            Item::Wallpaper => self.wallpaper_rows(ui),
            Item::Colours => kit::card_body(ui, &p, |ui| self.custom_colors_editor(ui)),
            Item::Spotify => self.account_row(ui),
            Item::ListenAlong => {
                // Sharing and the links are on the Friends tab: OPEN goes there (Settings closes,
                // as it would be in front of it).
                kit::row(ui, &p, "Listen along is in the Friends tab.", None, false, true, fin::text_button_w(ui, "Open", false), |ui| {
                    if kit::button(ui, &p, "Open", Kind::Secondary).on_hover_text("Open the Friends tab").clicked() {
                        self.close_settings();
                        self.show_friends();
                    }
                });
            }
            Item::Editing => kit::card_body(ui, &p, |ui| self.webapi_section(ui)),
            Item::Lastfm => kit::card_body(ui, &p, |ui| self.lastfm_section(ui)),
            Item::Discord => self.discord_section(ui),
            Item::KeepRunning | Item::Tray | Item::StartWithWindows | Item::MediaKeys | Item::MiniOnTop | Item::StartMenu => {
                self.system_row(ui, item);
            }
            Item::Shortcuts => kit::card_body(ui, &p, shortcuts_grid),
            Item::Version | Item::Plays | Item::Account | Item::Helpers | Item::Folder | Item::Details | Item::Credits => {
                self.about_item(ui, item);
            }
        }
    }

    /// Where songs' sound comes from now: a free account always uses YouTube.
    pub(super) fn sound_source(&self) -> crate::config::SoundSource {
        if self.profile.as_ref().and_then(|p| p.premium) == Some(false) {
            return crate::config::SoundSource::YouTube;
        }
        self.prefs.sound_source
    }

    fn sound_source_row(&mut self, ui: &mut Ui) {
        use crate::config::SoundSource;
        let p = self.palette;
        let free = self.profile.as_ref().and_then(|p| p.premium) == Some(false);
        let label = |s: SoundSource| match s {
            SoundSource::Spotify => "Spotify",
            SoundSource::YouTube => "YouTube",
        };
        let note = if free {
            "Your account is free, so songs play from YouTube"
        } else if self.sound_source() == SoundSource::YouTube {
            "The same songs, found on YouTube. Needs yt-dlp"
        } else {
            "Spotify plays the songs (Premium)"
        };
        let mut source = self.sound_source();
        kit::row(ui, &p, "Sound from", Some(note), false, !free, 180.0, |ui| {
            egui::ComboBox::from_id_salt("sound_source").selected_text(label(source)).width(170.0).show_ui(ui, |ui| {
                for s in [SoundSource::Spotify, SoundSource::YouTube] {
                    fin::choice(ui, &mut source, s, label(s));
                }
            });
        });
        if !free && source != self.prefs.sound_source {
            self.prefs.sound_source = source;
            self.send(Command::SetSoundSource(source));
        }
    }

    fn quality_row(&mut self, ui: &mut Ui, enabled: bool) {
        let p = self.palette;
        let label = |b: &str| match b {
            "low" => "Low (96 kbps)",
            "normal" => "Normal (160 kbps)",
            _ => "High (320 kbps)",
        };
        let mut bitrate = self.prefs.bitrate.clone();
        kit::row(ui, &p, "Quality", Some("Applies after a restart"), false, enabled, 180.0, |ui| {
            egui::ComboBox::from_id_salt("bitrate").selected_text(label(&bitrate)).width(170.0).show_ui(ui, |ui| {
                for b in ["low", "normal", "high"] {
                    fin::choice(ui, &mut bitrate, b.to_owned(), label(b));
                }
            });
        });
        if bitrate != self.prefs.bitrate {
            self.prefs.bitrate = bitrate;
            self.prefs_changed();
        }
    }

    fn crossfade_row(&mut self, ui: &mut Ui, enabled: bool) {
        let p = self.palette;
        let mut secs = self.prefs.crossfade_secs.min(12);
        let note = (!enabled).then_some("Not with YouTube sound yet");
        kit::row(ui, &p, "Crossfade", note, false, enabled, 220.0, |ui| {
            let shown = if secs == 0 { "Off".to_owned() } else { format!("{secs} s") };
            ui.add_sized(vec2(36.0, 16.0), egui::Label::new(RichText::new(shown).font(regular(12.0)).color(p.text_weak)));
            let mut v = f32::from(secs);
            if kit::line_slider(ui, &p, &mut v, 0.0..=12.0, 170.0, enabled).changed() {
                secs = v.round() as u8;
            }
        });
        if secs != self.prefs.crossfade_secs {
            self.prefs.crossfade_secs = secs;
            self.send(Command::SetCrossfade(secs));
        }
    }

    /// A shuffle rule, with the side panel's names and notes.
    fn rule_row(&mut self, ui: &mut Ui, item: Item) {
        let p = self.palette;
        let mut rules = self.rules.clone();
        let before = rules.clone();
        let shaped = !rules.truly_random;
        let blocked = match self.blocked.len() {
            0 => "Never play blocked songs".to_owned(),
            1 => "Never play blocked songs. 1 is blocked".to_owned(),
            n => format!("Never play blocked songs. {n} are blocked"),
        };
        let changed = match item {
            Item::TrueShuffle => {
                let note = if shaped { "The rules below shape the order" } else { "Every order equally likely" };
                kit::switch_row(ui, &p, "True shuffle", Some(note), &mut rules.truly_random, false, true)
            }
            Item::NoRepeats => kit::switch_row(ui, &p, "No repeats", Some("Play every song once first"), &mut rules.no_repeat_until_exhausted, true, shaped),
            Item::MixArtists => kit::switch_row(ui, &p, "Mix artists", Some("Not one artist twice in a row"), &mut rules.no_artist_repeat, true, shaped),
            Item::RareFirst => kit::switch_row(ui, &p, "Rare first", Some("Favour songs you rarely play"), &mut rules.favour_rarely_played, true, shaped),
            _ => kit::switch_row(ui, &p, "Skip blocked", Some(&blocked), &mut rules.skip_blocked, false, true),
        };
        if changed {
            self.set_rules(rules);
            self.note("Shuffle rules changed", Some(super::Undo::Rules(before)));
        }
    }

    fn account_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let who = self
            .profile
            .as_ref()
            .map(|pr| if pr.display_name.is_empty() { pr.username.clone() } else { pr.display_name.clone() })
            .unwrap_or_default();
        let note = format!("Logged in as {who}");
        kit::row(ui, &p, "Spotify account", Some(&note), false, true, 90.0, |ui| {
            if kit::button(ui, &p, "Log out", Kind::Secondary).clicked() {
                self.close_settings();
                self.send(Command::Logout);
            }
        });
    }

    fn webapi_section(&mut self, ui: &mut Ui) {
        let p = self.palette;
        if self.webapi_connected {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("✔ Connected").color(p.accent_ink).strong());
                ui.label(RichText::new("A backup, used only if Spotify refuses a change.").color(p.text_weak));
            });
            if let Some(m) = &self.webapi_message {
                note(ui, m, p.text_weak);
            }
            ui.add_space(4.0);
            if kit::button(ui, &self.palette, "Disconnect", Kind::Secondary).on_hover_text("Forget the editing login on this PC").clicked() {
                self.webapi_busy = true;
                self.webapi_disconnecting = true;
                self.send(Command::DisconnectWebApi);
            }
            ui.add_space(4.0);
            return;
        }

        note(ui, "Not needed: Truffle edits playlists and searches without it. It's only a backup.", p.text);
        ui.add_space(6.0);
        step(ui, 1, p.accent_ink, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Open");
                if ui.link("developer.spotify.com/dashboard").on_hover_text(DASHBOARD_URL).clicked() {
                    open_in_browser(ui.ctx(), DASHBOARD_URL);
                }
                ui.label("and log in.");
            });
        });
        step(ui, 2, p.accent_ink, |ui| {
            ui.label("Create an app (any name). Redirect URI:");
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(REDIRECT_URI).monospace().color(p.text_strong));
                if kit::button(ui, &p, "Copy", Kind::Quiet).on_hover_text("Copy to clipboard").clicked() {
                    ui.ctx().copy_text(REDIRECT_URI.to_owned());
                }
            });
            ui.label("Tick \"Web API\" and save.");
        });
        let mut connect = false;
        step(ui, 3, p.accent_ink, |ui| {
            ui.label("Paste the app's Client ID:");
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 110.0).clamp(160.0, 300.0);
                let resp = text_field(
                    ui,
                    &p,
                    TextEdit::singleline(&mut self.client_id_input)
                        .hint_text("Client ID (32 characters)")
                        .char_limit(64)
                        .desired_width(width)
                        .font(egui::TextStyle::Monospace)
                        .margin(Margin::symmetric(8, 5)),
                );
                let valid = looks_like_client_id(self.client_id_input.trim());
                // Stays usable while a sign-in is pending: a new attempt replaces the old one.
                let clicked = kit::button_enabled(ui, &p, "Connect", Kind::Primary, valid)
                    .on_hover_text("Opens your browser to allow Truffle to edit your playlists")
                    .clicked();
                let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if valid && (clicked || entered) {
                    connect = true;
                }
            });
            let trimmed = self.client_id_input.trim();
            if !trimmed.is_empty() && !looks_like_client_id(trimmed) {
                note(ui, "That isn't a Client ID (32 characters, not the Client secret).", p.error);
            }
        });
        if connect {
            self.webapi_busy = true;
            self.webapi_disconnecting = false;
            self.webapi_message = None;
            self.send(Command::ConnectWebApi { client_id: self.client_id_input.trim().to_owned() });
        }
        if self.webapi_busy {
            let mut cancel = false;
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Spinner::new().color(p.accent_ink));
                ui.label(RichText::new("Approve it in your browser, then come back here.").color(p.text_weak));
                if !self.webapi_disconnecting {
                    cancel = kit::button(ui, &p, "Cancel", Kind::Secondary).clicked();
                }
            });
            if cancel {
                // The answer to a cancel is not an error.
                self.webapi_disconnecting = true;
                self.send(Command::CancelWebApiConnect);
            }
        } else if let Some(m) = &self.webapi_message {
            note(ui, m, p.text_weak);
            // The one fix Spotify's own message doesn't explain.
            if m.contains("not registered") {
                note(ui, "On the dashboard, open your app's User Management and add your Spotify email.", p.text_weak);
            }
        }
        // A saved Client ID whose sign-in no longer works is retried at every start; without a
        // connection there is no Disconnect button, so offer this instead.
        let saved_client_id = self.shared_settings().webapi_client_id.is_some();
        if saved_client_id
            && !self.webapi_busy
            && kit::button(ui, &p, "Turn off editing", Kind::Quiet).on_hover_text("Forget the saved Client ID on this PC").clicked()
        {
            self.webapi_busy = true;
            self.webapi_disconnecting = true;
            self.send(Command::DisconnectWebApi);
        }
        ui.add_space(4.0);
    }

    fn equalizer_body(&mut self, ui: &mut Ui) {
        let p = self.palette;
        // `send_now`: a click-type change (toggle, preset, reset) goes out at once; slider drags
        // are rate-limited in `flush_equalizer`.
        let mut send_now = false;
        let mut edited = false;
        let mut released = false;
        let eq = &mut self.prefs.equalizer;
        ui.horizontal(|ui| {
            if kit::switch(ui, &p, &mut eq.enabled, true).changed() {
                send_now = true;
            }
            ui.label(RichText::new(if eq.enabled { "On" } else { "Off" }).font(medium(12.5)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(eq.enabled, |ui| {
                    if kit::button(ui, &p, "Reset", Kind::Secondary).on_hover_text("All bands back to 0 dB").clicked() {
                        eq::apply_preset(eq, FLAT);
                        send_now = true;
                    }
                    let current = eq::preset_label(eq).to_owned();
                    egui::ComboBox::from_id_salt("eq_preset").selected_text(&current).width(140.0).show_ui(ui, |ui| {
                        for (name, _) in eq::PRESETS {
                            if fin::menu_item(ui, None, name, None, current == name, true).clicked() {
                                eq::apply_preset(eq, name);
                                send_now = true;
                            }
                        }
                        if current == CUSTOM {
                            fin::menu_item(ui, None, CUSTOM, None, true, false);
                        }
                    });
                });
            });
        });
        ui.add_space(6.0);
        // Eleven columns (the preamp and ten bands) and the line between.
        let column = ((ui.available_width() - 24.0) / 11.0 - 2.0).clamp(26.0, EQ_COLUMN_WIDTH);
        ui.add_enabled_ui(eq.enabled, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let r = eq_column(ui, &p, column, &mut eq.preamp_db, eq::PREAMP_RANGE, "Pre")
                    .on_hover_text("Preamp: lowers everything before the bands, so boosted songs don't distort");
                if r.changed() {
                    eq.preset = CUSTOM.to_owned();
                    edited = true;
                }
                released |= r.drag_stopped();
                ui.add_space(8.0);
                let (sep, _) = ui.allocate_exact_size(vec2(1.0, EQ_SLIDER_LENGTH + 44.0), Sense::hover());
                ui.painter().vline(sep.center().x, sep.y_range(), Stroke::new(1.0, p.border));
                ui.add_space(8.0);
                let labels = eq::band_labels();
                for (i, label) in labels.iter().enumerate() {
                    let r = eq_column(ui, &p, column, &mut eq.gains_db[i], eq::GAIN_RANGE, label)
                        .on_hover_text(format!("{} Hz: {}", EQ_BANDS_HZ[i] as u32, eq::fmt_db(eq.gains_db[i])));
                    if r.changed() {
                        eq.preset = CUSTOM.to_owned();
                        edited = true;
                    }
                    released |= r.drag_stopped();
                }
            });
        });
        // Only while it can actually crackle: a band boosted more than the preamp takes off.
        let boost = eq.gains_db.iter().copied().fold(0.0_f32, f32::max);
        if eq.enabled && boost > 0.0 && eq.preamp_db > -boost + 0.01 {
            note(ui, "Boosted bands can crackle on loud songs: lower Pre to fix it.", p.text_weak);
        }
        if send_now || edited || released {
            self.eq_dirty = true;
        }
        if send_now || released {
            // Forces the send in `flush_equalizer` this frame.
            self.eq_last_sent = None;
        }
    }

    /// Sends pending equalizer changes: right away when forced (a click or the end of a drag), and
    /// at most every `EQ_SEND_INTERVAL` while a slider is being dragged.
    fn flush_equalizer(&mut self, ctx: &egui::Context, force: bool) {
        if !self.eq_dirty {
            return;
        }
        let since = self.eq_last_sent.map(|t| t.elapsed());
        if force || since.is_none_or(|s| s >= EQ_SEND_INTERVAL) {
            self.eq_dirty = false;
            self.eq_last_sent = Some(Instant::now());
            eq::sanitize(&mut self.prefs.equalizer);
            self.send(Command::SetEqualizer(self.prefs.equalizer.clone()));
        } else if let Some(since) = since {
            ctx.request_repaint_after(EQ_SEND_INTERVAL.saturating_sub(since));
        }
    }

    /// The System section's rows: playing on after closing, the tray, starting with Windows, the
    /// media keys, the mini player and the Start menu.
    fn system_row(&mut self, ui: &mut Ui, item: Item) {
        let p = self.palette;
        let mut changed = false;
        match item {
            Item::KeepRunning => {
                // Only the case that would surprise: closing still quits.
                let note = (self.prefs.keep_running && !self.keeps_running()).then_some("Needs the tray icon, else closing quits");
                changed |= kit::switch_row(ui, &p, "Keep playing after closing the window", note, &mut self.prefs.keep_running, false, true);
            }
            Item::Tray => {
                let note = (self.prefs.tray_icon && !self.shell.platform.tray_available()).then_some("Couldn't be shown. Switch it off and on to try again");
                changed |= kit::switch_row(ui, &p, "Tray icon", note, &mut self.prefs.tray_icon, false, true);
            }
            Item::StartWithWindows => {
                let pending = self.shell.start_with_windows_pending;
                let mut start = pending.unwrap_or(self.prefs.start_with_windows);
                let note = if self.prefs.tray_icon { "Starts hidden in the tray" } else { "Starts minimised" };
                if kit::switch_row(ui, &p, "Start with Windows", Some(note), &mut start, false, pending.is_none()) {
                    self.set_start_with_windows(start);
                }
            }
            Item::MediaKeys => {
                changed |= kit::switch_row(ui, &p, "Media keys and Windows' media controls", None, &mut self.prefs.media_controls, false, true);
            }
            Item::MiniOnTop => {
                let mut on_top = self.prefs.mini_player_on_top;
                if kit::switch_row(ui, &p, "Keep the mini player on top", None, &mut on_top, false, true) {
                    let ctx = ui.ctx().clone();
                    self.set_mini_on_top(&ctx, on_top);
                }
            }
            _ => {
                let busy = self.shell.shortcut_busy;
                kit::row(ui, &p, "Start menu shortcut", None, false, true, 120.0, |ui| {
                    if kit::button_enabled(ui, &p, "Add", Kind::Secondary, !busy).clicked() {
                        self.create_start_menu_shortcut();
                    }
                });
            }
        }
        if changed {
            self.prefs_changed();
            self.apply_platform_settings();
        }
    }

    fn discord_section(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let status = self.prefs.discord_status.then(|| self.shell.platform.discord_status());
        let note = status.as_ref().map(ToString::to_string);
        if kit::switch_row(ui, &p, "Show what I'm listening to", note.as_deref(), &mut self.prefs.discord_status, false, true) {
            if self.discord_id_edited.is_some() {
                self.commit_discord_id();
            }
            self.prefs_changed();
            self.apply_platform_settings();
        }
        if status.is_some() {
            // The connection changes on its own (Discord starting or closing): keep the text fresh.
            ui.ctx().request_repaint_after(DISCORD_STATUS_REFRESH);
        }
        kit::card_body(ui, &p, |ui| {
            // Truffle brings its own Discord application; one of the user's own only changes the
            // name shown, so its setup stays folded away.
            let own = self.prefs.discord_app_id.as_deref().is_some_and(|id| id.trim() != crate::platform::TRUFFLE_DISCORD_APP_ID);
            egui::CollapsingHeader::new(RichText::new("Show another name than \"Truffle\"").color(p.text_weak))
                .id_salt("discord_own_app")
                .default_open(own)
                .show(ui, |ui| self.discord_setup(ui));
        });
    }

    fn discord_setup(&mut self, ui: &mut Ui) {
        let p = self.palette;
        step(ui, 1, p.accent_ink, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Open");
                if ui.link("discord.com/developers/applications").on_hover_text(DISCORD_APPS_URL).clicked() {
                    open_in_browser(ui.ctx(), DISCORD_APPS_URL);
                }
                ui.label("and log in.");
            });
        });
        step(ui, 2, p.accent_ink, |ui| {
            ui.label("Make a New Application. Its name is what friends see.");
        });
        step(ui, 3, p.accent_ink, |ui| {
            ui.label("Paste its Application ID:");
            let width = (ui.available_width() - 8.0).clamp(160.0, 260.0);
            let resp = text_field(
                ui,
                &p,
                TextEdit::singleline(&mut self.discord_id_input)
                    .hint_text("Application ID")
                    .char_limit(24)
                    .desired_width(width)
                    .font(egui::TextStyle::Monospace)
                    .margin(Margin::symmetric(8, 5)),
            );
            if resp.changed() {
                self.discord_id_edited = Some(Instant::now());
            }
            if let Some(at) = self.discord_id_edited {
                let waited = at.elapsed();
                if resp.lost_focus() || waited >= DISCORD_ID_SETTLE {
                    self.commit_discord_id();
                } else {
                    ui.ctx().request_repaint_after(DISCORD_ID_SETTLE - waited);
                }
            }
            let typed = self.discord_id_input.trim();
            // With the status on, the status line explains a wrong ID.
            if !self.prefs.discord_status && !typed.is_empty() && !looks_like_discord_id(typed) {
                note(ui, "That isn't an Application ID (a number of about 18 digits).", p.error);
            }
        });
    }

    /// Applies the typed Discord Application ID (empty clears it).
    fn commit_discord_id(&mut self) {
        self.discord_id_edited = None;
        let typed = self.discord_id_input.trim();
        let id = (!typed.is_empty()).then(|| typed.to_owned());
        if id != self.prefs.discord_app_id {
            self.prefs.discord_app_id = id;
            self.prefs_changed();
            self.apply_platform_settings();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_is_in_a_card_once() {
        let mut seen = Vec::new();
        for card in CARDS {
            for &item in card.items {
                assert!(!seen.contains(&item), "{item:?} twice");
                seen.push(item);
            }
        }
        assert_eq!(seen.len(), 46);
    }

    #[test]
    fn every_section_has_cards() {
        for page in Page::ALL {
            assert!(CARDS.iter().any(|c| c.page == page), "{page:?}");
        }
    }

    #[test]
    fn the_search_finds_settings_by_their_words() {
        let card = |item: Item| CARDS.iter().find(|c| c.items.contains(&item)).unwrap();
        assert!(found("eq", card(Item::Equalizer), Item::Equalizer));
        assert!(found("bass", card(Item::Equalizer), Item::Equalizer));
        assert!(found("monitor", card(Item::Size), Item::Size));
        // The section's name finds everything in it.
        assert!(found("audio", card(Item::Crossfade), Item::Crossfade));
        // Every word must match.
        assert!(found("start windows", card(Item::StartWithWindows), Item::StartWithWindows));
        assert!(!found("start banana", card(Item::StartWithWindows), Item::StartWithWindows));
        assert!(!found("scrobble", card(Item::Theme), Item::Theme));
        // Wide screens, by either spelling and by what it does.
        for words in ["ultrawide", "centred", "centered", "wide screen", "fill", "split columns", "layout width"] {
            assert!(found(words, card(Item::WideScreens), Item::WideScreens), "{words}");
        }
        assert!(!found("ultrawide", card(Item::Theme), Item::Theme));
    }

    #[test]
    fn sections_are_found_by_name() {
        assert_eq!(Page::from_word("audio"), Some(Page::Audio));
        assert_eq!(Page::from_word("Accounts"), Some(Page::Accounts));
        assert_eq!(Page::from_word("nope"), None);
    }
}
