//! "Find duplicates" in one of the user's own playlists: the songs in it more than once (a single
//! and the album version, or the same song added twice), with the copies to take out ticked. The
//! playlist is read as Spotify has it now when the box opens, and grouped once then; the box only
//! draws the lines on screen, so a playlist of thousands of songs scrolls as easily as a short one.

use std::collections::HashMap;
use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, TextFormat, Ui, pos2, vec2};

use super::TruffleApp;
use super::fin;
use super::svgicon;
use super::theme::{Palette, bold, regular};
use super::tracks::one_line;
use super::util::{fmt_added, fmt_ms, fmt_thousands};
use crate::model::{Command, TrackInfo};
use crate::text::{SongKey, same_artists, song_key};

/// Copies further apart in length than this (ms) are marked OTHER LENGTH: a remix or an edit,
/// maybe.
const LENGTH_SLACK_MS: i64 = 5_000;
/// An unticked box's edge.
const BOX_EDGE: Color32 = Color32::from_rgb(85, 85, 93);
/// A copy's row (`.r`); a song's name with the gap and the line above it (`.grp`, `.gh`).
const ROW_H: f32 = 30.0;
const HEAD_H: f32 = 46.0;
/// The length, date added and label columns (`.m`, `.d`, `.stay`), and the gap between parts.
const LENGTH_W: f32 = 52.0;
const DATE_W: f32 = 92.0;
const LABEL_W: f32 = 90.0;
const GAP: f32 = 10.0;
/// Narrower rows leave the date out.
const DATE_ROOM: f32 = 420.0;

/// The open "Find duplicates" box.
pub(super) struct Dupes {
    uri: String,
    /// What was found; `None` while the playlist is read.
    found: Option<Found>,
}

/// The songs found, as the box lists them.
struct Found {
    /// The playlist as it was read.
    tracks: Arc<Vec<TrackInfo>>,
    /// How many songs are in it more than once.
    songs: usize,
    lines: Vec<Line>,
    /// Where each line starts from the top of the list, and the list's whole height last.
    tops: Vec<f32>,
    /// How many copies are ticked.
    ticked: usize,
}

/// A song in here more than once, or one copy of it.
enum Line {
    Song { name: String, artists: String },
    Copy(CopyRow),
}

struct CopyRow {
    /// Its place in the playlist.
    row: usize,
    /// Ticked: it goes.
    ticked: bool,
    /// The copy that stays unless ticked (the one added first).
    keep: bool,
    other_length: bool,
    /// Greyed in the list (Spotify can't play it).
    dim: bool,
    album: String,
    length: String,
    added: String,
}

impl Line {
    fn height(&self) -> f32 {
        match self {
            Line::Song { .. } => HEAD_H,
            Line::Copy(_) => ROW_H,
        }
    }
}

impl Found {
    fn new(tracks: Arc<Vec<TrackInfo>>, groups: &[Group]) -> Self {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let mut lines = Vec::new();
        for g in groups {
            let first = &tracks[g.rows[0]];
            lines.push(Line::Song { name: first.name.clone(), artists: first.artist_line() });
            for &row in &g.rows {
                let t = &tracks[row];
                lines.push(Line::Copy(CopyRow {
                    row,
                    ticked: g.ticked_at_start(&tracks, row),
                    keep: row == g.keep,
                    other_length: g.other_length(&tracks, row),
                    dim: !t.playable,
                    album: t.album.clone(),
                    length: fmt_ms(t.duration_ms),
                    added: fmt_added(t.added_at, now),
                }));
            }
        }
        let mut tops = Vec::with_capacity(lines.len() + 1);
        let mut y = 0.0;
        for line in &lines {
            tops.push(y);
            y += line.height();
        }
        tops.push(y);
        let ticked = lines.iter().filter(|l| matches!(l, Line::Copy(c) if c.ticked)).count();
        Self { tracks, songs: groups.len(), lines, tops, ticked }
    }

