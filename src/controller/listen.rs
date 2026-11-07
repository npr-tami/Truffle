//! Listen along (the rules are in `crate::listen`): following a host, and sharing what plays.
//!
//! Following: the user's own queue waits aside and the host's song plays from a one-song queue.
//! Their own play, pause, skip, seek, shuffle and queue commands are refused with a short note;
//! Stop ends following and brings their music back, paused where it was.
//!
//! Sharing: after every loop pass the playing state is compared with the one shared last; news is
//! written to the description of every friend's link that isn't kicked (each at most once a
//! second, the newest wins, within a budget all links share). The links themselves (making,
//! kicking, removing) are in `links`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use super::jobs::Job;
use super::links::Making;
use super::{Controller, Listen, lock, user_error};
use crate::library::Library;
use crate::listen::links::{Fanout, RATE_PAUSE_MS, is_stop};
use crate::listen::{self, Follow, Gone, Here, HostState, Mode, Step};
use crate::model::{Command, Event, Friend, LinkState, ListenHow, ListenStatus, PlayStatus, PlaybackState, Source, TrackInfo};
use crate::queue::{PlayQueue, SavedQueue};

/// The one-song queue's source when the host's song has no album.
const LISTEN_MIX: &str = "listen-along";
/// The Friends list is read again at most this often (unless something changed).
const FRIENDS_EVERY: Duration = Duration::from_secs(10);
/// How long quitting waits for the last "stopped" to reach listeners.
const LAST_WRITE_WAIT: Duration = Duration::from_secs(1);
/// A read of the host (or of whose a link is) that takes longer than this counts as failed (the
/// next one can go).
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// A song lookup, a write, making a link or the Friends list taking longer than this counts as
/// failed, so one stuck request can't hold up following, sharing or the list for the rest of the
/// session.
pub(super) const SLOW_TIMEOUT: Duration = Duration::from_secs(30);

/// `work`, or an error once it took longer than `limit`.
pub(super) async fn in_time<T>(limit: Duration, work: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(limit, work).await.unwrap_or_else(|_| Err(anyhow!("no answer in time")))
}

/// Results of listen-along work done in the background.
pub(super) enum Done {
    /// A read of the host followed (`None`: no state in it).
    Polled { id: u64, result: Result<Option<HostState>> },
    /// The host's song, looked up.
    Found { id: u64, result: Result<TrackInfo> },
    /// A write to a link went through, or not.
    Written { channel: String, text: String, result: Result<()> },
    /// A link's playlist was made (its uri), for `owner` (`left`: with an error, the playlist made
    /// before it went wrong).
    LinkMade { id: u32, owner: String, result: Result<String>, left: Option<String> },
    /// Whose links are (their owner's username), as read.
    LinksChecked(Vec<(String, Result<String>)>),
    /// A removed link's playlist left the library, or not.
    LinkDeleted { uri: String, result: Result<()> },
    /// A pasted link was read.
    Joined { uri: String, result: Result<Joined> },
    /// The Friends list (`None`: it took too long).
    Friends(Option<Vec<Friend>>),
    Peek { username: String, friend: Option<Friend> },
}

/// A listen-along channel read from a pasted link.
pub(super) struct Joined {
    owner: String,
    name: String,
    state: Option<HostState>,
}

/// The user's own music while they listen along.
struct Aside {
    queue: Option<PlayQueue>,
    position_ms: u32,
    ended: bool,
}

pub(super) struct Following {
    status: ListenStatus,
    follow: Follow,
    aside: Aside,
    /// A read is on its way (its id), and when the next one is due.
    poll: Option<u64>,
    next_poll: Instant,
    /// A song lookup is on its way (its id).
    lookup: Option<u64>,
    since: Instant,
}

/// Listen-along state of the controller (for one session: logging out drops it; the links
/// themselves are the controller's `links`).
#[derive(Default)]
pub(super) struct Along {
    following: Option<Following>,
    /// The writes to every link.
    pub(super) fanout: Fanout,
    /// The state shared last.
    pub(super) shared: Option<HostState>,
    /// Links being made: new ones, and broken ones made anew.
    pub(super) making: Vec<Making>,
    /// The links made in this session: never taken as gone (Spotify may not know them everywhere
    /// yet), and "Making…" until their first words land.
    pub(super) made: HashSet<String>,
    /// Links whose owner is being read.
    pub(super) checking: HashSet<String>,
    /// Removed links' playlists being taken out of the library, each with how many reconnects
    /// were done when it was asked for.
    pub(super) deleting: HashMap<String, u64>,
    /// Reconnects done in this session.
    pub(super) reconnects: u64,
    friends_at: Option<Instant>,
    friends_busy: bool,
}

impl Along {
    pub(super) fn following(&self) -> bool {
        self.following.is_some()
    }

    /// The user's own queue and where it was, while following.
    pub(super) fn aside(&self) -> Option<(Option<&PlayQueue>, u32)> {
        self.following.as_ref().map(|f| (f.aside.queue.as_ref(), f.aside.position_ms))
    }

    /// A queue restored in the background while following goes aside, if nothing is there yet.
    pub(super) fn put_aside(&mut self, queue: PlayQueue, position_ms: u32) {
        if let Some(f) = self.following.as_mut()
            && f.aside.queue.is_none()
        {
            f.aside = Aside { queue: Some(queue), position_ms, ended: false };
        }
    }
}

/// Milliseconds since 1970 (listen along compares clocks across computers).
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// What plays here, for the follow rules. Until a song of the host's is loaded (`hosts` false),
/// the user's own song counts as nothing, so the host's is loaded even if it is the same.
fn here(pb: &PlaybackState, hosts: bool) -> Here<'_> {
    Here {
        track: pb.track.as_ref().filter(|_| hosts).map(|t| t.uri.as_str()),
        position_ms: pb.current_position_ms(),
        duration_ms: pb.track.as_ref().map_or(0, |t| t.duration_ms),
        playing: pb.status == PlayStatus::Playing,
        loading: pb.status == PlayStatus::Loading,
    }
}

async fn read_host(library: &Library, how: &ListenHow) -> Result<Option<HostState>> {
    let read = async {
        match how {
            ListenHow::Channel(uri) => Ok(listen::parse(&library.channel(uri).await?.description)),
            ListenHow::Friend(username) => Ok(library.presence(username).await?.map(|a| a.state())),
        }
    };
    in_time(READ_TIMEOUT, read).await
}

impl Controller {
    pub(super) fn listening(&self) -> bool {
        self.along.following()
    }

    fn send_listening(&self) {
        self.ui.send(Event::Listening(self.along.following.as_ref().map(|f| f.status.clone())));
    }

    /// Commands that would fight the host are refused while following, with a short note.
    pub(super) fn refused_while_listening(&self, command: &Command) -> bool {
        let Some(f) = &self.along.following else { return false };
        let refused = matches!(
            command,
            Command::PlayPause
                | Command::Next
                | Command::Previous
                | Command::SeekMs(_)
                | Command::PlaySource { .. }
                | Command::NewRound { .. }
                | Command::Queue { .. }
                | Command::Unqueue(_)
                | Command::SetShuffle(_)
                | Command::Reshuffle
        );
        if refused {
            self.ui.send(Event::Status(format!("Listening along with {}. Press Stop first.", f.status.name)));
        }
        refused
    }

    // ------------------------------------------------------------ following

    pub(super) fn cmd_listen_along(&mut self, how: ListenHow, name: String) {
        if self.library.is_none() {
            self.ui.send(Event::Error("Log in to Spotify first.".into()));
            return;
        }
        self.follow(how, name);
    }

    fn follow(&mut self, how: ListenHow, name: String) {
        if self.along.following.as_ref().is_some_and(|f| f.status.how == how) {
            return;
        }
        // The song heard so far counts (the host's, or the user's own).
        self.record_if_heard();
        let aside = match self.along.following.take() {
            // Another host: the user's own music stays aside.
            Some(f) => f.aside,
            None => {
                self.pause_playback();
                Aside { queue: self.queue.take(), position_ms: self.pb.current_position_ms(), ended: self.ended }
            }
        };
        // Never the link itself: it is the key to listening.
        let by = match &how {
            ListenHow::Channel(_) => "a link",
            ListenHow::Friend(_) => "friend activity",
        };
        log::info!("listen along: following {name} (by {by})");
        let now = Instant::now();
        let status = ListenStatus { name, how };
        self.along.following =
            Some(Following { status, follow: Follow::new(now_ms()), aside, poll: None, next_poll: now, lookup: None, since: now });
        self.sync_queue_info();
        self.mark_playback();
        self.send_listening();
        self.poll_host();
    }

