//! Persisted MCP server registry and ACP launch selection.
//!
//! Favetto never installs, downloads or supervises MCP servers. A registry entry
//! only records how to launch (stdio) or reach (HTTP/SSE) a server the user
//! already installed; the ACP agent connects to it directly.
//!
//! Two rules shape this module:
//!
//! - **Secrets never leave the daemon.** Env/header values are declared by
//!   source (`env` variable or `command`) or as a write-only literal stored in
//!   `<data_dir>/mcp-secrets.json` with mode `0600`. RPC payloads carry only the
//!   declaration and a `has_value` marker.
//! - **A bad definition never blocks a session.** Unknown *explicit* names are
//!   an error, but removed/disabled/unapproved/unresolvable entries are omitted
//!   with a warning and the agent still starts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use favetto_core::config::McpServerSeed;
use favetto_core::mcp::{
    McpSecret, McpSecretSource, McpServerInfo, McpServerInput, McpSessionState, McpSource,
    McpTransport,
};
use favetto_core::Timestamp;
use sha2::{Digest, Sha256};

use crate::db::{self, McpServerRow};
use crate::state::State;

/// Largest accepted registry name.
const MAX_NAME_LEN: usize = 64;
/// Largest accepted command line.
const MAX_COMMAND_LEN: usize = 4096;
/// Largest accepted argument list.
const MAX_ARGS: usize = 64;
/// Largest accepted single argument.
const MAX_ARG_LEN: usize = 4096;
/// Largest accepted URL.
const MAX_URL_LEN: usize = 2048;
/// Largest accepted env/header declaration count per definition.
const MAX_SECRETS: usize = 64;
/// Largest accepted env/header name.
const MAX_SECRET_NAME_LEN: usize = 128;
/// Largest accepted secret command argv.
const MAX_SECRET_COMMAND: usize = 64;

/// The secrets file name inside the daemon data directory.
const SECRETS_FILE: &str = "mcp-secrets.json";

/// Secret kind key prefixes used by the on-disk secrets map.
const ENV_KIND: &str = "env";
const HEADER_KIND: &str = "header";

/// One registered definition plus its in-memory literal values.
///
/// Literal values are keyed by declaration name inside [`McpRegistry::secrets`],
/// never on this type, so a `Debug` print or clone of the row cannot leak them.
#[derive(Debug, Clone, Default)]
pub struct McpRegistry {
    pub entries: Vec<McpServerRow>,
    /// `server -> "env:NAME"/"header:NAME" -> value`.
    secrets: BTreeMap<String, BTreeMap<String, String>>,
    path: PathBuf,
}

impl McpRegistry {
    /// An empty registry backed by `<data_dir>/mcp-secrets.json`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            entries: Vec::new(),
            secrets: BTreeMap::new(),
            path: data_dir.join(SECRETS_FILE),
        }
    }

    /// Read the `0600` secrets file. A missing file is empty; a malformed one is
    /// logged and ignored rather than blocking startup.
    pub fn load_secrets(&mut self) {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return;
        };
        match serde_json::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&raw) {
            Ok(secrets) => self.secrets = secrets,
            Err(error) => tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "ignoring malformed MCP secrets file"
            ),
        }
    }

    /// The stored literal for one declaration, if any.
    pub fn literal(&self, server: &str, kind: &str, name: &str) -> Option<&str> {
        self.secrets
            .get(server)
            .and_then(|fields| fields.get(&format!("{kind}:{name}")))
            .map(String::as_str)
    }

    /// Whether a literal is stored for one declaration.
    pub fn has_literal(&self, server: &str, kind: &str, name: &str) -> bool {
        self.literal(server, kind, name).is_some()
    }

    fn set_literal(&mut self, server: &str, kind: &str, name: &str, value: String) {
        self.secrets
            .entry(server.to_string())
            .or_default()
            .insert(format!("{kind}:{name}"), value);
    }

    fn remove_server(&mut self, server: &str) {
        self.secrets.remove(server);
    }

    /// Persist literal values atomically with mode `0600` (never world-readable).
    pub fn save_secrets(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_vec_pretty(&self.secrets)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, raw)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Load the persisted definitions into the in-memory mirror.
