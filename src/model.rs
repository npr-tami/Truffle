//! Shared data types passed between the UI, the controller and the backend modules.
//! This file is the contract between modules: change it only in coordination.

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtistRef {
    pub name: String,
    /// `spotify:artist:...`, empty if unknown.
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    /// `spotify:track:...` (or `spotify:episode:...`). Local files use `spotify:local:...`.
    pub uri: String,
    pub name: String,
    pub artists: Vec<ArtistRef>,
    pub album: String,
    pub album_uri: String,
    /// Smallest cover that is at least ~64px, for list rows.
    pub cover_small: Option<String>,
    /// Large cover (~300px+), for the now-playing area.
    pub cover_large: Option<String>,
    pub duration_ms: u32,
    /// False for local files, region-blocked or removed tracks. Skipped by the queue.
    pub playable: bool,
    pub is_local: bool,
    /// When it was added to the list it is in (Unix seconds), 0 when unknown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub added_at: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl TrackInfo {
    pub fn artist_line(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Primary artist key used by the "no same artist twice in a row" rule.
    pub fn primary_artist_key(&self) -> &str {
        self.artists
            .first()
            .map(|a| if a.uri.is_empty() { a.name.as_str() } else { a.uri.as_str() })
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    /// `spotify:playlist:...`
    pub uri: String,
    pub name: String,
    pub description: String,
    pub owner_name: String,
    pub image_url: Option<String>,
    pub track_count: Option<u32>,
    /// Revision / snapshot id; changes whenever the playlist contents change. Used as cache key.
    pub revision: String,
    /// The logged-in user owns it or can collaborate (rename, cover, add/remove allowed).
    pub editable: bool,
    /// The logged-in user created it (not just followed or collaborating).
    #[serde(default)]
    pub owned: bool,
    /// The owner's Spotify username (for their profile), empty if unknown.
    #[serde(default)]
    pub owner_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    Playlist(String),
    LikedSongs,
    /// An album, opened from search or a song.
    Album(String),
    /// An artist's top songs, opened from search or a song.
    Artist(String),
    /// One of Truffle's own mixes, made from Liked Songs and play counts (`library::MIX_*`).
    Mix(String),
}

impl Source {
    pub fn cache_key(&self) -> String {
        match self {
            Source::Playlist(uri) => uri.replace(':', "_"),
            Source::LikedSongs => "liked_songs".to_owned(),
            Source::Album(uri) | Source::Artist(uri) => uri.replace(':', "_"),
            Source::Mix(id) => format!("mix_{}", id.replace(':', "_")),
        }
    }
}

/// An album, artist, playlist or person in search results or on an artist's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub uri: String,
    pub name: String,
    /// "Album · Artist", "Artist", "Playlist · Owner"; a person found by a search: their id.
    pub subtitle: String,
    pub image: Option<String>,
}

/// A user's profile: name, picture, follower counts, public playlists and recently played artists.
#[derive(Debug, Clone, Default)]
pub struct UserPage {
    pub username: String,
    pub name: String,
    pub image: Option<String>,
    pub followers: Option<u32>,
    pub following: Option<u32>,
    pub playlists_total: Option<u32>,
    pub playlists: Vec<Card>,
    pub artists: Vec<Card>,
    /// The logged-in user follows them (`None`: unknown).
    pub is_following: Option<bool>,
    /// They can be followed (Spotify's own setting).
    pub allow_follows: bool,
}

/// Likes and unlikes of the logged-in account still on their way to Spotify.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd)]
pub struct LikesWaiting {
    pub likes: usize,
    pub unlikes: usize,
}

impl LikesWaiting {
    pub fn total(&self) -> usize {
        self.likes + self.unlikes
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// One artist's songs in Liked Songs (the playable ones) and the releases they are on.
#[derive(Debug, Clone, Default)]
pub struct ArtistLikes {
    pub artist: String,
    pub songs: usize,
    /// Each release with how many of its songs are liked, most first.
    pub albums: Vec<(Card, usize)>,
}

/// The people (and artists) a user follows (`followers` false), or who follow them.
#[derive(Debug, Clone)]
pub struct People {
    pub username: String,
    pub followers: bool,
    pub list: Vec<Card>,
}

/// One page of search results.
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub query: String,
    pub offset: u32,
    pub tracks: Vec<TrackInfo>,
    pub albums: Vec<Card>,
    pub artists: Vec<Card>,
    pub playlists: Vec<Card>,
    /// People (Spotify users), from a search for them only (Command::SearchUsers).
    pub users: Vec<Card>,
    /// Another page of songs exists.
    pub more: bool,
    /// Where the next page of people starts, when there is one.
    pub users_next: Option<u32>,
}

/// Name, picture and (for artists) releases of an album or artist page.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub source: Source,
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    pub albums: Vec<Card>,
    /// An album's artists (each one opens its page).
    pub artists: Vec<ArtistRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserProfile {
    pub username: String,
    pub display_name: String,
    pub image_url: Option<String>,
    /// A Premium account (`None`: not known). Free accounts play songs from YouTube.
    #[serde(default)]
    pub premium: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

/// User-tunable shuffle behaviour. Shuffle is always a uniform Fisher-Yates shuffle;
/// these rules only post-process or weight it when switched on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShuffleRules {
    /// Avoid the same primary artist twice in a row when possible.
    pub no_artist_repeat: bool,
    /// Weight towards tracks with low local play counts (weighted random order).
    pub favour_rarely_played: bool,
    /// Skip tracks the user has blocked.
    pub skip_blocked: bool,
    /// Never repeat a track until the whole list has been played (persisted across restarts).
    pub no_repeat_until_exhausted: bool,
    /// True shuffle: every order equally likely. The rules that shape the order (rounds, artist
    /// spread, rarely played first) are set aside meanwhile but kept for when this goes off;
    /// blocked songs are still skipped while that rule is on.
    pub truly_random: bool,
}

impl ShuffleRules {
    /// The rules the shuffle actually follows.
    pub fn effective(&self) -> Self {
        if !self.truly_random {
            return self.clone();
        }
        Self {
            no_artist_repeat: false,
            favour_rarely_played: false,
            skip_blocked: self.skip_blocked,
            no_repeat_until_exhausted: false,
            truly_random: true,
        }
    }

    /// Whether lists have rounds ("47 of 312 heard"): only while songs aren't repeated before
    /// the whole list has played.
    pub fn rounds_on(&self) -> bool {
        self.effective().no_repeat_until_exhausted
    }
}

impl Default for ShuffleRules {
    fn default() -> Self {
        Self {
            no_artist_repeat: false,
            favour_rarely_played: false,
            skip_blocked: true,
            no_repeat_until_exhausted: true,
            truly_random: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayStatus {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Default)]
pub struct PlaybackState {
    pub status: PlayStatus,
    pub track: Option<TrackInfo>,
    /// Position at `position_at`. While playing, the UI extrapolates from it.
    pub position_ms: u32,
    pub position_at: Option<Instant>,
    /// 0..=100
    pub volume: u8,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub source: Option<Source>,
    /// Index of the current track within the play order, and the order length.
    pub queue_pos: usize,
    pub queue_len: usize,
}

impl PlaybackState {
    pub fn current_position_ms(&self) -> u32 {
        match (self.status, self.position_at) {
            (PlayStatus::Playing, Some(at)) => {
                let p = self.position_ms as u128 + at.elapsed().as_millis();
                let max = self.track.as_ref().map(|t| t.duration_ms as u128).unwrap_or(p);
                p.min(max) as u32
            }
            _ => self.position_ms,
        }
    }
}

/// One line of a song's lyrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub start_ms: u32,
    pub words: String,
}

/// An album or an artist in the user's collection: the albums and artists of their Liked Songs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionItem {
    pub uri: String,
    pub name: String,
    /// An album's artist; empty for an artist.
    pub sub: String,
    pub image: Option<String>,
    /// Liked songs on the album, or by the artist.
    pub songs: usize,
    /// Where its newest liked song is in Liked Songs (0: the song liked last).
    pub newest: usize,
}

/// One of Truffle's own mixes (Home's For you).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub image: Option<String>,
    pub songs: usize,
}

/// A recent album or single by one of the user's most liked artists (Home's New releases).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub uri: String,
    pub name: String,
    pub artist: String,
    pub image: Option<String>,
    /// "Album" or "Single".
    pub kind: &'static str,
    /// How long ago it came out.
    pub days_ago: i64,
}

