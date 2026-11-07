//! Last.fm scrobbling: desktop auth, "now playing", scrobble rules, offline queue.
//! See https://www.last.fm/api/scrobbling for the rules.
//!
//! The [`Scrobbler`] lives inside the controller, which drives it synchronously:
//! - [`observe`](Scrobbler::observe) with every playback state (the controller calls it from the
//!   one place that publishes the playback state, so it sees every change, crossfade hand-overs
//!   included, and at least once a second while playing),
//! - [`finalise_play`](Scrobbler::finalise_play) when a play ends on purpose (sleep timer,
//!   logout) and [`shutdown`](Scrobbler::shutdown) at exit,
//! - the four Last.fm commands, [`on_tick`](Scrobbler::on_tick) every second and
//!   [`network_up`](Scrobbler::network_up) after Spotify reconnects.
//!
//! Network calls run in spawned tokio tasks (one scrobble batch at a time) and come back as a
//! [`Report`] through the sink the controller provides (its job channel), which it hands to
//! [`on_report`](Scrobbler::on_report). So the controller loop never waits for Last.fm. The queue
//! of unsent scrobbles is written to disk whenever it changes, before anything is sent.
//!
//! Scrobbling happens only while scrobbling is switched on *and* Last.fm is connected (an API key
//! and secret in the settings plus a session). Plays outside that are not queued for later.
//! Each queued play remembers the account it was recorded for and is only sent to that one,
//! unless the user chooses otherwise in Settings.
//!
//! When Last.fm refuses the API key or secret, sending stops ("stopped") until the user connects
//! again or disconnects; plays are still queued for later meanwhile. The refusal is saved with the
//! session, so it is announced once rather than at every start, and each start checks once
//! whether Last.fm accepts Truffle again.
//!
//! The shared secret and the session key are never logged.

mod api;
#[cfg(test)]
mod mock_tests;
mod queue;
mod tracker;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use self::api::{Credentials, ErrorClass, ItemResult, LfmError, Session};
use self::queue::{ScrobbleQueue, same_user};
use self::tracker::{Output, PendingScrobble, ScrobbleTrack, Tracker};
use crate::config::{self, Paths, Settings};
use crate::model::{LastfmStatus, PlayStatus, TrackInfo};

/// `auth.getSession` is polled this often while the user approves Truffle in the browser.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Waiting for the approval stops after this long.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Pause between two scrobble batches.
const BATCH_GAP: Duration = Duration::from_secs(1);
/// Waits after failures: 15 s, 30 s, 1, 2, 5 and 10 min, then 30 min (each ±20 %).
const BACKOFF_SECS: [u64; 7] = [15, 30, 60, 120, 300, 600, 1800];
/// Rate limited (error 29): wait at least this long.
const SLOW_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// The daily scrobble limit was reached (ignored code 5).
const DAILY_LIMIT_WAIT: Duration = Duration::from_secs(60 * 60);
/// "Operation failed" (error 8) for a single scrobble: dropped after this many tries.
const MAX_ATTEMPTS: u8 = 3;
/// A send that hasn't reported back after this long is assumed lost (requests time out at 20 s).
const SEND_LOST_AFTER: Duration = Duration::from_secs(120);

/// Receives every new [`LastfmStatus`] (the controller wraps it in `Event::Lastfm`).
pub type StatusSink = Arc<dyn Fn(LastfmStatus) + Send + Sync>;
/// Takes [`Report`]s from background tasks back to the controller loop.
pub type ReportSink = Arc<dyn Fn(Report) + Send + Sync>;

/// The outcome of background Last.fm work, for [`Scrobbler::on_report`].
pub struct Report(ReportKind);

enum ReportKind {
    Connect { attempt: u64, result: Result<(Credentials, Session), ConnectFailure> },
    Scrobbled { generation: u64, batch: Vec<PendingScrobble>, result: Result<Vec<ItemResult>, LfmError> },
    /// Only failures are reported.
    NowPlaying { generation: u64, error: LfmError },
}

#[derive(Debug)]
enum ConnectFailure {
    Api(LfmError),
    Browser,
    /// The request token expired or was refused (codes 15 and 4 from `auth.getSession`).
    Expired,
    TimedOut,
}

impl ConnectFailure {
    fn user_message(&self) -> String {
        match self {
            ConnectFailure::Api(e) => problem_message(e),
            ConnectFailure::Browser => {
                "Truffle couldn't open your web browser. Make sure Windows has a default browser set, then try again.".into()
            }
            ConnectFailure::Expired => {
                "The Last.fm approval expired before it was finished. Click Connect to try again.".into()
            }
            ConnectFailure::TimedOut => {
                "Truffle stopped waiting for the Last.fm approval after 10 minutes. Click Connect to try again.".into()
            }
        }
    }
}

/// Settings to save after a successful connection (the controller owns the settings file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewConnection {
    pub api_key: String,
    pub api_secret: String,
}

/// A message for outside Settings (a toast), see [`Scrobbler::take_alert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    /// Scrobbling stopped or connecting failed: the user has to act.
    Problem(String),
    /// Good news (connected).
    Info(String),
}

impl Alert {
    #[cfg(test)]
    pub fn text(&self) -> &str {
        match self {
            Alert::Problem(text) | Alert::Info(text) => text,
        }
    }
}

/// `lastfm_session.json`: the session, plus the error code while Last.fm refuses the API key or
/// secret, so that a lasting refusal is announced once rather than at every start. No `Debug`:
/// it holds the session key.
#[derive(Serialize, Deserialize)]
struct SavedSession {
    username: String,
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<u32>,
    /// Copy of the API key and secret the session was made with, so a sign-in survives
    /// settings.json losing them (see [`restore_credentials`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_secret: Option<String>,
}

impl SavedSession {
    fn credentials(&self) -> Option<Credentials> {
        credentials(self.api_key.as_deref(), self.api_secret.as_deref())
    }
}

