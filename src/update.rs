//! Updates: Coda's releases on GitHub, looked at once a day (or on "Check now").
//!
//! Only GitHub's public list of releases is read: the versions, their notes and the exe's size and
//! checksum. Nothing about the user or their music is sent (the request carries a plain
//! "coda/<version>" user agent, no ids).
//!
//! The releases are looked for under the repo's new name first and its old one (Truffle) second,
//! and a release's exe may be called coda.exe or (as the app's old name) truffle.exe, so copies
//! from before and after the rename find each other's releases. Installing works from whatever the
//! running exe is called.
//!
//! Installing: the new exe is downloaded next to the running one (`coda.exe.new`) and checked
//! (size, and SHA-256 when GitHub gives one). The running exe is then renamed to a backup
//! (`coda.exe.old-<version>`, Windows lets a running exe be renamed) and the new one takes its
//! name. Coda quits as from the tray (saving the queue and the spot), and once its single-instance
//! lock is let go, `main` starts the new exe on its own ([`spawn_detached`]), with `--resume` when
//! music was playing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where Coda's releases are, in the order asked: the repo's new name, then its old one (a repo
/// that isn't there answers 404 and the next is asked; GitHub also forwards a renamed repo's old
/// name to the new one).
const RELEASES: [&str; 2] = [
    "https://api.github.com/repos/npr-tami/coda/releases?per_page=20",
    "https://api.github.com/repos/npr-tami/Truffle/releases?per_page=20",
];
/// The exe a release carries, in the order looked for: the new name, then the app's old one.
const ASSETS: [&str; 2] = ["coda.exe", "truffle.exe"];
const USER_AGENT: &str = concat!("coda/", env!("CARGO_PKG_VERSION"));
/// Once a day while Coda is open.
pub const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;
/// "Later" asks again after this.
pub const LATER_SECS: u64 = 24 * 60 * 60;
/// Passed to the new exe when music was playing: it plays on from the saved spot.
pub const RESUME_ARG: &str = "--resume";

/// This build's version.
pub const OURS: &str = env!("CARGO_PKG_VERSION");

/// A version as numbers ("v0.1.2" is [0, 1, 2]); `None` when it doesn't start with one. Anything
/// after the numbers ("-beta") is left out.
pub fn parse_version(s: &str) -> Option<[u64; 3]> {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let mut out = [0u64; 3];
    let mut parts = 0;
    for (k, part) in s.split('.').take(3).enumerate() {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        out[k] = digits.parse().ok()?;
        parts += 1;
        if digits.len() != part.len() {
            break;
        }
    }
    (parts > 0).then_some(out)
}

/// Whether `found` is a newer version than `ours` (false when either isn't a version).
pub fn is_newer(found: &str, ours: &str) -> bool {
    matches!((parse_version(found), parse_version(ours)), (Some(f), Some(o)) if f > o)
}

/// Whether two names are the same version ("v0.1.2" and "0.1.2").
pub fn same_version(a: &str, b: &str) -> bool {
    matches!((parse_version(a), parse_version(b)), (Some(x), Some(y)) if x == y)
}

/// What to do about a newer release that was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// Show the popup.
    Popup,
    /// "Later" was chosen and it isn't tomorrow yet: only the title bar's quiet pill.
    Pill,
    /// Nothing: not newer, or skipped.
    Nothing,
}

/// What to do about `found`, given this version, the versions skipped and when Later said to ask
/// again (unix seconds).
pub fn decide(found: &str, ours: &str, skipped: &[String], remind_after: u64, now: u64) -> Ask {
    if !is_newer(found, ours) || skipped.iter().any(|s| same_version(s, found)) {
        Ask::Nothing
    } else if now < remind_after {
        Ask::Pill
    } else {
        Ask::Popup
    }
}

/// Whether the daily check is due: automatic checks on and a day since the last one (or the clock
/// went back past it).
pub fn check_due(auto: bool, last_check: u64, now: u64) -> bool {
    auto && (now >= last_check.saturating_add(CHECK_EVERY_SECS) || now < last_check)
}

