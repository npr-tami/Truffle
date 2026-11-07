//! Listen along in the UI: the side panel's Friends tab (who plays what, Listen along, Join with a
//! link, and your own links from `links.rs`, in the layout picked: Card, List or Switch, the
//! `friends-tab.html` mockup), the pill above the player bar while listening along, and its
//! Listen along buttons.

use std::time::{Duration, Instant};

use egui::epaint::{RectShape, Shadow};
use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Order, Popup, Rect, RectAlign, Sense, Shape, Stroke, StrokeKind, Ui, ViewportId, pos2, vec2};

use super::fin::{self, Look};
use super::frame::{self, Mode};
use super::motion;
use super::panel::Tab;
use super::svgicon;
use super::theme::{bold, medium, regular};
use super::tracks::one_line;
use super::{MiddleTab, TruffleApp};
use crate::config::FriendsLayout;
use crate::model::{Command, Friend, ListenHow};

/// The "playing now" dot (the mockup's `--ok`).
const LIVE: Color32 = Color32::from_rgb(79, 209, 139);
/// The Friends list is asked for again this often while the tab shows.
const FRIENDS_EVERY: Duration = Duration::from_secs(15);
/// Another user's profile asks what they play this often while it shows.
const PEEK_EVERY: Duration = Duration::from_secs(30);
/// A person's row, and a link's.
pub(super) const ROW_H: f32 = 54.0;
const JOIN_H: f32 = 76.0;
const JOIN_BOX_ID: &str = "listen-join";
/// The Switch layout's strip with "Friends" | "Your listeners".
const SIDES_H: f32 = 48.0;
/// A section's heading (`.sec`).
const SEC_H: f32 = 31.0;
/// The switch's size (`.sw`).
const SWITCH: egui::Vec2 = vec2(36.0, 20.0);
/// The layouts as the layout button's menu lists them: the layout, its name and its icon.
const LAYOUTS: [(FriendsLayout, &str, &str); 3] =
    [(FriendsLayout::Card, "Card", "card"), (FriendsLayout::List, "List", "list"), (FriendsLayout::Switch, "Switch", "split")];

/// A part of the Friends tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    /// "Friends" | "Your listeners" (Switch), kept at the top.
    Sides,
    /// "Sharing your music" with the sharing switch, the summary and MANAGE LINKS (Card);
    /// `open`: the links show in it.
    Card { open: bool },
    /// "YOUR LINKS" with the sharing switch (List).
    LinksHead,
    /// "SHARING" with the switch, and who hears what (Switch's Your listeners).
    SharingHead,
    /// The links' rows and New link.
    Links,
    /// "FRIENDS".
    FriendsHead,
    /// The people to listen along with.
    Friends,
    /// Join with a link, kept at the foot.
    Join,
}

/// What the Friends tab shows in `layout`, top to bottom (`listeners`: Switch is on Your
/// listeners; `open`: the card's links are open).
fn parts(layout: FriendsLayout, listeners: bool, open: bool) -> Vec<Part> {
    match layout {
        FriendsLayout::Card => vec![Part::Card { open }, Part::FriendsHead, Part::Friends, Part::Join],
        FriendsLayout::List => vec![Part::LinksHead, Part::Links, Part::FriendsHead, Part::Friends, Part::Join],
        FriendsLayout::Switch if listeners => vec![Part::Sides, Part::SharingHead, Part::Links],
        FriendsLayout::Switch => vec![Part::Sides, Part::Friends, Part::Join],
    }
}

/// A layout's name and icon.
fn layout_look(layout: FriendsLayout) -> (&'static str, &'static str) {
    LAYOUTS.iter().find(|l| l.0 == layout).map_or(("Card", "card"), |l| (l.1, l.2))
}

