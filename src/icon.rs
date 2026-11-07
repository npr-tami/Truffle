//! The app icon (window, taskbar, tray, and the logo on the startup screen): a turntable with a
//! chrome tonearm on a black record, in two styles: a silver deck (`assets/logo.svg`) and a black
//! one framed in chrome (`assets/logo_black.svg`). Settings > Appearance picks one, or follows the
//! taskbar ([`style_for`]).
//!
//! The artwork is rendered by `examples/make_icon.rs` into `assets/` (run
//! `cargo run --example make_icon` to regenerate). The exe's own icon is embedded by build.rs from
//! assets/truffle.ico (the silver deck) as icon resource id 1. The PNGs below are compiled in
//! here, so the window and tray can show either style, and never depend on that step (the
//! resource is absent when rc.exe wasn't available at build time).

use std::sync::OnceLock;

use crate::config::AppIcon;

/// One of the icon's two looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconStyle {
    Silver,
    Black,
}

/// The style to show for the user's choice: "Automatic" stands out on the taskbar, silver on a
/// dark one and black on a light one.
pub fn style_for(choice: AppIcon, taskbar_dark: bool) -> IconStyle {
    match choice {
        AppIcon::Silver => IconStyle::Silver,
        AppIcon::Black => IconStyle::Black,
        AppIcon::Automatic if taskbar_dark => IconStyle::Silver,
        AppIcon::Automatic => IconStyle::Black,
    }
}

/// 256x256 artwork: eframe scales it to the exact title-bar and taskbar sizes itself.
static PNG_256: [&[u8]; 2] = [include_bytes!("../assets/truffle.png"), include_bytes!("../assets/truffle_black.png")];

/// The tray's sizes (the notification area's small icons at 100 to 200 % scaling), each rendered
/// at its size: crisper than a scaled bitmap.
const TRAY_SIZES: [u32; 5] = [16, 20, 24, 28, 32];
static TRAY_PNGS: [[&[u8]; 5]; 2] = [
    [
        include_bytes!("../assets/tray/silver_16.png"),
        include_bytes!("../assets/tray/silver_20.png"),
        include_bytes!("../assets/tray/silver_24.png"),
        include_bytes!("../assets/tray/silver_28.png"),
        include_bytes!("../assets/tray/silver_32.png"),
    ],
    [
        include_bytes!("../assets/tray/black_16.png"),
        include_bytes!("../assets/tray/black_20.png"),
        include_bytes!("../assets/tray/black_24.png"),
        include_bytes!("../assets/tray/black_28.png"),
        include_bytes!("../assets/tray/black_32.png"),
    ],
];

/// The deck's silver, for the fallback icon.
const SILVER: [u8; 3] = [0xC9, 0xD0, 0xDB];

fn index(style: IconStyle) -> usize {
    match style {
        IconStyle::Silver => 0,
        IconStyle::Black => 1,
    }
}

/// RGBA icon for the window/taskbar (256x256). Never fails: falls back to an empty icon (the
/// system default) if the embedded artwork can't be decoded.
pub fn window_icon(style: IconStyle) -> egui::IconData {
    static ICONS: [OnceLock<egui::IconData>; 2] = [OnceLock::new(), OnceLock::new()];
    ICONS[index(style)]
        .get_or_init(|| match decode_png(PNG_256[index(style)]) {
            Ok((rgba, width, height)) => egui::IconData { rgba, width, height },
            Err(e) => {
                log::warn!("could not decode the window icon: {e}");
                egui::IconData::default()
            }
        })
        .clone()
}

/// RGBA pixels + size for the tray icon, drawn for `size` (the system's small-icon size): the
/// smallest rendered size that is at least as big, else the biggest. Never fails: falls back to a
/// plain silver disc drawn in code.
pub fn tray_icon_rgba(style: IconStyle, size: u32) -> (Vec<u8>, u32, u32) {
    let at = TRAY_SIZES.iter().position(|&s| s >= size).unwrap_or(TRAY_SIZES.len() - 1);
    match decode_png(TRAY_PNGS[index(style)][at]) {
        Ok(icon) => icon,
        Err(e) => {
            log::warn!("could not decode the tray icon: {e}");
            (fallback_disc(32), 32, 32)
        }
    }
}

fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), image::ImageError> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)?.into_rgba8();
    let (width, height) = img.dimensions();
    Ok((img.into_raw(), width, height))
}

/// Anti-aliased silver disc, `size` x `size` RGBA.
fn fallback_disc(size: u32) -> Vec<u8> {
    let s = size as f32;
    let radius = s / 2.0 - 1.0;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            // Signed distance from the pixel centre to the disc's edge.
            let d = (x as f32 + 0.5 - s / 2.0).hypot(y as f32 + 0.5 - s / 2.0) - radius;
            let alpha = (0.5 - d).clamp(0.0, 1.0);
            rgba.extend_from_slice(&[SILVER[0], SILVER[1], SILVER[2], (alpha * 255.0).round() as u8]);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLES: [IconStyle; 2] = [IconStyle::Silver, IconStyle::Black];

    #[test]
    fn window_icons_are_256_rgba() {
        for style in STYLES {
            let icon = window_icon(style);
            assert_eq!((icon.width, icon.height), (256, 256));
            assert_eq!(icon.rgba.len(), 256 * 256 * 4);
            // Transparent corner, opaque centre.
            assert_eq!(icon.rgba[3], 0);
            let centre = (128 * 256 + 128) * 4;
            assert_eq!(icon.rgba[centre + 3], 255);
        }
        // The two styles differ: the silver deck is light where the black one is dark.
        let (silver, black) = (window_icon(IconStyle::Silver), window_icon(IconStyle::Black));
        let corner_of_deck = (210 * 256 + 40) * 4;
        assert!(silver.rgba[corner_of_deck] > black.rgba[corner_of_deck] + 60);
    }

    #[test]
    fn tray_icons_come_at_each_size() {
        for style in STYLES {
            for (want, got) in [(16, 16), (20, 20), (24, 24), (28, 28), (32, 32), (17, 20), (40, 32), (8, 16)] {
                let (rgba, w, h) = tray_icon_rgba(style, want);
                assert_eq!((w, h), (got, got), "{style:?} at {want}");
                assert_eq!(rgba.len(), (got * got * 4) as usize);
                assert_eq!(rgba[3], 0, "corner is transparent");
                // The deck fills most of the tile.
                let opaque = rgba.as_chunks::<4>().0.iter().filter(|p| p[3] == 255).count();
                assert!(opaque > (got * got / 2) as usize, "{style:?} {got}: only {opaque} opaque pixels");
            }
        }
    }

    #[test]
    fn automatic_stands_out_on_the_taskbar() {
        assert_eq!(style_for(AppIcon::Automatic, true), IconStyle::Silver);
        assert_eq!(style_for(AppIcon::Automatic, false), IconStyle::Black);
        for dark in [true, false] {
            assert_eq!(style_for(AppIcon::Silver, dark), IconStyle::Silver);
            assert_eq!(style_for(AppIcon::Black, dark), IconStyle::Black);
        }
    }

    #[test]
    fn exe_icon_file_has_all_sizes() {
        let ico = include_bytes!("../assets/truffle.ico");
        assert_eq!(&ico[..4], &[0, 0, 1, 0], "ICO header");
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        let sizes: Vec<u32> = (0..count)
            .map(|i| match ico[6 + 16 * i] {
                0 => 256,
                n => n as u32,
            })
            .collect();
        assert_eq!(sizes, [16, 20, 24, 32, 40, 48, 64, 128, 256]);
        // Every entry lies inside the file.
        for i in 0..count {
            let e = &ico[6 + 16 * i..6 + 16 * (i + 1)];
            let len = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
            let off = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
            assert!(off + len <= ico.len());
        }
    }

    #[test]
    fn fallback_disc_shape() {
        let px = fallback_disc(32);
        assert_eq!(px.len(), 32 * 32 * 4);
        assert_eq!(px[3], 0, "corner is transparent");
        assert_eq!(px[(16 * 32 + 16) * 4 + 3], 255, "centre is opaque");
    }
}
