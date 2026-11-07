//! Truffle's controls, in the style of the user's Glint app: one rounded shape for everything,
//! CAPITAL button labels, 40x20 switches, painted line icons, thin line sliders, setting cards and
//! text tabs.

use std::ops::RangeInclusive;

use egui::{
    Align, Color32, CornerRadius, CursorIcon, Frame, Layout, Margin, Response,
    Sense, Stroke, Ui, pos2, vec2,
};

use super::theme::{Palette, bold, regular};
use super::fin;
use super::motion;

/// Corner radius of every control (buttons, switches, drop-downs, cards, tabs).
pub const RADIUS: u8 = 6;
pub const BUTTON_HEIGHT: f32 = 28.0;
const SWITCH: egui::Vec2 = egui::vec2(36.0, 20.0);
/// Hit height of the line sliders (the line itself is 3 tall).
const SLIDER_HIT: f32 = 16.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `button` fill; most buttons.
    Secondary,
    /// Accent fill; at most one per view.
    Primary,
    /// Muted words, underlined on hover; for opt-outs and UNDO.
    Quiet,
}

/// A button with a CAPITAL label.
pub fn button(ui: &mut Ui, p: &Palette, label: &str, kind: Kind) -> Response {
    button_enabled(ui, p, label, kind, true)
}

pub fn button_enabled(ui: &mut Ui, p: &Palette, label: &str, kind: Kind, enabled: bool) -> Response {
    if kind == Kind::Quiet {
        let galley = ui.painter().layout_no_wrap(label.to_owned(), super::theme::medium(11.5), Color32::PLACEHOLDER);
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(4.0, 6.0), sense);
        if ui.is_rect_visible(rect) {
            // The words brighten and the underline fades in on hover.
            let h = motion::hover(ui.ctx(), resp.id, enabled && resp.hovered());
            let color = if enabled { motion::mix(p.text_weak, p.text, h) } else { p.text_faint };
            let pos = rect.center() - galley.size() / 2.0;
            if h > 0.0 {
                ui.painter().hline(pos.x..=pos.x + galley.size().x, pos.y + galley.size().y, Stroke::new(1.0, color.gamma_multiply(h)));
            }
            ui.painter().galley(pos, galley, color);
        }
        return if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    }
    // The Final design's `.b`: 28 tall, 14 either side, 11 px semibold capitals spaced .05em; the
    // primary one in the accent. Greyed: the plain grey with dim text.
    let primary = kind == Kind::Primary;
    let color = if !enabled { p.text_faint } else if primary { p.on_accent } else { p.text };
    let galley = fin::spaced(ui, &label.to_uppercase(), bold(11.0), Color32::PLACEHOLDER, 0.55);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 28.0, BUTTON_HEIGHT), sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let fill = match (primary && enabled, hovered) {
            (true, true) => fin::brighten(p.accent, 1.07),
            (true, false) => p.accent,
            (false, true) => p.widget_hover_bg,
            (false, false) => p.widget_bg,
        };
        // Hover and greying fade; held, it shrinks a little.
        let fill = motion::color(ui.ctx(), resp.id.with("fill"), fill, motion::HOVER);
        let color = motion::color(ui.ctx(), resp.id.with("text"), color, motion::HOVER);
        let scale = motion::press(ui.ctx(), resp.id, enabled && resp.is_pointer_button_down_on());
        motion::scaled(ui, rect.center(), scale, || {
            ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), fill);
            ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
        });
    }
    if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp }
}

/// The Final design's switch (`.sw`, 36x20). Returns its response; `changed()` when flipped.
pub fn switch(ui: &mut Ui, p: &Palette, on: &mut bool, enabled: bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(SWITCH, Sense::hover());
    if fin::switch(ui, p, rect, resp.id.with("sw"), *on, enabled, "") {
        *on = !*on;
        resp.mark_changed();
    }
    resp
}

/// Whether things may move (Windows' "Animation effects"); else they only fade. See `motion`.
pub fn animations_on() -> bool {
    super::motion::moves()
}

