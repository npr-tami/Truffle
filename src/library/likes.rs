//! Likes and unlikes on their way to Spotify. Each one goes into a queue kept per account in
//! `like_queue.json` (a file of its own, which an older Truffle leaves alone), where the latest
//! change of a song takes the place of one still waiting. The file is written (atomically) before
//! a change is sent and again once Spotify has it, so a crash, a restart, a logout or going
//! offline loses none.
//!
//! A task of its own ([`run`]), apart from the player and the window, sends the logged-in
//! account's queue in its order: a batch at a time and a second apart, as listen-along paces its
//! writes (Spotify's limit is shared with the music). It never gives up on a rate limit or network
//! trouble: it waits Spotify's Retry-After when there is one, else backs off (5 s, 15 s, 30 s,
//! 1 min, then every 5 min). Only a refusal clearly about one song drops that song: a batch
//! refused for its songs goes again in halves, and a song refused on its own is dropped once the
//! song after it goes through (when that is refused too, it is the write, and both wait).

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::lock;
use crate::config;
use crate::model::LikesWaiting;

/// Songs per write.
pub const BATCH: usize = 200;
/// Waits are counted in these (seconds; milliseconds in tests).
#[cfg(not(test))]
const UNIT_MS: u64 = 1_000;
#[cfg(test)]
const UNIT_MS: u64 = 1;
/// Between two writes.
const GAP: u64 = 1;
/// After failures in a row: 5 s, 15 s, 30 s, 1 min, then every 5 min.
const BACKOFF: [u64; 5] = [5, 15, 30, 60, 300];
/// A write with no answer after this long is taken as lost (and sent again).
const SEND_TIMEOUT: u64 = 120;
const FILE_VERSION: u32 = 1;

fn units(n: u64) -> Duration {
    Duration::from_millis(n * UNIT_MS)
}

/// A Spotify song's URI: the only thing Liked Songs takes.
pub fn is_song(uri: &str) -> bool {
    uri.strip_prefix("spotify:track:").is_some_and(|id| id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The key of an account's queue (Spotify user names are compared without case).
fn account(user: &str) -> String {
    user.to_ascii_lowercase()
}

/// A like or unlike of one song.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Change {
    uri: String,
    liked: bool,
    /// Tells changes apart (a new one for each; not saved).
    #[serde(skip)]
    seq: u64,
}

#[derive(Default, Deserialize)]
struct QueueFile {
    #[serde(default)]
    accounts: BTreeMap<String, Vec<Change>>,
}

#[derive(Serialize)]
struct QueueFileRef<'a> {
    version: u32,
    accounts: &'a BTreeMap<String, Vec<Change>>,
}

/// What the sender is doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum State {
    #[default]
    Idle,
    Sending,
    /// Nothing goes out until then, and why.
    Waiting { until: SystemTime, why: String },
}

/// What the sender tells the controller.
#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    /// Spotify took these.
    Sent { uris: Vec<String>, liked: bool },
    /// Spotify refused this song on its own and took the one after it: taken out.
    Dropped { uri: String, liked: bool },
    /// Nothing goes out for a while (see [`LikeQueue::state`]).
    Waiting,
}

/// Why a write didn't go through.
#[derive(Clone, Debug, PartialEq)]
pub enum SendError {
    /// Spotify said to slow down, and for how long if it said.
    Slow(Option<Duration>),
    /// Network trouble, or no connection to Spotify.
    Network(String),
    /// Refused for its songs (a bad id).
    Refused(String),
    /// Refused for another reason.
    Other(String),
}

#[derive(Default)]
struct Book {
    /// Each account's changes, oldest first.
    accounts: BTreeMap<String, Vec<Change>>,
    next_seq: u64,
    /// The file is there but couldn't be used at load: never written over until it is read.
    held: bool,
    state: State,
    /// Why the last write didn't go through, and when.
    last_error: Option<(String, SystemTime)>,
}

impl Book {
    /// `uris` liked (or unliked) at the end of `user`'s queue, each in place of a change of the
    /// same song.
    fn add(&mut self, user: &str, uris: &[String], liked: bool) {
        let fresh: HashSet<&str> = uris.iter().map(String::as_str).collect();
        let queue = self.accounts.entry(account(user)).or_default();
        queue.retain(|c| !fresh.contains(c.uri.as_str()));
        let mut seen = HashSet::new();
        for uri in uris.iter().filter(|u| seen.insert(u.as_str())) {
            self.next_seq += 1;
            queue.push(Change { uri: uri.clone(), liked, seq: self.next_seq });
        }
    }

