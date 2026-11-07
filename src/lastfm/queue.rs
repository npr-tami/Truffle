//! Scrobbles Last.fm hasn't accepted yet, oldest first, persisted in `lastfm_queue.json`.
//! Every change is written right away with the atomic
//! `config::write_json`, so a crash or power cut never loses a play.
//!
//! Each scrobble remembers the Last.fm account it was recorded for, so plays are never sent to
//! another account that connects later unless the user says so.

use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::tracker::PendingScrobble;
use crate::config;

/// Last.fm ignores scrobbles older than about two weeks (code 3); keep a margin of an hour.
pub const MAX_AGE_SECS: i64 = 14 * 24 * 3600 - 3600;
/// Beyond this many unsent scrobbles the oldest are dropped (they'd be too old by then anyway).
pub const MAX_ITEMS: usize = 10_000;
const FILE_VERSION: u32 = 1;
/// Reading the file is retried this often when Windows reports an error other than "not found".
const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Deserialize)]
struct QueueFile {
    #[serde(default)]
    items: Vec<PendingScrobble>,
}

#[derive(Serialize)]
struct QueueFileRef<'a> {
    version: u32,
    items: &'a [PendingScrobble],
}

#[derive(Debug, Default)]
pub struct ScrobbleQueue {
    /// Sorted by timestamp, oldest first.
    items: Vec<PendingScrobble>,
    /// The file couldn't be read at load and couldn't be moved aside either, so it may still hold
    /// unsent plays: before anything replaces it, it is read again (and merged) or moved aside.
    unread: bool,
    /// What became of that file, for Settings (see [`take_note`](Self::take_note)).
    note: Option<String>,
}

/// Whether a play recorded for `owner` belongs to `user` (Last.fm user names ignore case). An
/// unknown owner belongs to nobody.
pub fn same_user(owner: &str, user: &str) -> bool {
    !owner.is_empty() && owner.eq_ignore_ascii_case(user)
}

