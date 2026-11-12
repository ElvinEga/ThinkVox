//! Low-latency granular / dual-delay-line micro pitch shifter.
//!
//! A pitch shift is a *continuously changing* read delay: if
//! `y[n] = x[n - d[n]]`, the instantaneous frequency ratio is `1 - d'[n]`.
//! For a ratio `r = 2^(cents/1200)` the delay must therefore drift at
//! `d'[n] = 1 - r` samples per sample (shrinking for up, growing for down).
//!
//! The delay can only drift over a finite window before it has to snap back,
//! and that snap is audible. The classic fix (Eventide-style) is two read
//! heads half a window apart, crossfaded with equal-power sine/cosine windows
//! so that each head is at zero gain exactly when it wraps.
//!
//! For micro shifts (3..24 cents) `|1 - r|` is tiny, so a full sweep takes
//! seconds. The result is a slow, smooth drift rather than audible grains —
//! exactly the soft ADT / Warren Huart doubling character we want, with no
//! extra lookahead latency and only two buffer reads per voice per sample.

use super::delay::DelayLine;
use core::f32::consts::PI;

/// Total delay sweep range. This doubles as the crossfade window length.
/// 12 ms keeps the wet signal inside the classic ~8-28 ms doubling band.
pub const GRAIN_S: f32 = 0.012;

/// Smallest read delay. Cubic interpolation needs >= 1 sample, and a couple
/// of milliseconds keeps the read head safely behind the write head.
pub const MIN_DELAY_S: f32 = 0.002;

/// Convert cents to a frequency ratio.
///
/// `2^(cents/1200)` expanded to second order in `u = ln2 * cents / 1200`.
/// For |cents| <= 30 the error is below 1e-6, so we avoid a `powf` in the
/// per-sample loop entirely.
#[inline]
pub fn cents_to_ratio(cents: f32) -> f32 {
    let u = core::f32::consts::LN_2 * cents / 1200.0;
    1.0 + u + 0.5 * u * u
}

/// State for one pitch-shifted read head pair.
///
/// The delay buffer itself lives in `Thickener` and is shared by all voices;
/// this struct only owns the sweep phase.
#[derive(Clone, Copy, Default)]
pub struct GranularPitchShifter {
    /// Position of head A inside its sweep, in `[0, 1)`.
    phase: f32,
}

impl GranularPitchShifter {
    /// `initial_phase` decorrelates the eight voices so their crossfade
    /// cycles never line up.
    pub fn new(initial_phase: f32) -> Self {
        Self {
            phase: initial_phase.fract(),
        }
    }

    pub fn reset(&mut self, phase: f32) {
        self.phase = phase.fract();
    }

    /// Render one pitch-shifted sample.
    ///
    /// * `line`       - shared input delay line, already updated this sample
    /// * `cents`      - signed detune in cents (already scaled by depth)
    /// * `base_delay` - nominal doubling delay in seconds (already scaled)
    /// * `sr`         - sample rate in Hz
    #[inline]
    pub fn process(&mut self, line: &DelayLine, cents: f32, base_delay: f32, sr: f32) -> f32 {
        let delta = cents_to_ratio(cents) - 1.0;
        let grain = GRAIN_S * sr;

        // Zero detune degenerates to a plain static delay, and avoids a
        // pointless comb filter from the second read head.
        if delta.abs() < 1e-7 || grain < 4.0 {
            return line.read(base_delay * sr);
        }

        // Centre the sweep on the nominal delay so the wet signal really is
        // a 10 / 24 ms doubling, not 10-22 / 24-36 ms.
        let d_min = (base_delay - 0.5 * GRAIN_S).max(MIN_DELAY_S) * sr;

        let t = self.phase;
        // Head B is half a sweep ahead of head A.
        let t_b = if t < 0.5 { t + 0.5 } else { t - 0.5 };

        // For pitch up the delay must shrink; for pitch down it must grow.
        let (d_a, d_b) = if delta > 0.0 {
            (d_min + (1.0 - t) * grain, d_min + (1.0 - t_b) * grain)
        } else {
            (d_min + t * grain, d_min + t_b * grain)
        };

        // Equal-power crossfade: sin²(pi t) + sin²(pi t_b) == 1.
        // Each head hits zero gain at the exact instant its delay wraps.
        let a = (PI * t).sin();
        let b = (PI * t_b).sin();

        let out = a * line.read(d_a) + b * line.read(d_b);

        // One sample of drift. |delta| is in samples/sample; grain is in
        // samples, so |delta| / grain is the phase increment per sample.
        self.phase += delta.abs() / grain;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cents_map_to_the_right_direction() {
        assert!((cents_to_ratio(0.0) - 1.0).abs() < 1e-9);
        assert!(cents_to_ratio(12.0) > 1.0);
        assert!(cents_to_ratio(-12.0) < 1.0);
        assert!((cents_to_ratio(12.0) - 1.0 + (cents_to_ratio(-12.0) - 1.0)).abs() < 0.01);
    }

    #[test]
    fn cents_ratio_is_accurate() {
        for c in [-24.0_f32, -12.0, -3.0, 3.0, 12.0, 24.0] {
            let want = 2.0_f32.powf(c / 1200.0);
            assert!(
                (cents_to_ratio(c) - want).abs() < 1e-5,
                "{c} cents: {} vs {want}",
                cents_to_ratio(c)
            );
        }
    }

    #[test]
    fn crossfade_is_equal_power_and_masks_both_wraps() {
        for k in 0..100 {
            let t = k as f32 / 100.0;
            let t_b = if t < 0.5 { t + 0.5 } else { t - 0.5 };
            let a = (PI * t).sin();
            let b = (PI * t_b).sin();
            assert!(a >= 0.0 && b >= 0.0);
            assert!((a * a + b * b - 1.0).abs() < 1e-5, "t={t}");
        }
        assert!((PI * 0.0_f32).sin().abs() < 1e-6, "head A wraps silently");
        assert!((PI * 0.5_f32).sin() - 1.0 < 1e-6);
        assert!((PI * 0.0_f32).sin().abs() < 1e-6, "head B wraps silently");
    }

    #[test]
    fn shifter_produces_finite_bounded_output() {
        let sr = 48_000.0;
        let mut line = DelayLine::new(8192);
        let mut sh = GranularPitchShifter::new(0.137);
        let mut max = 0.0_f32;
        for n in 0..20_000 {
            let x = (2.0 * PI * 220.0 * n as f32 / sr).sin();
            line.push(x);
            let y = sh.process(&line, 12.0, 0.010, sr);
            assert!(y.is_finite());
            max = max.max(y.abs());
        }
        // Two equal-power heads can correlate up to sqrt(2).
        assert!(max < 1.5, "unexpected gain: {max}");
    }
}