    /// Takes in a saved queue: what was saved goes before anything changed since (and gives way
    /// to it). Anything that isn't a song is left out; of a song saved twice, the later counts.
    fn take_in(&mut self, file: QueueFile) {
        for (user, saved) in file.accounts {
            let mut seen = HashSet::new();
            let mut kept: Vec<Change> = saved
                .into_iter()
                .rev()
                .filter(|c| {
                    if !is_song(&c.uri) {
                        log::warn!("likes: left out {:?} (not a Spotify song)", c.uri);
                        return false;
                    }
                    seen.insert(c.uri.clone())
                })
                .collect();
            kept.reverse();
            let queue = self.accounts.entry(account(&user)).or_default();
            let newer: HashSet<&str> = queue.iter().map(|c| c.uri.as_str()).collect();
            kept.retain(|c| !newer.contains(c.uri.as_str()));
            for c in &mut kept {
                self.next_seq += 1;
                c.seq = self.next_seq;
            }
            kept.append(queue);
            *queue = kept;
        }
        self.accounts.retain(|_, queue| !queue.is_empty());
    }
}

enum Read {
    Missing,
    Parsed(QueueFile),
    /// Read, but not a queue.
    Corrupt(String),
    /// There, but couldn't be opened.
    Failed,
}

fn read(path: &Path) -> Read {
    if !path.exists() {
        return Read::Missing;
    }
    let Some(bytes) = config::read_with_retry(path) else {
        return if path.exists() { Read::Failed } else { Read::Missing };
    };
    // An empty file has nothing waiting.
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Read::Parsed(QueueFile::default());
    }
    match serde_json::from_slice(&bytes) {
        Ok(file) => Read::Parsed(file),
        Err(e) => Read::Corrupt(e.to_string()),
    }
}

/// The file couldn't be used at load: read it now and take in what it has, or set it aside when
/// it is damaged. False while it can't be opened (or moved) yet: then nothing is written.
fn read_again(book: &mut Book, path: &Path) -> bool {
    match read(path) {
        Read::Missing => {}
        Read::Parsed(file) => {
            log::info!("read {} after all", path.display());
            book.take_in(file);
        }
        Read::Corrupt(why) => {
            if config::quarantine(path).is_none() {
                return false;
            }
            log::warn!("{} can't be read ({why}); set it aside", path.display());
        }
        Read::Failed => return false,
    }
    book.held = false;
    true
}

struct Inner {
    path: PathBuf,
    book: Mutex<Book>,
    /// Saves take turns, so the file always ends up with the newest queue.
    saving: Mutex<()>,
    /// Something new to send.
    work: Notify,
    /// The connection to Spotify is back.
    network: Notify,
}

/// Every account's queue, shared by the controller (which adds changes) and the sender (which
/// takes out what Spotify took).
#[derive(Clone)]
pub struct LikeQueue {
    inner: Arc<Inner>,
}

impl LikeQueue {
    /// Reads the saved queue. A damaged file is set aside (`like_queue.json.corrupt-<time>`) and
    /// the queue starts empty; one that can't be opened is read again before anything is saved
    /// over it.
    pub fn load(path: &Path) -> Self {
        let mut book = Book::default();
        match read(path) {
            Read::Missing => {}
            Read::Parsed(file) => book.take_in(file),
            Read::Corrupt(why) => {
                log::error!("{} can't be read ({why}); setting it aside", path.display());
                book.held = config::quarantine(path).is_none();
            }
            Read::Failed => {
                log::error!("{} can't be opened; it is read again before it is saved", path.display());
                book.held = true;
            }
        }
        let waiting: usize = book.accounts.values().map(Vec::len).sum();
        if waiting > 0 {
            log::info!("likes: {waiting} waiting from before");
        }
        let inner = Inner { path: path.to_path_buf(), book: Mutex::new(book), saving: Mutex::new(()), work: Notify::new(), network: Notify::new() };
        Self { inner: Arc::new(inner) }
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        lock(&self.inner.book)
    }

    fn save(&self) {
        let _turn = lock(&self.inner.saving);
        let bytes = {
            let mut book = self.book();
            if book.held && !read_again(&mut book, &self.inner.path) {
                log::warn!("{} still can't be opened; not saved yet", self.inner.path.display());
                return;
            }
            serde_json::to_vec(&QueueFileRef { version: FILE_VERSION, accounts: &book.accounts })
        };
        let written = bytes.map_err(anyhow::Error::from).and_then(|bytes| config::write_atomic(&self.inner.path, &bytes));
        if let Err(e) = written {
            log::warn!("could not save the likes waiting for Spotify: {e:#}");
        }
    }

