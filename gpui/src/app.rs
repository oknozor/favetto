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
    div, prelude::*, px, Animation, AnimationExt, AnyElement, ClipboardItem, Context, Div, Entity,
    FocusHandle, Hsla, IntoElement, MouseButton, Render, Stateful, Window,
};

use crate::actions::{
    self, AgentClose, AgentInterrupt, AgentNewSession, CatalogChoiceNext, CatalogChoicePrev,
    CatalogFieldNext, CatalogFieldPrev, CatalogLaunchSubmit, CatalogNewOneshot, CatalogSearch,
    CatalogToggleFolder, CloseOverlay, DetailPromptComplete, DetailPromptNext, DetailPromptPrev,
    DetailPromptSend, ElicitationCancel, ElicitationConfirm, ElicitationDecline, ElicitationEdit,
    ElicitationLeft, ElicitationNext, ElicitationPrev, ElicitationRight, ElicitationToggle,
    FocusNext, FocusPrev, GoAgent, GoCatalog, GoInbox, GoSchedules, GoSettings, GoTasks,
    GoWorkflows, ListDown, ListEnd, ListHome, ListOpen, ListPageDown, ListPageUp, ListUp,
    PaletteConfirm, PaletteDown, PaletteUp, Refresh, RepoPickerConfirm, RepoPickerDown,
    RepoPickerUp, ScheduleDelete, ScheduleEdit, ScheduleFieldNext, ScheduleFieldPrev, ScheduleNew,
    ScheduleSubmit, ScheduleToggleEnabled, SettingsActivate, SettingsFieldNext, SettingsFieldPrev,
    SettingsSave, SettingsTest, TaskCancel, TaskConfirmAccept, TaskConfirmReject,
    TaskDismissLaunch, TaskFilterNext, TaskFilterPrev, TaskRetry, TaskSearch, TextBackspace,
    TextCopy, TextCut, TextDelete, TextDeleteWordBackward, TextDeleteWordForward, TextMoveDown,
    TextMoveEnd, TextMoveHome, TextMoveLeft, TextMoveRight, TextMoveUp, TextMoveWordLeft,
    TextMoveWordRight, TextNewline, TextPaste, TextSelectAll, TextSelectDown, TextSelectEnd,
    TextSelectHome, TextSelectLeft, TextSelectRight, TextSelectUp, TextSelectWordLeft,
    TextSelectWordRight, TogglePalette, ToggleShortcuts, WorkflowCancelRoot, WorkflowOpenInTasks,
    WorkflowRetryInstance, WorkflowRootNext, WorkflowRootPrev,
};
use crate::catalog::OneShotFocus;
use crate::components;
use crate::layout::{clamp_detail_width, container_query, shortcut_sheet_layout, LayoutMode};
use crate::palette::{CommandKind, CommandPalette};
use crate::repos::RepoOption;
use crate::resize::{ResizeDrag, ResizePreview, ResizeStart};
use crate::routes::Route;
use crate::schedules::ScheduleField;
use crate::state::{ClientState, TaskConfirm, Toast, ToastKind};
use crate::tasks::TaskCommand;
use crate::text_element::{TextEditState, TextElement, TextHost};
use crate::text_input::{TextInput, TextTarget};
use crate::theme::{
    Theme, DETAIL_WIDTH, RESIZE_HANDLE, SIDEBAR_WIDTH, STATUS_BAR_HEIGHT, TOP_BAR_HEIGHT,
};
use crate::views::{
    AgentView, CatalogView, DetailView, InboxView, SchedulesView, SettingsView, TasksView,
    WorkflowsView,
};
use crate::workflow::WorkflowCancelConfirm;

/// A Home/End/Page key, routed to the active list.
enum ListJump {
    Home,
    End,
    PageUp,
    PageDown,
}

/// The route entities, created once with the shell.
struct RouteViews {
    inbox: Entity<InboxView>,
    tasks: Entity<TasksView>,
    agent: Entity<AgentView>,
    catalog: Entity<CatalogView>,
    workflows: Entity<WorkflowsView>,
    schedules: Entity<SchedulesView>,
    settings: Entity<SettingsView>,
}

/// The editing commands the keymap and text actions share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextCommand {
    MoveLeft,
    MoveRight,
    MoveWordLeft,
    MoveWordRight,
    MoveHome,
    MoveEnd,
    MoveUp,
    MoveDown,
    SelectLeft,
    SelectRight,
    SelectWordLeft,
    SelectWordRight,
    SelectHome,
    SelectEnd,
    SelectUp,
    SelectDown,
    SelectAll,
    Backspace,
    Delete,
    DeleteWordBackward,
    DeleteWordForward,
    Newline,
}

fn apply_text_command(input: &mut TextInput, command: TextCommand, multiline: bool) -> bool {
    match command {
        TextCommand::MoveLeft => input.move_left(),
        TextCommand::MoveRight => input.move_right(),
        TextCommand::MoveWordLeft => input.move_word_left(),
        TextCommand::MoveWordRight => input.move_word_right(),
        TextCommand::MoveHome => input.move_home(),
        TextCommand::MoveEnd => input.move_end(),
        TextCommand::MoveUp => input.move_line_up(),
        TextCommand::MoveDown => input.move_line_down(),
        TextCommand::SelectLeft => input.select_left(),
        TextCommand::SelectRight => input.select_right(),
        TextCommand::SelectWordLeft => input.select_word_left(),
        TextCommand::SelectWordRight => input.select_word_right(),
        TextCommand::SelectHome => input.select_home(),
        TextCommand::SelectEnd => input.select_end(),
        TextCommand::SelectUp => input.select_line_up(),
        TextCommand::SelectDown => input.select_line_down(),
        TextCommand::SelectAll => input.select_all(),
        TextCommand::Backspace => input.backspace(),
        TextCommand::Delete => input.delete(),
        TextCommand::DeleteWordBackward => input.delete_word_backwards(),
        TextCommand::DeleteWordForward => input.delete_word_forwards(),
        TextCommand::Newline => {
            if !multiline {
                return false;
            }
            input.insert("\n");
        }
    }
    true
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
    repo_button_focus: FocusHandle,
    repo_picker_focus: FocusHandle,
    repo_picker_was_open: bool,
    settings_button_focus: FocusHandle,
    shortcuts_button_focus: FocusHandle,
    reconnect_button_focus: FocusHandle,
    toast_dismiss_focus: FocusHandle,
    confirm_back_focus: FocusHandle,
    confirm_accept_focus: FocusHandle,
    confirm_copy_focus: FocusHandle,
    toast_copy_focus: FocusHandle,
    shortcuts_close_focus: FocusHandle,
    detail_width: f32,
    detail_resize: Option<ResizeStart>,
    /// Shaped palette lines for click-to-place-caret and drag selection.
    palette_text: TextEditState,
}

