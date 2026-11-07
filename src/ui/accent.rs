//! Colour helpers: the vibrant colour of a piece of album art (for the dynamic accent), WCAG
//! contrast checks that keep text readable, and the accent cross-fade.

use std::time::{Duration, Instant};

use egui::Color32;

use super::util::mix;

/// How long the accent takes to fade to a new colour.
pub const ACCENT_FADE: Duration = Duration::from_millis(400);

/// At most this many pixels are looked at per image (evenly spaced), however large it is.
const MAX_SAMPLES: usize = 4096;
/// Hue buckets of 15 degrees.
const HUE_BINS: usize = 24;
/// Pixels less saturated / darker than this don't count as "a colour".
const MIN_SATURATION: f32 = 0.28;
const MIN_VALUE: f32 = 0.22;
/// The chosen hue must cover at least this share of the (opaque) image, weighted by vibrancy.
const MIN_SHARE: f32 = 0.025;

fn channel_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// WCAG relative luminance: 0 for black, 1 for white.
pub fn luminance(c: Color32) -> f32 {
    0.2126 * channel_to_linear(c.r()) + 0.7152 * channel_to_linear(c.g()) + 0.0722 * channel_to_linear(c.b())
}

/// WCAG contrast ratio between two colours, from 1 (same) to 21 (black on white).
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// The lowest contrast of `fg` against any of `bgs`.
fn worst_contrast(fg: Color32, bgs: &[Color32]) -> f32 {
    bgs.iter().map(|&bg| contrast_ratio(fg, bg)).fold(f32::INFINITY, f32::min)
}

/// Moves `fg` towards white or black, as little as needed, until it has at least `min_ratio`
/// contrast against every colour in `bgs`. If no blend reaches it (backgrounds both very dark and
/// very light), the blend with the best worst-case contrast is returned.
pub fn ensure_contrast(fg: Color32, bgs: &[Color32], min_ratio: f32) -> Color32 {
    if bgs.is_empty() || worst_contrast(fg, bgs) >= min_ratio {
        return fg;
    }
    const STEPS: u32 = 40;
    // The passing blend that needs the smallest change, else the blend with the best contrast.
    let mut passing: Option<(u32, Color32)> = None;
    let mut fallback = (worst_contrast(fg, bgs), fg);
    for target in [Color32::WHITE, Color32::BLACK] {
        for step in 1..=STEPS {
            let candidate = mix(fg, target, step as f32 / STEPS as f32);
            let contrast = worst_contrast(candidate, bgs);
            if contrast >= min_ratio {
                if passing.is_none_or(|(s, _)| step < s) {
                    passing = Some((step, candidate));
                }
                break;
            }
            if contrast > fallback.0 {
                fallback = (contrast, candidate);
            }
        }
    }
    passing.map_or(fallback.1, |(_, c)| c)
}

