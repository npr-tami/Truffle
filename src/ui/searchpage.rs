//! The Search page (`pgSearch`): results show as you type in the title bar's box, under chips (All,
//! Songs, Albums, Artists, Playlists, Users). All: the top result beside the first songs, then
//! artists and playlists, then albums. Users: the people Spotify sends for what was typed (and for
//! it in plain letters), asked for once the chip shows. With nothing typed: recent searches and
//! moods. Enter plays the top result.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Rect, ScrollArea, Sense, Stroke, StrokeKind, TextFormat, Ui, pos2, vec2};

use super::fin;
use super::search::source_of;
use super::sidebar::OnTop;
use super::svgicon;
use super::theme::{Palette, bold, heavy, medium, regular};
use super::tracks::one_line;
use super::util::{fmt_ms, spotify_web_link};
use super::wide;
use super::{MiddleTab, TruffleApp};
use crate::model::{Card, Command, SearchResults, Source, TrackInfo};
use crate::text::{find_folded, find_folded_name, fold, fold_name};

/// Typing pauses this long before the search goes out.
pub(super) const SEARCH_SETTLE: Duration = Duration::from_millis(350);
/// Recent searches kept.
const RECENT_MAX: usize = 8;
const MOODS: [&str; 5] = ["Calm", "Loud", "Night", "Focus", "Upbeat"];
const CHIPS: [&str; 6] = ["All", "Songs", "Albums", "Artists", "Playlists", "Users"];
/// The Users chip: people only.
const USERS: u8 = 5;
/// `.pg` with 16 above.
const PAD_TOP: f32 = 16.0;
const PAD_X: f32 = 24.0;
/// `.hdr`: 24 tall, 10 under it.
const HDR_H: f32 = 24.0;
const HDR_GAP: f32 = 10.0;
/// A song row (`.qr`): 40 tall, a 30 cover.
const QR_H: f32 = 40.0;

