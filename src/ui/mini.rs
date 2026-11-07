//! The mini player: a small borderless window with the little turntable, the title and artist,
//! previous / play / next and the heart, and the seek line. Drag it anywhere to move it.

use egui::{Align2, CornerRadius, Frame, Id, PointerButton, Rect, Sense, Stroke, StrokeKind, Ui, ViewportCommand, pos2, vec2};

use super::fin::{self, Look};
use super::theme::{bold, regular};
use super::tracks::one_line;
use super::TruffleApp;
use crate::model::{Command, PlayStatus};

/// The record's size on the mini player's turntable.
const DECK: f32 = 62.0;

impl TruffleApp {
    pub(super) fn mini_player(&mut self, ui: &mut Ui) {
        let p = self.palette;
        egui::CentralPanel::default().frame(Frame::new().fill(p.raised_bg)).show(ui, |ui| self.mini_contents(ui));
    }

    fn mini_contents(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let r = ui.max_rect();
        let ctx = ui.ctx().clone();

        // The background moves the window (there is no title bar); double-click goes back to the
        // full window. Everything made after it keeps its own clicks.
        let background = ui.interact(r, Id::new("mini_background"), Sense::click_and_drag());
        if background.drag_started_by(PointerButton::Primary) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        let mut back_to_full = background.double_clicked();

        // The little turntable, 12 in from the top left.
        let track = self.playback.track.clone();
        let deck = Rect::from_min_size(pos2(r.left() + 12.0, r.top() + 12.0), vec2(DECK * 1.16, DECK));
        let url = track.as_ref().and_then(|t| t.cover_large.clone().or_else(|| t.cover_small.clone()));
        let texture = url.as_deref().and_then(|u| self.images.get(u, 48.0)).map(|t| t.id);
        let spin = self.deck_spin(ui);
        let arm = super::motion::tonearm(ui.ctx(), Id::new("mini-window-arm"), self.is_playing(), self.song_done());
        fin::turntable(ui.painter(), deck.min, DECK, texture, spin, &arm);

        // Top right, from the edge: back to the full window, a warning while there are error notes
        // (they show only in the full window), keep on top.
        let button_at = |right: f32| Rect::from_min_size(pos2(right - 28.0, r.top() + 8.0), vec2(28.0, 28.0));
        let full = button_at(r.right() - 8.0);
        if fin::ib(ui, &p, full, Id::new("mini-full"), "mini", Look::Plain, "Back to the full window (Ctrl+M)").clicked() {
            back_to_full = true;
        }
        let mut right = full.left() - 2.0;
        if let Some(error) = self.toasts.iter().rev().find(|t| t.error).map(|t| t.text.clone()) {
            let warn = button_at(right);
            let tip = format!("{error}\n\nClick to open the full window.");
            if fin::ib(ui, &p, warn, Id::new("mini-warn"), "info", Look::On, &tip).clicked() {
                back_to_full = true;
            }
            right = warn.left() - 2.0;
        }
        let on_top = self.prefs.mini_player_on_top;
        let pin = button_at(right);
        let tip = if on_top { "Stays on top (click to turn off)" } else { "Keep on top" };
        if fin::ib(ui, &p, pin, Id::new("mini-pin"), "pin", Look::on(on_top), tip).clicked() {
            self.set_mini_on_top(&ctx, !on_top);
        }

        // The title (13 semibold, white) and artist (11, muted).
        let x = deck.right() + 10.0;
        let text_w = (pin.left() - 6.0 - x).max(20.0);
        match &track {
            Some(t) => {
                let title = one_line(ui, &t.name, bold(13.0), p.text_strong, text_w, false);
                let artist = one_line(ui, &t.artist_line(), regular(11.0), p.text_weak, text_w, false);
                let th = title.size().y;
                let elided = title.elided || artist.elided;
                ui.painter().galley(pos2(x, r.top() + 13.0), title, p.text_strong);
                ui.painter().galley(pos2(x, r.top() + 15.0 + th), artist, p.text_weak);
                if elided {
                    // Hover only: dragging the words still moves the window.
                    let hover = Rect::from_min_size(pos2(x, r.top() + 13.0), vec2(text_w, th * 2.0));
                    ui.interact(hover, Id::new("mini-title"), Sense::hover()).on_hover_text(format!("{} — {}", t.name, t.artist_line()));
                }
            }
            None => {
                ui.painter().text(pos2(x, r.top() + 14.0), Align2::LEFT_TOP, "Nothing playing", regular(12.0), p.text_weak);
            }
        }

        // Previous, play (32), next and the heart, along the turntable's foot.
        let has_track = track.is_some();
        let cy = deck.bottom() - 12.0;
        let at = |x: f32, w: f32| Rect::from_min_size(pos2(x, cy - w / 2.0), vec2(w, w));
        // Listening along, the host decides.
        let locked = self.listening();
        let look = if has_track && !locked { Look::Plain } else { Look::Dim };
        let mut cx = x - 6.0;
        if fin::ib(ui, &p, at(cx, 28.0), Id::new("mini-prev"), "prev", look, "Previous (Ctrl+←)").clicked() {
            self.send(Command::Previous);
        }
        cx += 28.0 + 6.0;
        let playing = matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading);
        let tip = if playing { "Pause (Space)" } else { "Play (Space)" };
        if fin::pp(ui, &p, at(cx, 32.0), Id::new("mini-play"), playing, has_track && !locked, tip).clicked() {
            self.send(Command::PlayPause);
        }
        cx += 32.0 + 6.0;
        if fin::ib(ui, &p, at(cx, 28.0), Id::new("mini-next"), "next", look, "Next (Ctrl+→)").clicked() {
            self.send(Command::Next);
        }
        cx += 28.0 + 8.0;
        if let Some(t) = track.as_ref().filter(|t| !t.is_local) {
            self.heart_at(ui, at(cx, 28.0), &t.uri);
        }

        // The seek line with the times, along the bottom.
        self.seek_strip(ui, r.left() + 14.0, r.right() - 14.0, r.bottom() - 17.0, Id::new("mini-seek"));

        // Its edge: the line, or the accent while the window has focus (so it is easy to spot).
        let focused = ui.input(|i| i.viewport().focused == Some(true));
        let edge = if focused { fin::acc(&p, 0.5) } else { p.border };
        ui.painter().rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, edge), StrokeKind::Inside);

        if back_to_full {
            self.toggle_mini(&ctx);
        }
    }
}
