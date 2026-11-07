//! Settings → Last.fm: the setup guide, the API key / shared secret fields, the browser approval,
//! the connected view, the "stopped" view (Last.fm refuses the key or secret) and the choice about
//! plays recorded for another account. Everything goes to the controller as Commands; the UI never writes the
//! `lastfm_*` settings itself (the controller saves them).

use std::fmt::Write as _;

use egui::{Grid, Margin, RichText, TextEdit, Ui};

use super::{note, step};
use crate::model::Command;
use crate::ui::TruffleApp;
use crate::ui::theme::text_field;
use crate::ui::util::{fmt_thousands, open_in_browser};

const CREATE_ACCOUNT_URL: &str = "https://www.last.fm/api/account/create";
const PROFILE_URL: &str = "https://www.last.fm/user/";
/// The API key and Shared secret fields: this wide, narrower in the docked Settings window.
const FIELD_WIDTH: f32 = 290.0;
const FIELD_MIN_WIDTH: f32 = 150.0;
/// Room beside the fields: their labels and Show / Hide.
const BESIDE_FIELDS: f32 = 160.0;

const KEY_INVALID: &str = "That isn't an API key (32 characters).";
const SECRET_INVALID: &str = "That isn't a Shared secret (32 characters).";
const SECRET_IS_KEY: &str =
    "The Shared secret is a different code from the API key: copy both from the Last.fm page.";

/// Last.fm API keys and shared secrets are 32 hexadecimal characters.
fn looks_like_lastfm_code(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What to say about the typed (trimmed) key and secret. An empty field gets no message yet, but
/// is not ready either.
#[derive(Debug, PartialEq, Eq)]
struct InputCheck {
    key_error: Option<&'static str>,
    secret_error: Option<&'static str>,
    ready: bool,
}

fn check_inputs(key: &str, secret: &str) -> InputCheck {
    let key_ok = looks_like_lastfm_code(key);
    let secret_ok = looks_like_lastfm_code(secret);
    let same = key_ok && secret_ok && key.eq_ignore_ascii_case(secret);
    InputCheck {
        key_error: (!key.is_empty() && !key_ok).then_some(KEY_INVALID),
        secret_error: if !secret.is_empty() && !secret_ok {
            Some(SECRET_INVALID)
        } else if same {
            Some(SECRET_IS_KEY)
        } else {
            None
        },
        ready: key_ok && secret_ok && !same,
    }
}

/// `https://www.last.fm/user/<name>`, with anything but plain letters, digits and `-_.~`
/// percent-encoded, so an odd name can never change where the link goes.
fn profile_url(username: &str) -> String {
    let mut url = String::with_capacity(PROFILE_URL.len() + username.len());
    url.push_str(PROFILE_URL);
    for b in username.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            url.push(b as char);
        } else {
            let _ = write!(url, "%{b:02X}");
        }
    }
    url
}

/// "1 play waiting to be sent" / "1,234 plays waiting to be sent"; nothing when none are waiting.
fn pending_text(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("1 play waiting to be sent".to_owned()),
        n => Some(format!("{} plays waiting to be sent", fmt_thousands(n))),
    }
}

/// "1 play" / "1,234 plays".
fn plays(n: usize) -> String {
    if n == 1 { "1 play".to_owned() } else { format!("{} plays", fmt_thousands(n)) }
}

/// What the setup screen says about plays waiting while no account is connected. `account`: the
/// one they were all played on, if known.
fn waiting_text(n: usize, account: Option<&str>) -> Option<String> {
    let waiting = pending_text(n)?;
    Some(match account {
        Some(name) => {
            format!("{waiting}, recorded while connected as {name}. {} sent once {name} is connected again.", they_are(n))
        }
        None => format!(
            "{waiting}. {} only sent to the Last.fm account {} recorded for, unless you choose otherwise after \
             connecting.",
            they_are(n),
            if n == 1 { "it was" } else { "they were" }
        ),
    })
}

