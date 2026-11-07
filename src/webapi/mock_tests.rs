//! End-to-end tests of the HTTP layer against a local mock server (no network needed).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use base64::prelude::{BASE64_STANDARD, Engine as _};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::auth::{self, StoredToken};
use super::*;

const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    /// Lower-cased request head.
    head: String,
    body: Vec<u8>,
}

impl Seen {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    fn query(&self, key: &str) -> Option<String> {
        let url = url::Url::parse(&format!("http://x{}", self.target)).ok()?;
        url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
    }
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn json(status: u16, body: &str) -> Self {
        Self { status, headers: vec![("Content-Type", "application/json".into())], body: body.to_owned() }
    }

    fn header(mut self, key: &'static str, value: &str) -> Self {
        self.headers.push((key, value.to_owned()));
        self
    }
}

type Responder = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

struct Mock {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    async fn start(responder: Responder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let responder = responder.clone();
                let log = log.clone();
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
                    let content_length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    while buf.len() < head_end + content_length {
                        let n = stream.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let first = String::from_utf8_lossy(&buf[..head_end]).lines().next().unwrap_or("").to_owned();
                    let mut parts = first.split(' ');
                    let request = Seen {
                        method: parts.next().unwrap_or("").to_owned(),
                        target: parts.next().unwrap_or("").to_owned(),
                        head,
                        body: buf[head_end..buf.len().min(head_end + content_length)].to_vec(),
                    };
                    log.lock().unwrap().push(request.clone());
                    let reply = responder(&request);
                    let mut out = format!(
                        "HTTP/1.1 {} Mock\r\nContent-Length: {}\r\nConnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (k, v) in &reply.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&reply.body);
                    let _ = stream.write_all(out.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { base, seen }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("truffle-webapi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn token(expires_at: u64) -> StoredToken {
    StoredToken {
        client_id: CLIENT_ID.into(),
        access_token: "access-1".into(),
        refresh_token: "refresh-1".into(),
        expires_at,
        authorized_at: auth::now_unix(),
        scope: SCOPES.join(" "),
    }
}

fn api_for(mock: &Mock, token_path: PathBuf, expires_at: u64) -> WebApi {
    let token = token(expires_at);
    auth::save_token(&token_path, &token).unwrap();
    WebApi::from_parts(build_http_client().unwrap(), token_path, token, endpoints(mock))
}

fn endpoints(mock: &Mock) -> Endpoints {
    Endpoints { api_base: mock.base.clone(), token_url: format!("{}/token", mock.base) }
}

fn far_future() -> u64 {
    auth::now_unix() + 3600
}

/// All scenarios share the process-wide rate-limit gate, so they run in one test, in order.
#[tokio::test]
async fn http_layer_end_to_end() {
    let dir = TempDir::new("e2e");
    let me_hits = Arc::new(AtomicUsize::new(0));
    let delete_hits = Arc::new(AtomicUsize::new(0));
    let responder: Responder = {
        let me_hits = me_hits.clone();
        let delete_hits = delete_hits.clone();
        Arc::new(move |req: &Seen| match (req.method.as_str(), req.path()) {
            ("GET", "/me") => {
                if me_hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    Reply::json(503, r#"{"error":{"status":503,"message":"busy"}}"#)
                } else {
                    Reply::json(200, r#"{"id":"me","display_name":"Me","images":[{"url":"u","width":64}]}"#)
                }
            }
            ("GET", "/playlists/abc") => Reply::json(
                200,
                r#"{"id":"abc","name":"Mine","description":null,"collaborative":false,"snapshot_id":"snap1",
                    "owner":{"id":"me","display_name":"Me"},"images":[],"items":{"total":3}}"#,
            ),
            ("PUT", "/playlists/abc") => Reply::json(200, ""),
            ("PUT", "/playlists/abc/images") => Reply::json(202, ""),
            ("PUT", "/me/library") => Reply::json(200, ""),
            ("DELETE", "/me/library") => {
                if delete_hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    Reply::json(429, r#"{"error":{"status":429,"message":"Too many requests"}}"#).header("Retry-After", "1")
                } else {
                    Reply::json(200, "")
                }
            }
            ("GET", "/me/tracks") => {
                let offset: usize = req.query("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
                let total = 120;
                let items: Vec<String> = (offset..(offset + 50).min(total))
                    .map(|i| {
                        if i == 7 {
                            r#"{"added_at":null,"track":null}"#.to_owned()
                        } else {
                            format!(r#"{{"track":{{"type":"track","uri":"spotify:track:t{i}","name":"T{i}","duration_ms":1000}}}}"#)
                        }
                    })
                    .collect();
                Reply::json(200, &format!(r#"{{"total":{total},"items":[{}]}}"#, items.join(",")))
            }
            _ => Reply::json(404, r#"{"error":{"status":404,"message":"nope"}}"#),
        })
    };
    let mock = Mock::start(responder).await;
    let api = api_for(&mock, dir.0.join("token.json"), far_future());

    // 5xx is retried; bearer token is sent.
    let profile = api.me().await.unwrap();
    assert_eq!((profile.username.as_str(), profile.display_name.as_str()), ("me", "Me"));
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[1].head.contains("authorization: bearer access-1"));

    // Only provided fields are sent.
    api.update_playlist_details("abc", Some("  New name "), None).await.unwrap();
    let last = mock.requests().pop().unwrap();
    assert_eq!(last.method, "PUT");
    let body: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(body, serde_json::json!({"name": "New name"}));
    api.update_playlist_details("abc", None, Some("line one\nline two")).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&mock.requests().pop().unwrap().body).unwrap();
    assert_eq!(body, serde_json::json!({"description": "line one line two"}));
    assert!(api.update_playlist_details("abc", Some("   "), None).await.is_err());

    // Liked songs are saved in batches of 40; local files are skipped; empty PUT has Content-Length 0.
    let before = mock.requests().len();
    let mut uris: Vec<String> = (0..45).map(|i| format!("spotify:track:x{i}")).collect();
    uris.push("spotify:local:a:b:c:1".into());
    api.set_liked(&uris, true).await.unwrap();
    let puts: Vec<Seen> = mock.requests()[before..].to_vec();
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[0].query("uris").unwrap().split(',').count(), 40);
    assert_eq!(puts[1].query("uris").unwrap().split(',').count(), 5);
    assert!(puts[0].head.contains("content-length: 0"));
    assert!(api.set_liked(&["spotify:local:a:b:c:1".to_owned()], true).await.is_err());

    // A short 429 is waited out and retried.
    let started = Instant::now();
    api.set_liked(&["spotify:track:x1".to_owned()], false).await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    assert_eq!(delete_hits.load(Ordering::SeqCst), 2);

    // Cover upload: base64 JPEG body with Content-Type image/jpeg, 202 accepted.
    let image_path = dir.0.join("cover.png");
    image::RgbImage::from_fn(900, 700, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]))
        .save(&image_path)
        .unwrap();
    api.upload_playlist_cover("abc", &image_path).await.unwrap();
    let upload = mock.requests().pop().unwrap();
    assert_eq!(upload.path(), "/playlists/abc/images");
    assert!(upload.head.contains("content-type: image/jpeg"));
    assert!(upload.body.len() <= MAX_COVER_BASE64_LEN);
    let jpeg = BASE64_STANDARD.decode(&upload.body).unwrap();
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);

    // Liked songs: three pages, null entry skipped, order kept, progress reported.
    let calls = Mutex::new(Vec::new());
    let (tracks, requests) = api
        .liked_tracks(&|loaded, total| calls.lock().unwrap().push((loaded, total)))
        .await
        .unwrap();
    assert_eq!(requests, 3);
    assert_eq!(tracks.len(), 119);
    assert_eq!(tracks[0].uri, "spotify:track:t0");
    assert_eq!(tracks[7].uri, "spotify:track:t8");
    assert_eq!(tracks[118].uri, "spotify:track:t119");
    let calls = calls.into_inner().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls.last(), Some(&(119, Some(120))));

    liked_since_reads_only_the_new_songs().await;
    refresh_after_401().await;
    invalid_grant_requires_reconnect().await;
    quota_exceeded_is_not_retried().await;
}

/// A song id as Spotify writes it (22 characters).
fn song(id: &str) -> String {
    format!("spotify:track:{id:0>22}")
}

/// Liked songs on the mock: uri and the second of 1 June 2025 it was liked (an empty uri is an
/// entry Spotify can't describe), newest first.
type Likes = Arc<Mutex<Vec<(String, i64)>>>;

fn liked_reply(likes: &[(String, i64)], req: &Seen) -> Reply {
    let offset: usize = req.query("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
    let limit: usize = req.query("limit").and_then(|l| l.parse().ok()).unwrap_or(20);
    let items: Vec<String> = likes
        .iter()
        .skip(offset)
        .take(limit)
        .map(|(uri, s)| {
            let at = format!("2025-06-01T{:02}:{:02}:{:02}Z", s / 3_600, s / 60 % 60, s % 60);
            if uri.is_empty() {
                format!(r#"{{"added_at":"{at}","track":null}}"#)
            } else {
                format!(r#"{{"added_at":"{at}","track":{{"type":"track","uri":"{uri}","name":"Spotify's {uri} 🎵","duration_ms":1000}}}}"#)
            }
        })
        .collect();
    Reply::json(200, &format!(r#"{{"total":{},"items":[{}]}}"#, likes.len(), items.join(",")))
}

/// The saved copy of `likes` (without the entries Spotify can't describe).
fn saved(likes: &[(String, i64)]) -> Vec<TrackInfo> {
    let day = crate::library::days_from_civil(2025, 6, 1) * 86_400;
    likes
        .iter()
        .filter(|(uri, _)| !uri.is_empty())
        .map(|(uri, s)| TrackInfo {
            uri: uri.clone(),
            name: format!("Saved {uri}"),
            artists: Vec::new(),
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 1000,
            playable: true,
            is_local: false,
            added_at: day + s,
        })
        .collect()
}

async fn liked_since_reads_only_the_new_songs() {
    let dir = TempDir::new("liked-since");
    let likes: Likes = Arc::default();
    let responder: Responder = {
        let likes = likes.clone();
        Arc::new(move |req: &Seen| match req.path() {
            "/me/tracks" => liked_reply(&likes.lock().unwrap(), req),
            _ => Reply::json(404, "{}"),
        })
    };
    let mock = Mock::start(responder).await;
    let api = api_for(&mock, dir.0.join("token.json"), far_future());
    let quiet = |_: usize, _: Option<usize>| {};

    // 36,864 saved, then 5 liked on top: the check and one page, the rest kept as saved.
    let old: Vec<(String, i64)> = (0..36_864).map(|k| (song(&k.to_string()), 80_000 - k)).collect();
    let known = saved(&old);
    let mut now: Vec<(String, i64)> = (0..5).map(|k| (song(&format!("new{k}")), 80_005 - k)).collect();
    now.extend(old.iter().cloned());
    *likes.lock().unwrap() = now;
    api.liked_fingerprint().await.unwrap();
    let (songs, requests) = api.liked_tracks_since(&known, 0, &quiet).await.unwrap();
    assert_eq!(requests, 1);
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(songs.len(), 36_869);
    assert_eq!(songs[0].uri, song("new0"));
    assert_eq!(songs[4].name, format!("Spotify's {} 🎵", song("new4")));
    assert_eq!(songs[4].added_at, known[0].added_at + 1);
    assert!(songs[5..] == known[..], "the saved copy is kept as it was");

    // A song liked again moves to the top; songs liked in the same second come in another order.
    let mut old: Vec<(String, i64)> = (0..120).map(|k| (song(&k.to_string()), 1_000 - k)).collect();
    for like in &mut old[..3] {
        like.1 = 1_000;
    }
    let known = saved(&old);
    let mut now = vec![(song("60"), 2_000), old[2].clone(), old[0].clone(), old[1].clone()];
    now.extend(old[3..].iter().filter(|(uri, _)| *uri != song("60")).cloned());
    *likes.lock().unwrap() = now;
    let (songs, requests) = api.liked_tracks_since(&known, 0, &quiet).await.unwrap();
    assert_eq!((songs.len(), requests), (120, 1));
    assert_eq!(songs[0].uri, song("60"));
    assert_eq!(songs.iter().filter(|t| t.uri == song("60")).count(), 1, "once, in its new place");
    assert!(songs[1..4] == known[..3]);

    // An entry Spotify can't describe (left out of the saved copy) still adds up.
    let mut old: Vec<(String, i64)> = (0..120).map(|k| (song(&k.to_string()), 1_000 - k)).collect();
    old[70].0.clear();
    let known = saved(&old);
    let mut now = vec![(song("new"), 2_000)];
    now.extend(old.iter().cloned());
    *likes.lock().unwrap() = now;
    let (songs, requests) = api.liked_tracks_since(&known, 1, &quiet).await.unwrap();
    assert_eq!((songs.len(), requests), (120, 1));

    // A song unliked further down: Spotify's total doesn't add up, so all of it is read again.
    let old: Vec<(String, i64)> = (0..120).map(|k| (song(&k.to_string()), 1_000 - k)).collect();
    let known = saved(&old);
    let mut now = vec![(song("newA"), 2_000), (song("newB"), 1_999)];
    now.extend(old.iter().filter(|(uri, _)| *uri != song("80")).cloned());
    *likes.lock().unwrap() = now;
    let (songs, requests) = api.liked_tracks_since(&known, 0, &quiet).await.unwrap();
    assert_eq!((songs.len(), requests), (121, 1 + 3));
    assert!(songs.iter().all(|t| t.uri != song("80")));
    assert!(songs.iter().all(|t| t.name.starts_with("Spotify's")), "read again, not kept");

    // A saved copy without dates (older Truffles) never meets: every song is read.
    let undated: Vec<TrackInfo> = known.iter().cloned().map(|t| TrackInfo { added_at: 0, ..t }).collect();
    let (songs, requests) = api.liked_tracks_since(&undated, 0, &quiet).await.unwrap();
    assert_eq!((songs.len(), requests), (121, 3));

    // Nothing liked at all.
    likes.lock().unwrap().clear();
    let (songs, requests) = api.liked_tracks_since(&known, 0, &quiet).await.unwrap();
    assert_eq!((songs.len(), requests), (0, 1));
    let (songs, requests) = api.liked_tracks_since(&[], 0, &quiet).await.unwrap();
    assert_eq!((songs.len(), requests), (0, 1));
}

async fn refresh_after_401() {
    let dir = TempDir::new("refresh");
    let responder: Responder = Arc::new(|req: &Seen| match (req.method.as_str(), req.path()) {
        ("POST", "/token") => {
            let form: std::collections::HashMap<String, String> =
                url::form_urlencoded::parse(&req.body).into_owned().collect();
            assert_eq!(form["grant_type"], "refresh_token");
            assert_eq!(form["refresh_token"], "refresh-1");
            assert_eq!(form["client_id"], CLIENT_ID);
            Reply::json(200, r#"{"access_token":"access-2","token_type":"Bearer","expires_in":3600}"#)
        }
        ("GET", "/me") if req.head.contains("bearer access-1") => {
            Reply::json(401, r#"{"error":{"status":401,"message":"The access token expired"}}"#)
        }
        ("GET", "/me") => Reply::json(200, r#"{"id":"me","display_name":null,"images":null}"#),
        _ => Reply::json(404, "{}"),
    });
    let mock = Mock::start(responder).await;
    let token_path = dir.0.join("token.json");
    let api = api_for(&mock, token_path.clone(), far_future());
    let profile = api.me().await.unwrap();
    assert_eq!(profile.display_name, "me");
    let stored = auth::load_token(&token_path).unwrap();
    assert_eq!(stored.access_token, "access-2");
    assert_eq!(stored.refresh_token, "refresh-1", "old refresh token kept when none is returned");
    assert_eq!(mock.requests().len(), 3);

    // After a Disconnect (file deleted), an old instance's refresh must not bring the file back.
    let stale = api_for(&mock, token_path.clone(), 0);
    auth::delete_token(&token_path).unwrap();
    stale.me().await.unwrap();
    assert!(!token_path.exists());
}

async fn invalid_grant_requires_reconnect() {
    let dir = TempDir::new("grant");
    let responder: Responder = Arc::new(|req: &Seen| match req.path() {
        "/token" => Reply::json(400, r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#),
        _ => Reply::json(200, r#"{"id":"me"}"#),
    });
    let mock = Mock::start(responder).await;
    let token_path = dir.0.join("token.json");
    let api = api_for(&mock, token_path.clone(), 0);
    let err = api.me().await.unwrap_err();
    assert!(is_reconnect_required(&err), "{err}");
    assert!(!token_path.exists(), "dead token file is deleted");
    let err = api.me().await.unwrap_err();
    assert!(is_reconnect_required(&err));
    assert_eq!(mock.requests().len(), 1, "no further requests after the token died");
}

async fn quota_exceeded_is_not_retried() {
    let dir = TempDir::new("quota");
    let responder: Responder = Arc::new(|_req: &Seen| {
        Reply::json(429, r#"{"error":{"status":429,"message":"Too many requests","reason":"QUOTA_EXCEEDED"}}"#)
            .header("Retry-After", "7200")
    });
    let mock = Mock::start(responder).await;
    let api = api_for(&mock, dir.0.join("token.json"), far_future());
    let err = api.me().await.unwrap_err().to_string();
    assert!(err.contains("quota") && err.contains("2 hours"), "{err}");
    let err = api.me().await.unwrap_err().to_string();
    assert!(err.contains("paused"), "{err}");
    assert_eq!(mock.requests().len(), 1, "quota 429 is neither retried nor repeated");
}

#[tokio::test]
async fn connect_without_browser() {
    let dir = TempDir::new("connect");
    let paths = Paths { config: dir.0.join("config"), cache: dir.0.join("cache") };
    std::fs::create_dir_all(&paths.config).unwrap();

    // Nothing stored: not connected, no browser.
    assert!(WebApi::connect(CLIENT_ID, &paths, false).await.unwrap().is_none());
    // Invalid client id: plain error.
    assert!(WebApi::connect("not-an-id", &paths, false).await.is_err());

    // A valid stored token is used without any network access.
    auth::save_token(&paths.webapi_token_file(), &token(far_future())).unwrap();
    let api = WebApi::connect(CLIENT_ID, &paths, false).await.unwrap().expect("connected");
    assert_eq!(api.inner.client_id, CLIENT_ID);
    assert!(api.days_until_reconnect().is_some_and(|d| (180..=182).contains(&d)));

    // A token stored for another client id is ignored.
    assert!(WebApi::connect("fedcba9876543210fedcba9876543210", &paths, false).await.unwrap().is_none());

    // A token missing scopes can't be used: the user is asked to reconnect.
    let mut narrow = token(far_future());
    narrow.scope = "user-read-private".into();
    auth::save_token(&paths.webapi_token_file(), &narrow).unwrap();
    let err = WebApi::connect(CLIENT_ID, &paths, false).await.unwrap_err();
    assert!(is_reconnect_required(&err), "{err}");

    // An expired sign-in that Spotify refuses to refresh: file deleted, user told to reconnect.
    let responder: Responder = Arc::new(|_req: &Seen| {
        Reply::json(400, r#"{"error":"invalid_grant","error_description":"Refresh token expired"}"#)
    });
    let mock = Mock::start(responder).await;
    auth::save_token(&paths.webapi_token_file(), &token(0)).unwrap();
    let err = WebApi::connect_with(CLIENT_ID, &paths, false, endpoints(&mock)).await.unwrap_err();
    assert!(is_reconnect_required(&err), "{err}");
    assert!(!paths.webapi_token_file().exists());
    assert!(WebApi::connect(CLIENT_ID, &paths, false).await.unwrap().is_none());

    // Spotify unreachable while refreshing: still connected, refresh retried on first use.
    auth::save_token(&paths.webapi_token_file(), &token(0)).unwrap();
    let offline = Endpoints { api_base: "http://127.0.0.1:9".into(), token_url: "http://127.0.0.1:9/token".into() };
    assert!(WebApi::connect_with(CLIENT_ID, &paths, false, offline).await.unwrap().is_some());

    // (WebApi::forget also cancels pending sign-ins, which would race with the auth tests.)
    auth::delete_token(&paths.webapi_token_file()).unwrap();
    assert!(!paths.webapi_token_file().exists());
    auth::delete_token(&paths.webapi_token_file()).unwrap();
}
