//! Middle column: the tab row (Home, Songs, Up next, Blocked, Search) and a list's page: the hero
//! (its cover over the cover blurred), the play bar with the one state button, the filter box and
//! the virtualized song table. Scrolling the songs folds the hero into a slim bar. Also the
//! playlist-details editor. A list can have a banner of the user's own across its hero (`banner`).

#[path = "banner.rs"]
pub(super) mod banner;
#[path = "listpage.rs"]
mod listpage;

use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Color32, CornerRadius, Galley, Id, Painter, Pos2, Rect,
    RichText, Stroke, TextEdit, TextFormat, Ui, pos2, vec2,
};

use self::banner::banner_items;
use super::dnd::SongsDrag;
use super::fin;
use super::kit;
use super::theme::{Palette, bold};
use super::util::{clean_description, fmt_song_count, fmt_thousands, mix};
use super::{EditDetails, MiddleTab, TruffleApp};
use crate::model::{Command, Source, TrackInfo};
use crate::text::{fold, push_fold};

/// The track list of the source shown in the centre.
pub struct TrackView {
    pub source: Source,
    pub tracks: Option<Arc<Vec<TrackInfo>>>,
    /// "title artists album" per track, folded (lower case, no accents) for filtering.
    search_keys: Vec<String>,
    pub filter: String,
    /// Indices into `tracks` that match `filter`, in playlist order.
    pub filtered: Vec<u32>,
    pub total_ms: u64,
    /// `(loaded, total)` while the controller is fetching.
    pub loading: Option<(usize, Option<usize>)>,
    pub error: Option<String>,
    /// Selected track (index into `tracks`): the one the keys act on, last clicked.
    pub selected: Option<usize>,
    /// The songs picked (Ctrl+click, Shift+click, Ctrl+A, Shift+arrows), indices into `tracks`;
    /// right-click and dragging act on all of them.
    pub picked: std::collections::BTreeSet<usize>,
    /// Where a Shift range starts.
    pub anchor: Option<usize>,
    /// Row (index into `filtered`) to scroll to on the next frame.
    pub scroll_to_row: Option<usize>,
    /// An artist's page: all its popular songs (else 5), and which releases (0 all, 1 albums,
    /// 2 singles).
    pub show_all: bool,
    pub releases: u8,
    /// How the songs on screen last changed while they showed: rows close or open as songs go or
    /// come (see `follow`).
    pub moves: Option<Moves>,
    /// The rows shown in the last pass, to see what changed since.
    last: Option<LastRows>,
}

/// How the songs on screen changed while they showed (a refresh, a like, Unblock): rows close up
/// where songs went and open where songs came, as their numbers roll.
pub struct Moves {
    /// The rows that went, as they were.
    pub gone: Vec<Gone>,
    /// The places among the rows of the songs that came.
    pub came: Vec<usize>,
    pub numbers: Renumbered,
    /// When they started (egui's clock), from the first pass that draws them.
    pub since: Option<f64>,
}

/// What the numbers were before songs were added to the list or taken out: the places in the list
/// of those added, and where those taken out stood.
#[derive(Clone, Default)]
pub struct Renumbered {
    added: Vec<usize>,
    removed: Vec<usize>,
}

impl Renumbered {
    /// The number the song at `ti` had before.
    pub fn was(&self, ti: usize) -> usize {
        ti + 1 - self.added.partition_point(|&a| a < ti) + self.removed.partition_point(|&b| b <= ti)
    }

    /// Whether any number changed.
    pub fn any(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }
}

/// A row whose song went: it fades as it closes up.
pub struct Gone {
    pub track: TrackInfo,
    /// Its number as it was.
    pub number: usize,
    /// Where it was: before this row (its place among the rows now; their count at the end).
    pub before: usize,
}

/// The rows a list showed in a pass, and what they were made of.
struct LastRows {
    tracks: Arc<Vec<TrackInfo>>,
    blocked: Arc<std::collections::HashSet<String>>,
    rows: Vec<u32>,
    filter: String,
    blocked_tab: bool,
    pass: u64,
}