    /// The places of the ticked copies.
    fn picked(&self) -> Vec<usize> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Copy(c) if c.ticked => Some(c.row),
                _ => None,
            })
            .collect()
    }

    /// The songs and their copies, scrolling when longer than `max_h`. Only the lines on screen
    /// are drawn; a click on a copy ticks or unticks it.
    fn list(&mut self, ui: &mut Ui, p: &Palette, w: f32, max_h: f32) {
        let total = self.tops[self.lines.len()];
        // Clear of the scroll bar, when there is one.
        let w = if total > max_h { w - 12.0 } else { w };
        egui::ScrollArea::vertical().id_salt("dupes_list").max_height(max_h).auto_shrink([false, true]).show_viewport(ui, |ui, view| {
            ui.set_height(total);
            let origin = ui.max_rect().min;
            let first = self.tops[1..].partition_point(|&bottom| bottom <= view.top());
            for k in first..self.lines.len() {
                if self.tops[k] >= view.bottom() {
                    break;
                }
                let rect = Rect::from_min_max(pos2(origin.x, origin.y + self.tops[k]), pos2(origin.x + w, origin.y + self.tops[k + 1]));
                match &mut self.lines[k] {
                    Line::Song { name, artists } => song_head(ui, rect, k, name, artists),
                    Line::Copy(c) => {
                        if copy_row(ui, p, rect, c) {
                            c.ticked = !c.ticked;
                            if c.ticked {
                                self.ticked += 1;
                            } else {
                                self.ticked -= 1;
                            }
                        }
                    }
                }
            }
        });
    }
}

impl TruffleApp {
    /// Opens "Find duplicates" for one of the user's own playlists and reads it as it is now. Not
    /// over another dialog, so one Enter never answers both.
    pub(super) fn find_duplicates(&mut self, uri: &str) {
        if self.dialog_open() || !self.playlist(uri).is_some_and(|p| p.editable) {
            return;
        }
        self.dupes = Some(Dupes { uri: uri.to_owned(), found: None });
        self.send(Command::FindDuplicates(uri.to_owned()));
    }

    /// The playlist read for the box, grouped once here. Nothing in it twice: no box, a note.
    pub(super) fn on_duplicates(&mut self, ctx: &egui::Context, uri: &str, tracks: Option<Arc<Vec<TrackInfo>>>) {
        if !self.dupes.as_ref().is_some_and(|d| d.uri == uri && d.found.is_none()) {
            return;
        }
        let Some(tracks) = tracks else {
            // (An error said why.)
            self.dupes = None;
            return;
        };
        let groups = find(&tracks);
        if groups.is_empty() {
            self.dupes = None;
            self.toast("No duplicates in this playlist.", false);
            return;
        }
        self.fonts.scan_all(
            ctx,
            groups.iter().flat_map(|g| &g.rows).flat_map(|&r| {
                let t = &tracks[r];
                [t.name.as_str(), t.album.as_str()].into_iter().chain(t.artists.iter().map(|a| a.name.as_str()))
            }),
        );
        if let Some(d) = self.dupes.as_mut() {
            d.found = Some(Found::new(tracks, &groups));
        }
    }

    /// The "Find duplicates" box (open while `dupes` is set): REMOVE <n> or CANCEL, Enter / Esc.
    pub(super) fn dupes_modal(&mut self, ctx: &egui::Context) {
        let p = self.palette;
        let Some(d) = self.dupes.as_mut() else { return };
        // Gone from the library, or no longer the user's to change: the box goes too.
        let Some(pl) = self.playlists.iter().find(|pl| pl.uri == d.uri && pl.editable) else {
            self.dupes = None;
            return;
        };
        let title = format!("Duplicates in \"{}\"", pl.name);
        match dupes_box(ctx, &p, &title, d) {
            Some(Answer::Remove(rows)) => {
                if let Some(Dupes { uri, found: Some(f) }) = self.dupes.take() {
                    self.send(Command::RemoveDuplicates { playlist_uri: uri, tracks: f.tracks, rows });
                }
            }
            Some(Answer::Cancel) => self.dupes = None,
            None => {}
        }
    }
}

