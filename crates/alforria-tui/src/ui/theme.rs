//! `theme/index.ts` + `context/theme.tsx` — the 33 built-in palettes and
//! the dark/light resolution (`ui/theme.rs`, M8.3).
//!
//! Every `theme/assets/*.json` ships verbatim (embedded via
//! `include_str!`); [`resolve_theme`] ports the TS color resolution
//! (hex → ref-name → `{dark, light}` variant, `transparent`/`none`).
//! Palette keys keep their TS names for [`Theme::get`].

use std::collections::BTreeMap;

use serde_json::Value;

use crate::state::kv::keys;

macro_rules! theme_asset {
    ($file:literal) => {
        include_str!(concat!("theme/assets/", $file))
    };
}

macro_rules! default_themes {
    ($($name:literal => $file:literal),* $(,)?) => {
        /// `DEFAULT_THEMES` (`theme/index.ts:130-164`) — name → raw JSON.
        pub const DEFAULT_THEMES: &[(&str, &str)] = &[
            $(($name, theme_asset!($file))),*
        ];
    };
}

default_themes! {
    "aura" => "aura.json",
    "ayu" => "ayu.json",
    "catppuccin" => "catppuccin.json",
    "catppuccin-frappe" => "catppuccin-frappe.json",
    "catppuccin-macchiato" => "catppuccin-macchiato.json",
    "cobalt2" => "cobalt2.json",
    "cursor" => "cursor.json",
    "dracula" => "dracula.json",
    "everforest" => "everforest.json",
    "flexoki" => "flexoki.json",
    "github" => "github.json",
    "gruvbox" => "gruvbox.json",
    "kanagawa" => "kanagawa.json",
    "material" => "material.json",
    "matrix" => "matrix.json",
    "mercury" => "mercury.json",
    "monokai" => "monokai.json",
    "nightowl" => "nightowl.json",
    "nord" => "nord.json",
    "one-dark" => "one-dark.json",
    "osaka-jade" => "osaka-jade.json",
    "opencode" => "opencode.json",
    "orng" => "orng.json",
    "lucent-orng" => "lucent-orng.json",
    "palenight" => "palenight.json",
    "rosepine" => "rosepine.json",
    "solarized" => "solarized.json",
    "synthwave84" => "synthwave84.json",
    "tokyonight" => "tokyonight.json",
    "vesper" => "vesper.json",
    "vercel" => "vercel.json",
    "zenburn" => "zenburn.json",
    "carbonfox" => "carbonfox.json",
}

/// The fallback theme (`context/theme.tsx:266`).
pub const DEFAULT_THEME: &str = "opencode";

/// `RGBA` from `@opentui/core` — floats in 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const fn from_ints(r: u8, g: u8, b: u8) -> Rgba {
        Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    pub const fn from_values(r: f32, g: f32, b: f32, a: f32) -> Rgba {
        Rgba { r, g, b, a }
    }

    /// `RGBA.fromHex` — `#rgb` / `#rrggbb` / `#rrggbbaa`.
    pub fn from_hex(input: &str) -> Option<Rgba> {
        let hex = input.strip_prefix('#')?;
        match hex.len() {
            3 => {
                let channel = |s: &str| u8::from_str_radix(&s.repeat(2), 16).ok();
                let [r, g, b] = [
                    channel(&hex[0..1])?,
                    channel(&hex[1..2])?,
                    channel(&hex[2..3])?,
                ];
                Some(Rgba::from_ints(r, g, b))
            }
            6 | 8 => {
                let channel = |s: &str| u8::from_str_radix(s, 16).ok();
                let (r, g, b) = (
                    channel(&hex[0..2])?,
                    channel(&hex[2..4])?,
                    channel(&hex[4..6])?,
                );
                let a = if hex.len() == 8 {
                    channel(&hex[6..8])? as f32 / 255.0
                } else {
                    1.0
                };
                Some(Rgba {
                    r: r as f32 / 255.0,
                    g: g as f32 / 255.0,
                    b: b as f32 / 255.0,
                    a,
                })
            }
            _ => None,
        }
    }

    /// `0.299r + 0.587g + 0.114b` (`theme/index.ts:105`).
    pub fn luminance(self) -> f32 {
        0.299 * self.r + 0.587 * self.g + 0.114 * self.b
    }

    /// Ratatui has no alpha channel — render as opaque RGB.
    pub fn to_color(self) -> ratatui::style::Color {
        ratatui::style::Color::Rgb(
            (self.r * 255.0).round() as u8,
            (self.g * 255.0).round() as u8,
            (self.b * 255.0).round() as u8,
        )
    }
}

/// `tint` (`theme/index.ts:346-351`) — result is opaque (`RGBA.fromInts`).
pub fn tint(base: Rgba, overlay: Rgba, alpha: f32) -> Rgba {
    let channel = |b: f32, o: f32| ((b + (o - b) * alpha) * 255.0).round();
    Rgba::from_ints(
        channel(base.r, overlay.r) as u8,
        channel(base.g, overlay.g) as u8,
        channel(base.b, overlay.b) as u8,
    )
}

