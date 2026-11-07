//! Pure conversions from Spotify protobuf messages to Truffle's model types.
//!
//! Everything here is synchronous, does no I/O and never panics on missing protobuf fields:
//! the generated accessors return defaults for absent optional fields, and every lookup that
//! can fail falls back to an empty / "unknown" value.

use librespot_core::{SpotifyId, date::Date};
use librespot_protocol::metadata::{self as pb, image::Size as ImageSize};
use librespot_protocol::playlist4_external::ListAttributes;

use crate::model::{ArtistRef, TrackInfo};

/// Spotify's public image CDN. Image file ids are 20 bytes, written as 40 lowercase hex chars.
pub const IMAGE_BASE: &str = "https://i.scdn.co/image/";

/// Lowercase hex encoding.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// `https://i.scdn.co/image/<hex>` for a raw image file id, `None` for an empty id.
pub fn image_url(file_id: &[u8]) -> Option<String> {
    if file_id.is_empty() {
        None
    } else {
        Some(format!("{IMAGE_BASE}{}", hex(file_id)))
    }
}

/// `spotify:<kind>:<base62>` from a 16-byte gid; empty string when the gid is missing/invalid.
pub fn gid_uri(kind: &str, gid: &[u8]) -> String {
    SpotifyId::from_raw(gid)
        .and_then(|id| id.to_base62())
        .map(|b62| format!("spotify:{kind}:{b62}"))
        .unwrap_or_default()
}

/// 64-bit FNV-1a over the URIs (with a separator). Stable across Rust versions, unlike
/// `DefaultHasher`, so it can be persisted as a cache revision.
pub fn fingerprint<'a>(uris: impl IntoIterator<Item = &'a str>) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for uri in uris {
        for b in uri.bytes().chain(std::iter::once(b'\n')) {
            hash ^= b as u64;
            hash = hash.wrapping_mul(PRIME);
        }
    }
    format!("{hash:016x}")
}

/// What decides whether the current account can play an item (mirrors librespot's player).
#[derive(Debug, Clone)]
pub struct Market {
    /// Two-letter country code of the account, empty if not known yet.
    pub country: String,
    /// Account catalogue ("premium" for every account that can use Truffle).
    pub catalogue: String,
    pub filter_explicit: bool,
    pub now: Date,
}

impl Market {
    pub fn from_session(session: &librespot_core::Session) -> Self {
        Self {
            country: session.country(),
            catalogue: session
                .get_user_attribute("catalogue")
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "premium".to_owned()),
            filter_explicit: session.filter_explicit_content(),
            now: Date::now_utc(),
        }
    }
}

fn country_listed(list: &str, country: &str) -> bool {
    list.as_bytes()
        .chunks(2)
        .any(|code| code.eq_ignore_ascii_case(country.as_bytes()))
}

/// Same rule as librespot's `allowed_for_user`: the first restriction that applies to the
/// user's catalogue and carries a country list decides.
pub fn allowed_in_market(restrictions: &[pb::Restriction], market: &Market) -> bool {
    if market.country.is_empty() {
        return true;
    }
    for r in restrictions
        .iter()
        .filter(|r| r.catalogue_str.iter().any(|c| c == &market.catalogue))
    {
        if r.has_countries_allowed() {
            return country_listed(r.countries_allowed(), &market.country);
        }
        if r.has_countries_forbidden() {
            return !country_listed(r.countries_forbidden(), &market.country);
        }
    }
    true
}

/// Same rule as librespot's `available`: no availability windows means available.
fn released(availability: &[pb::Availability], now: &Date) -> bool {
    if availability.is_empty() {
        return true;
    }
    availability.iter().any(|a| match a.start.as_ref() {
        None => true,
        Some(start) => Date::try_from(start).map(|s| *now >= s).unwrap_or(true),
    })
}

fn effective_width(img: &pb::Image) -> i32 {
    if img.width() > 0 {
        return img.width();
    }
    match img.size() {
        ImageSize::SMALL => 64,
        ImageSize::LARGE => 640,
        ImageSize::XLARGE => 1280,
        ImageSize::DEFAULT => 300,
    }
}

