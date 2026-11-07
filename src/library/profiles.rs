//! Profiles through the session, as Spotify's own apps show them (`/user-profile-view/v3`): a
//! user's name, picture, follower counts, public playlists and recently played artists, and the
//! people (and artists) they follow or who follow them. JSON, read defensively: a missing field is
//! just left out.

use serde_json::Value;

use crate::model::{Card, UserPage};

fn text(v: &Value) -> String {
    v.as_str().map(str::trim).unwrap_or_default().to_owned()
}

fn count(v: &Value) -> Option<u32> {
    v.as_u64().map(|n| n.min(u64::from(u32::MAX)) as u32)
}

fn image(v: &Value) -> Option<String> {
    let url = text(&v["image_url"]);
    if !url.is_empty() {
        return Some(url);
    }
    v["images"].as_array().and_then(|a| a.iter().find_map(|i| i["url"].as_str().filter(|u| !u.is_empty()))).map(str::to_owned)
}

/// The followers line under a person: "12 followers" (or "1 follower"); empty when unknown.
fn followers_text(v: &Value) -> String {
    match count(&v["followers_count"]) {
        Some(1) => "1 follower".to_owned(),
        Some(n) => format!("{n} followers"),
        None => String::new(),
    }
}

/// A person or artist in a list: a user's card says how many follow them, an artist's says
/// "Artist".
fn person(v: &Value) -> Option<Card> {
    let uri = text(&v["uri"]);
    let name = text(&v["name"]);
    if uri.is_empty() {
        return None;
    }
    let artist = uri.starts_with("spotify:artist:");
    let name = if name.is_empty() { uri.rsplit(':').next().unwrap_or_default().to_owned() } else { name };
    let subtitle = if artist { "Artist".to_owned() } else { followers_text(v) };
    Some(Card { uri, name, subtitle, image: image(v) })
}

/// A user's profile page from `/user-profile-view/v3/profile/{username}`.
pub fn user_page(username: &str, v: &Value) -> UserPage {
    let name = [text(&v["name"]), text(&v["display_name"])].into_iter().find(|n| !n.is_empty()).unwrap_or_else(|| username.to_owned());
    let playlists = v["public_playlists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let uri = text(&p["uri"]);
            let name = text(&p["name"]);
            (uri.starts_with("spotify:") && uri.contains("playlist:") && !name.is_empty()).then(|| {
                let owner = text(&p["owner_name"]);
                Card { uri, name, subtitle: if owner.is_empty() { "Playlist".to_owned() } else { format!("By {owner}") }, image: image(p) }
            })
        })
        .collect();
    let artists = v["recently_played_artists"].as_array().into_iter().flatten().filter_map(person).collect();
    UserPage {
        username: username.to_owned(),
        name,
        image: image(v),
        followers: count(&v["followers_count"]),
        following: count(&v["following_count"]),
        playlists_total: count(&v["total_public_playlists_count"]),
        playlists,
        artists,
        is_following: v["is_following"].as_bool(),
        allow_follows: v["allow_follows"].as_bool().unwrap_or(true),
    }
}

/// The people (and artists) in `/followers` or `/following`.
pub fn people(v: &Value) -> Vec<Card> {
    v["profiles"].as_array().into_iter().flatten().filter_map(person).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_profile_page_reads_counts_playlists_and_artists() {
        let v = json!({
            "uri": "spotify:user:wisp", "name": "wisp", "image_url": "https://i/me",
            "followers_count": 38, "following_count": 212, "total_public_playlists_count": 14,
            "public_playlists": [
                { "uri": "spotify:playlist:a", "name": "care", "image_url": "https://i/a", "owner_name": "wisp" },
                { "uri": "spotify:playlist:b", "name": "" }
            ],
            "recently_played_artists": [{ "uri": "spotify:artist:x", "name": "Yeat", "image_url": "https://i/x" }]
        });
        let page = user_page("wisp", &v);
        assert_eq!((page.name.as_str(), page.followers, page.following, page.playlists_total), ("wisp", Some(38), Some(212), Some(14)));
        assert_eq!((page.is_following, page.allow_follows), (None, true));
        let followed = user_page("x", &json!({ "is_following": true, "allow_follows": false }));
        assert_eq!((followed.is_following, followed.allow_follows), (Some(true), false));
        assert_eq!(page.playlists.len(), 1);
        assert_eq!(page.playlists[0].subtitle, "By wisp");
        assert_eq!((page.artists[0].name.as_str(), page.artists[0].subtitle.as_str()), ("Yeat", "Artist"));
        // Nothing known: the username stands in for the name.
        let empty = user_page("someone", &json!({}));
        assert_eq!((empty.name.as_str(), empty.followers), ("someone", None));
    }

    #[test]
    fn people_lists_mix_users_and_artists() {
        let v = json!({ "profiles": [
            { "uri": "spotify:user:a", "name": "Alyssa", "followers_count": 1 },
            { "uri": "spotify:artist:b", "name": "Mara Vey", "image_url": "https://i/b" },
            { "uri": "spotify:user:c" },
            { "name": "no uri" }
        ]});
        let list = people(&v);
        let got: Vec<(&str, &str)> = list.iter().map(|c| (c.name.as_str(), c.subtitle.as_str())).collect();
        assert_eq!(got, [("Alyssa", "1 follower"), ("Mara Vey", "Artist"), ("c", "")]);
    }
}
