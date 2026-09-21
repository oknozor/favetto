//! Webhook receivers for GitHub and Linear.
//!
//! Both endpoints verify the provider's HMAC-SHA256 signature before emitting an
//! event on the bus (which also persists it). Signature verification happens on the
//! raw body bytes, so the handlers read `Bytes` and pass it straight to the verifier.
//!
//! - GitHub: `X-Hub-Signature-256: sha256=<hex>`. The secret comes from
//!   `[webhook.github]` (literal `secret`, else the env var named by `secret_env`,
//!   else `GITHUB_WEBHOOK_SECRET`). Signed deliveries are summarised (never the raw
//!   body), persisted as an integration event, and matched against
//!   `[[webhook.github.rules]]` entries that enqueue catalog tasks. Deliveries are
//!   idempotent on `X-GitHub-Delivery`.
//! - Linear: `Linear-Signature: <hex>` keyed by `LINEAR_WEBHOOK_SECRET`.

use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use favetto_core::model::EventKind;

use crate::config::{FavettoConfig, GithubFilter, GithubRule};
use crate::state::State;
use crate::tasks::TaskDef;

type HmacSha256 = Hmac<Sha256>;

/// Largest number of bytes retained for a summarised title / body.
const SUMMARY_TITLE_MAX_BYTES: usize = 256;
const SUMMARY_BODY_MAX_BYTES: usize = 1024;

/// Default env var holding the GitHub signing secret.
const DEFAULT_GITHUB_SECRET_ENV: &str = "GITHUB_WEBHOOK_SECRET";

/// Webhook secrets, resolved from config + environment at daemon startup.
#[derive(Default, Clone)]
pub struct WebhookSecrets {
    pub github: Option<String>,
    pub linear: Option<String>,
}

impl WebhookSecrets {
    /// Resolve secrets from config + environment.
    ///
    /// GitHub precedence: literal `secret`, else the env var named by
    /// `secret_env` (default `GITHUB_WEBHOOK_SECRET`), else that same fallback.
    /// Linear always comes from `LINEAR_WEBHOOK_SECRET`.
    pub fn from_config(cfg: &FavettoConfig) -> Self {
        let gh = &cfg.webhook.github;
        let github = gh
            .secret
            .clone()
            .or_else(|| {
                gh.secret_env
                    .as_deref()
                    .and_then(|var| std::env::var(var).ok())
            })
            .or_else(|| std::env::var(DEFAULT_GITHUB_SECRET_ENV).ok());
        Self {
            github,
            linear: std::env::var("LINEAR_WEBHOOK_SECRET").ok(),
        }
    }
}

/// Webhook sub-routes. Merged into the daemon's axum app; handlers pull shared state
/// via `State<Arc<State>>`.
pub fn routes() -> Router<Arc<State>> {
    Router::new()
        .route("/webhooks/github", post(github_webhook))
        .route("/webhooks/linear", post(linear_webhook))
}

async fn github_webhook(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_github(&state, &headers, &body).await
}

/// GitHub webhook handler, factored out so it is directly testable.
async fn handle_github(state: &State, headers: &HeaderMap, body: &[u8]) -> Response {
    let gh = state.config.read().unwrap().webhook.github.clone();
    if !gh.enabled {
        return (StatusCode::NOT_FOUND, "github webhooks disabled").into_response();
    }

    let Some(secret) = state.webhooks.github.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GITHUB_WEBHOOK_SECRET not configured",
        )
            .into_response();
    };

    let sig = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing signature").into_response(),
    };

    if !verify_signature(secret, body, sig.strip_prefix("sha256=").unwrap_or(sig)) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    let payload: Value = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON").into_response(),
    };

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let action = payload.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let delivery = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());

    // Idempotency: a redelivered delivery is ack'd without persisting or enqueuing.
    if let Some(delivery_id) = delivery {
        match crate::db::record_delivery(&state.db, "github", delivery_id).await {
            Ok(true) => {}
            Ok(false) => return StatusCode::OK.into_response(),
            Err(e) => {
                tracing::warn!(error = %e, "failed to record webhook delivery");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to record delivery",
                )
                    .into_response();
            }
        }
    }

    let summary = summarize_github(event, &payload);

    if let Some(kind) = github_event_kind(event, action, &payload) {
        state.emit_event(kind, summary.clone()).await;
    }

    for rule in &gh.rules {
        if !rule.matches(event, action, &summary) {
            continue;
        }
        let dedupe = delivery.map(|d| format!("webhook:{d}:{}", rule.name));
        match crate::executor::enqueue_task(state, rule.task.clone(), summary.clone(), dedupe).await
        {
            Ok(enqueued) => {
                tracing::info!(
                    rule = %rule.name,
                    task = %rule.task,
                    task_id = %enqueued.id,
                    "webhook rule enqueued task"
                );
            }
            Err(e) => {
                tracing::warn!(rule = %rule.name, error = %e, "webhook rule failed to enqueue task")
            }
        }
    }

    StatusCode::OK.into_response()
}

