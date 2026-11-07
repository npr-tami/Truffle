//! The 10-band graphic equalizer applied in Truffle's own audio output.
//!
//! - RBJ "Audio EQ Cookbook" biquads at the audio's rate (librespot's fixed 44.1 kHz, or a YouTube
//!   song's own): a low shelf at 31 Hz, peaking filters for 62 Hz..8 kHz and a high shelf at
//!   16 kHz, after a preamp.
//! - Processing is in place on the decoder's f64 samples, per channel, with no allocation.
//! - Bands at 0 dB are skipped. Disabled or completely flat settings cost nothing at all.
//! - A settings change never clicks: the old and the new filters run side by side for 20 ms while
//!   the output crossfades from one to the other.
//! - A stereo-linked peak limiter after the filters keeps boosts from clipping. It runs whenever
//!   the equalizer does (and until it has fully recovered after the equalizer is switched off).

use std::f64::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::model::{EQ_BANDS_HZ, EqSettings};

const BANDS: usize = EQ_BANDS_HZ.len();
const CHANNELS: usize = 2;
/// librespot always outputs 44.1 kHz stereo (`librespot_playback::SAMPLE_RATE`).
pub(super) const RATE: f64 = librespot_playback::SAMPLE_RATE as f64;

/// Q of the peaking bands: a bit under one octave wide, so neighbouring bands blend smoothly.
const PEAK_Q: f64 = 1.2;
/// Shelf slope (1 = steepest without overshoot).
const SHELF_SLOPE: f64 = 1.0;
/// Gains within this of 0 dB count as flat (the band is skipped).
const FLAT_DB: f32 = 0.05;
pub(super) const MAX_BAND_DB: f32 = 12.0;
pub(super) const MIN_PREAMP_DB: f32 = -12.0;
/// Old and new filters are crossfaded over this long (in seconds) when the settings change.
const CHANGE_SECS: f64 = 0.02;
/// The limiter keeps peaks at or below this (about -0.3 dBFS).
const LIMIT: f64 = 0.966;
/// Time for the limiter to recover by about 63 % after a peak.
const LIMITER_RELEASE_SECS: f64 = 0.1;
/// Filter state below this is flushed to zero after each packet (avoids slow denormals in silence).
const DENORMAL: f64 = 1e-20;

/// Clamp equalizer settings to the supported ranges (non-numbers become 0 dB).
pub fn sanitize(settings: &EqSettings) -> EqSettings {
    let clean = |db: f32, min: f32, max: f32| if db.is_finite() { db.clamp(min, max) } else { 0.0 };
    let mut out = settings.clone();
    for gain in &mut out.gains_db {
        *gain = clean(*gain, -MAX_BAND_DB, MAX_BAND_DB);
    }
    out.preamp_db = clean(out.preamp_db, MIN_PREAMP_DB, 0.0);
    out
}

/// The numbers the filters are built from (no allocation to copy, unlike `EqSettings`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct EqParams {
    enabled: bool,
    gains_db: [f32; BANDS],
    preamp_db: f32,
}

impl From<&EqSettings> for EqParams {
    fn from(settings: &EqSettings) -> Self {
        let clean = sanitize(settings);
        Self { enabled: clean.enabled, gains_db: clean.gains_db, preamp_db: clean.preamp_db }
    }
}

/// Equalizer settings shared by the engine (writer) and every output (reader).
#[derive(Debug)]
pub(super) struct EqControl {
    generation: AtomicU64,
    params: Mutex<EqParams>,
}

impl EqControl {
    pub(super) fn new(settings: &EqSettings) -> Self {
        Self { generation: AtomicU64::new(1), params: Mutex::new(EqParams::from(settings)) }
    }

