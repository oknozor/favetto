//! Known-repos registry and repo-local catalog aggregation.
//!
//! The daemon persists a set of repositories whose `<repo>/.favetto/tasks/`
//! catalogs it loads. Each repo has a unique display `name` derived from its
//! directory basename (overridable at add time) that namespaces its tasks as
//! `repo:<name>/<relative-path>`; the absolute daemon-host `path` is what is
//! stored. One repo is the daemon-wide *active* repo, scoping the default target
//! of new catalog tasks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use favetto_core::model::{RepoInfo, RepoSource};
use favetto_core::rpc::ReposAddParams;
use favetto_core::tasks::{load_repo_catalog, resolve_repo_refs, split_repo, TaskDef};

use crate::event_bus::ServerPush;
use crate::state::State;

/// A repository known to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownRepo {
    /// Namespace key used in `repo:<name>/<path>` identities.
    pub name: String,
    /// Absolute daemon-host directory.
    pub path: PathBuf,
    /// How the repo entered the registry: explicitly or via the base-dir scan.
    pub source: RepoSource,
}

impl KnownRepo {
    /// The repo's `.favetto/tasks/` directory.
    pub fn tasks_dir(&self) -> PathBuf {
        favetto_core::paths::repo_tasks_dir(&self.path)
    }

    /// Whether the repo's catalog directory currently exists.
    pub fn available(&self) -> bool {
        self.tasks_dir().is_dir()
    }
}

/// Load the persisted registry from the database, sorted by name.
pub async fn load_registry(state: &State) -> anyhow::Result<Vec<KnownRepo>> {
    let rows = crate::db::list_repos(&state.db).await?;
    Ok(rows
        .into_iter()
        .map(|row| KnownRepo {
            name: row.name,
            path: PathBuf::from(row.path),
            source: row.source,
        })
        .collect())
}

/// Replace the in-memory registry from the database.
pub async fn refresh(state: &State) -> anyhow::Result<()> {
    let repos = load_registry(state).await?;
    *state.repos.write() = repos;
    Ok(())
}

/// Register every `[repos].known` entry that is not already present. Idempotent;
/// called once at daemon startup so config edits seed the persisted registry. An
/// explicit seed for a path that the base-dir scan already registered pins it by
/// promoting the row to `manual`.
pub async fn seed_from_config(state: &State, config: &favetto_core::config::FavettoConfig) {
    for seed in &config.repos.known {
        let path = favetto_core::paths::expand_tilde(&seed.path);
        let path_str = path.to_string_lossy().into_owned();
        match crate::db::repo_name_for_path(&state.db, &path_str).await {
            Ok(Some(name)) => {
                promote_in_place(state, &name).await;
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "failed to read known-repo registry");
                continue;
            }
        }
        let base = seed.name.clone().unwrap_or_else(|| derive_name(&path));
        let name = unique_name(state, &sanitize_name(&base));
        match crate::db::insert_repo(&state.db, &name, &path_str, RepoSource::Manual).await {
            Ok(()) => state.repos.write().push(KnownRepo {
                name,
                path,
                source: RepoSource::Manual,
            }),
            Err(e) => tracing::warn!(path = %path_str, error = %e, "failed to seed known repo"),
        }
    }
}

/// Persist `name` as a manual repo and mirror that on the in-memory entry, so an
/// explicit add/seed pins a repo the base-dir scan would otherwise reclaim.
async fn promote_in_place(state: &State, name: &str) {
    if let Err(e) = crate::db::set_repo_source(&state.db, name, RepoSource::Manual).await {
        tracing::warn!(repo = %name, error = %e, "failed to pin known repo");
        return;
    }
    if let Some(repo) = state
        .repos
        .write()
        .iter_mut()
        .find(|repo| repo.name == name)
    {
        repo.source = RepoSource::Manual;
    }
}