/// The full-window frame every route lives in.
///
/// `w_full()` is not redundant with `h_full()`: GPUI lays a root view out at
/// its content width when no width is requested. A window wider than that
/// content would then leave the surplus columns unpainted (the black strip
/// from issue #441) and the working list would never receive its share of the
/// extra space. Keeping both axes explicit makes the resize contract testable.
fn shell_root() -> Div {
    div().relative().flex().flex_col().w_full().h_full()
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
            agent: cx.new(|cx| AgentView::new(state.clone(), cx)),
            catalog: cx.new(|cx| CatalogView::new(state.clone(), cx)),
            workflows: cx.new(|cx| WorkflowsView::new(state.clone(), cx)),
            schedules: cx.new(|cx| SchedulesView::new(state.clone(), cx)),
            settings: cx.new(|cx| SettingsView::new(state.clone(), cx)),
        };
        let detail = cx.new(|cx| DetailView::new(state.clone(), cx));

        let root_focus = cx.focus_handle();
        let palette_focus = cx.focus_handle().tab_index(100).tab_stop(true);
        let palette_button_focus = cx.focus_handle().tab_index(1).tab_stop(true);
        let repo_button_focus = cx.focus_handle().tab_index(2).tab_stop(true);
        let repo_picker_focus = cx.focus_handle().tab_index(100).tab_stop(true);
        let settings_button_focus = cx.focus_handle().tab_index(3).tab_stop(true);
        let nav_focus = Route::ALL
            .iter()
            .map(|route| {
                cx.focus_handle()
                    .tab_index(10 + route.index() as isize)
                    .tab_stop(true)
            })
            .collect();
        let shortcuts_button_focus = cx.focus_handle().tab_index(70).tab_stop(true);
        let reconnect_button_focus = cx.focus_handle().tab_index(71).tab_stop(true);
        let toast_dismiss_focus = cx.focus_handle().tab_index(72).tab_stop(true);
        let confirm_back_focus = cx.focus_handle().tab_index(73).tab_stop(true);
        let confirm_accept_focus = cx.focus_handle().tab_index(74).tab_stop(true);
        let confirm_copy_focus = cx.focus_handle().tab_index(75).tab_stop(true);
        let toast_copy_focus = cx.focus_handle().tab_index(76).tab_stop(true);
        let shortcuts_close_focus = cx.focus_handle().tab_index(100).tab_stop(true);

        state.update(cx, |state, _| state.set_shell_focus(root_focus.clone()));
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
            repo_button_focus,
            repo_picker_focus,
            repo_picker_was_open: false,
            settings_button_focus,
            shortcuts_button_focus,
            reconnect_button_focus,
            toast_dismiss_focus,
            confirm_back_focus,
            confirm_accept_focus,
            confirm_copy_focus,
            toast_copy_focus,
            shortcuts_close_focus,
            detail_width: DETAIL_WIDTH,
            detail_resize: None,
            palette_text: TextEditState::default(),
        }
    }

    fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.close_repo_picker();
            state.navigate(route);
            cx.notify();
        });
        self.palette.close();
        self.shortcuts_open = false;
        cx.notify();
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shortcuts_open = false;
        self.state.update(cx, |state, cx| {
            state.cancel_task_confirm();
            state.close_repo_picker();
            cx.notify();
        });
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
            self.state.update(cx, |state, cx| {
                state.cancel_task_confirm();
                state.close_repo_picker();
                cx.notify();
            });
            window.focus(&self.shortcuts_close_focus);
        } else {
            window.focus(&self.root_focus);
        }
        cx.notify();
    }

    /// Open the searchable repository switcher (the top-bar button or palette).
    fn open_repo_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.close();
        self.shortcuts_open = false;
        self.state.update(cx, |state, cx| {
            state.cancel_task_confirm();
            state.cancel_workflow_confirm();
            if state.active_route() == Route::Schedules {
                state.cancel_schedule_confirm();
            }
            state.open_repo_picker();
            cx.notify();
        });
        window.focus(&self.repo_picker_focus);
        cx.notify();
    }

    fn close_repo_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.close_repo_picker();
            cx.notify();
        });
        window.focus(&self.repo_button_focus);
        cx.notify();
    }

    fn move_repo_picker(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.move_repo_picker_selection(delta);
            cx.notify();
        });
    }

    fn accept_repo_picker(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.activate_repo_selected();
            cx.notify();
        });
    }

    /// Escape closes the topmost overlay; with no overlay it clears the Tasks
    /// search, then acts as Back out of a pushed detail page (docs §1).
    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_open() {
            self.close_palette(window, cx);
        } else if self.shortcuts_open {
            self.shortcuts_open = false;
            window.focus(&self.root_focus);
            cx.notify();
        } else if self.state.read(cx).repo_picker_open() {
            self.close_repo_picker(window, cx);
        } else {
            self.state.update(cx, |state, cx| {
                if state.task_confirm().is_some() {
                    state.cancel_task_confirm();
                } else if state.workflow_confirm().is_some() {
                    state.cancel_workflow_confirm();
                } else if state.active_route() == Route::Schedules
                    && state.schedule_confirm().is_some()
                {
                    state.cancel_schedule_confirm();
                } else if state.active_route() == Route::Tasks && state.clear_tasks_search() {
                    // Esc is consumed by the search field; a second Esc steps
                    // back from a pushed detail.
                } else if state.active_route() == Route::Catalog && state.close_catalog_overlay() {
                    // The launch sheet, then the one-shot sheet, then the
                    // search field consume Esc in that order.
                } else if state.active_route() == Route::Schedules && state.schedule_editor_open() {
                    state.close_schedule_editor();
                } else if state.active_route() == Route::Settings && state.settings_blur() {
                    // Esc leaves the focused connection field first.
                } else {
                    state.close_detail();
                }
                cx.notify();
            });
            window.focus(&self.root_focus);
        }
    }

    fn on_jump(&mut self, action: &dyn gpui::Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(route) = actions::route_for_action(action) {
            self.navigate(route, cx);
            window.focus(&self.root_focus);
        }
    }

    fn move_palette(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.palette.move_selection(delta);
        cx.notify();
    }

    /// Move the active list highlight. The shell keeps focus, so the list stays
    /// one keystroke away; route guards keep Inbox/Tasks independent.
    fn move_list(&mut self, delta: isize, cx: &mut Context<Self>) {
        let route = self.state.read(cx).active_route();
        match route {
            Route::Inbox => self.state.update(cx, |state, cx| {
                state.move_inbox_selection(delta);
                cx.notify();
            }),
            Route::Tasks => self.state.update(cx, |state, cx| {
                state.move_tasks_selection(delta);
                cx.notify();
            }),
            Route::Agent => self.state.update(cx, |state, cx| {
                if state.standalone_agent().session_id.is_none() {
                    state.cycle_standalone_agent(delta);
                }
                cx.notify();
            }),
            Route::Catalog => self.state.update(cx, |state, cx| {
                if state.catalog().launch().is_some() {
                    state.cycle_catalog_form_focus(delta);
                } else if state
                    .catalog()
                    .oneshot()
                    .is_some_and(|form| form.focused == OneShotFocus::Agent)
                {
                    state.cycle_oneshot_agent(delta);
                } else if state.catalog().oneshot().is_none() {
                    state.move_catalog_selection(delta);
                }
                cx.notify();
            }),
            Route::Workflows => self.state.update(cx, |state, cx| {
                state.move_workflow_selection(delta);
                cx.notify();
            }),
            Route::Schedules => self.state.update(cx, |state, cx| {
                if state.schedule_editor_open() {
                    // The sheet owns ↑/↓ while its task picker is focused;
                    // otherwise the keys do not move the table behind the modal.
                    if state.schedule_focus() == Some(ScheduleField::Task) {
                        state.cycle_schedule_task(delta);
                    }
                } else {
                    state.move_schedule_selection(delta);
                }
                cx.notify();
            }),
            Route::Settings => {}
        }
    }

    /// Home/End/PageUp/PageDown on the active list.
    fn jump_list(&mut self, jump: ListJump, cx: &mut Context<Self>) {
        let route = self.state.read(cx).active_route();
        if route == Route::Workflows {
            self.state.update(cx, |state, cx| {
                match jump {
                    ListJump::Home => state.workflow_selection_edge(false),
                    ListJump::End => state.workflow_selection_edge(true),
                    ListJump::PageUp => state.workflow_selection_page(-1),
                    ListJump::PageDown => state.workflow_selection_page(1),
                }
                cx.notify();
            });
            return;
        }
        if route == Route::Schedules {
            self.state.update(cx, |state, cx| {
                match jump {
                    ListJump::Home | ListJump::PageUp => state.schedule_selection_edge(false),
                    ListJump::End | ListJump::PageDown => state.schedule_selection_edge(true),
                }
                cx.notify();
            });
            return;
        }
        if route != Route::Tasks {
            return;
        }
        self.state.update(cx, |state, cx| {
            match jump {
                ListJump::Home => state.tasks_home(),
                ListJump::End => state.tasks_end(),
                ListJump::PageUp => state.tasks_page(-1),
                ListJump::PageDown => state.tasks_page(1),
            }
            cx.notify();
        });
    }

    /// Open the highlighted row's detail (step 06 fills the detail).
    fn open_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Tab focus can land on a real button. Enter then belongs to that
        // button (GPUI activates it on key-up), not to the list underneath.
        if window.focused(cx).as_ref() != Some(&self.root_focus) {
            return;
        }
        let route = self.state.read(cx).active_route();
        match route {
            Route::Inbox => self.state.update(cx, |state, cx| {
                state.open_inbox_selected();
                cx.notify();
            }),
            Route::Tasks => self.state.update(cx, |state, cx| {
                state.open_tasks_selected();
                cx.notify();
            }),
            Route::Agent => self.state.update(cx, |state, cx| {
                state.start_standalone_agent(cx);
                cx.notify();
            }),
            Route::Catalog => self.state.update(cx, |state, cx| {
                state.open_catalog_selected();
                cx.notify();
            }),
            Route::Workflows => self.state.update(cx, |state, cx| {
                state.open_workflow_selected_in_place();
                cx.notify();
            }),
            Route::Schedules => self.state.update(cx, |state, cx| {
                state.open_schedule_selected(cx);
                cx.notify();
            }),
            Route::Settings => {}
        }
    }

    /// `[` / `]` on the Tasks segmented control.
    fn cycle_task_filter(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.cycle_tasks_filter(delta);
            cx.notify();
        });
    }

    /// `c` / `r`: open the confirmation, explaining when the action cannot
    /// apply (a live run cannot be retried, a finished one cannot be cancelled).
    fn request_task_action(&mut self, action: TaskCommand, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.request_task_action(action, cx);
            cx.notify();
        });
    }

    /// Enter/Space on the confirmation: apply optimistically and reconcile.
    fn accept_task_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The Back button owns Enter while it is focused; otherwise the shell's
        // confirm action would contradict the visible focus.
        if window.focused(cx).as_ref() == Some(&self.confirm_back_focus) {
            return;
        }
        // The confirmation is about to close (or has been rejected); return the
        // keyboard to the shell so Tab starts from a predictable place.
        window.focus(&self.root_focus);
        let route = self.state.read(cx).active_route();
        self.state.update(cx, |state, cx| {
            if route == Route::Workflows {
                state.accept_workflow_confirm(cx);
            } else if route == Route::Schedules {
                state.accept_schedule_confirm(cx);
            } else {
                state.accept_task_confirm(cx);
            }
            cx.notify();
        });
    }

    /// Esc on the confirmation.
    fn dismiss_task_confirm(&mut self, cx: &mut Context<Self>) {
        let route = self.state.read(cx).active_route();
        self.state.update(cx, |state, cx| {
            if route == Route::Workflows {
                state.cancel_workflow_confirm();
            } else if route == Route::Schedules {
                state.cancel_schedule_confirm();
            } else {
                state.cancel_task_confirm();
            }
            cx.notify();
        });
    }

    fn focus_task_search(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.activate_tasks_search();
            cx.notify();
        });
    }

    fn focus_catalog_search(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.activate_catalog_search();
            cx.notify();
        });
    }

    fn open_oneshot(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.open_oneshot();
            cx.notify();
        });
    }

    /// The sheet's submit chord. Validation stays inline; the action is a
    /// no-op when no sheet is open.
    fn submit_catalog_form(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            if state.catalog().launch().is_some() {
                state.submit_catalog_launch();
            } else if state.catalog().oneshot().is_some() {
                state.submit_oneshot();
            }
            cx.notify();
        });
    }

    fn cycle_catalog_form_focus(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.cycle_catalog_form_focus(delta);
            cx.notify();
        });
    }

    fn new_schedule(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.open_schedule_new();
            cx.notify();
        });
    }

    fn edit_schedule(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.open_schedule_selected(cx);
            cx.notify();
        });
    }

    fn delete_schedule(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.request_schedule_delete_selected(cx);
            cx.notify();
        });
    }

    fn cycle_schedule_focus(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.cycle_schedule_focus(delta);
            cx.notify();
        });
    }

    fn submit_schedule(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.submit_schedule();
            cx.notify();
        });
    }

    fn toggle_schedule_enabled(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.toggle_schedule_enabled();
            cx.notify();
        });
    }

    fn cycle_settings_focus(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.settings_cycle_focus(delta);
            cx.notify();
        });
    }

    fn activate_settings(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.settings_activate(cx);
            cx.notify();
        });
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.save_connection_settings(cx);
            cx.notify();
        });
    }

    fn test_settings(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.test_connection(cx);
            cx.notify();
        });
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
                    state.refresh_active();
                    cx.notify();
                });
            }
            CommandKind::SwitchRepo => {
                self.open_repo_picker(window, cx);
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
            Route::Agent => self.views.agent.clone().into_any_element(),
            Route::Catalog => self.views.catalog.clone().into_any_element(),
            Route::Workflows => self.views.workflows.clone().into_any_element(),
            Route::Schedules => self.views.schedules.clone().into_any_element(),
            Route::Settings => self.views.settings.clone().into_any_element(),
        }
    }

    fn top_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let (repo, connection) = {
            let state = self.state.read(cx);
            (state.repo_label(), state.connection().clone())
        };

        let repo_button = components::with_focus(
            div()
                .id("switch-repo")
                .h(px(32.0))
                .px(px(theme.spacing.s2))
                .flex()
                .items_center()
                .gap(px(theme.spacing.s1))
                .rounded(px(theme.radii.r4))
                .bg(theme.colors.surface)
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.text)
                .cursor_pointer()
                .track_focus(&self.repo_button_focus)
                .child(div().min_w_0().truncate().child(repo))
                .child(div().text_color(theme.colors.muted).child("▾"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.open_repo_picker(window, cx);
                })),
            theme,
        );

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
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.navigate(Route::Settings, cx);
                    window.focus(&this.root_focus);
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
                    .child(repo_button),
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
        let needs = self.state.read(cx).needs_count();
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
                    .map(|route| self.nav_entry(*route, needs, theme, cx)),
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

    fn nav_entry(
        &self,
        route: Route,
        needs: u32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let active = self.state.read(cx).active_route() == route;
        let badge = (route == Route::Inbox && needs > 0).then(|| {
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
                .child(needs.to_string())
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
                .on_click(cx.listener(move |this, _event, window, cx| {
                    this.navigate(route, cx);
                    window.focus(&this.root_focus);
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
        let connected = connection.is_connected();
        let status_color = match connection {
            crate::state::ConnectionState::Unauthorized { .. }
            | crate::state::ConnectionState::Disconnected { .. } => theme.colors.danger,
            _ => theme.colors.warning,
        };
        let status_label = connection.label();

        let left = if connected {
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
                })
        } else {
            // The ping/endpoint are noise while disconnected; the state label
            // already names the fix (including the token path).
            div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .text_color(status_color)
                        .child(status_label),
                )
                .child("·")
                .child(div().font_family("monospace").truncate().child(endpoint))
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
            .child(left)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.s2))
                    .child(cursor_label)
                    .child(shortcuts)
                    .child(reconnect),
            )
    }

    /// Which buffer owns the keyboard/caret. Modals and the palette come first,
    /// matching the old typed-character routing.
    fn active_text_target(&self, cx: &gpui::App) -> Option<TextTarget> {
        if self.shortcuts_open {
            return None;
        }
        if self.palette.is_open() {
            return Some(TextTarget::Palette);
        }
        self.state.read(cx).active_text_target()
    }

    fn text_is_multiline(&self, target: TextTarget, cx: &gpui::App) -> bool {
        match target {
            TextTarget::Palette => false,
            _ => self.state.read(cx).text_target_is_multiline(target),
        }
    }

    fn edit_active_text(&mut self, command: TextCommand, cx: &mut Context<Self>) {
        let Some(target) = self.active_text_target(cx) else {
            return;
        };
        let multiline = self.text_is_multiline(target, cx);
        if target == TextTarget::Palette {
            if apply_text_command(self.palette.input_mut(), command, multiline) {
                self.palette.after_edit();
                cx.notify();
            }
        } else {
            self.state.update(cx, |state, cx| {
                let changed = state
                    .text_input_mut(target)
                    .is_some_and(|input| apply_text_command(input, command, multiline));
                if changed {
                    state.after_text_edit(target);
                    cx.notify();
                }
            });
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn copy_active_text(&mut self, cut: bool, cx: &mut Context<Self>) {
        let Some(target) = self.active_text_target(cx) else {
            return;
        };
        let selected = if target == TextTarget::Palette {
            let input = self.palette.input();
            input
                .has_selection()
                .then(|| input.selection_text().to_string())
        } else {
            self.state.read(cx).text_input(target).and_then(|input| {
                input
                    .has_selection()
                    .then(|| input.selection_text().to_string())
            })
        };
        let Some(selected) = selected else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(selected));
        if cut {
            if target == TextTarget::Palette {
                self.palette.input_mut().delete_selection();
                self.palette.after_edit();
            } else {
                self.state.update(cx, |state, cx| {
                    if let Some(input) = state.text_input_mut(target) {
                        input.delete_selection();
                    }
                    state.after_text_edit(target);
                    cx.notify();
                });
            }
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn paste_active_text(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.active_text_target(cx) else {
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let multiline = self.text_is_multiline(target, cx);
        let text = if multiline {
            text
        } else {
            text.replace(['\n', '\r'], " ")
        };
        if text.is_empty() {
            return;
        }
        if target == TextTarget::Palette {
            self.palette.input_mut().insert(&text);
            self.palette.after_edit();
        } else {
            self.state.update(cx, |state, cx| {
                if let Some(input) = state.text_input_mut(target) {
                    input.insert(&text);
                }
                state.after_text_edit(target);
                cx.notify();
            });
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn insert_active_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let mut target = self.active_text_target(cx);
        if target.is_none() {
            // The one-shot picker keeps the agent chip focused until the user
            // types; typing has always edited the cwd and moved focus there.
            self.state.update(cx, |state, cx| {
                if state
                    .catalog()
                    .oneshot()
                    .is_some_and(|form| form.focused == OneShotFocus::Agent)
                {
                    state.focus_oneshot(OneShotFocus::Cwd);
                    target = Some(TextTarget::CatalogOneShotCwd);
                    cx.notify();
                }
            });
        }
        let Some(target) = target else {
            return;
        };
        let multiline = self.text_is_multiline(target, cx);
        let text: String = if multiline {
            text.chars()
                .filter(|character| !character.is_control() || *character == '\n')
                .collect()
        } else {
            text.chars()
                .filter(|character| !character.is_control())
                .collect()
        };
        if text.is_empty() {
            return;
        }
        if target == TextTarget::Palette {
            self.palette.input_mut().insert(&text);
            self.palette.after_edit();
        } else {
            self.state.update(cx, |state, cx| {
                if let Some(input) = state.text_input_mut(target) {
                    input.insert(&text);
                }
                state.after_text_edit(target);
                cx.notify();
            });
        }
        cx.notify();
        cx.stop_propagation();
    }

    fn palette_overlay(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
        entity: Entity<Self>,
    ) -> Div {
        let focused = self.palette_focus.is_focused(window);
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
                    .text_size(px(theme.font.base))
                    .child(
                        TextElement::new(
                            entity,
                            TextTarget::Palette,
                            self.palette_focus.clone(),
                            theme,
                        )
                        .placeholder("Type a command…"),
                    ),
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

    fn shortcuts_overlay(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> Div {
        let viewport = window.viewport_size();
        let layout = shortcut_sheet_layout(f32::from(viewport.width), f32::from(viewport.height));

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

        let commands = [("Command palette", "⌘K")];
        let navigation: [(&'static str, &'static str); 7] =
            Route::ALL.map(|route| (route.label(), route.shortcut()));
        let anywhere = [
            ("Shortcut sheet", "?"),
            ("Close overlay / back", "Esc"),
            ("Move palette selection", "↑ ↓"),
            ("Run palette command", "↵"),
            ("Move focus", "Tab"),
        ];
        let tasks = [
            ("Move the list", "↑ ↓"),
            ("First / last", "Home End"),
            ("Page up / down", "PgUp PgDn"),
            ("Open the run", "↵"),
            ("Search task names", "/"),
            ("Cancel a run", "c"),
            ("Retry a finished run", "r"),
            ("Dismiss the spawn failure", "d"),
            ("Previous / next filter", "[ ]"),
            ("Confirm a dialog", "↵"),
        ];
        let catalog = [
            ("Move the definition", "↑ ↓"),
            ("Open the launch sheet", "↵"),
            ("Search definitions", "/"),
            ("Collapse the folder", "h"),
            ("Test connection", "⌘T"),
            ("Save settings", "⌘↵"),
        ];
        let workflows = [
            ("Move the instance", "↑ ↓"),
            ("Previous / next root", "[ ]"),
            ("Open the run", "↵"),
            ("Cancel the root", "c"),
            ("Retry a failed instance", "r"),
        ];
        let schedules = [
            ("New schedule", "n"),
            ("Edit the row", "↵"),
            ("Delete the row", "d"),
            ("Next / previous field", "tab"),
            ("Toggle enabled", "⌘E"),
            ("Save schedule", "⌘↵"),
        ];
        let agent = [
            ("New session", "n"),
            ("Interrupt the turn", "i"),
            ("Close the session", "x"),
            ("Send the prompt", "↵"),
            ("Complete a slash command", "tab"),
        ];
        let detail = [
            ("Send the prompt", "↵"),
            ("Complete a slash command", "tab"),
            ("Select a command", "↑ ↓"),
            ("Leave the composer", "esc"),
        ];
        let text_fields = [
            ("Move by character", "← →"),
            ("Move by word", "⌥← ⌥→"),
            ("Line start / end", "Home End"),
            ("Extend the selection", "⇧← ⇧→"),
            ("Delete a word", "⌥⌫"),
            ("Copy / cut / paste", "⌘C ⌘X ⌘V"),
            ("Select all", "⌘A"),
            ("Newline (multiline)", "⇧↵"),
        ];

        let body = div()
            .id("shortcut-sheet-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(theme.spacing.s3))
            .child(shortcut_group(
                theme,
                layout.columns,
                shortcut_section(theme, "COMMANDS", &commands),
                shortcut_section(theme, "NAVIGATION", &navigation),
            ))
            .child(shortcut_group(
                theme,
                layout.columns,
                shortcut_section(theme, "TASKS", &tasks),
                shortcut_section(theme, "CATALOG", &catalog),
            ))
            .child(shortcut_group(
                theme,
                layout.columns,
                shortcut_section(theme, "WORKFLOWS", &workflows),
                shortcut_section(theme, "SCHEDULES", &schedules),
            ))
            .child(shortcut_group(
                theme,
                layout.columns,
                shortcut_section(theme, "AGENT", &agent),
                shortcut_section(theme, "DETAIL", &detail),
            ))
            .child(shortcut_section(theme, "ANYWHERE", &anywhere))
            .child(shortcut_section(theme, "TEXT FIELDS", &text_fields));

        let panel = div()
            .w(px(layout.width))
            .max_h(px(layout.max_height))
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .gap(px(theme.spacing.s3))
            .p(px(theme.spacing.s4))
            .rounded(px(theme.radii.r12))
            .border_1()
            .border_color(theme.colors.border)
            .bg(theme.colors.surface)
            .shadow_lg()
            .child(
                div()
                    .flex_none()
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
            .child(body);

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
    /// The cancel/retry confirmation. It names the task and spells out the
    /// consequence; the destructive action is the only danger-colored control
    /// on the surface (docs §2, Von Restorff).
    fn confirm_overlay(&self, theme: &Theme, confirm: &TaskConfirm, cx: &mut Context<Self>) -> Div {
        let (primary_bg, primary_fg) = if confirm.danger() {
            (
                Hsla::from(theme.colors.danger).opacity(0.16),
                theme.colors.danger,
            )
        } else {
            (
                Hsla::from(theme.colors.accent).opacity(0.16),
                theme.colors.accent,
            )
        };

        let back = components::with_focus(
            div()
                .id("task-confirm-back")
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
                .track_focus(&self.confirm_back_focus)
                .child("Back")
                .child(components::keycap(theme, "esc"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.dismiss_task_confirm(cx);
                    window.focus(&this.root_focus);
                })),
            theme,
        );

        let accept = components::with_focus(
            div()
                .id("task-confirm-accept")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(40.0))
                .px(px(theme.spacing.s4))
                .rounded(px(theme.radii.r8))
                .bg(primary_bg)
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(primary_fg)
                .cursor_pointer()
                .track_focus(&self.confirm_accept_focus)
                .child(confirm.confirm_label())
                .child(components::keycap(theme, "↵"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.accept_task_confirm(window, cx);
                })),
            theme,
        );

        let panel = div()
            .w(px(440.0))
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
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child(confirm.title()),
                    )
                    .child(components::keycap(theme, "esc")),
            )
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(confirm.body()),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(
                        "The row updates immediately; if the daemon rejects the call it is rolled back.",
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(theme.spacing.s2))
                    .pt(px(theme.spacing.s1))
                    .child(backend_hint(theme))
                    .child(back)
                    .child(accept),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(Hsla::from(theme.colors.bg).opacity(0.86))
            .key_context("Confirm")
            .child(panel)
    }

    /// The root-cancel confirmation. It names the root and the exact number of
    /// non-terminal instances the RPC will transition (docs §2, Von Restorff:
    /// it is the only danger control on the surface).
    fn workflow_confirm_overlay(
        &self,
        theme: &Theme,
        confirm: &WorkflowCancelConfirm,
        cx: &mut Context<Self>,
    ) -> Div {
        let back = components::with_focus(
            div()
                .id("workflow-confirm-back")
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
                .track_focus(&self.confirm_back_focus)
                .child("Back")
                .child(components::keycap(theme, "esc"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.dismiss_task_confirm(cx);
                    window.focus(&this.root_focus);
                })),
            theme,
        );

        let danger = confirm.danger();
        let accept_color = if danger {
            theme.colors.danger
        } else {
            theme.colors.accent
        };
        let accept = components::with_focus(
            div()
                .id("workflow-confirm-accept")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(40.0))
                .px(px(theme.spacing.s4))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(accept_color).opacity(0.16))
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(accept_color)
                .cursor_pointer()
                .track_focus(&self.confirm_accept_focus)
                .child(confirm.confirm_label())
                .child(components::keycap(theme, "↵"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.accept_task_confirm(window, cx);
                })),
            theme,
        );

        let panel = div()
            .w(px(460.0))
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
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child(confirm.title()),
                    )
                    .child(components::keycap(theme, "esc")),
            )
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(confirm.body()),
            )
            .child(
                div()
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(
                        "Every non-terminal row updates immediately; if the daemon rejects the call it is rolled back.",
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(theme.spacing.s2))
                    .pt(px(theme.spacing.s1))
                    .child(backend_hint(theme))
                    .child(back)
                    .child(accept),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(Hsla::from(theme.colors.bg).opacity(0.86))
            .key_context("Confirm")
            .child(panel)
    }

    /// The schedule-delete confirmation: names the task and cadence and keeps
    /// the daemon's rejection attached to the dialog.
    fn schedule_confirm_overlay(
        &self,
        theme: &Theme,
        confirm: &crate::schedules::ScheduleDeleteConfirm,
        cx: &mut Context<Self>,
    ) -> Div {
        let back = components::with_focus(
            div()
                .id("schedule-confirm-back")
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
                .track_focus(&self.confirm_back_focus)
                .child("Back")
                .child(components::keycap(theme, "esc"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.dismiss_task_confirm(cx);
                    window.focus(&this.root_focus);
                })),
            theme,
        );

        let accept_color = if confirm.danger() {
            theme.colors.danger
        } else {
            theme.colors.accent
        };
        let accept = components::with_focus(
            div()
                .id("schedule-confirm-accept")
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .h(px(40.0))
                .px(px(theme.spacing.s4))
                .rounded(px(theme.radii.r8))
                .bg(Hsla::from(accept_color).opacity(0.16))
                .text_size(px(theme.font.sm))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(accept_color)
                .cursor_pointer()
                .track_focus(&self.confirm_accept_focus)
                .child(confirm.confirm_label())
                .child(components::keycap(theme, "↵"))
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.accept_task_confirm(window, cx);
                })),
            theme,
        );

        let error = confirm.error.clone().map(|message| {
            let copy_text = message.clone();
            let copy = components::copy_button(
                theme,
                "schedule-confirm-error-copy",
                "Copy error",
                &self.confirm_copy_focus,
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.copy_text(copy_text.clone(), "the error", cx);
                    cx.notify();
                });
            }));
            div()
                .flex()
                .items_center()
                .gap(px(theme.spacing.s2))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(theme.font.xs))
                        .text_color(theme.colors.danger)
                        .child(message),
                )
                .child(copy)
        });

        let panel = div()
            .w(px(460.0))
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
                    .gap(px(theme.spacing.s2))
                    .child(
                        div()
                            .text_size(px(theme.font.lg))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.colors.text)
                            .child(confirm.title()),
                    )
                    .child(components::keycap(theme, "esc")),
            )
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(confirm.body()),
            )
            .when_some(error, |panel, error| panel.child(error))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(theme.spacing.s2))
                    .pt(px(theme.spacing.s1))
                    .child(backend_hint(theme))
                    .child(back)
                    .child(accept),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(Hsla::from(theme.colors.bg).opacity(0.86))
            .key_context("Confirm")
            .child(panel)
    }

    /// The searchable repository switcher, modelled on the TUI Ctrl+P picker.
    /// The global row is always first; unavailable
    /// checkouts stay visible but inert and marked `(missing)`.
    fn repo_overlay(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
        entity: Entity<Self>,
    ) -> Div {
        let (entries, selected, error, submitting) = {
            let state = self.state.read(cx);
            let picker = state.repo_picker();
            (
                state.repo_picker_entries(),
                picker.selected(),
                picker.error().map(str::to_string),
                picker.is_submitting(),
            )
        };
        let focused = self.repo_picker_focus.is_focused(window);
        let no_matches =
            !self.state.read(cx).repo_picker().filter().trim().is_empty() && entries.len() == 1;

        let input = div()
            .id("repo-picker-input")
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
            .track_focus(&self.repo_picker_focus)
            .cursor_text()
            .on_click(cx.listener(|this, _event, window, _cx| {
                window.focus(&this.repo_picker_focus);
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
                    .text_size(px(theme.font.base))
                    .child(
                        TextElement::new(
                            entity,
                            TextTarget::RepoPicker,
                            self.repo_picker_focus.clone(),
                            theme,
                        )
                        .placeholder("Search repositories by name or path…"),
                    ),
            )
            .child(components::keycap(theme, "esc"));

        let rows: Vec<AnyElement> = entries
            .into_iter()
            .enumerate()
            .map(|(index, option)| {
                let is_selected = index == selected;
                let RepoOption {
                    name,
                    label,
                    selectable,
                    active,
                } = option;
                let mut row = div()
                    .id(("repo-picker-row", index))
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
                    .text_size(px(theme.font.sm))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(if selectable {
                                theme.colors.text
                            } else {
                                theme.colors.muted
                            })
                            .child(label),
                    )
                    .when(active, |row| {
                        row.child(
                            div()
                                .text_size(px(theme.font.xs))
                                .text_color(theme.colors.accent)
                                .child("active"),
                        )
                    });
                if !selectable {
                    row = row.opacity(0.6);
                } else if !submitting {
                    row = row
                        .cursor_pointer()
                        .when(!is_selected, |row| {
                            row.hover(|style| style.bg(theme.colors.bg))
                        })
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.state.update(cx, |state, cx| {
                                state.select_repo_picker_row(index);
                                state.activate_repo(name.clone());
                                cx.notify();
                            });
                        }));
                }
                row.into_any_element()
            })
            .collect();

        let empty = no_matches.then(|| {
            div()
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s2))
                .text_size(px(theme.font.sm))
                .text_color(theme.colors.muted)
                .child("No repositories match.")
        });

        let error = error.map(|message| {
            div()
                .px(px(theme.spacing.s3))
                .py(px(theme.spacing.s2))
                .rounded(px(theme.radii.r4))
                .bg(Hsla::from(theme.colors.danger).opacity(0.12))
                .text_size(px(theme.font.xs))
                .text_color(theme.colors.danger)
                .child(message)
        });

        let footer_hint = if submitting {
            "Switching…".to_string()
        } else {
            "↑↓ move · ↵ switch · esc close".to_string()
        };

        let panel = div()
            .w(px(520.0))
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
                    .when_some(empty, |list, empty| list.child(empty))
                    .children(rows),
            )
            .when_some(error, |panel, error| {
                panel.child(
                    div()
                        .px(px(theme.spacing.s2))
                        .pb(px(theme.spacing.s2))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(theme.spacing.s3))
                    .px(px(theme.spacing.s3))
                    .py(px(theme.spacing.s2))
                    .border_t_1()
                    .border_color(theme.colors.border)
                    .text_size(px(theme.font.xs))
                    .text_color(theme.colors.muted)
                    .child(footer_hint)
                    .child("global clears the active repo"),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(96.0))
            .bg(Hsla::from(theme.colors.bg).opacity(0.86))
            .key_context("RepoPicker")
            .child(panel)
    }

    /// The self-dismissing toast above the status bar.
    fn toast_overlay(&self, theme: &Theme, toast: &Toast, cx: &mut Context<Self>) -> AnyElement {
        let (icon, color) = match toast.kind {
            ToastKind::Info => ("ℹ", theme.colors.accent),
            ToastKind::Success => ("✓", theme.colors.success),
            ToastKind::Error => ("⚠", theme.colors.danger),
        };
        let dismiss = components::with_focus(
            div()
                .id("toast-dismiss")
                .flex()
                .items_center()
                .justify_center()
                .gap(px(theme.spacing.s1))
                .px(px(theme.spacing.s2))
                .h(px(theme.controls.min))
                .rounded(px(theme.radii.r4))
                .text_color(theme.colors.muted)
                .cursor_pointer()
                .track_focus(&self.toast_dismiss_focus)
                .hover(|style| style.bg(theme.colors.bg))
                .child("Dismiss")
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.dismiss_toast();
                        cx.notify();
                    });
                })),
            theme,
        );

        let copy = (toast.kind == ToastKind::Error).then(|| {
            let message = toast.message.clone();
            components::copy_button(theme, "toast-copy", "Copy error", &self.toast_copy_focus)
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.state.update(cx, |state, cx| {
                        state.copy_text(message.clone(), "the error", cx);
                        cx.notify();
                    });
                }))
        });
        let panel = div()
            .absolute()
            .bottom(px(STATUS_BAR_HEIGHT + theme.spacing.s3))
            .right(px(theme.spacing.s4))
            .w(px(380.0))
            .flex()
            .items_start()
            .gap(px(theme.spacing.s2))
            .p(px(theme.spacing.s3))
            .rounded(px(theme.radii.r8))
            .border_1()
            .border_color(Hsla::from(color).opacity(0.5))
            .bg(theme.colors.surface)
            .shadow_lg()
            .child(
                div()
                    .text_size(px(theme.font.sm))
                    .text_color(color)
                    .child(icon),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(theme.font.sm))
                    .text_color(theme.colors.text)
                    .child(toast.message.clone()),
            )
            .when_some(copy, |panel, copy| panel.child(copy))
            .child(dismiss);

        // The toast is the one decorative entrance in the client. Reduced motion
        // removes the fade but keeps the same final state; the icon and message
        // are already present, so nothing is conveyed by the motion alone.
        if crate::motion::prefers_reduced_motion() {
            panel.into_any_element()
        } else {
            panel
                .with_animation(
                    "toast-in",
                    Animation::new(crate::motion::duration(140)),
                    |panel, delta| panel.opacity(delta),
                )
                .into_any_element()
        }
    }

    /// The invisible 8 px hit area on the detail column's left border. Drag
    /// moves are handled by `on_drag_move` so the width keeps following the
    /// pointer after it leaves the handle.
    fn detail_resize_handle(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("detail-resize")
            .absolute()
            .left(px(-(RESIZE_HANDLE / 2.0)))
            .top_0()
            .h_full()
            .w(px(RESIZE_HANDLE))
            .cursor_col_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, _cx| {
                    this.detail_resize = Some(ResizeStart::new(
                        f32::from(event.position.x),
                        this.detail_width,
                    ));
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| {
                    this.detail_resize = None;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| {
                    this.detail_resize = None;
                }),
            )
            .on_drag(ResizeDrag, |_drag, _offset, _window, cx| {
                cx.new(|_| ResizePreview)
            })
            .on_drag_move::<ResizeDrag>(cx.listener(
                |this, event: &gpui::DragMoveEvent<ResizeDrag>, window, cx| {
                    let Some(start) = this.detail_resize else {
                        return;
                    };
                    let pointer_x = f32::from(event.event.position.x);
                    let desired = start.requested_width(pointer_x);
                    let viewport_width = f32::from(window.viewport_size().width);
                    let width = clamp_detail_width(desired, viewport_width);
                    if width != this.detail_width {
                        this.detail_width = width;
                        cx.notify();
                    }
                },
            ))
            .into_any_element()
    }
}