    /// Likes (or unlikes) these songs for `user`: saved, then sent by the sender.
    pub fn push(&self, user: &str, uris: &[String], liked: bool) {
        self.book().add(user, uris, liked);
        self.save();
        self.inner.work.notify_one();
    }

    /// How many likes and unlikes of `user` wait.
    pub fn waiting(&self, user: &str) -> LikesWaiting {
        let book = self.book();
        let queue = book.accounts.get(&account(user)).map_or(&[][..], Vec::as_slice);
        let likes = queue.iter().filter(|c| c.liked).count();
        LikesWaiting { likes, unlikes: queue.len() - likes }
    }

    /// The change of `uri` still waiting, if one is (true: a like).
    pub fn pending(&self, user: &str, uri: &str) -> Option<bool> {
        let book = self.book();
        book.accounts.get(&account(user))?.iter().find(|c| c.uri == uri).map(|c| c.liked)
    }

    /// Those of `uris` with no change of `user` waiting.
    pub fn not_pending<'a>(&self, user: &str, uris: &'a [String]) -> Vec<&'a String> {
        let book = self.book();
        let waiting: HashSet<&str> = book.accounts.get(&account(user)).into_iter().flatten().map(|c| c.uri.as_str()).collect();
        uris.iter().filter(|u| !waiting.contains(u.as_str())).collect()
    }

    /// `liked` (as Spotify has it) with `user`'s waiting changes on top.
    pub fn apply(&self, user: &str, liked: &mut HashSet<String>) {
        let book = self.book();
        for c in book.accounts.get(&account(user)).into_iter().flatten() {
            if c.liked {
                liked.insert(c.uri.clone());
            } else {
                liked.remove(&c.uri);
            }
        }
    }

    pub fn state(&self) -> State {
        self.book().state.clone()
    }

    /// The connection to Spotify is back: a wait for network trouble ends now.
    pub fn network_up(&self) {
        self.inner.network.notify_waiters();
    }

    /// The sender was stopped (logged out).
    pub fn stopped(&self) {
        self.set_state(State::Idle);
    }

    /// The self-test's report ("likes"): what waits, what the sender does, and the last error.
    pub fn report(&self, user: &str) -> String {
        let (waiting, mine) = (self.waiting(user), account(user));
        let book = self.book();
        let others: usize = book.accounts.iter().filter(|(key, _)| **key != mine).map(|(_, q)| q.len()).sum();
        let sender = match &book.state {
            State::Idle => "idle".to_owned(),
            State::Sending => "sending".to_owned(),
            State::Waiting { until, why } => {
                let left = until.duration_since(SystemTime::now()).unwrap_or_default().as_secs();
                format!("waiting until {} UTC (in {left} s): {why}", clock(*until))
            }
        };
        let error = book.last_error.as_ref().map_or_else(|| "none".to_owned(), |(why, at)| format!("{why} (at {} UTC)", clock(*at)));
        format!(
            "Likes waiting for {user}: {} ({} likes, {} unlikes)\nOther accounts: {others}\nSender: {sender}\nLast error: {error}\n",
            waiting.total(),
            waiting.likes,
            waiting.unlikes
        )
    }

    /// The first `n` changes of an account's queue.
    fn head(&self, key: &str, n: usize) -> Vec<Change> {
        self.book().accounts.get(key).map(|q| q.iter().take(n).cloned().collect()).unwrap_or_default()
    }

    /// Takes out the changes Spotify took (not ones that took their place meanwhile).
    fn take_out(&self, key: &str, sent: &[Change]) {
        let seqs: HashSet<u64> = sent.iter().map(|c| c.seq).collect();
        {
            let mut book = self.book();
            if let Some(queue) = book.accounts.get_mut(key) {
                queue.retain(|c| !seqs.contains(&c.seq));
                if queue.is_empty() {
                    book.accounts.remove(key);
                }
            }
        }
        self.save();
    }

    /// Takes out one change (refused), if it is still there.
    fn drop_change(&self, key: &str, seq: u64) -> Option<Change> {
        let change = {
            let mut book = self.book();
            let queue = book.accounts.get_mut(key)?;
            let change = queue.remove(queue.iter().position(|c| c.seq == seq)?);
            if queue.is_empty() {
                book.accounts.remove(key);
            }
            change
        };
        self.save();
        Some(change)
    }

    fn set_state(&self, state: State) {
        self.book().state = state;
    }

    fn note_error(&self, why: &str) {
        self.book().last_error = Some((why.to_owned(), SystemTime::now()));
    }
}

