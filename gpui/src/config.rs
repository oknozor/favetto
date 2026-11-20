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

#![allow(dead_code)] // The test-only helpers (`save_to`, `rpc_url`, …) keep the file format and its callers together.

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

/// The appearance preference stored in `gpui.toml`.
///
/// `System` follows the operating system and is the default, so a fresh
/// install keeps matching the desktop environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Follow the operating system's light/dark appearance.
    #[default]
    System,
    /// Always use the dark palette.
    Dark,
    /// Always use the light palette.
    Light,
}

impl ThemeChoice {
    /// The label shown by the Appearance controls.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Dark => "Dark",
            Self::Light => "Light",
        }
    }

    /// Cycle System → Dark → Light → System (the keyboard activate action).
    pub fn cycle(self) -> Self {
        match self {
            Self::System => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::System,
        }
    }
}

/// The row density preference stored in `gpui.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// 32 px rows and the full spacing scale.
    #[default]
    Comfortable,
    /// Tighter spacing for short windows and dense lists.
    Compact,
}

impl Density {
    /// The label shown by the Appearance controls.
    pub fn label(self) -> &'static str {
        match self {
            Self::Comfortable => "Comfortable",
            Self::Compact => "Compact",
        }
    }

    /// Toggle between the two densities.
    pub fn toggled(self) -> Self {
        match self {
            Self::Comfortable => Self::Compact,
            Self::Compact => Self::Comfortable,
        }
    }
}

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
    /// The theme preference saved in `gpui.toml`.
    pub theme: ThemeChoice,
    /// The density preference saved in `gpui.toml`.
    pub density: Density,
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

    /// The token that belongs in `gpui.toml`, if any. A token read from the
    /// shared token file is deliberately not copied into the settings file:
    /// leaving it out keeps the file tiny and keeps the token in one place.
    pub fn token_override(&self) -> Option<&str> {
        match self.token_source {
            TokenSource::ConfigFile(_) => self.token.as_deref(),
            TokenSource::TokenFile(_) | TokenSource::Missing(_) => None,
        }
    }
}

/// The on-disk shape of `~/.config/favetto/gpui.toml`.
///
/// Every field is optional: a missing file, or an empty one, means "use the
/// defaults" (the local endpoint, the shared token file and system appearance).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct FileConfig {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    theme: ThemeChoice,
    #[serde(default)]
    density: Density,
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

    let endpoint_input = file.endpoint.as_deref().unwrap_or_default();
    let endpoint = match validate_endpoint(endpoint_input) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            if !endpoint_input.trim().is_empty() {
                warnings.push(format!(
                    "{}: endpoint {:?}: {err}",
                    config_path.display(),
                    endpoint_input
                ));
            }
            DEFAULT_ENDPOINT.to_string()
        }
    };

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
        theme: file.theme,
        density: file.density,
        warnings,
    }
}

/// Persist `endpoint` and (optionally) `token` to `gpui.toml`, keeping the
/// default appearance.
///
/// Used by tests and by callers that only change the connection; Settings uses
/// [`save_settings_to`] so the appearance survives a connection edit.
pub fn save_to(config_path: &Path, endpoint: &str, token: Option<&str>) -> std::io::Result<()> {
    save_settings_to(
        config_path,
        endpoint,
        token,
        ThemeChoice::default(),
        Density::default(),
    )
}

/// Persist the complete settings file: connection plus appearance.
pub fn save_settings_to(
    config_path: &Path,
    endpoint: &str,
    token: Option<&str>,
    theme: ThemeChoice,
    density: Density,
) -> std::io::Result<()> {
    let file = FileConfig {
        endpoint: Some(normalize_endpoint(endpoint)),
        token: token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        theme,
        density,
    };
    let text = toml::to_string(&file).unwrap_or_default();
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(config_path, text)
}

/// A precise reason an endpoint cannot be used, so Settings can say what to fix
/// instead of silently falling back to the default daemon (Postel's Law still
/// accepts every common shape; it just refuses impossible ones).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    /// The field was left blank.
    #[error("Enter a daemon address such as 127.0.0.1:7878.")]
    Empty,
    /// Whitespace in the middle of an address.
    #[error("The daemon address must not contain spaces.")]
    Whitespace,
    /// A scheme other than `http`/`https`.
    #[error("Only http:// and https:// are supported, not {0}://.")]
    UnsupportedScheme(String),
    /// Nothing that names a host.
    #[error("Enter a host name or IP address after the scheme, such as 127.0.0.1:7878.")]
    MissingHost,
    /// The address includes credentials, which this client does not support.
    #[error("Remove the username/password from the address; the token is configured separately.")]
    Credentials,
    /// A query string or fragment does not belong on a daemon base URL.
    #[error("A daemon address cannot include a query string or fragment.")]
    Query,
    /// The URL parser could not make sense of the address.
    #[error("{input:?} is not a valid daemon address: {reason}")]
    Invalid {
        /// What the user typed.
        input: String,
        /// The parser's explanation.
        reason: String,
    },
}