/// Fill a missing Last.fm API key / secret in `settings` from the saved sign-in. Returns whether
/// anything was restored (the caller should then save the settings). Call before the settings
/// are handed to the UI and the controller.
pub fn restore_credentials(paths: &Paths, settings: &mut Settings) -> bool {
    let has = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
    if has(&settings.lastfm_api_key) && has(&settings.lastfm_api_secret) {
        return false;
    }
    let Some(creds) = config::read_json::<SavedSession>(&paths.lastfm_session_file()).and_then(|s| s.credentials()) else {
        return false;
    };
    log::warn!("the Last.fm API key or secret was missing from the settings; restored it from the saved sign-in");
    settings.lastfm_api_key = Some(creds.api_key);
    settings.lastfm_api_secret = Some(creds.secret);
    true
}

struct Connecting {
    attempt: u64,
    task: JoinHandle<()>,
}

/// The note shown in Settings. Transient notes (a temporary send problem) go away once sending
/// works again.
struct Note {
    text: String,
    transient: bool,
}

#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
    until: Option<Instant>,
    /// The wait is for being offline (dropped early when the network comes back).
    network: bool,
}

impl Backoff {
    fn waiting(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    /// Note a failure; returns how long to wait.
    fn fail(&mut self, now: Instant, slow: bool) -> Duration {
        let mut wait = backoff_delay(self.failures, rand::random_range(0.8..1.2));
        self.failures = self.failures.saturating_add(1);
        if slow {
            wait = wait.max(SLOW_BACKOFF);
        }
        self.until = Some(now.checked_add(wait).unwrap_or(now));
        self.network = !slow;
        wait
    }

    fn hold(&mut self, now: Instant, wait: Duration) {
        self.until = Some(now.checked_add(wait).unwrap_or(now));
        self.network = false;
    }

    /// The network is back: drop a wait that was only for being offline.
    fn network_up(&mut self) -> bool {
        let drop = self.network && self.until.is_some();
        if drop {
            self.until = None;
        }
        drop
    }
}

/// The wait after `failures` earlier failures in a row, scaled by `jitter` (0.8..=1.2).
fn backoff_delay(failures: u32, jitter: f64) -> Duration {
    let index = usize::try_from(failures).unwrap_or(usize::MAX).min(BACKOFF_SECS.len() - 1);
    let jitter = if jitter.is_finite() { jitter.clamp(0.8, 1.2) } else { 1.0 };
    Duration::from_secs_f64(BACKOFF_SECS[index] as f64 * jitter)
}

pub struct Scrobbler {
    session_file: PathBuf,
    queue_file: PathBuf,
    client: Option<api::Client>,
    status_sink: StatusSink,
    reports: ReportSink,

    creds: Option<Credentials>,
    session: Option<Session>,
    enabled: bool,
    /// Bumped whenever the credentials or the session change; results of calls made with an
    /// older one don't change the connection.
    generation: u64,

    tracker: Tracker,
    queue: ScrobbleQueue,
    /// A queue change could not be written yet.
    queue_dirty: bool,

    connecting: Option<Connecting>,
    next_attempt: u64,
    /// When the batch in flight was sent.
    sending: Option<Instant>,
    backoff: Backoff,
    next_batch_at: Option<Instant>,
    /// Last.fm refused the API account with this error code (4, 10, 13 or 26: wrong key or
    /// secret, suspended): nothing is sent until the user connects again. Saved in the session
    /// file, so a later start doesn't announce it again.
    halted: Option<u32>,
    /// Stopped since an earlier run: one send may still go out, to see whether Last.fm accepts
    /// Truffle again (it only had a hiccup).
    probe: bool,
    /// The send in flight is that check.
    probing: bool,
    /// Send this many items one at a time (to find the one Last.fm rejects in a batch).
    isolate: usize,

    note: Option<Note>,
    /// A message for outside Settings (scrobbling stopped, a connection attempt ended); see
    /// [`take_alert`](Scrobbler::take_alert).
    alert: Option<Alert>,
    last_sent: Option<LastfmStatus>,
    finished: bool,
    /// The saved sign-in lacks the key and secret copy: write it once at start.
    backfill_session: bool,
}

impl Scrobbler {
    /// Load the saved session and unsent scrobbles. Nothing is sent before [`start`](Self::start).
    pub fn new(paths: &Paths, settings: &Settings, status_sink: StatusSink, reports: ReportSink) -> Self {
        let client = match api::Client::new() {
            Ok(client) => Some(client),
            Err(e) => {
                log::error!("could not set up the Last.fm connection: {e}");
                None
            }
        };
        Self::with_client(paths, settings, client, status_sink, reports)
    }

    fn with_client(
        paths: &Paths,
        settings: &Settings,
        client: Option<api::Client>,
        status_sink: StatusSink,
        reports: ReportSink,
    ) -> Self {
        let session_file = paths.lastfm_session_file();
        let saved = config::read_json::<SavedSession>(&session_file).filter(|s| !s.key.is_empty());
        let creds = credentials(settings.lastfm_api_key.as_deref(), settings.lastfm_api_secret.as_deref())
            .or_else(|| saved.as_ref().and_then(SavedSession::credentials));
        // Sign-ins saved before the key and secret were kept with them: add them now.
        let backfill = saved.as_ref().is_some_and(|s| s.credentials().is_none()) && creds.is_some();
        let halted = saved.as_ref().and_then(|s| s.refused);
        let session = saved.map(|s| Session { username: s.username, key: s.key });
        if session.is_some() && creds.is_none() {
            log::info!("a Last.fm sign-in is saved, but no API key and secret: not scrobbling");
        }
        let queue_file = paths.lastfm_queue_file();
        let (mut queue, note) = ScrobbleQueue::load(&queue_file);
        // Plays queued by an older Truffle don't name their account: the saved sign-in's.
        let adopted = session.as_ref().map_or(0, |s| queue.adopt_unknown(&s.username));
        Self {
            session_file,
            queue_file,
            client,
            status_sink,
            reports,
            creds,
            session,
            enabled: settings.lastfm_enabled,
            generation: 0,
            tracker: Tracker::default(),
            queue,
            queue_dirty: adopted > 0,
            connecting: None,
            next_attempt: 0,
            sending: None,
            backoff: Backoff::default(),
            next_batch_at: None,
            halted,
            probe: halted.is_some(),
            probing: false,
            isolate: 0,
            note: note.map(|text| Note { text, transient: false }),
            alert: None,
            last_sent: None,
            finished: false,
            backfill_session: backfill,
        }
    }

