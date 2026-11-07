//! The wallpaper, as in Glint: a picture behind the songs or behind the whole window, fitted,
//! placed, zoomed, blurred and faded at the edges the way Glint's Wallpaper card sets it. It is
//! decoded once on its own thread (only the newest picture asked for) and kept as one texture,
//! with a copy at half its size for blurring it again (on a thread of its own, only the newest
//! blur asked for); it never repaints by itself, and nothing is drawn while no picture is loaded.
//! Settings shows the pictures next to it as small ones to click, read on a thread too and only
//! while the Wallpaper card is shown. The list pages' banners (`tracks::banner`) are pictures from
//! the user's own files too: they are decoded here as well and kept beside the wallpaper.

use std::io::{BufRead, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use egui::epaint::Vertex;
use egui::{
    Color32, ColorImage, CornerRadius, CursorIcon, Id, Mesh, Pos2, Rangef, Rect, Sense, Stroke, StrokeKind, TextureFilter,
    TextureHandle, TextureId, TextureOptions, Ui, Vec2, pos2, vec2,
};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageReader};

use super::TruffleApp;
use super::fin;
use super::tracks::banner::Banners;
use crate::config::{Wallpaper, WallpaperFit};

/// The picture is kept at most this wide, and at most as many pixels as 4K (as in Glint).
const MAX_WIDTH: u32 = 3840;
const MAX_PIXELS: u64 = 3840 * 2160;
const WALLPAPER_CAP: Cap = Cap { width: MAX_WIDTH, pixels: MAX_PIXELS };
/// A blurred picture has no fine detail to keep: it is made at half that size, as in Glint.
const BLUR_CAP: Cap = Cap { width: MAX_WIDTH / 2, pixels: MAX_PIXELS / 4 };
/// The start of the status line's error, so a later choice can take it away again.
const WALLPAPER_ERROR: &str = "Couldn't show the wallpaper";
/// Smooth when shrunk a lot too (the picture is often far bigger than the area it fills).
pub(super) const TEXTURE: TextureOptions = TextureOptions::LINEAR.with_mipmap_mode(Some(TextureFilter::Linear));
/// The same, repeated past its edges: a tiled wallpaper.
const TILED: TextureOptions = TextureOptions::LINEAR_REPEAT.with_mipmap_mode(Some(TextureFilter::Linear));
/// The picture kinds Truffle opens.
const PICTURE_KINDS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// The pictures next to the wallpaper, as small pictures to click: at most this many, read this
/// small, and shown this big (as in Glint).
pub(super) const MAX_THUMBS: usize = 8;
const THUMB_CAP: Cap = Cap { width: 200, pixels: 200 * 200 };
pub(super) const THUMB_SIZE: Vec2 = vec2(70.0, 44.0);
const THUMB_GAP: f32 = 8.0;
/// Where the wallpaper sits: the pad with the picture on it (the area's shape, between these
/// wide), the 3x3 squares and the gap between them.
pub(super) const PAD_H: f32 = 100.0;
const PAD_MIN_W: f32 = 120.0;
const PAD_MAX_W: f32 = 220.0;
const GRID_CELL: f32 = 24.0;
const GRID_GAP: f32 = 4.0;
pub(super) const GRID_W: f32 = 3.0 * GRID_CELL + 2.0 * GRID_GAP;
pub(super) const PAD_GRID_GAP: f32 = 22.0;
/// Each fading edge is drawn in this many steps (its corners fade both ways, which one step
/// across a corner can't show).
const FADE_STEPS: usize = 6;

/// Glint's fit choices, as its drop-down names them.
pub(super) const FITS: [(WallpaperFit, &str); 5] = [
    (WallpaperFit::Fill, "Fill"),
    (WallpaperFit::Whole, "Whole"),
    (WallpaperFit::Stretch, "Stretch"),
    (WallpaperFit::Tile, "Tile"),
    (WallpaperFit::Actual, "Actual"),
];

/// A wallpaper slider, as on Glint's Wallpaper card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Slide {
    /// Where it sits across and down, 0 to 100%.
    Across,
    Down,
    /// The saved `strength`.
    Opacity,
    Zoom,
    Blur,
    SoftEdges,
}

/// Each slider: its row's title, smallest and biggest (Glint's ranges), in the card's order.
pub(super) const SLIDES: [(Slide, &str, f32, f32); 6] = [
    (Slide::Across, "Across (X)", 0.0, 100.0),
    (Slide::Down, "Down (Y)", 0.0, 100.0),
    (Slide::Opacity, "Opacity", 0.0, 100.0),
    (Slide::Zoom, "Zoom", -500.0, 500.0),
    (Slide::Blur, "Blur", 0.0, 50.0),
    (Slide::SoftEdges, "Soft edges", 0.0, 50.0),
];

/// A finished load: which load it was, and the picture or why it couldn't be opened.
type Loaded = (u64, Result<Fresh, String>);

/// A picture to load: which load it is, the file, the graphics card's largest texture, and the
/// blur (how many pixels, and whether it runs round the edges) to make of it straight away, so a
/// blurred wallpaper never shows sharp first.
struct LoadJob {
    serial: u64,
    path: PathBuf,
    max_side: u32,
    blur: Option<(u8, bool)>,
}

/// A loaded picture: as shown, kept at most 4K; its copy for blurring; how wide it would be kept
/// on any graphics card (see [`Kept`]); and the blur asked for with it.
struct Fresh {
    image: Arc<ColorImage>,
    source: Arc<ColorImage>,
    fitted_w: u32,
    blurred: Option<(u8, bool, ColorImage)>,
}

/// How big a picture is kept: at most `width` wide and `pixels` big (and never more than the
/// graphics card's largest texture along a side).
#[derive(Clone, Copy, Debug)]
pub(super) struct Cap {
    pub width: u32,
    pub pixels: u64,
}

/// What is kept of the shown picture once it is on the graphics card: its copy for blurring (at
/// most half of 4K, as a blur has no fine detail to keep), the load it came from, and how wide the
/// sharp picture would be kept on any graphics card: a blur is in those pixels (Glint's "picture's
/// own pixels"), whatever size it is made at.
struct Kept {
    source: Arc<ColorImage>,
    serial: u64,
    fitted_w: u32,
}

/// A blur: of which load of the picture, how many pixels, and whether its edges blur into the
/// opposite ones (a tiled picture, whose copies then meet without a line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlurKey {
    serial: u64,
    radius: u8,
    wrap: bool,
}

/// A blur to make, and the picture to make it of.
struct BlurJob {
    key: BlurKey,
    picture: Arc<ColorImage>,
    fitted_w: u32,
}

/// Shared with a thread of its own: the newest work asked for (asking again replaces work not
/// started yet, so dragging the Blur slider or clicking through pictures never queues them up, and
/// at most one runs at a time) and whether the thread runs.
struct Newest<J> {
    asked: Option<J>,
    running: bool,
}

impl<J> Default for Newest<J> {
    fn default() -> Self {
        Self { asked: None, running: false }
    }
}

/// Asks `slot`'s thread to do `job`, starting it (named `name`, doing each job with `work`) unless
/// it runs already: it takes the newest job once it's done with the one it's on, and ends when
/// nothing more is asked.
fn ask_newest<J: Send + 'static>(slot: &Arc<Mutex<Newest<J>>>, job: J, name: &str, work: impl Fn(J) + Send + 'static) -> std::io::Result<()> {
    let mut asked = slot.lock().unwrap_or_else(PoisonError::into_inner);
    asked.asked = Some(job);
    if std::mem::replace(&mut asked.running, true) {
        return Ok(());
    }
    drop(asked);
    let shared = Arc::clone(slot);
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
        loop {
            let job = {
                let mut asked = shared.lock().unwrap_or_else(PoisonError::into_inner);
                match asked.asked.take() {
                    Some(job) => job,
                    None => {
                        asked.running = false;
                        return;
                    }
                }
            };
            work(job);
        }
    });
    if let Err(e) = spawned {
        let mut asked = slot.lock().unwrap_or_else(PoisonError::into_inner);
        asked.running = false;
        asked.asked = None;
        return Err(e);
    }
    Ok(())
}

/// Work asked of `slot`'s thread and not started yet isn't needed any more.
fn forget<J>(slot: &Mutex<Newest<J>>) {
    slot.lock().unwrap_or_else(PoisonError::into_inner).asked = None;
}

/// What the thread reading the pictures next to the wallpaper sends: the list (newest first), then
/// each picture small as it's read, then that it's done (a picture that can't be read is left
/// out). Each with the read it belongs to.
enum NearbyMsg {
    Listed(u64, Vec<PathBuf>),
    Thumb(u64, usize, ColorImage),
    Done(u64),
}

/// The pictures next to the wallpaper, for the Nearby row.
struct Nearby {
    /// The folder they're from: None until the row is shown (again, after Settings closed).
    folder: Option<PathBuf>,
    /// The last picture's folder, kept while Truffle runs: after NONE its pictures still show, so
    /// one click goes back.
    last_folder: Option<PathBuf>,
    /// The folder was listed (until then nothing is said about it being empty).
    listed: bool,
    /// Each picture was read, or couldn't be: nothing more is coming.
    done: bool,
    pictures: Vec<(PathBuf, Option<TextureHandle>)>,
    /// Counts reads; the thread stops once `wanted` is another.
    serial: u64,
    wanted: Arc<AtomicU64>,
    /// The pass the row was last shown in (see `WallpaperState::pass`).
    seen: u64,
    tx: Sender<NearbyMsg>,
    rx: Receiver<NearbyMsg>,
}

impl Default for Nearby {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            folder: None,
            last_folder: None,
            listed: false,
            done: false,
            pictures: Vec::new(),
            serial: 0,
            wanted: Arc::default(),
            seen: 0,
            tx,
            rx,
        }
    }
}

/// The picture grabbed on the Position pad: where, its place then (0 to 100 across and down), how
/// far it can move in the pad (the pad less the picture), and whether Esc let go of it (it stays
/// let go of until the button is released).
#[derive(Clone, Copy, Debug)]
struct PadGrab {
    from: Pos2,
    place: (f32, f32),
    room: Vec2,
    let_go: bool,
}

/// A wallpaper slider or the pad held with the mouse: the wallpaper as it was when it was grabbed
/// (Esc puts it back, as in Glint), the slider's line (None for the pad), and the pass it was last
/// seen held in (see `WallpaperState::pass`).
struct Held {
    before: Wallpaper,
    line: Option<Id>,
    pass: u64,
}

/// The loaded picture, its blur, and the load and file picker under way; Settings' pictures next
/// to it; and the banners' pictures.
pub(super) struct WallpaperState {
    /// The path the newest load, or the failure, belongs to.
    path: String,
    /// Counts loads, so a result for an earlier choice is dropped.
    serial: u64,
    /// The newest load was for a tiled wallpaper (its texture repeats past its edges).
    asked_tiled: bool,
    /// The picture shown (until a newer one is loaded, so nothing blinks while it loads).
    picture: Option<Kept>,
    texture: Option<TextureHandle>,
    /// The texture repeats past its edges (made so for a tiled wallpaper).
    texture_tiled: bool,
    /// The last blur made of the picture (shown while a newer one is made), and the newest asked.
    blurred: Option<(BlurKey, TextureHandle)>,
    blur_asked: Option<BlurKey>,
    blur_slot: Arc<Mutex<Newest<BlurJob>>>,
    blurred_tx: Sender<(BlurKey, ColorImage)>,
    blurred_rx: Receiver<(BlurKey, ColorImage)>,
    /// Why the picture couldn't be shown.
    error: Option<String>,
    load_slot: Arc<Mutex<Newest<LoadJob>>>,
    loaded_tx: Sender<Loaded>,
    loaded_rx: Receiver<Loaded>,
    picker_open: bool,
    picked_tx: Sender<Option<PathBuf>>,
    picked_rx: Receiver<Option<PathBuf>>,
    /// Where the songs were last shown, for the pad in Settings (which hides them).
    songs_area: Rect,
    /// The main window's area and pixels per point, for the same pad: Settings may be a window of
    /// its own, whose shape (and screen) is not the main window's.
    window_area: Rect,
    window_ppp: f32,
    /// Counts the main window's drawn passes (Settings is drawn in them).
    pass: u64,
    nearby: Nearby,
    pad_grab: Option<PadGrab>,
    held: Option<Held>,
    /// The list pages' banners: their loaded pictures, loads and file picker.
    pub(super) banners: Banners,
}

