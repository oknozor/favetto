//! Read the theme the user configured in opencode and flatten it into favetto's
//! [`Theme`].
//!
//! opencode keeps terminal preferences in a single global `cli.json`
//! (`$XDG_CONFIG_HOME/opencode/cli.json`, overridable inline with
//! `OPENCODE_CLI_CONFIG_CONTENT`):
//!
//! ```json
//! { "theme": { "name": "tokyonight", "mode": "system" } }
//! ```
//!
//! `theme.name` is a built-in name, the `system` palette, or a custom theme
//! stem; `theme.mode` is `system`, `dark`, or `light`. Custom themes are
//! `<name>.json` files under `~/.config/opencode/themes/` (global) or
//! `<project>/.opencode/themes/` (project, closer wins).
//!
//! Two theme file formats are understood:
//!
//! - **v2** (`$schema https://opencode.ai/theme.json`, has a `base` key): a
//!   complete `base` token tree plus `light`/`dark` sections that each carry a
//!   `hue` palette and optional token overrides. Modes inherit only from the
//!   file's `base`.
//! - **v1** (has a `theme` key): `{"defs": {...}, "theme": {token: {dark, light}}}`
//!   with flat token names. These are the assets opencode used to ship.
//!
//! Both are flattened onto favetto's `Theme::apply_token` vocabulary. The
//! simplifications are deliberate and documented:
//!
//! - `diff.*` has no favetto group (there is no diff view yet) and is dropped.
//! - opencode's interaction states (`$hovered`/`$focused`/`$pressed`/
//!   `$selected`/`$disabled`) collapse onto each token's `base` colour; favetto
//!   derives its own `selected`/`focused`/`disabled` states.
//! - `@dialog` and `categorical` are ignored.
//!
//! Built-in themes are vendored under `assets/opencode-themes/` (MIT, see the
//! `NOTICE` there) and embedded at compile time, so a missing asset is a build
//! error rather than a runtime surprise.

use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;
use serde_json::Value;

use super::theme::{Theme, ThemeMode};

/// The theme selection read from opencode's `cli.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpencodeThemeSelection {
    pub name: Option<String>,
    pub mode: Option<String>,
}

/// A failure to load/parse an opencode theme. Callers warn and fall back.
#[derive(Debug)]
pub enum OpencodeThemeError {
    /// No built-in and no custom file matched the requested name.
    Unknown(String),
}

impl std::fmt::Display for OpencodeThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpencodeThemeError::Unknown(name) => write!(f, "unknown opencode theme {name:?}"),
        }
    }
}

impl std::error::Error for OpencodeThemeError {}

