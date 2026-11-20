//! Root view: the three-region shell from docs/design/gpui-client.md §4
//! (navigation / working list / detail) with the six navigation entries
//! (Inbox → Tasks → Catalog → Workflows → Schedules → Settings).
//!
//! Step 01 delivers the skeleton: each region is a bounded surface holding
//! a placeholder; later steps fill in the real routes (§1 surfaces table)
//! and the daemon wiring (step 02: `src/transport.rs`).

use gpui::{div, prelude::*, px, Context, Div, IntoElement, Stateful, Window};

use crate::theme::{Theme, SIDEBAR_WIDTH, STATUS_BAR_HEIGHT};

/// Routes of the root shell, mirroring the daemon vocabulary (docs §1; "Jakob's
/// Law" in §2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    /// What needs me now — the Inbox surface.
    #[default]
    Inbox,
    /// All runs, filter and search — the Tasks surface.
    Tasks,
    /// Find and launch a definition — the Catalog surface.
    Catalog,
    /// One root's DAG — the Workflows surface.
    Workflows,
    /// Recurring triggers — the Schedules surface.
    Schedules,
    /// Connection, agents, theme — the Settings surface.
    Settings,
}

impl Route {
    /// Navigation order (docs §1, Serial Position Effect): the two surfaces
    /// users need most come first, configuration is last.
    pub const ALL: [Route; 6] = [
        Route::Inbox,
        Route::Tasks,
        Route::Catalog,
        Route::Workflows,
        Route::Schedules,
        Route::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Route::Inbox => "Inbox",
            Route::Tasks => "Tasks",
            Route::Catalog => "Catalog",
            Route::Workflows => "Workflows",
            Route::Schedules => "Schedules",
            Route::Settings => "Settings",
        }
    }
}

/// Root view: the three bounded regions, separated by 1px borders (Law of
/// Common Region, docs §2).
#[derive(Default)]
pub struct RootView {
    route: Route,
}

impl RootView {
    /// One sidebar entry; clicking it switches the active route.
    fn nav_entry(&self, route: Route, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = route == self.route;
        div()
            .id(route.label())
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .w_full()
            .p(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .when(active, |s| s.bg(theme.colors.accent))
            .text_color(if active {
                theme.colors.text
            } else {
                theme.colors.muted
            })
            .text_size(px(theme.font.sm))
            .cursor_pointer()
            .child(route.label())
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.route = route;
                cx.notify();
            }))
    }

    /// Placeholder for the working-list region (docs §4: routes are filled
    /// in step by step).
    fn content_placeholder(&self, theme: &Theme) -> Div {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .gap(px(theme.spacing.s3))
            .rounded(px(theme.radii.r8))
            .border_1()
            .border_color(theme.colors.border)
            .text_color(theme.colors.muted)
            .child(
                div()
                    .text_size(px(theme.font.xl))
                    .text_color(theme.colors.text)
                    .child(self.route.label()),
            )
            .child(
                div()
                    .mt(px(theme.spacing.s1))
                    .text_size(px(theme.font.lg))
                    .child("Skeleton shell for this route (step 01)."),
            )
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());

        let sidebar = div()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .p(px(theme.spacing.s4))
            .gap(px(theme.spacing.s6))
            .border_r_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .children(
                Route::ALL
                    .iter()
                    .map(|route| self.nav_entry(*route, theme, cx)),
            );

        let content = self.content_placeholder(theme);

        let status_bar = div()
            .flex()
            .items_center()
            .h(px(STATUS_BAR_HEIGHT))
            .p(px(theme.spacing.s4))
            .rounded(px(theme.radii.r12))
            .border_t_1()
            .border_color(theme.colors.border)
            .text_color(theme.colors.muted)
            .text_size(px(theme.font.xs))
            .child("favetto — skeleton status bar (step 01)");

        div()
            .flex()
            .flex_col()
            .h_full()
            .bg(theme.colors.bg)
            .text_color(theme.colors.text)
            .text_size(px(theme.font.base))
            .child(sidebar)
            .child(content)
            .child(status_bar)
    }
}
