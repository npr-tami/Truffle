//! Draws the Final prototype's icons (`icon_svg.rs`): each is rasterised from its SVG at the exact pixel size
//! it is shown at (so it stays sharp at every scaling) and kept as a texture. Outlined and filled icons are
//! drawn white and tinted with the colour asked for; the logo keeps its own colours.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{Color32, ColorImage, Context, Id, Painter, Rect, TextureHandle, TextureOptions, pos2};

use super::icon_svg::{ICONS, Paint};

/// The prototype's outline width (CSS `stroke-width: 1.4` in the icons' 24-unit box).
pub const STROKE: f32 = 1.4;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    name: &'static str,
    px: u32,
    /// Outline width in thousandths of a view-box unit.
    stroke: u32,
}

#[derive(Clone, Default)]
struct Cache(Arc<Mutex<HashMap<Key, Option<TextureHandle>>>>);

/// Paints icon `name` filling `rect` (it is square in the prototype; a non-square rect uses its smaller
/// side, centred) in `color`, with the outline `stroke` wide (view-box units; [`STROKE`] normally).
/// An unknown name paints nothing.
pub fn paint(painter: &Painter, rect: Rect, name: &str, color: Color32, stroke: f32) {
    let ctx = painter.ctx();
    let side = rect.width().min(rect.height());
    let px = (side * ctx.pixels_per_point()).round().max(1.0) as u32;
    let Some(&(key_name, vb, kind, body)) = ICONS.iter().find(|i| i.0 == name) else {
        return;
    };
    let key = Key { name: key_name, px, stroke: (stroke * 1000.0).round() as u32 };
    let Some(texture) = texture(ctx, key, vb, kind, body) else { return };
    let r = Rect::from_center_size(rect.center(), egui::vec2(side, side));
    let tint = if kind == Paint::Own { Color32::WHITE } else { color };
    painter.image(texture.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), tint);
}

fn texture(ctx: &Context, key: Key, vb: f32, kind: Paint, body: &str) -> Option<TextureHandle> {
    let cache = ctx.data_mut(|d| d.get_temp_mut_or_default::<Cache>(Id::new("truffle-svg-icons")).clone());
    let mut map = cache.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(t) = map.get(&key) {
        return t.clone();
    }
    let made = rasterise(key, vb, kind, body).map(|image| {
        ctx.load_texture(format!("icon-{}-{}-{}", key.name, key.px, key.stroke), image, TextureOptions::LINEAR)
    });
    map.insert(key, made.clone());
    made
}

/// The SVG for one icon: white, so a tint gives it any colour.
fn svg_source(key: Key, vb: f32, kind: Paint, body: &str) -> String {
    let body = body.replace("currentColor", "#fff");
    let paint = match kind {
        Paint::Stroke => format!(
            r##"fill="none" stroke="#fff" stroke-width="{}" stroke-linecap="round" stroke-linejoin="round""##,
            key.stroke as f32 / 1000.0
        ),
        Paint::Fill => r##"fill="#fff" stroke="none""##.to_owned(),
        Paint::Own => String::new(),
    };
    format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {vb} {vb}" width="{vb}" height="{vb}" {paint}>{body}</svg>"#)
}

fn rasterise(key: Key, vb: f32, kind: Paint, body: &str) -> Option<ColorImage> {
    let svg = svg_source(key, vb, kind, body);
    let tree = match resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("icon {}: {e}", key.name);
            return None;
        }
    };
    let mut pixmap = resvg::tiny_skia::Pixmap::new(key.px, key.px)?;
    let scale = key.px as f32 / vb;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Some(ColorImage::from_rgba_premultiplied([key.px as usize, key.px as usize], pixmap.data()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_rasterises() {
        for &(name, vb, kind, body) in ICONS {
            let key = Key { name, px: 16, stroke: 1400 };
            let img = rasterise(key, vb, kind, body).unwrap_or_else(|| panic!("{name} did not render"));
            let inked = img.pixels.iter().filter(|p| p.a() > 0).count();
            assert!(inked > 4, "{name} is empty");
        }
    }

    #[test]
    fn the_icons_the_frame_uses_exist() {
        for name in ["logo", "side", "back", "fwd", "search", "min", "max", "x", "play", "pause", "prev", "next",
            "shuffle", "repeat", "vol", "volx", "heart", "heartf", "panel", "sleep", "mini", "gear", "more", "tt", "home",
            "album", "artist", "lib", "disc"]
        {
            assert!(ICONS.iter().any(|i| i.0 == name), "{name}");
        }
    }
}
