//! Connection settings: endpoint and bearer token.
//!
//! The desktop client reads `~/.config/favetto/gpui.toml` (optional) and falls
//! back to the daemon's default local endpoint and the token file the TUI and
//! web client already use (`~/.local/share/favetto/token`).
//!
//! Endpoint input follows **Postel's Law** (docs/design/gpui-client.md §2): the
//! user may type `127.0.0.1:7878`, `http://127.0.0.1:7878/`, or paste the full
//! `/rpc` URL from the API reference. [`normalize_endpoint`] accepts all of them
//! and yields one canonical base URL, so [`ClientConfig::rpc_url`] and
//! [`ClientConfig::events_url`] can never disagree about where the daemon lives.

#![allow(dead_code)] // `save` and the token-source helpers are consumed by step 09.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The daemon a fresh install talks to.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:7878";

/// Well-known daemon paths, appended to the normalized endpoint.
pub const RPC_PATH: &str = "/rpc";
/// The authenticated SSE push channel.
pub const EVENTS_PATH: &str = "/events";

/// Where the bearer token came from, so an `Unauthorized` state can point the
/// user at the file to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// `token = "…"` in `gpui.toml`.
    ConfigFile(PathBuf),
    /// The daemon's token file (`~/.local/share/favetto/token`).
    TokenFile(PathBuf),
    /// No token was configured; the path is where one would be read from.
    Missing(PathBuf),
}

impl TokenSource {
    /// A human-readable path for the connection banner / settings screen.
    pub fn label(&self) -> String {
        match self {
            Self::ConfigFile(path) => format!("{} (token = …)", path.display()),
            Self::TokenFile(path) => path.display().to_string(),
            Self::Missing(path) => format!("{} (file not found)", path.display()),
        }
    }

    /// The path the token was (or would be) read from.
    pub fn path(&self) -> &Path {
        match self {
            Self::ConfigFile(path) | Self::TokenFile(path) | Self::Missing(path) => path,
        }
    }
}

/// The effective connection settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// A normalized base URL (no trailing slash, no `/rpc` suffix).
    pub endpoint: String,
    /// The trimmed bearer token, if one was found.
    pub token: Option<String>,
    /// Where `token` came from, or where it was looked for.
    pub token_source: TokenSource,
    /// The `gpui.toml` path that was consulted (used when saving in Settings).
    pub config_path: PathBuf,
    /// Non-fatal problems found while loading (a malformed config or token
    /// file). The connection still starts with defaults.
    pub warnings: Vec<String>,
}

impl ClientConfig {
    /// `POST` target: `{endpoint}/rpc`.
    pub fn rpc_url(&self) -> String {
        join_url(&self.endpoint, RPC_PATH)
    }

    /// `GET` target: `{endpoint}/events`.
    pub fn events_url(&self) -> String {
        join_url(&self.endpoint, EVENTS_PATH)
    }
}

/// The on-disk shape of `~/.config/favetto/gpui.toml`.
///
/// Both fields are optional: a missing file, or an empty one, means "use the
/// defaults" (the local endpoint and the shared token file).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct FileConfig {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

/// `~/.config/favetto/gpui.toml` (respecting `XDG_CONFIG_HOME` via `dirs`).
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("favetto")
        .join("gpui.toml")
}

/// `~/.local/share/favetto/token` (respecting `XDG_DATA_HOME` via `dirs`).
pub fn default_token_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("favetto")
        .join("token")
}

/// Load the default config and token files.
pub fn load() -> ClientConfig {
    load_from(&default_config_path(), &default_token_path())
}

/// Load from explicit paths (separated so tests never touch the user's home).
pub fn load_from(config_path: &Path, token_path: &Path) -> ClientConfig {
    let mut warnings = Vec::new();

    let file = match fs::read_to_string(config_path) {
        Ok(text) => match toml::from_str::<FileConfig>(&text) {
            Ok(file) => file,
            Err(err) => {
                warnings.push(format!("{}: {err}", config_path.display()));
                FileConfig::default()
            }
        },
        Err(err) if err.kind() == ErrorKind::NotFound => FileConfig::default(),
        Err(err) => {
            warnings.push(format!("{}: {err}", config_path.display()));
            FileConfig::default()
        }
    };

    let endpoint = normalize_endpoint(file.endpoint.as_deref().unwrap_or_default());

    // A token in `gpui.toml` wins; otherwise fall back to the shared token file.
    let config_token = file
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let (token, token_source) = match config_token {
        Some(token) => (
            Some(token.to_string()),
            TokenSource::ConfigFile(config_path.to_path_buf()),
        ),
        None => match fs::read_to_string(token_path) {
            Ok(raw) => {
                let token = raw.trim();
                if token.is_empty() {
                    (None, TokenSource::Missing(token_path.to_path_buf()))
                } else {
                    (
                        Some(token.to_string()),
                        TokenSource::TokenFile(token_path.to_path_buf()),
                    )
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                (None, TokenSource::Missing(token_path.to_path_buf()))
            }
            Err(err) => {
                warnings.push(format!("{}: {err}", token_path.display()));
                (None, TokenSource::Missing(token_path.to_path_buf()))
            }
        },
    };

    ClientConfig {
        endpoint,
        token,
        token_source,
        config_path: config_path.to_path_buf(),
        warnings,
    }
}

/// Persist `endpoint` and (optionally) `token` to `gpui.toml`.
///
/// Used by the Settings surface in step 09; kept beside [`load_from`] so the
/// file format has one owner.
pub fn save_to(config_path: &Path, endpoint: &str, token: Option<&str>) -> std::io::Result<()> {
    let file = FileConfig {
        endpoint: Some(normalize_endpoint(endpoint)),
        token: token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
    };
    let text = toml::to_string(&file).unwrap_or_default();
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(config_path, text)
}

/// Normalize a user-entered endpoint into a canonical base URL.
///
/// Accepts `host:port`, `http(s)://host[:port][/path]`, a full `/rpc` or
/// `/events` URL, surrounding whitespace, and trailing slashes. Anything that
/// cannot name a host falls back to [`DEFAULT_ENDPOINT`] rather than failing the
/// whole connection.
pub fn normalize_endpoint(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return DEFAULT_ENDPOINT.to_string();
    }

    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            if scheme != "http" && scheme != "https" {
                return DEFAULT_ENDPOINT.to_string();
            }
            (scheme, rest)
        }
        None => ("http".to_string(), trimmed),
    };

    // `host:port/rpc/` → `host:port/rpc` → `host:port`.
    let mut rest = rest.trim_matches('/').to_string();
    for path in [RPC_PATH, EVENTS_PATH] {
        if let Some(base) = rest.strip_suffix(path) {
            rest = base.trim_end_matches('/').to_string();
        }
    }
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return DEFAULT_ENDPOINT.to_string();
    }

    format!("{scheme}://{rest}")
}

