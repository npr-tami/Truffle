//! The Albums and Artists pages (`pgAlbums`, `pgArtists`): the albums and artists of Liked Songs, as a
//! grid of covers (round pictures for artists) or a list, sorted by a drop-down. Only the rows on
//! screen are drawn. Sizes are the prototype's (`.pg`, `.phd`, `.h2`, `.grid`, `.card`, `.alr`).

use std::sync::Arc;

use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Popup, Rect, ScrollArea, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::fin::{self, Look};
use super::motion;
use super::sidebar::OnTop;
use super::svgicon;
use super::theme::{bold, heavy, medium, regular};
use super::tracks::one_line;
use super::util::fmt_thousands;
use super::wide;
use super::{MiddleTab, TruffleApp};
use crate::model::{Collection, CollectionItem, Command, Source};

/// What Albums and Artists are, said when hovering where they open.
pub(super) const FROM_LIKES: &str = "From your Liked Songs";

/// `.pg`'s padding: 20 above, 24 at the sides, 28 below.
const PAD_TOP: f32 = 20.0;
const PAD_X: f32 = 24.0;
const PAD_BOTTOM: f32 = 28.0;
/// The head (`.phd`, 30 tall) and the space under it.
const HEAD_H: f32 = 30.0;
const HEAD_GAP: f32 = 18.0;
/// A list row (`.alr`).
const LIST_ROW: f32 = 52.0;

/// How a page is sorted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Sort {
    Recent,
    Name,
    Artist,
    MostLiked,
}

impl Sort {
    fn label(self) -> &'static str {
        match self {
            Sort::Recent => "Recently added",
            Sort::Name => "Name",
            Sort::Artist => "Artist",
            Sort::MostLiked => "Most liked",
        }
    }
}

/// Each page's sort, order and view, and whether the Artists page shows only those followed.
pub(super) struct CollState {
    sort: [Sort; 2],
    reverse: [bool; 2],
    list: [bool; 2],
    following: bool,
}

impl Default for CollState {
    fn default() -> Self {
        Self { sort: [Sort::Recent, Sort::MostLiked], reverse: [false; 2], list: [false; 2], following: false }
    }
}

/// The artists followed, with their liked songs from `liked` (the artists of Liked Songs) and, until
/// Spotify has named them, their names and pictures from there too. Unnamed ones are left out.
pub(super) fn followed_items(following: &[CollectionItem], liked: &[CollectionItem]) -> Vec<CollectionItem> {
    let by_uri: std::collections::HashMap<&str, &CollectionItem> = liked.iter().map(|a| (a.uri.as_str(), a)).collect();
    following
        .iter()
        .filter_map(|f| {
            let mut item = f.clone();
            if let Some(l) = by_uri.get(f.uri.as_str()) {
                item.songs = l.songs;
                if item.name.is_empty() {
                    item.name = l.name.clone();
                }
                if item.image.is_none() {
                    item.image = l.image.clone();
                }
            }
            (!item.name.is_empty()).then_some(item)
        })
        .collect()
}

/// The page's items in the order chosen.
pub(super) fn sorted(items: &[CollectionItem], sort: Sort, reverse: bool) -> Vec<&CollectionItem> {
    let mut v: Vec<&CollectionItem> = items.iter().collect();
    match sort {
        Sort::Recent => v.sort_by_key(|i| i.newest),
        Sort::Name => v.sort_by_cached_key(|i| i.name.to_lowercase()),
        Sort::Artist => v.sort_by_cached_key(|i| (i.sub.to_lowercase(), i.newest)),
        Sort::MostLiked => v.sort_by(|a, b| b.songs.cmp(&a.songs).then(a.newest.cmp(&b.newest))),
    }
    if reverse {
        v.reverse();
    }
    v
}

impl TruffleApp {
    /// Shows the Albums or Artists page, asking for its contents the first time.
    pub(super) fn show_collection(&mut self, tab: MiddleTab) {
        self.middle_tab = tab;
        if !self.collection_asked {
            self.collection_asked = true;
            self.send(Command::Collection);
        }
    }

