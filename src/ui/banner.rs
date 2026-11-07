//! Banners: a picture of the user's own across a list's header (its hero), shown only in Truffle.
//! Set from the list's page (right-click the header, or the picture button on the play bar) and
//! kept in `prefs.banners` by the list's `Source::cache_key`: the picture's full path and how it
//! sits. "Adjust banner…" opens a box to move it (drag) and zoom it (wheel or slider), as in
//! Discord's profile banners.
//!
//! Each picture is decoded once on its own thread (with the wallpaper's decoder, smaller) and the
//! few shown last are kept as textures. Nothing repaints by itself. While a picture loads, or when
//! it can't be read, the page looks as it does without one.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use egui::epaint::{Vertex, WHITE_UV};
use egui::{
    Color32, ColorImage, CornerRadius, CursorIcon, Mesh, Painter, Pos2, Rect, Sense, Shape, Stroke,
    StrokeKind, TextureHandle, TextureId, Ui, Vec2, pos2, vec2,
};

use crate::config::Banner;
use crate::model::Source;
use crate::ui::{TruffleApp, Undo};
use crate::ui::fin;
use crate::ui::theme::Palette;
use crate::ui::wallpaper::{self, Cap, TEXTURE};

/// A banner is kept at most this wide, and at most as many pixels as 1440p: it spans one column.
const CAP: Cap = Cap { width: 2560, pixels: 2560 * 1440 };
/// Banner pictures kept loaded: the ones shown last.
const KEEP: usize = 4;
/// The start of the error note, so a later choice can take it away again.
const BANNER_ERROR: &str = "Couldn't show the banner";
/// The line between the banner and the page under it.
const LINE: f32 = 2.0;
/// The dark over the whole banner: some from the start, more as it folds into the slim bar (the
/// name sits in its middle there, not on the darker foot).
const SHADE: f32 = 0.12;
const SHADE_FOLDED: f32 = 0.45;
/// Darker towards the foot, under the name and the details (place down the banner, strength).
const SHADE_DOWN: [(f32, f32); 4] = [(0.0, 0.0), (0.3, 0.1), (0.65, 0.4), (1.0, 0.75)];
/// Darker towards the left, under the cover and the start of the name (place across, strength).
const SHADE_LEFT: [(f32, f32); 3] = [(0.0, 0.5), (0.35, 0.25), (0.7, 0.0)];

/// The few banner pictures kept, by path, the one shown last at the end. Adding one more than
/// [`KEEP`] lets go of the one shown longest ago.
struct Kept<T> {
    entries: Vec<(String, T)>,
}

impl<T> Default for Kept<T> {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl<T> Kept<T> {
    /// The picture for `path`, now the one shown last.
    fn get(&mut self, path: &str) -> Option<&T> {
        let i = self.entries.iter().position(|(p, _)| p == path)?;
        let entry = self.entries.remove(i);
        self.entries.push(entry);
        self.entries.last().map(|(_, t)| t)
    }

    fn insert(&mut self, path: String, value: T) {
        self.remove(&path);
        self.entries.push((path, value));
        if self.entries.len() > KEEP {
            self.entries.drain(..self.entries.len() - KEEP);
        }
    }