/// The best match of a search: what Enter plays.
#[derive(Clone)]
pub(super) enum Top {
    Song(TrackInfo),
    /// An artist, album or playlist card, and what it is.
    Card(Card, &'static str),
}

/// How well `name` matches `q` (folded, see `fold`; the design's `scoreOf`): whole, start, a word's
/// start, inside.
fn score(name: &str, q: &str) -> u32 {
    let n = fold(name);
    if q.is_empty() {
        return 0;
    }
    if n == q {
        return 100;
    }
    if n.starts_with(q) {
        return 90;
    }
    match n.find(q) {
        Some(i) if n[..i].ends_with(' ') => 80,
        Some(_) => 70,
        None => 0,
    }
}

/// The top result: the best-matching first artist, album, song or playlist (artists win ties).
pub(super) fn top_result(r: &SearchResults, tracks: &[TrackInfo]) -> Option<Top> {
    let q = fold(r.query.trim());
    let mut best: Option<(f32, Top)> = None;
    let mut consider = |s: f32, t: Top| {
        if best.as_ref().is_none_or(|(b, _)| s > *b) {
            best = Some((s, t));
        }
    };
    if let Some(a) = r.artists.first() {
        consider(score(&a.name, &q) as f32 + 0.3, Top::Card(a.clone(), "Artist"));
    }
    if let Some(a) = r.albums.first() {
        consider(score(&a.name, &q) as f32 + 0.2, Top::Card(a.clone(), "Album"));
    }
    if let Some(t) = tracks.first() {
        consider(score(&t.name, &q) as f32 + 0.1, Top::Song(t.clone()));
    }
    if let Some(p) = r.playlists.first() {
        consider(score(&p.name, &q) as f32, Top::Card(p.clone(), "Playlist"));
    }
    best.map(|(_, t)| t)
}

/// `text` on one line with the part matching `q` (folded) in the accent (`hl`), cut to `width`.
fn highlighted(ui: &Ui, text: &str, q: &str, font: egui::FontId, color: Color32, accent: Color32, width: f32) -> Arc<egui::Galley> {
    highlighted_at(ui, text, find_folded(text, q), font, color, accent, width)
}

/// [`highlighted`] with the match found already.
fn highlighted_at(ui: &Ui, text: &str, found: Option<Range<usize>>, font: egui::FontId, color: Color32, accent: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::default();
    let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
    match found {
        Some(m) => {
            job.append(&text[..m.start], 0.0, fmt(color));
            job.append(&text[m.clone()], 0.0, fmt(accent));
            job.append(&text[m.end..], 0.0, fmt(color));
        }
        None => job.append(text, 0.0, fmt(color)),
    }
    job.wrap = TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// The chips from `top`, on more lines when they don't fit in `w` (a narrow window): where each
/// went, and the one clicked.
fn chips(ui: &Ui, p: &Palette, left: f32, w: f32, top: f32, chosen: u8) -> (Vec<Rect>, Option<u8>) {
    let (mut x, mut y) = (left, top);
    let mut placed = Vec::with_capacity(CHIPS.len());
    let mut clicked = None;
    for (k, name) in CHIPS.iter().enumerate() {
        if x > left && x + fin::chip_w(ui, name, false) > left + w {
            x = left;
            y += 26.0 + 8.0;
        }
        let resp = fin::chip(ui, p, pos2(x, y + 13.0), Id::new(("search-chip", k)), name, None, chosen == k as u8, false);
        if resp.clicked() {
            clicked = Some(k as u8);
        }
        x = resp.rect.right() + 8.0;
        placed.push(resp.rect);
    }
    (placed, clicked)
}

/// The letters on a person's round picture when there is none: the first letter of each of their
/// first two words, styled letters as plain ones ("𝓔𝓶𝓶𝓪 𝓢" is ES); else the first character of a
/// name without letters (an emoji), and nothing for no name.
fn initials(name: &str) -> String {
    let first = |w: &str| fold(w).chars().find(|c| c.is_alphanumeric());
    let letters: String = name.split_whitespace().filter_map(first).take(2).flat_map(char::to_uppercase).collect();
    if letters.is_empty() { name.chars().find(|c| !c.is_whitespace()).map(String::from).unwrap_or_default() } else { letters }
}

/// A person's id, when it isn't just their name again (case aside): shown under it.
fn shown_id(u: &Card) -> Option<&str> {
    let id = u.subtitle.as_str();
    (!id.is_empty() && id.to_lowercase() != u.name.to_lowercase()).then_some(id)
}

/// A person (`.qr`): their round picture (`picture` paints it, else their initials), their name
/// with the match (`q`, folded as names are) in the accent, and their id in grey when it isn't just
/// their name again. A click opens their profile; a right-click offers what people rows do: Open
/// profile and Copy link.
fn user_row(ui: &mut Ui, p: &Palette, rect: Rect, u: &Card, q: &str, picture: &mut dyn FnMut(&Ui, Rect) -> bool) -> Option<Act> {
    let resp = ui.interact(rect, Id::new(("search-user", &u.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover_bg);
    }
    let pic = Rect::from_min_size(pos2(rect.left() + 6.0, rect.center().y - 15.0), vec2(30.0, 30.0));
    if !picture(ui, pic) {
        ui.painter().circle_filled(pic.center(), 15.0, p.widget_bg);
        ui.painter().text(pic.center(), Align2::CENTER_CENTER, initials(&u.name), bold(11.0), p.text_weak);
    }
    let x = pic.right() + 10.0;
    let tw = rect.right() - 8.0 - x;
    let name = highlighted_at(ui, &u.name, find_folded_name(&u.name, q), medium(12.0), p.text, p.accent_ink, tw);
    let sub = shown_id(u).map(|id| one_line(ui, id, regular(11.0), p.text_weak, tw, false));
    let nh = name.size().y;
    let h = nh + sub.as_ref().map_or(0.0, |s| 1.0 + s.size().y);
    let ty = rect.center().y - h / 2.0;
    ui.painter().galley(pos2(x, ty), name, p.text);
    if let Some(sub) = sub {
        ui.painter().galley(pos2(x, ty + nh + 1.0), sub, p.text_weak);
    }
    let user = u.uri.strip_prefix("spotify:user:").unwrap_or_default();
    let mut act = resp.clicked().then(|| Act::User(user.to_owned()));
    resp.context_menu(|ui| {
        ui.set_width(200.0);
        if fin::menu_item(ui, Some("person"), "Open profile", None, false, true).clicked() {
            act = Some(Act::User(user.to_owned()));
            ui.close();
        }
        let link = spotify_web_link(&u.uri);
        if fin::menu_item(ui, Some("link"), "Copy link", None, false, link.is_some()).clicked() {
            act = link.map(Act::CopyLink);
            ui.close();
        }
    });
    act
}

/// What a click on the page asked for.
enum Act {
    Play(TrackInfo),
    Open(Source),
    PlayTop,
    Chip(u8),
    Search(String),
    More,
    /// A person's profile, by their id.
    User(String),
    /// A link to a person's profile, to copy.
    CopyLink(String),
    MoreUsers,
}

impl TruffleApp {
    /// The title bar's box changed, gained focus, or was left with Enter or Esc.
    pub(super) fn search_box_events(&mut self, changed: bool, focused_now: bool, enter: bool, esc: bool) {
        if focused_now && self.middle_tab != MiddleTab::Search {
            self.search.back = Some(self.middle_tab);
            self.middle_tab = MiddleTab::Search;
        }
        if changed {
            self.search.typed_at = Some(Instant::now());
            if self.middle_tab != MiddleTab::Search {
                self.search.back = Some(self.middle_tab);
                self.middle_tab = MiddleTab::Search;
            }
        }
        if enter {
            self.remember_search();
            let current = self.search.results.as_ref().is_some_and(|r| r.query == self.search.input.trim());
            if current {
                self.play_top();
            } else {
                self.search.play_top = true;
                self.start_search();
            }
        } else if esc {
            if self.search.input.is_empty() {
                // Esc in an empty box: back where the search started.
                self.middle_tab = self.search.back.take().unwrap_or(MiddleTab::Home);
            }
            self.search.input.clear();
            self.search.sent.clear();
            self.search.results = None;
        }
    }

    /// Runs every frame: the search goes out once typing pauses.
    pub(super) fn search_logic(&mut self, ctx: &egui::Context) {
        let Some(at) = self.search.typed_at else { return };
        let waited = at.elapsed();
        if waited < SEARCH_SETTLE {
            ctx.request_repaint_after(SEARCH_SETTLE - waited);
            return;
        }
        self.search.typed_at = None;
        if self.search.input.trim().is_empty() {
            self.search.sent.clear();
            self.search.results = None;
            self.search.tracks.clear();
            self.search.error = None;
        } else {
            self.start_search();
        }
    }

    /// Keeps what was searched for (a search used: Enter, or a result opened).
    fn remember_search(&mut self) {
        let q = self.search.input.trim().to_owned();
        if q.chars().count() < 2 {
            return;
        }
        let list = &mut self.prefs.recent_searches;
        list.retain(|s| !s.eq_ignore_ascii_case(&q));
        list.insert(0, q);
        list.truncate(RECENT_MAX);
        self.prefs_changed();
    }

    /// Plays the top result: a song from its album, anything else from its start.
    pub(super) fn play_top(&mut self) {
        self.search.play_top = false;
        let Some(results) = self.search.results.clone() else { return };
        match top_result(&results, &self.search.tracks) {
            Some(Top::Song(t)) => self.play_from_album(&t),
            Some(Top::Card(card, _)) => {
                let shuffle = self.current_shuffle();
                self.send(Command::PlaySource { source: source_of(&card.uri), start_index: None, shuffle });
            }
            None => {}
        }
    }

    /// The Search page, filling `ui`.
    pub(super) fn search_view(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let mut act: Option<Act> = None;
        // Within the column Wide screens gives (Split fills).
        let r = ui.max_rect();
        let col = wide::Column::new(self.prefs.wide_screens, r.left(), r.width(), PAD_X, false);
        ScrollArea::vertical().id_salt("search-page").auto_shrink(false).show(ui, |ui| {
            let (left, w) = (col.left, col.width);
            let mut top = ui.cursor().top() + PAD_TOP;
            // Narrow: the title bar has no search box, so the page has its own (`.srch.big`).
            if ui.ctx().content_rect().width() < super::frame::NARROW {
                let rect = Rect::from_min_size(pos2(left, top), vec2(w, 36.0));
                let id = Id::new(super::search::SEARCH_BOX_ID);
                let mut text = std::mem::take(&mut self.search.input);
                let sb = fin::search_box(ui, &p, rect, id, &mut text, "Search songs, albums, artists", "");
                self.search.input = text;
                let left_with = |key| sb.resp.lost_focus() && ui.input(|i| i.key_pressed(key));
                let (enter, esc) = (left_with(egui::Key::Enter), left_with(egui::Key::Escape));
                self.search_box_events(sb.resp.changed(), false, enter, esc);
                self.search_logic(&ui.ctx().clone());
                top += 36.0 + 16.0;
            }
            let bottom = if self.search.input.trim().is_empty() && self.search.sent.is_empty() {
                self.search_start(ui, left, w, top, &mut act)
            } else {
                self.search_results(ui, left, w, top, &mut act)
            };
            ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left - PAD_X, top - PAD_TOP), pos2(left + w + PAD_X, bottom + 28.0)));
        });
        let _ = p;
        match act {
            Some(Act::Play(t)) => {
                self.remember_search();
                self.play_from_album(&t);
            }
            Some(Act::Open(source)) => {
                self.remember_search();
                self.open_source(source, false);
            }
            Some(Act::PlayTop) => {
                self.remember_search();
                self.play_top();
            }
            Some(Act::Chip(k)) => self.search.chip = k,
            Some(Act::Search(q)) => {
                self.search.input = q;
                self.search.typed_at = None;
                self.start_search();
            }
            Some(Act::More) => {
                self.search.loading = true;
                let offset = self.search.tracks.len() as u32;
                self.send(Command::Search { query: self.search.sent.clone(), offset });
            }
            Some(Act::User(user)) => {
                self.remember_search();
                self.show_user(&user);
            }
            Some(Act::CopyLink(link)) => {
                ui.ctx().copy_text(link);
                self.note("Profile link copied", None);
            }
            Some(Act::MoreUsers) => {
                if let Some(offset) = self.search.users.next {
                    self.ask_users(offset);
                }
            }
            None => {}
        }
    }

