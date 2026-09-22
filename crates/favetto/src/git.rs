//! Non-interactive `git` provisioning for agent processes.
//!
//! favetto never commits anything itself. It only shapes the environment the
//! **agent's** `git` sees, so an agent commit can never block on a
//! pinentry/askpass prompt while a session runs unattended.
//!
//! The settings are injected purely through git's *environment config*
//! (`GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<i>` / `GIT_CONFIG_VALUE_<i>`, git ≥
//! 2.31) plus `GIT_AUTHOR_*` / `GIT_COMMITTER_*`. The operator's real global,
//! repo, or worktree config is never modified. An absent `[git]` section still
//! forces `commit.gpgsign = false`, which is the fix for the reported hang;
//! `signing = "ssh"` / `"gpg"` opt back into signed agent commits with a
//! dedicated identity.
//!
//! A passphrase can come from `passphrase_env` (forwarded into the child's
//! environment) or `passphrase_command` (embedded in a generated wrapper that
//! reads the secret at signing time, so it never lands in the config file or in
//! the agent's environment).

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{GitSettings, GitSigning};

/// Overlay the fields present in `over` onto a clone of `base`.
///
/// "Present" is any `Some(..)` field, so a per-agent override only replaces the
/// values it sets and inherits the rest.
pub fn overlay(base: &GitSettings, over: &GitSettings) -> GitSettings {
    let mut merged = base.clone();
    if let Some(v) = over.signing {
        merged.signing = Some(v);
    }
    if let Some(v) = &over.user_name {
        merged.user_name = Some(v.clone());
    }
    if let Some(v) = &over.user_email {
        merged.user_email = Some(v.clone());
    }
    if let Some(v) = &over.signing_key {
        merged.signing_key = Some(v.clone());
    }
    if let Some(v) = &over.passphrase_env {
        merged.passphrase_env = Some(v.clone());
    }
    if let Some(v) = &over.passphrase_command {
        merged.passphrase_command = Some(v.clone());
    }
    merged
}

