//! The player bar (the Final design's `.pb`): the playing song on the left, the controls and the seek line in
//! the middle, the side panel button, volume, sleep timer, mini player and settings on the right. Also the
//! mini player's pieces (previous / play / next, the heart, the seek row).

use std::time::{Duration, Instant};

use egui::{Align2, Id, Popup, PopupCloseBehavior, Rect, RectAlign, Sense, Ui, pos2, vec2};

use super::fin::{self, Look};
use super::frame::{Layout, Mode};
use super::motion;
use super::svgicon;
use super::theme::{Palette, bold, regular};
use super::tracks::one_line;
use super::util::fmt_ms;
use super::TruffleApp;
use crate::model::{Command, PlayStatus, RepeatMode, SleepTimer, SleepTimerState};

pub const BAR_HEIGHT: f32 = 72.0;
const VOLUME_DEBOUNCE: Duration = Duration::from_millis(120);
/// Sleep timer lengths offered in its menu, in minutes.
const SLEEP_MINUTES: [u32; 5] = [15, 30, 45, 60, 90];

/// An instant old enough that a debounced volume change is sent right away.
pub(super) fn due_now() -> Instant {
    Instant::now().checked_sub(VOLUME_DEBOUNCE).unwrap_or_else(Instant::now)
}

/// Remaining sleep-timer time as minutes and seconds (`90:00`, `4:05`), or "song end".
pub(super) fn sleep_timer_label(state: SleepTimerState, now: Instant) -> String {
    match state {
        SleepTimerState::Until(at) => {
            let left = at.saturating_duration_since(now);
            // Round up, so "0:00" only shows once the time is really up.
            let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
            format!("{}:{:02}", secs / 60, secs % 60)
        }
        SleepTimerState::EndOfTrack => "song end".to_owned(),
    }
}

/// What the volume box did in a pass.
#[derive(Debug, PartialEq)]
enum VolumeBox {
    Open,
    /// Enter, with a volume typed (100 at most).
    Set(u8),
    /// Esc, a click elsewhere, or Enter on an empty box.
    Closed,
}

/// The box for typing a volume, just above `area` (the mute button and the volume line): `text` is
/// what has been typed (digits only), `now` the volume shown greyed until something is.
/// Where the volume's pill goes: `size`, centred over the knob of the line `bar` at `volume` (0-100),
/// 4 above it, kept inside `screen`.
fn bubble_rect(bar: Rect, volume: f32, size: egui::Vec2, screen: Rect) -> Rect {
    let knob = bar.left() + bar.width() * (volume / 100.0).clamp(0.0, 1.0);
    let x = (knob - size.x / 2.0).min(screen.right() - 4.0 - size.x).max(screen.left() + 4.0);
    Rect::from_min_size(pos2(x, bar.top() - 4.0 - size.y), size)
}

/// The volume as a number in a small pill over the knob of the volume line `bar`, over everything.
fn volume_bubble(ui: &Ui, p: &Palette, bar: Rect, volume: f32) {
    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("volume-bubble")));
    let g = painter.layout_no_wrap(format!("{}%", volume.round() as u8), super::theme::medium(11.0), p.text_strong);
    let pill = bubble_rect(bar, volume, g.size() + vec2(12.0, 6.0), ui.ctx().content_rect());
    let round = egui::CornerRadius::same(5);
    painter.add(egui::Shadow { offset: [0, 3], blur: 10, spread: 0, color: egui::Color32::from_black_alpha(110) }.as_shape(pill, round));
    painter.rect_filled(pill, round, p.raised_bg);
    painter.rect_stroke(pill, round, egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
    painter.galley(pill.min + vec2(6.0, 3.0), g, p.text_strong);
}

fn volume_box(ui: &Ui, p: &Palette, area: Rect, text: &mut String, now: u8) -> VolumeBox {
    let mut open = true;
    let mut typed = None;
    Popup::new(Id::new("bar-volume-box"), ui.ctx().clone(), area, ui.layer_id())
        .open_bool(&mut open)
        .align(RectAlign::TOP)
        .align_alternatives(&[RectAlign::TOP_END])
        .gap(8.0)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(text).desired_width(36.0).char_limit(3).hint_text(now.to_string()).horizontal_align(egui::Align::Center);
                let resp = super::theme::text_field(ui, p, edit);
                ui.label("%");
                if resp.lost_focus() {
                    typed = Some(ui.input(|i| i.key_pressed(egui::Key::Enter)));
                } else if !resp.has_focus() {
                    resp.request_focus();
                }
            });
        });
    text.retain(|c| c.is_ascii_digit());
    match typed {
        Some(true) => text.parse::<u32>().map_or(VolumeBox::Closed, |v| VolumeBox::Set(v.min(100) as u8)),
        None if open => VolumeBox::Open,
        _ => VolumeBox::Closed,
    }
}

