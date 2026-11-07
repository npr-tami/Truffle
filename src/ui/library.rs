//! The playlists' shared actions: the right-click menu (pin, edit, find duplicates, remove,
//! refresh), new and deleted playlists, songs dropped on a playlist, and the question when some
//! songs added are in it already.

use std::collections::VecDeque;
use std::ops::Range;
use std::time::Duration;

use egui::{Color32, Id, Key, PointerButton, Sense, TextEdit, Ui, pos2, vec2};

use super::fin;
use super::theme::Palette;

use super::TruffleApp;
use crate::model::{AlreadyIn, Command};

/// What a playlist's right-click menu asked for.
pub(super) enum RowMenu {
    Refresh,
    Pin(String, bool),
    Edit(String),
    FindDuplicates(String),
    AskRemove(String),
}

impl TruffleApp {
    /// Songs dropped on a playlist (a row or a sleeve) go to its end. Local files can't.
    pub(super) fn add_dropped_songs(&mut self, playlist_uri: String, mut uris: Vec<String>) {
        uris.retain(|u| !u.starts_with("spotify:local:"));
        if uris.is_empty() {
            self.toast("Local files can't be added to Spotify playlists.", false);
            return;
        }
        self.add_songs_to(playlist_uri, uris);
    }

    /// Adds songs to the end of a playlist. If it has some of them already, the controller asks
    /// first ("Already in").
    pub(super) fn add_songs_to(&mut self, playlist_uri: String, uris: Vec<String>) {
        self.send(Command::AddToPlaylist { playlist_uri, uris, again: Vec::new(), question: None });
    }

    /// Pins a playlist to the top of the sidebar (below the ones pinned before it), or unpins it.
    pub(super) fn set_pinned(&mut self, uri: &str, pin: bool) {
        self.prefs.pinned_playlists.retain(|u| u != uri);
        if pin {
            self.prefs.pinned_playlists.push(uri.to_owned());
        }
        self.prefs_changed();
        self.refilter_playlists();
    }

    /// Carries out what a playlist's right-click menu asked for.
    pub(super) fn apply_row_menu(&mut self, menu: RowMenu) {
        match menu {
            RowMenu::Refresh => self.send(Command::RefreshLibrary),
            RowMenu::Pin(uri, pin) => self.set_pinned(&uri, pin),
            RowMenu::Edit(uri) => self.open_edit_details(&uri),
            RowMenu::FindDuplicates(uri) => self.find_duplicates(&uri),
            RowMenu::AskRemove(uri) => self.confirm_remove = Some(uri),
        }
    }

    /// "Delete this playlist?" (open while `confirm_remove` is set): DELETE or CANCEL, Enter / Esc.
    pub(super) fn remove_playlist_modal(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let Some(uri) = self.confirm_remove.clone() else { return };
        let Some(pl) = self.playlist(&uri) else {
            self.confirm_remove = None;
            return;
        };
        let (name, owned) = (pl.name.clone(), pl.editable);
        let (mut yes, mut no) = (false, false);
        let modal = fin::dialog(ctx, Id::new("remove_playlist"), |ui| {
            ui.set_width(400.0);
            let title = if owned { format!("Delete \"{name}\"?") } else { format!("Remove \"{name}\" from your library?") };
            let note = if owned {
                "It leaves your Spotify library. Spotify keeps it for 90 days, so it can come back on spotify.com."
            } else {
                "You stop following it. The playlist stays on Spotify."
            };
            fin::dialog_head(ui, &title, Some(note));
            let (ok, cancel) = fin::dialog_actions(ui, &p, Id::new("remove_playlist"), if owned { "Delete" } else { "Remove" }, true);
            yes |= ok;
            no |= cancel;
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                yes = true;
            }
        });
        if yes {
            self.confirm_remove = None;
            // Not asked "Already in" about it while it goes.
            self.already_in.removing(uri.clone());
            self.send(Command::RemovePlaylist { uri });
        } else if no || modal.should_close() {
            self.confirm_remove = None;
        }
    }

    /// "Already in <playlist>" (open while `already_in` has a question): ONLY NEW ONES, ADD ALL or
    /// CANCEL; when it has every song, DON'T ADD or ADD ANYWAY. It opens by itself when the check
    /// comes back, so it waits while something else is open. The next question waits for this one.
    pub(super) fn already_in_modal(&mut self, ctx: &egui::Context) {
        // Settings may stay open a long while beside this window: say once why no box comes.
        if self.settings_open
            && let Some(uri) = self.already_in.untold()
            && let Some(pl) = self.playlist(&uri)
        {
            let text = format!("Already in \"{}\": close Settings to choose what to add.", pl.name);
            self.toast(text, false);
        }
        let name = self.already_in.first().and_then(|a| self.playlist(&a.playlist_uri)).map(|pl| pl.name.clone());
        let wait = self.something_else_open(ctx);
        let p = self.palette;
        if let Some(add) = self.already_in.ui(ctx, &p, name.as_deref(), wait) {
            self.send(add);
        }
    }

    /// The "Already in" box is up, or comes up in this pass: it takes the keys. Not while it waits.
    pub(super) fn already_in_up(&self, ctx: &egui::Context) -> bool {
        self.already_in.can_ask() && !self.something_else_open(ctx)
    }

    /// Something is open that the "Already in" box would cover, or take a click or key from:
    /// another dialog, a menu, a drag, the sidebar over the page, the shortcuts window, or Settings
    /// (its window sits over the middle of this one, where the box would be).
    fn something_else_open(&self, ctx: &egui::Context) -> bool {
        self.dialog_open()
            || self.help_open
            || self.settings_open
            || self.song_menu.is_some()
            || self.sidebar_over_open(ctx.content_rect().width())
            || egui::Popup::is_any_open(ctx)
            || egui::DragAndDrop::has_any_payload(ctx)
    }

    /// The "New playlist" box (open while `new_playlist` is `Some`).
    pub(super) fn new_playlist_modal(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let Some(name) = self.new_playlist.as_mut() else { return };
        let (mut create, mut cancel) = (false, false);
        let modal = fin::dialog(ctx, Id::new("new_playlist"), |ui| {
            ui.set_width(360.0);
            fin::dialog_head(ui, "New playlist", None);
            ui.add_space(14.0);
            let r = fin::field(ui, &p, TextEdit::singleline(name).hint_text("Name").char_limit(100).desired_width(f32::INFINITY));
            r.request_focus();
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                create = true;
            }
            let (ok, no) = fin::dialog_actions(ui, &p, Id::new("new_playlist"), "Create", !name.trim().is_empty());
            create |= ok;
            cancel |= no;
        });
        if create {
            let name = name.trim().to_owned();
            if !name.is_empty() {
                self.send(Command::CreatePlaylist { name });
                self.new_playlist = None;
            }
        } else if cancel || modal.should_close() {
            self.new_playlist = None;
        }
    }

}

