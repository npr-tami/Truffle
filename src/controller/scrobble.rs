//! Last.fm on the controller side: feeding the scrobbler (see `crate::lastfm`) with the playback
//! state, and handling the Last.fm commands and background results.
//!
//! The scrobbler sees the playback state every time the controller publishes it
//! (`flush_playback`, after every loop iteration including the 1 s tick), together with a play id
//! that changes for every new play of a track. So crossfade hand-overs, repeat-one, skips, the
//! sleep-timer pause, logout and shutdown all reach it in order.

use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;

use super::Controller;
use super::jobs::Job;
use crate::lastfm::{Alert, Report};
use crate::model::Event;

/// At shutdown, how long to wait for a scrobble batch that is already on its way (within
/// `main.rs`'s 3 s grace for the controller).
const SEND_WAIT_AT_SHUTDOWN: Duration = Duration::from_secs(2);

impl Controller {
    /// A new play of the current track begins (a new load, or "Previous" restarting it): its
    /// listening time and scrobble are counted from scratch.
    pub(super) fn begin_new_play(&mut self) {
        self.play_id = self.play_id.wrapping_add(1);
    }

    /// Hand the current playback state to the scrobbler.
    pub(super) fn observe_for_scrobbling(&mut self) {
        self.lastfm.observe(self.play_id, self.pb.track.as_ref(), self.pb.status, Instant::now());
    }

    /// The current play ends on purpose (sleep timer, logout): scrobbled if it was heard long
    /// enough, otherwise dropped. Resuming it later counts as a new play.
    pub(super) fn finalise_scrobble_play(&mut self) {
        self.lastfm.finalise_play(Instant::now());
    }

    pub(super) fn cmd_connect_lastfm(&mut self, api_key: String, api_secret: String) {
        self.lastfm.connect(&api_key, &api_secret);
    }

    pub(super) fn cmd_cancel_lastfm_connect(&mut self) {
        self.lastfm.cancel_connect();
    }

    pub(super) fn cmd_disconnect_lastfm(&mut self) {
        self.lastfm.disconnect();
    }

    pub(super) fn cmd_set_lastfm_enabled(&mut self, on: bool) {
        self.update_settings(|s| s.lastfm_enabled = on);
        self.lastfm.set_enabled(on, Instant::now());
    }

    pub(super) fn cmd_send_held_lastfm_scrobbles(&mut self, to: String) {
        self.lastfm.send_held(&to, Instant::now());
    }

    pub(super) fn cmd_discard_lastfm_scrobbles(&mut self, keep_for: Option<String>) {
        self.lastfm.discard(keep_for.as_deref());
    }

    pub(super) fn on_lastfm_report(&mut self, report: Report) {
        let connection = self.lastfm.on_report(report, Instant::now());
        // Settings shows the details; this reaches the user while Settings is closed.
        match self.lastfm.take_alert() {
            Some(Alert::Problem(text)) => self.ui.send(Event::Error(text)),
            Some(Alert::Info(text)) => self.ui.send(Event::Status(text)),
            None => {}
        }
        let Some(connection) = connection else { return };
        // Remembered only once they worked, so a typo is never saved.
        self.update_settings(|s| {
            s.lastfm_api_key = Some(connection.api_key);
            s.lastfm_api_secret = Some(connection.api_secret);
            s.lastfm_enabled = true;
        });
        // Written now rather than after the usual delay, so a crash can't lose the new connection.
        self.save_settings();
    }

    /// After `shutdown`: wait briefly for the outcome of a scrobble batch that was already on its
    /// way, so plays Last.fm took are removed from the queue instead of being sent again at the
    /// next start. Other results are no longer wanted.
    pub(super) async fn finish_lastfm_send(&mut self, jobs: &mut UnboundedReceiver<Job>) {
        let deadline = tokio::time::Instant::now() + SEND_WAIT_AT_SHUTDOWN;
        while self.lastfm.is_sending() {
            match tokio::time::timeout_at(deadline, jobs.recv()).await {
                Ok(Some(Job::Lastfm(report))) => {
                    self.lastfm.on_report(report, Instant::now());
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    log::warn!("a scrobble send didn't finish before shutdown; it may be sent again at the next start");
                    break;
                }
            }
        }
    }
}