    /// Announce the state and send what is waiting. Call once the controller loop runs.
    pub fn start(&mut self, now: Instant) {
        if std::mem::take(&mut self.backfill_session)
            && let Err(e) = self.save_session()
        {
            log::warn!("could not update the saved Last.fm sign-in: {e:#}");
        }
        if let Some(session) = self.session.as_ref().filter(|_| self.creds.is_some()) {
            log::info!(
                "Last.fm: connected as {}, scrobbling {}, {} unsent",
                session.username,
                if self.enabled { "on" } else { "paused" },
                self.queue.len()
            );
            if let Some(code) = self.halted {
                log::info!("Last.fm refused Truffle's API account earlier (error {code}); checking once whether it still does");
            }
        }
        self.prune();
        self.emit();
        self.try_send(now);
    }

    fn is_connected(&self) -> bool {
        self.creds.is_some() && self.session.is_some()
    }

    /// Scrobbles and "now playing" go out only when this is true.
    fn scrobbling(&self) -> bool {
        self.enabled && self.is_connected() && !self.finished
    }

    // ------------------------------------------------------------ playback

    /// Feed the current playback state: on every change and at least once a second while
    /// playing. `play_id` changes for every new play (load) of a track.
    pub fn observe(&mut self, play_id: u64, track: Option<&TrackInfo>, status: PlayStatus, now: Instant) {
        if self.finished {
            return;
        }
        let outputs = self.tracker.observe(play_id, track, status, now, unix_now());
        self.apply(outputs, now);
    }

    /// End the current play now (sleep timer, logout): scrobbled if it is eligible, else dropped.
    /// Playing the same track again afterwards counts as a new play.
    pub fn finalise_play(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        let outputs = self.tracker.finalise(now);
        self.apply(outputs, now);
    }

    fn apply(&mut self, outputs: Vec<Output>, now: Instant) {
        if outputs.is_empty() {
            return;
        }
        for output in outputs {
            match output {
                Output::NowPlaying(track) => self.send_now_playing(track),
                Output::Scrobble(item) => {
                    if self.enqueue(item) {
                        self.try_send(now);
                    }
                }
            }
        }
        self.emit();
    }

    /// Queue an eligible play for the connected account (and write the queue). Returns whether
    /// it was queued. While stopped, plays are still queued: they go out once Last.fm accepts
    /// Truffle again.
    fn enqueue(&mut self, mut item: PendingScrobble) -> bool {
        let Some(user) = self.session.as_ref().filter(|_| self.scrobbling()).map(|s| s.username.clone()) else {
            log::debug!("not scrobbling \"{}\": scrobbling is paused or Last.fm isn't connected", item.track.track);
            return false;
        };
        log::info!("scrobble queued: {} - {}", item.track.artist, item.track.track);
        item.user = user;
        // Also keeps a queue that isn't being sent (stopped, another account's plays) in bounds.
        self.prune();
        if !self.queue.push(item) {
            return false;
        }
        self.queue_dirty = true;
        self.save_queue();
        true
    }

    fn send_now_playing(&mut self, track: ScrobbleTrack) {
        if !self.scrobbling() || self.halted.is_some() {
            return;
        }
        let (Some(client), Some(creds), Some(session)) = (self.client.clone(), self.creds.clone(), self.session.clone())
        else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let (generation, reports) = (self.generation, self.reports.clone());
        runtime.spawn(async move {
            match client.now_playing(&creds, &session.key, &track).await {
                Ok(()) => log::debug!("now playing on Last.fm: {} - {}", track.artist, track.track),
                Err(error) => reports(Report(ReportKind::NowPlaying { generation, error })),
            }
        });
    }

    // ------------------------------------------------------------ sending

    /// Send the oldest scrobbles waiting for the connected account if nothing stops it. One batch
    /// at a time.
    fn try_send(&mut self, now: Instant) {
        if let Some(sent) = self.sending {
            if now.saturating_duration_since(sent) < SEND_LOST_AFTER {
                return;
            }
            log::warn!("a scrobble send never reported back; trying again");
            self.sending = None;
            if std::mem::take(&mut self.probing) {
                self.probe = true;
            }
        }
        if !self.scrobbling() || (self.halted.is_some() && !self.probe) || self.queue.is_empty() {
            return;
        }
        if self.backoff.waiting(now) || self.next_batch_at.is_some_and(|at| now < at) {
            return;
        }
        self.prune();
        let (Some(client), Some(creds), Some(session)) = (self.client.clone(), self.creds.clone(), self.session.clone())
        else {
            return;
        };
        let batch = self.queue.oldest_for(&session.username, if self.isolate > 0 { 1 } else { api::MAX_BATCH });
        if batch.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        if self.halted.is_some() {
            log::info!("checking whether Last.fm accepts Truffle's API account again");
            self.probe = false;
            self.probing = true;
        }
        log::debug!("sending {} scrobble(s) to Last.fm", batch.len());
        self.sending = Some(now);
        self.next_batch_at = None;
        let (generation, reports) = (self.generation, self.reports.clone());
        runtime.spawn(async move {
            let result = client.scrobble(&creds, &session.key, &batch).await;
            reports(Report(ReportKind::Scrobbled { generation, batch, result }));
        });
    }

