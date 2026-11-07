//! Fonts, colours and widget styling.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
use egui::{Color32, CornerRadius, FontData, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme, Visuals};

use super::accent::{contrast_ratio, ensure_contrast, luminance};
use super::util::{contrast_text, mix};
use crate::config::{CustomColors, ThemeMode};

/// Inter SemiBold (600): headings, titles and emphasised text.
pub const BOLD: &str = "truffle-bold";
/// Inter Medium (500): nav rows, tabs, chips, names under covers.
pub const MEDIUM: &str = "truffle-medium";
/// Inter Bold (700): the big page titles and counts.
pub const HEAVY: &str = "truffle-heavy";
/// Segoe UI (400), Bold (700) and Black (900): the Now playing view, drawn in Windows' own font as in
/// its design (Inter where Segoe UI is missing).
pub const SEGOE: &str = "truffle-segoe";
pub const SEGOE_BOLD: &str = "truffle-segoe-bold";
pub const SEGOE_BLACK: &str = "truffle-segoe-black";

/// Segoe UI (400).
pub fn segoe(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEGOE.into()))
}

/// Segoe UI Bold (700).
pub fn segoe_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEGOE_BOLD.into()))
}

/// Segoe UI Black (900).
pub fn segoe_black(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEGOE_BLACK.into()))
}

/// Inter SemiBold (600).
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

/// Inter Medium (500).
#[allow(dead_code)] // the Final build's next steps (nav rows, tabs, chips) use it
pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MEDIUM.into()))
}

/// Inter Bold (700).
pub fn heavy(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(HEAVY.into()))
}

/// Inter Regular (400).
pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// Preset accent colours offered in the side panel.
pub const ACCENT_PRESETS: [([u8; 3], &str); 8] = [
    ([29, 185, 84], "Green"),
    ([30, 144, 255], "Blue"),
    ([138, 92, 246], "Purple"),
    ([236, 72, 153], "Pink"),
    ([239, 68, 68], "Red"),
    ([249, 115, 22], "Orange"),
    ([234, 179, 8], "Yellow"),
    ([20, 184, 166], "Teal"),
];

/// Colours derived from the theme mode, the accent and any custom colours; used for custom
/// painting and for egui's own widget styling.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub accent: Color32,
    pub on_accent: Color32,
    /// The accent as words, icons and lines: moved as little as needed to read on every background
    /// (on the dark theme, the presets stay as they are).
    pub accent_ink: Color32,
    pub sidebar_bg: Color32,
    pub main_bg: Color32,
    pub bar_bg: Color32,
    pub raised_bg: Color32,
    pub placeholder: Color32,
    pub field_bg: Color32,
    pub field_border: Color32,
    pub hover_bg: Color32,
    pub selected_bg: Color32,
    /// The open page's row or card in the sidebar: a soft lift.
    pub chosen_bg: Color32,
    pub text: Color32,
    pub text_strong: Color32,
    pub text_weak: Color32,
    pub text_faint: Color32,
    pub error: Color32,
    /// Warnings (OTHER LENGTH): amber.
    pub warn: Color32,
    // egui widget styling.
    pub widget_bg: Color32,
    pub widget_hover_bg: Color32,
    pub widget_active_bg: Color32,
    pub border: Color32,
    pub faint_bg: Color32,
    pub extreme_bg: Color32,
    pub text_edit_bg: Color32,
    pub window_stroke: Color32,
}

/// Whether any custom colour is set.
pub fn has_custom_colors(c: &CustomColors) -> bool {
    c.background.is_some() || c.panel.is_some() || c.text.is_some() || c.text_weak.is_some() || c.hover.is_some()
}

fn rgb(v: [u8; 3]) -> Color32 {
    Color32::from_rgb(v[0], v[1], v[2])
}

/// Windows' mode for apps as last read, for "Same as Windows": `UNREAD`, `LIGHT` or `DARK`.
static WINDOWS_MODE: AtomicU8 = AtomicU8::new(UNREAD);
const UNREAD: u8 = 0;
const LIGHT: u8 = 1;
const DARK: u8 = 2;

/// Reads Windows' light or dark mode for apps again (one quick registry read). Returns true when
/// it changed since the last reading.
pub fn reread_windows_mode() -> bool {
    // A Windows that doesn't say gets Truffle's usual dark.
    let now = if crate::platform::windows_apps_dark().unwrap_or(true) { DARK } else { LIGHT };
    let before = WINDOWS_MODE.swap(now, Ordering::Relaxed);
    before != UNREAD && before != now
}

/// Whether `mode` shows the dark palette. "Same as Windows" reads Windows' mode the first time it
/// is needed; `reread_windows_mode` keeps it current after that.
pub fn shows_dark(mode: ThemeMode) -> bool {
    match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => {
            if WINDOWS_MODE.load(Ordering::Relaxed) == UNREAD {
                reread_windows_mode();
            }
            WINDOWS_MODE.load(Ordering::Relaxed) == DARK
        }
    }
}

impl Palette {
    /// The palette for a theme mode and accent, with `custom` colours (if any) on top.
    pub fn build(mode: ThemeMode, accent: Color32, custom: Option<&CustomColors>) -> Self {
        let mut p = Self::base(mode);
        if let Some(custom) = custom.filter(|c| has_custom_colors(c)) {
            p.apply_custom(custom);
        }
        p.set_accent(accent);
        p
    }

