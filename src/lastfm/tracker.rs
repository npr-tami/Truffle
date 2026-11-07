//! Play-time accounting for scrobbling. Pure: no I/O and no clock of its own; it is fed playback
//! snapshots plus the time, and says when to send "now playing" and when a play has earned its
//! scrobble.
//!
//! Rules:
//! - A *play* is one load of a track, identified by the controller's play id (which changes for
//!   every new load: next, repeat-one, a picked row, a crossfade, "Previous" restarting the track)
//!   together with the track URI. Reloading the same play (reconnect, interruption) keeps it.
//! - Listening time is wall-clock time while the status is `Playing`, measured between
//!   consecutive observations. Paused, loading and stopped time don't count, so seeking forward
//!   adds nothing and replaying a part counts as heard. A gap between observations longer than
//!   [`MAX_STEP`] (the computer slept, or the controller stalled) counts as nothing: the
//!   controller observes every second while it runs.
//! - A play is eligible when the track is longer than 30 s and at least min(half, 4 min) has been
//!   heard. It is scrobbled at most once, as soon as it is eligible, with the UTC time it started
//!   playing.
//! - Leaving a play (another play id or track, no track) finalises it: the time up to that moment
//!   counts, then it is gone. [`Tracker::finalise`] does the same on purpose (sleep timer,
//!   logout, shutdown); if the same play is resumed afterwards it counts as a new play.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::model::{PlayStatus, TrackInfo};

/// Tracks this long or shorter are never scrobbled.
const MIN_DURATION: Duration = Duration::from_secs(30);
/// Listening this long always makes a (long enough) track eligible.
const MAX_NEEDED: Duration = Duration::from_secs(240);
/// A longer gap between two observations is not counted as listening.
pub const MAX_STEP: Duration = Duration::from_secs(5);
/// "Now playing" is sent again on resume when the last one is at least this old.
pub const NOW_PLAYING_REFRESH: Duration = Duration::from_secs(5 * 60);

/// What Last.fm is told about a track.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrobbleTrack {
    /// Primary artist only.
    pub artist: String,
    pub track: String,
    #[serde(default)]
    pub album: String,
    /// Seconds; 0 if unknown.
    #[serde(default)]
    pub duration_s: u32,
    /// Spotify URI, for logs only.
    #[serde(default)]
    pub uri: String,
}

/// A scrobble waiting to be accepted by Last.fm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingScrobble {
    pub track: ScrobbleTrack,
    /// UTC UNIX seconds when the play started.
    pub timestamp: i64,
    /// Times Last.fm answered "operation failed" (error 8) for it alone.
    #[serde(default)]
    pub attempts: u8,
    /// The Last.fm account it was recorded for (set when it is queued); only sent to that one
    /// unless the user says otherwise. Empty: unknown (queued by an older Truffle).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
}

impl PendingScrobble {
    /// Same play: used to de-duplicate and to find an item again after a send.
    pub fn same_play(&self, other: &PendingScrobble) -> bool {
        self.timestamp == other.timestamp && self.track.artist == other.track.artist && self.track.track == other.track.track
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Tell Last.fm this track is playing now (best effort).
    NowPlaying(ScrobbleTrack),
    /// The play is eligible: queue its scrobble.
    Scrobble(PendingScrobble),
}

/// What gets scrobbled for `t`, or `None` when it shouldn't be: podcast episodes (the show only
/// stands in for an artist) and anything without both an artist and a title.
pub fn scrobble_track(t: &TrackInfo) -> Option<ScrobbleTrack> {
    if t.uri.starts_with("spotify:episode:") {
        return None;
    }
    let artist = t.artists.first()?.name.as_str();
    if artist.trim().is_empty() || t.name.trim().is_empty() {
        return None;
    }
    Some(ScrobbleTrack {
        artist: artist.to_owned(),
        track: t.name.clone(),
        album: t.album.trim().to_owned(),
        duration_s: t.duration_ms / 1000,
        uri: t.uri.clone(),
    })
}

/// Listening needed for a track of `duration`: min(half, 4 min); `None` for 30 s or shorter.
pub fn needed(duration: Duration) -> Option<Duration> {
    (duration > MIN_DURATION).then(|| (duration / 2).min(MAX_NEEDED))
}

struct Play {
    id: u64,
    uri: String,
    /// `None`: never reported to Last.fm.
    track: Option<ScrobbleTrack>,
    /// `None`: never eligible.
    needed: Option<Duration>,
    /// When it first played: the scrobble timestamp.
    started_unix: Option<i64>,
    listened: Duration,
    /// The last observation, while playing.
    last: Option<Instant>,
    scrobbled: bool,
    /// Finalised on purpose; playing again starts a new play.
    closed: bool,
}

impl Play {
    fn new(id: u64, t: &TrackInfo) -> Self {
        let track = scrobble_track(t);
        let needed = track.as_ref().and_then(|_| needed(Duration::from_millis(u64::from(t.duration_ms))));
        Self {
            id,
            uri: t.uri.clone(),
            track,
            needed,
            started_unix: None,
            listened: Duration::ZERO,
            last: None,
            scrobbled: false,
            closed: false,
        }
    }

