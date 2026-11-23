//! The active-repo model: `repos.list` state, visibility scoping and the
//! searchable switcher.
//!
//! The daemon owns the known-repos registry and the daemon-wide active repo;
//! this module mirrors the TUI's rules so the desktop client scopes rows the
//! same way. It is free of GPUI types, so filtering, availability and the
//! picker selection are unit-tested without a window.

use favetto_wire::model::RepoInfo;

use crate::search::{self, SearchField, SearchQuery};
use crate::text_input::TextInput;

/// One row of the repo picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoOption {
    /// The repo to activate; `None` is the leading `global` row that clears it.
    pub name: Option<String>,
    /// The rendered label, including the active marker and a `(missing)` suffix.
    pub label: String,
    /// Whether the row can be activated. Unavailable repos stay visible but
    /// inert.
    pub selectable: bool,
    /// Whether this row is the active repo.
    pub active: bool,
}

/// Whether a task/event row belongs to `active_repo`.
///
/// With no active repo every row is visible; otherwise only rows belonging to
/// the active repo are. Mirrors `favetto_tui`'s `App::repo_visible`.
pub fn repo_visible(active_repo: Option<&str>, repo: Option<&str>) -> bool {
    match active_repo {
        Some(active) => repo == Some(active),
        None => true,
    }
}

/// Whether a catalog entry belongs to `active_repo`.
///
/// The Catalog shows the active repo's local entries together with the global
/// (unscoped) ones; with no active repo every entry is visible. Unlike
/// [`repo_visible`] (Tasks/Inbox), globals stay visible while a repo is active.
pub fn catalog_visible(active_repo: Option<&str>, repo: Option<&str>) -> bool {
    match active_repo {
        Some(active) => repo == Some(active) || repo.is_none(),
        None => true,
    }
}

/// The top-bar label for the active repo.
///
/// It never falls back to the daemon cwd: before the first `repos.list` the
/// registry is unknown, so the label says so, and an empty active name means
/// the daemon-wide global view.
pub fn active_repo_label(active: Option<&str>, loaded: bool) -> String {
    if !loaded {
        return "repo: —".to_string();
    }
    format!("repo: {}", active.unwrap_or("global"))
}

/// The picker rows: a leading `global` row plus the known repos matching
/// `filter` with the shared fuzzy matcher. The repo name outranks its path, and
/// ties keep the registry order so the list is deterministic.
pub fn repo_picker_entries(repos: &[RepoInfo], filter: &str) -> Vec<RepoOption> {
    let mut entries = vec![RepoOption {
        name: None,
        label: "global  —  all repos".to_string(),
        selectable: true,
        active: false,
    }];

    let query = SearchQuery::new(filter);
    let matched_repos: Vec<(i64, usize, &RepoInfo)> = if query.is_empty() {
        repos
            .iter()
            .enumerate()
            .map(|(index, repo)| (0, index, repo))
            .collect()
    } else {
        let matcher = search::matcher();
        let mut scored = Vec::new();
        for (index, repo) in repos.iter().enumerate() {
            let fields = [
                SearchField::new(&repo.name, search::weight::LABEL),
                SearchField::new(&repo.path, search::weight::NAMESPACE),
            ];
            if let Some(score) = query.score(&matcher, &fields).map(|matched| matched.score) {
                scored.push((score, index, repo));
            }
        }
        scored.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        scored
    };

    for (_, _, repo) in matched_repos {
        let marker = if repo.active { "● " } else { "  " };
        let availability = if repo.available { "" } else { "  (missing)" };
        entries.push(RepoOption {
            name: Some(repo.name.clone()),
            label: format!("{marker}{}{availability}", repo.name),
            selectable: repo.available,
            active: repo.active,
        });
    }
    entries
}

/// The searchable picker's transient state (filter, highlight and in-flight
/// activation). The registry itself lives on the client state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoPicker {
    open: bool,
    filter: TextInput,
    selected: usize,
    error: Option<String>,
    submitting: bool,
}

impl RepoPicker {
    /// Open the picker with an empty filter, highlighting the active repo when
    /// there is one (otherwise the leading `global` row).
    pub fn open(&mut self, repos: &[RepoInfo]) {
        self.open = true;
        self.filter.set_value(String::new());
        self.error = None;
        self.submitting = false;
        self.selected = repos
            .iter()
            .position(|repo| repo.active)
            .map(|index| index + 1)
            .unwrap_or(0);
        self.clamp(repos);
    }