/// One release's notes, for the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notes {
    pub version: String,
    /// "20/11/2026", or empty.
    pub date: String,
    /// Its lines, list marks turned into "•".
    pub lines: Vec<String>,
}

/// The newest release above this version that carries the exe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// "0.1.2".
    pub version: String,
    pub url: String,
    pub size: u64,
    /// Lower-case hex, when GitHub gives one.
    pub sha256: Option<String>,
    /// Every release newer than ours, newest first.
    pub notes: Vec<Notes>,
}

#[derive(Debug, Deserialize)]
pub struct ApiRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Deserialize)]
struct ApiAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    browser_download_url: String,
    /// "sha256:<hex>".
    #[serde(default)]
    digest: Option<String>,
}

/// "2026-11-20T10:00:00Z" as "20/11/2026".
fn short_date(iso: &str) -> String {
    let d = iso.get(..10).unwrap_or("");
    let mut p = d.split('-');
    match (p.next(), p.next(), p.next()) {
        (Some(y), Some(m), Some(day)) if y.len() == 4 => format!("{day}/{m}/{y}"),
        _ => String::new(),
    }
}

/// A release's notes as lines: blank lines and headings' marks left out, list marks as "•".
fn note_lines(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let l = l.trim_start_matches('#').trim();
            match l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")) {
                Some(item) => format!("• {}", item.trim()),
                None => l.to_owned(),
            }
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// A release's exe: coda.exe, else truffle.exe (any case).
fn exe_asset(release: &ApiRelease) -> Option<&ApiAsset> {
    ASSETS.iter().find_map(|name| release.assets.iter().find(|a| a.name.eq_ignore_ascii_case(name)))
}

/// From GitHub's list: the newest published release above `ours` with the exe, and the notes of
/// every published release above `ours`. Drafts and pre-releases are left out.
pub fn pick(releases: &[ApiRelease], ours: &str) -> Option<Found> {
    let mut newer: Vec<(&ApiRelease, [u64; 3])> = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease && is_newer(&r.tag_name, ours))
        .filter_map(|r| parse_version(&r.tag_name).map(|v| (r, v)))
        .collect();
    newer.sort_by_key(|r| std::cmp::Reverse(r.1));
    let (v, asset) = newer.iter().find_map(|(r, v)| exe_asset(r).map(|a| (v, a)))?;
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()));
    let notes = newer
        .iter()
        .filter(|(_, rv)| rv <= v)
        .map(|(r, rv)| Notes {
            version: format!("{}.{}.{}", rv[0], rv[1], rv[2]),
            date: r.published_at.as_deref().map(short_date).unwrap_or_default(),
            lines: note_lines(r.body.as_deref().unwrap_or("")),
        })
        .collect();
    Some(Found { version: format!("{}.{}.{}", v[0], v[1], v[2]), url: asset.browser_download_url.clone(), size: asset.size, sha256, notes })
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .build()
        .context("couldn't set up the connection")
}

/// Asks GitHub for Coda's releases: the newest above this version, if any.
pub async fn check() -> Result<Option<Found>> {
    let http = client(Duration::from_secs(30))?;
    let releases = releases_from(&RELEASES, |url| {
        let http = http.clone();
        async move {
            let resp = http
                .get(url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|_| anyhow!("No connection to GitHub"))?;
            let status = resp.status().as_u16();
            let body = if resp.status().is_success() {
                resp.text().await.map_err(|_| anyhow!("GitHub's answer broke off"))?
            } else {
                String::new()
            };
            Ok((status, body))
        }
    })
    .await?;
    Ok(pick(&releases, OURS))
}

