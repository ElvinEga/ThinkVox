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

use egui::{Color32, Rect, RichText, Sense};
use noda_audio::oklab::with_alpha;
use noda_audio::taper::Taper;
use noda_audio::ui::theme::{Palette, Theme};
use noda_audio::ui::{
    ChannelLevel, Fader, Knob, LevelMeter, Preset, PresetBrowser, RackFrame, Reading, SectionHeader,
};
use truce::prelude::*;
use truce_egui::{EditorUi, EguiEditor};

pub mod dsp;
mod presets;

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
}

/// The plugin descriptor is also its own per-instance DSP state, so the
/// truce shell preserves delay lines and sweep phases across a hot reload.
pub struct ThickVox {
    thickener: Thickener,
    /// Smoothed output correlation in -1..+1, updated once per block.
    correlation: f32,
}

impl Default for ThickVox {
    fn default() -> Self {
        Self {
            thickener: Thickener::default(),
            // Silence is "perfectly mono" rather than "no reading": a fresh
            // instance should not light the phase warning before audio runs.
            correlation: 1.0,
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
    fn reset(state: &mut Self::DspState, _params: &Self::Params, config: &AudioConfig) {
        state.thickener.prepare(config.sample_rate as f32);
        state.correlation = 1.0;
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
            let sr = state.thickener.sample_rate() as f32;
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
        Box::new(
            EguiEditor::with_ui(params, (480, 700), ThickVoxEditor::new())
                .resizable(true)
                .min_size((464, 684))
                .max_size((1100, 940)),
        )
    }
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// Custom egui editor built from NodaAudio controls.
///
/// One rack chassis holds the whole editor: the four sections that shape the
/// cluster on the left, the master strip on the right. The chassis supplies
/// the power lamp, title badge and screw heads; `SectionHeader` supplies the
/// group titles and their accent rules, so the type and colour scale come
/// from the library rather than being redrawn here.
struct ThickVoxEditor {
    /// The theme writes into the egui context, so it only needs installing
    /// once per window (not once per frame).
    theme_installed: bool,
    /// NodaAudio's view of the factory bank, built once from `presets/`.
    preset_items: Vec<Preset>,
    /// The preset the picker currently names.
    preset_index: usize,
    /// The index whose values were last applied (or last matched). A picker
    /// index that differs from this one means the user chose a preset this
    /// frame, rather than the host restoring state underneath us.
    applied_preset: usize,
}

impl ThickVoxEditor {
    fn new() -> Self {
        let preset_items = presets::factory_presets()
            .iter()
            .map(|preset| {
                let mut item = Preset::new(preset.category, preset.name.as_str());
                if let Some(author) = &preset.author {
                    item = item.author(author.as_str());
                }
                item
            })
            .collect();
        let default = presets::default_index();
        Self {
            theme_installed: false,
            preset_items,
            preset_index: default,
            // Start "applied": opening the editor must not overwrite state
            // the host just restored. The sync pass corrects the label if
            // those values match a different factory preset.
            applied_preset: default,
        }
    }

    /// Width of the control column. The pair-spread row is the widest thing
    /// in it: four 70 pt knobs plus the 10 pt gaps between them.
    const CONTROLS_WIDTH: f32 = 316.0;

    /// Width of the master strip: a stereo meter plus the wet fader, with
    /// room for the meter's dB scale on its right.
    const OUTPUT_WIDTH: f32 = 96.0;

    /// Vertical room the chassis takes away from the body: its two 12 pt
    /// margins, the title badge row above `show` and the gap under it.
    const CHASSIS_CHROME: f32 = 52.0;
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

        // The chassis is the editor. Sizing it off the window rather than
        // off its contents keeps the metal reaching the edges when a host
        // hands the window more room than the controls need.
        let outer = ui.max_rect().shrink(8.0);
        let body_height = (outer.height() - Self::CHASSIS_CHROME).max(0.0);

        ui.scope_builder(egui::UiBuilder::new().max_rect(outer), |ui| {
            RackFrame::new("THICKVOX")
                .subtitle("8-VOICE VOCAL THICKENER")
                .power_on(!ctx.bypass.value())
                .height(body_height)
                .show(ui, |ui| self.body(ui, ctx, &palette, &theme));
        });
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
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("4 mirrored pairs · ±3 to ±12 cents · dry path untouched")
                    .size(9.5)
                    .color(palette.text_dim),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                power_switch(ui, ctx, palette);
            });
        });
        ui.add_space(theme.size.gap());

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            self.preset_row(ui, ctx, palette);
        });
        ui.add_space(theme.size.gap());

        // Two columns with the controls centered as a block, so a wider
        // window grows symmetric gutters instead of one gap after the meter.
        let gap = theme.size.gap() * 2.0;
        let block = Self::CONTROLS_WIDTH + gap + Self::OUTPUT_WIDTH;
        let gutter = ((ui.available_width() - block) * 0.5).max(0.0);

        ui.horizontal_top(|ui| {
            ui.add_space(gutter);

            ui.vertical(|ui| {
                ui.set_width(Self::CONTROLS_WIDTH);
                ui.spacing_mut().item_spacing.x = 10.0;
                let knob_width = {
                    let mut probe = 0.0;
                    Knob::new(&mut probe).desired_size(theme).x
                };

                section(ui, "THICKEN", palette.primary, knob_width, 3, |ui| {
                    percent_knob(ui, ctx, &ctx.mix, "Mix", (0.0, 100.0), 100.0);
                    unit_knob(ui, ctx, &ctx.detune_depth, "Detune", (0.5, 2.0), 2, "×");
                    unit_knob(ui, ctx, &ctx.delay_spread, "Delay", (0.7, 1.3), 2, "×");
                });

                section(ui, "IMAGE", palette.channel_monitor, knob_width, 1, |ui| {
                    // 0..200%: 100% is the untouched cluster, 200% is
                    // hyper-wide, 0% folds the wet bus to mono.
                    percent_knob(ui, ctx, &ctx.width, "Width", (0.0, 200.0), 100.0);
                    // The toggle is the only other thing in this row, so it
                    // sits against the far edge instead of floating after
                    // the dial with a ragged gap.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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

                section(ui, "PAIR SPREAD", palette.meter_ok, knob_width, 4, |ui| {
                    percent_knob(ui, ctx, &ctx.pair1_spread, "+/-3c", (0.0, 100.0), 100.0);
                    percent_knob(ui, ctx, &ctx.pair2_spread, "+/-6c", (0.0, 100.0), 100.0);
                    percent_knob(ui, ctx, &ctx.pair3_spread, "+/-9c", (0.0, 100.0), 100.0);
                    percent_knob(ui, ctx, &ctx.pair4_spread, "+/-12c", (0.0, 100.0), 100.0);
                });

                section(ui, "TONE", palette.meter_warn, knob_width, 2, |ui| {
                    unit_knob(ui, ctx, &ctx.side_hpf, "Side HPF", (20.0, 300.0), 0, "Hz");
                    db_knob(ui, ctx, &ctx.air, "Air", (0.0, 6.0));
                });
            });

            ui.add_space(gap);
            self.master_strip(ui, ctx, palette, theme);
        });
    }

    /// The factory preset picker plus a "modified" marker.
    ///
    /// The browser only reports an index; writing the values is our job.
    /// Applying is gated on `index != applied_preset`, so a restored host
    /// state survives the first paint, while a real pick writes every
    /// parameter in one `begin_edit` / `set_param` / `end_edit` gesture.
    fn preset_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &PluginContext<ThickVoxParams>,
        palette: &Palette,
    ) {
        let bank = presets::factory_presets();
        if bank.is_empty() {
            return;
        }
        self.preset_index = self.preset_index.min(bank.len() - 1);
        self.applied_preset = self.applied_preset.min(bank.len() - 1);

        let mut index = self.preset_index;
        ui.add(PresetBrowser::new(&self.preset_items, &mut index).width(210.0));

        if index != self.preset_index {
            self.preset_index = index;
            self.applied_preset = index;
            presets::apply(&bank[index], ctx);
        } else if let Some(found) = bank
            .iter()
            .position(|preset| presets::matches(preset, ctx))
        {
            // Follow a preset the host loaded (session restore, the host's
            // own browser): name the state instead of leaving a stale label.
            self.preset_index = found;
            self.applied_preset = found;
        }

        if !presets::matches(&bank[self.preset_index], ctx) {
            ui.add_space(2.0);
            ui.label(RichText::new("edited").size(9.0).color(palette.text_dim))
                .on_hover_text("Parameters differ from the selected preset");
        }
    }

    /// Meter and wet level: the right-hand strip that shows what the cluster
    /// above actually produced.
    fn master_strip(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &PluginContext<ThickVoxParams>,
        palette: &Palette,
        theme: &Theme,
    ) {
        ui.vertical(|ui| {
            ui.set_width(Self::OUTPUT_WIDTH);

            let levels = [
                ChannelLevel::from_linear(ctx.get_meter(ctx.meter_l.id())),
                ChannelLevel::from_linear(ctx.get_meter(ctx.meter_r.id())),
            ];
            let meter = LevelMeter::stereo(&levels)
                .scale(true)
                .readout(true)
                .clip(true)
                .id(egui::Id::new("thickvox-output-meter"));
            let meter_width = meter.desired_size(theme).x;

            let mut db = ctx.voice_level.raw_target() as f32;

            // Laid out bottom-up: the fader is pinned to the bottom of the
            // chassis and the meter takes every point left above it, so the
            // strip ends flush with the fader however tall the window is.
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                let fader = Fader::new(&mut db)
                    .range(-24.0, 6.0)
                    .taper(Taper::AUDIO)
                    .reset_db(Some(ctx.voice_level.info.default_plain as f32))
                    .scale(false)
                    .id(egui::Id::new("thickvox-voice-level"));
                let response = ui.add(fader);
                publish(ctx, &ctx.voice_level, db, &response);

                ui.add_space(theme.size.gap());
                ui.add(SectionHeader::new("VOICE LEVEL").accent(palette.primary));
                ui.add_space(4.0);
                correlation_meter(
                    ui,
                    ctx.get_meter(ctx.meter_corr.id()),
                    palette,
                    Self::OUTPUT_WIDTH,
                );
                ui.add_space(6.0);

                // Room for the output header, its gaps, and one more gap
                // under the chassis title.
                let band = ui.available_height() - (theme.size.gap() * 2.0 + 24.0);
                let meter_height = band.max(120.0);
                let (strip, _) = ui.allocate_exact_size(
                    egui::vec2(Self::OUTPUT_WIDTH, meter_height),
                    Sense::hover(),
                );
                let meter_rect = Rect::from_center_size(
                    strip.center(),
                    egui::vec2(meter_width, meter_height),
                );
                meter.paint(ui, theme, meter_rect);

                ui.add_space(4.0);
                ui.add(SectionHeader::new("OUTPUT").accent(palette.meter_ok));
                ui.add_space(theme.size.gap());
            });
        });
    }
}

