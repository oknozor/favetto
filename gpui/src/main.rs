//! Entry point for the favetto GPUI desktop client.
//!
//! Step 01 delivered the window; step 02 the daemon transport; step 03 the
//! navigation frame — sidebar, top bar, status bar, routes, command palette and
//! the action-based keyboard model (docs/design/gpui-client.md §1, §4).

mod actions;
mod agent;
mod agent_auth;
mod app;
mod catalog;
mod components;
mod config;
mod conversation;
mod daemon_config;
mod detail;
mod elicitation;
mod inbox;
mod launch;
mod layout;
mod markdown;
mod motion;
mod palette;
mod repos;
mod resize;
mod routes;
mod runtime;
mod schedules;
mod search;
mod session_menu;
mod settings;
mod sse;
mod state;
mod tasks;
mod text_element;
mod text_input;
mod theme;
mod transport;
mod views;
mod workflow;

use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};

use app::RootView;

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys(actions::bindings());

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(800.)),
                    cx,
                ))),
                window_min_size: Some(size(px(900.), px(600.))),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("favetto");
                cx.new(|cx| RootView::new(window, cx))
            },
        )
        .expect("opening the main window must succeed");
        cx.activate(true);
    });
}
