//! The design's small parts, measured from its prototype: icon
//! buttons (`.ib`), the round play button (`.pp`), the thin seek and volume lines (`.seek`) and the search box
//! (`.srch`). Sizes are the prototype's CSS pixels, which are egui points. Each part is drawn at a given
//! rect, so the frame can place it exactly where the prototype's layout puts it.

use egui::{Color32, CornerRadius, CursorIcon, Id, Rect, Response, Sense, Stroke, StrokeKind, TextEdit, Ui, pos2, vec2};

use super::motion;
use super::svgicon::{self, STROKE};
use super::theme::{Palette, palette, regular};

/// The accent at `a` (0-1) opacity, as the prototype's `--acc16`, `--acc28` and `--acc45`.
pub fn acc(p: &Palette, a: f32) -> Color32 {
    p.accent.gamma_multiply(a)
}

/// A tab's words: bright when it is the one picked, the text colour on hover, else muted.
pub fn tab_ink(p: &Palette, on: bool, hovered: bool) -> Color32 {
    if on {
        p.text_strong
    } else if hovered {
        p.text
    } else {
        p.text_weak
    }
}

/// A song row's title and artists colours: greyed when it can't play (or is blocked), the title
/// bright on the playing song.
pub fn row_ink(p: &Palette, dim: bool, current: bool) -> (Color32, Color32) {
    if dim {
        (p.text_faint, p.text_faint)
    } else if current {
        (p.text_strong, p.text_weak)
    } else {
        (p.text, p.text_weak)
    }
}

/// A chosen row or card (the page that is open): a soft lift, a raised fill with a faint edge and
/// no accent bar. The accent is kept for what plays (the record, the playing marks).
pub fn paint_chosen(painter: &egui::Painter, p: &Palette, rect: Rect) {
    let edge = if p.dark { Color32::from_white_alpha(14) } else { p.border };
    painter.rect_filled(rect, CornerRadius::same(6), p.chosen_bg);
    painter.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, edge), StrokeKind::Inside);
}

/// How an icon button looks besides hover: `.on` (accent), `.onb` (accent on a raised square), `.sel`
/// (accent on a tinted square with a bar on its left), `.dim` (greyed, not clickable).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Look {
    #[default]
    Plain,
    On,
    OnBox,
    Sel,
    Dim,
    /// Its menu is open (`.ib.open`): drawn as if hovered.
    Open,
}

impl Look {
    pub fn on(on: bool) -> Self {
        if on { Self::On } else { Self::Plain }
    }

    pub fn on_box(on: bool) -> Self {
        if on { Self::OnBox } else { Self::Plain }
    }
}

/// An icon button (`.ib`: 30×30 with a 16 px icon normally) at `rect`.
pub fn ib(ui: &Ui, p: &Palette, rect: Rect, id: Id, icon: &str, look: Look, tip: &str) -> Response {
    ib_icon(ui, p, rect, id, icon, look, tip, 16.0)
}

#[allow(clippy::too_many_arguments)]
pub fn ib_icon(ui: &Ui, p: &Palette, rect: Rect, id: Id, icon: &str, look: Look, tip: &str, icon_px: f32) -> Response {
    ib_with(ui, p, rect, id, look, tip, |painter, fg| svgicon::paint(painter, Rect::from_center_size(rect.center(), vec2(icon_px, icon_px)), icon, fg, STROKE))
}

/// The heart (`.ib`, accent and filled when liked): it pops when liked and dips when unliked (see
/// [`motion::heart`]). `id` is this heart's, one per song.
pub fn heart(ui: &Ui, p: &Palette, rect: Rect, id: Id, liked: bool, tip: &str) -> Response {
    let (scale, fill) = motion::heart(ui.ctx(), id, liked);
    let icon = Rect::from_center_size(rect.center(), vec2(16.0, 16.0));
    ib_with(ui, p, rect, id, Look::on(liked), tip, |painter, fg| {
        motion::scaled(ui, rect.center(), scale, || paint_heart(painter, icon, fg, p.accent_ink, fill));
    })
}

/// A heart's outline in `line`, filled with `full` as far as `fill` (0-1) has faded in.
pub fn paint_heart(painter: &egui::Painter, icon: Rect, line: Color32, full: Color32, fill: f32) {
    svgicon::paint(painter, icon, "heart", line, STROKE);
    if fill > 0.0 {
        svgicon::paint(painter, icon, "heartf", full.gamma_multiply(fill), STROKE);
    }
}

/// An icon button whose icon `icon(painter, colour)` draws.
fn ib_with(ui: &Ui, p: &Palette, rect: Rect, id: Id, look: Look, tip: &str, icon: impl FnOnce(&egui::Painter, Color32)) -> Response {
    let sense = if look == Look::Dim { Sense::hover() } else { Sense::click() };
    let resp = ui.interact(rect, id, sense);
    let hovered = resp.hovered() && look != Look::Dim;
    let round = CornerRadius::same(6);
    let (fg, bg) = match look {
        Look::Dim => (p.text_faint, None),
        Look::On => (p.accent_ink, hovered.then_some(p.hover_bg)),
        Look::OnBox => (p.accent_ink, Some(p.widget_hover_bg)),
        Look::Sel => (p.accent_ink, Some(acc(p, 0.16))),
        Look::Plain => (if hovered { p.text } else { p.text_weak }, hovered.then_some(p.hover_bg)),
        Look::Open => (p.text, Some(p.hover_bg)),
    };
    // Hover and look changes fade; held, it shrinks a little.
    let fg = motion::color(ui.ctx(), id.with("fg"), fg, motion::HOVER);
    let bg = motion::color(ui.ctx(), id.with("bg"), bg.unwrap_or(Color32::TRANSPARENT), motion::HOVER);
    let sel = motion::toward(ui.ctx(), id.with("sel"), look == Look::Sel, motion::HOVER);
    let scale = motion::press(ui.ctx(), id, look != Look::Dim && resp.is_pointer_button_down_on());
    motion::scaled(ui, rect.center(), scale, || {
        let painter = ui.painter();
        if bg.a() > 0 {
            painter.rect_filled(rect, round, bg);
        }
        if sel > 0.0 {
            let bar = Rect::from_min_size(rect.min, vec2(3.0, rect.height()));
            painter.rect_filled(bar, CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }, p.accent_ink.gamma_multiply(sel));
        }
        icon(painter, fg);
    });
    let resp = if look == Look::Dim { resp } else { resp.on_hover_cursor(CursorIcon::PointingHand) };
    if tip.is_empty() { resp } else { resp.on_hover_text(tip) }
}

