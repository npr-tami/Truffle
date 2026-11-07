//! Audio extras on the controller side: equalizer and crossfade settings, when to crossfade, and
//! the sleep timer.
//!
//! Crossfade: while a track plays, the controller works out when its last `fade` seconds begin
//! (from the playback position it already extrapolates for the UI) and, at that moment, does what
//! a natural end of track does: counts the play, moves the queue on, and starts the next track,
//! but through [`Engine::crossfade_to`](crate::engine::Engine::crossfade_to) so the two overlap.
//! The next track is the current one from then on. With nothing to crossfade into (end of the
//! queue, a sleep timer set to the end of the song, very short tracks) the track just ends as
//! before. Skips, seeks and pauses during the fade end it gracefully inside the engine.
//!
//! Sleep timer: `Minutes(n)` pauses at a set time; over its last 30 s the engine's fade gain
//! (separate from the user's volume) glides to silence, and it is restored once the pause has
//! taken effect. `EndOfTrack` pauses when the current song ends, with the next song loaded and
//! paused so play continues from there.

use std::time::{Duration, Instant};

use super::{Controller, lock, user_error};
use crate::config::SoundSource;
use crate::engine::{self, MAX_CROSSFADE_SECS};
use crate::model::{EqSettings, Event, PlayStatus, SleepTimer, SleepTimerState, TrackInfo, YtMatch};

/// Crossfades shorter than this are not worth doing (the track just ends instead).
const MIN_CROSSFADE_MS: u32 = 1_000;
/// Songs of Up next kept fetched with YouTube sound.
const FETCH_AHEAD: usize = 3;
/// The sleep timer fades the sound out over its last 30 seconds.
const SLEEP_FADE: Duration = Duration::from_secs(30);
/// Cancelling the sleep timer mid-fade brings the sound back up over this long.
const SLEEP_FADE_UNDO: Duration = Duration::from_secs(1);
/// Longest sleep timer.
const MAX_SLEEP_MINUTES: u32 = 24 * 60;
/// Deadlines this close count as reached (timers can fire a hair early or late).
const DEADLINE_SLACK: Duration = Duration::from_millis(5);

/// A running sleep timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SleepPlan {
    /// Pause at `fire`; `fading` once the last 30 s have begun.
    At { fire: Instant, fading: bool },
    /// Pause when the current track ends.
    EndOfTrack,
}

/// How a track is started: a plain load, or a crossfade from the one playing.
#[derive(Debug, Clone, Copy)]
pub(super) struct CrossfadePlan {
    pub fade: Duration,
    /// How much of the old track is left to hear.
    pub outgoing_left: Duration,
}

/// Where a crossfade of `fade_ms` starts in a track of `duration_ms`, or `None` if the track is
/// too short (the fade is at most half the track).
pub(super) fn crossfade_start_ms(duration_ms: u32, fade_ms: u32) -> Option<u32> {
    if duration_ms == 0 || fade_ms == 0 {
        return None;
    }
    let fade = fade_ms.min(duration_ms / 2);
    (fade >= MIN_CROSSFADE_MS).then(|| duration_ms - fade)
}

/// The length of a crossfade starting now: the setting, but no longer than what is left of the
/// old track nor half the new one. `None` if that is too short to be worth it.
pub(super) fn crossfade_length_ms(fade_ms: u32, left_ms: u32, next_duration_ms: u32) -> Option<u32> {
    let mut fade = fade_ms.min(left_ms);
    if next_duration_ms > 0 {
        fade = fade.min(next_duration_ms / 2);
    }
    (fade >= MIN_CROSSFADE_MS).then_some(fade)
}

/// "in 45 minutes", "in 1 hour", "in 1 hour 30 minutes".
fn describe_minutes(minutes: u32) -> String {
    let unit = |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let (hours, rest) = (minutes / 60, minutes % 60);
    match (hours, rest) {
        (0, m) => unit(m, "minute", "minutes"),
        (h, 0) => unit(h, "hour", "hours"),
        (h, m) => format!("{} {}", unit(h, "hour", "hours"), unit(m, "minute", "minutes")),
    }
}

impl Controller {
    // ------------------------------------------------------------ equalizer and crossfade

    pub(super) fn cmd_set_equalizer(&mut self, eq: EqSettings) {
        let eq = engine::sanitize_equalizer(&eq);
        if let Some(engine) = &self.engine {
            engine.set_equalizer(&eq);
        }
        self.update_settings(|s| s.equalizer = eq);
    }