impl TrackView {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            tracks: None,
            search_keys: Vec::new(),
            filter: String::new(),
            filtered: Vec::new(),
            total_ms: 0,
            loading: Some((0, None)),
            error: None,
            selected: None,
            picked: std::collections::BTreeSet::new(),
            anchor: None,
            scroll_to_row: None,
            show_all: false,
            releases: 0,
            moves: None,
            last: None,
        }
    }

    pub fn set_tracks(&mut self, tracks: Arc<Vec<TrackInfo>>) {
        self.search_keys = tracks
            .iter()
            .map(|t| {
                let mut key = String::with_capacity(t.name.len() + t.album.len() + 32);
                push_fold(&mut key, &t.name);
                for a in &t.artists {
                    key.push(' ');
                    push_fold(&mut key, &a.name);
                }
                key.push(' ');
                push_fold(&mut key, &t.album);
                key
            })
            .collect();
        self.total_ms = tracks.iter().map(|t| t.duration_ms as u64).sum();
        if self.selected.is_some_and(|i| i >= tracks.len()) {
            self.selected = None;
        }
        // Picks are places in the list: a reloaded list may have other songs there.
        self.picked.clear();
        self.anchor = None;
        self.tracks = Some(tracks);
        self.loading = None;
        self.error = None;
        self.refilter();
    }

    pub fn refilter(&mut self) {
        let words: Vec<String> = self.filter.split_whitespace().map(fold).collect();
        let len = self.tracks.as_ref().map_or(0, |t| t.len());
        self.filtered = if words.is_empty() {
            (0..len as u32).collect()
        } else {
            self.search_keys
                .iter()
                .enumerate()
                .filter(|(_, key)| words.iter().all(|w| key.contains(w.as_str())))
                .map(|(i, _)| i as u32)
                .collect()
        };
    }

    /// Follows the `rows` shown in this `pass` (egui's count) from `tracks` and `blocked`: when songs
    /// went or came since the pass before, while the filter and the tab stayed, they go into
    /// `moves`. Anything else (another filter, a pass without the list) just changes.
    pub fn follow(&mut self, tracks: &Arc<Vec<TrackInfo>>, blocked: &Arc<std::collections::HashSet<String>>, rows: &[u32], blocked_tab: bool, pass: u64) {
        let last = self.last.take().filter(|l| l.pass + 1 == pass && l.filter == self.filter && l.blocked_tab == blocked_tab);
        let same = |l: &LastRows| Arc::ptr_eq(&l.tracks, tracks) && Arc::ptr_eq(&l.blocked, blocked);
        match &last {
            None => self.moves = None,
            Some(l) if same(l) => {}
            Some(l) => {
                let uris = |list: &[TrackInfo], rows: &[u32]| rows.iter().map(|&i| list[i as usize].uri.clone()).collect::<Vec<_>>();
                self.moves = changes(&uris(&l.tracks, &l.rows), &uris(tracks, rows)).map(|(went, came)| {
                    let gone = went
                        .into_iter()
                        .map(|(k, before)| {
                            let i = l.rows[k] as usize;
                            Gone { track: l.tracks[i].clone(), number: i + 1, before }
                        })
                        .collect();
                    // The numbers follow the whole list.
                    let all = |list: &[TrackInfo]| list.iter().map(|t| t.uri.clone()).collect::<Vec<_>>();
                    let (removed, added) = if Arc::ptr_eq(&l.tracks, tracks) {
                        Default::default()
                    } else {
                        changes(&all(&l.tracks), &all(tracks)).map(|(r, a)| (r.into_iter().map(|(_, before)| before).collect(), a)).unwrap_or_default()
                    };
                    Moves { gone, came, numbers: Renumbered { added, removed }, since: None }
                });
                self.moves = self.moves.take().filter(|m| !m.gone.is_empty() || !m.came.is_empty() || m.numbers.any());
            }
        }
        self.last = match last {
            Some(mut l) if same(&l) => {
                l.pass = pass;
                Some(l)
            }
            _ => Some(LastRows { tracks: tracks.clone(), blocked: blocked.clone(), rows: rows.to_vec(), filter: self.filter.clone(), blocked_tab, pass }),
        };
    }

    /// Selects and scrolls to the first track with this URI (clearing the filter if it hides it).
    pub fn scroll_to_uri(&mut self, uri: &str) {
        let Some(index) = self.tracks.as_ref().and_then(|t| t.iter().position(|t| t.uri == uri)) else { return };
        if !self.filtered.contains(&(index as u32)) {
            self.filter.clear();
            self.refilter();
        }
        if let Some(row) = self.filtered.iter().position(|&i| i as usize == index) {
            self.scroll_to_row = Some(row);
            self.selected = Some(index);
            self.picked = std::iter::once(index).collect();
            self.anchor = Some(index);
        }
    }

    /// A click on the song at `ti` (`rows`: the rows as shown), with the keys held (see [`pick`]).
    pub fn click(&mut self, ti: usize, mods: egui::Modifiers, rows: &[u32]) {
        let (picked, anchor) = pick(&self.picked, self.anchor.or(self.selected), rows, ti, mods.command, mods.shift);
        self.selected = if picked.contains(&ti) { Some(ti) } else { picked.iter().next_back().copied() };
        self.picked = picked;
        self.anchor = anchor;
    }

    /// Lets go of every highlight (Esc, or a click on nothing): the songs picked and the one the
    /// keys move from.
    pub fn let_go(&mut self) {
        self.picked.clear();
        self.selected = None;
        self.anchor = None;
    }
}

/// The songs taken out (each with its place and that of the song now where it was) and the places
/// of the songs added.
type Changes = (Vec<(usize, usize)>, Vec<usize>);

