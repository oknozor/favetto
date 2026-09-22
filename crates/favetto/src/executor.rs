//! Task executor: drains the pending task queue and runs each task through its
//! configured external agent, plus reacts to `TaskFinished` events to start tasks
//! that depend on them (`needs = "other:finished"`).
//!
//! Concurrency and isolation are configurable (`[executor]`):
//!
//! - `parallel = false` (default): one task at a time.
//! - `parallel = true`: up to `max_concurrency` tasks run at once. When a task
//!   runs inside a git repository it gets its own `git worktree` (`worktree =
//!   true`), so tasks don't step on each other. Tasks that do **not** get a
//!   worktree are serialized per working directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::process::Command;
use tokio::sync::{OwnedMutexGuard, Semaphore};
use uuid::Uuid;

use favetto_core::model::{Event, EventKind, Task, TaskStatus};

use crate::agents::{resolve_session_title, Agent, TITLE_POLL_ATTEMPTS, TITLE_POLL_INTERVAL};
use crate::config::ExecutorSettings;
use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
use crate::tasks::TaskDef;

/// The result of an agent run. `error` is `Some` for a non-zero exit or a
/// missing session; `session_id`/`session_title` are carried either way so a
/// failed run keeps its reattach handle and its title.
struct RunOutcome {
    output: serde_json::Value,
    session_id: Option<String>,
    session_title: Option<String>,
    error: Option<String>,
}

/// Fold a finished (or failed-to-start) agent run onto the task row. Returns
/// whether the run succeeded. Session info is assigned on both arms.
fn record_run_outcome(task: &mut Task, outcome: anyhow::Result<RunOutcome>) -> bool {
    match outcome {
        Ok(run) => {
            task.session_id = run.session_id;
            task.session_title = run.session_title;
            if let Some(err) = run.error {
                task.status = TaskStatus::Failed;
                task.error = Some(err);
                false
            } else {
                task.status = TaskStatus::Succeeded;
                task.output = Some(run.output);
                task.error = None;
                true
            }
        }
        Err(e) => {
            task.status = TaskStatus::Failed;
            task.error = Some(e.to_string());
            false
        }
    }
}

/// Spawn the dispatcher and the dependency listener.
pub fn spawn(state: Arc<State>) -> tokio::task::JoinHandle<()> {
    let cfg = state.config.read().unwrap().executor.clone();

    let dep_state = state.clone();
    tokio::spawn(async move {
        let mut rx = dep_state.bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(ServerPush::Event(ev)) => {
                    if ev.kind == EventKind::TaskFinished {
                        start_dependents(&dep_state, &ev).await;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "executor dependency listener lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let dispatcher = tokio::spawn(async move {
        dispatch_loop(state, cfg).await;
    });
    dispatcher
}

/// Repeatedly claim pending tasks and run them, honoring the concurrency limit and
/// per-directory serialization.
async fn dispatch_loop(state: Arc<State>, cfg: ExecutorSettings) {
    let semaphore = Arc::new(Semaphore::new(cfg.concurrency()));
    let dirs = DirLocks::default();
    let mut interval = tokio::time::interval(Duration::from_millis(500));

    loop {
        interval.tick().await;
        let limit = (cfg.concurrency() as i64) * 4;
        let pending = match db::next_pending_tasks(&state.db, limit).await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "failed to poll task queue");
                continue;
            }
        };

        for task in pending {
            // Only dispatch if a slot is free (don't block the loop).
            let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                break;
            };

            let Some(def) = lookup_def(&state, &task.name) else {
                fail_task(&state, task, "task not found in the catalog").await;
                continue;
            };

            // Required input variables must be present even for unattended starts
            // (hooks / needs / spawn / schedule / raw RPC), which never prompt.
            if let Some(missing) = missing_required_vars(&def, &task.input).first() {
                fail_task(
                    &state,
                    task,
                    &format!("task '{}' requires input variable '{missing}'", def.name),
                )
                .await;
                continue;
            }

            let base = resolve_base_dir(&def, &task);
            let plan = match make_plan(&state, &cfg, &base, &task).await {
                Ok(p) => p,
                Err(e) => {
                    fail_task(&state, task, &format!("worktree setup failed: {e}")).await;
                    continue;
                }
            };

            // Reserve the directory so same-dir tasks serialize.
            let guard = if plan.needs_lock {
                match dirs.try_lock(&base) {
                    Some(g) => Some(g),
                    None => continue, // another task owns this directory
                }
            } else {
                None
            };

            if !db::claim_task(&state.db, task.id).await.unwrap_or(false) {
                continue; // already claimed
            }

            tracing::info!(
                task_id = %task.id,
                task = %task.name,
                cwd = %plan.cwd.display(),
                worktree = plan.worktree.is_some(),
                "executing task"
            );

            let state = state.clone();
            let def = def.clone();
            tokio::spawn(async move {
                run_one(&state, task, def, plan, guard).await;
                drop(permit);
            });
        }
    }
}

