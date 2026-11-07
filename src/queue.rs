//! Play order and shuffle engine. Pure logic, no I/O.
//!
//! Shuffle is a uniform Fisher-Yates permutation. Optional rules (see `ShuffleRules`) post-process
//! or weight it. Play history (counts, blocked tracks, per-source "already played this cycle")
//! is persisted by the controller via `PersistedState`.
//!
//! # True random shuffle
//!
//! Every queue owns a ChaCha12 [`StdRng`] seeded from the operating system's random source
//! ([`PlayQueue::new`]) or from a fixed seed ([`PlayQueue::new_seeded`], for tests). Orders are
//! produced with Fisher-Yates, and every random index is drawn with Lemire's multiply-and-reject
//! method, so there is no modulo bias: each permutation of the candidate tracks is exactly as likely
//! as any other. Rules only change that when they are switched on:
//!
//! * `skip_blocked`: blocked tracks are left out of new shuffled orders and are skipped by
//!   `advance`, `peek_next` and `back`, including tracks blocked after the order was built.
//!   A blocked track that the user explicitly picks (`start_index`, `jump_to_track`) still plays.
//! * `no_repeat_until_exhausted`: see "Cycles" below.
//! * `favour_rarely_played`: instead of Fisher-Yates, the tracks get a weighted random order
//!   (Efraimidis-Spirakis): each track gets the key `u^(1/w)` with `u` uniform in (0, 1] and weight
//!   `w = 1 / (1 + play_count)`, and tracks are sorted by descending key. This is equivalent to
//!   repeatedly drawing the next track with probability proportional to its weight among the
//!   tracks left. Keys are compared in log space (`(1 + plays) * ln u`), so big counts never
//!   underflow. With equal counts the order is uniform again. When the track that just played may
//!   not come first (a new repeat-all cycle) and it drew the top key, it gets a fresh key below the
//!   best other one; the keys are exponential (memoryless), so this is exactly "draw the first
//!   track among the others, then go on as usual".
//! * `no_artist_repeat`: see "Artist spreading" below.
//!
//! # Artist spreading (`no_artist_repeat`)
//!
//! After shuffling, one linear pass walks the order. A track whose primary artist
//! ([`TrackInfo::primary_artist_key`]) equals the artist placed just before it is held back in a
//! FIFO list and placed as soon as a different artist has been placed; all other tracks keep their
//! shuffled order. A track is only held when it matches the last placed artist and held tracks are
//! released as soon as the last artist differs, so all held tracks always share one artist and each
//! step is O(1): the pass is O(n). If tracks are still held when the input runs out (a clump at the
//! end), they are inserted into distinct, randomly chosen gaps whose two neighbours are other
//! artists (also O(n)). By the pigeonhole principle this places all of them whenever a repeat-free
//! order exists for that artist. Tracks only move where a repeat would otherwise happen, and only as
//! far as needed, so the result stays as close to the uniform shuffle as the rule allows. When no
//! repeat-free order exists (say 10 songs by one artist and 2 by others) the leftover songs of that
//! artist end up together at the end. Tracks with no known artist never count as repeats.
//!
//! # Cycles (`no_repeat_until_exhausted`)
//!
//! `history.played_in_cycle[source.cache_key()]` holds the URIs played since the current cycle of
//! that source began; [`record_play`] adds to it, and so does [`mark_seen`] for a track that was
//! left without a counted play (skipped, or it could not be played): it had its turn in this cycle,
//! so it neither holds the cycle open nor comes first in every new shuffle. A new shuffled order
//! puts tracks that are not in that set first and the already-played ones (plus the track playing
//! right now) after them; each group is shuffled on its own and `no_artist_repeat` works inside
//! each group. A set that is empty or holds only the playing track counts as a fresh cycle (one
//! group). Tracks added to the playing order later (a reloaded list, an unblocked track) that are
//! not in the set go among the not-yet-played tracks, never behind already-played ones.
//!
//! The cycle is complete once every playable track of the source that the shuffle may play (not
//! blocked, when `skip_blocked` is on) has had its turn ([`cycle_complete`]); the controller then
//! calls [`reset_cycle`]. The simplest way is to call [`record_play_and_update_cycle`] after each
//! counted play and [`mark_seen_and_update_cycle`] after each skip, which do both. Because the set
//! is persisted, reopening a playlist days later continues with the songs not heard yet in this
//! cycle.
//!
//! # Duplicates
//!
//! A playlist may contain the same URI more than once. The queue works with positions (indices into
//! the track list), never just URIs. When a track list is replaced (`update_tracks`, `restore`),
//! copies are matched by occurrence: the second copy in the old list maps to the second copy in the
//! new one.
//!
//! # Controller notes
//!
//! * After a track counts as played: `record_play_and_update_cycle(&mut history, queue.source(),
//!   &uri, queue.tracks(), &rules)`; after leaving one without a counted play:
//!   `mark_seen_and_update_cycle(..)` with the same arguments.
//! * For a manual "Next" press while repeat-one is on, call `advance` with `RepeatMode::All`
//!   (users expect Next to move on); use the real mode for automatic end-of-track advances.
//! * `peek_next` predicts `advance` exactly as long as rules and history do not change in between
//!   (it also stays right when the controller records the last play and resets the cycle at a
//!   repeat-all wrap). Treat it as a preload hint and compare URIs when `advance` returns.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

use rand::rngs::{StdRng, SysRng};
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Deserializer, Serialize};

use crate::model::{RepeatMode, ShuffleRules, Source, TrackInfo};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayHistory {
    /// Track URI -> times played (counted by the controller after 30 s or end of track).
    pub play_counts: HashMap<String, u32>,
    /// Blocked track URIs.
    pub blocked: HashSet<String>,
    /// Source cache key -> URIs already played in the current shuffle cycle.
    pub played_in_cycle: HashMap<String, HashSet<String>>,
}

/// Serializable queue state so a shuffle survives restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedQueue {
    pub source: Source,
    #[serde(default)]
    pub shuffled: bool,
    /// Play order as track URIs.
    pub order: Vec<String>,
    #[serde(default)]
    pub pos: usize,
    #[serde(default)]
    pub position_ms: u32,
    /// Spotify account the queue belongs to (empty in files from older versions: anyone's). Set by
    /// the controller; another account never gets it restored.
    #[serde(default)]
    pub username: String,
}

impl SavedQueue {
    /// URI of the track that was playing when the queue was saved.
    pub fn current_uri(&self) -> Option<&str> {
        self.order.get(self.pos).map(String::as_str)
    }

    /// Where to resume playback in a queue rebuilt by [`PlayQueue::restore`]: the saved position
    /// if the saved track is still the current one, otherwise 0 (start the new current track).
    pub fn resume_position_ms(&self, restored: &PlayQueue) -> u32 {
        match (self.current_uri(), restored.current()) {
            (Some(saved), Some(current)) if saved == current.uri => self.position_ms,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    pub history: PlayHistory,
    /// Read leniently: a queue saved in a shape this version can't read is dropped on its own,
    /// instead of failing the whole file (and with it the play history and blocked tracks).
    #[serde(deserialize_with = "lenient_queue")]
    pub queue: Option<SavedQueue>,
}

fn lenient_queue<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<SavedQueue>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_else(|e| {
        log::warn!("ignoring the saved queue, it can't be read: {e}");
        None
    }))
}

/// The play order of one source.
///
/// Invariants: `order` holds indices into `tracks`, each at most once, and only of playable tracks.
/// Without an orphan, `pos < order.len()` unless the queue is empty. With an orphan (the playing
/// track was removed by `update_tracks`), the orphan sits just before `order[pos]` and
/// `pos <= order.len()`.
#[derive(Debug)]
pub struct PlayQueue {
    source: Source,
    tracks: Arc<Vec<TrackInfo>>,
    /// Play order: indices into `tracks`.
    order: Vec<usize>,
    /// Current entry of `order` (or, while `orphan` is set, the entry that plays next).
    pos: usize,
    shuffled: bool,
    /// The playing track, after a reload removed it from the source (or made it unplayable).
    orphan: Option<TrackInfo>,
    rng: StdRng,
    /// Seed for the order of the next repeat-all cycle. Fixed in advance so that `peek_next` can
    /// tell what `advance` will do at the wrap without consuming randomness.
    next_cycle_seed: [u8; 32],
    /// Songs the user queued (Add to queue, Play next): they play before the list goes on, from
    /// any list.
    queued: Vec<TrackInfo>,
    /// The queued song playing now, between two of the list's songs (`pos` stays on the one
    /// before it).
    interlude: Option<TrackInfo>,
}

/// What `advance` would do next. Shared by `advance` and `peek_next` so they always agree.
enum NextStep {
    /// Repeat-one: play the current track again.
    Repeat,
    /// Move to this entry of the play order.
    At(usize),
    /// Repeat-all at the end of a shuffled order: start a new shuffled cycle.
    NewCycle,
    /// Nothing more to play.
    End,
}

impl PlayQueue {
    /// Build a queue. With `shuffle`, a fresh random order is generated; `start_index`
    /// (index into `tracks`), if given, is placed first. Without shuffle, order is source order
    /// starting at `start_index` (or 0).
    ///
    /// An unplayable start track is replaced by the next playable one in source order. Without a
    /// start index, the first track is chosen at random (shuffle) or is the first eligible one in
    /// source order; if nothing can be played the queue is empty (`current()` is `None`).
    pub fn new(
        source: Source,
        tracks: Arc<Vec<TrackInfo>>,
        start_index: Option<usize>,
        shuffle: bool,
        rules: &ShuffleRules,
        history: &PlayHistory,
    ) -> Self {
        Self::build(source, tracks, start_index, shuffle, rules, history, os_rng())
    }

    /// Deterministic variant for tests.
    #[cfg(test)]
    pub fn new_seeded(
        source: Source,
        tracks: Arc<Vec<TrackInfo>>,
        start_index: Option<usize>,
        shuffle: bool,
        rules: &ShuffleRules,
        history: &PlayHistory,
        seed: u64,
    ) -> Self {
        Self::build(source, tracks, start_index, shuffle, rules, history, StdRng::seed_from_u64(seed))
    }

    fn build(
        source: Source,
        tracks: Arc<Vec<TrackInfo>>,
        start_index: Option<usize>,
        shuffle: bool,
        rules: &ShuffleRules,
        history: &PlayHistory,
        mut rng: StdRng,
    ) -> Self {
        let next_cycle_seed = fresh_seed(&mut rng);
        let start = match start_index {
            Some(i) if i < tracks.len() => first_playable_from(&tracks, i),
            Some(i) => {
                log::warn!(
                    "start track {i} is past the end of the list ({} tracks); picking another start",
                    tracks.len()
                );
                None
            }
            None => None,
        };

        let (order, pos) = if shuffle {
            let candidates = eligible_indices(&tracks, rules, history, |i| Some(i) != start);
            let ctx = OrderCtx { tracks: &tracks, source: &source, rules, history };
            let rest = shuffled_order(&ctx, candidates, start.map(|s| &tracks[s]), &mut rng);
            (start.into_iter().chain(rest).collect::<Vec<_>>(), 0)
        } else {
            let order = playable_indices(&tracks);
            let first = start.or_else(|| {
                order.iter().copied().find(|&i| is_eligible(&tracks[i], rules, history))
            });
            match first.and_then(|f| order.binary_search(&f).ok()) {
                Some(p) => (order, p),
                None => (Vec::new(), 0),
            }
        };

        log::debug!(
            "queue built for {source:?}: {} of {} tracks, shuffle={shuffle}, start={start:?}",
            order.len(),
            tracks.len()
        );
        Self { source, tracks, order, pos, shuffled: shuffle, orphan: None, rng, next_cycle_seed, queued: Vec::new(), interlude: None }
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn tracks(&self) -> &Arc<Vec<TrackInfo>> {
        &self.tracks
    }

    pub fn is_shuffled(&self) -> bool {
        self.shuffled
    }

    pub fn current(&self) -> Option<&TrackInfo> {
        match &self.interlude {
            Some(t) => Some(t),
            None => current_of(&self.orphan, &self.tracks, &self.order, self.pos),
        }
    }

    /// Queues a song to play after the current one: first in line (`next`) or after the others.
    pub fn add_queued(&mut self, track: TrackInfo, next: bool) {
        if next {
            self.queued.insert(0, track);
        } else {
            self.queued.push(track);
        }
    }

    /// The songs queued, in the order they play.
    pub fn queued(&self) -> &[TrackInfo] {
        &self.queued
    }

    pub fn clear_queued(&mut self) {
        self.queued.clear();
    }

    /// Takes the queued song at `k` out of the queue.
    pub fn remove_queued(&mut self, k: usize) {
        if k < self.queued.len() {
            self.queued.remove(k);
        }
    }

    /// The repeat mode for the list's own next step: after a queued song, repeat-one moves on
    /// (it would otherwise play the list's song from before the queued one again).
    fn list_repeat(&self, repeat: RepeatMode) -> RepeatMode {
        if self.interlude.is_some() && repeat == RepeatMode::One { RepeatMode::All } else { repeat }
    }

    /// Index into `tracks` of the current track (e.g. to highlight its row). `None` when nothing is
    /// queued or the playing track was removed from the source by a reload.
    pub fn current_index(&self) -> Option<usize> {
        if self.orphan.is_some() || self.interlude.is_some() {
            None
        } else {
            self.order.get(self.pos).copied()
        }
    }

    /// The play order: indices into `tracks`.
    pub fn order(&self) -> &[usize] {
        &self.order
    }

    /// Index of the current entry within the play order.
    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// True when nothing is queued and nothing is playing from this queue.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty() && self.orphan.is_none()
    }

