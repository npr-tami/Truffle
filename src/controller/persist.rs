//! Files owned by the controller: `state.json` (play history + queue snapshot), a small
//! liked-songs snapshot so the heart icons appear instantly on the next start, and the
//! listen-along links (`listen_links.json`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{self, Paths};
use crate::listen::links::LinkBook;
use crate::queue::{PersistedState, PlayHistory, SavedQueue};

/// Reading `state.json` is retried this often when Windows reports an error other than "not
/// found" (an antivirus scan or backup tool can hold the file for a moment).
const READ_ATTEMPTS: u32 = 3;
const READ_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The state read at startup, and what to tell the user if the file had to be recovered.
#[derive(Default)]
pub(super) struct LoadedState {
    pub state: PersistedState,
    pub notice: Option<String>,
}

/// Loads `state.json`. A file that can't be parsed (for example left full of zero bytes by a
/// power cut) is never silently replaced: it is set aside as `state.json.corrupt-<time>`, and the
/// copy saved at the last successful start (`state.json.bak`) is used instead, if there is one.
pub(super) fn load_state(paths: &Paths) -> LoadedState {
    let path = paths.state_file();
    let backup = backup_file(&path);
    match read_state(&path) {
        ReadState::Parsed(state, bytes) => {
            // A copy known to be good, for the case that a later write is lost.
            if let Err(e) = config::write_atomic(&backup, &bytes) {
                log::warn!("could not back up the play history: {e:#}");
            }
            LoadedState { state: *state, notice: None }
        }
        ReadState::Missing => LoadedState::default(),
        ReadState::Corrupt(why) => {
            log::error!("{} can't be read ({why})", path.display());
            let kept = config::quarantine(&path);
            recover(&backup, kept.as_deref())
        }
        ReadState::Failed(why) => {
            log::error!("{} can't be opened ({why})", path.display());
            recover(&backup, None)
        }
    }
}

fn recover(backup: &Path, kept: Option<&Path>) -> LoadedState {
    let (state, mut notice) = match read_state(backup) {
        ReadState::Parsed(state, _) => {
            log::warn!("using the play history backup {}", backup.display());
            (
                *state,
                "Truffle's saved play history couldn't be read, so the copy from the last start was used. \
                 Plays and blocked songs from the last session may be missing."
                    .to_owned(),
            )
        }
        _ => (
            PersistedState::default(),
            "Truffle's saved play history couldn't be read and there was no usable backup, so play counts \
             and blocked songs start over."
                .to_owned(),
        ),
    };
    if let Some(name) = kept.and_then(Path::file_name) {
        notice.push_str(&format!(" The damaged file was kept as {}.", name.to_string_lossy()));
    }
    LoadedState { state, notice: Some(notice) }
}

enum ReadState {
    Missing,
    Parsed(Box<PersistedState>, Vec<u8>),
    /// Read, but not valid state JSON.
    Corrupt(String),
    /// Could not be read at all.
    Failed(String),
}

fn read_state(path: &Path) -> ReadState {
    let mut attempt = 1;
    let bytes = loop {
        match std::fs::read(path) {
            Ok(bytes) => break bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReadState::Missing,
            Err(e) if attempt < READ_ATTEMPTS => {
                log::warn!("could not read {} ({e}); retrying", path.display());
                attempt += 1;
                std::thread::sleep(READ_RETRY_DELAY);
            }
            Err(e) => return ReadState::Failed(e.to_string()),
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => ReadState::Parsed(Box::new(state), bytes),
        Err(e) => ReadState::Corrupt(e.to_string()),
    }
}

fn backup_file(state_file: &Path) -> PathBuf {
    state_file.with_extension("json.bak")
}

/// Borrowing mirror of `PersistedState` (identical field names, so identical JSON). Saving
/// through it avoids cloning a possibly large history every 30 s.
#[derive(Serialize)]
struct StateRef<'a> {
    history: &'a PlayHistory,
    queue: Option<&'a SavedQueue>,
}

/// Compile-time guard: if `PersistedState` gains a field, this exhaustive pattern stops compiling,
/// which is the reminder to add the field to `StateRef` as well.
const _: fn(PersistedState) = |PersistedState { history: _, queue: _ }| ();

pub(super) fn save_state(paths: &Paths, history: &PlayHistory, queue: Option<&SavedQueue>) -> Result<()> {
    config::write_json(&paths.state_file(), &StateRef { history, queue })
}

