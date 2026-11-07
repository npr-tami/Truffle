//! A song's right-click menu in the Final design (`.menu.ctx`): the song on top, then Add to playlist
//! (a submenu with a search box, New playlist and every playlist, "Added" on the ones that have it),
//! Add to queue, Play next, Go to album, Go to artist, Show lyrics, Like, Block and Copy link, the
//! ones with a shortcut showing it. The same keys work on the selected song (or the playing one)
//! without the menu.

use std::collections::HashSet;
use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Key, Modifiers, Order, Pos2, Rect, Sense, Shadow, Stroke, StrokeKind, Ui, pos2, vec2};

use super::fin;
use super::svgicon;
use super::theme::{bold, regular};
use super::tracks::{liked_color, one_line};
use super::util::spotify_web_link;
use super::TruffleApp;
use crate::model::{Command, Source, TrackInfo};
use crate::text::{contains_folded, fold};

/// `.menu`: 240 wide (the submenu 214), 4 inside, rows of 28, lines with 4 above and below.
const MENU_W: f32 = 240.0;
const SUB_W: f32 = 214.0;
const PAD: f32 = 4.0;
const ITEM_H: f32 = 28.0;
const SEP_H: f32 = 9.0;
const HEAD_H: f32 = 47.0;
/// The submenu's list shows this many playlists before it scrolls.
const SUB_ROWS: usize = 7;

/// An open song menu.
pub(super) struct SongMenu {
    track: TrackInfo,
    /// Every song it acts on (more than one: the picked songs of a list).
    tracks: Vec<TrackInfo>,
    at: Pos2,
    /// The list it was opened in (for "Remove from this playlist").
    from: Option<Source>,
    /// The Add to playlist submenu is open, and what is typed in its search box.
    sub: bool,
    find: String,
    /// The playlists (uris) known to hold every song it acts on, once the controller has answered
    /// ([`crate::model::Command::HeldBy`]): marked Added in the submenu.
    held: Option<HashSet<String>>,
    /// Passes drawn: the click that opened it must not close it.
    passes: u32,
}

/// What a menu row does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Item {
    AddTo,
    Queue,
    PlayNext,
    Album,
    Artist,
    Lyrics,
    Like,
    Block,
    Link,
    RemoveHere,
    /// YouTube sound: pick another video for the song.
    WrongSong,
    Sep,
}

impl TruffleApp {
    /// Opens the menu for `track` at `at` (the pointer, usually).
    pub(super) fn open_song_menu(&mut self, track: &TrackInfo, at: Pos2, from: Option<Source>) {
        self.song_menu = Some(SongMenu { track: track.clone(), tracks: vec![track.clone()], at, from, sub: false, find: String::new(), held: None, passes: 0 });
        self.ask_held();
    }

    /// Asks which playlists hold the open menu's songs (the answer comes as an event).
    fn ask_held(&mut self) {
        let Some(m) = &self.song_menu else { return };
        let uris: Vec<String> = m.tracks.iter().filter(|t| !t.is_local).map(|t| t.uri.clone()).collect();
        if !uris.is_empty() {
            self.send(Command::HeldBy(uris));
        }
    }

    /// The controller's answer: which playlists hold `uris`. Kept if the menu still acts on those songs.
    pub(super) fn held_came(&mut self, uris: Vec<String>, playlists: Vec<String>) {
        if let Some(m) = self.song_menu.as_mut()
            && m.tracks.iter().filter(|t| !t.is_local).map(|t| t.uri.as_str()).eq(uris.iter().map(String::as_str))
        {
            m.held = Some(playlists.into_iter().collect());
        }
    }

    /// The menu for `track` with its playlists (Add to playlist) already open: the Now playing card's +.
    pub(super) fn open_add_to(&mut self, track: &TrackInfo, at: Pos2, from: Option<Source>) {
        self.open_song_menu(track, at, from);
        if let Some(m) = self.song_menu.as_mut() {
            m.sub = true;
        }
    }

    /// Opens the menu for several songs (the picked ones).
    pub(super) fn open_songs_menu(&mut self, tracks: Vec<TrackInfo>, at: Pos2, from: Option<Source>) {
        let Some(first) = tracks.first().cloned() else { return };
        self.song_menu = Some(SongMenu { track: first, tracks, at, from, sub: false, find: String::new(), held: None, passes: 0 });
        self.ask_held();
    }

