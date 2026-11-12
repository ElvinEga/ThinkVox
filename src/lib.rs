//! # ThickVox
//!
//! An 8-voice vocal thickener / ADT-style doubler built with `truce`.
//!
//! Eight micro-pitch-shifted, short-delayed copies of the input are panned in
//! four mirrored L/R pairs, summed into a wet bus, then shaped in Mid/Side:
//! a Side-only highpass keeps the low end centred and mono-compatible, a gentle
//! Side high shelf adds "air" without brightening the dry lead vocal, and a
//! global Width control scales the whole stereo image. Each pair also has its
//! own Spread control (100% = classic hard L/R, 0% = both voices centred), so
//! the +/-3, +/-6, +/-9 and +/-12 cent pairs can be narrowed independently
//! without changing level. The dry path is never delayed, so the plugin reports
//! zero latency.
//!
//! See [`dsp`] for the signal flow and algorithm notes.

use egui::RichText;
use noda_audio::taper::Taper;
use noda_audio::ui::theme::{Palette, Theme};
use noda_audio::ui::{ChannelLevel, Fader, Knob, LevelMeter, Reading};
use truce::prelude::*;
use truce_egui::{EditorUi, EguiEditor};

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

    // ---- Meter lanes -------------------------------------------------
    // Not parameters: one-way 0..1 peak values the audio thread publishes
    // for the editor's stereo meter. The derive numbers them from
    // `METER_ID_BASE`, well clear of the parameter id space above.
    #[meter]
    pub meter_l: MeterSlot,

    #[meter]
    pub meter_r: MeterSlot,
}

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
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let bypassed = params.bypass.value();

        // dB -> linear needs a powf, but the value only changes while a
        // smoother is moving. Cache it so the steady state costs nothing.
        let mut last_voice_db = f32::NAN;
        let mut voice_gain = 0.0_f32;
        let mut last_air_db = f32::NAN;
        let mut air_gain = 0.0_f32;

        // Post-mix peaks for the editor. Accumulated here (instead of a
        // second buffer pass) because the frames are already in registers.
        let mut peak_l = 0.0_f32;
        let mut peak_r = 0.0_f32;

        // Every smoothed param is read exactly ONCE per sample here, before
        // being shared by all 8 voices. Reading inside the voice loop would
        // advance each smoother eight times too fast.
        buffer.for_each_stereo_frame(|fin, fout| {
            if bypassed {
                fout[0] = fin[0];
                fout[1] = fin[1];
            } else {
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
            }

            peak_l = peak_l.max(fout[0].abs());
            peak_r = peak_r.max(fout[1].abs());
        });

        // Meter lanes are 0..1 linear peaks; the editor turns them back
        // into dBFS. Clamp here so a hot bus can't overrun the lane.
        context.set_meter(params.meter_l.id(), peak_l.min(1.0));
        context.set_meter(params.meter_r.id(), peak_r.min(1.0));

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
        Box::new(
            EguiEditor::with_ui(params, (460, 575), ThickVoxEditor::new())
                .resizable(true)
                .min_size((444, 560))
                .max_size((1000, 900)),
        )
    }
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// Custom egui editor built from NodaAudio controls.
///
/// The layout mirrors the signal flow: thicken, image, pair spread, tone.
/// The right-hand column is metering plus the wet voice fader, so the
/// controls that shape the width sit next to the meter that shows it.
struct ThickVoxEditor {
    /// The theme writes into the egui context, so it only needs installing
    /// once per window (not once per frame).
    theme_installed: bool,
}

impl ThickVoxEditor {
    fn new() -> Self {
        Self {
            theme_installed: false,
        }
    }
}

impl EditorUi<ThickVoxParams> for ThickVoxEditor {
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &PluginContext<ThickVoxParams>) {
        if !self.theme_installed {
            Theme::dark().install(ui.ctx());
            self.theme_installed = true;
        }

        // Meter ballistics release on wall-clock time, so keep the window
        // painting between gestures or the bars freeze mid-fall.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));

        let theme = Theme::of(ui);
        let palette = theme.palette.clone();
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, palette.background);

        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| self.body(ui, ctx, &palette, &theme));
    }
}

