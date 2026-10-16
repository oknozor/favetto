//! The TUI's central colour palette.
//!
//! A single [`Theme`] is resolved once at startup and every widget paints
//! through it. The model mirrors opencode's token groups: nested, semantic
//! [`Theme`] tokens (`background`, `text`, `border`, `syntax`, `markdown`, …)
//! instead of a flat list of ad-hoc roles. `Theme` is `Copy`, so the `draw_*`
//! helpers can take it by value without fighting the borrow checker.
//!
//! Themes come in built-in flavours ([`ThemeName::Opencode`],
//! [`ThemeName::Catppuccin`]) selected by a [`ThemeMode`] (`system`/`dark`/`light`)
//! and may be overridden by a user JSON file in the themes directory. Interactive
//! state helpers ([`Theme::selected`], [`Theme::disabled`], …)
//! and raised-surface levels ([`RaisedLevel`]) keep the widgets from hand-rolling
//! styles.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

use favetto_core::model::TaskStatus;

/// Semantic colour palette for the whole TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: Background,
    pub text: Text,
    pub border: Border,
    pub scrollbar: Scrollbar,
    pub syntax: Syntax,
    pub markdown: Markdown,
}

/// Background surfaces, split into the base fill and the raised depth levels
/// used by cards (`base`), panels (`high`) and modal dialogs (`max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Background {
    pub base: Color,
    pub raised: Raised,
    pub action: Color,
    pub formfield: Color,
    pub feedback: Feedback,
}

/// The raised-surface depth ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raised {
    pub base: Color,
    pub high: Color,
    pub max: Color,
}

/// Foreground text roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Text {
    pub base: Color,
    pub muted: Color,
    pub action: Action,
    pub formfield: Color,
    pub feedback: Feedback,
}

/// Action colours (buttons, links, destructive actions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    pub primary: Color,
    pub secondary: Color,
    pub destructive: Color,
}

/// Status feedback colours, shared by the matching background and text groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feedback {
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
}

/// Border roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Border {
    pub base: Color,
    pub active: Color,
    pub subtle: Color,
}

/// Scrollbar role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollbar {
    pub base: Color,
}

/// Syntax-highlighting roles (Events payload, Catalog preview, dialog code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syntax {
    pub comment: Color,
    pub keyword: Color,
    pub function: Color,
    pub variable: Color,
    pub string: Color,
    pub number: Color,
    pub type_: Color,
    pub operator: Color,
    pub punctuation: Color,
}

/// Markdown-rendering roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Markdown {
    pub text: Color,
    pub heading: Color,
    pub link: Color,
    pub link_text: Color,
    pub code: Color,
    pub block_quote: Color,
    pub emphasis: Color,
    pub strong: Color,
    pub horizontal_rule: Color,
    pub list_item: Color,
    pub list_enumeration: Color,
    pub image: Color,
    pub image_text: Color,
    pub code_block: Color,
}

/// A raised-surface depth. Chrome and every floating window use
/// [`RaisedLevel::Base`]. The heavier rungs stay part of the raised-surface
/// schema (and its JSON) so the ladder remains complete, but no widget paints
/// them any more.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaisedLevel {
    Base,
    High,
    Max,
}

/// A bundled theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Opencode,
    Catppuccin,
}

impl ThemeName {
    pub const ALL: [ThemeName; 2] = [ThemeName::Opencode, ThemeName::Catppuccin];

    pub fn key(self) -> &'static str {
        match self {
            ThemeName::Opencode => "opencode",
            ThemeName::Catppuccin => "catppuccin",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Opencode => "Opencode",
            ThemeName::Catppuccin => "Catppuccin",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "opencode" => Some(ThemeName::Opencode),
            "catppuccin" => Some(ThemeName::Catppuccin),
            _ => None,
        }
    }
}

/// Light/dark selection, or `system` to derive it from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    System,
    Dark,
    Light,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Dark, ThemeMode::Light];

    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "System",
            ThemeMode::Dark => "Dark",
            ThemeMode::Light => "Light",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(ThemeMode::System),
            "dark" => Some(ThemeMode::Dark),
            "light" => Some(ThemeMode::Light),
            _ => None,
        }
    }
}

/// Build a [`Color::Rgb`] from a `0xRRGGBB` literal.
const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        (hex >> 16) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

impl Theme {
    /// The default dark palette (opencode-inspired).
    pub const fn dark() -> Self {
        Self::opencode_dark()
    }

    /// The default light palette (opencode-inspired).
    pub const fn light() -> Self {
        Self::opencode_light()
    }