/// The "Already in" questions, asked one at a time in the order the checks come back. A box stays
/// until the user answers it (or its playlist goes): a new add's question waits its turn. It takes
/// no click or key until it has been up a moment, so one meant for what was there isn't its answer.
#[derive(Default)]
pub(super) struct AlreadyInState {
    /// The latest question's number.
    last: u64,
    /// The questions to ask, with their numbers, in the order they came. The first one's box is
    /// open, or waits for something else to close.
    asks: VecDeque<(u64, AlreadyIn)>,
    /// The first one's box, once it has shown.
    shown: Option<Shown>,
    /// After an answer, clicks go nowhere until then (egui's clock): see `hold_clicks`.
    hold: Option<f64>,
    /// Playlists DELETE or REMOVE was pressed for: questions about them wait, and go with them.
    /// Until the playlists come again: one still there then wasn't removed, and is asked about.
    removing: Vec<String>,
    /// The newest question the user was told waits for Settings to close.
    told: u64,
}

/// An "Already in" box on screen.
#[derive(Clone, Copy, Debug)]
struct Shown {
    /// When it showed (egui's clock, in seconds).
    at: f64,
    /// The pass it was last drawn in.
    pass: u64,
    /// It takes clicks and keys.
    ready: bool,
}

/// How long (seconds) an "Already in" box is on screen before it takes a click or a key. It pops
/// up by itself, maybe just as the second click of a double-click comes.
const READ_FIRST: f64 = 0.4;

impl AlreadyInState {
    /// A check found songs the playlist has: they are asked about in turn.
    pub(super) fn found(&mut self, already: AlreadyIn) {
        self.last += 1;
        self.asks.push_back((self.last, already));
    }

    /// The question to ask now.
    fn first(&self) -> Option<&AlreadyIn> {
        self.asks.front().map(|(_, already)| already)
    }

    /// A question is open, or waits its turn.
    #[cfg(test)]
    fn asking(&self) -> bool {
        !self.asks.is_empty()
    }

    /// Takes the question asked now (answered, or not to be asked): the next one's turn.
    fn done(&mut self) -> Option<AlreadyIn> {
        self.shown = None;
        self.asks.pop_front().map(|(_, already)| already)
    }

    /// The question asked now can show: there is one, and its playlist isn't being removed.
    fn can_ask(&self) -> bool {
        self.first().is_some_and(|a| !self.removing.contains(&a.playlist_uri))
    }

    /// The newest question's playlist, once: to tell the user it waits for Settings to close.
    fn untold(&mut self) -> Option<String> {
        let (n, already) = self.asks.back()?;
        if *n <= self.told {
            return None;
        }
        self.told = *n;
        Some(already.playlist_uri.clone())
    }

    /// One pass of the question asked now, about the playlist `name` (`None`: it went, and the
    /// question goes too). Not shown while `wait`. Returns the add the answer asks for.
    fn ui(&mut self, ctx: &egui::Context, p: &Palette, name: Option<&str>, wait: bool) -> Option<Command> {
        let Some(already) = self.first() else {
            self.hold_clicks(ctx);
            return None;
        };
        let Some(name) = name else {
            self.done();
            return None;
        };
        if wait || !self.can_ask() {
            self.hold_clicks(ctx);
            return None;
        }
        let ask = already_ask(already, name);
        let answer = already_in_ui(ctx, p, self.id(), &ask, &mut self.shown)?;
        let add = self.answer(answer);
        // Held from this pass on: egui's first pass of a box (even an unseen one) takes no click.
        self.hold = Some(ctx.input(|i| i.time) + ctx.options(|o| o.input_options.max_double_click_delay));
        self.hold_clicks(ctx);
        add
    }

