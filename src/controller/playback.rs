//! Queue and engine side of the controller: what plays now, what plays next, play counting and
//! the `PlaybackState` the UI shows.
//!
//! A track that fails is not skipped blindly. librespot reports a download that stalled for about
//! 8 s (or a decode error) as an *end of track*, and only notices a dead connection when its
//! keep-alive times out (20 to 80 s), while every track load fails at once. So an end of track
//! well before the track's length counts as an interruption, and before an unavailable track is
//! skipped or an interrupted one retried, one cheap request checks that Spotify can be reached;
//! if it can't, the controller reconnects and resumes the same track afterwards.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::extras::CrossfadePlan;
use super::jobs::{Job, TracksPurpose};
use super::{Controller, Listen, drop_in_background, lock, user_error};
use crate::config::SoundSource;
use crate::engine::{Engine, EngineEvent};
use crate::model::{Event, PlayStatus, RepeatMode, ShuffleRules, Source, TrackInfo};
use crate::queue::{self, PlayQueue, SavedQueue};

/// A play is counted after this much listening (or at the end of the track).
const PLAY_COUNTS_AFTER_MS: u64 = 30_000;
/// "Previous" restarts the current track when more than this has played.
const PREVIOUS_RESTARTS_AFTER_MS: u32 = 3_000;
/// Stop instead of skipping forever when this many tracks in a row fail to load.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// Engine position reports closer than this to the extrapolated position are not re-sent to the UI.
const POSITION_DRIFT_MS: u32 = 1_500;
/// A seek to the same position this soon after the last one is ignored.
const SEEK_REPEAT_WINDOW: Duration = Duration::from_millis(500);
/// Length of the "Up next" preview sent to the UI.
const UP_NEXT_LEN: usize = 50;
/// An end of track reported more than this before the track's length is an interruption.
const EARLY_END_MARGIN_MS: u32 = 10_000;
/// An interrupted track resumes this much before where it was reported to stop: librespot waits
/// about 8 s for missing audio before it gives up, and the position kept running meanwhile.
const INTERRUPTION_REWIND_MS: u32 = 8_000;
/// A track that keeps stopping early on a working connection is retried this often, then skipped.
const MAX_INTERRUPTION_RETRIES: u32 = 2;
/// Reconnects waited for one track before its failures are taken as the track's own.
const MAX_RECONNECT_WAITS: u32 = 2;
/// Playing this long past the track's length without an end-of-track event counts as its end.
const END_WATCHDOG_MS: u64 = 10_000;
/// How much shorter or longer a YouTube match may be than Spotify's song (the match allows 12 s).
const YOUTUBE_LENGTH_SLACK_MS: u32 = 15_000;
/// How long a successful reachability check is trusted (so skipping several unplayable tracks in
/// a row needs only one check).
const REACHABLE_TRUST: Duration = Duration::from_secs(20);

/// Why the controller checks whether Spotify can be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeKind {
    /// The current track was reported unavailable.
    Unavailable,
    /// The current track stopped long before its end.
    Interrupted,
}

/// The record's view of a queue: songs heard this round that are no longer in the play order
/// (from an earlier queue of the same source), then the play order itself.
///
/// A shuffle with "Play every song before repeats" on is drawn as its round: every song heard
/// this round lies outside the needle, wherever the order has it. (A resumed round's order keeps
/// the songs heard so far at its end, to open the next round.) So the record reads the same "47
/// of 312" as the sleeves and the list's page. Without the rule it is the order as it plays.
fn deck_state(queue: &PlayQueue, history: &queue::PlayHistory, rules: &ShuffleRules) -> crate::model::DeckState {
    let order = queue.order();
    let pos = queue.pos().min(order.len());
    let in_order: std::collections::HashSet<usize> = order.iter().copied().collect();
    let tracks = queue.tracks();
    let played = history.played_in_cycle.get(&queue.source().cache_key());
    let heard = |i: usize| played.is_some_and(|p| p.contains(&tracks[i].uri));
    let mut full: Vec<u32> =
        (0..tracks.len()).filter(|&i| !in_order.contains(&i) && tracks[i].playable && heard(i)).map(|i| i as u32).collect();
    let now = if rules.no_repeat_until_exhausted && queue.is_shuffled() {
        let (passed, rest) = order.split_at(pos);
        let (current, after) = rest.split_first().map_or((None, rest), |(c, a)| (Some(c), a));
        // A resumed round keeps the songs heard so far at the end of its order (they only play
        // when the round starts over): those count as heard. Heard songs further up play again
        // (after stepping back with Previous), so they stay where they will play.
        let unheard_end = after.iter().rposition(|&i| !heard(i)).map_or(0, |k| k + 1);
        let (ahead, heard_tail) = after.split_at(unheard_end);
        full.extend(passed.iter().chain(heard_tail).filter(|&&i| heard(i)).map(|&i| i as u32));
        let now = full.len();
        full.extend(current.map(|&i| i as u32));
        full.extend(ahead.iter().map(|&i| i as u32));
        // Songs passed before the round started over (it can end mid-order) come round again at
        // the end.
        full.extend(passed.iter().filter(|&&i| !heard(i)).map(|&i| i as u32));
        now
    } else {
        let now = full.len() + pos;
        full.extend(order.iter().map(|&i| i as u32));
        now
    };
    crate::model::DeckState { source: queue.source().clone(), tracks: tracks.clone(), order: full, now, shuffled: queue.is_shuffled() }
}

/// How far each list's shuffle round has got, when that differs from what was `sent` last.
fn changed_round_progress(sent: Option<&HashMap<String, usize>>, history: &queue::PlayHistory) -> Option<Arc<HashMap<String, usize>>> {
    let progress = queue::round_progress(history);
    (sent != Some(&progress)).then(|| Arc::new(progress))
}

impl Controller {
    // ------------------------------------------------------------ PlaybackState bookkeeping

    pub(super) fn mark_playback(&mut self) {
        self.pb_dirty = true;
    }

    /// Send the playback state, the "Up next" preview, the record and the round progress if they
    /// changed since the last send.
    /// Runs after every loop iteration, so the scrobbler and listen along see every state here.
    pub(super) fn flush_playback(&mut self) {
        self.observe_for_scrobbling();
        self.share_playback();
        if self.pb_dirty {
            self.pb_dirty = false;
            self.ui.send(Event::Playback(self.pb.clone()));
        }
        let upnext_dirty = std::mem::take(&mut self.upnext_dirty);
        let round_dirty = std::mem::take(&mut self.round_dirty);
        if upnext_dirty {
            let rules = self.rules();
            let next: Vec<TrackInfo> = match &self.queue {
                Some(queue) => queue.upcoming_filtered(UP_NEXT_LEN, &rules, &self.history).into_iter().cloned().collect(),
                None => Vec::new(),
            };
            let same = match &self.last_upnext {
                Some(previous) => **previous == next,
                None => next.is_empty(),
            };
            if !same {
                let next = Arc::new(next);
                self.last_upnext = Some(next.clone());
                let queued = self.queue.as_ref().map_or(0, |q| q.queued().len());
                self.ui.send(Event::Queued(queued));
                self.ui.send(Event::UpNext(next));
                self.youtube_fetch_ahead();
            }
        }
        // The record draws the round, so a play that counts (or ends the round) moves it too.
        if upnext_dirty || round_dirty {
            let rules = self.rules();
            let deck = self.queue.as_ref().filter(|q| !q.is_empty()).map(|q| Arc::new(deck_state(q, &self.history, &rules)));
            if deck != self.last_deck {
                self.last_deck = deck.clone();
                self.ui.send(Event::Deck(deck));
            }
        }
        if round_dirty && let Some(progress) = changed_round_progress(self.last_round.as_deref(), &self.history) {
            self.last_round = Some(progress.clone());
            self.ui.send(Event::RoundProgress(progress));
        }
    }

