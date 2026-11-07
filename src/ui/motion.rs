//! How things move, as in the demo the owner picked (docs/mockups/motion.html): nothing flashes or
//! changes at once, every change eases over about 90-200 ms. The times here are Normal's; the Motion
//! setting scales them (Snappy 0.75, Relaxed 1.35). With Windows' "Animation effects" off, things
//! only fade, in the same short times: nothing moves, grows or slides.
//!
//! Arriving eases out (quick, then settling), leaving eases in (slow, then quick), small reactions
//! (hover, press, switches, tabs) ease out both ways. Animations never hold up a click or a key, and
//! frames are asked for only while something moves.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use egui::emath::TSTransform;
use egui::epaint::ClippedShape;
use egui::{
    Align2, Color32, Context, CornerRadius, Frame, Galley, Id, LayerId, Mesh, Modal, ModalResponse, Order, Painter, Pos2, Rect, Ui, Vec2, ViewportId,
    pos2, vec2,
};

use super::theme::Palette;
use crate::config::MotionSpeed;

/// A hover's colour.
pub const HOVER: f32 = 0.12;
/// A pressed button shrinking to [`PRESSED`].
pub const PRESS: f32 = 0.09;
/// A switch's knob.
pub const SWITCH: f32 = 0.16;
/// A tab's underline sliding to the tab picked.
pub const TAB: f32 = 0.18;
/// A message rising [`MESSAGE_RISE`] into place as it fades in.
pub const MESSAGE_IN: f32 = 0.20;
/// Anything leaving (a message, a dialog): quicker than arriving.
pub const LEAVE: f32 = 0.15;
/// The dim behind a dialog fading in.
pub const BACKDROP: f32 = 0.16;
/// A dialog's box growing from [`DIALOG_FROM`] as it fades in.
pub const DIALOG: f32 = 0.18;

/// How small a held button gets.
pub const PRESSED: f32 = 0.97;
/// How small a dialog's box starts.
pub const DIALOG_FROM: f32 = 0.96;
/// How far below its place a message starts (points).
pub const MESSAGE_RISE: f32 = 16.0;

/// The heart's pop when liking ([`pop`]).
pub const HEART: f32 = 0.20;
/// The heart's dip when unliking ([`dip`]).
pub const UNHEART: f32 = 0.15;
/// A row that appears or goes: its height and fade.
pub const ROW: f32 = 0.20;
/// A changed number rolling: the old one out, the new one in.
pub const COUNT: f32 = 0.20;
/// A page opening: fading in as it lifts [`PAGE_RISE`] into place.
pub const PAGE: f32 = 0.18;
/// How far below its place a page starts (points).
pub const PAGE_RISE: f32 = 8.0;
/// Content fading in when it arrives.
pub const CONTENT: f32 = 0.16;
/// A turntable's tonearm swinging onto the record or off it.
pub const ARM: f32 = 0.20;
/// How long a load shows nothing before its placeholder rows (a wait, not scaled by the speed).
pub const LOADING_QUIET: f32 = 0.3;
/// One sweep of the placeholder rows' shimmer.
pub const SHIMMER: f32 = 1.1;

/// The Motion setting's factor on every time.
pub fn factor(speed: MotionSpeed) -> f32 {
    match speed {
        MotionSpeed::Snappy => 0.75,
        MotionSpeed::Normal => 1.0,
        MotionSpeed::Relaxed => 1.35,
    }
}

static SPEED: AtomicU32 = AtomicU32::new(1f32.to_bits());
static MOVES: AtomicBool = AtomicBool::new(true);

/// Applies the Motion setting, to Truffle's times and egui's own (menus and tips fading in).
pub fn set_speed(ctx: &Context, speed: MotionSpeed) {
    let k = factor(speed);
    SPEED.store(k.to_bits(), Ordering::Relaxed);
    ctx.all_styles_mut(|s| s.animation_time = BACKDROP * k);
}

/// `base` seconds (Normal's) at the chosen speed.
pub fn secs(base: f32) -> f32 {
    base * f32::from_bits(SPEED.load(Ordering::Relaxed))
}

/// Whether things may move, grow and slide; else they only fade (Windows' "Animation effects").
pub fn moves() -> bool {
    MOVES.load(Ordering::Relaxed)
}

/// Reads Windows' "Animation effects" (Accessibility > Visual effects).
pub fn read_windows() {
    MOVES.store(windows_animations(), Ordering::Relaxed);
}

/// Reads Windows' "Animation effects" again when the window gets the focus back (it may have been
/// changed meanwhile). Never asks for a frame of its own.
pub fn follow_windows(ctx: &Context) {
    static FOCUSED: AtomicBool = AtomicBool::new(true);
    let focused = ctx.input(|i| i.focused);
    if focused && !FOCUSED.swap(focused, Ordering::Relaxed) {
        read_windows();
    } else if !focused {
        FOCUSED.store(false, Ordering::Relaxed);
    }
}

fn windows_animations() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW};
        let mut on: i32 = 1;
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to the pointer.
        let ok = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut on).cast(), 0) };
        ok == 0 || on != 0
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// The demo's ease-out, `cubic-bezier(.2, .8, .2, 1)`: quick at first, then settling.
pub fn ease_out(t: f32) -> f32 {
    bezier(0.2, 0.8, 0.2, 1.0, t)
}

