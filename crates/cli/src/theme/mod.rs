//! Theme engine for Sotto CLI: semantic colour palettes, presets, and custom themes.
//!
//! Provides colour tokens and formatting helpers for terminal rendering, honouring `NO_COLOR`
//! and non-interactive environments.
//!
//! Supports 5 built-in presets:
//! - `nord` (default): Cool slate/Nord palette
//! - `sordino`: Sotto brand warm paper-and-ink palette
//! - `terminal`: Adaptive 16 ANSI colours
//! - `monochrome`: Greyscale high-contrast
//! - `tokyo-night`: Vibrant dark indigo palette
//!
//! Custom themes can be defined as TOML files in the `themes/` directory next to the
//! global config (see [`crate::paths::themes_path`]): each `<name>.toml` file adds a theme
//! selectable by its `name` field, or by the filename when the field is absent.
//!
//! Styling is strictly gated: `--plain`, `NO_COLOR`, a non-TTY standard output or standard
//! input, and CI environments all disable ANSI output (see [`styling_active`]).

use std::fmt;
use std::io::IsTerminal;
use std::path::Path;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::error::Result;
use crate::remote::config::GlobalConfig;

/// Environment variable for overriding the theme.
pub const THEME_ENV: &str = "SOTTO_THEME";

/// A colour represented as 24-bit TrueColor RGB, an ANSI palette index (0-255), or the terminal default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Rgb(u8, u8, u8),
    Ansi(u8),
    Default,
}

impl Color {
    /// Return the ANSI escape sequence for setting the foreground colour.
    pub fn fg_ansi(&self) -> String {
        match self {
            Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m"),
            Color::Ansi(c) if *c < 8 => format!("\x1b[{}m", 30 + c),
            Color::Ansi(c) if *c < 16 => format!("\x1b[{}m", 90 + (c - 8)),
            Color::Ansi(c) => format!("\x1b[38;5;{c}m"),
            Color::Default => "\x1b[39m".to_string(),
        }
    }

    /// Return the ANSI escape sequence for setting the background colour.
    pub fn bg_ansi(&self) -> String {
        match self {
            Color::Rgb(r, g, b) => format!("\x1b[48;2;{r};{g};{b}m"),
            Color::Ansi(c) if *c < 8 => format!("\x1b[{}m", 40 + c),
            Color::Ansi(c) if *c < 16 => format!("\x1b[{}m", 100 + (c - 8)),
            Color::Ansi(c) => format!("\x1b[48;5;{c}m"),
            Color::Default => "\x1b[49m".to_string(),
        }
    }

    /// Parse a colour from a hex string (e.g. `"#88C0D0"` or `"88C0D0"`), a named ANSI
    /// colour (e.g. `"red"`, `"light_cyan"`), or an explicit 256-colour palette index
    /// (`"ansi(200)"`, the form [`Color`]'s serializer emits for indices above 15).
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("default") || trimmed.eq_ignore_ascii_case("none") {
            return Some(Color::Default);
        }

        // Try an explicit palette index: `ansi(0)` .. `ansi(255)` (case-insensitive).
        let lower = trimmed.to_ascii_lowercase();
        if let Some(inner) = lower
            .strip_prefix("ansi(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            return inner.trim().parse::<u8>().ok().map(Color::Ansi);
        }

        // Try named ANSI colours
        match lower.as_str() {
            "black" => return Some(Color::Ansi(0)),
            "red" => return Some(Color::Ansi(1)),
            "green" => return Some(Color::Ansi(2)),
            "yellow" => return Some(Color::Ansi(3)),
            "blue" => return Some(Color::Ansi(4)),
            "magenta" => return Some(Color::Ansi(5)),
            "cyan" => return Some(Color::Ansi(6)),
            "white" => return Some(Color::Ansi(7)),
            "gray" | "dark_gray" | "bright_black" => return Some(Color::Ansi(8)),
            "light_red" | "bright_red" => return Some(Color::Ansi(9)),
            "light_green" | "bright_green" => return Some(Color::Ansi(10)),
            "light_yellow" | "bright_yellow" => return Some(Color::Ansi(11)),
            "light_blue" | "bright_blue" => return Some(Color::Ansi(12)),
            "light_magenta" | "bright_magenta" => return Some(Color::Ansi(13)),
            "light_cyan" | "bright_cyan" => return Some(Color::Ansi(14)),
            "bright_white" => return Some(Color::Ansi(15)),
            _ => {}
        }