/// Where the controls and the seek line are centred in the player bar at `r`.
pub(super) fn controls_center(r: Rect, l: &Layout) -> f32 {
    let (np_w, rt_w) = if l.mode != Mode::Wide { (240.0, 210.0) } else { (290.0, 290.0) };
    let left = r.left() + 14.0 + np_w + 12.0;
    let right = r.right() - 14.0 - rt_w - 12.0;
    (left + right) / 2.0
}

/// The playing song's name in the player bar at `r`: clicked, or dragged onto a playlist to add the
/// song `uri` there, as from a list (none for a local file, which can't go into one). Only the name
/// drags, and only once the mouse moves: a click never shows the drag.
fn bar_title(ui: &Ui, r: Rect, uri: Option<&str>) -> egui::Response {
    let resp = ui.interact(r, Id::new("bar-title"), Sense::click_and_drag());
    // Only the left button drags songs, as in the lists.
    if let Some(uri) = uri
        && resp.drag_started_by(egui::PointerButton::Primary)
    {
        resp.dnd_set_drag_payload(super::dnd::SongsDrag { uris: vec![uri.to_owned()] });
    }
    // The hand while it can be clicked; while dragged, the drag's own cursor.
    if resp.dragged() { resp } else { resp.on_hover_cursor(egui::CursorIcon::PointingHand) }
}

impl TruffleApp {
    /// The player bar (`.pb`, 72 tall): the song on the left (its cover, or the little turntable while the
    /// side panel is closed), the controls and the seek line in the middle, the side panel button, volume
    /// and the rest on the right. Widths follow the prototype: 290 / 290, or 240 / 210 at medium widths.
    pub(super) fn player_bar(&mut self, ui: &mut Ui, l: &Layout) {
        let p = self.palette;
        let r = ui.max_rect();
        ui.painter().hline(r.x_range(), r.top() + 0.5, egui::Stroke::new(1.0, p.border));
        let medium = l.mode != Mode::Wide;
        let (np_w, rt_w) = if medium { (240.0, 210.0) } else { (290.0, 290.0) };
        let np = Rect::from_min_size(pos2(r.left() + 14.0, r.center().y - 30.0), vec2(np_w, 60.0));
        let rt = Rect::from_min_max(pos2(r.right() - 14.0 - rt_w, r.top()), pos2(r.right() - 14.0, r.bottom()));
        let mid = Rect::from_min_max(pos2(np.right() + 12.0, r.top()), pos2(rt.left() - 12.0, r.bottom()));
        self.bar_song(ui, np, l);
        self.bar_controls(ui, mid);
        self.bar_right(ui, rt, l, medium);
        let _ = p;
    }