    /// Read the host when it is due (from the tick and when following starts).
    pub(super) fn poll_host(&mut self) {
        if self.reconnect.is_some() || self.along.following.as_ref().is_none_or(|f| f.poll.is_some() || Instant::now() < f.next_poll) {
            return;
        }
        let Some(library) = self.library.clone() else { return };
        let id = self.new_id();
        let Some(f) = self.along.following.as_mut() else { return };
        f.poll = Some(id);
        let how = f.status.how.clone();
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = read_host(&library, &how).await;
            let _ = tx.send(Job::Listen { epoch, done: Done::Polled { id, result } });
        });
    }

    pub(super) fn on_listen_done(&mut self, done: Done) {
        match done {
            Done::Polled { id, result } => self.on_polled(id, result),
            Done::Found { id, result } => self.on_found(id, result),
            Done::Written { channel, text, result } => self.on_written_at(channel, text, result, now_ms()),
            Done::LinkMade { id, result, left, .. } => self.on_link_made(id, result, left),
            Done::LinksChecked(results) => self.on_links_checked(results),
            Done::LinkDeleted { uri, result } => self.on_link_deleted(&uri, result),
            Done::Joined { uri, result } => self.on_joined(uri, result),
            Done::Friends(list) => {
                self.along.friends_busy = false;
                if let Some(list) = list {
                    self.ui.send(Event::Friends(Arc::new(list)));
                }
            }
            Done::Peek { username, friend } => self.ui.send(Event::FriendPeek { username, friend }),
        }
    }

    /// Listen-along work done for a session that ended meanwhile: only a link's playlist made
    /// matters (nobody has it, so it must go), and a removed link's that left the library (it
    /// owes nothing more).
    pub(super) fn on_stale_listen_done(&mut self, done: Done) {
        match done {
            Done::LinkMade { owner, result: Ok(uri), .. } | Done::LinkMade { owner, left: Some(uri), .. } => self.link_orphaned(uri, owner),
            Done::LinkDeleted { uri, result: Ok(()) } => self.link_deleted(&uri),
            _ => {}
        }
    }

    fn on_polled(&mut self, id: u64, result: Result<Option<HostState>>) {
        let now = now_ms();
        let here = here(&self.pb, self.queue.is_some());
        let Some(f) = self.along.following.as_mut().filter(|f| f.poll == Some(id)) else { return };
        f.poll = None;
        let every = match f.status.how {
            ListenHow::Friend(_) => listen::FRIEND_POLL_MS,
            ListenHow::Channel(_) => listen::CHANNEL_POLL_MS,
        };
        f.next_poll = Instant::now() + Duration::from_millis(every);
        let step = match result {
            Ok(state) => f.follow.on_state(state, &here, now),
            Err(e) => {
                // The words only: the error's causes may name the link.
                log::info!("listen along: couldn't read {}: {e}", f.status.name);
                f.follow.on_failure()
            }
        };
        self.do_step(step);
    }

    fn do_step(&mut self, step: Step) {
        if step != Step::Wait {
            log::debug!("listen along: {step:?}");
        }
        match step {
            Step::Wait => {}
            Step::Load(track) => self.look_up_host_song(track),
            Step::Seek(ms) => self.cmd_seek(ms),
            Step::Pause(ms) => {
                self.pause_playback();
                if self.pb.current_position_ms().abs_diff(ms) > listen::SEEK_OFF_MS {
                    self.cmd_seek(ms);
                }
            }
            Step::Resume(ms) => self.resume_at(ms),
            Step::End(gone) => {
                let name = self.along.following.as_ref().map(|f| f.status.name.clone()).unwrap_or_default();
                log::info!("listen along: {name} is gone ({gone:?})");
                let note = match gone {
                    Gone::Stopped | Gone::Silent | Gone::Unreachable => format!("{name} stopped playing. Back to your music."),
                };
                self.end_listening(Some(note));
            }
        }
    }

    fn resume_at(&mut self, ms: u32) {
        if self.pb.status == PlayStatus::Stopped || self.pending_reload || self.engine.is_none() {
            // A song that had ended here is heard again: a new play.
            let over = self.pb.track.as_ref().is_some_and(|t| t.duration_ms > 0 && self.pb.position_ms >= t.duration_ms);
            self.load_current(true, ms, over);
            return;
        }
        if self.pb.current_position_ms().abs_diff(ms) > listen::SEEK_OFF_MS {
            self.cmd_seek(ms);
        }
        if self.pb.status == PlayStatus::Paused {
            self.cmd_play_pause();
        }
    }

    fn look_up_host_song(&mut self, track: String) {
        let Some(library) = self.library.clone() else { return };
        let id = self.new_id();
        let Some(f) = self.along.following.as_mut() else { return };
        f.lookup = Some(id);
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = in_time(SLOW_TIMEOUT, library.track(&track)).await;
            let _ = tx.send(Job::Listen { epoch, done: Done::Found { id, result } });
        });
    }

    fn on_found(&mut self, id: u64, result: Result<TrackInfo>) {
        let now = now_ms();
        let Some(f) = self.along.following.as_mut().filter(|f| f.lookup == Some(id)) else { return };
        f.lookup = None;
        let track = match result {
            Ok(t) => t,
            Err(e) => {
                log::warn!("listen along: couldn't look up the host's song: {e:#}");
                f.follow.load_failed();
                return;
            }
        };
        // The host moved on meanwhile: their next read loads the new song.
        let Some((position_ms, play)) = f.follow.target(&track.uri, now) else {
            f.follow.load_failed();
            return;
        };
        let name = f.status.name.clone();
        if !track.playable {
            f.follow.on_ended();
            self.ui.send(Event::Status(format!("This song can't play here. Waiting for {name}'s next one.")));
            return;
        }
        // Their song is already over (friend activity shows the last song for a while).
        let over = track.duration_ms > 0 && position_ms.saturating_add(1_000) >= track.duration_ms;
        if over {
            f.follow.on_ended();
        }
        log::info!("listen along: playing \"{}\" at {} s", track.name, position_ms / 1000);
        self.play_host_song(track, if over { 0 } else { position_ms }, play && !over);
        if over {
            self.ui.send(Event::Status(format!("Waiting for {name}'s next song.")));
        }
    }

    /// The host's song, from a one-song queue (the user's own waits aside).
    fn play_host_song(&mut self, track: TrackInfo, position_ms: u32, play: bool) {
        self.record_if_heard();
        let source = if track.album_uri.is_empty() { Source::Mix(LISTEN_MIX.to_owned()) } else { Source::Album(track.album_uri.clone()) };
        let rules = self.rules();
        self.queue = Some(PlayQueue::new(source, Arc::new(vec![track]), Some(0), false, &rules, &self.history));
        self.failures = 0;
        self.load_current(play, position_ms, true);
    }

    /// The host's song started playing here.
    pub(super) fn listen_playing(&mut self) {
        let now = now_ms();
        let here = here(&self.pb, self.queue.is_some());
        let Some(f) = self.along.following.as_mut() else { return };
        let step = f.follow.on_playing(&here, now);
        self.do_step(step);
    }

    /// The host's song ended here, or couldn't play: wait for their next one.
    pub(super) fn listen_ended(&mut self) {
        if let Some(f) = self.along.following.as_mut() {
            f.follow.on_ended();
        }
    }

    /// Stop following; the user's own music comes back, paused where it was.
    pub(super) fn end_listening(&mut self, note: Option<String>) {
        if !self.listening() {
            return;
        }
        // The host's song counts if it was heard long enough.
        self.record_if_heard();
        let Some(f) = self.along.following.take() else { return };
        log::info!("listen along: stopped following {}", f.status.name);
        self.pause_playback();
        let Aside { queue, position_ms, ended } = f.aside;
        self.queue = queue;
        let current = self.queue.as_ref().and_then(|q| q.current()).cloned();
        match current {
            Some(track) if ended => {
                self.listen = Listen::default();
                self.pb.track = Some(track);
                self.end_of_queue();
            }
            Some(_) => self.load_current(false, position_ms, true),
            None => {
                self.queue = None;
                self.stop_playback();
                self.listen = Listen::default();
                self.pb.track = None;
                self.sync_queue_info();
            }
        }
        self.mark_playback();
        self.send_listening();
        if let Some(note) = note {
            self.ui.send(Event::Status(note));
        }
    }

    /// Logging out: following ends (the queue aside was saved with the rest).
    pub(super) fn reset_listening(&mut self) {
        let was = self.listening();
        self.along = Along::default();
        if was {
            // The host's song, not the user's.
            self.queue = None;
            self.send_listening();
        }
    }

    // ------------------------------------------------------------ sharing

    pub(super) fn send_share(&self) {
        let (on, link) = {
            let s = lock(&self.settings);
            (s.listen_share, s.listen_channel.as_deref().and_then(listen::channel_link))
        };
        self.ui.send(Event::ListenShare { on, link });
    }

    pub(super) fn cmd_listen_share(&mut self, on: bool) {
        let was = lock(&self.settings).listen_share;
        self.update_settings(|s| s.listen_share = on);
        // Switched on: the next pass writes what plays to every link that can hear.
        self.along.shared = None;
        let me = self.me();
        if was && !on {
            // Said once to every link that could hear; nothing more is written after it. Kicked
            // links already say stopped.
            let now = now_ms();
            let text = listen::off_description(now);
            for uri in self.links.live_uris(&me) {
                self.along.fanout.push(&uri, text.clone(), now);
            }
        }
        self.send_share();
        if was != on {
            let any = self.links.count_mine(&me) + self.along.making.len() > 0;
            let text = match (on, any) {
                (true, true) => "Friends with a link can listen along now.",
                (true, false) => "Now make a link for a friend.",
                (false, _) => "Stopped sharing what you play.",
            };
            self.ui.send(Event::Status(text.into()));
        }
    }

    /// What plays here as a host state (`None` while a song is loading, or at its very end: the
    /// next song is shared once it plays).
    pub(super) fn host_state(&self, now: i64) -> Option<HostState> {
        let Some(track) = self.pb.track.as_ref() else { return Some(HostState::stopped(now)) };
        let mode = match self.pb.status {
            PlayStatus::Loading => return None,
            PlayStatus::Playing => Mode::Play,
            PlayStatus::Paused => Mode::Pause,
            PlayStatus::Stopped => return Some(HostState::stopped(now)),
        };
        let position_ms = self.pb.current_position_ms();
        if mode == Mode::Play && track.duration_ms > 0 && position_ms >= track.duration_ms {
            return None;
        }
        Some(HostState { track: track.uri.clone(), position_ms, sent_at_ms: now, mode })
    }

    /// After every loop pass: share what plays with every link that can hear if it is news (and
    /// sharing is on), make the links asked for, and send the writes that are due.
    pub(super) fn share_playback(&mut self) {
        // After quitting, the "stopped" already sent is the last word.
        if self.library.is_none() || self.finished {
            return;
        }
        self.queue_news(now_ms());
        self.start_makes();
        self.send_writes();
    }

    /// What plays, for every link that can hear, if it is news and sharing is on.
    pub(super) fn queue_news(&mut self, now: i64) {
        if !lock(&self.settings).listen_share {
            return;
        }
        let Some(state) = self.host_state(now) else { return };
        if !listen::worth_sharing(self.along.shared.as_ref(), &state, now) {
            return;
        }
        let text = listen::describe(&state);
        // Never a kicked link: its box keeps only the newest text, and would lose the stop.
        for uri in self.links.live_uris(&self.me()) {
            self.along.fanout.push(&uri, text.clone(), now);
        }
        self.along.shared = Some(state);
    }

    fn send_writes(&mut self) {
        if self.reconnect.is_some() {
            return;
        }
        let Some(library) = self.library.clone() else { return };
        for (channel, text) in self.along.fanout.take_due(now_ms()) {
            log::debug!("listen along: writing {}: {text}", self.link_label(&channel));
            let (epoch, tx, library) = (self.epoch, self.jobs.clone(), library.clone());
            tokio::spawn(async move {
                let result = in_time(SLOW_TIMEOUT, library.write_channel(&channel, &text)).await;
                let _ = tx.send(Job::Listen { epoch, done: Done::Written { channel, text, result } });
            });
        }
    }

    /// A write to a link finished at `now`.
    pub(super) fn on_written_at(&mut self, channel: String, text: String, result: Result<()>, now: i64) {
        let first = self.along.made.contains(&channel) && !self.along.fanout.landed(&channel);
        self.along.fanout.done(&channel, text.clone(), result.is_ok(), now);
        match result {
            Ok(()) => self.on_link_written(&channel, &text, first),
            Err(e) => {
                // The words only: the error's causes may name the playlist, which is the key to it.
                log::warn!("listen along: couldn't write {}: {e}", self.link_label(&channel));
                if crate::library::is_rate_limited(&e) {
                    log::info!("listen along: Spotify asked to slow down; link writes wait {} s", RATE_PAUSE_MS / 1000);
                    self.along.fanout.rate_limited(now);
                } else if crate::library::is_permanent(&e) {
                    self.link_refused(&channel);
                }
            }
        }
    }

    /// Quitting: every link written to in this session that could still hear is told the music
    /// stopped, and the stops still owed go out, all at once, a moment at most. A link that
    /// misses it: its listener stops by themselves ten minutes after the song; a stop still owed
    /// goes at the next login.
    pub(super) async fn share_stop(&mut self) {
        let Some(library) = self.library.clone() else { return };
        let now = now_ms();
        let stop = self.stop_words(now);
        for uri in self.links.owed_stops(&self.me()) {
            self.along.fanout.push(&uri, stop.clone(), now);
        }
        let words = self.along.fanout.last_words(now);
        if words.is_empty() {
            return;
        }
        // (A write already on its way to a link lands first: a link's writes take turns, within
        // the same moment.)
        let writes = words.into_iter().map(|(uri, text)| {
            let library = library.clone();
            async move {
                let sent = tokio::time::timeout(LAST_WRITE_WAIT, library.write_channel(&uri, &text)).await;
                (uri, text, sent)
            }
        });
        let (mut told, mut missed, mut changed) = (0, 0, false);
        for (uri, text, sent) in futures::future::join_all(writes).await {
            match sent {
                Ok(Ok(())) => {
                    told += 1;
                    // A kick's stop landed. (A removed link's stays owed: the next login says it
                    // again, then takes the playlist out of the library.)
                    if is_stop(&text)
                        && let Some(l) = self.links.by_uri_mut(&uri)
                        && l.kicked
                        && l.stop_owed
                    {
                        l.stop_owed = false;
                        changed = true;
                    }
                }
                Ok(Err(e)) => {
                    missed += 1;
                    log::info!("listen along: couldn't tell {} the music stopped: {e}", self.link_label(&uri));
                }
                Err(_) => missed += 1,
            }
        }
        log::info!("listen along: told {told} links the music stopped ({missed} missed)");
        if changed {
            self.save_links();
        }
    }

    /// At login: the stops still owed go out (and the removed links' playlists still owed out of
    /// the library), the link older Truffles write to follows this account's links, the links not
    /// checked yet (or broken) are checked, and the UI hears it.
    pub(super) fn listen_logged_in(&mut self) {
        if self.links_held {
            self.reload_links();
        }
        let now = now_ms();
        let stop = self.stop_words(now);
        for uri in self.links.owed_stops(&self.me()) {
            self.along.fanout.push(&uri, stop.clone(), now);
        }
        self.retry_deletes();
        self.mirror_links();
        self.check_links();
        self.send_links();
        self.send_share();
    }

    // ------------------------------------------------------------ friends and links

    pub(super) fn cmd_listen_join(&mut self, text: String) {
        let Some(uri) = listen::channel_uri(&text) else {
            self.ui.send(Event::Error("That isn't a listen-along link.".into()));
            return;
        };
        // (One made on another PC is caught once read: its owner is this account.)
        if self.own_link(&uri) {
            self.ui.send(Event::Error("That's your own listen-along link.".into()));
            return;
        }
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::Error("Log in to Spotify first.".into()));
            return;
        };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = async {
                let channel = library.channel(&uri).await?;
                if !listen::is_channel(&channel.description) {
                    bail!("That playlist isn't a listen-along link.");
                }
                let (name, _) = library.person(&channel.owner).await;
                let name = name.unwrap_or_else(|| channel.owner.clone());
                Ok(Joined { state: listen::parse(&channel.description), owner: channel.owner, name })
            }
            .await;
            let _ = tx.send(Job::Listen { epoch, done: Done::Joined { uri, result } });
        });
    }

    fn on_joined(&mut self, uri: String, result: Result<Joined>) {
        let joined = match result {
            Ok(j) => j,
            Err(e) => {
                self.ui.send(Event::Error(user_error("Couldn't join.", &e)));
                return;
            }
        };
        if self.profile.as_ref().is_some_and(|p| p.username.eq_ignore_ascii_case(&joined.owner)) {
            self.ui.send(Event::Error("That's your own listen-along link.".into()));
            return;
        }
        let name = joined.name;
        self.update_settings(|s| {
            s.listen_hosts.retain(|(_, u)| *u != uri);
            s.listen_hosts.insert(0, (name.clone(), uri.clone()));
        });
        if joined.state.is_some_and(|s| s.mode != Mode::Stop) {
            self.cmd_listen_along(ListenHow::Channel(uri), name);
        } else {
            self.ui.send(Event::Status(format!("{name} isn't playing right now. They're in Friends now.")));
        }
        self.cmd_friends(true);
    }

    pub(super) fn cmd_listen_forget(&mut self, uri: String) {
        if self.along.following.as_ref().is_some_and(|f| f.status.how == ListenHow::Channel(uri.clone())) {
            self.end_listening(None);
        }
        self.update_settings(|s| s.listen_hosts.retain(|(_, u)| *u != uri));
        self.cmd_friends(true);
    }

    pub(super) fn cmd_friends(&mut self, force: bool) {
        if self.along.friends_busy || (!force && self.along.friends_at.is_some_and(|at| at.elapsed() < FRIENDS_EVERY)) {
            return;
        }
        let Some(library) = self.library.clone() else { return };
        self.along.friends_busy = true;
        self.along.friends_at = Some(Instant::now());
        let hosts = lock(&self.settings).listen_hosts.clone();
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let list = tokio::time::timeout(SLOW_TIMEOUT, friends(&library, hosts)).await.ok();
            let _ = tx.send(Job::Listen { epoch, done: Done::Friends(list) });
        });
    }

    pub(super) fn cmd_friend_peek(&mut self, username: String) {
        let Some(library) = self.library.clone() else { return };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let friend = match library.presence(&username).await {
                Ok(Some(a)) => {
                    let duration = library.track(&a.track).await.ok().map(|t| t.duration_ms);
                    Some(activity_friend(a, duration, now_ms()))
                }
                Ok(None) => None,
                Err(e) => {
                    log::debug!("friend activity of {username}: {e:#}");
                    None
                }
            };
            let _ = tx.send(Job::Listen { epoch, done: Done::Peek { username, friend } });
        });
    }

    // ------------------------------------------------------------ test aid

    /// `listen share on|off`, `listen join <link>`, `listen follow <username>`, `listen channel
    /// <link>` (follow without remembering, own links too), `listen stop`, `listen friends` (the
    /// list, once read), `listen state`, and for the links (`<n>`: the place in the list as the
    /// Friends tab shows it, from 1): `listen links`, `listen newlink <name>`, `listen rename <n>
    /// <name>`, `listen kick <n>`, `listen unkick <n>`, `listen remove <n>` (no question asked),
    /// `listen relink <n>`. Each writes what listen along is doing to `truffle-selftest.txt` in
    /// `dir`.
    pub(super) fn listen_selftest(&mut self, args: &str, dir: &Path) {
        let mut words = args.split_whitespace();
        let what = words.next().unwrap_or("state");
        let rest = words.collect::<Vec<_>>().join(" ");
        // Only the first word: the rest may be a friend's name or a link.
        log::info!("self-test: listen {what}");
        let (n, name) = rest.split_once(' ').unwrap_or((rest.as_str(), ""));
        match (what, rest.as_str(), self.nth_link(n)) {
            ("newlink", name, _) => self.cmd_listen_new_link(name.to_owned()),
            ("rename", _, Some(id)) => self.cmd_listen_rename_link(id, name.to_owned()),
            ("kick", _, Some(id)) => self.cmd_listen_kick(id, true),
            ("unkick", _, Some(id)) => self.cmd_listen_kick(id, false),
            ("remove", _, Some(id)) => self.cmd_listen_remove_link(id),
            ("relink", _, Some(id)) => self.cmd_listen_remake_link(id),
            ("rename" | "kick" | "unkick" | "remove" | "relink", _, None) => log::warn!("self-test: no link at that place"),
            ("share", "on", _) => self.cmd_listen_share(true),
            ("share", "off", _) => self.cmd_listen_share(false),
            ("join", link, _) => self.cmd_listen_join(link.to_owned()),
            ("follow", user, _) if !user.is_empty() => self.cmd_listen_along(ListenHow::Friend(user.to_owned()), user.to_owned()),
            // Not refused for being one's own: this is how one account follows its own link.
            ("channel", link, _) => match listen::channel_uri(link) {
                Some(uri) => self.cmd_listen_along(ListenHow::Channel(uri), "the host".to_owned()),
                None => log::warn!("self-test: not a listen-along link"),
            },
            ("stop", _, _) => self.end_listening(Some("Stopped listening along.".into())),
            ("friends", _, _) => {
                if let Some(library) = self.library.clone() {
                    let hosts = lock(&self.settings).listen_hosts.clone();
                    let file = dir.join("truffle-selftest.txt");
                    tokio::spawn(async move {
                        let list = friends(&library, hosts).await;
                        let mut out = format!("friends: {}\n", list.len());
                        for f in &list {
                            let _ = writeln!(out, "{} | {:?} | {} - {} | live {} | started {}", f.name, f.how, f.artist, f.song, f.live, f.at_ms);
                        }
                        // Names and links: only in the file.
                        log::info!("self-test: {} friends written", list.len());
                        let _ = std::fs::write(file, out);
                    });
                }
                return;
            }
            _ => {}
        }
        let report = self.listen_report();
        // The report has the links (each the key to listening) and names: only in the file.
        log::info!("self-test: listen report written ({} lines)", report.lines().count());
        let _ = std::fs::write(dir.join("truffle-selftest.txt"), report);
    }

    /// The id of the link at place `n` (from 1) in the Friends tab's list.
    fn nth_link(&self, n: &str) -> Option<u32> {
        let n: usize = n.trim().parse().ok()?;
        self.links_view().get(n.checked_sub(1)?).map(|v| v.id)
    }

    fn listen_report(&self) -> String {
        let now = now_ms();
        let (sharing, channel, hosts) = {
            let s = lock(&self.settings);
            (s.listen_share, s.listen_channel.clone(), s.listen_hosts.clone())
        };
        let mut out = String::new();
        let view = self.links_view();
        let count = |state: LinkState| view.iter().filter(|v| v.state == state).count();
        let mirror = channel.as_deref().and_then(listen::channel_link).unwrap_or_else(|| "none".into());
        let _ = writeln!(
            out,
            "sharing: {} | links: {} ({} live, {} kicked, {} gone, {} making) | older Truffles write to: {mirror}",
            if sharing { "on" } else { "off" },
            view.len(),
            count(LinkState::Ready),
            count(LinkState::Kicked),
            count(LinkState::Gone),
            count(LinkState::Making)
        );
        let f = &self.along.fanout;
        let owed = self.links.removed.iter().filter(|r| r.stop_owed).count();
        let listed = self.links.removed.iter().filter(|r| r.delete_owed).count();
        let _ = writeln!(
            out,
            "budget {} | paused {} | removed {} ({owed} owe a stop, {listed} still to leave the library)",
            f.budget(),
            if f.paused(now) { "yes" } else { "no" },
            self.links.removed.len()
        );
        let mode = |text: &str| match listen::parse(text).map(|s| s.mode) {
            Some(Mode::Play) => "play",
            Some(Mode::Pause) => "pause",
            Some(Mode::Stop) if text.starts_with("Not sharing") => "off",
            Some(Mode::Stop) => "stop",
            None => "?",
        };
        for (n, v) in view.iter().enumerate() {
            let link = self.links.get(v.id);
            let uri = link.map(|l| l.uri.as_str());
            let state = match v.state {
                LinkState::Making => "making",
                LinkState::Ready => "live",
                LinkState::Kicked => "kicked",
                LinkState::Gone => "gone",
            };
            let owed = if link.is_some_and(|l| l.stop_owed) { "stop owed" } else { "-" };
            let slot = uri.and_then(|u| f.view(u));
            let written = match slot.and_then(|s| s.sent.zip(s.sent_ms)) {
                Some((t, at)) => format!("written: {} {} s ago", mode(t), (now - at).max(0) / 1000),
                None => "written: -".to_owned(),
            };
            let waiting = slot.and_then(|s| s.waiting).map_or_else(|| "waiting: -".to_owned(), |t| format!("waiting: {}", mode(t)));
            let writing = if slot.is_some_and(|s| s.writing) { "writing" } else { "-" };
            let address = uri.and_then(listen::channel_link).unwrap_or_default();
            let _ = writeln!(out, "#{} {} | {state} | {owed} | {written} | {waiting} | {writing} | {address}", n + 1, v.name);
        }
        let here_track = self.pb.track.as_ref().map(|t| format!("{} ({})", t.name, t.uri)).unwrap_or_else(|| "nothing".into());
        let at = self.pb.current_position_ms();
        match &self.along.following {
            Some(f) => {
                let _ = writeln!(out, "following: {} ({:?}) for {} s", f.status.name, f.status.how, f.since.elapsed().as_secs());
                match f.follow.last().zip(f.follow.position(now)) {
                    Some((s, want)) => {
                        let _ = writeln!(out, "host: {:?} {} at {:.1} s", s.mode, s.track, f64::from(want) / 1000.0);
                        let _ = writeln!(out, "here: {:?} {here_track} at {:.1} s (off by {:.1} s)", self.pb.status, f64::from(at) / 1000.0, f64::from(want.abs_diff(at)) / 1000.0);
                    }
                    None => {
                        let _ = writeln!(out, "host: not read yet | here: {:?} {here_track}", self.pb.status);
                    }
                }
            }
            None => {
                let _ = writeln!(out, "following: nobody | here: {:?} {here_track} at {:.1} s", self.pb.status, f64::from(at) / 1000.0);
            }
        }
        let names: Vec<String> = hosts.iter().map(|(n, u)| format!("{n} = {u}")).collect();
        let _ = writeln!(out, "hosts: {}", if names.is_empty() { "none".to_owned() } else { names.join(", ") });
        out
    }
}

