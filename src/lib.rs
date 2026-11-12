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

use egui::{Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use noda_audio::oklab::with_alpha;
use noda_audio::ui::theme::{Palette, Theme};
use noda_audio::ui::{ChannelLevel, Goniometer, Knob, LevelMeter, Reading};
use truce::prelude::*;
use truce_egui::{EditorUi, EguiEditor};

pub mod dsp;
mod presets;

use dsp::stereo_imager::{StereoSample, StereoScopeBuffer};
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

    /// Output L/R correlation. Unlike the level lanes this is published in
    /// its native -1..+1 range, because the editor needs the sign: negative
    /// means the wet cluster folds toward cancellation in mono.
    #[meter]
    pub meter_corr: MeterSlot,

    /// Lock-free ring buffer streaming stereo output samples to the goniometer.
    #[skip]
    pub scope: Arc<StereoScopeBuffer>,
}

/// The plugin descriptor is also its own per-instance DSP state, so the
/// truce shell preserves delay lines and sweep phases across a hot reload.
pub struct ThickVox {
    thickener: Thickener,
    /// Smoothed output correlation in -1..+1, updated once per block.
    correlation: f32,
    /// Downsample counter for feeding the scope buffer every 6 samples.
    scope_decimator: usize,
}

impl Default for ThickVox {
    fn default() -> Self {
        Self {
            thickener: Thickener::default(),
            // Silence is "perfectly mono" rather than "no reading": a fresh
            // instance should not light the phase warning before audio runs.
            correlation: 1.0,
            scope_decimator: 0,
        }
    }
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
    fn reset(state: &mut Self::DspState, params: &Self::Params, config: &AudioConfig) {
        state.thickener.prepare(config.sample_rate as f32);
        state.correlation = 1.0;
        state.scope_decimator = 0;
        params.scope.clear();
    }

    fn process(
        state: &mut Self::DspState,
        params: &Self::Params,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let bypassed = params.bypass.value();
        let frames = buffer.num_samples();

        // Output correlation sums. f64 keeps a long block of hot samples from
        // losing precision in the denominator; the ratio itself is f32.
        let mut lr = 0.0_f64;
        let mut ll = 0.0_f64;
        let mut rr = 0.0_f64;

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

            if state.scope_decimator == 0 {
                params.scope.push(fout[0], fout[1]);
                state.scope_decimator = 6;
            }
            state.scope_decimator -= 1;

            peak_l = peak_l.max(fout[0].abs());
            peak_r = peak_r.max(fout[1].abs());
            lr += f64::from(fout[0]) * f64::from(fout[1]);
            ll += f64::from(fout[0]) * f64::from(fout[0]);
            rr += f64::from(fout[1]) * f64::from(fout[1]);
        });

        // Meter lanes are 0..1 linear peaks; the editor turns them back
        // into dBFS. Clamp here so a hot bus can't overrun the lane.
        context.set_meter(params.meter_l.id(), peak_l.min(1.0));
        context.set_meter(params.meter_r.id(), peak_r.min(1.0));

        // Correlation is one sqrt per block. The one-pole coefficient follows
        // the block's sample count, so the reading settles in ~150 ms at any
        // host buffer size. Silence holds the previous value rather than
        // snapping to a random one.
        let denominator = (ll * rr).sqrt();
        if denominator > 1e-12 {
            let correlation = (lr / denominator).clamp(-1.0, 1.0) as f32;
            let sr = state.thickener.sample_rate();
            let alpha = (frames as f32 / (0.15 * sr)).clamp(0.02, 0.5);
            state.correlation += (correlation - state.correlation) * alpha;
        }
        context.set_meter(params.meter_corr.id(), state.correlation);

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
        Box::new(EguiEditor::with_ui(params, (1020, 650), ThickVoxEditor::new()).resizable(false))
    }
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

// Premium dark graphite + emerald color tokens matching the hardware visual
const VST_BG: Color32 = Color32::from_rgb(11, 14, 18);
const VST_CARD_BG: Color32 = Color32::from_rgb(17, 21, 28);
const VST_CARD_BORDER: Color32 = Color32::from_rgb(30, 37, 48);
const VST_EMERALD: Color32 = Color32::from_rgb(0, 229, 153);
const VST_TEXT_MAIN: Color32 = Color32::from_rgb(240, 244, 250);
const VST_TEXT_MUTED: Color32 = Color32::from_rgb(125, 138, 155);
const VST_TRACK_BG: Color32 = Color32::from_rgb(25, 31, 42);

/// State snapshot for A/B comparison.
#[derive(Clone, Copy)]
struct ParamSnapshot {
    mix: f64,
    detune_depth: f64,
    delay_spread: f64,
    width: f64,
    voice_level: f64,
    side_hpf: f64,
    air: f64,
    mid_only: bool,
    pair1_spread: f64,
    pair2_spread: f64,
    pair3_spread: f64,
    pair4_spread: f64,
}