/// The releases from the first of `urls` that is there: one answering 404 (a repo under another
/// name) passes on to the next; any other failure stops. `get` gives a url's status and body.
async fn releases_from<F, Fut>(urls: &[&'static str], get: F) -> Result<Vec<ApiRelease>>
where
    F: Fn(&'static str) -> Fut,
    Fut: std::future::Future<Output = Result<(u16, String)>>,
{
    for &url in urls {
        let (status, body) = get(url).await?;
        match status {
            200..=299 => return serde_json::from_str(&body).map_err(|_| anyhow!("GitHub's answer couldn't be read")),
            404 => {}
            _ => bail!("GitHub answered {status}"),
        }
    }
    bail!("GitHub answered 404")
}

/// How a download is going, shared with the UI.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    /// Set by Cancel.
    pub cancel: AtomicBool,
    /// 0 downloading, 1 checking, 2 ready.
    pub stage: AtomicU64,
    /// The end: the downloaded file, or why it failed.
    pub result: Mutex<Option<Result<PathBuf, String>>>,
}

/// The running exe.
pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("couldn't find coda's own exe")
}

/// Where the download goes: next to the running exe.
pub fn download_path(exe: &Path) -> PathBuf {
    exe.with_extension("exe.new")
}

/// Hex of SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether downloaded bytes are the release's exe: the size GitHub gives, and its checksum when
/// there is one. A small "MZ" check too, so an error page never becomes coda.exe.
pub fn verify(bytes: &[u8], size: u64, sha256: Option<&str>) -> Result<(), String> {
    if bytes.len() as u64 != size {
        return Err(format!("The download was {} bytes, not {size}", bytes.len()));
    }
    if !bytes.starts_with(b"MZ") {
        return Err("The download isn't a Windows program".to_owned());
    }
    if let Some(want) = sha256
        && sha256_hex(bytes) != want
    {
        return Err("The download's checksum doesn't match".to_owned());
    }
    Ok(())
}

/// Downloads `found`'s exe to `dest`, checks it, and puts the outcome in `progress.result`.
pub async fn download(found: Found, dest: PathBuf, progress: Arc<Progress>) {
    let result = download_inner(&found, &dest, &progress).await;
    if result.is_err() {
        let _ = std::fs::remove_file(&dest);
    }
    *progress.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result.map(|()| dest));
}

async fn download_inner(found: &Found, dest: &Path, progress: &Progress) -> Result<(), String> {
    let http = client(Duration::from_secs(15 * 60)).map_err(|e| e.to_string())?;
    let mut resp = http.get(&found.url).send().await.map_err(|_| "No connection to GitHub".to_owned())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub answered {}", resp.status()));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(found.size).unwrap_or(0));
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            return Err("Cancelled".to_owned());
        }
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                bytes.extend_from_slice(&chunk);
                progress.done.store(bytes.len() as u64, Ordering::Relaxed);
                if bytes.len() as u64 > found.size.saturating_add(1 << 20) {
                    return Err("The download is bigger than it should be".to_owned());
                }
            }
            Ok(None) => break,
            Err(_) => return Err("The connection broke off".to_owned()),
        }
    }
    progress.stage.store(1, Ordering::Relaxed);
    verify(&bytes, found.size, found.sha256.as_deref())?;
    std::fs::write(dest, &bytes).map_err(|e| format!("Couldn't save the new version: {e}"))?;
    progress.stage.store(2, Ordering::Relaxed);
    Ok(())
}

/// A free backup name next to `exe`: `coda.exe.old-0.1.1`, then `-2`, `-3`… (an older backup is
/// never written over).
pub fn backup_path(exe: &Path, ours: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let name = exe.file_name().map_or_else(|| ASSETS[0].into(), |n| n.to_string_lossy().into_owned());
    let first = exe.with_file_name(format!("{name}.old-{ours}"));
    if !exists(&first) {
        return first;
    }
    (2..1000).map(|k| exe.with_file_name(format!("{name}.old-{ours}-{k}"))).find(|p| !exists(p)).unwrap_or(first)
}

/// Swaps the running exe for `new`: the running one becomes `backup`, `new` takes its name. On
/// failure everything is put back as it was. Returns the backup's path.
pub fn swap(exe: &Path, new: &Path, backup: &Path) -> Result<()> {
    std::fs::rename(exe, backup).context("couldn't move the running coda aside")?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(backup, exe);
        return Err(anyhow!(e).context("couldn't put the new coda in place"));
    }
    Ok(())
}

