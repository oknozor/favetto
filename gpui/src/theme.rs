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

/// Narrowest usable task-detail column on wide windows.
pub const DETAIL_MIN_WIDTH: f32 = 280.0;

/// Widest the task-detail column may grow before it dwarfs the working list.
pub const DETAIL_MAX_WIDTH: f32 = 720.0;

/// Invisible horizontal hit area centred on the detail column's left border.
/// Wide enough to grab with a mouse without widening the visible 1 px divider.
pub const DETAIL_RESIZE_HANDLE: f32 = 8.0;

/// Minimum width reserved for the working list between the sidebar and detail.
pub const WORKING_LIST_MIN_WIDTH: f32 = 360.0;

/// Below this viewport width the detail region becomes a pushed page instead
/// of a third column (docs §1, "Responsive layout").
pub const NARROW_BREAKPOINT: f32 = 1120.0;

/// The smallest target an interactive control may have (docs §2, Fitts's Law:
/// "rows ≥ 32 px high"). Enforced by [`crate::components::with_focus`].
pub const MIN_TARGET: f32 = 32.0;

/// The height of a primary action (docs §2, Fitts's Law: "primary buttons
/// ≥ 40 px").
pub const PRIMARY_TARGET: f32 = 40.0;

/// The default row height used by dense lists and tables.
pub const ROW_HEIGHT: f32 = 32.0;

/// The gap between tightly coupled chips or keycaps. Not a spacing step: it is
/// a hairline-scale separator, deliberately outside the 4/8/12/16/24 scale.
pub const MICRO_GAP: f32 = 2.0;

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

/// Code-highlighting colors for fenced blocks (Zed-like, accessible on each
/// theme's surface). Views never use literal colours for syntax.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntaxColors {
    pub keyword: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub comment: Rgba,
    pub type_name: Rgba,
    pub function: Rgba,
    pub key: Rgba,
    pub punctuation: Rgba,
}

/// Control heights in px. Compact density changes spacing, never these values:
/// a target below [`MIN_TARGET`] would break Fitts's Law (docs §2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Controls {
    /// The minimum target size.
    pub min: f32,
    /// Dense table/list row.
    pub row: f32,
    /// The primary action on a surface.
    pub primary: f32,
}

/// The two themes of the client (docs §3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub spacing: Spacing,
    pub radii: Radii,
    pub font: FontScale,
    pub controls: Controls,
    pub colors: Colors,
    pub syntax: SyntaxColors,
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
    controls: Controls {
        min: MIN_TARGET,
        row: ROW_HEIGHT,
        primary: PRIMARY_TARGET,
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
    syntax: SyntaxColors {
        keyword: rgb(0xC678DD),
        string: rgb(0x98C379),
        number: rgb(0xD19A66),
        comment: rgb(0x7F848E),
        type_name: rgb(0xE5C07B),
        function: rgb(0x61AFEF),
        key: rgb(0xE06C75),
        punctuation: rgb(0xABB2BF),
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
    controls: Controls {
        min: MIN_TARGET,
        row: ROW_HEIGHT,
        primary: PRIMARY_TARGET,
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
    syntax: SyntaxColors {
        keyword: rgb(0xA626A4),
        string: rgb(0x50A14F),
        number: rgb(0x986801),
        comment: rgb(0xA0A1A7),
        type_name: rgb(0xC18401),
        function: rgb(0x4078F2),
        key: rgb(0xE45649),
        punctuation: rgb(0x383A42),
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
    /// Narrow-detail-column heading scale (docs §3 type scale).
    pub fn markdown_heading_size(&self, level: u8) -> f32 {
        match level {
            1 => self.font.lg,
            2 => self.font.base,
            3 | 4 => self.font.sm,
            _ => self.font.xs,
        }
    }

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
        // Density must never shrink a target below the accessibility floor.
        assert_eq!(compact.controls.min, MIN_TARGET);
        assert_eq!(compact.controls.primary, PRIMARY_TARGET);
        let light = Theme::for_settings(
            WindowAppearance::Dark,
            ThemeChoice::Light,
            Density::Comfortable,
        );
        assert_eq!(light.colors.bg, LIGHT.colors.bg);
    }

    #[test]
    fn syntax_palettes_differ_between_dark_and_light() {
        assert_ne!(DARK.syntax.keyword, LIGHT.syntax.keyword);
        assert_ne!(DARK.syntax.string, LIGHT.syntax.string);
        assert_ne!(DARK.syntax.comment, LIGHT.syntax.comment);
    }

    #[test]
    fn markdown_heading_sizes_are_non_increasing() {
        let sizes: Vec<f32> = (1..=6)
            .map(|level| DARK.markdown_heading_size(level))
            .collect();
        assert!(sizes[0] > sizes[1], "h1 must be larger than h2");
        assert!(sizes[0] > DARK.font.base, "h1 must exceed body text");
        assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]));
    }

    #[test]
    fn compact_only_shrinks_spacing() {
        let compact = Theme::for_settings(
            WindowAppearance::Dark,
            ThemeChoice::System,
            Density::Compact,
        );
        assert!(compact.spacing.s2 < DARK.spacing.s2);
        assert_eq!(compact.syntax, DARK.syntax);
        for level in 1..=6 {
            assert_eq!(
                compact.markdown_heading_size(level),
                DARK.markdown_heading_size(level)
            );
        }
    }
}
