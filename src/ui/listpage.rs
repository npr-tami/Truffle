//! A list's page in the Final design (`pgPlaylist` and `pgAlbum`; the artist page adds its releases):
//! the hero (the sleeve with its record peeking out, the kind, the name, the details, the buttons and,
//! when wide, SHUFFLE and LEFT), the bar with the Songs and Blocked tabs and the filter, and the song
//! rows. The whole page scrolls; only the rows on screen are drawn, so a list of 12,000 songs scrolls
//! as easily as one of 12. Sizes are the prototype's CSS (`.hero`, `.tbar`, `.tbl`, `.th`, `.sr`).
//! Wide screens: the page keeps to its column; Split puts the hero in a column on the left that
//! stays in view, with SHUFFLE and LEFT as tiles, and the bar and the songs on the right.

use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{Align, Align2, Color32, CornerRadius, CursorIcon, Id, Popup, Rect, ScrollArea, Sense, Stroke, StrokeKind, TextFormat, Ui, pos2, vec2};

use super::banner::{BannerAct, banner_items, paint_banner};
use super::{HeaderInfo, Ink, PlayState, RowAction, TrackView, loading_progress, one_line, play_state};
use crate::config::WideScreens;
use crate::model::{Command, LikesWaiting, PlayStatus, Source, TrackInfo};
use crate::ui::dnd::SongsDrag;
use crate::ui::fin::{self, Look};
use crate::ui::motion;
use crate::ui::profile::{COUNT_H, count_tiles};
use crate::ui::sidebar::Sleeve;
use crate::ui::svgicon;
use crate::ui::theme::{Palette, bold, heavy, medium, regular};
use crate::ui::util::{fmt_added, fmt_ms, fmt_thousands, fmt_total};
use crate::ui::wide;
use crate::ui::{MiddleTab, TRACK_FILTER_ID, TruffleApp};

/// `.hero`: 22 above the sleeve, 18 below, 24 at the sides.
const PAD_X: f32 = 24.0;
const HERO_TOP: f32 = 22.0;
const HERO_BOTTOM: f32 = 18.0;
/// The back row above a narrow page's hero (`.from`).
const NARROW_BACK_H: f32 = 44.0;
/// Split: the hero's column.
const SPLIT_LEFT: f32 = 500.0;
/// Split: the cover, as big as keeps its record in the column while it plays (0.48 of the cover
/// out, 0.92 of it wide).
const SPLIT_COVER: f32 = 357.0;
/// `.tbar`, `.th` and `.sr` (34 on an album's page, which has no covers).
const TBAR_H: f32 = 40.0;
const TH_H: f32 = 30.0;
const ROW_H: f32 = 38.0;
const ALBUM_ROW_H: f32 = 34.0;
const COMPACT_ROW_H: f32 = 32.0;
/// `.tbl`'s padding: 4 above, 16 at the sides, 24 below; a row's own 8 at the sides and 10 between
/// its parts.
const TBL_TOP: f32 = 4.0;
const TBL_X: f32 = 16.0;
const TBL_BOTTOM: f32 = 24.0;
const ROW_PAD: f32 = 8.0;
const GAP: f32 = 10.0;
/// The columns that are always there: number, cover, time and heart.
const NUM_W: f32 = 28.0;
const COVER_W: f32 = 28.0;
const TIME_W: f32 = 40.0;
const HEART_W: f32 = 28.0;
const UNBLOCK_W: f32 = 58.0;

/// How big a song row's parts are (Settings > Song size).
#[derive(Clone, Copy)]
struct SongSize {
    title: f32,
    sub: f32,
    album: f32,
    /// The number and the time.
    small: f32,
    cover: f32,
    heart: f32,
}

impl SongSize {
    fn of(level: u8) -> Self {
        match level {
            0 => SongSize { title: 12.0, sub: 11.0, album: 11.5, small: 11.0, cover: COVER_W, heart: 13.0 },
            1 => SongSize { title: 14.5, sub: 12.5, album: 13.0, small: 12.5, cover: 40.0, heart: 15.0 },
            _ => SongSize { title: 18.0, sub: 15.0, album: 15.5, small: 15.0, cover: 56.0, heart: 18.0 },
        }
    }

    /// A row's height: the default sizes keep the compact choice.
    fn row_h(level: u8, album: bool, compact: bool) -> f32 {
        match (level, album, compact) {
            (0, _, true) => COMPACT_ROW_H,
            (0, true, false) => ALBUM_ROW_H,
            (0, false, false) => ROW_H,
            (1, true, _) => 46.0,
            (1, false, _) => 52.0,
            (_, true, _) => 60.0,
            _ => 70.0,
        }
    }
}

/// Which columns a row has.
#[derive(Clone, Copy)]
struct Cols {
    /// The number column's width (16 in an artist's Popular).
    num: f32,
    cover: bool,
    /// The artists under the title (not on an album's or artist's page).
    sub: bool,
    /// The album column's width; 0 when it is left out.
    album: f32,
    unblock: bool,
    /// The artist and Date added columns' widths (Settings); 0 when left out.
    artist: f32,
    added: f32,
    sz: SongSize,
}

/// Where each part of a row goes.
struct Parts {
    num: Rect,
    cover: Rect,
    text: Rect,
    album: Rect,
    artist: Rect,
    added: Rect,
    unblock: Rect,
    time: Rect,
    heart: Rect,
}

impl Cols {
    fn parts(self, row: Rect) -> Parts {
        let col = |x: f32, w: f32| Rect::from_min_max(pos2(x, row.top()), pos2(x + w, row.bottom()));
        let num = col(row.left() + ROW_PAD, self.num);
        let cover = col(num.right() + GAP, if self.cover { self.sz.cover } else { 0.0 });
        let heart = col(row.right() - ROW_PAD - HEART_W, HEART_W);
        let time = col(heart.left() - GAP - TIME_W, TIME_W);
        let unblock = if self.unblock { col(time.left() - GAP - UNBLOCK_W, UNBLOCK_W) } else { col(time.left(), 0.0) };
        let added = if self.added > 0.0 { col(unblock.left() - GAP - self.added, self.added) } else { col(unblock.left(), 0.0) };
        let album = if self.album > 0.0 { col(added.left() - GAP - self.album, self.album) } else { col(added.left(), 0.0) };
        let artist = if self.artist > 0.0 { col(album.left() - GAP - self.artist, self.artist) } else { col(album.left(), 0.0) };
        let text_left = if self.cover { cover.right() + GAP } else { num.right() + GAP };
        let text = Rect::from_min_max(pos2(text_left, row.top()), pos2((artist.left() - GAP).max(text_left + 20.0), row.bottom()));
        Parts { num, cover, text, album, artist, added, unblock, time, heart }
    }
}

/// A caption in the table's head (`.th`: 10 px semibold capitals spaced .08em).
fn head_caps(ui: &Ui, text: &str) -> Arc<egui::Galley> {
    let p = crate::ui::theme::palette(ui.ctx());
    fin::spaced(ui, &text.to_uppercase(), bold(10.0), p.text_weak, 0.8)
}

/// One line of `font` text in `color`, cut with "…" at `width`, spaced by `spacing`.
fn spaced_line(ui: &Ui, text: &str, font: egui::FontId, color: Color32, width: f32, spacing: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat { font_id: font, color, extra_letter_spacing: spacing, ..Default::default() });
    job.wrap = TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// "<b>owner</b> · 1,001 songs · 38 h 55 min" (`.hero .meta`), cut to `width`.
fn meta_line(ui: &Ui, owner: &str, rest: &str, ink: Ink, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::default();
    if !owner.is_empty() {
        job.append(owner, 0.0, TextFormat { font_id: bold(12.0), color: ink.text, ..Default::default() });
        if !rest.is_empty() {
            job.append(" · ", 0.0, TextFormat { font_id: regular(12.0), color: ink.weak, ..Default::default() });
        }
    }
    job.append(rest, 0.0, TextFormat { font_id: regular(12.0), color: ink.weak, ..Default::default() });
    job.wrap = TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// The numbers of a list's meta line: its songs, their length, and (Liked Songs) the likes still
/// on their way to Spotify.
type HeroNumbers = (Option<usize>, u64, LikesWaiting);

/// The hover tip of the likes waiting.
const WAITING_TIP: &str = "Spotify is slowing likes down. Truffle keeps sending them.";

/// The details after the owner: "1,001 songs · 38 h 55 min", an artist's "10 popular songs",
/// Liked Songs' "36,864 songs · 1,240 waiting" (no length when `tight`).
fn meta_rest(source: &Source, (count, ms, waiting): HeroNumbers, tight: bool) -> String {
    let songs = count.map(|n| match source {
        Source::Artist(_) => format!("{n} popular song{}", if n == 1 { "" } else { "s" }),
        _ => format!("{} song{}", fmt_thousands(n), if n == 1 { "" } else { "s" }),
    });
    let waiting = Some(waiting_text(waiting)).filter(|w| !w.is_empty());
    songs.into_iter().chain(waiting).chain((ms > 0 && !tight).then(|| fmt_total(ms))).collect::<Vec<_>>().join(" · ")
}

/// "1,240 waiting · 12 to remove"; nothing when none wait.
fn waiting_text(waiting: LikesWaiting) -> String {
    let likes = (waiting.likes > 0).then(|| format!("{} waiting", fmt_thousands(waiting.likes)));
    let unlikes = (waiting.unlikes > 0).then(|| format!("{} to remove", fmt_thousands(waiting.unlikes)));
    likes.into_iter().chain(unlikes).collect::<Vec<_>>().join(" · ")
}

/// The songs a page shows: the filtered list, or (Blocked tab) the blocked ones among them.
fn shown_rows(filtered: &[u32], tracks: &[TrackInfo], blocked: &std::collections::HashSet<String>, blocked_tab: bool) -> Vec<u32> {
    if blocked_tab {
        filtered.iter().copied().filter(|&i| blocked.contains(&tracks[i as usize].uri)).collect()
    } else {
        filtered.to_vec()
    }
}

/// "carwash · 2019" into the artist and the year.
fn split_year(subtitle: &str) -> (&str, Option<&str>) {
    match subtitle.rsplit_once(" · ") {
        Some((who, year)) if year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()) => (who, Some(year)),
        _ => (subtitle, None),
    }
}

/// The rows (first, past the last) that show between `top` and `bottom`, for rows from `rows_top`.
fn visible_rows(rows_top: f32, row_h: f32, n: usize, top: f32, bottom: f32) -> (usize, usize) {
    let first = ((top - rows_top) / row_h).floor().max(0.0) as usize;
    let last = (((bottom - rows_top) / row_h).ceil().max(0.0) as usize).min(n);
    (first.min(n), last)
}

/// Where a list's page goes across the middle area (Wide screens, see `wide::Column`): the hero's
/// words from `hero.0` to `hero.1`, and the bar and the songs as a page of their own from `songs.0`,
/// `songs.1` wide (their content in from both sides by the page's padding, as a page filling the
/// middle area has it). Split: the hero is a column on the left of the songs.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Across {
    hero: (f32, f32),
    songs: (f32, f32),
    split: bool,
}

impl Across {
    /// The page in the middle area from `x`, `w` wide, with `pad` at its sides; `two`: it may split.
    fn new(wide: WideScreens, x: f32, w: f32, pad: f32, two: bool) -> Across {
        let col = wide::Column::new(wide, x, w, pad, two);
        if col.split {
            let ((left, left_w), (right, right_w)) = col.halves(SPLIT_LEFT);
            Across { hero: (left, left + left_w), songs: (right - pad, right_w + pad * 2.0), split: true }
        } else {
            Across { hero: (col.left, col.right()), songs: (col.left - pad, (col.width + pad * 2.0).min(w)), split: false }
        }
    }
}

/// Split: where the hero's column starts, for a page from `top` with `view` of it on screen: in
/// view however far the songs scroll; one taller than the window scrolls until its foot shows.
fn hero_top(top: f32, view: egui::Rangef, h: f32) -> f32 {
    top.max(view.min.min(view.max - h))
}

/// Split: the hero's column's height: the cover (`cs`); 24 under it the kind (`kind_h` tall), the
/// name and the details; 18 under them the buttons; 22 under those the shuffle numbers' `tiles`,
/// two to a row; 18 at the foot.
fn split_hero_h(cs: f32, kind_h: f32, tiles: usize) -> f32 {
    let words = 24.0 + kind_h + 4.0 + 31.5 + 6.0 + 19.0;
    let tiles = if tiles == 0 { 0.0 } else { 22.0 + tiles.div_ceil(2) as f32 * (COUNT_H + 10.0) - 10.0 };
    HERO_TOP + cs + words + 18.0 + 28.0 + tiles + HERO_BOTTOM
}

