//! Listen along: the state a host shares and how a listener follows it. No network and no clock
//! of its own (times are passed in, as milliseconds since 1970), so it is tested with a fake one;
//! `controller/listen.rs` does the asking, the writing and the playing.
//!
//! Two ways to follow someone:
//! - Spotify's friend activity, for a host on Spotify's own app: the song and when it started.
//! - A Truffle host's channel: a playlist of theirs with no songs whose description carries the
//!   state, `Listen along with me on Truffle. [tl1;<track uri>;<position ms>;<sent at>;<mode>]`,
//!   rewritten on every song change, pause, resume, seek and stop. Every friend has a channel of
//!   their own (a link, see `links`), so one can be kicked without the others noticing.

pub mod links;

use serde_json::Value;

/// The channel playlist's name.
pub const CHANNEL_NAME: &str = "Truffle · listen along";
/// A listener reads a channel this often, and friend activity this often.
pub const CHANNEL_POLL_MS: u64 = 3_000;
pub const FRIEND_POLL_MS: u64 = 5_000;
/// A host writes at most this often (the newest state wins).
pub const WRITE_EVERY_MS: i64 = 1_000;
/// After a failed write the next try waits this long.
const WRITE_RETRY_MS: i64 = 5_000;
/// A listener this far from the host seeks.
pub const SEEK_OFF_MS: u32 = 2_500;
/// Once the host's song starts playing here, this far off is worth a seek (a YouTube song takes a
/// few seconds to fetch).
pub const START_OFF_MS: u32 = 2_000;
/// A host shares a new state when its position moved this far from what the last one says.
const SHARE_OFF_MS: u32 = 1_500;
/// A host with no news this long (after their song should have ended) is taken as gone.
const SILENT_MS: i64 = 10 * 60_000;
/// A host unreadable this many times in a row is taken as gone.
const MAX_FAILURES: u32 = 5;
/// Friend activity without a known song length counts as "playing now" this long.
const LIVE_WITHOUT_LENGTH_MS: i64 = 10 * 60_000;
/// A host's clock is believed to run ahead of the listener's by at most this much (more is a
/// broken description).
const MAX_AHEAD_MS: i64 = 10 * 60_000;

const TAG: &str = "[tl1;";
const LEAD: &str = "Listen along with me on Truffle.";
const OFF_LEAD: &str = "Not sharing right now.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Play,
    Pause,
    Stop,
}

impl Mode {
    fn word(self) -> &'static str {
        match self {
            Mode::Play => "play",
            Mode::Pause => "pause",
            Mode::Stop => "stop",
        }
    }
}

/// What a host plays: the song, where it was at `sent_at_ms` (the host's clock), and whether it
/// plays. A stopped host has no song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostState {
    pub track: String,
    pub position_ms: u32,
    pub sent_at_ms: i64,
    pub mode: Mode,
}

impl HostState {
    pub fn stopped(now_ms: i64) -> Self {
        Self { track: String::new(), position_ms: 0, sent_at_ms: now_ms, mode: Mode::Stop }
    }

    /// Where the host's song is at `now_ms`: it moves on while playing (never back, if the clocks
    /// differ a little).
    pub fn position_at(&self, now_ms: i64) -> u32 {
        match self.mode {
            Mode::Play => {
                let gone = now_ms.saturating_sub(self.sent_at_ms).max(0);
                i64::from(self.position_ms).saturating_add(gone).min(i64::from(u32::MAX)) as u32
            }
            Mode::Pause | Mode::Stop => self.position_ms,
        }
    }
}

/// The channel's description for `state`.
pub fn describe(state: &HostState) -> String {
    with_lead(LEAD, state)
}

/// The description a channel gets when its owner stops sharing.
pub fn off_description(now_ms: i64) -> String {
    with_lead(OFF_LEAD, &HostState::stopped(now_ms))
}

fn with_lead(lead: &str, state: &HostState) -> String {
    format!("{lead} {TAG}{};{};{};{}]", state.track, state.position_ms, state.sent_at_ms, state.mode.word())
}