    /// After an answer, until no mouse button has been down for a double-click's time, while no
    /// box is up: an unseen one over everything, so the rest of a double-click on the box's button
    /// goes nowhere (on a song row under it, it would play the song).
    fn hold_clicks(&mut self, ctx: &egui::Context) {
        let Some(until) = self.hold else { return };
        let (now, pressing) = ctx.input(|i| (i.time, i.pointer.any_down() || i.pointer.any_released()));
        let until = if pressing { now + ctx.options(|o| o.input_options.max_double_click_delay) } else { until };
        if now >= until {
            self.hold = None;
            return;
        }
        self.hold = Some(until);
        egui::Modal::new(Id::new("already_in_hold")).frame(egui::Frame::NONE).backdrop_color(Color32::TRANSPARENT).show(ctx, |_| {});
        ctx.request_repaint_after(Duration::from_secs_f64(until - now));
    }

    /// Takes the question asked now with its answer: the add that asks for, if any. That add is
    /// checked again against the playlist as it is then, so the only songs that can go in twice
    /// are the ones the box said it has, after ADD ALL or ADD ANYWAY. If that check fails, the
    /// question comes back.
    fn answer(&mut self, answer: Answer) -> Option<Command> {
        let question = self.done()?;
        let (uris, again) = match answer {
            Answer::OnlyNew => (question.new.clone(), Vec::new()),
            Answer::All => {
                let mut again: Vec<String> = question.uris.iter().filter(|u| !question.new.contains(u)).cloned().collect();
                again.sort();
                again.dedup();
                (question.uris.clone(), again)
            }
            Answer::Nothing => return None,
        };
        Some(Command::AddToPlaylist { playlist_uri: question.playlist_uri.clone(), uris, again, question: Some(question) })
    }

    /// DELETE or REMOVE was pressed for this playlist: its questions wait, and go with it.
    pub(super) fn removing(&mut self, uri: String) {
        self.removing.push(uri);
    }

    /// Drops the questions about the playlists `gone` picks (they went).
    pub(super) fn drop_about(&mut self, gone: impl Fn(&str) -> bool) {
        if self.first().is_some_and(|a| gone(&a.playlist_uri)) {
            self.shown = None;
        }
        self.asks.retain(|(_, a)| !gone(&a.playlist_uri));
        self.removing.retain(|u| !gone(u));
    }

    /// The playlists came again (`has`: the ones there). Questions about the others go; the ones
    /// about a playlist being removed that is still there are asked after all.
    pub(super) fn listed(&mut self, has: impl Fn(&str) -> bool) {
        self.drop_about(|uri| !has(uri));
        self.removing.clear();
    }

    /// Drops every question (logged out, or another account). The numbers go on, so each box still
    /// has an id of its own.
    pub(super) fn clear(&mut self) {
        *self = Self { last: self.last, ..Self::default() };
    }

    /// The box's id, new for each question: nothing of the box before carries over (its size, a
    /// button pressed in it).
    fn id(&self) -> Id {
        Id::new("already_in").with(self.asks.front().map_or(0, |(n, _)| *n))
    }
}

/// What the "Already in" box picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// The songs it doesn't have, each once.
    OnlyNew,
    /// Every song, as asked.
    All,
    /// No song.
    Nothing,
}

/// The "Already in" box's words, and its buttons from left to right (the first is the accent one).
#[derive(Debug, PartialEq)]
struct AlreadyAsk {
    title: String,
    note: String,
    buttons: &'static [(&'static str, Answer)],
}

/// Some songs there, some not.
const SOME_THERE: &[(&str, Answer)] = &[("Only new ones", Answer::OnlyNew), ("Add all", Answer::All), ("Cancel", Answer::Nothing)];
/// Every song there.
const ALL_THERE: &[(&str, Answer)] = &[("Don't add", Answer::Nothing), ("Add anyway", Answer::All)];