/// What the connected (or stopped) view says about plays recorded for another account.
fn held_text(n: usize, account: Option<&str>, me: &str) -> String {
    let verb = if n == 1 { "is" } else { "are" };
    let recorded = match account {
        Some(name) => format!("recorded while connected as {name}"),
        None => "recorded for another Last.fm account".to_owned(),
    };
    format!("{} {recorded} {verb} waiting. {} only sent to {me} if you choose to.", plays(n), they_are(n))
}

/// "It is" / "They are".
fn they_are(n: usize) -> &'static str {
    if n == 1 { "It is" } else { "They are" }
}

impl TruffleApp {
    pub(super) fn lastfm_section(&mut self, ui: &mut Ui) {
        if self.lastfm.connecting {
            self.lastfm_setup_ui(ui);
        } else if self.lastfm.connected {
            self.lastfm_connected_ui(ui);
        } else if self.lastfm.stopped.is_some() {
            self.lastfm_stopped_ui(ui);
        } else {
            self.lastfm_setup_ui(ui);
        }
        ui.add_space(4.0);
    }

    fn lastfm_connected_ui(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let name = self.lastfm.username.as_deref().map(str::trim).filter(|n| !n.is_empty());
        ui.horizontal_wrapped(|ui| {
            match name {
                Some(name) => {
                    ui.label(RichText::new("✔ Connected as").color(p.accent_ink).strong());
                    let url = profile_url(name);
                    if ui.link(RichText::new(name).strong()).on_hover_text(&url).clicked() {
                        open_in_browser(ui.ctx(), &url);
                    }
                }
                None => {
                    ui.label(RichText::new("✔ Connected").color(p.accent_ink).strong());
                }
            }
        });

        let enabled = self.lastfm_enabled_switch(ui, "Send the songs you play in Truffle to your Last.fm profile");
        if !enabled {
            note(ui, "Paused: songs you play now aren't sent to Last.fm.", p.text_weak);
        }
        if let Some(text) = pending_text(self.lastfm.pending.saturating_sub(self.lastfm.held)) {
            ui.label(RichText::new(text).color(p.text_weak));
        }
        self.lastfm_held_ui(ui);
        if let Some(m) = &self.lastfm.message {
            note(ui, m, p.text);
        }
        ui.add_space(4.0);
        self.lastfm_disconnect_button(ui);
    }