/// The known repos plus the active repo as wire types, sorted by name.
pub fn list(state: &State) -> Vec<RepoInfo> {
    let active = state.active_repo.read().clone();
    state
        .repos
        .read()
        .iter()
        .map(|repo| repo_info(repo, active.as_deref()))
        .collect()
}

/// Build the wire [`RepoInfo`] for `repo`, marking it active when it is named.
pub fn repo_info(repo: &KnownRepo, active: Option<&str>) -> RepoInfo {
    RepoInfo {
        name: repo.name.clone(),
        path: repo.path.to_string_lossy().into_owned(),
        available: repo.available(),
        active: active == Some(repo.name.as_str()),
        source: repo.source,
    }
}

/// The known repo with `name`, if registered.
pub fn resolve(state: &State, name: &str) -> Option<KnownRepo> {
    state
        .repos
        .read()
        .iter()
        .find(|repo| repo.name == name)
        .cloned()
}

/// The `.md` file backing a task identity (global or repo-qualified).
///
/// Returns `None` for a repo-qualified name whose repo is not registered.
pub fn task_file_path(state: &State, name: &str) -> Option<PathBuf> {
    match split_repo(name) {
        Some((repo, rel)) => {
            let repo = resolve(state, repo)?;
            Some(repo.tasks_dir().join(format!("{rel}.md")))
        }
        None => Some(state.tasks_dir.join(format!("{name}.md"))),
    }
}

/// Load the full catalog: the global tasks directory plus every repo's
/// `.favetto/tasks/`, with same-repo bare references resolved.
pub fn load_all(state: &State, prior: &[TaskDef]) -> Vec<TaskDef> {
    let mut defs = crate::tasks::reload_catalog(&state.tasks_dir, prior);
    for repo in state.repos.read().iter() {
        defs.extend(load_repo_catalog(&repo.name, &repo.path, prior));
    }
    resolve_repo_refs(&mut defs);
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    defs
}

/// Register a repository from a daemon-host path.
///
/// The path is `~`-expanded and must be an existing directory. The display name
/// defaults to the directory basename, is sanitized to `[A-Za-z0-9._-]`, and is
/// made unique with a numeric suffix. Notifies the catalog watcher and reloads.
pub async fn add(state: &Arc<State>, params: ReposAddParams) -> anyhow::Result<RepoInfo> {
    let expanded = favetto_core::paths::expand_tilde(&params.path);
    if !expanded.is_dir() {
        anyhow::bail!("repo path '{}' is not a directory", expanded.display());
    }
    let path = std::fs::canonicalize(&expanded).unwrap_or(expanded);
    let existing = state
        .repos
        .read()
        .iter()
        .find(|repo| repo.path == path)
        .cloned();
    if let Some(existing) = existing {
        if existing.source == RepoSource::Scanned {
            // An explicit add pins a repo the base-dir scan discovered, keeping
            // its auto-derived (and possibly de-duplicated) name.
            promote_in_place(state, &existing.name).await;
            let repo = resolve(state, &existing.name).unwrap_or(existing);
            return Ok(repo_info(&repo, state.active_repo.read().as_deref()));
        }
        anyhow::bail!("repo '{}' is already known", path.display());
    }

    let base = params.name.unwrap_or_else(|| derive_name(&path));
    let sanitized = sanitize_name(&base);
    if sanitized.is_empty() {
        anyhow::bail!("could not derive a repo name from '{base}'");
    }
    let name = unique_name(state, &sanitized);

    let path_str = path.to_string_lossy().into_owned();
    crate::db::insert_repo(&state.db, &name, &path_str, RepoSource::Manual).await?;
    let repo = KnownRepo {
        name,
        path,
        source: RepoSource::Manual,
    };
    state.repos.write().push(repo.clone());
    state.repos.write().sort_by(|a, b| a.name.cmp(&b.name));

    crate::catalog_watch::reload(state).await;
    crate::catalog_watch::resync(state);

    Ok(repo_info(&repo, state.active_repo.read().as_deref()))
}