    /// Count the time since the last observation if it was playing then.
    fn advance(&mut self, now: Instant) {
        if let Some(last) = self.last {
            let step = now.saturating_duration_since(last);
            if step <= MAX_STEP {
                self.listened += step;
            } else {
                log::debug!("not counting {step:?} without observations as listening (the computer was probably asleep)");
            }
            self.last = Some(now);
        }
    }

    fn check(&mut self, out: &mut Vec<Output>) {
        if self.scrobbled || self.closed {
            return;
        }
        let (Some(track), Some(needed), Some(timestamp)) = (&self.track, self.needed, self.started_unix) else { return };
        if self.listened >= needed {
            self.scrobbled = true;
            out.push(Output::Scrobble(PendingScrobble { track: track.clone(), timestamp, attempts: 0, user: String::new() }));
        }
    }
}

#[derive(Default)]
pub struct Tracker {
    play: Option<Play>,
    /// When "now playing" was last produced.
    last_now_playing: Option<Instant>,
}

impl Tracker {
    /// Feed the current playback state. Call on every change and at least every second while
    /// playing. `unix` is the wall-clock time (UTC seconds) at `now`.
    pub fn observe(&mut self, play_id: u64, track: Option<&TrackInfo>, status: PlayStatus, now: Instant, unix: i64) -> Vec<Output> {
        let mut out = Vec::new();
        let playing = status == PlayStatus::Playing;
        let same = match (&self.play, track) {
            (Some(p), Some(t)) => p.id == play_id && p.uri == t.uri,
            _ => false,
        };
        if !same {
            // The previous play ends here (a crossfade hand-over included): its time up to now
            // counts, then it is finished.
            if let Some(mut old) = self.play.take() {
                old.advance(now);
                old.check(&mut out);
            }
            self.play = track.map(|t| Play::new(play_id, t));
        }
        let Some(t) = track else { return out };
        let Some(play) = self.play.as_mut() else { return out };
        if play.closed {
            if !playing {
                return out;
            }
            // Played again after being finalised on purpose (e.g. resumed after the sleep timer).
            *play = Play::new(play_id, t);
        }

        play.advance(now);
        if playing {
            if play.last.is_none() {
                // Starts or resumes now.
                let first = play.started_unix.is_none();
                play.started_unix.get_or_insert(unix);
                if let Some(track) = &play.track {
                    let stale = self.last_now_playing.is_none_or(|at| now.saturating_duration_since(at) >= NOW_PLAYING_REFRESH);
                    if first || stale {
                        out.push(Output::NowPlaying(track.clone()));
                        self.last_now_playing = Some(now);
                    }
                }
            }
            play.last = Some(now);
        } else {
            play.last = None;
        }
        play.check(&mut out);
        out
    }

    /// End the current play now (sleep timer, logout, shutdown): an eligible play that wasn't
    /// scrobbled yet is, an ineligible one is dropped.
    pub fn finalise(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if let Some(play) = self.play.as_mut()
            && !play.closed
        {
            play.advance(now);
            play.last = None;
            play.check(&mut out);
            play.closed = true;
        }
        out
    }