    /// Opencode-inspired dark theme.
    pub const fn opencode_dark() -> Self {
        Self {
            background: Background {
                base: rgb(0x212121),
                raised: Raised {
                    base: rgb(0x2a2a2a),
                    high: rgb(0x333333),
                    max: rgb(0x3d3d3d),
                },
                action: rgb(0xfab283),
                formfield: rgb(0x2f2f2f),
                feedback: Feedback {
                    error: rgb(0xe06c75),
                    warning: rgb(0xe5c07b),
                    success: rgb(0x7fd88f),
                    info: rgb(0x56b6c2),
                },
            },
            text: Text {
                base: rgb(0xe6e6e6),
                muted: rgb(0x8a8a8a),
                action: Action {
                    primary: rgb(0xfab283),
                    secondary: rgb(0x5c9cf5),
                    destructive: rgb(0xe06c75),
                },
                formfield: rgb(0xe6e6e6),
                feedback: Feedback {
                    error: rgb(0xe06c75),
                    warning: rgb(0xe5c07b),
                    success: rgb(0x7fd88f),
                    info: rgb(0x56b6c2),
                },
            },
            border: Border {
                base: rgb(0x4b4c5c),
                active: rgb(0xfab283),
                subtle: rgb(0x33333d),
            },
            scrollbar: Scrollbar {
                base: rgb(0x4b4c5c),
            },
            syntax: Syntax {
                comment: rgb(0x6c6c6c),
                keyword: rgb(0x9d7cd8),
                function: rgb(0x5c9cf5),
                variable: rgb(0xe6e6e6),
                string: rgb(0x7fd88f),
                number: rgb(0xf5a742),
                type_: rgb(0x56b6c2),
                operator: rgb(0xfab283),
                punctuation: rgb(0xa0a0a0),
            },
            markdown: Markdown {
                text: rgb(0xe6e6e6),
                heading: rgb(0xfab283),
                link: rgb(0x5c9cf5),
                link_text: rgb(0x5c9cf5),
                code: rgb(0xf5a742),
                block_quote: rgb(0x8a8a8a),
                emphasis: rgb(0xe5c07b),
                strong: rgb(0xe6e6e6),
                horizontal_rule: rgb(0x6c6c6c),
                list_item: rgb(0xfab283),
                list_enumeration: rgb(0xfab283),
                image: rgb(0x9d7cd8),
                image_text: rgb(0x9d7cd8),
                code_block: rgb(0xf5a742),
            },
        }
    }

    /// Opencode-inspired light theme.
    pub const fn opencode_light() -> Self {
        Self {
            background: Background {
                base: rgb(0xf8f8f8),
                raised: Raised {
                    base: rgb(0xffffff),
                    high: rgb(0xf0f0f0),
                    max: rgb(0xe8e8e8),
                },
                action: rgb(0x3b7dd8),
                formfield: rgb(0xf0f0f0),
                feedback: Feedback {
                    error: rgb(0xc62828),
                    warning: rgb(0xb26a00),
                    success: rgb(0x2e7d32),
                    info: rgb(0x0277bd),
                },
            },
            text: Text {
                base: rgb(0x2a2a2a),
                muted: rgb(0x8a8f98),
                action: Action {
                    primary: rgb(0x3b7dd8),
                    secondary: rgb(0x7b5bb6),
                    destructive: rgb(0xc62828),
                },
                formfield: rgb(0x2a2a2a),
                feedback: Feedback {
                    error: rgb(0xc62828),
                    warning: rgb(0xb26a00),
                    success: rgb(0x2e7d32),
                    info: rgb(0x0277bd),
                },
            },
            border: Border {
                base: rgb(0xc8ccd4),
                active: rgb(0x3b7dd8),
                subtle: rgb(0xe2e2e6),
            },
            scrollbar: Scrollbar {
                base: rgb(0xc8ccd4),
            },
            syntax: Syntax {
                comment: rgb(0x8a8f98),
                keyword: rgb(0x7b2ff2),
                function: rgb(0x1e66f5),
                variable: rgb(0x2a2a2a),
                string: rgb(0x2e7d32),
                number: rgb(0xb26a00),
                type_: rgb(0x0277bd),
                operator: rgb(0xd68c27),
                punctuation: rgb(0x5a5f68),
            },
            markdown: Markdown {
                text: rgb(0x2a2a2a),
                heading: rgb(0x3b7dd8),
                link: rgb(0x0277bd),
                link_text: rgb(0x0277bd),
                code: rgb(0xb26a00),
                block_quote: rgb(0x8a8f98),
                emphasis: rgb(0x7b5bb6),
                strong: rgb(0x2a2a2a),
                horizontal_rule: rgb(0x8a8f98),
                list_item: rgb(0x3b7dd8),
                list_enumeration: rgb(0x3b7dd8),
                image: rgb(0x7b5bb6),
                image_text: rgb(0x7b5bb6),
                code_block: rgb(0xb26a00),
            },
        }
    }

