//! Dependency-free guard tests for the VitePress documentation site.
//!
//! These keep the published site's wiring from silently regressing: the project
//! Pages `base`, the local search provider, the page set that mirrors the
//! README, the README's link back to the site, and the deploy workflow.

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

#[test]
fn vitepress_site_targets_the_project_pages_base() {
    let config = read("docs/.vitepress/config.mts");
    assert!(
        config.contains(r#"base: "/favetto/""#),
        "docs/.vitepress/config.mts must set base: \"/favetto/\" for the project Pages site"
    );
    assert!(
        config.contains("provider: \"local\""),
        "docs/.vitepress/config.mts must enable the built-in local search provider"
    );
}

#[test]
fn docs_pages_cover_the_manual() {
    let pages = [
        "docs/index.md",
        "docs/guide/getting-started.md",
        "docs/guide/tasks.md",
        "docs/guide/agents.md",
        "docs/guide/configuration.md",
        "docs/guide/webhooks.md",
        "docs/guide/tui.md",
        "docs/guide/execution.md",
        "docs/reference/architecture.md",
        "docs/reference/remote-api.md",
    ];

    let missing: Vec<&str> = pages
        .iter()
        .copied()
        .filter(|page| !repo_root().join(page).is_file())
        .collect();
    assert!(
        missing.is_empty(),
        "missing documentation pages: {missing:?}"
    );
}

#[test]
fn readme_links_the_published_site() {
    let readme = read("README.md");
    assert!(
        readme.contains("https://oknozor.github.io/favetto/"),
        "README.md must link to the published documentation site"
    );
}

#[test]
fn pages_workflow_exists() {
    let workflow = repo_root().join(".github/workflows/deploy-docs.yml");
    assert!(
        workflow.is_file(),
        "expected the GitHub Pages deploy workflow at {}",
        workflow.display()
    );
}
