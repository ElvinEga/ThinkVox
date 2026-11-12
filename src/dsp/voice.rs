//! One thickened voice: a fixed detune / delay / pan assignment plus a
//! granular micro pitch shifter.

use super::delay::DelayLine;
use super::pitch_shifter::GranularPitchShifter;

/// Immutable part of a voice (the "wiring", not the DSP memory).
#[derive(Clone, Copy)]
pub struct VoiceSpec {
    /// Signed detune in cents at Detune Depth = 1.0.
    pub cents: f32,
    /// Nominal doubling delay in milliseconds at Delay Spread = 1.0.
    pub delay_ms: f32,
    /// `true` = hard left voice, `false` = hard right voice.
    pub hard_left: bool,
}

/// The classic 8-voice ADT cluster.
///
/// Pitch polarity *and* delay time alternate across the stereo field:
/// each side gets two up / two down shifts, two 10 ms and two 24 ms voices,
/// so the image stays balanced and neither side is consistently brighter.
pub const VOICE_SPECS: [VoiceSpec; 8] = [
    VoiceSpec {
        cents: 3.0,
        delay_ms: 10.0,
        hard_left: true,
    },
    VoiceSpec {
        cents: -3.0,
        delay_ms: 24.0,
        hard_left: false,
    },
    VoiceSpec {
        cents: -6.0,
        delay_ms: 24.0,
        hard_left: true,
    },
    VoiceSpec {
        cents: 6.0,
        delay_ms: 10.0,
        hard_left: false,
    },
    VoiceSpec {
        cents: 9.0,
        delay_ms: 10.0,
        hard_left: true,
    },
    VoiceSpec {
        cents: -9.0,
        delay_ms: 24.0,
        hard_left: false,
    },
    VoiceSpec {
        cents: -12.0,
        delay_ms: 24.0,
        hard_left: true,
    },
    VoiceSpec {
        cents: 12.0,
        delay_ms: 10.0,
        hard_left: false,
    },
];

pub struct Voice {
    spec: VoiceSpec,
    shifter: GranularPitchShifter,
}

impl Voice {
    pub fn new(spec: VoiceSpec, index: usize) -> Self {
        Self {
            spec,
            shifter: GranularPitchShifter::new((index as f32 + 0.5) / 8.0),
        }
    }

    pub fn reset(&mut self, index: usize) {
        self.shifter.reset((index as f32 + 0.5) / 8.0);
    }

    #[inline]
    pub fn hard_left(&self) -> bool {
        self.spec.hard_left
    }

    /// Render this voice's pitch-shifted, delayed mono sample.
    #[inline]
    pub fn render(
        &mut self,
        line: &DelayLine,
        detune_depth: f32,
        delay_spread: f32,
        sr: f32,
    ) -> f32 {
        let cents = self.spec.cents * detune_depth;
        let base_delay = self.spec.delay_ms * 0.001 * delay_spread;
        self.shifter.process(line, cents, base_delay, sr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_is_stereo_balanced() {
        let l: Vec<_> = VOICE_SPECS.iter().filter(|v| v.hard_left).collect();
        let r: Vec<_> = VOICE_SPECS.iter().filter(|v| !v.hard_left).collect();
        assert_eq!(l.len(), 4);
        assert_eq!(r.len(), 4);

        for side in [&l, &r] {
            assert_eq!(side.iter().filter(|v| v.cents > 0.0).count(), 2);
            assert_eq!(side.iter().filter(|v| v.cents < 0.0).count(), 2);
            assert_eq!(side.iter().filter(|v| v.delay_ms == 10.0).count(), 2);
            assert_eq!(side.iter().filter(|v| v.delay_ms == 24.0).count(), 2);
        }
        // Opposite sides are mirror images in pitch polarity.
        assert_eq!(VOICE_SPECS[0].cents, -VOICE_SPECS[1].cents);
        assert_eq!(VOICE_SPECS[2].cents, -VOICE_SPECS[3].cents);
        assert_eq!(VOICE_SPECS[4].cents, -VOICE_SPECS[5].cents);
        assert_eq!(VOICE_SPECS[6].cents, -VOICE_SPECS[7].cents);
    }

    #[test]
    fn delay_range_stays_inside_the_doubling_band() {
        // Even at maximum Delay Spread the nominal delays stay <= ~31 ms.
        for v in &VOICE_SPECS {
            assert!(v.delay_ms * 1.3 <= 32.0);
            assert!(v.delay_ms * 0.7 >= 7.0);
        }
    }
}
