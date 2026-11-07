//! Listen-along links (the rules are in `crate::listen::links`): one channel playlist per friend.
//! Making, kicking, letting back in, renaming and removing them, reading whose they are, and
//! keeping `listen_links.json` and the link older Truffles write to (`Settings::listen_channel`)
//! in step. Sharing itself (what is written, and when) is in `listen`.
//!
//! The log names a link by its number ("link #3"), never by its friend's name or its playlist:
//! anyone with the playlist can listen.

use std::sync::Arc;

use anyhow::Result;

use super::jobs::Job;
use super::listen::{Done, READ_TIMEOUT, SLOW_TIMEOUT, in_time, now_ms};
use super::{Controller, lock, persist};
use crate::listen::links::{MAX_LINKS, clean_name, is_stop};
use crate::listen::{self, HostState};
use crate::model::{Event, LinkState, LinkView};

/// A link being made.
pub(super) struct Making {
    pub(super) id: u32,
    /// Who it is for.
    pub(super) name: String,
    /// A broken link made anew (it is in the book already).
    pub(super) remake: bool,
    /// Its playlist was asked for.
    pub(super) started: bool,
}

impl Controller {
    /// The logged-in account ("" while logged out).
    pub(super) fn me(&self) -> String {
        self.profile.as_ref().map(|p| p.username.clone()).unwrap_or_default()
    }

    /// How the log names a link: by its number, never its friend or its playlist.
    pub(super) fn link_label(&self, uri: &str) -> String {
        match self.links.by_uri(uri) {
            Some(l) => format!("link #{}", l.id),
            None if self.links.knows(uri) => "a removed link".to_owned(),
            None => "a link no longer kept".to_owned(),
        }
    }

    /// Whether `uri` is (or was) one of this account's links, here or made in this session.
    pub(super) fn own_link(&self, uri: &str) -> bool {
        self.links.is_own(uri, &self.me()) || self.along.made.contains(uri) || lock(&self.settings).listen_channel.as_deref() == Some(uri)
    }

    /// The logged-in account's links as the Friends tab shows them, in id order.
    pub(super) fn links_view(&self) -> Vec<LinkView> {
        let me = self.me();
        let mut view: Vec<LinkView> = self
            .links
            .mine(&me)
            .map(|l| {
                let making = self.along.making.iter().any(|m| m.id == l.id)
                    || (self.along.made.contains(&l.uri) && !self.along.fanout.landed(&l.uri));
                let state = if making {
                    LinkState::Making
                } else if l.gone {
                    LinkState::Gone
                } else if l.kicked {
                    LinkState::Kicked
                } else {
                    LinkState::Ready
                };
                let link = matches!(state, LinkState::Ready | LinkState::Kicked).then(|| listen::channel_link(&l.uri)).flatten();
                LinkView { id: l.id, name: l.name.clone(), link, state, made_ms: l.made_ms, here_ms: None }
            })
            .collect();
        let now = now_ms();
        for m in self.along.making.iter().filter(|m| !m.remake) {
            view.push(LinkView { id: m.id, name: m.name.clone(), link: None, state: LinkState::Making, made_ms: now, here_ms: None });
        }
        view.sort_by_key(|v| v.id);
        view
    }

    pub(super) fn send_links(&self) {
        self.ui.send(Event::ListenLinks(Arc::new(self.links_view())));
    }

    pub(super) fn save_links(&self) {
        if self.links_held {
            log::warn!("listen-along links not saved: their file couldn't be opened at start");
            return;
        }
        if let Err(e) = persist::save_links(&self.paths, &self.links) {
            log::warn!("could not save the listen-along links: {e:#}");
        }
    }

    /// `listen_links.json` couldn't be opened at start (another program held it): read again at
    /// login, before anything could change the links.
    pub(super) fn reload_links(&mut self) {
        let loaded = persist::load_links(&self.paths);
        if loaded.held {
            self.ui.send(Event::Status(persist::LINKS_LOCKED.into()));
            return;
        }
        log::info!("listen along: the links file could be read at login");
        self.links_held = false;
        self.links = loaded.book;
        let legacy = lock(&self.settings).listen_channel.clone();
        self.links.take_in(legacy.as_deref());
        self.save_links();
        if let Some(notice) = loaded.notice {
            self.ui.send(Event::Status(notice.into()));
        }
    }

