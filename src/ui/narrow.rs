//! The window under 760 wide (the Final design's narrow layout): the title bar, the page, the mini
//! player (`.mb`) and the bottom tabs (`.ntabs`: Home, Search, Library, Playing). Library lists the
//! playlists (with Albums and Artists chips), Playing (`pgPlaying`) is the record, the song, the
//! controls and the side panel's tabs, full width.

use std::time::Instant;

use egui::{Align2, CornerRadius, CursorIcon, Id, Rect, ScrollArea, Sense, Stroke, Ui, pos2, vec2};

use super::fin::{self, Look};
use super::frame::Layout;
use super::motion;
use super::panel::Tab;
use super::svgicon;
use super::theme::{bold, heavy, medium, regular};
use super::tracks::one_line;
use super::util::{fmt_ms, fmt_thousands, fmt_total};
use super::{MiddleTab, TruffleApp};
use crate::model::{Command, PlayStatus, RepeatMode};

/// The bottom tabs and the mini player above them.
const TABS_H: f32 = 58.0;
const MINI_H: f32 = 58.0;
const PAD_X: f32 = 16.0;

impl TruffleApp {
    /// Everything under the title bar at narrow widths, in `body` (down to the window's foot).
    pub(super) fn narrow_body(&mut self, ui: &mut Ui, body: Rect, l: &Layout) {
        let p = self.palette;
        let tabs = Rect::from_min_max(egui::pos2(body.left(), body.bottom() - TABS_H), body.max);
        let playing_page = self.middle_tab == MiddleTab::Playing;
        let mini = Rect::from_min_max(pos2(body.left(), tabs.top() - if playing_page { 0.0 } else { MINI_H }), pos2(body.right(), tabs.top()));
        let page = Rect::from_min_max(body.min, pos2(body.right(), mini.top()));
        super::frame::region(ui, page, "narrow-page", Some(p.main_bg), |ui| {
            self.wallpaper_behind_songs(ui);
            self.page(ui, p.main_bg, |app, ui| match app.middle_tab {
                MiddleTab::Playing => app.playing_page(ui),
                MiddleTab::Library => app.library_page(ui),
                _ => app.center_view(ui),
            });
        });
        if !playing_page {
            super::frame::region(ui, mini, "narrow-mini", Some(p.raised_bg), |ui| self.mini_bar(ui));
        }
        super::frame::region(ui, tabs, "narrow-tabs", Some(p.main_bg), |ui| self.bottom_tabs(ui));
        let _ = l;
    }