    fn on_scrobbled(&mut self, generation: u64, batch: Vec<PendingScrobble>, result: Result<Vec<ItemResult>, LfmError>, now: Instant) {
        self.sending = None;
        // The check whether Last.fm accepts Truffle again (see `probe`), for this connection.
        let probed = std::mem::take(&mut self.probing) && self.halted.is_some() && generation == self.generation;
        if probed {
            match &result {
                Ok(_) => self.resume(),
                // Refused again: stays stopped, and isn't announced again.
                Err(e) if matches!(api::classify(e), ErrorClass::Config) => {}
                // Learned nothing (offline, ...): check again with the next send.
                Err(_) => self.probe = true,
            }
        }
        match result {
            Ok(results) => {
                self.backoff.reset();
                let mut accepted = 0usize;
                let mut limited = false;
                for (item, result) in batch.iter().zip(&results) {
                    match result.code {
                        0 => {
                            accepted += 1;
                            self.queue.remove(item);
                        }
                        // Daily limit: keep it for later.
                        5 => limited = true,
                        code => {
                            log::warn!(
                                "Last.fm ignored the scrobble of \"{} - {}\": {}{}",
                                item.track.artist,
                                item.track.track,
                                ignored_reason(code),
                                if result.message.is_empty() { String::new() } else { format!(" ({})", result.message) }
                            );
                            self.queue.remove(item);
                        }
                    }
                }
                log::info!("Last.fm accepted {accepted} of {} scrobble(s)", batch.len());
                self.isolate = self.isolate.saturating_sub(batch.len());
                if limited {
                    self.backoff.hold(now, DAILY_LIMIT_WAIT);
                    self.set_note(
                        "Last.fm's daily scrobble limit has been reached. The remaining scrobbles are kept and sent later.",
                        true,
                    );
                } else {
                    if self.note.as_ref().is_some_and(|n| n.transient) {
                        self.note = None;
                    }
                    if probed {
                        self.set_note("Last.fm accepts Truffle's API account again, so scrobbling has resumed.", false);
                    }
                    self.next_batch_at = now.checked_add(BATCH_GAP);
                }
                self.queue_dirty = true;
                self.save_queue();
            }
            Err(e) => {
                log::warn!("sending {} scrobble(s) to Last.fm failed: {e}", batch.len());
                match api::classify(&e) {
                    ErrorClass::Retry => {
                        let wait = self.backoff.fail(now, false);
                        log::info!("trying Last.fm again in {} s", wait.as_secs());
                        self.set_note("Last.fm can't be reached right now. Scrobbles are saved and sent when it's back.", true);
                    }
                    ErrorClass::RetrySlow => {
                        let wait = self.backoff.fail(now, true);
                        log::info!("Last.fm rate limit; trying again in {} s", wait.as_secs());
                        self.set_note(
                            "Last.fm asked Truffle to slow down. Scrobbles are saved and sent again in a few minutes.",
                            true,
                        );
                    }
                    ErrorClass::Reauth if generation == self.generation => self.session_rejected(),
                    ErrorClass::Config if generation == self.generation => self.halt(&e),
                    // Sent with a connection that has since been replaced: try again with the new one.
                    ErrorClass::Reauth | ErrorClass::Config => self.next_batch_at = now.checked_add(BATCH_GAP),
                    ErrorClass::Drop => self.on_rejected(&batch, &e, now),
                }
            }
        }
        self.emit();
    }

    /// Last.fm called the request malformed. In a batch, find the culprit by sending the items
    /// one at a time; a single item is dropped (after a few tries for "operation failed").
    fn on_rejected(&mut self, batch: &[PendingScrobble], e: &LfmError, now: Instant) {
        if batch.len() > 1 {
            log::warn!("Last.fm rejected a batch of {}; sending them one at a time", batch.len());
            self.isolate = batch.len();
            self.next_batch_at = now.checked_add(BATCH_GAP);
            return;
        }
        let Some(item) = batch.first() else { return };
        let retry = e.code() == Some(8)
            && self.queue.get_mut(item).is_some_and(|queued| {
                queued.attempts = queued.attempts.saturating_add(1);
                queued.attempts < MAX_ATTEMPTS
            });
        if retry {
            self.backoff.fail(now, false);
        } else {
            log::warn!("Last.fm rejected the scrobble of \"{} - {}\" ({e}); dropping it", item.track.artist, item.track.track);
            self.queue.remove(item);
            self.isolate = self.isolate.saturating_sub(1);
            self.next_batch_at = now.checked_add(BATCH_GAP);
        }
        self.queue_dirty = true;
        self.save_queue();
    }

    /// Drop scrobbles too old for Last.fm, and say so.
    fn prune(&mut self) {
        let dropped = self.queue.prune(unix_now());
        if dropped == 0 {
            return;
        }
        log::warn!("dropped {dropped} unsent scrobble(s) older than two weeks");
        self.queue_dirty = true;
        self.save_queue();
        let verb = if dropped == 1 { "was" } else { "were" };
        self.set_note(
            format!("{} older than two weeks {verb} dropped, because Last.fm no longer accepts them.", count_scrobbles(dropped)),
            false,
        );
    }

    fn save_queue(&mut self) {
        match self.queue.save(&self.queue_file) {
            Ok(()) => self.queue_dirty = false,
            Err(e) => log::warn!("could not save the unsent scrobbles: {e:#}"),
        }
        // A queue file that couldn't be read at start was dealt with before this save.
        if let Some(note) = self.queue.take_note() {
            self.set_note(note, false);
        }
    }

    /// Error 9: the session key is gone (access removed in the Last.fm settings).
    fn session_rejected(&mut self) {
        log::warn!("Last.fm no longer accepts the saved session; forgetting it");
        if self.session.is_some() {
            self.alert = Some(Alert::Problem(
                "Last.fm scrobbling stopped: Last.fm no longer accepts Truffle's sign-in. Open Settings → Last.fm \
                 to connect again. Unsent plays are kept."
                    .into(),
            ));
        }
        self.session = None;
        self.generation += 1;
        self.halted = None;
        self.probe = false;
        if let Err(e) = remove_file(&self.session_file) {
            log::warn!("could not delete the Last.fm sign-in: {e}");
        }
        self.set_note(
            "Last.fm no longer accepts Truffle's sign-in, for example because access was removed in your Last.fm \
             settings. Click Connect to sign in again. Unsent scrobbles are kept.",
            false,
        );
    }

