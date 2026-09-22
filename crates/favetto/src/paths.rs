//! Shared path helpers.
//!
//! User-facing settings (config file and CLI) may spell paths with a leading
//! `~`, as `config.example.toml` and the README do. Rust's `PathBuf` does not
//! expand it, so a value like `~/.local/share/favetto/worktrees` would be
//! treated as relative and silently joined onto another directory. Every
//! configured path that reaches the filesystem goes through [`expand_tilde`]
//! first.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/` to the user's home directory.
///
/// Only a path whose first component is exactly `~` is expanded, so `~` becomes
/// the home directory and `~/foo/bar` becomes `<home>/foo/bar`. A `~user` form
/// (the first component merely starts with `~`) is left untouched, as are
/// absolute and relative paths. If the home directory cannot be determined the
/// path is returned unchanged.
pub fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    let Some(home) = dirs::home_dir() else {
        return path.to_path_buf();
    };
    if rest.as_os_str().is_empty() {
        home
    } else {
        home.join(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_leading_tilde() {
        let home = dirs::home_dir().expect("home directory");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/worktrees"), home.join("worktrees"));
        assert_eq!(
            expand_tilde("~/.local/share/favetto/worktrees"),
            home.join(".local/share/favetto/worktrees")
        );
    }

    #[test]
    fn leaves_non_tilde_paths_untouched() {
        assert_eq!(expand_tilde("relative/dir"), PathBuf::from("relative/dir"));
        assert_eq!(
            expand_tilde("/absolute/dir"),
            PathBuf::from("/absolute/dir")
        );
        // A `~user` form is not expanded; only a literal `~` component is.
        assert_eq!(expand_tilde("~other/dir"), PathBuf::from("~other/dir"));
    }
}
