//! Design tokens for the favetto desktop client (docs/design/gpui-client.md §3).
//!
//! One module holds the token set: the spacing scale (4/8/12/16/24), the
//! radii (4/8/12), the type scale (11/13/15/20/28), the semantic colors of
//! each theme, and the default sizes of the three regions.
//! [`Theme::for_appearance`] flips the theme with the OS appearance (docs §3:
//! "dark/light flipped by the OS appearance").

use std::sync::LazyLock;

use gpui::{rgb, Rgba, WindowAppearance};

use crate::config::{Density, ThemeChoice};

/// Sidebar width in px — the default width of the navigation region.
pub const SIDEBAR_WIDTH: f32 = 220.0;

/// Top bar height in px.
pub const TOP_BAR_HEIGHT: f32 = 52.0;

/// Status bar height in px — the default height of the status region.
pub const STATUS_BAR_HEIGHT: f32 = 28.0;

/// Detail column width in px on wide windows.
pub const DETAIL_WIDTH: f32 = 360.0;

/// Below this viewport width the detail region becomes a pushed page instead
/// of a third column (docs §1, "Responsive layout").
pub const NARROW_BREAKPOINT: f32 = 1120.0;

/// The spacing scale in px (docs §3): 4/8/12/16/24.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    /// 4px.
    pub s1: f32,
    /// 8px.
    pub s2: f32,
    /// 12px.
    pub s3: f32,
    /// 16px.
    pub s4: f32,
    /// 24px.
    pub s6: f32,
}

/// The radii in px (docs §3): 4/8/12.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radii {
    /// 4px.
    pub r4: f32,
    /// 8px.
    pub r8: f32,
    /// 12px.
    pub r12: f32,
}

/// The type scale in px (docs §3): 11/13/15/20/28.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontScale {
    /// 11px.
    pub xs: f32,
    /// 13px.
    pub sm: f32,
    /// 15px.
    pub base: f32,
    /// 20px.
    pub lg: f32,
    /// 28px.
    pub xl: f32,
}

/// Semantic colors of one theme (docs §3): `bg`, `surface`, `border`,
/// `text`, `muted`, `accent`, `warning`, `danger`, `success`. Status is
/// never color-only — icon + label + color (docs §2, "Accessibility").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    /// Page background.
    pub bg: Rgba,
    /// Bounded surfaces (sidebar, detail).
    pub surface: Rgba,
    /// Thin separators.
    pub border: Rgba,
    /// Primary text.
    pub text: Rgba,
    /// Secondary text.
    pub muted: Rgba,
    /// Running/open accent.
    pub accent: Rgba,
    /// Awaiting input.
    pub warning: Rgba,
    /// Failed.
    pub danger: Rgba,
    /// Succeeded.
    pub success: Rgba,
}

/// The two themes of the client (docs §3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub spacing: Spacing,
    pub radii: Radii,
    pub font: FontScale,
    pub colors: Colors,
}

static DARK: LazyLock<Theme> = LazyLock::new(|| Theme {
    spacing: Spacing {
        s1: 4.0,
        s2: 8.0,
        s3: 12.0,
        s4: 16.0,
        s6: 24.0,
    },
    radii: Radii {
        r4: 4.0,
        r8: 8.0,
        r12: 12.0,
    },
    font: FontScale {
        xs: 11.0,
        sm: 13.0,
        base: 15.0,
        lg: 20.0,
        xl: 28.0,
    },
    colors: Colors {
        bg: rgb(0x171717),
        surface: rgb(0x1F1F1F),
        border: rgb(0x38383A),
        text: rgb(0xF5F5F5),
        muted: rgb(0x8E8E93),
        accent: rgb(0x0A84FF),
        warning: rgb(0xFF9F0A),
        danger: rgb(0xFF453A),
        success: rgb(0x30D158),
    },
});

static LIGHT: LazyLock<Theme> = LazyLock::new(|| Theme {
    spacing: Spacing {
        s1: 4.0,
        s2: 8.0,
        s3: 12.0,
        s4: 16.0,
        s6: 24.0,
    },
    radii: Radii {
        r4: 4.0,
        r8: 8.0,
        r12: 12.0,
    },
    font: FontScale {
        xs: 11.0,
        sm: 13.0,
        base: 15.0,
        lg: 20.0,
        xl: 28.0,
    },
    colors: Colors {
        bg: rgb(0xF5F5F5),
        surface: rgb(0xFFFFFF),
        border: rgb(0xD1D1D6),
        text: rgb(0x1D1D1F),
        muted: rgb(0x8E8E93),
        accent: rgb(0x007AFF),
        warning: rgb(0xFF9500),
        danger: rgb(0xFF3B30),
        success: rgb(0x34C759),
    },
});

/// The compact spacing scale: every step shrinks by roughly a quarter, so
/// lists, cards and the sidebar gain room without changing the type scale.
fn compact(mut theme: Theme) -> Theme {
    theme.spacing = Spacing {
        s1: 3.0,
        s2: 6.0,
        s3: 9.0,
        s4: 12.0,
        s6: 18.0,
    };
    theme
}

static DARK_COMPACT: LazyLock<Theme> = LazyLock::new(|| compact(*DARK));
static LIGHT_COMPACT: LazyLock<Theme> = LazyLock::new(|| compact(*LIGHT));

impl Theme {
    /// The theme following the OS appearance (docs §3: "dark/light flipped
    /// by the OS appearance"). Kept as the convenience entry point for tests
    /// and future non-Settings consumers; the shell uses [`Self::for_settings`].
    #[allow(dead_code)]
    pub fn for_appearance(appearance: WindowAppearance) -> &'static Theme {
        Self::for_settings(appearance, ThemeChoice::System, Density::Comfortable)
    }

    /// The effective theme after the Settings appearance preferences:
    /// `System` follows the OS, `Dark`/`Light` override it, and density
    /// selects the spacing scale. Every check is a static lookup, so a view can
    /// call this on each render (live theme changes) without allocation.
    pub fn for_settings(
        appearance: WindowAppearance,
        choice: ThemeChoice,
        density: Density,
    ) -> &'static Theme {
        let dark = match choice {
            ThemeChoice::System => {
                matches!(
                    appearance,
                    WindowAppearance::Dark | WindowAppearance::VibrantDark
                )
            }
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
        };
        match (dark, density) {
            (true, Density::Comfortable) => &DARK,
            (true, Density::Compact) => &DARK_COMPACT,
            (false, Density::Comfortable) => &LIGHT,
            (false, Density::Compact) => &LIGHT_COMPACT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appearance_preferences_override_the_os_and_compact_shrinks_spacing() {
        let dark = Theme::for_settings(
            WindowAppearance::Light,
            ThemeChoice::Dark,
            Density::Comfortable,
        );
        assert_eq!(dark.colors.bg, DARK.colors.bg);
        let compact = Theme::for_settings(
            WindowAppearance::Dark,
            ThemeChoice::System,
            Density::Compact,
        );
        assert!(compact.spacing.s4 < DARK.spacing.s4);
        assert_eq!(compact.colors.bg, DARK.colors.bg);
        let light = Theme::for_settings(
            WindowAppearance::Dark,
            ThemeChoice::Light,
            Density::Comfortable,
        );
        assert_eq!(light.colors.bg, LIGHT.colors.bg);
    }
}
