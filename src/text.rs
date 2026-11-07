//! Text compared the way people read it: folded for filters (case and accents aside), and songs
//! matched by name and artists, the same way wherever Truffle looks for a song it has already
//! ("Find duplicates", and adding songs to a playlist).

mod fold;

use std::collections::HashMap;

use crate::model::{ArtistRef, TrackInfo};

pub use fold::{contains_folded, find_folded, find_folded_name, fold, fold_name, push_fold};

/// What a song is matched by before its artists are.
#[derive(PartialEq, Eq, Hash)]
pub enum SongKey {
    /// Its name, folded, its spaces trimmed and runs of them made one.
    Name(String),
    /// Its link: a song without artists (Spotify no longer has it) is only the same as itself.
    Link(String),
}

/// What `t` is matched by: the same name (case, accents and spaces aside; "Remastered" or "Live"
/// in a name makes another song), then the same artists ([`same_artists`]). Local files are never
/// matched: `None`.
pub fn song_key(t: &TrackInfo) -> Option<SongKey> {
    if t.is_local {
        None
    } else if t.artists.is_empty() {
        Some(SongKey::Link(t.uri.clone()))
    } else {
        Some(SongKey::Name(song_name(&t.name)))
    }
}

/// A name as songs are matched by: folded, its spaces trimmed and runs of them made one.
fn song_name(name: &str) -> String {
    fold(&name.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The same artists, in any order: by link when both have one, else by name.
pub fn same_artists(a: &[ArtistRef], b: &[ArtistRef]) -> bool {
    let same = |x: &ArtistRef, y: &ArtistRef| {
        if x.uri.is_empty() || y.uri.is_empty() { song_name(&x.name) == song_name(&y.name) } else { x.uri == y.uri }
    };
    a.iter().all(|x| b.iter().any(|y| same(x, y))) && b.iter().all(|y| a.iter().any(|x| same(x, y)))
}

/// The links of the songs in `asked` that `list` has, by its link or as another version of it (the
/// single and the album version): the same [`song_key`] and [`same_artists`].
pub fn same_songs<'a>(list: &[TrackInfo], asked: &'a [TrackInfo]) -> Vec<&'a str> {
    // By name first; the few songs of the same name are told apart by their artists.
    let mut by_name: HashMap<SongKey, Vec<&[ArtistRef]>> = HashMap::new();
    for t in list {
        if let Some(key) = song_key(t) {
            by_name.entry(key).or_default().push(&t.artists);
        }
    }
    asked
        .iter()
        .filter(|t| song_key(t).and_then(|k| by_name.get(&k)).is_some_and(|named| named.iter().any(|a| same_artists(a, &t.artists))))
        .map(|t| t.uri.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artist(name: &str, uri: &str) -> ArtistRef {
        ArtistRef { name: name.into(), uri: uri.into() }
    }

    /// A song `id` called `name` by `artists`, on the album `album`.
    fn song(id: &str, name: &str, artists: &[ArtistRef], album: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{id}"),
            name: name.into(),
            artists: artists.to_vec(),
            album: album.into(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 200_000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    #[test]
    fn the_single_is_there_when_the_album_version_is() {
        let wanted = [artist("The Wanted", "spotify:artist:tw")];
        let list = [song("album", "Glad You Came", &wanted, "Battleground"), song("other", "Chasing the Sun", &wanted, "Battleground")];
        let asked = [song("single", "Glad You Came", &wanted, "Glad You Came"), song("new", "Lightning", &wanted, "Battleground")];
        assert_eq!(same_songs(&list, &asked), ["spotify:track:single"]);
        // And its own link, of course.
        assert_eq!(same_songs(&list, &list[..1]), ["spotify:track:album"]);
    }

    #[test]
    fn case_accents_spaces_and_the_artists_order_do_not_matter() {
        let (a, b) = (artist("Calvin Harris", "spotify:artist:ch"), artist("Dua Lipa", "spotify:artist:dl"));
        let crue = [artist("Mötley Crüe", "spotify:artist:mc")];
        let list = [song("1", "One Kiss", &[a.clone(), b.clone()], "One Kiss"), song("2", "Dr. Feelgood", &crue, "Dr. Feelgood")];
        let asked = [
            song("x", "ONE  kiss ", &[b.clone(), a.clone()], "Hits"),
            song("y", "dr. féelgood", &[artist("motley crue", "")], "Live"),
            // Only one of the artists: another song.
            song("z", "One Kiss", std::slice::from_ref(&a), "Hits"),
        ];
        assert_eq!(same_songs(&list, &asked), ["spotify:track:x", "spotify:track:y"]);
    }

    #[test]
    fn remastered_and_live_are_other_songs() {
        let queen = [artist("Queen", "spotify:artist:q")];
        let list = [song("a", "Bohemian Rhapsody", &queen, "A Night at the Opera")];
        let asked = [song("b", "Bohemian Rhapsody - Remastered 2011", &queen, "Greatest Hits"), song("c", "Bohemian Rhapsody - Live", &queen, "Live Killers")];
        assert!(same_songs(&list, &asked).is_empty());
    }

    #[test]
    fn songs_without_details_only_match_their_own_link_and_local_files_none() {
        let queen = [artist("Queen", "spotify:artist:q")];
        // What a song Spotify couldn't describe comes back as: no artists.
        let unknown = |id: &str| TrackInfo { playable: false, ..song(id, "Unavailable track", &[], "") };
        let mut local = song("x", "Bohemian Rhapsody", &queen, "");
        local.uri = "spotify:local:Queen:A+Night:Bohemian+Rhapsody:354".into();
        local.is_local = true;
        let list = [unknown("gone"), local.clone(), song("a", "Bohemian Rhapsody", &queen, "")];
        assert_eq!(same_songs(&list, &[unknown("other"), unknown("gone")]), ["spotify:track:gone"]);
        assert!(same_songs(&list[..2], &[local, song("b", "Bohemian Rhapsody", &queen, "")]).is_empty());
    }
}