    pub(super) fn set(&self, settings: &EqSettings) {
        let params = EqParams::from(settings);
        let mut current = self.params.lock().unwrap_or_else(PoisonError::into_inner);
        if *current != params {
            *current = params;
            self.generation.fetch_add(1, Ordering::Release);
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn params(&self) -> EqParams {
        *self.params.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------------------------
// Filter design

/// Biquad coefficients, normalised so a0 = 1.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coeffs {
    const IDENTITY: Self = Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn normalised(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    fn peaking(freq: f64, gain_db: f64, q: f64, rate: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        Self::normalised(1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
    }

    fn shelf(freq: f64, gain_db: f64, slope: f64, rate: f64, high: bool) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / 2.0 * ((a + 1.0 / a) * (1.0 / slope - 1.0) + 2.0).sqrt();
        let k = 2.0 * a.sqrt() * alpha;
        // The high shelf is the low shelf with the sign of cos (and b1/a1) flipped.
        let s = if high { -1.0 } else { 1.0 };
        Self::normalised(
            a * ((a + 1.0) - s * (a - 1.0) * cos + k),
            s * 2.0 * a * ((a - 1.0) - s * (a + 1.0) * cos),
            a * ((a + 1.0) - s * (a - 1.0) * cos - k),
            (a + 1.0) + s * (a - 1.0) * cos + k,
            -s * 2.0 * ((a - 1.0) + s * (a + 1.0) * cos),
            (a + 1.0) + s * (a - 1.0) * cos - k,
        )
    }

    /// The filter for one band of the graphic equalizer.
    fn band(index: usize, gain_db: f32, rate: f64) -> Self {
        let freq = f64::from(EQ_BANDS_HZ[index]);
        let gain = f64::from(gain_db);
        if index == 0 {
            Self::shelf(freq, gain, SHELF_SLOPE, rate, false)
        } else if index == BANDS - 1 {
            Self::shelf(freq, gain, SHELF_SLOPE, rate, true)
        } else {
            Self::peaking(freq, gain, PEAK_Q, rate)
        }
    }

    /// Transposed direct form II: one multiply-add chain, good numerical behaviour in f64.
    #[inline(always)]
    fn run(&self, state: &mut [f64; 2], x: f64) -> f64 {
        let y = self.b0 * x + state[0];
        state[0] = self.b1 * x - self.a1 * y + state[1];
        state[1] = self.b2 * x - self.a2 * y;
        y
    }

    /// Gain in dB at `freq` (for tests and checks).
    #[cfg(test)]
    fn response_db(&self, freq: f64, rate: f64) -> f64 {
        let w = 2.0 * PI * freq / rate;
        // H(z) at z = e^{jw}: (b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2)
        let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
        let (nr, ni) = (self.b0 + self.b1 * c1 + self.b2 * c2, self.b1 * s1 + self.b2 * s2);
        let (dr, di) = (1.0 + self.a1 * c1 + self.a2 * c2, self.a1 * s1 + self.a2 * s2);
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).log10()
    }
}

/// A preamp plus the bands that are not flat, with their per-channel state.
#[derive(Debug, Clone)]
struct Bank {
    preamp: f64,
    coeffs: [Coeffs; BANDS],
    /// Band indices in use (the first `count`).
    used: [usize; BANDS],
    count: usize,
    state: [[[f64; 2]; BANDS]; CHANNELS],
}

impl Bank {
    /// `None` when the settings leave the sound untouched (disabled, or all flat with no preamp).
    fn design(params: &EqParams, rate: f64) -> Option<Self> {
        if !params.enabled {
            return None;
        }
        let mut bank = Self {
            preamp: 10f64.powf(f64::from(params.preamp_db) / 20.0),
            coeffs: [Coeffs::IDENTITY; BANDS],
            used: [0; BANDS],
            count: 0,
            state: [[[0.0; 2]; BANDS]; CHANNELS],
        };
        for (index, &gain) in params.gains_db.iter().enumerate() {
            if gain.abs() >= FLAT_DB {
                bank.coeffs[index] = Coeffs::band(index, gain, rate);
                bank.used[bank.count] = index;
                bank.count += 1;
            }
        }
        if bank.count == 0 && params.preamp_db.abs() < FLAT_DB {
            return None;
        }
        Some(bank)
    }

    /// Carry over the filter memory of bands both banks use, so the new filters start "warm".
    fn continue_from(&mut self, previous: &Bank) {
        for &band in &self.used[..self.count] {
            if previous.used[..previous.count].contains(&band) {
                for channel in 0..CHANNELS {
                    self.state[channel][band] = previous.state[channel][band];
                }
            }
        }
    }

    #[inline(always)]
    fn process(&mut self, channel: usize, x: f64) -> f64 {
        let mut y = x * self.preamp;
        let state = &mut self.state[channel];
        for &band in &self.used[..self.count] {
            y = self.coeffs[band].run(&mut state[band], y);
        }
        y
    }

    fn flush_denormals(&mut self) {
        for value in self.state.iter_mut().flatten().flatten() {
            if value.abs() < DENORMAL {
                *value = 0.0;
            }
        }
    }
}

/// Stereo-linked peak limiter: peaks above `KNEE` are bent smoothly towards `LIMIT` (never past
/// it), and the gain then recovers over about `LIMITER_RELEASE_SECS`. No lookahead, so it adds no
/// latency; it is a guard against clipping, not a loudness tool.
#[derive(Debug, Clone)]
struct Limiter {
    gain: f64,
    release: f64,
}

/// Peaks up to this pass unchanged.
const KNEE: f64 = 0.8;
/// The gain is snapped back to exactly 1 once it is this close (about -0.001 dB).
const LIMITER_SNAP: f64 = 1e-4;

impl Limiter {
    fn new(rate: f64) -> Self {
        Self { gain: 1.0, release: (-1.0 / (LIMITER_RELEASE_SECS * rate)).exp() }
    }

    fn idle(&self) -> bool {
        self.gain >= 1.0
    }

    /// The gain that brings a frame peaking at `peak` under the soft knee curve.
    fn ceiling(peak: f64) -> f64 {
        if peak <= KNEE {
            return 1.0;
        }
        let room = LIMIT - KNEE;
        // `min`: once tanh rounds to 1 the sum can land a hair above the limit.
        ((KNEE + room * ((peak - KNEE) / room).tanh()) / peak).min(LIMIT / peak)
    }

    #[inline(always)]
    fn process(&mut self, frame: &mut [f64]) {
        let peak = frame.iter().fold(0.0f64, |m, s| m.max(s.abs()));
        // Recover towards 1, but never above what this frame allows.
        let recovered = 1.0 - (1.0 - self.gain) * self.release;
        self.gain = recovered.min(Self::ceiling(peak));
        if self.gain > 1.0 - LIMITER_SNAP {
            self.gain = 1.0;
            return;
        }
        for sample in frame.iter_mut() {
            *sample *= self.gain;
        }
    }
}

/// The per-output equalizer: current filters, the filters being faded out after a change, and
/// the limiter. Owned by one output; reads the shared [`EqControl`] once per packet.
#[derive(Debug)]
pub(super) struct Equalizer {
    rate: f64,
    /// Frames in a crossfade between old and new filters.
    change_frames: u32,
    params: EqParams,
    generation: u64,
    current: Option<Bank>,
    /// The filters before the last change, still audible for `fade_left` more frames.
    previous: Option<Bank>,
    fade_left: u32,
    limiter: Limiter,
}

impl Equalizer {
    pub(super) fn new(control: &EqControl) -> Self {
        Self::with_rate(control, RATE)
    }

    /// An equalizer for audio at `rate` samples per second.
    pub(super) fn with_rate(control: &EqControl, rate: f64) -> Self {
        let generation = control.generation();
        let params = control.params();
        Self {
            rate,
            change_frames: (rate * CHANGE_SECS).round() as u32,
            params,
            generation,
            current: Bank::design(&params, rate),
            previous: None,
            fade_left: 0,
            limiter: Limiter::new(rate),
        }
    }

    #[cfg(test)]
    pub(super) fn rate(&self) -> f64 {
        self.rate
    }

    /// True while samples pass through untouched (so `process` costs nothing).
    pub(super) fn is_bypassed(&self) -> bool {
        // `fade_left` too: a crossfade only counts down in `process`, and `sync` waits for it.
        self.current.is_none() && self.previous.is_none() && self.fade_left == 0 && self.limiter.idle()
    }

    /// Forget all filter memory, as if nothing had been played: the output was stopped, and what
    /// plays next (another track, another position) must not start with the old one's ringing.
    pub(super) fn reset(&mut self) {
        if let Some(bank) = self.current.as_mut() {
            bank.state = [[[0.0; 2]; BANDS]; CHANNELS];
        }
        self.previous = None;
        self.fade_left = 0;
        self.limiter.gain = 1.0;
    }

    /// Pick up new settings (cheap when nothing changed). A change made while the previous one is
    /// still being crossfaded waits for that to finish, so the output never jumps.
    pub(super) fn sync(&mut self, control: &EqControl) {
        if self.fade_left > 0 {
            return;
        }
        let generation = control.generation();
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        let params = control.params();
        if params != self.params {
            self.apply(params);
        }
    }

    fn apply(&mut self, params: EqParams) {
        self.params = params;
        let mut next = Bank::design(&params, self.rate);
        if let (Some(next), Some(current)) = (next.as_mut(), self.current.as_ref()) {
            next.continue_from(current);
        }
        self.previous = std::mem::replace(&mut self.current, next);
        // Fade from the old output (filters, or the untouched signal) to the new one. Nothing to
        // fade when neither side touches the sound (switched on while flat, or off while flat).
        self.fade_left = if self.previous.is_some() || self.current.is_some() { self.change_frames } else { 0 };
        log::debug!("equalizer: {}", if self.current.is_some() { "updated" } else { "off" });
    }

    /// Equalize interleaved stereo samples in place. Never allocates.
    pub(super) fn process(&mut self, samples: &mut [f64]) {
        if self.is_bypassed() {
            return;
        }
        let (frames, _) = samples.as_chunks_mut::<CHANNELS>();
        for frame in frames {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let x = *sample;
                let wet = match self.current.as_mut() {
                    Some(bank) => bank.process(channel, x),
                    None => x,
                };
                *sample = if self.fade_left > 0 {
                    let old = match self.previous.as_mut() {
                        Some(bank) => bank.process(channel, x),
                        None => x,
                    };
                    let mix = 1.0 - f64::from(self.fade_left) / f64::from(self.change_frames);
                    old + (wet - old) * mix
                } else {
                    wet
                };
            }
            if self.fade_left > 0 {
                self.fade_left -= 1;
                if self.fade_left == 0 {
                    self.previous = None;
                }
            }
            self.limiter.process(frame);
        }
        if let Some(bank) = self.current.as_mut() {
            bank.flush_denormals();
        }
        if let Some(bank) = self.previous.as_mut() {
            bank.flush_denormals();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(gains_db: [f32; BANDS], preamp_db: f32) -> EqSettings {
        EqSettings { enabled: true, gains_db, preamp_db, preset: "Custom".into() }
    }

    fn one_band(index: usize, db: f32) -> EqSettings {
        let mut gains = [0.0; BANDS];
        gains[index] = db;
        settings(gains, 0.0)
    }

    /// A stereo sine at `freq`, both channels equal.
    fn sine(freq: f64, amplitude: f64, frames: usize) -> Vec<f64> {
        (0..frames)
            .flat_map(|n| {
                let s = amplitude * (2.0 * PI * freq * n as f64 / RATE).sin();
                [s, s]
            })
            .collect()
    }

    fn peak(samples: &[f64]) -> f64 {
        samples.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn peaking_bands_hit_the_requested_gain_at_their_centre() {
        for (index, &centre) in EQ_BANDS_HZ.iter().enumerate().take(BANDS - 1).skip(1) {
            for db in [-12.0f32, -3.0, 4.5, 12.0] {
                let c = Coeffs::band(index, db, RATE);
                let measured = c.response_db(f64::from(centre), RATE);
                assert!((measured - f64::from(db)).abs() < 0.01, "band {index} {db} dB measured {measured:.3}");
                // Two octaves away the band has much less effect.
                let far = c.response_db(f64::from(centre) / 4.0, RATE);
                assert!(far.abs() < f64::from(db.abs()) * 0.25, "band {index}: {far:.2} dB two octaves down");
            }
        }
    }

    #[test]
    fn shelves_boost_their_end_of_the_spectrum() {
        let low = Coeffs::band(0, 9.0, RATE);
        assert!((low.response_db(5.0, RATE) - 9.0).abs() < 0.2, "full gain well below 31 Hz");
        assert!((low.response_db(31.0, RATE) - 4.5).abs() < 0.2, "half the gain at the corner");
        assert!(low.response_db(1000.0, RATE).abs() < 0.05, "midrange untouched");
        let high = Coeffs::band(BANDS - 1, -6.0, RATE);
        assert!((high.response_db(21_000.0, RATE) + 6.0).abs() < 0.6, "full cut near the top");
        assert!((high.response_db(16_000.0, RATE) + 3.0).abs() < 0.3, "half the cut at the corner");
        assert!(high.response_db(1000.0, RATE).abs() < 0.05, "midrange untouched");
    }

    #[test]
    fn processing_a_sine_matches_the_design() {
        let control = EqControl::new(&one_band(5, 6.0));
        let mut eq = Equalizer::new(&control);
        let mut samples = sine(1000.0, 0.25, 44_100);
        eq.process(&mut samples);
        // After the filter settled, 1 kHz comes out 6 dB (x2) louder.
        let settled = peak(&samples[44_100..]);
        assert!((settled / 0.25 - 2.0).abs() < 0.02, "gain {}", settled / 0.25);
    }

    #[test]
    fn flat_or_disabled_settings_are_an_exact_bypass() {
        for settings in [EqSettings::default(), settings([0.0; BANDS], 0.0), EqSettings { enabled: false, ..one_band(3, 9.0) }] {
            let control = EqControl::new(&settings);
            let mut eq = Equalizer::new(&control);
            assert!(eq.is_bypassed());
            let original = sine(440.0, 0.9, 2048);
            let mut samples = original.clone();
            eq.process(&mut samples);
            assert_eq!(samples, original);
        }
    }

    #[test]
    fn boosts_never_exceed_the_limit() {
        let control = EqControl::new(&settings([12.0; BANDS], 0.0));
        let mut eq = Equalizer::new(&control);
        for freq in [40.0, 250.0, 3000.0, 15_000.0] {
            let mut samples = sine(freq, 0.99, 8192);
            eq.process(&mut samples);
            assert!(peak(&samples) <= LIMIT + 1e-12, "{freq} Hz peaked at {}", peak(&samples));
        }
    }

    #[test]
    fn the_limiter_bends_peaks_and_then_releases() {
        // Below the knee nothing changes; above it the curve rises smoothly towards the limit.
        assert_eq!(Limiter::ceiling(KNEE), 1.0);
        let mut last = KNEE;
        for i in 1..=400 {
            let peak = KNEE + f64::from(i) * 0.01;
            let out = peak * Limiter::ceiling(peak);
            assert!(out > last - 1e-12 && out <= LIMIT + 1e-12, "{peak} -> {out}");
            last = out;
        }
        let mut limiter = Limiter::new(RATE);
        let mut frame = [2.0, -2.0];
        limiter.process(&mut frame);
        assert!(frame[0] <= LIMIT && frame[0] > LIMIT - 0.01, "{frame:?}");
        for _ in 0..(RATE as usize) {
            limiter.process(&mut [0.1, 0.1]);
        }
        assert!(limiter.idle(), "back to full gain after a second: {}", limiter.gain);
    }

    #[test]
    fn changes_crossfade_without_jumps() {
        let control = EqControl::new(&EqSettings::default());
        let mut eq = Equalizer::new(&control);
        let amplitude = 0.3;
        let input = sine(100.0, amplitude, 44_100);
        let mut output = Vec::with_capacity(input.len());
        for (i, packet) in input.chunks(2048).enumerate() {
            // Switch on, change twice, then switch off again while audio flows.
            match i {
                3 => control.set(&one_band(2, 12.0)),
                4 => control.set(&one_band(2, -12.0)),
                10 => control.set(&EqSettings::default()),
                _ => {}
            }
            eq.sync(&control);
            let mut packet = packet.to_vec();
            eq.process(&mut packet);
            output.extend_from_slice(&packet);
        }
        // A 100 Hz sine at this level moves at most ~0.0043 per sample; boosted by up to 4x that
        // is ~0.017. A click would be a far bigger step.
        let (frames, _) = output.as_chunks::<2>();
        let biggest_step = frames.windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0.0, f64::max);
        assert!(biggest_step < 0.03, "largest sample-to-sample step {biggest_step}");
        // Once the limiter has recovered, switched-off means bypassed again.
        let mut more = sine(100.0, amplitude, 44_100);
        eq.process(&mut more);
        assert!(eq.is_bypassed(), "fully off again at the end");
    }

    /// Feeds `packets` packets of a 62 Hz sine through `eq` (picking up settings first) and returns
    /// the peak of the last one.
    fn play_bass(eq: &mut Equalizer, control: &EqControl, packets: usize) -> f64 {
        let mut last = 0.0;
        for packet in sine(62.0, 0.25, 2048 * packets).chunks(2 * 2048) {
            eq.sync(control);
            let mut packet = packet.to_vec();
            eq.process(&mut packet);
            last = peak(&packet);
        }
        last
    }

    #[test]
    fn switching_on_while_flat_still_takes_later_changes() {
        // The first-use path: ticking "Equalizer" (all bands still flat), then picking a preset.
        let control = EqControl::new(&EqSettings::default());
        let mut eq = Equalizer::new(&control);
        control.set(&settings([0.0; BANDS], 0.0));
        let untouched = peak(&sine(62.0, 0.25, 3 * 2048)[2 * 2 * 2048..]);
        assert_eq!(play_bass(&mut eq, &control, 3), untouched, "flat leaves the sound alone");
        assert!(eq.is_bypassed());
        control.set(&one_band(1, 12.0));
        let boosted = play_bass(&mut eq, &control, 20);
        assert!(boosted > 0.5, "62 Hz boosted by 12 dB, got peak {boosted}");
        // Off while flat and on again (flat) are no-ops too, and the next change still applies.
        control.set(&EqSettings::default());
        play_bass(&mut eq, &control, 20);
        control.set(&settings([0.0; BANDS], 0.0));
        control.set(&EqSettings { enabled: false, ..settings([0.0; BANDS], 0.0) });
        play_bass(&mut eq, &control, 3);
        control.set(&one_band(1, -12.0));
        let cut = play_bass(&mut eq, &control, 20);
        assert!(cut < 0.1, "62 Hz cut by 12 dB, got peak {cut}");
    }

    #[test]
    fn reset_forgets_the_previous_audio() {
        let mut gains = [0.0; BANDS];
        gains[..5].copy_from_slice(&[6.0, 5.0, 4.0, 2.5, 1.0]);
        let control = EqControl::new(&settings(gains, -6.0));
        let mut eq = Equalizer::new(&control);
        // Bass-heavy audio, then (after a stop) a track that starts in digital silence.
        let mut song = sine(50.0, 0.3, 44_100 + 300);
        eq.process(&mut song);
        let mut ringing = vec![0.0; 64];
        let mut cold = ringing.clone();
        let mut kept = Equalizer::new(&control);
        kept.process(&mut sine(50.0, 0.3, 44_100 + 300));
        kept.process(&mut ringing);
        assert!(ringing[0].abs() > 0.01, "without a reset the old filter rings on: {}", ringing[0]);
        eq.reset();
        eq.process(&mut cold);
        assert!(cold.iter().all(|&s| s == 0.0), "{:?}", &cold[..4]);
        assert!(!eq.is_bypassed(), "still equalizing");
    }

    #[test]
    fn settings_are_sanitized() {
        let wild = EqSettings { enabled: true, gains_db: [f32::NAN, 40.0, -40.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, f32::INFINITY], preamp_db: 6.0, preset: "x".into() };
        let clean = sanitize(&wild);
        assert_eq!(clean.gains_db[..4], [0.0, MAX_BAND_DB, -MAX_BAND_DB, 3.0]);
        assert_eq!(clean.gains_db[9], 0.0);
        assert_eq!(clean.preamp_db, 0.0);
        assert_eq!(sanitize(&EqSettings { preamp_db: -30.0, ..wild }).preamp_db, MIN_PREAMP_DB);
    }

    #[test]
    fn processing_does_not_allocate() {
        let control = EqControl::new(&settings([6.0, -3.0, 2.0, 0.0, 1.0, -4.0, 3.0, 5.0, -2.0, 8.0], -3.0));
        let mut eq = Equalizer::new(&control);
        let mut packet = sine(523.0, 0.8, 1024);
        // Warm up, including a settings change in flight.
        eq.process(&mut packet);
        control.set(&one_band(4, 3.0));
        let before = alloc_counter::allocations();
        for _ in 0..50 {
            eq.sync(&control);
            eq.process(&mut packet);
        }
        assert_eq!(alloc_counter::allocations() - before, 0, "allocations while equalizing");
    }

    /// Counts heap allocations made by the current thread (test builds only).
    mod alloc_counter {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
        }

        pub(super) fn allocations() -> u64 {
            ALLOCATIONS.with(Cell::get)
        }

        fn count() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        }

        pub(super) struct Counting;

        // SAFETY: forwards every call unchanged to the system allocator; only counts.
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                count();
                // SAFETY: same contract as the caller's.
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                count();
                // SAFETY: same contract as the caller's.
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                count();
                // SAFETY: same contract as the caller's.
                unsafe { System.realloc(ptr, layout, new_size) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: same contract as the caller's.
                unsafe { System.dealloc(ptr, layout) }
            }
        }

        #[global_allocator]
        static GLOBAL: Counting = Counting;
    }
}
