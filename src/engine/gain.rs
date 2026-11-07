//! Click-free gain envelopes, applied while the audio plays (not when it is decoded).
//!
//! The output keeps about half a second of decoded audio queued. A gain that was applied while
//! decoding would therefore be heard half a second late, which would put the two halves of a
//! crossfade out of step. Instead each queued packet asks its [`OutputGain`] for the gain while it
//! is being played (see `output::PacketSource`), so a fade starts the moment it is set.
//!
//! An envelope is a [`Ramp`] from one gain to another over a duration, measured in wall-clock
//! time. Changing it mid-way starts the new ramp from wherever the old one had got to, so there is
//! never a jump. The audio thread only ever `try_lock`s the ramp (and skips the lock entirely while
//! the gain rests at 1.0), so it can never block on the engine. When the lock is busy it uses the
//! last gain it saw instead (see [`OutputGain::last_known`]).

use std::f32::consts::FRAC_PI_2;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The shape of a ramp between its two gains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Curve {
    /// Straight line in amplitude.
    Linear,
    /// Equal-power crossfade shape: a fade-in follows sin, a fade-out cos, so a fade-in and a
    /// fade-out running side by side keep the overall loudness steady.
    EqualPower,
    /// Straight line on the volume slider's cubic taper (amplitude = x³): sounds evenly paced.
    Perceptual,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Ramp {
    from: f32,
    to: f32,
    start: Instant,
    len: Duration,
    curve: Curve,
}

impl Ramp {
    fn hold(value: f32, now: Instant) -> Self {
        Self { from: value, to: value, start: now, len: Duration::ZERO, curve: Curve::Linear }
    }

    pub(super) fn new(from: f32, to: f32, start: Instant, len: Duration, curve: Curve) -> Self {
        Self { from: clamp_gain(from), to: clamp_gain(to), start, len, curve }
    }

    fn finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.len
    }

    pub(super) fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start);
        if elapsed >= self.len || self.from == self.to {
            return self.to;
        }
        let t = (elapsed.as_secs_f64() / self.len.as_secs_f64()) as f32;
        match self.curve {
            Curve::Linear => self.from + (self.to - self.from) * t,
            Curve::EqualPower => {
                // Scaled so a full 1 -> 0 ramp is cos and a full 0 -> 1 ramp is sin.
                let shaped = if self.to < self.from { 1.0 - (t * FRAC_PI_2).cos() } else { (t * FRAC_PI_2).sin() };
                self.from + (self.to - self.from) * shaped
            }
            Curve::Perceptual => {
                let (a, b) = (self.from.cbrt(), self.to.cbrt());
                let x = a + (b - a) * t;
                x * x * x
            }
        }
    }
}

/// Gains are amplitude factors in 0..=1 (anything else, including NaN, is clamped).
fn clamp_gain(gain: f32) -> f32 {
    if gain.is_nan() { 1.0 } else { gain.clamp(0.0, 1.0) }
}

/// One gain envelope, written by the engine and read by the audio thread.
#[derive(Debug)]
pub(super) struct GainControl {
    ramp: Mutex<Ramp>,
    /// The envelope rests at exactly 1.0: readers need not look at `ramp`.
    unity: AtomicBool,
    /// The gain last read or set (`f32` bits), for a reader that finds the lock busy.
    last: AtomicU32,
}