/// The round accent play / pause button (`.pp`: 36 across with a 15 px icon; `.pp.big`: 52 and 20).
pub fn pp(ui: &Ui, p: &Palette, rect: Rect, id: Id, playing: bool, enabled: bool, tip: &str) -> Response {
    let d = rect.width().min(rect.height());
    let icon_px = if d >= 50.0 { 20.0 } else { 15.0 };
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    // `filter: brightness(1.08)` on hover.
    let fill = if resp.hovered() && enabled { brighten(p.accent, 1.08) } else { p.accent };
    let fill = motion::color(ui.ctx(), id.with("fill"), if enabled { fill } else { fill.gamma_multiply(0.5) }, motion::HOVER);
    let scale = motion::press(ui.ctx(), id, enabled && resp.is_pointer_button_down_on());
    motion::scaled(ui, rect.center(), scale, || {
        ui.painter().circle_filled(rect.center(), d / 2.0, fill);
        let icon = if playing { "pause" } else { "play" };
        svgicon::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(icon_px, icon_px)), icon, p.on_accent, STROKE);
    });
    let resp = if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    resp.on_hover_text(tip)
}

pub fn brighten(c: Color32, k: f32) -> Color32 {
    let f = |v: u8| ((v as f32 * k).round().min(255.0)) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// A press this near the knob of a [`knob_line`] grabs it where it is (as Glint's sliders).
const KNOB_GRAB: f32 = 8.0;

/// A [`knob_line`] held with the mouse: how far right of the knob it was pressed (the knob keeps
/// that distance from the mouse, so nothing jumps), and whether Esc let go of it.
#[derive(Clone, Copy, Debug, Default)]
struct Grab {
    dx: f32,
    let_go: bool,
}

/// A thin line slider (`.seek`) across `rect` (vertically centred in it): 3 px, the filled part in the
/// accent, a 10 px white knob while hovered or held. The whole rect is the grab area. Returns the response
/// and, while the button is held, the value (0-1) under the pointer.
pub fn line(ui: &Ui, p: &Palette, rect: Rect, id: Id, value: f32, enabled: bool) -> (Response, Option<f32>) {
    line_with(ui, p, rect, id, value, enabled, false)
}

/// A [`line`] whose knob, pressed on, is grabbed where it is; a press elsewhere on the line jumps
/// the knob there, as in Glint. [`let_go`] ends the grab before the button is released.
pub fn knob_line(ui: &Ui, p: &Palette, rect: Rect, id: Id, value: f32, enabled: bool) -> (Response, Option<f32>) {
    line_with(ui, p, rect, id, value, enabled, true)
}

/// The [`knob_line`] `id` stops following the mouse until its button is released (Esc while it is
/// held: what it was is put back by its owner).
pub fn let_go(ctx: &egui::Context, id: Id) {
    let key = id.with("grab");
    ctx.data_mut(|d| {
        if let Some(grab) = d.get_temp::<Grab>(key) {
            d.insert_temp(key, Grab { let_go: true, ..grab });
        }
    });
}

fn line_with(ui: &Ui, p: &Palette, rect: Rect, id: Id, value: f32, enabled: bool, grab_knob: bool) -> (Response, Option<f32>) {
    let sense = if enabled { Sense::click_and_drag() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    let bar = Rect::from_center_size(rect.center(), vec2(rect.width(), 3.0));
    let painter = ui.painter();
    painter.rect_filled(bar, CornerRadius::same(2), p.border);
    let mut held = (resp.is_pointer_button_down_on() || resp.dragged()) && enabled;
    let mut dx = 0.0;
    if grab_knob {
        let key = id.with("grab");
        if held {
            // Worked out on the press, then kept while it is held.
            let knob = bar.left() + bar.width() * value.clamp(0.0, 1.0);
            let press = ui.input(|i| i.pointer.press_origin());
            let grab = ui.data_mut(|d| {
                *d.get_temp_mut_or_insert_with(key, || Grab {
                    dx: press.map(|at| at.x - knob).filter(|dx| dx.abs() <= KNOB_GRAB).unwrap_or(0.0),
                    let_go: false,
                })
            });
            dx = grab.dx;
            held = !grab.let_go;
        } else {
            ui.data_mut(|d| d.remove::<Grab>(key));
        }
    }
    let at = if held {
        resp.interact_pointer_pos().map(|pos| ((pos.x - dx - bar.left()) / bar.width().max(1.0)).clamp(0.0, 1.0))
    } else {
        None
    };
    let v = at.unwrap_or(value).clamp(0.0, 1.0);
    let fill = Rect::from_min_max(bar.min, pos2(bar.left() + bar.width() * v, bar.bottom()));
    if v > 0.0 {
        painter.rect_filled(fill, CornerRadius::same(2), p.accent_ink);
    }
    // The knob fades in while hovered or held.
    let knob = motion::hover(ui.ctx(), id, enabled && (resp.hovered() || held));
    if knob > 0.0 {
        let c = pos2(fill.right(), bar.center().y);
        painter.circle_filled(c + vec2(0.0, 1.0), 5.5, Color32::from_black_alpha(150).gamma_multiply(knob));
        painter.circle_filled(c, 5.0, Color32::WHITE.gamma_multiply(knob));
    }
    let resp = if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    (resp, at)
}

/// What the search box did this frame.
pub struct SearchBox {
    pub resp: Response,
}

/// The search box (`.srch`) at `rect` (30 tall): a 13 px magnifier, the text, and a key hint such as
/// "Ctrl+K" on the right (an × to clear when there is text). Its border turns accent while focused.
#[allow(clippy::too_many_arguments)]
pub fn search_box(ui: &mut Ui, p: &Palette, rect: Rect, id: Id, text: &mut String, hint: &str, key: &str) -> SearchBox {
    search_box_with(ui, p, rect, id, text, hint, key, "search")
}

/// The search box with another icon (the song filter's funnel).
#[allow(clippy::too_many_arguments)]
pub fn search_box_with(ui: &mut Ui, p: &Palette, rect: Rect, id: Id, text: &mut String, hint: &str, key: &str, glyph: &str) -> SearchBox {
    // Made first, so the text and the × (made after it) sit on top and get their own clicks.
    let whole = ui.interact(rect, id.with("box"), Sense::click()).on_hover_cursor(CursorIcon::Text);
    let focused = ui.memory(|m| m.has_focus(id));
    let painter = ui.painter().clone();
    painter.rect_filled(rect, CornerRadius::same(6), p.field_bg);
    let border = motion::color(ui.ctx(), id.with("border"), if focused { p.accent_ink } else { p.field_border }, motion::HOVER);
    painter.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, border), StrokeKind::Inside);
    let icon = Rect::from_min_size(pos2(rect.left() + 10.0, rect.center().y - 6.5), vec2(13.0, 13.0));
    svgicon::paint(&painter, icon, glyph, motion::color(ui.ctx(), id.with("glyph"), if focused { p.text_strong } else { p.text_weak }, motion::HOVER), STROKE);

    let mut cleared = false;
    let right = if text.is_empty() {
        if key.is_empty() {
            rect.right() - 8.0
        } else {
            let g = ui.fonts_mut(|f| f.layout_no_wrap(key.to_owned(), regular(10.0), p.text_weak));
            let kw = g.size().x + 10.0;
            let kr = Rect::from_min_size(pos2(rect.right() - 8.0 - kw, rect.center().y - 8.0), vec2(kw, 16.0));
            painter.rect_stroke(kr, CornerRadius::same(4), Stroke::new(1.0, p.border), StrokeKind::Inside);
            painter.galley(pos2(kr.left() + 5.0, kr.center().y - g.size().y / 2.0), g, p.text_weak);
            kr.left()
        }
    } else {
        let xr = Rect::from_min_size(pos2(rect.right() - 8.0 - 13.0, rect.center().y - 6.5), vec2(13.0, 13.0));
        let xresp = ui.interact(xr.expand(3.0), id.with("x"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        svgicon::paint(&painter, xr, "x", motion::color(ui.ctx(), xresp.id, if xresp.hovered() { p.text_strong } else { p.text_weak }, motion::HOVER), STROKE);
        if xresp.on_hover_text("Clear").clicked() {
            cleared = true;
        }
        xr.left()
    };
    let field = Rect::from_min_max(pos2(icon.right() + 8.0, rect.top()), pos2(right - 6.0, rect.bottom()));
    let edit = TextEdit::singleline(text)
        .id(id)
        .frame(egui::Frame::new())
        .hint_text(egui::RichText::new(hint).font(regular(12.0)))
        .font(regular(12.0))
        .text_color(p.text_strong)
        .margin(egui::Margin::symmetric(0, 8))
        .vertical_align(egui::Align::Center);
    // egui paints the hint in the weak text colour, whatever colour it is given; that and the
    // caret follow `p`, which may be the other theme's (the dark title bar in the light theme).
    let resp = ui
        .scope_builder(egui::UiBuilder::new().max_rect(field), |ui| {
            let v = ui.visuals_mut();
            v.weak_text_color = Some(p.text_weak);
            if p.dark != v.dark_mode {
                v.text_cursor.stroke.color = p.text_strong;
            }
            ui.put(field, edit)
        })
        .inner;
    if cleared {
        text.clear();
        resp.request_focus();
    }
    if whole.clicked() {
        resp.request_focus();
    }
    SearchBox { resp }
}

/// A vertical gradient fill of `rect`, `top` to `bottom` colour.
pub fn vgradient(painter: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));
}