/// Last known liked-song URIs of one user, so hearts show before the fresh list arrives.
/// (The playlist list has its own cache in the library module.)
#[derive(Deserialize, Default)]
#[serde(default)]
struct LikedSnapshot {
    username: String,
    liked: Vec<String>,
}

#[derive(Serialize)]
struct LikedSnapshotRef<'a> {
    username: &'a str,
    liked: Vec<&'a str>,
}

fn liked_file(paths: &Paths) -> PathBuf {
    paths.cache.join("liked_snapshot.json")
}

/// The saved liked URIs, if they belong to `username`.
pub(super) fn load_liked(paths: &Paths, username: &str) -> Option<HashSet<String>> {
    let snapshot: LikedSnapshot = config::read_json(&liked_file(paths))?;
    snapshot
        .username
        .eq_ignore_ascii_case(username)
        .then(|| snapshot.liked.into_iter().collect())
}

pub(super) fn save_liked(paths: &Paths, username: &str, liked: &HashSet<String>) -> Result<()> {
    let liked = liked.iter().map(String::as_str).collect();
    config::write_json(&liked_file(paths), &LikedSnapshotRef { username, liked })
}

/// What to tell the user about `listen_links.json` (each only when it happens).
pub(super) const LINKS_FROM_BACKUP: &str = "Your listen-along links came from a backup copy.";
pub(super) const LINKS_LOST: &str = "Couldn't read your listen-along links.";
pub(super) const LINKS_LOCKED: &str = "Couldn't open your listen-along links. Restart Truffle to try again.";

/// `listen_links.json` as read.
#[derive(Debug, Default, PartialEq)]
pub(super) struct LoadedLinks {
    pub book: LinkBook,
    /// What to tell the user.
    pub notice: Option<&'static str>,
    /// The file (or, with the file gone or damaged, its copy) is there but couldn't be opened
    /// (another program may hold it for a moment), and nothing better could be used: neither is
    /// saved over while so (they are read again at login, and the user is told if they still
    /// can't be).
    pub held: bool,
}

enum ReadLinks {
    Missing,
    Parsed(LinkBook),
    /// Read, but not a book.
    Corrupt(String),
    /// There, but couldn't be opened.
    Failed,
}

fn read_links(path: &Path) -> ReadLinks {
    if !path.exists() {
        return ReadLinks::Missing;
    }
    let Some(bytes) = config::read_with_retry(path) else {
        return if path.exists() { ReadLinks::Failed } else { ReadLinks::Missing };
    };
    match serde_json::from_slice::<LinkBook>(&bytes) {
        Ok(mut book) => {
            if book.dedupe() {
                log::warn!("listen-along links: put right a book with the same link or id twice");
            }
            ReadLinks::Parsed(book)
        }
        Err(e) => ReadLinks::Corrupt(e.to_string()),
    }
}

/// Loads `listen_links.json`. No file (and no copy) is a fresh start. Every save also writes a
/// copy (`listen_links.json.bak`), used when the file can't be read: a damaged file is set aside
/// (`listen_links.json.corrupt-<time>`) and the copy saved as the file again at once; one that
/// can't be opened is left alone. A file gone with its copy still there (set aside at a start
/// that couldn't save the copy back) is the copy too. With no usable copy either the book starts
/// empty: links not in the book are never written, so nobody kicked gets back in by it; the
/// friends' links just go quiet.
pub(super) fn load_links(paths: &Paths) -> LoadedLinks {
    let path = paths.listen_links_file();
    let backup = backup_file(&path);
    match read_links(&path) {
        ReadLinks::Parsed(book) => LoadedLinks { book, ..LoadedLinks::default() },
        // A fresh start, or a file set aside as damaged at a start that saved nothing after: then
        // the copy is all that is left, and an empty book saved now would write over it too.
        ReadLinks::Missing => match read_links(&backup) {
            ReadLinks::Parsed(book) => restored(paths, &backup, book),
            ReadLinks::Failed => held(&backup),
            ReadLinks::Missing | ReadLinks::Corrupt(_) => LoadedLinks::default(),
        },
        ReadLinks::Corrupt(why) => {
            log::error!("{} can't be read ({why})", path.display());
            // Set aside if it can be, so the next save doesn't write over it.
            let kept = config::quarantine(&path).is_some();
            match read_links(&backup) {
                ReadLinks::Parsed(book) if kept => restored(paths, &backup, book),
                ReadLinks::Parsed(book) => {
                    log::warn!("using the listen-along links' copy {}", backup.display());
                    LoadedLinks { book, notice: Some(LINKS_FROM_BACKUP), held: false }
                }
                ReadLinks::Failed => held(&backup),
                ReadLinks::Missing | ReadLinks::Corrupt(_) => LoadedLinks { notice: Some(LINKS_LOST), ..LoadedLinks::default() },
            }
        }
        ReadLinks::Failed => {
            // Likely fine, only held by another program for now (an antivirus scan, a backup).
            log::error!("{} can't be opened", path.display());
            match read_links(&backup) {
                ReadLinks::Parsed(book) => {
                    log::warn!("using the listen-along links' copy {}", backup.display());
                    LoadedLinks { book, ..LoadedLinks::default() }
                }
                _ => LoadedLinks { held: true, ..LoadedLinks::default() },
            }
        }
    }
}

