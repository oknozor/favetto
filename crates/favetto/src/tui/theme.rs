//! The TUI's central colour palette.
//!
//! A single [`Theme`] is resolved once at startup — a built-in dark or light
//! variant, auto-detected from the terminal background (with a `FAVETTO_THEME`
//! override) — and every widget paints through it. `Theme` is `Copy`, so the
//! `draw_*` helpers can take it by value without fighting the borrow checker.

use ratatui::style::{Color, Modifier, Style};

use favetto_core::model::TaskStatus;

/// Semantic colour palette for the whole TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub surface: Color,
    pub border: Color,
    pub border_active: Color,
    pub muted: Color,
    pub accent: Color,
    pub title: Color,
    pub selected_fg: Color,
    pub selected_bg: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    pub syn_key: Color,
    pub syn_string: Color,
    pub syn_number: Color,
    pub syn_bool: Color,
    pub syn_comment: Color,
    pub syn_heading: Color,
    pub syn_code: Color,
    pub syn_link: Color,
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
    /// The built-in dark palette (Catppuccin-Mocha-ish).
    pub const fn dark() -> Self {
        Self {
            bg: rgb(0x1e1e2e),
            fg: rgb(0xcdd6f4),
            surface: rgb(0x252537),
            border: rgb(0x45475a),
            border_active: rgb(0x89b4fa),
            muted: rgb(0x6c7086),
            accent: rgb(0x89b4fa),
            title: rgb(0x89b4fa),
            selected_fg: rgb(0x1e1e2e),
            selected_bg: rgb(0x89b4fa),
            success: rgb(0xa6e3a1),
            warning: rgb(0xf9e2af),
            danger: rgb(0xf38ba8),
            info: rgb(0x89dceb),
            syn_key: rgb(0x89b4fa),
            syn_string: rgb(0xa6e3a1),
            syn_number: rgb(0xfab387),
            syn_bool: rgb(0xcba6f7),
            syn_comment: rgb(0x6c7086),
            syn_heading: rgb(0x89b4fa),
            syn_code: rgb(0xfab387),
            syn_link: rgb(0x89dceb),
        }
    }

    /// The built-in light palette.
    pub const fn light() -> Self {
        Self {
            bg: rgb(0xf5f5f7),
            fg: rgb(0x1f2430),
            surface: rgb(0xffffff),
            border: rgb(0xc8ccd4),
            border_active: rgb(0x1e66f5),
            muted: rgb(0x8a8f98),
            accent: rgb(0x1e66f5),
            title: rgb(0x1e66f5),
            selected_fg: rgb(0xffffff),
            selected_bg: rgb(0x1e66f5),
            success: rgb(0x2e7d32),
            warning: rgb(0xb26a00),
            danger: rgb(0xc62828),
            info: rgb(0x0277bd),
            syn_key: rgb(0x1e66f5),
            syn_string: rgb(0x2e7d32),
            syn_number: rgb(0xb26a00),
            syn_bool: rgb(0x7b2ff2),
            syn_comment: rgb(0x8a8f98),
            syn_heading: rgb(0x1e66f5),
            syn_code: rgb(0xb26a00),
            syn_link: rgb(0x0277bd),
        }
    }

    /// Resolve the active theme: `FAVETTO_THEME` override, then an OSC 11 query,
    /// then `COLORFGBG`, then dark.
    pub fn detect() -> Self {
        let env = std::env::var("FAVETTO_THEME").ok();
        let osc = query_osc11();
        let fgbg = std::env::var("COLORFGBG").ok();
        Self::detect_from(env.as_deref(), osc.as_deref(), fgbg.as_deref())
    }

    /// Pure precedence resolver, split out so it can be tested without a terminal.
    pub fn detect_from(env: Option<&str>, osc: Option<&str>, colorfgbg: Option<&str>) -> Self {
        match env.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("dark") => return Self::dark(),
            Some("light") => return Self::light(),
            _ => {}
        }
        if let Some(t) = osc.and_then(Self::from_osc_reply) {
            return t;
        }
        if let Some(t) = colorfgbg.and_then(Self::from_colorfgbg) {
            return t;
        }
        Self::dark()
    }

    /// Parse an `OSC 11` reply such as `rgb:1e/1e/2e` or `rgb:ffff/ffff/ffff`.
    ///
    /// The background luminance decides the variant: brighter than mid-grey is
    /// light, otherwise dark.
    pub fn from_osc_reply(reply: &str) -> Option<Self> {
        let start = reply.find("rgb:")?;
        let body = &reply[start + 4..];
        let mut components = body.split('/');
        let r = parse_component(components.next()?)?;
        let g = parse_component(components.next()?)?;
        let b = parse_component(components.next()?)?;
        let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        Some(if luminance > 128.0 {
            Self::light()
        } else {
            Self::dark()
        })
    }

    /// Parse `COLORFGBG` (e.g. `15;0`): the last field is the background palette
    /// index; `>= 7` is a light background.
    pub fn from_colorfgbg(v: &str) -> Option<Self> {
        let bg = v.rsplit(';').next()?;
        let index: u8 = bg.trim().parse().ok()?;
        Some(if index >= 7 {
            Self::light()
        } else {
            Self::dark()
        })
    }

    /// The base frame style: default foreground on the background.
    pub fn base(&self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }

    /// The raised-surface style used by popups and cards.
    pub fn surface_style(&self) -> Style {
        Style::default().fg(self.fg).bg(self.surface)
    }

    /// Border style; `active` uses the accent border.
    pub fn block(&self, active: bool) -> Style {
        Style::default().fg(if active {
            self.border_active
        } else {
            self.border
        })
    }

    /// Table header text.
    pub fn table_header(&self) -> Style {
        Style::default()
            .fg(self.title)
            .bg(self.bg)
            .add_modifier(Modifier::BOLD)
    }

    /// Popup/section title text (on the raised surface).
    pub fn title(&self) -> Style {
        Style::default()
            .fg(self.title)
            .bg(self.surface)
            .add_modifier(Modifier::BOLD)
    }

    /// Selected-row style.
    pub fn selected(&self) -> Style {
        Style::default().fg(self.selected_fg).bg(self.selected_bg)
    }

    /// De-emphasised text on the base surface.
    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted).bg(self.bg)
    }

    /// Accent text on the base surface.
    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent).bg(self.bg)
    }

    /// A task status's colour.
    pub fn semantic(&self, status: TaskStatus) -> Color {
        match status {
            TaskStatus::Pending => self.warning,
            TaskStatus::Running => self.accent,
            TaskStatus::Succeeded => self.success,
            TaskStatus::Failed => self.danger,
            TaskStatus::Cancelled => self.muted,
        }
    }
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
    fn dark_and_light_set_every_role_and_differ() {
        let dark = Theme::dark();
        let light = Theme::light();
        assert_ne!(dark, light);
        assert_eq!(dark.bg, rgb(0x1e1e2e));
        assert_eq!(dark.fg, rgb(0xcdd6f4));
        assert_eq!(dark.syn_key, rgb(0x89b4fa));
        assert_eq!(dark.syn_comment, rgb(0x6c7086));
        assert_eq!(light.bg, rgb(0xf5f5f7));
        assert_eq!(light.fg, rgb(0x1f2430));
        assert_eq!(light.syn_key, rgb(0x1e66f5));
        assert_eq!(light.syn_comment, rgb(0x8a8f98));
    }

    #[test]
    fn base_style_sets_fg_and_bg() {
        let dark = Theme::dark();
        assert_eq!(dark.base().fg, Some(dark.fg));
        assert_eq!(dark.base().bg, Some(dark.bg));
    }

    #[test]
    fn parses_osc11_reply() {
        assert_eq!(Theme::from_osc_reply("rgb:1e/1e/2e"), Some(Theme::dark()));
        assert_eq!(
            Theme::from_osc_reply("rgb:ffff/ffff/ffff"),
            Some(Theme::light())
        );
        assert_eq!(Theme::from_osc_reply("garbage"), None);
    }

    #[test]
    fn honours_colorfgbg() {
        assert_eq!(Theme::from_colorfgbg("15;0"), Some(Theme::dark()));
        assert_eq!(Theme::from_colorfgbg("0;15"), Some(Theme::light()));
        assert_eq!(Theme::from_colorfgbg("bogus"), None);
    }

    #[test]
    fn detect_from_precedence() {
        assert_eq!(Theme::detect_from(None, None, None), Theme::dark());
        assert_eq!(
            Theme::detect_from(Some("light"), Some("rgb:1e/1e/2e"), Some("15;0")),
            Theme::light()
        );
        assert_eq!(
            Theme::detect_from(None, Some("rgb:ffff/ffff/ffff"), Some("15;0")),
            Theme::light()
        );
        assert_eq!(Theme::detect_from(None, None, Some("15;0")), Theme::dark());
    }
}
