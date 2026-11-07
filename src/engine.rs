//! Audio playback through librespot's Player + SoftMixer + Truffle's own rodio output.
//!
//! The engine only plays what it is told; the queue and "what plays next" live in the controller.
//!
//! - Player events are filtered to the most recent `load` and de-duplicated before they become
//!   [`EngineEvent`]s (see `tracking`), so the controller never sees stale or repeated signals.
//! - Volume goes through librespot's SoftMixer with a cubic (perceptual) curve.
//! - Audio output is a small rodio sink of our own (see `output`) instead of librespot's rodio
//!   backend: librespot's backend panics when there is no output device (which aborts a release
//!   build) and hangs the player thread forever when the device disappears mid-song. Ours reports
//!   the problem, lets the player pause, and opens whatever the default device is on the next play.
//! - That output also runs the equalizer (`dsp`) and applies fades while the audio plays (`gain`).
//!
//! ## Crossfade
//!
//! librespot's `Player` plays one track at a time (with gapless hand-over), so two tracks can only
//! overlap with two players. The engine has two player *slots* sharing the session, the volume
//! control and the equalizer settings; each has its own output stream (Windows mixes them) and its
//! own fade envelope. Only the *active* slot's events reach the controller.
//!
//! The controller decides when a crossfade starts and calls [`Engine::crossfade_to`] with the next
//! track: it is loaded on the idle slot (usually already preloaded there), which becomes the
//! active one at once. The fades begin when the new track actually plays: the new slot fades in
//! and the old one fades out with equal-power curves, and the old player is stopped once silent.
//! Any skip, seek, pause or stop during a crossfade ends it gracefully (the old track fades out
//! within a quarter second, the new one comes up to full volume). With crossfading off the second
//! player does not exist, and playback is the original single-player gapless path.
//!
//! ## Sound source
//!
//! With the YouTube sound source a slot holds Truffle's own player instead of librespot's (see
//! `pcm`): the song Spotify names is found on YouTube, fetched whole, decoded (see `m4a`) and
//! played through the same output. It reports librespot's events, so all of the above works for
//! it unchanged. It needs each song's details first ([`Engine::hint`]).
//!
//! A switch of the source takes effect at the next load: the current song plays on, and the next
//! one starts on a player of the other kind (without a gapless hand-over at that one moment).
//! There are no crossfades with YouTube yet: its tracks start with a plain load.

mod dsp;
mod gain;
mod m4a;
mod output;
mod pcm;
mod tracking;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use librespot_core::{Session, SpotifyUri};
use librespot_playback::audio_backend::Sink;
use librespot_playback::config::{
    Bitrate, NormalisationMethod, NormalisationType, PlayerConfig, VolumeCtrl,
};
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent, PlayerEventChannel};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender, WeakUnboundedSender};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::config::{Paths, Settings, SoundSource};
use crate::model::{EqSettings, TrackInfo};
use crate::youtube::YouTube;

use self::dsp::EqControl;
use self::gain::{Curve, GainControl, OutputGain};
use self::pcm::{Hints, PcmEnv, PcmPlayer, PcmResolver, PcmSink, YouTubeResolver};
use self::tracking::Tracking;

pub use self::dsp::sanitize as sanitize_equalizer;

/// Longest supported crossfade, in seconds.
pub const MAX_CROSSFADE_SECS: u8 = 12;

/// How often the session is checked for having gone invalid.
const SESSION_POLL: Duration = Duration::from_secs(2);
/// A crossfade cut short (skip, seek, pause, stop) fades the old track out this quickly, and
/// brings the new one up to full volume just as quickly.
const CANCEL_RAMP: Duration = Duration::from_millis(250);
/// [`Engine::set_fade_gain`] glides to its new gain over this long, so it never clicks.
const FADE_GAIN_RAMP: Duration = Duration::from_millis(60);
/// The old player is stopped this long after its fade-out has reached silence.
const FADE_STOP_MARGIN: Duration = Duration::from_millis(200);
/// Shortest fade-out, for an old track that is about to end anyway.
const MIN_FADE_OUT: Duration = Duration::from_millis(50);
/// A stopped player has played out its queued audio (about half a second) within this long.
const DRAIN_TIME: Duration = Duration::from_millis(750);

const PLAYER_GONE: &str = "Audio playback isn't available right now. Press play to try again.";

/// Simplified, de-duplicated player events (filtered to the currently loaded URI where relevant).
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    Loading { uri: String },
    Playing { uri: String, position_ms: u32 },
    Paused { uri: String, position_ms: u32 },
    /// Position jump or correction while playing/paused.
    Position { uri: String, position_ms: u32 },
    /// The current track finished (emitted once per load).
    EndOfTrack { uri: String },
    /// The track cannot be played (region, removed, decode failure). Controller should skip.
    Unavailable { uri: String },
    /// Good moment to call `preload` with the next track.
    PreloadNext { uri: String },
    Stopped,
    /// The librespot session became invalid (network loss, kicked). Controller should reconnect.
    SessionInvalid,
}

pub struct Engine {
    shared: Arc<Mutex<Shared>>,
    /// For creating players: the second one when it is first needed, and one of the other kind
    /// when the sound source changes.
    parts: PlayerParts,
    /// Weak, so the event channel closes when the event task ends (it holds the only strong
    /// sender). The controller treats a closed channel as "player died".
    events: WeakUnboundedSender<EngineEvent>,
    /// Hands the event stream of a newly created player to the event task.
    new_streams: UnboundedSender<NewStream>,
    session_tx: watch::Sender<Session>,
    /// Engine-wide fade (sleep timer), separate from the user's volume.
    fade_gain: Arc<GainControl>,
}

impl Engine {
    /// Create the player. Must be called from within the tokio runtime.
    pub fn new(
        session: Session,
        settings: &Settings,
        _paths: &Paths,
        youtube: YouTube,
    ) -> Result<(Self, UnboundedReceiver<EngineEvent>)> {
        Self::with_resolver(session, settings, Arc::new(YouTubeResolver(youtube)))
    }