    /// Move to the next playable, non-blocked (if the rule is on) track. `RepeatMode::One`
    /// returns the current track; `All` wraps (re-shuffling for a new cycle when shuffled);
    /// `Off` returns None at the end.
    ///
    /// With `One`, a current track that has since been blocked is not repeated (the queue moves on
    /// as with `All`). A new shuffled cycle never starts with the track that just played (unless it
    /// is the only one). When `None` is returned the queue does not move.
    pub fn advance(&mut self, repeat: RepeatMode, rules: &ShuffleRules, history: &PlayHistory) -> Option<&TrackInfo> {
        // Queued songs first, in their order; then the list goes on after the song before them.
        if !self.queued.is_empty() {
            self.interlude = Some(self.queued.remove(0));
            return self.interlude.as_ref();
        }
        let repeat = self.list_repeat(repeat);
        self.interlude = None;
        match self.plan_next(repeat, rules, history) {
            NextStep::Repeat => {}
            NextStep::At(p) => {
                self.orphan = None;
                self.pos = p;
            }
            NextStep::NewCycle => {
                let order = self.next_cycle_order(rules, history);
                if order.is_empty() {
                    return None;
                }
                log::debug!("starting a new shuffled cycle of {} tracks", order.len());
                self.order = order;
                self.pos = 0;
                self.orphan = None;
                self.next_cycle_seed = fresh_seed(&mut self.rng);
            }
            // Nothing more (after a queued song too: it stays the one that played last).
            NextStep::End => return None,
        }
        self.current()
    }

    /// What `advance` would return, without moving (for preloading).
    pub fn peek_next(&self, repeat: RepeatMode, rules: &ShuffleRules, history: &PlayHistory) -> Option<&TrackInfo> {
        if let Some(q) = self.queued.first() {
            return Some(q);
        }
        let repeat = self.list_repeat(repeat);
        match self.plan_next(repeat, rules, history) {
            NextStep::Repeat => self.current(),
            NextStep::At(p) => self.order.get(p).map(|&i| &self.tracks[i]),
            NextStep::NewCycle => self.next_cycle_order(rules, history).first().map(|&i| &self.tracks[i]),
            NextStep::End => None,
        }
    }

    /// Move to the previous playable track (stays at 0 if at the start).
    ///
    /// Returns the track to play: the previous one, or the current one again when there is none.
    pub fn back(&mut self, rules: &ShuffleRules, history: &PlayHistory) -> Option<&TrackInfo> {
        // From a queued song: back to the list's song before it.
        if self.interlude.take().is_some() {
            return self.current();
        }
        let previous = (0..self.pos.min(self.order.len()))
            .rev()
            .find(|&k| is_eligible(&self.tracks[self.order[k]], rules, history));
        if let Some(k) = previous {
            self.pos = k;
            self.orphan = None;
        }
        self.current()
    }

    /// Jump to a given index into `tracks` (source order), e.g. double-click on a row.
    ///
    /// An unplayable track is replaced by the next playable one in source order (wrapping around).
    /// A blocked track plays, since the user picked it. When shuffled, the picked track is moved to
    /// right after the current one and becomes current; the rest of the order is unchanged, except
    /// that with `no_artist_repeat` the upcoming part is re-checked for artist repeats (taking the
    /// track out joins its two neighbours, and it now plays before another track).
    /// Returns `None` (and does not move) for an index outside the list or when nothing is playable.
    pub fn jump_to_track(&mut self, track_index: usize, rules: &ShuffleRules) -> Option<&TrackInfo> {
        if track_index >= self.tracks.len() {
            log::warn!("cannot jump to track {track_index}: the list has only {} tracks", self.tracks.len());
            return None;
        }
        let target = first_playable_from(&self.tracks, track_index)?;
        self.interlude = None;

        if self.shuffled {
            let mut insert_at = self.scan_start().min(self.order.len());
            if let Some(k) = self.order.iter().position(|&i| i == target) {
                if self.orphan.is_none() && k == self.pos {
                    return self.current();
                }
                self.order.remove(k);
                if k < insert_at {
                    insert_at -= 1;
                }
            }
            self.order.insert(insert_at, target);
            self.pos = insert_at;
            if rules.no_artist_repeat {
                let tracks = Arc::clone(&self.tracks);
                let rest = self.order.split_off(insert_at + 1);
                let rest = spread_artists(rest, &tracks, Some(tracks[target].primary_artist_key()), &mut self.rng);
                self.order.extend(rest);
            }
        } else {
            if self.order.is_empty() {
                self.order = playable_indices(&self.tracks);
            }
            self.pos = match self.order.binary_search(&target) {
                Ok(p) => p,
                Err(_) => {
                    // Cannot happen while the invariants hold; rebuild rather than misbehave.
                    log::warn!("play order was out of date; rebuilding it");
                    self.order = playable_indices(&self.tracks);
                    self.order.binary_search(&target).unwrap_or(0)
                }
            };
        }
        self.orphan = None;
        self.current()
    }

    /// New random order for everything after the current track (current track stays current).
    ///
    /// Tracks already played in this order (before the current one) stay where they are, so `back`
    /// still works. The new remainder holds every eligible track not yet played in this order, with
    /// the current rules (so it also picks up rule changes). On an unshuffled queue this turns
    /// shuffle on, exactly like `set_shuffle(true, ..)`.
    pub fn reshuffle(&mut self, rules: &ShuffleRules, history: &PlayHistory) {
        if !self.shuffled {
            self.set_shuffle(true, rules, history);
            return;
        }
        if self.is_empty() {
            log::debug!("reshuffle ignored: nothing is queued");
            return;
        }
        let keep = self.scan_start().min(self.order.len());
        let mut taken = vec![false; self.tracks.len()];
        for &i in &self.order[..keep] {
            taken[i] = true;
        }
        let candidates = eligible_indices(&self.tracks, rules, history, |i| !taken[i]);
        let tracks = Arc::clone(&self.tracks);
        let prev = current_of(&self.orphan, &tracks, &self.order, self.pos);
        let ctx = OrderCtx { tracks: &tracks, source: &self.source, rules, history };
        let rest = shuffled_order(&ctx, candidates, prev, &mut self.rng);
        self.order.truncate(keep);
        self.order.extend(rest);
    }

    /// Turn shuffle on (reshuffle the rest) or off (source order, continuing after the current track).
    ///
    /// Turning shuffle on makes the current track the first entry of a new order followed by every
    /// other eligible track, shuffled with the rules. Asking for the mode the queue is already in
    /// changes nothing (use `reshuffle` for a new order).
    pub fn set_shuffle(&mut self, shuffle: bool, rules: &ShuffleRules, history: &PlayHistory) {
        if shuffle == self.shuffled {
            return;
        }
        self.shuffled = shuffle;
        if self.is_empty() {
            return;
        }
        let current = self.current_index();
        if shuffle {
            let candidates = eligible_indices(&self.tracks, rules, history, |i| Some(i) != current);
            let tracks = Arc::clone(&self.tracks);
            let prev = current_of(&self.orphan, &tracks, &self.order, self.pos);
            let ctx = OrderCtx { tracks: &tracks, source: &self.source, rules, history };
            let rest = shuffled_order(&ctx, candidates, prev, &mut self.rng);
            self.order = current.into_iter().chain(rest).collect();
        } else {
            self.order = playable_indices(&self.tracks);
        }
        self.pos = if shuffle {
            0
        } else {
            current.and_then(|c| self.order.binary_search(&c).ok()).unwrap_or(0)
        };
    }

    /// Replace the track list after the source was reloaded; keeps the current track and the
    /// relative order of remaining tracks, appends new tracks (randomly placed if shuffled).
    ///
    /// Details: removed or now-unplayable tracks leave the order. Shuffled: new tracks (and tracks
    /// that became playable) are spread at random positions after the current track (with
    /// `no_repeat_until_exhausted`, ones not played this cycle only among the upcoming tracks not
    /// played this cycle either); with `no_artist_repeat`, the upcoming part is then re-checked
    /// for artist repeats. Unshuffled: the
    /// order is the new source order, continuing after the current track. If the playing track
    /// itself was removed, `current()` keeps returning it until the queue moves on, and `advance`
    /// continues with the track that would have followed it.
    pub fn update_tracks(&mut self, tracks: Arc<Vec<TrackInfo>>, rules: &ShuffleRules, history: &PlayHistory) {
        let old = std::mem::replace(&mut self.tracks, tracks);
        if self.is_empty() {
            return;
        }
        let new = Arc::clone(&self.tracks);
        let map = map_by_uri(&old, &new);
        let alive = |o: usize| map[o].filter(|&n| new[n].playable);

        // Remap the existing order; `anchor` is where the current track (or, for an orphan, the
        // track after it) lands in the remapped order.
        let had_orphan = self.orphan.is_some();
        let mut kept: Vec<usize> = Vec::with_capacity(new.len());
        let mut anchor = None;
        for (k, &o) in self.order.iter().enumerate() {
            if k == self.pos {
                anchor = Some(kept.len());
                if !had_orphan {
                    match alive(o) {
                        Some(n) => kept.push(n),
                        None => {
                            log::info!("the playing track was removed from the list; it will finish first");
                            self.orphan = Some(old[o].clone());
                        }
                    }
                    continue;
                }
            }
            if let Some(n) = alive(o) {
                kept.push(n);
            }
        }
        let anchor = anchor.unwrap_or(kept.len());
        let removed_any = kept.len() < self.order.len();

        if self.shuffled {
            // Tracks that were not (playable) in the old list are new to this order.
            let mut known = vec![false; new.len()];
            for (o, mapped) in map.iter().enumerate() {
                if let Some(n) = *mapped
                    && old[o].playable
                {
                    known[n] = true;
                }
            }
            let additions = eligible_indices(&new, rules, history, |n| !known[n]);
            let changed = removed_any || !additions.is_empty();
            let split = (if self.orphan.is_some() { anchor } else { anchor + 1 }).min(kept.len());
            let upcoming = kept.split_off(split);
            let ctx = OrderCtx { tracks: &new, source: &self.source, rules, history };
            let mut upcoming = merge_additions(&ctx, upcoming, additions, &mut self.rng);
            if changed && rules.no_artist_repeat {
                let prev_key = current_of(&self.orphan, &new, &kept, anchor).map(TrackInfo::primary_artist_key);
                upcoming = spread_artists(upcoming, &new, prev_key, &mut self.rng);
            }
            kept.extend(upcoming);
            self.order = kept;
            self.pos = anchor;
        } else {
            let next = kept.get(anchor).copied();
            self.order = playable_indices(&new);
            self.pos = match next {
                Some(n) => self.order.binary_search(&n).unwrap_or_else(|p| p),
                None => self.order.len(),
            };
        }
        log::debug!("queue updated: {} tracks in order, pos {}", self.order.len(), self.pos);
    }