/// How the box was answered.
#[derive(Debug, PartialEq)]
enum Answer {
    /// REMOVE: the places of the ticked copies.
    Remove(Vec<usize>),
    Cancel,
}

/// One pass of the box: `None` while it stays open.
fn dupes_box(ctx: &egui::Context, p: &Palette, title: &str, d: &mut Dupes) -> Option<Answer> {
    let mut answer = None;
    let modal = fin::dialog(ctx, Id::new("dupes"), |ui| {
        let screen = ui.ctx().content_rect();
        let w = 576.0_f32.min(screen.width() - 80.0).max(260.0);
        ui.set_width(w);
        // Enter is REMOVE's: taken before anything in the box can see it (while no other box is
        // over this one).
        let top = ui.ctx().memory(|m| m.top_modal_layer()).is_none_or(|l| l == ui.layer_id());
        let enter = top && ui.input_mut(|i| super::keys::take(i, Modifiers::NONE, Key::Enter));
        title_line(ui, title, w);
        let n = match d.found.as_mut() {
            None => {
                checking(ui, p);
                0
            }
            Some(f) => {
                ui.add_space(6.0);
                ui.add(egui::Label::new(egui::RichText::new(note(f.songs)).font(regular(12.0)).color(p.text_weak)).wrap());
                f.list(ui, p, w, (screen.height() - 240.0).clamp(90.0, 520.0));
                f.ticked
            }
        };
        let word = if d.found.is_some() { format!("Remove {}", fmt_thousands(n)) } else { "Remove".to_owned() };
        let (ok, no) = fin::dialog_actions(ui, p, Id::new("dupes"), &word, n > 0);
        if no {
            answer = Some(Answer::Cancel);
        } else if (ok || enter) && n > 0 {
            answer = d.found.as_ref().map(|f| Answer::Remove(f.picked()));
        }
    });
    if answer.is_none() && modal.should_close() {
        answer = Some(Answer::Cancel);
    }
    answer
}

/// The box's title on one line, cut with "…" when the playlist's name is long (whole on hover).
fn title_line(ui: &mut Ui, title: &str, w: f32) {
    let p = crate::ui::theme::palette(ui.ctx());
    let g = one_line(ui, title, bold(15.0), p.text_strong, w, false);
    let cut = g.elided;
    let (rect, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    ui.painter().galley(rect.min, g, p.text_strong);
    if cut {
        resp.on_hover_text(title);
    }
}

/// A spinner and "Checking…" while the playlist is read.
fn checking(ui: &mut Ui, p: &Palette) {
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(16.0).color(p.accent_ink));
        ui.label(egui::RichText::new("Checking…").font(regular(12.0)).color(p.text_weak));
    });
}

/// "3 songs are in here more than once. Ticked copies get removed."
fn note(songs: usize) -> String {
    let songs = if songs == 1 { "1 song is".to_owned() } else { format!("{} songs are", fmt_thousands(songs)) };
    format!("{songs} in here more than once. Ticked copies get removed.")
}

/// A song's name (bright) and its artists (muted) at the bottom of `rect`, under the line between
/// songs.
fn song_head(ui: &Ui, rect: Rect, line: usize, name: &str, artists: &str) {
    let p = crate::ui::theme::palette(ui.ctx());
    let painter = ui.painter();
    painter.hline(rect.x_range(), rect.top() + 16.5, Stroke::new(1.0, p.border));
    let mut job = LayoutJob::default();
    job.append(name, 0.0, TextFormat { font_id: bold(13.0), color: p.text_strong, ..Default::default() });
    if !artists.is_empty() {
        job.append(&format!(" · {artists}"), 0.0, TextFormat { font_id: regular(13.0), color: p.text_weak, ..Default::default() });
    }
    job.wrap = TextWrapping::truncate_at_width(rect.width());
    let g = painter.layout_job(job);
    let cut = g.elided;
    let text = Rect::from_min_size(pos2(rect.left(), rect.top() + 29.0), g.size());
    painter.galley(text.min, g, p.text_strong);
    if cut {
        ui.interact(text, Id::new(("dupe-song", line)), Sense::hover()).on_hover_text(format!("{name} · {artists}"));
    }
}