    /// A picture of the menu (a self-shot): the playing song's, with Add to playlist open.
    pub(super) fn shot_song_menu(&mut self, at: Pos2) {
        if self.song_menu.is_none()
            && let Some(t) = self.playback.track.clone()
        {
            self.song_menu = Some(SongMenu { track: t.clone(), tracks: vec![t], at, from: None, sub: true, find: String::new(), held: None, passes: 0 });
        }
    }

    /// The menu's rows for this song.
    fn menu_items(&self, m: &SongMenu) -> Vec<Item> {
        let current = self.playback.track.as_ref().is_some_and(|c| c.uri == m.track.uri);
        let removable = match &m.from {
            Some(Source::Playlist(uri)) => self.playlist(uri).is_some_and(|pl| pl.editable),
            _ => false,
        };
        if m.tracks.len() > 1 {
            // Several songs: what can be done to all of them.
            let mut items = vec![Item::AddTo, Item::Queue, Item::PlayNext, Item::Sep, Item::Like, Item::Block, Item::Link];
            if removable {
                items.extend([Item::Sep, Item::RemoveHere]);
            }
            return items;
        }
        let mut items = vec![Item::AddTo, Item::Queue, Item::PlayNext, Item::Sep, Item::Album, Item::Artist];
        if current {
            items.push(Item::Lyrics);
        }
        items.extend([Item::Sep, Item::Like, Item::Block, Item::Link]);
        if self.sound_source() == crate::config::SoundSource::YouTube && likeable(&m.track) {
            items.push(Item::WrongSong);
        }
        if removable {
            items.extend([Item::Sep, Item::RemoveHere]);
        }
        items
    }