    /// The historical Catppuccin-Mocha-ish dark palette.
    pub const fn catppuccin_dark() -> Self {
        Self {
            background: Background {
                base: rgb(0x1e1e2e),
                raised: Raised {
                    base: rgb(0x252537),
                    high: rgb(0x313145),
                    max: rgb(0x3b3b52),
                },
                action: rgb(0x89b4fa),
                formfield: rgb(0x313145),
                feedback: Feedback {
                    error: rgb(0xf38ba8),
                    warning: rgb(0xf9e2af),
                    success: rgb(0xa6e3a1),
                    info: rgb(0x89dceb),
                },
            },
            text: Text {
                base: rgb(0xcdd6f4),
                muted: rgb(0x6c7086),
                action: Action {
                    primary: rgb(0x89b4fa),
                    secondary: rgb(0x89dceb),
                    destructive: rgb(0xf38ba8),
                },
                formfield: rgb(0xcdd6f4),
                feedback: Feedback {
                    error: rgb(0xf38ba8),
                    warning: rgb(0xf9e2af),
                    success: rgb(0xa6e3a1),
                    info: rgb(0x89dceb),
                },
            },
            border: Border {
                base: rgb(0x45475a),
                active: rgb(0x89b4fa),
                subtle: rgb(0x2a2b3d),
            },
            scrollbar: Scrollbar {
                base: rgb(0x45475a),
            },
            syntax: Syntax {
                comment: rgb(0x6c7086),
                keyword: rgb(0x89b4fa),
                function: rgb(0x89b4fa),
                variable: rgb(0xcdd6f4),
                string: rgb(0xa6e3a1),
                number: rgb(0xfab387),
                type_: rgb(0xcba6f7),
                operator: rgb(0x89b4fa),
                punctuation: rgb(0x9399b2),
            },
            markdown: Markdown {
                text: rgb(0xcdd6f4),
                heading: rgb(0x89b4fa),
                link: rgb(0x89dceb),
                link_text: rgb(0x89dceb),
                code: rgb(0xfab387),
                block_quote: rgb(0x6c7086),
                emphasis: rgb(0xf9e2af),
                strong: rgb(0xcdd6f4),
                horizontal_rule: rgb(0x6c7086),
                list_item: rgb(0x89b4fa),
                list_enumeration: rgb(0x89b4fa),
                image: rgb(0xcba6f7),
                image_text: rgb(0xcba6f7),
                code_block: rgb(0xfab387),
            },
        }
    }

    /// The historical bespoke light palette.
    pub const fn catppuccin_light() -> Self {
        Self {
            background: Background {
                base: rgb(0xf5f5f7),
                raised: Raised {
                    base: rgb(0xffffff),
                    high: rgb(0xf0f0f4),
                    max: rgb(0xe6e6ec),
                },
                action: rgb(0x1e66f5),
                formfield: rgb(0xf0f0f4),
                feedback: Feedback {
                    error: rgb(0xc62828),
                    warning: rgb(0xb26a00),
                    success: rgb(0x2e7d32),
                    info: rgb(0x0277bd),
                },
            },
            text: Text {
                base: rgb(0x1f2430),
                muted: rgb(0x8a8f98),
                action: Action {
                    primary: rgb(0x1e66f5),
                    secondary: rgb(0x7b2ff2),
                    destructive: rgb(0xc62828),
                },
                formfield: rgb(0x1f2430),
                feedback: Feedback {
                    error: rgb(0xc62828),
                    warning: rgb(0xb26a00),
                    success: rgb(0x2e7d32),
                    info: rgb(0x0277bd),
                },
            },
            border: Border {
                base: rgb(0xc8ccd4),
                active: rgb(0x1e66f5),
                subtle: rgb(0xe4e4ea),
            },
            scrollbar: Scrollbar {
                base: rgb(0xc8ccd4),
            },
            syntax: Syntax {
                comment: rgb(0x8a8f98),
                keyword: rgb(0x1e66f5),
                function: rgb(0x1e66f5),
                variable: rgb(0x1f2430),
                string: rgb(0x2e7d32),
                number: rgb(0xb26a00),
                type_: rgb(0x7b2ff2),
                operator: rgb(0x1e66f5),
                punctuation: rgb(0x5a5f68),
            },
            markdown: Markdown {
                text: rgb(0x1f2430),
                heading: rgb(0x1e66f5),
                link: rgb(0x0277bd),
                link_text: rgb(0x0277bd),
                code: rgb(0xb26a00),
                block_quote: rgb(0x8a8f98),
                emphasis: rgb(0x7b2ff2),
                strong: rgb(0x1f2430),
                horizontal_rule: rgb(0x8a8f98),
                list_item: rgb(0x1e66f5),
                list_enumeration: rgb(0x1e66f5),
                image: rgb(0x7b2ff2),
                image_text: rgb(0x7b2ff2),
                code_block: rgb(0xb26a00),
            },
        }
    }

