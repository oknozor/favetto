//! Root view: the navigation frame from docs/design/gpui-client.md §1.
//!
//! The shell has three bounded regions (sidebar, working list, detail), a top
//! bar with the repo and connection pill, a status bar, the command palette
//! and the shortcut sheet. Routes are entities; the active route lives in
//! [`ClientState`] so the sidebar, palette and views never disagree.
//!
//! Responsive behaviour: wide windows render three columns; below
//! [`crate::theme::NARROW_BREAKPOINT`] the detail region is a pushed page. The
//! GPUI version pinned for this workspace has no container-query primitive, so
//! [`crate::layout::container_query`] reads the equivalent full-window width.

use gpui::{
    div, prelude::*, px, AnyElement, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render,
    Stateful, Window,
};

use crate::actions::{
    self, CloseOverlay, GoCatalog, GoInbox, GoSchedules, GoSettings, GoTasks, GoWorkflows,
    PaletteConfirm, PaletteDown, PaletteUp, TogglePalette, ToggleShortcuts,
};
use crate::components;
use crate::layout::{container_query, LayoutMode};
use crate::palette::{CommandKind, CommandPalette};
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::{Theme, DETAIL_WIDTH, SIDEBAR_WIDTH, STATUS_BAR_HEIGHT, TOP_BAR_HEIGHT};
use crate::views::{
    CatalogView, DetailView, InboxView, SchedulesView, SettingsView, TasksView, WorkflowsView,
};

/// The route entities, created once with the shell.
struct RouteViews {
    inbox: Entity<InboxView>,
    tasks: Entity<TasksView>,
    catalog: Entity<CatalogView>,
    workflows: Entity<WorkflowsView>,
    schedules: Entity<SchedulesView>,
    settings: Entity<SettingsView>,
}

/// The root shell view.
pub struct RootView {
    state: Entity<ClientState>,
    views: RouteViews,
    detail: Entity<DetailView>,
    palette: CommandPalette,
    shortcuts_open: bool,
    root_focus: FocusHandle,
    nav_focus: Vec<FocusHandle>,
    palette_focus: FocusHandle,
    palette_button_focus: FocusHandle,
    settings_button_focus: FocusHandle,
    shortcuts_button_focus: FocusHandle,
    reconnect_button_focus: FocusHandle,
    shortcuts_close_focus: FocusHandle,
}

impl RootView {
    /// Build the shell, create the route entities, and focus the root anchor so
    /// `⌘K` and `g`-jumps work from launch.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(ClientState::new);
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();

        let views = RouteViews {
            inbox: cx.new(|cx| InboxView::new(state.clone(), cx)),
            tasks: cx.new(|cx| TasksView::new(state.clone(), cx)),
            catalog: cx.new(|cx| CatalogView::new(state.clone(), cx)),
            workflows: cx.new(|cx| WorkflowsView::new(state.clone(), cx)),
            schedules: cx.new(|cx| SchedulesView::new(state.clone(), cx)),
            settings: cx.new(|cx| SettingsView::new(state.clone(), cx)),
        };
        let detail = cx.new(|cx| DetailView::new(state.clone(), cx));

        let root_focus = cx.focus_handle();
        let palette_focus = cx.focus_handle().tab_index(100).tab_stop(true);
        let palette_button_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let settings_button_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        let nav_focus = Route::ALL
            .iter()
            .map(|route| {
                cx.focus_handle()
                    .tab_index(10 + route.index() as isize)
                    .tab_stop(true)
            })
            .collect();
        let shortcuts_button_focus = cx.focus_handle().tab_index(30).tab_stop(true);
        let reconnect_button_focus = cx.focus_handle().tab_index(31).tab_stop(true);
        let shortcuts_close_focus = cx.focus_handle().tab_index(100).tab_stop(true);

        window.focus(&root_focus);

