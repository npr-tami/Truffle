//! A short silent video played in a loop (a song's Canvas, in the Now playing view), decoded by
//! Windows' own Media Foundation on a thread of its own: RGBA frames a few ahead, each with the time
//! it is due, counting on across the rounds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::Duration;

use egui::{Color32, ColorImage};

/// A decoded frame and when it is due from the video's start.
pub struct Frame {
    pub image: ColorImage,
    pub at: Duration,
}

/// A video file playing in a loop, as frames to take when they are due. Dropping it stops its thread.
pub struct VideoLoop {
    frames: Receiver<Frame>,
    stop: Arc<AtomicBool>,
}

impl VideoLoop {
    pub fn open(path: PathBuf) -> Self {
        // A few frames ahead: the decoding waits there.
        let (tx, frames) = sync_channel(3);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let spawned = std::thread::Builder::new().name("canvas-video".into()).spawn(move || {
            if let Err(e) = decode(&path, &tx, &flag) {
                log::warn!("canvas video {} not played: {e}", path.display());
            }
        });
        if let Err(e) = spawned {
            log::warn!("canvas video thread not started: {e}");
        }
        Self { frames, stop }
    }

    /// The next decoded frame, when one is ready; `Err` once the video can't give more (its file
    /// couldn't be read).
    pub fn next(&self) -> Result<Option<Frame>, ()> {
        match self.frames.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(()),
        }
    }
}

impl Drop for VideoLoop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// How far apart the rows of a frame `len` bytes long are: as far as the buffer holds rows when that's
/// wider than `stride` says. Decoders pad rows (1080 pixels to 1088) and the stride doesn't always
/// say so; read at the narrower stride, each row starts a little further off and the picture smears
/// into slanted stripes.
fn row_pitch(len: usize, width: usize, height: usize, stride: i32) -> i32 {
    let said = stride.unsigned_abs() as usize;
    let held = if height > 0 && len.is_multiple_of(height) { len / height } else { 0 };
    let pitch = if held > said && held >= width * 4 { held } else { said };
    let pitch = i32::try_from(pitch).unwrap_or(stride);
    if stride < 0 { -pitch } else { pitch }
}

/// Media Foundation's RGB32 frame (blue, green, red, unused; rows `stride` bytes apart, bottom-up
/// when it is negative) as an image.
fn to_image(bytes: &[u8], width: usize, height: usize, stride: i32) -> Option<ColorImage> {
    let pitch = stride.unsigned_abs() as usize;
    if pitch < width * 4 {
        return None;
    }
    let mut pixels = Vec::with_capacity(width * height);
    for r in 0..height {
        let row = if stride < 0 { height - 1 - r } else { r };
        let line = bytes.get(row * pitch..row * pitch + width * 4)?;
        pixels.extend(line.as_chunks::<4>().0.iter().map(|&[b, g, r, _]| Color32::from_rgb(r, g, b)));
    }
    Some(ColorImage::new([width, height], pixels))
}

/// When a round of the video ends, the next starts one frame (`step`) after its last frame.
fn next_round(offset: Duration, last: Duration, step: Duration) -> Duration {
    offset + last + step.max(Duration::from_millis(1))
}