/// The demo's ease-in, `cubic-bezier(.4, 0, 1, 1)`: slow at first, then quick.
pub fn ease_in(t: f32) -> f32 {
    bezier(0.4, 0.0, 1.0, 1.0, t)
}

/// A CSS `cubic-bezier(x1, y1, x2, y2)` at `t` (0-1).
fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t == 0.0 || t == 1.0 {
        return t;
    }
    let curve = |a: f32, b: f32, s: f32| 3.0 * (1.0 - s) * (1.0 - s) * s * a + 3.0 * (1.0 - s) * s * s * b + s * s * s;
    // x grows with s (x1 and x2 are within 0-1): halve towards the s where x is t.
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if curve(x1, x2, mid) < t { lo = mid } else { hi = mid }
    }
    curve(y1, y2, (lo + hi) / 2.0)
}

/// How far from its place something arriving `distance` away is at eased progress `t`: none with
/// fades only.
pub fn offset(distance: f32, t: f32, moves: bool) -> f32 {
    if moves { distance * (1.0 - t) } else { 0.0 }
}

/// The scale of something growing from `from` at eased progress `t`: always 1 with fades only.
pub fn grow(from: f32, t: f32, moves: bool) -> f32 {
    if moves { from + (1.0 - from) * t } else { 1.0 }
}

/// The heart's pop at `t` (0-1, linear): 0.7, up past full to 1.22 just after half way, back to 1.
/// 1 throughout with fades only.
pub fn pop(t: f32, moves: bool) -> f32 {
    if !moves {
        return 1.0;
    }
    let t = t.clamp(0.0, 1.0);
    if t < 0.55 { 0.7 + 0.52 * ease_out(t / 0.55) } else { 1.22 - 0.22 * ease_out((t - 0.55) / 0.45) }
}

/// The heart's dip when unliking at `t` (0-1, linear): down to 0.82 half way and back. 1
/// throughout with fades only.
pub fn dip(t: f32, moves: bool) -> f32 {
    if !moves {
        return 1.0;
    }
    let t = t.clamp(0.0, 1.0);
    let depth = if t < 0.5 { ease_in(t / 0.5) } else { 1.0 - ease_out((t - 0.5) / 0.5) };
    1.0 - 0.18 * depth
}

/// How much of its room a row that appears or goes takes at eased progress `t`: it opens and
/// shuts with it, or with fades only takes all of it while it shows at all.
pub fn room(t: f32, moves: bool) -> f32 {
    if moves { t } else if t > 0.0 { 1.0 } else { 0.0 }
}

/// The numbers to draw while `from` rolls to `to` at eased progress `t`, old first: each with how
/// far it is from its line (in line heights, below is +) and its opacity. A bigger number comes up
/// from below as the old one goes up and out; a smaller one comes down from above. They only fade
/// with fades only.
pub fn rolling<T: PartialOrd>(from: T, to: T, t: f32, moves: bool) -> [(T, f32, f32); 2] {
    let way = if to < from { -1.0 } else { 1.0 };
    [(from, offset(-way, 1.0 - t, moves), 1.0 - t), (to, offset(way, t, moves), t)]
}

/// How far placeholder rows have faded in after a load has run `waited` seconds: not at all for
/// the first [`LOADING_QUIET`] (a quick load shows nothing), then in over `length`.
fn placeholder(waited: f32, length: f32) -> f32 {
    if waited < LOADING_QUIET { 0.0 } else { ease_out((waited - LOADING_QUIET) / length.max(0.001)) }
}

/// `a` mixed towards `b` by `t` (0-1), alpha too.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    let (a, b) = (a.to_array(), b.to_array());
    Color32::from_rgba_premultiplied(lerp(a[0], b[0]), lerp(a[1], b[1]), lerp(a[2], b[2]), lerp(a[3], b[3]))
}

/// 0-1 going towards `on` over `base` seconds from wherever it is, easing out both ways (a hover,
/// a switch): turning back half way sets off from where it is, never jumps. The first time (and
/// after a pass without it) it is at `on` already.
pub fn toward(ctx: &Context, id: Id, on: bool, base: f32) -> f32 {
    glide(ctx, id.with("motion-toward"), if on { 1.0 } else { 0.0 }, base, |a: f32, b: f32, t| a + (b - a) * t)
}

/// A colour going to `target` over `base` seconds whenever that changes, from wherever it is (a
/// control's hover, its look). The first time (and after a pass without it) it is `target` already.
pub fn color(ctx: &Context, id: Id, target: Color32, base: f32) -> Color32 {
    glide(ctx, id.with("motion-color"), target, base, mix)
}

/// A value going to `target` (kept under `key`, asked for in each pass while it shows) over `base`
/// seconds whenever that changes, eased out from wherever it is then: `between` gives the value part
/// way. The first time it is `target`; so it is after a pass without it, as what changed out of
/// sight (a row scrolled away, another list's row in its place) doesn't move when it shows again.
fn glide<T: Copy + PartialEq + Send + Sync + 'static>(ctx: &Context, key: Id, target: T, base: f32, between: fn(T, T, f32) -> T) -> T {
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    let length = secs(base).max(0.001) as f64;
    // Per window: each counts its own passes.
    let key = key.with(ctx.viewport_id());
    let at = |g: &Glide<T>| between(g.from, g.to, ease_out(((now - g.start) / length).clamp(0.0, 1.0) as f32));
    let g = ctx.data_mut(|d| {
        let g = match d.get_temp::<Glide<T>>(key).filter(|g| g.pass == pass || g.pass + 1 == pass) {
            Some(g) if g.to == target => Glide { pass, ..g },
            Some(g) => Glide { from: at(&g), to: target, start: now, pass },
            None => Glide { from: target, to: target, start: f64::NEG_INFINITY, pass },
        };
        d.insert_temp(key, g);
        g
    });
    if now - g.start < length {
        ctx.request_repaint();
    }
    at(&g)
}

