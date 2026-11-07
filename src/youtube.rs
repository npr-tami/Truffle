//! YouTube as a sound source: the song Spotify names, found on YouTube and fetched as AAC audio
//! (m4a, format 140) for the engine to play. The library, likes and search stay on Spotify.
//!
//! Finding and fetching go through yt-dlp (a separate program: beside truffle.exe, in Truffle's
//! data folder, on PATH, or in the user's Downloads), which needs a JavaScript runtime (Node, Deno,
//! Bun or QuickJS) to read YouTube's player. Matching: a YouTube search for "artists - title",
//! scored by length (within a few seconds of Spotify's), title and artist words, official
//! "- Topic" channels, and against live / cover / sped up / nightcore versions the Spotify title
//! doesn't have. A match is remembered per Spotify song (and a user's own pick wins), in memory
//! and in `youtube-matches.json`.
//!
//! yt-dlp only finds the video and the address of its audio file; Truffle downloads the file
//! itself in pieces, which is several times faster than letting yt-dlp do it. The address comes
//! from YouTube's visionOS client first: it gets no ad, so the address works at once.
//!
//! To start songs sooner: songs' audio is kept on disk (`<cache>\youtube`, up to the audio cache
//! size), the next few songs of the queue are fetched ahead, and matches for the open list's
//! songs are searched for in the background, gently.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::sync::Notify;

use crate::config;
use crate::model::{Helpers, TrackInfo};

/// The programs YouTube mode needs.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ytdlp: PathBuf,
    /// yt-dlp's `--js-runtimes` name ("node", "deno", "bun", "quickjs") and the program.
    pub js: Option<(String, PathBuf)>,
}

/// A YouTube video that may be the song.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub duration_ms: u32,
    pub score: f32,
}

/// A song's audio, ready to decode.
#[derive(Debug, Clone)]
pub struct Audio {
    pub video_id: String,
    pub title: String,
    /// The m4a file (AAC), whole.
    pub bytes: Vec<u8>,
    /// From YouTube's details, when known.
    pub duration_ms: Option<u32>,
}

const NO_YTDLP: &str = "YouTube mode needs yt-dlp. Put yt-dlp.exe beside truffle.exe.";
const NO_JS: &str = "yt-dlp needs Node.js or Deno to read YouTube.";
const NOT_FOUND: &str = "Couldn't find this song on YouTube.";
const SEARCH_FAILED: &str = "Couldn't search YouTube.";
const GET_FAILED: &str = "Couldn't get this song from YouTube.";
const TOO_SLOW: &str = "YouTube took too long to answer.";
const OFFLINE: &str = "Can't reach YouTube. Check the internet connection.";
const REFUSED: &str = "YouTube is refusing for now. Try again later.";
const NO_START: &str = "Couldn't start yt-dlp.";
/// Background work that was called off (never shown).
const CALLED_OFF: &str = "Not needed any more.";

/// Whether a problem is YouTube's as a whole (refusing, unreachable), not one song's: then no song
/// will play for now, so skipping songs would only use them up.
pub fn is_service_wide(problem: &str) -> bool {
    [REFUSED, OFFLINE, NO_START, SEARCH_FAILED, NO_YTDLP, NO_JS].contains(&problem)
}

/// Results asked for per search.
const SEARCH_RESULTS: u32 = 8;
/// A video further than this from Spotify's length is another cut (or another song).
const MAX_LENGTH_DIFF_MS: u32 = 12_000;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a helper may take to print its version (yt-dlp unpacks itself first).
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
/// YouTube slows down bigger requests, so the file comes in pieces of at most this size.
const CHUNK_BYTES: u64 = 9_000_000;
/// Songs' audio kept in memory, so a replay or a song fetched ahead starts at once.
const RECENT_AUDIO: usize = 6;
/// Searches kept in memory (for "Wrong song?" right after a song started).
const RECENT_SEARCHES: usize = 16;
/// How often a refused (403) address is tried again while it may still be about to work (see
/// [`Stream::available_at`]).
const POLL_EVERY: Duration = Duration::from_millis(250);
/// How long past its `available_at` a refused address is still tried before asking for a new one
/// (seconds), and the most tries (about 20 s).
const POLL_SLACK: f64 = 2.0;
const MAX_POLLS: u32 = 80;
/// How long a failed program lookup is trusted before looking again.
const TOOLS_RECHECK: Duration = Duration::from_secs(5);
const MATCH_FILE: &str = "youtube-matches.json";
/// The folder in the cache that keeps songs' audio files.
const AUDIO_DIR: &str = "youtube";
/// yt-dlp's format choice: AAC in m4a (140), else another plain AAC m4a (not HE-AAC, 139, which
/// the decoder can't play properly).
const FORMAT: &str = "140/bestaudio[ext=m4a][acodec=mp4a.40.2]";
/// The quick way to the audio file: the visionOS client gets no ad (so the address works at once)
/// and needs no JavaScript.
const QUICK_RESOLVE: &str = "youtube:player_client=visionos;skip=hls,dash";
/// The quick way to search: without first fetching YouTube's web page.
const QUICK_SEARCH: &str = "youtubetab:skip=webpage";
/// Videos tried per song when the first ones turn out to be gone.
const MAX_TRIES: usize = 3;
/// What yt-dlp prints about the chosen audio file, as one line of JSON.
const STREAM_FIELDS: &str = "%(.{id,title,duration,url,filesize,available_at,http_headers})j";
const YOUTUBE: &str = "https://www.youtube.com";
/// A yt-dlp that has answered is left this long to finish by itself, then stopped.
const LINGER: Duration = Duration::from_secs(20);
/// How long stopping a yt-dlp cleanly may take (it may still be unpacking itself) before it is
/// killed outright.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// Background searches at once, and the least time between two of them starting.
const SEARCHES_AT_ONCE: usize = 2;
const SEARCH_GAP: Duration = Duration::from_secs(1);
/// Background searches for an open list stop after this many songs (the nearest ones): a whole
/// big list would mean a search a second for half an hour, and a refusal stops every song.
const SEARCH_AHEAD_MAX: usize = 200;
/// How often background searches look again while a song is being fetched.
const BUSY_RECHECK: Duration = Duration::from_millis(300);
/// Background searches and fetches stop this long after YouTube refuses, or can't be reached.
const REFUSED_PAUSE: Duration = Duration::from_secs(600);
const OFFLINE_PAUSE: Duration = Duration::from_secs(60);
/// Matches found in the background are saved this long after the first unsaved one.
const SAVE_DELAY: Duration = Duration::from_secs(5);
/// Windows: start yt-dlp without a console window; background runs at a lower priority.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

tokio::task_local! {
    /// Set for background work (fetching ahead, searching ahead): its yt-dlp runs at a lower
    /// priority, and none starts while the switch is off (that work was called off).
    static BACKGROUND: Arc<AtomicBool>;
}

/// Finds and fetches songs on YouTube. Cheap to clone.
#[derive(Clone)]
pub struct YouTube {
    inner: Arc<Inner>,
}

struct Inner {
    /// Where matches are remembered (`youtube-matches.json`).
    cache_dir: PathBuf,
    /// The folder truffle.exe is in (yt-dlp may sit beside it).
    exe_dir: Option<PathBuf>,
    /// The last program lookup, and when it ran.
    tools: Mutex<Option<(Instant, std::result::Result<Tools, String>)>>,
    /// Remembered matches, read from the file on first use.
    matches: Mutex<Option<Matches>>,
    /// Held while the match file is written, so the last save always has the latest matches.
    saving: Mutex<()>,
    /// Recent searches and their results, oldest first.
    searches: Mutex<VecDeque<(String, Vec<Entry>)>>,
    /// Recent songs' audio by Spotify URI, oldest first.
    recent: Mutex<VecDeque<(String, Audio)>>,
    /// One lock per song or search being worked on, so the same work isn't done twice at once.
    jobs: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// One HTTP client per kind of address downloads may use.
    http: Mutex<HashMap<Family, reqwest::Client>>,
    /// How many bytes of audio files may be kept on disk (0: none).
    disk_limit: AtomicU64,
    /// Half-written files from an earlier run are removed before the disk is first used.
    disk_cleaned: Once,
    /// Songs being fetched right now (background searches wait meanwhile), and those of them that
    /// are to play (fetching ahead waits for those).
    fetching: AtomicUsize,
    fetching_to_play: AtomicUsize,
    /// Background work waits until then: YouTube refused, or couldn't be reached.
    paused_until: Mutex<Option<Instant>>,
    /// A save of the matches is on its way.
    save_planned: AtomicBool,
    /// The songs to keep fetched ahead, and whether there are any (see [`BACKGROUND`]).
    ahead: Mutex<Ahead>,
    ahead_wake: Notify,
    ahead_on: Arc<AtomicBool>,
    /// The background searches for the open list, and whether there are any.
    plan: Mutex<SearchPlan>,
    plan_wake: Notify,
    search_on: Arc<AtomicBool>,
}

/// The songs to keep fetched ahead, and which of them were tried already.
#[derive(Default)]
struct Ahead {
    songs: Vec<TrackInfo>,
    tried: HashSet<String>,
    running: bool,
}