/// `POST` target for an endpoint in any accepted form.
pub fn rpc_url_for(endpoint: &str) -> String {
    join_url(&normalize_endpoint(endpoint), RPC_PATH)
}

/// `GET` target for an endpoint in any accepted form.
pub fn events_url_for(endpoint: &str) -> String {
    join_url(&normalize_endpoint(endpoint), EVENTS_PATH)
}

/// Join a normalized base to a daemon path.
fn join_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "favetto-gpui-config-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn endpoint_accepts_host_port_and_schemes() {
        assert_eq!(
            normalize_endpoint("127.0.0.1:7878"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            normalize_endpoint("  localhost:7878  "),
            "http://localhost:7878"
        );
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:7878/"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            normalize_endpoint("HTTPS://favetto.example.com"),
            "https://favetto.example.com"
        );
    }

    #[test]
    fn endpoint_strips_daemon_paths_and_trailing_slashes() {
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:7878/rpc"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:7878/rpc/"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            normalize_endpoint("https://example.com/events"),
            "https://example.com"
        );
        // A path prefix is preserved: only the well-known suffix is removed.
        assert_eq!(
            normalize_endpoint("https://example.com/favetto/rpc"),
            "https://example.com/favetto"
        );
    }

    #[test]
    fn endpoint_falls_back_for_unusable_input() {
        for input in ["", "   ", "http://", "https://", "ftp://example.com", "a b"] {
            assert_eq!(
                normalize_endpoint(input),
                DEFAULT_ENDPOINT,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn urls_are_joined_without_double_slashes() {
        let config = ClientConfig {
            endpoint: "http://host:7878".to_string(),
            token: None,
            token_source: TokenSource::Missing(PathBuf::from("/token")),
            config_path: PathBuf::from("/gpui.toml"),
            warnings: Vec::new(),
        };
        assert_eq!(config.rpc_url(), "http://host:7878/rpc");
        assert_eq!(config.events_url(), "http://host:7878/events");
    }

    #[test]
    fn missing_files_yield_the_default_endpoint_and_no_token() {
        let dir = temp_dir("missing");
        let config = load_from(&dir.join("gpui.toml"), &dir.join("token"));
        assert_eq!(config.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(config.token, None);
        assert!(matches!(config.token_source, TokenSource::Missing(_)));
        assert!(config.warnings.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn token_file_is_trimmed_and_used_as_a_fallback() {
        let dir = temp_dir("token");
        let token_path = dir.join("token");
        fs::write(&token_path, "  secret-token\n").expect("write token");

        let config = load_from(&dir.join("gpui.toml"), &token_path);
        assert_eq!(config.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(config.token.as_deref(), Some("secret-token"));
        assert!(matches!(config.token_source, TokenSource::TokenFile(_)));
        assert!(config.token_source.label().ends_with("token"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn config_file_endpoint_and_token_take_precedence() {
        let dir = temp_dir("config");
        let config_path = dir.join("gpui.toml");
        let token_path = dir.join("token");
        fs::write(&token_path, "file-token").expect("write token");
        fs::write(
            &config_path,
            "endpoint = \"192.168.1.10:9000/rpc\"\ntoken = \" config-token \"\n",
        )
        .expect("write config");

        let config = load_from(&config_path, &token_path);
        assert_eq!(config.endpoint, "http://192.168.1.10:9000");
        assert_eq!(config.token.as_deref(), Some("config-token"));
        assert!(matches!(config.token_source, TokenSource::ConfigFile(_)));
        assert!(config.warnings.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_config_warns_and_falls_back() {
        let dir = temp_dir("bad");
        let config_path = dir.join("gpui.toml");
        fs::write(&config_path, "endpoint = [not toml").expect("write config");

        let config = load_from(&config_path, &dir.join("token"));
        assert_eq!(config.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("gpui.toml"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_round_trips_through_load() {
        let dir = temp_dir("save");
        let config_path = dir.join("gpui.toml");
        save_to(&config_path, "https://example.com/rpc", Some(" tok ")).expect("save");

        let config = load_from(&config_path, &dir.join("missing-token"));
        assert_eq!(config.endpoint, "https://example.com");
        assert_eq!(config.token.as_deref(), Some("tok"));
        let _ = fs::remove_dir_all(dir);
    }
}
