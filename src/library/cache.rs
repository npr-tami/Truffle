//! On-disk and in-memory caches for the library.
//!
//! - `<cache>/tracks/<source.cache_key()>.json`: `{revision, saved_at, username, sync_token, tracks}`
//!   per source. Written atomically through `config::write_json`.
//! - `<cache>/playlists.json`: the last rootlist (already flattened into summaries) plus the
//!   owner display-name map, so the sidebar can be drawn instantly at startup.
//! - A tiny LRU of the most recently used track lists (shared `Arc`s, no copies).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{read_json, write_json};
use crate::model::{PlaylistSummary, Source, TrackInfo};

/// How many track lists stay in memory.
pub const LRU_CAPACITY: usize = 4;

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Deserialize)]
pub struct TrackCacheFile {
    pub revision: String,
    /// Account the list belongs to (only meaningful for Liked Songs). Empty in old files.
    #[serde(default)]
    pub username: String,
    /// Liked Songs: the collection's sync token the list was read at, to ask only for the changes
    /// since. Empty for other lists and in old files.
    #[serde(default)]
    pub sync_token: String,
    pub tracks: Vec<TrackInfo>,
}

/// Borrowing twin of [`TrackCacheFile`] so a shared list can be written without copying it.
#[derive(Serialize)]
struct TrackCacheRef<'a> {
    revision: &'a str,
    saved_at: u64,
    username: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    sync_token: &'a str,
    tracks: &'a [TrackInfo],
}

pub fn track_cache_path(dir: &Path, source: &Source) -> PathBuf {
    dir.join(format!("{}.json", source.cache_key()))
}

pub fn read_track_cache(path: &Path) -> Option<TrackCacheFile> {
    read_json(path)
}