/// Set once an update is in place: `main` starts the new exe after Coda has closed.
static RESTART: Mutex<Option<(PathBuf, bool)>> = Mutex::new(None);

/// Ask `main` to start `exe` once Coda has closed (with `--resume` when `resume`).
pub fn restart_after_exit(exe: PathBuf, resume: bool) {
    *RESTART.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((exe, resume));
}

/// What [`restart_after_exit`] asked for.
pub fn take_restart() -> Option<(PathBuf, bool)> {
    RESTART.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
}

/// Starts `exe` as a process of its own: no console, its own process group, and out of any job
/// Coda runs in (when the job allows it), so it never ends with whatever started this one.
pub fn spawn_detached(exe: &Path, resume: bool) -> std::io::Result<()> {
    let make = |flags: u32| {
        let mut cmd = std::process::Command::new(exe);
        if resume {
            cmd.arg(RESUME_ARG);
        }
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(flags);
        }
        #[cfg(not(windows))]
        let _ = flags;
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        cmd
    };
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    // A job that doesn't allow breaking away refuses the flag: then without it.
    make(base | CREATE_BREAKAWAY_FROM_JOB).spawn().or_else(|_| make(base).spawn()).map(drop)
}

/// "today at 14:02", "yesterday at 9:05" or "20/11/2026 at 14:02", for `then` (unix seconds) seen
/// at `now`, both shifted by `offset_secs` (the PC's time zone).
pub fn when_text(then: u64, now: u64, offset_secs: i64) -> String {
    let local = |t: u64| (t as i64).saturating_add(offset_secs);
    let (t, n) = (local(then), local(now));
    let (day_t, day_n) = (t.div_euclid(86_400), n.div_euclid(86_400));
    let secs = t.rem_euclid(86_400);
    let clock = format!("{}:{:02}", secs / 3600, secs % 3600 / 60);
    match day_n - day_t {
        0 => format!("today at {clock}"),
        1 => format!("yesterday at {clock}"),
        _ => {
            let (y, m, d) = civil_from_days(day_t);
            format!("{d:02}/{m:02}/{y} at {clock}")
        }
    }
}

/// The date of a day number (days since 1 Jan 1970).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Set by `--resume` (a restart after an update while music played): the saved queue plays on.
static RESUME: AtomicBool = AtomicBool::new(false);

pub fn set_resume() {
    RESUME.store(true, Ordering::Relaxed);
}