    /// The API key or secret was refused: stop sending until the user connects again (or
    /// disconnects). Settings shows why (`LastfmStatus::stopped`) with Connect again, Disconnect
    /// and the scrobbling switch. The refusal is saved with the session: the next start doesn't
    /// announce it again, and checks once whether Last.fm only had a hiccup.
    fn halt(&mut self, e: &LfmError) {
        let code = e.code().unwrap_or_default();
        log::warn!("Last.fm refused Truffle's API account ({e}); not sending until it is connected again");
        if self.halted != Some(code) {
            self.alert = Some(Alert::Problem(format!(
                "Last.fm scrobbling stopped: {}. Open Settings → Last.fm to connect again or disconnect. Unsent plays \
                 are kept.",
                halt_reason(code)
            )));
        }
        self.halted = Some(code);
        self.probe = false;
        // `stopped` explains it; an older note ("Connected as ...") would only confuse.
        self.note = None;
        if let Err(e) = self.save_session() {
            log::warn!("could not save that Last.fm refused the API account: {e:#}");
        }
    }

    /// Last.fm accepted a send again after refusing the API account (it only had a hiccup).
    fn resume(&mut self) {
        log::info!("Last.fm accepts Truffle's API account again; scrobbling resumes");
        self.halted = None;
        if let Err(e) = self.save_session() {
            log::warn!("could not save the Last.fm sign-in: {e:#}");
        }
    }

    /// Write the session (and a refusal, if any) to `lastfm_session.json`.
    fn save_session(&self) -> anyhow::Result<()> {
        let Some(session) = &self.session else { return Ok(()) };
        let saved = SavedSession {
            username: session.username.clone(),
            key: session.key.clone(),
            refused: self.halted,
            api_key: self.creds.as_ref().map(|c| c.api_key.clone()),
            api_secret: self.creds.as_ref().map(|c| c.secret.clone()),
        };
        config::write_json(&self.session_file, &saved)
    }

    /// A message for outside Settings, once: scrobbling stopped and needs the user (the sign-in
    /// was revoked, or the API key or secret is no longer accepted), or a connection attempt
    /// ended (Settings may have been closed meanwhile).
    pub fn take_alert(&mut self) -> Option<Alert> {
        self.alert.take()
    }

    // ------------------------------------------------------------ commands

    /// Check the key and secret, then run the browser approval. An existing connection keeps
    /// working until the new one succeeds.
    pub fn connect(&mut self, api_key: &str, api_secret: &str) {
        let (key, secret) = (api_key.trim(), api_secret.trim());
        let problem = if key.is_empty() || secret.is_empty() {
            Some("Paste both the API key and the shared secret of your Last.fm API account first.")
        } else if !api::is_valid_key(key) {
            Some(
                "That doesn't look like a Last.fm API key. Copy the 32-character API key from your API account \
                 page on Last.fm (last.fm/api/accounts).",
            )
        } else if !api::is_valid_key(secret) {
            Some(
                "That doesn't look like a Last.fm shared secret. Copy the 32-character shared secret from your API \
                 account page on Last.fm (last.fm/api/accounts).",
            )
        } else if key.eq_ignore_ascii_case(secret) {
            Some("The API key and the shared secret are the same. Copy each of them from your API account page on Last.fm.")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.set_note(problem, false);
            self.publish();
            return;
        }
        let (Some(client), Ok(runtime)) = (self.client.clone(), tokio::runtime::Handle::try_current()) else {
            log::error!("connecting to Last.fm is impossible: no HTTP client or async runtime");
            self.set_note("Truffle couldn't set up its connection to Last.fm. Restart Truffle and try again.", false);
            self.publish();
            return;
        };
        self.abort_connect();
        self.next_attempt += 1;
        let attempt = self.next_attempt;
        let creds = Credentials { api_key: key.to_owned(), secret: secret.to_owned() };
        let reports = self.reports.clone();
        let task = runtime.spawn(async move {
            let result = connect_flow(&client, creds).await;
            reports(Report(ReportKind::Connect { attempt, result }));
        });
        self.connecting = Some(Connecting { attempt, task });
        log::info!("connecting to Last.fm");
        self.set_note(
            "Your browser opens Last.fm. Allow Truffle to use your account there, then come back to Truffle.",
            false,
        );
        self.publish();
    }

    pub fn cancel_connect(&mut self) {
        if self.abort_connect() {
            log::info!("connecting to Last.fm cancelled");
            self.set_note("Connecting to Last.fm was cancelled.", false);
        }
        self.publish();
    }

    fn abort_connect(&mut self) -> bool {
        match self.connecting.take() {
            Some(connecting) => {
                connecting.task.abort();
                true
            }
            None => false,
        }
    }

    /// Forget the session; the key, the secret and unsent scrobbles stay.
    pub fn disconnect(&mut self) {
        self.abort_connect();
        let user = self.session.take().map(|s| s.username);
        self.generation += 1;
        self.halted = None;
        self.probe = false;
        self.probing = false;
        self.isolate = 0;
        self.backoff.reset();
        self.next_batch_at = None;
        let mut text = match remove_file(&self.session_file) {
            Ok(()) => "Disconnected from Last.fm.".to_owned(),
            Err(e) => {
                log::warn!("could not delete the Last.fm sign-in: {e}");
                "Disconnected from Last.fm, but Truffle couldn't delete the saved sign-in from this computer.".to_owned()
            }
        };
        log::info!("disconnected from Last.fm");
        // Only that account's plays are sent after connecting again; Settings lists any others.
        let (pending, when) = match &user {
            Some(user) => (self.queue.len() - self.queue.others(Some(user)).0, format!("when {user} is connected again")),
            None => (self.queue.len(), "after you connect again".to_owned()),
        };
        if pending > 0 {
            let verb = if pending == 1 { "is" } else { "are" };
            text.push_str(&format!(" {} {verb} kept and sent {when}.", count_scrobbles(pending).replacen(' ', " unsent ", 1)));
        }
        self.set_note(text, false);
        self.publish();
    }