        // Try hex RGB
        let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        } else if hex.len() == 3 {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            Some(Color::Rgb(r * 17, g * 17, b * 17))
        } else {
            None
        }
    }
}

impl Serialize for Color {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Color::Rgb(r, g, b) => serializer.serialize_str(&format!("#{r:02x}{g:02x}{b:02x}")),
            Color::Ansi(c) => serializer.serialize_str(&match c {
                0 => "black".into(),
                1 => "red".into(),
                2 => "green".into(),
                3 => "yellow".into(),
                4 => "blue".into(),
                5 => "magenta".into(),
                6 => "cyan".into(),
                7 => "white".into(),
                8 => "gray".into(),
                9 => "light_red".into(),
                10 => "light_green".into(),
                11 => "light_yellow".into(),
                12 => "light_blue".into(),
                13 => "light_magenta".into(),
                14 => "light_cyan".into(),
                15 => "bright_white".into(),
                other => format!("ansi({other})"),
            }),
            Color::Default => serializer.serialize_str("default"),
        }
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Color::parse(&s)
            .ok_or_else(|| de::Error::custom(format!("invalid colour specification: {s}")))
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Color::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Color::Ansi(c) => write!(f, "ansi({c})"),
            Color::Default => write!(f, "default"),
        }
    }
}

/// A cohesive terminal theme comprising semantic colours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    /// Theme name; optional in custom TOML files, where the filename is used when absent.
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<Color>,
    pub fg: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub muted: Color,
    pub border: Color,
    /// Runtime styling switch (never persisted): `false` makes every painter a passthrough.
    /// A freshly parsed theme starts active; [`resolve_theme`] turns it off when styling is
    /// gated (see [`styling_active`]).
    #[serde(skip, default = "default_active")]
    pub active: bool,
}

/// `active` is a runtime gate, not a persisted property, so TOML parsing defaults it on.
fn default_active() -> bool {
    true
}

impl Default for Theme {
    fn default() -> Self {
        Self::nord()
    }
}

impl Theme {
    /// The default theme: Cool Slate / Nord.
    pub fn nord() -> Self {
        Self {
            name: "nord".into(),
            bg: Some(Color::Rgb(0x24, 0x29, 0x33)),
            fg: Color::Rgb(0xEC, 0xEF, 0xF4),
            accent: Color::Rgb(0x88, 0xC0, 0xD0),
            success: Color::Rgb(0xA3, 0xBE, 0x8C),
            warning: Color::Rgb(0xEB, 0xCB, 0x8B),
            error: Color::Rgb(0xBF, 0x61, 0x6A),
            muted: Color::Rgb(0x4C, 0x56, 0x6A),
            border: Color::Rgb(0x3B, 0x42, 0x52),
            active: true,
        }
    }

    /// Sotto brand warm paper-and-ink theme (Sordino).
    pub fn sordino() -> Self {
        Self {
            name: "sordino".into(),
            bg: Some(Color::Rgb(0x16, 0x15, 0x0f)),
            fg: Color::Rgb(0xd9, 0xd4, 0xc6),
            accent: Color::Rgb(0xc7, 0xb4, 0x7e),
            success: Color::Rgb(0x86, 0xb5, 0x7e),
            warning: Color::Rgb(0xc7, 0xb4, 0x7e),
            error: Color::Rgb(0xe0, 0x70, 0x5f),
            muted: Color::Rgb(0x85, 0x7f, 0x6d),
            border: Color::Rgb(0x20, 0x1e, 0x17),
            active: true,
        }
    }

    /// Adaptive 16-colour ANSI theme following the host terminal emulator palette.
    pub fn terminal() -> Self {
        Self {
            name: "terminal".into(),
            bg: None,
            fg: Color::Default,
            accent: Color::Ansi(6),  // Cyan
            success: Color::Ansi(2), // Green
            warning: Color::Ansi(3), // Yellow
            error: Color::Ansi(1),   // Red
            muted: Color::Ansi(8),   // Dark grey / dim
            border: Color::Ansi(8),  // Dark grey
            active: true,
        }
    }