/// Vendored built-in themes: `(name, source)`. `opencode`/`opencode2` are the
/// v2 assets; the rest are the v1 assets opencode still ships.
const BUILTIN_THEMES: &[(&str, &str)] = &[
    (
        "opencode",
        include_str!("../../assets/opencode-themes/v2/opencode.json"),
    ),
    (
        "opencode2",
        include_str!("../../assets/opencode-themes/v2/opencode2.json"),
    ),
    (
        "aura",
        include_str!("../../assets/opencode-themes/aura.json"),
    ),
    ("ayu", include_str!("../../assets/opencode-themes/ayu.json")),
    (
        "carbonfox",
        include_str!("../../assets/opencode-themes/carbonfox.json"),
    ),
    (
        "catppuccin",
        include_str!("../../assets/opencode-themes/catppuccin.json"),
    ),
    (
        "catppuccin-frappe",
        include_str!("../../assets/opencode-themes/catppuccin-frappe.json"),
    ),
    (
        "catppuccin-macchiato",
        include_str!("../../assets/opencode-themes/catppuccin-macchiato.json"),
    ),
    (
        "cobalt2",
        include_str!("../../assets/opencode-themes/cobalt2.json"),
    ),
    (
        "cursor",
        include_str!("../../assets/opencode-themes/cursor.json"),
    ),
    (
        "dracula",
        include_str!("../../assets/opencode-themes/dracula.json"),
    ),
    (
        "everforest",
        include_str!("../../assets/opencode-themes/everforest.json"),
    ),
    (
        "flexoki",
        include_str!("../../assets/opencode-themes/flexoki.json"),
    ),
    (
        "github",
        include_str!("../../assets/opencode-themes/github.json"),
    ),
    (
        "gruvbox",
        include_str!("../../assets/opencode-themes/gruvbox.json"),
    ),
    (
        "kanagawa",
        include_str!("../../assets/opencode-themes/kanagawa.json"),
    ),
    (
        "lucent-orng",
        include_str!("../../assets/opencode-themes/lucent-orng.json"),
    ),
    (
        "material",
        include_str!("../../assets/opencode-themes/material.json"),
    ),
    (
        "matrix",
        include_str!("../../assets/opencode-themes/matrix.json"),
    ),
    (
        "mercury",
        include_str!("../../assets/opencode-themes/mercury.json"),
    ),
    (
        "monokai",
        include_str!("../../assets/opencode-themes/monokai.json"),
    ),
    (
        "nightowl",
        include_str!("../../assets/opencode-themes/nightowl.json"),
    ),
    (
        "nord",
        include_str!("../../assets/opencode-themes/nord.json"),
    ),
    (
        "one-dark",
        include_str!("../../assets/opencode-themes/one-dark.json"),
    ),
    (
        "orng",
        include_str!("../../assets/opencode-themes/orng.json"),
    ),
    (
        "osaka-jade",
        include_str!("../../assets/opencode-themes/osaka-jade.json"),
    ),
    (
        "palenight",
        include_str!("../../assets/opencode-themes/palenight.json"),
    ),
    (
        "rosepine",
        include_str!("../../assets/opencode-themes/rosepine.json"),
    ),
    (
        "solarized",
        include_str!("../../assets/opencode-themes/solarized.json"),
    ),
    (
        "synthwave84",
        include_str!("../../assets/opencode-themes/synthwave84.json"),
    ),
    (
        "tokyonight",
        include_str!("../../assets/opencode-themes/tokyonight.json"),
    ),
    (
        "vercel",
        include_str!("../../assets/opencode-themes/vercel.json"),
    ),
    (
        "vesper",
        include_str!("../../assets/opencode-themes/vesper.json"),
    ),
    (
        "zenburn",
        include_str!("../../assets/opencode-themes/zenburn.json"),
    ),
];

/// The vendored built-in theme names, sorted.
pub fn builtin_names() -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_THEMES
        .iter()
        .map(|(n, _)| (*n).to_string())
        .collect();
    names.sort();
    names
}

/// `[<cwd>/.opencode/themes, <config_dir>/opencode/themes]` (project first, so
/// it wins). Missing directories are harmless.
pub fn theme_directories(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![cwd.join(".opencode").join("themes")];
    if let Some(config) = dirs_config_dir() {
        dirs.push(config.join("opencode").join("themes"));
    }
    dirs
}