    /// Put the copies of `uri` that a shuffled order left out (it was blocked when the order was
    /// built) back into the upcoming part, e.g. after the user unblocks it. They are placed like
    /// new tracks in [`update_tracks`](Self::update_tracks). Returns true if the order changed.
    /// Unshuffled orders keep blocked tracks anyway (`advance` skips them), so nothing changes.
    pub fn reinsert(&mut self, uri: &str, rules: &ShuffleRules, history: &PlayHistory) -> bool {
        if !self.shuffled || self.is_empty() {
            return false;
        }
        let mut present = vec![false; self.tracks.len()];
        for &i in &self.order {
            present[i] = true;
        }
        let additions = eligible_indices(&self.tracks, rules, history, |i| !present[i] && self.tracks[i].uri == uri);
        if additions.is_empty() {
            return false;
        }
        let tracks = Arc::clone(&self.tracks);
        let prev_key = self.current().map(|t| t.primary_artist_key().to_owned());
        let start = self.scan_start().min(self.order.len());
        let upcoming = self.order.split_off(start);
        let ctx = OrderCtx { tracks: &tracks, source: &self.source, rules, history };
        let mut upcoming = merge_additions(&ctx, upcoming, additions, &mut self.rng);
        if rules.no_artist_repeat {
            upcoming = spread_artists(upcoming, &tracks, prev_key.as_deref(), &mut self.rng);
        }
        self.order.extend(upcoming);
        true
    }

    /// The next `n` tracks in play order, including ones `advance` would skip (the controller's
    /// "Up next" uses `upcoming_filtered`; the tests use this to look at the raw order).
    #[cfg(test)]
    pub fn upcoming(&self, n: usize) -> Vec<&TrackInfo> {
        let start = self.scan_start().min(self.order.len());
        self.order[start..].iter().take(n).map(|&i| &self.tracks[i]).collect()
    }

    /// Like `upcoming`, but leaves out tracks that `advance` would skip (blocked, if the rule is on).
    /// The queued songs come first.
    pub fn upcoming_filtered(&self, n: usize, rules: &ShuffleRules, history: &PlayHistory) -> Vec<&TrackInfo> {
        let start = self.scan_start().min(self.order.len());
        // After a queued song the list goes on with the entry after `pos`, which `scan_start`
        // already points past only when no queued song plays.
        let start = if self.interlude.is_some() { (self.pos + 1).min(self.order.len()) } else { start };
        self.queued
            .iter()
            .chain(self.order[start..].iter().map(|&i| &self.tracks[i]).filter(|t| is_eligible(t, rules, history)))
            .take(n)
            .collect()
    }

    pub fn save(&self, position_ms: u32) -> SavedQueue {
        SavedQueue {
            source: self.source.clone(),
            shuffled: self.shuffled,
            order: self.order.iter().map(|&i| self.tracks[i].uri.clone()).collect(),
            pos: self.pos,
            // A removed (orphan) track cannot be resumed; the next one starts from the beginning.
            position_ms: if self.orphan.is_some() { 0 } else { position_ms },
            username: String::new(),
        }
    }

    /// Rebuild from a saved queue against a (possibly changed) track list. Unknown URIs are dropped,
    /// new tracks are added. Returns None if nothing usable remains.
    ///
    /// If the saved current track is gone, the next surviving one becomes current; use
    /// [`SavedQueue::resume_position_ms`] to know where to resume. Shuffled queues get their new
    /// eligible tracks in random order after the remaining tracks not played this cycle (with
    /// `no_repeat_until_exhausted`; otherwise, and for new tracks already played this cycle, at the
    /// end). Unshuffled queues use the new source order.
    pub fn restore(
        saved: &SavedQueue,
        tracks: Arc<Vec<TrackInfo>>,
        rules: &ShuffleRules,
        history: &PlayHistory,
    ) -> Option<Self> {
        let mut rng = os_rng();
        let next_cycle_seed = fresh_seed(&mut rng);

        let saved_pos = saved.pos.min(saved.order.len().saturating_sub(1));
        let mut slots = UriSlots::new(&tracks);
        let mut mapped: Vec<usize> = Vec::with_capacity(saved.order.len());
        let mut anchor = 0;
        for (k, uri) in saved.order.iter().enumerate() {
            if k == saved_pos {
                anchor = mapped.len();
            }
            if let Some(i) = slots.take(uri)
                && tracks[i].playable
            {
                mapped.push(i);
            }
        }

        let (order, pos) = if saved.shuffled {
            let mut used = vec![false; tracks.len()];
            for &i in &mapped {
                used[i] = true;
            }
            let additions = eligible_indices(&tracks, rules, history, |i| !used[i]);
            let played = played_this_cycle(rules, history, &saved.source);
            let is_played = |i: usize| played.is_some_and(|p| p.contains(tracks[i].uri.as_str()));
            let (mut fresh, mut seen): (Vec<usize>, Vec<usize>) = additions.into_iter().partition(|&i| !is_played(i));
            fisher_yates(&mut fresh, &mut rng);
            fisher_yates(&mut seen, &mut rng);
            // New tracks not played this cycle go before the first upcoming track that was.
            let boundary = (anchor + 1..mapped.len()).find(|&k| is_played(mapped[k])).unwrap_or(mapped.len());
            let mut order = mapped;
            order.splice(boundary..boundary, fresh);
            order.extend(seen);
            (order, anchor)
        } else {
            let order = playable_indices(&tracks);
            let pos = mapped
                .get(anchor)
                .or(mapped.last())
                .and_then(|i| order.binary_search(i).ok())
                .unwrap_or(0);
            (order, pos)
        };

        if order.is_empty() {
            log::info!("saved queue for {:?} has no playable tracks left", saved.source);
            return None;
        }
        let pos = pos.min(order.len() - 1);
        Some(Self {
            source: saved.source.clone(),
            tracks,
            order,
            pos,
            shuffled: saved.shuffled,
            orphan: None,
            rng,
            next_cycle_seed,
            queued: Vec::new(),
            interlude: None,
        })
    }

    /// First entry of `order` that comes after the current track.
    fn scan_start(&self) -> usize {
        if self.orphan.is_some() {
            self.pos
        } else {
            self.pos + 1
        }
    }

    fn plan_next(&self, repeat: RepeatMode, rules: &ShuffleRules, history: &PlayHistory) -> NextStep {
        if repeat == RepeatMode::One && self.current().is_some_and(|t| is_eligible(t, rules, history)) {
            return NextStep::Repeat;
        }
        let eligible_at = |k: &usize| is_eligible(&self.tracks[self.order[*k]], rules, history);
        if let Some(p) = (self.scan_start()..self.order.len()).find(eligible_at) {
            return NextStep::At(p);
        }
        match repeat {
            RepeatMode::Off => NextStep::End,
            RepeatMode::All | RepeatMode::One => {
                if self.shuffled {
                    NextStep::NewCycle
                } else {
                    (0..self.order.len()).find(eligible_at).map_or(NextStep::End, NextStep::At)
                }
            }
        }
    }

    /// The order of the next repeat-all cycle: every eligible track, shuffled with the rules,
    /// generated from `next_cycle_seed` so `peek_next` and `advance` agree.
    fn next_cycle_order(&self, rules: &ShuffleRules, history: &PlayHistory) -> Vec<usize> {
        let mut rng = StdRng::from_seed(self.next_cycle_seed);
        let candidates = eligible_indices(&self.tracks, rules, history, |_| true);
        let ctx = OrderCtx { tracks: &self.tracks, source: &self.source, rules, history };
        shuffled_order(&ctx, candidates, self.current(), &mut rng)
    }
}

/// Record that a track was played in the current cycle for a source (for `no_repeat_until_exhausted`),
/// and bump its play count.
pub fn record_play(history: &mut PlayHistory, source: &Source, uri: &str) {
    if uri.is_empty() {
        return;
    }
    match history.play_counts.get_mut(uri) {
        Some(count) => *count = count.saturating_add(1),
        None => {
            history.play_counts.insert(uri.to_owned(), 1);
        }
    }
    mark_seen(history, source, uri);
}

/// Record that a track had its turn in the current cycle of a source without counting a play:
/// it was skipped before it counted, or it could not be played. Its play count is unchanged.
pub fn mark_seen(history: &mut PlayHistory, source: &Source, uri: &str) {
    if uri.is_empty() {
        return;
    }
    let played = history.played_in_cycle.entry(source.cache_key()).or_default();
    if !played.contains(uri) {
        played.insert(uri.to_owned());
    }
}

/// True when every playable track of `tracks` that the shuffle may play has had its turn in the
/// source's current cycle. Unplayable tracks never hold a cycle open, and neither do blocked ones
/// while `skip_blocked` is on (with it off they are played like any other track, so they count).
/// False for a list with nothing to play.
pub fn cycle_complete(history: &PlayHistory, source: &Source, tracks: &[TrackInfo], rules: &ShuffleRules) -> bool {
    let Some(played) = history.played_in_cycle.get(&source.cache_key()) else {
        return false;
    };
    let mut any = false;
    for track in tracks.iter().filter(|t| is_eligible(t, rules, history)) {
        if !played.contains(&track.uri) {
            return false;
        }
        any = true;
    }
    any
}

/// Start a new cycle for a source: every track counts as unplayed again.
pub fn reset_cycle(history: &mut PlayHistory, source: &Source) {
    history.played_in_cycle.remove(&source.cache_key());
}

/// `record_play`, then `reset_cycle` if that play completed the cycle. Returns true when a new
/// cycle started. This is what the controller should call after each counted play.
pub fn record_play_and_update_cycle(
    history: &mut PlayHistory,
    source: &Source,
    uri: &str,
    tracks: &[TrackInfo],
    rules: &ShuffleRules,
) -> bool {
    record_play(history, source, uri);
    update_cycle(history, source, tracks, rules)
}

/// `mark_seen`, then `reset_cycle` if that completed the cycle. Returns true when a new cycle
/// started. The controller calls this when it leaves a track without a counted play.
pub fn mark_seen_and_update_cycle(
    history: &mut PlayHistory,
    source: &Source,
    uri: &str,
    tracks: &[TrackInfo],
    rules: &ShuffleRules,
) -> bool {
    mark_seen(history, source, uri);
    update_cycle(history, source, tracks, rules)
}

fn update_cycle(history: &mut PlayHistory, source: &Source, tracks: &[TrackInfo], rules: &ShuffleRules) -> bool {
    if cycle_complete(history, source, tracks, rules) {
        log::info!("every track of {source:?} has had its turn; starting a new cycle");
        reset_cycle(history, source);
        true
    } else {
        false
    }
}

/// How many tracks of each source have had their turn in its current cycle, by
/// [`Source::cache_key`]. Sources whose cycle has not begun are left out.
pub fn round_progress(history: &PlayHistory) -> HashMap<String, usize> {
    history
        .played_in_cycle
        .iter()
        .filter(|(_, played)| !played.is_empty())
        .map(|(key, played)| (key.clone(), played.len()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Order building
// ---------------------------------------------------------------------------------------------

/// Borrowed inputs for building a shuffled order.
struct OrderCtx<'a> {
    tracks: &'a [TrackInfo],
    source: &'a Source,
    rules: &'a ShuffleRules,
    history: &'a PlayHistory,
}

/// Random order of `candidates` with the rules applied. `prev` is the track playing right before
/// this order (it is never among the candidates' first entry if another choice exists).
fn shuffled_order<R: Rng + ?Sized>(
    ctx: &OrderCtx<'_>,
    candidates: Vec<usize>,
    prev: Option<&TrackInfo>,
    rng: &mut R,
) -> Vec<usize> {
    let (unplayed, played) = split_by_cycle(ctx, candidates, prev.map(|t| t.uri.as_str()));
    let mut out = Vec::with_capacity(unplayed.len() + played.len());
    // An empty group consumes no randomness, so "one group" and "all in the second group" give
    // the same order for the same seed (this keeps peek_next and advance in agreement).
    for mut group in [unplayed, played] {
        if group.is_empty() {
            continue;
        }
        // The first entry of the whole order should not be the track that played just before.
        let avoid_first = if out.is_empty() { prev.map(|p| p.uri.as_str()) } else { None };
        if ctx.rules.favour_rarely_played {
            weighted_order(&mut group, ctx.tracks, ctx.history, avoid_first, rng);
        } else {
            fisher_yates(&mut group, rng);
            if let Some(uri) = avoid_first {
                avoid_same_uri_first(&mut group, ctx.tracks, uri, rng);
            }
        }
        let before = out.last().map(|&i| &ctx.tracks[i]).or(prev);
        if ctx.rules.no_artist_repeat {
            group = spread_artists(group, ctx.tracks, before.map(TrackInfo::primary_artist_key), rng);
        }
        out.extend(group);
    }
    out
}

/// The URIs already played in the source's current cycle, when `no_repeat_until_exhausted` uses them.
fn played_this_cycle<'a>(rules: &ShuffleRules, history: &'a PlayHistory, source: &Source) -> Option<&'a HashSet<String>> {
    if !rules.no_repeat_until_exhausted {
        return None;
    }
    history.played_in_cycle.get(&source.cache_key()).filter(|played| !played.is_empty())
}

