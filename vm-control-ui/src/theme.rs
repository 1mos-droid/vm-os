//! Data-only theme loader and strict validator for `vm-control-ui` (C4).
//!
//! Themes are pure JSON token documents (`colors`, `fonts`, `radii`, `spacing`,
//! `layout_preset`) validated with `#[serde(deny_unknown_fields)]`, bounded
//! ranges, and a 64 KiB file cap. Invalid or missing themes fall back cleanly
//! to the built-in Obsidian theme with a warning. VM security origin accent
//! colors remain isolated in `studio::vm_origin_accent`.

use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Schema version required for all theme JSON files.
pub const THEME_SCHEMA_VERSION: u32 = 1;

/// Maximum theme file size (64 KiB) to prevent resource exhaustion.
pub const MAX_THEME_FILE_BYTES: u64 = 64 * 1024;

/// Built-in default dark theme JSON shipped with `vm-control-ui`.
pub const BUILTIN_OBSIDIAN_JSON: &str = include_str!("../themes/obsidian.json");

/// Built-in daylight theme JSON shipped with `vm-control-ui`.
pub const BUILTIN_DAYLIGHT_JSON: &str = include_str!("../themes/daylight.json");

/// Layout density preset controlling coarse spacing/chrome density.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutPreset {
    Compact,
    Comfortable,
    Spacious,
}

impl LayoutPreset {
    /// Canonical lowercase identifier exposed to Slint (`Theme.layout-preset`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
            Self::Spacious => "spacious",
        }
    }

    /// Numeric density level (`0 = compact`, `1 = comfortable`, `2 = spacious`).
    pub fn density_level(self) -> i32 {
        match self {
            Self::Compact => 0,
            Self::Comfortable => 1,
            Self::Spacious => 2,
        }
    }
}

/// Core UI surface and text palette (excludes VM security origin colors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeColors {
    pub window_bg: String,
    pub canvas_bg: String,
    pub surface_bg: String,
    pub surface_elevated: String,
    pub border: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub text_muted: String,
    pub accent: String,
    pub danger: String,
    pub success: String,
}

/// Typography tokens validated against safe font family strings and pixel bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeFonts {
    pub ui_family: String,
    pub mono_family: String,
    pub base_size_px: u16,
    pub heading_size_px: u16,
}

/// Corner radius tokens in pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeRadii {
    pub sm_px: u16,
    pub md_px: u16,
    pub lg_px: u16,
    pub pill_px: u16,
}

/// Spacing scale tokens in pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeSpacing {
    pub xs_px: u16,
    pub sm_px: u16,
    pub md_px: u16,
    pub lg_px: u16,
}

/// Top-level validated JSON theme specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeFile {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub layout_preset: LayoutPreset,
    pub colors: ThemeColors,
    pub fonts: ThemeFonts,
    pub radii: ThemeRadii,
    pub spacing: ThemeSpacing,
}

/// Startup result from resolving an optional external theme path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeLoadOutcome {
    pub theme: ThemeFile,
    pub source_label: String,
    pub warning: Option<String>,
}

/// Parse an exact `#RRGGBB` 6-digit hexadecimal RGB string.
pub fn parse_hex_rgb(hex: &str) -> Result<(u8, u8, u8), String> {
    let bytes = hex.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' || !bytes[1..].iter().all(u8::is_ascii_hexdigit) {
        return Err(format!("invalid #RRGGBB color literal: '{hex}'"));
    }
    let r = u8::from_str_radix(&hex[1..3], 16).map_err(|e| format!("bad red hex: {e}"))?;
    let g = u8::from_str_radix(&hex[3..5], 16).map_err(|e| format!("bad green hex: {e}"))?;
    let b = u8::from_str_radix(&hex[5..7], 16).map_err(|e| format!("bad blue hex: {e}"))?;
    Ok((r, g, b))
}

fn validate_theme_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 48 {
        return Err("theme id must be 1..=48 characters".to_string());
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(format!(
            "theme id '{id}' must contain only lowercase ASCII letters, digits, '-' or '_'"
        ));
    }
    Ok(())
}