/// Resolve the environment a launch should add so the agent's `git` behaves.
///
/// Returns the `GIT_CONFIG_*`, identity, and (when needed) askpass variables.
/// `data_dir` holds the generated askpass/gpg wrapper scripts; a signing mode of
/// `off` never touches the filesystem.
pub fn resolve_env(
    settings: &GitSettings,
    data_dir: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut config: Vec<(String, String)> = Vec::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();

    if let Some(name) = &settings.user_name {
        config.push(("user.name".to_string(), name.clone()));
        env.insert("GIT_AUTHOR_NAME".to_string(), name.clone());
        env.insert("GIT_COMMITTER_NAME".to_string(), name.clone());
    }
    if let Some(email) = &settings.user_email {
        config.push(("user.email".to_string(), email.clone()));
        env.insert("GIT_AUTHOR_EMAIL".to_string(), email.clone());
        env.insert("GIT_COMMITTER_EMAIL".to_string(), email.clone());
    }

    match settings.effective_signing() {
        GitSigning::Off => {
            // The whole point: force unsigned commits so nothing can prompt.
            config.push(("commit.gpgsign".to_string(), "false".to_string()));
        }
        GitSigning::Ssh => {
            let key = required_key(settings, "ssh")?;
            config.push(("commit.gpgsign".to_string(), "true".to_string()));
            config.push(("gpg.format".to_string(), "ssh".to_string()));
            config.push(("user.signingkey".to_string(), key));

            if let Some(pass) = resolve_passphrase(settings)? {
                forward_passphrase_env(&pass, &mut env);
                let script = data_dir.join("git/favetto-askpass.sh");
                write_script(&script, &format!("#!/bin/sh\n{}\n", pass.capture()))?;
                let script = script.to_string_lossy().into_owned();
                env.insert("SSH_ASKPASS".to_string(), script.clone());
                env.insert("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string());
                env.insert("GIT_ASKPASS".to_string(), script);
                // OpenSSH only consults an askpass helper when a display is set
                // (older versions) or `SSH_ASKPASS_REQUIRE=force` is honored.
                env.insert("DISPLAY".to_string(), ":0".to_string());
            }
        }
        GitSigning::Gpg => {
            let key = required_key(settings, "gpg")?;
            config.push(("commit.gpgsign".to_string(), "true".to_string()));
            config.push(("gpg.format".to_string(), "openpgp".to_string()));
            config.push(("user.signingkey".to_string(), key));

            if let Some(pass) = resolve_passphrase(settings)? {
                forward_passphrase_env(&pass, &mut env);
                let script = data_dir.join("git/favetto-gpg.sh");
                let body = format!(
                    "#!/bin/sh\nexec gpg --pinentry-mode loopback --passphrase-fd 3 \"$@\" 3<<EOF\n$({})\nEOF\n",
                    pass.capture()
                );
                write_script(&script, &body)?;
                config.push((
                    "gpg.program".to_string(),
                    script.to_string_lossy().into_owned(),
                ));
            }
        }
    }

    let count = config.len().to_string();
    env.insert("GIT_CONFIG_COUNT".to_string(), count);
    for (i, (key, value)) in config.iter().enumerate() {
        env.insert(format!("GIT_CONFIG_KEY_{i}"), key.clone());
        env.insert(format!("GIT_CONFIG_VALUE_{i}"), value.clone());
    }
    Ok(env)
}

/// A resolved passphrase source.
enum Passphrase {
    /// Read from an environment variable forwarded into the agent.
    Env { var: String, value: String },
    /// Read by running a command at signing time.
    Command(Vec<String>),
}

impl Passphrase {
    /// A shell fragment that writes the passphrase to stdout.
    fn capture(&self) -> String {
        match self {
            Passphrase::Env { var, .. } => format!("printf '%s\\n' \"${{{var}}}\""),
            Passphrase::Command(argv) => argv
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// Forward the passphrase env var (when that source is used) into the child.
fn forward_passphrase_env(pass: &Passphrase, env: &mut BTreeMap<String, String>) {
    if let Passphrase::Env { var, value } = pass {
        env.insert(var.clone(), value.clone());
    }
}

fn resolve_passphrase(settings: &GitSettings) -> anyhow::Result<Option<Passphrase>> {
    if let Some(var) = &settings.passphrase_env {
        if !is_valid_env_name(var) {
            anyhow::bail!("git.passphrase_env '{var}' is not a valid environment variable name");
        }
        let value = std::env::var(var).map_err(|_| {
            anyhow::anyhow!("git.passphrase_env '{var}' is not set in the daemon environment")
        })?;
        return Ok(Some(Passphrase::Env {
            var: var.clone(),
            value,
        }));
    }
    if let Some(argv) = &settings.passphrase_command {
        if argv.is_empty() {
            anyhow::bail!("git.passphrase_command must not be empty");
        }
        return Ok(Some(Passphrase::Command(argv.clone())));
    }
    Ok(None)
}

fn required_key(settings: &GitSettings, mode: &str) -> anyhow::Result<String> {
    match settings.signing_key.as_deref() {
        Some(key) if !key.trim().is_empty() => Ok(expand_tilde(key)),
        _ => anyhow::bail!(
            "git.signing = \"{mode}\" requires git.signing_key \
             (a gpg key id/fingerprint, or an ssh public-key path/literal key)"
        ),
    }
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// POSIX single-quote a value for safe embedding in a `/bin/sh` script.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Write a `0700` script, creating its `0700` parent directory.
fn write_script(path: &Path, body: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700)?;
    }
    std::fs::write(path, body)?;
    set_mode(path, 0o700)?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Expand a leading `~/` (or a bare `~`) to the current user's home directory.
fn expand_tilde(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    } else if value == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("favetto-git-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Decode the `GIT_CONFIG_*` env encoding back into a key/value map.
    fn config_map(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let count: usize = env["GIT_CONFIG_COUNT"].parse().unwrap();
        (0..count)
            .map(|i| {
                (
                    env[&format!("GIT_CONFIG_KEY_{i}")].clone(),
                    env[&format!("GIT_CONFIG_VALUE_{i}")].clone(),
                )
            })
            .collect()
    }

    #[test]
    fn overlay_applies_only_present_fields() {
        let base = GitSettings {
            signing: Some(GitSigning::Off),
            user_name: Some("base".to_string()),
            user_email: Some("base@example.com".to_string()),
            signing_key: Some("base-key".to_string()),
            passphrase_env: None,
            passphrase_command: None,
        };
        let over = GitSettings {
            signing: Some(GitSigning::Ssh),
            user_name: None,
            user_email: Some("over@example.com".to_string()),
            ..Default::default()
        };
        let merged = overlay(&base, &over);
        assert_eq!(merged.effective_signing(), GitSigning::Ssh);
        // Untouched fields keep the base value.
        assert_eq!(merged.user_name.as_deref(), Some("base"));
        assert_eq!(merged.user_email.as_deref(), Some("over@example.com"));
        assert_eq!(merged.signing_key.as_deref(), Some("base-key"));
    }

    #[test]
    fn off_emits_commit_gpgsign_false() {
        let dir = temp_dir("off");
        let env = resolve_env(&GitSettings::default(), &dir).unwrap();
        assert_eq!(env.len(), 3, "unexpected env: {env:?}");
        assert_eq!(env["GIT_CONFIG_COUNT"], "1");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "commit.gpgsign");
        assert_eq!(env["GIT_CONFIG_VALUE_0"], "false");
        // Off never materializes a script.
        assert!(!dir.join("git").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_sets_git_author_and_committer() {
        let dir = temp_dir("identity");
        let settings = GitSettings {
            user_name: Some("favetto agent".to_string()),
            user_email: Some("agent@favetto.local".to_string()),
            ..Default::default()
        };
        let env = resolve_env(&settings, &dir).unwrap();
        assert_eq!(env["GIT_AUTHOR_NAME"], "favetto agent");
        assert_eq!(env["GIT_COMMITTER_NAME"], "favetto agent");
        assert_eq!(env["GIT_AUTHOR_EMAIL"], "agent@favetto.local");
        assert_eq!(env["GIT_COMMITTER_EMAIL"], "agent@favetto.local");
        let config = config_map(&env);
        assert_eq!(config["user.name"], "favetto agent");
        assert_eq!(config["user.email"], "agent@favetto.local");
        // Default signing is still off.
        assert_eq!(config["commit.gpgsign"], "false");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_emits_format_and_signing_key() {
        let dir = temp_dir("ssh");
        let settings = GitSettings {
            signing: Some(GitSigning::Ssh),
            signing_key: Some("~/.ssh/agent_ed25519.pub".to_string()),
            ..Default::default()
        };
        let env = resolve_env(&settings, &dir).unwrap();
        let config = config_map(&env);
        assert_eq!(config["commit.gpgsign"], "true");
        assert_eq!(config["gpg.format"], "ssh");
        assert_eq!(
            config["user.signingkey"],
            expand_tilde("~/.ssh/agent_ed25519.pub")
        );
        // No passphrase source -> no askpass plumbing.
        assert!(!env.contains_key("SSH_ASKPASS"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_without_signing_key_errors() {
        let dir = temp_dir("ssh-nokey");
        let settings = GitSettings {
            signing: Some(GitSigning::Ssh),
            ..Default::default()
        };
        let err = resolve_env(&settings, &dir).unwrap_err().to_string();
        assert!(err.contains("signing_key"), "error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpg_without_signing_key_errors() {
        let dir = temp_dir("gpg-nokey");
        let settings = GitSettings {
            signing: Some(GitSigning::Gpg),
            ..Default::default()
        };
        let err = resolve_env(&settings, &dir).unwrap_err().to_string();
        assert!(err.contains("signing_key"), "error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passphrase_env_is_forwarded_and_referenced() {
        let dir = temp_dir("pass-env");
        let var = format!("FAVETTO_TEST_PASS_{}", std::process::id());
        std::env::set_var(&var, "hunter2");
        let settings = GitSettings {
            signing: Some(GitSigning::Ssh),
            signing_key: Some("k".to_string()),
            passphrase_env: Some(var.clone()),
            ..Default::default()
        };
        let env = resolve_env(&settings, &dir).unwrap();
        // The secret is forwarded to the child under the configured name.
        assert_eq!(env[&var], "hunter2");
        assert_eq!(env["SSH_ASKPASS_REQUIRE"], "force");
        assert!(env.contains_key("SSH_ASKPASS"));
        assert_eq!(env["GIT_ASKPASS"], env["SSH_ASKPASS"]);

        let script = std::fs::read_to_string(env["SSH_ASKPASS"].as_str()).unwrap();
        assert!(script.contains(&var), "script: {script}");
        assert!(!script.contains("hunter2"), "secret leaked into script");
        std::env::remove_var(&var);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passphrase_env_missing_errors() {
        let dir = temp_dir("pass-env-missing");
        let var = format!("FAVETTO_TEST_ABSENT_{}", std::process::id());
        std::env::remove_var(&var);
        let settings = GitSettings {
            signing: Some(GitSigning::Ssh),
            signing_key: Some("k".to_string()),
            passphrase_env: Some(var.clone()),
            ..Default::default()
        };
        let err = resolve_env(&settings, &dir).unwrap_err().to_string();
        assert!(err.contains(&var), "error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passphrase_command_materializes_wrapper_without_secret() {
        let dir = temp_dir("pass-cmd");
        let settings = GitSettings {
            signing: Some(GitSigning::Gpg),
            signing_key: Some("ABC123".to_string()),
            passphrase_command: Some(vec![
                "secret-tool".to_string(),
                "lookup".to_string(),
                "service".to_string(),
                "favetto".to_string(),
            ]),
            ..Default::default()
        };
        let env = resolve_env(&settings, &dir).unwrap();
        let wrapper = dir.join("git/favetto-gpg.sh");
        assert!(wrapper.exists());
        let meta = std::fs::metadata(&wrapper).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);

        let script = std::fs::read_to_string(&wrapper).unwrap();
        assert!(script.contains("secret-tool"), "script: {script}");
        assert!(
            script.contains("--pinentry-mode loopback"),
            "script: {script}"
        );
        // The passphrase itself is never embedded; only the command that fetches it.
        assert!(!script.contains("hunter2"));
        let config = config_map(&env);
        assert_eq!(config["gpg.program"], wrapper.to_string_lossy());
        // The secret command is not duplicated into the environment.
        assert!(!env.values().any(|v| v.contains("secret-tool")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
