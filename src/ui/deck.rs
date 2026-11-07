//! The record's turn, shared by every turntable on screen (the side panel's, the one docked in the
//! player bar): it turns slowly while music plays (one turn per TURN_SECS), gets up to speed and
//! coasts to a stop, and asks for frames only while it moves. Also the shuffle actions behind the
//! rules (reshuffle, rules, blocking).

use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::Duration;

use egui::{Id, Ui};

use super::kit;
use super::util::fmt_thousands;
use super::{TruffleApp, Undo};
use crate::model::{Command, PlayStatus, ShuffleRules};

/// One turn of the record, in seconds: slower than a real 33 1/3 (1.8 s), which looks busy on
/// screen, so the turning stays calm.
const TURN_SECS: f32 = 7.5;
/// Time between frames while the record turns (20 a second, plenty at this speed).
const SPIN_FRAME: Duration = Duration::from_millis(50);
/// How long the platter takes to get up to speed, and to coast to a stop.
const SPIN_UP_SECS: f32 = 0.6;

/// The platter's turn, kept between frames.
#[derive(Clone, Copy, Default)]
struct Spin {
    /// How far it has turned (radians, clockwise).
    angle: f32,
    /// 0 (still) to 1 (full speed).
    speed: f32,
    /// `InputState::time` of the last step.
    at: f64,
}

impl Spin {
    /// Moves on to `now` (seconds): towards full speed while `on`, coasting to a stop otherwise. A
    /// long gap (the window was hidden) counts as a tenth of a second.
    fn step(&mut self, now: f64, on: bool) {
        let dt = ((now - self.at) as f32).clamp(0.0, 0.1);
        self.at = now;
        let ramp = dt / SPIN_UP_SECS;
        self.speed = if on { (self.speed + ramp).min(1.0) } else { (self.speed - ramp).max(0.0) };
        self.angle = (self.angle + self.speed * TAU / TURN_SECS * dt).rem_euclid(TAU);
        if !on && self.speed < 1e-3 {
            // Stopped: no more frames for a turn nobody could see.
            self.speed = 0.0;
        }
    }
}

impl TruffleApp {
    pub(super) fn reshuffle(&mut self) {
        self.send(Command::Reshuffle);
        let left = self.deck.as_ref().map_or(0, |d| d.order.len().saturating_sub(d.now + 1));
        self.note(format!("Re-ordered the {} songs still to come", fmt_thousands(left)), None);
    }

    /// The record's turn now (radians), stepping it on: for the turntable docked in the player bar.
    pub(super) fn deck_spin(&self, ui: &Ui) -> f32 {
        let playing = self.playback.track.is_some() && self.playback.status == PlayStatus::Playing;
        self.deck_turn_platter(ui, playing, true).angle
    }

    /// Moves the platter on by a frame, and asks for the next one while it turns: only while
    /// music plays (and as it coasts to a stop after), the deck is on screen, the record is set to
    /// spin and Windows' animations are on.
    fn deck_turn_platter(&self, ui: &Ui, playing: bool, visible: bool) -> Spin {
        let id = Id::new("deck_spin");
        let now = ui.input(|i| i.time);
        let mut spin: Spin = ui.data(|m| m.get_temp(id)).unwrap_or_default();
        let on = playing && self.prefs.spin_record && kit::animations_on();
        if self.prefs.spin_record && kit::animations_on() {
            spin.step(now, on);
        } else {
            spin.speed = 0.0;
            spin.at = now;
        }
        ui.data_mut(|m| m.insert_temp(id, spin));
        if visible && (on || spin.speed > 0.0) {
            ui.ctx().request_repaint_after(SPIN_FRAME);
        }
        spin
    }

    pub(super) fn set_rules(&mut self, rules: ShuffleRules) {
        self.rules = rules.clone();
        self.prefs.shuffle_rules = rules.clone();
        self.send(Command::SetShuffleRules(rules));
    }

    pub(super) fn set_blocked(&mut self, uri: String, block: bool) {
        let set = Arc::make_mut(&mut self.blocked);
        if block {
            set.insert(uri.clone());
        } else {
            set.remove(&uri);
        }
        self.send(Command::SetBlocked { uri: uri.clone(), blocked: block });
        let text = if block { "Blocked from shuffle" } else { "Allowed in shuffle again" };
        self.note(text, Some(Undo::Block(vec![uri], !block)));
    }

    /// Blocks (or allows) several songs, with one note whose Undo puts all of them back.
    pub(super) fn set_blocked_many(&mut self, uris: Vec<String>, block: bool) {
        let set = Arc::make_mut(&mut self.blocked);
        let changed: Vec<String> = uris.into_iter().filter(|u| if block { set.insert(u.clone()) } else { set.remove(u) }).collect();
        if changed.is_empty() {
            return;
        }
        for uri in &changed {
            self.send(Command::SetBlocked { uri: uri.clone(), blocked: block });
        }
        let n = changed.len();
        let text = match (block, n) {
            (true, 1) => "Blocked from shuffle".to_owned(),
            (true, _) => format!("{n} songs blocked from shuffle"),
            (false, 1) => "Allowed in shuffle again".to_owned(),
            (false, _) => format!("{n} songs allowed in shuffle again"),
        };
        self.note(text, Some(Undo::Block(changed, !block)));
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_turns_once_per_turn_time() {
        let mut spin = Spin { speed: 1.0, ..Spin::default() };
        // TURN_SECS at 30 frames a second is one whole turn.
        for f in 1..=(TURN_SECS * 30.0).round() as u32 {
            spin.step(f64::from(f) / 30.0, true);
        }
        let off = spin.angle.min(TAU - spin.angle);
        assert!(off < 0.01, "{}", spin.angle);
        // Up to speed and to a stop in SPIN_UP_SECS, a little each frame.
        let mut spin = Spin::default();
        spin.step(1.0, false);
        let mut at = 1.0;
        for _ in 0..(SPIN_UP_SECS * 30.0).round() as usize {
            at += 1.0 / 30.0;
            assert!(spin.speed < 1.0);
            spin.step(at, true);
        }
        assert!((spin.speed - 1.0).abs() < 1e-4, "{}", spin.speed);
        for _ in 0..(SPIN_UP_SECS * 30.0).round() as usize {
            at += 1.0 / 30.0;
            spin.step(at, false);
        }
        assert_eq!(spin.speed, 0.0);
        let still = spin.angle;
        spin.step(at + 1.0, false);
        assert_eq!(spin.angle, still);
        // A long gap (a hidden window) is not a jump of many turns.
        spin.speed = 1.0;
        spin.step(at + 100.0, true);
        assert!((spin.angle - still).rem_euclid(TAU) <= 0.1 * TAU / TURN_SECS + 1e-4);
    }
}