/// The design's little turntable (`.tt`), `d` across the record, drawn with its top-left at `origin`: a
/// grooved black record with a soft sheen, the cover (`label`) in the middle turned by `spin` radians, and
/// a steel tonearm at each of `arms`' degrees from straight down (1 parked, 13 to 34 playing, moving in
/// as the song goes on) with its opacity: one, or two as it fades from one place to the other (see
/// `motion::tonearm`). The whole thing is `d * 1.16` wide.
pub fn turntable(painter: &egui::Painter, origin: egui::Pos2, d: f32, label: Option<egui::TextureId>, spin: f32, arms: &[(f32, f32)]) {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let r = d / 2.0;
    let c = origin + vec2(r, r);
    // Shadow under the record, then its 2 px outer ring (#25252a) and 1 px black edge.
    painter.add(egui::Shadow { offset: [0, 8], blur: 22, spread: 0, color: Color32::from_black_alpha(140) }.as_shape(
        Rect::from_center_size(c, vec2(d, d)),
        CornerRadius::same(r.min(255.0) as u8),
    ));
    painter.circle_filled(c, r + 2.0, Color32::from_rgb(37, 37, 42));
    painter.circle_filled(c, r + 1.0, Color32::BLACK);
    // Grooves: #18181b with a 1 px #09090b ring every 2.6 px.
    painter.circle_filled(c, r, Color32::from_rgb(24, 24, 27));
    let mut ring = r - 0.5;
    while ring > r * 0.44 {
        painter.circle_stroke(c, ring, Stroke::new(1.0, Color32::from_rgb(9, 9, 11)));
        ring -= 2.6;
    }
    // The sheen: a conic gradient from 200°, white 9 % at +28° and 6.5 % at +208°.
    let lobe = |deg: f32| -> f32 {
        let a = (deg - 200.0).rem_euclid(360.0);
        let peak = |at: f32, from: f32, to: f32, v: f32| {
            if a >= from && a <= at { v * (a - from) / (at - from) } else if a > at && a <= to { v * (to - a) / (to - at) } else { 0.0 }
        };
        peak(28.0, 0.0, 62.0, 0.09) + peak(208.0, 180.0, 242.0, 0.065)
    };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(c, Color32::TRANSPARENT);
    const STEPS: u32 = 72;
    for k in 0..=STEPS {
        let deg = k as f32 * 360.0 / STEPS as f32;
        // CSS conic angles start at the top and go clockwise.
        let a = deg.to_radians() - FRAC_PI_2;
        let alpha = (lobe(deg) * 255.0).round() as u8;
        mesh.colored_vertex(c + vec2(a.cos(), a.sin()) * r, Color32::from_white_alpha(alpha));
    }
    for k in 0..STEPS {
        mesh.add_triangle(0, k + 1, k + 2);
    }
    painter.add(egui::Shape::mesh(mesh));
    painter.circle_stroke(c, r * 0.93, Stroke::new(1.0, Color32::from_white_alpha(9)));

    // The turning part: the dark label well, the cover, the spindle.
    painter.circle_filled(c, r * 0.44, Color32::from_rgb(15, 15, 17));
    painter.circle_stroke(c, r * 0.44, Stroke::new(1.0, Color32::from_white_alpha(10)));
    let lr = r * 0.36;
    match label {
        Some(tex) => {
            let mut m = egui::Mesh::with_texture(tex);
            m.vertices.push(egui::epaint::Vertex { pos: c, uv: pos2(0.5, 0.5), color: Color32::WHITE });
            const N: u32 = 48;
            for k in 0..N {
                let a = k as f32 / N as f32 * TAU;
                let uv = vec2((a - spin).cos(), (a - spin).sin()) * 0.5;
                m.vertices.push(egui::epaint::Vertex { pos: c + vec2(a.cos(), a.sin()) * lr, uv: pos2(0.5 + uv.x, 0.5 + uv.y), color: Color32::WHITE });
                m.add_triangle(0, k + 1, (k + 1) % N + 1);
            }
            painter.add(egui::Shape::mesh(m));
        }
        None => {
            painter.circle_filled(c, lr, Color32::from_rgb(35, 35, 39));
        }
    }
    painter.circle_stroke(c, lr, Stroke::new(1.0, Color32::from_black_alpha(180)));
    let hole = (d * 0.035).max(3.0) / 2.0;
    knob(painter, c, hole);

    for &(deg, opacity) in arms.iter().filter(|a| a.1 > 0.0) {
        let mut painter = painter.clone();
        painter.multiply_opacity(opacity);
        tonearm(&painter, origin, d, deg);
    }
    // The pivot (`.piv`): a chrome knob with a dark centre.
    let pd = d * 0.15;
    let pc = pos2(origin.x + d * 0.965 + pd / 2.0, origin.y + d * 0.025 + pd / 2.0);
    painter.circle_filled(pc + vec2(0.0, 2.0), pd / 2.0 + 1.0, Color32::from_black_alpha(120));
    knob(painter, pc, pd / 2.0);
    painter.circle_filled(pc, pd / 2.0 * 0.32, Color32::from_rgb(27, 29, 34));
}