    /// A colourless theme (`NO_COLOR` / dumb terminals): every token is
    /// [`Color::Reset`], so output relies on bold/dim/reverse only.
    pub const fn monochrome() -> Self {
        const RAW: Color = Color::Reset;
        let feedback = Feedback {
            error: RAW,
            warning: RAW,
            success: RAW,
            info: RAW,
        };
        Self {
            background: Background {
                base: RAW,
                raised: Raised {
                    base: RAW,
                    high: RAW,
                    max: RAW,
                },
                action: RAW,
                formfield: RAW,
                feedback,
            },
            text: Text {
                base: RAW,
                muted: RAW,
                action: Action {
                    primary: RAW,
                    secondary: RAW,
                    destructive: RAW,
                },
                formfield: RAW,
                feedback,
            },
            border: Border {
                base: RAW,
                active: RAW,
                subtle: RAW,
            },
            scrollbar: Scrollbar { base: RAW },
            syntax: Syntax {
                comment: RAW,
                keyword: RAW,
                function: RAW,
                variable: RAW,
                string: RAW,
                number: RAW,
                type_: RAW,
                operator: RAW,
                punctuation: RAW,
            },
            markdown: Markdown {
                text: RAW,
                heading: RAW,
                link: RAW,
                link_text: RAW,
                code: RAW,
                block_quote: RAW,
                emphasis: RAW,
                strong: RAW,
                horizontal_rule: RAW,
                list_item: RAW,
                list_enumeration: RAW,
                image: RAW,
                image_text: RAW,
                code_block: RAW,
            },
        }
    }

    /// Resolve a bundled theme for a concrete (already system-resolved) mode.
    pub fn builtin(name: ThemeName, mode: ThemeMode) -> Self {
        match (name, mode) {
            (ThemeName::Opencode, ThemeMode::Light) => Self::light(),
            (ThemeName::Opencode, _) => Self::dark(),
            (ThemeName::Catppuccin, ThemeMode::Light) => Self::catppuccin_light(),
            (ThemeName::Catppuccin, _) => Self::catppuccin_dark(),
        }
    }

    /// Resolve the active theme: `NO_COLOR`/dumb terminals first, then a bundled
    /// name or a user JSON theme, applying `mode` (`system` uses `system_mode`).
    pub fn resolve(
        name: &str,
        mode: ThemeMode,
        themes_dirs: &[PathBuf],
        system_mode: ThemeMode,
    ) -> Self {
        if no_color() {
            return Self::monochrome();
        }
        let resolved_mode = match mode {
            ThemeMode::System => match system_mode {
                ThemeMode::Light => ThemeMode::Light,
                _ => ThemeMode::Dark,
            },
            other => other,
        };
        if let Some(builtin) = ThemeName::from_key(name) {
            return Self::builtin(builtin, resolved_mode);
        }
        for dir in themes_dirs {
            let path = dir.join(format!("{name}.json"));
            if let Ok(source) = std::fs::read_to_string(&path) {
                match ThemeFile::parse(&source) {
                    Ok(file) => return file.overlay(resolved_mode),
                    Err(err) => {
                        tracing::warn!("failed to parse theme {}: {err}", path.display());
                    }
                }
            }
        }
        tracing::warn!("unknown theme {name:?}; falling back to the default dark theme");
        Self::dark()
    }

    /// Detect the terminal's preferred [`ThemeMode`] (`system`). Precedence:
    /// `FAVETTO_THEME` override, OSC 11 query, `COLORFGBG`, then dark.
    pub fn detect_mode() -> ThemeMode {
        let env = std::env::var("FAVETTO_THEME").ok();
        let osc = query_osc11();
        let fgbg = std::env::var("COLORFGBG").ok();
        Self::detect_mode_from(env.as_deref(), osc.as_deref(), fgbg.as_deref())
    }