/// A titled group: accent rule, then its controls on one row, centered under
/// the rule so a two-knob section doesn't leave its contents hanging off one
/// edge of a four-knob-wide column.
fn section(
    ui: &mut egui::Ui,
    title: &str,
    accent: Color32,
    knob_width: f32,
    knobs: usize,
    add_controls: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(6.0);
    ui.add(SectionHeader::new(title).accent(accent));
    ui.add_space(4.0);

    let spacing = ui.spacing().item_spacing.x * (knobs.saturating_sub(1)) as f32;
    let row = knob_width * knobs as f32 + spacing;
    // A single-control section is a two-ended row (one knob, one toggle), so
    // it hangs off the left edge instead of the dial floating mid-column.
    let pad = if knobs == 1 {
        0.0
    } else {
        ((ui.available_width() - row) * 0.5).max(0.0)
    };
    ui.horizontal(|ui| {
        ui.add_space(pad);
        add_controls(ui);
    });
}

/// Compact output-correlation bar: -1 (anti-phase) on the left, 0 in the
/// middle, +1 (mono) on the right.
///
/// The value is the raw meter lane, not a 0..1 level. A needle at the centre
/// is a very wide image, hard right is mono-compatible, and anything left of
/// centre will partially cancel when the mix is folded down.
fn correlation_meter(
    ui: &mut egui::Ui,
    correlation: f32,
    palette: &Palette,
    width: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 24.0), Sense::hover());
    let bar = Rect::from_min_size(rect.min, egui::vec2(rect.width(), 7.0));

    let color = if correlation < -0.1 {
        palette.destructive
    } else if correlation < 0.35 {
        palette.meter_warn
    } else {
        palette.meter_ok
    };

    let painter = ui.painter();
    painter.rect_filled(bar, 3.0, palette.track);

    let center = bar.center().x;
    let needle = bar.left() + (correlation.clamp(-1.0, 1.0) + 1.0) * 0.5 * bar.width();
    if (needle - center).abs() > 0.5 {
        let fill = Rect::from_min_max(
            egui::pos2(center.min(needle), bar.top() + 1.0),
            egui::pos2(center.max(needle), bar.bottom() - 1.0),
        );
        painter.rect_filled(fill, 1.0, color);
    }
    painter.line_segment(
        [
            egui::pos2(center, bar.top() - 1.0),
            egui::pos2(center, bar.bottom() + 1.0),
        ],
        egui::Stroke::new(1.0_f32, palette.text_dim),
    );

    let label_pos = egui::pos2(bar.left(), bar.bottom() + 2.0);
    painter.text(
        label_pos,
        egui::Align2::LEFT_TOP,
        "CORR",
        egui::FontId::proportional(8.5),
        palette.text_dim,
    );
    painter.text(
        egui::pos2(bar.right(), bar.bottom() + 2.0),
        egui::Align2::RIGHT_TOP,
        format!("{correlation:+.2}"),
        egui::FontId::monospace(9.0),
        color,
    );

    response.on_hover_text(
        "Output L/R correlation\n\
         +1 = mono · 0 = very wide · negative = phase cancellation when folded",
    )
}

