//! Home (the Final design's `pgHome`): "Good afternoon, wisp" with All / Playlists / Albums / Artists, the
//! quick tiles (lists you were in, with how far you got), Recently played as a cover-flow (the middle cover
//! big, the rest smaller and dimmer, stepping only on a click or ← →), then For you (mixes, with moods) and
//! New releases. Sizes are the prototype's; wider windows get more (see `wide`). Split: the greeting, the
//! tiles and Recently played on the left, For you and New releases on the right.

use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::fin;
use super::motion;
use super::sidebar::{OnTop, Raised};
use super::svgicon;
use super::theme::{bold, heavy, medium, regular};
use super::tracks::{liked_color, one_line};
use super::util::fmt_thousands;
use super::wide;
use super::{MiddleTab, TruffleApp};
use crate::model::Source;
use crate::text::{contains_folded, fold};

/// Lists opened that Home remembers (kept across restarts).
pub(super) const RECENT_MAX: usize = 20;

/// What Home shows: the chip, the cover-flow's turn and the mood.
#[derive(Default)]
pub(super) struct HomeState {
    chip: Chip,
    flow: i64,
    /// Where the cover-flow is drawn: eases toward `flow`, so the covers slide.
    flow_shown: f32,
    mood: Option<&'static str>,
    /// For you shows every mix (else one row).
    all_mixes: bool,
    /// How much smaller the covers are drawn for the page to end in the window (0: as designed), in
    /// the page's column and, side by side, the right one. See [`fit_shrink`].
    shrink: [f32; 2],
}

/// A column's covers: the scale they are drawn at, and what of its height they make, as designed and
/// as drawn: what fitting the page in the window can take from.
#[derive(Clone, Copy, Default, Debug)]
struct Covers {
    scale: f32,
    designed: f32,
    drawn: f32,
}

/// The covers shrink for the page to fit the window down to 70 % of their size; a page that needs
/// more than that scrolls, as there is more to see.
const FIT_LEAST: f32 = 0.7;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Chip {
    #[default]
    All,
    Playlists,
    Albums,
    Artists,
}

/// A list on Home.
#[derive(Clone)]
struct Item {
    source: Source,
    name: String,
    kind: &'static str,
    image: Option<String>,
    songs: Option<u32>,
}

const MOODS: [&str; 5] = ["Calm", "Loud", "Night", "Focus", "Upbeat"];

/// Split: the left column's width (the greeting, the tiles and Recently played).
const SPLIT_LEFT: f32 = 1060.0;

/// Recently played's box: above the covers, under them (the ledge and the caption), and between them.
const FLOW_TOP: f32 = 16.0;
const FLOW_UNDER: f32 = 60.0;
const FLOW_GAP: f32 = 14.0;

impl TruffleApp {
    pub(super) fn show_home(&mut self) {
        self.middle_tab = MiddleTab::Home;
    }

    fn home_item(&self, source: &Source) -> Option<Item> {
        match source {
            Source::LikedSongs => Some(Item { source: source.clone(), name: "Liked Songs".into(), kind: "Collection", image: None, songs: Some(self.liked.len() as u32) }),
            Source::Playlist(uri) => self.playlist(uri).map(|pl| Item { source: source.clone(), name: pl.name.clone(), kind: "Playlist", image: pl.image_url.clone(), songs: pl.track_count }),
            Source::Album(_) | Source::Artist(_) => {
                let info = self.source_info.get(source)?;
                let kind = if matches!(source, Source::Album(_)) { "Album" } else { "Artist" };
                Some(Item { source: source.clone(), name: info.title.clone(), kind, image: info.image.clone(), songs: None })
            }
            Source::Mix(id) => {
                let m = self.mixes.iter().find(|m| m.id == *id)?;
                Some(Item { source: source.clone(), name: m.name.clone(), kind: "Mix", image: m.image.clone(), songs: Some(m.songs as u32) })
            }
        }
    }

    /// Lists for the tiles and the cover-flow: the ones opened lately, then the pinned ones and Liked Songs.
    fn home_recent(&self, chip: Chip) -> Vec<Item> {
        let mut seen: Vec<Source> = Vec::new();
        let mut out = Vec::new();
        let pinned: Vec<Source> = self.prefs.pinned_playlists.iter().map(|u| Source::Playlist(u.clone())).collect();
        let playing = self.playback.source.clone();
        let candidates = playing.iter().chain(self.recent_sources.iter()).chain(pinned.iter()).chain(std::iter::once(&Source::LikedSongs)).chain(
            self.playlists.iter().map(|pl| Source::Playlist(pl.uri.clone())).collect::<Vec<_>>().iter(),
        ).cloned().collect::<Vec<_>>();
        for s in candidates {
            if seen.contains(&s) {
                continue;
            }
            let fits = match chip {
                Chip::All => true,
                Chip::Playlists => matches!(s, Source::Playlist(_) | Source::LikedSongs),
                Chip::Albums => matches!(s, Source::Album(_)),
                Chip::Artists => matches!(s, Source::Artist(_)),
            };
            if !fits {
                continue;
            }
            if let Some(item) = self.home_item(&s) {
                seen.push(s);
                out.push(item);
            }
        }
        out
    }

    pub(super) fn home_view(&mut self, ui: &mut Ui) {
        // New releases need the most liked artists: the collection is worked out once.
        if !self.collection_asked {
            self.collection_asked = true;
            self.send(crate::model::Command::Collection);
        }
        let r = ui.max_rect();
        egui::ScrollArea::vertical().id_salt("home-fin").auto_shrink(false).show(ui, |ui| {
            // 24 at the sides (16 at narrow widths), within the column Wide screens gives.
            let pad = if r.width() < 560.0 { 16.0 } else { 24.0 };
            let col = wide::Column::new(self.prefs.wide_screens, r.left(), r.width(), pad, true);
            let start = ui.cursor().top();
            let top = start + 20.0;
            let mut covers = self.home.shrink.map(|s| Covers { scale: 1.0 - s, ..Covers::default() });
            let ends = if col.split {
                // For you's heading level with the greeting.
                let ((left, lw), (right, rw)) = col.halves(SPLIT_LEFT);
                let mut y = self.home_head(ui, left, lw, top);
                y = self.home_tiles(ui, left, lw, y, true);
                y = self.home_flow(ui, left, lw, y, &mut covers[0]);
                let mut ry = self.home_for_you(ui, right, rw, col.width, top - 26.0, &mut covers[1]);
                ry = self.home_new_releases(ui, right, rw, col.width, ry, 2, &mut covers[1]);
                [y, ry]
            } else {
                let (left, cw) = (col.left, col.width);
                let mut y = self.home_head(ui, left, cw, top);
                y = self.home_tiles(ui, left, cw, y, false);
                y = self.home_flow(ui, left, cw, y, &mut covers[0]);
                y = self.home_for_you(ui, left, cw, cw, y, &mut covers[0]);
                [self.home_new_releases(ui, left, cw, cw, y, 1, &mut covers[0]), start]
            };
            let bottom = ends[0].max(ends[1]);
            ui.allocate_space(vec2(col.width, (bottom - start + 28.0).max(0.0)));
            // A page a little taller than the window fits it, its covers smaller: no scrolling for
            // nothing but the last captions. Redone at once, so the taller one never shows.
            let mut changed = false;
            for (c, end) in ends.into_iter().enumerate() {
                let want = fit_shrink(r.height(), end - start + 28.0, covers[c]);
                if (want - self.home.shrink[c]).abs() > 1e-3 {
                    self.home.shrink[c] = want;
                    changed = true;
                }
            }
            if changed {
                ui.ctx().request_discard("Home's covers fit the page in the window");
            }
        });
    }

