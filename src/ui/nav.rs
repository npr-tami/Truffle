//! Back and forward between pages, as in a browser: every page the middle column showed is
//! remembered (Home, a list, Albums, Artists, Profile, Search), and the title bar's arrows, Alt+Left
//! and Alt+Right and the mouse's side buttons go back and forth through them.

use super::search::source_of;
use super::{MiddleTab, TruffleApp};
use crate::model::Source;

/// How many pages back are kept.
const KEEP: usize = 50;

/// A page of the middle column.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Place {
    Home,
    List(Source),
    Albums,
    Artists,
    /// One's own profile (`None`) or someone else's.
    Profile(Option<String>),
    /// Whose followers (true) or following.
    People(String, bool),
    Search,
    Library,
    Playing,
}

/// The pages behind and ahead, and the page shown last frame.
#[derive(Default)]
pub(super) struct Nav {
    back: Vec<Place>,
    ahead: Vec<Place>,
    last: Option<Place>,
}

impl TruffleApp {
    /// The page shown now.
    pub(super) fn place(&self) -> Place {
        match self.middle_tab {
            MiddleTab::Home => Place::Home,
            MiddleTab::Albums => Place::Albums,
            MiddleTab::Artists => Place::Artists,
            MiddleTab::Profile => Place::Profile(self.user_view.clone()),
            MiddleTab::People => match &self.people_view {
                Some((user, followers)) => Place::People(user.clone(), *followers),
                None => Place::Home,
            },
            MiddleTab::Search => Place::Search,
            MiddleTab::Library => Place::Library,
            MiddleTab::Playing => Place::Playing,
            MiddleTab::Songs | MiddleTab::Blocked => match self.view.as_ref() {
                Some(v) => Place::List(v.source.clone()),
                None => Place::Home,
            },
        }
    }

    /// Runs every frame: a new page puts the last one behind (and forgets the ones ahead).
    pub(super) fn track_place(&mut self) {
        let now = self.place();
        match self.nav.last.take() {
            Some(last) if last != now => {
                self.nav.back.push(last);
                if self.nav.back.len() > KEEP {
                    self.nav.back.remove(0);
                }
                self.nav.ahead.clear();
            }
            _ => {}
        }
        self.nav.last = Some(now);
    }

    /// The name of the page Back goes to ("Library" when there is none behind).
    pub(super) fn back_label(&self) -> &'static str {
        match self.nav.back.last() {
            Some(Place::Home) => "Home",
            Some(Place::Search) => "Search",
            Some(Place::Albums) => "Albums",
            Some(Place::Artists) => "Artists",
            Some(Place::Profile(_)) => "Profile",
            Some(Place::People(_, true)) => "Followers",
            Some(Place::People(_, false)) => "Following",
            Some(Place::Playing) => "Playing",
            Some(Place::List(_)) => "Back",
            Some(Place::Library) | None => "Library",
        }
    }

    pub(super) fn can_go_back(&self) -> bool {
        !self.nav.back.is_empty()
    }

    pub(super) fn can_go_forward(&self) -> bool {
        !self.nav.ahead.is_empty()
    }

    pub(super) fn go_back(&mut self) {
        let Some(to) = self.nav.back.pop() else { return };
        let here = self.place();
        self.nav.ahead.push(here);
        self.go_to(to);
    }

    pub(super) fn go_forward(&mut self) {
        let Some(to) = self.nav.ahead.pop() else { return };
        let here = self.place();
        self.nav.back.push(here);
        self.go_to(to);
    }

    /// Shows `to` without counting it as a new page.
    fn go_to(&mut self, to: Place) {
        self.show_place(&to);
        self.nav.last = Some(to);
    }

    /// Shows `to` as a click on it would.
    pub(super) fn show_place(&mut self, to: &Place) {
        match to {
            Place::Home => self.show_home(),
            Place::List(source) => {
                let source = source.clone();
                self.open_source(source, false);
            }
            Place::Albums => self.show_collection(MiddleTab::Albums),
            Place::Artists => self.show_collection(MiddleTab::Artists),
            Place::Profile(None) => self.show_profile(),
            Place::Profile(Some(user)) => {
                let user = user.clone();
                self.show_user(&user);
            }
            Place::People(user, followers) => {
                let (user, followers) = (user.clone(), *followers);
                self.show_people(&user, followers);
            }
            Place::Search => self.middle_tab = MiddleTab::Search,
            Place::Library => self.middle_tab = MiddleTab::Library,
            Place::Playing => self.middle_tab = MiddleTab::Playing,
        }
    }
}

/// The page a self-test's "open <page>" asks for: "home", "profile", "liked", "likes <artist URI>"
/// (Your likes of that artist) or a playlist's, album's or artist's Spotify URI.
pub(super) fn place_of(page: &str) -> Option<Place> {
    let words: Vec<&str> = page.split_whitespace().collect();
    let is = |word: &str, name: &str| word.eq_ignore_ascii_case(name);
    let of = |uri: &str, kinds: &[&str]| kinds.iter().any(|kind| uri.strip_prefix(kind).is_some_and(|id| !id.is_empty() && !id.contains(':')));
    match words.as_slice() {
        [w] if is(w, "home") => Some(Place::Home),
        [w] if is(w, "profile") => Some(Place::Profile(None)),
        [w] if is(w, "liked") => Some(Place::List(Source::LikedSongs)),
        [w, artist] if is(w, "likes") && of(artist, &["spotify:artist:"]) => Some(Place::List(Source::Mix(format!("{}{artist}", crate::library::MIX_ARTIST)))),
        [uri] if of(uri, &["spotify:playlist:", "spotify:album:", "spotify:artist:"]) => Some(Place::List(source_of(uri))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_compare_by_page() {
        assert_eq!(Place::List(Source::LikedSongs), Place::List(Source::LikedSongs));
        assert_ne!(Place::Albums, Place::Artists);
    }

    #[test]
    fn a_self_test_opens_the_page_it_names() {
        assert_eq!(place_of("home"), Some(Place::Home));
        assert_eq!(place_of("  Profile "), Some(Place::Profile(None)));
        assert_eq!(place_of("LIKED"), Some(Place::List(Source::LikedSongs)));
        let artist = "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF";
        assert_eq!(place_of(&format!("likes   {artist}")), Some(Place::List(Source::Mix(format!("{}{artist}", crate::library::MIX_ARTIST)))));
        assert_eq!(place_of(artist), Some(Place::List(Source::Artist(artist.to_owned()))));
        assert_eq!(place_of("spotify:album:4aawyAB9vmqN3uQ7FjRGTy"), Some(Place::List(Source::Album("spotify:album:4aawyAB9vmqN3uQ7FjRGTy".into()))));
        assert_eq!(place_of("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"), Some(Place::List(Source::Playlist("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into()))));
    }

    #[test]
    fn a_self_test_opens_nothing_it_cannot_name() {
        for page in [
            "",
            "   ",
            "homes",
            "home now",
            "likes",
            "likes spotify:album:4aawyAB9vmqN3uQ7FjRGTy",
            "likes spotify:artist:",
            "spotify:artist:",
            "spotify:track:6rqhFgbbKwnb9MLmUQDhG6",
            "spotify:user:wisp",
            "spotify:artist:a:b",
            "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
            "liked 🎧",
            "Дворы",
        ] {
            assert_eq!(place_of(page), None, "{page:?}");
        }
    }
}
