//! Renders Truffle's app icon from the logo: a turntable, its chrome tonearm on a black record and
//! the glint where the needle meets the groove. Two styles: the silver deck (`assets/logo.svg`)
//! and the black one framed in chrome (`assets/logo_black.svg`), each with a simpler cut for
//! 16-32 px (`*_small.svg`).
//!
//! Regenerate the files in `assets/` with:
//!
//! ```text
//! cargo run --example make_icon
//! cargo run --example make_icon -- --preview C:\some\dir\preview.png   (optional contact sheet)
//! ```
//!
//! Every size is rendered directly from the vector logo at that size (no downscaling).
//!
//! Outputs:
//! - `assets/truffle.png`, `assets/truffle_black.png`   256x256, embedded for the window/taskbar
//!   icon and the startup screen
//! - `assets/tray/{silver,black}_{16,20,24,28,32}.png`  the tray icon at each scaling's size
//! - `assets/truffle.ico`   the silver deck at 16, 20, 24, 32, 40, 48, 64, 128, 256 (32-bit BMP
//!   entries, PNG for 256 as is standard; some decoders such as GDI+ mishandle smaller PNG
//!   entries), embedded into the exe by build.rs
//!
//! The preview is a contact sheet of both styles' icon sizes.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ImageEncoder, RgbaImage};

const ICO_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];
const TRAY_SIZES: [u32; 5] = [16, 20, 24, 28, 32];

fn main() {
    let mut preview: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--preview" {
            preview = args.next().map(PathBuf::from);
        }
    }

    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    std::fs::create_dir_all(&assets).expect("create assets/");

    let tray = assets.join("tray");
    std::fs::create_dir_all(&tray).expect("create assets/tray/");
    let mut sheets = Vec::new();
    for (style, name, png) in [(&SILVER, "silver", "truffle.png"), (&BLACK, "black", "truffle_black.png")] {
        let images: Vec<RgbaImage> = ICO_SIZES.iter().map(|&s| render(style, s)).collect();
        std::fs::write(assets.join(png), png_bytes(images.last().expect("256"))).expect("write png");
        for size in TRAY_SIZES {
            std::fs::write(tray.join(format!("{name}_{size}.png")), png_bytes(&render(style, size))).expect("write png");
        }
        if name == "silver" {
            std::fs::write(assets.join("truffle.ico"), ico_bytes(&images)).expect("write ico");
        }
        sheets.push(contact_sheet(&images));
    }
    println!("wrote {}", assets.display());

    if let Some(path) = preview {
        let (a, b) = (&sheets[0], &sheets[1]);
        let mut both = RgbaImage::new(a.width().max(b.width()), a.height() + b.height());
        image::imageops::overlay(&mut both, a, 0, 0);
        image::imageops::overlay(&mut both, b, 0, i64::from(a.height()));
        both.save(&path).expect("write preview");
        println!("wrote preview {}", path.display());
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering

/// One style of the logo, drawn in a 256 box, and its simpler cut for small sizes.
struct Logo {
    full: &'static str,
    small: &'static str,
}

const SILVER: Logo = Logo { full: include_str!("../assets/logo.svg"), small: include_str!("../assets/logo_small.svg") };
const BLACK: Logo = Logo { full: include_str!("../assets/logo_black.svg"), small: include_str!("../assets/logo_black_small.svg") };

/// Sizes up to this use the small cut: no grooves, knob or glint, and a thicker arm.
const SMALL_UP_TO: u32 = 32;

fn render(logo: &Logo, size: u32) -> RgbaImage {
    let svg = if size <= SMALL_UP_TO { logo.small } else { logo.full };
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()).expect("parse logo svg");
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("pixmap");
    let scale = size as f32 / tree.size().width();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // tiny-skia keeps premultiplied alpha; icons and PNGs want it straight.
    let mut img = RgbaImage::new(size, size);
    for (px, out) in pixmap.pixels().iter().zip(img.pixels_mut()) {
        let c = px.demultiply();
        *out = image::Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    img
}

// ---------------------------------------------------------------------------------------------
// Encoding

fn png_bytes(img: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::new();
    PngEncoder::new_with_quality(Cursor::new(&mut out), CompressionType::Best, FilterType::Adaptive)
        .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)
        .expect("encode png");
    out
}