/// The turntable's tonearm at `arm_deg`, turning on its pivot at the record's top right.
fn tonearm(painter: &egui::Painter, origin: egui::Pos2, d: f32, arm_deg: f32) {
    let aw = (d * 0.026).max(2.0);
    let top = pos2(origin.x + d * 1.04, origin.y + d * 0.10);
    let len = d * 0.61;
    let rot = egui::emath::Rot2::from_angle(arm_deg.to_radians());
    let down = rot * vec2(0.0, 1.0);
    let across = rot * vec2(1.0, 0.0);
    // Its shadow (0 2px 5px).
    painter.line_segment([top + vec2(0.0, 2.0), top + down * len + vec2(0.0, 2.0)], Stroke::new(aw + 2.0, Color32::from_black_alpha(90)));
    steel_bar(painter, top, down, across, aw, len, &[(0.0, (102, 108, 120)), (0.45, (244, 246, 250)), (0.62, (163, 172, 186)), (1.0, (90, 96, 108))]);
    // The counterweight above the pivot and the headshell at the end, turned a further -22°.
    let cw_w = (d * 0.075).max(4.0);
    let cw_h = (d * 0.1).max(4.0);
    steel_bar(painter, top - down * (d * 0.17), down, across, cw_w, cw_h, &[(0.0, (52, 56, 63)), (0.5, (168, 176, 189)), (1.0, (52, 56, 63))]);
    let hd_w = (d * 0.075).max(4.0);
    let hd_h = (d * 0.115).max(5.0);
    let hrot = egui::emath::Rot2::from_angle((arm_deg - 22.0).to_radians());
    let hdown = hrot * vec2(0.0, 1.0);
    let hacross = hrot * vec2(1.0, 0.0);
    let hd_top = top + down * (len + d * 0.075 - hd_h);
    steel_bar(painter, hd_top, hdown, hacross, hd_w, hd_h, &[(0.0, (74, 80, 92)), (0.5, (223, 228, 236)), (1.0, (74, 80, 92))]);
}