/// Pick (small ~64px+, large ~300-640px) cover URLs from a set of images.
pub fn pick_covers(images: &[pb::Image]) -> (Option<String>, Option<String>) {
    let mut sized: Vec<(i32, &[u8])> = images
        .iter()
        .filter(|i| !i.file_id().is_empty())
        .map(|i| (effective_width(i), i.file_id()))
        .collect();
    if sized.is_empty() {
        return (None, None);
    }
    sized.sort_by_key(|(w, _)| *w);
    let largest = sized[sized.len() - 1];
    let small = sized.iter().find(|(w, _)| *w >= 64).copied().unwrap_or(largest);
    let large = sized
        .iter()
        .rev()
        .find(|(w, _)| (300..=640).contains(w))
        .or_else(|| sized.iter().find(|(w, _)| *w >= 300))
        .copied()
        .unwrap_or(largest);
    (image_url(small.1), image_url(large.1))
}

fn album_images(album: &pb::Album) -> &[pb::Image] {
    let group = &album.cover_group.get_or_default().image;
    if group.is_empty() { &album.cover } else { group }
}

/// Map a `TRACK_V4` metadata message. `respect_files`: treat a track without audio files and
/// without alternatives as unplayable (disabled when a whole response carries no file lists,
/// so a server-side format change can never mark an entire library unplayable).
pub fn track_info(uri: &str, t: &pb::Track, market: &Market, respect_files: bool) -> TrackInfo {
    let album = t.album.get_or_default();

    let mut artists: Vec<ArtistRef> = t
        .artist
        .iter()
        .filter(|a| !a.name().is_empty())
        .map(|a| ArtistRef { name: a.name().to_owned(), uri: gid_uri("artist", a.gid()) })
        .collect();
    if artists.is_empty() {
        artists = t
            .artist_with_role
            .iter()
            .filter(|a| !a.artist_name().is_empty())
            .map(|a| ArtistRef {
                name: a.artist_name().to_owned(),
                uri: gid_uri("artist", a.artist_gid()),
            })
            .collect();
    }
    if artists.is_empty() {
        artists = album
            .artist
            .iter()
            .filter(|a| !a.name().is_empty())
            .map(|a| ArtistRef { name: a.name().to_owned(), uri: gid_uri("artist", a.gid()) })
            .collect();
    }

    let (cover_small, cover_large) = pick_covers(album_images(album));

    let has_audio = !t.file.is_empty() || !t.alternative.is_empty();
    let live = !t.has_earliest_live_timestamp()
        || Date::from_timestamp_ms(t.earliest_live_timestamp())
            .map(|d| market.now >= d)
            .unwrap_or(true);
    let playable = t.duration() > 0
        && (has_audio || !respect_files)
        && live
        && released(&t.availability, &market.now)
        && allowed_in_market(&t.restriction, market)
        && !(t.explicit() && market.filter_explicit);

    TrackInfo {
        uri: uri.to_owned(),
        name: non_empty_or(t.name(), "Unknown track"),
        artists,
        album: album.name().to_owned(),
        album_uri: gid_uri("album", album.gid()),
        cover_small,
        cover_large,
        duration_ms: t.duration().max(0) as u32,
        playable,
        is_local: false,
        added_at: 0,
    }
}

/// Map an `EPISODE_V4` metadata message. The show stands in for artist and album.
pub fn episode_info(uri: &str, e: &pb::Episode, market: &Market, respect_files: bool) -> TrackInfo {
    let show = e.show.get_or_default();
    let show_uri = gid_uri("show", show.gid());
    let own = &e.cover_image.get_or_default().image;
    let images = if own.is_empty() { &show.cover_image.get_or_default().image } else { own };
    let (cover_small, cover_large) = pick_covers(images);

    let artists = if show.name().is_empty() {
        Vec::new()
    } else {
        vec![ArtistRef { name: show.name().to_owned(), uri: show_uri.clone() }]
    };

    let playable = e.duration() > 0
        && (!e.audio.is_empty() || !respect_files)
        && released(&e.availability, &market.now)
        && allowed_in_market(&e.restriction, market)
        && !(e.explicit() && market.filter_explicit);

    TrackInfo {
        uri: uri.to_owned(),
        name: non_empty_or(e.name(), "Unknown episode"),
        artists,
        album: show.name().to_owned(),
        album_uri: show_uri,
        cover_small,
        cover_large,
        duration_ms: e.duration().max(0) as u32,
        playable,
        is_local: false,
        added_at: 0,
    }
}