impl ScrobbleQueue {
    /// Read the saved queue. A file that can't be read is set aside, and the returned note says
    /// so. It is never overwritten: if it can't be moved either, [`save`](Self::save) reads it
    /// again (or moves it) before writing.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        let mut attempt = 1;
        let bytes = loop {
            match std::fs::read(path) {
                Ok(bytes) => break bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
                // An antivirus scan or backup tool can hold the file for a moment.
                Err(e) if attempt < READ_ATTEMPTS => {
                    log::warn!("could not read {} ({e}); retrying", path.display());
                    attempt += 1;
                    std::thread::sleep(READ_RETRY_DELAY);
                }
                Err(e) => {
                    log::warn!("could not read {}: {e}", path.display());
                    return Self::set_aside(
                        path,
                        "Truffle couldn't read its list of unsent scrobbles. New plays are still scrobbled.",
                    );
                }
            }
        };
        match serde_json::from_slice::<QueueFile>(&bytes) {
            Ok(file) => {
                let mut queue = Self::default();
                for item in file.items {
                    queue.push(item);
                }
                (queue, None)
            }
            Err(e) => {
                log::warn!("{} can't be read ({e}); setting it aside", path.display());
                Self::set_aside(path, "Truffle's list of unsent scrobbles was damaged, so those scrobbles can't be sent.")
            }
        }
    }

    /// An empty queue in place of a file that can't be used. The file is moved aside if possible,
    /// so saving new scrobbles can't replace it; if not, [`save`](Self::save) deals with it first.
    fn set_aside(path: &Path, lead: &str) -> (Self, Option<String>) {
        let mut queue = Self::default();
        let mut note = lead.to_owned();
        match config::quarantine(path) {
            Some(kept) => {
                if let Some(name) = kept.file_name() {
                    note.push_str(&format!(" The file was kept as {}.", name.to_string_lossy()));
                }
            }
            None => {
                // Probably still held by another program.
                queue.unread = true;
                note.push_str(" The file is kept, and Truffle tries to read it again before saving new plays.");
            }
        }
        (queue, Some(note))
    }

    /// Write the queue. A file that couldn't be read at load is read again first and its plays
    /// merged in (or it is moved aside); while neither works, nothing is written, so it is never
    /// replaced.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        if self.unread {
            self.recover(path)?;
        }
        config::write_json(path, &QueueFileRef { version: FILE_VERSION, items: &self.items })
    }

    fn recover(&mut self, path: &Path) -> Result<()> {
        let parsed = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<QueueFile>(&bytes).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("{} is gone; nothing to read again", path.display());
                self.unread = false;
                return Ok(());
            }
            Err(e) => Err(e.to_string()),
        };
        match parsed {
            Ok(file) => {
                let found = file.items.len();
                for item in file.items {
                    self.push(item);
                }
                log::info!("read {} after all: {found} unsent scrobble(s) kept", path.display());
                self.note = Some(format!(
                    "Truffle could read its list of unsent scrobbles after all, so {found} {} kept.",
                    if found == 1 { "play is" } else { "plays are" }
                ));
            }
            Err(e) => {
                let Some(kept) = config::quarantine(path) else {
                    bail!("{} still can't be read ({e}) or set aside, so it isn't replaced yet", path.display());
                };
                let name = kept.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                log::warn!("{} still can't be read ({e}); set aside as {name}", path.display());
                self.note = Some(format!(
                    "Truffle couldn't read its earlier list of unsent scrobbles, so those scrobbles can't be sent. \
                     The file was kept as {name}."
                ));
            }
        }
        self.unread = false;
        Ok(())
    }

    /// A note for Settings about the file that couldn't be read at load, once it was dealt with.
    pub fn take_note(&mut self) -> Option<String> {
        self.note.take()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Add a scrobble in timestamp order. Returns false for a duplicate of a queued play.
    pub fn push(&mut self, item: PendingScrobble) -> bool {
        let same_second = self.same_second(item.timestamp);
        if self.items[same_second.clone()].iter().any(|q| q.same_play(&item)) {
            return false;
        }
        self.items.insert(same_second.end, item);
        if self.items.len() > MAX_ITEMS {
            let excess = self.items.len() - MAX_ITEMS;
            log::warn!("dropping the {excess} oldest unsent scrobble(s): more than {MAX_ITEMS} are waiting");
            self.items.drain(..excess);
        }
        true
    }

    /// Drop scrobbles Last.fm would no longer take. Returns how many.
    pub fn prune(&mut self, now_unix: i64) -> usize {
        let before = self.items.len();
        self.items.retain(|q| now_unix.saturating_sub(q.timestamp) <= MAX_AGE_SECS);
        before - self.items.len()
    }

    /// The oldest `n` scrobbles.
    #[cfg(test)]
    pub fn oldest(&self, n: usize) -> Vec<PendingScrobble> {
        self.items.iter().take(n).cloned().collect()
    }

    /// The oldest `n` scrobbles recorded for `user` (copies: the queue may change while they are
    /// being sent).
    pub fn oldest_for(&self, user: &str, n: usize) -> Vec<PendingScrobble> {
        self.items.iter().filter(|q| same_user(&q.user, user)).take(n).cloned().collect()
    }

    /// The scrobbles not recorded for `user` (all of them for `None`): how many, and the account
    /// they were recorded for when that is one known account.
    pub fn others(&self, user: Option<&str>) -> (usize, Option<String>) {
        let mut others = self.items.iter().filter(|q| user.is_none_or(|u| !same_user(&q.user, u)));
        let Some(first) = others.next() else { return (0, None) };
        let mut count = 1;
        let mut one_account = !first.user.is_empty();
        for q in others {
            count += 1;
            one_account &= same_user(&first.user, &q.user);
        }
        (count, one_account.then(|| first.user.clone()))
    }

    /// Record every scrobble that isn't `user`'s as `user`'s. Returns how many changed.
    pub fn adopt(&mut self, user: &str) -> usize {
        self.relabel(user, |owner| !same_user(owner, user))
    }

    /// Record the scrobbles of an unknown account (queued by an older Truffle) as `user`'s.
    pub fn adopt_unknown(&mut self, user: &str) -> usize {
        self.relabel(user, str::is_empty)
    }

    fn relabel(&mut self, user: &str, change: impl Fn(&str) -> bool) -> usize {
        let mut changed = 0;
        for q in self.items.iter_mut().filter(|q| change(&q.user)) {
            q.user = user.to_owned();
            changed += 1;
        }
        changed
    }

    /// Delete every scrobble except those recorded for `keep_for` (all of them for `None`).
    /// Returns how many.
    pub fn discard_except(&mut self, keep_for: Option<&str>) -> usize {
        let before = self.items.len();
        self.items.retain(|q| keep_for.is_some_and(|u| same_user(&q.user, u)));
        before - self.items.len()
    }

    /// Remove the queued copy of `item` (if it is still there).
    pub fn remove(&mut self, item: &PendingScrobble) -> bool {
        match self.position(item) {
            Some(i) => {
                self.items.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn get_mut(&mut self, item: &PendingScrobble) -> Option<&mut PendingScrobble> {
        self.position(item).and_then(|i| self.items.get_mut(i))
    }

    /// Index range of the items with this timestamp (the queue is sorted by timestamp).
    fn same_second(&self, timestamp: i64) -> std::ops::Range<usize> {
        let start = self.items.partition_point(|q| q.timestamp < timestamp);
        let end = self.items.partition_point(|q| q.timestamp <= timestamp);
        start..end
    }

    fn position(&self, item: &PendingScrobble) -> Option<usize> {
        let range = self.same_second(item.timestamp);
        let start = range.start;
        self.items[range].iter().position(|q| q.same_play(item)).map(|i| start + i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lastfm::tracker::ScrobbleTrack;

    fn item(track: &str, timestamp: i64) -> PendingScrobble {
        PendingScrobble {
            track: ScrobbleTrack {
                artist: "Artist".into(),
                track: track.into(),
                album: String::new(),
                duration_s: 200,
                uri: String::new(),
            },
            timestamp,
            attempts: 0,
            user: String::new(),
        }
    }

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("truffle-lastfm-queue-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("lastfm_queue.json")
    }

    #[test]
    fn keeps_timestamp_order_and_skips_duplicates() {
        let mut q = ScrobbleQueue::default();
        assert!(q.push(item("b", 200)));
        assert!(q.push(item("a", 100)));
        assert!(q.push(item("c", 300)));
        assert!(!q.push(item("a", 100)), "same play twice");
        assert!(q.push(item("a2", 100)), "another track at the same second is fine");
        let order: Vec<String> = q.oldest(10).into_iter().map(|i| i.track.track).collect();
        assert_eq!(order, ["a", "a2", "b", "c"]);
        assert_eq!(q.oldest(2).len(), 2);
        assert!(q.remove(&item("b", 200)));
        assert!(!q.remove(&item("b", 200)));
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn drops_old_entries_and_caps_the_size() {
        let now = 1_760_000_000;
        let mut q = ScrobbleQueue::default();
        q.push(item("too old", now - MAX_AGE_SECS - 1));
        q.push(item("just in time", now - MAX_AGE_SECS));
        q.push(item("fresh", now - 60));
        assert_eq!(q.prune(now), 1);
        assert_eq!(q.oldest(1)[0].track.track, "just in time");

        let mut q = ScrobbleQueue::default();
        for i in 0..(MAX_ITEMS as i64 + 5) {
            q.push(item("t", i));
        }
        assert_eq!(q.len(), MAX_ITEMS);
        assert_eq!(q.oldest(1)[0].timestamp, 5, "the oldest went first");
    }

    #[test]
    fn saves_and_loads() {
        let path = temp_file("roundtrip");
        let mut q = ScrobbleQueue::default();
        q.push(item("one", 100));
        let mut two = item("two", 50);
        two.attempts = 2;
        q.push(two.clone());
        q.save(&path).unwrap();
        let (loaded, note) = ScrobbleQueue::load(&path);
        assert!(note.is_none());
        assert_eq!(loaded.oldest(5), vec![two, item("one", 100)]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\":1"), "{text}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_file_is_an_empty_queue_and_a_damaged_one_is_kept_aside() {
        let path = temp_file("damaged");
        let (q, note) = ScrobbleQueue::load(&path);
        assert!(q.is_empty() && note.is_none());

        std::fs::write(&path, [0u8; 32]).unwrap();
        let (q, note) = ScrobbleQueue::load(&path);
        assert!(q.is_empty());
        assert!(note.is_some_and(|n| n.contains("lastfm_queue.json.corrupt-")));
        assert!(!path.exists(), "moved aside, not overwritten");
        let mut q = q;
        q.push(item("new", 300));
        q.save(&path).unwrap();
        assert!(q.take_note().is_none(), "nothing left to recover");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn for_user(track: &str, timestamp: i64, user: &str) -> PendingScrobble {
        PendingScrobble { user: user.into(), ..item(track, timestamp) }
    }

    #[test]
    fn scrobbles_belong_to_the_account_they_were_recorded_for() {
        let mut q = ScrobbleQueue::default();
        q.push(for_user("a1", 100, "Alice"));
        q.push(for_user("b1", 150, "bob"));
        q.push(for_user("a2", 200, "alice"));
        q.push(item("unknown", 250));
        let names = |v: Vec<PendingScrobble>| v.into_iter().map(|i| i.track.track).collect::<Vec<_>>();
        assert_eq!(names(q.oldest_for("ALICE", 10)), ["a1", "a2"], "user names ignore case");
        assert_eq!(names(q.oldest_for("alice", 1)), ["a1"]);
        assert!(q.oldest_for("", 10).is_empty(), "an unknown account is nobody's");
        assert_eq!(q.others(Some("alice")), (2, None), "bob's and an unknown one");
        assert_eq!(q.others(Some("bob")), (3, None));
        assert_eq!(q.others(None), (4, None));

        assert_eq!(q.adopt_unknown("bob"), 1);
        assert_eq!(q.others(Some("alice")), (2, Some("bob".to_owned())));
        assert_eq!(q.adopt("carol"), 4);
        assert_eq!(q.others(Some("carol")), (0, None));
        assert_eq!(q.others(None), (4, Some("carol".to_owned())));

        q.push(for_user("d1", 300, "dave"));
        assert_eq!(q.discard_except(Some("Dave")), 4);
        assert_eq!(names(q.oldest(10)), ["d1"]);
        assert_eq!(q.discard_except(None), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn the_account_is_saved_and_older_files_load() {
        let path = temp_file("accounts");
        let mut q = ScrobbleQueue::default();
        q.push(for_user("mine", 100, "alice"));
        q.push(item("old format", 50));
        q.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"user\"").count(), 1, "an unknown account isn't written: {text}");
        let (loaded, _) = ScrobbleQueue::load(&path);
        assert_eq!(loaded.oldest(5), vec![item("old format", 50), for_user("mine", 100, "alice")]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Another program holds the file so that it can't be read (or moved) at load: the next save
    /// must not replace it, and once it is free its plays are merged in.
    #[cfg(windows)]
    #[test]
    fn a_locked_file_is_read_again_before_it_is_replaced() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = temp_file("locked");
        let mut saved = ScrobbleQueue::default();
        saved.push(item("earlier one", 100));
        saved.push(item("earlier two", 200));
        saved.save(&path).unwrap();

        // No sharing at all: reading and renaming both fail.
        let lock = std::fs::File::options().read(true).share_mode(0).open(&path).unwrap();
        let (mut q, note) = ScrobbleQueue::load(&path);
        assert!(q.is_empty());
        assert!(note.is_some_and(|n| n.contains("read it again")));
        q.push(item("new", 300));
        assert!(q.save(&path).is_err(), "not written while the old file can't be read");
        assert!(q.take_note().is_none());
        drop(lock);

        q.save(&path).unwrap();
        assert!(q.take_note().is_some_and(|n| n.contains("after all") && n.contains("2 plays")));
        let (loaded, note) = ScrobbleQueue::load(&path);
        assert!(note.is_none());
        let names: Vec<String> = loaded.oldest(5).into_iter().map(|i| i.track.track).collect();
        assert_eq!(names, ["earlier one", "earlier two", "new"]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A damaged file that can't be moved aside (another program allows reading only) is set
    /// aside before the first save that works, never replaced.
    #[cfg(windows)]
    #[test]
    fn a_damaged_file_that_cant_be_moved_is_not_replaced() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;

        let path = temp_file("damaged-locked");
        std::fs::write(&path, b"{ not json").unwrap();
        let lock = std::fs::File::options().read(true).share_mode(FILE_SHARE_READ).open(&path).unwrap();
        let (mut q, note) = ScrobbleQueue::load(&path);
        assert!(q.is_empty() && note.is_some_and(|n| n.contains("damaged")));
        q.push(item("new", 300));
        assert!(q.save(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json", "still the old file");
        drop(lock);

        q.save(&path).unwrap();
        assert!(q.take_note().is_some_and(|n| n.contains("lastfm_queue.json.corrupt-")));
        assert_eq!(ScrobbleQueue::load(&path).0.len(), 1);
        let kept = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("lastfm_queue.json.corrupt-"));
        assert!(kept, "the damaged file was kept");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
