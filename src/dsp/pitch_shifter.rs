//! Stage 2: low-latency granular micro pitch shifter.
//!
//! # The physics
//!
//! A pitch shift is a *continuously drifting read delay*. If
//! `y[n] = x[n - d[n]]`, the instantaneous frequency ratio is `1 - d'[n]`.
//! For `r = 2^(cents/1200)` the delay must therefore drift at
//! `d'[n] = -(r - 1)` samples per sample: shrinking for pitch up, growing
//! for pitch down.
//!
//! # Why not the usual two-head crossfade
//!
//! The classic Eventide-style fix keeps **two** read heads half a window
//! apart and crossfades them continuously. For micro-shifts that is a bad
//! trade. The two heads are permanently 6..16 ms apart, so their sum is a
//! comb filter, and an equal-power crossfade of correlated material beats
//! against itself. Measured on the previous ThickVox implementation with a
//! 440 Hz sine at +12 cents:
//!
//! ```text
//!   RMS gain 110 Hz .. 7 kHz : 0.44 .. 0.91   (~6 dB of comb ripple)
//!   short-window RMS ripple   : ~50%
//! ```
//!
//! For |cents| <= 12 the drift is only 0.002..0.007 samples per sample, so a
//! single head needs to wrap only once every few seconds. This module keeps
//! **one active head** in steady state — pure resampling, flat frequency
//! response, no amplitude modulation — and only introduces a second head for
//! a short raised-cosine crossfade at the wrap. The replacement head is
//! positioned by a bounded normalized cross-correlation search (WSOLA-style)
//! so the two waveforms are in phase when summed, avoiding a cancellation
//! dip on voiced vocals.
//!
//! # Latency and real-time safety
//!
//! The head sweeps a 16 ms window, so the pitch stage contributes a mean
//! read-back of `min_offset + grain/2`. `DelayStage` subtracts exactly that
//! amount (see [`PitchConfig::mean_latency_ms`]), keeping the classic 10/24
//! ms wet offsets intact. Longer grains (32 ms) do not improve quality for
//! micro-shifts — they only double this latency — because steady-state
//! quality is set by the interpolation, not by grain length.
//!
//! All buffers and the fade table are built in [`PitchConfig::new`], off the
//! audio thread. [`MicroPitchShifter::process`] performs no allocation, no
//! locking, no `powf` and no trig; the expensive alignment search is cold,
//! bounded and runs only at a wrap.

use super::delay::DelayLine;

/// Read-head drift window in milliseconds. At 48 kHz this is 768 samples:
/// a 12-cent voice wraps about every 2.3 s, a 3-cent voice every ~9 s.
pub const GRAIN_MS: f32 = 16.0;

/// Crossfade length at a wrap, in milliseconds.
pub const FADE_MS: f32 = 6.0;

/// +/- search range for wrap alignment, in milliseconds. 4 ms covers half a
/// pitch period down to ~125 Hz, i.e. low male vocals.
pub const SEARCH_MS: f32 = 4.0;

/// Correlation window in milliseconds.
pub const CORR_MS: f32 = 3.0;

/// Keep the head a few samples behind the write pointer so it can continue
/// drifting through a whole fade without hitting the read clamp (which would
/// momentarily freeze the resampling and bend the pitch).
pub const MIN_OFFSET_SAMPLES: f32 = 8.0;

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

/// Sample-rate dependent, immutable-at-runtime configuration for the pitch
/// stage. Rebuilt in `Thickener::prepare` (never on the audio thread).
pub struct PitchConfig {
    /// Drift / grain window in samples.
    pub grain: f32,
    /// Lower bound of the head's read delay.
    pub min_offset: f32,
    /// Upper bound of the head's read delay.
    pub max_offset: f32,
    /// Raised-cosine 0..1 fade gains, one entry per fade sample.
    /// `(1 - fade[k]) + fade[k] == 1`, so aligned material sums to unity.
    pub fade: Vec<f32>,
    /// +/- correlation search radius in samples.
    pub search: i32,
    /// Correlation window length in samples.
    pub corr: usize,
    sr: f32,
}

