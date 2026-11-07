//! Searching Spotify and changing which songs and playlists the user has: add songs (asking first
//! about ones a playlist has already) or remove them, find and remove duplicates, make a playlist,
//! remove one from the library. All of it goes through the session, as Spotify's own apps do (the
//! optional Web API connection is only a fallback).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::jobs::Job;
use super::{Controller, user_error};
use crate::library::{Added, Library};
use crate::model::{AlreadyIn, Event, PlayStats, Source, TrackInfo};

/// How long a removed playlist stays hidden while Spotify's list still shows it.
const REMOVED_HIDE_FOR: Duration = Duration::from_secs(30 * 60);

/// Artists whose pictures are fetched for the Artists page (the most liked).
const COLLECTION_PICTURES: usize = 300;

/// What a finished browse job did.
pub(super) enum BrowseDone {
    /// Songs were added to or removed from this playlist.
    Items { playlist_uri: String, added: bool, count: usize },
    /// Songs weren't added: the playlist has some of them already.
    Already(AlreadyIn),
    /// Songs weren't added: the playlist couldn't be read to see whether it has them (why).
    /// `question`: the one the add answered, to ask again.
    Unchecked { playlist_uri: String, error: anyhow::Error, question: Option<AlreadyIn> },
    /// Of `asked` duplicates, `removed` went from this playlist; `error` stopped the rest.
    Duplicates { playlist_uri: String, asked: usize, removed: usize, error: Option<anyhow::Error> },
    /// A playlist was made.
    Created { uri: String, name: String },
    /// A playlist was removed from the library.
    Removed { uri: String },
    /// Spotify described the most played song (for Settings > About).
    TopSong { uri: String, track: TrackInfo },
}