    /// Pure precedence resolver for the system mode, split out so it can be
    /// tested without a terminal.
    pub fn detect_mode_from(
        env: Option<&str>,
        osc: Option<&str>,
        colorfgbg: Option<&str>,
    ) -> ThemeMode {
        match env.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("dark") => return ThemeMode::Dark,
            Some("light") => return ThemeMode::Light,
            _ => {}
        }
        if let Some(mode) = osc.and_then(Self::from_osc_reply) {
            return mode;
        }
        if let Some(mode) = colorfgbg.and_then(Self::from_colorfgbg) {
            return mode;
        }
        ThemeMode::Dark
    }

    /// Parse an `OSC 11` reply such as `rgb:1e/1e/2e` or `rgb:ffff/ffff/ffff`.
    ///
    /// The background luminance decides the mode: brighter than mid-grey is
    /// light, otherwise dark.
    pub fn from_osc_reply(reply: &str) -> Option<ThemeMode> {
        let start = reply.find("rgb:")?;
        let body = &reply[start + 4..];
        let mut components = body.split('/');
        let r = parse_component(components.next()?)?;
        let g = parse_component(components.next()?)?;
        let b = parse_component(components.next()?)?;
        let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        Some(if luminance > 128.0 {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        })
    }

    /// Parse `COLORFGBG` (e.g. `15;0`): the last field is the background palette
    /// index; `>= 7` is a light background.
    pub fn from_colorfgbg(v: &str) -> Option<ThemeMode> {
        let bg = v.rsplit(';').next()?;
        let index: u8 = bg.trim().parse().ok()?;
        Some(if index >= 7 {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        })
    }

    /// Discover `*.json` user theme stems across `dirs` (best effort, sorted).
    pub fn discover_user_themes(dirs: &[PathBuf]) -> Vec<String> {
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

    /// The base frame style: default foreground on the background.
    pub fn base(&self) -> Style {
        Style::default().fg(self.text.base).bg(self.background.base)
    }

    /// The raised-surface style used by panels and cards.
    pub fn surface_style(&self) -> Style {
        self.raised_style(RaisedLevel::Base)
    }

    /// A style on one rung of the raised-surface ladder.
    pub fn raised_style(&self, level: RaisedLevel) -> Style {
        Style::default().fg(self.text.base).bg(self.raised(level))
    }

    /// The background colour at one rung of the raised-surface ladder.
    pub fn raised(&self, level: RaisedLevel) -> Color {
        match level {
            RaisedLevel::Base => self.background.raised.base,
            RaisedLevel::High => self.background.raised.high,
            RaisedLevel::Max => self.background.raised.max,
        }
    }

    /// The dimming scrim painted behind modal dialogs.
    pub fn scrim(&self) -> Style {
        Style::default()
            .fg(self.text.muted)
            .bg(self.scrim_color())
            .add_modifier(Modifier::DIM)
    }

    fn scrim_color(&self) -> Color {
        match self.background.base {
            Color::Rgb(r, g, b) => Color::Rgb(r / 2, g / 2, b / 2),
            _ => Color::Reset,
        }
    }

    /// Border style; `active` uses the accent border.
    pub fn block(&self, active: bool) -> Style {
        Style::default().fg(if active {
            self.border.active
        } else {
            self.border.base
        })
    }

    /// Table header text.
    pub fn table_header(&self) -> Style {
        Style::default()
            .fg(self.text.action.primary)
            .bg(self.background.base)
            .add_modifier(Modifier::BOLD)
    }

    /// Popup/section title text (on the raised surface).
    pub fn title(&self) -> Style {
        Style::default()
            .fg(self.text.action.primary)
            .bg(self.background.raised.base)
            .add_modifier(Modifier::BOLD)
    }

    /// Selected-row style.
    pub fn selected(&self) -> Style {
        Style::default()
            .fg(self.selected_fg())
            .bg(self.selected_bg())
            .add_modifier(Modifier::BOLD)
    }

    /// The selected-row background.
    pub fn selected_bg(&self) -> Color {
        self.text.action.primary
    }

    /// A contrast-aware foreground for [`Theme::selected_bg`].
    pub fn selected_fg(&self) -> Color {
        contrast_fg(self.selected_bg())
    }

    /// De-emphasised text on the base surface.
    pub fn muted_style(&self) -> Style {
        Style::default()
            .fg(self.text.muted)
            .bg(self.background.base)
    }

    /// Accent text on the base surface.
    pub fn accent_style(&self) -> Style {
        Style::default()
            .fg(self.text.action.primary)
            .bg(self.background.base)
    }

    /// A dimmed "disabled" style.
    pub fn disabled(&self) -> Style {
        Style::default()
            .fg(self.text.muted)
            .add_modifier(Modifier::DIM)
    }

    /// A destructive/error accent style.
    pub fn destructive(&self) -> Style {
        Style::default().fg(self.text.action.destructive)
    }

    /// A task status's colour.
    pub fn semantic(&self, status: TaskStatus) -> Color {
        match status {
            TaskStatus::Pending => self.text.feedback.warning,
            TaskStatus::Running => self.text.action.primary,
            TaskStatus::AwaitingInput => self.text.feedback.warning,
            TaskStatus::Succeeded => self.text.feedback.success,
            TaskStatus::Failed => self.text.feedback.error,
            TaskStatus::Cancelled => self.text.muted,
        }
    }

    // -- flat accessors (kept so widget code reads comfortably) --------------

    pub fn bg(&self) -> Color {
        self.background.base
    }

    pub fn fg(&self) -> Color {
        self.text.base
    }

    pub fn muted(&self) -> Color {
        self.text.muted
    }

    pub fn accent(&self) -> Color {
        self.text.action.primary
    }

    pub fn title_color(&self) -> Color {
        self.text.action.primary
    }

    pub fn success(&self) -> Color {
        self.text.feedback.success
    }

    pub fn warning(&self) -> Color {
        self.text.feedback.warning
    }

    pub fn danger(&self) -> Color {
        self.text.feedback.error
    }

    pub fn info(&self) -> Color {
        self.text.feedback.info
    }

    pub fn syn_key(&self) -> Color {
        self.syntax.keyword
    }

    pub fn syn_string(&self) -> Color {
        self.syntax.string
    }

    pub fn syn_number(&self) -> Color {
        self.syntax.number
    }

    pub fn syn_bool(&self) -> Color {
        self.syntax.type_
    }

    pub fn syn_comment(&self) -> Color {
        self.syntax.comment
    }

    pub fn syn_heading(&self) -> Color {
        self.markdown.heading
    }

    pub fn syn_code(&self) -> Color {
        self.markdown.code
    }

    pub fn syn_link(&self) -> Color {
        self.markdown.link
    }

    fn apply_token(&mut self, path: &str, color: Color) {
        match path {
            "background.base" => self.background.base = color,
            "background.raised.base" => self.background.raised.base = color,
            "background.raised.high" => self.background.raised.high = color,
            "background.raised.max" => self.background.raised.max = color,
            "background.action" => self.background.action = color,
            "background.formfield" => self.background.formfield = color,
            "background.feedback.error" => self.background.feedback.error = color,
            "background.feedback.warning" => self.background.feedback.warning = color,
            "background.feedback.success" => self.background.feedback.success = color,
            "background.feedback.info" => self.background.feedback.info = color,
            "text.base" => self.text.base = color,
            "text.muted" => self.text.muted = color,
            "text.action.primary" => self.text.action.primary = color,
            "text.action.secondary" => self.text.action.secondary = color,
            "text.action.destructive" => self.text.action.destructive = color,
            "text.formfield" => self.text.formfield = color,
            "text.feedback.error" => self.text.feedback.error = color,
            "text.feedback.warning" => self.text.feedback.warning = color,
            "text.feedback.success" => self.text.feedback.success = color,
            "text.feedback.info" => self.text.feedback.info = color,
            "border.base" => self.border.base = color,
            "border.active" => self.border.active = color,
            "border.subtle" => self.border.subtle = color,
            "scrollbar.base" => self.scrollbar.base = color,
            "syntax.comment" => self.syntax.comment = color,
            "syntax.keyword" => self.syntax.keyword = color,
            "syntax.function" => self.syntax.function = color,
            "syntax.variable" => self.syntax.variable = color,
            "syntax.string" => self.syntax.string = color,
            "syntax.number" => self.syntax.number = color,
            "syntax.type" => self.syntax.type_ = color,
            "syntax.operator" => self.syntax.operator = color,
            "syntax.punctuation" => self.syntax.punctuation = color,
            "markdown.text" => self.markdown.text = color,
            "markdown.heading" => self.markdown.heading = color,
            "markdown.link" => self.markdown.link = color,
            "markdown.linkText" => self.markdown.link_text = color,
            "markdown.code" => self.markdown.code = color,
            "markdown.blockQuote" => self.markdown.block_quote = color,
            "markdown.emphasis" => self.markdown.emphasis = color,
            "markdown.strong" => self.markdown.strong = color,
            "markdown.horizontalRule" => self.markdown.horizontal_rule = color,
            "markdown.listItem" => self.markdown.list_item = color,
            "markdown.listEnumeration" => self.markdown.list_enumeration = color,
            "markdown.image" => self.markdown.image = color,
            "markdown.imageText" => self.markdown.image_text = color,
            "markdown.codeBlock" => self.markdown.code_block = color,
            other => tracing::warn!("unknown theme token {other:?}"),
        }
    }
}