/// How the songs `old` became `new` (their URIs): the places in `old` of the songs taken out, each
/// with the place in `new` of the song that now stands where it was, and the places in `new` of the
/// songs added. None when they differ otherwise (another order) or many changed: then the list
/// just changes.
fn changes(old: &[String], new: &[String]) -> Option<Changes> {
    const MANY: usize = 50;
    let kept: std::collections::HashSet<&String> = new.iter().collect();
    let had: std::collections::HashSet<&String> = old.iter().collect();
    let (mut i, mut j, mut out, mut added) = (0, 0, Vec::new(), Vec::new());
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            (i, j) = (i + 1, j + 1);
        } else if i < old.len() && !kept.contains(&old[i]) {
            out.push((i, j));
            i += 1;
        } else if j < new.len() && !had.contains(&new[j]) {
            added.push(j);
            j += 1;
        } else {
            return None;
        }
        if out.len() + added.len() > MANY {
            return None;
        }
    }
    Some((out, added))
}

/// Each artist's songs in Liked Songs (their page's "Your likes" row, its releases' counts). A
/// like makes them all out of date, but they stay on screen while fresh ones are asked for, so
/// nothing blinks.
#[derive(Default)]
pub(super) struct LikesByArtist {
    known: std::collections::HashMap<String, Arc<crate::model::ArtistLikes>>,
    /// Asked for since the last change to Liked Songs.
    asked: std::collections::HashSet<String>,
}

impl LikesByArtist {
    pub(super) fn get(&self, artist: &str) -> Option<&Arc<crate::model::ArtistLikes>> {
        self.known.get(artist)
    }

    /// Whether to ask for this artist's likes now: once after each change to Liked Songs (the
    /// ones known stay until the answer).
    pub(super) fn ask(&mut self, artist: &str) -> bool {
        !self.asked.contains(artist) && self.asked.insert(artist.to_owned())
    }

    /// Liked Songs changed: every artist's likes are asked for again when next shown.
    pub(super) fn stale(&mut self) {
        self.asked.clear();
    }

    /// An answer: it replaces what was shown.
    pub(super) fn answer(&mut self, likes: Arc<crate::model::ArtistLikes>) {
        self.known.insert(likes.artist.clone(), likes);
    }
}

struct HeaderInfo {
    kind: &'static str,
    title: String,
    owner: String,
    image_url: Option<String>,
    editable_uri: Option<String>,
    /// An artist: a round picture.
    round: bool,
    fallback_count: Option<u32>,
    /// The head is drawn over the list's banner.
    on_banner: bool,
}

/// The colours of the words in the head: the theme's, or light ones over a banner (its dark
/// gradient lies under them in both themes), with a soft shadow so they read on any picture.
#[derive(Clone, Copy)]
struct Ink {
    strong: Color32,
    text: Color32,
    weak: Color32,
    shadow: bool,
}

impl Ink {
    fn of(p: &Palette, info: &HeaderInfo) -> Self {
        if info.on_banner {
            let (text, weak) = (Color32::from_rgb(236, 236, 238), Color32::from_rgb(206, 206, 212));
            Ink { strong: Color32::WHITE, text, weak, shadow: true }
        } else {
            Ink { strong: p.text_strong, text: p.text, weak: p.text_weak, shadow: false }
        }
    }

    /// Paints laid-out words in `color`, over their shadow on a banner.
    fn paint(self, painter: &Painter, pos: Pos2, galley: Arc<Galley>, color: Color32) {
        if self.shadow {
            painter.galley_with_override_text_color(pos + vec2(0.0, 1.0), galley.clone(), Color32::from_black_alpha(150));
        }
        painter.galley(pos, galley, color);
    }
}

enum RowAction {
    /// Pick just this song (dragging one that isn't picked).
    Select(usize),
    /// A click on a row, with the keys held (Ctrl adds or takes away, Shift picks a range).
    Click(usize, egui::Modifiers),
    /// A right-click on a row: the menu for it (or for all the picked songs, when it is one of them).
    Menu(usize, egui::Pos2),
    Play(usize),
    Like(String, bool),
    Block(String, bool),
    Open(Source),
}

/// The picked songs after a click on `ti` (`rows`: the rows as shown, in order): Ctrl adds or takes
/// it away, Shift picks from the anchor to it (with Ctrl, added to what was picked), a plain click
/// picks just it. Returns the picks and the new anchor.
pub(super) fn pick(
    picked: &std::collections::BTreeSet<usize>,
    anchor: Option<usize>,
    rows: &[u32],
    ti: usize,
    ctrl: bool,
    shift: bool,
) -> (std::collections::BTreeSet<usize>, Option<usize>) {
    let pos = |i: usize| rows.iter().position(|&r| r as usize == i);
    if shift && let (Some(a), Some(b)) = (anchor.and_then(pos), pos(ti)) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let mut out = if ctrl { picked.clone() } else { std::collections::BTreeSet::new() };
        out.extend(rows[lo..=hi].iter().map(|&r| r as usize));
        return (out, anchor);
    }
    if ctrl {
        let mut out = picked.clone();
        if !out.remove(&ti) {
            out.insert(ti);
        }
        return (out, Some(ti));
    }
    (std::iter::once(ti).collect(), Some(ti))
}