/// The Albums and Artists pages' contents, newest first.
#[derive(Debug, Clone, Default)]
pub struct Collection {
    pub albums: Vec<CollectionItem>,
    pub artists: Vec<CollectionItem>,
}

/// Whom a listener follows: someone in Spotify's friend activity (their username), or a Truffle
/// host's listen-along channel (its playlist uri).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ListenHow {
    Friend(String),
    Channel(String),
}

/// Listening along with someone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenStatus {
    pub name: String,
    pub how: ListenHow,
}

/// Someone to listen along with (the Friends tab).
#[derive(Debug, Clone, PartialEq)]
pub struct Friend {
    pub how: ListenHow,
    pub name: String,
    pub image: Option<String>,
    /// Their song and its artists (empty when nothing is known).
    pub song: String,
    pub artist: String,
    /// Playing right now.
    pub live: bool,
    /// When their song started (ms since 1970), 0 when unknown.
    pub at_ms: i64,
}

/// One friend's listen-along link, as the Friends tab shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkView {
    pub id: u32,
    /// Who it is for (only on this PC).
    pub name: String,
    /// The link to send them (`None` while it is being made, or once it stopped working).
    pub link: Option<String>,
    pub state: LinkState,
    /// When it was made (ms since 1970; 0 when unknown).
    pub made_ms: i64,
    /// When its friend was last seen listening (`None`: not known; nothing tells yet).
    pub here_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Its playlist is being made, or its first words aren't on Spotify yet.
    Making,
    /// The friend can listen along.
    Ready,
    /// The friend hears nothing (the link said stopped, once).
    Kicked,
    /// Spotify says it is gone: it can be made anew or removed.
    Gone,
}