    /// Send the plays recorded for another account to the connected one after all. `to` is the
    /// account the user saw; nothing happens if another one is connected by now.
    pub fn send_held(&mut self, to: &str, now: Instant) {
        let user = self.session.as_ref().filter(|s| self.is_connected() && same_user(&s.username, to));
        if let Some(user) = user.map(|s| s.username.clone()) {
            let moved = self.queue.adopt(&user);
            if moved > 0 {
                log::info!("sending {moved} scrobble(s) recorded for another Last.fm account to {user}");
                self.queue_dirty = true;
                self.save_queue();
                let verb = if moved == 1 { "is" } else { "are" };
                self.set_note(format!("{} from another account {verb} sent to {user} too.", count_scrobbles(moved)), false);
                self.try_send(now);
            }
        }
        self.publish();
    }

    /// Delete unsent plays without sending them, except those recorded for `keep_for`: the
    /// connected account the user saw (`None`: none was connected, so all of them). Nothing
    /// happens if the connection changed since, so plays about to be sent are never deleted.
    pub fn discard(&mut self, keep_for: Option<&str>) {
        let current = self.session.as_ref().filter(|_| self.is_connected()).map(|s| s.username.as_str());
        let still_valid = match (keep_for, current) {
            (Some(keep), Some(current)) => same_user(current, keep),
            (None, None) => true,
            _ => false,
        };
        if !still_valid {
            log::info!("not discarding unsent scrobbles: the Last.fm connection changed meanwhile");
            self.publish();
            return;
        }
        let dropped = self.queue.discard_except(keep_for);
        if dropped > 0 {
            log::info!("discarded {dropped} unsent scrobble(s)");
            self.queue_dirty = true;
            self.save_queue();
            let verb = if dropped == 1 { "was" } else { "were" };
            self.set_note(format!("{} {verb} deleted without being sent.", count_scrobbles(dropped)), false);
        }
        self.publish();
    }

    /// Switch scrobbling (and "now playing") on or off without disconnecting.
    pub fn set_enabled(&mut self, on: bool, now: Instant) {
        if self.enabled != on {
            self.enabled = on;
            log::info!("Last.fm scrobbling {}", if on { "on" } else { "paused" });
        }
        if on {
            self.try_send(now);
        }
        self.publish();
    }

    /// Every second: send when a wait is over.
    pub fn on_tick(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        self.try_send(now);
        self.emit();
    }

    /// The internet connection is back (Spotify reconnected): stop waiting out an offline backoff.
    pub fn network_up(&mut self, now: Instant) {
        if self.backoff.network_up() {
            self.try_send(now);
            self.emit();
        }
    }

    /// Handle a background result. Returns the key and secret to save in the settings after a
    /// successful connection.
    pub fn on_report(&mut self, report: Report, now: Instant) -> Option<NewConnection> {
        if self.finished {
            // Only the outcome of a batch that was already on its way at shutdown still counts
            // (see `is_sending`): what Last.fm took is removed, so it isn't sent again.
            if let ReportKind::Scrobbled { generation, batch, result } = report.0 {
                self.on_scrobbled(generation, batch, result, now);
            }
            return None;
        }
        match report.0 {
            ReportKind::Connect { attempt, result } => {
                if self.connecting.as_ref().is_none_or(|c| c.attempt != attempt) {
                    // Cancelled or replaced meanwhile.
                    return None;
                }
                self.connecting = None;
                match result {
                    Ok((creds, session)) => return Some(self.on_connected(creds, session, now)),
                    Err(failure) => {
                        log::warn!("connecting to Last.fm failed: {failure:?}");
                        let message = failure.user_message();
                        // Settings may have been closed while the browser was open.
                        self.alert = Some(Alert::Problem(format!("Connecting to Last.fm didn't work. {message}")));
                        self.set_note(message, false);
                        self.emit();
                    }
                }
            }
            ReportKind::Scrobbled { generation, batch, result } => self.on_scrobbled(generation, batch, result, now),
            ReportKind::NowPlaying { generation, error } => {
                log::info!("Last.fm didn't take \"now playing\": {error}");
                if generation == self.generation {
                    match api::classify(&error) {
                        ErrorClass::Reauth => self.session_rejected(),
                        ErrorClass::Config => self.halt(&error),
                        _ => {}
                    }
                }
                self.emit();
            }
        }
        None
    }

    fn on_connected(&mut self, creds: Credentials, session: Session, now: Instant) -> NewConnection {
        log::info!("connected to Last.fm as {}", session.username);
        let name = session.username.clone();
        let saved = NewConnection { api_key: creds.api_key.clone(), api_secret: creds.secret.clone() };
        self.creds = Some(creds);
        self.session = Some(session);
        self.generation += 1;
        self.enabled = true;
        self.halted = None;
        self.probe = false;
        self.isolate = 0;
        self.backoff.reset();
        self.next_batch_at = None;
        let (text, alert) = match self.save_session() {
            Ok(()) => {
                let text = format!("Connected to Last.fm as {name}.");
                (text.clone(), Alert::Info(text))
            }
            Err(e) => {
                log::warn!("could not save the Last.fm sign-in: {e:#}");
                let text = format!(
                    "Connected to Last.fm as {name}, but Truffle couldn't save the sign-in, so you'll need to connect \
                     again after restarting Truffle."
                );
                (text.clone(), Alert::Problem(text))
            }
        };
        // Settings may have been closed while the browser was open.
        self.alert = Some(alert);
        let (held, _) = self.queue.others(Some(&name));
        if held > 0 {
            log::info!("{held} unsent scrobble(s) were recorded for another Last.fm account; not sending them to {name}");
        }
        self.set_note(text, false);
        self.emit();
        // Only plays recorded for this account; Settings asks about any others.
        self.try_send(now);
        self.emit();
        saved
    }

    /// Whether a scrobble batch is on its way. After [`shutdown`](Self::shutdown) the controller
    /// waits briefly for its outcome and hands it to [`on_report`](Self::on_report), so plays
    /// Last.fm took aren't sent again at the next start.
    pub fn is_sending(&self) -> bool {
        self.sending.is_some()
    }

