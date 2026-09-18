//! `mcp-filesystem` — a first-party Model Context Protocol server exposing a
//! small, root-constrained filesystem surface to agents.
//!
//! This is the reference stdio MCP server for M2: it proves the rmcp *server*
//! side of the stack, and is consumed by the favetto's MCP client pool like
//! any third-party server would be. Tools:
//! - `list_dir`   — list entries under a path
//! - `read_file`  — read a UTF-8 text file
//! - `write_file` — write UTF-8 text to a file (creating parent directories)
//!
//! All paths are resolved against `--root` and may not escape it. Because the MCP
//! protocol uses stdout, nothing is ever printed to stdout outside the transport.

use std::path::{Path, PathBuf};

use anyhow::Context;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The filesystem server. `Clone` is required by the router's handler plumbing.
#[derive(Clone)]
struct FilesystemServer {
    root: PathBuf,
}

impl FilesystemServer {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve a user-supplied path strictly inside `root`.
    ///
    /// Handles both absolute (anchored to root) and relative paths, and follows
    /// symlinks via `canonicalize` so `..` traversal cannot escape the root.
    fn resolve(&self, path: &str) -> anyhow::Result<PathBuf> {
        resolve_path(&self.root, path)
    }
}

/// Sandbox helper: resolve `path` strictly inside `root`, refusing `..`/absolute
/// escapes. Shared with tests to prove the filesystem tool is confined.
fn resolve_path(root: &Path, path: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        root.join(p.strip_prefix("/").unwrap_or(p))
    } else {
        root.join(p)
    };

    // If the final component doesn't exist yet (e.g. write_file), canonicalize
    // the parent and re-attach the file name.
    let resolved = if joined.exists() {
        joined.canonicalize()?
    } else {
        let parent = joined.parent().unwrap_or(root);
        let parent_canon = parent.canonicalize()?;
        let name = joined.file_name().context("path has no file name")?;
        parent_canon.join(name)
    };

    let root_canon = root.canonicalize()?;
    if !resolved.starts_with(&root_canon) {
        anyhow::bail!("path escapes root: {path}");
    }
    Ok(resolved)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct PathArg {
    /// Path relative to the workspace root (or absolute, anchored to the root).
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct WriteArgs {
    /// Path to write, relative to the workspace root.
    path: String,
    /// UTF-8 content to write.
    content: String,
}

#[tool_router]
impl FilesystemServer {
    #[tool(description = "List the entries of a directory under the workspace root.")]
    async fn list_dir(&self, Parameters(p): Parameters<PathArg>) -> String {
        let dir = match self.resolve(&p.path) {
            Ok(d) => d,
            Err(e) => return err(&p.path, e),
        };
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                let mut entries: Vec<String> = rd
                    .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                    .collect();
                entries.sort();
                serde_json::json!({ "path": p.path, "entries": entries }).to_string()
            }
            Err(e) => err(&p.path, e.into()),
        }
    }

    #[tool(description = "Read a UTF-8 text file under the workspace root.")]
    async fn read_file(&self, Parameters(p): Parameters<PathArg>) -> String {
        let path = match self.resolve(&p.path) {
            Ok(x) => x,
            Err(e) => return err(&p.path, e),
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::json!({ "path": p.path, "content": content }).to_string(),
            Err(e) => err(&p.path, e.into()),
        }
    }

    #[tool(description = "Write UTF-8 text to a file under the workspace root, creating parent directories.")]
    async fn write_file(&self, Parameters(p): Parameters<WriteArgs>) -> String {
        let path = match self.resolve(&p.path) {
            Ok(x) => x,
            Err(e) => return err(&p.path, e),
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return err(&p.path, e.into());
            }
        }
        match std::fs::write(&path, &p.content) {
            Ok(()) => serde_json::json!({ "path": p.path, "bytes": p.content.len() }).to_string(),
            Err(e) => err(&p.path, e.into()),
        }
    }
}

/// A uniform error shape returned as the tool result (keeps failures readable in logs).
fn err(path: &str, e: anyhow::Error) -> String {
    serde_json::json!({ "path": path, "error": e.to_string() }).to_string()
}

#[tool_handler(
    name = "mcp-filesystem",
    version = "0.1.0",
    instructions = "Read/write files under the workspace root."
)]
impl ServerHandler for FilesystemServer {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = parse_root(std::env::args().skip(1));
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let server = FilesystemServer::new(root);
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Minimal arg parsing: `mcp-filesystem [--root <path>]`.
fn parse_root(mut args: impl Iterator<Item = String>) -> PathBuf {
    let mut root = PathBuf::from(".");
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" | "-r" => {
                if let Some(v) = args.next() {
                    root = PathBuf::from(v);
                }
            }
            _ => {}
        }
    }
    root
}

#[cfg(test)]
mod tests {
    use super::resolve_path;
    use std::fs;
    use std::path::PathBuf;

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("favetto-fs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("file.txt"), "hello").unwrap();
        dir
    }

    #[test]
    fn resolves_within_root() {
        let root = sandbox("within");
        let p = resolve_path(&root, "file.txt").unwrap();
        assert!(p.starts_with(&root));
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "file.txt");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn blocks_dotdot_escape() {
        let root = sandbox("dotdot");
        assert!(resolve_path(&root, "../etc/passwd").is_err());
        assert!(resolve_path(&root, "sub/../../etc/passwd").is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn blocks_absolute_escape() {
        let root = sandbox("absolute");
        // Absolute paths are anchored to the root, so "/etc" resolves under root,
        // not the real /etc.
        let p = resolve_path(&root, "/etc").unwrap();
        assert!(p.starts_with(&root));
        fs::remove_dir_all(&root).ok();
    }
}
