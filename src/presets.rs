//! Factory and User preset table for the editor's preset picker.
//!
//! The `.preset` files under `presets/` are embedded at compile time for factory presets.
//! User-created presets are loaded from and saved to `~/.config/thickvox/presets/<Category>/<Name>.preset`.
//!
//! A preset stores *plain* values (the units on the knobs). Applying one
//! goes through the host bridge with `begin_edit` / `set_param` /
//! `end_edit`, and the range's own normalization supplies the taper, so a
//! preset writes exactly what turning the knob to that value would write.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use truce::core::PluginContext;
use truce::params::FloatParam;

use crate::ThickVoxParams;

/// One factory or user preset.
#[derive(Clone, Debug)]
pub(crate) struct Preset {
    pub(crate) name: String,
    pub(crate) category: String,
    #[allow(dead_code)]
    pub(crate) author: Option<String>,
    /// The library's "init" sound. Exactly one factory preset sets this.
    pub(crate) default: bool,
    /// Whether this is a user-saved preset loaded from disk.
    pub(crate) is_user: bool,
    /// Absolute path on disk (for user presets).
    #[allow(dead_code)]
    pub(crate) path: Option<PathBuf>,
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

/// Embedded factory presets, parsed once.
pub(crate) fn factory_presets() -> &'static [Preset] {
    static PRESETS: OnceLock<Vec<Preset>> = OnceLock::new();
    PRESETS.get_or_init(|| {
        PRESET_FILES
            .iter()
            .filter_map(|&(category, source)| parse(category.to_string(), source, false, None))
            .collect()
    })
}

/// Resolves `~/.config/thickvox/presets` directory.
pub(crate) fn user_presets_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        Some(
            PathBuf::from(home)
                .join(".config")
                .join("thickvox")
                .join("presets"),
        )
    } else if let Some(userprofile) = std::env::var_os("USERPROFILE") {
        Some(
            PathBuf::from(userprofile)
                .join(".config")
                .join("thickvox")
                .join("presets"),
        )
    } else {
        None
    }
}

/// Discovers both embedded factory presets and all user presets on disk.
pub(crate) fn all_presets() -> Vec<Preset> {
    let mut list = factory_presets().to_vec();
    if let Some(base) = user_presets_dir() {
        if base.exists() {
            scan_user_presets(&base, &mut list);
        }
    }
    list
}

/// Scans the given directory for `.preset` files and subdirectories.
pub(crate) fn scan_user_presets(base: &Path, list: &mut Vec<Preset>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };

    let mut user_presets = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("preset") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Some(p) = parse("User".to_string(), &content, true, Some(path)) {
                    user_presets.push(p);
                }
            }
        } else if path.is_dir() {
            let cat_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("User")
                .to_string();
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub in sub_entries.flatten() {
                    let sub_path = sub.path();
                    if sub_path.is_file()
                        && sub_path.extension().and_then(|s| s.to_str()) == Some("preset")
                    {
                        if let Ok(content) = std::fs::read_to_string(&sub_path) {
                            if let Some(p) = parse(cat_name.clone(), &content, true, Some(sub_path))
                            {
                                user_presets.push(p);
                            }
                        }
                    }
                }
            }
        }
    }

    user_presets.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    list.extend(user_presets);
}

/// Checks whether a user preset file with `name` exists under `category`.
pub(crate) fn user_preset_exists(name: &str, category: &str) -> bool {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return false;
    }
    let cat = if category.trim().is_empty() {
        "User"
    } else {
        category.trim()
    };
    if let Some(base) = user_presets_dir() {
        base.join(cat)
            .join(format!("{trimmed_name}.preset"))
            .exists()
    } else {
        false
    }
}

/// Saves the current plugin parameters as a user `.preset` file.
pub(crate) fn save_user_preset(
    name: &str,
    category: &str,
    author: &str,
    params: &ThickVoxParams,
) -> Result<PathBuf, String> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err("Preset name cannot be empty".to_string());
    }
    let cat = if category.trim().is_empty() {
        "User"
    } else {
        category.trim()
    };
    let author = if author.trim().is_empty() {
        "User"
    } else {
        author.trim()
    };

    let base =
        user_presets_dir().ok_or_else(|| "Could not determine user directory".to_string())?;
    let dir = base.join(cat);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create folder: {e}"))?;

    let file_path = dir.join(format!("{trimmed_name}.preset"));
    let content = serialize(trimmed_name, cat, author, params);
    std::fs::write(&file_path, content).map_err(|e| format!("Failed to write preset file: {e}"))?;

    Ok(file_path)
}