/// "Your Truffle" on Settings > About: what the play counts add up to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayStats {
    /// Plays of every song together.
    pub plays: u64,
    /// Songs played at least once.
    pub songs: usize,
    /// The most played song (`None` before the first play).
    pub top: Option<TopSong>,
}

/// The most played song: its uri and plays, and its name and cover once known.
#[derive(Debug, Clone, PartialEq)]
pub struct TopSong {
    pub uri: String,
    pub plays: u32,
    pub track: Option<TrackInfo>,
}

impl PlayStats {
    /// From Truffle's play counts (song uri -> plays). Of songs played equally often, the one whose
    /// uri sorts first is the top song, so it is the same one every time.
    pub fn from_counts(counts: &std::collections::HashMap<String, u32>) -> Self {
        let mut stats = Self::default();
        let mut top: Option<(&str, u32)> = None;
        for (uri, &plays) in counts.iter().filter(|(_, n)| **n > 0) {
            stats.plays += u64::from(plays);
            stats.songs += 1;
            if top.is_none_or(|(u, most)| plays > most || (plays == most && uri.as_str() < u)) {
                top = Some((uri, plays));
            }
        }
        stats.top = top.map(|(uri, plays)| TopSong { uri: uri.to_owned(), plays, track: None });
        stats
    }
}

/// Songs asked to go into a playlist that has some of them already. Nothing was added: the user
/// picks what goes in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlreadyIn {
    pub playlist_uri: String,
    /// The songs as asked (what "Add all" adds).
    pub uris: Vec<String>,
    /// The songs it doesn't have, each once, in the order asked (what "Only new ones" adds).
    pub new: Vec<String>,
    /// Different songs asked for.
    pub songs: usize,
    /// Of those, the ones it has.
    pub already: usize,
}

impl AlreadyIn {
    /// Which of the songs asked for (`uris`, local files already left out) a playlist holding
    /// `have` has. `None` when it has none of them, or only ones from `again` (the user was told it
    /// has those, and said to add them anyway): they can go in at once.
    pub fn find(playlist_uri: &str, uris: &[String], have: &std::collections::HashSet<String>, again: &[String]) -> Option<Self> {
        let mut seen = std::collections::HashSet::new();
        let (mut songs, mut already, mut new, mut ask) = (0, 0, Vec::new(), false);
        for uri in uris.iter().filter(|u| seen.insert(u.as_str())) {
            songs += 1;
            if have.contains(uri) {
                already += 1;
                ask |= !again.contains(uri);
            } else {
                new.push(uri.clone());
            }
        }
        ask.then(|| Self { playlist_uri: playlist_uri.to_owned(), uris: uris.to_vec(), new, songs, already })
    }
}