    /// Where songs' sound comes from now: a free account always uses YouTube.
    pub(super) fn sound_source(&self) -> SoundSource {
        if self.profile.as_ref().and_then(|p| p.premium) == Some(false) {
            return SoundSource::YouTube;
        }
        lock(&self.settings).sound_source
    }

    /// The account type arrived after login (Spotify says it a moment after connecting): a free
    /// account switches to YouTube sound.
    pub(super) fn refresh_premium(&mut self) {
        if self.profile.as_ref().is_none_or(|p| p.premium.is_some()) {
            return;
        }
        let Some(kind) = self.session.as_ref().and_then(|s| s.get_user_attribute("type")) else { return };
        let before = self.sound_source();
        let Some(profile) = self.profile.as_mut() else { return };
        profile.premium = Some(kind == "premium");
        let profile = profile.clone();
        log::info!("account type (late): {kind}");
        self.ui.send(Event::LoggedIn(profile));
        self.source_changed(before);
    }

    /// After something changed where songs' sound comes from: tell the engine, and the song
    /// playing now goes on from the same point, from its new source.
    fn source_changed(&mut self, before: SoundSource) {
        let now = self.sound_source();
        if before == now {
            return;
        }
        log::info!("sound source: {now:?}");
        if let Some(engine) = &self.engine {
            engine.set_sound_source(now);
        }
        if self.pb.track.is_some() && self.pb.status != PlayStatus::Stopped {
            let (play, position) = (self.want_play, self.pb.current_position_ms());
            self.load_current(play, position, false);
        }
        self.refresh_preload();
        self.youtube_fetch_ahead();
        self.youtube_search_ahead();
    }

    /// YouTube sound: the next few songs of Up next are fetched in the background, so they start
    /// at once (not while listening along: the host decides).
    pub(super) fn youtube_fetch_ahead(&mut self) {
        let songs: Vec<TrackInfo> = if self.sound_source() == SoundSource::YouTube && !self.listening() {
            self.last_upnext.iter().flat_map(|next| next.iter()).take(FETCH_AHEAD).cloned().collect()
        } else {
            Vec::new()
        };
        let uris: Vec<String> = songs.iter().map(|t| t.uri.clone()).collect();
        if uris != self.fetching_ahead {
            self.fetching_ahead = uris;
            self.youtube.fetch_ahead(songs);
        }
    }

    /// YouTube sound: matches for the open list's songs are searched for in the background.
    pub(super) fn youtube_search_ahead(&self) {
        match &self.shown {
            Some((_, songs)) if self.sound_source() == SoundSource::YouTube => {
                self.youtube.search_ahead(songs, self.pb.track.as_ref().map(|t| t.uri.as_str()));
            }
            _ => self.youtube.search_ahead(&[], None),
        }
    }

    pub(super) fn cmd_set_sound_source(&mut self, source: SoundSource) {
        let before = self.sound_source();
        self.update_settings(|s| s.sound_source = source);
        self.ui.send(Event::SoundSource(source));
        let now = self.sound_source();
        if now == SoundSource::YouTube
            && let Err(missing) = self.youtube.tools()
        {
            self.ui.send(Event::Error(missing));
        }
        self.source_changed(before);
    }

    /// The YouTube helpers' versions for Settings > About (each program run once, in the
    /// background).
    pub(super) fn cmd_helpers(&mut self) {
        let (youtube, ui) = (self.youtube.clone(), self.ui.clone());
        tokio::spawn(async move { ui.send(Event::Helpers(youtube.helpers().await)) });
    }

    /// The YouTube videos that may be this song, for "Wrong song?".
    pub(super) fn cmd_youtube_matches(&mut self, track: TrackInfo) {
        let (youtube, ui) = (self.youtube.clone(), self.ui.clone());
        tokio::spawn(async move {
            let result = match youtube.candidates(&track).await {
                Ok(list) => Ok(list
                    .into_iter()
                    .map(|c| YtMatch { video_id: c.video_id, title: c.title, channel: c.channel, duration_ms: c.duration_ms })
                    .collect()),
                Err(e) => Err(user_error("Couldn't look on YouTube.", &e)),
            };
            // After the search, which may have just found the song's video.
            let current = youtube.current(&track.uri);
            ui.send(Event::YouTubeMatches { uri: track.uri, current, result });
        });
    }

