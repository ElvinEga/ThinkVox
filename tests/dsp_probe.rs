//! Temporary numeric probe for shifter quality. Prints RMS gain vs frequency,
//! envelope ripple and measured output frequency. Kept as a diagnostic tool.

use std::f32::consts::PI;
use thickvox::dsp::delay::DelayLine;
use thickvox::dsp::pitch_shifter::GranularPitchShifter;

const SR: f32 = 48_000.0;

fn render(hz: f32, cents: f32, n: usize, phase: f32) -> Vec<f32> {
    let mut line = DelayLine::new(16384);
    let mut sh = GranularPitchShifter::new(phase);
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let x = (2.0 * PI * hz * k as f32 / SR).sin();
        line.push(x);
        out.push(sh.process(&line, cents, 0.010, SR));
    }
    out
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Frequency from zero-crossing intervals (linear interpolation of the cross).
fn measured_hz(y: &[f32]) -> f32 {
    let mut crosses = Vec::new();
    for i in 1..y.len() {
        if y[i - 1] < 0.0 && y[i] >= 0.0 {
            let f = -y[i - 1] / (y[i] - y[i - 1]);
            crosses.push(i as f32 - 1.0 + f);
        }
    }
    if crosses.len() < 2 {
        return 0.0;
    }
    let span = crosses[crosses.len() - 1] - crosses[0];
    (crosses.len() - 1) as f32 * SR / span
}

/// Ripple of the short-window RMS envelope, as a percentage.
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

#[test]
fn probe_baseline() {
    let n = (SR * 3.0) as usize;
    let skip = (SR * 0.25) as usize;

    println!("\n--- RMS gain vs frequency, +12 cents ---");
    let mut min_gain = f32::MAX;
    let mut max_gain = 0.0_f32;
    for hz in [
        110.0_f32, 165.0, 220.0, 330.0, 440.0, 660.0, 880.0, 1320.0, 1760.0, 2640.0, 3520.0,
        5280.0, 7040.0,
    ] {
        let y = render(hz, 12.0, n, 0.137);
        let g = rms(&y[skip..]);
        min_gain = min_gain.min(g);
        max_gain = max_gain.max(g);
        println!("  {hz:>7.0} Hz : {g:.4}  ({:.2} dB)", 20.0 * g.log10());
    }
    println!("  gain spread: {min_gain:.4} .. {max_gain:.4}");

    println!("\n--- envelope + pitch, 440 Hz ---");
    for cents in [0.0_f32, 3.0, 12.0, -12.0] {
        let y = render(440.0, cents, n, 0.137);
        let want = 440.0 * 2.0f32.powf(cents / 1200.0);
        let got = measured_hz(&y[skip..]);
        let ripple = envelope_ripple(&y[skip..], 1024);
        println!(
            "  {cents:>6.1} ct : want {want:8.3} Hz, got {got:8.3} Hz, RMS ripple {ripple:6.2}%"
        );
    }
}
