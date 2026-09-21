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

use crate::config::ExecutorSettings;
use crate::db;
use crate::event_bus::ServerPush;
use crate::state::State;
use crate::tasks::TaskDef;

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
    let outcome = match agent_name {
        Some(agent_name) => {
            run_agent_task(state, &task, &agent_name, &def, &prompt, &plan.cwd).await
        }
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

    let success = outcome.is_ok();
    match outcome {
        Ok((output, session_id)) => {
            task.status = TaskStatus::Succeeded;
            task.output = Some(output);
            task.session_id = session_id;
            task.error = None;
        }
        Err(e) => {
            task.status = TaskStatus::Failed;
            task.error = Some(e.to_string());
        }
    }
    task.finished_at = Some(Utc::now());

    let _ = db::upsert_task(&state.db, &task).await;
    state.bus.publish(ServerPush::TaskUpdated(task.clone()));

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
/// Returns the produced output and the agent's own session id, if captured.
async fn run_agent_task(
    state: &Arc<State>,
    task: &Task,
    agent_name: &str,
    def: &TaskDef,
    prompt: &str,
    cwd: &Path,
) -> anyhow::Result<(serde_json::Value, Option<String>)> {
    let agent = state.registry.get(agent_name).ok_or_else(|| {
        anyhow::anyhow!("agent '{agent_name}' is not configured under [agents.*]")
    })?;

    let ctx = crate::agents::AgentContext {
        cwd: Some(cwd.to_path_buf()),
        provider: def.provider.clone(),
        model: def.model.clone(),
        prompt: Some(prompt.to_string()),
        rows: 40,
        cols: 120,
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
    // The run's PTY only carried machine output (e.g. JSON events); drop it once
    // its session id is captured, since reattaching launches a fresh interactive
    // TUI on that session. Agents without resume keep the PTY so its final screen
    // can still be replayed.
    if result.session_id.is_some() && agent.capabilities().resume {
        let _ = state.agents.close(&info.id);
    }
    match result.exit_code {
        Some(0) => Ok((
            serde_json::json!({
                "output": result.raw,
                "agent": agent_name,
                "session_id": result.session_id,
                "result": result.output,
            }),
            result.session_id,
        )),
        Some(c) => anyhow::bail!("agent '{agent_name}' exited with {c}:\n{raw}"),
        None => anyhow::bail!("agent '{agent_name}' session disappeared"),
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
    let root = cfg
        .worktree_dir
        .clone()
        .unwrap_or_else(|| state.data_dir.join("worktrees"));
    let root = if root.is_absolute() {
        root
    } else {
        repo.join(root)
    };
    root.join(task.id.to_string())
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
    use crate::tasks::{TaskVar, VarType};

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
        };
        let ctx = render_context(&task);
        assert_eq!(crate::template::render("{{ task.name }}", &ctx), "triage");
        assert_eq!(crate::template::render("{{ input.issue_id }}", &ctx), "7");
        assert_eq!(crate::template::render("{{ prev.output }}", &ctx), "done");
    }
}