    fn base(mode: ThemeMode) -> Self {
        if shows_dark(mode) {
            // Glint's layered charcoal: playlists and deck on the background, songs on the
            // toolbar grey, cards and menus on the panel grey.
            Self {
                dark: true,
                accent: Color32::from_rgb(227, 38, 45),
                on_accent: Color32::WHITE,
                accent_ink: Color32::from_rgb(227, 38, 45),
                // The Final design's tokens: tb, bg, panel, btn, input, line, hov, text, bright,
                // muted and dis.
                sidebar_bg: Color32::from_rgb(21, 21, 23),
                main_bg: Color32::from_rgb(14, 14, 16),
                bar_bg: Color32::from_rgb(21, 21, 23),
                raised_bg: Color32::from_rgb(27, 27, 30),
                placeholder: Color32::from_rgb(35, 35, 39),
                field_bg: Color32::from_rgb(18, 18, 20),
                field_border: Color32::from_rgb(42, 42, 47),
                hover_bg: Color32::from_rgb(32, 32, 36),
                selected_bg: Color32::from_rgb(36, 36, 40),
                chosen_bg: Color32::from_rgb(38, 38, 43),
                text: Color32::from_rgb(232, 232, 234),
                text_strong: Color32::WHITE,
                text_weak: Color32::from_rgb(141, 141, 150),
                text_faint: Color32::from_rgb(74, 74, 80),
                error: Color32::from_rgb(248, 113, 113),
                warn: Color32::from_rgb(224, 176, 90),
                widget_bg: Color32::from_rgb(35, 35, 39),
                widget_hover_bg: Color32::from_rgb(44, 44, 49),
                widget_active_bg: Color32::from_rgb(54, 54, 60),
                border: Color32::from_rgb(42, 42, 47),
                faint_bg: Color32::from_rgb(24, 24, 27),
                extreme_bg: Color32::from_rgb(17, 17, 19),
                text_edit_bg: Color32::from_rgb(17, 17, 19),
                window_stroke: Color32::from_rgb(42, 42, 47),
            }
        } else {
            Self {
                dark: false,
                accent: Color32::from_rgb(227, 38, 45),
                on_accent: Color32::WHITE,
                accent_ink: Color32::from_rgb(227, 38, 45),
                sidebar_bg: Color32::from_rgb(233, 233, 236),
                main_bg: Color32::from_rgb(245, 245, 247),
                bar_bg: Color32::from_rgb(238, 238, 241),
                raised_bg: Color32::WHITE,
                placeholder: Color32::from_rgb(220, 220, 225),
                field_bg: Color32::WHITE,
                field_border: Color32::from_rgb(214, 214, 220),
                hover_bg: Color32::from_rgb(225, 225, 230),
                selected_bg: Color32::from_rgb(230, 230, 235),
                chosen_bg: Color32::WHITE,
                text: Color32::from_rgb(29, 29, 31),
                text_strong: Color32::BLACK,
                text_weak: Color32::from_rgb(107, 107, 115),
                text_faint: Color32::from_rgb(160, 160, 168),
                error: Color32::from_rgb(200, 30, 30),
                warn: Color32::from_rgb(154, 100, 0),
                // Dark enough to show on the sidebar's grey too.
                widget_bg: Color32::from_rgb(224, 224, 229),
                widget_hover_bg: Color32::from_rgb(214, 214, 220),
                widget_active_bg: Color32::from_rgb(202, 202, 209),
                border: Color32::from_rgb(224, 224, 228),
                faint_bg: Color32::from_rgb(240, 240, 243),
                extreme_bg: Color32::WHITE,
                text_edit_bg: Color32::WHITE,
                window_stroke: Color32::from_rgb(214, 214, 220),
            }
        }
    }

    /// The palette for parts drawn on something dark in both themes (a banner's shade, the record
    /// player): the dark one, with the same accent.
    pub fn on_dark(&self) -> Self {
        if self.dark { *self } else { Self::build(ThemeMode::Dark, self.accent, None) }
    }

    fn set_accent(&mut self, accent: Color32) {
        self.accent = accent;
        self.on_accent = contrast_text(accent);
        // See-through, as in Glint, so a wallpaper shows under the selected row. On the main
        // background it looks the same as mixing the two.
        self.selected_bg = accent.gamma_multiply(if self.dark { 0.16 } else { 0.12 });
        // Everywhere the accent marks something: rows (hovered, selected and picked too), the
        // panels, cards and menus, text boxes, buttons that are on (in a box or a tinted one).
        let tint = |bg: Color32, a: f32| bg.blend(accent.gamma_multiply(a));
        let bgs = [
            self.main_bg,
            self.sidebar_bg,
            self.bar_bg,
            self.raised_bg,
            self.chosen_bg,
            self.hover_bg,
            self.faint_bg,
            self.field_bg,
            self.widget_bg,
            self.widget_hover_bg,
            self.main_bg.blend(self.selected_bg),
            tint(self.main_bg, 0.10),
            tint(self.main_bg, 0.16),
            tint(self.sidebar_bg, 0.16),
        ];
        self.accent_ink = ensure_contrast(accent, &bgs, 3.0);
    }