    /// Whether the links can't change now: `listen_links.json` couldn't be read at start (nor at
    /// login), so a change couldn't be saved, and the next start would undo it (a new link lost,
    /// a kicked friend hearing again). Says so (a note, not an error: nothing went wrong), and
    /// shows the links as they are (Remove takes its row away at once).
    fn links_locked(&self) -> bool {
        if self.links_held {
            self.ui.send(Event::Status(persist::LINKS_LOCKED.into()));
            self.send_links();
        }
        self.links_held
    }

    /// The link older Truffles write to follows the links: the first of this account's that
    /// isn't kicked (saved at once, so an older Truffle started next writes to no kicked link).
    pub(super) fn mirror_links(&mut self) {
        if self.profile.is_none() {
            return;
        }
        let mirror = self.links.mirror(&self.me());
        if lock(&self.settings).listen_channel != mirror {
            self.update_settings(|s| s.listen_channel = mirror);
            self.save_settings();
        }
    }

    /// After any change to the links: saved, the older Truffles' link follows, writes for links
    /// no longer here are dropped, and the UI hears it.
    pub(super) fn links_changed(&mut self) {
        self.save_links();
        self.mirror_links();
        let me = self.me();
        let links = &self.links;
        self.along.fanout.retain(|uri| {
            links.by_uri(uri).is_some_and(|l| l.is_mine(&me)) || links.removed.iter().any(|r| r.uri == uri && r.stop_owed)
        });
        self.send_links();
        self.send_share();
    }

    /// What a link should say right now: what plays, or "not sharing"; a kicked one,
    /// [stopped](Self::stop_words). While a song loads: stopped for now, and once the song plays
    /// it is shared with every link that hears (this one too), even if it plays on where it was
    /// shared before.
    fn words_now(&mut self, now: i64, kicked: bool) -> String {
        if kicked {
            return self.stop_words(now);
        }
        if !lock(&self.settings).listen_share {
            return listen::off_description(now);
        }
        match self.host_state(now) {
            Some(state) => listen::describe(&state),
            None => {
                self.along.shared = None;
                listen::describe(&HostState::stopped(now))
            }
        }
    }

    /// The stop a kick, a removal or a stop owed writes: what every other link would say if the
    /// music stopped now ("not sharing" while sharing is off), so a kicked friend's link looks
    /// like everyone else's.
    pub(super) fn stop_words(&self, now: i64) -> String {
        if lock(&self.settings).listen_share { listen::describe(&HostState::stopped(now)) } else { listen::off_description(now) }
    }

    // ------------------------------------------------------------ commands

    pub(super) fn cmd_listen_new_link(&mut self, name: String) {
        // Refusals are notes, not errors: nothing went wrong.
        if self.library.is_none() {
            self.ui.send(Event::Status("Log in to Spotify first.".into()));
            return;
        }
        if self.links_locked() {
            return;
        }
        let making = self.along.making.iter().filter(|m| !m.remake).count();
        if self.links.count_mine(&self.me()) + making >= MAX_LINKS {
            self.ui.send(Event::Status(format!("That's {MAX_LINKS} links. Remove one first.")));
            return;
        }
        // Saved at once, so the id is never handed out again.
        let id = self.links.reserve_id();
        self.save_links();
        log::info!("listen along: making link #{id}");
        self.along.making.push(Making { id, name: clean_name(&name, id), remake: false, started: false });
        self.send_links();
        self.start_makes();
    }

    /// A broken link made anew: same id, name and kick, a new playlist (the friend needs the new
    /// link).
    pub(super) fn cmd_listen_remake_link(&mut self, id: u32) {
        if self.library.is_none() {
            self.ui.send(Event::Status("Log in to Spotify first.".into()));
            return;
        }
        if self.links_locked() {
            return;
        }
        let me = self.me();
        let Some(link) = self.links.get(id).filter(|l| l.is_mine(&me) && l.gone) else { return };
        if self.along.making.iter().any(|m| m.id == id) {
            return;
        }
        log::info!("listen along: making link #{id} anew");
        self.along.making.push(Making { id, name: link.name.clone(), remake: true, started: false });
        self.send_links();
        self.start_makes();
    }