/// Validate and normalize a user-entered endpoint into a canonical base URL.
///
/// Accepts `host:port`, `http(s)://host[:port][/path]`, a full `/rpc` or
/// `/events` URL, surrounding whitespace, and trailing slashes. Unlike
/// [`normalize_endpoint`], an unusable address is reported precisely instead of
/// silently falling back to the default daemon.
pub fn validate_endpoint(input: &str) -> Result<String, EndpointError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(EndpointError::Empty);
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(EndpointError::Whitespace);
    }

    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            if scheme != "http" && scheme != "https" {
                return Err(EndpointError::UnsupportedScheme(scheme));
            }
            (scheme, rest.to_string())
        }
        None => ("http".to_string(), trimmed.to_string()),
    };

    // `host:port/rpc/` → `host:port/rpc` → `host:port`.
    let mut rest = rest.trim_matches('/').to_string();
    for path in [RPC_PATH, EVENTS_PATH] {
        if let Some(base) = rest.strip_suffix(path) {
            rest = base.trim_end_matches('/').to_string();
        }
    }
    if rest.is_empty() {
        return Err(EndpointError::MissingHost);
    }

    let candidate = format!("{scheme}://{rest}");
    let url = reqwest::Url::parse(&candidate).map_err(|err| EndpointError::Invalid {
        input: trimmed.to_string(),
        reason: err.to_string(),
    })?;
    if url.host_str().is_none_or(str::is_empty) {
        return Err(EndpointError::MissingHost);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EndpointError::Credentials);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(EndpointError::Query);
    }

    Ok(format!("{scheme}://{rest}"))
}

/// Normalize a user-entered endpoint into a canonical base URL.
///
/// Accepts the same forms as [`validate_endpoint`]. Anything unusable falls
/// back to [`DEFAULT_ENDPOINT`] so a malformed config file still connects to
/// the local daemon; Settings uses [`validate_endpoint`] to show the fix first.
pub fn normalize_endpoint(input: &str) -> String {
    validate_endpoint(input).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string())
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
            theme: ThemeChoice::System,
            density: Density::Comfortable,
            warnings: Vec::new(),
        };
        assert_eq!(config.rpc_url(), "http://host:7878/rpc");
        assert_eq!(config.events_url(), "http://host:7878/events");
    }

    #[test]
    fn endpoint_validation_explains_each_rejection() {
        assert_eq!(validate_endpoint("  "), Err(EndpointError::Empty));
        assert_eq!(
            validate_endpoint("localhost:7878 please"),
            Err(EndpointError::Whitespace)
        );
        assert_eq!(
            validate_endpoint("ftp://example.com"),
            Err(EndpointError::UnsupportedScheme("ftp".to_string()))
        );
        assert!(matches!(
            validate_endpoint("http://"),
            Err(EndpointError::MissingHost)
        ));
        assert!(matches!(
            validate_endpoint("user:pass@example.com:7878"),
            Err(EndpointError::Credentials)
        ));
        assert!(matches!(
            validate_endpoint("http://127.0.0.1:7878?token=x"),
            Err(EndpointError::Query)
        ));
        assert!(matches!(
            validate_endpoint("127.0.0.1:notaport"),
            Err(EndpointError::Invalid { .. })
        ));

        // Every accepted form normalizes to one base URL.
        assert_eq!(
            validate_endpoint("127.0.0.1:7878"),
            Ok("http://127.0.0.1:7878".to_string())
        );
        assert_eq!(
            validate_endpoint("https://example.com/favetto/rpc/"),
            Ok("https://example.com/favetto".to_string())
        );
    }

    #[test]
    fn theme_and_density_round_trip_through_the_config_file() {
        let dir = temp_dir("appearance");
        let config_path = dir.join("gpui.toml");
        save_settings_to(
            &config_path,
            "127.0.0.1:7878",
            None,
            ThemeChoice::Dark,
            Density::Compact,
        )
        .expect("save");

        let config = load_from(&config_path, &dir.join("missing-token"));
        assert_eq!(config.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(config.theme, ThemeChoice::Dark);
        assert_eq!(config.density, Density::Compact);
        assert!(config.token.is_none());
        assert!(config.token_override().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_token_from_the_shared_file_is_not_copied_into_the_settings_file() {
        let dir = temp_dir("token-source");
        let token_path = dir.join("token");
        fs::write(&token_path, "shared").expect("write token");

        let from_file = load_from(&dir.join("gpui.toml"), &token_path);
        assert_eq!(from_file.token.as_deref(), Some("shared"));
        assert_eq!(from_file.token_override(), None);

        let config_path = dir.join("gpui.toml");
        fs::write(&config_path, "token = \"override\"\n").expect("write config");
        let from_config = load_from(&config_path, &token_path);
        assert_eq!(from_config.token_override(), Some("override"));
        let _ = fs::remove_dir_all(dir);
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