/// Hue in degrees (0..360), saturation and value (0..=1).
pub fn to_hsv(c: Color32) -> (f32, f32, f32) {
    let (r, g, b) = (c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let hue = if delta <= f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if max <= f32::EPSILON { 0.0 } else { delta / max };
    (hue.rem_euclid(360.0), saturation, max)
}

pub fn from_hsv(hue: f32, saturation: f32, value: f32) -> Color32 {
    let h = hue.rem_euclid(360.0) / 60.0;
    let s = saturation.clamp(0.0, 1.0);
    let v = value.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to_u8 = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(to_u8(r), to_u8(g), to_u8(b))
}

/// The most prominent vibrant colour of an image (opaque pixels, any order), or `None` when the
/// image is essentially grey, black or white. Looks at no more than ~4k pixels.
pub fn vibrant_color(pixels: &[Color32]) -> Option<Color32> {
    if pixels.is_empty() {
        return None;
    }
    let stride = pixels.len().div_ceil(MAX_SAMPLES).max(1);
    let mut weight = [0f32; HUE_BINS];
    let mut sums = [[0f32; 3]; HUE_BINS];
    let mut opaque = 0usize;
    for px in pixels.iter().step_by(stride) {
        if px.a() < 200 {
            continue;
        }
        opaque += 1;
        let (hue, saturation, value) = to_hsv(*px);
        if saturation < MIN_SATURATION || value < MIN_VALUE {
            continue;
        }
        // Favour saturated, reasonably bright colours over dull ones.
        let w = saturation * saturation * value;
        let bin = ((hue / 360.0 * HUE_BINS as f32) as usize).min(HUE_BINS - 1);
        weight[bin] += w;
        sums[bin][0] += px.r() as f32 * w;
        sums[bin][1] += px.g() as f32 * w;
        sums[bin][2] += px.b() as f32 * w;
    }
    if opaque == 0 {
        return None;
    }
    // Hue is circular: score each bin together with half of its neighbours.
    let score = |i: usize| {
        let prev = weight[(i + HUE_BINS - 1) % HUE_BINS];
        let next = weight[(i + 1) % HUE_BINS];
        weight[i] + 0.5 * (prev + next)
    };
    let best = (0..HUE_BINS).max_by(|&a, &b| score(a).total_cmp(&score(b)))?;
    if weight[best] <= 0.0 || score(best) < opaque as f32 * MIN_SHARE {
        return None;
    }
    let w = weight[best];
    let [r, g, b] = sums[best];
    Some(Color32::from_rgb((r / w).round() as u8, (g / w).round() as u8, (b / w).round() as u8))
}

/// The accent a black-and-white cover gives: a soft silver.
pub const SILVER: Color32 = Color32::from_rgb(196, 198, 206);
/// Below this saturation a cover counts as black and white.
const NEUTRAL_SATURATION: f32 = 0.1;

/// The colour an album cover gives the accent, so the accent always follows the art: its most
/// prominent vibrant colour; for a dull cover its main tint; for a black-and-white one, silver.
/// `None` only when there is nothing opaque to look at.
pub fn art_color(pixels: &[Color32]) -> Option<Color32> {
    if let Some(c) = vibrant_color(pixels) {
        return Some(c);
    }
    let stride = pixels.len().div_ceil(MAX_SAMPLES).max(1);
    let (mut sum, mut n, mut opaque) = ([0f32; 3], 0f32, false);
    for px in pixels.iter().step_by(stride).filter(|px| px.a() >= 200) {
        opaque = true;
        // Mid tones carry a dull cover's tint; near-black and near-white say nothing.
        let (_, _, value) = to_hsv(*px);
        if (0.15..=0.95).contains(&value) {
            sum[0] += f32::from(px.r());
            sum[1] += f32::from(px.g());
            sum[2] += f32::from(px.b());
            n += 1.0;
        }
    }
    if !opaque {
        return None;
    }
    if n == 0.0 {
        return Some(SILVER);
    }
    let avg = Color32::from_rgb((sum[0] / n).round() as u8, (sum[1] / n).round() as u8, (sum[2] / n).round() as u8);
    let (_, saturation, _) = to_hsv(avg);
    Some(if saturation < NEUTRAL_SATURATION { SILVER } else { avg })
}

/// Turns an album-art colour into a usable accent on the backgrounds `bgs` it is drawn on:
/// saturated and bright enough to read as a colour, with at least 3:1 contrast against each (the
/// WCAG level for controls) where possible.
pub fn adapt_accent(color: Color32, bgs: &[Color32]) -> Color32 {
    let (hue, saturation, value) = to_hsv(color);
    // Black-and-white art stays neutral (a light silver), rather than being pushed to whatever
    // hue its tiny tint happens to have.
    if saturation < NEUTRAL_SATURATION {
        return ensure_contrast(from_hsv(hue, saturation, value.max(0.72)), bgs, 3.0);
    }
    let vivid = from_hsv(hue, saturation.max(0.45), value.max(0.4));
    ensure_contrast(vivid, bgs, 3.0)
}

/// A colour that fades smoothly from one value to the next.
#[derive(Clone, Copy, Debug)]
pub struct AccentFade {
    from: Color32,
    to: Color32,
    started: Option<Instant>,
}

impl AccentFade {
    pub fn new(color: Color32) -> Self {
        Self { from: color, to: color, started: None }
    }

    /// The colour the fade is heading to.
    pub fn target(&self) -> Color32 {
        self.to
    }

    /// Fades to `to`, starting from wherever the colour is right now.
    pub fn retarget(&mut self, to: Color32, now: Instant) {
        if to == self.to {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.started = Some(now);
    }

    /// Switches to `to` at once, without fading.
    pub fn jump(&mut self, to: Color32) {
        *self = Self::new(to);
    }

    pub fn value(&self, now: Instant) -> Color32 {
        let Some(started) = self.started else { return self.to };
        let t = now.saturating_duration_since(started).as_secs_f32() / ACCENT_FADE.as_secs_f32();
        if t >= 1.0 {
            self.to
        } else {
            // Smoothstep easing.
            let eased = t * t * (3.0 - 2.0 * t);
            mix(self.from, self.to, eased)
        }
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.started.is_some_and(|s| now.saturating_duration_since(s) < ACCENT_FADE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cover_gives_an_accent() {
        // Black and white: silver, and it stays neutral as an accent.
        let grey: Vec<Color32> = (0..400).map(|i| Color32::from_gray((i % 200) as u8 + 30)).collect();
        assert_eq!(art_color(&grey), Some(SILVER));
        let dark = [Color32::from_rgb(18, 18, 20), Color32::from_rgb(21, 21, 23)];
        let (_, s, _) = to_hsv(adapt_accent(SILVER, &dark));
        assert!(s < NEUTRAL_SATURATION, "{s}");
        // A dull brown cover keeps its tint.
        let brown: Vec<Color32> = (0..400).map(|_| Color32::from_rgb(120, 100, 80)).collect();
        let c = art_color(&brown).expect("a tint");
        let (hue, _, _) = to_hsv(c);
        assert!((20.0..=45.0).contains(&hue), "{hue}");
        // Nothing opaque: nothing to go on.
        assert_eq!(art_color(&[Color32::TRANSPARENT; 10]), None);
    }

    const DARK_BG: Color32 = Color32::from_rgb(18, 18, 20);
    const LIGHT_BG: Color32 = Color32::from_rgb(250, 250, 251);

    #[test]
    fn contrast_matches_wcag() {
        assert!((contrast_ratio(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(Color32::RED, Color32::RED) - 1.0).abs() < 0.001);
    }

    #[test]
    fn hsv_round_trips() {
        for c in [Color32::from_rgb(29, 185, 84), Color32::from_rgb(200, 30, 140), Color32::from_rgb(12, 34, 250)] {
            let (h, s, v) = to_hsv(c);
            let back = from_hsv(h, s, v);
            for (a, b) in [(c.r(), back.r()), (c.g(), back.g()), (c.b(), back.b())] {
                assert!((a as i32 - b as i32).abs() <= 1, "{c:?} -> {back:?}");
            }
        }
    }

    #[test]
    fn picks_the_dominant_vivid_colour() {
        // Mostly dark grey with a big orange area and a little blue.
        let mut pixels = vec![Color32::from_rgb(40, 40, 40); 6000];
        pixels.extend(std::iter::repeat_n(Color32::from_rgb(240, 120, 20), 3000));
        pixels.extend(std::iter::repeat_n(Color32::from_rgb(20, 60, 230), 400));
        let c = vibrant_color(&pixels).unwrap();
        let (hue, _, _) = to_hsv(c);
        assert!((20.0..40.0).contains(&hue), "hue {hue} of {c:?}");
    }

    #[test]
    fn grey_art_has_no_vibrant_colour() {
        let mut pixels: Vec<Color32> = (0..5000).map(|i| Color32::from_gray((i % 256) as u8)).collect();
        // A few stray coloured pixels are not enough.
        pixels.extend(std::iter::repeat_n(Color32::from_rgb(255, 0, 0), 20));
        assert_eq!(vibrant_color(&pixels), None);
        assert_eq!(vibrant_color(&[]), None);
        assert_eq!(vibrant_color(&[Color32::TRANSPARENT; 50]), None);
    }

    #[test]
    fn adapted_accents_stay_visible() {
        let raws = [
            Color32::from_rgb(10, 10, 120),
            Color32::from_rgb(250, 240, 60),
            Color32::from_rgb(120, 20, 20),
            Color32::from_rgb(40, 200, 90),
            Color32::from_rgb(60, 220, 230),
        ];
        // Each theme's main area, panels and player bar; and a dark theme with light custom panels.
        let surfaces: [&[Color32]; 3] = [
            &[DARK_BG, Color32::from_rgb(11, 11, 12), Color32::from_rgb(14, 14, 16)],
            &[LIGHT_BG, Color32::from_rgb(240, 240, 243), Color32::from_rgb(234, 234, 238)],
            &[DARK_BG, Color32::from_rgb(230, 230, 230), Color32::from_rgb(230, 230, 230)],
        ];
        for raw in raws {
            for bgs in surfaces {
                let accent = adapt_accent(raw, bgs);
                for &bg in bgs {
                    assert!(contrast_ratio(accent, bg) >= 2.95, "{raw:?} on {bg:?} -> {accent:?}");
                }
            }
        }
    }

    #[test]
    fn ensure_contrast_changes_only_what_is_needed() {
        let fine = Color32::from_rgb(220, 220, 220);
        assert_eq!(ensure_contrast(fine, &[DARK_BG], 4.5), fine);
        let grey = Color32::from_rgb(90, 90, 90);
        let fixed = ensure_contrast(grey, &[DARK_BG], 4.5);
        assert!(contrast_ratio(fixed, DARK_BG) >= 4.5);
        assert!(fixed.r() > grey.r(), "lightened on a dark background");
        let fixed = ensure_contrast(Color32::from_rgb(200, 200, 200), &[LIGHT_BG], 4.5);
        assert!(contrast_ratio(fixed, LIGHT_BG) >= 4.5);
        // Impossible request: still returns something sensible.
        let _ = ensure_contrast(grey, &[Color32::BLACK, Color32::WHITE], 7.0);
    }

    #[test]
    fn fade_reaches_its_target() {
        let start = Instant::now();
        let mut fade = AccentFade::new(Color32::BLACK);
        assert!(!fade.animating(start));
        fade.retarget(Color32::WHITE, start);
        assert!(fade.animating(start));
        let mid = fade.value(start + ACCENT_FADE / 2);
        assert!(mid.r() > 60 && mid.r() < 200, "{mid:?}");
        assert_eq!(fade.value(start + ACCENT_FADE), Color32::WHITE);
        assert!(!fade.animating(start + ACCENT_FADE));
        // Retargeting mid-way continues from the current colour.
        fade.retarget(Color32::BLACK, start + ACCENT_FADE / 2);
        assert_eq!(fade.value(start + ACCENT_FADE / 2), mid);
    }
}
