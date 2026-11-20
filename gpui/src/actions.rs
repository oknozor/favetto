//! The shell's action vocabulary.
//!
//! Every command is declared as a GPUI action and bound in [`bindings`] against
//! a `key_context`; no view compares raw key strings for shortcuts. The keyboard
//! model is therefore discoverable (`?`), rebindable in one place, and testable
//! independently of the keymap.
//!
//! The root view renders its key context from the live state (`Root`, plus
//! `Tasks`, `Search` or `Confirm` as appropriate), so typed search text never
//! triggers the `g`-jumps, `r` or `?` bindings. The more specific bindings are
//! registered last: GPUI resolves equal-depth matches by binding order.

use gpui::{actions, Action, KeyBinding};

use crate::routes::Route;

actions!(
    favetto,
    [
        /// Open or close the command palette (`⌘K` / `Ctrl+K`).
        TogglePalette,
        /// Close the topmost overlay, or step back from a pushed detail.
        CloseOverlay,
        /// Open or close the shortcut sheet (`?`).
        ToggleShortcuts,
        /// Jump to the Inbox.
        GoInbox,
        /// Jump to Tasks.
        GoTasks,
        /// Jump to the Catalog.
        GoCatalog,
        /// Jump to Workflows.
        GoWorkflows,
        /// Jump to Schedules.
        GoSchedules,
        /// Jump to Settings.
        GoSettings,
        /// Step back from a pushed detail page.
        NavBack,
        /// Run the selected palette command.
        PaletteConfirm,
        /// Move the palette selection up.
        PaletteUp,
        /// Move the palette selection down.
        PaletteDown,
        /// Activate the focused sidebar entry.
        ActivateNav,
        /// Move the active list highlight up.
        ListUp,
        /// Move the active list highlight down.
        ListDown,
        /// Open the highlighted list row's detail.
        ListOpen,
        /// Move the active list highlight to the first row.
        ListHome,
        /// Move the active list highlight to the last row.
        ListEnd,
        /// Move the active list highlight one page up.
        ListPageUp,
        /// Move the active list highlight one page down.
        ListPageDown,
        /// Focus the Tasks search field (`/`).
        TaskSearch,
        /// Open the cancel confirmation for the highlighted run (`c`).
        TaskCancel,
        /// Open the retry confirmation for the highlighted finished run (`r`).
        TaskRetry,
        /// Select the previous status segment (`[`).
        TaskFilterPrev,
        /// Select the next status segment (`]`).
        TaskFilterNext,
        /// Confirm the pending cancel/retry (`↵`).
        TaskConfirmAccept,
        /// Dismiss the pending cancel/retry (`n`).
        TaskConfirmReject,
        /// Focus the Catalog search field (`/`).
        CatalogSearch,
        /// Open the one-shot session sheet (`secondary-n`).
        CatalogNewOneshot,
        /// Validate and launch from the open Catalog sheet (`secondary-↵`).
        CatalogLaunchSubmit,
        /// Move to the next Catalog form field (Tab).
        CatalogFieldNext,
        /// Move to the previous Catalog form field (Shift+Tab).
        CatalogFieldPrev,
        /// Open the root-cancel confirmation on Workflows (`c`).
        WorkflowCancelRoot,
        /// Retry the highlighted failed workflow instance (`r`).
        WorkflowRetryInstance,
        /// Open the Schedules create sheet (`n`).
        ScheduleNew,
        /// Open the Schedules edit sheet for the highlighted row (`↵`).
        ScheduleEdit,
        /// Open the delete confirmation for the highlighted schedule (`d`).
        ScheduleDelete,
        /// Toggle the enabled flag in the schedule sheet (Space).
        ScheduleToggleEnabled,
        /// Move to the next schedule field (Tab).
        ScheduleFieldNext,
        /// Move to the previous schedule field (Shift+Tab).
        ScheduleFieldPrev,
        /// Validate and save the schedule sheet (`⌘↵` / `Ctrl+↵`).
        ScheduleSubmit,
        /// Move to the next Settings control (Tab).
        SettingsFieldNext,
        /// Move to the previous Settings control (Shift+Tab).
        SettingsFieldPrev,
        /// Activate the focused Settings control (Enter).
        SettingsActivate,
        /// Save the connection draft (`⌘↵` / `Ctrl+↵`).
        SettingsSave,
        /// Test the draft connection (`⌘T` / `Ctrl+T`).
        SettingsTest,
        /// Refresh the connection and the current surface.
        Refresh,
        /// Restart the transport.
        Reconnect,
    ]
);