/// Names opencode knows: vendored built-ins plus discovered custom stems.
pub fn known_theme_names(dirs: &[PathBuf]) -> Vec<String> {
    let mut names = builtin_names();
    names.push("system".to_string());
    for name in discover_custom_themes(dirs) {
        if !names.iter().any(|n| n == &name) {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// Discover `*.json` custom theme stems across `dirs` (best effort, sorted).
fn discover_custom_themes(dirs: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if !names.iter().any(|n| n == stem) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// `$XDG_CONFIG_HOME` or the platform config dir (`~/.config`).
fn dirs_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    dirs::config_dir()
}

/// Parse the `cli.json` body and an optional inline override, extracting only
/// the theme selection. Malformed input yields all-`None`; this never panics.
pub fn parse_cli_config(file: Option<&str>, inline: Option<&str>) -> OpencodeThemeSelection {
    let mut selection = OpencodeThemeSelection::default();
    if let Some(source) = file {
        merge_selection(&mut selection, source);
    }
    if let Some(source) = inline {
        merge_selection(&mut selection, source);
    }
    selection
}

fn merge_selection(selection: &mut OpencodeThemeSelection, source: &str) {
    #[derive(Deserialize)]
    struct CliConfig {
        theme: Option<ThemeSelection>,
    }
    #[derive(Deserialize)]
    struct ThemeSelection {
        name: Option<String>,
        mode: Option<String>,
    }
    let Ok(config) = serde_json::from_str::<CliConfig>(source) else {
        return;
    };
    if let Some(theme) = config.theme {
        if let Some(name) = theme.name {
            if !name.trim().is_empty() {
                selection.name = Some(name);
            }
        }
        if let Some(mode) = theme.mode {
            if !mode.trim().is_empty() {
                selection.mode = Some(mode);
            }
        }
    }
}

/// Read `cli.json` from the opencode config dir and merge
/// `OPENCODE_CLI_CONFIG_CONTENT` over it.
pub fn load_cli_config() -> OpencodeThemeSelection {
    let file = dirs_config_dir()
        .map(|dir| dir.join("opencode").join("cli.json"))
        .and_then(|path| std::fs::read_to_string(path).ok());
    let inline = std::env::var("OPENCODE_CLI_CONFIG_CONTENT").ok();
    parse_cli_config(file.as_deref(), inline.as_deref())
}

/// Resolve `name` to a favetto [`Theme`] for a concrete `mode`, searching
/// custom files (project then global) before the vendored built-ins.
pub fn resolve(name: &str, mode: ThemeMode, dirs: &[PathBuf]) -> Result<Theme, OpencodeThemeError> {
    let name = name.trim();
    if name.eq_ignore_ascii_case("system") {
        return Ok(theme_from_source(
            builtin_source("opencode").expect("opencode built-in"),
            mode,
        ));
    }
    if let Some(source) = load_custom_source(name, dirs) {
        return Ok(theme_from_source(&source, mode));
    }
    match builtin_source(name) {
        Some(source) => Ok(theme_from_source(source, mode)),
        None => Err(OpencodeThemeError::Unknown(name.to_string())),
    }
}

fn builtin_source(name: &str) -> Option<&'static str> {
    BUILTIN_THEMES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, source)| *source)
}

fn load_custom_source(name: &str, dirs: &[PathBuf]) -> Option<String> {
    for dir in dirs {
        let path = dir.join(format!("{name}.json"));
        if let Ok(source) = std::fs::read_to_string(&path) {
            return Some(source);
        }
    }
    None
}

/// Build a [`Theme`] from a theme-document source, falling back to the bundled
/// opencode palette for the mode when parsing fails.
fn theme_from_source(source: &str, mode: ThemeMode) -> Theme {
    let resolved = resolve_mode(mode);
    let mut theme = match resolved {
        ThemeMode::Light => Theme::opencode_light(),
        _ => Theme::opencode_dark(),
    };
    let Ok(document) = serde_json::from_str::<Value>(source) else {
        tracing::warn!("opencode theme is not valid JSON; using the bundled palette");
        return theme;
    };
    let tokens = if document.get("base").is_some() {
        flatten_v2(&document, resolved)
    } else {
        flatten_v1(&document, resolved)
    };
    for (path, color) in tokens {
        theme.apply_token(&path, color);
    }
    theme
}