/// Enqueue a task (idle), announcing it on the bus and emitting `TaskIdle`.
pub async fn enqueue_task(
    state: &State,
    name: String,
    input: serde_json::Value,
    dedupe_key: Option<String>,
) -> anyhow::Result<Task> {
    let task = Task {
        id: Uuid::new_v4(),
        name: name.clone(),
        status: TaskStatus::Pending,
        input,
        output: None,
        dedupe_key,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        error: None,
        session_id: None,
        session_title: None,
    };
    db::insert_task(&state.db, &task).await?;
    crate::metrics::inc_tasks();
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));
    state
        .emit_event(
            EventKind::TaskIdle,
            serde_json::json!({ "name": name, "task_id": task.id }),
        )
        .await;
    Ok(task)
}

async fn run_one(
    state: &Arc<State>,
    mut task: Task,
    def: TaskDef,
    plan: Plan,
    _dir_guard: Option<OwnedMutexGuard<()>>,
) {
    // `claim_task` already persisted Running; mirror it on our copy.
    task.status = TaskStatus::Running;
    task.started_at = Some(Utc::now());
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));
    state
        .emit_event(
            EventKind::TaskStarted,
            serde_json::json!({ "name": task.name, "task_id": task.id }),
        )
        .await;

    let config = state.config.read().unwrap().clone();

    let context = render_context(&task);
    let prompt = crate::template::render(&def.prompt, &context);

    let agent_name = def.agent.clone().or_else(|| config.agent.default.clone());
    let outcome = match agent_name.as_deref() {
        Some(name) => run_agent_task(state, &task, name, &def, &prompt, &plan.cwd).await,
        None => Err(anyhow::anyhow!(
            "task '{}' has no agent: set `agent` in the task or `[agent].default` in the config",
            def.name
        )),
    };

    if let Some(wt) = &plan.worktree {
        if config.executor.keep_worktree {
            tracing::info!(path = %wt.path.display(), "worktree kept");
        } else {
            remove_worktree(&wt.repo, &wt.path, &wt.branch).await;
        }
    }

    let success = record_run_outcome(&mut task, outcome);
    task.finished_at = Some(Utc::now());

    let _ = db::upsert_task(&state.db, &task).await;
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));

    // If the CLI had not written the title yet, keep polling in the background and
    // push the update when it appears, so an open TUI fills the cell live.
    if task.session_id.is_some() && task.session_title.is_none() {
        let worktree_removed = plan.worktree.is_some() && !config.executor.keep_worktree;
        let backfill_cwd = if worktree_removed {
            plan.worktree
                .as_ref()
                .map(|w| w.repo.clone())
                .unwrap_or_else(|| plan.cwd.clone())
        } else {
            plan.cwd.clone()
        };
        if let Some(agent) = agent_name.as_deref().and_then(|n| state.registry.get(n)) {
            if agent.has_session_titles() {
                if let Some(sid) = task.session_id.clone() {
                    spawn_title_backfill(state.clone(), task.id, agent, sid, backfill_cwd);
                }
            }
        }
    }

    let kind = if success {
        EventKind::TaskCompleted
    } else {
        EventKind::TaskFailed
    };
    state
        .emit_event(kind, serde_json::json!({ "task_id": task.id }))
        .await;
    state
        .emit_event(
            EventKind::TaskFinished,
            serde_json::json!({ "name": task.name, "task_id": task.id, "success": success }),
        )
        .await;

    if success && def.spawn.is_some() {
        if let Err(e) = spawn_from_manifest(state, &task, &def, &plan.cwd).await {
            tracing::warn!(task = %task.name, error = %e, "failed to spawn tasks from handoff");
        }
    }
}