/// The programs YouTube sound needs, for Settings > About: each one found, with the version it
/// gave (empty when it gave none), or `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Helpers {
    pub ytdlp: Option<String>,
    /// The JavaScript runtime's name ("Node.js", "Deno") and version.
    pub js: Option<(String, String)>,
    /// What's missing, in plain words (`None` when both are there).
    pub missing: Option<String>,
}

/// Requests from the UI to the controller.
#[derive(Debug, Clone)]
pub enum Command {
    /// "Your Truffle" on Settings > About (Event::PlayStats answers, a second time once the most
    /// played song's name is looked up).
    PlayStats,
    /// The YouTube helpers' versions, for Settings > About (Event::Helpers answers).
    Helpers,
    /// Fetch the lyrics of a song (Event::Lyrics answers).
    Lyrics(String),
    /// Fetch a song's Canvas, its looping video (Event::Canvas answers).
    Canvas(String),
    /// Which of the user's playlists hold every one of these songs, from the lists Truffle has (no
    /// network): the Add to playlist menu marks them Added.
    HeldBy(Vec<String>),
    /// The albums and artists of Liked Songs (Event::Collection answers, twice: then with the
    /// artists' pictures).
    Collection,
    /// Play this song after the current one: first in line (`next`), or after the songs queued.
    Queue { track: Box<TrackInfo>, next: bool },
    /// A user's profile page (Event::UserPage answers).
    UserPage(String),
    /// An artist's songs in Liked Songs (Event::ArtistLikes answers).
    ArtistLikes(String),
    /// Like (or unlike) many songs at once.
    SetLikedMany { uris: Vec<String>, liked: bool },
    /// Follow (or unfollow) a user (Event::FollowUserDone answers).
    FollowUser { username: String, follow: bool },
    /// The YouTube videos that may be this song (Event::YouTubeMatches answers).
    YouTubeMatches(Box<TrackInfo>),
    /// Play this YouTube video for this song from now on.
    ChooseYouTube { track: Box<TrackInfo>, video_id: String },
    /// Who a user follows, or who follows them (Event::People answers).
    People { username: String, followers: bool },
    /// Follow (or unfollow) an artist; `generation` comes back in Event::FollowDone.
    Follow { uri: String, follow: bool, generation: u64 },
    /// Take the queued song at this place out (`None`: all of them).
    Unqueue(Option<usize>),
    /// Log in with cached credentials, or open the browser if `interactive`.
    Login { interactive: bool },
    Logout,
    RefreshLibrary,
    /// Load (from cache or network) the tracks of a source so the UI can show them.
    OpenSource { source: Source, force_refresh: bool },
    /// Start playing a source. `start_index` is an index into the source's track list.
    /// With `shuffle`, a new random order is generated (the start track, if given, plays first).
    PlaySource { source: Source, start_index: Option<usize>, shuffle: bool },
    /// Start a list over: forget which of its songs were heard this round, then play it in a fresh
    /// random order.
    NewRound { source: Source },
    PlayPause,
    Next,
    Previous,
    SeekMs(u32),
    /// 0..=100
    SetVolume(u8),
    SetShuffle(bool),
    SetRepeat(RepeatMode),
    SetShuffleRules(ShuffleRules),
    /// Re-shuffle the remaining queue with the current rules (keeps the current track).
    Reshuffle,
    /// Add/remove a track URI from the blocked set.
    SetBlocked { uri: String, blocked: bool },
    SetLiked { uri: String, liked: bool },
    /// Web API (optional): log in with the user's own developer client id.
    ConnectWebApi { client_id: String },
    /// Stop waiting for a browser sign-in started with `ConnectWebApi` (keeps any existing
    /// connection and the saved client id).
    CancelWebApiConnect,
    DisconnectWebApi,
    UpdatePlaylistDetails { uri: String, name: Option<String>, description: Option<String> },
    /// Path to a JPEG/PNG/WebP image; it is resized/re-encoded to a JPEG under 256 KB.
    SetPlaylistCover { uri: String, image_path: std::path::PathBuf },
    /// Apply new equalizer settings immediately (and persist them).
    SetEqualizer(EqSettings),
    /// Crossfade length in seconds between consecutive tracks (0 = off, gapless).
    SetCrossfade(u8),
    /// Where songs' sound comes from (Spotify or YouTube).
    SetSoundSource(crate::config::SoundSource),
    /// Start, replace or cancel (`None`) the sleep timer.
    SetSleepTimer(Option<SleepTimer>),
    /// Last.fm: save the user's API key + shared secret and run the browser approval
    /// (auth.getToken -> user approves -> auth.getSession). Replaces any existing connection.
    ConnectLastfm { api_key: String, api_secret: String },
    /// Stop waiting for a browser approval started with `ConnectLastfm`.
    CancelLastfmConnect,
    /// Forget the Last.fm session (keeps the key/secret and any unsent scrobbles).
    DisconnectLastfm,
    /// Pause/resume scrobbling without disconnecting.
    SetLastfmEnabled(bool),
    /// Last.fm: send the unsent plays recorded for another account (`LastfmStatus::held`) to the
    /// connected account `to` after all. Ignored if another account is connected by then.
    SendHeldLastfmScrobbles { to: String },
    /// Last.fm: delete unsent plays without sending them, except those recorded for `keep_for`:
    /// the connected account the UI showed (`None`: none was connected, so all of them). Ignored
    /// if the connection changed since.
    DiscardLastfmScrobbles { keep_for: Option<String> },
    /// Search Spotify (needs the editing connection). `offset` pages the songs, 10 at a time.
    Search { query: String, offset: u32 },
    /// Search Spotify's people (users) only, a page from `offset` (Event::SearchUsers answers).
    SearchUsers { query: String, offset: u32 },
    /// Add songs to the end of one of the user's playlists. It is read first, unless every song is
    /// in `again` (ones the user was told it has, and said to add anyway). If it has some of them
    /// other than those, nothing is added and Event::AlreadyInPlaylist asks the user. `question`:
    /// the one this add answers, asked again if the playlist can't be read.
    AddToPlaylist { playlist_uri: String, uris: Vec<String>, again: Vec<String>, question: Option<AlreadyIn> },
    /// Remove songs (every copy of each) from one of the user's playlists.
    RemoveFromPlaylist { playlist_uri: String, uris: Vec<String> },
    /// Read one of the user's playlists as Spotify has it now, to find the songs in it more than
    /// once (Event::Duplicates answers).
    FindDuplicates(String),
    /// Take rows out of a playlist read with `FindDuplicates`: `tracks` is the playlist as read,
    /// `rows` places in it. Only those copies go, also if the playlist changed meanwhile, or none
    /// when Truffle can't tell which they are.
    RemoveDuplicates { playlist_uri: String, tracks: Arc<Vec<TrackInfo>>, rows: Vec<usize> },
    /// Make a new, empty private playlist.
    CreatePlaylist { name: String },
    /// Remove a playlist from the library (unfollow; for the owner this is Spotify's delete).
    RemovePlaylist { uri: String },
    /// Listen along with someone (`name` shows while following).
    ListenAlong { how: ListenHow, name: String },
    /// Stop listening along: the user's own music comes back, paused.
    ListenStop,
    /// Let friends listen along (or not).
    ListenShare(bool),
    /// Join with a pasted listen-along link.
    ListenJoin(String),
    /// Forget a remembered Truffle host (by their channel's uri).
    ListenForget(String),
    /// Make a listen-along link for one friend (`name` stays on this PC).
    ListenNewLink { name: String },
    /// Give a link another name (only on this PC).
    ListenRenameLink { id: u32, name: String },
    /// Kick the friend with this link (they hear nothing), or let them back in (`kick` false).
    ListenKick { id: u32, kick: bool },
    /// Take a link out for good (its playlist leaves the library once it says stopped).
    ListenRemoveLink(u32),
    /// A new playlist for a link that stopped working (same name; the friend needs it again).
    ListenRemakeLink(u32),
    /// The people to listen along with (Event::Friends answers).
    Friends,
    /// What one user plays now (Event::FriendPeek answers).
    FriendPeek(String),
    /// Persist settings and shut down cleanly.
    Shutdown,
}