/// 32-bit BMP icon entry: BITMAPINFOHEADER (height doubled), bottom-up BGRA, then the 1-bit AND
/// mask (set where fully transparent, for old consumers that ignore alpha).
fn bmp_entry(img: &RgbaImage) -> Vec<u8> {
    let (w, h) = (img.width(), img.height());
    let mask_row = w.div_ceil(32) * 4;
    let mut out = Vec::new();
    let header: [u32; 10] = [40, w, h * 2, 0, 0, w * h * 4 + mask_row * h, 0, 0, 0, 0];
    for (i, v) in header.iter().enumerate() {
        if i == 3 {
            out.extend_from_slice(&1u16.to_le_bytes()); // planes
            out.extend_from_slice(&32u16.to_le_bytes()); // bit count
            continue;
        }
        if i == 4 {
            out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
            continue;
        }
        out.extend_from_slice(&v.to_le_bytes());
    }
    for y in (0..h).rev() {
        for x in 0..w {
            let p = img.get_pixel(x, y).0;
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    for y in (0..h).rev() {
        let mut row = vec![0u8; mask_row as usize];
        for x in 0..w {
            if img.get_pixel(x, y).0[3] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&row);
    }
    out
}

fn ico_bytes(images: &[RgbaImage]) -> Vec<u8> {
    let entries: Vec<Vec<u8>> =
        images.iter().map(|img| if img.width() >= 256 { png_bytes(img) } else { bmp_entry(img) }).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (img, data) in images.iter().zip(&entries) {
        let dim = |v: u32| if v >= 256 { 0u8 } else { v as u8 };
        out.extend_from_slice(&[dim(img.width()), dim(img.height()), 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for data in &entries {
        out.extend_from_slice(data);
    }
    out
}

/// Every size at 1x and magnified (nearest neighbour), on a dark and a light strip.
fn contact_sheet(images: &[RgbaImage]) -> RgbaImage {
    let pad = 16;
    let zoom_of = |w: u32| if w <= 48 { 6 } else if w <= 128 { 2 } else { 1 };
    let width: u32 = images.iter().map(|i| i.width() * zoom_of(i.width()) + pad).sum::<u32>() + pad;
    let row_h = 256 + 2 * pad;
    let mut sheet = RgbaImage::new(width, row_h * 2 + 2 * (256 + pad));
    for (band, colour) in [[32u8, 32, 32, 255], [243, 243, 243, 255]].iter().enumerate() {
        for y in 0..row_h {
            for x in 0..width {
                sheet.put_pixel(x, band as u32 * row_h + y, image::Rgba(*colour));
            }
        }
        let mut x = pad;
        for img in images {
            let z = zoom_of(img.width());
            let big = image::imageops::resize(img, img.width() * z, img.height() * z, image::imageops::Nearest);
            image::imageops::overlay(&mut sheet, &big, x as i64, (band as u32 * row_h + pad) as i64);
            x += big.width() + pad;
        }
    }
    // Real-size row (1x) under the magnified rows, both backgrounds.
    let base = row_h * 2;
    for (band, colour) in [[32u8, 32, 32, 255], [243, 243, 243, 255]].iter().enumerate() {
        for y in 0..(256 + pad) {
            for x in 0..width {
                sheet.put_pixel(x, base + band as u32 * (256 + pad) + y, image::Rgba(*colour));
            }
        }
        let mut x = pad;
        for img in images {
            image::imageops::overlay(&mut sheet, img, x as i64, (base + band as u32 * (256 + pad) + pad / 2) as i64);
            x += img.width() + pad;
        }
    }
    sheet
}
