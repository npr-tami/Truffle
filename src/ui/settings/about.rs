//! Settings → About, as in the mockup chosen for it (docs/mockups/about.html, "Spinning record"):
//! the record turning while music plays beside the wordmark, version and song; "Your Truffle" (the
//! play counts added up); "This copy" (account, YouTube helpers, Truffle's folder, details to copy
//! for a bug report); "Made with", and the open-source licences in a box over it. Sizes are the
//! mockup's CSS pixels, which are egui points; its line heights (1.4, the credits' 1.6) decide where
//! each line of text sits.

use std::f32::consts::{FRAC_PI_2, TAU};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Color32, CornerRadius, CursorIcon, FontId, Frame, Galley, Id, Painter, Pos2, Rect, Sense, Stroke, StrokeKind,
    TextureHandle, TextureId, Ui, pos2, vec2,
};

use super::Item;
use crate::config::SoundSource;
use crate::model::{Command, Helpers, PlayStats, PlayStatus};
use crate::ui::TruffleApp;
use crate::ui::accent::{from_hsv, to_hsv};
use crate::ui::fin;
use crate::ui::kit;
use crate::ui::theme::{bold, heavy, medium, regular};
use crate::ui::util::{fmt_thousands, mix};

/// The space under a card (`.card` margin-bottom), and a title's above it (`.ctitle` margin-top,
/// which the card above's 14 overlaps) and under it.
const CARD_GAP: f32 = 14.0;
const TITLE_TOP: f32 = 18.0;
const TITLE_BELOW: f32 = 8.0;
/// Lines of text are their font size times this apart (CSS `line-height`); the credits' more.
const LEADING: f32 = 1.4;
const CREDITS_LEADING: f32 = 1.6;
/// The record (`.disc`), its label and its hole, across.
const DISC: f32 = 150.0;
const LABEL: f32 = 56.0;
const HOLE: f32 = 8.0;
/// One turn of the record while music plays.
const TURN_SECS: f32 = 6.0;
/// The record's shadow fade (`box-shadow: 0 10px 30px`). CSS blurs 30 px as a bell curve, fading
/// over about 30 px each side of the edge; egui fades in a straight line, half of `blur` each side,
/// and 46 is the straight fade nearest that curve.
const SHADOW_BLUR: u8 = 46;
/// The grooves: #111 with a 1 px #1a1a1d ring every 3 px.
const GROOVE: Color32 = Color32::from_rgb(17, 17, 17);
const GROOVE_RING: Color32 = Color32::from_rgb(26, 26, 29);
const WORDMARK_W: f32 = 200.0;
/// `assets/wordmark.png`'s height for its width, until it is decoded.
const WORDMARK_ASPECT: f32 = 347.0 / 1495.0;
/// The most played song's cover.
const COVER: f32 = 40.0;
/// "Found" (`--ok`).
const OK: Color32 = Color32::from_rgb(74, 222, 128);
/// A button (`.btn`): 6 14 inside, a 1 px line round it, 12.5 px words.
const BUTTON_H: f32 = 12.5 * LEADING + 12.0 + 2.0;
/// A licence's row in the licences box.
const LICENCE_ROW: f32 = 26.0;

/// What the licences box lists: `assets/licences.txt` (`cargo run --example licences` makes it).
static LICENCES: &str = include_str!("../../../assets/licences.txt");

/// The About page's own state: what it asked for and got back, and its record's turn.
#[derive(Default)]
pub(in crate::ui) struct AboutState {
    /// "Your Truffle", asked for each time the page shows.
    stats: Option<PlayStats>,
    pub(super) stats_asked: bool,
    /// The YouTube helpers' versions, asked for once a run.
    helpers: Option<Helpers>,
    helpers_asked: bool,
    /// "Copy details" was clicked before the helpers' versions came: they are copied when they do.
    copy_waiting: bool,
    /// The record's turn (radians, clockwise) and the input time it was turned to.
    angle: f32,
    turned_at: f64,
    /// The wordmarks for dark and light backgrounds, each decoded off the UI thread the first time
    /// it is needed.
    wordmark: [Arc<OnceLock<TextureHandle>>; 2],
    wordmark_asked: [bool; 2],
    /// The header's glow (white, fading out; tinted with the accent), and the latest accent
    /// gradient picture with its two colours.
    glow: Option<TextureHandle>,
    gradient: Option<([u8; 4], [u8; 4], TextureHandle)>,
    /// The licences box is open.
    pub(super) licences_open: bool,
}

impl AboutState {
    /// "Copy details" was clicked: true when the details can go now, else they wait for the
    /// helpers' versions (a first run of yt-dlp takes a while).
    fn copy_asked(&mut self) -> bool {
        self.copy_waiting = self.helpers.is_none();
        !self.copy_waiting
    }

    /// The helpers' versions came: true when "Copy details" was waiting for them.
    fn helpers_came(&mut self, helpers: Helpers) -> bool {
        self.helpers = Some(helpers);
        std::mem::take(&mut self.copy_waiting)
    }

    /// New play counts, keeping the most played song's name and cover while the same song is
    /// looked up again (instead of a blank for a moment).
    fn stats_came(&mut self, mut stats: PlayStats) {
        let before = self.stats.as_ref().and_then(|s| s.top.as_ref());
        if let (Some(top), Some(before)) = (stats.top.as_mut(), before)
            && top.track.is_none()
            && top.uri == before.uri
        {
            top.track = before.track.clone();
        }
        self.stats = Some(stats);
    }
}

/// One entry of the licences list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Licence {
    name: &'static str,
    version: &'static str,
    licence: &'static str,
}

/// `assets/licences.txt`, read once.
fn licences() -> &'static [Licence] {
    static LIST: OnceLock<Vec<Licence>> = OnceLock::new();
    LIST.get_or_init(|| {
        LICENCES
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let mut parts = l.split('\t');
                let mut next = || parts.next().unwrap_or("").trim();
                Licence { name: next(), version: next(), licence: next() }
            })
            .collect()
    })
}