/// Last.fm connection state for the UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastfmStatus {
    /// A session key is stored and accepted.
    pub connected: bool,
    /// Waiting for the user to approve Truffle in the browser.
    pub connecting: bool,
    /// The Last.fm user name of the saved sign-in (while `connected`, and while `stopped`).
    pub username: Option<String>,
    /// Scrobbles waiting to be sent (offline / errors), `held` ones included.
    pub pending: usize,
    /// Scrobbling switched on in settings.
    pub enabled: bool,
    /// Plain-language note or error for the settings screen.
    pub message: Option<String>,
    /// Last.fm refuses the API key or shared secret while a sign-in is still saved: why, in plain
    /// language. Nothing is sent meanwhile (`connected` is false); Settings offers Connect again,
    /// Disconnect and the scrobbling switch.
    pub stopped: Option<String>,
    /// Of `pending`: plays recorded for another Last.fm account than the saved sign-in (or by an
    /// older Truffle), so they are only sent to it if the user says so. All of them while there
    /// is no sign-in.
    pub held: usize,
    /// The account the `held` plays were recorded for, when that is one known account.
    pub held_for: Option<String>,
}

/// 10-band graphic equalizer. Band centre frequencies are `EQ_BANDS_HZ`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EqSettings {
    pub enabled: bool,
    /// Gain per band in dB, clamped to -12.0..=12.0.
    pub gains_db: [f32; 10],
    /// Overall gain in dB applied before the bands (-12.0..=0.0) to avoid clipping when boosting.
    pub preamp_db: f32,
    /// Name of the preset the gains came from ("Custom" once edited by hand).
    pub preset: String,
}