pub async fn refresh(state: &State) -> anyhow::Result<()> {
    let entries = db::list_mcp_servers(&state.db).await?;
    let mut registry = state.mcp.write();
    registry.entries = entries;
    Ok(())
}

/// The redacted wire form of one row, marking literal-backed secrets.
fn info(registry: &McpRegistry, row: &McpServerRow) -> McpServerInfo {
    let redact = |secrets: &[McpSecret], kind: &str| -> Vec<McpSecret> {
        secrets
            .iter()
            .map(|secret| {
                let mut secret = secret.clone();
                secret.value = None;
                secret.has_value = secret.source == McpSecretSource::Literal
                    && registry.has_literal(&row.name, kind, &secret.name);
                secret
            })
            .collect()
    };
    McpServerInfo {
        name: row.name.clone(),
        transport: row.transport,
        command: row.command.clone(),
        args: row.args.clone(),
        env: redact(&row.env, ENV_KIND),
        url: row.url.clone(),
        headers: redact(&row.headers, HEADER_KIND),
        enabled: row.enabled,
        approved: row.approved,
        fingerprint: row.fingerprint.clone(),
        source: row.source,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// All registered definitions, secrets redacted.
pub fn list(state: &State) -> Vec<McpServerInfo> {
    let registry = state.mcp.read();
    registry
        .entries
        .iter()
        .map(|row| info(&registry, row))
        .collect()
}

/// One definition by name.
pub fn get(state: &State, name: &str) -> anyhow::Result<McpServerInfo> {
    let registry = state.mcp.read();
    let row = registry
        .entries
        .iter()
        .find(|row| row.name == name)
        .ok_or_else(|| anyhow::anyhow!("unknown MCP server '{name}'"))?;
    Ok(info(&registry, row))
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

/// A validated input, ready to persist. Secret literal values are separated from
/// declarations so they never reach SQLite.
struct Validated {
    transport: McpTransport,
    command: Option<String>,
    args: Vec<String>,
    url: Option<String>,
    env: Vec<McpSecret>,
    headers: Vec<McpSecret>,
    env_literals: Vec<(String, String)>,
    header_literals: Vec<(String, String)>,
    fingerprint: String,
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("MCP server name must not be empty");
    }
    if name.len() > MAX_NAME_LEN {
        anyhow::bail!("MCP server name must be at most {MAX_NAME_LEN} characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        anyhow::bail!("invalid MCP server name '{name}': use [A-Za-z0-9._-]");
    }
    Ok(())
}

fn validate_text(what: &str, value: &str, max: usize) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{what} must not be empty");
    }
    if value.len() > max {
        anyhow::bail!("{what} must be at most {max} bytes");
    }
    if value.chars().any(char::is_control) {
        anyhow::bail!("{what} must not contain control characters");
    }
    Ok(())
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first = chars.next();
    matches!(first, Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '.'
                        | '^'
                        | '_'
                        | '`'
                        | '|'
                        | '~'
                )
        })
}