impl YouTube {
    pub fn new(cache_dir: PathBuf, exe_dir: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cache_dir,
                exe_dir,
                tools: Mutex::new(None),
                matches: Mutex::new(None),
                saving: Mutex::new(()),
                searches: Mutex::new(VecDeque::new()),
                recent: Mutex::new(VecDeque::new()),
                jobs: Mutex::new(HashMap::new()),
                http: Mutex::new(HashMap::new()),
                disk_limit: AtomicU64::new(0),
                disk_cleaned: Once::new(),
                fetching: AtomicUsize::new(0),
                fetching_to_play: AtomicUsize::new(0),
                paused_until: Mutex::new(None),
                save_planned: AtomicBool::new(false),
                ahead: Mutex::new(Ahead::default()),
                ahead_wake: Notify::new(),
                ahead_on: Arc::new(AtomicBool::new(false)),
                plan: Mutex::new(SearchPlan::default()),
                plan_wake: Notify::new(),
                search_on: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// How much room songs' audio files may take on disk, in MB (the audio cache size; 0: none).
    /// A smaller size applies at once, in the background.
    pub fn set_disk_cache_mb(&self, mb: u64) {
        let limit = mb.saturating_mul(1024 * 1024);
        if limit < self.inner.disk_limit.swap(limit, Ordering::Relaxed) {
            let me = self.clone();
            std::thread::spawn(move || me.disk().prune(limit));
        }
    }

    /// The programs YouTube mode needs, or what's missing in plain words.
    pub fn tools(&self) -> std::result::Result<Tools, String> {
        let mut cached = lock(&self.inner.tools);
        if let Some((at, result)) = &*cached {
            match result {
                Ok(tools) if tools.ytdlp.is_file() => return Ok(tools.clone()),
                Err(e) if at.elapsed() < TOOLS_RECHECK => return Err(e.clone()),
                _ => {}
            }
        }
        let places = Places::of_system(&self.inner.cache_dir, self.inner.exe_dir.as_deref());
        let found = find_tools(&places);
        let changed = match (cached.as_ref().map(|(_, r)| r), &found) {
            (Some(Ok(old)), Ok(new)) => old.ytdlp != new.ytdlp || old.js != new.js,
            (Some(Err(old)), Err(new)) => old != new,
            _ => true,
        };
        if changed {
            match &found {
                Ok(t) => log::info!(
                    "YouTube: yt-dlp at {}, JavaScript: {}",
                    t.ytdlp.display(),
                    t.js.as_ref().map_or("none".to_owned(), |(n, p)| format!("{n} at {}", p.display()))
                ),
                Err(e) => log::info!("YouTube: {e}"),
            }
        }
        *cached = Some((Instant::now(), found.clone()));
        found
    }

    /// The programs YouTube mode needs and their versions, for Settings > About: each found one is
    /// run once with `--version`. Nothing about where they are is kept or logged.
    pub async fn helpers(&self) -> Helpers {
        let places = Places::of_system(&self.inner.cache_dir, self.inner.exe_dir.as_deref());
        let ytdlp = find_ytdlp(&places);
        let js = find_js(&places, ytdlp.as_deref().and_then(Path::parent));
        let missing = match (&ytdlp, &js) {
            (None, _) => Some(NO_YTDLP.to_owned()),
            (Some(_), None) => Some(NO_JS.to_owned()),
            (Some(_), Some(_)) => None,
        };
        let ytdlp = match ytdlp {
            Some(p) => Some(version_of(&p).await),
            None => None,
        };
        let js = match js {
            Some((runtime, p)) => Some((runtime_name(&runtime).to_owned(), version_of(&p).await)),
            None => None,
        };
        log::info!("About: yt-dlp {ytdlp:?}, JavaScript {js:?}");
        Helpers { ytdlp, js, missing }
    }

    /// The best match for a song (remembered per song; a user's own pick wins).
    pub async fn find(&self, track: &TrackInfo) -> Result<Candidate> {
        self.find_with(track, false).await
    }

    /// [`find`](Self::find); `background` searches aren't kept for "Wrong song?" and their
    /// matches are saved a little later, together.
    async fn find_with(&self, track: &TrackInfo, background: bool) -> Result<Candidate> {
        if let Some(known) = self.remembered(&track.uri) {
            return Ok(known);
        }
        let ranked = self.ranked(track, !background).await?;
        let Some((best, _)) = ranked.into_iter().find(|(_, fits)| *fits) else {
            log::info!("no YouTube match for {} ({} - {})", track.uri, track.artist_line(), track.name);
            bail!(NOT_FOUND);
        };
        log::info!("YouTube: {} plays video {} {:?} (score {:.1})", track.uri, best.video_id, best.title, best.score);
        self.with_matches(|m| m.found.insert(track.uri.clone(), Saved::from(&best)));
        if background {
            self.save_matches_soon();
        } else {
            self.save_matches_in_background().await;
        }
        Ok(best)
    }

    /// The video a song plays now, if it has one yet.
    pub fn current(&self, uri: &str) -> Option<String> {
        self.remembered(uri).map(|c| c.video_id)
    }

    /// How long the video a song plays is, when known.
    pub fn length(&self, uri: &str) -> Option<u32> {
        self.remembered(uri).map(|c| c.duration_ms).filter(|&ms| ms > 0)
    }

    /// Every likely match, best first (for "Wrong song?").
    pub async fn candidates(&self, track: &TrackInfo) -> Result<Vec<Candidate>> {
        Ok(self.ranked(track, true).await?.into_iter().map(|(c, _)| c).collect())
    }

    /// The user picked this video for this Spotify song: used from now on (and saved). Returns
    /// whether the song plays a different video now.
    pub fn choose(&self, uri: &str, video_id: &str) -> bool {
        if !valid_video_id(video_id) {
            log::warn!("ignoring a YouTube pick that isn't a video id: {video_id:?}");
            return false;
        }
        let changed = self.current(uri).as_deref() != Some(video_id);
        let saved = self
            .known_video(video_id)
            .unwrap_or_else(|| Saved { video_id: video_id.to_owned(), ..Saved::default() });
        self.with_matches(|m| m.picked.insert(uri.to_owned(), saved));
        if changed {
            lock(&self.inner.recent).retain(|(u, _)| u != uri);
        }
        log::info!("YouTube: {uri} now plays video {video_id} (picked)");
        self.save_matches();
        changed
    }

    /// The song's audio, whole: kept in memory, else on disk (its match known), else its match
    /// and then the m4a file from YouTube.
    pub async fn audio(&self, track: &TrackInfo) -> Result<Audio> {
        let job = self.job(format!("audio:{}", track.uri));
        let _job = job.lock().await;
        if let Some(audio) = self.recent_audio(&track.uri) {
            return Ok(audio);
        }
        if let Some(audio) = self.disk_audio(track).await {
            self.keep_audio(&track.uri, &audio);
            return Ok(audio);
        }
        let _fetching = Counted::new(&self.inner.fetching);
        let _to_play = BACKGROUND.try_with(|_| ()).is_err().then(|| Counted::new(&self.inner.fetching_to_play));
        let (audio, size) = match self.fetch(track).await {
            Ok(fetched) => fetched,
            Err(e) => {
                self.note_problem(&e);
                return Err(e);
            }
        };
        self.keep_audio(&track.uri, &audio);
        self.keep_on_disk(&audio, size);
        Ok(audio)
    }

    /// Another copy of a song's audio is needed (this one couldn't be decoded): it is dropped
    /// from memory and disk.
    pub fn discard(&self, uri: &str, video_id: &str) {
        lock(&self.inner.recent).retain(|(u, a)| u != uri && a.video_id != video_id);
        let (me, id) = (self.clone(), video_id.to_owned());
        tokio::task::spawn_blocking(move || me.disk().remove(&id));
    }

    /// The song's match, then its audio file from YouTube, and the file's size when known.
    async fn fetch(&self, track: &TrackInfo) -> Result<(Audio, Option<u64>)> {
        let tools = self.tools().map_err(|e| anyhow!(e))?;
        let started = Instant::now();
        let mut found = self.find(track).await?;
        // Blocked or removed videos still show up in searches, often several for one song.
        let mut gone = Vec::new();
        let stream = loop {
            match resolve(&tools, &found.video_id, true).await {
                Ok(stream) => break stream,
                Err(e) if e.is::<Gone>() && gone.len() + 1 < MAX_TRIES => {
                    log::info!("YouTube video {} for {} is gone; trying the next match", found.video_id, track.uri);
                    gone.push(found.video_id.clone());
                    found = self.next_match(track, &gone).await?;
                }
                Err(e) if e.is::<Gone>() => {
                    log::info!("YouTube video {} for {} is gone too; giving up", found.video_id, track.uri);
                    bail!(GET_FAILED);
                }
                Err(e) => return Err(e),
            }
        };
        if !gone.is_empty() {
            self.replace_gone(&track.uri, &gone, &found);
            self.save_matches_in_background().await;
        }
        let resolved = started.elapsed();
        let title = stream.title.clone().unwrap_or_else(|| found.title.clone());
        let duration_ms =
            stream.duration.map(secs_to_ms).or((found.duration_ms > 0).then_some(found.duration_ms));
        if found.title.is_empty() || found.duration_ms == 0 {
            self.fill_in(&track.uri, &found.video_id, &title, duration_ms.unwrap_or(0)).await;
        }
        // Long videos (mixes) get longer: at least about 2 Mbit/s.
        let limit = DOWNLOAD_TIMEOUT.max(Duration::from_secs(stream.size().unwrap_or(0) / 250_000));
        let download = self.download(&tools, &found.video_id, stream);
        let (bytes, size) = match tokio::time::timeout(limit, download).await {
            Ok(result) => result?,
            Err(_) => {
                log::warn!("YouTube download of {} took over {}s", found.video_id, limit.as_secs());
                bail!(TOO_SLOW);
            }
        };
        log::info!(
            "YouTube audio for {}: video {}, {} KB (found in {:.1}s, all in {:.1}s)",
            track.uri,
            found.video_id,
            bytes.len() / 1024,
            resolved.as_secs_f32(),
            started.elapsed().as_secs_f32()
        );
        Ok((Audio { video_id: found.video_id, title, bytes, duration_ms }, size))
    }

    /// The best search result that fits, passing over videos that turned out to be gone. Not
    /// remembered until it plays.
    async fn next_match(&self, track: &TrackInfo, gone: &[String]) -> Result<Candidate> {
        let ranked = self.ranked(track, true).await?;
        match ranked.into_iter().find(|(c, fits)| *fits && !gone.contains(&c.video_id)) {
            Some((next, _)) => Ok(next),
            None => {
                log::info!("no other YouTube match for {}", track.uri);
                bail!(NOT_FOUND);
            }
        }
    }

    /// Remember `video` for this song in place of the gone ones (a pick that's gone is dropped).
    fn replace_gone(&self, uri: &str, gone: &[String], video: &Candidate) {
        log::info!("YouTube: {uri} now plays video {} {:?}", video.video_id, video.title);
        self.with_matches(|m| {
            if m.picked.get(uri).is_some_and(|p| gone.contains(&p.video_id)) {
                m.picked.remove(uri);
            }
            m.found.insert(uri.to_owned(), Saved::from(video));
        });
    }

    /// The search results for a song, scored, best first, with whether each one fits. `keep`:
    /// the search is remembered for a while.
    async fn ranked(&self, track: &TrackInfo, keep: bool) -> Result<Vec<(Candidate, bool)>> {
        let wanted = Wanted::of(track);
        let query = search_query(&wanted);
        if query.is_empty() {
            bail!(NOT_FOUND);
        }
        let entries = self.search(&query, keep).await?;
        Ok(rank(&wanted, &entries))
    }

    /// A YouTube search (remembered for a while when it found something and `keep` is set).
    async fn search(&self, query: &str, keep: bool) -> Result<Vec<Entry>> {
        let job = self.job(format!("search:{query}"));
        let _job = job.lock().await;
        if let Some((_, entries)) = lock(&self.inner.searches).iter().find(|(q, _)| q == query) {
            return Ok(entries.clone());
        }
        let tools = self.tools().map_err(|e| anyhow!(e))?;
        let started = Instant::now();
        let entries = search_youtube(&tools, query).await.inspect_err(|e| self.note_problem(e))?;
        log::info!(
            "YouTube search for {query:?}: {} results in {:.1}s",
            entries.len(),
            started.elapsed().as_secs_f32()
        );
        // An empty answer may be a hiccup: ask again next time.
        if entries.is_empty() || !keep {
            return Ok(entries);
        }
        let mut searches = lock(&self.inner.searches);
        searches.retain(|(q, _)| q != query);
        searches.push_back((query.to_owned(), entries.clone()));
        while searches.len() > RECENT_SEARCHES {
            searches.pop_front();
        }
        Ok(entries)
    }

    /// The audio file at `stream`, in pieces, and its size when known. A refusal (403) is asked
    /// again every [`POLL_EVERY`] while the address may still be about to work, then the address
    /// is asked for again once, the usual way (the quick way's may be what YouTube refuses); every
    /// piece gets one more try.
    async fn download(&self, tools: &Tools, video_id: &str, mut stream: Stream) -> Result<(Vec<u8>, Option<u64>)> {
        let mut size = stream.size();
        let mut bytes = Vec::with_capacity(size.unwrap_or(4 << 20).min(64 << 20) as usize);
        let mut renewed = false;
        let mut polls = 0u32;
        'pieces: loop {
            let start = bytes.len() as u64;
            if size.is_some_and(|s| start >= s) {
                break;
            }
            let end = size.map_or(start + CHUNK_BYTES, |s| s.min(start + CHUNK_BYTES)) - 1;
            let mut retried = false;
            let piece = loop {
                let http = self.http(stream.family())?;
                match fetch_range(&http, &stream, start, end).await {
                    Ok(piece) => break piece,
                    // Asked past the end of a file of unknown size: YouTube answers 400 (others 416).
                    Err(FetchError::Status(400 | 416)) if size.is_none() && start > 0 => break 'pieces,
                    Err(FetchError::Status(403)) if stream.may_work_soon() && polls < MAX_POLLS => {
                        polls += 1;
                        tokio::time::sleep(POLL_EVERY).await;
                    }
                    Err(e) if !retried => {
                        retried = true;
                        log::info!("YouTube download of {video_id} at byte {start}: {e}; trying again");
                        if matches!(e, FetchError::Status(403)) && !renewed {
                            renewed = true;
                            stream = resolve(tools, video_id, false).await?;
                            if stream.size() != size {
                                size = stream.size();
                                bytes.clear();
                                continue 'pieces;
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("YouTube download of {video_id} at byte {start} failed: {e}");
                        bail!(GET_FAILED);
                    }
                }
            };
            let asked = end - start + 1;
            let got = piece.len() as u64;
            bytes.extend_from_slice(&piece);
            if size.is_none() && got < asked {
                break;
            }
        }
        if size.is_some_and(|s| s != bytes.len() as u64) || !is_mp4(&bytes) {
            log::warn!("YouTube download of {video_id}: {} bytes that aren't an m4a file", bytes.len());
            bail!(GET_FAILED);
        }
        if polls > 0 {
            log::info!("YouTube download of {video_id}: the address worked after {polls} refusals");
        }
        Ok((bytes, size))
    }

    /// The HTTP client that connects over this kind of address.
    fn http(&self, family: Family) -> Result<reqwest::Client> {
        let mut clients = lock(&self.inner.http);
        if let Some(client) = clients.get(&family) {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .dns_resolver(OnlyFamily(family))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|e| {
                log::warn!("could not build the YouTube HTTP client: {e}");
                anyhow!(GET_FAILED)
            })?;
        clients.insert(family, client.clone());
        Ok(client)
    }

    /// The lock for one piece of work, shared by every call doing that work right now.
    fn job(&self, key: String) -> Arc<tokio::sync::Mutex<()>> {
        let mut jobs = lock(&self.inner.jobs);
        jobs.retain(|_, job| Arc::strong_count(job) > 1);
        jobs.entry(key).or_default().clone()
    }

    fn recent_audio(&self, uri: &str) -> Option<Audio> {
        let current = self.remembered(uri).map(|c| c.video_id);
        let mut recent = lock(&self.inner.recent);
        let at = recent.iter().position(|(u, _)| u == uri)?;
        let hit = recent.remove(at)?;
        // Fetched for a video the user has since replaced.
        if current.is_some_and(|id| id != hit.1.video_id) {
            return None;
        }
        let audio = hit.1.clone();
        recent.push_back(hit);
        Some(audio)
    }

    fn keep_audio(&self, uri: &str, audio: &Audio) {
        let mut recent = lock(&self.inner.recent);
        recent.retain(|(u, _)| u != uri);
        recent.push_back((uri.to_owned(), audio.clone()));
        while recent.len() > RECENT_AUDIO {
            recent.pop_front();
        }
    }

    /// The audio files on disk, with leftovers of an earlier run cleared on first use. Blocks.
    fn disk(&self) -> DiskCache {
        let disk = DiskCache { dir: self.inner.cache_dir.join(AUDIO_DIR) };
        self.inner.disk_cleaned.call_once(|| disk.clean_leftovers());
        disk
    }

    fn disk_limit(&self) -> u64 {
        self.inner.disk_limit.load(Ordering::Relaxed)
    }

    /// The song's audio from disk, when its match is known and the file kept.
    async fn disk_audio(&self, track: &TrackInfo) -> Option<Audio> {
        if self.disk_limit() == 0 {
            return None;
        }
        let found = self.remembered(&track.uri)?;
        let (me, id) = (self.clone(), found.video_id.clone());
        let bytes = tokio::task::spawn_blocking(move || me.disk().read(&id)).await.ok()??;
        log::info!("YouTube audio for {}: video {}, {} KB, kept on disk", track.uri, found.video_id, bytes.len() / 1024);
        let duration_ms = (found.duration_ms > 0).then_some(found.duration_ms);
        Some(Audio { video_id: found.video_id, title: found.title, bytes, duration_ms })
    }

    /// Keeps a downloaded file on disk, in the background.
    fn keep_on_disk(&self, audio: &Audio, size: Option<u64>) {
        let limit = self.disk_limit();
        if limit == 0 {
            return;
        }
        let (me, id, bytes) = (self.clone(), audio.video_id.clone(), audio.bytes.clone());
        tokio::task::spawn_blocking(move || {
            if let Err(e) = me.disk().store(&id, &bytes, size, limit) {
                log::warn!("couldn't keep the YouTube audio of video {id}: {e:#}");
            }
        });
    }

    /// Whether the song's audio is in memory, or on disk with its match known. Blocks briefly.
    fn is_fetched(&self, uri: &str) -> bool {
        let current = self.remembered(uri).map(|c| c.video_id);
        let in_memory = lock(&self.inner.recent)
            .iter()
            .any(|(u, a)| u == uri && current.as_ref().is_none_or(|id| *id == a.video_id));
        in_memory || self.disk_limit() > 0 && current.is_some_and(|id| self.disk().has(&id))
    }

    /// Background work waits a while after YouTube refused, couldn't be reached or couldn't be
    /// asked at all.
    fn note_problem(&self, problem: &anyhow::Error) {
        let problem = problem.to_string();
        let pause = match problem.as_str() {
            REFUSED => REFUSED_PAUSE,
            p if is_service_wide(p) => OFFLINE_PAUSE,
            _ => return,
        };
        let until = Instant::now() + pause;
        let mut paused = lock(&self.inner.paused_until);
        if paused.is_none_or(|at| at < until) {
            *paused = Some(until);
            log::info!("YouTube: background searches and fetches wait {} s ({problem})", pause.as_secs());
        }
    }

    fn paused_until(&self) -> Option<Instant> {
        lock(&self.inner.paused_until).filter(|&until| until > Instant::now())
    }

    /// Keeps these songs fetched (in memory or on disk), one at a time in the background. A newer
    /// list replaces this one; a fetch already running finishes (after an empty list, without
    /// starting yt-dlp again).
    pub fn fetch_ahead(&self, songs: Vec<TrackInfo>) {
        let mut ahead = lock(&self.inner.ahead);
        ahead.songs = songs;
        ahead.tried.clear();
        self.inner.ahead_on.store(!ahead.songs.is_empty(), Ordering::SeqCst);
        if !ahead.running && !ahead.songs.is_empty() {
            ahead.running = true;
            tokio::spawn(BACKGROUND.scope(self.inner.ahead_on.clone(), self.clone().fetch_ahead_worker()));
        }
        drop(ahead);
        self.inner.ahead_wake.notify_one();
    }

    async fn fetch_ahead_worker(self) {
        loop {
            // A moment for the song to play to start fetching first, and for skips to settle.
            tokio::time::sleep(BUSY_RECHECK).await;
            if let Some(until) = self.paused_until() {
                tokio::time::sleep_until(until.into()).await;
                continue;
            }
            if self.inner.fetching_to_play.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(BUSY_RECHECK).await;
                continue;
            }
            let songs = lock(&self.inner.ahead).songs.clone();
            let me = self.clone();
            let next = tokio::task::spawn_blocking(move || {
                let tried = lock(&me.inner.ahead).tried.clone();
                songs.into_iter().find(|t| !t.is_local && !tried.contains(&t.uri) && !me.is_fetched(&t.uri))
            })
            .await
            .ok()
            .flatten();
            let Some(song) = next else {
                self.inner.ahead_wake.notified().await;
                continue;
            };
            lock(&self.inner.ahead).tried.insert(song.uri.clone());
            log::info!("YouTube: fetching ahead \"{}\"", song.name);
            if let Err(e) = self.audio(&song).await {
                log::info!("YouTube: couldn't fetch \"{}\" ahead: {e}", song.name);
            }
        }
    }

    /// Looks for matches for a list's songs in the background (searches only), nearest to
    /// `current` (else from the top) first, gently: see [`SearchPlan`]. A newer list replaces this
    /// one; one with nothing to search stops it, searches already running included.
    pub fn search_ahead(&self, songs: &[TrackInfo], current: Option<&str>) {
        let at = current.and_then(|uri| songs.iter().position(|t| t.uri == uri));
        let wanted: Vec<TrackInfo> = search_order(songs.len(), at)
            .into_iter()
            .map(|i| &songs[i])
            .filter(|t| !t.is_local && self.remembered(&t.uri).is_none())
            .take(SEARCH_AHEAD_MAX)
            .cloned()
            .collect();
        let mut plan = lock(&self.inner.plan);
        plan.replace(wanted);
        self.inner.search_on.store(!plan.waiting.is_empty(), Ordering::SeqCst);
        if !plan.started && !plan.waiting.is_empty() {
            plan.started = true;
            tokio::spawn(BACKGROUND.scope(self.inner.search_on.clone(), self.clone().search_ahead_worker()));
        }
        drop(plan);
        self.inner.plan_wake.notify_one();
    }

    async fn search_ahead_worker(self) {
        loop {
            let paused = self.paused_until();
            let busy = self.inner.fetching.load(Ordering::SeqCst) > 0;
            let step = lock(&self.inner.plan).next(Instant::now(), paused, busy, |uri| self.remembered(uri).is_some());
            match step {
                Step::Search(song) => {
                    let me = self.clone();
                    tokio::spawn(BACKGROUND.scope(self.inner.search_on.clone(), async move {
                        let result = me.find_with(&song, true).await;
                        let stopped = result.as_ref().is_err_and(|e| is_service_wide(&e.to_string()));
                        if let Err(e) = &result {
                            me.note_problem(e);
                        }
                        lock(&me.inner.plan).done(song, stopped);
                        me.inner.plan_wake.notify_one();
                    }));
                }
                Step::Wait(until) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(until.into()) => {}
                        () = self.inner.plan_wake.notified() => {}
                    }
                }
                Step::Idle => self.inner.plan_wake.notified().await,
            }
        }
    }

    /// A video's details from a recent search or a remembered match.
    fn known_video(&self, video_id: &str) -> Option<Saved> {
        let from_search = lock(&self.inner.searches)
            .iter()
            .rev()
            .flat_map(|(_, entries)| entries)
            .find(|e| e.id == video_id)
            .map(|e| Saved {
                video_id: e.id.clone(),
                title: e.title.clone().unwrap_or_default(),
                channel: channel_of(e).to_owned(),
                duration_ms: e.duration.map_or(0, secs_to_ms),
                score: 0.0,
            });
        from_search.or_else(|| {
            self.with_matches(|m| m.picked.values().chain(m.found.values()).find(|s| s.video_id == video_id).cloned())
        })
    }

    fn remembered(&self, uri: &str) -> Option<Candidate> {
        self.with_matches(|m| m.picked.get(uri).or_else(|| m.found.get(uri)).map(Saved::to_candidate))
    }

    /// Add the title and length to a remembered match that lacks them (a pick made by video id).
    async fn fill_in(&self, uri: &str, video_id: &str, title: &str, duration_ms: u32) {
        let changed = self.with_matches(|m| {
            let mut changed = false;
            for saved in [m.picked.get_mut(uri), m.found.get_mut(uri)].into_iter().flatten() {
                if saved.video_id == video_id && (saved.title.is_empty() || saved.duration_ms == 0) {
                    saved.title = title.to_owned();
                    saved.duration_ms = duration_ms;
                    changed = true;
                }
            }
            changed
        });
        if changed {
            self.save_matches_in_background().await;
        }
    }

    fn match_file(&self) -> PathBuf {
        self.inner.cache_dir.join(MATCH_FILE)
    }

    /// Runs `f` on the remembered matches, reading the file first if this is the first use (an
    /// unreadable file is ignored and replaced on the next save).
    fn with_matches<R>(&self, f: impl FnOnce(&mut Matches) -> R) -> R {
        let mut matches = lock(&self.inner.matches);
        let matches = matches.get_or_insert_with(|| config::read_json(&self.match_file()).unwrap_or_default());
        f(matches)
    }

    fn save_matches(&self) {
        let _saving = lock(&self.inner.saving);
        let bytes = match self.with_matches(|m| serde_json::to_vec(&*m)) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("could not save YouTube matches: {e}");
                return;
            }
        };
        if let Err(e) = config::write_atomic(&self.match_file(), &bytes) {
            log::warn!("{e:#}");
        }
    }

    async fn save_matches_in_background(&self) {
        let me = self.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || me.save_matches()).await {
            log::warn!("saving YouTube matches stopped: {e}");
        }
    }

    /// Saves the matches a little later, with any others found meanwhile.
    fn save_matches_soon(&self) {
        if self.inner.save_planned.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SAVE_DELAY).await;
            me.inner.save_planned.store(false, Ordering::SeqCst);
            me.save_matches_in_background().await;
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Counts one piece of work while it lives.
struct Counted<'a>(&'a AtomicUsize);

