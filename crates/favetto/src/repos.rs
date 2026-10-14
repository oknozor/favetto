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

use favetto_core::model::RepoInfo;
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
        .map(|(name, path)| KnownRepo {
            name,
            path: PathBuf::from(path),
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
/// called once at daemon startup so config edits seed the persisted registry.
pub async fn seed_from_config(state: &State, config: &favetto_core::config::FavettoConfig) {
    for seed in &config.repos.known {
        let path = favetto_core::paths::expand_tilde(&seed.path);
        let path_str = path.to_string_lossy().into_owned();
        match crate::db::repo_name_for_path(&state.db, &path_str).await {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "failed to read known-repo registry");
                continue;
            }
        }
        let base = seed.name.clone().unwrap_or_else(|| derive_name(&path));
        let name = unique_name(state, &sanitize_name(&base));
        match crate::db::insert_repo(&state.db, &name, &path_str).await {
            Ok(()) => state.repos.write().push(KnownRepo { name, path }),
            Err(e) => tracing::warn!(path = %path_str, error = %e, "failed to seed known repo"),
        }
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
    if state.repos.read().iter().any(|repo| repo.path == path) {
        anyhow::bail!("repo '{}' is already known", path.display());
    }

    let base = params.name.unwrap_or_else(|| derive_name(&path));
    let sanitized = sanitize_name(&base);
    if sanitized.is_empty() {
        anyhow::bail!("could not derive a repo name from '{base}'");
    }
    let name = unique_name(state, &sanitized);

    let path_str = path.to_string_lossy().into_owned();
    crate::db::insert_repo(&state.db, &name, &path_str).await?;
    let repo = KnownRepo { name, path };
    state.repos.write().push(repo.clone());
    state.repos.write().sort_by(|a, b| a.name.cmp(&b.name));

    crate::catalog_watch::reload(state).await;
    crate::catalog_watch::resync(state);

    Ok(repo_info(&repo, state.active_repo.read().as_deref()))
}

/// Remove a known repo by name. Clears the active repo if it was the removed one.
pub async fn remove(state: &Arc<State>, name: &str) -> anyhow::Result<bool> {
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
    if !repos.iter().any(|repo| repo.name == base) {
        return base.to_string();
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !repos.iter().any(|repo| repo.name == candidate) {
            return candidate;
        }
    }
    // Unreachable in practice: the loop only ends by returning.
    format!("{base}-{}", repos.len() + 1)
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
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let catalog = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let scheduler = tokio_cron_scheduler::JobScheduler::new().await.unwrap();
        Arc::new(State::new(StateInit {
            db: pool,
            bus: EventBus::new(64),
            token: favetto_core::auth::Token::generate(),
            webhooks: crate::webhooks::WebhookSecrets::from_config(
                &crate::config::FavettoConfig::default(),
            ),
            agents: crate::agents::AgentManager::new(),
            registry: crate::agents::AgentRegistry::default(),
            config: Arc::new(crate::config::FavettoConfig::default()),
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
}