    /// Left: the cover (48, round 5) or the docked turntable (50) while the side panel is closed, the
    /// title (12.5 semibold) and artists (11, muted), and the heart, 11 apart.
    fn bar_song(&mut self, ui: &mut Ui, np: Rect, l: &Layout) {
        let p = self.palette;
        let Some(track) = self.playback.track.clone() else {
            ui.painter().text(pos2(np.left(), np.center().y), Align2::LEFT_CENTER, "Nothing playing", regular(12.0), p.text_weak);
            return;
        };
        // Right-click the playing song: its menu, above the bar (with "Remove from this playlist"
        // when it plays from one of the user's playlists).
        let right_click = ui.input(|i| i.pointer.secondary_clicked().then(|| i.pointer.interact_pos()).flatten());
        if right_click.is_some_and(|pt| np.contains(pt)) {
            let from = self.playback.source.clone();
            self.open_song_menu(&track, pos2(np.left(), np.top() - 4.0), from);
        }
        let url = track.cover_large.clone().or_else(|| track.cover_small.clone());
        let cover_end = if l.panel_w == 0.0 {
            // `.dock`: 58 wide, the turntable 50 across, 1 px tucked in.
            let dock = Rect::from_min_size(pos2(np.left(), np.center().y - 25.0), vec2(58.0, 50.0));
            let texture = url.as_deref().and_then(|u| self.images.get(u, 36.0)).map(|t| t.id);
            let spin = self.deck_spin(ui);
            let arm = motion::tonearm(ui.ctx(), Id::new("dock-arm"), self.is_playing(), self.song_done());
            fin::turntable(ui.painter(), dock.min, 50.0, texture, spin, &arm);
            self.bar_picture(ui, dock, &track);
            dock.right() - 1.0
        } else {
            let cover = Rect::from_min_size(pos2(np.left(), np.center().y - 24.0), vec2(48.0, 48.0));
            if !self.images.paint(ui, url.as_deref(), cover, 5, p.placeholder) {
                svgicon::paint(ui.painter(), cover.shrink(14.0), "note", p.text_weak, svgicon::STROKE);
            }
            self.bar_picture(ui, cover, &track);
            cover.right()
        };
        let heart_w = if track.is_local { 0.0 } else { 30.0 + 11.0 };
        let text_left = cover_end + 11.0;
        let max_w = (np.right() - text_left - heart_w).max(30.0);
        let title = one_line(ui, &track.name, bold(12.5), p.text_strong, max_w, false);
        let artists = one_line(ui, &track.artist_line(), regular(11.0), p.text_weak, max_w, false);
        let text_w = title.size().x.max(artists.size().x);
        let h = title.size().y + 2.0 + artists.size().y;
        let y = np.center().y - h / 2.0;
        let title_rect = Rect::from_min_size(pos2(text_left, y), title.size());
        let title_resp = bar_title(ui, title_rect, (!track.is_local).then_some(track.uri.as_str()));
        if title_resp.dragged() {
            self.paint_songs_drag(ui, std::slice::from_ref(&track));
        }
        let title_color = p.text_strong;
        let th = title.size().y;
        ui.painter().galley(pos2(text_left, y), title, title_color);
        ui.painter().galley(pos2(text_left, y + th + 2.0), artists, p.text_weak);
        if title_resp.hovered() && !title_resp.dragged() {
            ui.painter().hline(title_rect.x_range(), title_rect.bottom() - 1.0, egui::Stroke::new(1.0, p.text_strong));
        }
        if title_resp.on_hover_text("Show playing song (Ctrl+J)").clicked() {
            self.locate_current_track();
        }
        // Each artist under the title opens their page.
        let mut ax = text_left;
        let comma = ui.painter().layout_no_wrap(", ".to_owned(), regular(11.0), p.text_weak).size().x;
        for (k, a) in track.artists.iter().enumerate() {
            let w = ui.painter().layout_no_wrap(a.name.clone(), regular(11.0), p.text_weak).size().x;
            let end = (ax + w).min(text_left + max_w);
            if end <= ax {
                break;
            }
            let r = Rect::from_min_max(pos2(ax, y + th + 2.0), pos2(end, y + h));
            if a.uri.starts_with("spotify:artist:") {
                let resp = ui.interact(r, Id::new(("bar-artist", k)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.hovered() {
                    ui.painter().hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(1.0, p.text_weak));
                }
                if resp.clicked() {
                    self.open_source(crate::model::Source::Artist(a.uri.clone()), false);
                }
            }
            ax += w + comma;
        }
        if !track.is_local {
            let heart = Rect::from_min_size(pos2(text_left + text_w + 11.0, np.center().y - 15.0), vec2(30.0, 30.0));
            self.heart_at(ui, heart, &track.uri);
        }
    }

    /// How far through the song playback is, 0-1.
    pub(super) fn song_done(&self) -> f32 {
        let duration = self.playback.track.as_ref().map_or(0, |t| t.duration_ms);
        if duration == 0 {
            return 0.0;
        }
        (self.playback.current_position_ms() as f32 / duration as f32).clamp(0.0, 1.0)
    }

    /// The heart (`.ib`, accent and filled when liked; it pops) at `rect`.
    pub(super) fn heart_at(&mut self, ui: &mut Ui, rect: Rect, uri: &str) {
        let p = self.palette;
        let liked = self.liked.contains(uri);
        let tip = if liked { "Remove from Liked Songs" } else { "Save to Liked Songs" };
        if fin::heart(ui, &p, rect, Id::new(("heart", uri)), liked, tip).clicked() {
            self.like_song(uri, !liked);
        }
    }

    /// Middle: shuffle, previous, play (36, 8 either side), next and repeat, 2 apart; 5 below them the
    /// seek line (at most 520 wide) with the times 10 either side.
    fn bar_controls(&mut self, ui: &mut Ui, mid: Rect) {
        let p = self.palette;
        let shuffle = self.current_shuffle();
        let repeat = if self.have_playback { self.playback.repeat } else { self.prefs.repeat };
        let has_track = self.playback.track.is_some();
        let playing = matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading);
        // Listening along, the host decides: the controls are dimmed.
        let locked = self.listening();
        let free = |look: Look| if locked { Look::Dim } else { look };

        let row_w = 30.0 * 4.0 + 2.0 * 4.0 + 36.0 + 16.0;
        let time_h = 14.0;
        let top = mid.center().y - (36.0 + 5.0 + time_h) / 2.0;
        let mut x = mid.center().x - row_w / 2.0;
        let cy = top + 18.0;
        let at = |x: f32, w: f32| Rect::from_min_size(pos2(x, cy - w / 2.0), vec2(w, w));

        let tip = if locked { "Listening along" } else if shuffle { "Shuffle is on" } else { "Shuffle is off" };
        if fin::ib(ui, &p, at(x, 30.0), Id::new("bar-shuffle"), "shuffle", free(Look::on(shuffle)), tip).clicked() {
            self.toggle_shuffle();
        }
        x += 32.0;
        let look = if has_track { free(Look::Plain) } else { Look::Dim };
        if fin::ib(ui, &p, at(x, 30.0), Id::new("bar-prev"), "prev", look, "Previous (Ctrl+←)").clicked() {
            self.send(Command::Previous);
        }
        x += 32.0 + 8.0;
        let tip = if locked { "Listening along" } else if playing { "Pause (Space)" } else { "Play (Space)" };
        if fin::pp(ui, &p, at(x, 36.0), Id::new("bar-play"), playing, has_track && !locked, tip).clicked() {
            self.send(Command::PlayPause);
        }
        x += 36.0 + 8.0 + 2.0;
        if fin::ib(ui, &p, at(x, 30.0), Id::new("bar-next"), "next", look, "Next (Ctrl+→)").clicked() {
            self.send(Command::Next);
        }
        x += 32.0;
        let (on, tip) = match repeat {
            RepeatMode::Off => (false, "Repeat is off"),
            RepeatMode::All => (true, "Repeating the whole list"),
            RepeatMode::One => (true, "Repeating this song"),
        };
        let rep = at(x, 30.0);
        if fin::ib(ui, &p, rep, Id::new("bar-repeat"), "repeat", free(Look::on(on)), tip).clicked() {
            self.cycle_repeat();
        }
        if repeat == RepeatMode::One && !locked {
            // `.r1`: a tiny accent "1" in the corner.
            ui.painter().text(pos2(rep.right() - 3.0, rep.top() + 3.0), Align2::RIGHT_TOP, "1", super::theme::heavy(8.0), p.accent_ink);
        }

        // The seek row.
        let sk_w = mid.width().min(520.0);
        let sk_y = top + 36.0 + 5.0 + time_h / 2.0;
        let left = mid.center().x - sk_w / 2.0;
        self.seek_strip(ui, left, left + sk_w, sk_y, Id::new("bar-seek"));
    }