impl Default for WallpaperState {
    fn default() -> Self {
        let (loaded_tx, loaded_rx) = std::sync::mpsc::channel();
        let (picked_tx, picked_rx) = std::sync::mpsc::channel();
        let (blurred_tx, blurred_rx) = std::sync::mpsc::channel();
        Self {
            path: String::new(),
            serial: 0,
            asked_tiled: false,
            picture: None,
            texture: None,
            texture_tiled: false,
            blurred: None,
            blur_asked: None,
            blur_slot: Arc::default(),
            blurred_tx,
            blurred_rx,
            error: None,
            load_slot: Arc::default(),
            loaded_tx,
            loaded_rx,
            picker_open: false,
            picked_tx,
            picked_rx,
            songs_area: Rect::NOTHING,
            window_area: Rect::NOTHING,
            window_ppp: 1.0,
            pass: 0,
            nearby: Nearby::default(),
            pad_grab: None,
            held: None,
            banners: Banners::default(),
        }
    }
}

impl WallpaperState {
    /// The file picker is open.
    pub fn picking(&self) -> bool {
        self.picker_open
    }

    /// The texture to draw and the picture's size: the blur asked for (or, while that's made, the
    /// last one made of this picture), else the sharp picture. The size is always the sharp one's,
    /// so a blur (made smaller) sits exactly where the sharp picture would. A tiled wallpaper
    /// waits for its texture that repeats (the picture is loaded again for it).
    fn shown(&self, w: &Wallpaper) -> Option<(TextureId, Vec2)> {
        let texture = self.texture.as_ref()?;
        let serial = self.picture.as_ref()?.serial;
        let wrap = w.fit == WallpaperFit::Tile;
        if wrap && !self.texture_tiled {
            return None;
        }
        let blurred = self.blurred.as_ref().filter(|(k, _)| w.blur > 0 && k.serial == serial && k.wrap == wrap);
        Some((blurred.map_or(texture.id(), |(_, t)| t.id()), texture.size_vec2()))
    }
}

impl TruffleApp {
    /// Runs every drawn frame (only then does egui know the graphics card's largest texture): takes
    /// a picked file, starts loading a newly chosen picture and takes in a finished one, keeps the
    /// blur and the pictures next to it up to date; the same for the banners.
    pub(super) fn wallpaper_logic(&mut self, ctx: &egui::Context) {
        // The main window's pass (Settings' own window has another shape).
        self.wallpaper.window_area = ctx.content_rect();
        self.wallpaper.window_ppp = ctx.pixels_per_point();
        self.wallpaper.pass += 1;
        self.banner_logic(ctx);
        while let Ok(picked) = self.wallpaper.picked_rx.try_recv() {
            self.wallpaper.picker_open = false;
            if let Some(path) = picked {
                self.prefs.wallpaper.path = path.to_string_lossy().into_owned();
                // Picking the same file again loads it again (it may have been changed meanwhile).
                self.wallpaper.path.clear();
                self.prefs_changed();
            }
        }
        // A tiled picture repeats past its edges and any other keeps its own: its texture is made
        // one way or the other, so a change between them loads the picture again (only the small
        // copy for blurring is kept, not the picture itself).
        let tiled = self.prefs.wallpaper.fit == WallpaperFit::Tile;
        let retile = !self.wallpaper.path.is_empty() && self.wallpaper.asked_tiled != tiled;
        if self.wallpaper.path != self.prefs.wallpaper.path || retile {
            self.load_wallpaper(ctx);
        }
        let max_side = ctx.input(|i| i.max_texture_side);
        while let Ok((serial, result)) = self.wallpaper.loaded_rx.try_recv() {
            if serial != self.wallpaper.serial {
                continue;
            }
            match result {
                // Shrunk to fit already, but a texture too big for the graphics card would stop Truffle.
                Ok(fresh) if fresh.image.width() > max_side || fresh.image.height() > max_side => {
                    self.wallpaper_failed("the picture is too large".to_owned());
                }
                Ok(fresh) => {
                    let w = &mut self.wallpaper;
                    let options = |tiled| if tiled { TILED } else { TEXTURE };
                    // egui lets go of the picture once it's on the graphics card.
                    w.texture = Some(ctx.load_texture("wallpaper", fresh.image, options(w.asked_tiled)));
                    w.texture_tiled = w.asked_tiled;
                    w.picture = Some(Kept { source: fresh.source, serial, fitted_w: fresh.fitted_w });
                    w.blurred = fresh.blurred.map(|(radius, wrap, image)| {
                        (BlurKey { serial, radius, wrap }, ctx.load_texture("wallpaper-blurred", image, options(wrap)))
                    });
                }
                Err(reason) => self.wallpaper_failed(reason),
            }
        }
        self.blur_logic(ctx);
        self.nearby_logic(ctx);
    }

    /// Decodes the chosen picture on its own thread (only the newest asked for: clicking through
    /// pictures never decodes more than one at a time). The picture shown now stays until the new
    /// one is ready, so nothing blinks; with no picture chosen it goes at once.
    fn load_wallpaper(&mut self, ctx: &egui::Context) {
        // An error about the last picture is out of date now.
        self.toasts.retain(|t| !t.text.starts_with(WALLPAPER_ERROR));
        let max_side = u32::try_from(ctx.input(|i| i.max_texture_side)).unwrap_or(u32::MAX);
        let radius = self.prefs.wallpaper.blur.min(Wallpaper::MAX_BLUR);
        let w = &mut self.wallpaper;
        w.path = self.prefs.wallpaper.path.clone();
        w.serial += 1;
        w.asked_tiled = self.prefs.wallpaper.fit == WallpaperFit::Tile;
        w.error = None;
        if w.path.is_empty() {
            w.picture = None;
            w.texture = None;
            w.blurred = None;
            w.blur_asked = None;
            // A load or a blur that hasn't started isn't needed any more.
            forget(&w.load_slot);
            forget(&w.blur_slot);
            return;
        }
        if let Some(folder) = picture_folder(&w.path) {
            w.nearby.last_folder = Some(folder);
        }
        let job = LoadJob { serial: w.serial, path: PathBuf::from(&w.path), max_side, blur: (radius > 0).then_some((radius, w.asked_tiled)) };
        let (tx, ctx) = (w.loaded_tx.clone(), ctx.clone());
        let asked = ask_newest(&w.load_slot, job, "truffle-wallpaper", move |job| {
            let _ = tx.send((job.serial, load(&job)));
            ctx.request_repaint();
        });
        if let Err(e) = asked {
            log::warn!("could not start loading the wallpaper: {e}");
            self.wallpaper_failed("Truffle couldn't start loading it".to_owned());
        }
    }

    /// Lets go of the picture shown and says why the chosen one can't be shown: in the Picture
    /// row's note, and in the status line unless Settings (with that row) is open.
    fn wallpaper_failed(&mut self, reason: String) {
        log::warn!("could not load the wallpaper {}: {reason}", self.wallpaper.path);
        if !self.settings_open {
            self.toast(format!("{WALLPAPER_ERROR}: {reason}. Choose another in Settings > Appearance."), true);
        }
        let w = &mut self.wallpaper;
        w.picture = None;
        w.texture = None;
        w.blurred = None;
        w.blur_asked = None;
        w.error = Some(reason);
    }

    /// Takes in finished blurs and asks for the one the settings want, when it isn't made or asked
    /// for already. Made on its own thread from the picture's copy for blurring. With no blur, the
    /// last one made isn't kept.
    fn blur_logic(&mut self, ctx: &egui::Context) {
        let w = &mut self.wallpaper;
        while let Ok((key, image)) = w.blurred_rx.try_recv() {
            // Any blur of this picture will do while the one asked for is made (it's nearer than
            // none), but never in place of the one asked for.
            let have_asked = w.blurred.as_ref().is_some_and(|(k, _)| Some(*k) == w.blur_asked);
            if w.picture.as_ref().is_some_and(|p| p.serial == key.serial) && !have_asked {
                let options = if key.wrap { TILED } else { TEXTURE };
                w.blurred = Some((key, ctx.load_texture("wallpaper-blurred", image, options)));
            }
        }
        let radius = self.prefs.wallpaper.blur.min(Wallpaper::MAX_BLUR);
        if radius == 0 {
            if w.blurred.is_some() || w.blur_asked.is_some() {
                w.blurred = None;
                w.blur_asked = None;
                forget(&w.blur_slot);
            }
            return;
        }
        let Some(picture) = w.picture.as_ref() else { return };
        let want = BlurKey { serial: picture.serial, radius, wrap: self.prefs.wallpaper.fit == WallpaperFit::Tile };
        if w.blur_asked == Some(want) {
            return;
        }
        w.blur_asked = Some(want);
        if w.blurred.as_ref().is_some_and(|(k, _)| *k == want) {
            // Back to the blur made last (the slider went back): one asked for meanwhile isn't needed.
            forget(&w.blur_slot);
            return;
        }
        let job = BlurJob { key: want, picture: Arc::clone(&picture.source), fitted_w: picture.fitted_w };
        let (tx, ctx) = (w.blurred_tx.clone(), ctx.clone());
        let asked = ask_newest(&w.blur_slot, job, "truffle-wallpaper-blur", move |job| {
            let image = blurred(&job.picture, job.fitted_w, job.key.radius, job.key.wrap);
            let _ = tx.send((job.key, image));
            ctx.request_repaint();
        });
        if let Err(e) = asked {
            // Shown sharp; asked again when the blur changes.
            log::warn!("could not start blurring the wallpaper: {e}");
        }
    }

