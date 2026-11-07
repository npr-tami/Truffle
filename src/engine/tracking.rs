//! Turns one librespot player's raw event stream into [`EngineEvent`]s for the track the
//! controller asked for last (see [`Tracking`]).

use std::time::Instant;

use librespot_core::SpotifyUri;
use librespot_playback::player::PlayerEvent;

use super::EngineEvent;

/// The track the controller asked for most recently.
#[derive(Debug, Clone)]
struct CurrentLoad {
    /// Exactly as the controller passed it, so it can compare with its own queue.
    uri: String,
    id: SpotifyUri,
}

/// What the controller last heard about a request: whether it was playing, and where.
#[derive(Debug, Clone, Copy)]
struct Heard {
    playing: bool,
    position_ms: u32,
    at: Instant,
}

impl Heard {
    fn position_now(&self) -> u32 {
        if !self.playing {
            return self.position_ms;
        }
        let elapsed = u32::try_from(self.at.elapsed().as_millis()).unwrap_or(u32::MAX);
        self.position_ms.saturating_add(elapsed)
    }
}

/// A request the player acknowledged, with its de-duplication flags.
#[derive(Debug, Clone, Copy)]
struct Request {
    id: u64,
    /// `EndOfTrack`/`Unavailable` already reported.
    finished: bool,
    /// `PreloadNext` already reported.
    preload_hinted: bool,
    /// The player decoded the track to its end (`EndOfTrack`).
    reached_end: bool,
    heard: Option<Heard>,
}

impl Request {
    fn new(id: u64) -> Self {
        Self { id, finished: false, preload_hinted: false, reached_end: false, heard: None }
    }
}

/// Bookkeeping that turns librespot's raw event stream into [`EngineEvent`]s.
///
/// Every `Player::load` makes librespot emit `PlayRequestIdChanged` with a fresh id before
/// anything else for that load. Events carrying an older id belong to a track the controller has
/// already moved away from and are dropped, as is everything that arrives between a `load` call
/// and its `PlayRequestIdChanged`.
///
/// One exception: librespot announces the new id *before* it tries the load, and can still reject
/// the load afterwards (it only logs the error). That happens when it reuses the decoder of the
/// track that is already loaded and the seek to the requested position fails (for example while
/// the network is flaky). It then carries on with the previous request. A load that went through
/// never emits the previous id again, so an event with that id after the acknowledgement means
/// exactly this: the previous request is followed again (same track), or the requested track is
/// reported unavailable (the player kept another track).
#[derive(Debug, Default)]
pub(super) struct Tracking {
    current: Option<CurrentLoad>,
    /// `load` calls whose `PlayRequestIdChanged` has not arrived yet.
    pending_loads: u32,
    /// The latest request the player has acknowledged.
    request: Option<Request>,
    /// The request acknowledged before `request` (see the type docs).
    previous: Option<Request>,
}

impl Tracking {
    pub(super) fn begin_load(&mut self, uri: String, id: SpotifyUri) {
        self.current = Some(CurrentLoad { uri, id });
        self.pending_loads = self.pending_loads.saturating_add(1);
    }

    /// Forget the current track (after `stop`): everything still in flight for it is stale.
    pub(super) fn clear(&mut self) {
        self.current = None;
        self.previous = None;
    }

    /// The player decoded its latest acknowledged track to the end: the last of it is still
    /// queued in the output (about half a second), playing out.
    pub(super) fn reached_end(&self) -> bool {
        self.request.is_some_and(|r| r.reached_end)
    }

