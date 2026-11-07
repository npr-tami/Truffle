//! Search (the Search tab of the middle column), album / artist cards, the "Add to playlist" menu
//! and playing a song from search by opening its album.

use std::sync::Arc;


use super::{MiddleTab, TruffleApp};
use crate::model::{Card, Command, SearchResults, Source, TrackInfo};

pub(super) const SEARCH_BOX_ID: &str = "spotify_search";
/// What was searched for and what came back.
#[derive(Default)]
pub(super) struct SearchState {
    /// The text in the search box.
    pub input: String,
    /// The query the results (or the pending request) belong to.
    pub sent: String,
    pub results: Option<Arc<SearchResults>>,
    /// Songs of every page loaded so far.
    pub tracks: Vec<TrackInfo>,
    pub loading: bool,
    pub error: Option<String>,
    /// The chip chosen (0 All, 1 Songs, 2 Albums, 3 Artists, 4 Playlists, 5 Users).
    pub chip: u8,
    /// People found for the Users chip.
    pub users: Users,
    /// When the box last changed: the search goes out once typing pauses.
    pub typed_at: Option<std::time::Instant>,
    /// Enter was pressed before the results came: play the top one when they do.
    pub play_top: bool,
    /// The page the search started from (Esc in an empty box goes back).
    pub back: Option<MiddleTab>,
}

impl SearchState {
    /// Takes in a page of results for the current query.
    pub fn receive(&mut self, results: Arc<SearchResults>) {
        if results.query != self.sent {
            return;
        }
        if results.offset == 0 {
            self.tracks = results.tracks.clone();
            self.results = Some(results);
        } else {
            self.tracks.extend(results.tracks.iter().cloned());
            if let Some(first) = self.results.as_mut() {
                Arc::make_mut(first).more = results.more;
            }
        }
        self.loading = false;
        self.error = None;
    }

    /// Takes in a page of people for the current query: each person once.
    pub fn receive_users(&mut self, results: &SearchResults) {
        if results.query != self.sent || results.query != self.users.query {
            return;
        }
        if results.offset == 0 {
            self.users.list.clear();
        }
        for u in &results.users {
            if !self.users.list.iter().any(|k| k.uri == u.uri) {
                self.users.list.push(u.clone());
            }
        }
        self.users.next = results.users_next;
        self.users.loading = false;
        self.users.error = None;
    }
}

/// People found (Spotify users), asked for once the Users chip shows.
#[derive(Default)]
pub(super) struct Users {
    /// The query they (or the page on its way) are for; empty: not asked.
    pub query: String,
    /// Every page loaded so far.
    pub list: Vec<Card>,
    /// Where the next page starts, when there is one.
    pub next: Option<u32>,
    pub loading: bool,
    pub error: Option<String>,
}

impl TruffleApp {

    pub(super) fn start_search(&mut self) {
        let query = self.search.input.trim().to_owned();
        if query.is_empty() {
            return;
        }
        self.middle_tab = MiddleTab::Search;
        if query == self.search.sent && self.search.results.is_some() {
            return;
        }
        let keep = SearchState { chip: self.search.chip, play_top: self.search.play_top, back: self.search.back, ..Default::default() };
        self.search = SearchState { input: self.search.input.clone(), sent: query.clone(), loading: true, ..keep };
        self.send(Command::Search { query, offset: 0 });
    }

    /// Plays a song that isn't in an open list by opening its album and starting there.
    pub(super) fn play_from_album(&mut self, t: &TrackInfo) {
        if !t.playable {
            self.toast("That song can't be played here.", true);
            return;
        }
        let source = Source::Album(t.album_uri.clone());
        if !t.album_uri.starts_with("spotify:album:") {
            self.toast("Couldn't find the album of that song.", true);
            return;
        }
        self.middle_tab = MiddleTab::Songs;
        self.pending_play = Some((source.clone(), t.uri.clone()));
        self.pending_scroll_uri = Some(t.uri.clone());
        let already = self.view.as_ref().is_some_and(|v| v.source == source && v.tracks.is_some());
        self.open_source(source, false);
        if already {
            self.start_pending_play();
        }
    }

    /// Starts the song waiting in `pending_play` once its list is on screen.
    pub(super) fn start_pending_play(&mut self) {
        let Some((source, uri)) = self.pending_play.clone() else { return };
        let Some(view) = self.view.as_ref().filter(|v| v.source == source) else { return };
        let Some(index) = view.tracks.as_ref().and_then(|t| t.iter().position(|t| t.uri == uri)) else { return };
        self.pending_play = None;
        let shuffle = self.current_shuffle();
        self.send(Command::PlaySource { source, start_index: Some(index), shuffle });
    }
}

/// The page a Spotify URI opens.
pub(super) fn source_of(uri: &str) -> Source {
    if uri.starts_with("spotify:artist:") {
        Source::Artist(uri.to_owned())
    } else if uri.starts_with("spotify:album:") {
        Source::Album(uri.to_owned())
    } else {
        Source::Playlist(uri.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of people for `query` from `offset`.
    fn page(query: &str, offset: u32, ids: &[&str], next: Option<u32>) -> SearchResults {
        let users = ids.iter().map(|&id| Card { uri: format!("spotify:user:{id}"), name: id.into(), subtitle: id.into(), image: None }).collect();
        SearchResults { query: query.into(), offset, users, users_next: next, ..Default::default() }
    }

    fn ids(s: &SearchState) -> Vec<&str> {
        s.users.list.iter().map(|c| c.subtitle.as_str()).collect()
    }

    #[test]
    fn pages_of_people_add_up_each_one_once() {
        let users = Users { query: "emma".into(), loading: true, error: Some("Couldn't search Spotify.".into()), ..Default::default() };
        let mut s = SearchState { sent: "emma".into(), users, ..Default::default() };
        // A page for what was typed before: left out.
        s.receive_users(&page("emm", 0, &["x"], None));
        assert!(s.users.list.is_empty() && s.users.loading);
        // Nobody found.
        s.receive_users(&page("emma", 0, &[], None));
        assert!(s.users.list.is_empty() && !s.users.loading && s.users.next.is_none() && s.users.error.is_none());
        // One, then a page that repeats them.
        s.receive_users(&page("emma", 0, &["a"], Some(10)));
        s.receive_users(&page("emma", 10, &["a", "b", "b", "c"], Some(20)));
        assert_eq!((ids(&s), s.users.next), (vec!["a", "b", "c"], Some(20)));
        s.receive_users(&page("emma", 20, &[], None));
        assert_eq!((ids(&s), s.users.next), (vec!["a", "b", "c"], None));
        // The first page again starts over; a search for something else is never taken in.
        s.receive_users(&page("emma", 0, &["z"], Some(10)));
        assert_eq!(ids(&s), ["z"]);
        s.sent = "bob".into();
        s.receive_users(&page("emma", 10, &["y"], None));
        assert_eq!(ids(&s), ["z"]);
    }
}