/// What a list's state button does, and says, right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PlayState {
    /// Start the list in a fresh random order.
    Play,
    /// Carry on: the list is paused (`paused`), or its round is under way, `heard` of `total`.
    Resume { heard: usize, total: usize, paused: bool },
    /// The list plays: pause it.
    Pause,
    /// Its songs are still loading.
    Loading { loaded: usize, total: Option<usize> },
}

impl PlayState {
    /// The word on the button and the count after it ("47/312").
    pub(super) fn label(self) -> (&'static str, Option<String>) {
        match self {
            PlayState::Play => ("Play", None),
            PlayState::Pause => ("Pause", None),
            PlayState::Resume { heard, total, .. } => {
                ("Resume", (heard > 0 && total > 0).then(|| format!("{}/{}", fmt_thousands(heard.min(total)), fmt_thousands(total))))
            }
            PlayState::Loading { loaded, total } => {
                let count = match total {
                    Some(t) if t > 0 => Some(format!("{}/{}", fmt_thousands(loaded.min(t)), fmt_thousands(t))),
                    _ => (loaded > 0).then(|| fmt_thousands(loaded)),
                };
                ("Loading", count)
            }
        }
    }

    pub(super) fn tip(self) -> String {
        match self {
            PlayState::Play => "Shuffle play: every song in a fresh random order".to_owned(),
            PlayState::Pause => "Pause (Space)".to_owned(),
            PlayState::Resume { paused: true, .. } => "Carry on where it stopped (Space)".to_owned(),
            PlayState::Resume { heard, total, .. } => {
                let left = total.saturating_sub(heard);
                format!("Carry on with this round: the {} songs not heard yet come first", fmt_thousands(left))
            }
            PlayState::Loading { .. } => "Loading the songs".to_owned(),
        }
    }
}

/// Where a list stands, for its state button.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct ListNow {
    /// It is the playing list, and it plays (or is about to).
    pub playing: bool,
    /// It is the playing list, paused or stopped.
    pub paused: bool,
    /// Shown, with its songs still loading.
    pub loading: Option<(usize, Option<usize>)>,
    pub songs: Option<usize>,
    /// Songs heard in its current round.
    pub heard: usize,
}

/// The state button's state: playing beats loading beats a pause or a round under way.
pub(super) fn play_state(now: &ListNow) -> PlayState {
    if now.playing {
        return PlayState::Pause;
    }
    if let Some((loaded, total)) = now.loading {
        return PlayState::Loading { loaded, total };
    }
    let total = now.songs.unwrap_or(0);
    if now.paused {
        return PlayState::Resume { heard: now.heard, total, paused: true };
    }
    if now.heard > 0 && now.heard < total {
        return PlayState::Resume { heard: now.heard, total, paused: false };
    }
    PlayState::Play
}

/// Lays out `text` on one line, elided with "…" to fit `max_width`.
pub(super) fn one_line(ui: &Ui, text: &str, font: egui::FontId, color: Color32, max_width: f32, strike: bool) -> Arc<Galley> {
    let format = TextFormat {
        font_id: font,
        color,
        strikethrough: if strike { Stroke::new(1.0, color) } else { Stroke::NONE },
        ..Default::default()
    };
    let mut job = LayoutJob::single_section(text.to_owned(), format);
    job.wrap = TextWrapping::truncate_at_width(max_width.max(1.0));
    ui.painter().layout_job(job)
}

/// "Loading 2,300 of 10,000 songs", with a thin bar when the total is known.
fn loading_progress(ui: &mut Ui, p: &Palette, loaded: usize, total: Option<usize>, width: f32) {
    let text = match total {
        Some(t) if t > 0 => format!("Loading {} of {} songs", fmt_thousands(loaded.min(t)), fmt_thousands(t)),
        _ if loaded > 0 => format!("Loading songs… {}", fmt_thousands(loaded)),
        _ => "Loading songs…".to_owned(),
    };
    let label = egui::Label::new(RichText::new(text).size(12.5).color(p.text_weak));
    let bar = total.filter(|&t| t > 0).map(|t| {
        egui::ProgressBar::new((loaded as f32 / t as f32).clamp(0.0, 1.0))
            .desired_width(width)
            .desired_height(6.0)
            .fill(p.accent_ink)
    });
    if let Some(bar) = bar {
        ui.add(bar);
    }
    ui.add(label);
}

/// The colour of the Liked Songs "cover".
pub(super) fn liked_color(p: &Palette) -> Color32 {
    mix(p.accent, Color32::from_rgb(70, 40, 160), 0.45)
}