impl ParamSnapshot {
    fn capture(ctx: &PluginContext<ThickVoxParams>) -> Self {
        Self {
            mix: ctx.mix.raw_target(),
            detune_depth: ctx.detune_depth.raw_target(),
            delay_spread: ctx.delay_spread.raw_target(),
            width: ctx.width.raw_target(),
            voice_level: ctx.voice_level.raw_target(),
            side_hpf: ctx.side_hpf.raw_target(),
            air: ctx.air.raw_target(),
            mid_only: ctx.mid_only.value(),
            pair1_spread: ctx.pair1_spread.raw_target(),
            pair2_spread: ctx.pair2_spread.raw_target(),
            pair3_spread: ctx.pair3_spread.raw_target(),
            pair4_spread: ctx.pair4_spread.raw_target(),
        }
    }

    fn restore(&self, ctx: &PluginContext<ThickVoxParams>) {
        ctx.automate(ctx.mix.id(), ctx.mix.info.range.normalize(self.mix));
        ctx.automate(
            ctx.detune_depth.id(),
            ctx.detune_depth.info.range.normalize(self.detune_depth),
        );
        ctx.automate(
            ctx.delay_spread.id(),
            ctx.delay_spread.info.range.normalize(self.delay_spread),
        );
        ctx.automate(ctx.width.id(), ctx.width.info.range.normalize(self.width));
        ctx.automate(
            ctx.voice_level.id(),
            ctx.voice_level.info.range.normalize(self.voice_level),
        );
        ctx.automate(
            ctx.side_hpf.id(),
            ctx.side_hpf.info.range.normalize(self.side_hpf),
        );
        ctx.automate(ctx.air.id(), ctx.air.info.range.normalize(self.air));
        ctx.automate(ctx.mid_only.id(), f64::from(u8::from(self.mid_only)));
        ctx.automate(
            ctx.pair1_spread.id(),
            ctx.pair1_spread.info.range.normalize(self.pair1_spread),
        );
        ctx.automate(
            ctx.pair2_spread.id(),
            ctx.pair2_spread.info.range.normalize(self.pair2_spread),
        );
        ctx.automate(
            ctx.pair3_spread.id(),
            ctx.pair3_spread.info.range.normalize(self.pair3_spread),
        );
        ctx.automate(
            ctx.pair4_spread.id(),
            ctx.pair4_spread.info.range.normalize(self.pair4_spread),
        );
    }
}

/// Custom egui editor built as a premium landscape VST plugin interface.
struct ThickVoxEditor {
    theme_installed: bool,
    preset_index: usize,
    applied_preset: usize,
    scope_samples: Vec<StereoSample>,
    // A/B comparison slots
    ab_slot: char,
    slot_a: Option<ParamSnapshot>,
    slot_b: Option<ParamSnapshot>,
    // Settings modal toggle
    show_settings: bool,
}

impl ThickVoxEditor {
    fn new() -> Self {
        let default = presets::default_index();
        Self {
            theme_installed: false,
            preset_index: default,
            applied_preset: default,
            scope_samples: Vec::with_capacity(crate::dsp::stereo_imager::DEFAULT_SCOPE_CAPACITY),
            ab_slot: 'A',
            slot_a: None,
            slot_b: None,
            show_settings: false,
        }
    }
}

impl EditorUi<ThickVoxParams> for ThickVoxEditor {
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &PluginContext<ThickVoxParams>) {
        if !self.theme_installed {
            let mut theme = Theme::dark();
            theme.palette.background = VST_BG;
            theme.palette.surface = VST_CARD_BG;
            theme.palette.border = VST_CARD_BORDER;
            theme.palette.primary = VST_EMERALD;
            theme.palette.meter_ok = VST_EMERALD;
            theme.palette.track = VST_TRACK_BG;
            theme.palette.foreground = VST_TEXT_MAIN;
            theme.palette.text_dim = VST_TEXT_MUTED;
            theme.palette.channel_monitor = VST_EMERALD;
            theme.install(ui.ctx());
            self.theme_installed = true;
        }

        // Fetch fresh scope samples into the reusable scratch vector
        ctx.scope.read_samples(&mut self.scope_samples);

        // Continuous refresh for smooth meter ballistics and goniometer animation
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(30));

        let theme = Theme::of(ui);
        let palette = theme.palette.clone();

        let window_rect = ui.max_rect();
        ui.painter().rect_filled(window_rect, 0.0, VST_BG);

        // Frame padding
        let outer = window_rect.shrink(14.0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(outer), |ui| {
            self.body(ui, ctx, &palette, &theme);
        });

        // Optional settings popover
        if self.show_settings {
            egui::Window::new("ThickVox Settings")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ui.ctx(), |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.label(
                        RichText::new("THICKVOX")
                            .strong()
                            .color(VST_EMERALD)
                            .size(14.0),
                    );
                    ui.label("8-Voice Vocal Thickener & ADT Stereo Processor");
                    ui.separator();
                    ui.label("• Latency: 0 samples (zero latency dry path)");
                    ui.label("• Detune: 4 mirrored pairs (±3c, ±6c, ±9c, ±12c)");
                    ui.label("• Shaping: Mid/Side highpass filter + Air shelf");
                    ui.label("• Real-time: Lock-free, zero-allocation DSP engine");
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        self.show_settings = false;
                    }
                });
        }
    }
}