    /// A new name, only on this PC: nothing goes to Spotify.
    pub(super) fn cmd_listen_rename_link(&mut self, id: u32, name: String) {
        if self.links_locked() {
            return;
        }
        if let Some(m) = self.along.making.iter_mut().find(|m| m.id == id && !m.remake) {
            m.name = clean_name(&name, id);
            self.send_links();
            return;
        }
        let mine = self.links.get(id).is_some_and(|l| l.is_mine(&self.me()));
        if mine && self.links.rename(id, &name) {
            self.links_changed();
        }
    }

    /// Kick: the link says stopped, once, then nothing more is written to it (until let back in).
    /// The friend's Truffle sees the host stop, as when they quit. Let back in: the link says
    /// what plays at once; same link, nothing to send again.
    pub(super) fn cmd_listen_kick(&mut self, id: u32, kick: bool) {
        if self.links_locked() {
            return;
        }
        let (me, now) = (self.me(), now_ms());
        let Some(link) = self.links.get(id).filter(|l| l.is_mine(&me)) else { return };
        let (uri, name, gone) = (link.uri.clone(), link.name.clone(), link.gone);
        if !self.links.kick(id, kick, &me) {
            return;
        }
        if kick {
            log::info!("listen along: kicked link #{id}");
            if !gone {
                // An ordinary stop (saved as owed until it lands, so it goes even after a restart).
                let text = self.stop_words(now);
                self.along.fanout.push(&uri, text, now);
            }
            self.ui.send(Event::Status(format!("{name} is paused and hears nothing now.")));
        } else {
            log::info!("listen along: let link #{id} back in");
            if !gone {
                // Takes the place of a stop not sent yet.
                let text = if lock(&self.settings).listen_share {
                    self.host_state(now).map(|s| listen::describe(&s))
                } else {
                    Some(listen::off_description(now))
                };
                match text {
                    Some(text) => self.along.fanout.push(&uri, text, now),
                    None => {
                        // A song is loading: nothing to say yet. The stop not sent yet is taken
                        // back, and once the song plays it is shared with every link that hears.
                        self.along.fanout.withdraw(&uri);
                        self.along.shared = None;
                    }
                }
            }
            self.ui.send(Event::Status(format!("{name} can listen again.")));
        }
        self.links_changed();
    }

    /// A link out for good: it says stopped (unless its kick already did), then its playlist
    /// leaves the library (Spotify's delete: 90 days to recover). Never written again, and never
    /// taken in again.
    pub(super) fn cmd_listen_remove_link(&mut self, id: u32) {
        if self.links_locked() {
            return;
        }
        let (me, now) = (self.me(), now_ms());
        // Still being made: it goes, and its playlist with it once made.
        if let Some(at) = self.along.making.iter().position(|m| m.id == id && !m.remake) {
            let m = self.along.making.remove(at);
            log::info!("listen along: removed link #{id} while it was being made");
            self.ui.send(Event::Status(format!("Removed {}'s link.", m.name)));
            self.send_links();
            return;
        }
        let Some(link) = self.links.get(id).filter(|l| l.is_mine(&me)) else { return };
        let name = link.name.clone();
        self.along.making.retain(|m| m.id != id);
        let Some(removed) = self.links.remove(id) else { return };
        log::info!("listen along: removed link #{id}");
        if removed.stop_owed {
            // Its last words first; the playlist goes once they landed.
            let text = self.stop_words(now);
            self.along.fanout.push(&removed.uri, text, now);
        } else if removed.delete_owed {
            // Kicked, and its stop already said: nothing more to say (owed, and saved below).
            self.start_delete(&removed.uri);
        }
        self.ui.send(Event::Status(format!("Removed {name}'s link.")));
        self.links_changed();
    }

    // ------------------------------------------------------------ making

    /// Asks for the playlists of the links being made (not while reconnecting: it would fail for
    /// nothing).
    pub(super) fn start_makes(&mut self) {
        if self.reconnect.is_some() {
            return;
        }
        let Some(library) = self.library.clone() else { return };
        let owner = self.me();
        for m in self.along.making.iter_mut().filter(|m| !m.started) {
            m.started = true;
            let (id, owner, epoch, tx, library) = (m.id, owner.clone(), self.epoch, self.jobs.clone(), library.clone());
            tokio::spawn(async move {
                // Also when it fails or takes too long once its playlist is made: that may be in
                // the library all the same.
                let mut made = None;
                let result = in_time(SLOW_TIMEOUT, library.create_channel(&mut made)).await;
                let left = made.filter(|_| result.is_err());
                let _ = tx.send(Job::Listen { epoch, done: Done::LinkMade { id, owner, result, left } });
            });
        }
    }

