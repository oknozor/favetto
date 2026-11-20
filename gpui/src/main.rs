//! Entry point for the favetto GPUI desktop client.
//!
//! Step 01 of the chain delivers the shell: a window titled "favetto"
//! (default 1280×800, minimum 900×600) with the three-region layout of
//! docs/design/gpui-client.md §4. The daemon transport lands in step 02.

mod app;
mod theme;

use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};

use app::RootView;

fn main() {
    Application::new().run(|cx: &mut App| {
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
                cx.new(|_| RootView::default())
            },
        )
        .expect("opening the main window must succeed");
        cx.activate(true);
    });
}