/// Build the template context for a task's prompt and handoff path. `prev` is
/// populated from `input._prev`, which the `needs` dependency listener attaches.
fn render_context(task: &Task) -> serde_json::Value {
    let mut ctx = serde_json::json!({
        "task": {
            "id": task.id.to_string(),
            "name": task.name,
        },
        "input": task.input,
    });
    if let Some(prev) = task.input.get("_prev") {
        ctx["prev"] = prev.clone();
    }
    ctx
}

/// Read the task's `spawn_file` (rendered template), parse it as a JSON array,
/// and enqueue one `spawn` task per element (the element becomes its `input`). An
/// empty array is a no-op. A non-array JSON value is treated as a single element.
async fn spawn_from_manifest(
    state: &Arc<State>,
    task: &Task,
    def: &TaskDef,
    cwd: &Path,
) -> anyhow::Result<()> {
    let spawn_task = def
        .spawn
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task '{}' has no `spawn` task", def.name))?;
    let spawn_file = def
        .spawn_file
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("task '{}' sets `spawn` but no `spawn_file`", def.name))?;

    let rendered = crate::template::render(spawn_file, &render_context(task));
    let path = if Path::new(&rendered).is_absolute() {
        PathBuf::from(&rendered)
    } else {
        cwd.join(&rendered)
    };

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read spawn_file {}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("spawn_file {} is not valid JSON: {e}", path.display()))?;

    let items = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    };
    if items.is_empty() {
        tracing::info!(task = %task.name, "spawn manifest is empty; nothing to enqueue");
        return Ok(());
    }

    for (i, item) in items.into_iter().enumerate() {
        let dedupe = format!("spawn:{}:{i}", task.id);
        match enqueue_task(state, spawn_task.to_string(), item, Some(dedupe)).await {
            Ok(enqueued) => {
                tracing::info!(task = %task.name, spawned = %enqueued.id, "spawned task");
            }
            Err(e) => tracing::warn!(error = %e, "failed to enqueue spawned task"),
        }
    }
    Ok(())
}

/// Run a task through its configured external agent as a real PTY session bound
/// to the task, so it can be reattached from the TUI and its output captured.
/// Returns the produced output plus the agent's own session id and its session
/// title (both optional), even when the run failed.
async fn run_agent_task(
    state: &Arc<State>,
    task: &Task,
    agent_name: &str,
    def: &TaskDef,
    prompt: &str,
    cwd: &Path,
) -> anyhow::Result<RunOutcome> {
    let agent = state.registry.get_checked(agent_name)?;

    let ctx = crate::agents::AgentContext {
        cwd: Some(cwd.to_path_buf()),
        provider: def.provider.clone(),
        model: def.model.clone(),
        prompt: Some(prompt.to_string()),
        rows: 40,
        cols: 120,
        git_signing: def.sign,
        ..Default::default()
    };
    let info = state.agents.start(
        agent_name,
        agent.clone(),
        Some(task.id.to_string()),
        crate::agents::Invocation::Headless {
            prompt,
            provider: def.provider.as_deref(),
            model: def.model.as_deref(),
        },
        ctx,
    )?;

    let code = state.agents.wait(&info.id).await;
    let raw = state.agents.output(&info.id);
    let mut result = agent.parse_output(&raw, code);
    // The manager also captures the id live from the PTY; prefer it if the
    // implementation's parser did not find one.
    if result.session_id.is_none() {
        result.session_id = state.agents.external_session_id(&info.id);
    }
    // Resolve the title with a bounded retry (a subprocess lookup in most CLIs)
    // before the worktree is torn down; a failure or an agent without titles
    // degrades to `None`.
    let session_title = match result.session_id.clone() {
        Some(sid) => {
            resolve_session_title(
                agent.clone(),
                &sid,
                cwd,
                TITLE_POLL_ATTEMPTS,
                TITLE_POLL_INTERVAL,
            )
            .await
        }
        None => None,
    };
    // The run's PTY only carried machine output (e.g. JSON events); drop it once
    // its session id is captured, since reattaching launches a fresh interactive
    // TUI on that session. Agents without resume keep the PTY so its final screen
    // can still be replayed.
    if result.session_id.is_some() && agent.capabilities().resume {
        let _ = state.agents.close(&info.id);
    }
    let output = serde_json::json!({
        "output": result.raw,
        "agent": agent_name,
        "session_id": result.session_id,
        "session_title": session_title,
        "result": result.output,
    });
    let error = match result.exit_code {
        Some(0) => None,
        Some(c) => Some(format!("agent '{agent_name}' exited with {c}:\n{raw}")),
        None => Some(format!("agent '{agent_name}' session disappeared")),
    };
    Ok(RunOutcome {
        output,
        session_id: result.session_id,
        session_title,
        error,
    })
}