/// SHUFFLE or LEFT: its name, its number and the words after it ("of 890", "· 7 h 18 min").
type Stat = (&'static str, usize, String);

/// Split: what a SHUFFLE or LEFT tile says under its number: "of 890 in this shuffle", "left ·
/// 7 h 18 min".
fn tile_words(label: &str, small: &str) -> String {
    match label {
        "SHUFFLE" => format!("{small} in this shuffle"),
        _ if small.is_empty() => "left".to_owned(),
        _ => format!("left {small}"),
    }
}

/// The id of the hero's own background, under its parts: it has the hero's right-click menu, and
/// a plain click on it is a click on nothing.
const HERO_BACK: &str = "list-hero-menu";

/// Whether this pass has a plain left click (no Ctrl or Shift) in `area` on nothing that takes
/// clicks (below the songs, beside them, on the column names, the hero's own background), with no
/// menu or box over it there.
fn clicked_nothing(ui: &Ui, area: Rect) -> bool {
    let plain = ui.input(|i| i.pointer.primary_clicked() && !i.modifiers.command && !i.modifiers.shift);
    plain && ui.rect_contains_pointer(area) && ui.ctx().interaction_snapshot(|s| s.clicked.is_none_or(|id| id == Id::new(HERO_BACK)))
}

/// A click on nothing on the page (`ui`, in its scroll area) lets go of the highlights, as Esc does.
fn let_go_on_nothing(ui: &Ui, view: Option<&mut TrackView>) {
    if let Some(v) = view
        && clicked_nothing(ui, ui.clip_rect())
    {
        v.let_go();
    }
}

/// The list's keys (see `list_keys`) on `v`, the songs `rows` of `count`: the song Enter plays.
fn song_keys(ui: &Ui, v: &mut TrackView, rows: &[u32], count: usize) -> Option<usize> {
    use egui::{Key, Modifiers};
    let (all, shift_up, shift_down, up, down, enter, esc) = ui.input_mut(|i| {
        (
            crate::ui::keys::take(i, Modifiers::COMMAND, Key::A),
            crate::ui::keys::take(i, Modifiers::SHIFT, Key::ArrowUp),
            crate::ui::keys::take(i, Modifiers::SHIFT, Key::ArrowDown),
            crate::ui::keys::take(i, Modifiers::NONE, Key::ArrowUp),
            crate::ui::keys::take(i, Modifiers::NONE, Key::ArrowDown),
            crate::ui::keys::take(i, Modifiers::NONE, Key::Enter),
            crate::ui::keys::pressed(i, Modifiers::NONE, Key::Escape),
        )
    });
    if all {
        v.picked = rows.iter().map(|&r| r as usize).collect();
        v.anchor = rows.first().map(|&r| r as usize);
        return None;
    }
    // Esc clears every highlight: the songs picked and the one the keys move from.
    if esc && (v.selected.is_some() || !v.picked.is_empty()) {
        v.let_go();
        return None;
    }
    let step: i64 = if up || shift_up { -1 } else if down || shift_down { 1 } else { 0 };
    if step != 0 {
        let at = v.selected.and_then(|s| rows.iter().position(|&r| r as usize == s));
        let next = match at {
            Some(p) => (p as i64 + step).clamp(0, rows.len() as i64 - 1) as usize,
            None => 0,
        };
        let ti = rows[next] as usize;
        if shift_up || shift_down {
            let anchor = v.anchor.or(v.selected).unwrap_or(ti);
            let (picked, _) = super::pick(&v.picked, Some(anchor), rows, ti, false, true);
            v.picked = picked;
            v.anchor = Some(anchor);
        } else {
            v.picked = std::iter::once(ti).collect();
            v.anchor = Some(ti);
        }
        v.selected = Some(ti);
        v.scroll_to_row = Some(next);
        return None;
    }
    v.selected.filter(|&s| enter && s < count && rows.contains(&(s as u32)))
}

impl TruffleApp {
    /// A list's page, filling `ui`.
    pub(super) fn list_page(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let Some(source) = self.view.as_ref().map(|v| v.source.clone()) else { return };
        let mut info = self.header_info(&source);
        // Narrow window: a back row on top, and the buttons under the hero (`.hero.nh`, `.nacts`).
        let narrow = ui.ctx().content_rect().width() < crate::ui::frame::NARROW;
        let album = matches!(source, Source::Album(_));
        let artist = matches!(source, Source::Artist(_));
        let blocked_tab = self.middle_tab == MiddleTab::Blocked;
        let banner = self.banner_for(ui.ctx(), &source, &info.title);
        info.on_banner = banner.is_some();
        // Across the middle area as Wide screens has it (a song list may split, an artist's page
        // doesn't); the sizes below go by the room the songs have.
        let area = ui.max_rect();
        let at = Across::new(self.prefs.wide_screens, area.left(), area.width(), if narrow { 16.0 } else { PAD_X }, !artist);
        let mw = at.songs.1;
        let cs = match (album, mw) {
            _ if at.split => SPLIT_COVER,
            _ if narrow => {
                if artist { 96.0 } else { 120.0 }
            }
            _ if artist => {
                if mw >= 600.0 { 120.0 } else { 96.0 }
            }
            (true, w) if w >= 600.0 => 132.0,
            (true, _) => 110.0,
            (false, w) if w >= 760.0 => 132.0,
            (false, w) if w >= 600.0 => 120.0,
            _ => 96.0,
        };
        // SHUFFLE and LEFT on a playlist's page, when there is room for them (tiles when split).
        let here = self.playback.track.is_some() && self.playback.source.as_ref() == Some(&source);
        let stats = if !album && !artist && (at.split || mw >= 760.0) { self.hero_stats(&source, here) } else { Vec::new() };
        // An artist's hero has 16 below (`pgArtist`).
        let hero_h = if at.split {
            split_hero_h(cs, ui.fonts_mut(|f| f.row_height(&bold(10.5))), stats.len())
        } else if narrow {
            NARROW_BACK_H + 4.0 + cs + 12.0 + 28.0 + 10.0
        } else {
            HERO_TOP + cs + if artist { 16.0 } else { HERO_BOTTOM }
        };
        let scroll_to = self.view.as_mut().and_then(|v| v.scroll_to_row.take());
        // Compact lists (Settings): shorter rows.
        let level = self.prefs.song_size.min(2);
        let row_h = SongSize::row_h(level, album, self.prefs.compact);
        // The artist and Date added columns (Settings), when there is room for them.
        let more = self.prefs.more_columns && !album && !narrow && mw >= 900.0;
        let cols = Cols {
            num: NUM_W,
            cover: !album,
            sub: !album && !more,
            album: if album || mw < 560.0 { 0.0 } else if mw < 760.0 { 130.0 } else if more && mw < 1200.0 { 170.0 } else { 190.0 },
            unblock: blocked_tab,
            artist: if !more { 0.0 } else if mw >= 1200.0 { 200.0 } else { 160.0 },
            added: if more && mw >= 1000.0 { 96.0 } else { 0.0 },
            sz: SongSize::of(level),
        };

        ScrollArea::vertical().id_salt(("list-page", source.cache_key())).auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let_go_on_nothing(ui, self.view.as_mut());
            let top = ui.cursor().top();
            let (left, w) = at.songs;
            let hero = if at.split {
                // A column on the left of the songs that stays in view as they scroll; the page is
                // at least as tall as it.
                ui.expand_to_include_rect(Rect::from_min_max(pos2(area.left(), top), pos2(left, top + hero_h)));
                let y = hero_top(top, ui.clip_rect().y_range(), hero_h);
                Rect::from_min_max(pos2(area.left(), y), pos2(left, y + hero_h))
            } else {
                let hero = Rect::from_min_size(pos2(area.left(), top), vec2(area.width(), hero_h));
                ui.advance_cursor_after_rect(hero);
                hero
            };
            if let Some(b) = banner {
                paint_banner(ui.painter(), &p, hero, hero_h, 0.0, b);
                // The editor shows the banner in this shape.
                self.wallpaper.banners.frame = hero.size();
            }
            self.list_hero(ui, hero, at, cs, &info, &source, narrow, &stats);
            if artist {
                self.artist_body(ui, left, w, mw, &source);
                return;
            }
            // An album's page has no tabs or filter: its songs follow the hero (start the songs'
            // side when split).
            if !album {
                let bar = Rect::from_min_size(pos2(left, ui.cursor().top()), vec2(w, TBAR_H));
                ui.advance_cursor_after_rect(bar);
                self.list_tbar(ui, bar, mw, if narrow { 16.0 } else { PAD_X });
            }
            let tbl_x = if narrow { 8.0 } else { TBL_X };
            // Songs that have just come fade in.
            let was = if self.view.as_ref().is_some_and(|v| v.tracks.is_some()) {
                motion::content_in(ui, Id::new(("list-songs", source.cache_key())))
            } else {
                ui.opacity()
            };
            self.list_songs(ui, left, w, tbl_x, cols, row_h, album, blocked_tab, scroll_to, &source);
            ui.set_opacity(was);
        });
    }

    /// The hero: the sleeve (or an artist's round picture), and beside it, standing on its foot, the
    /// kind, the name, the details, the buttons and `stats`; its words within `at.hero`. Split: they
    /// go under the sleeve, and `stats` as tiles under the buttons. Right-click: the banner.
    #[allow(clippy::too_many_arguments)]
    fn list_hero(&mut self, ui: &mut Ui, hero: Rect, at: Across, cs: f32, info: &HeaderInfo, source: &Source, narrow: bool, stats: &[Stat]) {
        let mw = at.songs.1;
        // A banner's dark shade lies under the hero in both themes.
        let p = if info.on_banner { self.palette.on_dark() } else { self.palette };
        let ink = Ink::of(&p, info);
        // Under everything in the hero, so its parts keep their own clicks.
        let menu = ui.interact(hero, Id::new(HERO_BACK), Sense::click());
        let here = self.playback.track.is_some() && self.playback.source.as_ref() == Some(source);
        if narrow {
            // The back row (`.from`): back, and the page it goes back to.
            let back = Rect::from_min_size(pos2(hero.left() + 10.0, hero.top() + 7.0), vec2(30.0, 30.0));
            if fin::ib(ui, &p, back, Id::new("list-back"), "back", Look::Plain, "Back").clicked() {
                self.go_back();
            }
            ui.painter().text(pos2(back.right() + 6.0, back.center().y), Align2::LEFT_CENTER, self.back_label(), medium(13.0), p.text);
        }
        let cover = if narrow {
            Rect::from_min_size(pos2(hero.left() + 18.0, hero.top() + NARROW_BACK_H + 4.0), vec2(cs, cs))
        } else {
            Rect::from_min_size(pos2(at.hero.0, hero.top() + HERO_TOP), vec2(cs, cs))
        };
        let editable = info.editable_uri.clone();
        let cover_resp = ui.interact(cover, Id::new("list-hero-cover"), if editable.is_some() { Sense::click() } else { Sense::hover() });
        let beside = if info.round {
            let r = (cs / 2.0) as u8;
            if !self.images.paint(ui, info.image_url.as_deref(), cover, r, p.widget_bg) {
                svgicon::paint(ui.painter(), cover.shrink(cs * 0.3), "artist", p.text_weak, svgicon::STROKE);
            }
            cover.right() + if narrow { 18.0 } else { 24.0 }
        } else {
            let sleeve = Sleeve {
                source: source.clone(),
                name: info.title.clone(),
                image: info.image_url.clone(),
                count: None,
                uri: None,
                editable: false,
            };
            self.paint_sleeve(ui, cover, &sleeve, here, cover_resp.hovered(), 0.20);
            cover.right() + cs * if here { 0.42 } else { 0.24 } + if narrow { 18.0 } else { 24.0 }
        };
        let x = if at.split { cover.left() } else { beside };
        // The user's own playlist: its cover changes on a click.
        if let Some(uri) = editable {
            if cover_resp.hovered() {
                let badge = Rect::from_min_size(cover.right_top() + vec2(-28.0, 6.0), vec2(22.0, 22.0));
                ui.painter().rect_filled(badge, CornerRadius::same(5), Color32::from_black_alpha(170));
                svgicon::paint(ui.painter(), badge.shrink(4.0), "pen", Color32::WHITE, svgicon::STROKE);
            }
            if cover_resp.on_hover_text("Change cover").on_hover_cursor(CursorIcon::PointingHand).clicked() {
                let ctx = ui.ctx().clone();
                self.pick_cover(&ctx, uri);
            }
        }

        // From the foot up: the buttons, the details, the name, the kind. Narrow: the words are
        // centred on the sleeve (the name 24) and the buttons go under it. Split: from the top, 24
        // under the sleeve.
        let right = at.hero.1;
        let tw = (right - x).max(60.0);
        let (title_size, title_line) = if narrow { (24.0, 25.2) } else { (30.0, 31.5) };
        // The owner, then the rest: its numbers roll when they change (a like on Liked Songs).
        let (owner, numbers) = self.hero_meta(info, source);
        let meta = meta_line(ui, &owner, "", ink, tw);
        let meta_w = meta.size().x;
        let rest = |v, ink| {
            let rest = meta_rest(source, v, mw < 560.0);
            // After a long owner cut short, nothing.
            let rest = if owner.is_empty() || rest.is_empty() { rest } else if tw - meta_w < 12.0 { String::new() } else { format!(" · {rest}") };
            meta_line(ui, "", &rest, ink, (tw - meta_w).max(1.0))
        };
        let line_h = meta.size().y.max(rest(numbers, ink).size().y);
        let title = spaced_line(ui, &info.title, heavy(title_size), ink.strong, tw, -0.02 * title_size);
        let kind = fin::spaced(ui, &self.hero_kind(info, source).to_uppercase(), bold(10.5), ink.weak, 1.05);
        let words_h = kind.size().y + 4.0 + title_line + 6.0 + 19.0;
        let (acts_at, meta_top) = if narrow {
            let top = cover.center().y - words_h / 2.0;
            (pos2(hero.left() + 16.0, cover.bottom() + 12.0), top + kind.size().y + 4.0 + title_line + 6.0)
        } else if at.split {
            let meta_top = cover.bottom() + 24.0 + kind.size().y + 4.0 + title_line + 6.0;
            (pos2(x, meta_top + 19.0 + 18.0), meta_top)
        } else {
            let acts_top = cover.bottom() - 2.0 - 28.0;
            (pos2(x, acts_top), acts_top - 14.0 - 19.0)
        };
        let title_top = meta_top - 6.0 - title_line - (title.size().y - title_line) / 2.0;
        let kind_top = meta_top - 6.0 - title_line - 4.0 - kind.size().y;
        let painter = ui.painter();
        ink.paint(painter, pos2(x, kind_top), kind, ink.weak);
        let title_rect = Rect::from_min_size(pos2(x, title_top), title.size());
        let elided = title.elided;
        ink.paint(painter, title_rect.min, title, ink.strong);
        if elided {
            ui.interact(title_rect, Id::new("list-hero-title"), Sense::hover()).on_hover_text(&info.title);
        }
        let meta_at = pos2(x, meta_top + (19.0 - line_h) / 2.0 - 1.0);
        ink.paint(painter, meta_at, meta, ink.text);
        let (id, rest_at) = (Id::new(("list-meta", source.cache_key())), meta_at + vec2(meta_w, 0.0));
        if ink.shadow {
            let dark = Color32::from_black_alpha(150);
            let shade = Ink { strong: dark, text: dark, weak: dark, shadow: false };
            motion::count(painter, id, numbers, rest_at + vec2(0.0, 1.0), Align2::LEFT_TOP, |v| rest(v, shade));
        }
        let rest_rect = motion::count(painter, id, numbers, rest_at, Align2::LEFT_TOP, |v| rest(v, ink));
        if !numbers.2.is_empty() {
            ui.interact(rest_rect, id.with("waiting"), Sense::hover()).on_hover_text(WAITING_TIP);
        }
        if let Some(link) = self.hero_links(ui, info, source, meta_at, meta_w) {
            match link {
                MetaLink::Artist(uri) => self.open_source(Source::Artist(uri), false),
                MetaLink::Owner(user) => self.show_user(&user),
            }
        }

        let act = self.hero_acts(ui, &p, acts_at, right, source, mw, here, if at.split { &[] } else { stats });
        if at.split && !stats.is_empty() {
            let words: Vec<String> = stats.iter().map(|(label, _, small)| tile_words(label, small)).collect();
            let tiles: Vec<(usize, &str, Option<bool>)> = stats.iter().zip(&words).map(|((_, n, _), w)| (*n, w.as_str(), None)).collect();
            count_tiles(ui, &p, x, right - x, acts_at.y + 28.0 + 22.0, &tiles);
        }
        self.header_menu(&menu, source);
        if let Some(act) = act {
            self.apply_hero_act(ui, source, act);
        }
    }

    /// The names at the start of the meta line, each a link: an album's artists, a playlist's owner.
    /// They are laid out as the line has them (bold 12, ", " between) and stop where it was cut.
    fn hero_links(&self, ui: &Ui, info: &HeaderInfo, source: &Source, at: egui::Pos2, width: f32) -> Option<MetaLink> {
        let ink = Ink::of(&self.palette, info);
        let links: Vec<(String, MetaLink)> = match source {
            Source::Album(_) => self
                .source_info
                .get(source)
                .map(|i| i.artists.iter().filter(|a| !a.uri.is_empty()).map(|a| (a.name.clone(), MetaLink::Artist(a.uri.clone()))).collect())
                .unwrap_or_default(),
            Source::Playlist(uri) => self
                .playlist(uri)
                .filter(|p| !p.owner_id.is_empty() && !info.owner.is_empty())
                .map(|p| vec![(info.owner.clone(), MetaLink::Owner(p.owner_id.clone()))])
                .unwrap_or_default(),
            Source::LikedSongs => self
                .profile
                .as_ref()
                .filter(|_| !info.owner.is_empty())
                .map(|me| vec![(info.owner.clone(), MetaLink::Owner(me.username.clone()))])
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut x = at.x;
        let mut picked = None;
        let comma = ui.painter().layout_no_wrap(", ".to_owned(), bold(12.0), Color32::WHITE).size().x;
        for (k, (name, link)) in links.into_iter().enumerate() {
            let g = ui.painter().layout_no_wrap(name, bold(12.0), Color32::WHITE);
            let end = (x + g.size().x).min(at.x + width);
            if end <= x {
                break;
            }
            let rect = Rect::from_min_max(pos2(x, at.y), pos2(end, at.y + g.size().y));
            let resp = ui.interact(rect, Id::new(("hero-link", k)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if resp.hovered() {
                ui.painter().hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(1.0, ink.text));
            }
            if resp.clicked() {
                picked = Some(link);
            }
            x += g.size().x + comma;
        }
        picked
    }

    /// The kind line: "Playlist · pinned", "Album", "Artist · following".
    fn hero_kind(&self, info: &HeaderInfo, source: &Source) -> String {
        match source {
            Source::Album(_) => {
                let songs = self.view.as_ref().and_then(|v| v.tracks.as_ref()).map_or(0, |t| t.len());
                let kind = if songs > 0 && songs <= 3 { "Single" } else { "Album" };
                match split_year(&info.owner) {
                    (_, Some(year)) => format!("{kind} · {year}"),
                    _ => kind.to_owned(),
                }
            }
            Source::LikedSongs if self.prefs.liked_pinned => "Playlist · pinned".to_owned(),
            Source::LikedSongs => "Playlist".to_owned(),
            Source::Playlist(uri) if self.prefs.pinned_playlists.contains(uri) => "Playlist · pinned".to_owned(),
            Source::Playlist(_) if info.editable_uri.is_none() && !info.owner.is_empty() => "Playlist · shared".to_owned(),
            Source::Artist(uri) if self.followed.contains(uri) => "Artist · following".to_owned(),
            _ => info.kind.to_owned(),
        }
    }

    /// The owner at the start of the meta line, and the numbers after it (see `meta_rest`): the
    /// songs and their length.
    fn hero_meta(&self, info: &HeaderInfo, source: &Source) -> (String, HeroNumbers) {
        let view = self.view.as_ref().filter(|v| v.source == *source);
        let loaded = view.and_then(|v| v.tracks.as_ref().map(|t| (t.len(), v.total_ms)));
        let count = match source {
            // Every like counts at once, the ones still on their way to Spotify too.
            Source::LikedSongs if !self.liked.is_empty() => Some(self.liked.len()),
            _ => loaded.map(|(n, _)| n).or(info.fallback_count.map(|n| n as usize)),
        };
        let ms = loaded.map_or(0, |(_, ms)| ms);
        let waiting = if *source == Source::LikedSongs { self.likes_waiting } else { LikesWaiting::default() };
        let owner = match source {
            // "carwash · 2019": the year is on the kind line.
            Source::Album(_) => split_year(&info.owner).0.to_owned(),
            Source::Artist(_) => String::new(),
            _ => info.owner.clone(),
        };
        (owner, (count, ms, waiting))
    }

    /// PLAY / PAUSE, RESHUFFLE, pin, edit and more (`.hero .acts`), then `stats` (SHUFFLE and LEFT)
    /// when wide. Returns what was asked for.
    #[allow(clippy::too_many_arguments)]
    fn hero_acts(&mut self, ui: &mut Ui, p: &Palette, at: egui::Pos2, right: f32, source: &Source, mw: f32, here: bool, stats: &[Stat]) -> Option<HeroAct> {
        let now = self.list_now(source);
        let state = play_state(&now);
        let can_play = now.songs.is_some_and(|n| n > 0) || now.playing || now.paused;
        let loading = matches!(state, PlayState::Loading { .. });
        let mut act = None;
        let (word, _) = state.label();
        let icon = if state == PlayState::Pause { "pause" } else { "play" };
        let browse = matches!(source, Source::Album(_) | Source::Artist(_));
        // Follow, when it didn't fit beside the buttons (`Some(followed)`).
        let mut follow_in_more: Option<bool> = None;
        let mut x = at.x;
        // An album or artist plays in order from its Play; its shuffle button shuffles it.
        let fresh = browse && state == PlayState::Play;
        let tip = if fresh { "Play from the first song".to_owned() } else { state.tip() };
        let play = fin::text_button(ui, p, pos2(x, at.y), Id::new("hero-play"), word, Some(icon), true, can_play && !loading).on_hover_text(tip);
        if play.clicked() {
            act = Some(if fresh { HeroAct::InOrder } else { HeroAct::Play(state) });
        }
        x = play.rect.right() + 8.0;
        let icon_at = |x: f32| Rect::from_min_size(pos2(x, at.y - 1.0), vec2(30.0, 30.0));
        if browse {
            let look = if can_play { Look::Plain } else { Look::Dim };
            if fin::ib(ui, p, icon_at(x), Id::new("hero-shuffle"), "shuffle", look, "Shuffle play").clicked() {
                act = Some(HeroAct::NewRound);
            }
            x += 30.0 + 8.0;
            // An artist: FOLLOW, or FOLLOWING with a check (once Truffle knows who is followed), when
            // there is room for it and More after it; else it is in More.
            if let Source::Artist(uri) = source
                && self.following.is_some()
            {
                let on = self.followed.contains(uri);
                let (word, icon, tip) = if on { ("Following", "check", "Unfollow") } else { ("Follow", "plus", "Follow on Spotify") };
                if x + fin::text_button_w(ui, word, true) + 8.0 + 30.0 <= right {
                    let b = fin::text_button(ui, p, pos2(x, at.y), Id::new("hero-follow"), word, Some(icon), false, true).on_hover_text(tip);
                    if b.clicked() {
                        act = Some(HeroAct::Follow(!on));
                    }
                    x = b.rect.right() + 8.0;
                } else {
                    follow_in_more = Some(on);
                }
            }
            if matches!(source, Source::Album(_)) {
                let add = fin::ib(ui, p, icon_at(x), Id::new("hero-add"), "plus", Look::Plain, "Add to playlist");
                let mine: Vec<(String, String)> = self.playlists.iter().filter(|pl| pl.editable).map(|pl| (pl.uri.clone(), pl.name.clone())).collect();
                Popup::menu(&add).show(|ui| {
                    ui.set_width(214.0);
                    fin::menu_label(ui, "Add every song to");
                    if mine.is_empty() {
                        fin::menu_item(ui, None, "No playlists of yours yet", None, false, false);
                        return;
                    }
                    ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (uri, name) in &mine {
                            if fin::menu_item(ui, Some("lib"), name, None, false, true).clicked() {
                                act = Some(HeroAct::AddAll(uri.clone()));
                                ui.close();
                            }
                        }
                    });
                });
                x += 30.0 + 8.0;
            }
        } else {
            let tip = if here { "Reshuffle the songs still to come (R)" } else { "Play in a fresh random order" };
            let re = fin::text_button(ui, p, pos2(x, at.y), Id::new("hero-reshuffle"), "Reshuffle", Some("refresh"), false, can_play && !loading).on_hover_text(tip);
            if re.clicked() {
                act = Some(HeroAct::Reshuffle);
            }
            x = re.rect.right() + 8.0;
        }
        let pinned = match source {
            Source::LikedSongs => Some(self.prefs.liked_pinned),
            Source::Playlist(uri) => Some(self.prefs.pinned_playlists.contains(uri)),
            _ => None,
        };
        if let Some(pinned) = pinned.filter(|_| mw >= 560.0) {
            let tip = if pinned { "Unpin" } else { "Pin to the top" };
            if fin::ib(ui, p, icon_at(x), Id::new("hero-pin"), "pin", Look::on(pinned), tip).clicked() {
                act = Some(HeroAct::Pin(!pinned));
            }
            x += 30.0 + 8.0;
        }
        let editable = match source {
            Source::Playlist(uri) => self.playlist(uri).is_some_and(|pl| pl.editable).then(|| uri.clone()),
            _ => None,
        };
        let dupes_of = editable.clone();
        if let Some(uri) = editable.filter(|_| mw >= 760.0) {
            if fin::ib(ui, p, icon_at(x), Id::new("hero-pen"), "pen", Look::Plain, "Edit name and description").clicked() {
                act = Some(HeroAct::Edit(uri));
            }
            x += 30.0 + 8.0;
        }
        let more = fin::ib(ui, p, icon_at(x), Id::new("hero-more"), "more", Look::Plain, "More");
        x += 30.0;
        let banner_set = self.prefs.banners.contains_key(&source.cache_key());
        let picking = self.wallpaper.banners.picking();
        Popup::menu(&more).show(|ui| {
            ui.set_width(220.0);
            if let Some(on) = follow_in_more {
                let (icon, word) = if on { ("x", "Unfollow") } else { ("plus", "Follow") };
                if fin::menu_item(ui, Some(icon), word, None, false, true).clicked() {
                    act = Some(HeroAct::Follow(!on));
                    ui.close();
                }
                fin::menu_sep(ui);
            }
            if fin::menu_item(ui, Some("play"), "Play from the start", None, false, can_play).clicked() {
                act = Some(HeroAct::FromStart);
                ui.close();
            }
            if fin::menu_item(ui, Some("list"), "Play in order", None, false, can_play).clicked() {
                act = Some(HeroAct::InOrder);
                ui.close();
            }
            if fin::menu_item(ui, Some("shuffle"), "New round", None, false, can_play).on_hover_text("Every song again, in a fresh order").clicked() {
                act = Some(HeroAct::NewRound);
                ui.close();
            }
            if here && fin::menu_item(ui, Some("note"), "Go to the playing song", None, false, true).clicked() {
                act = Some(HeroAct::Locate);
                ui.close();
            }
            fin::menu_sep(ui);
            if fin::menu_item(ui, Some("refresh"), "Reload from Spotify", None, false, true).clicked() {
                act = Some(HeroAct::Reload);
                ui.close();
            }
            // One of the user's own playlists.
            if let Some(uri) = &dupes_of
                && fin::menu_item(ui, Some("search"), "Find duplicates", None, false, true).clicked()
            {
                act = Some(HeroAct::FindDuplicates(uri.clone()));
                ui.close();
            }
            if let Some(chosen) = banner_items(ui, banner_set, picking) {
                act = Some(HeroAct::Banner(chosen));
            }
        });

        // SHUFFLE and LEFT (`.stat`), when there is room (a playlist's page).
        if mw >= 760.0 && !browse {
            x += 8.0;
            for (label, value, small) in stats {
                let lg = fin::spaced(ui, label, bold(9.5), p.text_weak, 0.95);
                let vg = ui.painter().layout_no_wrap(fmt_thousands(*value), bold(13.0), p.text_strong);
                let sg = ui.painter().layout_no_wrap(small.clone(), medium(11.0), p.text_weak);
                let w = 12.0 + lg.size().x.max(vg.size().x + 4.0 + sg.size().x) + 12.0;
                if x + w > right {
                    break;
                }
                let h = lg.size().y + 1.0 + vg.size().y;
                let top = at.y + 14.0 - h / 2.0;
                let painter = ui.painter();
                painter.vline(x, top..=(top + h), Stroke::new(1.0, p.border));
                painter.galley(pos2(x + 12.0, top), lg.clone(), p.text_weak);
                let vy = top + lg.size().y + 1.0;
                let vw = vg.size().x;
                let base = vy + vg.size().y;
                painter.galley(pos2(x + 12.0, vy), vg, p.text_strong);
                painter.galley(pos2(x + 12.0 + vw + 4.0, base - sg.size().y - 1.0), sg, p.text_weak);
                x += w;
            }
        }
        act
    }

    /// SHUFFLE "706 of 890" and LEFT "184 · 7 h 18 min" while the list plays; else how far its
    /// round got, if it has started one.
    fn hero_stats(&self, source: &Source, here: bool) -> Vec<Stat> {
        if here
            && let Some(d) = self.deck.as_ref().filter(|d| d.source == *source && !d.order.is_empty())
        {
            let n = d.order.len();
            let at = (d.now + 1).min(n);
            let pos = self.playback.current_position_ms() as u64;
            let ms: u64 = d.order.iter().skip(d.now).map(|&i| u64::from(d.tracks[i as usize].duration_ms)).sum::<u64>().saturating_sub(pos);
            return vec![
                ("SHUFFLE", at, format!("of {}", fmt_thousands(n))),
                ("LEFT", n - at, format!("· {}", fmt_total(ms))),
            ];
        }
        let heard = self.heard(source);
        match self.known_song_count(source) {
            Some(n) if heard > 0 && heard < n => vec![
                ("SHUFFLE", heard, format!("of {}", fmt_thousands(n))),
                ("LEFT", n - heard, String::new()),
            ],
            _ => Vec::new(),
        }
    }

    fn apply_hero_act(&mut self, ui: &Ui, source: &Source, act: HeroAct) {
        match act {
            HeroAct::Play(state) => self.press_play(source, state),
            HeroAct::Reshuffle => {
                let here = self.playback.track.is_some() && self.playback.source.as_ref() == Some(source);
                if here { self.reshuffle() } else { self.new_round(source) }
            }
            HeroAct::Pin(pin) => match source {
                Source::LikedSongs => {
                    self.prefs.liked_pinned = pin;
                    self.prefs_changed();
                }
                Source::Playlist(uri) => self.set_pinned(uri, pin),
                _ => {}
            },
            HeroAct::Edit(uri) => self.open_edit_details(&uri),
            HeroAct::FromStart => {
                let shuffle = self.current_shuffle();
                self.send(Command::PlaySource { source: source.clone(), start_index: Some(0), shuffle });
            }
            HeroAct::InOrder => self.send(Command::PlaySource { source: source.clone(), start_index: None, shuffle: false }),
            HeroAct::NewRound => self.new_round(source),
            HeroAct::Locate => self.locate_current_track(),
            HeroAct::Reload => self.open_source(source.clone(), true),
            HeroAct::FindDuplicates(uri) => self.find_duplicates(&uri),
            HeroAct::Banner(b) => self.banner_act(ui.ctx(), source, b),
            HeroAct::Follow(follow) => {
                if let Source::Artist(uri) = source {
                    // Its name and picture once the page has them (not the "Artist" stand-in).
                    let (name, image) = match self.source_info.get(source) {
                        Some(i) => (i.title.clone(), i.image.clone()),
                        None => (String::new(), None),
                    };
                    self.set_follow(uri, &name, image, follow);
                }
            }
            HeroAct::AddAll(playlist_uri) => {
                let uris: Vec<String> = self
                    .view
                    .as_ref()
                    .and_then(|v| v.tracks.as_ref())
                    .map(|t| t.iter().filter(|t| !t.is_local).map(|t| t.uri.clone()).collect())
                    .unwrap_or_default();
                if !uris.is_empty() {
                    self.add_songs_to(playlist_uri, uris);
                }
            }
        }
    }

    /// The bar under the hero (`.tbar`): the Songs and Blocked tabs with their counts, and the filter.
    fn list_tbar(&mut self, ui: &mut Ui, bar: Rect, mw: f32, pad: f32) {
        let p = self.palette;
        let painter = ui.painter().clone();
        painter.hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, p.border));
        let (songs, blocked_here, updating) = match self.view.as_ref() {
            Some(v) => {
                let tracks = v.tracks.as_deref().map_or(&[][..], |t| &t[..]);
                let blocked = tracks.iter().filter(|t| self.blocked.contains(&t.uri)).count();
                let songs = v.tracks.as_ref().map(|t| t.len()).or_else(|| self.known_song_count(&v.source)).unwrap_or(0);
                (songs, blocked, v.loading.filter(|_| v.tracks.is_some()))
            }
            None => (0, 0, None),
        };
        let key = self.view.as_ref().map(|v| v.source.cache_key()).unwrap_or_default();
        let mut x = bar.left() + pad;
        let mut under = None;
        for (k, (tab, name, count)) in [(MiddleTab::Songs, "Songs", songs), (MiddleTab::Blocked, "Blocked", blocked_here)].into_iter().enumerate() {
            let on = (self.middle_tab == MiddleTab::Blocked) == (tab == MiddleTab::Blocked);
            let ng = ui.painter().layout_no_wrap(name.to_owned(), if on { bold(12.0) } else { medium(12.0) }, Color32::PLACEHOLDER);
            let cg = ui.painter().layout_no_wrap(fmt_thousands(count), medium(10.5), p.text_weak);
            let w = ng.size().x + 5.0 + cg.size().x;
            let rect = Rect::from_min_size(pos2(x, bar.top()), vec2(w, bar.height()));
            let resp = ui.interact(rect, Id::new(("list-tab", name)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let color = motion::color(ui.ctx(), resp.id, fin::tab_ink(&p, on, resp.hovered()), motion::HOVER);
            let base = bar.center().y + ng.size().y / 2.0;
            painter.galley(pos2(x, bar.center().y - ng.size().y / 2.0), ng.clone(), color);
            // The count rolls when it changes (this list's: another list's is just there).
            let at = pos2(x + ng.size().x + 5.0, base - 0.5);
            motion::count(&painter, Id::new(("list-tab-count", name, &key)), count, at, Align2::LEFT_BOTTOM, |n| painter.layout_no_wrap(fmt_thousands(n), medium(10.5), p.text_weak));
            if on {
                under = Some((k, Rect::from_min_max(pos2(x, bar.bottom() - 2.0), pos2(x + w, bar.bottom() + 1.0))));
            }
            if resp.clicked() {
                self.middle_tab = tab;
            }
            x += w + 18.0;
        }
        // The line slides to the tab picked.
        motion::marker(ui.ctx(), Id::new("list-tab-line"), bar.min, under, motion::TAB, |line, a| {
            painter.rect_filled(line, CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 }, p.accent_ink.gamma_multiply(a));
        });

        let fw = if mw < 560.0 { 120.0 } else if mw < 760.0 { 140.0 } else { 220.0 };
        let fr = Rect::from_min_size(pos2(bar.right() - pad - fw, bar.center().y - 14.0), vec2(fw, 28.0));
        if let Some((loaded, total)) = updating {
            // Refreshing a list already shown: how far, left of the filter.
            let text = match total {
                Some(t) if t > 0 => format!("Updating {} / {}", fmt_thousands(loaded.min(t)), fmt_thousands(t)),
                _ => "Updating…".to_owned(),
            };
            painter.text(pos2(fr.left() - 12.0, bar.center().y), Align2::RIGHT_CENTER, text, regular(11.0), p.text_weak);
        }
        let Some(view) = self.view.as_mut() else { return };
        let before = view.filter.clone();
        let hint = if fw < 180.0 { "Filter" } else { "Filter songs" };
        let sb = fin::search_box_with(ui, &p, fr, Id::new(TRACK_FILTER_ID), &mut view.filter, hint, "Ctrl+F", "filter");
        // A focused text field gives up focus on Esc before any widget runs: `lost_focus` is true then.
        if sb.resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            view.filter.clear();
        }
        if view.filter != before {
            view.refilter();
        }
    }

    /// The table: its head, then the rows on screen (or what stands in for them).
    #[allow(clippy::too_many_arguments)]
    fn list_songs(&mut self, ui: &mut Ui, left: f32, w: f32, tbl_x: f32, cols: Cols, row_h: f32, album: bool, blocked_tab: bool, scroll_to: Option<usize>, source: &Source) {
        let p = self.palette;
        let top = ui.cursor().top() + TBL_TOP;
        let inner = Rect::from_min_max(pos2(left + tbl_x, top), pos2(left + w - tbl_x, top));
        let Some(view) = self.view.as_ref() else { return };
        let Some(tracks) = view.tracks.clone() else {
            // Still loading, or it failed.
            let (error, loading) = (view.error.clone(), view.loading);
            if error.is_none() {
                // Nothing at first (most loads are quick), then placeholder rows, and how far it
                // has got above them.
                let rows = Rect::from_min_size(pos2(inner.left(), top + 36.0), vec2(inner.width(), row_h * 8.0));
                ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left, top), pos2(left + w, rows.bottom() + TBL_BOTTOM)));
                let shown = motion::loading(ui, Id::new(("list-loading", source.cache_key())), rows, row_h, cols.cover, &p);
                let (loaded, total) = loading.unwrap_or((0, None));
                if shown > 0.0 && (loaded > 0 || total.is_some()) {
                    let head = Rect::from_min_max(pos2(left, top), pos2(left + w, rows.top()));
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(head).layout(egui::Layout::top_down(Align::Center)));
                    child.multiply_opacity(shown);
                    loading_progress(&mut child, &p, loaded, total, 260.0);
                }
                return;
            }
            let area = Rect::from_min_size(pos2(left, top), vec2(w, 160.0));
            ui.advance_cursor_after_rect(area);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area.shrink(24.0)).layout(egui::Layout::top_down(Align::Center)));
            let mut retry = false;
            if let Some(err) = error {
                child.add_space(20.0);
                child.label(egui::RichText::new("Couldn't load these songs").font(medium(15.0)).color(p.text));
                child.add_space(4.0);
                child.label(egui::RichText::new(err).font(regular(12.0)).color(p.text_weak));
                child.add_space(10.0);
                retry = crate::ui::kit::button(&mut child, &p, "Try again", crate::ui::kit::Kind::Secondary).clicked();
            }
            if retry {
                self.open_source(source.clone(), true);
            }
            return;
        };
        let rows = shown_rows(&view.filtered, &tracks, &self.blocked, blocked_tab);
        let filter = view.filter.trim().to_owned();
        let error = view.error.clone();
        let selected = view.selected;
        // Only songs on screen stay picked (a filter or the Blocked tab hides others).
        let shown: std::collections::HashSet<usize> = rows.iter().map(|&r| r as usize).collect();
        let pass = ui.ctx().cumulative_pass_nr();
        if let Some(v) = self.view.as_mut() {
            v.picked.retain(|i| shown.contains(i));
            v.follow(&tracks, &self.blocked, &rows, blocked_tab, pass);
        }
        let picked = self.view.as_ref().map(|v| v.picked.clone()).unwrap_or_default();

        // The head (`.th`); an album's has a line under it.
        let th = Rect::from_min_size(inner.min, vec2(inner.width(), TH_H));
        let parts = cols.parts(th);
        let painter = ui.painter().clone();
        let mid = th.center().y;
        let g = head_caps(ui, "#");
        painter.galley(pos2(parts.num.right() - g.size().x, mid - g.size().y / 2.0), g, p.text_weak);
        let g = head_caps(ui, "Title");
        painter.galley(pos2(parts.text.left(), mid - g.size().y / 2.0), g, p.text_weak);
        if cols.artist > 0.0 {
            let g = head_caps(ui, "Artist");
            painter.galley(pos2(parts.artist.left(), mid - g.size().y / 2.0), g, p.text_weak);
        }
        if cols.album > 0.0 {
            let g = head_caps(ui, "Album");
            painter.galley(pos2(parts.album.left(), mid - g.size().y / 2.0), g, p.text_weak);
        }
        if cols.added > 0.0 {
            let g = head_caps(ui, "Date added");
            painter.galley(pos2(parts.added.left(), mid - g.size().y / 2.0), g, p.text_weak);
        }
        let g = head_caps(ui, "Time");
        painter.galley(pos2(parts.time.right() - g.size().x, mid - g.size().y / 2.0), g, p.text_weak);
        let mut y = th.bottom();
        if album {
            painter.hline(th.x_range(), th.bottom() - 0.5, Stroke::new(1.0, p.border));
            y += 4.0;
        }
        if let Some(err) = error {
            painter.text(pos2(parts.text.left(), y + 10.0), Align2::LEFT_CENTER, format!("Couldn't refresh: {err}"), regular(11.5), p.error);
            y += 20.0;
        }

        if rows.is_empty() {
            let msg = if blocked_tab && filter.is_empty() {
                "No blocked songs. Right-click a song and choose Block song.".to_owned()
            } else if !filter.is_empty() {
                format!("No songs match \u{201c}{filter}\u{201d}")
            } else if *source == Source::LikedSongs {
                "No liked songs yet. Click the heart next to any song to save it here.".to_owned()
            } else {
                "This list is empty.".to_owned()
            };
            let area = Rect::from_min_size(pos2(left, top), vec2(w, y - top + 80.0));
            ui.advance_cursor_after_rect(area);
            painter.text(pos2(left + w / 2.0, y + 36.0), Align2::CENTER_CENTER, msg, regular(12.0), p.text_weak);
            return;
        }

        let rows_top = y;
        // Songs that went or came while shown close up or open where they are.
        let moving = self.row_moves(ui.ctx(), row_h);
        let extra: f32 = moving.closing.iter().map(|c| c.height).sum();
        let short: f32 = moving.opening.iter().map(|o| row_h - o.1).sum();
        // How far the rows closing and opening above row `k` move it.
        let shift = |k: usize| {
            let down: f32 = moving.closing.iter().filter(|c| c.before <= k).map(|c| c.height).sum();
            down - moving.opening.iter().filter(|o| o.0 < k).map(|o| row_h - o.1).sum::<f32>()
        };
        let all = Rect::from_min_max(pos2(left, top), pos2(left + w, rows_top + rows.len() as f32 * row_h + extra - short + TBL_BOTTOM));
        ui.advance_cursor_after_rect(all);
        let row_rect = |k: usize| Rect::from_min_size(pos2(inner.left(), rows_top + k as f32 * row_h + shift(k)), vec2(inner.width(), row_h));
        if let Some(k) = scroll_to.filter(|&k| k < rows.len()) {
            ui.scroll_to_rect(row_rect(k), Some(Align::Center));
        }
        let clip = ui.clip_rect();
        let (first, last) = visible_rows(rows_top, row_h, rows.len(), clip.top() - extra, clip.bottom() + short);
        let mut actions: Vec<RowAction> = Vec::new();
        let mut dragged = None;
        for (k, &ti) in rows.iter().enumerate().take(last).skip(first) {
            let ti = ti as usize;
            let on = if picked.is_empty() { selected == Some(ti) } else { picked.contains(&ti) };
            let was = moving.numbers.any().then(|| (moving.numbers.was(ti), moving.t));
            let row = RowCtx { rect: row_rect(k), ti, number: ti + 1, was, selected: on, cols };
            let d = match moving.opening.iter().find(|o| o.0 == k) {
                // Opening: shown in the room it takes so far, fading in.
                Some(&(_, room, fade)) => {
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(row.rect));
                    child.set_clip_rect(Rect::from_min_size(row.rect.min, vec2(row.rect.width(), room)).intersect(clip));
                    child.multiply_opacity(fade);
                    self.song_row(&mut child, &tracks[ti], &row, &mut actions)
                }
                None => self.song_row(ui, &tracks[ti], &row, &mut actions),
            };
            if d.is_some() {
                dragged = d;
            }
        }
        // The closing ones, fading in the room they still take; nothing on them takes clicks.
        let mut above = 0.0;
        for (n, c) in moving.closing.iter().enumerate() {
            let opened: f32 = moving.opening.iter().filter(|o| o.0 < c.before).map(|o| row_h - o.1).sum();
            let rect = Rect::from_min_size(pos2(inner.left(), rows_top + c.before as f32 * row_h + above - opened), vec2(inner.width(), row_h));
            above += c.height;
            let room = Rect::from_min_size(rect.min, vec2(rect.width(), c.height)).intersect(clip);
            if !room.is_positive() {
                continue;
            }
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            child.disable();
            child.set_clip_rect(room);
            child.set_opacity(ui.opacity() * c.fade);
            let row = RowCtx { rect, ti: usize::MAX - n, number: c.number, was: None, selected: false, cols };
            let _ = self.song_row(&mut child, &c.track, &row, &mut Vec::new());
        }
        if let Some(ti) = dragged {
            // Dragging a song that isn't picked picks it, as clicking it would.
            actions.push(RowAction::Select(ti));
        }
        self.list_keys(ui, &rows, &tracks, source, &mut actions);
        self.paint_songs_drag(ui, &tracks);
        self.apply_row_actions(ui, actions, &tracks, source, &rows);
    }

    /// The rows closing and opening in this pass as songs went or came (see `Moves`): the room each
    /// takes and how far it has faded, over `motion::ROW` (closing slow, then quick; opening quick,
    /// then settling). Forgotten once done.
    fn row_moves(&mut self, ctx: &egui::Context, row_h: f32) -> Moving {
        let Some(v) = self.view.as_mut() else { return Moving::default() };
        let now = ctx.input(|i| i.time);
        let length = f64::from(motion::secs(motion::ROW).max(0.001));
        if v.moves.as_ref().is_some_and(|m| m.since.is_some_and(|s| now - s >= length)) {
            v.moves = None;
        }
        let Some(m) = v.moves.as_mut() else { return Moving::default() };
        ctx.request_repaint();
        let t = ((now - *m.since.get_or_insert(now)) / length) as f32;
        let (shut, open, moves) = (motion::ease_in(t), motion::ease_out(t), motion::moves());
        Moving {
            closing: m
                .gone
                .iter()
                .map(|g| Closing { before: g.before, number: g.number, height: row_h * motion::room(1.0 - shut, moves), fade: 1.0 - shut, track: g.track.clone() })
                .collect(),
            opening: m.came.iter().map(|&k| (k, row_h * motion::room(open, moves), open)).collect(),
            numbers: m.numbers.clone(),
            t,
        }
    }

    /// The list's keys, while nothing else takes them (no text box, menu or dialog): Ctrl+A picks
    /// every song shown, the arrows move the selection (with Shift, picking the songs passed),
    /// Enter plays the selected song, Esc lets go of the picks.
    fn list_keys(&mut self, ui: &Ui, rows: &[u32], tracks: &[TrackInfo], source: &Source, actions: &mut Vec<RowAction>) {
        let ctx = ui.ctx();
        // "Already in" only while its box is up: one that waits (behind Settings, say) has nothing on
        // screen to say why the keys would do nothing.
        if rows.is_empty() || self.dialog_open() || self.already_in_up(ctx) || ctx.text_edit_focused() || self.song_menu.is_some() || self.help_open || egui::Popup::is_any_open(ctx) {
            return;
        }
        if self.typing_at_start {
            return;
        }
        let Some(v) = self.view.as_mut() else { return };
        if let Some(ti) = song_keys(ui, v, rows, tracks.len()) {
            let _ = source;
            actions.push(RowAction::Play(ti));
        }
    }

    /// One song (`.sr`). Returns its index when a drag of it starts.
    fn song_row(&mut self, ui: &mut Ui, t: &TrackInfo, row: &RowCtx, actions: &mut Vec<RowAction>) -> Option<usize> {
        let p = self.palette;
        let rect = row.rect;
        let ti = row.ti;
        let parts = row.cols.parts(rect);
        let sz = row.cols.sz;
        // The playing song is marked wherever it shows, as in the design.
        let current = self.playback.track.as_ref().is_some_and(|c| c.uri == t.uri);
        let playing = matches!(self.playback.status, PlayStatus::Playing | PlayStatus::Loading);
        let is_blocked = self.blocked.contains(&t.uri);
        let is_liked = self.liked.contains(&t.uri);
        let dim = !t.playable || is_blocked;
        // Rows can be dragged (onto a playlist, to add the song there); on a touch screen a drag
        // scrolls the list instead.
        let sense = if ui.input(|i| i.has_touch_screen()) { Sense::click() } else { Sense::click_and_drag() };
        let resp = ui.interact(rect, Id::new(("song-row", ti)), sense);
        let painter = ui.painter().clone();
        if current || resp.hovered() {
            painter.rect_filled(rect, CornerRadius::same(6), p.hover_bg);
        } else if row.selected {
            painter.rect_filled(rect, CornerRadius::same(6), fin::acc(&p, 0.10));
        }
        let mid = rect.center().y;

        // # (the playing song: the accent bars)
        if current {
            let at = Rect::from_min_size(pos2(parts.num.right() - 13.0, mid - 6.5), vec2(13.0, 13.0));
            svgicon::paint(&painter, at, if playing { "eq" } else { "pause" }, p.accent_ink, svgicon::STROKE);
        } else {
            let (was, t) = row.was.unwrap_or((row.number, 1.0));
            motion::roll(&painter, was, row.number, t, pos2(parts.num.right(), mid), Align2::RIGHT_CENTER, |n| {
                painter.layout_no_wrap(fmt_thousands(n), regular(sz.small), p.text_weak)
            });
        }
        // Cover
        if row.cols.cover {
            // Compact rows (32) take a 24 cover.
            let side = sz.cover.min(rect.height() - 8.0);
            let cr = Rect::from_center_size(parts.cover.center(), vec2(side, side));
            if !self.images.paint(ui, t.cover_small.as_deref(), cr, 3, p.widget_bg) {
                svgicon::paint(&painter, cr.shrink(8.0), "note", p.text_weak, svgicon::STROKE);
            }
            if dim {
                painter.rect_filled(cr, CornerRadius::same(3), p.main_bg.gamma_multiply(0.55));
            }
        }
        // Title and artists (each a link)
        let (title_c, sub_c) = fin::row_ink(&p, dim, current);
        let tw = parts.text.width();
        let title = one_line(ui, &t.name, if current { bold(sz.title) } else { medium(sz.title) }, title_c, tw, false);
        if row.cols.sub {
            let line = t.artist_line();
            let sub = one_line(ui, &line, regular(sz.sub), sub_c, tw, false);
            let h = title.size().y + 1.0 + sub.size().y;
            let ty = mid - h / 2.0;
            let sy = ty + title.size().y + 1.0;
            let sub_h = sub.size().y;
            let sub_w = sub.size().x;
            painter.galley(pos2(parts.text.left(), ty), title, title_c);
            painter.galley(pos2(parts.text.left(), sy), sub, sub_c);
            artist_links(ui, &painter, t, ti, pos2(parts.text.left(), sy), sub_w, sub_h, sz.sub, actions);
        } else {
            painter.galley(pos2(parts.text.left(), mid - title.size().y / 2.0), title, title_c);
        }
        // Artist column (Settings), each a link
        if row.cols.artist > 0.0 {
            let g = one_line(ui, &t.artist_line(), regular(sz.album), if dim { p.text_faint } else { p.text_weak }, parts.artist.width(), false);
            let at = pos2(parts.artist.left(), mid - g.size().y / 2.0);
            let (gw, gh) = (g.size().x, g.size().y);
            painter.galley(at, g, p.text_weak);
            artist_links(ui, &painter, t, ti, at, gw, gh, sz.album, actions);
        }
        // Album (a link)
        if row.cols.album > 0.0 {
            let g = one_line(ui, &t.album, regular(sz.album), if dim { p.text_faint } else { p.text_weak }, parts.album.width(), false);
            let r = Rect::from_min_size(pos2(parts.album.left(), mid - g.size().y / 2.0), g.size());
            let link = t.album_uri.starts_with("spotify:album:");
            let hovered = link && ui.interact(r, Id::new(("row-album", ti)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).clicked();
            if hovered {
                actions.push(RowAction::Open(Source::Album(t.album_uri.clone())));
            }
            if link && ui.rect_contains_pointer(r) {
                painter.hline(r.x_range(), r.bottom() - 1.0, Stroke::new(1.0, p.text_weak));
            }
            painter.galley(r.min, g, p.text_weak);
        }
        // Unblock (Blocked tab)
        if row.cols.unblock && is_blocked {
            let g = ui.painter().layout_no_wrap("Unblock".to_owned(), regular(10.5), p.text_weak);
            let b = Rect::from_center_size(pos2(parts.unblock.right() - (g.size().x + 14.0) / 2.0, mid), vec2(g.size().x + 14.0, 20.0));
            let r = ui.interact(b, Id::new(("row-unblock", ti)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let (c, edge) = if r.hovered() { (p.text_strong, p.text_weak) } else { (p.text_weak, p.border) };
            painter.rect_stroke(b, CornerRadius::same(4), Stroke::new(1.0, edge), StrokeKind::Inside);
            painter.text(b.center(), Align2::CENTER_CENTER, "Unblock", regular(10.5), c);
            if r.clicked() {
                actions.push(RowAction::Block(t.uri.clone(), false));
            }
        }
        // Date added (Settings)
        if row.cols.added > 0.0 {
            let text = fmt_added(t.added_at, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64));
            painter.text(pos2(parts.added.left(), mid), Align2::LEFT_CENTER, text, regular(sz.album), if dim { p.text_faint } else { p.text_weak });
        }
        // Time
        painter.text(pos2(parts.time.right(), mid), Align2::RIGHT_CENTER, fmt_ms(t.duration_ms), regular(sz.small), p.text_weak);
        // Heart
        if !t.is_local {
            let hr = Rect::from_center_size(parts.heart.center(), vec2(HEART_W, row.rect.height()));
            let r = ui
                .interact(hr, Id::new(("row-heart", ti)), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text(if is_liked { "Unlike" } else { "Like" });
            // Kept per song: another list's row in this place has its own.
            let line = motion::color(ui.ctx(), Id::new(("row-heart-line", &t.uri, ti)), if is_liked { p.accent_ink } else if r.hovered() { p.text } else { p.text_faint }, motion::HOVER);
            // It pops when liked, dips when unliked: this song's heart only.
            let (scale, fill) = motion::heart(ui.ctx(), Id::new(("row-heart-pop", &t.uri)), is_liked);
            let icon = Rect::from_center_size(hr.center(), vec2(sz.heart, sz.heart));
            motion::scaled(ui, hr.center(), scale, || fin::paint_heart(&painter, icon, line, p.accent_ink, fill));
            if r.clicked() {
                actions.push(RowAction::Like(t.uri.clone(), !is_liked));
            }
        }

        let mut dragged = None;
        // Only the left button drags songs (the middle one scrolls, as in a browser).
        if resp.drag_started_by(egui::PointerButton::Primary) {
            // A picked song drags every picked song; another one drags just itself (and gets picked).
            // Local files can't go into Spotify playlists.
            let picks = self.view.as_ref().filter(|v| v.picked.len() > 1 && v.picked.contains(&ti)).and_then(|v| {
                let tracks = v.tracks.as_ref()?;
                Some(v.picked.iter().filter_map(|&i| tracks.get(i)).filter(|t| !t.is_local).map(|t| t.uri.clone()).collect::<Vec<_>>())
            });
            match picks {
                Some(uris) if !uris.is_empty() => resp.dnd_set_drag_payload(SongsDrag { uris }),
                Some(_) => {}
                None if !t.is_local => {
                    resp.dnd_set_drag_payload(SongsDrag { uris: vec![t.uri.clone()] });
                    dragged = Some(ti);
                }
                None => {}
            }
        }
        if resp.double_clicked() {
            actions.push(RowAction::Play(ti));
        } else if resp.clicked() {
            actions.push(RowAction::Click(ti, ui.input(|i| i.modifiers)));
        }
        let resp = if t.playable {
            resp
        } else {
            resp.on_hover_text(if t.is_local { "Local file: Truffle can't play files stored on your computer" } else { "Not available on Spotify in your country" })
        };
        if resp.secondary_clicked() {
            let at = resp.interact_pointer_pos().unwrap_or(rect.center());
            actions.push(RowAction::Menu(ti, at));
        }
        dragged
    }

    /// What the rows asked for this frame.
    fn apply_row_actions(&mut self, _ui: &Ui, actions: Vec<RowAction>, tracks: &[TrackInfo], source: &Source, rows: &[u32]) {
        let shuffle = self.current_shuffle();
        let mut open = None;
        for action in actions {
            match action {
                RowAction::Select(i) => {
                    if let Some(v) = self.view.as_mut() {
                        v.selected = Some(i);
                        v.picked = std::iter::once(i).collect();
                        v.anchor = Some(i);
                    }
                }
                RowAction::Click(i, mods) => {
                    if let Some(v) = self.view.as_mut() {
                        v.click(i, mods, rows);
                    }
                }
                RowAction::Menu(i, at) => {
                    let from = self.view.as_ref().map(|v| v.source.clone());
                    let many: Vec<TrackInfo> = self
                        .view
                        .as_ref()
                        .filter(|v| v.picked.len() > 1 && v.picked.contains(&i))
                        .map(|v| v.picked.iter().filter_map(|&k| tracks.get(k).cloned()).collect())
                        .unwrap_or_default();
                    if many.len() > 1 {
                        self.open_songs_menu(many, at, from);
                    } else {
                        if let Some(v) = self.view.as_mut() {
                            v.selected = Some(i);
                            v.picked = std::iter::once(i).collect();
                            v.anchor = Some(i);
                        }
                        self.open_song_menu(&tracks[i], at, from);
                    }
                }
                RowAction::Play(i) => {
                    if let Some(v) = self.view.as_mut() {
                        v.selected = Some(i);
                        v.picked = std::iter::once(i).collect();
                        v.anchor = Some(i);
                    }
                    if tracks[i].playable {
                        self.send(Command::PlaySource { source: source.clone(), start_index: Some(i), shuffle });
                    } else {
                        self.toast("That song can't be played (it's a local file or not available on Spotify).", false);
                    }
                }
                RowAction::Like(uri, like) => self.like_song(&uri, like),
                RowAction::Block(uri, block) => self.set_blocked(uri, block),
                RowAction::Open(target) => open = Some(target),
            }
        }
        if let Some(target) = open {
            self.open_source(target, false);
        }
    }
}

impl TruffleApp {
    /// Under an artist's hero (`pgArtist`): Popular beside Releases when there is room (700 and
    /// up), else one above the other.
    fn artist_body(&mut self, ui: &mut Ui, left: f32, w: f32, mw: f32, source: &Source) {
        let x0 = left + PAD_X;
        let x1 = left + w - PAD_X;
        let top = self.artist_likes(ui, x0, x1, ui.cursor().top(), source);
        let two = mw >= 700.0;
        let rel_w = 372.0;
        let pop_right = if two { x1 - rel_w - 28.0 } else { x1 };
        let pop_bottom = self.artist_popular(ui, x0, pop_right, top, source);
        let (rx, ry) = if two { (pop_right + 28.0, top) } else { (x0, pop_bottom + 22.0) };
        let rel_bottom = self.artist_releases(ui, rx, x1, ry, two, source);
        let bottom = pop_bottom.max(rel_bottom) + 28.0;
        ui.advance_cursor_after_rect(Rect::from_min_max(pos2(left, top), pos2(left + w, bottom)));
    }

    /// The songs of this artist the user has liked (from Liked Songs): a row that opens them as a
    /// list of their own, to play or shuffle. Nothing when there are none: it slides open as the
    /// first like comes and shut as the last goes, and its count rolls. Returns where the page goes
    /// on.
    fn artist_likes(&mut self, ui: &mut Ui, x0: f32, x1: f32, top: f32, source: &Source) -> f32 {
        let p = self.palette;
        let Source::Artist(uri) = source else { return top };
        if self.artist_likes.ask(uri) {
            self.send(Command::ArtistLikes(uri.clone()));
        }
        let liked = self.artist_likes.get(uri).map_or(0, |l| l.songs);
        let id = Id::new(("artist-likes", uri));
        let shown = motion::toward(ui.ctx(), id.with("shown"), liked > 0, motion::ROW);
        if shown <= 0.0 {
            return top;
        }
        // Shutting, it keeps the count it had.
        let n = motion::last_count(ui.ctx(), id, liked);
        let room = motion::room(shown, motion::moves());
        let row = Rect::from_min_max(pos2(x0, top), pos2(x1, top + 56.0));
        let open = Rect::from_min_max(row.min, pos2(x1, top + 56.0 * room));
        let mut painter = ui.painter().with_clip_rect(ui.clip_rect().intersect(open));
        painter.multiply_opacity(shown);
        let resp = ui.interact(open, id, if liked > 0 { Sense::click() } else { Sense::hover() });
        let hover = motion::hover(ui.ctx(), id, resp.hovered() && liked > 0);
        let resp = if liked > 0 { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp };
        painter.rect_filled(row, CornerRadius::same(8), motion::mix(p.widget_bg, p.widget_hover_bg, hover));
        let badge = Rect::from_center_size(pos2(row.left() + 12.0 + 18.0, row.center().y), vec2(36.0, 36.0));
        painter.circle_filled(badge.center(), 18.0, fin::acc(&p, 0.18));
        svgicon::paint(&painter, Rect::from_center_size(badge.center(), vec2(16.0, 16.0)), "heartf", p.accent_ink, svgicon::STROKE);
        let tx = badge.right() + 12.0;
        let name = painter.layout_no_wrap("Your likes".to_owned(), bold(13.0), p.text_strong);
        let nh = name.size().y;
        let rest = painter.layout_no_wrap(if n == 1 { " song you've liked" } else { " songs you've liked" }.to_owned(), regular(11.5), p.text_weak);
        let h = nh + 2.0 + rest.size().y;
        painter.galley(pos2(tx, row.center().y - h / 2.0), name, p.text_strong);
        // The count rolls to a new one; the words follow it.
        let sy = row.center().y - h / 2.0 + nh + 2.0;
        let number = motion::count(&painter, id, n, pos2(tx, sy), Align2::LEFT_TOP, |n| painter.layout_no_wrap(fmt_thousands(n), regular(11.5), p.text_weak));
        painter.galley(pos2(number.right(), sy), rest, p.text_weak);
        svgicon::paint(&painter, Rect::from_center_size(pos2(row.right() - 22.0, row.center().y), vec2(14.0, 14.0)), "chevr", motion::mix(p.text_weak, p.text, hover), svgicon::STROKE);
        if resp.clicked() {
            self.open_source(Source::Mix(format!("{}{uri}", crate::library::MIX_ARTIST)), false);
        }
        top + (56.0 + 22.0) * room
    }

    /// A section's head (`.hdr`): its name (13.5 semibold), and "Show all" on the right when asked.
    fn section_head(ui: &mut Ui, x0: f32, x1: f32, y: f32, name: &str, link: Option<&str>, id: &str) -> bool {
        let p = crate::ui::theme::palette(ui.ctx());
        let g = ui.painter().layout_no_wrap(name.to_owned(), bold(13.5), p.text_strong);
        ui.painter().galley(pos2(x0, y + 12.0 - g.size().y / 2.0), g, p.text_strong);
        let Some(link) = link else { return false };
        let lg = ui.painter().layout_no_wrap(link.to_owned(), medium(11.0), p.text_weak);
        let r = Rect::from_min_size(pos2(x1 - lg.size().x, y + 12.0 - lg.size().y / 2.0), lg.size());
        let resp = ui.interact(r.expand(3.0), Id::new(id), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        ui.painter().galley(r.min, lg, if resp.hovered() { p.text } else { p.text_weak });
        resp.clicked()
    }

    /// Popular: the artist's top songs, 5 of them until "Show all". Returns its bottom.
    fn artist_popular(&mut self, ui: &mut Ui, x0: f32, x1: f32, top: f32, source: &Source) -> f32 {
        let p = self.palette;
        let Some(view) = self.view.as_ref() else { return top };
        let id = Id::new(("artist-popular", source.cache_key()));
        let Some(tracks) = view.tracks.clone() else {
            Self::section_head(ui, x0, x1, top, "Popular", None, "pop-all");
            if let Some(err) = view.error.clone() {
                // They failed: why, and a way to try again.
                let area = Rect::from_min_max(pos2(x0, top + 28.0), pos2(x1, top + 28.0 + ROW_H * 5.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(Align::Min)));
                motion::content_in(&mut child, id.with("failed"));
                child.add_space(8.0);
                child.label(egui::RichText::new("Couldn't load these songs").font(medium(15.0)).color(p.text));
                child.add_space(4.0);
                child.label(egui::RichText::new(err).font(regular(12.0)).color(p.text_weak));
                child.add_space(10.0);
                if crate::ui::kit::button(&mut child, &p, "Try again", crate::ui::kit::Kind::Secondary).clicked() {
                    self.open_source(source.clone(), true);
                }
                return area.bottom();
            }
            // Room for the 5 songs: nothing moves when they come.
            let rows = Rect::from_min_max(pos2(x0, top + 28.0), pos2(x1, top + 28.0 + ROW_H * 5.0));
            motion::loading(ui, id, rows, ROW_H, true, &p);
            return rows.bottom();
        };
        let was = motion::content_in(ui, id);
        let all = view.show_all;
        let selected = view.selected;
        let more = tracks.len() > 5;
        let link = more.then_some(if all { "Show less" } else { "Show all" });
        if Self::section_head(ui, x0, x1, top, "Popular", link, "pop-all")
            && let Some(v) = self.view.as_mut()
        {
            v.show_all = !all;
        }
        let n = if all { tracks.len() } else { tracks.len().min(5) };
        if let Some(v) = self.view.as_mut() {
            v.picked.retain(|&i| i < n);
        }
        let cols = Cols { num: 16.0, cover: true, sub: false, album: 0.0, unblock: false, artist: 0.0, added: 0.0, sz: SongSize::of(0) };
        let mut y = top + 24.0 + 4.0;
        let mut actions = Vec::new();
        let mut dragged = None;
        for (ti, t) in tracks.iter().enumerate().take(n) {
            let on = self.view.as_ref().is_some_and(|v| if v.picked.is_empty() { selected == Some(ti) } else { v.picked.contains(&ti) });
            let row = RowCtx { rect: Rect::from_min_size(pos2(x0, y), vec2(x1 - x0, ROW_H)), ti, number: ti + 1, was: None, selected: on, cols };
            if let Some(d) = self.song_row(ui, t, &row, &mut actions) {
                dragged = Some(d);
            }
            y += ROW_H;
        }
        if tracks.is_empty() {
            ui.painter().text(pos2(x0, y + 12.0), Align2::LEFT_CENTER, "Nothing here yet", regular(12.0), p.text_weak);
            y += 24.0;
        }
        if let Some(ti) = dragged {
            actions.push(RowAction::Select(ti));
        }
        ui.set_opacity(was);
        self.paint_songs_drag(ui, &tracks);
        let rows: Vec<u32> = (0..n as u32).collect();
        // The same keys as a list's (Ctrl+A, the arrows, Enter, Esc) for the songs shown.
        self.list_keys(ui, &rows, &tracks, source, &mut actions);
        self.apply_row_actions(ui, actions, &tracks, source, &rows);
        y
    }

    /// Releases: All / Albums / Singles, and their covers in a grid (4 of 78 beside Popular, else as
    /// many of at least 100 as fit, widened so the rows end at both edges). Returns its bottom.
    fn artist_releases(&mut self, ui: &mut Ui, x0: f32, x1: f32, top: f32, two: bool, source: &Source) -> f32 {
        let p = self.palette;
        let mut cards = self.source_info.get(source).map(|i| i.albums.clone()).unwrap_or_default();
        let chosen = self.view.as_ref().map_or(0, |v| v.releases);
        // This artist's liked songs per release, for the counts and the Liked chip (which also
        // shows releases the list leaves out, such as other editions).
        let likes = match source {
            Source::Artist(uri) => self.artist_likes.get(uri).cloned(),
            _ => None,
        };
        let liked_albums: std::collections::HashMap<String, usize> =
            likes.as_ref().map(|l| l.albums.iter().map(|(c, n)| (c.uri.clone(), *n)).collect()).unwrap_or_default();
        if chosen == 3
            && let Some(l) = &likes
        {
            for (card, _) in &l.albums {
                if !cards.iter().any(|k| k.uri == card.uri) {
                    cards.push(card.clone());
                }
            }
        }
        Self::section_head(ui, x0, x1, top, "Releases", None, "rel-head");
        // The chips (`.chip.sm`), on the right of the head.
        let mut x = x1;
        for (k, name) in [(3u8, "Liked"), (2, "Singles"), (1, "Albums"), (0, "All")] {
            let g = ui.painter().layout_no_wrap(name.to_owned(), medium(11.0), p.text);
            let r = Rect::from_min_size(pos2(x - g.size().x - 20.0, top), vec2(g.size().x + 20.0, 24.0));
            let resp = ui.interact(r, Id::new(("rel-chip", k)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let on = chosen == k;
            let fill = if on { fin::acc(&p, 0.16) } else if resp.hovered() { p.widget_hover_bg } else { p.widget_bg };
            ui.painter().rect_filled(r, CornerRadius::same(12), fill);
            if on {
                ui.painter().rect_stroke(r, CornerRadius::same(12), Stroke::new(1.0, fin::acc(&p, 0.45)), StrokeKind::Inside);
            }
            ui.painter().text(r.center(), Align2::CENTER_CENTER, name, medium(11.0), if on { p.text_strong } else { p.text });
            if resp.clicked()
                && let Some(v) = self.view.as_mut()
            {
                v.releases = k;
            }
            x = r.left() - 8.0;
        }
        // Each with how far it shows: under Liked one fades out before it goes, and in as it comes.
        let shown: Vec<_> = cards
            .iter()
            .filter_map(|c| match chosen {
                1 => c.subtitle.starts_with("Album").then_some((c, 1.0)),
                2 => (!c.subtitle.starts_with("Album")).then_some((c, 1.0)),
                3 => {
                    let liked = liked_albums.get(&c.uri).is_some_and(|&n| n > 0);
                    let fade = motion::toward(ui.ctx(), Id::new(("release-liked", &c.uri)), liked, motion::ROW);
                    (fade > 0.0).then_some((c, fade))
                }
                _ => Some((c, 1.0)),
            })
            .collect();
        let mut y = top + 24.0 + 10.0;
        if shown.is_empty() {
            let text = if chosen == 3 { "No liked songs on these yet" } else { "Nothing here yet" };
            let was = motion::content_in(ui, Id::new(("releases-none", source.cache_key(), chosen)));
            ui.painter().text(pos2(x0, y + 12.0), Align2::LEFT_CENTER, text, regular(12.0), p.text_weak);
            ui.set_opacity(was);
            return y + 24.0;
        }
        let (per_row, size) = if two { (4, 78.0) } else { wide::row_fit(x1 - x0, 100.0, 20.0) };
        let card_h = size + 5.0 + 14.0 + 12.0;
        let mut open = None;
        let was = ui.opacity();
        for (k, &(card, fade)) in shown.iter().enumerate() {
            let (col, row) = (k % per_row, k / per_row);
            // The others slide over to their new places as one goes or comes (by place in the grid,
            // so they keep up as the window is resized).
            let place = motion::slide(ui.ctx(), Id::new(("release-at", chosen, &card.uri)), vec2(col as f32, row as f32), motion::ROW);
            let at = pos2(x0 + place.x * (size + 20.0), y + place.y * (card_h + 12.0));
            ui.set_opacity(was * fade);
            let cover = Rect::from_min_size(at, vec2(size, size));
            let whole = Rect::from_min_size(at, vec2(size, card_h));
            let resp = ui.interact(whole, Id::new(("release", &card.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if !self.images.paint(ui, card.image.as_deref(), cover, 4, p.widget_bg) {
                svgicon::paint(ui.painter(), cover.shrink(size * 0.32), "album", p.text_weak, svgicon::STROKE);
            }
            if resp.hovered() {
                ui.painter().rect_filled(cover, CornerRadius::same(4), Color32::from_white_alpha(12));
            }
            let name = one_line(ui, &card.name, medium(11.0), p.text, size, false);
            let ny = cover.bottom() + 5.0;
            let nh = name.size().y;
            ui.painter().galley(pos2(at.x, ny), name, p.text);
            // "Single · 2022" reads "2022 · Single" here, as in the design.
            let sub = match card.subtitle.split_once(" · ") {
                Some((kind, year)) => format!("{year} · {kind}"),
                None => card.subtitle.clone(),
            };
            let sg = one_line(ui, &sub, regular(10.0), p.text_weak, size, false);
            ui.painter().galley(pos2(at.x, ny + nh), sg, p.text_weak);
            // A heart and how many of its songs are liked, on the cover's corner: it fades in and
            // out, and its count rolls.
            let liked_here = liked_albums.get(&card.uri).copied().unwrap_or(0);
            let tag_id = Id::new(("release-likes", &card.uri));
            let tag_shown = motion::toward(ui.ctx(), tag_id, liked_here > 0, motion::ROW);
            if tag_shown > 0.0 {
                let n = motion::last_count(ui.ctx(), tag_id, liked_here);
                let mut painter = ui.painter().clone();
                painter.multiply_opacity(tag_shown);
                let g = painter.layout_no_wrap(fmt_thousands(n), bold(9.5), Color32::WHITE);
                let tag = Rect::from_min_size(pos2(cover.left() + 5.0, cover.bottom() - 5.0 - 16.0), vec2(g.size().x + 22.0, 16.0));
                painter.rect_filled(tag, CornerRadius::same(8), Color32::from_black_alpha(170));
                svgicon::paint(&painter, Rect::from_min_size(pos2(tag.left() + 5.0, tag.center().y - 4.5), vec2(9.0, 9.0)), "heartf", p.on_dark().accent_ink, svgicon::STROKE);
                motion::count(&painter, tag_id, n, pos2(tag.left() + 16.0, tag.center().y), Align2::LEFT_CENTER, |n| painter.layout_no_wrap(fmt_thousands(n), bold(9.5), Color32::WHITE));
            }
            if resp.clicked() {
                open = Some(card.uri.clone());
            }
        }
        ui.set_opacity(was);
        let rows = shown.len().div_ceil(per_row);
        let height = rows as f32 * card_h + rows.saturating_sub(1) as f32 * 12.0;
        y += motion::slide(ui.ctx(), Id::new(("releases-height", source.cache_key(), chosen)), vec2(0.0, height), motion::ROW).y;
        if let Some(uri) = open {
            self.open_source(Source::Album(uri), false);
        }
        y
    }
}

/// What the hero's buttons and its More menu ask for.
/// What a name in the hero's meta line opens.
enum MetaLink {
    Artist(String),
    /// A user's profile (their username).
    Owner(String),
}

enum HeroAct {
    Play(PlayState),
    Reshuffle,
    Pin(bool),
    Edit(String),
    FromStart,
    InOrder,
    NewRound,
    Locate,
    Reload,
    /// "Find duplicates" in this playlist (one of the user's own).
    FindDuplicates(String),
    Banner(BannerAct),
    /// Add every song to this playlist (an album's +).
    AddAll(String),
    /// Follow (or unfollow) the artist.
    Follow(bool),
}

/// The rows closing and opening in a pass, and the numbers rolling (`row_moves`).
#[derive(Default)]
struct Moving {
    closing: Vec<Closing>,
    /// Rows opening: their place among the rows, the room they take and how far they have faded in.
    opening: Vec<(usize, f32, f32)>,
    numbers: super::Renumbered,
    /// How far it has got (0-1, linear; a count rolls as long).
    t: f32,
}

/// A row whose song went, as it closes (`row_moves`).
struct Closing {
    before: usize,
    number: usize,
    height: f32,
    fade: f32,
    track: TrackInfo,
}

/// One row's place and state.
struct RowCtx {
    rect: Rect,
    ti: usize,
    number: usize,
    /// While the numbers roll: the one before and how far it has rolled (0-1, linear).
    was: Option<(usize, f32)>,
    selected: bool,
    cols: Cols,
}

/// The artists' names over an already drawn line at `at` (`width` of it shown), each a link to
/// the artist's page.
#[allow(clippy::too_many_arguments)]
fn artist_links(ui: &mut Ui, painter: &egui::Painter, t: &TrackInfo, ti: usize, at: egui::Pos2, width: f32, h: f32, size: f32, actions: &mut Vec<RowAction>) {
    let p = crate::ui::theme::palette(ui.ctx());
    let comma = ui.painter().layout_no_wrap(", ".to_owned(), regular(size), p.text_weak).size().x;
    let mut x = at.x;
    for (k, a) in t.artists.iter().enumerate() {
        let name_w = ui.painter().layout_no_wrap(a.name.clone(), regular(size), p.text_weak).size().x;
        let link = Rect::from_min_size(pos2(x, at.y), vec2(name_w, h));
        if link.right() > at.x + width + 0.5 {
            break;
        }
        if a.uri.starts_with("spotify:artist:") {
            let r = ui.interact(link, Id::new(("row-artist", ti, k, at.x as i32)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if r.hovered() {
                painter.hline(link.x_range(), link.bottom() - 1.0, Stroke::new(1.0, p.text_weak));
            }
            if r.clicked() {
                actions.push(RowAction::Open(Source::Artist(a.uri.clone())));
            }
        }
        x = link.right() + comma;
    }
}

#[cfg(test)]
mod tests {
    use egui::{Modifiers, Pos2};

    use super::*;
    use crate::ui::kit::headless::{key, move_to, press, release};

    const ROWS: [u32; 3] = [0, 1, 2];

    /// One pass of a page in a scroll area, as the list's page has it: the hero (36 tall, a button
    /// at x 500 to 560 on it), three songs' rows (30 tall from y 40, x 20 to 580), and a menu at
    /// (300, 250) over the page if `menu`: the clicks and keys taken as the page takes them.
    /// Returns the song a drag started on.
    fn page(ctx: &egui::Context, events: Vec<egui::Event>, mods: Modifiers, v: &mut TrackView, menu: bool) -> Option<usize> {
        let events = std::iter::once(egui::Event::ModifiersChanged(mods)).chain(events).collect();
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 400.0))), events, ..Default::default() };
        let mut dragged = None;
        let mut out = ctx.run_ui(raw, |ui| {
            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let_go_on_nothing(ui, Some(&mut *v));
                let _ = ui.interact(Rect::from_min_size(Pos2::ZERO, vec2(600.0, 36.0)), Id::new(HERO_BACK), Sense::click());
                let _ = ui.interact(Rect::from_min_size(pos2(500.0, 8.0), vec2(60.0, 20.0)), Id::new("hero-button"), Sense::click());
                for (k, &ti) in ROWS.iter().enumerate() {
                    let rect = Rect::from_min_size(pos2(20.0, 40.0 + k as f32 * 30.0), vec2(560.0, 30.0));
                    let resp = ui.interact(rect, Id::new(("song-row", ti)), Sense::click_and_drag());
                    if resp.drag_started() {
                        dragged = Some(ti as usize);
                    }
                    if resp.clicked() {
                        v.click(ti as usize, ui.input(|i| i.modifiers), &ROWS);
                    }
                }
                let _ = song_keys(ui, v, &ROWS, ROWS.len());
            });
            if menu {
                egui::Area::new(Id::new("menu")).fixed_pos(pos2(300.0, 250.0)).show(ui.ctx(), |ui| ui.allocate_space(vec2(120.0, 80.0)));
            }
        });
        out.textures_delta.clear();
        dragged
    }

    fn click(ctx: &egui::Context, at: Pos2, mods: Modifiers, v: &mut TrackView, menu: bool) {
        for events in [move_to(at), press(at), release(at)] {
            page(ctx, events, mods, v, menu);
        }
    }

    fn picks(v: &TrackView) -> (Option<usize>, Vec<usize>, Option<usize>) {
        (v.selected, v.picked.iter().copied().collect(), v.anchor)
    }

    #[test]
    fn a_click_on_nothing_lets_go_as_esc_does() {
        let ctx = egui::Context::default();
        let mut v = TrackView::new(Source::LikedSongs);
        let none = (None, vec![], None);
        page(&ctx, Vec::new(), Modifiers::NONE, &mut v, false);
        let (row0, row1, row2) = (pos2(300.0, 55.0), pos2(300.0, 85.0), pos2(300.0, 115.0));
        let below = pos2(300.0, 300.0);

        // A row: picked. Ctrl adds one; a Ctrl+click on nothing keeps them.
        click(&ctx, row1, Modifiers::NONE, &mut v, false);
        assert_eq!(picks(&v), (Some(1), vec![1], Some(1)));
        click(&ctx, row2, Modifiers::COMMAND, &mut v, false);
        assert_eq!(picks(&v), (Some(2), vec![1, 2], Some(2)));
        click(&ctx, below, Modifiers::COMMAND, &mut v, false);
        assert_eq!(picks(&v), (Some(2), vec![1, 2], Some(2)));
        // Below the rows, and beside them: let go.
        click(&ctx, below, Modifiers::NONE, &mut v, false);
        assert_eq!(picks(&v), none);
        click(&ctx, row0, Modifiers::NONE, &mut v, false);
        click(&ctx, pos2(8.0, 55.0), Modifiers::NONE, &mut v, false);
        assert_eq!(picks(&v), none);
        // A button on the hero keeps them; the hero's own background lets go.
        click(&ctx, row0, Modifiers::NONE, &mut v, false);
        click(&ctx, pos2(530.0, 18.0), Modifiers::NONE, &mut v, false);
        assert_eq!(picks(&v), (Some(0), vec![0], Some(0)));
        click(&ctx, pos2(100.0, 18.0), Modifiers::NONE, &mut v, false);
        assert_eq!(picks(&v), none);

        // Dragging a row out onto nothing is no click: it drags, and the pick stays.
        click(&ctx, row0, Modifiers::NONE, &mut v, false);
        page(&ctx, press(row0), Modifiers::NONE, &mut v, false);
        assert_eq!(page(&ctx, move_to(below), Modifiers::NONE, &mut v, false), Some(0));
        page(&ctx, release(below), Modifiers::NONE, &mut v, false);
        assert_eq!(picks(&v), (Some(0), vec![0], Some(0)));
        // On a menu over the page: not on nothing.
        click(&ctx, pos2(320.0, 270.0), Modifiers::NONE, &mut v, true);
        assert_eq!(picks(&v), (Some(0), vec![0], Some(0)));

        // Esc lets go too.
        page(&ctx, key(egui::Key::Escape), Modifiers::NONE, &mut v, false);
        assert_eq!(picks(&v), none);
    }

    #[test]
    fn more_columns_take_room_from_the_title() {
        let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(1100.0, 46.0));
        let c = Cols { num: NUM_W, cover: true, sub: false, album: 170.0, unblock: false, artist: 160.0, added: 96.0, sz: SongSize::of(1) };
        let parts = c.parts(row);
        assert_eq!(parts.added.right(), parts.time.left() - GAP);
        assert_eq!(parts.album.right(), parts.added.left() - GAP);
        assert_eq!(parts.artist.right(), parts.album.left() - GAP);
        assert_eq!(parts.text.right(), parts.artist.left() - GAP);
        assert_eq!(parts.cover.width(), 40.0);
    }

    #[test]
    fn columns_keep_their_widths_and_the_title_takes_the_rest() {
        let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 38.0));
        let parts = Cols { num: NUM_W, cover: true, sub: true, album: 190.0, unblock: false, artist: 0.0, added: 0.0, sz: SongSize::of(0) }.parts(row);
        assert_eq!(parts.num.width(), NUM_W);
        assert_eq!(parts.heart.right(), 900.0 - ROW_PAD);
        assert_eq!(parts.time.right(), parts.heart.left() - GAP);
        assert_eq!(parts.album.right(), parts.time.left() - GAP);
        assert_eq!(parts.text.left(), parts.cover.right() + GAP);
        assert_eq!(parts.text.right(), parts.album.left() - GAP);
        // No covers and no album (an album's page): the title starts after the number.
        let parts = Cols { num: NUM_W, cover: false, sub: false, album: 0.0, unblock: false, artist: 0.0, added: 0.0, sz: SongSize::of(0) }.parts(row);
        assert_eq!(parts.text.left(), parts.num.right() + GAP);
    }

    #[test]
    fn only_rows_on_screen_are_drawn() {
        // 10,000 rows of 38 from y=200; the screen shows 400..800.
        assert_eq!(visible_rows(200.0, 38.0, 10_000, 400.0, 800.0), (5, 16));
        // Above the rows: from the first.
        assert_eq!(visible_rows(200.0, 38.0, 10_000, 0.0, 300.0), (0, 3));
        // Past the end: none.
        assert_eq!(visible_rows(200.0, 38.0, 3, 1000.0, 1400.0), (3, 3));
    }

    #[test]
    fn normal_windows_get_todays_page_whatever_the_choice() {
        // The middle area from 300 (after the sidebar), 24 at the sides; up to 1848 nothing changes.
        for w in [368.0, 600.0, 1200.0, 1500.0, 1847.0, 1848.0] {
            for wide in [WideScreens::Centred, WideScreens::Fill, WideScreens::Split] {
                for two in [true, false] {
                    let at = Across::new(wide, 300.0, w, PAD_X, two);
                    assert_eq!(at, Across { hero: (324.0, 276.0 + w), songs: (300.0, w), split: false }, "{wide:?} at {w}");
                }
            }
        }
        // Narrow: 16 at the sides.
        assert_eq!(Across::new(WideScreens::Split, 0.0, 400.0, 16.0, true), Across { hero: (16.0, 384.0), songs: (0.0, 400.0), split: false });
    }

    #[test]
    fn centred_keeps_the_page_in_the_middle() {
        // A 34" window: the middle area 2800 wide from 300.
        let at = Across::new(WideScreens::Centred, 300.0, 2800.0, PAD_X, true);
        assert_eq!(at, Across { hero: (800.0, 2600.0), songs: (776.0, 1848.0), split: false });
        // The same room on both sides, for the words and for the songs.
        assert_eq!(at.hero.0 - 300.0, 3100.0 - at.hero.1);
        assert_eq!(at.songs.0 - 300.0, 3100.0 - (at.songs.0 + at.songs.1));
        // A hair over 1848 it starts to centre, without a jump.
        let over = Across::new(WideScreens::Centred, 0.0, 1849.0, PAD_X, true);
        assert_eq!((over.hero.1 - over.hero.0, over.songs.1), (1800.0, 1848.0));
        // 5120 wide (4432 of middle area): still 1800 of words; an artist's page too.
        for two in [true, false] {
            let at = Across::new(WideScreens::Centred, 300.0, 4432.0, PAD_X, two);
            assert_eq!((at.hero.1 - at.hero.0, at.split), (1800.0, false));
        }
        // Fill: the whole width, as today.
        assert_eq!(Across::new(WideScreens::Fill, 300.0, 2800.0, PAD_X, true), Across { hero: (324.0, 3076.0), songs: (300.0, 2800.0), split: false });
    }

    #[test]
    fn split_puts_the_hero_beside_the_songs() {
        // Just under 1900 of content: one column, as Fill.
        let under = Across::new(WideScreens::Split, 300.0, 1947.0, PAD_X, true);
        assert_eq!(under, Across::new(WideScreens::Fill, 300.0, 1947.0, PAD_X, true));
        for w in [1948.0, 2800.0, 3440.0, 4432.0, 5120.0] {
            let at = Across::new(WideScreens::Split, 300.0, w, PAD_X, true);
            assert!(at.split, "{w}");
            // The hero's column: 500 from the content's left edge.
            assert_eq!(at.hero, (324.0, 824.0), "{w}");
            // The songs: 48 after it, to the area's right edge, 24 in from both sides as a page.
            assert_eq!(at.songs.0 + PAD_X, at.hero.1 + wide::GAP, "{w}");
            assert_eq!(at.songs.0 + at.songs.1, 300.0 + w, "{w}");
            // An artist's page doesn't split: it fills.
            assert_eq!(Across::new(WideScreens::Split, 300.0, w, PAD_X, false), Across::new(WideScreens::Fill, 300.0, w, PAD_X, false), "{w}");
        }
        // At the threshold the songs have 1400: room for every column.
        assert_eq!(Across::new(WideScreens::Split, 300.0, 1948.0, PAD_X, true).songs.1, 1400.0);
    }

    #[test]
    fn a_page_narrower_than_its_sides_has_nothing_backwards() {
        for wide in [WideScreens::Centred, WideScreens::Fill, WideScreens::Split] {
            for w in [0.0, 10.0, 31.0, 33.0] {
                let at = Across::new(wide, 0.0, w, 16.0, true);
                assert_eq!((at.songs, at.split), ((0.0, w), false), "{wide:?} at {w}");
                assert!(at.hero.1 >= at.hero.0, "{at:?}");
            }
        }
    }

    #[test]
    fn the_split_hero_stays_in_view() {
        let view = egui::Rangef::new(100.0, 900.0);
        // Not scrolled: where the page starts.
        assert_eq!(hero_top(100.0, view, 600.0), 100.0);
        // Scrolled, however far: at the top of what shows.
        assert_eq!(hero_top(-400.0, view, 600.0), 100.0);
        assert_eq!(hero_top(-450_000.0, view, 600.0), 100.0);
        // Exactly as tall as the view.
        assert_eq!(hero_top(-400.0, view, 800.0), 100.0);
        // Taller than the window: it scrolls until its foot shows, then stays.
        assert_eq!(hero_top(100.0, view, 1000.0), 100.0);
        assert_eq!(hero_top(0.0, view, 1000.0), 0.0);
        assert_eq!(hero_top(-500.0, view, 1000.0), -100.0);
        assert_eq!(hero_top(-500.0, view, 1000.0) + 1000.0, view.max);
    }

    #[test]
    fn the_split_hero_is_as_tall_as_its_parts() {
        let none = split_hero_h(SPLIT_COVER, 14.0, 0);
        assert_eq!(none, HERO_TOP + SPLIT_COVER + 24.0 + 14.0 + 4.0 + 31.5 + 6.0 + 19.0 + 18.0 + 28.0 + HERO_BOTTOM);
        // One row of tiles for one or two numbers, a second row for a third.
        assert_eq!(split_hero_h(SPLIT_COVER, 14.0, 1), split_hero_h(SPLIT_COVER, 14.0, 2));
        assert_eq!(split_hero_h(SPLIT_COVER, 14.0, 2) - none, 22.0 + COUNT_H);
        assert_eq!(split_hero_h(SPLIT_COVER, 14.0, 3) - none, 22.0 + COUNT_H * 2.0 + 10.0);
        // With SHUFFLE and LEFT it still fits a 1080 screen's page with room to spare.
        assert!(split_hero_h(SPLIT_COVER, 14.0, 2) < 700.0);
        // The record stays in the column, even while it plays (0.48 of the cover out, 0.92 wide).
        const { assert!(SPLIT_COVER * (0.48 + 0.92) <= SPLIT_LEFT) };
    }

    #[test]
    fn split_tiles_say_what_their_numbers_are() {
        assert_eq!(tile_words("SHUFFLE", "of 1,989"), "of 1,989 in this shuffle");
        assert_eq!(tile_words("LEFT", "· 59 h 7 min"), "left · 59 h 7 min");
        // A round that isn't playing has no time left to show.
        assert_eq!(tile_words("LEFT", ""), "left");
    }

    #[test]
    fn likes_waiting_count_down_and_go_at_none() {
        let waiting = |likes, unlikes| waiting_text(LikesWaiting { likes, unlikes });
        assert_eq!(waiting(0, 0), "");
        assert_eq!(waiting(1, 0), "1 waiting");
        assert_eq!(waiting(10_000, 0), "10,000 waiting");
        assert_eq!(waiting(0, 12), "12 to remove");
        assert_eq!(waiting(1_240, 1), "1,240 waiting · 1 to remove");
        let liked = Source::LikedSongs;
        let rest = |waiting, tight| meta_rest(&liked, (Some(36_864), 140_100_000, waiting), tight);
        assert_eq!(rest(LikesWaiting { likes: 1_240, unlikes: 0 }, true), "36,864 songs · 1,240 waiting");
        assert_eq!(rest(LikesWaiting { likes: 1_240, unlikes: 12 }, false), "36,864 songs · 1,240 waiting · 12 to remove · 38 h 55 min");
        assert_eq!(rest(LikesWaiting::default(), false), "36,864 songs · 38 h 55 min");
        // Before the songs are known, what waits still shows.
        assert_eq!(meta_rest(&liked, (None, 0, LikesWaiting { likes: 1, unlikes: 0 }), false), "1 waiting");
    }

    #[test]
    fn the_likes_waiting_are_cut_to_the_line_at_any_width() {
        let ctx = egui::Context::default();
        crate::ui::theme::install_fonts(&ctx);
        let p = Palette::build(crate::config::ThemeMode::Light, Color32::from_rgb(30, 215, 96), None);
        let ink = Ink { strong: p.text_strong, text: p.text, weak: p.text_weak, shadow: false };
        let waiting = LikesWaiting { likes: 10_000, unlikes: 12 };
        let rest = format!(" · {}", meta_rest(&Source::LikedSongs, (Some(36_864), 2_100 * 3_600_000, waiting), false));
        let mut full = 0.0;
        crate::ui::kit::headless::pass(&ctx, Vec::new(), |ui| full = meta_line(ui, "", &rest, ink, f32::INFINITY).size().x);
        assert!(full > 100.0);
        for width in [1.0, 12.0, full / 2.0, full + 1.0, 559.0, 560.0, 3440.0, 5120.0] {
            crate::ui::kit::headless::pass(&ctx, Vec::new(), |ui| {
                let owner = meta_line(ui, "Sam 🍄 with a very long name that goes on and on", "", ink, width);
                let line = meta_line(ui, "", &rest, ink, width);
                // (Narrower than the "…" itself, the "…" still shows; egui lets the last letter
                // overhang a little.)
                let room = width.max(24.0) + 4.0;
                assert!(owner.size().x <= room && line.size().x <= room, "{width}");
                if width <= full / 2.0 {
                    assert!(line.elided, "{width}");
                } else {
                    assert!(!line.elided && line.text().contains("10,000 waiting · 12 to remove"), "{width}");
                }
            });
        }
    }
}
