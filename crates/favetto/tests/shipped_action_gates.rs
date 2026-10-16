//! Guard test for the two shipped `new_issues` poll gates.
//!
//! Both gates must stay `kind = "action"` tasks that run `http.request` and gate
//! their `spawn` on a non-empty `items` output. If either silently regressed to a
//! plain agent task (`kind` dropped, `[action]` removed, or the `[action.when]`
//! predicate lost) the daemon would launch an agent — and spend a worktree and a
//! model call — on every scheduled poll, even an empty one, which is exactly the
//! bug fixed in issue #310.

use std::fs;
use std::path::{Path, PathBuf};

use favetto::tasks::{parse_task_md, TaskKind, WhenOp};

/// The repository-local poll gate and the example copy shipped under `tasks/`.
const GATES: [&str; 2] = [
    ".favetto/tasks/new_issues.md",
    "tasks/examples/gates/new-issues.md",
];

/// Repository root: `<manifest dir>/crates/favetto` → up two levels.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/favetto always has a repository root two levels up")
        .to_path_buf()
}

#[test]
fn shipped_new_issues_gates_are_http_request_action_tasks() {
    for relative in GATES {
        let path = repo_root().join(relative);
        let md = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        let def = parse_task_md(relative, &md)
            .unwrap_or_else(|err| panic!("{} failed to parse: {err}", path.display()));

        assert_eq!(
            def.kind,
            TaskKind::Action,
            "{relative} must be a kind = \"action\" task"
        );
        assert_eq!(def.agent, None, "{relative} must not name an agent");

        let action = def
            .action
            .as_ref()
            .unwrap_or_else(|| panic!("{relative} has no [action] table"));
        assert_eq!(
            action.id, "http.request",
            "{relative} must run the http.request action"
        );

        let when = action
            .when
            .as_ref()
            .unwrap_or_else(|| panic!("{relative} must gate spawn with [action.when]"));
        assert_eq!(
            when.path, "items",
            "{relative} must test the `items` output"
        );
        assert_eq!(
            when.op,
            WhenOp::NotEmpty,
            "{relative} must gate on not_empty"
        );
        assert!(
            when.value.is_none(),
            "{relative}: not_empty takes no `value`"
        );
        assert_ne!(
            action.params.get("fail_on_error"),
            Some(&toml::Value::Boolean(false)),
            "{relative} must not opt out of failing on a non-2xx response"
        );
    }
}