impl ThickVoxEditor {
    fn body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &PluginContext<ThickVoxParams>,
        palette: &Palette,
        theme: &Theme,
    ) {
        // 1. TOP TOOLBAR
        self.top_toolbar(ui, ctx, palette);
        ui.add_space(4.0);

        // 2. SUBTITLE ROW
        self.subtitle_row(ui);
        ui.add_space(10.0);

        // 3. MAIN WORKSPACE (Upper Grid + Bottom Tray)
        let avail_w = ui.available_width();
        let avail_h = ui.available_height();

        let bottom_tray_h = 136.0_f32;
        let gap = 12.0_f32;
        let upper_grid_h = (avail_h - bottom_tray_h - gap).max(380.0);

        // Main upper grid: [ Left Column (Thicken / Pair Spread) ] [ Center Column (Image / Tone) ] [ Right Column (Output) ]
        let output_card_w = 160.0_f32;
        let left_center_w = (avail_w - output_card_w - gap * 2.0).max(640.0);
        let col1_w = (left_center_w * 0.58).round();
        let col2_w = left_center_w - col1_w;

        let row_h = ((upper_grid_h - gap) * 0.5).round();

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;

            // Column 1: THICKEN (top) and PAIR SPREAD (bottom)
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = gap;

                // CARD 1: THICKEN
                render_card(ui, "THICKEN", egui::vec2(col1_w, row_h), palette, |ui| {
                    ui.horizontal_centered(|ui| {
                        let slot_w = ui.available_width() / 3.0;
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let knob_d = 62.0;

                        ui.allocate_ui_with_layout(
                            egui::vec2(slot_w, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                percent_knob(ui, ctx, &ctx.mix, "Mix", (0.0, 100.0), 100.0, knob_d);
                            },
                        );
                        ui.allocate_ui_with_layout(
                            egui::vec2(slot_w, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                unit_knob(
                                    ui,
                                    ctx,
                                    &ctx.detune_depth,
                                    "Detune",
                                    (0.5, 2.0),
                                    2,
                                    "×",
                                    knob_d,
                                );
                            },
                        );
                        ui.allocate_ui_with_layout(
                            egui::vec2(slot_w, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                unit_knob(
                                    ui,
                                    ctx,
                                    &ctx.delay_spread,
                                    "Delay",
                                    (0.7, 1.3),
                                    2,
                                    "×",
                                    knob_d,
                                );
                            },
                        );
                    });
                });

                // CARD 3: PAIR SPREAD
                render_card(
                    ui,
                    "PAIR SPREAD",
                    egui::vec2(col1_w, row_h),
                    palette,
                    |ui| {
                        ui.horizontal_centered(|ui| {
                            let slot_w = ui.available_width() / 4.0;
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let knob_d = 50.0;

                            ui.allocate_ui_with_layout(
                                egui::vec2(slot_w, ui.available_height()),
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    percent_knob(
                                        ui,
                                        ctx,
                                        &ctx.pair1_spread,
                                        "+/ 3c",
                                        (0.0, 100.0),
                                        100.0,
                                        knob_d,
                                    );
                                },
                            );
                            ui.allocate_ui_with_layout(
                                egui::vec2(slot_w, ui.available_height()),
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    percent_knob(
                                        ui,
                                        ctx,
                                        &ctx.pair2_spread,
                                        "+/ 6c",
                                        (0.0, 100.0),
                                        100.0,
                                        knob_d,
                                    );
                                },
                            );
                            ui.allocate_ui_with_layout(
                                egui::vec2(slot_w, ui.available_height()),
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    percent_knob(
                                        ui,
                                        ctx,
                                        &ctx.pair3_spread,
                                        "+/ 9c",
                                        (0.0, 100.0),
                                        100.0,
                                        knob_d,
                                    );
                                },
                            );
                            ui.allocate_ui_with_layout(
                                egui::vec2(slot_w, ui.available_height()),
                                egui::Layout::top_down(egui::Align::Center),
                                |ui| {
                                    percent_knob(
                                        ui,
                                        ctx,
                                        &ctx.pair4_spread,
                                        "+/ 12c",
                                        (0.0, 100.0),
                                        100.0,
                                        knob_d,
                                    );
                                },
                            );
                        });
                    },
                );
            });

            // Column 2: IMAGE (top) and TONE (bottom)
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = gap;

                // CARD 2: IMAGE
                render_card(ui, "IMAGE", egui::vec2(col2_w, row_h), palette, |ui| {
                    ui.horizontal_centered(|ui| {
                        let knob_d = 62.0;

                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width() * 0.5, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                percent_knob(
                                    ui,
                                    ctx,
                                    &ctx.width,
                                    "Width",
                                    (0.0, 200.0),
                                    100.0,
                                    knob_d,
                                );
                            },
                        );

                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), ui.available_height()),
                            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                            |ui| {
                                let mut mid_only = ctx.mid_only.value();
                                if emerald_checkbox(ui, &mut mid_only, "Mid Only").changed() {
                                    ctx.automate(ctx.mid_only.id(), f64::from(u8::from(mid_only)));
                                }
                            },
                        );
                    });
                });

                // CARD 4: TONE
                render_card(ui, "TONE", egui::vec2(col2_w, row_h), palette, |ui| {
                    ui.horizontal_centered(|ui| {
                        let slot_w = ui.available_width() * 0.5;
                        let knob_d = 62.0;

                        ui.allocate_ui_with_layout(
                            egui::vec2(slot_w, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                unit_knob(
                                    ui,
                                    ctx,
                                    &ctx.side_hpf,
                                    "Side HPF",
                                    (20.0, 300.0),
                                    0,
                                    "Hz",
                                    knob_d,
                                );
                            },
                        );
                        ui.allocate_ui_with_layout(
                            egui::vec2(slot_w, ui.available_height()),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                db_knob(ui, ctx, &ctx.air, "Air", (0.0, 6.0), knob_d);
                            },
                        );
                    });
                });
            });

            // Column 3: OUTPUT (tall card spanning both rows)
            render_card(
                ui,
                "OUTPUT",
                egui::vec2(output_card_w, upper_grid_h),
                palette,
                |ui| {
                    ui.vertical_centered(|ui| {
                        let levels = [
                            ChannelLevel::from_linear(ctx.get_meter(ctx.meter_l.id())),
                            ChannelLevel::from_linear(ctx.get_meter(ctx.meter_r.id())),
                        ];
                        let meter = LevelMeter::stereo(&levels)
                            .scale(true)
                            .readout(false)
                            .clip(true)
                            .id(egui::Id::new("thickvox-output-meter"));
                        let meter_width = meter.desired_size(theme).x;

                        let meter_height = (ui.available_height() - 42.0).max(120.0);
                        let (strip, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), meter_height),
                            Sense::hover(),
                        );
                        let meter_rect = Rect::from_center_size(
                            strip.center(),
                            egui::vec2(meter_width, meter_height),
                        );
                        meter.paint(ui, theme, meter_rect);

                        ui.add_space(8.0);
                        correlation_strip(
                            ui,
                            ctx.get_meter(ctx.meter_corr.id()),
                            ui.available_width() - 8.0,
                        );
                    });
                },
            );
        });

        ui.add_space(gap);

        // 4. BOTTOM TRAY: STEREO IMAGER + VOICE LEVEL
        self.bottom_tray(ui, ctx, egui::vec2(avail_w, bottom_tray_h), palette);
    }

    /// Top toolbar matching the reference image:
    /// Mac window dots, brand badge, preset navigator, A/B toggle, settings, and power button.
    fn top_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &PluginContext<ThickVoxParams>,
        palette: &Palette,
    ) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;

            // Window dots (decorative macOS traffic lights)
            let (dots_rect, _) = ui.allocate_exact_size(egui::vec2(44.0, 24.0), Sense::hover());
            let painter = ui.painter();
            let dot_y = dots_rect.center().y;
            painter.circle_filled(
                egui::pos2(dots_rect.left() + 6.0, dot_y),
                4.5,
                Color32::from_rgb(255, 95, 86),
            );
            painter.circle_filled(
                egui::pos2(dots_rect.left() + 20.0, dot_y),
                4.5,
                Color32::from_rgb(255, 189, 46),
            );
            painter.circle_filled(
                egui::pos2(dots_rect.left() + 34.0, dot_y),
                4.5,
                Color32::from_rgb(39, 201, 63),
            );

            ui.add_space(4.0);

            // Glowing emerald lamp
            let (lamp_rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 24.0), Sense::hover());
            let lamp_center = lamp_rect.center();
            let lamp_p = ui.painter();
            if !ctx.bypass.value() {
                lamp_p.circle_filled(lamp_center, 7.5, with_alpha(VST_EMERALD, 0.25));
                lamp_p.circle_filled(lamp_center, 4.5, VST_EMERALD);
            } else {
                lamp_p.circle_filled(lamp_center, 4.5, Color32::from_rgb(60, 70, 80));
            }

            // Brand title + subtitle
            ui.label(
                RichText::new("THICKVOX")
                    .font(FontId::proportional(14.0))
                    .strong()
                    .color(VST_TEXT_MAIN),
            );
            ui.label(
                RichText::new("|")
                    .font(FontId::proportional(13.0))
                    .color(Color32::from_rgb(65, 75, 90)),
            );
            ui.label(
                RichText::new("8-VOICE VOCAL THICKENER")
                    .font(FontId::proportional(11.0))
                    .color(VST_TEXT_MUTED),
            );

            // Right-aligned toolbar controls
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;

                // Power ON / BYP toggle pill
                power_switch(ui, ctx);

                // Settings gear
                let gear_btn = egui::Button::new(
                    RichText::new("⚙")
                        .font(FontId::proportional(12.0))
                        .color(VST_TEXT_MUTED),
                )
                .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 8))
                .stroke(Stroke::new(1.0_f32, VST_CARD_BORDER))
                .corner_radius(CornerRadius::same(5))
                .min_size(egui::vec2(28.0, 24.0));
                if ui.add(gear_btn).on_hover_text("Settings & Info").clicked() {
                    self.show_settings = !self.show_settings;
                }

                // A/B comparison pill
                let ab_text = format!("A/B [{}]", self.ab_slot);
                let ab_btn = egui::Button::new(
                    RichText::new(ab_text)
                        .font(FontId::proportional(10.5))
                        .color(VST_TEXT_MAIN),
                )
                .fill(Color32::from_rgba_unmultiplied(255, 255, 255, 10))
                .stroke(Stroke::new(1.0_f32, VST_CARD_BORDER))
                .corner_radius(CornerRadius::same(5))
                .min_size(egui::vec2(48.0, 24.0));
                if ui
                    .add(ab_btn)
                    .on_hover_text("Toggle between State A and State B")
                    .clicked()
                {
                    self.toggle_ab(ctx);
                }

                // Preset selector pill: < [Name] >
                self.preset_navigator(ui, ctx, palette);
            });
        });
    }

    /// Subtitle banner with small waveform glyph
    fn subtitle_row(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            // Small emerald waveform glyph
            let (glyph_rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 14.0), Sense::hover());
            let gp = ui.painter();
            let bars = [4.0, 9.0, 13.0, 8.0, 3.0];
            for (i, &h) in bars.iter().enumerate() {
                let x = glyph_rect.left() + i as f32 * 3.2;
                let mid_y = glyph_rect.center().y;
                gp.line_segment(
                    [
                        egui::pos2(x, mid_y - h * 0.5),
                        egui::pos2(x, mid_y + h * 0.5),
                    ],
                    Stroke::new(1.8_f32, VST_EMERALD),
                );
            }

            ui.label(
                RichText::new("4 mirrored pairs · ±3 to ±12 cents · dry path untouched")
                    .font(FontId::proportional(10.5))
                    .color(VST_TEXT_MUTED),
            );
        });
    }

    /// Preset pill navigator with cycle arrows and popup selector
    fn preset_navigator(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &PluginContext<ThickVoxParams>,
        _palette: &Palette,
    ) {
        let bank = presets::factory_presets();
        if bank.is_empty() {
            return;
        }

        self.preset_index = self.preset_index.min(bank.len() - 1);
        self.applied_preset = self.applied_preset.min(bank.len() - 1);

        if let Some(found) = bank.iter().position(|preset| presets::matches(preset, ctx)) {
            self.preset_index = found;
            self.applied_preset = found;
        }

        let is_edited = !presets::matches(&bank[self.preset_index], ctx);

        let frame = egui::Frame::NONE
            .fill(VST_CARD_BG)
            .stroke(Stroke::new(1.0_f32, VST_CARD_BORDER))
            .corner_radius(CornerRadius::same(5))
            .inner_margin(egui::Margin::symmetric(4, 2));

        frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;

                // Previous preset button
                let prev = ui
                    .add(
                        egui::Button::new(
                            RichText::new("◀")
                                .font(FontId::proportional(9.0))
                                .color(VST_TEXT_MUTED),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::NONE)
                        .min_size(egui::vec2(16.0, 18.0)),
                    )
                    .clicked();
                if prev {
                    let prev_idx = if self.preset_index == 0 {
                        bank.len() - 1
                    } else {
                        self.preset_index - 1
                    };
                    self.preset_index = prev_idx;
                    self.applied_preset = prev_idx;
                    presets::apply(&bank[prev_idx], ctx);
                }

                // Preset Name Dropdown
                let mut current_name = bank[self.preset_index].name.clone();
                if is_edited {
                    current_name.push_str(" *");
                }

                egui::ComboBox::from_id_salt("thickvox_preset_pill_picker")
                    .selected_text(
                        RichText::new(current_name)
                            .font(FontId::proportional(11.0))
                            .strong()
                            .color(VST_TEXT_MAIN),
                    )
                    .width(135.0)
                    .show_ui(ui, |ui| {
                        for (idx, preset) in bank.iter().enumerate() {
                            let mut selected = self.preset_index == idx;
                            let label = format!("{}  ·  {}", preset.name, preset.category);
                            if ui.selectable_value(&mut selected, true, label).clicked() {
                                self.preset_index = idx;
                                self.applied_preset = idx;
                                presets::apply(preset, ctx);
                            }
                        }
                    });

                // Next preset button
                let next = ui
                    .add(
                        egui::Button::new(
                            RichText::new("▶")
                                .font(FontId::proportional(9.0))
                                .color(VST_TEXT_MUTED),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::NONE)
                        .min_size(egui::vec2(16.0, 18.0)),
                    )
                    .clicked();
                if next {
                    let next_idx = (self.preset_index + 1) % bank.len();
                    self.preset_index = next_idx;
                    self.applied_preset = next_idx;
                    presets::apply(&bank[next_idx], ctx);
                }
            });
        });
    }

    /// Toggle between A and B comparison slots
    fn toggle_ab(&mut self, ctx: &PluginContext<ThickVoxParams>) {
        let current = ParamSnapshot::capture(ctx);
        if self.ab_slot == 'A' {
            self.slot_a = Some(current);
            self.ab_slot = 'B';
            if let Some(target) = self.slot_b {
                target.restore(ctx);
            } else {
                self.slot_b = Some(current);
            }
        } else {
            self.slot_b = Some(current);
            self.ab_slot = 'A';
            if let Some(target) = self.slot_a {
                target.restore(ctx);
            }
        }
    }

    /// Bottom Tray spanning full width: Stereo Imager + Voice Level Slider
    fn bottom_tray(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &PluginContext<ThickVoxParams>,
        size: Vec2,
        _palette: &Palette,
    ) {
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let painter = ui.painter();

        // Card frame
        painter.rect_filled(rect, CornerRadius::same(8), VST_CARD_BG);
        painter.rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(1.0_f32, VST_CARD_BORDER),
            StrokeKind::Inside,
        );

        let content_rect = rect.shrink(10.0);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content_rect));

        child.horizontal_centered(|ui| {
            let total_w = ui.available_width();
            let left_w = (total_w * 0.28).max(180.0);
            let center_w = (total_w * 0.40).max(220.0);
            let right_w = total_w - left_w - center_w;

            // 1. LEFT: Stereo Imager title & glyph
            ui.allocate_ui_with_layout(
                egui::vec2(left_w, ui.available_height()),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add_space(8.0);
                    // 4-bar waveform icon
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::vec2(22.0, 24.0), Sense::hover());
                    let ip = ui.painter();
                    let bar_heights = [7.0, 15.0, 21.0, 11.0];
                    for (i, &h) in bar_heights.iter().enumerate() {
                        let x = icon_rect.left() + 2.0 + i as f32 * 5.0;
                        let mid_y = icon_rect.center().y;
                        ip.line_segment(
                            [
                                egui::pos2(x, mid_y - h * 0.5),
                                egui::pos2(x, mid_y + h * 0.5),
                            ],
                            Stroke::new(2.2_f32, VST_EMERALD),
                        );
                    }
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("STEREO IMAGER")
                            .font(FontId::proportional(12.0))
                            .strong()
                            .color(VST_TEXT_MAIN),
                    );
                },
            );

            // 2. CENTER: Goniometer vectorscope
            ui.allocate_ui_with_layout(
                egui::vec2(center_w, ui.available_height()),
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                |ui| {
                    let scope_size = (ui.available_height() - 8.0).min(136.0);
                    ui.add(
                        Goniometer::new(&self.scope_samples)
                            .size(scope_size)
                            .gain(1.8)
                            .show_grid(true)
                            .show_correlation(false),
                    );
                },
            );

            // 3. RIGHT: Voice Level slider
            ui.allocate_ui_with_layout(
                egui::vec2(right_w, ui.available_height()),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.add_space(14.0);
                    let slider_w = (right_w - 120.0).clamp(120.0, 200.0);
                    voice_level_slider(ui, ctx, slider_w);
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("VOICE LEVEL")
                            .font(FontId::proportional(11.0))
                            .strong()
                            .color(VST_TEXT_MUTED),
                    );
                },
            );
        });
    }
}