fn validate_display_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 || name.trim() != name {
        return Err(
            "theme name must be non-empty <= 64 chars without leading/trailing whitespace"
                .to_string(),
        );
    }
    if name.chars().any(char::is_control) {
        return Err("theme name must not contain control characters".to_string());
    }
    Ok(())
}

fn validate_font_family(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 64 || value.trim() != value {
        return Err(format!(
            "{field} must be non-empty <= 64 chars without leading/trailing whitespace"
        ));
    }
    if value.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '/' | '\\' | ';' | '{' | '}' | ':' | '\'' | '"' | '<' | '>'
            )
    }) {
        return Err(format!("{field} contains disallowed characters: '{value}'"));
    }
    Ok(())
}

impl ThemeFile {
    /// Validate all fields, color formats, and numeric ranges on a parsed theme.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != THEME_SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {} (expected {THEME_SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        validate_theme_id(&self.id)?;
        validate_display_name(&self.name)?;

        for (label, value) in [
            ("colors.window_bg", &self.colors.window_bg),
            ("colors.canvas_bg", &self.colors.canvas_bg),
            ("colors.surface_bg", &self.colors.surface_bg),
            ("colors.surface_elevated", &self.colors.surface_elevated),
            ("colors.border", &self.colors.border),
            ("colors.text_primary", &self.colors.text_primary),
            ("colors.text_secondary", &self.colors.text_secondary),
            ("colors.text_muted", &self.colors.text_muted),
            ("colors.accent", &self.colors.accent),
            ("colors.danger", &self.colors.danger),
            ("colors.success", &self.colors.success),
        ] {
            parse_hex_rgb(value).map_err(|e| format!("{label}: {e}"))?;
        }

        validate_font_family("fonts.ui_family", &self.fonts.ui_family)?;
        validate_font_family("fonts.mono_family", &self.fonts.mono_family)?;

        if !(10..=20).contains(&self.fonts.base_size_px) {
            return Err(format!(
                "fonts.base_size_px {} outside allowed range 10..=20",
                self.fonts.base_size_px
            ));
        }
        if !(14..=28).contains(&self.fonts.heading_size_px) {
            return Err(format!(
                "fonts.heading_size_px {} outside allowed range 14..=28",
                self.fonts.heading_size_px
            ));
        }
        if self.fonts.heading_size_px < self.fonts.base_size_px {
            return Err("fonts.heading_size_px must be >= fonts.base_size_px".to_string());
        }

        if self.radii.sm_px > 24 {
            return Err("radii.sm_px exceeds 24".to_string());
        }
        if self.radii.md_px > 32 || self.radii.md_px < self.radii.sm_px {
            return Err("radii.md_px must satisfy sm_px <= md_px <= 32".to_string());
        }
        if self.radii.lg_px > 40 || self.radii.lg_px < self.radii.md_px {
            return Err("radii.lg_px must satisfy md_px <= lg_px <= 40".to_string());
        }
        if self.radii.pill_px > 40 {
            return Err("radii.pill_px exceeds 40".to_string());
        }

        if !(2..=16).contains(&self.spacing.xs_px) {
            return Err("spacing.xs_px must be in 2..=16".to_string());
        }
        if !(4..=24).contains(&self.spacing.sm_px) || self.spacing.sm_px < self.spacing.xs_px {
            return Err("spacing.sm_px must satisfy xs_px <= sm_px <= 24".to_string());
        }
        if !(6..=32).contains(&self.spacing.md_px) || self.spacing.md_px < self.spacing.sm_px {
            return Err("spacing.md_px must satisfy sm_px <= md_px <= 32".to_string());
        }
        if !(8..=48).contains(&self.spacing.lg_px) || self.spacing.lg_px < self.spacing.md_px {
            return Err("spacing.lg_px must satisfy md_px <= lg_px <= 48".to_string());
        }

        Ok(())
    }

    /// Return `true` when `colors.surface_bg` has perceived luminance >= 50%.
    pub fn is_light_surface(&self) -> bool {
        if let Ok((r, g, b)) = parse_hex_rgb(&self.colors.surface_bg) {
            let luma = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000;
            return luma >= 128;
        }
        false
    }
}