/// Where something is on its way to `to` (a place, a size) over `base` seconds whenever that
/// changes, from wherever it was (a card moving to its new place). There at once the first time
/// (and after a pass without it), and always with fades only.
pub fn slide(ctx: &Context, id: Id, to: Vec2, base: f32) -> Vec2 {
    if !moves() {
        return to;
    }
    glide(ctx, id.with("motion-slide"), to, base, |a: Vec2, b: Vec2, t| a + (b - a) * t)
}

/// What `value` (of `id`, asked for in each pass while it shows) was before its last change, and
/// how far the new one has faded in (0-1, eased out) over `base` seconds: for something that with
/// fades only doesn't move but fades out at its old place as it fades in at the new one. `(value,
/// 1.0)` once it is there.
pub fn crossfade<T: Copy + PartialEq + Send + Sync + 'static>(ctx: &Context, id: Id, value: T, base: f32) -> (T, f32) {
    let (was, ago) = changed(ctx, id.with("crossfade"), value);
    let t = ago / secs(base).max(0.001);
    if was == value || t >= 1.0 {
        return (value, 1.0);
    }
    ctx.request_repaint();
    (was, ease_out(t))
}

/// A turntable's tonearm (see `fin::turntable`): its angle off the record, or on it while `on`
/// (further in as `done`, how far the song has got, grows). It swings over [`ARM`]; with fades only
/// it fades out at one place as it fades in at the other. Each angle with its opacity.
pub fn tonearm(ctx: &Context, id: Id, on: bool, done: f32) -> [(f32, f32); 2] {
    let swing = toward(ctx, id, on, ARM);
    let (off, onto) = (1.0, 13.0 + done * 21.0);
    if moves() { [(off + (onto - off) * swing, 1.0), (onto, 0.0)] } else { [(off, 1.0 - swing), (onto, swing)] }
}

/// A value on its way, for [`glide`].
#[derive(Clone, Copy)]
struct Glide<T> {
    from: T,
    to: T,
    start: f64,
    pass: u64,
}

/// A hover's 0-1 (its colour fading in and out).
pub fn hover(ctx: &Context, id: Id, hovered: bool) -> f32 {
    toward(ctx, id.with("hover"), hovered, HOVER)
}

/// The scale of a control while the mouse is held on it: down to [`PRESSED`] and back. 1 with
/// fades only.
pub fn press(ctx: &Context, id: Id, held: bool) -> f32 {
    1.0 - (1.0 - PRESSED) * toward(ctx, id.with("press"), held && moves(), PRESS)
}

/// Draws what `paint` draws on `ui`'s layer scaled by `scale` around `about` (a pressed button).
/// Only the picture: where it takes clicks stays as it is.
pub fn scaled<R>(ui: &Ui, about: Pos2, scale: f32, paint: impl FnOnce() -> R) -> R {
    transformed(ui.ctx(), ui.layer_id(), paint, |_| (scale != 1.0).then(|| grown(about, scale)))
}

/// Draws what `paint` draws on `ui`'s layer moved by `by` (a message rising into place). Only the
/// picture: where it takes clicks stays as it is.
pub fn moved<R>(ui: &mut Ui, by: Vec2, paint: impl FnOnce(&mut Ui) -> R) -> R {
    let (ctx, layer) = (ui.ctx().clone(), ui.layer_id());
    transformed(&ctx, layer, || paint(ui), |_| (by != Vec2::ZERO).then(|| TSTransform::from_translation(by)))
}

/// [`moved`], but kept inside `ui`'s clip rect as it was (a page rising within its part of the
/// window, not over the bar below it).
pub fn moved_within<R>(ui: &mut Ui, by: Vec2, paint: impl FnOnce(&mut Ui) -> R) -> R {
    let (ctx, layer, keep) = (ui.ctx().clone(), ui.layer_id(), ui.clip_rect());
    let start = ctx.graphics_mut(|g| g.entry(layer).next_idx());
    let r = paint(ui);
    if by != Vec2::ZERO {
        ctx.graphics_mut(|g| {
            let list = g.entry(layer);
            for k in start.0..list.next_idx().0 {
                list.mutate_shape(egui::layers::ShapeIdx(k), |c| {
                    c.shape.translate(by);
                    c.clip_rect = c.clip_rect.translate(by).intersect(keep);
                });
            }
        });
    }
    r
}

/// Scaling by `scale` around `about`.
fn grown(about: Pos2, scale: f32) -> TSTransform {
    TSTransform::new(about.to_vec2() * (1.0 - scale), scale)
}