    /// The user picked the video this song should play; if it is playing, it goes on from the
    /// same point with the new video.
    pub(super) fn cmd_choose_youtube(&mut self, track: TrackInfo, video_id: String) {
        if !self.youtube.choose(&track.uri, &video_id) {
            return;
        }
        if let Some(engine) = &self.engine {
            engine.forget(&track.uri);
        }
        if self.sound_source() == SoundSource::YouTube && self.is_current(&track.uri) && self.pb.status != PlayStatus::Stopped {
            let (play, position) = (self.want_play, self.pb.current_position_ms());
            self.load_current(play, position, false);
        } else {
            self.refresh_preload();
        }
        self.ui.send(Event::Status(format!("\"{}\" now plays the video you picked.", track.name)));
    }

    pub(super) fn cmd_set_crossfade(&mut self, secs: u8) {
        let secs = secs.min(MAX_CROSSFADE_SECS);
        let was_on = self.crossfade_ms > 0;
        self.crossfade_ms = u32::from(secs) * 1000;
        if let Some(engine) = &self.engine {
            engine.set_crossfade(secs);
        }
        self.update_settings(|s| s.crossfade_secs = secs);
        self.crossfade_checked = false;
        if was_on != (secs > 0) {
            // The next track is preloaded on a different player with crossfading on.
            self.refresh_preload();
        }
    }

    /// Crossfading is on (not with YouTube sound yet: its player plays one song at a time; nor
    /// while listening along).
    fn crossfade_on(&self) -> bool {
        self.crossfade_ms > 0 && self.sound_source() == SoundSource::Spotify && !self.listening()
    }

    /// Whether the current track is expected to crossfade into the next (decides where the next
    /// track is preloaded). Not once its crossfade point has passed without one: it then ends by
    /// itself, and the next track follows on the same player.
    pub(super) fn crossfade_planned(&self) -> bool {
        self.crossfade_on()
            && !self.crossfade_checked
            && self.sleep != Some(SleepPlan::EndOfTrack)
            && self.pb.track.as_ref().is_some_and(|t| crossfade_start_ms(t.duration_ms, self.crossfade_ms).is_some())
    }

    /// When the current track's crossfade into the next should start, if it should.
    fn crossfade_due(&self) -> Option<Instant> {
        if !self.crossfade_on()
            || self.crossfade_checked
            || self.pb.status != PlayStatus::Playing
            || self.end_handled
            || self.pending_reload
            || self.reconnect.is_some()
            || self.probe.is_some()
            || self.engine.is_none()
            || self.sleep == Some(SleepPlan::EndOfTrack)
        {
            return None;
        }
        let track = self.pb.track.as_ref()?;
        let at = self.pb.position_at?;
        let start_ms = crossfade_start_ms(track.duration_ms, self.crossfade_ms)?;
        Some(at + Duration::from_millis(u64::from(start_ms.saturating_sub(self.pb.position_ms))))
    }

    /// The current track has reached its crossfade point: move on to the next one, overlapping.
    fn on_crossfade_due(&mut self) {
        // Whatever happens, this track's crossfade point has been dealt with.
        self.crossfade_checked = true;
        if !self.try_crossfade() {
            // It ends by itself: have the next track (if any) follow gaplessly on the current
            // player instead of the one it was preloaded on for the crossfade.
            self.refresh_preload();
        }
    }

    /// Starts the crossfade into the next track, if there is one worth crossfading into.
    fn try_crossfade(&mut self) -> bool {
        let Some(track) = self.pb.track.clone() else { return false };
        let left_ms = track.duration_ms.saturating_sub(self.pb.current_position_ms());
        if left_ms < MIN_CROSSFADE_MS {
            return false;
        }
        // From here on this is the end of the track, as far as play counting goes.
        if !self.listen.recorded {
            self.record_current_play();
        }
        let rules = self.rules();
        let repeat = self.repeat();
        let next_duration = match &self.queue {
            Some(queue) => queue.peek_next(repeat, &rules, &self.history).map(|t| t.duration_ms),
            None => None,
        };
        // Nothing to crossfade into (the end of the queue): the track simply ends.
        let Some(next_duration) = next_duration else { return false };
        let Some(fade_ms) = crossfade_length_ms(self.crossfade_ms, left_ms, next_duration) else { return false };
        let advanced = match self.queue.as_mut() {
            Some(queue) => queue.advance(repeat, &rules, &self.history).is_some(),
            None => false,
        };
        if !advanced {
            return false;
        }
        log::info!("crossfading from \"{}\" into the next track over {:.1} s", track.name, f64::from(fade_ms) / 1000.0);
        let plan = CrossfadePlan {
            fade: Duration::from_millis(u64::from(fade_ms)),
            outgoing_left: Duration::from_millis(u64::from(left_ms)),
        };
        self.load_current_with(true, 0, true, Some(plan));
        true
    }