    /// Opens the native file picker on a background thread (it blocks until closed).
    pub(super) fn pick_wallpaper(&mut self, ctx: &egui::Context) {
        if self.wallpaper.picker_open {
            return;
        }
        self.wallpaper.picker_open = true;
        let tx = self.wallpaper.picked_tx.clone();
        let ctx = ctx.clone();
        let folder = picture_folder(&self.prefs.wallpaper.path).or_else(|| self.wallpaper.nearby.last_folder.clone());
        let spawned = std::thread::Builder::new().name("truffle-file-dialog".into()).spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Choose a wallpaper").add_filter("Images", &PICTURE_KINDS);
            // Start in the folder of the picture shown now (or the last one).
            if let Some(folder) = folder.filter(|f| f.is_dir()) {
                dialog = dialog.set_directory(folder);
            }
            let _ = tx.send(dialog.pick_file());
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            log::warn!("could not open the file picker: {e}");
            self.wallpaper.picker_open = false;
            self.toast("Could not open the file picker.", true);
        }
    }

    /// The Picture row's note: why the picture couldn't be shown, if it couldn't.
    pub(super) fn wallpaper_error_note(&self) -> Option<String> {
        let reason = self.wallpaper.error.as_ref().filter(|_| !self.prefs.wallpaper.path.is_empty())?;
        Some(format!("Couldn't be shown: {reason}"))
    }

    /// At the start of a panel's contents: the wallpaper, when it shows behind the whole window.
    pub(super) fn wallpaper_behind_panel(&self, ui: &Ui) {
        if self.prefs.wallpaper.whole_window {
            self.paint_wallpaper(ui, ui.ctx().content_rect());
        }
    }

    /// At the start of the songs panel's contents: the wallpaper, placed in that panel or across
    /// the whole window.
    pub(super) fn wallpaper_behind_songs(&mut self, ui: &Ui) {
        self.wallpaper.songs_area = ui.max_rect();
        let area = if self.prefs.wallpaper.whole_window { ui.ctx().content_rect() } else { ui.max_rect() };
        self.paint_wallpaper(ui, area);
    }

    /// Paints the picture laid out in `area`, only the part inside the panel; the panel's contents
    /// draw on top.
    fn paint_wallpaper(&self, ui: &Ui, area: Rect) {
        let w = &self.prefs.wallpaper;
        let Some((texture, size)) = self.wallpaper.shown(w) else { return };
        // At its own size (tile, actual), one of its pixels is one of the screen's.
        let own = 1.0 / ui.ctx().pixels_per_point();
        if let Some(mesh) = wallpaper_mesh(texture, size, area, ui.clip_rect(), w, own, strength(w)) {
            ui.painter().add(mesh);
        }
    }

    /// The area the wallpaper is laid out in (the songs, or the main window), for the pad.
    fn pad_area(&self, ctx: &egui::Context) -> Rect {
        let w = &self.prefs.wallpaper;
        // The main window's shape, also while Settings is a window of its own beside it.
        let window = Some(self.wallpaper.window_area).filter(|r| r.is_positive()).unwrap_or_else(|| ctx.content_rect());
        if w.whole_window || !self.wallpaper.songs_area.is_positive() { window } else { self.wallpaper.songs_area }
    }

    /// How wide the Position row's pad is: the area's shape at the pad's height, within limits
    /// (and within `room`, on a narrow Settings window).
    pub(super) fn pad_width(&self, ctx: &egui::Context, room: f32) -> f32 {
        let area = self.pad_area(ctx);
        (PAD_H * area.width() / area.height().max(1.0)).clamp(PAD_MIN_W, PAD_MAX_W).min(room.max(PAD_H * 0.6))
    }

    /// The Position row's pad, `width` wide: the songs (or the window) in small, with the
    /// wallpaper exactly where it lies (fit, place, zoom, opacity, soft edges) and faint rows over
    /// it, as in Glint. The picture moves with the mouse while it's held, as far as it has room; a
    /// double-click puts it back. Returns its new place (0 to 1 across and down) when it moved.
    pub(super) fn wallpaper_pad(&mut self, ui: &mut Ui, w: &Wallpaper, width: f32) -> Option<(f32, f32)> {
        let p = self.palette;
        let area = self.pad_area(ui.ctx());
        let (pad, resp) = ui.allocate_exact_size(vec2(width, PAD_H), Sense::click_and_drag());
        // The pad is the area made small: everything in it (the picture at its own size too)
        // shrinks the same.
        let k = pad.width() / area.width().max(1.0);
        let own = k / self.wallpaper.window_ppp.max(0.1);
        let shown = self.wallpaper.shown(w);
        let painter = ui.painter_at(pad);
        painter.rect_filled(pad, 0.0, p.main_bg);
        if let Some((texture, size)) = shown
            && let Some(mesh) = wallpaper_mesh(texture, size, pad, pad.intersect(ui.clip_rect()), w, own, strength(w))
        {
            painter.add(mesh);
        }
        let ink = p.text.gamma_multiply(0.28);
        for (n, part) in [0.55, 0.42, 0.62, 0.36, 0.5, 0.45].into_iter().enumerate() {
            let y = pad.top() + 10.0 + n as f32 * 14.0;
            painter.rect_filled(Rect::from_min_size(pos2(pad.left() + 10.0, y), vec2(7.0, 7.0)), 0.0, ink);
            painter.rect_filled(Rect::from_min_size(pos2(pad.left() + 22.0, y + 1.5), vec2(pad.width() * part, 4.0)), 0.0, ink);
        }
        let dragging = self.wallpaper.pad_grab.is_some_and(|g| !g.let_go);
        let edge = if resp.hovered() || dragging { p.accent_ink } else { p.border };
        ui.painter().rect_stroke(pad, 0.0, Stroke::new(if dragging { 2.0 } else { 1.0 }, edge), StrokeKind::Inside);
        let resp = resp.on_hover_cursor(if dragging { CursorIcon::Grabbing } else { CursorIcon::Grab });
        if resp.double_clicked() {
            self.wallpaper.pad_grab = None;
            let d = Wallpaper::default();
            return Some((d.at_x, d.at_y));
        }
        let room = shown.map(|(_, size)| pad.size() - drawn_size(size, pad.size(), w.fit, w.zoom, own));
        pad_drag(ui, &resp, &mut self.wallpaper.pad_grab, w, room)
    }

    /// The Nearby row's pictures: the ones next to the wallpaper, newest first, as many as fit in
    /// the row (right-aligned); the one in use outlined in the accent. They're read (in the
    /// background) once the row shows. Returns the one clicked.
    pub(super) fn nearby_thumbs(&mut self, ui: &mut Ui) -> Option<PathBuf> {
        let p = self.palette;
        let room = ui.available_width();
        if ui.is_rect_visible(ui.max_rect()) {
            self.wallpaper.nearby.seen = self.wallpaper.pass;
            self.want_nearby(ui.ctx());
        }
        let n = &self.wallpaper.nearby;
        if n.listed && n.pictures.is_empty() {
            ui.label(egui::RichText::new("None in this folder").font(super::theme::regular(11.5)).color(p.text_weak));
            return None;
        }
        let fit = (((room + THUMB_GAP) / (THUMB_SIZE.x + THUMB_GAP)).floor().max(0.0) as usize).min(n.pictures.len());
        if fit == 0 {
            return None;
        }
        let strip = vec2(fit as f32 * THUMB_SIZE.x + (fit as f32 - 1.0) * THUMB_GAP, THUMB_SIZE.y);
        let (rect, _) = ui.allocate_exact_size(strip, Sense::hover());
        let mut picked = None;
        for (i, (path, texture)) in n.pictures.iter().take(fit).enumerate() {
            let at = Rect::from_min_size(rect.min + vec2(i as f32 * (THUMB_SIZE.x + THUMB_GAP), 0.0), THUMB_SIZE);
            let resp = ui.interact(at, Id::new(("wallpaper-nearby", i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            let painter = ui.painter();
            painter.rect_filled(at, 0.0, p.field_bg);
            if let Some(texture) = texture {
                painter.image(texture.id(), at, cover_uv(texture.size_vec2(), at.size()), Color32::WHITE);
            }
            let in_use = same_path(path, &self.prefs.wallpaper.path);
            if in_use {
                painter.rect_stroke(at.expand(1.0), 0.0, Stroke::new(2.0, p.accent_ink), StrokeKind::Inside);
            } else {
                painter.rect_stroke(at, 0.0, Stroke::new(1.0, if resp.hovered() { p.text } else { p.border }), StrokeKind::Inside);
            }
            let name = path.file_name().map_or_else(|| path.to_string_lossy(), |n| n.to_string_lossy());
            if resp.on_hover_text(name).clicked() && !in_use {
                picked = Some(path.clone());
            }
        }
        picked
    }

    /// Reads the pictures next to the wallpaper small (in its folder, or with none the last
    /// picture's), on their own thread, unless they're read (or being read) already.
    fn want_nearby(&mut self, ctx: &egui::Context) {
        let n = &mut self.wallpaper.nearby;
        let Some(folder) = picture_folder(&self.prefs.wallpaper.path).or_else(|| n.last_folder.clone()) else { return };
        let max_side = u32::try_from(ctx.input(|i| i.max_texture_side)).unwrap_or(u32::MAX);
        n.want(folder, ctx, max_side);
    }

    /// Whether the Nearby row has a folder to show: the picture's, or after NONE the last one's.
    pub(super) fn nearby_folder_known(&self) -> bool {
        picture_folder(&self.prefs.wallpaper.path).is_some() || self.wallpaper.nearby.last_folder.is_some()
    }

    /// Takes in the pictures next to the wallpaper as they're read. A read stops once the card is
    /// out of view (it starts again when it's back), and they're let go of when Settings closes.
    fn nearby_logic(&mut self, ctx: &egui::Context) {
        let n = &mut self.wallpaper.nearby;
        n.take(ctx);
        n.settle(self.settings_open, self.wallpaper.pass);
    }

    /// After the Wallpaper card's rows, before their change is kept: the wallpaper as it was when
    /// a slider (`line`, its line's id) or the pad was grabbed, for Esc while it's held. The pad
    /// isn't held while it isn't shown (`pad_shown`).
    pub(super) fn wallpaper_held(&mut self, line: Option<Id>, pad_shown: bool) {
        let w = &mut self.wallpaper;
        if !pad_shown {
            w.pad_grab = None;
        }
        if line.is_none() && w.pad_grab.is_none() {
            w.held = None;
            return;
        }
        let pass = w.pass;
        w.held.get_or_insert_with(|| Held { before: self.prefs.wallpaper.clone(), line, pass }).pass = pass;
    }

    /// Esc while a wallpaper slider or the pad is held: the wallpaper as it was before it was
    /// grabbed, and the slider or pad lets go until the button is released (as in Glint). False
    /// when none is held: Esc is for something else then.
    pub(super) fn wallpaper_esc(&mut self, ctx: &egui::Context) -> bool {
        // Held in the last pass, and the button still down: a hold from before the card went (or
        // Settings closed) is over.
        let (down, pass) = (ctx.input(|i| i.pointer.any_down()), self.wallpaper.pass);
        let Some(held) = self.wallpaper.held.take().filter(|h| down && pass.saturating_sub(h.pass) <= 1) else { return false };
        if let Some(line) = held.line {
            fin::let_go(ctx, line);
        }
        if let Some(grab) = self.wallpaper.pad_grab.as_mut() {
            grab.let_go = true;
        }
        if held.before != self.prefs.wallpaper {
            self.prefs.wallpaper = held.before;
            self.prefs_changed();
            ctx.request_repaint();
        }
        true
    }
}

impl Nearby {
    /// Reads the pictures in `folder` small, on their own thread, unless they're read (or being
    /// read) already.
    fn want(&mut self, folder: PathBuf, ctx: &egui::Context, max_side: u32) {
        if self.folder.as_ref() == Some(&folder) {
            return;
        }
        self.serial += 1;
        self.wanted.store(self.serial, Ordering::Relaxed);
        self.folder = Some(folder.clone());
        self.listed = false;
        self.done = false;
        self.pictures.clear();
        let (tx, serial, wanted, ctx) = (self.tx.clone(), self.serial, Arc::clone(&self.wanted), ctx.clone());
        let spawned = std::thread::Builder::new().name("truffle-nearby".into()).spawn(move || {
            let list = nearby_pictures(&folder, MAX_THUMBS);
            let _ = tx.send(NearbyMsg::Listed(serial, list.clone()));
            ctx.request_repaint();
            for (i, path) in list.iter().enumerate() {
                // Settings closed, the card went out of view, or another folder is wanted now.
                if wanted.load(Ordering::Relaxed) != serial {
                    return;
                }
                match decode(path, THUMB_CAP, max_side) {
                    Ok(image) => {
                        let _ = tx.send(NearbyMsg::Thumb(serial, i, image));
                        ctx.request_repaint();
                    }
                    Err(reason) => log::debug!("no small picture of {}: {reason}", path.display()),
                }
            }
            let _ = tx.send(NearbyMsg::Done(serial));
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            log::warn!("could not start reading the pictures next to the wallpaper: {e}");
            self.listed = true;
            self.done = true;
        }
    }

    /// Takes in what the thread sent, for the read wanted now.
    fn take(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                NearbyMsg::Listed(serial, list) if serial == self.serial => {
                    self.pictures = list.into_iter().map(|p| (p, None)).collect();
                    self.listed = true;
                }
                NearbyMsg::Thumb(serial, i, image) if serial == self.serial => {
                    if let Some(slot) = self.pictures.get_mut(i) {
                        slot.1 = Some(ctx.load_texture("wallpaper-nearby", image, TEXTURE));
                    }
                }
                NearbyMsg::Done(serial) if serial == self.serial => self.done = true,
                _ => {}
            }
        }
    }

    /// Stops a read not done yet once the row was out of view for over two passes (`pass` is
    /// now; it's read again when it's back), and lets go of the pictures once Settings closes. A
    /// read that's done stays, also with a picture in it that couldn't be read. A folder still
    /// being listed is left to it (the listing can't be stopped, and one slow to answer, such as
    /// a network folder that's away, would only get another).
    fn settle(&mut self, settings_open: bool, pass: u64) {
        let out_of_view = pass.saturating_sub(self.seen) > 2;
        let reading = self.listed && !self.done;
        let stop = if settings_open { self.folder.is_some() && out_of_view && reading } else { self.folder.is_some() || self.listed };
        if stop {
            self.serial += 1;
            self.wanted.store(self.serial, Ordering::Relaxed);
            self.folder = None;
        }
        if !settings_open {
            self.listed = false;
            self.pictures.clear();
        }
    }
}

/// The pad's mouse: the picture is grabbed on the press, where it is (`w`'s place, with `room` to
/// move in the pad; None while no picture shows), moves with the mouse from the first point while
/// the button is held, and is let go of on release (or by Esc, until then). Returns its new place
/// (0 to 1 across and down) when that changed. (Glint's.)
fn pad_drag(ui: &Ui, resp: &egui::Response, grab: &mut Option<PadGrab>, w: &Wallpaper, room: Option<Vec2>) -> Option<(f32, f32)> {
    if !resp.is_pointer_button_down_on() {
        // Let go of: the last move already counted.
        *grab = None;
        return None;
    }
    let place = (percent(w.at_x), percent(w.at_y));
    if grab.is_none()
        && let Some(room) = room
    {
        let from = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos()).unwrap_or(resp.rect.center());
        *grab = Some(PadGrab { from, place, room, let_go: false });
    }
    let held = grab.filter(|g| !g.let_go)?;
    let at = resp.interact_pointer_pos()?;
    let d = Wallpaper::default();
    let moved = pan_place(held.place, (at - held.from).into(), held.room.into(), (percent(d.at_x), percent(d.at_y)));
    (moved != place).then_some((moved.0 / 100.0, moved.1 / 100.0))
}