    /// [`new`](Self::new), with the YouTube source's songs found by `resolver`.
    fn with_resolver(
        session: Session,
        settings: &Settings,
        resolver: Arc<dyn PcmResolver>,
    ) -> Result<(Self, UnboundedReceiver<EngineEvent>)> {
        let runtime = Handle::try_current()
            .context("Internal error: the audio engine was started outside the background runtime.")?;

        // Linear control: `volume_to_mixer` already applies the perceptual curve.
        let mixer = SoftMixer::open(MixerConfig {
            volume_ctrl: VolumeCtrl::Linear,
            ..MixerConfig::default()
        })
        .map_err(|e| anyhow!("Truffle couldn't set up its volume control: {e}"))?;
        mixer.set_volume(volume_to_mixer(settings.volume));

        let eq = Arc::new(EqControl::new(&settings.equalizer));
        let fade_gain = Arc::new(GainControl::new(1.0));
        let pcm = Arc::new(PcmEnv {
            resolver,
            hints: Hints::default(),
            problem: output::ProblemSlot::default(),
            runtime: runtime.clone(),
        });
        let parts = PlayerParts { config: player_config(settings), mixer, eq, pcm };
        let source = settings.sound_source;
        let mut shared = Shared::new(&fade_gain, settings.crossfade_secs > 0, source);
        // Subscribed before the first load, so no event is missed.
        let first = spawn_player(&parts, source, session.clone(), 0, &mut shared.slots[0]);
        let shared = Arc::new(Mutex::new(shared));

        let (events, events_rx) = mpsc::unbounded_channel();
        let weak_events = events.downgrade();
        let (new_streams, new_streams_rx) = mpsc::unbounded_channel();
        runtime.spawn(forward_player_events(shared.clone(), first, new_streams_rx, events));

        let (session_tx, session_rx) = watch::channel(session);
        runtime.spawn(watch_session(session_rx, weak_events.clone()));

        log::info!(
            "audio engine ready (sound from {source:?}, quality {}, normalisation {}, equalizer {}, crossfade {} s)",
            settings.bitrate,
            if settings.normalisation { "on" } else { "off" },
            if settings.equalizer.enabled { "on" } else { "off" },
            settings.crossfade_secs.min(MAX_CROSSFADE_SECS),
        );
        Ok((Self { shared, parts, events: weak_events, new_streams, session_tx, fade_gain }, events_rx))
    }

    /// Load a `spotify:track:` / `spotify:episode:` URI. Ends a crossfade in progress.
    pub fn load(&self, uri: &str, start_playing: bool, position_ms: u32) -> Result<()> {
        let parsed = parse_playable_uri(uri)?;
        // Hold the lock while sending, so the order of loads and their bookkeeping always match.
        let mut shared = lock(&self.shared);
        shared.settle();
        self.match_source(&mut shared);
        let idle = 1 - shared.active;
        // Only for a skip. Not when the current track ended by itself (its last half second is
        // still playing out: the next one follows it on the same player, as without crossfade),
        // not when the current player has the track preloaded too, and not while the other player
        // may still be playing out the end of an earlier fade-out (it starts at full volume).
        let switch = start_playing
            && position_ms == 0
            && !shared.active().tracking.reached_end()
            && shared.active().preloaded.as_ref() != Some(&parsed)
            && shared.slots[idle].player.is_some()
            && shared.slots[idle].preloaded.as_ref() == Some(&parsed)
            && shared.slots[idle].quiet_at <= Instant::now();
        if !switch && shared.slots[idle].preloaded.as_ref() == Some(&parsed) {
            // It plays on the current player instead; that preload is used up.
            shared.slots[idle].preloaded = None;
        }
        if switch {
            // Preloaded on the other player for a crossfade (a skip to the next track): start it
            // there at once and fade the current one out quickly, instead of loading it again.
            let from = shared.active;
            shared.slots[from].stop_gently();
            shared.active = idle;
        }
        let slot = shared.active_mut();
        let Some(player) = slot.player.as_ref() else { bail!(PLAYER_GONE) };
        if switch {
            slot.gain.own.set(1.0);
            slot.tracking = Tracking::default();
            player.stop();
        }
        slot.preloaded = None;
        slot.tracking.begin_load(uri.to_owned(), parsed.clone());
        player.load(parsed, start_playing, position_ms);
        Ok(())
    }

    /// Start `uri` (from the beginning) on the second player and crossfade into it: from now on
    /// it is the current track. The fades last `fade`; the outgoing track, which has
    /// `outgoing_left` still to play, fades out within that time. Without a second player this
    /// is a plain `load`.
    pub fn crossfade_to(&self, uri: &str, fade: Duration, outgoing_left: Duration) -> Result<()> {
        let parsed = parse_playable_uri(uri)?;
        let mut shared = lock(&self.shared);
        // A crossfade still running (the track in between was very short) ends now.
        if let Some(previous) = shared.crossfade.take()
            && let Some(outgoing) = previous.outgoing
        {
            shared.slots[outgoing].stop_gently();
        }
        let from = shared.active;
        let Some(to) = self.idle_slot_ready(&mut shared) else {
            drop(shared);
            return self.load(uri, true, 0);
        };
        let incoming = &mut shared.slots[to];
        // Silent until it is really playing; `Shared::start_fades` then fades it in.
        incoming.gain.own.set(0.0);
        incoming.tracking = Tracking::default();
        incoming.tracking.begin_load(uri.to_owned(), parsed.clone());
        lock(&incoming.problem).take();
        incoming.preloaded = None;
        if let Some(player) = incoming.player.as_ref() {
            // A no-op for the stopped player it normally is.
            player.stop();
            player.load(parsed, true, 0);
        }
        // The old track plays on at full volume until the new one is heard. Its events are no
        // longer reported: as far as the controller knows, the new track is the current one.
        shared.slots[from].tracking.clear();
        shared.active = to;
        shared.next_fade_id += 1;
        let now = Instant::now();
        shared.crossfade = Some(Crossfade {
            id: shared.next_fade_id,
            outgoing: Some(from),
            fade: fade.max(MIN_FADE_OUT),
            outgoing_ends: now.checked_add(outgoing_left).unwrap_or(now),
            started: false,
        });
        log::debug!("crossfading into {uri} over {fade:?} ({outgoing_left:?} of the old track left)");
        Ok(())
    }

    /// Preload the next track for gapless playback (on the current player).
    pub fn preload(&self, uri: &str) -> Result<()> {
        let parsed = parse_playable_uri(uri)?;
        let mut shared = lock(&self.shared);
        let source = shared.source;
        let slot = shared.active_mut();
        // Right after a switch of the sound source the current player is of the other kind until
        // the next load replaces it: a preload on it would be wasted.
        if let Some(player) = slot.player.as_ref().filter(|p| p.source() == source) {
            player.preload(parsed.clone());
            slot.preloaded = Some(parsed);
        }
        Ok(())
    }

    /// Preload the track a coming [`crossfade_to`](Self::crossfade_to) will start, on the player
    /// that will play it (creating the second player if needed).
    pub fn preload_for_crossfade(&self, uri: &str) -> Result<()> {
        let parsed = parse_playable_uri(uri)?;
        let mut shared = lock(&self.shared);
        let index = self.idle_slot_ready(&mut shared).unwrap_or(shared.active);
        let source = shared.source;
        let slot = &mut shared.slots[index];
        if let Some(player) = slot.player.as_ref().filter(|p| p.source() == source) {
            player.preload(parsed.clone());
            slot.preloaded = Some(parsed);
        }
        Ok(())
    }

    pub fn play(&self) {
        let shared = lock(&self.shared);
        if let Some(player) = shared.active().player.as_ref() {
            player.play();
        }
    }

    pub fn pause(&self) {
        let mut shared = lock(&self.shared);
        shared.settle();
        if let Some(player) = shared.active().player.as_ref() {
            player.pause();
        }
    }