/// `ansiToRgba` (`theme/index.ts:301-344`) — the standard 16, the
/// 6x6x6 cube and the grayscale ramp.
pub fn ansi_to_rgba(code: i64) -> Rgba {
    if (0..16).contains(&code) {
        let table = [
            "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
            "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
        ];
        return Rgba::from_hex(table[code as usize]).unwrap_or(Rgba::from_ints(0, 0, 0));
    }
    if (16..232).contains(&code) {
        let index = code - 16;
        let b = index % 6;
        let g = (index / 6) % 6;
        let r = index / 36;
        let value = |x: i64| if x == 0 { 0 } else { x * 40 + 55 };
        return Rgba::from_ints(value(r) as u8, value(g) as u8, value(b) as u8);
    }
    if (232..256).contains(&code) {
        let gray = ((code - 232) * 10 + 8) as u8;
        return Rgba::from_ints(gray, gray, gray);
    }
    Rgba::from_ints(0, 0, 0)
}

/// `TerminalColors` from `@opentui/core` — the live terminal palette.
/// The Rust runtime has no `renderer.getPalette()` seam, so only the
/// `system` theme generation consumes this and callers pass defaults.
#[derive(Debug, Clone, Default)]
pub struct TerminalColors {
    pub default_background: Option<String>,
    pub default_foreground: Option<String>,
    pub palette: Vec<String>,
}

impl TerminalColors {
    /// The port's fallback source: no queried palette, so `col(i)`
    /// resolves through [`ansi_to_rgba`].
    pub fn defaults() -> TerminalColors {
        TerminalColors::default()
    }

    fn color(&self, index: usize) -> Rgba {
        self.palette
            .get(index)
            .and_then(|hex| Rgba::from_hex(hex))
            .unwrap_or_else(|| ansi_to_rgba(index as i64))
    }
}

fn hex(color: Rgba) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (color.r * 255.0).round() as u8,
        (color.g * 255.0).round() as u8,
        (color.b * 255.0).round() as u8
    )
}

fn hex_alpha(color: Rgba) -> String {
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        (color.r * 255.0).round() as u8,
        (color.g * 255.0).round() as u8,
        (color.b * 255.0).round() as u8,
        (color.a * 255.0).round() as u8
    )
}

/// `generateGrayScale` (`theme/index.ts:471-523`) — 12 steps above
/// (dark) / below (light) the terminal background.
fn generate_gray_scale(bg: Rgba, is_dark: bool) -> Vec<Rgba> {
    let (bg_r, bg_g, bg_b) = (bg.r * 255.0, bg.g * 255.0, bg.b * 255.0);
    let luminance = 0.299 * bg_r + 0.587 * bg_g + 0.114 * bg_b;
    let mut grays = vec![Rgba::from_ints(0, 0, 0); 13];
    for (i, gray) in grays.iter_mut().enumerate().skip(1).take(12) {
        let factor = i as f32 / 12.0;
        let (new_r, new_g, new_b) = if is_dark {
            if luminance < 10.0 {
                let gray = (factor * 0.4 * 255.0).floor();
                (gray, gray, gray)
            } else {
                let new_lum = luminance + (255.0 - luminance) * factor * 0.4;
                let ratio = new_lum / luminance;
                (
                    (bg_r * ratio).min(255.0),
                    (bg_g * ratio).min(255.0),
                    (bg_b * ratio).min(255.0),
                )
            }
        } else if luminance > 245.0 {
            let gray = (255.0 - factor * 0.4 * 255.0).floor();
            (gray, gray, gray)
        } else {
            let new_lum = luminance * (1.0 - factor * 0.4);
            let ratio = new_lum / luminance;
            (
                (bg_r * ratio).max(0.0),
                (bg_g * ratio).max(0.0),
                (bg_b * ratio).max(0.0),
            )
        };
        *gray = Rgba::from_ints(
            new_r.floor() as u8,
            new_g.floor() as u8,
            new_b.floor() as u8,
        );
    }
    grays
}

/// `generateMutedTextColor` (`theme/index.ts:525-554`).
fn generate_muted_text_color(bg: Rgba, is_dark: bool) -> Rgba {
    let (bg_r, bg_g, bg_b) = (bg.r * 255.0, bg.g * 255.0, bg.b * 255.0);
    let bg_lum = 0.299 * bg_r + 0.587 * bg_g + 0.114 * bg_b;
    let gray = if is_dark {
        if bg_lum < 10.0 {
            180.0
        } else {
            (160.0 + bg_lum * 0.3).floor().min(200.0)
        }
    } else if bg_lum > 245.0 {
        75.0
    } else {
        (100.0 - (255.0 - bg_lum) * 0.2).floor().max(60.0)
    };
    Rgba::from_ints(gray as u8, gray as u8, gray as u8)
}

