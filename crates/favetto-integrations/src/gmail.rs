//! Thin Gmail client over `reqwest` (no `google-gmail1` code-gen).
//!
//! Only the fields favetto uses are typed; everything else round-trips through the
//! API's JSON. List views use `format=metadata` (cheap), full bodies are fetched
//! only on demand via `get_message`/`get_thread`. Incoming MIME is read from the
//! API's already-parsed `payload` structure (base64url parts), and outgoing mail is
//! built as raw MIME and base64url-encoded.
//!
//! Auth is either a static access token (`GMAIL_ACCESS_TOKEN`) or an OAuth refresh
//! flow (`GMAIL_CLIENT_ID` + `GMAIL_CLIENT_SECRET` + a refresh token from
//! `GMAIL_REFRESH_TOKEN` or the OS keyring). `GMAIL_BASE_URL` points at a mock for
//! offline development.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Debug, Error)]
pub enum GmailError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("auth: {0}")]
    Auth(String),
    #[error("gmail api {status}: {body}")]
    Api { status: u16, body: String },
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// How to authenticate API calls.
#[derive(Clone)]
pub enum Auth {
    /// A fixed bearer token (`GMAIL_ACCESS_TOKEN`).
    Static { token: String },
    /// OAuth 2.0 refresh-token flow with an in-memory access-token cache.
    OAuth(OAuthConfig),
}

#[derive(Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub token_url: String,
    cached: Arc<RwLock<CachedToken>>,
}

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

impl OAuthConfig {
    pub fn new(client_id: String, client_secret: String, refresh_token: String) -> Self {
        Self {
            client_id,
            client_secret,
            refresh_token,
            token_url: DEFAULT_TOKEN_URL.to_string(),
            cached: Arc::new(RwLock::new(CachedToken {
                access_token: String::new(),
                expires_at: Instant::now(),
            })),
        }
    }

    /// Return a valid access token, refreshing it (thin `reqwest` POST) when the
    /// cached one is at (or near) expiry.
    async fn access_token(&self) -> Result<String, GmailError> {
        {
            let cached = self.cached.read().unwrap();
            if cached.expires_at > Instant::now() && !cached.access_token.is_empty() {
                return Ok(cached.access_token.clone());
            }
        }

        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("refresh_token", self.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];
        let resp = reqwest::Client::new()
            .post(&self.token_url)
            .form(&params)
            .send()
            .await?
            .error_for_status()?;
        let body: Value = resp.json().await?;

        let access_token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GmailError::Auth("token response missing access_token".into()))?
            .to_string();
        let expires_in = body.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);

        *self.cached.write().unwrap() = CachedToken {
            access_token: access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in.saturating_sub(60)),
        };

        Ok(access_token)
    }
}

