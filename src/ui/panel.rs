//! The side panel (the Final design's `.rp`, 340 wide, 280 at medium widths): "Song N of M in this shuffle"
//! and what is left, the turntable on its plinth (Ctrl+D hides it), and four tabs: Lyrics (synced, the line
//! being sung big with an accent bar), Up next, the shuffle rules and Friends (`friends.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Rect, Sense, Stroke, Ui, pos2, vec2};

use super::fin::{self, Look};
use super::frame::{Layout, Mode};
use super::motion;
use super::svgicon;
use super::theme::{bold, heavy, medium, regular};
use super::tracks::one_line;
use super::util::{fmt_ms, fmt_thousands, fmt_total};
use super::{TruffleApp, Undo};
use crate::model::{Command, LyricLine, PlayStatus, TrackInfo};

/// How long (base seconds, see [`motion::secs`]) the side panel's lyrics take to glide when the sung
/// line changes: the list sliding to keep it in place, the line growing into its size and colour.
const LYRIC_GLIDE: f32 = 0.45;

/// The lyrics list's scroll: gliding from `from` to `to` since `since`, for the sung line `li`.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct LyricScroll {
    from: f32,
    to: f32,
    since: f64,
    li: Option<usize>,
    /// Set once the list has been shown: the first time it is put in place without a glide.
    shown: bool,
}

impl LyricScroll {
    /// Where the scroll is at `now`, the glide taking `secs`.
    fn at(&self, now: f64, secs: f64) -> f32 {
        let t = if secs > 0.0 { ((now - self.since) / secs).clamp(0.0, 1.0) as f32 } else { 1.0 };
        self.from + (self.to - self.from) * motion::ease_out(t)
    }

    /// Still on its way.
    fn gliding(&self, now: f64, secs: f64) -> bool {
        self.from != self.to && now - self.since < secs
    }
}

/// A galley's text with a newline where each of its rows ends, so another layout breaks the same way.
fn rows_text(galley: &egui::Galley) -> String {
    galley.rows.iter().map(|r| r.glyphs.iter().map(|g| g.chr).collect::<String>().trim_end().to_owned()).collect::<Vec<_>>().join("
")
}

/// A song's lyrics as far as Truffle knows them.
#[derive(Clone)]
pub enum Lyrics {
    Loading,
    None,
    Lines { lines: Arc<Vec<LyricLine>>, synced: bool },
}

/// Lyrics fetched this run, by song.
pub type LyricsCache = HashMap<String, Lyrics>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Lyrics,
    UpNext,
    Rules,
    Friends,
}

/// The class a lyric line gets from its distance to the line being sung (`li`, -1 before the first):
/// far (dim), plain (muted), next (text) or now (big).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineLook {
    Far,
    Plain,
    Next,
    Now,
}

pub fn line_look(i: usize, li: Option<usize>) -> LineLook {
    let Some(li) = li else {
        return if i < 2 { LineLook::Next } else if i < 3 { LineLook::Plain } else { LineLook::Far };
    };
    let d = i as i64 - li as i64;
    match d {
        d if d <= -3 => LineLook::Far,
        d if d < 0 => LineLook::Plain,
        0 => LineLook::Now,
        d if d <= 2 => LineLook::Next,
        3 => LineLook::Plain,
        _ => LineLook::Far,
    }
}

/// The line being sung at `pos_ms` (the last one that has started), if any.
pub fn line_at(lines: &[LyricLine], pos_ms: u32) -> Option<usize> {
    lines.iter().rposition(|l| l.start_ms <= pos_ms)
}

impl TruffleApp {
    /// The playing song's lyrics: asked for once, the first time they are needed.
    pub(super) fn current_lyrics(&mut self) -> Option<Lyrics> {
        let uri = self.playback.track.as_ref().filter(|t| !t.is_local)?.uri.clone();
        if let Some(l) = self.lyrics.get(&uri) {
            return Some(l.clone());
        }
        self.lyrics.insert(uri.clone(), Lyrics::Loading);
        self.send(Command::Lyrics(uri));
        Some(Lyrics::Loading)
    }

