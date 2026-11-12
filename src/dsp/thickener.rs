//! The 8-voice engine + Mid/Side wet stage.
//!
//! The wet chain is now two clearly separated stages:
//!
//! ```text
//!   mono sum --> DelayStage (10 / 24 ms ADT offsets)
//!                    |--> GrainHistory[short] --+
//!                    |--> GrainHistory[long]  --+--> 8 x MicroPitchShifter
//!                                                        |
//!                                          per-pair pan -> wet L / wet R
//!                                                        |
//!                           M/S -> Side HPF -> Side Air -> Width -> Mix
//! ```
//!
//! The delay stage never sees a pitch ratio and the pitch stage never sees a
//! millisecond value. `DelayStage` subtracts the pitch stage's mean
//! read-back so the classic 10 / 24 ms offsets are preserved at the output.

use super::delay::{DelayLine, DelayStage};
use super::filter::OnePole;
use super::pitch_shifter::PitchConfig;
use super::voice::{VOICE_SPECS, Voice};
use core::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_4};

/// Buffer capacity in seconds for the shared input history. Worst case is
/// 24 ms * 1.3 spread = 31.2 ms (before pitch-latency compensation), so
/// 80 ms leaves plenty of headroom even at 192 kHz.
pub const MAX_DELAY_S: f32 = 0.080;

/// Fixed corner for the Side-only "Air" shelf.
const AIR_HZ: f32 = 4000.0;

/// Per-sample control values, read once per sample by the plugin and handed
/// to the engine. Keeping this a plain stack struct means the DSP stays
/// framework-independent and directly unit-testable.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// Wet fader, 0..1. Dry stays at full level.
    pub mix: f32,
    /// Scales the +/-3/6/9/12 cent assignments (0.5..2.0).
    pub detune_depth: f32,
    /// Scales the 10 / 24 ms pair (0.7..1.3).
    pub delay_spread: f32,
    /// M/S Side gain: 0 = mono, 1 = normal, 2 = hyper-wide.
    pub width: f32,
    /// Per-pair pan spread, 0 = both voices centred, 1 = hard L/R.
    /// Pair `p` covers voices `2p` (left) and `2p + 1` (right), i.e.
    /// +/-3, +/-6, +/-9 and +/-12 cents.
    pub pair_spread: [f32; 4],
    /// Per-voice linear gain (from the Voice Level dB parameter).
    pub voice_gain: f32,
    /// Side highpass corner in Hz (keeps low end centred / mono-safe).
    pub tone_hz: f32,
    /// Side high-shelf linear gain (from the Air dB parameter).
    pub air_gain: f32,
    /// Collapse the wet cluster into Mid only ("centred thickness").
    pub mid_only: bool,
}

impl Settings {
    /// Useful starting point for tests and offline rendering.
    pub fn neutral() -> Self {
        Self {
            mix: 0.35,
            detune_depth: 1.0,
            delay_spread: 1.0,
            width: 1.0,
            pair_spread: [1.0; 4],
            // -8 dB
            voice_gain: 0.398_107_17,
            tone_hz: 120.0,
            air_gain: 1.0,
            mid_only: false,
        }
    }
}

/// Stage 1 delay in milliseconds after compensating for the pitch stage's
/// mean read-back. The floor keeps the cubic interpolator well behind the
/// write pointer at every Delay Spread setting.
#[inline]
pub fn doubling_delay_ms(nominal_ms: f32, spread: f32, pitch_latency_ms: f32) -> f32 {
    (nominal_ms * spread - pitch_latency_ms).max(1.0)
}

/// Constant-power pan gains for the **left** voice of a pair.
///
/// `spread = 1` gives `(1, 0)` (hard left), `spread = 0` gives
/// `(1/sqrt2, 1/sqrt2)` (dead centre). The right voice simply swaps the two
/// gains. `cos^2 + sin^2 == 1`, so panning never changes loudness.
#[inline]
pub fn pair_pan(spread: f32) -> (f32, f32) {
    let a = (1.0 - spread.clamp(0.0, 1.0)) * FRAC_PI_4;
    (a.cos(), a.sin())
}

