//! Catalog: browse task definitions, preview their source and launch them.
//!
//! The grouping, fuzzy filter, validation and form state live in
//! [`crate::catalog`] (pure and unit-tested); this file only paints them.
//!
//! Layout follows docs/design/gpui-client.md §2:
//! - **Choice Overload** — one repo namespace, then one folder, then rows; the
//!   search field filters names and prompt text.
//! - **Hick's Law** — the primary action is *Start with variables*; the
//!   one-shot session is a secondary toolbar action.
//! - **Working Memory** — collapsed groups and the query survive the session;
//!   the selected row keeps its preview beside the list.

use gpui::{
    div, prelude::*, px, AnyElement, Context, Div, Entity, FocusHandle, Hsla, IntoElement, Render,
    ScrollHandle, Stateful, Window,
};

use crate::catalog::{
    var_type_label, CatalogEntryView, CatalogGroup, CatalogPreview, CatalogStatus, LaunchForm,
    OneShotFocus, OneShotForm,
};
use crate::components;
use crate::routes::Route;
use crate::state::ClientState;
use crate::theme::Theme;

/// Width of the preview pane beside the list.
const PREVIEW_WIDTH: f32 = 350.0;

/// The Catalog surface.
pub struct CatalogView {
    state: Entity<ClientState>,
    list_scroll: ScrollHandle,
    preview_scroll: ScrollHandle,
    scrolled_to: Option<String>,
    retry_focus: FocusHandle,
    oneshot_focus: FocusHandle,
    copy_focus: FocusHandle,
    preview_retry_focus: FocusHandle,
    launch_cancel_focus: FocusHandle,
    launch_submit_focus: FocusHandle,
    oneshot_cancel_focus: FocusHandle,
    oneshot_submit_focus: FocusHandle,
    error_copy_focus: FocusHandle,
    search_clear_focus: FocusHandle,
    start_focus: FocusHandle,
}

impl CatalogView {
    pub fn new(state: Entity<ClientState>, cx: &mut Context<Self>) -> Self {
        cx.observe(&state, |_this, _state, cx| cx.notify()).detach();
        Self {
            state,
            list_scroll: ScrollHandle::new(),
            preview_scroll: ScrollHandle::new(),
            scrolled_to: None,
            retry_focus: cx.focus_handle().tab_index(20).tab_stop(true),
            oneshot_focus: cx.focus_handle().tab_index(21).tab_stop(true),
            copy_focus: cx.focus_handle().tab_index(22).tab_stop(true),
            preview_retry_focus: cx.focus_handle().tab_index(23).tab_stop(true),
            launch_cancel_focus: cx.focus_handle().tab_index(24).tab_stop(true),
            launch_submit_focus: cx.focus_handle().tab_index(25).tab_stop(true),
            oneshot_cancel_focus: cx.focus_handle().tab_index(26).tab_stop(true),
            oneshot_submit_focus: cx.focus_handle().tab_index(27).tab_stop(true),
            error_copy_focus: cx.focus_handle().tab_index(28).tab_stop(true),
            search_clear_focus: cx.focus_handle().tab_index(29).tab_stop(true),
            start_focus: cx.focus_handle().tab_index(30).tab_stop(true),
        }
    }