/// The state in a channel's description, if it has one (Spotify may have escaped or trimmed the
/// words around it).
pub fn parse(description: &str) -> Option<HostState> {
    let start = description.find(TAG)? + TAG.len();
    let rest = &description[start..];
    let body = &rest[..rest.find(']')?];
    let mut parts = body.split(';');
    let track = parts.next()?.trim();
    let position_ms = parts.next()?.trim().parse().ok()?;
    let sent_at_ms = parts.next()?.trim().parse().ok()?;
    let mode = match parts.next()?.trim() {
        "play" => Mode::Play,
        "pause" => Mode::Pause,
        "stop" => Mode::Stop,
        _ => return None,
    };
    if parts.next().is_some() || (!track.is_empty() && !track.starts_with("spotify:")) {
        return None;
    }
    if track.is_empty() && mode != Mode::Stop {
        return None;
    }
    Some(HostState { track: track.to_owned(), position_ms, sent_at_ms, mode })
}

/// Whether a description is a listen-along channel's.
pub fn is_channel(description: &str) -> bool {
    description.contains(TAG)
}

/// Whether `next` tells listeners something the state shared last doesn't: another song, a pause or
/// resume, or a jump (a seek).
pub fn worth_sharing(last: Option<&HostState>, next: &HostState, now_ms: i64) -> bool {
    let Some(last) = last else { return true };
    if last.track != next.track || last.mode != next.mode {
        return true;
    }
    match next.mode {
        Mode::Play | Mode::Pause => last.position_at(now_ms).abs_diff(next.position_ms) > SHARE_OFF_MS,
        Mode::Stop => false,
    }
}

/// The channel playlist in a pasted link or uri (`https://open.spotify.com/playlist/<id>?si=…`,
/// `spotify:playlist:<id>`), as `spotify:playlist:<id>`.
pub fn channel_uri(text: &str) -> Option<String> {
    let text = text.trim();
    let after = if let Some(i) = text.find("playlist/") {
        &text[i + "playlist/".len()..]
    } else {
        &text[text.find("playlist:")? + "playlist:".len()..]
    };
    let id: String = after.chars().take_while(char::is_ascii_alphanumeric).collect();
    (id.len() == 22).then(|| format!("spotify:playlist:{id}"))
}

/// The link a host sends to friends.
pub fn channel_link(uri: &str) -> Option<String> {
    let id = uri.strip_prefix("spotify:playlist:")?;
    (!id.is_empty()).then(|| format!("https://open.spotify.com/playlist/{id}"))
}

// ------------------------------------------------------------------------------------ friend activity

/// What Spotify's friend activity says about one person: their last song and when it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub username: String,
    pub name: String,
    pub image: Option<String>,
    pub track: String,
    pub track_name: String,
    pub artist: String,
    pub started_ms: i64,
}

impl Activity {
    /// As a host state: their song, playing from its start.
    pub fn state(&self) -> HostState {
        HostState { track: self.track.clone(), position_ms: 0, sent_at_ms: self.started_ms, mode: Mode::Play }
    }
}

/// Everyone in a buddylist answer (`{"friends": [...]}`) who played something.
pub fn parse_buddylist(json: &Value) -> Vec<Activity> {
    json.get("friends").and_then(Value::as_array).map(|list| list.iter().filter_map(parse_activity).collect()).unwrap_or_default()
}

/// One person's activity (`{"timestamp", "user", "track"}`).
pub fn parse_activity(json: &Value) -> Option<Activity> {
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("").to_owned();
    let user = json.get("user")?;
    let username = text(user, "uri").strip_prefix("spotify:user:")?.to_owned();
    let track = json.get("track")?;
    let uri = text(track, "uri");
    if username.is_empty() || !uri.starts_with("spotify:") {
        return None;
    }
    let name = Some(text(user, "name")).filter(|n| !n.is_empty()).unwrap_or_else(|| username.clone());
    let image = Some(text(user, "imageUrl")).filter(|i| !i.is_empty());
    let artist = track.get("artist").map(|a| text(a, "name")).unwrap_or_default();
    let started_ms = json.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
    Some(Activity { username, name, image, track: uri, track_name: text(track, "name"), artist, started_ms })
}

/// Whether a song that started at `started_ms` still plays at `now_ms` (`duration_ms`: its
/// length, when known).
pub fn playing_now(started_ms: i64, duration_ms: Option<u32>, now_ms: i64) -> bool {
    if started_ms <= 0 || started_ms > now_ms + 60_000 {
        return false;
    }
    match duration_ms.filter(|&d| d > 0) {
        Some(d) => now_ms < started_ms + i64::from(d) + 30_000,
        None => now_ms - started_ms < LIVE_WITHOUT_LENGTH_MS,
    }
}

// ------------------------------------------------------------------------------------ host side