    /// At exit: finish the current play (an eligible one is queued, not sent), stop connecting,
    /// and make sure the queue file is up to date. Safe to call more than once. Nothing new is
    /// sent after this; only the outcome of a batch already on its way is still recorded.
    pub fn shutdown(&mut self, now: Instant) {
        if self.finished {
            return;
        }
        for output in self.tracker.finalise(now) {
            if let Output::Scrobble(item) = output {
                // Queued for the next start: a send started now could be cut off after Last.fm
                // took it but before it was removed here, and be sent twice.
                self.enqueue(item);
            }
        }
        self.finished = true;
        self.abort_connect();
        if self.queue_dirty {
            self.save_queue();
        }
    }

    // ------------------------------------------------------------ status

    fn set_note(&mut self, text: impl Into<String>, transient: bool) {
        self.note = Some(Note { text: text.into(), transient });
    }

    fn status(&self) -> LastfmStatus {
        let session = self.session.as_ref().filter(|_| self.is_connected());
        // Stopped (key or secret refused) is not "connected": Settings shows why, with Connect
        // again, Disconnect and the scrobbling switch.
        let stopped = self.halted.filter(|_| session.is_some()).map(stopped_message);
        let (held, held_for) = self.queue.others(session.map(|s| s.username.as_str()));
        LastfmStatus {
            connected: session.is_some() && stopped.is_none(),
            connecting: self.connecting.is_some(),
            username: session.map(|s| s.username.clone()),
            pending: self.queue.len(),
            enabled: self.enabled,
            message: self.note.as_ref().map(|n| n.text.clone()),
            stopped,
            held,
            held_for,
        }
    }

    /// Send the status even if it didn't change: the UI shows the effect of a command at once and
    /// takes this as the confirmation (or correction).
    fn publish(&mut self) {
        self.last_sent = None;
        self.emit();
    }

    /// Send the status if it changed since the last one sent.
    fn emit(&mut self) {
        let status = self.status();
        if self.last_sent.as_ref() != Some(&status) {
            self.last_sent = Some(status.clone());
            (self.status_sink)(status);
        }
    }
}

/// The browser approval: `auth.getToken`, open the approval page, poll `auth.getSession`.
async fn connect_flow(client: &api::Client, creds: Credentials) -> Result<(Credentials, Session), ConnectFailure> {
    let token = client.get_token(&creds).await.map_err(ConnectFailure::Api)?;
    open_browser(api::auth_url(&creds.api_key, &token)).await?;
    let started = Instant::now();
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        match client.get_session(&creds, &token).await {
            Ok(session) => return Ok((creds, session)),
            // Not approved yet.
            Err(LfmError::Api { code: 14, .. }) => {}
            Err(LfmError::Api { code: 4 | 15, .. }) => return Err(ConnectFailure::Expired),
            Err(e) if matches!(api::classify(&e), ErrorClass::Retry | ErrorClass::RetrySlow) => {
                log::debug!("still waiting for the Last.fm approval ({e})");
            }
            Err(e) => return Err(ConnectFailure::Api(e)),
        }
        if started.elapsed() >= APPROVAL_TIMEOUT {
            return Err(ConnectFailure::TimedOut);
        }
    }
}

async fn open_browser(url: String) -> Result<(), ConnectFailure> {
    match tokio::task::spawn_blocking(move || open::that_detached(url)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            log::warn!("could not open the browser: {e}");
            Err(ConnectFailure::Browser)
        }
        Err(e) => {
            log::warn!("the browser launcher failed: {e}");
            Err(ConnectFailure::Browser)
        }
    }
}

/// Plain-language text for a failed call.
fn problem_message(e: &LfmError) -> String {
    match e {
        LfmError::Api { code: 10, .. } => "Last.fm didn't accept the API key. Check that you copied the API key (not the \
            shared secret) from your API account page (last.fm/api/accounts), then click Connect again."
            .into(),
        LfmError::Api { code: 13, .. } => "Last.fm didn't accept the shared secret. Check that you copied it from your \
            API account page (last.fm/api/accounts), then click Connect again."
            .into(),
        LfmError::Api { code: 26, .. } => "Last.fm has suspended this API key. Create a new API account at \
            last.fm/api/account/create, then connect with its API key and shared secret."
            .into(),
        LfmError::Api { code: 4, .. } => {
            "Last.fm refused Truffle's sign-in details. Check the API key and shared secret, then click Connect again.".into()
        }
        LfmError::Api { code: 29, .. } => {
            "Last.fm is getting too many requests from this connection. Please wait a few minutes and try again.".into()
        }
        LfmError::Api { code: 11 | 16, .. } | LfmError::Net(_) | LfmError::Status(_) | LfmError::Parse(_) => {
            "Last.fm can't be reached right now. Check your internet connection and try again.".into()
        }
        LfmError::Api { code, message } => {
            let message = message.trim().trim_end_matches('.');
            if message.is_empty() {
                format!("Last.fm reported a problem (error {code}). Please try again.")
            } else {
                format!("Last.fm reported a problem: {message}. Please try again.")
            }
        }
    }
}

/// Short reason for [`Scrobbler::take_alert`] when the API account is refused (codes 4, 10, 13, 26).
fn halt_reason(code: u32) -> &'static str {
    match code {
        10 => "Last.fm didn't accept the API key",
        13 => "Last.fm didn't accept the shared secret",
        26 => "Last.fm has suspended the API key",
        _ => "Last.fm refused Truffle's sign-in details",
    }
}

/// What Settings says while stopped because Last.fm refused the API account with `code`.
fn stopped_message(code: u32) -> String {
    format!(
        "{} Songs you play meanwhile are kept and sent once Last.fm accepts Truffle again.",
        problem_message(&LfmError::Api { code, message: String::new() })
    )
}