    fn remove(&mut self, path: &str) {
        self.entries.retain(|(p, _)| p != path);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// A finished load.
struct Loaded {
    path: String,
    /// Which load it was: a result for a load given up meanwhile is dropped.
    serial: u64,
    /// The list's name, for the note when it fails.
    name: String,
    result: Result<ColorImage, String>,
}

/// The loaded banner pictures, the loads under way and the file picker.
pub(in crate::ui) struct Banners {
    kept: Kept<TextureHandle>,
    /// Paths being decoded, with their load's number.
    loading: Vec<(String, u64)>,
    serial: u64,
    /// Paths that couldn't be shown, and why: not tried again until chosen again.
    failed: HashMap<String, String>,
    loaded_tx: Sender<Loaded>,
    loaded_rx: Receiver<Loaded>,
    picker_open: bool,
    /// A picked file (or none: cancelled), for the list with this cache key.
    picked_tx: Sender<(String, Option<PathBuf>)>,
    picked_rx: Receiver<(String, Option<PathBuf>)>,
    /// The size of the last banner drawn on a page (its whole hero), so the editor shows it in
    /// that shape.
    pub(in crate::ui) frame: Vec2,
    pub(in crate::ui) adjusting: Option<Adjusting>,
}

impl Default for Banners {
    fn default() -> Self {
        let (loaded_tx, loaded_rx) = std::sync::mpsc::channel();
        let (picked_tx, picked_rx) = std::sync::mpsc::channel();
        Self {
            kept: Kept::default(),
            loading: Vec::new(),
            serial: 0,
            failed: HashMap::new(),
            loaded_tx,
            loaded_rx,
            picker_open: false,
            picked_tx,
            picked_rx,
            frame: vec2(560.0, 200.0),
            adjusting: None,
        }
    }
}

impl Banners {
    /// The file picker is open.
    pub(super) fn picking(&self) -> bool {
        self.picker_open
    }

    /// Lets go of a picture: its texture, its load under way and its failure.
    fn forget(&mut self, path: &str) {
        self.kept.remove(path);
        self.loading.retain(|(p, _)| p != path);
        self.failed.remove(path);
    }

    fn forget_all(&mut self) {
        self.kept.clear();
        self.loading.clear();
        self.failed.clear();
    }
}

/// What a banner menu item asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BannerAct {
    Pick,
    Adjust,
    Remove,
}

/// A banner shown on a list's page: its texture and the picture's size.
#[derive(Clone, Copy, Debug)]
pub(super) struct Shown {
    pub texture: TextureId,
    pub size: Vec2,
    pub zoom: f32,
    /// The picture's point at the banner's middle (0 to 1).
    pub focus: Pos2,
}

/// The "Adjust banner" box: the list it is for, the banner being changed, and how it was.
pub(in crate::ui) struct Adjusting {
    key: String,
    /// The list's name, for the title.
    name: String,
    banner: Banner,
    before: Banner,
}

impl TruffleApp {
    /// Runs every drawn frame (from `wallpaper_logic`): takes a picked file and finished loads.
    pub(in crate::ui) fn banner_logic(&mut self, ctx: &egui::Context) {
        while let Ok((key, picked)) = self.wallpaper.banners.picked_rx.try_recv() {
            self.wallpaper.banners.picker_open = false;
            if let Some(path) = picked {
                let path = path.to_string_lossy().into_owned();
                // Picking the same file again loads it again (it may have been changed meanwhile).
                self.wallpaper.banners.forget(&path);
                let banner = Banner::new(path);
                if let Some(old) = self.prefs.banners.insert(key.clone(), banner.clone()) {
                    self.forget_unused_banner(&old.path);
                }
                self.clear_banner_errors();
                self.prefs_changed();
                // A new picture opens the editor, to place it.
                let name = self.banner_list_name(&key);
                self.wallpaper.banners.adjusting = Some(Adjusting { key, name, before: banner.clone(), banner });
            }
        }
        let max_side = ctx.input(|i| i.max_texture_side);
        while let Ok(done) = self.wallpaper.banners.loaded_rx.try_recv() {
            let b = &mut self.wallpaper.banners;
            let Some(i) = b.loading.iter().position(|(p, s)| *p == done.path && *s == done.serial) else { continue };
            b.loading.swap_remove(i);
            match done.result {
                // Shrunk to fit already, but a texture too big for the graphics card would stop Truffle.
                Ok(image) if image.width() > max_side || image.height() > max_side => {
                    self.banner_failed(done.path, &done.name, "the picture is too large".to_owned());
                }
                Ok(image) => {
                    let texture = ctx.load_texture(format!("banner:{}", done.path), image, TEXTURE);
                    b.kept.insert(done.path, texture);
                }
                Err(reason) => self.banner_failed(done.path, &done.name, reason),
            }
        }
    }

