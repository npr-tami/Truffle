//! Truffle's own player for decoded audio (the YouTube sound source). It stands in for librespot's
//! `Player` in a slot and speaks its language: the same commands, and the same `PlayerEvent`s in
//! the same order, so `Tracking`, the event task and the crossfade bookkeeping treat it like any
//! other player.
//!
//! - A song is found and fetched on the tokio runtime (see [`PcmResolver`]) and handed to the
//!   player's thread, which decodes it and writes it to Truffle's output (equalizer, fades and
//!   device problems included). A newer load cancels an older fetch.
//! - The output holds about half a second of audio, and that back-pressure paces the thread.
//! - The user's volume is applied here (librespot applies it inside its player). No normalisation.
//! - The next song is asked for a few seconds after a song starts, not near its end as librespot
//!   does: finding and fetching it takes several seconds.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use futures::future::BoxFuture;
use librespot_core::SpotifyUri;
use librespot_playback::audio_backend::SinkResult;
use librespot_playback::mixer::VolumeGetter;
use librespot_playback::player::{PlayerEvent, PlayerEventChannel};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;

use crate::model::TrackInfo;
use crate::youtube::YouTube;

use super::lock;
use super::m4a::M4aDecoder;
use super::output::ProblemSlot;

/// How many songs' details are kept for finding them on YouTube.
const MAX_HINTS: usize = 64;
/// The next song is asked for once this much of the current one has played.
const PRELOAD_AFTER: Duration = Duration::from_secs(3);

const NO_DETAILS: &str = "Couldn't look this song up on YouTube.";
/// Reasons the tests' fake resolver gives (the real one passes the YouTube module's own).
#[cfg(test)]
const NOT_FOUND: &str = "Couldn't find this song on YouTube.";
#[cfg(test)]
const DOWNLOAD_FAILED: &str = "Couldn't download this song from YouTube.";
const UNPLAYABLE: &str = "Couldn't play this song's YouTube audio.";

/// A song's audio, decoded to interleaved stereo samples.
pub(super) trait PcmDecoder: Send {
    /// Samples per second (per channel).
    fn rate(&self) -> u32;
    /// Where the audio after the last packet starts, in ms.
    fn position_ms(&self) -> u32;
    /// Jumps to `position_ms` (kept within the song) and returns where it landed.
    fn seek(&mut self, position_ms: u32) -> Result<u32>;
    /// The next packet (about 1024 frames), `None` at the end of the song.
    fn next_packet(&mut self) -> Result<Option<Vec<f64>>>;
}

/// Finds and fetches a song's audio (on YouTube; a fake in tests).
pub(super) trait PcmResolver: Send + Sync {
    /// The song's decoder, or why it can't be played, in plain words.
    fn open(&self, track: TrackInfo) -> BoxFuture<'static, Result<Box<dyn PcmDecoder>, String>>;
}

/// Where a PCM player's audio goes: Truffle's output (a recorder in tests).
pub(super) trait PcmSink {
    fn start(&mut self) -> SinkResult<()>;
    fn stop(&mut self) -> SinkResult<()>;
    /// Interleaved stereo at `rate` samples per second.
    fn write(&mut self, samples: Vec<f64>, rate: u32) -> SinkResult<()>;
}

/// Songs' details by URI, most recent last: a PCM player only gets a URI to play.
#[derive(Default)]
pub(super) struct Hints(Mutex<VecDeque<(SpotifyUri, TrackInfo)>>);

impl Hints {
    pub(super) fn remember(&self, track: &TrackInfo) {
        let Ok(uri) = SpotifyUri::from_uri(track.uri.trim()) else { return };
        let mut hints = lock(&self.0);
        hints.retain(|(known, _)| *known != uri);
        if hints.len() >= MAX_HINTS {
            hints.pop_front();
        }
        hints.push_back((uri, track.clone()));
    }

    fn get(&self, uri: &SpotifyUri) -> Option<TrackInfo> {
        lock(&self.0).iter().rev().find(|(known, _)| known == uri).map(|(_, track)| track.clone())
    }
}

/// What every PCM player shares with the engine.
pub(super) struct PcmEnv {
    pub(super) resolver: Arc<dyn PcmResolver>,
    pub(super) hints: Hints,
    /// Why the last song couldn't be played, in plain words.
    pub(super) problem: ProblemSlot,
    pub(super) runtime: Handle,
}

/// Finds songs on YouTube and decodes their m4a audio.
pub(super) struct YouTubeResolver(pub(super) YouTube);

impl PcmResolver for YouTubeResolver {
    fn open(&self, track: TrackInfo) -> BoxFuture<'static, Result<Box<dyn PcmDecoder>, String>> {
        let youtube = self.0.clone();
        Box::pin(async move {
            youtube.tools()?;
            let audio = match youtube.audio(&track).await {
                Ok(audio) => audio,
                Err(e) => {
                    log::warn!("no YouTube audio for \"{}\": {e:#}", track.name);
                    // The YouTube module's errors are short plain sentences.
                    return Err(e.to_string());
                }
            };
            log::info!("playing \"{}\" from YouTube ({}, {} KB)", track.name, audio.video_id, audio.bytes.len() / 1024);
            let duration_ms = audio.duration_ms.filter(|&ms| ms > 0).unwrap_or(track.duration_ms);
            match M4aDecoder::open(audio.bytes, duration_ms) {
                Ok(decoder) => Ok(Box::new(decoder) as Box<dyn PcmDecoder>),
                Err(e) => {
                    log::warn!("couldn't decode the YouTube audio of \"{}\" ({}): {e:#}", track.name, audio.video_id);
                    youtube.discard(&track.uri, &audio.video_id);
                    Err(UNPLAYABLE.to_owned())
                }
            }
        })
    }
}