/// A user theme file: a name plus per-mode maps of `token.path = "colour"`.
///
/// Colours are `#rrggbb`, `transparent`, or a `$hue.<hue>.<100..900>` reference.
/// Missing tokens inherit from the built-in default for that mode, so partial
/// themes work.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThemeFile {
    /// The theme's display name (informational; the file stem selects it).
    #[serde(default)]
    #[allow(dead_code)]
    pub name: Option<String>,
    #[serde(default)]
    pub dark: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub light: Option<BTreeMap<String, String>>,
}

impl ThemeFile {
    pub fn parse(source: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(source)
    }

    /// Apply this file's tokens for `mode` on top of the built-in default.
    pub fn overlay(&self, mode: ThemeMode) -> Theme {
        let light = matches!(mode, ThemeMode::Light);
        let mut theme = if light {
            Theme::opencode_light()
        } else {
            Theme::opencode_dark()
        };
        let tokens = if light { &self.light } else { &self.dark };
        if let Some(tokens) = tokens {
            for (path, value) in tokens {
                match resolve_color(value) {
                    Some(color) => theme.apply_token(path, color),
                    None => tracing::warn!("unrecognised theme colour {value:?} for {path:?}"),
                }
            }
        }
        theme
    }
}

/// Parse a theme colour: `#rrggbb`, `transparent`, or `$hue.<hue>.<step>`.
pub fn resolve_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("transparent") {
        return Some(Color::Reset);
    }
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex).map(rgb);
    }
    if let Some(reference) = value.strip_prefix('$') {
        let mut parts = reference.split('.');
        if parts.next()? != "hue" {
            return None;
        }
        let hue = parts.next()?;
        let step: u16 = parts.next()?.parse().ok()?;
        if !(100..=900).contains(&step) || !step.is_multiple_of(100) {
            return None;
        }
        return hue_color(hue, step);
    }
    None
}