    pub fn stop(&self) {
        let mut shared = lock(&self.shared);
        shared.settle();
        let slot = shared.active_mut();
        // Anything the player still reports for the old track is stale from here on.
        slot.tracking.clear();
        if let Some(player) = slot.player.as_ref() {
            player.stop();
        }
        // librespot only reports `Stopped` when something was loaded; report it unconditionally.
        // Sent under the lock so no event mapped earlier can arrive after it.
        if let Some(events) = self.events.upgrade() {
            let _ = events.send(EngineEvent::Stopped);
        }
    }

    pub fn seek(&self, position_ms: u32) {
        let mut shared = lock(&self.shared);
        shared.settle();
        if let Some(player) = shared.active().player.as_ref() {
            player.seek(position_ms);
        }
    }

    /// 0..=100, applied through the soft mixer with a perceptual curve.
    pub fn set_volume(&self, volume: u8) {
        self.parts.mixer.set_volume(volume_to_mixer(volume));
    }

    /// Swap in a freshly connected session after a reconnect.
    pub fn set_session(&self, session: Session) {
        {
            let shared = lock(&self.shared);
            for player in shared.slots.iter().filter_map(|slot| slot.player.as_ref()) {
                player.set_session(session.clone());
            }
        }
        // Re-arms the `SessionInvalid` check for the new session.
        self.session_tx.send_replace(session);
    }

    /// Apply new equalizer settings. They are heard within about half a second (the audio that
    /// is already queued), crossfaded from the old ones.
    pub fn set_equalizer(&self, settings: &EqSettings) {
        self.parts.eq.set(settings);
    }

    /// Crossfade length in seconds (0 = off). The engine only needs to know whether to keep its
    /// second player; the controller decides when to call [`crossfade_to`](Self::crossfade_to).
    pub fn set_crossfade(&self, secs: u8) {
        let mut shared = lock(&self.shared);
        shared.keep_second = secs > 0;
        shared.retire_idle();
    }

    /// Engine-wide fade gain (0..=1) on top of the user's volume, reached within ~60 ms.
    pub fn set_fade_gain(&self, gain: f32) {
        self.fade_gain.ramp_to(gain, FADE_GAIN_RAMP, Curve::Linear);
    }

    /// Glide the engine-wide fade gain to `gain` over `over`, evenly paced to the ear (the sleep
    /// timer's fade-out). It follows the clock, playing or not.
    pub fn fade_gain_to(&self, gain: f32, over: Duration) {
        self.fade_gain.ramp_to(gain, over, Curve::Perceptual);
    }

    /// A plain-language description of the last audio output failure (no speakers/headphones
    /// found, or the device was unplugged), if any. Taking it clears it. When this happens the
    /// player pauses itself (`EngineEvent::Paused`), and the next `play` opens whatever output
    /// device is the Windows default at that moment.
    pub fn take_audio_problem(&self) -> Option<String> {
        let shared = lock(&self.shared);
        let idle = 1 - shared.active;
        lock(&shared.slots[idle].problem).take();
        lock(&shared.active().problem).take()
    }

    /// Where songs' sound comes from. The current song plays on; the next load (or preload) uses
    /// the new source. Loading the current song again at its position switches it at once.
    pub fn set_sound_source(&self, source: SoundSource) {
        let mut shared = lock(&self.shared);
        if shared.source != source {
            log::info!("sound source: {source:?}");
            shared.source = source;
        }
    }

    /// Remember a song's details (title, artists, length) by its URI: the YouTube source needs
    /// them to find it. Give them before loading or preloading the song; the most recent 64 are
    /// kept. Without them the song is reported `Unavailable`.
    pub fn hint(&self, track: &TrackInfo) {
        self.parts.pcm.hints.remember(track);
    }

    /// Another YouTube video was picked for this song: the YouTube players drop its audio, so its
    /// next load fetches the new one.
    pub fn forget(&self, uri: &str) {
        let Ok(parsed) = parse_playable_uri(uri) else { return };
        let shared = lock(&self.shared);
        for slot in &shared.slots {
            if let Some(SlotPlayer::Pcm(player)) = slot.player.as_ref() {
                player.forget(parsed.clone());
            }
        }
    }

    /// Why the last song from YouTube couldn't be played (reported as `Unavailable`), in plain
    /// words. Taking it clears it.
    pub fn take_source_problem(&self) -> Option<String> {
        lock(&self.parts.pcm.problem).take()
    }

    /// The idle slot, with a player in it (created if needed). `None` if that is not possible.
    fn idle_slot_ready(&self, shared: &mut Shared) -> Option<usize> {
        // No crossfades with YouTube yet: its tracks start with a plain load.
        if shared.source != SoundSource::Spotify {
            return None;
        }
        let idle = 1 - shared.active;
        if let Some(player) = shared.slots[idle].player.as_ref() {
            if player.source() == shared.source {
                return Some(idle);
            }
            // A YouTube player left from before the source was switched, maybe still fading out.
            if shared.crossfade.is_some_and(|c| c.outgoing == Some(idle)) {
                return None;
            }
            shared.slots[idle].halt();
            if let Some(player) = shared.slots[idle].player.take() {
                drop_players_in_background(vec![player]);
            }
        }
        if shared.closing {
            return None;
        }
        let session = self.session_tx.borrow().clone();
        let stream = spawn_player(&self.parts, SoundSource::Spotify, session, idle, &mut shared.slots[idle]);
        if self.new_streams.send(stream).is_err() {
            log::warn!("the audio engine's event task has stopped; playing without a crossfade");
            if let Some(player) = shared.slots[idle].player.take() {
                drop_players_in_background(vec![player]);
            }
            return None;
        }
        log::debug!("started the second player for crossfades");
        Some(idle)
    }

    /// After a switch of the sound source: the current player is replaced by one of the other
    /// kind (what it has queued still plays out), and an idle one is shut down (it is made again
    /// when needed).
    fn match_source(&self, shared: &mut Shared) {
        let source = shared.source;
        let idle = 1 - shared.active;
        if shared.slots[idle].player.as_ref().is_some_and(|p| p.source() != source) {
            shared.slots[idle].halt();
            shared.slots[idle].preloaded = None;
            if let Some(player) = shared.slots[idle].player.take() {
                drop_players_in_background(vec![player]);
            }
        }
        let active = shared.active;
        if shared.closing || !shared.slots[active].player.as_ref().is_some_and(|p| p.source() != source) {
            return;
        }
        shared.slots[active].halt();
        let old = shared.slots[active].player.take();
        let session = self.session_tx.borrow().clone();
        let stream = spawn_player(&self.parts, source, session, active, &mut shared.slots[active]);
        drop_players_in_background(old.into_iter().collect());
        if self.new_streams.send(stream).is_err() {
            log::warn!("the audio engine's event task has stopped; no player for {source:?}");
            if let Some(player) = shared.slots[active].player.take() {
                drop_players_in_background(vec![player]);
            }
            return;
        }
        log::debug!("the player now plays from {source:?}");
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let players: Vec<SlotPlayer> = {
            let mut shared = lock(&self.shared);
            shared.closing = true;
            shared.crossfade = None;
            shared.slots.iter_mut().filter_map(|slot| slot.player.take()).collect()
        };
        for player in &players {
            player.stop();
        }
        drop_players_in_background(players);
    }
}