/// The folder of the picture at `path`, when it names one.
fn picture_folder(path: &str) -> Option<PathBuf> {
    Path::new(path).parent().filter(|f| !f.as_os_str().is_empty()).map(Path::to_path_buf)
}

/// How strongly the picture shows, 0 to 1.
fn strength(w: &Wallpaper) -> f32 {
    f32::from(w.strength.min(100)) / 100.0
}

/// How much of the area's width and height the picture fades out over at each edge, 0 to 0.5.
fn soft_edges(w: &Wallpaper) -> f32 {
    f32::from(w.soft_edges.min(Wallpaper::MAX_SOFT_EDGES)) / 100.0
}

/// How big the picture is drawn in an area `area` big, as in Glint (list.rs
/// `wallpaper_drawn_size`): filling it (cut to fit), all of it, pulled to its exact size, or the
/// picture's own size (`own` points for each of its pixels) for tile and actual; then the zoom.
fn drawn_size(size: Vec2, area: Vec2, fit: WallpaperFit, zoom: i16, own: f32) -> Vec2 {
    let z = zoom_factor(zoom);
    let k = match fit {
        WallpaperFit::Stretch => return area * z,
        WallpaperFit::Fill => (area.x / size.x).max(area.y / size.y),
        WallpaperFit::Whole => (area.x / size.x).min(area.y / size.y),
        WallpaperFit::Tile | WallpaperFit::Actual => own,
    };
    size * (k * z)
}

/// Where the picture goes, as in Glint (list.rs `wallpaper_layout`): drawn as big as its fit and
/// zoom say, then that side of it against that side of the area (`at_x` 0 the left, 1 the right;
/// `at_y` 0 the top, 1 the bottom). Whatever spills over is cut off when painting; a tiled
/// picture repeats all over the area from there.
fn place(size: Vec2, area: Rect, w: &Wallpaper, own: f32) -> Rect {
    if !(size.x > 0.0 && size.y > 0.0) {
        return Rect::NOTHING;
    }
    let shown = drawn_size(size, area.size(), w.fit, w.zoom, own);
    let x = area.left() + (area.width() - shown.x) * w.at_x;
    let y = area.top() + (area.height() - shown.y) * w.at_y;
    Rect::from_min_size(pos2(x, y), shown)
}

/// The wallpaper drawn in `area` as Glint draws it: placed (see [`place`]), once or tiled all over
/// the area, `strength` strong, fading out at the area's edges over its soft edges (the corners
/// fade both ways). Only the part inside `clip` is made. None when none of it would show.
fn wallpaper_mesh(texture: TextureId, size: Vec2, area: Rect, clip: Rect, w: &Wallpaper, own: f32, strength: f32) -> Option<Mesh> {
    let rect = place(size, area, w, own);
    if !(rect.is_positive() && rect.is_finite() && area.is_positive()) || strength <= 0.0 {
        return None;
    }
    let tiled = w.fit == WallpaperFit::Tile;
    let shown = if tiled { area } else { rect.intersect(area) }.intersect(clip);
    if !shown.is_positive() {
        return None;
    }
    let fade = soft_edges(w);
    let (xs, ys) = (fade_lines(shown.x_range(), area.x_range(), fade), fade_lines(shown.y_range(), area.y_range(), fade));
    if xs.len() < 2 || ys.len() < 2 {
        return None;
    }
    let mut mesh = Mesh::with_texture(texture);
    for &y in &ys {
        for &x in &xs {
            // Past 0 to 1 only when tiled (the texture repeats then).
            let uv = pos2((x - rect.left()) / rect.width(), (y - rect.top()) / rect.height());
            let a = strength * fade_alpha((x - area.left()) / area.width(), fade) * fade_alpha((y - area.top()) / area.height(), fade);
            mesh.vertices.push(Vertex { pos: pos2(x, y), uv, color: Color32::WHITE.gamma_multiply(a) });
        }
    }
    let across = xs.len() as u32;
    for row in 0..ys.len() as u32 - 1 {
        for col in 0..across - 1 {
            let k = row * across + col;
            mesh.add_triangle(k, k + 1, k + across);
            mesh.add_triangle(k + 1, k + across + 1, k + across);
        }
    }
    Some(mesh)
}

/// Where a side of the drawing is cut, from `shown`'s start to its end: where the fade along the
/// area starts and stops, with steps between (nothing in between without soft edges).
fn fade_lines(shown: Rangef, area: Rangef, fade: f32) -> Vec<f32> {
    let mut lines = vec![shown.min, shown.max];
    if fade > 0.0 {
        let band = area.span() * fade;
        for k in 0..=FADE_STEPS {
            let d = band * k as f32 / FADE_STEPS as f32;
            lines.extend([area.min + d, area.max - d]);
        }
    }
    lines.retain(|&v| v >= shown.min && v <= shown.max);
    lines.sort_by(f32::total_cmp);
    lines.dedup_by(|a, b| (*a - *b).abs() < 0.01);
    lines
}

/// How much of the picture shows at `t` (0 to 1) across an area that fades out over `fade` (0 to
/// 0.5) at both ends: nothing at the very edge, all of it from `fade` in (Glint's `fade_stops`).
fn fade_alpha(t: f32, fade: f32) -> f32 {
    if fade <= 0.0 {
        return 1.0;
    }
    let t = t.clamp(0.0, 1.0);
    (t.min(1.0 - t) / fade).min(1.0)
}

/// Glint's zoom: +100 is twice the size, −100 half of it.
fn zoom_factor(zoom: i16) -> f32 {
    let z = f32::from(zoom);
    if z >= 0.0 { 1.0 + z / 100.0 } else { 1.0 / (1.0 - z / 100.0) }
}

/// 0 to 1 as a whole percentage.
fn percent(v: f32) -> f32 {
    (v * 100.0).round().clamp(0.0, 100.0)
}

/// A slider's smallest and biggest.
fn slide_range(s: Slide) -> (f32, f32) {
    let &(_, _, min, max) = SLIDES.iter().find(|x| x.0 == s).unwrap_or(&SLIDES[0]);
    (min, max)
}

/// What slider `s` shows for `w`, a whole number within its ends (across and down 0 to 100%,
/// opacity 0 to 100, zoom -500 to 500...).
pub(super) fn slide_value(s: Slide, w: &Wallpaper) -> f32 {
    let v = match s {
        Slide::Across => percent(w.at_x),
        Slide::Down => percent(w.at_y),
        Slide::Opacity => f32::from(w.strength),
        Slide::Zoom => f32::from(w.zoom),
        Slide::Blur => f32::from(w.blur),
        Slide::SoftEdges => f32::from(w.soft_edges),
    };
    let (min, max) = slide_range(s);
    v.round().clamp(min, max)
}

/// Sets slider `s` of `w` to `v`, kept to its ends and to whole steps (nothing for a value that
/// isn't a number).
pub(super) fn set_slide(s: Slide, w: &mut Wallpaper, v: f32) {
    if !v.is_finite() {
        return;
    }
    let (min, max) = slide_range(s);
    let v = v.round().clamp(min, max);
    match s {
        Slide::Across => w.at_x = v / 100.0,
        Slide::Down => w.at_y = v / 100.0,
        Slide::Opacity => w.strength = v as u8,
        Slide::Zoom => w.zoom = v as i16,
        Slide::Blur => w.blur = v as u8,
        Slide::SoftEdges => w.soft_edges = v as u8,
    }
}

/// What slider `s` is for a new wallpaper (the small tick under it; a double-click goes back).
pub(super) fn slide_default(s: Slide) -> f32 {
    slide_value(s, &Wallpaper::default())
}

/// Slider `s` at `v` from the mouse, on a slider `width` wide: a whole number, or its default
/// `standard` when that's very near, like a notch (within 1% of the slider, or 4 points of it,
/// whichever is less). Only the mouse snaps: a number typed stays as typed.
pub(super) fn notched(s: Slide, v: f32, width: f32, standard: f32) -> f32 {
    let (min, max) = slide_range(s);
    let notch = ((max - min) * 0.01).min(4.0 / width.max(1.0) * (max - min));
    if (v - standard).abs() <= notch { standard } else { v.round().clamp(min, max) }
}

