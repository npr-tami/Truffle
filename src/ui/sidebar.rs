//! The 300 sidebar (the Final design's `.side`): Home, Albums and Artists, a line, then Pinned (sleeves in
//! three columns, Liked Songs first) and Playlists (All / Mine / Others, a filter, sleeves or a list). Every
//! playlist is a sleeve with its record peeking out on the right; the record slides further out on hover, and
//! the playing one stays out with an accent ring and moving bars.

use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Pos2, Rect, Sense, Shadow, Stroke, StrokeKind, Ui, pos2, vec2};

use super::dnd::SongsDrag;
use super::fin::{self, Look};
use super::library::{RowMenu, playlist_menu};
use super::motion;
use super::svgicon;
use super::theme::{Palette, bold, medium, regular};
use super::tracks::{liked_color, one_line};
use super::util::fmt_thousands;
use super::{MiddleTab, TruffleApp};
use crate::config::{LibraryView, PlaylistFilter};
use crate::model::{Command, PlayStatus, Source};
use crate::text::{contains_folded, fold};

/// One sleeve to draw.
pub(super) struct Sleeve {
    pub(super) source: Source,
    pub(super) name: String,
    pub(super) image: Option<String>,
    pub(super) count: Option<u32>,
    pub(super) uri: Option<String>,
    pub(super) editable: bool,
}

/// What a sleeve painted on its layer (shapes `from` up to `to`), so it can go over the cards
/// painted after it, and how high.
#[derive(Clone, Copy, Debug)]
pub(super) struct Raised {
    layer: egui::LayerId,
    from: usize,
    to: usize,
    lift: Lift,
}

/// How high a sleeve whose record is out goes: the hovered one over everything, then one whose
/// record is still sliding back in, then the playing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Lift {
    Playing,
    Back,
    Hovered,
}

impl Lift {
    /// A sleeve's, when its record is out: hovered, sliding `back` in, or playing.
    fn of(hovered: bool, back: bool, playing: bool) -> Option<Lift> {
        if hovered {
            Some(Lift::Hovered)
        } else if back {
            Some(Lift::Back)
        } else if playing {
            Some(Lift::Playing)
        } else {
            None
        }
    }
}

impl Raised {
    /// Where the next shape goes on `ui`'s layer.
    fn mark(ui: &Ui) -> usize {
        ui.ctx().graphics(|g| g.get(ui.layer_id()).map_or(0, |l| l.next_idx().0))
    }
}

/// A row's sleeves whose record is out (hovered, playing, or sliding back in). Once the row is
/// painted they go on top, so the record lies over the next card's cover instead of under it.
#[derive(Default)]
pub(super) struct OnTop(Vec<Raised>);

impl OnTop {
    pub(super) fn add(&mut self, raised: Option<Raised>) {
        self.0.extend(raised);
    }

    /// Moves them over everything painted since, with their clipping as it was, the highest
    /// [`Lift`] on top. Of the same, the first painted ends on top: records come out to the right,
    /// over the later ones.
    pub(super) fn lift(self, ctx: &egui::Context) {
        if self.0.is_empty() {
            return;
        }
        // Each one moved goes over the ones moved before it.
        let mut order: Vec<(usize, Raised)> = self.0.into_iter().enumerate().collect();
        order.sort_by_key(|&(i, r)| (r.lift, std::cmp::Reverse(i)));
        ctx.graphics_mut(|g| {
            for (_, r) in order {
                let list = g.entry(r.layer);
                let shapes: Vec<egui::epaint::ClippedShape> = list.all_entries().skip(r.from).take(r.to.saturating_sub(r.from)).cloned().collect();
                for k in r.from..r.from + shapes.len() {
                    list.reset_shape(egui::layers::ShapeIdx(k));
                }
                for c in shapes {
                    list.add(c.clip_rect, c.shape);
                }
            }
        });
    }
}

/// The Pinned grid's edit mode (its Edit / Done): on, and the pin being dragged to a new place (its
/// index among the pins, and where it was grabbed from its cell's corner).
#[derive(Default)]
pub(super) struct PinEdit {
    pub(super) on: bool,
    pub(super) drag: Option<(usize, egui::Vec2)>,
}

/// How much bigger a sleeve's cover gets as it lifts toward the user on hover.
const LIFT: f32 = 0.04;

/// A sleeve's cover `lift` of the way (0-1) up toward the user: the rect it is drawn in (a little
/// bigger around its centre) and its shadow (at rest it falls right onto the record; lifted it falls
/// below too, wider and darker).
fn lifted(cover: Rect, lift: f32) -> (Rect, Shadow) {
    let rect = cover.scale_from_center(1.0 + LIFT * lift);
    let shadow = Shadow {
        offset: [3, (6.0 * lift).round() as i8],
        blur: (8.0 + 10.0 * lift).round() as u8,
        spread: 0,
        color: Color32::from_black_alpha((90.0 + 60.0 * lift).round() as u8),
    };
    (rect, shadow)
}

/// The places 0..`n` with the one at `from` moved to `to`, the others closing up.
fn reorder(n: usize, from: usize, to: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    if from < n {
        let k = order.remove(from);
        order.insert(to.min(n - 1), k);
    }
    order
}

/// The slot whose centre is nearest `at`.
fn nearest_slot(slots: &[Rect], at: Pos2) -> usize {
    slots.iter().enumerate().min_by(|a, b| a.1.center().distance(at).total_cmp(&b.1.center().distance(at))).map_or(0, |(i, _)| i)
}