/// Dropping a player joins its thread, which can take a moment while it finishes the audio it
/// has queued. Never do that on a runtime worker or the UI thread. If the thread cannot be
/// spawned, the players drop here.
fn drop_players_in_background(players: Vec<SlotPlayer>) {
    if players.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("truffle-player-shutdown".into())
        .spawn(move || drop(players));
    if let Err(e) = spawned {
        log::warn!("could not shut the player down in the background: {e}");
    }
}

/// What a new player is made from.
struct PlayerParts {
    config: PlayerConfig,
    mixer: SoftMixer,
    eq: Arc<EqControl>,
    pcm: Arc<PcmEnv>,
}

/// Create a player of `source`'s kind for `slot`, with Truffle's output wired to the slot's fades
/// and problems.
fn spawn_player(parts: &PlayerParts, source: SoundSource, session: Session, index: usize, slot: &mut Slot) -> NewStream {
    let sink = output::SinkParts { problem: slot.problem.clone(), gain: slot.gain.clone(), eq: parts.eq.clone() };
    let (player, events) = match source {
        SoundSource::Spotify => {
            let player = Player::new(
                parts.config.clone(),
                session,
                parts.mixer.get_soft_volume(),
                move || Box::new(output::TruffleSink::new(sink)) as Box<dyn Sink>,
            );
            let events = player.get_player_event_channel();
            (SlotPlayer::Spotify(player), events)
        }
        SoundSource::YouTube => {
            let (player, events) = PcmPlayer::new(parts.pcm.clone(), parts.mixer.get_soft_volume(), move || {
                Box::new(output::PcmOutput::new(sink)) as Box<dyn PcmSink>
            });
            (SlotPlayer::Pcm(player), events)
        }
    };
    slot.generation += 1;
    slot.player = Some(player);
    slot.tracking = Tracking::default();
    slot.preloaded = None;
    NewStream { slot: index, generation: slot.generation, events }
}

// ---------------------------------------------------------------------------------------------
// Player slots and crossfades

/// A slot's player: librespot's (Spotify plays the song) or Truffle's own (decoded YouTube audio).
enum SlotPlayer {
    Spotify(Arc<Player>),
    Pcm(PcmPlayer),
}

impl SlotPlayer {
    fn source(&self) -> SoundSource {
        match self {
            Self::Spotify(_) => SoundSource::Spotify,
            Self::Pcm(_) => SoundSource::YouTube,
        }
    }

    fn load(&self, uri: SpotifyUri, start_playing: bool, position_ms: u32) {
        match self {
            Self::Spotify(player) => player.load(uri, start_playing, position_ms),
            Self::Pcm(player) => player.load(uri, start_playing, position_ms),
        }
    }

    fn preload(&self, uri: SpotifyUri) {
        match self {
            Self::Spotify(player) => player.preload(uri),
            Self::Pcm(player) => player.preload(uri),
        }
    }

    fn play(&self) {
        match self {
            Self::Spotify(player) => player.play(),
            Self::Pcm(player) => player.play(),
        }
    }

    fn pause(&self) {
        match self {
            Self::Spotify(player) => player.pause(),
            Self::Pcm(player) => player.pause(),
        }
    }

    fn stop(&self) {
        match self {
            Self::Spotify(player) => player.stop(),
            Self::Pcm(player) => player.stop(),
        }
    }

    fn seek(&self, position_ms: u32) {
        match self {
            Self::Spotify(player) => player.seek(position_ms),
            Self::Pcm(player) => player.seek(position_ms),
        }
    }

    /// Only librespot's player uses the session.
    fn set_session(&self, session: Session) {
        if let Self::Spotify(player) = self {
            player.set_session(session);
        }
    }
}

/// One player and what belongs to it.
struct Slot {
    player: Option<SlotPlayer>,
    /// Bumped for each player created in this slot, so the event stream of a player that was
    /// shut down on purpose is never mistaken for the current one's.
    generation: u64,
    tracking: Tracking,
    /// The track last preloaded on this player (until its next load).
    preloaded: Option<SpotifyUri>,
    /// This player's fade envelope (times the engine-wide one), applied while its audio plays.
    gain: Arc<OutputGain>,
    problem: output::ProblemSlot,
    /// From when on this player has surely played out what it had queued when last stopped.
    quiet_at: Instant,
}

impl Slot {
    fn new(global: &Arc<GainControl>) -> Self {
        Self {
            player: None,
            generation: 0,
            tracking: Tracking::default(),
            preloaded: None,
            gain: Arc::new(OutputGain::new(global.clone())),
            problem: output::ProblemSlot::default(),
            quiet_at: Instant::now(),
        }
    }

    /// Stop the player. Its queued audio still plays out, under its current fade.
    fn halt(&mut self) {
        self.tracking.clear();
        if let Some(player) = self.player.as_ref() {
            player.stop();
            self.quiet_at = Instant::now() + DRAIN_TIME;
        }
    }

    /// Fade out quickly and stop.
    fn stop_gently(&mut self) {
        self.gain.own.ramp_to(0.0, CANCEL_RAMP, Curve::EqualPower);
        self.halt();
    }
}

#[derive(Debug, Clone, Copy)]
struct Crossfade {
    id: u64,
    /// The slot fading out, until its player is stopped.
    outgoing: Option<usize>,
    fade: Duration,
    /// When the old track's audio runs out.
    outgoing_ends: Instant,
    /// The new track is playing and the fades are running.
    started: bool,
}

/// State shared by the engine and its event task.
struct Shared {
    slots: [Slot; 2],
    active: usize,
    crossfade: Option<Crossfade>,
    next_fade_id: u64,
    /// Crossfading is on: keep the second player between crossfades.
    keep_second: bool,
    /// The engine was dropped: every player is shutting down.
    closing: bool,
    /// The kind of player loads go to.
    source: SoundSource,
}

/// A new player's event stream, for the event task.
struct NewStream {
    slot: usize,
    generation: u64,
    events: PlayerEventChannel,
}

impl Shared {
    fn new(global: &Arc<GainControl>, keep_second: bool, source: SoundSource) -> Self {
        Self {
            slots: [Slot::new(global), Slot::new(global)],
            active: 0,
            crossfade: None,
            next_fade_id: 0,
            keep_second,
            closing: false,
            source,
        }
    }

    fn active(&self) -> &Slot {
        &self.slots[self.active]
    }

    fn active_mut(&mut self) -> &mut Slot {
        &mut self.slots[self.active]
    }