/// Resolve a bare command name against `PATH`, returning an absolute path.
fn resolve_executable(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    let candidate = if command.contains(std::path::MAIN_SEPARATOR) {
        path.to_path_buf()
    } else {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(command))
                .find(|candidate| is_executable(candidate))
        })?
    };
    if is_executable(&candidate) {
        Some(std::fs::canonicalize(&candidate).unwrap_or(candidate))
    } else {
        None
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn fingerprint(
    transport: McpTransport,
    command: Option<&str>,
    args: &[String],
    url: Option<&str>,
    env: &[McpSecret],
    headers: &[McpSecret],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(transport.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(command.unwrap_or_default().as_bytes());
    hasher.update([0]);
    for arg in args {
        hasher.update(arg.as_bytes());
        hasher.update([0]);
    }
    hasher.update(url.unwrap_or_default().as_bytes());
    hasher.update([0]);
    for secret in env {
        hasher.update(secret.name.as_bytes());
        hasher.update([0]);
    }
    hasher.update([0]);
    for secret in headers {
        hasher.update(secret.name.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn validate_input(input: &McpServerInput, require_literals: bool) -> anyhow::Result<Validated> {
    let name = input.name.trim();
    validate_name(name)?;

    let transport = input.transport;
    let mut command = None;
    let mut url = None;
    match transport {
        McpTransport::Stdio => {
            let raw = input
                .command
                .as_deref()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .ok_or_else(|| anyhow::anyhow!("stdio MCP server '{name}' requires a command"))?;
            validate_text("stdio command", raw, MAX_COMMAND_LEN)?;
            let resolved = resolve_executable(raw).ok_or_else(|| {
                anyhow::anyhow!("stdio command '{raw}' was not found on PATH or is not executable")
            })?;
            command = Some(resolved.to_string_lossy().into_owned());
            if !input.url.as_deref().unwrap_or_default().trim().is_empty() {
                anyhow::bail!("stdio MCP server '{name}' must not set a url");
            }
        }
        McpTransport::Http | McpTransport::Sse => {
            let raw = input
                .url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("{} MCP server '{name}' requires a url", transport.as_str())
                })?;
            validate_text("MCP url", raw, MAX_URL_LEN)?;
            let parsed = url::Url::parse(raw)
                .map_err(|error| anyhow::anyhow!("invalid MCP url '{raw}': {error}"))?;
            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                anyhow::bail!("MCP url '{raw}' must use http or https");
            }
            if parsed.host_str().is_none() {
                anyhow::bail!("MCP url '{raw}' must include a host");
            }
            if input
                .command
                .as_deref()
                .map(str::trim)
                .is_some_and(|command| !command.is_empty())
            {
                anyhow::bail!(
                    "{} MCP server '{name}' must not set a command",
                    transport.as_str()
                );
            }
            url = Some(raw.to_string());
        }
    }

    if input.args.len() > MAX_ARGS {
        anyhow::bail!("MCP server '{name}' accepts at most {MAX_ARGS} args");
    }
    for arg in &input.args {
        if arg.len() > MAX_ARG_LEN {
            anyhow::bail!("MCP arg must be at most {MAX_ARG_LEN} bytes");
        }
        if arg.chars().any(char::is_control) {
            anyhow::bail!("MCP args must not contain control characters");
        }
    }

    if transport != McpTransport::Stdio && !input.env.is_empty() {
        anyhow::bail!(
            "{} MCP server '{name}' must not set env",
            transport.as_str()
        );
    }
    if transport == McpTransport::Stdio && !input.headers.is_empty() {
        anyhow::bail!("stdio MCP server '{name}' must not set headers");
    }

    let (env, env_literals) =
        validate_secrets(name, ENV_KIND, &input.env, is_env_name, require_literals)?;
    let (headers, header_literals) = validate_secrets(
        name,
        HEADER_KIND,
        &input.headers,
        is_header_name,
        require_literals,
    )?;

    let fingerprint = fingerprint(
        transport,
        command.as_deref(),
        &input.args,
        url.as_deref(),
        &env,
        &headers,
    );

    Ok(Validated {
        transport,
        command,
        args: input.args.clone(),
        url,
        env,
        headers,
        env_literals,
        header_literals,
        fingerprint,
    })
}

/// Declaration list plus the literal values supplied on write.
type ValidatedSecrets = (Vec<McpSecret>, Vec<(String, String)>);

fn validate_secrets(
    server: &str,
    kind: &str,
    secrets: &[McpSecret],
    valid_name: fn(&str) -> bool,
    require_literals: bool,
) -> anyhow::Result<ValidatedSecrets> {
    if secrets.len() > MAX_SECRETS {
        anyhow::bail!("MCP server '{server}' accepts at most {MAX_SECRETS} {kind} entries");
    }
    let mut declarations = Vec::with_capacity(secrets.len());
    let mut literals = Vec::new();
    let mut seen = BTreeSet::new();
    for secret in secrets {
        let name = secret.name.trim();
        if !valid_name(name) {
            anyhow::bail!("invalid MCP {kind} name '{name}'");
        }
        if name.len() > MAX_SECRET_NAME_LEN {
            anyhow::bail!("MCP {kind} name must be at most {MAX_SECRET_NAME_LEN} characters");
        }
        if !seen.insert(name.to_string()) {
            anyhow::bail!("duplicate MCP {kind} name '{name}'");
        }
        let mut declaration = McpSecret::new(name);
        declaration.source = secret.source;
        match secret.source {
            McpSecretSource::Env => {
                let env = secret
                    .env
                    .as_deref()
                    .map(str::trim)
                    .filter(|env| !env.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("MCP {kind} '{name}' requires the source env var")
                    })?;
                if !is_env_name(env) {
                    anyhow::bail!("invalid source env var name '{env}' for MCP {kind} '{name}'");
                }
                declaration.env = Some(env.to_string());
            }
            McpSecretSource::Command => {
                if secret.command.is_empty() || secret.command.len() > MAX_SECRET_COMMAND {
                    anyhow::bail!(
                        "MCP {kind} '{name}' requires 1..={MAX_SECRET_COMMAND} argv entries"
                    );
                }
                for arg in &secret.command {
                    validate_text("secret command argument", arg, MAX_ARG_LEN)?;
                }
                declaration.command = secret.command.clone();
            }
            McpSecretSource::Literal => match secret.value.as_deref() {
                Some(value) => {
                    validate_text("literal secret", value, MAX_ARG_LEN)?;
                    literals.push((name.to_string(), value.to_string()));
                }
                None if require_literals => {
                    anyhow::bail!("literal MCP {kind} '{name}' requires a value");
                }
                None => {}
            },
        }
        declarations.push(declaration);
    }
    Ok((declarations, literals))
}

// ---------------------------------------------------------------------------
// management
// ---------------------------------------------------------------------------

fn apply_literals(
    registry: &mut McpRegistry,
    server: &str,
    kind: &str,
    declarations: &[McpSecret],
    literals: Vec<(String, String)>,
) {
    // Drop stored values for declarations that went away or changed source.
    let keep: BTreeSet<&str> = declarations
        .iter()
        .filter(|secret| secret.source == McpSecretSource::Literal)
        .map(|secret| secret.name.as_str())
        .collect();
    if let Some(fields) = registry.secrets.get_mut(server) {
        let prefix = format!("{kind}:");
        fields.retain(|key, _| match key.strip_prefix(&prefix) {
            Some(name) => keep.contains(name),
            // Leave the other secret kind untouched.
            None => true,
        });
        if fields.is_empty() {
            registry.secrets.remove(server);
        }
    }
    for (name, value) in literals {
        registry.set_literal(server, kind, &name, value);
    }
}

async fn persist(state: &State, row: McpServerRow) -> anyhow::Result<()> {
    db::upsert_mcp_server(&state.db, &row).await?;
    Ok(())
}

/// Register a new definition.
pub async fn add(
    state: &State,
    input: &McpServerInput,
    approve: Option<bool>,
) -> anyhow::Result<McpServerInfo> {
    let name = input.name.trim().to_string();
    let validated = validate_input(input, true)?;
    {
        let registry = state.mcp.read();
        if registry.entries.iter().any(|row| row.name == name) {
            anyhow::bail!("MCP server '{name}' is already registered");
        }
    }
    let now = Timestamp::now();
    let approved = match validated.transport {
        McpTransport::Stdio => approve.unwrap_or(false),
        McpTransport::Http | McpTransport::Sse => true,
    };
    let row = McpServerRow {
        name: name.clone(),
        transport: validated.transport,
        command: validated.command,
        args: validated.args,
        url: validated.url,
        env: validated.env,
        headers: validated.headers,
        enabled: input.enabled.unwrap_or(true),
        approved,
        fingerprint: validated.fingerprint,
        source: McpSource::Manual,
        created_at: now,
        updated_at: now,
    };
    // Validate and record any literal values before the row becomes visible.
    {
        let mut registry = state.mcp.write();
        apply_literals(
            &mut registry,
            &name,
            ENV_KIND,
            &row.env,
            validated.env_literals,
        );
        apply_literals(
            &mut registry,
            &name,
            HEADER_KIND,
            &row.headers,
            validated.header_literals,
        );
        registry.save_secrets()?;
    }
    persist(state, row.clone()).await?;
    let mut registry = state.mcp.write();
    registry.entries.push(row.clone());
    registry.entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(info(&registry, &row))
}

/// Replace an existing definition.
pub async fn update(
    state: &State,
    name: &str,
    input: &McpServerInput,
    approve: Option<bool>,
) -> anyhow::Result<McpServerInfo> {
    let existing = {
        let registry = state.mcp.read();
        registry
            .entries
            .iter()
            .find(|row| row.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown MCP server '{name}'"))?
    };
    let mut normalized = input.clone();
    normalized.name = existing.name.clone();
    let validated = validate_input(&normalized, false)?;
    let now = Timestamp::now();
    let approved = match validated.transport {
        McpTransport::Stdio => {
            if approve == Some(true) {
                true
            } else if validated.fingerprint == existing.fingerprint {
                existing.approved
            } else {
                // Editing the command/args (or its secret names) revokes consent.
                false
            }
        }
        McpTransport::Http | McpTransport::Sse => true,
    };
    let row = McpServerRow {
        name: existing.name.clone(),
        transport: validated.transport,
        command: validated.command,
        args: validated.args,
        url: validated.url,
        env: validated.env,
        headers: validated.headers,
        enabled: input.enabled.unwrap_or(existing.enabled),
        approved,
        fingerprint: validated.fingerprint,
        source: existing.source,
        created_at: existing.created_at,
        updated_at: now,
    };
    {
        let mut registry = state.mcp.write();
        apply_literals(
            &mut registry,
            &row.name,
            ENV_KIND,
            &row.env,
            validated.env_literals,
        );
        apply_literals(
            &mut registry,
            &row.name,
            HEADER_KIND,
            &row.headers,
            validated.header_literals,
        );
        registry.save_secrets()?;
    }
    persist(state, row.clone()).await?;
    let mut registry = state.mcp.write();
    if let Some(slot) = registry.entries.iter_mut().find(|row| row.name == name) {
        *slot = row.clone();
    }
    Ok(info(&registry, &row))
}

/// Remove a definition and any stored literal values.
pub async fn remove(state: &State, name: &str) -> anyhow::Result<bool> {
    let removed = db::delete_mcp_server(&state.db, name).await?;
    {
        let mut registry = state.mcp.write();
        registry.entries.retain(|row| row.name != name);
        registry.remove_server(name);
        registry.save_secrets()?;
    }
    Ok(removed)
}

/// Enable or disable a definition.
pub async fn set_enabled(
    state: &State,
    name: &str,
    enabled: bool,
) -> anyhow::Result<McpServerInfo> {
    let existing = {
        let registry = state.mcp.read();
        registry
            .entries
            .iter()
            .find(|row| row.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown MCP server '{name}'"))?
    };
    let row = McpServerRow {
        enabled,
        updated_at: Timestamp::now(),
        ..existing
    };
    persist(state, row.clone()).await?;
    let mut registry = state.mcp.write();
    if let Some(slot) = registry.entries.iter_mut().find(|row| row.name == name) {
        *slot = row.clone();
    }
    Ok(info(&registry, &row))
}

/// Approve (or revoke) launching a stdio command at its current fingerprint.
pub async fn approve(state: &State, name: &str, approved: bool) -> anyhow::Result<McpServerInfo> {
    let existing = {
        let registry = state.mcp.read();
        registry
            .entries
            .iter()
            .find(|row| row.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown MCP server '{name}'"))?
    };
    if existing.transport != McpTransport::Stdio {
        anyhow::bail!("MCP server '{name}' is not a stdio definition; approval does not apply");
    }
    let row = McpServerRow {
        approved,
        updated_at: Timestamp::now(),
        ..existing
    };
    persist(state, row.clone()).await?;
    let mut registry = state.mcp.write();
    if let Some(slot) = registry.entries.iter_mut().find(|row| row.name == name) {
        *slot = row.clone();
    }
    Ok(info(&registry, &row))
}

/// Seed the registry from `[[mcp.servers]]`. Existing names are never
/// overwritten, so app edits survive restarts. Invalid seeds are logged and
/// skipped.
pub async fn seed_from_config(state: &State, config: &favetto_core::config::FavettoConfig) {
    for seed in &config.mcp.servers {
        if let Ok(Some(_)) = db::get_mcp_server(&state.db, &seed.name).await {
            continue;
        }
        if let Err(error) = seed_one(state, seed).await {
            tracing::warn!(server = %seed.name, error = %error, "skipping invalid MCP seed");
        }
    }
}

async fn seed_one(state: &State, seed: &McpServerSeed) -> anyhow::Result<()> {
    let mut env = Vec::new();
    for (name, value) in &seed.env {
        env.push(McpSecret::with_literal(name.clone(), value.clone()));
    }
    for (name, source) in &seed.env_from {
        env.push(McpSecret {
            name: name.clone(),
            source: McpSecretSource::Env,
            env: Some(source.clone()),
            command: Vec::new(),
            value: None,
            has_value: false,
        });
    }
    let mut headers = Vec::new();
    for (name, value) in &seed.headers {
        headers.push(McpSecret::with_literal(name.clone(), value.clone()));
    }
    for (name, source) in &seed.headers_from {
        headers.push(McpSecret {
            name: name.clone(),
            source: McpSecretSource::Env,
            env: Some(source.clone()),
            command: Vec::new(),
            value: None,
            has_value: false,
        });
    }
    let input = McpServerInput {
        name: seed.name.clone(),
        transport: seed.transport,
        command: seed.command.clone(),
        args: seed.args.clone(),
        env,
        url: seed.url.clone(),
        headers,
        enabled: Some(seed.enabled),
    };
    let validated = validate_input(&input, true)?;
    let now = Timestamp::now();
    let row = McpServerRow {
        name: seed.name.clone(),
        transport: validated.transport,
        command: validated.command,
        args: validated.args,
        url: validated.url,
        env: validated.env,
        headers: validated.headers,
        enabled: seed.enabled,
        approved: seed.approved,
        fingerprint: validated.fingerprint,
        source: McpSource::Config,
        created_at: now,
        updated_at: now,
    };
    {
        let mut registry = state.mcp.write();
        apply_literals(
            &mut registry,
            &row.name,
            ENV_KIND,
            &row.env,
            validated.env_literals,
        );
        apply_literals(
            &mut registry,
            &row.name,
            HEADER_KIND,
            &row.headers,
            validated.header_literals,
        );
        registry.save_secrets()?;
    }
    persist(state, row).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// launch selection
// ---------------------------------------------------------------------------

/// One resolved MCP server ready to be converted into an ACP `McpServer`.
///
/// This carries secret values, so its `Debug` is redacted and it is never
/// serialized.
#[derive(Clone)]
pub struct McpCandidate {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for McpCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpCandidate")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("url", &self.url)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The result of resolving a session's MCP selection.
#[derive(Debug, Default)]
pub struct McpSelection {
    /// The requested registry names, in order, including omitted ones.
    pub selected: Vec<String>,
    /// Definitions that resolved and are allowed to launch.
    pub servers: Vec<McpCandidate>,
    /// Human-readable reasons an entry was omitted.
    pub warnings: Vec<String>,
}

impl McpSelection {
    /// The initial session state before per-transport capability filtering.
    pub fn state(&self) -> McpSessionState {
        McpSessionState {
            selected: self.selected.clone(),
            active: self
                .servers
                .iter()
                .map(|server| server.name.clone())
                .collect(),
            warnings: self.warnings.clone(),
        }
    }

    /// Names recorded on the durable session so a restore replays them.
    pub fn recorded_names(&self) -> Vec<String> {
        self.selected.clone()
    }
}

fn default_names(registry: &McpRegistry) -> Vec<String> {
    registry
        .entries
        .iter()
        .filter(|row| row.enabled)
        .filter(|row| row.transport != McpTransport::Stdio || row.approved)
        .map(|row| row.name.clone())
        .collect()
}

fn resolve_secret(
    registry: &McpRegistry,
    server: &str,
    kind: &str,
    secret: &McpSecret,
) -> Result<String, String> {
    match secret.source {
        McpSecretSource::Env => {
            let var = secret.env.as_deref().unwrap_or_default();
            std::env::var(var).map_err(|_| {
                format!(
                    "MCP server '{server}': {kind} '{}' reads env var '{var}', which is not set",
                    secret.name
                )
            })
        }
        McpSecretSource::Command => {
            let (program, args) = secret.command.split_first().ok_or_else(|| {
                format!(
                    "MCP server '{server}': {kind} '{}' has no command",
                    secret.name
                )
            })?;
            let output = Command::new(program).args(args).output().map_err(|error| {
                format!(
                    "MCP server '{server}': {kind} '{}' secret command failed: {error}",
                    secret.name
                )
            })?;
            if !output.status.success() {
                return Err(format!(
                    "MCP server '{server}': {kind} '{}' secret command exited with {}",
                    secret.name, output.status
                ));
            }
            String::from_utf8(output.stdout)
                .map(|value| value.trim_end_matches(['\r', '\n']).to_string())
                .map_err(|_| {
                    format!(
                        "MCP server '{server}': {kind} '{}' secret command produced non-UTF-8 output",
                        secret.name
                    )
                })
        }
        McpSecretSource::Literal => registry
            .literal(server, kind, &secret.name)
            .map(str::to_string)
            .ok_or_else(|| {
                format!(
                    "MCP server '{server}': {kind} '{}' has no stored literal value",
                    secret.name
                )
            }),
    }
}

fn candidate(registry: &McpRegistry, row: &McpServerRow) -> Result<McpCandidate, String> {
    let mut env = BTreeMap::new();
    for secret in &row.env {
        env.insert(
            secret.name.clone(),
            resolve_secret(registry, &row.name, ENV_KIND, secret)?,
        );
    }
    let mut headers = BTreeMap::new();
    for secret in &row.headers {
        headers.insert(
            secret.name.clone(),
            resolve_secret(registry, &row.name, HEADER_KIND, secret)?,
        );
    }
    let command = row.command.as_deref().map(PathBuf::from);
    Ok(McpCandidate {
        name: row.name.clone(),
        transport: row.transport,
        command,
        args: row.args.clone(),
        env,
        url: row.url.clone(),
        headers,
    })
}

fn resolve_names(
    registry: &McpRegistry,
    names: &[String],
    unknown_is_error: bool,
) -> anyhow::Result<McpSelection> {
    let mut selection = McpSelection {
        selected: names.to_vec(),
        ..McpSelection::default()
    };
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(row) = registry.entries.iter().find(|row| &row.name == name) else {
            if unknown_is_error {
                anyhow::bail!("unknown MCP server '{name}'; register it with mcp.add first");
            }
            selection.warnings.push(format!(
                "MCP server '{name}' is no longer registered; omitting it"
            ));
            continue;
        };
        if !row.enabled {
            selection
                .warnings
                .push(format!("MCP server '{name}' is disabled; omitting it"));
            continue;
        }
        if row.transport == McpTransport::Stdio && !row.approved {
            selection.warnings.push(format!(
                "MCP server '{name}' has not been approved to launch its command; omitting it"
            ));
            continue;
        }
        match candidate(registry, row) {
            Ok(candidate) => selection.servers.push(candidate),
            Err(warning) => selection.warnings.push(format!("{warning}; omitting it")),
        }
    }
    Ok(selection)
}

/// Resolve the selection for a new session.
///
/// `explicit` is a client request (`Some`, even empty, wins), `task` is the
/// catalog header, and `None`/`None` means every enabled + approved definition.
/// An unknown *explicit* name is an error; every other omission is a warning.
pub fn resolve_for_launch(
    state: &State,
    explicit: Option<&[String]>,
    task: Option<&[String]>,
) -> anyhow::Result<McpSelection> {
    let registry = state.mcp.read();
    match explicit {
        Some(names) => resolve_names(&registry, names, true),
        None => match task {
            Some(names) => resolve_names(&registry, names, false),
            None => {
                let names = default_names(&registry);
                resolve_names(&registry, &names, false)
            }
        },
    }
}

/// Resolve a session's recorded selection for a restore.
///
/// `None` means the row predates the registry (use the current default); a
/// recorded list replays exactly that set, omitting anything removed, disabled,
/// unapproved or unresolvable with a warning.
pub fn resolve_for_restore(state: &State, recorded: Option<&[String]>) -> McpSelection {
    let registry = state.mcp.read();
    match recorded {
        Some(names) => {
            resolve_names(&registry, names, false).unwrap_or_else(|error| McpSelection {
                warnings: vec![error.to_string()],
                ..McpSelection::default()
            })
        }
        None => {
            let names = default_names(&registry);
            resolve_names(&registry, &names, false).unwrap_or_else(|error| McpSelection {
                warnings: vec![error.to_string()],
                ..McpSelection::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable() -> PathBuf {
        // `/bin/sh` exists on every Unix test host; use the canonical path.
        std::fs::canonicalize("/bin/sh").unwrap()
    }

    fn input(name: &str) -> McpServerInput {
        McpServerInput {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some(executable().to_string_lossy().into_owned()),
            args: vec!["-c".to_string(), "true".to_string()],
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            enabled: None,
        }
    }

    #[test]
    fn rejects_invalid_names_and_missing_commands() {
        let mut server = input("bad name");
        assert!(validate_input(&server, true).is_err());
        server.name = "ok".to_string();
        server.command = Some("definitely-not-a-real-command-anywhere".to_string());
        assert!(validate_input(&server, true).is_err());
    }

    #[test]
    fn requires_transport_specific_fields() {
        let mut server = input("web");
        server.transport = McpTransport::Http;
        assert!(
            validate_input(&server, true).is_err(),
            "http requires a url"
        );
        server.url = Some("ftp://example.com".to_string());
        assert!(
            validate_input(&server, true).is_err(),
            "scheme must be http(s)"
        );
        server.url = Some("https://example.com/mcp".to_string());
        server.command = None;
        assert!(validate_input(&server, true).is_ok());
    }

    #[test]
    fn fingerprint_changes_when_the_command_changes() {
        let server = input("fp");
        let first = validate_input(&server, true).unwrap().fingerprint;
        let mut changed = server.clone();
        changed.args.push("--extra".to_string());
        let second = validate_input(&changed, true).unwrap().fingerprint;
        assert_ne!(first, second);

        let mut renamed_secret = server.clone();
        renamed_secret.env.push(McpSecret {
            name: "TOKEN".to_string(),
            source: McpSecretSource::Env,
            env: Some("TOKEN".to_string()),
            command: Vec::new(),
            value: None,
            has_value: false,
        });
        let third = validate_input(&renamed_secret, true).unwrap().fingerprint;
        assert_ne!(first, third);
    }

    #[test]
    fn literal_values_are_extracted_from_declarations() {
        let mut server = input("literal");
        server.env.push(McpSecret::with_literal("TOKEN", "hunter2"));
        let validated = validate_input(&server, true).unwrap();
        assert_eq!(validated.env[0].source, McpSecretSource::Literal);
        assert!(validated.env[0].value.is_none());
        assert_eq!(validated.env_literals[0].0, "TOKEN");
        assert_eq!(validated.env_literals[0].1, "hunter2");
    }

    #[test]
    fn literal_declaration_without_value_is_rejected_on_add() {
        let mut server = input("literal-missing");
        server.env.push(McpSecret {
            name: "TOKEN".to_string(),
            source: McpSecretSource::Literal,
            env: None,
            command: Vec::new(),
            value: None,
            has_value: false,
        });
        assert!(validate_input(&server, true).is_err());
        assert!(validate_input(&server, false).is_ok());
    }

    #[test]
    fn secret_values_are_redacted_from_debug() {
        let value = Timestamp::now();
        let row = McpServerRow {
            name: "redacted".to_string(),
            transport: McpTransport::Stdio,
            command: Some("/bin/sh".to_string()),
            args: Vec::new(),
            url: None,
            env: vec![McpSecret::new("TOKEN")],
            headers: Vec::new(),
            enabled: true,
            approved: true,
            fingerprint: "f".to_string(),
            source: McpSource::Manual,
            created_at: value,
            updated_at: value,
        };
        let mut registry = McpRegistry::new(Path::new("/tmp"));
        registry.set_literal("redacted", ENV_KIND, "TOKEN", "super-secret".to_string());
        let debug = format!("{row:?}");
        assert!(!debug.contains("super-secret"));
        let secret = McpSecret::with_literal("TOKEN", "super-secret");
        assert!(!format!("{secret:?}").contains("super-secret"));
    }
}