/// Remove a known repo by name. Clears the active repo if it was the removed one.
///
/// Scanned repos are rejected: their directory is still on disk, so the scan
/// would immediately re-register them.
pub async fn remove(state: &Arc<State>, name: &str) -> anyhow::Result<bool> {
    if let Some(repo) = resolve(state, name) {
        if repo.source == RepoSource::Scanned {
            anyhow::bail!("repo '{name}' is managed by the base-directory scan");
        }
    }
    let removed = crate::db::delete_repo(&state.db, name).await?;
    if !removed {
        return Ok(false);
    }
    state.repos.write().retain(|repo| repo.name != name);
    if state.active_repo.read().as_deref() == Some(name) {
        *state.active_repo.write() = None;
        crate::db::set_active_repo(&state.db, None).await?;
    }

    crate::catalog_watch::reload(state).await;
    crate::catalog_watch::resync(state);
    Ok(true)
}

/// Set (or clear) the daemon-wide active repo and notify clients.
pub async fn activate(state: &Arc<State>, name: Option<String>) -> anyhow::Result<()> {
    if let Some(name) = name.as_deref() {
        if resolve(state, name).is_none() {
            anyhow::bail!("unknown repo '{name}'");
        }
    }
    crate::db::set_active_repo(&state.db, name.as_deref()).await?;
    *state.active_repo.write() = name;
    state.bus.publish(ServerPush::CatalogUpdated);
    Ok(())
}

/// Repo roots under `base` within `depth` levels that contain a `.favetto`
/// directory. Sorted, skips hidden entries and symlinks, and does not descend
/// into a directory that is itself a repo (so nested checkouts are not counted
/// twice). `depth = 1` means immediate children only.
pub fn scan_base_dir(base: &Path, depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if depth == 0 {
        return found;
    }
    let mut stack = vec![(base.to_path_buf(), 1usize)];
    while let Some((dir, level)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') {
                    return false;
                }
                // `file_type` does not follow symlinks, so a symlinked checkout
                // is skipped and a scan cannot loop.
                matches!(entry.file_type(), Ok(ft) if ft.is_dir() && !ft.is_symlink())
            })
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for child in children {
            if child.join(".favetto").is_dir() {
                found.push(child);
            } else if level < depth {
                stack.push((child, level + 1));
            }
        }
    }
    found.sort();
    found
}