async fn linear_webhook(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(secret) = state.webhooks.linear.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "LINEAR_WEBHOOK_SECRET not configured",
        )
            .into_response();
    };

    let sig = match headers
        .get("linear-signature")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing signature").into_response(),
    };

    if !verify_signature(secret, &body, sig) {
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON").into_response(),
    };

    let action = payload.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let kind = match action {
        "create" => Some(EventKind::TicketCreated),
        "update" => Some(EventKind::TicketUpdated),
        _ => None,
    };

    if let Some(kind) = kind {
        state.emit_event(kind, payload).await;
    }

    StatusCode::OK.into_response()
}

/// Truncate on a UTF-8 char boundary to at most `max_bytes` bytes.
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Reduce a GitHub payload to the fields rules and task prompts care about. All
/// keys are present; missing values are `null`, an empty string, or an empty array.
/// The title/body are truncated so the persisted summary stays small.
fn summarize_github(event: &str, payload: &Value) -> Value {
    let subject = payload.get("issue").or_else(|| payload.get("pull_request"));
    let repo = payload.get("repository");
    let pull_request = payload.get("pull_request");

    let number = subject
        .and_then(|s| s.get("number"))
        .cloned()
        .unwrap_or(Value::Null);
    let title = subject
        .and_then(|s| s.get("title"))
        .and_then(|t| t.as_str())
        .map(|t| truncate_utf8(t, SUMMARY_TITLE_MAX_BYTES))
        .unwrap_or_default();
    let body = subject
        .and_then(|s| s.get("body"))
        .and_then(|b| b.as_str())
        .or_else(|| {
            payload
                .get("comment")
                .and_then(|c| c.get("body"))
                .and_then(|b| b.as_str())
        })
        .map(|b| truncate_utf8(b, SUMMARY_BODY_MAX_BYTES))
        .unwrap_or_default();
    let author = payload
        .get("sender")
        .and_then(|s| s.get("login"))
        .and_then(|l| l.as_str())
        .unwrap_or("");
    let html_url = subject
        .and_then(|s| s.get("html_url"))
        .and_then(|u| u.as_str())
        .or_else(|| {
            repo.and_then(|r| r.get("html_url"))
                .and_then(|u| u.as_str())
        })
        .unwrap_or("");
    let labels = subject
        .and_then(|s| s.get("labels"))
        .and_then(|l| l.as_array())
        .map(|arr| {
            Value::Array(
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .map(|n| Value::String(n.to_string()))
                    .collect(),
            )
        })
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let base_ref = pull_request
        .and_then(|p| p.get("base"))
        .and_then(|b| b.get("ref"))
        .and_then(|r| r.as_str())
        .unwrap_or("");
    let head_ref = pull_request
        .and_then(|p| p.get("head"))
        .and_then(|h| h.get("ref"))
        .and_then(|r| r.as_str())
        .unwrap_or("");

    serde_json::json!({
        "event": event,
        "action": payload.get("action").and_then(|a| a.as_str()).unwrap_or(""),
        "repo": repo.and_then(|r| r.get("full_name")).and_then(|n| n.as_str()).unwrap_or(""),
        "repo_owner": repo.and_then(|r| r.get("owner")).and_then(|o| o.get("login")).and_then(|l| l.as_str()).unwrap_or(""),
        "repo_name": repo.and_then(|r| r.get("name")).and_then(|n| n.as_str()).unwrap_or(""),
        "number": number,
        "title": title,
        "body": body,
        "author": author,
        "html_url": html_url,
        "labels": labels,
        "base_ref": base_ref,
        "head_ref": head_ref,
        "ref": payload.get("ref").and_then(|r| r.as_str()).unwrap_or(""),
        "commit_count": payload.get("commits").and_then(|c| c.as_array()).map(|c| c.len()).unwrap_or(0),
        "installation_id": payload.get("installation").and_then(|i| i.get("id")).cloned().unwrap_or(Value::Null),
        "organization": payload.get("organization").and_then(|o| o.get("login")).and_then(|l| l.as_str()).unwrap_or(""),
    })
}

