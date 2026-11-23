//! Durable metadata for the ACP sessions Favetto creates.
//!
//! The agent remains the source of truth for conversation history. This store
//! records only identity and lifecycle: the Favetto-local id used by RPCs, the
//! configured agent, the working directory, optional task/run linkage, the
//! agent's own ACP id, a display title, and the current status. It never stores a
//! transcript, environment, or credentials.
//!
//! A record is inserted as `loading` before the ACP process starts. The agent's
//! session id and `ready` are written together once `session/new` returns, so a
//! row is never advertised as resumable without the id needed to resume it.
//! Closing or failing updates the row in place: history is never deleted by
//! teardown.

use chrono::Utc;
use sqlx::SqlitePool;

use crate::db::{self, AgentSessionRecord, AgentSessionStatus};

/// The durable writer attached to [`super::AgentManager`].
#[derive(Clone)]
pub(crate) struct SessionStore {
    pool: SqlitePool,
}

/// A session's durable identity plus the run it is backing, if any.
#[derive(Clone)]
pub(crate) struct SessionBinding {
    pub store: SessionStore,
    /// Favetto's local id, never the agent's ACP id.
    pub id: String,
    /// The execution attempt this session belongs to, for a task run.
    pub run_id: Option<String>,
}

impl SessionStore {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert the launch record. A failure here aborts the launch: without a
    /// durable row the session must not run untracked.
    pub(crate) async fn create(&self, record: &AgentSessionRecord) -> anyhow::Result<()> {
        db::insert_agent_session(&self.pool, record).await
    }

    /// Persist the agent's ACP id, mark the session ready, and attach the id to
    /// the backing run. The session id and readiness are one write so a failed
    /// update leaves the row `loading`, never falsely resumable.
    pub(crate) async fn mark_ready(
        &self,
        id: &str,
        external_session_id: &str,
        run_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let written =
            db::mark_agent_session_ready(&self.pool, id, external_session_id, Utc::now()).await?;
        if !written {
            anyhow::bail!("session record '{id}' was not found while recording its ACP id");
        }
        if let Some(run_id) = run_id {
            match db::set_task_run_session_id(&self.pool, run_id, external_session_id).await {
                Ok(true) => {}
                Ok(false) => tracing::warn!(
                    session = %id,
                    run_id,
                    "task run vanished before its ACP session id was recorded"
                ),
                Err(error) => tracing::warn!(
                    session = %id,
                    run_id,
                    error = %error,
                    "failed to record the ACP session id on the task run; finalization will retry"
                ),
            }
        }
        Ok(())
    }

    /// Mark an existing record as being restored. `session/load` is in flight,
    /// so the record must not be advertised as ready until it succeeds.
    pub(crate) async fn mark_loading(&self, id: &str) -> anyhow::Result<()> {
        let written = db::set_agent_session_status(
            &self.pool,
            id,
            AgentSessionStatus::Loading,
            None,
            Utc::now(),
        )
        .await?;
        if !written {
            anyhow::bail!("session record '{id}' was not found while starting its load");
        }
        Ok(())
    }

    /// Record a title reported by the agent. Title updates are display-only, so
    /// a missing row or write fault is logged rather than ending the session.
    pub(crate) async fn set_title(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let written = db::set_agent_session_title(&self.pool, id, Some(title), Utc::now()).await?;
        if !written {
            anyhow::bail!("session record '{id}' was not found while recording its title");
        }
        Ok(())
    }

    /// The local record already imported for a provider session, keyed by the
    /// configured agent and the provider's own ACP id.
    pub(crate) async fn find_by_external(
        &self,
        agent: &str,
        external_session_id: &str,
    ) -> anyhow::Result<Option<AgentSessionRecord>> {
        db::get_agent_session_by_external(&self.pool, agent, external_session_id).await
    }

    /// Mark the session closed, keeping the row for history.
    pub(crate) async fn mark_ended(&self, id: &str) -> anyhow::Result<()> {
        if !db::close_agent_session(&self.pool, id, Utc::now()).await? {
            tracing::debug!(session = %id, "session was already terminal when it closed");
        }
        Ok(())
    }