fn parse_hex(hex: &str) -> Option<u32> {
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// A small hue scale: step `500` is the base hue, lower steps blend toward
/// white (lighter) and higher steps toward black (darker).
fn hue_color(hue: &str, step: u16) -> Option<Color> {
    let base = match hue {
        "gray" => (0x80u8, 0x80u8, 0x80u8),
        "red" => (0xe0, 0x6c, 0x75),
        "orange" => (0xf5, 0xa7, 0x42),
        "yellow" => (0xe5, 0xc0, 0x7b),
        "green" => (0x7f, 0xd8, 0x8f),
        "cyan" => (0x56, 0xb6, 0xc2),
        "blue" => (0x5c, 0x9c, 0xf5),
        "purple" => (0x9d, 0x7c, 0xd8),
        _ => return None,
    };
    let blend = |channel: u8, other: u8, t: f32| -> u8 {
        (channel as f32 + (other as f32 - channel as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    let (r, g, b) = if step < 500 {
        let t = (500 - step) as f32 / 500.0 * 0.85;
        (
            blend(base.0, 0xff, t),
            blend(base.1, 0xff, t),
            blend(base.2, 0xff, t),
        )
    } else {
        let t = (step - 500) as f32 / 400.0 * 0.65;
        (
            blend(base.0, 0x00, t),
            blend(base.1, 0x00, t),
            blend(base.2, 0x00, t),
        )
    };
    Some(Color::Rgb(r, g, b))
}

/// Pick a black or white foreground for `bg` by luminance (resolved colours
/// only; other colours have no reliable luminance).
pub fn contrast_fg(bg: Color) -> Color {
    match bg {
        Color::Rgb(r, g, b) => {
            let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
            if luminance > 140.0 {
                Color::Black
            } else {
                Color::White
            }
        }
        _ => Color::Reset,
    }
}

/// Whether colour output should be suppressed (`NO_COLOR` set, or dumb terminal).
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
        || std::env::var("TERM")
            .map(|t| t.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false)
}

/// Parse a single `rgb:` component (1–4 hex digits) into an 8-bit channel.
fn parse_component(s: &str) -> Option<u8> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(&digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some((value * 255 / max) as u8)
}

/// Best-effort OSC 11 background query. Returns the raw `rgb:...` payload.
///
/// Must be called in raw mode and before any other reader consumes stdin.
#[cfg(unix)]
fn query_osc11() -> Option<String> {
    use std::io::Write;

    let mut stdout = std::io::stdout();
    stdout.write_all(b"\x1b]11;?\x1b\\").ok()?;
    stdout.flush().ok()?;

    let mut pollfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` is a valid, initialized structure and the timeout bounds it.
    let ready = unsafe { libc::poll(&mut pollfd, 1, 150) };
    if ready <= 0 {
        return None;
    }

    let mut buf = [0u8; 64];
    // SAFETY: `buf` is a valid writable buffer of the given length.
    let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return None;
    }
    let reply = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
    let start = reply.find("rgb:")?;
    let rest = &reply[start..];
    let end = rest.find(['\x07', '\\']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// Non-Unix stub: never query the terminal.
#[cfg(not(unix))]
fn query_osc11() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_is_opencode_and_modes_differ() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_ne!(dark, light);
        assert_eq!(dark.background.base, rgb(0x212121));
        assert_eq!(light.background.base, rgb(0xf8f8f8));
        assert_eq!(dark.text.action.primary, rgb(0xfab283));
        assert_eq!(light.text.action.primary, rgb(0x3b7dd8));
        // Every token group is set (never left as reset) in a colour theme.
        assert_ne!(dark.syntax.keyword, Color::Reset);
        assert_ne!(dark.markdown.heading, Color::Reset);
        assert_ne!(dark.border.active, Color::Reset);
    }

    #[test]
    fn catppuccin_preserves_the_historical_palette() {
        let dark = Theme::catppuccin_dark();
        assert_eq!(dark.background.base, rgb(0x1e1e2e));
        assert_eq!(dark.text.base, rgb(0xcdd6f4));
        assert_eq!(dark.syntax.keyword, rgb(0x89b4fa));
        assert_eq!(dark.syntax.comment, rgb(0x6c7086));

        let light = Theme::catppuccin_light();
        assert_eq!(light.background.base, rgb(0xf5f5f7));
        assert_eq!(light.text.base, rgb(0x1f2430));
        assert_eq!(light.syntax.keyword, rgb(0x1e66f5));
        assert_eq!(light.syntax.comment, rgb(0x8a8f98));
    }

    #[test]
    fn monochrome_uses_only_reset() {
        let theme = Theme::monochrome();
        assert_eq!(theme.background.base, Color::Reset);
        assert_eq!(theme.text.action.primary, Color::Reset);
        assert_eq!(theme.border.active, Color::Reset);
        assert_eq!(theme.syntax.keyword, Color::Reset);
        assert_eq!(theme.markdown.heading, Color::Reset);
        assert_eq!(theme.background.raised.max, Color::Reset);
    }

    #[test]
    fn base_style_sets_fg_and_bg() {
        let dark = Theme::dark();
        assert_eq!(dark.base().fg, Some(dark.fg()));
        assert_eq!(dark.base().bg, Some(dark.bg()));
    }

    #[test]
    fn selected_uses_a_contrast_aware_foreground() {
        let dark = Theme::dark();
        assert_eq!(dark.selected().bg, Some(dark.selected_bg()));
        assert_eq!(dark.selected().fg, Some(dark.selected_fg()));
        // The bright dark-theme accent gets a dark foreground.
        assert_eq!(dark.selected_fg(), Color::Black);
    }

    #[test]
    fn raised_levels_are_distinct() {
        let theme = Theme::dark();
        let base = theme.raised(RaisedLevel::Base);
        let high = theme.raised(RaisedLevel::High);
        let max = theme.raised(RaisedLevel::Max);
        assert_ne!(base, high);
        assert_ne!(high, max);
        assert_eq!(theme.raised_style(RaisedLevel::Max).bg, Some(max));
    }

    #[test]
    fn scrim_darkens_the_base_surface() {
        let theme = Theme::dark();
        assert_eq!(theme.scrim().bg, Some(rgb(0x101010)));
    }

    #[test]
    fn parses_osc11_reply_to_a_mode() {
        assert_eq!(Theme::from_osc_reply("rgb:1e/1e/2e"), Some(ThemeMode::Dark));
        assert_eq!(
            Theme::from_osc_reply("rgb:ffff/ffff/ffff"),
            Some(ThemeMode::Light)
        );
        assert_eq!(Theme::from_osc_reply("garbage"), None);
    }

    #[test]
    fn honours_colorfgbg() {
        assert_eq!(Theme::from_colorfgbg("15;0"), Some(ThemeMode::Dark));
        assert_eq!(Theme::from_colorfgbg("0;15"), Some(ThemeMode::Light));
        assert_eq!(Theme::from_colorfgbg("bogus"), None);
    }

    #[test]
    fn detect_mode_precedence() {
        assert_eq!(Theme::detect_mode_from(None, None, None), ThemeMode::Dark);
        assert_eq!(
            Theme::detect_mode_from(Some("light"), Some("rgb:1e/1e/2e"), Some("15;0")),
            ThemeMode::Light
        );
        assert_eq!(
            Theme::detect_mode_from(None, Some("rgb:ffff/ffff/ffff"), Some("15;0")),
            ThemeMode::Light
        );
        assert_eq!(
            Theme::detect_mode_from(None, None, Some("15;0")),
            ThemeMode::Dark
        );
    }

    #[test]
    fn resolves_builtin_names_and_modes() {
        assert_eq!(
            Theme::resolve("opencode", ThemeMode::Dark, &[], ThemeMode::Dark),
            Theme::opencode_dark()
        );
        assert_eq!(
            Theme::resolve("catppuccin", ThemeMode::Light, &[], ThemeMode::Dark),
            Theme::catppuccin_light()
        );
        // `system` follows the pre-resolved mode.
        assert_eq!(
            Theme::resolve("opencode", ThemeMode::System, &[], ThemeMode::Light),
            Theme::opencode_light()
        );
    }

    #[test]
    fn unknown_theme_falls_back_to_dark() {
        let theme = Theme::resolve("does-not-exist", ThemeMode::Dark, &[], ThemeMode::Dark);
        assert_eq!(theme, Theme::dark());
    }

    #[test]
    fn user_theme_overlay_inherits_missing_tokens() {
        let source = r##"{
            "name": "solarish",
            "dark": {
                "background.base": "#101418",
                "text.action.primary": "#ffcc00"
            }
        }"##;
        let file = ThemeFile::parse(source).unwrap();
        let theme = file.overlay(ThemeMode::Dark);
        assert_eq!(theme.background.base, rgb(0x101418));
        assert_eq!(theme.text.action.primary, rgb(0xffcc00));
        // Untouched tokens inherit from the default dark theme.
        assert_eq!(theme.text.base, Theme::opencode_dark().text.base);
        assert_eq!(theme.border.base, Theme::opencode_dark().border.base);
    }

    #[test]
    fn user_theme_transparent_and_hue_reference_resolve() {
        assert_eq!(resolve_color("transparent"), Some(Color::Reset));
        assert_eq!(resolve_color("#AABBCC"), Some(rgb(0xaabbcc)));
        let hue = resolve_color("$hue.blue.500").expect("hue resolves");
        assert_eq!(hue, rgb(0x5c9cf5));
        assert!(resolve_color("$hue.blue.250").is_none());
        assert!(resolve_color("$hue.notahue.500").is_none());
    }

    #[test]
    fn semantic_covers_all_statuses() {
        let theme = Theme::dark();
        assert_eq!(theme.semantic(TaskStatus::Pending), theme.warning());
        assert_eq!(theme.semantic(TaskStatus::Running), theme.accent());
        assert_eq!(theme.semantic(TaskStatus::AwaitingInput), theme.warning());
        assert_eq!(theme.semantic(TaskStatus::Succeeded), theme.success());
        assert_eq!(theme.semantic(TaskStatus::Failed), theme.danger());
        assert_eq!(theme.semantic(TaskStatus::Cancelled), theme.muted());
    }
}
