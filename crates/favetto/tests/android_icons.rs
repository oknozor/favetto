//! Static guards for the Android launcher-icon pipeline.
//!
//! `dioxus-cli` 0.7.10 ignores `[bundle] icon` / `[android] icon` and hardcodes
//! the stock Android Studio launcher resources, so the favetto badge cannot be
//! wired through config alone. Instead the badge is derived from
//! `docs/public/logo.png` into `web/assets/icon.png` and a repo-controlled
//! `web/android/res` tree, and a committed Gradle init script overlays that tree
//! onto the project `dx` generates. `web/android/dx.sh` installs the init script
//! and is what CI bundles through.
//!
//! None of that is exercised by a runtime test, so these read the committed
//! sources and fail if a density, the adaptive-icon wiring, or the overlay
//! plumbing regresses. They mirror the conventions in `mobile_keyboard.rs`.

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

fn read_bytes(relative: &str) -> Vec<u8> {
    let path = repo_root().join(relative);
    fs::read(&path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

/// Read the `width`/`height` out of a PNG's IHDR chunk.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    if bytes.len() < 24 || !bytes.starts_with(&SIG) {
        return None;
    }
    if bytes.get(12..16) != Some(b"IHDR".as_slice()) {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

/// The legacy density buckets and their `ic_launcher.png` sizes in pixels.
const LEGACY: &[(&str, u32)] = &[
    ("mdpi", 48),
    ("hdpi", 72),
    ("xhdpi", 96),
    ("xxhdpi", 144),
    ("xxxhdpi", 192),
];

/// The adaptive-icon foreground canvases (108 dp × density) in pixels.
const ADAPTIVE: &[(&str, u32)] = &[
    ("mdpi", 108),
    ("hdpi", 162),
    ("xhdpi", 216),
    ("xxhdpi", 324),
    ("xxxhdpi", 432),
];

#[test]
fn bundle_icon_is_the_512_badge() {
    let bytes = read_bytes("web/assets/icon.png");
    assert_eq!(
        png_size(&bytes),
        Some((512, 512)),
        "web/assets/icon.png must be the 512×512 badge derived from the brand logo"
    );
}

#[test]
fn legacy_launcher_icons_have_the_required_densities() {
    for (dpi, size) in LEGACY {
        let relative = format!("web/android/res/mipmap-{dpi}/ic_launcher.png");
        let bytes = read_bytes(&relative);
        assert_eq!(
            png_size(&bytes),
            Some((*size, *size)),
            "{relative} must be {size}×{size}"
        );
    }
}

#[test]
fn adaptive_foreground_canvases_are_108dp() {
    for (dpi, size) in ADAPTIVE {
        let relative = format!("web/android/res/mipmap-{dpi}/ic_launcher_foreground.png");
        let bytes = read_bytes(&relative);
        assert_eq!(
            png_size(&bytes),
            Some((*size, *size)),
            "{relative} must be {size}×{size} (108 dp canvas)"
        );
    }
}

#[test]
fn adaptive_icon_references_the_badge_layers() {
    let adaptive = read("web/android/res/mipmap-anydpi-v26/ic_launcher.xml");
    assert!(
        adaptive.contains("<adaptive-icon"),
        "the adaptive icon must be an `<adaptive-icon>`"
    );
    assert!(
        adaptive.contains("@color/ic_launcher_background"),
        "the adaptive icon must reference the `ic_launcher_background` colour"
    );
    assert!(
        adaptive.contains("@mipmap/ic_launcher_foreground"),
        "the adaptive icon must reference the `ic_launcher_foreground` mipmap"
    );

    let background = read("web/android/res/values/ic_launcher_background.xml");
    assert!(
        background.contains(r#"name="ic_launcher_background""#),
        "values/ic_launcher_background.xml must define `ic_launcher_background`"
    );
}

#[test]
fn gradle_init_script_overlays_the_generated_resources() {
    let script = read("web/android/gradle/favetto-icons.gradle.kts");
    for needle in [
        "FAVETTO_ANDROID_RES",
        "beforeProject",
        "ic_launcher_foreground.xml",
        "mipmap-",
    ] {
        assert!(
            script.contains(needle),
            "the Gradle init script must contain `{needle}`"
        );
    }
}

#[test]
fn dx_wrapper_installs_the_overlay() {
    let wrapper = read("web/android/dx.sh");
    for needle in ["init.d", "favetto-icons.gradle.kts", "FAVETTO_ANDROID_RES"] {
        assert!(
            wrapper.contains(needle),
            "web/android/dx.sh must contain `{needle}`"
        );
    }
}

#[test]
fn dioxus_toml_keeps_the_bundle_icon() {
    let config = read("web/Dioxus.toml");
    assert!(
        config.contains(r#"icon = ["assets/icon.png"]"#),
        "web/Dioxus.toml must keep `[bundle] icon = [\"assets/icon.png\"]`"
    );
}

#[test]
fn android_ci_job_bundles_through_the_wrapper() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains("android/dx.sh"),
        "the Android CI job must bundle through `android/dx.sh`"
    );
}

#[test]
fn docs_mention_the_icon_pipeline() {
    for relative in ["web/README.md", "docs/guide/web-client.md"] {
        let doc = read(relative);
        for needle in ["generate_icons.py", "web/android/res"] {
            assert!(doc.contains(needle), "{relative} must document `{needle}`");
        }
    }
}