/// A tiny hint in the confirmation footer (kept out of the title row so the
/// task name is the first thing read).
fn backend_hint(theme: &Theme) -> Div {
    div()
        .text_size(px(theme.font.xs))
        .text_color(theme.colors.muted)
        .child("↵ confirm · esc back")
}

/// One column of the shortcut sheet.
fn shortcut_section(theme: &Theme, title: &str, rows: &[(&str, &str)]) -> Div {
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(components::section_label(theme, title))
        .children(
            rows.iter()
                .map(|(label, keys)| components::shortcut_row(theme, label, keys)),
        )
}

/// Place a pair of shortcut sections side by side, or stack them when the
/// sheet is too narrow for two readable columns.
fn shortcut_group(theme: &Theme, columns: usize, left: Div, right: Div) -> Div {
    let group = div().flex().gap(px(theme.spacing.s6));
    if columns == 1 {
        group.flex_col().child(left).child(right)
    } else {
        group
            .items_start()
            .child(div().flex_1().min_w_0().child(left))
            .child(div().flex_1().min_w_0().child(right))
    }
}

impl TextHost for RootView {
    fn text_input(&self, target: TextTarget) -> Option<&TextInput> {
        (target == TextTarget::Palette).then(|| self.palette.input())
    }

    fn text_input_mut(&mut self, target: TextTarget) -> Option<&mut TextInput> {
        (target == TextTarget::Palette).then(|| self.palette.input_mut())
    }