/// Text laid out as the mockup's CSS lays it out: its lines `leading` times the font size apart.
struct Text {
    galley: Arc<Galley>,
    /// How far below its line box's top the text is drawn (half the leading).
    lead: f32,
    /// The line boxes' height.
    h: f32,
}

/// `sections` (words, font, colour) as one text, wrapped at `wrap`, or kept to one line and cut
/// short with "…" (`one_line`).
fn layout(ui: &Ui, sections: &[(&str, FontId, Color32)], leading: f32, wrap: f32, one_line: bool) -> Text {
    let font = sections.first().map_or_else(|| regular(12.0), |s| s.1.clone());
    let line = font.size * leading;
    let mut job = LayoutJob::default();
    for (words, font, color) in sections {
        job.append(words, 0.0, TextFormat { font_id: font.clone(), color: *color, line_height: Some(line), ..Default::default() });
    }
    let wrap = wrap.max(1.0);
    job.wrap = if one_line { TextWrapping::truncate_at_width(wrap) } else { TextWrapping::wrap_at_width(wrap) };
    let row = ui.fonts_mut(|f| f.row_height(&font));
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let h = galley.size().y.max(line);
    Text { galley, lead: ((line - row) / 2.0).max(0.0), h }
}

fn paint_text(painter: &Painter, at: Pos2, text: Text) {
    let p = crate::ui::theme::palette(painter.ctx());
    painter.galley(pos2(at.x, at.y + text.lead), text.galley, p.text);
}

/// A card of the About page (`.card`): the raised card grey (`--panel`, the mockup's `--card`), a
/// 1 px line, round 8, 14 under it; with its title above it (`.ctitle`: 13 semibold, muted, 2 in,
/// 8 above the card).
pub(super) fn card(ui: &mut Ui, title: Option<&str>, add: impl FnOnce(&mut Ui)) {
    let p = crate::ui::theme::palette(ui.ctx());
    if let Some(title) = title {
        ui.add_space(TITLE_TOP - CARD_GAP);
        let t = layout(ui, &[(title, bold(13.0), p.text_weak)], LEADING, ui.available_width() - 4.0, true);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), t.h), Sense::hover());
        paint_text(ui.painter(), rect.min + vec2(2.0, 0.0), t);
        ui.add_space(TITLE_BELOW);
    }
    Frame::new().fill(p.raised_bg).stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        add(ui);
    });
    ui.add_space(CARD_GAP);
}

/// The accent gradient (the mockup's #ff8fd1 to #7b5cff, made from its accent #b56cff): the accent
/// turned 55° towards pink and paler, to 18° the other way and deeper.
fn accent_gradient(accent: Color32) -> (Color32, Color32) {
    let (h, s, v) = to_hsv(accent);
    (from_hsv(h + 55.0, s * 0.76, v), from_hsv(h - 18.0, (s * 1.11).min(1.0), v))
}

/// How far along a `linear-gradient(135deg, …)` over a square `2 * half` across a point is (0 at
/// the top-left corner, 1 at the bottom-right), `v` from the square's middle.
fn gradient_at(v: egui::Vec2, half: f32) -> f32 {
    (v.x + v.y) / (4.0 * half) + 0.5
}

/// What a round label shows.
enum LabelFill {
    Cover(TextureId),
    Gradient(Color32, Color32),
}