pub const EQ_BANDS_HZ: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

impl Default for EqSettings {
    fn default() -> Self {
        Self { enabled: false, gains_db: [0.0; 10], preamp_db: 0.0, preset: "Flat".to_owned() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SleepTimer {
    /// Stop after this many minutes (the last ~30 s fade out).
    Minutes(u32),
    /// Stop when the current track ends.
    EndOfTrack,
}

/// What the sleep timer is currently doing, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepTimerState {
    /// Fires at this instant.
    Until(Instant),
    EndOfTrack,
}

/// A YouTube video that may be a song (for "Wrong song?").
#[derive(Debug, Clone, PartialEq)]
pub struct YtMatch {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub duration_ms: u32,
}

/// Notifications from the controller to the UI.
#[derive(Debug, Clone)]
pub enum Event {
    /// An event about one account's library (Spotify username): dropped if another account is
    /// logged in by the time it comes.
    ForAccount { account: String, event: Box<Event> },
    /// "Your Truffle" on Settings > About.
    PlayStats(PlayStats),
    /// The YouTube helpers' versions.
    Helpers(Helpers),
    /// The albums and artists of Liked Songs.
    Collection(Arc<Collection>),
    /// New releases by the artists followed (else the most liked), newest first.
    NewReleases(Arc<Vec<Release>>),
    /// The artists the user follows, followed last first: first bare, then (`named`) with names
    /// and pictures (an artist Spotify wouldn't describe keeps an empty name).
    Following { list: Arc<Vec<CollectionItem>>, named: bool },
    /// A user's profile page.
    UserPage(Arc<UserPage>),
    /// An artist's songs in Liked Songs.
    ArtistLikes(Arc<ArtistLikes>),
    /// A follow (or unfollow) of a user went through (`ok`), or didn't.
    FollowUserDone { username: String, follow: bool, ok: bool },
    /// Where songs' sound should come from (the setting; a free account uses YouTube anyway).
    SoundSource(crate::config::SoundSource),
    /// The YouTube videos that may be a song, and the one it plays now.
    YouTubeMatches { uri: String, current: Option<String>, result: Result<Vec<YtMatch>, String> },
    /// Who a user follows, or who follows them.
    People(Arc<People>),
    /// A profile page or people list couldn't be loaded.
    ProfileFailed { username: String, message: String },
    /// A follow (or unfollow) went through (`ok`), or Spotify refused it.
    FollowDone { uri: String, follow: bool, generation: u64, ok: bool },
    /// Truffle's own mixes.
    Mixes(Arc<Vec<MixInfo>>),
    /// How many of the songs in Up next were queued by the user (they come first).
    Queued(usize),
    /// A song's lyrics (`None`: Spotify has none for it). `synced` lines carry their start time.
    Lyrics { uri: String, lines: Option<Arc<Vec<LyricLine>>>, synced: bool },
    /// A song's Canvas: its video, downloaded (`video`), or a picture to load (`picture`); neither
    /// when it has none or it couldn't be had.
    Canvas { uri: String, video: Option<std::path::PathBuf>, picture: Option<String> },
    /// The answer to [`Command::HeldBy`]: the playlists (uris) holding every one of `uris`.
    HeldBy { uris: Vec<String>, playlists: Vec<String> },
    /// Transient status line text.
    Status(String),
    /// User-facing error message (plain language).
    Error(String),
    /// No cached login; the UI should show the login screen.
    NeedLogin,
    LoggingIn,
    LoggedIn(UserProfile),
    LoggedOut,
    Library(Vec<PlaylistSummary>),
    /// The playlist list couldn't be loaded and there is no saved copy to show.
    LibraryFailed { message: String },
    PlaylistUpdated(PlaylistSummary),
    SourceLoading { source: Source, loaded: usize, total: Option<usize> },
    SourceLoaded { source: Source, tracks: Arc<Vec<TrackInfo>> },
    SourceFailed { source: Source, message: String },
    Playback(PlaybackState),
    BlockedSet(Arc<std::collections::HashSet<String>>),
    LikedSet(Arc<std::collections::HashSet<String>>),
    /// Likes and unlikes waiting for Spotify, for Liked Songs' header (none while they go
    /// straight through).
    LikesWaiting(LikesWaiting),
    WebApi { connected: bool, message: Option<String> },
    /// The next tracks in play order after the current one (a short preview, e.g. up to 50),
    /// for the UI's "Up next" list. Send whenever the queue or the current position changes.
    UpNext(Arc<Vec<TrackInfo>>),
    /// The sleep timer changed (`None` = off). Sent on set, cancel and when it fires.
    SleepTimer(Option<SleepTimerState>),
    /// Last.fm connection/scrobbling state changed.
    Lastfm(LastfmStatus),
    /// The whole play order of the playing source, for the record deck (`None`: nothing queued).
    /// Sent whenever the order or the current position changes.
    Deck(Option<Arc<DeckState>>),
    /// How many songs of each list were heard in its current shuffle round, by
    /// [`Source::cache_key`]. Sent at login and whenever a play is counted or a round restarts.
    RoundProgress(Arc<std::collections::HashMap<String, usize>>),
    SearchResults(Arc<SearchResults>),
    SearchFailed { query: String, message: String },
    /// A page of people found (only `users` and `users_next` are filled).
    SearchUsers(Arc<SearchResults>),
    SearchUsersFailed { query: String, message: String },
    /// Header details of an album or artist page.
    SourceInfo(SourceInfo),
    /// A playlist was removed from the library.
    PlaylistRemoved { uri: String },
    /// Songs asked to go into a playlist that has some of them already: nothing was added yet.
    /// The questions are asked one at a time.
    AlreadyInPlaylist(AlreadyIn),
    /// A playlist as Spotify has it now, for "Find duplicates" (`None`: it couldn't be read; an
    /// `Error` says why).
    Duplicates { uri: String, tracks: Option<Arc<Vec<TrackInfo>>> },
    /// Listening along with someone (`None`: not).
    Listening(Option<ListenStatus>),
    /// Friends may listen along (`on`), and the link older Truffles write to (the first link not
    /// kicked, if any; the Friends tab shows every link from `ListenLinks` instead).
    ListenShare {
        on: bool,
        #[allow(dead_code)]
        link: Option<String>,
    },
    /// The logged-in account's listen-along links, in id order (links being made included).
    ListenLinks(Arc<Vec<LinkView>>),
    /// The people to listen along with, playing ones first.
    Friends(Arc<Vec<Friend>>),
    /// What one user plays now (`None`: nothing Spotify shows).
    FriendPeek { username: String, friend: Option<Friend> },
    /// A test aid (`truffle-selftest.request`, "open <page>"): the middle shows `page` as if it was
    /// clicked, and "opened <page>" goes to `report`.
    OpenPage { page: String, report: std::path::PathBuf },
}

/// The play order of the playing source as the record draws it: heard songs first (the outer
/// groove), then the playing song, then the rest in the order they will play.
#[derive(Debug, Clone)]
pub struct DeckState {
    pub source: Source,
    pub tracks: Arc<Vec<TrackInfo>>,
    /// Indices into `tracks`.
    pub order: Vec<u32>,
    /// Entry of `order` that is playing (or, if the playing song was removed from the playlist,
    /// plays next); the entries before it were heard this round.
    pub now: usize,
    pub shuffled: bool,
}

impl PartialEq for DeckState {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
            && Arc::ptr_eq(&self.tracks, &other.tracks)
            && self.now == other.now
            && self.shuffled == other.shuffled
            && self.order == other.order
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn counts(list: &[(&str, u32)]) -> HashMap<String, u32> {
        list.iter().map(|(uri, n)| ((*uri).to_owned(), *n)).collect()
    }

    #[test]
    fn play_stats_add_up_the_play_counts() {
        let stats = PlayStats::from_counts(&counts(&[("spotify:track:a", 3), ("spotify:track:b", 52), ("spotify:track:c", 1)]));
        assert_eq!((stats.plays, stats.songs), (56, 3));
        let top = stats.top.expect("a top song");
        assert_eq!((top.uri.as_str(), top.plays, top.track), ("spotify:track:b", 52, None));
        // A song counted 0 times wasn't played.
        let stats = PlayStats::from_counts(&counts(&[("spotify:track:a", 0), ("spotify:track:b", 2)]));
        assert_eq!((stats.plays, stats.songs), (2, 1));
        // Nothing played yet.
        assert_eq!(PlayStats::from_counts(&HashMap::new()), PlayStats::default());
    }

    fn uris(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| format!("spotify:track:{id}")).collect()
    }

    fn playlist_with(ids: &[&str]) -> std::collections::HashSet<String> {
        uris(ids).into_iter().collect()
    }

    #[test]
    fn songs_not_in_the_playlist_go_in_without_asking() {
        assert_eq!(AlreadyIn::find("spotify:playlist:p", &uris(&["a", "b", "a"]), &playlist_with(&["c", "d"]), &[]), None);
        assert_eq!(AlreadyIn::find("spotify:playlist:p", &uris(&["a"]), &playlist_with(&[]), &[]), None);
    }

    #[test]
    fn songs_already_in_the_playlist_are_counted_once() {
        // b is picked twice and is there; a is picked twice and isn't; c is new.
        let asked = uris(&["a", "b", "a", "c", "b"]);
        let found = AlreadyIn::find("spotify:playlist:p", &asked, &playlist_with(&["b", "x"]), &[]).expect("b is there");
        assert_eq!(found.playlist_uri, "spotify:playlist:p");
        assert_eq!((found.already, found.songs), (1, 3));
        // "Only new ones": each new song once, in the order asked. "Add all": everything as asked.
        assert_eq!(found.new, uris(&["a", "c"]));
        assert_eq!(found.uris, asked);
    }

    #[test]
    fn songs_all_in_the_playlist_leave_nothing_new() {
        let one = AlreadyIn::find("spotify:playlist:p", &uris(&["a", "a"]), &playlist_with(&["a"]), &[]).expect("a is there");
        assert_eq!((one.already, one.songs, one.new.len()), (1, 1, 0));
        let all = AlreadyIn::find("spotify:playlist:p", &uris(&["a", "b"]), &playlist_with(&["b", "a"]), &[]).expect("both are there");
        assert_eq!((all.already, all.songs, all.new.len()), (2, 2, 0));
    }

    #[test]
    fn only_songs_the_user_said_to_add_again_go_in_without_asking_again() {
        // "Add all" for [a, b] when only a was there: still only a there, so both go in.
        let asked = uris(&["a", "b"]);
        assert_eq!(AlreadyIn::find("spotify:playlist:p", &asked, &playlist_with(&["a"]), &uris(&["a"])), None);
        // b went in meanwhile (from the phone): asked again, with what is there now.
        let found = AlreadyIn::find("spotify:playlist:p", &asked, &playlist_with(&["a", "b"]), &uris(&["a"])).expect("b is there now");
        assert_eq!((found.already, found.songs, found.new.len()), (2, 2, 0));
    }

    #[test]
    fn the_top_song_of_a_tie_is_the_same_every_time() {
        let tied = [("spotify:track:m", 7), ("spotify:track:c", 7), ("spotify:track:x", 7), ("spotify:track:z", 2)];
        // A HashMap's order changes from one map to the next: the answer must not.
        for _ in 0..50 {
            let top = PlayStats::from_counts(&counts(&tied)).top.expect("a top song");
            assert_eq!(top.uri, "spotify:track:c");
        }
    }
}