/// Map a GitHub event + action to a favetto event kind. `None` means the delivery
/// is recognised as a no-op (it is still ack'd).
fn github_event_kind(event: &str, action: &str, payload: &Value) -> Option<EventKind> {
    Some(match event {
        "issues" => match action {
            "opened" => EventKind::IssueCreated,
            "closed" => EventKind::IssueClosed,
            "reopened" => EventKind::IssueReopened,
            "labeled" => EventKind::IssueLabeled,
            "assigned" => EventKind::IssueAssigned,
            _ => EventKind::IssueUpdated,
        },
        "issue_comment" => match action {
            "created" => EventKind::IssueCommentCreated,
            _ => return None,
        },
        "pull_request" => match action {
            "opened" => EventKind::PrCreated,
            "reopened" => EventKind::PrReopened,
            "synchronize" => EventKind::PrSynchronized,
            "closed" => {
                let merged = payload
                    .get("pull_request")
                    .and_then(|p| p.get("merged"))
                    .and_then(|m| m.as_bool())
                    .unwrap_or(false);
                if merged {
                    EventKind::PrMerged
                } else {
                    EventKind::PrClosed
                }
            }
            "ready_for_review" => EventKind::PrReadyForReview,
            "review_requested" => EventKind::PrReviewRequested,
            _ => EventKind::PrUpdated,
        },
        "pull_request_review" => match action {
            "submitted" => EventKind::PrReviewSubmitted,
            _ => return None,
        },
        "push" => EventKind::PushReceived,
        "workflow_run" => match action {
            "completed" => EventKind::ActionRunCompleted,
            _ => return None,
        },
        "check_suite" => match action {
            "completed" => EventKind::CheckSuiteCompleted,
            _ => return None,
        },
        "check_run" => match action {
            "completed" => EventKind::CheckRunCompleted,
            _ => return None,
        },
        _ => return None,
    })
}

/// Whether `event` is a GitHub event name rules may target.
fn supported_github_event(event: &str) -> bool {
    matches!(
        event,
        "issues"
            | "issue_comment"
            | "pull_request"
            | "pull_request_review"
            | "push"
            | "workflow_run"
            | "check_suite"
            | "check_run"
    )
}

impl GithubFilter {
    /// Every configured field must match; unset fields match anything.
    fn matches(&self, summary: &Value) -> bool {
        fn field<'a>(summary: &'a Value, key: &str) -> &'a str {
            summary.get(key).and_then(|v| v.as_str()).unwrap_or("")
        }

        for (pattern, key) in [
            (&self.repo, "repo"),
            (&self.author, "author"),
            (&self.base_ref, "base_ref"),
            (&self.head_ref, "head_ref"),
        ] {
            if let Some(pattern) = pattern {
                if !glob_match(pattern, field(summary, key)) {
                    return false;
                }
            }
        }

        if !self.labels_contains.is_empty() {
            let labels: Vec<&str> = summary
                .get("labels")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|l| l.as_str()).collect())
                .unwrap_or_default();
            if !self
                .labels_contains
                .iter()
                .any(|want| labels.contains(&want.as_str()))
            {
                return false;
            }
        }

        true
    }
}

impl GithubRule {
    /// Whether this rule applies to a summarised event.
    fn matches(&self, event: &str, action: &str, summary: &Value) -> bool {
        self.enabled
            && self.event == event
            && self.action.as_deref().map_or(true, |a| a == action)
            && self.filter.matches(summary)
    }
}

/// Glob match with the `glob` crate's default options (so `*` also crosses `/`).
fn glob_match(pattern: &str, value: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(value))
        .unwrap_or(false)
}