enum Command {
    Load { uri: SpotifyUri, play: bool, position_ms: u32 },
    Preload(SpotifyUri),
    Play,
    Pause,
    Stop,
    Seek(u32),
    /// A song's audio is out of date (another video was picked): it isn't used again.
    Forget(SpotifyUri),
    /// A fetch finished.
    Resolved { job: u64, result: Result<Box<dyn PcmDecoder>, String> },
    Quit,
}

/// A player for decoded audio, driven like librespot's `Player`.
pub(super) struct PcmPlayer {
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl PcmPlayer {
    /// Starts the player's thread; `sink` is built on it. Returns the player's event stream.
    pub(super) fn new<F>(env: Arc<PcmEnv>, volume: Box<dyn VolumeGetter + Send>, sink: F) -> (Self, PlayerEventChannel)
    where
        F: FnOnce() -> Box<dyn PcmSink> + Send + 'static,
    {
        let (commands, receiver) = mpsc::channel();
        let (events, event_stream) = unbounded_channel();
        let reply = commands.clone();
        let spawned = std::thread::Builder::new().name("truffle-pcm-player".into()).spawn(move || {
            let worker = Worker {
                env,
                commands: receiver,
                reply,
                events,
                sink: sink(),
                sink_running: false,
                volume,
                last_request: 0,
                last_job: 0,
                state: State::Stopped,
                preload: Preload::None,
            };
            worker.run();
        });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(e) => {
                // The event stream is closed then, which the engine reports as a dead player.
                log::error!("couldn't start the YouTube player: {e}");
                None
            }
        };
        (Self { commands, thread }, event_stream)
    }

    pub(super) fn load(&self, uri: SpotifyUri, play: bool, position_ms: u32) {
        self.command(Command::Load { uri, play, position_ms });
    }

    pub(super) fn preload(&self, uri: SpotifyUri) {
        self.command(Command::Preload(uri));
    }

    pub(super) fn play(&self) {
        self.command(Command::Play);
    }

    pub(super) fn pause(&self) {
        self.command(Command::Pause);
    }

    pub(super) fn stop(&self) {
        self.command(Command::Stop);
    }

    pub(super) fn seek(&self, position_ms: u32) {
        self.command(Command::Seek(position_ms));
    }

    pub(super) fn forget(&self, uri: SpotifyUri) {
        self.command(Command::Forget(uri));
    }

    fn command(&self, command: Command) {
        if self.commands.send(command).is_err() {
            log::debug!("the YouTube player has stopped");
        }
    }
}

impl Drop for PcmPlayer {
    /// Waits for the thread to finish what it has queued (like librespot's `Player`).
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Quit);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::error!("the YouTube player thread panicked");
        }
    }
}

/// A fetch running on the runtime.
struct Job {
    id: u64,
    abort: AbortHandle,
}

/// The song loaded in the player.
struct Loaded {
    request: u64,
    uri: SpotifyUri,
    decoder: Box<dyn PcmDecoder>,
    playing: bool,
    /// Decoded to its end (`EndOfTrack` sent).
    ended: bool,
    /// `TimeToPreloadNextTrack` sent.
    hinted: bool,
    /// Frames written since it started.
    frames: u64,
    /// Another video was picked for it: a reload fetches it anew.
    stale: bool,
}

enum State {
    Stopped,
    Loading { request: u64, uri: SpotifyUri, play: bool, position_ms: u32, job: Job },
    Loaded(Box<Loaded>),
}

enum Preload {
    None,
    Loading { uri: SpotifyUri, job: Job },
    Ready { uri: SpotifyUri, decoder: Box<dyn PcmDecoder> },
}

impl Preload {
    fn uri(&self) -> Option<&SpotifyUri> {
        match self {
            Self::None => None,
            Self::Loading { uri, .. } | Self::Ready { uri, .. } => Some(uri),
        }
    }
}

/// The player's thread.
struct Worker {
    env: Arc<PcmEnv>,
    commands: Receiver<Command>,
    /// Handed to fetches, which report back with it.
    reply: Sender<Command>,
    events: UnboundedSender<PlayerEvent>,
    sink: Box<dyn PcmSink>,
    sink_running: bool,
    volume: Box<dyn VolumeGetter + Send>,
    last_request: u64,
    last_job: u64,
    state: State,
    preload: Preload,
}