/// Resolve `system` to dark at this layer; callers that detect the terminal
/// mode pass an already-concrete `ThemeMode`.
fn resolve_mode(mode: ThemeMode) -> ThemeMode {
    match mode {
        ThemeMode::Light => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

// ---------------------------------------------------------------------------
// v1: `{defs: {name: colour}, theme: {token: colour | {dark, light}}}`
// ---------------------------------------------------------------------------

fn flatten_v1(document: &Value, mode: ThemeMode) -> Vec<(String, Color)> {
    let defs = document.get("defs").and_then(Value::as_object);
    let Some(theme) = document.get("theme").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (token, value) in theme {
        let Some(raw) = pick_mode_value(value, mode) else {
            continue;
        };
        let Some(color) = resolve_v1_color(raw, defs, 0) else {
            continue;
        };
        for path in v1_token_paths(token) {
            out.push((path.to_string(), color));
        }
    }
    out
}

fn pick_mode_value(value: &Value, mode: ThemeMode) -> Option<&Value> {
    if value.is_string() {
        return Some(value);
    }
    let object = value.as_object()?;
    let primary = match mode {
        ThemeMode::Light => "light",
        _ => "dark",
    };
    let secondary = if primary == "dark" { "light" } else { "dark" };
    object.get(primary).or_else(|| object.get(secondary))
}

fn resolve_v1_color(
    value: &Value,
    defs: Option<&serde_json::Map<String, Value>>,
    depth: usize,
) -> Option<Color> {
    let raw = value.as_str()?.trim();
    if depth > 16 {
        return None;
    }
    if raw.eq_ignore_ascii_case("transparent") {
        return Some(Color::Reset);
    }
    if raw.starts_with('#') {
        return parse_hex_color(raw);
    }
    // Otherwise `raw` names a `defs` entry (which may itself be a reference).
    let def = defs?.get(raw)?;
    resolve_v1_color(def, defs, depth + 1)
}

/// Map a flat v1 token name to one or more favetto token paths.
fn v1_token_paths(token: &str) -> &'static [&'static str] {
    match token {
        "primary" => &["text.action.primary"],
        "secondary" => &["text.action.secondary"],
        "accent" => &["text.action.primary"],
        "error" => &["text.feedback.error", "background.feedback.error"],
        "warning" => &["text.feedback.warning", "background.feedback.warning"],
        "success" => &["text.feedback.success", "background.feedback.success"],
        "info" => &["text.feedback.info", "background.feedback.info"],
        "text" => &["text.base"],
        "textMuted" => &["text.muted"],
        "background" => &["background.base"],
        "backgroundPanel" => &["background.raised.base"],
        "backgroundElement" => &["background.raised.high"],
        "backgroundMenu" => &["background.raised.max"],
        "border" => &["border.base"],
        "borderActive" => &["border.active"],
        "borderSubtle" => &["border.subtle"],
        "markdownText" => &["markdown.text"],
        "markdownHeading" => &["markdown.heading"],
        "markdownLink" => &["markdown.link"],
        "markdownLinkText" => &["markdown.link_text"],
        "markdownCode" => &["markdown.code"],
        "markdownBlockQuote" => &["markdown.block_quote"],
        "markdownEmph" => &["markdown.emphasis"],
        "markdownStrong" => &["markdown.strong"],
        "markdownHorizontalRule" => &["markdown.horizontal_rule"],
        "markdownListItem" => &["markdown.list_item"],
        "markdownListEnumeration" => &["markdown.list_enumeration"],
        "markdownImage" => &["markdown.image"],
        "markdownImageText" => &["markdown.image_text"],
        "markdownCodeBlock" => &["markdown.code_block"],
        "syntaxComment" => &["syntax.comment"],
        "syntaxKeyword" => &["syntax.keyword"],
        "syntaxFunction" => &["syntax.function"],
        "syntaxVariable" => &["syntax.variable"],
        "syntaxString" => &["syntax.string"],
        "syntaxNumber" => &["syntax.number"],
        "syntaxType" => &["syntax.type"],
        "syntaxOperator" => &["syntax.operator"],
        "syntaxPunctuation" => &["syntax.punctuation"],
        // `diff*`, `selectedListItemText`, `thinkingOpacity`, … are dropped.
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// v2: `{base: token tree, light/dark: {hue, overrides}}`
// ---------------------------------------------------------------------------

fn flatten_v2(document: &Value, mode: ThemeMode) -> Vec<(String, Color)> {
    let base = document.get("base").cloned().unwrap_or(Value::Null);
    let section = pick_mode_section(document, mode);
    let hue = section
        .and_then(|s| s.get("hue"))
        .cloned()
        .unwrap_or(Value::Null);
    let overrides = section.map(|s| {
        let mut clone = s.clone();
        if let Some(object) = clone.as_object_mut() {
            object.remove("hue");
        }
        clone
    });
    let mut merged = base;
    if let Some(overrides) = overrides {
        deep_merge(&mut merged, &overrides);
    }

    let mut leaves = Vec::new();
    let mut path = Vec::new();
    collect_leaves(&merged, &mut path, &mut leaves);

    let mut out = Vec::new();
    for (path, value) in leaves {
        let Some(color) = resolve_v2_value(value, &merged, &hue, 0) else {
            continue;
        };
        if let Some(mapped) = v2_token_path(&path) {
            out.push((mapped.to_string(), color));
        }
    }
    out
}

fn pick_mode_section(document: &Value, mode: ThemeMode) -> Option<&Value> {
    let primary = match mode {
        ThemeMode::Light => "light",
        _ => "dark",
    };
    let secondary = if primary == "dark" { "light" } else { "dark" };
    document.get(primary).or_else(|| document.get(secondary))
}

fn deep_merge(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (slot, value) => *slot = value.clone(),
    }
}

/// Collect every leaf string in `value` as `(dotted path, value)`, skipping
/// `$`-prefixed interaction-state keys.
fn collect_leaves<'a>(
    value: &'a Value,
    path: &mut Vec<String>,
    out: &mut Vec<(String, &'a Value)>,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key.starts_with('$') {
                    continue;
                }
                path.push(key.clone());
                collect_leaves(child, path, out);
                path.pop();
            }
        }
        _ => out.push((path.join("."), value)),
    }
}