    /// The banner to show on `source`'s page, once its picture has loaded; the first time it is
    /// asked for, its loading starts. `name` is the list's, for a note if it can't be read.
    pub(super) fn banner_for(&mut self, ctx: &egui::Context, source: &Source, name: &str) -> Option<Shown> {
        let key = source.cache_key();
        // While its editor is open, the page shows the banner as it is being placed.
        let banner = match &self.wallpaper.banners.adjusting {
            Some(a) if a.key == key => a.banner.clone(),
            _ => self.prefs.banners.get(&key)?.clone(),
        };
        self.banner_picture(ctx, &banner, name)
    }

    /// The loaded picture for `banner`, starting its load the first time.
    fn banner_picture(&mut self, ctx: &egui::Context, banner: &Banner, name: &str) -> Option<Shown> {
        let b = &mut self.wallpaper.banners;
        if let Some(texture) = b.kept.get(&banner.path) {
            return Some(Shown { texture: texture.id(), size: texture.size_vec2(), zoom: banner.zoom, focus: pos2(banner.x, banner.y) });
        }
        if b.failed.contains_key(&banner.path) || b.loading.iter().any(|(p, _)| *p == banner.path) {
            return None;
        }
        self.load_banner(ctx, banner.path.clone(), name.to_owned());
        None
    }

    /// The name of the list with this cache key, for the editor's title.
    fn banner_list_name(&self, key: &str) -> String {
        if key == Source::LikedSongs.cache_key() {
            return "Liked Songs".to_owned();
        }
        self.playlists
            .iter()
            .find(|pl| Source::Playlist(pl.uri.clone()).cache_key() == key)
            .map(|pl| pl.name.clone())
            .or_else(|| self.source_info.values().find(|i| i.source.cache_key() == key).map(|i| i.title.clone()))
            .unwrap_or_else(|| "this page".to_owned())
    }

    /// Decodes the picture on its own thread.
    fn load_banner(&mut self, ctx: &egui::Context, path: String, name: String) {
        let max_side = u32::try_from(ctx.input(|i| i.max_texture_side)).unwrap_or(u32::MAX);
        let b = &mut self.wallpaper.banners;
        b.serial += 1;
        let serial = b.serial;
        b.loading.push((path.clone(), serial));
        let (tx, ctx) = (b.loaded_tx.clone(), ctx.clone());
        let job_path = path.clone();
        let job_name = name.clone();
        let spawned = std::thread::Builder::new().name("truffle-banner".into()).spawn(move || {
            let result = wallpaper::decode(Path::new(&job_path), CAP, max_side);
            let _ = tx.send(Loaded { path: job_path, serial, name: job_name, result });
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            log::warn!("could not start loading a banner: {e}");
            self.wallpaper.banners.loading.retain(|(_, s)| *s != serial);
            self.banner_failed(path, &name, "Truffle couldn't start loading it".to_owned());
        }
    }

    /// Remembers why the picture can't be shown (it isn't tried again until chosen again) and
    /// says so in a note.
    fn banner_failed(&mut self, path: String, name: &str, reason: String) {
        log::warn!("could not load the banner {path}: {reason}");
        self.toast(format!("{BANNER_ERROR} of {name}: {reason}. Right-click its header to choose another."), true);
        self.wallpaper.banners.failed.insert(path, reason);
    }

    /// An error about an earlier banner is out of date once banners change.
    fn clear_banner_errors(&mut self) {
        self.toasts.retain(|t| !t.text.starts_with(BANNER_ERROR));
    }

    /// Lets go of a picture no list uses any more.
    fn forget_unused_banner(&mut self, path: &str) {
        if !self.prefs.banners.values().any(|b| b.path == path) {
            self.wallpaper.banners.forget(path);
        }
    }

    /// Opens the native file picker for `source`'s banner on a background thread (it blocks
    /// until closed).
    fn pick_banner(&mut self, ctx: &egui::Context, source: &Source) {
        if self.wallpaper.banners.picker_open {
            return;
        }
        let key = source.cache_key();
        // Start in the folder of this list's banner, else of the wallpaper.
        let near = self.prefs.banners.get(&key).map_or(&self.prefs.wallpaper.path, |b| &b.path);
        let folder = Path::new(near).parent().map(Path::to_path_buf);
        self.wallpaper.banners.picker_open = true;
        let tx = self.wallpaper.banners.picked_tx.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("truffle-file-dialog".into()).spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Choose a banner")
                .add_filter("Images", &["jpg", "jpeg", "png", "webp"]);
            if let Some(folder) = folder.filter(|f| f.is_dir()) {
                dialog = dialog.set_directory(folder);
            }
            let _ = tx.send((key, dialog.pick_file()));
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            log::warn!("could not open the file picker: {e}");
            self.wallpaper.banners.picker_open = false;
            self.toast("Could not open the file picker.", true);
        }
    }