impl Worker {
    fn run(mut self) {
        loop {
            let command = if self.is_playing() {
                match self.commands.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => break,
                }
            } else {
                match self.commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => break,
                }
            };
            match command {
                Some(Command::Quit) => break,
                Some(command) => self.handle(command),
                None => self.play_packet(),
            }
        }
        if let State::Loading { job, .. } = &self.state {
            job.abort.abort();
        }
        if let Preload::Loading { job, .. } = &self.preload {
            job.abort.abort();
        }
        self.ensure_sink_stopped();
    }

    fn is_playing(&self) -> bool {
        matches!(&self.state, State::Loaded(loaded) if loaded.playing && !loaded.ended)
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Load { uri, play, position_ms } => self.load(uri, play, position_ms),
            Command::Preload(uri) => self.preload(uri),
            Command::Play => self.play(),
            Command::Pause => self.pause(),
            Command::Stop => self.stop(),
            Command::Seek(position_ms) => self.seek(position_ms),
            Command::Forget(uri) => self.forget(&uri),
            Command::Resolved { job, result } => self.resolved(job, result),
            Command::Quit => {}
        }
    }

    fn send(&self, event: PlayerEvent) {
        // Nobody listening any more: the engine is shutting this player down.
        let _ = self.events.send(event);
    }

    fn load(&mut self, uri: SpotifyUri, play: bool, position_ms: u32) {
        self.last_request += 1;
        let request = self.last_request;
        self.send(PlayerEvent::PlayRequestIdChanged { play_request_id: request });
        match std::mem::replace(&mut self.state, State::Stopped) {
            // The same song again (repeat one, or a reload at another position): keep its audio.
            State::Loaded(loaded) if loaded.uri == uri && !loaded.stale => {
                self.start(request, uri, loaded.decoder, play, position_ms);
                return;
            }
            // Already on its way: wait for it, for this request.
            State::Loading { uri: loading, job, .. } if loading == uri => {
                self.send(PlayerEvent::Loading { play_request_id: request, track_id: uri.clone(), position_ms });
                self.state = State::Loading { request, uri, play, position_ms, job };
                return;
            }
            State::Loading { job, .. } => job.abort.abort(),
            State::Loaded(_) | State::Stopped => {}
        }
        if let Preload::Ready { uri: preloaded, .. } = &self.preload
            && *preloaded == uri
            && let Preload::Ready { decoder, .. } = std::mem::replace(&mut self.preload, Preload::None)
        {
            self.start(request, uri, decoder, play, position_ms);
            return;
        }
        self.send(PlayerEvent::Loading { play_request_id: request, track_id: uri.clone(), position_ms });
        let job = match std::mem::replace(&mut self.preload, Preload::None) {
            Preload::Loading { uri: preloading, job } if preloading == uri => Some(job),
            other => {
                self.preload = other;
                self.resolve(&uri)
            }
        };
        match job {
            Some(job) => self.state = State::Loading { request, uri, play, position_ms, job },
            None => self.unavailable(request, uri, NO_DETAILS),
        }
    }

    /// Another video was picked for `uri`: a ready or coming preload of it is dropped, and the
    /// loaded song (if it is that one) is fetched anew on its next load.
    fn forget(&mut self, uri: &SpotifyUri) {
        if self.preload.uri() == Some(uri) {
            if let Preload::Loading { job, .. } = &self.preload {
                job.abort.abort();
            }
            self.preload = Preload::None;
        }
        if let State::Loaded(loaded) = &mut self.state
            && loaded.uri == *uri
        {
            loaded.stale = true;
        }
        // Still fetching the old video: fetch the new one instead, for the same request.
        let loading_it = matches!(&self.state, State::Loading { uri: loading, .. } if loading == uri);
        if loading_it
            && let Some(fresh) = self.resolve(uri)
            && let State::Loading { job, .. } = &mut self.state
        {
            job.abort.abort();
            *job = fresh;
        }
    }

    /// Starts fetching `uri` on the runtime. `None` without the song's details.
    fn resolve(&mut self, uri: &SpotifyUri) -> Option<Job> {
        let track = self.env.hints.get(uri)?;
        self.last_job += 1;
        let id = self.last_job;
        let reply = self.reply.clone();
        let open = self.env.resolver.open(track);
        let task = self.env.runtime.spawn(async move {
            let result = open.await;
            let _ = reply.send(Command::Resolved { job: id, result });
        });
        Some(Job { id, abort: task.abort_handle() })
    }

    fn resolved(&mut self, job: u64, result: Result<Box<dyn PcmDecoder>, String>) {
        if let State::Loading { job: loading, .. } = &self.state
            && loading.id == job
            && let State::Loading { request, uri, play, position_ms, .. } =
                std::mem::replace(&mut self.state, State::Stopped)
        {
            match result {
                Ok(decoder) => self.start(request, uri, decoder, play, position_ms),
                Err(problem) => self.unavailable(request, uri, &problem),
            }
            return;
        }
        if let Preload::Loading { job: preloading, .. } = &self.preload
            && preloading.id == job
            && let Preload::Loading { uri, .. } = std::mem::replace(&mut self.preload, Preload::None)
        {
            match result {
                Ok(decoder) => self.preload = Preload::Ready { uri, decoder },
                // Like librespot: reported with the current request, which marks a failed preload.
                Err(problem) => {
                    log::info!("couldn't preload {uri} from YouTube: {problem}");
                    self.send(PlayerEvent::Unavailable { play_request_id: self.last_request, track_id: uri });
                }
            }
        }
        // Otherwise it was for a load or preload that has been replaced since.
    }

    fn start(&mut self, request: u64, uri: SpotifyUri, mut decoder: Box<dyn PcmDecoder>, play: bool, position_ms: u32) {
        if position_ms != decoder.position_ms()
            && let Err(e) = decoder.seek(position_ms)
        {
            log::warn!("YouTube player: {e:#}");
        }
        let position_ms = decoder.position_ms();
        if play {
            self.ensure_sink_running();
            self.send(PlayerEvent::Playing { play_request_id: request, track_id: uri.clone(), position_ms });
        } else {
            self.ensure_sink_stopped();
            self.send(PlayerEvent::Paused { play_request_id: request, track_id: uri.clone(), position_ms });
        }
        self.state =
            State::Loaded(Box::new(Loaded { request, uri, decoder, playing: play, ended: false, hinted: false, frames: 0, stale: false }));
    }

    fn unavailable(&mut self, request: u64, uri: SpotifyUri, problem: &str) {
        log::warn!("can't play {uri} from YouTube: {problem}");
        *lock(&self.env.problem) = Some(problem.to_owned());
        self.send(PlayerEvent::Unavailable { play_request_id: request, track_id: uri });
        self.state = State::Stopped;
    }

    fn preload(&mut self, uri: SpotifyUri) {
        if self.preload.uri() == Some(&uri) {
            return;
        }
        // Never the current song: a failure would read as the current song's.
        let current = match &self.state {
            State::Stopped => None,
            State::Loading { uri, .. } => Some(uri),
            State::Loaded(loaded) => Some(&loaded.uri),
        };
        if current == Some(&uri) {
            return;
        }
        if let Preload::Loading { job, .. } = std::mem::replace(&mut self.preload, Preload::None) {
            job.abort.abort();
        }
        match self.resolve(&uri) {
            Some(job) => self.preload = Preload::Loading { uri, job },
            None => {
                log::info!("couldn't preload {uri} from YouTube: {NO_DETAILS}");
                self.send(PlayerEvent::Unavailable { play_request_id: self.last_request, track_id: uri });
            }
        }
    }

    fn play(&mut self) {
        match &mut self.state {
            State::Loaded(loaded) if !loaded.playing && !loaded.ended => {
                loaded.playing = true;
                let event = PlayerEvent::Playing {
                    play_request_id: loaded.request,
                    track_id: loaded.uri.clone(),
                    position_ms: loaded.decoder.position_ms(),
                };
                // The output is open before "playing" is reported, as when a song starts.
                self.ensure_sink_running();
                self.send(event);
            }
            State::Loading { play, .. } => *play = true,
            _ => {}
        }
    }

    fn pause(&mut self) {
        match &mut self.state {
            State::Loaded(loaded) if loaded.playing && !loaded.ended => {
                loaded.playing = false;
                let event = PlayerEvent::Paused {
                    play_request_id: loaded.request,
                    track_id: loaded.uri.clone(),
                    position_ms: loaded.decoder.position_ms(),
                };
                // Plays out what is queued first, so the position is where the sound stopped.
                self.ensure_sink_stopped();
                self.send(event);
            }
            State::Loading { play, .. } => *play = false,
            _ => self.ensure_sink_stopped(),
        }
    }

    fn seek(&mut self, position_ms: u32) {
        match &mut self.state {
            State::Loading { position_ms: wanted, .. } => *wanted = position_ms,
            State::Loaded(loaded) if !loaded.ended => match loaded.decoder.seek(position_ms) {
                Ok(landed) => {
                    let event =
                        PlayerEvent::Seeked { play_request_id: loaded.request, track_id: loaded.uri.clone(), position_ms: landed };
                    self.send(event);
                }
                Err(e) => log::error!("YouTube player: {e:#}"),
            },
            _ => log::debug!("YouTube player: nothing to seek in"),
        }
    }

    fn stop(&mut self) {
        self.ensure_sink_stopped();
        match std::mem::replace(&mut self.state, State::Stopped) {
            State::Loaded(loaded) => {
                self.send(PlayerEvent::Stopped { play_request_id: loaded.request, track_id: loaded.uri });
            }
            State::Loading { request, uri, job, .. } => {
                job.abort.abort();
                self.send(PlayerEvent::Stopped { play_request_id: request, track_id: uri });
            }
            State::Stopped => {}
        }
    }

    /// Decodes and plays one packet of the current song.
    fn play_packet(&mut self) {
        let State::Loaded(loaded) = &mut self.state else { return };
        let mut samples = match loaded.decoder.next_packet() {
            Ok(Some(samples)) => samples,
            Ok(None) => {
                self.reached_end();
                return;
            }
            Err(e) => {
                // Like librespot: an undecodable song ends here.
                log::error!("YouTube player: {e:#}");
                self.reached_end();
                return;
            }
        };
        let volume = self.volume.attenuation_factor();
        if volume < 1.0 {
            for sample in &mut samples {
                *sample *= volume;
            }
        }
        let rate = loaded.decoder.rate();
        loaded.frames += (samples.len() / 2) as u64;
        if !loaded.hinted && loaded.frames * 1000 >= PRELOAD_AFTER.as_millis() as u64 * u64::from(rate.max(1)) {
            loaded.hinted = true;
            let hint =
                PlayerEvent::TimeToPreloadNextTrack { play_request_id: loaded.request, track_id: loaded.uri.clone() };
            self.send(hint);
        }
        if let Err(e) = self.sink.write(samples, rate) {
            // The output has stored the reason for the controller; pausing reports it.
            log::warn!("YouTube player: {e}");
            self.pause();
        }
    }

    /// The song has been decoded to its end; the last of it still plays out, and the next one
    /// follows on without a gap.
    fn reached_end(&mut self) {
        let State::Loaded(loaded) = &mut self.state else { return };
        loaded.ended = true;
        let event = PlayerEvent::EndOfTrack { play_request_id: loaded.request, track_id: loaded.uri.clone() };
        self.send(event);
    }

    fn ensure_sink_running(&mut self) {
        if !self.sink_running {
            if let Err(e) = self.sink.start() {
                log::warn!("YouTube player: couldn't start the output: {e}");
            }
            self.sink_running = true;
        }
    }

    fn ensure_sink_stopped(&mut self) {
        if self.sink_running {
            if let Err(e) = self.sink.stop() {
                log::warn!("YouTube player: couldn't stop the output: {e}");
            }
            self.sink_running = false;
        }
    }
}