    /// The Albums or Artists page, filling `ui`.
    pub(super) fn collection_page(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let artists = self.middle_tab == MiddleTab::Artists;
        let k = usize::from(artists);
        let mw = ui.max_rect().width();
        let collection = self.collection.clone().unwrap_or_else(|| Arc::new(Collection::default()));
        // The Artists page has All and Following once Truffle knows who is followed.
        let following = self.following.clone().filter(|_| artists);
        let only_followed = following.is_some() && self.coll.following;
        let followed: Vec<CollectionItem> = match &following {
            Some(f) if only_followed => followed_items(f, &collection.artists),
            _ => Vec::new(),
        };
        let items = if only_followed {
            &followed
        } else if artists {
            &collection.artists
        } else {
            &collection.albums
        };
        let list = sorted(items, self.coll.sort[k], self.coll.reverse[k]);
        let as_list = self.coll.list[k];
        let size = if artists {
            if mw >= 640.0 { 100.0 } else { 84.0 }
        } else if mw >= 1000.0 {
            148.0
        } else if mw >= 700.0 {
            132.0
        } else {
            112.0
        };
        // Within the column Wide screens gives (Split fills).
        let r = ui.max_rect();
        let col = wide::Column::new(self.prefs.wide_screens, r.left(), r.width(), PAD_X, false);
        ScrollArea::vertical().id_salt(("collection", k)).auto_shrink(false).show(ui, |ui| {
            let (left, w) = (col.left, col.width);
            let top = ui.cursor().top() + PAD_TOP;
            self.collection_head(ui, left, w, top, artists, list.len());
            let mut y = top + HEAD_H + HEAD_GAP;
            if following.is_some() {
                // `.chips`: 14 above, 18 below.
                let cy = top + HEAD_H + 14.0 + 13.0;
                let all = fin::chip(ui, &p, pos2(left, cy), Id::new("artists-all"), "All", None, !self.coll.following, false);
                if all.clicked() {
                    self.coll.following = false;
                }
                if fin::chip(ui, &p, pos2(all.rect.right() + 8.0, cy), Id::new("artists-following"), "Following", None, self.coll.following, false).clicked() {
                    self.coll.following = true;
                }
                y = cy + 13.0 + 18.0;
            }
            let still_following = only_followed && list.is_empty() && following.as_ref().is_some_and(|f| !f.is_empty()) && !self.following_named;
            let bottom = if (self.collection.is_none() && !only_followed) || still_following {
                // Nothing at first, then placeholder rows.
                let rows = Rect::from_min_size(pos2(left, y), vec2(w, 56.0 * 6.0));
                motion::loading(ui, Id::new(("library-loading", k)), rows, 56.0, true, &self.palette);
                rows.bottom()
            } else if only_followed && list.is_empty() {
                let any = following.as_ref().is_some_and(|f| !f.is_empty());
                let text = if any { "Couldn't load the artists you follow." } else { "You don't follow any artists yet." };
                ui.painter().text(pos2(left, y + 10.0), Align2::LEFT_CENTER, text, regular(12.0), p.text_weak);
                y + 40.0
            } else if list.is_empty() {
                let what = if artists { "artists" } else { "albums" };
                ui.painter().text(pos2(left, y + 10.0), Align2::LEFT_CENTER, format!("No {what} yet. Like some songs and they show here."), regular(12.0), p.text_weak);
                y + 40.0
            } else {
                // What has just come fades in.
                let was = motion::content_in(ui, Id::new(("library", k, only_followed)));
                let bottom = if as_list { self.collection_list(ui, left, w, y, &list, artists) } else { self.collection_grid(ui, left, w, y, &list, artists, size) };
                ui.set_opacity(was);
                bottom
            };
            let all = Rect::from_min_max(pos2(left - PAD_X, top - PAD_TOP), pos2(left + w + PAD_X, bottom + PAD_BOTTOM));
            ui.advance_cursor_after_rect(all);
        });
    }