    fn remove_banner(&mut self, source: &Source) {
        let key = source.cache_key();
        if let Some(old) = self.prefs.banners.remove(&key) {
            self.forget_unused_banner(&old.path);
            self.clear_banner_errors();
            self.prefs_changed();
            self.note("Banner removed", Some(Undo::Banners(BTreeMap::from([(key, old)]))));
        }
    }

    /// Settings' REMOVE ALL.
    pub(in crate::ui) fn remove_all_banners(&mut self) {
        let count = self.prefs.banners.len();
        if count == 0 {
            return;
        }
        let before = std::mem::take(&mut self.prefs.banners);
        self.wallpaper.banners.forget_all();
        self.clear_banner_errors();
        self.prefs_changed();
        let text = if count == 1 { "Banner removed".to_owned() } else { format!("{count} banners removed") };
        self.note(text, Some(Undo::Banners(before)));
    }

    /// UNDO after removing banners: they come back, except on lists given another one meanwhile.
    /// Their pictures load again when their pages show.
    pub(in crate::ui) fn put_banners_back(&mut self, banners: BTreeMap<String, Banner>) {
        let mut back = 0;
        for (key, path) in banners {
            if let std::collections::btree_map::Entry::Vacant(slot) = self.prefs.banners.entry(key) {
                slot.insert(path);
                back += 1;
            }
        }
        let text = match back {
            0 => "Those lists have other banners now".to_owned(),
            1 => "Banner put back".to_owned(),
            n => format!("{n} banners put back"),
        };
        if back > 0 {
            self.prefs_changed();
        }
        self.toast(text, false);
    }

    /// Does what a banner menu item asked for, for `source`'s page.
    pub(super) fn banner_act(&mut self, ctx: &egui::Context, source: &Source, act: BannerAct) {
        match act {
            BannerAct::Pick => self.pick_banner(ctx, source),
            BannerAct::Adjust => {
                let key = source.cache_key();
                if let Some(banner) = self.prefs.banners.get(&key).cloned() {
                    let name = self.banner_list_name(&key);
                    self.wallpaper.banners.adjusting = Some(Adjusting { key, name, before: banner.clone(), banner });
                }
            }
            BannerAct::Remove => self.remove_banner(source),
        }
    }

