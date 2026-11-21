//! The command palette model: fuzzy filtering, selection and recents.
//!
//! The model is deliberately free of GPUI types so it can be unit-tested
//! without a window (docs/design/gpui-client.md §4, "Tests"). The root view
//! owns one instance plus the focus handle and renders it as an overlay.
//!
//! Ordering follows docs §2: a recent command is one keystroke away (Working
//! Memory), and an empty query lists recents first before the canonical order.

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use crate::routes::Route;

/// What running a command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    /// Switch the active route.
    Navigate(Route),
    /// Restart the SSE stream and re-validate the connection.
    Reconnect,
    /// Ask the daemon for a fresh `system.ping` round trip.
    Refresh,
    /// Open the searchable repository switcher.
    SwitchRepo,
}

/// One row in the palette.
#[derive(Clone, Copy, Debug)]
pub struct Command {
    /// Stable id used for recents across a session.
    pub id: &'static str,
    /// The row label, also the primary fuzzy haystack.
    pub label: &'static str,
    /// Extra search terms (synonyms, daemon vocabulary).
    pub keywords: &'static str,
    /// The visible keyboard equivalent, when one exists.
    pub shortcut: Option<&'static str>,
    /// What pressing Enter does.
    pub kind: CommandKind,
}

/// The palette's commands, in canonical (Serial Position) order.
pub const COMMANDS: [Command; 9] = [
    Command {
        id: "nav-inbox",
        label: "Go to Inbox",
        keywords: "inbox needs you blocked awaiting permission failed navigation jump",
        shortcut: Some("g i"),
        kind: CommandKind::Navigate(Route::Inbox),
    },
    Command {
        id: "nav-tasks",
        label: "Go to Tasks",
        keywords: "tasks runs list filter search navigation jump",
        shortcut: Some("g t"),
        kind: CommandKind::Navigate(Route::Tasks),
    },
    Command {
        id: "nav-catalog",
        label: "Go to Catalog",
        keywords: "catalog definitions launch variables navigation jump",
        shortcut: Some("g c"),
        kind: CommandKind::Navigate(Route::Catalog),
    },
    Command {
        id: "nav-workflows",
        label: "Go to Workflows",
        keywords: "workflows dag root progress nodes navigation jump",
        shortcut: Some("g w"),
        kind: CommandKind::Navigate(Route::Workflows),
    },
    Command {
        id: "nav-schedules",
        label: "Go to Schedules",
        keywords: "schedules recurring triggers cadence navigation jump",
        shortcut: Some("g s"),
        kind: CommandKind::Navigate(Route::Schedules),
    },
    Command {
        id: "open-settings",
        label: "Open Settings",
        keywords: "settings preferences configuration connection token appearance agents",
        shortcut: None,
        kind: CommandKind::Navigate(Route::Settings),
    },
    Command {
        id: "switch-repo",
        label: "Switch repository",
        keywords: "repo repository workspace switch active change",
        shortcut: None,
        kind: CommandKind::SwitchRepo,
    },
    Command {
        id: "reconnect",
        label: "Reconnect to daemon",
        keywords: "reconnect connection retry transport stream offline",
        shortcut: None,
        kind: CommandKind::Reconnect,
    },
    Command {
        id: "refresh",
        label: "Refresh data",
        keywords: "refresh reload sync ping poll now",
        shortcut: None,
        kind: CommandKind::Refresh,
    },
];