/// The chassis power switch. It reads ON/BYP rather than a checkbox so the
/// state is legible at a glance from across a console.
fn power_switch(ui: &mut egui::Ui, ctx: &PluginContext<ThickVoxParams>, palette: &Palette) {
    let engaged = !ctx.bypass.value();
    let (label, color, hint) = if engaged {
        ("ON", palette.meter_ok, "Bypass ThickVox")
    } else {
        ("BYP", palette.destructive, "Engage ThickVox")
    };
    let button = egui::Button::new(
        RichText::new(label)
            .font(egui::FontId::proportional(10.0))
            .strong()
            .color(color),
    )
    .fill(with_alpha(color, 0.14))
    .stroke(egui::Stroke::new(1.0_f32, with_alpha(color, 0.55)))
    .min_size(egui::vec2(36.0, 20.0));

    if ui.add(button).on_hover_text(hint).clicked() {
        // A click is a complete gesture, so it goes through `automate`
        // rather than a bare parameter write.
        ctx.automate(ctx.bypass.id(), f64::from(u8::from(engaged)));
    }
}

// ---------------------------------------------------------------------------
// Editor helpers
// ---------------------------------------------------------------------------

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
            reading: Reading::Number {
                decimals: 0,
                unit: Some("%"),
            },
        },
    );
}

/// A knob in the parameter's own units (Hz, cents multiplier, ...).
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
            reading: Reading::Number {
                decimals,
                unit: Some(unit),
            },
        },
    );
}

/// A knob that reads in decibels (`-inf dB` included, rather than `-inf`).
fn db_knob(
    ui: &mut egui::Ui,
    ctx: &PluginContext<ThickVoxParams>,
    param: &FloatParam,
    label: &'static str,
    display: (f32, f32),
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
) {
    let mut shown = (param.raw_target() as f32) * spec.display_scale;
    let response = ui.add(
        Knob::new(&mut shown)
            .range(spec.display.0, spec.display.1)
            .label(spec.label)
            .reading(spec.reading)
            // Double-click returns to the parameter's declared default, which
            // is the value the host's "reset" shows too.
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