/// The host's writes: at most one on its way, at most one a second, always the newest state.
#[derive(Debug, Default)]
pub struct Outbox {
    /// What Spotify took last.
    sent: Option<String>,
    /// The newest text not sent yet.
    pending: Option<String>,
    busy: bool,
    /// The write on its way was taken back: if it fails, it isn't tried again.
    withdrawn: bool,
    /// No write before this.
    next_ms: i64,
}

impl Outbox {
    pub fn push(&mut self, text: String) {
        if !self.busy && self.pending.is_none() && self.sent.as_deref() == Some(text.as_str()) {
            return;
        }
        self.pending = Some(text);
    }

    /// Whether there is a text to write and it is time ([`take`](Self::take) would give it).
    pub fn due(&self, now_ms: i64) -> bool {
        self.pending.is_some() && !self.waiting(now_ms)
    }

    /// A write is on its way, or the next one must wait a little.
    fn waiting(&self, now_ms: i64) -> bool {
        // A wait longer than any this sets means the clock was set back.
        self.busy || (now_ms < self.next_ms && self.next_ms - now_ms <= WRITE_RETRY_MS)
    }

    /// The text to write now, if it is time (the write is then on its way until `done`).
    pub fn take(&mut self, now_ms: i64) -> Option<String> {
        if self.waiting(now_ms) {
            return None;
        }
        let text = self.pending.take()?;
        self.busy = true;
        self.next_ms = now_ms + WRITE_EVERY_MS;
        Some(text)
    }

    /// A write finished. A failed one is tried again later, unless a newer text came meanwhile
    /// (or it was taken back).
    pub fn done(&mut self, text: String, ok: bool, now_ms: i64) {
        self.busy = false;
        let withdrawn = std::mem::take(&mut self.withdrawn);
        if ok {
            self.sent = Some(text);
        } else {
            if self.pending.is_none() && !withdrawn {
                self.pending = Some(text);
            }
            self.next_ms = now_ms + WRITE_RETRY_MS;
        }
    }

    /// Takes back the text not sent yet, and the one on its way if it fails: nothing more is
    /// written until the next [`push`](Self::push).
    pub fn withdraw(&mut self) {
        self.pending = None;
        self.withdrawn = self.busy;
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn pending(&self) -> Option<&str> {
        self.pending.as_deref()
    }

    pub fn sent(&self) -> Option<&str> {
        self.sent.as_deref()
    }
}

// ------------------------------------------------------------------------------------ listener side

/// What plays here, for [`Follow`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Here<'a> {
    pub track: Option<&'a str>,
    pub position_ms: u32,
    /// The song's length (0: unknown).
    pub duration_ms: u32,
    pub playing: bool,
    pub loading: bool,
}

/// Why following ended by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    /// The host stopped.
    Stopped,
    /// No news for a long time.
    Silent,
    /// The host couldn't be read several times in a row.
    Unreachable,
}

/// What the listener's player should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Wait,
    /// Find this song and play it from where the host is (see [`Follow::target`]).
    Load(String),
    Seek(u32),
    /// Pause, at this position.
    Pause(u32),
    /// Play on, from this position.
    Resume(u32),
    End(Gone),
}

/// Following one host: the newest state, and how it relates to what plays here.
#[derive(Debug, Clone)]
pub struct Follow {
    last: Option<HostState>,
    /// When the state last changed (the listener's clock).
    changed_ms: i64,
    failures: u32,
    /// The song being looked up and loaded for the host, until it plays.
    loading: Option<String>,
    /// The state under which the host's song ended here (it isn't started again for it).
    finished: Option<HostState>,
    /// How far the host's clock runs ahead of this one, as far as seen.
    ahead_ms: i64,
}

impl Follow {
    pub fn new(now_ms: i64) -> Self {
        Self { last: None, changed_ms: now_ms, failures: 0, loading: None, finished: None, ahead_ms: 0 }
    }

    pub fn last(&self) -> Option<&HostState> {
        self.last.as_ref()
    }

    /// Where the host's song is at `now_ms`, by the newest state.
    pub fn position(&self, now_ms: i64) -> Option<u32> {
        self.last.as_ref().map(|s| self.host_at(s, now_ms))
    }

    /// Where `s` puts the host's song at `now_ms` (this clock), allowing for theirs running ahead.
    fn host_at(&self, s: &HostState, now_ms: i64) -> u32 {
        s.position_at(now_ms.saturating_add(self.ahead_ms))
    }