    /// A link's playlist made for a session that ended meanwhile (logged out, or the connection
    /// was lost and made anew): it is no link, and nobody has it. Kept as a removed link owing its
    /// stop, so the next login of its account writes that and then takes the playlist out of the
    /// library (at once, if that account is logged in again already).
    pub(super) fn link_orphaned(&mut self, uri: String, owner: String) {
        if self.links.knows(&uri) || owner.is_empty() {
            return;
        }
        log::info!("listen along: a link's playlist was made as the session ended; it goes at the next login");
        self.links.bury(&uri, &owner, true);
        self.save_links();
        if self.library.is_some() && self.me().eq_ignore_ascii_case(&owner) {
            let now = now_ms();
            let text = self.stop_words(now);
            self.along.fanout.push(&uri, text, now);
        }
    }

    /// A link's playlist made, or not (`left`: with an error, the playlist made before it went
    /// wrong: it may be in the library, where nothing would show it).
    pub(super) fn on_link_made(&mut self, id: u32, result: Result<String>, left: Option<String>) {
        let entry = self.along.making.iter().position(|m| m.id == id).map(|at| self.along.making.remove(at));
        let me = self.me();
        let uri = match result {
            Ok(uri) => uri,
            Err(e) => {
                log::warn!("listen along: couldn't make link #{id}: {e}");
                if let Some(uri) = left {
                    // Nobody has it: kept as a removed link, and out of the library if it got in.
                    self.links.bury(&uri, &me, false);
                    self.delete_link_playlist(&uri);
                }
                if entry.is_some() {
                    self.ui.send(Event::Error("Couldn't make the link.".into()));
                    self.send_links();
                }
                return;
            }
        };
        let now = now_ms();
        let placed = match &entry {
            // Only while it is still broken: one found working again meanwhile (the login check)
            // keeps its playlist, the one its friend has.
            Some(m) if m.remake => self.links.get(id).is_some_and(|l| l.gone) && self.links.replace_uri(id, &uri, &me, now),
            Some(m) => {
                self.links.add(id, &m.name, &uri, &me, now);
                true
            }
            None => false,
        };
        if !placed {
            // Removed while being made, or working again: nobody has this playlist, so it goes at
            // once.
            match self.links.get(id).filter(|_| entry.as_ref().is_some_and(|m| m.remake)) {
                Some(l) => {
                    log::info!("listen along: link #{id} works again; the playlist made for it goes");
                    self.ui.send(Event::Status(format!("{}'s link works again.", l.name)));
                }
                None => log::info!("listen along: link #{id} was removed while being made; its playlist goes"),
            }
            self.links.bury(&uri, &me, false);
            self.delete_link_playlist(&uri);
            self.send_links();
            return;
        }
        log::info!("listen along: made link #{id}");
        self.along.made.insert(uri.clone());
        // Its first words at once: a new playlist has no description, and joining it would say
        // it isn't a listen-along link. It shows "Making…" until they land.
        let kicked = self.links.get(id).is_some_and(|l| l.kicked);
        let text = self.words_now(now, kicked);
        self.along.fanout.push(&uri, text, now);
        self.links_changed();
    }

    // ------------------------------------------------------------ writes landing

    /// A write to a link landed (`first`: the first to one made in this session).
    pub(super) fn on_link_written(&mut self, uri: &str, text: &str, first: bool) {
        if is_stop(text) {
            if let Some(l) = self.links.by_uri_mut(uri)
                && l.kicked
                && l.stop_owed
            {
                l.stop_owed = false;
                self.save_links();
            } else if let Some(r) = self.links.removed_mut(uri)
                && r.stop_owed
            {
                // Its last words landed: now its playlist goes (saved there).
                r.stop_owed = false;
                self.delete_link_playlist(uri);
            }
        }
        if first {
            // "Making…" becomes the link to copy.
            self.send_links();
        }
    }