    fn header(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let oneshot = components::with_focus(
            div()
                .id("catalog-oneshot")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(34.0))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r8))
                .border_1()
                .border_color(theme.colors.border)
                .text_color(theme.colors.text)
                .text_size(px(theme.font.sm))
                .cursor_pointer()
                .hover(|style| style.bg(theme.colors.surface))
                .track_focus(&self.oneshot_focus)
                .child("New one-shot session")
                .child(components::keycap(theme, "⌘N"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.open_oneshot();
                        cx.notify();
                    });
                })),
            theme,
        );

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s4))
            .py(px(theme.spacing.s3))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(components::section_label(theme, "CATALOG"))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("Task definitions"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::keycap(theme, "↑↓"))
                    .child(hint(theme, "move"))
                    .child(components::keycap(theme, "↵"))
                    .child(hint(theme, "start"))
                    .child(components::keycap(theme, "h"))
                    .child(hint(theme, "collapse"))
                    .child(oneshot),
            )
    }

    /// A fake-caret search field like the Tasks and palette inputs: the shell
    /// keeps focus and routes typed characters here (no second focus ring).
    fn search_field(
        &self,
        theme: &Theme,
        query: &str,
        active: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let clear = (!query.is_empty()).then(|| {
            let state = self.state.clone();
            components::with_focus(
                div()
                    .id("catalog-search-clear")
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(theme.controls.min))
                    .px(px(theme.spacing.s2))
                    .rounded(px(theme.radii.r4))
                    .text_color(theme.colors.muted)
                    .cursor_pointer()
                    .track_focus(&self.search_clear_focus)
                    .hover(|style| style.bg(theme.colors.bg))
                    .child("Clear")
                    .on_click(cx.listener(move |_this, _event, _window, cx| {
                        state.update(cx, |state, cx| {
                            state.clear_catalog_search();
                            cx.notify();
                        });
                        cx.stop_propagation();
                    })),
                theme,
            )
        });

        div()
            .id("catalog-search")
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .flex_1()
            .min_w_0()
            .h(px(36.0))
            .px(px(theme.spacing.s3))
            .rounded(px(theme.radii.r8))
            .border_1()
            .border_color(if active {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(theme.colors.surface)
            .cursor_text()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.activate_catalog_search();
                    cx.notify();
                });
            }))
            .child(
                div()
                    .text_color(if active {
                        theme.colors.accent
                    } else {
                        theme.colors.muted
                    })
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
                            .text_size(px(theme.font.sm))
                            .text_color(if query.is_empty() {
                                theme.colors.muted
                            } else {
                                theme.colors.text
                            })
                            .child(if query.is_empty() {
                                "Search definition names or prompts…".to_string()
                            } else {
                                query.to_string()
                            }),
                    )
                    .when(active, |text| {
                        text.child(div().text_color(theme.colors.accent).child("▌"))
                    }),
            )
            .when_some(clear, |field, clear| field.child(clear))
            .child(components::keycap(
                theme,
                if active || !query.is_empty() {
                    "esc"
                } else {
                    "/"
                },
            ))
    }

    fn controls(
        &self,
        theme: &Theme,
        query: &str,
        active: bool,
        matched: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s4))
            .py(px(theme.spacing.s2))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(self.search_field(theme, query, active, cx))
            .child(hint(
                theme,
                &if query.trim().is_empty() {
                    format!("{total} definition{}", plural(total))
                } else {
                    format!("{matched} of {total} match")
                },
            ))
    }

    fn retry_button(&self, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        components::with_focus(
            div()
                .id("catalog-retry")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(theme.controls.primary))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                .text_color(theme.colors.accent)
                .text_size(px(theme.font.sm))
                .cursor_pointer()
                .track_focus(&self.retry_focus)
                .child("Retry")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.refresh_catalog();
                        cx.notify();
                    });
                })),
            theme,
        )
    }

    fn error_copy_button(
        &self,
        theme: &Theme,
        message: &str,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let message = message.to_string();
        components::copy_button(theme, id, "Copy error", &self.error_copy_focus).on_click(
            cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.copy_text(message.clone(), "the error", cx);
                    cx.notify();
                });
            }),
        )
    }

    fn loading_body(&self, theme: &Theme) -> Div {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s4))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(components::section_label(theme, "DEFINITIONS"))
                    .child(hint(theme, "Loading…")),
            )
            .children((0..7).map(|index| skeleton_row(theme, 130.0 + index as f32 * 22.0)))
    }

    fn error_body(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(theme.spacing.s3))
            .p(px(theme.spacing.s6))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(48.0))
                    .h(px(48.0))
                    .rounded(px(theme.radii.r12))
                    .border_1()
                    .border_color(Hsla::from(theme.colors.danger).opacity(0.35))
                    .bg(Hsla::from(theme.colors.danger).opacity(0.12))
                    .text_color(theme.colors.danger)
                    .text_size(px(theme.font.xl))
                    .child("⚠"),
            )
            .child(components::section_label(theme, "CATALOG"))
            .child(
                div()
                    .text_size(px(theme.font.lg))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child("The catalog could not load"),
            )
            .child(
                div()
                    .max_w(px(520.0))
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .text_center()
                    .child(message.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(self.error_copy_button(theme, message, "catalog-error-copy", cx))
                    .child(self.retry_button(theme, cx)),
            )
    }

    fn error_banner(&self, theme: &Theme, message: &str, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s3))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(Hsla::from(theme.colors.danger).opacity(0.45))
            .bg(Hsla::from(theme.colors.danger).opacity(0.10))
            .child(
                div()
                    .text_color(theme.colors.danger)
                    .text_size(px(theme.font.xs))
                    .child("⚠"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.text)
                    .child(format!("Refresh failed: {message}")),
            )
            .child(self.error_copy_button(theme, message, "catalog-error-copy-banner", cx))
            .child(self.retry_button(theme, cx))
    }

    fn group_header(
        &self,
        theme: &Theme,
        index: usize,
        group: &CatalogGroup,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let collapsed = self.state.read(cx).catalog().is_collapsed(&group.key);
        let key = group.key.clone();
        div()
            .id(("catalog-group", index))
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .w_full()
            .h(px(32.0))
            .px(px(theme.spacing.s2))
            .rounded(px(theme.radii.r4))
            .cursor_pointer()
            .hover(|style| style.bg(theme.colors.surface))
            .child(
                div()
                    .w(px(14.0))
                    .text_center()
                    .text_color(theme.colors.muted)
                    .child(if collapsed { "▸" } else { "▾" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(theme.font.sm))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.colors.text)
                    .child(group.label.clone()),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(format!("{} definition{}", group.count, plural(group.count))),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.toggle_catalog_group(&key);
                    cx.notify();
                });
            }))
    }

    fn folder_header(
        &self,
        theme: &Theme,
        index: usize,
        group: &CatalogGroup,
        folder: &str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let key = if folder == "(root)" {
            group.key.clone()
        } else {
            format!("{}/{}", group.key, folder)
        };
        let collapsed = self.state.read(cx).catalog().is_collapsed(&key);
        div()
            .id(("catalog-folder", index))
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .w_full()
            .h(px(theme.controls.min))
            .pl(px(theme.spacing.s6))
            .pr(px(theme.spacing.s2))
            .cursor_pointer()
            .hover(|style| style.bg(theme.colors.surface))
            .child(
                div()
                    .w(px(12.0))
                    .text_center()
                    .text_color(theme.colors.muted)
                    .text_size(px(theme.font.xs))
                    .child(if collapsed { "▸" } else { "▾" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family("monospace")
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(if folder == "(root)" {
                        folder.to_string()
                    } else {
                        format!("{folder}/")
                    }),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.toggle_catalog_group(&key);
                    cx.notify();
                });
            }))
    }

    fn row(
        &self,
        theme: &Theme,
        index: usize,
        entry: &CatalogEntryView,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let name = entry.name.clone();
        let required = (entry.required_vars > 0).then(|| {
            div()
                .flex_shrink_0()
                .px(px(theme.spacing.s1 + 2.0))
                .py(px(1.0))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(Hsla::from(theme.colors.warning).opacity(0.45))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.warning)
                .child(format!("{} required", entry.required_vars))
        });
        let vars = (!entry.vars.is_empty()).then(|| {
            meta_chip(
                theme,
                format!(
                    "{} var{}",
                    entry.vars.len(),
                    if entry.vars.len() == 1 { "" } else { "s" }
                ),
            )
        });
        let kind = if entry.kind == "action" { "⚙" } else { "✳" };

        div()
            .id(("catalog-row", index))
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .w_full()
            .min_h(px(36.0))
            .px(px(theme.spacing.s2))
            .py(px(theme.spacing.s1))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(if selected {
                theme.colors.accent
            } else {
                theme.colors.bg
            })
            .bg(if selected {
                Hsla::from(theme.colors.accent).opacity(0.10)
            } else {
                Hsla::from(theme.colors.bg)
            })
            .when(!selected, |row| {
                row.hover(|style| style.bg(Hsla::from(theme.colors.accent).opacity(0.05)))
            })
            .cursor_pointer()
            .child(
                div()
                    .w(px(18.0))
                    .text_center()
                    .text_size(px(theme.font.xs))
                    .text_color(if selected {
                        theme.colors.accent
                    } else {
                        theme.colors.muted
                    })
                    .child(kind),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family("monospace")
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(entry.label.clone()),
            )
            .when_some(required, |row, badge| row.child(badge))
            .when_some(vars, |row, badge| row.child(badge))
            .when_some(entry.agent.clone(), |row, agent| {
                row.child(meta_chip(theme, format!("agent {agent}")))
            })
            .when_some(entry.schedule.clone(), |row, schedule| {
                row.child(meta_chip(theme, format!("◷ {schedule}")))
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.select_catalog_entry(&name);
                    cx.notify();
                });
            }))
    }

    fn list_pane(
        &self,
        theme: &Theme,
        groups: &[CatalogGroup],
        selected: Option<&str>,
        matched: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mut scroll = div()
            .id("catalog-list-scroll")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(theme.spacing.s2));

        if total == 0 {
            scroll = scroll.child(components::empty_state(
                theme,
                Route::Catalog.icon(),
                "DEFINITIONS",
                "No task definitions",
                "Task definitions are Markdown files under this repo's `.favetto/tasks/` directory (or the daemon's global tasks directory). Add a `.md` file with a TOML header and it appears here.",
                "Add a file under .favetto/tasks/",
            ));
        } else if groups.is_empty() {
            let state = self.state.clone();
            scroll = scroll.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .p(px(theme.spacing.s6))
                    .child(
                        div()
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.text)
                            .child("No definition matches the search."),
                    )
                    .child(
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .child(format!(
                                "{matched} of {total} definitions match. Esc clears the search."
                            )),
                    )
                    .child(components::with_focus(
                        div()
                            .id("catalog-clear-search")
                            .px(px(theme.spacing.s3))
                            .py(px(theme.spacing.s1))
                            .rounded(px(theme.radii.r4))
                            .border_1()
                            .border_color(theme.colors.border)
                            .text_size(px(theme.font.sm))
                            .text_color(theme.colors.accent)
                            .cursor_pointer()
                            .track_focus(&self.search_clear_focus)
                            .child("Clear search")
                            .on_click(cx.listener(move |_this, _event, _window, cx| {
                                state.update(cx, |state, cx| {
                                    state.clear_catalog_search();
                                    cx.notify();
                                });
                            })),
                        theme,
                    )),
            );
        } else {
            let mut index = 0usize;
            for group in groups {
                if self.state.read(cx).catalog().is_collapsed(&group.key) {
                    scroll = scroll.child(self.group_header(theme, index, group, cx));
                    index += 1;
                    continue;
                }
                scroll = scroll.child(self.group_header(theme, index, group, cx));
                index += 1;
                for folder in &group.folders {
                    if self.state.read(cx).catalog().is_collapsed(&folder.key) {
                        scroll = scroll.child(self.folder_header(
                            theme,
                            index,
                            group,
                            &folder.label,
                            cx,
                        ));
                        index += 1;
                        continue;
                    }
                    scroll =
                        scroll.child(self.folder_header(theme, index, group, &folder.label, cx));
                    index += 1;
                    for entry in &folder.entries {
                        let is_selected = selected == Some(entry.name.as_str());
                        scroll = scroll.child(self.row(theme, index, entry, is_selected, cx));
                        index += 1;
                    }
                }
            }
        }
        scroll
    }

    fn preview_pane(
        &self,
        theme: &Theme,
        selected: Option<&str>,
        preview: Option<&CatalogPreview>,
        cx: &mut Context<Self>,
    ) -> Div {
        let header = div()
            .flex()
            .items_center()
            .gap(px(theme.spacing.s2))
            .px(px(theme.spacing.s3))
            .py(px(theme.spacing.s2))
            .border_b_1()
            .border_color(theme.colors.border)
            .child(components::section_label(theme, "SOURCE"))
            .child(div().flex_1())
            .when_some(
                preview.and_then(|preview| preview.markdown.as_ref()),
                |header, _markdown| {
                    header.child(components::with_focus(
                        div()
                            .id("catalog-copy")
                            .px(px(theme.spacing.s1 + 2.0))
                            .rounded(px(theme.radii.r4))
                            .border_1()
                            .border_color(theme.colors.border)
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.muted)
                            .cursor_pointer()
                            .track_focus(&self.copy_focus)
                            .child("Copy")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.copy_catalog_source(cx);
                                    cx.notify();
                                });
                            })),
                        theme,
                    ))
                },
            );

        let body: AnyElement = match (selected, preview) {
            (Some(name), Some(preview)) if preview.name == name => {
                if let Some(markdown) = &preview.markdown {
                    div()
                        .id("catalog-preview")
                        .font_family("monospace")
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.text)
                        .whitespace_normal()
                        .child(markdown.clone())
                        .into_any_element()
                } else if let Some(error) = &preview.error {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(theme.spacing.s2))
                        .p(px(theme.spacing.s3))
                        .child(
                            div()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.danger)
                                .child(format!("The source could not load: {error}")),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(theme.spacing.s2))
                                .child(self.error_copy_button(
                                    theme,
                                    error,
                                    "catalog-preview-error-copy",
                                    cx,
                                ))
                                .child(components::with_focus(
                                    div()
                                        .id("catalog-preview-retry")
                                        .px(px(theme.spacing.s2))
                                        .py(px(2.0))
                                        .rounded(px(theme.radii.r4))
                                        .border_1()
                                        .border_color(theme.colors.border)
                                        .text_size(px(theme.font.xs))
                                        .text_color(theme.colors.accent)
                                        .cursor_pointer()
                                        .track_focus(&self.preview_retry_focus)
                                        .child("Retry")
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.state.update(cx, |state, cx| {
                                                state.retry_catalog_preview();
                                                cx.notify();
                                            });
                                        })),
                                    theme,
                                )),
                        )
                        .into_any_element()
                } else {
                    div()
                        .p(px(theme.spacing.s3))
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.muted)
                        .child("Loading source…")
                        .into_any_element()
                }
            }
            (Some(name), _) => div()
                .p(px(theme.spacing.s3))
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.muted)
                .child(format!("Loading {name}…"))
                .into_any_element(),
            (None, _) => div()
                .p(px(theme.spacing.s3))
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.muted)
                .child("Select a definition to read its Markdown source.")
                .into_any_element(),
        };

        let start = selected.map(|name| {
            let name = name.to_string();
            components::with_focus(
                div()
                    .id("catalog-start")
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .h(px(theme.controls.primary))
                    .px(px(theme.spacing.s3))
                    .rounded(px(theme.radii.r8))
                    .bg(Hsla::from(theme.colors.accent).opacity(0.16))
                    .text_size(px(theme.font.sm))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.colors.accent)
                    .cursor_pointer()
                    .track_focus(&self.start_focus)
                    .child("Start with variables…")
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.state.update(cx, |state, cx| {
                            state.open_catalog_launch(&name, cx);
                            cx.notify();
                        });
                    })),
                theme,
            )
        });

        div()
            .w(px(PREVIEW_WIDTH))
            .flex_shrink_0()
            .min_h_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.bg)
            .child(header)
            .child(
                div()
                    .id("catalog-preview-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.preview_scroll)
                    .p(px(theme.spacing.s3))
                    .child(body),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s2))
                    .px(px(theme.spacing.s3))
                    .py(px(theme.spacing.s2))
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .child(hint(theme, "Raw Markdown · read-only"))
                    .when_some(start, |footer, start| footer.child(start)),
            )
    }

    fn metadata_chip(&self, theme: &Theme, label: String) -> Div {
        div()
            .flex_shrink()
            .min_w(px(0.0))
            .max_w(px(220.0))
            .truncate()
            .px(px(theme.spacing.s1 + 2.0))
            .py(px(2.0))
            .rounded(px(theme.radii.r4))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.bg)
            .text_size(px(theme.font.xs))
            .text_color(theme.colors.muted)
            .child(label)
    }

    fn launch_sheet(&self, theme: &Theme, form: &LaunchForm, cx: &mut Context<Self>) -> Div {
        let entry = &form.entry;
        let mut metadata = div().flex().flex_wrap().gap(px(theme.spacing.s1 + 2.0));
        if let Some(agent) = &entry.agent {
            metadata = metadata.child(self.metadata_chip(theme, format!("agent {agent}")));
        }
        if entry.provider.is_some() || entry.model.is_some() {
            let provider = entry.provider.as_deref().unwrap_or("default provider");
            let model = entry.model.as_deref().unwrap_or("default model");
            metadata = metadata.child(self.metadata_chip(theme, format!("{provider} · {model}")));
        }
        if let Some(cwd) = &entry.cwd {
            metadata = metadata.child(self.metadata_chip(theme, format!("cwd {cwd}")));
        }
        if let Some(schedule) = &entry.schedule {
            metadata = metadata.child(self.metadata_chip(theme, format!("◷ {schedule}")));
        }
        if let Some(needs) = &entry.needs {
            metadata = metadata.child(self.metadata_chip(theme, format!("needs {needs}")));
        }
        if !entry.kind.is_empty() {
            metadata = metadata.child(self.metadata_chip(theme, format!("kind {}", entry.kind)));
        }

        let mut fields = div().flex().flex_col().gap(px(theme.spacing.s3));
        if form.fields.is_empty() {
            fields = fields.child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.muted)
                    .child("This definition declares no variables; it launches with no input."),
            );
        }
        for (index, field) in form.fields.iter().enumerate() {
            let focused = form.focused == index;
            let value = field.value.clone();
            let multiline = field.var.multiline;
            let prompt = if field.var.prompt.trim().is_empty() {
                field.var.name.clone()
            } else {
                field.var.prompt.clone()
            };
            let error = field.error.clone();
            let choices = field.var.choices.clone();
            let state = self.state.clone();

            let mut label = div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.text)
                        .child(prompt),
                )
                .child(
                    div()
                        .font_family("monospace")
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.muted)
                        .child(field.var.name.clone()),
                )
                .child(meta_chip(
                    theme,
                    var_type_label(field.var.var_type).to_string(),
                ));
            if field.var.required {
                label = label.child(
                    div()
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.warning)
                        .child("required"),
                );
            }

            let box_ = div()
                .id(("catalog-var", index))
                .flex()
                .flex_col()
                .justify_center()
                .min_h(px(if multiline { 80.0 } else { 36.0 }))
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s2))
                .rounded(px(theme.radii.r8))
                .border_1()
                .border_color(if focused {
                    theme.colors.accent
                } else if error.is_some() {
                    theme.colors.danger
                } else {
                    theme.colors.border
                })
                .bg(theme.colors.bg)
                .cursor_text()
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.focus_catalog_field(index);
                        cx.notify();
                    });
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .when(multiline, |line| line.items_start())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .text_size(px(theme.font.sm))
                                .text_color(if value.is_empty() {
                                    theme.colors.muted
                                } else {
                                    theme.colors.text
                                })
                                .child(if value.is_empty() {
                                    "Type a value…".to_string()
                                } else {
                                    value.clone()
                                }),
                        )
                        .when(focused, |line| {
                            line.child(div().text_color(theme.colors.accent).child("▌"))
                        }),
                );

            let choice_row = choices.map(|choices| {
                let mut row = div().flex().flex_wrap().gap(px(theme.spacing.s1));
                for (choice_index, choice) in choices.into_iter().enumerate() {
                    let selected = field.value == choice;
                    let choice_value = choice.clone();
                    let state = state.clone();
                    row = row.child(
                        div()
                            .id(("catalog-choice", index * 100 + choice_index))
                            .flex()
                            .items_center()
                            .h(px(theme.controls.min))
                            .px(px(theme.spacing.s2))
                            .rounded(px(theme.radii.r4))
                            .border_1()
                            .border_color(if selected {
                                theme.colors.accent
                            } else {
                                theme.colors.border
                            })
                            .when(selected, |chip| {
                                chip.bg(Hsla::from(theme.colors.accent).opacity(0.16))
                                    .text_color(theme.colors.accent)
                            })
                            .when(!selected, |chip| chip.text_color(theme.colors.muted))
                            .text_size(px(theme.font.xs))
                            .cursor_pointer()
                            .when(selected, |chip| chip.child("✓"))
                            .child(choice)
                            .on_click(cx.listener(move |_this, _event, _window, cx| {
                                state.update(cx, |state, cx| {
                                    state.set_catalog_field_value(index, choice_value.clone());
                                    cx.notify();
                                });
                            })),
                    );
                }
                row
            });

            let error_line = error.map(|error| {
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.danger)
                    .child(error)
            });

            fields = fields.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s1))
                    .child(label)
                    .child(box_)
                    .when_some(choice_row, |field, row| field.child(row))
                    .when_some(error_line, |field, line| field.child(line)),
            );
        }

        let server_error = form.server_error.clone().map(|message| {
            let copy = self.error_copy_button(theme, &message, "catalog-sheet-error-copy", cx);
            div()
                .flex()
                .items_start()
                .gap(px(theme.spacing.s2))
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(Hsla::from(theme.colors.danger).opacity(0.5))
                .bg(Hsla::from(theme.colors.danger).opacity(0.10))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.colors.danger)
                        .text_size(px(theme.font.xs))
                        .child("⚠"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.text)
                        .child(message),
                )
                .child(copy)
        });

        let cancel = components::with_focus(
            div()
                .id("catalog-launch-cancel")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.launch_cancel_focus)
                .child("Cancel")
                .child(components::keycap(theme, "esc"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.close_catalog_launch();
                        cx.notify();
                    });
                })),
            theme,
        );

        let submit = components::with_focus(
            div()
                .id("catalog-launch-submit")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(40.0))
                .px(px(theme.spacing.s4))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(theme.colors.accent).opacity(if form.submitting {
                    0.08
                } else {
                    0.20
                }))
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.colors.accent)
                .cursor_pointer()
                .track_focus(&self.launch_submit_focus)
                .child(if form.submitting {
                    "Starting…"
                } else {
                    "Launch"
                })
                .child(components::keycap(theme, "⌘↵"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.submit_catalog_launch();
                        cx.notify();
                    });
                })),
            theme,
        );

        let panel = div()
            .w(px(620.0))
            .max_h(px(680.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(theme.radii.r12))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s1))
                    .p(px(theme.spacing.s4))
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child(format!("Start {}", entry.name)),
                    )
                    .child(metadata),
            )
            .child(
                div()
                    .id("catalog-launch-fields")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s3))
                    .p(px(theme.spacing.s4))
                    .child(fields)
                    .when_some(server_error, |body, error| body.child(error)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s3))
                    .p(px(theme.spacing.s3))
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(components::keycap(theme, "tab"))
                            .child(hint(theme, "next field"))
                            .child(components::keycap(theme, "←→"))
                            .child(hint(theme, "choice"))
                            .child(components::keycap(theme, "⌘↵"))
                            .child(hint(theme, "launch")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(cancel)
                            .child(submit),
                    ),
            );

        overlay(theme, panel)
    }

    fn oneshot_sheet(&self, theme: &Theme, form: &OneShotForm, cx: &mut Context<Self>) -> Div {
        let agent_rows: Vec<AnyElement> = if form.agents.is_empty() {
            vec![div()
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.muted)
                .child(
                    "The daemon reported no agents. Configure one in the daemon before opening a session.",
                )
                .into_any_element()]
        } else {
            form.agents
                .iter()
                .enumerate()
                .map(|(agent_index, choice)| {
                    let selected = form.agent.as_deref() == Some(choice.name.as_str());
                    let focused = form.focused == OneShotFocus::Agent && selected;
                    let name = choice.name.clone();
                    let state = self.state.clone();
                    let badge = choice
                        .default
                        .then(|| meta_chip(theme, "default".to_string()));
                    let unavailable = (!choice.available).then(|| {
                        div()
                            .text_size(px(theme.font.xs))
                            .text_color(theme.colors.warning)
                            .child("not found")
                    });
                    div()
                        .id(("catalog-agent", agent_index))
                        .flex()
                        .items_center()
                        .gap(px(theme.spacing.s2))
                        .w_full()
                        .h(px(34.0))
                        .px(px(theme.spacing.s2))
                        .rounded(px(theme.radii.r4))
                        .border_1()
                        .border_color(if focused {
                            theme.colors.accent
                        } else {
                            theme.colors.bg
                        })
                        .when(selected, |row| {
                            row.bg(Hsla::from(theme.colors.accent).opacity(0.10))
                        })
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.colors.surface))
                        .child(
                            div()
                                .w(px(16.0))
                                .text_center()
                                .text_color(if selected {
                                    theme.colors.accent
                                } else {
                                    theme.colors.muted
                                })
                                .child(if selected { "●" } else { "○" }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(theme.font.sm))
                                .text_color(theme.colors.text)
                                .child(choice.display_name.clone()),
                        )
                        .child(
                            div()
                                .font_family("monospace")
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.muted)
                                .child(choice.name.clone()),
                        )
                        .when_some(badge, |row, badge| row.child(badge))
                        .when_some(unavailable, |row, badge| row.child(badge))
                        .on_click(cx.listener(move |_this, _event, _window, cx| {
                            state.update(cx, |state, cx| {
                                state.select_oneshot_agent(&name);
                                cx.notify();
                            });
                        }))
                        .into_any_element()
                })
                .collect()
        };

        let cwd_focused = form.focused == OneShotFocus::Cwd;
        let cwd_value = form.cwd.clone();
        let cwd_box = div()
            .id("catalog-oneshot-cwd")
            .flex()
            .items_center()
            .h(px(36.0))
            .px(px(theme.spacing.s3))
            .rounded(px(theme.radii.r8))
            .border_1()
            .border_color(if cwd_focused {
                theme.colors.accent
            } else {
                theme.colors.border
            })
            .bg(theme.colors.bg)
            .cursor_text()
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.focus_oneshot(OneShotFocus::Cwd);
                    cx.notify();
                });
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(theme.font.sm))
                    .text_color(if cwd_value.is_empty() {
                        theme.colors.muted
                    } else {
                        theme.colors.text
                    })
                    .child(if cwd_value.is_empty() {
                        "Daemon cwd (optional)".to_string()
                    } else {
                        cwd_value.clone()
                    }),
            )
            .when(cwd_focused, |field| {
                field.child(div().text_color(theme.colors.accent).child("▌"))
            });

        let server_error = form.server_error.clone().map(|message| {
            let copy = self.error_copy_button(theme, &message, "catalog-sheet-error-copy", cx);
            div()
                .flex()
                .items_start()
                .gap(px(theme.spacing.s2))
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(Hsla::from(theme.colors.danger).opacity(0.5))
                .bg(Hsla::from(theme.colors.danger).opacity(0.10))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.colors.danger)
                        .text_size(px(theme.font.xs))
                        .child("⚠"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(theme.font.sm))
                        .text_color(theme.colors.text)
                        .child(message),
                )
                .child(copy)
        });

        let cancel = components::with_focus(
            div()
                .id("catalog-oneshot-cancel")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(36.0))
                .px(px(theme.spacing.s3))
                .rounded(px(theme.radii.r4))
                .border_1()
                .border_color(theme.colors.border)
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.oneshot_cancel_focus)
                .child("Cancel")
                .child(components::keycap(theme, "esc"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.close_oneshot();
                        cx.notify();
                    });
                })),
            theme,
        );

        let submit = components::with_focus(
            div()
                .id("catalog-oneshot-submit")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(40.0))
                .px(px(theme.spacing.s4))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(theme.colors.accent).opacity(if form.submitting {
                    0.08
                } else {
                    0.20
                }))
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.colors.accent)
                .cursor_pointer()
                .track_focus(&self.oneshot_submit_focus)
                .child(if form.submitting {
                    "Opening…"
                } else {
                    "Open session"
                })
                .child(components::keycap(theme, "⌘↵"))
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.submit_oneshot();
                        cx.notify();
                    });
                })),
            theme,
        );

        let panel = div()
            .w(px(560.0))
            .max_h(px(640.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(theme.radii.r12))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .shadow_lg()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s1))
                    .p(px(theme.spacing.s4))
                    .border_b_1()
                    .border_color(theme.colors.border)
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child("New one-shot session"),
                    )
                    .child(hint(
                        theme,
                        "An inline agent session that is not part of the catalog.",
                    )),
            )
            .child(
                div()
                    .id("catalog-oneshot-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(theme.spacing.s3))
                    .p(px(theme.spacing.s4))
                    .child(components::section_label(theme, "AGENT"))
                    .child(div().flex().flex_col().gap(px(2.0)).children(agent_rows))
                    .child(components::section_label(theme, "WORKING DIRECTORY"))
                    .child(cwd_box)
                    .when_some(server_error, |body, error| body.child(error)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s3))
                    .p(px(theme.spacing.s3))
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(components::keycap(theme, "tab"))
                            .child(hint(theme, "agent / cwd"))
                            .child(components::keycap(theme, "⌘↵"))
                            .child(hint(theme, "open")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme.spacing.s2))
                            .child(cancel)
                            .child(submit),
                    ),
            );

        overlay(theme, panel)
    }
}