/// L/R -> M/S. Lossless and invertible (with the 0.5 convention).
#[inline]
pub fn encode_ms(l: f32, r: f32) -> (f32, f32) {
    ((l + r) * 0.5, (l - r) * 0.5)
}

/// M/S -> L/R. Pass `side * width` to scale the stereo image.
#[inline]
pub fn decode_ms(mid: f32, side: f32) -> (f32, f32) {
    (mid + side, mid - side)
}

pub struct Thickener {
    /// Stage 1: shared input history for the ADT doubling delay.
    delay: DelayStage,
    /// Stage 2: one grain history per ADT delay group (10 ms / 24 ms).
    /// Four voices share each history; only their read heads differ.
    hist: [DelayLine; 2],
    voices: [Voice; 8],
    cfg: PitchConfig,
    /// Side highpass state.
    tone_hp: OnePole,
    /// Side air shelf state.
    air_shelf: OnePole,
    /// Last spread value seen per pair, used to cache the trig pan gains.
    last_pair_spread: [f32; 4],
    /// Cached `(left gain, right gain)` for the left voice of each pair.
    pair_pan: [(f32, f32); 4],
    sr: f32,
}

impl Default for Thickener {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

impl Thickener {
    pub fn new(sr: f32) -> Self {
        let cfg = PitchConfig::new(sr);
        let sr = cfg.sample_rate();
        let input_cap = (MAX_DELAY_S * sr).ceil() as usize + 8;
        let grain_cap = cfg.history_capacity();
        let mut this = Self {
            delay: DelayStage::new(input_cap),
            hist: core::array::from_fn(|_| DelayLine::new(grain_cap)),
            voices: core::array::from_fn(|i| Voice::new(VOICE_SPECS[i], i, &cfg)),
            cfg,
            tone_hp: OnePole::default(),
            air_shelf: OnePole::default(),
            last_pair_spread: [f32::NAN; 4],
            pair_pan: [(1.0, 0.0); 4],
            sr,
        };
        this.clear();
        this
    }

    /// Called from `reset()` (off the audio thread). Reallocates only when
    /// the sample rate actually changed, then clears all DSP memory.
    pub fn prepare(&mut self, sr: f32) {
        let sr = sr.clamp(8_000.0, 384_000.0);
        if (self.sr - sr).abs() > 0.5 {
            let cfg = PitchConfig::new(sr);
            let input_cap = (MAX_DELAY_S * sr).ceil() as usize + 8;
            let grain_cap = cfg.history_capacity();
            self.delay = DelayStage::new(input_cap);
            self.hist = core::array::from_fn(|_| DelayLine::new(grain_cap));
            self.cfg = cfg;
            self.sr = sr;
        }
        self.clear();
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }

    /// Mean read-back contributed by the pitch stage (milliseconds).
    pub fn pitch_latency_ms(&self) -> f32 {
        self.cfg.mean_latency_ms()
    }

    /// Exposed for diagnostics / tests.
    pub fn input_delay_capacity(&self) -> usize {
        self.delay.capacity()
    }

    pub fn grain_history_capacity(&self) -> usize {
        self.hist[0].capacity()
    }

    /// Clear delay lines, sweep phases, pan cache and filter state.
    pub fn clear(&mut self) {
        self.delay.clear();
        for h in &mut self.hist {
            h.clear();
        }
        for (i, v) in self.voices.iter_mut().enumerate() {
            v.reset(i, &self.cfg);
        }
        self.tone_hp.reset();
        self.air_shelf.reset();
        // Force a pan-coefficient rebuild on the next sample.
        self.last_pair_spread = [f32::NAN; 4];
    }