/// A round label `r` in radius around `c`, turned `turn` radians: the cover, or the gradient across
/// it. Its edge is softened over a pixel, as egui's own shapes are.
fn paint_label(painter: &Painter, c: Pos2, r: f32, turn: f32, fill: &LabelFill) {
    const N: u32 = 64;
    let feather = 1.0 / painter.pixels_per_point();
    let tex = match fill {
        LabelFill::Cover(t) => *t,
        LabelFill::Gradient(..) => TextureId::default(),
    };
    // A point `dir * radius` from the middle, as it is on the label before it turned.
    let vertex = |dir: egui::Vec2, radius: f32, edge: bool| {
        let local = egui::emath::Rot2::from_angle(-turn) * dir * radius;
        let (uv, color) = match fill {
            LabelFill::Cover(_) => (pos2(0.5 + local.x / (2.0 * r), 0.5 + local.y / (2.0 * r)), Color32::WHITE),
            LabelFill::Gradient(a, b) => (egui::epaint::WHITE_UV, mix(*a, *b, gradient_at(local, r))),
        };
        egui::epaint::Vertex { pos: c + dir * radius, uv, color: if edge { Color32::TRANSPARENT } else { color } }
    };
    let mut mesh = egui::Mesh::with_texture(tex);
    mesh.vertices.push(vertex(egui::Vec2::ZERO, 0.0, false));
    for k in 0..N {
        let a = k as f32 / N as f32 * TAU;
        let dir = vec2(a.cos(), a.sin());
        mesh.vertices.push(vertex(dir, r - feather / 2.0, false));
        mesh.vertices.push(vertex(dir, r + feather / 2.0, true));
    }
    for k in 0..N {
        let (inner, outer) = (1 + 2 * k, 2 + 2 * k);
        let (next_inner, next_outer) = (1 + 2 * ((k + 1) % N), 2 + 2 * ((k + 1) % N));
        mesh.add_triangle(0, inner, next_inner);
        mesh.add_triangle(inner, outer, next_outer);
        mesh.add_triangle(inner, next_outer, next_inner);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// The record's sheen (`conic-gradient(from 30deg, transparent 0 12%, white 7 % at 16%, transparent
/// 22% 62%, white 5 % at 66%, transparent 72%)`), turned with the record.
fn paint_sheen(painter: &Painter, c: Pos2, r: f32, turn: f32) {
    const STOPS: [(f32, f32); 6] = [(0.12, 0.0), (0.16, 0.07), (0.22, 0.0), (0.62, 0.0), (0.66, 0.05), (0.72, 0.0)];
    const SLICES: u32 = 180;
    let alpha = |at: f32| {
        STOPS.windows(2).find(|w| at >= w[0].0 && at <= w[1].0).map_or(0.0, |w| {
            let t = (at - w[0].0) / (w[1].0 - w[0].0).max(f32::EPSILON);
            w[0].1 + (w[1].1 - w[0].1) * t
        })
    };
    let white = |at: f32| Color32::from_white_alpha((alpha(at) * 255.0).round() as u8);
    // CSS turns start at the top and go clockwise.
    let rim = |at: f32| {
        let a = 30f32.to_radians() + at * TAU + turn - FRAC_PI_2;
        c + vec2(a.cos(), a.sin()) * r
    };
    let mut mesh = egui::Mesh::default();
    for k in 0..SLICES {
        let (from, to) = (k as f32 / SLICES as f32, (k + 1) as f32 / SLICES as f32);
        let mid = (from + to) / 2.0;
        if alpha(from) == 0.0 && alpha(mid) == 0.0 && alpha(to) == 0.0 {
            continue;
        }
        // Each slice's middle has its own colour, so the colour follows the angle only.
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(c, white(mid));
        mesh.colored_vertex(rim(from), white(from));
        mesh.colored_vertex(rim(to), white(to));
        mesh.add_triangle(i, i + 1, i + 2);
    }
    painter.add(egui::Shape::mesh(mesh));
}

/// The header's glow as a picture over the whole header: `radial-gradient(ellipse at 15% 50%,
/// accent 12 %, transparent 60%)`, in white, to be tinted with the accent. CSS sizes the ellipse to
/// reach the farthest corner, keeping its closest-side proportions, which makes it the same over
/// any header, measured in parts of its width and height.
fn glow_texture(ctx: &egui::Context) -> TextureHandle {
    const W: usize = 128;
    const H: usize = 32;
    let (cx, cy) = (0.15_f32, 0.5_f32);
    let k = (((1.0 - cx) / cx).powi(2) + 1.0).sqrt();
    let (rx, ry) = (k * cx, k * cy);
    let pixels = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .map(|(x, y)| {
            let u = (x as f32 + 0.5) / W as f32;
            let v = (y as f32 + 0.5) / H as f32;
            let d = (((u - cx) / rx).powi(2) + ((v - cy) / ry).powi(2)).sqrt();
            Color32::from_white_alpha(((0.12 * (1.0 - d / 0.6)).max(0.0) * 255.0).round() as u8)
        })
        .collect();
    ctx.load_texture("about-glow", egui::ColorImage::new([W, H], pixels), egui::TextureOptions::LINEAR)
}

/// What sits at a row's right end.
enum Part<'a> {
    /// Muted words (`.val`).
    Val(&'a str),
    /// A green dot and "Found", or "Not found".
    Found(bool),
    Button(&'a str),
}

/// A row (`.row`): 12 16 inside, 52 tall at least, a line above it unless it is the card's first;
/// the title (13 medium) with its note under it (12 muted, 2 below), and `parts` at the right end,
/// 14 apart, everything centred down the row. Returns the button clicked (its place in `parts`).
fn row(ui: &mut Ui, title: &str, note: &str, parts: &[Part]) -> Option<usize> {
    let p = crate::ui::theme::palette(ui.ctx());
    let w = ui.available_width();
    let first = ui.cursor().top() <= ui.min_rect().top() + 0.5;
    let words = |ui: &Ui, s: &str| layout(ui, &[(s, regular(12.5), p.text_weak)], LEADING, f32::INFINITY, true);
    let widths: Vec<f32> = parts
        .iter()
        .map(|part| match part {
            Part::Val(s) => words(ui, s).galley.size().x,
            Part::Found(true) => 7.0 + 6.0 + words(ui, "Found").galley.size().x,
            Part::Found(false) => words(ui, "Not found").galley.size().x,
            Part::Button(s) => words(ui, s).galley.size().x + 28.0 + 2.0,
        })
        .collect();
    let parts_w = widths.iter().sum::<f32>() + 14.0 * widths.len().saturating_sub(1) as f32;
    let text_w = (w - 32.0 - parts_w - if parts.is_empty() { 0.0 } else { 14.0 }).max(60.0);
    let t = layout(ui, &[(title, medium(13.0), p.text)], LEADING, text_w, false);
    let n = layout(ui, &[(note, regular(12.0), p.text_weak)], LEADING, text_w, false);
    let text_h = t.h + 2.0 + n.h;
    let parts_h = if parts.iter().any(|p| matches!(p, Part::Button(_))) { BUTTON_H } else { 12.5 * LEADING };
    let line = if first { 0.0 } else { 1.0 };
    let h = (line + 24.0 + text_h.max(parts_h)).max(52.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let painter = ui.painter().clone();
    if !first {
        painter.hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, p.border));
    }
    let cy = rect.top() + line + (h - line) / 2.0;
    let x = rect.left() + 16.0;
    let top = cy - text_h / 2.0;
    let th = t.h;
    paint_text(&painter, pos2(x, top), t);
    paint_text(&painter, pos2(x, top + th + 2.0), n);

    let mut clicked = None;
    let mut x = rect.right() - 16.0 - parts_w;
    for (i, (part, pw)) in parts.iter().zip(widths).enumerate() {
        let line_top = cy - 12.5 * LEADING / 2.0;
        match part {
            Part::Val(s) => paint_text(&painter, pos2(x, line_top), words(ui, s)),
            Part::Found(found) => {
                let label = words(ui, if *found { "Found" } else { "Not found" });
                let mut at = x;
                if *found {
                    // `vertical-align: 1px`: the dot's foot 1 px above the words' baseline.
                    let first = label.galley.rows.first().and_then(|r| r.glyphs.first().map(|g| r.pos.y + g.pos.y));
                    let baseline = line_top + label.lead + first.unwrap_or(10.0);
                    painter.circle_filled(pos2(at + 3.5, baseline - 1.0 - 3.5), 3.5, OK);
                    at += 7.0 + 6.0;
                }
                paint_text(&painter, pos2(at, line_top), label);
            }
            Part::Button(s) => {
                let r = Rect::from_min_size(pos2(x, cy - BUTTON_H / 2.0), vec2(pw, BUTTON_H));
                let resp = ui.interact(r, Id::new(("about-button", *s)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                painter.rect(r, CornerRadius::same(6), if resp.hovered() { p.widget_hover_bg } else { p.widget_bg }, Stroke::new(1.0, p.border), StrokeKind::Inside);
                let label = layout(ui, &[(s, regular(12.5), p.text)], LEADING, f32::INFINITY, true);
                let lw = label.galley.size().x;
                paint_text(&painter, pos2(r.center().x - lw / 2.0, r.top() + 7.0), label);
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        }
        x += pw + 14.0;
    }
    clicked
}

/// "Version 0.1.0 · built 25 Oct 2026".
const VERSION_LINE: &str = concat!("Version ", env!("CARGO_PKG_VERSION"), " · built ", env!("TRUFFLE_BUILD_DATE"));

/// Said while "Copy details" waits for the YouTube helpers' versions.
const CHECKING_HELPERS: &str = "Checking the YouTube helpers…";

/// "Sound from …" words for a sound source.
fn source_name(source: SoundSource) -> &'static str {
    match source {
        SoundSource::Spotify => "Spotify",
        SoundSource::YouTube => "YouTube",
    }
}

/// A program and its version ("yt-dlp 2026.10.21"), just its name when it gave none.
fn named(name: &str, version: &str) -> String {
    if version.is_empty() { name.to_owned() } else { format!("{name} {version}") }
}

/// What "Copy details" copies, for a bug report: Truffle's version and build date, Windows, the
/// account type, where the sound comes from and the YouTube helpers. Never a name, a path or a key.
fn details_text(windows: &str, premium: Option<bool>, sound: SoundSource, helpers: Option<&Helpers>) -> String {
    let account = match premium {
        Some(true) => "Premium",
        Some(false) => "Free",
        None => "not known",
    };
    let (ytdlp, js) = match helpers {
        Some(h) => (
            h.ytdlp.as_deref().map_or_else(|| "yt-dlp not found".to_owned(), |v| named("yt-dlp", v)),
            h.js.as_ref().map_or_else(|| "JavaScript runtime not found".to_owned(), |(name, v)| named(name, v)),
        ),
        None => ("yt-dlp not checked yet".to_owned(), "JavaScript runtime not checked yet".to_owned()),
    };
    format!(
        "Truffle {}, built {}\n{windows}\nAccount: {account}\nSound from {}\n{ytdlp}\n{js}\n",
        env!("CARGO_PKG_VERSION"),
        env!("TRUFFLE_BUILD_DATE"),
        source_name(sound)
    )
}

/// "most played · 52 times".
fn most_played(plays: u32) -> String {
    if plays == 1 { "most played · 1 time".to_owned() } else { format!("most played · {} times", fmt_thousands(plays as usize)) }
}

impl TruffleApp {
    pub(in crate::ui) fn on_play_stats(&mut self, ctx: &egui::Context, stats: PlayStats) {
        if let Some(track) = stats.top.as_ref().and_then(|t| t.track.as_ref()) {
            self.fonts.scan(ctx, &track.name);
        }
        self.about.stats_came(stats);
    }

    pub(in crate::ui) fn on_helpers(&mut self, ctx: &egui::Context, helpers: Helpers) {
        if self.about.helpers_came(helpers) {
            self.copy_details(ctx);
        }
    }

    /// One of the About page's settings (its card's contents).
    pub(super) fn about_item(&mut self, ui: &mut Ui, item: Item) {
        // Each helper is run once a run, in the background, when any of About first shows (a
        // search can show "Something wrong?" alone, and its details name them).
        if !self.about.helpers_asked {
            self.about.helpers_asked = true;
            self.send(Command::Helpers);
        }
        match item {
            Item::Plays => self.about_stats(ui),
            Item::Account | Item::Helpers | Item::Folder | Item::Details => self.about_row(ui, item),
            Item::Credits => self.about_credits(ui),
            _ => self.about_header(ui),
        }
    }

    /// Turns the record on to this frame while music plays (Spin and Windows' animations on), and
    /// asks for the next frame meanwhile: at the screen's own rate, as the mockup's CSS turns it.
    /// Its turn now, in radians.
    fn turn_record(&mut self, ui: &Ui) -> f32 {
        let now = ui.input(|i| i.time);
        let playing = self.playback.track.is_some() && self.playback.status == PlayStatus::Playing;
        if playing && self.prefs.spin_record && kit::animations_on() {
            // A long gap (the page was away) counts as a tenth of a second.
            let dt = ((now - self.about.turned_at) as f32).clamp(0.0, 0.1);
            self.about.angle = (self.about.angle + dt * TAU / TURN_SECS).rem_euclid(TAU);
            ui.ctx().request_repaint();
        }
        self.about.turned_at = now;
        self.about.angle
    }

    /// The wordmark for a dark or a `light` background once it is decoded (in the background, the
    /// first time it is asked for).
    fn about_wordmark(&mut self, ctx: &egui::Context, light: bool) -> Option<TextureHandle> {
        let k = usize::from(light);
        if !self.about.wordmark_asked[k] {
            self.about.wordmark_asked[k] = true;
            let (slot, ctx) = (self.about.wordmark[k].clone(), ctx.clone());
            let bytes = if light { crate::ui::login::WORDMARK_LIGHT } else { crate::ui::login::WORDMARK };
            self.runtime.spawn_blocking(move || {
                let _ = slot.set(crate::ui::login::png_texture(&ctx, "about-wordmark", bytes));
                ctx.request_repaint();
            });
        }
        self.about.wordmark[k].get().cloned()
    }

    /// The accent gradient as a 32 x 32 picture, made again only when the accent changes.
    fn gradient_texture(&mut self, ctx: &egui::Context, from: Color32, to: Color32) -> TextureId {
        let key = (from.to_array(), to.to_array());
        if let Some((a, b, tex)) = &self.about.gradient
            && (*a, *b) == key
        {
            return tex.id();
        }
        const N: usize = 32;
        let pixels = (0..N)
            .flat_map(|y| (0..N).map(move |x| (x, y)))
            .map(|(x, y)| mix(from, to, gradient_at(vec2(x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0), 16.0)))
            .collect();
        let tex = ctx.load_texture("about-gradient", egui::ColorImage::new([N, N], pixels), egui::TextureOptions::LINEAR);
        let id = tex.id();
        self.about.gradient = Some((key.0, key.1, tex));
        id
    }

    /// The header (`.h-rec`): 22 24 inside, the record and, 24 right of it, the wordmark (200
    /// wide), the version and build date (4 under it) and the song playing (12 under that), all
    /// centred down the card, over a soft accent glow behind the record.
    fn about_header(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let w = ui.available_width();
        let text_x = 24.0 + DISC + 24.0;
        let text_w = (w - text_x - 24.0).max(40.0);
        let wordmark = self.about_wordmark(ui.ctx(), !p.dark);
        let wordmark_h = wordmark.as_ref().map_or(WORDMARK_W * WORDMARK_ASPECT, |t| {
            let [tw, th] = t.size();
            WORDMARK_W * th as f32 / tw.max(1) as f32
        });
        let version = layout(ui, &[(VERSION_LINE, regular(12.0), p.text_weak)], LEADING, text_w, true);
        let track = self.playback.track.as_ref();
        let cover = track.and_then(|t| t.cover_large.clone().or_else(|| t.cover_small.clone()));
        let now = track.map(|t| {
            let artists = t.artist_line();
            let song = if artists.is_empty() { t.name.clone() } else { format!("{artists} – {}", t.name) };
            layout(ui, &[("Now spinning: ", regular(12.0), p.text_weak), (&song, medium(12.0), p.text)], LEADING, text_w, true)
        });
        let column_h = wordmark_h + 4.0 + version.h + now.as_ref().map_or(0.0, |n| 12.0 + n.h);
        let (rect, _) = ui.allocate_exact_size(vec2(w, 22.0 + DISC.max(column_h) + 22.0), Sense::hover());
        if !ui.is_rect_visible(rect) {
            return;
        }
        // Kept inside the card, as its `overflow: hidden` keeps the record's shadow.
        let painter = ui.painter().with_clip_rect(rect);

        // The glow, inside the card's line (round 7 there).
        let glow = self.about.glow.get_or_insert_with(|| glow_texture(ui.ctx())).id();
        painter.add(
            egui::epaint::RectShape::filled(rect, CornerRadius::same(7), p.accent)
                .with_texture(glow, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
        );

        let turn = self.turn_record(ui);
        let c = pos2(rect.left() + 24.0 + DISC / 2.0, rect.center().y);
        let label = match cover.and_then(|url| self.images.get(&url, LABEL)) {
            Some(tex) => LabelFill::Cover(tex.id),
            None => {
                let (a, b) = accent_gradient(p.accent);
                LabelFill::Gradient(a, b)
            }
        };
        paint_record(&painter, c, turn, &label);

        let x = rect.left() + text_x;
        let mut y = rect.center().y - column_h / 2.0;
        if let Some(tex) = &wordmark {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(tex.id(), Rect::from_min_size(pos2(x, y), vec2(WORDMARK_W, wordmark_h)), uv, Color32::WHITE);
        }
        y += wordmark_h + 4.0;
        let vh = version.h;
        paint_text(&painter, pos2(x, y), version);
        if let Some(now) = now {
            paint_text(&painter, pos2(x, y + vh + 12.0), now);
        }
    }

    /// "Your Truffle" (`.stats`): three equal columns split by lines, 16 18 inside: songs played,
    /// different songs (22 px bold numbers over 12 px muted words), and the most played song with
    /// its 40 px cover.
    fn about_stats(&mut self, ui: &mut Ui) {
        let p = self.palette;
        if !self.about.stats_asked {
            self.about.stats_asked = true;
            self.send(Command::PlayStats);
        }
        let w = ui.available_width();
        let (n_line, k_line) = (22.0 * LEADING, 12.0 * LEADING);
        let (rect, _) = ui.allocate_exact_size(vec2(w, 16.0 + n_line + 2.0 + k_line + 16.0), Sense::hover());
        let painter = ui.painter().clone();
        let column = w / 3.0;
        let stats = self.about.stats.clone();
        let (plays, songs) = stats.as_ref().map_or_else(
            || ("–".to_owned(), "–".to_owned()),
            |s| (fmt_thousands(usize::try_from(s.plays).unwrap_or(usize::MAX)), fmt_thousands(s.songs)),
        );
        let left = |i: usize| rect.left() + column * i as f32;
        for (i, (number, words)) in [(plays, "songs played"), (songs, "different songs")].into_iter().enumerate() {
            let x = left(i) + if i > 0 { 1.0 } else { 0.0 } + 18.0;
            let fit = column - 36.0 - 1.0;
            paint_text(&painter, pos2(x, rect.top() + 16.0), layout(ui, &[(&number, heavy(22.0), p.text_strong)], LEADING, fit, true));
            paint_text(&painter, pos2(x, rect.top() + 16.0 + n_line + 2.0), layout(ui, &[(words, regular(12.0), p.text_weak)], LEADING, fit, true));
        }
        painter.vline(left(1) + 0.5, rect.y_range(), Stroke::new(1.0, p.border));
        // The most played song (`.top`): left out until something was played.
        let Some(top) = stats.and_then(|s| s.top) else { return };
        painter.vline(left(2) + 0.5, rect.y_range(), Stroke::new(1.0, p.border));
        let x = left(2) + 1.0 + 18.0;
        let y = rect.top() + 16.0;
        let cover = Rect::from_min_size(pos2(x, y), vec2(COVER, COVER));
        let url = top.track.as_ref().and_then(|t| t.cover_small.as_deref().or(t.cover_large.as_deref())).map(str::to_owned);
        match url.and_then(|u| self.images.get(&u, COVER)) {
            Some(tex) => egui::Image::from_texture(tex).corner_radius(CornerRadius::same(4)).paint_at(ui, cover),
            None => {
                let (a, b) = accent_gradient(self.palette.accent);
                let tex = self.gradient_texture(ui.ctx(), a, b);
                painter.add(
                    egui::epaint::RectShape::filled(cover, CornerRadius::same(4), Color32::WHITE)
                        .with_texture(tex, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
                );
            }
        }
        let fit = (column - 1.0 - 36.0 - COVER - 10.0).max(20.0);
        let name = top.track.as_ref().map_or("–", |t| t.name.as_str());
        let title = layout(ui, &[(name, bold(13.0), p.text_strong)], LEADING, fit, true);
        let count = layout(ui, &[(&most_played(top.plays), regular(12.0), p.text_weak)], LEADING, fit, true);
        let block_h = title.h + 2.0 + count.h;
        let ty = y + (COVER - block_h) / 2.0;
        let th = title.h;
        paint_text(&painter, pos2(cover.right() + 10.0, ty), title);
        paint_text(&painter, pos2(cover.right() + 10.0, ty + th + 2.0), count);
    }

    /// The rows of "This copy".
    fn about_row(&mut self, ui: &mut Ui, item: Item) {
        match item {
            Item::Account => {
                let profile = self.profile.as_ref();
                let who = profile.map_or_else(String::new, |pr| if pr.display_name.is_empty() { pr.username.clone() } else { pr.display_name.clone() });
                let note = match profile.and_then(|pr| pr.premium) {
                    Some(true) => format!("{who} · Premium"),
                    Some(false) => format!("{who} · Free"),
                    None => who,
                };
                let sound = format!("Sound from {}", source_name(self.sound_source()));
                row(ui, "Account", &note, &[Part::Val(&sound)]);
            }
            Item::Helpers => match &self.about.helpers {
                None => {
                    row(ui, "YouTube helpers", "Looking…", &[]);
                }
                Some(h) => match &h.missing {
                    None => {
                        let ytdlp = named("yt-dlp", h.ytdlp.as_deref().unwrap_or_default());
                        let js = h.js.as_ref().map_or_else(String::new, |(name, v)| named(name, v));
                        row(ui, "YouTube helpers", &format!("{ytdlp} · {js}"), &[Part::Found(true)]);
                    }
                    Some(why) => {
                        row(ui, "YouTube helpers", why, &[Part::Found(false)]);
                    }
                },
            },
            // Release builds keep the settings in another folder (`%APPDATA%\truffle\config`), so
            // they aren't named here.
            Item::Folder => match row(ui, "Truffle's folder", "Cache and logs", &[Part::Button("Open folder"), Part::Button("Open log")]) {
                Some(0) => self.open_path(self.paths.data_dir(), "Truffle's folder"),
                Some(_) => match crate::config::log_file().filter(|f| f.is_file()) {
                    Some(log) => self.open_path(log, "the log"),
                    None => self.toast("There's no log file yet.", false),
                },
                None => {}
            },
            _ => {
                let note = "Copies the version and these details, to send with a screenshot";
                if row(ui, "Something wrong?", note, &[Part::Button("Copy details")]).is_some() {
                    if self.about.copy_asked() {
                        self.copy_details(ui.ctx());
                    } else {
                        self.note(CHECKING_HELPERS, None);
                    }
                }
            }
        }
    }

    /// Copies the details for a bug report, and says so.
    fn copy_details(&mut self, ctx: &egui::Context) {
        let premium = self.profile.as_ref().and_then(|pr| pr.premium);
        let text = details_text(&crate::platform::windows_version(), premium, self.sound_source(), self.about.helpers.as_ref());
        ctx.copy_text(text);
        self.toasts.retain(|t| t.text != CHECKING_HELPERS);
        self.note("Details copied", None);
    }

    /// Opens a folder or file with Windows' own app for it, off the UI thread.
    fn open_path(&self, path: PathBuf, what: &'static str) {
        self.runtime.spawn_blocking(move || {
            if let Err(e) = open::that_detached(&path) {
                log::warn!("could not open {what}: {e}");
            }
        });
    }

    /// "Made with" (`.credits`): 14 18 inside, 12 px muted lines 1.6 apart, the names in the text
    /// colour, and the accent link that opens the licences.
    fn about_credits(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let accent = p.accent_ink;
        let w = ui.available_width();
        let fit = (w - 36.0).max(60.0);
        let plain = |s: &'static str| (s, regular(12.0), p.text_weak);
        let name = |s: &'static str| (s, medium(12.0), p.text);
        let lines = [
            layout(ui, &[plain("Made by "), name("prip"), plain(". Written in Rust with egui.")], CREDITS_LEADING, fit, false),
            layout(
                ui,
                &[plain("Your library and songs come from "), name("Spotify"), plain(" (librespot). YouTube sound uses "), name("yt-dlp"), plain(".")],
                CREDITS_LEADING,
                fit,
                false,
            ),
            layout(ui, &[("Open-source licences", regular(12.0), accent)], CREDITS_LEADING, fit, true),
        ];
        let h = 14.0 + lines.iter().map(|l| l.h).sum::<f32>() + 14.0;
        let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        let painter = ui.painter().clone();
        let mut y = rect.top() + 14.0;
        let mut link = Rect::NOTHING;
        for (i, line) in lines.into_iter().enumerate() {
            let lh = line.h;
            if i == 2 {
                link = Rect::from_min_size(pos2(rect.left() + 18.0, y), vec2(line.galley.size().x, lh));
            }
            paint_text(&painter, pos2(rect.left() + 18.0, y), line);
            y += lh;
        }
        if ui.interact(link, Id::new("about-licences"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).clicked() {
            self.about.licences_open = true;
        }
    }

    /// The open-source licences, in a box over Settings (as "Pick the right video"): each crate,
    /// the font and the programs, with its version and licence. Esc, Close or a click beside it
    /// closes it.
    pub(super) fn licences_modal(&mut self, ctx: &egui::Context) {
        if !self.about.licences_open {
            return;
        }
        let p = self.palette;
        let list = licences();
        let mut close = false;
        let modal = fin::dialog(ctx, Id::new("about_licences"), |ui| {
            ui.set_width(480f32.min(ctx.content_rect().width() - 80.0).max(200.0));
            fin::dialog_head(ui, "Open-source licences", Some("Truffle is made with these, each under its own licence."));
            ui.add_space(10.0);
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::ScrollArea::vertical().max_height(LICENCE_ROW * 12.0).auto_shrink([false, true]).show_rows(ui, LICENCE_ROW, list.len(), |ui, range| {
                    for l in &list[range] {
                        licence_row(ui, l);
                    }
                });
            });
            ui.add_space(12.0);
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            let bw = fin::text_button_w(ui, "Close", false);
            close |= fin::text_button(ui, &p, pos2(bar.right() - bw, bar.top()), Id::new("about_licences_close"), "Close", None, false, true).clicked();
        });
        if close || modal.should_close() {
            self.about.licences_open = false;
        }
    }
}

/// The record (`.disc`), `DISC` across around `c` and turned `turn` radians: its soft shadow, the
/// grooves, a faint sheen, a 1 px line inside its edge, the label (56 across, in a 2 px ring of the
/// background) and the hole (8 across, the background).
fn paint_record(painter: &Painter, c: Pos2, turn: f32, label: &LabelFill) {
    let p = crate::ui::theme::palette(painter.ctx());
    let r = DISC / 2.0;
    // `box-shadow: 0 10px 30px rgba(0,0,0,.6)`.
    let shadow = egui::Shadow { offset: [0, 10], blur: SHADOW_BLUR, spread: 0, color: Color32::from_black_alpha(153) };
    painter.add(shadow.as_shape(Rect::from_center_size(c, vec2(DISC, DISC)), CornerRadius::same(r as u8)));
    painter.circle_filled(c, r, GROOVE);
    // The rings the label covers aren't drawn.
    let mut ring = 2.5;
    while ring < r - 1.0 {
        if ring > LABEL / 2.0 + 2.0 {
            painter.circle_stroke(c, ring, Stroke::new(1.0, GROOVE_RING));
        }
        ring += 3.0;
    }
    paint_sheen(painter, c, r, turn);
    // `inset 0 0 0 1px #2a2a2f`.
    painter.circle_stroke(c, r - 0.5, Stroke::new(1.0, p.border));
    painter.circle_filled(c, LABEL / 2.0 + 2.0, p.main_bg);
    paint_label(painter, c, LABEL / 2.0, turn, label);
    painter.circle_filled(c, HOLE / 2.0, p.main_bg);
}

/// A licence's row: the name (12.5 medium) and version (11.5 muted) on the left, the licence on
/// the right (clear of the scroll bar), cut short with "…" when there is no room.
fn licence_row(ui: &mut Ui, l: &Licence) {
    let p = crate::ui::theme::palette(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), LICENCE_ROW), Sense::hover());
    let painter = ui.painter();
    let name = painter.layout_no_wrap(l.name.to_owned(), medium(12.5), p.text);
    let version = painter.layout_no_wrap(l.version.to_owned(), regular(11.5), p.text_weak);
    let left_w = name.size().x + 6.0 + version.size().x;
    let right = rect.right() - 12.0;
    let room = (right - rect.left() - left_w - 16.0).max(40.0);
    let mut job = LayoutJob::single_section(l.licence.to_owned(), TextFormat::simple(regular(11.5), p.text_weak));
    job.wrap = TextWrapping::truncate_at_width(room);
    let licence = painter.layout_job(job);
    let cy = rect.center().y;
    painter.galley(pos2(rect.left(), cy - name.size().y / 2.0), name.clone(), p.text);
    painter.galley(pos2(rect.left() + name.size().x + 6.0, cy - version.size().y / 2.0), version, p.text_weak);
    painter.galley(pos2(right - licence.size().x, cy - licence.size().y / 2.0), licence, p.text_weak);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_thousands_commas() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(4_212), "4,212");
        assert_eq!(fmt_thousands(1_380), "1,380");
        assert_eq!(fmt_thousands(12_345_678), "12,345,678");
        assert_eq!(most_played(52), "most played · 52 times");
        assert_eq!(most_played(1), "most played · 1 time");
        assert_eq!(most_played(1_000), "most played · 1,000 times");
    }

    #[test]
    fn the_version_line_has_the_build_date() {
        assert_eq!(VERSION_LINE, format!("Version {} · built {}", env!("CARGO_PKG_VERSION"), env!("TRUFFLE_BUILD_DATE")));
    }

    #[test]
    fn copied_details_say_what_helps_and_nothing_personal() {
        let helpers = Helpers {
            ytdlp: Some("2026.10.21".to_owned()),
            js: Some(("Node.js".to_owned(), "24.19.0".to_owned())),
            missing: None,
        };
        let text = details_text("Windows 11 25H2 (build 26200.6899)", Some(true), SoundSource::Spotify, Some(&helpers));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                format!("Truffle {}, built {}", env!("CARGO_PKG_VERSION"), env!("TRUFFLE_BUILD_DATE")).as_str(),
                "Windows 11 25H2 (build 26200.6899)",
                "Account: Premium",
                "Sound from Spotify",
                "yt-dlp 2026.10.21",
                "Node.js 24.19.0",
            ]
        );
        let missing = Helpers { ytdlp: Some(String::new()), js: None, missing: Some("yt-dlp needs Node.js or Deno to read YouTube.".to_owned()) };
        let text = details_text("Windows 10 22H2 (build 19045)", Some(false), SoundSource::YouTube, Some(&missing));
        assert!(text.contains("Account: Free\nSound from YouTube\nyt-dlp\nJavaScript runtime not found\n"), "{text}");
        let text = details_text("Windows", None, SoundSource::Spotify, None);
        assert!(text.contains("yt-dlp not checked yet"), "{text}");
        // The real thing: no user name, folder, drive or key in it.
        let real = details_text(&crate::platform::windows_version(), Some(true), SoundSource::Spotify, Some(&helpers));
        for leak in ['\\', '/', '@'] {
            assert!(!real.contains(leak), "{real}");
        }
        let user = std::env::var("USERNAME").unwrap_or_default();
        assert!(user.len() < 3 || !real.to_lowercase().contains(&user.to_lowercase()), "{real}");
        assert!(!real.contains(":\\") && !real.contains("Users"), "{real}");
    }

    #[test]
    fn the_licences_list_every_crate_and_the_rest() {
        let list = licences();
        assert!(list.len() > 100, "only {} licences", list.len());
        let has = |name: &str| list.iter().any(|l| l.name == name);
        for name in ["librespot-core", "librespot-playback", "egui", "eframe", "Inter", "yt-dlp", "Node.js"] {
            assert!(has(name), "{name} isn't listed");
        }
        assert_eq!(list.iter().find(|l| l.name == "Inter").map(|l| l.licence), Some("OFL-1.1"));
        assert_eq!(list.iter().find(|l| l.name == "yt-dlp").map(|l| l.licence), Some("Unlicense"));
        assert_eq!(list.iter().find(|l| l.name == "Node.js").map(|l| l.licence), Some("MIT"));
        for l in list {
            assert!(!l.licence.is_empty(), "{} has no licence", l.name);
        }
        // Truffle itself is not one of its own dependencies.
        assert!(!has("truffle"));
    }

    #[test]
    fn copy_details_waits_for_the_helpers() {
        let mut about = AboutState::default();
        // Clicked before the versions came: nothing yet, and copied once they come.
        assert!(!about.copy_asked());
        assert!(about.helpers_came(Helpers::default()));
        // Known now: copied at once, and nothing is left waiting.
        assert!(about.copy_asked());
        assert!(!about.helpers_came(Helpers::default()));
    }

    #[test]
    fn the_most_played_song_stays_while_it_is_looked_up_again() {
        let track = |uri: &str, name: &str| crate::model::TrackInfo {
            uri: uri.to_owned(),
            name: name.to_owned(),
            artists: Vec::new(),
            album: String::new(),
            album_uri: String::new(),
            cover_small: Some("https://i.scdn.co/image/b".to_owned()),
            cover_large: None,
            duration_ms: 200_000,
            playable: true,
            is_local: false,
            added_at: 0,
        };
        let song = |uri: &str, name: Option<&str>| PlayStats {
            plays: 56,
            songs: 3,
            top: Some(crate::model::TopSong { uri: uri.to_owned(), plays: 52, track: name.map(|n| track(uri, n)) }),
        };
        let name = |about: &AboutState| about.stats.as_ref().and_then(|s| s.top.as_ref()?.track.as_ref()).map(|t| t.name.clone());
        let mut about = AboutState::default();
        about.stats_came(song("spotify:track:b", Some("im #obsessed w u lol")));
        // The same song, not described yet this time: its name stays.
        about.stats_came(song("spotify:track:b", None));
        assert_eq!(name(&about).as_deref(), Some("im #obsessed w u lol"));
        // Another song is the most played now: not given the old one's name.
        about.stats_came(song("spotify:track:a", None));
        assert_eq!(name(&about), None);
    }

    #[test]
    fn the_record_shadow_fades_like_the_mockups() {
        // CSS's 30 px blur: a bell curve with a spread (sigma) of 15 across the shadow's edge, so
        // the shadow fades from 0 (far out) to 1 (far in) as the curve's running total.
        let sigma = 15.0_f64;
        let step = 0.05;
        let xs: Vec<f64> = (-1600..=1600).map(|k| f64::from(k) * step).collect();
        let bell = |x: f64| (-x * x / (2.0 * sigma * sigma)).exp() / (sigma * (2.0 * std::f64::consts::PI).sqrt());
        let mut total = 0.0;
        let css: Vec<f64> = xs.iter().map(|&x| {
            total += (bell(x - step) + bell(x)) / 2.0 * step;
            total
        }).collect();
        // egui's: a straight line from out to in across `blur`, centred on the edge.
        let off = |blur: u8| xs.iter().zip(&css).map(|(x, c)| ((0.5 + x / f64::from(blur)).clamp(0.0, 1.0) - c).powi(2)).sum::<f64>();
        let nearest = (10..=90).min_by(|a, b| off(*a).total_cmp(&off(*b)));
        assert_eq!(nearest, Some(SHADOW_BLUR));
        // Copying CSS's number straight over fades in half the room.
        assert!(off(30) > 5.0 * off(SHADOW_BLUR));
    }

    #[test]
    fn the_accent_gradient_is_the_mockups_for_its_accent() {
        let (a, b) = accent_gradient(Color32::from_rgb(0xb5, 0x6c, 0xff));
        let near = |c: Color32, want: [u8; 3]| c.r().abs_diff(want[0]) <= 2 && c.g().abs_diff(want[1]) <= 2 && c.b().abs_diff(want[2]) <= 2;
        assert!(near(a, [0xff, 0x8f, 0xd1]), "{a:?}");
        assert!(near(b, [0x7b, 0x5c, 0xff]), "{b:?}");
        // From the top-left corner to the bottom-right one.
        assert!((gradient_at(vec2(-10.0, -10.0), 10.0)).abs() < 1e-6);
        assert!((gradient_at(vec2(10.0, 10.0), 10.0) - 1.0).abs() < 1e-6);
        assert!((gradient_at(vec2(10.0, -10.0), 10.0) - 0.5).abs() < 1e-6);
    }
}
