//! Factory-preset table for the editor's preset picker.
//!
//! The `.preset` files under `presets/` are the single source of truth:
//! they are embedded at compile time, so the picker can never drift from
//! what `cargo truce build` ships in the bundle. They are parsed once, on
//! the first editor open, and never touched from the audio thread.
//!
//! A preset stores *plain* values (the units on the knobs). Applying one
//! goes through the host bridge with `begin_edit` / `set_param` /
//! `end_edit`, and the range's own normalization supplies the taper, so a
//! preset writes exactly what turning the knob to that value would write.

use std::sync::OnceLock;

use truce::core::PluginContext;
use truce::params::FloatParam;

use crate::ThickVoxParams;

/// One factory preset, parsed from its author-facing `.preset` file.
pub(crate) struct FactoryPreset {
    pub(crate) name: String,
    pub(crate) category: &'static str,
    #[allow(dead_code)]
    pub(crate) author: Option<String>,
    /// The library's "init" sound. Exactly one factory preset sets this.
    pub(crate) default: bool,
    /// `(parameter id, plain value)`, in file order.
    pub(crate) params: Vec<(u32, f64)>,
}

/// The embedded preset files, in picker order. The category is the parent
/// directory name, matching how the preset library groups them on disk.
const PRESET_FILES: &[(&str, &str)] = &[
    (
        "Width",
        include_str!("../presets/Width/Classic Wide.preset"),
    ),
    (
        "Width",
        include_str!("../presets/Width/Medium Width.preset"),
    ),
    ("Width", include_str!("../presets/Width/Clustered.preset")),
    ("Width", include_str!("../presets/Width/Asymmetric.preset")),
    (
        "Thickness",
        include_str!("../presets/Thickness/Centred Thickness.preset"),
    ),
];

/// Every factory preset, parsed on first use.
pub(crate) fn factory_presets() -> &'static [FactoryPreset] {
    static PRESETS: OnceLock<Vec<FactoryPreset>> = OnceLock::new();
    PRESETS.get_or_init(|| {
        PRESET_FILES
            .iter()
            .filter_map(|&(category, source)| parse(category, source))
            .collect()
    })
}

/// Index of the preset marked `default = true`, or 0 when none is.
pub(crate) fn default_index() -> usize {
    factory_presets()
        .iter()
        .position(|preset| preset.default)
        .unwrap_or(0)
}

/// Write every value in `preset` to the host, one complete gesture per
/// parameter. Unknown ids are ignored so an old file cannot drive a
/// parameter that has since been retired.
pub(crate) fn apply(preset: &FactoryPreset, ctx: &PluginContext<ThickVoxParams>) {
    for &(id, plain) in &preset.params {
        match id {
            0 => automate(ctx, &ctx.mix, plain),
            1 => automate(ctx, &ctx.detune_depth, plain),
            2 => automate(ctx, &ctx.delay_spread, plain),
            4 => automate(ctx, &ctx.width, plain),
            5 => automate(ctx, &ctx.voice_level, plain),
            6 => automate(ctx, &ctx.side_hpf, plain),
            7 => automate(ctx, &ctx.air, plain),
            8 => ctx.automate(ctx.mid_only.id(), f64::from(u8::from(plain >= 0.5))),
            10 => automate(ctx, &ctx.pair1_spread, plain),
            11 => automate(ctx, &ctx.pair2_spread, plain),
            12 => automate(ctx, &ctx.pair3_spread, plain),
            13 => automate(ctx, &ctx.pair4_spread, plain),
            _ => {}
        }
    }
}

fn automate(ctx: &PluginContext<ThickVoxParams>, param: &FloatParam, plain: f64) {
    // `param.id()` (not the id from the file) so a renamed key cannot
    // route a value to the wrong parameter.
    ctx.automate(param.id(), param.info.range.normalize(plain));
}

/// Whether the live parameter values equal `preset`'s values.
///
/// Used to follow host-side changes: after a session restore or a preset
/// loaded by the host, the picker should name the preset the state landed
/// on. Normalized round-trips are not bit-exact, so compare with a small
/// relative tolerance rather than `==`.
pub(crate) fn matches(preset: &FactoryPreset, params: &ThickVoxParams) -> bool {
    preset.params.iter().all(|&(id, plain)| match id {
        0 => close(params.mix.raw_target(), plain),
        1 => close(params.detune_depth.raw_target(), plain),
        2 => close(params.delay_spread.raw_target(), plain),
        4 => close(params.width.raw_target(), plain),
        5 => close(params.voice_level.raw_target(), plain),
        6 => close(params.side_hpf.raw_target(), plain),
        7 => close(params.air.raw_target(), plain),
        8 => params.mid_only.value() == (plain >= 0.5),
        10 => close(params.pair1_spread.raw_target(), plain),
        11 => close(params.pair2_spread.raw_target(), plain),
        12 => close(params.pair3_spread.raw_target(), plain),
        13 => close(params.pair4_spread.raw_target(), plain),
        _ => true,
    })
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= 1e-4_f64.max(expected.abs() * 1e-4)
}