    /// Asks for a page of people for what was searched (the first once the Users chip shows).
    fn ask_users(&mut self, offset: u32) {
        let query = self.search.sent.clone();
        self.search.users.query = query.clone();
        self.search.users.loading = true;
        self.search.users.error = None;
        self.send(Command::SearchUsers { query, offset });
    }

    /// A section's head (`.hdr`): its name, and "Show all" (to a chip) on the right.
    fn search_hdr(ui: &Ui, x0: f32, x1: f32, y: f32, name: &str, show_all: Option<u8>, act: &mut Option<Act>) {
        let p = crate::ui::theme::palette(ui.ctx());
        let g = ui.painter().layout_no_wrap(name.to_owned(), bold(13.5), p.text_strong);
        ui.painter().galley(pos2(x0, y + HDR_H / 2.0 - g.size().y / 2.0), g, p.text_strong);
        if let Some(chip) = show_all {
            let lg = ui.painter().layout_no_wrap("Show all".to_owned(), medium(11.0), p.text_weak);
            let r = Rect::from_min_size(pos2(x1 - lg.size().x, y + HDR_H / 2.0 - lg.size().y / 2.0), lg.size());
            let resp = ui.interact(r.expand(3.0), Id::new(("search-all", name)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            ui.painter().galley(r.min, lg, if resp.hovered() { p.text } else { p.text_weak });
            if resp.clicked() {
                *act = Some(Act::Chip(chip));
            }
        }
    }

    /// Nothing typed: recent searches, moods and a word on how it works. Returns the bottom.
    fn search_start(&mut self, ui: &mut Ui, left: f32, w: f32, top: f32, act: &mut Option<Act>) -> f32 {
        let p = self.palette;
        let mut y = top;
        let recent = self.prefs.recent_searches.clone();
        if !recent.is_empty() {
            Self::search_hdr(ui, left, left + w, y, "Recent searches", None, act);
            y += HDR_H + HDR_GAP;
            let mut x = left;
            for (k, r) in recent.iter().enumerate() {
                let resp = fin::chip(ui, &p, pos2(x, y + 13.0), Id::new(("recent-search", k)), r, Some("search"), false, false);
                if resp.clicked() {
                    *act = Some(Act::Search(r.clone()));
                }
                x = resp.rect.right() + 8.0;
            }
            y += 26.0 + 22.0;
        }
        Self::search_hdr(ui, left, left + w, y, "Moods", None, act);
        y += HDR_H + HDR_GAP;
        let mut x = left;
        for m in MOODS {
            let resp = fin::chip(ui, &p, pos2(x, y + 13.0), Id::new(("mood-search", m)), m, None, false, false);
            if resp.clicked() {
                *act = Some(Act::Search(m.to_lowercase()));
            }
            x = resp.rect.right() + 8.0;
        }
        y += 26.0 + 22.0;
        let text = "Type anything: songs, albums, artists and playlists show up as you type. Enter plays the top result.";
        let g = ui.painter().layout(text.to_owned(), regular(12.0), p.text_weak, w);
        let h = g.size().y;
        ui.painter().galley(pos2(left, y), g, p.text_weak);
        y + h
    }

    /// The chips and the results. Returns the bottom.
    fn search_results(&mut self, ui: &mut Ui, left: f32, w: f32, top: f32, act: &mut Option<Act>) -> f32 {
        let p = self.palette;
        let (placed, clicked) = chips(ui, &p, left, w, top, self.search.chip);
        if let Some(k) = clicked {
            *act = Some(Act::Chip(k));
        }
        let mut y = placed.last().map_or(top, |r| r.bottom()) + 16.0;
        let say = |ui: &Ui, y: f32, text: &str, color: Color32| {
            let g = ui.painter().layout(text.to_owned(), regular(12.0), color, w);
            let h = g.size().y;
            ui.painter().galley(pos2(left, y), g, color);
            y + h
        };
        if self.search.chip == USERS {
            // People come on their own: asked for once the chip shows.
            if self.search.users.query != self.search.sent {
                self.ask_users(0);
            }
            Self::search_hdr(ui, left, left + w, y, "Users", None, act);
            y += HDR_H + HDR_GAP;
            let q = fold_name(self.search.sent.trim());
            for u in &self.search.users.list {
                let images = &mut self.images;
                let mut picture = |ui: &Ui, pic: Rect| images.paint(ui, u.image.as_deref(), pic, 15, p.widget_bg);
                if let Some(a) = user_row(ui, &p, Rect::from_min_size(pos2(left, y), vec2(w, QR_H)), u, &q, &mut picture) {
                    *act = Some(a);
                }
                y += QR_H;
            }
            let users = &self.search.users;
            if let Some(error) = &users.error {
                y = say(ui, y + 4.0, error, p.error);
            } else if users.list.is_empty() {
                let text = if users.loading { format!("Searching for \u{201c}{}\u{201d}…", self.search.sent) } else { "No users match".to_owned() };
                return say(ui, y, &text, p.text_weak);
            }
            // Also after a page that failed: it is asked again.
            if users.next.is_some() {
                let label = if users.loading { "Loading" } else { "More users" };
                let r = fin::text_button(ui, &p, pos2(left, y + 8.0), Id::new("search-more-users"), label, None, false, !users.loading);
                if r.clicked() {
                    *act = Some(Act::MoreUsers);
                }
                y = r.rect.bottom();
            }
            return y;
        }
        if let Some(error) = self.search.error.clone() {
            return say(ui, y, &error, p.error);
        }
        let Some(results) = self.search.results.clone() else {
            return say(ui, y, &format!("Searching for \u{201c}{}\u{201d}…", self.search.input.trim()), p.text_weak);
        };
        let q = fold(results.query.trim());
        let tracks = self.search.tracks.clone();
        if tracks.is_empty() && results.albums.is_empty() && results.artists.is_empty() && results.playlists.is_empty() {
            return say(ui, y, &format!("Nothing found for \u{201c}{}\u{201d}.", results.query), p.text_weak);
        }
        let x1 = left + w;
        match self.search.chip {
            1 => {
                Self::search_hdr(ui, left, x1, y, "Songs", None, act);
                y += HDR_H + HDR_GAP;
                for t in &tracks {
                    self.search_song(ui, Rect::from_min_size(pos2(left, y), vec2(w, QR_H)), t, &q, act);
                    y += QR_H;
                }
                if results.more {
                    let label = if self.search.loading { "Loading" } else { "More songs" };
                    let r = fin::text_button(ui, &p, pos2(left, y + 8.0), Id::new("search-more"), label, None, false, !self.search.loading);
                    if r.clicked() {
                        *act = Some(Act::More);
                    }
                    y = r.rect.bottom();
                }
                y
            }
            2 => self.search_grid(ui, left, w, y, &results.albums, "Albums", 120.0, false, &q, act),
            3 => self.search_grid(ui, left, w, y, &results.artists, "Artists", 100.0, true, &q, act),
            4 => self.search_grid(ui, left, w, y, &results.playlists, "Playlists", 100.0, false, &q, act),
            _ => {
                // All: the top result beside the first songs (stacked when narrow).
                let two = w >= 600.0;
                let col = if two { 280.0 } else { w };
                let top_card = Rect::from_min_size(pos2(left, y + HDR_H + HDR_GAP), vec2(col, if two { 188.0 } else { 88.0 }));
                Self::search_hdr(ui, left, left + col, y, "Top result", None, act);
                if let Some(top) = top_result(&results, &tracks) {
                    self.top_card(ui, top_card, &top, &q, act);
                }
                let (sx, sy) = if two { (left + col + 24.0, y) } else { (left, top_card.bottom() + 18.0) };
                Self::search_hdr(ui, sx, x1, sy, "Songs", Some(1), act);
                let mut ry = sy + HDR_H + HDR_GAP;
                for t in tracks.iter().take(if two { 4 } else { 5 }) {
                    self.search_song(ui, Rect::from_min_size(pos2(sx, ry), vec2(x1 - sx, QR_H)), t, &q, act);
                    ry += QR_H;
                }
                if tracks.is_empty() {
                    ui.painter().text(pos2(sx + 6.0, ry + 12.0), Align2::LEFT_CENTER, "No songs match", regular(12.0), p.text_weak);
                    ry += 24.0;
                }
                y = top_card.bottom().max(ry) + 18.0;
                // Artists and playlists in one row: as many cards of at least 100 as fit, widened so
                // the row ends at both edges, the pictures 76 % of them (room for a record).
                let items: Vec<(&Card, bool)> = results.artists.iter().map(|c| (c, true)).chain(results.playlists.iter().map(|c| (c, false))).collect();
                if !items.is_empty() {
                    Self::search_hdr(ui, left, x1, y, "Artists, playlists", Some(3), act);
                    y += HDR_H + HDR_GAP;
                    let (fit, cw) = wide::row_fit(w, 100.0, 16.0);
                    let pic = (cw * 0.76).round();
                    let mut row_h: f32 = 0.0;
                    let mut on_top = OnTop::default();
                    for (k, (card, round)) in items.into_iter().take(fit).enumerate() {
                        row_h = row_h.max(self.search_card(ui, pos2(left + k as f32 * (cw + 16.0), y), pic, cw, card, round, &q, act, &mut on_top));
                    }
                    on_top.lift(ui.ctx());
                    y += row_h + 18.0;
                }
                // Albums in one row the same way, at least 110.
                if !results.albums.is_empty() {
                    Self::search_hdr(ui, left, x1, y, "Albums", Some(2), act);
                    y += HDR_H + HDR_GAP;
                    let (fit, cw) = wide::row_fit(w, 110.0, 16.0);
                    let mut row_h: f32 = 0.0;
                    for (k, card) in results.albums.iter().take(fit).enumerate() {
                        row_h = row_h.max(self.search_card(ui, pos2(left + k as f32 * (cw + 16.0), y), cw, cw, card, false, &q, act, &mut OnTop::default()));
                    }
                    y += row_h;
                }
                y
            }
        }
    }

    /// The top result (`.topres`): its picture, its name with the match in the accent, what it is,
    /// and a round play button.
    fn top_card(&mut self, ui: &mut Ui, rect: Rect, top: &Top, q: &str, act: &mut Option<Act>) {
        let p = self.palette;
        let resp = ui.interact(rect, Id::new("search-top"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let painter = ui.painter().clone();
        painter.rect_filled(rect, CornerRadius::same(8), if resp.hovered() { p.hover_bg } else { p.raised_bg });
        painter.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
        // Beside the songs: the picture above the name. Narrow (88 tall): side by side.
        let tall = rect.height() > 100.0;
        let art = if tall { Rect::from_min_size(rect.min + vec2(16.0, 16.0), vec2(84.0, 84.0)) } else { Rect::from_min_size(rect.min + vec2(12.0, 12.0), vec2(64.0, 64.0)) };
        let (name, sub, image, round, open) = match top {
            Top::Song(t) => (t.name.clone(), format!("Song · {}", t.artist_line()), t.cover_large.clone().or_else(|| t.cover_small.clone()), false, None),
            Top::Card(c, kind) => {
                let sub = if c.subtitle.is_empty() || c.subtitle == *kind { (*kind).to_owned() } else { format!("{kind} · {}", c.subtitle) };
                (c.name.clone(), sub, c.image.clone(), *kind == "Artist", Some(source_of(&c.uri)))
            }
        };
        let r = if round { (art.width() / 2.0) as u8 } else { 4 };
        if let Some(Source::Playlist(uri)) = open.as_ref() {
            let source = Source::Playlist(uri.clone());
            let playing = self.playback.track.is_some() && self.playback.source.as_ref() == Some(&source);
            let sleeve = super::sidebar::Sleeve { source, name: name.clone(), image: image.clone(), count: None, uri: None, editable: false };
            self.paint_sleeve(ui, art, &sleeve, playing, resp.hovered(), 0.20);
        } else if !self.images.paint(ui, image.as_deref(), art, r, p.widget_bg) {
            svgicon::paint(&painter, art.shrink(26.0), if round { "artist" } else { "note" }, p.text_weak, svgicon::STROKE);
        }
        let play = if tall {
            Rect::from_min_size(rect.max - vec2(16.0 + 34.0, 16.0 + 34.0), vec2(34.0, 34.0))
        } else {
            Rect::from_center_size(pos2(rect.right() - 12.0 - 17.0, rect.center().y), vec2(34.0, 34.0))
        };
        if tall {
            let tw = rect.width() - 32.0;
            let ng = highlighted(ui, &name, q, heavy(20.0), p.text_strong, p.accent_ink, tw);
            let ny = art.bottom() + 14.0;
            let nh = ng.size().y;
            painter.galley(pos2(rect.left() + 16.0, ny), ng, p.text_strong);
            let sg = one_line(ui, &sub, regular(12.0), p.text_weak, tw - 44.0, false);
            painter.galley(pos2(rect.left() + 16.0, ny + nh + 3.0), sg, p.text_weak);
        } else {
            let x = art.right() + 12.0;
            let tw = play.left() - 10.0 - x;
            let ng = highlighted(ui, &name, q, heavy(17.0), p.text_strong, p.accent_ink, tw);
            let sg = one_line(ui, &sub, regular(12.0), p.text_weak, tw, false);
            let h = ng.size().y + 3.0 + sg.size().y;
            let ny = rect.center().y - h / 2.0;
            let nh = ng.size().y;
            painter.galley(pos2(x, ny), ng, p.text_strong);
            painter.galley(pos2(x, ny + nh + 3.0), sg, p.text_weak);
        }
        let pr = ui.interact(play, Id::new("search-top-play"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Play (Enter)");
        painter.circle_filled(play.center(), 17.0, if pr.hovered() { p.accent } else { p.widget_bg });
        svgicon::paint(&painter, Rect::from_center_size(play.center() + vec2(1.0, 0.0), vec2(14.0, 14.0)), "play", if pr.hovered() { p.on_accent } else { p.text }, svgicon::STROKE);
        if pr.clicked() {
            *act = Some(Act::PlayTop);
        } else if resp.clicked() {
            *act = Some(match (top, open) {
                (Top::Song(t), _) => Act::Play(t.clone()),
                (_, Some(s)) => Act::Open(s),
                _ => Act::PlayTop,
            });
        }
    }

    /// A song (`.qr`): cover, name with the match in the accent, artists, time. Double-click plays it;
    /// dragged onto a playlist in the sidebar it goes in there, as from a list.
    fn search_song(&mut self, ui: &mut Ui, rect: Rect, t: &TrackInfo, q: &str, act: &mut Option<Act>) {
        let p = self.palette;
        let resp = ui.interact(rect, Id::new(("search-song", &t.uri)), Sense::click_and_drag());
        // Only the left button drags songs; local files can't go into Spotify playlists.
        if resp.drag_started_by(egui::PointerButton::Primary) && !t.is_local {
            resp.dnd_set_drag_payload(super::dnd::SongsDrag { uris: vec![t.uri.clone()] });
        }
        let current = self.playback.track.as_ref().is_some_and(|c| c.uri == t.uri);
        if current || resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover_bg);
        }
        let cover = Rect::from_min_size(pos2(rect.left() + 6.0, rect.center().y - 15.0), vec2(30.0, 30.0));
        if !self.images.paint(ui, t.cover_small.as_deref(), cover, 3, p.widget_bg) {
            svgicon::paint(ui.painter(), cover.shrink(8.0), "note", p.text_weak, svgicon::STROKE);
        }
        let x = cover.right() + 10.0;
        let tw = rect.right() - 8.0 - 34.0 - 10.0 - x;
        let color = if current { p.text_strong } else { p.text };
        let title = highlighted(ui, &t.name, q, if current { bold(12.0) } else { medium(12.0) }, color, p.accent_ink, tw);
        let sub = one_line(ui, &t.artist_line(), regular(11.0), p.text_weak, tw, false);
        let h = title.size().y + 1.0 + sub.size().y;
        let ty = rect.center().y - h / 2.0;
        let th = title.size().y;
        ui.painter().galley(pos2(x, ty), title, color);
        ui.painter().galley(pos2(x, ty + th + 1.0), sub, p.text_weak);
        ui.painter().text(pos2(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, fmt_ms(t.duration_ms), regular(11.0), p.text_weak);
        if resp.double_clicked() {
            *act = Some(Act::Play(t.clone()));
        }
        if resp.secondary_clicked() {
            let at = resp.interact_pointer_pos().unwrap_or(rect.center());
            self.open_song_menu(t, at, None);
        }
    }

    /// A result card: its picture (`size`, round for artists), its name with the match in the
    /// accent and what it is, `cw` wide. Returns its height.
    #[allow(clippy::too_many_arguments)]
    fn search_card(&mut self, ui: &mut Ui, at: egui::Pos2, size: f32, cw: f32, card: &Card, round: bool, q: &str, act: &mut Option<Act>, on_top: &mut OnTop) -> f32 {
        let p = self.palette;
        let h = size + 6.0 + 15.0 + 14.0;
        let whole = Rect::from_min_size(at, vec2(cw, h));
        let resp = ui.interact(whole, Id::new(("search-card", &card.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let pic = Rect::from_min_size(pos2(if round { at.x + (cw - size) / 2.0 } else { at.x }, at.y), vec2(size, size));
        let r = if round { (size / 2.0) as u8 } else { 4 };
        if card.uri.starts_with("spotify:playlist:") {
            // A playlist: its sleeve, with the record peeking out (`sl`).
            let source = Source::Playlist(card.uri.clone());
            let playing = self.playback.track.is_some() && self.playback.source.as_ref() == Some(&source);
            let sleeve = super::sidebar::Sleeve { source, name: card.name.clone(), image: card.image.clone(), count: None, uri: None, editable: false };
            on_top.add(self.paint_sleeve(ui, pic, &sleeve, playing, resp.hovered(), 0.20));
        } else {
            if !self.images.paint(ui, card.image.as_deref(), pic, r, p.widget_bg) {
                svgicon::paint(ui.painter(), pic.shrink(size * 0.3), if round { "artist" } else { "album" }, p.text_weak, svgicon::STROKE);
            }
            if resp.hovered() {
                ui.painter().rect_filled(pic, CornerRadius::same(r), Color32::from_white_alpha(10));
            }
        }
        let name = highlighted(ui, &card.name, q, medium(11.5), p.text, p.accent_ink, cw);
        let sub_text = if round { "Artist".to_owned() } else { card.subtitle.clone() };
        let sub = one_line(ui, &sub_text, regular(10.5), p.text_weak, cw, false);
        let ny = pic.bottom() + 6.0;
        let nh = name.size().y;
        let (nx, sx) = if round { (at.x + (cw - name.size().x) / 2.0, at.x + (cw - sub.size().x) / 2.0) } else { (at.x, at.x) };
        ui.painter().galley(pos2(nx, ny), name, p.text);
        ui.painter().galley(pos2(sx, ny + nh), sub, p.text_weak);
        if resp.clicked() {
            *act = Some(Act::Open(source_of(&card.uri)));
        }
        h
    }

    /// One kind of result as a grid. Returns the bottom.
    #[allow(clippy::too_many_arguments)]
    fn search_grid(&mut self, ui: &mut Ui, left: f32, w: f32, top: f32, cards: &[Card], name: &str, size: f32, round: bool, q: &str, act: &mut Option<Act>) -> f32 {
        let p = self.palette;
        Self::search_hdr(ui, left, left + w, top, name, None, act);
        let y = top + HDR_H + HDR_GAP;
        if cards.is_empty() {
            ui.painter().text(pos2(left, y + 12.0), Align2::LEFT_CENTER, format!("No {} match", name.to_lowercase()), regular(12.0), p.text_weak);
            return y + 24.0;
        }
        let gap = if round { 26.0 } else { 24.0 };
        // Playlists: 76 sleeves in cards 100 wide, room for the record.
        let base = if name == "Playlists" { 100.0 } else { size };
        // The cards grow so the rows end at both edges.
        let (cols, cw) = wide::row_fit(w, base, gap);
        let pic = if name == "Playlists" { (cw * 0.76).round() } else { cw };
        let mut bottom = y;
        let mut on_top = OnTop::default();
        for (k, card) in cards.iter().enumerate() {
            let at = pos2(left + (k % cols) as f32 * (cw + gap), y + (k / cols) as f32 * (pic + 36.0 + 18.0));
            let h = self.search_card(ui, at, pic, cw, card, round, q, act, &mut on_top);
            bottom = bottom.max(at.y + h);
        }
        on_top.lift(ui.ctx());
        bottom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_names_beat_starts_beat_words_beat_insides() {
        assert_eq!(score("Neon", "neon"), 100);
        assert_eq!(score("Neon rain", "neon"), 90);
        assert_eq!(score("Cold neon", "neon"), 80);
        assert_eq!(score("Xneonx", "neon"), 70);
        assert_eq!(score("Rain", "neon"), 0);
        // Accents aside.
        assert_eq!(score("Mötley Crüe", "motley crue"), 100);
        assert_eq!(score("Mötley Crüe", "motley"), 90);
        assert_eq!(score("Sigur Rós", "ros"), 80);
    }

    fn card(name: &str) -> Card {
        Card { uri: format!("spotify:album:{name}"), name: name.into(), subtitle: String::new(), image: None }
    }

    #[test]
    fn the_best_match_is_the_top_result_and_artists_win_ties() {
        let mut r = SearchResults { query: "neon".into(), ..Default::default() };
        r.albums = vec![card("Neon")];
        r.artists = vec![card("Neon")];
        assert!(matches!(top_result(&r, &[]), Some(Top::Card(_, "Artist"))));
        r.artists = vec![card("The Neons")];
        assert!(matches!(top_result(&r, &[]), Some(Top::Card(_, "Album"))));
        // What was typed is folded too.
        let mut r = SearchResults { query: "Beyonce".into(), ..Default::default() };
        r.albums = vec![card("Beyoncé")];
        r.artists = vec![card("Beyoncé Tribute Band")];
        assert!(matches!(top_result(&r, &[]), Some(Top::Card(_, "Album"))));
    }

    fn person(id: &str, name: &str) -> Card {
        Card { uri: format!("spotify:user:{id}"), name: name.into(), subtitle: id.into(), image: None }
    }

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        crate::ui::kit::headless::pass(&ctx, Vec::new(), |_| {});
        ctx
    }

    /// One pass of `u`'s row, as wide as the screen (`w`), for what was typed `q`: what it asked
    /// for, and the words it (or its menu) wrote and where.
    fn row(ctx: &egui::Context, w: f32, events: Vec<egui::Event>, u: &Card, q: &str) -> (Option<Act>, Vec<(String, Rect)>) {
        let p = Palette::build(crate::config::ThemeMode::Dark, Color32::RED, None);
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 400.0))), events, ..Default::default() };
        let mut act = None;
        let q = fold_name(q);
        let mut out = ctx.run_ui(raw, |ui| act = user_row(ui, &p, Rect::from_min_size(pos2(0.0, 0.0), vec2(w, QR_H)), u, &q, &mut |_, _| false));
        out.textures_delta.clear();
        let words = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                // Nothing (or only spaces) to see has no place.
                egui::Shape::Text(t) if !t.galley.text().trim().is_empty() => Some((t.galley.text().to_owned(), t.visual_bounding_rect())),
                _ => None,
            })
            .collect();
        (act, words)
    }

    /// The right button, pressed or let go at `at`.
    fn right(at: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![egui::Event::PointerMoved(at), egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE }]
    }

    /// A click at `at` on `u`'s row, `w` wide: what it asked for.
    fn click(ctx: &egui::Context, w: f32, at: egui::Pos2, u: &Card) -> Option<Act> {
        use crate::ui::kit::headless::{move_to, press, release};
        row(ctx, w, move_to(at), u, "");
        row(ctx, w, press(at), u, "");
        row(ctx, w, release(at), u, "").0
    }

    /// A right-click on `u`'s row opens its menu (asking for nothing by itself); then a click on
    /// its `item`: what that asked for.
    fn from_menu(u: &Card, item: &str) -> Option<Act> {
        let ctx = ctx();
        let at = pos2(200.0, QR_H / 2.0);
        // Drawn once first: a press goes to what was there in the pass before.
        row(&ctx, 600.0, crate::ui::kit::headless::move_to(at), u, "");
        row(&ctx, 600.0, right(at, true), u, "");
        assert!(row(&ctx, 600.0, right(at, false), u, "").0.is_none());
        let (_, words) = row(&ctx, 600.0, Vec::new(), u, "");
        let shown: Vec<&str> = words.iter().map(|(w, _)| w.as_str()).filter(|w| ["Open profile", "Copy link"].contains(w)).collect();
        assert_eq!(shown, ["Open profile", "Copy link"], "the menu people rows have, no more");
        let spot = words.iter().find(|(w, _)| w == item).map(|(_, r)| r.center()).expect("in the menu");
        click(&ctx, 600.0, spot, u)
    }

    #[test]
    fn a_person_opens_their_profile_and_their_menu_copies_their_link() {
        let mira = person("mira.l", "Mira L");
        assert!(matches!(click(&ctx(), 600.0, pos2(200.0, QR_H / 2.0), &mira), Some(Act::User(ref id)) if id == "mira.l"));
        assert!(matches!(from_menu(&mira, "Open profile"), Some(Act::User(ref id)) if id == "mira.l"));
        assert!(matches!(from_menu(&mira, "Copy link"), Some(Act::CopyLink(ref l)) if l == "https://open.spotify.com/user/mira.l"));
    }

    #[test]
    fn rows_keep_their_words_inside_at_any_width() {
        let ctx = ctx();
        let long = "𝓔𝓶𝓶𝓪 has a very long name ".repeat(40);
        let names = [long.as_str(), "🔥🔥🔥", "", "  ", "\u{202e}ammE", "مريم", "Z\u{336}\u{335}algo Emma", "宇多田ヒカル", "Emma"];
        for w in [120.0, 599.0, 600.0, 601.0, 3440.0, 5120.0] {
            for name in names {
                let u = person(&"id".repeat(60), name);
                let (_, words) = row(&ctx, w, Vec::new(), &u, "emma");
                assert!(!words.is_empty(), "{name:?} at {w}");
                for (word, r) in words {
                    assert!(r.left() >= 0.0 && r.right() <= w + 0.5 && r.bottom() <= QR_H, "{name:?} at {w}: {word:?} at {r:?}");
                }
                assert!(matches!(click(&ctx, w, pos2(w - 10.0, QR_H / 2.0), &u), Some(Act::User(_))), "{name:?} at {w}");
            }
        }
    }

    /// One pass of the chips on a screen `w` wide: where each went, and the one clicked.
    fn chips_at(ctx: &egui::Context, w: f32, events: Vec<egui::Event>) -> (Vec<Rect>, Option<u8>) {
        let p = Palette::build(crate::config::ThemeMode::Light, Color32::RED, None);
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 400.0))), events, ..Default::default() };
        let mut out = (Vec::new(), None);
        ctx.run_ui(raw, |ui| out = chips(ui, &p, 0.0, w, 0.0, USERS)).textures_delta.clear();
        out
    }

    #[test]
    fn the_chips_take_more_lines_when_narrow_and_users_can_always_be_picked() {
        use crate::ui::kit::headless::{move_to, press, release};
        let ctx = ctx();
        // Just as wide as the chips on one line: one line; half a point less: two.
        let mut one_line = 0.0;
        crate::ui::kit::headless::pass(&ctx, Vec::new(), |ui| one_line = CHIPS.iter().map(|c| fin::chip_w(ui, c, false) + 8.0).sum::<f32>() - 8.0);
        let lines = |placed: &[Rect]| {
            let mut tops: Vec<i32> = placed.iter().map(|r| r.top() as i32).collect();
            tops.dedup();
            tops.len()
        };
        assert_eq!(lines(&chips_at(&ctx, one_line + 0.01, Vec::new()).0), 1);
        assert_eq!(lines(&chips_at(&ctx, one_line - 0.5, Vec::new()).0), 2);
        for w in [90.0, 200.0, 352.0, 600.0, 3440.0, 5120.0] {
            let (placed, _) = chips_at(&ctx, w, Vec::new());
            assert_eq!(placed.len(), CHIPS.len());
            for (k, r) in placed.iter().enumerate() {
                assert!(r.left() >= 0.0 && r.right() <= w.max(r.width()) + 0.5, "chip {k} at {w}: {r:?}");
                assert!(placed[..k].iter().all(|q| !q.intersects(*r)), "chip {k} at {w} lies over another");
            }
            let at = placed[USERS as usize].center();
            chips_at(&ctx, w, move_to(at));
            chips_at(&ctx, w, press(at));
            assert_eq!(chips_at(&ctx, w, release(at)).1, Some(USERS), "at {w}");
        }
    }

    #[test]
    fn initials_are_plain_letters_or_the_first_character() {
        assert_eq!(initials("Emma Stone"), "ES");
        assert_eq!(initials("emma"), "E");
        assert_eq!(initials("Mötley Crüe Fan Club"), "MC");
        assert_eq!(initials("𝓔𝓶𝓶𝓪 𝓢"), "ES");
        assert_eq!(initials("ⓔⓜⓜⓐ"), "E");
        assert_eq!(initials("Z\u{336}\u{335}algo"), "Z");
        assert_eq!(initials("@emma_k"), "E");
        assert_eq!(initials("宇多田ヒカル"), "宇");
        assert_eq!(initials("محمد علي"), "مع");
        assert_eq!(initials("🔥🔥 "), "🔥");
        assert_eq!(initials("  "), "");
        assert_eq!(initials(""), "");
        assert_eq!(initials(&"x".repeat(10_000)), "X");
    }

    #[test]
    fn a_persons_id_shows_when_it_is_not_their_name() {
        assert_eq!(shown_id(&person("mira.l", "Mira L")), Some("mira.l"));
        assert_eq!(shown_id(&person("emma", "Emma")), None);
        assert_eq!(shown_id(&person("ÉMMA", "émma")), None);
        assert_eq!(shown_id(&person("emma", "𝓔𝓶𝓶𝓪")), Some("emma"));
        assert_eq!(shown_id(&person("", "Emma")), None);
    }
}
