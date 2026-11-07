//! Your listen-along links, as the Friends tab shows them (`friends.rs`): the sharing switch, one
//! row per friend's link (kick, let back in, the ⋯ menu), the New link row, the card's summary,
//! and the New, Rename and Remove dialogs (the `listeners.html` mockup's "Tonight" steps).
//! Everything goes to the controller as Commands; the rows show what it sends back
//! (`Event::ListenLinks`). A friend's name never leaves this PC.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use egui::text::{CCursor, CCursorRange, LayoutJob, TextWrapping};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Id, Key, Popup, Rect, RectAlign, Response, RichText, Sense, Shape, Stroke, TextEdit,
    TextFormat, Ui, Vec2, ViewportId, pos2, vec2,
};

use crate::listen::links::{MAX_LINKS, NAME_MAX};
use crate::model::{Command, LinkState, LinkView};
use crate::ui::{AuthState, TruffleApp};
use crate::ui::fin::{self, Look};
use crate::ui::friends::ROW_H;
use crate::ui::theme::{Palette, bold, medium, regular};
use crate::ui::tracks::one_line;

/// A row reads "Link copied" this long after its link was copied.
const COPIED_FOR: Duration = Duration::from_secs(2);
/// Between a row's buttons, and the ⋯ button's size.
const GAP: f32 = 6.0;
const MORE: f32 = 26.0;
/// A link's switch (`.sw`).
const SWITCH: Vec2 = vec2(36.0, 20.0);
/// A row's round picture (as a friend's).
const PIC: f32 = 38.0;
/// The soft colours of the links' circles (the mockup's), one per name.
const PASTELS: [Color32; 5] = [
    Color32::from_rgb(243, 166, 200),
    Color32::from_rgb(159, 183, 255),
    Color32::from_rgb(201, 178, 255),
    Color32::from_rgb(158, 224, 192),
    Color32::from_rgb(255, 210, 138),
];

/// A link dialog open over the window.
pub(in crate::ui) enum LinkDialog {
    /// New link, with the name typed so far.
    New(String),
    /// Rename a link, with the name typed so far.
    Rename(u32, String),
    /// "Remove …'s link?" (`shown`: it was on screen before this pass, so Enter means yes;
    /// the Enter that opened it from a keyboard-focused REMOVE doesn't).
    Remove { id: u32, shown: bool },
}

impl LinkDialog {
    /// The link it is about (none for a new one).
    pub(in crate::ui) fn link(&self) -> Option<u32> {
        match self {
            LinkDialog::New(_) => None,
            LinkDialog::Rename(id, _) | LinkDialog::Remove { id, .. } => Some(*id),
        }
    }
}

/// What a row asked for.
#[derive(Debug, PartialEq, Eq)]
enum RowAct {
    Copy(String),
    Kick(bool),
    Remake,
    Rename,
    AskRemove,
}

/// What a link's row shows besides the link itself.
struct RowLook {
    sharing: bool,
    /// Its link was just copied.
    copied: bool,
    now_ms: i64,
    /// This PC's clock minus UTC, for the date it was made.
    offset_ms: i64,
}

impl TruffleApp {
    /// Sharing on or off.
    pub(super) fn set_listen_share(&mut self, on: bool) {
        self.listen_share = on;
        self.send(Command::ListenShare(on));
    }

    /// The sharing switch at `rect` (greyed while logged out).
    pub(super) fn share_switch(&mut self, ui: &Ui, rect: Rect) {
        let p = self.palette;
        let tip = "Friends with a link hear what you play, as you play it";
        if fin::switch(ui, &p, rect, Id::new("listen-share"), self.listen_share, self.auth == AuthState::LoggedIn, tip) {
            self.set_listen_share(!self.listen_share);
        }
    }

    /// The links, once logged in. Logged out (or still logging in) they aren't known yet: none
    /// are shown, and nothing says there are none.
    fn known_links(&self) -> Option<Arc<Vec<LinkView>>> {
        (self.auth == AuthState::LoggedIn).then(|| self.listen_links.clone())
    }

    /// The Card's line under "Sharing your music" (see [`summary`]).
    pub(super) fn links_summary(&self) -> String {
        summary(self.listen_share, self.known_links().as_deref().map(Vec::as_slice))
    }

    /// The links' rows and the New link row, from `y` down, `left` to `left + width`. Returns
    /// the bottom.
    pub(super) fn link_rows(&mut self, ui: &mut Ui, left: f32, width: f32, mut y: f32) -> f32 {
        let p = self.palette;
        let links = self.known_links();
        let (now_ms, offset_ms) = (now_ms(), local_offset_ms());
        for link in links.iter().flat_map(|l| l.iter()) {
            let copied = self.link_copied.is_some_and(|(id, at)| id == link.id && at.elapsed() < COPIED_FOR);
            let look = RowLook { sharing: self.listen_share, copied, now_ms, offset_ms };
            let rect = Rect::from_min_size(pos2(left, y), vec2(width, ROW_H));
            if let Some(act) = link_row(ui, &p, rect, link, &look) {
                self.do_row_act(ui, link, act);
            }
            y += ROW_H + 2.0;
        }
        let rect = Rect::from_min_size(pos2(left, y), vec2(width, ROW_H));
        if new_link_row(ui, &p, rect, links.map(|l| l.len())) {
            self.link_dialog = Some(LinkDialog::New(String::new()));
        }
        rect.bottom()
    }