/// "just now", "5 min ago", "2 h ago", "3 d ago".
pub fn ago(at_ms: i64, now_ms: i64) -> String {
    let mins = (now_ms - at_ms).max(0) / 60_000;
    match mins {
        0 => "just now".to_owned(),
        m if m < 60 => format!("{m} min ago"),
        m if m < 48 * 60 => format!("{} h ago", m / 60),
        m => format!("{} d ago", m / (24 * 60)),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// A small round button (`.la`): "Listen along", or "Listening" in the accent while on.
fn listen_button(ui: &Ui, p: &super::theme::Palette, right: f32, cy: f32, id: Id, on: bool) -> egui::Response {
    let text = if on { "Listening" } else { "Listen along" };
    let font = if on { bold(11.0) } else { medium(11.0) };
    let g = ui.painter().layout_no_wrap(text.to_owned(), font, p.text);
    let rect = Rect::from_min_size(pos2(right - g.size().x - 20.0, cy - 13.0), vec2(g.size().x + 20.0, 26.0));
    let resp = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let r = CornerRadius::same(13);
    let (fill, line, color) = match (on, resp.hovered()) {
        (true, _) => (p.accent, Color32::TRANSPARENT, p.on_accent),
        (false, true) => (fin::acc(p, 0.16), fin::acc(p, 0.32), p.text_strong),
        (false, false) => (p.widget_bg, p.border, p.text),
    };
    ui.painter().rect_filled(rect, r, fill);
    if line != Color32::TRANSPARENT {
        ui.painter().rect_stroke(rect, r, Stroke::new(1.0, line), StrokeKind::Inside);
    }
    ui.painter().galley_with_override_text_color(pos2(rect.left() + 10.0, rect.center().y - g.size().y / 2.0), g, color);
    resp.on_hover_text(if on { "Stop listening along" } else { "Hear what they play, as they play it" })
}

impl TruffleApp {
    /// Listening along with someone: the play controls are theirs.
    pub(super) fn listening(&self) -> bool {
        self.listening.is_some()
    }

    fn listening_to(&self, how: &ListenHow) -> bool {
        self.listening.as_ref().is_some_and(|l| l.how == *how)
    }

    /// Listen along with them, or stop if already.
    pub(super) fn toggle_listen(&mut self, how: ListenHow, name: String) {
        if self.listening_to(&how) {
            self.send(Command::ListenStop);
        } else {
            self.send(Command::ListenAlong { how, name });
        }
    }

    /// The Friends tab in the layout picked (see [`parts`]): people from Spotify's friend activity
    /// and the Truffle friends joined with a link, playing ones first; your links and sharing;
    /// "Join with a link" at the foot.
    pub(super) fn panel_friends(&mut self, ui: &mut Ui, body: Rect) {
        if self.friends_asked.is_none_or(|at| at.elapsed() >= FRIENDS_EVERY) {
            self.friends_asked = Some(Instant::now());
            self.send(Command::Friends);
        }
        ui.ctx().request_repaint_after(FRIENDS_EVERY);
        let parts = parts(self.prefs.friends_layout, self.listeners_shown, self.prefs.friends_links_open);
        let mut top = body.top();
        if parts.contains(&Part::Sides) {
            let strip = Rect::from_min_size(body.min, vec2(body.width(), SIDES_H));
            self.sides_switch(ui, strip);
            top = strip.bottom();
        }
        let join_h = if parts.contains(&Part::Join) { JOIN_H } else { 0.0 };
        let list_rect = Rect::from_min_max(pos2(body.left(), top), pos2(body.right(), (body.bottom() - join_h).max(top)));
        let mut list_ui = ui.new_child(egui::UiBuilder::new().id_salt("friends-list").max_rect(list_rect));
        list_ui.set_clip_rect(list_rect.intersect(ui.clip_rect()));
        let (left, width) = (list_rect.left() + 8.0, list_rect.width() - 16.0);
        egui::ScrollArea::vertical().id_salt("friends-scroll").auto_shrink(false).show(&mut list_ui, |ui| {
            let start = ui.cursor().top();
            let mut y = start;
            for &part in &parts {
                y = match part {
                    Part::Card { open } => self.sharing_card(ui, left, width, y, open),
                    Part::LinksHead => self.friends_head(ui, left, width, y, "YOUR LINKS", Some("Sharing")),
                    Part::SharingHead => {
                        let y = self.friends_head(ui, left, width, y, "SHARING", Some(if self.listen_share { "On" } else { "Off" }));
                        let note = "Friends with a link hear what you play. Only you see their names.";
                        let g = ui.painter().layout(note.to_owned(), regular(11.5), self.palette.text_weak, width - 16.0);
                        let gh = g.size().y;
                        ui.painter().galley(pos2(left + 8.0, y + 4.0), g, self.palette.text_weak);
                        y + 4.0 + gh + 8.0
                    }
                    Part::Links => self.link_rows(ui, left, width, y),
                    Part::FriendsHead => self.friends_head(ui, left, width, y + 4.0, "FRIENDS", None),
                    Part::Friends => self.friend_rows(ui, left, width, y),
                    Part::Sides | Part::Join => y,
                };
            }
            ui.allocate_space(vec2(width, (y - start + 8.0).max(0.0)));
        });
        if join_h > 0.0 {
            self.join_box(ui, Rect::from_min_max(pos2(body.left(), list_rect.bottom()), body.max));
        }
    }

    /// The Card layout's card (`.card`): "Sharing your music" with the sharing switch, the summary
    /// under it, and MANAGE LINKS, which opens and closes the links right in the card (kept for
    /// next time). Returns its bottom.
    fn sharing_card(&mut self, ui: &mut Ui, left: f32, width: f32, y: f32, open: bool) -> f32 {
        let p = self.palette;
        // Its fill goes under what is drawn in it, once its height is known.
        let fill = ui.painter().add(Shape::Noop);
        let top = y + 12.0;
        let (inner_l, inner_r) = (left + 12.0, left + width - 12.0);
        let sw = Rect::from_min_size(pos2(inner_r - SWITCH.x, top + 12.0), SWITCH);
        let title = one_line(ui, "Sharing your music", bold(13.0), p.text_strong, (sw.left() - 10.0 - inner_l).max(20.0), false);
        let th = title.size().y;
        ui.painter().galley(pos2(inner_l, sw.center().y - th / 2.0), title, p.text_strong);
        self.share_switch(ui, sw);
        let summary = one_line(ui, &self.links_summary(), regular(11.5), p.text_weak, inner_r - inner_l, false);
        let sy = sw.bottom() + 3.0;
        let sh = summary.size().y;
        ui.painter().galley(pos2(inner_l, sy), summary, p.text_weak);
        // MANAGE LINKS ▾ (▴ while open).
        let my = sy + sh + 10.0;
        let word = fin::spaced(ui, "MANAGE LINKS", bold(11.0), p.accent_ink, 0.66);
        let (ww, wh) = (word.size().x, word.size().y);
        let hit = Rect::from_min_size(pos2(inner_l, my), vec2(ww + 4.0 + 12.0, wh)).expand(4.0);
        let tip = if open { "Hide your links" } else { "Show your links" };
        let resp = ui.interact(hit, Id::new("friends-manage-links"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
        // Hovered: further from the background.
        let color = if resp.hovered() { fin::brighten(p.accent_ink, if p.dark { 1.15 } else { 0.85 }) } else { p.accent_ink };
        ui.painter().galley_with_override_text_color(pos2(inner_l, my), word, color);
        let arrow = Rect::from_min_size(pos2(inner_l + ww + 4.0, my + wh / 2.0 - 6.0), vec2(12.0, 12.0));
        svgicon::paint(ui.painter(), arrow, if open { "chevu" } else { "chev" }, color, svgicon::STROKE);
        if resp.clicked() {
            self.prefs.friends_links_open = !open;
            self.prefs_changed();
        }
        let bottom = if open { self.link_rows(ui, left + 4.0, width - 8.0, my + wh + 10.0) + 4.0 } else { my + wh + 12.0 };
        let card = Rect::from_min_max(pos2(left, top), pos2(left + width, bottom));
        ui.painter().set(fill, RectShape::new(card, CornerRadius::same(10), p.raised_bg, Stroke::new(1.0, p.field_border), StrokeKind::Inside));
        card.bottom()
    }

    /// A section's heading (`.sec`): `title` in small capitals, and with a `label` ("Sharing",
    /// "On") the sharing switch after it on the right. Returns its bottom.
    fn friends_head(&mut self, ui: &Ui, left: f32, width: f32, y: f32, title: &str, label: Option<&str>) -> f32 {
        let p = self.palette;
        let g = fin::spaced(ui, title, bold(10.5), p.text_weak, 0.75);
        let cy = y + 12.0 + g.size().y / 2.0;
        ui.painter().galley(pos2(left + 8.0, cy - g.size().y / 2.0), g, p.text_weak);
        if let Some(label) = label {
            let sw = Rect::from_min_size(pos2(left + width - 8.0 - SWITCH.x, cy - SWITCH.y / 2.0), SWITCH);
            self.share_switch(ui, sw);
            let l = ui.painter().layout_no_wrap(label.to_owned(), medium(11.0), p.text);
            ui.painter().galley(pos2(sw.left() - 7.0 - l.size().x, cy - l.size().y / 2.0), l, p.text);
        }
        y + SEC_H
    }

    /// The Switch layout's two-part switch (`.seg`) in `strip`: "Friends" | "Your listeners".
    fn sides_switch(&mut self, ui: &Ui, strip: Rect) {
        let p = self.palette;
        let seg = Rect::from_min_max(pos2(strip.left() + 8.0, strip.top() + 12.0), pos2(strip.right() - 8.0, strip.bottom() - 4.0));
        ui.painter().rect_filled(seg, CornerRadius::same(8), p.hover_bg);
        let half = (seg.width() - 6.0) / 2.0;
        let part = |i: usize| Rect::from_min_size(pos2(seg.left() + 3.0 + i as f32 * half, seg.top() + 3.0), vec2(half, seg.height() - 6.0));
        // The raised half slides to the side picked.
        let chosen = usize::from(self.listeners_shown);
        motion::marker(ui.ctx(), Id::new("friends-side-pill"), seg.min, Some((chosen, part(chosen))), motion::TAB, |r, a| {
            ui.painter().rect_filled(r, CornerRadius::same(6), (if p.dark { p.widget_hover_bg } else { p.raised_bg }).gamma_multiply(a));
        });
        for (i, (word, listeners)) in [("Friends", false), ("Your listeners", true)].into_iter().enumerate() {
            let r = part(i);
            let on = self.listeners_shown == listeners;
            let resp = ui.interact(r, Id::new(("friends-side", i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let color = motion::color(ui.ctx(), resp.id, fin::tab_ink(&p, on, resp.hovered()), motion::HOVER);
            let g = one_line(ui, word, if on { bold(12.0) } else { medium(12.0) }, color, half - 8.0, false);
            ui.painter().galley(r.center() - g.size() / 2.0, g, color);
            if resp.clicked() {
                self.listeners_shown = listeners;
            }
        }
    }

    /// The Friends tab's layout button (on the tabs' strip while it shows), with the layout's
    /// icon: its menu picks Card, List or Switch.
    pub(super) fn friends_layout_button(&mut self, ui: &Ui, rect: Rect) {
        let p = self.palette;
        let now = self.prefs.friends_layout;
        let id = Id::new("friends-layout");
        let open = Popup::is_id_open(ui.ctx(), id.with("popup"));
        let (name, icon) = layout_look(now);
        let b = fin::ib_icon(ui, &p, rect, id, icon, if open { Look::Open } else { Look::Plain }, &format!("Layout: {name}"), 14.0);
        let mut pick = None;
        Popup::menu(&b).align(RectAlign::BOTTOM_END).show(|ui| {
            ui.set_width(150.0);
            for (layout, name, icon) in LAYOUTS {
                if fin::menu_item(ui, Some(icon), name, None, layout == now, true).clicked() {
                    pick = Some(layout);
                    ui.close();
                }
            }
        });
        if let Some(layout) = pick.filter(|&l| l != now) {
            self.prefs.friends_layout = layout;
            self.prefs_changed();
        }
    }

    /// Shows the Friends tab: in the side panel (opened if closed), or on the Playing page when
    /// the window is narrow.
    pub(super) fn show_friends(&mut self) {
        self.frame.panel_tab = Tab::Friends;
        let w = self.ctx.input_for(ViewportId::ROOT, |i| i.content_rect().width());
        match frame::mode_of(w) {
            Mode::Wide => self.prefs.show_rules_panel = true,
            Mode::Medium => self.prefs.panel_medium = true,
            Mode::Narrow => self.middle_tab = MiddleTab::Playing,
        }
        self.prefs_changed();
        self.ctx.request_repaint_of(ViewportId::ROOT);
    }

    /// The people to listen along with from `y` down, or why there is nobody. Returns the bottom.
    fn friend_rows(&mut self, ui: &mut Ui, left: f32, width: f32, y: f32) -> f32 {
        let p = self.palette;
        let Some(list) = self.friends.clone() else {
            ui.painter().text(pos2(left + 8.0, y + 22.0), Align2::LEFT_CENTER, "Looking for friends…", medium(12.0), p.text_weak);
            return y + 44.0;
        };
        if list.is_empty() {
            let y = y + 16.0;
            ui.painter().text(pos2(left + 8.0, y), Align2::LEFT_TOP, "Nobody to listen along with yet", medium(12.5), p.text);
            let note = "People you follow show up here while they play music on Spotify. A friend on Truffle can send you their link.";
            let g = ui.painter().layout(note.to_owned(), regular(11.5), p.text_weak, width - 16.0);
            let gh = g.size().y;
            ui.painter().galley(pos2(left + 8.0, y + 22.0), g, p.text_weak);
            return y + 22.0 + gh + 8.0;
        }
        let now = now_ms();
        let mut act: Option<(ListenHow, String)> = None;
        let mut forget: Option<String> = None;
        let mut profile: Option<String> = None;
        let mut y = y + 4.0;
        for f in list.iter() {
            let row = Rect::from_min_size(pos2(left, y), vec2(width, ROW_H));
            match self.friend_row(ui, row, f, now) {
                Some(RowAction::Listen) => act = Some((f.how.clone(), f.name.clone())),
                Some(RowAction::Profile(u)) => profile = Some(u),
                Some(RowAction::Forget(uri)) => forget = Some(uri),
                None => {}
            }
            y += ROW_H + 2.0;
        }
        if let Some((how, name)) = act {
            self.toggle_listen(how, name);
        }
        if let Some(uri) = forget {
            self.send(Command::ListenForget(uri));
        }
        if let Some(user) = profile {
            self.show_user(&user);
        }
        y
    }

    /// One person (`.fr`): the round picture with a green dot while they play, their name, what
    /// they play, and Listen along. Rows of people not playing now are dimmed.
    fn friend_row(&mut self, ui: &mut Ui, rect: Rect, f: &Friend, now: i64) -> Option<RowAction> {
        let p = self.palette;
        let on = self.listening_to(&f.how);
        let resp = ui.interact(rect, Id::new(("friend-row", &f.how)), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(8), p.hover_bg);
        }
        let dim = |c: Color32| if f.live || on { c } else { c.gamma_multiply(0.55) };
        let pic = Rect::from_min_size(pos2(rect.left() + 8.0, rect.center().y - 19.0), vec2(38.0, 38.0));
        if !self.images.paint(ui, f.image.as_deref(), pic, 19, p.widget_bg) {
            ui.painter().circle_filled(pic.center(), 19.0, dim(p.widget_bg));
            svgicon::paint(ui.painter(), pic.shrink(11.0), "person", dim(p.text_weak), svgicon::STROKE);
        }
        if f.live {
            let c = pos2(pic.right() - 4.5, pic.bottom() - 4.5);
            ui.painter().circle_filled(c, 7.5, p.sidebar_bg);
            ui.painter().circle_filled(c, 5.5, LIVE);
        }
        let mut right = rect.right() - 8.0;
        let mut action = None;
        if f.live || on {
            let b = listen_button(ui, &p, right, rect.center().y, Id::new(("friend-listen", &f.how)), on);
            right = b.rect.left() - 10.0;
            if b.clicked() {
                action = Some(RowAction::Listen);
            }
        }
        let x = pic.right() + 10.0;
        let tw = (right - x).max(20.0);
        let name = one_line(ui, &f.name, bold(12.5), dim(p.text_strong), tw, false);
        let what = match (f.song.is_empty(), f.live) {
            (false, true) if f.artist.is_empty() => f.song.clone(),
            (false, true) => format!("{} · {}", f.song, f.artist),
            (_, false) if f.at_ms > 0 => format!("Last played {}", ago(f.at_ms, now)),
            _ => "Not playing".to_owned(),
        };
        let eq_w = if f.live { 10.0 + 5.0 } else { 0.0 };
        let sub = one_line(ui, &what, regular(11.0), dim(p.text_weak), (tw - eq_w).max(10.0), false);
        let h = name.size().y + 2.0 + sub.size().y;
        let top = rect.center().y - h / 2.0;
        let nh = name.size().y;
        ui.painter().galley(pos2(x, top), name, p.text_strong);
        let sy = top + nh + 2.0;
        if f.live {
            let eq = Rect::from_min_size(pos2(x, sy + sub.size().y / 2.0 - 5.0), vec2(10.0, 10.0));
            svgicon::paint(ui.painter(), eq, "eq", p.accent_ink, svgicon::STROKE);
        }
        ui.painter().galley(pos2(x + eq_w, sy), sub, p.text_weak);
        let tip = match &f.how {
            ListenHow::Friend(_) => "From Spotify's friend activity",
            ListenHow::Channel(_) => "On Truffle, joined with their link",
        };
        let resp = resp.on_hover_cursor(CursorIcon::PointingHand).on_hover_text(tip);
        if resp.clicked()
            && action.is_none()
            && let ListenHow::Friend(user) = &f.how
        {
            action = Some(RowAction::Profile(user.clone()));
        }
        resp.context_menu(|ui| {
            ui.set_width(200.0);
            let word = if on { "Stop listening along" } else { "Listen along" };
            if fin::menu_item(ui, Some("eq"), word, None, false, f.live || on).clicked() {
                action = Some(RowAction::Listen);
                ui.close();
            }
            match &f.how {
                ListenHow::Friend(user) => {
                    if fin::menu_item(ui, Some("person"), "Open profile", None, false, true).clicked() {
                        action = Some(RowAction::Profile(user.clone()));
                        ui.close();
                    }
                }
                ListenHow::Channel(uri) => {
                    if fin::menu_item(ui, Some("x"), "Forget", None, false, true).clicked() {
                        action = Some(RowAction::Forget(uri.clone()));
                        ui.close();
                    }
                }
            }
        });
        action
    }

    /// "Join with a link" (`.srch`-like box and JOIN): a link a friend on Truffle sent.
    fn join_box(&mut self, ui: &mut Ui, rect: Rect) {
        let p = self.palette;
        ui.painter().hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, p.field_border));
        let left = rect.left() + 16.0;
        let width = rect.width() - 32.0;
        let head = fin::spaced(ui, "JOIN WITH A LINK", bold(10.5), p.text_weak, 0.95);
        let hh = head.size().y;
        ui.painter().galley(pos2(left, rect.top() + 12.0), head, p.text_weak);
        let row_y = rect.top() + 12.0 + hh + 8.0;
        let join_w = fin::text_button_w(ui, "Join", false);
        let field = Rect::from_min_size(pos2(left, row_y), vec2((width - join_w - 8.0).max(60.0), 28.0));
        let id = Id::new(JOIN_BOX_ID);
        let focused = ui.memory(|m| m.has_focus(id));
        ui.painter().rect_filled(field, CornerRadius::same(6), p.field_bg);
        ui.painter().rect_stroke(field, CornerRadius::same(6), Stroke::new(1.0, if focused { p.accent_ink } else { p.field_border }), StrokeKind::Inside);
        let edit = egui::TextEdit::singleline(&mut self.join_input)
            .id(id)
            .frame(egui::Frame::new())
            .hint_text(egui::RichText::new("Paste a friend's link").font(regular(12.0)))
            .font(regular(12.0))
            .text_color(p.text)
            .margin(egui::Margin::symmetric(0, 7))
            .vertical_align(egui::Align::Center);
        let resp = ui.put(field.shrink2(vec2(10.0, 0.0)), edit);
        let entered = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let can = !self.join_input.trim().is_empty();
        let join = fin::text_button(ui, &p, pos2(field.right() + 8.0, row_y), Id::new("listen-join-go"), "Join", None, true, can);
        if (join.clicked() || entered) && can {
            let link = std::mem::take(&mut self.join_input);
            self.send(Command::ListenJoin(link.trim().to_owned()));
        }
    }

    /// While listening along: "Listening along with <name>  STOP", centred on `cx` above `bottom`.
    pub(super) fn listen_pill(&mut self, ctx: &egui::Context, cx: f32, bottom: f32) {
        let Some(status) = self.listening.clone() else { return };
        let p = self.palette;
        let mut stop = false;
        egui::Area::new(Id::new("listen-pill")).order(Order::Foreground).fixed_pos(pos2(0.0, 0.0)).fade_in(false).interactable(true).show(ctx, |ui| {
            let screen = ctx.content_rect();
            let lead = ui.painter().layout_no_wrap("Listening along with ".to_owned(), regular(12.0), p.text);
            let name = one_line(ui, &status.name, bold(12.0), p.text_strong, 220.0, false);
            let word = ui.painter().layout_no_wrap("Stop".to_owned(), medium(11.0), p.text);
            let stop_w = word.size().x + 20.0;
            let inner = 12.0 + 8.0 + 10.0 + lead.size().x + name.size().x + 10.0 + stop_w + 4.0;
            let x = (cx - inner / 2.0).clamp(screen.left() + 8.0, (screen.right() - 8.0 - inner).max(screen.left() + 8.0));
            let pill = Rect::from_min_size(pos2(x, bottom - 30.0), vec2(inner, 30.0));
            let _ = ui.interact(pill, Id::new("listen-pill-body"), Sense::click());
            let round = CornerRadius::same(15);
            ui.painter().add(Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(128) }.as_shape(pill, round));
            ui.painter().rect_filled(pill, round, p.raised_bg);
            ui.painter().rect_stroke(pill, round, Stroke::new(1.0, fin::acc(&p, 0.32)), StrokeKind::Inside);
            let dot = pos2(pill.left() + 12.0 + 4.0, pill.center().y);
            ui.painter().circle_filled(dot, 7.0, fin::acc(&p, 0.18));
            ui.painter().circle_filled(dot, 4.0, p.accent_ink);
            let mut tx = dot.x + 4.0 + 10.0;
            let (lw, lh) = (lead.size().x, lead.size().y);
            ui.painter().galley(pos2(tx, pill.center().y - lh / 2.0), lead, p.text);
            tx += lw;
            let nh = name.size().y;
            ui.painter().galley(pos2(tx, pill.center().y - nh / 2.0), name, p.text_strong);
            let b = Rect::from_min_size(pos2(pill.right() - 4.0 - stop_w, pill.center().y - 11.0), vec2(stop_w, 22.0));
            let resp = ui.interact(b, Id::new("listen-pill-stop"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            ui.painter().rect_filled(b, CornerRadius::same(11), if resp.hovered() { p.widget_hover_bg } else { p.widget_bg });
            let wh = word.size().y;
            ui.painter().galley(pos2(b.left() + 10.0, b.center().y - wh / 2.0), word, p.text);
            if resp.on_hover_text("Stop listening along; your music comes back").clicked() {
                stop = true;
            }
        });
        if stop {
            self.send(Command::ListenStop);
        }
    }

    /// What another user plays now (for their profile's Listen along), asked for now and then.
    pub(super) fn peek_friend(&mut self, username: &str) -> Option<Friend> {
        let due = self.friend_peek.get(username).is_none_or(|(at, _)| at.elapsed() >= PEEK_EVERY);
        if due {
            let known = self.friend_peek.get(username).and_then(|(_, f)| f.clone());
            self.friend_peek.insert(username.to_owned(), (Instant::now(), known));
            self.send(Command::FriendPeek(username.to_owned()));
        }
        self.friend_peek.get(username).and_then(|(_, f)| f.clone())
    }
}

enum RowAction {
    Listen,
    Profile(String),
    Forget(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_played_reads_plainly() {
        let now = 10_000_000_000;
        assert_eq!(ago(now - 20_000, now), "just now");
        assert_eq!(ago(now - 5 * 60_000, now), "5 min ago");
        assert_eq!(ago(now - 2 * 3_600_000, now), "2 h ago");
        assert_eq!(ago(now - 3 * 86_400_000, now), "3 d ago");
        assert_eq!(ago(now + 60_000, now), "just now");
    }

    #[test]
    fn each_layout_shows_its_parts() {
        use FriendsLayout::{Card, List, Switch};
        // Card: the card (its links in it only while open), the friends, Join at the foot.
        assert_eq!(parts(Card, false, false), [Part::Card { open: false }, Part::FriendsHead, Part::Friends, Part::Join]);
        assert_eq!(parts(Card, true, true), [Part::Card { open: true }, Part::FriendsHead, Part::Friends, Part::Join]);
        // List: your links with the switch, then the friends.
        assert_eq!(parts(List, true, false), [Part::LinksHead, Part::Links, Part::FriendsHead, Part::Friends, Part::Join]);
        // Switch: the friends and Join, or your listeners: sharing and the links, no Join.
        assert_eq!(parts(Switch, false, true), [Part::Sides, Part::Friends, Part::Join]);
        assert_eq!(parts(Switch, true, true), [Part::Sides, Part::SharingHead, Part::Links]);
    }

    #[test]
    fn every_layout_has_its_name_and_an_icon() {
        assert_eq!(LAYOUTS.map(|l| l.1), ["Card", "List", "Switch"]);
        for (layout, name, icon) in LAYOUTS {
            assert_eq!(layout_look(layout), (name, icon));
            assert!(super::super::icon_svg::ICONS.iter().any(|i| i.0 == icon), "{icon}");
        }
        assert_eq!(FriendsLayout::default(), FriendsLayout::Card);
    }
}