/// A thin line slider (3 tall, 16 tall to grab): accent fill, knob while hovered or dragged; the
/// knob pressed on is grabbed where it is, a press elsewhere jumps it there (as in Glint).
/// `changed()` while the value moves; `drag_stopped()` on release. Its `id` is the one
/// [`fin::let_go`] takes.
pub fn line_slider(ui: &mut Ui, p: &Palette, value: &mut f32, range: RangeInclusive<f32>, width: f32, enabled: bool) -> Response {
    let (rect, alloc) = ui.allocate_exact_size(vec2(width.max(20.0), SLIDER_HIT), Sense::hover());
    let (lo, hi) = (*range.start(), *range.end());
    let span = (hi - lo).max(f32::EPSILON);
    let (mut resp, at) = fin::knob_line(ui, p, rect, alloc.id.with("line"), ((*value - lo) / span).clamp(0.0, 1.0), enabled);
    if let Some(t) = at {
        let v = lo + t * span;
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    resp
}

/// A card for settings rows: panel fill, 1-wide frame. An optional heading sits above it.
pub fn card<R>(ui: &mut Ui, p: &Palette, heading: Option<&str>, add: impl FnOnce(&mut Ui) -> R) -> R {
    if let Some(h) = heading {
        ui.add_space(4.0);
        let g = fin::spaced(ui, &h.to_uppercase(), bold(10.5), p.text_weak, 0.95);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), g.size().y + 10.0), Sense::hover());
        ui.painter().galley(pos2(rect.left() + 2.0, rect.top()), g, p.text_weak);
    }
    let r = Frame::new()
        .fill(p.sidebar_bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
        .inner;
    ui.add_space(18.0);
    r
}

/// Free-form content inside a card, with the row padding (and a line above unless it's first).
pub fn card_body<R>(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui) -> R) -> R {
    separator_unless_first(ui, p);
    Frame::new()
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            add(ui)
        })
        .inner
}

fn separator_unless_first(ui: &mut Ui, p: &Palette) {
    let top = ui.cursor().top();
    if top > ui.min_rect().top() + 0.5 {
        let x = ui.max_rect().x_range();
        let _ = p;
        ui.painter().hline(x, top, Stroke::new(1.0, p.border));
    }
}

/// A settings row: title, optional muted note and a control on the right `control_w` wide.
/// `indent` rows belong to the row above; they're greyed when `enabled` is false.
#[allow(clippy::too_many_arguments)]
pub fn row<R>(
    ui: &mut Ui,
    p: &Palette,
    title: &str,
    note: Option<&str>,
    indent: bool,
    enabled: bool,
    control_w: f32,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    separator_unless_first(ui, p);
    let left = if indent { 40 } else { 16 };
    Frame::new()
        .inner_margin(Margin { left, right: 16, top: 12, bottom: 12 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(26.0);
            // As tall as a button from the start, so a lone title sits level with its control.
            ui.allocate_ui_with_layout(vec2(ui.available_width(), BUTTON_HEIGHT), Layout::left_to_right(Align::Center), |ui| {
                let text_w = (ui.available_width() - control_w - 24.0).max(80.0);
                // Placed as one block, so it is centred on the row like the control.
                row_text(ui, title, note, enabled, text_w);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add_enabled_ui(enabled, control).inner).inner
            })
            .inner
        })
        .inner
}

/// A taller settings row (`height` inside its padding), as Glint's Nearby and Position rows: the
/// title and note at its top, a control on the right `control_w` wide, centred down the row.
#[allow(clippy::too_many_arguments)]
pub fn tall_row<R>(
    ui: &mut Ui,
    p: &Palette,
    title: &str,
    note: Option<&str>,
    enabled: bool,
    height: f32,
    control_w: f32,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    separator_unless_first(ui, p);
    Frame::new()
        .inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 12 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
            let text_w = (rect.width() - control_w - 24.0).max(80.0);
            let text = egui::Rect::from_min_size(rect.min, vec2(text_w, height));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(text).layout(Layout::top_down(Align::Min)));
            row_text(&mut left, title, note, enabled, text_w);
            let place = egui::Rect::from_min_max(pos2(text.right() + 24.0, rect.top()), rect.max);
            let mut right = ui.new_child(egui::UiBuilder::new().max_rect(place).layout(Layout::right_to_left(Align::Center)));
            right.add_enabled_ui(enabled, control).inner
        })
        .inner
}

/// A row's title, and its muted note under it, as one block `text_w` wide.
fn row_text(ui: &mut Ui, title: &str, note: Option<&str>, enabled: bool, text_w: f32) {
    let p = crate::ui::theme::palette(ui.ctx());
    let (title_c, note_c) = if enabled { (p.text, p.text_weak) } else { (p.text_faint, p.text_faint) };
    // Laid out first, so the block's height is known before it's placed.
    let title_g = ui.fonts_mut(|f| f.layout(title.to_owned(), super::theme::medium(12.5), title_c, text_w));
    place_row_text(ui, title_g, title_c, note, note_c, text_w);
}

