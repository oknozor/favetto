//! Static Markdown prose and table scaffolding for `favetto __doc`.
//!
//! The renderers in [`crate::docgen`] walk clap, schemars and `EventKind` and
//! compose the results with the fragments below. Keeping the prose here as data
//! lets it be reviewed without reading the generation logic, and keeps the
//! renderers focused on reflection.
//!
//! Fragments include the newlines that separate them, so a renderer appends one
//! with [`String::push_str`] and only uses a formatting macro for interpolated
//! values.

/// Opening of `config.md`, through the trailing blank line.
pub(crate) const CONFIG_INTRO: &str = "\
# Configuration reference

Generated from `crates/favetto-core/src/config.rs` by the docs generator. Do not edit by hand.

Every section is optional; favetto falls back to built-in defaults for anything you omit. Values resolve in the order **CLI flag → config file → built-in default**, and every `FAVETTO__SECTION__KEY` environment variable overrides the file.

";

/// Header and separator of the config field table.
pub(crate) const FIELD_TABLE: &str = "| Field | Type | Default | Required | Description |\n\
     |-------|------|---------|----------|-------------|\n";

/// Opening of `cli.md`, through the trailing blank line.
pub(crate) const CLI_INTRO: &str = "\
# CLI reference

Generated from `crates/favetto/src/cli.rs` by the docs generator. Do not edit by hand.

`favetto` is the daemon and doc generator; its `tui` subcommand execs the sibling `favetto-tui` client binary. Run `favetto <command> --help` for the same information at the terminal.

";

/// Header and separator of a subcommand argument table.
pub(crate) const ARGS_TABLE: &str = "| Flag | Value | Default | Description |\n\
     |------|-------|---------|-------------|\n";

/// Opening of `events.md`, through the trailing blank line.
pub(crate) const EVENTS_INTRO: &str = "\
# Event kinds

Generated from `crates/favetto-core/src/model.rs` by the docs generator. Do not edit by hand.

Every event persisted on the bus is one of the kinds below. The **Event** column is the stable `snake_case` wire and storage form; `from_name` also accepts the spec's `PascalCase` spelling.

";

/// Header and separator of the event-kind table.
pub(crate) const EVENTS_TABLE: &str = "| Event | Description |\n\
     |-------|-------------|\n";

/// Opening of `remote-api.md`, through the wire-frame explanation.
pub(crate) const REMOTE_API_INTRO: &str = "\
# Remote API

Generated from `crates/favetto-core/src/rpc.rs` by the docs generator. Do not edit by hand.

The daemon exposes one wire protocol over two transports:

- **Unix socket** `/tmp/favetto.sock` (local, trusted).
- **WebSocket** `ws://127.0.0.1:7878/rpc` (bearer token required).

Both carry the same MessagePack-encoded frames. A frame is a tagged envelope with one of three shapes:

```rust
enum Frame {
    Request(Request),           // client → server
    Response(Response),         // server → client reply
    Notification(Notification), // unsolicited server push
}
```

A `Request` carries `id`, `method`, and `params`; the matching `Response` carries the same `id` plus either `result` or a structured `error` (`code`, `message`, optional `data`).

";

/// Heading of the client → server methods section.
pub(crate) const CLIENT_METHODS_HEADING: &str = "## Client → server methods\n\n";

/// Header and separator of a method table.
pub(crate) const METHOD_TABLE: &str = "| Method | Purpose |\n\
     |--------|---------|\n";

/// Heading of the server → client pushes section.
pub(crate) const SERVER_PUSHES_HEADING: &str = "## Server → client pushes\n\n";

/// Heading of the error-codes section.
pub(crate) const ERROR_CODES_HEADING: &str = "## Error codes\n\n";

/// Header and separator of the error-code table.
pub(crate) const ERROR_TABLE: &str = "| Code | Meaning |\n\
     |------|---------|\n";

/// Heading of the example section.
pub(crate) const EXAMPLE_HEADING: &str = "## Example\n\n";

/// Intro of the example section, through the trailing blank line.
pub(crate) const EXAMPLE_INTRO: &str =
    "A request is a single MessagePack map; in JSON it looks like:\n\n";

/// Opening fence of a `json` code block.
pub(crate) const JSON_FENCE_OPEN: &str = "```json\n";

/// Closing fence of a code block, followed by a blank line.
pub(crate) const CODE_FENCE_CLOSE: &str = "```\n\n";

/// Example request frame.
pub(crate) const EXAMPLE_REQUEST: &str =
    r#"{"type":"request","id":1,"method":"tasks.start","params":{"name":"hello"}}"#;

/// Intro of the example reply, through the trailing blank line.
pub(crate) const EXAMPLE_REPLY_INTRO: &str = "and the reply:\n\n";

/// Example response frame.
pub(crate) const EXAMPLE_REPLY: &str =
    r#"{"type":"response","id":1,"result":{"task_id":"…","status":"idle"}}"#;

/// Closing note of the example section, through the trailing blank line.
pub(crate) const EXAMPLE_OUTRO: &str = "\
Subscribing with `events.subscribe` and `last_event_id` replays persisted events before switching to live delivery, so a TUI that reconnects never misses a task result.

";

/// Heading of the HTTP-endpoints section.
pub(crate) const HTTP_ENDPOINTS_HEADING: &str = "## HTTP endpoints\n\n";

/// Intro of the HTTP-endpoints section, through the trailing blank line.
pub(crate) const HTTP_ENDPOINTS_INTRO: &str =
    "The daemon also serves HTTP endpoints alongside the RPC transports:\n\n";

/// `GET /metrics` HTTP endpoint bullet.
pub(crate) const HTTP_ENDPOINT_METRICS: &str = "- `GET /metrics` — Prometheus metrics.\n";

/// Pairing HTTP endpoint bullet.
pub(crate) const HTTP_ENDPOINT_PAIR: &str =
    "- `POST /pair/generate` and `POST /pair/exchange` — pairing.\n";

/// GitHub webhook HTTP endpoint bullet.
pub(crate) const HTTP_ENDPOINT_WEBHOOKS: &str =
    "- `POST /webhooks/github` — GitHub webhook receiver (see [Webhooks & hooks](../guide/webhooks)).\n";