impl Controller {
    pub(super) fn cmd_search(&mut self, query: String, offset: u32) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::SearchFailed { query, message: "Log in to Spotify first.".into() });
            return;
        };
        let ui = self.ui.clone();
        tokio::spawn(async move {
            match library.search(&query, offset).await {
                Ok(results) => ui.send(Event::SearchResults(Arc::new(results))),
                Err(e) => {
                    log::warn!("search for {query:?} failed: {e:#}");
                    ui.send(Event::SearchFailed { message: user_error("Couldn't search Spotify.", &e), query });
                }
            }
        });
    }

    pub(super) fn cmd_search_users(&mut self, query: String, offset: u32) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::SearchUsersFailed { query, message: "Log in to Spotify first.".into() });
            return;
        };
        let ui = self.ui.clone();
        tokio::spawn(async move {
            match library.search_users(&query, offset).await {
                Ok(results) => ui.send(Event::SearchUsers(Arc::new(results))),
                Err(e) => {
                    log::warn!("search for people {query:?} failed: {e:#}");
                    ui.send(Event::SearchUsersFailed { message: user_error("Couldn't search Spotify.", &e), query });
                }
            }
        });
    }

    /// The albums and artists of Liked Songs, then again with the artists' pictures (one request
    /// each, so only for the most liked).
    pub(super) fn cmd_collection(&mut self) {
        let Some(library) = self.library.clone() else {
            // Still logging in: done once the library is there.
            self.pending_collection = true;
            return;
        };
        let ui = self.ui.clone();
        library.set_play_counts(self.history.play_counts.clone());
        // Tagged with the account: a logout and another login while this runs drops what it finds.
        let account = library.username();
        let send = move |event: Event| ui.send(Event::ForAccount { account: account.clone(), event: Box::new(event) });
        tokio::spawn(async move {
            let mut c = match library.collection().await {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("collection: {e:#}");
                    return;
                }
            };
            send(Event::Collection(Arc::new(c.clone())));
            // Truffle's mixes: the fixed ones and the three most liked artists'.
            let mut by_songs: Vec<&crate::model::CollectionItem> = c.artists.iter().collect();
            by_songs.sort_by_key(|a| std::cmp::Reverse(a.songs));
            let artists: Vec<(String, String)> = by_songs.iter().take(3).map(|a| (a.uri.clone(), a.name.clone())).collect();
            send(Event::Mixes(Arc::new(library.mixes(&artists).await)));
            // The artists followed: first bare (for the Follow buttons), names and pictures later.
            let followed = match library.followed_artists().await {
                Ok(uris) => {
                    log::info!("following {} artists", uris.len());
                    let bare = uris.iter().enumerate().map(|(k, uri)| crate::model::CollectionItem { uri: uri.clone(), newest: k, ..Default::default() });
                    send(Event::Following { list: Arc::new(bare.collect()), named: false });
                    Some(uris)
                }
                Err(e) => {
                    log::warn!("followed artists: {e:#}");
                    None
                }
            };
            // New releases out in the last 90 days: by the 50 artists followed last, else by the 40
            // most liked.
            let top: Vec<String> = match followed.as_ref().filter(|f| !f.is_empty()) {
                Some(uris) => uris.iter().take(50).cloned().collect(),
                None => {
                    let mut liked_most: Vec<(usize, String)> = c.artists.iter().map(|a| (a.songs, a.uri.clone())).collect();
                    liked_most.sort_by_key(|a| std::cmp::Reverse(a.0));
                    liked_most.into_iter().take(40).map(|(_, u)| u).collect()
                }
            };
            let releases = library.new_releases(top, 90).await;
            send(Event::NewReleases(Arc::new(releases)));
            let mut most: Vec<(usize, String)> = c.artists.iter().map(|a| (a.songs, a.uri.clone())).collect();
            most.sort_by_key(|a| std::cmp::Reverse(a.0));
            let uris: Vec<String> = most.into_iter().take(COLLECTION_PICTURES).map(|(_, u)| u).collect();
            let pictures = library.artist_pictures(uris).await;
            for a in &mut c.artists {
                if let Some(p) = pictures.get(&a.uri) {
                    a.image = Some(p.clone());
                }
            }
            send(Event::Collection(Arc::new(c)));
            // Last: the followed artists' names and pictures (one lookup each).
            if let Some(uris) = followed {
                send(Event::Following { list: Arc::new(library.artist_briefs(uris).await), named: true });
            }
        });
    }

    /// Follows or unfollows an artist (the UI has shown it already, and undoes it if Spotify
    /// refuses). Each artist's writes go through its own queue, one after the other, in the order
    /// asked; every one is answered with Event::FollowDone, in that order.
    pub(super) fn cmd_follow(&mut self, uri: String, follow: bool, generation: u64) {
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::FollowDone { uri, follow, generation, ok: false });
            return;
        };
        let queue = self.follow_queues.entry(uri.clone()).or_insert_with(|| {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Library, bool, u64)>();
            let (ui, uri) = (self.ui.clone(), uri.clone());
            tokio::spawn(async move {
                while let Some((library, follow, generation)) = rx.recv().await {
                    let result = library.set_following(&uri, follow).await;
                    if let Err(e) = &result {
                        log::warn!("follow {uri} ({follow}): {e:#}");
                        let lead = if follow { "Couldn't follow this artist." } else { "Couldn't unfollow this artist." };
                        ui.send(Event::Error(user_error(lead, e)));
                    }
                    ui.send(Event::FollowDone { uri: uri.clone(), follow, generation, ok: result.is_ok() });
                }
            });
            tx
        });
        if let Err(tokio::sync::mpsc::error::SendError((_, follow, generation))) = queue.send((library, follow, generation)) {
            self.ui.send(Event::FollowDone { uri, follow, generation, ok: false });
        }
    }

    /// A user's profile page.
    pub(super) fn cmd_user_page(&mut self, username: String) {
        let Some(library) = self.library.clone() else { return };
        let ui = self.ui.clone();
        tokio::spawn(async move {
            match library.user_page(&username).await {
                Ok(page) => ui.send(Event::UserPage(Arc::new(page))),
                Err(e) => ui.send(Event::ProfileFailed { message: user_error("Couldn't load the profile.", &e), username }),
            }
        });
    }

    /// Follows (or unfollows) a user; the UI shows it already and undoes it if Spotify refuses.
    pub(super) fn cmd_follow_user(&mut self, username: String, follow: bool) {
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::FollowUserDone { username, follow, ok: false });
            return;
        };
        let ui = self.ui.clone();
        tokio::spawn(async move {
            let result = library.set_following_user(&username, follow).await;
            if let Err(e) = &result {
                ui.send(Event::Error(user_error(if follow { "Couldn't follow them." } else { "Couldn't unfollow them." }, e)));
            }
            ui.send(Event::FollowUserDone { username, follow, ok: result.is_ok() });
        });
    }

    /// An artist's songs in Liked Songs.
    /// An artist's likes: at once from the Liked Songs list in memory and the likes as they are
    /// now, so the page never waits and a like counts the moment it is made. Only when the list
    /// isn't in memory is it read (from disk, or Spotify) first.
    pub(super) fn cmd_artist_likes(&mut self, artist: String) {
        let Some(library) = self.library.clone() else { return };
        let whole = match library.artist_likes_now(&artist, &self.liked) {
            Some((likes, whole)) => {
                self.ui.send(Event::ArtistLikes(Arc::new(likes)));
                whole
            }
            None => false,
        };
        if whole {
            return;
        }
        // A like whose details aren't in memory, or no list yet: counted again once it is read.
        let (ui, liked) = (self.ui.clone(), self.liked.clone());
        tokio::spawn(async move {
            match library.artist_likes(&artist, &liked).await {
                Ok(likes) => ui.send(Event::ArtistLikes(Arc::new(likes))),
                Err(e) => log::warn!("liked songs of {artist}: {e:#}"),
            }
        });
    }

    /// Who a user follows, or who follows them.
    pub(super) fn cmd_people(&mut self, username: String, followers: bool) {
        let Some(library) = self.library.clone() else { return };
        let ui = self.ui.clone();
        tokio::spawn(async move {
            match library.people(&username, followers).await {
                Ok(people) => ui.send(Event::People(Arc::new(people))),
                Err(e) => ui.send(Event::ProfileFailed { message: user_error("Couldn't load the list.", &e), username }),
            }
        });
    }

    /// "Your Truffle" on Settings > About: the play counts added up, and the most played song's
    /// name and cover from a list already loaded (or from the last time). Not known: the numbers go
    /// at once, and the song follows once Spotify has described it.
    pub(super) fn cmd_play_stats(&mut self) {
        let stats = self.play_stats();
        let wanted = stats.top.as_ref().filter(|top| top.track.is_none()).map(|top| top.uri.clone());
        self.ui.send(Event::PlayStats(stats));
        let (Some(uri), Some(library)) = (wanted, self.library.clone()) else { return };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            match library.track(&uri).await {
                Ok(track) => {
                    let _ = tx.send(Job::Browse { epoch, result: Ok(BrowseDone::TopSong { uri, track }) });
                }
                Err(e) => log::debug!("the most played song {uri}: {e:#}"),
            }
        });
    }

    /// The play counts added up, with the most played song's details when they are known (and
    /// kept for the next time).
    fn play_stats(&mut self) -> PlayStats {
        let mut stats = PlayStats::from_counts(&self.history.play_counts);
        if let Some(top) = stats.top.as_mut() {
            top.track = self.known_track(&top.uri);
            if let Some(track) = &top.track {
                self.top_song = Some((top.uri.clone(), track.clone()));
            }
        }
        stats
    }

    /// A song's details from what is loaded already: the playing song, the playing list, the list
    /// open in the UI, the most played song found before and the lists the library keeps in memory.
    fn known_track(&self, uri: &str) -> Option<TrackInfo> {
        let playing = self.pb.track.iter().chain(self.queue.iter().flat_map(|q| q.tracks().iter()));
        let shown = self.shown.iter().flat_map(|(_, tracks)| tracks.iter());
        let found_before = self.top_song.as_ref().filter(|(u, _)| u == uri).map(|(_, t)| t);
        playing.chain(shown).find(|t| t.uri == uri).or(found_before).cloned().or_else(|| self.library.as_ref()?.cached_track(uri))
    }

    /// Fetches a song's Canvas from Spotify: a video is downloaded into the cache (kept there for the
    /// next time), a picture only named for the UI to load.
    pub(super) fn cmd_canvas(&mut self, uri: String) {
        let Some(session) = self.session.clone() else { return };
        let ui = self.ui.clone();
        let dir = self.paths.cache.join("canvas");
        tokio::spawn(async move {
            let found = match crate::canvas::fetch(&session, std::slice::from_ref(&uri)).await {
                Ok(found) => found.into_iter().next(),
                Err(e) => {
                    log::debug!("no canvas for {uri}: {e}");
                    None
                }
            };
            let (video, picture) = match found {
                Some(c) if c.is_video() => match crate::canvas::download(&c.url, &dir).await {
                    Ok(path) => (Some(path), None),
                    Err(e) => {
                        log::warn!("canvas for {uri} not downloaded: {e:#}");
                        (None, None)
                    }
                },
                Some(c) => (None, Some(c.url)),
                None => (None, None),
            };
            ui.send(Event::Canvas { uri, video, picture });
        });
    }

    /// Which of the user's playlists hold every one of `uris`, from the lists Truffle has in memory or on
    /// disk (no network); the Add to playlist menu marks them Added. A playlist never opened is unknown,
    /// so it isn't marked.
    pub(super) fn cmd_held_by(&mut self, uris: Vec<String>) {
        let Some(library) = self.library.clone() else { return };
        let ui = self.ui.clone();
        let lists: Vec<String> = self.playlists.iter().filter(|pl| pl.editable).map(|pl| pl.uri.clone()).collect();
        tokio::task::spawn_blocking(move || {
            let mut playlists = Vec::new();
            for pl in lists {
                if let Some(tracks) = library.cached_tracks(&Source::Playlist(pl.clone()))
                    && holds_all(&uris, tracks.iter().map(|t| t.uri.as_str()))
                {
                    playlists.push(pl);
                }
            }
            ui.send(Event::HeldBy { uris, playlists });
        });
    }

    /// Fetches a song's lyrics from Spotify (the same service its own app uses). When Spotify has them
    /// without times (or not at all), LRCLIB's timed ones are taken if it has them.
    pub(super) fn cmd_lyrics(&mut self, uri: String) {
        let Some(session) = self.session.clone() else { return };
        let ui = self.ui.clone();
        let track = self.known_track(&uri);
        tokio::spawn(async move {
            let id = match librespot_core::SpotifyUri::from_uri(&uri) {
                Ok(librespot_core::SpotifyUri::Track { id }) => Some(id),
                _ => None,
            };
            let found = match id {
                Some(id) => match librespot_metadata::lyrics::Lyrics::get(&session, &id).await {
                    Ok(l) => Some(l),
                    Err(e) => {
                        log::debug!("no lyrics for {uri}: {e}");
                        None
                    }
                },
                None => None,
            };
            let (lines, synced) = match found {
                Some(l) => {
                    let synced = l.lyrics.sync_type == librespot_metadata::lyrics::SyncType::LineSynced;
                    let lines: Vec<crate::model::LyricLine> = l
                        .lyrics
                        .lines
                        .into_iter()
                        .filter(|x| !x.words.trim().is_empty())
                        .map(|x| crate::model::LyricLine { start_ms: x.start_time_ms.parse().unwrap_or(0), words: x.words })
                        .collect();
                    (if lines.is_empty() { None } else { Some(Arc::new(lines)) }, synced)
                }
                None => (None, false),
            };
            let (lines, synced) = match track.filter(|_| !synced) {
                Some(track) => match crate::lrclib::find(&track).await {
                    crate::lrclib::Found::Timed(timed) => (Some(Arc::new(timed)), true),
                    crate::lrclib::Found::Plain(plain) if lines.is_none() => (Some(Arc::new(plain)), false),
                    _ => (lines, synced),
                },
                None => (lines, synced),
            };
            ui.send(Event::Lyrics { uri, lines, synced });
        });
    }

    /// Adds songs to a playlist if it has none of them yet (as Spotify has it now, not as a list
    /// on screen or saved shows it), other than `again` (the user said to add those anyway). If it
    /// has some, nothing is added and the user is asked (Event::AlreadyInPlaylist); the answer
    /// comes back as another add, checked the same way. If that check fails, the question it
    /// answers (`question`) is asked again, so trying again is one click.
    pub(super) fn cmd_add_new(&mut self, playlist_uri: String, uris: Vec<String>, again: Vec<String>, question: Option<AlreadyIn>) {
        if uris.is_empty() {
            return;
        }
        let Some((library, uri)) = self.edit_target(&playlist_uri) else { return };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = library.add_new_to_playlist(&uri, &uris, &again).await.map(|added| match added {
                Added::Songs(count) => BrowseDone::Items { playlist_uri, added: true, count },
                Added::Already(already) => BrowseDone::Already(already),
                Added::Unchecked(error) => BrowseDone::Unchecked { playlist_uri, error, question },
            });
            let _ = tx.send(Job::Browse { epoch, result });
        });
    }

    pub(super) fn cmd_remove_items(&mut self, playlist_uri: String, uris: Vec<String>) {
        if uris.is_empty() {
            return;
        }
        let Some((library, uri)) = self.edit_target(&playlist_uri) else { return };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let done = library.remove_from_playlist(&uri, &uris).await.map(|count| BrowseDone::Items { playlist_uri, added: false, count });
            let _ = tx.send(Job::Browse { epoch, result: done });
        });
    }

    /// Reads a playlist as Spotify has it now, for "Find duplicates".
    pub(super) fn cmd_find_duplicates(&mut self, playlist_uri: String) {
        let Some((library, uri)) = self.edit_target(&playlist_uri) else {
            // (It said why.)
            self.ui.send(Event::Duplicates { uri: playlist_uri, tracks: None });
            return;
        };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = library.playlist_now(&uri, &|_, _| {}).await;
            let _ = tx.send(Job::Duplicates { epoch, uri, result });
        });
    }

    /// The playlist read for "Find duplicates" goes to the UI (or why it couldn't be read).
    pub(super) fn on_duplicates_read(&mut self, uri: String, result: Result<Arc<Vec<TrackInfo>>>) {
        let tracks = match result {
            Ok(tracks) => Some(tracks),
            Err(e) => {
                self.ui.send(Event::Error(user_error("Couldn't read the playlist.", &e)));
                None
            }
        };
        self.ui.send(Event::Duplicates { uri, tracks });
    }

    /// Takes the rows picked in "Find duplicates" out of the playlist, by place.
    pub(super) fn cmd_remove_duplicates(&mut self, playlist_uri: String, tracks: Arc<Vec<TrackInfo>>, rows: Vec<usize>) {
        if rows.is_empty() {
            return;
        }
        let Some((library, uri)) = self.edit_target(&playlist_uri) else { return };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let (removed, error) = library.remove_rows(&uri, &tracks, &rows).await;
            let done = BrowseDone::Duplicates { playlist_uri: uri, asked: rows.len(), removed, error };
            let _ = tx.send(Job::Browse { epoch, result: Ok(done) });
        });
    }

    pub(super) fn cmd_create_playlist(&mut self, name: String) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.ui.send(Event::Error("A playlist needs a name.".into()));
            return;
        }
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::Error("Log in to Spotify first.".into()));
            return;
        };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = library.create_playlist(&name).await.map(|uri| BrowseDone::Created { uri, name });
            let _ = tx.send(Job::Browse { epoch, result });
        });
    }

    pub(super) fn cmd_remove_playlist(&mut self, uri: String) {
        let Some(library) = self.library.clone() else {
            self.ui.send(Event::Error("Log in to Spotify first.".into()));
            return;
        };
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = library.remove_playlist(&uri).await.map(|()| BrowseDone::Removed { uri });
            let _ = tx.send(Job::Browse { epoch, result });
        });
    }

    /// Leaves out playlists removed a moment ago that Spotify's list still has; forgets the ones
    /// it no longer has (or that were removed long ago).
    pub(super) fn hide_removed_playlists(&mut self, playlists: &mut Vec<crate::model::PlaylistSummary>) {
        self.removed_playlists
            .retain(|uri, at| at.elapsed() < REMOVED_HIDE_FOR && playlists.iter().any(|p| p.uri == *uri));
        if !self.removed_playlists.is_empty() {
            playlists.retain(|p| !self.removed_playlists.contains_key(&p.uri));
        }
    }

    pub(super) fn on_browse_done(&mut self, result: Result<BrowseDone>) {
        let done = match result {
            Ok(done) => done,
            Err(e) => {
                log::warn!("changing playlists failed: {e:#}");
                self.webapi_failed(&e);
                self.ui.send(Event::Error(user_error("Couldn't change your playlists.", &e)));
                // Whatever did or didn't land: show the playlists as Spotify has them now.
                self.request_library(true);
                return;
            }
        };
        match done {
            BrowseDone::Items { playlist_uri, added, count } => {
                log::info!("{} {count} songs {} {playlist_uri}", if added { "added" } else { "removed" }, if added { "to" } else { "from" });
                let name = self.playlists.iter().find(|p| p.uri == playlist_uri).map(|p| p.name.clone()).unwrap_or_else(|| "the playlist".into());
                let songs = if count == 1 { "1 song".to_owned() } else { format!("{count} songs") };
                let text = match (count, added) {
                    (0, false) => format!("Already gone from {name}"),
                    (_, true) => format!("Added {songs} to {name}"),
                    (_, false) => format!("Removed {songs} from {name}"),
                };
                self.ui.send(Event::Status(text));
                // Show the change: reload the list if it is on screen, and the playlist list (counts).
                let source = Source::Playlist(playlist_uri);
                if self.open_source.as_ref() == Some(&source) {
                    self.cmd_open_source(source, true);
                }
                self.request_library(true);
            }
            BrowseDone::Already(already) => {
                log::info!("{} of {} songs are in {} already; asking", already.already, already.songs, already.playlist_uri);
                self.ui.send(Event::AlreadyInPlaylist(already));
            }
            BrowseDone::Unchecked { playlist_uri, error, question } => {
                // Nothing changed: no need to read the playlists again.
                log::warn!("couldn't check {playlist_uri} before adding to it: {error:#}");
                self.ui.send(Event::Error("Couldn't check the playlist. Try again.".into()));
                // An answer's check: its question comes back, to try again from.
                if let Some(question) = question {
                    self.ui.send(Event::AlreadyInPlaylist(question));
                }
            }
            BrowseDone::Duplicates { playlist_uri, asked, removed, error } => {
                log::info!("removed {removed} of {asked} duplicates from {playlist_uri}");
                match error {
                    None => self.ui.send(Event::Status(removed_duplicates(removed))),
                    Some(e) => {
                        let why = user_error("Couldn't remove the duplicates.", &e);
                        let text = if removed == 0 { why } else { format!("Removed {removed} of {asked} duplicates. {why}") };
                        self.ui.send(Event::Error(text));
                    }
                }
                // The library read the playlist again after the change: its saved copy is current.
                let source = Source::Playlist(playlist_uri);
                if self.open_source.as_ref() == Some(&source) {
                    self.cmd_open_source(source, false);
                }
                self.request_library(false);
            }
            BrowseDone::Created { uri, name } => {
                self.ui.send(Event::Status(format!("Made the playlist {name}")));
                log::info!("created playlist {uri}");
                self.request_library(true);
            }
            BrowseDone::Removed { uri } => {
                log::info!("removed playlist {uri} from the library");
                self.removed_playlists.insert(uri.clone(), Instant::now());
                self.playlists.retain(|p| p.uri != uri);
                self.ui.send(Event::PlaylistRemoved { uri });
                self.ui.send(Event::Status("Removed from your library".into()));
                self.request_library(true);
            }
            BrowseDone::TopSong { uri, track } => {
                self.top_song = Some((uri, track));
                // Sent again with the song; no new lookup if another song is the most played now.
                let stats = self.play_stats();
                self.ui.send(Event::PlayStats(stats));
            }
        }
    }
}