    /// High-contrast greyscale monochrome theme.
    pub fn monochrome() -> Self {
        Self {
            name: "monochrome".into(),
            bg: Some(Color::Rgb(0x00, 0x00, 0x00)),
            fg: Color::Rgb(0xFF, 0xFF, 0xFF),
            accent: Color::Rgb(0xFF, 0xFF, 0xFF),
            success: Color::Rgb(0xFF, 0xFF, 0xFF),
            warning: Color::Rgb(0xAA, 0xAA, 0xAA),
            error: Color::Rgb(0xFF, 0xFF, 0xFF),
            muted: Color::Rgb(0x66, 0x66, 0x66),
            border: Color::Rgb(0x44, 0x44, 0x44),
            active: true,
        }
    }

    /// Vibrant dark night palette.
    pub fn tokyo_night() -> Self {
        Self {
            name: "tokyo-night".into(),
            bg: Some(Color::Rgb(0x1a, 0x1b, 0x26)),
            fg: Color::Rgb(0xc0, 0xca, 0xf5),
            accent: Color::Rgb(0x7d, 0xcf, 0xff),
            success: Color::Rgb(0x9e, 0xce, 0x6a),
            warning: Color::Rgb(0xe0, 0xaf, 0x68),
            error: Color::Rgb(0xf7, 0x76, 0x8e),
            muted: Color::Rgb(0x56, 0x5f, 0x89),
            border: Color::Rgb(0x29, 0x2e, 0x42),
            active: true,
        }
    }

    /// Return all built-in theme presets.
    pub fn presets() -> Vec<Self> {
        vec![
            Self::nord(),
            Self::sordino(),
            Self::terminal(),
            Self::monochrome(),
            Self::tokyo_night(),
        ]
    }

    /// Turn styling on or off. [`resolve_theme`] turns it off for `--plain`, `NO_COLOR`, CI and
    /// output that is not an interactive terminal.
    pub fn with_active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Paint text using the specified colour, bold, and dim settings.
    pub fn paint(&self, text: &str, color: &Color, bold: bool, dim: bool) -> String {
        if !self.active || (matches!(color, Color::Default) && !bold && !dim) {
            return text.to_string();
        }
        let mut ansi = String::new();
        if bold {
            ansi.push_str("\x1b[1m");
        }
        if dim {
            ansi.push_str("\x1b[2m");
        }
        ansi.push_str(&color.fg_ansi());
        format!("{ansi}{text}\x1b[0m")
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint(text, &self.accent, false, false)
    }

    pub fn bold_accent(&self, text: &str) -> String {
        self.paint(text, &self.accent, true, false)
    }

    /// The marker for the current item in a list: an accented `▸` when styled, and a plain `*`
    /// otherwise. Unstyled output is what scripts read, and `*` is what `env ls` has always
    /// printed, so piping it must not change it.
    pub fn marker(&self) -> String {
        if self.active {
            self.bold_accent("▸")
        } else {
            "*".to_string()
        }
    }

    pub fn success(&self, text: &str) -> String {
        self.paint(text, &self.success, false, false)
    }

    pub fn bold_success(&self, text: &str) -> String {
        self.paint(text, &self.success, true, false)
    }

    pub fn warning(&self, text: &str) -> String {
        self.paint(text, &self.warning, false, false)
    }

    pub fn error(&self, text: &str) -> String {
        self.paint(text, &self.error, false, false)
    }

    pub fn bold_error(&self, text: &str) -> String {
        self.paint(text, &self.error, true, false)
    }

    pub fn muted(&self, text: &str) -> String {
        self.paint(text, &self.muted, false, true)
    }

    pub fn fg(&self, text: &str) -> String {
        self.paint(text, &self.fg, false, false)
    }

    pub fn border(&self, text: &str) -> String {
        self.paint(text, &self.border, false, false)
    }
}

/// Load custom themes from TOML files in `themes_dir`.
///
/// Every `*.toml` file is parsed as a [`Theme`]; files that fail to read or parse are
/// skipped, so one broken custom file can never break the CLI. A theme whose `name` field
/// is absent or empty takes the file's stem as its name. A missing directory yields no
/// themes rather than an error.
pub fn load_custom_themes(themes_dir: &Path) -> Vec<Theme> {
    let mut custom = Vec::new();
    let entries = match std::fs::read_dir(themes_dir) {
        Ok(e) => e,
        Err(_) => return custom,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let is_toml = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"));
        if !is_toml {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut theme) = toml::from_str::<Theme>(&content) {
                if theme.name.is_empty() {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        theme.name = stem.to_string();
                    }
                }
                // Without a stem either, the theme could never be selected; skip it.
                if theme.name.is_empty() {
                    continue;
                }
                custom.push(theme);
            }
        }
    }
    custom
}