    /// Lays the user's colours over the base theme. Colours the user didn't pick are derived from
    /// the ones they did, and all text is nudged until it stays readable on the backgrounds.
    fn apply_custom(&mut self, c: &CustomColors) {
        if let Some(bg) = c.background.map(rgb) {
            self.main_bg = bg;
            self.dark = luminance(bg) < 0.2;
            let dark = self.dark;
            // "Away from the background": lighter on dark themes, darker on light ones.
            let shade = |amount: f32| mix(bg, if dark { Color32::WHITE } else { Color32::BLACK }, amount);
            let raise = |amount: f32| if dark { mix(bg, Color32::WHITE, amount) } else { mix(bg, Color32::WHITE, 0.8) };
            self.raised_bg = raise(0.09);
            self.chosen_bg = raise(0.1);
            self.field_bg = raise(0.03);
            self.extreme_bg = raise(0.04);
            self.text_edit_bg = raise(0.05);
            self.placeholder = shade(0.12);
            self.field_border = shade(if dark { 0.2 } else { 0.18 });
            self.hover_bg = shade(if dark { 0.08 } else { 0.07 });
            self.faint_bg = shade(0.025);
            self.window_stroke = shade(0.13);
            self.widget_bg = shade(0.1);
            self.widget_hover_bg = shade(0.15);
            self.widget_active_bg = shade(0.2);
            self.border = shade(0.15);
            // Panels sit a little darker than the main area, as in the base themes.
            self.sidebar_bg = mix(bg, Color32::BLACK, if dark { 0.38 } else { 0.04 });
            self.bar_bg = mix(bg, Color32::BLACK, if dark { 0.22 } else { 0.065 });
            if c.text.is_none() {
                self.text = if dark { Color32::from_rgb(222, 222, 226) } else { Color32::from_rgb(28, 28, 32) };
                self.text_strong = if dark { Color32::WHITE } else { Color32::BLACK };
            }
            if c.text_weak.is_none() {
                self.text_weak = mix(self.text, bg, 0.35);
            }
        }
        if let Some(panel) = c.panel.map(rgb) {
            self.sidebar_bg = panel;
            self.bar_bg = panel;
        }
        if let Some(text) = c.text.map(rgb) {
            self.text = text;
            self.text_strong = mix(text, if self.dark { Color32::WHITE } else { Color32::BLACK }, 0.6);
        }
        if let Some(weak) = c.text_weak.map(rgb) {
            self.text_weak = weak;
        }
        if c.background.is_some() || c.text_weak.is_some() || c.text.is_some() {
            self.text_faint = mix(self.text_weak, self.main_bg, 0.4);
        }
        if let Some(hover) = c.hover.map(rgb) {
            self.hover_bg = hover;
        }

        // Rows are drawn on the hover colour while the pointer is over them, with the same text.
        let bgs = [self.main_bg, self.sidebar_bg, self.bar_bg, self.raised_bg];
        let with_hover = [self.main_bg, self.sidebar_bg, self.bar_bg, self.raised_bg, self.chosen_bg, self.hover_bg];
        // When no colour reads on every background (a light panel on a dark theme), the song list
        // and the cards win.
        let essential = [self.main_bg, self.raised_bg];
        self.text = readable_on(self.text, &with_hover, &essential, 4.5);
        self.text_strong = readable_on(self.text_strong, &with_hover, &essential, 4.5);
        self.text_weak = readable_on(self.text_weak, &with_hover, &essential, 3.0);
        self.text_faint = ensure_contrast(self.text_faint, &bgs, 1.8);
        self.error = ensure_contrast(self.error, &[self.main_bg, self.raised_bg], 3.0);
        self.warn = ensure_contrast(self.warn, &[self.main_bg, self.raised_bg], 4.5);
        // A hover colour so far from the backgrounds that no text colour reads on both: bring it
        // closer to the main background (where the text reads) until the text reads on it too.
        let texts = [(self.text, 4.5), (self.text_strong, 4.5), (self.text_weak, 3.0)];
        self.hover_bg = background_for(self.hover_bg, self.main_bg, &texts);
    }
}

/// `fg` adjusted to read on every colour in `all`, or, if no text colour can, on `essential`.
fn readable_on(fg: Color32, all: &[Color32], essential: &[Color32], min_ratio: f32) -> Color32 {
    let candidate = ensure_contrast(fg, all, min_ratio);
    if all.iter().all(|&bg| contrast_ratio(candidate, bg) >= min_ratio) {
        candidate
    } else {
        ensure_contrast(fg, essential, min_ratio)
    }
}

/// Moves `bg` towards `toward`, as little as needed, until every `(text, ratio)` reads on it.
fn background_for(bg: Color32, toward: Color32, texts: &[(Color32, f32)]) -> Color32 {
    const STEPS: u32 = 20;
    (0..=STEPS)
        .map(|step| mix(bg, toward, step as f32 / STEPS as f32))
        .find(|&c| texts.iter().all(|&(text, ratio)| contrast_ratio(text, c) >= ratio))
        .unwrap_or(toward)
}

/// A text box with a visible border and an accent outline while focused.
pub fn text_field(ui: &mut egui::Ui, p: &Palette, edit: egui::TextEdit<'_>) -> egui::Response {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.selection.stroke = Stroke::new(1.5, p.accent_ink);
        v.selection.bg_fill = p.accent.gamma_multiply(0.35);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.field_border);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.text_faint);
        ui.add(edit.background_color(p.field_bg))
    })
    .inner
}

fn windows_fonts_dir() -> PathBuf {
    std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("Fonts")
}

