//! Album art / playlist cover / avatar cache.
//!
//! * memory: `(url, pixel size)` -> `TextureHandle`, evicted when not drawn for a minute and the
//!   cache holds more than [`EVICT_THRESHOLD`] entries;
//! * disk: original bytes under `Paths::image_cache_dir()`, file name = SHA-256 of the URL;
//! * network: reqwest on the tokio runtime, at most [`MAX_CONCURRENT`] jobs at once, one download per
//!   URL at a time; decode + downscale on the blocking pool to the size actually displayed.
//!
//! Requests for images that scrolled out of view before their turn are cancelled.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, anyhow};
use egui::load::SizedTexture;
use egui::{Color32, ColorImage, CornerRadius, Rect, TextureHandle, TextureOptions};
use sha2::{Digest, Sha256};

const MAX_CONCURRENT: usize = 6;
const EVICT_THRESHOLD: usize = 400;
const EVICT_AFTER: Duration = Duration::from_secs(60);
/// Above this many entries the least recently drawn are dropped regardless of age.
const HARD_CAP: usize = 1200;
/// A queued request not drawn for this long is dropped (it scrolled away).
const CANCEL_AFTER: Duration = Duration::from_millis(400);
const RETRY_FAILED_AFTER: Duration = Duration::from_secs(120);
const DISK_CACHE_MAX_BYTES: u64 = 300 * 1024 * 1024;
const DISK_CACHE_TARGET_BYTES: u64 = 200 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Texture sizes (pixels). A request is rounded up to the next bucket.
const BUCKETS: [u32; 9] = [32, 48, 64, 96, 128, 192, 256, 384, 640];

enum State {
    Loading(Arc<AtomicBool>),
    Ready { tex: TextureHandle, swatch: Option<Color32> },
    Failed(Instant),
}

/// The vibrant colour of an image, as far as the cache knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swatch {
    /// Not decoded yet (or not requested at any size).
    Pending,
    /// Decoded but greyish, or the image could not be loaded.
    None,
    Color(Color32),
}

struct Entry {
    state: State,
    last_used: Instant,
}

enum Outcome {
    Ready(TextureHandle, Option<Color32>),
    Failed(String),
    Cancelled,
}

struct Loaded {
    key: u64,
    /// The cancel flag of the request, identifying which load this result belongs to.
    token: Arc<AtomicBool>,
    outcome: Outcome,
}

