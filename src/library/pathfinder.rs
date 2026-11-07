//! Search through Pathfinder, the web player's GraphQL service (the session's own login: no Web
//! API key): the persisted queries and their hashes, and the answers turned into Truffle's cards.
//! Hashes are retired every few months; the next one is tried then (see `fetch::pathfinder`).

use serde_json::{Value, json};

use crate::model::Card;
use crate::text::{fold, fold_name};

/// A persisted query: its name and its hashes, newest first.
pub struct Query {
    pub operation: &'static str,
    pub hashes: &'static [&'static str],
    /// Where its answer is under `data.searchV2`.
    pub section: &'static str,
}

pub const TRACKS: Query =
    Query { operation: "searchTracks", hashes: &["b02683192a98dde7966b5e6655a79eeb62713eab703eda9902c932818dd52751"], section: "tracksV2" };
pub const ALBUMS: Query =
    Query { operation: "searchAlbums", hashes: &["202cb3305e31e5a0767ba7925f28bd728cf8f8b0217e6da43909056071cd70e9"], section: "albumsV2" };
pub const ARTISTS: Query =
    Query { operation: "searchArtists", hashes: &["7bf95d754fdbe32c8b161fbbe54d1ae50974900df4dce4c8f1afcbcad153224d"], section: "artists" };
pub const PLAYLISTS: Query = Query {
    operation: "searchPlaylists",
    hashes: &["d520014e748f9ea44f7707d8df1819867ac1205e8b7f3e28f22fe5fc858921b1"],
    section: "playlists",
};
/// Everything at once: the fallback when a query of one kind is gone.
pub const DESKTOP: Query = Query {
    operation: "searchDesktop",
    hashes: &[
        "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03",
        "fcad5a3e0d5af727fb76966f06971c19cfa2275e6ff7671196753e008611873c",
        "3c9d3f60dac5dea3876b6db3f534192b1c1d90032c4233c1bbaba526db41eb31",
        "4801118d4a100f756e833d33984436a3899cff359c532f8fd3aaf174b60b3b49",
    ],
    section: "",
};

/// Following users: `isFollowingUsers` (variables `uris`), `followUsers` and `unfollowUsers`
/// (variables `usernames`) share one stored query.
pub const FOLLOW_USERS_HASH: &str = "c00e0cb6c7766e7230fc256cf4fe07aec63b53d1160a323940fce7b664e95596";

/// Whether a `followUsers` / `unfollowUsers` answer says it went through for `username`.
pub fn follow_users_ok(answer: &Value, operation: &str, username: &str) -> bool {
    answer["data"][operation]["responses"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r["result"].as_bool() == Some(true) && r["username"].as_str().is_none_or(|u| u.eq_ignore_ascii_case(username)))
}

/// Whether an `isFollowingUsers` answer says `uri` is followed (`None`: not in the answer).
pub fn is_following_user(answer: &Value, uri: &str) -> Option<bool> {
    answer["data"]["users"].as_array()?.iter().find(|u| u["uri"].as_str() == Some(uri)).map(|u| u["following"].as_bool().unwrap_or(false))
}

/// Results per kind.
pub const LIMIT: usize = 10;