/// A bar `w` wide and `len` long from `top` along `down`, shaded across its width by `stops`
/// (position 0-1, rgb), like the prototype's steel gradients.
fn steel_bar(painter: &egui::Painter, top: egui::Pos2, down: egui::Vec2, across: egui::Vec2, w: f32, len: f32, stops: &[(f32, (u8, u8, u8))]) {
    let mut m = egui::Mesh::default();
    for &(t, (r, g, b)) in stops {
        let x = across * (t - 0.5) * w;
        let col = Color32::from_rgb(r, g, b);
        m.colored_vertex(top + x, col);
        m.colored_vertex(top + x + down * len, col);
    }
    for i in 0..(stops.len() as u32 - 1) {
        let a = i * 2;
        m.add_triangle(a, a + 1, a + 2);
        m.add_triangle(a + 1, a + 3, a + 2);
    }
    painter.add(egui::Shape::mesh(m));
}

/// The chrome knob (`--knob`: a radial gradient lit from the top left).
fn knob(painter: &egui::Painter, c: egui::Pos2, r: f32) {
    let stops = [(1.0, Color32::from_rgb(42, 47, 58)), (0.72, Color32::from_rgb(93, 102, 121)), (0.34, Color32::from_rgb(195, 204, 218)), (0.0, Color32::WHITE)];
    // The light sits at 35 % / 30 % of the knob: circles shrink towards it.
    let light = c + vec2(-0.15 * 2.0 * r, -0.20 * 2.0 * r);
    for w in stops.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        for k in 0..4 {
            let t = t0 + (t1 - t0) * k as f32 / 4.0;
            let col = super::util::mix(c0, c1, k as f32 / 4.0);
            let center = light + (c - light) * t;
            painter.circle_filled(center, (r * t).max(0.3), col);
        }
    }
}

/// A one-line text with the design's letter spacing (CSS `letter-spacing` in px).
pub fn spaced(ui: &Ui, text: &str, font: egui::FontId, color: Color32, spacing: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    job.append(text, 0.0, egui::TextFormat { font_id: font, color, extra_letter_spacing: spacing, ..Default::default() });
    job.wrap.max_rows = 1;
    ui.fonts_mut(|f| f.layout_job(job))
}

/// A rounded rect filled with a vertical gradient, `top` to `bottom` (a 1×32 texture stretched over it).
pub fn gradient_rect(painter: &egui::Painter, rect: Rect, radius: u8, top: Color32, bottom: Color32) {
    type Gradients = std::collections::HashMap<([u8; 4], [u8; 4]), egui::TextureHandle>;
    #[derive(Clone, Default)]
    struct Cache(std::sync::Arc<std::sync::Mutex<Gradients>>);
    let ctx = painter.ctx();
    let cache = ctx.data_mut(|d| d.get_temp_mut_or_default::<Cache>(Id::new("truffle-gradients")).clone());
    let key = (top.to_array(), bottom.to_array());
    let tex = {
        let mut map = cache.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(key)
            .or_insert_with(|| {
                let pixels = (0..32).map(|i| super::util::mix(top, bottom, i as f32 / 31.0)).collect();
                let image = egui::ColorImage::new([1, 32], pixels);
                ctx.load_texture(format!("gradient-{key:?}"), image, egui::TextureOptions::LINEAR)
            })
            .clone()
    };
    let shape = egui::epaint::RectShape::filled(rect, CornerRadius::same(radius), Color32::WHITE)
        .with_texture(tex.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)));
    painter.add(shape);
}

/// The switch (`.sw`): 36×20, a 1.2 px border and a 13.6 px knob; on: accent with a black or white
/// knob, whichever reads on it; `enabled` false greys it. Returns true when it was clicked (the
/// caller flips its value).
pub fn switch(ui: &Ui, p: &Palette, rect: Rect, id: Id, on: bool, enabled: bool, tip: &str) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    let painter = ui.painter();
    let look = |on: bool| match (on, enabled) {
        (true, true) => (p.accent, p.accent, p.on_accent),
        (false, true) => (Color32::TRANSPARENT, p.text_weak, p.text_weak),
        (true, false) => (p.text_faint, p.text_faint, p.text_weak),
        (false, false) => (Color32::TRANSPARENT, p.text_faint, p.text_faint),
    };
    // The knob slides across as the colours fade (with fades only, it fades from one end to the other).
    let t = motion::toward(ui.ctx(), id.with("knob"), on, motion::SWITCH);
    let ((fill0, border0, knob0), (fill1, border1, knob1)) = (look(false), look(true));
    let fill = motion::mix(fill0, fill1, t);
    if fill.a() > 0 {
        painter.rect_filled(rect, CornerRadius::same(10), fill);
    }
    painter.rect_stroke(rect, CornerRadius::same(10), Stroke::new(1.2, motion::mix(border0, border1, t)), StrokeKind::Inside);
    let (off_x, on_x) = (rect.left() + 2.0 + 8.0, rect.left() + 17.6 + 8.0);
    if motion::moves() {
        painter.circle_filled(pos2(off_x + (on_x - off_x) * t, rect.center().y), 6.8, motion::mix(knob0, knob1, t));
    } else {
        painter.circle_filled(pos2(off_x, rect.center().y), 6.8, knob0.gamma_multiply(1.0 - t));
        painter.circle_filled(pos2(on_x, rect.center().y), 6.8, knob1.gamma_multiply(t));
    }
    let resp = if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    let resp = if tip.is_empty() { resp } else { resp.on_hover_text(tip) };
    resp.clicked()
}