/// The shell keymap. `secondary` is Command on macOS and Control elsewhere, so
/// `⌘K`/`Ctrl+K` and `⌘,`/`Ctrl+,` are one binding each (Jakob's Law, docs §2).
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("secondary-k", TogglePalette, Some("Root")),
        KeyBinding::new("escape", CloseOverlay, Some("Root")),
        KeyBinding::new(
            "shift-/",
            ToggleShortcuts,
            Some("Root && !Palette && !Search && !Confirm"),
        ),
        // Some layouts deliver `?` as its own key rather than shift-/.
        KeyBinding::new(
            "?",
            ToggleShortcuts,
            Some("Root && !Palette && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "g i",
            GoInbox,
            Some("Root && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "g t",
            GoTasks,
            Some("Root && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "g c",
            GoCatalog,
            Some("Root && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "g w",
            GoWorkflows,
            Some("Root && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "g s",
            GoSchedules,
            Some("Root && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "secondary-,",
            GoSettings,
            Some("Root && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        // List navigation (docs §1: every list is operable with ↑/↓/Enter).
        KeyBinding::new(
            "up",
            ListUp,
            Some("Root && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "down",
            ListDown,
            Some("Root && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "home",
            ListHome,
            Some("Root && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "end",
            ListEnd,
            Some("Root && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "pageup",
            ListPageUp,
            Some("Root && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "pagedown",
            ListPageDown,
            Some("Root && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "enter",
            ListOpen,
            Some("Root && !Palette && !Shortcuts && !Confirm && !Sheet"),
        ),
        KeyBinding::new(
            "r",
            Refresh,
            Some("Root && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new("up", PaletteUp, Some("Palette")),
        KeyBinding::new("down", PaletteDown, Some("Palette")),
        // Tab cycles the palette selection instead of leaving the modal, so the
        // overlay keeps keyboard focus while it is open.
        KeyBinding::new("tab", PaletteDown, Some("Palette")),
        KeyBinding::new("shift-tab", PaletteUp, Some("Palette")),
        KeyBinding::new("enter", PaletteConfirm, Some("Palette")),
        // Tasks-specific bindings are registered after the generic ones so a
        // same-depth match resolves to them (GPUI prefers the later binding).
        KeyBinding::new(
            "c",
            TaskCancel,
            Some("Tasks && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "r",
            TaskRetry,
            Some("Tasks && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "/",
            TaskSearch,
            Some("Tasks && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "[",
            TaskFilterPrev,
            Some("Tasks && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "]",
            TaskFilterNext,
            Some("Tasks && !Palette && !Shortcuts && !Search && !Confirm"),
        ),
        KeyBinding::new(
            "enter",
            TaskConfirmAccept,
            Some("Tasks && Confirm && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "y",
            TaskConfirmAccept,
            Some("Tasks && Confirm && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "n",
            TaskConfirmReject,
            Some("Tasks && Confirm && !Palette && !Shortcuts"),
        ),
        // Catalog: search, the secondary one-shot sheet, and the modal form's
        // submit/focus chords. The submit chord is registered last so it wins
        // over any generic Enter binding inside the sheet.
        KeyBinding::new(
            "/",
            CatalogSearch,
            Some("Catalog && !Palette && !Shortcuts && !Search && !Launch && !Oneshot && !Confirm"),
        ),
        KeyBinding::new(
            "secondary-n",
            CatalogNewOneshot,
            Some("Catalog && !Palette && !Shortcuts && !Search && !Launch && !Oneshot && !Confirm"),
        ),
        KeyBinding::new("tab", CatalogFieldNext, Some("Catalog && Launch")),
        KeyBinding::new("shift-tab", CatalogFieldPrev, Some("Catalog && Launch")),
        KeyBinding::new("tab", CatalogFieldNext, Some("Catalog && Oneshot")),
        KeyBinding::new("shift-tab", CatalogFieldPrev, Some("Catalog && Oneshot")),
        KeyBinding::new(
            "secondary-enter",
            CatalogLaunchSubmit,
            Some("Catalog && Launch"),
        ),
        KeyBinding::new(
            "secondary-enter",
            CatalogLaunchSubmit,
            Some("Catalog && Oneshot"),
        ),
        // Workflows: the root cancel and failed-instance retry are registered
        // after the generic `r` refresh so they win on this surface only.
        KeyBinding::new(
            "c",
            WorkflowCancelRoot,
            Some("Workflows && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "r",
            WorkflowRetryInstance,
            Some("Workflows && !Palette && !Shortcuts && !Confirm"),
        ),
        KeyBinding::new(
            "enter",
            TaskConfirmAccept,
            Some("Workflows && Confirm && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "y",
            TaskConfirmAccept,
            Some("Workflows && Confirm && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "n",
            TaskConfirmReject,
            Some("Workflows && Confirm && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "n",
            ScheduleNew,
            Some("Schedules && !Palette && !Shortcuts && !Sheet && !Confirm"),
        ),
        KeyBinding::new(
            "d",
            ScheduleDelete,
            Some("Schedules && !Palette && !Shortcuts && !Sheet && !Confirm"),
        ),
        // Registered after the generic ListOpen so Enter opens the schedule
        // sheet on this surface, while the other lists keep their own handler.
        KeyBinding::new(
            "enter",
            ScheduleEdit,
            Some("Schedules && !Palette && !Shortcuts && !Sheet && !Confirm"),
        ),
        KeyBinding::new("tab", ScheduleFieldNext, Some("Schedules && Sheet")),
        KeyBinding::new("shift-tab", ScheduleFieldPrev, Some("Schedules && Sheet")),
        KeyBinding::new(
            "secondary-enter",
            ScheduleSubmit,
            Some("Schedules && Sheet"),
        ),
        // A plain Space/letter must stay available for the cron expression, so
        // the enabled toggle uses a modifier chord (and the sheet's row click).
        KeyBinding::new(
            "secondary-e",
            ScheduleToggleEnabled,
            Some("Schedules && Sheet"),
        ),
        KeyBinding::new(
            "enter",
            TaskConfirmAccept,
            Some("Schedules && Confirm && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "y",
            TaskConfirmAccept,
            Some("Schedules && Confirm && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "n",
            TaskConfirmReject,
            Some("Schedules && Confirm && !Palette && !Shortcuts"),
        ),
        // Settings: Tab walks the logical controls, Enter activates the focused
        // one, and the secondary chords save/test without leaving the surface.
        KeyBinding::new(
            "tab",
            SettingsFieldNext,
            Some("Settings && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "shift-tab",
            SettingsFieldPrev,
            Some("Settings && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "enter",
            SettingsActivate,
            Some("Settings && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "secondary-enter",
            SettingsSave,
            Some("Settings && !Palette && !Shortcuts"),
        ),
        KeyBinding::new(
            "secondary-t",
            SettingsTest,
            Some("Settings && !Palette && !Shortcuts"),
        ),
    ]
}

/// Map a `g`-jump action back to its route. Kept as one function so tests can
/// assert that every bound jump reaches the intended surface.
pub fn route_for_action(action: &dyn Action) -> Option<Route> {
    if action.partial_eq(&GoInbox) {
        Some(Route::Inbox)
    } else if action.partial_eq(&GoTasks) {
        Some(Route::Tasks)
    } else if action.partial_eq(&GoCatalog) {
        Some(Route::Catalog)
    } else if action.partial_eq(&GoWorkflows) {
        Some(Route::Workflows)
    } else if action.partial_eq(&GoSchedules) {
        Some(Route::Schedules)
    } else if action.partial_eq(&GoSettings) {
        Some(Route::Settings)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contexts() -> Vec<(String, Option<String>)> {
        bindings()
            .iter()
            .map(|binding| {
                (
                    binding
                        .action()
                        .name()
                        .rsplit("::")
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    binding.predicate().map(|predicate| predicate.to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn every_jump_action_maps_to_its_route() {
        assert_eq!(route_for_action(&GoInbox), Some(Route::Inbox));
        assert_eq!(route_for_action(&GoTasks), Some(Route::Tasks));
        assert_eq!(route_for_action(&GoCatalog), Some(Route::Catalog));
        assert_eq!(route_for_action(&GoWorkflows), Some(Route::Workflows));
        assert_eq!(route_for_action(&GoSchedules), Some(Route::Schedules));
        assert_eq!(route_for_action(&GoSettings), Some(Route::Settings));
        assert_eq!(route_for_action(&TogglePalette), None);
    }

    #[test]
    fn the_keymap_binds_actions_in_the_right_contexts() {
        let contexts = contexts();
        assert!(contexts.contains(&("TogglePalette".into(), Some("Root".into()))));
        assert!(contexts.iter().any(|(action, context)| {
            action == "GoInbox" && context.as_deref().is_some_and(|c| c.contains("!Palette"))
        }));
        assert!(contexts.iter().any(|(action, context)| {
            action == "PaletteConfirm" && context.as_deref() == Some("Palette")
        }));
        for action in ["ListUp", "ListDown", "ListOpen", "Refresh"] {
            assert!(
                contexts.iter().any(|(bound, context)| bound == action
                    && context
                        .as_deref()
                        .is_some_and(|context| context.contains("!Palette"))),
                "{action} must be reachable outside the palette"
            );
        }
    }

    #[test]
    fn task_shortcuts_are_scoped_to_the_tasks_surface() {
        let contexts = contexts();
        for (action, keys) in [
            ("TaskCancel", "c"),
            ("TaskRetry", "r"),
            ("TaskSearch", "/"),
            ("TaskFilterPrev", "["),
            ("TaskFilterNext", "]"),
        ] {
            let matches: Vec<_> = bindings()
                .into_iter()
                .filter(|binding| {
                    binding.action().name().ends_with(action)
                        && binding.keystrokes().iter().any(|key| key.key() == keys)
                })
                .collect();
            assert_eq!(matches.len(), 1, "{action} must have exactly one binding");
            assert!(
                matches[0]
                    .predicate()
                    .is_some_and(|predicate| predicate.to_string().contains("Tasks")),
                "{action} must only fire on the Tasks surface"
            );
            assert!(
                contexts.iter().any(|(bound, _)| bound == action),
                "{action} missing from the keymap"
            );
        }
        // The confirmation dialog owns Enter (registered after ListOpen).
        let order: Vec<_> = bindings()
            .iter()
            .filter(|binding| binding.keystrokes().iter().any(|key| key.key() == "enter"))
            .map(|binding| {
                binding
                    .action()
                    .name()
                    .rsplit("::")
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        let list_open = order.iter().position(|name| name == "ListOpen");
        let accept = order.iter().position(|name| name == "TaskConfirmAccept");
        assert!(list_open.is_some() && accept.is_some());
        assert!(accept > list_open);
    }

    #[test]
    fn workflow_shortcuts_are_scoped_to_the_workflows_surface() {
        for (action, key) in [("WorkflowCancelRoot", "c"), ("WorkflowRetryInstance", "r")] {
            let matches: Vec<_> = bindings()
                .into_iter()
                .filter(|binding| {
                    binding.action().name().ends_with(action)
                        && binding
                            .keystrokes()
                            .iter()
                            .any(|stroke| stroke.key() == key)
                })
                .collect();
            assert_eq!(matches.len(), 1, "{action} must have exactly one binding");
            assert!(
                matches[0]
                    .predicate()
                    .is_some_and(|predicate| predicate.to_string().contains("Workflows")),
                "{action} must only fire on the Workflows surface"
            );
        }
    }

    #[test]
    fn catalog_shortcuts_are_scoped_to_the_catalog_and_its_sheets() {
        for (action, key) in [
            ("CatalogSearch", "/"),
            ("CatalogNewOneshot", "n"),
            ("CatalogLaunchSubmit", "enter"),
            ("CatalogFieldNext", "tab"),
            ("CatalogFieldPrev", "tab"),
        ] {
            let matches: Vec<_> = bindings()
                .into_iter()
                .filter(|binding| {
                    binding.action().name().ends_with(action)
                        && binding
                            .keystrokes()
                            .iter()
                            .any(|stroke| stroke.key() == key)
                })
                .collect();
            assert!(!matches.is_empty(), "{action} has no binding");
            assert!(
                matches.iter().all(|binding| binding
                    .predicate()
                    .is_some_and(|predicate| predicate.to_string().contains("Catalog"))),
                "{action} must only fire on the Catalog surface"
            );
        }
    }

    #[test]
    fn schedule_shortcuts_are_scoped_to_the_schedules_surface_and_sheet() {
        for (action, key, needs_sheet) in [
            ("ScheduleNew", "n", false),
            ("ScheduleDelete", "d", false),
            ("ScheduleEdit", "enter", false),
            ("ScheduleSubmit", "enter", true),
            ("ScheduleFieldNext", "tab", true),
            ("ScheduleToggleEnabled", "e", true),
        ] {
            let matches: Vec<_> = bindings()
                .into_iter()
                .filter(|binding| {
                    binding.action().name().ends_with(action)
                        && binding
                            .keystrokes()
                            .iter()
                            .any(|stroke| stroke.key() == key)
                })
                .collect();
            assert_eq!(matches.len(), 1, "{action} must have one binding");
            let predicate = matches[0].predicate().expect("context").to_string();
            assert!(
                predicate.contains("Schedules"),
                "{action} must be scoped to Schedules"
            );
            let mentions_sheet = predicate.contains("Sheet") && !predicate.contains("!Sheet");
            assert_eq!(mentions_sheet, needs_sheet, "{action} sheet scoping");
        }
    }

    #[test]
    fn settings_shortcuts_are_scoped_to_the_settings_surface() {
        for (action, key) in [
            ("SettingsFieldNext", "tab"),
            ("SettingsActivate", "enter"),
            ("SettingsSave", "enter"),
            ("SettingsTest", "t"),
        ] {
            let matches: Vec<_> = bindings()
                .into_iter()
                .filter(|binding| {
                    binding.action().name().ends_with(action)
                        && binding
                            .keystrokes()
                            .iter()
                            .any(|stroke| stroke.key() == key)
                })
                .collect();
            assert_eq!(matches.len(), 1, "{action} must have one binding");
            assert!(
                matches[0]
                    .predicate()
                    .is_some_and(|predicate| predicate.to_string().contains("Settings")),
                "{action} must be scoped to Settings"
            );
        }
    }
}