/// A friend-activity person as a Friends row.
fn activity_friend(a: listen::Activity, duration_ms: Option<u32>, now: i64) -> Friend {
    Friend {
        live: listen::playing_now(a.started_ms, duration_ms, now),
        at_ms: a.started_ms,
        how: ListenHow::Friend(a.username),
        name: a.name,
        image: a.image,
        song: a.track_name,
        artist: a.artist,
    }
}

/// The Friends list: friend activity and the Truffle hosts joined, playing ones first.
async fn friends(library: &Library, hosts: Vec<(String, String)>) -> Vec<Friend> {
    let now = now_ms();
    let activity = library.buddylist().await.unwrap_or_else(|e| {
        log::info!("friend activity: {e:#}");
        Vec::new()
    });
    let mut channels = Vec::new();
    for (name, uri) in hosts {
        match library.channel(&uri).await {
            Ok(c) => {
                let (_, image) = library.person(&c.owner).await;
                channels.push((name, uri, image, listen::parse(&c.description)));
            }
            Err(e) => {
                // Not the link: it is the key to listening.
                log::info!("a listen-along link joined couldn't be read: {e}");
                channels.push((name, uri, None, None));
            }
        }
    }
    // The songs' names and lengths, in one lookup.
    let mut uris: Vec<String> = activity.iter().map(|a| a.track.clone()).collect();
    uris.extend(channels.iter().filter_map(|c| c.3.as_ref()).filter(|s| !s.track.is_empty()).map(|s| s.track.clone()));
    uris.sort();
    uris.dedup();
    let details: HashMap<String, TrackInfo> = if uris.is_empty() {
        HashMap::new()
    } else {
        library.tracks_by_uri(&uris).await.map(|v| v.into_iter().map(|t| (t.uri.clone(), t)).collect()).unwrap_or_default()
    };
    let mut list: Vec<Friend> = activity
        .into_iter()
        .map(|a| {
            let duration = details.get(&a.track).map(|t| t.duration_ms);
            activity_friend(a, duration, now)
        })
        .collect();
    for (name, uri, image, state) in channels {
        let state = state.filter(|s| s.mode != Mode::Stop);
        let song = state.as_ref().and_then(|s| details.get(&s.track));
        let live = state.as_ref().is_some_and(|s| {
            s.mode == Mode::Play && song.is_none_or(|t| t.duration_ms == 0 || s.position_at(now) < t.duration_ms + 30_000)
        });
        list.push(Friend {
            how: ListenHow::Channel(uri),
            name,
            image,
            song: song.map(|t| t.name.clone()).unwrap_or_default(),
            artist: song.map(TrackInfo::artist_line).unwrap_or_default(),
            live,
            at_ms: state.map_or(0, |s| s.sent_at_ms),
        });
    }
    list.sort_by_key(|f| (!f.live, std::cmp::Reverse(f.at_ms)));
    list
}