/// Return the built-in default Obsidian theme.
pub fn builtin_default_theme() -> ThemeFile {
    parse_theme_str(BUILTIN_OBSIDIAN_JSON).expect("built-in obsidian theme JSON must validate")
}

/// Strictly parse and validate a theme JSON string.
pub fn parse_theme_str(raw: &str) -> Result<ThemeFile, String> {
    if raw.len() as u64 > MAX_THEME_FILE_BYTES {
        return Err(format!(
            "theme payload exceeds maximum allowed size of {MAX_THEME_FILE_BYTES} bytes"
        ));
    }
    let theme: ThemeFile =
        serde_json::from_str(raw).map_err(|e| format!("JSON schema error: {e}"))?;
    theme.validate()?;
    Ok(theme)
}

/// Read a bounded JSON theme file from disk and strictly validate it.
pub fn load_theme_from_file(path: &Path) -> Result<ThemeFile, String> {
    let meta = fs::metadata(path).map_err(|e| format!("cannot stat '{}': {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("'{}' is not a regular file", path.display()));
    }
    if meta.len() > MAX_THEME_FILE_BYTES {
        return Err(format!(
            "'{}' exceeds {} byte theme limit ({} bytes)",
            path.display(),
            MAX_THEME_FILE_BYTES,
            meta.len()
        ));
    }
    let file = File::open(path).map_err(|e| format!("cannot open '{}': {e}", path.display()))?;
    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_THEME_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
    if buf.len() as u64 > MAX_THEME_FILE_BYTES {
        return Err(format!(
            "'{}' exceeds {} byte theme limit",
            path.display(),
            MAX_THEME_FILE_BYTES
        ));
    }
    let raw = String::from_utf8(buf)
        .map_err(|e| format!("'{}' is not valid UTF-8: {e}", path.display()))?;
    parse_theme_str(&raw)
}

/// Resolve CLI `--theme` precedence over non-empty `HYPEROS_THEME` env value.
pub fn resolve_theme_path(cli_theme: Option<&Path>, env_theme: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = cli_theme {
        return Some(p.to_path_buf());
    }
    env_theme
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Load a theme path if specified, or fall back to built-in Obsidian with a clear warning.
pub fn load_theme_or_fallback(path: Option<&Path>) -> ThemeLoadOutcome {
    let Some(p) = path else {
        return ThemeLoadOutcome {
            theme: builtin_default_theme(),
            source_label: "built-in (obsidian)".to_string(),
            warning: None,
        };
    };

    match load_theme_from_file(p) {
        Ok(theme) => ThemeLoadOutcome {
            theme,
            source_label: p.display().to_string(),
            warning: None,
        },
        Err(err) => ThemeLoadOutcome {
            theme: builtin_default_theme(),
            source_label: "built-in fallback (obsidian)".to_string(),
            warning: Some(format!(
                "Theme '{}' invalid ({err}); using built-in Obsidian fallback.",
                p.display()
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_tmp_file(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hyperos-studio-theme-{}-{nanos}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn shipped_obsidian_and_daylight_themes_parse_and_validate() {
        let obsidian = parse_theme_str(BUILTIN_OBSIDIAN_JSON).unwrap();
        assert_eq!(obsidian.id, "obsidian");
        assert_eq!(obsidian.layout_preset, LayoutPreset::Compact);
        assert_eq!(obsidian.layout_preset.density_level(), 0);
        assert!(!obsidian.is_light_surface());
        assert_eq!(
            parse_hex_rgb(&obsidian.colors.window_bg).unwrap(),
            (9, 13, 22)
        );

        let daylight = parse_theme_str(BUILTIN_DAYLIGHT_JSON).unwrap();
        assert_eq!(daylight.id, "daylight");
        assert_eq!(daylight.layout_preset, LayoutPreset::Comfortable);
        assert_eq!(daylight.layout_preset.density_level(), 1);
        assert!(daylight.is_light_surface());
        assert_ne!(daylight.colors.surface_bg, obsidian.colors.surface_bg);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_schema_version() {
        let with_unknown = BUILTIN_OBSIDIAN_JSON.replacen(
            "\"schema_version\": 1,",
            "\"schema_version\": 1,\n  \"exec_hook\": \"/bin/sh\",",
            1,
        );
        let err = parse_theme_str(&with_unknown).unwrap_err();
        assert!(err.contains("unknown field"), "err={err}");

        let wrong_ver =
            BUILTIN_OBSIDIAN_JSON.replacen("\"schema_version\": 1,", "\"schema_version\": 2,", 1);
        assert!(parse_theme_str(&wrong_ver)
            .unwrap_err()
            .contains("unsupported schema_version"));
    }

    #[test]
    fn rejects_invalid_hex_colors_and_out_of_range_metrics() {
        let short_hex = BUILTIN_OBSIDIAN_JSON.replacen("#090d16", "#090", 1);
        assert!(parse_theme_str(&short_hex)
            .unwrap_err()
            .contains("colors.window_bg"));

        let func_hex = BUILTIN_OBSIDIAN_JSON.replacen("#06b6d4", "rgb(6,182,212)", 1);
        assert!(parse_theme_str(&func_hex)
            .unwrap_err()
            .contains("colors.accent"));

        let huge_font =
            BUILTIN_OBSIDIAN_JSON.replacen("\"base_size_px\": 12,", "\"base_size_px\": 72,", 1);
        assert!(parse_theme_str(&huge_font)
            .unwrap_err()
            .contains("outside allowed range"));

        let inverted_radii = BUILTIN_OBSIDIAN_JSON.replacen("\"lg_px\": 16,", "\"lg_px\": 4,", 1);
        assert!(parse_theme_str(&inverted_radii)
            .unwrap_err()
            .contains("radii.lg_px"));
    }

    #[test]
    fn rejects_float_numbers_and_unsafe_font_family_tokens() {
        let float_px =
            BUILTIN_OBSIDIAN_JSON.replacen("\"base_size_px\": 12,", "\"base_size_px\": 12.5,", 1);
        assert!(parse_theme_str(&float_px).is_err());

        let bad_font = BUILTIN_OBSIDIAN_JSON.replacen("\"DejaVu Sans\"", "\"/tmp/evil;sh\"", 1);
        assert!(parse_theme_str(&bad_font)
            .unwrap_err()
            .contains("disallowed characters"));
    }

    #[test]
    fn load_theme_or_fallback_handles_missing_corrupt_and_oversized_files() {
        let missing = unique_tmp_file("missing.json");
        let res_missing = load_theme_or_fallback(Some(&missing));
        assert_eq!(res_missing.theme.id, "obsidian");
        assert!(res_missing.warning.as_deref().unwrap().contains("invalid"));

        let corrupt = unique_tmp_file("corrupt.json");
        fs::write(&corrupt, "{ not valid json").unwrap();
        let res_corrupt = load_theme_or_fallback(Some(&corrupt));
        assert_eq!(res_corrupt.theme.id, "obsidian");
        assert!(res_corrupt
            .warning
            .as_deref()
            .unwrap()
            .contains("JSON schema error"));
        let _ = fs::remove_file(&corrupt);

        let oversized = unique_tmp_file("oversized.json");
        fs::write(
            &oversized,
            vec![b' '; (MAX_THEME_FILE_BYTES + 128) as usize],
        )
        .unwrap();
        let res_over = load_theme_or_fallback(Some(&oversized));
        assert_eq!(res_over.theme.id, "obsidian");
        assert!(res_over.warning.as_deref().unwrap().contains("exceeds"));
        let _ = fs::remove_file(&oversized);
    }

    #[test]
    fn resolve_theme_path_prefers_cli_over_env_and_ignores_blank_env() {
        let cli = PathBuf::from("/etc/hyperos/custom.json");
        assert_eq!(
            resolve_theme_path(Some(&cli), Some("/tmp/other.json")).as_deref(),
            Some(cli.as_path())
        );
        assert_eq!(
            resolve_theme_path(None, Some("  /etc/hyperos/daylight.json  ")).as_deref(),
            Some(Path::new("/etc/hyperos/daylight.json"))
        );
        assert_eq!(resolve_theme_path(None, Some("   ")), None);
    }
}