/// Stand-ins for YouTube, for tests here and in the engine.
#[cfg(test)]
pub(super) mod fakes {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::model::ArtistRef;

    pub(in crate::engine) fn track(uri: &str, duration_ms: u32) -> TrackInfo {
        TrackInfo {
            uri: uri.to_owned(),
            name: "Song".into(),
            artists: vec![ArtistRef { name: "Artist".into(), uri: String::new() }],
            album: "Album".into(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    /// A song of steady sound (0.8).
    pub(in crate::engine) struct FakeDecoder {
        rate: u32,
        duration_ms: u32,
        frame: u64,
    }

    impl FakeDecoder {
        pub(in crate::engine) fn new(rate: u32, duration_ms: u32) -> Self {
            Self { rate, duration_ms, frame: 0 }
        }

        fn total_frames(&self) -> u64 {
            u64::from(self.duration_ms) * u64::from(self.rate) / 1000
        }
    }

    impl PcmDecoder for FakeDecoder {
        fn rate(&self) -> u32 {
            self.rate
        }

        fn position_ms(&self) -> u32 {
            (self.frame * 1000 / u64::from(self.rate)) as u32
        }

        fn seek(&mut self, position_ms: u32) -> Result<u32> {
            let position_ms = position_ms.min(self.duration_ms);
            self.frame = u64::from(position_ms) * u64::from(self.rate) / 1000;
            Ok(position_ms)
        }

        fn next_packet(&mut self) -> Result<Option<Vec<f64>>> {
            let left = self.total_frames().saturating_sub(self.frame);
            if left == 0 {
                return Ok(None);
            }
            let frames = left.min(1024);
            self.frame += frames;
            Ok(Some(vec![0.8; frames as usize * 2]))
        }
    }

    /// How a fake song answers: after `delay`, a song of (rate, length in ms), or a problem.
    #[derive(Clone)]
    struct Song {
        delay: Duration,
        result: Result<(u32, u32), String>,
    }

    /// Answers with fake songs, and counts what it was asked and what was cancelled.
    #[derive(Default)]
    pub(in crate::engine) struct FakeResolver {
        songs: Mutex<HashMap<String, Song>>,
        calls: Mutex<Vec<String>>,
        cancelled: Arc<AtomicUsize>,
    }

    impl FakeResolver {
        pub(in crate::engine) fn song(&self, uri: &str, delay_ms: u64, result: Result<(u32, u32), &str>) {
            let song = Song { delay: Duration::from_millis(delay_ms), result: result.map_err(str::to_owned) };
            lock(&self.songs).insert(uri.to_owned(), song);
        }

        pub(in crate::engine) fn calls(&self, uri: &str) -> usize {
            lock(&self.calls).iter().filter(|called| *called == uri).count()
        }

        pub(in crate::engine) fn cancelled(&self) -> usize {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    /// Counts a fetch that was dropped before it finished.
    struct Unfinished(Option<Arc<AtomicUsize>>);

    impl Unfinished {
        fn finish(&mut self) {
            self.0 = None;
        }
    }

    impl Drop for Unfinished {
        fn drop(&mut self) {
            if let Some(cancelled) = self.0.take() {
                cancelled.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl PcmResolver for FakeResolver {
        fn open(&self, track: TrackInfo) -> BoxFuture<'static, Result<Box<dyn PcmDecoder>, String>> {
            lock(&self.calls).push(track.uri.clone());
            let song = lock(&self.songs).get(&track.uri).cloned();
            let mut unfinished = Unfinished(Some(self.cancelled.clone()));
            Box::pin(async move {
                let Some(song) = song else {
                    unfinished.finish();
                    return Err(NOT_FOUND.to_owned());
                };
                tokio::time::sleep(song.delay).await;
                unfinished.finish();
                let (rate, duration_ms) = song.result?;
                Ok(Box::new(FakeDecoder::new(rate, duration_ms)) as Box<dyn PcmDecoder>)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use librespot_playback::audio_backend::SinkError;

    use super::fakes::{FakeResolver, track};
    use super::*;

    const A: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const B: &str = "spotify:track:5sWHDYs0csV6RS48xBl0tH";
    const C: &str = "spotify:track:6rqhFgbbKwnb9MLmUQDhG6";
    const D: &str = "spotify:track:0eGsygTp906u18L0Oimnem";
    const LONG: u32 = 600_000;

    fn id(uri: &str) -> SpotifyUri {
        SpotifyUri::from_uri(uri).unwrap()
    }

    #[derive(Default)]
    struct SinkLog {
        starts: u32,
        stops: u32,
        packets: usize,
        rates: Vec<u32>,
        first_sample: Option<f64>,
        /// The packet with this number (counting from 0) fails to play.
        fail_at: Option<usize>,
    }

    /// Records what reaches the output; each packet takes `pace` to "play".
    struct FakeSink {
        log: Arc<Mutex<SinkLog>>,
        pace: Duration,
        broken: Arc<AtomicBool>,
    }

    impl PcmSink for FakeSink {
        fn start(&mut self) -> SinkResult<()> {
            lock(&self.log).starts += 1;
            Ok(())
        }

        fn stop(&mut self) -> SinkResult<()> {
            lock(&self.log).stops += 1;
            Ok(())
        }

        fn write(&mut self, samples: Vec<f64>, rate: u32) -> SinkResult<()> {
            if self.broken.load(Ordering::SeqCst) {
                return Err(SinkError::OnWrite("unplugged".into()));
            }
            std::thread::sleep(self.pace);
            let mut log = lock(&self.log);
            if log.fail_at == Some(log.packets) {
                log.fail_at = None;
                return Err(SinkError::OnWrite("unplugged".into()));
            }
            log.packets += 1;
            if log.rates.last() != Some(&rate) {
                log.rates.push(rate);
            }
            log.first_sample.get_or_insert(samples[0]);
            Ok(())
        }
    }

    struct FixedVolume(f64);

    impl VolumeGetter for FixedVolume {
        fn attenuation_factor(&self) -> f64 {
            self.0
        }
    }

    struct Rig {
        player: PcmPlayer,
        events: PlayerEventChannel,
        resolver: Arc<FakeResolver>,
        env: Arc<PcmEnv>,
        sink: Arc<Mutex<SinkLog>>,
        broken: Arc<AtomicBool>,
    }

    fn rig_with(volume: f64, pace: Duration) -> Rig {
        let resolver = Arc::new(FakeResolver::default());
        let env = Arc::new(PcmEnv {
            resolver: resolver.clone(),
            hints: Hints::default(),
            problem: ProblemSlot::default(),
            runtime: Handle::current(),
        });
        let sink = Arc::new(Mutex::new(SinkLog::default()));
        let broken = Arc::new(AtomicBool::new(false));
        let fake = FakeSink { log: sink.clone(), pace, broken: broken.clone() };
        let (player, events) =
            PcmPlayer::new(env.clone(), Box::new(FixedVolume(volume)), move || Box::new(fake) as Box<dyn PcmSink>);
        Rig { player, events, resolver, env, sink, broken }
    }

    /// Each packet (about 23 ms of audio) takes 1 ms.
    fn rig() -> Rig {
        rig_with(1.0, Duration::from_millis(1))
    }

    fn name(uri: &SpotifyUri) -> &'static str {
        [A, B, C, D].into_iter().zip(["A", "B", "C", "D"]).find(|(known, _)| id(known) == *uri).map_or("?", |(_, n)| n)
    }

    /// An event in short: "playing 1 A" is `Playing` of request 1 for song A.
    fn describe(event: &PlayerEvent) -> String {
        let (what, request, uri) = match event {
            PlayerEvent::PlayRequestIdChanged { play_request_id } => return format!("id {play_request_id}"),
            PlayerEvent::Loading { play_request_id, track_id, .. } => ("loading", play_request_id, track_id),
            PlayerEvent::Playing { play_request_id, track_id, .. } => ("playing", play_request_id, track_id),
            PlayerEvent::Paused { play_request_id, track_id, .. } => ("paused", play_request_id, track_id),
            PlayerEvent::Seeked { play_request_id, track_id, .. } => ("seeked", play_request_id, track_id),
            PlayerEvent::TimeToPreloadNextTrack { play_request_id, track_id } => ("hint", play_request_id, track_id),
            PlayerEvent::EndOfTrack { play_request_id, track_id } => ("end", play_request_id, track_id),
            PlayerEvent::Unavailable { play_request_id, track_id } => ("unavailable", play_request_id, track_id),
            PlayerEvent::Stopped { play_request_id, track_id } => ("stopped", play_request_id, track_id),
            other => return format!("{other:?}"),
        };
        format!("{what} {request} {}", name(uri))
    }

    fn position(event: &PlayerEvent) -> u32 {
        match event {
            PlayerEvent::Playing { position_ms, .. }
            | PlayerEvent::Paused { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. } => *position_ms,
            other => panic!("no position in {other:?}"),
        }
    }

    impl Rig {
        /// A song the fake YouTube has (with its details given to the player).
        fn song(&self, uri: &str, delay_ms: u64, duration_ms: u32) {
            self.resolver.song(uri, delay_ms, Ok((44_100, duration_ms)));
            self.env.hints.remember(&track(uri, duration_ms));
        }

        async fn next_any(&mut self) -> PlayerEvent {
            match tokio::time::timeout(Duration::from_secs(5), self.events.recv()).await {
                Ok(Some(event)) => event,
                Ok(None) => panic!("the player stopped"),
                Err(_) => panic!("no event within 5 s"),
            }
        }

        /// The next event apart from preload hints (which come at a time that depends on speed).
        async fn next(&mut self) -> PlayerEvent {
            loop {
                let event = self.next_any().await;
                if !matches!(event, PlayerEvent::TimeToPreloadNextTrack { .. }) {
                    return event;
                }
            }
        }

        async fn expect(&mut self, expected: &[&str]) -> Vec<PlayerEvent> {
            let mut events = Vec::new();
            for want in expected {
                let event = self.next().await;
                assert_eq!(describe(&event), *want, "{event:?}");
                events.push(event);
            }
            events
        }

        /// Nothing (but maybe a preload hint) is reported for `ms`.
        async fn quiet(&mut self, ms: u64) {
            let until = tokio::time::Instant::now() + Duration::from_millis(ms);
            while let Ok(event) = tokio::time::timeout_at(until, self.events.recv()).await {
                let event = event.expect("the player stopped");
                assert!(matches!(event, PlayerEvent::TimeToPreloadNextTrack { .. }), "unexpected {event:?}");
            }
        }

        fn problem(&self) -> Option<String> {
            lock(&self.env.problem).take()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_and_play_report_like_librespot() {
        let mut rig = rig_with(1.0, Duration::ZERO);
        rig.song(A, 20, 5_000);
        rig.player.load(id(A), true, 0);
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(describe(&rig.next_any().await));
        }
        // The next song is asked for after 3 s of this one; the end is reported once.
        assert_eq!(seen, ["id 1", "loading 1 A", "playing 1 A", "hint 1 A", "end 1 A"]);
        rig.quiet(300).await;
        let sink = lock(&rig.sink);
        assert_eq!((sink.starts, sink.stops), (1, 0), "the output keeps running for the next song");
        assert_eq!(sink.rates, [44_100]);
        let frames: usize = 5 * 44_100;
        assert_eq!(sink.packets, frames.div_ceil(1024), "the whole song was written");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_resume_and_stop() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        rig.player.pause();
        let paused = rig.expect(&["paused 1 A"]).await;
        let at = position(&paused[0]);
        assert!(at > 0, "paused at {at} ms");
        let written = lock(&rig.sink).packets;
        assert_eq!(lock(&rig.sink).stops, 1);
        rig.quiet(100).await;
        assert_eq!(lock(&rig.sink).packets, written, "nothing is written while paused");
        rig.player.play();
        let playing = rig.expect(&["playing 1 A"]).await;
        assert_eq!(position(&playing[0]), at, "resumes where it paused");
        // A second pause while paused reports nothing.
        rig.player.pause();
        rig.player.pause();
        rig.expect(&["paused 1 A"]).await;
        rig.quiet(100).await;
        rig.player.stop();
        rig.expect(&["stopped 1 A"]).await;
        let sink = lock(&rig.sink);
        assert_eq!((sink.starts, sink.stops), (2, 2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn seeks_report_where_they_landed() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        rig.player.seek(100_000);
        let seeked = rig.expect(&["seeked 1 A"]).await;
        assert_eq!(position(&seeked[0]), 100_000);
        rig.player.pause();
        let paused = rig.expect(&["paused 1 A"]).await;
        assert!(position(&paused[0]) >= 100_000, "{paused:?}");
        rig.player.seek(30_000);
        let seeked = rig.expect(&["seeked 1 A"]).await;
        assert_eq!(position(&seeked[0]), 30_000, "while paused too");
        // Past the end: kept within the song, which then ends.
        rig.player.play();
        rig.expect(&["playing 1 A"]).await;
        rig.player.seek(u32::MAX);
        let seeked = rig.expect(&["seeked 1 A", "end 1 A"]).await;
        assert_eq!(position(&seeked[0]), LONG);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_seek_while_loading_starts_there() {
        let mut rig = rig();
        rig.song(A, 200, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A"]).await;
        rig.player.seek(42_000);
        let playing = rig.expect(&["playing 1 A"]).await;
        assert_eq!(position(&playing[0]), 42_000);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_preloaded_song_starts_without_loading() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.song(B, 0, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        rig.player.preload(id(B));
        rig.player.preload(id(B));
        tokio::time::sleep(Duration::from_millis(200)).await;
        rig.player.load(id(B), true, 0);
        rig.expect(&["id 2", "playing 2 B"]).await;
        assert_eq!(rig.resolver.calls(B), 1, "fetched once");
        assert_eq!(lock(&rig.sink).stops, 0, "no gap between the two");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_load_during_its_preload_waits_for_it() {
        let mut rig = rig();
        rig.song(B, 300, LONG);
        rig.player.preload(id(B));
        rig.player.load(id(B), true, 0);
        rig.expect(&["id 1", "loading 1 B", "playing 1 B"]).await;
        assert_eq!(rig.resolver.calls(B), 1);
        assert_eq!(rig.resolver.cancelled(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_newer_load_cancels_the_older_fetch() {
        let mut rig = rig();
        rig.song(A, 2_000, LONG);
        rig.song(B, 50, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A"]).await;
        rig.player.load(id(B), true, 0);
        rig.expect(&["id 2", "loading 2 B", "playing 2 B"]).await;
        assert_eq!(rig.resolver.cancelled(), 1, "A's fetch was stopped");
        rig.quiet(300).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_video_picked_while_loading_is_fetched_instead() {
        let mut rig = rig();
        rig.song(A, 2_000, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A"]).await;
        rig.song(A, 50, LONG);
        rig.player.forget(id(A));
        rig.expect(&["playing 1 A"]).await;
        assert_eq!(rig.resolver.calls(A), 2, "fetched anew");
        assert_eq!(rig.resolver.cancelled(), 1, "the old fetch was stopped");
        // And once loaded, a reload after another pick fetches again too.
        rig.player.forget(id(A));
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 2", "loading 2 A", "playing 2 A"]).await;
        assert_eq!(rig.resolver.calls(A), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reloading_the_same_song_keeps_its_audio() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        rig.player.load(id(A), true, 50_000);
        let playing = rig.expect(&["id 2", "playing 2 A"]).await;
        assert_eq!(position(&playing[1]), 50_000);
        assert_eq!(rig.resolver.calls(A), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_song_without_details_or_match_is_unavailable_with_a_reason() {
        let mut rig = rig();
        rig.player.load(id(C), true, 0);
        rig.expect(&["id 1", "loading 1 C", "unavailable 1 C"]).await;
        assert_eq!(rig.problem().as_deref(), Some(NO_DETAILS));
        assert_eq!(rig.resolver.calls(C), 0);
        rig.env.hints.remember(&track(D, 200_000));
        rig.resolver.song(D, 0, Err(NOT_FOUND));
        rig.player.load(id(D), true, 0);
        rig.expect(&["id 2", "loading 2 D", "unavailable 2 D"]).await;
        assert_eq!(rig.problem().as_deref(), Some(NOT_FOUND));
        assert_eq!(rig.problem(), None, "taking it clears it");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_preload_reads_as_a_preload() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.env.hints.remember(&track(D, 200_000));
        rig.resolver.song(D, 0, Err(DOWNLOAD_FAILED));
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        rig.player.preload(id(D));
        // The current request with the other song: `Tracking` ignores it.
        rig.expect(&["unavailable 1 D"]).await;
        assert_eq!(rig.problem(), None, "only songs that should play leave a reason");
        // Preloading the current song does nothing.
        rig.player.preload(id(A));
        rig.quiet(100).await;
        assert_eq!(rig.resolver.calls(A), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_while_loading_cancels_the_fetch() {
        let mut rig = rig();
        rig.song(A, 2_000, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A"]).await;
        rig.player.stop();
        rig.expect(&["stopped 1 A"]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(rig.resolver.cancelled(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_load_starts_paused() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.player.load(id(A), false, 7_000);
        let paused = rig.expect(&["id 1", "loading 1 A", "paused 1 A"]).await;
        assert_eq!(position(&paused[2]), 7_000);
        rig.quiet(100).await;
        assert_eq!(lock(&rig.sink).packets, 0);
        rig.player.play();
        rig.expect(&["playing 1 A"]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn volume_and_rate_reach_the_output() {
        let mut rig = rig_with(0.5, Duration::from_millis(1));
        rig.resolver.song(A, 0, Ok((48_000, LONG)));
        rig.env.hints.remember(&track(A, LONG));
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let sink = lock(&rig.sink);
        assert_eq!(sink.rates, [48_000]);
        assert_eq!(sink.first_sample, Some(0.4), "the fake song's 0.8 at half volume");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_output_failure_pauses() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        rig.broken.store(true, Ordering::SeqCst);
        rig.expect(&["paused 1 A"]).await;
        assert_eq!(lock(&rig.sink).stops, 1);
        rig.broken.store(false, Ordering::SeqCst);
        rig.player.play();
        rig.expect(&["playing 1 A"]).await;
        assert_eq!(lock(&rig.sink).starts, 2, "pressing play opens the output again");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_output_failure_at_the_preload_moment_still_asks_for_the_next_song() {
        let mut rig = rig_with(1.0, Duration::ZERO);
        rig.song(A, 0, LONG);
        // The packet that takes the song past 3 s fails to play.
        lock(&rig.sink).fail_at = Some(3 * 44_100 / 1024);
        rig.player.load(id(A), true, 0);
        let mut seen = Vec::new();
        while seen.last().is_none_or(|last| last != "paused 1 A") {
            seen.push(describe(&rig.next_any().await));
        }
        assert_eq!(seen, ["id 1", "loading 1 A", "playing 1 A", "hint 1 A", "paused 1 A"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_player_closes_its_events() {
        let mut rig = rig();
        rig.song(A, 0, LONG);
        rig.player.load(id(A), true, 0);
        rig.expect(&["id 1", "loading 1 A", "playing 1 A"]).await;
        let Rig { player, mut events, sink, .. } = rig;
        drop(player);
        while let Some(event) = events.recv().await {
            assert!(matches!(event, PlayerEvent::TimeToPreloadNextTrack { .. }), "{event:?}");
        }
        assert_eq!(lock(&sink).stops, 1, "the output was closed");
    }

    /// Opens the real sample file (YouTube format 140) whatever the song.
    struct SampleFile;

    fn sample_path() -> std::path::PathBuf {
        crate::config::test_root().join("test.m4a")
    }

    impl PcmResolver for SampleFile {
        fn open(&self, track: TrackInfo) -> BoxFuture<'static, Result<Box<dyn PcmDecoder>, String>> {
            Box::pin(async move {
                let bytes = std::fs::read(sample_path()).map_err(|e| e.to_string())?;
                let decoder = M4aDecoder::open(bytes, track.duration_ms).map_err(|e| e.to_string())?;
                Ok(Box::new(decoder) as Box<dyn PcmDecoder>)
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sample_file_plays_to_its_end() {
        if !sample_path().exists() {
            eprintln!("{} is missing; skipped", sample_path().display());
            return;
        }
        let env = Arc::new(PcmEnv {
            resolver: Arc::new(SampleFile),
            hints: Hints::default(),
            problem: ProblemSlot::default(),
            runtime: Handle::current(),
        });
        env.hints.remember(&track(A, 217_000));
        let log = Arc::new(Mutex::new(SinkLog::default()));
        let fake = FakeSink { log: log.clone(), pace: Duration::ZERO, broken: Arc::new(AtomicBool::new(false)) };
        let (player, mut events) =
            PcmPlayer::new(env, Box::new(FixedVolume(1.0)), move || Box::new(fake) as Box<dyn PcmSink>);
        player.load(id(A), true, 200_000);
        let mut seen = Vec::new();
        while seen.len() < 5 {
            let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await.unwrap().unwrap();
            if let PlayerEvent::Playing { position_ms, .. } = &event {
                assert!(position_ms.abs_diff(200_000) <= 50, "starts at {position_ms} ms");
            }
            seen.push(describe(&event));
        }
        assert_eq!(seen, ["id 1", "loading 1 A", "playing 1 A", "hint 1 A", "end 1 A"]);
        let log = lock(&log);
        assert_eq!(log.rates, [44_100]);
        // About the last 17 s of the song, in packets of 1024 frames.
        let seconds = log.packets as f64 * 1024.0 / 44_100.0;
        assert!((15.0..19.0).contains(&seconds), "{seconds} s written");
    }

    #[test]
    fn hints_keep_the_most_recent_songs() {
        let hints = Hints::default();
        hints.remember(&track(A, 1_000));
        hints.remember(&track("not a uri", 1_000));
        assert_eq!(hints.get(&id(A)).map(|t| t.duration_ms), Some(1_000));
        hints.remember(&track(A, 2_000));
        assert_eq!(hints.get(&id(A)).map(|t| t.duration_ms), Some(2_000), "the newest details win");
        assert_eq!(lock(&hints.0).len(), 1);
        for n in 0..MAX_HINTS {
            hints.remember(&track(&format!("spotify:track:{n:0>22}"), 1));
        }
        assert!(hints.get(&id(A)).is_none(), "the oldest are forgotten");
        assert_eq!(lock(&hints.0).len(), MAX_HINTS);
    }
}