    /// Home, Search, Library and Playing (`.ntabs`): icon above name; the chosen one bright, with an
    /// accent bar on top.
    fn bottom_tabs(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let r = ui.max_rect();
        ui.painter().hline(r.x_range(), r.top() + 0.5, Stroke::new(1.0, p.border));
        let library = matches!(self.middle_tab, MiddleTab::Library | MiddleTab::Albums | MiddleTab::Artists | MiddleTab::Songs | MiddleTab::Blocked)
            && !self.home_shown();
        let tabs = [
            ("home", "Home", self.home_shown(), MiddleTab::Home),
            ("search", "Search", self.middle_tab == MiddleTab::Search, MiddleTab::Search),
            ("lib", "Library", library, MiddleTab::Library),
            ("disc", "Playing", self.middle_tab == MiddleTab::Playing, MiddleTab::Playing),
        ];
        let w = r.width() / tabs.len() as f32;
        // The bar slides to the tab picked.
        let chosen = tabs.iter().position(|t| t.2).map(|k| (k, Rect::from_center_size(pos2(r.left() + (k as f32 + 0.5) * w, r.top() + 1.5), vec2(40.0, 3.0))));
        motion::marker(ui.ctx(), Id::new("ntab-bar"), r.min, chosen, motion::TAB, |bar, a| {
            ui.painter().rect_filled(bar, CornerRadius { nw: 0, ne: 0, sw: 2, se: 2 }, p.accent_ink.gamma_multiply(a));
        });
        for (k, (icon, name, on, tab)) in tabs.into_iter().enumerate() {
            let cell = Rect::from_min_size(pos2(r.left() + k as f32 * w, r.top()), vec2(w, r.height()));
            let resp = ui.interact(cell, Id::new(("ntab", name)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let color = fin::tab_ink(&p, on, resp.hovered());
            let icon_c = motion::color(ui.ctx(), resp.id.with("icon"), if on { p.accent_ink } else { color }, motion::HOVER);
            let color = motion::color(ui.ctx(), resp.id, color, motion::HOVER);
            svgicon::paint(ui.painter(), Rect::from_center_size(pos2(cell.center().x, cell.top() + 21.0), vec2(16.0, 16.0)), icon, icon_c, svgicon::STROKE);
            ui.painter().text(pos2(cell.center().x, cell.top() + 41.0), Align2::CENTER_CENTER, name, if on { bold(11.0) } else { medium(11.0) }, color);
            if resp.clicked() {
                match tab {
                    MiddleTab::Home => self.show_home(),
                    MiddleTab::Search => self.middle_tab = MiddleTab::Search,
                    other => self.middle_tab = other,
                }
            }
        }
    }

    /// The mini player (`.mb`): how far the song is (a line on top), the record, the song, the heart,
    /// play / pause and next. A click on it opens Playing.
    fn mini_bar(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let r = ui.max_rect();
        let open = ui.interact(r, Id::new("mini-bar"), Sense::click());
        let done = self.song_done();
        ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(r.width(), 2.0)), CornerRadius::ZERO, p.border);
        ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(r.width() * done, 2.0)), CornerRadius::ZERO, p.accent_ink);
        let Some(track) = self.playback.track.clone() else {
            ui.painter().text(pos2(r.left() + PAD_X, r.center().y), Align2::LEFT_CENTER, "Nothing playing", regular(12.0), p.text_weak);
            return;
        };
        // `.mp`: 58 tall with a line on top (the progress bar over it), 10 in on the left and 8 on the
        // right, 10 apart; the turntable 40 across, the play button 34 (4 before it, 2 after).
        let cy = r.top() + 1.0 + 28.5;
        let dock = Rect::from_min_size(pos2(r.left() + 10.0, cy - 20.0), vec2(40.0 * 1.16, 40.0));
        let url = track.cover_large.clone().or_else(|| track.cover_small.clone());
        let texture = url.as_deref().and_then(|u| self.images.get(u, 36.0)).map(|t| t.id);
        let spin = self.deck_spin(ui);
        let arm = motion::tonearm(ui.ctx(), Id::new("mini-arm"), self.is_playing(), done);
        fin::turntable(ui.painter(), dock.min, 40.0, texture, spin, &arm);
        let next = Rect::from_center_size(pos2(r.right() - 8.0 - 15.0, cy), vec2(30.0, 30.0));
        let play = Rect::from_center_size(pos2(next.left() - 12.0 - 17.0, cy), vec2(34.0, 34.0));
        let heart = Rect::from_center_size(pos2(play.left() - 14.0 - 15.0, cy), vec2(30.0, 30.0));
        let x = dock.right() + 10.0;
        let tw = heart.left() - 10.0 - x;
        let t = one_line(ui, &track.name, bold(12.5), p.text_strong, tw, false);
        let s = one_line(ui, &track.artist_line(), regular(11.0), p.text_weak, tw, false);
        let h = t.size().y + 1.0 + s.size().y;
        let th = t.size().y;
        ui.painter().galley(pos2(x, cy - h / 2.0), t, p.text_strong);
        ui.painter().galley(pos2(x, cy - h / 2.0 + th + 1.0), s, p.text_weak);
        if !track.is_local {
            self.heart_at(ui, heart, &track.uri);
        }
        let playing = matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading);
        let locked = self.listening();
        if fin::pp(ui, &p, play, Id::new("mini-play"), playing, !locked, if playing { "Pause" } else { "Play" }).clicked() {
            self.send(Command::PlayPause);
        }
        if fin::ib(ui, &p, next, Id::new("mini-next"), "next", if locked { Look::Dim } else { Look::Plain }, "Next").clicked() {
            self.send(Command::Next);
        }
        let right_click = ui.input(|i| i.pointer.secondary_clicked().then(|| i.pointer.interact_pos()).flatten());
        if right_click.is_some_and(|pt| r.contains(pt)) {
            self.open_song_menu(&track, pos2(r.left() + 10.0, r.top() - 4.0), None);
        } else if open.clicked() {
            self.middle_tab = MiddleTab::Playing;
        }
    }

    /// Library (`pgLibrary`, narrow): the head with Playlists / Albums / Artists, then the playlists.
    pub(super) fn library_page(&mut self, ui: &mut Ui) {
        ScrollArea::vertical().id_salt("library-page").auto_shrink(false).show(ui, |ui| {
            let left = ui.max_rect().left() + PAD_X;
            let w = ui.max_rect().width() - PAD_X * 2.0;
            let top = ui.cursor().top();
            let y = self.library_head(ui, left, w, top + 16.0);
            ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left, top), pos2(left + w, y)));
            // The sidebar's lists, as wide as the page (4 sleeves across at 480).
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(pos2(left - 10.0, y), pos2(left + w + 10.0, y + 100_000.0))));
            child.spacing_mut().item_spacing = vec2(0.0, 0.0);
            self.sidebar_lists(&mut child);
            let used = child.min_rect();
            ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left, y), pos2(left + w, used.bottom() + 24.0)));
        });
    }

    /// "Library" (20 bold) with a + for a new playlist, then the Playlists / Albums / Artists chips.
    /// Returns where the page goes on.
    pub(super) fn library_head(&mut self, ui: &mut Ui, left: f32, w: f32, top: f32) -> f32 {
        let p = self.palette;
        let g = ui.painter().layout_no_wrap("Library".to_owned(), heavy(20.0), p.text_strong);
        ui.painter().galley(pos2(left, top + 15.0 - g.size().y / 2.0), g, p.text_strong);
        let plus = Rect::from_min_size(pos2(left + w - 30.0, top), vec2(30.0, 30.0));
        if fin::ib(ui, &p, plus, Id::new("lib-new"), "plus", Look::Plain, "New playlist").clicked() {
            self.new_playlist = Some(String::new());
        }
        let y = top + 30.0 + 12.0;
        let mut x = left;
        for (tab, name) in [(MiddleTab::Library, "Playlists"), (MiddleTab::Albums, "Albums"), (MiddleTab::Artists, "Artists")] {
            let mut resp = fin::chip(ui, &p, pos2(x, y + 13.0), Id::new(("lib-chip", name)), name, None, self.middle_tab == tab, false);
            if tab != MiddleTab::Library {
                resp = resp.on_hover_text(super::collection::FROM_LIKES);
            }
            if resp.clicked() {
                match tab {
                    MiddleTab::Library => self.middle_tab = MiddleTab::Library,
                    other => self.show_collection(other),
                }
            }
            x = resp.rect.right() + 8.0;
        }
        y + 26.0 + 12.0
    }

    /// Playing (`pgPlaying`): where it plays from, the record, the song, the line and the controls,
    /// then Up next, Lyrics and Rules.
    fn playing_page(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let r = ui.max_rect();
        let left = r.left() + PAD_X;
        let right = r.right() - PAD_X;
        // The head: back, PLAYING FROM and the list, the song's place on the right.
        let head = Rect::from_min_size(r.min, vec2(r.width(), 50.0));
        let down = Rect::from_min_size(pos2(left - 6.0, head.center().y - 15.0), vec2(30.0, 30.0));
        if fin::ib(ui, &p, down, Id::new("playing-down"), "chev", Look::Plain, "Back").clicked() {
            self.go_back();
        }
        let from = self.playback.source.as_ref().map(|s| self.source_title(s)).unwrap_or_default();
        let kicker = fin::spaced(ui, "PLAYING FROM", bold(10.0), p.text_weak, 1.0);
        let kh = kicker.size().y;
        ui.painter().galley(pos2(down.right() + 8.0, head.center().y - kh - 1.0), kicker, p.text_weak);
        let place = self.deck.as_ref().filter(|d| !d.order.is_empty()).map(|d| format!("Song {} of {}", fmt_thousands((d.now + 1).min(d.order.len())), fmt_thousands(d.order.len())));
        let more = Rect::from_min_size(pos2(right - 30.0 + 6.0, head.center().y - 15.0), vec2(30.0, 30.0));
        let more_resp = fin::ib(ui, &p, more, Id::new("playing-more"), "more", Look::Plain, "More");
        if more_resp.clicked()
            && let Some(t) = self.playback.track.clone()
        {
            self.open_song_menu(&t, pos2(more.right() - 240.0, more.bottom() + 4.0), None);
        }
        let mut place_left = more.left() - 6.0;
        if let Some(place) = place {
            let g = ui.painter().layout_no_wrap(place, regular(11.5), p.text_weak);
            place_left = more.left() - 6.0 - g.size().x;
            ui.painter().galley(pos2(place_left, head.center().y - g.size().y / 2.0), g, p.text_weak);
        }
        let name = one_line(ui, &from, bold(13.0), p.text_strong, place_left - 10.0 - (down.right() + 8.0), false);
        ui.painter().galley(pos2(down.right() + 8.0, head.center().y + 1.0), name, p.text_strong);

        // The record (as on the side panel), as tall as fits.
        let d = ((right - left) / 1.6).clamp(120.0, 200.0);
        let plinth = Rect::from_min_size(pos2(left, head.bottom()), vec2(right - left, (d * 1.2f32).round()));
        self.panel_plinth(ui, plinth, d);

        // The song, its heart, the line and the controls.
        let mut y = plinth.bottom() + 18.0;
        if let Some(track) = self.playback.track.clone() {
            let heart = Rect::from_min_size(pos2(right - 30.0 + 6.0, y + 6.0), vec2(30.0, 30.0));
            let tw = heart.left() - 8.0 - left;
            let t = one_line(ui, &track.name, heavy(19.0), p.text_strong, tw, false);
            let s = one_line(ui, &format!("{} · {}", track.artist_line(), track.album), regular(12.0), p.text_weak, tw, false);
            let th = t.size().y;
            ui.painter().galley(pos2(left, y), t, p.text_strong);
            ui.painter().galley(pos2(left, y + th + 2.0), s, p.text_weak);
            if !track.is_local {
                self.heart_at(ui, heart, &track.uri);
            }
            y += th + 2.0 + 16.0 + 14.0;
            y = self.playing_seek(ui, left, right, y, track.duration_ms);
        } else {
            ui.painter().text(pos2(left, y + 10.0), Align2::LEFT_CENTER, "Nothing playing", regular(13.0), p.text_weak);
            y += 40.0;
        }
        y = self.playing_controls(ui, r.center().x, y + 8.0);

        // The side panel's tabs, full width, and their page.
        let tabs = Rect::from_min_size(pos2(r.left(), y + 10.0), vec2(r.width(), 38.0));
        self.panel_tabs(ui, tabs);
        // (The Friends tab has its layout button there.)
        if self.frame.panel_tab != Tab::Friends
            && let Some(d) = self.deck.as_ref().filter(|d| !d.order.is_empty())
        {
            let to_go = d.order.len().saturating_sub((d.now + 1).min(d.order.len()));
            ui.painter().text(pos2(right, tabs.center().y), Align2::RIGHT_CENTER, format!("{} to go", fmt_thousands(to_go)), regular(11.0), p.text_weak);
        }
        let body = Rect::from_min_max(pos2(r.left(), tabs.bottom()), r.max);
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("playing-body").max_rect(body));
        child.set_clip_rect(body.intersect(ui.clip_rect()));
        match self.frame.panel_tab {
            Tab::Lyrics => self.panel_lyrics(&mut child, body),
            Tab::UpNext => self.panel_up_next(&mut child, body),
            Tab::Rules => self.panel_rules(&mut child, body, true),
            Tab::Friends => self.panel_friends(&mut child, body),
        }
    }

    /// A list's name, as far as it is known.
    fn source_title(&self, s: &crate::model::Source) -> String {
        use crate::model::Source;
        match s {
            Source::LikedSongs => "Liked Songs".to_owned(),
            Source::Playlist(uri) => self.playlist(uri).map(|pl| pl.name.clone()).unwrap_or_default(),
            Source::Album(_) | Source::Artist(_) => self.source_info.get(s).map(|i| i.title.clone()).unwrap_or_default(),
            Source::Mix(id) => self.mixes.iter().find(|m| m.id == *id).map(|m| m.name.clone()).unwrap_or_default(),
        }
    }

    /// The time, the line and the length. Returns the bottom.
    fn playing_seek(&mut self, ui: &mut Ui, left: f32, right: f32, y: f32, duration: u32) -> f32 {
        let p = self.palette;
        let dur = duration.max(1) as f32;
        let live = self.playback.current_position_ms() as f32;
        let pos = self.seek_preview.unwrap_or(live).clamp(0.0, dur);
        let cy = y + 8.0;
        let cur = ui.painter().layout_no_wrap(fmt_ms(pos as u32), regular(11.0), p.accent_ink);
        let end = ui.painter().layout_no_wrap(fmt_ms(duration), regular(11.0), p.text_weak);
        let (cw, ew) = (cur.size().x, end.size().x);
        ui.painter().galley(pos2(left, cy - cur.size().y / 2.0), cur, p.accent_ink);
        ui.painter().galley(pos2(right - ew, cy - end.size().y / 2.0), end, p.text_weak);
        let line = Rect::from_min_max(pos2(left + cw + 10.0, cy - 9.0), pos2(right - ew - 10.0, cy + 9.0));
        let (_, held) = fin::line(ui, &p, line, Id::new("playing-seek"), pos / dur, duration > 0 && !self.listening());
        if let Some(v) = held {
            self.seek_preview = Some(v * dur);
        } else if let Some(target) = self.seek_preview.take() {
            let target = target.round() as u32;
            self.playback.position_ms = target;
            self.playback.position_at = Some(Instant::now());
            self.send(Command::SeekMs(target));
        }
        cy + 9.0
    }

    /// Shuffle, previous, the big play button (52), next and repeat, centred on `cx`. Returns the
    /// bottom.
    fn playing_controls(&mut self, ui: &mut Ui, cx: f32, top: f32) -> f32 {
        let p = self.palette;
        let shuffle = self.current_shuffle();
        let repeat = if self.have_playback { self.playback.repeat } else { self.prefs.repeat };
        let has = self.playback.track.is_some();
        let playing = matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading);
        let cy = top + 26.0;
        let at = |x: f32, s: f32| Rect::from_center_size(pos2(x, cy), vec2(s, s));
        // Listening along, the host decides.
        let locked = self.listening();
        let free = |look: Look| if locked { Look::Dim } else { look };
        if fin::ib(ui, &p, at(cx - 106.0, 30.0), Id::new("playing-shuffle"), "shuffle", free(Look::on(shuffle)), "Shuffle").clicked() {
            self.playback.shuffle = !shuffle;
            self.prefs.shuffle = !shuffle;
            self.send(Command::SetShuffle(!shuffle));
        }
        let look = if has { free(Look::Plain) } else { Look::Dim };
        if fin::ib(ui, &p, at(cx - 66.0, 30.0), Id::new("playing-prev"), "prev", look, "Previous").clicked() {
            self.send(Command::Previous);
        }
        if fin::pp(ui, &p, at(cx, 52.0), Id::new("playing-play"), playing, has && !locked, if playing { "Pause" } else { "Play" }).clicked() {
            self.send(Command::PlayPause);
        }
        if fin::ib(ui, &p, at(cx + 66.0, 30.0), Id::new("playing-next"), "next", look, "Next").clicked() {
            self.send(Command::Next);
        }
        let on = repeat != RepeatMode::Off;
        if fin::ib(ui, &p, at(cx + 106.0, 30.0), Id::new("playing-repeat"), "repeat", free(Look::on(on)), "Repeat").clicked() {
            let next = repeat.cycle();
            self.playback.repeat = next;
            self.prefs.repeat = next;
            self.send(Command::SetRepeat(next));
        }
        let _ = fmt_total;
        top + 52.0
    }
}