/// Serializes current parameter values into `.preset` TOML format.
pub(crate) fn serialize(
    name: &str,
    category: &str,
    author: &str,
    params: &ThickVoxParams,
) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0x8421_1248_dead_beef);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);

    let uuid = format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (seed >> 32) as u32,
        (seed >> 16) as u16,
        seed as u16 & 0x0fff,
        0x8000 | ((seed >> 48) as u16 & 0x3fff),
        seed & 0xffff_ffff_ffff,
    );

    let mut out = String::new();
    out.push_str(&format!("uuid = \"{uuid}\"\n"));
    out.push_str(&format!("name = \"{}\"\n", name.replace('"', "\\\"")));
    out.push_str(&format!("author = \"{}\"\n", author.replace('"', "\\\"")));
    out.push_str(&format!(
        "category = \"{}\"\n",
        category.replace('"', "\\\"")
    ));
    out.push_str("tags = [\"user\"]\ndefault = false\n\n[params]\n");
    out.push_str(&format!("0 = {:.3}   # Mix\n", params.mix.raw_target()));
    out.push_str(&format!(
        "1 = {:.2}   # Detune Depth\n",
        params.detune_depth.raw_target()
    ));
    out.push_str(&format!(
        "2 = {:.2}   # Delay Spread\n",
        params.delay_spread.raw_target()
    ));
    out.push_str(&format!(
        "4 = {:.2}   # Stereo Width\n",
        params.width.raw_target()
    ));
    out.push_str(&format!(
        "5 = {:.1}   # Voice Level\n",
        params.voice_level.raw_target()
    ));
    out.push_str(&format!(
        "6 = {:.0}   # Side HPF\n",
        params.side_hpf.raw_target()
    ));
    out.push_str(&format!("7 = {:.1}   # Air\n", params.air.raw_target()));
    out.push_str(&format!("8 = {}  # Mid Only\n", params.mid_only.value()));
    out.push_str(&format!(
        "10 = {:.2}  # Pair 1 Spread\n",
        params.pair1_spread.raw_target()
    ));
    out.push_str(&format!(
        "11 = {:.2}  # Pair 2 Spread\n",
        params.pair2_spread.raw_target()
    ));
    out.push_str(&format!(
        "12 = {:.2}  # Pair 3 Spread\n",
        params.pair3_spread.raw_target()
    ));
    out.push_str(&format!(
        "13 = {:.2}  # Pair 4 Spread\n",
        params.pair4_spread.raw_target()
    ));
    out
}

/// Reveals the presets directory in the platform's file manager.
pub(crate) fn reveal_presets_in_finder() {
    if let Some(base) = user_presets_dir() {
        let _ = std::fs::create_dir_all(&base);
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("open").arg(&base).spawn();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("explorer").arg(&base).spawn();
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let _ = std::process::Command::new("xdg-open").arg(&base).spawn();
        }
    }
}

/// Index of the preset marked `default = true`, or 0 when none is.
pub(crate) fn default_index(bank: &[Preset]) -> usize {
    bank.iter().position(|preset| preset.default).unwrap_or(0)
}

/// Write every value in `preset` to the host, one complete gesture per
/// parameter. Unknown ids are ignored so an old file cannot drive a
/// parameter that has since been retired.
pub(crate) fn apply(preset: &Preset, ctx: &PluginContext<ThickVoxParams>) {
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
pub(crate) fn matches(preset: &Preset, params: &ThickVoxParams) -> bool {
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
fn parse(
    default_category: String,
    source: &str,
    is_user: bool,
    path: Option<PathBuf>,
) -> Option<Preset> {
    let mut name = None;
    let mut author = None;
    let mut category = None;
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
                "category" => category = Some(unquote(value)),
                "author" => author = Some(unquote(value)),
                "default" => default = value == "true",
                _ => {}
            }
        }
    }

    Some(Preset {
        name: name.filter(|name| !name.is_empty())?,
        category: category.unwrap_or(default_category),
        author,
        default,
        is_user,
        path,
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
            "Test".to_string(),
            r#"
            uuid = "x"
            name = "Leading Space"   # trailing comment
            author = "A # not-a-comment"
            category = "Custom"
            tags = ["one", "two"]
            default = true

            [params]
            0 = 0.5  # Mix
            8 = false
            9 = true
            "#,
            false,
            None,
        )
        .unwrap();
        assert_eq!(preset.name, "Leading Space");
        assert_eq!(preset.author.as_deref(), Some("A # not-a-comment"));
        assert_eq!(preset.category, "Custom");
        assert!(preset.default);
        assert_eq!(preset.params, vec![(0, 0.5), (8, 0.0), (9, 1.0)]);
    }

    #[test]
    fn user_preset_scan_and_parse_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("thickvox_test_{}", std::process::id()));
        let leads_dir = temp_dir.join("Leads");
        std::fs::create_dir_all(&leads_dir).unwrap();

        let preset_content = r#"
        uuid = "test-1234"
        name = "Silky Lead"
        author = "Test Suite"
        category = "Leads"

        [params]
        0 = 0.40
        1 = 1.20
        2 = 0.95
        4 = 1.50
        5 = -6.0
        6 = 140.0
        7 = 2.5
        8 = false
        10 = 0.8
        11 = 0.8
        12 = 0.8
        13 = 0.8
        "#;
        std::fs::write(leads_dir.join("Silky Lead.preset"), preset_content).unwrap();

        let mut list = Vec::new();
        scan_user_presets(&temp_dir, &mut list);

        assert_eq!(list.len(), 1);
        let p = &list[0];
        assert_eq!(p.name, "Silky Lead");
        assert_eq!(p.category, "Leads");
        assert!(p.is_user);
        assert_eq!(p.params.len(), 12);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn user_preset_serialize_and_parse() {
        let params = ThickVoxParams::default();

        let serialized = serialize("My Vocal Sound", "Lead", "Producer X", &params);
        assert!(serialized.contains("name = \"My Vocal Sound\""));
        assert!(serialized.contains("category = \"Lead\""));
        assert!(serialized.contains("author = \"Producer X\""));
        assert!(serialized.contains("[params]"));

        let parsed = parse("Lead".to_string(), &serialized, true, None).unwrap();
        assert_eq!(parsed.name, "My Vocal Sound");
        assert_eq!(parsed.category, "Lead");
        assert_eq!(parsed.author.as_deref(), Some("Producer X"));
        assert!(parsed.is_user);
        assert_eq!(parsed.params.len(), 12);
        assert!(matches(&parsed, &params));
    }
}