/// The × over a cover's top right corner in the Pinned grid's edit mode: 18 across, dark (red when
/// hovered). Clicked, it unpins.
fn unpin_badge(ui: &Ui, p: &Palette, cover: Rect, id: Id) -> egui::Response {
    let c = pos2(cover.right() - 3.0, cover.top() + 3.0);
    let resp = ui.interact(Rect::from_center_size(c, vec2(18.0, 18.0)), id.with("unpin"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text("Unpin");
    ui.painter().circle_filled(c, 9.0, if resp.hovered() { p.error } else { Color32::from_black_alpha(190) });
    ui.painter().circle_stroke(c, 9.0, Stroke::new(1.0, Color32::from_white_alpha(40)));
    let k = 3.0;
    ui.painter().line_segment([pos2(c.x - k, c.y - k), pos2(c.x + k, c.y + k)], Stroke::new(1.5, Color32::WHITE));
    ui.painter().line_segment([pos2(c.x - k, c.y + k), pos2(c.x + k, c.y - k)], Stroke::new(1.5, Color32::WHITE));
    resp
}

/// A nav row (Home, Albums, Artists) at `row`: its icon and name, chosen when `on`, and what it is
/// when hovered (`tip`).
pub(super) fn nav_row(ui: &Ui, p: &Palette, row: Rect, icon: &str, name: &str, on: bool, tip: Option<&str>) -> egui::Response {
    let mut resp = ui.interact(row, Id::new(("nav", name)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    if let Some(tip) = tip {
        resp = resp.on_hover_text(tip);
    }
    if on {
        fin::paint_chosen(ui.painter(), p, row);
    } else if resp.hovered() {
        ui.painter().rect_filled(row, CornerRadius::same(6), p.hover_bg);
    }
    let icon_rect = Rect::from_min_size(pos2(row.left() + 12.0, row.center().y - 8.0), vec2(16.0, 16.0));
    svgicon::paint(ui.painter(), icon_rect, icon, if on { p.accent_ink } else { p.text_weak }, svgicon::STROKE);
    let font = if on { bold(12.5) } else { medium(12.5) };
    ui.painter().text(pos2(icon_rect.right() + 12.0, row.center().y), Align2::LEFT_CENTER, name, font, if on { p.text_strong } else { p.text });
    resp
}

impl TruffleApp {
    pub(super) fn sidebar_full(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let r = ui.max_rect();
        // Nav rows: 34 tall, 2 apart, 10 in from the top and 8 from the sides.
        let mut y = r.top() + 10.0;
        let home = self.home_shown();
        let (albums_on, artists_on) = (self.middle_tab == MiddleTab::Albums, self.middle_tab == MiddleTab::Artists);
        for (icon, name, on) in [("home", "Home", home), ("album", "Albums", albums_on), ("artist", "Artists", artists_on)] {
            let row = Rect::from_min_size(pos2(r.left() + 8.0, y), vec2(r.width() - 16.0, 34.0));
            // Albums and Artists are those of Liked Songs: no number (it read as something else).
            let tip = (icon != "home").then_some(super::collection::FROM_LIKES);
            if nav_row(ui, &p, row, icon, name, on, tip).clicked() {
                match icon {
                    "home" => self.show_home(),
                    "album" => self.show_collection(MiddleTab::Albums),
                    _ => self.show_collection(MiddleTab::Artists),
                }
            }
            y += 34.0 + 2.0;
        }
        y += 8.0 - 2.0;
        ui.painter().hline((r.left() + 14.0)..=(r.right() - 14.0), y, Stroke::new(1.0, p.border));
        y += 1.0 + 6.0;

        let list = Rect::from_min_max(pos2(r.left(), y), r.max);
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("side-scroll").max_rect(list));
        child.set_clip_rect(list.intersect(ui.clip_rect()));
        let lists = egui::ScrollArea::vertical().id_salt("side-scroll-area").auto_shrink(false).show(&mut child, |ui| self.sidebar_lists(ui));
        // The fade at the bottom (30 px to the sidebar's grey), when the lists go on past it.
        if lists.content_size.y > lists.inner_rect.height() + 0.5 {
            let fade = Rect::from_min_max(pos2(r.left(), r.bottom() - 30.0), r.max);
            fin::vgradient(ui.painter(), fade, Color32::TRANSPARENT, p.sidebar_bg);
        }
    }

    pub(super) fn sidebar_lists(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let width = ui.available_width();
        let left = ui.max_rect().left();
        let mut y = ui.cursor().top();
        let needle = fold(self.playlist_search.trim());
        let matches = |name: &str| contains_folded(name, &needle);

        // Pinned: Liked Songs, then the pinned playlists in the order they were pinned.
        let mut pins: Vec<Sleeve> = Vec::new();
        let liked = || Sleeve { source: Source::LikedSongs, name: "Liked Songs".into(), image: None, count: Some(self.liked.len() as u32), uri: None, editable: false };
        // Liked Songs is pinned like any playlist, or else first under Playlists (it is the user's own).
        let liked_rest = !self.prefs.liked_pinned && matches("Liked Songs") && self.prefs.playlist_filter != PlaylistFilter::Others;
        if self.prefs.liked_pinned && matches("Liked Songs") {
            pins.push(liked());
        }
        for uri in self.prefs.pinned_playlists.clone() {
            if let Some(pl) = self.playlists.iter().find(|pl| pl.uri == uri).filter(|pl| matches(&pl.name)) {
                pins.push(Sleeve { source: Source::Playlist(pl.uri.clone()), name: pl.name.clone(), image: pl.image_url.clone(), count: pl.track_count, uri: Some(pl.uri.clone()), editable: pl.editable });
            }
        }
        let mut rest: Vec<Sleeve> = if liked_rest { vec![liked()] } else { Vec::new() };
        rest.extend(self
            .playlist_filtered
            .iter()
            .filter_map(|&i| self.playlists.get(i))
            .filter(|pl| !self.prefs.pinned_playlists.contains(&pl.uri))
            .map(|pl| Sleeve { source: Source::Playlist(pl.uri.clone()), name: pl.name.clone(), image: pl.image_url.clone(), count: pl.track_count, uri: Some(pl.uri.clone()), editable: pl.editable }));

        if !pins.is_empty() {
            y = self.side_section(ui, left, width, y, "Pinned", |_, _, _| false);
            let edit = self.pin_edit.on;
            y = self.sleeve_grid_fin(ui, left, width, y, &pins, edit);
        } else {
            self.pin_edit.on = false;
        }

        // Playlists, with All / Mine / Others on the right.
        y = self.side_section(ui, left, width, y, "Playlists", |app, ui, sec| app.mine_tabs(ui, sec));

        // The tools: the filter (28 tall), sleeves / list, new playlist.
        let tools = Rect::from_min_size(pos2(left + 14.0, y + 2.0), vec2(width - 14.0 - 10.0, 28.0));
        let plus = Rect::from_min_size(pos2(tools.right() - 30.0, tools.center().y - 15.0), vec2(30.0, 30.0));
        let list_btn = plus.translate(vec2(-34.0, 0.0));
        let grid_btn = list_btn.translate(vec2(-34.0, 0.0));
        let search = Rect::from_min_max(tools.min, pos2(grid_btn.left() - 4.0, tools.bottom()));
        let mut text = std::mem::take(&mut self.playlist_search);
        let sb = fin::search_box(ui, &p, search, Id::new(super::PLAYLIST_SEARCH_ID), &mut text, "Filter playlists", "Ctrl+L");
        let mut changed = text != self.playlist_search;
        if sb.resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) && !text.is_empty() {
            text.clear();
            changed = true;
        }
        self.playlist_search = text;
        if sb.resp.changed() || changed {
            self.refilter_playlists();
        }
        let sleeves = self.prefs.library_view == LibraryView::Sleeves;
        if fin::ib(ui, &p, grid_btn, Id::new("side-grid"), "grid", Look::on_box(sleeves), "Sleeves").clicked() && !sleeves {
            self.prefs.library_view = LibraryView::Sleeves;
            self.prefs_changed();
        }
        if fin::ib(ui, &p, list_btn, Id::new("side-list"), "list", Look::on_box(!sleeves), "List").clicked() && sleeves {
            self.prefs.library_view = LibraryView::List;
            self.prefs_changed();
        }
        if fin::ib(ui, &p, plus, Id::new("side-plus"), "plus", Look::Plain, "New playlist").clicked() {
            self.new_playlist = Some(String::new());
        }
        y = tools.bottom() + 8.0;

        if rest.is_empty() && !self.library_loaded && let Some(err) = self.library_error.clone() {
            // Nothing to show: why, and a way to try again.
            let area = Rect::from_min_size(pos2(left + 16.0, y + 4.0), vec2(width - 32.0, 160.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Min)));
            motion::content_in(&mut child, Id::new("playlists-failed"));
            child.label(egui::RichText::new(err).font(regular(12.0)).color(p.text_weak));
            child.add_space(8.0);
            if super::kit::button(&mut child, &p, "Try again", super::kit::Kind::Secondary).clicked() {
                self.library_error = None;
                self.send(Command::RefreshLibrary);
            }
            y = child.min_rect().bottom() + 12.0;
        } else if rest.is_empty() && !self.library_loaded {
            // Nothing at first, then placeholder rows.
            let rows = Rect::from_min_size(pos2(left + 8.0, y), vec2(width - 16.0, 36.0 * 5.0));
            motion::loading(ui, Id::new("playlists-loading"), rows, 36.0, true, &p);
            y = rows.bottom();
        } else if rest.is_empty() {
            let text = if needle.is_empty() {
                "No playlists here".to_owned()
            } else {
                format!("No playlists here match “{}”", self.playlist_search.trim())
            };
            ui.painter().text(pos2(left + 16.0, y + 10.0), Align2::LEFT_TOP, text, regular(12.0), p.text_weak);
            y += 44.0;
        } else {
            // What has just come fades in.
            let was = motion::content_in(ui, Id::new("playlists"));
            y = if sleeves { self.sleeve_grid_fin(ui, left, width, y, &rest, false) } else { self.sleeve_list_fin(ui, left, width, y, &rest) };
            ui.set_opacity(was);
        }
        // Room under the last row to bring it out from under the fade, only when the lists are taller
        // than the sidebar: lists that fit don't scroll.
        let height = y - ui.cursor().top();
        let room = if height > ui.clip_rect().height() { 24.0 } else { 0.0 };
        ui.allocate_space(vec2(width, (height + room).max(0.0)));
    }

    /// A section heading (`.sec`): 30 tall, small spaced capitals, and whatever goes on its right.
    fn side_section(&mut self, ui: &mut Ui, left: f32, width: f32, y: f32, title: &str, right: impl FnOnce(&mut Self, &mut Ui, Rect) -> bool) -> f32 {
        let p = self.palette;
        let sec = Rect::from_min_size(pos2(left, y), vec2(width, 30.0));
        let g = fin::spaced(ui, &title.to_uppercase(), bold(10.5), p.text_weak, 0.95);
        let gh = g.size().y;
        ui.painter().galley(pos2(sec.left() + 16.0, sec.center().y - gh / 2.0), g, p.text_weak);
        if title == "Pinned" {
            // Edit: the grid's edit mode, where sleeves drag to new places and the × unpins; Done ends it.
            let editing = self.pin_edit.on;
            let g2 = ui.fonts_mut(|f| f.layout_no_wrap(if editing { "Done" } else { "Edit" }.to_owned(), medium(11.0), p.text_weak));
            let er = Rect::from_min_size(pos2(sec.right() - 12.0 - g2.size().x, sec.center().y - g2.size().y / 2.0), g2.size());
            let resp = ui.interact(er, Id::new("pinned-edit"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            ui.painter().galley_with_override_text_color(er.min, g2, if editing { p.accent_ink } else if resp.hovered() { p.text } else { p.text_weak });
            if resp.on_hover_text(if editing { "Drag a sleeve to move it; × unpins it" } else { "Move or unpin the pinned playlists" }).clicked() {
                self.pin_edit.on = !editing;
                self.pin_edit.drag = None;
            }
        }
        right(self, ui, sec);
        sec.bottom()
    }

    /// All / Mine / Others (`.mt3`), right-aligned in the section heading.
    fn mine_tabs(&mut self, ui: &mut Ui, sec: Rect) -> bool {
        let p = self.palette;
        let mut x = sec.right() - 12.0;
        let mut changed = false;
        let tabs = [(PlaylistFilter::Others, "Others"), (PlaylistFilter::Mine, "Mine"), (PlaylistFilter::All, "All")];
        let galleys = tabs.map(|(_, name)| ui.fonts_mut(|f| f.layout_no_wrap(name.to_owned(), medium(11.0), p.text_weak)));
        let rects = galleys.each_ref().map(|g| {
            let w = g.size().x + 16.0;
            let rect = Rect::from_min_size(pos2(x - w, sec.center().y - (g.size().y + 6.0) / 2.0), vec2(w, g.size().y + 6.0));
            x = rect.left() - 2.0;
            rect
        });
        // The pill slides to the one picked, under the words.
        let chosen = tabs.iter().position(|&(filter, _)| self.prefs.playlist_filter == filter).map(|k| (k, rects[k]));
        motion::marker(ui.ctx(), Id::new("mt3-pill"), sec.right_top(), chosen, motion::TAB, |r, a| {
            ui.painter().rect_filled(r, CornerRadius::same(5), p.widget_bg.gamma_multiply(a));
        });
        for (((filter, name), g), rect) in tabs.into_iter().zip(galleys).zip(rects) {
            let on = self.prefs.playlist_filter == filter;
            let resp = ui.interact(rect, Id::new(("mt3", name)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let color = motion::color(ui.ctx(), resp.id, fin::tab_ink(&p, on, resp.hovered()), motion::HOVER);
            ui.painter().galley_with_override_text_color(pos2(rect.left() + 8.0, rect.top() + 3.0), g, color);
            if resp.clicked() && !on {
                self.prefs.playlist_filter = filter;
                self.prefs_changed();
                self.refilter_playlists();
                changed = true;
            }
        }
        changed
    }

    /// Sleeves in three columns (`.pgrid` / `.pc`): a 62 sleeve, the name and the number of songs. The
    /// Pinned grid in its edit mode (`edit`): a dragged sleeve follows the mouse, lifted, and the others
    /// slide out of its way; letting go keeps that order. Liked Songs stays first while it is pinned.
    fn sleeve_grid_fin(&mut self, ui: &mut Ui, left: f32, width: f32, y: f32, items: &[Sleeve], edit: bool) -> f32 {
        let inner_left = left + 10.0;
        // Three across in the sidebar; as many of about 112 as fit on the narrow Library page.
        let cols = (((width - 20.0) / 112.0).floor() as usize).max(3);
        let cell_w = ((width - 20.0) - 2.0 * (cols - 1) as f32) / cols as f32;
        // Measured from the prototype: rows 116 apart (2 of them the gap).
        let cell_h = 114.0 + (cell_w - 92.0).max(0.0);
        let slots: Vec<Rect> = (0..items.len())
            .map(|i| Rect::from_min_size(pos2(inner_left + (i % cols) as f32 * (cell_w + 2.0), y + (i / cols) as f32 * (cell_h + 2.0)), vec2(cell_w, cell_h)))
            .collect();
        // Being dragged: which pin, from where in its cell, and the slot its cell is nearest now.
        let drag = self.pin_edit.drag.filter(|&(from, _)| edit && from < items.len());
        let pointer = ui.input(|i| i.pointer.latest_pos());
        let to = drag.zip(pointer).map(|((from, grab), at)| {
            let first = usize::from(items.first().is_some_and(|s| s.source == Source::LikedSongs));
            nearest_slot(&slots, at - grab + slots[from].size() / 2.0).max(first)
        });
        let order = match drag.zip(to) {
            Some(((from, _), to)) => reorder(items.len(), from, to),
            None => (0..items.len()).collect(),
        };
        let mut on_top = OnTop::default();
        for (slot, &k) in order.iter().enumerate() {
            if drag.is_some_and(|(from, _)| from == k) {
                continue;
            }
            // In the edit mode the cells slide to where they go.
            let min = if edit { motion::slide(ui.ctx(), Id::new(("pin-slot", &items[k].source)), slots[slot].min.to_vec2(), motion::TAB).to_pos2() } else { slots[slot].min };
            let cell = Rect::from_min_size(min, slots[slot].size());
            if !ui.is_rect_visible(cell) {
                continue;
            }
            on_top.add(self.sleeve_card(ui, cell, &items[k], edit.then_some(k)));
        }
        on_top.lift(ui.ctx());
        if let (Some((from, grab)), Some(at)) = (drag, pointer) {
            // The dragged sleeve, lifted, under the mouse, over everything.
            let cell = Rect::from_min_size(at - grab, slots[from].size());
            let layer = egui::LayerId::new(egui::Order::Tooltip, Id::new("pin-drag"));
            let ghost = ui.new_child(egui::UiBuilder::new().layer_id(layer).max_rect(cell));
            ghost.painter().rect_filled(cell, CornerRadius::same(6), self.palette.hover_bg);
            let s = &items[from];
            let (open, playing) = (self.view.as_ref().is_some_and(|v| v.source == s.source), self.is_playing_source(&s.source));
            self.sleeve_body(&ghost, cell, s, open, playing, true);
            if ui.input(|i| i.pointer.primary_released() || !i.pointer.primary_down()) {
                self.pin_edit.drag = None;
                self.pins_reordered(items, &order);
            } else {
                ui.ctx().request_repaint();
            }
        }
        if items.is_empty() { y } else { slots.last().map_or(y, |r| r.bottom() + 6.0) }
    }

    /// The pinned playlists in the order the edit mode's drag left `items` (Liked Songs isn't in the
    /// list: it stays first while pinned). Pins not shown (filtered out) keep their place at the end.
    fn pins_reordered(&mut self, items: &[Sleeve], order: &[usize]) {
        let mut uris: Vec<String> = order.iter().filter_map(|&k| match &items[k].source { Source::Playlist(u) => Some(u.clone()), _ => None }).collect();
        let unseen: Vec<String> = self.prefs.pinned_playlists.iter().filter(|u| !uris.contains(u)).cloned().collect();
        uris.extend(unseen);
        if uris != self.prefs.pinned_playlists {
            self.prefs.pinned_playlists = uris;
            self.prefs_changed();
        }
    }

    /// Unpins a sleeve (the edit mode's ×).
    fn unpin(&mut self, source: &Source) {
        match source {
            Source::LikedSongs => {
                self.prefs.liked_pinned = false;
                self.prefs_changed();
            }
            Source::Playlist(uri) => self.set_pinned(uri, false),
            _ => {}
        }
    }

    /// A sleeve in a grid cell. In the Pinned grid's edit mode (`edit`: its place among the pins) a click
    /// doesn't open it, dragging it starts moving it, and the × over the cover's corner unpins it.
    fn sleeve_card(&mut self, ui: &mut Ui, cell: Rect, s: &Sleeve, edit: Option<usize>) -> Option<Raised> {
        let p = self.palette;
        let id = Id::new(("sleeve", &s.source));
        let sense = if edit.is_some() { Sense::click_and_drag() } else { Sense::click() };
        let cursor = if edit.is_some() { CursorIcon::Grab } else { CursorIcon::PointingHand };
        let resp = ui.interact(cell, id, sense).on_hover_cursor(cursor).on_hover_text(&s.name);
        let open = self.view.as_ref().is_some_and(|v| v.source == s.source);
        let playing = self.is_playing_source(&s.source);
        if open {
            fin::paint_chosen(ui.painter(), &p, cell);
        } else if resp.hovered() {
            ui.painter().rect_filled(cell, CornerRadius::same(6), p.hover_bg);
        }
        let (raised, cover) = self.sleeve_body(ui, cell, s, open, playing, resp.hovered());
        match edit {
            None => self.sleeve_actions(ui, &resp, s),
            Some(index) => {
                // Liked Songs stays first: it isn't dragged.
                if resp.drag_started_by(egui::PointerButton::Primary)
                    && s.source != Source::LikedSongs
                    && let Some(at) = resp.interact_pointer_pos()
                {
                    self.pin_edit.drag = Some((index, at - cell.min));
                }
                if unpin_badge(ui, &p, cover, id).clicked() {
                    self.unpin(&s.source);
                }
            }
        }
        raised
    }

    /// The cover with its record, the name and the count in `cell`: what the sleeve painted for
    /// [`OnTop`], and the cover's rect.
    fn sleeve_body(&mut self, ui: &Ui, cell: Rect, s: &Sleeve, open: bool, playing: bool, hovered: bool) -> (Option<Raised>, Rect) {
        let p = self.palette;
        // 62 in the sidebar's cells (92 wide); bigger in wider cells.
        let side = (62.0 + (cell.width() - 92.0).max(0.0)).min(96.0);
        let cover = Rect::from_min_size(pos2(cell.left() + 7.0, cell.top() + 7.0), vec2(side, side));
        let raised = self.paint_sleeve(ui, cover, s, playing, hovered, 0.20);
        let text_w = cell.width() - 7.0 - 4.0;
        let name = one_line(ui, &s.name, if open { bold(11.5) } else { medium(11.5) }, if open { p.text_strong } else { p.text }, text_w, false);
        let nh = name.size().y;
        ui.painter().galley(pos2(cover.left(), cover.bottom() + 6.0 + 1.0), name, p.text);
        if let Some(c) = s.count {
            // The number rolls when it changes; " songs" follows it.
            let at = pos2(cover.left(), cover.bottom() + 6.0 + 1.0 + nh + 1.0);
            let n = motion::count(ui.painter(), Id::new(("sleeve-count", &s.source)), c as usize, at, Align2::LEFT_TOP, |n| ui.painter().layout_no_wrap(fmt_thousands(n), regular(10.5), p.text_weak));
            ui.painter().text(pos2(n.right(), at.y), Align2::LEFT_TOP, " songs", regular(10.5), p.text_weak);
        }
        (raised, cover)
    }

    /// The playlist list (`.plist` / `.pr`): 36-tall rows with a 28 sleeve, the name and the count.
    fn sleeve_list_fin(&mut self, ui: &mut Ui, left: f32, width: f32, y: f32, items: &[Sleeve]) -> f32 {
        let p = self.palette;
        let mut y = y;
        for s in items {
            let row = Rect::from_min_size(pos2(left + 8.0, y), vec2(width - 16.0, 36.0));
            y += 37.0;
            if !ui.is_rect_visible(row) {
                continue;
            }
            let resp = ui.interact(row, Id::new(("sleeve-row", &s.source)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let open = self.view.as_ref().is_some_and(|v| v.source == s.source);
            if open {
                fin::paint_chosen(ui.painter(), &p, row);
            } else if resp.hovered() {
                ui.painter().rect_filled(row, CornerRadius::same(6), p.hover_bg);
            }
            let cover = Rect::from_min_size(pos2(row.left() + 8.0, row.center().y - 14.0), vec2(28.0, 28.0));
            let playing = self.is_playing_source(&s.source);
            self.paint_sleeve(ui, cover, s, playing, resp.hovered(), 0.30);
            let count = s.count.map(|c| fmt_thousands(c as usize)).unwrap_or_default();
            let cg = ui.fonts_mut(|f| f.layout_no_wrap(count, regular(11.0), p.text_weak));
            let name_left = cover.right() + 28.0 * 0.24 + 5.0 + 9.0;
            let name_w = row.right() - 8.0 - cg.size().x - 9.0 - name_left;
            let name = one_line(ui, &s.name, medium(12.0), p.text, name_w.max(20.0), false);
            let nh = name.size().y;
            ui.painter().galley(pos2(name_left, row.center().y - nh / 2.0), name, p.text);
            if let Some(c) = s.count {
                let at = pos2(row.right() - 8.0, row.center().y);
                motion::count(ui.painter(), Id::new(("row-count", &s.source)), c as usize, at, Align2::RIGHT_CENTER, |n| ui.painter().layout_no_wrap(fmt_thousands(n), regular(11.0), p.text_weak));
            }
            self.sleeve_actions(ui, &resp, s);
        }
        y + 4.0
    }

    /// A click opens the playlist; songs dropped on one of the user's playlists go into it; the right-click
    /// menu pins, edits or removes it.
    fn sleeve_actions(&mut self, ui: &Ui, resp: &egui::Response, s: &Sleeve) {
        let p = self.palette;
        if resp.clicked() {
            self.open_source(s.source.clone(), false);
        }
        if s.source == Source::LikedSongs {
            let pinned = self.prefs.liked_pinned;
            let mut flip = false;
            resp.context_menu(|ui| {
                ui.set_width(210.0);
                if fin::menu_item(ui, Some("pin"), if pinned { "Unpin" } else { "Pin to the top" }, None, false, true).clicked() {
                    flip = true;
                    ui.close();
                }
            });
            if flip {
                self.prefs.liked_pinned = !pinned;
                self.prefs_changed();
            }
        }
        if let Some(uri) = s.uri.clone() {
            if s.editable {
                if resp.dnd_hover_payload::<SongsDrag>().is_some() {
                    ui.painter().rect_stroke(resp.rect, CornerRadius::same(6), Stroke::new(2.0, p.accent_ink), StrokeKind::Inside);
                }
                if let Some(songs) = resp.dnd_release_payload::<SongsDrag>() {
                    self.add_dropped_songs(uri.clone(), songs.uris.clone());
                }
            }
            let pinned = self.prefs.pinned_playlists.contains(&uri);
            let editable = s.editable;
            let mut menu: Option<RowMenu> = None;
            resp.context_menu(|ui| playlist_menu(ui, &uri, pinned, editable, &mut menu));
            if let Some(menu) = menu {
                self.apply_row_menu(menu);
            }
        }
    }

    pub(super) fn is_playing_source(&self, source: &Source) -> bool {
        matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading) && self.playback.source.as_ref() == Some(source)
    }

    /// A sleeve (`.sl`) with its cover at `cover`: the record behind it peeking out on the right (`rest` of
    /// the cover's width in; 46 % on hover; 48 % and an accent ring while it plays), sliding between.
    /// Hovered, the sleeve lifts toward the user ([`lifted`]).
    /// Hovered, playing, or with its record sliding back in, it says what it painted, for [`OnTop`].
    pub(super) fn paint_sleeve(&mut self, ui: &Ui, cover: Rect, s: &Sleeve, playing: bool, hovered: bool, rest: f32) -> Option<Raised> {
        let p = self.palette;
        let id = Id::new(("vn", &s.source, cover.width() as i32));
        // Hovered, the sleeve lifts toward the user: a little bigger, its shadow longer (not with fades
        // only: it would jump).
        let rise = motion::toward(ui.ctx(), id.with("lift"), hovered && motion::moves(), motion::TAB);
        let (cover, shadow) = lifted(cover, rise);
        let size = cover.width();
        let target = if playing { 0.48 } else if hovered { 0.46 } else { rest };
        // With fades only it doesn't slide: it fades out where it was as it fades in at its new place.
        let at = motion::slide(ui.ctx(), id, vec2(target, 0.0), motion::TAB).x;
        let (was, fade) = if motion::moves() { (at, 1.0) } else { motion::crossfade(ui.ctx(), id, target, motion::TAB) };
        let back = [(was, 1.0 - fade), (at, fade)].iter().any(|&(x, shown)| shown > 0.0 && x > target + 0.001);
        // Still a little lifted on its way down, it stays over its neighbours too.
        let lift = Lift::of(hovered, back || rise > 0.001, playing);
        let from = lift.map(|_| Raised::mark(ui));
        let d = size * 0.92;
        // The label: the playing song's cover on the playing sleeve, else the playlist's own.
        let label_url = if playing {
            self.playback.track.as_ref().and_then(|t| t.cover_small.clone().or_else(|| t.cover_large.clone()))
        } else {
            s.image.clone()
        };
        let label = label_url.as_deref().and_then(|u| self.images.get(u, d * 0.4)).map(|t| t.id);
        let fill = if s.source == Source::LikedSongs { liked_color(&p) } else { p.widget_bg };
        for (at, opacity) in [(was, 1.0 - fade), (at, fade)].into_iter().filter(|r| r.1 > 0.0) {
            let mut painter = ui.painter().clone();
            painter.multiply_opacity(opacity);
            fin::sleeve_record(&painter, pos2(cover.left() + size * at + d / 2.0, cover.top() + size * 0.04 + d / 2.0), d, label, fill);
        }
        // The cover, with its shadow falling right onto the record (and below it, lifted).
        ui.painter().add(shadow.as_shape(cover, CornerRadius::same(4)));
        if s.source == Source::LikedSongs {
            ui.painter().rect_filled(cover, CornerRadius::same(4), liked_color(&p));
            let h = cover.width() * 0.42;
            svgicon::paint(ui.painter(), Rect::from_center_size(cover.center(), vec2(h, h)), "heartf", Color32::WHITE, svgicon::STROKE);
        } else if !self.images.paint(ui, s.image.as_deref(), cover, 4, p.widget_bg) {
            let h = (cover.width() * 0.4).max(10.0);
            svgicon::paint(ui.painter(), Rect::from_center_size(cover.center(), vec2(h, h)), "note", p.text_weak, svgicon::STROKE);
        }
        ui.painter().rect_stroke(cover, CornerRadius::same(4), Stroke::new(1.0, Color32::from_white_alpha(18)), StrokeKind::Inside);
        if playing {
            ui.painter().rect_stroke(cover, CornerRadius::same(4), Stroke::new(2.0, p.accent_ink), StrokeKind::Outside);
            if size >= 44.0 {
                let badge = Rect::from_min_size(pos2(cover.left() + 5.0, cover.bottom() - 5.0 - 18.0), vec2(18.0, 18.0));
                ui.painter().rect_filled(badge, CornerRadius::same(4), Color32::from_black_alpha(158));
                svgicon::paint(ui.painter(), badge.shrink(3.0), "eq", p.on_dark().accent_ink, svgicon::STROKE);
            }
        }
        from.zip(lift).map(|(from, lift)| Raised { layer: ui.layer_id(), from, to: Raised::mark(ui), lift })
    }
}

#[cfg(test)]
mod tests {
    use egui::epaint::ClippedShape;
    use egui::{Color32, CornerRadius, Pos2, Rect, Shape, Ui, pos2, vec2};

    use super::{Lift, OnTop, Raised, lifted, nearest_slot, reorder};
    use crate::ui::fin;
    use crate::ui::kit::headless::drawn;

    /// The record's rim (`fin::sleeve_record`).
    const RIM: Color32 = Color32::from_rgb(58, 58, 64);
    const COVERS: [Color32; 3] = [Color32::RED, Color32::GREEN, Color32::BLUE];
    /// The page's part of the window: the shapes keep it as their clip.
    const PAGE: Rect = Rect { min: Pos2 { x: 0.0, y: 0.0 }, max: Pos2 { x: 500.0, y: 300.0 } };

    /// A sleeve at `x` as `paint_sleeve` draws it: the record coming out to the right, then the
    /// cover (100 wide). What it painted when lifted.
    fn sleeve(ui: &Ui, x: f32, cover: Color32, lift: Option<Lift>) -> Option<Raised> {
        let from = Raised::mark(ui);
        fin::sleeve_record(ui.painter(), pos2(x + 46.0 + 46.0, 60.0), 92.0, None, cover);
        ui.painter().rect_filled(Rect::from_min_size(pos2(x, 10.0), vec2(100.0, 100.0)), CornerRadius::same(4), cover);
        lift.map(|lift| Raised { layer: ui.layer_id(), from, to: Raised::mark(ui), lift })
    }

    /// None lifted.
    const PLAIN: [Option<Lift>; 3] = [None; 3];

    /// One pass of a row of three sleeves 110 apart, lifted as in `lifts` after the row: the
    /// shapes in the order drawn.
    fn row(lifts: [Option<Lift>; 3]) -> Vec<ClippedShape> {
        drawn(&egui::Context::default(), Vec::new(), |ui| {
            ui.set_clip_rect(PAGE);
            let mut on_top = OnTop::default();
            for (k, cover) in COVERS.into_iter().enumerate() {
                on_top.add(sleeve(ui, 10.0 + k as f32 * 110.0, cover, lifts[k]));
            }
            on_top.lift(ui.ctx());
        })
    }

    /// Where the `k`th sleeve's cover and record rim are among `shapes`.
    fn at(shapes: &[ClippedShape], k: usize) -> (usize, usize) {
        let cover = shapes.iter().position(|c| matches!(&c.shape, Shape::Rect(r) if r.fill == COVERS[k])).expect("cover");
        let x = 10.0 + k as f32 * 110.0 + 92.0;
        let rim = shapes.iter().position(|c| matches!(&c.shape, Shape::Circle(r) if r.fill == RIM && (r.center.x - x).abs() < 0.5)).expect("record");
        (cover, rim)
    }

    #[test]
    fn a_hovered_record_lies_over_the_next_cover() {
        // As before: each cover over the record of the one before it.
        let plain = row(PLAIN);
        assert!(at(&plain, 0).1 < at(&plain, 1).0);
        // The first hovered: its record (and its cover) over the second's cover, still in order.
        let shapes = row([Some(Lift::Hovered), None, None]);
        let (first, second) = (at(&shapes, 0), at(&shapes, 1));
        assert!(first.1 > second.0 && first.0 > second.0, "{first:?} {second:?}");
        assert!(first.1 < first.0, "its record went over its own cover");
        // Clipped as it was, and nothing lost or doubled.
        assert!(shapes.iter().all(|c| c.clip_rect == PAGE || matches!(c.shape, Shape::Noop)));
        let drawn = |s: &[ClippedShape]| s.iter().filter(|c| !matches!(c.shape, Shape::Noop)).count();
        assert_eq!(drawn(&shapes), drawn(&plain));
    }

    #[test]
    fn of_two_lifted_alike_the_first_ends_on_top() {
        // Both sliding back in: the first's record over the second's cover, the second's over the
        // third's.
        let shapes = row([Some(Lift::Back), Some(Lift::Back), None]);
        let (first, second, third) = (at(&shapes, 0), at(&shapes, 1), at(&shapes, 2));
        assert!(first.1 > second.0, "{first:?} {second:?}");
        assert!(second.1 > third.0, "{second:?} {third:?}");
        // The last of a row has no one to lie over: lifted, nothing else changes.
        let last = row([None, None, Some(Lift::Hovered)]);
        assert!(at(&last, 1).1 < at(&last, 2).0 && at(&last, 0).1 < at(&last, 1).0);
        // None lifted: none moved.
        assert_eq!(row(PLAIN).len(), row(PLAIN).iter().filter(|c| !matches!(c.shape, Shape::Noop)).count());
    }

    #[test]
    fn the_hovered_goes_over_everything_and_the_playing_over_the_rest() {
        // Playing: its record over the next cover, as when hovered.
        let playing = row([Some(Lift::Playing), None, None]);
        assert!(at(&playing, 0).1 > at(&playing, 1).0);
        // The one after it hovered: that one over the playing record, and its record over the third.
        let shapes = row([Some(Lift::Playing), Some(Lift::Hovered), None]);
        let (first, second, third) = (at(&shapes, 0), at(&shapes, 1), at(&shapes, 2));
        assert!(second.0 > first.1 && second.1 > third.0, "{first:?} {second:?} {third:?}");
        // One still sliding back in stays over the playing one until it's in.
        let back = row([Some(Lift::Back), Some(Lift::Playing), None]);
        assert!(at(&back, 0).1 > at(&back, 1).0);
        // Which sleeves go up, and how high.
        assert_eq!(Lift::of(false, false, true), Some(Lift::Playing));
        assert_eq!(Lift::of(true, false, true), Some(Lift::Hovered));
        assert_eq!(Lift::of(false, true, true), Some(Lift::Back));
        assert_eq!(Lift::of(false, false, false), None);
    }

    #[test]
    fn a_hovered_sleeve_lifts_toward_the_user() {
        let cover = Rect::from_min_size(pos2(10.0, 10.0), vec2(100.0, 100.0));
        let (same, shadow) = lifted(cover, 0.0);
        assert_eq!((same, shadow.offset, shadow.blur, shadow.color.a()), (cover, [3, 0], 8, 90));
        let (up, shadow) = lifted(cover, 1.0);
        assert_eq!(up.center(), cover.center());
        assert!((up.width() - 104.0).abs() < 1e-3);
        assert_eq!((shadow.offset, shadow.blur, shadow.color.a()), ([3, 6], 18, 150));
    }

    #[test]
    fn dragging_a_pin_moves_it_and_closes_the_gap() {
        assert_eq!(reorder(5, 3, 1), vec![0, 3, 1, 2, 4]);
        assert_eq!(reorder(5, 1, 3), vec![0, 2, 3, 1, 4]);
        assert_eq!(reorder(5, 2, 2), vec![0, 1, 2, 3, 4]);
        assert_eq!(reorder(5, 1, 9), vec![0, 2, 3, 4, 1]);
        let slots: Vec<Rect> = (0..3).map(|i| Rect::from_min_size(pos2(i as f32 * 100.0, 0.0), vec2(90.0, 110.0))).collect();
        assert_eq!(nearest_slot(&slots, pos2(160.0, 50.0)), 1);
        assert_eq!(nearest_slot(&slots, pos2(-50.0, 50.0)), 0);
        assert_eq!(nearest_slot(&slots, pos2(400.0, 50.0)), 2);
    }
}