/// Split candidates into (not yet played this cycle, already played) for `no_repeat_until_exhausted`.
/// Both keep the candidates' order. The playing track counts as played.
fn split_by_cycle(ctx: &OrderCtx<'_>, candidates: Vec<usize>, current_uri: Option<&str>) -> (Vec<usize>, Vec<usize>) {
    let Some(played) = played_this_cycle(ctx.rules, ctx.history, ctx.source) else {
        return (candidates, Vec::new());
    };
    // Nothing (or only the playing track) played yet: a fresh cycle, no grouping.
    if played.iter().all(|uri| Some(uri.as_str()) == current_uri) {
        return (candidates, Vec::new());
    }
    candidates.into_iter().partition(|&i| {
        let uri = ctx.tracks[i].uri.as_str();
        !played.contains(uri) && Some(uri) != current_uri
    })
}

/// Uniform Fisher-Yates shuffle.
fn fisher_yates<R: Rng + ?Sized>(items: &mut [usize], rng: &mut R) {
    for i in (1..items.len()).rev() {
        let j = uniform_below(rng, i + 1);
        items.swap(i, j);
    }
}

/// Uniform integer in `0..n` (`n > 0`) without modulo bias (Lemire's multiply-and-reject method).
fn uniform_below<R: Rng + ?Sized>(rng: &mut R, n: usize) -> usize {
    debug_assert!(n > 0, "uniform_below needs a non-empty range");
    let n = n as u64;
    let mut m = u128::from(rng.next_u64()) * u128::from(n);
    if (m as u64) < n {
        let threshold = n.wrapping_neg() % n;
        while (m as u64) < threshold {
            m = u128::from(rng.next_u64()) * u128::from(n);
        }
    }
    (m >> 64) as usize
}

/// Uniform float in (0, 1] with 53 random bits.
fn unit_interval<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    ((rng.next_u64() >> 11) + 1) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// Efraimidis-Spirakis weighted random order with `w = 1 / (1 + play_count)`:
/// key `u^(1/w) = u^(1 + plays)`, sorted descending, compared as `(1 + plays) * ln u`.
///
/// With `avoid_first`, entries with that URI may not come first (when another URI exists). If one
/// drew the top key, every such entry ahead of the best other entry gets a fresh key below that
/// one's: `-key` is exponentially distributed with rate `w`, and by memorylessness this gives
/// exactly the weighted order conditioned on another track coming first (the first track is a
/// weighted draw among the others, then every next one a weighted draw among all left).
fn weighted_order<R: Rng + ?Sized>(
    items: &mut Vec<usize>,
    tracks: &[TrackInfo],
    history: &PlayHistory,
    avoid_first: Option<&str>,
    rng: &mut R,
) {
    let exponent = |i: usize| 1.0 + f64::from(history.play_counts.get(tracks[i].uri.as_str()).copied().unwrap_or(0));
    let mut keyed: Vec<(f64, usize)> = items.iter().map(|&i| (exponent(i) * unit_interval(rng).ln(), i)).collect();
    keyed.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
    if let Some(uri) = avoid_first
        && let Some(first_other) = keyed.iter().position(|&(_, i)| tracks[i].uri != uri)
        && first_other > 0
    {
        let best_other = keyed[first_other].0;
        for entry in &mut keyed[..first_other] {
            entry.0 = best_other + exponent(entry.1) * unit_interval(rng).ln();
        }
        // On an exact tie (u = 1) the other track still goes first.
        let avoided = |i: usize| tracks[i].uri == uri;
        keyed.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| avoided(a.1).cmp(&avoided(b.1))));
    }
    items.clear();
    items.extend(keyed.into_iter().map(|(_, i)| i));
}

/// If the group starts with the same URI as the previous track (a duplicate, or the last track of
/// the previous cycle), swap a random later entry with a different URI into first place.
fn avoid_same_uri_first<R: Rng + ?Sized>(group: &mut [usize], tracks: &[TrackInfo], uri: &str, rng: &mut R) {
    if group.len() < 2 || tracks[group[0]].uri != uri {
        return;
    }
    let mut pick = None;
    for _ in 0..8 {
        let j = 1 + uniform_below(rng, group.len() - 1);
        if tracks[group[j]].uri != uri {
            pick = Some(j);
            break;
        }
    }
    // Almost every entry is a copy of the same track: take the first different one.
    let pick = pick.or_else(|| (1..group.len()).find(|&j| tracks[group[j]].uri != uri));
    if let Some(j) = pick {
        group.swap(0, j);
    }
}

/// Reorder so that no two neighbours share a primary artist where possible (see the module docs).
/// `prev` is the artist key of the track playing before this sequence. O(n).
fn spread_artists<R: Rng + ?Sized>(seq: Vec<usize>, tracks: &[TrackInfo], prev: Option<&str>, rng: &mut R) -> Vec<usize> {
    let key = |i: usize| tracks[i].primary_artist_key();
    let clash = |k: &str, last: Option<&str>| !k.is_empty() && last == Some(k);

    let mut out: Vec<usize> = Vec::with_capacity(seq.len());
    // Invariant: every held track has the same artist, and while any are held, `last` is that artist
    // or a different one (in which case the next step releases a held track).
    let mut held: VecDeque<usize> = VecDeque::new();
    let mut last = prev;
    let mut input = seq.into_iter();
    loop {
        if let Some(&h) = held.front() {
            let k = key(h);
            if !clash(k, last) {
                held.pop_front();
                out.push(h);
                last = Some(k);
                continue;
            }
        }
        let Some(i) = input.next() else { break };
        let k = key(i);
        if clash(k, last) {
            held.push_back(i);
        } else {
            out.push(i);
            last = Some(k);
        }
    }
    if held.is_empty() {
        return out;
    }

    // Leftover clump of one artist: put each into a distinct random gap whose neighbours are other
    // artists. Gap g sits before out[g] (g == out.len() is the end).
    let artist = key(held[0]);
    let left_ok = |g: usize| if g == 0 { prev != Some(artist) } else { key(out[g - 1]) != artist };
    let right_ok = |g: usize| g == out.len() || key(out[g]) != artist;
    let mut gaps: Vec<usize> = (0..=out.len()).filter(|&g| left_ok(g) && right_ok(g)).collect();
    let take = held.len().min(gaps.len());
    for i in 0..take {
        let j = i + uniform_below(rng, gaps.len() - i);
        gaps.swap(i, j);
    }
    let mut chosen = gaps[..take].to_vec();
    chosen.sort_unstable();

    let mut result = Vec::with_capacity(out.len() + held.len());
    let mut chosen = chosen.into_iter().peekable();
    for g in 0..=out.len() {
        if chosen.peek() == Some(&g) {
            chosen.next();
            result.extend(held.pop_front());
        }
        if let Some(&i) = out.get(g) {
            result.push(i);
        }
    }
    // No repeat-free placement exists for the rest: they stay together at the end.
    result.extend(held);
    result
}

/// Places `additions` (new to the order) at random among `upcoming` (the part of the order after
/// the current track), keeping `upcoming`'s order. With `no_repeat_until_exhausted`, additions not
/// played this cycle only go into the leading run of upcoming tracks not played this cycle either,
/// so no already-played track comes before them; additions already played go among the rest.
fn merge_additions<R: Rng + ?Sized>(
    ctx: &OrderCtx<'_>,
    mut upcoming: Vec<usize>,
    additions: Vec<usize>,
    rng: &mut R,
) -> Vec<usize> {
    let Some(played) = played_this_cycle(ctx.rules, ctx.history, ctx.source) else {
        return random_merge(upcoming, additions, rng);
    };
    let is_played = |i: usize| played.contains(ctx.tracks[i].uri.as_str());
    let boundary = upcoming.iter().position(|&i| is_played(i)).unwrap_or(upcoming.len());
    let (fresh, seen): (Vec<usize>, Vec<usize>) = additions.into_iter().partition(|&i| !is_played(i));
    let tail = upcoming.split_off(boundary);
    let mut merged = random_merge(upcoming, fresh, rng);
    merged.extend(random_merge(tail, seen, rng));
    merged
}

