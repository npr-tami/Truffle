//! Changing playlists and searching through the session, the way Spotify's own apps do: no Web
//! API key needed. A change is sent once; a conflict (the list changed meanwhile) is read again and
//! sent once more; an unclear answer is checked by reading back, never resent blindly (an add
//! could land twice). Songs the user adds are first looked for in the playlist as Spotify has it
//! now (by link, or another version of the same song), so ones already there are asked about
//! instead of going in twice. The Web API (when connected) is only a fallback for a change Spotify
//! refused outright.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use librespot_core::{Error as CoreError, Session, SpotifyId};
use librespot_protocol::playlist4_external::ListChanges;
use serde_json::Value;

use super::fetch::{self, Sent};
use super::{Library, build_tracks, convert, edits, friendly, lock, normalized, pathfinder, require_username};
use crate::model::{AlreadyIn, SearchResults, TrackInfo};
use crate::text;

/// How long to wait before checking whether an unclear change landed.
const CHECK_AFTER: Duration = Duration::from_secs(2);
/// Rows taken out by place could no longer be told apart from the others.
const LIST_CHANGED: &str = "The playlist changed. Try again.";

/// Refusals the Web API may still get through (not a login, rate or server problem).
fn web_api_may_help(e: &CoreError) -> bool {
    matches!(fetch::status_of(e), Some(400 | 403 | 404 | 405 | 415 | 501))
}

fn playlist_spotify_id(uri: &str) -> Result<SpotifyId> {
    convert::playlist_id(uri).ok_or_else(|| anyhow!("That isn't a Spotify playlist Truffle can edit."))
}

fn base62(id: &SpotifyId) -> Result<String> {
    id.to_base62().map_err(|e| anyhow!("{e}"))
}

/// The songs that can go into a Spotify playlist (no local files); an error when that is none.
fn spotify_songs(uris: &[String]) -> Result<Vec<String>> {
    let songs: Vec<String> = uris.iter().filter(|u| u.starts_with("spotify:") && !u.starts_with("spotify:local:")).cloned().collect();
    if songs.is_empty() {
        bail!("Local files can't be added to Spotify playlists.");
    }
    Ok(songs)
}

/// What `add_new_to_playlist` did.
#[derive(Debug)]
pub enum Added {
    /// They all went in (how many): none was in the playlist, or only ones from `again`.
    Songs(usize),
    /// Some were in it already: nothing went in.
    Already(AlreadyIn),
    /// The playlist couldn't be read, or not all of it (why): nothing went in.
    Unchecked(anyhow::Error),
}

/// A playlist's revision and its rows as they are now.
async fn playlist_rows(session: &Session, id: &SpotifyId) -> Result<(Vec<u8>, Vec<edits::Row>)> {
    let items = fetch::playlist_items(session, id).await.map_err(|e| friendly("Couldn't read the playlist.", e, session))?;
    // Places only mean something with every row read.
    if !items.complete {
        bail!("Couldn't read all of the playlist. Try again.");
    }
    Ok((items.revision, items.uris.into_iter().zip(items.added).collect()))
}

impl Library {
    async fn header_revision(&self, session: &Session, id: &SpotifyId) -> Result<(Vec<u8>, i32)> {
        let header = fetch::playlist_header(session, id).await.map_err(|e| friendly("Couldn't read the playlist.", e, session))?;
        Ok((header.revision().to_vec(), header.length()))
    }

    /// Sends one change to a playlist made against its latest revision; on a conflict reads it
    /// again and sends once more. `repeat_safe`: an unclear answer may be sent again (setting the
    /// same name twice does no harm).
    async fn change_attributes(&self, session: &Session, id: &SpotifyId, build: impl Fn(&[u8]) -> ListChanges) -> Result<Sent> {
        let endpoint = fetch::playlist_changes_endpoint(id).map_err(|e| anyhow!("{e}"))?;
        let mut sent = Sent::Conflict;
        for _ in 0..2 {
            let (revision, _) = self.header_revision(session, id).await?;
            sent = fetch::post_list_changes(session, &endpoint, &build(&revision)).await;
            if !matches!(sent, Sent::Conflict | Sent::Unsure(_)) {
                break;
            }
        }
        Ok(sent)
    }

    // ------------------------------------------------------------------------------- search

