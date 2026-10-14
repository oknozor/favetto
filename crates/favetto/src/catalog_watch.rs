//! Live reload of the task catalog.
//!
//! The daemon watches its tasks directory and, when `*.md` files change, reloads
//! the in-memory catalog — merging with the previous definitions so a parse error
//! never drops a task — reconciles the catalog's cron schedules, and broadcasts
//! `catalog.updated` so connected TUIs re-fetch the list.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::event_bus::ServerPush;
use crate::state::State;

/// Coalesce bursts of filesystem events (editors often write several) into a
/// single reload.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Watch the catalog roots and reload the catalog on changes.
///
/// The watched roots are the global tasks directory plus every known repo's
/// `.favetto/tasks/` directory. The OS watcher is set up synchronously so that,
/// once this returns, no further change can be missed; the debounced reload loop
/// then runs on a background task for the lifetime of the process. A root that
/// does not exist yet is represented by its nearest existing ancestor and
/// re-targeted once it appears. Adding/removing a repo sends a control signal on
/// [`State::watch_tx`] which re-targets the watcher and reloads.
pub fn spawn(state: Arc<State>) -> anyhow::Result<()> {
    let ctrl_rx = state
        .watch_rx
        .lock()
        .take()
        .ok_or_else(|| anyhow::anyhow!("catalog watcher already spawned"))?;
    let (fs_tx, fs_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                if event_relevant(&ev) {
                    let _ = fs_tx.send(());
                }
            }
            Err(e) => tracing::warn!(error = %e, "catalog watch error"),
        })?;

    let mut watching = Vec::new();
    for target in desired_targets(&state) {
        if let Err(e) = watcher.watch(&target, RecursiveMode::Recursive) {
            tracing::warn!(dir = %target.display(), error = %e, "failed to watch catalog root");
        } else {
            watching.push(target);
        }
    }
    tracing::info!(
        roots = watching.len(),
        tasks_dir = %state.tasks_dir.display(),
        "watching task catalogs for changes"
    );

    tokio::spawn(debounce_loop(watcher, watching, fs_rx, ctrl_rx, state));
    Ok(())
}

/// Signal the catalog watcher to re-target its roots and reload (a repo was
/// added or removed). Non-blocking; the watcher coalesces the work.
pub fn resync(state: &State) {
    let _ = state.watch_tx.send(());
}

/// The path to watch for a catalog root: the directory when it exists,
/// otherwise its nearest existing ancestor so that creating it later is
/// observed.
fn watch_target(tasks_dir: &Path) -> PathBuf {
    if tasks_dir.exists() {
        return tasks_dir.to_path_buf();
    }
    let mut candidate = tasks_dir;
    while let Some(parent) = candidate.parent() {
        if parent.exists() {
            return parent.to_path_buf();
        }
        candidate = parent;
    }
    PathBuf::from(".")
}

/// The set of paths the watcher should currently watch: the global tasks
/// directory plus each known repo's `.favetto/tasks/` (when its checkout
/// exists), each mapped through [`watch_target`] and de-duplicated.
fn desired_targets(state: &State) -> Vec<PathBuf> {
    let mut roots = vec![state.tasks_dir.clone()];
    for repo in state.repos.read().iter() {
        if repo.path.exists() {
            roots.push(repo.tasks_dir());
        }
    }
    let mut targets: Vec<PathBuf> = roots.iter().map(|root| watch_target(root)).collect();
    targets.sort();
    targets.dedup();
    targets
}

/// Re-point the watcher at the current set of desired targets, adding new roots
/// and dropping removed ones.
fn retarget_all(watcher: &mut RecommendedWatcher, watching: &mut Vec<PathBuf>, state: &State) {
    let desired = desired_targets(state);
    for target in watching.iter() {
        if !desired.contains(target) {
            let _ = watcher.unwatch(target);
        }
    }
    for target in &desired {
        if !watching.contains(target) {
            match watcher.watch(target, RecursiveMode::Recursive) {
                Ok(()) => tracing::info!(dir = %target.display(), "catalog watch added a root"),
                Err(e) => {
                    tracing::warn!(dir = %target.display(), error = %e, "failed to watch catalog root")
                }
            }
        }
    }
    watching.retain(|target| desired.contains(target));
}