impl<'a> Counted<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

// --- Background searches ---

/// The order a list's songs are searched in: nearest to the one at `current` first (the next one
/// before the one before it), else from the top.
fn search_order(len: usize, current: Option<usize>) -> Vec<usize> {
    let Some(at) = current.filter(|&at| at < len) else { return (0..len).collect() };
    let mut order = vec![at];
    for step in 1..len {
        if at + step < len {
            order.push(at + step);
        }
        if step <= at {
            order.push(at - step);
        }
    }
    order
}

/// Which background search runs next, and when: at most [`SEARCHES_AT_ONCE`] at once and one
/// start per [`SEARCH_GAP`], none while a song is being fetched or while YouTube is refusing,
/// and each song once per run (one that got no answer because YouTube refused goes back first).
#[derive(Default)]
struct SearchPlan {
    waiting: VecDeque<TrackInfo>,
    running: usize,
    last_start: Option<Instant>,
    tried: HashSet<String>,
    /// The worker that runs the searches is running.
    started: bool,
}

#[derive(Debug, PartialEq)]
enum Step {
    Search(TrackInfo),
    /// Ask again then (or when woken).
    Wait(Instant),
    /// Nothing to do until woken.
    Idle,
}

impl SearchPlan {
    fn replace(&mut self, songs: Vec<TrackInfo>) {
        self.waiting = songs.into_iter().filter(|t| !self.tried.contains(&t.uri)).collect();
    }

    fn next(&mut self, now: Instant, paused_until: Option<Instant>, busy: bool, matched: impl Fn(&str) -> bool) -> Step {
        if let Some(until) = paused_until.filter(|&until| until > now) {
            return Step::Wait(until);
        }
        if self.waiting.is_empty() || self.running >= SEARCHES_AT_ONCE {
            return Step::Idle;
        }
        if busy {
            return Step::Wait(now + BUSY_RECHECK);
        }
        if let Some(at) = self.last_start.map(|t| t + SEARCH_GAP).filter(|&at| at > now) {
            return Step::Wait(at);
        }
        while let Some(song) = self.waiting.pop_front() {
            if matched(&song.uri) || !self.tried.insert(song.uri.clone()) {
                continue;
            }
            self.running += 1;
            self.last_start = Some(now);
            return Step::Search(song);
        }
        Step::Idle
    }

    /// A search ended; `stopped`: YouTube refused or couldn't be reached, so it is tried again.
    fn done(&mut self, song: TrackInfo, stopped: bool) {
        self.running = self.running.saturating_sub(1);
        if stopped {
            self.tried.remove(&song.uri);
            self.waiting.push_front(song);
        }
    }
}

// --- Songs' audio on disk ---

/// Songs' audio files (`<video id>.m4a`), the least recently used removed first when they take
/// more room than allowed. Every method blocks.
struct DiskCache {
    dir: PathBuf,
}

impl DiskCache {
    fn file(&self, video_id: &str) -> PathBuf {
        self.dir.join(format!("{video_id}.m4a"))
    }

    fn has(&self, video_id: &str) -> bool {
        valid_video_id(video_id) && self.file(video_id).is_file()
    }

    /// A video's audio, marked as just used. A file that isn't an m4a is removed.
    fn read(&self, video_id: &str) -> Option<Vec<u8>> {
        if !valid_video_id(video_id) {
            return None;
        }
        let path = self.file(video_id);
        let bytes = std::fs::read(&path).ok()?;
        if !is_mp4(&bytes) {
            log::warn!("removing {}: not an m4a file", path.display());
            let _ = std::fs::remove_file(&path);
            return None;
        }
        // Windows doesn't keep last-access times up to date: the modified time says when it was used.
        if let Ok(file) = std::fs::File::options().write(true).open(&path) {
            let _ = file.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    /// Keeps a video's audio if it is the whole m4a file (`size` bytes) and fits in `limit` bytes:
    /// written whole or not at all, then the oldest files go until all fit.
    fn store(&self, video_id: &str, bytes: &[u8], size: Option<u64>, limit: u64) -> Result<()> {
        if !valid_video_id(video_id) {
            bail!("{video_id:?} isn't a video id");
        }
        if size != Some(bytes.len() as u64) || !is_mp4(bytes) {
            bail!("{} bytes that aren't a whole m4a file of {size:?} bytes", bytes.len());
        }
        // Kept, it would push every other file out, then itself.
        if bytes.len() as u64 > limit {
            self.prune(limit);
            return Ok(());
        }
        static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(&self.dir)?;
        let path = self.file(video_id);
        let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!("{video_id}.{}-{n}.tmp", std::process::id()));
        let written = (|| {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow!("could not write {}: {e}", path.display()));
        }
        self.prune(limit);
        Ok(())
    }

    fn remove(&self, video_id: &str) {
        if valid_video_id(video_id) {
            let _ = std::fs::remove_file(self.file(video_id));
        }
    }

    /// Removes the least recently used files until the rest fit in `limit` bytes.
    fn prune(&self, limit: u64) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "m4a"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some((meta.modified().ok()?, meta.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
        files.sort_by_key(|(modified, _, _)| *modified);
        for (_, len, path) in files {
            if total <= limit {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }

    /// Removes files left half-written by an earlier run.
    fn clean_leftovers(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|x| x == "tmp") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// --- Finding the programs ---

/// Where the programs may be.
struct Places {
    /// Beside truffle.exe, then Truffle's data folder.
    own: Vec<PathBuf>,
    /// The folders on PATH.
    path: Vec<PathBuf>,
    /// The user's folder (`%USERPROFILE%`).
    home: Option<PathBuf>,
    program_files: Option<PathBuf>,
}

impl Places {
    fn of_system(cache_dir: &Path, exe_dir: Option<&Path>) -> Self {
        let mut own: Vec<PathBuf> = exe_dir.map(Path::to_path_buf).into_iter().collect();
        own.extend(data_dir(cache_dir));
        let path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).collect())
            .unwrap_or_default();
        Self {
            own,
            path,
            home: std::env::var_os("USERPROFILE").map(PathBuf::from),
            program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
        }
    }
}

/// Truffle's data folder (`...\truffle`), the parent of its `cache` folder.
fn data_dir(cache_dir: &Path) -> Option<PathBuf> {
    cache_dir
        .ancestors()
        .take(2)
        .find(|d| d.file_name().is_some_and(|n| n.eq_ignore_ascii_case("cache")))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// JavaScript runtimes yt-dlp can use, preferred first: its name for one, and the program names.
const JS_RUNTIMES: &[(&str, &[&str])] =
    &[("node", &["node"]), ("deno", &["deno"]), ("bun", &["bun"]), ("quickjs", &["qjs", "quickjs"])];

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn find_tools(places: &Places) -> std::result::Result<Tools, String> {
    let ytdlp = find_ytdlp(places).ok_or_else(|| NO_YTDLP.to_owned())?;
    match find_js(places, ytdlp.parent()) {
        Some(js) => Ok(Tools { ytdlp, js: Some(js) }),
        None => Err(NO_JS.to_owned()),
    }
}

fn find_ytdlp(places: &Places) -> Option<PathBuf> {
    let ytdlp_name = exe("yt-dlp");
    let downloads = places.home.as_ref().map(|h| h.join("Downloads"));
    places.own.iter().chain(&places.path).chain(&downloads).map(|dir| dir.join(&ytdlp_name)).find(|p| p.is_file())
}

/// The JavaScript runtime yt-dlp would use (its name for it, and the program), looked for beside
/// yt-dlp first.
fn find_js(places: &Places, beside: Option<&Path>) -> Option<(String, PathBuf)> {
    let beside = beside.map(Path::to_path_buf);
    JS_RUNTIMES.iter().find_map(|(runtime, programs)| {
        let usual = match *runtime {
            "node" => places.program_files.as_ref().map(|p| p.join("nodejs")),
            "deno" => places.home.as_ref().map(|h| h.join(".deno").join("bin")),
            "bun" => places.home.as_ref().map(|h| h.join(".bun").join("bin")),
            _ => None,
        };
        let dirs: Vec<&PathBuf> = beside.iter().chain(&places.own).chain(&places.path).chain(&usual).collect();
        programs.iter().find_map(|program| {
            let name = exe(program);
            dirs.iter().map(|d| d.join(&name)).find(|p| p.is_file()).map(|p| ((*runtime).to_owned(), p))
        })
    })
}

/// How Settings > About names a JavaScript runtime (by yt-dlp's name for it).
fn runtime_name(runtime: &str) -> &str {
    match runtime {
        "node" => "Node.js",
        "deno" => "Deno",
        "bun" => "Bun",
        "quickjs" => "QuickJS",
        other => other,
    }
}

/// The version in what a program printed for `--version`: the first word of its first line that
/// starts with a digit, without a leading "v" ("v24.19.0" -> "24.19.0", "deno 2.1.4 (stable)" ->
/// "2.1.4").
fn version_in(printed: &str) -> Option<String> {
    let first = printed.lines().map(str::trim).find(|l| !l.is_empty())?;
    first
        .split_whitespace()
        .map(|w| w.strip_prefix('v').unwrap_or(w))
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .map(|w| w.trim_end_matches([',', ';']).to_owned())
}

/// Runs a program with `--version` (no window, nothing on its input) and gives the version it
/// printed, or an empty text when it gave none in time. One that is slow is left to end by itself:
/// yt-dlp stopped while it unpacks leaves its files behind.
async fn version_of(program: &Path) -> String {
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(false);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    match tokio::time::timeout(VERSION_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => version_in(&String::from_utf8_lossy(&out.stdout)).unwrap_or_default(),
        Ok(Err(e)) => {
            log::warn!("About: a YouTube helper didn't start: {e}");
            String::new()
        }
        Err(_) => {
            log::warn!("About: a YouTube helper gave no version within {}s", VERSION_TIMEOUT.as_secs());
            String::new()
        }
    }
}

// --- Running yt-dlp ---

/// Why a yt-dlp run gave nothing.
#[derive(Debug)]
enum RunError {
    Start(std::io::Error),
    Io(std::io::Error),
    Timeout(Duration),
    /// It ended with an error; what it wrote about it.
    Failed(String),
    /// Not started: the background work it was for was called off.
    CalledOff,
}

impl RunError {
    /// Logs the details and gives the short text the user sees.
    fn into_user_error(self, what: &str, fallback: &'static str) -> anyhow::Error {
        let shown = match &self {
            RunError::Start(e) => {
                log::warn!("{what}: could not start yt-dlp: {e}");
                NO_START
            }
            RunError::Io(e) => {
                log::warn!("{what}: reading yt-dlp's output failed: {e}");
                fallback
            }
            RunError::Timeout(limit) => {
                log::warn!("{what}: yt-dlp took over {}s", limit.as_secs());
                TOO_SLOW
            }
            RunError::Failed(stderr) => {
                log::warn!("{what}: yt-dlp failed: {}", tail(stderr, 600));
                explain(stderr).unwrap_or(fallback)
            }
            RunError::CalledOff => CALLED_OFF,
        };
        anyhow!(shown)
    }
}

/// The video is no longer on YouTube, or can't be played without signing in.
#[derive(Debug)]
struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This video isn't on YouTube anymore.")
    }
}

impl std::error::Error for Gone {}

fn is_gone(stderr: &str) -> bool {
    let s = stderr.to_lowercase();
    !is_limited(&s)
        && [
            "video unavailable",
            "video is unavailable",
            "private video",
            "has been removed",
            "has been terminated",
            "available in your country",
            "this video is not available",
            "confirm your age",
            "members-only",
            "requires payment",
        ]
        .iter()
        .any(|k| s.contains(k))
}

/// Whether YouTube is turning this computer away for a while (it then calls every video
/// "unavailable ... try again later").
fn is_limited(lowercase: &str) -> bool {
    ["not a bot", "http error 429", "try again later", "rate-limited", "rate limited"]
        .iter()
        .any(|k| lowercase.contains(k))
}

/// A short, plain reason for a common yt-dlp error.
fn explain(stderr: &str) -> Option<&'static str> {
    let s = stderr.to_lowercase();
    if is_limited(&s) {
        Some(REFUSED)
    } else if ["getaddrinfo", "urlopen error", "timed out", "unreachable", "connection"].iter().any(|k| s.contains(k))
    {
        Some(OFFLINE)
    } else {
        None
    }
}

/// The last `max` bytes of a text, at a character boundary.
fn tail(text: &str, max: usize) -> &str {
    let text = text.trim();
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Runs yt-dlp with these arguments (`js`: with the JavaScript runtime) and gives the first line
/// of JSON it prints.
async fn run_ytdlp(tools: &Tools, args: &[&str], js: bool, limit: Duration) -> Result<String, RunError> {
    let mut cmd = tokio::process::Command::new(&tools.ytdlp);
    cmd.args(["--ignore-config", "--no-warnings"]);
    if js && let Some((name, path)) = &tools.js {
        let mut runtime = OsString::from(format!("{name}:"));
        runtime.push(path.as_os_str());
        cmd.arg("--js-runtimes").arg(runtime);
    }
    cmd.args(args).env("PYTHONIOENCODING", "utf-8");
    let background = BACKGROUND.try_with(|on| on.load(Ordering::SeqCst)).ok();
    if background == Some(false) {
        return Err(RunError::CalledOff);
    }
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW | if background.is_some() { BELOW_NORMAL_PRIORITY_CLASS } else { 0 });
    first_json_line(cmd, limit).await
}

/// Starts a program and gives the first line of JSON it prints as soon as it is printed (yt-dlp
/// takes about half a second more to end), leaving the program to finish by itself. When it
/// prints none, what it wrote about the error.
async fn first_json_line(mut cmd: tokio::process::Command, limit: Duration) -> Result<String, RunError> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(false);
    let mut child = cmd.spawn().map_err(RunError::Start)?;
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let mut running = Running(Some(child));
    let errors = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut buf).await;
        }
        buf
    });
    let outcome = tokio::time::timeout(limit, async {
        if let Some(stdout) = stdout {
            let mut out = tokio::io::BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                if out.read_until(b'\n', &mut line).await? == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&line);
                if text.trim_start().starts_with('{') {
                    return Ok(Ok((text.trim().to_owned(), out)));
                }
            }
        }
        let status = running.wait().await?;
        Ok(Err(status))
    })
    .await;
    match outcome {
        Err(_) => {
            running.kill();
            Err(RunError::Timeout(limit))
        }
        Ok(Err(e)) => Err(RunError::Io(e)),
        Ok(Ok(Ok((json, rest)))) => {
            running.finish_in_background(rest);
            Ok(json)
        }
        Ok(Ok(Err(status))) => {
            let stderr = errors.await.unwrap_or_default();
            if status.success() {
                Ok(String::new())
            } else {
                Err(RunError::Failed(String::from_utf8_lossy(&stderr).into_owned()))
            }
        }
    }
}