    /// The greeting (22 bold) and the date (11.5 muted) on the left, the chips on the right.
    fn home_head(&mut self, ui: &mut Ui, left: f32, cw: f32, y: f32) -> f32 {
        let p = self.palette;
        let hour = chrono_hour();
        let part = if (5..12).contains(&hour) { "morning" } else if (12..18).contains(&hour) { "afternoon" } else { "evening" };
        let name = self.profile.as_ref().map(|u| u.display_name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "there".into());
        let greet = fin::spaced(ui, &format!("Good {part}, {name}"), heavy(22.0), p.text_strong, -0.33);
        let date = ui.fonts_mut(|f| f.layout_no_wrap(date_text(), regular(11.5), p.text_weak));
        let gh = greet.size().y;
        let gw = greet.size().x;
        let chips: Vec<(Chip, String, std::sync::Arc<egui::Galley>)> = [Chip::All, Chip::Playlists, Chip::Albums, Chip::Artists]
            .into_iter()
            .map(|chip| {
                let label = format!("{chip:?}");
                let g = ui.fonts_mut(|f| f.layout_no_wrap(label.clone(), medium(11.5), p.text));
                (chip, label, g)
            })
            .collect();
        let chips_w: f32 = chips.iter().map(|(_, _, g)| g.size().x + 24.0 + 8.0).sum::<f32>() - 8.0;
        // Narrow (the chips don't fit beside the greeting): the chips go under it, without the date.
        let stacked = left + gw + 24.0 + chips_w > left + cw;
        ui.painter().galley(pos2(left, y), greet, p.text_strong);
        let (block_h, chips_y, mut x) = if stacked {
            (gh + 12.0 + 26.0, y + gh + 12.0, left)
        } else {
            let block_h = gh + 3.0 + date.size().y;
            ui.painter().galley(pos2(left, y + gh + 3.0), date, p.text_weak);
            // Bottom-aligned with the greeting block (`align-items: flex-end`).
            (block_h, y + block_h - 26.0, left + cw - chips_w)
        };
        for (chip, label, g) in chips {
            let w = g.size().x + 24.0;
            let rect = Rect::from_min_size(pos2(x, chips_y), vec2(w, 26.0));
            if chip_at(ui, &p, rect, Id::new(("home-chip", &label)), g, self.home.chip == chip) {
                self.home.chip = chip;
                self.home.flow = 0;
            }
            x = rect.right() + 8.0;
        }
        y + block_h
    }

