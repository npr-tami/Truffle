//! Windows media overlay, lock screen and media keys (System Media Transport Controls via
//! souvlaki). Lives on the UI thread; button presses arrive on a Windows thread-pool thread and
//! are forwarded through the platform channel.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};

use super::state::{NowPlaying, Status};

/// Bumped whenever controls are dropped. souvlaki never unregisters its seek-bar handler, so a
/// handler from an earlier attach checks this and goes quiet instead of sending duplicates.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub struct Media {
    controls: MediaControls,
}

impl Media {
    /// Attach to the window's media controls. `on_event` runs on a background thread.
    pub fn new(hwnd: isize, on_event: impl Fn(MediaControlEvent) + Send + 'static) -> Result<Self, String> {
        if hwnd == 0 {
            return Err("no window handle".to_owned());
        }
        let config = PlatformConfig { display_name: "Truffle", dbus_name: "truffle", hwnd: Some(hwnd as *mut c_void) };
        let mut controls = MediaControls::new(config).map_err(|e| e.to_string())?;
        let generation = GENERATION.load(Ordering::SeqCst);
        controls
            .attach(move |event| {
                if GENERATION.load(Ordering::SeqCst) == generation {
                    on_event(event);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { controls })
    }

    /// Show a new track (or keep the last one when nothing is loaded).
    pub fn set_track(&mut self, track: Option<&NowPlaying>) {
        let Some(track) = track else { return };
        let artist = (!track.artist.is_empty()).then_some(track.artist.as_str());
        let album = (!track.album.is_empty()).then_some(track.album.as_str());
        let cover = track.cover.as_deref().filter(|url| url.starts_with("https://") || url.starts_with("http://"));
        let metadata = MediaMetadata {
            title: Some(track.title.as_str()),
            artist,
            album,
            cover_url: cover,
            duration: (track.duration_ms > 0).then(|| Duration::from_millis(u64::from(track.duration_ms))),
        };
        if let Err(e) = self.controls.set_metadata(metadata) {
            log::debug!("media overlay: could not set the track details: {e}");
        }
    }

    pub fn set_status(&mut self, status: Status, position_ms: u32) {
        let progress = Some(MediaPosition(Duration::from_millis(u64::from(position_ms))));
        let playback = match status {
            Status::Playing => MediaPlayback::Playing { progress },
            Status::Paused => MediaPlayback::Paused { progress },
            Status::Stopped => MediaPlayback::Stopped,
        };
        if let Err(e) = self.controls.set_playback(playback) {
            log::debug!("media overlay: could not set the playback state: {e}");
        }
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        GENERATION.fetch_add(1, Ordering::SeqCst);
        // souvlaki's own Drop detaches (hides the overlay and removes the button handler).
    }
}