/// Helper that renders a card container with rounded corners, subtle border,
/// and signature emerald pill header.
fn render_card<R>(
    ui: &mut egui::Ui,
    title: &str,
    size: Vec2,
    palette: &Palette,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();

    // Background & border
    painter.rect_filled(rect, CornerRadius::same(8), VST_CARD_BG);
    painter.rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0_f32, VST_CARD_BORDER),
        StrokeKind::Inside,
    );

    // Accent pill
    let pill_rect = Rect::from_min_size(
        egui::pos2(rect.left() + 14.0, rect.top() + 14.0),
        egui::vec2(3.5, 13.0),
    );
    painter.rect_filled(pill_rect, CornerRadius::same(2), VST_EMERALD);

    // Header Title
    painter.text(
        egui::pos2(rect.left() + 23.0, rect.top() + 13.0),
        egui::Align2::LEFT_TOP,
        title,
        FontId::proportional(11.5),
        palette.foreground,
    );

    // Inner content area
    let content_rect = Rect::from_min_max(
        egui::pos2(rect.left() + 12.0, rect.top() + 38.0),
        egui::pos2(rect.right() - 12.0, rect.bottom() - 10.0),
    );

    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(content_rect));
    content(&mut child)
}

/// Custom styled emerald checkbox
fn emerald_checkbox(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let font = FontId::proportional(12.0);
    let box_size = 18.0_f32;
    let text_size = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), VST_TEXT_MAIN)
        .size();

    let total_w = box_size + 8.0 + text_size.x;
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(total_w, 22.0), Sense::click());

    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }

    let box_rect = Rect::from_center_size(
        egui::pos2(rect.left() + box_size * 0.5, rect.center().y),
        egui::vec2(box_size, box_size),
    );

    let painter = ui.painter();
    if *value {
        painter.rect_filled(box_rect, CornerRadius::same(4), VST_EMERALD);
        // White checkmark
        let p1 = egui::pos2(box_rect.left() + 4.5, box_rect.center().y);
        let p2 = egui::pos2(box_rect.left() + 7.5, box_rect.bottom() - 5.0);
        let p3 = egui::pos2(box_rect.right() - 4.5, box_rect.top() + 5.0);
        painter.line_segment(
            [p1, p2],
            Stroke::new(2.0_f32, Color32::from_rgb(12, 16, 22)),
        );
        painter.line_segment(
            [p2, p3],
            Stroke::new(2.0_f32, Color32::from_rgb(12, 16, 22)),
        );
    } else {
        painter.rect_filled(
            box_rect,
            CornerRadius::same(4),
            Color32::from_rgb(22, 27, 36),
        );
        painter.rect_stroke(
            box_rect,
            CornerRadius::same(4),
            Stroke::new(1.2_f32, Color32::from_rgb(60, 72, 90)),
            StrokeKind::Inside,
        );
    }

    painter.text(
        egui::pos2(box_rect.right() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        VST_TEXT_MAIN,
    );

    response.on_hover_text("Collapse the wet cluster into Mid for mono-safe density")
}