impl PitchConfig {
    pub fn new(sr: f32) -> Self {
        let sr = sr.clamp(8_000.0, 384_000.0);
        let grain = (GRAIN_MS * 0.001 * sr).round().max(64.0);
        let fade_len = (FADE_MS * 0.001 * sr).round().max(8.0) as usize;
        let search = (SEARCH_MS * 0.001 * sr).round().max(8.0) as i32;
        let corr = (CORR_MS * 0.001 * sr).round().max(16.0) as usize;
        let min_offset = MIN_OFFSET_SAMPLES;
        let max_offset = min_offset + grain;

        let mut fade = Vec::with_capacity(fade_len);
        for k in 0..fade_len {
            let t = k as f32 / (fade_len - 1) as f32;
            fade.push(0.5 - 0.5 * (core::f32::consts::PI * t).cos());
        }

        Self {
            grain,
            min_offset,
            max_offset,
            fade,
            search,
            corr,
            sr,
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }

    /// Minimum capacity for each grain history. Covers the far head, the
    /// alignment search, the correlation window and interpolation margin.
    pub fn history_capacity(&self) -> usize {
        (self.max_offset + self.grain + self.search as f32 + self.corr as f32 + 128.0) as usize
    }

    /// Mean read-back of the pitch stage in milliseconds. The delay stage
    /// subtracts this so the wet offsets stay at 10 / 24 ms.
    pub fn mean_latency_ms(&self) -> f32 {
        1000.0 * (self.min_offset + 0.5 * self.grain) / self.sr
    }
}

/// One pitch-shifted read head. The delay history it reads from is owned by
/// `Thickener` (one per ADT delay group) and shared by the four voices in
/// that group; only the head state is per-voice.
#[derive(Clone, Copy)]
pub struct MicroPitchShifter {
    /// Active read delay, in samples.
    offset: f32,
    /// Replacement head delay; valid only while `fade_left > 0`.
    incoming: f32,
    /// Samples left in the wrap crossfade. 0 = single-head steady state.
    fade_left: usize,
    last_cents: f32,
    delta: f32,
    /// Wrap count, for diagnostics and tests.
    wraps: u32,
}

impl MicroPitchShifter {
    /// `index` decorrelates the eight heads so their wraps never coincide.
    /// `pitch_up` picks the end of the window the head starts near, giving
    /// every voice a long first sweep.
    pub fn new(index: usize, pitch_up: bool, cfg: &PitchConfig) -> Self {
        let f = (index as f32 + 0.5) / 8.0;
        let half = 0.5 * cfg.grain;
        let offset = if pitch_up {
            cfg.max_offset - f * half
        } else {
            cfg.min_offset + f * half
        };
        Self {
            offset,
            incoming: offset,
            fade_left: 0,
            last_cents: f32::NAN,
            delta: 0.0,
            wraps: 0,
        }
    }

    pub fn reset(&mut self, index: usize, pitch_up: bool, cfg: &PitchConfig) {
        *self = Self::new(index, pitch_up, cfg);
    }

    pub fn offset(&self) -> f32 {
        self.offset
    }

    pub fn is_fading(&self) -> bool {
        self.fade_left > 0
    }

    pub fn wraps(&self) -> u32 {
        self.wraps
    }

    /// Render one pitch-shifted sample from `hist`.
    ///
    /// `hist` must already have this sample's *delayed* input pushed into it.
    /// `cents` may change every sample (Detune Depth is smoothed upstream);
    /// the ratio conversion is a three-multiply Taylor expansion, not `powf`.
    #[inline]
    pub fn process(&mut self, hist: &DelayLine, cents: f32, cfg: &PitchConfig) -> f32 {
        if cents != self.last_cents {
            self.last_cents = cents;
            self.delta = cents_to_ratio(cents) - 1.0;
        }
        let delta = self.delta;

        // Both heads drift at the same rate so neither bends pitch during a
        // crossfade. delta == 0 is fine: constant offset, no wrap, no comb.
        self.offset -= delta;
        if self.fade_left > 0 {
            self.incoming -= delta;
        } else if self.offset < cfg.min_offset || self.offset > cfg.max_offset {
            self.start_wrap(hist, cfg);
        }

        let out = if self.fade_left > 0 {
            let k = cfg.fade.len() - self.fade_left;
            let w = cfg.fade[k];
            (1.0 - w) * hist.read(self.offset) + w * hist.read(self.incoming)
        } else {
            // The common case: one head, zero coloration.
            hist.read(self.offset)
        };

        if self.fade_left > 0 {
            self.fade_left -= 1;
            if self.fade_left == 0 {
                self.offset = self.incoming.clamp(cfg.min_offset, cfg.max_offset);
            }
        }

        out
    }