    /// Close the picker and clear its transient state.
    pub fn close(&mut self) {
        self.open = false;
        self.filter.set_value(String::new());
        self.selected = 0;
        self.error = None;
        self.submitting = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn filter(&self) -> &str {
        self.filter.value()
    }

    pub fn input(&self) -> &TextInput {
        &self.filter
    }

    pub fn input_mut(&mut self) -> &mut TextInput {
        &mut self.filter
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_submitting(&self) -> bool {
        self.submitting
    }

    /// The filtered rows, always with the leading `global` row.
    pub fn entries(&self, repos: &[RepoInfo]) -> Vec<RepoOption> {
        repo_picker_entries(repos, self.filter.value())
    }

    /// The highlighted row.
    pub fn selected_option(&self, repos: &[RepoInfo]) -> Option<RepoOption> {
        self.entries(repos).into_iter().nth(self.selected)
    }

    /// Move the highlight, wrapping at both ends.
    pub fn move_selection(&mut self, delta: isize, repos: &[RepoInfo]) {
        let count = self.entries(repos).len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
    }

    /// Highlight a row (a click), clamped to the filtered rows.
    pub fn select(&mut self, selected: usize, repos: &[RepoInfo]) {
        self.selected = selected;
        self.clamp(repos);
    }

    /// Append one typed character to the filter.
    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn push_char(&mut self, character: char, repos: &[RepoInfo]) {
        if character.is_control() {
            return;
        }
        self.filter.push_char(character);
        self.clamp(repos);
    }

    /// Remove the last filter character.
    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn backspace(&mut self, repos: &[RepoInfo]) {
        self.filter.backspace();
        self.clamp(repos);
    }

    /// Re-clamp after an in-place edit from the shared text element.
    pub fn after_edit(&mut self, repos: &[RepoInfo]) {
        self.clamp(repos);
    }

    /// Re-clamp after the registry or filter changed.
    pub fn clamp_selection(&mut self, repos: &[RepoInfo]) {
        self.clamp(repos);
    }

    /// Mark an activation as in flight.
    pub fn begin_submit(&mut self) {
        self.submitting = true;
        self.error = None;
    }

    /// Keep the daemon rejection attached to the picker.
    pub fn fail(&mut self, message: String) {
        self.submitting = false;
        self.error = Some(message);
    }

    fn clamp(&mut self, repos: &[RepoInfo]) {
        let count = self.entries(repos).len();
        self.selected = if count == 0 {
            0
        } else {
            self.selected.min(count - 1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::model::RepoSource;

    fn repo(name: &str, path: &str, available: bool, active: bool) -> RepoInfo {
        RepoInfo {
            name: name.to_string(),
            path: path.to_string(),
            available,
            active,
            source: RepoSource::Manual,
        }
    }

    #[test]
    fn visibility_matches_the_tui_scoping_rules() {
        assert!(repo_visible(None, None));
        assert!(repo_visible(None, Some("a")));
        assert!(repo_visible(Some("a"), Some("a")));
        assert!(!repo_visible(Some("a"), Some("b")));
        assert!(!repo_visible(Some("a"), None));

        assert!(catalog_visible(None, None));
        assert!(catalog_visible(None, Some("a")));
        assert!(catalog_visible(Some("a"), Some("a")));
        assert!(catalog_visible(Some("a"), None));
        assert!(!catalog_visible(Some("a"), Some("b")));
    }

    #[test]
    fn the_label_names_the_active_repo_or_global_without_the_cwd() {
        assert_eq!(active_repo_label(None, false), "repo: —");
        assert_eq!(active_repo_label(None, true), "repo: global");
        assert_eq!(active_repo_label(Some("favetto"), true), "repo: favetto");
    }

    #[test]
    fn picker_prepends_global_filters_case_insensitively_and_marks_missing() {
        let repos = vec![
            repo("favetto", "/code/favetto", true, true),
            repo("web", "/srv/web", true, false),
            repo("gone", "/srv/gone", false, false),
        ];
        let entries = repo_picker_entries(&repos, "");
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, None);
        assert!(entries[0].selectable);
        assert_eq!(entries[1].name.as_deref(), Some("favetto"));
        assert!(entries[1].active);
        assert!(entries[1].label.starts_with("● "));

        let by_name = repo_picker_entries(&repos, "FAV");
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name[1].name.as_deref(), Some("favetto"));

        let by_path = repo_picker_entries(&repos, "srv/web");
        assert_eq!(by_path.len(), 2);
        assert_eq!(by_path[1].name.as_deref(), Some("web"));

        let gone = repo_picker_entries(&repos, "gone");
        assert_eq!(gone.len(), 2);
        assert!(!gone[1].selectable);
        assert!(gone[1].label.ends_with("(missing)"));
    }

    #[test]
    fn picker_matches_fuzzily_and_case_insensitively() {
        let repos = vec![
            repo("favetto", "/code/favetto", true, false),
            repo("web", "/srv/web", true, false),
        ];
        let fuzzy = repo_picker_entries(&repos, "FVTO");
        assert_eq!(fuzzy.len(), 2);
        assert_eq!(fuzzy[1].name.as_deref(), Some("favetto"));
        assert_eq!(fuzzy[0].name, None, "the global row stays pinned");

        let by_path = repo_picker_entries(&repos, "srwb");
        assert_eq!(by_path.len(), 2);
        assert_eq!(by_path[1].name.as_deref(), Some("web"));

        let none = repo_picker_entries(&repos, "zzz");
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].name, None);
    }

    #[test]
    fn the_picker_preselects_the_active_repo_and_reclamps_after_filtering() {
        let repos = vec![
            repo("favetto", "/code/favetto", true, false),
            repo("web", "/srv/web", true, true),
        ];
        let mut picker = RepoPicker::default();
        picker.open(&repos);
        assert!(picker.is_open());
        assert_eq!(picker.selected(), 2);
        assert_eq!(
            picker.selected_option(&repos).unwrap().name.as_deref(),
            Some("web")
        );

        picker.push_char('f', &repos);
        assert_eq!(picker.filter(), "f");
        assert_eq!(picker.entries(&repos).len(), 2);
        assert_eq!(picker.selected(), 1, "the highlight clamps after filtering");

        picker.move_selection(1, &repos);
        assert_eq!(picker.selected(), 0, "selection wraps to global");
        picker.backspace(&repos);
        assert_eq!(picker.filter(), "");

        picker.close();
        assert!(!picker.is_open());
        assert_eq!(picker.selected(), 0);
    }
}