/// What `paint` draws on `layer`, then changed by what `how` makes of its result (if anything).
fn transformed<R>(ctx: &Context, layer: LayerId, paint: impl FnOnce() -> R, how: impl FnOnce(&R) -> Option<TSTransform>) -> R {
    let start = ctx.graphics_mut(|g| g.entry(layer).next_idx());
    let r = paint();
    if let Some(t) = how(&r) {
        ctx.graphics_mut(|g| {
            let list = g.entry(layer);
            let end = list.next_idx();
            list.transform_range(start, end, t);
        });
    }
    r
}

/// The dim behind a dialog `id` as it fades in over [`BACKDROP`] (`color` once it is there).
pub fn backdrop(ctx: &Context, id: Id, color: Color32) -> Color32 {
    color.gamma_multiply(arrive(ctx, id.with("backdrop"), BACKDROP))
}

/// A dialog's box: what `paint` draws on `ui` fades in over [`DIALOG`], growing from
/// [`DIALOG_FROM`] around the middle of the rect it gives back. Only the picture.
pub fn dialog_box<R>(ui: &mut Ui, id: Id, paint: impl FnOnce(&mut Ui) -> (Rect, R)) -> R {
    let t = arrive(ui.ctx(), id.with("box"), DIALOG);
    let scale = grow(DIALOG_FROM, t, moves());
    let (ctx, layer, opacity) = (ui.ctx().clone(), ui.layer_id(), ui.opacity());
    ui.multiply_opacity(t);
    let (_, r) = transformed(&ctx, layer, || paint(ui), |(rect, _)| (scale != 1.0).then(|| grown(rect.center(), scale)));
    ui.set_opacity(opacity);
    r
}

/// Shows an [`egui::Modal`] dialog `id` in `frame` over a `dim` backdrop, both arriving as the
/// demo shows (see [`backdrop`], [`dialog_box`]) and fading out once it is no longer shown. Clicks
/// and keys work on it from the first frame.
pub fn dialog<T>(ctx: &Context, id: Id, frame: Frame, dim: Color32, content: impl FnOnce(&mut Ui) -> T) -> ModalResponse<T> {
    // egui's own fade is off; the frame is drawn inside, so the whole box fades and grows.
    let modal = Modal::new(id).area(Modal::default_area(id).fade_in(false)).frame(Frame::NONE).backdrop_color(backdrop(ctx, id, dim));
    let shown = modal.show(ctx, |ui| {
        dialog_box(ui, id, |ui| {
            let r = frame.show(ui, content);
            (r.response.rect, r.inner)
        })
    });
    fade_out_later(ctx, shown.response.layer_id, id, Vec2::ZERO);
    shown
}

/// When `id` started showing: its clock, kept while it is asked for in every pass.
#[derive(Clone, Copy)]
struct Shown {
    since: f64,
    pass: u64,
}

/// Seconds since `id` started showing. Asked for in each pass while it shows; a pass without it
/// starts the clock again.
pub fn shown_for(ctx: &Context, id: Id) -> f32 {
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    let key = id.with("motion-shown");
    let since = ctx.data_mut(|d| {
        let kept = d.get_temp::<Shown>(key).filter(|s| s.pass == pass || s.pass + 1 == pass);
        let since = kept.map_or(now, |s| s.since);
        d.insert_temp(key, Shown { since, pass });
        since
    });
    (now - since).max(0.0) as f32
}

/// 0-1, eased out, over `base` seconds since `id` started showing (see [`shown_for`]): something
/// arriving. Asks for frames until it is there.
pub fn arrive(ctx: &Context, id: Id, base: f32) -> f32 {
    let t = (shown_for(ctx, id) / secs(base).max(0.001)).min(1.0);
    if t < 1.0 {
        ctx.request_repaint();
    }
    ease_out(t)
}

/// What has just come for `id` (asked for in each pass while it shows) fading in over
/// [`CONTENT`]: multiplies `ui`'s opacity. Gives back the opacity it had, to put back after.
pub fn content_in(ui: &mut Ui, id: Id) -> f32 {
    let was = ui.opacity();
    ui.multiply_opacity(arrive(ui.ctx(), id.with("content"), CONTENT));
    was
}

/// A value's last change, for [`changed`].
#[derive(Clone, Copy)]
struct Change<T> {
    was: T,
    now: T,
    at: f64,
    pass: u64,
}

/// What `value` (of `id`, asked for in each pass while it shows) was before its last change, and
/// the seconds since (infinite if it never changed). After a pass without it, a new value is taken
/// as it is: what changed out of sight doesn't move when it shows again.
fn changed<T: Copy + PartialEq + Send + Sync + 'static>(ctx: &Context, id: Id, value: T) -> (T, f32) {
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    // Per window: each counts its own passes.
    let key = id.with(("motion-changed", ctx.viewport_id()));
    let c = ctx.data_mut(|d| {
        let c = match d.get_temp::<Change<T>>(key).filter(|c| c.pass == pass || c.pass + 1 == pass) {
            Some(c) if c.now != value => Change { was: c.now, now: value, at: now, pass },
            Some(c) => Change { pass, ..c },
            None => Change { was: value, now: value, at: f64::NEG_INFINITY, pass },
        };
        d.insert_temp(key, c);
        c
    });
    (c.was, (now - c.at) as f32)
}