    /// The seek line from `left` to `right` around `cy`, with the times (11, muted) 10 either side.
    /// While it is held only the preview follows the pointer; the one seek goes out on release.
    pub(super) fn seek_strip(&mut self, ui: &mut Ui, left: f32, right: f32, cy: f32, id: Id) {
        let p = self.palette;
        let has_track = self.playback.track.is_some();
        let duration = self.playback.track.as_ref().map_or(0, |t| t.duration_ms);
        let dur = duration.max(1) as f32;
        let live = self.playback.current_position_ms() as f32;
        let pos = self.seek_preview.unwrap_or(live).clamp(0.0, dur);
        let cur_text = if has_track { fmt_ms(pos as u32) } else { "-:--".to_owned() };
        let end_text = if has_track { fmt_ms(duration) } else { "-:--".to_owned() };
        let font = regular(11.0);
        let cur = ui.fonts_mut(|f| f.layout_no_wrap(cur_text, font.clone(), p.text_weak));
        let end = ui.fonts_mut(|f| f.layout_no_wrap(end_text, font, p.text_weak));
        // Both times sit in a slot as wide as the song's length written in the widest digit (Inter's
        // digits differ in width): the line stays put while the time beside it changes.
        let shape = if has_track { fmt_ms(duration) } else { "-:--".to_owned() };
        let slot = ('0'..='9')
            .map(|d| {
                let text: String = shape.chars().map(|c| if c.is_ascii_digit() { d } else { c }).collect();
                ui.fonts_mut(|f| f.layout_no_wrap(text, regular(11.0), p.text_weak)).size().x
            })
            .fold(0.0_f32, f32::max);
        ui.painter().galley(pos2(left, cy - cur.size().y / 2.0), cur, p.text_weak);
        ui.painter().galley(pos2(right - end.size().x, cy - end.size().y / 2.0), end, p.text_weak);
        let line = Rect::from_min_max(pos2(left + slot + 10.0, cy - 9.0), pos2(right - slot - 10.0, cy + 9.0));
        let (resp, held) = fin::line(ui, &p, line, id, pos / dur, has_track && duration > 0 && !self.listening());
        if let Some(v) = held {
            self.seek_preview = Some(v * dur);
        } else if let Some(target) = self.seek_preview.take() {
            let target = target.round() as u32;
            self.playback.position_ms = target;
            self.playback.position_at = Some(Instant::now());
            self.send(Command::SeekMs(target));
        }
        if resp.hovered() && has_track && let Some(at) = resp.hover_pos() {
            let t = ((at.x - line.left()) / line.width()).clamp(0.0, 1.0);
            resp.on_hover_text_at_pointer(fmt_ms((t * dur) as u32));
        }
    }