    /// Mark the session failed with the reason, keeping the row for history.
    pub(crate) async fn mark_failed(&self, id: &str, error: &str) -> anyhow::Result<()> {
        if !db::fail_agent_session(&self.pool, id, error, Utc::now()).await? {
            tracing::debug!(session = %id, "session was already terminal when it failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::{DateTime, Utc};
    use favetto_core::model::{RunStatus, Task, TaskRun, TaskStatus};
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use super::*;
    use crate::db::{self, AgentSessionStatus};

    async fn scratch_pool(tag: &str) -> (PathBuf, SqlitePool) {
        let dir = std::env::temp_dir().join(format!(
            "favetto-{tag}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::open(&dir.join("test.db")).await.unwrap();
        db::migrate(&pool).await.unwrap();
        (dir, pool)
    }

    fn loading(id: &str, now: DateTime<Utc>) -> AgentSessionRecord {
        AgentSessionRecord::loading(id, "echo", Path::new("/work"), now)
    }

    #[tokio::test]
    async fn a_failed_ready_write_does_not_advertise_the_session_as_resumable() {
        let (dir, pool) = scratch_pool("session-ready-failure").await;
        let now = Utc::now();
        db::insert_agent_session(&pool, &loading("local-1", now))
            .await
            .unwrap();
        // Simulate a metadata write fault without removing the row.
        sqlx::query(
            "CREATE TRIGGER fail_ready BEFORE UPDATE ON agent_sessions
             BEGIN SELECT RAISE(ABORT, 'disk full'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let store = SessionStore::new(pool.clone());
        let error = store
            .mark_ready("local-1", "ses_acp_1", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("disk full"), "{error}");

        sqlx::query("DROP TRIGGER fail_ready")
            .execute(&pool)
            .await
            .unwrap();
        let stored = db::get_agent_session(&pool, "local-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.status,
            AgentSessionStatus::Loading,
            "a failed write must leave the row non-resumable"
        );
        assert!(stored.external_session_id.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ready_records_the_acp_id_and_the_run_link() {
        let (dir, pool) = scratch_pool("session-ready-run").await;
        let now = Utc::now();
        db::insert_agent_session(&pool, &loading("local-1", now))
            .await
            .unwrap();

        let task = Task {
            id: Uuid::new_v4(),
            name: "t".to_string(),
            status: TaskStatus::Running,
            attempt: 1,
            input: serde_json::Value::Null,
            output: None,
            dedupe_key: None,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            error: None,
            failure: None,
            session_id: None,
            session_title: None,
            parent_id: None,
            root_id: None,
            interactive: false,
            agent: None,
            provider: None,
            model: None,
        };
        db::insert_task(&pool, &task).await.unwrap();
        let run = TaskRun {
            id: Uuid::new_v4(),
            task_id: task.id,
            attempt: 1,
            status: RunStatus::Running,
            agent: Some("echo".to_string()),
            model: None,
            session_id: None,
            started_at: Some(now),
            finished_at: None,
            exit_code: None,
            error: None,
            usage: None,
            failure: None,
        };
        db::insert_task_run(&pool, &run).await.unwrap();

        let store = SessionStore::new(pool.clone());
        store
            .mark_ready("local-1", "ses_acp_1", Some(&run.id.to_string()))
            .await
            .unwrap();

        let stored = db::get_agent_session(&pool, "local-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, AgentSessionStatus::Ready);
        assert_eq!(stored.external_session_id.as_deref(), Some("ses_acp_1"));
        assert_ne!(stored.id, "ses_acp_1", "local and ACP ids stay separate");

        let stored_run = db::get_task_run(&pool, run.id).await.unwrap().unwrap();
        assert_eq!(stored_run.session_id.as_deref(), Some("ses_acp_1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn closing_and_failing_keep_the_metadata() {
        let (dir, pool) = scratch_pool("session-terminal").await;
        let now = Utc::now();
        db::insert_agent_session(&pool, &loading("ended", now))
            .await
            .unwrap();
        db::insert_agent_session(&pool, &loading("failed", now))
            .await
            .unwrap();
        let store = SessionStore::new(pool.clone());

        store.mark_ended("ended").await.unwrap();
        store.mark_failed("failed", "boom").await.unwrap();

        let ended = db::get_agent_session(&pool, "ended")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ended.status, AgentSessionStatus::Ended);
        let failed = db::get_agent_session(&pool, "failed")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, AgentSessionStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert_eq!(db::list_agent_sessions(&pool, 10).await.unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failed_ready_write_reports_a_deleted_record() {
        let (dir, pool) = scratch_pool("session-ready-missing").await;
        let store = SessionStore::new(pool);
        assert!(store
            .mark_ready("missing", "ses_acp_1", None)
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