/// Random interleaving of `additions` (shuffled) into `existing`, keeping `existing`'s order.
/// Every interleaving is equally likely, so each addition lands at a uniformly random position.
fn random_merge<R: Rng + ?Sized>(existing: Vec<usize>, mut additions: Vec<usize>, rng: &mut R) -> Vec<usize> {
    if additions.is_empty() {
        return existing;
    }
    fisher_yates(&mut additions, rng);
    let (mut left_existing, mut left_added) = (existing.len(), additions.len());
    let mut out = Vec::with_capacity(left_existing + left_added);
    let mut existing = existing.into_iter();
    let mut additions = additions.into_iter();
    while left_existing + left_added > 0 {
        if uniform_below(rng, left_existing + left_added) < left_added {
            out.extend(additions.next());
            left_added -= 1;
        } else {
            out.extend(existing.next());
            left_existing -= 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn current_of<'a>(orphan: &'a Option<TrackInfo>, tracks: &'a [TrackInfo], order: &[usize], pos: usize) -> Option<&'a TrackInfo> {
    match orphan {
        Some(track) => Some(track),
        None => order.get(pos).map(|&i| &tracks[i]),
    }
}

fn is_eligible(track: &TrackInfo, rules: &ShuffleRules, history: &PlayHistory) -> bool {
    track.playable && !(rules.skip_blocked && history.blocked.contains(&track.uri))
}

/// Indices of all playable tracks, in source order.
fn playable_indices(tracks: &[TrackInfo]) -> Vec<usize> {
    tracks.iter().enumerate().filter(|(_, t)| t.playable).map(|(i, _)| i).collect()
}

/// Indices (source order) of tracks that `advance` may play and that pass `keep`.
fn eligible_indices(
    tracks: &[TrackInfo],
    rules: &ShuffleRules,
    history: &PlayHistory,
    keep: impl Fn(usize) -> bool,
) -> Vec<usize> {
    tracks
        .iter()
        .enumerate()
        .filter(|&(i, t)| keep(i) && is_eligible(t, rules, history))
        .map(|(i, _)| i)
        .collect()
}

/// `index` if it is playable, else the next playable track in source order (wrapping around).
fn first_playable_from(tracks: &[TrackInfo], index: usize) -> Option<usize> {
    (index..tracks.len()).chain(0..index).find(|&i| tracks[i].playable)
}

/// Maps each index of `old` to the index of the same URI in `new` (copies matched by occurrence).
fn map_by_uri(old: &[TrackInfo], new: &[TrackInfo]) -> Vec<Option<usize>> {
    let mut slots = UriSlots::new(new);
    old.iter().map(|t| slots.take(&t.uri)).collect()
}

/// For each URI, the positions holding it in source order; `take` hands each position out once.
struct UriSlots<'a> {
    head: HashMap<&'a str, usize>,
    next: Vec<usize>,
}

impl<'a> UriSlots<'a> {
    const NONE: usize = usize::MAX;

    fn new(tracks: &'a [TrackInfo]) -> Self {
        let mut head = HashMap::with_capacity(tracks.len());
        let mut next = vec![Self::NONE; tracks.len()];
        for (i, track) in tracks.iter().enumerate().rev() {
            if let Some(following) = head.insert(track.uri.as_str(), i) {
                next[i] = following;
            }
        }
        Self { head, next }
    }

    fn take(&mut self, uri: &str) -> Option<usize> {
        let slot = self.head.get_mut(uri)?;
        let i = *slot;
        if i == Self::NONE {
            return None;
        }
        *slot = self.next[i];
        Some(i)
    }
}

fn fresh_seed(rng: &mut StdRng) -> [u8; 32] {
    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);
    seed
}

/// A generator seeded from the operating system's random source.
fn os_rng() -> StdRng {
    match StdRng::try_from_rng(&mut SysRng) {
        Ok(rng) => rng,
        Err(e) => {
            log::warn!("the system random source is unavailable ({e}); using a time-based seed");
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            // RandomState carries per-process random keys, so this still differs between runs.
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u128(nanos);
            hasher.write_u32(std::process::id());
            StdRng::seed_from_u64(hasher.finish())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ArtistRef;
    use std::time::{Duration, Instant};

    fn track(id: usize, artist: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{id:022}"),
            name: format!("Track {id}"),
            artists: vec![ArtistRef { name: artist.to_owned(), uri: format!("spotify:artist:{artist}") }],
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 180_000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    fn distinct_tracks(n: usize) -> Arc<Vec<TrackInfo>> {
        Arc::new((0..n).map(|i| track(i, &format!("artist{i}"))).collect())
    }

    /// Tracks grouped by artist: `counts[a]` tracks by artist `a`, ids in sequence.
    fn artist_tracks(counts: &[usize]) -> Arc<Vec<TrackInfo>> {
        let mut v = Vec::new();
        for (a, &c) in counts.iter().enumerate() {
            for _ in 0..c {
                let id = v.len();
                v.push(track(id, &format!("A{a}")));
            }
        }
        Arc::new(v)
    }

    fn rules_off() -> ShuffleRules {
        ShuffleRules {
            no_artist_repeat: false,
            favour_rarely_played: false,
            skip_blocked: false,
            no_repeat_until_exhausted: false,
            truly_random: false,
        }
    }

    fn all_rules() -> ShuffleRules {
        ShuffleRules {
            no_artist_repeat: true,
            favour_rarely_played: true,
            skip_blocked: true,
            no_repeat_until_exhausted: true,
            truly_random: false,
        }
    }

    #[test]
    fn true_shuffle_sets_the_order_rules_aside() {
        let on = ShuffleRules { truly_random: true, ..all_rules() };
        let used = on.effective();
        assert!(!used.no_repeat_until_exhausted && !used.no_artist_repeat && !used.favour_rarely_played);
        assert!(used.skip_blocked, "blocked songs stay out");
        assert!(!on.rounds_on());
        assert_eq!(all_rules().effective(), all_rules());
    }

    fn src() -> Source {
        Source::Playlist("spotify:playlist:test".to_owned())
    }

    fn sorted(mut v: Vec<usize>) -> Vec<usize> {
        v.sort_unstable();
        v
    }

    fn uri_index(tracks: &[TrackInfo], uri: &str) -> usize {
        tracks.iter().position(|t| t.uri == uri).expect("uri in list")
    }

    fn adjacent_artist_repeats(tracks: &[TrackInfo], order: &[usize]) -> usize {
        order
            .windows(2)
            .filter(|w| tracks[w[0]].primary_artist_key() == tracks[w[1]].primary_artist_key())
            .count()
    }

    #[test]
    fn queue_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<PlayQueue>();
        check::<PersistedState>();
    }

    #[test]
    fn uniform_bounded_integers() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut counts = [0u32; 3];
        for _ in 0..30_000 {
            counts[uniform_below(&mut rng, 3)] += 1;
        }
        for c in counts {
            assert!((9_500..10_500).contains(&c), "{counts:?}");
        }
        assert_eq!(uniform_below(&mut rng, 1), 0);
    }

    // (1) Chi-square uniformity -----------------------------------------------------------------

    #[test]
    fn shuffle_is_uniform_over_all_permutations() {
        let tracks = distinct_tracks(4);
        let history = PlayHistory::default();
        // None of these may change the distribution here: all rules off; the default rules with an
        // empty history; favour-rarely-played with equal (zero) play counts.
        let rule_sets = [
            rules_off(),
            ShuffleRules::default(),
            ShuffleRules { favour_rarely_played: true, ..rules_off() },
        ];
        let runs = 60_000u64;
        for (r, rules) in rule_sets.iter().enumerate() {
            let mut counts: HashMap<Vec<usize>, u64> = HashMap::new();
            for seed in 0..runs {
                let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, rules, &history, seed + r as u64 * runs);
                *counts.entry(q.order.clone()).or_default() += 1;
            }
            assert_eq!(counts.len(), 24, "rule set {r}: not every permutation appeared");
            let expected = runs as f64 / 24.0;
            let chi2: f64 = counts.values().map(|&c| (c as f64 - expected).powi(2) / expected).sum();
            // 23 degrees of freedom: the 99.9th percentile is 49.7. 70 is generous, yet any real bias
            // (e.g. the classic "swap with any index" mistake) gives values in the hundreds.
            assert!(chi2 < 70.0, "rule set {r}: chi-square {chi2:.1} too large: {counts:?}");
        }
    }

    #[test]
    fn shuffle_rest_is_uniform_after_start_track() {
        let tracks = distinct_tracks(4);
        let history = PlayHistory::default();
        let runs = 30_000u64;
        let mut counts: HashMap<Vec<usize>, u64> = HashMap::new();
        for seed in 0..runs {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), Some(2), true, &rules_off(), &history, seed);
            assert_eq!(q.order[0], 2);
            *counts.entry(q.order[1..].to_vec()).or_default() += 1;
        }
        assert_eq!(counts.len(), 6);
        let expected = runs as f64 / 6.0;
        let chi2: f64 = counts.values().map(|&c| (c as f64 - expected).powi(2) / expected).sum();
        // 5 degrees of freedom: 99.9th percentile is 20.5.
        assert!(chi2 < 30.0, "chi-square {chi2:.1}: {counts:?}");
    }

    #[test]
    fn every_track_is_equally_likely_at_every_position() {
        let n = 10;
        let tracks = distinct_tracks(n);
        let history = PlayHistory::default();
        let runs = 50_000u64;
        let mut counts = vec![vec![0u64; n]; n];
        for seed in 0..runs {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules_off(), &history, seed);
            for (pos, &t) in q.order.iter().enumerate() {
                counts[t][pos] += 1;
            }
        }
        let expected = runs as f64 / n as f64;
        let chi2: f64 = counts.iter().flatten().map(|&c| (c as f64 - expected).powi(2) / expected).sum();
        // (n-1)^2 = 81 degrees of freedom: 99.9th percentile is about 125.
        assert!(chi2 < 150.0, "chi-square {chi2:.1}: {counts:?}");
        for row in &counts {
            for &c in row {
                assert!((c as f64 - expected).abs() < expected * 0.08, "cell {c} vs {expected}");
            }
        }
    }

    #[test]
    fn favour_rarely_played_pushes_played_tracks_back() {
        let tracks = distinct_tracks(10);
        let mut history = PlayHistory::default();
        history.play_counts.insert(tracks[0].uri.clone(), 100);
        let rules = ShuffleRules { favour_rarely_played: true, ..rules_off() };
        let runs = 3_000u64;
        let mut position_sum = 0usize;
        let mut first = 0;
        for seed in 0..runs {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            assert_eq!(sorted(q.order.clone()), (0..10).collect::<Vec<_>>());
            let p = q.order.iter().position(|&i| i == 0).unwrap();
            position_sum += p;
            first += usize::from(p == 0);
        }
        let mean = position_sum as f64 / runs as f64;
        assert!(mean > 8.0, "heavily played track should sit near the end, mean position {mean}");
        assert!(first < 15, "heavily played track came first {first} times");
    }

    // (2) Start track first -----------------------------------------------------------------------

    #[test]
    fn start_track_plays_first() {
        let tracks = distinct_tracks(12);
        let history = PlayHistory::default();
        for seed in 0..200u64 {
            let start = (seed % 12) as usize;
            for rules in [rules_off(), all_rules(), ShuffleRules::default()] {
                let q = PlayQueue::new_seeded(src(), tracks.clone(), Some(start), true, &rules, &history, seed);
                assert_eq!(q.current_index(), Some(start));
                assert_eq!(q.pos(), 0);
                assert_eq!(sorted(q.order.clone()), (0..12).collect::<Vec<_>>());
            }
            let q = PlayQueue::new_seeded(src(), tracks.clone(), Some(start), false, &rules_off(), &history, seed);
            assert_eq!(q.current_index(), Some(start));
            assert_eq!(q.order, (0..12).collect::<Vec<_>>());
            assert_eq!(q.pos(), start);
        }
        // Out-of-range start: still a valid queue.
        let q = PlayQueue::new_seeded(src(), tracks.clone(), Some(99), false, &rules_off(), &history, 1);
        assert_eq!(q.current_index(), Some(0));
        let q = PlayQueue::new_seeded(src(), Arc::new(Vec::new()), Some(0), true, &rules_off(), &history, 1);
        assert!(q.is_empty());
        assert!(q.current().is_none());
    }

    // (3) Blocked / unplayable never returned -----------------------------------------------------

    #[test]
    fn blocked_and_unplayable_tracks_are_never_returned() {
        let mut v: Vec<TrackInfo> = (0..20).map(|i| track(i, &format!("artist{i}"))).collect();
        for i in [3, 7, 15] {
            v[i].playable = false;
        }
        let tracks = Arc::new(v);
        let mut history = PlayHistory::default();
        for i in [0, 9, 19] {
            history.blocked.insert(tracks[i].uri.clone());
        }
        let rules = ShuffleRules { skip_blocked: true, ..rules_off() };
        let bad = |t: &TrackInfo| !t.playable || history.blocked.contains(&t.uri);

        for shuffle in [true, false] {
            for seed in 0..20u64 {
                let mut q = PlayQueue::new_seeded(src(), tracks.clone(), None, shuffle, &rules, &history, seed);
                assert!(!bad(q.current().unwrap()));
                if shuffle {
                    assert_eq!(q.len(), 14, "blocked and unplayable tracks are left out of the shuffle");
                }
                for _ in 0..100 {
                    let peeked = q.peek_next(RepeatMode::All, &rules, &history).map(|t| t.uri.clone());
                    let next = q.advance(RepeatMode::All, &rules, &history).map(|t| t.uri.clone());
                    assert_eq!(peeked, next);
                    assert!(!bad(q.current().unwrap()));
                }
                for _ in 0..30 {
                    assert!(!bad(q.back(&rules, &history).unwrap()));
                }
                for t in q.upcoming_filtered(50, &rules, &history) {
                    assert!(!bad(t));
                }
            }
        }

        // Jumping to an unplayable track plays the next playable one.
        for shuffle in [false, true] {
            let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(1), shuffle, &rules, &history, 3);
            assert_eq!(q.jump_to_track(3, &rules).unwrap().uri, tracks[4].uri);
            assert_eq!(q.current_index(), Some(4));
            assert_eq!(q.jump_to_track(15, &rules).unwrap().uri, tracks[16].uri);
            assert!(q.jump_to_track(20, &rules).is_none());
            assert_eq!(q.current_index(), Some(16));
        }
        // Unplayable start index: the next playable track starts.
        let q = PlayQueue::new_seeded(src(), tracks.clone(), Some(7), true, &rules, &history, 5);
        assert_eq!(q.current_index(), Some(8));

        // A track blocked after the (unshuffled) queue was built is skipped too.
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(4), false, &rules, &history, 5);
        let mut later = history.clone();
        later.blocked.insert(tracks[5].uri.clone());
        assert_eq!(q.advance(RepeatMode::Off, &rules, &later).unwrap().uri, tracks[6].uri);
        assert_eq!(q.back(&rules, &later).unwrap().uri, tracks[4].uri);

        // Nothing playable at all: empty queue, nothing returned.
        let mut none: Vec<TrackInfo> = (0..3).map(|i| track(i, "x")).collect();
        for t in &mut none {
            t.playable = false;
        }
        let mut q = PlayQueue::new_seeded(src(), Arc::new(none), Some(0), true, &rules, &history, 1);
        assert!(q.is_empty());
        assert!(q.advance(RepeatMode::All, &rules, &history).is_none());
        assert!(q.jump_to_track(1, &rules).is_none());
    }

    // (4) No artist repeat ------------------------------------------------------------------------

    #[test]
    fn no_artist_repeat_when_feasible() {
        let tracks = artist_tracks(&[10, 8, 6, 4, 2]);
        let n = tracks.len();
        let history = PlayHistory::default();
        let rules = ShuffleRules { no_artist_repeat: true, ..rules_off() };
        for seed in 0..400u64 {
            let start = if seed % 3 == 0 { None } else { Some(seed as usize % n) };
            let q = PlayQueue::new_seeded(src(), tracks.clone(), start, true, &rules, &history, seed);
            assert_eq!(sorted(q.order.clone()), (0..n).collect::<Vec<_>>());
            if let Some(s) = start {
                assert_eq!(q.order[0], s);
            }
            assert_eq!(adjacent_artist_repeats(&tracks, &q.order), 0, "seed {seed}: {:?}", q.order);
        }

        // With cycles and weights as well: half of each artist played, so both groups stay feasible.
        let mut history = PlayHistory::default();
        for i in (0..n).step_by(2) {
            record_play(&mut history, &src(), &tracks[i].uri);
        }
        for seed in 0..200u64 {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &all_rules(), &history, seed);
            assert_eq!(adjacent_artist_repeats(&tracks, &q.order), 0, "seed {seed}");
            assert!(q.order[..15].iter().all(|&i| i % 2 == 1), "unplayed tracks come first");
        }
    }

    #[test]
    fn no_artist_repeat_minimises_repeats_when_infeasible() {
        // 10 by one artist, 2 by another: at least 10 - 1 - 2 = 7 repeats are unavoidable.
        let tracks = artist_tracks(&[10, 2]);
        let history = PlayHistory::default();
        let rules = ShuffleRules { no_artist_repeat: true, ..rules_off() };
        for seed in 0..300u64 {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            assert_eq!(sorted(q.order.clone()), (0..12).collect::<Vec<_>>());
            assert_eq!(adjacent_artist_repeats(&tracks, &q.order), 7, "seed {seed}: {:?}", q.order);
        }
    }

    #[test]
    fn spread_artists_leaves_valid_orders_alone() {
        let tracks = artist_tracks(&[3, 3, 3]);
        let mut rng = StdRng::seed_from_u64(1);
        let order = vec![0, 3, 6, 1, 4, 7, 2, 5, 8];
        assert_eq!(spread_artists(order.clone(), &tracks, None, &mut rng), order);
        // A clump at the end is spread into gaps.
        let clumped = vec![3, 6, 4, 7, 0, 1, 2, 5, 8];
        let fixed = spread_artists(clumped, &tracks, None, &mut rng);
        assert_eq!(sorted(fixed.clone()), (0..9).collect::<Vec<_>>());
        assert_eq!(adjacent_artist_repeats(&tracks, &fixed), 0, "{fixed:?}");
    }

    // (5) Repeat modes ----------------------------------------------------------------------------

    #[test]
    fn repeat_modes() {
        let tracks = distinct_tracks(5);
        let history = PlayHistory::default();
        let rules = rules_off();

        // One: the same track again, the queue does not move.
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(2), false, &rules, &history, 1);
        for _ in 0..3 {
            assert_eq!(q.advance(RepeatMode::One, &rules, &history).unwrap().uri, tracks[2].uri);
        }
        assert_eq!(q.pos(), 2);

        // Off, unshuffled: to the end, then None, staying on the last track.
        assert_eq!(q.advance(RepeatMode::Off, &rules, &history).unwrap().uri, tracks[3].uri);
        assert_eq!(q.advance(RepeatMode::Off, &rules, &history).unwrap().uri, tracks[4].uri);
        assert!(q.peek_next(RepeatMode::Off, &rules, &history).is_none());
        assert!(q.advance(RepeatMode::Off, &rules, &history).is_none());
        assert_eq!(q.current().unwrap().uri, tracks[4].uri);

        // All, unshuffled: wraps to the top of the list.
        assert_eq!(q.peek_next(RepeatMode::All, &rules, &history).unwrap().uri, tracks[0].uri);
        assert_eq!(q.advance(RepeatMode::All, &rules, &history).unwrap().uri, tracks[0].uri);

        // Off, shuffled: every track once, then None.
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, 2);
        let mut seen = vec![q.current_index().unwrap()];
        while let Some(t) = q.advance(RepeatMode::Off, &rules, &history) {
            seen.push(uri_index(&tracks, &t.uri));
        }
        assert_eq!(sorted(seen), (0..5).collect::<Vec<_>>());

        // All, shuffled: new cycles; each a full permutation, never repeating across the boundary.
        for seed in 0..100u64 {
            let mut q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            let mut played = vec![q.current_index().unwrap()];
            for _ in 0..(5 * 6 - 1) {
                let peeked = q.peek_next(RepeatMode::All, &rules, &history).map(|t| t.uri.clone());
                let next = q.advance(RepeatMode::All, &rules, &history).unwrap().uri.clone();
                assert_eq!(peeked.as_deref(), Some(next.as_str()));
                played.push(uri_index(&tracks, &next));
            }
            for cycle in played.chunks(5) {
                assert_eq!(sorted(cycle.to_vec()), (0..5).collect::<Vec<_>>());
            }
            assert!(played.windows(2).all(|w| w[0] != w[1]), "immediate repeat: {played:?}");
        }

        // Two tracks with repeat-all must alternate.
        let two = distinct_tracks(2);
        for seed in 0..50u64 {
            let mut q = PlayQueue::new_seeded(src(), two.clone(), None, true, &rules, &history, seed);
            let mut last = q.current_index().unwrap();
            for _ in 0..10 {
                let now = uri_index(&two, &q.advance(RepeatMode::All, &rules, &history).unwrap().uri);
                assert_ne!(now, last);
                last = now;
            }
        }

        // A single track with repeat-all plays again.
        let one = distinct_tracks(1);
        let mut q = PlayQueue::new_seeded(src(), one.clone(), None, true, &rules, &history, 1);
        assert_eq!(q.advance(RepeatMode::All, &rules, &history).unwrap().uri, one[0].uri);

        // Repeat-one does not repeat a track that was blocked meanwhile.
        let block_rules = ShuffleRules { skip_blocked: true, ..rules_off() };
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(1), false, &block_rules, &history, 1);
        let mut blocked = history.clone();
        blocked.blocked.insert(tracks[1].uri.clone());
        assert_eq!(q.advance(RepeatMode::One, &block_rules, &blocked).unwrap().uri, tracks[2].uri);
    }

    #[test]
    fn peek_matches_advance_across_cycle_resets() {
        let tracks = distinct_tracks(6);
        let rules = ShuffleRules { favour_rarely_played: false, ..all_rules() };
        for seed in 0..50u64 {
            let mut history = PlayHistory::default();
            let mut q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            let mut played = vec![q.current_index().unwrap()];
            for _ in 0..25 {
                let peeked = q.peek_next(RepeatMode::All, &rules, &history).map(|t| t.uri.clone());
                let uri = q.current().unwrap().uri.clone();
                record_play_and_update_cycle(&mut history, &src(), &uri, &tracks, &rules);
                let next = q.advance(RepeatMode::All, &rules, &history).map(|t| t.uri.clone());
                assert_eq!(peeked, next, "seed {seed}");
                played.push(uri_index(&tracks, &next.unwrap()));
            }
            assert!(played.windows(2).all(|w| w[0] != w[1]));
            for cycle in played.chunks(6).filter(|c| c.len() == 6) {
                assert_eq!(sorted(cycle.to_vec()), (0..6).collect::<Vec<_>>());
            }
        }
    }

    // (6) Cycle exhaustion ------------------------------------------------------------------------

    #[test]
    fn cycle_puts_unplayed_first_and_resets_when_exhausted() {
        let tracks = distinct_tracks(10);
        let rules = ShuffleRules { no_repeat_until_exhausted: true, ..rules_off() };
        let mut history = PlayHistory::default();
        for t in &tracks[..5] {
            record_play(&mut history, &src(), &t.uri);
        }
        record_play(&mut history, &src(), &tracks[0].uri);
        assert_eq!(history.play_counts[&tracks[0].uri], 2);
        assert!(!cycle_complete(&history, &src(), &tracks, &rules));
        assert!(!cycle_complete(&history, &Source::LikedSongs, &tracks, &rules));

        for seed in 0..100u64 {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            assert!(q.order[..5].iter().all(|&i| i >= 5), "unplayed first: {:?}", q.order);
            assert!(q.order[5..].iter().all(|&i| i < 5));
            // Starting on an already played track: it plays first, then the unplayed ones.
            let q = PlayQueue::new_seeded(src(), tracks.clone(), Some(2), true, &rules, &history, seed);
            assert_eq!(q.order[0], 2);
            assert!(q.order[1..6].iter().all(|&i| i >= 5));
            assert!(q.order[6..].iter().all(|&i| i < 5));
        }

        // Play through: the cycle completes on the 5th new track and starts over.
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, 9);
        let mut plays = 0;
        loop {
            let uri = q.current().unwrap().uri.clone();
            plays += 1;
            if record_play_and_update_cycle(&mut history, &src(), &uri, &tracks, &rules) {
                break;
            }
            assert!(q.advance(RepeatMode::Off, &rules, &history).is_some());
        }
        assert_eq!(plays, 5);
        assert!(!history.played_in_cycle.contains_key(&src().cache_key()));
        // A fresh cycle: no grouping, so anything may come first.
        let firsts: HashSet<usize> = (0..200u64)
            .map(|seed| PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed).order[0])
            .collect();
        assert_eq!(firsts.len(), 10);

        // Unplayable tracks, and blocked ones while they are skipped, do not hold the cycle open;
        // other sources are separate.
        let skip = ShuffleRules { skip_blocked: true, ..rules };
        let mut v: Vec<TrackInfo> = (0..4).map(|i| track(i, "x")).collect();
        v[3].playable = false;
        let mut h = PlayHistory::default();
        h.blocked.insert(v[2].uri.clone());
        record_play(&mut h, &Source::LikedSongs, &v[0].uri);
        record_play(&mut h, &src(), &v[0].uri);
        assert!(!cycle_complete(&h, &src(), &v, &skip));
        record_play(&mut h, &src(), &v[1].uri);
        assert!(cycle_complete(&h, &src(), &v, &skip));
        assert!(!cycle_complete(&h, &Source::LikedSongs, &v, &skip));
        reset_cycle(&mut h, &src());
        assert!(!cycle_complete(&h, &src(), &v, &skip));
        assert!(h.played_in_cycle.contains_key(&Source::LikedSongs.cache_key()));
        assert!(!cycle_complete(&PlayHistory::default(), &src(), &[], &skip));
    }

    // (7) Save / restore, updates, duplicates -----------------------------------------------------

    #[test]
    fn save_restore_round_trip_with_changed_tracks() {
        let tracks = distinct_tracks(12);
        let history = PlayHistory::default();
        let rules = rules_off();
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(3), true, &rules, &history, 11);
        for _ in 0..4 {
            q.advance(RepeatMode::Off, &rules, &history);
        }
        let saved = q.save(12_345);
        assert_eq!(saved.pos, 4);
        assert_eq!(saved.current_uri(), Some(q.current().unwrap().uri.as_str()));

        // Through JSON, as the controller persists it.
        let state = PersistedState { history: history.clone(), queue: Some(saved) };
        let json = serde_json::to_string(&state).unwrap();
        let saved = serde_json::from_str::<PersistedState>(&json).unwrap().queue.unwrap();
        assert!(serde_json::from_str::<PersistedState>("{}").unwrap().queue.is_none());

        // Same list: identical queue.
        let same = PlayQueue::restore(&saved, tracks.clone(), &rules, &history).unwrap();
        assert_eq!(same.order, q.order);
        assert_eq!(same.pos(), 4);
        assert!(same.is_shuffled());
        assert_eq!(saved.resume_position_ms(&same), 12_345);

        // Changed list: drop one played and one upcoming track, add two, reverse the source order.
        let removed_history = saved.order[1].clone();
        let removed_upcoming = saved.order[7].clone();
        let mut changed: Vec<TrackInfo> = tracks
            .iter()
            .filter(|t| t.uri != removed_history && t.uri != removed_upcoming)
            .cloned()
            .collect();
        changed.push(track(100, "new1"));
        changed.push(track(101, "new2"));
        changed.reverse();
        let changed = Arc::new(changed);
        let r = PlayQueue::restore(&saved, changed.clone(), &rules, &history).unwrap();
        assert_eq!(r.current().unwrap().uri, saved.order[4]);
        assert_eq!(r.pos(), 3);
        assert_eq!(saved.resume_position_ms(&r), 12_345);
        let uris: Vec<&str> = r.order.iter().map(|&i| changed[i].uri.as_str()).collect();
        let expected: Vec<&str> = saved
            .order
            .iter()
            .map(String::as_str)
            .filter(|u| *u != removed_history && *u != removed_upcoming)
            .collect();
        assert_eq!(&uris[..10], &expected[..]);
        let mut tail: Vec<&str> = uris[10..].to_vec();
        tail.sort_unstable();
        let (new1, new2) = (track(100, "").uri, track(101, "").uri);
        assert_eq!(tail, vec![new1.as_str(), new2.as_str()]);
        assert_eq!(sorted(r.order.clone()), (0..12).collect::<Vec<_>>());

        // Current track gone: the next survivor becomes current, resume from 0.
        let without_current: Vec<TrackInfo> =
            tracks.iter().filter(|t| t.uri != saved.order[4]).cloned().collect();
        let r = PlayQueue::restore(&saved, Arc::new(without_current), &rules, &history).unwrap();
        assert_eq!(r.current().unwrap().uri, saved.order[5]);
        assert_eq!(saved.resume_position_ms(&r), 0);

        // Unshuffled: current kept, new source order.
        let mut u = PlayQueue::new_seeded(src(), tracks.clone(), Some(5), false, &rules, &history, 1);
        u.advance(RepeatMode::Off, &rules, &history);
        let su = u.save(77);
        let mut changed_u: Vec<TrackInfo> = tracks.iter().filter(|t| t.uri != tracks[2].uri).cloned().collect();
        changed_u.push(track(200, "new"));
        changed_u.reverse();
        let changed_u = Arc::new(changed_u);
        let ru = PlayQueue::restore(&su, changed_u.clone(), &rules, &history).unwrap();
        assert!(!ru.is_shuffled());
        assert_eq!(ru.current().unwrap().uri, tracks[6].uri);
        assert_eq!(su.resume_position_ms(&ru), 77);
        assert_eq!(ru.order, (0..12).collect::<Vec<_>>());
        assert_eq!(ru.pos(), uri_index(&changed_u, &tracks[6].uri));

        // Nothing usable left.
        assert!(PlayQueue::restore(&saved, Arc::new(Vec::new()), &rules, &history).is_none());
        let empty_saved = SavedQueue {
            source: src(),
            shuffled: true,
            order: Vec::new(),
            pos: 3,
            position_ms: 0,
            username: String::new(),
        };
        assert!(PlayQueue::restore(&empty_saved, Arc::new(Vec::new()), &rules, &history).is_none());
    }

    #[test]
    fn duplicates_are_tracked_by_position() {
        // Same URI at positions 0 and 2.
        let a = track(1, "a");
        let tracks = Arc::new(vec![a.clone(), track(2, "b"), a.clone(), track(3, "c")]);
        let history = PlayHistory::default();
        let rules = rules_off();

        let q = PlayQueue::new_seeded(src(), tracks.clone(), Some(2), false, &rules, &history, 1);
        assert_eq!(q.current_index(), Some(2));

        for seed in 0..50u64 {
            let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(0), true, &rules, &history, seed);
            assert_eq!(sorted(q.order.clone()), vec![0, 1, 2, 3]);
            // The other copy never plays right after the first.
            assert_ne!(q.order[1], 2, "seed {seed}");
            assert_eq!(q.jump_to_track(2, &rules).unwrap().uri, a.uri);
            assert_eq!(q.current_index(), Some(2));

            let saved = q.save(0);
            let r = PlayQueue::restore(&saved, tracks.clone(), &rules, &history).unwrap();
            assert_eq!(sorted(r.order.clone()), vec![0, 1, 2, 3]);
            assert_eq!(r.pos(), q.pos());

            let before = q.order.clone();
            q.update_tracks(tracks.clone(), &rules, &history);
            assert_eq!(q.order, before);

            // Drop one copy: the other copy stays.
            let fewer = Arc::new(vec![track(2, "b"), a.clone(), track(3, "c")]);
            let r = PlayQueue::restore(&saved, fewer.clone(), &rules, &history).unwrap();
            assert_eq!(sorted(r.order.clone()), vec![0, 1, 2]);
        }
    }

    #[test]
    fn update_tracks_keeps_current_and_order() {
        let tracks = distinct_tracks(10);
        let history = PlayHistory::default();
        let rules = rules_off();
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(0), true, &rules, &history, 4);
        q.advance(RepeatMode::Off, &rules, &history);
        q.advance(RepeatMode::Off, &rules, &history);
        let current_uri = q.current().unwrap().uri.clone();
        let upcoming_before: Vec<String> = q.upcoming(100).iter().map(|t| t.uri.clone()).collect();
        let dropped = upcoming_before[2].clone();

        let mut v: Vec<TrackInfo> = tracks.iter().filter(|t| t.uri != dropped).cloned().collect();
        v.extend((20..23).map(|i| track(i, &format!("n{i}"))));
        v.swap(0, 5);
        let v = Arc::new(v);
        q.update_tracks(v.clone(), &rules, &history);
        assert_eq!(q.current().unwrap().uri, current_uri);
        assert_eq!(q.pos(), 2);
        assert_eq!(q.len(), 12);
        let after: Vec<String> = q.upcoming(100).iter().map(|t| t.uri.clone()).collect();
        let old_kept: Vec<&String> = after.iter().filter(|u| upcoming_before.contains(u)).collect();
        let expected: Vec<&String> = upcoming_before.iter().filter(|u| **u != dropped).collect();
        assert_eq!(old_kept, expected, "relative order of remaining tracks is kept");
        assert_eq!(after.len(), 9);
        assert_eq!(sorted(q.order.clone()), (0..12).collect::<Vec<_>>());

        // The playing track removed: it stays current until the queue moves on.
        let next_uri = q.upcoming(1)[0].uri.clone();
        let without: Vec<TrackInfo> = v.iter().filter(|t| t.uri != current_uri).cloned().collect();
        q.update_tracks(Arc::new(without), &rules, &history);
        assert_eq!(q.current().unwrap().uri, current_uri);
        assert_eq!(q.current_index(), None);
        assert_eq!(q.upcoming(1)[0].uri, next_uri);
        assert_eq!(q.save(500).position_ms, 0);
        assert_eq!(q.peek_next(RepeatMode::Off, &rules, &history).unwrap().uri, next_uri);
        assert_eq!(q.advance(RepeatMode::Off, &rules, &history).unwrap().uri, next_uri);

        // Unshuffled: follows the new source order after the current track.
        let mut u = PlayQueue::new_seeded(src(), tracks.clone(), Some(4), false, &rules, &history, 1);
        let mut v2: Vec<TrackInfo> = tracks.to_vec();
        v2.remove(5);
        v2.insert(0, track(50, "z"));
        let v2 = Arc::new(v2);
        u.update_tracks(v2.clone(), &rules, &history);
        assert_eq!(u.current().unwrap().uri, tracks[4].uri);
        assert_eq!(u.advance(RepeatMode::Off, &rules, &history).unwrap().uri, tracks[6].uri);
        // Remove the playing one: the next in source order follows.
        let v3: Vec<TrackInfo> = v2.iter().filter(|t| t.uri != tracks[6].uri).cloned().collect();
        u.update_tracks(Arc::new(v3), &rules, &history);
        assert_eq!(u.current().unwrap().uri, tracks[6].uri);
        assert_eq!(u.advance(RepeatMode::Off, &rules, &history).unwrap().uri, tracks[7].uri);
    }

    #[test]
    fn shuffle_toggle_and_reshuffle() {
        let tracks = distinct_tracks(10);
        let history = PlayHistory::default();
        let rules = rules_off();

        // Off: continue in source order after the current track.
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(4), true, &rules, &history, 8);
        q.advance(RepeatMode::Off, &rules, &history);
        let cur = q.current_index().unwrap();
        q.set_shuffle(false, &rules, &history);
        assert!(!q.is_shuffled());
        assert_eq!(q.current_index(), Some(cur));
        let expected_next = (cur + 1) % 10;
        assert_eq!(q.advance(RepeatMode::All, &rules, &history).unwrap().uri, tracks[expected_next].uri);

        // On: current first, every other track after it.
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(3), false, &rules, &history, 8);
        q.advance(RepeatMode::Off, &rules, &history);
        q.set_shuffle(true, &rules, &history);
        assert!(q.is_shuffled());
        assert_eq!(q.current_index(), Some(4));
        assert_eq!(q.pos(), 0);
        assert_eq!(sorted(q.order.clone()), (0..10).collect::<Vec<_>>());
        // Asking again changes nothing.
        let before = q.order.clone();
        q.set_shuffle(true, &rules, &history);
        assert_eq!(q.order, before);

        // Reshuffle keeps history and current, reorders the rest.
        q.advance(RepeatMode::Off, &rules, &history);
        q.advance(RepeatMode::Off, &rules, &history);
        let head = q.order[..3].to_vec();
        let mut changed = false;
        for _ in 0..10 {
            let rest_before = q.order[3..].to_vec();
            q.reshuffle(&rules, &history);
            assert_eq!(&q.order[..3], &head[..]);
            assert_eq!(q.pos(), 2);
            assert_eq!(sorted(q.order[3..].to_vec()), sorted(rest_before.clone()));
            changed |= q.order[3..] != rest_before[..];
        }
        assert!(changed);

        // Reshuffle picks up rule changes: newly blocked tracks leave the remainder.
        let mut blocked = history.clone();
        let victim = q.order[5];
        blocked.blocked.insert(tracks[victim].uri.clone());
        let block_rules = ShuffleRules { skip_blocked: true, ..rules_off() };
        q.reshuffle(&block_rules, &blocked);
        assert!(!q.order.contains(&victim));
        assert_eq!(q.len(), 9);
    }

    // (7b) Cycle bookkeeping for skips, blocked tracks and additions ------------------------------

    #[test]
    fn skipped_tracks_finish_the_cycle_without_counting_plays() {
        let tracks = distinct_tracks(5);
        let rules = ShuffleRules::default();
        let mut history = PlayHistory::default();
        // Tracks 0-3 are heard; track 4 is always skipped (or can't be played).
        for t in &tracks[..4] {
            assert!(!record_play_and_update_cycle(&mut history, &src(), &t.uri, &tracks, &rules));
        }
        // Until its turn is recorded, it holds the cycle open and leads every new shuffle.
        for seed in 0..50u64 {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            assert_eq!(q.order[0], 4);
        }
        assert!(mark_seen_and_update_cycle(&mut history, &src(), &tracks[4].uri, &tracks, &rules));
        assert!(!history.play_counts.contains_key(&tracks[4].uri), "a skip is not a play");
        assert!(!history.played_in_cycle.contains_key(&src().cache_key()), "a new cycle started");
        let firsts: HashSet<usize> = (0..200u64)
            .map(|seed| PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed).order[0])
            .collect();
        assert_eq!(firsts.len(), 5);

        // Mid-cycle, a skipped track goes with the played ones, behind the unheard ones.
        let mut h = PlayHistory::default();
        record_play(&mut h, &src(), &tracks[0].uri);
        mark_seen(&mut h, &src(), &tracks[1].uri);
        assert_eq!(h.play_counts.get(&tracks[1].uri), None);
        for seed in 0..50u64 {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &h, seed);
            assert!(q.order[..3].iter().all(|&i| i >= 2), "{:?}", q.order);
        }
    }

    #[test]
    fn round_progress_counts_turns_per_source() {
        let tracks = distinct_tracks(3);
        let rules = ShuffleRules::default();
        let mut history = PlayHistory::default();
        assert!(round_progress(&history).is_empty());
        record_play(&mut history, &src(), &tracks[0].uri);
        mark_seen(&mut history, &src(), &tracks[1].uri);
        // The same track twice is one turn.
        record_play(&mut history, &src(), &tracks[1].uri);
        record_play(&mut history, &Source::LikedSongs, &tracks[0].uri);
        let progress = round_progress(&history);
        assert_eq!(progress.len(), 2);
        assert_eq!(progress.get(&src().cache_key()), Some(&2));
        assert_eq!(progress.get(&Source::LikedSongs.cache_key()), Some(&1));
        // A finished round starts over at nothing, and an emptied set does not show up.
        assert!(record_play_and_update_cycle(&mut history, &src(), &tracks[2].uri, &tracks, &rules));
        history.played_in_cycle.insert("empty".to_owned(), HashSet::new());
        assert_eq!(round_progress(&history), HashMap::from([(Source::LikedSongs.cache_key(), 1)]));
    }

    #[test]
    fn blocked_tracks_that_are_played_hold_the_cycle_open() {
        let tracks = distinct_tracks(3);
        let mut history = PlayHistory::default();
        history.blocked.insert(tracks[2].uri.clone());
        let played_too = ShuffleRules { skip_blocked: false, ..ShuffleRules::default() };
        assert!(!record_play_and_update_cycle(&mut history, &src(), &tracks[0].uri, &tracks, &played_too));
        assert!(
            !record_play_and_update_cycle(&mut history, &src(), &tracks[1].uri, &tracks, &played_too),
            "the blocked track is still to be played"
        );
        for seed in 0..30u64 {
            let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &played_too, &history, seed);
            assert_eq!(q.order[0], 2, "it comes before the tracks already heard");
        }
        assert!(record_play_and_update_cycle(&mut history, &src(), &tracks[2].uri, &tracks, &played_too));

        // With skip_blocked on, the shuffle never plays it, so it never holds the cycle open.
        let skipping = ShuffleRules::default();
        let mut h = PlayHistory::default();
        h.blocked.insert(tracks[2].uri.clone());
        assert!(!record_play_and_update_cycle(&mut h, &src(), &tracks[0].uri, &tracks, &skipping));
        assert!(record_play_and_update_cycle(&mut h, &src(), &tracks[1].uri, &tracks, &skipping));
    }

    #[test]
    fn new_tracks_join_the_unplayed_part_of_the_cycle() {
        let tracks = distinct_tracks(10);
        let rules = ShuffleRules::default();
        let mut history = PlayHistory::default();
        for t in &tracks[..5] {
            record_play(&mut history, &src(), &t.uri);
        }
        let mut more = tracks.to_vec();
        more.push(track(10, "new"));
        let more = Arc::new(more);
        let unheard_first = |order: &[usize]| {
            let new_at = order.iter().position(|&i| i == 10).expect("new track queued");
            let first_heard = order.iter().position(|&i| i < 5).expect("heard tracks queued");
            new_at < first_heard
        };
        for seed in 0..200u64 {
            // Reloaded while playing: [current][4 unheard][5 heard] + 1 new track.
            let mut q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            let saved = q.save(0);
            q.update_tracks(more.clone(), &rules, &history);
            assert!(unheard_first(&q.order), "seed {seed}: {:?}", q.order);
            assert_eq!(sorted(q.order.clone()), (0..11).collect::<Vec<_>>());
            // Restored after a restart.
            let r = PlayQueue::restore(&saved, more.clone(), &rules, &history).unwrap();
            assert!(unheard_first(&r.order), "seed {seed}: {:?}", r.order);
            assert_eq!(r.current_index(), saved.current_uri().map(|u| uri_index(&more, u)));
        }
        // Without the rule a restored queue still gets new tracks at the end.
        let off = rules_off();
        let q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &off, &history, 3);
        let r = PlayQueue::restore(&q.save(0), more.clone(), &off, &history).unwrap();
        assert_eq!(r.order.last(), Some(&10));
    }

    #[test]
    fn unblocked_tracks_return_to_the_shuffle() {
        let tracks = distinct_tracks(10);
        let rules = ShuffleRules::default();
        let mut history = PlayHistory::default();
        history.blocked.insert(tracks[3].uri.clone());
        for seed in 0..50u64 {
            let mut h = history.clone();
            let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(0), true, &rules, &h, seed);
            assert!(!q.order.contains(&3));
            h.blocked.remove(&tracks[3].uri);
            assert!(q.reinsert(&tracks[3].uri, &rules, &h));
            assert!(q.order[q.pos() + 1..].contains(&3), "seed {seed}: {:?}", q.order);
            assert_eq!(q.current_index(), Some(0));
            assert_eq!(sorted(q.order.clone()), (0..10).collect::<Vec<_>>());
            assert!(!q.reinsert(&tracks[3].uri, &rules, &h), "already back");
        }
        // Unshuffled orders keep blocked tracks anyway.
        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(0), false, &rules, &history, 1);
        assert!(!q.reinsert(&tracks[3].uri, &rules, &PlayHistory::default()));
    }

    #[test]
    fn jumping_keeps_the_artist_rule() {
        let tracks = artist_tracks(&[3, 3, 3]);
        let rules = ShuffleRules { no_artist_repeat: true, ..rules_off() };
        let history = PlayHistory::default();
        for seed in 0..400u64 {
            let mut q = PlayQueue::new_seeded(src(), tracks.clone(), None, true, &rules, &history, seed);
            assert_eq!(adjacent_artist_repeats(&tracks, &q.order), 0);
            let pick = q.order[3];
            q.jump_to_track(pick, &rules).unwrap();
            assert_eq!(q.current_index(), Some(pick));
            assert_eq!(adjacent_artist_repeats(&tracks, &q.order[q.pos()..]), 0, "seed {seed}: {:?}", q.order);
            assert_eq!(sorted(q.order.clone()), (0..9).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_weighted_new_cycle_keeps_the_weights_when_the_last_track_drew_first() {
        let tracks = distinct_tracks(3);
        let source = src();
        let rules = ShuffleRules { favour_rarely_played: true, ..rules_off() };
        let mut rng = StdRng::seed_from_u64(5);

        // a just played (1 play), b (0 plays), c (1000 plays). c should almost never start the
        // new cycle: weight(c) / (weight(b) + weight(c)) is about 0.1 %.
        let mut history = PlayHistory::default();
        history.play_counts.insert(tracks[0].uri.clone(), 1);
        history.play_counts.insert(tracks[2].uri.clone(), 1000);
        let ctx = OrderCtx { tracks: &tracks, source: &source, rules: &rules, history: &history };
        let runs = 20_000;
        let mut c_first = 0;
        for _ in 0..runs {
            let order = shuffled_order(&ctx, vec![0, 1, 2], Some(&tracks[0]), &mut rng);
            assert_ne!(order[0], 0);
            c_first += usize::from(order[0] == 2);
        }
        assert!(c_first < 80, "c came first {c_first} times in {runs}");

        // Exact distribution: a (0 plays, just played), b (0), c (1): weights 1, 1, 1/2.
        // P(b,a,c) = 4/9, P(b,c,a) = 2/9, P(c,a,b) = 1/6, P(c,b,a) = 1/6.
        let mut history = PlayHistory::default();
        history.play_counts.insert(tracks[2].uri.clone(), 1);
        let ctx = OrderCtx { tracks: &tracks, source: &source, rules: &rules, history: &history };
        let runs = 36_000u64;
        let mut counts: HashMap<Vec<usize>, u64> = HashMap::new();
        for _ in 0..runs {
            *counts.entry(shuffled_order(&ctx, vec![0, 1, 2], Some(&tracks[0]), &mut rng)).or_default() += 1;
        }
        let expected = [(vec![1, 0, 2], 4.0 / 9.0), (vec![1, 2, 0], 2.0 / 9.0), (vec![2, 0, 1], 1.0 / 6.0), (vec![2, 1, 0], 1.0 / 6.0)];
        assert_eq!(counts.len(), 4, "{counts:?}");
        let chi2: f64 = expected
            .iter()
            .map(|(order, p)| {
                let e = runs as f64 * p;
                (counts.get(order).copied().unwrap_or(0) as f64 - e).powi(2) / e
            })
            .sum();
        // 3 degrees of freedom: the 99.9th percentile is 16.3.
        assert!(chi2 < 25.0, "chi-square {chi2:.1}: {counts:?}");
    }

    #[test]
    fn a_bad_saved_queue_does_not_lose_the_history() {
        let json = r#"{"history":{"play_counts":{"spotify:track:a":3},"blocked":["spotify:track:b"]},
                       "queue":{"source":"bogus","order":7}}"#;
        let state: PersistedState = serde_json::from_str(json).unwrap();
        assert!(state.queue.is_none());
        assert_eq!(state.history.play_counts["spotify:track:a"], 3);
        assert!(state.history.blocked.contains("spotify:track:b"));
        // Queues saved by older versions (no account) still load.
        let old = r#"{"queue":{"source":"LikedSongs","shuffled":true,"order":["spotify:track:a"],"pos":0,"position_ms":5}}"#;
        let queue = serde_json::from_str::<PersistedState>(old).unwrap().queue.unwrap();
        assert_eq!((queue.username.as_str(), queue.position_ms), ("", 5));
        assert!(serde_json::from_str::<PersistedState>(r#"{"queue":null}"#).unwrap().queue.is_none());
    }

    // (8) Performance smoke test ------------------------------------------------------------------

    #[test]
    fn ten_thousand_tracks_stay_fast() {
        let started = Instant::now();
        let n = 10_000;
        let tracks: Arc<Vec<TrackInfo>> = Arc::new(
            (0..n)
                .map(|i| {
                    let artist = if i % 10 < 3 { "big".to_owned() } else { format!("a{}", i % 400) };
                    let mut t = track(i, &artist);
                    t.playable = i % 211 != 0;
                    t
                })
                .collect(),
        );
        let mut history = PlayHistory::default();
        for (i, t) in tracks.iter().enumerate() {
            if i % 3 == 0 {
                history.play_counts.insert(t.uri.clone(), (i % 17) as u32);
            }
            if i % 2 == 0 {
                history.played_in_cycle.entry(src().cache_key()).or_default().insert(t.uri.clone());
            }
            if i % 97 == 0 {
                history.blocked.insert(t.uri.clone());
            }
        }
        let rules = all_rules();

        let mut q = PlayQueue::new_seeded(src(), tracks.clone(), Some(17), true, &rules, &history, 42);
        let total = q.len();
        assert!(total > 9_800);
        assert_eq!(adjacent_artist_repeats(&tracks, &q.order), 0);
        let mut played = 1;
        while q.peek_next(RepeatMode::Off, &rules, &history).is_some() {
            q.advance(RepeatMode::Off, &rules, &history).unwrap();
            played += 1;
        }
        assert_eq!(played, total);
        assert!(q.advance(RepeatMode::All, &rules, &history).is_some(), "wraps into a new cycle");

        for k in 0..100 {
            q.jump_to_track(k * 97 + 5, &rules);
        }
        q.reshuffle(&rules, &history);
        q.set_shuffle(false, &rules, &history);
        q.set_shuffle(true, &rules, &history);

        let mut changed: Vec<TrackInfo> = tracks.iter().filter(|t| !t.uri.ends_with('7')).cloned().collect();
        changed.extend((n..n + 1_000).map(|i| track(i, &format!("n{}", i % 50))));
        let changed = Arc::new(changed);
        q.update_tracks(changed.clone(), &rules, &history);
        let saved = q.save(1_000);
        let restored = PlayQueue::restore(&saved, changed.clone(), &rules, &history).unwrap();
        assert_eq!(restored.current().map(|t| t.uri.as_str()), saved.current_uri());
        // Every eligible track is already in the saved order, so nothing is added.
        assert!(restored.len() >= q.len());
        assert_eq!(&restored.order[..q.len()], &q.order[..]);

        // Worst case for the artist pass: 90% one artist (infeasible, many held tracks).
        let skewed: Arc<Vec<TrackInfo>> =
            Arc::new((0..n).map(|i| track(i, if i % 10 == 0 { "other" } else { "same" })).collect());
        let q = PlayQueue::new_seeded(src(), skewed.clone(), None, true, &rules, &PlayHistory::default(), 1);
        assert_eq!(q.len(), n);

        let elapsed = started.elapsed();
        // Linear/n log n work here takes well under a second even in debug builds; anything
        // quadratic in 10,000 tracks blows far past this.
        assert!(elapsed < Duration::from_secs(4), "too slow: {elapsed:?}");
    }

    #[test]
    fn queued_songs_play_next_then_the_list_goes_on() {
        let tracks = Arc::new((0..5).map(|i| track(i, "a")).collect::<Vec<_>>());
        let rules = ShuffleRules::default();
        let history = PlayHistory::default();
        let mut q = PlayQueue::new_seeded(Source::LikedSongs, tracks, Some(0), false, &rules, &history, 7);
        assert_eq!(q.current().unwrap().name, "Track 0");
        q.add_queued(track(101, "b"), false);
        q.add_queued(track(102, "b"), false);
        q.add_queued(track(100, "b"), true);
        assert_eq!(q.peek_next(RepeatMode::Off, &rules, &history).unwrap().name, "Track 100");
        let up: Vec<String> = q.upcoming_filtered(5, &rules, &history).iter().map(|t| t.name.clone()).collect();
        assert_eq!(up, ["Track 100", "Track 101", "Track 102", "Track 1", "Track 2"]);
        let mut played = Vec::new();
        for _ in 0..4 {
            played.push(q.advance(RepeatMode::Off, &rules, &history).unwrap().name.clone());
        }
        assert_eq!(played, ["Track 100", "Track 101", "Track 102", "Track 1"]);
        assert!(q.queued().is_empty());
    }

    #[test]
    fn back_from_a_queued_song_returns_to_the_list() {
        let tracks = Arc::new((0..3).map(|i| track(i, "a")).collect::<Vec<_>>());
        let rules = ShuffleRules::default();
        let history = PlayHistory::default();
        let mut q = PlayQueue::new_seeded(Source::LikedSongs, tracks, Some(1), false, &rules, &history, 7);
        q.add_queued(track(100, "b"), false);
        assert_eq!(q.advance(RepeatMode::One, &rules, &history).unwrap().name, "Track 100");
        assert_eq!(q.current_index(), None);
        // Repeat-one after a queued song moves on rather than replaying the song before it.
        assert_eq!(q.peek_next(RepeatMode::One, &rules, &history).unwrap().name, "Track 2");
        assert_eq!(q.back(&rules, &history).unwrap().name, "Track 1");
    }

}