impl TruffleApp {
    fn header_info(&self, source: &Source) -> HeaderInfo {
        match source {
            Source::Mix(id) if self.mixes.iter().all(|m| m.id != *id) && id.starts_with(crate::library::MIX_ARTIST) => {
                // An artist's liked songs (from their page).
                let artist = id.trim_start_matches(crate::library::MIX_ARTIST);
                let liked = self.collection.as_ref().and_then(|c| c.artists.iter().find(|a| a.uri == artist));
                let count = self.artist_likes.get(artist).map(|l| l.songs as u32);
                let info = self.source_info.get(&Source::Artist(artist.to_owned()));
                HeaderInfo {
                    kind: "Your likes",
                    title: liked.map(|a| a.name.clone()).or_else(|| info.map(|i| i.title.clone())).unwrap_or_else(|| "Artist".to_owned()),
                    owner: "Songs you've liked".to_owned(),
                    image_url: liked.and_then(|a| a.image.clone()).or_else(|| info.and_then(|i| i.image.clone())),
                    editable_uri: None,
                    round: true,
                    fallback_count: count.or(liked.map(|a| a.songs as u32)),
                    on_banner: false,
                }
            }
            Source::Mix(id) => {
                let m = self.mixes.iter().find(|m| m.id == *id);
                HeaderInfo {
                    kind: "Truffle mix",
                    title: m.map_or_else(|| "Mix".to_owned(), |m| m.name.clone()),
                    owner: m.map(|m| m.description.clone()).unwrap_or_default(),
                    image_url: m.and_then(|m| m.image.clone()),
                    editable_uri: None,
                    round: false,
                    fallback_count: m.map(|m| m.songs as u32),
                    on_banner: false,
                }
            }
            Source::LikedSongs => HeaderInfo {
                kind: "Collection",
                title: "Liked Songs".to_owned(),
                owner: self.profile.as_ref().map(|p| p.display_name.clone()).unwrap_or_default(),
                image_url: None,
                editable_uri: None,
                round: false,
                fallback_count: (!self.liked.is_empty()).then(|| self.liked.len() as u32),
                on_banner: false,
            },
            Source::Playlist(uri) => match self.playlist(uri) {
                Some(p) => HeaderInfo {
                    kind: "Playlist",
                    title: p.name.clone(),
                    owner: p.owner_name.clone(),
                    image_url: p.image_url.clone(),
                    editable_uri: p.editable.then(|| p.uri.clone()),
                    round: false,
                    fallback_count: p.track_count,
                    on_banner: false,
                },
                None => HeaderInfo {
                    kind: "Playlist",
                    title: "Playlist".to_owned(),
                    owner: String::new(),
                    image_url: None,
                    editable_uri: None,
                    round: false,
                    fallback_count: None,
                    on_banner: false,
                },
            },
            Source::Album(_) | Source::Artist(_) => {
                let info = self.source_info.get(source);
                let artist = matches!(source, Source::Artist(_));
                let kind = if artist { "Artist" } else { "Album" };
                HeaderInfo {
                    kind,
                    title: info.map(|i| i.title.clone()).unwrap_or_else(|| kind.to_owned()),
                    owner: info.map(|i| i.subtitle.clone()).unwrap_or_default(),
                    image_url: info.and_then(|i| i.image.clone()),
                    editable_uri: None,
                    round: artist,
                    fallback_count: None,
                    on_banner: false,
                }
            }
        }
    }

    /// How many songs a list has, as far as anything says: its loaded songs, else the library's
    /// count. The sidebar, Home, the list's page and the record all count with this.
    pub(super) fn known_song_count(&self, source: &Source) -> Option<usize> {
        self.loaded_song_count(source).or_else(|| match source {
            Source::LikedSongs => (!self.liked.is_empty()).then(|| self.liked.len()),
            Source::Playlist(uri) => self.playlist(uri).and_then(|p| p.track_count).map(|n| n as usize),
            Source::Album(_) | Source::Artist(_) => None,
            Source::Mix(id) => self.mixes.iter().find(|m| m.id == *id).map(|m| m.songs),
        })
    }

    /// The songs loaded for a list: the open list's, else the record's when it plays from it.
    pub(super) fn loaded_song_count(&self, source: &Source) -> Option<usize> {
        let open = self.view.as_ref().filter(|v| v.source == *source).and_then(|v| v.tracks.as_ref());
        open.or_else(|| self.deck.as_ref().filter(|d| d.source == *source).map(|d| &d.tracks)).map(|t| t.len())
    }

    /// Songs heard in a list's current round.
    /// Songs heard in this list's round. Rounds only exist while "Play every song before
    /// repeats" is on; with it off every play is a fresh shuffle, so nothing counts as heard.
    pub(super) fn heard(&self, source: &Source) -> usize {
        if !self.rules.rounds_on() {
            return 0;
        }
        self.round_progress.get(&source.cache_key()).copied().unwrap_or(0)
    }

