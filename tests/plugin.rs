//! End-to-end tests through the real plugin `process()` path.
//!
//! The DSP unit tests in `src/dsp/` cover the algorithm in isolation;
//! these drive the truce shell (params, smoothing, bus adapter, `reset`)
//! the same way a host does, with no DAW and no audio device.

use std::f32::consts::PI;
use std::time::Duration;

use thickvox::{Plugin, ThickVoxParamsParamId as P};
use truce_test::{InputSource, assertions, driver};

fn tone() -> InputSource {
    InputSource::Generator(Box::new(|frame, sr| {
        (2.0 * PI * 220.0 * frame as f32 / sr as f32).sin() * 0.5
    }))
}

#[test]
fn stereo_render_is_finite_and_bounded() {
    let result = driver!(Plugin)
        .sample_rate(48_000.0)
        .channels(2)
        .block_size(512)
        .duration(Duration::from_millis(500))
        .input(tone())
        .run();

    assertions::assert_nonzero(&result);
    assertions::assert_no_nans(&result);
    assertions::assert_peak_below(&result, 2.0);

    assert_eq!(result.output.len(), 2, "expected a stereo bus");
    let frames = result.output[0].len();
    assert!(frames > 20_000);
}

#[test]
fn voices_create_real_stereo_difference() {
    let result = driver!(Plugin)
        .sample_rate(48_000.0)
        .channels(2)
        .duration(Duration::from_millis(500))
        .input(tone())
        .run();

    let max_diff = result.output[0]
        .iter()
        .zip(&result.output[1])
        .fold(0.0_f32, |m, (l, r)| m.max((l - r).abs()));
    assert!(max_diff > 0.01, "wet cluster produced no width: {max_diff}");
}

#[test]
fn mono_input_bus_does_not_panic_or_diverge() {
    // Exercises the mono-in -> stereo-out layout / frame fan-out path.
    let result = driver!(Plugin)
        .sample_rate(44_100.0)
        .channels(1)
        .block_size(256)
        .duration(Duration::from_millis(200))
        .input(tone())
        .run();

    assertions::assert_nonzero(&result);
    assertions::assert_no_nans(&result);
}

#[test]
fn bypass_is_bit_transparent() {
    let result = driver!(Plugin)
        .sample_rate(48_000.0)
        .channels(2)
        .duration(Duration::from_millis(100))
        .input(InputSource::Constant(0.5))
        .set_param(P::Bypass, 1.0)
        .run();

    for ch in &result.output {
        for &x in ch {
            assert_eq!(x, 0.5, "bypass must be a straight wire");
        }
    }
}

#[test]
fn zero_mix_is_bit_transparent() {
    // Mix is normalized 0..1 over its 0..1 range, so 0.0 = dry only.
    let result = driver!(Plugin)
        .sample_rate(48_000.0)
        .channels(2)
        .duration(Duration::from_millis(100))
        .input(InputSource::Constant(0.5))
        .set_param(P::Mix, 0.0)
        .run();

    for ch in &result.output {
        for &x in ch {
            assert_eq!(x, 0.5, "Mix=0 must be a straight wire");
        }
    }
}

#[test]
fn zero_width_folds_to_mono() {
    let result = driver!(Plugin)
        .sample_rate(48_000.0)
        .channels(2)
        .duration(Duration::from_millis(300))
        .input(tone())
        .set_param(P::Width, 0.0)
        .run();

    let max_diff = result.output[0]
        .iter()
        .zip(&result.output[1])
        .fold(0.0_f32, |m, (l, r)| m.max((l - r).abs()));
    assert!(max_diff < 1e-5, "Width=0 should be mono-safe: {max_diff}");
}

#[test]
fn extreme_automation_does_not_blow_up() {
    // Push every continuous control to both rails across the run.
    let result = driver!(Plugin)
        .sample_rate(96_000.0)
        .channels(2)
        .duration(Duration::from_millis(400))
        .input(tone())
        .set_param(P::Mix, 1.0)
        .set_param(P::DetuneDepth, 1.0)
        .set_param(P::DelaySpread, 1.0)
        .set_param(P::Width, 1.0)
        .set_param(P::Pair1Spread, 1.0)
        .set_param(P::Pair2Spread, 1.0)
        .set_param(P::Pair3Spread, 1.0)
        .set_param(P::Pair4Spread, 1.0)
        .set_param(P::VoiceLevel, 1.0)
        .set_param(P::SideHpf, 1.0)
        .set_param(P::Air, 1.0)
        .run();

    assertions::assert_no_nans(&result);
    assertions::assert_nonzero(&result);
}

/// Host-level proof that the four pair knobs are actually wired through to
/// the engine (the DSP unit tests only prove the engine itself responds).
#[test]
fn pair_spread_knobs_reach_the_dsp() {
    let baseline = driver!(Plugin)
        .sample_rate(48_000.0)
        .channels(2)
        .duration(Duration::from_millis(300))
        .input(tone())
        .run();

    let pairs = [
        P::Pair1Spread,
        P::Pair2Spread,
        P::Pair3Spread,
        P::Pair4Spread,
    ];
    for (i, p) in pairs.into_iter().enumerate() {
        let result = driver!(Plugin)
            .sample_rate(48_000.0)
            .channels(2)
            .duration(Duration::from_millis(300))
            .input(tone())
            .set_param(p, 0.0)
            .run();

        let max_diff = result.output[0]
            .iter()
            .zip(&baseline.output[0])
            .fold(0.0_f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(
            max_diff > 1e-4,
            "pair {i} spread knob had no effect: {max_diff}"
        );
    }
}

/// Narrowing every pair to the centre must collapse the wet image to mono,
/// independently of the global Width control.
#[test]
fn all_pair_pans_centred_folds_to_mono() {
    let result = driver!(Plugin)
        .sample_rate(48_000.0)
        .channels(2)
        .duration(Duration::from_millis(300))
        .input(tone())
        .set_param(P::Pair1Spread, 0.0)
        .set_param(P::Pair2Spread, 0.0)
        .set_param(P::Pair3Spread, 0.0)
        .set_param(P::Pair4Spread, 0.0)
        .run();

    let max_diff = result.output[0]
        .iter()
        .zip(&result.output[1])
        .fold(0.0_f32, |m, (l, r)| m.max((l - r).abs()));
    assert!(max_diff < 1e-5, "centred pairs should be mono: {max_diff}");
}

#[cfg(feature = "rt-paranoid")]
#[test]
fn process_never_allocates() {
    use truce_test::assert_no_audio_alloc;
    assert_no_audio_alloc(|| {
        driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .run()
    });
}