struct Shared {
    client: Option<reqwest::Client>,
    dir: PathBuf,
    permits: tokio::sync::Semaphore,
    url_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub struct ImageCache {
    ctx: egui::Context,
    runtime: tokio::runtime::Handle,
    shared: Arc<Shared>,
    entries: HashMap<u64, Entry>,
    tx: Sender<Loaded>,
    rx: Receiver<Loaded>,
    now: Instant,
}

impl ImageCache {
    pub fn new(ctx: &egui::Context, runtime: tokio::runtime::Handle, dir: PathBuf) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("Truffle/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(MAX_CONCURRENT)
            .build()
            .map_err(|e| log::warn!("image downloads disabled, could not create HTTP client: {e}"))
            .ok();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("could not create image cache folder {}: {e}", dir.display());
        }
        let prune_dir = dir.clone();
        runtime.spawn_blocking(move || prune_disk_cache(&prune_dir));
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            ctx: ctx.clone(),
            runtime,
            shared: Arc::new(Shared {
                client,
                dir,
                permits: tokio::sync::Semaphore::new(MAX_CONCURRENT),
                url_locks: Mutex::new(HashMap::new()),
            }),
            entries: HashMap::new(),
            tx,
            rx,
            now: Instant::now(),
        }
    }

    /// Call at the start of every UI pass.
    pub fn begin_frame(&mut self) {
        self.now = Instant::now();
    }

    /// Applies finished loads. Cheap; call every frame (also while minimized).
    pub fn poll(&mut self) {
        while let Ok(Loaded { key, token, outcome }) = self.rx.try_recv() {
            let Some(entry) = self.entries.get_mut(&key) else { continue };
            let State::Loading(current) = &entry.state else { continue };
            let same_request = Arc::ptr_eq(current, &token);
            match outcome {
                Outcome::Ready(tex, swatch) => entry.state = State::Ready { tex, swatch },
                Outcome::Failed(msg) if same_request => {
                    log::debug!("image load failed: {msg}");
                    entry.state = State::Failed(Instant::now());
                }
                Outcome::Cancelled if same_request => {
                    self.entries.remove(&key);
                }
                // A stale request for the same image failed or was cancelled; the newer one decides.
                Outcome::Failed(_) | Outcome::Cancelled => {}
            }
        }
    }

    /// How many images are cached, and how many of them are ready to draw.
    pub fn stats(&self) -> (usize, usize) {
        (self.entries.len(), self.entries.values().filter(|e| matches!(e.state, State::Ready { .. })).count())
    }

    /// Call at the end of every UI pass: cancels requests that scrolled away and evicts textures
    /// that have not been drawn for a while.
    pub fn end_frame(&mut self) {
        let now = self.now;
        let over = self.entries.len() > EVICT_THRESHOLD;
        self.entries.retain(|_, e| match &e.state {
            State::Loading(cancel) => {
                if now.duration_since(e.last_used) > CANCEL_AFTER {
                    cancel.store(true, Ordering::Relaxed);
                    false
                } else {
                    true
                }
            }
            _ => !over || now.duration_since(e.last_used) <= EVICT_AFTER,
        });
        // Fast scrolling through a huge list can still pile up textures within a minute: keep the
        // most recently drawn ones only.
        if self.entries.len() > HARD_CAP {
            let mut by_age: Vec<(Instant, u64)> = self
                .entries
                .iter()
                .filter(|(_, e)| !matches!(e.state, State::Loading(_)))
                .map(|(k, e)| (e.last_used, *k))
                .collect();
            by_age.sort_unstable_by_key(|(used, _)| *used);
            let excess = self.entries.len() - EVICT_THRESHOLD;
            for (_, key) in by_age.into_iter().take(excess) {
                self.entries.remove(&key);
            }
        }
    }

    /// Returns the texture for `url` sized for `size_pts` logical points (square), starting a load if
    /// needed. `None` while loading or if the image is unavailable.
    pub fn get(&mut self, url: &str, size_pts: f32) -> Option<SizedTexture> {
        if url.is_empty() {
            return None;
        }
        let wanted = (size_pts * self.ctx.pixels_per_point()).ceil().max(1.0) as u32;
        let px = BUCKETS.iter().copied().find(|&b| b >= wanted).unwrap_or(BUCKETS[BUCKETS.len() - 1]);
        let key = key_of(url, px);
        let now = self.now;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = now;
            match &entry.state {
                State::Ready { tex, .. } => return Some(SizedTexture::from_handle(tex)),
                State::Loading(_) => return None,
                State::Failed(at) if now.duration_since(*at) < RETRY_FAILED_AFTER => return None,
                State::Failed(_) => {}
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.entries.insert(key, Entry { state: State::Loading(cancel.clone()), last_used: now });
        let shared = self.shared.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let url = url.to_owned();
        self.runtime.spawn(async move {
            let outcome = match load(&shared, &ctx, &url, px, &cancel).await {
                Ok(Some((tex, swatch))) => Outcome::Ready(tex, swatch),
                Ok(None) => Outcome::Cancelled,
                Err(e) => Outcome::Failed(format!("{url}: {e:#}")),
            };
            let _ = tx.send(Loaded { key, token: cancel, outcome });
            ctx.request_repaint();
        });
        None
    }

    /// Paints the image for `url` into `rect`, or a placeholder while it loads / when missing.
    pub fn paint(&mut self, ui: &egui::Ui, url: Option<&str>, rect: Rect, radius: u8, placeholder: Color32) -> bool {
        if !ui.is_rect_visible(rect) {
            return false;
        }
        let tex = url.and_then(|u| self.get(u, rect.width().max(rect.height())));
        match tex {
            Some(tex) => {
                egui::Image::from_texture(tex).corner_radius(CornerRadius::same(radius)).paint_at(ui, rect);
                true
            }
            None => {
                ui.painter().rect_filled(rect, CornerRadius::same(radius), placeholder);
                false
            }
        }
    }

    /// The vibrant colour of `url`'s image, from whichever size of it was decoded. Never starts a
    /// load: the image has to be drawn somewhere first.
    pub fn swatch(&self, url: &str) -> Swatch {
        if url.is_empty() {
            return Swatch::None;
        }
        let mut pending = false;
        let mut failed = false;
        for px in BUCKETS {
            match self.entries.get(&key_of(url, px)).map(|e| &e.state) {
                Some(State::Ready { swatch: Some(c), .. }) => return Swatch::Color(*c),
                Some(State::Ready { swatch: None, .. }) => return Swatch::None,
                Some(State::Loading(_)) => pending = true,
                Some(State::Failed(_)) => failed = true,
                None => {}
            }
        }
        if failed && !pending { Swatch::None } else { Swatch::Pending }
    }
}

fn key_of(url: &str, px: u32) -> u64 {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    px.hash(&mut h);
    h.finish()
}

fn file_name(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    let mut name = String::with_capacity(36);
    for b in digest.iter().take(16) {
        name.push_str(&format!("{b:02x}"));
    }
    name.push_str(".img");
    name
}

impl Shared {
    fn url_lock(&self, url: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.url_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.entry(url.to_owned()).or_default().clone()
    }