    /// End a crossfade early: the old track fades out quickly and the current one comes up to
    /// full volume. A no-op when there is none.
    fn settle(&mut self) {
        if let Some(crossfade) = self.crossfade.take() {
            log::debug!("crossfade cut short");
            if let Some(outgoing) = crossfade.outgoing {
                self.slots[outgoing].stop_gently();
            }
        }
        let gain = &self.active().gain.own;
        if !gain.is_unity() {
            gain.ramp_to(1.0, CANCEL_RAMP, Curve::EqualPower);
        }
        self.retire_idle();
    }

    /// Shut the idle player down when crossfading is off (it holds a thread and decoder state).
    fn retire_idle(&mut self) {
        if self.keep_second || self.crossfade.is_some() {
            return;
        }
        let idle = 1 - self.active;
        self.slots[idle].preloaded = None;
        if let Some(player) = self.slots[idle].player.take() {
            log::debug!("closing the second player (crossfade is off)");
            player.stop();
            drop_players_in_background(vec![player]);
        }
    }

    /// Map an event of the player in `slot`. Returns the engine events to report and, once a
    /// crossfade's fades have started, when to stop the old player (`(crossfade id, delay)`).
    fn on_event(&mut self, slot: usize, event: PlayerEvent) -> (Vec<EngineEvent>, Option<(u64, Duration)>) {
        if slot != self.active {
            self.on_idle_event(slot, &event);
            return (Vec::new(), None);
        }
        let mapped = self.slots[slot].tracking.on_event(event);
        let mut stop_after = None;
        if self.crossfade.is_some_and(|c| !c.started) {
            if mapped.iter().any(|e| matches!(e, EngineEvent::Playing { .. })) {
                stop_after = self.start_fades(true);
            } else if mapped.iter().any(|e| matches!(e, EngineEvent::Unavailable { .. })) {
                // The new track can't play: let the old one fade out while the controller moves on.
                stop_after = self.start_fades(false);
            }
        }
        (mapped, stop_after)
    }

    /// The new track is playing (or failed): run the fades.
    fn start_fades(&mut self, incoming_playing: bool) -> Option<(u64, Duration)> {
        let crossfade = self.crossfade.as_mut()?;
        crossfade.started = true;
        let Crossfade { id, outgoing, fade, outgoing_ends, .. } = *crossfade;
        let incoming = &self.slots[self.active].gain.own;
        if incoming_playing {
            incoming.ramp_to(1.0, fade, Curve::EqualPower);
        } else {
            incoming.set(1.0);
        }
        let Some(outgoing) = outgoing else {
            // The old track already ended (the new one took long to load).
            self.crossfade = None;
            self.retire_idle();
            return None;
        };
        let fade_out = fade.min(outgoing_ends.saturating_duration_since(Instant::now())).max(MIN_FADE_OUT);
        self.slots[outgoing].gain.own.ramp_to(0.0, fade_out, Curve::EqualPower);
        log::debug!("crossfade running: in over {fade:?}, out over {fade_out:?}");
        Some((id, fade_out + FADE_STOP_MARGIN))
    }

    /// Events of the idle player are not reported; they only tell when the old track is done.
    fn on_idle_event(&mut self, slot: usize, event: &PlayerEvent) {
        let outgoing = self.crossfade.is_some_and(|c| c.outgoing == Some(slot));
        // Decoded to its end, or its output failed: stopping lets the queued audio play out
        // under the fade and closes its output.
        if outgoing && matches!(event, PlayerEvent::EndOfTrack { .. } | PlayerEvent::Paused { .. }) {
            self.slots[slot].halt();
            self.outgoing_stopped();
        }
    }

    /// The fade-out of crossfade `id` has reached silence.
    fn finish_fade_out(&mut self, id: u64) {
        let Some(crossfade) = self.crossfade.filter(|c| c.id == id) else { return };
        if let Some(outgoing) = crossfade.outgoing {
            self.slots[outgoing].halt();
        }
        self.outgoing_stopped();
    }

    fn outgoing_stopped(&mut self) {
        let Some(crossfade) = self.crossfade.as_mut() else { return };
        crossfade.outgoing = None;
        if crossfade.started {
            self.crossfade = None;
            self.retire_idle();
        }
    }
}

