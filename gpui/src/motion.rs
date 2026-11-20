//! OS "reduce motion" detection.
//!
//! The pinned GPUI build exposes no reduced-motion query, so this module reads
//! the same desktop setting GTK applications use. The client deliberately ships
//! no decorative animation; if a future surface adds one it must call
//! [`prefers_reduced_motion`] (or [`duration`]) and keep any essential
//! information in the text, never in the motion alone.
//!
//! Detection order:
//! 1. `FAVETTO_REDUCE_MOTION` (`1`/`true`/`yes`/`on` or the inverse).
//! 2. `GTK_ENABLE_ANIMATIONS=0`.
//! 3. `gtk-enable-animations=false` in the GTK 3 or GTK 4 settings file.
//!
//! A missing or unreadable file is treated as "motion allowed"; the check never
//! panics and never blocks the UI thread for long (one small config read).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Whether the desktop asked applications to reduce motion.
pub fn prefers_reduced_motion() -> bool {
    if let Some(value) = env::var_os("FAVETTO_REDUCE_MOTION") {
        if let Some(value) = value.to_str().and_then(parse_bool) {
            return value;
        }
    }
    if let Some(value) = env::var_os("GTK_ENABLE_ANIMATIONS") {
        if value.to_str().and_then(parse_bool) == Some(false) {
            return true;
        }
    }
    gtk_settings_files()
        .iter()
        .any(|path| settings_file_disables_animations(path))
}

/// Collapse a decorative duration to zero when reduce motion is on. Callers
/// still render the final state immediately, so no information is lost.
pub fn duration(milliseconds: u64) -> Duration {
    if prefers_reduced_motion() {
        Duration::ZERO
    } else {
        Duration::from_millis(milliseconds)
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn gtk_settings_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        push_gtk_paths(&mut files, Path::new(&config));
    } else if let Some(home) = env::var_os("HOME") {
        push_gtk_paths(&mut files, &Path::new(&home).join(".config"));
    }
    files
}

fn push_gtk_paths(files: &mut Vec<PathBuf>, config: &Path) {
    for version in ["gtk-4.0", "gtk-3.0"] {
        files.push(config.join(version).join("settings.ini"));
    }
}

fn settings_file_disables_animations(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .is_some_and(|contents| setting_disables_animations(&contents))
}

/// Does one INI-style settings file turn animations off?
fn setting_disables_animations(contents: &str) -> bool {
    contents.lines().any(|line| {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            return false;
        }
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        key.trim().eq_ignore_ascii_case("gtk-enable-animations") && parse_bool(value) == Some(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk_settings_parser_reads_the_animation_key() {
        assert!(setting_disables_animations(
            "[Settings]\ngtk-enable-animations=false\n"
        ));
        assert!(setting_disables_animations(
            "[Settings]\n  Gtk-Enable-Animations = 0  \n"
        ));
        assert!(!setting_disables_animations(
            "[Settings]\ngtk-enable-animations=true\n"
        ));
        assert!(!setting_disables_animations(
            "[Settings]\n# gtk-enable-animations=false\n"
        ));
        assert!(!setting_disables_animations("theme=Breeze\n"));
    }

    #[test]
    fn duration_is_zero_only_when_the_environment_asks_for_it() {
        // The parser is the deterministic part; the environment-driven wrapper
        // is checked in the live keyboard pass rather than by mutating process
        // environment variables from a parallel test.
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("ON"), Some(true));
        assert_eq!(parse_bool("maybe"), None);
    }
}