    /// A read of the host came back (`None`: it had no state in it).
    pub fn on_state(&mut self, state: Option<HostState>, here: &Here, now_ms: i64) -> Step {
        let Some(state) = state else { return self.on_failure() };
        self.failures = 0;
        if self.last.as_ref() != Some(&state) {
            // A state sent "after now" shows the host's clock runs ahead: without allowing for it
            // the song here would be pulled back to where the state was sent, read after read.
            let ahead = state.sent_at_ms.saturating_sub(now_ms);
            if ahead <= MAX_AHEAD_MS {
                self.ahead_ms = self.ahead_ms.max(ahead);
            }
            self.changed_ms = now_ms;
            self.last = Some(state);
        }
        self.step(here, now_ms)
    }

    /// The host couldn't be read.
    pub fn on_failure(&mut self) -> Step {
        self.failures += 1;
        if self.failures >= MAX_FAILURES { Step::End(Gone::Unreachable) } else { Step::Wait }
    }

    /// The song to load for the host and where, if `track` is still what they play.
    pub fn target(&self, track: &str, now_ms: i64) -> Option<(u32, bool)> {
        let s = self.last.as_ref().filter(|s| s.track == track && s.mode != Mode::Stop)?;
        Some((self.host_at(s, now_ms), s.mode == Mode::Play))
    }

    /// Looking up or loading the host's song failed: the next state tries again.
    pub fn load_failed(&mut self) {
        self.loading = None;
    }

    /// The host's song ended here (or couldn't play): wait for their next one instead of
    /// starting it again.
    pub fn on_ended(&mut self) {
        self.loading = None;
        self.finished = self.last.clone();
    }

    /// The host's song started playing here: check where it is, more finely than usual.
    pub fn on_playing(&mut self, here: &Here, now_ms: i64) -> Step {
        let Some(s) = self.last.as_ref() else { return Step::Wait };
        if here.track != Some(s.track.as_str()) {
            return Step::Wait;
        }
        self.loading = None;
        match s.mode {
            Mode::Play => {
                let want = self.host_at(s, now_ms);
                if here.duration_ms > 0 && want >= here.duration_ms {
                    Step::Wait
                } else if here.position_ms.abs_diff(want) > START_OFF_MS {
                    Step::Seek(want)
                } else {
                    Step::Wait
                }
            }
            Mode::Pause => Step::Pause(s.position_ms),
            Mode::Stop => Step::End(Gone::Stopped),
        }
    }

