//! A test aid: with `truffle-selftest.request` beside truffle.exe ("search" or "edit"), Truffle
//! checks search through the session and, for "edit", every playlist change on a throwaway
//! playlist it makes itself ("Truffle test"), then deletes it. It never touches any other playlist:
//! every change goes to the uri made in this run, and it stops before starting if a playlist with
//! that name already exists. The report goes to `truffle-selftest.txt` and the log.

use std::fmt::Write as _;

use librespot_core::SpotifyId;

use super::{Added, Library, convert, fetch};

pub const TEST_NAME: &str = "Truffle test";

struct Report(String, bool);

impl Report {
    fn check(&mut self, what: &str, ok: bool, detail: impl std::fmt::Display) {
        let _ = writeln!(self.0, "{} {what}: {detail}", if ok { "PASS" } else { "FAIL" });
        self.1 &= ok;
    }
}

/// Read-only: what Spotify's profile pages answer (the user's own profile, followers and
/// following), shortened, to see their shape.
pub async fn probe(lib: Library) -> String {
    let session = lib.session();
    let me = session.username();
    let mut out = String::new();
    let show = |label: &str, r: Result<bytes::Bytes, librespot_core::Error>| -> String {
        match r {
            Ok(b) => {
                let text = serde_json::from_slice::<serde_json::Value>(&b).map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()).unwrap_or_else(|_| String::from_utf8_lossy(&b).into_owned());
                let cut: String = text.chars().take(5000).collect();
                format!("== {label} ==
{cut}
")
            }
            Err(e) => format!("== {label} == ERROR {e}
"),
        }
    };
    out += &show("profile", session.spclient().get_user_profile(&me, Some(3), Some(3)).await);
    out += &show("followers", session.spclient().get_user_followers(&me).await);
    out += &show("following", session.spclient().get_user_following(&me).await);
    out
}

/// Read-only: what Spotify's friend activity answers (who plays what now), shortened, to see
/// whether the session may read it and its shape; and the user's own presence.
pub async fn presence(lib: Library) -> String {
    let session = lib.session();
    let me = session.username();
    let mut out = String::new();
    for (label, endpoint) in [
        ("buddylist", "/presence-view/v1/buddylist".to_owned()),
        ("buddylist v2", "/presence-view/v2/buddylist".to_owned()),
        ("own presence", format!("/presence-view/v1/user/{me}")),
    ] {
        let answer = session.spclient().request(&reqwest::Method::GET, &endpoint, None, None).await;
        let text = match answer {
            Ok(b) => serde_json::from_slice::<serde_json::Value>(&b)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                .unwrap_or_else(|_| String::from_utf8_lossy(&b).into_owned()),
            Err(e) => format!("ERROR {e}"),
        };
        let cut: String = text.chars().take(4000).collect();
        let _ = writeln!(out, "== {label} ==\n{cut}\n");
    }
    out
}

/// Follows Spotify's own account, checks, unfollows it, checks (left as it was before).
pub async fn follow_test(lib: Library) -> String {
    let mut r = Report(String::new(), true);
    let user = "spotify";
    let before = lib.follows_user(user).await;
    r.check("state before", before.is_ok(), format!("{before:?}"));
    if matches!(before, Ok(Some(true))) {
        r.check("start", false, "already following; nothing changed");
        return finish(r);
    }
    let f = lib.set_following_user(user, true).await;
    let now = lib.follows_user(user).await;
    r.check("follow", f.is_ok() && matches!(now, Ok(Some(true))), format!("{f:?} -> {now:?}"));
    let u = lib.set_following_user(user, false).await;
    let after = lib.follows_user(user).await;
    r.check("unfollow", u.is_ok() && matches!(after, Ok(Some(false)) | Ok(None)), format!("{u:?} -> {after:?}"));
    finish(r)
}

/// Read-only: the people a search for `name` finds, as the Users chip has them: how many in the
/// first pages (up to 5, until there are 20), then the first 20 names and links.
pub async fn users(lib: Library, name: &str) -> String {
    if name.is_empty() {
        return "FAIL users: no name (write \"users <name>\")\n".to_owned();
    }
    let mut found: Vec<crate::model::Card> = Vec::new();
    let (mut next, mut pages) = (Some(0), 0);
    while let Some(offset) = next.filter(|_| found.len() < 20 && pages < 5) {
        pages += 1;
        match lib.search_users(name, offset).await {
            Ok(page) => {
                for u in page.users {
                    if !found.iter().any(|k| k.uri == u.uri) {
                        found.push(u);
                    }
                }
                next = page.users_next;
            }
            Err(e) => return format!("FAIL users \"{name}\": {e:#}\n"),
        }
    }
    let more = if next.is_some() { ", and Spotify has more" } else { "" };
    let mut out = format!("{} people found for \"{name}\" in {pages} page(s){more}\n", found.len());
    for u in found.iter().take(20) {
        let _ = writeln!(out, "{} | {}", u.name, u.uri);
    }
    out
}

/// Runs the checks; `taken` are the names of the playlists already in the library.
pub async fn run(lib: Library, writes: bool, taken: Vec<String>) -> String {
    let mut r = Report(String::new(), true);
    // Search.
    let mut songs: Vec<String> = Vec::new();
    match lib.search("daft punk", 0).await {
        Ok(found) => {
            let ok = !found.tracks.is_empty() && !found.albums.is_empty() && !found.artists.is_empty() && !found.playlists.is_empty();
            r.check(
                "search",
                ok,
                format!("{} songs, {} albums, {} artists, {} playlists, more={}", found.tracks.len(), found.albums.len(), found.artists.len(), found.playlists.len(), found.more),
            );
            songs = found.tracks.iter().filter(|t| t.uri.starts_with("spotify:track:")).map(|t| t.uri.clone()).take(4).collect();
            if let Some(a) = found.artists.first() {
                r.check("search artist card", !a.name.is_empty() && a.image.is_some(), format!("{} / picture: {}", a.name, a.image.is_some()));
            }
        }
        Err(e) => r.check("search", false, format!("{e:#}")),
    }
    match lib.search("daft punk", 10).await {
        Ok(found) => r.check("search page 2", !found.tracks.is_empty(), format!("{} songs", found.tracks.len())),
        Err(e) => r.check("search page 2", false, format!("{e:#}")),
    }
    if !writes {
        return finish(r);
    }
    // The names as Spotify has them now (the app's copy may be old or still loading).
    let names: Vec<String> = match lib.playlists().await {
        Ok(list) => list.into_iter().map(|p| p.name).chain(taken).collect(),
        Err(e) => {
            r.check("start", false, format!("couldn't read your playlists to check the name: {e:#}; nothing was changed"));
            return finish(r);
        }
    };
    if names.iter().any(|n| n.starts_with(TEST_NAME)) {
        r.check("start", false, "a playlist named \"Truffle test\" already exists; nothing was changed");
        return finish(r);
    }
    if songs.len() < 4 {
        r.check("start", false, "search gave too few songs to test with; nothing was changed");
        return finish(r);
    }
    let session = lib.session();
    let before = fetch::rootlist_raw(&session).await.map(|(_, l)| l);

    // 1. Make it.
    let uri = match lib.create_playlist(TEST_NAME).await {
        Ok(uri) => uri,
        Err(e) => {
            r.check("create", false, format!("{e:#}"));
            let _ = writeln!(r.0, "Check your library for a playlist named \"{TEST_NAME}\" and delete it if it's there.");
            return finish(r);
        }
    };
    let Some(id) = convert::playlist_id(&uri) else {
        r.check("create", false, format!("odd uri {uri}"));
        return finish(r);
    };
    let header = |id: SpotifyId| {
        let session = session.clone();
        async move { fetch::playlist_header(&session, &id).await }
    };
    match header(id).await {
        Ok(h) => r.check("create", h.attributes.name() == TEST_NAME, format!("{uri} named {:?}, {} songs", h.attributes.name(), h.length())),
        Err(e) => r.check("create", false, format!("can't read it back: {e}")),
    }
    let first = fetch::rootlist_raw(&session).await.ok().and_then(|(_, l)| l.first().and_then(|u| convert::normalize_playlist_uri(u)));
    r.check("in library, first", first.as_deref() == Some(uri.as_str()), format!("{first:?}"));

    // 2. Rename, describe, clear the description.
    let renamed = format!("{TEST_NAME} 2");
    let step = lib.update_playlist_details(&uri, Some(&renamed), None).await;
    let name = header(id).await.map(|h| h.attributes.name().to_owned()).unwrap_or_default();
    r.check("rename", step.is_ok() && name == renamed, format!("{step:?} -> {name:?}"));
    let step = lib.update_playlist_details(&uri, None, Some("test")).await;
    let d = header(id).await.map(|h| h.attributes.description().to_owned()).unwrap_or_default();
    r.check("description", step.is_ok() && d == "test", format!("{step:?} -> {d:?}"));
    let step = lib.update_playlist_details(&uri, None, Some("")).await;
    let d = header(id).await.map(|h| h.attributes.description().to_owned()).unwrap_or_else(|_| "?".into());
    r.check("clear description", step.is_ok() && d.is_empty(), format!("{step:?} -> {d:?}"));

    // 3. Songs: add three, ask before adding one of them again, add it again as the user said,
    // remove it (both copies), then add a new one asking first.
    let items = |id: SpotifyId| {
        let session = session.clone();
        async move { fetch::playlist_items(&session, &id).await.map(|i| i.uris).unwrap_or_default() }
    };
    let (a, b, c, d) = (songs[0].clone(), songs[1].clone(), songs[2].clone(), songs[3].clone());
    let step = lib.add_to_playlist(&uri, &[a.clone(), b.clone(), c.clone()]).await;
    let now = items(id).await;
    r.check("add 3", step.as_ref().ok() == Some(&3) && now == [a.clone(), b.clone(), c.clone()], format!("{step:?} -> {} songs", now.len()));
    let step = lib.add_new_to_playlist(&uri, &[d.clone(), a.clone()], &[]).await;
    let now = items(id).await;
    let asked = matches!(&step, Ok(Added::Already(q)) if (q.already, q.songs) == (1, 2) && q.new == [d.clone()]);
    r.check("ask first", asked && now == [a.clone(), b.clone(), c.clone()], format!("{step:?} -> {} songs", now.len()));
    let step = lib.add_new_to_playlist(&uri, std::slice::from_ref(&a), std::slice::from_ref(&a)).await;
    let now = items(id).await;
    let again = matches!(step, Ok(Added::Songs(1)));
    r.check("add again", again && now == [a.clone(), b.clone(), c.clone(), a.clone()], format!("{step:?} -> {} songs", now.len()));
    let step = lib.remove_from_playlist(&uri, std::slice::from_ref(&a)).await;
    let now = items(id).await;
    r.check("remove", step.as_ref().ok() == Some(&2) && now == [b.clone(), c.clone()], format!("{step:?} -> {} songs", now.len()));
    let step = lib.add_new_to_playlist(&uri, std::slice::from_ref(&d), &[]).await;
    let now = items(id).await;
    r.check("add new", matches!(step, Ok(Added::Songs(1))) && now == [b.clone(), c.clone(), d.clone()], format!("{step:?} -> {} songs", now.len()));

    // 4. A cover: a plain colour picture.
    let path = lib.inner.paths.cache.join("truffle-selftest-cover.jpg");
    let picture = image::RgbImage::from_pixel(640, 640, image::Rgb([232, 80, 106]));
    match picture.save_with_format(&path, image::ImageFormat::Jpeg) {
        Ok(()) => {
            let step = lib.set_playlist_cover(&uri, &path).await;
            let has = header(id).await.map(|h| h.attributes.has_picture()).unwrap_or(false);
            r.check("cover", step.is_ok() && has, format!("{step:?}, picture set: {has}"));
        }
        Err(e) => r.check("cover", false, format!("couldn't make the test picture: {e}")),
    }
    let _ = std::fs::remove_file(&path);

    // 5. Delete it; the library list is as it was.
    let step = lib.remove_playlist(&uri).await;
    let after = fetch::rootlist_raw(&session).await.map(|(_, l)| l);
    let same = match (&before, &after) {
        (Ok(b), Ok(a)) => a == b,
        _ => false,
    };
    r.check("delete", step.is_ok() && same, format!("{step:?}, library list back as before: {same}"));
    finish(r)
}

fn finish(r: Report) -> String {
    let head = if r.1 { "ALL PASSED" } else { "SOME FAILED" };
    format!("{head}\n{}", r.0)
}