/// A running yt-dlp: one whose answer came is left to finish by itself, one no longer waited for
/// is stopped at once (see [`stop`]).
struct Running(Option<tokio::process::Child>);

impl Running {
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let Some(child) = self.0.as_mut() else {
            return Err(std::io::Error::other("yt-dlp already finished"));
        };
        let status = child.wait().await?;
        self.0 = None;
        Ok(status)
    }

    /// Stops it now, in the background.
    fn kill(&mut self) {
        if let Some(child) = self.0.take() {
            std::thread::spawn(move || stop(child));
        }
    }

    /// Leaves it to finish by itself, reading the rest of what it prints.
    fn finish_in_background(mut self, mut rest: impl AsyncRead + Unpin + Send + 'static) {
        let mut running = Running(self.0.take());
        tokio::spawn(async move {
            let _ = tokio::time::timeout(LINGER, tokio::io::copy(&mut rest, &mut tokio::io::sink())).await;
            drop(rest);
            if tokio::time::timeout(LINGER, running.wait()).await.is_err() {
                running.kill();
            }
        });
    }
}

impl Drop for Running {
    /// Nobody waits for it any more (the song was skipped): stopped, so it asks YouTube nothing more.
    fn drop(&mut self) {
        if self.0.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None))) {
            self.kill();
        }
    }
}

/// Stops a yt-dlp without leaving files behind. yt-dlp.exe unpacks itself to a temporary folder and
/// runs a second copy from there; killing the first leaves the folder (and the second running), but
/// once the second is stopped the first removes the folder and ends. One without a second copy, or
/// that doesn't end, is killed with all it started. Blocks a few seconds at most.
fn stop(mut child: tokio::process::Child) {
    #[cfg(windows)]
    {
        let started = Instant::now();
        let mut copy_stopped = false;
        while started.elapsed() < STOP_WAIT {
            if !matches!(child.try_wait(), Ok(None)) {
                return;
            }
            if !copy_stopped {
                let copies = second_copies(&child);
                copies.iter().for_each(|&pid| kill_tree_of(pid));
                copy_stopped = !copies.is_empty();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    kill_tree(child);
}

/// Stops a program and the programs it started. Blocks.
fn kill_tree(mut child: tokio::process::Child) {
    // The first process stays alive until taskkill has found the second through it.
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        kill_tree_of(pid);
    }
    let _ = child.start_kill();
}

#[cfg(windows)]
fn kill_tree_of(pid: u32) {
    let taskkill = std::env::var_os("SystemRoot")
        .map_or_else(|| PathBuf::from("taskkill"), |r| Path::new(&r).join("System32").join("taskkill.exe"));
    let mut kill = std::process::Command::new(taskkill);
    kill.args(["/T", "/F", "/PID", &pid.to_string()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    std::os::windows::process::CommandExt::creation_flags(&mut kill, CREATE_NO_WINDOW);
    let _ = kill.status();
}

/// yt-dlp's second copy: started by it, after it, from a file of the same name.
#[cfg(windows)]
fn second_copies(child: &tokio::process::Child) -> Vec<u32> {
    let (Some(pid), Some(handle)) = (child.id(), child.raw_handle()) else { return Vec::new() };
    let Some(born) = started_at(handle) else { return Vec::new() };
    let all = processes();
    let Some(name) = all.iter().find(|p| p.id == pid).map(|p| p.name.to_lowercase()) else { return Vec::new() };
    all.iter()
        .filter(|p| p.parent == pid && p.name.to_lowercase() == name && started_after(p.id, born))
        .map(|p| p.id)
        .collect()
}

/// When a program started (Windows file time).
#[cfg(windows)]
fn started_at(process: std::os::windows::io::RawHandle) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let (mut created, mut exited, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: GetProcessTimes writes four FILETIMEs; the handle is a live process handle.
    let ok = unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } != 0;
    ok.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Whether program `pid` started at or after `time` (so it isn't an older one whose parent ended
/// and whose id was given out again).
#[cfg(windows)]
fn started_after(pid: u32, time: u64) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: the handle is checked before use and closed after.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let started = started_at(process);
        CloseHandle(process);
        started.is_some_and(|at| at >= time)
    }
}

/// A running program, as Windows lists it.
#[cfg(windows)]
struct Process {
    id: u32,
    parent: u32,
    name: String,
}

/// The programs running now.
#[cfg(windows)]
fn processes() -> Vec<Process> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    let mut found = Vec::new();
    // SAFETY: the snapshot is checked and closed; each entry is sized as the API asks.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            found.push(Process { id: entry.th32ProcessID, parent: entry.th32ParentProcessID, name });
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    found
}

// --- Search results and scoring ---

/// One search result, as yt-dlp lists it.
#[derive(Debug, Clone, Deserialize)]
struct Entry {
    id: String,
    title: Option<String>,
    /// Seconds.
    duration: Option<f64>,
    channel: Option<String>,
    uploader: Option<String>,
    live_status: Option<String>,
}

fn channel_of(entry: &Entry) -> &str {
    entry.channel.as_deref().or(entry.uploader.as_deref()).unwrap_or("")
}

/// The videos in yt-dlp's `-J --flat-playlist` output for a search (streams and odd entries left out).
fn parse_search(json: &[u8]) -> Result<Vec<Entry>> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        entries: Option<Vec<serde_json::Value>>,
    }
    let page: Page = serde_json::from_slice(json)?;
    Ok(page
        .entries
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Entry>(v).ok())
        .filter(|e| valid_video_id(&e.id) && !matches!(e.live_status.as_deref(), Some("is_live" | "is_upcoming")))
        .collect())
}

/// YouTube video ids are 11 letters, digits, `-` and `_`.
fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn secs_to_ms(secs: f64) -> u32 {
    (secs * 1000.0).round() as u32
}

/// The Spotify song a match is scored against.
struct Wanted {
    title: String,
    artists: Vec<String>,
    /// 0 when unknown.
    duration_ms: u32,
}

impl Wanted {
    fn of(track: &TrackInfo) -> Self {
        Self {
            title: track.name.clone(),
            artists: track.artists.iter().map(|a| a.name.clone()).filter(|n| !n.trim().is_empty()).collect(),
            duration_ms: track.duration_ms,
        }
    }
}

/// "artists - title" (at most three artists), with credits and release notes left out.
fn search_query(wanted: &Wanted) -> String {
    let artists = wanted.artists.iter().take(3).map(String::as_str).collect::<Vec<_>>().join(" ");
    let title = clean_title(&wanted.title);
    match (artists.is_empty(), title.is_empty()) {
        (_, true) => artists,
        (true, false) => title,
        (false, false) => format!("{artists} - {title}"),
    }
}

/// The title without credits and release notes ("(feat. X)", " - Remastered 2011"). Notes that
/// change the sound ("Live", "Acoustic", "Remix") stay.
fn clean_title(title: &str) -> String {
    let mut kept = String::with_capacity(title.len());
    let mut rest = title;
    while let Some(open) = rest.find(['(', '[']) {
        let close = if rest[open..].starts_with('(') { ')' } else { ']' };
        let Some(len) = rest[open + 1..].find(close) else { break };
        kept.push_str(&rest[..open]);
        if !is_note(&rest[open + 1..open + 1 + len]) {
            kept.push_str(&rest[open..open + len + 2]);
        }
        rest = &rest[open + len + 2..];
    }
    kept.push_str(rest);
    let mut parts = kept.split(" - ");
    let mut cleaned = parts.next().unwrap_or("").to_owned();
    for part in parts.filter(|p| !is_note(p)) {
        cleaned.push_str(" - ");
        cleaned.push_str(part);
    }
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() { title.split_whitespace().collect::<Vec<_>>().join(" ") } else { cleaned }
}

/// Whether part of a title is a credit or a release note rather than part of the name.
fn is_note(part: &str) -> bool {
    let w = words(part);
    let Some(first) = w.first() else { return false };
    matches!(first.as_str(), "feat" | "ft" | "featuring" | "with" | "prod" | "from")
        || w.iter().any(|x| {
            x.starts_with("remaster") || matches!(x.as_str(), "deluxe" | "anniversary" | "explicit" | "mono" | "stereo")
        })
        || [["bonus", "track"], ["single", "version"], ["album", "version"]].iter().any(|p| has_phrase(&w, &p[..]))
}

/// Lowercase words (letters and digits only).
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Lowercase letters and digits, without spaces ("TV Girl" and "TVGirl" both give "tvgirl").
fn compact(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Whether these words appear one after another.
fn has_phrase<S: AsRef<str>>(words: &[String], phrase: &[S]) -> bool {
    count_phrase(words, phrase) > 0
}

/// How many times these words appear one after another.
fn count_phrase<S: AsRef<str>>(words: &[String], phrase: &[S]) -> usize {
    if phrase.is_empty() {
        return 0;
    }
    words.windows(phrase.len()).filter(|w| w.iter().zip(phrase).all(|(a, b)| a == b.as_ref())).count()
}

/// Versions that aren't the song as released (penalised unless the Spotify title says so).
const OTHER_VERSIONS: &[&[&str]] = &[
    &["live"],
    &["cover"],
    &["remix"],
    &["sped", "up"],
    &["speed", "up"],
    &["slowed"],
    &["nightcore"],
    &["karaoke"],
    &["8d"],
    &["1", "hour"],
    &["1hour"],
    &["hours"],
    &["reverb"],
    &["instrumental"],
    &["acoustic"],
    &["mashup"],
    &["bass", "boosted"],
    &["acapella"],
    &["a", "cappella"],
    &["reaction"],
    &["extended"],
    &["tour"],
    &["concert"],
    &["rehearsal"],
    &["rehearsals"],
    &["demo"],
    &["tutorial"],
];

/// Usually the same song, but not quite the released recording ("2019 Mix"): a smaller penalty.
const OTHER_MIXES: &[&[&str]] = &[&["mix"]];

/// How well a search result fits the song (higher is better), and whether it may be played
/// without asking: its length is close enough and it isn't another version.
fn score(wanted: &Wanted, entry: &Entry) -> (f32, bool) {
    let title = words(entry.title.as_deref().unwrap_or(""));
    let channel = channel_of(entry);
    let mut score = 0.0;

    // Length: the strongest sign. Too far off sorts below everything that fits.
    let fits = match (wanted.duration_ms, entry.duration.map(secs_to_ms)) {
        (0, _) => true,
        (_, None) => {
            score -= 100.0;
            false
        }
        (want, Some(got)) => {
            let diff = want.abs_diff(got);
            if diff <= MAX_LENGTH_DIFF_MS {
                score += 40.0 * (1.0 - diff as f32 / MAX_LENGTH_DIFF_MS as f32);
                true
            } else {
                score -= 100.0 + diff as f32 / 1000.0;
                false
            }
        }
    };

    let title_words = words(&clean_title(&wanted.title));
    if !title_words.is_empty() {
        let present = title_words.iter().filter(|w| title.contains(w)).count();
        score += 20.0 * present as f32 / title_words.len() as f32;
    }

    let artists: Vec<Vec<String>> = wanted.artists.iter().map(|a| words(a)).filter(|w| !w.is_empty()).collect();
    let channel_words = words(channel);
    if artists.iter().any(|a| has_phrase(&title, a.as_slice()) || has_phrase(&channel_words, a.as_slice())) {
        score += 12.0;
    }

    // Official channels: YouTube's "Artist - Topic" (the released recording), or the artist's own.
    let channel = channel.to_lowercase();
    let (name, topic) = match channel.strip_suffix(" - topic") {
        Some(name) => (compact(name), true),
        None => (compact(&channel), false),
    };
    if topic {
        score += 6.0;
    }
    let official = wanted.artists.iter().map(|a| compact(a)).filter(|a| !a.is_empty()).any(|a| {
        name.strip_prefix(a.as_str()).is_some_and(|rest| matches!(rest, "" | "vevo" | "official" | "music"))
    });
    if official {
        score += 4.0;
    }

    // Other versions, unless the Spotify title is that version ("Song - Live"). A word that is part
    // of the song's name or an artist's ("Live Forever", the band Live) only counts beyond the
    // times those already have it. Other versions are never picked on their own.
    let mut own = words(&wanted.title);
    let mut named = words(&song_name(&wanted.title));
    for artist in &wanted.artists {
        own.extend(words(artist));
        named.extend(words(artist));
    }
    let mut other_version = false;
    for (versions, penalty, never_pick) in [(OTHER_VERSIONS, 25.0, true), (OTHER_MIXES, 8.0, false)] {
        for &version in versions {
            let usual = count_phrase(&named, version);
            if count_phrase(&own, version) <= usual && count_phrase(&title, version) > usual {
                score -= penalty;
                other_version |= never_pick;
            }
        }
    }
    (score, fits && !other_version)
}

/// The song's own name: the title without anything in brackets or after " - " ("Live Forever"
/// keeps its "Live", "Song - Live" and "Song (Live)" don't).
fn song_name(title: &str) -> String {
    let head = title.split(" - ").next().unwrap_or("");
    let mut name = String::with_capacity(head.len());
    let mut depth = 0u32;
    for c in head.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => name.push(c),
            _ => {}
        }
    }
    if name.trim().is_empty() { title.to_owned() } else { name }
}

/// Search results as candidates, best first: those that fit (see [`score`]), then the rest. Ties
/// keep YouTube's order.
fn rank(wanted: &Wanted, entries: &[Entry]) -> Vec<(Candidate, bool)> {
    let mut seen = Vec::new();
    let mut ranked: Vec<(Candidate, bool)> = entries
        .iter()
        .filter(|e| {
            let new = !seen.contains(&e.id.as_str());
            seen.push(e.id.as_str());
            new
        })
        .map(|e| {
            let (score, fits) = score(wanted, e);
            let candidate = Candidate {
                video_id: e.id.clone(),
                title: e.title.clone().unwrap_or_default(),
                channel: channel_of(e).to_owned(),
                duration_ms: e.duration.map_or(0, secs_to_ms),
                score,
            };
            (candidate, fits)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.score.total_cmp(&a.0.score)));
    ranked
}

// --- Remembered matches ---

/// Remembered matches by Spotify URI (`youtube-matches.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Matches {
    /// Videos the user picked ("Wrong song?").
    #[serde(default)]
    picked: BTreeMap<String, Saved>,
    /// The best search results.
    #[serde(default)]
    found: BTreeMap<String, Saved>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Saved {
    video_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    duration_ms: u32,
    #[serde(default)]
    score: f32,
}

impl Saved {
    fn to_candidate(&self) -> Candidate {
        Candidate {
            video_id: self.video_id.clone(),
            title: self.title.clone(),
            channel: self.channel.clone(),
            duration_ms: self.duration_ms,
            score: self.score,
        }
    }
}