    /// Cold path: once every few seconds per voice.
    #[cold]
    #[inline(never)]
    fn start_wrap(&mut self, hist: &DelayLine, cfg: &PitchConfig) {
        let raw = if self.offset < cfg.min_offset {
            self.offset + cfg.grain
        } else {
            self.offset - cfg.grain
        };
        self.incoming = best_alignment(hist, self.offset, raw, cfg);
        self.fade_left = cfg.fade.len();
        self.wraps = self.wraps.wrapping_add(1);
    }
}

/// Find the lag near `raw_new` whose surrounding waveform best matches the
/// waveform around `old`. Maximising `dot / sqrt(energy_new)` is normalized
/// cross-correlation up to the (constant) energy of the old window.
///
/// Bounded: `(2*search + 1) * corr` iterations, no allocation, no locking.
#[cold]
#[inline(never)]
fn best_alignment(hist: &DelayLine, old: f32, raw_new: f32, cfg: &PitchConfig) -> f32 {
    let mut best_score = f32::MIN;
    let mut best_k = 0_i32;
    let n = cfg.corr;

    for k in -cfg.search..=cfg.search {
        let base = raw_new + k as f32;
        if base < cfg.min_offset || base > cfg.max_offset + cfg.grain {
            continue;
        }
        let mut dot = 0.0_f32;
        let mut energy = 0.0_f32;
        for i in 0..n {
            let a = hist.sample_nearest(old + i as f32);
            let b = hist.sample_nearest(base + i as f32);
            dot += a * b;
            energy += b * b;
        }
        let score = if energy > 1e-9 {
            dot / energy.sqrt()
        } else {
            0.0
        };
        // Tiny bias towards small shifts avoids gratuitous jumps on noise.
        let score = score - (k.abs() as f32) * 1e-5;
        if score > best_score {
            best_score = score;
            best_k = k;
        }
    }

    (raw_new + best_k as f32).clamp(cfg.min_offset, cfg.max_offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    const SR: f32 = 48_000.0;

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
            assert!((cents_to_ratio(c) - want).abs() < 1e-5, "{c} cents");
        }
    }

    #[test]
    fn fade_table_is_complementary() {
        let cfg = PitchConfig::new(SR);
        assert!(cfg.fade[0].abs() < 1e-6, "fade starts silent");
        assert!(
            (cfg.fade[cfg.fade.len() - 1] - 1.0).abs() < 1e-6,
            "ends full"
        );
        for &w in &cfg.fade {
            assert!(((1.0 - w) + w - 1.0).abs() < 1e-6);
            assert!((0.0..=1.0).contains(&w));
        }
    }

    #[test]
    fn config_scales_with_sample_rate() {
        let a = PitchConfig::new(48_000.0);
        let b = PitchConfig::new(96_000.0);
        assert!(b.grain > a.grain * 1.9);
        assert!(b.history_capacity() > a.history_capacity());
        // Mean read-back is a time, not a sample count.
        assert!((a.mean_latency_ms() - b.mean_latency_ms()).abs() < 0.1);
        assert!(a.history_capacity() >= (a.max_offset + a.grain) as usize);
    }