    /// Last.fm refuses the API key or secret while a sign-in is saved: say why and offer every way
    /// out (connect again with corrected details, pause, disconnect).
    fn lastfm_stopped_ui(&mut self, ui: &mut Ui) {
        let p = self.palette;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("⚠ Scrobbling stopped").color(p.error).strong());
            if let Some(name) = self.lastfm.username.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                ui.label(RichText::new(format!("(signed in as {name})")).color(p.text_weak));
            }
        });
        if let Some(reason) = &self.lastfm.stopped {
            note_text(ui, reason, p.text);
        }
        ui.add_space(4.0);

        let enabled =
            self.lastfm_enabled_switch(ui, "While this is on, songs you play are kept and sent once Last.fm accepts Truffle again");
        if !enabled {
            note(ui, "Paused: songs you play now aren't kept for Last.fm.", p.text_weak);
        }
        if let Some(text) = pending_text(self.lastfm.pending.saturating_sub(self.lastfm.held)) {
            ui.label(RichText::new(text).color(p.text_weak));
        }
        self.lastfm_held_ui(ui);
        if let Some(m) = &self.lastfm.message {
            note(ui, m, p.text);
        }
        ui.add_space(6.0);

        note_text(ui, "To fix it, paste the API key and Shared secret again (or those of a new API account), then connect again:", p.text);
        let entered = self.lastfm_fields(ui, true);
        self.lastfm_connect_button(ui, "Connect again", entered);
        ui.add_space(4.0);
        self.lastfm_disconnect_button(ui);
    }

    fn lastfm_setup_ui(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let connecting = self.lastfm.connecting;
        note_text(ui, "Scrobble what you play to Last.fm. Needs your own free Last.fm API account (about 2 minutes).", p.text);
        ui.add_space(8.0);

        step(ui, 1, p.accent_ink, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Open");
                if ui.link("last.fm/api/account/create").on_hover_text(CREATE_ACCOUNT_URL).clicked() {
                    open_in_browser(ui.ctx(), CREATE_ACCOUNT_URL);
                }
                ui.label("and log in.");
            });
            note_text(ui, "Name it \"Truffle\", leave the rest empty and submit.", p.text);
        });
        step(ui, 2, p.accent_ink, |ui| {
            note_text(ui, "Paste the API key and Shared secret below.", p.text);
        });
        step(ui, 3, p.accent_ink, |ui| {
            note_text(ui, "Connect, and approve it in your browser.", p.text);
        });
        ui.add_space(2.0);

        let entered = self.lastfm_fields(ui, !connecting);
        self.lastfm_connect_button(ui, "Connect", entered);

        if self.lastfm.username.is_some() {
            // Connecting again while stopped: the plays wait for this connection.
            if let Some(text) = pending_text(self.lastfm.pending) {
                note(ui, &format!("{text}. They are sent once Truffle is connected again."), p.text_weak);
            }
        } else if let Some(text) = waiting_text(self.lastfm.pending, self.lastfm.held_for.as_deref()) {
            note(ui, &text, p.text_weak);
            if !connecting {
                self.lastfm_discard_ui(ui, None, self.lastfm.pending);
            }
        }
        if let Some(m) = &self.lastfm.message {
            note(ui, m, p.text);
        }
    }

    /// The API key and Shared secret fields. Returns whether Enter was pressed in one of them.
    fn lastfm_fields(&mut self, ui: &mut Ui, enabled: bool) -> bool {
        let p = self.palette;
        let mut entered = false;
        let field_w = (ui.available_width() - BESIDE_FIELDS).clamp(FIELD_MIN_WIDTH, FIELD_WIDTH);
        ui.add_enabled_ui(enabled, |ui| {
            Grid::new("lastfm_fields").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("API key");
                let resp = text_field(
                    ui,
                    &p,
                    TextEdit::singleline(&mut self.lastfm_key_input)
                        .hint_text("32 characters")
                        .char_limit(64)
                        .desired_width(field_w)
                        .font(egui::TextStyle::Monospace)
                        .margin(Margin::symmetric(8, 5)),
                );
                entered |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.end_row();

                ui.label("Shared secret");
                ui.horizontal(|ui| {
                    let resp = text_field(
                        ui,
                        &p,
                        TextEdit::singleline(&mut self.lastfm_secret_input)
                            .hint_text("32 characters")
                            .password(!self.lastfm_show_secret)
                            .char_limit(64)
                            .desired_width(field_w)
                            .font(egui::TextStyle::Monospace)
                            .margin(Margin::symmetric(8, 5)),
                    );
                    entered |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let (label, tip) = if self.lastfm_show_secret {
                        ("Hide", "Hide the Shared secret")
                    } else {
                        ("Show", "Show the Shared secret")
                    };
                    if super::super::kit::button(ui, &p, label, super::super::kit::Kind::Quiet).on_hover_text(tip).clicked() {
                        self.lastfm_show_secret = !self.lastfm_show_secret;
                    }
                });
                ui.end_row();
            });
        });
        entered
    }

    /// Mistakes in the typed key and secret, then the Connect button, or the spinner and Cancel
    /// while waiting for the browser.
    fn lastfm_connect_button(&mut self, ui: &mut Ui, label: &str, entered: bool) {
        let p = self.palette;
        let check = check_inputs(self.lastfm_key_input.trim(), self.lastfm_secret_input.trim());
        if self.lastfm.connecting {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Spinner::new().color(p.accent_ink));
                ui.label(RichText::new("Approve it in your browser…").color(p.text_weak));
            });
            let cancel = super::super::kit::button(ui, &self.palette, "Cancel", super::super::kit::Kind::Secondary).on_hover_text("Stop waiting for the browser").clicked();
            if cancel {
                // Shown at once; the controller confirms with the next status.
                self.lastfm.connecting = false;
                self.send(Command::CancelLastfmConnect);
            }
            return;
        }
        for error in [check.key_error, check.secret_error].into_iter().flatten() {
            note(ui, error, p.error);
        }
        ui.add_space(6.0);
        let clicked = super::super::kit::button_enabled(ui, &p, label, super::super::kit::Kind::Primary, check.ready)
            .on_hover_text("Opens your browser to approve Truffle on Last.fm")
            .on_disabled_hover_text("Paste your API key and Shared secret first")
            .clicked();
        if check.ready && (clicked || entered) {
            self.lastfm_connect();
        }
    }

    /// The "Scrobble my plays" switch. Returns its (new) state.
    fn lastfm_enabled_switch(&mut self, ui: &mut Ui, hover: &str) -> bool {
        let mut enabled = self.lastfm.enabled;
        let p = self.palette;
        let resp = ui
            .horizontal(|ui| {
                let r = super::super::kit::switch(ui, &p, &mut enabled, true);
                ui.label("Scrobble my plays");
                r
            })
            .inner
            .on_hover_text(hover);
        if resp.changed() {
            // Shown at once; the controller saves it and confirms with the next status.
            self.lastfm.enabled = enabled;
            self.send(Command::SetLastfmEnabled(enabled));
        }
        enabled
    }

    fn lastfm_disconnect_button(&mut self, ui: &mut Ui) {
        let disconnect = super::super::kit::button(ui, &self.palette, "Disconnect", super::super::kit::Kind::Secondary).on_hover_text(
            "Forget the Last.fm login on this computer. Your API key and any plays not sent yet are kept; they are \
             only sent to the account they were played on.",
        );
        if disconnect.clicked() {
            // Shown at once; the controller confirms with the next status.
            self.lastfm.connected = false;
            self.lastfm.stopped = None;
            self.lastfm.username = None;
            self.lastfm.message = None;
            self.lastfm_confirm_discard = false;
            self.send(Command::DisconnectLastfm);
        }
    }

    /// Plays recorded for another account than the connected one: send them here, or delete them.
    fn lastfm_held_ui(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let held = self.lastfm.held;
        let Some(me) = self.lastfm.username.clone().filter(|_| held > 0) else { return };
        ui.add_space(2.0);
        note_text(ui, &held_text(held, self.lastfm.held_for.as_deref(), &me), p.text);
        if self.lastfm_confirm_discard {
            self.lastfm_discard_ui(ui, Some(me), held);
            return;
        }
        ui.horizontal_wrapped(|ui| {
            let send = ui
                .button(format!("Send them to {me}"))
                .on_hover_text("Scrobble these plays to the connected Last.fm account after all");
            if send.clicked() {
                self.send(Command::SendHeldLastfmScrobbles { to: me });
            }
            if super::super::kit::button(ui, &self.palette, "Discard…", super::super::kit::Kind::Secondary).on_hover_text("Delete these plays without sending them").clicked() {
                self.lastfm_confirm_discard = true;
            }
        });
    }

    /// "Discard…", then a confirmation, for `count` waiting plays: all of them (`keep_for` None) or
    /// those not recorded for `keep_for`.
    fn lastfm_discard_ui(&mut self, ui: &mut Ui, keep_for: Option<String>, count: usize) {
        let p = self.palette;
        if !self.lastfm_confirm_discard {
            if super::super::kit::button(ui, &self.palette, "Discard…", super::super::kit::Kind::Secondary).on_hover_text("Delete these plays without sending them").clicked() {
                self.lastfm_confirm_discard = true;
            }
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("Delete {} without sending {}?", plays(count), if count == 1 { "it" } else { "them" })).color(p.error));
            if super::super::kit::button(ui, &self.palette, "Delete", super::super::kit::Kind::Secondary).clicked() {
                self.lastfm_confirm_discard = false;
                self.send(Command::DiscardLastfmScrobbles { keep_for });
            }
            if super::super::kit::button(ui, &self.palette, "Keep", super::super::kit::Kind::Secondary).clicked() {
                self.lastfm_confirm_discard = false;
            }
        });
    }

    fn lastfm_connect(&mut self) {
        let api_key = self.lastfm_key_input.trim().to_owned();
        let api_secret = self.lastfm_secret_input.trim().to_owned();
        self.lastfm_key_input.clone_from(&api_key);
        self.lastfm_secret_input.clone_from(&api_secret);
        self.lastfm_show_secret = false;
        // Shown at once; the controller confirms with the next status.
        self.lastfm.connecting = true;
        self.lastfm.message = None;
        self.send(Command::ConnectLastfm { api_key, api_secret });
    }
}