    /// Time heard of the current play (tests and logs).
    #[cfg(test)]
    pub fn listened(&self) -> Option<Duration> {
        self.play.as_ref().map(|p| p.listened)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ArtistRef;

    const T0_UNIX: i64 = 1_760_000_000;

    fn track(uri: &str, secs: u32) -> TrackInfo {
        TrackInfo {
            uri: uri.into(),
            name: format!("Song {uri}"),
            artists: vec![
                ArtistRef { name: "Main Artist".into(), uri: String::new() },
                ArtistRef { name: "Guest".into(), uri: String::new() },
            ],
            album: "Album".into(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: secs * 1000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    /// Drives a tracker the way the controller does: an observation every second.
    struct Sim {
        tracker: Tracker,
        start: Instant,
        at: Duration,
        play_id: u64,
        track: Option<TrackInfo>,
        status: PlayStatus,
        out: Vec<Output>,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                tracker: Tracker::default(),
                start: Instant::now(),
                at: Duration::ZERO,
                play_id: 0,
                track: None,
                status: PlayStatus::Stopped,
                out: Vec::new(),
            }
        }
        fn now(&self) -> Instant {
            self.start + self.at
        }
        fn unix(&self) -> i64 {
            T0_UNIX + self.at.as_secs() as i64
        }
        fn observe(&mut self) {
            let (now, unix) = (self.now(), self.unix());
            let out = self.tracker.observe(self.play_id, self.track.as_ref(), self.status, now, unix);
            self.out.extend(out);
        }
        /// A new play of `t` with `status`.
        fn load(&mut self, t: TrackInfo, status: PlayStatus) {
            self.play_id += 1;
            self.track = Some(t);
            self.status = status;
            self.observe();
        }
        fn set(&mut self, status: PlayStatus) {
            self.status = status;
            self.observe();
        }
        /// Let `secs` pass with an observation every second (ms resolution for odd lengths).
        fn run(&mut self, ms: u64) {
            let mut left = ms;
            while left > 0 {
                let step = left.min(1000);
                self.at += Duration::from_millis(step);
                left -= step;
                self.observe();
            }
        }
        /// Time passes without any observation (the computer sleeps).
        fn sleep(&mut self, secs: u64) {
            self.at += Duration::from_secs(secs);
        }
        fn finalise(&mut self) {
            let now = self.now();
            let out = self.tracker.finalise(now);
            self.out.extend(out);
        }
        fn scrobbles(&self) -> Vec<PendingScrobble> {
            self.out
                .iter()
                .filter_map(|o| match o {
                    Output::Scrobble(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        }
        fn now_playings(&self) -> usize {
            self.out.iter().filter(|o| matches!(o, Output::NowPlaying(_))).count()
        }
    }

    #[test]
    fn thresholds() {
        assert_eq!(needed(Duration::from_secs(30)), None);
        assert_eq!(needed(Duration::from_secs(31)), Some(Duration::from_millis(15_500)));
        assert_eq!(needed(Duration::from_secs(200)), Some(Duration::from_secs(100)));
        assert_eq!(needed(Duration::from_secs(480)), Some(Duration::from_secs(240)));
        assert_eq!(needed(Duration::from_secs(600)), Some(Duration::from_secs(240)));
    }

    #[test]
    fn a_31_second_track_scrobbles_at_half_and_a_30_second_one_never() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 31), PlayStatus::Playing);
        sim.run(15_000);
        assert!(sim.scrobbles().is_empty());
        sim.run(500);
        assert_eq!(sim.scrobbles().len(), 1);
        assert_eq!(sim.scrobbles()[0].timestamp, T0_UNIX);
        sim.run(20_000);
        assert_eq!(sim.scrobbles().len(), 1, "at most once per play");

        let mut sim = Sim::new();
        sim.load(track("spotify:track:b", 30), PlayStatus::Playing);
        sim.run(30_000);
        sim.load(track("spotify:track:c", 30), PlayStatus::Playing);
        assert!(sim.scrobbles().is_empty());
        assert_eq!(sim.now_playings(), 2, "now playing is still sent for short tracks");
    }

    #[test]
    fn a_long_track_scrobbles_after_four_minutes() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 600), PlayStatus::Playing);
        sim.run(239_000);
        assert!(sim.scrobbles().is_empty());
        sim.run(1_000);
        assert_eq!(sim.scrobbles().len(), 1);
    }