    /// Spotify refused a write for good: the playlist is gone, or isn't this account's.
    pub(super) fn link_refused(&mut self, uri: &str) {
        let me = self.me();
        if let Some(r) = self.links.removed_mut(uri) {
            let unchecked = r.owner.is_empty();
            // A removed link of this account's that can't be written: nothing more can be said to
            // it. One of another account's still owes its stop at that account's next login.
            let cleared = !unchecked && r.owner.eq_ignore_ascii_case(&me) && std::mem::take(&mut r.stop_owed);
            self.along.fanout.forget(uri);
            if cleared {
                self.save_links();
            } else if unchecked {
                // Never checked: maybe another account's. Whose it is decides.
                self.check(vec![uri.to_owned()]);
            }
            return;
        }
        let Some(link) = self.links.by_uri(uri).filter(|l| l.is_mine(&me)) else { return };
        if self.along.made.contains(uri) {
            // Just made: Spotify may not know it everywhere yet. Tried again in a moment.
            return;
        }
        if link.owner.is_empty() {
            // Not checked yet: maybe another account's. Whose it is decides.
            self.check(vec![uri.to_owned()]);
            return;
        }
        self.link_gone(uri);
    }

    /// A link stopped working: never written again until made anew (no new playlist by itself:
    /// that would be a new link the friend doesn't have).
    fn link_gone(&mut self, uri: &str) {
        let Some(link) = self.links.by_uri_mut(uri).filter(|l| !l.gone) else { return };
        link.gone = true;
        link.stop_owed = false;
        let (id, name) = (link.id, link.name.clone());
        log::info!("listen along: link #{id} stopped working");
        self.along.fanout.forget(uri);
        self.ui.send(Event::Status(format!("{name}'s link stopped working.")));
        self.links_changed();
    }

    /// A removed link's playlist leaves the library. Owed (and saved, with the rest of the book)
    /// until Spotify says it is out, so one that fails, or is cut off by quitting, is tried again
    /// at the next login, reconnect and library refresh: Truffle hides it, so nothing else would
    /// ever show it.
    fn delete_link_playlist(&mut self, uri: &str) {
        if let Some(r) = self.links.removed_mut(uri) {
            r.delete_owed = true;
        }
        self.save_links();
        self.start_delete(uri);
    }

