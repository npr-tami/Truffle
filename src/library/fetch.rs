//! Network side of the library: thin, retrying wrappers around librespot's `SpClient`.
//! All functions return `librespot_core::Error` so callers can look at `ErrorKind`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use librespot_core::{Error as CoreError, Session, SpotifyId, error::ErrorKind};
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use librespot_protocol::metadata as pb;
use librespot_protocol::playlist4_external::{ListChanges, MetaItem, SelectedListContent};
use protobuf::{EnumOrUnknown, Message};
use reqwest::Method;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};

use super::convert::{self, Market};
use super::wire;
use crate::model::TrackInfo;

/// Track metadata entities per extended-metadata request.
pub const METADATA_BATCH: usize = 500;
/// Extended-metadata requests in flight at once.
pub const METADATA_CONCURRENCY: usize = 4;
/// Batches that the server rejects as too large are split down to this size.
const MIN_SPLIT_BATCH: usize = 25;
/// Batch size for re-requesting entities a response left out.
const RETRY_BATCH: usize = 100;
/// Items per page when a playlist has to be paged explicitly.
const PLAYLIST_PAGE: usize = 2000;
/// Safety cap for every paging loop.
const MAX_PAGES: usize = 500;
/// Items per collection page.
const COLLECTION_PAGE: i32 = 300;
/// A collection page that timed out is asked for again at half the size, down to this.
const MIN_COLLECTION_PAGE: i32 = 50;
/// Tries per collection page (spclient already repeats network errors within each).
const PAGE_TRIES: u32 = 4;
/// Safety cap for paging through a collection set (250,000 items at the smallest page size).
const MAX_SET_PAGES: usize = 5_000;

const COLLECTION_SET: &str = "collection";
/// The collection set of the artists the user follows.
const ARTIST_SET: &str = "artist";
const COLLECTION_MIME: &str = "application/vnd.collection-v2.spotify.proto";
const PROTOBUF_MIME: &str = "application/x-protobuf";

/// Errors worth another attempt (spclient already retries plain network errors internally).
pub fn is_transient(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Unavailable
            | ErrorKind::DeadlineExceeded
            | ErrorKind::ResourceExhausted
            | ErrorKind::Unknown
            | ErrorKind::Aborted
    )
}

/// The first wait before another attempt (later ones grow from it).
#[cfg(not(test))]
const RETRY_UNIT_MS: u64 = 1_000;
#[cfg(test)]
const RETRY_UNIT_MS: u64 = 1;

fn retry_delay(kind: ErrorKind, attempt: u32) -> Duration {
    let base = match kind {
        // 429 with a Retry-After longer than librespot's 10 s limit.
        ErrorKind::ResourceExhausted => 5 * RETRY_UNIT_MS * 3u64.pow(attempt.saturating_sub(1)),
        _ => RETRY_UNIT_MS * 3u64.pow(attempt.saturating_sub(1)),
    };
    Duration::from_millis(base + rand::random_range(0..=RETRY_UNIT_MS * 2 / 5))
}

/// Run `op` with retries and exponential backoff on transient errors.
pub async fn retry<T, F, Fut>(what: &str, mut op: F) -> Result<T, CoreError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, CoreError>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let max_attempts = match e.kind {
                    ErrorKind::ResourceExhausted => 3,
                    // spclient already tried several access points for these.
                    ErrorKind::Unavailable | ErrorKind::DeadlineExceeded => 2,
                    k if is_transient(k) => 3,
                    _ => 1,
                };
                if attempt >= max_attempts {
                    return Err(e);
                }
                let delay = retry_delay(e.kind, attempt);
                log::warn!("{what} failed ({e}); retrying in {} ms", delay.as_millis());
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn get(session: &Session, what: &str, endpoint: &str) -> Result<Bytes, CoreError> {
    retry(what, || async move { session.spclient().request(&Method::GET, endpoint, None, None).await }).await
}

fn parse_list(bytes: &[u8]) -> Result<SelectedListContent, CoreError> {
    Ok(SelectedListContent::parse_from_bytes(bytes)?)
}

// ---------------------------------------------------------------------------------------------
// Rootlist

pub struct RootEntry {
    /// Normalized `spotify:playlist:<id>`.
    pub uri: String,
    /// Decoration from the rootlist (attributes, length, owner, revision), if it could be
    /// matched to the item.
    pub meta: Option<MetaItem>,
}

pub struct Rootlist {
    pub revision: Vec<u8>,
    pub entries: Vec<RootEntry>,
}

/// The user's rootlist, folders flattened in order. Uses
/// `SpClient::get_rootlist(from, length)` (`/playlist/v2/user/{user}/rootlist?decorate=
/// revision,attributes,length,owner,capabilities,status_code&from=&length=`).
pub async fn rootlist(session: &Session) -> Result<Rootlist, CoreError> {
    let mut revision = Vec::new();
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut from = 0usize;

    for page_no in 0..MAX_PAGES {
        let bytes = match retry("library list", || async move { session.spclient().get_rootlist(from, None).await }).await {
            Ok(bytes) => bytes,
            // An account that never had a playlist has no rootlist yet.
            Err(e) if page_no == 0 && e.kind == ErrorKind::NotFound => {
                log::info!("no rootlist for this account yet ({e})");
                break;
            }
            Err(e) => return Err(e),
        };
        let msg = parse_list(&bytes)?;
        if page_no == 0 {
            revision = msg.revision().to_vec();
        }
        let contents = msg.contents.get_or_default();
        let items = &contents.items;
        let metas = &contents.meta_items;
        let playlist_count = items.iter().filter(|i| convert::normalize_playlist_uri(i.uri()).is_some()).count();

        // Decorations are normally one per item, in order. Be defensive about other shapes.
        let aligned_all = metas.len() == items.len();
        let aligned_playlists = !aligned_all && metas.len() == playlist_count;
        if !aligned_all && !aligned_playlists && !metas.is_empty() {
            log::warn!(
                "rootlist page has {} items but {} decorations; fetching playlist details one by one",
                items.len(),
                metas.len()
            );
        }

        let mut playlist_idx = 0usize;
        for (idx, item) in items.iter().enumerate() {
            let uri = item.uri();
            let Some(norm) = convert::normalize_playlist_uri(uri) else {
                if !uri.starts_with("spotify:start-group:") && !uri.starts_with("spotify:end-group:") {
                    log::debug!("skipping non-playlist rootlist item {uri}");
                }
                continue;
            };
            let meta = if aligned_all {
                metas.get(idx).cloned()
            } else if aligned_playlists {
                metas.get(playlist_idx).cloned()
            } else {
                None
            };
            playlist_idx += 1;
            if !seen.insert(norm.clone()) {
                continue;
            }
            let meta = meta.filter(|m| m.attributes.is_some() || m.has_length() || !m.revision().is_empty());
            if let Some(m) = &meta
                && m.has_status_code()
                && !matches!(m.status_code(), 0 | 200)
            {
                log::debug!("rootlist item {norm} has status {}", m.status_code());
            }
            entries.push(RootEntry { uri: norm, meta });
        }

        let returned = items.len();
        from += returned;
        let more = if msg.has_length() { from < msg.length().max(0) as usize } else { contents.truncated() };
        if !more || returned == 0 {
            break;
        }
    }
    Ok(Rootlist { revision, entries })
}

