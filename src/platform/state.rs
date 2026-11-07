//! Pure helpers shared by the platform surfaces: a compact "now playing" snapshot, change
//! detection (new track, play/pause, seek) and text fitting for the tray and Discord.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{ArtistRef, PlayStatus, PlaybackState, TrackInfo};

/// How far the position may drift from where it should be before it counts as a seek.
pub const JUMP_TOLERANCE_MS: i64 = 2_000;

/// Playback status as the platform surfaces show it (`Loading` is folded into the previous one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Stopped,
    Paused,
    Playing,
}

/// The parts of a track the platform surfaces show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: Option<String>,
    pub duration_ms: u32,
}

impl NowPlaying {
    pub fn from_track(track: &TrackInfo) -> Self {
        Self {
            uri: track.uri.clone(),
            title: track.name.clone(),
            artist: track.artist_line(),
            album: track.album.clone(),
            cover: track.cover_large.clone(),
            duration_ms: track.duration_ms,
        }
    }

    /// Whether `track` would show exactly like this snapshot. Allocation-free (runs every frame).
    pub fn matches(&self, track: &TrackInfo) -> bool {
        self.uri == track.uri
            && self.title == track.name
            && self.album == track.album
            && self.cover.as_deref() == track.cover_large.as_deref()
            && self.duration_ms == track.duration_ms
            && artist_line_eq(&self.artist, &track.artists)
    }
}

/// `line == TrackInfo::artist_line()` for these artists, without building the line.
pub fn artist_line_eq(line: &str, artists: &[ArtistRef]) -> bool {
    let mut rest = line;
    for (i, artist) in artists.iter().enumerate() {
        if i > 0 {
            match rest.strip_prefix(", ") {
                Some(r) => rest = r,
                None => return false,
            }
        }
        match rest.strip_prefix(artist.name.as_str()) {
            Some(r) => rest = r,
            None => return false,
        }
    }
    rest.is_empty()
}

/// What changed since the last observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Changes {
    pub track: bool,
    pub status: bool,
    /// The position jumped (seek, or playback resumed somewhere else).
    pub position: bool,
}

impl Changes {
    pub fn any(&self) -> bool {
        self.track || self.status || self.position
    }
}

/// The last state reported to the platform surfaces, plus the latest raw position.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    pub track: Option<Arc<NowPlaying>>,
    pub status: Status,
    /// Position (ms) at the last reported change.
    pub anchor_position_ms: u32,
    /// Unix time (ms) at which the track would have been at 0:00, at the last reported change.
    pub anchor_started_unix_ms: i64,
    /// Latest position (ms) seen, updated on every observation (for relative seeks).
    pub latest_position_ms: u32,
}

impl Tracker {
    /// Fold a new playback state in. Cheap when nothing changed: no allocation.
    pub fn observe(&mut self, playback: &PlaybackState, now_unix_ms: i64) -> Changes {
        let track_changed = match (&self.track, &playback.track) {
            (Some(shown), Some(track)) => !shown.matches(track),
            (None, None) => false,
            _ => true,
        };
        if track_changed {
            self.track = playback.track.as_ref().map(|t| Arc::new(NowPlaying::from_track(t)));
        }

        let loading = playback.status == PlayStatus::Loading;
        let status = if playback.track.is_none() {
            Status::Stopped
        } else {
            match playback.status {
                PlayStatus::Playing => Status::Playing,
                PlayStatus::Paused => Status::Paused,
                PlayStatus::Stopped => Status::Stopped,
                // Keep showing what was shown until the track actually starts or stops.
                PlayStatus::Loading => self.status,
            }
        };
        let status_changed = status != self.status;
        self.status = status;

        let position = playback.current_position_ms();
        self.latest_position_ms = position;
        let started = started_unix_ms(now_unix_ms, position);
        let duration = playback.track.as_ref().map_or(0, |t| t.duration_ms);

        let position_changed = !track_changed
            && !status_changed
            && !loading
            && match status {
                // At the very end the position is clamped, so the implied start drifts: ignore.
                Status::Playing => {
                    (duration == 0 || position < duration)
                        && (started - self.anchor_started_unix_ms).abs() > JUMP_TOLERANCE_MS
                }
                Status::Paused => (i64::from(position) - i64::from(self.anchor_position_ms)).abs() > JUMP_TOLERANCE_MS,
                Status::Stopped => false,
            };

        let changes = Changes { track: track_changed, status: status_changed, position: position_changed };
        if changes.any() {
            self.anchor_position_ms = position;
            self.anchor_started_unix_ms = started;
        }
        changes
    }
}