    /// One page of search results: 10 songs from `offset`, and on the first page 10 albums,
    /// artists and playlists too.
    pub async fn search(&self, query: &str, offset: u32) -> Result<SearchResults> {
        let term = pathfinder::search_term(query);
        if term.is_empty() {
            return Ok(SearchResults { query: query.trim().to_owned(), offset, ..Default::default() });
        }
        let session = self.session();
        require_username(&session)?;
        match self.search_session(&session, &term, offset as usize).await {
            Ok(mut results) => {
                results.query = query.trim().to_owned();
                results.offset = offset;
                Ok(results)
            }
            Err(e) => match self.webapi() {
                Some(api) => {
                    log::warn!("search through the session failed ({e:#}); using the Web API");
                    api.search(query, offset).await
                }
                None => Err(e),
            },
        }
    }

    async fn search_session(&self, session: &Session, term: &str, offset: usize) -> Result<SearchResults> {
        let first = offset == 0;
        let kinds: &[&pathfinder::Query] =
            if first { &[&pathfinder::TRACKS, &pathfinder::ALBUMS, &pathfinder::ARTISTS, &pathfinder::PLAYLISTS] } else { &[&pathfinder::TRACKS] };
        let answers = futures::future::join_all(kinds.iter().map(|q| search_query(session, q, term, offset))).await;
        // A kind that failed (or whose query is retired) comes from one shared everything-search.
        let mut desktop: Option<Option<Value>> = None;
        let mut sections: Vec<Value> = Vec::with_capacity(kinds.len());
        let mut last_error = None;
        for (q, answer) in kinds.iter().zip(answers) {
            let section = match answer {
                Ok(Some(v)) => Some(v),
                Ok(None) => None,
                Err(e) => {
                    log::warn!("{}: {e}", q.operation);
                    last_error = Some(e);
                    None
                }
            };
            let section = match section {
                Some(s) => s,
                None => {
                    if desktop.is_none() {
                        desktop = Some(match search_query(session, &pathfinder::DESKTOP, term, offset).await {
                            Ok(v) => v,
                            Err(e) => {
                                last_error = Some(e);
                                None
                            }
                        });
                    }
                    match desktop.as_ref().and_then(Option::as_ref) {
                        Some(all) => all[q.section].clone(),
                        None => {
                            let e = last_error.take();
                            return Err(match e {
                                Some(e) => friendly("Couldn't search Spotify.", e, session),
                                None => anyhow!("Spotify's search changed and Truffle can't use it right now."),
                            });
                        }
                    }
                }
            };
            sections.push(section);
        }
        let tracks_section = &sections[0];
        let uris = pathfinder::track_uris(tracks_section);
        let tracks = build_tracks(session, &uris, &HashMap::new(), &|_, _| {}).await.map_err(|e| friendly("Couldn't load the songs found.", e, session))?;
        let total = pathfinder::total(tracks_section).min(1000);
        let more = !tracks.is_empty() && offset + tracks.len() < total;
        let mut results = SearchResults { tracks, more, ..Default::default() };
        if first {
            results.albums = pathfinder::album_cards(&sections[1]);
            results.artists = pathfinder::artist_cards(&sections[2]);
            results.playlists = pathfinder::playlist_cards(&sections[3]);
        }
        Ok(results)
    }

    /// One page of people (Spotify users) found through the everything-search: up to 10 from
    /// `offset` for what was typed and, when folding changes it ("𝓮mma" or "émma"), as many for
    /// the folded form; each once, those whose name holds what was typed first. Spotify decides
    /// who it sends: people it doesn't send can't be found.
    pub async fn search_users(&self, query: &str, offset: u32) -> Result<SearchResults> {
        let term = pathfinder::search_term(query);
        let mut results = SearchResults { query: query.trim().to_owned(), offset, ..Default::default() };
        if term.is_empty() {
            return Ok(results);
        }
        let session = self.session();
        require_username(&session)?;
        let terms = pathfinder::user_terms(&term);
        let answers = futures::future::join_all(terms.iter().map(|t| search_query(&session, &pathfinder::DESKTOP, t, offset as usize))).await;
        let mut sections = Vec::with_capacity(answers.len());
        let mut last_error = None;
        for answer in answers {
            match answer {
                Ok(Some(mut all)) => sections.push(all["users"].take()),
                Ok(None) => {}
                Err(e) => {
                    log::warn!("searchDesktop (people): {e}");
                    last_error = Some(e);
                }
            }
        }
        if sections.is_empty() {
            return Err(match last_error {
                Some(e) => friendly("Couldn't search Spotify.", e, &session),
                None => anyhow!("Spotify's search changed and Truffle can't use it right now."),
            });
        }
        let more = sections.iter().any(|s| pathfinder::has_more(s, offset as usize));
        results.users = pathfinder::merge_users(sections.iter().map(pathfinder::user_cards), &term);
        results.users_next = more.then_some(offset + pathfinder::LIMIT as u32);
        Ok(results)
    }