/// `generateSystem` (`theme/index.ts:360-469`) — a palette built from
/// the terminal colours. The Rust runtime cannot query the live
/// terminal palette (`renderer.getPalette`), so callers pass
/// [`TerminalColors::defaults`] and the ANSI-256 fallbacks are used
/// (recorded divergence from the TS reference).
pub fn generate_system(colors: &TerminalColors, mode: Mode) -> Value {
    let bg = colors
        .default_background
        .as_deref()
        .and_then(Rgba::from_hex)
        .unwrap_or_else(|| colors.color(0));
    let fg = colors
        .default_foreground
        .as_deref()
        .and_then(Rgba::from_hex)
        .unwrap_or_else(|| colors.color(7));
    let transparent = Rgba::from_values(bg.r, bg.g, bg.b, 0.0);
    let is_dark = mode == Mode::Dark;

    let col = |i: usize| colors.color(i);
    let red = col(1);
    let green = col(2);
    let yellow = col(3);
    let blue = col(4);
    let magenta = col(5);
    let cyan = col(6);
    let red_bright = col(9);
    let green_bright = col(10);

    let grays = generate_gray_scale(bg, is_dark);
    let text_muted = generate_muted_text_color(bg, is_dark);

    let diff_alpha = if is_dark { 0.22 } else { 0.14 };
    let diff_added_bg = tint(bg, green, diff_alpha);
    let diff_removed_bg = tint(bg, red, diff_alpha);
    let diff_context_bg = grays[2];
    let diff_added_line_number_bg = tint(grays[2], green, diff_alpha);
    let diff_removed_line_number_bg = tint(grays[2], red, diff_alpha);
    let diff_line_number = text_muted;

    let entries = [
        ("primary", cyan),
        ("secondary", magenta),
        ("accent", cyan),
        ("error", red),
        ("warning", yellow),
        ("success", green),
        ("info", cyan),
        ("text", fg),
        ("textMuted", text_muted),
        ("selectedListItemText", bg),
        ("background", transparent),
        ("backgroundPanel", grays[2]),
        ("backgroundElement", grays[3]),
        ("backgroundMenu", grays[3]),
        ("borderSubtle", grays[6]),
        ("border", grays[7]),
        ("borderActive", grays[8]),
        ("diffAdded", green),
        ("diffRemoved", red),
        ("diffContext", grays[7]),
        ("diffHunkHeader", grays[7]),
        ("diffHighlightAdded", green_bright),
        ("diffHighlightRemoved", red_bright),
        ("diffAddedBg", diff_added_bg),
        ("diffRemovedBg", diff_removed_bg),
        ("diffContextBg", diff_context_bg),
        ("diffLineNumber", diff_line_number),
        ("diffAddedLineNumberBg", diff_added_line_number_bg),
        ("diffRemovedLineNumberBg", diff_removed_line_number_bg),
        ("markdownText", fg),
        ("markdownHeading", fg),
        ("markdownLink", blue),
        ("markdownLinkText", cyan),
        ("markdownCode", green),
        ("markdownBlockQuote", yellow),
        ("markdownEmph", yellow),
        ("markdownStrong", fg),
        ("markdownHorizontalRule", grays[7]),
        ("markdownListItem", blue),
        ("markdownListEnumeration", cyan),
        ("markdownImage", blue),
        ("markdownImageText", cyan),
        ("markdownCodeBlock", fg),
        ("syntaxComment", text_muted),
        ("syntaxKeyword", magenta),
        ("syntaxFunction", blue),
        ("syntaxVariable", fg),
        ("syntaxString", green),
        ("syntaxNumber", yellow),
        ("syntaxType", cyan),
        ("syntaxOperator", cyan),
        ("syntaxPunctuation", fg),
    ];
    let mut theme = serde_json::Map::new();
    for (key, color) in entries {
        let value = if color.a == 0.0 {
            Value::String(hex_alpha(color))
        } else {
            Value::String(hex(color))
        };
        theme.insert(key.to_string(), value);
    }
    serde_json::json!({ "theme": theme })
}

/// `"dark" | "light"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }

    pub fn pick(value: &Value) -> Option<Mode> {
        match value.as_str()? {
            "dark" => Some(Mode::Dark),
            "light" => Some(Mode::Light),
            _ => None,
        }
    }
}

/// The resolved `Theme` (`theme/index.ts:36-91`) — keys keep their TS names
/// for [`Theme::get`].
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub primary: Rgba,
    pub secondary: Rgba,
    pub accent: Rgba,
    pub error: Rgba,
    pub warning: Rgba,
    pub success: Rgba,
    pub info: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    pub selected_list_item_text: Rgba,
    pub background: Rgba,
    pub background_panel: Rgba,
    pub background_element: Rgba,
    pub background_menu: Rgba,
    pub border: Rgba,
    pub border_active: Rgba,
    pub border_subtle: Rgba,
    pub diff_added: Rgba,
    pub diff_removed: Rgba,
    pub diff_context: Rgba,
    pub diff_hunk_header: Rgba,
    pub diff_highlight_added: Rgba,
    pub diff_highlight_removed: Rgba,
    pub diff_added_bg: Rgba,
    pub diff_removed_bg: Rgba,
    pub diff_context_bg: Rgba,
    pub diff_line_number: Rgba,
    pub diff_added_line_number_bg: Rgba,
    pub diff_removed_line_number_bg: Rgba,
    pub markdown_text: Rgba,
    pub markdown_heading: Rgba,
    pub markdown_link: Rgba,
    pub markdown_link_text: Rgba,
    pub markdown_code: Rgba,
    pub markdown_block_quote: Rgba,
    pub markdown_emph: Rgba,
    pub markdown_strong: Rgba,
    pub markdown_horizontal_rule: Rgba,
    pub markdown_list_item: Rgba,
    pub markdown_list_enumeration: Rgba,
    pub markdown_image: Rgba,
    pub markdown_image_text: Rgba,
    pub markdown_code_block: Rgba,
    pub syntax_comment: Rgba,
    pub syntax_keyword: Rgba,
    pub syntax_function: Rgba,
    pub syntax_variable: Rgba,
    pub syntax_string: Rgba,
    pub syntax_number: Rgba,
    pub syntax_type: Rgba,
    pub syntax_operator: Rgba,
    pub syntax_punctuation: Rgba,
    pub thinking_opacity: f32,
    /// `_hasSelectedListItemText` (`theme/index.ts:275-282`).
    pub has_selected_list_item_text: bool,
}

