//! The shell's action vocabulary.
//!
//! Every command is declared as a GPUI action and bound in [`bindings`] against
//! a `key_context`; no view compares raw key strings for shortcuts. The keyboard
//! model is therefore discoverable (`?`), rebindable in one place, and testable
//! independently of the keymap.

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
        /// Move the Inbox Needs you highlight up.
        InboxUp,
        /// Move the Inbox Needs you highlight down.
        InboxDown,
        /// Open the highlighted Inbox row's task detail.
        InboxOpen,
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
        KeyBinding::new("shift-/", ToggleShortcuts, Some("Root && !Palette")),
        // Some layouts deliver `?` as its own key rather than shift-/.
        KeyBinding::new("?", ToggleShortcuts, Some("Root && !Palette")),
        KeyBinding::new("g i", GoInbox, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("g t", GoTasks, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("g c", GoCatalog, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("g w", GoWorkflows, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("g s", GoSchedules, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new(
            "secondary-,",
            GoSettings,
            Some("Root && !Palette && !Shortcuts"),
        ),
        // Inbox list navigation (docs §1: every list is operable with ↑/↓/Enter).
        KeyBinding::new("up", InboxUp, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("down", InboxDown, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("enter", InboxOpen, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("r", Refresh, Some("Root && !Palette && !Shortcuts")),
        KeyBinding::new("up", PaletteUp, Some("Palette")),
        KeyBinding::new("down", PaletteDown, Some("Palette")),
        // Tab cycles the palette selection instead of leaving the modal, so the
        // overlay keeps keyboard focus while it is open.
        KeyBinding::new("tab", PaletteDown, Some("Palette")),
        KeyBinding::new("shift-tab", PaletteUp, Some("Palette")),
        KeyBinding::new("enter", PaletteConfirm, Some("Palette")),
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
        let bindings = bindings();
        let contexts: Vec<_> = bindings
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
            .collect();

        assert!(contexts.contains(&("TogglePalette".into(), Some("Root".into()))));
        assert!(contexts.iter().any(|(action, context)| {
            action == "GoInbox" && context.as_deref().is_some_and(|c| c.contains("!Palette"))
        }));
        assert!(contexts.iter().any(|(action, context)| {
            action == "PaletteConfirm" && context.as_deref() == Some("Palette")
        }));
        for action in ["InboxUp", "InboxDown", "InboxOpen", "Refresh"] {
            assert!(
                contexts.iter().any(|(bound, context)| bound == action
                    && context
                        .as_deref()
                        .is_some_and(|context| context.contains("!Palette"))),
                "{action} must be reachable outside the palette"
            );
        }
    }
}