/// A heart (`id`: one per song where it shows) as it is liked or unliked: its scale and how full
/// it is (0-1). Liking pops it ([`pop`], over [`HEART`]) as it fills; unliking dips it ([`dip`],
/// over [`UNHEART`]) as it empties.
pub fn heart(ctx: &Context, id: Id, liked: bool) -> (f32, f32) {
    let (was, ago) = changed(ctx, id.with("heart"), liked);
    let t = ago / secs(if liked { HEART } else { UNHEART }).max(0.001);
    if was == liked || t >= 1.0 {
        return (1.0, if liked { 1.0 } else { 0.0 });
    }
    ctx.request_repaint();
    let fill = ease_out(t);
    if liked { (pop(t, moves()), fill) } else { (dip(t, moves()), 1.0 - fill) }
}

/// Draws the number `n` (as `galley` lays it out) placed at `pos` by `align`. When it changes it
/// rolls there from the one before over [`COUNT`] (see [`rolling`]), inside its line. Gives back
/// where it is (the wider of the two while rolling).
pub fn count<T: Copy + PartialOrd + Send + Sync + 'static>(painter: &Painter, id: Id, n: T, pos: Pos2, align: Align2, galley: impl Fn(T) -> Arc<Galley>) -> Rect {
    let (was, ago) = changed(painter.ctx(), id.with("count"), n);
    roll(painter, was, n, ago / secs(COUNT).max(0.001), pos, align, galley)
}

/// Draws `n` as [`count`] does while it rolls from `was`, `t` (0-1, linear) of the way there: for
/// a change known some other way (a row's number as songs above it go).
pub fn roll<T: Copy + PartialOrd>(painter: &Painter, was: T, n: T, t: f32, pos: Pos2, align: Align2, galley: impl Fn(T) -> Arc<Galley>) -> Rect {
    if was == n || t >= 1.0 {
        let g = galley(n);
        let rect = align.anchor_size(pos, g.size());
        painter.galley(rect.min, g, Color32::PLACEHOLDER);
        return rect;
    }
    painter.ctx().request_repaint();
    let parts = rolling(was, n, ease_out(t), moves()).map(|(v, dy, a)| (galley(v), dy, a));
    let size = parts[0].0.size().max(parts[1].0.size());
    let rect = align.anchor_size(pos, size);
    for (g, dy, a) in parts {
        let mut line = painter.with_clip_rect(painter.clip_rect().intersect(rect));
        line.multiply_opacity(a);
        let at = align.anchor_size(pos, g.size()).min + vec2(0.0, dy * size.y);
        line.galley(at, g, Color32::PLACEHOLDER);
    }
    rect
}

/// `n`, or while it is 0 the last count `id` had that wasn't: for something that fades out as
/// its count goes to none.
pub fn last_count(ctx: &Context, id: Id, n: usize) -> usize {
    let key = id.with("last-count");
    if n > 0 {
        ctx.data_mut(|d| d.insert_temp(key, n));
        n
    } else {
        ctx.data(|d| d.get_temp(key)).unwrap_or(0)
    }
}

/// While something loads (`id`, asked for in each pass while it does): nothing for the first
/// [`LOADING_QUIET`], then placeholder rows down `area`, `row_h` apart, fading in and shimmering in
/// the palette's colours. `cover`: a square at each row's start, as songs have. Gives back how far
/// they have faded in (0 while it is quiet), for anything shown with them.
pub fn loading(ui: &Ui, id: Id, area: Rect, row_h: f32, cover: bool, p: &Palette) -> f32 {
    let ctx = ui.ctx();
    let waited = shown_for(ctx, id.with("loading"));
    let shown = placeholder(waited, secs(CONTENT));
    if shown <= 0.0 {
        ctx.request_repaint_after(std::time::Duration::from_secs_f32((LOADING_QUIET - waited).max(0.0) + 0.01));
        return 0.0;
    }
    ctx.request_repaint();
    let (base, shine) = (mix(p.main_bg, p.text, 0.08), mix(p.main_bg, p.text, 0.14));
    let mut painter = ui.painter().with_clip_rect(area.intersect(ui.clip_rect()));
    painter.multiply_opacity(shown);
    let phase = (ctx.input(|i| i.time) / f64::from(secs(SHIMMER).max(0.01))).fract() as f32;
    // A soft band of shine sweeps across every row at once; with fades only they glow and dim.
    let band = area.width().max(1.0) * 0.4;
    let x = area.left() - band + (area.width() + band * 2.0) * phase;
    let glow = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
    let block = |r: Rect| {
        if !moves() {
            painter.rect_filled(r, CornerRadius::same(4), mix(base, shine, glow));
            return;
        }
        painter.rect_filled(r, CornerRadius::same(4), base);
        // Inside the rounding, so the corners stay round.
        let inner = r.shrink2(vec2(4.0, 0.0));
        if x + band / 2.0 <= inner.left() || x - band / 2.0 >= inner.right() {
            return;
        }
        let mut mesh = Mesh::default();
        for (k, (dx, c)) in [(-band / 2.0, Color32::TRANSPARENT), (0.0, shine), (band / 2.0, Color32::TRANSPARENT)].into_iter().enumerate() {
            mesh.colored_vertex(pos2(x + dx, r.top()), c);
            mesh.colored_vertex(pos2(x + dx, r.bottom()), c);
            if k > 0 {
                let i = 2 * k as u32;
                mesh.add_triangle(i - 2, i - 1, i);
                mesh.add_triangle(i - 1, i + 1, i);
            }
        }
        painter.with_clip_rect(inner.intersect(painter.clip_rect())).add(mesh);
    };
    for k in 0..(area.height() / row_h).floor().max(1.0) as usize {
        let mid = area.top() + row_h * (k as f32 + 0.5);
        let mut left = area.left() + 8.0;
        if cover {
            let s = (row_h - 14.0).clamp(16.0, 40.0);
            block(Rect::from_min_size(pos2(left, mid - s / 2.0), vec2(s, s)));
            left += s + 12.0;
        }
        let w = (area.right() - 8.0 - left).max(0.0);
        // Names of a few lengths, as a list's are.
        let share = [0.58, 0.44, 0.7, 0.5, 0.64, 0.38][k % 6];
        block(Rect::from_min_size(pos2(left, mid - 9.0), vec2(w * share, 9.0)));
        block(Rect::from_min_size(pos2(left, mid + 4.0), vec2(w * share * 0.55, 7.0)));
    }
    shown
}