    // ------------------------------------------------------------ sleep timer

    pub(super) fn cmd_set_sleep_timer(&mut self, timer: Option<SleepTimer>) {
        let was_fading = matches!(self.sleep, Some(SleepPlan::At { fading: true, .. }));
        let had_timer = self.sleep.is_some();
        let now = Instant::now();
        let (plan, status) = match timer {
            None | Some(SleepTimer::Minutes(0)) => (None, had_timer.then(|| "Sleep timer off.".to_owned())),
            Some(SleepTimer::Minutes(minutes)) => {
                let minutes = minutes.min(MAX_SLEEP_MINUTES);
                let fire = now + Duration::from_secs(u64::from(minutes) * 60);
                let status = format!("Sleep timer set: music pauses in {}.", describe_minutes(minutes));
                (Some(SleepPlan::At { fire, fading: false }), Some(status))
            }
            Some(SleepTimer::EndOfTrack) => {
                (Some(SleepPlan::EndOfTrack), Some("Sleep timer set: music pauses when this song ends.".to_owned()))
            }
        };
        self.sleep = plan;
        if was_fading && let Some(engine) = &self.engine {
            // The old timer had started fading the sound out: bring it back.
            engine.fade_gain_to(1.0, SLEEP_FADE_UNDO);
        }
        log::info!("sleep timer: {:?}", self.sleep);
        self.ui.send(Event::SleepTimer(self.sleep_state()));
        if let Some(status) = status {
            self.ui.send(Event::Status(status));
        }
        // "Pause at the end of this song" and crossfading exclude each other.
        self.crossfade_checked = false;
        self.refresh_preload();
    }

    fn sleep_state(&self) -> Option<SleepTimerState> {
        match self.sleep? {
            SleepPlan::At { fire, .. } => Some(SleepTimerState::Until(fire)),
            SleepPlan::EndOfTrack => Some(SleepTimerState::EndOfTrack),
        }
    }

    fn sleep_deadline(&self) -> Option<Instant> {
        match self.sleep? {
            SleepPlan::At { fire, fading: false } => Some(fire.checked_sub(SLEEP_FADE).unwrap_or(fire)),
            SleepPlan::At { fire, fading: true } => Some(fire),
            SleepPlan::EndOfTrack => None,
        }
    }

    fn check_sleep_timer(&mut self, now: Instant) {
        let Some(SleepPlan::At { fire, fading }) = self.sleep else { return };
        let now = now + DEADLINE_SLACK;
        if now >= fire {
            self.fire_sleep_timer();
        } else if !fading && now + SLEEP_FADE >= fire {
            self.sleep = Some(SleepPlan::At { fire, fading: true });
            log::info!("sleep timer: fading out");
            if let Some(engine) = &self.engine {
                engine.fade_gain_to(0.0, fire.saturating_duration_since(Instant::now()));
            }
        }
    }

    /// The timer ran out: pause (if playing) and say so.
    fn fire_sleep_timer(&mut self) {
        self.sleep = None;
        let playing = self.want_play || matches!(self.pb.status, PlayStatus::Playing | PlayStatus::Loading);
        // Listening along would start the music again with the host's next song.
        self.end_listening(None);
        if playing {
            self.pause_playback();
            // The listener has probably fallen asleep: this play ends here (scrobbled only if it
            // was already heard long enough); resuming tomorrow is a new play.
            self.finalise_scrobble_play();
        }
        // The fade left the sound silent. Restore it once the pause has taken effect (the engine
        // reports `Paused` after the queued audio has played out), or before anything plays
        // again; never right away, as a pause made just before may still be playing out.
        self.fade_restore_pending = self.engine.is_some();
        log::info!("sleep timer: done");
        self.ui.send(Event::SleepTimer(None));
        self.ui.send(Event::Status(if playing { "Sleep timer: music paused." } else { "Sleep timer ended." }.into()));
    }