/// Inter (the Final design's font: 400, 500, 600 and 700, bundled under the SIL Open Font Licence), with
/// Segoe UI and egui's own fonts behind it for any character Inter lacks.
pub fn install_fonts(ctx: &egui::Context) {
    static REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
    static MEDIUM_TTF: &[u8] = include_bytes!("../../assets/fonts/Inter-Medium.ttf");
    static SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
    static BOLD_TTF: &[u8] = include_bytes!("../../assets/fonts/Inter-Bold.ttf");

    let mut defs = egui::FontDefinitions::default();
    let mut fallback = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Ok(bytes) = std::fs::read(windows_fonts_dir().join("segoeui.ttf")) {
        defs.font_data.insert("segoe-ui".into(), Arc::new(FontData::from_owned(bytes)));
        fallback.insert(0, "segoe-ui".into());
    }
    for (name, bytes, family) in [
        ("inter-400", REGULAR, FontFamily::Proportional),
        ("inter-500", MEDIUM_TTF, FontFamily::Name(MEDIUM.into())),
        ("inter-600", SEMIBOLD, FontFamily::Name(BOLD.into())),
        ("inter-700", BOLD_TTF, FontFamily::Name(HEAVY.into())),
    ] {
        defs.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
        let mut list = fallback.clone();
        list.insert(0, name.into());
        defs.families.insert(family, list);
    }
    // The Now playing view's Segoe UI weights, each with Inter's nearest behind it.
    for (name, file, family, inter) in [
        ("segoe-ui", "segoeui.ttf", SEGOE, "inter-400"),
        ("segoe-ui-700", "segoeuib.ttf", SEGOE_BOLD, "inter-700"),
        ("segoe-ui-900", "seguibl.ttf", SEGOE_BLACK, "inter-700"),
    ] {
        if !defs.font_data.contains_key(name)
            && let Ok(bytes) = std::fs::read(windows_fonts_dir().join(file))
        {
            defs.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
        }
        let mut list = fallback.clone();
        list.retain(|f| f != name);
        list.insert(0, inter.into());
        if defs.font_data.contains_key(name) {
            list.insert(0, name.into());
        }
        defs.families.insert(FontFamily::Name(family.into()), list);
    }
    ctx.set_fonts(defs);
}

/// Applies the palette (theme, accent, custom colours) and the density to egui's styles.
pub fn apply_style(ctx: &egui::Context, p: &Palette, compact: bool) {
    let theme = if p.dark { Theme::Dark } else { Theme::Light };
    ctx.set_theme(theme);
    ctx.data_mut(|d| d.insert_temp(palette_id(), *p));
    let visuals = build_visuals(p);
    ctx.style_mut_of(theme, |s| {
        s.visuals = visuals;
        s.text_styles = [
            (TextStyle::Small, regular(11.5)),
            (TextStyle::Body, regular(13.0)),
            (TextStyle::Button, regular(13.0)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            (TextStyle::Heading, bold(22.0)),
        ]
        .into();
        s.interaction.selectable_labels = false;
        // A control lights up only with the mouse on it (egui counts 5 points round it as on it, so a
        // button next to the mouse looked hovered).
        s.interaction.interact_radius = 0.0;
        if compact {
            s.spacing.item_spacing = egui::vec2(7.0, 4.0);
            s.spacing.button_padding = egui::vec2(8.0, 3.0);
            s.spacing.interact_size.y = 22.0;
        } else {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(10.0, 5.0);
            s.spacing.interact_size.y = 24.0;
        }
        s.spacing.slider_rail_height = 3.0;
        // Menus (`.menu`): 4 inside, rows packed (each row is 28 tall).
        s.spacing.menu_margin = Margin::same(4);
        s.spacing.window_margin = Margin::same(14);
    });
}

fn palette_id() -> egui::Id {
    egui::Id::new("truffle-palette")
}

/// The palette `apply_style` last set, for parts drawn with only a `Ui` at hand (menus, dialogs).
/// Before that, the plain palette of egui's theme.
pub fn palette(ctx: &egui::Context) -> Palette {
    ctx.data(|d| d.get_temp(palette_id())).unwrap_or_else(|| {
        let mode = if ctx.theme() == Theme::Dark { ThemeMode::Dark } else { ThemeMode::Light };
        Palette::build(mode, rgb(ACCENT_PRESETS[0].0), None)
    })
}

fn build_visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    let radius = CornerRadius::same(6);
    v.panel_fill = p.main_bg;
    v.window_fill = p.raised_bg;
    v.faint_bg_color = p.faint_bg;
    v.extreme_bg_color = p.extreme_bg;
    v.text_edit_bg_color = Some(p.text_edit_bg);
    v.code_bg_color = p.placeholder;
    v.hyperlink_color = p.accent_ink;
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0, p.on_accent);
    v.slider_trailing_fill = true;
    v.error_fg_color = p.error;
    // Hints in text boxes.
    v.weak_text_color = Some(p.text_weak);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_stroke = Stroke::new(1.0, p.window_stroke);
    v.window_shadow = Shadow { offset: [0, 10], blur: 28, spread: 0, color: Color32::from_black_alpha(if p.dark { 140 } else { 50 }) };
    v.popup_shadow = Shadow { offset: [0, 16], blur: 44, spread: 0, color: Color32::from_black_alpha(if p.dark { 166 } else { 50 }) };
    v.image_loading_spinners = false;

    let (inactive_bg, hovered_bg, active_bg, border) = (p.widget_bg, p.widget_hover_bg, p.widget_active_bg, p.border);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.main_bg;
    w.noninteractive.weak_bg_fill = p.main_bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = radius;
    for (wv, bg, fg) in [
        (&mut w.inactive, inactive_bg, p.text),
        (&mut w.hovered, hovered_bg, p.text_strong),
        (&mut w.active, active_bg, p.text_strong),
        (&mut w.open, inactive_bg, p.text_strong),
    ] {
        wv.bg_fill = bg;
        wv.weak_bg_fill = bg;
        wv.fg_stroke = Stroke::new(1.5, fg);
        wv.corner_radius = radius;
        wv.bg_stroke = Stroke::NONE;
    }
    w.hovered.bg_stroke = Stroke::new(1.0, border);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.hovered.expansion = 0.0;
    w.active.expansion = 0.0;
    v
}