    /// Maps one player event to the engine events it means (usually none or one).
    pub(super) fn on_event(&mut self, event: PlayerEvent) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        self.map_event(event, &mut out);
        out
    }

    fn map_event(&mut self, event: PlayerEvent, out: &mut Vec<EngineEvent>) {
        let request_id = match &event {
            PlayerEvent::PlayRequestIdChanged { play_request_id } => {
                // A seek during loading restarts the load with the *same* id; only a new id
                // acknowledges one of our `load` calls.
                if self.request.map(|r| r.id) != Some(*play_request_id) {
                    self.previous = self.request.take();
                    self.request = Some(Request::new(*play_request_id));
                    self.pending_loads = self.pending_loads.saturating_sub(1);
                }
                return;
            }
            PlayerEvent::Preloading { track_id } => {
                log::debug!("preloaded {track_id}");
                return;
            }
            PlayerEvent::PositionChanged { play_request_id, .. } => *play_request_id,
            other => match other.get_play_request_id() {
                Some(id) => id,
                None => return,
            },
        };

        if self.pending_loads > 0 {
            return;
        }
        let Some(current) = self.current.clone() else { return };
        if self.request.map(|r| r.id) != Some(request_id) && !self.follow_previous(&event, request_id, &current, out) {
            return;
        }
        // An event of the acknowledged request shows its load went through.
        self.previous = None;
        let Some(request) = self.request.as_mut() else { return };
        let uri = current.uri;
        let now = Instant::now();

        match event {
            PlayerEvent::Loading { .. } => {
                request.heard = None;
                out.push(EngineEvent::Loading { uri });
            }
            PlayerEvent::Playing { position_ms, .. } => {
                request.heard = Some(Heard { playing: true, position_ms, at: now });
                out.push(EngineEvent::Playing { uri, position_ms });
            }
            PlayerEvent::Paused { position_ms, .. } => {
                request.heard = Some(Heard { playing: false, position_ms, at: now });
                out.push(EngineEvent::Paused { uri, position_ms });
            }
            PlayerEvent::Seeked { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. } => {
                if let Some(heard) = request.heard.as_mut() {
                    heard.position_ms = position_ms;
                    heard.at = now;
                }
                out.push(EngineEvent::Position { uri, position_ms });
            }
            PlayerEvent::TimeToPreloadNextTrack { .. } => {
                if !request.preload_hinted && !request.finished {
                    request.preload_hinted = true;
                    out.push(EngineEvent::PreloadNext { uri });
                }
            }
            PlayerEvent::EndOfTrack { .. } => {
                // librespot can repeat EndOfTrack for one load (decoder errors keep the loop going).
                request.heard = None;
                request.reached_end = true;
                if !request.finished {
                    request.finished = true;
                    out.push(EngineEvent::EndOfTrack { uri });
                }
            }
            PlayerEvent::Unavailable { track_id, .. } => {
                // A failed *preload* is reported with the current request id but the preloaded
                // track's id. The current track is fine; the controller will find out about the
                // other one when it loads it.
                if track_id != current.id {
                    log::info!("could not preload {track_id}; it will be retried when it is played");
                } else if !request.finished {
                    request.finished = true;
                    out.push(EngineEvent::Unavailable { uri });
                }
            }
            // `Engine::stop` reports `Stopped` itself; anything else carries no state we need.
            _ => {}
        }
    }

    /// Handles an event of the request acknowledged before the current one (see the type docs).
    /// Returns true when the event is to be handled as the current request's.
    fn follow_previous(
        &mut self,
        event: &PlayerEvent,
        request_id: u64,
        current: &CurrentLoad,
        out: &mut Vec<EngineEvent>,
    ) -> bool {
        let Some(previous) = self.previous.filter(|p| p.id == request_id) else { return false };
        // Only events of a loaded track prove the player is still on it (an `Unavailable` with this
        // id can be a failed preload, a `Stopped` only says it is gone).
        let Some(track_id) = playing_track_id(event) else { return false };
        self.previous = None;
        if *track_id != current.id {
            log::warn!("the player did not switch to {}; reporting it as unavailable", current.uri);
            if let Some(request) = self.request.as_mut()
                && !request.finished
            {
                request.finished = true;
                out.push(EngineEvent::Unavailable { uri: current.uri.clone() });
            }
            return false;
        }
        log::warn!("the player rejected the last load of {} and kept the earlier one; following it", current.uri);
        // The controller still waits for the rejected load: first tell it what the player is doing.
        let carries_state = matches!(event, PlayerEvent::Playing { .. } | PlayerEvent::Paused { .. });
        if !carries_state && let Some(heard) = previous.heard {
            let (uri, position_ms) = (current.uri.clone(), heard.position_now());
            out.push(if heard.playing {
                EngineEvent::Playing { uri, position_ms }
            } else {
                EngineEvent::Paused { uri, position_ms }
            });
        }
        self.request = Some(previous);
        true
    }
}