/// One pass of the "Already in" box `id`: what was picked, `None` while it stays open. `shown`:
/// the box on screen (`None`, or not drawn in the pass before: it shows now). It takes no click or
/// key until it has been up `READ_FIRST` with no mouse button down, so a click counts only if
/// pressed after that. Enter picks the accent button, Esc adds nothing, and a click beside it does
/// nothing: it opened by itself, so the click was meant for what was there.
fn already_in_ui(ctx: &egui::Context, p: &Palette, id: Id, ask: &AlreadyAsk, shown: &mut Option<Shown>) -> Option<Answer> {
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    let mut up = match *shown {
        Some(s) if s.pass + 1 == pass => s,
        // New, or back after a while away (behind another dialog, or the mini player).
        _ => Shown { at: now, pass, ready: false },
    };
    up.pass = pass;
    let mut answer = None;
    let modal = fin::dialog(ctx, id, |ui| {
        let widths: Vec<f32> = ask.buttons.iter().map(|&(label, _)| fin::text_button_w(ui, label, false)).collect();
        // Narrower in a narrow window (a large interface size), 16 from its sides, but never
        // narrower than a button.
        let room = ui.ctx().content_rect().width() - 2.0 * (22.0 + 16.0);
        ui.set_width(400.0f32.min(room).max(widths.iter().copied().fold(0.0, f32::max)));
        fin::dialog_head(ui, &ask.title, Some(&ask.note));
        ui.add_space(20.0);
        // The buttons on the right, 8 apart, on more rows if they don't fit on one; each row
        // placed from its last one. Only a click with the mouse counts: a button with the
        // keyboard's focus looks no different, so Enter stays the accent one's.
        let rows = button_rows(&widths, ui.available_width());
        let (block, _) = ui.allocate_exact_size(vec2(ui.available_width(), rows.len() as f32 * (28.0 + 8.0) - 8.0), Sense::hover());
        for (r, row) in rows.into_iter().enumerate() {
            let top = block.top() + r as f32 * (28.0 + 8.0);
            let mut x = block.right();
            for k in row.rev() {
                let (label, pick) = ask.buttons[k];
                x -= widths[k];
                if fin::text_button(ui, p, pos2(x, top), id.with(k), label, None, k == 0, true).clicked_by(PointerButton::Primary) && up.ready {
                    answer = Some(pick);
                }
                x -= 8.0;
            }
        }
    });
    // Only a key pressed now, not the repeats of one held down: those would answer each box that
    // comes next.
    if answer.is_none() && up.ready && modal.is_top_modal {
        answer = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key { key: Key::Enter, pressed: true, repeat: false, .. } => ask.buttons.first().map(|&(_, pick)| pick),
                egui::Event::Key { key: Key::Escape, pressed: true, repeat: false, .. } => Some(Answer::Nothing),
                _ => None,
            })
        });
    }
    if !up.ready {
        let left = up.at + READ_FIRST - now;
        if left > 0.0 {
            ctx.request_repaint_after(Duration::from_secs_f64(left));
        } else {
            // A button held down from before isn't let go of as a click on it.
            up.ready = !ctx.input(|i| i.pointer.any_down());
        }
    }
    *shown = Some(up);
    answer
}

/// Buttons `widths` wide, 8 apart, on rows `room` wide: which go on each row, top first. As many as
/// fit go on a row, in order; one wider than `room` has a row of its own.
fn button_rows(widths: &[f32], room: f32) -> Vec<Range<usize>> {
    let mut rows: Vec<Range<usize>> = Vec::new();
    let mut used = 0.0;
    for (k, &w) in widths.iter().enumerate() {
        match rows.last_mut() {
            Some(row) if used + 8.0 + w <= room => {
                row.end = k + 1;
                used += 8.0 + w;
            }
            _ => {
                rows.push(k..k + 1);
                used = w;
            }
        }
    }
    rows
}

/// What the "Already in" box says about these songs and the playlist `name`.
fn already_ask(already: &AlreadyIn, name: &str) -> AlreadyAsk {
    let title = format!("Already in \"{name}\"");
    let (note, buttons) = if already.already < already.songs {
        (format!("{} of {} songs are already in this playlist.", already.already, already.songs), SOME_THERE)
    } else if already.songs == 1 {
        ("This song is already in this playlist.".to_owned(), ALL_THERE)
    } else {
        (format!("All {} songs are already in this playlist.", already.songs), ALL_THERE)
    };
    AlreadyAsk { title, note, buttons }
}

/// A playlist's right-click menu (list rows and sleeves): pin, edit details, find duplicates,
/// delete or remove. Sets `menu` to what was picked.
pub(super) fn playlist_menu(ui: &mut Ui, uri: &str, pinned: bool, editable: bool, menu: &mut Option<RowMenu>) {
    ui.set_width(210.0);
    if fin::menu_item(ui, Some("pin"), if pinned { "Unpin" } else { "Pin to the top" }, None, false, true).clicked() {
        *menu = Some(RowMenu::Pin(uri.to_owned(), !pinned));
        ui.close();
    }
    if editable && fin::menu_item(ui, Some("pen"), "Edit details", None, false, true).clicked() {
        *menu = Some(RowMenu::Edit(uri.to_owned()));
        ui.close();
    }
    if editable && fin::menu_item(ui, Some("search"), "Find duplicates", None, false, true).clicked() {
        *menu = Some(RowMenu::FindDuplicates(uri.to_owned()));
        ui.close();
    }
    if fin::menu_item(ui, Some("refresh"), "Refresh playlists", None, false, true).clicked() {
        *menu = Some(RowMenu::Refresh);
        ui.close();
    }
    fin::menu_sep(ui);
    let what = if editable { "Delete playlist" } else { "Remove from library" };
    if fin::menu_item(ui, Some("x"), what, None, false, true).clicked() {
        *menu = Some(RowMenu::AskRemove(uri.to_owned()));
        ui.close();
    }
}

#[cfg(test)]
mod tests {
    use egui::{Color32, Key, Pos2, Rect};

    use super::*;
    use crate::config::ThemeMode;
    use crate::ui::kit::headless::{key, pass, press, release};

    const GYM: &str = "spotify:playlist:gym";
    const ROAD: &str = "spotify:playlist:road";