/// Debounce filesystem and control events, then reload the catalog once the
/// stream is quiet.
///
/// Reloads never overlap: this loop is the only caller and it awaits each one, so
/// the last filesystem state always wins.
async fn debounce_loop(
    mut watcher: RecommendedWatcher,
    mut watching: Vec<PathBuf>,
    mut fs_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    mut ctrl_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    state: Arc<State>,
) {
    loop {
        tokio::select! {
            event = fs_rx.recv() => {
                if event.is_none() {
                    return;
                }
            }
            signal = ctrl_rx.recv() => {
                if signal.is_none() {
                    return;
                }
                // The repo set changed: re-target before reloading.
                retarget_all(&mut watcher, &mut watching, &state);
            }
        }
        loop {
            match tokio::time::timeout(DEBOUNCE, fs_rx.recv()).await {
                // Another change arrived within the debounce window: keep waiting.
                Ok(Some(())) => continue,
                // The watcher went away: stop the loop.
                Ok(None) => return,
                // Quiet for `DEBOUNCE`: apply the change.
                Err(_) => break,
            }
        }
        retarget_all(&mut watcher, &mut watching, &state);
        reload(&state).await;
    }
}

/// Whether a filesystem event plausibly concerns a `*.md` task definition.
///
/// Directory-level events have no path extension and are treated as relevant so
/// file creation/removal is never missed; editor scratch files (e.g. `foo.md~`)
/// are ignored.
fn event_relevant(ev: &notify::Event) -> bool {
    if ev.paths.is_empty() {
        return true;
    }
    ev.paths
        .iter()
        .any(|p| match p.extension().and_then(|e| e.to_str()) {
            None => true,
            Some(ext) => ext.eq_ignore_ascii_case("md"),
        })
}