/// Re-scan `[repos].base_dir` and reconcile the registry.
///
/// Newly discovered repos are registered as [`RepoSource::Scanned`]; scanned
/// rows whose directory vanished are deleted, while manual/seeded repos are
/// never reclaimed. Clears the active repo if it was reclaimed and publishes
/// `catalog.updated` when the registry changed. Returns whether it changed.
pub async fn rediscover(state: &Arc<State>) -> anyhow::Result<bool> {
    let Some(base) = state.config.repos.base_dir.clone() else {
        return Ok(false);
    };
    let base = favetto_core::paths::expand_tilde(&base);
    let depth = state.config.repos.depth.max(1);

    let mut candidates: Vec<PathBuf> = scan_base_dir(&base, depth)
        .into_iter()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .collect();
    candidates.sort();
    candidates.dedup();

    let snapshot: Vec<KnownRepo> = state.repos.read().clone();

    // Start from the repos that survive: every manual repo, plus scanned repos
    // whose directory is still a scan candidate.
    let mut next: Vec<KnownRepo> = Vec::new();
    let mut covered: Vec<PathBuf> = Vec::new();
    for repo in &snapshot {
        let canonical = std::fs::canonicalize(&repo.path).unwrap_or_else(|_| repo.path.clone());
        let keep = match repo.source {
            RepoSource::Manual => true,
            RepoSource::Scanned => candidates.contains(&canonical),
        };
        if keep {
            covered.push(canonical);
            next.push(repo.clone());
        }
    }

    // Register each remaining candidate deterministically (candidates are
    // sorted) so repeated scans assign stable suffixes.
    for candidate in &candidates {
        if covered.contains(candidate) {
            continue;
        }
        let base_name = sanitize_name(&derive_name(candidate));
        if base_name.is_empty() {
            continue;
        }
        let name = unique_name_in(&next, &base_name);
        let path_str = candidate.to_string_lossy().into_owned();
        match crate::db::insert_repo(&state.db, &name, &path_str, RepoSource::Scanned).await {
            Ok(()) => {
                next.push(KnownRepo {
                    name,
                    path: candidate.clone(),
                    source: RepoSource::Scanned,
                });
                covered.push(candidate.clone());
            }
            Err(e) => {
                tracing::warn!(path = %path_str, error = %e, "failed to register scanned repo")
            }
        }
    }
    next.sort_by(|a, b| a.name.cmp(&b.name));

    // Deregister scanned rows whose directory vanished.
    for repo in &snapshot {
        if repo.source == RepoSource::Scanned && !next.iter().any(|r| r.name == repo.name) {
            if let Err(e) = crate::db::delete_repo(&state.db, &repo.name).await {
                tracing::warn!(repo = %repo.name, error = %e, "failed to deregister scanned repo");
            }
        }
    }

    let mut snapshot_sorted = snapshot.clone();
    snapshot_sorted.sort_by(|a, b| a.name.cmp(&b.name));
    if snapshot_sorted == next {
        return Ok(false);
    }

    *state.repos.write() = next.clone();

    // A vanished active repo falls back to the global catalog.
    let active = state.active_repo.read().clone();
    if let Some(active) = active {
        if !next.iter().any(|repo| repo.name == active) {
            *state.active_repo.write() = None;
            crate::db::set_active_repo(&state.db, None).await?;
        }
    }

    state.bus.publish(ServerPush::CatalogUpdated);
    Ok(true)
}

/// `rediscover` plus a catalog reload and watcher re-target, for an on-demand
/// `repos.refresh` call.
pub async fn refresh_from_scan(state: &Arc<State>) -> anyhow::Result<bool> {
    let changed = rediscover(state).await?;
    if changed {
        crate::catalog_watch::reload(state).await;
        crate::catalog_watch::resync(state);
    }
    Ok(changed)
}

/// Derive a display name from a repo directory's basename.
pub fn derive_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string()
}

/// Sanitize a display name to `[A-Za-z0-9._-]`; other characters become `-`.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Ensure a name is unique within the registry, appending `-2`, `-3`, … as needed.
fn unique_name(state: &State, base: &str) -> String {
    let repos = state.repos.read();
    unique_name_in(&repos, base)
}