/// Entry for something Spotify no longer has (or that Truffle can't play).
pub fn unavailable_info(uri: &str) -> TrackInfo {
    TrackInfo {
        uri: uri.to_owned(),
        name: "Unavailable track".to_owned(),
        artists: Vec::new(),
        album: String::new(),
        album_uri: String::new(),
        cover_small: None,
        cover_large: None,
        duration_ms: 0,
        playable: false,
        is_local: false,
        added_at: 0,
    }
}

pub fn is_local_uri(uri: &str) -> bool {
    uri.starts_with("spotify:local:")
}

/// Track and episode URIs are what the metadata service knows about.
pub fn is_catalog_item(uri: &str) -> bool {
    (uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:"))
        && uri.rsplit(':').next().is_some_and(|id| id.len() == 22)
}

/// Decode one `application/x-www-form-urlencoded` component (`+` = space, `%XX` = byte).
pub fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `spotify:local:<artist>:<album>:<title>:<seconds>` (components form-encoded). Local files
/// can't be streamed, so they are shown but never played.
pub fn local_track_info(uri: &str) -> TrackInfo {
    let rest = uri.strip_prefix("spotify:local:").unwrap_or("");
    let parts: Vec<&str> = rest.split(':').collect();
    let (artist, album, title, seconds) = match parts.len() {
        0 | 1 => ("", "", rest.to_owned(), ""),
        2 => (parts[0], "", parts[1].to_owned(), ""),
        3 => (parts[0], parts[1], parts[2].to_owned(), ""),
        // Components are encoded, so extra colons only appear in malformed URIs: keep them in
        // the title.
        n => (parts[0], parts[1], parts[2..n - 1].join(":"), parts[n - 1]),
    };
    let artist = form_decode(artist);
    let title = form_decode(&title);
    TrackInfo {
        uri: uri.to_owned(),
        name: if title.is_empty() { "Local file".to_owned() } else { title },
        artists: if artist.is_empty() {
            Vec::new()
        } else {
            vec![ArtistRef { name: artist, uri: String::new() }]
        },
        album: form_decode(album),
        album_uri: String::new(),
        cover_small: None,
        cover_large: None,
        duration_ms: seconds.parse::<u32>().map(|s| s.saturating_mul(1000)).unwrap_or(0),
        playable: false,
        is_local: true,
        added_at: 0,
    }
}

/// Playlist cover: the custom/generated picture id, else the mosaic URLs Spotify lists.
pub fn playlist_image(attrs: &ListAttributes) -> Option<String> {
    if let Some(url) = image_url(attrs.picture()) {
        return Some(url);
    }
    for wanted in ["default", "large", "small", "xlarge"] {
        if let Some(p) = attrs
            .picture_size
            .iter()
            .find(|p| p.target_name() == wanted && !p.url().is_empty())
        {
            return Some(p.url().to_owned());
        }
    }
    if let Some(p) = attrs.picture_size.iter().find(|p| !p.url().is_empty()) {
        return Some(p.url().to_owned());
    }
    attrs
        .format_attributes
        .iter()
        .find(|a| matches!(a.key(), "image_url" | "image") && a.value().starts_with("https://"))
        .map(|a| a.value().to_owned())
}

/// Playlist descriptions may contain links (`<a href=...>`) and HTML entities.
pub fn clean_description(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let decoded = tail.find(';').filter(|&end| end <= 10).and_then(|end| {
            let entity = &tail[1..end];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            ch.map(|c| (c, end + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

fn non_empty_or(s: &str, fallback: &str) -> String {
    if s.trim().is_empty() { fallback.to_owned() } else { s.to_owned() }
}

/// The base62 id inside `spotify:playlist:<id>` or `spotify:user:<u>:playlist:<id>`.
pub fn playlist_id(uri: &str) -> Option<SpotifyId> {
    let idx = uri.rfind("playlist:")?;
    let id = uri[idx + "playlist:".len()..].split(['?', ':']).next()?;
    SpotifyId::from_base62(id).ok()
}

/// Canonical `spotify:playlist:<id>` form, if the URI is a playlist.
pub fn normalize_playlist_uri(uri: &str) -> Option<String> {
    let id = playlist_id(uri)?;
    id.to_base62().ok().map(|b62| format!("spotify:playlist:{b62}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_to_url() {
        let id = [0xab, 0x67, 0x61, 0x6d, 0x00, 0x00, 0xb2, 0x73, 0x01, 0xff];
        assert_eq!(image_url(&id).as_deref(), Some("https://i.scdn.co/image/ab67616d0000b27301ff"));
        assert_eq!(image_url(&[]), None);
    }

    #[test]
    fn covers_pick_small_and_large() {
        let mk = |id: u8, size: ImageSize, width: i32| {
            let mut i = pb::Image::new();
            i.set_file_id(vec![id]);
            i.set_size(size);
            if width > 0 {
                i.set_width(width);
            }
            i
        };
        let images = vec![
            mk(1, ImageSize::LARGE, 0),
            mk(2, ImageSize::SMALL, 0),
            mk(3, ImageSize::DEFAULT, 0),
        ];
        let (small, large) = pick_covers(&images);
        assert_eq!(small.as_deref(), Some("https://i.scdn.co/image/02"));
        assert_eq!(large.as_deref(), Some("https://i.scdn.co/image/01"));
        assert_eq!(pick_covers(&[]), (None, None));
    }

    #[test]
    fn local_files_parse() {
        let t = local_track_info("spotify:local:Some+Artist:Some%3AAlbum:Home+Recording:187");
        assert!(t.is_local && !t.playable);
        assert_eq!(t.name, "Home Recording");
        assert_eq!(t.album, "Some:Album");
        assert_eq!(t.artists[0].name, "Some Artist");
        assert_eq!(t.duration_ms, 187_000);
        let odd = local_track_info("spotify:local:::Untitled:");
        assert_eq!(odd.name, "Untitled");
        assert!(odd.artists.is_empty());
    }

    #[test]
    fn form_decoding() {
        assert_eq!(form_decode("a+b%20c%"), "a b c%");
        assert_eq!(form_decode("%zz"), "%zz");
        assert_eq!(form_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn descriptions_are_cleaned() {
        assert_eq!(
            clean_description("Hits by <a href=\"spotify:artist:x\">Artist</a> &amp; friends&#x27; &#39;best&#39; &bogus"),
            "Hits by Artist & friends' 'best' &bogus"
        );
    }

    #[test]
    fn restrictions_follow_librespot_rules() {
        let market = Market {
            country: "SE".into(),
            catalogue: "premium".into(),
            filter_explicit: false,
            now: Date::now_utc(),
        };
        let mut allowed = pb::Restriction::new();
        allowed.catalogue_str = vec!["premium".into()];
        allowed.set_countries_allowed("USGBSE".into());
        assert!(allowed_in_market(std::slice::from_ref(&allowed), &market));
        allowed.set_countries_allowed("USGB".into());
        assert!(!allowed_in_market(std::slice::from_ref(&allowed), &market));
        let mut forbidden = pb::Restriction::new();
        forbidden.catalogue_str = vec!["premium".into()];
        forbidden.set_countries_forbidden("SEDK".into());
        assert!(!allowed_in_market(std::slice::from_ref(&forbidden), &market));
        let mut other = pb::Restriction::new();
        other.catalogue_str = vec!["free".into()];
        other.set_countries_allowed("US".into());
        assert!(allowed_in_market(std::slice::from_ref(&other), &market));
    }

    #[test]
    fn playlist_uris() {
        let a = normalize_playlist_uri("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M");
        let b = normalize_playlist_uri("spotify:user:someone:playlist:37i9dQZF1DXcBWIGoYBM5M");
        assert_eq!(a.as_deref(), Some("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M"));
        assert_eq!(a, b);
        assert_eq!(normalize_playlist_uri("spotify:start-group:abc:Folder"), None);
        assert!(is_catalog_item("spotify:track:4uLU6hMCjMI75M1A2tKUQC"));
        assert!(!is_catalog_item("spotify:local:a:b:c:1"));
    }

    #[test]
    fn fingerprint_is_order_sensitive() {
        let a = fingerprint(["spotify:track:a", "spotify:track:b"]);
        let b = fingerprint(["spotify:track:b", "spotify:track:a"]);
        assert_ne!(a, b);
        assert_eq!(a, fingerprint(["spotify:track:a", "spotify:track:b"]));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn gid_to_uri() {
        let gid = 0x6c3e_1f3e_9d4f_4f7c_9b3a_44d5_86f1_0b2au128.to_be_bytes();
        let uri = gid_uri("artist", &gid);
        assert!(uri.starts_with("spotify:artist:") && uri.len() == "spotify:artist:".len() + 22);
        assert_eq!(gid_uri("artist", &[1, 2, 3]), "");
    }
}