/// A number as Glint writes it: "8", "8.5".
fn number(x: f32) -> String {
    if (x - x.round()).abs() < 0.01 { format!("{}", x.round() as i32) } else { format!("{x:.1}") }
}

/// A slider's number, as its box shows it: "+50" zoomed in, "30%" across and down.
pub(super) fn slide_label(s: Slide, v: f32) -> String {
    match s {
        Slide::Zoom if v > 0.0 => format!("+{}", number(v)),
        Slide::Across | Slide::Down => format!("{}%", number(v)),
        _ => number(v),
    }
}

/// A number typed in slider `s`'s box: "40", "-50", "+20", "30%", " 7.6 " (rounded). None while
/// it isn't one yet ("", "-", "abc"). Past either end it's that end.
pub(super) fn typed_number(s: Slide, typed: &str) -> Option<f32> {
    let t = typed.trim().trim_end_matches('%').trim();
    let t = t.strip_prefix('+').unwrap_or(t).replace(',', ".");
    let v: f32 = t.parse().ok().filter(|v: &f32| v.is_finite())?;
    let (min, max) = slide_range(s);
    Some(v.round().clamp(min, max))
}

/// Where square `i` of the 3x3 puts the wallpaper: across and down, 0, 50 or 100.
pub(super) fn grid_place(i: usize) -> (f32, f32) {
    ((i % 3) as f32 * 50.0, (i / 3 % 3) as f32 * 50.0)
}