/// Light per-playlist attributes for rootlist entries that came without decorations.
pub async fn playlist_header(session: &Session, id: &SpotifyId) -> Result<SelectedListContent, CoreError> {
    let endpoint = format!(
        "/playlist/v2/playlist/{}?decorate=revision,attributes,length,owner,capabilities&from=0&length=1",
        id.to_base62()?
    );
    match get(session, "playlist details", &endpoint).await {
        Ok(bytes) => parse_list(&bytes),
        // Parameters not accepted: the plain endpoint returns the same fields (and the items).
        Err(e) if e.kind == ErrorKind::InvalidArgument => {
            parse_list(&retry("playlist details", || async move { session.spclient().get_playlist(id).await }).await?)
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------------------------
// Playlist contents

pub struct PlaylistItems {
    pub revision: Vec<u8>,
    pub uris: Vec<String>,
    /// When each item was added (Unix seconds, 0 when unknown), beside `uris`.
    pub added: Vec<i64>,
    /// Every item came. Not when the pages stopped short (an empty page, paging ignored, too
    /// many pages): then the playlist has more than `uris`.
    pub complete: bool,
}

/// An item's "added" time in seconds (Spotify keeps milliseconds).
fn item_added(i: &librespot_protocol::playlist4_external::Item) -> i64 {
    i.attributes.timestamp().max(0) / 1000
}

/// Every item URI of a playlist, in order, duplicates kept. First
/// `SpClient::get_playlist` (`/playlist/v2/playlist/{id}`), then, if the server truncated the
/// list, explicit `?from=&length=` pages. Restarts once if the playlist changes mid-way. A read
/// that stops short still returns what came (see `complete`).
pub async fn playlist_items(session: &Session, id: &SpotifyId) -> Result<PlaylistItems, CoreError> {
    let b62 = id.to_base62()?;
    let mut restarted = false;
    'restart: loop {
        let bytes = retry("playlist", || async move { session.spclient().get_playlist(id).await }).await?;
        let msg = parse_list(&bytes)?;
        let revision = msg.revision().to_vec();
        let total = msg.has_length().then(|| msg.length().max(0) as usize);
        let contents = msg.contents.get_or_default();
        let mut uris: Vec<String> = Vec::with_capacity(total.unwrap_or(contents.items.len()));
        uris.extend(contents.items.iter().map(|i| i.uri().to_owned()));
        let mut added: Vec<i64> = contents.items.iter().map(item_added).collect();
        let mut truncated = contents.truncated();
        drop(msg);

        for _ in 0..MAX_PAGES {
            if !more_to_read(total, uris.len(), truncated) {
                break;
            }
            let from = uris.len();
            let endpoint = format!("/playlist/v2/playlist/{b62}?from={from}&length={PLAYLIST_PAGE}");
            let page = parse_list(&get(session, "playlist page", &endpoint).await?)?;
            if !page.revision().is_empty() && page.revision() != revision.as_slice() {
                if restarted {
                    return Err(CoreError::aborted("the playlist kept changing while it was loading"));
                }
                log::info!("playlist {b62} changed while loading; starting over");
                restarted = true;
                continue 'restart;
            }
            let c = page.contents.get_or_default();
            if c.items.is_empty() {
                break;
            }
            if c.has_pos() && c.pos().max(0) as usize != from {
                log::warn!("playlist {b62}: server ignored paging (pos {} != {from})", c.pos());
                break;
            }
            uris.extend(c.items.iter().map(|i| i.uri().to_owned()));
            added.extend(c.items.iter().map(item_added));
            truncated = c.truncated();
        }
        if let Some(t) = total
            && uris.len() != t
        {
            log::warn!("playlist {b62}: got {} items, expected {t}", uris.len());
        }
        let complete = !more_to_read(total, uris.len(), truncated);
        return Ok(PlaylistItems { revision, uris, added, complete });
    }
}

/// A playlist read has more to come: fewer items than its length (`total`, when known), or the
/// last answer said it was cut short (`truncated`).
fn more_to_read(total: Option<usize>, got: usize, truncated: bool) -> bool {
    match total {
        Some(t) => got < t,
        None => truncated,
    }
}

// ---------------------------------------------------------------------------------------------
// Artist names and pictures (batched)

/// Artists asked for in one extended-metadata request (each answer carries the whole artist).
const ARTIST_BATCH: usize = 50;

/// The picture closest to 300 px wide of an artist's portraits.
fn artist_picture(artist: &pb::Artist) -> Option<String> {
    artist
        .portrait_group
        .get_or_default()
        .image
        .iter()
        .chain(artist.portrait.iter())
        .filter(|i| !i.file_id().is_empty())
        .min_by_key(|i| (i.width() - 300).abs())
        .and_then(|i| convert::image_url(i.file_id()))
}

/// Names and pictures of artists, by uri: `ARTIST_V4` through extended metadata, 50 to a request
/// (one request per artist, as `Artist::get` does, gets refused when there are hundreds). Artists
/// still missing after a second pass are left out.
pub async fn artist_details(session: &Session, uris: &[String]) -> HashMap<String, (String, Option<String>)> {
    let mut out: HashMap<String, (String, Option<String>)> = HashMap::with_capacity(uris.len());
    let mut pending: Vec<String> = uris.iter().filter(|u| u.starts_with("spotify:artist:")).cloned().collect();
    for pass in 0..2 {
        if pending.is_empty() {
            break;
        }
        if pass > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        for batch in pending.chunks(ARTIST_BATCH) {
            let request = BatchedEntityRequest {
                entity_request: batch
                    .iter()
                    .map(|uri| EntityRequest {
                        entity_uri: uri.clone(),
                        query: vec![ExtensionQuery { extension_kind: EnumOrUnknown::new(ExtensionKind::ARTIST_V4), ..Default::default() }],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            let response = match retry("artist details", || {
                let request = request.clone();
                async move { session.spclient().get_extended_metadata(request).await }
            })
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("artist details ({} artists): {e}", batch.len());
                    continue;
                }
            };
            for array in &response.extended_metadata {
                for data in &array.extension_data {
                    let status = data.header.as_ref().map(|h| h.status_code).unwrap_or(0);
                    let Some(any) = data.extension_data.as_ref() else { continue };
                    if any.value.is_empty() || !(status == 0 || (200..300).contains(&status)) {
                        continue;
                    }
                    match pb::Artist::parse_from_bytes(&any.value) {
                        Ok(artist) if !artist.name().is_empty() => {
                            out.insert(data.entity_uri.clone(), (artist.name().to_owned(), artist_picture(&artist)));
                        }
                        Ok(_) => {}
                        Err(e) => log::warn!("unreadable artist {}: {e}", data.entity_uri),
                    }
                }
            }
        }
        pending.retain(|u| !out.contains_key(u));
    }
    if !pending.is_empty() {
        log::info!("artist details: Spotify left out {} of {}", pending.len(), uris.len());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Track / episode metadata (batched)

fn metadata_request(uris: &[String]) -> BatchedEntityRequest {
    BatchedEntityRequest {
        entity_request: uris
            .iter()
            .map(|uri| EntityRequest {
                entity_uri: uri.clone(),
                query: vec![ExtensionQuery {
                    extension_kind: EnumOrUnknown::new(if uri.starts_with("spotify:episode:") {
                        ExtensionKind::EPISODE_V4
                    } else {
                        ExtensionKind::TRACK_V4
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

enum Parsed {
    Track(pb::Track),
    Episode(pb::Episode),
}

struct BatchOutcome {
    found: Vec<TrackInfo>,
    missing: Vec<String>,
}

/// One `SpClient::get_extended_metadata` call (`POST /extended-metadata/v0/extended-metadata`)
/// for TRACK_V4 / EPISODE_V4 extensions.
async fn metadata_batch(session: &Session, uris: &[String], market: &Market) -> Result<BatchOutcome, CoreError> {
    let request = metadata_request(uris);
    let response = retry("track details", || {
        let request = request.clone();
        async move { session.spclient().get_extended_metadata(request).await }
    })
    .await?;
    drop(request);

    let wanted: HashSet<&str> = uris.iter().map(String::as_str).collect();
    let mut parsed: Vec<(String, Parsed)> = Vec::with_capacity(uris.len());
    for array in &response.extended_metadata {
        for data in &array.extension_data {
            let status = data.header.as_ref().map(|h| h.status_code).unwrap_or(0);
            let Some(any) = data.extension_data.as_ref() else { continue };
            if any.value.is_empty() || !(status == 0 || (200..300).contains(&status)) {
                continue;
            }
            let uri = data.entity_uri.as_str();
            if !wanted.contains(uri) {
                log::debug!("track details: unexpected entity {uri}");
                continue;
            }
            let item = if uri.starts_with("spotify:episode:") {
                pb::Episode::parse_from_bytes(&any.value).map(Parsed::Episode)
            } else {
                pb::Track::parse_from_bytes(&any.value).map(Parsed::Track)
            };
            match item {
                Ok(p) => parsed.push((uri.to_owned(), p)),
                Err(e) => log::warn!("unreadable metadata for {uri}: {e}"),
            }
        }
    }
    drop(response);

    // If not a single item carries an audio file list, the server format changed: don't let
    // that mark everything unplayable (the player still skips what it really can't play).
    let any_audio = parsed.iter().any(|(_, p)| match p {
        Parsed::Track(t) => !t.file.is_empty() || !t.alternative.is_empty(),
        Parsed::Episode(e) => !e.audio.is_empty(),
    });
    let respect_files = any_audio || parsed.len() < 3;
    if !respect_files {
        log::warn!("track metadata came without audio file lists; not using them for playability");
    }

    let mut found = Vec::with_capacity(parsed.len());
    let mut got: HashSet<String> = HashSet::with_capacity(parsed.len());
    for (uri, p) in parsed {
        let info = match &p {
            Parsed::Track(t) => convert::track_info(&uri, t, market, respect_files),
            Parsed::Episode(e) => convert::episode_info(&uri, e, market, respect_files),
        };
        got.insert(uri);
        found.push(info);
    }
    let missing = uris.iter().filter(|u| !got.contains(u.as_str())).cloned().collect();
    Ok(BatchOutcome { found, missing })
}

async fn run_batch(session: &Session, batch: Vec<String>, market: &Market) -> (Vec<String>, Result<BatchOutcome, CoreError>) {
    let result = metadata_batch(session, &batch, market).await;
    (batch, result)
}

/// Largest batch the service accepted so far; lowered whenever a batch is rejected as too
/// large, and remembered for the rest of the run.
static BATCH_LIMIT: AtomicUsize = AtomicUsize::new(METADATA_BATCH);

/// Metadata for `uris` (unique track/episode URIs) in batches with bounded concurrency.
/// `on_batch` is called with the URIs of every finished batch (for progress). Entities the
/// service doesn't return after a couple of passes become "Unavailable track" entries.
pub async fn hydrate(
    session: &Session,
    uris: Vec<String>,
    market: &Market,
    mut on_batch: impl FnMut(&[String]),
) -> Result<HashMap<String, TrackInfo>, CoreError> {
    let mut out: HashMap<String, TrackInfo> = HashMap::with_capacity(uris.len());
    let mut pending: Vec<String> = uris;

    for pass in 0..3 {
        if pending.is_empty() {
            break;
        }
        let mut limit = BATCH_LIMIT.load(Ordering::Relaxed).max(1);
        if pass > 0 {
            limit = limit.min(RETRY_BATCH);
        }
        let mut queue: VecDeque<Vec<String>> = VecDeque::from([std::mem::take(&mut pending)]);
        let mut missing: Vec<String> = Vec::new();
        let mut in_flight = FuturesUnordered::new();
        loop {
            while in_flight.len() < METADATA_CONCURRENCY {
                let Some(mut batch) = queue.pop_front() else { break };
                if batch.len() > limit {
                    let rest = batch.split_off(limit);
                    queue.push_front(rest);
                }
                in_flight.push(run_batch(session, batch, market));
            }
            let Some((batch, result)) = in_flight.next().await else { break };
            match result {
                Ok(outcome) => {
                    for info in outcome.found {
                        out.insert(info.uri.clone(), info);
                    }
                    // Only report finished items; missing ones get another pass.
                    if outcome.missing.is_empty() {
                        on_batch(&batch);
                    } else {
                        let missing_set: HashSet<&String> = outcome.missing.iter().collect();
                        let done: Vec<String> = batch.into_iter().filter(|u| !missing_set.contains(u)).collect();
                        on_batch(&done);
                    }
                    missing.extend(outcome.missing);
                }
                // The service rejected the request: too large, or one odd entity in it. Use
                // smaller batches until the culprit is isolated, unless nothing has worked at
                // all (then the endpoint itself is failing and splitting would only multiply
                // requests).
                Err(e) if e.kind == ErrorKind::InvalidArgument && (batch.len() > MIN_SPLIT_BATCH || !out.is_empty()) => {
                    if batch.len() == 1 {
                        log::warn!("track details for {} rejected ({e}); marking it unavailable", batch[0]);
                        out.insert(batch[0].clone(), convert::unavailable_info(&batch[0]));
                        on_batch(&batch);
                    } else {
                        limit = limit.min(batch.len() / 2).max(1);
                        if batch.len() > MIN_SPLIT_BATCH {
                            BATCH_LIMIT.fetch_min(limit.max(MIN_SPLIT_BATCH), Ordering::Relaxed);
                        }
                        log::info!("track details request for {} items rejected ({e}); using batches of {limit}", batch.len());
                        queue.push_front(batch);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        if !missing.is_empty() {
            log::info!("track details pass {pass}: {} items missing", missing.len());
        }
        pending = missing;
    }

    if !pending.is_empty() {
        log::warn!("{} items have no metadata; marking them unavailable", pending.len());
        on_batch(&pending);
        for uri in pending {
            out.insert(uri.clone(), convert::unavailable_info(&uri));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// User profile

pub struct ProfileInfo {
    pub name: Option<String>,
    pub image_url: Option<String>,
}

/// `SpClient::get_user_profile` (`/user-profile-view/v3/profile/{username}`, JSON).
pub async fn user_profile(session: &Session, username: &str) -> Result<ProfileInfo, CoreError> {
    let bytes = retry("user profile", || async move {
        match session.spclient().get_user_profile(username, Some(0), Some(0)).await {
            Err(e) if e.kind == ErrorKind::InvalidArgument => session.spclient().get_user_profile(username, None, None).await,
            other => other,
        }
    })
    .await?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| CoreError::failed_precondition(format!("profile is not JSON: {e}")))?;
    let text = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let image_url = text("image_url").or_else(|| {
        value
            .get("images")
            .and_then(|v| v.as_array())
            .and_then(|a| a.iter().find_map(|i| i.get("url").and_then(|u| u.as_str())))
            .map(str::to_owned)
    });
    Ok(ProfileInfo { name: text("name").or_else(|| text("display_name")), image_url })
}

// ---------------------------------------------------------------------------------------------
// Liked Songs

fn liked_track_uri(raw: &str) -> Option<String> {
    let uri = raw.split('?').next().unwrap_or(raw);
    convert::is_catalog_item(uri).then(|| uri.to_owned()).filter(|u| u.starts_with("spotify:track:"))
}

#[derive(Default)]
struct PageData {
    uris: Vec<String>,
    page_url: String,
    next_page_url: String,
}

fn json_str(v: &serde_json::Value, snake: &str, camel: &str) -> String {
    v.get(snake).or_else(|| v.get(camel)).and_then(|s| s.as_str()).unwrap_or("").to_owned()
}

fn page_from_json(v: &serde_json::Value) -> PageData {
    let uris = v
        .get("tracks")
        .and_then(|t| t.as_array())
        .map(|tracks| tracks.iter().filter_map(|t| t.get("uri").and_then(|u| u.as_str())).filter_map(liked_track_uri).collect())
        .unwrap_or_default();
    PageData { uris, page_url: json_str(v, "page_url", "pageUrl"), next_page_url: json_str(v, "next_page_url", "nextPageUrl") }
}

/// A fetched page may be a `ContextPage` or a whole `Context`; merge into one page.
fn pages_from_json(bytes: &[u8]) -> Result<PageData, CoreError> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| CoreError::failed_precondition(format!("page is not JSON: {e}")))?;
    match v.get("pages").and_then(|p| p.as_array()) {
        Some(pages) => {
            let mut merged = PageData::default();
            for p in pages {
                let page = page_from_json(p);
                merged.uris.extend(page.uris);
                if merged.uris.is_empty() && !page.page_url.is_empty() {
                    merged.page_url = page.page_url;
                }
                if !page.next_page_url.is_empty() {
                    merged.next_page_url = page.next_page_url;
                }
            }
            Ok(merged)
        }
        None => Ok(page_from_json(&v)),
    }
}

/// Liked Songs as the collection service lists them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LikedSet {
    /// Song uris newest first, each with when it was liked (Unix seconds, 0 when unknown).
    pub songs: Vec<(String, i64)>,
    /// The collection's sync token as of this list (empty when it was read another way).
    pub sync_token: String,
}

/// A finished Liked Songs read, with the route and requests it took (for the log).
#[derive(Debug)]
pub struct LikedRead {
    pub set: LikedSet,
    pub route: &'static str,
    pub requests: usize,
}

const ROUTE_DELTA: &str = "collection delta";
const ROUTE_PAGING: &str = "collection paging";
const ROUTE_CONTEXT: &str = "context-resolve";

/// Liked Songs through context-resolve (`SpClient::get_context("spotify:user:<u>:collection")`,
/// the URI form documented in librespot's spclient.rs), following `page_url`/`next_page_url`
/// with `SpClient::get_next_page`. Newest first, as Spotify orders them, without dates. The first
/// request has the server resolve the whole collection, which times out (504) for very large
/// ones, so this is only the fallback.
pub async fn liked_uris_context(session: &Session) -> Result<LikedRead, CoreError> {
    let context_uri = format!("spotify:user:{}:collection", session.username());
    let uri = context_uri.as_str();
    let mut requests = 0usize;
    let ctx = retry("Liked Songs (context-resolve)", || {
        requests += 1;
        async move { session.spclient().get_context(uri).await }
    })
    .await?;

    let first_pages: Vec<PageData> = ctx
        .pages
        .iter()
        .map(|p| PageData {
            uris: p.tracks.iter().map(|t| t.uri()).filter_map(liked_track_uri).collect(),
            page_url: p.page_url().to_owned(),
            next_page_url: p.next_page_url().to_owned(),
        })
        .collect();
    drop(ctx);

    let mut out: Vec<String> = Vec::new();
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut fetches = 0usize;
    for mut page in first_pages {
        loop {
            if page.uris.is_empty() && !page.page_url.is_empty() && seen_urls.insert(page.page_url.clone()) {
                fetches += 1;
                if fetches > MAX_PAGES {
                    return Err(CoreError::out_of_range("too many Liked Songs pages"));
                }
                let url = page.page_url.clone();
                let next_after = std::mem::take(&mut page.next_page_url);
                let u = url.as_str();
                page = pages_from_json(
                    &retry("Liked Songs (context-resolve) page", || {
                        requests += 1;
                        async move { session.spclient().get_next_page(u).await }
                    })
                    .await?,
                )?;
                if page.next_page_url.is_empty() {
                    page.next_page_url = next_after;
                }
                page.page_url.clear();
                continue;
            }
            out.append(&mut page.uris);
            if page.next_page_url.is_empty() || !seen_urls.insert(page.next_page_url.clone()) {
                break;
            }
            fetches += 1;
            if fetches > MAX_PAGES {
                return Err(CoreError::out_of_range("too many Liked Songs pages"));
            }
            let url = std::mem::take(&mut page.next_page_url);
            let u = url.as_str();
            page = pages_from_json(
                &retry("Liked Songs (context-resolve) page", || {
                    requests += 1;
                    async move { session.spclient().get_next_page(u).await }
                })
                .await?,
            )?;
        }
    }
    let mut unique = HashSet::with_capacity(out.len());
    out.retain(|u| unique.insert(u.clone()));
    let songs = out.into_iter().map(|u| (u, 0)).collect();
    Ok(LikedRead { set: LikedSet { songs, sync_token: String::new() }, route: ROUTE_CONTEXT, requests })
}

/// POST a collection2v2 protobuf body once (spclient still repeats network errors). Tries the
/// collection media type first and plain `application/x-protobuf` if the server rejects it.
async fn collection_post_once(session: &Session, endpoint: &str, body: &[u8]) -> Result<Bytes, CoreError> {
    let mut last = None;
    for mime in [COLLECTION_MIME, PROTOBUF_MIME] {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
        headers.insert(ACCEPT, HeaderValue::from_static(mime));
        match session.spclient().request(&Method::POST, endpoint, Some(headers), Some(body)).await {
            Err(e) if e.kind == ErrorKind::InvalidArgument => last = Some(e),
            other => return other,
        }
    }
    Err(last.unwrap_or_else(|| CoreError::invalid_argument("collection request rejected")))
}

/// [`collection_post_once`] with retries.
async fn collection_post(session: &Session, what: &str, endpoint: &str, body: &[u8]) -> Result<Bytes, CoreError> {
    retry(what, || collection_post_once(session, endpoint, body)).await
}

/// What paging through a collection set gave.
#[derive(Default)]
struct SetRead {
    /// Items still in the set, newest first.
    items: Vec<wire::CollectionItem>,
    sync_token: String,
    /// The set's sync token was still the one known: nothing past the first page was read.
    unchanged: bool,
    requests: usize,
}

/// Pages through a collection set with `post` (one `/collection/v2/paging` request: pagination
/// token and page size in, reply out). A page that fails is asked for again on its own (at half
/// the size if it timed out) instead of starting over. With `known_token`, stops after the first
/// page when the set's sync token is still that one.
async fn page_through<F, Fut>(what: &str, known_token: &str, mut post: F) -> Result<SetRead, CoreError>
where
    F: FnMut(String, i32) -> Fut,
    Fut: Future<Output = Result<Bytes, CoreError>>,
{
    let mut read = SetRead::default();
    let mut token = String::new();
    let mut limit = COLLECTION_PAGE;
    for page_no in 1..=MAX_SET_PAGES {
        let mut tries = 0;
        let bytes = loop {
            tries += 1;
            read.requests += 1;
            match post(token.clone(), limit).await {
                Ok(bytes) => break bytes,
                Err(e) if tries < PAGE_TRIES && is_transient(e.kind) => {
                    if e.kind == ErrorKind::DeadlineExceeded {
                        limit = (limit / 2).max(MIN_COLLECTION_PAGE);
                    }
                    let delay = retry_delay(e.kind, tries);
                    log::warn!("{what} page {page_no} failed ({e}); retrying in {} ms", delay.as_millis());
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        };
        let page = wire::decode_page_response(&bytes).map_err(|e| CoreError::failed_precondition(format!("{what} page: {e}")))?;
        if read.sync_token.is_empty() {
            read.sync_token = page.sync_token;
        }
        if page_no == 1 && !known_token.is_empty() && read.sync_token == known_token {
            read.unchanged = true;
            return Ok(read);
        }
        let returned = page.items.len();
        read.items.extend(page.items.into_iter().filter(|i| !i.is_removed));
        if page.next_page_token.is_empty() || page.next_page_token == token || returned == 0 {
            // Stable sort keeps the server order for equal timestamps.
            read.items.sort_by_key(|i| std::cmp::Reverse(i.added_at));
            return Ok(read);
        }
        token = page.next_page_token;
    }
    Err(CoreError::out_of_range(format!("too many {what} pages")))
}

/// Every item still in one of the collection service's sets (`POST /collection/v2/paging`), newest
/// first.
async fn collection_set(session: &Session, set: &str, what: &str) -> Result<Vec<wire::CollectionItem>, CoreError> {
    let username = session.username();
    let username = username.as_str();
    let read = page_through(what, "", |token, limit| {
        let body = wire::encode_page_request(username, set, &token, limit);
        async move { collection_post_once(session, "/collection/v2/paging", &body).await }
    })
    .await?;
    Ok(read.items)
}

/// Track uris of collection items (newest first), each once, with when they were liked.
fn songs_of(items: Vec<wire::CollectionItem>) -> Vec<(String, i64)> {
    let mut unique = HashSet::with_capacity(items.len());
    items
        .into_iter()
        .filter_map(|i| liked_track_uri(&i.uri).map(|u| (u, i64::from(i.added_at))))
        .filter(|(u, _)| unique.insert(u.clone()))
        .collect()
}

/// `known` with the changes the collection service listed since its sync token: songs taken out
/// leave, songs added (or liked again) go in by date. The last change to a song counts.
fn with_changes(known: &[(String, i64)], changes: &[wire::CollectionItem]) -> Vec<(String, i64)> {
    let changed: Vec<(String, &wire::CollectionItem)> = changes.iter().filter_map(|c| Some((liked_track_uri(&c.uri)?, c))).collect();
    let last: HashMap<&str, usize> = changed.iter().enumerate().map(|(i, (u, _))| (u.as_str(), i)).collect();
    let mut songs: Vec<(String, i64)> = changed
        .iter()
        .enumerate()
        .filter(|(i, (u, c))| !c.is_removed && last[u.as_str()] == *i)
        .map(|(_, (u, c))| (u.clone(), i64::from(c.added_at)))
        .collect();
    songs.extend(known.iter().filter(|(u, _)| !last.contains_key(u.as_str())).cloned());
    // Stable: songs liked in the same second keep their order.
    songs.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    songs
}

/// Liked Songs brought up to date from `known` (the list as last read): only the changes since
/// its sync token when the collection service can list them (`delta`, one request), else every
/// page (`page`), stopping after the first when the sync token shows nothing changed.
async fn sync_liked_with<D, DFut, P, PFut>(known: Option<&LikedSet>, mut delta: D, page: P) -> Result<LikedRead, CoreError>
where
    D: FnMut(String) -> DFut,
    DFut: Future<Output = Result<Bytes, CoreError>>,
    P: FnMut(String, i32) -> PFut,
    PFut: Future<Output = Result<Bytes, CoreError>>,
{
    let known = known.filter(|k| !k.sync_token.is_empty());
    let mut requests = 0usize;
    if let Some(known) = known {
        let reply = retry("Liked Songs changes", || {
            requests += 1;
            delta(known.sync_token.clone())
        })
        .await
        .and_then(|bytes| wire::decode_delta_response(&bytes).map_err(|e| CoreError::failed_precondition(format!("Liked Songs changes: {e}"))));
        match reply {
            Ok(d) if d.delta_update_possible => {
                let songs = with_changes(&known.songs, &d.items);
                let sync_token = if d.sync_token.is_empty() { known.sync_token.clone() } else { d.sync_token };
                return Ok(LikedRead { set: LikedSet { songs, sync_token }, route: ROUTE_DELTA, requests });
            }
            Ok(_) => log::info!("Liked Songs: Spotify can't list the changes since the last read; reading them all"),
            Err(e) if is_transient(e.kind) => return Err(e),
            Err(e) => log::info!("Liked Songs changes unavailable ({e}); reading them all"),
        }
    }
    let read = page_through("Liked Songs", known.map_or("", |k| k.sync_token.as_str()), page).await?;
    requests += read.requests;
    let set = match known {
        Some(known) if read.unchanged => known.clone(),
        _ => LikedSet { songs: songs_of(read.items), sync_token: read.sync_token },
    };
    Ok(LikedRead { set, route: ROUTE_PAGING, requests })
}

/// Liked Songs through the collection service, brought up to date from `known` (see
/// [`sync_liked_with`]): `POST /collection/v2/delta`, else `POST /collection/v2/paging`.
pub async fn liked_sync(session: &Session, known: Option<&LikedSet>) -> Result<LikedRead, CoreError> {
    let username = session.username();
    let username = username.as_str();
    sync_liked_with(
        known,
        |token| {
            let body = wire::encode_delta_request(username, COLLECTION_SET, &token);
            async move { collection_post_once(session, "/collection/v2/delta", &body).await }
        },
        |token, limit| {
            let body = wire::encode_page_request(username, COLLECTION_SET, &token, limit);
            async move { collection_post_once(session, "/collection/v2/paging", &body).await }
        },
    )
    .await
}

/// The artists the user follows (the collection service's set "artist"), followed last first.
pub async fn followed_artist_uris(session: &Session) -> Result<Vec<String>, CoreError> {
    let items = collection_set(session, ARTIST_SET, "Followed artists").await?;
    let mut unique = HashSet::with_capacity(items.len());
    Ok(items
        .into_iter()
        .map(|i| i.uri)
        .filter(|u| u.starts_with("spotify:artist:") && unique.insert(u.clone()))
        .collect())
}

/// Adds one item to (or takes it out of) a collection set (`POST /collection/v2/write`).
async fn set_write(session: &Session, set: &str, what: &str, uri: &str, add: bool) -> Result<(), CoreError> {
    let username = session.username();
    let item = wire::CollectionItem {
        uri: uri.to_owned(),
        added_at: super::cache::now_secs().min(i32::MAX as u64) as i32,
        is_removed: !add,
    };
    let client_update_id = format!("{:016x}", rand::random::<u64>());
    let body = wire::encode_write_request(&username, set, std::slice::from_ref(&item), &client_update_id);
    collection_post(session, what, "/collection/v2/write", &body).await?;
    Ok(())
}

/// Adds (or takes out) many items in one collection write, sent once: the caller decides when to
/// try again.
async fn set_write_many(session: &Session, set: &str, uris: &[String], add: bool) -> Result<(), CoreError> {
    let username = session.username();
    let now = super::cache::now_secs().min(i32::MAX as u64) as i32;
    let items: Vec<wire::CollectionItem> = uris.iter().map(|u| wire::CollectionItem { uri: u.clone(), added_at: now, is_removed: !add }).collect();
    let client_update_id = format!("{:016x}", rand::random::<u64>());
    let body = wire::encode_write_request(&username, set, &items, &client_update_id);
    collection_post_once(session, "/collection/v2/write", &body).await?;
    Ok(())
}

/// Adds (or takes out) many songs in Liked Songs in one write, sent once (the like queue tries
/// again).
pub async fn collection_write_many(session: &Session, uris: &[String], liked: bool) -> Result<(), CoreError> {
    set_write_many(session, COLLECTION_SET, uris, liked).await
}

/// Follow or unfollow one artist.
pub async fn follow_write(session: &Session, uri: &str, follow: bool) -> Result<(), CoreError> {
    set_write(session, ARTIST_SET, "Follow update", uri, follow).await
}

// ---------------------------------------------------------------------------------------------
// Playlist changes, sent once (as Spotify's own apps do, through the session)

/// How long a change may take before Truffle stops waiting (it may still land).
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How a change went.
pub enum Sent {
    /// Spotify took it (the reply: a `SelectedListContent` with the new revision, or JSON).
    Applied(Bytes),
    /// The list changed since its revision was read: nothing was applied.
    Conflict,
    /// Spotify refused it (or it was never sent): nothing was applied.
    Refused(CoreError),
    /// No clear answer: it may or may not have been applied.
    Unsure(CoreError),
}

/// The HTTP status of a failed request, if it got that far.
pub fn status_of(e: &CoreError) -> Option<u16> {
    match e.error.downcast_ref::<librespot_core::http_client::HttpClientError>() {
        Some(librespot_core::http_client::HttpClientError::StatusCode(code)) => Some(code.as_u16()),
        _ => None,
    }
}

/// What a request's result means for a change that must not be sent twice.
pub fn classify(result: Result<Bytes, CoreError>) -> Sent {
    match result {
        Ok(bytes) => Sent::Applied(bytes),
        Err(e) => match status_of(&e) {
            Some(409) => Sent::Conflict,
            Some(s) if (400..500).contains(&s) && s != 408 => Sent::Refused(e),
            // librespot's own rate limit refuses before anything is sent.
            None if e.kind == ErrorKind::ResourceExhausted => Sent::Refused(e),
            _ => Sent::Unsure(e),
        },
    }
}

/// A request with the session's login and client token.
async fn authed(
    session: &Session,
    url: &str,
    content_type: &str,
    accept: Option<&str>,
    extra: &[(&'static str, String)],
    body: Bytes,
) -> Result<http::Request<Bytes>, CoreError> {
    let token = session.login5().auth_token().await?;
    let mut b = http::Request::builder()
        .method(http::Method::POST)
        .uri(url)
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::AUTHORIZATION, format!("{} {}", token.token_type, token.access_token));
    if let Some(a) = accept {
        b = b.header(http::header::ACCEPT, a);
    }
    if let Ok(ct) = session.spclient().client_token().await {
        b = b.header("client-token", ct);
    }
    for (name, value) in extra {
        b = b.header(*name, value.as_str());
    }
    b.body(body).map_err(CoreError::invalid_argument)
}

/// One request, never repeated (librespot's HTTP client only waits out a 429, which wasn't
/// applied).
async fn send_once(session: &Session, url: &str, content_type: &str, accept: Option<&str>, body: Bytes) -> Sent {
    let req = match authed(session, url, content_type, accept, &[], body).await {
        Ok(r) => r,
        Err(e) => return Sent::Refused(e),
    };
    match tokio::time::timeout(WRITE_TIMEOUT, session.http_client().request_body(req)).await {
        Err(_) => Sent::Unsure(CoreError::deadline_exceeded("no answer from Spotify")),
        Ok(result) => classify(result),
    }
}

/// The content types a list change is sent as: the desktop app's first, then plain protobuf
/// (the one that worked is remembered).
const CHANGES_TYPES: [&str; 2] = ["application/x-www-form-urlencoded", "application/x-protobuf"];
static CHANGES_TYPE: AtomicUsize = AtomicUsize::new(0);

/// Sends `changes` to `endpoint` (`/playlist/v2/playlist/{id}/changes` or the rootlist's).
pub async fn post_list_changes(session: &Session, endpoint: &str, changes: &ListChanges) -> Sent {
    let base = match session.spclient().base_url().await {
        Ok(b) => b,
        Err(e) => return Sent::Refused(e),
    };
    let body = match changes.write_to_bytes() {
        Ok(b) => Bytes::from(b),
        Err(e) => return Sent::Refused(CoreError::invalid_argument(e)),
    };
    let url = format!("{base}{endpoint}");
    let first = CHANGES_TYPE.load(Ordering::Relaxed).min(1);
    let sent = send_once(session, &url, CHANGES_TYPES[first], Some("application/x-protobuf"), body.clone()).await;
    // Refused for its type (not applied): the other type once.
    if let Sent::Refused(e) = &sent
        && matches!(status_of(e), Some(400 | 415))
    {
        let other = 1 - first;
        let again = send_once(session, &url, CHANGES_TYPES[other], Some("application/x-protobuf"), body).await;
        if matches!(again, Sent::Applied(_)) {
            log::info!("playlist changes go as {}", CHANGES_TYPES[other]);
            CHANGES_TYPE.store(other, Ordering::Relaxed);
        }
        return again;
    }
    sent
}

/// A new playlist the web player's way: Spotify picks its id and says it in the reply
/// (`{"uri": ..., "revision": ...}`).
pub async fn create_playlist_web(session: &Session, name: &str) -> Sent {
    let base = match session.spclient().base_url().await {
        Ok(b) => b,
        Err(e) => return Sent::Refused(e),
    };
    let body = serde_json::json!({
        "ops": [{ "kind": "UPDATE_LIST_ATTRIBUTES", "updateListAttributes": { "newAttributes": { "values": { "name": name } } } }]
    })
    .to_string();
    send_once(session, &format!("{base}/playlist/v2/playlist"), "application/json", Some("application/json"), Bytes::from(body)).await
}

pub fn playlist_changes_endpoint(id: &SpotifyId) -> Result<String, CoreError> {
    Ok(format!("/playlist/v2/playlist/{}/changes", id.to_base62()?))
}

pub fn rootlist_changes_endpoint(session: &Session) -> String {
    format!("/playlist/v2/user/{}/rootlist/changes", session.username())
}

/// The new revision in a change's reply (best effort: an applied change counts even without it).
pub fn revision_of(reply: &[u8]) -> Option<Vec<u8>> {
    SelectedListContent::parse_from_bytes(reply).ok().map(|m| m.revision().to_vec()).filter(|r| !r.is_empty())
}

/// The library list exactly as stored (folder markers and all, since a removal names a row by
/// its place) and its revision. Starts over once if it changes while being read.
pub async fn rootlist_raw(session: &Session) -> Result<(Vec<u8>, Vec<String>), CoreError> {
    'restart: for _attempt in 0..2 {
        let mut revision = Vec::new();
        let mut uris = Vec::new();
        let mut from = 0usize;
        for page_no in 0..MAX_PAGES {
            let bytes = match retry("library list", || async move { session.spclient().get_rootlist(from, None).await }).await {
                Ok(b) => b,
                Err(e) if page_no == 0 && e.kind == ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
                Err(e) => return Err(e),
            };
            let msg = parse_list(&bytes)?;
            if page_no == 0 {
                revision = msg.revision().to_vec();
            } else if !msg.revision().is_empty() && msg.revision() != revision.as_slice() {
                continue 'restart;
            }
            let contents = msg.contents.get_or_default();
            let returned = contents.items.len();
            uris.extend(contents.items.iter().map(|i| i.uri().to_owned()));
            from += returned;
            let more = if msg.has_length() { from < msg.length().max(0) as usize } else { contents.truncated() };
            if !more || returned == 0 {
                break;
            }
        }
        return Ok((revision, uris));
    }
    Err(CoreError::aborted("the library list kept changing while it was read"))
}

// ---------------------------------------------------------------------------------------------
// Playlist pictures

/// Uploads a JPEG for a playlist's picture and registers it; returns the picture's id (what the
/// list's `picture` attribute holds).
pub async fn upload_playlist_picture(session: &Session, id: &SpotifyId, jpeg: Vec<u8>) -> Result<Vec<u8>, CoreError> {
    // 1. The picture itself.
    let req = authed(session, "https://image-upload.spotify.com/v4/playlist", "image/jpeg", Some("application/json"), &[], Bytes::from(jpeg))
        .await?;
    let reply = tokio::time::timeout(WRITE_TIMEOUT, session.http_client().request_body(req))
        .await
        .map_err(|_| CoreError::deadline_exceeded("no answer from Spotify's picture upload"))??;
    let v: serde_json::Value = serde_json::from_slice(&reply).map_err(CoreError::failed_precondition)?;
    let upload_token = v["uploadToken"].as_str().ok_or_else(|| CoreError::failed_precondition("no upload token"))?.to_owned();
    // 2. Registered for this playlist: the reply names the picture.
    let base = session.spclient().base_url().await?;
    let url = format!("{base}/playlist/v2/playlist/{}/register-image", id.to_base62()?);
    let body = serde_json::json!({ "uploadToken": upload_token }).to_string();
    let req = authed(session, &url, "application/json", Some("application/json"), &[], Bytes::from(body)).await?;
    let reply = tokio::time::timeout(WRITE_TIMEOUT, session.http_client().request_body(req))
        .await
        .map_err(|_| CoreError::deadline_exceeded("no answer from Spotify"))??;
    let v: serde_json::Value = serde_json::from_slice(&reply).map_err(CoreError::failed_precondition)?;
    let picture = v["picture"].as_str().ok_or_else(|| CoreError::failed_precondition("no picture id"))?;
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(picture).map_err(CoreError::failed_precondition)
}

// ---------------------------------------------------------------------------------------------
// Search (Pathfinder, the web player's GraphQL service)

const PATHFINDER_URL: &str = "https://api-partner.spotify.com/pathfinder/v2/query";

/// One persisted Pathfinder query; `Ok(None)` when Spotify no longer knows this hash.
pub async fn pathfinder(session: &Session, operation: &str, hash: &str, variables: serde_json::Value) -> Result<Option<serde_json::Value>, CoreError> {
    let body = serde_json::json!({
        "variables": variables,
        "operationName": operation,
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
    })
    .to_string();
    let extra = [
        ("app-platform", "Win32_x86_64".to_owned()),
        ("spotify-app-version", librespot_core::version::spotify_semantic_version().to_string()),
    ];
    let result = retry(operation, || {
        let body = body.clone();
        let extra = extra.clone();
        async move {
            let req = authed(session, PATHFINDER_URL, "application/json;charset=UTF-8", Some("application/json"), &extra, Bytes::from(body)).await?;
            session.http_client().request_body(req).await
        }
    })
    .await;
    let bytes = match result {
        Ok(b) => b,
        // A retired hash (412, or a plain 400 / 404 without a body to say so).
        Err(e) if matches!(status_of(&e), Some(400 | 404 | 412)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(CoreError::failed_precondition)?;
    let retired = v["errors"].as_array().is_some_and(|errs| {
        errs.iter().any(|e| e["extensions"]["code"].as_str() == Some("PERSISTED_QUERY_NOT_FOUND") || e["message"].as_str() == Some("PersistedQueryNotFound"))
    });
    Ok((!retired).then_some(v))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn a_read_that_stopped_short_is_not_all_of_the_playlist() {
        // 2,000 of 3,000 came, then a page was empty or ignored the paging.
        assert!(more_to_read(Some(3000), 2000, false));
        // No length given: the last answer said it was cut short.
        assert!(more_to_read(None, 100, true));
        assert!(!more_to_read(Some(3000), 3000, true));
        assert!(!more_to_read(None, 100, false));
    }

    fn uri(id: &str) -> String {
        format!("spotify:track:{id:0>22}")
    }

    fn item(id: &str, added_at: i32) -> wire::CollectionItem {
        wire::CollectionItem { uri: uri(id), added_at, is_removed: false }
    }

    fn removed(id: &str) -> wire::CollectionItem {
        wire::CollectionItem { uri: uri(id), added_at: 0, is_removed: true }
    }

    /// `n` songs, newest first, liked a second apart.
    fn songs(n: usize) -> Vec<wire::CollectionItem> {
        (0..n).map(|i| item(&i.to_string(), 1_700_000_000 - i as i32)).collect()
    }

    fn known(n: usize, sync_token: &str) -> LikedSet {
        LikedSet { songs: songs_of(songs(n)), sync_token: sync_token.into() }
    }

    /// A collection service holding `items` (newest first): pages by offset tokens, answers delta
    /// requests with `delta`, and fails a page with the errors queued for its token first.
    struct Fake {
        items: Vec<wire::CollectionItem>,
        sync_token: String,
        delta: Result<wire::DeltaResponse, ErrorKind>,
        fail: Mutex<HashMap<String, VecDeque<ErrorKind>>>,
        /// Page requests: (pagination token, page size).
        pages: Mutex<Vec<(String, i32)>>,
        /// Delta requests: the sync token each asked from.
        deltas: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(items: Vec<wire::CollectionItem>) -> Self {
            Self {
                items,
                sync_token: "s1".into(),
                delta: Err(ErrorKind::NotFound),
                fail: Mutex::default(),
                pages: Mutex::default(),
                deltas: Mutex::default(),
            }
        }

        fn fail(&self, token: &str, kinds: &[ErrorKind]) {
            self.fail.lock().unwrap().insert(token.into(), kinds.iter().copied().collect());
        }

        fn page(&self, token: String, limit: i32) -> Result<Bytes, CoreError> {
            self.pages.lock().unwrap().push((token.clone(), limit));
            if let Some(kind) = self.fail.lock().unwrap().get_mut(&token).and_then(VecDeque::pop_front) {
                return Err(CoreError::new(kind, "test"));
            }
            let from = token.parse::<usize>().unwrap_or(0).min(self.items.len());
            let to = (from + limit as usize).min(self.items.len());
            let page = wire::PageResponse {
                items: self.items[from..to].to_vec(),
                next_page_token: if to < self.items.len() { to.to_string() } else { String::new() },
                sync_token: self.sync_token.clone(),
            };
            Ok(Bytes::from(wire::encode_page_response(&page)))
        }

        fn delta(&self, token: String) -> Result<Bytes, CoreError> {
            self.deltas.lock().unwrap().push(token);
            match &self.delta {
                Ok(d) => Ok(Bytes::from(wire::encode_delta_response(d))),
                Err(kind) => Err(CoreError::new(*kind, "test")),
            }
        }

        async fn sync(&self, known: Option<&LikedSet>) -> Result<LikedRead, CoreError> {
            sync_liked_with(
                known,
                |token| {
                    let reply = self.delta(token);
                    async move { reply }
                },
                |token, limit| {
                    let reply = self.page(token, limit);
                    async move { reply }
                },
            )
            .await
        }
    }

    #[tokio::test]
    async fn a_big_library_is_read_page_by_page() {
        let fake = Fake::new(songs(36_864));
        let read = fake.sync(None).await.unwrap();
        assert_eq!((read.route, read.requests), (ROUTE_PAGING, 123));
        assert_eq!(read.set.songs.len(), 36_864);
        assert_eq!(read.set.songs[0], (uri("0"), 1_700_000_000));
        assert_eq!(read.set.songs[36_863].0, uri("36863"));
        assert_eq!(read.set.sync_token, "s1", "kept for asking only the changes next time");
        assert!(fake.pages.lock().unwrap().iter().all(|&(_, limit)| limit == COLLECTION_PAGE), "no request asks for all of it");
        assert!(fake.deltas.lock().unwrap().is_empty(), "nothing to ask the changes since");

        // A list read another way has no sync token: every page again.
        let fake = Fake::new(songs(700));
        let read = fake.sync(Some(&LikedSet { songs: songs_of(songs(700)), sync_token: String::new() })).await.unwrap();
        assert_eq!((read.route, read.requests, read.set.songs.len()), (ROUTE_PAGING, 3, 700));
        assert!(fake.deltas.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sizes_around_a_page() {
        for n in [0, 1, 299, 300, 301, 600, 601] {
            let read = Fake::new(songs(n)).sync(None).await.unwrap();
            assert_eq!(read.set.songs.len(), n, "{n} songs");
            assert_eq!(read.requests, n.div_ceil(300).max(1), "{n} songs");
        }
    }

    #[tokio::test]
    async fn a_page_that_fails_twice_is_asked_for_again_on_its_own() {
        let fake = Fake::new(songs(36_864));
        fake.fail("600", &[ErrorKind::DeadlineExceeded, ErrorKind::DeadlineExceeded]);
        let read = fake.sync(None).await.unwrap();
        assert_eq!(read.set.songs, songs_of(songs(36_864)), "all of them, in order, each once");
        let pages = fake.pages.lock().unwrap().clone();
        let first: Vec<(&str, i32)> = pages[..5].iter().map(|(t, l)| (t.as_str(), *l)).collect();
        // Nothing started over; the page that timed out was asked for again, smaller each time.
        assert_eq!(first, [("", 300), ("300", 300), ("600", 300), ("600", 150), ("600", 75)]);
        assert!(pages[5..].iter().all(|&(_, limit)| limit == 75));
        assert_eq!(read.requests, pages.len());

        // Network trouble (not a time-out) keeps the size.
        let fake = Fake::new(songs(1_000));
        fake.fail("300", &[ErrorKind::Unknown, ErrorKind::Unavailable]);
        assert_eq!(fake.sync(None).await.unwrap().set.songs.len(), 1_000);
        let pages = fake.pages.lock().unwrap();
        assert_eq!(pages.iter().filter(|(t, l)| t == "300" && *l == 300).count(), 3);
        assert_eq!(pages.len(), 4 + 2);
    }

    #[tokio::test]
    async fn a_page_that_keeps_failing_ends_the_read() {
        let fake = Fake::new(songs(1_000));
        fake.fail("300", &[ErrorKind::DeadlineExceeded; 9]);
        assert_eq!(fake.sync(None).await.unwrap_err().kind, ErrorKind::DeadlineExceeded);
        let limits: Vec<i32> = fake.pages.lock().unwrap().iter().map(|&(_, l)| l).collect();
        assert_eq!(limits, [300, 300, 150, 75, MIN_COLLECTION_PAGE], "never smaller than the floor");

        // Refused (not network trouble): not asked again.
        let fake = Fake::new(songs(10));
        fake.fail("", &[ErrorKind::PermissionDenied]);
        assert_eq!(fake.sync(None).await.unwrap_err().kind, ErrorKind::PermissionDenied);
        assert_eq!(fake.pages.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_delta_brings_in_three_likes_and_one_unlike() {
        let known = known(36_864, "s1");
        let mut fake = Fake::new(Vec::new());
        fake.delta = Ok(wire::DeltaResponse {
            delta_update_possible: true,
            items: vec![item("new1", 1_700_000_003), item("new2", 1_700_000_002), removed("17"), item("new3", 1_700_000_001)],
            sync_token: "s2".into(),
        });
        let read = fake.sync(Some(&known)).await.unwrap();
        assert_eq!((read.route, read.requests), (ROUTE_DELTA, 1));
        assert_eq!(read.set.sync_token, "s2");
        assert_eq!(read.set.songs.len(), 36_866);
        let newest: Vec<&str> = read.set.songs[..4].iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(newest, [uri("new1"), uri("new2"), uri("new3"), uri("0")]);
        assert!(read.set.songs.iter().all(|(u, _)| *u != uri("17")));
        assert_eq!(read.set.songs[4..], known.songs[1..36_864].iter().filter(|(u, _)| *u != uri("17")).cloned().collect::<Vec<_>>()[..]);
        assert_eq!(*fake.deltas.lock().unwrap(), ["s1"]);
        assert!(fake.pages.lock().unwrap().is_empty(), "no page read");
    }

    #[tokio::test]
    async fn nothing_changed_costs_one_request() {
        let known = known(36_864, "s1");
        let mut fake = Fake::new(songs(36_864));
        fake.delta = Ok(wire::DeltaResponse { delta_update_possible: true, items: Vec::new(), sync_token: String::new() });
        let read = fake.sync(Some(&known)).await.unwrap();
        assert_eq!((read.route, read.requests), (ROUTE_DELTA, 1));
        assert_eq!(read.set, known, "same songs, same token");
        assert!(fake.pages.lock().unwrap().is_empty());

        // An empty library stays empty.
        let empty = LikedSet { songs: Vec::new(), sync_token: "s1".into() };
        assert_eq!(fake.sync(Some(&empty)).await.unwrap().set, empty);
    }

    #[tokio::test]
    async fn without_a_delta_every_page_is_read_unless_the_token_is_the_same() {
        let known = known(1_000, "s1");
        // The token is too old for a delta: every page.
        let mut fake = Fake::new(songs(1_001));
        fake.sync_token = "s2".into();
        fake.delta = Ok(wire::DeltaResponse::default());
        let read = fake.sync(Some(&known)).await.unwrap();
        assert_eq!((read.route, read.requests), (ROUTE_PAGING, 1 + 4));
        assert_eq!((read.set.songs.len(), read.set.sync_token.as_str()), (1_001, "s2"));

        // No delta route: the first page's token shows nothing changed.
        let fake = Fake::new(songs(1_000));
        let read = fake.sync(Some(&known)).await.unwrap();
        assert_eq!((read.route, read.requests), (ROUTE_PAGING, 2));
        assert_eq!(read.set, known);
        assert_eq!(fake.pages.lock().unwrap().len(), 1);

        // Network trouble asking for the changes: tried again, then given up (another route
        // takes over) rather than reading every page.
        let mut fake = Fake::new(songs(1_000));
        fake.delta = Err(ErrorKind::Unavailable);
        assert_eq!(fake.sync(Some(&known)).await.unwrap_err().kind, ErrorKind::Unavailable);
        assert_eq!(fake.deltas.lock().unwrap().len(), 2);
        assert!(fake.pages.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn odd_items_are_left_out_and_nothing_is_listed_twice() {
        let mut items = songs(3);
        items.insert(1, wire::CollectionItem { uri: "spotify:episode:0000000000000000000ep1".into(), added_at: 1_700_000_000, is_removed: false });
        items.push(wire::CollectionItem { uri: "spotify:local:Artist:Album:Name 🎵 with a very long title:180".into(), added_at: 5, is_removed: false });
        items.push(item("0", 1_600_000_000));
        items.push(removed("gone"));
        items.push(wire::CollectionItem { uri: format!("{}?si=x", uri("q")), added_at: 1, is_removed: false });
        items.push(wire::CollectionItem { uri: "spotify:track:short".into(), added_at: 1, is_removed: false });
        let read = Fake::new(items).sync(None).await.unwrap();
        let uris: Vec<&str> = read.set.songs.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(uris, [uri("0"), uri("1"), uri("2"), uri("q")]);
    }

    #[tokio::test]
    async fn a_server_that_keeps_naming_the_same_page_is_not_followed_forever() {
        let reply = wire::encode_page_response(&wire::PageResponse { items: songs(2), next_page_token: "again".into(), sync_token: String::new() });
        let mut calls = 0;
        let read = page_through("Test", "", |_, _| {
            calls += 1;
            let reply = Ok(Bytes::from(reply.clone()));
            async move { reply }
        })
        .await
        .unwrap();
        assert_eq!((calls, read.requests), (2, 2));
    }

    #[test]
    fn the_last_change_to_a_song_counts() {
        let known = songs_of(songs(5));
        let changes = [
            item("x", 1_800_000_000),
            removed("x"),
            // Liked again: goes to the top.
            item("3", 1_800_000_001),
            removed("1"),
            item("1", 1_800_000_002),
            removed("never liked"),
            wire::CollectionItem { uri: "spotify:episode:0000000000000000000ep1".into(), added_at: 1_900_000_000, is_removed: false },
        ];
        let got: Vec<String> = with_changes(&known, &changes).into_iter().map(|(u, _)| u).collect();
        assert_eq!(got, [uri("1"), uri("3"), uri("0"), uri("2"), uri("4")]);
        assert_eq!(with_changes(&known, &[]), known);
        assert!(with_changes(&[], &[removed("0")]).is_empty());

        // A whole playlist liked at once (one second): all in, in the order listed, before the rest.
        let batch: Vec<wire::CollectionItem> = (0..10_000).map(|i| item(&format!("b{i}"), 1_800_000_000)).collect();
        let got = with_changes(&known, &batch);
        assert_eq!(got.len(), 10_005);
        assert_eq!((got[0].0.as_str(), got[9_999].0.as_str(), got[10_000].0.as_str()), (uri("b0").as_str(), uri("b9999").as_str(), uri("0").as_str()));
    }
}