impl ThickVoxEditor {
    /// Width the metering column is pinned to. Left as a column of its own
    /// rather than letting it claim the remaining width, which otherwise
    /// strands the meter and fader at the far edge of a wide window.
    const OUTPUT_WIDTH: f32 = 92.0;

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &PluginContext<ThickVoxParams>,
        palette: &Palette,
        theme: &Theme,
    ) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("THICKVOX")
                    .strong()
                    .size(18.0)
                    .color(palette.primary),
            );
            ui.label(
                RichText::new("8-voice vocal thickener")
                    .size(11.0)
                    .color(palette.text_dim),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut bypassed = ctx.bypass.value();
                if ui
                    .add(egui::Checkbox::new(&mut bypassed, "Bypass"))
                    .changed()
                {
                    ctx.automate(ctx.bypass.id(), f64::from(u8::from(bypassed)));
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();

        ui.add_space(4.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                section_label(ui, "THICKEN", palette);
                ui.horizontal(|ui| {
                    percent_knob(ui, ctx, &ctx.mix, "Mix", (0.0, 100.0), 100.0);
                    unit_knob(ui, ctx, &ctx.detune_depth, "Detune", (0.5, 2.0), 2, "×");
                    unit_knob(ui, ctx, &ctx.delay_spread, "Delay", (0.7, 1.3), 2, "×");
                });

                section_label(ui, "IMAGE", palette);
                ui.horizontal(|ui| {
                    // 0..200%: 100% is the untouched cluster, 200% is
                    // hyper-wide, 0% folds the wet bus to mono.
                    percent_knob(ui, ctx, &ctx.width, "Width", (0.0, 200.0), 100.0);
                    ui.vertical(|ui| {
                        // Sit the toggle level with the dial it belongs to
                        // rather than with the top of the knob cell.
                        ui.add_space(theme.size.knob_diameter() * 0.5 - 11.0);
                        let mut mid_only = ctx.mid_only.value();
                        if ui
                            .add(egui::Checkbox::new(&mut mid_only, "Mid Only"))
                            .on_hover_text(
                                "Fold the wet cluster to mono for a denser, mono-safe thickness",
                            )
                            .changed()
                        {
                            ctx.automate(ctx.mid_only.id(), f64::from(u8::from(mid_only)));
                        }
                    });
                });

                section_label(ui, "PAIR SPREAD", palette);
                ui.horizontal(|ui| {
                    percent_knob(ui, ctx, &ctx.pair1_spread, "+/-3c", (0.0, 100.0), 100.0);
                    percent_knob(ui, ctx, &ctx.pair2_spread, "+/-6c", (0.0, 100.0), 100.0);
                    percent_knob(ui, ctx, &ctx.pair3_spread, "+/-9c", (0.0, 100.0), 100.0);
                    percent_knob(ui, ctx, &ctx.pair4_spread, "+/-12c", (0.0, 100.0), 100.0);
                });

                section_label(ui, "TONE", palette);
                ui.horizontal(|ui| {
                    unit_knob(ui, ctx, &ctx.side_hpf, "Side HPF", (20.0, 300.0), 0, "Hz");
                    unit_knob(ui, ctx, &ctx.air, "Air", (0.0, 6.0), 1, "dB");
                });
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(10.0);

            ui.vertical(|ui| {
                ui.set_width(Self::OUTPUT_WIDTH);
                section_label(ui, "OUTPUT", palette);
                let levels = [
                    ChannelLevel::from_linear(ctx.get_meter(ctx.meter_l.id())),
                    ChannelLevel::from_linear(ctx.get_meter(ctx.meter_r.id())),
                ];
                ui.add(
                    LevelMeter::stereo(&levels)
                        .scale(true)
                        .readout(true)
                        .clip(true)
                        .id(egui::Id::new("thickvox-output-meter")),
                );

                ui.add_space(8.0);
                section_label(ui, "VOICE LEVEL", palette);
                let mut db = ctx.voice_level.raw_target() as f32;
                let fader = Fader::new(&mut db)
                    .range(-24.0, 6.0)
                    .taper(Taper::AUDIO)
                    .scale(false)
                    .id(egui::Id::new("thickvox-voice-level"));
                let response = ui.add(fader);
                if response.changed() {
                    publish(ctx, &ctx.voice_level, db, &response);
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Editor helpers
// ---------------------------------------------------------------------------

fn section_label(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(text)
            .size(10.0)
            .strong()
            .color(palette.text_dim),
    );
    ui.add_space(2.0);
}

/// A 0..100% knob. `display_scale` converts the parameter's plain unit to
/// percent (1.0 for 0..1 params, 100.0 for 0..100 params).
fn percent_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    label: &'static str,
    display: (f32, f32),
    display_scale: f32,
) {
    param_knob(
        ui,
        ctx,
        param,
        KnobSpec {
            label,
            display,
            display_scale,
            decimals: 0,
            unit: Some("%"),
        },
    );
}

/// A knob in the parameter's own units (Hz, dB, cents multiplier, ...).
fn unit_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    label: &'static str,
    display: (f32, f32),
    decimals: usize,
    unit: &'static str,
) {
    param_knob(
        ui,
        ctx,
        param,
        KnobSpec {
            label,
            display,
            display_scale: 1.0,
            decimals,
            unit: Some(unit),
        },
    );
}

/// How a parameter is presented on a knob.
struct KnobSpec {
    label: &'static str,
    display: (f32, f32),
    display_scale: f32,
    decimals: usize,
    unit: Option<&'static str>,
}

fn param_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    spec: KnobSpec,
) {
    let mut shown = (param.raw_target() as f32) * spec.display_scale;
    let response = ui.add(
        Knob::new(&mut shown)
            .range(spec.display.0, spec.display.1)
            .label(spec.label)
            .reading(Reading::Number {
                decimals: spec.decimals,
                unit: spec.unit,
            }),
    );
    if response.changed() {
        publish(ctx, param, shown / spec.display_scale, &response);
    }
}

/// Hand a widget's plain value back to the host.
///
/// A drag is one automation gesture, not sixty writes, so `begin_edit`
/// brackets it. The range normalization is where a skewed or logarithmic
/// parameter law gets its taper back.
fn publish(
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    plain: f32,
    response: &egui::Response,
) {
    if !response.changed() {
        return;
    }
    let id = param.id();
    if response.drag_started() {
        ctx.begin_edit(id);
    }
    ctx.set_param(id, param.info.range.normalize(f64::from(plain)));
    if response.drag_stopped() {
        ctx.end_edit(id);
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