    fn do_row_act(&mut self, ui: &Ui, link: &LinkView, act: RowAct) {
        match act {
            RowAct::Copy(address) => {
                ui.ctx().copy_text(address);
                self.link_copied = Some((link.id, Instant::now()));
                // "Link copied" goes back to what the row said by itself.
                ui.ctx().request_repaint_after(COPIED_FOR);
                ui.ctx().request_repaint_after_for(COPIED_FOR, ViewportId::ROOT);
                self.note(format!("{}'s link copied", link.name), None);
            }
            RowAct::Kick(kick) => self.send(Command::ListenKick { id: link.id, kick }),
            RowAct::Remake => self.send(Command::ListenRemakeLink(link.id)),
            RowAct::Rename => {
                ui.ctx().data_mut(|d| d.remove_temp::<bool>(rename_field(link.id).with("chosen")));
                self.link_dialog = Some(LinkDialog::Rename(link.id, link.name.clone()));
            }
            RowAct::AskRemove => self.link_dialog = Some(LinkDialog::Remove { id: link.id, shown: false }),
        }
    }

    /// The open link dialog, over the window: New link, Rename link, or Remove. Clicking outside
    /// or Esc cancels.
    pub(super) fn link_dialog_modal(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let names = self.listen_links.clone();
        let Some(dialog) = self.link_dialog.as_mut() else { return };
        let Some(command) = link_dialog_ui(ctx, &p, dialog, &names) else { return };
        self.link_dialog = None;
        if let Some(command) = command {
            if let Command::ListenRemoveLink(id) = command {
                // The row goes at once.
                std::sync::Arc::make_mut(&mut self.listen_links).retain(|l| l.id != id);
            }
            self.send(command);
        }
    }
}

/// One pass of a link dialog: `None` while it stays open, `Some(None)` to close it,
/// `Some(Some(command))` to send that and close it.
fn link_dialog_ui(ctx: &egui::Context, p: &Palette, dialog: &mut LinkDialog, names: &[LinkView]) -> Option<Option<Command>> {
    match dialog {
        LinkDialog::New(text) => {
            let id = Id::new("listen_new_link");
            let (mut ok, mut no) = (false, false);
            let modal = fin::dialog(ctx, id, |ui| {
                ui.set_width(360f32.min(ctx.content_rect().width() - 80.0).max(200.0));
                fin::dialog_head(ui, "New link", Some("Only you see the name."));
                fin::field_label(ui, "Who's it for?");
                ok |= name_field(ui, p, text, id.with("name"), false);
                let (a, b) = fin::dialog_actions(ui, p, id, "Make link", !text.trim().is_empty());
                ok |= a;
                no |= b;
            });
            if ok && !text.trim().is_empty() {
                Some(Some(Command::ListenNewLink { name: text.trim().to_owned() }))
            } else {
                (no || modal.should_close()).then_some(None)
            }
        }
        LinkDialog::Rename(link, text) => {
            let id = Id::new(("listen_rename_link", *link));
            let (mut ok, mut no) = (false, false);
            let modal = fin::dialog(ctx, id, |ui| {
                ui.set_width(360f32.min(ctx.content_rect().width() - 80.0).max(200.0));
                fin::dialog_head(ui, "Rename link", Some("Only you see the name."));
                ui.add_space(14.0);
                ok |= name_field(ui, p, text, rename_field(*link), true);
                let (a, b) = fin::dialog_actions(ui, p, id, "Save", !text.trim().is_empty());
                ok |= a;
                no |= b;
            });
            if ok && !text.trim().is_empty() {
                Some(Some(Command::ListenRenameLink { id: *link, name: text.trim().to_owned() }))
            } else {
                (no || modal.should_close()).then_some(None)
            }
        }
        LinkDialog::Remove { id: link, shown } => {
            let link = *link;
            let Some(view) = names.iter().find(|l| l.id == link) else { return Some(None) };
            let id = Id::new(("listen_remove_link", link));
            // Only an Enter pressed while it shows (not held down from before) says yes, and only
            // while no button has the keyboard's focus: one that has it takes the Enter itself
            // (a focused CANCEL cancels).
            let focused = ctx.memory(|m| m.focused()).is_some();
            let enter = *shown && !focused && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::Enter, pressed: true, repeat: false, .. })));
            *shown = true;
            let (mut yes, mut no) = (false, false);
            let modal = fin::dialog(ctx, id, |ui| {
                ui.set_width(400f32.min(ctx.content_rect().width() - 80.0).max(200.0));
                let note = "Their link stops working for good. To let them back later, make a new link.";
                fin::dialog_head(ui, &format!("Remove {}'s link?", view.name), Some(note));
                let (a, b) = fin::dialog_actions(ui, p, id, "Remove", true);
                yes |= a || enter;
                no |= b;
            });
            // Cancel wins: removing can't be undone.
            if no {
                Some(None)
            } else if yes {
                Some(Some(Command::ListenRemoveLink(link)))
            } else {
                modal.should_close().then_some(None)
            }
        }
    }
}

/// The Card's line under "Sharing your music": "2 links · Sam paused", "No links yet" or
/// "Sharing is off · 2 links"; logged out (`None`), "Log in to see your links".
fn summary(sharing: bool, links: Option<&[LinkView]>) -> String {
    let Some(links) = links else { return "Log in to see your links".to_owned() };
    let count = match links.len() {
        0 => return "No links yet".to_owned(),
        1 => "1 link".to_owned(),
        n => format!("{n} links"),
    };
    if !sharing {
        return format!("Sharing is off · {count}");
    }
    let mut parts = vec![count];
    let kicked: Vec<&str> = links.iter().filter(|l| l.state == LinkState::Kicked).map(|l| l.name.as_str()).collect();
    match kicked.as_slice() {
        [] => {}
        [one] => parts.push(format!("{one} paused")),
        [a, b] => parts.push(format!("{a} and {b} paused")),
        more => parts.push(format!("{} paused", more.len())),
    }
    let gone = links.iter().filter(|l| l.state == LinkState::Gone).count();
    if gone > 0 {
        parts.push(format!("{gone} stopped working"));
    }
    parts.join(" · ")
}