/// The 3x3 squares of the Position row (`rect` holds them): the one the wallpaper is at filled
/// with the accent. Returns the place (0 to 1 across and down) of the one clicked.
pub(super) fn place_grid(ui: &mut Ui, accent: Color32, rect: Rect, w: &Wallpaper) -> Option<(f32, f32)> {
    let p = crate::ui::theme::palette(ui.ctx());
    let (x, y) = (percent(w.at_x), percent(w.at_y));
    let mut picked = None;
    for i in 0..9 {
        let (col, row) = ((i % 3) as f32, (i / 3) as f32);
        let at = Rect::from_min_size(rect.min + vec2(col, row) * (GRID_CELL + GRID_GAP), vec2(GRID_CELL, GRID_CELL));
        let resp = ui.interact(at, Id::new(("wallpaper-spot", i)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let (gx, gy) = grid_place(i);
        let on = (gx - x).abs() < 0.5 && (gy - y).abs() < 0.5;
        let painter = ui.painter();
        painter.rect_filled(at, CornerRadius::ZERO, if on { accent } else { p.field_bg });
        let edge = if on { p.accent_ink } else if resp.hovered() { p.text } else { p.border };
        painter.rect_stroke(at, CornerRadius::ZERO, Stroke::new(1.0, edge), StrokeKind::Inside);
        if resp.clicked() {
            picked = Some((gx / 100.0, gy / 100.0));
        }
    }
    picked
}

/// The place (0 to 100 across and down) after the picture on the pad was grabbed at `place` and
/// dragged by `delta`: it moves with the mouse, as far as its `room` in the pad goes (negative
/// room: the picture is bigger than the pad, and its other side shows). No room: it stays. Very
/// near the default (`standard`, within 1%) it lands on it, like a notch. (Glint's `pan_place`.)
fn pan_place(place: (f32, f32), delta: (f32, f32), room: (f32, f32), standard: (f32, f32)) -> (f32, f32) {
    let one = |at: f32, d: f32, room: f32, standard: f32| {
        if room.abs() < 1.0 || !d.is_finite() {
            return at;
        }
        let v = (at + d / room * 100.0).clamp(0.0, 100.0);
        if (v - standard).abs() <= 1.0 { standard } else { v.round() }
    };
    (one(place.0, delta.0, room.0, standard.0), one(place.1, delta.1, room.1, standard.1))
}

/// The part of a picture `pic` big (as 0 to 1 across and down) that fills `dest` big without being
/// pulled out of shape: its middle, cut to `dest`'s shape.
fn cover_uv(pic: Vec2, dest: Vec2) -> Rect {
    let (pw, ph) = (pic.x.max(1.0), pic.y.max(1.0));
    let k = (dest.x / pw).max(dest.y / ph).max(0.0001);
    let (sw, sh) = ((dest.x / k).min(pw) / pw, (dest.y / k).min(ph) / ph);
    Rect::from_min_size(pos2((1.0 - sw) / 2.0, (1.0 - sh) / 2.0), vec2(sw, sh))
}

/// Whether `a` and `b` name the same file (as Windows reads names: any capitals, either slash).
fn same_path(a: &Path, b: &str) -> bool {
    let tidy = |t: &str| t.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    !b.is_empty() && tidy(&a.to_string_lossy()) == tidy(b)
}

/// Whether Truffle opens a file with this name as a picture (JPEG, PNG or WebP).
fn is_picture(path: &Path) -> bool {
    path.extension().is_some_and(|x| PICTURE_KINDS.contains(&x.to_string_lossy().to_lowercase().as_str()))
}

/// The pictures in `dir` (not in its folders), newest first, at most `most`.
fn nearby_pictures(dir: &Path, most: usize) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| is_picture(p))
        .map(|p| (std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().take(most).map(|(_, p)| p).collect()
}

/// A blur of `radius` pixels of the sharp wallpaper (`full` wide), for a picture `made` wide: as
/// much, in that picture's pixels (at least 1 when there is any blur). (Glint's `blur_at`.)
fn blur_at(radius: u8, made: u32, full: u32) -> usize {
    if radius == 0 {
        return 0;
    }
    ((f64::from(radius) * f64::from(made) / f64::from(full.max(1))).round() as usize).max(1)
}

/// Loads a wallpaper on its thread: the picture kept at most 4K, its copy for blurring, and the
/// blur asked for with it.
fn load(job: &LoadJob) -> Result<Fresh, String> {
    let (image, [w, h]) = decode_sized(&job.path, WALLPAPER_CAP, job.max_side)?;
    let fitted_w = kept_size(w, h, WALLPAPER_CAP, u32::MAX).0;
    let image = Arc::new(image);
    // A picture small enough already is its own copy (nothing more is kept).
    let source = blur_source(&image).map_or_else(|| Arc::clone(&image), Arc::new);
    let blurred = job.blur.map(|(radius, wrap)| (radius, wrap, blurred(&source, fitted_w, radius, wrap)));
    Ok(Fresh { image, source, fitted_w, blurred })
}

/// The picture's copy for blurring: at half its size at most, as Glint blurs it. None when it's
/// that small already.
fn blur_source(picture: &ColorImage) -> Option<ColorImage> {
    let (w, h) = (picture.width() as u32, picture.height() as u32);
    let (bw, bh) = kept_size(w.max(1), h.max(1), BLUR_CAP, u32::MAX);
    if (bw, bh) == (w, h) || w == 0 || h == 0 {
        return None;
    }
    let sharp: Vec<u8> = picture.pixels.iter().flat_map(|c| c.to_array()).collect();
    let sharp = image::RgbaImage::from_raw(w, h, sharp)?;
    // Averages the pixels each one covers (they're premultiplied, so that's right as it is).
    let small = image::imageops::thumbnail(&sharp, bw, bh);
    Some(ColorImage::from_rgba_premultiplied([bw as usize, bh as usize], small.as_raw()))
}

/// The picture's copy for blurring ([`blur_source`]) blurred `radius` of the sharp wallpaper's
/// pixels (it is `fitted_w` wide), as in Glint; `wrap`: its edges blur into the opposite ones.
fn blurred(source: &ColorImage, fitted_w: u32, radius: u8, wrap: bool) -> ColorImage {
    let (w, h) = (source.width(), source.height());
    if w == 0 || h == 0 {
        return source.clone();
    }
    let mut pixels: Vec<u8> = source.pixels.iter().flat_map(|c| c.to_array()).collect();
    blur_pixels(&mut pixels, w, h, blur_at(radius, w as u32, fitted_w), wrap);
    ColorImage::from_rgba_premultiplied([w, h], &pixels)
}

/// Blurs premultiplied RGBA pixels (`width` x `height`, top-down) by `radius` pixels: three box
/// blurs across and down, which come close to a soft (Gaussian) blur. Past the picture's edges its
/// edge pixels go on (so the edges don't darken) - or, with `wrap`, the opposite edge's pixels come
/// next. Nothing happens for radius 0 or no pixels. (Glint's `blur_pixels`.)
///
/// The passes keep 8 more bits than a pixel has, and the end result is dithered a little, so a big
/// blur fades smoothly instead of in steps.
fn blur_pixels(pixels: &mut [u8], width: usize, height: usize, radius: usize, wrap: bool) {
    if radius == 0 || width == 0 || height == 0 || pixels.len() != width * height * 4 {
        return;
    }
    let mut fine: Vec<u16> = pixels.iter().map(|&b| u16::from(b) << 8).collect();
    let mut line: Vec<u16> = Vec::with_capacity(width.max(height) * 4);
    for _ in 0..3 {
        for y in 0..height {
            line.clear();
            line.extend_from_slice(&fine[y * width * 4..(y + 1) * width * 4]);
            box_line(&line, width, radius, wrap, |x, px| fine[(y * width + x) * 4..][..4].copy_from_slice(&px));
        }
        for x in 0..width {
            line.clear();
            for y in 0..height {
                line.extend_from_slice(&fine[(y * width + x) * 4..][..4]);
            }
            box_line(&line, height, radius, wrap, |y, px| fine[(y * width + x) * 4..][..4].copy_from_slice(&px));
        }
    }
    // Back to 8 bits, with a 4 x 4 ordered dither; a colour never ends above its see-through
    // (the pixels stay premultiplied).
    const BAYER: [u16; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
    for y in 0..height {
        for x in 0..width {
            let d = u32::from(BAYER[(y % 4) * 4 + x % 4] * 16 + 8);
            let at = (y * width + x) * 4;
            let a = ((u32::from(fine[at + 3]) + d) >> 8).min(255) as u8;
            pixels[at + 3] = a;
            for c in 0..3 {
                pixels[at + c] = (((u32::from(fine[at + c]) + d) >> 8).min(255) as u8).min(a);
            }
        }
    }
}

/// One box blur along a line of `n` pixels (4 values each): each pixel the average of those within
/// `radius` of it, the end pixels going on past the ends (with `wrap`: the line going round, its
/// other end coming next). `put` takes each pixel made.
fn box_line(line: &[u16], n: usize, radius: usize, wrap: bool, mut put: impl FnMut(usize, [u16; 4])) {
    let at = |i: isize| if wrap { i.rem_euclid(n as isize) as usize * 4 } else { i.clamp(0, n as isize - 1) as usize * 4 };
    let r = radius as isize;
    let count = (2 * radius + 1) as u32;
    let mut sum = [0u32; 4];
    for k in -r..=r {
        let p = at(k);
        for (c, s) in sum.iter_mut().enumerate() {
            *s += u32::from(line[p + c]);
        }
    }
    for i in 0..n as isize {
        put(i as usize, std::array::from_fn(|c| ((sum[c] + count / 2) / count) as u16));
        let (add, take) = (at(i + r + 1), at(i - r));
        for (c, s) in sum.iter_mut().enumerate() {
            *s = *s + u32::from(line[add + c]) - u32::from(line[take + c]);
        }
    }
}

/// The size a `w` x `h` picture is kept at: at most `cap`'s width and pixels, with no side longer
/// than `max_side` (the graphics card's largest texture), the same shape.
fn kept_size(w: u32, h: u32, cap: Cap, max_side: u32) -> (u32, u32) {
    let pixels = u64::from(w) * u64::from(h);
    let k = (f64::from(cap.width.min(max_side)) / f64::from(w))
        .min(f64::from(max_side) / f64::from(h))
        .min((cap.pixels as f64 / pixels as f64).sqrt());
    if k >= 1.0 {
        return (w, h);
    }
    // A hair over before rounding down, so 3840 doesn't come out as 3839.
    let shrink = |n: u32| ((f64::from(n) * k + 1e-6).floor() as u32).max(1);
    (shrink(w), shrink(h))
}

/// Opens the picture on the loading thread, kept within `cap`. The error says why it couldn't be
/// opened, in words for a note ("the file is missing").
pub(super) fn decode(path: &Path, cap: Cap, max_side: u32) -> Result<ColorImage, String> {
    decode_sized(path, cap, max_side).map(|(image, _)| image)
}

/// [`decode`], and the picture's own size (the right way up).
fn decode_sized(path: &Path, cap: Cap, max_side: u32) -> Result<(ColorImage, [u32; 2]), String> {
    let reader = ImageReader::open(path).and_then(ImageReader::with_guessed_format).map_err(|e| io_reason(&e))?;
    decode_from(reader, cap, max_side)
}

/// Decodes, shrinks the picture to the kept size and turns it the way the camera was held.
fn decode_from<R: BufRead + Seek>(reader: ImageReader<R>, cap: Cap, max_side: u32) -> Result<(ColorImage, [u32; 2]), String> {
    let mut decoder = reader.into_decoder().map_err(|e| reason(&e))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let picture = DynamicImage::from_decoder(decoder).map_err(|e| reason(&e))?;
    if picture.width() == 0 || picture.height() == 0 {
        return Err("the picture is empty".to_owned());
    }
    let (own_w, own_h) = upright((picture.width(), picture.height()), orientation);
    let rgba = kept_picture(picture, orientation, cap, max_side).into_rgba8();
    let image = ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    Ok((image, [own_w, own_h]))
}

/// A picture stored `size` big, as shown: a quarter turn swaps its sides.
fn upright(size: (u32, u32), orientation: Orientation) -> (u32, u32) {
    use Orientation::{Rotate90, Rotate90FlipH, Rotate270, Rotate270FlipH};
    if matches!(orientation, Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH) { (size.1, size.0) } else { size }
}

/// Shrinks the picture to the kept size, then turns it. In that order a quarter turn, which
/// copies the picture, copies only the small one.
fn kept_picture(mut picture: DynamicImage, orientation: Orientation, cap: Cap, max_side: u32) -> DynamicImage {
    let stored = (picture.width(), picture.height());
    // The kept size is for the picture as shown; turned back, it's the size to shrink to.
    let (shown_w, shown_h) = upright(stored, orientation);
    let (w, h) = upright(kept_size(shown_w, shown_h, cap, max_side), orientation);
    if (w, h) != stored {
        // Averages the pixels each one covers: quick, and needs no buffer bigger than the result.
        picture = picture.thumbnail_exact(w, h);
    }
    picture.apply_orientation(orientation);
    picture
}

/// Why a picture couldn't be opened, for the Picture row's note.
fn reason(e: &ImageError) -> String {
    match e {
        ImageError::IoError(e) => io_reason(e),
        ImageError::Unsupported(_) => "it isn't a JPEG, PNG or WebP picture".to_owned(),
        ImageError::Decoding(_) => "the file is damaged or isn't a picture".to_owned(),
        ImageError::Limits(_) => "the picture is too large".to_owned(),
        _ => e.to_string(),
    }
}

fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "the file is missing".to_owned(),
        std::io::ErrorKind::PermissionDenied => "Windows won't let Truffle read the file".to_owned(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 200 x 100 picture in a 100 x 100 area at the origin (at its own size, a pixel a point).
    fn placed(w: Wallpaper) -> Rect {
        place(vec2(200.0, 100.0), Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)), &w, 1.0)
    }

    fn fit(fit: WallpaperFit) -> Wallpaper {
        Wallpaper { fit, at_x: 0.0, at_y: 0.0, ..Wallpaper::default() }
    }

    #[test]
    fn each_fit_sizes_the_picture_as_glint_does() {
        // Fill covers the area, whole fits inside it, stretch is the area's shape.
        assert_eq!(placed(fit(WallpaperFit::Fill)), Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0)));
        assert_eq!(placed(fit(WallpaperFit::Whole)), Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 50.0)));
        assert_eq!(placed(fit(WallpaperFit::Stretch)), Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)));
        // Tile and actual are its own size: a pixel a point here, half a point on a 2x screen.
        assert_eq!(placed(fit(WallpaperFit::Actual)).size(), vec2(200.0, 100.0));
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
        assert_eq!(place(vec2(200.0, 100.0), area, &fit(WallpaperFit::Tile), 0.5).size(), vec2(100.0, 50.0));
        // The zoom goes on top of each, stretch too.
        let zoomed = |f| placed(Wallpaper { zoom: 100, ..fit(f) }).size();
        assert_eq!(zoomed(WallpaperFit::Whole), vec2(200.0, 100.0));
        assert_eq!(zoomed(WallpaperFit::Stretch), vec2(200.0, 200.0));
        assert_eq!(zoomed(WallpaperFit::Actual), vec2(400.0, 200.0));
        // A picture with no size goes nowhere.
        assert_eq!(place(Vec2::ZERO, area, &Wallpaper::default(), 1.0), Rect::NOTHING);
    }

    #[test]
    fn the_picture_sits_at_its_side() {
        // Filling: the spare width is to the left (right side in view), or split for the middle.
        assert_eq!(placed(Wallpaper::default()).min, pos2(-100.0, 0.0));
        assert_eq!(placed(Wallpaper { at_x: 0.5, ..Wallpaper::default() }).min, pos2(-50.0, 0.0));
        // Whole: the spare height is above it at the bottom, below it at the top.
        let whole = Wallpaper { fit: WallpaperFit::Whole, ..Wallpaper::default() };
        assert_eq!(placed(Wallpaper { at_y: 1.0, ..whole.clone() }).min, pos2(0.0, 50.0));
        assert_eq!(placed(Wallpaper { at_x: 0.0, at_y: 0.0, ..whole.clone() }).min, pos2(0.0, 0.0));
        assert_eq!(placed(Wallpaper { at_x: 1.0, at_y: 0.5, ..whole }).min, pos2(0.0, 25.0));
        // Actual, smaller than the area: in the middle, or against the bottom right.
        let small = |at| place(vec2(40.0, 20.0), Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)), &Wallpaper { fit: WallpaperFit::Actual, at_x: at, at_y: at, ..Wallpaper::default() }, 1.0);
        assert_eq!(small(0.5).min, pos2(30.0, 40.0));
        assert_eq!(small(1.0).max, pos2(100.0, 100.0));
        // The area's own corner counts.
        let moved = place(vec2(200.0, 100.0), Rect::from_min_size(pos2(10.0, 20.0), vec2(100.0, 100.0)), &Wallpaper::default(), 1.0);
        assert_eq!(moved.min, pos2(-90.0, 20.0));
    }

    #[test]
    fn zoom_grows_and_shrinks_the_picture() {
        let zoomed = |zoom| placed(Wallpaper { zoom, at_x: 0.0, ..Wallpaper::default() }).size();
        assert_eq!(zoomed(100), vec2(400.0, 200.0));
        assert_eq!(zoomed(50), vec2(300.0, 150.0));
        assert_eq!(zoomed(-100), vec2(100.0, 50.0));
        // Glint's ends: six times as big, a sixth.
        assert_eq!(zoomed(500), vec2(1200.0, 600.0));
        assert!((zoomed(-500).x - 200.0 / 6.0).abs() < 0.001);
        // Zoomed out at the bottom, the spare room is above the picture.
        let out = placed(Wallpaper { zoom: -100, at_y: 1.0, ..Wallpaper::default() });
        assert_eq!(out.min, pos2(0.0, 50.0));
    }

    /// The (position, uv, alpha) of each corner of a mesh's grid.
    fn corners(mesh: &Mesh) -> Vec<(Pos2, Pos2, u8)> {
        let first = mesh.vertices.first().unwrap();
        let last = mesh.vertices.last().unwrap();
        vec![(first.pos, first.uv, first.color.a()), (last.pos, last.uv, last.color.a())]
    }

    #[test]
    fn the_drawing_is_cut_to_the_area_and_tiles_repeat() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
        let tex = TextureId::Managed(1);
        // Filling, at the right: the picture's right half shows, at full strength.
        let mesh = wallpaper_mesh(tex, vec2(200.0, 100.0), area, area, &Wallpaper::default(), 1.0, 1.0).unwrap();
        assert_eq!(mesh.vertices.len(), 4, "one plain quad without soft edges");
        assert_eq!(corners(&mesh), vec![(pos2(0.0, 0.0), pos2(0.5, 0.0), 255), (pos2(100.0, 100.0), pos2(1.0, 1.0), 255)]);
        // Only the part in the panel is made.
        let half = Rect::from_min_size(pos2(50.0, 0.0), vec2(50.0, 100.0));
        let cut = wallpaper_mesh(tex, vec2(200.0, 100.0), area, half, &Wallpaper::default(), 1.0, 1.0).unwrap();
        assert_eq!(corners(&cut)[0].0, pos2(50.0, 0.0));
        // Tiled at its own size (40 x 20, from the top left): the whole area, the picture
        // repeated 2.5 times across and 5 down.
        let tiled = Wallpaper { fit: WallpaperFit::Tile, at_x: 0.0, at_y: 0.0, ..Wallpaper::default() };
        let tiles = wallpaper_mesh(tex, vec2(40.0, 20.0), area, area, &tiled, 1.0, 0.4).unwrap();
        assert_eq!(corners(&tiles), vec![(pos2(0.0, 0.0), pos2(0.0, 0.0), 102), (pos2(100.0, 100.0), pos2(2.5, 5.0), 102)]);
        // Nothing drawn at no strength, or out of the panel.
        assert!(wallpaper_mesh(tex, vec2(200.0, 100.0), area, area, &Wallpaper::default(), 1.0, 0.0).is_none());
        let away = Rect::from_min_size(pos2(300.0, 0.0), vec2(50.0, 50.0));
        assert!(wallpaper_mesh(tex, vec2(200.0, 100.0), area, away, &Wallpaper::default(), 1.0, 1.0).is_none());
    }

    #[test]
    fn soft_edges_fade_the_picture_out_at_the_areas_edges() {
        // Glint's stops: nothing at the edge, all of it from the fade in, both ends alike.
        assert_eq!(fade_alpha(0.0, 0.2), 0.0);
        assert_eq!(fade_alpha(0.1, 0.2), 0.5);
        assert_eq!(fade_alpha(0.2, 0.2), 1.0);
        assert_eq!(fade_alpha(0.5, 0.2), 1.0);
        assert!((fade_alpha(0.95, 0.2) - 0.25).abs() < 1e-6);
        assert_eq!(fade_alpha(1.0, 0.2), 0.0);
        // No soft edges: all of it everywhere; at most half: only the middle shows whole.
        assert_eq!(fade_alpha(0.0, 0.0), 1.0);
        assert_eq!(fade_alpha(0.5, 0.5), 1.0);
        assert_eq!(fade_alpha(0.25, 0.5), 0.5);
        assert_eq!(soft_edges(&Wallpaper { soft_edges: 20, ..Wallpaper::default() }), 0.2);

        // Drawn: each vertex as strong as the fade says there, across times down (the corners
        // fade both ways), whatever the picture's size.
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let w = Wallpaper { soft_edges: 20, fit: WallpaperFit::Stretch, ..Wallpaper::default() };
        for size in [vec2(400.0, 200.0), vec2(3840.0, 2160.0)] {
            let mesh = wallpaper_mesh(TextureId::Managed(1), size, area, area, &w, 1.0, 0.5).unwrap();
            for v in &mesh.vertices {
                let want = 0.5 * fade_alpha(v.pos.x / 200.0, 0.2) * fade_alpha(v.pos.y / 100.0, 0.2);
                assert!((f32::from(v.color.a()) / 255.0 - want).abs() < 0.01, "{:?}", v.pos);
            }
            // The fade starts and stops exactly at 20% in from each edge.
            assert!(mesh.vertices.iter().any(|v| v.pos == pos2(40.0, 20.0) && v.color.a() == 128));
            assert!(mesh.vertices.iter().any(|v| v.pos == pos2(160.0, 80.0)));
        }
    }

    #[test]
    fn the_pad_moves_the_picture_with_the_mouse() {
        // Glint's own cases. 60 x 40 spare room: 30 across is half of it.
        let d = (50.0, 50.0);
        assert_eq!(pan_place((0.0, 0.0), (30.0, 0.0), (60.0, 40.0), d), (50.0, 0.0));
        assert_eq!(pan_place((50.0, 50.0), (-15.0, 10.0), (60.0, 40.0), d), (25.0, 75.0));
        // A picture bigger than the pad moves the other way round (its other side comes in).
        assert_eq!(pan_place((100.0, 0.0), (30.0, 0.0), (-60.0, -40.0), d), (50.0, 0.0));
        // Kept to the ends; no room, or no number: it stays.
        assert_eq!(pan_place((10.0, 90.0), (-1000.0, 1000.0), (60.0, 40.0), d), (0.0, 100.0));
        assert_eq!(pan_place((30.0, 70.0), (25.0, 25.0), (0.4, -0.2), d), (30.0, 70.0));
        assert_eq!(pan_place((30.0, 70.0), (f32::NAN, f32::INFINITY), (60.0, 40.0), d), (30.0, 70.0));
        // Near the default it lands on it; a bit further, a whole number.
        assert_eq!(pan_place((0.0, 0.0), (29.6, 0.0), (60.0, 40.0), d), (50.0, 0.0));
        assert_eq!(pan_place((0.0, 0.0), (28.8, 0.0), (60.0, 40.0), d), (48.0, 0.0));
        // The room is the pad less the picture as drawn there: filling a pad of another shape
        // leaves room only one way.
        let room = vec2(200.0, 100.0) - drawn_size(vec2(400.0, 100.0), vec2(200.0, 100.0), WallpaperFit::Fill, 0, 1.0);
        assert_eq!(room, vec2(-200.0, 0.0));
    }

    #[test]
    fn the_squares_are_the_corners_edges_and_middle() {
        assert_eq!(grid_place(0), (0.0, 0.0));
        assert_eq!(grid_place(2), (100.0, 0.0));
        assert_eq!(grid_place(4), (50.0, 50.0));
        assert_eq!(grid_place(7), (50.0, 100.0));
        assert_eq!(grid_place(8), (100.0, 100.0));
        // A new wallpaper's place (right, top) is square 2, as the sliders show it.
        let d = Wallpaper::default();
        assert_eq!((slide_value(Slide::Across, &d), slide_value(Slide::Down, &d)), grid_place(2));
    }

    #[test]
    fn sliders_read_and_set_their_settings() {
        let mut w = Wallpaper { at_x: 0.333, at_y: 1.0, strength: 15, zoom: -40, ..Wallpaper::default() };
        assert_eq!(slide_value(Slide::Across, &w), 33.0);
        assert_eq!(slide_value(Slide::Down, &w), 100.0);
        assert_eq!(slide_value(Slide::Opacity, &w), 15.0);
        assert_eq!(slide_value(Slide::Zoom, &w), -40.0);
        set_slide(Slide::Across, &mut w, 70.4);
        set_slide(Slide::Zoom, &mut w, 900.0);
        set_slide(Slide::Blur, &mut w, 12.0);
        set_slide(Slide::SoftEdges, &mut w, -3.0);
        set_slide(Slide::Opacity, &mut w, f32::NAN);
        assert_eq!((w.at_x, w.zoom, w.blur, w.soft_edges, w.strength), (0.7, 500, 12, 0, 15));
        // The defaults (the ticks) are a new wallpaper's.
        assert_eq!(SLIDES.map(|(s, ..)| slide_default(s)), [100.0, 0.0, 40.0, 0.0, 0.0, 0.0]);
        // The mouse lands on the default when very near it (1% of the slider, or 4 points).
        assert_eq!(notched(Slide::Opacity, 40.8, 180.0, 40.0), 40.0);
        assert_eq!(notched(Slide::Opacity, 41.6, 180.0, 40.0), 42.0);
        // Zoom's slider is 1000 long: 1% is 10, less than 4 points' 22.
        assert_eq!(notched(Slide::Zoom, 9.9, 180.0, 0.0), 0.0);
        assert_eq!(notched(Slide::Zoom, 12.3, 180.0, 0.0), 12.0);
        // On a long slider 4 points is the less.
        assert_eq!(notched(Slide::Opacity, 41.5, 1000.0, 40.0), 42.0);
        assert_eq!(notched(Slide::Opacity, 40.3, 1000.0, 40.0), 40.0);
        // Their numbers as Glint shows them.
        assert_eq!(slide_label(Slide::Zoom, 50.0), "+50");
        assert_eq!(slide_label(Slide::Zoom, -50.0), "-50");
        assert_eq!(slide_label(Slide::Zoom, 0.0), "0");
        assert_eq!(slide_label(Slide::Across, 30.0), "30%");
        assert_eq!(slide_label(Slide::Opacity, 15.0), "15");
    }

    #[test]
    fn typed_numbers_are_read_as_glint_reads_them() {
        assert_eq!(typed_number(Slide::Opacity, "40"), Some(40.0));
        assert_eq!(typed_number(Slide::Zoom, "-50"), Some(-50.0));
        assert_eq!(typed_number(Slide::Zoom, "+20"), Some(20.0));
        assert_eq!(typed_number(Slide::Across, "30%"), Some(30.0));
        assert_eq!(typed_number(Slide::Blur, " 7.6 "), Some(8.0));
        assert_eq!(typed_number(Slide::SoftEdges, "12,4"), Some(12.0));
        // Past the ends: the end.
        assert_eq!(typed_number(Slide::Blur, "99"), Some(50.0));
        assert_eq!(typed_number(Slide::Zoom, "-9000"), Some(-500.0));
        assert_eq!(typed_number(Slide::Down, "150 %"), Some(100.0));
        // Not a number yet.
        for t in ["", "-", "+", "%", "abc", "inf", "NaN"] {
            assert_eq!(typed_number(Slide::Opacity, t), None, "{t:?}");
        }
    }

    #[test]
    fn a_blur_is_as_much_whatever_size_its_made_at() {
        // Glint's: made at half the sharp wallpaper's width, a 20-pixel blur is 10 of its pixels.
        assert_eq!(blur_at(20, 1920, 3840), 10);
        // A small picture isn't made bigger: made at its own size, the blur stays as asked.
        assert_eq!(blur_at(20, 800, 800), 20);
        // No blur stays none; any blur stays at least 1.
        assert_eq!(blur_at(0, 100, 1000), 0);
        assert_eq!(blur_at(1, 10, 4000), 1);
        // A 4K picture is blurred at half its size (and its 1080p twin at its own), the same look:
        // a hard edge comes out just as soft, as a part of the picture.
        let edge = |w: usize, h: usize| {
            let pixels = (0..w * h).map(|i| if i % w < w / 2 { Color32::BLACK } else { Color32::WHITE }).collect();
            ColorImage::new([w, h], pixels)
        };
        let big = blurred(&blur_source(&edge(3840, 8)).expect("made smaller"), 3840, 20, false);
        assert!(blur_source(&edge(1920, 4)).is_none(), "small enough already");
        let small = blurred(&edge(1920, 4), 3840, 20, false);
        assert_eq!((big.size, small.size), ([1920, 4], [1920, 4]));
        // A 4K picture's copy for blurring is a quarter of it.
        assert_eq!(kept_size(3840, 2160, BLUR_CAP, u32::MAX), (1920, 1080));
        let row = |img: &ColorImage| img.pixels[..img.width()].iter().map(|c| c.r()).collect::<Vec<_>>();
        let (a, b) = (row(&big), row(&small));
        assert!(a.iter().zip(&b).all(|(x, y)| x.abs_diff(*y) <= 2), "the two blurs differ");
        // Three blurs of 10 pixels: soft near the edge, untouched past 30 pixels from it.
        assert!(a[950] > 20 && a[950] < 128 && a[969] > 128 && a[969] < 235, "not soft near the edge: {} {}", a[950], a[969]);
        assert!(a[960 - 40] == 0 && a[960 + 40] == 255, "soft too far out");
    }

    #[test]
    fn a_big_blur_fades_in_small_steps() {
        // A hard edge blurred 30 pixels: across the soft part, no step between neighbours is
        // bigger than a few levels.
        let (w, h) = (200usize, 8usize);
        let mut px = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 30 } else { 230 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        blur_pixels(&mut px, w, h, 30, false);
        let y = 3;
        let biggest = (1..w).map(|x| (i32::from(px[(y * w + x) * 4]) - i32::from(px[(y * w + x - 1) * 4])).abs()).max().unwrap();
        assert!(biggest <= 6, "a step of {biggest}");
        // Opaque stays opaque; the far ends keep their colours (within the dither).
        assert!(px.chunks(4).all(|p| p[3] == 255));
        assert!(i32::from(px[(y * w) * 4]).abs_diff(30) <= 1 && i32::from(px[(y * w + w - 1) * 4]).abs_diff(230) <= 1);
    }

    #[test]
    fn a_tiled_wallpaper_blurs_round_its_edges() {
        // Black on its left half, white on its right: tiled, its copies' left edge meets the right.
        let (w, h) = (16usize, 4usize);
        let mut px = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 0 } else { 255 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut plain = px.clone();
        blur_pixels(&mut px, w, h, 3, true);
        blur_pixels(&mut plain, w, h, 3, false);
        let at = |p: &[u8], x: usize| i32::from(p[x * 4]);
        // Where two copies meet is as soft as the step in the middle of the picture.
        let seam = (at(&px, 0) - at(&px, w - 1)).abs();
        let middle = (at(&px, w / 2) - at(&px, w / 2 - 1)).abs();
        assert!(seam <= middle + 2, "seam {seam}, middle {middle}");
        // Not wrapped, the edges keep their own colours: a hard line where copies meet.
        assert!((at(&plain, 0) - at(&plain, w - 1)).abs() > 200);
    }

    #[test]
    fn nearby_pictures_are_the_folders_pictures_newest_first() {
        let dir = std::env::temp_dir().join(format!("truffle-nearby-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("inside.png")).unwrap();
        let now = std::time::SystemTime::now();
        // Ten pictures, each an hour older than the one before, and files that aren't pictures.
        let names = ["a.jpg", "b.JPEG", "c.png", "d.webp", "e.PNG", "f.jpg", "g.jpg", "h.png", "i.webp", "j.jpg"];
        for (i, name) in names.iter().enumerate() {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_modified(now - std::time::Duration::from_secs(3600 * i as u64)).unwrap();
        }
        for other in ["notes.txt", "song.mp3", "photo.gif", "photo.bmp", "png"] {
            std::fs::write(dir.join(other), b"x").unwrap();
        }
        let found = nearby_pictures(&dir, MAX_THUMBS);
        let found: Vec<String> = found.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(found, names[..8].to_vec(), "newest first, at most 8, only JPEG, PNG and WebP");
        assert!(nearby_pictures(&dir.join("nowhere"), MAX_THUMBS).is_empty());
        // The one in use is found by its name however it's written.
        assert!(same_path(&dir.join("c.png"), &dir.join("C.PNG").to_string_lossy().replace('\\', "/")));
        assert!(!same_path(&dir.join("c.png"), ""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_pictures_are_cut_to_fill_their_box() {
        // A wide picture in a box of another shape: its middle, the full height.
        let uv = cover_uv(vec2(400.0, 100.0), vec2(70.0, 44.0));
        assert!((uv.height() - 1.0).abs() < 1e-6 && (uv.center().x - 0.5).abs() < 1e-6);
        assert!((uv.width() - (70.0 / 44.0 * 100.0) / 400.0).abs() < 1e-4);
        // A tall one: the full width, the middle of its height.
        let uv = cover_uv(vec2(100.0, 400.0), vec2(70.0, 44.0));
        assert!((uv.width() - 1.0).abs() < 1e-6 && (uv.center().y - 0.5).abs() < 1e-6);
    }

    /// A common graphics card's largest texture.
    const GPU_SIDE: u32 = 16_384;

    #[test]
    fn big_pictures_are_kept_at_most_4k() {
        assert_eq!(kept_size(1920, 1080, WALLPAPER_CAP, GPU_SIDE), (1920, 1080));
        assert_eq!(kept_size(7680, 4320, WALLPAPER_CAP, GPU_SIDE), (3840, 2160));
        // A wide panorama by its width, a tall one by its number of pixels.
        assert_eq!(kept_size(8000, 1000, WALLPAPER_CAP, GPU_SIDE), (3840, 480));
        let (w, h) = kept_size(3000, 6000, WALLPAPER_CAP, GPU_SIDE);
        assert!(u64::from(w) * u64::from(h) <= MAX_PIXELS && w <= MAX_WIDTH);
        assert_eq!((w, h), (2036, 4072));
    }

    #[test]
    fn long_pictures_fit_the_graphics_card() {
        // A long scrolling screenshot, and one small enough in pixels to be kept whole otherwise.
        assert_eq!(kept_size(1080, 36_000, WALLPAPER_CAP, GPU_SIDE), (491, 16_384));
        assert_eq!(kept_size(500, 16_500, WALLPAPER_CAP, GPU_SIDE), (496, 16_384));
        // A card with a smaller limit; the width is held to it too.
        assert_eq!(kept_size(1080, 9000, WALLPAPER_CAP, 8192), (983, 8192));
        assert_eq!(kept_size(7680, 4320, WALLPAPER_CAP, 2048), (2048, 1152));
    }

    #[test]
    fn turned_pictures_are_shrunk_before_turning() {
        // Stored lying down, red on the left and blue on the right, taken with the camera turned.
        let (red, blue) = (image::Rgba([255, 0, 0, 255]), image::Rgba([0, 0, 255, 255]));
        let stored = image::RgbaImage::from_fn(400, 100, |x, _| if x < 200 { red } else { blue });
        let kept = kept_picture(DynamicImage::ImageRgba8(stored), Orientation::Rotate90, WALLPAPER_CAP, 200).into_rgba8();
        // Shown standing up, its long side within the limit.
        assert_eq!(kept.dimensions(), (50, 200));
        // Turned clockwise: the stored left side is at the top.
        assert_eq!(*kept.get_pixel(25, 0), red);
        assert_eq!(*kept.get_pixel(25, 199), blue);
        assert_eq!(upright((400, 100), Orientation::Rotate90), (100, 400));
        assert_eq!(upright((400, 100), Orientation::FlipHorizontal), (400, 100));
    }

    #[test]
    fn decodes_and_shrinks_and_explains_failures() {
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4000, 50, image::Rgba([10, 20, 30, 255])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let reader = ImageReader::new(std::io::Cursor::new(png)).with_guessed_format().unwrap();
        let (image, own) = decode_from(reader, WALLPAPER_CAP, GPU_SIDE).unwrap();
        assert_eq!(image.size, [3840, 48]);
        assert_eq!(own, [4000, 50], "its own size too, for the blur");
        assert_eq!(image.pixels[0], Color32::from_rgb(10, 20, 30));

        let garbage = ImageReader::new(std::io::Cursor::new(b"not a picture".to_vec())).with_guessed_format().unwrap();
        assert_eq!(decode_from(garbage, WALLPAPER_CAP, GPU_SIDE).unwrap_err(), "it isn't a JPEG, PNG or WebP picture");
        assert_eq!(decode(Path::new("target/no-such-wallpaper.png"), WALLPAPER_CAP, GPU_SIDE).unwrap_err(), "the file is missing");
    }

    #[test]
    fn a_loaded_wallpaper_keeps_only_a_small_copy_and_comes_with_its_blur() {
        let dir = std::env::temp_dir().join(format!("truffle-load-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (wide, small) = (dir.join("wide.png"), dir.join("small.png"));
        image::RgbaImage::from_pixel(4000, 50, image::Rgba([10, 20, 30, 255])).save(&wide).unwrap();
        image::RgbaImage::from_pixel(300, 200, image::Rgba([10, 20, 30, 255])).save(&small).unwrap();
        let job = |path: &Path, blur| LoadJob { serial: 1, path: path.to_path_buf(), max_side: GPU_SIDE, blur };
        // Shown at most 4K; kept for blurring at half that; blurred straight away when asked.
        let fresh = load(&job(&wide, Some((10, false)))).unwrap();
        assert_eq!((fresh.image.size, fresh.source.size, fresh.fitted_w), ([3840, 48], [1920, 24], 3840));
        let (radius, wrap, blur) = fresh.blurred.expect("blurred with it");
        assert_eq!((radius, wrap, blur.size), (10, false, [1920, 24]));
        // A small picture is its own copy; no blur, none made.
        let fresh = load(&job(&small, None)).unwrap();
        assert!(Arc::ptr_eq(&fresh.image, &fresh.source) && fresh.blurred.is_none());
        assert_eq!(load(&job(&dir.join("gone.png"), None)).err().as_deref(), Some("the file is missing"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_newest_work_is_done_one_at_a_time() {
        use std::sync::atomic::AtomicUsize;
        let slot: Arc<Mutex<Newest<u32>>> = Arc::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let busy = Arc::new(AtomicUsize::new(0));
        // Twenty pictures clicked through quickly, each taking a while.
        for job in 0..20 {
            let (tx, busy) = (tx.clone(), Arc::clone(&busy));
            ask_newest(&slot, job, "truffle-test-newest", move |job| {
                assert_eq!(busy.fetch_add(1, Ordering::SeqCst), 0, "two at once");
                std::thread::sleep(std::time::Duration::from_millis(30));
                busy.fetch_sub(1, Ordering::SeqCst);
                let _ = tx.send(job);
            })
            .unwrap();
        }
        drop(tx);
        // (The list ends once the thread has ended: nothing more was asked.)
        let done: Vec<u32> = rx.iter().collect();
        assert_eq!(done.last(), Some(&19), "the newest is always done: {done:?}");
        assert!(done.len() <= 3, "the ones in between were dropped: {done:?}");
        assert!(!slot.lock().unwrap().running);
    }

    #[test]
    fn a_nearby_read_is_done_also_with_a_picture_it_cant_read() {
        let dir = std::env::temp_dir().join(format!("truffle-nearby-done-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbaImage::from_pixel(40, 20, image::Rgba([200, 0, 0, 255])).save(dir.join("good.png")).unwrap();
        std::fs::write(dir.join("broken.jpg"), b"not a picture").unwrap();
        let ctx = egui::Context::default();
        let mut n = Nearby::default();
        n.want(dir.clone(), &ctx, GPU_SIDE);
        let start = std::time::Instant::now();
        while !n.done && start.elapsed() < std::time::Duration::from_secs(20) {
            n.take(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(n.done, "never done");
        assert_eq!(n.pictures.len(), 2);
        assert_eq!(n.pictures.iter().filter(|(_, t)| t.is_some()).count(), 1, "only the good one shows");
        // Out of view for long with Settings open: what was read stays, and isn't read again.
        let serial = n.serial;
        n.settle(true, n.seen + 10);
        n.want(dir.clone(), &ctx, GPU_SIDE);
        assert_eq!((n.folder.as_ref(), n.serial, n.pictures.len()), (Some(&dir), serial, 2));
        // Settings closed: let go of.
        n.settle(false, n.seen + 11);
        assert!(n.folder.is_none() && n.pictures.is_empty());
        // The folder for Nearby: the picture's (none for a name alone, or no picture).
        assert_eq!(picture_folder(&dir.join("good.png").to_string_lossy()), Some(dir.clone()));
        assert_eq!((picture_folder("good.png"), picture_folder("")), (None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pad_moves_the_picture_from_the_first_point() {
        use super::super::kit::headless::{move_to, pass, press, release};
        let ctx = egui::Context::default();
        let pad = Rect::from_min_size(pos2(100.0, 100.0), vec2(200.0, 100.0));
        // The picture has 20 points to spare across the pad, none down.
        let room = Some(vec2(20.0, 0.0));
        let mut w = Wallpaper { fit: WallpaperFit::Whole, at_x: 0.5, at_y: 0.5, ..Wallpaper::default() };
        let mut grab = None;
        let step = |events, w: &mut Wallpaper, grab: &mut Option<PadGrab>| {
            let mut moved = None;
            pass(&ctx, events, |ui| {
                let resp = ui.interact(pad, Id::new("pad"), Sense::click_and_drag());
                moved = pad_drag(ui, &resp, grab, w, room);
            });
            if let Some((x, y)) = moved {
                (w.at_x, w.at_y) = (x, y);
            }
            moved
        };
        step(move_to(pos2(200.0, 150.0)), &mut w, &mut grab);
        assert_eq!(step(press(pos2(200.0, 150.0)), &mut w, &mut grab), None, "a press alone moves nothing");
        assert!(grab.is_some(), "grabbed on the press");
        // One point is a twentieth of the room: 5% further right, at once.
        assert_eq!(step(move_to(pos2(201.0, 150.0)), &mut w, &mut grab), Some((0.55, 0.5)));
        // Esc lets go of it until the button is released.
        if let Some(g) = grab.as_mut() {
            g.let_go = true;
        }
        assert_eq!(step(move_to(pos2(210.0, 150.0)), &mut w, &mut grab), None);
        step(release(pos2(210.0, 150.0)), &mut w, &mut grab);
        assert!(grab.is_none());
        assert_eq!((w.at_x, w.at_y), (0.55, 0.5));
        // No picture shown: nothing to grab.
        let mut none = None;
        pass(&ctx, press(pos2(200.0, 150.0)), |ui| {
            let resp = ui.interact(pad, Id::new("pad"), Sense::click_and_drag());
            assert_eq!(pad_drag(ui, &resp, &mut none, &w, None), None);
        });
        assert!(none.is_none());
    }
}