    /// Process one stereo frame. Returns the **wet** L/R contribution;
    /// the caller adds the dry signal.
    #[inline]
    pub fn process_sample(&mut self, in_l: f32, in_r: f32, s: &Settings) -> (f32, f32) {
        // Voices are driven from the mono sum so a stereo source doesn't
        // double-feed any one side. A mono bus is fanned to L=R by the host
        // adapter, which also reproduces the mono sum exactly.
        let mono = (in_l + in_r) * 0.5;
        let sr = self.sr;

        // ---- Stage 1: ADT doubling delay ---------------------------------
        self.delay.push(mono);
        let comp = self.cfg.mean_latency_ms();
        let short_ms = doubling_delay_ms(10.0, s.delay_spread, comp);
        let long_ms = doubling_delay_ms(24.0, s.delay_spread, comp);
        let delayed_short = self.delay.read_ms(short_ms, sr);
        let delayed_long = self.delay.read_ms(long_ms, sr);

        // ---- Stage 2: independent granular micro pitch -------------------
        self.hist[0].push(delayed_short);
        self.hist[1].push(delayed_long);

        // Four trig calls only when a spread is actually moving; zero in
        // steady state. Spreads are smoothed upstream, so this is both
        // click-free and cheap on the audio thread.
        for p in 0..4 {
            let sp = s.pair_spread[p];
            if sp != self.last_pair_spread[p] {
                self.last_pair_spread[p] = sp;
                self.pair_pan[p] = pair_pan(sp);
            }
        }
        let pan = self.pair_pan;

        let hist = &self.hist;
        let cfg = &self.cfg;
        let mut wet_l = 0.0_f32;
        let mut wet_r = 0.0_f32;
        // Un-panned voice sum. Mid Only mode uses this instead of `mid`, so
        // folding the image to the centre cannot change level when a pair
        // spread knob is moved (hard pan would otherwise be 3 dB down).
        let mut voice_sum = 0.0_f32;
        for (idx, v) in self.voices.iter_mut().enumerate() {
            let y = v.head.process(&hist[v.group], v.cents(s.detune_depth), cfg) * s.voice_gain;
            voice_sum += y;
            let (l, r) = pan[idx / 2];
            let (pl, pr) = if v.hard_left() { (l, r) } else { (r, l) };
            wet_l += y * pl;
            wet_r += y * pr;
        }

        // ---- Mid/Side stage -------------------------------------------
        let (mid, side_raw) = encode_ms(wet_l, wet_r);

        let tone_a = OnePole::coeff(s.tone_hz, sr);
        let air_a = OnePole::coeff(AIR_HZ, sr);
        let side_hp = self.tone_hp.highpass(side_raw, tone_a);
        let side_eq = self.air_shelf.high_shelf(side_hp, air_a, s.air_gain);

        let mix = s.mix.clamp(0.0, 1.0);

        if s.mid_only {
            // Centred thickness: all voices into Mid, no Side energy.
            // Drive it from the un-panned voice sum at 1/sqrt(2), i.e. what a
            // fully centred constant-power pan law would have produced. This
            // keeps the level identical to the default hard-panned setting and
            // makes the mode immune to the four pair spread knobs.
            let m = voice_sum * FRAC_1_SQRT_2 * mix;
            (m, m)
        } else {
            let side = side_eq * s.width.max(0.0);
            let (out_l, out_r) = decode_ms(mid, side);
            (out_l * mix, out_r * mix)
        }
    }

    /// Process a mono buffer into a stereo pair of `Vec`s. Convenience for
    /// tests / offline rendering; the plugin itself never allocates.
    #[cfg(test)]
    pub fn render_mono_block(&mut self, input: &[f32], s: &Settings) -> (Vec<f32>, Vec<f32>) {
        let mut l = Vec::with_capacity(input.len());
        let mut r = Vec::with_capacity(input.len());
        for &x in input {
            let (wl, wr) = self.process_sample(x, x, s);
            l.push(x + wl);
            r.push(x + wr);
        }
        (l, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    fn sine(sr: f32, hz: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * hz * i as f32 / sr).sin())
            .collect()
    }

    #[test]
    fn mid_side_roundtrip_is_lossless() {
        for (l, r) in [(1.0_f32, 0.0), (0.0, 1.0), (-0.7, 0.35), (0.5, 0.5)] {
            let (m, s) = encode_ms(l, r);
            let (l2, r2) = decode_ms(m, s);
            assert!((l - l2).abs() < 1e-7 && (r - r2).abs() < 1e-7);
        }
        let (m, s) = encode_ms(0.8, 0.8);
        assert!((m - 0.8).abs() < 1e-7);
        assert!(s.abs() < 1e-7);
    }