impl GainControl {
    pub(super) fn new(value: f32) -> Self {
        let value = clamp_gain(value);
        Self {
            ramp: Mutex::new(Ramp::hold(value, Instant::now())),
            unity: AtomicBool::new(value == 1.0),
            last: AtomicU32::new(value.to_bits()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Ramp> {
        self.ramp.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hold the envelope's lock (tests: makes the audio thread's reads fail).
    #[cfg(test)]
    pub(super) fn hold(&self) -> MutexGuard<'_, Ramp> {
        self.lock()
    }

    /// Move from the current gain to `target` over `len`, shaped by `curve`.
    pub(super) fn ramp_to(&self, target: f32, len: Duration, curve: Curve) {
        let target = clamp_gain(target);
        let now = Instant::now();
        let mut ramp = self.lock();
        let current = ramp.value_at(now);
        if current == target && ramp.finished(now) {
            return;
        }
        *ramp = if len.is_zero() { Ramp::hold(target, now) } else { Ramp::new(current, target, now, len, curve) };
        // The gain from this moment on, for a reader that comes while the lock is still held.
        let starts_at = if len.is_zero() { target } else { current };
        self.last.store(starts_at.to_bits(), Ordering::Relaxed);
        // Under the lock, so a reader can't mark it unity again in between.
        self.unity.store(target == 1.0 && len.is_zero(), Ordering::Release);
    }

    /// Jump to `value` at once (only for an output that is silent anyway).
    pub(super) fn set(&self, value: f32) {
        self.ramp_to(value, Duration::ZERO, Curve::Linear);
    }

    /// The gain right now (may wait for the lock).
    #[cfg(test)]
    pub(super) fn value(&self) -> f32 {
        if self.unity.load(Ordering::Acquire) {
            return 1.0;
        }
        self.lock().value_at(Instant::now())
    }

    pub(super) fn is_unity(&self) -> bool {
        self.unity.load(Ordering::Acquire)
    }

    /// The gain at `now` for the audio thread. Never blocks: `None` if the engine is changing the
    /// envelope this very moment (the caller keeps its previous gain for a moment).
    fn sample(&self, now: Instant) -> Option<f32> {
        let ramp = self.ramp.try_lock().ok()?;
        let value = ramp.value_at(now);
        self.last.store(value.to_bits(), Ordering::Relaxed);
        if value == 1.0 && ramp.finished(now) {
            self.unity.store(true, Ordering::Release);
        }
        Some(value)
    }

    /// The gain at `now` without looking at the ramp when it rests at 1.0.
    fn sample_unless_unity(&self, now: Instant) -> Option<f32> {
        if self.is_unity() { Some(1.0) } else { self.sample(now) }
    }

    /// The gain last read or set: never waits, never fails.
    fn last(&self) -> f32 {
        if self.is_unity() { 1.0 } else { f32::from_bits(self.last.load(Ordering::Relaxed)) }
    }
}

/// The gain one output (player) applies while playing: its own crossfade envelope times the
/// engine-wide fade (sleep timer) envelope.
#[derive(Debug)]
pub(super) struct OutputGain {
    /// Crossfade envelope of this output.
    pub(super) own: GainControl,
    /// Shared by all outputs.
    pub(super) global: Arc<GainControl>,
}

impl OutputGain {
    pub(super) fn new(global: Arc<GainControl>) -> Self {
        Self { own: GainControl::new(1.0), global }
    }

    /// The gain to play at now, or `None` to keep the previous one (see [`GainControl::sample`]).
    /// Costs two atomic loads while both envelopes rest at 1.0. An envelope at rest is never
    /// locked, so outputs crossfading with each other don't compete for the shared `global` one.
    pub(super) fn current(&self) -> Option<f32> {
        if self.own.is_unity() && self.global.is_unity() {
            return Some(1.0);
        }
        let now = Instant::now();
        Some(self.own.sample_unless_unity(now)? * self.global.sample_unless_unity(now)?)
    }

    /// The gain last seen or set, for when [`current`](Self::current) can't tell right now.
    pub(super) fn last_known(&self) -> f32 {
        self.own.last() * self.global.last()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn ramps_reach_their_endpoints() {
        let t0 = Instant::now();
        for curve in [Curve::Linear, Curve::EqualPower, Curve::Perceptual] {
            let down = Ramp::new(1.0, 0.0, t0, SECOND, curve);
            assert_eq!(down.value_at(t0), 1.0, "{curve:?}");
            assert_eq!(down.value_at(at(t0, 1000)), 0.0, "{curve:?}");
            assert_eq!(down.value_at(at(t0, 5000)), 0.0, "{curve:?}");
            let up = Ramp::new(0.0, 1.0, t0, SECOND, curve);
            assert_eq!(up.value_at(t0), 0.0, "{curve:?}");
            assert_eq!(up.value_at(at(t0, 1000)), 1.0, "{curve:?}");
            // Before the start it holds the starting gain.
            assert_eq!(up.value_at(t0 - Duration::from_millis(10)), 0.0, "{curve:?}");
        }
    }

    #[test]
    fn ramps_are_monotonic() {
        let t0 = Instant::now();
        for curve in [Curve::Linear, Curve::EqualPower, Curve::Perceptual] {
            let down = Ramp::new(1.0, 0.0, t0, SECOND, curve);
            let up = Ramp::new(0.2, 0.9, t0, SECOND, curve);
            let (mut last_down, mut last_up) = (f32::MAX, f32::MIN);
            for ms in (0..=1000).step_by(10) {
                let (d, u) = (down.value_at(at(t0, ms)), up.value_at(at(t0, ms)));
                assert!(d <= last_down && u >= last_up, "{curve:?} at {ms} ms");
                (last_down, last_up) = (d, u);
            }
        }
    }

    #[test]
    fn equal_power_crossfade_keeps_the_power_steady() {
        let t0 = Instant::now();
        let fade = Duration::from_secs(6);
        let out = Ramp::new(1.0, 0.0, t0, fade, Curve::EqualPower);
        let incoming = Ramp::new(0.0, 1.0, t0, fade, Curve::EqualPower);
        for ms in (0..=6000).step_by(100) {
            let (a, b) = (out.value_at(at(t0, ms)), incoming.value_at(at(t0, ms)));
            let power = a * a + b * b;
            assert!((power - 1.0).abs() < 1e-4, "power {power} at {ms} ms");
        }
    }

    #[test]
    fn perceptual_fade_follows_the_volume_taper() {
        let t0 = Instant::now();
        let fade = Ramp::new(1.0, 0.0, t0, SECOND, Curve::Perceptual);
        // Half way along the slider is 1/8 amplitude (about -18 dB), like the volume control.
        assert!((fade.value_at(at(t0, 500)) - 0.125).abs() < 1e-4);
    }

    #[test]
    fn retargeting_mid_ramp_starts_from_the_current_gain() {
        let control = GainControl::new(1.0);
        assert!(control.is_unity());
        control.ramp_to(0.0, Duration::from_secs(3600), Curve::Linear);
        assert!(!control.is_unity());
        let before = control.value();
        assert!(before > 0.99, "barely started: {before}");
        // Turning around goes back up from there, without a jump.
        control.ramp_to(1.0, Duration::from_secs(3600), Curve::Linear);
        let after = control.value();
        assert!((after - before).abs() < 0.01, "{before} -> {after}");
        assert!(!control.is_unity(), "still on its way back");
    }

    #[test]
    fn unity_is_detected_again_once_a_ramp_ends() {
        let global = Arc::new(GainControl::new(1.0));
        let output = OutputGain::new(global.clone());
        assert_eq!(output.current(), Some(1.0));
        output.own.set(0.5);
        assert_eq!(output.current(), Some(0.5));
        global.set(0.5);
        assert_eq!(output.current(), Some(0.25));
        output.own.ramp_to(1.0, Duration::from_millis(1), Curve::EqualPower);
        global.ramp_to(1.0, Duration::from_millis(1), Curve::Perceptual);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(output.current(), Some(1.0));
        // The reader noticed the rest state, so the next read skips the locks.
        assert!(output.own.is_unity() && global.is_unity());
    }

    #[test]
    fn a_busy_envelope_never_blocks_the_reader() {
        let control = GainControl::new(0.5);
        let held = control.lock();
        assert_eq!(control.sample(Instant::now()), None);
        drop(held);
        assert_eq!(control.sample(Instant::now()), Some(0.5));
    }

    #[test]
    fn an_envelope_at_rest_is_never_locked() {
        // Two outputs crossfading share `global`, which rests at 1.0: a reader must not need it.
        let global = Arc::new(GainControl::new(1.0));
        let output = OutputGain::new(global.clone());
        output.own.set(0.25);
        let held = global.hold();
        assert_eq!(output.current(), Some(0.25));
        drop(held);
        // A busy envelope that is not at rest: the last known gain stands in.
        let held = output.own.hold();
        assert_eq!(output.current(), None);
        assert_eq!(output.last_known(), 0.25);
        drop(held);
        global.set(0.5);
        assert_eq!(output.current(), Some(0.125));
        assert_eq!(output.last_known(), 0.125);
    }

    #[test]
    fn gains_are_clamped() {
        let control = GainControl::new(3.0);
        assert!(control.is_unity());
        control.set(f32::NAN);
        assert_eq!(control.value(), 1.0);
        control.set(-1.0);
        assert_eq!(control.value(), 0.0);
    }
}