    /// Asks Spotify to take a playlist out of the library, unless that is on its way already
    /// (not while reconnecting: it would fail for nothing, and is tried again once reconnected).
    fn start_delete(&mut self, uri: &str) {
        if self.reconnect.is_some() || self.along.deleting.contains_key(uri) {
            return;
        }
        let Some(library) = self.library.clone() else { return };
        self.along.deleting.insert(uri.to_owned(), self.along.reconnects);
        let (uri, epoch, tx) = (uri.to_owned(), self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let result = in_time(SLOW_TIMEOUT, library.remove_channel(&uri)).await;
            let _ = tx.send(Job::Listen { epoch, done: Done::LinkDeleted { uri, result } });
        });
    }

    /// The playlists this account's removed links still owe out of the library, asked for again.
    pub(super) fn retry_deletes(&mut self) {
        for uri in self.links.owed_deletes(&self.me()) {
            self.start_delete(&uri);
        }
    }

    /// After a library refresh: a removed link's playlist still in it goes (a delete that seemed
    /// to fail but didn't, or an add Spotify took after saying it hadn't), and the deletes owed
    /// are asked for again.
    pub(super) fn delete_listed_links(&mut self) {
        let listed: Vec<String> = self
            .playlists
            .iter()
            .filter(|p| p.owned && self.links.removed.iter().any(|r| r.uri == p.uri && !r.stop_owed && !r.delete_owed))
            .map(|p| p.uri.clone())
            .collect();
        if !listed.is_empty() {
            log::info!("listen along: {} removed links' playlists are still in the library", listed.len());
            for uri in &listed {
                if let Some(r) = self.links.removed_mut(uri) {
                    r.delete_owed = true;
                }
            }
            self.save_links();
        }
        self.retry_deletes();
    }

    pub(super) fn on_link_deleted(&mut self, uri: &str, result: Result<()>) {
        let asked = self.along.deleting.remove(uri);
        match result {
            Ok(()) => {
                log::info!("listen along: {}'s playlist left the library", self.link_label(uri));
                self.link_deleted(uri);
            }
            Err(e) => {
                // Asked for before a reconnect, and still on its way when that asked for the owed
                // ones again (so it was skipped there): asked for again now, on the new
                // connection. Otherwise at the next login, reconnect or library refresh.
                let again = self.reconnect.is_none()
                    && asked.is_some_and(|n| n != self.along.reconnects)
                    && self.links.owed_deletes(&self.me()).iter().any(|u| u == uri);
                let when = if again { "now" } else { "later" };
                log::warn!("listen along: couldn't take {}'s playlist out of the library (tried again {when}): {e}", self.link_label(uri));
                if again {
                    self.start_delete(uri);
                }
            }
        }
    }

    /// A removed link's playlist is out of the library (also for a session that ended
    /// meanwhile): nothing more is owed to it.
    pub(super) fn link_deleted(&mut self, uri: &str) {
        if let Some(r) = self.links.removed_mut(uri)
            && std::mem::take(&mut r.delete_owed)
        {
            self.save_links();
        }
    }

    // ------------------------------------------------------------ whose links are

    /// Reads whose the links not checked yet are, and whether broken ones are back.
    pub(super) fn check_links(&mut self) {
        let me = self.me();
        let uris = self.links.mine(&me).filter(|l| l.owner.is_empty() || l.gone).map(|l| l.uri.clone()).collect();
        self.check(uris);
    }

    fn check(&mut self, uris: Vec<String>) {
        let Some(library) = self.library.clone() else { return };
        let uris: Vec<String> = uris.into_iter().filter(|u| self.along.checking.insert(u.clone())).collect();
        if uris.is_empty() {
            return;
        }
        let (epoch, tx) = (self.epoch, self.jobs.clone());
        tokio::spawn(async move {
            let mut results = Vec::with_capacity(uris.len());
            for uri in uris {
                let owner = in_time(READ_TIMEOUT, async { Ok(library.channel(&uri).await?.owner) }).await;
                results.push((uri, owner));
            }
            let _ = tx.send(Job::Listen { epoch, done: Done::LinksChecked(results) });
        });
    }

    pub(super) fn on_links_checked(&mut self, results: Vec<(String, Result<String>)>) {
        let (me, now) = (self.me(), now_ms());
        let mut changed = false;
        let mut gone = Vec::new();
        // Links back from "gone" (and whether kicked), and removed ones of this account's that
        // owe a stop.
        let mut back = Vec::new();
        let mut stops = Vec::new();
        for (uri, result) in results {
            self.along.checking.remove(&uri);
            if let Some(r) = self.links.removed_mut(&uri) {
                // A removed link refused before its owner was known.
                match result {
                    Ok(owner) if !owner.is_empty() => {
                        let mine = owner.eq_ignore_ascii_case(&me);
                        r.owner = owner;
                        changed = true;
                        // This account's after all: said once more (a second refusal gives up).
                        if mine && r.stop_owed {
                            stops.push(uri);
                        }
                    }
                    Ok(_) => {}
                    // Gone: nothing more can be said to it.
                    Err(e) if crate::library::is_permanent(&e) => changed |= std::mem::take(&mut r.stop_owed),
                    Err(e) => log::info!("listen along: couldn't check a removed link: {e}"),
                }
                continue;
            }
            let Some(link) = self.links.by_uri_mut(&uri) else { continue };
            match result {
                Ok(owner) if !owner.is_empty() => {
                    if link.owner != owner {
                        link.owner = owner;
                        changed = true;
                    }
                    // Readable and this account's again (put back on spotify.com): a kicked one
                    // owes its stop again (the one before may never have landed), and a live one
                    // hears what plays now.
                    if link.gone && link.owner.eq_ignore_ascii_case(&me) {
                        link.gone = false;
                        link.stop_owed = link.kicked;
                        log::info!("listen along: link #{} works again", link.id);
                        back.push((uri, link.kicked));
                        changed = true;
                    }
                }
                Ok(_) => {}
                Err(e) if crate::library::is_permanent(&e) => gone.push(uri),
                Err(e) => log::info!("listen along: couldn't check link #{}: {e}", link.id),
            }
        }
        if changed {
            self.links_changed();
        }
        for (uri, kicked) in back {
            let text = self.words_now(now, kicked);
            self.along.fanout.push(&uri, text, now);
        }
        for uri in stops {
            let text = self.stop_words(now);
            self.along.fanout.push(&uri, text, now);
        }
        for uri in gone {
            self.link_gone(&uri);
        }
    }
}