    #[test]
    fn the_scrobble_carries_the_primary_artist_and_track_details() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 200), PlayStatus::Playing);
        sim.run(100_000);
        let s = &sim.scrobbles()[0];
        assert_eq!(s.track.artist, "Main Artist");
        assert_eq!(s.track.track, "Song spotify:track:a");
        assert_eq!(s.track.album, "Album");
        assert_eq!(s.track.duration_s, 200);
    }

    #[test]
    fn loading_first_then_playing_starts_the_clock_when_audio_starts() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 200), PlayStatus::Loading);
        sim.run(5_000);
        assert_eq!(sim.now_playings(), 0);
        sim.set(PlayStatus::Playing);
        assert_eq!(sim.now_playings(), 1);
        sim.run(100_000);
        assert_eq!(sim.scrobbles().len(), 1);
        assert_eq!(sim.scrobbles()[0].timestamp, T0_UNIX + 5, "timestamp = when it started playing");
    }

    #[test]
    fn pauses_do_not_count_and_keep_the_timestamp() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 200), PlayStatus::Playing);
        sim.run(60_000);
        sim.set(PlayStatus::Paused);
        sim.run(600_000);
        assert!(sim.scrobbles().is_empty());
        assert_eq!(sim.tracker.listened(), Some(Duration::from_secs(60)));
        sim.set(PlayStatus::Playing);
        sim.run(39_000);
        assert!(sim.scrobbles().is_empty());
        sim.run(1_000);
        assert_eq!(sim.scrobbles().len(), 1);
        assert_eq!(sim.scrobbles()[0].timestamp, T0_UNIX);
    }

    #[test]
    fn seeking_forward_adds_nothing() {
        // Play 5 s, seek to just before the end (only the status flickers through Loading).
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 200), PlayStatus::Playing);
        sim.run(5_000);
        sim.set(PlayStatus::Loading);
        sim.set(PlayStatus::Playing);
        sim.run(3_000);
        // The track ends and the next one starts.
        sim.load(track("spotify:track:b", 200), PlayStatus::Playing);
        assert!(sim.scrobbles().is_empty());
    }

    #[test]
    fn repeat_one_is_a_new_play_with_its_own_timestamp() {
        let mut sim = Sim::new();
        let t = track("spotify:track:a", 100);
        sim.load(t.clone(), PlayStatus::Playing);
        sim.run(100_000);
        sim.load(t.clone(), PlayStatus::Loading);
        sim.set(PlayStatus::Playing);
        sim.run(100_000);
        let s = sim.scrobbles();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].timestamp, T0_UNIX);
        assert_eq!(s[1].timestamp, T0_UNIX + 100);
        assert_eq!(sim.now_playings(), 2);
    }

    #[test]
    fn reloading_the_same_play_continues_it() {
        // A reconnect reloads the track with the same play id: no second scrobble or timestamp.
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 200), PlayStatus::Playing);
        sim.run(60_000);
        sim.set(PlayStatus::Loading);
        sim.run(10_000);
        sim.set(PlayStatus::Playing);
        sim.run(40_000);
        let s = sim.scrobbles();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].timestamp, T0_UNIX);
        assert_eq!(sim.now_playings(), 1, "a quick resume doesn't re-send now playing");
        sim.run(200_000);
        assert_eq!(sim.scrobbles().len(), 1);
    }

    #[test]
    fn now_playing_is_resent_after_a_long_pause() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 600), PlayStatus::Playing);
        sim.run(10_000);
        sim.set(PlayStatus::Paused);
        sim.run(60_000);
        sim.set(PlayStatus::Playing);
        assert_eq!(sim.now_playings(), 1);
        sim.run(10_000);
        sim.set(PlayStatus::Paused);
        sim.run(301_000);
        sim.set(PlayStatus::Playing);
        assert_eq!(sim.now_playings(), 2);
    }

    #[test]
    fn crossfade_hand_over_finalises_the_previous_track() {
        // A 100 s track crossfades into the next at 94 s: the next is current from then on.
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 100), PlayStatus::Playing);
        sim.run(49_400);
        assert!(sim.scrobbles().is_empty());
        // The fade starts 600 ms later, between two ticks: the time up to it still counts.
        sim.at += Duration::from_millis(600);
        sim.load(track("spotify:track:b", 100), PlayStatus::Loading);
        assert_eq!(sim.scrobbles().len(), 1, "A reached 50 s at the hand-over");
        assert_eq!(sim.scrobbles()[0].track.uri, "spotify:track:a");
        sim.run(500);
        sim.set(PlayStatus::Playing);
        sim.run(50_000);
        let s = sim.scrobbles();
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].track.uri, "spotify:track:b");
        assert_eq!(s[1].timestamp, T0_UNIX + 50, "B's timestamp is when B was first heard");
    }

    #[test]
    fn a_skipped_track_is_not_scrobbled() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 200), PlayStatus::Playing);
        sim.run(99_000);
        sim.load(track("spotify:track:b", 200), PlayStatus::Playing);
        sim.run(10_000);
        assert!(sim.scrobbles().is_empty());
    }

    #[test]
    fn sleep_without_observations_does_not_count() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 600), PlayStatus::Playing);
        sim.run(60_000);
        sim.sleep(3_600);
        // The first observation after the gap counts nothing; counting goes on from there.
        sim.run(1_000);
        assert_eq!(sim.tracker.listened(), Some(Duration::from_secs(60)));
        sim.run(1_000);
        assert!(sim.scrobbles().is_empty());
        assert_eq!(sim.tracker.listened(), Some(Duration::from_secs(61)));
    }

    #[test]
    fn finalising_scrobbles_only_an_eligible_play() {
        // Sleep timer pause before the threshold: nothing.
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 300), PlayStatus::Playing);
        sim.run(100_000);
        sim.status = PlayStatus::Paused;
        sim.finalise();
        sim.observe();
        assert!(sim.scrobbles().is_empty());
        // Resumed the next morning: a new play with a new timestamp, and now playing again.
        sim.run(8 * 3_600_000);
        sim.set(PlayStatus::Playing);
        assert_eq!(sim.now_playings(), 2);
        sim.run(150_000);
        let s = sim.scrobbles();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].timestamp, T0_UNIX + 100 + 8 * 3600);

        // Shutdown just after crossing the threshold between two ticks: scrobbled at finalise.
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 31), PlayStatus::Playing);
        sim.run(15_000);
        sim.at += Duration::from_millis(600);
        sim.finalise();
        assert_eq!(sim.scrobbles().len(), 1);
        sim.finalise();
        assert_eq!(sim.scrobbles().len(), 1);
    }

    #[test]
    fn no_track_ends_the_play() {
        let mut sim = Sim::new();
        sim.load(track("spotify:track:a", 200), PlayStatus::Playing);
        sim.run(99_500);
        sim.at += Duration::from_millis(500);
        // Logged out: the playback state is reset.
        sim.track = None;
        sim.status = PlayStatus::Stopped;
        sim.observe();
        assert_eq!(sim.scrobbles().len(), 1);
        assert_eq!(sim.tracker.listened(), None);
    }

    #[test]
    fn episodes_and_tracks_without_artist_or_title_are_ignored() {
        let mut episode = track("spotify:episode:x", 1800);
        episode.artists = vec![ArtistRef { name: "Some Show".into(), uri: String::new() }];
        let mut no_artist = track("spotify:local:::Song:200", 200);
        no_artist.artists.clear();
        let mut blank_title = track("spotify:track:b", 200);
        blank_title.name = "  ".into();
        for t in [episode, no_artist, blank_title] {
            assert!(scrobble_track(&t).is_none());
            let mut sim = Sim::new();
            sim.load(t, PlayStatus::Playing);
            sim.run(600_000);
            sim.finalise();
            assert!(sim.out.is_empty(), "{:?}", sim.out);
        }
        // A local file with enough details is fine.
        let mut local = track("spotify:local:Artist:Album:Song:200", 200);
        local.is_local = true;
        assert!(scrobble_track(&local).is_some());
    }
}