    /// Change the status, keeping the listening clock and the position extrapolation right.
    pub(super) fn set_status(&mut self, status: PlayStatus) {
        if self.pb.status == status {
            return;
        }
        let now = Instant::now();
        if self.pb.status == PlayStatus::Playing {
            if let Some(since) = self.listen.since.take() {
                self.listen.heard_ms += since.elapsed().as_millis() as u64;
            }
            // Freeze the extrapolated position before leaving Playing.
            self.pb.position_ms = self.pb.current_position_ms();
            self.pb.position_at = Some(now);
        }
        if status == PlayStatus::Playing {
            self.listen.since = Some(now);
            self.pb.position_at = Some(now);
        }
        self.pb.status = status;
        self.mark_playback();
    }

    pub(super) fn set_position(&mut self, position_ms: u32) {
        self.pb.position_ms = position_ms;
        self.pb.position_at = Some(Instant::now());
        self.mark_playback();
    }

    /// Copy source / position / length / shuffle from the queue into the playback state.
    /// Called after every queue change, so it also schedules an "Up next" refresh.
    pub(super) fn sync_queue_info(&mut self) {
        self.upnext_dirty = true;
        let (source, pos, len, shuffle) = match &self.queue {
            Some(q) => (Some(q.source().clone()), q.pos(), q.len(), q.is_shuffled()),
            None => (None, 0, 0, lock(&self.settings).shuffle),
        };
        if self.pb.source != source || self.pb.queue_pos != pos || self.pb.queue_len != len || self.pb.shuffle != shuffle {
            self.pb.source = source;
            self.pb.queue_pos = pos;
            self.pb.queue_len = len;
            self.pb.shuffle = shuffle;
            self.mark_playback();
        }
    }

    fn current_uri(&self) -> Option<&str> {
        self.pb.track.as_ref().map(|t| t.uri.as_str())
    }

    pub(super) fn is_current(&self, uri: &str) -> bool {
        self.current_uri() == Some(uri)
    }

    fn heard_ms(&self) -> u64 {
        self.listen.heard_ms + self.listen.since.map_or(0, |since| since.elapsed().as_millis() as u64)
    }

    /// Count a play once 30 s of the current track have been heard.
    pub(super) fn record_if_heard(&mut self) {
        if !self.listen.recorded && self.pb.track.is_some() && self.heard_ms() >= PLAY_COUNTS_AFTER_MS {
            self.record_current_play();
        }
    }

    /// Bump the play count and the "played this cycle" set; once every track of the source has
    /// had its turn, the queue helper starts a new cycle.
    pub(super) fn record_current_play(&mut self) {
        if self.listening() {
            // The host's song counts as a play; it has no round of its own.
            if let Some(track) = &self.pb.track {
                let count = self.history.play_counts.entry(track.uri.clone()).or_insert(0);
                *count = count.saturating_add(1);
            }
            self.listen.recorded = true;
            self.state_dirty = true;
            return;
        }
        let rules = self.rules();
        let (Some(queue), Some(track)) = (&self.queue, &self.pb.track) else { return };
        queue::record_play_and_update_cycle(&mut self.history, queue.source(), &track.uri, queue.tracks(), &rules);
        self.listen.recorded = true;
        self.state_dirty = true;
        self.round_dirty = true;
    }

    /// The current track is left without a counted play (skipped, blocked, or it can't be
    /// played). It still had its turn in this shuffle cycle, so it neither holds the cycle open
    /// nor leads every new shuffle. Its play count is not touched.
    fn mark_current_seen(&mut self) {
        if self.listen.recorded || self.listen.seen {
            return;
        }
        if self.listening() {
            self.listen.seen = true;
            return;
        }
        let rules = self.rules();
        let (Some(queue), Some(track)) = (&self.queue, &self.pb.track) else { return };
        queue::mark_seen_and_update_cycle(&mut self.history, queue.source(), &track.uri, queue.tracks(), &rules);
        self.listen.seen = true;
        self.state_dirty = true;
        self.round_dirty = true;
    }

    /// Before moving away from the current track by choice (next, a picked row, another source,
    /// a block): count it as played if it was heard long enough, else as seen for this cycle.
    fn leave_current(&mut self) {
        self.record_if_heard();
        self.mark_current_seen();
    }

    /// The queue as saved in state.json, tagged with the account it belongs to.
    pub(super) fn saved_live_queue(&self) -> Option<SavedQueue> {
        super::listen::saved_queue(self)
    }

    // ------------------------------------------------------------ engine

    /// Make sure the audio engine exists (creating it if needed). Shows an error and returns
    /// false if it can't be started.
    pub(super) fn ensure_engine(&mut self) -> bool {
        if self.engine.is_some() {
            return true;
        }
        let Some(session) = self.session.clone() else {
            self.ui.send(Event::Error("You're not logged in to Spotify yet.".into()));
            return false;
        };
        let mut settings = self.settings_snapshot();
        settings.sound_source = self.sound_source();
        match Engine::new(session, &settings, &self.paths, self.youtube.clone()) {
            Ok((engine, events)) => {
                engine.set_volume(self.pb.volume);
                self.engine = Some(engine);
                self.engine_rx = Some(events);
                // Equalizer and crossfade come from the settings; the sleep fade is ours.
                self.apply_fade_to_new_engine();
                true
            }
            Err(e) => {
                self.ui.send(Event::Error(user_error(
                    "Truffle couldn't start audio playback. Check that speakers or headphones are connected, then press play again.",
                    &e,
                )));
                false
            }
        }
    }

    /// The engine's event channel closed: its player thread is gone.
    pub(super) fn engine_events_closed(&mut self) {
        self.engine_rx = None;
        if let Some(engine) = self.engine.take() {
            log::error!("the audio engine stopped unexpectedly");
            drop_in_background(engine);
            self.pending_reload = true;
            self.want_play = false;
            if self.pb.status != PlayStatus::Stopped {
                self.set_status(PlayStatus::Paused);
            }
            self.ui.send(Event::Error(
                "Audio playback stopped unexpectedly. Press play to try again.".into(),
            ));
        }
    }