/// One copy (`.r`): its tick box, album, length, date added and, on the right, STAYS or OTHER
/// LENGTH. Returns true when clicked.
fn copy_row(ui: &Ui, p: &Palette, rect: Rect, c: &CopyRow) -> bool {
    let resp = ui.interact(rect, Id::new(("dupe-copy", c.row)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let painter = ui.painter();
    let mid = rect.center().y;
    // The tick box (`.cb`): the accent with a check while ticked.
    let tick = Rect::from_center_size(pos2(rect.left() + 7.5, mid), vec2(15.0, 15.0));
    if c.ticked {
        painter.rect_filled(tick, CornerRadius::same(4), p.accent);
        svgicon::paint(painter, tick.shrink(1.5), "check", p.on_accent, 3.4);
    } else {
        let edge = if resp.hovered() { p.text_weak } else { BOX_EDGE };
        painter.rect_stroke(tick, CornerRadius::same(4), Stroke::new(1.5, edge), StrokeKind::Inside);
    }
    // From the right: the label, the date (when there is room), the length; the album takes the rest.
    let mut right = rect.right();
    let label = if c.other_length {
        Some(("Other length", p.warn))
    } else if c.keep && !c.ticked {
        Some(("Stays", p.text_weak))
    } else {
        None
    };
    if let Some((word, color)) = label {
        let g = fin::spaced(ui, &word.to_uppercase(), bold(10.0), color, 0.6);
        painter.galley(pos2(right - g.size().x, mid - g.size().y / 2.0), g, color);
    }
    right -= LABEL_W + GAP;
    if rect.width() >= DATE_ROOM {
        painter.text(pos2(right, mid), Align2::RIGHT_CENTER, &c.added, regular(12.0), p.text_weak);
        right -= DATE_W + GAP;
    }
    painter.text(pos2(right, mid), Align2::RIGHT_CENTER, &c.length, regular(12.0), if c.other_length { p.warn } else { p.text_weak });
    right -= LENGTH_W + GAP;
    let left = tick.right() + GAP;
    let album = one_line(ui, &c.album, regular(12.0), if c.dim { p.text_weak } else { p.text }, right - left, false);
    let cut = album.elided;
    painter.galley(pos2(left, mid - album.size().y / 2.0), album, p.text);
    let resp = if cut { resp.on_hover_text(&c.album) } else { resp };
    resp.clicked()
}

// ------------------------------------------------------------------------------- matching

/// A song that is in a playlist more than once.
#[derive(Debug, PartialEq)]
struct Group {
    /// Where its copies are, first to last.
    rows: Vec<usize>,
    /// The copy that stays: the one added first.
    keep: usize,
}

impl Group {
    /// The copy at `row` is more than 5 seconds longer or shorter than the one that stays.
    fn other_length(&self, tracks: &[TrackInfo], row: usize) -> bool {
        (i64::from(tracks[row].duration_ms) - i64::from(tracks[self.keep].duration_ms)).abs() > LENGTH_SLACK_MS
    }

    /// Every copy but the one that stays starts ticked, except one of another length.
    fn ticked_at_start(&self, tracks: &[TrackInfo], row: usize) -> bool {
        row != self.keep && !self.other_length(tracks, row)
    }
}

/// The songs in `tracks` more than once, in the order they first show. The same song is the same
/// name (case, accents and spaces aside) by the same artists, in any order: "Remastered" or "Live"
/// in a name makes another song. One without artists (Spotify no longer has it) is only the same
/// as itself. Local files are left out. (`song_key` and `same_artists`, as adding to a playlist
/// checks too.)
fn find(tracks: &[TrackInfo]) -> Vec<Group> {
    // By name first; the few songs of the same name are told apart by their artists.
    let mut by_name: HashMap<SongKey, Vec<usize>> = HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, t) in tracks.iter().enumerate() {
        let Some(key) = song_key(t) else { continue };
        let named = by_name.entry(key).or_default();
        match named.iter().find(|&&g| same_artists(&tracks[groups[g][0]].artists, &t.artists)) {
            Some(&g) => groups[g].push(i),
            None => {
                named.push(groups.len());
                groups.push(vec![i]);
            }
        }
    }
    groups.into_iter().filter(|rows| rows.len() > 1).map(|rows| Group { keep: first_added(tracks, &rows), rows }).collect()
}

/// The copy added first: the earliest date, the earlier place on the same date or when a date
/// isn't known.
fn first_added(tracks: &[TrackInfo], rows: &[usize]) -> usize {
    let dated = rows.iter().all(|&r| tracks[r].added_at > 0);
    rows.iter().copied().min_by_key(|&r| (if dated { tracks[r].added_at } else { 0 }, r)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeMode;
    use crate::model::ArtistRef;
    use crate::ui::kit::headless::{key, move_to, pass, press, release};

    fn artist(name: &str, uri: &str) -> ArtistRef {
        ArtistRef { name: name.into(), uri: uri.into() }
    }

    /// A song `id` called `name` by `artists`, `secs` long, added at `added`.
    fn song(id: &str, name: &str, artists: &[ArtistRef], secs: u32, added: i64) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{id}"),
            name: name.into(),
            artists: artists.to_vec(),
            album: format!("Album {id}"),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: secs * 1000,
            playable: true,
            is_local: false,
            added_at: added,
        }
    }

    fn rows(tracks: &[TrackInfo]) -> Vec<Vec<usize>> {
        find(tracks).into_iter().map(|g| g.rows).collect()
    }

    #[test]
    fn the_same_song_is_found_whatever_its_case_accents_or_spaces() {
        let crue = [artist("Mötley Crüe", "spotify:artist:mc")];
        let tracks = [
            song("a", "Dr. Feelgood", &crue, 290, 100),
            song("b", "Kickstart My Heart", &crue, 282, 100),
            song("c", "dr.  feelgood ", &crue, 290, 200),
            song("d", "DR. FÉELGOOD", &crue, 291, 300),
        ];
        assert_eq!(rows(&tracks), [vec![0, 2, 3]]);
    }

    #[test]
    fn the_same_artists_in_any_order() {
        let (a, b) = (artist("Calvin Harris", "spotify:artist:ch"), artist("Dua Lipa", "spotify:artist:dl"));
        let tracks = [
            song("a", "One Kiss", &[a.clone(), b.clone()], 214, 1),
            song("b", "One Kiss", &[b.clone(), a.clone()], 214, 2),
            // Only one of them: another song.
            song("c", "One Kiss", std::slice::from_ref(&a), 214, 3),
            // A name without a link matches by name, accents aside.
            song("d", "One Kiss", &[artist("calvin harris", ""), artist("Dúa Lipa", "")], 214, 4),
        ];
        assert_eq!(rows(&tracks), [vec![0, 1, 3]]);
        // Two artists both linked: by link, whatever the name says.
        let tracks = [song("a", "Song", &[artist("Same", "spotify:artist:1")], 200, 1), song("b", "Song", &[artist("Same", "spotify:artist:2")], 200, 2)];
        assert!(rows(&tracks).is_empty());
    }

    #[test]
    fn a_single_and_the_album_version_and_one_song_twice() {
        let wanted = [artist("The Wanted", "spotify:artist:tw")];
        let mut single = song("single", "Glad You Came", &wanted, 198, 200);
        single.album = "Glad You Came".into();
        let tracks = [
            song("album", "Glad You Came", &wanted, 197, 100),
            song("other", "Chasing the Sun", &wanted, 199, 150),
            single,
            song("album", "Glad You Came", &wanted, 197, 300),
        ];
        let found = find(&tracks);
        assert_eq!(found, [Group { rows: vec![0, 2, 3], keep: 0 }]);
    }

    #[test]
    fn other_names_local_files_and_unknown_songs() {
        let a = [artist("Queen", "spotify:artist:q")];
        let mut local = song("x", "Bohemian Rhapsody", &a, 354, 0);
        local.uri = "spotify:local:Queen:A+Night:Bohemian+Rhapsody:354".into();
        local.is_local = true;
        let mut gone = song("gone", "Unavailable track", &[], 0, 0);
        gone.playable = false;
        let mut also_gone = gone.clone();
        also_gone.uri = "spotify:track:gone2".into();
        let mut greyed = song("grey", "Bohemian Rhapsody", &a, 354, 5);
        greyed.playable = false;
        let tracks = [
            song("a", "Bohemian Rhapsody", &a, 354, 1),
            song("b", "Bohemian Rhapsody - Remastered 2011", &a, 355, 2),
            song("c", "Bohemian Rhapsody - Live", &a, 360, 3),
            local,
            gone.clone(),
            also_gone,
            greyed,
            song("u", "Unique", &a, 100, 6),
            gone,
        ];
        // Remastered and Live are other songs; the local file is left out; a greyed copy counts;
        // songs Spotify no longer has only match themselves.
        assert_eq!(rows(&tracks), [vec![0, 6], vec![4, 8]]);
    }

    #[test]
    fn the_copy_added_first_stays() {
        let a = [artist("A", "spotify:artist:a")];
        let tracks = [song("1", "S", &a, 200, 300), song("2", "S", &a, 200, 100), song("3", "S", &a, 200, 200)];
        assert_eq!(find(&tracks)[0].keep, 1);
        // The same day: the earlier place.
        let tracks = [song("1", "S", &a, 200, 300), song("2", "S", &a, 200, 100), song("3", "S", &a, 200, 100)];
        assert_eq!(find(&tracks)[0].keep, 1);
        // A date not known: by place.
        let tracks = [song("1", "S", &a, 200, 300), song("2", "S", &a, 200, 0), song("3", "S", &a, 200, 100)];
        assert_eq!(find(&tracks)[0].keep, 0);
    }

    #[test]
    fn copies_of_another_length_start_unticked() {
        let a = [artist("A", "spotify:artist:a")];
        let tracks = [
            song("1", "S", &a, 200, 1),
            song("2", "S", &a, 205, 2),
            song("3", "S", &a, 194, 3),
            song("4", "S", &a, 341, 4),
        ];
        let g = &find(&tracks)[0];
        assert_eq!(g.keep, 0);
        // 5 seconds off is the same length; 6 or more is another.
        let other: Vec<bool> = g.rows.iter().map(|&r| g.other_length(&tracks, r)).collect();
        assert_eq!(other, [false, false, true, true]);
        // The one that stays and the other lengths start unticked; the rest ticked.
        let ticked: Vec<bool> = g.rows.iter().map(|&r| g.ticked_at_start(&tracks, r)).collect();
        assert_eq!(ticked, [false, true, false, false]);
        let found = Found::new(Arc::new(tracks.to_vec()), &find(&tracks));
        assert_eq!((found.ticked, found.picked()), (1, vec![1]));
    }

    #[test]
    fn lines_are_the_songs_then_their_copies() {
        let a = [artist("A", "spotify:artist:a")];
        let tracks = [song("1", "S", &a, 200, 2), song("2", "T", &a, 200, 1), song("3", "S", &a, 200, 1), song("4", "T", &a, 200, 3)];
        let found = Found::new(Arc::new(tracks.to_vec()), &find(&tracks));
        let shown: Vec<String> = found
            .lines
            .iter()
            .map(|l| match l {
                Line::Song { name, .. } => name.clone(),
                Line::Copy(c) => format!("{}{}", c.row, if c.keep { " stays" } else if c.ticked { " goes" } else { "" }),
            })
            .collect();
        assert_eq!(shown, ["S", "0 goes", "2 stays", "T", "1 stays", "3 goes"]);
        assert_eq!(found.tops, [0.0, HEAD_H, HEAD_H + ROW_H, HEAD_H + ROW_H * 2.0, HEAD_H * 2.0 + ROW_H * 2.0, HEAD_H * 2.0 + ROW_H * 3.0, HEAD_H * 2.0 + ROW_H * 4.0]);
    }

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        pass(&ctx, Vec::new(), |_| {});
        ctx
    }

    /// One pass of the box over a 600 x 400 window: how it was answered.
    fn answer(ctx: &egui::Context, events: Vec<egui::Event>, d: &mut Dupes) -> Option<Answer> {
        let p = Palette::build(ThemeMode::Dark, Color32::RED, None);
        let mut out = None;
        pass(ctx, events, |ui| out = dupes_box(ui.ctx(), &p, "Duplicates in \"Mix\"", d));
        out
    }

    /// A click on the copy at `row`.
    fn click(ctx: &egui::Context, row: usize, d: &mut Dupes) -> Option<Answer> {
        let at = ctx.read_response(Id::new(("dupe-copy", row))).expect("the copy is shown").rect.center();
        answer(ctx, move_to(at), d);
        answer(ctx, press(at), d);
        answer(ctx, release(at), d)
    }

    /// S twice (the second ticked), T twice (the second another length).
    fn found() -> Dupes {
        let a = [artist("A", "spotify:artist:a")];
        let tracks = [song("1", "S", &a, 200, 1), song("2", "S", &a, 200, 2), song("3", "T", &a, 200, 3), song("4", "T", &a, 400, 4)];
        Dupes { uri: "spotify:playlist:p".into(), found: Some(Found::new(Arc::new(tracks.to_vec()), &find(&tracks))) }
    }

    #[test]
    fn enter_removes_the_ticked_copies_and_esc_cancels() {
        let ctx = ctx();
        // While the playlist is read: Enter does nothing, Esc cancels.
        let mut d = Dupes { uri: "spotify:playlist:p".into(), found: None };
        assert_eq!(answer(&ctx, Vec::new(), &mut d), None);
        assert_eq!(answer(&ctx, key(Key::Enter), &mut d), None);
        assert_eq!(answer(&ctx, key(Key::Escape), &mut d), Some(Answer::Cancel));
        let mut d = found();
        assert_eq!(answer(&ctx, Vec::new(), &mut d), None);
        assert_eq!(answer(&ctx, key(Key::Enter), &mut d), Some(Answer::Remove(vec![1])));
        assert_eq!(answer(&ctx, key(Key::Escape), &mut d), Some(Answer::Cancel));
    }

    #[test]
    fn a_click_ticks_or_unticks_a_copy() {
        let ctx = ctx();
        let mut d = found();
        // The box takes a few passes to find its size and place.
        for _ in 0..3 {
            answer(&ctx, Vec::new(), &mut d);
        }
        // Nothing ticked: Enter does nothing.
        assert_eq!(click(&ctx, 1, &mut d), None);
        assert_eq!(answer(&ctx, key(Key::Enter), &mut d), None);
        // The copy that stays can go too.
        click(&ctx, 0, &mut d);
        click(&ctx, 1, &mut d);
        assert_eq!(answer(&ctx, key(Key::Enter), &mut d), Some(Answer::Remove(vec![0, 1])));
    }

    #[test]
    fn the_note_counts_songs() {
        assert_eq!(note(1), "1 song is in here more than once. Ticked copies get removed.");
        assert_eq!(note(3), "3 songs are in here more than once. Ticked copies get removed.");
        assert_eq!(note(1200), "1,200 songs are in here more than once. Ticked copies get removed.");
    }
}