    fn uris(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| format!("spotify:track:{id}")).collect()
    }

    fn asked_in(playlist_uri: &str, ids: &[&str], have: &[&str]) -> AlreadyIn {
        let have = uris(have).into_iter().collect();
        AlreadyIn::find(playlist_uri, &uris(ids), &have, &[]).expect("some are there")
    }

    fn asked(ids: &[&str], have: &[&str]) -> AlreadyIn {
        asked_in("spotify:playlist:p", ids, have)
    }

    /// The playlist the question asked now is about.
    fn first(state: &AlreadyInState) -> Option<&str> {
        state.first().map(|a| a.playlist_uri.as_str())
    }

    fn labels(ask: &AlreadyAsk) -> Vec<&str> {
        ask.buttons.iter().map(|(label, _)| *label).collect()
    }

    /// What an add asks for: the playlist, the songs, and the ones that may go in again.
    fn add(command: Option<Command>) -> Option<(String, Vec<String>, Vec<String>)> {
        match command? {
            Command::AddToPlaylist { playlist_uri, uris, again, .. } => Some((playlist_uri, uris, again)),
            other => panic!("not an add: {other:?}"),
        }
    }

    #[test]
    fn some_there_offers_only_the_new_ones_first() {
        let ask = already_ask(&asked(&["a", "b", "c", "b"], &["b"]), "Gym mix");
        assert_eq!(ask.title, "Already in \"Gym mix\"");
        assert_eq!(ask.note, "1 of 3 songs are already in this playlist.");
        assert_eq!(labels(&ask), ["Only new ones", "Add all", "Cancel"]);
        // Enter takes the first; Esc and CANCEL add nothing.
        assert_eq!(ask.buttons[0].1, Answer::OnlyNew);
        assert_eq!(ask.buttons[2].1, Answer::Nothing);
    }

    #[test]
    fn one_song_there_asks_about_that_song() {
        let ask = already_ask(&asked(&["a", "a"], &["a"]), "Gym mix");
        assert_eq!(ask.title, "Already in \"Gym mix\"");
        assert_eq!(ask.note, "This song is already in this playlist.");
        assert_eq!(labels(&ask), ["Don't add", "Add anyway"]);
        // Enter and Esc both add nothing.
        assert_eq!(ask.buttons[0].1, Answer::Nothing);
        assert_eq!(ask.buttons[1].1, Answer::All);
    }

    #[test]
    fn every_song_there_says_how_many() {
        let ask = already_ask(&asked(&["a", "b", "c"], &["c", "b", "a", "d"]), "Gym mix");
        assert_eq!(ask.note, "All 3 songs are already in this playlist.");
        assert_eq!(labels(&ask), ["Don't add", "Add anyway"]);
    }

    #[test]
    fn each_answer_adds_what_the_box_said_and_is_checked_again() {
        // Gym has a; a, b and a again were dropped on it.
        let mut state = AlreadyInState::default();
        state.found(asked_in(GYM, &["a", "b", "a"], &["a"]));
        // ONLY NEW ONES: b once. Gym is read again first, and b goes in only if it still isn't
        // there (say, added from the phone meanwhile).
        assert_eq!(add(state.answer(Answer::OnlyNew)), Some((GYM.to_owned(), uris(&["b"]), Vec::new())));
        assert!(!state.asking());
        // ADD ALL: the songs as asked. Only a, which the box said is there, may go in again.
        state.found(asked_in(GYM, &["a", "b", "a"], &["a"]));
        assert_eq!(add(state.answer(Answer::All)), Some((GYM.to_owned(), uris(&["a", "b", "a"]), uris(&["a"]))));
        // CANCEL: nothing, and the question is answered.
        state.found(asked_in(GYM, &["a", "b"], &["a"]));
        assert_eq!(add(state.answer(Answer::Nothing)), None);
        assert!(!state.asking());
        // ADD ANYWAY with every song there: each may go in again.
        state.found(asked_in(ROAD, &["c", "d"], &["d", "c"]));
        assert_eq!(add(state.answer(Answer::All)), Some((ROAD.to_owned(), uris(&["c", "d"]), uris(&["c", "d"]))));
        assert_eq!(add(state.answer(Answer::All)), None);
    }

    #[test]
    fn an_answer_takes_its_question_along_to_be_asked_again() {
        // Gym has a; a and b were dropped on it. Whatever the answer adds, the question goes with
        // it: if Gym can't be read for the check, it comes back as it was.
        for answer in [Answer::OnlyNew, Answer::All] {
            let mut state = AlreadyInState::default();
            let question = asked_in(GYM, &["a", "b"], &["a"]);
            state.found(question.clone());
            match state.answer(answer) {
                Some(Command::AddToPlaylist { question: back, .. }) => assert_eq!(back, Some(question)),
                other => panic!("not an add: {other:?}"),
            }
        }
    }

    #[test]
    fn questions_are_asked_one_at_a_time_in_the_order_they_come() {
        let mut state = AlreadyInState::default();
        state.found(asked_in(ROAD, &["c"], &["c"]));
        state.found(asked_in(GYM, &["a", "b"], &["a"]));
        let road = state.id();
        assert_eq!(first(&state), Some(ROAD));
        state.answer(Answer::Nothing);
        // The next one's box has an id of its own.
        assert_eq!(first(&state), Some(GYM));
        assert_ne!(state.id(), road);
        assert!(add(state.answer(Answer::OnlyNew)).is_some());
        assert!(!state.asking());
    }

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        pass(&ctx, Vec::new(), |_| {});
        ctx
    }

    fn palette() -> Palette {
        Palette::build(ThemeMode::Dark, Color32::RED, None)
    }

    /// One pass of the questions in `state` (each about "Gym mix"), not shown while `wait`: the add
    /// the answer asks for.
    fn show_or_wait(ctx: &egui::Context, state: &mut AlreadyInState, events: Vec<egui::Event>, wait: bool) -> Option<Command> {
        let p = palette();
        let mut out = None;
        pass(ctx, events, |ui| out = state.ui(ui.ctx(), &p, Some("Gym mix"), wait));
        out
    }

    fn show(ctx: &egui::Context, state: &mut AlreadyInState, events: Vec<egui::Event>) -> Option<Command> {
        show_or_wait(ctx, state, events, false)
    }

    /// Half a second of passes (60 a second) with the box up and nothing pressed: long enough to
    /// read it, after which it takes clicks and keys.
    fn read(ctx: &egui::Context, state: &mut AlreadyInState) {
        for _ in 0..30 {
            assert!(show(ctx, state, Vec::new()).is_none());
        }
    }

    /// A pass of the questions in `state` with nothing pressed: where the first one's button `k`
    /// (from the left) is.
    fn button(ctx: &egui::Context, state: &mut AlreadyInState, k: usize) -> Rect {
        let p = palette();
        let mut at = None;
        pass(ctx, Vec::new(), |ui| {
            assert!(state.ui(ui.ctx(), &p, Some("Gym mix"), false).is_none());
            at = ui.ctx().read_response(state.id().with(k)).map(|r| r.rect);
        });
        at.expect("the box is up")
    }

    /// Pressed at `at`, let go in the next pass: what that asked for.
    fn click(ctx: &egui::Context, state: &mut AlreadyInState, at: Pos2) -> Option<Command> {
        assert!(show(ctx, state, press(at)).is_none());
        show(ctx, state, release(at))
    }

    fn key_event(key: Key, pressed: bool, repeat: bool) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed, repeat, modifiers: egui::Modifiers::NONE }
    }

    /// `key` pressed and let go.
    fn tap(key: Key) -> Vec<egui::Event> {
        vec![key_event(key, true, false), key_event(key, false, false)]
    }

    /// `key` still held down: one of its repeats.
    fn held(key: Key) -> Vec<egui::Event> {
        vec![key_event(key, true, true)]
    }

    fn let_go(key: Key) -> Vec<egui::Event> {
        vec![key_event(key, false, false)]
    }

    #[test]
    fn a_box_takes_no_click_or_key_until_it_has_been_up_a_moment() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        state.found(asked(&["a", "b"], &["a"]));
        // The Enter that closed another dialog, in the pass this one pops up in.
        assert!(show(&ctx, &mut state, tap(Key::Enter)).is_none());
        // Just up: ONLY NEW ONES, CANCEL, Enter and Esc do nothing.
        let (only_new, cancel) = (button(&ctx, &mut state, 0).center(), button(&ctx, &mut state, 2).center());
        assert!(click(&ctx, &mut state, only_new).is_none());
        assert!(click(&ctx, &mut state, cancel).is_none());
        assert!(show(&ctx, &mut state, tap(Key::Enter)).is_none());
        assert!(show(&ctx, &mut state, tap(Key::Escape)).is_none());
        assert!(state.asking());
        // Once it has been up a moment, a click is its answer.
        read(&ctx, &mut state);
        assert_eq!(add(click(&ctx, &mut state, only_new)), Some(("spotify:playlist:p".to_owned(), uris(&["b"]), Vec::new())));
    }

    #[test]
    fn the_second_click_of_a_double_click_does_not_answer_the_next_box() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        // Gym has some of the songs dropped on it, Road all: two boxes, one after the other.
        state.found(asked_in(GYM, &["a", "b"], &["a"]));
        state.found(asked_in(ROAD, &["c"], &["c"]));
        read(&ctx, &mut state);
        // CANCEL (on the right) on Gym's box...
        let at = button(&ctx, &mut state, 2).center();
        assert!(click(&ctx, &mut state, at).is_none());
        assert_eq!(first(&state), Some(ROAD));
        // ...and the second click comes as Road's box is up, with ADD ANYWAY under the pointer.
        show(&ctx, &mut state, Vec::new());
        assert!(button(&ctx, &mut state, 1).contains(at));
        assert!(click(&ctx, &mut state, at).is_none());
        assert_eq!(first(&state), Some(ROAD));
        // Read first, the same click adds the song again.
        read(&ctx, &mut state);
        assert_eq!(add(click(&ctx, &mut state, at)), Some((ROAD.to_owned(), uris(&["c"]), uris(&["c"]))));
    }

    #[test]
    fn a_press_from_before_the_box_was_up_a_moment_is_no_click() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        state.found(asked(&["a"], &["a"]));
        show(&ctx, &mut state, Vec::new());
        let add_anyway = button(&ctx, &mut state, 1).center();
        assert!(show(&ctx, &mut state, press(add_anyway)).is_none());
        // Held down while the moment passes, then let go (still quick enough for a click).
        read(&ctx, &mut state);
        assert!(show(&ctx, &mut state, release(add_anyway)).is_none());
        assert!(state.asking());
        // A new click is an answer.
        assert!(add(click(&ctx, &mut state, add_anyway)).is_some());
    }

    #[test]
    fn a_key_held_down_answers_one_box_at_most() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        // Gym has every song (Enter: DON'T ADD), Road and Gym again some (Enter: ONLY NEW ONES).
        state.found(asked_in(GYM, &["a"], &["a"]));
        state.found(asked_in(ROAD, &["a", "b"], &["a"]));
        state.found(asked_in(GYM, &["a", "c"], &["a"]));
        read(&ctx, &mut state);
        assert!(show(&ctx, &mut state, key(Key::Enter)).is_none());
        assert_eq!(first(&state), Some(ROAD));
        // Enter held down: its repeats go on after Road's box is up and ready.
        for _ in 0..40 {
            assert!(show(&ctx, &mut state, held(Key::Enter)).is_none());
        }
        assert_eq!(first(&state), Some(ROAD));
        show(&ctx, &mut state, let_go(Key::Enter));
        // Esc held down cancels Road's question only.
        assert!(show(&ctx, &mut state, key(Key::Escape)).is_none());
        for _ in 0..40 {
            assert!(show(&ctx, &mut state, held(Key::Escape)).is_none());
        }
        assert_eq!(first(&state), Some(GYM));
        show(&ctx, &mut state, let_go(Key::Escape));
        // Enter pressed again is the last one's answer.
        assert_eq!(add(show(&ctx, &mut state, tap(Key::Enter))), Some((GYM.to_owned(), uris(&["c"]), Vec::new())));
    }

    #[test]
    fn with_every_song_there_enter_and_esc_add_nothing() {
        for k in [Key::Enter, Key::Escape] {
            let ctx = ctx();
            let mut state = AlreadyInState::default();
            state.found(asked(&["a"], &["a"]));
            read(&ctx, &mut state);
            assert!(show(&ctx, &mut state, tap(k)).is_none());
            assert!(!state.asking());
        }
    }

    #[test]
    fn a_click_beside_the_box_does_nothing() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        state.found(asked(&["a", "b"], &["a"]));
        read(&ctx, &mut state);
        // On the dimmed page around it: say, meant for a menu it popped up over.
        assert!(click(&ctx, &mut state, pos2(20.0, 20.0)).is_none());
        assert!(state.asking());
    }

    #[test]
    fn enter_stays_the_accent_button_after_tab() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        state.found(asked(&["a", "b", "c"], &["a", "b", "c"]));
        read(&ctx, &mut state);
        // Tab gives a button the keyboard's focus, which looks no different: Enter is still
        // DON'T ADD, not ADD ANYWAY.
        show(&ctx, &mut state, tap(Key::Tab));
        show(&ctx, &mut state, Vec::new());
        assert!(show(&ctx, &mut state, tap(Key::Enter)).is_none());
        assert!(!state.asking());
    }

    #[test]
    fn a_box_waits_while_something_else_is_open_and_comes_back_as_new() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        state.found(asked(&["a", "b"], &["a"]));
        // Another dialog is open: no box, and its Enter isn't an answer here.
        for _ in 0..30 {
            assert!(show_or_wait(&ctx, &mut state, tap(Key::Enter), true).is_none());
        }
        assert!(state.asking());
        assert!(ctx.read_response(state.id().with(0)).is_none());
        // It closed: the box shows, and is read first.
        assert!(show(&ctx, &mut state, tap(Key::Enter)).is_none());
        read(&ctx, &mut state);
        // Away again for a moment (the shortcuts window): back, it takes no Enter at once.
        show_or_wait(&ctx, &mut state, Vec::new(), true);
        show(&ctx, &mut state, Vec::new());
        assert!(show(&ctx, &mut state, tap(Key::Enter)).is_none());
        assert!(state.asking());
        read(&ctx, &mut state);
        assert!(add(show(&ctx, &mut state, tap(Key::Enter))).is_some());
    }

    #[test]
    fn questions_about_a_playlist_that_went_go_too() {
        let ctx = ctx();
        let p = palette();
        let mut state = AlreadyInState::default();
        state.found(asked_in(GYM, &["a"], &["a"]));
        state.found(asked_in(ROAD, &["c"], &["c"]));
        state.found(asked_in(GYM, &["b"], &["b"]));
        read(&ctx, &mut state);
        state.drop_about(|uri| uri == GYM);
        assert_eq!(first(&state), Some(ROAD));
        assert!(state.shown.is_none());
        // Gone from the playlists by the time it is asked: dropped too.
        pass(&ctx, Vec::new(), |ui| assert!(state.ui(ui.ctx(), &p, None, false).is_none()));
        assert!(!state.asking());
        // Logged out: every question goes, but the numbers go on (each box keeps an id of its own).
        state.found(asked_in(GYM, &["a"], &["a"]));
        let before = state.id();
        state.clear();
        assert!(!state.asking());
        state.found(asked_in(GYM, &["a"], &["a"]));
        assert_ne!(state.id(), before);
    }

    #[test]
    fn a_question_about_a_playlist_being_removed_waits_and_goes_with_it() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        // Asked while "Delete Gym?" was open, then DELETE; another check comes back after it.
        state.found(asked_in(GYM, &["a", "b"], &["a"]));
        state.removing(GYM.to_owned());
        state.found(asked_in(GYM, &["c"], &["c"]));
        // No box for Gym while it goes, and no Enter is its answer.
        for _ in 0..30 {
            assert!(show(&ctx, &mut state, tap(Key::Enter)).is_none());
        }
        assert!(ctx.read_response(state.id().with(0)).is_none());
        // Gone: so are its questions.
        state.drop_about(|uri| uri == GYM);
        assert!(!state.asking());
        // DELETE didn't go through: the playlists come again with Gym in them, and it is asked.
        state.found(asked_in(GYM, &["a", "b"], &["a"]));
        state.removing(GYM.to_owned());
        state.listed(|uri| uri == GYM);
        show(&ctx, &mut state, Vec::new());
        assert!(ctx.read_response(state.id().with(0)).is_some());
    }

    #[test]
    fn a_question_that_waits_for_settings_is_told_about_once() {
        let mut state = AlreadyInState::default();
        assert_eq!(state.untold(), None);
        // Dropped on Gym with Settings open (or Settings opened since): told once.
        state.found(asked_in(GYM, &["a", "b"], &["a"]));
        assert_eq!(state.untold().as_deref(), Some(GYM));
        assert_eq!(state.untold(), None);
        // Dropped on Road too: told about that one.
        state.found(asked_in(ROAD, &["c"], &["c"]));
        assert_eq!(state.untold().as_deref(), Some(ROAD));
        assert_eq!(state.untold(), None);
    }

    /// One pass of the questions in `state` over a song row that fills the window: the row's
    /// response.
    fn over_row(ctx: &egui::Context, state: &mut AlreadyInState, events: Vec<egui::Event>) -> egui::Response {
        let p = palette();
        let mut row = None;
        pass(ctx, events, |ui| {
            row = Some(ui.interact(ui.max_rect(), Id::new("row"), Sense::click()));
            state.ui(ui.ctx(), &p, Some("Gym mix"), false);
        });
        row.expect("drawn")
    }

    #[test]
    fn the_rest_of_a_double_click_on_the_last_box_goes_nowhere() {
        let ctx = ctx();
        let mut state = AlreadyInState::default();
        state.found(asked(&["a"], &["a"]));
        read(&ctx, &mut state);
        // DON'T ADD double-clicked: the first click answers...
        let at = button(&ctx, &mut state, 0).center();
        assert!(!over_row(&ctx, &mut state, press(at)).clicked());
        assert!(!over_row(&ctx, &mut state, release(at)).clicked());
        assert!(!state.asking());
        // ...and the second, with the box gone, doesn't reach the song row that was under it.
        over_row(&ctx, &mut state, Vec::new());
        assert!(!over_row(&ctx, &mut state, press(at)).clicked());
        assert!(!over_row(&ctx, &mut state, release(at)).clicked());
        // A moment later a click there is the row's again, and a single one.
        for _ in 0..30 {
            over_row(&ctx, &mut state, Vec::new());
        }
        assert!(!over_row(&ctx, &mut state, press(at)).clicked());
        let row = over_row(&ctx, &mut state, release(at));
        assert!(row.clicked() && !row.double_clicked());
    }

    #[test]
    fn buttons_that_dont_fit_on_one_row_go_on_more() {
        // How many buttons go on each row: in order, as many as fit, 8 apart; one too wide has a
        // row of its own.
        let on_rows = |room| button_rows(&[120.0, 80.0, 70.0], room).into_iter().map(|r| r.len()).collect::<Vec<_>>();
        assert_eq!(on_rows(400.0), [3]);
        assert_eq!(on_rows(286.0), [3]);
        assert_eq!(on_rows(200.0), [1, 2]);
        assert_eq!(on_rows(100.0), [1, 1, 1]);
    }

    #[test]
    fn in_a_narrow_window_the_box_fits_and_its_buttons_wrap() {
        let p = palette();
        // The narrowest window at an interface size of 125%, 150% and 200%; some songs there, and
        // all of them.
        for w in [320.0, 267.0, 200.0] {
            for have in [&["a"][..], &["a", "b", "c"]] {
                let ctx = ctx();
                let mut state = AlreadyInState::default();
                state.found(asked(&["a", "b", "c"], have));
                let screen = Rect::from_min_size(Pos2::ZERO, vec2(w, 480.0));
                let mut buttons: Vec<Rect> = Vec::new();
                for _ in 0..3 {
                    let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
                    ctx.run_ui(raw, |ui| {
                        assert!(state.ui(ui.ctx(), &p, Some("Gym mix"), false).is_none());
                        buttons = (0..3).filter_map(|k| ui.ctx().read_response(state.id().with(k)).map(|r| r.rect)).collect();
                    })
                    .textures_delta
                    .clear();
                }
                assert_eq!(buttons.len(), if have.len() < 3 { 3 } else { 2 });
                for (k, b) in buttons.iter().enumerate() {
                    assert!(screen.shrink(16.0).contains_rect(*b), "{w}: button {k} at {b:?}");
                    assert!(buttons[..k].iter().all(|o| !o.intersects(*b)), "{w}: button {k} on another");
                }
                // Too narrow for one row: on more.
                if w < 300.0 {
                    assert!(buttons.iter().any(|b| b.top() > buttons[0].top()), "{w}: one row");
                }
            }
        }
    }
}