    /// Right, laid out from the right edge: settings, mini player and sleep (wide) or settings and
    /// "more" (medium), the volume line (80, or 54; 6 before it and 2 after), the mute button and the side
    /// panel button.
    fn bar_right(&mut self, ui: &mut Ui, rt: Rect, l: &Layout, medium: bool) {
        let p = self.palette;
        let cy = rt.center().y;
        let mut x = rt.right();
        // The next slot `w` wide, leftwards from `x`.
        let next = |x: &mut f32, w: f32| {
            *x -= w;
            Rect::from_min_size(pos2(*x, cy - 15.0), vec2(w, 30.0))
        };
        if fin::ib(ui, &p, next(&mut x, 30.0), Id::new("bar-gear"), "gear", Look::Plain, "Settings (Ctrl+,)").clicked() {
            self.open_settings();
        }
        if medium {
            let more = fin::ib(ui, &p, next(&mut x, 30.0), Id::new("bar-more"), "more", Look::Plain, "More");
            self.sleep_and_mini_menu(ui, &more, true);
        } else {
            let mini = next(&mut x, 30.0);
            if fin::ib(ui, &p, mini, Id::new("bar-mini"), "mini", Look::Plain, "Mini player (Ctrl+M)").clicked() {
                let ctx = ui.ctx().clone();
                self.toggle_mini(&ctx);
            }
            if fin::ib(ui, &p, next(&mut x, 30.0), Id::new("bar-stage"), "view", Look::Plain, "Now playing (Ctrl+I)").clicked() {
                let ctx = ui.ctx().clone();
                self.toggle_stage(&ctx);
            }
            let tip = match self.sleep_timer {
                Some(state) => format!("Sleep timer: stops in {}", sleep_timer_label(state, Instant::now())),
                None => "Sleep timer".to_owned(),
            };
            let sleep = fin::ib(ui, &p, next(&mut x, 30.0), Id::new("bar-sleep"), "sleep", Look::on(self.sleep_timer.is_some()), &tip);
            self.sleep_and_mini_menu(ui, &sleep, false);
        }
        x -= 6.0;
        let vol_w = if medium { 54.0 } else { 80.0 };
        let vol_rect = next(&mut x, vol_w);
        let vol_rect = Rect::from_min_size(pos2(vol_rect.left(), cy - 9.0), vec2(vol_w, 18.0));
        let (resp, held) = fin::line(ui, &p, vol_rect, Id::new("bar-volume"), self.volume_ui / 100.0, true);
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
                self.volume_dirty_at = Some(due_now());
            }
        }
        // The volume as a number over the knob, at once while the line is hovered or held (a tooltip
        // waits, and hides during a drag).
        if resp.hovered() || held.is_some() {
            volume_bubble(ui, &p, vol_rect, self.volume_ui);
        }
        x -= 2.0;
        let vol = self.volume_ui.round() as u8;
        let (icon, tip) = if vol == 0 { ("volx", "Unmute") } else { ("vol", "Mute") };
        let mute = next(&mut x, 30.0);
        // Right-click the mute button or the line: a box to type the volume in, just above them.
        let area = mute.union(vol_rect);
        let right_click = ui.input(|i| i.pointer.secondary_clicked().then(|| i.pointer.interact_pos()).flatten());
        if right_click.is_some_and(|pt| area.contains(pt)) {
            self.volume_box = Some(String::new());
        }
        self.volume_box_ui(ui, area);
        if fin::ib(ui, &p, mute, Id::new("bar-mute"), icon, Look::Plain, tip).clicked() {
            if vol > 0 {
                self.volume_before_mute = Some(vol);
                self.volume_ui = 0.0;
            } else {
                self.volume_ui = self.volume_before_mute.take().unwrap_or(50) as f32;
            }
            self.volume_dirty_at = Some(due_now());
        }
        let open = l.panel_w > 0.0;
        if fin::ib(ui, &p, next(&mut x, 30.0), Id::new("bar-panel"), "panel", Look::on_box(open), "Side panel (Ctrl+P)").clicked() {
            self.toggle_side_panel(l.w);
        }

        self.send_volume(ui.ctx(), resp.drag_stopped() || held.is_none());
    }

    /// Sends a volume change once it has waited long enough: at most one per VOLUME_DEBOUNCE while
    /// its line is dragged, at once when it is `let_go`.
    pub(super) fn send_volume(&mut self, ctx: &egui::Context, let_go: bool) {
        if let Some(at) = self.volume_dirty_at {
            let elapsed = at.elapsed();
            if elapsed >= VOLUME_DEBOUNCE || let_go {
                self.volume_dirty_at = None;
                let v = self.volume_ui.round().clamp(0.0, 100.0) as u8;
                self.playback.volume = v;
                self.send(Command::SetVolume(v));
            } else {
                ctx.request_repaint_after(VOLUME_DEBOUNCE - elapsed);
            }
        }
    }

    /// Shuffle on or off (the player bar's and the Now playing card's button).
    pub(super) fn toggle_shuffle(&mut self) {
        let on = !self.current_shuffle();
        self.playback.shuffle = on;
        self.prefs.shuffle = on;
        self.send(Command::SetShuffle(on));
    }

    /// Repeat: off, the whole list, this song, off again.
    pub(super) fn cycle_repeat(&mut self) {
        let repeat = if self.have_playback { self.playback.repeat } else { self.prefs.repeat };
        let next = repeat.cycle();
        self.playback.repeat = next;
        self.prefs.repeat = next;
        self.send(Command::SetRepeat(next));
    }

    /// The sleep timer menu on `resp` (with the mini player first when `with_mini`, for "more").
    /// The volume box, while one is open, over `area` (the mute button and the volume line).
    fn volume_box_ui(&mut self, ui: &Ui, area: Rect) {
        let Some(mut text) = self.volume_box.take() else { return };
        match volume_box(ui, &self.palette, area, &mut text, self.volume_ui.round() as u8) {
            VolumeBox::Open => self.volume_box = Some(text),
            VolumeBox::Set(v) => {
                self.volume_ui = f32::from(v);
                self.volume_before_mute = None;
                self.volume_dirty_at = Some(due_now());
            }
            VolumeBox::Closed => {}
        }
    }

    fn sleep_and_mini_menu(&mut self, ui: &mut Ui, resp: &egui::Response, with_mini: bool) {
        let active = self.sleep_timer;
        let has_track = self.playback.track.is_some();
        let mut choice: Option<Option<SleepTimer>> = None;
        let (mut mini, mut stage) = (false, false);
        Popup::menu(resp)
            .align(RectAlign::TOP_END)
            .align_alternatives(&[RectAlign::TOP_START, RectAlign::BOTTOM_END])
            .show(|ui| {
                ui.set_width(200.0);
                if with_mini {
                    if fin::menu_item(ui, Some("mini"), "Mini player", Some("Ctrl+M"), false, true).clicked() {
                        mini = true;
                        ui.close();
                    }
                    if fin::menu_item(ui, Some("view"), "Now playing", Some("Ctrl+I"), false, true).clicked() {
                        stage = true;
                        ui.close();
                    }
                    fin::menu_sep(ui);
                }
                fin::menu_label(ui, "Stop playing after");
                for minutes in SLEEP_MINUTES {
                    if fin::menu_item(ui, Some("sleep"), &format!("{minutes} minutes"), None, false, true).clicked() {
                        choice = Some(Some(SleepTimer::Minutes(minutes)));
                        ui.close();
                    }
                }
                let end = fin::menu_item(ui, Some("note"), "End of this song", None, false, has_track);
                let end = if has_track { end } else { end.on_hover_text("Play something first") };
                if end.clicked() {
                    choice = Some(Some(SleepTimer::EndOfTrack));
                    ui.close();
                }
                if active.is_some() {
                    fin::menu_sep(ui);
                    if fin::menu_item(ui, Some("x"), "Turn off", None, false, true).clicked() {
                        choice = Some(None);
                        ui.close();
                    }
                }
            });
        if mini {
            let ctx = ui.ctx().clone();
            self.toggle_mini(&ctx);
        }
        if stage {
            let ctx = ui.ctx().clone();
            self.toggle_stage(&ctx);
        }
        if let Some(timer) = choice {
            // Show the choice at once; the controller confirms it with Event::SleepTimer.
            self.sleep_timer = timer.map(|t| match t {
                SleepTimer::Minutes(m) => SleepTimerState::Until(Instant::now() + Duration::from_secs(u64::from(m) * 60)),
                SleepTimer::EndOfTrack => SleepTimerState::EndOfTrack,
            });
            self.send(Command::SetSleepTimer(timer));
        }
    }

    /// The playing song's picture on the left (its cover, or the docked turntable) at `r`: a click
    /// opens the song's album, at the song. One without an album shows where it plays from, as the
    /// title does.
    fn bar_picture(&mut self, ui: &mut Ui, r: Rect, track: &crate::model::TrackInfo) {
        let album = track.album_uri.starts_with("spotify:album:");
        let tip = if album { "Go to album" } else { "Show playing song (Ctrl+J)" };
        let resp = ui.interact(r, Id::new("bar-cover"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        if resp.on_hover_text(tip).clicked() {
            if album {
                self.show_track_in(crate::model::Source::Album(track.album_uri.clone()), track.uri.clone());
            } else {
                self.locate_current_track();
            }
        }
    }

    /// Opens the playing source and scrolls to the playing track.
    pub(super) fn locate_current_track(&mut self) {
        let Some(uri) = self.playback.track.as_ref().map(|t| t.uri.clone()) else { return };
        let Some(source) = self.playback.source.clone() else { return };
        self.show_track_in(source, uri);
    }

    /// Opens `source` and scrolls to the track `uri` in it.
    fn show_track_in(&mut self, source: crate::model::Source, uri: String) {
        self.middle_tab = super::MiddleTab::Songs;
        let showing = self.view.as_ref().is_some_and(|v| v.source == source && v.tracks.is_some());
        if showing {
            if let Some(view) = self.view.as_mut() {
                view.scroll_to_uri(&uri);
            }
        } else {
            self.pending_scroll_uri = Some(uri);
            self.open_source(source, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_volume_pill_sits_over_the_knob_inside_the_window() {
        let bar = Rect::from_min_size(pos2(500.0, 700.0), vec2(80.0, 18.0));
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 760.0));
        let size = vec2(40.0, 20.0);
        // Half volume: centred over the middle of the line, 4 above it.
        assert_eq!(bubble_rect(bar, 50.0, size, screen), Rect::from_min_size(pos2(520.0, 676.0), size));
        // Empty and full: over the ends.
        assert_eq!(bubble_rect(bar, 0.0, size, screen).center().x, 500.0);
        assert_eq!(bubble_rect(bar, 100.0, size, screen).center().x, 580.0);
        // At the window's edge it stays inside.
        let edge = Rect::from_min_size(pos2(940.0, 700.0), vec2(54.0, 18.0));
        assert_eq!(bubble_rect(edge, 100.0, size, screen).right(), 996.0);
    }

    #[test]
    fn sleep_timer_label_counts_down_in_whole_seconds() {
        let now = Instant::now();
        let at = |ms| SleepTimerState::Until(now + Duration::from_millis(ms));
        assert_eq!(sleep_timer_label(at(90_000), now), "1:30");
        assert_eq!(sleep_timer_label(at(89_100), now), "1:30");
        assert_eq!(sleep_timer_label(at(0), now), "0:00");
        assert_eq!(sleep_timer_label(at(90 * 60_000), now), "90:00");
        assert_eq!(sleep_timer_label(SleepTimerState::EndOfTrack, now), "song end");
    }

    use crate::config::ThemeMode;
    use crate::ui::kit::headless::{key, move_to, pass, press, release, text};
    use egui::Color32;

    /// The playing song's name, low on the 600 x 400 test screen as in the player bar.
    const TITLE: Rect = Rect { min: egui::Pos2 { x: 80.0, y: 352.0 }, max: egui::Pos2 { x: 180.0, y: 368.0 } };

    /// One pass of the name with `events`: whether it was clicked, and whether a song is dragged.
    fn title_pass(ctx: &egui::Context, events: Vec<egui::Event>, uri: Option<&str>) -> (bool, bool) {
        let mut clicked = false;
        pass(ctx, events, |ui| clicked = bar_title(ui, TITLE, uri).clicked());
        (clicked, egui::DragAndDrop::has_payload_of_type::<crate::ui::dnd::SongsDrag>(ctx))
    }

    #[test]
    fn a_click_on_the_playing_songs_name_drags_nothing_and_moving_it_does() {
        let ctx = egui::Context::default();
        let song = Some("spotify:track:a");
        let (at, away) = (TITLE.center(), TITLE.center() + vec2(40.0, -60.0));
        // Pressed and held still: nothing is dragged (no chip, no drag cursor).
        title_pass(&ctx, move_to(at), song);
        assert_eq!(title_pass(&ctx, press(at), song), (false, false));
        assert_eq!(title_pass(&ctx, Vec::new(), song), (false, false));
        // Let go: a click (it shows where the song plays from).
        assert_eq!(title_pass(&ctx, release(at), song), (true, false));
        // Pressed and moved: the song is dragged, until let go.
        title_pass(&ctx, press(at), song);
        assert_eq!(title_pass(&ctx, move_to(away), song), (false, true));
        assert_eq!(title_pass(&ctx, release(away), song), (false, false));
        // A local file can't go into a playlist: it isn't dragged.
        title_pass(&ctx, press(at), None);
        assert_eq!(title_pass(&ctx, move_to(away), None), (false, false));
        title_pass(&ctx, release(away), None);
    }

    /// The mute button and the volume line, low on the 600 x 400 test screen as in the player bar.
    const AREA: Rect = Rect { min: egui::Pos2 { x: 450.0, y: 370.0 }, max: egui::Pos2 { x: 560.0, y: 390.0 } };

    /// One pass of the volume box with `events`: what it did, `typed` so far, and where it sits.
    fn volume_pass(ctx: &egui::Context, events: Vec<egui::Event>, typed: &mut String) -> (VolumeBox, Option<Rect>) {
        let p = Palette::build(ThemeMode::Light, Color32::RED, None);
        let mut out = VolumeBox::Closed;
        pass(ctx, events, |ui| out = volume_box(ui, &p, AREA, typed, 40));
        (out, ctx.memory(|m| m.area_rect(Id::new("bar-volume-box"))))
    }

    #[test]
    fn the_volume_box_sits_right_over_the_volume_and_sets_what_is_typed() {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        let mut typed = String::new();
        let (did, _) = volume_pass(&ctx, Vec::new(), &mut typed);
        assert_eq!(did, VolumeBox::Open);
        let (_, at) = volume_pass(&ctx, Vec::new(), &mut typed);
        let at = at.expect("the box is shown");
        assert!(at.bottom() <= AREA.top() && at.bottom() >= AREA.top() - 20.0, "just above the volume, not in the middle: {at:?}");
        assert!(at.left() < AREA.right() && at.right() > AREA.left(), "over the volume line and button: {at:?}");
        // Letters are dropped; 3 digits at most; more than 100 is 100.
        volume_pass(&ctx, text("1x5"), &mut typed);
        assert_eq!(typed, "15");
        volume_pass(&ctx, text("09"), &mut typed);
        assert_eq!(typed, "150");
        assert_eq!(volume_pass(&ctx, key(egui::Key::Enter), &mut typed).0, VolumeBox::Set(100));
    }

    #[test]
    fn the_volume_box_closes_without_a_change_on_esc_a_click_elsewhere_or_an_empty_enter() {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        for close in [key(egui::Key::Escape), [press(egui::pos2(100.0, 100.0)), release(egui::pos2(100.0, 100.0))].concat(), key(egui::Key::Enter)] {
            let mut typed = String::new();
            volume_pass(&ctx, Vec::new(), &mut typed);
            volume_pass(&ctx, Vec::new(), &mut typed);
            if !matches!(close.first(), Some(egui::Event::Key { key: egui::Key::Enter, .. })) {
                volume_pass(&ctx, text("7"), &mut typed);
            }
            let (did, _) = volume_pass(&ctx, close.clone(), &mut typed);
            let (did, _) = if did == VolumeBox::Open { volume_pass(&ctx, Vec::new(), &mut typed) } else { (did, None) };
            assert_eq!(did, VolumeBox::Closed, "{close:?}");
            // The next one starts fresh.
            ctx.memory_mut(|m| m.data.clear());
        }
    }
}
