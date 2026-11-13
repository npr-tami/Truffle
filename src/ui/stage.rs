//! The Now playing view (docs/mockups/now-playing.html, built 1:1): the playing song's cover on the
//! left, its lyrics on the right and a small player card in the corner, over the whole window (not
//! fullscreen; F11 makes it that while it is open). Its glow and the play button take the cover's
//! colour; the lyrics move up as they are sung, the coming lines fainter and blurred; the card is
//! frosted glass. Ctrl+I or the player bar's button opens it, Esc or the arrow at the top left closes
//! it, and with the mouse still for 3 s the card and the title bar fade away.
//!
//! Its sizes are the design's, which was drawn 1000 wide: they grow and shrink with the window's width
//! (`k` points to a design pixel). Its blur is real (`blur.rs`), as in the design.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use egui::emath::{GuiRounding as _, lerp};
use egui::epaint::{TextShape, Vertex};
use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, Color32, CornerRadius, CursorIcon, Galley, Id, Mesh, Rect, Response, Sense, Stroke, StrokeKind, TextureHandle, TextureId, TextureOptions, Ui, Vec2, ViewportCommand, pos2, vec2};

use super::blur::{self, Job, Source};
use super::images::Swatch;
use super::panel::{Lyrics, line_at};
use super::theme::{segoe, segoe_black, segoe_bold};
use super::tracks::one_line;
use super::util::fmt_ms;
use super::{TruffleApp, fin, motion, svgicon};
use crate::model::{Command, LyricLine, PlayStatus, RepeatMode};
use crate::video::{Frame, VideoLoop};

/// The width the design was drawn at: its sizes scale with the window's width from there.
const DESIGN_W: f32 = 1000.0;
/// The design's height at that width (16:10).
const DESIGN_H: f32 = 625.0;
/// Below this the sizes stop shrinking (the card would get too small to use in a small window).
const MIN_K: f32 = 0.6;

const BG: Color32 = Color32::from_rgb(11, 11, 13);
/// `--text` and `--muted`.
const TEXT: Color32 = Color32::from_rgb(232, 232, 234);
const MUTED: Color32 = Color32::from_rgb(141, 141, 150);
/// The card: `rgba(21,21,23,.72)` with a `rgba(255,255,255,.08)` edge.
const CARD_BG: Color32 = Color32::from_rgba_premultiplied(15, 15, 17, 184);
const CARD_EDGE: Color32 = Color32::from_rgba_premultiplied(20, 20, 20, 20);
/// A hovered button's box (`rgba(255,255,255,.07)`), and Windows' red under the close button.
const HOVER_BG: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);
const CLOSE_RED: Color32 = Color32::from_rgb(196, 43, 28);
/// A line's track (`rgba(255,255,255,.14)`).
const LINE_BG: Color32 = Color32::from_rgba_premultiplied(36, 36, 36, 36);

/// CSS's `ease` (its transitions' default), `ease-in-out`, and the design's own curve for things that
/// move (`cubic-bezier(.2,.7,.2,1)`).
const EASE: [f32; 4] = [0.25, 0.1, 0.25, 1.0];
const EASE_IN_OUT: [f32; 4] = [0.42, 0.0, 0.58, 1.0];
const GLIDE: [f32; 4] = [0.2, 0.7, 0.2, 1.0];

/// The card's text sizes and line height (the design's body: 1.45).
const TITLE: f32 = 15.0;
const ARTIST: f32 = 11.5;
const TIME: f32 = 10.5;
const LINE_H: f32 = 1.45;
/// A lyric line's sizes: the one being sung, the rest; their line height and the gap between them.
const SUNG: f32 = 34.0;
const OTHER: f32 = 22.0;
const LYRIC_H: f32 = 1.17;
const LYRIC_GAP: f32 = 13.0;
/// Lyrics without times start this far down (of the view's height), clear of the title bar.
const UNTIMED_TOP: f32 = 0.12;

/// The view's state, kept while Truffle runs.
pub(super) struct Stage {
    pub open: bool,
    /// The first pass since it opened: everything starts where it belongs, without moving there.
    fresh: bool,
    /// When it opened: the cover's drift and the glow's breathing count from then.
    opened_at: f64,
    /// F11 made the window fullscreen here: it goes back as the view closes.
    fullscreen: bool,
    /// The card's lyrics button turned them off.
    pub lyrics_off: bool,
    /// The song's colour last worked out (kept while the next cover is decoded).
    song: Option<Color32>,
    /// The song the lines' motion is for, and how many lines it has.
    lines_for: Option<(String, usize)>,
    lines: Vec<LineMotion>,
    /// How far lyrics without times are scrolled.
    scroll: f32,
    /// 0-1: the lyrics shown; the darkening behind them; the cover moved to the middle (no lyrics).
    lyrics: Tween,
    scrim: Tween,
    middle: Tween,
    /// 0-1: the cover shown, rather than the song's Canvas.
    cover: Tween,
    /// Each song's Canvas as far as it is known: asked for once (`None` while the answer comes).
    canvases: HashMap<String, Option<CanvasFile>>,
    /// The playing song's Canvas video, while the view shows it.
    player: Option<CanvasPlayer>,
}

/// A song's Canvas: a video in the cache, a picture to load, or none.
#[derive(Clone, Debug, PartialEq)]
enum CanvasFile {
    None,
    Video(PathBuf),
    Picture(String),
}

/// A Canvas video playing: its frames, the texture they go to, how far it has played (it goes on
/// only while the music plays), and a frame decoded ahead that isn't due yet.
struct CanvasPlayer {
    uri: String,
    video: VideoLoop,
    texture: Option<TextureHandle>,
    time: f64,
    last: f64,
    waiting: Option<Frame>,
}

impl Stage {
    /// What the controller found for a song's Canvas.
    pub fn canvas_came(&mut self, uri: String, video: Option<PathBuf>, picture: Option<String>) {
        let file = match (video, picture) {
            (Some(path), _) => CanvasFile::Video(path),
            (None, Some(url)) => CanvasFile::Picture(url),
            (None, None) => CanvasFile::None,
        };
        self.canvases.insert(uri, Some(file));
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self {
            open: false,
            fresh: true,
            opened_at: 0.0,
            fullscreen: false,
            lyrics_off: false,
            song: None,
            lines_for: None,
            lines: Vec::new(),
            scroll: 0.0,
            lyrics: Tween::at(1.0, EASE),
            scrim: Tween::at(1.0, EASE),
            middle: Tween::at(0.0, GLIDE),
            cover: Tween::at(1.0, EASE),
            canvases: HashMap::new(),
            player: None,
        }
    }
}

/// A value moving as a CSS transition does: from where it was toward a new target, over `secs` along
/// `curve`; a new target on the way starts from where it got to.
#[derive(Clone, Copy, Debug)]
struct Tween {
    from: f32,
    to: f32,
    start: f64,
    secs: f32,
    curve: [f32; 4],
}

impl Tween {
    fn at(value: f32, curve: [f32; 4]) -> Self {
        Self { from: value, to: value, start: f64::NEG_INFINITY, secs: 0.0, curve }
    }

    fn value(&self, now: f64) -> f32 {
        if self.secs <= 0.0 {
            return self.to;
        }
        let t = ((now - self.start) / f64::from(self.secs)).clamp(0.0, 1.0) as f32;
        self.from + (self.to - self.from) * bezier(self.curve, t)
    }

    fn moving(&self, now: f64) -> bool {
        self.secs > 0.0 && now - self.start < f64::from(self.secs)
    }

    /// Heads for `to` over `secs` (at once when 0).
    fn set(&mut self, to: f32, now: f64, secs: f32) {
        if to == self.to {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.start = now;
        self.secs = secs;
    }

    fn jump(&mut self, to: f32) {
        *self = Self::at(to, self.curve);
    }
}

/// One lyric line's place (its top, from the view's), size, opacity and blur (design pixels).
#[derive(Clone, Copy, Debug)]
struct LineMotion {
    top: Tween,
    size: Tween,
    opacity: Tween,
    blur: Tween,
}

impl LineMotion {
    fn new() -> Self {
        Self { top: Tween::at(0.0, GLIDE), size: Tween::at(OTHER, GLIDE), opacity: Tween::at(0.0, EASE), blur: Tween::at(0.0, EASE) }
    }

    fn tweens(&mut self) -> [&mut Tween; 4] {
        [&mut self.top, &mut self.size, &mut self.opacity, &mut self.blur]
    }