    #[test]
    fn delay_stage_compensates_pitch_latency() {
        let t = Thickener::new(48_000.0);
        let comp = t.pitch_latency_ms();
        assert!(comp > 1.0 && comp < 20.0, "unexpected pitch latency {comp}");
        for spread in [0.7_f32, 1.0, 1.3] {
            for nominal in [10.0_f32, 24.0] {
                let stage1 = doubling_delay_ms(nominal, spread, comp);
                // Total wet offset (stage 1 + mean stage 2) is the classic
                // Huart delay, except where the 1 ms safety floor applies.
                let total = stage1 + comp;
                assert!(
                    (total - nominal * spread).abs() < 0.01 || stage1 == 1.0,
                    "spread={spread} nominal={nominal}: {total}"
                );
                assert!(total <= 32.0, "wet offset left the doubling band");
            }
        }
    }

    #[test]
    fn stage_buffers_are_sized_independently() {
        let t = Thickener::new(48_000.0);
        // Two grain histories, not eight: four voices share each.
        assert_eq!(t.hist.len(), 2);
        assert!(t.grain_history_capacity() > t.cfg.max_offset as usize);
        assert!(t.input_delay_capacity() >= (MAX_DELAY_S * 48_000.0) as usize);
    }

    #[test]
    fn zero_mix_is_exactly_dry() {
        let mut t = Thickener::new(48_000.0);
        let mut s = Settings::neutral();
        s.mix = 0.0;
        let input = sine(48_000.0, 220.0, 4096);
        let (l, r) = t.render_mono_block(&input, &s);
        for i in 0..input.len() {
            assert_eq!(l[i], input[i]);
            assert_eq!(r[i], input[i]);
        }
    }

    #[test]
    fn zero_width_is_mono_compatible() {
        let mut t = Thickener::new(48_000.0);
        let mut s = Settings::neutral();
        s.width = 0.0;
        s.air_gain = 1.0;
        let input = sine(48_000.0, 220.0, 8192);
        let (l, r) = t.render_mono_block(&input, &s);
        for i in 0..input.len() {
            assert!(
                (l[i] - r[i]).abs() < 1e-6,
                "frame {i}: {} vs {}",
                l[i],
                r[i]
            );
        }
    }

    #[test]
    fn mid_only_wet_is_identical_on_both_sides() {
        let mut t = Thickener::new(48_000.0);
        let mut s = Settings::neutral();
        s.mid_only = true;
        let input = sine(48_000.0, 300.0, 8192);
        let (l, r) = t.render_mono_block(&input, &s);
        for i in 0..input.len() {
            assert!((l[i] - r[i]).abs() < 1e-6);
        }
    }

    #[test]
    fn mid_only_level_is_independent_of_pair_spread() {
        let input = sine(48_000.0, 220.0, 8192);
        let mut settings = Settings::neutral();
        settings.mid_only = true;

        let mut hard = Thickener::new(48_000.0);
        let (hard_l, _) = hard.render_mono_block(&input, &settings);

        settings.pair_spread = [0.0; 4];
        let mut centred = Thickener::new(48_000.0);
        let (centred_l, _) = centred.render_mono_block(&input, &settings);

        for i in 0..input.len() {
            assert!(
                (hard_l[i] - centred_l[i]).abs() < 1e-6,
                "frame {i}: {} vs {}",
                hard_l[i],
                centred_l[i]
            );
        }
    }

    #[test]
    fn mixed_settings_stay_finite_and_bounded() {
        let sr = 48_000.0;
        for (depth, spread, width, pan, tone) in [
            (0.5_f32, 0.7_f32, 0.0_f32, 0.0_f32, 20.0_f32),
            (2.0, 1.3, 2.0, 1.0, 300.0),
            (1.0, 1.0, 1.0, 0.5, 120.0),
        ] {
            let mut t = Thickener::new(sr);
            let mut s = Settings::neutral();
            s.detune_depth = depth;
            s.delay_spread = spread;
            s.width = width;
            s.pair_spread = [pan; 4];
            s.tone_hz = tone;
            s.air_gain = 2.0;
            let input = sine(sr, 180.0, 20_000);
            let (l, r) = t.render_mono_block(&input, &s);
            for i in 0..input.len() {
                assert!(l[i].is_finite() && r[i].is_finite(), "non-finite at {i}");
                assert!(l[i].abs() < 4.0 && r[i].abs() < 4.0, "runaway at {i}");
            }
        }
    }