/// A text button (`.b`): 28 tall, 14 either side, round 6, 11 px semibold capitals spaced .05em, an
/// optional 13 px icon 7 before the text. `primary` fills it with the accent. Returns its response.
#[allow(clippy::too_many_arguments)]
pub fn text_button(ui: &Ui, p: &Palette, at: egui::Pos2, id: Id, text: &str, icon: Option<&str>, primary: bool, enabled: bool) -> Response {
    let label = text.to_uppercase();
    let color = if !enabled { p.text_faint } else if primary { p.on_accent } else { p.text };
    let color = motion::color(ui.ctx(), id.with("text"), color, motion::HOVER);
    let g = spaced(ui, &label, super::theme::bold(11.0), Color32::PLACEHOLDER, 0.55);
    let icon_w = if icon.is_some() { 13.0 + 7.0 } else { 0.0 };
    let w = 14.0 + icon_w + g.size().x + 14.0;
    let rect = Rect::from_min_size(at, vec2(w, 28.0));
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let resp = ui.interact(rect, id, sense);
    // A greyed accent button is a plain grey one.
    let fill = match (primary && enabled, resp.hovered() && enabled) {
        (true, true) => brighten(p.accent, 1.07),
        (true, false) => p.accent,
        (false, true) => p.widget_hover_bg,
        (false, false) => p.widget_bg,
    };
    let fill = motion::color(ui.ctx(), id.with("fill"), fill, motion::HOVER);
    let scale = motion::press(ui.ctx(), id, enabled && resp.is_pointer_button_down_on());
    motion::scaled(ui, rect.center(), scale, || {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(6), fill);
        let mut x = rect.left() + 14.0;
        if let Some(icon) = icon {
            svgicon::paint(painter, Rect::from_min_size(pos2(x, rect.center().y - 6.5), vec2(13.0, 13.0)), icon, color, STROKE);
            x += 20.0;
        }
        painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, color);
    });
    if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp }
}

/// A round picture `r` in radius around `c`, turned `angle` radians (a record's label).
pub fn disc_image(painter: &egui::Painter, c: egui::Pos2, r: f32, tex: egui::TextureId, angle: f32) {
    use std::f32::consts::TAU;
    let mut m = egui::Mesh::with_texture(tex);
    m.vertices.push(egui::epaint::Vertex { pos: c, uv: pos2(0.5, 0.5), color: Color32::WHITE });
    const N: u32 = 40;
    for k in 0..N {
        let a = k as f32 / N as f32 * TAU;
        let uv = vec2((a - angle).cos(), (a - angle).sin()) * 0.5;
        m.vertices.push(egui::epaint::Vertex { pos: c + vec2(a.cos(), a.sin()) * r, uv: pos2(0.5 + uv.x, 0.5 + uv.y), color: Color32::WHITE });
        m.add_triangle(0, k + 1, (k + 1) % N + 1);
    }
    painter.add(egui::Shape::mesh(m));
}

/// The record behind a sleeve (`.sl .vn`): `d` across, grooved (#2a2a2f with a #17171a ring every 2.4 px),
/// a 1.5 px #3a3a40 edge, and the label (40 %) with its hole.
pub fn sleeve_record(painter: &egui::Painter, c: egui::Pos2, d: f32, label: Option<egui::TextureId>, label_fill: Color32) {
    let r = d / 2.0;
    painter.circle_filled(c, r + 1.5, Color32::from_rgb(58, 58, 64));
    painter.circle_filled(c, r, Color32::from_rgb(42, 42, 47));
    let mut ring = r - 0.5;
    while ring > r * 0.4 {
        painter.circle_stroke(c, ring, Stroke::new(1.0, Color32::from_rgb(23, 23, 26)));
        ring -= 2.4;
    }
    painter.circle_stroke(c, r - 0.5, Stroke::new(1.0, Color32::from_white_alpha(13)));
    let lr = r * 0.4;
    match label {
        Some(t) => disc_image(painter, c, lr, t, 0.0),
        None => {
            painter.circle_filled(c, lr, label_fill);
        }
    }
    painter.circle_stroke(c, lr, Stroke::new(1.0, Color32::from_black_alpha(128)));
    painter.circle_filled(c, (lr * 0.14).max(1.0), Color32::from_rgb(11, 11, 12));
}