    /// "Albums 12", then grid / list and the sort drop-down on the right (`.phd`).
    fn collection_head(&mut self, ui: &mut Ui, left: f32, w: f32, top: f32, artists: bool, n: usize) {
        let p = self.palette;
        let k = usize::from(artists);
        let mid = top + HEAD_H / 2.0;
        let title = ui.painter().layout_no_wrap(if artists { "Artists" } else { "Albums" }.to_owned(), heavy(22.0), p.text_strong);
        let tw = title.size().x;
        ui.painter().galley(pos2(left, mid - title.size().y / 2.0), title, p.text_strong);
        if self.collection.is_some() {
            ui.painter().text(pos2(left + tw + 6.0, mid + 3.0), Align2::LEFT_CENTER, fmt_thousands(n), regular(12.0), p.text_weak);
        }
        // The sort drop-down (`.b.dd`): 28 tall, its icon, the sort's name and a chevron.
        let label = self.coll.sort[k].label();
        let g = ui.painter().layout_no_wrap(label.to_owned(), medium(12.0), p.text);
        let dw = 10.0 + 13.0 + 7.0 + g.size().x + 7.0 + 13.0 + 10.0;
        let dd = Rect::from_min_size(pos2(left + w - dw, mid - 14.0), vec2(dw, 28.0));
        let resp = ui.interact(dd, Id::new(("coll-sort", k)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        ui.painter().rect_filled(dd, CornerRadius::same(6), if resp.hovered() { p.widget_hover_bg } else { p.widget_bg });
        svgicon::paint(ui.painter(), Rect::from_min_size(pos2(dd.left() + 10.0, mid - 6.5), vec2(13.0, 13.0)), "sort", p.text, svgicon::STROKE);
        ui.painter().galley(pos2(dd.left() + 30.0, mid - g.size().y / 2.0), g, p.text);
        svgicon::paint(ui.painter(), Rect::from_min_size(pos2(dd.right() - 23.0, mid - 6.5), vec2(13.0, 13.0)), "chev", p.text, svgicon::STROKE);
        let options: &[Sort] = if artists { &[Sort::MostLiked, Sort::Recent, Sort::Name] } else { &[Sort::Recent, Sort::Name, Sort::Artist, Sort::MostLiked] };
        Popup::menu(&resp).show(|ui| {
            ui.set_width(196.0);
            fin::menu_label(ui, "Sort by");
            for &o in options {
                let on = self.coll.sort[k] == o;
                if fin::menu_item(ui, None, o.label(), None, on, true).clicked() {
                    self.coll.sort[k] = o;
                    ui.close();
                }
            }
            fin::menu_sep(ui);
            if fin::menu_item(ui, Some("sort"), "Reverse order", None, self.coll.reverse[k], true).clicked() {
                self.coll.reverse[k] = !self.coll.reverse[k];
                ui.close();
            }
        });
        // Grid and list (`.ib`, raised while chosen).
        let list = self.coll.list[k];
        let lr = Rect::from_min_size(pos2(dd.left() - 6.0 - 30.0, mid - 15.0), vec2(30.0, 30.0));
        let gr = lr.translate(vec2(-36.0, 0.0));
        if fin::ib(ui, &p, gr, Id::new(("coll-grid", k)), "grid", Look::on_box(!list), "Grid").clicked() {
            self.coll.list[k] = false;
        }
        if fin::ib(ui, &p, lr, Id::new(("coll-list", k)), "list", Look::on_box(list), "List").clicked() {
            self.coll.list[k] = true;
        }
    }

    /// The grid (`.grid` of `.card`s): as many columns of at least `size` as fit, the covers widened
    /// so the rows end at both edges. Returns its bottom.
    #[allow(clippy::too_many_arguments)]
    fn collection_grid(&mut self, ui: &mut Ui, left: f32, w: f32, top: f32, list: &[&CollectionItem], artists: bool, size: f32) -> f32 {
        let (col_gap, row_gap) = if artists { (26.0, 16.0) } else { ((size * 0.24).round(), 18.0) };
        let (cols, size) = wide::row_fit(w, size, col_gap);
        let card_h = size + 7.0 + 15.0 + 14.0;
        let rows = list.len().div_ceil(cols);
        let row_h = card_h + row_gap;
        let clip = ui.clip_rect();
        let first = (((clip.top() - top) / row_h).floor().max(0.0) as usize).min(rows);
        let last = ((((clip.bottom() - top) / row_h).ceil().max(0.0)) as usize).min(rows);
        let mut open = None;
        let mut on_top = OnTop::default();
        for r in first..last {
            for c in 0..cols {
                let Some(item) = list.get(r * cols + c) else { break };
                let at = pos2(left + c as f32 * (size + col_gap), top + r as f32 * row_h);
                if self.collection_card(ui, at, size, item, artists, &mut on_top) {
                    open = Some(item.uri.clone());
                }
            }
        }
        on_top.lift(ui.ctx());
        if let Some(uri) = open {
            self.open_source(if artists { Source::Artist(uri) } else { Source::Album(uri) }, false);
        }
        top + rows as f32 * row_h - row_gap
    }

    /// One card: the cover (an artist's round picture), the name and a line under it (`.card .n`,
    /// `.card .s`). The album that plays shows its record. True when clicked.
    fn collection_card(&mut self, ui: &mut Ui, at: egui::Pos2, size: f32, item: &CollectionItem, artists: bool, on_top: &mut OnTop) -> bool {
        let p = self.palette;
        let cover = Rect::from_min_size(at, vec2(size, size));
        let whole = Rect::from_min_size(at, vec2(size, size + 36.0));
        let resp = ui.interact(whole, Id::new(("coll-card", &item.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let playing_album = !artists && self.playback.track.as_ref().is_some_and(|t| t.album_uri == item.uri);
        if artists {
            if !self.images.paint(ui, item.image.as_deref(), cover, (size / 2.0) as u8, p.widget_bg) {
                ui.painter().circle_filled(cover.center(), size / 2.0, p.widget_bg);
                svgicon::paint(ui.painter(), cover.shrink(size * 0.3), "artist", p.text_weak, svgicon::STROKE);
            }
        } else if playing_album {
            let sleeve = super::sidebar::Sleeve {
                source: Source::Album(item.uri.clone()),
                name: item.name.clone(),
                image: item.image.clone(),
                count: None,
                uri: None,
                editable: false,
            };
            on_top.add(self.paint_sleeve(ui, cover, &sleeve, true, resp.hovered(), 0.2));
        } else {
            if !self.images.paint(ui, item.image.as_deref(), cover, 4, p.widget_bg) {
                svgicon::paint(ui.painter(), cover.shrink(size * 0.32), "album", p.text_weak, svgicon::STROKE);
            }
            ui.painter().rect_stroke(cover, CornerRadius::same(4), Stroke::new(1.0, Color32::from_white_alpha(15)), StrokeKind::Inside);
        }
        if resp.hovered() && !playing_album {
            let r = if artists { (size / 2.0) as u8 } else { 4 };
            ui.painter().rect_filled(cover, CornerRadius::same(r), Color32::from_white_alpha(10));
        }
        let name = one_line(ui, &item.name, medium(12.0), p.text, size, false);
        let songs = format!("{} liked", fmt_thousands(item.songs));
        let sub_text = if !artists {
            format!("{} · {songs}", item.sub)
        } else if item.songs == 0 && self.followed.contains(&item.uri) {
            "Following".to_owned()
        } else {
            songs
        };
        let sub = one_line(ui, &sub_text, regular(11.0), p.text_weak, size, false);
        let ny = cover.bottom() + 7.0;
        let nh = name.size().y;
        let (nx, sx) = if artists {
            (cover.center().x - name.size().x / 2.0, cover.center().x - sub.size().x / 2.0)
        } else {
            (at.x, at.x)
        };
        ui.painter().galley(pos2(nx, ny), name, p.text);
        ui.painter().galley(pos2(sx, ny + nh), sub, p.text_weak);
        resp.clicked()
    }

    /// The list (`.alr` rows): picture, name, and the liked songs on the right. Returns its bottom.
    #[allow(clippy::too_many_arguments)]
    fn collection_list(&mut self, ui: &mut Ui, left: f32, w: f32, top: f32, list: &[&CollectionItem], artists: bool) -> f32 {
        let p = self.palette;
        let clip = ui.clip_rect();
        let first = (((clip.top() - top) / LIST_ROW).floor().max(0.0) as usize).min(list.len());
        let last = ((((clip.bottom() - top) / LIST_ROW).ceil().max(0.0)) as usize).min(list.len());
        let mut open = None;
        for (k, item) in list.iter().enumerate().take(last).skip(first) {
            let row = Rect::from_min_size(pos2(left, top + k as f32 * LIST_ROW), vec2(w, LIST_ROW));
            let resp = ui.interact(row, Id::new(("coll-row", &item.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if resp.hovered() {
                ui.painter().rect_filled(row, CornerRadius::same(6), p.hover_bg);
            }
            let pic = Rect::from_min_size(pos2(row.left() + 8.0, row.center().y - 20.0), vec2(40.0, 40.0));
            let r = if artists { 20 } else { 4 };
            if !self.images.paint(ui, item.image.as_deref(), pic, r, p.widget_bg) {
                svgicon::paint(ui.painter(), pic.shrink(11.0), if artists { "artist" } else { "album" }, p.text_weak, svgicon::STROKE);
            }
            let x = pic.right() + 12.0;
            let right = row.right() - 8.0 - 90.0 - 12.0;
            let t = one_line(ui, &item.name, bold(12.0), p.text, right - x, false);
            let sub = if artists { format!("{} liked songs", fmt_thousands(item.songs)) } else { item.sub.clone() };
            let s = one_line(ui, &sub, regular(11.0), p.text_weak, right - x, false);
            let h = t.size().y + 2.0 + s.size().y;
            let ty = row.center().y - h / 2.0;
            let th = t.size().y;
            ui.painter().galley(pos2(x, ty), t, p.text);
            ui.painter().galley(pos2(x, ty + th + 2.0), s, p.text_weak);
            if !artists {
                let n = item.songs;
                ui.painter().text(pos2(row.right() - 8.0, row.center().y), Align2::RIGHT_CENTER, format!("{} liked", fmt_thousands(n)), regular(11.0), p.text_weak);
            } else if self.followed.contains(&item.uri) {
                ui.painter().text(pos2(row.right() - 8.0, row.center().y), Align2::RIGHT_CENTER, "Following", regular(11.0), p.text_weak);
            }
            if resp.clicked() {
                open = Some(item.uri.clone());
            }
        }
        if let Some(uri) = open {
            self.open_source(if artists { Source::Artist(uri) } else { Source::Album(uri) }, false);
        }
        top + list.len() as f32 * LIST_ROW
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, sub: &str, songs: usize, newest: usize) -> CollectionItem {
        CollectionItem { uri: format!("spotify:album:{name}"), name: name.into(), sub: sub.into(), image: None, songs, newest }
    }

    #[test]
    fn sorts_put_the_right_one_first() {
        let items = vec![item("b", "Zed", 1, 2), item("A", "Amy", 5, 1), item("c", "Bo", 3, 0)];
        let names = |v: Vec<&CollectionItem>| v.iter().map(|i| i.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(sorted(&items, Sort::Recent, false)), ["c", "A", "b"]);
        // Names sort without minding capitals.
        assert_eq!(names(sorted(&items, Sort::Name, false)), ["A", "b", "c"]);
        assert_eq!(names(sorted(&items, Sort::Artist, false)), ["A", "c", "b"]);
        assert_eq!(names(sorted(&items, Sort::MostLiked, false)), ["A", "c", "b"]);
        assert_eq!(names(sorted(&items, Sort::Recent, true)), ["b", "A", "c"]);
    }

    #[test]
    fn followed_artists_take_liked_counts_and_wait_for_names() {
        let artist = |uri: &str, name: &str, songs: usize, newest: usize| CollectionItem { uri: uri.into(), name: name.into(), songs, newest, ..Default::default() };
        let liked = vec![artist("a:1", "Mara", 12, 4), artist("a:2", "Juno", 3, 9)];
        // Followed last first; a:2 not named yet (liked: named from there), a:3 unnamed and not liked.
        let following = vec![artist("a:3", "", 0, 0), artist("a:2", "", 0, 1), artist("a:4", "Sable", 0, 2)];
        let got = followed_items(&following, &liked);
        let view: Vec<(&str, &str, usize, usize)> = got.iter().map(|a| (a.uri.as_str(), a.name.as_str(), a.songs, a.newest)).collect();
        assert_eq!(view, [("a:2", "Juno", 3, 1), ("a:4", "Sable", 0, 2)]);
    }
}