/// Body text at the normal size that wraps to the available width.
fn note_text(ui: &mut Ui, text: &str, color: egui::Color32) {
    ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const SECRET: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn empty_fields_are_not_ready_and_not_flagged() {
        assert_eq!(check_inputs("", ""), InputCheck { key_error: None, secret_error: None, ready: false });
        assert_eq!(check_inputs(KEY, ""), InputCheck { key_error: None, secret_error: None, ready: false });
    }

    #[test]
    fn valid_key_and_secret_are_ready() {
        assert_eq!(check_inputs(KEY, SECRET), InputCheck { key_error: None, secret_error: None, ready: true });
        // Upper-case hex is still hex.
        assert!(check_inputs(&KEY.to_uppercase(), SECRET).ready);
    }

    #[test]
    fn wrong_shapes_are_flagged() {
        let short = &KEY[..31];
        let not_hex = "0123456789abcdef0123456789abcdeg";
        let c = check_inputs(short, not_hex);
        assert_eq!(c.key_error, Some(KEY_INVALID));
        assert_eq!(c.secret_error, Some(SECRET_INVALID));
        assert!(!c.ready);
        assert!(!check_inputs(&format!("{KEY}0"), SECRET).ready);
        // Non-ASCII never passes, whatever its byte length.
        let accented = "0123456789abcdef0123456789abcdé";
        assert_eq!(accented.len(), 32);
        assert!(!looks_like_lastfm_code(accented));
    }

    #[test]
    fn the_same_code_twice_is_flagged() {
        let c = check_inputs(KEY, &KEY.to_uppercase());
        assert_eq!(c.secret_error, Some(SECRET_IS_KEY));
        assert!(!c.ready);
    }

    #[test]
    fn profile_links_are_escaped() {
        assert_eq!(profile_url("RJ_fan-99"), "https://www.last.fm/user/RJ_fan-99");
        assert_eq!(profile_url("a b/c?d"), "https://www.last.fm/user/a%20b%2Fc%3Fd");
        assert_eq!(profile_url("é"), "https://www.last.fm/user/%C3%A9");
    }

    #[test]
    fn pending_counts_read_naturally() {
        assert_eq!(pending_text(0), None);
        assert_eq!(pending_text(1).as_deref(), Some("1 play waiting to be sent"));
        assert_eq!(pending_text(3).as_deref(), Some("3 plays waiting to be sent"));
        assert_eq!(pending_text(1234).as_deref(), Some("1,234 plays waiting to be sent"));
    }

    #[test]
    fn waiting_plays_name_their_account() {
        assert_eq!(waiting_text(0, Some("alice")), None);
        assert_eq!(
            waiting_text(2, Some("alice")).as_deref(),
            Some("2 plays waiting to be sent, recorded while connected as alice. They are sent once alice is connected again.")
        );
        assert_eq!(
            waiting_text(1, None).as_deref(),
            Some(
                "1 play waiting to be sent. It is only sent to the Last.fm account it was recorded for, unless you \
                 choose otherwise after connecting."
            )
        );
        assert_eq!(
            held_text(1, Some("alice"), "bob"),
            "1 play recorded while connected as alice is waiting. It is only sent to bob if you choose to."
        );
        assert!(held_text(1_234, None, "bob").starts_with("1,234 plays recorded for another Last.fm account are waiting."));
    }
}
