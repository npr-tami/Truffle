//! "Wrong song?": with YouTube sound, the videos that may be a song, to pick the right one. The
//! pick is remembered for that song and, if it is playing, it goes on from the same point.

use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Rect, Sense, Ui, pos2, vec2};

use super::TruffleApp;
use super::fin;
use super::svgicon;
use super::theme::{medium, regular};
use super::tracks::one_line;
use super::util::fmt_ms;
use crate::model::{Command, TrackInfo, YtMatch};

/// The open picker.
pub(super) struct YtPick {
    track: TrackInfo,
    /// The video the song plays now.
    current: Option<String>,
    /// The videos (or why they couldn't be found); `None` while searching.
    result: Option<Result<Vec<YtMatch>, String>>,
}

const ROW_H: f32 = 44.0;

impl TruffleApp {
    pub(super) fn open_yt_pick(&mut self, track: &TrackInfo) {
        self.send(Command::YouTubeMatches(Box::new(track.clone())));
        self.yt_pick = Some(YtPick { track: track.clone(), current: None, result: None });
    }

    pub(super) fn on_yt_matches(&mut self, uri: &str, current: Option<String>, result: Result<Vec<YtMatch>, String>) {
        if let Some(pick) = self.yt_pick.as_mut().filter(|p| p.track.uri == uri) {
            pick.current = current;
            pick.result = Some(result);
        }
    }

    pub(super) fn yt_pick_modal(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let Some(pick) = self.yt_pick.as_ref() else { return };
        let mut chosen: Option<String> = None;
        let mut close = false;
        let modal = fin::dialog(ctx, Id::new("yt_pick"), |ui| {
            ui.set_width(480.0);
            let who = format!("{} - {}", pick.track.artist_line(), pick.track.name);
            fin::dialog_head(ui, "Pick the right video", Some(&format!("For \"{who}\" ({}). Truffle remembers your pick.", fmt_ms(pick.track.duration_ms))));
            ui.add_space(10.0);
            match &pick.result {
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0).color(p.accent_ink));
                        ui.label(egui::RichText::new("Looking on YouTube…").font(regular(12.0)).color(p.text_weak));
                    });
                }
                Some(Err(e)) => {
                    ui.label(egui::RichText::new(e).font(regular(12.0)).color(p.error));
                }
                Some(Ok(list)) if list.is_empty() => {
                    ui.label(egui::RichText::new("Nothing found on YouTube.").font(regular(12.0)).color(p.text_weak));
                }
                Some(Ok(list)) => {
                    egui::ScrollArea::vertical().max_height(ROW_H * 7.0).show(ui, |ui| {
                        for m in list {
                            if video_row(ui, m, pick.current.as_deref() == Some(m.video_id.as_str()), pick.track.duration_ms, p.accent_ink) {
                                chosen = Some(m.video_id.clone());
                            }
                        }
                    });
                }
            }
            ui.add_space(12.0);
            let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            let w = fin::text_button_w(ui, "Close", false);
            close |= fin::text_button(ui, &p, pos2(row.right() - w, row.top()), Id::new("yt_pick_close"), "Close", None, false, true).clicked();
        });
        if let Some(video_id) = chosen {
            if let Some(pick) = self.yt_pick.take() {
                self.send(Command::ChooseYouTube { track: Box::new(pick.track), video_id });
            }
        } else if close || modal.should_close() {
            self.yt_pick = None;
        }
    }
}

/// One video: its title, channel and length (how far off Spotify's), a check on the one that plays
/// now. Returns true when clicked.
fn video_row(ui: &mut Ui, m: &YtMatch, current: bool, spotify_ms: u32, accent: Color32) -> bool {
    let p = crate::ui::theme::palette(ui.ctx());
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::click());
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), p.widget_hover_bg);
    }
    let right = if current {
        svgicon::paint(painter, Rect::from_center_size(pos2(rect.right() - 18.0, rect.center().y), vec2(14.0, 14.0)), "check", accent, svgicon::STROKE);
        rect.right() - 36.0
    } else {
        rect.right() - 10.0
    };
    let text_w = (right - rect.left() - 20.0 - if current { 50.0 } else { 0.0 }).max(40.0);
    let title = one_line(ui, &m.title, medium(12.5), if current { p.text_strong } else { p.text }, text_w, false);
    let off = (i64::from(m.duration_ms) - i64::from(spotify_ms)) / 1000;
    let off = match off {
        0 => "same length".to_owned(),
        n if n > 0 => format!("{n} s longer"),
        n => format!("{} s shorter", -n),
    };
    let sub = format!("{} · {} · {off}", if m.channel.is_empty() { "YouTube" } else { m.channel.as_str() }, fmt_ms(m.duration_ms));
    let sub = one_line(ui, &sub, regular(11.0), p.text_weak, text_w, false);
    let h = title.size().y + 2.0 + sub.size().y;
    let top = rect.center().y - h / 2.0;
    let th = title.size().y;
    ui.painter().galley(pos2(rect.left() + 10.0, top), title, p.text);
    ui.painter().galley(pos2(rect.left() + 10.0, top + th + 2.0), sub, p.text_weak);
    if current {
        ui.painter().text(pos2(right, rect.center().y), Align2::RIGHT_CENTER, "In use", regular(10.5), p.text_weak);
    }
    resp.clicked() && !current
}