/// Parse one author-facing `.preset` file.
///
/// The format is a small TOML subset: top-level string / bool keys, then a
/// `[params]` table of `id = number|bool` entries. Arrays (`tags`) and
/// unknown keys are ignored, which is what keeps this parser from growing
/// into a TOML dependency.
fn parse(category: &'static str, source: &str) -> Option<FactoryPreset> {
    let mut name = None;
    let mut author = None;
    let mut default = false;
    let mut params = Vec::new();
    let mut in_params = false;

    for raw in source.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_params = line == "[params]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if in_params {
            let Ok(id) = key.parse::<u32>() else {
                continue;
            };
            match value {
                "true" => params.push((id, 1.0)),
                "false" => params.push((id, 0.0)),
                _ => {
                    if let Ok(number) = value.parse::<f64>() {
                        params.push((id, number));
                    }
                }
            }
        } else {
            match key {
                "name" => name = Some(unquote(value)),
                "author" => author = Some(unquote(value)),
                "default" => default = value == "true",
                _ => {}
            }
        }
    }

    Some(FactoryPreset {
        name: name.filter(|name| !name.is_empty())?,
        category,
        author,
        default,
        params,
    })
}

/// Drop a trailing `# …` comment, leaving `#` inside quoted strings alone.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..index],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id a preset file may set. Ids 3 and 9 are deliberately absent
    /// from the factory bank: 3 is retired, 9 is the bypass switch, and a
    /// preset must not silently take the plugin out of bypass.
    const PRESET_IDS: [u32; 12] = [0, 1, 2, 4, 5, 6, 7, 8, 10, 11, 12, 13];

    #[test]
    fn every_factory_file_parses() {
        assert_eq!(factory_presets().len(), PRESET_FILES.len());
    }

    #[test]
    fn every_preset_sets_every_parameter() {
        for preset in factory_presets() {
            assert!(!preset.name.is_empty());
            assert!(!preset.params.is_empty(), "{} has no params", preset.name);
            for &(id, _) in &preset.params {
                assert!(
                    PRESET_IDS.contains(&id),
                    "{} sets unexpected parameter id {id}",
                    preset.name
                );
            }
            for id in PRESET_IDS {
                assert!(
                    preset.params.iter().any(|&(set, _)| set == id),
                    "{} does not set parameter {id}",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn exactly_one_default_preset() {
        assert_eq!(
            factory_presets()
                .iter()
                .filter(|preset| preset.default)
                .count(),
            1
        );
    }

    #[test]
    fn parameters_stay_inside_their_declared_ranges() {
        // Catch a typo in a `.preset` file at test time rather than as a
        // silent clamp when the user picks it.
        let params = ThickVoxParams::default();
        for preset in factory_presets() {
            for &(id, value) in &preset.params {
                let param: &FloatParam = match id {
                    0 => &params.mix,
                    1 => &params.detune_depth,
                    2 => &params.delay_spread,
                    4 => &params.width,
                    5 => &params.voice_level,
                    6 => &params.side_hpf,
                    7 => &params.air,
                    10 => &params.pair1_spread,
                    11 => &params.pair2_spread,
                    12 => &params.pair3_spread,
                    13 => &params.pair4_spread,
                    _ => continue,
                };
                let (lo, hi) = (param.info.range.min(), param.info.range.max());
                assert!(
                    value >= lo && value <= hi,
                    "{} sets {id} to {value}, outside [{lo}, {hi}]",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn parser_ignores_comments_and_unknown_keys() {
        let preset = parse(
            "Test",
            r#"
            uuid = "x"
            name = "Leading Space"   # trailing comment
            author = "A # not-a-comment"
            tags = ["one", "two"]
            default = true

            [params]
            0 = 0.5  # Mix
            8 = false
            9 = true
            "#,
        )
        .unwrap();
        assert_eq!(preset.name, "Leading Space");
        assert_eq!(preset.author.as_deref(), Some("A # not-a-comment"));
        assert!(preset.default);
        assert_eq!(preset.params, vec![(0, 0.5), (8, 0.0), (9, 1.0)]);
    }
}