pub fn write_track_cache(path: &Path, revision: &str, username: &str, sync_token: &str, tracks: &[TrackInfo]) -> Result<()> {
    write_json(path, &TrackCacheRef { revision, saved_at: now_secs(), username, sync_token, tracks })
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistsCacheFile {
    pub username: String,
    /// Hex revision of the rootlist the summaries came from.
    pub revision: String,
    pub saved_at: u64,
    /// Owner username -> display name.
    pub owners: HashMap<String, String>,
    pub playlists: Vec<PlaylistSummary>,
}

pub fn playlists_cache_path(cache_root: &Path) -> PathBuf {
    cache_root.join("playlists.json")
}

pub fn read_playlists_cache(path: &Path) -> Option<PlaylistsCacheFile> {
    read_json(path)
}

pub fn write_playlists_cache(path: &Path, file: &PlaylistsCacheFile) -> Result<()> {
    write_json(path, file)
}

#[derive(Clone)]
pub struct CachedList {
    pub key: String,
    pub revision: String,
    pub username: String,
    /// See [`TrackCacheFile::sync_token`].
    pub sync_token: String,
    pub tracks: Arc<Vec<TrackInfo>>,
}

/// Most recently used at the back.
#[derive(Default)]
pub struct Lru {
    entries: VecDeque<CachedList>,
}

impl Lru {
    pub fn get(&mut self, key: &str) -> Option<CachedList> {
        let idx = self.entries.iter().position(|e| e.key == key)?;
        let entry = self.entries.remove(idx)?;
        self.entries.push_back(entry.clone());
        Some(entry)
    }

    pub fn put(&mut self, entry: CachedList) {
        self.entries.retain(|e| e.key != entry.key);
        self.entries.push_back(entry);
        while self.entries.len() > LRU_CAPACITY {
            self.entries.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// A song's details from any list kept in memory.
    pub fn find_track(&self, uri: &str) -> Option<TrackInfo> {
        self.entries.iter().rev().find_map(|e| e.tracks.iter().find(|t| t.uri == uri)).cloned()
    }

    /// The details of those of `uris` any list kept in memory has playable, each once: one look
    /// through the lists, however many songs.
    pub fn find_tracks(&self, uris: &[String]) -> Vec<TrackInfo> {
        let mut want: HashSet<&str> = uris.iter().map(String::as_str).collect();
        let mut found = Vec::new();
        for t in self.entries.iter().rev().flat_map(|e| e.tracks.iter()) {
            if want.is_empty() {
                break;
            }
            if t.playable && !t.is_local && want.remove(t.uri.as_str()) {
                found.push(t.clone());
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ArtistRef;

    fn sample(n: usize) -> Vec<TrackInfo> {
        (0..n)
            .map(|i| TrackInfo {
                uri: format!("spotify:track:{i:022}"),
                name: format!("Song {i}"),
                artists: vec![ArtistRef { name: "A".into(), uri: "spotify:artist:x".into() }],
                album: "Album".into(),
                album_uri: String::new(),
                cover_small: Some("https://i.scdn.co/image/00".into()),
                cover_large: None,
                duration_ms: 1000 + i as u32,
                playable: i % 2 == 0,
                is_local: false,
                added_at: 0,
            })
            .collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("truffle-test-{tag}-{}-{}", std::process::id(), now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn track_cache_round_trip() {
        let dir = temp_dir("tracks");
        let source = Source::Playlist("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into());
        let path = track_cache_path(&dir, &source);
        assert!(path.ends_with("spotify_playlist_37i9dQZF1DXcBWIGoYBM5M.json"));
        let tracks = sample(3);
        write_track_cache(&path, "abcd", "me", "", &tracks).unwrap();
        let back = read_track_cache(&path).unwrap();
        assert_eq!(back.revision, "abcd");
        assert_eq!(back.username, "me");
        assert_eq!(back.tracks, tracks);
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(raw["saved_at"].as_u64().is_some_and(|t| t > 0));
        assert!(raw.get("sync_token").is_none(), "only Liked Songs keep a sync token");
        assert_eq!(back.sync_token, "");
        write_track_cache(&path, "abcd", "me", "sync-1", &tracks).unwrap();
        assert_eq!(read_track_cache(&path).unwrap().sync_token, "sync-1");
        // Corrupt files are ignored, not fatal.
        std::fs::write(&path, b"{not json").unwrap();
        assert!(read_track_cache(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn liked_songs_saved_by_older_truffles_still_load() {
        let dir = temp_dir("old-liked");
        let path = track_cache_path(&dir, &Source::LikedSongs);
        // Before usernames and sync tokens were kept.
        std::fs::write(&path, r#"{"revision":"webapi:2:x:y","saved_at":1,"tracks":[{"uri":"spotify:track:a","name":"A 🎵","artists":[],"album":"","album_uri":"","cover_small":null,"cover_large":null,"duration_ms":1,"playable":true,"is_local":false}]}"#).unwrap();
        let back = read_track_cache(&path).unwrap();
        assert_eq!((back.username.as_str(), back.sync_token.as_str()), ("", ""));
        assert_eq!(back.tracks.len(), 1);
        assert_eq!(back.tracks[0].added_at, 0);
        // An empty list.
        std::fs::write(&path, r#"{"revision":"","tracks":[]}"#).unwrap();
        assert!(read_track_cache(&path).unwrap().tracks.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlists_cache_round_trip() {
        let dir = temp_dir("playlists");
        let path = playlists_cache_path(&dir);
        let mut file = PlaylistsCacheFile { username: "me".into(), revision: "01".into(), ..Default::default() };
        file.owners.insert("spotify".into(), "Spotify".into());
        file.playlists.push(PlaylistSummary {
            uri: "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".into(),
            name: "Today's Top Hits".into(),
            description: String::new(),
            owner_name: "Spotify".into(),
            image_url: None,
            track_count: Some(50),
            revision: "ff".into(),
            editable: false,
            owned: false,
            owner_id: String::new(),
        });
        write_playlists_cache(&path, &file).unwrap();
        let back = read_playlists_cache(&path).unwrap();
        assert_eq!(back.playlists, file.playlists);
        assert_eq!(back.owners, file.owners);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lru_evicts_oldest() {
        let mut lru = Lru::default();
        let list = Arc::new(sample(1));
        for i in 0..LRU_CAPACITY + 1 {
            lru.put(CachedList { key: i.to_string(), revision: String::new(), username: String::new(), sync_token: String::new(), tracks: list.clone() });
        }
        assert!(lru.get("0").is_none());
        assert!(lru.get("1").is_some());
        lru.put(CachedList { key: "new".into(), revision: String::new(), username: String::new(), sync_token: String::new(), tracks: list });
        // "1" was touched, so "2" is now the oldest.
        assert!(lru.get("2").is_none());
        assert!(lru.get("1").is_some());
    }
}
