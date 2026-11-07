//! The Profile page (`pgProfile`), opened from the avatar: the user's picture and name, followers and
//! following, their most liked artists and their own playlists. Someone else's profile (from a
//! playlist's owner or a people list) shows their picture, counts, public playlists and recently
//! played artists; followers and following open a people list (users and artists) to go on from.
//! Split (Wide screens): one's own picture, name and counts on the left, the rows on the right.

use egui::text::LayoutJob;
use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Rect, ScrollArea, Sense, Stroke, StrokeKind, TextFormat, Ui, pos2, vec2};

use super::collection::{Sort, sorted};
use super::fin;
use super::motion;
use super::sidebar::OnTop;
use super::svgicon;
use super::theme::{Palette, bold, heavy, medium, regular};
use super::tracks::one_line;
use super::util::fmt_thousands;
use super::wide;
use super::{MiddleTab, TruffleApp};
use std::sync::Arc;
use crate::model::{CollectionItem, Command, ListenHow, Source};

const PAD_X: f32 = 24.0;
/// Split: the left column's width (the picture, the name and the counts).
const SPLIT_LEFT: f32 = 440.0;
/// Split: a count's tile (also SHUFFLE and LEFT on a song list's page).
pub(super) const COUNT_H: f32 = 58.0;
/// Between Top artists' pictures.
const ARTIST_GAP: f32 = 28.0;

/// One of the user's playlists: what it opens, its name, cover and number of songs.
type Mine = (Source, String, Option<String>, Option<u32>);

/// Top artists across `w` on a page whose content is `page_w` wide: how many fit and how big, at
/// least 68 (112 with Bigger cards), growing to the mockups' 150 (194) on wide pages.
fn artist_fit(w: f32, page_w: f32, bigger: bool) -> (usize, f32) {
    let min = if bigger { wide::grow(page_w, 112.0, 194.0) } else { wide::grow(page_w, 68.0, 150.0) };
    wide::row_fit(w, min, ARTIST_GAP)
}

/// Your playlists' smallest card on a page whose content is `page_w` wide (`split` in two): 96 (132
/// with Bigger cards), growing to the mockups' 190 (226) on wide pages, 180 (216) when split.
fn list_min(page_w: f32, split: bool, bigger: bool) -> f32 {
    let big = if split { 180.0 } else { 190.0 };
    if bigger { wide::grow(page_w, 132.0, big + 36.0) } else { wide::grow(page_w, 96.0, big) }
}

impl TruffleApp {
    /// A user's profile: one's own, or someone else's.
    pub(super) fn show_user(&mut self, username: &str) {
        let me = self.profile.as_ref().is_some_and(|p| p.username.eq_ignore_ascii_case(username));
        if me {
            self.show_profile();
            return;
        }
        self.user_view = Some(username.to_owned());
        self.ask_profile(username);
        self.middle_tab = MiddleTab::Profile;
    }

    /// Asks for a user's profile page once (again after it failed).
    fn ask_profile(&mut self, username: &str) {
        if self.profiles_failed.remove(username) || self.profiles_asked.insert(username.to_owned()) {
            self.send(Command::UserPage(username.to_owned()));
        }
    }

    /// Who a user follows, or who follows them.
    pub(super) fn show_people(&mut self, username: &str, followers: bool) {
        self.people_view = Some((username.to_owned(), followers));
        let key = (username.to_owned(), followers);
        if !self.people.contains_key(&key) {
            self.send(Command::People { username: username.to_owned(), followers });
        }
        self.middle_tab = MiddleTab::People;
    }

    pub(super) fn show_profile(&mut self) {
        self.user_view = None;
        if let Some(me) = self.profile.as_ref().map(|p| p.username.clone()) {
            self.ask_profile(&me);
        }
        self.middle_tab = MiddleTab::Profile;
        if !self.collection_asked {
            self.collection_asked = true;
            self.send(Command::Collection);
        }
    }