    /// Draws the open menu (once a pass, over everything) and does what was picked.
    pub(super) fn song_menu_ui(&mut self, ctx: &egui::Context) {
        let Some(mut m) = self.song_menu.take() else { return };
        let p = self.palette;
        m.passes += 1;
        let items = self.menu_items(&m);
        let h = PAD * 2.0 + HEAD_H + 4.0 + items.iter().map(|i| if *i == Item::Sep { SEP_H } else { ITEM_H }).sum::<f32>();
        let screen = ctx.content_rect();
        let x = if m.at.x + MENU_W > screen.right() - 6.0 { (m.at.x - MENU_W).max(screen.left() + 6.0) } else { m.at.x };
        let y = if m.at.y + h > screen.bottom() - 6.0 { (m.at.y - h).max(screen.top() + 6.0) } else { m.at.y };
        let menu = Rect::from_min_size(pos2(x, y), vec2(MENU_W, h));
        // Several songs: liked (blocked) when all of them are.
        let liked = m.tracks.iter().filter(|t| likeable(t)).all(|t| self.liked.contains(&t.uri));
        let blocked = m.tracks.iter().all(|t| self.blocked.contains(&t.uri));
        let many = m.tracks.len();
        let mut picked: Option<Item> = None;
        let mut add_to: Option<Source> = None;
        let mut new_playlist = false;
        let mut sub_rect = None;
        let mut sub_hovered = false;

        egui::Area::new(Id::new("song-menu")).order(Order::Foreground).fixed_pos(menu.min).fade_in(false).interactable(true).show(ctx, |ui| {
            let (_, bg) = ui.allocate_exact_size(menu.size(), Sense::click());
            let _ = bg;
            paint_box(ui, menu);
            // The song: cover, name, artists and album.
            let head = Rect::from_min_size(menu.min + vec2(PAD, PAD), vec2(MENU_W - PAD * 2.0, HEAD_H));
            let cover = Rect::from_min_size(pos2(head.left() + 8.0, head.top() + 6.0), vec2(32.0, 32.0));
            if !self.images.paint(ui, m.track.cover_small.as_deref(), cover, 3, p.widget_bg) {
                svgicon::paint(ui.painter(), cover.shrink(9.0), "note", p.text_weak, svgicon::STROKE);
            }
            let tx = cover.right() + 10.0;
            let tw = head.right() - 8.0 - tx;
            let (name, sub) = if many > 1 {
                (format!("{many} songs"), "Picked in this list".to_owned())
            } else {
                (m.track.name.clone(), format!("{} · {}", m.track.artist_line(), m.track.album))
            };
            let t = one_line(ui, &name, bold(12.0), p.text_strong, tw, false);
            let s = one_line(ui, &sub, regular(11.0), p.text_weak, tw, false);
            let th = t.size().y;
            let block_h = th + 1.0 + s.size().y;
            let ty = cover.center().y - block_h / 2.0;
            ui.painter().galley(pos2(tx, ty), t, p.text_strong);
            ui.painter().galley(pos2(tx, ty + th + 1.0), s, p.text_weak);
            ui.painter().hline(head.x_range(), head.bottom() - 0.5, Stroke::new(1.0, p.border));

            let mut iy = head.bottom() + 4.0;
            for item in &items {
                if *item == Item::Sep {
                    ui.painter().hline((menu.left() + PAD + 6.0)..=(menu.right() - PAD - 6.0), iy + 4.0, Stroke::new(1.0, p.border));
                    iy += SEP_H;
                    continue;
                }
                let row = Rect::from_min_size(pos2(menu.left() + PAD, iy), vec2(MENU_W - PAD * 2.0, ITEM_H));
                let resp = ui.interact(row, Id::new(("song-menu-item", *item as u8)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                let open_sub = *item == Item::AddTo && m.sub;
                let hot = resp.hovered() || open_sub;
                if hot {
                    ui.painter().rect_filled(row, CornerRadius::same(5), p.widget_hover_bg);
                }
                let (icon, label, key) = match item {
                    Item::AddTo => ("plus", "Add to playlist", ""),
                    Item::Queue => ("queue", "Add to queue", "Ctrl+Q"),
                    Item::PlayNext => ("playnext", "Play next", "Ctrl+N"),
                    Item::Album => ("album", "Go to album", ""),
                    Item::Artist => ("artist", "Go to artist", ""),
                    Item::Lyrics => ("lyrics", "Show lyrics", ""),
                    Item::Like => (if liked { "heartf" } else { "heart" }, if liked { "Unlike" } else { "Like" }, "Ctrl+S"),
                    Item::Block => ("block", if blocked { "Unblock song" } else { "Block song" }, "Ctrl+Del"),
                    Item::Link => ("link", if many > 1 { "Copy links" } else { "Copy link" }, "Ctrl+C"),
                    Item::RemoveHere => ("x", "Remove from this playlist", ""),
                    Item::WrongSong => ("refresh", "Wrong song? Pick another", ""),
                    Item::Sep => unreachable!(),
                };
                let icon_c = if hot { p.text } else { p.text_weak };
                svgicon::paint(ui.painter(), Rect::from_min_size(pos2(row.left() + 10.0, row.center().y - 6.5), vec2(13.0, 13.0)), icon, icon_c, svgicon::STROKE);
                ui.painter().text(pos2(row.left() + 33.0, row.center().y), Align2::LEFT_CENTER, label, regular(12.0), if hot { p.text_strong } else { p.text });
                if *item == Item::AddTo {
                    svgicon::paint(ui.painter(), Rect::from_min_size(pos2(row.right() - 10.0 - 13.0, row.center().y - 6.5), vec2(13.0, 13.0)), "chevr", p.text_weak, svgicon::STROKE);
                    if resp.hovered() || resp.clicked() {
                        m.sub = true;
                    }
                } else {
                    if !key.is_empty() {
                        ui.painter().text(pos2(row.right() - 10.0, row.center().y), Align2::RIGHT_CENTER, key, regular(10.5), p.text_weak);
                    }
                    if resp.hovered() {
                        m.sub = false;
                    }
                    if resp.clicked() {
                        picked = Some(*item);
                    }
                }
                if *item == Item::AddTo {
                    sub_rect = Some(row);
                }
                iy += ITEM_H;
            }
        });

        // The Add to playlist submenu, beside its row (on the left when there is no room).
        if m.sub
            && let Some(row) = sub_rect
        {
            let playlists = self.sub_playlists(&m);
            let rows = playlists.len().min(SUB_ROWS);
            let sh = PAD * 2.0 + 34.0 + ITEM_H + SEP_H + rows.max(1) as f32 * ITEM_H;
            let right_side = menu.right() + 5.0 + SUB_W <= screen.right() - 6.0;
            let sx = if right_side { menu.right() + 5.0 } else { menu.left() - 5.0 - SUB_W };
            let sy = (row.top() - PAD - 3.0).min(screen.bottom() - 6.0 - sh).max(screen.top() + 6.0);
            let sub = Rect::from_min_size(pos2(sx, sy), vec2(SUB_W, sh));
            egui::Area::new(Id::new("song-menu-sub")).order(Order::Foreground).fixed_pos(sub.min).fade_in(false).interactable(true).show(ctx, |ui| {
                let (_, bg) = ui.allocate_exact_size(sub.size(), Sense::click());
                sub_hovered = bg.hovered() || ui.rect_contains_pointer(sub);
                paint_box(ui, sub);
                let find = Rect::from_min_size(sub.min + vec2(PAD + 2.0, PAD + 2.0), vec2(SUB_W - PAD * 2.0 - 4.0, 28.0));
                fin::search_box(ui, &p, find, Id::new("song-menu-find"), &mut m.find, "Find playlist", "");
                let mut iy = find.bottom() + 4.0;
                let row = Rect::from_min_size(pos2(sub.left() + PAD, iy), vec2(SUB_W - PAD * 2.0, ITEM_H));
                let resp = ui.interact(row, Id::new("song-menu-new"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                if resp.hovered() {
                    ui.painter().rect_filled(row, CornerRadius::same(5), p.widget_hover_bg);
                }
                svgicon::paint(ui.painter(), Rect::from_min_size(pos2(row.left() + 10.0, row.center().y - 6.5), vec2(13.0, 13.0)), "plus", p.text_weak, svgicon::STROKE);
                ui.painter().text(pos2(row.left() + 33.0, row.center().y), Align2::LEFT_CENTER, "New playlist…", regular(12.0), p.text);
                if resp.clicked() {
                    new_playlist = true;
                }
                iy += ITEM_H;
                ui.painter().hline((sub.left() + PAD + 6.0)..=(sub.right() - PAD - 6.0), iy + 4.0, Stroke::new(1.0, p.border));
                iy += SEP_H;
                let list = Rect::from_min_max(pos2(sub.left() + PAD, iy), pos2(sub.right() - PAD, sub.bottom() - PAD));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
                child.set_clip_rect(list);
                egui::ScrollArea::vertical().id_salt("song-menu-list").max_height(list.height()).auto_shrink(false).show(&mut child, |ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                    if playlists.is_empty() {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new("No playlists match").font(regular(11.5)).color(p.text_weak));
                    }
                    for (source, name, image, has, can) in &playlists {
                        let (r, resp) = ui.allocate_exact_size(vec2(list.width(), ITEM_H), if *can { Sense::click() } else { Sense::hover() });
                        if resp.hovered() && *can {
                            ui.painter().rect_filled(r, CornerRadius::same(5), p.widget_hover_bg);
                        }
                        let pic = Rect::from_min_size(pos2(r.left() + 8.0, r.center().y - 9.0), vec2(18.0, 18.0));
                        if *source == Source::LikedSongs {
                            ui.painter().rect_filled(pic, CornerRadius::same(2), liked_color(&p));
                            svgicon::paint(ui.painter(), pic.shrink(4.0), "heartf", Color32::WHITE, svgicon::STROKE);
                        } else if !self.images.paint(ui, image.as_deref(), pic, 2, p.widget_bg) {
                            svgicon::paint(ui.painter(), pic.shrink(3.0), "note", p.text_weak, svgicon::STROKE);
                        }
                        let right = if *has { r.right() - 64.0 } else { r.right() - 8.0 };
                        let g = one_line(ui, name, regular(12.0), if *can { p.text } else { p.text_faint }, right - (pic.right() + 10.0), false);
                        ui.painter().galley(pos2(pic.right() + 10.0, r.center().y - g.size().y / 2.0), g, p.text);
                        if *has {
                            svgicon::paint(ui.painter(), Rect::from_min_size(pos2(r.right() - 58.0, r.center().y - 6.0), vec2(12.0, 12.0)), "check", p.accent_ink, svgicon::STROKE);
                            ui.painter().text(pos2(r.right() - 8.0, r.center().y), Align2::RIGHT_CENTER, "Added", regular(11.0), p.accent_ink);
                        }
                        let resp = if *can { resp.on_hover_cursor(CursorIcon::PointingHand) } else { resp.on_hover_text("Local files can't be added to Spotify playlists") };
                        // A playlist marked Added still takes the click: the list on screen may be
                        // behind, and "Already in" asks from the playlist as Spotify has it.
                        if resp.clicked() && *can && !(*has && *source == Source::LikedSongs) {
                            add_to = Some(source.clone());
                        }
                    }
                });
            });
        }

        // Esc, or a click anywhere else, closes it.
        let (esc, pressed, pointer) = ctx.input(|i| (i.key_pressed(Key::Escape), i.pointer.any_pressed(), i.pointer.interact_pos()));
        let inside = pointer.is_some_and(|pt| menu.contains(pt)) || sub_hovered;
        let close = esc || (pressed && !inside && m.passes > 1);
        let track = m.track.clone();
        let tracks = m.tracks.clone();
        let from = m.from.clone();
        if picked.is_none() && add_to.is_none() && !new_playlist && !close {
            self.song_menu = Some(m);
        }
        if let Some(item) = picked {
            self.songs_action(item, &tracks, from.as_ref(), ctx);
        }
        let _ = track;
        if let Some(target) = add_to {
            let uris: Vec<String> = tracks.iter().filter(|t| !t.is_local).map(|t| t.uri.clone()).collect();
            match target {
                Source::LikedSongs => {
                    let new: Vec<String> = uris.into_iter().filter(|u| !self.liked.contains(u)).collect();
                    self.like_songs(new, true);
                }
                Source::Playlist(uri) if !uris.is_empty() => self.add_songs_to(uri, uris),
                _ => {}
            }
        }
        if new_playlist {
            self.new_playlist = Some(String::new());
        }
    }

    /// Liked Songs and the user's playlists for the submenu, matching its search: (source, name,
    /// picture, has the song, can take it).
    fn sub_playlists(&self, m: &SongMenu) -> Vec<(Source, String, Option<String>, bool, bool)> {
        let find = fold(m.find.trim());
        let matches = |name: &str| contains_folded(name, &find);
        // "Added" on the lists known to hold them all: the open one, those the controller found in the
        // lists it has, and Liked Songs when all are liked.
        let open = self
            .view
            .as_ref()
            .filter(|v| v.tracks.as_ref().is_some_and(|t| m.tracks.iter().all(|s| t.iter().any(|t| t.uri == s.uri))))
            .map(|v| v.source.clone());
        let any_spotify = m.tracks.iter().any(|t| !t.is_local);
        let mut out = Vec::new();
        if matches("Liked Songs") && any_spotify {
            let all_liked = m.tracks.iter().filter(|t| likeable(t)).all(|t| self.liked.contains(&t.uri));
            out.push((Source::LikedSongs, "Liked Songs".to_owned(), None, all_liked, true));
        }
        for pl in self.playlists.iter().filter(|pl| pl.editable && matches(&pl.name)) {
            let source = Source::Playlist(pl.uri.clone());
            let has = open.as_ref() == Some(&source) || m.held.as_ref().is_some_and(|h| h.contains(&pl.uri));
            out.push((source, pl.name.clone(), pl.image_url.clone(), has, any_spotify));
        }
        out
    }

    /// Likes or unlikes several songs at once (sent to Spotify in a few batches). Anything that
    /// can't be liked (local files, episodes) is skipped.
    pub(super) fn like_songs(&mut self, uris: Vec<String>, like: bool) {
        let uris: Vec<String> = uris.into_iter().filter(|u| u.starts_with("spotify:track:") && self.liked.contains(u) != like).collect();
        match uris.len() {
            0 => {}
            1 => self.like_song(&uris[0], like),
            _ => {
                self.artist_likes.stale();
                let set = std::sync::Arc::make_mut(&mut self.liked);
                for uri in &uris {
                    if like {
                        set.insert(uri.clone());
                    } else {
                        set.remove(uri);
                    }
                }
                self.send(Command::SetLikedMany { uris, liked: like });
            }
        }
    }

    /// Likes or unlikes a song (the heart, the menu, Ctrl+S).
    pub(super) fn like_song(&mut self, uri: &str, like: bool) {
        self.artist_likes.stale();
        let set = std::sync::Arc::make_mut(&mut self.liked);
        if like {
            set.insert(uri.to_owned());
        } else {
            set.remove(uri);
        }
        self.send(Command::SetLiked { uri: uri.to_owned(), liked: like });
    }

    /// What a menu row (or its shortcut) does to several songs: each the same way (liked or blocked
    /// all together; queued in their order).
    fn songs_action(&mut self, item: Item, tracks: &[TrackInfo], from: Option<&Source>, ctx: &egui::Context) {
        let [one] = tracks else {
            match item {
                Item::Queue => {
                    for t in tracks {
                        self.send(Command::Queue { track: Box::new(t.clone()), next: false });
                    }
                }
                // Each one first in line, from the last: they play in their order.
                Item::PlayNext => {
                    for t in tracks.iter().rev() {
                        self.send(Command::Queue { track: Box::new(t.clone()), next: true });
                    }
                }
                Item::Like => {
                    let like = !tracks.iter().filter(|t| likeable(t)).all(|t| self.liked.contains(&t.uri));
                    let uris = tracks.iter().filter(|t| likeable(t)).map(|t| t.uri.clone()).collect();
                    self.like_songs(uris, like);
                }
                Item::Block => {
                    let block = !tracks.iter().all(|t| self.blocked.contains(&t.uri));
                    self.set_blocked_many(tracks.iter().map(|t| t.uri.clone()).collect(), block);
                }
                Item::Link => {
                    let links: Vec<String> = tracks.iter().filter_map(|t| spotify_web_link(&t.uri)).collect();
                    if !links.is_empty() {
                        ctx.copy_text(links.join("\n"));
                        self.note(format!("{} links copied", links.len()), None);
                    }
                }
                Item::RemoveHere => {
                    if let Some(Source::Playlist(uri)) = from {
                        let uris = tracks.iter().map(|t| t.uri.clone()).collect();
                        self.send(Command::RemoveFromPlaylist { playlist_uri: uri.clone(), uris });
                    }
                }
                _ => {}
            }
            return;
        };
        self.song_action(item, one, from, ctx);
    }

    /// What a menu row (or its shortcut) does to `track`.
    fn song_action(&mut self, item: Item, track: &TrackInfo, from: Option<&Source>, ctx: &egui::Context) {
        match item {
            Item::Queue => self.send(Command::Queue { track: Box::new(track.clone()), next: false }),
            Item::PlayNext => self.send(Command::Queue { track: Box::new(track.clone()), next: true }),
            // A page to go to: out of the Now playing view, which would hide it.
            Item::Album => {
                if track.album_uri.starts_with("spotify:album:") {
                    self.close_stage(ctx);
                    self.open_source(Source::Album(track.album_uri.clone()), false);
                }
            }
            Item::Artist => {
                if let Some(a) = track.artists.iter().find(|a| a.uri.starts_with("spotify:artist:")) {
                    self.close_stage(ctx);
                    self.open_source(Source::Artist(a.uri.clone()), false);
                }
            }
            // In the Now playing view, its own lyrics come back.
            Item::Lyrics if self.stage.open => self.stage.lyrics_off = false,
            Item::Lyrics => {
                self.frame.panel_tab = super::panel::Tab::Lyrics;
                let w = ctx.content_rect().width();
                if self.frame_layout(w).panel_w <= 0.0 {
                    self.toggle_side_panel(w);
                }
            }
            Item::Like => {
                let liked = self.liked.contains(&track.uri);
                self.like_song(&track.uri, !liked);
            }
            Item::Block => {
                let blocked = self.blocked.contains(&track.uri);
                self.set_blocked(track.uri.clone(), !blocked);
            }
            Item::Link => {
                if let Some(link) = spotify_web_link(&track.uri) {
                    ctx.copy_text(link);
                    self.note("Link copied", None);
                }
            }
            Item::RemoveHere => {
                if let Some(Source::Playlist(uri)) = from {
                    self.send(Command::RemoveFromPlaylist { playlist_uri: uri.clone(), uris: vec![track.uri.clone()] });
                }
            }
            Item::WrongSong => self.open_yt_pick(track),
            Item::AddTo | Item::Sep => {}
        }
    }

    /// The menu's shortcuts, on the selected song of the open list (else the playing song; always it
    /// in the Now playing view, which hides the list).
    pub(super) fn song_shortcuts(&mut self, ui: &Ui) {
        let Some(item) = song_key(ui) else { return };
        let on_list = !self.stage.open && matches!(self.middle_tab, super::MiddleTab::Songs | super::MiddleTab::Blocked);
        let songs = key_songs(self.view.as_ref().filter(|_| on_list), self.playback.track.as_ref());
        let from = self.view.as_ref().map(|v| v.source.clone());
        let ctx = ui.ctx().clone();
        if !songs.is_empty() {
            self.songs_action(item, &songs, from.as_ref(), &ctx);
        }
    }
}

/// The menu row whose key was pressed: Ctrl+S, Ctrl+Q, Ctrl+N or Ctrl+Del (taken, so nothing else
/// sees them), or Ctrl+C (egui's copy). None while a text box is typed in: the keys are its own.
fn song_key(ui: &Ui) -> Option<Item> {
    if ui.ctx().text_edit_focused() {
        return None;
    }
    ui.input_mut(|i| {
        if super::keys::take(i, Modifiers::CTRL, Key::S) {
            Some(Item::Like)
        } else if super::keys::take(i, Modifiers::CTRL, Key::Q) {
            Some(Item::Queue)
        } else if super::keys::take(i, Modifiers::CTRL, Key::N) {
            Some(Item::PlayNext)
        } else if super::keys::take(i, Modifiers::CTRL, Key::Delete) {
            Some(Item::Block)
        } else if i.events.iter().any(|e| matches!(e, egui::Event::Copy)) {
            Some(Item::Link)
        } else {
            None
        }
    })
}

/// The songs a key acts on: the songs picked in the list shown (`view`, when it is on screen) if
/// it shows more than one of them, else its selected song if it shows it (not filtered out), else
/// the playing one.
fn key_songs(view: Option<&super::tracks::TrackView>, playing: Option<&TrackInfo>) -> Vec<TrackInfo> {
    if let Some(v) = view {
        let tracks = v.tracks.as_deref().map(Vec::as_slice).unwrap_or_default();
        let shown = |i: usize| v.filtered.contains(&(i as u32));
        let many: Vec<TrackInfo> = v.picked.iter().filter(|&&i| shown(i)).filter_map(|&i| tracks.get(i).cloned()).collect();
        if many.len() > 1 {
            return many;
        }
        if let Some(t) = v.selected.filter(|&i| shown(i)).and_then(|i| tracks.get(i)) {
            return vec![t.clone()];
        }
    }
    playing.cloned().into_iter().collect()
}

/// The menu's box: `--panel` with a 1 px line, round 8, and its deep shadow.
fn paint_box(ui: &Ui, rect: Rect) {
    let p = crate::ui::theme::palette(ui.ctx());
    let shadow = Shadow { offset: [0, 16], blur: 44, spread: 0, color: Color32::from_black_alpha(166) };
    ui.painter().add(shadow.as_shape(rect, CornerRadius::same(8)));
    ui.painter().rect_filled(rect, CornerRadius::same(8), p.raised_bg);
    ui.painter().rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
}

/// A song that can go in Liked Songs: a Spotify song, not a local file or an episode.
pub(super) fn likeable(t: &TrackInfo) -> bool {
    !t.is_local && t.uri.starts_with("spotify:track:")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ui::kit::headless::pass;
    use crate::ui::tracks::TrackView;

    fn song(id: &str) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{id}"),
            name: id.into(),
            artists: Vec::new(),
            album: String::new(),
            album_uri: String::new(),
            cover_small: None,
            cover_large: None,
            duration_ms: 1000,
            playable: true,
            is_local: false,
            added_at: 0,
        }
    }

