//! Dependency-free guard tests for the VitePress documentation site.
//!
//! These keep the published site's wiring from silently regressing: the project
//! Pages `base`, the local search provider, the page set that mirrors the
//! README, the README's link back to the site, the generated reference, the
//! placeholder assets, and the deploy workflow.

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

/// Every Markdown file under `docs/`, excluding `node_modules` and build output.
fn docs_markdown() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // `docs/.vitepress` holds config and generated build state only.
                if path.file_name().is_some_and(|n| n == ".vitepress") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Ok(contents) = fs::read_to_string(&path) {
                    out.push((path, contents));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo_root().join("docs"), &mut out);
    out
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
        "docs/guide/installation.md",
        "docs/guide/getting-started.md",
        "docs/guide/first-task.md",
        "docs/guide/catalog.md",
        "docs/guide/variables-and-prompts.md",
        "docs/guide/agents.md",
        "docs/guide/schedules-and-dependencies.md",
        "docs/guide/webhooks.md",
        "docs/guide/parallel-worktrees.md",
        "docs/guide/git-signing.md",
        "docs/guide/remote-access.md",
        "docs/guide/configuration.md",
        "docs/guide/tui.md",
        "docs/guide/troubleshooting.md",
        "docs/reference/architecture.md",
        "docs/reference/cli.md",
        "docs/reference/config.md",
        "docs/reference/tasks.md",
        "docs/reference/events.md",
        "docs/reference/environment.md",
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
fn readme_drops_stale_task_references() {
    let readme = read("README.md");
    for stale in ["triage_favetto_issues", "triage_cocogitto_issues"] {
        assert!(
            !readme.contains(stale),
            "README.md still references the removed task `{stale}`"
        );
    }
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

#[test]
fn deploy_workflow_watches_reference_sources() {
    let workflow = read(".github/workflows/deploy-docs.yml");
    assert!(
        workflow.contains("\"crates/**\""),
        "the deploy workflow must rebuild when crates/** changes"
    );
    assert!(
        workflow.contains("__doc"),
        "the deploy workflow must regenerate the reference before building"
    );
}

#[test]
fn logo_and_screenshot_placeholders_exist() {
    let root = repo_root();
    assert!(
        root.join("docs/public/logo.png").is_file(),
        "docs/public/logo.png is missing"
    );
    assert!(
        root.join("docs/public/screenshots").is_dir(),
        "docs/public/screenshots/ is missing"
    );

    // Every image referenced from a page must resolve to a real file.
    for (path, contents) in docs_markdown() {
        let mut rest = contents.as_str();
        while let Some(idx) = rest.find("/screenshots/") {
            rest = &rest[idx + "/screenshots/".len()..];
            let end = rest
                .find(|c: char| c == ')' || c == '"' || c == '\'' || c.is_whitespace())
                .unwrap_or(rest.len());
            let file = repo_root()
                .join("docs/public/screenshots")
                .join(&rest[..end]);
            assert!(
                file.is_file(),
                "{} references missing screenshot {}",
                path.display(),
                file.display()
            );
        }
    }
}

#[test]
fn generated_schema_lists_every_section() {
    let raw = read("docs/public/favetto-schema.json");
    let schema: serde_json::Value =
        serde_json::from_str(&raw).expect("favetto-schema.json must parse as JSON");
    let properties = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("schema must have top-level properties");
    for section in [
        "agent", "agents", "auth", "daemon", "executor", "git", "webhook", "web", "tui",
    ] {
        assert!(
            properties.contains_key(section),
            "schema is missing the `{section}` section"
        );
    }
}

#[test]
fn generated_reference_is_present() {
    for page in [
        "docs/reference/cli.md",
        "docs/reference/config.md",
        "docs/reference/events.md",
        "docs/reference/remote-api.md",
    ] {
        let body = read(page);
        assert!(
            body.contains("Generated from") && body.contains("Do not edit by hand"),
            "{page} does not look generated"
        );
    }
}

#[test]
fn documented_task_files_exist() {
    // Any `tasks/favetto/<name>.md` path mentioned in a page must exist.
    for (path, contents) in docs_markdown() {
        let mut rest = contents.as_str();
        while let Some(idx) = rest.find("tasks/favetto/") {
            rest = &rest[idx..];
            let end = rest.find(['`', ')', '"', '\'']).unwrap_or(rest.len());
            let relative = &rest[..end];
            if relative.ends_with(".md") {
                assert!(
                    repo_root().join(relative).is_file(),
                    "{} references missing task file {relative}",
                    path.display()
                );
            }
            rest = &rest[end..];
        }
    }
}