impl Theme {
    /// `theme[color as keyof typeof theme]` (`local.tsx:126`) — palette
    /// keys by their TS names.
    pub fn get(&self, key: &str) -> Option<Rgba> {
        Some(match key {
            "primary" => self.primary,
            "secondary" => self.secondary,
            "accent" => self.accent,
            "error" => self.error,
            "warning" => self.warning,
            "success" => self.success,
            "info" => self.info,
            "text" => self.text,
            "textMuted" => self.text_muted,
            "selectedListItemText" => self.selected_list_item_text,
            "background" => self.background,
            "backgroundPanel" => self.background_panel,
            "backgroundElement" => self.background_element,
            "backgroundMenu" => self.background_menu,
            "border" => self.border,
            "borderActive" => self.border_active,
            "borderSubtle" => self.border_subtle,
            "diffAdded" => self.diff_added,
            "diffRemoved" => self.diff_removed,
            "diffContext" => self.diff_context,
            "diffHunkHeader" => self.diff_hunk_header,
            "diffHighlightAdded" => self.diff_highlight_added,
            "diffHighlightRemoved" => self.diff_highlight_removed,
            "diffAddedBg" => self.diff_added_bg,
            "diffRemovedBg" => self.diff_removed_bg,
            "diffContextBg" => self.diff_context_bg,
            "diffLineNumber" => self.diff_line_number,
            "diffAddedLineNumberBg" => self.diff_added_line_number_bg,
            "diffRemovedLineNumberBg" => self.diff_removed_line_number_bg,
            "markdownText" => self.markdown_text,
            "markdownHeading" => self.markdown_heading,
            "markdownLink" => self.markdown_link,
            "markdownLinkText" => self.markdown_link_text,
            "markdownCode" => self.markdown_code,
            "markdownBlockQuote" => self.markdown_block_quote,
            "markdownEmph" => self.markdown_emph,
            "markdownStrong" => self.markdown_strong,
            "markdownHorizontalRule" => self.markdown_horizontal_rule,
            "markdownListItem" => self.markdown_list_item,
            "markdownListEnumeration" => self.markdown_list_enumeration,
            "markdownImage" => self.markdown_image,
            "markdownImageText" => self.markdown_image_text,
            "markdownCodeBlock" => self.markdown_code_block,
            "syntaxComment" => self.syntax_comment,
            "syntaxKeyword" => self.syntax_keyword,
            "syntaxFunction" => self.syntax_function,
            "syntaxVariable" => self.syntax_variable,
            "syntaxString" => self.syntax_string,
            "syntaxNumber" => self.syntax_number,
            "syntaxType" => self.syntax_type,
            "syntaxOperator" => self.syntax_operator,
            "syntaxPunctuation" => self.syntax_punctuation,
            _ => return None,
        })
    }
}

macro_rules! resolved {
    ($map:expr, $selected:expr, $background_menu:expr, $($field:ident => $key:literal),* $(,)?) => {
        Theme {
            $($field: take_color(&mut $map, $key)?,)*
            selected_list_item_text: $selected,
            background_menu: $background_menu,
            thinking_opacity: 0.6,
            has_selected_list_item_text: false,
        }
    };
}

fn take_color(map: &mut BTreeMap<String, Rgba>, key: &str) -> anyhow::Result<Rgba> {
    map.remove(key)
        .ok_or_else(|| anyhow::anyhow!("theme is missing \"{key}\""))
}

/// `isTheme` (`theme/index.ts:194-198`).
pub fn is_theme(value: &Value) -> bool {
    value.get("theme").map(Value::is_object).unwrap_or(false)
}

