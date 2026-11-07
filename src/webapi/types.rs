//! Web API JSON shapes (only the fields Truffle reads) and their mapping to `model` types.
//! Every nested field is optional: Spotify omits or nulls many of them in Development Mode.

use serde::{Deserialize, Deserializer};

use crate::model::{ArtistRef, TrackInfo, UserProfile};

/// Treat an explicit JSON `null` like a missing field.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct ApiImage {
    #[serde(default, deserialize_with = "null_default")]
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ApiUser {
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub images: Vec<ApiImage>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ApiArtist {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ApiAlbum {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub images: Vec<ApiImage>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ApiTrack {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub is_playable: Option<bool>,
    #[serde(default)]
    pub is_local: Option<bool>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub artists: Vec<ApiArtist>,
    #[serde(default)]
    pub album: Option<ApiAlbum>,
}

/// An entry of `GET /me/tracks` (`track` may be null for removed tracks).
#[derive(Debug, Default, Deserialize)]
pub(super) struct ApiSavedTrack {
    /// When it was liked (`2024-05-11T19:02:44Z`).
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub track: Option<ApiTrack>,
}

/// A paging object; items are kept raw so one odd entry can't fail the whole page.
#[derive(Debug, Default, Deserialize)]
pub(super) struct ApiPage {
    #[serde(default, deserialize_with = "null_default")]
    pub items: Vec<serde_json::Value>,
    #[serde(default)]
    pub total: Option<u32>,
}

/// Unix seconds of a UTC time as Spotify writes it (`2024-05-11T19:02:44Z`; fractions dropped).
pub(super) fn unix_time(text: &str) -> Option<i64> {
    let (date, time) = text.trim().split_once('T')?;
    let time = time.strip_suffix('Z').or_else(|| time.strip_suffix("+00:00"))?;
    let number = |s: &str| (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<i64>().ok()).flatten();
    let mut d = date.split('-');
    let (y, m, day) = (number(d.next()?)?, number(d.next()?)?, number(d.next()?)?);
    let mut t = time.split('.').next()?.split(':');
    let (h, min, s) = (number(t.next()?)?, number(t.next()?)?, number(t.next()?)?);
    if d.next().is_some() || t.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || min > 59 || s > 60 {
        return None;
    }
    Some(crate::library::days_from_civil(y, m, day) * 86_400 + h * 3_600 + min * 60 + s)
}

/// Smallest image at least `min_width` wide; else the largest known one; else the first.
pub(super) fn pick_image(images: &[ApiImage], min_width: u32) -> Option<String> {
    let usable = || images.iter().filter(|i| !i.url.is_empty());
    usable()
        .filter(|i| i.width.is_some_and(|w| w >= min_width))
        .min_by_key(|i| i.width.unwrap_or(u32::MAX))
        .or_else(|| usable().filter(|i| i.width.is_some()).max_by_key(|i| i.width.unwrap_or(0)))
        .or_else(|| usable().next())
        .map(|i| i.url.clone())
}

fn uri_or_from_id(uri: Option<String>, kind: &str, id: Option<&str>) -> String {
    match uri.filter(|u| !u.is_empty()) {
        Some(uri) => uri,
        None => match id.filter(|id| !id.is_empty()) {
            Some(id) => format!("spotify:{kind}:{id}"),
            None => String::new(),
        },
    }
}

pub(super) fn user_profile(user: ApiUser) -> UserProfile {
    let display_name = user
        .display_name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| user.id.clone());
    UserProfile { image_url: pick_image(&user.images, 64), display_name, username: user.id, premium: None }
}

/// Map a Web API track. `None` for entries without a usable URI.
pub(super) fn track_info(track: ApiTrack) -> Option<TrackInfo> {
    let kind = track.kind.as_deref().unwrap_or("track");
    let uri = uri_or_from_id(track.uri, kind, track.id.as_deref());
    if uri.is_empty() {
        return None;
    }
    let is_local = track.is_local.unwrap_or(false) || uri.starts_with("spotify:local:");
    let streamable = uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:");
    let album = track.album.unwrap_or_default();
    Some(TrackInfo {
        name: track.name,
        artists: track
            .artists
            .into_iter()
            .filter(|a| !a.name.is_empty() || a.id.is_some())
            .map(|a| ArtistRef { uri: uri_or_from_id(a.uri, "artist", a.id.as_deref()), name: a.name })
            .collect(),
        album_uri: uri_or_from_id(album.uri, "album", album.id.as_deref()),
        cover_small: pick_image(&album.images, 64),
        cover_large: pick_image(&album.images, 300),
        album: album.name,
        duration_ms: track.duration_ms.unwrap_or(0).min(u64::from(u32::MAX)) as u32,
        playable: streamable && !is_local && track.is_playable.unwrap_or(true),
        is_local,
        uri,
        added_at: 0,
    })
}

/// Parses a `GET /search` response. Odd or missing entries (Spotify sends `null` for removed
/// playlists) are skipped one by one.
pub(super) fn search_results(query: &str, offset: u32, v: &serde_json::Value) -> crate::model::SearchResults {
    use crate::model::Card;
    let items = |kind: &str| -> Vec<serde_json::Value> {
        v.get(kind).and_then(|k| k.get("items")).and_then(|i| i.as_array()).cloned().unwrap_or_default()
    };
    let images = |item: &serde_json::Value| -> Vec<ApiImage> {
        item.get("images").and_then(|i| serde_json::from_value(i.clone()).ok()).unwrap_or_default()
    };
    let text = |item: &serde_json::Value, key: &str| item.get(key).and_then(|s| s.as_str()).unwrap_or_default().to_owned();
    let first_artist = |item: &serde_json::Value| {
        item.get("artists").and_then(|a| a.as_array()).and_then(|a| a.first()).map(|a| text(a, "name")).unwrap_or_default()
    };
    let card = |item: &serde_json::Value, kind: &str, subtitle: String| -> Option<Card> {
        let uri = uri_or_from_id(item.get("uri").and_then(|u| u.as_str()).map(str::to_owned), kind, item.get("id").and_then(|i| i.as_str()));
        let name = text(item, "name");
        (!uri.is_empty() && !name.is_empty()).then(|| Card { uri, name, subtitle, image: pick_image(&images(item), 160) })
    };

    let tracks: Vec<TrackInfo> = items("tracks")
        .into_iter()
        .filter_map(|t| serde_json::from_value::<ApiTrack>(t).ok())
        .filter_map(track_info)
        .collect();
    let total = v.get("tracks").and_then(|t| t.get("total")).and_then(|t| t.as_u64()).unwrap_or(0);
    let albums = items("albums")
        .iter()
        .filter_map(|a| {
            let kind = match text(a, "album_type").as_str() {
                "single" => "Single",
                "compilation" => "Compilation",
                _ => "Album",
            };
            let artist = first_artist(a);
            card(a, "album", if artist.is_empty() { kind.to_owned() } else { format!("{kind} · {artist}") })
        })
        .collect();
    let artists = items("artists").iter().filter_map(|a| card(a, "artist", "Artist".to_owned())).collect();
    let playlists = items("playlists")
        .iter()
        .filter(|p| !p.is_null())
        .filter_map(|p| {
            let owner = p.get("owner").map(|o| text(o, "display_name")).unwrap_or_default();
            card(p, "playlist", if owner.is_empty() { "Playlist".to_owned() } else { format!("Playlist · {owner}") })
        })
        .collect();
    let more = u64::from(offset) + (tracks.len() as u64) < total.min(1000) && !tracks.is_empty();
    crate::model::SearchResults { query: query.to_owned(), offset, tracks, albums, artists, playlists, more, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_search_results() {
        let v = serde_json::json!({
            "tracks": {"total": 42, "items": [
                {"uri": "spotify:track:a", "name": "Song", "duration_ms": 1000, "artists": [{"name": "Ann"}],
                 "album": {"uri": "spotify:album:x", "name": "LP", "images": []}},
                {"broken": true}
            ]},
            "albums": {"items": [{"id": "x", "name": "LP", "album_type": "single", "artists": [{"name": "Ann"}],
                                  "images": [{"url": "http://img", "width": 300}]}]},
            "artists": {"items": [{"uri": "spotify:artist:y", "name": "Ann"}]},
            "playlists": {"items": [null, {"uri": "spotify:playlist:z", "name": "Mix", "owner": {"display_name": "Bo"}}]}
        });
        let r = search_results("ann", 0, &v);
        assert_eq!(r.tracks.len(), 1);
        assert_eq!(r.tracks[0].album_uri, "spotify:album:x");
        assert_eq!(r.albums[0].uri, "spotify:album:x");
        assert_eq!(r.albums[0].subtitle, "Single · Ann");
        assert_eq!(r.albums[0].image.as_deref(), Some("http://img"));
        assert_eq!(r.artists[0].name, "Ann");
        assert_eq!(r.playlists.len(), 1);
        assert_eq!(r.playlists[0].subtitle, "Playlist · Bo");
        assert!(r.more);
        assert!(!search_results("ann", 41, &v).more);
    }

    #[test]
    fn image_choice() {
        let images = vec![
            ApiImage { url: "big".into(), width: Some(640) },
            ApiImage { url: "mid".into(), width: Some(300) },
            ApiImage { url: "small".into(), width: Some(64) },
        ];
        assert_eq!(pick_image(&images, 64).as_deref(), Some("small"));
        assert_eq!(pick_image(&images, 300).as_deref(), Some("mid"));
        assert_eq!(pick_image(&images, 1000).as_deref(), Some("big"));
        let unknown = vec![ApiImage { url: "only".into(), width: None }];
        assert_eq!(pick_image(&unknown, 300).as_deref(), Some("only"));
        assert_eq!(pick_image(&[], 64), None);
    }

    #[test]
    fn liked_page_maps_tracks_and_skips_nulls() {
        let json = r#"{
          "total": 4, "limit": 50, "offset": 0,
          "items": [
            {"added_at": "2024-05-11T19:02:44Z", "track": {
              "type": "track", "id": "4iV5W9uYEdYUVa79Axb7Rh", "uri": "spotify:track:4iV5W9uYEdYUVa79Axb7Rh",
              "name": "Example Song", "duration_ms": 215040, "is_playable": true, "is_local": false,
              "artists": [{"id": "0TnOYISbd1XYRBk9myaseg", "name": "Example Artist", "uri": "spotify:artist:0TnOYISbd1XYRBk9myaseg"}],
              "album": {"id": "2up3OPMp9Tb4dAKM2erWXQ", "name": "Example Album", "images": [
                {"url": "L", "width": 640, "height": 640}, {"url": "M", "width": 300, "height": 300},
                {"url": "S", "width": 64, "height": 64}]}}},
            {"added_at": "2021-08-30T08:15:00Z", "track": {
              "type": "track", "id": "1301WleyT98MSxVHPZCA6M", "name": "Region Locked", "duration_ms": 201000,
              "is_playable": false, "restrictions": {"reason": "market"}, "artists": null, "album": null}},
            {"added_at": "2019-03-03T12:00:00Z", "track": null},
            null
          ]
        }"#;
        let page: ApiPage = serde_json::from_str(json).unwrap();
        assert_eq!(page.total, Some(4));
        let tracks: Vec<TrackInfo> = page
            .items
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Option<ApiSavedTrack>>(v).ok().flatten())
            .filter_map(|s| s.track)
            .filter_map(track_info)
            .collect();
        assert_eq!(tracks.len(), 2);
        let first = &tracks[0];
        assert_eq!(first.uri, "spotify:track:4iV5W9uYEdYUVa79Axb7Rh");
        assert_eq!(first.artist_line(), "Example Artist");
        assert_eq!(first.artists[0].uri, "spotify:artist:0TnOYISbd1XYRBk9myaseg");
        assert_eq!(first.album, "Example Album");
        assert_eq!(first.album_uri, "spotify:album:2up3OPMp9Tb4dAKM2erWXQ");
        assert_eq!(first.cover_small.as_deref(), Some("S"));
        assert_eq!(first.cover_large.as_deref(), Some("M"));
        assert_eq!(first.duration_ms, 215040);
        assert!(first.playable && !first.is_local);
        let locked = &tracks[1];
        assert_eq!(locked.uri, "spotify:track:1301WleyT98MSxVHPZCA6M");
        assert!(!locked.playable);
        assert!(locked.artists.is_empty());
        assert_eq!(locked.cover_small, None);
    }

    #[test]
    fn like_times_are_read() {
        let day = crate::library::days_from_civil(2024, 5, 11) * 86_400;
        assert_eq!(unix_time("2024-05-11T19:02:44Z"), Some(day + 68_564));
        assert_eq!(unix_time(" 2024-05-11T19:02:44.250Z"), Some(day + 68_564));
        assert_eq!(unix_time("2024-05-11T19:02:44+00:00"), Some(day + 68_564));
        assert_eq!(unix_time("1970-01-01T00:00:00Z"), Some(0));
        for odd in [
            "",
            "2024-05-11",
            "2024-05-11T19:02:44",
            "2024-13-01T00:00:00Z",
            "2024-05-11T24:00:00Z",
            "2024-05-11T-1:02:44Z",
            "2024-05-11T19:02Z",
            "2024-05-11-01T19:02:44Z",
            "yesterday 🎵",
            "２０２４-05-11T19:02:44Z",
            "99999999999999999999-05-11T19:02:44Z",
        ] {
            assert_eq!(unix_time(odd), None, "{odd}");
        }
    }

    #[test]
    fn local_tracks_are_not_playable() {
        let track: ApiTrack = serde_json::from_str(
            r#"{"type":"track","id":null,"uri":"spotify:local:A:B:Home:187","name":"Home",
                "duration_ms":187000,"is_local":true,"artists":[{"id":null,"name":"A"}],
                "album":{"id":null,"name":"B","images":[]}}"#,
        )
        .unwrap();
        let info = track_info(track).unwrap();
        assert!(info.is_local && !info.playable);
        assert_eq!(info.artists[0].uri, "");
        assert_eq!(info.album_uri, "");
    }

    #[test]
    fn profile_mapping() {
        let user: ApiUser =
            serde_json::from_str(r#"{"id":"user1","display_name":null,"images":[]}"#).unwrap();
        let p = user_profile(user);
        assert_eq!(p.username, "user1");
        assert_eq!(p.display_name, "user1");
        assert_eq!(p.image_url, None);
    }
}