/// Merge-reload the catalog from disk and reconcile its schedules. When nothing
/// changed, no client push is emitted.
pub(crate) async fn reload(state: &Arc<State>) {
    let prior = state.catalog.read().clone();
    let updated = crate::repos::load_all(state, &prior);
    if updated == prior {
        return;
    }
    tracing::info!(
        before = prior.len(),
        after = updated.len(),
        "task catalog reloaded"
    );
    *state.catalog.write() = updated;

    if let Err(e) = crate::workflow::regenerate(&state.catalog.read(), &state.data_dir) {
        tracing::warn!(error = %e, "failed to write workflow.dot");
    }

    if let Err(e) = crate::scheduler::reconcile_catalog_schedules(state).await {
        tracing::warn!(error = %e, "failed to reconcile catalog schedules");
    }
    state.bus.publish(ServerPush::CatalogUpdated);
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event_bus::EventBus;
    use crate::state::StateInit;

    /// A unique scratch directory for watcher tests.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "favetto-watch-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `State` backed by a scratch SQLite database and the given tasks dir.
    async fn test_state(dir: &std::path::Path, tasks_dir: &std::path::Path) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let catalog = Arc::new(parking_lot::RwLock::new(
            crate::tasks::load_catalog(tasks_dir).unwrap(),
        ));
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
            tasks_dir: tasks_dir.to_path_buf(),
            catalog,
            scheduler,
            hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
        }))
    }

    /// Poll `cond` until it holds or `timeout` elapses.
    async fn wait_for<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn reloads_catalog_and_pushes_when_a_file_appears() {
        let dir = temp_dir("appears");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\nprompt a\n").unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        let mut rx = state.bus.subscribe();
        spawn(state.clone()).unwrap();

        std::fs::write(tasks_dir.join("b.md"), "agent = \"x\"\n---\nprompt b\n").unwrap();

        let pushed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Ok(ServerPush::CatalogUpdated) => break true,
                    Ok(_) => continue,
                    Err(_) => break false,
                }
            }
        })
        .await
        .unwrap();
        assert!(pushed, "expected a catalog.updated push");
        assert!(state.catalog.read().iter().any(|d| d.name == "b"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reload_rewrites_workflow_dot() {
        let dir = temp_dir("workflow");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("a.md"),
            "agent = \"x\"\nspawn = \"b\"\n---\nprompt a\n",
        )
        .unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        // Force a change so `reload` actually regenerates the graph.
        std::fs::write(tasks_dir.join("b.md"), "agent = \"x\"\n---\nprompt b\n").unwrap();
        super::reload(&state).await;

        let dot = std::fs::read_to_string(dir.join("workflow.dot")).unwrap();
        assert!(dot.contains("\"a\" -> \"b\" [label=\"spawn\"];"), "{dot}");
        assert!(dot.contains("\"b\" [label=\"b\"];"), "{dot}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watcher_converges_on_edit_and_delete() {
        let dir = temp_dir("converge");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\none\n").unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        spawn(state.clone()).unwrap();

        // Edit the prompt: the catalog picks up the new body.
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\ntwo\n").unwrap();
        let converged = wait_for(Duration::from_secs(5), || {
            state
                .catalog
                .read()
                .iter()
                .any(|d| d.name == "a" && d.prompt == "two")
        })
        .await;
        assert!(converged, "edited prompt should reach the catalog");

        // Delete the file: the task is dropped.
        std::fs::remove_file(tasks_dir.join("a.md")).unwrap();
        let removed = wait_for(Duration::from_secs(5), || state.catalog.read().is_empty()).await;
        assert!(removed, "deleted task should leave the catalog");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watcher_picks_up_nested_file_and_edit_and_delete() {
        let dir = temp_dir("nested");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(tasks_dir.join("pipelines")).unwrap();
        std::fs::write(
            tasks_dir.join("pipelines/plan.md"),
            "agent = \"x\"\n---\none\n",
        )
        .unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        spawn(state.clone()).unwrap();

        // Create a new file inside the subfolder: the recursive watcher sees it.
        std::fs::write(
            tasks_dir.join("pipelines/new.md"),
            "agent = \"x\"\n---\nnew\n",
        )
        .unwrap();
        let added = wait_for(Duration::from_secs(5), || {
            state
                .catalog
                .read()
                .iter()
                .any(|d| d.name == "pipelines/new")
        })
        .await;
        assert!(added, "nested file should be discovered");

        // Edit it: the catalog picks up the new body.
        std::fs::write(
            tasks_dir.join("pipelines/plan.md"),
            "agent = \"x\"\n---\ntwo\n",
        )
        .unwrap();
        let edited = wait_for(Duration::from_secs(5), || {
            state
                .catalog
                .read()
                .iter()
                .any(|d| d.name == "pipelines/plan" && d.prompt == "two")
        })
        .await;
        assert!(edited, "nested edit should reach the catalog");

        // Delete it: the task is dropped.
        std::fs::remove_file(tasks_dir.join("pipelines/plan.md")).unwrap();
        let removed = wait_for(Duration::from_secs(5), || {
            !state
                .catalog
                .read()
                .iter()
                .any(|d| d.name == "pipelines/plan")
        })
        .await;
        assert!(removed, "deleted nested task should leave the catalog");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn watcher_picks_up_a_directory_created_later() {
        let dir = temp_dir("later");
        let tasks_dir = dir.join("tasks");
        // Deliberately do not create `tasks` before starting the watcher.
        let state = test_state(&dir, &tasks_dir).await;
        spawn(state.clone()).unwrap();

        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\nprompt\n").unwrap();

        let found = wait_for(Duration::from_secs(5), || {
            state.catalog.read().iter().any(|d| d.name == "a")
        })
        .await;
        assert!(
            found,
            "task created after the watcher started should appear"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_edit_keeps_previous_definition() {
        let dir = temp_dir("invalid");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\noriginal\n").unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        let before = state.catalog.read().clone();

        // Break the header: the task must survive with its previous prompt.
        std::fs::write(tasks_dir.join("a.md"), "agent = \n---\nbroken\n").unwrap();
        super::reload(&state).await;

        assert_eq!(*state.catalog.read(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reconciles_catalog_schedules_on_reload() {
        let dir = temp_dir("schedules");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("a.md"),
            "agent = \"x\"\nschedule = \"0 0 8 * * *\"\n---\nprompt\n",
        )
        .unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        crate::scheduler::reconcile_catalog_schedules(&state)
            .await
            .unwrap();
        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        assert!(scheduled.iter().any(|s| s.id == "catalog:a"));

        // Removing the `schedule` header must retire the cron job.
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\nprompt\n").unwrap();
        super::reload(&state).await;

        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        assert!(!scheduled.iter().any(|s| s.id == "catalog:a"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn catalog_schedule_id_includes_folder() {
        let dir = temp_dir("nested-schedules");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(tasks_dir.join("pipelines")).unwrap();
        std::fs::write(
            tasks_dir.join("pipelines/plan.md"),
            "agent = \"x\"\nschedule = \"0 0 8 * * *\"\n---\nprompt\n",
        )
        .unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        crate::scheduler::reconcile_catalog_schedules(&state)
            .await
            .unwrap();
        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        let entry = scheduled
            .iter()
            .find(|s| s.id == "catalog:pipelines/plan")
            .expect("folder-qualified schedule id");
        assert_eq!(entry.task, "pipelines/plan");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reconcile_updates_changed_cron_and_keeps_manual_schedules() {
        let dir = temp_dir("reconcile");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("a.md"),
            "agent = \"x\"\nschedule = \"0 0 8 * * *\"\n---\nprompt\n",
        )
        .unwrap();

        let state = test_state(&dir, &tasks_dir).await;

        // A manually created (non-catalog) schedule must survive reconciliation.
        crate::scheduler::upsert(
            &state,
            &favetto_core::model::Schedule {
                id: "manual".to_string(),
                cron: "0 0 9 * * *".to_string(),
                task: "a".to_string(),
                input: serde_json::json!({}),
                enabled: true,
                last_run: None,
            },
        )
        .await
        .unwrap();

        crate::scheduler::reconcile_catalog_schedules(&state)
            .await
            .unwrap();
        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        assert_eq!(
            scheduled
                .iter()
                .find(|s| s.id == "catalog:a")
                .map(|s| s.cron.as_str()),
            Some("0 0 8 * * *")
        );

        // Editing the cron updates the registered `catalog:a` schedule in place.
        std::fs::write(
            tasks_dir.join("a.md"),
            "agent = \"x\"\nschedule = \"0 30 8 * * *\"\n---\nprompt\n",
        )
        .unwrap();
        super::reload(&state).await;

        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        assert_eq!(
            scheduled
                .iter()
                .find(|s| s.id == "catalog:a")
                .map(|s| s.cron.as_str()),
            Some("0 30 8 * * *")
        );
        assert!(scheduled.iter().any(|s| s.id == "manual"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod repo_tests {
    use super::*;
    use crate::event_bus::EventBus;
    use crate::state::StateInit;
    use std::time::Duration;

    /// A unique scratch directory for repo watcher tests.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "favetto-watch-repo-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn test_state(dir: &std::path::Path, tasks_dir: &std::path::Path) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let catalog = Arc::new(parking_lot::RwLock::new(
            crate::tasks::load_catalog(tasks_dir).unwrap(),
        ));
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
            tasks_dir: tasks_dir.to_path_buf(),
            catalog,
            scheduler,
            hook_store: Arc::new(parking_lot::RwLock::new(Vec::new())),
        }))
    }

    async fn wait_for<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn watcher_tracks_a_repo_added_and_removed_at_runtime() {
        let dir = temp_dir("runtime");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(tasks_dir.join("a.md"), "agent = \"x\"\n---\na\n").unwrap();

        let repo = dir.join("myrepo");
        std::fs::create_dir_all(repo.join(".favetto/tasks")).unwrap();
        std::fs::write(
            repo.join(".favetto/tasks/triage.md"),
            "agent = \"x\"\n---\nrepo\n",
        )
        .unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        spawn(state.clone()).unwrap();

        // Registering the repo immediately picks up its catalog via reload.
        crate::repos::add(
            &state,
            favetto_core::rpc::ReposAddParams {
                path: repo.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        assert!(state
            .catalog
            .read()
            .iter()
            .any(|d| d.name == "repo:myrepo/triage"));

        // The watcher re-targeted onto the repo's tasks dir: a new file appears.
        std::fs::write(
            repo.join(".favetto/tasks/second.md"),
            "agent = \"x\"\n---\nsecond\n",
        )
        .unwrap();
        let added = wait_for(Duration::from_secs(5), || {
            state
                .catalog
                .read()
                .iter()
                .any(|d| d.name == "repo:myrepo/second")
        })
        .await;
        assert!(added, "a new repo task should be discovered");

        // Removing the repo drops its tasks.
        crate::repos::remove(&state, "myrepo").await.unwrap();
        let removed = wait_for(Duration::from_secs(5), || {
            !state
                .catalog
                .read()
                .iter()
                .any(|d| d.name.starts_with("repo:myrepo/"))
        })
        .await;
        assert!(removed, "removing a repo should drop its tasks");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn repo_qualified_schedule_registers_and_retires() {
        let dir = temp_dir("schedule");
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let repo = dir.join("myrepo");
        std::fs::create_dir_all(repo.join(".favetto/tasks")).unwrap();
        std::fs::write(
            repo.join(".favetto/tasks/cron.md"),
            "agent = \"x\"\nschedule = \"0 0 8 * * *\"\n---\nbody\n",
        )
        .unwrap();

        let state = test_state(&dir, &tasks_dir).await;
        crate::repos::add(
            &state,
            favetto_core::rpc::ReposAddParams {
                path: repo.to_string_lossy().into_owned(),
                name: None,
            },
        )
        .await
        .unwrap();
        crate::scheduler::reconcile_catalog_schedules(&state)
            .await
            .unwrap();
        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        let entry = scheduled
            .iter()
            .find(|s| s.id == "catalog:repo:myrepo/cron")
            .expect("qualified catalog schedule");
        assert_eq!(entry.task, "repo:myrepo/cron");

        // Removing the repo retires its schedule.
        crate::repos::remove(&state, "myrepo").await.unwrap();
        crate::scheduler::reconcile_catalog_schedules(&state)
            .await
            .unwrap();
        let scheduled = crate::db::list_schedules(&state.db).await.unwrap();
        assert!(!scheduled.iter().any(|s| s.id.starts_with("catalog:repo:")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