/// True once, when this start should play on from the saved spot.
pub fn take_resume() -> bool {
    RESUME.swap(false, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(parse_version("v0.1.2"), Some([0, 1, 2]));
        assert_eq!(parse_version("0.2"), Some([0, 2, 0]));
        assert_eq!(parse_version("1.0.0-beta"), Some([1, 0, 0]));
        assert_eq!(parse_version("nightly"), None);
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("junk", "0.1.1"));
        assert!(same_version("v0.1.2", "0.1.2"));
    }

    #[test]
    fn later_skip_and_off() {
        let none: Vec<String> = Vec::new();
        assert_eq!(decide("0.1.2", "0.1.1", &none, 0, 1_000), Ask::Popup);
        // Later: only the pill until tomorrow, then the popup again.
        let later = 1_000 + LATER_SECS;
        assert_eq!(decide("0.1.2", "0.1.1", &none, later, 1_000 + 60), Ask::Pill);
        assert_eq!(decide("0.1.2", "0.1.1", &none, later, later), Ask::Popup);
        // Skip: never this one again, but a newer one asks.
        let skipped = vec!["0.1.2".to_owned()];
        assert_eq!(decide("v0.1.2", "0.1.1", &skipped, 0, 1_000), Ask::Nothing);
        assert_eq!(decide("0.1.3", "0.1.1", &skipped, 0, 1_000), Ask::Popup);
        // Not newer.
        assert_eq!(decide("0.1.1", "0.1.1", &none, 0, 1_000), Ask::Nothing);
        // Off: no daily check; on, once a day.
        assert!(!check_due(false, 0, 1_000_000));
        assert!(check_due(true, 0, 1_000_000));
        assert!(!check_due(true, 1_000_000, 1_000_000 + 3_600));
        assert!(check_due(true, 1_000_000, 1_000_000 + CHECK_EVERY_SECS));
        assert!(check_due(true, 1_000_000, 10), "a clock set back checks again");
    }

    #[test]
    fn picks_the_newest_release_with_the_exe() {
        let json = r###"[
            {"tag_name":"v0.1.4","draft":true,"assets":[{"name":"coda.exe","size":1,"browser_download_url":"d"}]},
            {"tag_name":"v0.1.3","prerelease":true,"assets":[{"name":"coda.exe","size":1,"browser_download_url":"p"}]},
            {"tag_name":"v0.1.2","body":"## Changes\n- Settings are a page\n\n* Updates","published_at":"2026-11-20T10:00:00Z",
             "assets":[{"name":"coda.exe","size":15,"browser_download_url":"u","digest":"sha256:ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD"}]},
            {"tag_name":"v0.1.1","body":"old","assets":[{"name":"coda.exe","size":9,"browser_download_url":"o"}]},
            {"tag_name":"v0.1.0","assets":[]}
        ]"###;
        let releases: Vec<ApiRelease> = serde_json::from_str(json).unwrap();
        let found = pick(&releases, "0.1.0").unwrap();
        assert_eq!(found.version, "0.1.2");
        assert_eq!(found.url, "u");
        assert_eq!(found.size, 15);
        assert_eq!(found.sha256.as_deref(), Some("abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"));
        assert_eq!(found.notes.len(), 2);
        assert_eq!(found.notes[0].date, "20/11/2026");
        assert_eq!(found.notes[0].lines, vec!["Changes", "• Settings are a page", "• Updates"]);
        assert_eq!(found.notes[1].version, "0.1.1");
        assert!(pick(&releases, "0.1.2").is_none());
        // A newer release without the exe: the newest one with it, and both notes.
        let json = r#"[{"tag_name":"v0.2.0","assets":[]},{"tag_name":"v0.1.5","assets":[{"name":"coda.exe","size":3,"browser_download_url":"x"}]}]"#;
        let releases: Vec<ApiRelease> = serde_json::from_str(json).unwrap();
        let found = pick(&releases, "0.1.1").unwrap();
        assert_eq!(found.version, "0.1.5");
        assert_eq!(found.notes.len(), 1);
    }

    #[test]
    fn an_old_name_exe_is_found_too() {
        // Releases made before the rename carry truffle.exe; newer ones carry both.
        let json = r#"[
            {"tag_name":"v0.1.4","assets":[{"name":"truffle.exe","size":4,"browser_download_url":"old4"},{"name":"coda.exe","size":5,"browser_download_url":"new4"}]},
            {"tag_name":"v0.1.3","assets":[{"name":"Truffle.exe","size":3,"browser_download_url":"old3"}]},
            {"tag_name":"v0.1.2","assets":[{"name":"notes.txt","size":1,"browser_download_url":"n"}]}
        ]"#;
        let releases: Vec<ApiRelease> = serde_json::from_str(json).unwrap();
        let found = pick(&releases, "0.1.2").unwrap();
        assert_eq!((found.version.as_str(), found.url.as_str(), found.size), ("0.1.4", "new4", 5), "coda.exe first");
        let found = pick(&releases[1..], "0.1.2").unwrap();
        assert_eq!((found.version.as_str(), found.url.as_str()), ("0.1.3", "old3"), "else truffle.exe");
        assert!(pick(&releases[2..], "0.1.1").is_none(), "a release without either exe");
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    #[test]
    fn the_old_repo_is_asked_when_the_new_one_isnt_there() {
        let asked = Mutex::new(Vec::new());
        let list = r#"[{"tag_name":"v9.0.0","assets":[{"name":"truffle.exe","size":2,"browser_download_url":"t"}]}]"#;
        let get = |url: &'static str| {
            asked.lock().unwrap().push(url);
            let answer = if url.contains("/coda/") { (404, String::new()) } else { (200, list.to_owned()) };
            async move { Ok(answer) }
        };
        let releases = run(releases_from(&RELEASES, get)).unwrap();
        assert_eq!(*asked.lock().unwrap(), RELEASES.to_vec());
        assert_eq!(pick(&releases, OURS).unwrap().url, "t");
        assert!(RELEASES[0].contains("/npr-tami/coda/") && RELEASES[1].contains("/npr-tami/Truffle/"));
    }

    #[test]
    fn the_new_repo_is_enough_when_it_is_there() {
        let asked = Mutex::new(Vec::new());
        let get = |url: &'static str| {
            asked.lock().unwrap().push(url);
            async move { Ok((200, "[]".to_owned())) }
        };
        assert!(run(releases_from(&RELEASES, get)).unwrap().is_empty());
        assert_eq!(*asked.lock().unwrap(), vec![RELEASES[0]]);
    }

    #[test]
    fn other_failures_dont_fall_back() {
        let asked = Mutex::new(Vec::new());
        let get = |url: &'static str| {
            asked.lock().unwrap().push(url);
            async move { Ok((403, String::new())) }
        };
        let e = run(releases_from(&RELEASES, get)).unwrap_err();
        assert_eq!(e.to_string(), "GitHub answered 403");
        assert_eq!(asked.lock().unwrap().len(), 1);
        // Neither repo there: the 404 is told.
        let e = run(releases_from(&RELEASES, |_| async { Ok((404, String::new())) })).unwrap_err();
        assert_eq!(e.to_string(), "GitHub answered 404");
    }

    #[test]
    fn an_exe_installed_under_its_old_name_keeps_that_name() {
        // A coda.exe that an old updater put in place as truffle.exe: the download and the
        // backup go by the running exe's own name.
        let dir = Path::new("X:/apps");
        let exe = dir.join("truffle.exe");
        assert_eq!(download_path(&exe), dir.join("truffle.exe.new"));
        assert_eq!(backup_path(&exe, "0.1.3", |_| false), dir.join("truffle.exe.old-0.1.3"));
        assert_eq!(download_path(&dir.join("coda.exe")), dir.join("coda.exe.new"));
    }

    #[test]
    fn a_download_must_be_the_release() {
        let exe = b"MZ rest of it";
        let sum = sha256_hex(exe);
        assert!(verify(exe, exe.len() as u64, Some(&sum)).is_ok());
        assert!(verify(exe, exe.len() as u64, None).is_ok());
        assert!(verify(exe, 99, Some(&sum)).is_err());
        assert!(verify(exe, exe.len() as u64, Some(&"0".repeat(64))).is_err());
        assert!(verify(b"<html>error", 11, None).is_err());
    }

    #[test]
    fn times_read_plainly() {
        // 20 Nov 2026 14:02 UTC.
        let t = 1_795_183_320;
        assert_eq!(when_text(t, t + 60, 0), "today at 14:02");
        assert_eq!(when_text(t, t + 86_400, 0), "yesterday at 14:02");
        assert_eq!(when_text(t, t + 3 * 86_400, 0), "20/11/2026 at 14:02");
        assert_eq!(when_text(t, t + 60, 3_600), "today at 15:02");
    }

    #[test]
    fn backups_are_never_written_over() {
        let dir = Path::new("X:/apps");
        let exe = dir.join("coda.exe");
        assert_eq!(backup_path(&exe, "0.1.1", |_| false), dir.join("coda.exe.old-0.1.1"));
        let taken = [dir.join("coda.exe.old-0.1.1"), dir.join("coda.exe.old-0.1.1-2")];
        assert_eq!(backup_path(&exe, "0.1.1", |p| taken.iter().any(|t| t == p)), dir.join("coda.exe.old-0.1.1-3"));
    }
}