/// Validate `[webhook.github]` rules at daemon startup.
///
/// Unknown events, unknown catalog tasks, and invalid globs are fatal so a typo is
/// caught at boot rather than silently dropping deliveries.
pub fn validate_rules(cfg: &FavettoConfig, catalog: &[TaskDef]) -> anyhow::Result<()> {
    let gh = &cfg.webhook.github;
    if !gh.enabled && gh.rules.is_empty() {
        return Ok(());
    }

    let mut names: HashSet<&str> = HashSet::new();
    for rule in &gh.rules {
        if rule.name.trim().is_empty() {
            anyhow::bail!("webhook.github rule has an empty name");
        }
        if !names.insert(rule.name.as_str()) {
            anyhow::bail!("duplicate webhook.github rule name: {}", rule.name);
        }
        if !supported_github_event(&rule.event) {
            anyhow::bail!(
                "webhook.github rule '{}' names unsupported event '{}'",
                rule.name,
                rule.event
            );
        }
        if !catalog.iter().any(|d| d.name == rule.task) {
            anyhow::bail!(
                "webhook.github rule '{}' names unknown catalog task '{}'",
                rule.name,
                rule.task
            );
        }
        for (field, pattern) in [
            ("repo", &rule.filter.repo),
            ("author", &rule.filter.author),
            ("base_ref", &rule.filter.base_ref),
            ("head_ref", &rule.filter.head_ref),
        ] {
            if let Some(pattern) = pattern {
                if pattern.is_empty() {
                    anyhow::bail!(
                        "webhook.github rule '{}' has an empty {field} filter",
                        rule.name
                    );
                }
                glob::Pattern::new(pattern).map_err(|e| {
                    anyhow::anyhow!(
                        "webhook.github rule '{}' has invalid {field} glob '{pattern}': {e}",
                        rule.name
                    )
                })?;
            }
        }
        if rule
            .filter
            .labels_contains
            .iter()
            .any(|label| label.trim().is_empty())
        {
            anyhow::bail!(
                "webhook.github rule '{}' has an empty labels_contains entry",
                rule.name
            );
        }
    }

    Ok(())
}