    fn moving(&self, now: f64) -> bool {
        [self.top, self.size, self.opacity, self.blur].iter().any(|t| t.moving(now))
    }
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` at `t` (0-1 of the time): how far along the value is.
fn bezier([x1, y1, x2, y2]: [f32; 4], t: f32) -> f32 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let (cx, cy) = (3.0 * x1, 3.0 * y1);
    let (bx, by) = (3.0 * (x2 - x1) - cx, 3.0 * (y2 - y1) - cy);
    let (ax, ay) = (1.0 - cx - bx, 1.0 - cy - by);
    let x = |s: f32| ((ax * s + bx) * s + cx) * s;
    // The curve's parameter where it is `t` across: Newton's method, or halving where that strays.
    let mut s = t;
    for _ in 0..8 {
        let slope = (3.0 * ax * s + 2.0 * bx) * s + cx;
        let err = x(s) - t;
        if err.abs() < 1e-6 || slope.abs() < 1e-6 {
            break;
        }
        s -= err / slope;
    }
    if !(0.0..=1.0).contains(&s) || (x(s) - t).abs() > 1e-4 {
        let (mut lo, mut hi) = (0.0, 1.0);
        for _ in 0..32 {
            s = (lo + hi) / 2.0;
            if x(s) < t {
                lo = s;
            } else {
                hi = s;
            }
        }
    }
    ((ay * s + by) * s + cy) * s
}

/// How far an `infinite alternate` CSS animation `secs` long is `elapsed` seconds in (0-1), with
/// `curve` both ways.
fn swing(elapsed: f64, secs: f32, curve: [f32; 4]) -> f32 {
    let p = (elapsed / f64::from(secs)).rem_euclid(2.0) as f32;
    bezier(curve, if p < 1.0 { p } else { 2.0 - p })
}

/// The design's frame in a window `full`: 16:10, as big as fits and centred. The view keeps the
/// design's shape at any window size, nothing stretched; around it is only the background.
fn design_frame(full: Rect) -> Rect {
    let w = full.width().min(full.height() * DESIGN_W / DESIGN_H);
    Rect::from_center_size(full.center(), vec2(w, w * DESIGN_H / DESIGN_W))
}

/// Design pixels to points in a frame `width` wide.
fn scale(width: f32) -> f32 {
    (width / DESIGN_W).max(MIN_K)
}

/// The lyrics column: from 51 % across to 5 % from the right.
fn lyrics_column(full: Rect) -> (f32, f32) {
    (full.left() + full.width() * 0.51, full.right() - full.width() * 0.05)
}

/// A line `d` lines after the sung one (before it when negative): its opacity and its blur.
fn line_look(d: isize) -> (f32, f32) {
    match d {
        0 => (1.0, 0.0),
        1 => (0.5, 1.0),
        2 => (0.3, 2.2),
        3 => (0.16, 3.6),
        -1 => (0.14, 2.6),
        _ => (0.0, 5.0),
    }
}

/// How many lines line `i` is after the sung one; before any is sung, as if one before the first was.
fn distance(i: usize, cur: Option<usize>) -> isize {
    i as isize - cur.map_or(-1, |c| c as isize)
}

/// Each line's top (from the view's top) for lines `heights` tall: the sung one's middle 42 % down,
/// the rest `gap` apart above and below it. Before any is sung, the first sits as if after a sung
/// line with no height.
fn line_tops(heights: &[f32], cur: Option<usize>, view_h: f32, gap: f32) -> Vec<f32> {
    let anchor = view_h * 0.42;
    let mut tops = vec![0.0; heights.len()];
    if heights.is_empty() {
        return tops;
    }
    let (c, top) = match cur.filter(|&c| c < heights.len()) {
        Some(c) => (c, anchor - heights[c] / 2.0),
        None => (0, anchor + gap),
    };
    tops[c] = top;
    let mut y = top + heights[c] + gap;
    for (t, h) in tops[c + 1..].iter_mut().zip(&heights[c + 1..]) {
        *t = y;
        y += h + gap;
    }
    let mut y = top;
    for (t, h) in tops[..c].iter_mut().zip(&heights[..c]).rev() {
        y -= gap + h;
        *t = y;
    }
    tops
}

/// Each line's top for lines `heights` tall, one under another from `top`, `gap` apart.
fn still_tops(heights: &[f32], top: f32, gap: f32) -> Vec<f32> {
    let mut y = top;
    heights
        .iter()
        .map(|h| {
            let t = y;
            y += h + gap;
            t
        })
        .collect()
}

/// The cover's box: 60 % wide and the view's height, its left 1 % out (20 % in, in the middle,
/// without lyrics: `middle` of the way there).
fn cover_box(full: Rect, middle: f32) -> Rect {
    let w = full.width();
    Rect::from_min_size(pos2(full.left() + w * lerp(-0.01..=0.20, middle), full.top()), vec2(w * 0.60, full.height()))
}

/// The Canvas's box (`#viz`): 62 % × 96 %, 3 % out at the left, 2 % down (20 % in without lyrics).
fn canvas_box(full: Rect, middle: f32) -> Rect {
    let (w, h) = (full.width(), full.height());
    Rect::from_min_size(pos2(full.left() + w * lerp(-0.03..=0.20, middle), full.top() + h * 0.02), vec2(w * 0.62, h * 0.96))
}

/// The cover's glow: 76 % × 116 %, 8 % out at the left and the top (12 % in without lyrics).
fn cover_glow_box(full: Rect, middle: f32) -> Rect {
    let (w, h) = (full.width(), full.height());
    Rect::from_min_size(pos2(full.left() + w * lerp(-0.08..=0.12, middle), full.top() - h * 0.08), vec2(w * 0.76, h * 1.16))
}

/// The song's glow: 78 % × 95 %, 18 % out at the left and 40 % below the bottom.
fn glow_box(full: Rect) -> Rect {
    let (w, h) = (full.width(), full.height());
    Rect::from_min_size(pos2(full.left() - w * 0.18, full.top() + h * 0.45), vec2(w * 0.78, h * 0.95))
}

/// The part of an image `tex` big that `background-size: cover` shows in a box `size` (centred).
fn cover_uv(tex: Vec2, size: Vec2) -> Rect {
    let (image, frame) = (tex.x / tex.y.max(1.0), size.x / size.y.max(1.0));
    if frame > image {
        let v = image / frame;
        Rect::from_min_max(pos2(0.0, 0.5 - v / 2.0), pos2(1.0, 0.5 + v / 2.0))
    } else {
        let u = frame / image;
        Rect::from_min_max(pos2(0.5 - u / 2.0, 0.0), pos2(0.5 + u / 2.0, 1.0))
    }
}

/// A picture in `rect` (showing `uv` of it) at `opacity`, faded by the design's mask: solid to
/// `solid` of the way out, then gone at the edge (`radial-gradient(ellipse 50% 50%, #000 50%,
/// transparent 100%)` for the cover: `solid` 0.5; 0.55 for the Canvas), as a fine grid.
fn masked_cover(texture: TextureId, rect: Rect, uv: Rect, opacity: f32, solid: f32) -> Mesh {
    const N: u32 = 32;
    let mut mesh = Mesh::with_texture(texture);
    for j in 0..=N {
        for i in 0..=N {
            let (u, v) = (i as f32 / N as f32, j as f32 / N as f32);
            let r = ((u - 0.5) * 2.0).hypot((v - 0.5) * 2.0);
            let a = ((1.0 - r) / (1.0 - solid)).clamp(0.0, 1.0) * opacity;
            mesh.vertices.push(Vertex {
                pos: pos2(lerp(rect.x_range(), u), lerp(rect.y_range(), v)),
                uv: pos2(lerp(uv.x_range(), u), lerp(uv.y_range(), v)),
                color: Color32::WHITE.gamma_multiply(a),
            });
        }
    }
    for j in 0..N {
        for i in 0..N {
            let a = j * (N + 1) + i;
            let c = a + N + 1;
            mesh.add_triangle(a, a + 1, c);
            mesh.add_triangle(a + 1, c + 1, c);
        }
    }
    mesh
}

/// An ellipse filling `rect`, `color` in the middle fading to nothing at its edge (CSS
/// `radial-gradient(closest-side, color, transparent)`).
fn radial_mesh(rect: Rect, color: Color32) -> Mesh {
    const RINGS: u32 = 16;
    const SEGMENTS: u32 = 72;
    let mut mesh = Mesh::default();
    let c = rect.center();
    mesh.colored_vertex(c, color);
    for ring in 1..=RINGS {
        let r = ring as f32 / RINGS as f32;
        for s in 0..SEGMENTS {
            let a = s as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            mesh.colored_vertex(c + vec2(a.cos() * rect.width(), a.sin() * rect.height()) * (r / 2.0), color.gamma_multiply(1.0 - r));
        }
    }
    for s in 0..SEGMENTS {
        mesh.add_triangle(0, 1 + s, 1 + (s + 1) % SEGMENTS);
    }
    for ring in 1..RINGS {
        let (inner, outer) = (1 + (ring - 1) * SEGMENTS, 1 + ring * SEGMENTS);
        for s in 0..SEGMENTS {
            let t = (s + 1) % SEGMENTS;
            mesh.add_triangle(inner + s, inner + t, outer + s);
            mesh.add_triangle(inner + t, outer + t, outer + s);
        }
    }
    mesh
}

/// The darkening behind the lyrics: nothing to 38 % across, then 50 % at 58 % and 72 % at the right.
fn scrim_mesh(full: Rect, opacity: f32) -> Mesh {
    let mut mesh = Mesh::default();
    for (x, a) in [(0.0, 0.0), (0.38, 0.0), (0.58, 0.5), (1.0, 0.72)] {
        let color = BG.gamma_multiply(a * opacity);
        let x = lerp(full.x_range(), x);
        mesh.colored_vertex(pos2(x, full.top()), color);
        mesh.colored_vertex(pos2(x, full.bottom()), color);
    }
    for i in 0..3 {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 2);
        mesh.add_triangle(a + 1, a + 3, a + 2);
    }
    mesh
}

/// A lyric line as the design lays it out: Segoe UI Black at `size`, its lines 1.17 apart, its letters
/// 1 % of the size closer, its rows ending where CSS ends them in `width` (see [`css_rows`]).
fn lay(ui: &Ui, text: &str, size: f32, width: f32) -> Arc<Galley> {
    let format = TextFormat { font_id: segoe_black(size), color: Color32::WHITE, extra_letter_spacing: -0.01 * size, line_height: Some(LYRIC_H * size), ..Default::default() };
    let line = ui.fonts_mut(|f| f.layout_job(LayoutJob::single_section(text.to_owned(), format.clone())));
    let job = match css_rows(&line, text, format.extra_letter_spacing, width) {
        Some(rows) => LayoutJob::single_section(rows, format),
        None => {
            let mut job = LayoutJob::single_section(text.to_owned(), format);
            job.wrap.max_width = width;
            job
        }
    };
    ui.fonts_mut(|f| f.layout_job(job))
}

/// Where a lyric line's row may end: at spaces (they go) or after a hyphen between letters (it stays).
#[derive(Clone, Copy, PartialEq)]
enum Chance {
    Space,
    Hyphen,
}

/// `text`, laid out on one row as `line`, with a newline where CSS ends its rows in `width`: after the
/// last word that fits, the spacing after its last letter counted and the space after it not. egui's
/// own wrapping wants that space to fit too, so it ends such rows a word early. `None` for text CSS
/// also breaks between letters (Chinese, Japanese, Korean, Thai): egui wraps that.
fn css_rows(line: &Galley, text: &str, spacing: f32, width: f32) -> Option<String> {
    let [row] = line.rows.as_slice() else { return None };
    let glyphs = &row.glyphs;
    if glyphs.is_empty() || text.chars().any(breaks_between_letters) {
        return None;
    }
    // In the text: each chance, and the bytes a newline takes the place of there.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut cuts = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        if c == ' ' {
            let end = chars[i..].iter().position(|&(_, c)| c != ' ').map_or(chars.len(), |n| i + n);
            if i > 0 && end < chars.len() {
                cuts.push((Chance::Space, at, chars[end].0));
            }
            i = end;
        } else {
            if c == '-' && i > 0 && chars[i - 1].1.is_alphabetic() && chars.get(i + 1).is_some_and(|n| n.1.is_alphabetic()) {
                cuts.push((Chance::Hyphen, at + 1, at + 1));
            }
            i += 1;
        }
    }
    // In the glyphs: how wide a row ending there is as CSS counts it, from the line's start (the
    // spacing after its last letter in), and where the next row would start.
    let mut starts = Vec::with_capacity(glyphs.len());
    let mut x = 0.0;
    for (j, g) in glyphs.iter().enumerate() {
        if j > 0 {
            x += spacing;
        }
        starts.push(x);
        x += g.advance_width;
    }
    let edge = |j: usize| starts[j] + glyphs[j].advance_width + spacing;
    let mut spots = Vec::new();
    let mut j = 0;
    while j < glyphs.len() {
        let c = glyphs[j].chr;
        if c == ' ' {
            let end = glyphs[j..].iter().position(|g| g.chr != ' ').map_or(glyphs.len(), |n| j + n);
            if j > 0 && end < glyphs.len() {
                spots.push((Chance::Space, edge(j - 1), starts[end]));
            }
            j = end;
        } else {
            if c == '-' && j > 0 && glyphs[j - 1].chr.is_alphabetic() && glyphs.get(j + 1).is_some_and(|n| n.chr.is_alphabetic()) {
                spots.push((Chance::Hyphen, edge(j), starts[j + 1]));
            }
            j += 1;
        }
    }
    if spots.len() != cuts.len() || spots.iter().zip(&cuts).any(|(s, c)| s.0 != c.0) {
        return None;
    }
    // As CSS: each row takes the words that fit; a word wider than a row gets one of its own.
    let total = edge(glyphs.len() - 1);
    let (mut from, mut fits, mut ends): (f32, Option<usize>, Vec<usize>) = (0.0, None, Vec::new());
    for k in 0..=spots.len() {
        let end = spots.get(k).map_or(total, |s| s.1);
        if end - from > width {
            if let Some(b) = fits.take() {
                ends.push(b);
                from = spots[b].2;
            }
            if end - from > width && k < spots.len() {
                ends.push(k);
                from = spots[k].2;
                continue;
            }
        }
        if k < spots.len() {
            fits = Some(k);
        }
    }
    let mut out = String::with_capacity(text.len() + ends.len());
    let mut copied = 0;
    for k in ends {
        let (_, cut, resume) = cuts[k];
        out.push_str(&text[copied..cut]);
        out.push('\n');
        copied = resume;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

/// Letters CSS may end a row between, no space needed: Chinese, Japanese, Korean, Thai, Lao,
/// Khmer, Myanmar.
fn breaks_between_letters(c: char) -> bool {
    matches!(c as u32, 0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1100..=0x11FF | 0x1780..=0x17FF | 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FA1F)
}

/// How far below its line box's top a galley goes for its first baseline to sit where CSS puts it: the
/// font's ascent, after half of what the line height leaves around the font's own height.
fn baseline_shift(galley: &Galley, size: f32) -> f32 {
    let Some((row, glyph)) = galley.rows.iter().find_map(|r| r.glyphs.first().map(|g| (r, g))) else { return 0.0 };
    (LYRIC_H * size - glyph.font_height) / 2.0 + glyph.font_ascent - (row.pos.y + glyph.pos.y)
}

/// The title bar's parts: the bar (42 tall), the arrow back (32, 10 in) and the window's three
/// buttons (32, 3 apart, 10 from the right).
fn bar_parts(full: Rect, k: f32) -> (Rect, Rect, [Rect; 3]) {
    let bar = Rect::from_min_size(full.min, vec2(full.width(), 42.0 * k));
    let cy = bar.center().y;
    let side = vec2(32.0 * k, 32.0 * k);
    let leave = Rect::from_center_size(pos2(bar.left() + 26.0 * k, cy), side);
    let buttons = std::array::from_fn(|i| Rect::from_center_size(pos2(bar.right() - 26.0 * k - (2 - i) as f32 * 35.0 * k, cy), side));
    (bar, leave, buttons)
}

/// The card's height: its rows (1.45 line height), their gaps, its padding and its 1-point edges.
fn card_height(k: f32) -> f32 {
    let rows = (TITLE + ARTIST) * LINE_H + 2.0 + TIME * LINE_H + 36.0 + 15.0;
    let gaps = 12.0 + 10.0 + 9.0;
    (15.0 + rows + gaps + 13.0) * k + 2.0
}

/// The card: 330 wide, 30 from the left and the bottom.
fn card_rect(full: Rect, k: f32) -> Rect {
    let bottom = full.bottom() - 30.0 * k;
    Rect::from_min_max(pos2(full.left() + 30.0 * k, bottom - card_height(k)), pos2(full.left() + 360.0 * k, bottom))
}

/// Where the card's parts go (the design's flex rows, worked out).
#[derive(Debug)]
struct CardParts {
    content: Rect,
    /// The title's and the artists' line boxes.
    title: Rect,
    artist: Rect,
    /// Like, add to playlist, lyrics on or off, more (27 each, 1 apart, reaching 6 into the padding).
    acts: [Rect; 4],
    /// The seek row's middle.
    seek_y: f32,
    /// Shuffle, previous, play, next, repeat (30, the play button 36 with 8 more either side; 3
    /// apart, centred).
    ctl: [Rect; 5],
    vol_icon: Rect,
    /// The volume line itself (120 × 3.2).
    vol_bar: Rect,
}

fn card_parts(card: Rect, k: f32) -> CardParts {
    let content = Rect::from_min_max(card.min + vec2(1.0 + 17.0 * k, 1.0 + 15.0 * k), card.max - vec2(1.0 + 17.0 * k, 1.0 + 13.0 * k));
    let acts_w = (4.0 * 27.0 + 3.0) * k;
    let acts_right = content.right() + 6.0 * k;
    let acts = std::array::from_fn(|i| Rect::from_min_size(pos2(acts_right - acts_w + i as f32 * 28.0 * k, content.top()), vec2(27.0 * k, 27.0 * k)));
    let meta_right = acts_right - acts_w - 10.0 * k;
    let title = Rect::from_min_max(content.min, pos2(meta_right, content.top() + TITLE * LINE_H * k));
    let artist = Rect::from_min_max(pos2(content.left(), title.bottom() + 2.0 * k), pos2(meta_right, title.bottom() + (2.0 + ARTIST * LINE_H) * k));
    let seek_top = artist.bottom().max(content.top() + 27.0 * k) + 12.0 * k;
    let seek_y = seek_top + TIME * LINE_H * k / 2.0;
    let ctl_y = seek_top + (TIME * LINE_H + 10.0 + 18.0) * k;
    let widths = [30.0, 30.0, 36.0, 30.0, 30.0];
    let group = widths.iter().sum::<f32>() + 4.0 * 3.0 + 2.0 * 8.0;
    let mut x = content.center().x - group * k / 2.0;
    let ctl = std::array::from_fn(|i| {
        let margin = if i == 2 { 8.0 * k } else { 0.0 };
        let w = widths[i] * k;
        let r = Rect::from_center_size(pos2(x + margin + w / 2.0, ctl_y), vec2(w, w));
        x += w + 2.0 * margin + 3.0 * k;
        r
    });
    let vol_y = ctl_y + (18.0 + 9.0 + 7.5) * k;
    let vol_icon = Rect::from_center_size(pos2(content.left() + 7.5 * k, vol_y), vec2(15.0 * k, 15.0 * k));
    let vol_bar = Rect::from_min_size(pos2(vol_icon.right() + 9.0 * k, vol_y - 1.6 * k), vec2(120.0 * k, 3.2 * k));
    CardParts { content, title, artist, acts, seek_y, ctl, vol_icon, vol_bar }
}

/// A card button (`.sb` 27 and `.ib` 30): its icon in `color`, white on a faint box while hovered
/// (unless it is `on`: then it keeps `color`, as `.ib.on` does).
#[allow(clippy::too_many_arguments)]
fn card_button(ui: &Ui, rect: Rect, id: Id, icon: &str, icon_px: f32, color: Color32, on: bool, enabled: bool, tip: &str, k: f32) -> Response {
    let resp = ui.interact(rect, id, if enabled { Sense::click() } else { Sense::hover() });
    let hovered = enabled && resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same((6.0 * k).round() as u8), HOVER_BG);
    }
    let fg = match (enabled, hovered && !on) {
        (false, _) => color.gamma_multiply(0.4),
        (true, true) => Color32::WHITE,
        (true, false) => color,
    };
    svgicon::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(icon_px, icon_px)), icon, fg, svgicon::STROKE);
    if enabled { resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip) } else { resp }
}