/// A marker's move from one item to another, for [`marker`].
#[derive(Clone, Copy)]
struct Move {
    chosen: usize,
    from: Rect,
    to: Rect,
    start: f64,
}

/// Where a moving thing is between `from` and `to` at eased progress `t`.
fn between(from: Rect, to: Rect, t: f32) -> Rect {
    Rect::from_min_max(from.min.lerp(to.min, t), from.max.lerp(to.max, t))
}

/// Draws the marker on the chosen item (a tab's underline, a segmented control's pill) as
/// `draw(rect, opacity)`; `target` is the item's number and its rect. When another item is chosen
/// the marker slides there over `base` seconds (with fades only, the old one fades out as the new
/// one fades in); with none chosen it fades away. It is kept relative to `origin` (the strip's
/// corner on the side its items line up from), so it stays put when the strip scrolls or stretches.
pub fn marker(ctx: &Context, id: Id, origin: Pos2, target: Option<(usize, Rect)>, base: f32, mut draw: impl FnMut(Rect, f32)) {
    let shown = toward(ctx, id.with("marker-shown"), target.is_some(), base);
    let now = ctx.input(|i| i.time);
    let key = id.with("motion-marker");
    let length = secs(base).max(0.001) as f64;
    let m = ctx.data_mut(|d| {
        let m = match (d.get_temp::<Move>(key), target) {
            (kept, None) => return kept,
            (None, Some((chosen, rect))) => {
                let to = rect.translate(-origin.to_vec2());
                Move { chosen, from: to, to, start: f64::NEG_INFINITY }
            }
            (Some(m), Some((chosen, rect))) if m.chosen == chosen => Move { to: rect.translate(-origin.to_vec2()), ..m },
            (Some(m), Some((chosen, rect))) => {
                // From where it is now, so a quick second pick turns it smoothly.
                let t = ease_out(((now - m.start) / length).clamp(0.0, 1.0) as f32);
                let at = if moves() { between(m.from, m.to, t) } else if t < 0.5 { m.from } else { m.to };
                Move { chosen, from: at, to: rect.translate(-origin.to_vec2()), start: now }
            }
        };
        d.insert_temp(key, m);
        Some(m)
    });
    let Some(m) = m.filter(|_| shown > 0.0) else { return };
    let t = ((now - m.start) / length).clamp(0.0, 1.0) as f32;
    if t < 1.0 {
        ctx.request_repaint();
    }
    let t = ease_out(t);
    let shift = origin.to_vec2();
    if moves() {
        draw(between(m.from, m.to, t).translate(shift), shown);
    } else {
        if t < 1.0 {
            draw(m.from.translate(shift), (1.0 - t) * shown);
        }
        draw(m.to.translate(shift), t * shown);
    }
}

/// What a layer drew in the last pass it showed, fading out after it stopped.
#[derive(Clone)]
struct Leaving {
    key: Id,
    viewport: ViewportId,
    shapes: Vec<ClippedShape>,
    pass: u64,
    drift: Vec2,
    left_at: Option<f64>,
}

#[derive(Clone, Default)]
struct AllLeaving(Vec<Leaving>);

fn leaving_id() -> Id {
    Id::new("truffle-motion-leaving")
}

/// Keeps what `layer` drew this pass, so that once a pass goes by without this call for `key`, it
/// fades out over [`LEAVE`] (sinking by `drift` meanwhile, unless fades only), over the page but
/// under every pop-up and dialog. Nothing of it takes clicks. Call after the layer is drawn;
/// [`end_pass`] paints.
pub fn fade_out_later(ctx: &Context, layer: LayerId, key: Id, drift: Vec2) {
    let shapes: Vec<ClippedShape> = ctx.graphics(|g| g.get(layer).map(|l| l.all_entries().cloned().collect()).unwrap_or_default());
    let it = Leaving { key, viewport: ctx.viewport_id(), shapes, pass: ctx.cumulative_pass_nr(), drift, left_at: None };
    ctx.data_mut(|d| {
        let all = &mut d.get_temp_mut_or_default::<AllLeaving>(leaving_id()).0;
        match all.iter_mut().find(|l| l.key == key && l.viewport == it.viewport) {
            Some(l) => *l = it,
            None => all.push(it),
        }
    });
}