    fn press(key: Key, modifiers: Modifiers) -> Vec<egui::Event> {
        vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]
    }

    /// One pass of the song keys before the page (with a text box when `typing`, which takes the
    /// focus): the row asked for, and whether the key was left for others.
    fn keyed(ctx: &egui::Context, events: Vec<egui::Event>, typing: bool) -> (Option<Item>, bool) {
        let mut out = (None, false);
        let mut text = String::new();
        pass(ctx, events, |ui| {
            out.0 = song_key(ui);
            out.1 = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { .. })));
            if typing {
                ui.add(egui::TextEdit::singleline(&mut text).id(Id::new("typing"))).request_focus();
            }
        });
        out
    }

    fn uris(songs: Vec<TrackInfo>) -> Vec<String> {
        songs.into_iter().map(|t| t.uri).collect()
    }

    #[test]
    fn ctrl_n_and_ctrl_q_queue_the_selected_song_and_nothing_quits() {
        let ctx = egui::Context::default();
        // Ctrl as Windows sends it (Ctrl and Command).
        let ctrl = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
        assert_eq!(keyed(&ctx, press(Key::N, ctrl), false), (Some(Item::PlayNext), false));
        assert_eq!(keyed(&ctx, press(Key::Q, ctrl), false), (Some(Item::Queue), false));
        assert_eq!(keyed(&ctx, press(Key::S, ctrl), false), (Some(Item::Like), false));
        assert_eq!(keyed(&ctx, press(Key::Delete, ctrl), false), (Some(Item::Block), false));
        // The old keys do nothing now, nor do the same with Shift or Alt too (AMD's and Discord's
        // keys, AltGr).
        for (key, held) in [
            (Key::E, ctrl),
            (Key::E, Modifiers { shift: true, ..ctrl }),
            (Key::A, Modifiers::ALT),
            (Key::R, Modifiers::ALT),
            (Key::N, Modifiers { shift: true, ..ctrl }),
            (Key::Q, Modifiers { alt: true, ..ctrl }),
            (Key::S, Modifiers { shift: true, ..ctrl }),
            (Key::N, Modifiers::NONE),
        ] {
            assert_eq!(keyed(&ctx, press(key, held), false), (None, true), "{key:?} with {held:?}");
        }
        // While a text box is typed in, the keys are its own.
        keyed(&ctx, Vec::new(), true);
        for key in [Key::N, Key::Q, Key::S, Key::Delete] {
            assert_eq!(keyed(&ctx, press(key, ctrl), true), (None, true), "{key:?} while typing");
        }
    }

    #[test]
    fn the_keys_act_on_the_picked_or_selected_song_else_the_playing_one() {
        let mut v = TrackView::new(Source::LikedSongs);
        v.set_tracks(Arc::new(vec![song("a"), song("b"), song("c")]));
        let playing = song("x");
        v.selected = Some(1);
        assert_eq!(uris(key_songs(Some(&v), Some(&playing))), ["spotify:track:b"]);
        // Several picked: all of them, in their order; one picked: the selected one.
        v.picked = [2, 0].into_iter().collect();
        assert_eq!(uris(key_songs(Some(&v), Some(&playing))), ["spotify:track:a", "spotify:track:c"]);
        v.picked = [2].into_iter().collect();
        assert_eq!(uris(key_songs(Some(&v), None)), ["spotify:track:b"]);
        // Filtered out: as if not there.
        v.picked = [0, 1].into_iter().collect();
        v.filtered = vec![0, 2];
        assert_eq!(uris(key_songs(Some(&v), Some(&playing))), ["spotify:track:x"]);
        // No list on screen, or nothing selected in it: the playing song, else none.
        assert_eq!(uris(key_songs(None, Some(&playing))), ["spotify:track:x"]);
        let empty = TrackView::new(Source::LikedSongs);
        assert_eq!(uris(key_songs(Some(&empty), Some(&playing))), ["spotify:track:x"]);
        assert!(key_songs(Some(&empty), None).is_empty() && key_songs(None, None).is_empty());
    }
}