    // ------------------------------------------------------------------------------- songs

    /// Adds songs at the end of a playlist, 100 per change. Returns how many were added.
    pub async fn add_to_playlist(&self, uri: &str, uris: &[String]) -> Result<usize> {
        let id = playlist_spotify_id(uri)?;
        let songs = spotify_songs(uris)?;
        let session = self.session();
        let user = require_username(&session)?;
        let _turn = self.inner.write_gate.lock().await;
        self.add_songs(&session, &user, &id, &songs).await
    }

    /// Adds songs at the end of a playlist as `add_to_playlist` does, but only if it has none of
    /// them yet, other than `again` (ones the user was told it has, and said to add anyway). It has
    /// a song when it has its link, or another version of it: the same name by the same artists,
    /// as "Find duplicates" matches them (the single, when it has the album version). It is read
    /// from Spotify first, never from the saved copy (which may be behind): after any change of
    /// Truffle's on its way, and no other one comes between the read and the add. With only songs
    /// from `again` there is nothing to look for, and it isn't read.
    pub async fn add_new_to_playlist(&self, uri: &str, uris: &[String], again: &[String]) -> Result<Added> {
        let id = playlist_spotify_id(uri)?;
        let songs = spotify_songs(uris)?;
        let session = self.session();
        let user = require_username(&session)?;
        let _turn = self.inner.write_gate.lock().await;
        if !songs.iter().all(|s| again.contains(s)) {
            let items = match fetch::playlist_items(&session, &id).await {
                Ok(items) if items.complete => items,
                // Songs further down weren't read: they would look new.
                Ok(items) => return Ok(Added::Unchecked(anyhow!("only {} of its songs came", items.uris.len()))),
                Err(e) => return Ok(Added::Unchecked(friendly("Couldn't read the playlist.", e, &session))),
            };
            let mut have: HashSet<String> = items.uris.iter().cloned().collect();
            // Songs it hasn't by link may be there as another version.
            let others: Vec<String> = songs.iter().filter(|s| !have.contains(*s)).cloned().collect();
            if !others.is_empty() {
                match self.song_details(&session, uri, &items, &others).await {
                    Ok((list, asked)) => have.extend(text::same_songs(&list, &asked).into_iter().map(str::to_owned)),
                    // The check by link stands alone.
                    Err(e) => log::warn!("couldn't compare songs by name in {uri}: {e:#}"),
                }
            }
            if let Some(already) = AlreadyIn::find(uri, &songs, &have, again) {
                return Ok(Added::Already(already));
            }
        }
        self.add_songs(&session, &user, &id, &songs).await.map(Added::Songs)
    }

    /// The details of a playlist's songs (read as `items`) and of songs asked for (`uris`), to tell
    /// whether it has another version of one. Its songs' are the saved copy's while that is its
    /// revision, and the asked ones' come from the lists kept in memory: only what neither has is
    /// fetched. A song Spotify can't describe has no artists, so it matches only its own link.
    async fn song_details(&self, session: &Session, uri: &str, items: &fetch::PlaylistItems, uris: &[String]) -> Result<(Arc<Vec<TrackInfo>>, Vec<TrackInfo>)> {
        // The asked ones first: saving the playlist's may push a list out of memory.
        let asked = self.tracks_known_first(session, uris).await?;
        let list = self.playlist_details(session, uri, items).await?;
        Ok((list, asked))
    }