/// Horizontal slider for Voice Level
fn voice_level_slider(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    width: f32,
) -> egui::Response {
    let desired_size = egui::vec2(width, 24.0);
    let (rect, response) = ui.allocate_exact_size(desired_size, Sense::click_and_drag());

    let param = &ctx.voice_level;
    let min = param.info.range.min() as f32; // -24.0
    let max = param.info.range.max() as f32; // 6.0
    let default = param.info.default_plain as f32; // -8.0
    let mut db = param.raw_target() as f32;

    let id = param.id();
    if response.drag_started() {
        ctx.begin_edit(id);
    }
    if response.dragged() || response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            db = min + t * (max - min);
            ctx.set_param(id, param.info.range.normalize(f64::from(db)));
        }
    }
    if response.double_clicked() {
        db = default;
        ctx.set_param(id, param.info.range.normalize(f64::from(db)));
    }
    if response.drag_stopped() {
        ctx.end_edit(id);
    }

    let painter = ui.painter();
    let track_rect = Rect::from_center_size(rect.center(), egui::vec2(rect.width(), 6.0));
    painter.rect_filled(track_rect, CornerRadius::same(3), VST_TRACK_BG);

    let t = ((db - min) / (max - min)).clamp(0.0, 1.0);
    let fill_rect = Rect::from_min_max(
        track_rect.min,
        egui::pos2(
            track_rect.left() + t * track_rect.width(),
            track_rect.bottom(),
        ),
    );
    if fill_rect.width() > 1.0 {
        painter.rect_filled(fill_rect, CornerRadius::same(3), VST_EMERALD);
    }

    // Thumb handle
    let thumb_center = egui::pos2(track_rect.left() + t * track_rect.width(), rect.center().y);
    painter.circle_filled(
        thumb_center + egui::vec2(0.0, 1.0),
        7.0,
        Color32::from_black_alpha(80),
    );
    painter.circle_filled(thumb_center, 7.0, Color32::WHITE);
    painter.circle_stroke(
        thumb_center,
        7.0,
        Stroke::new(1.0_f32, Color32::from_rgb(200, 215, 230)),
    );

    response.on_hover_text(format!(
        "Voice Level: {db:+.1} dB\nDouble-click to reset ({default:+.1} dB)"
    ))
}

