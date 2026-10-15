//! Static guards for the Android soft-keyboard overlay fix.
//!
//! Focusing a text field used to resize the Android WebView, which pushed the
//! fixed bottom navigation, dialogs and toasts up instead of letting the
//! keyboard overlay the content. Two pieces of wiring fix that:
//!
//! 1. `web/index.html` asks the browser to overlay the keyboard
//!    (`interactive-widget=overlays-content`).
//! 2. `web/android/MainActivity.kt`, wired through `web/Dioxus.toml`
//!    (`android_main_activity`), keeps the native window metrics fixed
//!    (`SOFT_INPUT_ADJUST_NOTHING`).
//!
//! Neither is exercised by a runtime test, so these read the committed sources
//! and fail if the hints are dropped or the build config stops pointing at the
//! custom activity. They also keep the committed `web/dist` from regressing to
//! a built bundle (it must stay the no-assets placeholder).

use std::fs;
use std::path::{Path, PathBuf};

/// Repository root: `<manifest dir>/crates/favetto` → up two levels.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/favetto always has a repository root two levels up")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

/// The client asks the browser to float the IME over the page.
#[test]
fn viewport_tells_the_keyboard_to_overlay_content() {
    let index = read("web/index.html");
    assert!(
        index.contains("interactive-widget=overlays-content"),
        "web/index.html must set `interactive-widget=overlays-content` in the viewport meta"
    );
    assert!(
        index.contains("viewport-fit=cover"),
        "web/index.html must keep `viewport-fit=cover` for the safe-area insets"
    );
}

/// The native activity is wired through `Dioxus.toml` and exists on disk.
#[test]
fn native_activity_is_wired_through_dioxus_toml() {
    let config = read("web/Dioxus.toml");
    assert!(
        config.contains(r#"android_main_activity = "android/MainActivity.kt""#),
        "web/Dioxus.toml must point `android_main_activity` at `android/MainActivity.kt`"
    );
    assert!(
        repo_root().join("web/android/MainActivity.kt").is_file(),
        "web/android/MainActivity.kt must exist"
    );
}

/// The custom activity keeps the window metrics fixed while the IME is shown.
#[test]
fn native_activity_keeps_the_window_metrics_fixed() {
    let activity = read("web/android/MainActivity.kt");
    assert!(
        activity.contains(": WryActivity()"),
        "MainActivity must extend the Dioxus `WryActivity`"
    );
    assert!(
        activity.contains("SOFT_INPUT_ADJUST_NOTHING"),
        "MainActivity must force `SOFT_INPUT_ADJUST_NOTHING`"
    );
    assert!(
        activity.contains("setSoftInputMode"),
        "MainActivity must call `window.setSoftInputMode(...)`"
    );
}

/// `web/Dioxus.toml` must parse as TOML both on its own and after the Android CI
/// signing step appends its own `[bundle.android]` table. A committed active
/// table makes the append a duplicate key, and `dx bundle` fails before it
/// builds the APK.
#[test]
fn dioxus_toml_has_no_duplicate_bundle_android_table() {
    let config = read("web/Dioxus.toml");

    // The committed manifest parses on its own.
    toml::from_str::<toml::Value>(&config).expect("web/Dioxus.toml must be valid TOML");

    // The CI signing step appends this block verbatim (see
    // `.github/workflows/ci.yml`). If the committed file already declares the
    // table, the combination is an invalid TOML duplicate.
    let ci_signing = concat!(
        "[bundle.android]\n",
        "jks_file = \"android-release.jks\"\n",
        "jks_password = \"ci-password\"\n",
        "key_alias = \"favetto\"\n",
        "key_password = \"ci-password\"\n",
    );
    let combined = format!("{config}{ci_signing}");
    toml::from_str::<toml::Value>(&combined)
        .expect("web/Dioxus.toml plus the Android CI signing table must be valid TOML");

    // At most one active (uncommented) `[bundle.android]` header.
    let active = config
        .lines()
        .filter(|line| line.trim() == "[bundle.android]")
        .count();
    assert!(
        active <= 1,
        "web/Dioxus.toml must not declare an active [bundle.android] table"
    );
}

/// The embedded `web/dist` stays the diagnostic placeholder, never a build.
#[test]
fn committed_dist_is_the_no_assets_placeholder() {
    let dist = read("web/dist/index.html");
    assert!(
        dist.contains("favetto-placeholder"),
        "web/dist/index.html must stay the no-assets placeholder"
    );
    assert!(
        !dist.contains("assets/favetto-web"),
        "web/dist/index.html must not reference a built hashed bundle"
    );
}