/// Why Last.fm ignored a scrobble (`ignoredMessage` code), for the log.
fn ignored_reason(code: u32) -> String {
    match code {
        1 => "Last.fm filters this artist".into(),
        2 => "Last.fm filters this track".into(),
        3 => "its time is too long ago".into(),
        4 => "its time is in the future (is the computer's clock right?)".into(),
        other => format!("reason code {other}"),
    }
}

/// "1 scrobble", "3 scrobbles".
fn count_scrobbles(n: usize) -> String {
    format!("{n} scrobble{}", if n == 1 { "" } else { "s" })
}

/// Both values set (trimmed), else `None`.
fn credentials(api_key: Option<&str>, secret: Option<&str>) -> Option<Credentials> {
    let api_key = api_key.map(str::trim).filter(|k| !k.is_empty())?;
    let secret = secret.map(str::trim).filter(|s| !s.is_empty())?;
    Some(Credentials { api_key: api_key.to_owned(), secret: secret.to_owned() })
}

fn remove_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const SECRET: &str = "fedcba9876543210fedcba9876543210";

    fn temp_paths(name: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("truffle-lastfm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths { config: root.join("config"), cache: root.join("cache") };
        std::fs::create_dir_all(&paths.config).unwrap();
        paths
    }

    fn scrobbler(paths: &Paths, settings: &Settings) -> Scrobbler {
        Scrobbler::with_client(paths, settings, None, Arc::new(|_| {}), Arc::new(|_| {}))
    }

    #[test]
    fn sign_in_survives_settings_losing_the_key_and_secret() {
        let paths = temp_paths("restore");
        // A sign-in saved before the key and secret were kept with it.
        std::fs::write(paths.lastfm_session_file(), br#"{"username":"nandf","key":"sk"}"#).unwrap();
        let mut settings = Settings {
            lastfm_api_key: Some(KEY.into()),
            lastfm_api_secret: Some(SECRET.into()),
            ..Settings::default()
        };
        let mut s = scrobbler(&paths, &settings);
        assert!(s.is_connected());
        s.start(Instant::now());
        let saved: SavedSession = config::read_json(&paths.lastfm_session_file()).unwrap();
        assert_eq!(saved.api_key.as_deref(), Some(KEY), "the copy is added at start");
        assert_eq!(saved.api_secret.as_deref(), Some(SECRET));

        // Later the settings lose them: the sign-in still works and they are restored.
        settings.lastfm_api_key = None;
        settings.lastfm_api_secret = None;
        assert!(scrobbler(&paths, &settings).is_connected());
        assert!(restore_credentials(&paths, &mut settings));
        assert_eq!(settings.lastfm_api_key.as_deref(), Some(KEY));
        assert_eq!(settings.lastfm_api_secret.as_deref(), Some(SECRET));
        assert!(!restore_credentials(&paths, &mut settings), "nothing to do once present");
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn no_saved_sign_in_restores_nothing() {
        let paths = temp_paths("nothing");
        let mut settings = Settings::default();
        assert!(!restore_credentials(&paths, &mut settings));
        assert!(settings.lastfm_api_key.is_none());
        assert!(!scrobbler(&paths, &settings).is_connected());
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn backoff_grows_then_caps_with_jitter() {
        let secs: Vec<u64> = (0..9).map(|n| backoff_delay(n, 1.0).as_secs()).collect();
        assert_eq!(secs, [15, 30, 60, 120, 300, 600, 1800, 1800, 1800]);
        assert_eq!(backoff_delay(0, 0.8), Duration::from_secs(12));
        assert_eq!(backoff_delay(0, 1.2), Duration::from_secs(18));
        assert_eq!(backoff_delay(0, 5.0), Duration::from_secs(18), "jitter is clamped");
        assert_eq!(backoff_delay(0, f64::NAN), Duration::from_secs(15), "never panics");
        assert_eq!(backoff_delay(u32::MAX, 1.0), Duration::from_secs(1800));
    }

    #[test]
    fn backoff_states() {
        let now = Instant::now();
        let mut b = Backoff::default();
        assert!(!b.waiting(now));
        let wait = b.fail(now, false);
        assert!(wait >= Duration::from_secs(12) && wait <= Duration::from_secs(18));
        assert!(b.waiting(now) && !b.waiting(now + wait));
        assert!(b.network_up(), "an offline wait ends when the network is back");
        assert!(!b.waiting(now));
        // Rate limited: at least 5 minutes, and not cut short by the network.
        assert!(b.fail(now, true) >= SLOW_BACKOFF);
        assert!(!b.network_up());
        assert!(b.waiting(now + Duration::from_secs(299)));
        b.reset();
        assert!(!b.waiting(now));
        b.hold(now, DAILY_LIMIT_WAIT);
        assert!(!b.network_up() && b.waiting(now + Duration::from_secs(3599)));
    }

    #[test]
    fn messages_are_plain() {
        let api = |code: u32, message: &str| LfmError::Api { code, message: message.into() };
        assert!(problem_message(&api(10, "Invalid API key")).contains("API key"));
        assert!(problem_message(&api(13, "")).contains("shared secret"));
        assert!(problem_message(&api(26, "")).contains("suspended"));
        assert!(problem_message(&LfmError::Net("dns".into())).contains("can't be reached"));
        assert_eq!(problem_message(&api(6, "Track not found.")), "Last.fm reported a problem: Track not found. Please try again.");
        assert_eq!(problem_message(&api(99, "")), "Last.fm reported a problem (error 99). Please try again.");
        assert_eq!(count_scrobbles(1), "1 scrobble");
        assert_eq!(count_scrobbles(2), "2 scrobbles");
        assert_eq!(count_scrobbles(3).replacen(' ', " unsent ", 1), "3 unsent scrobbles");
    }

    #[test]
    fn credentials_need_both_values() {
        assert!(credentials(Some(" k "), Some("s")).is_some_and(|c| c.api_key == "k"));
        assert!(credentials(Some("k"), None).is_none());
        assert!(credentials(Some(""), Some("s")).is_none());
        assert!(credentials(None, Some("s")).is_none());
    }
}