/// Retrieve all available themes: the built-in presets plus custom themes in `themes_dir`.
/// A custom theme sharing a preset's name does not shadow it; the preset wins.
pub fn available_themes(themes_dir: Option<&Path>) -> Vec<Theme> {
    let mut all = Theme::presets();
    if let Some(dir) = themes_dir {
        for custom in load_custom_themes(dir) {
            if !all
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(&custom.name))
            {
                all.push(custom);
            }
        }
    }
    all
}

/// Find a theme by name (case-insensitive) among the available themes, or `None` when no
/// theme has that name. [`resolve_theme`] maps the `None` case to the `nord` default.
pub fn find_theme(name: &str, themes_dir: Option<&Path>) -> Option<Theme> {
    available_themes(themes_dir)
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(name.trim()))
}

/// Pick the requested theme name by precedence: the explicit CLI override (`--theme <name>`)
/// beats the `SOTTO_THEME` environment variable, which beats the saved configuration.
/// Empty names fall through to the next source; `None` means no source named a theme.
pub fn pick_theme_name<'a>(
    override_name: Option<&'a str>,
    env_name: Option<&'a str>,
    configured_name: Option<&'a str>,
) -> Option<&'a str> {
    [override_name, env_name, configured_name]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty())
}

/// Whether ANSI styling may be emitted. Each gate disables it independently, so pipelines
/// and logs never see escape codes even when a theme is selected:
/// - `plain`: the `--plain` flag was passed;
/// - `no_color`: `NO_COLOR` is present and non-empty (the no-color.org contract);
/// - `ci`: running in a CI environment (headless by definition);
/// - `stdout_tty` / `stdin_tty`: piped output or redirected input means non-interactive.
pub fn styling_active(
    plain: bool,
    no_color: bool,
    ci: bool,
    stdout_tty: bool,
    stdin_tty: bool,
) -> bool {
    !plain && !no_color && !ci && stdout_tty && stdin_tty
}

/// `NO_COLOR` disables styling when present and non-empty (an empty `NO_COLOR=` keeps
/// colours on, per the no-color.org contract).
fn no_color_set() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// CI environments are headless by definition, so styling stays off even on the rare CI
/// runner with a PTY. `CI=0`, `CI=false`, and empty values count as unset.
pub fn ci_enabled(value: Option<&str>) -> bool {
    value
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false"
        })
        .unwrap_or(false)
}

fn ci_set() -> bool {
    ci_enabled(std::env::var("CI").ok().as_deref())
}

/// Resolve the active theme by precedence (see [`pick_theme_name`]), defaulting to `nord`
/// when no source names a theme or the named theme is unknown. The theme's `active` flag
/// follows [`styling_active`]: `--plain`, `NO_COLOR`, CI, and non-TTY execution all turn
/// styling off while keeping the selected palette.
pub fn resolve_theme(
    override_name: Option<&str>,
    config_path: Option<&Path>,
    themes_dir: Option<&Path>,
    plain: bool,
) -> Theme {
    let env_name = std::env::var(THEME_ENV).ok();
    resolve_theme_with(
        override_name,
        env_name.as_deref(),
        config_path,
        themes_dir,
        plain,
    )
}

/// [`resolve_theme`] with the `SOTTO_THEME` value passed in, so its tests neither depend on nor
/// mutate the process environment (which would race with parallel tests).
fn resolve_theme_with(
    override_name: Option<&str>,
    env_name: Option<&str>,
    config_path: Option<&Path>,
    themes_dir: Option<&Path>,
    plain: bool,
) -> Theme {
    // An unreadable or corrupt config degrades to "no saved preference" rather than failing:
    // a cosmetic choice must never break startup (custom themes skip silently likewise).
    let configured_name = config_path.and_then(|p| {
        GlobalConfig::load_from(p)
            .ok()
            .flatten()
            .and_then(|c| c.theme)
    });

    let chosen_name = pick_theme_name(override_name, env_name, configured_name.as_deref());

    let theme = match chosen_name {
        Some(name) => find_theme(name, themes_dir).unwrap_or_else(Theme::nord),
        None => Theme::nord(),
    };

    let active = styling_active(
        plain,
        no_color_set(),
        ci_set(),
        std::io::stdout().is_terminal(),
        std::io::stdin().is_terminal(),
    );
    theme.with_active(active)
}

