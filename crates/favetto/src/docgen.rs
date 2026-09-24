//! Generates the reference documentation from the source of truth.
//!
//! `favetto __doc` (a hidden subcommand) renders:
//!
//! - `docs/reference/config.md` from the `schemars` JSON Schema of
//!   [`crate::config::FavettoConfig`],
//! - `docs/reference/cli.md` from the clap command tree,
//! - `docs/reference/events.md` from [`favetto_core::model::EventKind::ALL`],
//! - `docs/reference/remote-api.md` from
//!   [`favetto_core::rpc::CLIENT_METHODS`] / [`favetto_core::rpc::SERVER_PUSHES`],
//! - `docs/public/favetto-schema.json`, the raw schema.
//!
//! The functions that build the Markdown are pure so they can be unit-tested;
//! [`run`] only writes their output. The config renderer is adapted from
//! cocogitto's `cog-doc` Markdown renderer, generalized to walk the schema as a
//! [`serde_json::Value`] so it works with both schemars 1.x (`$defs`) and 0.8
//! (`definitions`).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use favetto_core::model::EventKind;
use favetto_core::rpc::{self, error_code};

use crate::cli::{Cli, DocArgs};
use crate::config::FavettoConfig;

/// Resolve the docs directory: an explicit `--docs-dir`, else `<repo>/docs`.
pub fn docs_dir(explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(dir) => dir.to_path_buf(),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("crates/favetto always has a repository root two levels up")
            .join("docs"),
    }
}

/// Escape a string for use inside a Markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `$defs` / `definitions` map of a schema document, if any.
fn defs(schema: &serde_json::Value) -> Option<&serde_json::Value> {
    schema.get("$defs").or_else(|| schema.get("definitions"))
}

/// The name of a `$ref` (e.g. `#/$defs/AgentSettings` -> `AgentSettings`).
fn ref_name(reference: &str) -> &str {
    reference
        .rsplit('/')
        .next()
        .unwrap_or(reference)
        .trim_end_matches(".json")
}

/// A human-readable label for a schema node, resolving `$ref`s by name.
fn type_label(node: &serde_json::Value) -> String {
    if let Some(reference) = node.get("$ref").and_then(|v| v.as_str()) {
        return ref_name(reference).to_string();
    }
    if let Some(values) = node.get("enum").and_then(|v| v.as_array()) {
        return values
            .iter()
            .filter_map(|v| v.as_str())
            .map(|v| format!("`{v}`"))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(variants) = node.get("oneOf").or_else(|| node.get("anyOf")) {
        if let Some(variants) = variants.as_array() {
            let mut labels: Vec<String> = variants
                .iter()
                .map(type_label)
                .filter(|label| label != "null")
                .collect();
            labels.dedup();
            let nullable = variants
                .iter()
                .any(|v| v.get("type").and_then(|t| t.as_str()) == Some("null"));
            let joined = labels.join(" | ");
            return if nullable {
                format!("{joined} (optional)")
            } else {
                joined
            };
        }
    }
    if let Some(types) = node.get("type") {
        if let Some(list) = types.as_array() {
            let non_null: Vec<String> = list
                .iter()
                .filter_map(|v| v.as_str())
                .filter(|t| *t != "null")
                .map(|kind| single_type_label(kind, node))
                .collect();
            let nullable = list.iter().any(|v| v.as_str() == Some("null"));
            let joined = non_null.join(" | ");
            return if nullable {
                format!("{joined} (optional)")
            } else {
                joined
            };
        }
        if let Some(kind) = types.as_str() {
            return single_type_label(kind, node);
        }
    }
    "any".to_string()
}

/// Label a single JSON Schema `type` keyword, given its containing node so
/// `array` (via `items`) and `object` (via `additionalProperties`) can be
/// elaborated.
fn single_type_label(kind: &str, node: &serde_json::Value) -> String {
    match kind {
        "object" => object_type_label(node),
        "array" => node
            .get("items")
            .map(|items| format!("{}[]", type_label(items)))
            .unwrap_or_else(|| "array".to_string()),
        other => other.to_string(),
    }
}

/// `Map<String, T>` for a map-typed object, `object` otherwise.
fn object_type_label(node: &serde_json::Value) -> String {
    match node.get("additionalProperties") {
        Some(serde_json::Value::Bool(true)) => "object".to_string(),
        Some(additional) => format!("Map<String, {}>", type_label(additional)),
        None => "object".to_string(),
    }
}

/// Render a JSON default value compactly for the `Default` table column.
fn default_label(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::String(s) => format!("`{s}`"),
        other => format!("`{other}`"),
    }
}