/// The track of an event that only a loaded (playing or paused) track produces.
fn playing_track_id(event: &PlayerEvent) -> Option<&SpotifyUri> {
    match event {
        PlayerEvent::Playing { track_id, .. }
        | PlayerEvent::Paused { track_id, .. }
        | PlayerEvent::Seeked { track_id, .. }
        | PlayerEvent::PositionCorrection { track_id, .. }
        | PlayerEvent::PositionChanged { track_id, .. }
        | PlayerEvent::TimeToPreloadNextTrack { track_id, .. }
        | PlayerEvent::EndOfTrack { track_id, .. } => Some(track_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK_A: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
    const TRACK_B: &str = "spotify:track:5sWHDYs0csV6RS48xBl0tH";

    fn id(uri: &str) -> SpotifyUri {
        SpotifyUri::from_uri(uri).unwrap()
    }

    impl Tracking {
        /// `on_event` for events that map to at most one engine event.
        fn map(&mut self, event: PlayerEvent) -> Option<EngineEvent> {
            let mut out = self.on_event(event);
            assert!(out.len() <= 1, "expected at most one event, got {out:?}");
            out.pop()
        }
    }

    fn loaded(uri: &str, request_id: u64) -> Tracking {
        let mut t = Tracking::default();
        t.begin_load(uri.to_owned(), id(uri));
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: request_id }), None);
        t
    }

    #[test]
    fn maps_basic_events() {
        let mut t = loaded(TRACK_A, 1);
        assert_eq!(
            t.map(PlayerEvent::Loading { play_request_id: 1, track_id: id(TRACK_A), position_ms: 0 }),
            Some(EngineEvent::Loading { uri: TRACK_A.into() })
        );
        assert_eq!(
            t.map(PlayerEvent::Playing { play_request_id: 1, track_id: id(TRACK_A), position_ms: 5 }),
            Some(EngineEvent::Playing { uri: TRACK_A.into(), position_ms: 5 })
        );
        assert_eq!(
            t.map(PlayerEvent::Seeked { play_request_id: 1, track_id: id(TRACK_A), position_ms: 9000 }),
            Some(EngineEvent::Position { uri: TRACK_A.into(), position_ms: 9000 })
        );
        assert_eq!(
            t.map(PlayerEvent::PositionCorrection {
                play_request_id: 1,
                track_id: id(TRACK_A),
                position_ms: 9500
            }),
            Some(EngineEvent::Position { uri: TRACK_A.into(), position_ms: 9500 })
        );
        assert_eq!(
            t.map(PlayerEvent::Paused { play_request_id: 1, track_id: id(TRACK_A), position_ms: 9600 }),
            Some(EngineEvent::Paused { uri: TRACK_A.into(), position_ms: 9600 })
        );
    }

    #[test]
    fn end_of_track_and_preload_hint_are_reported_once() {
        let mut t = loaded(TRACK_A, 1);
        let hint = || PlayerEvent::TimeToPreloadNextTrack { play_request_id: 1, track_id: id(TRACK_A) };
        assert_eq!(t.map(hint()), Some(EngineEvent::PreloadNext { uri: TRACK_A.into() }));
        assert_eq!(t.map(hint()), None);

        let end = || PlayerEvent::EndOfTrack { play_request_id: 1, track_id: id(TRACK_A) };
        assert!(!t.reached_end());
        assert_eq!(t.map(end()), Some(EngineEvent::EndOfTrack { uri: TRACK_A.into() }));
        assert!(t.reached_end(), "the end of the track is playing out");
        assert_eq!(t.map(end()), None);
        assert_eq!(
            t.map(PlayerEvent::Unavailable { play_request_id: 1, track_id: id(TRACK_A) }),
            None,
            "already finished"
        );
    }

    #[test]
    fn failed_preload_does_not_skip_the_current_track() {
        let mut t = loaded(TRACK_A, 1);
        assert_eq!(
            t.map(PlayerEvent::Unavailable { play_request_id: 1, track_id: id(TRACK_B) }),
            None
        );
        assert_eq!(
            t.map(PlayerEvent::Unavailable { play_request_id: 1, track_id: id(TRACK_A) }),
            Some(EngineEvent::Unavailable { uri: TRACK_A.into() })
        );
    }

    #[test]
    fn events_for_a_replaced_track_are_ignored() {
        let mut t = loaded(TRACK_A, 1);
        // The controller loads B; until the player acknowledges it, A's events are stale.
        t.begin_load(TRACK_B.to_owned(), id(TRACK_B));
        assert_eq!(t.map(PlayerEvent::EndOfTrack { play_request_id: 1, track_id: id(TRACK_A) }), None);
        assert_eq!(t.map(PlayerEvent::Playing { play_request_id: 1, track_id: id(TRACK_A), position_ms: 0 }), None);
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: 2 }), None);
        assert_eq!(
            t.map(PlayerEvent::Loading { play_request_id: 2, track_id: id(TRACK_B), position_ms: 0 }),
            Some(EngineEvent::Loading { uri: TRACK_B.into() })
        );
        assert_eq!(
            t.map(PlayerEvent::Playing { play_request_id: 2, track_id: id(TRACK_B), position_ms: 0 }),
            Some(EngineEvent::Playing { uri: TRACK_B.into(), position_ms: 0 })
        );
        // An id older than the previous request stays stale.
        assert_eq!(t.map(PlayerEvent::EndOfTrack { play_request_id: 1, track_id: id(TRACK_A) }), None);
    }

    #[test]
    fn a_rejected_reload_of_the_same_track_keeps_reporting_it() {
        // Playing A on request 1; the controller reloads A at another position (after a reconnect).
        let mut t = loaded(TRACK_A, 1);
        let playing = t.map(PlayerEvent::Playing { play_request_id: 1, track_id: id(TRACK_A), position_ms: 60_000 });
        assert!(playing.is_some());
        t.begin_load(TRACK_A.to_owned(), id(TRACK_A));
        // librespot announces request 2, then its seek fails and it keeps playing request 1.
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: 2 }), None);
        let hint = t.on_event(PlayerEvent::TimeToPreloadNextTrack { play_request_id: 1, track_id: id(TRACK_A) });
        // First where the player really is (still playing, a little past 60 s), then the event.
        assert_eq!(hint.len(), 2, "{hint:?}");
        assert!(
            matches!(&hint[0], EngineEvent::Playing { uri, position_ms } if uri == TRACK_A && *position_ms >= 60_000),
            "{hint:?}"
        );
        assert_eq!(hint[1], EngineEvent::PreloadNext { uri: TRACK_A.into() });
        // The end of the track is reported once, so the queue moves on.
        let end = || PlayerEvent::EndOfTrack { play_request_id: 1, track_id: id(TRACK_A) };
        assert_eq!(t.on_event(end()), vec![EngineEvent::EndOfTrack { uri: TRACK_A.into() }]);
        assert!(t.on_event(end()).is_empty());
    }

    #[test]
    fn a_rejected_reload_while_paused_reports_the_pause() {
        let mut t = loaded(TRACK_A, 1);
        assert!(t.map(PlayerEvent::Paused { play_request_id: 1, track_id: id(TRACK_A), position_ms: 5_000 }).is_some());
        t.begin_load(TRACK_A.to_owned(), id(TRACK_A));
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: 2 }), None);
        // Pressing play again resumes request 1.
        assert_eq!(
            t.on_event(PlayerEvent::Playing { play_request_id: 1, track_id: id(TRACK_A), position_ms: 5_000 }),
            vec![EngineEvent::Playing { uri: TRACK_A.into(), position_ms: 5_000 }]
        );
        let seek = t.on_event(PlayerEvent::Seeked { play_request_id: 1, track_id: id(TRACK_A), position_ms: 9_000 });
        assert_eq!(seek, vec![EngineEvent::Position { uri: TRACK_A.into(), position_ms: 9_000 }]);
    }

    #[test]
    fn a_rejected_load_of_another_track_reports_that_track_unavailable() {
        let mut t = loaded(TRACK_A, 1);
        t.begin_load(TRACK_B.to_owned(), id(TRACK_B));
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: 2 }), None);
        let seek = || PlayerEvent::Seeked { play_request_id: 1, track_id: id(TRACK_A), position_ms: 1_000 };
        assert_eq!(t.on_event(seek()), vec![EngineEvent::Unavailable { uri: TRACK_B.into() }]);
        // Only once; A's further events stay stale.
        assert!(t.on_event(seek()).is_empty());
        assert!(t.on_event(PlayerEvent::EndOfTrack { play_request_id: 1, track_id: id(TRACK_A) }).is_empty());
    }

    #[test]
    fn a_failed_preload_with_the_previous_id_is_not_a_rejection() {
        let mut t = loaded(TRACK_A, 1);
        t.begin_load(TRACK_B.to_owned(), id(TRACK_B));
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: 2 }), None);
        assert_eq!(t.map(PlayerEvent::Unavailable { play_request_id: 1, track_id: id(TRACK_A) }), None);
        assert_eq!(t.map(PlayerEvent::Stopped { play_request_id: 1, track_id: id(TRACK_A) }), None);
        assert_eq!(
            t.map(PlayerEvent::Playing { play_request_id: 2, track_id: id(TRACK_B), position_ms: 0 }),
            Some(EngineEvent::Playing { uri: TRACK_B.into(), position_ms: 0 })
        );
    }

    #[test]
    fn reloading_the_same_track_rearms_end_of_track() {
        let mut t = loaded(TRACK_A, 1);
        let end = |rid| PlayerEvent::EndOfTrack { play_request_id: rid, track_id: id(TRACK_A) };
        assert!(t.map(end(1)).is_some());
        // Repeat-one: load the same URI again.
        t.begin_load(TRACK_A.to_owned(), id(TRACK_A));
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: 2 }), None);
        assert!(!t.reached_end(), "a new request starts from the top");
        assert_eq!(t.map(end(2)), Some(EngineEvent::EndOfTrack { uri: TRACK_A.into() }));
    }

    #[test]
    fn seek_while_loading_keeps_the_request() {
        let mut t = loaded(TRACK_A, 1);
        // librespot restarts the load with the same id and re-announces it.
        assert_eq!(t.map(PlayerEvent::PlayRequestIdChanged { play_request_id: 1 }), None);
        assert_eq!(
            t.map(PlayerEvent::Loading { play_request_id: 1, track_id: id(TRACK_A), position_ms: 30_000 }),
            Some(EngineEvent::Loading { uri: TRACK_A.into() })
        );
    }

    #[test]
    fn nothing_is_reported_after_stop() {
        let mut t = loaded(TRACK_A, 1);
        t.clear();
        assert_eq!(t.map(PlayerEvent::Stopped { play_request_id: 1, track_id: id(TRACK_A) }), None);
        assert_eq!(
            t.map(PlayerEvent::Playing { play_request_id: 1, track_id: id(TRACK_A), position_ms: 0 }),
            None
        );
    }
}