/// A "Show all"-style link (11 px medium, muted, brighter on hover) at the right end of `row`. True when
/// clicked.
pub fn link(ui: &Ui, row: Rect, id: Id, text: &str) -> bool {
    let p = palette(ui.ctx());
    let g = ui.fonts_mut(|f| f.layout_no_wrap(text.to_owned(), super::theme::medium(11.0), p.text_weak));
    let rect = Rect::from_min_size(pos2(row.right() - g.size().x, row.center().y - g.size().y / 2.0), g.size());
    let resp = ui.interact(rect.expand(3.0), id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let h = motion::hover(ui.ctx(), id, resp.hovered());
    ui.painter().galley_with_override_text_color(rect.min, g, motion::mix(p.text_weak, p.text, h));
    resp.clicked()
}

/// A soft elliptic glow of `color` filling `rect` (fading to nothing at its edge), cut to `clip`.
pub fn radial_glow(painter: &egui::Painter, rect: Rect, color: Color32, clip: Rect) {
    use std::f32::consts::TAU;
    let painter = painter.with_clip_rect(clip.intersect(painter.clip_rect()));
    let mut m = egui::Mesh::default();
    m.colored_vertex(rect.center(), color);
    const N: u32 = 48;
    for k in 0..=N {
        let a = k as f32 / N as f32 * TAU;
        m.colored_vertex(rect.center() + vec2(a.cos() * rect.width() / 2.0, a.sin() * rect.height() / 2.0), Color32::TRANSPARENT);
    }
    for k in 0..N {
        m.add_triangle(0, k + 1, k + 2);
    }
    painter.add(egui::Shape::mesh(m));
}

/// A 1 px line across `x`, fading out at both ends (the cover-flow's ledge).
pub fn hfade_line(painter: &egui::Painter, x: std::ops::RangeInclusive<f32>, y: f32, color: Color32) {
    let (a, b) = (*x.start(), *x.end());
    let w = b - a;
    let stops = [(0.0, Color32::TRANSPARENT), (0.2, color), (0.8, color), (1.0, Color32::TRANSPARENT)];
    let mut m = egui::Mesh::default();
    for (t, c) in stops {
        m.colored_vertex(pos2(a + w * t, y), c);
        m.colored_vertex(pos2(a + w * t, y + 1.0), c);
    }
    for i in 0..3u32 {
        let k = i * 2;
        m.add_triangle(k, k + 1, k + 2);
        m.add_triangle(k + 1, k + 3, k + 2);
    }
    painter.add(egui::Shape::mesh(m));
}

/// A chip (`.chip`): 26 tall (24 `small`), 12 either side (10), round, 11.5 px medium (11), an optional
/// 13 px icon 7 before the text; `on`: the accent tint with a ring. Placed with its left at `at.x`,
/// centred on `at.y`.
#[allow(clippy::too_many_arguments)]
pub fn chip(ui: &Ui, p: &Palette, at: egui::Pos2, id: Id, text: &str, icon: Option<&str>, on: bool, small: bool) -> Response {
    let (h, pad, size) = if small { (24.0, 10.0, 11.0) } else { (26.0, 12.0, 11.5) };
    let color = if on { p.text_strong } else { p.text };
    let g = ui.painter().layout_no_wrap(text.to_owned(), super::theme::medium(size), color);
    let icon_w = if icon.is_some() { 13.0 + 7.0 } else { 0.0 };
    let rect = Rect::from_min_size(pos2(at.x, at.y - h / 2.0), vec2(pad + icon_w + g.size().x + pad, h));
    let resp = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let r = CornerRadius::same((h / 2.0) as u8);
    let fill = motion::color(ui.ctx(), id.with("fill"), if on { acc(p, 0.16) } else if resp.hovered() { p.widget_hover_bg } else { p.widget_bg }, motion::HOVER);
    let painter = ui.painter();
    painter.rect_filled(rect, r, fill);
    if on {
        painter.rect_stroke(rect, r, Stroke::new(1.0, acc(p, 0.45)), StrokeKind::Inside);
    }
    let mut x = rect.left() + pad;
    if let Some(icon) = icon {
        svgicon::paint(painter, Rect::from_min_size(pos2(x, rect.center().y - 6.5), vec2(13.0, 13.0)), icon, color, STROKE);
        x += icon_w;
    }
    painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, color);
    resp
}

/// How wide `chip` draws `text` without an icon.
pub fn chip_w(ui: &Ui, text: &str, small: bool) -> f32 {
    let (pad, size) = if small { (10.0, 11.0) } else { (12.0, 11.5) };
    pad + ui.painter().layout_no_wrap(text.to_owned(), super::theme::medium(size), Color32::PLACEHOLDER).size().x + pad
}

/// The dim over everything behind a dialog.
pub const BACKDROP: Color32 = Color32::from_black_alpha(150);

/// A dialog's box (like the shortcuts window): `--panel`, a 1 px line, round 12, 22 inside, a deep
/// shadow.
pub fn dialog_frame(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.raised_bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(22))
        .shadow(egui::Shadow { offset: [0, 18], blur: 48, spread: 0, color: Color32::from_black_alpha(170) })
}

/// A dialog in [`dialog_frame`] over [`BACKDROP`]: it fades and grows in, and fades out once it
/// is no longer shown.
pub fn dialog<T>(ctx: &egui::Context, id: Id, content: impl FnOnce(&mut Ui) -> T) -> egui::ModalResponse<T> {
    motion::dialog(ctx, id, dialog_frame(&palette(ctx)), BACKDROP, content)
}

/// A dialog's title (15 semibold, bright) and, under it, a line of muted words.
pub fn dialog_head(ui: &mut Ui, title: &str, note: Option<&str>) {
    let p = palette(ui.ctx());
    ui.label(egui::RichText::new(title).font(super::theme::bold(15.0)).color(p.text_strong));
    if let Some(note) = note {
        ui.add_space(6.0);
        ui.add(egui::Label::new(egui::RichText::new(note).font(regular(12.0)).color(p.text_weak)).wrap());
    }
}

/// The colours of menu rows (text, bright, muted, greyed, hover fill, line).
fn menu_colors(ui: &Ui) -> (Color32, Color32, Color32, Color32, Color32, Color32) {
    let p = palette(ui.ctx());
    (p.text, p.text_strong, p.text_weak, p.text_faint, p.widget_hover_bg, p.border)
}