    /// The add itself, with the write gate held.
    async fn add_songs(&self, session: &Session, user: &str, id: &SpotifyId, songs: &[String]) -> Result<usize> {
        let endpoint = fetch::playlist_changes_endpoint(id).map_err(|e| anyhow!("{e}"))?;
        let (mut revision, mut length) = self.header_revision(session, id).await?;
        let mut done = 0usize;
        for chunk in songs.chunks(edits::ADD_CHUNK) {
            let n = chunk.len();
            let mut retried = false;
            loop {
                let now = edits::now_ms();
                let changes = edits::changes(user, &revision, vec![edits::add_last(chunk, now)], now);
                match fetch::post_list_changes(session, &endpoint, &changes).await {
                    Sent::Applied(reply) => {
                        done += n;
                        length += n as i32;
                        revision = match fetch::revision_of(&reply) {
                            Some(r) => r,
                            None => self.header_revision(session, id).await?.0,
                        };
                        break;
                    }
                    Sent::Conflict if !retried => {
                        retried = true;
                        (revision, length) = self.header_revision(session, id).await?;
                    }
                    Sent::Conflict => return Err(partial(done, songs.len(), "The playlist changed while saving. Please try again.")),
                    // (No Web API backup: its own retries could add the songs twice.)
                    Sent::Refused(e) => return Err(partial_err(done, songs.len(), friendly("Couldn't add the songs.", e, session))),
                    Sent::Unsure(e) => {
                        log::warn!("adding songs: no clear answer ({e}); checking");
                        tokio::time::sleep(CHECK_AFTER).await;
                        let (now_revision, now_length) = self.header_revision(session, id).await?;
                        if now_length >= length + n as i32 {
                            done += n;
                            (revision, length) = (now_revision, now_length);
                            break;
                        }
                        return Err(partial(done, songs.len(), "Spotify didn't confirm the change. Check the playlist before trying again."));
                    }
                }
            }
        }
        Ok(done)
    }

    /// Takes every copy of these songs out of a playlist. Returns how many rows went.
    pub async fn remove_from_playlist(&self, uri: &str, uris: &[String]) -> Result<usize> {
        let id = playlist_spotify_id(uri)?;
        let remove: HashSet<&str> = uris.iter().map(String::as_str).collect();
        let session = self.session();
        let user = require_username(&session)?;
        let _turn = self.inner.write_gate.lock().await;
        let endpoint = fetch::playlist_changes_endpoint(&id).map_err(|e| anyhow!("{e}"))?;
        for attempt in 0..2 {
            let items = fetch::playlist_items(&session, &id).await.map_err(|e| friendly("Couldn't read the playlist.", e, &session))?;
            let ops = edits::removal_ops(&items.uris, &remove);
            let rows = ops.len();
            if rows == 0 {
                return Ok(0);
            }
            let now = edits::now_ms();
            match fetch::post_list_changes(&session, &endpoint, &edits::changes(&user, &items.revision, ops, now)).await {
                Sent::Applied(_) => return Ok(rows),
                Sent::Conflict if attempt == 0 => continue,
                Sent::Conflict => bail!("The playlist changed while saving. Please try again."),
                Sent::Refused(e) => {
                    if web_api_may_help(&e)
                        && let Some(api) = self.webapi()
                    {
                        log::warn!("removing songs through the session was refused ({e}); using the Web API");
                        api.remove_items(&base62(&id)?, uris).await?;
                        return Ok(rows);
                    }
                    return Err(friendly("Couldn't remove the songs.", e, &session));
                }
                Sent::Unsure(e) => {
                    log::warn!("removing songs: no clear answer ({e}); checking");
                    tokio::time::sleep(CHECK_AFTER).await;
                    let now = fetch::playlist_items(&session, &id).await.map_err(|e| friendly("Couldn't read the playlist.", e, &session))?;
                    if !now.uris.iter().any(|u| remove.contains(u.as_str())) {
                        return Ok(rows);
                    }
                    bail!("Spotify didn't confirm the change. Check the playlist before trying again.");
                }
            }
        }
        bail!("The playlist changed while saving. Please try again.")
    }

    /// Takes rows out of a playlist by place (one copy of a song in it twice, not every copy):
    /// `before` is the playlist as it was read, `rows` places in it. Only those rows go. If the
    /// playlist changed since, each row is found again as its song's same copy; when that can't be
    /// told, nothing more goes. 100 rows per change. Returns how many went, and the error that
    /// stopped it, if one did. The saved copy is read again afterwards.
    pub async fn remove_rows(&self, uri: &str, before: &[TrackInfo], rows: &[usize]) -> (usize, Option<anyhow::Error>) {
        let mut done = 0;
        let result = self.take_out_rows(uri, before, rows, &mut done).await;
        if done > 0 {
            // The saved copy is the playlist as it was: read now, it shows as it is.
            if let Err(e) = self.playlist_now(uri, &|_, _| {}).await {
                log::warn!("reading {uri} again after removing rows: {e:#}");
                lock(&self.inner.state).revisions.remove(&normalized(uri));
            }
        }
        (done, result.err())
    }