impl Render for CatalogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let (
            status,
            groups,
            query,
            search_active,
            selected,
            preview,
            launch,
            oneshot,
            matched,
            total,
            selected_index,
        ) = {
            let state = self.state.read(cx);
            let catalog = state.catalog();
            let selected = catalog.selected().map(str::to_string);
            (
                catalog.status().clone(),
                catalog.groups(),
                catalog.query().to_string(),
                catalog.is_search_active(),
                selected.clone(),
                catalog.preview().cloned(),
                catalog.launch().cloned(),
                catalog.oneshot().cloned(),
                catalog.matched_count(),
                catalog.total_count(),
                selected
                    .as_deref()
                    .and_then(|name| catalog.display_index_of(name)),
            )
        };

        if selected != self.scrolled_to {
            if let Some(index) = selected_index {
                self.list_scroll.scroll_to_item(index);
            }
            self.scrolled_to = selected.clone();
        }

        let overlays = launch
            .as_ref()
            .map(|form| self.launch_sheet(theme, form, cx))
            .or_else(|| {
                oneshot
                    .as_ref()
                    .map(|form| self.oneshot_sheet(theme, form, cx))
            });

        if matches!(&status, CatalogStatus::Loading) && total == 0 {
            return div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .bg(theme.colors.bg)
                .child(self.header(theme, cx))
                .child(self.controls(theme, &query, search_active, matched, total, cx))
                .child(self.loading_body(theme))
                .when_some(overlays, |root, overlay| root.child(overlay));
        }

        if let CatalogStatus::Failed { message } = &status {
            if total == 0 {
                return div()
                    .relative()
                    .size_full()
                    .flex()
                    .flex_col()
                    .bg(theme.colors.bg)
                    .child(self.header(theme, cx))
                    .child(self.controls(theme, &query, search_active, matched, total, cx))
                    .child(self.error_body(theme, message, cx))
                    .when_some(overlays, |root, overlay| root.child(overlay));
            }
        }

        let mut root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.colors.bg)
            .child(self.header(theme, cx))
            .child(self.controls(theme, &query, search_active, matched, total, cx));

        if let CatalogStatus::Failed { message } = &status {
            root = root.child(
                div()
                    .px(px(theme.spacing.s3))
                    .py(px(theme.spacing.s2))
                    .child(self.error_banner(theme, message, cx)),
            );
        }

        root.child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(self.list_pane(theme, &groups, selected.as_deref(), matched, total, cx))
                .when(total > 0, |split| {
                    split.child(self.preview_pane(theme, selected.as_deref(), preview.as_ref(), cx))
                }),
        )
        .when_some(overlays, |root, overlay| root.child(overlay))
    }
}

fn overlay(theme: &Theme, panel: Div) -> Div {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(Hsla::from(theme.colors.bg).opacity(0.86))
        .child(panel)
}

fn hint(theme: &Theme, text: &str) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(text.to_string())
}

fn meta_chip(theme: &Theme, label: String) -> Div {
    div()
        .flex_shrink()
        .min_w(px(0.0))
        .max_w(px(160.0))
        .truncate()
        .px(px(theme.spacing.s1 + 2.0))
        .py(px(1.0))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.bg)
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child(label)
}

fn skeleton_row(theme: &Theme, width: f32) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(theme.spacing.s3))
        .h(px(36.0))
        .px(px(theme.spacing.s3))
        .rounded(px(theme.radii.r4))
        .border_1()
        .border_color(theme.colors.border)
        .bg(theme.colors.surface)
        .child(
            div()
                .w(px(3.0))
                .h(px(22.0))
                .rounded(px(2.0))
                .bg(theme.colors.border),
        )
        .child(
            div()
                .w(px(90.0))
                .h(px(10.0))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.border),
        )
        .child(
            div()
                .w(px(width))
                .h(px(10.0))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.border),
        )
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}