impl From<&Candidate> for Saved {
    fn from(c: &Candidate) -> Self {
        Self {
            video_id: c.video_id.clone(),
            title: c.title.clone(),
            channel: c.channel.clone(),
            duration_ms: c.duration_ms,
            score: c.score,
        }
    }
}

// --- Fetching the audio file ---

/// The chosen audio file, from yt-dlp.
#[derive(Debug, Clone, Deserialize)]
struct Stream {
    url: String,
    title: Option<String>,
    /// Seconds.
    duration: Option<f64>,
    filesize: Option<u64>,
    /// When the address starts working (Unix seconds): YouTube refuses it (403) for the first
    /// few seconds, as long as the ad it would show first.
    available_at: Option<f64>,
    /// Headers yt-dlp would send with the download.
    http_headers: Option<BTreeMap<String, String>>,
}

impl Stream {
    /// Whether a refusal may still end by itself: it's less than [`POLL_SLACK`] seconds past
    /// `available_at` (the address often works a few seconds before then).
    fn may_work_soon(&self) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        self.available_at.is_some_and(|at| now < at + POLL_SLACK)
    }

    /// The file's size in bytes: from yt-dlp, else the `clen` in the address.
    fn size(&self) -> Option<u64> {
        self.filesize.filter(|&s| s > 0).or_else(|| {
            let url = url::Url::parse(&self.url).ok()?;
            url.query_pairs().find(|(k, _)| k == "clen").and_then(|(_, v)| v.parse().ok())
        })
    }
}

impl Stream {
    /// The kind of address the file's address was given to (its `ip=`).
    fn family(&self) -> Family {
        let ip = url::Url::parse(&self.url).ok().and_then(|url| {
            url.query_pairs().find(|(k, _)| k == "ip").and_then(|(_, v)| v.parse::<IpAddr>().ok())
        });
        match ip {
            Some(IpAddr::V4(_)) => Family::V4,
            Some(IpAddr::V6(_)) => Family::V6,
            None => Family::Any,
        }
    }
}

/// A kind of internet address. YouTube's file addresses only work from the address that asked
/// for them, so a file asked for over IPv6 is refused (403) over IPv4 and the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Family {
    Any,
    V4,
    V6,
}

/// Looks names up the way Windows does, keeping only one kind of address.
struct OnlyFamily(Family);

impl reqwest::dns::Resolve for OnlyFamily {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let family = self.0;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| match family {
                    Family::Any => true,
                    Family::V4 => a.is_ipv4(),
                    Family::V6 => a.is_ipv6(),
                })
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no {family:?} address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn parse_stream(out: &[u8]) -> Result<Stream> {
    let text = String::from_utf8_lossy(out);
    let line = text.lines().map(str::trim).find(|l| l.starts_with('{')).ok_or_else(|| anyhow!("no details"))?;
    let stream: Stream = serde_json::from_str(line)?;
    if !stream.url.starts_with("https://") {
        bail!("no https address");
    }
    Ok(stream)
}

/// Where a video's audio file is (the address works for a few hours, from this computer only):
/// the quick way first when `quick` is set, else (or when that fails) the usual way.
async fn resolve(tools: &Tools, video_id: &str, quick: bool) -> Result<Stream> {
    if !valid_video_id(video_id) {
        bail!(GET_FAILED);
    }
    let what = format!("YouTube video {video_id}");
    let page = format!("{YOUTUBE}/watch?v={video_id}");
    let usual = ["--no-playlist", "-q", "-f", FORMAT, "--print", STREAM_FIELDS, page.as_str()];
    if quick {
        let quick: Vec<&str> = ["--extractor-args", QUICK_RESOLVE].into_iter().chain(usual).collect();
        let why = match run_ytdlp(tools, &quick, false, RESOLVE_TIMEOUT).await {
            Ok(json) => match parse_stream(json.as_bytes()) {
                Ok(stream) => return Ok(stream),
                Err(e) => format!("gave unreadable details ({e})"),
            },
            Err(e) => match quick_failure(e) {
                Ok(why) => why,
                Err(e) => return Err(e.into_user_error(&what, GET_FAILED)),
            },
        };
        log::info!("{what}: the quick way {why}; trying the usual way");
    }
    match run_ytdlp(tools, &usual, true, RESOLVE_TIMEOUT).await {
        Ok(json) => parse_stream(json.as_bytes()).map_err(|e| {
            log::warn!("yt-dlp gave unreadable details for video {video_id}: {e}");
            anyhow!(GET_FAILED)
        }),
        Err(RunError::Failed(stderr)) if is_gone(&stderr) => {
            log::info!("{what}: {}", tail(&stderr, 300));
            Err(Gone.into())
        }
        Err(e) => Err(e.into_user_error(&what, GET_FAILED)),
    }
}

/// A YouTube search: the quick way, else the usual way.
async fn search_youtube(tools: &Tools, query: &str) -> Result<Vec<Entry>> {
    let what = format!("YouTube search for {query:?}");
    let target = format!("ytsearch{SEARCH_RESULTS}:{query}");
    let quick = ["--extractor-args", QUICK_SEARCH, "-J", "--flat-playlist", target.as_str()];
    let why = match run_ytdlp(tools, &quick, false, SEARCH_TIMEOUT).await {
        Ok(json) => match parse_search(json.as_bytes()) {
            Ok(entries) if !entries.is_empty() => return Ok(entries),
            Ok(_) => "found nothing".to_owned(),
            Err(e) => format!("gave unreadable results ({e})"),
        },
        Err(e) => match quick_failure(e) {
            Ok(why) => why,
            Err(e) => return Err(e.into_user_error(&what, SEARCH_FAILED)),
        },
    };
    log::info!("{what}: the quick way {why}; trying the usual way");
    let json = run_ytdlp(tools, &quick[2..], true, SEARCH_TIMEOUT)
        .await
        .map_err(|e| e.into_user_error(&what, SEARCH_FAILED))?;
    parse_search(json.as_bytes()).map_err(|e| {
        log::warn!("{what} gave unreadable results: {e}");
        anyhow!(SEARCH_FAILED)
    })
}

/// Why the quick way failed, when the usual way may still work; else the error to give.
fn quick_failure(error: RunError) -> Result<String, RunError> {
    match error {
        RunError::Failed(stderr) if explain(&stderr).is_none() => Ok(format!("failed ({})", tail(&stderr, 300))),
        RunError::Io(e) => Ok(format!("failed ({e})")),
        e => Err(e),
    }
}

#[derive(Debug)]
enum FetchError {
    Status(u16),
    Net(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Status(code) => write!(f, "HTTP {code}"),
            FetchError::Net(e) => f.write_str(e),
        }
    }
}

/// Bytes `start..=end` of the file. The range goes in the address (`&range=`), which YouTube
/// doesn't slow down the way it does a Range header.
async fn fetch_range(http: &reqwest::Client, stream: &Stream, start: u64, end: u64) -> Result<Vec<u8>, FetchError> {
    let net = |e: reqwest::Error| FetchError::Net(e.without_url().to_string());
    let sep = if stream.url.contains('?') { '&' } else { '?' };
    let mut request = http.get(format!("{}{sep}range={start}-{end}", stream.url));
    for (name, value) in stream.http_headers.iter().flatten() {
        request = request.header(name.as_str(), value.as_str());
    }
    let response =
        request.header("Origin", YOUTUBE).header("Referer", format!("{YOUTUBE}/")).send().await.map_err(net)?;
    let status = response.status();
    if !status.is_success() {
        return Err(FetchError::Status(status.as_u16()));
    }
    let body = response.bytes().await.map_err(net)?;
    let asked = end - start + 1;
    if body.is_empty() || body.len() as u64 > asked {
        return Err(FetchError::Net(format!("{} bytes for a {asked} byte range", body.len())));
    }
    Ok(body.to_vec())
}