/// "23:41:05" (UTC).
fn clock(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// The wait after `failures` failures in a row (5 s, 15 s, 30 s, 1 min, then 5 min), scaled by
/// `jitter` (0.9 to 1.1).
fn backoff(failures: u32, jitter: f64) -> Duration {
    let step = usize::try_from(failures).unwrap_or(usize::MAX).min(BACKOFF.len() - 1);
    let jitter = if jitter.is_finite() { jitter.clamp(0.9, 1.1) } else { 1.0 };
    units(BACKOFF[step]).mul_f64(jitter)
}

/// Sends `user`'s queue for as long as the task runs: `send` makes one write, and `report` hears
/// what went through, what was dropped, and when a wait starts.
pub async fn run<F, Fut>(queue: LikeQueue, user: String, mut send: F, report: impl Fn(Report))
where
    F: FnMut(Vec<String>, bool) -> Fut,
    Fut: Future<Output = Result<(), SendError>>,
{
    let key = account(&user);
    let mut size = BATCH;
    let mut failures = 0u32;
    // A change Spotify refused on its own, and why: dropped once the one after it goes through.
    let mut suspect: Option<(u64, String)> = None;
    loop {
        let head = queue.head(&key, size + 1);
        let Some((first, liked)) = head.first().map(|c| (c.seq, c.liked)) else {
            suspect = None;
            queue.set_state(State::Idle);
            queue.inner.work.notified().await;
            continue;
        };
        let probe = suspect.as_ref().is_some_and(|(seq, _)| *seq == first);
        let batch: Vec<Change> = if probe {
            head.into_iter().skip(1).take(1).collect()
        } else {
            suspect = None;
            head.into_iter().take(size).take_while(|c| c.liked == liked).collect()
        };
        let Some(liked) = batch.first().map(|c| c.liked) else {
            // Nothing after it to tell by: it may be the write, not the song. Both wait.
            let why = suspect.take().map(|(_, why)| why).unwrap_or_default();
            hold(&queue, &key, backoff(failures, rand::random_range(0.9..1.1)), format!("Spotify refused a song: {why}"), false, &report).await;
            failures = failures.saturating_add(1);
            size = BATCH;
            continue;
        };
        queue.set_state(State::Sending);
        let uris: Vec<String> = batch.iter().map(|c| c.uri.clone()).collect();
        let sent = tokio::time::timeout(units(SEND_TIMEOUT), send(uris.clone(), liked))
            .await
            .unwrap_or_else(|_| Err(SendError::Network("no answer from Spotify".to_owned())));
        match sent {
            Ok(()) => {
                queue.take_out(&key, &batch);
                let kind = if liked { "likes" } else { "unlikes" };
                log::info!("likes: {} {kind} went through ({} waiting)", uris.len(), queue.waiting(&key).total());
                report(Report::Sent { uris, liked });
                if let Some((seq, why)) = suspect.take()
                    && let Some(change) = queue.drop_change(&key, seq)
                {
                    log::warn!("likes: Spotify refused {} on its own ({why}) and took the song after it; dropped it", change.uri);
                    report(Report::Dropped { uri: change.uri, liked: change.liked });
                }
                failures = 0;
                size = (size * 2).min(BATCH);
            }
            Err(SendError::Refused(why)) if batch.len() > 1 => {
                log::info!("likes: Spotify refused {} songs ({why}); sending them in halves", batch.len());
                queue.note_error(&why);
                size = batch.len() / 2;
            }
            Err(SendError::Refused(why)) if !probe => {
                log::info!("likes: Spotify refused {} ({why}); trying the song after it", batch[0].uri);
                queue.note_error(&why);
                suspect = Some((batch[0].seq, why));
            }
            Err(e) => {
                // A song after a refused one refused too means the write, not one song: both wait.
                suspect = None;
                let jitter = rand::random_range(0.9..1.1);
                let (wait, why, network) = match e {
                    SendError::Slow(Some(after)) => (after.max(units(GAP)), format!("Spotify asked to wait {} s", after.as_secs()), false),
                    SendError::Slow(None) => (backoff(failures, jitter), "Spotify asked to slow down".to_owned(), false),
                    SendError::Network(why) => (backoff(failures, jitter), format!("network trouble: {why}"), true),
                    SendError::Refused(why) => (backoff(failures, jitter), format!("Spotify refused the songs: {why}"), false),
                    SendError::Other(why) => (backoff(failures, jitter), why, true),
                };
                hold(&queue, &key, wait, why, network, &report).await;
                failures = failures.saturating_add(1);
                size = BATCH;
                continue;
            }
        }
        tokio::time::sleep(units(GAP)).await;
    }
}

/// Holds the sender for `wait` (`why` goes to the log and the self-test). A wait for network
/// trouble ends early when the connection is back.
async fn hold(queue: &LikeQueue, key: &str, wait: Duration, why: String, network: bool, report: &impl Fn(Report)) {
    log::info!("likes: {why}; sending again in {} s ({} waiting)", wait.as_secs(), queue.waiting(key).total());
    queue.note_error(&why);
    queue.set_state(State::Waiting { until: SystemTime::now() + wait, why });
    report(Report::Waiting);
    if network {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = queue.inner.network.notified() => log::info!("likes: the connection is back; sending again"),
        }
    } else {
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("truffle-likes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("like_queue.json")
    }

    /// `n` songs from `from` on, in order.
    fn songs_from(from: usize, n: usize) -> Vec<String> {
        (from..from + n).map(|i| format!("spotify:track:{i:0>22}")).collect()
    }

    fn songs(n: usize) -> Vec<String> {
        songs_from(0, n)
    }

    type Writes = Arc<Mutex<Vec<(Vec<String>, bool)>>>;

    /// Waits (up to 30 s) until `done`.
    async fn until(done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(30), "timed out");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Runs the sender for `user` until nothing of theirs waits and it rests. `answer` is Spotify:
    /// the n-th write's songs and kind in, what it says out. Returns the writes it took (in order)
    /// and what the sender reported.
    async fn send_all(
        queue: &LikeQueue,
        user: &str,
        answer: impl Fn(usize, &[String], bool) -> Result<(), SendError> + Send + Sync + 'static,
    ) -> (Vec<(Vec<String>, bool)>, Vec<Report>) {
        let took: Writes = Arc::default();
        let reports = Arc::new(Mutex::new(Vec::new()));
        let (calls, answer) = (Arc::new(AtomicUsize::new(0)), Arc::new(answer));
        let send = {
            let took = took.clone();
            move |uris: Vec<String>, liked: bool| {
                let (took, calls, answer) = (took.clone(), calls.clone(), answer.clone());
                async move {
                    let result = answer(calls.fetch_add(1, Ordering::SeqCst), &uris, liked);
                    if result.is_ok() {
                        took.lock().unwrap().push((uris, liked));
                    }
                    result
                }
            }
        };
        let heard = reports.clone();
        let task = tokio::spawn(run(queue.clone(), user.to_owned(), send, move |r| heard.lock().unwrap().push(r)));
        until(|| queue.waiting(user).is_empty() && queue.state() == State::Idle).await;
        task.abort();
        let took = took.lock().unwrap().clone();
        let reports = reports.lock().unwrap().clone();
        (took, reports)
    }

    fn all_songs(writes: &[(Vec<String>, bool)]) -> Vec<String> {
        writes.iter().flat_map(|(uris, _)| uris.iter().cloned()).collect()
    }

    #[test]
    fn only_spotify_songs_are_queued() {
        assert!(is_song("spotify:track:4uLU6hMCjMI75M1A2tKUQC"));
        assert!(!is_song("spotify:track:4uLU6hMCjMI75M1A2tKUQ"), "21 characters");
        assert!(!is_song("spotify:track:4uLU6hMCjMI75M1A2tKUQC "));
        assert!(!is_song("spotify:track:4uLU6hMCjMI75M1A2tKUQ🍄"));
        assert!(!is_song("spotify:episode:4uLU6hMCjMI75M1A2tKUQC"));
        assert!(!is_song("spotify:local:Artist:Album:Song:180"));
        assert!(!is_song(""));
    }

    #[test]
    fn waits_grow_to_five_minutes_and_stay() {
        let secs = |failures| backoff(failures, 1.0).as_millis() as u64;
        assert_eq!([0, 1, 2, 3, 4].map(secs), [5, 15, 30, 60, 300]);
        assert_eq!(secs(5), 300);
        assert_eq!(secs(u32::MAX), 300);
        assert_eq!(backoff(0, 2.0), units(5).mul_f64(1.1), "a little jitter, no more");
        assert_eq!(backoff(4, 0.0), units(300).mul_f64(0.9));
        assert_eq!(backoff(0, f64::NAN), units(5));
    }

    #[tokio::test]
    async fn ten_thousand_likes_get_through_429s_in_order() {
        let path = temp_file("ten-thousand");
        let queue = LikeQueue::load(&path);
        let all = songs(10_000);
        queue.push("alex", &all, true);
        assert_eq!(queue.waiting("alex"), LikesWaiting { likes: 10_000, unlikes: 0 });
        let times = Arc::new(Mutex::new(Vec::new()));
        let heard = times.clone();
        let (took, reports) = send_all(&queue, "alex", move |n, _, _| {
            heard.lock().unwrap().push(Instant::now());
            match n {
                // Spotify says to slow down with a Retry-After (twice), then without one, then the
                // connection drops.
                7 | 8 => Err(SendError::Slow(Some(Duration::from_millis(40)))),
                19 => Err(SendError::Slow(None)),
                29 => Err(SendError::Network("connection reset".into())),
                _ => Ok(()),
            }
        })
        .await;
        assert_eq!(all_songs(&took), all, "every like, once, in order");
        assert!(took.iter().all(|(uris, liked)| *liked && uris.len() <= BATCH));
        assert_eq!(took.len(), 50);
        let times = times.lock().unwrap();
        let gap = |n: usize| times[n + 1] - times[n];
        assert!(gap(7) >= Duration::from_millis(40) && gap(8) >= Duration::from_millis(40), "the Retry-After is waited");
        assert!(gap(19) >= units(4) && gap(29) >= units(4), "no Retry-After: backed off");
        assert_eq!(reports.iter().filter(|r| **r == Report::Waiting).count(), 4);
        assert_eq!(reports.iter().filter(|r| matches!(r, Report::Sent { .. })).count(), 50);
        assert!(queue.report("alex").contains("Last error: network trouble: connection reset"));
        assert_eq!(LikeQueue::load(&path).waiting("alex"), LikesWaiting::default(), "the saved queue is empty too");
    }

    #[tokio::test]
    async fn it_never_gives_up() {
        let queue = LikeQueue::load(&temp_file("never"));
        let all = songs(3);
        queue.push("alex", &all, false);
        // Spotify says to slow down six times running, and the network is down for two more tries.
        let (took, reports) = send_all(&queue, "alex", |n, _, _| match n {
            0..6 => Err(SendError::Slow(None)),
            6 | 7 => Err(SendError::Network("timed out".into())),
            _ => Ok(()),
        })
        .await;
        assert_eq!(took, vec![(all, false)]);
        assert_eq!(reports.iter().filter(|r| **r == Report::Waiting).count(), 8);
    }

    #[tokio::test]
    async fn a_write_with_no_answer_is_sent_again() {
        let queue = LikeQueue::load(&temp_file("no-answer"));
        let all = songs(5);
        queue.push("alex", &all, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let took: Writes = Arc::default();
        let (counted, kept) = (calls.clone(), took.clone());
        let send = move |uris: Vec<String>, liked: bool| {
            let (calls, took) = (counted.clone(), kept.clone());
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    std::future::pending::<()>().await;
                }
                took.lock().unwrap().push((uris, liked));
                Ok(())
            }
        };
        let task = tokio::spawn(run(queue.clone(), "alex".into(), send, |_| {}));
        until(|| queue.waiting("alex").is_empty()).await;
        task.abort();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(*took.lock().unwrap(), vec![(all, true)]);
    }

    #[tokio::test]
    async fn a_restart_halfway_goes_on_from_the_file() {
        let path = temp_file("restart");
        let queue = LikeQueue::load(&path);
        let all = songs(1_000);
        queue.push("alex", &all, true);
        // Spotify takes three writes; Truffle is closed while the fourth is on its way.
        let (calls, took): (Arc<AtomicUsize>, Writes) = Default::default();
        let (counted, kept) = (calls.clone(), took.clone());
        let send = move |uris: Vec<String>, liked: bool| {
            let (calls, took) = (counted.clone(), kept.clone());
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 3 {
                    std::future::pending::<()>().await;
                }
                took.lock().unwrap().push((uris, liked));
                Ok(())
            }
        };
        let task = tokio::spawn(run(queue.clone(), "alex".into(), send, |_| {}));
        until(|| calls.load(Ordering::SeqCst) == 4).await;
        task.abort();
        drop(queue);
        let first = took.lock().unwrap().clone();
        assert_eq!(first.len(), 3);

        let again = LikeQueue::load(&path);
        assert_eq!(again.waiting("alex"), LikesWaiting { likes: 400, unlikes: 0 });
        let (rest, _) = send_all(&again, "alex", |_, _, _| Ok(())).await;
        let sent: Vec<String> = all_songs(&first).into_iter().chain(all_songs(&rest)).collect();
        assert_eq!(sent, all, "none lost, none twice, in order");
    }

    #[tokio::test]
    async fn the_latest_change_of_a_song_wins() {
        let queue = LikeQueue::load(&temp_file("latest"));
        let [a, b, c]: [String; 3] = songs(3).try_into().unwrap();
        // Liked, then unliked before it went out: only the unlike goes.
        queue.push("alex", std::slice::from_ref(&a), true);
        queue.push("alex", &[b.clone(), a.clone()], false);
        assert_eq!(queue.waiting("alex"), LikesWaiting { likes: 0, unlikes: 2 });
        assert_eq!(queue.pending("alex", &a), Some(false));
        let mut liked: HashSet<String> = [a.clone(), b.clone(), c.clone()].into();
        queue.apply("alex", &mut liked);
        assert_eq!(liked, [c.clone()].into());
        let (took, _) = send_all(&queue, "alex", |_, _, _| Ok(())).await;
        assert_eq!(took, vec![(vec![b.clone(), a.clone()], false)]);

        // Unliked while its like is on its way: the like goes, then the unlike.
        queue.push("alex", &[a.clone(), c.clone()], true);
        let clicked = queue.clone();
        let unlike = c.clone();
        let (took, _) = send_all(&queue, "alex", move |n, _, _| {
            if n == 0 {
                clicked.push("alex", std::slice::from_ref(&unlike), false);
            }
            Ok(())
        })
        .await;
        assert_eq!(took, vec![(vec![a, c.clone()], true), (vec![c], false)]);
    }

    #[tokio::test]
    async fn a_refusal_about_one_song_drops_only_that_song() {
        let queue = LikeQueue::load(&temp_file("one-song"));
        let all = songs(10);
        let bad = all[4].clone();
        queue.push("alex", &all, true);
        let refused = bad.clone();
        let (took, reports) = send_all(&queue, "alex", move |_, uris, _| {
            if uris.contains(&refused) { Err(SendError::Refused("400 invalid id".into())) } else { Ok(()) }
        })
        .await;
        let others: Vec<String> = all.iter().filter(|u| **u != bad).cloned().collect();
        assert_eq!(all_songs(&took), others, "the rest went through, in order");
        assert_eq!(reports.iter().filter(|r| matches!(r, Report::Dropped { .. })).collect::<Vec<_>>(), [&Report::Dropped { uri: bad, liked: true }]);
        assert!(!reports.contains(&Report::Waiting), "no waiting for a bad song");
    }

    #[tokio::test]
    async fn a_refused_last_song_waits_for_one_after_it() {
        let queue = LikeQueue::load(&temp_file("last-song"));
        let all = songs(5);
        let bad = all[4].clone();
        queue.push("alex", &all, true);
        let (calls, took): (Arc<AtomicUsize>, Writes) = Default::default();
        let (counted, kept, refused) = (calls.clone(), took.clone(), bad.clone());
        let send = move |uris: Vec<String>, liked: bool| {
            let (calls, took, refused) = (counted.clone(), kept.clone(), refused.clone());
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if uris.contains(&refused) {
                    return Err(SendError::Refused("400".into()));
                }
                took.lock().unwrap().push((uris, liked));
                Ok(())
            }
        };
        let reports = Arc::new(Mutex::new(Vec::new()));
        let heard = reports.clone();
        let task = tokio::spawn(run(queue.clone(), "alex".into(), send, move |r| heard.lock().unwrap().push(r)));
        // Alone, it can't be told from a write Spotify refuses: kept, tried again later.
        until(|| queue.waiting("alex").likes == 1 && calls.load(Ordering::SeqCst) >= 8).await;
        assert_eq!(queue.pending("alex", &bad), Some(true));
        assert!(reports.lock().unwrap().contains(&Report::Waiting));
        assert!(!reports.lock().unwrap().iter().any(|r| matches!(r, Report::Dropped { .. })));
        // A song liked after it tells: Spotify takes that one, so the refusal was about this one.
        let next = songs_from(5, 1);
        queue.push("alex", &next, true);
        until(|| queue.waiting("alex").is_empty() && queue.state() == State::Idle).await;
        task.abort();
        assert!(reports.lock().unwrap().contains(&Report::Dropped { uri: bad, liked: true }));
        assert_eq!(all_songs(&took.lock().unwrap()), [&all[..4], &next[..]].concat());
    }

    #[tokio::test]
    async fn a_write_refused_for_every_song_drops_none() {
        let queue = LikeQueue::load(&temp_file("every-song"));
        let all = songs(10);
        queue.push("alex", &all, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let send = move |_: Vec<String>, _: bool| {
            counted.fetch_add(1, Ordering::SeqCst);
            async { Err(SendError::Refused("400 bad request".into())) }
        };
        let reports = Arc::new(Mutex::new(Vec::new()));
        let heard = reports.clone();
        let task = tokio::spawn(run(queue.clone(), "alex".into(), send, move |r| heard.lock().unwrap().push(r)));
        until(|| calls.load(Ordering::SeqCst) >= 20).await;
        task.abort();
        assert_eq!(queue.waiting("alex").likes, 10, "nothing dropped");
        let reports = reports.lock().unwrap();
        assert!(reports.iter().all(|r| *r == Report::Waiting) && !reports.is_empty());
    }

    #[tokio::test]
    async fn each_account_has_its_own_queue() {
        let path = temp_file("accounts");
        let queue = LikeQueue::load(&path);
        let (alexs, sams) = (songs(3), songs_from(3, 2));
        queue.push("alex", &alexs, true);
        // alex logs out, and sam logs in.
        queue.push("Sam", &sams, false);
        let mut liked: HashSet<String> = sams.iter().cloned().collect();
        queue.apply("sam", &mut liked);
        assert!(liked.is_empty(), "sam's hearts don't show alex's likes");
        let (took, _) = send_all(&queue, "sam", |_, _, _| Ok(())).await;
        assert_eq!(took, vec![(sams, false)]);
        assert_eq!(queue.waiting("alex").likes, 3);
        assert!(queue.report("sam").contains("Other accounts: 3"));
        // Back as alex (after a restart): theirs go now.
        let again = LikeQueue::load(&path);
        assert_eq!((again.waiting("ALEX").likes, again.waiting("sam").unlikes), (3, 0));
        let (took, _) = send_all(&again, "Alex", |_, _, _| Ok(())).await;
        assert_eq!(took, vec![(alexs, true)]);
    }

    /// Files set aside beside `path`.
    fn kept_aside(path: &Path) -> usize {
        let dir = std::fs::read_dir(path.parent().unwrap()).unwrap();
        dir.filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-")).count()
    }

    #[test]
    fn a_damaged_or_empty_file_is_an_empty_queue() {
        // Cut short, and full of zero bytes (a power cut): set aside, never written over.
        for (n, damaged) in [&b"{\"accounts\":{\"alex\":[{\"uri\":"[..], &[0u8; 64][..]].into_iter().enumerate() {
            let path = temp_file(&format!("damaged-{n}"));
            std::fs::write(&path, damaged).unwrap();
            let queue = LikeQueue::load(&path);
            assert_eq!(queue.waiting("alex"), LikesWaiting::default());
            assert!(!path.exists());
            assert_eq!(kept_aside(&path), 1);
            queue.push("alex", &songs(2), true);
            assert_eq!(LikeQueue::load(&path).waiting("alex").likes, 2);
        }
        // Empty: nothing waits, nothing to keep.
        let path = temp_file("empty");
        for empty in ["", "  \n"] {
            std::fs::write(&path, empty).unwrap();
            assert_eq!(LikeQueue::load(&path).waiting("alex"), LikesWaiting::default());
            assert!(path.exists());
        }
        assert_eq!(kept_aside(&path), 0);
    }

    #[test]
    fn a_saved_queue_keeps_songs_only_and_the_later_change() {
        let path = temp_file("saved");
        let a = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
        let json = format!(
            r#"{{"version":7,"later":true,"accounts":{{"Alex":[{{"uri":"{a}","liked":true}},{{"uri":"spotify:episode:x","liked":true}},{{"uri":"{a}","liked":false}}],"sam":[]}}}}"#
        );
        std::fs::write(&path, json).unwrap();
        let queue = LikeQueue::load(&path);
        assert_eq!(queue.waiting("alex"), LikesWaiting { likes: 0, unlikes: 1 });
        assert_eq!(queue.pending("alex", a), Some(false));
        assert!(queue.report("alex").starts_with("Likes waiting for alex: 1 (0 likes, 1 unlikes)\nOther accounts: 0\nSender: idle\n"));
    }

    #[test]
    fn a_file_that_cant_be_opened_is_read_before_it_is_saved_over() {
        let path = temp_file("held");
        // A folder in its place can't be read as a file (as a file held by another program).
        std::fs::create_dir_all(&path).unwrap();
        let queue = LikeQueue::load(&path);
        queue.push("alex", &songs(1), true);
        assert!(path.is_dir(), "not written over");
        assert_eq!(queue.waiting("alex").likes, 1, "the like still waits in memory");
    }
}