/// What a link's row says under its name.
fn status(link: &LinkView, look: &RowLook) -> &'static str {
    match link.state {
        _ if look.copied => "Link copied",
        LinkState::Making => "Making…",
        LinkState::Gone => "This link stopped working",
        LinkState::Kicked => "Paused · hears nothing",
        LinkState::Ready if look.sharing => "Can listen",
        LinkState::Ready => "Sharing is off",
    }
}

/// One link (`.row`, as a friend's): its initial on a soft circle, its name and what it can do
/// (one line each, cut short with "…"), and on its right ⋯ then its switch (on: they can listen;
/// off: paused, they hear nothing); on a gone link, × (remove) then NEW LINK. A paused one is
/// dimmed. A right-click on it opens the ⋯ menu.
fn link_row(ui: &mut Ui, p: &Palette, rect: Rect, link: &LinkView, look: &RowLook) -> Option<RowAct> {
    // Clicked with the right button only: Tab goes straight to its buttons.
    let row = ui.interact(rect, Id::new(("listen-link-row", link.id)), Sense::CLICK);
    if row.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.hover_bg);
    }
    let cy = rect.center().y;
    let side = Rect::from_center_size(pos2(rect.right() - 8.0 - MORE / 2.0, cy), vec2(MORE, MORE));
    let mut right = rect.right() - 8.0;
    let mut act = None;
    match link.state {
        LinkState::Ready | LinkState::Kicked => {
            let more_id = more_button(link.id);
            // Lit while its menu is open (`.ib.open`).
            let open = Popup::is_id_open(ui.ctx(), more_id.with("popup"));
            let more = fin::ib_icon(ui, p, side, more_id, "more", if open { Look::Open } else { Look::Plain }, "More", 14.0);
            // Under the ⋯, ending at its right edge, as in the mockup (it opens to the left if
            // there is no room below).
            Popup::menu(&more).align(RectAlign::BOTTOM_END).show(|ui| link_menu(ui, link, &mut act));
            let ready = link.state == LinkState::Ready;
            let tip = if ready { "Turn off to pause them. Same link when you turn it back on" } else { "Paused: they hear nothing. Turn on to let them listen" };
            let sw = Rect::from_center_size(pos2(side.left() - GAP - SWITCH.x / 2.0, cy), SWITCH);
            let id = Id::new(("listen-link-flip", link.id));
            right = sw.left();
            if fin::switch(ui, p, sw, id, ready, true, tip) && flip_counts(ui, id) {
                act = Some(RowAct::Kick(ready));
            }
        }
        LinkState::Gone => {
            if fin::ib_icon(ui, p, side, Id::new(("listen-link-remove", link.id)), "x", Look::Plain, "Remove", 14.0).clicked() {
                act = Some(RowAct::AskRemove);
            }
            let tip = format!("A new link for {}. Send it again", link.name);
            let remake = small_button(ui, p, side.left() - GAP, cy, Id::new(("listen-link-remake", link.id)), "New link", false).on_hover_text(tip);
            right = remake.rect.left();
            if remake.clicked() {
                act = Some(RowAct::Remake);
            }
        }
        LinkState::Making => {}
    }
    let dim = link.state == LinkState::Kicked;
    let fade = |c: Color32| if dim { c.gamma_multiply(0.5) } else { c };
    let pic = Rect::from_min_size(pos2(rect.left() + 8.0, cy - PIC / 2.0), vec2(PIC, PIC));
    initial_circle(ui, pic, &link.name, dim);
    let x = pic.right() + 10.0;
    let tw = (right - 10.0 - x).max(20.0);
    let name = one_line(ui, &link.name, bold(12.5), fade(p.text_strong), tw, false);
    let sub = one_line(ui, status(link, look), regular(11.0), fade(p.text_weak), tw, false);
    let (nh, sh) = (name.size().y, sub.size().y);
    let top = cy - (nh + 2.0 + sh) / 2.0;
    let cut = name.elided;
    ui.painter().galley(pos2(x, top), name, p.text_strong);
    ui.painter().galley(pos2(x, top + nh + 2.0), sub, p.text_weak);
    // On hover: the whole name when it is cut short, and when the link was made.
    let made = (link.made_ms > 0).then(|| format!("Link {}", made_words(link.made_ms, look.now_ms, look.offset_ms)));
    let tip: Vec<&str> = [cut.then_some(link.name.as_str()), made.as_deref()].into_iter().flatten().collect();
    let row = if tip.is_empty() { row } else { row.on_hover_text(tip.join("\n")) };
    if matches!(link.state, LinkState::Ready | LinkState::Kicked) {
        row.context_menu(|ui| link_menu(ui, link, &mut act));
    }
    act
}

/// A link's menu (its ⋯, or a right-click on its row): Copy link, Rename, Remove.
fn link_menu(ui: &mut Ui, link: &LinkView, act: &mut Option<RowAct>) {
    ui.set_width(190.0);
    let tip = format!("Send it to {}. They paste it in Friends", link.name);
    if fin::menu_item(ui, Some("link"), "Copy link", None, false, link.link.is_some()).on_hover_text(tip).clicked() {
        *act = link.link.clone().map(RowAct::Copy);
        ui.close();
    }
    if fin::menu_item(ui, Some("pen"), "Rename", None, false, true).clicked() {
        *act = Some(RowAct::Rename);
        ui.close();
    }
    fin::menu_sep(ui);
    if fin::menu_item(ui, Some("x"), "Remove", None, false, true).clicked() {
        *act = Some(RowAct::AskRemove);
        ui.close();
    }
}

/// A link's round picture (`.av`): its first letter on a soft colour picked by its name.
fn initial_circle(ui: &Ui, pic: Rect, name: &str, dim: bool) {
    let k = if dim { 0.5 } else { 1.0 };
    let n = name.chars().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
    ui.painter().circle_filled(pic.center(), pic.width() / 2.0, PASTELS[n as usize % PASTELS.len()].gamma_multiply(k));
    let letter: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    ui.painter().text(pic.center(), Align2::CENTER_CENTER, letter, bold(15.0), Color32::from_rgb(17, 17, 17).gamma_multiply(k));
}