    #[test]
    fn pair_pan_is_constant_power() {
        for k in 0..=20 {
            let sp = k as f32 / 20.0;
            let (l, r) = pair_pan(sp);
            assert!((l * l + r * r - 1.0).abs() < 1e-6, "spread={sp}");
        }
        let (l, r) = pair_pan(1.0);
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6, "hard left");
        let (l, r) = pair_pan(0.0);
        assert!((l - r).abs() < 1e-6, "centred");
    }

    #[test]
    fn all_pairs_centred_folds_wet_to_mono() {
        let mut t = Thickener::new(48_000.0);
        let mut s = Settings::neutral();
        s.pair_spread = [0.0; 4];
        let input = sine(48_000.0, 220.0, 8192);
        let (l, r) = t.render_mono_block(&input, &s);
        for i in 0..input.len() {
            assert!((l[i] - r[i]).abs() < 1e-6, "frame {i}");
        }
    }

    #[test]
    fn each_pair_spread_changes_the_output() {
        let input = sine(48_000.0, 220.0, 8192);
        let mut baseline = Thickener::new(48_000.0);
        let base = Settings::neutral();
        let (bl, _) = baseline.render_mono_block(&input, &base);

        for p in 0..4 {
            let mut t = Thickener::new(48_000.0);
            let mut s = Settings::neutral();
            s.pair_spread[p] = 0.0;
            let (l, _) = t.render_mono_block(&input, &s);
            let max_diff = bl
                .iter()
                .zip(&l)
                .fold(0.0_f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(max_diff > 1e-4, "pair {p} spread had no effect");
        }
    }

    #[test]
    fn long_run_stays_clean_across_many_wraps() {
        // 20 s at 48 kHz: every voice wraps its grain window several times.
        let sr = 48_000.0;
        let mut t = Thickener::new(sr);
        let s = Settings::neutral();
        let mut max_step = 0.0_f32;
        let mut prev = (0.0_f32, 0.0_f32);
        for n in 0..(sr as usize * 20) {
            let x = (2.0 * PI * 220.0 * n as f32 / sr).sin();
            let (l, r) = t.process_sample(x, x, &s);
            assert!(l.is_finite() && r.is_finite(), "non-finite at {n}");
            assert!(l.abs() < 3.0 && r.abs() < 3.0, "runaway at {n}");
            max_step = max_step.max((l - prev.0).abs()).max((r - prev.1).abs());
            prev = (l, r);
        }
        assert!(max_step < 0.3, "wrap discontinuity: {max_step}");
    }

    #[test]
    fn wet_is_quieter_than_dry_at_defaults() {
        let sr = 48_000.0;
        let mut t = Thickener::new(sr);
        let s = Settings::neutral();
        let input = sine(sr, 220.0, 48_000);
        let (l, r) = t.render_mono_block(&input, &s);
        let dry_rms: f32 = (input.iter().map(|x| x * x).sum::<f32>() / input.len() as f32).sqrt();
        let wet_rms: f32 = (0..input.len())
            .map(|i| {
                let wl = l[i] - input[i];
                let wr = r[i] - input[i];
                wl * wl + wr * wr
            })
            .sum::<f32>()
            / (2.0 * input.len() as f32);
        let wet_rms = wet_rms.sqrt();
        let db = 20.0 * (wet_rms / dry_rms).log10();
        // "Felt more than heard": roughly -6..-20 dB relative to dry.
        assert!(db > -24.0 && db < -3.0, "wet/dry = {db:.1} dB");
    }

    #[test]
    fn sample_rate_change_rebuilds_and_clears() {
        let mut t = Thickener::new(48_000.0);
        let cap_48 = t.grain_history_capacity();
        t.prepare(96_000.0);
        assert_eq!(t.sample_rate(), 96_000.0);
        assert!(t.grain_history_capacity() > cap_48);
        assert!(t.input_delay_capacity() > (MAX_DELAY_S * 48_000.0) as usize);
        let s = Settings::neutral();
        let (l, r) = t.render_mono_block(&sine(96_000.0, 440.0, 1000), &s);
        assert!(l.iter().chain(r.iter()).all(|x| x.is_finite()));
    }
}
