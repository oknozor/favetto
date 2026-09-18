//! Notification channels and history.
//!
//! Channels are deliberately thin:
//! - `log` — emit via `tracing` (always available, for tests).
//! - `webhook` — `POST {subject, body}` JSON to a URL.
//! - `ntfy` — `POST` the body to `https://<server>/<topic>` (ntfy.sh-compatible).
//!
//! Every send is recorded in the `notifications` table (persisted history). Desktop
//! (`notify-rust`) and email (`lettre`) channels are deferred — they need a display
//! and an SMTP server respectively.

use serde_json::Value;

use crate::db;
use crate::state::State;

/// Send a notification through `channel` and record it.
pub async fn send(
    state: &State,
    channel: &str,
    config: &Value,
    subject: &str,
    body: &str,
) {
    let result = match channel {
        "log" => {
            tracing::info!(subject, body, "notification (log)");
            Ok(())
        }
        "webhook" => send_webhook(config, subject, body).await,
        "ntfy" => send_ntfy(config, subject, body).await,
        other => Err(anyhow::anyhow!("unknown notification channel: {other}")),
    };

    let status = if result.is_ok() { "ok" } else { "error" };
    if let Err(e) = result {
        tracing::warn!(channel, subject, error = %e, "notification failed");
    }
    if let Err(e) = db::insert_notification(&state.db, channel, subject, body, status).await {
        tracing::warn!(error = %e, "failed to persist notification");
    }
}

async fn send_webhook(config: &Value, subject: &str, body: &str) -> anyhow::Result<()> {
    let url = config
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("webhook channel requires 'url'"))?;
    reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({ "subject": subject, "body": body }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn send_ntfy(config: &Value, subject: &str, body: &str) -> anyhow::Result<()> {
    let topic = config
        .get("topic")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("ntfy channel requires 'topic'"))?;
    let server = config
        .get("server")
        .and_then(|v| v.as_str())
        .unwrap_or("https://ntfy.sh");
    reqwest::Client::new()
        .post(format!("{}/{}", server.trim_end_matches('/'), topic))
        .header("Title", subject)
        .body(body.to_string())
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