/// Current Unix time in milliseconds.
pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Unix time (ms) at which a track now at `position_ms` started.
pub fn started_unix_ms(now_unix_ms: i64, position_ms: u32) -> i64 {
    now_unix_ms.saturating_sub(i64::from(position_ms))
}

/// Discord timestamps (start, end) in Unix ms for a track that started at `started_unix_ms`.
/// No end (and so no progress bar) when the duration is unknown.
pub fn timestamps(started_unix_ms: i64, duration_ms: u32) -> (i64, Option<i64>) {
    let end = (duration_ms > 0).then(|| started_unix_ms.saturating_add(i64::from(duration_ms)));
    (started_unix_ms, end)
}

/// Target of a relative seek, clamped to the track.
pub fn seek_target(position_ms: u32, delta_ms: i64, duration_ms: u32) -> u32 {
    let target = i64::from(position_ms).saturating_add(delta_ms).max(0);
    let target = if duration_ms > 0 { target.min(i64::from(duration_ms)) } else { target };
    u32::try_from(target).unwrap_or(u32::MAX)
}

/// Number of UTF-16 code units (how Windows and Discord count string length).
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Cut `s` to at most `max_units` UTF-16 code units, ending with "…" when it was cut.
pub fn truncate_utf16(s: &str, max_units: usize) -> String {
    if utf16_len(s) <= max_units {
        return s.to_owned();
    }
    let budget = max_units.saturating_sub(1);
    let mut used = 0;
    let mut out = String::with_capacity(s.len().min(max_units * 2));
    for c in s.chars() {
        if used + c.len_utf16() > budget {
            break;
        }
        used += c.len_utf16();
        out.push(c);
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    if max_units > 0 {
        out.push('…');
    }
    out
}

/// Tray tooltips hold 127 characters plus the terminator.
const TOOLTIP_MAX: usize = 127;

/// "Truffle — <song> · <artist>", or just "Truffle" when nothing is loaded.
pub fn tray_tooltip(track: Option<&NowPlaying>) -> String {
    let Some(track) = track else { return "Truffle".to_owned() };
    let text = if track.artist.is_empty() {
        format!("Truffle — {}", track.title)
    } else {
        format!("Truffle — {} · {}", track.title, track.artist)
    };
    truncate_utf16(&text, TOOLTIP_MAX)
}

/// A menu label that shows `text` literally: '&' would otherwise underline the next letter and a
/// tab would start an accelerator column.
pub fn menu_label(text: &str, max_units: usize) -> String {
    truncate_utf16(&text.replace(['\t', '\r', '\n'], " "), max_units).replace('&', "&&")
}

/// The tray menu's (disabled) first line.
pub fn now_playing_label(track: Option<&NowPlaying>, status: Status) -> String {
    match track {
        None => "Nothing playing".to_owned(),
        Some(t) => {
            let line = if t.artist.is_empty() { t.title.clone() } else { format!("{} — {}", t.title, t.artist) };
            let line = if status == Status::Paused { format!("{line} (paused)") } else { line };
            menu_label(&line, 64)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn track(uri: &str, name: &str, artists: &[&str], duration_ms: u32) -> TrackInfo {
        TrackInfo {
            uri: uri.to_owned(),
            name: name.to_owned(),
            artists: artists.iter().map(|a| ArtistRef { name: (*a).to_owned(), uri: String::new() }).collect(),
            album: "Album".to_owned(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: Some("https://i.scdn.co/image/x".to_owned()),
            duration_ms,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    fn playing(t: &TrackInfo, position_ms: u32, at: Instant) -> PlaybackState {
        PlaybackState {
            status: PlayStatus::Playing,
            track: Some(t.clone()),
            position_ms,
            position_at: Some(at),
            ..Default::default()
        }
    }

    #[test]
    fn artist_line_comparison_matches_artist_line() {
        let t = track("u", "n", &["A", "B, C", "D"], 1);
        assert!(artist_line_eq(&t.artist_line(), &t.artists));
        assert!(!artist_line_eq("A, B, C", &t.artists));
        assert!(!artist_line_eq("A, B, C, D, E", &t.artists));
        assert!(artist_line_eq("", &[]));
        assert!(!artist_line_eq("A", &[]));
    }

    #[test]
    fn detects_track_status_and_seek_changes_only() {
        let mut tracker = Tracker::default();
        let t = track("spotify:track:1", "Song", &["Artist"], 200_000);
        let now = 1_000_000_000_000;
        let at = Instant::now();

        let c = tracker.observe(&playing(&t, 10_000, at), now);
        assert!(c.track && c.status);
        assert_eq!(tracker.status, Status::Playing);

        // Same state a frame later: nothing to send.
        assert!(!tracker.observe(&playing(&t, 10_000, at), now + 16).any());

        // The controller re-anchors a second later at the right position: still nothing.
        let later = at + Duration::from_secs(1);
        let c = tracker.observe(&playing(&t, 11_000, later), now + 1_000);
        assert!(!c.any(), "{c:?}");

        // Seek forward 30 s.
        let c = tracker.observe(&playing(&t, 41_000, later), now + 1_000);
        assert_eq!(c, Changes { track: false, status: false, position: true });

        // Pause.
        let mut paused = playing(&t, 41_500, later);
        paused.status = PlayStatus::Paused;
        let c = tracker.observe(&paused, now + 1_500);
        assert!(c.status && !c.position);

        // Loading keeps the previous status and never reports a seek.
        let mut loading = paused.clone();
        loading.status = PlayStatus::Loading;
        loading.position_ms = 0;
        assert!(!tracker.observe(&loading, now + 9_000).any());
        assert_eq!(tracker.status, Status::Paused);

        // Seeking while paused.
        paused.position_ms = 100_000;
        assert!(tracker.observe(&paused, now + 9_500).position);

        // New track, then nothing loaded.
        let t2 = track("spotify:track:2", "Other", &["Artist"], 100_000);
        assert!(tracker.observe(&playing(&t2, 0, later), now + 10_000).track);
        let c = tracker.observe(&PlaybackState::default(), now + 11_000);
        assert!(c.track && c.status);
        assert_eq!(tracker.status, Status::Stopped);
        assert!(tracker.track.is_none());
    }

    #[test]
    fn metadata_update_for_the_same_uri_counts_as_a_track_change() {
        let mut tracker = Tracker::default();
        let mut t = track("spotify:track:1", "Song", &["Artist"], 200_000);
        let at = Instant::now();
        tracker.observe(&playing(&t, 0, at), 0);
        t.cover_large = Some("https://i.scdn.co/image/y".to_owned());
        assert!(tracker.observe(&playing(&t, 0, at), 0).track);
    }

    #[test]
    fn timestamp_math() {
        assert_eq!(started_unix_ms(10_000, 2_500), 7_500);
        assert_eq!(timestamps(7_500, 180_000), (7_500, Some(187_500)));
        assert_eq!(timestamps(7_500, 0), (7_500, None));
        assert_eq!(seek_target(5_000, -10_000, 60_000), 0);
        assert_eq!(seek_target(55_000, 10_000, 60_000), 60_000);
        assert_eq!(seek_target(5_000, 10_000, 0), 15_000);
    }

    #[test]
    fn text_fitting() {
        assert_eq!(truncate_utf16("short", 10), "short");
        assert_eq!(truncate_utf16("abcdefghij", 5), "abcd…");
        assert_eq!(utf16_len(&truncate_utf16(&"😀".repeat(100), 127)), 127);
        assert_eq!(menu_label("Simon & Garfunkel\tLive", 64), "Simon && Garfunkel Live");

        let np = NowPlaying {
            uri: "u".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            cover: None,
            duration_ms: 1,
        };
        assert_eq!(tray_tooltip(Some(&np)), "Truffle — Song · Artist");
        assert_eq!(tray_tooltip(None), "Truffle");
        let long = NowPlaying { title: "x".repeat(400), ..np.clone() };
        assert!(utf16_len(&tray_tooltip(Some(&long))) <= 127);
        assert_eq!(now_playing_label(Some(&np), Status::Paused), "Song — Artist (paused)");
        assert_eq!(now_playing_label(None, Status::Stopped), "Nothing playing");
    }
}