/// Writing systems whose glyphs are not in Segoe UI or egui's bundled fonts, and the styled letters
/// people put in their names. Their Windows fonts are large (tens of MB), so each is loaded on a
/// background thread only once text needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Script {
    Cjk,
    Hangul,
    Thai,
    Indic,
    Styled,
}

impl Script {
    const ALL: [Script; 5] = [Script::Cjk, Script::Hangul, Script::Thai, Script::Indic, Script::Styled];

    fn bit(self) -> u8 {
        1 << (self as u8)
    }

    fn of(c: char) -> Option<Script> {
        match c as u32 {
            0x0900..=0x0DFF => Some(Script::Indic),
            0x0E00..=0x0E7F => Some(Script::Thai),
            0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => Some(Script::Hangul),
            0x2E80..=0x2FFF | 0x3000..=0x30FF | 0x31F0..=0x31FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FA1F => Some(Script::Cjk),
            // Letterlike symbols (ℰ ℍ), circled letters (Ⓔ ⓜ) and maths letters (𝓂 𝖊 𝕖).
            0x2100..=0x214F | 0x2460..=0x24FF | 0x1D400..=0x1D7FF => Some(Script::Styled),
            _ => None,
        }
    }

    fn candidates(self) -> &'static [&'static str] {
        match self {
            Script::Cjk => &["msyh.ttc", "YuGothM.ttc", "meiryo.ttc", "simsun.ttc", "msgothic.ttc"],
            Script::Hangul => &["malgun.ttf", "gulim.ttc"],
            Script::Thai => &["LeelawUI.ttf", "leelawad.ttf", "tahoma.ttf"],
            Script::Indic => &["Nirmala.ttc", "Nirmala.ttf", "mangal.ttf"],
            Script::Styled => &["seguisym.ttf"],
        }
    }
}

/// A loaded font file for a script: `(file name, bytes)`, or `None` if none was found.
type FontLoad = (Script, Option<(String, Vec<u8>)>);

/// Lazily installs fallback fonts for non-Latin scripts the first time such text shows up.
pub struct ScriptFonts {
    requested: u8,
    tx: Sender<FontLoad>,
    rx: Receiver<FontLoad>,
}

impl ScriptFonts {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { requested: 0, tx, rx }
    }

    pub fn all_requested(&self) -> bool {
        Script::ALL.iter().all(|s| self.requested & s.bit() != 0)
    }

    /// Looks at `text` and starts loading any missing script fonts it needs.
    pub fn scan(&mut self, ctx: &egui::Context, text: &str) {
        if self.all_requested() || text.is_ascii() {
            return;
        }
        for c in text.chars() {
            if let Some(script) = Script::of(c)
                && self.requested & script.bit() == 0
            {
                self.requested |= script.bit();
                self.load(ctx, script);
                if self.all_requested() {
                    return;
                }
            }
        }
    }

    pub fn scan_all<'a>(&mut self, ctx: &egui::Context, texts: impl IntoIterator<Item = &'a str>) {
        for t in texts {
            if self.all_requested() {
                return;
            }
            self.scan(ctx, t);
        }
    }

    fn load(&self, ctx: &egui::Context, script: Script) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("truffle-font".into()).spawn(move || {
            let dir = windows_fonts_dir();
            let found = script
                .candidates()
                .iter()
                .find_map(|name| std::fs::read(dir.join(name)).ok().map(|bytes| ((*name).to_owned(), bytes)));
            if found.is_none() {
                log::info!("no system font found for {script:?} text");
            }
            let _ = tx.send((script, found));
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            log::warn!("could not start font loader: {e}");
        }
    }

    /// Installs fonts that finished loading. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(load) = self.rx.try_recv() {
            install(ctx, load);
        }
    }
}

/// Every font family Truffle draws with: egui's two, the Inter weights and the Now playing view's
/// Segoe UI ones.
fn families() -> [FontFamily; 8] {
    let name = |n: &str| FontFamily::Name(n.into());
    [FontFamily::Proportional, FontFamily::Monospace, name(MEDIUM), name(BOLD), name(HEAVY), name(SEGOE), name(SEGOE_BOLD), name(SEGOE_BLACK)]
}