/// A row's button (`.b`: 24 tall, 10 either side, 10 px bold capitals), ending at `right`;
/// `primary` in the accent.
fn small_button(ui: &Ui, p: &Palette, right: f32, cy: f32, id: Id, text: &str, primary: bool) -> Response {
    let color = if primary { p.on_accent } else { p.text };
    let g = fin::spaced(ui, &text.to_uppercase(), bold(10.0), color, 0.6);
    let rect = Rect::from_min_max(pos2(right - g.size().x - 20.0, cy - 12.0), pos2(right, cy + 12.0));
    let resp = ui.interact(rect, id, Sense::click());
    let fill = match (primary, resp.hovered()) {
        (true, true) => fin::brighten(p.accent, 1.07),
        (true, false) => p.accent,
        (false, true) => p.widget_hover_bg,
        (false, false) => p.widget_bg,
    };
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    ui.painter().galley(pos2(rect.left() + 10.0, cy - g.size().y / 2.0), g, color);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// The New link row (`.add`): + on a dashed circle, "New link", and under it "No links yet. Make
/// one for each friend" (the + in the accent), "Each friend gets their own link. Up to 10", or,
/// greyed, "That's 10 links. Remove one first". Logged out (`count` `None`): "Log in to see your
/// links", greyed. Returns whether it was clicked.
fn new_link_row(ui: &mut Ui, p: &Palette, rect: Rect, count: Option<usize>) -> bool {
    let full = count.is_some_and(|n| n >= MAX_LINKS);
    let can = count.is_some() && !full;
    let resp = ui.interact(rect, Id::new("listen-new-link"), if can { Sense::click() } else { Sense::hover() });
    let hot = can && resp.hovered();
    if hot {
        ui.painter().rect_filled(rect, CornerRadius::same(8), p.hover_bg);
    }
    let (title, note) = match count {
        None => ("Log in to see your links", None),
        Some(0) => ("New link", Some("No links yet. Make one for each friend".to_owned())),
        Some(_) if full => ("New link", Some(format!("That's {MAX_LINKS} links. Remove one first"))),
        Some(_) => ("New link", Some(format!("Each friend gets their own link. Up to {MAX_LINKS}"))),
    };
    let pic = Rect::from_min_size(pos2(rect.left() + 8.0, rect.center().y - PIC / 2.0), vec2(PIC, PIC));
    let ink = if !can { p.text_faint } else if count == Some(0) { p.accent_ink } else if hot { p.text } else { p.text_weak };
    dashed_circle(ui, pic, if can { p.text_weak.gamma_multiply(0.6) } else { p.text_faint });
    ui.painter().text(pic.center(), Align2::CENTER_CENTER, "+", regular(18.0), ink);
    let x = pic.right() + 10.0;
    let tw = (rect.right() - 8.0 - x).max(20.0);
    let title_c = if !can { p.text_faint } else if hot { p.text } else { p.text_weak };
    let title = one_line(ui, title, medium(12.0), title_c, tw, false);
    let note = note.map(|n| {
        let mut job = LayoutJob::single_section(n, TextFormat::simple(regular(11.0), p.text_weak));
        job.wrap = TextWrapping { max_width: tw, max_rows: 2, break_anywhere: false, overflow_character: Some('…') };
        ui.painter().layout_job(job)
    });
    let th = title.size().y;
    let h = th + note.as_ref().map_or(0.0, |g| 2.0 + g.size().y);
    let top = rect.center().y - h / 2.0;
    ui.painter().galley(pos2(x, top), title, title_c);
    if let Some(g) = note {
        ui.painter().galley(pos2(x, top + th + 2.0), g, p.text_weak);
    }
    let resp = if can { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
    let resp = if can && count != Some(0) { resp.on_hover_text("Each friend gets their own link") } else { resp };
    can && resp.clicked()
}

/// A dashed circle just inside `rect` (`.add .pl`).
fn dashed_circle(ui: &Ui, rect: Rect, color: Color32) {
    let r = rect.width() / 2.0 - 0.5;
    let points: Vec<_> = (0..=48).map(|i| rect.center() + r * egui::Vec2::angled(i as f32 / 48.0 * std::f32::consts::TAU)).collect();
    ui.painter().extend(Shape::dashed_line(&points, Stroke::new(1.0, color), 3.0, 2.5));
}

/// Whether a click on a link's switch (`id`) counts: the second click of a double-click (or Enter
/// or Space held down on it) would flip it straight back, so those don't.
fn flip_counts(ui: &Ui, id: Id) -> bool {
    let held = ui.memory(|m| m.has_focus(id)) && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::Enter | Key::Space, repeat: true, .. })));
    let again = ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary) || i.pointer.button_triple_clicked(egui::PointerButton::Primary));
    !held && !again
}

/// A link's ⋯ button (its menu's id is this `.with("popup")`).
fn more_button(link: u32) -> Id {
    Id::new(("listen-link-more", link))
}

/// The Rename dialog's text box.
fn rename_field(link: u32) -> Id {
    Id::new(("listen_rename_field", link))
}