fn description(node: &serde_json::Value) -> String {
    node.get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Render one object node as a `## <Title>` heading plus a field table.
fn render_object(out: &mut String, title: &str, node: &serde_json::Value) {
    let _ = writeln!(out, "## {title}\n");
    let desc = description(node);
    if !desc.is_empty() {
        let _ = writeln!(out, "{}\n", desc.trim());
    }

    let Some(properties) = node.get("properties").and_then(|v| v.as_object()) else {
        let _ = writeln!(out, "{}\n", type_label(node));
        return;
    };

    let required: Vec<&str> = node
        .get("required")
        .and_then(|v| v.as_array())
        .map(|values| values.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let _ = writeln!(out, "| Field | Type | Default | Required | Description |");
    let _ = writeln!(out, "|-------|------|---------|----------|-------------|");
    for (name, property) in properties {
        let is_required = required.contains(&name.as_str());
        let default = property
            .get("default")
            .map(default_label)
            .unwrap_or_else(|| "—".to_string());
        let _ = writeln!(
            out,
            "| `{name}` | {} | {default} | {} | {} |",
            cell(&type_label(property)),
            if is_required { "yes" } else { "no" },
            cell(&description(property)),
        );
    }
    let _ = writeln!(out);
}

/// Render the config reference from a JSON Schema document (root schema with
/// optional `$defs`/`definitions`).
pub fn render_config(schema: &serde_json::Value) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Configuration reference\n");
    let _ = writeln!(
        out,
        "Generated from `crates/favetto/src/config.rs` by the docs generator. \
         Do not edit by hand.\n"
    );
    let _ = writeln!(
        out,
        "Every section is optional; favetto falls back to built-in defaults for anything \
         you omit. Values resolve in the order **CLI flag → config file → built-in default**, \
         and every `FAVETTO__SECTION__KEY` environment variable overrides the file.\n"
    );

    let root_title = schema
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("FavettoConfig");
    render_object(&mut out, root_title, schema);

    if let Some(defs) = defs(schema).and_then(|v| v.as_object()) {
        for (name, definition) in defs {
            render_object(&mut out, name, definition);
        }
    }
    out
}

/// Render one subcommand's arguments as a table.
fn render_args(out: &mut String, sub: &clap::Command) {
    let args: Vec<&clap::Arg> = sub
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter(|arg| {
            let id = arg.get_id().as_str();
            id != "help" && id != "version"
        })
        .collect();
    if args.is_empty() {
        return;
    }

    let _ = writeln!(out, "| Flag | Value | Default | Description |");
    let _ = writeln!(out, "|------|-------|---------|-------------|");
    for arg in args {
        let long = arg.get_long().map(|l| format!("`--{l}`"));
        let short = arg.get_short().map(|s| format!("`-{s}`"));
        let flag = match (long, short) {
            (Some(l), Some(s)) => format!("{l}, {s}"),
            (Some(l), None) => l,
            (None, Some(s)) => s,
            (None, None) => arg.get_id().to_string(),
        };
        let is_flag = matches!(
            arg.get_action(),
            clap::ArgAction::SetTrue
                | clap::ArgAction::SetFalse
                | clap::ArgAction::Count
                | clap::ArgAction::Help
                | clap::ArgAction::Version
        );
        let value = if is_flag {
            String::new()
        } else {
            arg.get_value_names()
                .map(|names| {
                    names
                        .iter()
                        .map(|n| format!("`<{n}>`"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default()
        };
        let default = arg
            .get_default_values()
            .first()
            .map(|d| format!("`{}`", d.to_string_lossy()))
            .unwrap_or_else(|| "—".to_string());
        let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
        let _ = writeln!(out, "| {flag} | {value} | {default} | {} |", cell(&help));
    }
    let _ = writeln!(out);
}

fn render_command(out: &mut String, path: &str, cmd: &clap::Command) {
    let about = cmd
        .get_long_about()
        .or_else(|| cmd.get_about())
        .map(|a| a.to_string())
        .unwrap_or_default();
    let _ = writeln!(out, "## `{path}`\n");
    if !about.is_empty() {
        let _ = writeln!(out, "{}\n", cell(&about));
    }
    render_args(out, cmd);
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let child = format!("{path} {}", sub.get_name());
        render_command(out, &child, sub);
    }
}

/// Render the CLI reference from the clap command tree.
pub fn render_cli(cmd: &clap::Command) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# CLI reference\n");
    let _ = writeln!(
        out,
        "Generated from `crates/favetto/src/cli.rs` by the docs generator. \
         Do not edit by hand.\n"
    );
    let _ = writeln!(
        out,
        "`favetto` is a single binary: `daemon` runs the scheduler, task queue, persistence \
         and remote API; `tui` attaches a client. Run `favetto <command> --help` for the same \
         information at the terminal.\n"
    );
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        render_command(
            &mut out,
            &format!("{} {}", cmd.get_name(), sub.get_name()),
            sub,
        );
    }
    out
}

/// Render the event-kind reference.
pub fn render_events() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Event kinds\n");
    let _ = writeln!(
        out,
        "Generated from `crates/favetto-core/src/model.rs` by the docs generator. \
         Do not edit by hand.\n"
    );
    let _ = writeln!(
        out,
        "Every event persisted on the bus is one of the kinds below. The **Event** column \
         is the stable `snake_case` wire and storage form; `from_name` also accepts the \
         spec's `PascalCase` spelling.\n"
    );
    let _ = writeln!(out, "| Event | Description |");
    let _ = writeln!(out, "|-------|-------------|");
    for kind in EventKind::ALL {
        let _ = writeln!(
            out,
            "| `{}` | {} |",
            kind.as_str(),
            cell(kind.description())
        );
    }
    let _ = writeln!(out);
    out
}

/// Render the remote API reference (methods, pushes, errors, HTTP endpoints).
pub fn render_remote_api() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Remote API\n");
    let _ = writeln!(
        out,
        "Generated from `crates/favetto-core/src/rpc.rs` by the docs generator. \
         Do not edit by hand.\n"
    );
    let _ = writeln!(
        out,
        "The daemon exposes one wire protocol over two transports:\n"
    );
    let _ = writeln!(
        out,
        "- **Unix socket** `/tmp/favetto.sock` (local, trusted)."
    );
    let _ = writeln!(
        out,
        "- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).\n"
    );
    let _ = writeln!(
        out,
        "Both carry the same MessagePack-encoded frames. A frame is a tagged envelope with \
         one of three shapes:\n"
    );
    let _ = writeln!(out, "```rust");
    let _ = writeln!(out, "enum Frame {{");
    let _ = writeln!(out, "    Request(Request),           // client → server");
    let _ = writeln!(
        out,
        "    Response(Response),         // server → client reply"
    );
    let _ = writeln!(
        out,
        "    Notification(Notification), // unsolicited server push"
    );
    let _ = writeln!(out, "}}");
    let _ = writeln!(out, "```\n");
    let _ = writeln!(
        out,
        "A `Request` carries `id`, `method`, and `params`; the matching `Response` carries \
         the same `id` plus either `result` or a structured `error` (`code`, `message`, \
         optional `data`).\n"
    );

    let _ = writeln!(out, "## Client → server methods\n");
    let _ = writeln!(out, "| Method | Purpose |");
    let _ = writeln!(out, "|--------|---------|");
    for (name, purpose) in rpc::CLIENT_METHODS {
        let _ = writeln!(out, "| `{name}` | {} |", cell(purpose));
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Server → client pushes\n");
    let _ = writeln!(out, "| Method | Purpose |");
    let _ = writeln!(out, "|--------|---------|");
    for (name, purpose) in rpc::SERVER_PUSHES {
        let _ = writeln!(out, "| `{name}` | {} |", cell(purpose));
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Error codes\n");
    let _ = writeln!(out, "| Code | Meaning |");
    let _ = writeln!(out, "|------|---------|");
    for (code, meaning) in [
        (error_code::PARSE, "Invalid JSON / MessagePack frame."),
        (error_code::INVALID_REQUEST, "Not a valid request object."),
        (error_code::METHOD_NOT_FOUND, "Unknown method."),
        (error_code::INVALID_PARAMS, "Invalid method parameters."),
        (error_code::INTERNAL, "Internal server error."),
        (
            error_code::UNAUTHORIZED,
            "Missing or rejected bearer token.",
        ),
    ] {
        let _ = writeln!(out, "| `{code}` | {meaning} |");
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Example\n");
    let _ = writeln!(
        out,
        "A request is a single MessagePack map; in JSON it looks like:\n"
    );
    let _ = writeln!(out, "```json");
    let _ = writeln!(
        out,
        r#"{{"type":"request","id":1,"method":"tasks.start","params":{{"name":"hello"}}}}"#
    );
    let _ = writeln!(out, "```\n");
    let _ = writeln!(out, "and the reply:\n");
    let _ = writeln!(out, "```json");
    let _ = writeln!(
        out,
        r#"{{"type":"response","id":1,"result":{{"task_id":"…","status":"idle"}}}}"#
    );
    let _ = writeln!(out, "```\n");
    let _ = writeln!(
        out,
        "Subscribing with `events.subscribe` and `last_event_id` replays persisted events \
         before switching to live delivery, so a TUI that reconnects never misses a task \
         result.\n"
    );

    let _ = writeln!(out, "## HTTP endpoints\n");
    let _ = writeln!(
        out,
        "The daemon also serves HTTP endpoints alongside the RPC transports:\n"
    );
    let _ = writeln!(out, "- `GET /metrics` — Prometheus metrics.");
    let _ = writeln!(
        out,
        "- `POST /pair/generate` and `POST /pair/exchange` — pairing."
    );
    let _ = writeln!(
        out,
        "- `POST /webhooks/github` — GitHub webhook receiver (see [Webhooks & hooks](../guide/webhooks))."
    );
    let _ = writeln!(out);
    out
}

/// Write every generated document under `docs/`, printing each path.
///
/// The `wrote <path>` line is intentional CLI progress on stdout — the doc
/// generator is meant to be run by hand and its output is scriptable — so it is
/// deliberately a `println!` rather than a `tracing` event. Route new
/// diagnostics through `tracing` instead.
pub fn run(args: &DocArgs) -> anyhow::Result<()> {
    let docs = docs_dir(args.docs_dir.as_deref());
    let reference = docs.join("reference");
    let public = docs.join("public");
    fs::create_dir_all(&reference)?;
    fs::create_dir_all(&public)?;

    let schema = serde_json::to_value(schemars::schema_for!(FavettoConfig))?;
    let schema_json = format!("{}\n", serde_json::to_string_pretty(&schema)?);
    let files: Vec<(PathBuf, String)> = vec![
        (reference.join("config.md"), render_config(&schema)),
        (reference.join("cli.md"), render_cli(&Cli::command())),
        (reference.join("events.md"), render_events()),
        (reference.join("remote-api.md"), render_remote_api()),
        (public.join("favetto-schema.json"), schema_json),
    ];

    for (path, contents) in files {
        fs::write(&path, contents)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handbuilt_schema() -> serde_json::Value {
        serde_json::json!({
            "title": "FavettoConfig",
            "description": "Global configuration for favetto.",
            "type": "object",
            "required": ["agent"],
            "properties": {
                "agent": {
                    "description": "Default external agent.",
                    "$ref": "#/$defs/AgentSettings"
                },
                "enabled": {
                    "description": "Master switch.",
                    "type": "boolean",
                    "default": true
                },
                "mode": {
                    "description": "Signing mode.",
                    "type": "string",
                    "enum": ["off", "ssh", "gpg"]
                },
                "worktree_dir": {
                    "description": "Where worktrees live.",
                    "anyOf": [
                        { "$ref": "#/$defs/AgentSettings" },
                        { "type": "null" }
                    ]
                }
            },
            "$defs": {
                "AgentSettings": {
                    "description": "Agent selection.",
                    "type": "object",
                    "properties": {
                        "default": {
                            "description": "The default agent name.",
                            "type": ["string", "null"]
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn render_config_includes_fields_refs_defaults_and_enums() {
        let rendered = render_config(&handbuilt_schema());
        assert!(rendered.contains("## FavettoConfig"));
        assert!(rendered.contains("| `agent` |"));
        assert!(rendered.contains("Global configuration for favetto."));
        assert!(rendered.contains("AgentSettings"));
        assert!(rendered.contains("`enabled`"));
        assert!(rendered.contains("`true`"));
        assert!(
            rendered.contains("`off`") && rendered.contains("`ssh`") && rendered.contains("`gpg`")
        );
        assert!(rendered.contains("(optional)"));
        assert!(rendered.contains("## AgentSettings"));
    }

    #[test]
    fn render_cli_covers_public_commands_and_hides_internals() {
        let rendered = render_cli(&Cli::command());
        for expected in [
            "favetto daemon",
            "favetto tui",
            "favetto pair",
            "favetto token-rotate",
            "--listen",
            "--token-file",
            "--tasks-dir",
        ] {
            assert!(
                rendered.contains(expected),
                "CLI reference is missing {expected}"
            );
        }
        assert!(
            !rendered.contains("__doc"),
            "hidden __doc leaked into the reference"
        );
        assert!(
            !rendered.contains("__agent-exec"),
            "hidden __agent-exec leaked into the reference"
        );
    }

    #[test]
    fn render_events_covers_every_kind() {
        let rendered = render_events();
        for kind in EventKind::ALL {
            assert!(
                rendered.contains(kind.as_str()),
                "event reference is missing {}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn render_remote_api_covers_every_method_and_push() {
        let rendered = render_remote_api();
        for (name, _) in rpc::CLIENT_METHODS {
            assert!(rendered.contains(name), "remote API is missing {name}");
        }
        for (name, _) in rpc::SERVER_PUSHES {
            assert!(rendered.contains(name), "remote API is missing {name}");
        }
    }

    #[test]
    fn docs_dir_defaults_to_the_repository_docs() {
        let dir = docs_dir(None);
        assert!(
            dir.ends_with("docs"),
            "unexpected docs dir: {}",
            dir.display()
        );
        let explicit = docs_dir(Some(Path::new("/tmp/elsewhere")));
        assert_eq!(explicit, PathBuf::from("/tmp/elsewhere"));
    }
}