/// The queue state.json keeps: the user's own while they listen along.
pub(super) fn saved_queue(controller: &Controller) -> Option<SavedQueue> {
    let (queue, position_ms) = match controller.along.aside() {
        Some((queue, position_ms)) => (queue?, position_ms),
        None => (controller.queue.as_ref()?, controller.pb.current_position_ms()),
    };
    let mut saved = queue.save(position_ms);
    saved.username = controller.profile.as_ref().map(|p| p.username.clone()).unwrap_or_default();
    Some(saved)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::{Paths, Settings};
    use crate::controller::jobs::{Job, TracksPurpose};
    use crate::controller::{UiLink, persist};
    use crate::model::{PlaylistSummary, UserProfile};

    /// A controller with no session (no engine: loads end paused) writing into a temp folder.
    fn controller(name: &str) -> Controller {
        let dir = std::env::temp_dir().join(format!("truffle-listen-{name}-{}", std::process::id()));
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        let (events, _) = std::sync::mpsc::channel();
        let ui = UiLink { events, ctx: egui::Context::default() };
        let (jobs, _) = tokio::sync::mpsc::unbounded_channel();
        Controller::new(paths, Arc::new(Mutex::new(Settings::default())), ui, jobs)
    }

    fn done(c: Controller) {
        let dir = c.paths.config.parent().map(Path::to_path_buf);
        drop(c);
        if let Some(dir) = dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn track(id: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{id}"),
            name: id.to_owned(),
            artists: Vec::new(),
            album: String::new(),
            album_uri: format!("spotify:album:{id}"),
            cover_small: None,
            cover_large: None,
            duration_ms: 200_000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    fn mine() -> (Source, Arc<Vec<TrackInfo>>) {
        (Source::Playlist("spotify:playlist:mine".into()), Arc::new(vec![track("a"), track("b"), track("c")]))
    }

    fn host() -> ListenHow {
        ListenHow::Channel("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into())
    }

    fn playing(c: &Controller) -> (Option<&str>, u32, PlayStatus) {
        (c.pb.track.as_ref().map(|t| t.uri.as_str()), c.pb.current_position_ms(), c.pb.status)
    }

    #[test]
    fn stop_brings_the_users_music_back_paused_where_it_was() {
        let mut c = controller("stop");
        let (source, tracks) = mine();
        let rules = c.rules();
        c.queue = Some(PlayQueue::new(source, tracks, Some(1), false, &rules, &c.history));
        c.load_current(false, 42_000, true);
        c.follow(host(), "Robin".into());
        c.play_host_song(track("h"), 30_000, true);
        assert_eq!(playing(&c).0, Some("spotify:track:h"));
        c.end_listening(None);
        assert!(!c.listening());
        assert_eq!(playing(&c), (Some("spotify:track:b"), 42_000, PlayStatus::Paused));
        assert_eq!(c.queue.as_ref().map(PlayQueue::len), Some(3));
        done(c);
    }

    #[test]
    fn a_queue_restored_while_listening_along_comes_back_after() {
        let mut c = controller("restore");
        // Following started while the saved queue's songs were still loading.
        c.follow(host(), "Robin".into());
        c.play_host_song(track("h"), 0, true);
        let (source, tracks) = mine();
        let rules = c.rules();
        let saved = PlayQueue::new(source.clone(), tracks.clone(), Some(2), false, &rules, &c.history).save(77_000);
        c.play_req = 7;
        c.on_tracks(7, source, TracksPurpose::Restore { saved }, Ok(tracks));
        assert_eq!(playing(&c).0, Some("spotify:track:h"), "the host's song plays on");
        c.end_listening(None);
        assert_eq!(playing(&c), (Some("spotify:track:c"), 77_000, PlayStatus::Paused));
        done(c);
    }

    // ------------------------------------------------------------ links

    const ME: &str = "alex";

    fn link(n: u32) -> String {
        format!("spotify:playlist:{n:0>22}")
    }

    fn gone() -> anyhow::Error {
        anyhow::Error::new(librespot_core::Error::not_found("no such playlist"))
    }

    /// Another account's playlist: Spotify won't let this one write it.
    fn refused() -> anyhow::Error {
        anyhow::Error::new(librespot_core::Error::permission_denied("not yours"))
    }

    fn profile(username: &str) -> Option<UserProfile> {
        Some(UserProfile { username: username.into(), display_name: String::new(), image_url: None, premium: None })
    }

    /// A controller on `paths` (kept between "runs"), with no session, and what it sends the UI.
    fn controller_at(paths: Paths, settings: Arc<Mutex<Settings>>) -> (Controller, Receiver<Event>) {
        let (events, rx) = std::sync::mpsc::channel();
        let ui = UiLink { events, ctx: egui::Context::default() };
        let (jobs, _) = tokio::sync::mpsc::unbounded_channel();
        (Controller::new(paths, settings, ui, jobs), rx)
    }

    /// A fresh controller logged in as ME (no session: nothing is sent to Spotify), with `n`
    /// links of ME's, ids 1 to n.
    fn with_links(name: &str, n: u32) -> (Controller, Receiver<Event>) {
        let dir = std::env::temp_dir().join(format!("truffle-links-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        let (mut c, rx) = controller_at(paths, Arc::new(Mutex::new(Settings::default())));
        c.profile = Some(UserProfile { username: ME.into(), display_name: "alex".into(), image_url: None, premium: None });
        for i in 1..=n {
            let id = c.links.reserve_id();
            c.links.add(id, &format!("Friend {i}"), &link(i), ME, 0);
        }
        c.links_changed();
        (c, rx)
    }

    /// A song paused at `ms` (so the shared state doesn't move with the clock).
    fn paused_at(c: &mut Controller, ms: u32) {
        c.pb.track = Some(track("h"));
        c.pb.status = PlayStatus::Paused;
        c.pb.position_ms = ms;
    }

    /// Every write due at `now` lands; returns them.
    fn land(c: &mut Controller, now: i64) -> Vec<(String, String)> {
        let due = c.along.fanout.take_due(now);
        for (uri, text) in &due {
            c.on_written_at(uri.clone(), text.clone(), Ok(()), now);
        }
        due
    }

    fn statuses(rx: &Receiver<Event>) -> Vec<String> {
        rx.try_iter().filter_map(|e| if let Event::Status(s) | Event::Error(s) = e { Some(s) } else { None }).collect()
    }

    /// A playlist in the library list (`editable`: and the user's own).
    fn pl(uri: &str, name: &str, description: &str, editable: bool) -> PlaylistSummary {
        PlaylistSummary {
            uri: uri.into(),
            name: name.into(),
            description: description.into(),
            owner_name: String::new(),
            image_url: None,
            track_count: Some(0),
            revision: String::new(),
            editable,
            owned: editable,
            owner_id: String::new(),
        }
    }

    #[test]
    fn a_kick_writes_one_stop_then_nothing() {
        let (mut c, rx) = with_links("kick", 2);
        c.update_settings(|s| s.listen_share = true);
        let t = now_ms();
        paused_at(&mut c, 10_000);
        c.queue_news(t);
        assert_eq!(land(&mut c, t).len(), 2);
        c.cmd_listen_kick(1, true);
        assert!(statuses(&rx).contains(&"Friend 1 is paused and hears nothing now.".to_owned()));
        let due = land(&mut c, t + 1_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, link(1));
        assert!(is_stop(&due[0].1) && due[0].1.starts_with("Listen along with me on Truffle."), "an ordinary stop: {}", due[0].1);
        assert!(!c.links.get(1).unwrap().stop_owed, "it landed");
        // From then on, news reaches the other link only, and quitting has nothing for link 1.
        for i in 0..5 {
            let at = t + 2_000 + i64::from(i) * 2_000;
            paused_at(&mut c, 60_000 + i * 30_000);
            c.queue_news(at);
            assert_eq!(land(&mut c, at).iter().map(|(u, _)| u.clone()).collect::<Vec<_>>(), vec![link(2)]);
        }
        assert!(c.along.fanout.last_words(t + 30_000).iter().all(|(u, _)| *u != link(1)));
        done(c);
    }

    #[test]
    fn letting_back_in_writes_the_current_state_at_once() {
        let (mut c, rx) = with_links("unkick", 1);
        c.update_settings(|s| s.listen_share = true);
        let t = now_ms();
        paused_at(&mut c, 10_000);
        c.queue_news(t);
        land(&mut c, t);
        c.cmd_listen_kick(1, true);
        land(&mut c, t + 1_000);
        // Moved on while they were out; nothing new is shared, but letting back in says it.
        paused_at(&mut c, 42_000);
        c.cmd_listen_kick(1, false);
        assert!(statuses(&rx).contains(&"Friend 1 can listen again.".to_owned()));
        let due = land(&mut c, t + 2_000);
        assert_eq!(due.len(), 1);
        let state = listen::parse(&due[0].1).expect("a state");
        assert_eq!((state.mode, state.position_ms), (Mode::Pause, 42_000));
        assert_eq!(c.links.get(1).map(|l| (l.kicked, l.stop_owed)), Some((false, false)));
        // With sharing off it says so instead. A kick undone before its stop went: the stop
        // never goes.
        c.update_settings(|s| s.listen_share = false);
        c.cmd_listen_kick(1, true);
        c.cmd_listen_kick(1, false);
        let due = land(&mut c, t + 3_000);
        assert_eq!(due.len(), 1);
        assert!(due[0].1.starts_with("Not sharing right now."), "{}", due[0].1);
        done(c);
    }

    #[test]
    fn switching_off_tells_live_links_only() {
        let (mut c, _rx) = with_links("off", 2);
        c.cmd_listen_share(true);
        let t = now_ms();
        paused_at(&mut c, 10_000);
        c.queue_news(t);
        land(&mut c, t);
        c.cmd_listen_kick(2, true);
        land(&mut c, t + 1_000);
        c.cmd_listen_share(false);
        let due = land(&mut c, t + 2_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, link(1));
        assert!(due[0].1.starts_with("Not sharing right now."));
        // Off: nothing more is shared.
        paused_at(&mut c, 90_000);
        c.queue_news(t + 3_000);
        assert!(land(&mut c, t + 3_000).is_empty());
        done(c);
    }

    #[test]
    fn a_gone_link_is_marked_not_replaced() {
        let (mut c, rx) = with_links("gone", 2);
        c.update_settings(|s| s.listen_share = true);
        while rx.try_recv().is_ok() {}
        c.on_written_at(link(1), "x".into(), Err(gone()), now_ms());
        let l = c.links.get(1).unwrap();
        assert!(l.gone);
        assert!(statuses(&rx).contains(&"Friend 1's link stopped working.".to_owned()));
        // No new playlist by itself: the friend wouldn't have it.
        assert!(c.along.making.is_empty());
        assert_eq!(c.links.links.len(), 2);
        assert_eq!(lock(&c.settings).listen_channel, Some(link(2)), "older Truffles move on");
        assert_eq!(c.links_view()[0].state, LinkState::Gone);
        // One not checked yet may just be another account's: whose it is decides, not one refusal.
        c.links.get_mut(2).unwrap().owner = String::new();
        c.on_written_at(link(2), "x".into(), Err(gone()), now_ms());
        assert!(!c.links.get(2).unwrap().gone);
        done(c);
    }

    #[test]
    fn a_link_made_this_session_is_never_marked_gone() {
        let (mut c, _rx) = with_links("made", 0);
        let id = c.links.reserve_id();
        c.along.making.push(Making { id, name: "Sam".into(), remake: false, started: true });
        assert_eq!(c.links_view()[0].state, LinkState::Making);
        c.on_link_made(id, Ok(link(7)), None);
        let l = c.links.get(id).expect("in the book");
        assert_eq!((l.name.as_str(), l.owner.as_str()), ("Sam", ME));
        // Its first words wait to go (sharing is off: "not sharing"); until they land, Making.
        let view = c.links_view();
        assert_eq!((view[0].state, view[0].link.clone()), (LinkState::Making, None));
        let t = now_ms();
        let due = c.along.fanout.take_due(t);
        assert_eq!(due.len(), 1);
        // Spotify doesn't know it everywhere yet: refused, but not taken as gone; tried again.
        c.on_written_at(due[0].0.clone(), due[0].1.clone(), Err(gone()), t);
        assert!(!c.links.get(id).unwrap().gone);
        assert_eq!(land(&mut c, t + 6_000).len(), 1);
        let view = c.links_view();
        assert_eq!(view[0].state, LinkState::Ready);
        assert_eq!(view[0].link, listen::channel_link(&link(7)));
        assert_eq!(lock(&c.settings).listen_channel, Some(link(7)));
        done(c);
    }

    #[test]
    fn removing_a_link_stops_it_and_join_refuses_it() {
        let (mut c, rx) = with_links("remove", 2);
        c.update_settings(|s| s.listen_share = true);
        let t = now_ms();
        paused_at(&mut c, 10_000);
        c.queue_news(t);
        land(&mut c, t);
        c.cmd_listen_remove_link(1);
        assert!(c.links.get(1).is_none());
        assert!(c.links.removed.iter().any(|r| r.uri == link(1) && r.stop_owed));
        assert!(statuses(&rx).contains(&"Removed Friend 1's link.".to_owned()));
        let due = land(&mut c, t + 1_000);
        assert_eq!(due.len(), 1);
        assert!(due[0].0 == link(1) && is_stop(&due[0].1));
        assert!(c.links.removed.iter().all(|r| !r.stop_owed), "its stop landed");
        // Never written again.
        paused_at(&mut c, 80_000);
        c.queue_news(t + 2_000);
        assert_eq!(land(&mut c, t + 2_000).iter().map(|(u, _)| u.clone()).collect::<Vec<_>>(), vec![link(2)]);
        // Joining one's own link, removed or not, is refused before anything is read.
        for n in [1, 2] {
            c.cmd_listen_join(listen::channel_link(&link(n)).unwrap());
            assert_eq!(statuses(&rx), vec!["That's your own listen-along link.".to_owned()]);
        }
        done(c);
    }

    #[test]
    fn a_link_back_from_gone_gets_its_stop_or_what_plays() {
        let (mut c, _rx) = with_links("back", 2);
        c.update_settings(|s| s.listen_share = true);
        let t = now_ms();
        paused_at(&mut c, 10_000);
        // Link 1 is kicked, and its stop is refused as gone: nothing is owed any more.
        c.cmd_listen_kick(1, true);
        let due = c.along.fanout.take_due(t);
        assert!(due.len() == 1 && due[0].0 == link(1) && is_stop(&due[0].1));
        c.on_written_at(link(1), due[0].1.clone(), Err(gone()), t);
        // Link 2 stops working while live.
        c.on_written_at(link(2), "x".into(), Err(gone()), t);
        assert!(c.links.links.iter().all(|l| l.gone && !l.stop_owed));
        // At the next login both can be read again (put back on spotify.com).
        c.on_links_checked(vec![(link(1), Ok(ME.into())), (link(2), Ok(ME.into()))]);
        assert!(c.links.get(1).is_some_and(|l| !l.gone && l.kicked && l.stop_owed), "the kick's stop is owed again");
        let due = land(&mut c, t + 1_000);
        let to = |n: u32| due.iter().find(|(u, _)| *u == link(n)).map(|(_, x)| x.clone());
        assert!(to(1).is_some_and(|x| is_stop(&x)), "the kicked one hears stopped at last");
        let state = to(2).and_then(|x| listen::parse(&x)).expect("the live one hears what plays");
        assert_eq!((state.mode, state.position_ms), (Mode::Pause, 10_000));
        assert!(!c.links.get(1).unwrap().stop_owed, "it landed");
        let states: Vec<LinkState> = c.links_view().iter().map(|v| v.state).collect();
        assert_eq!(states, vec![LinkState::Kicked, LinkState::Ready]);
        done(c);
    }

    #[test]
    fn letting_back_in_while_a_song_loads_takes_the_stop_back() {
        let (mut c, _rx) = with_links("loading", 1);
        c.update_settings(|s| s.listen_share = true);
        let t = now_ms();
        paused_at(&mut c, 10_000);
        c.queue_news(t);
        land(&mut c, t);
        // Kicked, and let back in before its stop went, while the next song loads.
        c.cmd_listen_kick(1, true);
        c.pb.status = PlayStatus::Loading;
        c.cmd_listen_kick(1, false);
        c.queue_news(t + 1_000);
        assert!(land(&mut c, t + 1_000).is_empty(), "the kick's stop never goes");
        // Once the song plays, it is shared at once, also with the link let back in.
        c.pb.track = Some(track("n"));
        c.pb.status = PlayStatus::Paused;
        c.pb.position_ms = 0;
        c.queue_news(t + 2_000);
        let due = land(&mut c, t + 2_000);
        assert_eq!(due.len(), 1);
        assert_eq!(listen::parse(&due[0].1).map(|s| s.track), Some("spotify:track:n".to_owned()));
        // A stop already on its way that fails isn't tried again after the undo.
        c.cmd_listen_kick(1, true);
        let due = c.along.fanout.take_due(t + 3_000);
        assert!(due.len() == 1 && is_stop(&due[0].1));
        c.pb.status = PlayStatus::Loading;
        c.cmd_listen_kick(1, false);
        c.on_written_at(link(1), due[0].1.clone(), Err(anyhow!("no answer in time")), t + 3_100);
        c.queue_news(t + 10_000);
        assert!(land(&mut c, t + 10_000).is_empty(), "not tried again");
        done(c);
    }

    #[test]
    fn a_removed_link_another_account_cant_write_still_owes_its_stop() {
        let (mut c, _rx) = with_links("tomb", 0);
        // alex's removed link, never checked (removed before the first login read whose it is).
        c.links.bury(&link(3), "", true);
        // Another account logs in on this PC: the stop goes, and Spotify refuses it.
        c.profile = profile("kai");
        c.listen_logged_in();
        let t = now_ms();
        let due = c.along.fanout.take_due(t);
        assert_eq!(due.len(), 1);
        c.on_written_at(link(3), due[0].1.clone(), Err(refused()), t);
        assert!(c.links.removed[0].stop_owed, "not given up on: it may be another account's");
        // Read: it is alex's. Still owed, at alex's next login only.
        c.on_links_checked(vec![(link(3), Ok(ME.into()))]);
        assert_eq!((c.links.removed[0].owner.as_str(), c.links.removed[0].stop_owed), (ME, true));
        assert!(c.links.owed_stops("kai").is_empty());
        assert_eq!(c.links.owed_stops(ME), vec![link(3)]);
        assert!(c.along.fanout.take_due(t + 10_000).is_empty(), "kai says nothing more to it");
        // One of the logged-in account's own that can't be written: given up on.
        c.links.bury(&link(4), "kai", true);
        c.on_written_at(link(4), "x".into(), Err(refused()), t);
        assert!(c.links.removed.iter().any(|r| r.uri == link(4) && !r.stop_owed));
        // Never checked and the logged-in account's after all: said once more, then given up on.
        c.links.bury(&link(5), "", true);
        c.on_written_at(link(5), "x".into(), Err(refused()), t);
        c.on_links_checked(vec![(link(5), Ok("kai".into()))]);
        let due = c.along.fanout.take_due(t + 20_000);
        assert!(due.len() == 1 && due[0].0 == link(5) && is_stop(&due[0].1));
        c.on_written_at(link(5), due[0].1.clone(), Err(refused()), t + 20_000);
        assert!(c.links.removed.iter().any(|r| r.uri == link(5) && !r.stop_owed));
        done(c);
    }

    #[test]
    fn with_sharing_off_a_kick_says_what_every_link_says() {
        let (mut c, rx) = with_links("offkick", 3);
        assert!(!lock(&c.settings).listen_share, "off by default");
        let t = now_ms();
        c.cmd_listen_kick(1, true);
        c.cmd_listen_remove_link(2);
        let due = land(&mut c, t);
        assert_eq!(due.len(), 2);
        for (_, text) in &due {
            assert!(is_stop(text) && text.starts_with("Not sharing right now."), "{text}");
        }
        assert!(!c.links.get(1).unwrap().stop_owed && c.links.removed.iter().all(|r| !r.stop_owed), "both landed");
        // With sharing on, a kick is an ordinary stop.
        c.update_settings(|s| s.listen_share = true);
        c.cmd_listen_kick(3, true);
        let due = land(&mut c, t + 1_000);
        assert!(due[0].1.starts_with("Listen along with me on Truffle.") && is_stop(&due[0].1));
        // Asking for a link while logged out is a note, not an error.
        while rx.try_recv().is_ok() {}
        c.cmd_listen_new_link("Sam".into());
        let notes: Vec<Event> = rx.try_iter().collect();
        assert!(notes.iter().any(|e| matches!(e, Event::Status(s) if s == "Log in to Spotify first.")));
        assert!(!notes.iter().any(|e| matches!(e, Event::Error(_))));
        done(c);
    }

    #[test]
    fn a_link_made_as_the_session_ended_goes_at_the_next_login() {
        let (mut c, _rx) = with_links("orphan", 0);
        let id = c.links.reserve_id();
        c.along.making.push(Making { id, name: "Sam".into(), remake: false, started: true });
        // Logged out while its playlist was being made: the playlist comes for an old session.
        let epoch = c.epoch;
        c.epoch += 1;
        c.reset_listening();
        c.handle_job(Job::Listen { epoch, done: Done::LinkMade { id, owner: ME.into(), result: Ok(link(8)), left: None } });
        assert!(c.links.get(id).is_none(), "no link");
        assert!(c.links.removed.iter().any(|r| r.uri == link(8) && r.owner == ME && r.stop_owed), "but known, owing its stop");
        assert!(c.own_link(&link(8)));
        // One that failed leaves nothing.
        c.handle_job(Job::Listen { epoch, done: Done::LinkMade { id, owner: ME.into(), result: Err(anyhow!("no answer in time")), left: None } });
        assert_eq!(c.links.removed.len(), 1);
        // At the next login its stop goes; once that landed, the playlist leaves the library.
        c.listen_logged_in();
        let due = land(&mut c, now_ms());
        assert!(due.len() == 1 && due[0].0 == link(8) && is_stop(&due[0].1));
        assert!(c.links.removed.iter().all(|r| !r.stop_owed));
        done(c);
    }

    /// `path` held open by another program that lets nobody read it meanwhile (as an antivirus
    /// scan can, for a moment).
    #[cfg(windows)]
    fn hold_open(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).share_mode(0).open(path).unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn a_links_file_held_open_at_start_is_never_written_over_and_read_at_login() {
        let dir = std::env::temp_dir().join(format!("truffle-links-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        let mut book = crate::listen::links::LinkBook::default();
        for i in 1..=2 {
            let id = book.reserve_id();
            book.add(id, &format!("Friend {i}"), &link(i), ME, 0);
        }
        book.kick(1, true, ME);
        persist::save_links(&paths, &book).unwrap();
        let file = paths.listen_links_file();
        std::fs::remove_file(file.with_extension("json.bak")).unwrap();
        let before = std::fs::read(&file).unwrap();
        // Truffle starts while another program holds the file, and there is no copy.
        let held = hold_open(&file);
        let settings = Settings { listen_channel: Some(link(2)), ..Settings::default() };
        let (mut c, _rx) = controller_at(paths, Arc::new(Mutex::new(settings)));
        assert!(c.links_held);
        assert_eq!(c.links.live_uris(ME), vec![link(2)], "only the link older Truffles write to");
        c.save_links();
        drop(held);
        assert_eq!(std::fs::read(&file).unwrap(), before, "never written over");
        // Let go of by login: read then, with its kick.
        c.profile = profile(ME);
        c.listen_logged_in();
        assert!(!c.links_held);
        assert_eq!(c.links.links.len(), 2);
        assert!(c.links.get(1).is_some_and(|l| l.kicked));
        assert_eq!(c.links.live_uris(ME), vec![link(2)]);
        done(c);
    }

    #[test]
    fn a_kick_survives_a_restart() {
        let (mut c, _rx) = with_links("restart", 2);
        c.cmd_listen_kick(1, true);
        let (paths, settings) = (c.paths.clone(), c.settings.clone());
        drop(c);
        // Truffle restarts before the stop went out.
        let (mut c, _rx) = controller_at(paths, settings);
        let l = c.links.get(1).expect("still there");
        assert!(l.kicked && l.stop_owed);
        assert_eq!(c.links.live_uris(ME), vec![link(2)]);
        // At login the stop owed goes out first thing.
        c.profile = Some(UserProfile { username: ME.into(), display_name: String::new(), image_url: None, premium: None });
        c.listen_logged_in();
        let due = land(&mut c, now_ms());
        assert_eq!(due.len(), 1);
        assert!(due[0].0 == link(1) && is_stop(&due[0].1));
        assert!(!c.links.get(1).unwrap().stop_owed);
        done(c);
    }

    #[test]
    fn the_old_setting_follows_kicks() {
        let (mut c, _rx) = with_links("mirror", 2);
        let channel = |c: &Controller| lock(&c.settings).listen_channel.clone();
        assert_eq!(channel(&c), Some(link(1)));
        c.cmd_listen_kick(1, true);
        assert_eq!(channel(&c), Some(link(2)));
        // Saved at once, not seconds later: an older Truffle started now writes to no kicked link.
        // (The file itself isn't checked: tests running side by side share one save order.)
        assert!(c.settings_changed_at.is_none());
        c.cmd_listen_kick(2, true);
        assert_eq!(channel(&c), None);
        c.cmd_listen_kick(1, false);
        assert_eq!(channel(&c), Some(link(1)));
        // A link an older Truffle made meanwhile is taken in at the next start.
        c.update_settings(|s| s.listen_channel = Some(link(5)));
        c.save_settings();
        let (paths, settings) = (c.paths.clone(), c.settings.clone());
        drop(c);
        let (c, _rx) = controller_at(paths, settings);
        assert_eq!(c.links.by_uri(&link(5)).map(|l| l.name.as_str()), Some(crate::listen::links::OLDER_LINK));
        assert!(c.links.get(2).is_some_and(|l| l.kicked), "kicks kept");
        done(c);
    }

    #[test]
    fn the_one_old_channel_becomes_the_first_link_and_keeps_working() {
        let dir = std::env::temp_dir().join(format!("truffle-links-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        let settings = Settings { listen_share: true, listen_channel: Some(link(3)), ..Settings::default() };
        let (c, _rx) = controller_at(paths, Arc::new(Mutex::new(settings)));
        assert_eq!(c.links.links.len(), 1);
        let l = &c.links.links[0];
        assert_eq!((l.uri.clone(), l.name.as_str()), (link(3), crate::listen::links::FIRST_LINK));
        assert!(c.paths.listen_links_file().exists(), "written at once");
        // Settings gives it no date: when it was made isn't known (not the day of the upgrade).
        assert_eq!(c.links_view()[0].made_ms, 0);
        // Sharing stays as it was, and the friend's link is still where older Truffles write.
        let s = lock(&c.settings).clone();
        assert_eq!((s.listen_share, s.listen_channel), (true, Some(link(3))));
        done(c);
    }

    #[test]
    fn link_playlists_never_reach_the_lists() {
        let (mut c, _rx) = with_links("hide", 1);
        c.along.made.insert(link(2));
        c.links.remove(1);
        let id = c.links.reserve_id();
        c.links.add(id, "Kai", &link(4), ME, 0);
        c.update_settings(|s| s.listen_channel = Some(link(3)));
        c.playlists = vec![
            pl("spotify:playlist:mine", "Mine", "", true),
            pl(&link(1), "Removed one", "", true),
            pl(&link(2), "Made just now", "", true),
            pl(&link(3), "Older Truffles'", "", true),
            pl(&link(4), "Renamed in Spotify", "", true),
            pl("spotify:playlist:elsewhere", listen::CHANNEL_NAME, "", true),
            pl("spotify:playlist:worded", "Anything", "Listen along with me on Truffle. [tl1;;0;5;stop]", true),
            pl("spotify:playlist:theirs", listen::CHANNEL_NAME, "", false),
        ];
        let shown: Vec<String> = c.playlists_for_ui().into_iter().map(|p| p.name).collect();
        assert_eq!(shown, vec!["Mine".to_owned(), listen::CHANNEL_NAME.to_owned()], "only another's one stays");
        done(c);
    }

    #[test]
    fn a_links_file_damaged_once_is_never_lost_at_the_next_start() {
        let dir = std::env::temp_dir().join(format!("truffle-links-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        // Alyssa's link (the one older Truffles write to) and Sam's, kicked.
        let mut book = crate::listen::links::LinkBook::default();
        for (i, name) in [(1, "Alyssa"), (2, "Sam")] {
            let id = book.reserve_id();
            book.add(id, name, &link(i), ME, 0);
        }
        book.kick(2, true, ME);
        persist::save_links(&paths, &book).unwrap();
        // A power cut leaves the file all zero bytes.
        std::fs::write(paths.listen_links_file(), [0u8; 64]).unwrap();
        let settings = Arc::new(Mutex::new(Settings { listen_channel: Some(link(1)), ..Settings::default() }));
        let (c, _rx) = controller_at(paths.clone(), settings.clone());
        assert_eq!(c.links, book, "the copy");
        assert_eq!(c.links_notice, Some(persist::LINKS_FROM_BACKUP));
        // A session that changes no link saves nothing; the next start still has them all.
        drop(c);
        let (c, _rx) = controller_at(paths, settings);
        assert_eq!(c.links, book, "every link, kick and name");
        assert_eq!(c.links_notice, None, "nothing more to say");
        done(c);
    }

    #[test]
    fn no_link_changes_while_its_file_cant_be_read() {
        let (mut c, rx) = with_links("locked", 2);
        c.update_settings(|s| s.listen_share = true);
        // listen_links.json couldn't be opened at start, nor at login: changes couldn't be saved,
        // and the next start would undo them (a kicked friend hearing again).
        c.links_held = true;
        let (before, mirror) = (c.links.clone(), lock(&c.settings).listen_channel.clone());
        while rx.try_recv().is_ok() {}
        c.cmd_listen_kick(1, true);
        c.cmd_listen_remove_link(2);
        c.cmd_listen_rename_link(1, "Sam".into());
        assert_eq!(c.links, before, "nothing changed");
        assert_eq!(lock(&c.settings).listen_channel, mirror, "nor the link older Truffles write to");
        assert!(c.along.fanout.take_due(now_ms()).is_empty(), "nothing written");
        let events: Vec<Event> = rx.try_iter().collect();
        let notes = events.iter().filter(|e| matches!(e, Event::Status(s) if s == persist::LINKS_LOCKED)).count();
        assert_eq!(notes, 3, "each says why");
        // The rows as they are (Remove took its row away at once).
        assert!(events.iter().any(|e| matches!(e, Event::ListenLinks(v) if v.len() == 2)));
        assert!(!events.iter().any(|e| matches!(e, Event::Error(_))), "notes, not errors");
        // Once it could be read, they go through.
        c.links_held = false;
        c.cmd_listen_kick(1, true);
        assert!(c.links.get(1).is_some_and(|l| l.kicked));
        done(c);
    }

    #[test]
    fn a_removed_links_playlist_is_owed_out_of_the_library_until_it_is() {
        let (mut c, _rx) = with_links("delete", 3);
        let t = now_ms();
        // Link 1 removed: once its stop landed, its playlist is owed out of the library.
        c.cmd_listen_remove_link(1);
        assert!(c.links.owed_deletes(ME).is_empty(), "not before its stop");
        land(&mut c, t);
        assert_eq!(c.links.owed_deletes(ME), vec![link(1)]);
        // Link 2 kicked (its stop landed), then removed: owed at once.
        c.cmd_listen_kick(2, true);
        land(&mut c, t + 1_000);
        c.cmd_listen_remove_link(2);
        assert_eq!(c.links.owed_deletes(ME), vec![link(1), link(2)]);
        // Saved: quitting before Spotify answered (or a restart) keeps them owed.
        let (paths, settings) = (c.paths.clone(), c.settings.clone());
        drop(c);
        let (mut c, _rx) = controller_at(paths, settings);
        c.profile = profile(ME);
        assert_eq!(c.links.owed_deletes(ME), vec![link(1), link(2)]);
        // One that failed stays owed (tried again at login, reconnect and refresh); one that went
        // owes nothing more.
        c.on_link_deleted(&link(1), Err(anyhow!("Your library changed while saving. Please try again.")));
        c.on_link_deleted(&link(2), Ok(()));
        assert_eq!(c.links.owed_deletes(ME), vec![link(1)]);
        // One that went for a session that ended meanwhile too.
        let epoch = c.epoch;
        c.epoch += 1;
        c.handle_job(Job::Listen { epoch, done: Done::LinkDeleted { uri: link(1), result: Ok(()) } });
        assert!(c.links.owed_deletes(ME).is_empty());
        // A removed link's playlist a library refresh still finds is owed again; a live link's,
        // another account's and the user's own playlists aren't.
        c.playlists = vec![pl(&link(2), "x", "", true), pl(&link(3), "x", "", true), pl("spotify:playlist:mine", "Mine", "", true)];
        c.links.bury(&link(4), "kai", false);
        c.playlists.push(pl(&link(4), "x", "", false));
        c.delete_listed_links();
        assert_eq!(c.links.removed.iter().filter(|r| r.delete_owed).map(|r| r.uri.clone()).collect::<Vec<_>>(), vec![link(2)]);
        assert!(c.paths.listen_links_file().exists());
        let (paths, settings) = (c.paths.clone(), c.settings.clone());
        drop(c);
        let (c, _rx) = controller_at(paths, settings);
        assert_eq!(c.links.owed_deletes(ME), vec![link(2)], "saved");
        done(c);
    }

    #[tokio::test]
    async fn a_delete_still_on_its_way_as_a_reconnect_finishes_is_asked_for_again_if_it_fails() {
        let (mut c, _rx) = with_links("redelete", 1);
        let (jobs, mut answers) = tokio::sync::mpsc::unbounded_channel();
        c.jobs = jobs;
        let t = now_ms();
        // Link 1 kicked (its stop landed), then removed: its playlist is asked out of the library.
        c.cmd_listen_kick(1, true);
        land(&mut c, t);
        // A session never connected: nothing reaches Spotify, and every delete fails at once.
        let session = || librespot_core::Session::new(librespot_core::SessionConfig::default(), None);
        c.library = Some(Library::new(session(), c.paths.clone()));
        c.cmd_listen_remove_link(1);
        assert_eq!(c.along.deleting.get(&link(1)), Some(&0));
        let answer = answers.recv().await.expect("its answer");
        // The connection was lost meanwhile, and the reconnect finished before that answer came:
        // the retry after reconnecting skips the delete still on its way.
        c.reconnect = Some((7, tokio::spawn(async {})));
        c.on_reconnected(7, Some(session()));
        assert_eq!(c.along.deleting.get(&link(1)), Some(&0), "not asked twice at once");
        // It failed: asked for again at once, on the new connection.
        c.handle_job(answer);
        assert_eq!(c.along.deleting.get(&link(1)), Some(&1), "asked for again");
        let answer = answers.recv().await.expect("its answer");
        // Failed again, with no reconnect since: left for the next login, reconnect or refresh.
        c.handle_job(answer);
        assert!(c.along.deleting.is_empty(), "not asked again and again");
        assert_eq!(c.links.owed_deletes(ME), vec![link(1)], "still owed");
        done(c);
    }

    #[test]
    fn a_link_whose_making_failed_after_its_playlist_was_made_goes() {
        let (mut c, rx) = with_links("unmade", 0);
        let id = c.links.reserve_id();
        c.along.making.push(Making { id, name: "Sam".into(), remake: false, started: true });
        // Made, but adding it to the library had no clear answer: it may be there all the same.
        let err = anyhow!("Made the playlist, but couldn't add it to your library. Use Refresh.");
        c.on_link_made(id, Err(err), Some(link(6)));
        assert!(c.links.get(id).is_none(), "no link");
        assert!(statuses(&rx).contains(&"Couldn't make the link.".to_owned()));
        let r = c.links.removed.iter().find(|r| r.uri == link(6)).expect("known, so hidden and never taken in");
        assert!(!r.stop_owed && r.delete_owed, "nobody has it: out of the library, no stop");
        assert_eq!(c.links.owed_deletes(ME), vec![link(6)]);
        // The same for a session that ended meanwhile (its stop first, at the next login).
        let id = c.links.reserve_id();
        let epoch = c.epoch;
        c.epoch += 1;
        c.handle_job(Job::Listen { epoch, done: Done::LinkMade { id, owner: ME.into(), result: Err(anyhow!("no answer in time")), left: Some(link(7)) } });
        assert!(c.links.removed.iter().any(|r| r.uri == link(7) && r.stop_owed));
        done(c);
    }

    #[test]
    fn making_a_broken_link_anew_as_it_works_again_keeps_the_working_one() {
        let (mut c, rx) = with_links("remade", 1);
        c.update_settings(|s| s.listen_share = true);
        let t = now_ms();
        paused_at(&mut c, 10_000);
        // Friend 1 was kicked, then the link stopped working.
        c.cmd_listen_kick(1, true);
        c.on_written_at(link(1), "x".into(), Err(gone()), t);
        assert!(c.links.get(1).is_some_and(|l| l.gone && !l.stop_owed));
        // At login it is read again (put back on spotify.com) while alex makes it anew: the check
        // comes back first, and the kick's stop is owed again.
        c.along.making.push(Making { id: 1, name: "Friend 1".into(), remake: true, started: true });
        c.on_links_checked(vec![(link(1), Ok(ME.into()))]);
        while rx.try_recv().is_ok() {}
        c.on_link_made(1, Ok(link(9)), None);
        let l = c.links.get(1).unwrap();
        assert_eq!((l.uri.clone(), l.gone, l.kicked, l.stop_owed), (link(1), false, true, true), "the link its friend has");
        assert!(statuses(&rx).contains(&"Friend 1's link works again.".to_owned()));
        // The new playlist is nobody's: out of the library, never written.
        assert!(c.links.removed.iter().any(|r| r.uri == link(9) && !r.stop_owed && r.delete_owed));
        let due = land(&mut c, t + 1_000);
        assert_eq!(due.len(), 1);
        assert!(due[0].0 == link(1) && is_stop(&due[0].1), "the kicked friend still hears stopped");
        assert_eq!(c.links_view()[0].state, LinkState::Kicked);
        done(c);
    }

    #[test]
    fn a_link_made_while_a_song_loads_hears_it_once_it_plays() {
        let (mut c, _rx) = with_links("makeload", 0);
        c.update_settings(|s| s.listen_share = true);
        paused_at(&mut c, 10_000);
        c.queue_news(now_ms());
        // The song loads again where it was (a reconnect) as a new link's playlist is made.
        c.pb.status = PlayStatus::Loading;
        let id = c.links.reserve_id();
        c.along.making.push(Making { id, name: "Sam".into(), remake: false, started: true });
        c.on_link_made(id, Ok(link(5)), None);
        let t = now_ms();
        let due = land(&mut c, t);
        assert!(due.len() == 1 && is_stop(&due[0].1), "stopped for now");
        // It plays on at the same spot: not news, but the new link hears it.
        c.pb.status = PlayStatus::Paused;
        c.queue_news(t + 1_000);
        let due = land(&mut c, t + 1_000);
        let state = due.first().and_then(|(_, x)| listen::parse(x)).expect("what plays");
        assert_eq!((state.mode, state.position_ms), (Mode::Pause, 10_000));
        done(c);
    }

    #[test]
    fn the_hosts_song_heard_again_after_it_ended_here_is_a_new_play() {
        let mut c = controller("again");
        c.follow(host(), "Robin".into());
        c.play_host_song(track("h"), 0, true);
        // It played to its end here (counted), then the host plays it again from the start.
        c.listen.recorded = true;
        c.end_handled = true;
        c.set_position(200_000);
        let play = c.play_id;
        c.resume_at(0);
        assert_ne!(c.play_id, play);
        assert!(!c.listen.recorded);
        done(c);
    }
}