/// Background title poll window (after the synchronous poll gave up): ~60 s.
const BACKFILL_ATTEMPTS: u32 = 30;
const BACKFILL_INTERVAL: Duration = Duration::from_secs(2);

/// Poll for a missing session title in the background and persist the first one
/// that appears, publishing a `TaskUpdated` so an open TUI fills the cell live.
fn spawn_title_backfill(
    state: Arc<State>,
    task_id: Uuid,
    agent: Arc<dyn Agent>,
    session_id: String,
    cwd: PathBuf,
) {
    tokio::spawn(async move {
        backfill_title(
            &state,
            task_id,
            agent,
            session_id,
            cwd,
            BACKFILL_ATTEMPTS,
            BACKFILL_INTERVAL,
        )
        .await;
    });
}

/// The body of [`spawn_title_backfill`], split out so it can be awaited in tests.
async fn backfill_title(
    state: &Arc<State>,
    task_id: Uuid,
    agent: Arc<dyn Agent>,
    session_id: String,
    cwd: PathBuf,
    attempts: u32,
    interval: Duration,
) {
    let Some(title) = resolve_session_title(agent, &session_id, &cwd, attempts, interval).await
    else {
        return;
    };
    let Ok(Some(mut task)) = db::get_task(&state.db, task_id).await else {
        return;
    };
    if task.session_title.is_some() {
        return; // another path won the race
    }
    task.session_title = Some(title);
    if db::upsert_task(&state.db, &task).await.is_ok() {
        state.bus.publish(ServerPush::TaskUpdated(task));
    }
}

/// Mark a task failed (used when it can't even be started).
async fn fail_task(state: &State, task: Task, error: &str) {
    let mut task = task;
    task.status = TaskStatus::Failed;
    task.error = Some(error.to_string());
    task.finished_at = Some(Utc::now());
    let _ = db::upsert_task(&state.db, &task).await;
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));
    state
        .emit_event(
            EventKind::TaskFinished,
            serde_json::json!({ "name": task.name, "task_id": task.id, "success": false }),
        )
        .await;
}

fn lookup_def(state: &State, name: &str) -> Option<TaskDef> {
    state
        .catalog
        .read()
        .unwrap()
        .iter()
        .find(|d| d.name == name)
        .cloned()
}

/// Names of the task's `required` input variables that are absent, JSON `null`,
/// or an empty string in `input`. Optional omitted/empty values are left alone.
pub fn missing_required_vars(def: &TaskDef, input: &serde_json::Value) -> Vec<String> {
    def.vars
        .iter()
        .filter(|v| v.required)
        .filter(|v| match input.get(&v.name) {
            None | Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.is_empty(),
            _ => false,
        })
        .map(|v| v.name.clone())
        .collect()
}

/// The directory a task starts from: per-run `input.cwd`, else the task's `cwd`,
/// else the daemon's working directory.
fn resolve_base_dir(def: &TaskDef, task: &Task) -> PathBuf {
    task.input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| def.cwd.as_ref().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A concrete execution directory plan.
struct Plan {
    cwd: PathBuf,
    /// Serialize same-directory tasks (no worktree isolation).
    needs_lock: bool,
    worktree: Option<Worktree>,
}

struct Worktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
}

/// Choose the task's working directory: a fresh git worktree when parallel +
/// `worktree` and the base dir is inside a repository, otherwise the base dir.
async fn make_plan(
    state: &State,
    cfg: &ExecutorSettings,
    base: &Path,
    task: &Task,
) -> anyhow::Result<Plan> {
    if cfg.parallel && cfg.worktree {
        if let Some(repo) = git_toplevel(base).await {
            let path = worktree_path(state, cfg, &repo, task);
            let branch = branch_name(task);
            create_worktree(&repo, &path, &branch).await?;
            return Ok(Plan {
                cwd: path.clone(),
                needs_lock: false,
                worktree: Some(Worktree { repo, path, branch }),
            });
        }
    }
    Ok(Plan {
        cwd: base.to_path_buf(),
        needs_lock: true,
        worktree: None,
    })
}

async fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn branch_name(task: &Task) -> String {
    let mut slug: String = task
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    slug.truncate(40);
    let short = task.id.to_string();
    format!("favetto/{slug}-{}", &short[..8])
}

fn worktree_path(state: &State, cfg: &ExecutorSettings, repo: &Path, task: &Task) -> PathBuf {
    worktree_root(cfg, &state.data_dir, repo).join(task.id.to_string())
}

/// Resolve the configured worktree root. A leading `~` is expanded to the home
/// directory; an absolute path is used as-is; a non-`~` relative path (the
/// documented "repo-relative" form) is resolved against the repo root. Unset
/// falls back to `<data_dir>/worktrees`.
fn worktree_root(cfg: &ExecutorSettings, data_dir: &Path, repo: &Path) -> PathBuf {
    let root = cfg
        .worktree_dir
        .clone()
        .map(crate::paths::expand_tilde)
        .unwrap_or_else(|| data_dir.join("worktrees"));
    if root.is_absolute() {
        root
    } else {
        repo.join(root)
    }
}

async fn create_worktree(repo: &Path, path: &Path, branch: &str) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(()); // reuse a leftover worktree for this task
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--force", "-B", branch])
        .arg(path)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn remove_worktree(repo: &Path, path: &Path, branch: &str) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .await;
    if let Err(e) = out {
        tracing::warn!(error = %e, path = %path.display(), "failed to remove worktree");
    }
    // Drop the per-task branch as well (the task opted out of keeping work).
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "-D", branch])
        .output()
        .await;
}

/// Per-directory locks, so tasks without worktree isolation serialize.
#[derive(Clone, Default)]
struct DirLocks(Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>);

impl DirLocks {
    fn try_lock(&self, dir: &Path) -> Option<OwnedMutexGuard<()>> {
        let lock = self
            .0
            .lock()
            .unwrap()
            .entry(dir.to_path_buf())
            .or_default()
            .clone();
        lock.try_lock_owned().ok()
    }
}

