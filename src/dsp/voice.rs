//! One thickened voice: an immutable detune / delay / pan assignment plus
//! its own granular read head.
//!
//! The signal path is deliberately split across two stages that a `Voice`
//! only *coordinates*:
//!
//! ```text
//!   DelayStage (10 / 24 ms) --> GrainHistory[group] --> Voice::head
//!                                                        |
//!                                                   pan + level
//! ```
//!
//! A voice owns no audio buffer of its own. The two grain histories are
//! shared by the four voices in each delay group, so the eight independent
//! pitch sweeps cost eight small state structs instead of eight buffers.

use super::pitch_shifter::{MicroPitchShifter, PitchConfig};

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

/// Which of the two delayed mono streams feeds this voice.
/// 0 = the short (10 ms) group, 1 = the long (24 ms) group.
#[inline]
pub fn delay_group(spec: VoiceSpec) -> usize {
    if spec.delay_ms < 15.0 { 0 } else { 1 }
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
    pub spec: VoiceSpec,
    /// Stage 2: this voice's independent granular read head.
    pub head: MicroPitchShifter,
    /// Index into `Thickener::hist`.
    pub group: usize,
}

impl Voice {
    pub fn new(spec: VoiceSpec, index: usize, cfg: &PitchConfig) -> Self {
        Self {
            spec,
            head: MicroPitchShifter::new(index, spec.cents > 0.0, cfg),
            group: delay_group(spec),
        }
    }

    pub fn reset(&mut self, index: usize, cfg: &PitchConfig) {
        self.head.reset(index, self.spec.cents > 0.0, cfg);
    }

    #[inline]
    pub fn hard_left(&self) -> bool {
        self.spec.hard_left
    }

    /// Signed detune in cents after the Detune Depth control.
    #[inline]
    pub fn cents(&self, detune_depth: f32) -> f32 {
        self.spec.cents * detune_depth
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

    #[test]
    fn delay_groups_are_balanced() {
        let cfg = PitchConfig::new(48_000.0);
        let voices: Vec<Voice> = VOICE_SPECS
            .iter()
            .enumerate()
            .map(|(i, &spec)| Voice::new(spec, i, &cfg))
            .collect();
        assert_eq!(voices.iter().filter(|v| v.group == 0).count(), 4);
        assert_eq!(voices.iter().filter(|v| v.group == 1).count(), 4);

        // Heads are spread across the window so wraps do not coincide.
        let offsets: Vec<f32> = voices.iter().map(|v| v.head.offset()).collect();
        let unique = {
            let mut o = offsets.clone();
            o.sort_by(|a, b| a.partial_cmp(b).unwrap());
            o.windows(2).all(|w| (w[1] - w[0]).abs() > 1.0)
        };
        assert!(unique, "heads should be decorrelated: {offsets:?}");
    }
}