/// Output correlation strip underneath the stereo meter
fn correlation_strip(ui: &mut egui::Ui, correlation: f32, width: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 24.0), Sense::hover());
    let bar = Rect::from_min_size(
        egui::pos2(rect.left() + 38.0, rect.top() + 6.0),
        egui::vec2(rect.width() - 84.0, 6.5),
    );

    let color = if correlation < -0.05 {
        Color32::from_rgb(255, 80, 80)
    } else if correlation < 0.35 {
        Color32::from_rgb(255, 180, 50)
    } else {
        VST_EMERALD
    };

    let painter = ui.painter();

    // Label on left
    painter.text(
        egui::pos2(rect.left(), rect.center().y),
        egui::Align2::LEFT_CENTER,
        "CORR",
        FontId::proportional(10.0),
        VST_TEXT_MUTED,
    );

    // Track
    painter.rect_filled(bar, CornerRadius::same(3), VST_TRACK_BG);

    let center = bar.center().x;
    let needle = bar.left() + (correlation.clamp(-1.0, 1.0) + 1.0) * 0.5 * bar.width();
    if (needle - center).abs() > 0.5 {
        let fill = Rect::from_min_max(
            egui::pos2(center.min(needle), bar.top() + 0.5),
            egui::pos2(center.max(needle), bar.bottom() - 0.5),
        );
        painter.rect_filled(fill, CornerRadius::same(1), color);
    }
    painter.line_segment(
        [
            egui::pos2(center, bar.top() - 1.0),
            egui::pos2(center, bar.bottom() + 1.0),
        ],
        Stroke::new(1.0_f32, Color32::from_rgb(70, 80, 95)),
    );

    // Readout on right
    painter.text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        format!("{correlation:+.2}"),
        FontId::monospace(10.5),
        color,
    );

    response.on_hover_text(
        "Output L/R correlation\n\
         +1.00 = mono · 0.00 = wide stereo · negative = phase cancellation",
    )
}