/// Wait for a crossfade's fade-out to finish, then stop the old player.
async fn finish_fade_out(shared: Weak<Mutex<Shared>>, id: u64, after: Duration) {
    tokio::time::sleep(after).await;
    if let Some(shared) = shared.upgrade() {
        lock(&shared).finish_fade_out(id);
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration helpers

fn player_config(settings: &Settings) -> PlayerConfig {
    PlayerConfig {
        bitrate: bitrate_from_setting(&settings.bitrate),
        gapless: true,
        normalisation: settings.normalisation,
        // Track gain suits shuffled playback; Basic is clip-safe and cheaper than the limiter.
        normalisation_type: NormalisationType::Track,
        normalisation_method: NormalisationMethod::Basic,
        // Output is f32, so there is nothing to dither.
        ditherer: None,
        // The UI extrapolates the position itself; no periodic position events needed.
        position_update_interval: None,
        ..PlayerConfig::default()
    }
}

fn bitrate_from_setting(setting: &str) -> Bitrate {
    match setting.trim().to_ascii_lowercase().as_str() {
        "low" | "96" => Bitrate::Bitrate96,
        "normal" | "160" => Bitrate::Bitrate160,
        "high" | "320" => Bitrate::Bitrate320,
        other => {
            log::warn!("unknown audio quality setting {other:?}, using high quality");
            Bitrate::Bitrate320
        }
    }
}

/// Maps the 0..=100 volume slider onto the SoftMixer's 0..=65535 range, which (with
/// `VolumeCtrl::Linear`) is a plain amplitude factor. A cubic taper (amplitude = x^3, about
/// -18 dB at half way) makes equal slider steps sound like roughly equal loudness steps.
/// 0 is true silence; every other setting stays audible-in-principle (never rounds to 0).
fn volume_to_mixer(volume: u8) -> u16 {
    let volume = volume.min(100);
    if volume == 0 {
        return 0;
    }
    let x = f64::from(volume) / 100.0;
    let amplitude = x * x * x;
    let scaled = (amplitude * f64::from(u16::MAX)).round();
    // `scaled` is within 0..=65535 because amplitude is within 0..=1.
    (scaled as u16).max(1)
}

/// Parses a URI the player can play: tracks and podcast episodes.
fn parse_playable_uri(uri: &str) -> Result<SpotifyUri> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        bail!("No track was given to play.");
    }
    let parsed = SpotifyUri::from_uri(trimmed)
        .map_err(|_| anyhow!("\"{trimmed}\" isn't a valid Spotify track link."))?;
    match parsed {
        SpotifyUri::Track { .. } | SpotifyUri::Episode { .. } => Ok(parsed),
        SpotifyUri::Local { .. } => bail!("Local files can't be played in Truffle."),
        other => bail!(
            "Only songs and podcast episodes can be played (this is a {} link).",
            other.item_type()
        ),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Event forwarding

enum Received {
    NewStream(Option<NewStream>),
    Event(usize, Option<PlayerEvent>),
}

async fn next_event(stream: &mut Option<(u64, PlayerEventChannel)>) -> Option<PlayerEvent> {
    match stream {
        Some((_, events)) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// Maps the players' events and reports the active one's. Ends (closing the engine's event
/// channel) when the active player's thread dies, or once the engine is dropped and every player
/// has shut down.
async fn forward_player_events(
    shared: Arc<Mutex<Shared>>,
    first: NewStream,
    mut new_streams: UnboundedReceiver<NewStream>,
    events: UnboundedSender<EngineEvent>,
) {
    let mut streams: [Option<(u64, PlayerEventChannel)>; 2] = [None, None];
    streams[first.slot] = Some((first.generation, first.events));
    let mut accepting = true;
    loop {
        let received = {
            let [first, second] = &mut streams;
            tokio::select! {
                added = new_streams.recv(), if accepting => Received::NewStream(added),
                event = next_event(first) => Received::Event(0, event),
                event = next_event(second) => Received::Event(1, event),
            }
        };
        match received {
            Received::NewStream(Some(stream)) => streams[stream.slot] = Some((stream.generation, stream.events)),
            Received::NewStream(None) => accepting = false,
            Received::Event(slot, Some(event)) => {
                log::trace!("player {slot} event: {event:?}");
                let generation = streams[slot].as_ref().map_or(0, |(generation, _)| *generation);
                // Map and send under the lock, so a `load`/`stop` can't slip in between the two
                // and leave an already-stale event to be delivered after it.
                let delivered = {
                    let mut state = lock(&shared);
                    // An event of a player that has since been replaced in this slot.
                    if state.slots[slot].generation != generation {
                        continue;
                    }
                    let (mapped, stop_after) = state.on_event(slot, event);
                    if let Some((id, after)) = stop_after {
                        tokio::spawn(finish_fade_out(Arc::downgrade(&shared), id, after));
                    }
                    mapped.into_iter().all(|mapped| events.send(mapped).is_ok())
                };
                if !delivered {
                    break;
                }
            }
            Received::Event(slot, None) => {
                let generation = streams[slot].take().map_or(0, |(generation, _)| generation);
                let mut state = lock(&shared);
                let died = !state.closing
                    && state.slots[slot].generation == generation
                    && state.slots[slot].player.is_some();
                if died {
                    if slot == state.active {
                        log::error!("the audio player stopped unexpectedly");
                        break;
                    }
                    log::warn!("the second audio player stopped unexpectedly");
                    if let Some(player) = state.slots[slot].player.take() {
                        drop_players_in_background(vec![player]);
                    }
                }
            }
        }
        if !accepting && streams.iter().all(Option::is_none) {
            break;
        }
    }
    log::debug!("player event streams closed");
}

/// Emits `SessionInvalid` once per session. `Engine::set_session` re-arms it; dropping the
/// engine (or the player thread ending) ends the task.
async fn watch_session(mut session_rx: watch::Receiver<Session>, events: WeakUnboundedSender<EngineEvent>) {
    let mut tick = tokio::time::interval(SESSION_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reported = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let Some(events) = events.upgrade() else {
                    break;
                };
                if !reported && session_rx.borrow().is_invalid() {
                    reported = true;
                    log::warn!("the Spotify connection was lost");
                    if events.send(EngineEvent::SessionInvalid).is_err() {
                        break;
                    }
                }
            }
            changed = session_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                reported = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pcm::fakes::{FakeResolver, track};
    use super::*;

    const TRACK_A: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const TRACK_B: &str = "spotify:track:5sWHDYs0csV6RS48xBl0tH";

    fn id(uri: &str) -> SpotifyUri {
        SpotifyUri::from_uri(uri).unwrap()
    }

    #[test]
    fn engine_can_be_shared_across_tasks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Engine>();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn engine_starts_and_stops_without_a_connection() {
        // An unconnected session is enough to build the player; nothing touches the network.
        let session = Session::new(librespot_core::SessionConfig::default(), None);
        let paths = Paths { config: "unused".into(), cache: "unused".into() };
        let youtube = YouTube::new("unused".into(), None);
        let (engine, mut events) = Engine::new(session, &Settings::default(), &paths, youtube).unwrap();
        engine.set_volume(40);
        engine.set_equalizer(&EqSettings { enabled: true, gains_db: [3.0; 10], ..EqSettings::default() });
        engine.set_fade_gain(0.5);
        assert!(engine.load("spotify:album:4aawyAB9vmqN3uQ7FjRGTy", true, 0).is_err());
        assert!(engine.preload("garbage").is_err());
        assert!(engine.crossfade_to("garbage", Duration::from_secs(5), Duration::from_secs(5)).is_err());
        // Crossfade on starts the second player when it is first needed; off shuts it down.
        engine.set_crossfade(6);
        engine.preload_for_crossfade(TRACK_B).unwrap();
        assert!(lock(&engine.shared).slots[1].player.is_some());
        engine.set_crossfade(0);
        assert!(lock(&engine.shared).slots[1].player.is_none());
        engine.stop();
        assert_eq!(events.recv().await, Some(EngineEvent::Stopped));
        assert_eq!(engine.take_audio_problem(), None);
        drop(engine);
        // Once the player threads have ended, the event channel closes.
        let closed = tokio::time::timeout(Duration::from_secs(10), events.recv()).await;
        assert_eq!(closed, Ok(None));
    }

    /// Plays one second of silence through the real default output device.
    #[test]
    #[ignore = "opens the default audio device; run with --ignored"]
    fn audio_output_smoke() {
        use librespot_playback::audio_backend::Sink as _;
        use librespot_playback::convert::Converter;
        use librespot_playback::decoder::AudioPacket;

        let problem = output::ProblemSlot::default();
        let parts = output::SinkParts {
            problem: problem.clone(),
            gain: Arc::new(OutputGain::new(Arc::new(GainControl::new(1.0)))),
            eq: Arc::new(EqControl::new(&EqSettings::default())),
        };
        let mut sink = output::TruffleSink::new(parts);
        sink.start().expect("start never fails");
        if lock(&problem).is_some() {
            // No device on this machine: explained, not a panic or a failed start.
            return;
        }
        let mut converter = Converter::new(None);
        for _ in 0..44 {
            sink.write(AudioPacket::Samples(vec![0.0; 2004]), &mut converter).unwrap();
        }
        sink.stop().unwrap();
        // Re-opening after a stop works too (this is what resuming from pause does).
        sink.start().unwrap();
        sink.stop().unwrap();
        assert_eq!(lock(&problem).take(), None);
    }

    // ---- sound source (songs are loaded paused, so no audio device is opened) ----

    fn engine_with_fake_youtube(source: SoundSource) -> (Engine, UnboundedReceiver<EngineEvent>, Arc<FakeResolver>) {
        let session = Session::new(librespot_core::SessionConfig::default(), None);
        let resolver = Arc::new(FakeResolver::default());
        let settings = Settings { sound_source: source, crossfade_secs: 6, ..Settings::default() };
        let (engine, events) = Engine::with_resolver(session, &settings, resolver.clone()).unwrap();
        (engine, events, resolver)
    }

    async fn next_event(events: &mut UnboundedReceiver<EngineEvent>) -> EngineEvent {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
        event.expect("an event within 5 s").expect("the engine is running")
    }

    fn active_source(engine: &Engine) -> Option<SoundSource> {
        lock(&engine.shared).active().player.as_ref().map(SlotPlayer::source)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn youtube_songs_report_the_same_events() {
        let (engine, mut events, resolver) = engine_with_fake_youtube(SoundSource::YouTube);
        assert_eq!(active_source(&engine), Some(SoundSource::YouTube), "the first player follows the setting");
        resolver.song(TRACK_A, 10, Ok((44_100, 200_000)));
        engine.hint(&track(TRACK_A, 200_000));
        engine.load(TRACK_A, false, 5_000).unwrap();
        assert_eq!(next_event(&mut events).await, EngineEvent::Loading { uri: TRACK_A.into() });
        assert_eq!(next_event(&mut events).await, EngineEvent::Paused { uri: TRACK_A.into(), position_ms: 5_000 });
        engine.seek(20_000);
        assert_eq!(next_event(&mut events).await, EngineEvent::Position { uri: TRACK_A.into(), position_ms: 20_000 });
        // A song without details can't be looked up: unavailable, with a reason.
        engine.load(TRACK_B, false, 0).unwrap();
        assert_eq!(next_event(&mut events).await, EngineEvent::Loading { uri: TRACK_B.into() });
        assert_eq!(next_event(&mut events).await, EngineEvent::Unavailable { uri: TRACK_B.into() });
        assert_eq!(engine.take_source_problem().as_deref(), Some("Couldn't look this song up on YouTube."));
        assert_eq!(engine.take_source_problem(), None);
        // No crossfades with YouTube yet: the next song is preloaded on the one player.
        engine.preload_for_crossfade(TRACK_A).unwrap();
        assert!(lock(&engine.shared).slots[1].player.is_none());
        engine.stop();
        assert_eq!(next_event(&mut events).await, EngineEvent::Stopped);
        drop(engine);
        let closed = tokio::time::timeout(Duration::from_secs(10), events.recv()).await;
        assert_eq!(closed, Ok(None));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_youtube_failures_never_reach_the_controller() {
        let (engine, mut events, resolver) = engine_with_fake_youtube(SoundSource::YouTube);
        resolver.song(TRACK_A, 200, Err("Couldn't find this song on YouTube."));
        resolver.song(TRACK_B, 0, Ok((44_100, 200_000)));
        engine.hint(&track(TRACK_A, 200_000));
        engine.hint(&track(TRACK_B, 200_000));
        // A is replaced while it is still being looked up: only B is reported.
        engine.load(TRACK_A, false, 0).unwrap();
        engine.load(TRACK_B, false, 0).unwrap();
        assert_eq!(next_event(&mut events).await, EngineEvent::Loading { uri: TRACK_B.into() });
        assert_eq!(next_event(&mut events).await, EngineEvent::Paused { uri: TRACK_B.into(), position_ms: 0 });
        // A failed preload says nothing either; the song is reported when it is loaded.
        engine.preload(TRACK_A).unwrap();
        let quiet = tokio::time::timeout(Duration::from_millis(400), events.recv()).await;
        assert!(quiet.is_err(), "{quiet:?}");
        assert_eq!(engine.take_source_problem(), None);
        engine.load(TRACK_A, false, 0).unwrap();
        assert_eq!(next_event(&mut events).await, EngineEvent::Loading { uri: TRACK_A.into() });
        assert_eq!(next_event(&mut events).await, EngineEvent::Unavailable { uri: TRACK_A.into() });
        assert_eq!(engine.take_source_problem().as_deref(), Some("Couldn't find this song on YouTube."));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_sound_source_takes_over_at_the_next_load() {
        let (engine, mut events, resolver) = engine_with_fake_youtube(SoundSource::Spotify);
        assert_eq!(active_source(&engine), Some(SoundSource::Spotify));
        engine.preload_for_crossfade(TRACK_B).unwrap();
        assert!(lock(&engine.shared).slots[1].player.is_some(), "Spotify's second player for crossfades");
        engine.set_sound_source(SoundSource::YouTube);
        assert_eq!(active_source(&engine), Some(SoundSource::Spotify), "the current song plays on");
        resolver.song(TRACK_A, 10, Ok((44_100, 200_000)));
        engine.hint(&track(TRACK_A, 200_000));
        engine.load(TRACK_A, false, 0).unwrap();
        assert_eq!(active_source(&engine), Some(SoundSource::YouTube));
        assert!(lock(&engine.shared).slots[1].player.is_none(), "the idle Spotify player is shut down");
        // The new player's events reach the controller.
        assert_eq!(next_event(&mut events).await, EngineEvent::Loading { uri: TRACK_A.into() });
        assert_eq!(next_event(&mut events).await, EngineEvent::Paused { uri: TRACK_A.into(), position_ms: 0 });
        // Back to Spotify: until the next load the YouTube player stays, and preloads skip it.
        engine.set_sound_source(SoundSource::Spotify);
        engine.preload(TRACK_B).unwrap();
        assert_eq!(resolver.calls(TRACK_B), 0);
        assert_eq!(active_source(&engine), Some(SoundSource::YouTube));
        engine.stop();
        assert_eq!(next_event(&mut events).await, EngineEvent::Stopped);
    }

    // ---- crossfade bookkeeping (no players needed) ----

    /// Slot 0 played A; a crossfade to B on slot 1 was just requested.
    fn crossfading() -> Shared {
        let mut shared = Shared::new(&Arc::new(GainControl::new(1.0)), true, SoundSource::Spotify);
        shared.slots[1].gain.own.set(0.0);
        shared.slots[1].tracking.begin_load(TRACK_B.into(), id(TRACK_B));
        shared.active = 1;
        shared.crossfade = Some(Crossfade {
            id: 7,
            outgoing: Some(0),
            fade: Duration::from_secs(6),
            outgoing_ends: Instant::now() + Duration::from_secs(6),
            started: false,
        });
        let (mapped, _) = shared.on_event(1, PlayerEvent::PlayRequestIdChanged { play_request_id: 1 });
        assert!(mapped.is_empty());
        shared
    }

    fn playing(uri: &str, request: u64) -> PlayerEvent {
        PlayerEvent::Playing { play_request_id: request, track_id: id(uri), position_ms: 0 }
    }

    #[test]
    fn crossfade_starts_when_the_new_track_plays() {
        let mut shared = crossfading();
        // The old player's events are not reported any more.
        let hint = PlayerEvent::TimeToPreloadNextTrack { play_request_id: 9, track_id: id(TRACK_A) };
        let (mapped, stop) = shared.on_event(0, hint);
        assert!(mapped.is_empty() && stop.is_none());
        // Loading the new one does not start the fades yet.
        let loading = PlayerEvent::Loading { play_request_id: 1, track_id: id(TRACK_B), position_ms: 0 };
        let (mapped, stop) = shared.on_event(1, loading);
        assert_eq!(mapped, vec![EngineEvent::Loading { uri: TRACK_B.into() }]);
        assert!(stop.is_none());
        assert_eq!(shared.slots[1].gain.own.value(), 0.0, "silent until it plays");
        // Playing does: B fades in, A fades out, and A is stopped once silent.
        let (mapped, stop) = shared.on_event(1, playing(TRACK_B, 1));
        assert_eq!(mapped, vec![EngineEvent::Playing { uri: TRACK_B.into(), position_ms: 0 }]);
        let (fade_id, after) = stop.expect("a stop is scheduled");
        assert_eq!(fade_id, 7);
        assert!(after > Duration::from_secs(5) && after <= Duration::from_secs(6) + FADE_STOP_MARGIN, "{after:?}");
        assert!(!shared.slots[1].gain.own.is_unity() && !shared.slots[0].gain.own.is_unity());
        assert!(shared.crossfade.is_some_and(|c| c.started));
        // A second Playing (after a pause) changes nothing.
        assert!(shared.on_event(1, playing(TRACK_B, 1)).1.is_none());
        shared.finish_fade_out(7);
        assert!(shared.crossfade.is_none());
    }

    #[test]
    fn the_old_track_ending_first_finishes_the_crossfade() {
        let mut shared = crossfading();
        let end = PlayerEvent::EndOfTrack { play_request_id: 9, track_id: id(TRACK_A) };
        assert!(shared.on_event(0, end).0.is_empty());
        assert_eq!(shared.crossfade.map(|c| c.outgoing), Some(None));
        // The new track still fades in when it starts; nothing is left to stop.
        let (_, stop) = shared.on_event(1, playing(TRACK_B, 1));
        assert!(stop.is_none());
        assert!(shared.crossfade.is_none());
        assert!(!shared.slots[1].gain.own.is_unity(), "fading in");
    }

    #[test]
    fn an_unplayable_new_track_lets_the_old_one_fade_out() {
        let mut shared = crossfading();
        let failed = PlayerEvent::Unavailable { play_request_id: 1, track_id: id(TRACK_B) };
        let (mapped, stop) = shared.on_event(1, failed);
        assert_eq!(mapped, vec![EngineEvent::Unavailable { uri: TRACK_B.into() }]);
        assert!(stop.is_some());
        // The slot is ready at full volume for whatever the controller plays next.
        assert!(shared.slots[1].gain.own.is_unity());
    }

    #[test]
    fn cutting_a_crossfade_short_fades_the_old_track_out_quickly() {
        let mut shared = crossfading();
        shared.on_event(1, playing(TRACK_B, 1));
        shared.settle();
        assert!(shared.crossfade.is_none());
        // B comes up to full volume within the cancel ramp; A goes quiet as fast.
        std::thread::sleep(CANCEL_RAMP + Duration::from_millis(30));
        assert_eq!(shared.slots[1].gain.own.value(), 1.0);
        assert_eq!(shared.slots[0].gain.own.value(), 0.0);
        // A stale timer of the finished crossfade does nothing.
        shared.finish_fade_out(7);
    }

    #[test]
    fn a_short_old_track_fades_out_within_what_is_left() {
        let mut shared = crossfading();
        if let Some(crossfade) = shared.crossfade.as_mut() {
            crossfade.outgoing_ends = Instant::now() + Duration::from_secs(2);
        }
        let (_, stop) = shared.on_event(1, playing(TRACK_B, 1));
        let (_, after) = stop.expect("a stop is scheduled");
        assert!(after <= Duration::from_secs(2) + FADE_STOP_MARGIN, "{after:?}");
    }

    // ---- volume curve ----

    #[test]
    fn volume_endpoints() {
        assert_eq!(volume_to_mixer(0), 0);
        assert_eq!(volume_to_mixer(100), u16::MAX);
        assert_eq!(volume_to_mixer(255), u16::MAX, "values above 100 clamp");
    }

    #[test]
    fn volume_is_monotonic_and_never_silent_above_zero() {
        let mut previous = 0;
        for v in 1..=100u8 {
            let mixed = volume_to_mixer(v);
            assert!(mixed >= previous, "volume {v} went down");
            assert!(mixed > 0, "volume {v} is silent");
            previous = mixed;
        }
        // Strictly increasing once past the very bottom of the range.
        for v in 3..100u8 {
            assert!(volume_to_mixer(v + 1) > volume_to_mixer(v), "volume {v}->{} flat", v + 1);
        }
    }

    #[test]
    fn volume_curve_is_perceptual() {
        // Half way is about -18 dB (amplitude 1/8), well below a linear half.
        let half = f64::from(volume_to_mixer(50)) / f64::from(u16::MAX);
        assert!((half - 0.125).abs() < 0.001, "half = {half}");
        let db = 20.0 * half.log10();
        assert!((-18.5..-17.5).contains(&db), "half way is {db} dB");
    }

    // ---- URIs ----

    #[test]
    fn accepts_tracks_and_episodes() {
        assert!(matches!(parse_playable_uri(TRACK_A), Ok(SpotifyUri::Track { .. })));
        assert!(matches!(
            parse_playable_uri("spotify:episode:512ojhOuo1ktJprKbVcKyQ"),
            Ok(SpotifyUri::Episode { .. })
        ));
        assert!(parse_playable_uri(&format!("  {TRACK_A}\n")).is_ok(), "surrounding whitespace");
    }

    #[test]
    fn rejects_unplayable_or_malformed_uris() {
        for bad in [
            "",
            "   ",
            "not a uri",
            "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC",
            "spotify:track:short",
            "spotify:track",
            "spotify:album:4aawyAB9vmqN3uQ7FjRGTy",
            "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M",
            "spotify:local:Artist:Album:Title:200",
        ] {
            assert!(parse_playable_uri(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn bitrate_settings() {
        assert_eq!(bitrate_from_setting("low"), Bitrate::Bitrate96);
        assert_eq!(bitrate_from_setting("normal"), Bitrate::Bitrate160);
        assert_eq!(bitrate_from_setting("high"), Bitrate::Bitrate320);
        assert_eq!(bitrate_from_setting(" High "), Bitrate::Bitrate320);
        assert_eq!(bitrate_from_setting("160"), Bitrate::Bitrate160);
        assert_eq!(bitrate_from_setting("bogus"), Bitrate::Bitrate320);
    }
}