    fn release_url_lock(&self, url: &str, lock: Arc<tokio::sync::Mutex<()>>) {
        let mut locks = self.url_locks.lock().unwrap_or_else(|e| e.into_inner());
        // Ours plus the map's reference: nobody else is waiting for this URL.
        if Arc::strong_count(&lock) <= 2 {
            locks.remove(url);
        }
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let client = self.client.as_ref().ok_or_else(|| anyhow!("no HTTP client"))?;
        let response = client.get(url).send().await.context("request failed")?;
        let response = response.error_for_status().context("server refused the image")?;
        let bytes = response.bytes().await.context("download interrupted")?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(anyhow!("image too large ({} bytes)", bytes.len()));
        }
        Ok(bytes.to_vec())
    }
}

/// Returns `Ok(None)` if cancelled before any work was done.
async fn load(
    shared: &Arc<Shared>,
    ctx: &egui::Context,
    url: &str,
    px: u32,
    cancel: &AtomicBool,
) -> Result<Option<(TextureHandle, Option<Color32>)>> {
    let _permit = shared.permits.acquire().await.context("image loader shut down")?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let path = shared.dir.join(file_name(url));

    let lock = shared.url_lock(url);
    let fetched = {
        let _guard = lock.lock().await;
        fetch_bytes(shared, url, &path, cancel).await
    };
    shared.release_url_lock(url, lock);
    let Some((bytes, from_disk)) = fetched? else { return Ok(None) };

    match decode_off_thread(ctx, url, bytes, px).await {
        Ok(tex) => Ok(Some(tex)),
        Err(e) if from_disk => {
            // Corrupt cache file: drop it and fetch a fresh copy once.
            log::debug!("discarding unreadable cached image {}: {e:#}", path.display());
            let _ = tokio::fs::remove_file(&path).await;
            let bytes = shared.download(url).await?;
            let tex = decode_off_thread(ctx, url, bytes.clone(), px).await?;
            write_cache_file(&path, &bytes).await;
            Ok(Some(tex))
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(e)
        }
    }
}

/// Bytes from the disk cache, or downloaded (and stored). `None` if cancelled.
async fn fetch_bytes(shared: &Shared, url: &str, path: &Path, cancel: &AtomicBool) -> Result<Option<(Vec<u8>, bool)>> {
    if let Ok(bytes) = tokio::fs::read(path).await
        && !bytes.is_empty()
    {
        return Ok(Some((bytes, true)));
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let bytes = shared.download(url).await?;
    write_cache_file(path, &bytes).await;
    Ok(Some((bytes, false)))
}

async fn write_cache_file(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("part");
    let result = async {
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(e) = result {
        log::debug!("could not cache image {}: {e}", path.display());
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

/// Decodes on the blocking pool into a texture, plus the image's vibrant colour (for the dynamic
/// accent; a few thousand pixels at most, so it costs next to nothing).
async fn decode_off_thread(ctx: &egui::Context, url: &str, bytes: Vec<u8>, px: u32) -> Result<(TextureHandle, Option<Color32>)> {
    let ctx = ctx.clone();
    let name = format!("art:{px}:{url}");
    tokio::task::spawn_blocking(move || -> Result<(TextureHandle, Option<Color32>)> {
        let image = decode_square(&bytes, px)?;
        let swatch = super::accent::art_color(&image.pixels);
        Ok((ctx.load_texture(name, image, TextureOptions::LINEAR), swatch))
    })
    .await
    .context("image decoder stopped")?
}

/// Decodes, centre-crops to a square and downsizes to at most `px` x `px`.
fn decode_square(bytes: &[u8], px: u32) -> Result<ColorImage> {
    let mut img = image::load_from_memory(bytes).context("not a supported image")?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err(anyhow!("empty image"));
    }
    if w != h {
        let side = w.min(h);
        img = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    }
    if img.width() > px {
        img = img.resize_exact(px, px, image::imageops::FilterType::Triangle);
    }
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Keeps the on-disk cache bounded: when above the limit, delete the oldest files.
fn prune_disk_cache(dir: &Path) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= DISK_CACHE_MAX_BYTES {
        return;
    }
    files.sort_by_key(|f| f.0);
    let mut removed = 0usize;
    for (_, len, path) in files {
        if total <= DISK_CACHE_TARGET_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
            removed += 1;
        }
    }
    log::info!("image cache trimmed: removed {removed} old files");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_stable_hex() {
        let a = file_name("https://i.scdn.co/image/abc");
        assert_eq!(a, file_name("https://i.scdn.co/image/abc"));
        assert_ne!(a, file_name("https://i.scdn.co/image/abd"));
        assert_eq!(a.len(), 36);
        assert!(a.trim_end_matches(".img").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn crops_and_downscales() {
        let img = image::RgbaImage::from_pixel(300, 200, image::Rgba([10, 20, 30, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let out = decode_square(&png, 64).unwrap();
        assert_eq!(out.size, [64, 64]);
    }
}