fn resolve_v2_value(value: &Value, root: &Value, hue: &Value, depth: usize) -> Option<Color> {
    if depth > 32 {
        return None;
    }
    let raw = match value {
        Value::String(raw) => raw.trim(),
        // An object (e.g. an unresolved group) has no concrete `base`; take it.
        Value::Object(object) => {
            let base = object.get("base")?;
            return resolve_v2_value(base, root, hue, depth + 1);
        }
        _ => return None,
    };
    if raw.eq_ignore_ascii_case("transparent") {
        return Some(Color::Reset);
    }
    if raw.starts_with('#') {
        return parse_hex_color(raw);
    }
    if let Some(reference) = raw.strip_prefix("$hue.") {
        return resolve_hue(reference, hue, 0);
    }
    if let Some(path) = raw.strip_prefix('$') {
        let target = lookup_path(root, path)?;
        return resolve_v2_value(target, root, hue, depth + 1);
    }
    None
}

fn resolve_hue(reference: &str, hue: &Value, depth: usize) -> Option<Color> {
    if depth > 16 {
        return None;
    }
    let mut parts = reference.split('.');
    let name = parts.next()?;
    let step = parts.next();
    let entry = hue.get(name)?;
    match step {
        Some(step) => {
            if let Value::String(alias) = entry {
                if let Some(alias) = alias.strip_prefix("$hue.") {
                    let mut alias_parts = alias.split('.');
                    let target = alias_parts.next()?;
                    let nested_step = alias_parts.next().unwrap_or(step);
                    return resolve_hue(&format!("{target}.{nested_step}"), hue, depth + 1);
                }
            }
            let color = entry.get(step)?;
            resolve_v2_value(color, hue, hue, depth + 1)
        }
        None => {
            // A bare alias such as `$hue.accent`; follow it to a step-less hue.
            let alias = entry.as_str()?;
            let target = alias.strip_prefix("$hue.")?;
            resolve_hue(target, hue, depth + 1)
        }
    }
}