/// The copy's book, with the file gone: saved as the file again at once, because a start that
/// changes no link saves nothing, and the next start would find no file.
fn restored(paths: &Paths, backup: &Path, book: LinkBook) -> LoadedLinks {
    log::warn!("using the listen-along links' copy {}", backup.display());
    if let Err(e) = save_links(paths, &book) {
        log::warn!("could not save the listen-along links' copy as the file again: {e:#}");
    }
    LoadedLinks { book, notice: Some(LINKS_FROM_BACKUP), held: false }
}

/// The file gone or damaged, and its copy there but held: an empty book that is never saved over
/// the copy (the user hears of it at login, if it still can't be opened then).
fn held(backup: &Path) -> LoadedLinks {
    log::error!("{} can't be opened", backup.display());
    LoadedLinks { held: true, ..LoadedLinks::default() }
}

/// Saves the book, then the same again as its copy (one after the other, so a power cut leaves
/// one of them whole).
pub(super) fn save_links(paths: &Paths, book: &LinkBook) -> Result<()> {
    let path = paths.listen_links_file();
    let bytes = serde_json::to_vec(book)?;
    config::write_atomic(&path, &bytes)?;
    if let Err(e) = config::write_atomic(&backup_file(&path), &bytes) {
        log::warn!("could not copy the listen-along links: {e:#}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!("truffle-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        std::fs::create_dir_all(&paths.config).unwrap();
        paths
    }

    fn history_with_block(uri: &str) -> PlayHistory {
        let mut history = PlayHistory::default();
        history.blocked.insert(uri.to_owned());
        history.play_counts.insert(uri.to_owned(), 4);
        history
    }

    #[test]
    fn a_damaged_state_file_is_kept_and_the_backup_used() {
        let paths = temp_paths("damaged");
        save_state(&paths, &history_with_block("spotify:track:a"), None).unwrap();
        // A normal start reads it and backs it up.
        let loaded = load_state(&paths);
        assert!(loaded.notice.is_none());
        assert!(loaded.state.history.blocked.contains("spotify:track:a"));
        assert!(backup_file(&paths.state_file()).exists());

        // The next save is lost to a power cut: the file is all zero bytes.
        std::fs::write(paths.state_file(), [0u8; 128]).unwrap();
        let loaded = load_state(&paths);
        assert!(loaded.state.history.blocked.contains("spotify:track:a"), "restored from the backup");
        let notice = loaded.notice.expect("the user is told");
        assert!(notice.contains("state.json.corrupt-"), "{notice}");
        assert!(!paths.state_file().exists(), "the damaged file was moved, not overwritten");
        let kept = std::fs::read_dir(&paths.config)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("state.json.corrupt-"));
        assert!(kept);
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    fn corrupt_kept(paths: &Paths) -> bool {
        std::fs::read_dir(&paths.config).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("listen_links.json.corrupt-"))
    }

    fn sam(kicked: bool) -> LinkBook {
        let mut book = LinkBook::default();
        let id = book.reserve_id();
        book.add(id, "Sam", "spotify:playlist:0000000000000000000001", "alex", 5);
        book.kick(id, kicked, "alex");
        book
    }

    #[test]
    fn links_read_back_and_a_damaged_file_is_kept_aside_and_said() {
        let paths = temp_paths("links");
        // No file: a fresh start, nothing to say.
        assert_eq!(load_links(&paths), LoadedLinks::default());
        let book = sam(true);
        save_links(&paths, &book).unwrap();
        assert!(backup_file(&paths.listen_links_file()).exists(), "every save writes the copy too");
        assert_eq!(load_links(&paths), LoadedLinks { book: book.clone(), ..LoadedLinks::default() });
        // Damaged: set aside, and the copy used (the kick kept); the user is told.
        std::fs::write(paths.listen_links_file(), b"{\"links\": [").unwrap();
        assert_eq!(load_links(&paths), LoadedLinks { book: book.clone(), notice: Some(LINKS_FROM_BACKUP), held: false });
        assert!(corrupt_kept(&paths), "moved, not written over");
        // The copy was saved as the file again: the next start reads it, with nothing to say,
        // even if nothing was saved in between.
        assert_eq!(load_links(&paths), LoadedLinks { book: book.clone(), ..LoadedLinks::default() });
        // A file gone with its copy there (set aside at a start that couldn't save it back): the
        // copy, never an empty book that a save would write over both with.
        std::fs::remove_file(paths.listen_links_file()).unwrap();
        assert_eq!(load_links(&paths), LoadedLinks { book: book.clone(), notice: Some(LINKS_FROM_BACKUP), held: false });
        assert_eq!(load_links(&paths), LoadedLinks { book: book.clone(), ..LoadedLinks::default() }, "the file is back");
        // Damaged with no usable copy: the book starts empty, and the user is told.
        std::fs::write(paths.listen_links_file(), b"{\"links\": [").unwrap();
        std::fs::write(backup_file(&paths.listen_links_file()), [0u8; 64]).unwrap();
        assert_eq!(load_links(&paths), LoadedLinks { book: LinkBook::default(), notice: Some(LINKS_LOST), held: false });
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    /// The file held open by another program that lets nobody read it meanwhile (as an antivirus
    /// scan can, for a moment).
    #[cfg(windows)]
    fn hold_open(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().read(true).share_mode(0).open(path).unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn a_links_file_that_cant_be_opened_is_never_set_aside_or_lost() {
        let paths = temp_paths("links-held");
        let path = paths.listen_links_file();
        let book = sam(true);
        save_links(&paths, &book).unwrap();
        // Held: not set aside; the copy is used (it is the same as the file), nothing to say.
        let held = hold_open(&path);
        assert_eq!(load_links(&paths), LoadedLinks { book: book.clone(), ..LoadedLinks::default() });
        assert!(path.exists() && !corrupt_kept(&paths));
        // Held, and no copy: an empty book, held so it is never saved over the file.
        std::fs::remove_file(backup_file(&path)).unwrap();
        assert_eq!(load_links(&paths), LoadedLinks { book: LinkBook::default(), notice: None, held: true });
        drop(held);
        // Let go of: the file is whole, with its kick.
        assert_eq!(load_links(&paths).book, book);
        // The file gone or damaged, and its copy held: held too, so the copy isn't saved over.
        save_links(&paths, &book).unwrap();
        std::fs::remove_file(&path).unwrap();
        let held = hold_open(&backup_file(&path));
        assert_eq!(load_links(&paths), LoadedLinks { book: LinkBook::default(), notice: None, held: true });
        std::fs::write(&path, b"{\"links\": [").unwrap();
        assert_eq!(load_links(&paths), LoadedLinks { book: LinkBook::default(), notice: None, held: true });
        drop(held);
        assert_eq!(load_links(&paths), LoadedLinks { book, notice: Some(LINKS_FROM_BACKUP), held: false });
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn a_damaged_state_file_without_backup_starts_over_and_says_so() {
        let paths = temp_paths("nobackup");
        std::fs::write(paths.state_file(), b"{\"history\": {\"play_counts\": [").unwrap();
        let loaded = load_state(&paths);
        assert!(loaded.state.history.play_counts.is_empty());
        assert!(loaded.notice.is_some_and(|n| n.contains("start over")));
        // No file at all is a normal first start.
        let fresh = temp_paths("fresh");
        let loaded = load_state(&fresh);
        assert!(loaded.notice.is_none());
        let _ = std::fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = std::fs::remove_dir_all(fresh.config.parent().unwrap());
    }
}