    pub(super) fn list_now(&self, source: &Source) -> ListNow {
        let current = self.playback.track.is_some() && self.playback.source.as_ref() == Some(source);
        let playing = current && self.is_playing();
        let loading = self.view.as_ref().filter(|v| v.source == *source && v.tracks.is_none()).and_then(|v| v.loading);
        ListNow { playing, paused: current && !playing, loading, songs: self.known_song_count(source), heard: self.heard(source) }
    }

    /// What the state button does when pressed.
    pub(super) fn press_play(&mut self, source: &Source, state: PlayState) {
        match state {
            PlayState::Pause | PlayState::Resume { paused: true, .. } => self.send(Command::PlayPause),
            // With "Play every song before repeats" on, a new order puts the songs not heard yet
            // this round first: that carries the round on.
            PlayState::Resume { .. } | PlayState::Play => {
                self.send(Command::PlaySource { source: source.clone(), start_index: None, shuffle: true });
            }
            PlayState::Loading { .. } => {}
        }
    }

    /// The chevron's "New round": every song in a fresh random order, as if none had been heard.
    fn new_round(&mut self, source: &Source) {
        self.send(Command::NewRound { source: source.clone() });
    }

    /// Opens the name and description editor for one of the user's playlists.
    pub(super) fn open_edit_details(&mut self, uri: &str) {
        let Some(pl) = self.playlist(uri) else { return };
        let (name, description) = (pl.name.clone(), clean_description(&pl.description));
        self.edit_details = Some(EditDetails {
            uri: uri.to_owned(),
            original_name: name.clone(),
            original_description: description.clone(),
            name,
            description,
        });
    }

    /// Home shows instead of the Songs tab while no list is open, and whenever it was asked for.
    pub(super) fn home_shown(&self) -> bool {
        self.middle_tab == MiddleTab::Home || (self.middle_tab == MiddleTab::Songs && self.view.is_none())
    }

    pub(super) fn center_view(&mut self, ui: &mut Ui) {
        if self.home_shown() {
            // Home has no tab row (the Final design): the page fills the column.
            self.home_view(ui);
            return;
        }
        // A list's page (its Songs and Blocked tabs are on it), or the search results.
        match self.middle_tab {
            MiddleTab::Search => self.search_view(ui),
            MiddleTab::Albums | MiddleTab::Artists => self.collection_page(ui),
            MiddleTab::Profile => self.profile_page(ui),
            MiddleTab::People => self.people_page(ui),
            // Narrow-only pages, when the window is made wider: the sidebar shows the playlists
            // and the side panel the playing song.
            MiddleTab::Library | MiddleTab::Playing => self.home_view(ui),
            _ => self.list_page(ui),
        }
    }

    /// The header's right-click menu: the list's banner.
    fn header_menu(&mut self, response: &egui::Response, source: &Source) {
        let set = self.prefs.banners.contains_key(&source.cache_key());
        let picking = self.wallpaper.banners.picking();
        let mut act = None;
        response.context_menu(|ui| {
            ui.set_width(200.0);
            act = banner_items(ui, set, picking);
        });
        if let Some(act) = act {
            self.banner_act(&response.ctx, source, act);
        }
    }