fn lookup_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Map a v2 token path to a favetto token path.
fn v2_token_path(path: &str) -> Option<&'static str> {
    let path = match path {
        "text.base" => "text.base",
        "text.muted" => "text.muted",
        "text.faint" => "text.muted",
        "text.action.primary.base" => "text.action.primary",
        "text.action.secondary.base" => "text.action.secondary",
        "text.action.destructive.base" => "text.action.destructive",
        "text.formfield.base" => "text.formfield",
        "text.feedback.error.base" => "text.feedback.error",
        "text.feedback.warning.base" => "text.feedback.warning",
        "text.feedback.success.base" => "text.feedback.success",
        "text.feedback.info.base" => "text.feedback.info",
        "background.base" => "background.base",
        "background.raised.base" => "background.raised.base",
        "background.raised.high" => "background.raised.high",
        "background.raised.max" => "background.raised.max",
        "background.action.primary.base" => "background.action",
        "background.formfield.base" => "background.formfield",
        "background.feedback.error.base" => "background.feedback.error",
        "background.feedback.warning.base" => "background.feedback.warning",
        "background.feedback.success.base" => "background.feedback.success",
        "background.feedback.info.base" => "background.feedback.info",
        "border.base" => "border.base",
        "border.active" => "border.active",
        "border.subtle" => "border.subtle",
        "scrollbar.base" => "scrollbar.base",
        "syntax.comment" => "syntax.comment",
        "syntax.keyword" => "syntax.keyword",
        "syntax.function" => "syntax.function",
        "syntax.variable" => "syntax.variable",
        "syntax.string" => "syntax.string",
        "syntax.number" => "syntax.number",
        "syntax.type" => "syntax.type",
        "syntax.operator" => "syntax.operator",
        "syntax.punctuation" => "syntax.punctuation",
        "markdown.text" => "markdown.text",
        "markdown.heading" => "markdown.heading",
        "markdown.link" => "markdown.link",
        "markdown.linkText" => "markdown.link_text",
        "markdown.code" => "markdown.code",
        "markdown.blockQuote" => "markdown.block_quote",
        "markdown.emphasis" => "markdown.emphasis",
        "markdown.strong" => "markdown.strong",
        "markdown.horizontalRule" => "markdown.horizontal_rule",
        "markdown.listItem" => "markdown.list_item",
        "markdown.listEnumeration" => "markdown.list_enumeration",
        "markdown.image" => "markdown.image",
        "markdown.imageText" => "markdown.image_text",
        "markdown.codeBlock" => "markdown.code_block",
        _ => return None,
    };
    Some(path)
}

/// Parse `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` (alpha is ignored).
pub fn parse_hex_color(value: &str) -> Option<Color> {
    let hex = value.trim().strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (r, g, b) = match hex.len() {
        3 => {
            let r = nibble(&hex[0..1])?;
            let g = nibble(&hex[1..2])?;
            let b = nibble(&hex[2..3])?;
            (r * 17, g * 17, b * 17)
        }
        4 => {
            let r = nibble(&hex[0..1])? * 17;
            let g = nibble(&hex[1..2])? * 17;
            let b = nibble(&hex[2..3])? * 17;
            (r, g, b)
        }
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ),
        8 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ),
        _ => return None,
    };
    Some(Color::Rgb(r, g, b))
}