/// `resolveTheme(theme, mode)` (`theme/index.ts:241-299`).
pub fn resolve_theme(raw: &Value, mode: Mode) -> anyhow::Result<Theme> {
    let defs = raw.get("defs").and_then(Value::as_object);
    let entries = raw
        .get("theme")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("theme has no theme object"))?;

    fn resolve_color(
        value: &Value,
        mode: Mode,
        defs: Option<&serde_json::Map<String, Value>>,
        entries: &serde_json::Map<String, Value>,
        chain: &[&str],
    ) -> anyhow::Result<Rgba> {
        if let Some(name) = value.as_str() {
            if name == "transparent" || name == "none" {
                return Ok(Rgba::from_values(0.0, 0.0, 0.0, 0.0));
            }
            if let Some(hex) = name.strip_prefix('#') {
                return Rgba::from_hex(&format!("#{hex}"))
                    .ok_or_else(|| anyhow::anyhow!("invalid color: {name}"));
            }
            if chain.contains(&name) {
                return Err(anyhow::anyhow!(
                    "Circular color reference: {}",
                    [chain, &[name]].concat().join(" -> ")
                ));
            }
            let next = defs
                .and_then(|d| d.get(name))
                .or_else(|| entries.get(name))
                .ok_or_else(|| {
                    anyhow::anyhow!("Color reference \"{name}\" not found in defs or theme")
                })?;
            let chain = [chain, &[name]].concat();
            return resolve_color(next, mode, defs, entries, &chain);
        }
        if let Some(code) = value.as_i64() {
            return Ok(ansi_to_rgba(code));
        }
        if let Some(variant) = value.as_object() {
            let side = match mode {
                Mode::Dark => "dark",
                Mode::Light => "light",
            };
            let value = variant
                .get(side)
                .ok_or_else(|| anyhow::anyhow!("variant is missing its {side} value"))?;
            return resolve_color(value, mode, defs, entries, chain);
        }
        Err(anyhow::anyhow!("unsupported color value"))
    }

    let resolve = |key: &str| {
        let value = entries.get(key).ok_or_else(|| {
            anyhow::anyhow!("Color reference \"{key}\" not found in defs or theme")
        })?;
        resolve_color(value, mode, defs, entries, &[])
    };

    let mut map = BTreeMap::new();
    for key in entries.keys() {
        if matches!(
            key.as_str(),
            "selectedListItemText" | "backgroundMenu" | "thinkingOpacity"
        ) {
            continue;
        }
        map.insert(key.clone(), resolve(key)?);
    }

    let has_selected_list_item_text = entries.contains_key("selectedListItemText");
    let selected = match entries.get("selectedListItemText") {
        Some(value) => resolve_color(value, mode, defs, entries, &[])?,
        None => map.get("background").copied().unwrap(),
    };
    let background_menu = match entries.get("backgroundMenu") {
        Some(value) => resolve_color(value, mode, defs, entries, &[])?,
        None => map.get("backgroundElement").copied().unwrap(),
    };
    let thinking_opacity = entries
        .get("thinkingOpacity")
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .unwrap_or(0.6);

    let theme = resolved! {
        map,
        selected,
        background_menu,
        primary => "primary",
        secondary => "secondary",
        accent => "accent",
        error => "error",
        warning => "warning",
        success => "success",
        info => "info",
        text => "text",
        text_muted => "textMuted",
        background => "background",
        background_panel => "backgroundPanel",
        background_element => "backgroundElement",
        border => "border",
        border_active => "borderActive",
        border_subtle => "borderSubtle",
        diff_added => "diffAdded",
        diff_removed => "diffRemoved",
        diff_context => "diffContext",
        diff_hunk_header => "diffHunkHeader",
        diff_highlight_added => "diffHighlightAdded",
        diff_highlight_removed => "diffHighlightRemoved",
        diff_added_bg => "diffAddedBg",
        diff_removed_bg => "diffRemovedBg",
        diff_context_bg => "diffContextBg",
        diff_line_number => "diffLineNumber",
        diff_added_line_number_bg => "diffAddedLineNumberBg",
        diff_removed_line_number_bg => "diffRemovedLineNumberBg",
        markdown_text => "markdownText",
        markdown_heading => "markdownHeading",
        markdown_link => "markdownLink",
        markdown_link_text => "markdownLinkText",
        markdown_code => "markdownCode",
        markdown_block_quote => "markdownBlockQuote",
        markdown_emph => "markdownEmph",
        markdown_strong => "markdownStrong",
        markdown_horizontal_rule => "markdownHorizontalRule",
        markdown_list_item => "markdownListItem",
        markdown_list_enumeration => "markdownListEnumeration",
        markdown_image => "markdownImage",
        markdown_image_text => "markdownImageText",
        markdown_code_block => "markdownCodeBlock",
        syntax_comment => "syntaxComment",
        syntax_keyword => "syntaxKeyword",
        syntax_function => "syntaxFunction",
        syntax_variable => "syntaxVariable",
        syntax_string => "syntaxString",
        syntax_number => "syntaxNumber",
        syntax_type => "syntaxType",
        syntax_operator => "syntaxOperator",
        syntax_punctuation => "syntaxPunctuation",
    };
    Ok(Theme {
        thinking_opacity,
        has_selected_list_item_text,
        ..theme
    })
}

/// `selectedForeground(theme, bg?)` (`theme/index.ts:95-111`).
pub fn selected_foreground(theme: &Theme, bg: Option<Rgba>) -> Rgba {
    if theme.has_selected_list_item_text {
        return theme.selected_list_item_text;
    }
    if theme.background.a == 0.0 {
        let target = bg.unwrap_or(theme.primary);
        return if target.luminance() > 0.5 {
            Rgba::from_ints(0, 0, 0)
        } else {
            Rgba::from_ints(255, 255, 255)
        };
    }
    theme.background
}

/// The `system` palette (`theme/index.ts:360-469`) — generated from
/// the terminal colours, here the ANSI-256 defaults.
pub fn system_theme(mode: Mode) -> Value {
    generate_system(&TerminalColors::defaults(), mode)
}