    fn active_text_target(&self) -> Option<TextTarget> {
        (self.palette.is_open() && !self.shortcuts_open).then_some(TextTarget::Palette)
    }

    fn focus_text_target(&mut self, _target: TextTarget) {}

    fn after_text_edit(&mut self, _target: TextTarget) {
        self.palette.after_edit();
    }

    fn text_edit_state(&self) -> &TextEditState {
        &self.palette_text
    }

    fn text_edit_state_mut(&mut self) -> &mut TextEditState {
        &mut self.palette_text
    }
}

impl gpui::EntityInputHandler for RootView {
    fn text_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        actual_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        crate::text_element::text_for_range(self.palette.input(), range_utf16, actual_range)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        Some(crate::text_element::selected_text_range(
            self.palette.input(),
        ))
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        crate::text_element::marked_text_range(self.palette.input())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.palette.input_mut().unmark_text();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::text_element::replace_text_in_range(
            self.palette.input_mut(),
            range_utf16,
            text,
            false,
        );
        self.palette.after_edit();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::text_element::replace_and_mark_text_in_range(
            self.palette.input_mut(),
            range_utf16,
            new_text,
            new_selected_range_utf16,
            false,
        );
        self.palette.after_edit();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _element_bounds: gpui::Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        crate::text_element::bounds_for_range(
            self.palette.input(),
            self.palette_text.layout(TextTarget::Palette),
            range_utf16,
        )
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        crate::text_element::character_index_for_point(
            self.palette.input(),
            self.palette_text.layout(TextTarget::Palette),
            point,
        )
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.state.read(cx).theme(window.appearance());
        let mode = container_query(window.viewport_size().width);
        let route = self.state.read(cx).active_route();
        let viewport_width = f32::from(window.viewport_size().width);
        let detail_width = if mode == LayoutMode::Wide {
            let clamped = clamp_detail_width(self.detail_width, viewport_width);
            self.detail_width = clamped;
            clamped
        } else {
            self.detail_width
        };
        // Only the run lists have a detail page to push or a column to fill.
        let detail_open = self.state.read(cx).is_detail_open() && route.shows_detail();
        let palette_open = self.palette.is_open();
        let shortcuts_open = self.shortcuts_open;
        let repo_picker_open = self.state.read(cx).repo_picker_open();
        // A repos response closes the modal asynchronously once the requested
        // repo is active; return focus once it is gone, unless another overlay
        // already took it.
        if self.repo_picker_was_open
            && !repo_picker_open
            && !palette_open
            && !shortcuts_open
            && window.focused(cx).as_ref() != Some(&self.root_focus)
        {
            window.focus(&self.repo_button_focus);
        }
        self.repo_picker_was_open = repo_picker_open;

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
                .when(
                    route.shows_detail() && (route.always_reserves_detail_column() || detail_open),
                    |body| {
                        body.child(
                            div()
                                .relative()
                                .w(px(detail_width))
                                .flex_shrink_0()
                                .border_l_1()
                                .border_color(theme.colors.border)
                                .child(detail)
                                .child(self.detail_resize_handle(cx)),
                        )
                    },
                ),
        };

        let (
            route,
            confirm_open,
            search_active,
            confirm,
            workflow_confirm,
            schedule_confirm,
            toast,
            catalog_search_active,
            catalog_launch_open,
            catalog_oneshot_open,
            schedule_sheet_open,
            settings_typing,
            detail_prompt_active,
            detail_elicitation_active,
        ) = {
            let state = self.state.read(cx);
            (
                state.active_route(),
                state.task_confirm().is_some(),
                state.is_tasks_search_active(),
                state.task_confirm().cloned(),
                state.workflow_confirm().cloned(),
                state.schedule_confirm().cloned(),
                state.toast().cloned(),
                state.is_catalog_search_active(),
                state.catalog().launch().is_some(),
                state.catalog().oneshot().is_some(),
                state.schedule_editor_open(),
                state.settings().is_editing_text(),
                state.detail_prompt_active(),
                state.detail_elicitation().is_some(),
            )
        };
        let mut key_context = String::from("Root");
        if route == Route::Tasks {
            key_context.push_str(" Tasks");
            if search_active {
                // Typed characters reach the query instead of the g/r/? bindings.
                key_context.push_str(" Search");
            }
            if confirm_open {
                key_context.push_str(" Confirm");
            }
        } else if route == Route::Agent {
            key_context.push_str(" Agent");
            if detail_prompt_active {
                key_context.push_str(" Search");
            }
        } else if route == Route::Workflows {
            key_context.push_str(" Workflows");
            if workflow_confirm.is_some() {
                key_context.push_str(" Confirm");
            }
        } else if route == Route::Catalog {
            key_context.push_str(" Catalog");
            // A focused form owns typed text exactly like the search field; the
            // shared `Search` context keeps `g`-jumps, `r` and `?` from firing
            // while a var or cwd is being typed.
            if catalog_search_active || catalog_launch_open || catalog_oneshot_open {
                key_context.push_str(" Search");
            }
            if catalog_launch_open {
                key_context.push_str(" Launch");
            }
            if catalog_oneshot_open {
                key_context.push_str(" Oneshot");
            }
        } else if route == Route::Schedules {
            key_context.push_str(" Schedules");
            if schedule_confirm.is_some() {
                key_context.push_str(" Confirm");
            }
            if schedule_sheet_open {
                // A sheet owns typed text (the cron expression uses spaces),
                // so single-key shortcuts must not fire while it is open.
                key_context.push_str(" Sheet Search");
            }
        } else if route == Route::Settings {
            key_context.push_str(" Settings");
            if settings_typing {
                // The endpoint/token field owns typed characters; g-jumps and
                // other single-key shortcuts must not fire mid-address.
                key_context.push_str(" Search");
            }
        }
        if repo_picker_open {
            // The modal owns the keyboard; `Search` keeps typed text from
            // triggering the global single-key shortcuts.
            key_context.push_str(" RepoPicker Search");
        }
        if detail_prompt_active {
            // The composer owns typed text and key navigation; keep the generic
            // `Search` suppression so single-key jumps never fire mid-prompt.
            key_context.push_str(" DetailPrompt Search");
        }
        if detail_elicitation_active {
            // The form modal owns typed text and every navigation key.
            key_context.push_str(" Elicitation Search");
        }

        // The text-editing bindings are registered last in `actions.rs`; the
        // `TextField`/`Multiline` contexts make which surface wins explicit.
        let text_target = if self.shortcuts_open {
            None
        } else if self.palette.is_open() {
            Some(TextTarget::Palette)
        } else {
            self.state.read(cx).active_text_target()
        };
        if text_target.is_some() {
            key_context.push_str(" TextField");
        }
        if text_target.is_some_and(|target| {
            target != TextTarget::Palette && self.state.read(cx).text_target_is_multiline(target)
        }) {
            key_context.push_str(" Multiline");
        }

        let root = shell_root()
            .bg(theme.colors.bg)
            .text_color(theme.colors.text)
            .text_size(px(theme.font.base))
            .key_context(key_context.as_str())
            .track_focus(&self.root_focus)
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                if this.palette.is_open() {
                    this.close_palette(window, cx);
                } else {
                    this.open_palette(window, cx);
                }
            }))
            .on_action(cx.listener(|_this, _: &FocusNext, window, cx| {
                window.focus_next();
                cx.notify();
            }))
            .on_action(cx.listener(|_this, _: &FocusPrev, window, cx| {
                window.focus_prev();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseOverlay, window, cx| {
                if this.state.read(cx).detail_prompt_active() {
                    this.state.update(cx, |state, cx| {
                        state.blur_detail_prompt();
                        cx.notify();
                    });
                    window.focus(&this.root_focus);
                    cx.stop_propagation();
                    return;
                }
                this.close_overlay(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleShortcuts, window, cx| {
                this.toggle_shortcuts(window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoInbox, window, cx| {
                this.on_jump(action, window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoTasks, window, cx| {
                this.on_jump(action, window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoAgent, window, cx| {
                this.on_jump(action, window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoCatalog, window, cx| {
                this.on_jump(action, window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoWorkflows, window, cx| {
                this.on_jump(action, window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoSchedules, window, cx| {
                this.on_jump(action, window, cx);
            }))
            .on_action(cx.listener(|this, action: &GoSettings, window, cx| {
                this.on_jump(action, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ListUp, _window, cx| {
                this.move_list(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &ListDown, _window, cx| {
                this.move_list(1, cx);
            }))
            .on_action(cx.listener(|this, _: &ListHome, _window, cx| {
                this.jump_list(ListJump::Home, cx);
            }))
            .on_action(cx.listener(|this, _: &ListEnd, _window, cx| {
                this.jump_list(ListJump::End, cx);
            }))
            .on_action(cx.listener(|this, _: &ListPageUp, _window, cx| {
                this.jump_list(ListJump::PageUp, cx);
            }))
            .on_action(cx.listener(|this, _: &ListPageDown, _window, cx| {
                this.jump_list(ListJump::PageDown, cx);
            }))
            .on_action(cx.listener(|this, _: &ListOpen, window, cx| {
                this.open_list(window, cx);
            }))
            .on_action(cx.listener(|this, _: &TaskSearch, _window, cx| {
                this.focus_task_search(cx);
            }))
            .on_action(cx.listener(|this, _: &CatalogSearch, _window, cx| {
                this.focus_catalog_search(cx);
            }))
            .on_action(cx.listener(|this, _: &CatalogNewOneshot, _window, cx| {
                this.open_oneshot(cx);
            }))
            .on_action(cx.listener(|this, _: &CatalogLaunchSubmit, _window, cx| {
                this.submit_catalog_form(cx);
            }))
            .on_action(cx.listener(|this, _: &CatalogFieldNext, _window, cx| {
                this.cycle_catalog_form_focus(1, cx);
            }))
            .on_action(cx.listener(|this, _: &CatalogFieldPrev, _window, cx| {
                this.cycle_catalog_form_focus(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &CatalogChoicePrev, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.cycle_catalog_choice(-1);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &CatalogChoiceNext, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.cycle_catalog_choice(1);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ScheduleNew, _window, cx| {
                this.new_schedule(cx);
            }))
            .on_action(cx.listener(|this, _: &ScheduleEdit, _window, cx| {
                this.edit_schedule(cx);
            }))
            .on_action(cx.listener(|this, _: &ScheduleDelete, _window, cx| {
                this.delete_schedule(cx);
            }))
            .on_action(cx.listener(|this, _: &ScheduleFieldNext, _window, cx| {
                this.cycle_schedule_focus(1, cx);
            }))
            .on_action(cx.listener(|this, _: &ScheduleFieldPrev, _window, cx| {
                this.cycle_schedule_focus(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &ScheduleSubmit, _window, cx| {
                this.submit_schedule(cx);
            }))
            .on_action(cx.listener(|this, _: &ScheduleToggleEnabled, _window, cx| {
                this.toggle_schedule_enabled(cx);
            }))
            .on_action(cx.listener(|this, _: &SettingsFieldNext, _window, cx| {
                this.cycle_settings_focus(1, cx);
            }))
            .on_action(cx.listener(|this, _: &SettingsFieldPrev, _window, cx| {
                this.cycle_settings_focus(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &SettingsActivate, _window, cx| {
                this.activate_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &SettingsSave, _window, cx| {
                this.save_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &SettingsTest, _window, cx| {
                this.test_settings(cx);
            }))
            .on_action(cx.listener(|this, _: &TaskCancel, _window, cx| {
                this.request_task_action(TaskCommand::Cancel, cx);
            }))
            .on_action(cx.listener(|this, _: &TaskRetry, _window, cx| {
                this.request_task_action(TaskCommand::Retry, cx);
            }))
            .on_action(cx.listener(|this, _: &TaskDismissLaunch, _window, cx| {
                this.state.update(cx, |state, cx| {
                    let failures = state.launch_failures();
                    if state.dismiss_launch_failures(&failures) {
                        cx.notify();
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &WorkflowCancelRoot, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.request_workflow_cancel(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &WorkflowRetryInstance, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.retry_workflow_selected(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &WorkflowOpenInTasks, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.open_workflow_selected();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &WorkflowRootPrev, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.move_workflow_root(-1);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &WorkflowRootNext, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.move_workflow_root(1);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &CatalogToggleFolder, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.toggle_catalog_selected_folder();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &TaskFilterPrev, _window, cx| {
                this.cycle_task_filter(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &TaskFilterNext, _window, cx| {
                this.cycle_task_filter(1, cx);
            }))
            .on_action(cx.listener(|this, _: &TaskConfirmAccept, window, cx| {
                this.accept_task_confirm(window, cx);
            }))
            .on_action(cx.listener(|this, _: &TaskConfirmReject, _window, cx| {
                this.dismiss_task_confirm(cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.refresh_active();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &AgentNewSession, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.start_standalone_agent(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &AgentInterrupt, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.interrupt_detail_turn(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &AgentClose, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.close_standalone_agent(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &DetailPromptSend, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.send_detail_prompt(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &DetailPromptComplete, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.detail_prompt_complete_command();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &DetailPromptNext, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.detail_prompt_move_command(1);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &DetailPromptPrev, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.detail_prompt_move_command(-1);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationNext, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_next();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationPrev, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_prev();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationLeft, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_adjust(true);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationRight, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_adjust(false);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationToggle, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_toggle();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationConfirm, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_confirm(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationDecline, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_decline(cx);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationEdit, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_edit();
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &ElicitationCancel, _window, cx| {
                this.state.update(cx, |state, cx| {
                    state.elicitation_cancel(cx);
                    cx.notify();
                });
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
            .on_action(cx.listener(|this, _: &RepoPickerUp, _window, cx| {
                this.move_repo_picker(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &RepoPickerDown, _window, cx| {
                this.move_repo_picker(1, cx);
            }))
            .on_action(cx.listener(|this, _: &RepoPickerConfirm, _window, cx| {
                this.accept_repo_picker(cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveLeft, _window, cx| {
                this.edit_active_text(TextCommand::MoveLeft, cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveRight, _window, cx| {
                this.edit_active_text(TextCommand::MoveRight, cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveWordLeft, _window, cx| {
                this.edit_active_text(TextCommand::MoveWordLeft, cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveWordRight, _window, cx| {
                this.edit_active_text(TextCommand::MoveWordRight, cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveHome, _window, cx| {
                this.edit_active_text(TextCommand::MoveHome, cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveEnd, _window, cx| {
                this.edit_active_text(TextCommand::MoveEnd, cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveUp, _window, cx| {
                this.edit_active_text(TextCommand::MoveUp, cx);
            }))
            .on_action(cx.listener(|this, _: &TextMoveDown, _window, cx| {
                this.edit_active_text(TextCommand::MoveDown, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectLeft, _window, cx| {
                this.edit_active_text(TextCommand::SelectLeft, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectRight, _window, cx| {
                this.edit_active_text(TextCommand::SelectRight, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectWordLeft, _window, cx| {
                this.edit_active_text(TextCommand::SelectWordLeft, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectWordRight, _window, cx| {
                this.edit_active_text(TextCommand::SelectWordRight, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectHome, _window, cx| {
                this.edit_active_text(TextCommand::SelectHome, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectEnd, _window, cx| {
                this.edit_active_text(TextCommand::SelectEnd, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectUp, _window, cx| {
                this.edit_active_text(TextCommand::SelectUp, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectDown, _window, cx| {
                this.edit_active_text(TextCommand::SelectDown, cx);
            }))
            .on_action(cx.listener(|this, _: &TextSelectAll, _window, cx| {
                this.edit_active_text(TextCommand::SelectAll, cx);
            }))
            .on_action(cx.listener(|this, _: &TextBackspace, _window, cx| {
                this.edit_active_text(TextCommand::Backspace, cx);
            }))
            .on_action(cx.listener(|this, _: &TextDelete, _window, cx| {
                this.edit_active_text(TextCommand::Delete, cx);
            }))
            .on_action(
                cx.listener(|this, _: &TextDeleteWordBackward, _window, cx| {
                    this.edit_active_text(TextCommand::DeleteWordBackward, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &TextDeleteWordForward, _window, cx| {
                this.edit_active_text(TextCommand::DeleteWordForward, cx);
            }))
            .on_action(cx.listener(|this, _: &TextNewline, _window, cx| {
                this.edit_active_text(TextCommand::Newline, cx);
            }))
            .on_action(cx.listener(|this, _: &TextCopy, _window, cx| {
                this.copy_active_text(false, cx);
            }))
            .on_action(cx.listener(|this, _: &TextCut, _window, cx| {
                this.copy_active_text(true, cx);
            }))
            .on_action(cx.listener(|this, _: &TextPaste, _window, cx| {
                this.paste_active_text(cx);
            }))
            .on_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    let keystroke = &event.keystroke;
                    if this.shortcuts_open {
                        return;
                    }
                    // Modifier chords are declared as text-editing actions in
                    // `crate::actions`; only plain typed characters reach here.
                    if keystroke.modifiers.control
                        || keystroke.modifiers.platform
                        || keystroke.modifiers.alt
                    {
                        return;
                    }
                    let Some(text) = keystroke.key_char.as_deref() else {
                        return;
                    };
                    this.insert_active_text(text, cx);
                }),
            )
            .child(top_bar)
            .child(body)
            .child(status_bar);

        if palette_open {
            root.child(self.palette_overlay(theme, window, cx, cx.entity()))
        } else if shortcuts_open {
            root.child(self.shortcuts_overlay(theme, window, cx))
        } else {
            let mut root = root;
            if let Some(toast) = &toast {
                root = root.child(self.toast_overlay(theme, toast, cx));
            }
            if repo_picker_open {
                root = root.child(self.repo_overlay(theme, window, cx, cx.entity()));
            }
            if let Some(confirm) = &confirm {
                root = root.child(self.confirm_overlay(theme, confirm, cx));
            }
            if let Some(confirm) = &workflow_confirm {
                root = root.child(self.workflow_confirm_overlay(theme, confirm, cx));
            }
            if let Some(confirm) = &schedule_confirm {
                root = root.child(self.schedule_confirm_overlay(theme, confirm, cx));
            }
            root
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        point, px, size, Bounds, Pixels, Size, TestAppContext, VisualTestContext, WindowBounds,
        WindowOptions,
    };

    /// A minimal stand-in for the shell: the same root helper, a fixed sidebar
    /// and a flexible working list. Rendering the real `RootView` would start
    /// the daemon runtime, so the resize contract is tested on the frame the
    /// shell uses.
    struct ShellProbe;

    impl Render for ShellProbe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            shell_root()
                .debug_selector(|| "shell-root".to_string())
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .child(
                            div()
                                .w(px(240.0))
                                .h_full()
                                .debug_selector(|| "shell-sidebar".to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .debug_selector(|| "shell-list".to_string()),
                        ),
                )
        }
    }

    fn open_probe(cx: &mut TestAppContext, window_size: Size<Pixels>) -> &mut VisualTestContext {
        let window = cx.update(|cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.0), px(0.0)),
                        size: window_size,
                    })),
                    ..Default::default()
                },
                |_window, cx| cx.new(|_cx| ShellProbe),
            )
            .expect("opening the test window must succeed")
        });
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        cx
    }

    #[gpui::test]
    fn the_shell_root_and_working_list_follow_a_window_resize(cx: &mut TestAppContext) {
        let cx = open_probe(cx, size(px(900.0), px(700.0)));

        let before = cx.debug_bounds("shell-root").expect("root is rendered");
        let before_list = cx.debug_bounds("shell-list").expect("list is rendered");
        assert_eq!(before.size, size(px(900.0), px(700.0)));
        assert_eq!(before_list.size.width, px(660.0));

        cx.simulate_resize(size(px(1400.0), px(900.0)));
        cx.run_until_parked();

        let after = cx
            .debug_bounds("shell-root")
            .expect("root is rendered after the resize");
        let after_list = cx
            .debug_bounds("shell-list")
            .expect("list is rendered after the resize");
        assert_eq!(after.size, size(px(1400.0), px(900.0)));
        assert_eq!(after_list.size.width, px(1160.0));
        assert!(after_list.size.width > before_list.size.width);
    }
}