/// A menu row (`.menu .mi`): 28 tall, round 5, a 13 px icon (muted, brighter on hover), the words
/// (cut short with "…" if too long, the whole on hover), and on the right a key (10.5 muted) or, when
/// `on`, an accent check. Greyed when not `enabled`.
pub fn menu_item(ui: &mut Ui, icon: Option<&str>, label: &str, key: Option<&str>, on: bool, enabled: bool) -> Response {
    let w = ui.available_width();
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), sense);
    let hot = motion::hover(ui.ctx(), resp.id, enabled && resp.hovered());
    let (text, bright, muted, dis, hover, _) = menu_colors(ui);
    let painter = ui.painter();
    if hot > 0.0 {
        painter.rect_filled(rect, CornerRadius::same(5), hover.gamma_multiply(hot));
    }
    let (text_c, icon_c) = if enabled { (motion::mix(text, bright, hot), motion::mix(muted, text, hot)) } else { (dis, dis) };
    let mut x = rect.left() + 10.0;
    if let Some(icon) = icon {
        svgicon::paint(painter, Rect::from_min_size(pos2(x, rect.center().y - 6.5), vec2(13.0, 13.0)), icon, icon_c, STROKE);
        x += 13.0 + 10.0;
    }
    // The right end: the check, or the key.
    let mut right = rect.right() - 10.0;
    if on {
        svgicon::paint(painter, Rect::from_min_size(pos2(right - 13.0, rect.center().y - 6.5), vec2(13.0, 13.0)), "check", palette(ui.ctx()).accent_ink, STROKE);
        right -= 13.0 + 10.0;
    } else if let Some(key) = key {
        let g = painter.layout_no_wrap(key.to_owned(), regular(10.5), muted);
        painter.galley(pos2(right - g.size().x, rect.center().y - g.size().y / 2.0), g.clone(), muted);
        right -= g.size().x + 18.0;
    }
    let mut job = egui::text::LayoutJob::single_section(label.to_owned(), egui::TextFormat::simple(regular(12.0), text_c));
    job.wrap = egui::text::TextWrapping::truncate_at_width((right - x).max(10.0));
    let g = painter.layout_job(job);
    let cut = g.elided;
    painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, text_c);
    let resp = if cut { resp.on_hover_text(label) } else { resp };
    if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp }
}

/// A choice in a drop-down list: a menu row with the accent check on the chosen one. Picking it
/// sets `value`.
pub fn choice<T: PartialEq>(ui: &mut Ui, value: &mut T, this: T, label: &str) {
    let on = *value == this;
    if menu_item(ui, None, label, None, on, true).clicked() {
        *value = this;
    }
}

/// A line between groups of menu rows (`.menu .ml`).
pub fn menu_sep(ui: &mut Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 9.0), Sense::hover());
    let line = menu_colors(ui).5;
    ui.painter().hline((rect.left() + 6.0)..=(rect.right() - 6.0), rect.center().y, Stroke::new(1.0, line));
}

/// A small muted heading inside a menu ("Stop playing after").
pub fn menu_label(ui: &mut Ui, text: &str) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
    let muted = menu_colors(ui).2;
    ui.painter().text(pos2(rect.left() + 10.0, rect.center().y), egui::Align2::LEFT_CENTER, text, regular(11.0), muted);
}

/// A text box in a dialog (like `.srch`): `--input`, a 1 px line (the accent while typing in it),
/// round 6, 12 px bright words and a muted hint.
pub fn field(ui: &mut Ui, p: &Palette, edit: TextEdit<'_>) -> Response {
    // The box is the text box's own frame, so a click anywhere in it (padding too) lands in it.
    let frame = egui::Frame::new().fill(p.field_bg).stroke(Stroke::new(1.0, p.field_border)).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::symmetric(10, 7));
    let resp = ui.add(edit.frame(frame).font(regular(12.0)).text_color(p.text_strong));
    if resp.has_focus() {
        ui.painter().rect_stroke(resp.rect, CornerRadius::same(6), Stroke::new(1.0, p.accent_ink), StrokeKind::Inside);
    }
    resp
}

/// A field's name above it (11 px muted).
pub fn field_label(ui: &mut Ui, text: &str) {
    ui.add_space(12.0);
    ui.label(egui::RichText::new(text).font(regular(11.0)).color(palette(ui.ctx()).text_weak));
    ui.add_space(4.0);
}

/// How wide `text_button` draws `text`.
pub fn text_button_w(ui: &Ui, text: &str, icon: bool) -> f32 {
    let g = spaced(ui, &text.to_uppercase(), super::theme::bold(11.0), Color32::PLACEHOLDER, 0.55);
    14.0 + if icon { 20.0 } else { 0.0 } + g.size().x + 14.0
}

/// A dialog's buttons, on the right: the accent one (`ok`, greyed unless `ok_enabled`), then CANCEL.
/// Returns (ok clicked, cancel clicked).
pub fn dialog_actions(ui: &mut Ui, p: &Palette, id: Id, ok: &str, ok_enabled: bool) -> (bool, bool) {
    ui.add_space(20.0);
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
    let cancel_w = text_button_w(ui, "Cancel", false);
    let ok_w = text_button_w(ui, ok, false);
    let cancel = text_button(ui, p, pos2(row.right() - cancel_w, row.top()), id.with("cancel"), "Cancel", None, false, true).clicked();
    let ok = text_button(ui, p, pos2(row.right() - cancel_w - 8.0 - ok_w, row.top()), id.with("ok"), ok, None, true, ok_enabled).clicked();
    (ok, cancel)
}