/// Paints what is fading out (see [`fade_out_later`]). Call at the very end of each pass of each
/// window.
pub fn end_pass(ctx: &Context) {
    let mut all = ctx.data_mut(|d| std::mem::take(&mut d.get_temp_mut_or_default::<AllLeaving>(leaving_id()).0));
    if all.is_empty() {
        return;
    }
    let (now, pass, viewport) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr(), ctx.viewport_id());
    let length = secs(LEAVE).max(0.001) as f64;
    let mut fading = false;
    all.retain_mut(|l| {
        if l.viewport != viewport || l.pass == pass {
            return true;
        }
        // Only what showed in the pass just before (the window may have been hidden since).
        let Some(left_at) = l.left_at.or_else(|| (l.pass + 1 == pass).then_some(now)) else { return false };
        l.left_at = Some(left_at);
        let t = ((now - left_at) / length) as f32;
        if t >= 1.0 {
            return false;
        }
        paint_leaving(ctx, l, ease_in(t));
        fading = true;
        true
    });
    if fading {
        ctx.request_repaint();
    }
    ctx.data_mut(|d| d.insert_temp(leaving_id(), AllLeaving(all)));
}

fn paint_leaving(ctx: &Context, l: &Leaving, t: f32) {
    let shift = if moves() { l.drift * t } else { Vec2::ZERO };
    // Not an area: painted after the middle order's areas, before every pop-up's and dialog's.
    let mut painter = ctx.layer_painter(LayerId::new(Order::Middle, l.key.with("leaving")));
    painter.set_opacity(1.0 - t);
    for c in &l.shapes {
        let mut shape = c.shape.clone();
        shape.translate(shift);
        painter.with_clip_rect(c.clip_rect.translate(shift)).add(shape);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_follow_the_speed() {
        assert_eq!(factor(MotionSpeed::Normal), 1.0);
        assert!((HOVER * factor(MotionSpeed::Snappy) - 0.09).abs() < 1e-6);
        assert!((DIALOG * factor(MotionSpeed::Relaxed) - 0.243).abs() < 1e-6);
        // At Normal nothing is slower than the owner's cap, nor quicker than 90 ms.
        for t in [HOVER, PRESS, SWITCH, TAB, MESSAGE_IN, LEAVE, BACKDROP, DIALOG, HEART, UNHEART, ROW, COUNT, PAGE, CONTENT, ARM] {
            assert!((0.09..=0.2).contains(&t), "{t}");
        }
    }

    #[test]
    fn easing_starts_and_ends_in_place() {
        for f in [ease_out, ease_in] {
            assert_eq!(f(0.0), 0.0);
            assert_eq!(f(1.0), 1.0);
            assert_eq!(f(-1.0), 0.0);
            assert_eq!(f(2.0), 1.0);
            let mut last = 0.0;
            for i in 1..=20 {
                let v = f(i as f32 / 20.0);
                assert!(v >= last, "goes back at {i}");
                last = v;
            }
        }
        // Arriving is most of the way there by half time; leaving has hardly started.
        assert!(ease_out(0.5) > 0.8, "{}", ease_out(0.5));
        assert!(ease_in(0.5) < 0.4, "{}", ease_in(0.5));
    }

    #[test]
    fn fades_only_never_moves_or_grows() {
        assert_eq!(offset(16.0, 0.0, true), 16.0);
        assert_eq!(offset(16.0, 1.0, true), 0.0);
        assert_eq!(offset(16.0, 0.0, false), 0.0);
        assert_eq!(grow(DIALOG_FROM, 0.0, true), DIALOG_FROM);
        assert_eq!(grow(DIALOG_FROM, 1.0, true), 1.0);
        assert_eq!(grow(DIALOG_FROM, 0.0, false), 1.0);
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            assert_eq!(pop(t, false), 1.0);
            assert_eq!(dip(t, false), 1.0);
        }
    }

    #[test]
    fn the_heart_pops_and_dips() {
        assert!((pop(0.0, true) - 0.7).abs() < 1e-6);
        assert!((pop(0.55, true) - 1.22).abs() < 1e-3);
        assert!((pop(1.0, true) - 1.0).abs() < 1e-6);
        assert!((dip(0.5, true) - 0.82).abs() < 1e-3);
        assert!((dip(0.0, true) - 1.0).abs() < 1e-6);
        assert!((dip(1.0, true) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn colours_mix_with_their_alpha() {
        let clear = Color32::TRANSPARENT;
        let white = Color32::WHITE;
        assert_eq!(mix(clear, white, 0.0), clear);
        assert_eq!(mix(clear, white, 1.0), white);
        assert_eq!(mix(clear, white, 0.5).a(), 128);
    }

    #[test]
    fn a_clock_runs_while_asked_for_every_pass() {
        let ctx = Context::default();
        let id = Id::new("thing");
        let run = |time: f64, ask: bool| {
            let raw = egui::RawInput { time: Some(time), ..Default::default() };
            let mut out = None;
            // Nothing to draw on: the fonts' texture is let go of.
            ctx.run_ui(raw, |ui| {
                if ask {
                    out = Some(shown_for(ui.ctx(), id));
                }
            })
            .textures_delta
            .clear();
            out
        };
        assert_eq!(run(1.0, true), Some(0.0));
        assert!((run(1.1, true).unwrap() - 0.1).abs() < 1e-4);
        // A pass without it: it starts again.
        run(1.2, false);
        assert_eq!(run(1.3, true), Some(0.0));
    }

    #[test]
    fn turning_back_half_way_never_jumps() {
        let ctx = Context::default();
        let run = |time: f64, on: bool| {
            let raw = egui::RawInput { time: Some(time), ..Default::default() };
            let mut out = 0.0;
            ctx.run_ui(raw, |ui| out = toward(ui.ctx(), Id::new("switch"), on, SWITCH)).textures_delta.clear();
            out
        };
        assert_eq!(run(1.0, false), 0.0);
        run(1.01, true);
        let part = run(1.01 + f64::from(SWITCH) * 0.3, true);
        assert!(part > 0.5 && part < 1.0, "{part}");
        // Off again: it sets off back from where it was.
        let back = run(1.01 + f64::from(SWITCH) * 0.3 + 0.001, false);
        assert!((back - part).abs() < 0.05, "{part} {back}");
        assert_eq!(run(2.0, false), 0.0);
    }

    #[test]
    fn a_count_rolls_up_when_it_grows_and_down_when_it_shrinks() {
        // Growing: the new number starts a line below, the old one leaves a line up.
        assert_eq!(rolling(12, 13, 0.0, true), [(12, 0.0, 1.0), (13, 1.0, 0.0)]);
        assert_eq!(rolling(12, 13, 1.0, true), [(12, -1.0, 0.0), (13, 0.0, 1.0)]);
        // Shrinking: the other way round.
        assert_eq!(rolling(13, 12, 0.0, true), [(13, 0.0, 1.0), (12, -1.0, 0.0)]);
        assert_eq!(rolling(13, 12, 1.0, true), [(13, 1.0, 0.0), (12, 0.0, 1.0)]);
        // Fades only: they stay on the line.
        assert_eq!(rolling(12, 13, 0.5, false), [(12, 0.0, 0.5), (13, 0.0, 0.5)]);
    }

    #[test]
    fn a_load_shows_nothing_at_first_then_fades_its_rows_in() {
        assert_eq!(placeholder(0.0, CONTENT), 0.0);
        assert_eq!(placeholder(LOADING_QUIET - 0.01, CONTENT), 0.0);
        let early = placeholder(LOADING_QUIET + CONTENT / 4.0, CONTENT);
        assert!(early > 0.0 && early < 1.0, "{early}");
        assert_eq!(placeholder(LOADING_QUIET + CONTENT, CONTENT), 1.0);
        assert_eq!(placeholder(5.0, CONTENT), 1.0);
    }

    #[test]
    fn a_row_takes_its_room_as_it_comes_or_goes() {
        assert_eq!(room(0.25, true), 0.25);
        assert_eq!(room(0.0, true), 0.0);
        // Fades only: all of it while it shows at all, none once gone.
        assert_eq!(room(0.25, false), 1.0);
        assert_eq!(room(0.0, false), 0.0);
    }

    #[test]
    fn only_a_heart_that_changes_in_sight_pops() {
        let ctx = Context::default();
        let run = |time: f64, liked: Option<bool>| {
            let raw = egui::RawInput { time: Some(time), ..Default::default() };
            let mut out = None;
            ctx.run_ui(raw, |ui| out = liked.map(|l| heart(ui.ctx(), Id::new("song"), l))).textures_delta.clear();
            out
        };
        // As it is at first, still.
        assert_eq!(run(1.0, Some(false)), Some((1.0, 0.0)));
        // Liked: it starts small and empty, and is full and still once HEART is over.
        let (scale, fill) = run(1.1, Some(true)).unwrap();
        assert!(scale < 1.0 && fill == 0.0, "{scale} {fill}");
        assert_eq!(run(1.1 + f64::from(HEART) + 0.01, Some(true)), Some((1.0, 1.0)));
        // Unliked out of sight (a pass without it): empty at once when it shows again.
        run(1.5, None);
        assert_eq!(run(1.6, Some(false)), Some((1.0, 0.0)));
    }

    #[test]
    fn a_marker_slides_only_to_a_new_pick() {
        let ctx = Context::default();
        let run = |time: f64, target: Option<(usize, Rect)>| {
            let raw = egui::RawInput { time: Some(time), ..Default::default() };
            let mut drawn = Vec::new();
            ctx.run_ui(raw, |ui| marker(ui.ctx(), Id::new("tabs"), Pos2::ZERO, target, TAB, |r, a| drawn.push((r, a))))
                .textures_delta
                .clear();
            drawn
        };
        let first = Rect::from_min_size(Pos2::ZERO, Vec2::new(40.0, 3.0));
        let second = first.translate(Vec2::new(100.0, 0.0));
        assert_eq!(run(1.0, Some((0, first))), [(first, 1.0)]);
        // The same tab, wider (a resize): there at once.
        let wider = first.expand2(Vec2::new(5.0, 0.0));
        assert_eq!(run(1.05, Some((0, wider))), [(wider, 1.0)]);
        // Another tab: it sets off from where it was and gets there in time.
        assert_eq!(run(1.1, Some((1, second))), [(wider, 1.0)]);
        let half_way = run(1.1 + f64::from(TAB) / 2.0, Some((1, second)))[0].0;
        assert!(half_way.left() > wider.left() && half_way.left() < second.left(), "{half_way:?}");
        assert_eq!(run(1.1 + f64::from(TAB) + 0.01, Some((1, second))), [(second, 1.0)]);
    }
}