    pub(super) fn handle_engine_event(&mut self, event: EngineEvent) {
        log::trace!("engine event: {event:?}");
        match event {
            EngineEvent::Loading { uri } => {
                if self.is_current(&uri) && self.want_play {
                    self.set_status(PlayStatus::Loading);
                }
            }
            EngineEvent::Playing { uri, position_ms } => {
                if self.is_current(&uri) {
                    self.failures = 0;
                    self.pending_reload = false;
                    self.want_play = true;
                    self.set_status(PlayStatus::Playing);
                    self.set_position(position_ms);
                    self.listen_playing();
                }
            }
            EngineEvent::Paused { uri, position_ms } => {
                // A pause by the sleep timer has now taken effect (the queued audio played out).
                self.restore_fade_gain();
                if self.is_current(&uri) {
                    self.failures = 0;
                    self.pending_reload = false;
                    self.want_play = false;
                    self.set_status(PlayStatus::Paused);
                    self.set_position(position_ms);
                }
                // The engine pauses by itself when the speakers/headphones go away; say why.
                if let Some(problem) = self.engine.as_ref().and_then(|e| e.take_audio_problem()) {
                    self.ui.send(Event::Error(problem));
                    // Listening along would start the sound again within seconds, on whatever
                    // plays now (the speakers, with the headphones out).
                    self.end_listening(None);
                }
            }
            EngineEvent::Position { uri, position_ms } => {
                if self.is_current(&uri) {
                    let expected = self.pb.current_position_ms();
                    self.pb.position_ms = position_ms;
                    self.pb.position_at = Some(Instant::now());
                    if expected.abs_diff(position_ms) > POSITION_DRIFT_MS || self.pb.status != PlayStatus::Playing {
                        self.mark_playback();
                    }
                }
            }
            EngineEvent::EndOfTrack { uri } => {
                if self.is_current(&uri) {
                    self.on_end_of_track();
                }
            }
            EngineEvent::Unavailable { uri } => {
                // A failed preload is reported with the preloaded URI: ignore it here.
                if self.is_current(&uri) {
                    self.refresh_premium();
                    self.on_unavailable();
                }
            }
            EngineEvent::PreloadNext { uri } => {
                if self.is_current(&uri) {
                    self.preload_next();
                }
            }
            EngineEvent::Stopped => {
                self.restore_fade_gain();
                if self.pb.status == PlayStatus::Playing {
                    self.want_play = false;
                    self.set_status(PlayStatus::Stopped);
                }
            }
            EngineEvent::SessionInvalid => self.start_reconnect(),
        }
    }

    fn on_end_of_track(&mut self) {
        if self.end_handled {
            return;
        }
        self.end_handled = true;
        let Some(track) = self.pb.track.clone() else { return };
        let position = self.pb.current_position_ms();
        // A YouTube song is played from the whole downloaded file: an early end means its video
        // is shorter, not a dropped connection.
        if self.sound_source() == SoundSource::Spotify
            && track.duration_ms > 0
            && position.saturating_add(EARLY_END_MARGIN_MS) < track.duration_ms
        {
            self.on_interrupted(&track, position);
            return;
        }
        if !self.listen.recorded {
            self.record_current_play();
        }
        if self.listening() {
            // The host's song is over here: wait for their next one.
            self.want_play = false;
            self.pending_reload = true;
            self.set_status(PlayStatus::Paused);
            self.set_position(track.duration_ms);
            self.listen_ended();
            if self.sleep_at_end_of_track() {
                self.end_listening(None);
            }
            return;
        }
        // A sleep timer waiting for the end of this song: pause with the next one loaded.
        let sleep_now = self.sleep_at_end_of_track();
        let rules = self.rules();
        let repeat = self.repeat();
        let advanced = match self.queue.as_mut() {
            Some(queue) => queue.advance(repeat, &rules, &self.history).is_some(),
            None => false,
        };
        if advanced {
            // Normally still playing; if the user paused meanwhile, the next track waits paused.
            let play = self.want_play && !sleep_now;
            self.load_current(play, 0, true);
        } else {
            self.end_of_queue();
        }
    }

    /// The track stopped long before its end: not played, not to be skipped. Resume it where it
    /// stopped once Spotify can be reached; skip it only if it keeps stopping on a working
    /// connection (a damaged file).
    fn on_interrupted(&mut self, track: &TrackInfo, position: u32) {
        log::warn!(
            "\"{}\" stopped at {} s of {} s; treating it as an interruption",
            track.name,
            position / 1000,
            track.duration_ms / 1000
        );
        self.interruptions += 1;
        if self.interruptions > MAX_INTERRUPTION_RETRIES {
            self.skip_unplayable(&format!("Skipped \"{}\" because it kept stopping partway through.", track.name), true);
            return;
        }
        self.pending_reload = true;
        self.set_status(if self.want_play { PlayStatus::Loading } else { PlayStatus::Paused });
        self.set_position(position.saturating_sub(INTERRUPTION_REWIND_MS));
        self.wait_for_spotify(ProbeKind::Interrupted);
    }

    fn on_unavailable(&mut self) {
        self.source_problem = None;
        if self.sound_source() == SoundSource::YouTube {
            // Without yt-dlp nothing can play: say so instead of skipping every song.
            if let Err(missing) = self.youtube.tools() {
                self.stop_playback();
                self.listen_ended();
                self.ui.send(Event::Error(missing));
                return;
            }
            let problem = self.engine.as_ref().and_then(|e| e.take_source_problem());
            // YouTube refusing or out of reach: no song would play, so wait on this one, paused.
            if let Some(problem) = problem.as_deref().filter(|p| crate::youtube::is_service_wide(p)) {
                self.pending_reload = true;
                self.want_play = false;
                self.set_status(PlayStatus::Paused);
                // Listening along: not tried again until the host's next song.
                self.listen_ended();
                self.ui.send(Event::Error(problem.to_owned()));
                return;
            }
            self.source_problem = problem;
        }
        self.pending_reload = true;
        if self.want_play {
            self.set_status(PlayStatus::Loading);
        }
        self.wait_for_spotify(ProbeKind::Unavailable);
    }