    /// Called at the natural end of a track: if the timer waits for it, it fires now. Returns
    /// whether it did (the caller then loads the next track paused).
    pub(super) fn sleep_at_end_of_track(&mut self) -> bool {
        if self.sleep != Some(SleepPlan::EndOfTrack) {
            return false;
        }
        self.sleep = None;
        log::info!("sleep timer: the song ended");
        self.ui.send(Event::SleepTimer(None));
        self.ui.send(Event::Status("Sleep timer: music paused.".into()));
        true
    }

    /// Undo the sleep timer's fade-out (after its pause), before anything plays again.
    pub(super) fn restore_fade_gain(&mut self) {
        if !self.fade_restore_pending {
            return;
        }
        self.fade_restore_pending = false;
        if let Some(engine) = &self.engine {
            engine.set_fade_gain(1.0);
        }
    }

    /// Bring a newly created engine in line with the sleep timer (it starts at full gain).
    pub(super) fn apply_fade_to_new_engine(&mut self) {
        self.fade_restore_pending = false;
        if let (Some(engine), Some(SleepPlan::At { fire, fading: true })) = (&self.engine, self.sleep) {
            engine.fade_gain_to(0.0, fire.saturating_duration_since(Instant::now()));
        }
    }

    // ------------------------------------------------------------ timers

    /// The next moment `on_deadline` has something to do.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        match (self.sleep_deadline(), self.crossfade_due()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(super) fn on_deadline(&mut self) {
        let now = Instant::now();
        self.check_sleep_timer(now);
        if self.crossfade_due().is_some_and(|at| at <= now + DEADLINE_SLACK) {
            self.on_crossfade_due();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_starts_its_length_before_the_end() {
        assert_eq!(crossfade_start_ms(200_000, 6_000), Some(194_000));
        // At most half the track.
        assert_eq!(crossfade_start_ms(8_000, 6_000), Some(4_000));
        // Too short to bother, or unknown length, or off.
        assert_eq!(crossfade_start_ms(1_500, 6_000), None);
        assert_eq!(crossfade_start_ms(0, 6_000), None);
        assert_eq!(crossfade_start_ms(200_000, 0), None);
    }

    #[test]
    fn crossfade_length_respects_both_tracks() {
        assert_eq!(crossfade_length_ms(6_000, 6_000, 180_000), Some(6_000));
        // Less left of the old track (a seek near the end).
        assert_eq!(crossfade_length_ms(6_000, 2_500, 180_000), Some(2_500));
        // A short next track: half of it, so its own fade-out can't overlap this fade-in.
        assert_eq!(crossfade_length_ms(6_000, 6_000, 5_000), Some(2_500));
        // Unknown length of the next track: the setting.
        assert_eq!(crossfade_length_ms(6_000, 6_000, 0), Some(6_000));
        assert_eq!(crossfade_length_ms(6_000, 600, 180_000), None);
        assert_eq!(crossfade_length_ms(6_000, 6_000, 1_200), None);
    }

    #[test]
    fn consecutive_short_tracks_never_overlap_three_ways() {
        // Track B (len) fades in over at most len/2 and starts fading out at len - fade >= len/2,
        // so its fade-in is over before its fade-out begins, whatever the lengths.
        for len in [2_000u32, 3_000, 5_000, 9_000, 11_999, 12_000, 30_000] {
            for setting in [1_000u32, 6_000, 12_000] {
                let fade_in = crossfade_length_ms(setting, setting, len).unwrap_or(0);
                if let Some(start) = crossfade_start_ms(len, setting) {
                    assert!(fade_in <= start, "len {len}, setting {setting}: in {fade_in} > out at {start}");
                }
            }
        }
    }

    #[test]
    fn sleep_timer_wording() {
        assert_eq!(describe_minutes(1), "1 minute");
        assert_eq!(describe_minutes(45), "45 minutes");
        assert_eq!(describe_minutes(60), "1 hour");
        assert_eq!(describe_minutes(120), "2 hours");
        assert_eq!(describe_minutes(90), "1 hour 30 minutes");
        assert_eq!(describe_minutes(61), "1 hour 1 minute");
    }
}