/// A card line (`.bar`: 3.2 tall, white at 14 % with the part done in `fill`), taking clicks and drags
/// 8 above and below it; a knob shows while it is hovered or held, as on Truffle's other lines. Returns
/// its response and, while held, where (0-1).
fn card_line(ui: &Ui, bar: Rect, id: Id, value: f32, enabled: bool, fill: Color32, k: f32) -> (Response, Option<f32>) {
    let resp = ui.interact(bar.expand2(vec2(0.0, 8.0 * k)), id, if enabled { Sense::click_and_drag() } else { Sense::hover() });
    // Only the left button moves it: the right one opens the box to type a volume in.
    let held = enabled && ui.input(|i| i.pointer.primary_down()) && (resp.is_pointer_button_down_on() || resp.dragged_by(egui::PointerButton::Primary));
    let at = if held { resp.interact_pointer_pos().map(|p| ((p.x - bar.left()) / bar.width().max(1.0)).clamp(0.0, 1.0)) } else { None };
    let v = at.unwrap_or(value).clamp(0.0, 1.0);
    let round = CornerRadius::same((2.0 * k).round() as u8);
    ui.painter().rect_filled(bar, round, LINE_BG);
    if v > 0.0 {
        ui.painter().rect_filled(Rect::from_min_max(bar.min, pos2(bar.left() + bar.width() * v, bar.bottom())), round, fill);
    }
    let knob = motion::hover(ui.ctx(), id, enabled && (resp.hovered() || held));
    if knob > 0.0 {
        let c = pos2(bar.left() + bar.width() * v, bar.center().y);
        ui.painter().circle_filled(c + vec2(0.0, 1.0), 5.5 * k, Color32::from_black_alpha(150).gamma_multiply(knob));
        ui.painter().circle_filled(c, 5.0 * k, Color32::WHITE.gamma_multiply(knob));
    }
    let resp = if enabled { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    (resp, at)
}

impl TruffleApp {
    /// Ctrl+I and the player bar's button.
    pub(super) fn toggle_stage(&mut self, ctx: &egui::Context) {
        if self.stage.open {
            self.close_stage(ctx);
        } else {
            self.show_stage(ctx);
            self.open_stage_window(ctx);
        }
    }

    /// The view shows, in the window as it is (pictures keep the size they asked for).
    pub(super) fn show_stage(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let st = &mut self.stage;
        st.open = true;
        st.fresh = true;
        st.opened_at = now;
        st.lines_for = None;
    }

    /// Esc, the arrow at the top left and Ctrl+I: back to the window as it was (out of fullscreen if
    /// F11 made it that here).
    pub(super) fn close_stage(&mut self, ctx: &egui::Context) {
        if !self.stage.open {
            return;
        }
        self.stage.open = false;
        // Its video stops decoding.
        self.stage.player = None;
        self.stage.fullscreen = false;
        self.close_stage_window(ctx);
    }

    /// Closes the view for the mini player, which takes the window over: the window the view came
    /// from, to come back to.
    pub(super) fn drop_stage(&mut self) -> Option<super::shell::FullGeometry> {
        self.stage.open = false;
        self.stage.player = None;
        self.stage.fullscreen = false;
        self.take_stage_window()
    }

    /// The Canvas texture's size while one plays (for the memory report).
    pub(super) fn canvas_texture_size(&self) -> Option<[usize; 2]> {
        self.stage.player.as_ref()?.texture.as_ref().map(|t| t.size())
    }

    /// F11 while it is open: fullscreen, or back.
    pub(super) fn stage_fullscreen(&mut self, ctx: &egui::Context) {
        let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!full));
        self.stage.fullscreen = !full;
    }

    /// For pictures only (`stage-demo` in a self-shot, logged in or not): the view with the design's own
    /// song ("Low Tide" by Mara Quell, its words, and its cover from the image cache), paused on its
    /// third line with nothing drifting, to set beside the design.
    pub(super) fn stage_demo(&mut self, ui: &mut Ui) {
        const URI: &str = "spotify:track:truffle-stage-demo";
        const WORDS: [&str; 8] = [
            "Salt on the window, the radio low",
            "Tracing the coastline wherever we go",
            "Tide pulls the city back into the blue",
            "Every wave I hear is calling you",
            "Keep the porch light on",
            "I\u{2019}ll be home before it\u{2019}s gone",
            "Low tide, low tide",
            "Hold me where the waters hide",
        ];
        if self.playback.track.as_ref().is_none_or(|t| t.uri != URI) {
            self.playback.track = Some(crate::model::TrackInfo {
                uri: URI.to_owned(),
                name: "Low Tide".to_owned(),
                artists: vec![crate::model::ArtistRef { name: "Mara Quell".to_owned(), uri: String::new() }],
                album: "Low Tide".to_owned(),
                album_uri: String::new(),
                cover_small: None,
                cover_large: Some("https://i.scdn.co/image/truffle-stage-demo".to_owned()),
                duration_ms: 231_000,
                playable: true,
                is_local: false,
                added_at: 0,
            });
            let lines = WORDS.iter().enumerate().map(|(i, w)| LyricLine { start_ms: 14_000 + i as u32 * 3_200, words: (*w).to_owned() }).collect();
            self.lyrics.insert(URI.to_owned(), Lyrics::Lines { lines: Arc::new(lines), synced: true });
            self.playback.status = PlayStatus::Paused;
            self.playback.position_ms = 14_000 + 2 * 3_200 + 100;
            self.playback.position_at = None;
            self.playback.shuffle = true;
            self.prefs.shuffle = true;
            self.volume_ui = 64.0;
            Arc::make_mut(&mut self.liked).insert(URI.to_owned());
        }
        let ctx = ui.ctx().clone();
        if !self.stage.open {
            if self.shot_flag("stage-window") {
                // As Ctrl+I opens it, in a window of its own (with "0 0" the picture keeps its size).
                self.toggle_stage(&ctx);
            } else {
                // In the window the picture asked for.
                self.show_stage(&ctx);
            }
        }
        // Still, as a picture: everything where it ends up (`stage-off`: no lyrics).
        self.stage.opened_at = ctx.input(|i| i.time);
        self.stage.fresh = true;
        self.stage.lyrics_off = self.shot_flag("stage-off");
        // `stage-canvas`: with `demo-canvas.mp4` beside truffle.exe as its Canvas.
        let canvas = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("demo-canvas.mp4"))).filter(|_| self.shot_flag("stage-canvas"));
        self.stage.canvases.insert(URI.to_owned(), Some(canvas.map_or(CanvasFile::None, CanvasFile::Video)));
        let full = ui.max_rect();
        self.stage_ui(ui, full);
    }

    /// The view, over `full` (the whole window).
    pub(super) fn stage_ui(&mut self, ui: &mut Ui, full: Rect) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        let frame = design_frame(full);
        let k = scale(frame.width());
        let track = self.playback.track.clone();
        let cover_url = track.as_ref().and_then(|t| t.cover_large.clone().or_else(|| t.cover_small.clone()));
        let cover = cover_url.as_deref().and_then(|u| self.images.get(u, full.width().max(full.height())));
        // The song's colour: its cover's, made a readable colour on the dark (as Truffle's accent is),
        // kept while a new cover is still coming; at once as the view opens.
        let target = match cover_url.as_deref().map(|u| self.images.swatch(u)) {
            Some(Swatch::Color(c)) => super::accent::adapt_accent(c, &[BG]),
            Some(Swatch::Pending) => self.stage.song.unwrap_or(self.palette.accent),
            Some(Swatch::None) | None => self.palette.accent,
        };
        self.stage.song = Some(target);
        let song = if self.stage.fresh { target } else { motion::color(&ctx, Id::new("stage-song"), target, 0.4) };
        // The lyrics: shown, none (the cover moves to the middle), or still coming (as they were).
        let lyrics = if self.stage.lyrics_off || track.is_none() { Some(Lyrics::None) } else { self.current_lyrics() };
        let lines = match &lyrics {
            Some(Lyrics::Lines { lines, synced }) => Some((lines.clone(), *synced)),
            _ => None,
        };
        let shown = match lyrics {
            Some(Lyrics::Lines { .. }) => Some(true),
            Some(Lyrics::Loading) => None,
            Some(Lyrics::None) | None => Some(false),
        };
        // The song's Canvas, when it has one: shown instead of the cover.
        let canvas = self.stage_canvas(&ctx, now, full);

        let st = &mut self.stage;
        let glide = if motion::moves() { motion::secs(0.9) } else { 0.0 };
        if let Some(on) = shown {
            st.lyrics.set(f32::from(u8::from(on)), now, motion::secs(0.6));
            st.scrim.set(f32::from(u8::from(on)), now, motion::secs(0.8));
            st.middle.set(f32::from(u8::from(!on)), now, glide);
        }
        st.cover.set(f32::from(u8::from(canvas.is_none())), now, motion::secs(0.8));
        let fresh = std::mem::take(&mut st.fresh);
        if fresh {
            for t in [&mut st.lyrics, &mut st.scrim, &mut st.middle, &mut st.cover] {
                t.jump(t.to);
            }
        }
        let (middle, scrim, shows_cover, since) = (st.middle.value(now), st.scrim.value(now), st.cover.value(now), now - st.opened_at);
        let mut moving = [st.lyrics, st.scrim, st.middle, st.cover].iter().any(|t| t.moving(now));

        let painter = ui.painter().with_clip_rect(full);
        painter.rect_filled(full, CornerRadius::ZERO, BG);
        // The song's glow, breathing (9 s each way): blurred 30, at 34 %.
        let breathe = if motion::moves() { swing(since, 9.0, EASE_IN_OUT) } else { 0.0 };
        let s = 1.0 + 0.08 * breathe;
        let gb = glow_box(frame);
        let glow = Rect::from_center_size(gb.center() + vec2(0.02 * gb.width(), -0.02 * gb.height()) * breathe * s, gb.size() * s);
        let sigma = 30.0 * k * s;
        blur::paint(&painter, Job { source: Source::Mesh { mesh: radial_mesh(glow, song), texels: false }, area: glow.expand(3.0 * sigma), sigma, tint: [0.34; 4], saturate: 1.0, shape: blur::Shape::All });
        if let Some(tex) = cover.filter(|_| shows_cover > 0.0) {
            // The cover's own glow: blurred 48, 1.4 times as colourful, at 45 %.
            let cg = cover_glow_box(frame, middle);
            let mut quad = Mesh::with_texture(tex.id);
            quad.add_rect_with_uv(cg, cover_uv(tex.size, cg.size()), Color32::WHITE);
            let sigma = 48.0 * k;
            let tint = [0.45 * shows_cover; 4];
            blur::paint(&painter, Job { source: Source::Mesh { mesh: quad, texels: false }, area: cg.expand(3.0 * sigma), sigma, tint, saturate: 1.4, shape: blur::Shape::All });
            // The cover at 95 %, drifting closer and back (16 s each way).
            let drift = 1.02 + if motion::moves() { 0.07 * swing(since, 16.0, EASE_IN_OUT) } else { 0.0 };
            let cb = cover_box(frame, middle);
            painter.add(masked_cover(tex.id, Rect::from_center_size(cb.center(), cb.size() * drift), cover_uv(tex.size, cb.size()), 0.95 * shows_cover, 0.5));
        }
        if let Some((id, size)) = canvas.filter(|_| shows_cover < 1.0) {
            // The Canvas filling its box, faded at its edges as the design's is.
            let vb = canvas_box(frame, middle);
            painter.add(masked_cover(id, vb, cover_uv(size, vb.size()), 1.0 - shows_cover, 0.55));
        }
        if scrim > 0.0 {
            painter.add(scrim_mesh(frame, scrim));
        }
        if let Some((lines, synced)) = lines {
            moving |= self.stage_lyrics(ui, &painter, frame, k, now, &lines, synced, fresh);
        }
        self.stage_bar(ui, full, k);
        self.stage_card(ui, frame, full, k, song);
        if moving || motion::moves() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// The playing song's Canvas, asked for once: its picture, or its video's frame due now (the
    /// video goes on only while the music plays, and stays still when things may not move). Its
    /// texture and size; `None` while there is none to show.
    fn stage_canvas(&mut self, ctx: &egui::Context, now: f64, full: Rect) -> Option<(TextureId, Vec2)> {
        let uri = self.playback.track.as_ref().filter(|t| !t.is_local).map(|t| t.uri.clone());
        let Some(uri) = uri else {
            self.stage.player = None;
            return None;
        };
        let file = match self.stage.canvases.get(&uri) {
            Some(Some(file)) => file.clone(),
            Some(None) => CanvasFile::None,
            None => {
                self.stage.canvases.insert(uri.clone(), None);
                self.send(Command::Canvas(uri.clone()));
                CanvasFile::None
            }
        };
        let path = match file {
            CanvasFile::Video(path) => path,
            CanvasFile::Picture(url) => {
                self.stage.player = None;
                return self.images.get(&url, full.width().max(full.height())).map(|t| (t.id, t.size));
            }
            CanvasFile::None => {
                self.stage.player = None;
                return None;
            }
        };
        if self.stage.player.as_ref().is_none_or(|p| p.uri != uri) {
            self.stage.player = Some(CanvasPlayer { uri: uri.clone(), video: VideoLoop::open(path), texture: None, time: 0.0, last: now, waiting: None });
        }
        let playing = self.playback.status == PlayStatus::Playing && motion::moves();
        let player = self.stage.player.as_mut()?;
        if playing {
            player.time += (now - player.last).clamp(0.0, 0.25);
        }
        player.last = now;
        // The last frame due by now (the first one at once), skipping any in between.
        let (mut show, mut failed) = (None, false);
        loop {
            if player.waiting.is_none() {
                match player.video.next() {
                    Ok(frame) => player.waiting = frame,
                    Err(()) => {
                        failed = true;
                        break;
                    }
                }
            }
            let first = player.texture.is_none() && show.is_none();
            if !player.waiting.as_ref().is_some_and(|f| first || f.at.as_secs_f64() <= player.time) {
                break;
            }
            show = player.waiting.take();
            if !playing {
                break;
            }
        }
        if let Some(frame) = show {
            match &mut player.texture {
                Some(texture) => texture.set(frame.image, TextureOptions::LINEAR),
                None => player.texture = Some(ctx.load_texture("stage-canvas", frame.image, TextureOptions::LINEAR)),
            }
        }
        let shown = player.texture.as_ref().map(|t| (t.id(), t.size_vec2()));
        if failed {
            // It can't be played: the cover instead, for this song.
            self.stage.player = None;
            self.stage.canvases.insert(uri, Some(CanvasFile::None));
            return None;
        }
        if playing {
            ctx.request_repaint();
        }
        shown
    }

    /// The lyrics, right of the cover: the sung line big and white, the others smaller, fainter and
    /// blurred the further they are, sliding to their places as the song goes on (`jump`: straight
    /// there, as the view opens). A click on one plays from it. Lyrics without times sit still and
    /// scroll with the wheel. Returns whether any line is still moving.
    #[allow(clippy::too_many_arguments)]
    fn stage_lyrics(&mut self, ui: &mut Ui, painter: &egui::Painter, frame: Rect, k: f32, now: f64, lines: &Arc<Vec<LyricLine>>, synced: bool, jump: bool) -> bool {
        let column = self.stage.lyrics.value(now);
        // The blur Settings asks for, of the design's (Appearance > Motion > Lyrics blur).
        let blur_strength = f32::from(self.prefs.now_playing_blur.min(100)) / 100.0;
        let (left, right) = lyrics_column(frame);
        let width = right - left;
        let pos = self.seek_preview.map_or(self.playback.current_position_ms(), |v| v as u32);
        let cur = if synced { line_at(lines, pos) } else { None };
        let texts: Vec<&str> = lines.iter().map(|l| if l.words.trim().is_empty() { "♪" } else { l.words.as_str() }).collect();
        let size_of = |i: usize| if synced && distance(i, cur) == 0 { SUNG } else { OTHER };
        // Each line laid out once, at the sung size, and drawn smaller by scaling it: it breaks in the
        // same places sung or not, and grows and shrinks smoothly.
        let big = SUNG * k;
        let galleys: Vec<Arc<Galley>> = texts.iter().map(|t| lay(ui, t, big, width)).collect();
        let heights: Vec<f32> = galleys.iter().enumerate().map(|(i, g)| g.size().y * size_of(i) / SUNG).collect();
        // Lines without times sit still from near the top: none is being sung, so none is in the middle.
        let mut tops = if synced { line_tops(&heights, cur, frame.height(), LYRIC_GAP * k) } else { still_tops(&heights, frame.height() * UNTIMED_TOP, LYRIC_GAP * k) };
        let uri = self.playback.track.as_ref().map(|t| t.uri.clone()).unwrap_or_default();
        let st = &mut self.stage;
        let new_song = st.lines_for.as_ref().is_none_or(|(u, n)| *u != uri || *n != lines.len());
        if new_song {
            st.lines = vec![LineMotion::new(); lines.len()];
            st.lines_for = Some((uri, lines.len()));
            st.scroll = 0.0;
        }
        let fresh = new_song || jump;
        if !synced {
            if ui.rect_contains_pointer(Rect::from_x_y_ranges(left..=right, frame.y_range())) {
                st.scroll -= ui.input(|i| i.smooth_scroll_delta.y);
            }
            let end = tops.last().zip(heights.last()).map_or(0.0, |(t, h)| t + h);
            st.scroll = st.scroll.clamp(0.0, (end - frame.height() * 0.42).max(0.0));
            for t in &mut tops {
                *t -= st.scroll;
            }
        }
        let (glide, fade) = (if motion::moves() { motion::secs(0.75) } else { 0.0 }, motion::secs(0.75));
        for (i, m) in st.lines.iter_mut().enumerate() {
            let (opacity, blur) = if synced { line_look(distance(i, cur)) } else { (0.5, 0.0) };
            let to = [tops[i] / k, size_of(i), opacity, blur];
            for (t, (to, secs)) in m.tweens().into_iter().zip(to.into_iter().zip([glide, glide, fade, fade])) {
                if fresh {
                    t.jump(to);
                } else {
                    t.set(to, now, secs);
                }
            }
        }
        let motions = st.lines.clone();
        let mut moving = false;
        let ppp = ui.ctx().pixels_per_point();
        let can_seek = synced && !self.listening();
        for (i, m) in motions.iter().enumerate() {
            moving |= m.moving(now);
            let opacity = m.opacity.value(now) * column;
            if opacity <= 0.002 {
                continue;
            }
            let s = m.size.value(now) / SUNG;
            let galley = &galleys[i];
            let top = frame.top() + m.top.value(now) * k;
            let height = galley.size().y * s;
            if top > frame.bottom() + height || top + height < frame.top() - height {
                continue;
            }
            // Its first baseline where CSS puts it, at this size; on whole pixels at rest, to stay sharp.
            let mut at = pos2(left, top + baseline_shift(galley, big) * s);
            if (s - 1.0).abs() < 1e-4 {
                at = at.round_to_pixels(ppp);
            }
            let sigma = m.blur.value(now) * k * blur_strength;
            if blur::broken() {
                // Without the GPU's help egui draws it, laid out at the size it has now.
                let size = m.size.value(now) * k;
                let g = lay(ui, texts[i], size, width);
                painter.add(TextShape::new(pos2(left, top + baseline_shift(&g, size)), g, Color32::WHITE).with_opacity_factor(opacity));
            } else {
                let mut mesh = blur::galley_mesh(galley, at);
                for v in &mut mesh.vertices {
                    v.pos = at + (v.pos - at) * s;
                }
                let area = mesh.calc_bounds().expand(3.0 * sigma + 1.0);
                blur::paint(painter, Job { source: Source::Mesh { mesh, texels: true }, area, sigma, tint: [opacity; 4], saturate: 1.0, shape: blur::Shape::All });
            }
            let resp = ui.interact(Rect::from_min_size(pos2(left, top), vec2(width, height)), Id::new(("stage-line", i)), Sense::click());
            if can_seek && resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(format!("Play from {}", fmt_ms(lines[i].start_ms))).clicked() {
                let ms = lines[i].start_ms;
                self.playback.position_ms = ms;
                self.playback.position_at = Some(Instant::now());
                self.send(Command::SeekMs(ms));
            }
        }
        moving
    }

    /// The title bar, see-through: the arrow back and "Now playing" on the left, the window's buttons
    /// on the right; its empty part moves the window.
    fn stage_bar(&mut self, ui: &mut Ui, full: Rect, k: f32) {
        let (bar, leave, buttons) = bar_parts(full, k);
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(bar).id_salt("stage-bar"));
        ui.set_clip_rect(full);
        let ctx = ui.ctx().clone();
        let (maximized, fullscreen) = ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false)));
        // Fullscreen (F11) doesn't move; a double-click or the middle button leaves it instead.
        let drag = ui.interact(bar, Id::new("stage-drag"), Sense::click_and_drag());
        if drag.drag_started() && !fullscreen {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        let mut size_button = drag.double_clicked();
        let round = CornerRadius::same((6.0 * k).round() as u8);
        let back = ui.interact(leave, Id::new("stage-leave"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Back (Esc)");
        ui.painter().rect_filled(leave, round, if back.hovered() { Color32::from_white_alpha(31) } else { HOVER_BG });
        svgicon::paint(ui.painter(), Rect::from_center_size(leave.center(), vec2(15.0 * k, 15.0 * k)), "chev", TEXT, svgicon::STROKE);
        ui.painter().text(pos2(leave.right() + 8.0 * k, leave.center().y), Align2::LEFT_CENTER, "Now playing", segoe(11.5 * k), MUTED);
        let size_tip = if fullscreen { "Leave fullscreen (F11)" } else if maximized { "Restore" } else { "Maximise" };
        for (rect, (icon, tip)) in buttons.into_iter().zip([("min", "Minimise"), ("max", size_tip), ("x", "Close")]) {
            let resp = ui.interact(rect, Id::new(("stage-wc", icon)), Sense::click()).on_hover_text(tip);
            if resp.hovered() {
                ui.painter().rect_filled(rect, round, if icon == "x" { CLOSE_RED } else { HOVER_BG });
            }
            svgicon::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(15.0 * k, 15.0 * k)), icon, if resp.hovered() { Color32::WHITE } else { MUTED }, svgicon::STROKE);
            if resp.clicked() {
                match icon {
                    "min" => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
                    "max" => size_button = true,
                    _ => ctx.send_viewport_cmd(ViewportCommand::Close),
                }
            }
        }
        if size_button {
            if fullscreen {
                self.stage_fullscreen(&ctx);
            } else {
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
        }
        if back.clicked() {
            self.close_stage(&ctx);
        }
    }

    /// The player card, bottom left: frosted glass with a soft shadow, the song and its buttons, the
    /// seek line, the controls and the volume.
    fn stage_card(&mut self, ui: &mut Ui, frame: Rect, full: Rect, k: f32, song: Color32) {
        let card = card_rect(frame, k);
        let radius = 14.0 * k;
        let painter = ui.painter().with_clip_rect(full);
        // What is behind it blurred 18 and 1.2 times as colourful; its shadow (0 12 30 at 45 %) only
        // outside it.
        blur::paint(&painter, Job { source: Source::Backdrop, area: card.expand(54.0 * k), sigma: 18.0 * k, tint: [1.0; 4], saturate: 1.2, shape: blur::Shape::Inside(card, radius) });
        let shadow = card.translate(vec2(0.0, 12.0 * k));
        let sigma = 15.0 * k;
        blur::paint(&painter, Job { source: Source::Mesh { mesh: blur::rounded_rect_mesh(shadow, radius, Color32::from_black_alpha(115)), texels: false }, area: shadow.expand(3.0 * sigma), sigma, tint: [1.0; 4], saturate: 1.0, shape: blur::Shape::Outside(card, radius) });
        painter.rect(card, CornerRadius::same(radius.round() as u8), CARD_BG, Stroke::new(1.0, CARD_EDGE), StrokeKind::Inside);

        let parts = card_parts(card, k);
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(card).id_salt("stage-card"));
        ui.set_clip_rect(full);
        let track = self.playback.track.clone();
        let (title, artists) = track.as_ref().map_or_else(|| ("Nothing playing".to_owned(), String::new()), |t| (t.name.clone(), t.artist_line()));
        let g = one_line(&ui, &title, segoe_bold(TITLE * k), Color32::WHITE, parts.title.width(), false);
        ui.painter().galley(pos2(parts.title.left(), parts.title.center().y - g.size().y / 2.0), g, Color32::WHITE);
        let g = one_line(&ui, &artists, segoe(ARTIST * k), MUTED, parts.artist.width(), false);
        ui.painter().galley(pos2(parts.artist.left(), parts.artist.center().y - g.size().y / 2.0), g, MUTED);

        // Like, add to playlist, lyrics on or off, more.
        let song_track = track.as_ref().filter(|t| !t.is_local);
        let liked = song_track.is_some_and(|t| self.liked.contains(&t.uri));
        let [heart, add, words, more] = parts.acts;
        let tip = if liked { "Remove from Liked Songs" } else { "Save to Liked Songs" };
        let (icon, color) = if liked { ("heartf", song) } else { ("heart", MUTED) };
        if card_button(&ui, heart, Id::new("stage-heart"), icon, 15.0 * k, color, false, song_track.is_some(), tip, k).clicked()
            && let Some(t) = song_track
        {
            let uri = t.uri.clone();
            self.like_song(&uri, !liked);
        }
        let menu_at = |r: Rect| pos2(r.left(), r.top() - 4.0);
        if card_button(&ui, add, Id::new("stage-add"), "plus", 15.0 * k, MUTED, false, song_track.is_some(), "Add to playlist", k).clicked()
            && let Some(t) = song_track
        {
            let from = self.playback.source.clone();
            self.open_add_to(t, menu_at(add), from);
        }
        let tip = if self.stage.lyrics_off { "Show lyrics" } else { "Hide lyrics" };
        if card_button(&ui, words, Id::new("stage-lyrics"), "lyrics", 15.0 * k, MUTED, false, true, tip, k).clicked() {
            self.stage.lyrics_off = !self.stage.lyrics_off;
        }
        if card_button(&ui, more, Id::new("stage-more"), "more", 15.0 * k, MUTED, false, track.is_some(), "More", k).clicked()
            && let Some(t) = &track
        {
            let from = self.playback.source.clone();
            self.open_song_menu(t, menu_at(more), from);
        }

        // The seek row: the time so far, the line, the song's length.
        let duration = track.as_ref().map_or(0, |t| t.duration_ms);
        let dur = duration.max(1) as f32;
        let at = self.seek_preview.unwrap_or(self.playback.current_position_ms() as f32).clamp(0.0, dur);
        let (done, length) = if track.is_some() { (fmt_ms(at as u32), fmt_ms(duration)) } else { ("-:--".to_owned(), "-:--".to_owned()) };
        let time_size = TIME * k;
        let done = ui.fonts_mut(|f| f.layout_no_wrap(done, segoe(time_size), MUTED));
        let length = ui.fonts_mut(|f| f.layout_no_wrap(length, segoe(time_size), MUTED));
        let line = Rect::from_min_max(pos2(parts.content.left() + done.size().x + 10.0 * k, parts.seek_y - 1.6 * k), pos2(parts.content.right() - length.size().x - 10.0 * k, parts.seek_y + 1.6 * k));
        ui.painter().galley(pos2(parts.content.left(), parts.seek_y - done.size().y / 2.0), done, MUTED);
        ui.painter().galley(pos2(parts.content.right() - length.size().x, parts.seek_y - length.size().y / 2.0), length, MUTED);
        let locked = self.listening();
        let (resp, held) = card_line(&ui, line, Id::new("stage-seek"), at / dur, track.is_some() && duration > 0 && !locked, song, k);
        if let Some(v) = held {
            self.seek_preview = Some(v * dur);
        } else if let Some(target) = self.seek_preview.take() {
            let target = target.round() as u32;
            self.playback.position_ms = target;
            self.playback.position_at = Some(Instant::now());
            self.send(Command::SeekMs(target));
        }
        if resp.hovered()
            && track.is_some()
            && let Some(p) = resp.hover_pos()
        {
            let t = ((p.x - line.left()) / line.width()).clamp(0.0, 1.0);
            resp.on_hover_text_at_pointer(fmt_ms((t * dur) as u32));
        }

        // Shuffle, previous, play, next, repeat (dimmed while listening along: the host decides).
        let has_track = track.is_some();
        let playing = matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading);
        let shuffle = self.current_shuffle();
        let repeat = if self.have_playback { self.playback.repeat } else { self.prefs.repeat };
        let [shuffle_r, prev_r, play_r, next_r, repeat_r] = parts.ctl;
        let tip = if locked { "Listening along" } else if shuffle { "Shuffle is on" } else { "Shuffle is off" };
        if card_button(&ui, shuffle_r, Id::new("stage-shuffle"), "shuffle", 16.0 * k, if shuffle { song } else { TEXT }, shuffle, !locked, tip, k).clicked() {
            self.toggle_shuffle();
        }
        if card_button(&ui, prev_r, Id::new("stage-prev"), "prev", 16.0 * k, TEXT, false, has_track && !locked, "Previous (Ctrl+←)", k).clicked() {
            self.send(Command::Previous);
        }
        let enabled = has_track && !locked;
        let resp = ui.interact(play_r, Id::new("stage-play"), if enabled { Sense::click() } else { Sense::hover() });
        let fill = if resp.hovered() && enabled { fin::brighten(song, 1.1) } else { song };
        ui.painter().circle_filled(play_r.center(), play_r.width() / 2.0, if enabled { fill } else { fill.gamma_multiply(0.5) });
        svgicon::paint(ui.painter(), Rect::from_center_size(play_r.center(), vec2(16.0 * k, 16.0 * k)), if playing { "pause" } else { "play" }, Color32::WHITE, svgicon::STROKE);
        let tip = if locked { "Listening along" } else if playing { "Pause (Space)" } else { "Play (Space)" };
        if enabled && resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip).clicked() {
            self.send(Command::PlayPause);
        }
        if card_button(&ui, next_r, Id::new("stage-next"), "next", 16.0 * k, TEXT, false, has_track && !locked, "Next (Ctrl+→)", k).clicked() {
            self.send(Command::Next);
        }
        let (on, tip) = match repeat {
            RepeatMode::Off => (false, "Repeat is off"),
            RepeatMode::All => (true, "Repeating the whole list"),
            RepeatMode::One => (true, "Repeating this song"),
        };
        if card_button(&ui, repeat_r, Id::new("stage-repeat"), "repeat", 16.0 * k, if on { song } else { TEXT }, on, !locked, if locked { "Listening along" } else { tip }, k).clicked() {
            self.cycle_repeat();
        }
        if repeat == RepeatMode::One && !locked {
            ui.painter().text(pos2(repeat_r.right() - 3.0 * k, repeat_r.top() + 3.0 * k), Align2::RIGHT_TOP, "1", segoe_bold(8.0 * k), song);
        }

        // The volume: the speaker (a click mutes) and its line.
        let vol = self.volume_ui.round() as u8;
        let (icon, tip) = if vol == 0 { ("volx", "Unmute") } else { ("vol", "Mute") };
        let speaker = ui.interact(parts.vol_icon.expand(4.0 * k), Id::new("stage-mute"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
        svgicon::paint(ui.painter(), parts.vol_icon, icon, if speaker.hovered() { Color32::WHITE } else { MUTED }, svgicon::STROKE);
        if speaker.clicked() {
            if vol > 0 {
                self.volume_before_mute = Some(vol);
                self.volume_ui = 0.0;
            } else {
                self.volume_ui = f32::from(self.volume_before_mute.take().unwrap_or(50));
            }
            self.volume_dirty_at = Some(super::player::due_now());
        }
        let (resp, held) = card_line(&ui, parts.vol_bar, Id::new("stage-volume"), self.volume_ui / 100.0, true, TEXT, k);
        // Right-click the speaker or the line: a box to type the volume in, as in the player bar.
        let area = parts.vol_icon.expand(4.0 * k).union(resp.rect);
        let right_click = ui.input(|i| i.pointer.secondary_clicked().then(|| i.pointer.interact_pos()).flatten());
        if right_click.is_some_and(|pt| area.contains(pt)) {
            self.volume_box = Some(String::new());
        }
        self.volume_box_ui(&ui, area);
        if let Some(v) = held {
            self.volume_ui = (v * 100.0).round();
            self.volume_before_mute = None;
            self.volume_dirty_at.get_or_insert_with(Instant::now);
        }
        self.volume_dragging = held.is_some();
        if resp.hovered() {
            let scroll: f32 = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::MouseWheel { delta, .. } => Some(delta.y),
                        _ => None,
                    })
                    .sum()
            });
            if scroll != 0.0 {
                self.volume_ui = (self.volume_ui + scroll.signum() * 5.0).clamp(0.0, 100.0);
                self.volume_dirty_at = Some(super::player::due_now());
            }
        }
        // The volume as a number beside the line, at once while it is hovered or held (a tooltip waits,
        // and hides during a drag).
        if resp.hovered() || held.is_some() {
            let at = pos2(parts.vol_bar.right() + 10.0 * k, parts.vol_bar.center().y);
            ui.painter().text(at, Align2::LEFT_CENTER, format!("{}%", self.volume_ui.round() as u8), segoe(TIME * k), TEXT);
        }
        self.send_volume(ui.ctx(), resp.drag_stopped() || held.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Pos2;

    #[test]
    fn the_curves_run_from_nothing_to_all_and_ease_as_css_does() {
        for curve in [EASE, EASE_IN_OUT, GLIDE] {
            assert_eq!(bezier(curve, 0.0), 0.0);
            assert_eq!(bezier(curve, 1.0), 1.0);
            let mut last = 0.0;
            for i in 1..100 {
                let v = bezier(curve, i as f32 / 100.0);
                assert!(v >= last - 1e-5, "{curve:?} goes back at {i}");
                last = v;
            }
        }
        // ease-in-out is symmetric; the design's glide is well past half-way at half the time.
        assert!((bezier(EASE_IN_OUT, 0.5) - 0.5).abs() < 1e-3);
        assert!(bezier(GLIDE, 0.5) > 0.85);
        // CSS's `ease` at a quarter of the time: about 0.41 (its known value).
        assert!((bezier(EASE, 0.25) - 0.4094).abs() < 0.01, "{}", bezier(EASE, 0.25));
    }

    #[test]
    fn an_alternating_animation_goes_out_and_comes_back() {
        assert_eq!(swing(0.0, 9.0, EASE_IN_OUT), 0.0);
        assert!((swing(9.0, 9.0, EASE_IN_OUT) - 1.0).abs() < 1e-4);
        assert!(swing(18.0, 9.0, EASE_IN_OUT).abs() < 1e-4);
        assert!((swing(4.5, 9.0, EASE_IN_OUT) - swing(13.5, 9.0, EASE_IN_OUT)).abs() < 1e-4);
    }

    #[test]
    fn a_tween_heads_for_a_new_target_from_where_it_got_to() {
        let mut t = Tween::at(0.0, GLIDE);
        t.set(10.0, 0.0, 1.0);
        assert_eq!(t.value(0.0), 0.0);
        assert!(t.moving(0.5));
        let half = t.value(0.5);
        t.set(0.0, 0.5, 1.0);
        assert!((t.value(0.5) - half).abs() < 1e-4, "starts where it was");
        assert_eq!(t.value(2.0), 0.0);
        assert!(!t.moving(2.0));
        t.set(5.0, 3.0, 0.0);
        assert_eq!(t.value(3.0), 5.0, "no time: at once");
    }

    #[test]
    fn the_sung_line_sits_42_percent_down_with_the_rest_around_it() {
        let near = |a: f32, b: f32| assert!((a - b).abs() < 1e-3, "{a} is not {b}");
        let heights = [30.0, 40.0, 30.0, 30.0];
        let tops = line_tops(&heights, Some(1), 600.0, 13.0);
        near(tops[1], 252.0 - 20.0);
        near(tops[2], tops[1] + 40.0 + 13.0);
        near(tops[3], tops[2] + 30.0 + 13.0);
        near(tops[0], tops[1] - 13.0 - 30.0);
        // Before the first line: it waits just under the middle.
        let tops = line_tops(&heights, None, 600.0, 13.0);
        near(tops[0], 252.0 + 13.0);
        assert!(line_tops(&[], Some(0), 600.0, 13.0).is_empty());
        // Without times: from the top down.
        let tops = still_tops(&heights, 72.0, 13.0);
        near(tops[0], 72.0);
        near(tops[1], 72.0 + 30.0 + 13.0);
        near(tops[3], 72.0 + 30.0 + 40.0 + 30.0 + 3.0 * 13.0);
    }

    #[test]
    fn lines_fade_and_blur_the_further_they_are_from_the_sung_one() {
        assert_eq!(line_look(0), (1.0, 0.0));
        let next: Vec<_> = (1..=3).map(line_look).collect();
        assert!(next.windows(2).all(|w| w[1].0 < w[0].0 && w[1].1 > w[0].1));
        assert_eq!(line_look(4).0, 0.0);
        assert_eq!(line_look(-2).0, 0.0);
        assert!(line_look(-1).0 > 0.0);
        assert_eq!(distance(0, None), 1, "before the first line, it is next");
        assert_eq!(distance(3, Some(3)), 0);
    }

    #[test]
    fn at_the_designs_width_the_card_is_the_designs() {
        let full = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 625.0));
        let k = scale(full.width());
        assert_eq!(k, 1.0);
        let card = card_rect(full, k);
        assert_eq!(card.left(), 30.0);
        assert_eq!(card.width(), 330.0);
        assert_eq!(card.bottom(), 595.0);
        assert!((card.height() - 167.65).abs() < 0.01, "{}", card.height());
        let p = card_parts(card, k);
        assert_eq!(p.content.left(), 48.0);
        assert!((p.title.height() - 21.75).abs() < 1e-4);
        // The small buttons end 6 into the padding; the controls are centred.
        assert!((p.acts[3].right() - (p.content.right() + 6.0)).abs() < 1e-3);
        assert!((p.ctl[2].center().x - p.content.center().x).abs() < 1e-3);
        assert_eq!(p.ctl[2].width(), 36.0);
        assert!((p.ctl[1].right() + 3.0 + 8.0 - p.ctl[2].left()).abs() < 1e-3);
        assert_eq!(p.vol_bar.width(), 120.0);
        // A small window keeps it usable.
        assert_eq!(scale(400.0), MIN_K);
    }

    /// The rows `lay` gives `text` at the sung size in `width`, 1.25 pixels a point (as at 125 % scaling).
    fn rows_of(text: &str, width: f32) -> Vec<String> {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        ctx.set_pixels_per_point(1.25);
        let mut rows = Vec::new();
        for pass in 0..2 {
            ctx.run_ui(Default::default(), |ui| {
                if pass == 1 {
                    let galley = lay(ui, text, SUNG, width);
                    rows = galley.rows.iter().map(|r| r.glyphs.iter().map(|g| g.chr).collect::<String>().trim_end().to_owned()).collect();
                }
            })
            .textures_delta
            .clear();
        }
        rows
    }

    #[test]
    fn a_long_line_breaks_where_the_design_breaks_it() {
        // Edge measures "Every wave I hear is calling" 439.8 wide: it fits the design's 440 column,
        // the space after it hanging over the edge.
        assert_eq!(rows_of("Every wave I hear is calling you", 440.0), ["Every wave I hear is calling", "you"]);
        // A word wider than a row gets one of its own; a hyphen between letters may end a row.
        assert_eq!(rows_of("Supercalifragilisticexpialidocious yes", 300.0), ["Supercalifragilisticexpialidocious", "yes"]);
        assert_eq!(rows_of("one-two", 10.0), ["one-", "two"]);
        assert_eq!(rows_of("Low tide", 440.0), ["Low tide"]);
        // Japanese breaks between letters: egui wraps it.
        assert!(rows_of("東京の夜は静かに眠る街の光が揺れる", 120.0).len() > 1);
    }

    #[test]
    fn the_view_keeps_the_designs_shape_in_any_window() {
        let wide = design_frame(Rect::from_min_size(Pos2::ZERO, vec2(2752.0, 1100.0)));
        assert_eq!(wide.height(), 1100.0);
        assert!((wide.width() - 1760.0).abs() < 1e-3 && (wide.center().x - 1376.0).abs() < 1e-3);
        let tall = design_frame(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 900.0)));
        assert_eq!(tall.width(), 800.0);
        assert!((tall.height() - 500.0).abs() < 1e-3 && (tall.center().y - 450.0).abs() < 1e-3);
        let exact = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 625.0));
        assert_eq!(design_frame(exact), exact);
    }

    #[test]
    fn the_cover_fills_its_box_as_css_cover_does_and_moves_to_the_middle_without_lyrics() {
        let uv = cover_uv(vec2(640.0, 640.0), vec2(600.0, 625.0));
        assert!((uv.width() - 0.96).abs() < 1e-4 && uv.height() == 1.0);
        let uv = cover_uv(vec2(640.0, 640.0), vec2(760.0, 725.0));
        assert!(uv.width() == 1.0 && (uv.height() - 725.0 / 760.0).abs() < 1e-4);
        let full = Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 625.0));
        assert_eq!(cover_box(full, 0.0).left(), -10.0);
        assert!((cover_box(full, 1.0).center().x - 500.0).abs() < 1e-3, "in the middle");
    }

    #[test]
    fn the_covers_mask_is_solid_in_the_middle_and_gone_at_the_edges() {
        let mesh = masked_cover(TextureId::default(), Rect::from_min_size(Pos2::ZERO, vec2(100.0, 100.0)), Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), 1.0, 0.5);
        let at = |p: Pos2| mesh.vertices.iter().find(|v| (v.pos - p).length() < 0.01).map(|v| v.color.a());
        assert_eq!(at(pos2(50.0, 50.0)), Some(255));
        assert_eq!(at(pos2(0.0, 50.0)), Some(0));
        assert_eq!(at(pos2(0.0, 0.0)), Some(0));
    }
}
