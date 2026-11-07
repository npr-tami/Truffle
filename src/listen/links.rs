//! Listen-along links: every friend gets a channel of their own, so one of them can be kicked (and
//! let back in) without the others noticing. Pure, like the rest of `listen`: no network and no
//! clock of its own; `controller/links.rs` does the making, writing and saving.
//!
//! - [`LinkBook`]: the links of this PC (`listen_links.json`), the removed ones kept as tombstones
//!   so they are never written or taken in again, and the next id.
//! - [`Fanout`]: one [`Outbox`] per link and a shared write budget, so ten links never use up the
//!   calls librespot allows the whole session (300 per 30 s, and a write is 2 of them).
//!
//! A kick is an ordinary stop written once, then nothing: the friend's Truffle sees the host stop
//! playing, as when they quit. Every link has the same playlist name; the friend's name stays here.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{HostState, Mode, Outbox, describe, parse};

/// Links per Spotify account (links being made count too).
pub const MAX_LINKS: usize = 10;
/// A link's name is at most this many characters.
pub const NAME_MAX: usize = 30;
/// The name of the one channel from before links (it becomes the first link).
pub const FIRST_LINK: &str = "First link";
/// The name of a channel an older Truffle made after links came.
pub const OLDER_LINK: &str = "Older link";
/// Removed links kept as tombstones (the oldest go first).
const REMOVED_KEEP: usize = 100;

/// Writes that may go at once, and how often one more may.
pub const WRITE_BURST: u32 = 12;
pub const WRITE_REFILL_MS: i64 = 1_000;
/// After Spotify says "slow down", nothing is written for this long.
pub const RATE_PAUSE_MS: i64 = 30_000;

/// One friend's link: a channel playlist of the host's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListenLink {
    /// Never used again for another link.
    pub id: u32,
    /// Who it is for. Only on this PC: Spotify never sees it.
    pub name: String,
    pub uri: String,
    /// The Spotify account it belongs to ("": not checked yet).
    pub owner: String,
    /// Kicked: nothing but a stop is ever written to it.
    pub kicked: bool,
    /// Kicked (or removed), and the stop hasn't reached Spotify yet.
    pub stop_owed: bool,
    /// Spotify says it is gone or not ours: never written.
    pub gone: bool,
    pub made_ms: i64,
}

impl ListenLink {
    /// Whether it belongs to `me` (or nobody checked yet).
    pub fn is_mine(&self, me: &str) -> bool {
        self.owner.is_empty() || self.owner.eq_ignore_ascii_case(me)
    }

    /// Whether what plays is written to it.
    pub fn live(&self, me: &str) -> bool {
        self.is_mine(me) && !self.kicked && !self.gone
    }
}

/// A removed link: never written or taken in again (only its last stop, if still owed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Removed {
    pub uri: String,
    pub owner: String,
    pub stop_owed: bool,
    /// Its playlist is to leave the library (once no stop is owed), and Spotify hasn't said it
    /// did yet: tried again until it does.
    pub delete_owed: bool,
}

impl Removed {
    fn is_mine(&self, me: &str) -> bool {
        self.owner.is_empty() || self.owner.eq_ignore_ascii_case(me)
    }
}

/// The links of this PC, in id order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkBook {
    pub links: Vec<ListenLink>,
    pub removed: Vec<Removed>,
    pub next_id: u32,
}

impl LinkBook {
    /// Takes in the channel older Truffles know (`Settings::listen_channel`) if the book doesn't
    /// know it: the one channel from before links becomes the "First link", one an older Truffle
    /// made later an "Older link". When it was made isn't known, so it has no date. Removed and
    /// kicked links are known, so neither comes back this way. Returns whether the book changed.
    pub fn take_in(&mut self, legacy: Option<&str>) -> bool {
        let Some(uri) = legacy.and_then(super::channel_uri) else { return false };
        if self.knows(&uri) {
            return false;
        }
        let fresh = self.links.is_empty() && self.removed.is_empty() && self.next_id == 0;
        let id = self.reserve_id();
        let name = if fresh { FIRST_LINK } else { OLDER_LINK };
        self.add(id, name, &uri, "", 0);
        true
    }

    /// Whether `uri` is one of the links, or was one.
    pub fn knows(&self, uri: &str) -> bool {
        self.links.iter().any(|l| l.uri == uri) || self.removed.iter().any(|r| r.uri == uri)
    }

    /// Whether `uri` is (or was) a link of `me`'s: joining it would be listening to oneself.
    pub fn is_own(&self, uri: &str, me: &str) -> bool {
        self.links.iter().any(|l| l.uri == uri && l.is_mine(me)) || self.removed.iter().any(|r| r.uri == uri && r.is_mine(me))
    }