/// Puts a loaded script font behind every family (the lyrics and the Home cards are in Medium and
/// Bold: missing there, their Japanese showed as squares).
fn install(ctx: &egui::Context, (script, found): FontLoad) {
    let Some((name, bytes)) = found else { return };
    log::info!("installing {name} for {script:?} text");
    let families = families().into_iter().map(|family| InsertFontFamily { family, priority: FontPriority::Lowest }).collect();
    ctx.add_font(FontInsert::new(&format!("system-{name}"), FontData::from_owned(bytes), families));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{fin, kit, sidebar};

    const GREEN: Color32 = Color32::from_rgb(29, 185, 84);

    #[test]
    fn hovered_rows_stay_readable() {
        // A near-white hover on the dark theme (white text), a mid grey one (weak text too dark
        // for it), and a dark hover on the light theme.
        let cases = [
            (ThemeMode::Dark, [235, 235, 235]),
            (ThemeMode::Dark, [90, 90, 90]),
            (ThemeMode::Light, [40, 40, 40]),
        ];
        for (mode, hover) in cases {
            let custom = CustomColors { hover: Some(hover), ..Default::default() };
            let p = Palette::build(mode, GREEN, Some(&custom));
            for (text, ratio) in [(p.text, 4.5), (p.text_strong, 4.5), (p.text_weak, 3.0)] {
                assert!(contrast_ratio(text, p.hover_bg) >= ratio, "{hover:?}: {text:?} on {:?}", p.hover_bg);
                assert!(contrast_ratio(text, p.main_bg) >= ratio, "{hover:?}: {text:?} on the background");
            }
        }
        // A hover colour that works is kept as picked.
        let custom = CustomColors { hover: Some([50, 54, 70]), ..Default::default() };
        assert_eq!(Palette::build(ThemeMode::Dark, GREEN, Some(&custom)).hover_bg, Color32::from_rgb(50, 54, 70));
    }

    #[test]
    fn no_custom_colours_keeps_the_base_theme() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let plain = Palette::build(mode, GREEN, None);
            let empty = Palette::build(mode, GREEN, Some(&CustomColors::default()));
            assert_eq!(format!("{plain:?}"), format!("{empty:?}"));
            assert_eq!(plain.accent, GREEN);
        }
    }

    #[test]
    fn same_as_windows_is_the_light_or_the_dark_palette() {
        let wanted = if shows_dark(ThemeMode::System) { ThemeMode::Dark } else { ThemeMode::Light };
        let system = Palette::build(ThemeMode::System, GREEN, None);
        assert_eq!(format!("{system:?}"), format!("{:?}", Palette::build(wanted, GREEN, None)));
        reread_windows_mode();
        assert!(!reread_windows_mode(), "reading again without a change in Windows is no change");
    }

    #[test]
    fn custom_colours_keep_text_readable() {
        // A light background on the dark theme with text left alone; unreadable text colours; a
        // light panel on the dark theme.
        let cases = [
            CustomColors { background: Some([240, 236, 220]), ..Default::default() },
            CustomColors {
                background: Some([20, 30, 60]),
                text: Some([30, 40, 70]),
                text_weak: Some([25, 35, 65]),
                ..Default::default()
            },
            CustomColors { panel: Some([250, 250, 250]), ..Default::default() },
        ];
        for custom in &cases {
            for mode in [ThemeMode::Dark, ThemeMode::Light] {
                let p = Palette::build(mode, GREEN, Some(custom));
                assert!(contrast_ratio(p.text, p.main_bg) >= 4.5, "{custom:?} {mode:?}: {:?} on {:?}", p.text, p.main_bg);
                assert!(contrast_ratio(p.text_weak, p.main_bg) >= 3.0, "{custom:?} {mode:?}");
            }
        }
        let light = Palette::build(ThemeMode::Dark, GREEN, Some(&cases[0]));
        assert!(!light.dark, "a light background switches to light widgets");
    }

    /// Japanese, Korean, Thai and Hindi show in every font family (the lyrics and the Home cards
    /// use Inter Medium and Bold, not only Regular), with the Windows fonts loaded as the app loads
    /// them: from the text of a controller event. So do the styled letters of names such as
    /// "ℰ𝓂𝓂𝒶" (squares before). Scripts without a font on this PC are skipped.
    #[test]
    fn script_fonts_reach_every_family() {
        use crate::model::{Event, LyricLine};
        use crate::ui::kit::headless::pass;

        const SAMPLES: [(Script, &[&str]); 5] = [
            (Script::Cjk, &["おやすみ", "日本語"]),
            (Script::Hangul, &["한국어"]),
            (Script::Thai, &["ภาษาไทย"]),
            (Script::Indic, &["हिन्दी"]),
            (Script::Styled, &["ℰ𝓂𝓂𝒶", "𝖊𝖒𝖒𝖆", "𝐞𝐦𝐦𝐚", "𝕖𝕞𝕞𝕒", "𝚎𝚖𝚖𝚊", "Ⓔⓜⓜⓐ", "ℍ"]),
        ];
        if !windows_fonts_dir().join("msyh.ttc").exists() {
            eprintln!("skipped: no msyh.ttc on this PC");
            return;
        }
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        pass(&ctx, Vec::new(), |_| {});

        let lines = SAMPLES.iter().flat_map(|(_, s)| s.iter()).map(|w| LyricLine { start_ms: 0, words: (*w).to_owned() }).collect();
        let event = Event::Lyrics { uri: "spotify:track:1".into(), lines: Some(Arc::new(lines)), synced: false };
        let mut fonts = ScriptFonts::new();
        fonts.scan(&ctx, &format!("{event:?}"));
        assert!(fonts.all_requested(), "every script was asked for");
        let mut found = Vec::new();
        for _ in Script::ALL {
            let load = fonts.rx.recv_timeout(std::time::Duration::from_secs(60)).expect("the loader answers");
            if load.1.is_some() {
                found.push(load.0);
            }
            install(&ctx, load);
        }
        assert!(found.contains(&Script::Cjk));
        pass(&ctx, Vec::new(), |_| {});

        let families = ctx.fonts(|f| f.families());
        for name in [MEDIUM, BOLD, HEAVY] {
            assert!(families.contains(&FontFamily::Name(name.into())), "{name} is set up");
        }
        // Only the samples' own script: egui says no to any character its family's square comes from
        // (Latin in Monospace), though it shows.
        let mut squares = Vec::new();
        for (script, samples) in SAMPLES.iter().filter(|(s, _)| found.contains(s)) {
            for family in &families {
                for s in *samples {
                    if !ctx.fonts_mut(|f| f.has_glyphs(&FontId::new(13.0, family.clone()), s)) {
                        squares.push(format!("{script:?} {s} in {family:?}"));
                    }
                }
            }
        }
        assert!(squares.is_empty(), "shows as squares: {squares:#?}");
    }

    /// Both themes, each with the default accent, the presets, a very light and a very dark one.
    fn palettes() -> Vec<Palette> {
        let mut accents: Vec<Color32> = ACCENT_PRESETS.iter().map(|&(c, _)| rgb(c)).collect();
        accents.extend([Color32::from_rgb(227, 38, 45), Color32::from_rgb(250, 248, 225), Color32::from_rgb(18, 10, 40)]);
        [ThemeMode::Dark, ThemeMode::Light].into_iter().flat_map(|mode| accents.iter().map(move |&a| Palette::build(mode, a, None))).collect()
    }

    fn named(p: &Palette) -> String {
        format!("{} theme, accent {:?}", if p.dark { "dark" } else { "light" }, p.accent)
    }

    /// Every background words are drawn on; see-through ones as they show on the page.
    fn backgrounds(p: &Palette) -> Vec<(&'static str, Color32)> {
        vec![
            ("the page", p.main_bg),
            ("the sidebar", p.sidebar_bg),
            ("the player bar", p.bar_bg),
            ("a card", p.raised_bg),
            ("a faint box", p.faint_bg),
            ("a text box", p.field_bg),
            ("a hovered row", p.hover_bg),
            ("the open page's row", p.chosen_bg),
            ("a button", p.widget_bg),
            ("a hovered button", p.widget_hover_bg),
            ("a held button", p.widget_active_bg),
            ("a selected row", p.main_bg.blend(p.selected_bg)),
            ("a picked song", p.main_bg.blend(fin::acc(p, 0.10))),
            ("a chip that is on", p.main_bg.blend(fin::acc(p, 0.16))),
            ("a chip on the sidebar", p.sidebar_bg.blend(fin::acc(p, 0.16))),
        ]
    }

    #[test]
    fn words_read_on_every_background() {
        for p in palettes() {
            for (bg_name, bg) in backgrounds(&p) {
                for (word, fg, min) in [("text", p.text, 4.5), ("bright text", p.text_strong, 4.5), ("muted text", p.text_weak, 3.0)] {
                    let c = contrast_ratio(fg, bg);
                    assert!(c >= min, "{}: {word} on {bg_name} is {c:.2}", named(&p));
                }
                // Accent words, icons and lines. A held button is egui's own, with bright words.
                if bg_name != "a held button" {
                    let c = contrast_ratio(p.accent_ink, bg);
                    assert!(c >= 3.0, "{}: the accent on {bg_name} is {c:.2}", named(&p));
                }
            }
            // Greyed words (a song that can't play) stay in sight.
            for bg in [p.main_bg, p.sidebar_bg, p.raised_bg] {
                assert!(contrast_ratio(p.text_faint, bg) >= 1.8, "{}", named(&p));
            }
            // OTHER LENGTH, in a dialog.
            for bg in [p.main_bg, p.raised_bg] {
                let c = contrast_ratio(p.warn, bg);
                assert!(c >= 4.5, "{}: the warning is {c:.2}", named(&p));
            }
        }
        // The dark theme's presets, and the red Truffle starts with, keep their colour.
        for c in ACCENT_PRESETS.iter().map(|&(c, _)| rgb(c)).chain([Color32::from_rgb(227, 38, 45)]) {
            assert_eq!(Palette::build(ThemeMode::Dark, c, None).accent_ink, c);
        }
    }

    #[test]
    fn rows_and_tabs_pick_readable_words() {
        for p in palettes() {
            // A song row: on the page, hovered (and the playing one), picked.
            for bg in [p.main_bg, p.hover_bg, p.main_bg.blend(fin::acc(&p, 0.10))] {
                for current in [false, true] {
                    let (title, artists) = fin::row_ink(&p, false, current);
                    assert!(contrast_ratio(title, bg) >= 4.5, "{}: title, playing {current}", named(&p));
                    assert!(contrast_ratio(artists, bg) >= 3.0, "{}: artists, playing {current}", named(&p));
                }
            }
            for bg in [p.main_bg, p.sidebar_bg, p.raised_bg] {
                for (on, hovered) in [(false, false), (false, true), (true, false), (true, true)] {
                    assert!(contrast_ratio(fin::tab_ink(&p, on, hovered), bg) >= 3.0, "{}: tab, on {on}", named(&p));
                }
            }
        }
    }

    /// Two passes of `draw` in `p`'s theme: each piece of text the second painted, and each icon (as
    /// "an icon"), with the colour it shows in.
    fn painted_words(p: &Palette, draw: impl Fn(&mut egui::Ui)) -> Vec<(String, Color32)> {
        let ctx = egui::Context::default();
        install_fonts(&ctx);
        apply_style(&ctx, p, false);
        let raw = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0))), ..Default::default() };
        // Nothing to draw on: the fonts' texture is let go of.
        ctx.run_ui(raw(), &draw).textures_delta.clear();
        let mut out = ctx.run_ui(raw(), &draw);
        out.textures_delta.clear();
        let mut shapes: Vec<egui::Shape> = out.shapes.into_iter().map(|c| c.shape).collect();
        let mut words = Vec::new();
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(more) => shapes.extend(more),
                egui::Shape::Text(t) => {
                    for section in &t.galley.job.sections {
                        let text = t.galley.job.text[section.byte_range.start.0..section.byte_range.end.0].trim();
                        let own = section.format.color;
                        let color = t.override_text_color.unwrap_or(if own == Color32::PLACEHOLDER { t.fallback_color } else { own });
                        if !text.is_empty() {
                            words.push((text.to_owned(), color));
                        }
                    }
                }
                // An icon: its picture, tinted.
                egui::Shape::Mesh(m) if m.texture_id != egui::TextureId::default() => {
                    if let Some(v) = m.vertices.first() {
                        words.push(("an icon".to_owned(), v.color));
                    }
                }
                _ => {}
            }
        }
        words
    }

    #[test]
    fn widgets_paint_readable_words() {
        use egui::{Id, Rect, pos2, vec2};
        type Draw = Box<dyn Fn(&mut egui::Ui, &Palette)>;
        let row = Rect::from_min_size(pos2(8.0, 10.0), vec2(280.0, 34.0));
        let field = Rect::from_min_size(pos2(8.0, 10.0), vec2(280.0, 30.0));
        let button = Rect::from_min_size(pos2(8.0, 8.0), vec2(30.0, 30.0));
        let accents = [Color32::from_rgb(227, 38, 45), Color32::from_rgb(250, 248, 225), Color32::from_rgb(18, 10, 40)];
        for p in [ThemeMode::Light, ThemeMode::Dark].into_iter().flat_map(|mode| accents.map(|a| Palette::build(mode, a, None))) {
            let cases: Vec<(&str, Color32, Draw)> = vec![
                ("a nav row", p.sidebar_bg, Box::new(move |ui, p| { let _ = sidebar::nav_row(ui, p, row, "album", "Albums", false, None); })),
                ("the open nav row", p.chosen_bg, Box::new(move |ui, p| { let _ = sidebar::nav_row(ui, p, row, "home", "Home", true, None); })),
                (
                    "an empty search box",
                    p.field_bg,
                    Box::new(move |ui, p| { let _ = fin::search_box(ui, p, field, Id::new("s"), &mut String::new(), "Filter playlists", "Ctrl+L"); }),
                ),
                ("a search box", p.field_bg, Box::new(move |ui, p| { let _ = fin::search_box(ui, p, field, Id::new("s"), &mut "care".to_owned(), "Filter songs", "Ctrl+F"); })),
                (
                    "the title bar's search box",
                    p.on_dark().field_bg,
                    Box::new(move |ui, p| { let _ = fin::search_box(ui, &p.on_dark(), field, Id::new("s"), &mut String::new(), "Search songs", "Ctrl+K"); }),
                ),
                ("a button that is on, in a box", p.widget_hover_bg, Box::new(move |ui, p| { let _ = fin::ib(ui, p, button, Id::new("b"), "grid", fin::Look::OnBox, ""); })),
                ("a button that is on", p.bar_bg, Box::new(move |ui, p| { let _ = fin::ib(ui, p, button, Id::new("b"), "grid", fin::Look::On, ""); })),
                ("a liked heart", p.main_bg, Box::new(move |ui, p| { let _ = fin::heart(ui, p, button, Id::new("h"), true, ""); })),
                ("a chip", p.widget_bg, Box::new(|ui, p| { let _ = fin::chip(ui, p, pos2(8.0, 30.0), Id::new("c"), "Playlists", None, false, false); })),
                ("a chip that is on", p.main_bg.blend(fin::acc(&p, 0.16)), Box::new(|ui, p| { let _ = fin::chip(ui, p, pos2(8.0, 30.0), Id::new("c"), "Mine", None, true, false); })),
                ("a button", p.widget_bg, Box::new(|ui, p| { let _ = fin::text_button(ui, p, pos2(8.0, 10.0), Id::new("b"), "Reshuffle", Some("shuffle"), false, true); })),
                ("a menu row", p.raised_bg, Box::new(|ui, _| { let _ = fin::menu_item(ui, Some("plus"), "Add to queue", Some("Ctrl+Q"), false, true); })),
                ("a menu row that is on", p.raised_bg, Box::new(|ui, _| { let _ = fin::menu_item(ui, None, "Shuffle", None, true, true); })),
                (
                    "a dialog's head",
                    p.raised_bg,
                    Box::new(|ui, _| {
                        fin::dialog_head(ui, "Find duplicates", Some("Songs that are in it twice."));
                        fin::field_label(ui, "Name");
                    }),
                ),
                ("a dialog's text box", p.field_bg, Box::new(|ui, p| { let _ = fin::field(ui, p, egui::TextEdit::singleline(&mut String::new()).hint_text("Playlist name")); })),
                ("a settings row", p.sidebar_bg, Box::new(|ui, p| { let _ = kit::switch_row(ui, p, "Show the record", Some("Ctrl+D hides it"), &mut true, false, true); })),
            ];
            for (what, bg, draw) in &cases {
                let words = painted_words(&p, |ui| draw(ui, &p));
                assert!(!words.is_empty(), "{}: {what} painted nothing", named(&p));
                for (text, color) in words {
                    let c = contrast_ratio(bg.blend(color), *bg);
                    assert!(c >= 3.0, "{}: {text:?} in {what} is {c:.2} ({color:?} on {bg:?})", named(&p));
                }
            }
        }
    }

    #[test]
    fn the_title_bars_hint_is_the_dark_themes() {
        // egui paints a hint in the theme's weak colour, whatever colour it is given.
        let p = Palette::build(ThemeMode::Light, Color32::from_rgb(227, 38, 45), None);
        let dark = p.on_dark();
        let field = egui::Rect::from_min_size(egui::pos2(8.0, 10.0), egui::vec2(280.0, 30.0));
        let words = painted_words(&p, |ui| {
            let _ = fin::search_box(ui, &dark, field, egui::Id::new("s"), &mut String::new(), "Search songs", "Ctrl+K");
        });
        assert!(words.contains(&("Search songs".to_owned(), dark.text_weak)), "{words:?}");
    }
}