        Self {
            state,
            views,
            detail,
            palette: CommandPalette::default(),
            shortcuts_open: false,
            root_focus,
            nav_focus,
            palette_focus,
            palette_button_focus,
            settings_button_focus,
            shortcuts_button_focus,
            reconnect_button_focus,
            shortcuts_close_focus,
        }
    }

    fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.navigate(route);
            cx.notify();
        });
        self.palette.close();
        self.shortcuts_open = false;
        cx.notify();
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shortcuts_open = false;
        self.palette.open();
        window.focus(&self.palette_focus);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.close();
        window.focus(&self.root_focus);
        cx.notify();
    }

    fn toggle_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shortcuts_open = !self.shortcuts_open;
        if self.shortcuts_open {
            self.palette.close();
            window.focus(&self.shortcuts_close_focus);
        } else {
            window.focus(&self.root_focus);
        }
        cx.notify();
    }

    /// Escape closes the topmost overlay; with no overlay it acts as Back out
    /// of a pushed detail page (docs §1, responsive layout).
    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_open() {
            self.close_palette(window, cx);
        } else if self.shortcuts_open {
            self.shortcuts_open = false;
            window.focus(&self.root_focus);
            cx.notify();
        } else {
            self.state.update(cx, |state, cx| {
                state.close_detail();
                cx.notify();
            });
            window.focus(&self.root_focus);
        }
    }

    fn on_jump(&mut self, action: &dyn gpui::Action, cx: &mut Context<Self>) {
        if let Some(route) = actions::route_for_action(action) {
            self.navigate(route, cx);
        }
    }

    fn move_palette(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.palette.move_selection(delta);
        cx.notify();
    }

    fn select_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(kind) = self.palette.activate() {
            self.execute(kind, window, cx);
        }
    }

    fn execute(&mut self, kind: CommandKind, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.close();
        self.shortcuts_open = false;
        window.focus(&self.root_focus);
        match kind {
            CommandKind::Navigate(route) => {
                self.state.update(cx, |state, cx| {
                    state.navigate(route);
                    cx.notify();
                });
            }
            CommandKind::Reconnect => {
                self.state.update(cx, |state, cx| {
                    state.reconnect();
                    cx.notify();
                });
            }
            CommandKind::Refresh => {
                self.state.update(cx, |state, cx| {
                    state.ping();
                    cx.notify();
                });
            }
        }
        cx.notify();
    }

    fn activate_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(kind) = self.palette.activate_id(id) {
            self.execute(kind, window, cx);
        }
    }

    /// The active route entity, as one element for the working-list region.
    fn active_content(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.state.read(cx).active_route() {
            Route::Inbox => self.views.inbox.clone().into_any_element(),
            Route::Tasks => self.views.tasks.clone().into_any_element(),
            Route::Catalog => self.views.catalog.clone().into_any_element(),
            Route::Workflows => self.views.workflows.clone().into_any_element(),
            Route::Schedules => self.views.schedules.clone().into_any_element(),
            Route::Settings => self.views.settings.clone().into_any_element(),
        }
    }

    fn top_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (repo, connection) = {
            let state = self.state.read(cx);
            (state.repo_label().to_string(), state.connection().clone())
        };

        let palette_button = components::with_focus(
            div()
                .id("open-palette")
                .h(px(32.0))
                .px(px(theme.spacing.s2))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.surface)
                .text_color(theme.colors.muted)
                .text_size(px(theme.font.xs))
                .cursor_pointer()
                .track_focus(&self.palette_button_focus)
                .child(components::keycap(theme, "⌘K"))
                .child("Commands")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.open_palette(window, cx);
                })),
            theme,
        );

        let settings_button = components::with_focus(
            div()
                .id("open-settings")
                .h(px(32.0))
                .px(px(theme.spacing.s2))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.surface)
                .text_color(theme.colors.text)
                .text_size(px(theme.font.xs))
                .cursor_pointer()
                .track_focus(&self.settings_button_focus)
                .child(Route::Settings.icon())
                .child("Settings")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.navigate(Route::Settings, cx);
                })),
            theme,
        );

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s3))
            .h(px(TOP_BAR_HEIGHT))
            .px(px(theme.spacing.s4))
            .border_b_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.bg)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s3))
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("favetto"),
                    )
                    .child(div().w(px(1.0)).h(px(20.0)).bg(theme.colors.border))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s1))
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child("repo:")
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.colors.text)
                                    .child(repo),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::connection_pill(theme, &connection))
                    .child(palette_button)
                    .child(settings_button),
            )
    }

    fn sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let active = self.state.read(cx).active_route();
        div()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .p(px(theme.spacing.s4))
            .gap(px(theme.spacing.s2))
            .border_r_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .child(
                div()
                    .px(px(theme.spacing.s2))
                    .pb(px(theme.spacing.s1))
                    .child(components::section_label(theme, "NAVIGATION")),
            )
            .children(
                Route::ALL
                    .iter()
                    .map(|route| self.nav_entry(*route, theme, cx)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .px(px(theme.spacing.s2))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(active.subtitle()),
            )
            .child(
                div()
                    .px(px(theme.spacing.s2))
                    .py(px(theme.spacing.s1))
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("⌘K  Commands"),
            )
    }

    fn nav_entry(&self, route: Route, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let active = self.state.read(cx).active_route() == route;
        let awaiting = self.state.read(cx).awaiting_count();
        let badge = (route == Route::Inbox && awaiting > 0).then(|| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .min_w(px(20.0))
                .h(px(18.0))
                .px(px(theme.spacing.s1))
                .rounded(px(theme.radii.r12))
                .border_1()
                .border_color(theme.colors.warning)
                .text_color(theme.colors.warning)
                .text_size(px(theme.font.xs))
                .child(awaiting.to_string())
        });

        components::with_focus(
            div()
                .id(("nav", route.index()))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .w_full()
                .h(px(36.0))
                .px(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .cursor_pointer()
                .text_size(px(theme.font.sm))
                .text_color(if active {
                    theme.colors.text
                } else {
                    theme.colors.muted
                })
                .when(active, |entry| {
                    entry.bg(Hsla::from(theme.colors.accent).opacity(0.16))
                })
                .when(!active, |entry| {
                    entry.hover(|style| style.bg(theme.colors.bg))
                })
                .track_focus(&self.nav_focus[route.index()])
                .child(
                    div()
                        .w(px(20.0))
                        .text_center()
                        .text_color(if active {
                            theme.colors.accent
                        } else {
                            theme.colors.muted
                        })
                        .child(route.icon()),
                )
                .child(div().flex_1().child(route.label()))
                .when_some(route.jump_shortcut(), |entry, hint| {
                    entry.child(
                        div()
                            .font_family("monospace")
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child(hint),
                    )
                })
                .when_some(badge, |entry, badge| entry.child(badge))
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.navigate(route, cx);
                })),
            theme,
        )
    }

    fn status_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (endpoint, ping, events, cursor, error, connection) = {
            let state = self.state.read(cx);
            (
                state.config().endpoint.clone(),
                state.ping_label(),
                state.push_count(),
                state.cursor(),
                state.last_error().map(str::to_string),
                state.connection().clone(),
            )
        };

        let shortcuts = components::with_focus(
            div()
                .id("open-shortcuts")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .px(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .text_color(theme.colors.muted)
                .cursor_pointer()
                .track_focus(&self.shortcuts_button_focus)
                .child(components::keycap(theme, "?"))
                .child("Shortcuts")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.toggle_shortcuts(window, cx);
                })),
            theme,
        );

        let reconnect_state = self.state.clone();
        let reconnect = components::with_focus(
            div()
                .id("status-reconnect")
                .px(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.reconnect_button_focus)
                .child("Reconnect")
                .on_click(cx.listener(move |_this, _event, _window, cx| {
                    reconnect_state.update(cx, |state, cx| {
                        state.reconnect();
                        cx.notify();
                    });
                })),
            theme,
        );

        let cursor_label = match cursor {
            Some(cursor) => format!("{events} events · cursor {cursor}"),
            None => format!("{events} events"),
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s3))
            .h(px(STATUS_BAR_HEIGHT))
            .px(px(theme.spacing.s4))
            .border_t_1()
            .border_color(theme.colors.border)
            .text_color(theme.colors.muted)
            .text_size(px(theme.font.xs))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .min_w_0()
                    .child(div().font_family("monospace").truncate().child(endpoint))
                    .child("·")
                    .child(div().truncate().child(ping))
                    .when_some(error, |left, error| {
                        left.child(div().text_color(theme.colors.danger).child(error))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .when(!connection.is_connected(), |right| {
                        let color = match connection {
                            crate::state::ConnectionState::Unauthorized { .. }
                            | crate::state::ConnectionState::Disconnected { .. } => {
                                theme.colors.danger
                            }
                            _ => theme.colors.warning,
                        };
                        right.child(
                            div()
                                .max_w(px(320.0))
                                .truncate()
                                .text_color(color)
                                .child(connection.label()),
                        )
                    })
                    .child(cursor_label)
                    .child(shortcuts)
                    .child(reconnect),
            )
    }

    fn palette_overlay(&self, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let focused = self.palette_focus.is_focused(window);
        let query = self.palette.query().to_string();
        let selected = self.palette.selected();
        let results = self.palette.filtered();

        let input = div()
            .id("palette-input")
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .h(px(48.0))
            .px(px(theme.spacing.s3))
            .border_2()
            .border_color(if focused {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .track_focus(&self.palette_focus)
            .cursor_text()
            .on_click(cx.listener(|this, _event, window, _cx| {
                window.focus(&this.palette_focus);
            }))
            .child(
                div()
                    .text_color(theme.colors.accent)
                    .text_size(px(theme.font.base))
                    .child("⌕"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(theme.font.base))
                            .text_color(if query.is_empty() {
                                theme.colors.muted
                            } else {
                                theme.colors.text
                            })
                            .child(if query.is_empty() {
                                "Type a command…".to_string()
                            } else {
                                query.clone()
                            }),
                    )
                    .when(focused, |text| {
                        text.child(div().text_color(theme.colors.accent).child("▌"))
                    }),
            )
            .child(components::keycap(theme, "esc"));

        let rows: Vec<AnyElement> = if results.is_empty() {
            vec![div()
                .flex()
                .flex_col()
                .gap(px(theme.spacing.s1))
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s4))
                .child(
                    div()
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.text)
                        .child("No matching commands"),
                )
                .child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child("Try inbox, settings, reconnect or refresh."),
                )
                .into_any_element()]
        } else {
            results
                .iter()
                .enumerate()
                .map(|(index, command)| {
                    let is_selected = index == selected;
                    let is_recent = self.palette.is_recent(command.id);
                    let id = command.id;
                    div()
                        .id(("palette-row", index))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(theme.spacing.s3))
                        .h(px(38.0))
                        .px(px(theme.spacing.s2))
                        .rounded(px(theme.radii.r4))
                        .border_2()
                        .border_color(if is_selected {
                            theme.colors.accent
                        } else {
                            theme.colors.surface
                        })
                        .when(is_selected, |row| {
                            row.bg(Hsla::from(theme.colors.accent).opacity(0.12))
                        })
                        .when(!is_selected, |row| {
                            row.hover(|style| style.bg(theme.colors.bg))
                        })
                        .cursor_pointer()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(theme.spacing.s2))
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(px(theme.font.sm))
                                        .text_color(theme.colors.text)
                                        .child(command.label),
                                )
                                .when(is_recent, |label| {
                                    label.child(
                                        div()
                                            .text_size(px(theme.font.xs))
                                            .text_color(theme.colors.muted)
                                            .child("recent"),
                                    )
                                }),
                        )
                        .child(
                            command
                                .shortcut
                                .map(|keys| components::keycap(theme, keys))
                                .unwrap_or_else(|| div()),
                        )
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            this.activate_command(id, window, cx);
                        }))
                        .into_any_element()
                })
                .collect()
        };

        let panel = div()
            .w(px(560.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(theme.radii.r12))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .shadow_lg()
            .child(input)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .p(px(theme.spacing.s2))
                    .children(rows),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s3))
                    .px(px(theme.spacing.s3))
                    .py(px(theme.spacing.s2))
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child("↑↓")
                    .child("move")
                    .child("↵")
                    .child("open")
                    .child("esc")
                    .child("close"),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(96.0))
            .bg(Hsla::from(theme.colors.bg).opacity(0.86))
            .key_context("Palette")
            .child(panel)
    }

    fn shortcuts_overlay(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let close = components::with_focus(
            div()
                .id("shortcuts-close")
                .px(px(theme.spacing.s2))
                .py(px(2.0))
                .rounded(px(theme.radii.r4))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.shortcuts_close_focus)
                .child("Close")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.shortcuts_open = false;
                    window.focus(&this.root_focus);
                    cx.notify();
                })),
            theme,
        );

        let navigation: [(&'static str, &'static str); 6] =
            Route::ALL.map(|route| (route.label(), route.shortcut()));
        let anywhere = [
            ("Shortcut sheet", "?"),
            ("Close overlay / back", "Esc"),
            ("Move palette selection", "↑ ↓"),
            ("Run palette command", "↵"),
            ("Move focus", "Tab"),
        ];

        let panel = div()
            .w(px(520.0))
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s3))
            .p(px(theme.spacing.s4))
            .rounded(px(theme.radii.r12))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("Keyboard shortcuts"),
                    )
                    .child(close),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(components::section_label(theme, "NAVIGATION"))
                    .children(
                        navigation
                            .iter()
                            .map(|(label, keys)| components::shortcut_row(theme, label, keys)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(components::section_label(theme, "ANYWHERE"))
                    .children(
                        anywhere
                            .iter()
                            .map(|(label, keys)| components::shortcut_row(theme, label, keys)),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(Hsla::from(theme.colors.bg).opacity(0.86))
            .key_context("Shortcuts")
            .child(panel)
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(window.appearance());
        let mode = container_query(window.viewport_size().width);
        let detail_open = self.state.read(cx).is_detail_open();
        let palette_open = self.palette.is_open();
        let shortcuts_open = self.shortcuts_open;

        let sidebar = self.sidebar(theme, cx);
        let top_bar = self.top_bar(theme, cx);
        let status_bar = self.status_bar(theme, cx);
        let content = self.active_content(cx);
        let detail = self.detail.clone();

        let body = match (mode, detail_open) {
            (LayoutMode::Narrow, true) => div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(sidebar)
                .child(div().flex_1().min_w_0().child(detail)),
            (LayoutMode::Narrow, false) => div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(sidebar)
                .child(div().flex_1().min_w_0().child(content)),
            (LayoutMode::Wide, _) => div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(sidebar)
                .child(div().flex_1().min_w_0().child(content))
                .child(
                    div()
                        .w(px(DETAIL_WIDTH))
                        .flex_shrink_0()
                        .border_l_1()
                        .border_color(theme.colors.border)
                        .child(detail),
                ),
        };

        let root = div()
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .bg(theme.colors.bg)
            .text_color(theme.colors.text)
            .text_size(px(theme.font.base))
            .key_context("Root")
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                if this.palette.is_open() {
                    this.close_palette(window, cx);
                } else {
                    this.open_palette(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                this.close_overlay(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleShortcuts, window, cx| {
                this.toggle_shortcuts(window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoInbox, _window, cx| {
                this.on_jump(action, cx);
            }))
            .on_action(cx.listener(|this, action: &GoTasks, _window, cx| {
                this.on_jump(action, cx);
            }))
            .on_action(cx.listener(|this, action: &GoCatalog, _window, cx| {
                this.on_jump(action, cx);
            }))
            .on_action(cx.listener(|this, action: &GoWorkflows, _window, cx| {
                this.on_jump(action, cx);
            }))
            .on_action(cx.listener(|this, action: &GoSchedules, _window, cx| {
                this.on_jump(action, cx);
            }))
            .on_action(cx.listener(|this, action: &GoSettings, _window, cx| {
                this.on_jump(action, cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteUp, _window, cx| {
                this.move_palette(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _window, cx| {
                this.move_palette(1, cx);
            }))
            .on_action(cx.listener(|this, _: &PaletteConfirm, window, cx| {
                this.select_palette(window, cx);
            }))
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    if !this.palette.is_open() {
                        return;
                    }
                    let keystroke = &event.keystroke;
                    if keystroke.modifiers.control
                        || keystroke.modifiers.platform
                        || keystroke.modifiers.alt
                    {
                        return;
                    }
                    if keystroke.key == "backspace" {
                        this.palette.backspace();
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    if let Some(text) = &keystroke.key_char {
                        for character in text.chars() {
                            this.palette.push_char(character);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .child(top_bar)
            .child(body)
            .child(status_bar);

        if palette_open {
            root.child(self.palette_overlay(theme, window, cx))
        } else if shortcuts_open {
            root.child(self.shortcuts_overlay(theme, cx))
        } else {
            root
        }
    }
}