/// The chassis power switch pill: "ON" in bright emerald when engaged, "BYP" in dark red when bypassed.
fn power_switch(ui: &mut egui::Ui, ctx: &PluginContext<ThickVoxParams>) {
    let engaged = !ctx.bypass.value();
    let (label, bg, fg, border) = if engaged {
        (
            "ON",
            VST_EMERALD,
            Color32::from_rgb(10, 15, 20),
            VST_EMERALD,
        )
    } else {
        (
            "BYP",
            Color32::from_rgb(45, 16, 20),
            Color32::from_rgb(255, 80, 80),
            Color32::from_rgb(180, 40, 50),
        )
    };

    let button = egui::Button::new(
        RichText::new(label)
            .font(FontId::proportional(11.0))
            .strong()
            .color(fg),
    )
    .fill(bg)
    .stroke(Stroke::new(1.0_f32, border))
    .corner_radius(CornerRadius::same(5))
    .min_size(egui::vec2(38.0, 24.0));

    if ui
        .add(button)
        .on_hover_text(if engaged {
            "Bypass ThickVox"
        } else {
            "Engage ThickVox"
        })
        .clicked()
    {
        ctx.automate(ctx.bypass.id(), f64::from(u8::from(engaged)));
    }
}

// ---------------------------------------------------------------------------
// Editor helpers
// ---------------------------------------------------------------------------

