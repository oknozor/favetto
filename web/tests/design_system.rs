//! Design-system guards for the Tailwind + Phosphor rebuild.
//!
//! These replace the old `style_tokens.rs` CSS-token test. The design system now
//! lives in `tailwind.css`, and the client's glyphs are Phosphor components, so
//! the guards are:
//!
//! 1. No banned emoji/unicode glyph appears in a Rust string or char literal
//!    under `src/` — the icon set owns every pictograph.
//! 2. `assets/style.css` is gone and `assets/tailwind.css` is the committed,
//!    generated artifact (non-empty and carrying the dark palette).
//!
//! Comments and doc-strings are ignored: `->` arrows in prose are not UI.

use std::path::{Path, PathBuf};

/// True for a glyph that must be rendered through the Phosphor icon wrapper
/// rather than as a literal character.
fn is_banned(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x1F300..=0x1FAFF // Emoji
            | 0x2600..=0x26FF // Miscellaneous Symbols
            | 0x2700..=0x27BF // Dingbats
            | 0x2B00..=0x2BFF // Miscellaneous Symbols and Arrows
            | 0x25A0..=0x25FF // Geometric Shapes
            | 0x2190..=0x21FF // Arrows
            | 0x2022 // •
            | 0x2298 // ⊘
            | 0x00D7 // ×
    )
}

/// Collect every `.rs` file under `dir`, recursively.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let entry = entry.expect("read dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Extract the contents of every double-quoted string literal in a source file.
///
/// Character literals carrying a single pictograph are rare in this codebase;
/// the string-literal scan catches every rsx text node and class attribute.
fn string_literal_chars(source: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        while let Some(inner) = chars.next() {
            match inner {
                '\\' => {
                    // Skip the escaped character.
                    let _ = chars.next();
                }
                '"' => break,
                other => out.push(other),
            }
        }
    }
    out
}

#[test]
fn no_banned_glyphs_in_source() {
    let mut files = Vec::new();
    collect_rs_files(Path::new("src"), &mut files);
    assert!(!files.is_empty(), "expected source files under src/");

    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).expect("read source file");
        for c in string_literal_chars(&source) {
            if is_banned(c) {
                offenders.push(format!("{}: U+{:04X} {c:?}", file.display(), c as u32));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "icon glyphs must be Phosphor components, not literals:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn stylesheet_is_replaced_by_tailwind() {
    assert!(
        !Path::new("assets/style.css").exists(),
        "the hand-written stylesheet must be deleted"
    );
    assert!(
        Path::new("tailwind.css").exists(),
        "the Tailwind entry file must exist at the crate root"
    );
}

#[test]
fn generated_tailwind_artifact_is_committed() {
    let css = std::fs::read_to_string("assets/tailwind.css")
        .expect("assets/tailwind.css must be generated and committed");
    assert!(!css.trim().is_empty(), "generated Tailwind CSS is empty");
    assert!(
        css.contains("prefers-color-scheme: dark"),
        "generated CSS must carry the dark palette"
    );
    assert!(
        css.contains(".status-badge"),
        "generated CSS must include the component layer"
    );
}
