//! Numeric quality regressions for the granular micro pitch shifter.
//!
//! These are the measurements that motivated the rewrite. The previous
//! two-read-head / equal-power-crossfade design produced, on a 440 Hz sine
//! at +12 cents:
//!
//! ```text
//!   RMS gain 110 Hz .. 7 kHz : 0.44 .. 0.91   (~6 dB comb ripple)
//!   short-window RMS ripple   : ~50%
//! ```
//!
//! The single-active-head design must stay flat and modulation-free.

use std::f32::consts::PI;
use thickvox::dsp::delay::DelayLine;
use thickvox::dsp::pitch_shifter::{MicroPitchShifter, PitchConfig};
use thickvox::dsp::thickener::{Thickener, doubling_delay_ms};

const SR: f32 = 48_000.0;

fn render(hz: f32, cents: f32, n: usize, index: usize) -> (Vec<f32>, u32) {
    let cfg = PitchConfig::new(SR);
    let mut hist = DelayLine::new(cfg.history_capacity());
    let mut sh = MicroPitchShifter::new(index, cents > 0.0, &cfg);
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let x = (2.0 * PI * hz * k as f32 / SR).sin();
        hist.push(x);
        out.push(sh.process(&hist, cents, &cfg));
    }
    (out, sh.wraps())
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

/// Cycle-aligned RMS ripple. A fixed 1024-sample window at 220 Hz holds
/// only 4.7 cycles, and that partial cycle alone reads as ~3% modulation.
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
fn frequency_response_is_flat() {
    let freqs = [
        110.0_f32, 165.0, 220.0, 330.0, 440.0, 660.0, 880.0, 1320.0, 1760.0, 2640.0, 3520.0,
        5280.0, 7040.0,
    ];
    let mut gains = Vec::new();
    for &hz in &freqs {
        let (y, wraps) = render(hz, 12.0, 24_000, 0);
        assert_eq!(wraps, 0, "test window must not contain a wrap");
        gains.push(rms(&y[2048..]));
    }
    let lo = gains.iter().copied().fold(f32::MAX, f32::min);
    let hi = gains.iter().copied().fold(0.0_f32, f32::max);
    let db = 20.0 * (hi / lo).log10();
    for (hz, g) in freqs.iter().zip(&gains) {
        println!("  {hz:>7.0} Hz : {g:.4}");
    }
    println!("gain spread: {lo:.4} .. {hi:.4} ({db:.2} dB)");
    assert!(db < 2.0, "comb coloration returned: {db:.2} dB");
}

#[test]
fn steady_state_has_no_amplitude_modulation() {
    for cents in [3.0_f32, 12.0, -12.0] {
        let (y, wraps) = render(440.0, cents, 48_000, 0);
        assert_eq!(wraps, 0);
        let ripple = sine_envelope_ripple(&y[4096..], 440.0);
        let want = 440.0 * 2.0_f32.powf(cents / 1200.0);
        let got = measured_hz(&y[4096..]);
        println!("{cents:>6.1} ct : ripple {ripple:5.2}%  pitch {got:8.3} Hz (want {want:.3})");
        assert!(
            ripple < 1.0,
            "envelope ripple {ripple:.2}% at {cents} cents"
        );
        assert!((got - want).abs() < 0.5, "pitch error at {cents} cents");
    }
}

#[test]
fn wraps_are_inaudible() {
    let n = (SR * 20.0) as usize;
    let (y, wraps) = render(220.0, 12.0, n, 0);
    assert!(wraps >= 2, "expected several wraps, got {wraps}");
    let mut max_step = 0.0_f32;
    for i in 1..y.len() {
        max_step = max_step.max((y[i] - y[i - 1]).abs());
    }
    // A 220 Hz sine steps by at most 2*pi*220/48000 = 0.0288 per sample.
    // Allow generous headroom for the two-head crossfade, but not a click.
    println!("wraps: {wraps}, max sample step: {max_step:.5}");
    assert!(max_step < 0.09, "discontinuity at a wrap: {max_step}");

    // Skip the cold-start: the grain history is all zeros for its first
    // ~750 samples, which would otherwise read as a level dip.
    let ripple = envelope_ripple(&y[8192..], 4096);
    println!("20 s envelope ripple (after warm-up): {ripple:.2}%");
    assert!(ripple < 5.0, "level pump across wraps: {ripple:.2}%");
}

#[test]
fn delay_and_pitch_latencies_add_up_to_the_classic_offsets() {
    let engine = Thickener::new(SR);
    let comp = engine.pitch_latency_ms();
    assert!(comp > 1.0 && comp < 20.0, "pitch stage latency {comp} ms");

    for spread in [0.7_f32, 1.0, 1.3] {
        for nominal in [10.0_f32, 24.0] {
            let stage1 = doubling_delay_ms(nominal, spread, comp);
            let total = stage1 + comp;
            println!(
                "spread {spread:.1}, nominal {nominal:>4.0} ms -> stage1 {stage1:5.2} + pitch {comp:5.2} = {total:5.2} ms"
            );
            assert!(
                (total - nominal * spread).abs() < 0.01 || stage1 == 1.0,
                "wet offset drifted to {total} ms"
            );
        }
    }
}