    pub(super) fn side_panel(&mut self, ui: &mut Ui, l: &Layout) {
        let r = ui.max_rect();
        let full = l.panel_w >= 340.0;
        let head = Rect::from_min_size(r.min, vec2(r.width(), 58.0));
        self.panel_head(ui, head, full);
        let mut y = head.bottom();

        if self.prefs.panel_record {
            // The plinth: 32 narrower than the panel, 1.2 times the record; the record 176 / 150 wide,
            // or 150 / 132 at medium widths, by the window's height.
            let tall = ui.ctx().content_rect().height() >= 820.0;
            let d = match (l.mode == Mode::Wide, tall) {
                (true, true) => 176.0,
                (true, false) => 150.0,
                (false, true) => 150.0,
                (false, false) => 132.0,
            };
            let plinth = Rect::from_min_size(pos2(r.left() + 16.0, y), vec2(r.width() - 32.0, (d * 1.2f32).round()));
            self.panel_plinth(ui, plinth, d);
            y = plinth.bottom() + 14.0;
        }

        // The tabs: 38 tall, a line above and below, 18 apart.
        let tabs = Rect::from_min_size(pos2(r.left(), y), vec2(r.width(), 38.0));
        self.panel_tabs(ui, tabs);
        let body = Rect::from_min_max(pos2(r.left(), tabs.bottom()), r.max);
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("panel-body").max_rect(body));
        child.set_clip_rect(body.intersect(ui.clip_rect()));
        match self.frame.panel_tab {
            Tab::Lyrics => self.panel_lyrics(&mut child, body),
            Tab::UpNext => self.panel_up_next(&mut child, body),
            Tab::Rules => self.panel_rules(&mut child, body, full),
            Tab::Friends => self.panel_friends(&mut child, body),
        }
    }

    /// "Song 706 of 890 in this shuffle" (19 bold, then 11.5 muted), "184 to go · about 7 h 18 min" (11)
    /// under it, and the record and close buttons on the right.
    fn panel_head(&mut self, ui: &mut Ui, head: Rect, full: bool) {
        let p = self.palette;
        let x_btn = Rect::from_min_size(pos2(head.right() - 8.0 - 30.0, head.center().y - 15.0), vec2(30.0, 30.0));
        let tt_btn = x_btn.translate(vec2(-32.0, 0.0));
        let w = ui.ctx().content_rect().width();
        if fin::ib(ui, &p, x_btn, Id::new("panel-close"), "x", Look::Plain, "Close (Ctrl+P)").clicked() {
            self.toggle_side_panel(w);
        }
        let rec = self.prefs.panel_record;
        if fin::ib(ui, &p, tt_btn, Id::new("panel-rec"), "tt", Look::on_box(rec), "Record (Ctrl+D)").clicked() {
            self.toggle_panel_record();
        }
        let left = head.left() + 16.0;
        let max_w = tt_btn.left() - 8.0 - left;
        let Some(d) = self.deck.clone().filter(|d| !d.order.is_empty()) else {
            ui.painter().text(pos2(left, head.center().y), Align2::LEFT_CENTER, "Nothing playing", regular(12.0), p.text_weak);
            return;
        };
        let n = d.order.len();
        let at = (d.now + 1).min(n);
        let to_go = n.saturating_sub(at);
        let pos = self.playback.current_position_ms() as u64;
        let ms: u64 = d.order.iter().skip(d.now).map(|&i| u64::from(d.tracks[i as usize].duration_ms)).sum::<u64>().saturating_sub(pos);
        let whose = if d.shuffled { "in this shuffle" } else { "in order" };
        let tail = if full { format!("of {} {whose}", fmt_thousands(n)) } else { format!("of {}", fmt_thousands(n)) };
        let big = ui.fonts_mut(|f| f.layout_no_wrap(format!("Song {}", fmt_thousands(at)), heavy(19.0), p.text_strong));
        let small = one_line(ui, &tail, regular(11.5), p.text_weak, (max_w - big.size().x - 4.0).max(20.0), false);
        let sub = one_line(ui, &format!("{} to go · about {}", fmt_thousands(to_go), fmt_total(ms)), regular(11.0), p.text_weak, max_w, false);
        let h = big.size().y + 2.0 + sub.size().y;
        let top = head.center().y - h / 2.0;
        let bw = big.size().x;
        let base = top + big.size().y;
        ui.painter().galley(pos2(left, top), big, p.text_strong);
        // " of …" sits on the big number's baseline.
        ui.painter().galley(pos2(left + bw + 4.0, base - small.size().y - 1.0), small, p.text_weak);
        ui.painter().galley(pos2(left, base + 2.0), sub, p.text_weak);
    }

    /// Ctrl+D and the record button: the turntable on the side panel shows or hides.
    pub(super) fn toggle_panel_record(&mut self) {
        self.prefs.panel_record = !self.prefs.panel_record;
        self.prefs_changed();
        let text = if self.prefs.panel_record { "Record shown" } else { "Record hidden · Ctrl+D shows it" };
        self.note(text, None);
    }

    /// The plinth (`.plinth`): a dark gradient box, round 12, with the turntable in the middle, the start
    /// button and its light front left, and the 33 / 45 keys front right.
    pub(super) fn panel_plinth(&mut self, ui: &mut Ui, plinth: Rect, d: f32) {
        // The record player is dark in both themes.
        let p = self.palette.on_dark();
        let painter = ui.painter().clone();
        fin::gradient_rect(&painter, plinth, 12, Color32::from_rgb(26, 26, 29), Color32::from_rgb(17, 17, 19));
        painter.rect_stroke(plinth, CornerRadius::same(12), Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        painter.hline((plinth.left() + 10.0)..=(plinth.right() - 10.0), plinth.top() + 1.5, Stroke::new(1.0, Color32::from_white_alpha(11)));

        let track = self.playback.track.clone();
        let url = track.as_ref().and_then(|t| t.cover_large.clone().or_else(|| t.cover_small.clone()));
        let label = url.as_deref().and_then(|u| self.images.get(u, d * 0.72)).map(|t| t.id);
        let spin = self.deck_spin(ui);
        let playing = self.playback.status == PlayStatus::Playing;
        let arm = motion::tonearm(ui.ctx(), Id::new("panel-arm"), self.is_playing(), self.song_done());
        let origin = pos2(plinth.center().x - d * 1.16 / 2.0, plinth.center().y - d / 2.0);
        fin::turntable(&painter, origin, d, label, spin, &arm);

        // Start / stop (18×12) and the light, 12 in and 10 up.
        let stb = Rect::from_min_size(pos2(plinth.left() + 12.0, plinth.bottom() - 10.0 - 12.0), vec2(18.0, 12.0));
        let resp = ui.interact(stb.expand(3.0), Id::new("plinth-start"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        painter.rect_filled(stb, CornerRadius::same(3), if resp.hovered() { p.widget_hover_bg } else { p.widget_bg });
        painter.rect_stroke(stb, CornerRadius::same(3), Stroke::new(1.0, Color32::from_rgb(52, 52, 58)), egui::StrokeKind::Inside);
        let tip = if self.listening() { "Listening along" } else { "Start / stop" };
        if resp.on_hover_text(tip).clicked() && track.is_some() && !self.listening() {
            self.send(Command::PlayPause);
        }
        let led = pos2(stb.right() + 7.0 + 3.0, stb.center().y);
        if playing {
            painter.circle_filled(led, 6.0, p.accent_ink.gamma_multiply(0.25));
            painter.circle_filled(led, 3.0, p.accent_ink);
        } else {
            painter.circle_filled(led, 3.0, p.text_faint);
        }
        // 33 and 45: 9.5 px, 5 either side, 4 apart, 12 in and 9 up.
        let font = regular(9.5);
        let mut x = plinth.right() - 12.0;
        for (text, on) in [("45", false), ("33", true)] {
            let g = ui.fonts_mut(|f| f.layout_no_wrap(text.to_owned(), font.clone(), if on { p.text } else { p.text_weak }));
            let key = Rect::from_min_size(pos2(x - g.size().x - 10.0, plinth.bottom() - 9.0 - g.size().y - 2.0), vec2(g.size().x + 10.0, g.size().y + 2.0));
            painter.rect_filled(key, CornerRadius::same(3), if on { p.widget_hover_bg } else { p.widget_bg });
            painter.galley(pos2(key.left() + 5.0, key.top() + 1.0), g, if on { p.text } else { p.text_weak });
            x = key.left() - 4.0;
        }
    }

    /// Lyrics, Up next (with how many), Rules and Friends; with Friends, its layout button on the
    /// right.
    pub(super) fn panel_tabs(&mut self, ui: &mut Ui, tabs: Rect) {
        let p = self.palette;
        let painter = ui.painter().clone();
        painter.hline(tabs.x_range(), tabs.top() + 0.5, Stroke::new(1.0, p.border));
        painter.hline(tabs.x_range(), tabs.bottom() - 0.5, Stroke::new(1.0, p.border));
        let count = self.up_next.as_ref().map_or(0, |l| l.len().min(50));
        let mut x = tabs.left() + 16.0;
        let mut line = None;
        for (k, (tab, name)) in [(Tab::Lyrics, "Lyrics"), (Tab::UpNext, "Up next"), (Tab::Rules, "Rules"), (Tab::Friends, "Friends")].into_iter().enumerate() {
            let on = self.frame.panel_tab == tab;
            let font = if on { bold(12.0) } else { medium(12.0) };
            let g = ui.fonts_mut(|f| f.layout_no_wrap(name.to_owned(), font, Color32::WHITE));
            let mut w = g.size().x;
            let small = (tab == Tab::UpNext && count > 0).then(|| ui.fonts_mut(|f| f.layout_no_wrap(count.to_string(), medium(10.5), p.text_weak)));
            if let Some(s) = &small {
                w += 5.0 + s.size().x;
            }
            let rect = Rect::from_min_size(pos2(x, tabs.top()), vec2(w, tabs.height()));
            let resp = ui.interact(rect, Id::new(("panel-tab", name)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let color = fin::tab_ink(&p, on, resp.hovered());
            let color = motion::color(ui.ctx(), resp.id, color, motion::HOVER);
            let gh = g.size().y;
            painter.galley_with_override_text_color(pos2(x, rect.center().y - gh / 2.0), g, color);
            if let Some(s) = small {
                let sh = s.size().y;
                painter.galley(pos2(x + w - s.size().x, rect.center().y - sh / 2.0 + 0.5), s, p.text_weak);
            }
            if on {
                line = Some((k, Rect::from_min_max(pos2(rect.left(), rect.bottom() - 2.0), pos2(rect.right(), rect.bottom() + 1.0))));
            }
            if resp.clicked() {
                self.frame.panel_tab = tab;
            }
            x += w + 18.0;
        }
        // The line slides to the tab picked.
        motion::marker(ui.ctx(), Id::new("panel-tab-line"), tabs.min, line, motion::TAB, |bar, a| {
            painter.rect_filled(bar, CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 }, p.accent_ink.gamma_multiply(a));
        });
        // The Friends tab's layout, at the strip's right end while it shows.
        if self.frame.panel_tab == Tab::Friends {
            let b = Rect::from_min_size(pos2(tabs.right() - 6.0 - 24.0, tabs.center().y - 12.0), vec2(24.0, 24.0));
            self.friends_layout_button(ui, b);
        }
    }

    /// The lyrics (`.lyr`): 14 px medium lines 12 apart, the line being sung 19 px bold with a 3 px accent
    /// bar, the lines around it brighter and the far ones dim; the sung line stays 30 % down the view.
    /// When it changes the list glides there and the new line grows as the last one shrinks back
    /// ([`LYRIC_GLIDE`]); with fades only everything jumps. Taller and bigger while the record is
    /// hidden. A click on a line plays from there.
    pub(super) fn panel_lyrics(&mut self, ui: &mut Ui, body: Rect) {
        let p = self.palette;
        let tall = !self.prefs.panel_record;
        let (pad_top, gap, size, now_size) = if tall { (24.0, 14.0, 15.0, 21.0) } else { (18.0, 12.0, 14.0, 19.0) };
        let lyrics = self.current_lyrics();
        // What has just come for this song fades in.
        let fade_id = Id::new(("lyrics", self.playback.track.as_ref().map(|t| t.uri.clone())));
        let left = body.left() + 20.0;
        let width = body.width() - 20.0 - 22.0;
        let lines = match lyrics {
            Some(Lyrics::Lines { lines, synced }) => (lines, synced),
            Some(Lyrics::Loading) => {
                // Nothing at first, then placeholder lines.
                let rows = Rect::from_min_size(pos2(left, body.top() + pad_top - 8.0), vec2(width, (size * 2.2) * 6.0));
                motion::loading(ui, Id::new("lyrics-loading"), rows, size * 2.2, false, &self.palette);
                return;
            }
            Some(Lyrics::None) | None => {
                let text = if self.playback.track.is_some() { "No lyrics for this song" } else { "Nothing playing" };
                let was = motion::content_in(ui, fade_id);
                ui.painter().text(pos2(left, body.top() + pad_top + 8.0), Align2::LEFT_CENTER, text, medium(size), p.text_weak);
                ui.set_opacity(was);
                return;
            }
        };
        let was = motion::content_in(ui, fade_id);
        let (lines, synced) = lines;
        let pos = self.seek_preview.map_or(self.playback.current_position_ms(), |v| v as u32);
        let li = if synced { line_at(&lines, pos) } else { None };
        let ctx = ui.ctx().clone();

        // Lay every line out to find where the sung one is. Each line is `w` of the way to being the
        // sung one: its size, weight, indent and colour follow, so the line becoming sung grows as the
        // last one shrinks back.
        let mut galleys = Vec::with_capacity(lines.len());
        let mut y = 0.0;
        let mut now_top = None;
        for (i, line) in lines.iter().enumerate() {
            let look = if synced { line_look(i, li) } else { LineLook::Next };
            let w = if synced { motion::toward(&ctx, Id::new(("lyric-now", i)), look == LineLook::Now, LYRIC_GLIDE) } else { 0.0 };
            // In quarter-pixel steps: every size is a font of its own to egui, kept for the run.
            let px = ((size + (now_size - size) * w) * 4.0).round() / 4.0;
            let font = if w > 0.5 { heavy(px) } else { medium(px) };
            let indent = 15.0 * w;
            // Every line breaks where it breaks when sung (laid out at that size, with that indent), so
            // it doesn't re-wrap as it grows: those breaks go into the text at the smaller sizes.
            let sung = ui.fonts_mut(|f| f.layout_job(egui::text::LayoutJob::simple(line.words.clone(), heavy(now_size), p.text, width - 15.0)));
            let g = if w >= 1.0 {
                sung
            } else {
                let job = egui::text::LayoutJob::simple(rows_text(&sung), font, p.text, width - indent);
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let margin = 2.0 * w;
            y += margin;
            if look == LineLook::Now {
                now_top = Some(y);
            }
            let h = g.size().y;
            galleys.push((y, indent, look, w, g, i));
            y += h + margin + gap;
        }
        let content_h = y - gap + 140.0;
        let view_h = body.height() - pad_top;
        let want = now_top.map_or(0.0, |t| t - view_h * 0.3);
        let target = if want > view_h * 0.1 { want } else { 0.0 };
        // When the sung line changes the list glides to keep it in place (a jump with fades only, and
        // the first time); the wheel scrolls freely and ends a glide.
        let id = Id::new("lyrics-scroll");
        let secs = f64::from(motion::secs(LYRIC_GLIDE));
        let now = ctx.input(|i| i.time);
        let mut s = ui.data(|d| d.get_temp::<LyricScroll>(id)).unwrap_or_default();
        if s.li != li || !s.shown {
            let from = if s.shown && motion::moves() { s.at(now, secs) } else { target };
            s = LyricScroll { from, to: target, since: now, li, shown: true };
        }
        let hovered = ui.rect_contains_pointer(body);
        if hovered {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let here = s.at(now, secs) - scroll;
                s.from = here;
                s.to = here;
            }
        }
        let most = (content_h - view_h).max(0.0);
        let offset = s.at(now, secs).clamp(0.0, most);
        if s.gliding(now, secs) {
            ctx.request_repaint();
        }
        ui.data_mut(|d| d.insert_temp(id, s));

        let top = body.top() + pad_top - offset;
        for (gy, indent, look, w, g, i) in galleys {
            let at = pos2(left + indent, top + gy);
            let rect = Rect::from_min_size(pos2(left, at.y), vec2(width, g.size().y));
            if rect.bottom() < body.top() || rect.top() > body.bottom() {
                continue;
            }
            let resp = ui.interact(rect, Id::new(("lyric", i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let rest = match (look, resp.hovered()) {
                (LineLook::Far, true) => p.text_weak,
                (LineLook::Plain | LineLook::Next, true) => p.text,
                (LineLook::Now, _) | (LineLook::Next, false) => p.text,
                (LineLook::Far, false) => p.text_faint,
                (LineLook::Plain, false) => p.text_weak,
            };
            let color = motion::mix(rest, p.text_strong, w);
            if w > 0.01 {
                // The accent bar comes and goes with the sung line.
                ui.painter().rect_filled(Rect::from_min_size(pos2(left, rect.top()), vec2(3.0, rect.height())), CornerRadius::ZERO, p.accent_ink.gamma_multiply(w));
            }
            ui.painter().galley_with_override_text_color(at, g, color);
            if synced && !self.listening() && resp.on_hover_text(format!("Play from {}", fmt_ms(lines[i].start_ms))).clicked() {
                let ms = lines[i].start_ms;
                self.playback.position_ms = ms;
                self.playback.position_at = Some(std::time::Instant::now());
                self.send(Command::SeekMs(ms));
            }
        }
        ui.set_opacity(was);
        // The fade at the bottom (60 px to the panel's grey).
        let fade = Rect::from_min_max(pos2(body.left(), body.bottom() - 60.0), body.max);
        fin::vgradient(ui.painter(), fade, Color32::TRANSPARENT, p.sidebar_bg);
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }

    /// Up next (`.rpb`): Now, Queued, and From shuffle (the next 50 of the play order), each a small
    /// spaced capital heading over 40-tall rows.
    pub(super) fn panel_up_next(&mut self, ui: &mut Ui, body: Rect) {
        let p = self.palette;
        let id = Id::new("upnext-scroll");
        egui::ScrollArea::vertical().id_salt(id).auto_shrink(false).show(ui, |ui| {
            let left = body.left() + 12.0;
            let width = body.width() - 24.0;
            let mut y = ui.cursor().top() + 4.0;
            let heading = |ui: &mut Ui, y: &mut f32, text: &str, count: Option<usize>, action: Option<&str>| -> bool {
                *y += 12.0;
                let g = fin::spaced(ui, &text.to_uppercase(), bold(10.5), p.text_weak, 0.95);
                let gh = g.size().y;
                let gw = g.size().x;
                ui.painter().galley(pos2(left + 4.0, *y), g, p.text_weak);
                if let Some(c) = count {
                    ui.painter().text(pos2(left + 4.0 + gw + 8.0, *y + gh / 2.0), Align2::LEFT_CENTER, c.to_string(), medium(10.5), p.text_weak);
                }
                let mut clicked = false;
                if let Some(a) = action {
                    let g2 = ui.fonts_mut(|f| f.layout_no_wrap(a.to_owned(), medium(11.0), p.text_weak));
                    let r = Rect::from_min_size(pos2(left + width - 4.0 - g2.size().x, *y), g2.size());
                    let resp = ui.interact(r, Id::new(("upnext-act", a)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                    ui.painter().galley_with_override_text_color(r.min, g2, if resp.hovered() { p.text } else { p.text_weak });
                    clicked = resp.clicked();
                }
                *y += gh + 6.0;
                clicked
            };
            if let Some(t) = self.playback.track.clone() {
                heading(ui, &mut y, "Now", None, None);
                self.queue_row_fin(ui, Rect::from_min_size(pos2(left, y), vec2(width, 40.0)), &t, true, false);
                y += 40.0;
            }
            // Up next starts with the songs the user queued.
            let list = self.up_next.clone().unwrap_or_default();
            let queued = self.queued.min(list.len());
            if heading(ui, &mut y, "Queued", Some(queued), (queued > 0).then_some("Clear")) {
                self.send(Command::Unqueue(None));
            }
            if queued == 0 {
                ui.painter().text(pos2(left + 6.0, y + 2.0), Align2::LEFT_TOP, "Right-click a song and choose Add to queue", regular(11.0), p.text_weak);
                y += 20.0;
            }
            for (k, t) in list.iter().take(queued).enumerate() {
                let row = Rect::from_min_size(pos2(left, y), vec2(width, 40.0));
                self.queue_row_fin(ui, row, t, false, true);
                let menu = ui.interact(row, Id::new(("queued-row", k)), Sense::click());
                let mut remove = false;
                menu.context_menu(|ui| {
                    ui.set_width(200.0);
                    if fin::menu_item(ui, Some("x"), "Remove from queue", None, false, true).clicked() {
                        remove = true;
                        ui.close();
                    }
                });
                if remove {
                    self.send(Command::Unqueue(Some(k)));
                }
                y += 40.0;
            }
            let shuffle = self.current_shuffle();
            let rest = &list[queued..];
            if heading(ui, &mut y, if shuffle { "From shuffle" } else { "Next in the list" }, Some(rest.len().min(50)), shuffle.then_some("Reshuffle")) {
                self.reshuffle();
            }
            for t in rest.iter().take(50) {
                self.queue_row_fin(ui, Rect::from_min_size(pos2(left, y), vec2(width, 40.0)), t, false, true);
                y += 40.0;
            }
            ui.allocate_space(vec2(width, (y - ui.cursor().top() + 16.0).max(0.0)));
            let _ = p;
        });
    }

    /// A 40-tall song row (`.qr`): the drag grip, a 28 cover, the title and artists, and the time.
    fn queue_row_fin(&mut self, ui: &mut Ui, rect: Rect, t: &TrackInfo, now: bool, grip: bool) {
        let p = self.palette;
        let resp = ui.interact(rect, Id::new(("qrow", &t.uri, now)), Sense::click());
        if now || resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover_bg);
        }
        let mut x = rect.left() + 6.0;
        if grip {
            svgicon::paint(ui.painter(), Rect::from_min_size(pos2(x, rect.center().y - 6.5), vec2(13.0, 13.0)), "drag", p.text_faint, svgicon::STROKE);
            x += 13.0 + 10.0;
        }
        let cover = Rect::from_min_size(pos2(x, rect.center().y - 14.0), vec2(28.0, 28.0));
        if !self.images.paint(ui, t.cover_small.as_deref(), cover, 3, p.placeholder) {
            svgicon::paint(ui.painter(), cover.shrink(7.0), "note", p.text_weak, svgicon::STROKE);
        }
        x = cover.right() + 10.0;
        let right = rect.right() - 8.0;
        let time_w = 34.0;
        let eq_w = if now { 13.0 + 10.0 } else { 0.0 };
        let tw = (right - time_w - 10.0 - eq_w - x).max(20.0);
        let title = one_line(ui, &t.name, if now { bold(12.0) } else { medium(12.0) }, if now { p.text_strong } else { p.text }, tw, false);
        let artist = one_line(ui, &t.artist_line(), regular(11.0), p.text_weak, tw, false);
        let h = title.size().y + 1.0 + artist.size().y;
        let ty = rect.center().y - h / 2.0;
        let th = title.size().y;
        ui.painter().galley(pos2(x, ty), title, p.text);
        ui.painter().galley(pos2(x, ty + th + 1.0), artist, p.text_weak);
        if now {
            svgicon::paint(ui.painter(), Rect::from_min_size(pos2(right - time_w - 10.0 - 13.0, rect.center().y - 6.5), vec2(13.0, 13.0)), "eq", p.accent_ink, svgicon::STROKE);
        }
        ui.painter().text(pos2(right, rect.center().y), Align2::RIGHT_CENTER, fmt_ms(t.duration_ms), regular(11.0), p.text_weak);
        if resp.on_hover_cursor(CursorIcon::PointingHand).double_clicked() && !now {
            self.play_track_from_up_next(&t.uri);
        }
    }

    fn play_track_from_up_next(&mut self, uri: &str) {
        let Some(d) = self.deck.clone() else { return };
        if let Some(ti) = d.tracks.iter().position(|t| t.uri == uri) {
            self.send(Command::PlaySource { source: d.source.clone(), start_index: Some(ti), shuffle: d.shuffled });
        }
    }

    /// The rules card (`.rules`): True shuffle, then the three rules that shape the order (indented, dim
    /// while True shuffle is on), then Skip blocked; RESHUFFLE under it.
    pub(super) fn panel_rules(&mut self, ui: &mut Ui, body: Rect, full: bool) {
        let p = self.palette;
        let left = body.left() + 16.0;
        let width = body.width() - 32.0;
        let mut rules = self.rules.clone();
        let truly = rules.truly_random;
        let rows: [(&str, &str, bool, bool, u8); 5] = [
            ("True shuffle", if truly { "Every order equally likely" } else { "The rules below shape the order" }, truly, true, 0),
            ("No repeats", "Play every song once first", rules.no_repeat_until_exhausted, !truly, 1),
            ("Mix artists", "Not one artist twice in a row", rules.no_artist_repeat, !truly, 2),
            ("Rare first", "Favour songs you rarely play", rules.favour_rarely_played, !truly, 3),
            ("Skip blocked", "Never play blocked songs", rules.skip_blocked, true, 4),
        ];
        let font = if full { 12.0 } else { 11.5 };
        // Measure the rows first: 34 tall at least, 7 above and below the text.
        let card_top = body.top() + 4.0 + 10.0;
        let mut heights = Vec::new();
        for (title, note, ..) in rows {
            let h = if note.is_empty() { 34.0f32 } else { (7.0 + font * 1.3 + 2.0 + 10.5 * 1.3 + 7.0f32).max(34.0) };
            heights.push(h.round());
            let _ = title;
        }
        let card_h: f32 = heights.iter().sum::<f32>() + 2.0;
        let card = Rect::from_min_size(pos2(left, card_top), vec2(width, card_h));
        ui.painter().rect_filled(card, CornerRadius::same(8), p.raised_bg);
        ui.painter().rect_stroke(card, CornerRadius::same(8), Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
        let mut y = card.top() + 1.0;
        let mut changed: Option<(String, bool)> = None;
        for (k, ((title, note, on, enabled, which), h)) in rows.into_iter().zip(heights).enumerate() {
            let row = Rect::from_min_size(pos2(card.left(), y), vec2(card.width(), h));
            if k > 0 {
                ui.painter().hline(row.x_range(), row.top(), Stroke::new(1.0, p.border));
            }
            let sub = (1..=3).contains(&which);
            let indent = if sub { if full { 26.0 } else { 20.0 } } else { 12.0 };
            let sw = Rect::from_min_size(pos2(row.right() - (if full { 10.0 } else { 8.0 }) - 36.0, row.center().y - 10.0), vec2(36.0, 20.0));
            let text_w = sw.left() - (if full { 10.0 } else { 6.0 }) - (row.left() + indent);
            let color = if enabled { p.text } else { p.text_faint };
            let tg = one_line(ui, title, regular(font), color, text_w, false);
            if note.is_empty() {
                let th = tg.size().y;
                ui.painter().galley(pos2(row.left() + indent, row.center().y - th / 2.0), tg, color);
            } else {
                let ng = one_line(ui, note, regular(10.5), if enabled { p.text_weak } else { p.text_faint }, text_w, false);
                let total = tg.size().y + 2.0 + ng.size().y;
                let ty = row.center().y - total / 2.0;
                let th = tg.size().y;
                ui.painter().galley(pos2(row.left() + indent, ty), tg, color);
                ui.painter().galley(pos2(row.left() + indent, ty + th + 2.0), ng, if enabled { p.text_weak } else { p.text_faint });
            }
            if fin::switch(ui, &p, sw, Id::new(("rule", which)), on, enabled, title) {
                match which {
                    0 => rules.truly_random = !on,
                    1 => rules.no_repeat_until_exhausted = !on,
                    2 => rules.no_artist_repeat = !on,
                    3 => rules.favour_rarely_played = !on,
                    _ => rules.skip_blocked = !on,
                }
                changed = Some((title.to_owned(), !on));
            }
            y += h;
        }
        if let Some((title, now_on)) = changed {
            let before = self.rules.clone();
            self.set_rules(rules);
            self.note(format!("{title}: {}", if now_on { "on" } else { "off" }), Some(Undo::Rules(before)));
        }
        // RESHUFFLE and the hint, 12 under the card.
        let can = self.playback.track.is_some() && self.current_shuffle();
        let at = pos2(left, card.bottom() + 12.0);
        let resp = fin::text_button(ui, &p, at, Id::new("panel-reshuffle"), "Reshuffle", Some("refresh"), false, can);
        let hint_x = resp.rect.right() + 10.0;
        if resp.on_hover_text("Shuffle the songs still to come again (R)").clicked() && can {
            self.reshuffle();
        }
        ui.painter().text(pos2(hint_x, at.y + 14.0), Align2::LEFT_CENTER, "Ctrl+D hides the record", regular(11.0), p.text_weak);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lyrics_list_glides_to_the_sung_line() {
        let s = LyricScroll { from: 100.0, to: 300.0, since: 10.0, li: Some(3), shown: true };
        assert_eq!(s.at(10.0, 0.5), 100.0);
        assert_eq!(s.at(10.5, 0.5), 300.0);
        assert_eq!(s.at(11.0, 0.5), 300.0);
        let mid = s.at(10.25, 0.5);
        assert!(mid > 200.0 && mid < 300.0, "eased out: past halfway at half time, {mid}");
        assert!(s.gliding(10.2, 0.5) && !s.gliding(10.6, 0.5));
        // Stopped by the wheel: where it was, no longer on its way.
        let still = LyricScroll { from: 150.0, to: 150.0, ..s };
        assert!(!still.gliding(10.2, 0.5));
    }

    fn lines(starts: &[u32]) -> Vec<LyricLine> {
        starts.iter().map(|&s| LyricLine { start_ms: s, words: format!("at {s}") }).collect()
    }

    #[test]
    fn the_sung_line_is_the_last_one_started() {
        let l = lines(&[1000, 5000, 9000]);
        assert_eq!(line_at(&l, 0), None);
        assert_eq!(line_at(&l, 1000), Some(0));
        assert_eq!(line_at(&l, 8999), Some(1));
        assert_eq!(line_at(&l, 60_000), Some(2));
    }

    #[test]
    fn lines_dim_with_distance_like_the_mockup() {
        let looks: Vec<LineLook> = (0..9).map(|i| line_look(i, Some(4))).collect();
        use LineLook::*;
        assert_eq!(looks, [Far, Far, Plain, Plain, Now, Next, Next, Plain, Far]);
        assert_eq!((line_look(0, None), line_look(1, None), line_look(2, None), line_look(3, None)), (Next, Next, Plain, Far));
    }
}