    /// The quick tiles (`.qt`): two rows of [`tile_cols`] (`split`: four rows of two), 52 tall, 8 apart,
    /// 18 under the greeting.
    fn home_tiles(&mut self, ui: &mut Ui, left: f32, cw: f32, y: f32, split: bool) -> f32 {
        let p = self.palette;
        let (cols, rows) = if split { (2, 4) } else { (tile_cols(cw), 2) };
        let items: Vec<Item> = self.home_recent(self.home.chip).into_iter().take(cols * rows).collect();
        let top = y + 18.0;
        let tw = (cw - 8.0 * (cols as f32 - 1.0)) / cols as f32;
        for (i, item) in items.iter().enumerate() {
            let rect = Rect::from_min_size(pos2(left + (i % cols) as f32 * (tw + 8.0), top + (i / cols) as f32 * (52.0 + 8.0)), vec2(tw, 52.0));
            let resp = ui.interact(rect, Id::new(("tile", &item.source)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let on = self.playback.source.as_ref() == Some(&item.source);
            ui.painter().rect_filled(rect, CornerRadius::same(6), if resp.hovered() { p.hover_bg } else { p.raised_bg });
            let cover = Rect::from_min_size(rect.min, vec2(52.0, 52.0));
            self.home_cover(ui, cover, item, 6);
            let (sub, prog) = self.tile_progress(item, on);
            let right = rect.right() - 12.0 - if on { 16.0 + 8.0 } else { 0.0 };
            let tx = cover.right() + 11.0;
            let name = one_line(ui, &item.name, bold(12.0), p.text_strong, (right - tx).max(20.0), false);
            let subg = one_line(ui, &sub, regular(10.5), p.text_weak, (right - tx).max(20.0), false);
            let h = name.size().y + 2.0 + subg.size().y;
            let ty = rect.center().y - h / 2.0;
            let nh = name.size().y;
            ui.painter().galley(pos2(tx, ty), name, p.text_strong);
            ui.painter().galley(pos2(tx, ty + nh + 2.0), subg, p.text_weak);
            // The progress line along the bottom, from the cover to the edge.
            let bar = Rect::from_min_max(pos2(cover.right(), rect.bottom() - 2.0), rect.right_bottom());
            ui.painter().rect_filled(bar, CornerRadius::ZERO, if p.dark { Color32::from_white_alpha(13) } else { Color32::from_black_alpha(13) });
            if prog > 0.0 {
                let fill = Rect::from_min_max(bar.min, pos2(bar.left() + bar.width() * prog.min(1.0), bar.bottom()));
                ui.painter().rect_filled(fill, CornerRadius::ZERO, if on { p.accent_ink } else { Color32::from_rgb(106, 106, 115) });
            }
            if on {
                ui.painter().rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.5, p.accent_ink), StrokeKind::Inside);
                svgicon::paint(ui.painter(), Rect::from_min_size(pos2(rect.right() - 12.0 - 16.0, rect.center().y - 8.0), vec2(16.0, 16.0)), "eq", p.accent_ink, svgicon::STROKE);
            } else {
                ui.painter().rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, p.border), StrokeKind::Inside);
            }
            if resp.clicked() {
                self.open_source(item.source.clone(), false);
            }
        }
        let rows = items.len().div_ceil(cols);
        if rows == 0 { top } else { top + rows as f32 * 60.0 - 8.0 }
    }

    /// "Song 706 of 890" for the playing list, "Song 41 of 503" for a round under way, else its size.
    fn tile_progress(&self, item: &Item, on: bool) -> (String, f32) {
        if on && let Some(d) = self.deck.as_ref().filter(|d| !d.order.is_empty()) {
            let at = (d.now + 1).min(d.order.len());
            return (format!("Song {} of {}", fmt_thousands(at), fmt_thousands(d.order.len())), at as f32 / d.order.len() as f32);
        }
        if let (Some(&heard), Some(total)) = (self.round_progress.get(&item.source.cache_key()), item.songs)
            && heard > 0
            && total > 0
        {
            return (format!("Song {} of {}", fmt_thousands(heard), fmt_thousands(total as usize)), heard as f32 / total as f32);
        }
        match item.songs {
            Some(n) => (format!("{} songs", fmt_thousands(n as usize)), 0.0),
            None => (item.kind.to_owned(), 0.0),
        }
    }

    /// A list's square picture: its cover, Liked Songs' heart, or a note.
    fn home_cover(&mut self, ui: &Ui, rect: Rect, item: &Item, radius: u8) {
        let p = self.palette;
        if item.source == Source::LikedSongs {
            let p = self.palette;
            ui.painter().rect_filled(rect, CornerRadius::same(radius), liked_color(&p));
            let h = rect.width() * 0.42;
            svgicon::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(h, h)), "heartf", Color32::WHITE, svgicon::STROKE);
        } else if !self.images.paint(ui, item.image.as_deref(), rect, radius, p.widget_bg) {
            let h = (rect.width() * 0.36).max(10.0);
            svgicon::paint(ui.painter(), Rect::from_center_size(rect.center(), vec2(h, h)), "note", p.text_weak, svgicon::STROKE);
        }
    }

    /// A list's cover as a sleeve with its record peeking out (Liked Songs, any playlist, a mix),
    /// like the sidebar's; an album's or artist's as a plain cover.
    fn home_sleeve(&mut self, ui: &Ui, cover: Rect, item: &Item, hovered: bool) -> Option<Raised> {
        if !matches!(item.source, Source::Playlist(_) | Source::LikedSongs | Source::Mix(_)) {
            self.home_cover(ui, cover, item, 4);
            ui.painter().rect_stroke(cover, CornerRadius::same(4), Stroke::new(1.0, Color32::from_white_alpha(15)), StrokeKind::Inside);
            return None;
        }
        let playing = self.playback.track.is_some() && self.playback.source.as_ref() == Some(&item.source);
        let sleeve = super::sidebar::Sleeve {
            source: item.source.clone(),
            name: item.name.clone(),
            image: item.image.clone(),
            count: item.songs,
            uri: None,
            editable: false,
        };
        self.paint_sleeve(ui, cover, &sleeve, playing, hovered, 0.20)
    }

    /// A section heading (`.hdr`): 13.5 semibold, 26 under what came before, things on its right.
    fn home_section(&mut self, ui: &mut Ui, left: f32, cw: f32, y: f32, title: &str) -> (f32, Rect) {
        let p = self.palette;
        let top = y + 26.0;
        let row = Rect::from_min_size(pos2(left, top), vec2(cw, 24.0));
        let g = ui.fonts_mut(|f| f.layout_no_wrap(title.to_owned(), bold(13.5), p.text_strong));
        let gh = g.size().y;
        ui.painter().galley(pos2(left, row.center().y - gh / 2.0), g, p.text_strong);
        (row.bottom() + 10.0, row)
    }

    /// Recently played as a cover-flow (`.flow`): the covers of [`flow_sizes`], the ones further out
    /// smaller and dimmer; the middle one's name, what it is and PLAY under them. The box is as tall as
    /// the middle cover needs. Its covers are drawn at `covers`' scale, and counted in it.
    fn home_flow(&mut self, ui: &mut Ui, left: f32, cw: f32, y: f32, covers: &mut Covers) -> f32 {
        let p = self.palette;
        let items = self.home_recent(self.home.chip);
        if items.is_empty() {
            return y;
        }
        let (top, head) = self.home_section(ui, left, cw, y, "Recently played");
        if fin::link(ui, head, Id::new("flow-all"), "Show all") {
            self.note("The whole list of what you played comes with the Albums page", None);
        }
        let l = items.len();
        let (mut sizes, alphas) = flow_sizes(cw, l);
        let n = sizes.len();
        let half = (n - 1) / 2;
        covers.designed += sizes[half];
        if covers.scale < 1.0 {
            sizes.iter_mut().for_each(|s| *s *= covers.scale);
        }
        let big = sizes[half];
        covers.drawn += big;
        let box_ = Rect::from_min_size(pos2(left, top), vec2(cw, FLOW_TOP + big + FLOW_UNDER));
        // `radial-gradient(55% 80% at 50% 38%, #1a1a1e, #111113 75%)`, and a 1 px line inside. The
        // light theme: a white glow on its faint grey.
        let (stage, light) = if p.dark { (Color32::from_rgb(17, 17, 19), Color32::from_rgb(26, 26, 30)) } else { (p.faint_bg, p.raised_bg) };
        ui.painter().rect_filled(box_, CornerRadius::same(10), stage);
        let glow = Rect::from_center_size(pos2(box_.center().x, box_.top() + box_.height() * 0.38), vec2(box_.width() * 0.8, box_.height() * 1.1));
        fin::radial_glow(ui.painter(), glow, light, box_);
        ui.painter().rect_stroke(box_, CornerRadius::same(10), Stroke::new(1.0, p.border), StrokeKind::Inside);

        // Covers sliding in and out past the ends are cut at the box's edge (`overflow: hidden`).
        let clip = ui.clip_rect();
        ui.set_clip_rect(box_.intersect(clip));
        let total_w: f32 = sizes.iter().sum::<f32>() + FLOW_GAP * (n as f32 - 1.0);
        let row_bottom = box_.top() + FLOW_TOP + big;
        let mut slot_x = Vec::with_capacity(n);
        let mut x = box_.center().x - total_w / 2.0;
        for &sz in &sizes {
            slot_x.push(x);
            x += sz + FLOW_GAP;
        }
        // The covers slide: the drawn place eases toward the chosen one. With Windows' animations
        // off they don't: the row fades out at the old places as it fades in at the new ones.
        let (was, fade) = motion::crossfade(ui.ctx(), Id::new("flow-places"), self.home.flow, motion::TAB);
        let rows = if motion::moves() || fade >= 1.0 { vec![(None, 1.0)] } else { vec![(Some(was as f32), 1.0 - fade), (None, fade)] };
        let target = self.home.flow as f32;
        let dt = ui.input(|i| i.stable_dt).min(0.1);
        let shown = &mut self.home.flow_shown;
        if (*shown - target).abs() > 3.5 || !super::kit::animations_on() {
            *shown = target;
        } else {
            *shown += (target - *shown) * (1.0 - (-dt * 11.0).exp());
            if (*shown - target).abs() < 0.002 {
                *shown = target;
            } else {
                ui.ctx().request_repaint();
            }
        }
        let mut step: Option<i64> = None;
        let mut open_mid = false;
        // The old places only show (fading out); the new ones take clicks.
        for (old, row_fade) in rows {
            let c = old.unwrap_or(self.home.flow_shown);
            // Every cover near the middle, at its place between the slots (`t`: 0 is the leftmost slot);
            // the outer ones shrink and fade out past the ends. Drawn from the outside in.
            let mut drawn: Vec<(i64, f32)> = ((c.floor() as i64 - half as i64 - 1)..=(c.ceil() as i64 + half as i64 + 1))
                .map(|i| (i, i as f32 - c + half as f32))
                .filter(|(_, t)| *t > -1.0 && *t < n as f32)
                .collect();
            drawn.sort_by(|a, b| (b.1 - half as f32).abs().total_cmp(&(a.1 - half as f32).abs()));
            for (i, t) in drawn {
                let item = &items[i.rem_euclid(l as i64) as usize];
                let (left, sz, alpha) = flow_slot(t, &sizes, &alphas, &slot_x);
                if alpha <= 0.01 || sz < 4.0 {
                    continue;
                }
                let alpha = alpha * row_fade;
                let rect = Rect::from_min_size(pos2(left, row_bottom - sz), vec2(sz, sz));
                let resp = old.is_none().then(|| ui.interact(rect, Id::new(("flow", i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text(&item.name));
                // The shadow under it, then the cover, fainter further out.
                let shadow = Color32::from_black_alpha((140.0 * alpha) as u8);
                ui.painter().add(egui::Shadow { offset: [0, 12], blur: 26, spread: 0, color: shadow }.as_shape(rect, CornerRadius::same(4)));
                let mut faded = ui.new_child(egui::UiBuilder::new().max_rect(box_));
                faded.set_clip_rect(box_.intersect(clip));
                faded.multiply_opacity(alpha);
                self.home_cover(&faded, rect, item, 4);
                let middle = (t - half as f32).abs() < 0.5;
                let ring = if middle { Color32::from_white_alpha(36) } else { Color32::from_white_alpha(15) };
                faded.painter().rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, ring), StrokeKind::Inside);
                if resp.is_some_and(|r| r.clicked()) {
                    // The middle one opens; another one slides to the middle.
                    if middle {
                        open_mid = true;
                    } else {
                        step = Some((t - half as f32).round() as i64);
                    }
                }
            }
        }
        ui.set_clip_rect(clip);
        // The ledge under the covers.
        let ledge_y = row_bottom + 1.0;
        let ledge = if p.dark { Color32::from_rgb(58, 58, 65) } else { p.field_border };
        fin::hfade_line(ui.painter(), (box_.left() + 40.0)..=(box_.right() - 40.0), ledge_y, ledge);
        // The arrows, 28 round, 10 in, level with the middle cover (78 down beside a 150 one).
        for (dx, icon, d) in [(10.0, "back", -1i64), (-38.0, "fwd", 1i64)] {
            let at = if dx > 0.0 { box_.left() + dx } else { box_.right() + dx };
            let rect = Rect::from_min_size(pos2(at, box_.top() + FLOW_TOP + big / 2.0 - 13.0), vec2(28.0, 28.0));
            let resp = ui.interact(rect, Id::new(("flow-arrow", d)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            ui.painter().circle_filled(rect.center(), 14.0, if resp.hovered() { p.widget_hover_bg } else { p.widget_bg });
            ui.painter().circle_stroke(rect.center(), 13.5, Stroke::new(1.0, p.border));
            svgicon::paint(ui.painter(), rect.shrink(7.5), icon, p.text, svgicon::STROKE);
            if resp.on_hover_text(if d < 0 { "Previous (←)" } else { "Next (→)" }).clicked() {
                step = Some(d);
            }
        }
        if !ui.ctx().egui_wants_keyboard_input() {
            let (lft, rgt) = ui.input(|i| {
                (super::keys::pressed(i, egui::Modifiers::NONE, egui::Key::ArrowLeft), super::keys::pressed(i, egui::Modifiers::NONE, egui::Key::ArrowRight))
            });
            if lft {
                step = Some(-1);
            } else if rgt {
                step = Some(1);
            }
        }
        if let Some(d) = step {
            self.home.flow += d;
        }
        if open_mid {
            let source = items[self.home.flow.rem_euclid(l as i64) as usize].source.clone();
            self.open_source(source, false);
            // Opening it puts it first in Recently played: it stays in the middle, without a slide.
            self.home.flow = 0;
            self.home.flow_shown = 0.0;
        }
        // The caption: the middle one's name, what it is, and PLAY.
        let mid = &items[(self.home.flow.rem_euclid(l as i64)) as usize];
        let sub = match (&mid.source, mid.songs) {
            (Source::Playlist(_) | Source::LikedSongs, Some(n)) => format!("{} · {} songs", mid.kind.to_lowercase(), fmt_thousands(n as usize)),
            _ => mid.kind.to_lowercase(),
        };
        let name = ui.fonts_mut(|f| f.layout_no_wrap(mid.name.clone(), bold(13.0), p.text_strong));
        let subg = ui.fonts_mut(|f| f.layout_no_wrap(sub, regular(12.0), p.text_weak));
        let play_w = 14.0 + 13.0 + 7.0 + 30.0 + 14.0;
        let total = name.size().x + 10.0 + subg.size().x + 10.0 + play_w;
        let cy = row_bottom + 28.0;
        let mut cx = box_.center().x - total.min(box_.width() - 24.0) / 2.0;
        let (nw, nh) = (name.size().x, name.size().y);
        ui.painter().galley(pos2(cx, cy - nh / 2.0), name, p.text_strong);
        cx += nw + 10.0;
        let (sw, sh) = (subg.size().x, subg.size().y);
        ui.painter().galley(pos2(cx, cy - sh / 2.0), subg, p.text_weak);
        cx += sw + 10.0;
        let play = fin::text_button(ui, &p, pos2(cx, cy - 12.0), Id::new("flow-play"), "Play", Some("play"), true, true);
        if play.clicked() {
            let source = mid.source.clone();
            self.open_source(source.clone(), false);
            self.send(crate::model::Command::PlaySource { source, start_index: None, shuffle: true });
        }
        box_.bottom()
    }

    /// For you (`.cards`): the mixes, at least 136 across at 740 and wider (104 / 100 below, more on
    /// wide pages, see [`card_min`]), as many as fit, with the mood chips after the heading.
    /// Spotify's own mixes saved in the library.
    fn home_for_you(&mut self, ui: &mut Ui, left: f32, cw: f32, page_w: f32, y: f32, covers: &mut Covers) -> f32 {
        let p = self.palette;
        if self.home.chip == Chip::Albums || self.home.chip == Chip::Artists {
            return y;
        }
        let mood = self.home.mood;
        let needle = mood.map(fold);
        // Truffle's own mixes first (they have no mood), then the Spotify mixes in the library.
        let own: Vec<Item> = if mood.is_none() {
            self.mixes
                .iter()
                .map(|m| Item { source: Source::Mix(m.id.clone()), name: m.name.clone(), kind: "Mix", image: m.image.clone(), songs: Some(m.songs as u32) })
                .collect()
        } else {
            Vec::new()
        };
        let spotify: Vec<Item> = self
            .playlists
            .iter()
            .filter(|pl| pl.owner_name == "Spotify")
            .filter(|pl| needle.as_deref().is_none_or(|m| contains_folded(&pl.name, m) || contains_folded(&pl.description, m)))
            .map(|pl| Item { source: Source::Playlist(pl.uri.clone()), name: pl.name.clone(), kind: "Playlist", image: pl.image_url.clone(), songs: pl.track_count })
            .collect();
        let mut mixes: Vec<Item> = own.into_iter().chain(spotify).collect();
        if mixes.is_empty() && mood.is_none() {
            return y;
        }
        let (top, head) = self.home_section(ui, left, cw, y, "For you");
        // The mood chips after the title (24 tall, 10 either side, 11 px).
        let title_w = ui.fonts_mut(|f| f.layout_no_wrap("For you".to_owned(), bold(13.5), Color32::WHITE)).size().x;
        let mut x = head.left() + title_w + 10.0 + 6.0;
        for m in std::iter::once(None).chain(MOODS.iter().map(|m| Some(*m))) {
            let label = m.unwrap_or("All");
            let g = ui.fonts_mut(|f| f.layout_no_wrap(label.to_owned(), medium(11.0), p.text));
            let w = g.size().x + 20.0;
            if x + w > head.right() - 60.0 {
                break;
            }
            let rect = Rect::from_min_size(pos2(x, head.center().y - 12.0), vec2(w, 24.0));
            if chip_at(ui, &p, rect, Id::new(("mood", label)), g, mood == m) {
                self.home.mood = m;
            }
            x = rect.right() + 8.0;
        }
        if fin::link(ui, head, Id::new("foryou-all"), if self.home.all_mixes { "Show less" } else { "Show all" }) {
            self.home.all_mixes = !self.home.all_mixes;
        }
        // Each card is as wide as its sleeve and the record peeking out of it; they grow a little
        // so the row ends at the right edge.
        let gap = 14.0;
        // Shown all, they take the rows they need at their size; one row is drawn at `scale`.
        let scale = if self.home.all_mixes { 1.0 } else { covers.scale };
        let (fit, card_w, designed_w) = fitted_row(cw, (card_min(cw, page_w) * 1.24).round(), gap, scale);
        let size = (card_w / 1.24).round();
        if !self.home.all_mixes {
            mixes.truncate(fit);
        }
        if mixes.is_empty() {
            ui.painter().text(pos2(left, top + 6.0), Align2::LEFT_TOP, "No mixes for this mood", regular(12.0), p.text_weak);
            return top + 30.0;
        }
        if !self.home.all_mixes {
            covers.designed += (designed_w / 1.24).round();
            covers.drawn += size;
        }
        let mut bottom = top;
        let row_h = size + 7.0 + 15.0 + 3.0 + 14.0 + 18.0;
        let mut on_top = OnTop::default();
        for (i, item) in mixes.iter().enumerate() {
            let at = pos2(left + (i % fit) as f32 * (card_w + gap), top + (i / fit) as f32 * row_h);
            let rect = Rect::from_min_size(at, vec2(size, size));
            let card = Rect::from_min_max(rect.min, pos2(rect.left() + card_w, rect.bottom() + 7.0 + 15.0 + 3.0 + 14.0));
            let resp = ui.interact(card, Id::new(("mix", &item.source)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            on_top.add(self.home_sleeve(ui, rect, item, resp.hovered()));
            let name = one_line(ui, &item.name, medium(12.0), if resp.hovered() { p.text_strong } else { p.text }, card_w, false);
            let nh = name.size().y;
            ui.painter().galley(pos2(rect.left(), rect.bottom() + 7.0), name, p.text);
            let sub = item.songs.map(|n| format!("{} songs", fmt_thousands(n as usize))).unwrap_or_default();
            let sg = one_line(ui, &sub, regular(11.0), p.text_weak, card_w, false);
            ui.painter().galley(pos2(rect.left(), rect.bottom() + 7.0 + nh + 3.0), sg, p.text_weak);
            if resp.clicked() {
                self.open_source(item.source.clone(), false);
            }
            bottom = card.bottom();
        }
        on_top.lift(ui.ctx());
        bottom
    }
}

impl TruffleApp {
    /// New releases: albums and singles of the last 90 days by the most liked artists, `rows` rows of
    /// as many as fit (see [`card_min`]); NEW on the ones of the last two weeks (`.tag`).
    #[allow(clippy::too_many_arguments)]
    fn home_new_releases(&mut self, ui: &mut Ui, left: f32, cw: f32, page_w: f32, y: f32, rows: usize, covers: &mut Covers) -> f32 {
        let p = self.palette;
        if self.home.chip == Chip::Playlists || self.home.chip == Chip::Artists {
            return y;
        }
        let Some(releases) = self.new_releases.clone().filter(|r| !r.is_empty()) else { return y };
        let (top, head) = self.home_section(ui, left, cw, y, "New releases");
        if fin::link(ui, head, Id::new("releases-all"), "Show all") {
            self.show_collection(MiddleTab::Albums);
        }
        let gap = 18.0;
        // The covers grow a little so the row ends at the right edge; drawn at `scale`, more fit.
        let (fit, size, designed) = fitted_row(cw, card_min(cw, page_w), gap, covers.scale);
        let shown = releases.len().min(fit * rows).div_ceil(fit) as f32;
        covers.designed += designed * shown;
        covers.drawn += size * shown;
        let row_h = size + 7.0 + 15.0 + 3.0 + 14.0 + 18.0;
        let mut bottom = top;
        let mut open = None;
        for (i, r) in releases.iter().take(fit * rows).enumerate() {
            let rect = Rect::from_min_size(pos2(left + (i % fit) as f32 * (size + gap), top + (i / fit) as f32 * row_h), vec2(size, size));
            let card = Rect::from_min_max(rect.min, pos2(rect.right(), rect.bottom() + 7.0 + 15.0 + 3.0 + 14.0));
            let resp = ui.interact(card, Id::new(("release-home", &r.uri)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if !self.images.paint(ui, r.image.as_deref(), rect, 4, p.widget_bg) {
                svgicon::paint(ui.painter(), rect.shrink(size * 0.32), "album", p.text_weak, svgicon::STROKE);
            }
            ui.painter().rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, Color32::from_white_alpha(15)), StrokeKind::Inside);
            if r.days_ago <= 14 {
                // The chrome NEW tag, 6 in from the corner.
                let g = fin::spaced(ui, "NEW", bold(9.0), Color32::from_rgb(21, 23, 29), 0.8);
                let tag = Rect::from_min_size(rect.min + vec2(6.0, 6.0), g.size() + vec2(10.0, 4.0));
                ui.painter().add(egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(150) }.as_shape(tag, CornerRadius::same(3)));
                fin::vgradient(ui.painter(), Rect::from_min_max(tag.min, pos2(tag.right(), tag.center().y)), Color32::from_rgb(248, 250, 253), Color32::from_rgb(199, 206, 219));
                fin::vgradient(ui.painter(), Rect::from_min_max(pos2(tag.left(), tag.center().y), tag.max), Color32::from_rgb(143, 152, 170), Color32::from_rgb(227, 232, 239));
                ui.painter().galley(tag.min + vec2(5.0, 2.0), g, Color32::from_rgb(21, 23, 29));
            }
            let name = one_line(ui, &r.name, medium(12.0), if resp.hovered() { p.text_strong } else { p.text }, size, false);
            let nh = name.size().y;
            ui.painter().galley(pos2(rect.left(), rect.bottom() + 7.0), name, p.text);
            let when = match r.days_ago {
                0 => "today".to_owned(),
                1 => "yesterday".to_owned(),
                d if d < 14 => format!("{d} days ago"),
                d => format!("{} weeks ago", d / 7),
            };
            let sg = one_line(ui, &format!("{} · {} · {when}", r.artist, r.kind), regular(11.0), p.text_weak, size, false);
            ui.painter().galley(pos2(rect.left(), rect.bottom() + 7.0 + nh + 3.0), sg, p.text_weak);
            if resp.clicked() {
                open = Some(r.uri.clone());
            }
            bottom = card.bottom();
        }
        if let Some(uri) = open {
            self.open_source(Source::Album(uri), false);
        }
        bottom
    }
}

/// [`wide::row_fit`] with the cards at most `scale` of the width it gives them, as many more as that
/// takes to fill the row: how many, how wide, and how wide as designed.
fn fitted_row(cw: f32, min: f32, gap: f32, scale: f32) -> (usize, f32, f32) {
    let (n, w) = wide::row_fit(cw, min, gap);
    if scale >= 1.0 {
        return (n, w, w);
    }
    let more = (((cw + gap) / (w * scale + gap) - 1e-4).ceil() as usize).max(n);
    (more, (cw - gap * (more - 1) as f32) / more as f32, w)
}

/// How much smaller a column's covers are drawn (none, or up to 1 − [`FIT_LEAST`]) for it to end in a
/// window `room` tall: it is `height` tall with them drawn as `covers` says. A pixel to spare, for
/// the covers' rounding.
fn fit_shrink(room: f32, height: f32, covers: Covers) -> f32 {
    if covers.designed <= 0.0 {
        return 0.0;
    }
    let scale = (room - 1.0 - (height - covers.drawn)) / covers.designed;
    if scale >= 1.0 || scale < FIT_LEAST { 0.0 } else { 1.0 - scale }
}

/// A chip (`.chip`): 26 (or 24) tall, round, raised grey; on: the accent at 16 % with a 45 % ring. True
/// when clicked.
fn chip_at(ui: &Ui, p: &super::theme::Palette, rect: Rect, id: Id, g: std::sync::Arc<egui::Galley>, on: bool) -> bool {
    let resp = ui.interact(rect, id, Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let r = (rect.height() / 2.0) as u8;
    let fill = if on { fin::acc(p, 0.16) } else if resp.hovered() { p.widget_hover_bg } else { p.widget_bg };
    ui.painter().rect_filled(rect, CornerRadius::same(r), fill);
    if on {
        ui.painter().rect_stroke(rect, CornerRadius::same(r), Stroke::new(1.0, fin::acc(p, 0.45)), StrokeKind::Inside);
    }
    let gh = g.size().y;
    let gw = g.size().x;
    ui.painter().galley_with_override_text_color(pos2(rect.center().x - gw / 2.0, rect.center().y - gh / 2.0), g, if on { p.text_strong } else { p.text });
    resp.clicked()
}

/// The hour of the day on this PC (for the greeting).
fn chrono_hour() -> u32 {
    local_now().0
}

/// "Thursday 22 October".
fn date_text() -> String {
    let (_, day, month, weekday) = local_now();
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    format!("{} {} {}", DAYS[weekday as usize % 7], day, MONTHS[(month as usize).saturating_sub(1) % 12])
}

/// (hour, day of month, month 1-12, weekday 0 = Sunday) in local time.
fn local_now() -> (u32, u32, u32, u32) {
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct SystemTime {
            year: u16,
            month: u16,
            weekday: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            ms: u16,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetLocalTime(t: *mut SystemTime);
        }
        let mut t = SystemTime::default();
        // SAFETY: GetLocalTime fills the struct it is given.
        unsafe { GetLocalTime(&mut t) };
        (u32::from(t.hour), u32::from(t.day), u32::from(t.month), u32::from(t.weekday))
    }
    #[cfg(not(windows))]
    {
        (12, 1, 1, 0)
    }
}

/// For you's and New releases' smallest cover in a column `cw` wide on a page whose content is
/// `page_w` wide: 136 (104 / 100 under 740 / 560), growing to the mockups' 190 on wide pages.
fn card_min(cw: f32, page_w: f32) -> f32 {
    if cw >= 740.0 {
        wide::grow(page_w, 136.0, 190.0)
    } else if cw >= 560.0 {
        104.0
    } else {
        100.0
    }
}

/// The quick tiles' columns `w` wide: 2 under 720, 3 under 1400, 4 under 2200, else 6.
fn tile_cols(w: f32) -> usize {
    if w < 720.0 {
        2
    } else if w < 1400.0 {
        3
    } else if w < 2200.0 {
        4
    } else {
        6
    }
}

/// Recently played's middle cover in a box `w` wide: 150 up to 1100, then growing to 210 at 1800,
/// 230 at 2700 and 280 at 4700, no bigger.
fn flow_middle(w: f32) -> f32 {
    const AT: [(f32, f32); 4] = [(1100.0, 150.0), (1800.0, 210.0), (2700.0, 230.0), (4700.0, 280.0)];
    let k = AT.iter().position(|&(x, _)| w < x).unwrap_or(AT.len());
    match k {
        0 => AT[0].1,
        k if k == AT.len() => AT[k - 1].1,
        k => {
            let ((x0, y0), (x1, y1)) = (AT[k - 1], AT[k]);
            y0 + (y1 - y0) * (w - x0) / (x1 - x0)
        }
    }
}

/// Recently played's covers left to right in a box `w` wide holding `items`: their sizes and
/// opacities. Up to 1100 wide as always: 7 (150 in the middle, 50 at the ends), 5 under 740, 3 under
/// 560. Wider, more and bigger ones: 9 from 1800 (210 in the middle), then one more each side at
/// 2250, 2700 (13, 230), 3400 and 4100, falling away smoothly to the ends. Fewer items keep the
/// middle ones. Never wider than the box: fewer covers where they don't fit, the last three smaller.
fn flow_sizes(w: f32, items: usize) -> (Vec<f32>, Vec<f32>) {
    // From the middle out, as parts of the middle cover, and their opacities.
    const NEAR: [(f32, f32); 4] = [(1.0, 1.0), (112.0 / 150.0, 0.86), (80.0 / 150.0, 0.62), (50.0 / 150.0, 0.38)];
    // Past 7: the end covers' part of the middle one, by how many there are each side (4 to 8).
    const ENDS: [f32; 5] = [0.305, 0.25, 0.191, 0.17, 0.15];
    let full = if w < 560.0 {
        1
    } else if w < 740.0 {
        2
    } else {
        3 + [1800.0, 2250.0, 2700.0, 3400.0, 4100.0].iter().filter(|&&at| w >= at).count()
    };
    // An odd number, so one is in the middle.
    let most = (if items % 2 == 1 { items } else { items.saturating_sub(1) }).max(1);
    let middle = flow_middle(w);
    let out: Vec<(f32, f32)> = (0..=full.min((most - 1) / 2))
        .map(|k| {
            if full <= 3 {
                let (part, alpha) = NEAR[k];
                ((middle * part).round(), alpha)
            } else {
                let f = (k as f32 / full as f32).powf(1.27);
                ((middle * ENDS[full - 4].powf(f)).round(), 1.0 - 0.62 * f)
            }
        })
        .collect();
    let row_w = |half: usize| out[..=half].iter().map(|o| o.0).sum::<f32>() * 2.0 - out[0].0 + FLOW_GAP * (half * 2) as f32;
    let mut half = out.len() - 1;
    while half > 1 && row_w(half) > w {
        half -= 1;
    }
    let row: Vec<(f32, f32)> = out[..=half].iter().rev().chain(out[1..=half].iter()).copied().collect();
    let fit = ((w - FLOW_GAP * (half * 2) as f32) / (row_w(half) - FLOW_GAP * (half * 2) as f32)).min(1.0);
    row.into_iter().map(|(size, alpha)| (if fit < 1.0 { (size * fit).floor().max(1.0) } else { size }, alpha)).unzip()
}

/// A cover-flow cover at place `t` between the slots (0: the leftmost): its left edge, size and
/// opacity, eased between the two slots around it, and shrinking and fading out past the ends.
fn flow_slot(t: f32, sizes: &[f32], alphas: &[f32], slot_x: &[f32]) -> (f32, f32, f32) {
    let n = sizes.len();
    let last = (n - 1) as f32;
    if t <= 0.0 {
        let sz = sizes[0] * (1.0 + t * 0.4);
        (slot_x[0] + t * (sizes[0] + FLOW_GAP), sz, alphas[0] * (1.0 + t))
    } else if t >= last {
        let e = t - last;
        let sz = sizes[n - 1] * (1.0 - e * 0.4);
        (slot_x[n - 1] + e * (sizes[n - 1] + FLOW_GAP), sz, alphas[n - 1] * (1.0 - e))
    } else {
        let k = (t.floor() as usize).min(n - 2);
        let f = t - k as f32;
        let lerp = |a: f32, b: f32| a + (b - a) * f;
        (lerp(slot_x[k], slot_x[k + 1]), lerp(sizes[k], sizes[k + 1]), lerp(alphas[k], alphas[k + 1]))
    }
}

#[cfg(test)]
mod flow_tests {
    use super::{Covers, FLOW_GAP, card_min, fit_shrink, fitted_row, flow_sizes, flow_slot, tile_cols};
    use crate::ui::wide;

    /// How wide a row of covers is, with the gaps.
    fn row_w(sizes: &[f32]) -> f32 {
        sizes.iter().sum::<f32>() + FLOW_GAP * (sizes.len() - 1) as f32
    }

    #[test]
    fn covers_sit_on_their_slots_and_ease_between() {
        let sizes = [80.0, 150.0, 80.0];
        let alphas = [0.5, 1.0, 0.5];
        let xs = [0.0, 94.0, 258.0];
        assert_eq!(flow_slot(1.0, &sizes, &alphas, &xs), (94.0, 150.0, 1.0));
        let (x, s, a) = flow_slot(0.5, &sizes, &alphas, &xs);
        assert_eq!((x, s, a), (47.0, 115.0, 0.75));
        // Past the ends: smaller and fading out.
        let (_, s, a) = flow_slot(-0.5, &sizes, &alphas, &xs);
        assert!(s < 80.0 && a < 0.5);
        assert_eq!(flow_slot(3.0, &sizes, &alphas, &xs).2, 0.0);
    }

    #[test]
    fn for_you_and_new_releases_grow_like_the_mockups() {
        // As before on normal pages and in narrow columns.
        for (cw, min) in [(0.0, 100.0), (300.0, 100.0), (559.0, 100.0), (560.0, 104.0), (739.0, 104.0), (740.0, 136.0), (1452.0, 136.0), (1500.0, 136.0)] {
            assert_eq!(card_min(cw, cw), min, "{cw}");
        }
        // (column, page, New releases' covers, For you's cards): Centred's 1800 has 8 covers of
        // about 209 (mockup A: 8 of 204), For you 7 as its cards carry the record too; Split's right
        // column on a 34" window as big (mockup C: 7 of 210); Fill there and on a 5120 window.
        for (cw, page, releases, mixes) in [(1800.0, 1800.0, 8, 7), (1644.0, 2752.0, 7, 6), (2752.0, 2752.0, 13, 11), (4432.0, 4432.0, 21, 17)] {
            let min = card_min(cw, page);
            assert_eq!(min, 190.0, "{cw} in {page}");
            let (n, size) = wide::row_fit(cw, min, 18.0);
            assert!(n == releases && size >= 190.0, "{n} of {size} in {cw}");
            let (n, card_w) = wide::row_fit(cw, (min * 1.24).round(), 14.0);
            assert!(n == mixes && (card_w / 1.24).round() >= 190.0, "{n} of {card_w} in {cw}");
        }
        assert_eq!(wide::row_fit(1800.0, card_min(1800.0, 1800.0), 18.0).1, 209.25);
        // Between 1500 and 1800 it grows a little at a time.
        assert_eq!(card_min(1650.0, 1650.0), 163.0);
        assert!(card_min(1501.0, 1501.0) > 136.0 && card_min(1799.0, 1799.0) < 190.0);
    }

    #[test]
    fn tiles_get_more_columns_as_the_page_widens() {
        for (w, cols) in [(368.0, 2), (719.0, 2), (720.0, 3), (1399.0, 3), (1400.0, 4), (1800.0, 4), (2199.0, 4), (2200.0, 6), (2752.0, 6), (4432.0, 6)] {
            assert_eq!(tile_cols(w), cols, "{w}");
        }
        // Up to a 34" window's page no tile is wider than about 550 (it was 912 there).
        let mut w = 368.0;
        while w <= 2800.0 {
            let cols = tile_cols(w) as f32;
            let tile = (w - 8.0 * (cols - 1.0)) / cols;
            assert!(tile <= 550.0, "{tile} wide at {w}");
            w += 1.0;
        }
    }

    #[test]
    fn recently_played_is_as_before_up_to_1100() {
        let seven = (vec![50.0, 80.0, 112.0, 150.0, 112.0, 80.0, 50.0], vec![0.38, 0.62, 0.86, 1.0, 0.86, 0.62, 0.38]);
        for w in [740.0, 1059.0, 1060.0, 1100.0] {
            assert_eq!(flow_sizes(w, 20), seven, "{w}");
        }
        // Just past it, no jump.
        assert_eq!(flow_sizes(1101.0, 20).0, seven.0);
        assert_eq!(flow_sizes(739.0, 20).0, [80.0, 112.0, 150.0, 112.0, 80.0]);
        assert_eq!(flow_sizes(600.0, 20).0, [80.0, 112.0, 150.0, 112.0, 80.0]);
        assert_eq!(flow_sizes(559.0, 20).0, [112.0, 150.0, 112.0]);
        // Five don't fit in 560 to 589 (they were cut off): three, as big as ever.
        assert_eq!(flow_sizes(560.0, 20).0, [112.0, 150.0, 112.0]);
        assert_eq!(flow_sizes(590.0, 20).0.len(), 5);
    }

    #[test]
    fn recently_played_grows_with_the_box() {
        // (box, covers, middle): Split's left column, Centred's 1800, a 34" window's page, 5120 wide.
        for (w, n, middle) in [(1060.0, 7, 150.0), (1200.0, 7, 159.0), (1799.0, 7, 210.0), (1800.0, 9, 210.0), (2700.0, 13, 230.0), (2752.0, 13, 231.0), (4432.0, 17, 273.0), (9000.0, 17, 280.0)] {
            let (sizes, _) = flow_sizes(w, 40);
            assert_eq!((sizes.len(), sizes[sizes.len() / 2]), (n, middle), "{w}");
        }
        // As in the mockups: 64, 96, 130, 170, 210 at 1800; 44, 64, 88, 116, 150, 190, 230 at 2700.
        let near = |got: &[f32], want: &[f32]| got.iter().zip(want).all(|(g, w)| (g - w).abs() <= 6.0);
        let a = flow_sizes(1800.0, 40).0;
        assert!(near(&a, &[64.0, 96.0, 130.0, 170.0, 210.0, 170.0, 130.0, 96.0, 64.0]), "{a:?}");
        let b = flow_sizes(2700.0, 40).0;
        assert!(near(&b[..7], &[44.0, 64.0, 88.0, 116.0, 150.0, 190.0, 230.0]), "{b:?}");
        // Few lists: the middle ones of the same row.
        assert_eq!(flow_sizes(2700.0, 3).0, b[5..8]);
        assert_eq!(flow_sizes(2700.0, 4).0, b[5..8]);
        assert_eq!(flow_sizes(2700.0, 1).0, [230.0]);
        assert_eq!(flow_sizes(2700.0, 0).0, [230.0]);
    }

    #[test]
    fn recently_played_always_fits_and_falls_away_to_the_ends() {
        let mut was = (0, 0.0);
        let mut w = 300.0;
        while w <= 5200.0 {
            for items in [0, 1, 2, 3, 4, 7, 20] {
                let (sizes, alphas) = flow_sizes(w, items);
                let n = sizes.len();
                assert!(n % 2 == 1 && n <= items.max(1), "{n} covers for {items} at {w}");
                assert!(row_w(&sizes) <= w, "{} wide in {w}", row_w(&sizes));
                for k in 0..n / 2 {
                    // The same both sides, smaller and dimmer further out.
                    assert_eq!((sizes[k], alphas[k]), (sizes[n - 1 - k], alphas[n - 1 - k]), "{w}");
                    assert!(sizes[k] < sizes[k + 1] && alphas[k] < alphas[k + 1], "{sizes:?} {alphas:?} at {w}");
                }
            }
            // With plenty to show, never fewer or smaller covers as the box widens.
            let (sizes, _) = flow_sizes(w, 40);
            let now = (sizes.len(), sizes[sizes.len() / 2]);
            assert!(now.0 >= was.0 && now.1 >= was.1, "{now:?} after {was:?} at {w}");
            was = now;
            w += 7.0;
        }
        // Squeezed (a very narrow window): three, smaller, still inside.
        let (sizes, _) = flow_sizes(320.0, 20);
        assert!(sizes.len() == 3 && sizes[1] < 150.0 && row_w(&sizes) <= 320.0, "{sizes:?}");
    }

    #[test]
    fn a_page_a_little_too_tall_fits_the_window_with_smaller_covers() {
        let covers = Covers { scale: 1.0, designed: 450.0, drawn: 450.0 };
        // 40 too tall (and the pixel to spare): the covers give it up between them.
        let s = fit_shrink(1000.0, 1040.0, covers);
        assert!((s - 41.0 / 450.0).abs() < 1e-4, "{s}");
        // Drawn that much smaller, the same: what isn't covers stays as it is.
        let smaller = Covers { scale: 1.0 - s, designed: 450.0, drawn: 450.0 * (1.0 - s) };
        assert!((fit_shrink(1000.0, 1040.0 - 450.0 * s, smaller) - s).abs() < 1e-4);
        // It fits: as designed. Far too tall (there's more to see): as designed, and it scrolls.
        assert_eq!(fit_shrink(1000.0, 990.0, covers), 0.0);
        assert_eq!(fit_shrink(1000.0, 1300.0, covers), 0.0);
        // Nothing to shrink.
        assert_eq!(fit_shrink(1000.0, 1040.0, Covers::default()), 0.0);
    }

    #[test]
    fn a_fitted_row_has_more_smaller_cards() {
        let (n, w) = wide::row_fit(1232.0, 136.0, 18.0);
        assert_eq!(fitted_row(1232.0, 136.0, 18.0, 1.0), (n, w, w));
        let (m, v, designed) = fitted_row(1232.0, 136.0, 18.0, 0.85);
        assert!(m > n && v <= w * 0.85 + 1e-3 && designed == w, "{m} {v} {n} {w}");
        // Still from edge to edge.
        assert!((m as f32 * v + (m - 1) as f32 * 18.0 - 1232.0).abs() < 1e-3);
    }
}
