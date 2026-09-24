//! Guard tests for the library target's public surface.
//!
//! `favetto` is built as both a binary and a library. These references only
//! compile when the modules the binary and future consumers rely on stay public
//! in the library crate — the whole point of the library target.
//!
//! The TUI client was extracted into the `favetto-tui` crate (issue #107), so
//! the wire client and its `Client`/`Transport` types are no longer part of this
//! surface; `favetto::tui` is now only the dispatcher.

use favetto::cli::{Cli, Command, DaemonArgs, DocArgs, PairArgs, TokenRotateArgs, TuiArgs};
use favetto::config::FavettoConfig;
use favetto::paths::expand_tilde;
use favetto::tasks::TaskDef;
use favetto::template::render;
use favetto::workflow::WorkflowGraph;

#[test]
fn library_target_exposes_the_reusable_surface() {
    // Subcommand entry points.
    let entry_points = [
        std::any::type_name_of_val(&favetto::daemon::run),
        std::any::type_name_of_val(&favetto::tui::run),
        std::any::type_name_of_val(&favetto::pair::run),
        std::any::type_name_of_val(&favetto::docgen::run),
    ];
    assert!(entry_points.iter().all(|name| !name.is_empty()));

    // Modules shared between the daemon and the TUI client (re-exported from
    // `favetto-core`).
    let shared = [
        std::any::type_name::<Cli>(),
        std::any::type_name::<Command>(),
        std::any::type_name::<DaemonArgs>(),
        std::any::type_name::<DocArgs>(),
        std::any::type_name::<PairArgs>(),
        std::any::type_name::<TokenRotateArgs>(),
        std::any::type_name::<TuiArgs>(),
        std::any::type_name::<FavettoConfig>(),
        std::any::type_name::<TaskDef>(),
        std::any::type_name::<WorkflowGraph>(),
        std::any::type_name_of_val(&favetto::tasks::parse_task_md),
        std::any::type_name_of_val(&favetto::workflow::build_graph),
        std::any::type_name_of_val(&favetto::ws::outbound),
    ];
    assert!(shared.iter().all(|name| !name.is_empty()));

    // Spot-check a couple of pure helpers so the imports are exercised.
    assert_eq!(render("{{ x }}", &serde_json::json!({ "x": 1 })), "1");
    assert!(expand_tilde("~").is_absolute());
}