    /// Feed a sine one sample at a time, pushing into the history first.
    fn render(hz: f32, cents: f32, n: usize, index: usize) -> (Vec<f32>, MicroPitchShifter) {
        let cfg = PitchConfig::new(SR);
        let mut hist = DelayLine::new(cfg.history_capacity());
        let mut sh = MicroPitchShifter::new(index, cents > 0.0, &cfg);
        let mut out = Vec::with_capacity(n);
        for k in 0..n {
            let x = (2.0 * PI * hz * k as f32 / SR).sin();
            hist.push(x);
            out.push(sh.process(&hist, cents, &cfg));
        }
        (out, sh)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn envelope_ripple(y: &[f32], win: usize) -> f32 {
        let mut lo = f32::MAX;
        let mut hi = 0.0_f32;
        for chunk in y.chunks(win) {
            if chunk.len() == win {
                let r = rms(chunk);
                lo = lo.min(r);
                hi = hi.max(r);
            }
        }
        (hi - lo) / (0.5 * (hi + lo)) * 100.0
    }

    /// RMS ripple over windows holding a whole number of sine cycles. A
    /// fixed 1024-sample window at 220 Hz only holds 4.7 cycles, and the
    /// partial cycle alone reads as ~3% "modulation" on a pure sine.
    fn sine_envelope_ripple(y: &[f32], hz: f32) -> f32 {
        let win = ((SR / hz) * 32.0).round().max(16.0) as usize;
        envelope_ripple(y, win)
    }

    fn measured_hz(y: &[f32]) -> f32 {
        let mut first = None;
        let mut last = None;
        let mut count = 0_u32;
        for i in 1..y.len() {
            if y[i - 1] < 0.0 && y[i] >= 0.0 {
                let f = -y[i - 1] / (y[i] - y[i - 1]);
                let t = i as f32 - 1.0 + f;
                if first.is_none() {
                    first = Some(t);
                }
                last = Some(t);
                count += 1;
            }
        }
        if count < 2 {
            return 0.0;
        }
        (count - 1) as f32 * SR / (last.unwrap() - first.unwrap())
    }

    #[test]
    fn single_head_has_no_amplitude_modulation() {
        // Start high so a +12 cent head cannot wrap inside one second.
        for hz in [220.0_f32, 440.0, 1000.0] {
            let (y, sh) = render(hz, 12.0, 48_000, 0);
            assert_eq!(sh.wraps(), 0, "test window must not contain a wrap");
            let ripple = sine_envelope_ripple(&y[4096..], hz);
            assert!(
                ripple < 1.0,
                "{hz} Hz envelope ripple {ripple:.2}% (old design: ~50%)"
            );
        }
    }

    #[test]
    fn single_head_has_flat_gain_response() {
        let mut lo = f32::MAX;
        let mut hi = 0.0_f32;
        for hz in [
            110.0_f32, 220.0, 440.0, 880.0, 1760.0, 3520.0, 5280.0, 7040.0,
        ] {
            let (y, _) = render(hz, 12.0, 24_000, 0);
            let g = rms(&y[2048..]);
            lo = lo.min(g);
            hi = hi.max(g);
        }
        let db = 20.0 * (hi / lo).log10();
        assert!(db < 2.0, "gain ripple {db:.2} dB (old design: ~6 dB)");
        assert!(
            (0.707_106_77 - lo).abs() < 0.08,
            "unexpected gain drop: {lo}"
        );
    }

    #[test]
    fn measured_pitch_ratio_is_correct() {
        for cents in [3.0_f32, 12.0, -12.0] {
            let hz = 440.0;
            let (y, _) = render(hz, cents, 96_000, 0);
            let want = hz * 2.0_f32.powf(cents / 1200.0);
            let got = measured_hz(&y[8192..]);
            assert!(
                (got - want).abs() < 0.5,
                "{cents} cents: want {want:.3} Hz, got {got:.3} Hz"
            );
        }
    }

    #[test]
    fn zero_cents_is_a_transparent_static_delay() {
        let (y, sh) = render(440.0, 0.0, 24_000, 0);
        assert_eq!(sh.wraps(), 0);
        assert!((rms(&y[2048..]) - 0.707_106_77).abs() < 0.01);
        assert!((measured_hz(&y[2048..]) - 440.0).abs() < 0.2);
    }

    #[test]
    fn wraps_are_continuous_and_bounded() {
        // 20 s at +12 cents forces several wraps of the 16 ms window.
        let n = (SR * 20.0) as usize;
        let cfg = PitchConfig::new(SR);
        let mut hist = DelayLine::new(cfg.history_capacity());
        let mut sh = MicroPitchShifter::new(0, true, &cfg);
        let hz = 220.0;
        let mut prev = 0.0_f32;
        let mut max_step = 0.0_f32;
        let mut peak = 0.0_f32;
        for k in 0..n {
            let x = (2.0 * PI * hz * k as f32 / SR).sin();
            hist.push(x);
            let y = sh.process(&hist, 12.0, &cfg);
            assert!(y.is_finite(), "non-finite at {k}");
            max_step = max_step.max((y - prev).abs());
            peak = peak.max(y.abs());
            prev = y;
        }
        assert!(sh.wraps() >= 2, "expected wraps, got {}", sh.wraps());
        // A 220 Hz sine can legitimately step by 2*pi*220/48000 ~= 0.029.
        // A discontinuity at a wrap would be many times that.
        assert!(max_step < 0.08, "wrap click, max step {max_step}");
        assert!(peak < 1.2, "unexpected gain: {peak}");
    }

    #[test]
    fn output_is_finite_for_extreme_cents() {
        for cents in [-100.0_f32, -24.0, 24.0, 100.0] {
            let (y, _) = render(330.0, cents, 20_000, 3);
            assert!(y.iter().all(|v| v.is_finite() && v.abs() < 2.0), "{cents}");
        }
    }
}