    async fn take_out_rows(&self, uri: &str, before: &[TrackInfo], rows: &[usize], done: &mut usize) -> Result<()> {
        let id = playlist_spotify_id(uri)?;
        let session = self.session();
        let user = require_username(&session)?;
        let _turn = self.inner.write_gate.lock().await;
        let endpoint = fetch::playlist_changes_endpoint(&id).map_err(|e| anyhow!("{e}"))?;
        // The playlist as it is believed to be, the rows still to go (lowest first), and the
        // revision to send the next change on (`None`: read the playlist again first, as after
        // every change that went through).
        let mut list: Vec<edits::Row> = before.iter().map(|t| (t.uri.clone(), t.added_at)).collect();
        let mut left = rows.to_vec();
        left.sort_unstable();
        left.dedup();
        let mut base: Option<Vec<u8>> = None;
        let mut retried = false;
        while !left.is_empty() {
            let revision = match base.take() {
                Some(revision) => revision,
                None => {
                    let (revision, now) = playlist_rows(&session, &id).await?;
                    left = edits::rows_now(&list, &left, &now).ok_or_else(|| anyhow!(LIST_CHANGED))?;
                    list = now;
                    revision
                }
            };
            let chunk = edits::next_chunk(&mut left);
            let now = edits::now_ms();
            let changes = edits::changes(&user, &revision, edits::row_removals(&list, &chunk), now);
            match fetch::post_list_changes(&session, &endpoint, &changes).await {
                Sent::Applied(_) => {
                    *done += chunk.len();
                    edits::take_rows(&mut list, &chunk);
                    // Read again before the next change: a song added elsewhere meanwhile would
                    // shift the places, and Spotify may merge a change sent on an older revision.
                    base = None;
                    retried = false;
                }
                Sent::Conflict if !retried => {
                    retried = true;
                    left.extend(chunk);
                }
                Sent::Conflict => bail!(LIST_CHANGED),
                // (No Web API backup: it takes out every copy of a song.)
                Sent::Refused(e) => return Err(friendly("Couldn't remove the songs.", e, &session)),
                Sent::Unsure(e) => {
                    log::warn!("removing rows: no clear answer ({e}); checking");
                    tokio::time::sleep(CHECK_AFTER).await;
                    let (revision, now) = playlist_rows(&session, &id).await?;
                    edits::take_rows(&mut list, &chunk);
                    if now != list {
                        bail!("Spotify didn't confirm the change. Check the playlist before trying again.");
                    }
                    *done += chunk.len();
                    base = Some(revision);
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------- details

    /// Renames a playlist and/or changes its description (`Some("")` clears it).
    pub async fn update_playlist_details(&self, uri: &str, name: Option<&str>, description: Option<&str>) -> Result<()> {
        let _turn = self.inner.write_gate.lock().await;
        self.send_details(uri, name, description).await
    }

    /// Writes a listen-along link's description. Not behind the write gate the user's own
    /// playlist changes take turns at, but behind one of the link's own: nothing else changes
    /// link playlists (they are hidden from every list, so the details editor can't reach them),
    /// so adding a song never waits behind ten links, and quitting can tell every link at once.
    /// Writes to the same link still take turns: quitting's last word waits for a write already
    /// on its way (both read the same revision, and the older one would otherwise be sent again
    /// after the conflict and land last). A link ever opened for editing would need the gate again.
    pub async fn write_channel(&self, uri: &str, description: &str) -> Result<()> {
        let _turn = self.inner.channel_gates.turn(uri).await;
        self.send_details(uri, None, Some(description)).await
    }

    async fn send_details(&self, uri: &str, name: Option<&str>, description: Option<&str>) -> Result<()> {
        let id = playlist_spotify_id(uri)?;
        let session = self.session();
        let user = require_username(&session)?;
        let build = |revision: &[u8]| {
            let now = edits::now_ms();
            edits::changes(&user, revision, vec![edits::attrs(name, description, None, false)], now)
        };
        match self.change_attributes(&session, &id, build).await? {
            Sent::Applied(_) => Ok(()),
            Sent::Refused(e) if web_api_may_help(&e) && self.webapi().is_some() => {
                log::warn!("renaming through the session was refused ({e}); using the Web API");
                let api = self.webapi().ok_or_else(|| anyhow!("Couldn't save."))?;
                api.update_playlist_details(&base62(&id)?, name, description).await
            }
            Sent::Refused(e) => Err(friendly("Couldn't save the playlist's details.", e, &session)),
            Sent::Conflict => bail!("The playlist changed while saving. Please try again."),
            Sent::Unsure(e) => Err(friendly("Spotify didn't confirm the change. Check the playlist before trying again.", e, &session)),
        }
    }

    /// A new picture for a playlist, from an image file. Returns its address once Spotify has it.
    pub async fn set_playlist_cover(&self, uri: &str, image_path: &Path) -> Result<Option<String>> {
        let id = playlist_spotify_id(uri)?;
        let session = self.session();
        let user = require_username(&session)?;
        let path = image_path.to_path_buf();
        let jpeg = tokio::task::spawn_blocking(move || crate::webapi::prepare_cover_jpeg(&path))
            .await
            .map_err(|_| anyhow!("Truffle couldn't process the picture. Please try a different one."))??;
        let _turn = self.inner.write_gate.lock().await;
        let picture = match fetch::upload_playlist_picture(&session, &id, jpeg).await {
            Ok(p) => p,
            Err(e) => {
                if let Some(api) = self.webapi() {
                    log::warn!("cover upload through the session failed ({e}); using the Web API");
                    api.upload_playlist_cover(&base62(&id)?, image_path).await?;
                    return Ok(None);
                }
                return Err(friendly("Spotify didn't accept the picture from Truffle.", e, &session));
            }
        };
        let build = |revision: &[u8]| {
            let now = edits::now_ms();
            edits::changes(&user, revision, vec![edits::attrs(None, None, Some(picture.clone()), false)], now)
        };
        match self.change_attributes(&session, &id, build).await? {
            Sent::Applied(_) => Ok(convert::image_url(&picture)),
            Sent::Refused(e) => Err(friendly("Couldn't change the playlist cover.", e, &session)),
            Sent::Conflict => bail!("The playlist changed while saving. Please try again."),
            Sent::Unsure(e) => Err(friendly("Spotify didn't confirm the change. Check the playlist before trying again.", e, &session)),
        }
    }

    // ------------------------------------------------------------------------------- library list

    /// Makes a playlist (private, first in the library list). Returns its uri.
    pub async fn create_playlist(&self, name: &str) -> Result<String> {
        self.make_playlist(name, false, &mut None).await
    }

    /// Makes a listen-along link's playlist. The log never names it: anyone with the playlist can
    /// listen. `made`: its uri as soon as the playlist is made, also if adding it to the library
    /// then fails or is cut off (it may be there all the same, and every list hides it).
    pub async fn create_channel(&self, made: &mut Option<String>) -> Result<String> {
        self.make_playlist(crate::listen::CHANNEL_NAME, true, made).await
    }

    /// `quiet`: the log says "a link's playlist" instead of its uri. `made`: its uri once step 1
    /// made it.
    async fn make_playlist(&self, name: &str, quiet: bool, made: &mut Option<String>) -> Result<String> {
        let session = self.session();
        let user = require_username(&session)?;
        let _turn = self.inner.write_gate.lock().await;
        let id = SpotifyId::from_raw(&rand::random::<u128>().to_be_bytes()).map_err(|e| anyhow!("{e}"))?;
        let mut uri = format!("spotify:playlist:{}", base62(&id)?);
        let endpoint = fetch::playlist_changes_endpoint(&id).map_err(|e| anyhow!("{e}"))?;
        // 1. The first change makes the playlist (never resent with a new id: two would appear).
        let now = edits::now_ms();
        let make = edits::changes(&user, edits::NEW_PLAYLIST_REVISION, vec![edits::attrs(Some(name), None, None, false)], now);
        match fetch::post_list_changes(&session, &endpoint, &make).await {
            Sent::Applied(_) => {}
            Sent::Unsure(e) => {
                log::warn!("making a playlist: no clear answer ({e}); checking");
                tokio::time::sleep(CHECK_AFTER).await;
                if fetch::playlist_header(&session, &id).await.is_err() {
                    bail!("Spotify didn't confirm the new playlist. Use Refresh to check before trying again.");
                }
            }
            Sent::Conflict | Sent::Refused(_) => {
                log::info!("Spotify didn't take a playlist id of Truffle's own; asking it for one");
                match fetch::create_playlist_web(&session, name).await {
                    Sent::Applied(reply) => match serde_json::from_slice::<Value>(&reply).ok().and_then(|v| v["uri"].as_str().map(str::to_owned)) {
                        Some(made) => {
                            uri = convert::normalize_playlist_uri(&made).unwrap_or(made);
                        }
                        None => bail!("Spotify made the playlist but didn't say which. Use Refresh."),
                    },
                    Sent::Unsure(_) => bail!("Spotify didn't confirm the new playlist. Use Refresh to check before trying again."),
                    Sent::Conflict => bail!("Couldn't make the playlist. Please try again."),
                    Sent::Refused(e) => return Err(friendly("Couldn't make the playlist.", e, &session)),
                }
            }
        }
        let label = if quiet { "a link's playlist".to_owned() } else { format!("playlist {uri}") };
        log::info!("made {label}");
        *made = Some(uri.clone());
        // 2. Into the library list, first.
        let endpoint = fetch::rootlist_changes_endpoint(&session);
        for attempt in 0..2 {
            let (revision, _) = fetch::rootlist_raw(&session).await.map_err(|e| friendly("Couldn't read your library.", e, &session))?;
            let now = edits::now_ms();
            let add = edits::changes(&user, &revision, vec![edits::add_first(&uri, now, false)], now);
            match fetch::post_list_changes(&session, &endpoint, &add).await {
                Sent::Applied(_) => return Ok(uri),
                Sent::Conflict if attempt == 0 => continue,
                Sent::Unsure(e) => {
                    log::warn!("adding {label} to the library: no clear answer ({e}); checking");
                    tokio::time::sleep(CHECK_AFTER).await;
                    if fetch::rootlist_raw(&session).await.is_ok_and(|(_, list)| list.iter().any(|u| convert::normalize_playlist_uri(u).as_deref() == Some(uri.as_str()))) {
                        return Ok(uri);
                    }
                    break;
                }
                Sent::Conflict | Sent::Refused(_) => break,
            }
        }
        log::warn!("made {label} but couldn't add it to the library list");
        bail!("Made the playlist, but couldn't add it to your library. Use Refresh.")
    }

    /// Takes a playlist out of the library list (for one the user made, that is deleting it;
    /// Spotify keeps it 90 days to recover).
    pub async fn remove_playlist(&self, uri: &str) -> Result<()> {
        self.unlist_playlist(uri, false).await
    }

    /// Takes a removed listen-along link's playlist out of the library list, without naming it in
    /// the log.
    pub async fn remove_channel(&self, uri: &str) -> Result<()> {
        self.unlist_playlist(uri, true).await
    }

    /// `quiet`: the log says "a link's playlist" instead of its uri.
    async fn unlist_playlist(&self, uri: &str, quiet: bool) -> Result<()> {
        let target = convert::normalize_playlist_uri(uri).ok_or_else(|| anyhow!("That isn't a Spotify playlist."))?;
        let session = self.session();
        let user = require_username(&session)?;
        let _turn = self.inner.write_gate.lock().await;
        let endpoint = fetch::rootlist_changes_endpoint(&session);
        let rows = |list: &[String]| -> Vec<usize> {
            list.iter().enumerate().filter(|(_, u)| convert::normalize_playlist_uri(u).as_deref() == Some(target.as_str())).map(|(i, _)| i).collect()
        };
        let listed = |list: &[String]| (!rows(list).is_empty()).then_some(());
        for attempt in 0..2 {
            let (revision, list) = fetch::rootlist_raw(&session).await.map_err(|e| friendly("Couldn't read your library.", e, &session))?;
            let found = rows(&list);
            if found.is_empty() {
                return Ok(());
            }
            // Highest row first, so the earlier ones keep their places.
            let ops = found.iter().rev().map(|&i| edits::rem_one(i, &list[i])).collect();
            let now = edits::now_ms();
            let rem = edits::changes(&user, &revision, ops, now);
            match fetch::post_list_changes(&session, &endpoint, &rem).await {
                Sent::Applied(_) => return Ok(()),
                Sent::Conflict if attempt == 0 => continue,
                Sent::Conflict => break,
                Sent::Refused(e) => {
                    if web_api_may_help(&e)
                        && let Some(api) = self.webapi()
                    {
                        log::warn!("removing a playlist through the session was refused ({e}); using the Web API");
                        return api.remove_playlist(uri).await;
                    }
                    return Err(friendly("Couldn't remove the playlist.", e, &session));
                }
                Sent::Unsure(e) => {
                    let label = if quiet { "a link's playlist" } else { target.as_str() };
                    log::warn!("removing {label}: no clear answer ({e}); checking");
                    tokio::time::sleep(CHECK_AFTER).await;
                    if fetch::rootlist_raw(&session).await.is_ok_and(|(_, list)| listed(&list).is_none()) {
                        return Ok(());
                    }
                    bail!("Spotify didn't confirm the change. Check your library before trying again.");
                }
            }
        }
        bail!("Your library changed while saving. Please try again.")
    }
}

/// One of the persisted searches; `Ok(None)` when every hash of it is retired. The answer is the
/// query's own section (or, for the everything-search, all of `searchV2`).
async fn search_query(session: &Session, q: &pathfinder::Query, term: &str, offset: usize) -> Result<Option<Value>, CoreError> {
    for hash in q.hashes {
        if let Some(answer) = fetch::pathfinder(session, q.operation, hash, pathfinder::variables(term, offset)).await? {
            let all = &answer["data"]["searchV2"];
            if !all.is_object() {
                log::warn!("{} gave no result: {}", q.operation, answer["errors"]);
                return Ok(None);
            }
            return Ok(Some(if q.section.is_empty() { all.clone() } else { all[q.section].clone() }));
        }
        log::warn!("{} hash {} is retired", q.operation, &hash[..8]);
    }
    Ok(None)
}

fn partial(done: usize, total: usize, then: &str) -> anyhow::Error {
    if done == 0 { anyhow!("{then}") } else { anyhow!("Added {done} of {total} songs. {then}") }
}

fn partial_err(done: usize, total: usize, e: anyhow::Error) -> anyhow::Error {
    if done == 0 { e } else { anyhow!("Added {done} of {total} songs. {e}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ArtistRef;

    const PLAYLIST: &str = "spotify:playlist:p";

    /// A song `id` called `name` by The Wanted.
    fn song(id: &str, name: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{id}"),
            name: name.into(),
            artists: vec![ArtistRef { name: "The Wanted".into(), uri: "spotify:artist:tw".into() }],
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 198_000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    fn uris(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| format!("spotify:track:{id}")).collect()
    }

    /// What an add counts as in a playlist of `list`: its links, and, when the details of the songs
    /// asked for could be had (`asked`), another version of one.
    fn there(list: &[TrackInfo], asked: Option<&[TrackInfo]>) -> HashSet<String> {
        let mut have: HashSet<String> = list.iter().map(|t| t.uri.clone()).collect();
        if let Some(asked) = asked {
            have.extend(text::same_songs(list, asked).into_iter().map(str::to_owned));
        }
        have
    }

    #[test]
    fn the_single_is_already_there_when_the_album_version_is() {
        let list = [song("album", "Glad You Came"), song("sun", "Chasing the Sun")];
        let asked = [song("single", "Glad You Came"), song("new", "Lightning")];
        let have = there(&list, Some(&asked));
        let found = AlreadyIn::find(PLAYLIST, &uris(&["single", "new"]), &have, &[]).expect("the single is there");
        assert_eq!((found.already, found.songs), (1, 2));
        // ONLY NEW ONES leaves it out.
        assert_eq!(found.new, uris(&["new"]));
        // ADD ALL: the box said it is there, so checked again it goes in without asking.
        assert_eq!(AlreadyIn::find(PLAYLIST, &found.uris, &have, &uris(&["single"])), None);
    }

    #[test]
    fn without_details_only_the_link_counts() {
        let list = [song("album", "Glad You Came"), convert::unavailable_info("spotify:track:gone")];
        // The details couldn't be had: by link alone.
        assert_eq!(AlreadyIn::find(PLAYLIST, &uris(&["single"]), &there(&list, None), &[]), None);
        // Spotify couldn't describe the songs asked for: no name or artists to match by.
        let asked = [convert::unavailable_info("spotify:track:single"), convert::unavailable_info("spotify:track:gone2")];
        assert_eq!(AlreadyIn::find(PLAYLIST, &uris(&["single", "gone2"]), &there(&list, Some(&asked)), &[]), None);
        // The link still counts.
        let found = AlreadyIn::find(PLAYLIST, &uris(&["album", "single"]), &there(&list, None), &[]).expect("album is there");
        assert_eq!((found.already, found.new), (1, uris(&["single"])));
    }
}