    pub(super) fn profile_page(&mut self, ui: &mut Ui) {
        if let Some(user) = self.user_view.clone() {
            self.user_profile_page(ui, &user);
            return;
        }
        let p = self.palette;
        let r = ui.max_rect();
        let ps = if r.width() >= 600.0 { 132.0 } else { 104.0 };
        let col = wide::Column::new(self.prefs.wide_screens, r.left(), r.width(), PAD_X, true);
        let name = self.profile.as_ref().map(|u| if u.display_name.is_empty() { u.username.clone() } else { u.display_name.clone() }).unwrap_or_default();
        let username = self.profile.as_ref().map(|u| u.username.clone()).unwrap_or_default();
        let image = self.profile.as_ref().and_then(|u| u.image_url.clone());
        let mine: Vec<Mine> = self
            .playlists
            .iter()
            .filter(|pl| pl.editable)
            .map(|pl| (Source::Playlist(pl.uri.clone()), pl.name.clone(), pl.image_url.clone(), pl.track_count))
            .collect();
        // Top artists: a row of as many as fit (two rows beside the picture when split).
        let (rows, artists_w) = if col.split { (2, col.halves(SPLIT_LEFT).1.1) } else { (1, col.width) };
        let per_row = artist_fit(artists_w, col.width, self.prefs.fill_cards).0;
        let top: Vec<CollectionItem> = self
            .collection
            .as_ref()
            .map(|c| sorted(&c.artists, Sort::MostLiked, false).into_iter().take(per_row * rows).cloned().collect())
            .unwrap_or_default();
        let artists_known = self.collection.as_ref().map(|c| c.artists.len());
        let counts = self.user_pages.get(&username).map(|u| (u.followers, u.following));
        let mut open: Option<Source> = None;
        let mut go_artists = false;
        let mut people_to: Option<(String, bool)> = None;
        ScrollArea::vertical().id_salt("profile").auto_shrink(false).show(ui, |ui| {
            let top_y = ui.cursor().top();
            let (left, x1, mut y, side_bottom) = if col.split {
                // The picture, the name, the buttons and the counts on the left; the rows on the
                // right, level with the picture.
                let ((l, lw), (rx, rw)) = col.halves(SPLIT_LEFT);
                let own = mine.len();
                let tiles: Vec<(usize, &str, Option<bool>)> = [
                    counts.and_then(|c| c.0).map(|n| (n as usize, if n == 1 { "follower" } else { "followers" }, Some(true))),
                    counts.and_then(|c| c.1).map(|n| (n as usize, "following", Some(false))),
                    Some((own, if own == 1 { "playlist of your own" } else { "playlists of your own" }, None)),
                    Some((self.liked.len(), "liked songs", None)),
                    artists_known.map(|n| (n, "artists", None)),
                ]
                .into_iter()
                .flatten()
                .collect();
                let (go, bottom) = self.profile_side(ui, l, lw, top_y + 22.0, &name, &username, image.as_deref(), &tiles);
                if let Some(followers) = go {
                    people_to = Some((username.clone(), followers));
                }
                (rx, rx + rw, top_y + 22.0, bottom)
            } else {
                let (left, x1) = (col.left, col.right());
                // The hero (`padding: 22px 24px 16px`): the round picture, PROFILE, the name, the
                // details and the buttons, standing on the picture's foot.
                let pic = Rect::from_min_size(pos2(left, top_y + 22.0), vec2(ps, ps));
                self.profile_picture(ui, pic, image.as_deref());
                let x = pic.right() + 24.0;
                let acts_top = pic.bottom() - 2.0 - 28.0;
                let meta_top = acts_top - 14.0 - 19.0;
                let title = ui.painter().layout_no_wrap(name.clone(), heavy(30.0), p.text_strong);
                let title_top = meta_top - 6.0 - 31.5 - (title.size().y - 31.5) / 2.0;
                let kind = fin::spaced(ui, "PROFILE", bold(10.5), p.text_weak, 1.05);
                let kind_top = meta_top - 6.0 - 31.5 - 4.0 - kind.size().y;
                ui.painter().galley(pos2(x, kind_top), kind, p.text_weak);
                ui.painter().galley(pos2(x, title_top), title, p.text_strong);
                let strong = TextFormat { font_id: bold(12.0), color: p.text, ..Default::default() };
                let weak = TextFormat { font_id: regular(12.0), color: p.text_weak, ..Default::default() };
                let mut job = LayoutJob::default();
                job.append(&fmt_thousands(mine.len()), 0.0, strong.clone());
                job.append(if mine.len() == 1 { " playlist of your own" } else { " playlists of your own" }, 0.0, weak.clone());
                job.append(" · ", 0.0, weak.clone());
                job.append(&fmt_thousands(self.liked.len()), 0.0, strong.clone());
                job.append(" liked songs", 0.0, weak.clone());
                if let Some(n) = artists_known {
                    job.append(" by ", 0.0, weak.clone());
                    job.append(&fmt_thousands(n), 0.0, strong);
                    job.append(" artists", 0.0, weak);
                }
                // Followers and following first (each opening its list), then the rest, cut to fit.
                let meta_y = meta_top + 1.0;
                let mut mx = x;
                if let Some((followers, following)) = counts {
                    let (go, end) = follow_counts(ui, pos2(x, meta_y), followers, following, false);
                    if go.is_some() {
                        people_to = go.map(|g| (username.clone(), g));
                    }
                    if end > x {
                        let g = ui.painter().layout_no_wrap(" · ".to_owned(), regular(12.0), p.text_weak);
                        mx = end + g.size().x;
                        ui.painter().galley(pos2(end, meta_y), g, p.text_weak);
                    }
                }
                job.wrap = egui::text::TextWrapping::truncate_at_width((x1 - mx).max(1.0));
                let meta = ui.painter().layout_job(job);
                ui.painter().galley(pos2(mx, meta_y), meta, p.text_weak);
                self.profile_buttons(ui, pos2(x, acts_top), &username);
                (left, x1, pic.bottom() + 16.0 + 4.0, top_y)
            };

            // Top artists and the user's playlists.
            let lists_all_id = Id::new("profile-lists-all");
            let lists_all = ui.ctx().data(|d| d.get_temp::<bool>(lists_all_id)).unwrap_or(false);
            let head = |ui: &Ui, y: f32, name: &str, note: &str, id: &str, link: &str| -> bool {
                let g = ui.painter().layout_no_wrap(name.to_owned(), bold(13.5), p.text_strong);
                let gw = g.size().x;
                ui.painter().galley(pos2(left, y + 12.0 - g.size().y / 2.0), g, p.text_strong);
                ui.painter().text(pos2(left + gw + 10.0, y + 12.0), Align2::LEFT_CENTER, note, regular(11.0), p.text_weak);
                let lg = ui.painter().layout_no_wrap(link.to_owned(), medium(11.0), p.text_weak);
                let r = Rect::from_min_size(pos2(x1 - lg.size().x, y + 12.0 - lg.size().y / 2.0), lg.size());
                let resp = ui.interact(r.expand(3.0), Id::new(id), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                ui.painter().galley(r.min, lg, if resp.hovered() { p.text } else { p.text_weak });
                resp.clicked()
            };
            if head(ui, y, "Top artists", "by your liked songs", "profile-artists", "Show all") {
                go_artists = true;
            }
            y += 24.0 + 10.0;
            y = self.profile_artists(ui, left, x1, col, y, &top, artists_known.is_some(), &mut open);
            y += 22.0;
            // Show all: every playlist; Show less: two rows again.
            if head(ui, y, "Your playlists", &fmt_thousands(mine.len()), "profile-lists", if lists_all { "Show less" } else { "Show all" }) {
                ui.ctx().data_mut(|d| d.insert_temp(lists_all_id, !lists_all));
            }
            y += 24.0 + 10.0;
            y = self.profile_lists(ui, left, x1, col, y, &mine, lists_all, &mut open);
            let bottom = y.max(side_bottom);
            ui.advance_cursor_after_rect(Rect::from_min_max(pos2(col.left - PAD_X, top_y), pos2(col.right() + PAD_X, bottom + 28.0)));
        });
        if go_artists {
            self.show_collection(MiddleTab::Artists);
        }
        if let Some((user, followers)) = people_to {
            self.show_people(&user, followers);
        }
        if let Some(source) = open {
            self.open_source(source, false);
        }
    }

    /// The user's round picture (a pink round with a person while there is none).
    fn profile_picture(&mut self, ui: &Ui, pic: Rect, image: Option<&str>) {
        let ps = pic.width();
        if !self.images.paint(ui, image, pic, (ps / 2.0) as u8, Color32::from_rgb(232, 80, 106)) {
            svgicon::paint(ui.painter(), pic.shrink(ps * 0.3), "artist", Color32::from_white_alpha(200), svgicon::STROKE);
        }
    }

    /// Share (copies a link to the profile) and Settings, from `at`.
    fn profile_buttons(&mut self, ui: &Ui, at: egui::Pos2, username: &str) {
        let p = self.palette;
        let share = fin::text_button(ui, &p, at, Id::new("profile-share"), "Share", Some("share"), false, !username.is_empty());
        if share.on_hover_text("Copy a link to your Spotify profile").clicked() {
            ui.ctx().copy_text(format!("https://open.spotify.com/user/{username}"));
            self.note("Profile link copied", None);
        }
        let settings = fin::text_button(ui, &p, pos2(at.x + 108.0, at.y), Id::new("profile-settings"), "Settings", Some("gear"), false, true);
        if settings.clicked() {
            self.open_settings();
        }
    }

    /// Split: the picture, PROFILE, the name and the buttons one under the other, centred in the
    /// column from `left`, `w` wide, then the counts as small tiles, two to a row (followers and
    /// following open their lists). Returns which list to open (true: followers) and the bottom.
    #[allow(clippy::too_many_arguments)]
    fn profile_side(&mut self, ui: &mut Ui, left: f32, w: f32, y: f32, name: &str, username: &str, image: Option<&str>, tiles: &[(usize, &str, Option<bool>)]) -> (Option<bool>, f32) {
        let p = self.palette;
        let mid = left + w / 2.0;
        let ps = w.min(300.0);
        let pic = Rect::from_min_size(pos2(mid - ps / 2.0, y), vec2(ps, ps));
        self.profile_picture(ui, pic, image);
        let kind = fin::spaced(ui, "PROFILE", bold(10.5), p.text_weak, 1.05);
        let kind_top = pic.bottom() + 22.0;
        let kh = kind.size().y;
        ui.painter().galley(pos2(mid - kind.size().x / 2.0, kind_top), kind, p.text_weak);
        let title = one_line(ui, name, heavy(30.0), p.text_strong, w, false);
        let title_top = kind_top + kh + 4.0;
        let th = title.size().y;
        ui.painter().galley(pos2(mid - title.size().x / 2.0, title_top), title, p.text_strong);
        let acts_w = 108.0 + fin::text_button_w(ui, "Settings", true);
        let acts_top = title_top + th + 14.0;
        self.profile_buttons(ui, pos2(mid - acts_w / 2.0, acts_top), username);
        count_tiles(ui, &p, left, w, acts_top + 28.0 + 22.0, tiles)
    }

    /// Top artists from `y`: round pictures of at least 68 (112 with Bigger cards, more on wide
    /// pages, see [`artist_fit`]), 28 apart, as many across `left`..`x1` as fit, sized so the row
    /// ends at both edges, in as many rows as `top` fills. Nothing while the collection is read
    /// (`known`), then a placeholder row. Returns the bottom.
    #[allow(clippy::too_many_arguments)]
    fn profile_artists(&mut self, ui: &mut Ui, left: f32, x1: f32, col: wide::Column, y: f32, top: &[CollectionItem], known: bool, open: &mut Option<Source>) -> f32 {
        let p = self.palette;
        if !known {
            let row = Rect::from_min_size(pos2(left, y), vec2(x1 - left, 48.0));
            motion::loading(ui, Id::new("profile-top-loading"), row, 48.0, true, &p);
            return y + 48.0;
        }
        if top.is_empty() {
            let was = motion::content_in(ui, Id::new("profile-top-none"));
            ui.painter().text(pos2(left, y + 10.0), Align2::LEFT_CENTER, "No top artists yet", regular(12.0), p.text_weak);
            ui.set_opacity(was);
            return y + 30.0;
        }
        // What has just come fades in.
        let was = motion::content_in(ui, Id::new("profile-top"));
        let fill = self.prefs.fill_cards;
        let (per_row, d) = artist_fit(x1 - left, col.width, fill);
        let font = if fill { 12.0 } else { 11.0 };
        let row_h = d + 7.0 + font + 4.0 + 14.0;
        for (k, a) in top.iter().enumerate() {
            let pic = Rect::from_min_size(pos2(left + (k % per_row) as f32 * (d + ARTIST_GAP), y + (k / per_row) as f32 * row_h), vec2(d, d));
            let hit = Rect::from_min_size(pic.min, vec2(d, d + 22.0));
            let resp = ui.interact(hit, Id::new(("profile-artist", &a.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if !self.images.paint(ui, a.image.as_deref(), pic, (d / 2.0) as u8, p.widget_bg) {
                ui.painter().circle_filled(pic.center(), d / 2.0, p.widget_bg);
                svgicon::paint(ui.painter(), pic.shrink(d * 0.3), "artist", p.text_weak, svgicon::STROKE);
            }
            let g = one_line(ui, &a.name, medium(font), p.text, d + 8.0, false);
            ui.painter().galley(pos2(pic.center().x - g.size().x / 2.0, pic.bottom() + 7.0), g, p.text);
            if resp.clicked() {
                *open = Some(Source::Artist(a.uri.clone()));
            }
        }
        ui.set_opacity(was);
        y + top.len().div_ceil(per_row) as f32 * row_h - 14.0
    }

    /// The user's playlists from `y`: sleeves in cards of at least 96 (132 with Bigger cards, more on
    /// wide pages, see [`list_min`]), as many across `left`..`x1` as fit, sized so the row ends at
    /// both edges; two rows of them, or every one with `all`. Songs dropped on one go into it;
    /// right-click has its menu. Returns the bottom.
    #[allow(clippy::too_many_arguments)]
    fn profile_lists(&mut self, ui: &mut Ui, left: f32, x1: f32, col: wide::Column, y: f32, mine: &[Mine], all: bool, open: &mut Option<Source>) -> f32 {
        let p = self.palette;
        let fill = self.prefs.fill_cards;
        let gap = if fill { 16.0 } else { 12.0 };
        let (per_row, cell) = wide::row_fit(x1 - left, list_min(col.width, col.split, fill), gap);
        // With Bigger cards the sleeve is the whole card; else 76 in 96, the record peeking out in it.
        let cs = if fill { cell } else { (cell * 76.0 / 96.0).round() };
        let shown = if all { mine.len() } else { mine.len().min(per_row * 2) };
        let row_h = cs + 34.0 + 14.0;
        let mut on_top = OnTop::default();
        for (k, (source, name, image, count)) in mine.iter().take(shown).enumerate() {
            let cx = left + (k % per_row) as f32 * (cell + gap);
            let cover = Rect::from_min_size(pos2(cx, y + (k / per_row) as f32 * row_h), vec2(cs, cs));
            let hit = Rect::from_min_size(cover.min, vec2(cell, cs + 34.0));
            let resp = ui.interact(hit, Id::new(("profile-list", source.cache_key())), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let playing = self.playback.track.is_some() && self.playback.source.as_ref() == Some(source);
            let sleeve = super::sidebar::Sleeve { source: source.clone(), name: name.clone(), image: image.clone(), count: *count, uri: None, editable: true };
            on_top.add(self.paint_sleeve(ui, cover, &sleeve, playing, resp.hovered(), 0.2));
            let g = one_line(ui, name, medium(if fill { 12.5 } else { 11.5 }), p.text, cell, false);
            let gh = g.size().y;
            ui.painter().galley(pos2(cx, cover.bottom() + 7.0), g, p.text);
            let songs = count.map(|n| format!("{} songs", fmt_thousands(n as usize))).unwrap_or_default();
            ui.painter().text(pos2(cx, cover.bottom() + 7.0 + gh), Align2::LEFT_TOP, songs, regular(10.5), p.text_weak);
            if resp.clicked() {
                *open = Some(source.clone());
            }
            // As in the sidebar: songs dropped on it are added, and right-click has its menu.
            if let Source::Playlist(uri) = source {
                if resp.dnd_hover_payload::<super::dnd::SongsDrag>().is_some() {
                    ui.painter().rect_stroke(cover.expand(2.0), CornerRadius::same(6), Stroke::new(2.0, p.accent_ink), StrokeKind::Outside);
                }
                if let Some(songs) = resp.dnd_release_payload::<super::dnd::SongsDrag>() {
                    self.add_dropped_songs(uri.clone(), songs.uris.clone());
                }
                let pinned = self.prefs.pinned_playlists.contains(uri);
                let editable = self.playlist(uri).is_some_and(|pl| pl.editable);
                let mut menu = None;
                resp.context_menu(|ui| super::library::playlist_menu(ui, uri, pinned, editable, &mut menu));
                if let Some(menu) = menu {
                    self.apply_row_menu(menu);
                }
            }
        }
        on_top.lift(ui.ctx());
        y + shown.div_ceil(per_row).max(1) as f32 * row_h - 14.0
    }

    /// Someone else's profile: their picture, name, counts (followers and following open their
    /// lists), Share, their public playlists and the artists they played lately.
    fn user_profile_page(&mut self, ui: &mut Ui, user: &str) {
        let p = self.palette;
        let r = ui.max_rect();
        let ps = if r.width() >= 600.0 { 132.0 } else { 104.0 };
        // Split doesn't split it: like Fill.
        let col = wide::Column::new(self.prefs.wide_screens, r.left(), r.width(), PAD_X, false);
        let page = self.user_pages.get(user).cloned();
        let failed = self.profiles_failed.contains(user);
        let mut open: Option<Source> = None;
        let mut people_to: Option<bool> = None;
        let mut user_to: Option<String> = None;
        let mut follow_to: Option<bool> = None;
        let mut listen_to: Option<(ListenHow, String)> = None;
        let peek = self.peek_friend(user);
        let how = ListenHow::Friend(user.to_owned());
        let listening = self.listening.as_ref().is_some_and(|l| l.how == how);
        ScrollArea::vertical().id_salt(("user-profile", user)).auto_shrink(false).show(ui, |ui| {
            let (left, w) = (col.left, col.width);
            let x1 = left + w;
            let top_y = ui.cursor().top();
            let Some(page) = page else {
                if failed {
                    let was = motion::content_in(ui, Id::new(("profile-failed", user)));
                    ui.painter().text(pos2(left, top_y + 40.0), Align2::LEFT_CENTER, "Couldn't load this profile.", regular(12.0), p.text_weak);
                    ui.set_opacity(was);
                } else {
                    // Nothing at first, then placeholder rows.
                    let rows = Rect::from_min_size(pos2(left, top_y + 22.0), vec2(x1 - left, 56.0 * 4.0));
                    motion::loading(ui, Id::new(("profile-loading", user)), rows, 56.0, true, &self.palette);
                }
                ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left, top_y), pos2(x1, top_y + 80.0)));
                return;
            };
            // What has just come fades in.
            let was = motion::content_in(ui, Id::new(("profile", user)));
            let pic = Rect::from_min_size(pos2(left, top_y + 22.0), vec2(ps, ps));
            if !self.images.paint(ui, page.image.as_deref(), pic, (ps / 2.0) as u8, p.widget_bg) {
                ui.painter().circle_filled(pic.center(), ps / 2.0, p.widget_bg);
                svgicon::paint(ui.painter(), pic.shrink(ps * 0.3), "artist", p.text_weak, svgicon::STROKE);
            }
            let x = pic.right() + 24.0;
            let acts_top = pic.bottom() - 2.0 - 28.0;
            let meta_top = acts_top - 14.0 - 19.0;
            let title = one_line(ui, &page.name, heavy(30.0), p.text_strong, (x1 - x).max(60.0), false);
            let title_top = meta_top - 6.0 - 31.5 - (title.size().y - 31.5) / 2.0;
            let kind = fin::spaced(ui, "PROFILE", bold(10.5), p.text_weak, 1.05);
            let kind_top = meta_top - 6.0 - 31.5 - 4.0 - kind.size().y;
            ui.painter().galley(pos2(x, kind_top), kind, p.text_weak);
            ui.painter().galley(pos2(x, title_top), title, p.text_strong);
            let meta_y = meta_top + 1.0;
            // Followers and following first (each opening its list), then the playlists, cut to fit.
            let (go, end) = follow_counts(ui, pos2(x, meta_y), page.followers, page.following, false);
            if go.is_some() {
                people_to = go;
            }
            if let Some(n) = page.playlists_total.or((!page.playlists.is_empty()).then_some(page.playlists.len() as u32)) {
                let strong = TextFormat { font_id: bold(12.0), color: p.text, ..Default::default() };
                let weak = TextFormat { font_id: regular(12.0), color: p.text_weak, ..Default::default() };
                let mut job = LayoutJob::default();
                if end > x {
                    job.append(" · ", 0.0, weak.clone());
                }
                job.append(&fmt_thousands(n as usize), 0.0, strong);
                job.append(if n == 1 { " public playlist" } else { " public playlists" }, 0.0, weak);
                job.wrap = egui::text::TextWrapping::truncate_at_width((x1 - end).max(1.0));
                let g = ui.painter().layout_job(job);
                ui.painter().galley(pos2(end, meta_y), g, p.text_weak);
            }
            let mut bx = x;
            if page.allow_follows
                && let Some(on) = page.is_following
            {
                let (word, icon, tip) = if on { ("Following", "check", "Unfollow") } else { ("Follow", "plus", "Follow on Spotify") };
                let b = fin::text_button(ui, &p, pos2(bx, acts_top), Id::new("user-follow"), word, Some(icon), !on, true).on_hover_text(tip);
                if b.clicked() {
                    follow_to = Some(!on);
                }
                bx = b.rect.right() + 8.0;
            }
            // Listen along while they play something (Spotify's friend activity).
            let live = peek.as_ref().filter(|f| f.live);
            if listening || live.is_some() {
                let word = if listening { "Listening" } else { "Listen along" };
                let b = fin::text_button(ui, &p, pos2(bx, acts_top), Id::new("user-listen"), word, Some("eq"), listening, true);
                let tip = if listening { "Stop listening along" } else { "Hear what they play, as they play it" };
                bx = b.rect.right() + 8.0;
                if b.on_hover_text(tip).clicked() {
                    listen_to = Some((how.clone(), page.name.clone()));
                }
            }
            let share = fin::text_button(ui, &p, pos2(bx, acts_top), Id::new("user-share"), "Share", Some("share"), false, true);
            let share_end = share.rect.right();
            if share.on_hover_text("Copy a link to this profile").clicked() {
                ui.ctx().copy_text(format!("https://open.spotify.com/user/{}", page.username));
                self.note("Profile link copied", None);
            }
            // What they play now, beside the buttons when there is room.
            if let Some(f) = live.filter(|f| !f.song.is_empty()) {
                let sx = share_end + 14.0;
                let text = if f.artist.is_empty() { format!("Playing {}", f.song) } else { format!("Playing {} · {}", f.song, f.artist) };
                if x1 - sx > 80.0 {
                    let cy = acts_top + 14.0;
                    svgicon::paint(ui.painter(), Rect::from_min_size(pos2(sx, cy - 5.0), vec2(10.0, 10.0)), "eq", p.accent_ink, svgicon::STROKE);
                    let g = one_line(ui, &text, regular(12.0), p.text_weak, x1 - sx - 15.0, false);
                    ui.painter().galley(pos2(sx + 15.0, cy - g.size().y / 2.0), g, p.text_weak);
                }
            }

            let mut y = pic.bottom() + 20.0;
            let head = |ui: &Ui, y: f32, name: &str| {
                ui.painter().text(pos2(left, y + 12.0), Align2::LEFT_CENTER, name, bold(13.5), p.text_strong);
            };
            if !page.playlists.is_empty() {
                head(ui, y, "Public playlists");
                y += 34.0;
                let (cols, cell) = wide::row_fit(w, 96.0, 12.0);
                let mut on_top = OnTop::default();
                for (k, card) in page.playlists.iter().enumerate() {
                    let cx = left + (k % cols) as f32 * (cell + 12.0);
                    let cy = y + (k / cols) as f32 * 124.0;
                    let side = (cell * 0.8).round();
                    let cover = Rect::from_min_size(pos2(cx, cy), vec2(side, side));
                    let hit = Rect::from_min_size(cover.min, vec2(cell, side + 34.0));
                    let resp = ui.interact(hit, Id::new(("user-list", &card.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                    let source = Source::Playlist(card.uri.clone());
                    let playing = self.playback.track.is_some() && self.playback.source.as_ref() == Some(&source);
                    let sleeve = super::sidebar::Sleeve { source: source.clone(), name: card.name.clone(), image: card.image.clone(), count: None, uri: None, editable: false };
                    on_top.add(self.paint_sleeve(ui, cover, &sleeve, playing, resp.hovered(), 0.2));
                    let g = one_line(ui, &card.name, medium(11.5), p.text, cell, false);
                    ui.painter().galley(pos2(cx, cover.bottom() + 7.0), g, p.text);
                    if resp.clicked() {
                        open = Some(source);
                    }
                }
                on_top.lift(ui.ctx());
                y += page.playlists.len().div_ceil(cols) as f32 * 124.0 + 8.0;
            }
            if !page.artists.is_empty() {
                head(ui, y, "Recently played artists");
                y += 34.0;
                let bottom = people_grid(self, ui, left, w, y, &page.artists, &mut open, &mut user_to);
                y = bottom + 8.0;
            }
            if page.playlists.is_empty() && page.artists.is_empty() {
                ui.painter().text(pos2(left, y + 10.0), Align2::LEFT_CENTER, "Nothing public on this profile yet.", regular(12.0), p.text_weak);
                y += 30.0;
            }
            ui.set_opacity(was);
            ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left - PAD_X, top_y), pos2(x1 + PAD_X, y + 28.0)));
        });
        if let Some((how, name)) = listen_to {
            self.toggle_listen(how, name);
        }
        if let Some(follow) = follow_to {
            self.set_user_followed(user, follow);
            self.send(Command::FollowUser { username: user.to_owned(), follow });
            let name = self.user_pages.get(user).map(|u| u.name.clone()).unwrap_or_else(|| user.to_owned());
            self.toast(if follow { format!("Following {name}") } else { format!("Unfollowed {name}") }, false);
        }
        if let Some(followers) = people_to {
            self.show_people(user, followers);
        }
        if let Some(u) = user_to {
            self.show_user(&u);
        }
        if let Some(source) = open {
            self.open_source(source, false);
        }
    }

    /// Shows a user as followed (or not) at once: the button, their follower count, and one's own
    /// following (read again when next shown).
    pub(super) fn set_user_followed(&mut self, user: &str, follow: bool) {
        if let Some(page) = self.user_pages.get_mut(user) {
            let page = Arc::make_mut(page);
            if page.is_following != Some(follow) {
                if let Some(n) = page.followers.as_mut() {
                    *n = if follow { n.saturating_add(1) } else { n.saturating_sub(1) };
                }
                page.is_following = Some(follow);
            }
        }
        // Their followers list and one's own following are read again when next shown.
        self.people.remove(&(user.to_owned(), true));
        if let Some(me) = self.profile.as_ref().map(|p| p.username.clone()) {
            self.people.remove(&(me.clone(), false));
            self.profiles_asked.remove(&me);
        }
    }

    /// Who a user follows, or who follows them: round cards of people and artists.
    pub(super) fn people_page(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let Some((user, followers)) = self.people_view.clone() else { return };
        let list = self.people.get(&(user.clone(), followers)).cloned();
        let failed = self.profiles_failed.contains(&user);
        let whose = self
            .user_pages
            .get(&user)
            .map(|u| u.name.clone())
            .or_else(|| self.profile.as_ref().filter(|p| p.username == user).map(|p| p.display_name.clone()))
            .unwrap_or_else(|| user.clone());
        let mut open: Option<Source> = None;
        let mut user_to: Option<String> = None;
        let mut back_to_profile = false;
        let r = ui.max_rect();
        let col = wide::Column::new(self.prefs.wide_screens, r.left(), r.width(), PAD_X, false);
        ScrollArea::vertical().id_salt(("people", &user, followers)).auto_shrink(false).show(ui, |ui| {
            let (left, w) = (col.left, col.width);
            let top_y = ui.cursor().top() + 20.0;
            let title = ui.painter().layout_no_wrap(if followers { "Followers" } else { "Following" }.to_owned(), heavy(22.0), p.text_strong);
            let tw = title.size().x;
            ui.painter().galley(pos2(left, top_y + 15.0 - title.size().y / 2.0), title, p.text_strong);
            if let Some(l) = &list {
                ui.painter().text(pos2(left + tw + 6.0, top_y + 18.0), Align2::LEFT_CENTER, fmt_thousands(l.list.len()), regular(12.0), p.text_weak);
            }
            // Whose: their name, back to their profile.
            let g = ui.painter().layout_no_wrap(whose.clone(), medium(12.0), p.text_weak);
            let r = Rect::from_min_size(pos2(left, top_y + 34.0), g.size());
            let resp = ui.interact(r, Id::new("people-whose"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            ui.painter().galley(r.min, g, if resp.hovered() { p.text } else { p.text_weak });
            if resp.clicked() {
                back_to_profile = true;
            }
            let y = top_y + 34.0 + 30.0;
            // What has just come (or why it didn't) fades in.
            let was = if list.is_none() && !failed { ui.opacity() } else { motion::content_in(ui, Id::new(("people", &user, followers))) };
            let bottom = match &list {
                None if failed => {
                    ui.painter().text(pos2(left, y + 10.0), Align2::LEFT_CENTER, "Couldn't load the list.", regular(12.0), p.text_weak);
                    y + 30.0
                }
                None => {
                    // Nothing at first, then placeholder rows.
                    let rows = Rect::from_min_size(pos2(left, y), vec2(w, 52.0 * 5.0));
                    motion::loading(ui, Id::new("people-loading"), rows, 52.0, true, &self.palette);
                    rows.bottom()
                }
                Some(l) if l.list.is_empty() => {
                    let text = if followers { "No followers yet." } else { "Not following anyone yet." };
                    ui.painter().text(pos2(left, y + 10.0), Align2::LEFT_CENTER, text, regular(12.0), p.text_weak);
                    y + 30.0
                }
                Some(l) => people_grid(self, ui, left, w, y, &l.list, &mut open, &mut user_to),
            };
            ui.set_opacity(was);
            ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left - PAD_X, top_y - 20.0), pos2(left + w + PAD_X, bottom + 28.0)));
        });
        if back_to_profile {
            self.show_user(&user);
        }
        if let Some(u) = user_to {
            self.show_user(&u);
        }
        if let Some(source) = open {
            self.open_source(source, false);
        }
    }
}

/// Split: the counts as small tiles, two to a row, across `left`..`left + w` from `top`: the number
/// over what it counts, both cut to fit. The ones with a list (true: followers) open it. Returns the
/// list clicked and the bottom (`top` with none).
pub(super) fn count_tiles(ui: &Ui, p: &Palette, left: f32, w: f32, top: f32, tiles: &[(usize, &str, Option<bool>)]) -> (Option<bool>, f32) {
    let tw = ((w - 10.0) / 2.0).max(1.0);
    let room = (tw - 28.0).max(1.0);
    let mut go = None;
    for (k, &(n, label, opens)) in tiles.iter().enumerate() {
        let rect = Rect::from_min_size(pos2(left + (k % 2) as f32 * (tw + 10.0), top + (k / 2) as f32 * (COUNT_H + 10.0)), vec2(tw, COUNT_H));
        let hovered = opens.is_some_and(|followers| {
            let resp = ui.interact(rect, Id::new(("profile-count", followers)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if resp.clicked() {
                go = Some(followers);
            }
            resp.hovered()
        });
        ui.painter().rect_filled(rect, CornerRadius::same(8), if hovered { p.hover_bg } else { p.raised_bg });
        ui.painter().rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
        // Kept inside the tile, however narrow.
        let inside = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        let num = one_line(ui, &fmt_thousands(n), bold(18.0), p.text_strong, room, false);
        let nh = num.size().y;
        inside.galley(pos2(rect.left() + 14.0, rect.top() + 9.0), num, p.text_strong);
        let g = one_line(ui, label, regular(11.5), p.text_weak, room, false);
        inside.galley(pos2(rect.left() + 14.0, rect.top() + 9.0 + nh + 1.0), g, p.text_weak);
    }
    let bottom = if tiles.is_empty() { top } else { top + tiles.len().div_ceil(2) as f32 * (COUNT_H + 10.0) - 10.0 };
    (go, bottom)
}

/// "38 followers · 212 following" from `at`, each count a link (hovered: underlined). `dot`: a
/// " · " goes first. Returns which was clicked (true: followers) and where the words end.
fn follow_counts(ui: &Ui, at: egui::Pos2, followers: Option<u32>, following: Option<u32>, dot: bool) -> (Option<bool>, f32) {
    let p = crate::ui::theme::palette(ui.ctx());
    let mut x = at.x;
    let mut clicked = None;
    let mut first = !dot;
    for (n, word, is_followers) in [(followers, "followers", true), (following, "following", false)] {
        let Some(n) = n else { continue };
        if !first {
            let g = ui.painter().layout_no_wrap(" · ".to_owned(), regular(12.0), p.text_weak);
            let gw = g.size().x;
            ui.painter().galley(pos2(x, at.y), g, p.text_weak);
            x += gw;
        }
        first = false;
        let word = if n == 1 && is_followers { "follower" } else { word };
        let strong = TextFormat { font_id: bold(12.0), color: p.text, ..Default::default() };
        let weak = TextFormat { font_id: regular(12.0), color: p.text_weak, ..Default::default() };
        let mut job = LayoutJob::default();
        job.append(&fmt_thousands(n as usize), 0.0, strong);
        job.append(&format!(" {word}"), 0.0, weak);
        let g = ui.painter().layout_job(job);
        let rect = Rect::from_min_size(pos2(x, at.y), g.size());
        let resp = ui.interact(rect, Id::new(("follow-count", is_followers)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        ui.painter().galley(rect.min, g, p.text_weak);
        if resp.hovered() {
            ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, egui::Stroke::new(1.0, p.text));
        }
        if resp.clicked() {
            clicked = Some(is_followers);
        }
        x = rect.right();
    }
    (clicked, x)
}

/// Round cards (people and artists) filling the width; a person opens their profile, an artist
/// their page. Returns the bottom.
#[allow(clippy::too_many_arguments)]
fn people_grid(app: &mut TruffleApp, ui: &mut Ui, left: f32, w: f32, top: f32, list: &[crate::model::Card], open: &mut Option<Source>, user_to: &mut Option<String>) -> f32 {
    let p = app.palette;
    let (cols, size) = wide::row_fit(w, 100.0, 26.0);
    let row_h = size + 7.0 + 15.0 + 14.0 + 16.0;
    let clip = ui.clip_rect();
    for (k, card) in list.iter().enumerate() {
        let at = pos2(left + (k % cols) as f32 * (size + 26.0), top + (k / cols) as f32 * row_h);
        let pic = Rect::from_min_size(at, vec2(size, size));
        if pic.bottom() + 40.0 < clip.top() || pic.top() > clip.bottom() {
            continue;
        }
        let hit = Rect::from_min_size(at, vec2(size, size + 36.0));
        let resp = ui.interact(hit, Id::new(("person", &card.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        if !app.images.paint(ui, card.image.as_deref(), pic, (size / 2.0) as u8, p.widget_bg) {
            ui.painter().circle_filled(pic.center(), size / 2.0, p.widget_bg);
            svgicon::paint(ui.painter(), pic.shrink(size * 0.3), "artist", p.text_weak, svgicon::STROKE);
        }
        if resp.hovered() {
            ui.painter().circle_filled(pic.center(), size / 2.0, Color32::from_white_alpha(10));
        }
        let name = one_line(ui, &card.name, medium(12.0), p.text, size + 16.0, false);
        let nh = name.size().y;
        ui.painter().galley(pos2(pic.center().x - name.size().x / 2.0, pic.bottom() + 7.0), name, p.text);
        if !card.subtitle.is_empty() {
            let sub = one_line(ui, &card.subtitle, regular(11.0), p.text_weak, size + 16.0, false);
            ui.painter().galley(pos2(pic.center().x - sub.size().x / 2.0, pic.bottom() + 7.0 + nh), sub, p.text_weak);
        }
        if resp.clicked() {
            if let Some(user) = card.uri.strip_prefix("spotify:user:") {
                *user_to = Some(user.to_owned());
            } else if card.uri.starts_with("spotify:artist:") {
                *open = Some(Source::Artist(card.uri.clone()));
            }
        }
    }
    top + list.len().div_ceil(cols) as f32 * row_h - 16.0
}

#[cfg(test)]
mod tests {
    use egui::epaint::ClippedShape;
    use egui::{Color32, Pos2, Shape};

    use super::*;
    use crate::config::ThemeMode;
    use crate::ui::kit::headless::{drawn, move_to, pass, press, release};

    const TILES: [(usize, &str, Option<bool>); 5] =
        [(7, "followers", Some(true)), (227, "following", Some(false)), (14, "playlists of your own", None), (23_872, "liked songs", None), (8_046, "artists", None)];

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        pass(&ctx, Vec::new(), |_| {});
        ctx
    }

    fn palette(mode: ThemeMode) -> Palette {
        Palette::build(mode, Color32::RED, None)
    }

    /// One pass of `tiles` in a column `w` wide from (20, 10): the list clicked and the bottom.
    fn tiles_pass(ctx: &egui::Context, events: Vec<egui::Event>, w: f32, tiles: &[(usize, &str, Option<bool>)]) -> (Option<bool>, f32) {
        let p = palette(ThemeMode::Dark);
        let mut out = (None, 0.0);
        pass(ctx, events, |ui| out = count_tiles(ui, &p, 20.0, w, 10.0, tiles));
        out
    }

    /// A click at `at` on the tiles: the list it opens.
    fn click(at: Pos2) -> Option<bool> {
        let ctx = ctx();
        tiles_pass(&ctx, Vec::new(), 440.0, &TILES);
        tiles_pass(&ctx, move_to(at), 440.0, &TILES);
        tiles_pass(&ctx, press(at), 440.0, &TILES);
        tiles_pass(&ctx, release(at), 440.0, &TILES).0
    }

    #[test]
    fn the_counts_are_tiles_two_to_a_row() {
        // 440 wide: tiles 215 wide, 58 tall, 10 apart; five make three rows.
        assert_eq!(tiles_pass(&ctx(), Vec::new(), 440.0, &TILES), (None, 10.0 + 3.0 * 68.0 - 10.0));
        assert_eq!(tiles_pass(&ctx(), Vec::new(), 440.0, &TILES[..1]), (None, 10.0 + 58.0));
        assert_eq!(tiles_pass(&ctx(), Vec::new(), 440.0, &[]), (None, 10.0));
        // Followers and following open their lists; the others open nothing.
        assert_eq!(click(pos2(20.0 + 100.0, 10.0 + 29.0)), Some(true));
        assert_eq!(click(pos2(20.0 + 225.0 + 100.0, 10.0 + 29.0)), Some(false));
        assert_eq!(click(pos2(20.0 + 100.0, 10.0 + 68.0 + 29.0)), None);
        // Under the tiles: nothing.
        assert_eq!(click(pos2(20.0 + 100.0, 204.0 + 20.0)), None);
    }

    #[test]
    fn long_counts_and_names_stay_in_their_tiles() {
        let long = [(usize::MAX, "🎧🎧 playlists of your own, and a very long way past the end of the tile 🎧", Some(true)), (0, "", None), (1, "🎧", None)];
        for w in [60.0, 215.0, 440.0] {
            let p = palette(ThemeMode::Dark);
            let shapes: Vec<ClippedShape> = drawn(&ctx(), Vec::new(), |ui| {
                count_tiles(ui, &p, 20.0, w, 10.0, &long);
            });
            for c in &shapes {
                // What shows of it.
                let r = c.shape.visual_bounding_rect().intersect(c.clip_rect);
                assert!(r.is_negative() || (r.left() >= 19.0 && r.right() <= 20.0 + w + 1.0), "{r:?} out of the {w} column");
            }
        }
    }

    #[test]
    fn the_tiles_take_the_palettes_colours() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let p = palette(mode);
            let shapes = drawn(&ctx(), Vec::new(), |ui| {
                count_tiles(ui, &p, 20.0, 440.0, 10.0, &TILES);
            });
            for c in &shapes {
                match &c.shape {
                    Shape::Rect(r) if r.fill != Color32::TRANSPARENT => assert_eq!(r.fill, p.raised_bg, "{mode:?}"),
                    Shape::Rect(r) => assert_eq!(r.stroke.color, p.border, "{mode:?}"),
                    Shape::Text(t) => assert!(t.galley.job.sections.iter().all(|s| [p.text_strong, p.text_weak].contains(&s.format.color)), "{mode:?}"),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn top_artists_fill_the_row() {
        // (row, page, Bigger cards, how many): a normal page (1452) has 15 of 68 grown to 70.7, as
        // before; Centred's 1800 has 10 of about 155 as in mockup A; a 34" window's page in Fill 15,
        // and Split's right column there 12 a row as in mockup C; a 5120 window's page 25.
        let want = [
            (1452.0, 1452.0, false, 15),
            (1500.0, 1500.0, false, 15),
            (1650.0, 1650.0, false, 12),
            (1800.0, 1800.0, false, 10),
            (2264.0, 2752.0, false, 12),
            (2752.0, 2752.0, false, 15),
            (4432.0, 4432.0, false, 25),
            (1452.0, 1452.0, true, 10),
            (1800.0, 1800.0, true, 8),
            (2752.0, 2752.0, true, 12),
            (300.0, 300.0, true, 2),
            (100.0, 100.0, true, 1),
        ];
        for (w, page, bigger, n) in want {
            let (got, d) = artist_fit(w, page, bigger);
            assert_eq!(got, n, "{w} in {page}");
            assert!((got as f32 * d + (got - 1) as f32 * ARTIST_GAP - w).abs() < 0.01, "{got} of {d} in {w}");
            if page >= 1800.0 {
                assert!(d >= if bigger { 194.0 } else { 150.0 }, "{d} in {w}");
            }
        }
        // Up to 1500 exactly as before; Bigger cards always bigger.
        let mut w = 0.0;
        while w <= 5120.0 {
            if w <= 1500.0 {
                assert_eq!(artist_fit(w, w, false), wide::row_fit(w, 68.0, ARTIST_GAP), "{w}");
                assert_eq!(artist_fit(w, w, true), wide::row_fit(w, 112.0, ARTIST_GAP), "{w}");
            }
            assert!(artist_fit(w, w, true).1 >= artist_fit(w, w, false).1, "{w}");
            w += 3.0;
        }
    }

    #[test]
    fn your_playlists_grow_like_the_mockups() {
        // (row, page, split, Bigger cards, how many): as before on a normal page; 8 a row at Centred's
        // 1800 (mockup A); 11 in Split's right column on a 34" window (mockup C); exactly 22 of 190
        // in a 5120 window's page.
        let want = [
            (1452.0, 1452.0, false, false, 13),
            (1800.0, 1800.0, false, false, 8),
            (2264.0, 2752.0, true, false, 11),
            (2752.0, 2752.0, false, false, 13),
            (4432.0, 4432.0, false, false, 22),
            (1452.0, 1452.0, false, true, 9),
            (1800.0, 1800.0, false, true, 7),
            (90.0, 90.0, false, true, 1),
        ];
        for (w, page, split, bigger, n) in want {
            let gap = if bigger { 16.0 } else { 12.0 };
            let min = list_min(page, split, bigger);
            let (got, cell) = wide::row_fit(w, min, gap);
            assert_eq!(got, n, "{w} in {page}");
            assert!(cell >= min.min(w), "{cell} under {min} in {w}");
        }
        assert_eq!((list_min(4432.0, false, false), list_min(4432.0, true, false)), (190.0, 180.0));
        assert_eq!((list_min(1500.0, false, false), list_min(1500.0, true, true)), (96.0, 132.0));
        assert_eq!(list_min(5120.0, true, true), 216.0);
    }
}
