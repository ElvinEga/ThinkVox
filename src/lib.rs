//! # ThickVox
//!
//! An 8-voice vocal thickener / ADT-style doubler built with `truce`.
//!
//! Eight micro-pitch-shifted, short-delayed copies of the input are hard-panned
//! in a balanced L/R cluster, summed into a wet bus, then shaped in Mid/Side:
//! a Side-only highpass keeps the low end centred and mono-compatible, a gentle
//! Side high shelf adds "air" without brightening the dry lead vocal, and a
//! single Width control scales the whole stereo image. Each of the four
//! +/-cent pairs also has its own pan-spread knob, so the +/-3, +/-6, +/-9
//! and +/-12 cent pairs can be narrowed independently without changing level.
//! The dry path is never delayed, so the plugin reports zero latency.
//!
//! See [`dsp`] for the signal flow and algorithm notes.

use truce::prelude::*;
use truce_gui_types::layout::{GridLayout, knob, section, toggle};

pub mod dsp;

use dsp::thickener::{Settings, Thickener};

/// Host-visible, automatable parameters.
///
/// IDs are pinned explicitly: they become CLAP `clap_id` / VST3 `ParamID`
/// values and must never shift once a session has been saved.
#[derive(Params)]
pub struct ThickVoxParams {
    /// Wet fader. Dry stays at full level; wet is "felt more than heard".
    #[param(
        id = 0,
        name = "Mix",
        range = "linear(0, 1)",
        default = 0.35,
        unit = "%",
        smooth = "linear(20)",
        group = "Thicken"
    )]
    pub mix: FloatParam,

    /// Scales the +/-3/6/9/12 cent voice assignments.
    #[param(
        id = 1,
        name = "Detune Depth",
        range = "linear(0.5, 2)",
        default = 1.0,
        unit = "%",
        smooth = "linear(20)",
        group = "Thicken"
    )]
    pub detune_depth: FloatParam,

    /// Scales the 10 / 24 ms delay pair.
    #[param(
        id = 2,
        name = "Delay Spread",
        range = "linear(0.7, 1.3)",
        default = 1.0,
        unit = "%",
        smooth = "linear(20)",
        group = "Thicken"
    )]
    pub delay_spread: FloatParam,

    // id = 3 is retired (was the single global "Voice Pan" control). IDs are
    // never reused, so existing automation lanes stay meaningful.
    /// Global M/S Side gain: 0% = mono, 100% = normal, 200% = hyper-wide.
    /// Multiplies whatever width the pair spreads already produced.
    #[param(
        id = 4,
        name = "Stereo Width",
        range = "linear(0, 2)",
        default = 1.0,
        unit = "%",
        smooth = "linear(20)",
        group = "Image"
    )]
    pub width: FloatParam,

    /// Per-voice trim. Defaults to -8 dB so the summed 8-voice cluster sits
    /// roughly 6..12 dB below the dry vocal.
    #[param(
        id = 5,
        name = "Voice Level",
        range = "linear(-24, 6)",
        default = -8.0,
        unit = "dB",
        smooth = "exp(10)",
        group = "Tone"
    )]
    pub voice_level: FloatParam,

    /// Highpass on the wet Side only. Keeps low-end energy centred.
    #[param(
        id = 6,
        name = "Side HPF",
        range = "log(20, 300)",
        default = 120.0,
        unit = "Hz",
        smooth = "exp(20)",
        group = "Tone"
    )]
    pub side_hpf: FloatParam,

    /// High shelf on the wet Side only (4 kHz). Brightens the doubled voices
    /// without adding sibilance to the centred dry vocal.
    #[param(
        id = 7,
        name = "Air",
        range = "linear(0, 6)",
        default = 1.5,
        unit = "dB",
        smooth = "exp(10)",
        group = "Tone"
    )]
    pub air: FloatParam,

    /// Collapse the wet cluster into Mid for a denser, mono-safe thickness.
    #[param(id = 8, name = "Mid Only Wet", group = "Image")]
    pub mid_only: BoolParam,

    #[param(id = 9, name = "Bypass", flags = "automatable | bypass")]
    pub bypass: BoolParam,

    // ---- Per-pair pan spread -----------------------------------------
    // Each pair owns one left and one right voice. 100% = hard L/R,
    // 0% = both voices centred. Constant-power pan law, so moving a knob
    // never changes perceived loudness. Setting all four equal reproduces
    // a single global pan-spread control; different values create an
    // asymmetric image (e.g. P1/P3 wide, P2/P4 narrow).
    #[param(
        id = 10,
        name = "Pair 1 Spread",
        range = "linear(0, 1)",
        default = 1.0,
        unit = "%",
        smooth = "linear(20)",
        group = "Pair Pan"
    )]
    pub pair1_spread: FloatParam,

    #[param(
        id = 11,
        name = "Pair 2 Spread",
        range = "linear(0, 1)",
        default = 1.0,
        unit = "%",
        smooth = "linear(20)",
        group = "Pair Pan"
    )]
    pub pair2_spread: FloatParam,

    #[param(
        id = 12,
        name = "Pair 3 Spread",
        range = "linear(0, 1)",
        default = 1.0,
        unit = "%",
        smooth = "linear(20)",
        group = "Pair Pan"
    )]
    pub pair3_spread: FloatParam,

    #[param(
        id = 13,
        name = "Pair 4 Spread",
        range = "linear(0, 1)",
        default = 1.0,
        unit = "%",
        smooth = "linear(20)",
        group = "Pair Pan"
    )]
    pub pair4_spread: FloatParam,
}