    /// While songs from the table are dragged: their name by the pointer.
    pub(super) fn paint_songs_drag(&self, ui: &Ui, tracks: &[TrackInfo]) {
        let p = self.palette;
        let Some(drag) = egui::DragAndDrop::payload::<SongsDrag>(ui.ctx()) else { return };
        // Songs dragged from elsewhere (the player bar) are drawn there.
        if drag.uris.first().is_none_or(|first| !tracks.iter().any(|t| t.uri == *first)) {
            return;
        }
        let Some(pointer) = ui.ctx().pointer_latest_pos() else { return };
        let text = match drag.uris.as_slice() {
            [uri] => tracks.iter().find(|t| t.uri == *uri).map_or_else(|| "1 song".to_owned(), |t| t.name.clone()),
            uris => fmt_song_count(uris.len()),
        };
        let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::new("songs_drag")));
        let g = painter.layout_no_wrap(text, bold(12.0), p.text_strong);
        let w = g.size().x.min(260.0);
        let rect = Rect::from_min_size(pointer + vec2(14.0, 10.0), vec2(w + 38.0, 28.0));
        painter.add(ui.visuals().popup_shadow.as_shape(rect, CornerRadius::same(kit::RADIUS)));
        painter.rect_filled(rect, CornerRadius::same(kit::RADIUS), p.raised_bg);
        painter.rect_stroke(rect, CornerRadius::same(kit::RADIUS), Stroke::new(1.0, p.accent.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        super::svgicon::paint(&painter, Rect::from_center_size(pos2(rect.left() + 15.0, rect.center().y), vec2(13.0, 13.0)), "note", p.accent_ink, super::svgicon::STROKE);
        let clip = Rect::from_min_max(pos2(rect.left() + 28.0, rect.top()), pos2(rect.right() - 8.0, rect.bottom()));
        painter.with_clip_rect(clip).galley(pos2(clip.left(), rect.center().y - g.size().y / 2.0), g, p.text_strong);
    }

    pub(super) fn edit_details_modal(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let Some(edit) = self.edit_details.as_mut() else { return };
        let (mut save, mut cancel) = (false, false);
        let modal = fin::dialog(ctx, Id::new("edit_details"), |ui| {
            ui.set_width(440.0);
            fin::dialog_head(ui, "Edit details", None);
            ui.add_space(4.0);
            fin::field_label(ui, "Name");
            fin::field(ui, &p, TextEdit::singleline(&mut edit.name).char_limit(100).desired_width(f32::INFINITY));
            fin::field_label(ui, "Description");
            fin::field(
                ui,
                &p,
                TextEdit::multiline(&mut edit.description)
                    .char_limit(300)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .hint_text("Optional"),
            );
            let name_ok = !edit.name.trim().is_empty();
            let (ok, no) = fin::dialog_actions(ui, &p, Id::new("edit_details"), "Save", name_ok);
            save |= ok;
            cancel |= no;
        });

        if save {
            if let Some(edit) = self.edit_details.take() {
                let name = edit.name.trim().to_owned();
                let description = edit.description.split_whitespace().collect::<Vec<_>>().join(" ");
                let name = (name != edit.original_name).then_some(name);
                let description = (description != edit.original_description).then_some(description);
                if name.is_some() || description.is_some() {
                    self.send(Command::UpdatePlaylistDetails { uri: edit.uri, name, description });
                    self.toast("Saving changes…", false);
                }
            }
        } else if cancel || modal.should_close() {
            self.edit_details = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now(playing: bool, paused: bool, loading: Option<(usize, Option<usize>)>, songs: Option<usize>, heard: usize) -> ListNow {
        ListNow { playing, paused, loading, songs, heard }
    }

    #[test]
    fn the_state_button_says_what_it_will_do() {
        // Nothing heard yet: PLAY.
        assert_eq!(play_state(&now(false, false, None, Some(312), 0)), PlayState::Play);
        // A round under way: RESUME 47/312.
        let resume = play_state(&now(false, false, None, Some(312), 47));
        assert_eq!(resume, PlayState::Resume { heard: 47, total: 312, paused: false });
        assert_eq!(resume.label(), ("Resume", Some("47/312".to_owned())));
        // A finished round (or one counted against a list that shrank) starts afresh.
        assert_eq!(play_state(&now(false, false, None, Some(312), 312)), PlayState::Play);
        // Playing beats everything, also while its songs reload.
        assert_eq!(play_state(&now(true, false, Some((10, Some(312))), Some(312), 47)), PlayState::Pause);
        // Paused: RESUME, with the round's count when there is one.
        assert_eq!(play_state(&now(false, true, None, Some(312), 0)).label(), ("Resume", None));
        assert_eq!(play_state(&now(false, true, None, Some(312), 5)).label(), ("Resume", Some("5/312".to_owned())));
        // Loading shows how far it got.
        let loading = play_state(&now(false, false, Some((1200, Some(3400))), None, 0));
        assert_eq!(loading.label(), ("Loading", Some("1,200/3,400".to_owned())));
        assert_eq!(play_state(&now(false, false, Some((0, None)), None, 0)).label(), ("Loading", None));
    }

    fn song(name: &str, artist: &str, album: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{name}"),
            name: name.to_owned(),
            artists: vec![crate::model::ArtistRef { name: artist.to_owned(), uri: String::new() }],
            album: album.to_owned(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 200_000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    #[test]
    fn the_filter_ignores_case_and_accents() {
        let mut view = TrackView::new(Source::LikedSongs);
        view.set_tracks(Arc::new(vec![
            song("Kickstart My Heart", "Mötley Crüe", "Dr. Feelgood"),
            song("Halo", "Beyoncé", "I Am... Sasha Fierce"),
            song("Hoppípolla", "Sigur Rós", "Takk..."),
            song("Army of Me", "Björk", "Post"),
            song("Кукушка", "Кино", "Чёрный альбом"),
        ]));
        let mut shown = |filter: &str| {
            view.filter = filter.to_owned();
            view.refilter();
            view.filtered.clone()
        };
        assert_eq!(shown(""), [0, 1, 2, 3, 4]);
        assert_eq!(shown("motley"), [0]);
        assert_eq!(shown("MÖTLEY heart"), [0]);
        assert_eq!(shown("beyonce"), [1]);
        assert_eq!(shown("sigur ros"), [2]);
        assert_eq!(shown("hoppipolla"), [2]);
        assert_eq!(shown("bjork post"), [3]);
        assert_eq!(shown("КИНО"), [4]);
        assert!(shown("motley bjork").is_empty());
    }

    #[test]
    fn songs_that_go_or_come_are_placed_where_they_were() {
        let list = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let old = list(&["a", "b", "c", "d"]);
        // One gone: it stood before "c" (now index 1).
        assert_eq!(changes(&old, &list(&["a", "c", "d"])), Some((vec![(1, 1)], vec![])));
        // Two together, and one at the end.
        assert_eq!(changes(&old, &list(&["c"])), Some((vec![(0, 0), (1, 0), (3, 1)], vec![])));
        // Songs added, at the top and the end.
        assert_eq!(changes(&old, &list(&["x", "a", "b", "c", "d", "y"])), Some((vec![], vec![0, 5])));
        assert_eq!(changes(&old, &list(&["x", "a", "c", "d"])), Some((vec![(1, 2)], vec![0])));
        // Another order: no telling what went where.
        assert_eq!(changes(&old, &list(&["b", "a", "c", "d"])), None);
    }

    #[test]
    fn a_view_follows_what_changes_in_sight() {
        let list = |names: &[&str]| Arc::new(names.iter().map(|n| song(n, "a", "b")).collect::<Vec<_>>());
        let rows = |n: usize| (0..n as u32).collect::<Vec<_>>();
        let blocked = Arc::default();
        let mut view = TrackView::new(Source::LikedSongs);
        let old = list(&["a", "b", "c", "d"]);
        view.set_tracks(old.clone());
        view.follow(&old, &blocked, &rows(4), false, 1);
        assert!(view.moves.is_none());
        // "c" went and "x" came at the top: "c" closes before "d" (row 3 now), "x" opens.
        let new = list(&["x", "a", "b", "d"]);
        view.set_tracks(new.clone());
        view.follow(&new, &blocked, &rows(4), false, 2);
        let moves = view.moves.as_ref().unwrap();
        assert_eq!(moves.gone.iter().map(|g| (g.track.name.as_str(), g.number, g.before)).collect::<Vec<_>>(), [("c", 3, 3)]);
        assert_eq!(moves.came, [0]);
        // "a" was 1 and is 2 now; "d" was 4 and stays.
        assert_eq!((moves.numbers.was(1), moves.numbers.was(3)), (1, 4));
        // Still there in the next pass; gone after a pass without the list.
        view.follow(&new, &blocked, &rows(4), false, 3);
        assert!(view.moves.is_some());
        let newer = list(&["x", "a", "d"]);
        view.set_tracks(newer.clone());
        view.follow(&newer, &blocked, &rows(3), false, 5);
        assert!(view.moves.is_none());
    }

    #[test]
    fn artist_likes_stay_shown_while_fresh_ones_are_asked_for() {
        let likes = |songs: usize| Arc::new(crate::model::ArtistLikes { artist: "spotify:artist:a".into(), songs, albums: Vec::new() });
        let mut kept = LikesByArtist::default();
        let a = "spotify:artist:a";
        // Asked once, not again while the answer is on its way.
        assert!(kept.ask(a));
        assert!(!kept.ask(a));
        assert!(kept.get(a).is_none());
        kept.answer(likes(12));
        assert!(!kept.ask(a));
        // A like: asked again, and the 12 stays until the answer.
        kept.stale();
        assert_eq!(kept.get(a).map(|l| l.songs), Some(12));
        assert!(kept.ask(a));
        assert!(!kept.ask(a));
        assert_eq!(kept.get(a).map(|l| l.songs), Some(12));
        kept.answer(likes(13));
        assert_eq!(kept.get(a).map(|l| l.songs), Some(13));
    }
}

#[cfg(test)]
mod pick_tests {
    use super::pick;
    use std::collections::BTreeSet;

    fn set(v: &[usize]) -> BTreeSet<usize> {
        v.iter().copied().collect()
    }

    #[test]
    fn clicks_pick_ranges_and_toggle() {
        // Shown in this order (a filter or the Blocked tab can leave gaps).
        let rows = [0u32, 2, 3, 5, 8];
        let (p, a) = pick(&BTreeSet::new(), None, &rows, 3, false, false);
        assert_eq!((p.clone(), a), (set(&[3]), Some(3)));
        // Shift: from the anchor to it, in shown order.
        let (p, a) = pick(&p, a, &rows, 8, false, true);
        assert_eq!((p.clone(), a), (set(&[3, 5, 8]), Some(3)));
        // Ctrl: takes one away, adds another.
        let (p, a) = pick(&p, a, &rows, 5, true, false);
        assert_eq!((p.clone(), a), (set(&[3, 8]), Some(5)));
        let (p, a) = pick(&p, a, &rows, 0, true, false);
        assert_eq!(p.clone(), set(&[0, 3, 8]));
        // Ctrl+Shift: a range added to what was picked.
        let (p, _) = pick(&p, a, &rows, 2, true, true);
        assert_eq!(p, set(&[0, 2, 3, 8]));
        // A plain click picks just it.
        assert_eq!(pick(&p, Some(0), &rows, 5, false, false).0, set(&[5]));
    }
}