/// A 0..100% knob with custom diameter
fn percent_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    label: &'static str,
    display: (f32, f32),
    display_scale: f32,
    diameter: f32,
) {
    param_knob(
        ui,
        ctx,
        param,
        KnobSpec {
            label,
            display,
            display_scale,
            reading: Reading::Number {
                decimals: 0,
                unit: Some("%"),
            },
        },
        diameter,
    );
}

/// A knob in the parameter's own units (Hz, cents multiplier, ...) with custom diameter
fn unit_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    label: &'static str,
    display: (f32, f32),
    decimals: usize,
    unit: &'static str,
    diameter: f32,
) {
    param_knob(
        ui,
        ctx,
        param,
        KnobSpec {
            label,
            display,
            display_scale: 1.0,
            reading: Reading::Number {
                decimals,
                unit: Some(unit),
            },
        },
        diameter,
    );
}

/// A knob that reads in decibels with custom diameter
fn db_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    label: &'static str,
    display: (f32, f32),
    diameter: f32,
) {
    param_knob(
        ui,
        ctx,
        param,
        KnobSpec {
            label,
            display,
            display_scale: 1.0,
            reading: Reading::Db,
        },
        diameter,
    );
}

/// How a parameter is presented on a knob.
struct KnobSpec {
    label: &'static str,
    display: (f32, f32),
    display_scale: f32,
    reading: Reading<'static>,
}

fn param_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    spec: KnobSpec,
    diameter: f32,
) {
    let mut shown = (param.raw_target() as f32) * spec.display_scale;
    let response = ui.add(
        Knob::new(&mut shown)
            .range(spec.display.0, spec.display.1)
            .label(spec.label)
            .reading(spec.reading)
            .diameter(diameter)
            .scale(true)
            .reset(Some((param.info.default_plain as f32) * spec.display_scale)),
    );
    publish(ctx, param, shown / spec.display_scale, &response);
}

/// Hand a widget's plain value back to the host.
///
/// A drag is one automation gesture, not sixty writes, so `begin_edit`
/// brackets it. The bracket is opened and closed on the drag itself rather
/// than on a value change, otherwise a drag that ends on a frame where the
/// value happens to be static would leave the gesture open forever. The
/// range normalization is where a skewed or logarithmic parameter law gets
/// its taper back.
fn publish(
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    plain: f32,
    response: &egui::Response,
) {
    let id = param.id();
    if response.drag_started() {
        ctx.begin_edit(id);
    }
    if response.changed() {
        ctx.set_param(id, param.info.range.normalize(f64::from(plain)));
    }
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