    /// The "Adjust banner" box: the banner in its page's shape; drag to move it, the wheel or the
    /// slider to zoom. SAVE keeps it, CANCEL (or Esc) puts it back as it was.
    pub(in crate::ui) fn banner_adjust_modal(&mut self, ctx: &egui::Context) {
        let Some(adjusting) = self.wallpaper.banners.adjusting.as_ref() else { return };
        let (key, name, mut banner) = (adjusting.key.clone(), adjusting.name.clone(), adjusting.banner.clone());
        let p = self.palette;
        let shown = self.banner_picture(ctx, &banner, &name);
        let frame = self.wallpaper.banners.frame;
        let (mut save, mut cancel, mut reset) = (false, false, false);
        let modal = fin::dialog(ctx, egui::Id::new("banner_adjust"), |ui| {
            let w = 560.0_f32.min(ctx.content_rect().width() - 100.0).max(260.0);
            ui.set_width(w);
            fin::dialog_head(ui, &format!("Adjust banner · {name}"), Some("Drag to move it, scroll to zoom."));
            ui.add_space(14.0);
            // Up to a little taller than wide: Split's hero column (Wide screens).
            let aspect = if frame.x > 0.0 { (frame.y / frame.x).clamp(0.15, 1.2) } else { 0.36 };
            let (rect, resp) = ui.allocate_exact_size(vec2(w, (w * aspect).round()), Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, CornerRadius::same(8), p.main_bg);
            match shown {
                Some(shown) => {
                    let uv = crop_uv(shown.size, rect.size(), banner.zoom, pos2(banner.x, banner.y));
                    painter.image(shown.texture, rect, uv, Color32::WHITE);
                    // Dragging moves the picture with the pointer.
                    let drag = resp.drag_delta();
                    let mut focus = uv.center();
                    if drag != Vec2::ZERO {
                        focus.x -= drag.x / rect.width() * uv.width();
                        focus.y -= drag.y / rect.height() * uv.height();
                    }
                    // The wheel zooms, around the middle.
                    if resp.hovered() {
                        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                        if scroll != 0.0 {
                            banner.zoom = (banner.zoom * (1.0 + scroll * 0.002)).clamp(1.0, Banner::MAX_ZOOM);
                        }
                    }
                    // Kept where the picture still covers the whole banner.
                    let settled = crop_uv(shown.size, rect.size(), banner.zoom, focus).center();
                    banner.x = settled.x;
                    banner.y = settled.y;
                }
                None => {
                    painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Loading the picture…", crate::ui::theme::regular(12.0), p.text_weak);
                }
            }
            painter.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
            let cursor = if resp.dragged() { CursorIcon::Grabbing } else { CursorIcon::Grab };
            resp.on_hover_cursor(cursor);
            // Zoom: the name, a line, how far in, and RESET.
            ui.add_space(12.0);
            let (row, _) = ui.allocate_exact_size(vec2(w, 28.0), Sense::hover());
            let mid = row.center().y;
            ui.painter().text(pos2(row.left(), mid), egui::Align2::LEFT_CENTER, "Zoom", crate::ui::theme::regular(12.0), p.text);
            let reset_w = fin::text_button_w(ui, "Reset", false);
            if fin::text_button(ui, &p, pos2(row.right() - reset_w, row.top()), egui::Id::new("banner-reset"), "Reset", None, false, true).clicked() {
                reset = true;
            }
            let pct_right = row.right() - reset_w - 12.0;
            ui.painter().text(pos2(pct_right, mid), egui::Align2::RIGHT_CENTER, format!("{:.0}%", banner.zoom * 100.0), crate::ui::theme::regular(11.0), p.text_weak);
            let line = Rect::from_min_max(pos2(row.left() + 52.0, mid - 6.0), pos2(pct_right - 44.0, mid + 6.0));
            let frac = (banner.zoom - 1.0) / (Banner::MAX_ZOOM - 1.0);
            if let (_, Some(f)) = fin::line(ui, &p, line, egui::Id::new("banner-zoom"), frac, shown.is_some()) {
                banner.zoom = 1.0 + f.clamp(0.0, 1.0) * (Banner::MAX_ZOOM - 1.0);
            }
            let (ok, no) = fin::dialog_actions(ui, &p, egui::Id::new("banner_adjust"), "Save", true);
            save |= ok;
            cancel |= no;
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                save = true;
            }
        });
        if reset {
            banner = Banner::new(banner.path.clone());
        }
        if let Some(a) = self.wallpaper.banners.adjusting.as_mut() {
            a.banner = banner.clone();
        }
        if save {
            self.wallpaper.banners.adjusting = None;
            if let Some(slot) = self.prefs.banners.get_mut(&key) {
                *slot = banner;
                self.prefs_changed();
            }
        } else if (cancel || modal.should_close())
            // Closing the box (taken here) puts the banner back as it was before.
            && let Some(a) = self.wallpaper.banners.adjusting.take()
            && let Some(slot) = self.prefs.banners.get_mut(&key)
            && *slot != a.before
        {
            *slot = a.before;
            self.prefs_changed();
        }
    }
}

/// The banner's items in a menu: set (or change) it, and remove it while one is `set`.
pub(super) fn banner_items(ui: &mut Ui, set: bool, picking: bool) -> Option<BannerAct> {
    let mut act = None;
    let label = if set { "Change banner" } else { "Set banner" };
    let r = crate::ui::fin::menu_item(ui, Some("palette"), label, None, false, !picking).on_hover_text("A picture across the top of this page");
    if r.clicked() {
        act = Some(BannerAct::Pick);
        ui.close();
    }
    if set && crate::ui::fin::menu_item(ui, Some("drag"), "Adjust banner", None, false, true).on_hover_text("Move and zoom it").clicked() {
        act = Some(BannerAct::Adjust);
        ui.close();
    }
    if set && crate::ui::fin::menu_item(ui, Some("x"), "Remove banner", None, false, true).clicked() {
        act = Some(BannerAct::Remove);
        ui.close();
    }
    act
}

/// The part of a `size` picture (texture coordinates, 0 to 1) shown in an `area`-sized rect,
/// `zoom` times closer than covering it, around the picture's point `focus`; moved as little as
/// needed to stay inside the picture (so the banner is always covered).
fn crop_uv(size: Vec2, area: Vec2, zoom: f32, focus: Pos2) -> Rect {
    let cover = cover_uv(size, area);
    let zoom = if zoom.is_finite() { zoom.max(1.0) } else { 1.0 };
    let (w, h) = (cover.width() / zoom, cover.height() / zoom);
    let x = focus.x.clamp(w / 2.0, 1.0 - w / 2.0);
    let y = focus.y.clamp(h / 2.0, 1.0 - h / 2.0);
    Rect::from_center_size(pos2(x, y), vec2(w, h))
}

/// The part of a `size` picture (in texture coordinates, 0 to 1) that fills an `area`-sized
/// rect without stretching: the picture covers it, with its middle in view.
fn cover_uv(size: Vec2, area: Vec2) -> Rect {
    let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    if size.x <= 0.0 || size.y <= 0.0 || area.x <= 0.0 || area.y <= 0.0 {
        return whole;
    }
    let (picture, wanted) = (size.x / size.y, area.x / area.y);
    if picture > wanted {
        // Wider than the area: the sides are cut off.
        let w = wanted / picture;
        Rect::from_min_max(pos2(0.5 - w / 2.0, 0.0), pos2(0.5 + w / 2.0, 1.0))
    } else {
        let h = picture / wanted;
        Rect::from_min_max(pos2(0.0, 0.5 - h / 2.0), pos2(1.0, 0.5 + h / 2.0))
    }
}

/// The part of the picture shown over a `width` x `shown` strip of a head whose whole hero is
/// `full` tall: the picture covers the whole hero as the user placed it, and as the hero folds
/// the strip keeps its middle (the folded bar shows the same picture, cropped, at the same size).
fn banner_uv(size: Vec2, width: f32, full: f32, shown: f32, zoom: f32, focus: Pos2) -> Rect {
    let full = full.max(shown);
    let uv = crop_uv(size, vec2(width, full), zoom, focus);
    if full <= 0.0 {
        return uv;
    }
    let h = uv.height() * (shown / full);
    let mid = uv.center().y;
    Rect::from_min_max(pos2(uv.left(), mid - h / 2.0), pos2(uv.right(), mid + h / 2.0))
}

/// Paints the banner over `rect`, the part of the head shown, whose whole hero is `full` tall and
/// folded by `folded` (0: all of it shows, 1: the slim bar). First the page's colour, so the
/// wallpaper never shows through (nor through a see-through picture); then the picture; then a
/// dark gradient towards the foot and the left, under the words; and the line along its foot (and
/// a hairline along its top).
pub(super) fn paint_banner(painter: &Painter, p: &Palette, rect: Rect, full: f32, folded: f32, banner: Shown) {
    painter.rect_filled(rect, CornerRadius::ZERO, p.main_bg);
    let uv = banner_uv(banner.size, rect.width(), full, rect.height(), banner.zoom, banner.focus);
    painter.image(banner.texture, rect, uv, Color32::WHITE);
    let shade = egui::lerp(SHADE..=SHADE_FOLDED, folded.clamp(0.0, 1.0));
    painter.rect_filled(rect, CornerRadius::ZERO, black(shade));
    painter.add(shade_band(rect, &SHADE_DOWN, true));
    painter.add(shade_band(rect, &SHADE_LEFT, false));
    let line = Rect::from_min_max(pos2(rect.left(), rect.bottom() - LINE), rect.max);
    painter.rect_filled(line, CornerRadius::ZERO, p.accent_ink);
    // The tab row above may show the wallpaper too: a hairline sets the banner apart there, as
    // the line under the tabs does on the other tabs.
    let top = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 1.0));
    painter.rect_filled(top, CornerRadius::ZERO, p.border);
}