impl Auth {
    /// Build auth from the environment. Prefers OAuth when the refresh-token pieces
    /// are present, else a static access token.
    pub fn from_env() -> Option<Self> {
        let client_id = std::env::var("GMAIL_CLIENT_ID").ok();
        let client_secret = std::env::var("GMAIL_CLIENT_SECRET").ok();
        if let (Some(client_id), Some(client_secret)) = (client_id, client_secret) {
            if let Some(refresh_token) = load_refresh_token() {
                return Some(Auth::OAuth(OAuthConfig::new(
                    client_id,
                    client_secret,
                    refresh_token,
                )));
            }
        }
        std::env::var("GMAIL_ACCESS_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(|token| Auth::Static { token })
    }
}

/// Refresh token from `GMAIL_REFRESH_TOKEN`, else the OS keyring (`favetto` /
/// `gmail_refresh_token`).
fn load_refresh_token() -> Option<String> {
    if let Ok(t) = std::env::var("GMAIL_REFRESH_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    keyring::Entry::new("favetto", "gmail_refresh_token")
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// Typed model (used fields only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessageMeta {
    pub id: String,
    #[serde(rename = "threadId", default)]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListMessagesResult {
    #[serde(default)]
    pub messages: Vec<MessageMeta>,
    #[serde(rename = "nextPageToken", default)]
    pub next_page_token: Option<String>,
    #[serde(rename = "resultSizeEstimate", default)]
    pub result_size_estimate: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "threadId", default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(rename = "labelIds", default)]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub payload: Option<Payload>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Payload {
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub body: Body,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Body {
    /// base64url-encoded content.
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Part {
    #[serde(rename = "partId", default)]
    pub part_id: Option<String>,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub body: Body,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SentMessage {
    pub id: String,
    #[serde(rename = "threadId", default)]
    pub thread_id: Option<String>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.payload
            .as_ref()?
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    pub fn subject(&self) -> Option<&str> {
        self.header("Subject")
    }

    pub fn from(&self) -> Option<&str> {
        self.header("From")
    }

    /// Decode the message's `text/plain` (or `text/html`) body, preferring the first
    /// matching part, falling back to the top-level body.
    pub fn body_text(&self) -> Option<String> {
        let payload = self.payload.as_ref()?;
        let part = payload
            .parts
            .iter()
            .find(|p| p.mime_type.as_deref() == Some("text/plain"))
            .or_else(|| payload.parts.iter().find(|p| p.mime_type.as_deref() == Some("text/html")));
        let data = part
            .as_ref()
            .and_then(|p| p.body.data.as_deref())
            .or(payload.body.data.as_deref())?;
        decode_base64url(data)
    }
}

fn decode_base64url(s: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()?;
    String::from_utf8(bytes).ok()
}

fn encode_base64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct GmailClient {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
}

impl GmailClient {
    pub fn new(base_url: impl Into<String>, auth: Auth) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            auth,
        }
    }

    async fn token(&self) -> Result<String, GmailError> {
        match &self.auth {
            Auth::Static { token } => Ok(token.clone()),
            Auth::OAuth(cfg) => cfg.access_token().await,
        }
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(String, String)],
    ) -> Result<T, GmailError> {
        let token = self.token().await?;
        let mut req = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(token);
        if !query.is_empty() {
            req = req.query(query);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(GmailError::Api {
                status: status.as_u16(),
                body,
            });
        }
        Ok(serde_json::from_str(&body)?)
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
    ) -> Result<T, GmailError> {
        let token = self.token().await?;
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(GmailError::Api {
                status: status.as_u16(),
                body: text,
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    pub async fn list_labels(&self) -> Result<Vec<Label>, GmailError> {
        #[derive(Deserialize)]
        struct Labels {
            labels: Vec<Label>,
        }
        Ok(self
            .get::<Labels>("/users/me/labels", &[])
            .await?
            .labels)
    }

    /// List message metadata (cheap `format=metadata`), optionally filtered by a
    /// Gmail query and paginated.
    pub async fn list_messages(
        &self,
        query: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<ListMessagesResult, GmailError> {
        let mut q = vec![("format".to_string(), "metadata".to_string())];
        if let Some(query) = query {
            q.push(("q".to_string(), query.to_string()));
        }
        if let Some(token) = page_token {
            q.push(("pageToken".to_string(), token.to_string()));
        }
        self.get("/users/me/messages", &q).await
    }

    /// Fetch a full message (`format=full`).
    pub async fn get_message(&self, id: &str) -> Result<Message, GmailError> {
        self.get(
            &format!("/users/me/messages/{id}"),
            &[("format".to_string(), "full".to_string())],
        )
        .await
    }

    pub async fn get_thread(&self, id: &str) -> Result<Thread, GmailError> {
        self.get(&format!("/users/me/threads/{id}"), &[]).await
    }

    /// Add/remove labels on a message.
    pub async fn modify_labels(
        &self,
        id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<Message, GmailError> {
        let body = serde_json::json!({ "addLabelIds": add, "removeLabelIds": remove });
        self.post(&format!("/users/me/messages/{id}/modify"), body)
            .await
    }

    /// Send a plain-text message, building the raw MIME and base64url encoding it.
    pub async fn send_message(
        &self,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<SentMessage, GmailError> {
        let raw = format!(
            "From: favetto@localhost\r\nTo: {to}\r\nSubject: {subject}\r\nContent-Type: text/plain; charset=\"UTF-8\"\r\n\r\n{body}"
        );
        let payload = serde_json::json!({ "raw": encode_base64url(raw.as_bytes()) });
        self.post("/users/me/messages/send", payload).await
    }
}