/// Start every catalog task whose `needs` matches `name:finished`.
///
/// `needs` (and `spawn`) values are relative-path names, so a dependency on a
/// task in a subfolder is written as `pipelines/plan:finished`.
async fn start_dependents(state: &Arc<State>, ev: &Event) {
    let Some(name) = ev.payload.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    let dependents: Vec<String> = state
        .catalog
        .read()
        .unwrap()
        .iter()
        .filter(|d| d.needs.as_deref() == Some(&format!("{name}:finished")))
        .map(|d| d.name.clone())
        .collect();

    // Attach the finished task's result as `input._prev` so dependent prompts can
    // reach it as `{{ prev.output }}` / `{{ prev.session_id }}`.
    let input = match ev
        .payload
        .get("task_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => match db::get_task(&state.db, id).await {
            Ok(Some(prev)) => serde_json::json!({
                "_prev": {
                    "name": prev.name,
                    "task_id": prev.id.to_string(),
                    "status": prev.status.as_str(),
                    "output": prev.output,
                    "session_id": prev.session_id,
                }
            }),
            _ => serde_json::json!({}),
        },
        None => serde_json::json!({}),
    };

    for dep in dependents {
        tracing::info!(task = %dep, trigger = %name, "auto-starting dependent task");
        let dedupe = format!("needs:{}:{}", dep, ev.id);
        if let Err(e) = enqueue_task(state, dep, input.clone(), Some(dedupe)).await {
            tracing::warn!(error = %e, "failed to enqueue dependent task");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentManager, AgentRegistry};
    use crate::config::FavettoConfig;
    use crate::event_bus::EventBus;
    use crate::tasks::{TaskVar, VarType};
    use crate::webhooks::WebhookSecrets;
    use favetto_core::auth::Token;
    use std::sync::RwLock;

    fn def_with_vars() -> TaskDef {
        let var = |name: &str, required: bool| TaskVar {
            name: name.to_string(),
            prompt: "p".to_string(),
            default: None,
            required,
            multiline: false,
            var_type: VarType::String,
            choices: None,
        };
        TaskDef {
            name: "t".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            needs: None,
            spawn: None,
            spawn_file: None,
            sign: None,
            vars: vec![var("required_one", true), var("optional_one", false)],
            prompt: "hello".to_string(),
        }
    }

    #[test]
    fn missing_required_vars_detects_absent_null_and_empty() {
        let def = def_with_vars();
        assert_eq!(
            missing_required_vars(&def, &serde_json::json!({})),
            ["required_one"]
        );
        assert_eq!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": null })),
            ["required_one"]
        );
        assert_eq!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": "" })),
            ["required_one"]
        );
        // Present values (including typed ones) satisfy the requirement.
        assert!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": "x" })).is_empty()
        );
        assert!(missing_required_vars(&def, &serde_json::json!({ "required_one": 3 })).is_empty());
        // Optional vars never appear.
        assert!(
            missing_required_vars(&def, &serde_json::json!({ "required_one": "x" })).is_empty()
        );
    }

    #[test]
    fn worktree_root_expands_tilde_and_resolves_relative() {
        let home = dirs::home_dir().expect("home directory");
        let data_dir = Path::new("/data");
        let repo = Path::new("/repo");
        let root_for = |dir: Option<&str>| {
            let cfg = ExecutorSettings {
                worktree_dir: dir.map(PathBuf::from),
                ..Default::default()
            };
            worktree_root(&cfg, data_dir, repo)
        };

        // `~` expands to the home directory before the absolute/relative decision,
        // so it is never joined onto the repo root.
        assert_eq!(
            root_for(Some("~/.local/share/favetto/worktrees")),
            home.join(".local/share/favetto/worktrees")
        );

        // A non-`~` relative value stays repo-relative.
        assert_eq!(
            root_for(Some(".favetto/worktrees")),
            Path::new("/repo/.favetto/worktrees")
        );

        // An absolute value is used as-is.
        assert_eq!(
            root_for(Some("/abs/worktrees")),
            Path::new("/abs/worktrees")
        );

        // Unset falls back to `<data_dir>/worktrees`.
        assert_eq!(root_for(None), Path::new("/data/worktrees"));
    }

    #[test]
    fn render_context_exposes_task_input_and_prev() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "triage".to_string(),
            status: TaskStatus::Succeeded,
            input: serde_json::json!({
                "issue_id": 7,
                "_prev": { "output": "done", "name": "triage" },
            }),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
        };
        let ctx = render_context(&task);
        assert_eq!(crate::template::render("{{ task.name }}", &ctx), "triage");
        assert_eq!(crate::template::render("{{ input.issue_id }}", &ctx), "7");
        assert_eq!(crate::template::render("{{ prev.output }}", &ctx), "done");
    }

    #[tokio::test]
    async fn unavailable_agent_fails_with_clear_error() {
        let dir = std::env::temp_dir().join(format!("favetto-executor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let mut cfg = FavettoConfig::default();
        cfg.agent.default = Some("opencode".to_string());
        let registry = AgentRegistry::from_config_with(&cfg, &|_| false).unwrap();

        let state = Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            dir.clone(),
            dir.clone(),
            Arc::new(RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ));

        let task = Task {
            id: Uuid::new_v4(),
            name: "oneshot".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
        };
        let def = def_with_vars();
        let err = run_agent_task(&state, &task, "opencode", &def, "prompt", Path::new("/tmp"))
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent 'opencode' is not installed"), "{err}");
        assert!(err.contains("not found on PATH"), "{err}");
    }

    fn task_with_session(session_id: Option<&str>, session_title: Option<&str>) -> Task {
        Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Running,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: session_id.map(str::to_string),
            session_title: session_title.map(str::to_string),
        }
    }

    #[test]
    fn failed_run_preserves_session_info() {
        let mut task = task_with_session(None, None);
        let success = record_run_outcome(
            &mut task,
            Ok(RunOutcome {
                output: serde_json::json!({}),
                session_id: Some("ses_1".to_string()),
                session_title: Some("Fix the widget".to_string()),
                error: Some("exit 1".to_string()),
            }),
        );
        assert!(!success);
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.session_id.as_deref(), Some("ses_1"));
        assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
        assert!(task.error.as_deref().unwrap().contains("exit 1"));
        assert!(task.output.is_none());
    }

    #[test]
    fn successful_run_records_output_and_session() {
        let mut task = task_with_session(None, None);
        task.error = Some("stale".to_string());
        let success = record_run_outcome(
            &mut task,
            Ok(RunOutcome {
                output: serde_json::json!({ "ok": true }),
                session_id: Some("ses_1".to_string()),
                session_title: Some("Fix the widget".to_string()),
                error: None,
            }),
        );
        assert!(success);
        assert_eq!(task.status, TaskStatus::Succeeded);
        assert_eq!(task.output, Some(serde_json::json!({ "ok": true })));
        assert_eq!(task.session_id.as_deref(), Some("ses_1"));
        assert_eq!(task.session_title.as_deref(), Some("Fix the widget"));
        assert!(task.error.is_none());
    }

    #[test]
    fn run_without_agent_still_fails() {
        let mut task = task_with_session(Some("ses_keep"), Some("Keep"));
        let success = record_run_outcome(&mut task, Err(anyhow::anyhow!("no agent")));
        assert!(!success);
        assert_eq!(task.status, TaskStatus::Failed);
        // A pre-run failure never drops an already-resolved session.
        assert_eq!(task.session_id.as_deref(), Some("ses_keep"));
        assert_eq!(task.session_title.as_deref(), Some("Keep"));
        assert!(task.error.as_deref().unwrap().contains("no agent"));
    }

    /// A fake `opencode session list --format json` executable that ignores its
    /// arguments and prints `json`.
    #[cfg(unix)]
    fn session_list_script(dir: &Path, json: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("opencode-session-list.sh");
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s' '{json}'\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A `State` whose `opencode` command is `command` (a title-lookup fixture).
    #[cfg(unix)]
    async fn title_state(dir: &Path, command: &Path) -> Arc<State> {
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let mut cfg = FavettoConfig::default();
        cfg.agent.default = Some("opencode".to_string());
        cfg.agents.insert(
            "opencode".to_string(),
            crate::config::AgentConfig {
                command: command.to_string_lossy().into_owned(),
                ..Default::default()
            },
        );
        let registry = AgentRegistry::from_config_with(&cfg, &|_| true).unwrap();
        Arc::new(State::new(
            pool,
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: None,
                linear: None,
            },
            AgentManager::new(),
            registry,
            Arc::new(RwLock::new(cfg)),
            dir.to_path_buf(),
            dir.to_path_buf(),
            Arc::new(RwLock::new(Vec::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_title_fills_blank_session_title() {
        let dir = std::env::temp_dir().join(format!("favetto-backfill-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"One shot title"}]"#);
        let state = title_state(&dir, &script).await;

        let task = task_with_session(Some("ses_1"), None);
        db::insert_task(&state.db, &task).await.unwrap();

        let agent = state.registry.get("opencode").unwrap();
        backfill_title(
            &state,
            task.id,
            agent,
            "ses_1".to_string(),
            dir.clone(),
            3,
            Duration::from_millis(1),
        )
        .await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.session_title.as_deref(), Some("One shot title"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_title_does_not_overwrite_existing_title() {
        let dir = std::env::temp_dir().join(format!("favetto-backfill-keep-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = session_list_script(&dir, r#"[{"id":"ses_1","title":"One shot title"}]"#);
        let state = title_state(&dir, &script).await;

        let task = task_with_session(Some("ses_1"), Some("Existing"));
        db::insert_task(&state.db, &task).await.unwrap();

        let agent = state.registry.get("opencode").unwrap();
        backfill_title(
            &state,
            task.id,
            agent,
            "ses_1".to_string(),
            dir.clone(),
            2,
            Duration::from_millis(1),
        )
        .await;

        let stored = db::get_task(&state.db, task.id).await.unwrap().unwrap();
        assert_eq!(stored.session_title.as_deref(), Some("Existing"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_name_slugifies_folder_qualified_task() {
        let task = Task {
            id: Uuid::new_v4(),
            name: "pipelines/plan".to_string(),
            status: TaskStatus::Pending,
            input: serde_json::json!({}),
            output: None,
            dedupe_key: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            error: None,
            session_id: None,
            session_title: None,
        };
        let short = &task.id.to_string()[..8];
        assert_eq!(
            branch_name(&task),
            format!("favetto/pipelines-plan-{short}")
        );
    }
}