/// `allThemes()` — the built-in registry (custom/plugin themes are not
/// part of the Rust runtime) plus the generated `system` palette.
pub fn all_themes() -> Vec<(&'static str, Value)> {
    let mut themes: Vec<(&'static str, Value)> = DEFAULT_THEMES
        .iter()
        .filter_map(|(name, raw)| serde_json::from_str(raw).ok().map(|v| (*name, v)))
        .collect();
    themes.push(("system", system_theme(Mode::Dark)));
    themes
}

/// `hasTheme(name)` (`theme/index.ts:215-218`).
pub fn has_theme(name: &str) -> bool {
    name == "system"
        || DEFAULT_THEMES
            .iter()
            .any(|(candidate, _)| *candidate == name)
}

/// The theme context state (`context/theme.tsx:84-98`): active theme name,
/// dark/light mode and its lock, persisted through `kv.json`.
#[derive(Debug, Clone)]
pub struct ThemeStore {
    pub active: String,
    pub mode: Mode,
    pub lock: Option<Mode>,
}

impl Default for ThemeStore {
    fn default() -> ThemeStore {
        ThemeStore {
            active: DEFAULT_THEME.to_string(),
            mode: Mode::Dark,
            lock: None,
        }
    }
}

/// `terminalMode(colors)` (`theme/index.ts:353-358`), inverted: the
/// Rust runtime cannot query the terminal palette live, so the
/// background is detected from the `COLORFGBG` env signal (set by
/// terminals/multiplexers that know the background). Unknown
/// backgrounds default to dark.
fn detected_mode() -> Mode {
    detected_mode_with(std::env::var("COLORFGBG").ok().as_deref())
}

fn detected_mode_with(colorfgbg: Option<&str>) -> Mode {
    colorfgbg
        .and_then(|value| {
            let background = value.rsplit(';').next()?.trim().parse::<u8>().ok()?;
            match background {
                0..=6 | 8 | 16 => Some(Mode::Dark),
                _ => Some(Mode::Light),
            }
        })
        .unwrap_or(Mode::Dark)
}

impl ThemeStore {
    /// `ThemeProvider` init (`context/theme.tsx:114-124`).
    pub fn init(kv: &mut crate::state::kv::Kv, config_theme: Option<&str>) -> ThemeStore {
        let lock = kv.get(keys::THEME_MODE_LOCK, Value::Null);
        let lock = Mode::pick(&lock);
        let mode = lock.unwrap_or_else(detected_mode);
        if lock.is_none() {
            let legacy = kv.get(keys::THEME_MODE, Value::Null);
            if Mode::pick(&legacy).is_some() {
                kv.set(keys::THEME_MODE, Value::Null);
            }
        }
        let active = config_theme
            .map(str::to_string)
            .or_else(|| match kv.get(keys::THEME, Value::Null) {
                value if value.is_string() => value.as_str().map(str::to_string),
                _ => None,
            })
            .unwrap_or_else(|| DEFAULT_THEME.to_string());
        ThemeStore { active, mode, lock }
    }

    /// `theme.set(name)` (`context/theme.tsx:293-298`).
    pub fn set(&mut self, kv: &mut crate::state::kv::Kv, name: &str) -> bool {
        if !has_theme(name) {
            return false;
        }
        self.active = name.to_string();
        kv.set(keys::THEME, Value::String(name.to_string()));
        true
    }

    /// `setMode` = `pin` (`context/theme.tsx:209-213`).
    pub fn set_mode(&mut self, kv: &mut crate::state::kv::Kv, mode: Mode) {
        self.lock = Some(mode);
        kv.set(
            keys::THEME_MODE_LOCK,
            Value::String(mode.name().to_string()),
        );
        self.apply(mode);
    }

    /// `unlock` = `free` (`context/theme.tsx:215-220`).
    pub fn unlock(&mut self, kv: &mut crate::state::kv::Kv) {
        self.lock = None;
        kv.set(keys::THEME_MODE_LOCK, Value::Null);
        kv.set(keys::THEME_MODE, Value::Null);
    }

    /// `apply(mode)` (`context/theme.tsx:202-207`).
    pub fn apply(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
    }

    /// The `values()` memo (`context/theme.tsx:256-267`): active theme,
    /// kv-saved fallback, then `opencode`.
    pub fn resolve(&self, kv: &crate::state::kv::Kv) -> anyhow::Result<Theme> {
        if let Some((_, raw)) = DEFAULT_THEMES
            .iter()
            .find(|(name, _)| *name == self.active.as_str())
        {
            return resolve_theme(&serde_json::from_str::<Value>(raw)?, self.mode);
        }
        if self.active == "system" {
            return resolve_theme(&system_theme(self.mode), self.mode);
        }
        if let Some(saved) = kv.get(keys::THEME, Value::Null).as_str() {
            if saved == "system" {
                return resolve_theme(&system_theme(self.mode), self.mode);
            }
            if let Some((_, raw)) = DEFAULT_THEMES.iter().find(|(name, _)| *name == saved) {
                return resolve_theme(&serde_json::from_str::<Value>(raw)?, self.mode);
            }
        }
        resolve_theme(
            &serde_json::from_str::<Value>(
                DEFAULT_THEMES
                    .iter()
                    .find(|(name, _)| *name == DEFAULT_THEME)
                    .map(|(_, raw)| *raw)
                    .unwrap(),
            )?,
            self.mode,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_every_builtin_theme() {
        assert_eq!(DEFAULT_THEMES.len(), 33);
        assert_eq!(all_themes().len(), 34);
        for (name, _) in DEFAULT_THEMES {
            let (name, raw) = DEFAULT_THEMES
                .iter()
                .find(|(candidate, _)| candidate == name)
                .unwrap();
            let value: Value = serde_json::from_str(raw).expect(name);
            assert!(is_theme(&value), "{name} is not a theme object");
            resolve_theme(&value, Mode::Dark).expect(name);
            resolve_theme(&value, Mode::Light).expect(name);
        }
        let system = system_theme(Mode::Dark);
        assert!(is_theme(&system));
        resolve_theme(&system, Mode::Dark).expect("system");
        resolve_theme(&system, Mode::Light).expect("system");
    }

    #[test]
    fn resolves_dark_and_light_variants() {
        let value: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "opencode")
                .unwrap()
                .1,
        )
        .unwrap();
        let dark = resolve_theme(&value, Mode::Dark).unwrap();
        let light = resolve_theme(&value, Mode::Light).unwrap();
        assert_eq!(dark.background, Rgba::from_hex("#0a0a0a").unwrap());
        assert_eq!(light.background, Rgba::from_hex("#ffffff").unwrap());
        assert_eq!(dark.syntax_keyword, Rgba::from_hex("#9d7cd8").unwrap());
        assert_eq!(light.syntax_keyword, Rgba::from_hex("#d68c27").unwrap());
        assert_eq!(dark.thinking_opacity, 0.6);
    }

    #[test]
    fn transparent_background_has_no_alpha() {
        let value: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "lucent-orng")
                .unwrap()
                .1,
        )
        .unwrap();
        let theme = resolve_theme(&value, Mode::Dark).unwrap();
        assert_eq!(theme.background.a, 0.0);
        assert!(theme.has_selected_list_item_text);
        // lucent-orng defines backgroundMenu explicitly (darkPanelBg).
        assert_eq!(theme.background_menu, Rgba::from_hex("#2a1a1599").unwrap());
        assert_eq!(
            theme.background_element,
            Rgba::from_values(0.0, 0.0, 0.0, 0.0)
        );
        // An explicit selectedListItemText wins (`theme/index.ts:96-99`).
        assert_eq!(
            selected_foreground(&theme, None),
            theme.selected_list_item_text
        );
        // A transparent background derives selection contrast from
        // bg/primary (`theme/index.ts:102-107`).
        let transparent = Theme {
            has_selected_list_item_text: false,
            ..theme
        };
        let expected = if transparent.primary.luminance() > 0.5 {
            Rgba::from_ints(0, 0, 0)
        } else {
            Rgba::from_ints(255, 255, 255)
        };
        assert_eq!(selected_foreground(&transparent, None), expected);
    }

    #[test]
    fn falls_back_to_background_selection_color() {
        let value: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "opencode")
                .unwrap()
                .1,
        )
        .unwrap();
        let theme = resolve_theme(&value, Mode::Dark).unwrap();
        assert!(!theme.has_selected_list_item_text);
        assert_eq!(
            selected_foreground(&theme, None),
            Rgba::from_hex("#0a0a0a").unwrap()
        );
    }

    #[test]
    fn rejects_circular_color_refs() {
        let mut value: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "opencode")
                .unwrap()
                .1,
        )
        .unwrap();
        value["defs"]["one"] = json_two();
        value["defs"]["two"] = json_one();
        value["theme"]["primary"] = json_one();
        let error = resolve_theme(&value, Mode::Dark).unwrap_err();
        assert!(
            error.to_string().contains("Circular color reference"),
            "{error}"
        );
    }

    fn json_one() -> Value {
        Value::String("two".to_string())
    }

    fn json_two() -> Value {
        Value::String("one".to_string())
    }

    #[test]
    fn rejects_missing_color_refs() {
        let mut value: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "opencode")
                .unwrap()
                .1,
        )
        .unwrap();
        value["theme"]["border"] = Value::String("missing-color".to_string());
        let error = resolve_theme(&value, Mode::Dark).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("\"missing-color\" not found in defs or theme"),
            "{error}"
        );
    }

    #[test]
    fn theme_lookup_by_ts_key() {
        let value: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "opencode")
                .unwrap()
                .1,
        )
        .unwrap();
        let theme = resolve_theme(&value, Mode::Dark).unwrap();
        assert_eq!(theme.get("textMuted"), Some(theme.text_muted));
        assert_eq!(theme.get("backgroundPanel"), Some(theme.background_panel));
        assert_eq!(theme.get("nope"), None);
    }

    #[test]
    fn tint_blends_and_rounds() {
        let base = Rgba::from_ints(0, 0, 0);
        let overlay = Rgba::from_ints(255, 255, 255);
        assert_eq!(tint(base, overlay, 0.25), Rgba::from_ints(64, 64, 64));
    }

    #[test]
    fn has_theme_checks_the_registry() {
        assert!(has_theme("opencode"));
        assert!(has_theme("one-dark"));
        assert!(has_theme("system"));
        assert!(!has_theme(""));
        assert!(!has_theme("nonexistent"));
    }

    #[test]
    fn ansi_256_colors_resolve() {
        assert_eq!(ansi_to_rgba(0), Rgba::from_hex("#000000").expect("black"));
        assert_eq!(ansi_to_rgba(16), Rgba::from_ints(0, 0, 0));
        assert_eq!(ansi_to_rgba(17), Rgba::from_ints(0, 0, 95));
        assert_eq!(ansi_to_rgba(231), Rgba::from_ints(255, 255, 255));
        assert_eq!(ansi_to_rgba(232), Rgba::from_ints(8, 8, 8));
        assert_eq!(ansi_to_rgba(255), Rgba::from_ints(238, 238, 238));
        let mut value: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "opencode")
                .unwrap()
                .1,
        )
        .unwrap();
        value["theme"]["syntaxKeyword"] = json_number(196);
        let theme = resolve_theme(&value, Mode::Dark).unwrap();
        assert_eq!(theme.syntax_keyword, ansi_to_rgba(196));
    }

    fn json_number(value: i64) -> Value {
        serde_json::json!(value)
    }

    #[test]
    fn system_theme_generates_from_default_palette() {
        let raw = system_theme(Mode::Dark);
        let theme = resolve_theme(&raw, Mode::Dark).unwrap();
        // bg defaults to ANSI black, fg to bright white-1 (#c0c0c0).
        assert_eq!(theme.background, Rgba::from_values(0.0, 0.0, 0.0, 0.0));
        assert_eq!(theme.text, Rgba::from_hex("#c0c0c0").unwrap());
        // Primary is cyan (ANSI 6, #008080), error red (#800000).
        assert_eq!(theme.primary, Rgba::from_hex("#008080").unwrap());
        assert_eq!(theme.error, Rgba::from_hex("#800000").unwrap());
        assert!(matches!(raw, serde_json::Value::Object { .. }));
    }

    #[test]
    fn store_resolves_the_system_theme() {
        let mut kv = crate::state::kv::Kv::in_memory();
        let mut store = ThemeStore::init(&mut kv, None);
        assert!(has_theme("system"));
        store.set(&mut kv, "system");
        assert_eq!(store.active, "system");
        let theme = store.resolve(&kv).unwrap();
        let system = resolve_theme(&system_theme(Mode::Dark), Mode::Dark).unwrap();
        assert_eq!(theme.background, system.background);
        assert_eq!(theme.background, Rgba::from_values(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn detected_mode_follows_colorfgbg() {
        assert_eq!(detected_mode_with(None), Mode::Dark);
        assert_eq!(detected_mode_with(Some("15;0")), Mode::Dark);
        assert_eq!(detected_mode_with(Some("0;15")), Mode::Light);
        assert_eq!(detected_mode_with(Some("15;default;0")), Mode::Dark);
        assert_eq!(detected_mode_with(Some("garbage")), Mode::Dark);
        assert_eq!(detected_mode_with(Some("15;7")), Mode::Light);
        assert_eq!(detected_mode_with(Some("15;8")), Mode::Dark);
    }

    #[test]
    fn store_resolves_active_theme_with_fallback() {
        let mut kv = crate::state::kv::Kv::in_memory();
        let mut store = ThemeStore::init(&mut kv, None);
        assert_eq!(store.active, DEFAULT_THEME);
        store.active = "nonexistent".to_string();
        let theme = store.resolve(&kv).unwrap();
        assert_eq!(theme.background, Rgba::from_hex("#0a0a0a").unwrap());

        store.set(&mut kv, "nord");
        assert!(!store.set(&mut kv, "does-not-exist"));
        assert_eq!(store.active, "nord");
        assert_eq!(
            kv.get(crate::state::kv::keys::THEME, Value::Null),
            Value::String("nord".to_string())
        );

        // unknown active falls back to the kv-saved theme
        kv.set(
            crate::state::kv::keys::THEME,
            Value::String("nord".to_string()),
        );
        store.active = "nonexistent".to_string();
        let theme = store.resolve(&kv).unwrap();
        let nord: Value = serde_json::from_str(
            DEFAULT_THEMES
                .iter()
                .find(|(name, _)| *name == "nord")
                .unwrap()
                .1,
        )
        .unwrap();
        let nord = resolve_theme(&nord, Mode::Dark).unwrap();
        assert_eq!(theme.background, nord.background);
    }

    #[test]
    fn store_mode_lock_persists_through_kv() {
        let mut kv = crate::state::kv::Kv::in_memory();
        kv.set(
            crate::state::kv::keys::THEME_MODE_LOCK,
            Value::String("light".to_string()),
        );
        let store = ThemeStore::init(&mut kv, Some("gruvbox"));
        assert_eq!(store.mode, Mode::Light);
        assert_eq!(store.lock, Some(Mode::Light));
        assert_eq!(store.active, "gruvbox");

        kv.set(crate::state::kv::keys::THEME_MODE_LOCK, Value::Null);
        let mut store = ThemeStore::init(&mut kv, None);
        assert_eq!(store.mode, detected_mode());
        store.unlock(&mut kv);
        store.set_mode(&mut kv, Mode::Light);
        assert_eq!(store.mode, Mode::Light);
        assert_eq!(store.lock, Some(Mode::Light));
    }
}