/// A laid-out title and its note under it (3 below, 11.5 regular), placed as one block.
fn place_row_text(ui: &mut Ui, title_g: std::sync::Arc<egui::Galley>, title_c: Color32, note: Option<&str>, note_c: Color32, text_w: f32) {
    let note_g = note.filter(|n| !n.is_empty()).map(|n| ui.fonts_mut(|f| f.layout(n.to_owned(), regular(11.5), note_c, text_w)));
    let th = title_g.size().y;
    let h = th + note_g.as_ref().map_or(0.0, |g| 3.0 + g.size().y);
    let (block, _) = ui.allocate_exact_size(vec2(text_w, h), Sense::hover());
    ui.painter().galley(block.min, title_g, title_c);
    if let Some(g) = note_g {
        ui.painter().galley(block.min + vec2(0.0, th + 3.0), g, note_c);
    }
}

/// A [`line_slider`] with a small tick under it at `standard`, its default (as Glint's sliders),
/// left out while it is at the default; the knob covers it.
#[allow(clippy::too_many_arguments)]
pub fn tick_slider(ui: &mut Ui, p: &Palette, value: &mut f32, range: RangeInclusive<f32>, width: f32, enabled: bool, standard: f32) -> Response {
    // Kept a place under the slider, filled in once the slider is drawn.
    let tick = ui.painter().add(egui::Shape::Noop);
    let (lo, hi) = (*range.start(), *range.end());
    let resp = line_slider(ui, p, value, range, width, enabled);
    let span = (hi - lo).max(f32::EPSILON);
    let (at, standard) = (((*value - lo) / span).clamp(0.0, 1.0), ((standard - lo) / span).clamp(0.0, 1.0));
    if (at - standard).abs() > 0.0005 {
        let r = resp.rect;
        let x = r.left() + r.width() * standard;
        let color = if enabled { p.text_weak.gamma_multiply(0.9) } else { p.text_faint };
        let mark = egui::Rect::from_min_size(pos2((x - 1.0).round(), r.center().y + 4.5), vec2(2.0, 4.0));
        ui.painter().set(tick, egui::Shape::rect_filled(mark, 0.0, color));
    }
    resp
}

/// What a number box did this frame.
#[derive(Debug, Default)]
pub struct Typed {
    /// Typing in it started.
    pub started: bool,
    /// What it says, when typed in this frame.
    pub changed: Option<String>,
    /// What was typed in it when typing ended (Enter, Tab, or clicked away), once.
    pub ended: Option<String>,
}

/// A small box with a number to type in, `width` wide, saying `shown` while it isn't typed in
/// (what is typed is kept meanwhile): `--input`, a 1 px line (the accent while typing), round 6.
/// Typing starts with all of it chosen, so a new number replaces it.
pub fn number_box(ui: &mut Ui, p: &Palette, id: egui::Id, shown: &str, width: f32, enabled: bool) -> Typed {
    let typing = ui.memory(|m| m.has_focus(id));
    let mut text = if typing { ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| shown.to_owned()) } else { shown.to_owned() };
    let frame = Frame::new()
        .fill(p.field_bg)
        .stroke(Stroke::new(1.0, p.field_border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(8, 6));
    let edit = egui::TextEdit::singleline(&mut text)
        .id(id)
        .frame(frame)
        .desired_width(width - 16.0)
        .font(regular(12.0))
        .text_color(if enabled { p.text } else { p.text_faint })
        .vertical_align(Align::Center);
    let resp = ui.add_enabled(enabled, edit);
    if resp.has_focus() {
        ui.painter().rect_stroke(resp.rect, CornerRadius::same(RADIUS), Stroke::new(1.0, p.accent_ink), egui::StrokeKind::Inside);
    }
    let started = resp.gained_focus();
    if started && let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
        let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(text.chars().count()));
        state.cursor.set_char_range(Some(all));
        state.store(ui.ctx(), id);
    }
    if resp.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, text.clone()));
    }
    let changed = (resp.changed() && resp.has_focus()).then(|| text.clone());
    // egui says a box lost the focus on two frames running, and once another box took it this one
    // shows its number again: the text kept while typing says what was typed, and goes with the
    // first.
    let ended = if resp.lost_focus() {
        ui.data_mut(|d| d.remove_temp::<String>(id)).map(|kept| if typing { text } else { kept })
    } else {
        None
    };
    Typed { started, changed, ended }
}