fn black(strength: f32) -> Color32 {
    Color32::from_black_alpha((strength.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Black over `rect` along `stops` (place 0 to 1, strength 0 to 1): down it when `down`, else
/// across it from the left. Past the last stop nothing is drawn.
fn shade_band(rect: Rect, stops: &[(f32, f32)], down: bool) -> Shape {
    let mut mesh = Mesh::default();
    for (i, &(t, k)) in stops.iter().enumerate() {
        let color = black(k);
        let (a, b) = if down {
            let y = egui::lerp(rect.y_range(), t);
            (pos2(rect.left(), y), pos2(rect.right(), y))
        } else {
            let x = egui::lerp(rect.x_range(), t);
            (pos2(x, rect.top()), pos2(x, rect.bottom()))
        };
        mesh.vertices.push(Vertex { pos: a, uv: WHITE_UV, color });
        mesh.vertices.push(Vertex { pos: b, uv: WHITE_UV, color });
        if i > 0 {
            let n = 2 * i as u32;
            mesh.add_triangle(n - 2, n - 1, n);
            mesh.add_triangle(n - 1, n + 1, n);
        }
    }
    mesh.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Rect, b: Rect) -> bool {
        (a.min - b.min).length() < 1e-5 && (a.max - b.max).length() < 1e-5
    }

    fn uv(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(pos2(x0, y0), pos2(x1, y1))
    }

    #[test]
    fn the_picture_covers_the_area_with_its_middle_in_view() {
        // A wide picture in a less wide area: its sides are cut off, evenly.
        assert!(close(cover_uv(vec2(400.0, 100.0), vec2(200.0, 100.0)), uv(0.25, 0.0, 0.75, 1.0)));
        // A tall one in a wide area: top and bottom go.
        assert!(close(cover_uv(vec2(100.0, 100.0), vec2(400.0, 100.0)), uv(0.0, 0.375, 1.0, 0.625)));
        // The same shape: all of it.
        assert!(close(cover_uv(vec2(1600.0, 400.0), vec2(800.0, 200.0)), uv(0.0, 0.0, 1.0, 1.0)));
        // Nothing to fit: all of it, never a division by zero.
        assert!(close(cover_uv(vec2(0.0, 100.0), vec2(800.0, 200.0)), uv(0.0, 0.0, 1.0, 1.0)));
        assert!(close(cover_uv(vec2(800.0, 200.0), vec2(800.0, 0.0)), uv(0.0, 0.0, 1.0, 1.0)));
    }

    #[test]
    fn the_folded_bar_shows_the_middle_of_the_same_picture() {
        // A 2000 x 1000 picture over an 800 x 200 hero: all its width, the middle 500 rows.
        let mid = pos2(0.5, 0.5);
        let whole = banner_uv(vec2(2000.0, 1000.0), 800.0, 200.0, 200.0, 1.0, mid);
        assert!(close(whole, uv(0.0, 0.25, 1.0, 0.75)));
        // Folded to 50 tall: a quarter of those rows, around the same middle, at the same size.
        let folded = banner_uv(vec2(2000.0, 1000.0), 800.0, 200.0, 50.0, 1.0, mid);
        assert!(close(folded, uv(0.0, 0.4375, 1.0, 0.5625)));
        // A strip taller than the hero fits the strip instead (never stretched).
        assert!(close(banner_uv(vec2(2000.0, 1000.0), 800.0, 40.0, 50.0, 1.0, mid), cover_uv(vec2(2000.0, 1000.0), vec2(800.0, 50.0))));
    }

    #[test]
    fn a_moved_and_zoomed_banner_stays_inside_its_picture() {
        let (size, area) = (vec2(2000.0, 1000.0), vec2(800.0, 200.0));
        // Zoomed in twice: half the width and height of the covering crop, around the focus.
        let z = crop_uv(size, area, 2.0, pos2(0.5, 0.5));
        assert!(close(z, uv(0.25, 0.375, 0.75, 0.625)));
        // Pushed past the edge: it stops at the picture's edge, still covering the banner.
        let edge = crop_uv(size, area, 2.0, pos2(0.0, 1.0));
        assert!(close(edge, uv(0.0, 0.75, 0.5, 1.0)));
        // Zoom 1 covers as before; nonsense zoom counts as 1.
        assert!(close(crop_uv(size, area, 1.0, pos2(0.5, 0.5)), cover_uv(size, area)));
        assert!(close(crop_uv(size, area, f32::NAN, pos2(0.5, 0.5)), cover_uv(size, area)));
    }

    #[test]
    fn old_banners_saved_as_a_path_still_load() {
        let old: BTreeMap<String, Banner> = serde_json::from_str(r#"{"a": "C:\\pics\\x.png"}"#).unwrap();
        assert_eq!(old["a"], Banner::new("C:\\pics\\x.png".to_owned()));
        let new: BTreeMap<String, Banner> = serde_json::from_str(r#"{"a": {"path": "p.png", "zoom": 9, "x": 0.2}}"#).unwrap();
        assert_eq!(new["a"], Banner { path: "p.png".into(), zoom: Banner::MAX_ZOOM, x: 0.2, y: 0.5 });
        let back: BTreeMap<String, Banner> = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back, new);
    }

    #[test]
    fn the_banner_shown_longest_ago_goes_first() {
        let mut kept = Kept::default();
        for (i, path) in ["a", "b", "c", "d"].into_iter().enumerate() {
            kept.insert(path.to_owned(), i);
        }
        // Showing "a" again keeps it; "b" is now the one shown longest ago.
        assert_eq!(kept.get("a"), Some(&0));
        kept.insert("e".to_owned(), 4);
        assert_eq!(kept.entries.len(), KEEP);
        assert_eq!(kept.get("b"), None);
        assert_eq!(kept.get("a"), Some(&0));
        // Loading a picture again replaces it rather than keeping it twice.
        kept.insert("c".to_owned(), 7);
        assert_eq!(kept.entries.len(), KEEP);
        assert_eq!(kept.get("c"), Some(&7));
        kept.insert("f".to_owned(), 5);
        assert_eq!(kept.get("d"), None);
        let left: Vec<&str> = kept.entries.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(left, ["e", "a", "c", "f"]);
        kept.remove("a");
        kept.clear();
        assert!(kept.entries.is_empty());
    }
}
