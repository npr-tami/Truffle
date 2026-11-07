//! Equalizer presets and helpers for the Sound settings.

use crate::model::{EQ_BANDS_HZ, EqSettings};

/// Gain limits per band and for the preamp, in dB.
pub const GAIN_RANGE: std::ops::RangeInclusive<f32> = -12.0..=12.0;
pub const PREAMP_RANGE: std::ops::RangeInclusive<f32> = -12.0..=0.0;
/// The preset name shown once the bands were changed by hand.
pub const CUSTOM: &str = "Custom";
pub const FLAT: &str = "Flat";

/// Built-in presets: gains in dB for 31, 62, 125, 250, 500 Hz, 1, 2, 4, 8 and 16 kHz.
pub const PRESETS: [(&str, [f32; 10]); 11] = [
    (FLAT, [0.0; 10]),
    ("Bass boost", [6.0, 5.0, 4.0, 2.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Bass reducer", [-6.0, -5.0, -4.0, -2.5, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Treble boost", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.5, 4.0, 5.0, 6.0]),
    ("Vocal", [-2.0, -3.0, -3.0, 1.0, 3.5, 4.0, 3.5, 2.0, 0.0, -1.0]),
    ("Rock", [4.5, 3.5, 2.0, -0.5, -1.5, -1.0, 1.0, 2.5, 3.5, 4.0]),
    ("Pop", [-1.0, 0.0, 1.5, 3.0, 4.0, 3.5, 2.0, 0.5, -0.5, -1.0]),
    ("Electronic", [4.5, 4.0, 1.5, 0.0, -2.0, 1.5, 0.5, 1.5, 4.0, 4.5]),
    ("Acoustic", [4.0, 3.5, 2.5, 1.0, 1.5, 1.5, 3.0, 3.5, 3.0, 2.0]),
    ("Late night", [-4.0, -3.0, -1.5, 0.0, 1.0, 2.0, 2.0, 1.0, -1.0, -2.5]),
    ("Loudness", [5.0, 4.0, 2.0, 0.0, -1.0, -0.5, 0.0, 1.5, 3.5, 4.5]),
];

pub fn preset_gains(name: &str) -> Option<[f32; 10]> {
    PRESETS.iter().find(|(n, _)| *n == name).map(|(_, g)| *g)
}

/// The name to show for the current settings: the preset, or "Custom" for anything else.
pub fn preset_label(eq: &EqSettings) -> &str {
    match PRESETS.iter().find(|(n, _)| *n == eq.preset) {
        Some((name, _)) => name,
        None => CUSTOM,
    }
}

/// "31", "125", "1k", "16k".
pub fn band_label(hz: f32) -> String {
    if hz >= 1000.0 {
        let k = hz / 1000.0;
        if (k - k.round()).abs() < 0.05 { format!("{}k", k.round() as u32) } else { format!("{k:.1}k") }
    } else {
        format!("{}", hz.round() as u32)
    }
}

pub fn band_labels() -> [String; 10] {
    EQ_BANDS_HZ.map(band_label)
}

/// A preamp that keeps the loudest boosted band from clipping: minus the largest boost, rounded to
/// half a dB.
pub fn safe_preamp(gains: &[f32; 10]) -> f32 {
    let boost = gains.iter().copied().filter(|g| g.is_finite()).fold(0.0f32, f32::max);
    (-(boost * 2.0).ceil() / 2.0).clamp(*PREAMP_RANGE.start(), *PREAMP_RANGE.end())
}

/// Settings for a preset, enabled.
pub fn apply_preset(eq: &mut EqSettings, name: &str) {
    if let Some(gains) = preset_gains(name) {
        eq.gains_db = gains;
        eq.preamp_db = safe_preamp(&gains);
        eq.preset = name.to_owned();
    }
}

/// Clamps gains into range and replaces anything that isn't a number (a hand-edited settings file).
pub fn sanitize(eq: &mut EqSettings) {
    let clamp = |v: f32, range: &std::ops::RangeInclusive<f32>| if v.is_finite() { v.clamp(*range.start(), *range.end()) } else { 0.0 };
    for g in &mut eq.gains_db {
        *g = clamp(*g, &GAIN_RANGE);
    }
    eq.preamp_db = clamp(eq.preamp_db, &PREAMP_RANGE);
}

/// "+3.5 dB", "0 dB", "-6 dB".
pub fn fmt_db(v: f32) -> String {
    let v = (v * 2.0).round() / 2.0;
    if v.abs() < 0.05 {
        "0 dB".to_owned()
    } else if (v - v.round()).abs() < 0.01 {
        format!("{:+} dB", v.round() as i32)
    } else {
        format!("{v:+.1} dB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_the_bands() {
        assert_eq!(band_labels(), ["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"].map(String::from));
    }

    #[test]
    fn presets_are_in_range_and_unique() {
        for (i, (name, gains)) in PRESETS.iter().enumerate() {
            assert!(gains.iter().all(|g| GAIN_RANGE.contains(g)), "{name}");
            assert!(PRESETS.iter().skip(i + 1).all(|(other, _)| other != name));
            assert_ne!(*name, CUSTOM);
        }
    }

    #[test]
    fn preset_sets_a_safe_preamp() {
        let mut eq = EqSettings::default();
        apply_preset(&mut eq, "Bass boost");
        assert_eq!(eq.preset, "Bass boost");
        assert_eq!(eq.preamp_db, -6.0);
        assert_eq!(preset_label(&eq), "Bass boost");
        apply_preset(&mut eq, "Bass reducer");
        assert_eq!(eq.preamp_db, 0.0);
        apply_preset(&mut eq, "No such preset");
        assert_eq!(eq.preset, "Bass reducer");
        eq.preset = "something old".into();
        assert_eq!(preset_label(&eq), CUSTOM);
        assert_eq!(safe_preamp(&[3.2; 10]), -3.5);
        assert_eq!(safe_preamp(&[40.0; 10]), -12.0);
    }

    #[test]
    fn sanitizes_bad_values() {
        let mut eq = EqSettings { gains_db: [f32::NAN, 99.0, -99.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], preamp_db: 5.0, ..Default::default() };
        sanitize(&mut eq);
        assert_eq!(&eq.gains_db[..4], &[0.0, 12.0, -12.0, 1.0]);
        assert_eq!(eq.preamp_db, 0.0);
    }

    #[test]
    fn formats_decibels() {
        assert_eq!(fmt_db(0.0), "0 dB");
        assert_eq!(fmt_db(3.0), "+3 dB");
        assert_eq!(fmt_db(-2.5), "-2.5 dB");
        assert_eq!(fmt_db(-0.01), "0 dB");
    }
}