/// A name box (`char_limit` 30, focused), "30 characters at most" under it once that many are
/// typed. `choose_all`: all of it is chosen when it opens, so typing replaces it. Returns whether
/// Enter was pressed in it.
fn name_field(ui: &mut Ui, p: &Palette, text: &mut String, id: Id, choose_all: bool) -> bool {
    let hint = RichText::new("Name").color(p.text_weak);
    let r = fin::field(ui, p, TextEdit::singleline(text).id(id).hint_text(hint).char_limit(NAME_MAX).desired_width(f32::INFINITY));
    // Enter lets go of the box's focus: read that before taking the focus back for it. Only taken
    // when the box hasn't got it: asking every pass would cut an IME's composing short each time,
    // and let the arrow keys move the focus off the box.
    let entered = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    if !r.has_focus() {
        r.request_focus();
    }
    let chosen = id.with("chosen");
    if choose_all
        && r.has_focus()
        && !ui.data(|d| d.get_temp::<bool>(chosen).unwrap_or(false))
        && let Some(mut state) = TextEdit::load_state(ui.ctx(), id)
    {
        let all = CCursorRange::two(CCursor::new(0), CCursor::new(text.chars().count()));
        state.cursor.set_char_range(Some(all));
        state.store(ui.ctx(), id);
        ui.data_mut(|d| d.insert_temp(chosen, true));
    }
    if text.chars().count() >= NAME_MAX {
        ui.add_space(5.0);
        ui.label(RichText::new(format!("{NAME_MAX} characters at most")).font(regular(11.0)).color(p.text_weak));
    }
    entered
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "made today", "made 25 Oct", or "made 25 Oct 2025" in another year, by this PC's clock
/// (`offset_ms`: local time minus UTC).
fn made_words(made_ms: i64, now_ms: i64, offset_ms: i64) -> String {
    let day = |ms: i64| (ms + offset_ms).div_euclid(86_400_000);
    let (made, today) = (day(made_ms), day(now_ms));
    if made == today {
        return "made today".to_owned();
    }
    let (y, m, d) = civil_from_days(made);
    let month = MONTHS[(m - 1).clamp(0, 11) as usize];
    if y == civil_from_days(today).0 { format!("made {d} {month}") } else { format!("made {d} {month} {y}") }
}

/// Days since 1970 as (year, month 1-12, day) (Howard Hinnant's civil_from_days).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The days since 1970 of a date (the other way round).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// How far this PC's clock runs ahead of UTC (its time zone and summer time), to the minute.
fn local_offset_ms() -> i64 {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct SystemTime {
            year: u16,
            month: u16,
            weekday: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            ms: u16,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetLocalTime(t: *mut SystemTime);
        }
        let mut t = SystemTime::default();
        // SAFETY: GetLocalTime fills the struct it is given.
        unsafe { GetLocalTime(&mut t) };
        let days = days_from_civil(i64::from(t.year), i64::from(t.month), i64::from(t.day));
        let local = days * 86_400_000 + (i64::from(t.hour) * 3_600 + i64::from(t.minute) * 60 + i64::from(t.second)) * 1_000 + i64::from(t.ms);
        ((local - now_ms()) as f64 / 60_000.0).round() as i64 * 60_000
    }
    #[cfg(not(windows))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use egui::Pos2;

    use super::*;
    use crate::config::ThemeMode;
    use crate::ui::kit::headless::{key, move_to, pass, press, release, text};

    const DAY: i64 = 86_400_000;
    /// 25 Oct 2026, 12:00 UTC.
    const NOON: i64 = 1_792_929_600_000;
    const LINK: &str = "https://open.spotify.com/playlist/0000000000000000000001";
    /// A row 600 wide from x 0: the ⋯ (or ×) is 8 in from its right end, its one button 6
    /// before that.
    const ROW: Rect = Rect { min: Pos2::ZERO, max: Pos2 { x: 600.0, y: ROW_H } };
    const SIDE_X: f32 = 600.0 - 8.0 - MORE / 2.0;
    const BUTTON_RIGHT: f32 = 600.0 - 8.0 - MORE - GAP;
    /// The middle of a link's switch.
    const SWITCH_X: f32 = BUTTON_RIGHT - SWITCH.x / 2.0;

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        pass(&ctx, Vec::new(), |_| {});
        ctx
    }

    fn view(name: &str, state: LinkState) -> LinkView {
        let link = matches!(state, LinkState::Ready | LinkState::Kicked).then(|| LINK.to_owned());
        LinkView { id: 1, name: name.into(), link, state, made_ms: NOON - DAY, here_ms: None }
    }

    fn palette() -> Palette {
        Palette::build(ThemeMode::Dark, Color32::RED, None)
    }

    /// One pass of a row (`link`, or the New link row with `count` links, `None` logged out) at
    /// [`ROW`]: what it asked for.
    fn draw(ctx: &egui::Context, events: Vec<egui::Event>, link: Option<&LinkView>, count: Option<usize>) -> (Option<RowAct>, bool) {
        let p = palette();
        let look = RowLook { sharing: true, copied: false, now_ms: NOON, offset_ms: 0 };
        let mut out = (None, false);
        pass(ctx, events, |ui| match link {
            Some(l) => out.0 = link_row(ui, &p, ROW, l, &look),
            None => out.1 = new_link_row(ui, &p, ROW, count),
        });
        out
    }

    /// A click at (`x`, the middle of the row) on a link's row: what it asked for.
    fn click(ctx: &egui::Context, x: f32, link: &LinkView) -> Option<RowAct> {
        let y = ROW.center().y;
        draw(ctx, Vec::new(), Some(link), None);
        draw(ctx, move_to(pos2(x, y)), Some(link), None);
        draw(ctx, press(pos2(x, y)), Some(link), None);
        draw(ctx, release(pos2(x, y)), Some(link), None).0
    }

    /// One pass of `dialog` over a 600 x 400 window: what it asked for (see `link_dialog_ui`).
    fn dialog(ctx: &egui::Context, events: Vec<egui::Event>, dialog: &mut LinkDialog, names: &[LinkView]) -> Option<Option<Command>> {
        let p = palette();
        let mut out = None;
        pass(ctx, events, |ui| out = link_dialog_ui(ui.ctx(), &p, dialog, names));
        out
    }

    fn enter_up() -> Vec<egui::Event> {
        vec![egui::Event::Key { key: Key::Enter, physical_key: None, pressed: false, repeat: false, modifiers: egui::Modifiers::NONE }]
    }

    /// One pass of `dialog`: whether it asked to cut an IME's composing short.
    fn dialog_cuts_ime(ctx: &egui::Context, events: Vec<egui::Event>, dialog: &mut LinkDialog) -> bool {
        let p = palette();
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0))), events, ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| {
            link_dialog_ui(ui.ctx(), &p, dialog, &[]);
        });
        out.textures_delta.clear();
        out.platform_output.ime.is_some_and(|ime| ime.should_interrupt_composition)
    }

    /// The middle of the row's button (NEW LINK) with these words.
    fn button_x(ctx: &egui::Context, text: &str) -> f32 {
        let mut w = 0.0;
        pass(ctx, Vec::new(), |ui| w = fin::spaced(ui, &text.to_uppercase(), bold(10.0), Color32::PLACEHOLDER, 0.6).size().x + 20.0);
        BUTTON_RIGHT - w / 2.0
    }

    #[test]
    fn a_ready_row_pauses_and_copies_its_link_from_its_menu() {
        let ctx = ctx();
        let ready = view("Alyssa", LinkState::Ready);
        // Right to left: ⋯, then the switch (on).
        assert_eq!(click(&ctx, SWITCH_X, &ready), Some(RowAct::Kick(true)));
        // The ⋯ opens its menu; it asks for nothing by itself. Its first row copies the link.
        assert_eq!(click(&ctx, SIDE_X, &ready), None);
        let menu = more_button(1).with("popup");
        assert!(Popup::is_id_open(&ctx, menu), "open");
        // (Placed under the ⋯ from the pass after it opens.)
        draw(&ctx, Vec::new(), Some(&ready), None);
        let copy = ctx.memory(|m| m.area_rect(menu)).expect("shown").left_top() + vec2(40.0, 20.0);
        draw(&ctx, move_to(copy), Some(&ready), None);
        draw(&ctx, press(copy), Some(&ready), None);
        assert_eq!(draw(&ctx, release(copy), Some(&ready), None).0, Some(RowAct::Copy(LINK.to_owned())));
    }

    #[test]
    fn a_paused_row_lets_back_in_and_a_making_one_has_nothing_to_click() {
        let ctx = ctx();
        let kicked = view("wisp", LinkState::Kicked);
        assert_eq!(click(&ctx, SWITCH_X, &kicked), Some(RowAct::Kick(false)));
        let making = view("Sam", LinkState::Making);
        for x in [SIDE_X, BUTTON_RIGHT - 20.0, BUTTON_RIGHT - 100.0, 100.0] {
            assert_eq!(click(&ctx, x, &making), None);
        }
        // A gone one: × asks to remove it, NEW LINK makes it anew.
        let gone = view("Kai", LinkState::Gone);
        assert_eq!(click(&ctx, SIDE_X, &gone), Some(RowAct::AskRemove));
        assert_eq!(click(&ctx, button_x(&ctx, "New link"), &gone), Some(RowAct::Remake));
    }

    #[test]
    fn a_long_name_is_cut_short_not_its_buttons() {
        let ctx = ctx();
        let long = view(&"W".repeat(NAME_MAX), LinkState::Ready);
        assert_eq!(click(&ctx, SWITCH_X, &long), Some(RowAct::Kick(true)));
    }

    #[test]
    fn a_row_says_what_its_link_can_do() {
        let look = |sharing, copied| RowLook { sharing, copied, now_ms: NOON, offset_ms: 0 };
        assert_eq!(status(&view("Sam", LinkState::Ready), &look(true, false)), "Can listen");
        assert_eq!(status(&view("Sam", LinkState::Ready), &look(false, false)), "Sharing is off");
        assert_eq!(status(&view("Sam", LinkState::Kicked), &look(true, false)), "Paused · hears nothing");
        assert_eq!(status(&view("Sam", LinkState::Gone), &look(true, false)), "This link stopped working");
        assert_eq!(status(&view("Sam", LinkState::Making), &look(true, false)), "Making…");
        assert_eq!(status(&view("Sam", LinkState::Kicked), &look(true, true)), "Link copied");
    }

    #[test]
    fn the_summary_counts_links_and_names_who_is_paused() {
        let links = |states: &[(&str, LinkState)]| states.iter().map(|&(n, s)| view(n, s)).collect::<Vec<_>>();
        assert_eq!(summary(true, None), "Log in to see your links");
        assert_eq!(summary(true, Some(&[])), "No links yet");
        assert_eq!(summary(false, Some(&[])), "No links yet");
        assert_eq!(summary(true, Some(&links(&[("Alyssa", LinkState::Ready)]))), "1 link");
        let two = links(&[("Alyssa", LinkState::Ready), ("Sam", LinkState::Kicked)]);
        assert_eq!(summary(true, Some(&two)), "2 links · Sam paused");
        assert_eq!(summary(false, Some(&two)), "Sharing is off · 2 links");
        let more = links(&[("Sam", LinkState::Kicked), ("Kai", LinkState::Kicked), ("Mo", LinkState::Gone)]);
        assert_eq!(summary(true, Some(&more)), "3 links · Sam and Kai paused · 1 stopped working");
        let many = links(&[("Sam", LinkState::Kicked), ("Kai", LinkState::Kicked), ("Mo", LinkState::Kicked)]);
        assert_eq!(summary(true, Some(&many)), "3 links · 3 paused");
    }

    #[test]
    fn ten_links_grey_out_new_link() {
        let ctx = ctx();
        let at = |count: Option<usize>| {
            let p = ROW.center();
            draw(&ctx, Vec::new(), None, count);
            draw(&ctx, move_to(p), None, count);
            draw(&ctx, press(p), None, count);
            draw(&ctx, release(p), None, count).1
        };
        assert!(at(Some(0)), "the first link");
        assert!(at(Some(MAX_LINKS - 1)));
        assert!(!at(Some(MAX_LINKS)), "greyed at ten");
        // Logged out, the links aren't known: greyed, and it doesn't say there are none.
        assert!(!at(None));
    }

    #[test]
    fn enter_in_the_name_box_makes_the_link() {
        let ctx = ctx();
        let mut d = LinkDialog::New(String::new());
        assert!(dialog(&ctx, Vec::new(), &mut d, &[]).is_none(), "open, the box focused");
        assert!(dialog(&ctx, text("Sam"), &mut d, &[]).is_none());
        let made = dialog(&ctx, key(Key::Enter), &mut d, &[]);
        assert!(matches!(made, Some(Some(Command::ListenNewLink { ref name })) if name == "Sam"), "{made:?}");
        // Nothing typed: Enter does nothing, and the box stays focused for typing.
        let mut d = LinkDialog::New(String::new());
        dialog(&ctx, Vec::new(), &mut d, &[]);
        assert!(dialog(&ctx, key(Key::Enter), &mut d, &[]).is_none());
        dialog(&ctx, text("Kai"), &mut d, &[]);
        assert!(matches!(dialog(&ctx, key(Key::Enter), &mut d, &[]), Some(Some(Command::ListenNewLink { ref name })) if name == "Kai"));
    }

    #[test]
    fn enter_in_the_rename_box_saves() {
        let ctx = ctx();
        let mut d = LinkDialog::Rename(4, "Sam".into());
        dialog(&ctx, Vec::new(), &mut d, &[]);
        // All of it was chosen as it opened: typing replaces it.
        dialog(&ctx, Vec::new(), &mut d, &[]);
        dialog(&ctx, text("Alyssa"), &mut d, &[]);
        let saved = dialog(&ctx, key(Key::Enter), &mut d, &[]);
        assert!(matches!(saved, Some(Some(Command::ListenRenameLink { id: 4, ref name })) if name == "Alyssa"), "{saved:?}");
    }

    #[test]
    fn the_enter_that_opens_remove_doesnt_remove() {
        let ctx = ctx();
        let names = [view("Kai", LinkState::Gone)];
        // Opened by Enter on a keyboard-focused REMOVE: that same Enter is not a yes.
        let mut d = LinkDialog::Remove { id: 1, shown: false };
        assert!(dialog(&ctx, key(Key::Enter), &mut d, &names).is_none());
        // Enter held down from then on isn't either (the key repeats).
        assert!(dialog(&ctx, key(Key::Enter), &mut d, &names).is_none());
        assert!(dialog(&ctx, enter_up(), &mut d, &names).is_none());
        // A fresh Enter while it shows is.
        assert!(matches!(dialog(&ctx, key(Key::Enter), &mut d, &names), Some(Some(Command::ListenRemoveLink(1)))));
        // A link that went meanwhile: the dialog closes.
        let mut d = LinkDialog::Remove { id: 9, shown: true };
        assert!(matches!(dialog(&ctx, Vec::new(), &mut d, &names), Some(None)));
    }

    #[test]
    fn a_double_click_on_kick_doesnt_let_straight_back_in() {
        let ctx = ctx();
        let (ready, kicked) = (view("Alyssa", LinkState::Ready), view("Alyssa", LinkState::Kicked));
        // Over the middle of KICK and of the wider LET BACK IN that takes its place.
        let x = button_x(&ctx, "Kick");
        assert_eq!(click(&ctx, x, &ready), Some(RowAct::Kick(true)));
        // The row shows LET BACK IN where KICK was before the second click of the double-click.
        assert_eq!(click(&ctx, x, &kicked), None, "the second click doesn't count");
        // A click of its own, later, lets back in; and the other way round.
        for _ in 0..40 {
            draw(&ctx, Vec::new(), Some(&kicked), None);
        }
        assert_eq!(click(&ctx, x, &kicked), Some(RowAct::Kick(false)));
        assert_eq!(click(&ctx, x, &ready), None);
    }

    #[test]
    fn enter_held_on_a_focused_kick_doesnt_let_straight_back_in() {
        let ctx = ctx();
        let (ready, kicked) = (view("Alyssa", LinkState::Ready), view("Alyssa", LinkState::Kicked));
        // Tab past the ⋯ to KICK, then Enter: kicked.
        draw(&ctx, key(Key::Tab), Some(&ready), None);
        draw(&ctx, key(Key::Tab), Some(&ready), None);
        assert_eq!(draw(&ctx, key(Key::Enter), Some(&ready), None).0, Some(RowAct::Kick(true)));
        // Held down, the key repeats on LET BACK IN in its place: nothing.
        for _ in 0..3 {
            assert_eq!(draw(&ctx, key(Key::Enter), Some(&kicked), None).0, None);
        }
    }

    #[test]
    fn enter_on_a_focused_cancel_cancels_the_remove() {
        let names = [view("Alyssa", LinkState::Ready)];
        // Opened with the mouse: nothing has the focus. Tab gives it to CANCEL (made first), and
        // Enter on it cancels.
        let ctx = ctx();
        let mut d = LinkDialog::Remove { id: 1, shown: false };
        assert!(dialog(&ctx, Vec::new(), &mut d, &names).is_none());
        assert!(dialog(&ctx, key(Key::Tab), &mut d, &names).is_none());
        assert!(matches!(dialog(&ctx, key(Key::Enter), &mut d, &names), Some(None)), "cancelled, not removed");
        // Tab once more: REMOVE has it, and Enter on it removes.
        let ctx = self::ctx();
        let mut d = LinkDialog::Remove { id: 1, shown: false };
        dialog(&ctx, Vec::new(), &mut d, &names);
        dialog(&ctx, key(Key::Tab), &mut d, &names);
        dialog(&ctx, key(Key::Tab), &mut d, &names);
        assert!(matches!(dialog(&ctx, key(Key::Enter), &mut d, &names), Some(Some(Command::ListenRemoveLink(1)))));
    }

    #[test]
    fn remove_opened_from_the_keyboard_is_said_yes_to_with_enter() {
        let ctx = ctx();
        let p = palette();
        let gone = view("Kai", LinkState::Gone);
        let look = RowLook { sharing: true, copied: false, now_ms: NOON, offset_ms: 0 };
        let mut open: Option<LinkDialog> = None;
        // As the window does it: the rows first, then the dialog over them, in the same pass.
        let frame = |events: Vec<egui::Event>, open: &mut Option<LinkDialog>| {
            let mut out = None;
            pass(&ctx, events, |ui| {
                if link_row(ui, &p, ROW, &gone, &look) == Some(RowAct::AskRemove) {
                    *open = Some(LinkDialog::Remove { id: 1, shown: false });
                }
                if let Some(d) = open.as_mut() {
                    out = link_dialog_ui(ui.ctx(), &p, d, std::slice::from_ref(&gone));
                }
            });
            out
        };
        // Tab to × (the row's first button), Enter: it asks, and that Enter isn't the yes.
        frame(key(Key::Tab), &mut open);
        assert!(frame(key(Key::Enter), &mut open).is_none());
        assert!(open.is_some(), "asked");
        frame(enter_up(), &mut open);
        // The button behind the dialog let go of the focus: a fresh Enter says yes.
        assert!(matches!(frame(key(Key::Enter), &mut open), Some(Some(Command::ListenRemoveLink(1)))));
    }

    #[test]
    fn the_name_box_keeps_its_focus_without_cutting_an_ime_short() {
        let ctx = ctx();
        let mut d = LinkDialog::New(String::new());
        dialog(&ctx, Vec::new(), &mut d, &[]);
        dialog(&ctx, Vec::new(), &mut d, &[]);
        // A name composed with an IME, one pass a key: never cut short.
        let preedit = |t: &str| egui::Event::Ime(egui::ImeEvent::Preedit { text: t.into(), active_range_chars: None });
        for event in [preedit("s"), preedit("さ"), egui::Event::Ime(egui::ImeEvent::Commit("さ".into()))] {
            assert!(!dialog_cuts_ime(&ctx, vec![event], &mut d), "cut short");
        }
        assert!(matches!(&d, LinkDialog::New(t) if t == "さ"), "typed");
        // The arrow keys move in the name, not the focus off it.
        let mut d = LinkDialog::Rename(4, "Sam".into());
        dialog(&ctx, Vec::new(), &mut d, &[]);
        dialog(&ctx, Vec::new(), &mut d, &[]);
        dialog(&ctx, key(Key::ArrowRight), &mut d, &[]);
        dialog(&ctx, text(" Kai"), &mut d, &[]);
        assert!(matches!(&d, LinkDialog::Rename(4, t) if t == "Sam Kai"), "typed at the end");
        // Enter still saves it.
        let saved = dialog(&ctx, key(Key::Enter), &mut d, &[]);
        assert!(matches!(saved, Some(Some(Command::ListenRenameLink { id: 4, ref name })) if name == "Sam Kai"), "{saved:?}");
    }

    #[test]
    fn the_more_menu_opens_under_the_button_ending_at_its_right_edge() {
        let ctx = ctx();
        let ready = view("Alyssa", LinkState::Ready);
        let p = palette();
        let look = RowLook { sharing: true, copied: false, now_ms: NOON, offset_ms: 0 };
        // A wide window: the menu would fit either way, so only its alignment decides.
        let wide = |events: Vec<egui::Event>| {
            let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 600.0))), events, ..Default::default() };
            ctx.run_ui(raw, |ui| {
                link_row(ui, &p, ROW, &ready, &look);
            })
            .textures_delta
            .clear();
        };
        wide(Vec::new());
        let at = pos2(SIDE_X, ROW.center().y);
        wide(move_to(at));
        wide(press(at));
        wide(release(at));
        wide(Vec::new());
        let menu = more_button(1).with("popup");
        assert!(Popup::is_id_open(&ctx, menu), "open");
        let rect = ctx.memory(|m| m.area_rect(menu)).expect("shown");
        let edge = SIDE_X + MORE / 2.0;
        assert!((rect.right() - edge).abs() < 1.0, "ends at the ⋯'s right edge: {rect:?}");
        assert!(rect.left() < edge - MORE - 100.0, "opens to the left: {rect:?}");
    }

    #[test]
    fn a_right_click_on_a_row_opens_its_menu() {
        let ctx = ctx();
        let ready = view("Alyssa", LinkState::Ready);
        let at = pos2(150.0, ROW.center().y);
        draw(&ctx, move_to(at), Some(&ready), None);
        let right = |pressed| vec![egui::Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: egui::Modifiers::NONE }];
        draw(&ctx, right(true), Some(&ready), None);
        draw(&ctx, right(false), Some(&ready), None);
        draw(&ctx, Vec::new(), Some(&ready), None);
        assert!(egui::Popup::is_any_open(&ctx), "its menu");
    }

    #[test]
    fn dates_go_both_ways() {
        for days in [-1, 0, 1, 59, 60, 365, 19_000, 20_752] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(NOON / DAY), (2026, 10, 25));
    }

    #[test]
    fn made_says_today_the_day_or_the_year() {
        assert_eq!(made_words(NOON - 3_600_000, NOON, 0), "made today");
        assert_eq!(made_words(NOON - DAY, NOON, 0), "made 24 Oct");
        assert_eq!(made_words(NOON - 300 * DAY, NOON, 0), "made 29 Dec 2025");
        // Made at 23:30 UTC, seen at 00:30 the next day UTC: in UTC+2 both were on the 26th.
        let made = NOON + 11 * 3_600_000 + 1_800_000;
        assert_eq!(made_words(made, made + 3_600_000, 0), "made 25 Oct");
        assert_eq!(made_words(made, made + 3_600_000, 2 * 3_600_000), "made today");
    }
}