    /// What to do now, from the newest state and what plays here.
    pub fn step(&mut self, here: &Here, now_ms: i64) -> Step {
        let Some(s) = self.last.clone() else { return Step::Wait };
        if s.mode == Mode::Stop {
            return Step::End(Gone::Stopped);
        }
        let same = here.track == Some(s.track.as_str());
        // A playing host whose song should have ended long ago without news is gone.
        if s.mode == Mode::Play {
            let left = if same && here.duration_ms > 0 { i64::from(here.duration_ms.saturating_sub(self.host_at(&s, self.changed_ms))) } else { 0 };
            if now_ms - self.changed_ms > SILENT_MS + left {
                return Step::End(Gone::Silent);
            }
        }
        if !same {
            if self.loading.as_deref() == Some(s.track.as_str()) || self.finished.as_ref() == Some(&s) {
                return Step::Wait;
            }
            self.loading = Some(s.track.clone());
            return Step::Load(s.track);
        }
        if here.loading || self.finished.as_ref() == Some(&s) {
            return Step::Wait;
        }
        match s.mode {
            Mode::Pause if here.playing => Step::Pause(s.position_ms),
            Mode::Pause if here.position_ms.abs_diff(s.position_ms) > SEEK_OFF_MS => Step::Seek(s.position_ms),
            Mode::Pause => Step::Wait,
            _ => {
                let want = self.host_at(&s, now_ms);
                if here.duration_ms > 0 && want >= here.duration_ms {
                    // Their song is over here too: wait for their next one.
                    Step::Wait
                } else if !here.playing {
                    Step::Resume(want)
                } else if here.position_ms.abs_diff(want) > SEEK_OFF_MS {
                    Step::Seek(want)
                } else {
                    Step::Wait
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";

    fn state(position_ms: u32, sent_at_ms: i64, mode: Mode) -> HostState {
        HostState { track: T.to_owned(), position_ms, sent_at_ms, mode }
    }

    #[test]
    fn descriptions_round_trip() {
        for mode in [Mode::Play, Mode::Pause] {
            let s = state(83_250, 1_761_400_000_123, mode);
            let text = describe(&s);
            assert!(text.starts_with("Listen along with me on Truffle. [tl1;"), "{text}");
            assert_eq!(parse(&text), Some(s));
        }
        let off = off_description(42);
        assert_eq!(off, "Not sharing right now. [tl1;;0;42;stop]");
        assert_eq!(parse(&off), Some(HostState::stopped(42)));
        // A stop with the song it stopped on is kept too.
        let s = state(5, 6, Mode::Stop);
        assert_eq!(parse(&describe(&s)), Some(s));
    }

    #[test]
    fn descriptions_survive_words_around_them() {
        let text = format!("Hi! &amp; more words <b>[tl1;{T};1000;2000;pause]</b> trailing");
        assert_eq!(parse(&text), Some(state(1000, 2000, Mode::Pause)));
        assert!(is_channel(&text));
        assert!(!is_channel("Just a playlist"));
    }

    #[test]
    fn bad_descriptions_are_not_states() {
        for bad in [
            "",
            "no tag here",
            "[tl1;",
            "[tl1;spotify:track:x;1;2]",
            "[tl1;spotify:track:x;1;2;dance]",
            "[tl1;spotify:track:x;-1;2;play]",
            "[tl1;spotify:track:x;one;2;play]",
            "[tl1;spotify:track:x;1;2;play;extra]",
            "[tl1;;0;2;play]",
            "[tl1;notaspotifyuri;0;2;play]",
            "[tl2;spotify:track:x;1;2;play]",
            "[tl1;spotify:track:x;1;2;play",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_position_moves_on_only_while_playing() {
        let s = state(10_000, 1_000_000, Mode::Play);
        assert_eq!(s.position_at(1_000_000), 10_000);
        assert_eq!(s.position_at(1_004_500), 14_500);
        // A clock a little behind the host's never goes back.
        assert_eq!(s.position_at(999_000), 10_000);
        let p = state(10_000, 1_000_000, Mode::Pause);
        assert_eq!(p.position_at(2_000_000), 10_000);
        assert_eq!(HostState::stopped(5).position_at(99), 0);
    }

    #[test]
    fn only_news_is_shared() {
        let now = 1_000_000;
        let last = state(10_000, now, Mode::Play);
        assert!(worth_sharing(None, &last, now));
        // Playing on as expected: nothing new.
        assert!(!worth_sharing(Some(&last), &state(15_000, now + 5_000, Mode::Play), now + 5_000));
        assert!(!worth_sharing(Some(&last), &state(16_400, now + 5_000, Mode::Play), now + 5_000));
        // A seek, a pause, another song.
        assert!(worth_sharing(Some(&last), &state(60_000, now + 5_000, Mode::Play), now + 5_000));
        assert!(worth_sharing(Some(&last), &state(15_000, now + 5_000, Mode::Pause), now + 5_000));
        let other = HostState { track: "spotify:track:other".into(), ..last.clone() };
        assert!(worth_sharing(Some(&last), &other, now));
        // A seek while paused.
        let paused = state(10_000, now, Mode::Pause);
        assert!(!worth_sharing(Some(&paused), &state(10_000, now + 60_000, Mode::Pause), now + 60_000));
        assert!(worth_sharing(Some(&paused), &state(40_000, now + 60_000, Mode::Pause), now + 60_000));
        // Stopped twice is nothing new.
        assert!(!worth_sharing(Some(&HostState::stopped(1)), &HostState::stopped(2), 3));
    }

    #[test]
    fn links_and_uris_give_the_channel() {
        let id = "37i9dQZF1DXcBWIGoYBM5M";
        let want = Some(format!("spotify:playlist:{id}"));
        assert_eq!(channel_uri(&format!("https://open.spotify.com/playlist/{id}?si=abc123")), want);
        assert_eq!(channel_uri(&format!("  open.spotify.com/intl-de/playlist/{id}  ")), want);
        assert_eq!(channel_uri(&format!("spotify:playlist:{id}")), want);
        assert_eq!(channel_uri(&format!("spotify:user:me:playlist:{id}")), want);
        assert_eq!(channel_uri("https://open.spotify.com/playlist/short"), None);
        assert_eq!(channel_uri("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"), None);
        assert_eq!(channel_uri(""), None);
        assert_eq!(channel_link(&format!("spotify:playlist:{id}")), Some(format!("https://open.spotify.com/playlist/{id}")));
        assert_eq!(channel_link("spotify:album:x"), None);
    }

    #[test]
    fn friend_activity_is_read() {
        let json: Value = serde_json::from_str(
            r#"{"friends":[
                {"timestamp":1761400000000,"user":{"uri":"spotify:user:robin","name":"Robin","imageUrl":"https://i.scdn.co/a"},
                 "track":{"uri":"spotify:track:4uLU6hMCjMI75M1A2tKUQC","name":"For You","imageUrl":"x",
                          "album":{"name":"Who Really Cares","uri":"spotify:album:a"},"artist":{"name":"TV Girl","uri":"spotify:artist:b"},
                          "context":{"uri":"spotify:playlist:c","name":"care","index":0}}},
                {"timestamp":1,"user":{"uri":"spotify:user:quiet"}},
                {"timestamp":2,"user":{"uri":"spotify:user:noname"},"track":{"uri":"spotify:track:x"}}
            ]}"#,
        )
        .unwrap();
        let list = parse_buddylist(&json);
        assert_eq!(list.len(), 2);
        let a = &list[0];
        assert_eq!((a.username.as_str(), a.name.as_str(), a.image.as_deref()), ("robin", "Robin", Some("https://i.scdn.co/a")));
        assert_eq!((a.track_name.as_str(), a.artist.as_str(), a.started_ms), ("For You", "TV Girl", 1_761_400_000_000));
        assert_eq!(a.state(), HostState { track: T.into(), position_ms: 0, sent_at_ms: 1_761_400_000_000, mode: Mode::Play });
        // No name: the username stands in.
        assert_eq!(list[1].name, "noname");
        assert!(parse_buddylist(&serde_json::json!({"nope": 1})).is_empty());
    }

    #[test]
    fn playing_now_follows_the_song_length() {
        let start = 1_000_000_000;
        assert!(playing_now(start, Some(200_000), start + 100_000));
        assert!(playing_now(start, Some(200_000), start + 220_000), "a little grace");
        assert!(!playing_now(start, Some(200_000), start + 240_000));
        assert!(playing_now(start, None, start + 9 * 60_000));
        assert!(!playing_now(start, None, start + 11 * 60_000));
        assert!(!playing_now(0, None, start));
    }

    #[test]
    fn writes_are_one_a_second_and_the_newest_wins() {
        let mut out = Outbox::default();
        out.push("a".into());
        assert_eq!(out.take(0).as_deref(), Some("a"));
        // While one is on its way, newer ones wait; only the newest is kept.
        out.push("b".into());
        out.push("c".into());
        assert_eq!(out.take(10), None);
        out.done("a".into(), true, 300);
        assert_eq!(out.take(500), None, "not twice within a second");
        assert_eq!(out.take(1_000).as_deref(), Some("c"));
        out.done("c".into(), true, 1_200);
        // The same text again is not written again.
        out.push("c".into());
        assert_eq!(out.take(5_000), None);
        assert_eq!(out.sent(), Some("c"));
    }

    #[test]
    fn a_failed_write_is_tried_again_later_unless_overtaken() {
        let mut out = Outbox::default();
        out.push("a".into());
        let a = out.take(0).unwrap();
        out.done(a, false, 100);
        assert_eq!(out.take(1_100), None, "waits after a failure");
        assert_eq!(out.take(5_100).as_deref(), Some("a"));
        out.push("b".into());
        out.done("a".into(), false, 5_200);
        assert_eq!(out.take(20_000).as_deref(), Some("b"), "the newer text replaces the failed one");
        assert!(out.busy());
    }

    fn here(track: Option<&str>, position_ms: u32, playing: bool) -> Here<'_> {
        Here { track, position_ms, duration_ms: 200_000, playing, loading: false }
    }

    #[test]
    fn following_loads_the_hosts_song_once_then_keeps_in_step() {
        let mut clock = 1_000_000i64;
        let mut f = Follow::new(clock);
        assert_eq!(f.step(&here(None, 0, false), clock), Step::Wait);
        let s = state(30_000, clock, Mode::Play);
        assert_eq!(f.on_state(Some(s.clone()), &here(Some("spotify:track:mine"), 5_000, false), clock), Step::Load(T.into()));
        // The lookup is still on its way: no second load.
        clock += 3_000;
        assert_eq!(f.on_state(Some(s.clone()), &here(Some("spotify:track:mine"), 5_000, false), clock), Step::Wait);
        assert_eq!(f.target(T, clock), Some((33_000, true)));
        assert_eq!(f.target("spotify:track:other", clock), None);
        // It plays here, 1 s behind: close enough once started.
        let s_here = here(Some(T), 32_000, true);
        assert_eq!(f.on_playing(&s_here, clock), Step::Wait);
        // 2.2 s behind at the start is worth a seek (YouTube took a while).
        assert_eq!(f.on_playing(&here(Some(T), 30_800, true), clock), Step::Seek(33_000));
        // Later, only more than 2.5 s off.
        clock += 3_000;
        assert_eq!(f.on_state(Some(s.clone()), &here(Some(T), 34_000, true), clock), Step::Wait);
        assert_eq!(f.on_state(Some(s.clone()), &here(Some(T), 30_000, true), clock), Step::Seek(36_000));
    }

    #[test]
    fn following_pauses_resumes_and_seeks_with_the_host() {
        let mut clock = 5_000_000i64;
        let mut f = Follow::new(clock);
        f.on_state(Some(state(10_000, clock, Mode::Play)), &here(None, 0, false), clock);
        clock += 3_000;
        // The host paused at 12 s.
        let paused = state(12_000, clock, Mode::Pause);
        assert_eq!(f.on_state(Some(paused.clone()), &here(Some(T), 13_000, true), clock), Step::Pause(12_000));
        assert_eq!(f.on_state(Some(paused.clone()), &here(Some(T), 12_100, false), clock + 3_000), Step::Wait);
        // Sought while paused.
        let moved = state(90_000, clock, Mode::Pause);
        assert_eq!(f.on_state(Some(moved), &here(Some(T), 12_100, false), clock + 3_000), Step::Seek(90_000));
        // Playing again from 90 s.
        clock += 10_000;
        let again = state(90_000, clock, Mode::Play);
        assert_eq!(f.on_state(Some(again), &here(Some(T), 90_000, false), clock + 1_000), Step::Resume(91_000));
    }

    #[test]
    fn the_end_of_the_hosts_song_is_waited_out() {
        let mut clock = 9_000_000i64;
        let mut f = Follow::new(clock);
        let s = state(190_000, clock, Mode::Play);
        f.on_state(Some(s.clone()), &here(None, 0, false), clock);
        clock += 12_000;
        // Past its end and stopped here: no restart, no skip; wait for their next song.
        assert_eq!(f.on_state(Some(s.clone()), &here(Some(T), 200_000, false), clock), Step::Wait);
        // Ended here a little early (a shorter YouTube video): not started again for the same state.
        let mut g = Follow::new(clock);
        let s2 = state(150_000, clock, Mode::Play);
        g.on_state(Some(s2.clone()), &here(Some(T), 150_000, true), clock);
        g.on_ended();
        assert_eq!(g.on_state(Some(s2.clone()), &here(Some(T), 190_000, false), clock + 40_000), Step::Wait);
        // Their next song is loaded.
        let next = HostState { track: "spotify:track:next".into(), position_ms: 0, sent_at_ms: clock + 41_000, mode: Mode::Play };
        assert_eq!(g.on_state(Some(next), &here(Some(T), 190_000, false), clock + 42_000), Step::Load("spotify:track:next".into()));
    }

    #[test]
    fn following_ends_when_the_host_is_gone() {
        let clock = 1_000i64;
        // A stop.
        let mut f = Follow::new(clock);
        assert_eq!(f.on_state(Some(HostState::stopped(clock)), &here(None, 0, false), clock), Step::End(Gone::Stopped));
        // Unreadable five times in a row (a good read in between starts the count again).
        let mut f = Follow::new(clock);
        for _ in 0..3 {
            assert_eq!(f.on_failure(), Step::Wait);
        }
        f.on_state(Some(state(0, clock, Mode::Play)), &here(Some(T), 0, true), clock);
        for _ in 0..4 {
            assert_eq!(f.on_state(None, &here(Some(T), 0, true), clock), Step::Wait);
        }
        assert_eq!(f.on_failure(), Step::End(Gone::Unreachable));
        // No news for 10 minutes after their song should have ended.
        let mut f = Follow::new(clock);
        let s = state(0, clock, Mode::Play);
        f.on_state(Some(s.clone()), &here(Some(T), 0, true), clock);
        let end = clock + 200_000;
        assert_eq!(f.on_state(Some(s.clone()), &here(Some(T), 200_000, false), end + 9 * 60_000), Step::Wait);
        assert_eq!(f.on_state(Some(s.clone()), &here(Some(T), 200_000, false), end + 10 * 60_000 + 1_000), Step::End(Gone::Silent));
        // A paused host is never "gone" for being quiet.
        let mut f = Follow::new(clock);
        let p = state(5_000, clock, Mode::Pause);
        f.on_state(Some(p.clone()), &here(Some(T), 5_000, false), clock);
        assert_eq!(f.on_state(Some(p), &here(Some(T), 5_000, false), clock + 3_600_000), Step::Wait);
    }

    #[test]
    fn a_failed_load_is_tried_again_with_the_next_state() {
        let clock = 0i64;
        let mut f = Follow::new(clock);
        let s = state(0, clock, Mode::Play);
        assert_eq!(f.on_state(Some(s.clone()), &here(None, 0, false), clock), Step::Load(T.into()));
        f.load_failed();
        assert_eq!(f.on_state(Some(s), &here(None, 0, false), clock + 3_000), Step::Load(T.into()));
    }

    #[test]
    fn a_host_clock_running_ahead_doesnt_pull_the_listener_back() {
        let mut clock = 1_000_000i64;
        let mut f = Follow::new(clock);
        // The host's clock is 30 s ahead: their state seems sent in the future.
        let s = state(60_000, clock + 30_000, Mode::Play);
        assert_eq!(f.on_state(Some(s.clone()), &here(None, 0, false), clock), Step::Load(T.into()));
        assert_eq!(f.target(T, clock), Some((60_000, true)));
        // It plays on here as it does there: no jumps back to 60 s every read.
        clock += 3_000;
        assert_eq!(f.on_state(Some(s.clone()), &here(Some(T), 63_000, true), clock), Step::Wait);
        clock += 3_000;
        assert_eq!(f.on_state(Some(s.clone()), &here(Some(T), 66_000, true), clock), Step::Wait);
        assert_eq!(f.position(clock), Some(66_000));
        // Their next song, sent 1 s ago by their clock, starts 1 s in here too.
        clock += 10_000;
        let next = HostState { track: "spotify:track:next".into(), position_ms: 0, sent_at_ms: clock + 29_000, mode: Mode::Play };
        assert_eq!(f.on_state(Some(next), &here(Some(T), 76_000, true), clock), Step::Load("spotify:track:next".into()));
        assert_eq!(f.target("spotify:track:next", clock), Some((1_000, true)));
    }

    #[test]
    fn broken_times_in_a_description_dont_crash() {
        let text = format!("[tl1;{T};5;-9223372036854775808;play]");
        let s = parse(&text).unwrap();
        assert_eq!(s.position_at(1_761_400_000_000), u32::MAX);
        let far = HostState { sent_at_ms: i64::MAX, ..s.clone() };
        assert_eq!(far.position_at(i64::MIN), 5);
        let mut f = Follow::new(0);
        f.on_state(Some(s), &here(None, 0, false), 1_761_400_000_000);
        // A state "sent" in the year 292 million is not taken as the host's clock.
        let mut g = Follow::new(0);
        g.on_state(Some(far), &here(None, 0, false), 1_761_400_000_000);
        assert_eq!(g.position(1_761_400_000_000), Some(5));
    }

    #[test]
    fn sharing_with_nothing_playing_doesnt_say_not_sharing() {
        let text = describe(&HostState::stopped(42));
        assert!(text.starts_with("Listen along with me on Truffle. "), "{text}");
        assert_eq!(parse(&text), Some(HostState::stopped(42)));
    }

    #[test]
    fn writes_go_on_after_the_clock_is_set_back() {
        let mut out = Outbox::default();
        out.push("a".into());
        let a = out.take(1_761_400_000_000).unwrap();
        out.done(a, true, 1_761_400_000_100);
        // The computer's clock was an hour fast and is put right: the next write isn't held back
        // an hour.
        out.push("b".into());
        assert_eq!(out.take(1_761_396_400_000).as_deref(), Some("b"));
    }

    #[test]
    fn nothing_is_done_while_the_song_loads_here() {
        let clock = 0i64;
        let mut f = Follow::new(clock);
        let s = state(0, clock, Mode::Play);
        f.on_state(Some(s.clone()), &here(None, 0, false), clock);
        let loading = Here { track: Some(T), position_ms: 0, duration_ms: 200_000, playing: false, loading: true };
        assert_eq!(f.on_state(Some(s), &loading, clock + 6_000), Step::Wait);
    }
}