/// A row with a switch. Returns true when it was flipped.
pub fn switch_row(ui: &mut Ui, p: &Palette, title: &str, note: Option<&str>, value: &mut bool, indent: bool, enabled: bool) -> bool {
    row(ui, p, title, note, indent, enabled, SWITCH.x, |ui| switch(ui, p, value, enabled).changed())
}

/// egui passes without a window, for tests: the mouse and keys of each pass, and what it draws.
#[cfg(test)]
pub(super) mod headless {
    use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, Ui, vec2};

    /// One pass of `add` with `events`, on a 600 x 400 screen.
    pub fn pass(ctx: &egui::Context, events: Vec<Event>, add: impl FnMut(&mut Ui)) {
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0))), events, ..Default::default() };
        // Nothing to draw on: the fonts' texture is let go of.
        ctx.run_ui(raw, add).textures_delta.clear();
    }

    /// [`pass`], and what it drew, in the order drawn.
    pub fn drawn(ctx: &egui::Context, events: Vec<Event>, add: impl FnMut(&mut Ui)) -> Vec<egui::epaint::ClippedShape> {
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0))), events, ..Default::default() };
        let mut out = ctx.run_ui(raw, add);
        out.textures_delta.clear();
        std::mem::take(&mut out.shapes)
    }

    pub fn move_to(at: Pos2) -> Vec<Event> {
        vec![Event::PointerMoved(at)]
    }

    /// The left button pressed at `at` (the mouse moved there first).
    pub fn press(at: Pos2) -> Vec<Event> {
        vec![Event::PointerMoved(at), Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]
    }

    pub fn release(at: Pos2) -> Vec<Event> {
        vec![Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]
    }

    pub fn key(key: Key) -> Vec<Event> {
        vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]
    }

    pub fn text(t: &str) -> Vec<Event> {
        vec![Event::Text(t.to_owned())]
    }
}

#[cfg(test)]
mod tests {
    use super::headless::{move_to, pass, press, release};
    use super::*;
    use crate::config::ThemeMode;
    use egui::{Pos2, Rect, UiBuilder};

    /// A 0 to 100 slider, 200 wide from x 100 (its line at y 108): one pass of it.
    fn slider(ctx: &egui::Context, events: Vec<egui::Event>, value: &mut f32) -> Response {
        let p = Palette::build(ThemeMode::Dark, Color32::RED, None);
        let rect = Rect::from_min_size(pos2(100.0, 100.0), vec2(200.0, SLIDER_HIT));
        let mut out = None;
        pass(ctx, events, |ui| {
            out = Some(ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| line_slider(ui, &p, value, 0.0..=100.0, 200.0, true)).inner);
        });
        out.expect("drawn")
    }

    fn at(x: f32) -> Pos2 {
        pos2(x, 108.0)
    }

    #[test]
    fn a_slider_knob_is_grabbed_where_it_is() {
        let ctx = egui::Context::default();
        let mut v = 40.0;
        slider(&ctx, move_to(at(183.0)), &mut v);
        // Pressed 3 points right of the knob (at 180): it stays, then keeps that distance.
        slider(&ctx, press(at(183.0)), &mut v);
        assert!((v - 40.0).abs() < 0.01, "jumped to {v}");
        slider(&ctx, move_to(at(193.0)), &mut v);
        assert!((v - 45.0).abs() < 0.01, "{v}");
        slider(&ctx, release(at(193.0)), &mut v);
        // Elsewhere on the line, the knob jumps to the press.
        slider(&ctx, press(at(250.0)), &mut v);
        assert!((v - 75.0).abs() < 0.01, "{v}");
        // Let go of (Esc): it stops following the mouse until the button is released.
        let r = slider(&ctx, move_to(at(260.0)), &mut v);
        assert!((v - 80.0).abs() < 0.01, "{v}");
        fin::let_go(&ctx, r.id);
        slider(&ctx, move_to(at(290.0)), &mut v);
        assert!((v - 80.0).abs() < 0.01, "followed after Esc: {v}");
        slider(&ctx, release(at(290.0)), &mut v);
        slider(&ctx, press(at(120.0)), &mut v);
        assert!((v - 10.0).abs() < 0.01, "the next press counts again: {v}");
    }
}