fn nibble(s: &str) -> Option<u8> {
    u8::from_str_radix(s, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2_MINIMAL: &str = r##"{
        "base": {
            "text": {
                "base": "$hue.neutral.200",
                "muted": "$hue.neutral.400",
                "action": {
                    "primary": { "base": "$hue.interactive.200", "$hovered": "$hue.neutral.700" }
                }
            },
            "background": {
                "base": "$hue.neutral.800",
                "raised": { "base": "$hue.neutral.700", "high": "$hue.neutral.600", "max": "$hue.neutral.500" },
                "action": { "primary": { "base": "transparent" } },
                "formfield": { "base": "$background.base" },
                "feedback": { "error": { "base": "$hue.red.200" } }
            },
            "border": { "base": "#484848" },
            "scrollbar": { "base": "#606060" },
            "syntax": { "comment": "$hue.neutral.400", "keyword": "$hue.accent.200" },
            "markdown": { "heading": "$hue.accent.200", "link": "$text.base" },
            "diff": { "text": { "added": "#4fd6be" } },
            "@dialog": { "background": { "base": "$background.raised.base" } }
        },
        "light": {
            "hue": {
                "gray": { "200": "#1a1a1a", "400": "#8a8a8a", "500": "#bebebe", "600": "#f5f5f5", "700": "#fafafa", "800": "#ffffff" },
                "blue": { "200": "#3b7dd8" },
                "orange": { "200": "#d68c27" },
                "red": { "200": "#d1383d" },
                "accent": "$hue.orange",
                "interactive": "$hue.blue",
                "neutral": "$hue.gray"
            }
        },
        "dark": {
            "hue": {
                "gray": { "200": "#eeeeee", "400": "#808080", "500": "#4c4c4c", "600": "#1e1e1e", "700": "#141414", "800": "#0a0a0a" },
                "blue": { "200": "#5c9cf5" },
                "orange": { "200": "#fab283" },
                "red": { "200": "#e06c75" },
                "accent": "$hue.orange",
                "interactive": "$hue.blue",
                "neutral": "$hue.gray"
            }
        }
    }"##;

    const V1_MINIMAL: &str = r##"{
        "defs": { "bg": "#101418", "fg": "#e6e6e6", "blue": "#5c9cf5" },
        "theme": {
            "primary": { "dark": "blue", "light": "#3b7dd8" },
            "text": { "dark": "fg", "light": "#2a2a2a" },
            "background": { "dark": "bg", "light": "#ffffff" },
            "markdownHeading": { "dark": "#fab283", "light": "#3b7dd8" }
        }
    }"##;

    #[test]
    fn parse_cli_config_reads_theme_name_and_mode() {
        let selection = parse_cli_config(
            Some(r##"{"theme":{"name":"tokyonight","mode":"dark"}}"##),
            None,
        );
        assert_eq!(selection.name.as_deref(), Some("tokyonight"));
        assert_eq!(selection.mode.as_deref(), Some("dark"));
    }

    #[test]
    fn inline_cli_config_overrides_the_file() {
        let selection = parse_cli_config(
            Some(r##"{"theme":{"name":"nord","mode":"dark"}}"##),
            Some(r##"{"theme":{"name":"synthwave84"}}"##),
        );
        assert_eq!(selection.name.as_deref(), Some("synthwave84"));
        assert_eq!(selection.mode.as_deref(), Some("dark"));
    }

    #[test]
    fn malformed_cli_config_is_ignored() {
        let selection = parse_cli_config(Some("not json"), Some("{broken"));
        assert_eq!(selection, OpencodeThemeSelection::default());
    }

    #[test]
    fn hex_colours_parse_with_several_lengths() {
        assert_eq!(parse_hex_color("#fff"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_hex_color("#f00"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(
            parse_hex_color("#AABBCC"),
            Some(Color::Rgb(0xaa, 0xbb, 0xcc))
        );
        assert_eq!(
            parse_hex_color("#AABBCCDD"),
            Some(Color::Rgb(0xaa, 0xbb, 0xcc))
        );
        assert_eq!(parse_hex_color("#12345"), None);
        assert_eq!(parse_hex_color("not-a-colour"), None);
    }

    #[test]
    fn resolves_v2_hue_and_token_references() {
        let theme = theme_from_source(V2_MINIMAL, ThemeMode::Dark);
        assert_eq!(theme.background.base, Color::Rgb(0x0a, 0x0a, 0x0a));
        assert_eq!(theme.background.raised.base, Color::Rgb(0x14, 0x14, 0x14));
        assert_eq!(theme.text.base, Color::Rgb(0xee, 0xee, 0xee));
        assert_eq!(theme.text.action.primary, Color::Rgb(0x5c, 0x9c, 0xf5));
        // A `$token` reference (`$text.base`) resolves through the merged tree.
        assert_eq!(theme.markdown.link, Color::Rgb(0xee, 0xee, 0xee));
        // `$background.base` reference for the form-field fill.
        assert_eq!(theme.background.formfield, Color::Rgb(0x0a, 0x0a, 0x0a));
        // `transparent` maps to reset.
        assert_eq!(theme.background.action, Color::Reset);
        // A hue alias (`accent` -> `orange`).
        assert_eq!(theme.syntax.keyword, Color::Rgb(0xfa, 0xb2, 0x83));
        // `diff.*` has no favetto group; it is silently dropped.
        // The light section resolves independently.
        let light = theme_from_source(V2_MINIMAL, ThemeMode::Light);
        assert_eq!(light.background.base, Color::Rgb(0xff, 0xff, 0xff));
        assert_eq!(light.text.base, Color::Rgb(0x1a, 0x1a, 0x1a));
        assert_ne!(light, theme);
    }

    #[test]
    fn resolves_v1_defs_and_modes() {
        let dark = theme_from_source(V1_MINIMAL, ThemeMode::Dark);
        assert_eq!(dark.background.base, Color::Rgb(0x10, 0x14, 0x18));
        assert_eq!(dark.text.base, Color::Rgb(0xe6, 0xe6, 0xe6));
        assert_eq!(dark.text.action.primary, Color::Rgb(0x5c, 0x9c, 0xf5));
        assert_eq!(dark.markdown.heading, Color::Rgb(0xfa, 0xb2, 0x83));

        let light = theme_from_source(V1_MINIMAL, ThemeMode::Light);
        assert_eq!(light.background.base, Color::Rgb(0xff, 0xff, 0xff));
        assert_eq!(light.text.base, Color::Rgb(0x2a, 0x2a, 0x2a));
        assert_eq!(light.text.action.primary, Color::Rgb(0x3b, 0x7d, 0xd8));
        assert_ne!(light, dark);
    }

    #[test]
    fn v1_single_mode_falls_back_to_the_other() {
        let source = r##"{
            "theme": { "text": { "dark": "#abcdef" } }
        }"##;
        // Requesting light from a dark-only token uses the dark value.
        let theme = theme_from_source(source, ThemeMode::Light);
        assert_eq!(theme.text.base, Color::Rgb(0xab, 0xcd, 0xef));
    }

    #[test]
    fn every_vendored_builtin_resolves() {
        for name in builtin_names() {
            let theme = resolve(&name, ThemeMode::Dark, &[]).expect("built-in resolves");
            // Each built-in overrides at least one token away from the bundled
            // default (some, like `lucent-orng`, deliberately clear the
            // background to `transparent`).
            assert_ne!(
                theme,
                Theme::opencode_dark(),
                "built-in {name} parsed to the bundled default"
            );
        }
    }

    #[test]
    fn unknown_theme_is_an_error() {
        let err = resolve("does-not-exist", ThemeMode::Dark, &[]).unwrap_err();
        assert!(matches!(err, OpencodeThemeError::Unknown(_)));
    }

    #[test]
    fn custom_theme_directory_wins_over_builtin() {
        let dir = std::env::temp_dir().join(format!("favetto-oc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.json"), V2_MINIMAL).unwrap();
        let theme = resolve("opencode", ThemeMode::Dark, std::slice::from_ref(&dir)).unwrap();
        // The custom file's background differs from the vendored built-in.
        assert_eq!(theme.background.base, Color::Rgb(0x0a, 0x0a, 0x0a));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_directory_wins_over_global() {
        let base = std::env::temp_dir().join(format!("favetto-oc-proj-{}", std::process::id()));
        let project = base.join("project");
        let global = base.join("global");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(project.join("mine.json"), V2_MINIMAL).unwrap();
        let global_only = resolve("mine", ThemeMode::Dark, std::slice::from_ref(&global));
        assert!(global_only.is_err());
        let dirs = vec![project, global];
        let theme = resolve("mine", ThemeMode::Dark, &dirs).unwrap();
        assert_eq!(theme.background.base, Color::Rgb(0x0a, 0x0a, 0x0a));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn known_names_include_builtins_and_system() {
        let names = known_theme_names(&[]);
        assert!(names.iter().any(|n| n == "opencode"));
        assert!(names.iter().any(|n| n == "synthwave84"));
        assert!(names.iter().any(|n| n == "system"));
    }
}