/// Compute HMAC-SHA256 over `body` and compare against `signature` (hex) in constant time.
fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let expected = hex::encode(digest);
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::EventBus;
    use crate::tasks::TaskDef;
    use axum::http::{HeaderMap, HeaderValue};
    use favetto_core::auth::Token;
    use favetto_core::model::{EventKind, TaskStatus};
    use sqlx::SqlitePool;
    use std::path::PathBuf;
    use std::sync::RwLock;

    const SECRET: &str = "topsecret";

    /// Hex HMAC-SHA256, without the `sha256=` prefix.
    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn github_headers(event: &str, delivery: Option<&str>, signature: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_str(event).unwrap());
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&format!("sha256={signature}")).unwrap(),
        );
        if let Some(delivery) = delivery {
            headers.insert(
                "x-github-delivery",
                HeaderValue::from_str(delivery).unwrap(),
            );
        }
        headers
    }

    async fn test_state(rules: Vec<GithubRule>) -> (Arc<State>, PathBuf, SqlitePool) {
        let dir = std::env::temp_dir().join(format!("favetto-webhook-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("test.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();

        let mut cfg = FavettoConfig::default();
        cfg.webhook.github.enabled = true;
        cfg.webhook.github.rules = rules;

        let state = Arc::new(State::new(
            pool.clone(),
            EventBus::new(64),
            Token::generate(),
            WebhookSecrets {
                github: Some(SECRET.to_string()),
                linear: None,
            },
            crate::agents::AgentManager::new(),
            crate::agents::AgentRegistry::default(),
            Arc::new(RwLock::new(cfg)),
            dir.clone(),
            dir.clone(),
            Arc::new(RwLock::new(Vec::<TaskDef>::new())),
            tokio_cron_scheduler::JobScheduler::new().await.unwrap(),
            Arc::new(RwLock::new(Vec::new())),
        ));
        (state, dir, pool)
    }

    fn rule(name: &str, action: Option<&str>, filter: GithubFilter) -> GithubRule {
        GithubRule {
            name: name.to_string(),
            event: "issues".to_string(),
            action: action.map(str::to_string),
            task: "triage".to_string(),
            enabled: true,
            filter,
        }
    }

    fn issue_opened_body() -> serde_json::Value {
        serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 7,
                "title": "Fix the thing",
                "body": "details",
                "html_url": "https://github.com/oknozor/favetto/issues/7",
                "labels": [{ "name": "bug" }, { "name": "triage" }],
            },
            "repository": {
                "full_name": "oknozor/favetto",
                "name": "favetto",
                "owner": { "login": "oknozor" },
                "html_url": "https://github.com/oknozor/favetto",
            },
            "sender": { "login": "oknozor" },
        })
    }

    #[test]
    fn verify_signature_accepts_valid_and_rejects_wrong() {
        let body = b"{\"hello\":1}";
        assert!(verify_signature(SECRET, body, &sign(SECRET, body)));
        assert!(!verify_signature(SECRET, body, &sign("other", body)));
        assert!(!verify_signature(SECRET, body, "deadbeef"));
    }

    #[test]
    fn maps_github_event_kinds() {
        let cases: &[(&str, &str, bool, EventKind)] = &[
            ("issues", "opened", false, EventKind::IssueCreated),
            ("issues", "closed", false, EventKind::IssueClosed),
            ("issues", "reopened", false, EventKind::IssueReopened),
            ("issues", "labeled", false, EventKind::IssueLabeled),
            ("issues", "assigned", false, EventKind::IssueAssigned),
            ("issues", "milestoned", false, EventKind::IssueUpdated),
            (
                "issue_comment",
                "created",
                false,
                EventKind::IssueCommentCreated,
            ),
            ("pull_request", "opened", false, EventKind::PrCreated),
            ("pull_request", "reopened", false, EventKind::PrReopened),
            (
                "pull_request",
                "synchronize",
                false,
                EventKind::PrSynchronized,
            ),
            ("pull_request", "closed", false, EventKind::PrClosed),
            ("pull_request", "closed", true, EventKind::PrMerged),
            (
                "pull_request",
                "ready_for_review",
                false,
                EventKind::PrReadyForReview,
            ),
            (
                "pull_request",
                "review_requested",
                false,
                EventKind::PrReviewRequested,
            ),
            ("pull_request", "edited", false, EventKind::PrUpdated),
            (
                "pull_request_review",
                "submitted",
                false,
                EventKind::PrReviewSubmitted,
            ),
            ("push", "", false, EventKind::PushReceived),
            (
                "workflow_run",
                "completed",
                false,
                EventKind::ActionRunCompleted,
            ),
            (
                "check_suite",
                "completed",
                false,
                EventKind::CheckSuiteCompleted,
            ),
            (
                "check_run",
                "completed",
                false,
                EventKind::CheckRunCompleted,
            ),
        ];
        for (event, action, merged, expected) in cases {
            let payload = serde_json::json!({ "pull_request": { "merged": merged } });
            assert_eq!(
                github_event_kind(event, action, &payload).as_ref(),
                Some(expected),
                "event {event}/{action} merged={merged}"
            );
        }
        // An unrecognised `issues` action still maps to a generic update.
        assert_eq!(
            github_event_kind("issues", "created", &serde_json::json!({})),
            Some(EventKind::IssueUpdated)
        );
        assert_eq!(
            github_event_kind("watch", "started", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn supported_events_are_exact() {
        for event in [
            "issues",
            "issue_comment",
            "pull_request",
            "pull_request_review",
            "push",
            "workflow_run",
            "check_suite",
            "check_run",
        ] {
            assert!(supported_github_event(event));
        }
        assert!(!supported_github_event("watch"));
        assert!(!supported_github_event("issues_comment"));
    }

    #[test]
    fn truncate_utf8_respects_byte_cap_and_boundaries() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
        // "é" is two bytes; cutting at 3 must not split the second char.
        let s = "ééé"; // 6 bytes
        assert_eq!(truncate_utf8(s, 3), "é");
        assert_eq!(truncate_utf8(s, 4), "éé");
        assert!(truncate_utf8(s, 5).is_char_boundary(truncate_utf8(s, 5).len()));
    }

    #[test]
    fn summarize_extracts_fields_and_truncates() {
        let long_title = "t".repeat(SUMMARY_TITLE_MAX_BYTES + 50);
        let long_body = "b".repeat(SUMMARY_BODY_MAX_BYTES + 50);
        let body = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 1,
                "title": long_title,
                "body": long_body,
                "html_url": "https://github.com/oknozor/favetto/issues/1",
                "labels": [{ "name": "bug" }],
            },
            "repository": { "full_name": "oknozor/favetto", "owner": { "login": "oknozor" }, "name": "favetto" },
            "sender": { "login": "alice" },
        });
        let s = summarize_github("issues", &body);
        assert_eq!(s["event"], "issues");
        assert_eq!(s["action"], "opened");
        assert_eq!(s["repo"], "oknozor/favetto");
        assert_eq!(s["repo_owner"], "oknozor");
        assert_eq!(s["repo_name"], "favetto");
        assert_eq!(s["number"], 1);
        assert_eq!(s["author"], "alice");
        assert_eq!(s["labels"], serde_json::json!(["bug"]));
        assert_eq!(s["title"].as_str().unwrap().len(), SUMMARY_TITLE_MAX_BYTES);
        assert_eq!(s["body"].as_str().unwrap().len(), SUMMARY_BODY_MAX_BYTES);
        assert_eq!(s["base_ref"], "");
        assert_eq!(s["commit_count"], 0);
    }

    #[test]
    fn summarize_push_fields() {
        let body = serde_json::json!({
            "ref": "refs/heads/main",
            "commits": [{ "id": "a" }, { "id": "b" }],
            "repository": { "full_name": "oknozor/favetto" },
        });
        let s = summarize_github("push", &body);
        assert_eq!(s["ref"], "refs/heads/main");
        assert_eq!(s["commit_count"], 2);
        assert_eq!(s["number"], Value::Null);
        assert_eq!(s["labels"], serde_json::json!([]));
    }

    #[test]
    fn filter_matches_globs_and_labels() {
        let summary = summarize_github("issues", &issue_opened_body());
        assert!(GithubFilter {
            repo: Some("oknozor/*".to_string()),
            ..Default::default()
        }
        .matches(&summary));
        assert!(GithubFilter {
            author: Some("okn*".to_string()),
            ..Default::default()
        }
        .matches(&summary));
        assert!(GithubFilter {
            labels_contains: vec!["bug".to_string(), "nope".to_string()],
            ..Default::default()
        }
        .matches(&summary));
        assert!(!GithubFilter {
            labels_contains: vec!["nope".to_string()],
            ..Default::default()
        }
        .matches(&summary));
        // A set field that fails vetoes the match.
        assert!(!GithubFilter {
            repo: Some("someone-else/*".to_string()),
            ..Default::default()
        }
        .matches(&summary));
        // Unset filters match everything.
        assert!(GithubFilter::default().matches(&summary));
    }

    #[test]
    fn rule_matches_event_action_and_enabled() {
        let summary = summarize_github("issues", &issue_opened_body());
        let r = rule("r", Some("opened"), GithubFilter::default());
        assert!(r.matches("issues", "opened", &summary));
        assert!(!r.matches("issues", "closed", &summary));
        assert!(!r.matches("pull_request", "opened", &summary));

        let any_action = rule("r", None, GithubFilter::default());
        assert!(any_action.matches("issues", "closed", &summary));

        let mut disabled = rule("r", None, GithubFilter::default());
        disabled.enabled = false;
        assert!(!disabled.matches("issues", "opened", &summary));
    }

    fn catalog() -> Vec<TaskDef> {
        vec![TaskDef {
            name: "triage".to_string(),
            agent: None,
            provider: None,
            model: None,
            cwd: None,
            schedule: None,
            needs: None,
            spawn: None,
            spawn_file: None,
            vars: Vec::new(),
            prompt: String::new(),
        }]
    }

    fn cfg_with(rules: Vec<GithubRule>) -> FavettoConfig {
        let mut cfg = FavettoConfig::default();
        cfg.webhook.github.enabled = true;
        cfg.webhook.github.rules = rules;
        cfg
    }

    #[test]
    fn validate_rules_rejects_bad_config() {
        // Unknown event.
        let mut r = rule("r", None, GithubFilter::default());
        r.event = "watch".to_string();
        assert!(validate_rules(&cfg_with(vec![r]), &catalog()).is_err());

        // Unknown catalog task.
        let mut r = rule("r", None, GithubFilter::default());
        r.task = "nope".to_string();
        assert!(validate_rules(&cfg_with(vec![r]), &catalog()).is_err());

        // Invalid glob.
        let mut r = rule("r", None, GithubFilter::default());
        r.filter.repo = Some("[".to_string());
        assert!(validate_rules(&cfg_with(vec![r]), &catalog()).is_err());

        // Duplicate name.
        let r1 = rule("dup", None, GithubFilter::default());
        let r2 = rule("dup", None, GithubFilter::default());
        assert!(validate_rules(&cfg_with(vec![r1, r2]), &catalog()).is_err());

        // Empty name.
        let r = rule("   ", None, GithubFilter::default());
        assert!(validate_rules(&cfg_with(vec![r]), &catalog()).is_err());

        // Empty label.
        let mut r = rule("r", None, GithubFilter::default());
        r.filter.labels_contains = vec!["".to_string()];
        assert!(validate_rules(&cfg_with(vec![r]), &catalog()).is_err());

        // Valid.
        assert!(validate_rules(
            &cfg_with(vec![rule("ok", Some("opened"), GithubFilter::default())]),
            &catalog()
        )
        .is_ok());

        // Disabled with no rules is fine even with an empty catalog.
        assert!(validate_rules(&FavettoConfig::default(), &[]).is_ok());
    }

    #[tokio::test]
    async fn handle_github_enqueues_on_match() {
        let (state, dir, pool) = test_state(vec![rule(
            "triage-opened",
            Some("opened"),
            GithubFilter::default(),
        )])
        .await;
        let body = serde_json::to_vec(&issue_opened_body()).unwrap();
        let headers = github_headers("issues", Some("d-1"), &sign(SECRET, &body));

        let resp = handle_github(&state, &headers, &body).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let events = crate::db::tail_events(&pool, 100).await.unwrap();
        let issue_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::IssueCreated)
            .collect();
        assert_eq!(issue_events.len(), 1);
        let summary = &issue_events[0].payload;
        assert_eq!(summary["repo"], "oknozor/favetto");
        assert_eq!(summary["number"], 7);

        let tasks = crate::db::list_tasks(&pool).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Pending);
        assert_eq!(tasks[0].input["title"], "Fix the thing");
        assert_eq!(
            crate::template::render("{{ repo }}#{{ number }}", &tasks[0].input),
            "oknozor/favetto#7"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_github_runs_every_matching_rule() {
        let (state, dir, pool) = test_state(vec![
            rule("a", Some("opened"), GithubFilter::default()),
            rule("b", None, GithubFilter::default()),
        ])
        .await;
        let body = serde_json::to_vec(&issue_opened_body()).unwrap();
        let headers = github_headers("issues", Some("d-2"), &sign(SECRET, &body));

        assert_eq!(
            handle_github(&state, &headers, &body).await.status(),
            StatusCode::OK
        );
        assert_eq!(crate::db::list_tasks(&pool).await.unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_github_is_idempotent_per_delivery() {
        let (state, dir, pool) =
            test_state(vec![rule("a", Some("opened"), GithubFilter::default())]).await;
        let body = serde_json::to_vec(&issue_opened_body()).unwrap();
        let headers = github_headers("issues", Some("same-delivery"), &sign(SECRET, &body));

        assert_eq!(
            handle_github(&state, &headers, &body).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            handle_github(&state, &headers, &body).await.status(),
            StatusCode::OK
        );

        let events = crate::db::tail_events(&pool, 100).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::IssueCreated)
                .count(),
            1
        );
        assert_eq!(crate::db::list_tasks(&pool).await.unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_github_ignores_unknown_events() {
        let (state, dir, pool) =
            test_state(vec![rule("a", Some("opened"), GithubFilter::default())]).await;
        let body = serde_json::to_vec(&serde_json::json!({ "action": "started" })).unwrap();
        let headers = github_headers("watch", Some("d-3"), &sign(SECRET, &body));

        assert_eq!(
            handle_github(&state, &headers, &body).await.status(),
            StatusCode::OK
        );
        assert!(crate::db::tail_events(&pool, 100).await.unwrap().is_empty());
        assert!(crate::db::list_tasks(&pool).await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn handle_github_rejects_bad_signature() {
        let (state, dir, pool) =
            test_state(vec![rule("a", Some("opened"), GithubFilter::default())]).await;
        let body = serde_json::to_vec(&issue_opened_body()).unwrap();
        let headers = github_headers("issues", Some("d-4"), &sign("wrong-secret", &body));

        assert_eq!(
            handle_github(&state, &headers, &body).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(crate::db::tail_events(&pool, 100).await.unwrap().is_empty());
        assert!(crate::db::list_tasks(&pool).await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