/// Look up a command by its stable id (used when reordering recents).
fn command_by_id(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

/// The palette's session state.
#[derive(Debug, Default)]
pub struct CommandPalette {
    open: bool,
    query: String,
    selected: usize,
    recent: Vec<&'static str>,
}

impl CommandPalette {
    /// How many recents the palette remembers (Miller's Law: a short chunk).
    pub const RECENT_LIMIT: usize = 5;

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Open the palette. The query survives across openings in a session
    /// (docs §2, Working Memory); the selection restarts at the best match.
    pub fn open(&mut self) {
        self.open = true;
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Set the query from typed input and move the selection to the best match.
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.selected = 0;
    }

    /// Append one typed character (used by the text input).
    pub fn push_char(&mut self, c: char) {
        let mut query = self.query.clone();
        query.push(c);
        self.set_query(query);
    }

    /// Remove the last character of the query.
    pub fn backspace(&mut self) {
        let mut query = self.query.clone();
        query.pop();
        self.set_query(query);
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The commands matching the current query. With an empty query this is
    /// recents first, then every command in canonical order, each exactly once.
    pub fn filtered(&self) -> Vec<&'static Command> {
        let query = self.query.trim();
        if query.is_empty() {
            return self.recent_first();
        }

        let matcher = SkimMatcherV2::default();
        let mut scored = Vec::new();
        for (position, command) in COMMANDS.iter().enumerate() {
            let label_score = matcher.fuzzy_match(command.label, query);
            let keyword_score = matcher.fuzzy_match(command.keywords, query);
            let mut score = 0i64;
            let mut matched = false;
            // A label hit outranks a keyword-only hit.
            if let Some(label) = label_score {
                matched = true;
                score += label.saturating_add(100);
            }
            if let Some(keyword) = keyword_score {
                matched = true;
                score += keyword;
            }
            if !matched {
                continue;
            }
            // Recents win ties and near-ties, so a command used moments ago is
            // immediately at hand even with a short query.
            if let Some(recent_position) = self.recent.iter().position(|id| *id == command.id) {
                score += (Self::RECENT_LIMIT - recent_position) as i64 * 25;
            }
            scored.push((score, position, command));
        }
        scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        scored.into_iter().map(|(_, _, command)| command).collect()
    }

    /// The command at the highlighted row.
    pub fn selected_command(&self) -> Option<&'static Command> {
        self.filtered().get(self.selected).copied()
    }

    /// Move the highlight by one row, wrapping at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.filtered().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let count = count as isize;
        let next = (self.selected as isize + delta).rem_euclid(count);
        self.selected = next as usize;
    }

    /// Run the highlighted command and remember it as the most recent one.
    pub fn activate(&mut self) -> Option<CommandKind> {
        let command = self.selected_command()?;
        self.mark_recent(command.id);
        Some(command.kind)
    }

    /// Run a specific command id (a palette row click).
    pub fn activate_id(&mut self, id: &str) -> Option<CommandKind> {
        let command = command_by_id(id)?;
        self.mark_recent(command.id);
        Some(command.kind)
    }

    /// Whether a command is in the recent list (the UI can mark it).
    pub fn is_recent(&self, id: &str) -> bool {
        self.recent.contains(&id)
    }

    fn mark_recent(&mut self, id: &'static str) {
        self.recent.retain(|recent| *recent != id);
        self.recent.insert(0, id);
        self.recent.truncate(Self::RECENT_LIMIT);
    }

    fn recent_first(&self) -> Vec<&'static Command> {
        let mut ordered = Vec::with_capacity(COMMANDS.len());
        for id in &self.recent {
            if let Some(command) = command_by_id(id) {
                ordered.push(command);
            }
        }
        for command in &COMMANDS {
            if !ordered.iter().any(|existing| existing.id == command.id) {
                ordered.push(command);
            }
        }
        ordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette_with(query: &str) -> CommandPalette {
        let mut palette = CommandPalette::default();
        palette.open();
        palette.set_query(query.to_string());
        palette
    }

    #[test]
    fn fuzzy_queries_pick_the_named_surface() {
        for (query, expected) in [
            ("inbox", "nav-inbox"),
            ("inb", "nav-inbox"),
            ("tasks", "nav-tasks"),
            ("catalog", "nav-catalog"),
            ("workflows", "nav-workflows"),
            ("schedules", "nav-schedules"),
            ("settings", "open-settings"),
            ("repo", "switch-repo"),
            ("switch repo", "switch-repo"),
        ] {
            let palette = palette_with(query);
            let first = palette.filtered().first().map(|command| command.id);
            assert_eq!(first, Some(expected), "query {query:?}");
        }
    }

    #[test]
    fn keyword_only_matches_are_found() {
        // "offline" is not in any label; it only appears in the reconnect keywords.
        let palette = palette_with("offline");
        assert_eq!(palette.filtered().first().unwrap().id, "reconnect");
    }

    #[test]
    fn an_empty_query_lists_every_command_once_with_recents_first() {
        let mut palette = CommandPalette::default();
        palette.open();
        let ids: Vec<_> = palette
            .filtered()
            .iter()
            .map(|command| command.id)
            .collect();
        assert_eq!(ids.len(), COMMANDS.len());
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), COMMANDS.len());
        assert_eq!(ids.first(), Some(&"nav-inbox"));

        // Recents move to the top and never duplicate.
        palette.set_query("refresh".into());
        palette.activate();
        palette.set_query(String::new());
        let ids: Vec<_> = palette
            .filtered()
            .iter()
            .map(|command| command.id)
            .collect();
        assert_eq!(ids.first(), Some(&"refresh"));
        assert_eq!(ids.iter().filter(|id| **id == "refresh").count(), 1);

        palette.set_query("tasks".into());
        palette.activate();
        palette.set_query(String::new());
        let ids: Vec<_> = palette
            .filtered()
            .iter()
            .map(|command| command.id)
            .collect();
        assert_eq!(&ids[..2], ["nav-tasks", "refresh"]);
    }

    #[test]
    fn selection_wraps_and_activate_returns_the_highlighted_command() {
        let mut palette = palette_with("tasks");
        assert_eq!(
            palette.selected_command().map(|c| c.kind),
            Some(CommandKind::Navigate(Route::Tasks))
        );
        palette.move_selection(-1);
        assert_eq!(palette.selected(), 0, "single match wraps to itself");

        // With every command listed, selection wraps in both directions.
        palette.set_query(String::new());
        let count = palette.filtered().len();
        palette.move_selection(-1);
        assert_eq!(palette.selected(), count - 1);
        palette.move_selection(1);
        assert_eq!(palette.selected(), 0);
        palette.move_selection(count as isize);
        assert_eq!(palette.selected(), 0, "a full cycle returns to the start");

        palette.set_query("zzzz-no-match".into());
        assert!(palette.filtered().is_empty());
        assert_eq!(palette.activate(), None);
    }

    #[test]
    fn activate_runs_the_selected_kind_and_records_it() {
        let mut palette = palette_with("reconnect");
        assert_eq!(palette.activate(), Some(CommandKind::Reconnect));
        assert!(palette.is_recent("reconnect"));
        assert_eq!(
            palette.query(),
            "reconnect",
            "the query survives activation"
        );

        let mut palette = palette_with("catalog");
        assert_eq!(
            palette.activate(),
            Some(CommandKind::Navigate(Route::Catalog))
        );

        let mut palette = palette_with("switch repository");
        assert_eq!(palette.activate(), Some(CommandKind::SwitchRepo));
        assert!(palette.is_recent("switch-repo"));
    }

    #[test]
    fn recents_are_capped_and_deduplicated() {
        let mut palette = CommandPalette::default();
        for (query, expected) in [
            ("inbox", "nav-inbox"),
            ("tasks", "nav-tasks"),
            ("catalog", "nav-catalog"),
            ("workflows", "nav-workflows"),
            ("schedules", "nav-schedules"),
            ("settings", "open-settings"),
        ] {
            palette.set_query(query.into());
            palette.activate();
            assert!(palette.is_recent(expected));
        }
        // Six distinct activations, but only five are remembered; the newest is first.
        assert_eq!(
            palette.recent.to_vec(),
            vec![
                "open-settings",
                "nav-schedules",
                "nav-workflows",
                "nav-catalog",
                "nav-tasks"
            ]
        );
        // Re-activating an existing recent moves it to the front without duplicating.
        palette.set_query("tasks".into());
        palette.activate();
        assert_eq!(palette.recent.first(), Some(&"nav-tasks"));
        assert_eq!(
            palette
                .recent
                .iter()
                .filter(|id| **id == "nav-tasks")
                .count(),
            1
        );
    }
}