    /// `me`'s links, in id order.
    pub fn mine<'a>(&'a self, me: &'a str) -> impl Iterator<Item = &'a ListenLink> + 'a {
        self.links.iter().filter(move |l| l.is_mine(me))
    }

    /// The links what plays is written to.
    pub fn live_uris(&self, me: &str) -> Vec<String> {
        self.links.iter().filter(|l| l.live(me)).map(|l| l.uri.clone()).collect()
    }

    /// The link older Truffles write to (`Settings::listen_channel`): the first that isn't kicked.
    pub fn mirror(&self, me: &str) -> Option<String> {
        self.links.iter().find(|l| l.live(me)).map(|l| l.uri.clone())
    }

    /// How many links `me` has (kicked and broken ones count, removed ones don't).
    pub fn count_mine(&self, me: &str) -> usize {
        self.mine(me).count()
    }

    /// The links and tombstones of `me`'s that still owe a stop.
    pub fn owed_stops(&self, me: &str) -> Vec<String> {
        let links = self.links.iter().filter(|l| l.stop_owed && !l.gone && l.is_mine(me)).map(|l| l.uri.clone());
        links.chain(self.removed.iter().filter(|r| r.stop_owed && r.is_mine(me)).map(|r| r.uri.clone())).collect()
    }

    /// The tombstones of `me`'s whose playlist is still to leave the library (their stop said).
    pub fn owed_deletes(&self, me: &str) -> Vec<String> {
        self.removed.iter().filter(|r| r.delete_owed && !r.stop_owed && r.is_mine(me)).map(|r| r.uri.clone()).collect()
    }

    pub fn get(&self, id: u32) -> Option<&ListenLink> {
        self.links.iter().find(|l| l.id == id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut ListenLink> {
        self.links.iter_mut().find(|l| l.id == id)
    }

    pub fn by_uri(&self, uri: &str) -> Option<&ListenLink> {
        self.links.iter().find(|l| l.uri == uri)
    }

    pub fn by_uri_mut(&mut self, uri: &str) -> Option<&mut ListenLink> {
        self.links.iter_mut().find(|l| l.uri == uri)
    }

    pub fn removed_mut(&mut self, uri: &str) -> Option<&mut Removed> {
        self.removed.iter_mut().find(|r| r.uri == uri)
    }

    /// A new id, never handed out before (the book is saved with it, so it isn't after a restart
    /// either).
    pub fn reserve_id(&mut self) -> u32 {
        let after = self.links.iter().map(|l| l.id).max().unwrap_or(0);
        let id = self.next_id.max(after.saturating_add(1)).max(1);
        self.next_id = id.saturating_add(1);
        id
    }

    /// A new link (its id from [`reserve_id`](Self::reserve_id)).
    pub fn add(&mut self, id: u32, name: &str, uri: &str, owner: &str, now_ms: i64) {
        let name = clean_name(name, id);
        self.links.push(ListenLink { id, name, uri: uri.to_owned(), owner: owner.to_owned(), made_ms: now_ms, ..Default::default() });
        self.links.sort_by_key(|l| l.id);
    }

    /// Kicks a link (it owes a stop, unless it is broken or another account's), or lets it back
    /// in. Returns whether anything changed.
    pub fn kick(&mut self, id: u32, kick: bool, me: &str) -> bool {
        let Some(l) = self.get_mut(id) else { return false };
        if l.kicked == kick {
            return false;
        }
        l.kicked = kick;
        l.stop_owed = kick && !l.gone && l.is_mine(me);
        true
    }

    /// Takes a link out for good; it stays as a tombstone, owing a stop unless it is broken or
    /// its kick's stop already went (then its playlist is owed out of the library at once; a
    /// broken one's is left alone).
    pub fn remove(&mut self, id: u32) -> Option<Removed> {
        let at = self.links.iter().position(|l| l.id == id)?;
        let l = self.links.remove(at);
        let stop_owed = !l.gone && (!l.kicked || l.stop_owed);
        let removed = Removed { uri: l.uri, owner: l.owner, stop_owed, delete_owed: !l.gone && !stop_owed };
        self.removed.push(removed.clone());
        self.trim_removed();
        Some(removed)
    }

    /// Keeps a playlist that is no link (any more) as a tombstone: never written or taken in
    /// again, only its stop if `stop_owed` (then it leaves the library).
    pub fn bury(&mut self, uri: &str, owner: &str, stop_owed: bool) {
        if let Some(r) = self.removed_mut(uri) {
            r.stop_owed |= stop_owed;
            return;
        }
        self.removed.push(Removed { uri: uri.to_owned(), owner: owner.to_owned(), stop_owed, delete_owed: false });
        self.trim_removed();
    }

    /// A broken link's new playlist: same id, name and kick; the old one becomes a tombstone.
    /// Returns false if the link is gone from the book.
    pub fn replace_uri(&mut self, id: u32, uri: &str, owner: &str, now_ms: i64) -> bool {
        let Some(l) = self.links.iter_mut().find(|l| l.id == id) else { return false };
        let old = std::mem::replace(&mut l.uri, uri.to_owned());
        let old_owner = std::mem::replace(&mut l.owner, owner.to_owned());
        l.gone = false;
        l.made_ms = now_ms;
        l.stop_owed = l.kicked;
        self.removed.push(Removed { uri: old, owner: old_owner, ..Default::default() });
        self.trim_removed();
        true
    }

    /// A new name (only on this PC). Returns whether it changed.
    pub fn rename(&mut self, id: u32, name: &str) -> bool {
        let name = clean_name(name, id);
        let Some(l) = self.get_mut(id) else { return false };
        if l.name == name {
            return false;
        }
        l.name = name;
        true
    }

    /// Puts a book read from disk right: every link once by playlist (a copy that was kicked
    /// keeps it kicked), every id once, a removed link never back, the ids in order. Returns
    /// whether anything changed.
    pub fn dedupe(&mut self) -> bool {
        let before = self.clone();
        let mut removed: Vec<Removed> = Vec::new();
        for r in std::mem::take(&mut self.removed) {
            match removed.iter_mut().find(|k| k.uri == r.uri) {
                Some(k) => {
                    k.stop_owed |= r.stop_owed;
                    k.delete_owed |= r.delete_owed;
                }
                None if !r.uri.is_empty() => removed.push(r),
                None => {}
            }
        }
        let mut links: Vec<ListenLink> = Vec::new();
        for l in std::mem::take(&mut self.links) {
            if l.uri.is_empty() || removed.iter().any(|r| r.uri == l.uri) {
                continue;
            }
            match links.iter_mut().find(|k| k.uri == l.uri) {
                Some(k) => {
                    k.kicked |= l.kicked;
                    k.stop_owed |= l.stop_owed;
                    k.gone |= l.gone;
                }
                None => links.push(l),
            }
        }
        links.sort_by_key(|l| l.id);
        let mut seen = HashSet::new();
        let mut renumber = Vec::new();
        for (i, l) in links.iter().enumerate() {
            if l.id == 0 || !seen.insert(l.id) {
                renumber.push(i);
            }
        }
        self.removed = removed;
        let top = links.iter().map(|l| l.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(top.saturating_add(1));
        for i in renumber {
            links[i].id = self.next_id;
            self.next_id = self.next_id.saturating_add(1);
        }
        links.sort_by_key(|l| l.id);
        self.links = links;
        *self != before
    }

    fn trim_removed(&mut self) {
        while self.removed.len() > REMOVED_KEEP {
            // The oldest that owes nothing goes first.
            let at = self.removed.iter().position(|r| !r.stop_owed && !r.delete_owed).unwrap_or(0);
            self.removed.remove(at);
        }
    }
}

/// A link's name as kept: trimmed, one space between words, at most [`NAME_MAX`] characters;
/// "Link 3" when nothing is left.
pub fn clean_name(raw: &str, id: u32) -> String {
    let squashed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = squashed.chars().take(NAME_MAX).collect();
    let cut = cut.trim_end();
    if cut.is_empty() { format!("Link {id}") } else { cut.to_owned() }
}

/// Whether a channel text is a stop (a kick, a removal, sharing off, quitting, nothing playing).
pub fn is_stop(text: &str) -> bool {
    parse(text).is_some_and(|s| s.mode == Mode::Stop)
}

// ------------------------------------------------------------------------------------ fan-out

/// One link's writes.
#[derive(Debug, Default)]
struct Slot {
    outbox: Outbox,
    /// When the waiting text started waiting.
    since_ms: i64,
    /// The text on its way.
    writing: Option<String>,
    /// When Spotify last took a text.
    sent_ms: Option<i64>,
}

impl Slot {
    fn take(&mut self, now_ms: i64) -> Option<String> {
        let text = self.outbox.take(now_ms)?;
        self.writing = Some(text.clone());
        Some(text)
    }
}

/// How one link's writes stand, for the test aid.
#[derive(Debug, Clone, Copy)]
pub struct SlotView<'a> {
    pub sent: Option<&'a str>,
    pub sent_ms: Option<i64>,
    pub waiting: Option<&'a str>,
    pub writing: bool,
}

/// The host's writes to every link: each link's own [`Outbox`] (at most one write on its way, at
/// most one a second, the newest text wins), and a budget they share: [`WRITE_BURST`] at once,
/// then one a second. A stop needs no budget and goes first.
#[derive(Debug)]
pub struct Fanout {
    slots: BTreeMap<String, Slot>,
    tokens: u32,
    /// When the budget last grew.
    refilled_ms: Option<i64>,
    /// After a "slow down": nothing is written before this.
    paused_until: Option<i64>,
}

impl Default for Fanout {
    fn default() -> Self {
        Self { slots: BTreeMap::new(), tokens: WRITE_BURST, refilled_ms: None, paused_until: None }
    }
}

impl Fanout {
    /// The newest text for one link.
    pub fn push(&mut self, uri: &str, text: String, now_ms: i64) {
        let slot = self.slots.entry(uri.to_owned()).or_default();
        let waiting = slot.outbox.pending().is_some();
        slot.outbox.push(text);
        if !waiting && slot.outbox.pending().is_some() {
            slot.since_ms = now_ms;
        }
    }

    /// The writes to send now, (link, text) each: stops first, then the links that waited
    /// longest, as far as the budget goes. Each is on its way until [`done`](Self::done).
    pub fn take_due(&mut self, now_ms: i64) -> Vec<(String, String)> {
        if let Some(until) = self.paused_until {
            // A wait longer than any this sets means the clock was set back.
            if now_ms < until && until - now_ms <= RATE_PAUSE_MS {
                return Vec::new();
            }
            self.paused_until = None;
        }
        self.refill(now_ms);
        let mut out = Vec::new();
        let mut queued: Vec<(i64, String)> = Vec::new();
        for (uri, slot) in &mut self.slots {
            if !slot.outbox.due(now_ms) {
                continue;
            }
            if slot.outbox.pending().is_some_and(is_stop) {
                if let Some(text) = slot.take(now_ms) {
                    out.push((uri.clone(), text));
                }
            } else {
                queued.push((slot.since_ms, uri.clone()));
            }
        }
        queued.sort();
        for (_, uri) in queued {
            if self.tokens == 0 {
                break;
            }
            if let Some(text) = self.slots.get_mut(&uri).and_then(|s| s.take(now_ms)) {
                self.tokens -= 1;
                out.push((uri, text));
            }
        }
        out
    }

    fn refill(&mut self, now_ms: i64) {
        let last = *self.refilled_ms.get_or_insert(now_ms);
        if now_ms < last {
            self.refilled_ms = Some(now_ms);
            return;
        }
        let more = (now_ms - last) / WRITE_REFILL_MS;
        if more > 0 {
            self.tokens = (i64::from(self.tokens) + more).min(i64::from(WRITE_BURST)) as u32;
            self.refilled_ms = Some(if self.tokens == WRITE_BURST { now_ms } else { last + more * WRITE_REFILL_MS });
        }
    }

    /// A write finished (a link pruned or made anew meanwhile is left alone). A failed one is
    /// tried again later, unless a newer text came.
    pub fn done(&mut self, uri: &str, text: String, ok: bool, now_ms: i64) {
        let Some(slot) = self.slots.get_mut(uri) else { return };
        slot.writing = None;
        if ok {
            slot.sent_ms = Some(now_ms);
        }
        slot.outbox.done(text, ok, now_ms);
    }

    /// Spotify said "slow down": no writes for a while.
    pub fn rate_limited(&mut self, now_ms: i64) {
        self.paused_until = Some(now_ms + RATE_PAUSE_MS);
    }

    /// Quitting: the last word for every link written to this session. A stop not landed yet
    /// (a kick, a removal, sharing off) is sent again; a link whose newest text isn't a stop
    /// hears that the music stopped. No budget: this is the end.
    pub fn last_words(&self, now_ms: i64) -> Vec<(String, String)> {
        let stop = describe(&HostState::stopped(now_ms));
        let mut out = Vec::new();
        for (uri, slot) in &self.slots {
            let unsent = slot.outbox.pending().or(slot.writing.as_deref());
            match unsent.or(slot.outbox.sent()) {
                Some(t) if is_stop(t) => {
                    if let Some(t) = unsent {
                        out.push((uri.clone(), t.to_owned()));
                    }
                }
                Some(_) => out.push((uri.clone(), stop.clone())),
                None => {}
            }
        }
        out
    }

    /// Takes back what waits to go to a link (and a write on its way, if it fails): nothing more
    /// goes to it until the next [`push`](Self::push).
    pub fn withdraw(&mut self, uri: &str) {
        if let Some(slot) = self.slots.get_mut(uri) {
            slot.outbox.withdraw();
        }
    }

    /// Forgets a link's writes (a write on its way is then ignored when it lands).
    pub fn forget(&mut self, uri: &str) {
        self.slots.remove(uri);
    }

    /// Keeps only the links `keep` says yes to (a write still on its way to another is ignored
    /// when it lands).
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.slots.retain(|uri, _| keep(uri));
    }

    /// Whether Spotify took a text for this link in this session.
    pub fn landed(&self, uri: &str) -> bool {
        self.slots.get(uri).is_some_and(|s| s.outbox.sent().is_some())
    }

    pub fn view(&self, uri: &str) -> Option<SlotView<'_>> {
        self.slots.get(uri).map(|s| SlotView { sent: s.outbox.sent(), sent_ms: s.sent_ms, waiting: s.outbox.pending(), writing: s.outbox.busy() })
    }

    pub fn budget(&self) -> u32 {
        self.tokens
    }

    pub fn paused(&self, now_ms: i64) -> bool {
        self.paused_until.is_some_and(|until| now_ms < until && until - now_ms <= RATE_PAUSE_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listen::off_description;

    const ME: &str = "alex";
    const OLD: &str = "spotify:playlist:1111111111111111111111";

    fn uri(n: u32) -> String {
        format!("spotify:playlist:{n:0>22}")
    }

    /// A book with `n` links of ME's, ids 1 to n.
    fn book(n: u32) -> LinkBook {
        let mut b = LinkBook::default();
        for i in 1..=n {
            let id = b.reserve_id();
            b.add(id, &format!("Friend {i}"), &uri(i), ME, 0);
        }
        b
    }

    fn play(at: i64) -> String {
        describe(&HostState { track: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(), position_ms: 1_000, sent_at_ms: at, mode: Mode::Play })
    }

    fn stop(at: i64) -> String {
        describe(&HostState::stopped(at))
    }

    /// Every write due now lands at once; returns them.
    fn land(f: &mut Fanout, now: i64) -> Vec<(String, String)> {
        let due = f.take_due(now);
        for (u, t) in &due {
            f.done(u, t.clone(), true, now);
        }
        due
    }

    #[test]
    fn the_old_channel_becomes_the_first_link() {
        let mut b = LinkBook::default();
        assert!(b.take_in(Some(OLD)));
        assert_eq!(b.links.len(), 1);
        let l = &b.links[0];
        // When it was made isn't known: no date (not the day Truffle took it in).
        assert_eq!((l.id, l.name.as_str(), l.uri.as_str(), l.owner.as_str(), l.made_ms), (1, FIRST_LINK, OLD, "", 0));
        assert!(!l.kicked && !l.gone && !l.stop_owed);
        // Not checked yet, so it counts as the logged-in account's, and older Truffles keep it.
        assert_eq!(b.live_uris(ME), vec![OLD.to_owned()]);
        assert_eq!(b.mirror(ME).as_deref(), Some(OLD));
        // No old channel: nothing to take in.
        assert!(!LinkBook::default().take_in(None));
        assert!(!LinkBook::default().take_in(Some("not a playlist")));
    }

    #[test]
    fn a_channel_an_older_truffle_made_is_taken_in_once() {
        let mut b = book(2);
        let made = uri(9);
        assert!(b.take_in(Some(&made)));
        assert_eq!(b.by_uri(&made).map(|l| (l.name.as_str(), l.made_ms)), Some((OLDER_LINK, 0)));
        assert!(!b.take_in(Some(&made)), "known now");
        assert!(!b.take_in(Some(&uri(1))), "one of the links");
        assert_eq!(b.links.len(), 3);
    }

    #[test]
    fn removed_and_kicked_links_never_come_back_through_the_old_setting() {
        let mut b = book(3);
        assert!(b.kick(1, true, ME));
        assert!(b.remove(2).is_some());
        let before = b.clone();
        assert!(!b.take_in(Some(&uri(1))));
        assert!(!b.take_in(Some(&uri(2))));
        assert_eq!(b, before);
        assert!(b.get(1).is_some_and(|l| l.kicked));
        assert!(b.knows(&uri(2)) && b.get(2).is_none());
    }

    #[test]
    fn the_old_truffles_link_is_the_first_one_not_kicked() {
        let mut b = book(3);
        assert_eq!(b.mirror(ME), Some(uri(1)));
        b.kick(1, true, ME);
        assert_eq!(b.mirror(ME), Some(uri(2)));
        b.get_mut(2).unwrap().gone = true;
        assert_eq!(b.mirror(ME), Some(uri(3)));
        b.kick(3, true, ME);
        assert_eq!(b.mirror(ME), None, "every link kicked: older Truffles get none");
        b.kick(1, false, ME);
        assert_eq!(b.mirror(ME), Some(uri(1)));
        // Another account's link is never the mirror.
        let mut other = LinkBook::default();
        other.add(1, "x", &uri(1), "kai", 0);
        assert_eq!(other.mirror(ME), None);
    }

    #[test]
    fn kicked_gone_and_other_account_links_get_no_news() {
        let mut b = book(4);
        b.kick(1, true, ME);
        b.get_mut(2).unwrap().gone = true;
        b.get_mut(3).unwrap().owner = "kai".into();
        assert_eq!(b.live_uris(ME), vec![uri(4)]);
        // Owners are compared as Spotify does, without case.
        assert_eq!(b.live_uris("ALEX"), vec![uri(4)]);
        // Kicking a broken link owes nothing (it can't be written); another account's neither.
        let l1 = b.get(1).unwrap();
        assert!(l1.kicked && l1.stop_owed);
        b.kick(2, true, ME);
        assert!(!b.get(2).unwrap().stop_owed);
        b.kick(3, true, ME);
        assert!(!b.get(3).unwrap().stop_owed);
        // Letting back in clears what was owed.
        b.kick(1, false, ME);
        assert!(!b.get(1).unwrap().stop_owed);
        assert_eq!(b.count_mine(ME), 3, "kicked and broken links count; another account's don't");
    }

    #[test]
    fn ids_are_never_reused() {
        let mut b = book(3);
        b.remove(3);
        let id = b.reserve_id();
        assert_eq!(id, 4, "not the removed link's 3");
        // A remade link keeps its id; its old playlist becomes a tombstone.
        b.get_mut(1).unwrap().gone = true;
        assert!(b.replace_uri(1, &uri(11), ME, 5));
        let l = b.get(1).unwrap();
        assert_eq!((l.uri.clone(), l.gone, l.made_ms), (uri(11), false, 5));
        assert!(b.knows(&uri(1)));
        // A book whose next id was lost still never hands out one in use.
        let mut lost = book(2);
        lost.next_id = 0;
        assert_eq!(lost.reserve_id(), 3);
    }

    #[test]
    fn names_are_trimmed_and_kept_to_30_characters() {
        assert_eq!(clean_name("  Sam  ", 1), "Sam");
        assert_eq!(clean_name("Sam \t and   Kai", 1), "Sam and Kai");
        assert_eq!(clean_name("", 4), "Link 4");
        assert_eq!(clean_name("   ", 5), "Link 5");
        let long = "é".repeat(40);
        assert_eq!(clean_name(&long, 1).chars().count(), 30, "characters, not bytes");
        // Cut at a space: no space left at the end.
        let spaced = format!("{} b", "a".repeat(29));
        assert_eq!(clean_name(&spaced, 1), "a".repeat(29));
        let mut b = book(1);
        assert!(b.rename(1, "  Alyssa "));
        assert_eq!(b.get(1).unwrap().name, "Alyssa");
        assert!(!b.rename(1, "Alyssa"));
        assert!(!b.rename(9, "nobody"));
    }

    #[test]
    fn at_most_ten_links_per_account() {
        let mut b = book(9);
        assert!(b.count_mine(ME) < MAX_LINKS);
        b.kick(9, true, ME);
        let id = b.reserve_id();
        b.add(id, "Ten", &uri(10), ME, 0);
        assert_eq!(b.count_mine(ME), MAX_LINKS, "kicked links count");
        b.remove(10);
        assert!(b.count_mine(ME) < MAX_LINKS, "removed ones don't");
        // Another account's links are its own ten.
        let id = b.reserve_id();
        b.add(id, "Theirs", &uri(20), "kai", 0);
        assert_eq!(b.count_mine(ME), 9);
        assert_eq!(b.count_mine("kai"), 1);
    }

    #[test]
    fn a_saved_book_reads_back_and_odd_fields_are_ignored() {
        let mut b = book(2);
        b.kick(2, true, ME);
        b.remove(1);
        let json = serde_json::to_string(&b).unwrap();
        let back: LinkBook = serde_json::from_str(&json).unwrap();
        assert_eq!(back, b);
        // A newer Truffle's fields are ignored, and missing ones take their defaults.
        let odd: LinkBook = serde_json::from_str(&format!(
            r#"{{"links":[{{"id":3,"name":"Sam","uri":"{u}","last_here_ms":5}}],"next_id":4,"later":true}}"#,
            u = uri(3)
        ))
        .unwrap();
        assert_eq!(odd.links.len(), 1);
        assert_eq!((odd.links[0].name.as_str(), odd.links[0].kicked, odd.links[0].owner.as_str()), ("Sam", false, ""));
        // The same playlist twice: kept once, and kicked if either copy was.
        let mut twice = LinkBook::default();
        twice.links.push(ListenLink { id: 1, name: "A".into(), uri: uri(1), ..Default::default() });
        twice.links.push(ListenLink { id: 2, name: "B".into(), uri: uri(1), kicked: true, stop_owed: true, ..Default::default() });
        twice.links.push(ListenLink { id: 2, name: "C".into(), uri: uri(3), ..Default::default() });
        twice.links.push(ListenLink { id: 4, name: "D".into(), uri: uri(4), ..Default::default() });
        twice.removed.push(Removed { uri: uri(4), ..Default::default() });
        assert!(twice.dedupe());
        assert_eq!(twice.links.len(), 2, "one copy of 1, and 4 stays removed");
        let one = twice.by_uri(&uri(1)).unwrap();
        assert!(one.kicked && one.stop_owed);
        let ids: HashSet<u32> = twice.links.iter().map(|l| l.id).collect();
        assert_eq!(ids.len(), 2, "every id once");
        assert!(twice.next_id > twice.links.iter().map(|l| l.id).max().unwrap());
        assert!(!twice.dedupe(), "nothing more to put right");
    }

    // ------------------------------------------------------------ fan-out

    #[test]
    fn news_reaches_every_live_link_each_at_its_own_pace() {
        let mut f = Fanout::default();
        let now = 1_000_000;
        for i in 1..=3 {
            f.push(&uri(i), play(now), now);
        }
        assert_eq!(land(&mut f, now).len(), 3);
        // Within a second, each link waits for its own next turn.
        for i in 1..=3 {
            f.push(&uri(i), play(now + 500), now + 500);
        }
        assert!(f.take_due(now + 500).is_empty());
        assert_eq!(land(&mut f, now + 1_000).len(), 3);
        // The same text again is not written again.
        f.push(&uri(1), play(now + 500), now + 2_000);
        assert!(f.take_due(now + 3_000).is_empty());
        assert!(f.landed(&uri(1)) && !f.landed(&uri(9)));
    }

    #[test]
    fn one_failing_link_doesnt_hold_up_the_others() {
        let mut f = Fanout::default();
        let now = 5_000_000;
        f.push(&uri(1), play(now), now);
        f.push(&uri(2), play(now), now);
        let due = f.take_due(now);
        assert_eq!(due.len(), 2);
        f.done(&uri(1), due[0].1.clone(), false, now + 100);
        f.done(&uri(2), due[1].1.clone(), true, now + 100);
        f.push(&uri(2), play(now + 1_500), now + 1_500);
        // Link 2's news goes; link 1 waits out its retry.
        assert_eq!(land(&mut f, now + 1_500), vec![(uri(2), play(now + 1_500))]);
        assert_eq!(land(&mut f, now + 5_200), vec![(uri(1), play(now))]);
        // A write landing for a link forgotten meanwhile changes nothing.
        f.forget(&uri(2));
        f.done(&uri(2), "x".into(), true, now + 6_000);
        assert!(f.view(&uri(2)).is_none());
    }

    #[test]
    fn a_burst_is_spread_by_the_budget_and_every_link_gets_the_newest() {
        let mut f = Fanout::default();
        let mut now = 9_000_000;
        // A new song with 10 links: all go at once, from the burst.
        for i in 1..=10 {
            f.push(&uri(i), play(now), now);
        }
        assert_eq!(land(&mut f, now).len(), 10);
        assert_eq!(f.budget(), WRITE_BURST - 10);
        // Then scrubbing: a new state every second for a minute. At most one write a second
        // (plus what is left of the burst), and no link waits more than about 10 s.
        let mut last_heard = [now; 10];
        let mut writes = 0;
        for step in 1..=60 {
            now += 1_000;
            for i in 1..=10 {
                f.push(&uri(i), play(now), now);
            }
            for (u, _) in land(&mut f, now) {
                let i = (1..=10).position(|n| uri(n) == u).unwrap();
                assert!(now - last_heard[i] <= 11_000, "link {} waited {} ms at step {step}", i + 1, now - last_heard[i]);
                last_heard[i] = now;
                writes += 1;
            }
        }
        assert!(writes <= 60 + 2, "{writes} writes in 60 s");
        assert!(writes >= 58, "the budget is used, {writes} writes");
        // Every link ends on the newest state.
        now += 30_000;
        land(&mut f, now);
        for i in 1..=10 {
            assert_eq!(f.view(&uri(i)).unwrap().sent, Some(play(now - 30_000).as_str()));
        }
    }

    #[test]
    fn stops_skip_the_queue_and_the_budget() {
        let mut f = Fanout::default();
        let now = 2_000_000;
        for i in 1..=12 {
            f.push(&uri(i), play(now), now);
        }
        assert_eq!(land(&mut f, now).len(), 12);
        assert_eq!(f.budget(), 0);
        // No budget left: news waits, but a kick's stop goes at once.
        f.push(&uri(1), play(now + 1_000), now + 1_000);
        f.push(&uri(2), play(now + 1_000), now + 1_000);
        f.push(&uri(5), stop(now + 1_000), now + 1_000);
        let due = f.take_due(now + 1_000);
        assert_eq!(due[0], (uri(5), stop(now + 1_000)), "the stop first");
        assert_eq!(due.len(), 2, "and one more, from the second's budget");
    }

    #[test]
    fn a_rate_limit_pauses_every_write_for_a_while() {
        let mut f = Fanout::default();
        let now = 3_000_000;
        f.push(&uri(1), play(now), now);
        let due = f.take_due(now);
        f.done(&uri(1), due[0].1.clone(), false, now);
        f.rate_limited(now);
        assert!(f.paused(now + 1));
        f.push(&uri(2), stop(now + 10), now + 10);
        assert!(f.take_due(now + 29_000).is_empty(), "not even a stop");
        let due = f.take_due(now + RATE_PAUSE_MS);
        assert_eq!(due.len(), 2);
        assert!(!f.paused(now + RATE_PAUSE_MS));
        // A clock set back doesn't keep it paused for hours.
        let mut g = Fanout::default();
        g.rate_limited(now);
        g.push(&uri(1), play(now), now);
        assert_eq!(g.take_due(now - 3_600_000).len(), 1);
    }

    #[test]
    fn quitting_tells_every_link_that_could_still_hear() {
        let mut f = Fanout::default();
        let now = 4_000_000;
        // 1 heard a song, 2 was kicked (its stop landed), 3's kick stop is still waiting, 4's
        // write is on its way, 5 was told sharing is off.
        for i in 1..=5 {
            f.push(&uri(i), play(now), now);
        }
        land(&mut f, now);
        f.push(&uri(2), stop(now + 1_000), now + 1_000);
        f.push(&uri(4), play(now + 1_000), now + 1_000);
        f.push(&uri(5), off_description(now + 1_000), now + 1_000);
        let due = f.take_due(now + 1_000);
        for (u, t) in due {
            if u != uri(4) {
                f.done(&u, t, true, now + 1_000);
            }
        }
        f.push(&uri(3), stop(now + 1_500), now + 1_500);
        let words = f.last_words(now + 2_000);
        let to = |i: u32| words.iter().find(|(u, _)| *u == uri(i)).map(|(_, t)| t.clone());
        assert_eq!(to(1), Some(stop(now + 2_000)));
        assert_eq!(to(2), None, "already stopped");
        assert_eq!(to(3), Some(stop(now + 1_500)), "its own stop, sent at last");
        assert_eq!(to(4), Some(stop(now + 2_000)));
        assert_eq!(to(5), None);
        assert_eq!(words.len(), 3);
    }

    #[test]
    fn a_stop_taken_back_never_goes_even_if_its_write_fails() {
        let mut f = Fanout::default();
        let now = 6_000_000;
        // Waiting: taken back, nothing goes.
        f.push(&uri(1), stop(now), now);
        f.withdraw(&uri(1));
        assert!(f.take_due(now).is_empty());
        // On its way, then taken back: if it fails, it isn't tried again.
        f.push(&uri(1), stop(now + 1_000), now + 1_000);
        let due = f.take_due(now + 1_000);
        assert_eq!(due.len(), 1);
        f.withdraw(&uri(1));
        f.done(&uri(1), due[0].1.clone(), false, now + 1_100);
        assert!(f.take_due(now + 10_000).is_empty(), "not tried again");
        // The next news goes as usual, and a later failure is tried again.
        f.push(&uri(1), play(now + 11_000), now + 11_000);
        let due = f.take_due(now + 11_000);
        assert_eq!(due, vec![(uri(1), play(now + 11_000))]);
        f.done(&uri(1), due[0].1.clone(), false, now + 11_100);
        assert_eq!(f.take_due(now + 17_000).len(), 1, "tried again");
        // Taking back from a link with nothing written is nothing.
        f.withdraw(&uri(9));
        assert!(f.view(&uri(9)).is_none());
    }

    #[test]
    fn a_buried_playlist_is_known_and_owes_its_stop_once() {
        let mut b = book(1);
        b.bury(&uri(5), ME, true);
        assert!(b.knows(&uri(5)) && b.is_own(&uri(5), ME));
        assert_eq!(b.owed_stops(ME), vec![uri(5)]);
        // Buried twice: one tombstone, still owing.
        b.bury(&uri(5), ME, false);
        assert_eq!(b.removed.len(), 1);
        assert!(b.removed[0].stop_owed);
        // Another account's owes only at its own login.
        b.bury(&uri(6), "kai", true);
        assert_eq!(b.owed_stops(ME), vec![uri(5)]);
        assert_eq!(b.owed_stops("kai"), vec![uri(6)]);
    }

    #[test]
    fn a_removed_links_playlist_is_owed_out_of_the_library_until_it_is() {
        let mut b = book(3);
        // Kicked and its stop said: removed, its playlist is owed out at once.
        b.kick(1, true, ME);
        b.get_mut(1).unwrap().stop_owed = false;
        let r = b.remove(1).unwrap();
        assert!(!r.stop_owed && r.delete_owed);
        // Live: its stop first (the playlist is owed once that landed).
        let r = b.remove(2).unwrap();
        assert!(r.stop_owed && !r.delete_owed);
        // Broken: neither.
        b.get_mut(3).unwrap().gone = true;
        let r = b.remove(3).unwrap();
        assert!(!r.stop_owed && !r.delete_owed);
        assert_eq!(b.owed_deletes(ME), vec![uri(1)]);
        // Never before its stop is said; another account's only at its own login.
        b.removed_mut(&uri(2)).unwrap().delete_owed = true;
        b.bury(&uri(7), "kai", false);
        b.removed_mut(&uri(7)).unwrap().delete_owed = true;
        assert_eq!(b.owed_deletes(ME), vec![uri(1)]);
        assert_eq!(b.owed_deletes("kai"), vec![uri(7)]);
        // A copy that doesn't owe it doesn't let it off.
        b.removed.push(Removed { uri: uri(1), ..Default::default() });
        assert!(b.dedupe());
        assert_eq!(b.owed_deletes(ME), vec![uri(1)]);
        // When old tombstones go, ones that still owe something stay.
        for i in 100..300 {
            b.bury(&uri(i), ME, false);
        }
        assert!(b.knows(&uri(1)) && b.knows(&uri(2)) && b.knows(&uri(7)));
        assert!(!b.knows(&uri(3)), "owing nothing, it went first");
    }

    #[test]
    fn the_kick_text_reads_like_an_ordinary_stop() {
        let kick = stop(1_761_400_000_000);
        assert_eq!(kick, "Listen along with me on Truffle. [tl1;;0;1761400000000;stop]");
        assert!(!kick.to_lowercase().contains("kick"));
        assert!(is_stop(&kick) && is_stop(&off_description(1)) && !is_stop(&play(1)));
    }
}