/// Whether the bytes start like an MP4 / m4a file (an `ftyp` box).
fn is_mp4(bytes: &[u8]) -> bool {
    bytes.get(4..8) == Some(b"ftyp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_youtube_wide_problems_stop_the_skipping() {
        assert!(is_service_wide(REFUSED) && is_service_wide(OFFLINE) && is_service_wide(NO_YTDLP));
        assert!(!is_service_wide(NOT_FOUND) && !is_service_wide(GET_FAILED) && !is_service_wide(TOO_SLOW));
    }
    use crate::model::ArtistRef;

    /// `yt-dlp -J --flat-playlist "ytsearch8:TV Girl - For You"` on 2026-10-25 (yt-dlp
    /// 2026.10.21), trimmed to the fields that matter.
    const SEARCH_SAMPLE: &str = r##"{"id": "TV Girl - For You", "title": "TV Girl - For You", "_type": "playlist", "entries": [
{"_type": "url", "ie_key": "Youtube", "id": "G2-_sP5v-Zo", "url": "https://www.youtube.com/watch?v=G2-_sP5v-Zo", "title": "TV Girl - For You", "duration": 217, "channel_id": "UCakexY9Khc2vKLgrf29svzQ", "channel": "TV Girl", "uploader": "TV Girl", "live_status": null},
{"_type": "url", "ie_key": "Youtube", "id": "J5-Tp5m7osU", "url": "https://www.youtube.com/watch?v=J5-Tp5m7osU", "title": "TV Girl - For You (Fan-made Music Video)", "duration": 218, "channel_id": "UCM2_rSDP-J8NnDPz2gRJXMg", "channel": "pulledoutpin", "uploader": "pulledoutpin", "live_status": null},
{"_type": "url", "ie_key": "Youtube", "id": "ibiuTdWg-ZU", "url": "https://www.youtube.com/watch?v=ibiuTdWg-ZU", "title": "For You - Tv Girl 1 hour ", "duration": 3647, "channel_id": "UCTAizwdveFIf9Vb_AiiI_5g", "channel": "ValCal", "uploader": "ValCal", "live_status": null},
{"_type": "url", "ie_key": "Youtube", "id": "e61uoknAC5M", "url": "https://www.youtube.com/watch?v=e61uoknAC5M", "title": "For You by Tv Girl | \u0645\u062a\u0631\u062c\u0645\u0629", "duration": 214, "channel_id": "UCWmairnDmgmqpGVPW9YfEOw", "channel": "21th", "uploader": "21th", "live_status": null},
{"_type": "url", "ie_key": "Youtube", "id": "Ud5CcM3Vo18", "url": "https://www.youtube.com/watch?v=Ud5CcM3Vo18", "title": "for you - tv girl (slowed)", "duration": 251, "channel_id": "UCOk_57mllpX4H5Tqgo6aRNA", "channel": "reunibliss", "uploader": "reunibliss", "live_status": null},
{"_type": "url", "ie_key": "Youtube", "id": "aDFmc2aZ26k", "url": "https://www.youtube.com/watch?v=aDFmc2aZ26k", "title": "for you - TV Girl", "duration": 216, "channel_id": "UCGXMqcVIRGmQyjQsbnUp47A", "channel": "music plus", "uploader": "music plus", "live_status": null},
{"_type": "url", "ie_key": "Youtube", "id": "wQh4ANXZLsc", "url": "https://www.youtube.com/watch?v=wQh4ANXZLsc", "title": "tv girl - For you (sped up/nightcore + reverb) \u0f04\u0618 \u02d1", "duration": 216, "channel_id": "UC7oE_-lIM6i-9WiJiqVr8iQ", "channel": "puppy", "uploader": "puppy", "live_status": null},
{"_type": "url", "ie_key": "Youtube", "id": "sfOwemFm5KM", "url": "https://www.youtube.com/watch?v=sfOwemFm5KM", "title": "TV Girl \"If You Want It\" | OFF THE AVENUE E95", "duration": 192, "channel_id": "UCJ0MDVTDD_8ryFEleEiZwsw", "channel": "offtheavenue", "uploader": "offtheavenue", "live_status": null}
], "extractor": "youtube:search", "_version": {"version": "2026.10.21"}}"##;

    fn track(uri: &str, name: &str, artists: &[&str], duration_ms: u32) -> TrackInfo {
        TrackInfo {
            uri: uri.to_owned(),
            name: name.to_owned(),
            artists: artists.iter().map(|a| ArtistRef { name: (*a).to_owned(), uri: String::new() }).collect(),
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    fn wanted(name: &str, artists: &[&str], secs: u32) -> Wanted {
        Wanted::of(&track("spotify:track:x", name, artists, secs * 1000))
    }

    fn entry(id: &str, title: &str, secs: f64, channel: &str) -> Entry {
        Entry {
            id: id.to_owned(),
            title: Some(title.to_owned()),
            duration: Some(secs),
            channel: Some(channel.to_owned()),
            uploader: None,
            live_status: None,
        }
    }

    fn ids(ranked: &[(Candidate, bool)]) -> Vec<&str> {
        ranked.iter().map(|(c, _)| c.video_id.as_str()).collect()
    }

    fn best(ranked: &[(Candidate, bool)]) -> Option<&str> {
        ranked.iter().find(|(_, fits)| *fits).map(|(c, _)| c.video_id.as_str())
    }

    /// Test files go under `TRUFFLE_TEST_DIR`, else the temp folder.
    fn test_dir(name: &str) -> PathBuf {
        let base = crate::config::test_root();
        let dir = base.join(format!("test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_a_real_search() {
        let entries = parse_search(SEARCH_SAMPLE.as_bytes()).unwrap();
        assert_eq!(entries.len(), 8);
        let first = &entries[0];
        assert_eq!(first.id, "G2-_sP5v-Zo");
        assert_eq!(first.title.as_deref(), Some("TV Girl - For You"));
        assert_eq!(first.duration, Some(217.0));
        assert_eq!(channel_of(first), "TV Girl");
        assert!(entries[3].title.as_deref().unwrap().ends_with("\u{645}\u{62a}\u{631}\u{62c}\u{645}\u{629}"));
    }

    #[test]
    fn parsing_skips_streams_and_odd_entries() {
        let json = br#"{"entries": [
            {"id": "aaaaaaaaaaa", "title": "Song", "duration": 200.5, "channel": null, "uploader": "Someone"},
            {"id": "bbbbbbbbbbb", "title": "Live now", "duration": null, "live_status": "is_live"},
            {"id": "not an id", "title": "Odd"},
            {"title": "No id"},
            null
        ]}"#;
        let entries = parse_search(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].duration, Some(200.5));
        assert_eq!(channel_of(&entries[0]), "Someone");
        assert!(parse_search(b"{}").unwrap().is_empty());
        assert!(parse_search(br#"{"entries": null}"#).unwrap().is_empty());
        assert!(parse_search(b"ERROR: no").is_err());
    }

    #[test]
    fn real_search_picks_the_official_upload() {
        let entries = parse_search(SEARCH_SAMPLE.as_bytes()).unwrap();
        let ranked = rank(&wanted("For You", &["TV Girl"], 217), &entries);
        assert_eq!(best(&ranked), Some("G2-_sP5v-Zo"));
        let order = ids(&ranked);
        // The sped-up one has the right length but comes after every plain upload.
        let sped = order.iter().position(|id| *id == "wQh4ANXZLsc").unwrap();
        for plain in ["G2-_sP5v-Zo", "J5-Tp5m7osU", "aDFmc2aZ26k", "e61uoknAC5M"] {
            assert!(order.iter().position(|id| *id == plain).unwrap() < sped, "{plain} after the sped-up one");
        }
        // The 1 hour loop, the slowed one and the other song don't fit the length.
        for wrong in ["ibiuTdWg-ZU", "Ud5CcM3Vo18", "sfOwemFm5KM"] {
            let (_, fits) = ranked.iter().find(|(c, _)| c.video_id == wrong).unwrap();
            assert!(!fits, "{wrong} fits");
        }
        assert!(order[..5].iter().all(|id| !["ibiuTdWg-ZU", "Ud5CcM3Vo18", "sfOwemFm5KM"].contains(id)));
    }

    #[test]
    fn topic_channel_beats_variants() {
        let entries = [
            entry("live0000000", "TV Girl - For You (Live at KEXP)", 218.0, "KEXP"),
            entry("sped0000000", "For You (Sped Up)", 216.0, "sped up songs"),
            entry("hour0000000", "TV Girl - For You [1 HOUR]", 3600.0, "loops"),
            entry("video000000", "TV Girl - For You (Official Video)", 221.0, "TV Girl"),
            entry("topic000000", "For You", 217.0, "TV Girl - Topic"),
            entry("cover000000", "For You (cover)", 217.0, "Some Band"),
        ];
        let ranked = rank(&wanted("For You", &["TV Girl"], 217), &entries);
        assert_eq!(ids(&ranked)[..2], ["topic000000", "video000000"]);
        assert_eq!(ranked.last().unwrap().0.video_id, "hour0000000");
        assert!(!ranked.last().unwrap().1);
    }

    #[test]
    fn length_is_the_strongest_sign() {
        // An exact-length upload by someone else beats the artist's own video that runs 9 s long.
        let entries = [
            entry("video000000", "TV Girl - For You (Official Video)", 226.0, "TV Girl"),
            entry("upload00000", "TV Girl - For You", 217.0, "someone"),
        ];
        let ranked = rank(&wanted("For You", &["TV Girl"], 217), &entries);
        assert_eq!(best(&ranked), Some("upload00000"));
    }

    #[test]
    fn nothing_fits_when_every_length_is_off() {
        let mut no_length = entry("nolength000", "TV Girl - For You", 0.0, "TV Girl");
        no_length.duration = None;
        let entries = [
            entry("long0000000", "TV Girl - For You", 230.0, "TV Girl"),
            entry("short000000", "TV Girl - For You", 204.0, "TV Girl - Topic"),
            no_length,
        ];
        let ranked = rank(&wanted("For You", &["TV Girl"], 217), &entries);
        assert_eq!(best(&ranked), None);
        assert_eq!(ranked.len(), 3);
        // Within 12 s it fits.
        let ranked = rank(&wanted("For You", &["TV Girl"], 217), &[entry("near0000000", "For You", 229.0, "x")]);
        assert_eq!(best(&ranked), Some("near0000000"));
    }

    #[test]
    fn a_live_song_wants_the_live_video() {
        let entries = [
            entry("studio00000", "Artist - Song", 200.0, "Artist - Topic"),
            entry("live0000000", "Artist - Song (Live)", 241.0, "Artist"),
        ];
        let ranked = rank(&wanted("Song - Live", &["Artist"], 240), &entries);
        assert_eq!(best(&ranked), Some("live0000000"));
        // And a studio song passes over a live video of the same length.
        let entries = [
            entry("live0000000", "Artist - Song (Live)", 200.0, "Artist"),
            entry("studio00000", "Artist - Song", 203.0, "fan uploads"),
        ];
        let ranked = rank(&wanted("Song", &["Artist"], 200), &entries);
        assert_eq!(best(&ranked), Some("studio00000"));
    }

    #[test]
    fn other_mixes_and_tour_recordings_lose() {
        let entries = [
            entry("mix00000000", "Here Comes The Sun (2019 Mix)", 186.0, "The Beatles"),
            entry("tour0000000", "The Beatles - Here Comes The Sun (World Tour 1966)", 185.0, "The Beatles"),
            entry("lyrics00000", "The Beatles - Here Comes The Sun (Lyrics)", 185.0, "7clouds Rock"),
        ];
        let ranked = rank(&wanted("Here Comes The Sun - Remastered 2009", &["The Beatles"], 185), &entries);
        assert_eq!(ids(&ranked), ["lyrics00000", "mix00000000", "tour0000000"]);
    }

    #[test]
    fn a_version_word_in_the_song_name_still_catches_other_versions() {
        // "Live Forever": the live recording is 1 s off, the studio upload 4 s off.
        let entries = [
            entry("live0000000", "Oasis - Live Forever (Live at Knebworth 1996)", 277.0, "Oasis"),
            entry("studio00000", "Oasis - Live Forever", 280.0, "some uploader"),
        ];
        let ranked = rank(&wanted("Live Forever - Remastered", &["Oasis"], 276), &entries);
        assert_eq!(best(&ranked), Some("studio00000"));
        // The song that is the live recording still wants it.
        let ranked = rank(&wanted("Live Forever - Live at Knebworth 1996", &["Oasis"], 276), &entries);
        assert_eq!(best(&ranked), Some("live0000000"));
        // The band Live: their name alone doesn't make an upload live, a second "Live" does.
        let entries = [
            entry("live0000000", "Live - Lightning Crashes (Live at Woodstock 1999)", 326.0, "Live"),
            entry("studio00000", "Live - Lightning Crashes", 330.0, "Live"),
        ];
        let ranked = rank(&wanted("Lightning Crashes", &["Live"], 325), &entries);
        assert_eq!(best(&ranked), Some("studio00000"));
        let entries = [
            entry("cover000000", "Cover Me (Bruce Springsteen cover)", 207.0, "a band"),
            entry("studio00000", "Bruce Springsteen - Cover Me", 209.0, "Bruce Springsteen"),
        ];
        let ranked = rank(&wanted("Cover Me", &["Bruce Springsteen"], 206), &entries);
        assert_eq!(best(&ranked), Some("studio00000"));
    }

    #[test]
    fn cyrillic_titles_match_in_any_case() {
        let entries = [
            entry("translit000", "Kino - Gruppa Krovi", 287.0, "someone"),
            entry("live0000000", "КИНО - ГРУППА КРОВИ (Live 1988)", 290.0, "archive"),
            entry("topic000000", "Группа крови", 289.0, "Кино - Topic"),
            entry("upper000000", "КИНО — ГРУППА КРОВИ", 287.0, "КИНО"),
        ];
        let ranked = rank(&wanted("Группа крови", &["Кино"], 287), &entries);
        assert_eq!(ids(&ranked)[..2], ["upper000000", "topic000000"]);
        assert!(!ranked.iter().find(|(c, _)| c.video_id == "live0000000").unwrap().1);
    }

    #[test]
    fn other_versions_are_never_picked_on_their_own() {
        let entries = [
            entry("karaoke0000", "Artist - Song (Karaoke Version)", 200.0, "Sing King"),
            entry("sped0000000", "Artist - Song (sped up)", 201.0, "speedy"),
            entry("video000000", "Artist - Song (Official Video)", 260.0, "Artist"),
        ];
        let ranked = rank(&wanted("Song", &["Artist"], 200), &entries);
        assert_eq!(best(&ranked), None);
        // Still offered for "Wrong song?", ahead of the wrong length.
        assert_eq!(ids(&ranked)[..2], ["karaoke0000", "sped0000000"]);
        // A mix of the same recording may still play.
        let entries = [entry("mix00000000", "Artist - Song (2019 Mix)", 200.0, "Artist")];
        assert_eq!(best(&rank(&wanted("Song", &["Artist"], 200), &entries)), Some("mix00000000"));
    }

    #[test]
    fn ties_keep_youtubes_order() {
        let entries = [entry("first000000", "Song", 200.0, "a"), entry("second00000", "Song", 200.0, "b")];
        let ranked = rank(&wanted("Song", &["Nobody"], 200), &entries);
        assert_eq!(ids(&ranked), ["first000000", "second00000"]);
        let ranked = rank(&wanted("Song", &["Nobody"], 200), &[entries[1].clone(), entries[0].clone()]);
        assert_eq!(ids(&ranked), ["second00000", "first000000"]);
    }

    #[test]
    fn unknown_spotify_length_lets_everything_fit() {
        let entries =
            [entry("other000000", "Song", 100.0, "x"), entry("artist00000", "Artist - Song", 500.0, "Artist")];
        let ranked = rank(&wanted("Song", &["Artist"], 0), &entries);
        assert_eq!(best(&ranked), Some("artist00000"));
        assert!(ranked.iter().all(|(_, fits)| *fits));
    }

    #[test]
    fn duplicate_results_show_once() {
        let entries = [entry("same0000000", "Song", 200.0, "a"), entry("same0000000", "Song", 200.0, "a")];
        assert_eq!(rank(&wanted("Song", &["A"], 200), &entries).len(), 1);
    }

    #[test]
    fn titles_lose_credits_and_release_notes() {
        assert_eq!(clean_title("Here Comes The Sun - Remastered 2009"), "Here Comes The Sun");
        assert_eq!(clean_title("Hey Jude - 2015 Remaster"), "Hey Jude");
        assert_eq!(clean_title("Song (feat. Someone Else)"), "Song");
        assert_eq!(clean_title("Song [feat. X] - Radio Edit"), "Song - Radio Edit");
        assert_eq!(clean_title("Song (with Friend) (Remastered)"), "Song");
        assert_eq!(clean_title("Let It Go - From \"Frozen\"/Soundtrack Version"), "Let It Go");
        assert_eq!(clean_title("Song - Live"), "Song - Live");
        assert_eq!(clean_title("Song (Acoustic)"), "Song (Acoustic)");
        assert_eq!(clean_title("Song - Mono Version - Bonus Track"), "Song");
        assert_eq!(clean_title("Song (Unclosed"), "Song (Unclosed");
        assert_eq!(clean_title("(feat. X)"), "(feat. X)");
        assert_eq!(clean_title("  Spaced   Out  "), "Spaced Out");
    }

    #[test]
    fn search_query_is_artists_then_clean_title() {
        assert_eq!(search_query(&wanted("For You", &["TV Girl"], 217)), "TV Girl - For You");
        assert_eq!(search_query(&wanted("Song (feat. C)", &["A", "B", "C", "D"], 1)), "A B C - Song");
        assert_eq!(search_query(&wanted("Episode 12", &[], 1)), "Episode 12");
    }

    #[test]
    fn video_ids_are_checked() {
        assert!(valid_video_id("G2-_sP5v-Zo"));
        assert!(!valid_video_id("G2-_sP5v-Z"));
        assert!(!valid_video_id("G2-_sP5v-Zo&x"));
        assert!(!valid_video_id("--exec=calc"));
    }

    #[test]
    fn stream_details_parse() {
        let out = br#"{"id": "G2-_sP5v-Zo", "title": "TV Girl - For You", "duration": 217, "url": "https://rr1.googlevideo.com/videoplayback?itag=140&clen=3511032&dur=216.897", "filesize": null, "http_headers": {"User-Agent": "Mozilla/5.0"}}
"#;
        let stream = parse_stream(out).unwrap();
        assert_eq!(stream.size(), Some(3_511_032));
        assert_eq!(stream.duration.map(secs_to_ms), Some(217_000));
        assert_eq!(stream.http_headers.as_ref().unwrap()["User-Agent"], "Mozilla/5.0");
        assert_eq!(stream.family(), Family::Any);
        assert!(parse_stream(b"").is_err());
        assert!(parse_stream(br#"{"url": "file:///c:/x"}"#).is_err());
        let at = |ip: &str| Stream {
            url: format!("https://rr1.googlevideo.com/videoplayback?expire=1&ip={ip}&id=o-A"),
            title: None,
            duration: None,
            filesize: None,
            available_at: None,
            http_headers: None,
        };
        assert_eq!(at("2001%3A4644%3A9dc4%3A0%3Ad7ab%3Aa699%3A37c8%3Aadb6").family(), Family::V6);
        assert_eq!(at("193.212.4.219").family(), Family::V4);
        assert_eq!(at("nonsense").family(), Family::Any);
    }

    #[test]
    fn gone_videos_and_common_errors_are_told_apart() {
        assert!(is_gone("ERROR: [youtube] abc: Video unavailable. This video has been removed by the uploader"));
        assert!(is_gone("ERROR: [youtube] abc: Private video. Sign in if you've been granted access"));
        // yt-dlp 2026.10.21, both ways, for a video id that isn't there.
        assert!(is_gone("ERROR: [youtube] aaaaaaaaaaa: This video is unavailable"));
        assert!(!is_gone("ERROR: [youtube] abc: Sign in to confirm you're not a bot"));
        // YouTube's answer when it's limiting this computer: every video, so not gone.
        let limited = "ERROR: [youtube] abc: Video unavailable. This content isn't available, try again later.";
        assert!(!is_gone(limited));
        assert_eq!(explain(limited), Some(REFUSED));
        assert_eq!(explain("ERROR: [youtube] abc: Sign in to confirm you're not a bot"), Some(REFUSED));
        let offline = "ERROR: Unable to download API page: <urlopen error [Errno 11001] getaddrinfo failed>";
        assert_eq!(explain(offline), Some(OFFLINE));
        assert_eq!(explain("ERROR: something new"), None);
        assert_eq!(tail("abcdé", 2), "é");
    }

    #[test]
    fn mp4_files_are_recognised() {
        assert!(is_mp4(b"\0\0\0\x18ftypdash"));
        assert!(!is_mp4(b"<html>"));
        assert!(!is_mp4(b""));
    }

    #[test]
    fn data_dir_is_the_parent_of_cache() {
        let root = Path::new(r"C:\Users\u\AppData\Local\truffle");
        assert_eq!(data_dir(&root.join("cache")), Some(root.to_path_buf()));
        assert_eq!(data_dir(&root.join("cache").join("youtube")), Some(root.to_path_buf()));
        assert_eq!(data_dir(Path::new(r"C:\elsewhere")), None);
    }

    #[test]
    fn programs_are_found_in_order() {
        let dir = test_dir("tools");
        let (app, data, path1, path2, home) =
            (dir.join("app"), dir.join("data"), dir.join("path1"), dir.join("path2"), dir.join("home"));
        for d in [&app, &data, &path1, &path2, &home.join("Downloads")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let touch = |p: PathBuf| std::fs::write(p, b"").unwrap();
        let places = || Places {
            own: vec![app.clone(), data.clone()],
            path: vec![path1.clone(), path2.clone()],
            home: Some(home.clone()),
            program_files: None,
        };

        assert_eq!(find_tools(&places()).unwrap_err(), NO_YTDLP);
        touch(home.join("Downloads").join(exe("yt-dlp")));
        assert_eq!(find_tools(&places()).unwrap_err(), NO_JS);

        touch(path2.join(exe("qjs")));
        let tools = find_tools(&places()).unwrap();
        assert_eq!(tools.ytdlp, home.join("Downloads").join(exe("yt-dlp")));
        assert_eq!(tools.js, Some(("quickjs".to_owned(), path2.join(exe("qjs")))));

        // Deno beats QuickJS, Node beats both, wherever they are.
        touch(home.join("Downloads").join(exe("deno")));
        assert_eq!(find_tools(&places()).unwrap().js.unwrap().0, "deno");
        touch(path1.join(exe("node")));
        assert_eq!(find_tools(&places()).unwrap().js, Some(("node".to_owned(), path1.join(exe("node")))));

        // yt-dlp: on PATH beats Downloads, the data folder beats PATH, beside truffle.exe beats all.
        touch(path2.join(exe("yt-dlp")));
        assert_eq!(find_tools(&places()).unwrap().ytdlp, path2.join(exe("yt-dlp")));
        touch(data.join(exe("yt-dlp")));
        assert_eq!(find_tools(&places()).unwrap().ytdlp, data.join(exe("yt-dlp")));
        touch(app.join(exe("yt-dlp")));
        assert_eq!(find_tools(&places()).unwrap().ytdlp, app.join(exe("yt-dlp")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helpers_versions_are_read_from_what_they_print() {
        assert_eq!(version_in("2026.10.21\n").as_deref(), Some("2026.10.21"));
        assert_eq!(version_in("v24.19.0\r\n").as_deref(), Some("24.19.0"));
        assert_eq!(version_in("deno 2.1.4 (stable, release, x86_64-pc-windows-msvc)\nv8 13.0\n").as_deref(), Some("2.1.4"));
        assert_eq!(version_in("QuickJS version 2024-01-13").as_deref(), Some("2024-01-13"));
        assert_eq!(version_in("\n  \n"), None);
        assert_eq!(version_in("no version here"), None);
        assert_eq!(runtime_name("node"), "Node.js");
        assert_eq!(runtime_name("deno"), "Deno");
    }

    #[test]
    fn matches_survive_a_restart() {
        let dir = test_dir("matches");
        let uri = "spotify:track:abc";
        let yt = YouTube::new(dir.clone(), None);
        let found = Candidate {
            video_id: "G2-_sP5v-Zo".to_owned(),
            title: "TV Girl - For You".to_owned(),
            channel: "TV Girl".to_owned(),
            duration_ms: 217_000,
            score: 76.0,
        };
        yt.with_matches(|m| m.found.insert(uri.to_owned(), Saved::from(&found)));
        yt.save_matches();
        assert_eq!(YouTube::new(dir.clone(), None).remembered(uri), Some(found.clone()));

        // A pick wins over the search's match, also after a restart.
        yt.choose(uri, "aDFmc2aZ26k");
        yt.choose("spotify:track:other", "not a video id");
        let again = YouTube::new(dir.clone(), None);
        let picked = again.remembered(uri).unwrap();
        assert_eq!(picked.video_id, "aDFmc2aZ26k");
        assert_eq!(again.remembered("spotify:track:other"), None);
        // A pick that turned out to be gone gives way, also after a restart.
        again.replace_gone(uri, &["aDFmc2aZ26k".to_owned()], &found);
        again.save_matches();
        assert_eq!(YouTube::new(dir.clone(), None).remembered(uri), Some(found.clone()));

        // A broken file is ignored, then replaced.
        std::fs::write(dir.join(MATCH_FILE), b"{\"picked\": [1, 2").unwrap();
        let broken = YouTube::new(dir.clone(), None);
        assert_eq!(broken.remembered(uri), None);
        broken.choose(uri, "G2-_sP5v-Zo");
        assert_eq!(YouTube::new(dir.clone(), None).remembered(uri).unwrap().video_id, "G2-_sP5v-Zo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_at_the_same_time_keep_every_match() {
        let dir = test_dir("concurrent");
        let yt = YouTube::new(dir.clone(), None);
        let ids = ["G2-_sP5v-Zo", "J5-Tp5m7osU", "ibiuTdWg-ZU", "e61uoknAC5M", "Ud5CcM3Vo18", "aDFmc2aZ26k"];
        std::thread::scope(|s| {
            for (n, id) in ids.iter().enumerate() {
                let yt = yt.clone();
                s.spawn(move || yt.choose(&format!("spotify:track:{n}"), id));
            }
        });
        let again = YouTube::new(dir.clone(), None);
        for (n, id) in ids.iter().enumerate() {
            assert_eq!(again.remembered(&format!("spotify:track:{n}")).unwrap().video_id, *id);
        }
        let leftovers = std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().is_ok_and(|e| e.path() != dir.join(MATCH_FILE)));
        assert_eq!(leftovers.count(), 0, "temp files left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gone_videos_give_way_to_the_next_fitting_one() {
        let dir = test_dir("gone");
        let yt = YouTube::new(dir.clone(), None);
        let song = track("spotify:track:abc", "For You", &["TV Girl"], 217_000);
        let entries = parse_search(SEARCH_SAMPLE.as_bytes()).unwrap();
        lock(&yt.inner.searches).push_back(("TV Girl - For You".to_owned(), entries));
        let first = yt.find(&song).await.unwrap().video_id;
        assert_eq!(first, "G2-_sP5v-Zo");
        // Each gone video is passed over, never going back to an earlier one.
        let mut gone = vec![first];
        let second = yt.next_match(&song, &gone).await.unwrap().video_id;
        gone.push(second.clone());
        let third = yt.next_match(&song, &gone).await.unwrap();
        assert!(!gone.contains(&third.video_id));
        // Until nothing that fits is left.
        let all: Vec<String> = rank(&Wanted::of(&song), &parse_search(SEARCH_SAMPLE.as_bytes()).unwrap())
            .into_iter()
            .filter(|(_, fits)| *fits)
            .map(|(c, _)| c.video_id)
            .collect();
        assert_eq!(yt.next_match(&song, &all).await.unwrap_err().to_string(), NOT_FOUND);
        // The one that plays is remembered in place of the gone pick.
        yt.choose(&song.uri, &second);
        yt.replace_gone(&song.uri, &gone, &third);
        assert_eq!(yt.find(&song).await.unwrap().video_id, third.video_id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pick_takes_its_details_from_the_search() {
        let dir = test_dir("pick");
        let yt = YouTube::new(dir.clone(), None);
        let entries = parse_search(SEARCH_SAMPLE.as_bytes()).unwrap();
        lock(&yt.inner.searches).push_back(("TV Girl - For You".to_owned(), entries));
        yt.choose("spotify:track:abc", "aDFmc2aZ26k");
        let picked = yt.remembered("spotify:track:abc").unwrap();
        assert_eq!(picked.title, "for you - TV Girl");
        assert_eq!(picked.channel, "music plus");
        assert_eq!(picked.duration_ms, 216_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_audio_keeps_the_last_few() {
        let dir = test_dir("recent");
        let yt = YouTube::new(dir.clone(), None);
        let audio =
            |id: &str| Audio { video_id: id.to_owned(), title: String::new(), bytes: vec![1, 2, 3], duration_ms: None };
        for n in 0..=RECENT_AUDIO {
            yt.keep_audio(&format!("u{n}"), &audio(&format!("v{n}")));
        }
        assert!(yt.recent_audio("u0").is_none());
        assert_eq!(yt.recent_audio("u1").unwrap().video_id, "v1");
        // u1 was just used, so u2 goes first.
        yt.keep_audio("new", &audio("vnew"));
        assert!(yt.recent_audio("u2").is_none());
        assert!(yt.recent_audio("u1").is_some());
        // A new pick drops the old audio.
        yt.choose("u1", "G2-_sP5v-Zo");
        assert!(yt.recent_audio("u1").is_none());
        // Also audio of the old video that was still on its way when the pick came.
        yt.keep_audio("u1", &audio("v1"));
        assert!(yt.recent_audio("u1").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A local stand-in for YouTube's file server, as measured on 2026-10-25: `&range=a-b` in the
    /// address; a range past the end gives the rest of the file; a start at or past the end
    /// gives 400.
    async fn file_server(file: Vec<u8>) -> String {
        file_server_refusing(file, 0).await.0
    }

    /// [`file_server`] that refuses (403) the first `refusals` requests; and the number of
    /// requests so far.
    async fn file_server_refusing(file: Vec<u8>, refusals: usize) -> (String, Arc<AtomicUsize>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/videoplayback?itag=140", listener.local_addr().unwrap());
        let file = Arc::new(file);
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let file = file.clone();
                let refused = counted.fetch_add(1, Ordering::SeqCst) < refusals;
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        head.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let range = head
                        .split_whitespace()
                        .nth(1)
                        .and_then(|path| path.split(['?', '&']).find_map(|p| p.strip_prefix("range=")))
                        .and_then(|r| r.split_once('-'))
                        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));
                    let (status, body) = match range {
                        _ if refused => ("403 Forbidden", &file[..0]),
                        Some((a, b)) if a < file.len() && a <= b => ("200 OK", &file[a..file.len().min(b + 1)]),
                        _ => ("400 Bad Request", &file[..0]),
                    };
                    let reply =
                        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (url, requests)
    }

    /// A made-up m4a file of `len` bytes.
    fn m4a(len: usize) -> Vec<u8> {
        let mut file: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        file[..8].copy_from_slice(b"\0\0\0\x18ftyp");
        file
    }

    fn local_stream(url: String, filesize: Option<u64>) -> Stream {
        Stream { url, title: None, duration: None, filesize, available_at: None, http_headers: None }
    }

    #[tokio::test]
    async fn downloads_in_pieces_to_the_last_byte() {
        let dir = test_dir("download");
        let yt = YouTube::new(dir.clone(), None);
        let tools = Tools { ytdlp: PathBuf::from("unused"), js: None };
        let len = 2 * CHUNK_BYTES as usize + 1;
        let file = m4a(len);
        let url = file_server(file.clone()).await;
        let (got, size) = yt.download(&tools, "G2-_sP5v-Zo", local_stream(url, Some(len as u64))).await.unwrap();
        assert_eq!(size, Some(len as u64));
        assert_eq!(got.len(), len);
        assert!(got == file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn downloads_without_a_known_size() {
        let dir = test_dir("download-unknown");
        let yt = YouTube::new(dir.clone(), None);
        let tools = Tools { ytdlp: PathBuf::from("unused"), js: None };
        // The last piece comes up short; and a file of exactly two pieces ends at a 400.
        for len in [3_511_032, 2 * CHUNK_BYTES as usize] {
            let file = m4a(len);
            let url = file_server(file.clone()).await;
            let (got, size) = yt.download(&tools, "G2-_sP5v-Zo", local_stream(url, None)).await.unwrap();
            assert_eq!(size, None);
            assert_eq!(got.len(), len);
            assert!(got == file);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn unix_now() -> f64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
    }

    #[tokio::test]
    async fn a_refused_address_is_asked_again_until_it_works() {
        let dir = test_dir("poll");
        let yt = YouTube::new(dir.clone(), None);
        let tools = Tools { ytdlp: PathBuf::from("unused"), js: None };
        let file = m4a(300_000);
        // The address works a moment before `available_at`, as YouTube's do: no new address needed.
        let (url, requests) = file_server_refusing(file.clone(), 3).await;
        let mut stream = local_stream(url, Some(file.len() as u64));
        stream.available_at = Some(unix_now() + 0.5);
        let started = Instant::now();
        let (got, _) = yt.download(&tools, "G2-_sP5v-Zo", stream).await.unwrap();
        assert!(got == file);
        assert_eq!(requests.load(Ordering::SeqCst), 4);
        assert!(started.elapsed() >= POLL_EVERY * 3, "asked again too soon: {:?}", started.elapsed());
        // Well past `available_at`, a refusal asks for a new address at once (here yt-dlp can't start).
        let (url, requests) = file_server_refusing(file.clone(), 1).await;
        let mut stream = local_stream(url, Some(file.len() as u64));
        stream.available_at = Some(unix_now() - POLL_SLACK - 1.0);
        let err = yt.download(&tools, "G2-_sP5v-Zo", stream).await.unwrap_err();
        assert_eq!(err.to_string(), NO_START);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Windows command line through cmd.exe.
    #[cfg(windows)]
    fn shell(line: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/C").raw_arg(line);
        cmd
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_first_json_line_comes_before_the_program_ends() {
        // Prints a line, then the JSON, then takes about 2 s to end.
        let started = Instant::now();
        let line = first_json_line(shell("echo [youtube] working& echo {\"id\": 1}& ping -n 3 127.0.0.1 >nul"), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(line, "{\"id\": 1}");
        assert!(started.elapsed() < Duration::from_millis(1500), "waited for the end: {:?}", started.elapsed());
        // No JSON: what it wrote about the error.
        match first_json_line(shell("echo ERROR: Video unavailable 1>&2& exit 1"), Duration::from_secs(10)).await {
            Err(RunError::Failed(stderr)) => assert!(stderr.contains("Video unavailable"), "{stderr:?}"),
            other => panic!("{other:?}"),
        }
        // Ended well without JSON.
        assert_eq!(first_json_line(shell("echo nothing"), Duration::from_secs(10)).await.unwrap(), "");
        // Too slow.
        let started = Instant::now();
        let slow = first_json_line(shell("ping -n 4 127.0.0.1 >nul"), Duration::from_millis(300)).await;
        assert!(matches!(slow, Err(RunError::Timeout(_))), "{slow:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_failing_program_is_given_up_on_in_time() {
        // It fails at once, but what it started keeps its error output open for about 5 s.
        let started = Instant::now();
        let run = first_json_line(
            shell("start /b ping -n 6 127.0.0.1 >nul& echo ERROR: x 1>&2& exit 1"),
            Duration::from_millis(1000),
        )
        .await;
        assert!(run.is_err(), "{run:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "waited {:?}", started.elapsed());
    }

    /// A stand-in for yt-dlp (a batch file): `quick` runs when it is asked the quick way (visionOS
    /// client, or a search without the web page), else `usual` (lines of batch commands). Every
    /// run's arguments are added to `calls.txt` beside it.
    #[cfg(windows)]
    fn fake_ytdlp(dir: &Path, quick: &[&str], usual: &[&str]) -> Tools {
        std::fs::create_dir_all(dir).unwrap();
        let script = format!(
            "@echo off\r\n>>\"%~dp0calls.txt\" echo %*\r\necho %* | findstr /C:visionos /C:youtubetab >nul && goto quick\r\n{}\r\nexit /b 0\r\n:quick\r\n{}\r\nexit /b 0\r\n",
            usual.join("\r\n"),
            quick.join("\r\n")
        );
        let path = dir.join("yt-dlp.cmd");
        std::fs::write(&path, script).unwrap();
        Tools { ytdlp: path, js: None }
    }

    #[cfg(windows)]
    fn calls(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("calls.txt")).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_quick_way_falls_back_to_the_usual_way() {
        let dir = test_dir("fallback");
        let id = "G2-_sP5v-Zo";
        let usual_ok = [
            "echo WARNING: [youtube] printed where the details go",
            "echo [youtube] G2-_sP5v-Zo: Downloading player",
            "echo {\"id\": \"G2-_sP5v-Zo\", \"url\": \"https://127.0.0.1:1/usual\"}",
        ];
        // Age-restricted: the visionOS client asks to sign in, the usual clients play it (seen on
        // 2026-10-25); and a missing format. The usual way, past its other lines.
        let age = ["echo ERROR: [youtube] x: Sign in to confirm your age. This video may be inappropriate 1>&2", "exit /b 1"];
        let format = ["echo ERROR: [youtube] x: Requested format is not available 1>&2", "exit /b 1"];
        for (name, quick) in [("age-quick", age), ("format", format)] {
            let at = dir.join(name);
            let tools = fake_ytdlp(&at, &quick, &usual_ok);
            let stream = resolve(&tools, id, true).await.unwrap();
            assert!(stream.url.ends_with("/usual"), "{}", stream.url);
            let made = calls(&at);
            assert_eq!(made.len(), 2, "{made:?}");
            assert!(made[0].contains("visionos") && !made[1].contains("visionos"), "{made:?}");
        }
        // Ended well without details: the usual way too.
        let at = dir.join("silent");
        let tools = fake_ytdlp(&at, &["exit /b 0"], &usual_ok);
        assert!(resolve(&tools, id, true).await.unwrap().url.ends_with("/usual"));
        assert_eq!(calls(&at).len(), 2);
        // Refused: not asked again the usual way.
        let at = dir.join("refused");
        let tools = fake_ytdlp(&at, &["echo ERROR: [youtube] x: Sign in to confirm you're not a bot 1>&2", "exit /b 1"], &usual_ok);
        assert_eq!(resolve(&tools, id, true).await.unwrap_err().to_string(), REFUSED);
        assert_eq!(calls(&at).len(), 1);
        // Refused both ways for age: gone (the next match is tried).
        let at = dir.join("age");
        let tools = fake_ytdlp(&at, &age, &age);
        assert!(resolve(&tools, id, true).await.unwrap_err().is::<Gone>());
        assert_eq!(calls(&at).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_refused_quick_address_is_renewed_the_usual_way() {
        let dir = test_dir("renew");
        let yt = YouTube::new(dir.clone(), None);
        // Where the new addresses point: a server that hangs up at once.
        let closer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closer.local_addr().unwrap().port();
        tokio::spawn(async move { while closer.accept().await.is_ok() {} });
        let quick = format!("echo {{\"url\": \"https://127.0.0.1:{port}/quick\"}}");
        let usual = format!("echo {{\"url\": \"https://127.0.0.1:{port}/usual\"}}");
        let tools = fake_ytdlp(&dir.join("fake"), &[&quick], &[&usual]);
        // The quick way's address (no wait) is refused for good.
        let file = m4a(300_000);
        let (url, _) = file_server_refusing(file.clone(), usize::MAX).await;
        let stream = local_stream(url, Some(file.len() as u64));
        assert!(yt.download(&tools, "G2-_sP5v-Zo", stream).await.is_err());
        let made = calls(&dir.join("fake"));
        assert_eq!(made.len(), 1, "{made:?}");
        assert!(!made[0].contains("visionos"), "asked the quick way again: {made:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn called_off_background_work_starts_no_yt_dlp() {
        let dir = test_dir("called-off");
        let tools = fake_ytdlp(&dir.join("fake"), &[], &["echo {\"id\": 1}"]);
        let run = |on: Option<bool>| {
            let tools = tools.clone();
            async move {
                // Generous: run at low priority, it can wait long while a build keeps every core busy.
                let run = run_ytdlp(&tools, &["x"], false, Duration::from_secs(120));
                match on {
                    Some(on) => BACKGROUND.scope(Arc::new(AtomicBool::new(on)), run).await,
                    None => run.await,
                }
            }
        };
        assert!(matches!(run(Some(false)).await, Err(RunError::CalledOff)));
        assert!(calls(&dir.join("fake")).is_empty());
        assert_eq!(run(Some(true)).await.unwrap(), "{\"id\": 1}");
        // A song to play never waits on the switches.
        assert_eq!(run(None).await.unwrap(), "{\"id\": 1}");
        assert_eq!(calls(&dir.join("fake")).len(), 2);

        // The switches: on while there is something to fetch or search ahead.
        let yt = YouTube::new(dir.clone(), None);
        *lock(&yt.inner.tools) = Some((Instant::now(), Ok(tools.clone())));
        // (Paused, so nothing runs here.)
        yt.note_problem(&anyhow!(REFUSED));
        yt.fetch_ahead(vec![song(1)]);
        yt.search_ahead(&[song(2)], None);
        assert!(yt.inner.ahead_on.load(Ordering::SeqCst) && yt.inner.search_on.load(Ordering::SeqCst));
        // Spotify sound, logging out, listening along: nothing.
        yt.fetch_ahead(Vec::new());
        yt.search_ahead(&[], None);
        assert!(!yt.inner.ahead_on.load(Ordering::SeqCst) && !yt.inner.search_on.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_stopped_yt_dlp_leaves_nothing_behind() {
        let dir = test_dir("stop");
        let Ok(tools) = find_tools(&Places::of_system(&dir, None)) else {
            eprintln!("no yt-dlp on this computer");
            return;
        };
        let temp = std::env::temp_dir();
        let left = |pid: u32| {
            let running = processes().iter().any(|p| p.id == pid);
            let unpacked = std::fs::read_dir(&temp)
                .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&format!("_MEI{pid}"))))
                .unwrap_or(false);
            running || unpacked
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            // yt-dlp waiting for a list of videos on its input: it asks YouTube nothing.
            let mut inputs = Vec::new();
            let mut start = || {
                let mut cmd = tokio::process::Command::new(&tools.ytdlp);
                cmd.args(["--ignore-config", "-a", "-"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
                cmd.creation_flags(CREATE_NO_WINDOW);
                let mut child = cmd.spawn().unwrap();
                inputs.push(child.stdin.take());
                let pid = child.id().unwrap();
                (Running(Some(child)), pid)
            };
            // Given up on while it unpacks itself; and stopped once it runs (a timeout).
            let (dropped, a) = start();
            drop(dropped);
            let (mut timed_out, b) = start();
            tokio::time::sleep(Duration::from_millis(1500)).await;
            timed_out.kill();
            let until = Instant::now() + Duration::from_secs(8);
            while (left(a) || left(b)) && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(!left(a), "the dropped one is still there");
            assert!(!left(b), "the stopped one is still there");
            drop(inputs);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_keeps_whole_files_and_drops_the_least_used() {
        let dir = test_dir("disk");
        let disk = DiskCache { dir: dir.join(AUDIO_DIR) };
        let ids = ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc", "ddddddddddd"];
        let file = m4a(100);
        for id in &ids[..3] {
            disk.store(id, &file, Some(100), 1_000).unwrap();
        }
        // Used long ago, in order a, b, c; then a is played again.
        let now = SystemTime::now();
        for (n, id) in ids[..3].iter().enumerate() {
            let f = std::fs::File::options().write(true).open(disk.file(id)).unwrap();
            f.set_modified(now - Duration::from_secs(300 - 60 * n as u64)).unwrap();
        }
        assert!(disk.read(ids[0]).unwrap() == file);
        // Room for three: the least recently used (b) goes.
        disk.store(ids[3], &file, Some(100), 350).unwrap();
        assert!(disk.has(ids[0]) && !disk.has(ids[1]) && disk.has(ids[2]) && disk.has(ids[3]));
        // Written whole: nothing half-written stays behind.
        let names: Vec<String> =
            std::fs::read_dir(&disk.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| n.ends_with(".m4a")), "{names:?}");
        // A newer copy replaces the old one.
        let other = m4a(120);
        disk.store(ids[2], &other, Some(120), 1_000).unwrap();
        assert!(disk.read(ids[2]).unwrap() == other);
        // A smaller limit applies at the next save.
        disk.store(ids[1], &file, Some(100), 0).unwrap();
        assert!(ids.iter().all(|id| !disk.has(id)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_smaller_cache_size_applies_at_once() {
        let dir = test_dir("disk-shrink");
        let yt = YouTube::new(dir.clone(), None);
        yt.set_disk_cache_mb(10);
        let disk = yt.disk();
        let ids = ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"];
        let now = SystemTime::now();
        for (n, id) in ids.iter().enumerate() {
            disk.store(id, &m4a(600_000), Some(600_000), 10 << 20).unwrap();
            let f = std::fs::File::options().write(true).open(disk.file(id)).unwrap();
            f.set_modified(now - Duration::from_secs(300 - 60 * n as u64)).unwrap();
        }
        let settles_at = |want: usize| {
            let until = Instant::now() + Duration::from_secs(3);
            while ids.iter().filter(|id| disk.has(id)).count() != want && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            ids.iter().filter(|id| disk.has(id)).count() == want
        };
        // Room for one: the most recently played stays.
        yt.set_disk_cache_mb(1);
        assert!(settles_at(1) && disk.has(ids[2]));
        // Off: none.
        yt.set_disk_cache_mb(0);
        assert!(settles_at(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_song_bigger_than_the_cache_doesnt_empty_it() {
        let dir = test_dir("disk-big");
        let disk = DiskCache { dir: dir.join(AUDIO_DIR) };
        disk.store("aaaaaaaaaaa", &m4a(400), Some(400), 500).unwrap();
        disk.store("bbbbbbbbbbb", &m4a(600), Some(600), 500).unwrap();
        assert!(disk.has("aaaaaaaaaaa") && !disk.has("bbbbbbbbbbb"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_turns_away_bad_files() {
        let dir = test_dir("disk-bad");
        let disk = DiskCache { dir: dir.join(AUDIO_DIR) };
        let id = "aaaaaaaaaaa";
        // Not the whole file, not an m4a, not a video id, size unknown.
        assert!(disk.store(id, &m4a(100), Some(200), 1_000).is_err());
        assert!(disk.store(id, b"<html>not audio</html>", Some(22), 1_000).is_err());
        assert!(disk.store("../../evil", &m4a(100), Some(100), 1_000).is_err());
        assert!(disk.store(id, &m4a(100), None, 1_000).is_err());
        assert!(!disk.has(id));
        // A file that isn't an m4a is removed when read.
        std::fs::create_dir_all(&disk.dir).unwrap();
        std::fs::write(disk.file(id), b"garbage").unwrap();
        assert!(disk.read(id).is_none());
        assert!(!disk.file(id).exists());
        // Half-written files of an earlier run are cleared; kept files stay.
        disk.store(id, &m4a(100), Some(100), 1_000).unwrap();
        std::fs::write(disk.dir.join("bbbbbbbbbbb.123-0.tmp"), b"half").unwrap();
        disk.clean_leftovers();
        let names: Vec<String> =
            std::fs::read_dir(&disk.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, [format!("{id}.m4a")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_song_kept_on_disk_plays_without_yt_dlp() {
        let dir = test_dir("disk-play");
        let yt = YouTube::new(dir.clone(), None);
        let song = track("spotify:track:abc", "For You", &["TV Girl"], 217_000);
        let found = Candidate {
            video_id: "G2-_sP5v-Zo".to_owned(),
            title: "TV Girl - For You".to_owned(),
            channel: "TV Girl".to_owned(),
            duration_ms: 217_000,
            score: 76.0,
        };
        yt.with_matches(|m| m.found.insert(song.uri.clone(), Saved::from(&found)));
        let file = m4a(5_000);
        yt.disk().store(&found.video_id, &file, Some(5_000), 1 << 20).unwrap();
        // Off: not used.
        assert!(yt.disk_audio(&song).await.is_none());
        assert!(!yt.is_fetched(&song.uri));
        yt.set_disk_cache_mb(1);
        assert!(yt.is_fetched(&song.uri));
        let audio = yt.audio(&song).await.unwrap();
        assert!(audio.bytes == file);
        assert_eq!((audio.title.as_str(), audio.duration_ms), ("TV Girl - For You", Some(217_000)));
        // Another video picked: that one isn't on disk.
        yt.choose(&song.uri, "aDFmc2aZ26k");
        assert!(yt.disk_audio(&song).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_searches_go_nearest_first() {
        assert_eq!(search_order(6, Some(2)), [2, 3, 1, 4, 0, 5]);
        assert_eq!(search_order(3, Some(2)), [2, 1, 0]);
        assert_eq!(search_order(3, None), [0, 1, 2]);
        assert_eq!(search_order(3, Some(9)), [0, 1, 2]);
        assert!(search_order(0, Some(0)).is_empty());
    }

    fn song(n: usize) -> TrackInfo {
        track(&format!("spotify:track:{n}"), &format!("Song {n}"), &["Artist"], 200_000)
    }

    fn searched(step: Step) -> String {
        match step {
            Step::Search(t) => t.uri,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn background_searches_are_gentle() {
        let mut plan = SearchPlan::default();
        plan.replace((0..5).map(song).collect());
        let none = |_: &str| false;
        let t0 = Instant::now();
        assert_eq!(searched(plan.next(t0, None, false, none)), "spotify:track:0");
        // One start a second.
        assert_eq!(plan.next(t0 + Duration::from_millis(200), None, false, none), Step::Wait(t0 + SEARCH_GAP));
        assert_eq!(searched(plan.next(t0 + SEARCH_GAP, None, false, none)), "spotify:track:1");
        // Two at once at most.
        let t2 = t0 + SEARCH_GAP * 2;
        assert_eq!(plan.next(t2, None, false, none), Step::Idle);
        plan.done(song(0), false);
        // Not while a song is being fetched.
        assert_eq!(plan.next(t2, None, true, none), Step::Wait(t2 + BUSY_RECHECK));
        // Songs matched meanwhile are passed over.
        assert_eq!(searched(plan.next(t2, None, false, |uri| uri == "spotify:track:2")), "spotify:track:3");
        // YouTube refused: everything waits, and the song is searched again first.
        plan.done(song(3), true);
        plan.done(song(1), false);
        let t3 = t2 + SEARCH_GAP;
        let until = t3 + REFUSED_PAUSE;
        assert_eq!(plan.next(t3, Some(until), false, none), Step::Wait(until));
        assert_eq!(searched(plan.next(until, Some(until), false, none)), "spotify:track:3");
        assert_eq!(searched(plan.next(until + SEARCH_GAP, None, false, none)), "spotify:track:4");
        plan.done(song(3), false);
        plan.done(song(4), false);
        assert_eq!(plan.next(until + SEARCH_GAP * 2, None, false, none), Step::Idle);
        // A list opened again: songs already searched aren't searched again.
        plan.replace((0..6).map(song).collect());
        let t4 = until + SEARCH_GAP * 3;
        assert_eq!(searched(plan.next(t4, None, false, none)), "spotify:track:2");
        assert_eq!(searched(plan.next(t4 + SEARCH_GAP, None, false, none)), "spotify:track:5");
    }

    #[test]
    fn refusals_pause_background_work() {
        let dir = test_dir("pause");
        let yt = YouTube::new(dir.clone(), None);
        yt.note_problem(&anyhow!(NOT_FOUND));
        assert_eq!(yt.paused_until(), None);
        yt.note_problem(&anyhow!(REFUSED));
        let until = yt.paused_until().unwrap();
        assert!(until > Instant::now() + REFUSED_PAUSE - Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[ignore = "runs yt-dlp and downloads from YouTube"]
    async fn youtube_live_finds_and_fetches_tv_girl_for_you() {
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Info).try_init();
        let dir = test_dir("live");
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        let yt = YouTube::new(dir.clone(), exe_dir);
        let tools = yt.tools().expect("yt-dlp and a JavaScript runtime");
        eprintln!("tools: {tools:?}");
        let song = track("spotify:track:test-tv-girl-for-you", "For You", &["TV Girl"], 217_000);

        let started = Instant::now();
        let (a, b) = tokio::join!(yt.audio(&song), yt.audio(&song));
        let first = started.elapsed();
        let (a, b) = (a.unwrap(), b.unwrap());
        eprintln!(
            "video {} {:?}: {} bytes, {:?} ms, in {:.1}s (two calls at once)",
            a.video_id,
            a.title,
            a.bytes.len(),
            a.duration_ms,
            first.as_secs_f32()
        );
        assert!(is_mp4(&a.bytes), "starts with {:?}", &a.bytes[..a.bytes.len().min(8)]);
        assert_eq!(a.bytes, b.bytes);
        assert!(a.duration_ms.is_some_and(|d| d.abs_diff(217_000) <= MAX_LENGTH_DIFF_MS));
        assert!(a.bytes.len() > 1_000_000);

        let started = Instant::now();
        let again = yt.audio(&song).await.unwrap();
        assert_eq!(again.video_id, a.video_id);
        assert!(started.elapsed() < Duration::from_millis(200), "replay took {:?}", started.elapsed());

        let restarted = YouTube::new(dir.clone(), None);
        assert_eq!(restarted.find(&song).await.unwrap().video_id, a.video_id);
        let list = yt.candidates(&song).await.unwrap();
        assert_eq!(list.first().map(|c| c.video_id.as_str()), Some(a.video_id.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Time from asking for a song to having its audio, for a few songs: never seen, match known,
    /// cached. `TRUFFLE_BENCH` names the run; the report goes to `speed2` under `TRUFFLE_TEST_DIR`.
    #[tokio::test]
    #[ignore = "runs yt-dlp and downloads from YouTube"]
    async fn youtube_live_speed() {
        let _ = env_logger::builder().is_test(true).filter_level(log::LevelFilter::Info).try_init();
        let label = std::env::var("TRUFFLE_BENCH").unwrap_or_else(|_| "run".to_owned());
        let base = crate::config::test_root().join("speed2");
        let dir = base.join(format!("bench-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
        let songs = [
            track("spotify:track:bench1", "For You", &["TV Girl"], 217_000),
            track("spotify:track:bench2", "Creep", &["Radiohead"], 238_640),
            track("spotify:track:bench3", "Dreams - 2004 Remaster", &["Fleetwood Mac"], 257_800),
            track("spotify:track:bench4", "Do I Wanna Know?", &["Arctic Monkeys"], 272_394),
            track("spotify:track:bench5", "Get Lucky (Radio Edit)", &["Daft Punk", "Pharrell Williams"], 248_413),
        ];
        let gap = Duration::from_millis(1500);
        let mut report = String::new();
        let mut times: [Vec<f32>; 3] = Default::default();
        let mut time = |kind: usize, name: &str, took: Duration, ok: Result<&Audio, &anyhow::Error>| {
            let line = match ok {
                Ok(a) => format!("{} {name}: {:.2}s ({}, {} KB)\n", ["a", "b", "c"][kind], took.as_secs_f32(), a.video_id, a.bytes.len() / 1024),
                Err(e) => format!("{} {name}: FAILED after {:.2}s: {e:#}\n", ["a", "b", "c"][kind], took.as_secs_f32()),
            };
            eprint!("{line}");
            report.push_str(&line);
            if ok.is_ok() {
                times[kind].push(took.as_secs_f32());
            }
        };

        // (a) Never seen.
        let yt = YouTube::new(dir.clone(), exe_dir.clone());
        yt.set_disk_cache_mb(100);
        for song in &songs {
            let started = Instant::now();
            let got = yt.audio(song).await;
            time(0, &song.name, started.elapsed(), got.as_ref());
            tokio::time::sleep(gap).await;
        }
        // After a restart: (b) match known, without the disk cache; (c) kept on disk.
        let yt = YouTube::new(dir.clone(), exe_dir.clone());
        let kept = YouTube::new(dir.clone(), exe_dir.clone());
        kept.set_disk_cache_mb(100);
        for song in &songs {
            let started = Instant::now();
            let got = yt.audio(song).await;
            time(1, &song.name, started.elapsed(), got.as_ref());
            let started = Instant::now();
            let got = kept.audio(song).await;
            time(2, &song.name, started.elapsed(), got.as_ref());
            tokio::time::sleep(gap).await;
        }
        drop(time);
        for (list, name) in times.iter_mut().zip(["a never seen", "b match known", "c cached"]) {
            list.sort_by(f32::total_cmp);
            let median = list.get(list.len() / 2).copied().unwrap_or(f32::NAN);
            let line = format!("median {name}: {median:.2}s over {}\n", list.len());
            eprint!("{line}");
            report.push_str(&line);
        }
        std::fs::write(base.join(format!("bench-{label}.txt")), report).unwrap();
    }
}

