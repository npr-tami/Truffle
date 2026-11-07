//! The scrobbler end to end against a local mock of the Last.fm API (no network needed).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;

use super::*;
use crate::model::ArtistRef;

// Made-up test values (not real credentials).
const KEY: &str = "0123456789abcdef0123456789abcdef";
const SECRET: &str = "fedcba9876543210fedcba9876543210";
const SK: &str = "5e55105e55105e55105e55105e551050";

type Params = BTreeMap<String, String>;
type Responder = Arc<dyn Fn(&Params) -> (u16, String) + Send + Sync>;

#[derive(Debug, Clone)]
struct Seen {
    /// Lower-cased request head.
    head: String,
    body: String,
    params: Params,
}

struct Mock {
    root: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(responder: Responder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/2.0/", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (responder, log) = (responder.clone(), log.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let head_end = loop {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break p + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    while buf.len() < head_end + length {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body_bytes = &buf[head_end..buf.len().min(head_end + length)];
                    let params: Params = url::form_urlencoded::parse(body_bytes).into_owned().collect();
                    let body = String::from_utf8_lossy(body_bytes).into_owned();
                    let (status, reply) = responder(&params);
                    log.lock().unwrap().push(Seen { head, body, params });
                    let out = format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = stream.write_all(out.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { root, seen }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn calls(&self, method: &str) -> Vec<Seen> {
        self.requests().into_iter().filter(|r| r.params.get("method").map(String::as_str) == Some(method)).collect()
    }

    /// Wait (briefly) until `n` calls of `method` arrived.
    async fn wait_for(&self, method: &str, n: usize) {
        for _ in 0..200 {
            if self.calls(method).len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} {method} call(s), got {}", self.calls(method).len());
    }
}

const OK_NOW_PLAYING: &str = r##"{"nowplaying":{"track":{"corrected":"0","#text":"x"},"ignoredMessage":{"code":"0","#text":""}}}"##;

fn accepted(n: usize) -> String {
    let item = r##"{"ignoredMessage":{"code":"0","#text":""},"timestamp":"1"}"##;
    if n == 1 {
        format!(r#"{{"scrobbles":{{"@attr":{{"accepted":1,"ignored":0}},"scrobble":{item}}}}}"#)
    } else {
        let items = vec![item; n].join(",");
        format!(r#"{{"scrobbles":{{"@attr":{{"accepted":{n},"ignored":0}},"scrobble":[{items}]}}}}"#)
    }
}

fn batch_size(p: &Params) -> usize {
    p.keys().filter(|k| k.starts_with("artist[")).count()
}

/// Now playing always works; `scrobble` decides the answer to `track.scrobble`.
fn responder(scrobble: impl Fn(&Params) -> (u16, String) + Send + Sync + 'static) -> Responder {
    Arc::new(move |p: &Params| match p.get("method").map(String::as_str) {
        Some("track.updateNowPlaying") => (200, OK_NOW_PLAYING.to_owned()),
        Some("track.scrobble") => scrobble(p),
        _ => (400, r#"{"error":3,"message":"Invalid Method"}"#.to_owned()),
    })
}

fn track(uri: &str, name: &str, secs: u32) -> TrackInfo {
    TrackInfo {
        uri: uri.into(),
        name: name.into(),
        artists: vec![
            ArtistRef { name: "Main Artist".into(), uri: String::new() },
            ArtistRef { name: "Guest".into(), uri: String::new() },
        ],
        album: "Album".into(),
        album_uri: String::new(),
        cover_small: None,
        cover_large: None,
        duration_ms: secs * 1000,
        playable: true,
        is_local: false,
        added_at: 0,
    }
}

/// A play recorded for the harness's account ("listener").
fn pending(name: &str, age_secs: i64) -> PendingScrobble {
    pending_for(name, age_secs, "listener")
}

/// A play recorded for `user` (empty: queued by an older Truffle, account unknown).
fn pending_for(name: &str, age_secs: i64, user: &str) -> PendingScrobble {
    PendingScrobble {
        track: ScrobbleTrack {
            artist: "Main Artist".into(),
            track: name.into(),
            album: String::new(),
            duration_s: 200,
            uri: String::new(),
        },
        timestamp: unix_now() - age_secs,
        attempts: 0,
        user: user.into(),
    }
}

struct Setup {
    connected: bool,
    enabled: bool,
    queued: Vec<PendingScrobble>,
}

impl Default for Setup {
    fn default() -> Self {
        Self { connected: true, enabled: true, queued: Vec::new() }
    }
}

struct Harness {
    scrobbler: Scrobbler,
    reports: UnboundedReceiver<Report>,
    statuses: Arc<Mutex<Vec<LastfmStatus>>>,
    paths: Paths,
    mock: Mock,
    t0: Instant,
}

impl Harness {
    async fn new(name: &str, responder: Responder, setup: Setup) -> Self {
        let dir = std::env::temp_dir().join(format!("truffle-lastfm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        std::fs::create_dir_all(&paths.config).unwrap();
        if setup.connected {
            config::write_json(&paths.lastfm_session_file(), &Session { username: "listener".into(), key: SK.into() })
                .unwrap();
        }
        if !setup.queued.is_empty() {
            let mut queue = ScrobbleQueue::default();
            for item in setup.queued {
                queue.push(item);
            }
            queue.save(&paths.lastfm_queue_file()).unwrap();
        }
        Self::open(paths, responder, setup.enabled).await
    }

    /// A Scrobbler on the files in `paths`, talking to a new mock.
    async fn open(paths: Paths, responder: Responder, enabled: bool) -> Self {
        let mock = Mock::start(responder).await;
        let settings = Settings {
            lastfm_enabled: enabled,
            lastfm_api_key: Some(KEY.into()),
            lastfm_api_secret: Some(SECRET.into()),
            ..Settings::default()
        };
        let (tx, reports) = tokio::sync::mpsc::unbounded_channel();
        let statuses = Arc::new(Mutex::new(Vec::new()));
        let sink = statuses.clone();
        let client = api::Client::with_root(&mock.root).unwrap();
        let scrobbler = Scrobbler::with_client(
            &paths,
            &settings,
            Some(client),
            Arc::new(move |s| sink.lock().unwrap().push(s)),
            Arc::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        Self { scrobbler, reports, statuses, paths, mock, t0: Instant::now() }
    }

    /// Quit and start again on the same files, with a new mock answering.
    async fn restart(self, responder: Responder) -> Self {
        let mut old = self;
        old.scrobbler.shutdown(Instant::now());
        // Keep the files: the old harness's Drop would delete them.
        let paths = std::mem::replace(&mut old.paths, Paths { config: PathBuf::new(), cache: PathBuf::new() });
        drop(old);
        Self::open(paths, responder, true).await
    }

    fn at(&self, secs: f64) -> Instant {
        self.t0 + Duration::from_secs_f64(secs)
    }

    /// Play `t` (play `id`) from `from` s for `secs` seconds, observing every second.
    fn play(&mut self, id: u64, t: &TrackInfo, from: u64, secs: u64) {
        for s in from..=from + secs {
            let now = self.at(s as f64);
            self.scrobbler.observe(id, Some(t), PlayStatus::Playing, now);
        }
    }

    async fn report(&mut self) -> Report {
        match tokio::time::timeout(Duration::from_secs(5), self.reports.recv()).await {
            Ok(Some(report)) => report,
            _ => panic!("no report from the scrobbler"),
        }
    }

    /// Wait for the next report and hand it to the scrobbler at `secs`.
    async fn answer(&mut self, secs: f64) -> Option<NewConnection> {
        let report = self.report().await;
        let now = self.at(secs);
        self.scrobbler.on_report(report, now)
    }

    fn status(&self) -> LastfmStatus {
        self.statuses.lock().unwrap().last().cloned().expect("a status was sent")
    }

    fn queue_on_disk(&self) -> Option<usize> {
        self.paths.lastfm_queue_file().exists().then(|| ScrobbleQueue::load(&self.paths.lastfm_queue_file()).0.len())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(dir) = self.paths.config.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// (is it a problem, its text)
fn alert_text(alert: Option<Alert>) -> Option<(bool, String)> {
    alert.map(|a| (matches!(a, Alert::Problem(_)), a.text().to_owned()))
}

fn saved_refusal(paths: &Paths) -> Option<u32> {
    config::read_json::<SavedSession>(&paths.lastfm_session_file()).and_then(|s| s.refused)
}

/// The signature covers exactly what was sent (minus format/api_sig) and the secret never is.
fn assert_signed(r: &Seen) {
    let mut p = r.params.clone();
    let sig = p.remove("api_sig").expect("api_sig sent");
    assert_eq!(p.remove("format").as_deref(), Some("json"));
    assert_eq!(api::sign(&p, SECRET), sig);
    assert_eq!(p.get("api_key").map(String::as_str), Some(KEY));
    assert!(!r.body.contains(SECRET), "the shared secret must never be sent");
    assert!(r.head.contains("user-agent: truffle/"), "{}", r.head);
    assert!(r.head.starts_with("post "));
}

#[tokio::test]
async fn an_eligible_play_is_saved_then_sent() {
    let mut h = Harness::new("send", responder(|p| (200, accepted(batch_size(p)))), Setup::default()).await;
    h.scrobbler.start(h.t0);
    assert!(h.status().connected && h.status().enabled);
    assert_eq!(h.status().username.as_deref(), Some("listener"));

    let t = track("spotify:track:a", "Song A", 40);
    h.play(1, &t, 0, 19);
    assert_eq!(h.status().pending, 0, "not eligible before 20 s");
    h.play(1, &t, 20, 0);
    // Written to disk before anything was sent.
    assert_eq!(h.status().pending, 1);
    assert_eq!(h.queue_on_disk(), Some(1));

    h.answer(21.0).await;
    assert_eq!(h.status().pending, 0);
    assert_eq!(h.queue_on_disk(), Some(0));

    h.mock.wait_for("track.updateNowPlaying", 1).await;
    let now_playing = &h.mock.calls("track.updateNowPlaying")[0];
    assert_signed(now_playing);
    assert_eq!(now_playing.params["artist"], "Main Artist", "primary artist only");
    assert_eq!(now_playing.params["track"], "Song A");
    assert_eq!(now_playing.params["album"], "Album");
    assert_eq!(now_playing.params["duration"], "40");
    assert_eq!(now_playing.params["sk"], SK);

    let scrobbles = h.mock.calls("track.scrobble");
    assert_eq!(scrobbles.len(), 1);
    let s = &scrobbles[0];
    assert_signed(s);
    assert_eq!(s.params["artist[0]"], "Main Artist");
    assert_eq!(s.params["track[0]"], "Song A");
    let ts: i64 = s.params["timestamp[0]"].parse().unwrap();
    assert!((unix_now() - ts).abs() < 60, "timestamp is when it started playing");
}

#[tokio::test]
async fn an_invalid_session_is_forgotten_and_the_queue_kept() {
    let invalid = |_: &Params| (403, r#"{"error":9,"message":"Invalid session key - Please re-authenticate"}"#.to_owned());
    let mut h = Harness::new("reauth", responder(invalid), Setup::default()).await;
    h.scrobbler.start(h.t0);
    let t = track("spotify:track:a", "Song A", 40);
    h.play(1, &t, 0, 20);
    h.answer(21.0).await;

    let status = h.status();
    assert!(!status.connected && status.username.is_none());
    assert_eq!(status.pending, 1, "the scrobble is kept for after reconnecting");
    assert!(status.message.as_deref().is_some_and(|m| m.contains("Connect")), "{status:?}");
    assert!(!h.paths.lastfm_session_file().exists());
    assert_eq!(h.queue_on_disk(), Some(1));
    // Said once outside Settings too.
    assert!(alert_text(h.scrobbler.take_alert()).is_some_and(|(problem, a)| problem && a.contains("Settings")));
    assert!(h.scrobbler.take_alert().is_none());

    // Nothing more is sent, and new plays aren't queued while disconnected.
    h.scrobbler.on_tick(h.at(60.0));
    h.play(2, &track("spotify:track:b", "Song B", 40), 61, 30);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1);
    assert_eq!(h.status().pending, 1);
}

#[tokio::test]
async fn a_refused_secret_halts_and_offers_connect_again() {
    let refused = |_: &Params| (403, r#"{"error":13,"message":"Invalid method signature supplied"}"#.to_owned());
    let mut h = Harness::new("halt", responder(refused), Setup::default()).await;
    h.scrobbler.start(h.t0);
    h.play(1, &track("spotify:track:a", "Song A", 40), 0, 20);
    h.answer(21.0).await;

    // Settings says why, and still knows the sign-in (for Disconnect and the switch).
    let status = h.status();
    assert!(!status.connected, "{status:?}");
    assert!(status.stopped.as_deref().is_some_and(|m| m.contains("shared secret")), "{status:?}");
    assert_eq!(status.username.as_deref(), Some("listener"));
    assert_eq!(status.pending, 1);
    assert_eq!(status.held, 0);
    // Told once outside Settings; the sign-in stays saved, with the refusal.
    assert!(
        alert_text(h.scrobbler.take_alert())
            .is_some_and(|(problem, a)| problem && a.contains("shared secret") && a.contains("Settings"))
    );
    assert!(h.scrobbler.take_alert().is_none());
    assert!(h.paths.lastfm_session_file().exists());
    assert_eq!(saved_refusal(&h.paths), Some(13));

    // Nothing more is sent, but plays are still kept for after reconnecting.
    h.play(2, &track("spotify:track:b", "Song B", 40), 30, 25);
    h.scrobbler.on_tick(h.at(60.0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1);
    assert_eq!(h.mock.calls("track.updateNowPlaying").len(), 1, "no now playing while halted");
    assert_eq!(h.status().pending, 2);
    assert_eq!(h.queue_on_disk(), Some(2));
    assert!(h.scrobbler.take_alert().is_none(), "not repeated");

    // Cancelling a new connection attempt doesn't hide why scrobbling stopped.
    h.scrobbler.cancel_connect();
    assert!(h.status().stopped.is_some());

    // Pausing works while stopped: plays aren't kept any more.
    h.scrobbler.set_enabled(false, h.at(61.0));
    h.play(3, &track("spotify:track:c", "Song C", 40), 62, 25);
    assert_eq!(h.status().pending, 2);
    h.scrobbler.set_enabled(true, h.at(90.0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1, "switching on doesn't send while stopped");

    // The next start: stopped at once, checks once, and says nothing new.
    let mut h = h.restart(responder(refused)).await;
    h.scrobbler.start(h.t0);
    let first = h.statuses.lock().unwrap().first().cloned().unwrap();
    assert!(!first.connected && first.stopped.is_some(), "never shown as connected: {first:?}");
    h.answer(1.0).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1, "one check");
    assert!(h.status().stopped.is_some());
    assert!(h.scrobbler.take_alert().is_none(), "a lasting refusal is announced once");
    h.play(1, &track("spotify:track:d", "Song D", 40), 2, 25);
    h.scrobbler.on_tick(h.at(30.0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1, "only one check per start");
    assert_eq!(h.status().pending, 3);

    // Disconnect works while stopped and forgets the refusal; the plays wait for this account.
    h.scrobbler.disconnect();
    let status = h.status();
    assert!(!status.connected && status.stopped.is_none() && status.username.is_none(), "{status:?}");
    assert!(!h.paths.lastfm_session_file().exists());
    assert_eq!(status.pending, 3, "kept");
    assert_eq!(status.held, 3);
    assert_eq!(status.held_for.as_deref(), Some("listener"));
    assert!(status.message.as_deref().is_some_and(|m| m.contains("when listener is connected again")), "{status:?}");
}

#[tokio::test]
async fn a_refusal_that_went_away_resumes_by_itself_at_the_next_start() {
    let refused = |_: &Params| (403, r#"{"error":10,"message":"Invalid API key"}"#.to_owned());
    let mut h = Harness::new("resume", responder(refused), Setup::default()).await;
    h.scrobbler.start(h.t0);
    h.play(1, &track("spotify:track:a", "Song A", 40), 0, 20);
    h.answer(21.0).await;
    assert!(h.status().stopped.is_some());
    assert!(h.scrobbler.take_alert().is_some());

    let mut h = h.restart(responder(|p| (200, accepted(batch_size(p))))).await;
    h.scrobbler.start(h.t0);
    h.answer(1.0).await;
    let status = h.status();
    assert!(status.connected && status.stopped.is_none(), "{status:?}");
    assert_eq!(status.pending, 0);
    assert!(status.message.as_deref().is_some_and(|m| m.contains("again")), "{status:?}");
    assert_eq!(saved_refusal(&h.paths), None);
    assert!(h.paths.lastfm_session_file().exists());
    assert!(h.scrobbler.take_alert().is_none());
    // Now playing works again too.
    h.play(1, &track("spotify:track:b", "Song B", 40), 2, 1);
    h.mock.wait_for("track.updateNowPlaying", 1).await;
}

#[tokio::test]
async fn offline_keeps_scrobbles_and_retries_after_the_wait() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let flaky = move |p: &Params| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            (503, "<html>Service Unavailable</html>".to_owned())
        } else {
            (200, accepted(batch_size(p)))
        }
    };
    let mut h = Harness::new("offline", responder(flaky), Setup::default()).await;
    h.scrobbler.start(h.t0);
    h.play(1, &track("spotify:track:a", "Song A", 40), 0, 20);
    h.answer(21.0).await;
    let status = h.status();
    assert_eq!(status.pending, 1);
    assert!(status.message.as_deref().is_some_and(|m| m.contains("can't be reached")), "{status:?}");

    // The wait (at least 12 s) isn't over yet.
    h.scrobbler.on_tick(h.at(25.0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1);

    // Spotify reconnecting means the network is back: try again right away.
    h.scrobbler.network_up(h.at(26.0));
    h.answer(27.0).await;
    let status = h.status();
    assert_eq!(status.pending, 0);
    assert!(status.message.is_none(), "the temporary note goes away: {status:?}");
    assert_eq!(h.mock.calls("track.scrobble").len(), 2);
}

#[tokio::test]
async fn ignored_scrobbles_are_dropped_and_the_daily_limit_waits() {
    let mixed = |_: &Params| {
        (
            200,
            r##"{"scrobbles":{"@attr":{"accepted":0,"ignored":2},"scrobble":[
                {"ignoredMessage":{"code":"1","#text":"Artist was ignored"}},
                {"ignoredMessage":{"code":"5","#text":"Daily scrobble limit exceeded"}}]}}"##
                .to_owned(),
        )
    };
    let setup = Setup { queued: vec![pending("filtered", 600), pending("over the limit", 300)], ..Setup::default() };
    let mut h = Harness::new("ignored", responder(mixed), setup).await;
    h.scrobbler.start(h.t0);
    assert_eq!(h.status().pending, 2);
    h.answer(1.0).await;
    let status = h.status();
    assert_eq!(status.pending, 1, "the filtered one is gone, the one over the limit kept");
    assert!(status.message.as_deref().is_some_and(|m| m.contains("daily")), "{status:?}");
    let kept = ScrobbleQueue::load(&h.paths.lastfm_queue_file()).0.oldest(5);
    assert_eq!(kept[0].track.track, "over the limit");

    // Not before an hour has passed, not even when the network "comes back".
    h.scrobbler.network_up(h.at(2.0));
    h.scrobbler.on_tick(h.at(3_000.0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1);
    let sent = &h.mock.calls("track.scrobble")[0];
    assert_eq!(sent.params["track[0]"], "filtered", "oldest first");
    assert_eq!(sent.params["track[1]"], "over the limit");
}

#[tokio::test]
async fn a_rejected_batch_is_retried_one_by_one() {
    let picky = |p: &Params| {
        if batch_size(p) > 1 || p.get("track[0]").map(String::as_str) == Some("bad") {
            (400, r#"{"error":6,"message":"Invalid parameters"}"#.to_owned())
        } else {
            (200, accepted(1))
        }
    };
    let setup = Setup { queued: vec![pending("bad", 600), pending("good", 300)], ..Setup::default() };
    let mut h = Harness::new("isolate", responder(picky), setup).await;
    h.scrobbler.start(h.t0);
    h.answer(0.5).await;
    assert_eq!(h.status().pending, 2, "nothing dropped for a bad batch");
    h.scrobbler.on_tick(h.at(2.0));
    h.answer(2.5).await;
    assert_eq!(h.status().pending, 1, "the bad one alone is dropped");
    h.scrobbler.on_tick(h.at(4.0));
    h.answer(4.5).await;
    assert_eq!(h.status().pending, 0);
    let sizes: Vec<usize> = h.mock.calls("track.scrobble").iter().map(|r| batch_size(&r.params)).collect();
    assert_eq!(sizes, [2, 1, 1]);
}

#[tokio::test]
async fn nothing_is_queued_or_sent_while_paused() {
    let mut h = Harness::new("paused", responder(|p| (200, accepted(batch_size(p)))), Setup {
        enabled: false,
        ..Setup::default()
    })
    .await;
    h.scrobbler.start(h.t0);
    assert!(h.status().connected && !h.status().enabled);
    h.play(1, &track("spotify:track:a", "Song A", 40), 0, 40);
    assert_eq!(h.status().pending, 0);
    assert_eq!(h.queue_on_disk(), None, "no file written");

    // Switched on: the next play is scrobbled.
    h.scrobbler.set_enabled(true, h.at(41.0));
    h.play(2, &track("spotify:track:b", "Song B", 40), 41, 20);
    assert_eq!(h.status().pending, 1);
    h.answer(62.0).await;
    assert_eq!(h.status().pending, 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let now_playing = h.mock.calls("track.updateNowPlaying");
    assert_eq!(now_playing.len(), 1, "no now playing while paused");
    assert_eq!(now_playing[0].params["track"], "Song B");
}

#[tokio::test]
async fn nothing_is_queued_without_a_connection() {
    let mut h =
        Harness::new("noconn", responder(|p| (200, accepted(batch_size(p)))), Setup { connected: false, ..Setup::default() })
            .await;
    h.scrobbler.start(h.t0);
    assert!(!h.status().connected);
    h.play(1, &track("spotify:track:a", "Song A", 40), 0, 40);
    h.scrobbler.finalise_play(h.at(41.0));
    assert_eq!(h.status().pending, 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.mock.requests().is_empty());
}

#[tokio::test]
async fn disconnect_keeps_unsent_scrobbles() {
    let setup = Setup { enabled: false, queued: vec![pending("one", 600), pending("two", 300)], ..Setup::default() };
    let mut h = Harness::new("disconnect", responder(|p| (200, accepted(batch_size(p)))), setup).await;
    h.scrobbler.start(h.t0);
    h.scrobbler.disconnect();
    let status = h.status();
    assert!(!status.connected);
    assert_eq!(status.pending, 2);
    assert!(status.message.as_deref().is_some_and(|m| m.contains("2 unsent scrobbles are kept")), "{status:?}");
    assert!(!h.paths.lastfm_session_file().exists());
    assert_eq!(h.queue_on_disk(), Some(2));
}

#[tokio::test]
async fn connect_checks_the_key_and_secret_first() {
    let mut h = Harness::new("validate", responder(|p| (200, accepted(batch_size(p)))), Setup {
        connected: false,
        ..Setup::default()
    })
    .await;
    h.scrobbler.start(h.t0);
    h.scrobbler.connect("not a key", SECRET);
    assert!(h.status().message.as_deref().is_some_and(|m| m.contains("API key")));
    assert!(!h.status().connecting);
    h.scrobbler.connect(KEY, "short");
    assert!(h.status().message.as_deref().is_some_and(|m| m.contains("shared secret")));
    h.scrobbler.connect(KEY, KEY);
    assert!(h.status().message.as_deref().is_some_and(|m| m.contains("the same")));
    h.scrobbler.connect("", "");
    assert!(!h.status().connecting);
    // The same refusal twice is still confirmed each time (the UI shows "connecting" at once).
    let sent = h.statuses.lock().unwrap().len();
    h.scrobbler.connect("", "");
    h.scrobbler.cancel_connect();
    assert_eq!(h.statuses.lock().unwrap().len(), sent + 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.mock.requests().is_empty());
}

#[tokio::test]
async fn shutdown_queues_an_eligible_play_without_sending_it() {
    let mut h = Harness::new("shutdown", responder(|p| (200, accepted(batch_size(p)))), Setup::default()).await;
    h.scrobbler.start(h.t0);
    let t = track("spotify:track:a", "Song A", 40);
    h.play(1, &t, 0, 19);
    assert_eq!(h.status().pending, 0);
    // Quit between two ticks, just after the 20 s mark.
    h.scrobbler.shutdown(h.at(19.5 + 0.6));
    assert_eq!(h.queue_on_disk(), Some(1));
    h.scrobbler.observe(1, Some(&t), PlayStatus::Playing, h.at(30.0));
    h.scrobbler.on_tick(h.at(31.0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.mock.calls("track.scrobble").is_empty(), "sent at the next start instead");

    // An ineligible play at shutdown leaves nothing behind.
    let mut h = Harness::new("shutdown2", responder(|p| (200, accepted(batch_size(p)))), Setup::default()).await;
    h.scrobbler.start(h.t0);
    h.play(1, &t, 0, 10);
    h.scrobbler.shutdown(h.at(10.5));
    assert_eq!(h.queue_on_disk(), None);
}

#[tokio::test]
async fn auth_calls_parse_tokens_sessions_and_errors() {
    let auth: Responder = Arc::new(|p: &Params| match p.get("method").map(String::as_str) {
        Some("auth.getToken") => (200, r#"{"token":"cf45fe5a3e3cebe168480a086d7fe481"}"#.to_owned()),
        Some("auth.getSession") if p.get("token").map(String::as_str) == Some("approved") => {
            (200, format!(r#"{{"session":{{"name":"listener","key":"{SK}","subscriber":0}}}}"#))
        }
        Some("auth.getSession") => (403, r#"{"error":14,"message":"This token has not been authorized"}"#.to_owned()),
        _ => (400, r#"{"error":3,"message":"Invalid Method"}"#.to_owned()),
    });
    let mock = Mock::start(auth).await;
    let client = api::Client::with_root(&mock.root).unwrap();
    let creds = Credentials { api_key: KEY.into(), secret: SECRET.into() };
    assert_eq!(client.get_token(&creds).await.unwrap(), "cf45fe5a3e3cebe168480a086d7fe481");
    let waiting = client.get_session(&creds, "pending").await.unwrap_err();
    assert_eq!(waiting.code(), Some(14));
    let session = client.get_session(&creds, "approved").await.unwrap();
    assert_eq!(session, Session { username: "listener".into(), key: SK.into() });
    for r in mock.requests() {
        assert_signed(&r);
        assert!(!r.params.contains_key("sk"), "auth calls carry no session key");
    }
}

#[tokio::test]
async fn plays_from_another_account_wait_for_the_users_choice() {
    let setup = Setup {
        connected: false,
        queued: vec![pending_for("from alice", 900, "alice"), pending_for("alice too", 600, "Alice")],
        ..Setup::default()
    };
    let mut h = Harness::new("accounts", responder(|p| (200, accepted(batch_size(p)))), setup).await;
    h.scrobbler.start(h.t0);
    let status = h.status();
    assert_eq!((status.pending, status.held), (2, 2));
    assert_eq!(status.held_for.as_deref(), Some("alice"));

    // Bob connects: nothing of Alice's goes to his profile.
    let bob = Session { username: "bob".into(), key: SK.into() };
    let creds = Credentials { api_key: KEY.into(), secret: SECRET.into() };
    h.scrobbler.on_connected(creds, bob, h.at(1.0));
    let status = h.status();
    assert!(status.connected);
    assert_eq!((status.pending, status.held), (2, 2));
    assert_eq!(status.held_for.as_deref(), Some("alice"));
    // His own plays are sent as usual.
    h.play(1, &track("spotify:track:b", "Bob's song", 40), 2, 20);
    h.answer(23.0).await;
    let sent = h.mock.calls("track.scrobble");
    assert_eq!(sent.len(), 1);
    assert_eq!(batch_size(&sent[0].params), 1);
    assert_eq!(sent[0].params["track[0]"], "Bob's song");
    assert_eq!(h.status().pending, 2);

    // A choice made for another account than the connected one (a stale view) is ignored.
    h.scrobbler.send_held("carol", h.at(25.0));
    h.scrobbler.discard(Some("carol"));
    h.scrobbler.discard(None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1);
    assert_eq!(h.status().pending, 2);

    // Discarded for Bob: Alice's plays are gone, unsent.
    h.scrobbler.discard(Some("Bob"));
    let status = h.status();
    assert_eq!((status.pending, status.held), (0, 0));
    assert_eq!(h.queue_on_disk(), Some(0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1);
}

#[tokio::test]
async fn held_plays_are_sent_only_when_the_user_says_so() {
    let setup = Setup {
        connected: false,
        queued: vec![pending_for("from alice", 900, "alice"), pending_for("unknown account", 600, "")],
        ..Setup::default()
    };
    let mut h = Harness::new("held", responder(|p| (200, accepted(batch_size(p)))), setup).await;
    h.scrobbler.start(h.t0);
    assert_eq!(h.status().held_for, None, "two different accounts");
    let creds = Credentials { api_key: KEY.into(), secret: SECRET.into() };
    h.scrobbler.on_connected(creds, Session { username: "bob".into(), key: SK.into() }, h.at(1.0));
    assert!(alert_text(h.scrobbler.take_alert()).is_some_and(|(problem, a)| !problem && a.contains("as bob")));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.mock.calls("track.scrobble").is_empty());

    h.scrobbler.send_held("Bob", h.at(2.0));
    h.answer(3.0).await;
    let status = h.status();
    assert_eq!((status.pending, status.held), (0, 0));
    let sent = h.mock.calls("track.scrobble");
    assert_eq!(sent.len(), 1);
    assert_eq!(batch_size(&sent[0].params), 2);
    assert_eq!(sent[0].params["sk"], SK);
}

#[tokio::test]
async fn discarding_while_disconnected_deletes_the_waiting_plays() {
    let setup = Setup { connected: false, queued: vec![pending("one", 600), pending("two", 300)], ..Setup::default() };
    let mut h = Harness::new("discard", responder(|p| (200, accepted(batch_size(p)))), setup).await;
    h.scrobbler.start(h.t0);
    h.scrobbler.discard(None);
    let status = h.status();
    assert_eq!(status.pending, 0);
    assert!(status.message.as_deref().is_some_and(|m| m.contains("2 scrobbles were deleted")), "{status:?}");
    assert_eq!(h.queue_on_disk(), Some(0));
}

#[tokio::test]
async fn plays_queued_by_an_older_truffle_belong_to_the_saved_sign_in() {
    let setup = Setup { queued: vec![pending_for("old format", 600, "")], ..Setup::default() };
    let mut h = Harness::new("adopt", responder(|p| (200, accepted(batch_size(p)))), setup).await;
    h.scrobbler.start(h.t0);
    assert_eq!(h.status().held, 0);
    h.answer(1.0).await;
    assert_eq!(h.status().pending, 0);
    assert_eq!(h.mock.calls("track.scrobble")[0].params["track[0]"], "old format");
}

#[tokio::test]
async fn a_batch_on_its_way_at_shutdown_is_still_recorded() {
    let setup = Setup { queued: vec![pending("one", 600), pending("two", 300)], ..Setup::default() };
    let mut h = Harness::new("inflight", responder(|p| (200, accepted(batch_size(p)))), setup).await;
    h.scrobbler.start(h.t0);
    assert!(h.scrobbler.is_sending());
    h.scrobbler.shutdown(h.at(0.1));
    // The controller hands over the outcome that arrives after shutdown.
    h.answer(0.5).await;
    assert!(!h.scrobbler.is_sending());
    assert_eq!(h.queue_on_disk(), Some(0), "what Last.fm took isn't sent again at the next start");
    // Nothing new goes out after shutdown.
    h.scrobbler.on_tick(h.at(5.0));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.mock.calls("track.scrobble").len(), 1);
}

#[tokio::test]
async fn the_end_of_a_connection_attempt_is_announced_outside_settings() {
    let mut h = Harness::new("connect-alerts", responder(|p| (200, accepted(batch_size(p)))), Setup {
        connected: false,
        ..Setup::default()
    })
    .await;
    h.scrobbler.start(h.t0);
    // Stand-ins for running attempts (the real one would open a browser).
    h.scrobbler.connecting = Some(Connecting { attempt: 7, task: tokio::spawn(async {}) });
    let failed = Report(ReportKind::Connect { attempt: 7, result: Err(ConnectFailure::TimedOut) });
    assert!(h.scrobbler.on_report(failed, h.at(1.0)).is_none());
    assert!(alert_text(h.scrobbler.take_alert()).is_some_and(|(problem, a)| problem && a.contains("10 minutes")));
    assert!(h.status().message.as_deref().is_some_and(|m| m.contains("10 minutes")));

    h.scrobbler.connecting = Some(Connecting { attempt: 8, task: tokio::spawn(async {}) });
    let creds = Credentials { api_key: KEY.into(), secret: SECRET.into() };
    let session = Session { username: "listener".into(), key: SK.into() };
    let ok = Report(ReportKind::Connect { attempt: 8, result: Ok((creds, session)) });
    assert!(h.scrobbler.on_report(ok, h.at(2.0)).is_some());
    assert!(
        alert_text(h.scrobbler.take_alert())
            .is_some_and(|(problem, a)| !problem && a == "Connected to Last.fm as listener.")
    );
    assert!(h.status().connected);
    assert_eq!(saved_refusal(&h.paths), None);
}