use ThickVoxParamsParamId as P;

/// The plugin descriptor is also its own per-instance DSP state, so the
/// truce shell preserves delay lines and sweep phases across a hot reload.
#[derive(Default)]
pub struct ThickVox {
    thickener: Thickener,
}

impl PluginLogic for ThickVox {
    type Params = ThickVoxParams;
    type DspState = Self;

    /// Stereo in/out is the primary layout. The second one lets a mono vocal
    /// track feed the plugin while still getting a stereo output bus, which
    /// is where the widening actually becomes audible.
    fn bus_layouts() -> Vec<BusLayout> {
        vec![
            BusLayout::stereo(),
            BusLayout::new()
                .with_input("Main", ChannelConfig::Mono)
                .with_output("Main", ChannelConfig::Stereo),
        ]
    }

    /// Off the audio thread: size the delay line for the host sample rate and
    /// clear all DSP memory. Called again if the sample rate changes.
    fn reset(state: &mut Self::DspState, _params: &Self::Params, config: &AudioConfig) {
        state.thickener.prepare(config.sample_rate as f32);
    }

    fn process(
        state: &mut Self::DspState,
        params: &Self::Params,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let bypassed = params.bypass.value();

        // dB -> linear needs a powf, but the value only changes while a
        // smoother is moving. Cache it so the steady state costs nothing.
        let mut last_voice_db = f32::NAN;
        let mut voice_gain = 0.0_f32;
        let mut last_air_db = f32::NAN;
        let mut air_gain = 0.0_f32;

        // Every smoothed param is read exactly ONCE per sample here, before
        // being shared by all 8 voices. Reading inside the voice loop would
        // advance each smoother eight times too fast.
        buffer.for_each_stereo_frame(|fin, fout| {
            if bypassed {
                fout[0] = fin[0];
                fout[1] = fin[1];
                return;
            }

            let vdb = params.voice_level.read();
            if vdb != last_voice_db {
                last_voice_db = vdb;
                voice_gain = db_to_linear(vdb);
            }
            let adb = params.air.read();
            if adb != last_air_db {
                last_air_db = adb;
                air_gain = db_to_linear(adb);
            }

            let s = Settings {
                mix: params.mix.read(),
                detune_depth: params.detune_depth.read(),
                delay_spread: params.delay_spread.read(),
                width: params.width.read(),
                pair_spread: [
                    params.pair1_spread.read(),
                    params.pair2_spread.read(),
                    params.pair3_spread.read(),
                    params.pair4_spread.read(),
                ],
                voice_gain,
                tone_hz: params.side_hpf.read(),
                air_gain,
                mid_only: params.mid_only.value(),
            };

            let (wet_l, wet_r) = state.thickener.process_sample(fin[0], fin[1], &s);
            fout[0] = fin[0] + wet_l;
            fout[1] = fin[1] + wet_r;
        });

        ProcessStatus::Normal
    }

    /// The wet cluster keeps ringing for ~50 ms after input stops.
    fn tail(state: &Self::DspState) -> u32 {
        (0.05 * state.thickener.sample_rate() as f64) as u32
    }

    // `latency` stays at the default 0: the dry path is not delayed, so
    // reporting the wet doubling time would make the host shift the dry
    // vocal and destroy the intended ADT offset.

    fn editor(params: Arc<ThickVoxParams>) -> Box<dyn Editor> {
        truce_gui::default_editor(
            params,
            GridLayout::build(vec![
                section(
                    "THICKEN",
                    vec![
                        knob(P::Mix, "Mix"),
                        knob(P::DetuneDepth, "Detune"),
                        knob(P::DelaySpread, "Spread"),
                    ],
                ),
                // One knob per +/-cent pair: 100% = hard L/R, 0% = centred.
                section(
                    "PAIR PAN",
                    vec![
                        knob(P::Pair1Spread, "+/-3c"),
                        knob(P::Pair2Spread, "+/-6c"),
                        knob(P::Pair3Spread, "+/-9c"),
                        knob(P::Pair4Spread, "+/-12c"),
                    ],
                ),
                section(
                    "IMAGE",
                    vec![knob(P::Width, "Width"), toggle(P::MidOnly, "Mid Only")],
                ),
                section(
                    "TONE & LEVEL",
                    vec![
                        knob(P::VoiceLevel, "Voice Lvl"),
                        knob(P::SideHpf, "Side HPF"),
                        knob(P::Air, "Air"),
                        toggle(P::Bypass, "Bypass"),
                    ],
                ),
            ])
            .with_title("THICKVOX")
            .with_subtitle("v0.1"),
        )
    }
}

truce::plugin! {
    logic: ThickVox,
    params: ThickVoxParams,
}

// Installs the real-time allocation checker under `--features rt-paranoid`
// (a no-op otherwise). Wrap a driver run in `assert_no_audio_alloc` to fail
// a test if `process` ever allocates.
truce::enable_rt_paranoid!();