    /// Before blaming the current track, make sure Spotify can be reached: while the connection is
    /// down every track fails. The outcome goes to `on_reachable`, or a reconnect is started and
    /// the track is reloaded when it succeeds.
    fn wait_for_spotify(&mut self, kind: ProbeKind) {
        if self.reconnect.is_some() || self.session.as_ref().is_some_and(|s| s.is_invalid()) {
            self.start_reconnect();
            return;
        }
        if kind == ProbeKind::Unavailable && self.last_reachable.is_some_and(|at| at.elapsed() < REACHABLE_TRUST) {
            self.on_reachable(kind);
            return;
        }
        let Some(library) = self.library.clone() else {
            self.on_reachable(kind);
            return;
        };
        let id = self.new_id();
        self.probe = Some((id, kind));
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let reachable = library.spotify_reachable().await;
            let _ = tx.send(Job::Reachability { epoch, id, reachable });
        });
    }

    pub(super) fn on_reachability(&mut self, id: u64, reachable: bool) {
        let Some((latest, kind)) = self.probe else { return };
        if latest != id {
            return;
        }
        self.probe = None;
        if reachable {
            self.last_reachable = Some(Instant::now());
            self.on_reachable(kind);
            return;
        }
        self.last_reachable = None;
        self.reconnect_waits += 1;
        if self.reconnect_waits > MAX_RECONNECT_WAITS {
            // Reconnecting worked before, yet this track still can't get through: its own problem.
            log::warn!("still failing after {MAX_RECONNECT_WAITS} reconnects; giving up on this track");
            self.on_reachable(kind);
        } else {
            log::warn!("Spotify can't be reached; reconnecting before trying the track again");
            self.start_reconnect();
        }
    }

    /// Spotify answers, so the failure is the track's own.
    fn on_reachable(&mut self, kind: ProbeKind) {
        match kind {
            ProbeKind::Interrupted => {
                let (play, position) = (self.want_play, self.pb.position_ms);
                self.load_current(play, position, false);
            }
            ProbeKind::Unavailable => {
                let name = self.pb.track.as_ref().map(|t| t.name.clone()).unwrap_or_default();
                log::warn!("track unavailable: {name}");
                // Ask Spotify about the song itself first: see `on_recheck`.
                if self.start_recheck() {
                    return;
                }
                let message = match self.source_problem.take() {
                    Some(problem) => format!("Skipped \"{name}\". {problem}"),
                    None => format!("Skipped \"{name}\" because Spotify can't play it right now."),
                };
                self.skip_unplayable(&message, true);
            }
        }
    }

    /// Looks the current track up again on Spotify, past the saved lists (Spotify sound only, and
    /// not while listening along). The answer goes to `on_recheck`. False if it can't be asked.
    fn start_recheck(&mut self) -> bool {
        if self.sound_source() != SoundSource::Spotify || self.listening() {
            return false;
        }
        let (Some(library), Some(uri)) = (self.library.clone(), self.current_uri().map(str::to_owned)) else {
            return false;
        };
        let id = self.new_id();
        self.recheck = Some((id, uri.clone()));
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = library.recheck(&uri).await;
            let _ = tx.send(Job::Recheck { epoch, id, uri, result });
        });
        true
    }

    /// A failed track, looked up again on a working connection. If Spotify has no audio for it any
    /// more, it is greyed out wherever it shows and skipped without counting as a failure: songs
    /// gone from Spotify never stop playback, and they can't come round again. Otherwise (or if
    /// the lookup failed) it is skipped as a failure, as before.
    pub(super) fn on_recheck(&mut self, id: u64, uri: &str, result: anyhow::Result<TrackInfo>) {
        let asked = self.recheck.as_ref().is_some_and(|(latest, u)| *latest == id && u == uri);
        if asked {
            self.recheck = None;
        }
        // Still waiting on this track: not moved on or reloaded since.
        let waiting = asked && self.is_current(uri) && self.pending_reload;
        let gone = matches!(&result, Ok(t) if !t.playable);
        if gone && (waiting || !self.is_current(uri)) {
            self.grey_out(uri);
        }
        if !waiting {
            return;
        }
        let name = self.pb.track.as_ref().map(|t| t.name.clone()).unwrap_or_default();
        if gone {
            log::info!("\"{name}\" has no audio on Spotify any more; greyed out and skipped");
            self.skip_unplayable(&format!("Skipped \"{name}\" because Spotify doesn't have it any more."), false);
            return;
        }
        if let Err(e) = &result {
            log::warn!("could not look up \"{name}\" again: {e:#}");
        }
        self.skip_unplayable(&format!("Skipped \"{name}\" because Spotify can't play it right now."), true);
    }

    /// `uri` can't be played any more: greyed out in the queue (it leaves the play order; as the
    /// current track the queue goes on after it), in its source's saved list, and on screen.
    fn grey_out(&mut self, uri: &str) {
        let queued = self.queue.as_ref().and_then(|q| Some((q.source().clone(), crate::library::greyed_out(q.tracks(), uri)?)));
        if let Some((source, tracks)) = queued {
            self.refresh_queue_tracks(&source, &Arc::new(tracks));
            // A mix is made of Liked Songs.
            let list = if matches!(source, Source::Mix(_)) { Source::LikedSongs } else { source };
            if let Some(library) = &self.library {
                library.grey_out(&list, uri);
            }
        }
        let shown = self.shown.as_ref().and_then(|(s, t)| Some((s.clone(), crate::library::greyed_out(t, uri)?)));
        if let Some((source, tracks)) = shown {
            self.show_tracks(source, Arc::new(tracks));
        }
    }

    /// Move past a track that can't be played. A `failure` counts toward stopping after several in
    /// a row (`note_failure`); a song gone from Spotify doesn't, as it says nothing about the
    /// connection.
    fn skip_unplayable(&mut self, message: &str, failure: bool) {
        if self.listening() {
            // Nothing to skip to: wait for the host's next song.
            self.ui.send(Event::Status(message.to_owned()));
            self.hold_for_host();
            return;
        }
        self.mark_current_seen();
        if failure && !self.note_failure() {
            return;
        }
        self.ui.send(Event::Status(message.to_owned()));
        let play = self.want_play;
        if self.step_forward() {
            self.load_current(play, 0, true);
        } else {
            self.end_of_queue();
        }
    }

    /// The computer was asleep for about `gap`: no audio played meanwhile, so neither the shown
    /// position nor the listening time may count it (nor may the end-of-track watchdog).
    pub(super) fn discount_suspend(&mut self, gap: Duration) {
        if self.pb.status != PlayStatus::Playing {
            return;
        }
        log::info!("no housekeeping for {gap:?} (the computer was probably asleep); not counting it as playback");
        let now = Instant::now();
        if let Some(at) = self.pb.position_at.as_mut() {
            *at = (*at + gap).min(now);
        }
        if let Some(since) = self.listen.since.as_mut() {
            *since = (*since + gap).min(now);
        }
        self.mark_playback();
    }

    /// The player can drop an end-of-track event (see `engine::Tracking`); without it the queue
    /// would sit at the end of the track forever. Called every tick.
    pub(super) fn check_end_watchdog(&mut self) {
        if self.pb.status != PlayStatus::Playing || self.end_handled {
            return;
        }
        let (Some(track), Some(at)) = (&self.pb.track, self.pb.position_at) else { return };
        if track.duration_ms == 0 {
            return;
        }
        let played = u64::from(self.pb.position_ms) + at.elapsed().as_millis() as u64;
        // A YouTube video may be longer than Spotify's song.
        let (length, slack) = match self.sound_source() {
            SoundSource::YouTube => (self.youtube.length(&track.uri).unwrap_or(0).max(track.duration_ms), u64::from(YOUTUBE_LENGTH_SLACK_MS)),
            SoundSource::Spotify => (track.duration_ms, 0),
        };
        if played > u64::from(length) + END_WATCHDOG_MS + slack {
            log::warn!("no end of track reported for \"{}\"; moving on", track.name);
            self.on_end_of_track();
        }
    }

    fn preload_next(&mut self) {
        // YouTube sound doesn't need the Spotify connection; listening along, the host decides.
        if self.listening() || self.reconnect.is_some() && self.sound_source() == SoundSource::Spotify {
            return;
        }
        let rules = self.rules();
        let repeat = self.repeat();
        // A crossfade starts the next track on the other player: preload it there.
        let crossfade = self.crossfade_planned();
        let (Some(engine), Some(queue)) = (&self.engine, &self.queue) else { return };
        let Some(next) = queue.peek_next(repeat, &rules, &self.history) else { return };
        if Some(next.uri.as_str()) == self.current_uri() && !crossfade {
            return;
        }
        engine.hint(next);
        let preloaded = if crossfade { engine.preload_for_crossfade(&next.uri) } else { engine.preload(&next.uri) };
        if let Err(e) = preloaded {
            log::debug!("could not preload {}: {e:#}", next.uri);
        }
        self.preload_requested = true;
    }

    /// After a change that affects "what plays next", replace an already issued preload.
    pub(super) fn refresh_preload(&mut self) {
        if self.preload_requested {
            self.preload_next();
        }
    }

    /// The queue changed what plays next (shuffle, rules, a refreshed track list, a block): there
    /// may be a different track (or one at all) to crossfade into, even if this track's crossfade
    /// point has passed, so look at it again; and replace the preload.
    fn upcoming_changed(&mut self) {
        self.crossfade_checked = false;
        self.refresh_preload();
    }

    // ------------------------------------------------------------ loading

    /// Load the queue's current track into the engine. `fresh` means a new play of the track
    /// (resets play counting); reloading the same track at a position keeps it.
    /// Tracks that can't even be handed to the engine are skipped (bounded by the failure limit).
    pub(super) fn load_current(&mut self, play: bool, position_ms: u32, fresh: bool) {
        self.load_current_with(play, position_ms, fresh, None);
    }

    /// [`load_current`](Self::load_current), optionally crossfading from the track playing now
    /// (always from the start, playing).
    pub(super) fn load_current_with(&mut self, play: bool, position_ms: u32, fresh: bool, crossfade: Option<CrossfadePlan>) {
        let mut position_ms = position_ms;
        let mut fresh = fresh;
        // A new load supersedes any reachability check or lookup still running for the old one.
        self.probe = None;
        self.recheck = None;
        self.crossfade_checked = false;
        if play {
            self.restore_fade_gain();
        }
        loop {
            let Some(track) = self.queue.as_ref().and_then(|q| q.current()).cloned() else {
                self.stop_playback();
                return;
            };
            let new_play = fresh || !self.is_current(&track.uri);
            self.want_play = play;
            self.ended = false;
            self.end_handled = false;
            let status = match (play, new_play, self.pb.status) {
                (false, _, _) => PlayStatus::Paused,
                (true, false, PlayStatus::Playing) => PlayStatus::Playing,
                (true, _, _) => PlayStatus::Loading,
            };
            // Status first: leaving Playing books the listening time to the previous track.
            self.set_status(status);
            if new_play {
                self.listen = Listen::default();
                self.begin_new_play();
                self.preload_requested = false;
                self.interruptions = 0;
                self.reconnect_waits = 0;
            }
            if self.pb.track.as_ref() != Some(&track) {
                self.pb.track = Some(track.clone());
            }
            self.set_position(position_ms);
            self.sync_queue_info();

            if self.reconnect.is_some() && self.sound_source() == SoundSource::Spotify {
                // Loaded once the connection is back.
                self.pending_reload = true;
                return;
            }
            if !self.ensure_engine() {
                self.pending_reload = true;
                self.want_play = false;
                self.set_status(PlayStatus::Paused);
                return;
            }
            if let Some(engine) = &self.engine {
                engine.hint(&track);
            }
            let result = match (&self.engine, crossfade) {
                (Some(engine), Some(plan)) => engine.crossfade_to(&track.uri, plan.fade, plan.outgoing_left),
                (Some(engine), None) => engine.load(&track.uri, play, position_ms),
                (None, _) => return,
            };
            match result {
                Ok(()) => {
                    self.pending_reload = false;
                    return;
                }
                Err(e) => {
                    log::warn!("could not load {}: {e:#}", track.uri);
                    if self.listening() {
                        self.hold_for_host();
                        return;
                    }
                    self.mark_current_seen();
                    if !self.note_failure() {
                        return;
                    }
                    if !self.step_forward() {
                        self.end_of_queue();
                        return;
                    }
                    position_ms = 0;
                    fresh = true;
                }
            }
        }
    }

    /// The host's song can't play here: paused, waiting for their next one.
    fn hold_for_host(&mut self) {
        self.want_play = false;
        self.pending_reload = true;
        self.set_status(PlayStatus::Paused);
        self.listen_ended();
    }

    /// Count a failed track; after too many in a row, stop with an explanation and return false.
    fn note_failure(&mut self) -> bool {
        self.failures += 1;
        if self.failures < MAX_CONSECUTIVE_FAILURES {
            return true;
        }
        self.failures = 0;
        self.stop_playback();
        self.ui.send(Event::Error(
            "Several tracks in a row couldn't be played, so Truffle stopped playback. Check your internet connection, then press play to try again.".into(),
        ));
        false
    }

    /// Move the queue forward for a skip (not a natural end): repeat-one moves on like repeat-all.
    fn step_forward(&mut self) -> bool {
        let rules = self.rules();
        let repeat = match self.repeat() {
            RepeatMode::One => RepeatMode::All,
            other => other,
        };
        match self.queue.as_mut() {
            Some(queue) => queue.advance(repeat, &rules, &self.history).is_some(),
            None => false,
        }
    }

    pub(super) fn stop_playback(&mut self) {
        if let Some(engine) = &self.engine {
            engine.stop();
        }
        self.probe = None;
        self.recheck = None;
        self.want_play = false;
        self.pending_reload = true;
        self.set_status(PlayStatus::Stopped);
        self.set_position(0);
    }

    pub(super) fn end_of_queue(&mut self) {
        self.stop_playback();
        self.ended = true;
        self.sync_queue_info();
    }

    /// Replace the queue with a new one for `source` and start playing.
    pub(super) fn start_source(
        &mut self,
        source: Source,
        tracks: Arc<Vec<TrackInfo>>,
        start_index: Option<usize>,
        shuffle: bool,
    ) {
        if self.listening() {
            // Asked for before listening along started.
            log::info!("not starting {source:?}: listening along");
            return;
        }
        self.leave_current();
        let rules = self.rules();
        let queue = PlayQueue::new(source, tracks, start_index, shuffle, &rules, &self.history);
        if queue.current().is_none() {
            self.ui.send(Event::Error(
                "There's nothing playable here. The tracks may be unavailable in your country, removed from Spotify, or local files on another device.".into(),
            ));
            return;
        }
        self.queue = Some(queue);
        self.saved_queue = None;
        self.failures = 0;
        self.state_dirty = true;
        self.load_current(true, 0, true);
    }

    /// A row picked in the source that is already playing, with the same shuffle setting: move to
    /// that track inside the current play order instead of building a new one, so the rest of the
    /// shuffle (and the "Up next" list) stays as it was. Returns false when a new queue is needed:
    /// another source, a different shuffle mode, a changed track list, or a queue that ran out.
    fn jump_within_queue(&mut self, source: &Source, tracks: &Arc<Vec<TrackInfo>>, index: usize, shuffle: bool) -> bool {
        if self.ended {
            return false;
        }
        let Some(queue) = self.queue.as_ref() else { return false };
        let same_list = Arc::ptr_eq(queue.tracks(), tracks) || **queue.tracks() == **tracks;
        if queue.source() != source || queue.is_shuffled() != shuffle || !same_list {
            return false;
        }
        self.leave_current();
        let rules = self.rules();
        if self.queue.as_mut().and_then(|q| q.jump_to_track(index, &rules)).is_none() {
            return false;
        }
        self.failures = 0;
        self.state_dirty = true;
        self.load_current(true, 0, true);
        true
    }

    /// Rebuild the queue saved at the last shutdown and load its track paused (never autoplay).
    pub(super) fn apply_restore(&mut self, saved: &SavedQueue, tracks: Arc<Vec<TrackInfo>>) {
        self.saved_queue = None;
        let rules = self.rules();
        match PlayQueue::restore(saved, tracks, &rules, &self.history) {
            Some(queue) if queue.current().is_some() => {
                log::info!("restored the saved queue ({} tracks)", queue.len());
                // The saved position only applies if the saved track is still the current one.
                let position_ms = saved.resume_position_ms(&queue);
                // Listening along: it waits aside for when that ends.
                if self.listening() {
                    self.along.put_aside(queue, position_ms);
                    return;
                }
                self.queue = Some(queue);
                self.load_current(false, position_ms, true);
            }
            _ => log::info!("the saved queue no longer matches its playlist; starting fresh"),
        }
    }

    /// Keep the play queue in step with a freshly loaded track list of its source.
    pub(super) fn refresh_queue_tracks(&mut self, source: &Source, tracks: &Arc<Vec<TrackInfo>>) {
        // Listening along, the queue holds only the host's song (its album's list must not fill it).
        if self.listening() {
            return;
        }
        let rules = self.rules();
        let Some(queue) = self.queue.as_mut() else { return };
        if queue.source() != source || Arc::ptr_eq(queue.tracks(), tracks) || **queue.tracks() == **tracks {
            return;
        }
        queue.update_tracks(tracks.clone(), &rules, &self.history);
        let current = queue.current().cloned();
        if let Some(current) = current
            && self.pb.track.as_ref().is_some_and(|t| t.uri == current.uri && *t != current)
        {
            self.pb.track = Some(current);
            self.mark_playback();
        }
        self.state_dirty = true;
        self.sync_queue_info();
        self.upcoming_changed();
    }

    /// The freshest track list we know for `source`: what the UI shows (its `start_index`
    /// refers to that list), else the library cache, else the queue's copy.
    fn known_tracks(&self, source: &Source) -> Option<Arc<Vec<TrackInfo>>> {
        if let Some((shown_source, tracks)) = &self.shown
            && shown_source == source
        {
            return Some(tracks.clone());
        }
        if let Some(tracks) = self.library.as_ref().and_then(|l| l.cached_tracks(source)) {
            return Some(tracks);
        }
        self.queue.as_ref().filter(|q| q.source() == source).map(|q| q.tracks().clone())
    }

    // ------------------------------------------------------------ commands

    pub(super) fn cmd_play_source(&mut self, source: Source, start_index: Option<usize>, shuffle: bool) {
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::Error("Log in to Spotify to play music.".into()));
            return;
        };
        self.update_settings(|s| s.shuffle = shuffle);
        match self.known_tracks(&source) {
            Some(tracks) => {
                // Supersedes any play or restore still loading.
                self.play_req = self.new_id();
                let jumped = start_index.is_some_and(|i| self.jump_within_queue(&source, &tracks, i, shuffle));
                if !jumped {
                    self.start_source(source, tracks, start_index, shuffle);
                }
            }
            None => {
                self.ui.send(Event::Status("Loading tracks…".into()));
                let report = self.open_source.as_ref() == Some(&source);
                self.play_req = self.spawn_tracks(library, source, false, TracksPurpose::Play { start_index, shuffle }, report);
            }
        }
    }

    /// Start a list over: the songs heard in its round so far count as unheard again, and it plays
    /// in a fresh random order. The song playing now keeps its turn in the round it played in.
    pub(super) fn cmd_new_round(&mut self, source: Source) {
        if self.library.is_some() {
            self.leave_current();
            queue::reset_cycle(&mut self.history, &source);
            self.round_dirty = true;
            self.state_dirty = true;
        }
        self.cmd_play_source(source, None, true);
    }

    /// Pause whatever plays (or is about to).
    pub(super) fn pause_playback(&mut self) {
        self.want_play = false;
        // Pausing needs no connection: during a reconnect the player may still be playing audio
        // it has already downloaded.
        if let Some(engine) = &self.engine {
            engine.pause();
        }
        if self.pb.status != PlayStatus::Stopped {
            self.set_status(PlayStatus::Paused);
        }
    }

    pub(super) fn cmd_play_pause(&mut self) {
        match self.pb.status {
            PlayStatus::Playing | PlayStatus::Loading => self.pause_playback(),
            PlayStatus::Paused => {
                self.want_play = true;
                self.restore_fade_gain();
                if self.reconnect.is_some() && self.sound_source() == SoundSource::Spotify {
                    self.pending_reload = true;
                    self.set_status(PlayStatus::Loading);
                } else if self.probe.is_some() {
                    // A reachability check decides how to go on; it uses `want_play`.
                    self.set_status(PlayStatus::Loading);
                } else if self.pending_reload || self.engine.is_none() {
                    let position = self.pb.position_ms;
                    self.load_current(true, position, false);
                } else {
                    if let Some(engine) = &self.engine {
                        engine.play();
                    }
                    self.set_status(PlayStatus::Playing);
                }
            }
            PlayStatus::Stopped => {
                if self.queue.is_none() {
                    self.ui.send(Event::Status("Pick a playlist or Liked Songs to start playing.".into()));
                    return;
                }
                if self.ended {
                    // Start the finished queue over (a new cycle when shuffled).
                    let rules = self.rules();
                    let restarted = match self.queue.as_mut() {
                        Some(queue) => queue.advance(RepeatMode::All, &rules, &self.history).is_some(),
                        None => false,
                    };
                    if restarted {
                        self.load_current(true, 0, true);
                    } else {
                        self.ui.send(Event::Status("Nothing in this queue can be played.".into()));
                    }
                } else {
                    let position = self.pb.position_ms;
                    self.load_current(true, position, false);
                }
            }
        }
    }

    /// Add to queue / Play next: the song plays after the current one (or the songs queued).
    pub(super) fn cmd_queue(&mut self, track: TrackInfo, next: bool) {
        let Some(queue) = self.queue.as_mut() else {
            self.ui.send(Event::Status("Play something first; queued songs play after it.".into()));
            return;
        };
        let name = track.name.clone();
        queue.add_queued(track, next);
        // The song loaded ahead may no longer be the next one: load the new next one ahead.
        self.refresh_preload();
        self.sync_queue_info();
        self.ui.send(Event::Status(if next { format!("{name} plays next") } else { format!("Added {name} to the queue") }));
    }

    pub(super) fn cmd_unqueue(&mut self, at: Option<usize>) {
        let Some(queue) = self.queue.as_mut() else { return };
        match at {
            Some(k) => queue.remove_queued(k),
            None => queue.clear_queued(),
        }
        self.refresh_preload();
        self.sync_queue_info();
    }

    pub(super) fn cmd_next(&mut self) {
        if self.queue.is_none() {
            return;
        }
        self.leave_current();
        self.failures = 0;
        if self.step_forward() {
            self.load_current(true, 0, true);
        } else {
            self.end_of_queue();
        }
    }

    pub(super) fn cmd_previous(&mut self) {
        if self.queue.is_none() {
            return;
        }
        if self.pb.status != PlayStatus::Stopped
            && self.pb.track.is_some()
            && self.pb.current_position_ms() > PREVIOUS_RESTARTS_AFTER_MS
        {
            // Hearing the track again from the start is a new play for scrobbling.
            self.begin_new_play();
            self.cmd_seek(0);
            return;
        }
        self.record_if_heard();
        self.failures = 0;
        let rules = self.rules();
        let moved = match self.queue.as_mut() {
            Some(queue) => queue.back(&rules, &self.history).is_some(),
            None => false,
        };
        if moved {
            self.load_current(true, 0, true);
        }
    }

    pub(super) fn cmd_seek(&mut self, position_ms: u32) {
        // The same seek again right away (a UI repeating itself) would only restart the
        // engine's seek over and over.
        let now = Instant::now();
        if self.last_seek.is_some_and(|(at_ms, at)| at_ms == position_ms && now.duration_since(at) < SEEK_REPEAT_WINDOW) {
            return;
        }
        self.last_seek = Some((position_ms, now));
        let Some(track) = &self.pb.track else { return };
        // The crossfade point may now lie ahead again.
        self.crossfade_checked = false;
        let mut position_ms = position_ms;
        let length = match self.sound_source() {
            SoundSource::YouTube => self.youtube.length(&track.uri).unwrap_or(track.duration_ms),
            SoundSource::Spotify => track.duration_ms,
        };
        if length > 1_000 {
            position_ms = position_ms.min(length - 1_000);
        }
        if self.pb.status == PlayStatus::Stopped {
            self.load_current(false, position_ms, false);
        } else if !self.pending_reload
            && let Some(engine) = &self.engine
        {
            // The engine has the track; seeking needs no session for audio it has already
            // downloaded, so this works during a reconnect too.
            engine.seek(position_ms);
            self.set_position(position_ms);
        } else {
            self.set_position(position_ms);
            // During a reconnect or a reachability check, the track is reloaded at this position
            // when that is over.
            if self.reconnect.is_none() && self.probe.is_none() {
                let play = self.want_play;
                self.load_current(play, position_ms, false);
            }
        }
    }

    pub(super) fn cmd_set_volume(&mut self, volume: u8) {
        let volume = volume.min(100);
        if let Some(engine) = &self.engine {
            engine.set_volume(volume);
        }
        if self.pb.volume != volume {
            self.pb.volume = volume;
            self.mark_playback();
        }
        self.update_settings(|s| s.volume = volume);
    }

    pub(super) fn cmd_set_shuffle(&mut self, on: bool) {
        self.update_settings(|s| s.shuffle = on);
        let rules = self.rules();
        if let Some(queue) = self.queue.as_mut() {
            queue.set_shuffle(on, &rules, &self.history);
            self.state_dirty = true;
        }
        self.sync_queue_info();
        self.upcoming_changed();
    }

    pub(super) fn cmd_set_repeat(&mut self, mode: RepeatMode) {
        self.update_settings(|s| s.repeat = mode);
        // There may be a next track to crossfade into now (or no longer).
        self.crossfade_checked = false;
        if self.pb.repeat != mode {
            self.pb.repeat = mode;
            self.mark_playback();
        }
        self.refresh_preload();
    }

    pub(super) fn cmd_set_shuffle_rules(&mut self, rules: ShuffleRules) {
        if self.rules() == rules {
            return;
        }
        self.update_settings(|s| s.shuffle_rules = rules.clone());
        if let Some(queue) = self.queue.as_mut()
            && queue.is_shuffled()
        {
            queue.reshuffle(&rules, &self.history);
            self.state_dirty = true;
        }
        self.sync_queue_info();
        self.upcoming_changed();
    }

    pub(super) fn cmd_reshuffle(&mut self) {
        if self.queue.is_none() {
            self.ui.send(Event::Status("Start playing something first, then you can reshuffle it.".into()));
            return;
        }
        let rules = self.rules();
        if let Some(queue) = self.queue.as_mut() {
            if queue.is_shuffled() {
                queue.reshuffle(&rules, &self.history);
            } else {
                queue.set_shuffle(true, &rules, &self.history);
            }
        }
        self.update_settings(|s| s.shuffle = true);
        self.state_dirty = true;
        self.sync_queue_info();
        self.upcoming_changed();
        self.ui.send(Event::Status("Shuffled the rest of the queue.".into()));
    }

    pub(super) fn cmd_set_blocked(&mut self, uri: String, blocked: bool) {
        let changed = if blocked {
            self.history.blocked.insert(uri.clone())
        } else {
            self.history.blocked.remove(&uri)
        };
        let rules = self.rules();
        if changed {
            self.state_dirty = true;
            // "Up next" leaves out blocked tracks when the rule is on.
            self.upnext_dirty = true;
            // A shuffled order built while the track was blocked left it out: put it back.
            if !blocked
                && let Some(queue) = self.queue.as_mut()
                && queue.reinsert(&uri, &rules, &self.history)
            {
                self.sync_queue_info();
            }
        }
        self.send_blocked();
        if blocked && self.is_current(&uri) && self.queue.is_some() && rules.skip_blocked && !self.listening() {
            self.ui.send(Event::Status("Blocked. Skipping to the next track.".into()));
            self.leave_current();
            let play = self.want_play;
            self.failures = 0;
            if self.step_forward() {
                self.load_current(play, 0, true);
            } else {
                self.end_of_queue();
            }
        } else {
            self.upcoming_changed();
        }
        if changed {
            // A block is a deliberate choice: write it now rather than at the next periodic save
            // (a crash in between would lose it).
            self.save_state();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use super::{changed_round_progress, deck_state};
    use crate::config::{Paths, Settings};
    use crate::controller::{Controller, UiLink};
    use crate::model::{PlayStatus, RepeatMode, ShuffleRules, Source, TrackInfo};
    use crate::queue::{self, PlayHistory, PlayQueue};

    fn track(i: usize) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{i}"),
            name: String::new(),
            artists: Vec::new(),
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 180_000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    /// A controller with no session (no engine: every load ends paused, to be loaded again),
    /// playing a list of `n` songs from the first, unshuffled. It writes into a temp folder.
    fn playing_list(name: &str, n: usize) -> Controller {
        let dir = std::env::temp_dir().join(format!("truffle-playback-{name}-{}", std::process::id()));
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        let (events, _) = std::sync::mpsc::channel();
        let ui = UiLink { events, ctx: egui::Context::default() };
        let (jobs, _) = tokio::sync::mpsc::unbounded_channel();
        let mut c = Controller::new(paths, Arc::new(Mutex::new(Settings::default())), ui, jobs);
        let rules = c.rules();
        let tracks: Arc<Vec<TrackInfo>> = Arc::new((0..n).map(track).collect());
        c.queue = Some(PlayQueue::new(Source::Playlist("spotify:playlist:a".into()), tracks, Some(0), false, &rules, &c.history));
        c.load_current(true, 0, true);
        c
    }

    fn done(c: Controller) {
        let dir = c.paths.config.parent().map(std::path::Path::to_path_buf);
        drop(c);
        if let Some(dir) = dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The current song failed on a working connection, and Spotify's fresh answer about it comes.
    fn answer(c: &mut Controller, id: u64, answer: anyhow::Result<TrackInfo>) {
        let uri = c.current_uri().expect("a current song").to_owned();
        c.recheck = Some((id, uri.clone()));
        c.on_recheck(id, &uri, answer);
    }

    fn gone(i: usize) -> anyhow::Result<TrackInfo> {
        Ok(TrackInfo { playable: false, ..track(i) })
    }

    #[test]
    fn songs_gone_from_spotify_are_greyed_out_and_never_stop_playback() {
        let mut c = playing_list("gone", 8);
        // Six in a row, one more than the failures that stop playback.
        for i in 0..6 {
            assert!(c.is_current(&format!("spotify:track:{i}")));
            answer(&mut c, i as u64, gone(i));
        }
        assert!(c.is_current("spotify:track:6"), "each one was skipped");
        assert_ne!(c.pb.status, PlayStatus::Stopped);
        assert_eq!(c.failures, 0);
        let q = c.queue.as_ref().expect("a queue");
        let playable: Vec<bool> = q.tracks().iter().map(|t| t.playable).collect();
        assert_eq!(playable, [false, false, false, false, false, false, true, true], "greyed out");
        assert_eq!(q.order(), [6, 7], "only the songs left play from now on");
        done(c);
    }

    #[test]
    fn songs_spotify_still_has_count_as_failures_and_stop_playback_after_a_few() {
        let mut c = playing_list("failing", 8);
        for i in 0..5 {
            // Spotify lists audio for the song, or couldn't be asked.
            let result = if i % 2 == 0 { Ok(track(i)) } else { Err(anyhow::anyhow!("no answer")) };
            answer(&mut c, i as u64, result);
        }
        assert_eq!(c.pb.status, PlayStatus::Stopped);
        assert!(c.queue.as_ref().expect("a queue").tracks().iter().all(|t| t.playable), "nothing greyed out");
        done(c);
    }

    #[test]
    fn a_late_answer_greys_the_song_out_but_skips_nothing() {
        let mut c = playing_list("late", 4);
        c.recheck = Some((1, "spotify:track:0".into()));
        // On to the next song before Spotify answered about the first.
        let rules = c.rules();
        c.queue.as_mut().expect("a queue").advance(RepeatMode::Off, &rules, &c.history);
        c.load_current(false, 0, true);
        c.on_recheck(1, "spotify:track:0", gone(0));
        assert!(c.is_current("spotify:track:1"), "still on the song picked");
        let q = c.queue.as_ref().expect("a queue");
        assert!(!q.tracks()[0].playable);
        assert_eq!(q.order(), [1, 2, 3]);
        // A song Spotify still has stays as it was.
        c.on_recheck(2, "spotify:track:2", Ok(track(2)));
        assert!(c.queue.as_ref().expect("a queue").tracks()[2].playable);
        done(c);
    }

    #[test]
    fn the_record_shows_the_round_heard_so_far_outside_the_needle() {
        let list = Source::Playlist("spotify:playlist:a".into());
        let tracks: Arc<Vec<TrackInfo>> = Arc::new((0..10).map(track).collect());
        let mut history = PlayHistory::default();
        for t in &tracks[..4] {
            queue::record_play(&mut history, &list, &t.uri);
        }
        // A resumed round: the new order keeps the four heard songs at its end, the record puts
        // them outside, so it reads "4 of 10" like the sleeves.
        let rules = ShuffleRules::default();
        let q = PlayQueue::new(list.clone(), tracks.clone(), None, true, &rules, &history);
        let deck = deck_state(&q, &history, &rules);
        assert_eq!(deck.now, 4);
        assert_eq!(deck.order[..4].iter().copied().collect::<HashSet<u32>>(), (0..4).collect());
        assert_eq!(deck.order[4] as usize, q.order()[q.pos()], "the needle is on the playing song");
        let mut every = deck.order.clone();
        every.sort_unstable();
        assert_eq!(every, (0..10).collect::<Vec<u32>>(), "every song once");
        // The playing song counting changes nothing on the record.
        queue::record_play(&mut history, &list, &tracks[deck.order[4] as usize].uri);
        assert_eq!(deck_state(&q, &history, &rules).now, 4);
        // Without the rule the record is the order as it plays.
        let off = ShuffleRules { no_repeat_until_exhausted: false, ..rules };
        let q = PlayQueue::new(list, tracks, None, true, &off, &history);
        assert_eq!(deck_state(&q, &history, &off).now, 0);
    }

    #[test]
    fn songs_heard_again_after_stepping_back_stay_where_they_play() {
        let list = Source::Playlist("spotify:playlist:a".into());
        let tracks: Arc<Vec<TrackInfo>> = Arc::new((0..6).map(track).collect());
        let mut history = PlayHistory::default();
        let rules = ShuffleRules::default();
        let mut q = PlayQueue::new(list.clone(), tracks.clone(), None, true, &rules, &history);
        // Two songs heard, then Previous twice: both lie ahead of the needle again.
        let first = q.order()[0];
        let second = q.order()[1];
        queue::record_play(&mut history, &list, &tracks[first].uri);
        q.advance(crate::model::RepeatMode::Off, &rules, &history);
        queue::record_play(&mut history, &list, &tracks[second].uri);
        q.advance(crate::model::RepeatMode::Off, &rules, &history);
        q.back(&rules, &history);
        q.back(&rules, &history);
        let deck = deck_state(&q, &history, &rules);
        let at = |i: usize| deck.order.iter().position(|&o| o as usize == i).unwrap();
        assert_eq!(deck.order[deck.now] as usize, q.order()[q.pos()], "the needle is on the playing song");
        assert_eq!(deck.order[deck.now + 1] as usize, q.order()[q.pos() + 1], "the next groove is the next song");
        assert!(at(second) > deck.now, "a song that plays again is ahead of the needle");
    }

    #[test]
    fn round_progress_is_sent_only_when_it_changed() {
        let list = Source::Playlist("spotify:playlist:a".into());
        let mut history = PlayHistory::default();
        // Nothing sent yet: even "no rounds begun" is news (the login case).
        let sent = changed_round_progress(None, &history).expect("first send");
        assert!(sent.is_empty());
        assert!(changed_round_progress(Some(&sent), &history).is_none());

        queue::record_play(&mut history, &list, "spotify:track:1");
        let sent = changed_round_progress(Some(&sent), &history).expect("a counted play");
        assert_eq!(sent.get(&list.cache_key()), Some(&1));
        // Hearing the same song again in this round changes nothing.
        queue::record_play(&mut history, &list, "spotify:track:1");
        assert!(changed_round_progress(Some(&sent), &history).is_none());
        // A skip marks a song as having had its turn.
        queue::mark_seen(&mut history, &list, "spotify:track:2");
        let sent = changed_round_progress(Some(&sent), &history).expect("a skip");
        assert_eq!(sent.get(&list.cache_key()), Some(&2));
        // A new round starts from nothing.
        queue::reset_cycle(&mut history, &list);
        let sent = changed_round_progress(Some(&sent), &history).expect("a round reset");
        assert!(sent.get(&list.cache_key()).is_none());
    }
}