/// Save the chosen theme name into the global configuration file, preserving the other
/// fields (server/web URLs). Callers validate the name with [`find_theme`] first.
pub fn save_theme_preference(theme_name: &str, config_path: &Path) -> Result<()> {
    let mut config = GlobalConfig::load_from(config_path)?.unwrap_or_default();
    config.theme = Some(theme_name.to_string());
    config.save_to(config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_parsing_hex() {
        assert_eq!(Color::parse("#88C0D0"), Some(Color::Rgb(0x88, 0xC0, 0xD0)));
        assert_eq!(Color::parse("88C0D0"), Some(Color::Rgb(0x88, 0xC0, 0xD0)));
        assert_eq!(Color::parse("#fff"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(Color::parse("#000"), Some(Color::Rgb(0, 0, 0)));
        assert_eq!(Color::parse("default"), Some(Color::Default));
        assert_eq!(Color::parse("none"), Some(Color::Default));
        assert_eq!(Color::parse("DEFAULT"), Some(Color::Default));
        assert_eq!(Color::parse("invalid"), None);
        assert_eq!(Color::parse(""), None);
        assert_eq!(Color::parse("#12"), None);
        assert_eq!(Color::parse("#gggggg"), None);
    }

    #[test]
    fn color_parsing_ansi_names() {
        assert_eq!(Color::parse("red"), Some(Color::Ansi(1)));
        assert_eq!(Color::parse("green"), Some(Color::Ansi(2)));
        assert_eq!(Color::parse("cyan"), Some(Color::Ansi(6)));
        assert_eq!(Color::parse("gray"), Some(Color::Ansi(8)));
        assert_eq!(Color::parse("light_cyan"), Some(Color::Ansi(14)));
        assert_eq!(Color::parse("Red"), Some(Color::Ansi(1)));
        assert_eq!(Color::parse("BRIGHT_WHITE"), Some(Color::Ansi(15)));
    }

    #[test]
    fn color_parsing_palette_index() {
        assert_eq!(Color::parse("ansi(0)"), Some(Color::Ansi(0)));
        assert_eq!(Color::parse("ansi(200)"), Some(Color::Ansi(200)));
        assert_eq!(Color::parse("ansi(255)"), Some(Color::Ansi(255)));
        assert_eq!(Color::parse("ANSI( 12 )"), Some(Color::Ansi(12)));
        assert_eq!(Color::parse("ansi(256)"), None);
        assert_eq!(Color::parse("ansi(-1)"), None);
        assert_eq!(Color::parse("ansi()"), None);
        assert_eq!(Color::parse("ansi(red)"), None);
    }

    #[test]
    fn color_toml_round_trip() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Wrap {
            color: Color,
        }
        for color in [
            Color::Rgb(0x88, 0xC0, 0xD0),
            Color::Ansi(1),
            Color::Ansi(14),
            Color::Ansi(200),
            Color::Default,
        ] {
            let text = toml::to_string(&Wrap { color }).unwrap();
            let back: Wrap = toml::from_str(&text).unwrap();
            assert_eq!(back.color, color, "roundtrip failed for {color:?}");
        }
    }

    #[test]
    fn theme_painting_honors_active() {
        let theme = Theme::nord().with_active(true);
        let painted = theme.accent("hello");
        assert!(painted.contains("\x1b[38;2;136;192;208m"));
        assert!(painted.contains("hello"));
        assert!(painted.ends_with("\x1b[0m"));

        // Every painter is a passthrough when the theme is inactive.
        let inactive = Theme::nord().with_active(false);
        assert_eq!(inactive.accent("hello"), "hello");
        assert_eq!(inactive.bold_accent("hello"), "hello");
        assert_eq!(inactive.success("hello"), "hello");
        assert_eq!(inactive.warning("hello"), "hello");
        assert_eq!(inactive.error("hello"), "hello");
        assert_eq!(inactive.muted("hello"), "hello");
        assert_eq!(inactive.fg("hello"), "hello");
        assert_eq!(inactive.border("hello"), "hello");
    }

    #[test]
    fn marker_is_a_plain_asterisk_when_unstyled() {
        // Unstyled output is what scripts read; it keeps the `*` that `env ls` always printed.
        assert_eq!(Theme::nord().with_active(false).marker(), "*");
        let styled = Theme::nord().with_active(true).marker();
        assert!(
            styled.contains('▸') && styled.contains("\x1b["),
            "{styled:?}"
        );
    }

    #[test]
    fn styling_active_requires_every_gate() {
        assert!(styling_active(false, false, false, true, true));
        assert!(!styling_active(true, false, false, true, true)); // --plain
        assert!(!styling_active(false, true, false, true, true)); // NO_COLOR
        assert!(!styling_active(false, false, true, true, true)); // CI
        assert!(!styling_active(false, false, false, false, true)); // piped stdout
        assert!(!styling_active(false, false, false, true, false)); // redirected stdin
    }

    #[test]
    fn ci_enabled_ignores_empty_and_false_values() {
        assert!(!ci_enabled(None));
        assert!(!ci_enabled(Some("")));
        assert!(!ci_enabled(Some("0")));
        assert!(!ci_enabled(Some(" false ")));
        assert!(ci_enabled(Some("true")));
        assert!(ci_enabled(Some("github")));
    }

    #[test]
    fn theme_presets_have_unique_names() {
        let presets = Theme::presets();
        assert_eq!(presets.len(), 5);
        let names: Vec<_> = presets.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"nord"));
        assert!(names.contains(&"sordino"));
        assert!(names.contains(&"terminal"));
        assert!(names.contains(&"monochrome"));
        assert!(names.contains(&"tokyo-night"));
    }

    /// A valid custom theme file; `name_line` is e.g. `name = "custom-synth"` or empty.
    fn custom_toml(name_line: &str) -> String {
        format!(
            "{name_line}\n\
             bg = \"#120024\"\n\
             fg = \"#ffffff\"\n\
             accent = \"#ff007f\"\n\
             success = \"#00ff66\"\n\
             warning = \"#ffaa00\"\n\
             error = \"#ff0033\"\n\
             muted = \"#775588\"\n\
             border = \"#331144\"\n"
        )
    }

    #[test]
    fn custom_theme_toml_roundtrip() {
        let theme: Theme = toml::from_str(&custom_toml("name = \"custom-synth\"")).unwrap();
        assert_eq!(theme.name, "custom-synth");
        assert_eq!(theme.accent, Color::Rgb(0xff, 0x00, 0x7f));
        assert_eq!(theme.bg, Some(Color::Rgb(0x12, 0x00, 0x24)));
        // A freshly parsed theme starts active; `resolve_theme` gates it at runtime.
        assert!(theme.active);
    }

    #[test]
    fn custom_theme_toml_name_defaults_to_empty() {
        let theme: Theme = toml::from_str(&custom_toml("")).unwrap();
        assert_eq!(theme.name, "");
    }

    #[test]
    fn custom_theme_toml_rejects_invalid_color() {
        let text = custom_toml("name = \"broken\"").replace("#ff007f", "not-a-color");
        let err = toml::from_str::<Theme>(&text).unwrap_err();
        assert!(
            err.to_string().contains("invalid colour specification"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn custom_theme_toml_rejects_missing_tokens() {
        let err = toml::from_str::<Theme>("name = \"thin\"\nfg = \"#ffffff\"\n").unwrap_err();
        assert!(
            err.to_string().contains("missing field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_custom_themes_reads_dir_and_skips_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let themes_dir = dir.path().join("themes");
        std::fs::create_dir(&themes_dir).unwrap();
        std::fs::write(
            themes_dir.join("synth.toml"),
            custom_toml("name = \"custom-synth\""),
        )
        .unwrap();
        // No `name` field: the filename stem becomes the theme name.
        std::fs::write(themes_dir.join("noname.toml"), custom_toml("")).unwrap();
        // Broken TOML and non-TOML files are skipped, never fatal.
        std::fs::write(themes_dir.join("broken.toml"), "accent = [unclosed").unwrap();
        std::fs::write(themes_dir.join("notes.txt"), "not a theme").unwrap();

        let loaded = load_custom_themes(&themes_dir);
        let mut names: Vec<_> = loaded.iter().map(|t| t.name.clone()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["custom-synth".to_string(), "noname".to_string()]
        );
        assert!(loaded.iter().all(|t| t.active));

        // A missing directory yields no themes rather than an error.
        assert!(load_custom_themes(&dir.path().join("absent")).is_empty());
    }

    #[test]
    fn available_themes_keep_preset_over_same_named_custom() {
        let dir = tempfile::tempdir().unwrap();
        let themes_dir = dir.path().join("themes");
        std::fs::create_dir(&themes_dir).unwrap();
        std::fs::write(themes_dir.join("nord.toml"), custom_toml("name = \"NORD\"")).unwrap();
        std::fs::write(
            themes_dir.join("extra.toml"),
            custom_toml("name = \"extra\""),
        )
        .unwrap();

        let all = available_themes(Some(&themes_dir));
        assert_eq!(all.len(), 6);
        // The preset wins (case-insensitively); the custom `nord` is dropped.
        let nord = all.iter().find(|t| t.name == "nord").unwrap();
        assert_eq!(nord.accent, Color::Rgb(0x88, 0xC0, 0xD0));
        assert!(all.iter().any(|t| t.name == "extra"));
    }

    #[test]
    fn find_theme_is_case_insensitive_and_trims() {
        assert_eq!(find_theme("NORD", None).unwrap().name, "nord");
        assert_eq!(
            find_theme("  tokyo-night ", None).unwrap().name,
            "tokyo-night"
        );
        assert!(find_theme("does-not-exist", None).is_none());
    }

    #[test]
    fn pick_theme_name_precedence() {
        assert_eq!(pick_theme_name(Some("a"), Some("b"), Some("c")), Some("a"));
        assert_eq!(pick_theme_name(None, Some("b"), Some("c")), Some("b"));
        assert_eq!(pick_theme_name(None, None, Some("c")), Some("c"));
        assert_eq!(pick_theme_name(None, None, None), None);
        // Empty names fall through to the next source; surrounding space is trimmed.
        assert_eq!(pick_theme_name(Some(""), Some("  "), Some("c")), Some("c"));
        assert_eq!(pick_theme_name(Some("  a  "), None, None), Some("a"));
    }

    #[test]
    fn resolve_theme_precedence() {
        // Through `resolve_theme_with`, so a SOTTO_THEME set in the shell running the tests
        // cannot change the answer.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = Some(config_path.as_path());

        // Default when nothing configured
        assert_eq!(
            resolve_theme_with(None, None, config, None, false).name,
            "nord"
        );

        // Configured in config.toml
        save_theme_preference("sordino", &config_path).unwrap();
        assert_eq!(
            resolve_theme_with(None, None, config, None, false).name,
            "sordino"
        );

        // SOTTO_THEME beats the configured theme
        let t = resolve_theme_with(None, Some("monochrome"), config, None, false);
        assert_eq!(t.name, "monochrome");

        // The CLI override beats both
        let t = resolve_theme_with(Some("tokyo-night"), Some("monochrome"), config, None, false);
        assert_eq!(t.name, "tokyo-night");

        // Unknown names fall back to nord (the binary warns; see main.rs)
        let t = resolve_theme_with(Some("does-not-exist"), None, config, None, false);
        assert_eq!(t.name, "nord");

        // Plain flag disables styling (test output is piped, so `active` is always false
        // here; the on/off matrix lives in `styling_active_requires_every_gate`)
        assert!(!resolve_theme_with(None, None, config, None, true).active);
    }

    #[test]
    fn resolve_theme_selects_custom_theme() {
        let dir = tempfile::tempdir().unwrap();
        let themes_dir = dir.path().join("themes");
        std::fs::create_dir(&themes_dir).unwrap();
        std::fs::write(
            themes_dir.join("synth.toml"),
            custom_toml("name = \"custom-synth\""),
        )
        .unwrap();

        let theme = resolve_theme(Some("custom-synth"), None, Some(&themes_dir), false);
        assert_eq!(theme.name, "custom-synth");
        assert_eq!(theme.accent, Color::Rgb(0xff, 0x00, 0x7f));
    }

    #[test]
    fn save_theme_preference_preserves_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        GlobalConfig {
            server_url: Some("https://api.sotto.dev".into()),
            web_url: Some("https://app.sotto.dev".into()),
            theme: None,
        }
        .save_to(&config_path)
        .unwrap();

        save_theme_preference("monochrome", &config_path).unwrap();
        let back = GlobalConfig::load_from(&config_path).unwrap().unwrap();
        assert_eq!(back.theme.as_deref(), Some("monochrome"));
        assert_eq!(back.server_url.as_deref(), Some("https://api.sotto.dev"));
        assert_eq!(back.web_url.as_deref(), Some("https://app.sotto.dev"));
    }
}