/// Free text for the search: Web API style filters (`artist:"x"`) become plain words.
pub fn search_term(query: &str) -> String {
    let words: Vec<&str> = query
        .split_whitespace()
        .map(|w| ["track:", "artist:", "album:", "isrc:"].iter().find_map(|f| w.strip_prefix(f)).unwrap_or(w))
        .collect();
    words.join(" ").replace('"', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn variables(term: &str, offset: usize) -> Value {
    json!({
        "searchTerm": term,
        "offset": offset,
        "limit": LIMIT,
        "numberOfTopResults": 5,
        "includeAudiobooks": false,
        "includeArtistHasConcertsField": false,
        "includePreReleases": false,
        "includeLocalConcertsField": false,
        "includeAuthors": false,
    })
}

/// An item's data: tracks come as `items[].item.data`, the others as `items[].data`.
fn data(item: &Value) -> &Value {
    if item["item"]["data"].is_object() { &item["item"]["data"] } else { &item["data"] }
}

fn items(section: &Value) -> impl Iterator<Item = &Value> {
    section["items"].as_array().into_iter().flatten().map(data)
}

pub fn total(section: &Value) -> usize {
    section["totalCount"].as_u64().unwrap_or(0) as usize
}

/// Whether a section that came for the page at `offset` has more after it (Spotify pages no
/// further than 1000).
pub fn has_more(section: &Value, offset: usize) -> bool {
    let n = section["items"].as_array().map_or(0, Vec::len);
    n > 0 && offset + n < total(section).min(1000)
}

/// The songs' uris, in order.
pub fn track_uris(section: &Value) -> Vec<String> {
    items(section).filter_map(|d| d["uri"].as_str()).filter(|u| u.starts_with("spotify:track:")).map(str::to_owned).collect()
}

/// The picture closest to 160 px wide among `sources` (`[{url, width, height}]`).
fn picture(sources: &Value) -> Option<String> {
    let list = sources.as_array()?;
    list.iter()
        .filter(|s| s["url"].as_str().is_some_and(|u| !u.is_empty()))
        .min_by_key(|s| s["width"].as_i64().map_or(i64::MAX / 2, |w| (w - 160).abs()))
        .and_then(|s| s["url"].as_str())
        .map(str::to_owned)
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn card(uri: &Value, name: &Value, subtitle: String, image: Option<String>) -> Option<Card> {
    let (uri, name) = (text(uri), text(name));
    (!uri.is_empty() && !name.is_empty()).then_some(Card { uri, name, subtitle, image })
}

pub fn album_cards(section: &Value) -> Vec<Card> {
    items(section)
        .filter_map(|d| {
            let kind = match d["type"].as_str().unwrap_or_default() {
                "SINGLE" => "Single",
                "EP" => "EP",
                "COMPILATION" => "Compilation",
                _ => "Album",
            };
            let artist = text(&d["artists"]["items"][0]["profile"]["name"]);
            let subtitle = if artist.is_empty() { kind.to_owned() } else { format!("{kind} · {artist}") };
            card(&d["uri"], &d["name"], subtitle, picture(&d["coverArt"]["sources"]))
        })
        .collect()
}

pub fn artist_cards(section: &Value) -> Vec<Card> {
    items(section)
        .filter_map(|d| card(&d["uri"], &d["profile"]["name"], "Artist".to_owned(), picture(&d["visuals"]["avatarImage"]["sources"])))
        .collect()
}

pub fn playlist_cards(section: &Value) -> Vec<Card> {
    items(section)
        .filter_map(|d| {
            let owner = text(&d["ownerV2"]["data"]["name"]);
            let subtitle = if owner.is_empty() { "Playlist".to_owned() } else { format!("Playlist · {owner}") };
            card(&d["uri"], &d["name"], subtitle, picture(&d["images"]["items"][0]["sources"]))
        })
        .collect()
}

/// The people (Spotify users) of the everything-search's `users` section: their name (else their
/// id), their id as the subtitle, and their picture. Items that aren't a "User", or have no user
/// link, are skipped.
pub fn user_cards(section: &Value) -> Vec<Card> {
    items(section)
        .filter(|d| d["__typename"].as_str().is_none_or(|t| t == "User"))
        .filter_map(|d| {
            let uri = text(&d["uri"]);
            let from_uri = uri.strip_prefix("spotify:user:").filter(|id| !id.is_empty())?.to_owned();
            let id = [&d["username"], &d["id"]].into_iter().map(|v| text(v).trim().to_owned()).find(|id| !id.is_empty()).unwrap_or(from_uri);
            let name = text(&d["displayName"]).trim().to_owned();
            let name = if name.is_empty() { id.clone() } else { name };
            Some(Card { uri, name, subtitle: id, image: picture(&d["avatar"]["sources"]) })
        })
        .collect()
}

/// What a people search asks for: what was typed and, when folding changes more than its case
/// ("𝓮mma" or "émma" is "emma"), the folded form too.
pub fn user_terms(term: &str) -> Vec<String> {
    let folded = fold(term).split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() || folded == term.to_lowercase() { vec![term.to_owned()] } else { vec![term.to_owned(), folded] }
}

/// The people of the answers for each of [`user_terms`], each once and in the order they came,
/// then those whose name or id holds `term` moved first (both folded as names are: "𝓔𝓶𝓶𝓪" and
/// "Еmmа" with Cyrillic letters hold "emma").
pub fn merge_users(answers: impl IntoIterator<Item = Vec<Card>>, term: &str) -> Vec<Card> {
    let q = fold_name(term);
    let mut all: Vec<Card> = Vec::new();
    for c in answers.into_iter().flatten() {
        if !all.iter().any(|k| k.uri == c.uri) {
            all.push(c);
        }
    }
    all.sort_by_cached_key(|c| !(fold_name(&c.name).contains(&q) || fold_name(&c.subtitle).contains(&q)));
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_answers_are_read() {
        let done = json!({ "data": { "followUsers": { "responses": [{ "__typename": "FollowUserResult", "username": "bob", "result": true }] } } });
        assert!(follow_users_ok(&done, "followUsers", "bob"));
        assert!(!follow_users_ok(&done, "unfollowUsers", "bob"));
        let failed = json!({ "data": { "followUsers": { "responses": [{ "__typename": "GenericError" }] } } });
        assert!(!follow_users_ok(&failed, "followUsers", "bob"));
        let state = json!({ "data": { "users": [{ "uri": "spotify:user:bob", "following": true }, { "__typename": "NotFound", "uri": "spotify:user:x" }] } });
        assert_eq!(is_following_user(&state, "spotify:user:bob"), Some(true));
        assert_eq!(is_following_user(&state, "spotify:user:x"), Some(false));
        assert_eq!(is_following_user(&state, "spotify:user:none"), None);
    }

    #[test]
    fn filters_become_plain_words() {
        assert_eq!(search_term(r#"track:"Get Lucky" artist:"Daft Punk""#), "Get Lucky Daft Punk");
        assert_eq!(search_term("  bonobo  "), "bonobo");
    }

    #[test]
    fn answers_become_cards_and_songs() {
        let tracks = json!({ "totalCount": 31, "items": [
            { "item": { "data": { "uri": "spotify:track:1", "name": "A" } } },
            { "item": { "data": { "uri": "spotify:episode:2" } } },
            { "data": { "uri": "spotify:track:3" } },
        ]});
        assert_eq!(track_uris(&tracks), ["spotify:track:1", "spotify:track:3"]);
        assert_eq!(total(&tracks), 31);

        let albums = json!({ "items": [
            { "data": { "uri": "spotify:album:a", "name": "Discovery", "type": "ALBUM",
                "artists": { "items": [{ "profile": { "name": "Daft Punk" } }] },
                "coverArt": { "sources": [
                    { "url": "https://i/640", "width": 640 }, { "url": "https://i/300", "width": 300 }, { "url": "https://i/64", "width": 64 } ] } } },
            { "data": { "uri": "spotify:album:b", "name": "", "type": "SINGLE" } },
        ]});
        let cards = album_cards(&albums);
        assert_eq!(cards.len(), 1);
        assert_eq!((cards[0].subtitle.as_str(), cards[0].image.as_deref()), ("Album · Daft Punk", Some("https://i/64")));

        let artists = json!({ "items": [{ "data": { "uri": "spotify:artist:x", "profile": { "name": "Bonobo" },
            "visuals": { "avatarImage": { "sources": [{ "url": "https://i/a", "width": null }] } } } }] });
        let a = artist_cards(&artists);
        assert_eq!((a[0].name.as_str(), a[0].image.as_deref()), ("Bonobo", Some("https://i/a")));

        let playlists = json!({ "items": [{ "data": { "uri": "spotify:playlist:p", "name": "Best Of Bonobo",
            "ownerV2": { "data": { "name": "Bonobo" } }, "images": { "items": [{ "sources": [{ "url": "https://i/p" }] }] } } }] });
        let p = playlist_cards(&playlists);
        assert_eq!((p[0].subtitle.as_str(), p[0].image.as_deref()), ("Playlist · Bonobo", Some("https://i/p")));
        // A missing section is just empty.
        assert!(album_cards(&Value::Null).is_empty() && track_uris(&Value::Null).is_empty());
    }

    /// A person as the everything-search sends one.
    fn user(id: &str, name: &str) -> Value {
        json!({ "data": { "__typename": "User", "uri": format!("spotify:user:{id}"), "id": id, "username": id, "displayName": name,
            "avatar": { "sources": [{ "url": format!("https://i/{id}/640"), "width": 640 }, { "url": format!("https://i/{id}/64"), "width": 64 }] } } })
    }

    fn card(id: &str, name: &str) -> Card {
        Card { uri: format!("spotify:user:{id}"), name: name.into(), subtitle: id.into(), image: None }
    }

    fn names(list: &[Card]) -> Vec<&str> {
        list.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn people_are_read_from_the_users_section() {
        let long = "𝓛𝓸𝓷𝓰 ".repeat(400);
        let section = json!({ "totalCount": 31, "items": [
            user("mira.l", "  Mira L "),
            // No name: the id stands in (from the link when nothing else has it); no picture.
            { "data": { "__typename": "User", "uri": "spotify:user:31xyz" } },
            { "data": { "__typename": "User", "uri": "spotify:user:a", "displayName": "   ", "avatar": { "sources": [] } } },
            // No type said: still a person. Emoji and very long names stay whole.
            { "data": { "uri": "spotify:user:b", "displayName": "🔥🔥" } },
            { "data": { "__typename": "User", "uri": "spotify:user:c", "displayName": long } },
            // Not a person, or not one Truffle can open: skipped.
            { "data": { "__typename": "NotFound", "uri": "spotify:user:gone" } },
            { "data": { "__typename": "User", "uri": "spotify:artist:x", "displayName": "An artist" } },
            { "data": { "__typename": "User", "uri": "spotify:user:", "displayName": "Nobody" } },
            { "data": { "__typename": "User", "displayName": "No link" } },
            { "data": null }, { "data": { "uri": 7 } }, 7, "x", {}, null,
        ]});
        let people = user_cards(&section);
        let got: Vec<(&str, &str, &str)> = people.iter().map(|c| (c.uri.as_str(), c.name.as_str(), c.subtitle.as_str())).collect();
        assert_eq!(
            got,
            [
                ("spotify:user:mira.l", "Mira L", "mira.l"),
                ("spotify:user:31xyz", "31xyz", "31xyz"),
                ("spotify:user:a", "a", "a"),
                ("spotify:user:b", "🔥🔥", "b"),
                ("spotify:user:c", long.trim(), "c"),
            ]
        );
        assert_eq!(people[0].image.as_deref(), Some("https://i/mira.l/64"));
        assert!(people[1..].iter().all(|c| c.image.is_none()));
        // No users section, or one that isn't a list: nobody, and no more.
        for nothing in [Value::Null, json!({ "items": {} }), json!({ "totalCount": 3 }), json!([])] {
            assert!(user_cards(&nothing).is_empty() && !has_more(&nothing, 0), "{nothing}");
        }
    }

    #[test]
    fn more_pages_while_spotify_has_more() {
        let page = |n: usize, total: usize| json!({ "totalCount": total, "items": vec![json!({}); n] });
        assert!(has_more(&page(10, 31), 0));
        assert!(has_more(&page(10, 31), 20));
        assert!(!has_more(&page(1, 31), 30));
        assert!(!has_more(&page(0, 31), 10));
        assert!(!has_more(&page(10, 10), 0));
        // Spotify pages no further than 1000.
        assert!(!has_more(&page(10, 5000), 990));
    }

    #[test]
    fn fancy_or_accented_typing_also_asks_for_plain_letters() {
        for same in ["emma", "Emma", "EMMA STONE", "宇多田", "Кино", "🔥", "a b"] {
            assert_eq!(user_terms(same), [same], "{same}");
        }
        assert_eq!(user_terms("émma"), ["émma", "emma"]);
        assert_eq!(user_terms("𝓮mma"), ["𝓮mma", "emma"]);
        assert_eq!(user_terms("Ｅｍｍａ Ｓｔｏｎｅ"), ["Ｅｍｍａ Ｓｔｏｎｅ", "emma stone"]);
        assert_eq!(user_terms("E\u{301}mma \u{336} x"), ["E\u{301}mma \u{336} x", "emma x"]);
        // Only marks: nothing plain to ask for.
        assert_eq!(user_terms("\u{301}\u{336}"), ["\u{301}\u{336}"]);
    }

    #[test]
    fn people_come_once_and_name_matches_first() {
        let typed = vec![card("emma1", "Emma"), card("bob", "Bob"), card("fan", "emma_fan")];
        let folded = vec![card("fan", "emma_fan"), card("fancy", "𝓔𝓶𝓶𝓪"), card("cyr", "\u{415}mm\u{430}"), card("skye", "Skye"), card("emma99", "Skye 2")];
        let merged = merge_users([typed.clone(), folded.clone()], "𝓮mma");
        // Bob and Skye hold no "emma" (Skye 2's id does); each one once.
        assert_eq!(names(&merged), ["Emma", "emma_fan", "𝓔𝓶𝓶𝓪", "\u{415}mm\u{430}", "Skye 2", "Bob", "Skye"]);
        // Plain typing ranks fancy names alike.
        assert_eq!(names(&merge_users([folded], "EMMA")), ["emma_fan", "𝓔𝓶𝓶𝓪", "\u{415}mm\u{430}", "Skye 2", "Skye"]);
        // No answers, empty ones, or one page only: as it came.
        assert!(merge_users(Vec::<Vec<Card>>::new(), "emma").is_empty());
        assert!(merge_users([Vec::new(), Vec::new()], "emma").is_empty());
        assert_eq!(names(&merge_users([typed], "")), ["Emma", "Bob", "emma_fan"]);
        let odd = vec![card("x", ""), card("rtl", "מרים"), card("long", &"x".repeat(5000)), card("e", "Emma")];
        assert_eq!(names(&merge_users([odd], "emma"))[0], "Emma");
    }
}