/// [`unique_name`] against an explicit set of repos (used while rebuilding the
/// registry so newly allocated names are considered too).
fn unique_name_in(existing: &[KnownRepo], base: &str) -> String {
    if !existing.iter().any(|repo| repo.name == base) {
        return base.to_string();
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|repo| repo.name == candidate) {
            return candidate;
        }
    }
    // Unreachable in practice: the loop only ends by returning.
    format!("{base}-{}", existing.len() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::EventBus;
    use crate::state::StateInit;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "favetto-repos-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn test_state(dir: &Path) -> Arc<State> {
        test_state_with_config(dir, crate::config::FavettoConfig::default()).await
    }

    async fn test_state_with_config(
        dir: &Path,
        config: crate::config::FavettoConfig,
    ) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let catalog = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let scheduler = tokio_cron_scheduler::JobScheduler::new().await.unwrap();
        let config = Arc::new(config);
        Arc::new(State::new(StateInit {
            config_path: std::path::PathBuf::new(),
            db: pool,
            bus: EventBus::new(64),
            token: favetto_core::auth::Token::generate(),
            webhooks: crate::webhooks::WebhookSecrets::from_config(&config),
            agents: crate::agents::AgentManager::new(),
            registry: crate::agents::AgentRegistry::default(),
            config,
            data_dir: dir.to_path_buf(),
            tasks_dir,
            catalog,
            scheduler,
            hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
        }))
    }

    fn make_repo(root: &Path, name: &str) -> PathBuf {
        let repo = root.join(name);
        std::fs::create_dir_all(repo.join(".favetto/tasks")).unwrap();
        std::fs::write(
            repo.join(".favetto/tasks/triage.md"),
            "agent = \"x\"\n---\nbody\n",
        )
        .unwrap();
        repo
    }

    #[tokio::test]
    async fn add_derives_name_and_maps_files() {
        let root = temp_dir("add");
        let state = test_state(&root).await;
        let repo_path = make_repo(&root, "favetto");

        let info = add(
            &state,
            ReposAddParams {
                path: repo_path.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(info.name, "favetto");
        assert!(info.available);
        assert!(!info.active);

        // The catalog now carries the repo's task with a qualified name.
        let catalog = crate::repos::load_all(&state, &[]);
        assert!(catalog.iter().any(|d| d.name == "repo:favetto/triage"));

        // `task_file_path` maps the qualified identity back to the repo file.
        assert_eq!(
            task_file_path(&state, "repo:favetto/triage"),
            Some(repo_path.join(".favetto/tasks/triage.md"))
        );
        assert_eq!(
            task_file_path(&state, "plain"),
            Some(state.tasks_dir.join("plain.md"))
        );
        assert_eq!(task_file_path(&state, "repo:missing/a"), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn add_rejects_missing_dir_and_duplicates() {
        let root = temp_dir("reject");
        let state = test_state(&root).await;
        let repo_path = make_repo(&root, "favetto");

        let missing = root.join("nope");
        assert!(add(
            &state,
            ReposAddParams {
                path: missing.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .is_err());

        add(
            &state,
            ReposAddParams {
                path: repo_path.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        // The same path cannot be added twice.
        assert!(add(
            &state,
            ReposAddParams {
                path: repo_path.to_string_lossy().into_owned(),
                name: Some("other".to_string()),
            },
        )
        .await
        .is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn duplicate_basenames_get_unique_suffixed_names() {
        let root = temp_dir("unique");
        let state = test_state(&root).await;
        let a = root.join("a").join("repo");
        let b = root.join("b").join("repo");
        for p in [&a, &b] {
            std::fs::create_dir_all(p.join(".favetto/tasks")).unwrap();
        }

        let first = add(
            &state,
            ReposAddParams {
                path: a.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        let second = add(
            &state,
            ReposAddParams {
                path: b.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(first.name, "repo");
        assert_eq!(second.name, "repo-2");

        // A dirty explicit name is sanitized.
        let other = root.join("weird dir");
        std::fs::create_dir_all(other.join(".favetto/tasks")).unwrap();
        let info = add(
            &state,
            ReposAddParams {
                path: other.to_string_lossy().into_owned(),
                name: Some("weird name!".to_string()),
            },
        )
        .await
        .unwrap();
        assert_eq!(info.name, "weird-name");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remove_clears_active_and_availability_flips() {
        let root = temp_dir("remove");
        let state = test_state(&root).await;
        let repo_path = make_repo(&root, "favetto");
        add(
            &state,
            ReposAddParams {
                path: repo_path.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        activate(&state, Some("favetto".to_string())).await.unwrap();
        assert_eq!(state.active_repo.read().as_deref(), Some("favetto"));

        // Removing the repo drops its tasks and clears the active repo.
        assert!(remove(&state, "favetto").await.unwrap());
        assert!(state.active_repo.read().is_none());
        assert!(crate::repos::load_all(&state, &[]).is_empty());
        assert!(!remove(&state, "favetto").await.unwrap());

        // Availability flips when the catalog directory disappears.
        let repo_path = make_repo(&root, "web");
        add(
            &state,
            ReposAddParams {
                path: repo_path.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        assert!(
            list(&state)
                .iter()
                .find(|r| r.name == "web")
                .unwrap()
                .available
        );
        std::fs::remove_dir_all(repo_path.join(".favetto/tasks")).unwrap();
        assert!(
            !list(&state)
                .iter()
                .find(|r| r.name == "web")
                .unwrap()
                .available
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sanitize_name_replaces_and_trims() {
        assert_eq!(sanitize_name("my repo!"), "my-repo");
        assert_eq!(sanitize_name("a/b/c"), "a-b-c");
        assert_eq!(sanitize_name("--x--"), "x");
        assert_eq!(sanitize_name("ok.name_1-2"), "ok.name_1-2");
    }

    fn config_with_base(base: &Path, depth: usize) -> crate::config::FavettoConfig {
        let mut config = crate::config::FavettoConfig::default();
        config.repos.base_dir = Some(base.to_path_buf());
        config.repos.depth = depth;
        config
    }

    fn scanned_names(state: &State) -> Vec<String> {
        list(state)
            .into_iter()
            .filter(|r| r.source == RepoSource::Scanned)
            .map(|r| r.name)
            .collect()
    }

    #[test]
    fn scan_base_dir_finds_children_with_favetto() {
        let root = temp_dir("scan-basic");
        make_repo(&root, "with-config");
        // A child without `.favetto/`.
        std::fs::create_dir_all(root.join("plain")).unwrap();
        // A hidden child with `.favetto/` is skipped.
        std::fs::create_dir_all(root.join(".hidden/.favetto")).unwrap();
        // A plain file is not a repo.
        std::fs::write(root.join("file.txt"), "x").unwrap();

        let found = scan_base_dir(&root, 1);
        assert_eq!(found, vec![root.join("with-config")]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_base_dir_respects_depth() {
        let root = temp_dir("scan-depth");
        make_repo(&root, "immediate");
        let nested = root.join("org").join("repo");
        std::fs::create_dir_all(nested.join(".favetto")).unwrap();

        assert_eq!(scan_base_dir(&root, 1), vec![root.join("immediate")]);

        let mut two = scan_base_dir(&root, 2);
        two.sort();
        let mut expected = vec![root.join("immediate"), nested];
        expected.sort();
        assert_eq!(two, expected);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn scan_base_dir_skips_symlinks_and_nested_repos() {
        let root = temp_dir("scan-links");
        let outer = make_repo(&root, "outer");
        // A nested repo is not descended into: only the outer repo is returned.
        std::fs::create_dir_all(outer.join("inner/.favetto")).unwrap();
        // A symlinked checkout is skipped (no follow).
        std::os::unix::fs::symlink(&outer, root.join("link")).unwrap();

        let found = scan_base_dir(&root, 4);
        assert_eq!(found, vec![outer]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn rediscover_registers_scanned_repos_deterministically() {
        let root = temp_dir("rediscover");
        let base = root.join("code");
        make_repo(&base.join("a"), "repo");
        make_repo(&base.join("b"), "repo");
        let state = test_state_with_config(&root, config_with_base(&base, 2)).await;

        assert!(rediscover(&state).await.unwrap());
        assert_eq!(scanned_names(&state), vec!["repo", "repo-2"]);
        assert!(state
            .repos
            .read()
            .iter()
            .all(|r| r.source == RepoSource::Scanned));

        // A second scan is stable: same names, nothing changed.
        assert!(!rediscover(&state).await.unwrap());
        assert_eq!(scanned_names(&state), vec!["repo", "repo-2"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn rediscover_removes_scanned_repo_when_it_vanishes() {
        let root = temp_dir("rediscover-gone");
        let base = root.join("code");
        let repo = make_repo(&base, "gone");
        let state = test_state_with_config(&root, config_with_base(&base, 1)).await;

        assert!(rediscover(&state).await.unwrap());
        assert!(crate::repos::load_all(&state, &[])
            .iter()
            .any(|d| d.name == "repo:gone/triage"));
        let mut rx = state.bus.subscribe();

        std::fs::remove_dir_all(&repo).unwrap();
        assert!(rediscover(&state).await.unwrap());
        assert!(list(&state).is_empty());
        assert!(crate::db::list_repos(&state.db).await.unwrap().is_empty());
        assert!(crate::repos::load_all(&state, &[]).is_empty());
        assert!(matches!(rx.try_recv(), Ok(ServerPush::CatalogUpdated)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn rediscover_never_reclaims_manual_repos() {
        let root = temp_dir("rediscover-manual");
        let base = root.join("code");
        make_repo(&base, "scanned");
        let manual = make_repo(&root, "manual");
        let state = test_state_with_config(&root, config_with_base(&base, 1)).await;

        add(
            &state,
            ReposAddParams {
                path: manual.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();

        rediscover(&state).await.unwrap();
        let names: Vec<String> = list(&state).into_iter().map(|r| r.name).collect();
        assert!(names.contains(&"manual".to_string()), "{names:?}");
        assert!(names.contains(&"scanned".to_string()), "{names:?}");

        // Deleting the manual repo's directory does not reclaim it.
        std::fs::remove_dir_all(&manual).unwrap();
        rediscover(&state).await.unwrap();
        assert!(list(&state).iter().any(|r| r.name == "manual"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn rediscover_clears_active_when_the_active_repo_vanishes() {
        let root = temp_dir("rediscover-active");
        let base = root.join("code");
        let repo = make_repo(&base, "active");
        let state = test_state_with_config(&root, config_with_base(&base, 1)).await;

        rediscover(&state).await.unwrap();
        activate(&state, Some("active".to_string())).await.unwrap();
        assert_eq!(state.active_repo.read().as_deref(), Some("active"));

        std::fs::remove_dir_all(&repo).unwrap();
        rediscover(&state).await.unwrap();
        assert!(state.active_repo.read().is_none());
        assert!(crate::db::get_active_repo(&state.db)
            .await
            .unwrap()
            .is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn add_promotes_a_scanned_repo_to_manual() {
        let root = temp_dir("rediscover-promote");
        let base = root.join("code");
        let repo = make_repo(&base, "promoted");
        let state = test_state_with_config(&root, config_with_base(&base, 1)).await;

        rediscover(&state).await.unwrap();
        assert_eq!(scanned_names(&state), vec!["promoted"]);

        let info = add(
            &state,
            ReposAddParams {
                path: repo.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(info.source, RepoSource::Manual);
        assert_eq!(info.name, "promoted");

        // Once pinned, a vanished directory no longer reclaims it.
        std::fs::remove_dir_all(&repo).unwrap();
        rediscover(&state).await.unwrap();
        assert!(list(&state).iter().any(|r| r.name == "promoted"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn seed_promotes_a_scanned_repo_to_manual() {
        let root = temp_dir("rediscover-seed");
        let base = root.join("code");
        let repo = make_repo(&base, "seeded");
        let state = test_state_with_config(&root, config_with_base(&base, 1)).await;

        rediscover(&state).await.unwrap();
        assert_eq!(scanned_names(&state), vec!["seeded"]);

        let path = std::fs::canonicalize(&repo).unwrap();
        let mut seed_config = crate::config::FavettoConfig::default();
        seed_config.repos.known.push(crate::config::RepoSeed {
            path: path.clone(),
            name: None,
        });
        seed_from_config(&state, &seed_config).await;

        let repo_entry = state
            .repos
            .read()
            .iter()
            .find(|r| r.name == "seeded")
            .cloned()
            .unwrap();
        assert_eq!(repo_entry.source, RepoSource::Manual);

        std::fs::remove_dir_all(&repo).unwrap();
        rediscover(&state).await.unwrap();
        assert!(list(&state).iter().any(|r| r.name == "seeded"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remove_rejects_a_scanned_repo() {
        let root = temp_dir("rediscover-remove");
        let base = root.join("code");
        make_repo(&base, "scanned");
        let state = test_state_with_config(&root, config_with_base(&base, 1)).await;

        rediscover(&state).await.unwrap();
        assert!(remove(&state, "scanned").await.is_err());
        assert!(list(&state).iter().any(|r| r.name == "scanned"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