#[cfg(windows)]
fn decode(path: &Path, tx: &SyncSender<Frame>, stop: &AtomicBool) -> windows::core::Result<()> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::HSTRING;

    // SAFETY: COM and Media Foundation are started and stopped on this thread, around their use;
    // the buffer is read only while it is locked.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let started = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
        let result = started.and_then(|()| {
            let url = HSTRING::from(path.as_os_str());
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let (mut offset, mut last, mut step) = (Duration::ZERO, Duration::ZERO, Duration::from_millis(33));
            // Each round opens the file again: simpler than seeking back, and it is small.
            loop {
                let mut attributes = None;
                MFCreateAttributes(&mut attributes, 1)?;
                let attributes = attributes.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
                // Decoded frames converted to RGB32 by Windows.
                attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
                let reader = MFCreateSourceReaderFromURL(&url, &attributes)?;
                let rgb = MFCreateMediaType()?;
                rgb.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                rgb.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
                reader.SetCurrentMediaType(stream, None, &rgb)?;
                let current = reader.GetCurrentMediaType(stream)?;
                let size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
                let (width, height) = ((size >> 32) as usize, (size & 0xffff_ffff) as usize);
                let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or((width * 4) as i32, |s| s as i32);
                let mut frames = 0;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let (mut flags, mut time, mut sample) = (0u32, 0i64, None);
                    reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
                    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                        break;
                    }
                    let Some(sample) = sample else { continue };
                    let at = Duration::from_nanos(u64::try_from(time).unwrap_or(0) * 100);
                    if at > last {
                        step = at - last;
                    }
                    last = at;
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
                    buffer.Lock(&mut data, None, Some(&mut len))?;
                    let pitch = row_pitch(len as usize, width, height, stride);
                    let image = if data.is_null() { None } else { to_image(std::slice::from_raw_parts(data, len as usize), width, height, pitch) };
                    buffer.Unlock()?;
                    if let Some(image) = image {
                        frames += 1;
                        if tx.send(Frame { image, at: offset + at }).is_err() {
                            return Ok(());
                        }
                    }
                }
                if frames == 0 {
                    return Err(windows::core::Error::from(windows::Win32::Foundation::E_FAIL));
                }
                offset = next_round(offset, last, step);
                last = Duration::ZERO;
            }
        });
        let _ = MFShutdown();
        CoUninitialize();
        result
    }
}

#[cfg(not(windows))]
fn decode(_path: &Path, _tx: &SyncSender<Frame>, _stop: &AtomicBool) -> Result<(), &'static str> {
    Err("videos play on Windows only")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two by two, blue-green-red-unused bytes, rows 12 apart (4 bytes of padding).
    const FRAME: [u8; 24] = [10, 20, 30, 0, 11, 21, 31, 0, 9, 9, 9, 9, 40, 50, 60, 0, 41, 51, 61, 0, 9, 9, 9, 9];

    #[test]
    fn a_frame_becomes_an_rgb_image_top_row_first() {
        let image = to_image(&FRAME, 2, 2, 12).expect("an image");
        assert_eq!(image.size, [2, 2]);
        assert_eq!(image.pixels[0], Color32::from_rgb(30, 20, 10));
        assert_eq!(image.pixels[3], Color32::from_rgb(61, 51, 41));
        // Bottom-up (a negative stride): the last row in memory is the top one.
        let image = to_image(&FRAME, 2, 2, -12).expect("an image");
        assert_eq!(image.pixels[0], Color32::from_rgb(60, 50, 40));
        // Too little data or a stride narrower than a row: no image, no panic.
        assert!(to_image(&FRAME[..19], 2, 2, 12).is_none());
        assert!(to_image(&FRAME, 4, 2, 12).is_none());
    }

    #[test]
    fn padded_rows_are_read_where_they_start() {
        // A Canvas on 29 Oct: 1080 wide, its rows 4352 bytes apart (1088 pixels), the stride saying 4320.
        assert_eq!(row_pitch(4352 * 1920, 1080, 1920, 4320), 4352);
        assert_eq!(row_pitch(4352 * 1920, 1080, 1920, -4320), -4352);
        // Not padded, or a length that tells nothing: the stride.
        assert_eq!(row_pitch(4320 * 1920, 1080, 1920, 4320), 4320);
        assert_eq!(row_pitch(4320 * 1920 + 7, 1080, 1920, 4320), 4320);
        // A padded frame read at its pitch: each row from where it starts.
        let mut frame = vec![0u8; 12 * 2];
        frame[..8].copy_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        frame[12..20].copy_from_slice(&[7, 8, 9, 0, 10, 11, 12, 0]);
        let image = to_image(&frame, 2, 2, row_pitch(frame.len(), 2, 2, 8)).expect("an image");
        assert_eq!(image.pixels[2], Color32::from_rgb(9, 8, 7));
    }

    #[test]
    fn the_next_round_starts_one_frame_after_the_last() {
        let round = next_round(Duration::ZERO, Duration::from_millis(7967), Duration::from_millis(33));
        assert_eq!(round, Duration::from_millis(8000));
        assert_eq!(next_round(round, Duration::from_millis(7967), Duration::from_millis(33)), Duration::from_millis(16000));
    }
}