/// "Removed 1 duplicate", "Removed 3 duplicates".
fn removed_duplicates(n: usize) -> String {
    if n == 1 { "Removed 1 duplicate".to_owned() } else { format!("Removed {n} duplicates") }
}

/// Whether a list with the songs `have` holds every one of `wanted` (none wanted: no).
pub(super) fn holds_all<'a>(wanted: &[String], have: impl Iterator<Item = &'a str>) -> bool {
    if wanted.is_empty() {
        return false;
    }
    let mut left: std::collections::HashSet<&str> = wanted.iter().map(String::as_str).collect();
    for uri in have {
        left.remove(uri);
        if left.is_empty() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::mpsc::Receiver;

    use super::*;
    use crate::config::{Paths, Settings};
    use crate::controller::UiLink;

    /// A controller with no session writing into a temp folder, and what it sends the UI.
    fn controller(name: &str) -> (Controller, Receiver<Event>) {
        let dir = std::env::temp_dir().join(format!("truffle-browse-{name}-{}", std::process::id()));
        let paths = Paths { config: dir.join("config"), cache: dir.join("cache") };
        let (events, rx) = std::sync::mpsc::channel();
        let ui = UiLink { events, ctx: egui::Context::default() };
        let (jobs, _) = tokio::sync::mpsc::unbounded_channel();
        (Controller::new(paths, Arc::new(Mutex::new(Settings::default())), ui, jobs), rx)
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
            cover_small: Some(format!("https://i.scdn.co/image/{id}")),
            cover_large: None,
            duration_ms: 200_000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    /// A liked song by `artist`.
    fn by(id: &str, artist: &str) -> TrackInfo {
        TrackInfo { artists: vec![crate::model::ArtistRef { name: artist.into(), uri: format!("spotify:artist:{artist}") }], ..track(id) }
    }

    /// The songs of the list the UI was sent next (skipping the name and picture).
    fn next_list(events: &Receiver<Event>) -> Option<Vec<String>> {
        loop {
            match events.try_recv() {
                Ok(Event::SourceInfo(_)) => continue,
                Ok(Event::SourceLoaded { tracks, .. }) => return Some(tracks.iter().map(|t| t.name.clone()).collect()),
                _ => return None,
            }
        }
    }

    #[tokio::test]
    async fn an_artists_likes_open_at_once_from_the_liked_songs_truffle_has() {
        // Reading Liked Songs from Spotify again can take minutes (or time out); the page must
        // not wait for it. This session never connects, so nothing comes from Spotify.
        let (mut c, events) = controller("artist-likes");
        let session = librespot_core::Session::new(librespot_core::SessionConfig::default(), None);
        let library = Library::new(session, c.paths.clone());
        library.keep_in_memory(&Source::LikedSongs, vec![by("1", "a"), by("2", "b"), by("3", "a"), by("4", "a")]);
        c.library = Some(library);
        let page = Source::Mix(format!("{}spotify:artist:a", crate::library::MIX_ARTIST));
        c.cmd_open_source(page.clone(), false);
        assert_eq!(next_list(&events), Some(vec!["1".into(), "3".into(), "4".into()]), "shown at once, before Spotify answers");
        // Liked since (5, its details known from another list) and unliked since (3): the page
        // matches the "Your likes" count, which counts the likes as they are now.
        c.library.as_ref().unwrap().keep_in_memory(&Source::Album("spotify:album:5".into()), vec![by("5", "a")]);
        c.liked = ["1", "2", "4", "5"].iter().map(|i| format!("spotify:track:{i}")).collect();
        c.cmd_open_source(page, false);
        assert_eq!(next_list(&events), Some(vec!["5".into(), "1".into(), "4".into()]));
        // A mix not made from Liked Songs still loads the usual way.
        c.cmd_open_source(Source::Mix("listen".into()), false);
        assert!(matches!(events.try_recv(), Ok(Event::SourceLoading { .. })));
        done(c);
    }

    /// The most played song in the next "Your Truffle" sent, with its details if they came along.
    fn next_top(events: &Receiver<Event>) -> (String, Option<String>) {
        match events.try_recv() {
            Ok(Event::PlayStats(stats)) => {
                let top = stats.top.expect("a most played song");
                (top.uri, top.track.map(|t| t.name))
            }
            _ => panic!("no PlayStats sent"),
        }
    }

    #[test]
    fn songs_already_there_are_asked_about_and_a_failed_check_adds_nothing() {
        let (mut c, events) = controller("already");
        while events.try_recv().is_ok() {}
        let have = ["spotify:track:a".to_owned()].into_iter().collect();
        let already = AlreadyIn::find("spotify:playlist:p", &["spotify:track:a".to_owned()], &have, &[]).expect("a is there");
        // Some of them there: the UI asks.
        c.on_browse_done(Ok(BrowseDone::Already(already.clone())));
        assert!(matches!(events.try_recv(), Ok(Event::AlreadyInPlaylist(asked)) if asked == already));
        // The check failed: one plain error, and the playlists aren't read again (nothing changed).
        let error = anyhow::anyhow!("offline");
        c.on_browse_done(Ok(BrowseDone::Unchecked { playlist_uri: "spotify:playlist:p".into(), error, question: None }));
        assert!(matches!(events.try_recv(), Ok(Event::Error(text)) if text == "Couldn't check the playlist. Try again."));
        assert!(events.try_recv().is_err());
        // An answer's check failed: the same error, and its question comes back to try again from.
        let error = anyhow::anyhow!("offline");
        c.on_browse_done(Ok(BrowseDone::Unchecked { playlist_uri: "spotify:playlist:p".into(), error, question: Some(already.clone()) }));
        assert!(matches!(events.try_recv(), Ok(Event::Error(text)) if text == "Couldn't check the playlist. Try again."));
        assert!(matches!(events.try_recv(), Ok(Event::AlreadyInPlaylist(asked)) if asked == already));
        assert!(events.try_recv().is_err());
        // A check from before a logout is dropped.
        let old = c.epoch;
        c.epoch += 1;
        c.handle_job(Job::Browse { epoch: old, result: Ok(BrowseDone::Already(already)) });
        assert!(events.try_recv().is_err());
        done(c);
    }

    #[test]
    fn the_most_played_song_is_described_once() {
        let (mut c, events) = controller("top");
        while events.try_recv().is_ok() {}
        c.history.play_counts.insert("spotify:track:b".into(), 52);
        c.history.play_counts.insert("spotify:track:a".into(), 3);
        // In no list loaded: the numbers go without the song.
        c.cmd_play_stats();
        assert_eq!(next_top(&events), ("spotify:track:b".into(), None));
        // Spotify's answer goes at once, and is kept: the next time it comes with the numbers.
        c.on_browse_done(Ok(BrowseDone::TopSong { uri: "spotify:track:b".into(), track: track("b") }));
        assert_eq!(next_top(&events), ("spotify:track:b".into(), Some("b".into())));
        c.cmd_play_stats();
        assert_eq!(next_top(&events), ("spotify:track:b".into(), Some("b".into())));
        // Another song played more by now: the old answer isn't put on it.
        c.history.play_counts.insert("spotify:track:a".into(), 60);
        c.cmd_play_stats();
        assert_eq!(next_top(&events), ("spotify:track:a".into(), None));
        assert!(events.try_recv().is_err());
        done(c);
    }

    /// The next note sent, and whether it is an error.
    fn next_note(events: &Receiver<Event>) -> (String, bool) {
        match events.try_recv() {
            Ok(Event::Status(text)) => (text, false),
            Ok(Event::Error(text)) => (text, true),
            _ => panic!("no note sent"),
        }
    }

    #[test]
    fn removed_duplicates_are_counted() {
        assert_eq!(removed_duplicates(1), "Removed 1 duplicate");
        assert_eq!(removed_duplicates(3), "Removed 3 duplicates");
        let (mut c, events) = controller("dupes");
        while events.try_recv().is_ok() {}
        let removed = |asked, removed, error: Option<&str>| {
            let error = error.map(|e| anyhow::anyhow!("{e}"));
            Ok(BrowseDone::Duplicates { playlist_uri: "spotify:playlist:p".into(), asked, removed, error })
        };
        c.on_browse_done(removed(2, 2, None));
        assert_eq!(next_note(&events), ("Removed 2 duplicates".into(), false));
        // Stopped part way: how many went, and why the rest didn't.
        c.on_browse_done(removed(3, 1, Some("The playlist changed. Try again.")));
        assert_eq!(next_note(&events), ("Removed 1 of 3 duplicates. The playlist changed. Try again.".into(), true));
        c.on_browse_done(removed(3, 0, Some("The playlist changed. Try again.")));
        assert_eq!(next_note(&events), ("The playlist changed. Try again.".into(), true));
        assert!(events.try_recv().is_err());
        done(c);
    }

    #[test]
    fn a_playlist_that_cant_be_read_closes_the_box() {
        let (mut c, events) = controller("find");
        while events.try_recv().is_ok() {}
        c.cmd_find_duplicates("spotify:playlist:p".into());
        assert_eq!(next_note(&events), ("Log in to Spotify first.".into(), true));
        assert!(matches!(events.try_recv(), Ok(Event::Duplicates { tracks: None, .. })));
        done(c);
    }
}

#[cfg(test)]
mod held_tests {
    use super::holds_all;

    #[test]
    fn a_list_holds_the_songs_when_every_one_is_in_it() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let list = ["spotify:track:a", "spotify:track:b", "spotify:track:c"];
        assert!(holds_all(&s(&["spotify:track:b"]), list.iter().copied()));
        assert!(holds_all(&s(&["spotify:track:c", "spotify:track:a"]), list.iter().copied()));
        assert!(!holds_all(&s(&["spotify:track:a", "spotify:track:x"]), list.iter().copied()));
        assert!(!holds_all(&s(&[]), list.iter().copied()));
        assert!(!holds_all(&s(&["spotify:track:a"]), std::iter::empty()));
    }
}
