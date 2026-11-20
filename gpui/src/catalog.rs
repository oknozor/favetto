//! The Catalog model: grouping, fuzzy search, preview and the launch forms.
//!
//! Everything here is free of GPUI types so it can be unit-tested without a
//! window (docs/design/gpui-client.md §4, "Tests"). The view renders these
//! values and the root routes typed characters into the active form.
//!
//! Grouping follows the daemon's vocabulary: a repo-local definition is named
//! `repo:<name>/<path>` and is grouped under that namespace, then by the
//! directory that contains it (docs §2, Choice Overload: one dimension at a
//! time). A global definition (no `repo`) is grouped under `Global`.

use std::collections::HashSet;

use favetto_wire::rpc::CatalogEntry;
use favetto_wire::task_var::{TaskVar, VarType};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde_json::{Map, Value};

/// The first-fetch state of `catalog.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CatalogStatus {
    /// No list has arrived yet.
    #[default]
    Loading,
    /// A list has been folded in (it may still be empty).
    Loaded,
    /// The last fetch failed; an older list is kept visible.
    Failed {
        /// The daemon or transport error.
        message: String,
    },
}

/// One definition, prepared for the list rows.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntryView {
    /// The fully-qualified name passed back to `catalog.get` / `tasks.start`.
    pub name: String,
    /// The name without its `repo:<name>/` namespace prefix.
    pub label: String,
    /// `repo:<name>` for a repo-local task, `global` otherwise.
    pub namespace: String,
    /// The directory containing the definition, relative to the namespace.
    pub folder: String,
    pub agent: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub schedule: Option<String>,
    pub needs: Option<String>,
    pub kind: String,
    pub vars: Vec<TaskVar>,
    pub required_vars: usize,
    pub prompt: String,
}

impl CatalogEntryView {
    fn new(entry: &CatalogEntry) -> Self {
        let (namespace, path) = split_name(&entry.name, entry.repo.as_deref());
        let folder = path
            .rsplit_once('/')
            .map(|(folder, _)| folder.to_string())
            .unwrap_or_default();
        Self {
            name: entry.name.clone(),
            label: path,
            namespace,
            folder,
            agent: entry.agent.clone(),
            provider: entry.provider.clone(),
            model: entry.model.clone(),
            cwd: entry.cwd.clone(),
            schedule: entry.schedule.clone(),
            needs: entry.needs.clone(),
            kind: entry.kind.clone(),
            required_vars: entry.vars.iter().filter(|var| var.required).count(),
            vars: entry.vars.clone(),
            prompt: entry.prompt.clone(),
        }
    }
}

/// One directory inside a namespace.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogFolder {
    /// Stable collapse key, unique across namespaces.
    pub key: String,
    /// `(root)`, or the relative directory.
    pub label: String,
    pub entries: Vec<CatalogEntryView>,
}

/// One repo namespace (or the global group).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogGroup {
    /// Stable collapse key (`repo:<name>`, `global`).
    pub key: String,
    /// `repo:<name>` or `Global`.
    pub label: String,
    pub folders: Vec<CatalogFolder>,
    pub count: usize,
}

/// The fetched raw Markdown for the selected definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPreview {
    pub name: String,
    pub markdown: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
}

/// One editable `[[vars]]` field in the launch sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct VarField {
    pub var: TaskVar,
    pub value: String,
    pub error: Option<String>,
}

impl VarField {
    fn new(var: &TaskVar) -> Self {
        Self {
            var: var.clone(),
            value: var.default.clone().unwrap_or_default(),
            error: None,
        }
    }
}

/// The form shown when a definition is launched.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchForm {
    pub entry: CatalogEntry,
    pub fields: Vec<VarField>,
    pub focused: usize,
    /// The daemon's rejection, kept attached to the form (never a toast-only
    /// error: the user must be able to correct the input).
    pub server_error: Option<String>,
    pub submitting: bool,
}

impl LaunchForm {
    fn new(entry: CatalogEntry) -> Self {
        let fields = entry.vars.iter().map(VarField::new).collect();
        Self {
            entry,
            fields,
            focused: 0,
            server_error: None,
            submitting: false,
        }
    }

    /// Move the focus, wrapping at both ends (Tab / Shift+Tab).
    pub fn cycle_focus(&mut self, delta: isize) {
        if self.fields.is_empty() {
            self.focused = 0;
            return;
        }
        let count = self.fields.len() as isize;
        self.focused = (self.focused as isize + delta).rem_euclid(count) as usize;
    }

    /// Validate every field inline. Returns true when the form can be sent.
    pub fn validate(&mut self) -> bool {
        let mut valid = true;
        for field in &mut self.fields {
            field.error = None;
            if field.value.trim().is_empty() {
                if field.var.required {
                    field.error = Some("This value is required.".to_string());
                    valid = false;
                }
                continue;
            }
            if let Err(error) = field.var.coerce(&field.value) {
                field.error = Some(error.to_string());
                valid = false;
            }
        }
        valid
    }

    /// The `input` object for `tasks.start`. Only declared vars are included;
    /// optional blank values are omitted, exactly like the TUI form.
    pub fn input(&self) -> Option<Value> {
        let mut map = Map::new();
        for field in &self.fields {
            if field.error.is_some() {
                return None;
            }
            if field.value.trim().is_empty() {
                if field.var.required {
                    return None;
                }
                continue;
            }
            let value = field.var.coerce(&field.value).ok()?;
            map.insert(field.var.name.clone(), value);
        }
        Some(Value::Object(map))
    }
}

/// Which control owns the one-shot form's keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneShotFocus {
    Agent,
    Cwd,
}

/// One selectable agent in the one-shot picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentChoice {
    pub name: String,
    pub display_name: String,
    pub available: bool,
    pub default: bool,
}

/// The secondary "New one-shot session" form.
#[derive(Debug, Clone, PartialEq)]
pub struct OneShotForm {
    pub agents: Vec<AgentChoice>,
    pub agent: Option<String>,
    pub cwd: String,
    pub focused: OneShotFocus,
    pub server_error: Option<String>,
    pub submitting: bool,
}

impl OneShotForm {
    fn new(agents: Vec<AgentChoice>) -> Self {
        let agent = agents
            .iter()
            .find(|choice| choice.default && choice.available)
            .or_else(|| agents.iter().find(|choice| choice.available))
            .or_else(|| agents.first())
            .map(|choice| choice.name.clone());
        Self {
            agents,
            agent,
            cwd: String::new(),
            focused: OneShotFocus::Cwd,
            server_error: None,
            submitting: false,
        }
    }

    /// Step the agent selection, wrapping. Only the agent picker uses this.
    pub fn cycle_agent(&mut self, delta: isize) {
        if self.agents.is_empty() {
            self.agent = None;
            return;
        }
        let current = self
            .agent
            .as_ref()
            .and_then(|name| self.agents.iter().position(|choice| &choice.name == name))
            .unwrap_or(0);
        let count = self.agents.len() as isize;
        let next = (current as isize + delta).rem_euclid(count) as usize;
        self.agent = Some(self.agents[next].name.clone());
    }

    pub fn cycle_focus(&mut self) {
        self.focused = match self.focused {
            OneShotFocus::Agent => OneShotFocus::Cwd,
            OneShotFocus::Cwd => OneShotFocus::Agent,
        };
    }
}

/// The session state of the Catalog surface.
#[derive(Debug, Default)]
pub struct CatalogState {
    entries: Vec<CatalogEntry>,
    status: CatalogStatus,
    query: String,
    search_active: bool,
    /// Collapsed namespace and folder keys, remembered for the session.
    collapsed: HashSet<String>,
    selected: Option<String>,
    preview: Option<CatalogPreview>,
    launch: Option<LaunchForm>,
    oneshot: Option<OneShotForm>,
}

impl CatalogState {
    pub fn status(&self) -> &CatalogStatus {
        &self.status
    }

    pub fn has_entries(&self) -> bool {
        !self.entries.is_empty()
    }

    pub fn fail(&mut self, message: String) {
        self.status = CatalogStatus::Failed { message };
    }

    /// Fold a fresh `catalog.list` in. Returns the definition whose preview
    /// should be fetched, when the selection moved or the preview was dropped.
    pub fn set_entries(&mut self, entries: Vec<CatalogEntry>) -> Option<String> {
        self.entries = entries;
        self.status = CatalogStatus::Loaded;
        self.ensure_selection();
        self.preview_needed()
    }

    /// Show the skeleton only while a load is in flight and no older list
    /// exists.
    pub fn begin_load(&mut self) {
        if self.entries.is_empty() {
            self.status = CatalogStatus::Loading;
        }
    }

    /// The selected definition left the list or the filter.
    fn ensure_selection(&mut self) {
        let visible = self.visible_names();
        let still_visible = self
            .selected
            .as_ref()
            .is_some_and(|name| visible.iter().any(|candidate| candidate == name));
        if !still_visible {
            self.selected = visible.first().cloned();
            self.preview = None;
        }
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Select a row; the preview is requested by the caller through
    /// [`CatalogState::preview_needed`].
    pub fn select(&mut self, name: &str) {
        if self.selected.as_deref() != Some(name) {
            self.selected = Some(name.to_string());
            self.preview = None;
        }
    }

    /// The definition a preview read is needed for, if any.
    pub fn preview_needed(&self) -> Option<String> {
        let name = self.selected.clone()?;
        match &self.preview {
            Some(preview) if preview.name == name => None,
            _ => Some(name),
        }
    }

    pub fn begin_preview(&mut self, name: &str) {
        self.preview = Some(CatalogPreview {
            name: name.to_string(),
            markdown: None,
            loading: true,
            error: None,
        });
    }

    pub fn set_preview(&mut self, name: &str, markdown: String) {
        if self.selected.as_deref() != Some(name) {
            return;
        }
        self.preview = Some(CatalogPreview {
            name: name.to_string(),
            markdown: Some(markdown),
            loading: false,
            error: None,
        });
    }

    pub fn fail_preview(&mut self, name: &str, message: String) {
        if self.selected.as_deref() != Some(name) {
            return;
        }
        self.preview = Some(CatalogPreview {
            name: name.to_string(),
            markdown: None,
            loading: false,
            error: Some(message),
        });
    }

    pub fn preview(&self) -> Option<&CatalogPreview> {
        self.preview.as_ref()
    }

    /// Drop the cached source so the next selection check re-reads it (the
    /// daemon pushed `catalog.updated`).
    pub fn invalidate_preview(&mut self) {
        self.preview = None;
    }

    // -- search ------------------------------------------------------------

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn is_search_active(&self) -> bool {
        self.search_active
    }

    pub fn activate_search(&mut self) {
        self.search_active = true;
    }

    pub fn deactivate_search(&mut self) {
        self.search_active = false;
    }

    pub fn push_search_char(&mut self, character: char) -> Option<String> {
        if character.is_control() {
            return None;
        }
        self.query.push(character);
        self.ensure_selection();
        self.preview_needed()
    }

    pub fn backspace_search(&mut self) -> Option<String> {
        self.query.pop();
        self.ensure_selection();
        self.preview_needed()
    }

    /// Clear the query and leave search mode. Returns whether anything changed.
    pub fn clear_search(&mut self) -> bool {
        let changed = self.search_active || !self.query.is_empty();
        self.search_active = false;
        self.query.clear();
        self.ensure_selection();
        changed
    }

    // -- grouping ----------------------------------------------------------

    /// The visible groups after the fuzzy filter, namespaces alphabetically and
    /// `Global` last. Entries inside a folder are alphabetical, which keeps a
    /// row's position stable as live pushes change nothing in the catalog.
    pub fn groups(&self) -> Vec<CatalogGroup> {
        let matcher = SkimMatcherV2::default();
        let query = self.query.trim();
        let mut views: Vec<CatalogEntryView> = self
            .entries
            .iter()
            .filter(|entry| query.is_empty() || fuzzy_score(entry, query, &matcher).is_some())
            .map(CatalogEntryView::new)
            .collect();
        views.sort_by(|left, right| left.name.cmp(&right.name));

        let mut groups: Vec<CatalogGroup> = Vec::new();
        for view in views {
            let group_index = groups
                .iter()
                .position(|group| group.key == view.namespace)
                .unwrap_or_else(|| {
                    groups.push(CatalogGroup {
                        key: view.namespace.clone(),
                        label: namespace_label(&view.namespace),
                        folders: Vec::new(),
                        count: 0,
                    });
                    groups.len() - 1
                });
            let group = &mut groups[group_index];
            group.count += 1;
            let folder_key = if view.folder.is_empty() {
                view.namespace.clone()
            } else {
                format!("{}/{}", view.namespace, view.folder)
            };
            let folder_index = group
                .folders
                .iter()
                .position(|folder| folder.key == folder_key)
                .unwrap_or_else(|| {
                    group.folders.push(CatalogFolder {
                        key: folder_key,
                        label: if view.folder.is_empty() {
                            "(root)".to_string()
                        } else {
                            view.folder.clone()
                        },
                        entries: Vec::new(),
                    });
                    group.folders.len() - 1
                });
            group.folders[folder_index].entries.push(view);
        }

        groups.sort_by(|left, right| {
            let left_global = left.key == "global";
            let right_global = right.key == "global";
            left_global
                .cmp(&right_global)
                .then_with(|| left.key.cmp(&right.key))
        });
        groups
    }

    fn visible_entries(&self) -> Vec<CatalogEntryView> {
        let matcher = SkimMatcherV2::default();
        let query = self.query.trim();
        self.entries
            .iter()
            .filter(|entry| query.is_empty() || fuzzy_score(entry, query, &matcher).is_some())
            .map(CatalogEntryView::new)
            .collect()
    }

    /// The number of definitions the current query matches.
    pub fn matched_count(&self) -> usize {
        self.visible_entries().len()
    }

    pub fn total_count(&self) -> usize {
        self.entries.len()
    }

    /// The raw definitions, used by the Schedules task picker. Sorted by name
    /// as the daemon returns them.
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// The rows in display order, used by keyboard selection. Collapsed
    /// groups/folders are skipped, so the highlight only lands on visible rows.
    pub fn visible_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for group in self.groups() {
            if self.is_collapsed(&group.key) {
                continue;
            }
            for folder in group.folders {
                if self.is_collapsed(&folder.key) {
                    continue;
                }
                for entry in folder.entries {
                    names.push(entry.name);
                }
            }
        }
        names
    }

    pub fn is_collapsed(&self, key: &str) -> bool {
        self.collapsed.contains(key)
    }

    /// The index of a row in the rendered scroll container, counting the
    /// namespace and folder headers that precede it. `None` when the row is
    /// hidden by a collapsed group or the current filter.
    pub fn display_index_of(&self, name: &str) -> Option<usize> {
        let mut index = 0usize;
        for group in self.groups() {
            // A collapsed group still renders its own header.
            index += 1;
            if self.is_collapsed(&group.key) {
                continue;
            }
            for folder in group.folders {
                // A collapsed folder still renders its own header.
                index += 1;
                if self.is_collapsed(&folder.key) {
                    continue;
                }
                for entry in folder.entries {
                    if entry.name == name {
                        return Some(index);
                    }
                    index += 1;
                }
            }
        }
        None
    }

    /// Collapse or expand one group/folder. Session-scoped by design.
    pub fn toggle_collapsed(&mut self, key: &str) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_string());
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let names = self.visible_names();
        if names.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|name| names.iter().position(|candidate| candidate == name))
            .unwrap_or(0);
        let last = names.len() as isize - 1;
        let next = (current as isize + delta).clamp(0, last) as usize;
        self.select(&names[next]);
    }

    // -- launch sheet ------------------------------------------------------

    pub fn launch(&self) -> Option<&LaunchForm> {
        self.launch.as_ref()
    }

    pub fn launch_mut(&mut self) -> Option<&mut LaunchForm> {
        self.launch.as_mut()
    }

    pub fn open_launch(&mut self, name: &str) -> bool {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
        else {
            return false;
        };
        self.launch = Some(LaunchForm::new(entry));
        true
    }

    pub fn close_launch(&mut self) {
        self.launch = None;
    }

    /// Append one typed character to the focused var (multiline values accept
    /// newlines through [`CatalogState::push_launch_newline`]).
    pub fn push_launch_char(&mut self, character: char) {
        let Some(form) = self.launch.as_mut() else {
            return;
        };
        let Some(field) = form.fields.get_mut(form.focused) else {
            return;
        };
        if character.is_control() {
            return;
        }
        field.value.push(character);
        field.error = None;
        form.server_error = None;
    }

    pub fn backspace_launch(&mut self) {
        let Some(form) = self.launch.as_mut() else {
            return;
        };
        let Some(field) = form.fields.get_mut(form.focused) else {
            return;
        };
        field.value.pop();
        field.error = None;
        form.server_error = None;
    }

    /// Enter inside a multiline var inserts a line break instead of submitting.
    pub fn push_launch_newline(&mut self) -> bool {
        let Some(form) = self.launch.as_mut() else {
            return false;
        };
        let Some(field) = form.fields.get_mut(form.focused) else {
            return false;
        };
        if !field.var.multiline {
            return false;
        }
        field.value.push('\n');
        field.error = None;
        form.server_error = None;
        true
    }

    pub fn cycle_launch_focus(&mut self, delta: isize) {
        if let Some(form) = self.launch.as_mut() {
            form.cycle_focus(delta);
        }
    }

    // -- one-shot ----------------------------------------------------------

    pub fn oneshot(&self) -> Option<&OneShotForm> {
        self.oneshot.as_ref()
    }

    pub fn oneshot_mut(&mut self) -> Option<&mut OneShotForm> {
        self.oneshot.as_mut()
    }

    pub fn open_oneshot(&mut self, agents: Vec<AgentChoice>) {
        self.oneshot = Some(OneShotForm::new(agents));
    }

    pub fn close_oneshot(&mut self) {
        self.oneshot = None;
    }

    pub fn push_oneshot_char(&mut self, character: char) {
        let Some(form) = self.oneshot.as_mut() else {
            return;
        };
        if character.is_control() {
            return;
        }
        // Typing while the agent picker is highlighted edits the cwd: the
        // picker is changed with arrow keys/clicks, never with free text.
        form.focused = OneShotFocus::Cwd;
        form.cwd.push(character);
        form.server_error = None;
    }

    pub fn backspace_oneshot(&mut self) {
        let Some(form) = self.oneshot.as_mut() else {
            return;
        };
        form.focused = OneShotFocus::Cwd;
        form.cwd.pop();
        form.server_error = None;
    }

    pub fn cycle_oneshot_focus(&mut self) {
        if let Some(form) = self.oneshot.as_mut() {
            form.cycle_focus();
        }
    }

    pub fn set_oneshot_agent(&mut self, name: &str) {
        if let Some(form) = self.oneshot.as_mut() {
            form.agent = Some(name.to_string());
            form.focused = OneShotFocus::Agent;
            form.server_error = None;
        }
    }

    pub fn set_oneshot_focus(&mut self, focus: OneShotFocus) {
        if let Some(form) = self.oneshot.as_mut() {
            form.focused = focus;
        }
    }

    pub fn cycle_oneshot_agent(&mut self, delta: isize) {
        if let Some(form) = self.oneshot.as_mut() {
            form.cycle_agent(delta);
        }
    }

    pub fn any_form_open(&self) -> bool {
        self.launch.is_some() || self.oneshot.is_some()
    }

    /// Escape: close the topmost sheet first, then clear the search. Returns
    /// whether the key was consumed.
    pub fn close_overlay(&mut self) -> bool {
        if self.launch.is_some() {
            self.launch = None;
            true
        } else if self.oneshot.is_some() {
            self.oneshot = None;
            true
        } else if self.search_active || !self.query.is_empty() {
            self.clear_search();
            true
        } else {
            false
        }
    }
}

/// Split a qualified name into its namespace and relative path. The daemon
/// sends the repo on the entry, but the prefix is also stripped defensively for
/// an older daemon that leaves `repo` unset.
fn split_name(name: &str, repo: Option<&str>) -> (String, String) {
    if let Some(repo) = repo {
        let prefix = format!("repo:{repo}/");
        let path = name.strip_prefix(&prefix).unwrap_or(name);
        return (format!("repo:{repo}"), path.to_string());
    }
    if let Some(rest) = name.strip_prefix("repo:") {
        if let Some((repo, path)) = rest.split_once('/') {
            return (format!("repo:{repo}"), path.to_string());
        }
    }
    ("global".to_string(), name.to_string())
}

fn namespace_label(namespace: &str) -> String {
    if namespace == "global" {
        "Global".to_string()
    } else {
        namespace.to_string()
    }
}

fn fuzzy_score(entry: &CatalogEntry, query: &str, matcher: &SkimMatcherV2) -> Option<i64> {
    let name = matcher
        .fuzzy_match(&entry.name, query)
        .map(|score| score + 200);
    let prompt = matcher.fuzzy_match(&entry.prompt, query);
    match (name, prompt) {
        (None, None) => None,
        (Some(name), Some(prompt)) => Some(name + prompt),
        (Some(name), None) | (None, Some(name)) => Some(name),
    }
}

/// `string`, `int` or `bool` for a var's type chip.
pub fn var_type_label(var_type: VarType) -> &'static str {
    match var_type {
        VarType::String => "string",
        VarType::Int => "int",
        VarType::Bool => "bool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use favetto_wire::task_var::VarType;

    fn var(name: &str, required: bool) -> TaskVar {
        TaskVar {
            name: name.to_string(),
            prompt: format!("Enter {name}"),
            default: None,
            required,
            multiline: false,
            var_type: VarType::String,
            choices: None,
        }
    }

    fn entry(name: &str, repo: Option<&str>, prompt: &str, vars: Vec<TaskVar>) -> CatalogEntry {
        CatalogEntry {
            name: name.to_string(),
            agent: Some("opencode".to_string()),
            provider: Some("opencode-go".to_string()),
            model: Some("deepseek".to_string()),
            cwd: Some("/code/favetto".to_string()),
            schedule: None,
            kind: "agent".to_string(),
            needs: None,
            vars,
            prompt: prompt.to_string(),
            repo: repo.map(str::to_string),
        }
    }

    #[test]
    fn repo_and_folder_structure_is_nested_one_level_at_a_time() {
        let state = CatalogState {
            entries: vec![
                entry(
                    "repo:favetto/gpui/01-scaffold",
                    Some("favetto"),
                    "shell",
                    vec![],
                ),
                entry(
                    "repo:favetto/gpui/02-transport",
                    Some("favetto"),
                    "wire",
                    vec![],
                ),
                entry("repo:favetto/release", Some("favetto"), "ship", vec![]),
                entry("global/hello", None, "say hi", vec![]),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        let groups = state.groups();
        assert_eq!(groups.len(), 2);
        // Repo groups sort before Global.
        assert_eq!(groups[0].key, "repo:favetto");
        assert_eq!(groups[1].key, "global");
        assert_eq!(groups[0].count, 3);
        let folder_labels: Vec<_> = groups[0]
            .folders
            .iter()
            .map(|folder| folder.label.as_str())
            .collect();
        assert!(folder_labels.contains(&"gpui"));
        assert!(folder_labels.contains(&"(root)"));
        let gpui = groups[0]
            .folders
            .iter()
            .find(|folder| folder.label == "gpui")
            .unwrap();
        assert_eq!(gpui.entries.len(), 2);
        assert_eq!(gpui.entries[0].label, "gpui/01-scaffold");
    }

    #[test]
    fn an_unset_repo_prefix_is_still_split_defensively() {
        let state = CatalogState {
            entries: vec![entry("repo:other/docs/guide", None, "read", vec![])],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        let groups = state.groups();
        assert_eq!(groups[0].key, "repo:other");
        assert_eq!(groups[0].folders[0].label, "docs");
    }

    #[test]
    fn search_filters_names_and_prompts_fuzzily() {
        let mut state = CatalogState {
            entries: vec![
                entry(
                    "repo:favetto/triage",
                    Some("favetto"),
                    "sort the inbox",
                    vec![],
                ),
                entry(
                    "repo:favetto/release",
                    Some("favetto"),
                    "publish a changelog",
                    vec![],
                ),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        assert_eq!(state.matched_count(), 2);
        state.push_search_char('t');
        state.push_search_char('r');
        state.push_search_char('i');
        assert_eq!(state.matched_count(), 1);
        assert_eq!(state.selected(), Some("repo:favetto/triage"));

        state.clear_search();
        state.push_search_char('c');
        state.push_search_char('h');
        state.push_search_char('a');
        assert_eq!(state.selected(), Some("repo:favetto/release"));
    }

    #[test]
    fn collapsed_groups_are_remembered_per_session() {
        let mut state = CatalogState::default();
        assert!(!state.is_collapsed("repo:favetto/gpui"));
        state.toggle_collapsed("repo:favetto/gpui");
        assert!(state.is_collapsed("repo:favetto/gpui"));
        state.toggle_collapsed("repo:favetto/gpui");
        assert!(!state.is_collapsed("repo:favetto/gpui"));
    }

    #[test]
    fn collapsed_groups_are_skipped_by_navigation_and_the_scroll_index() {
        let mut state = CatalogState {
            entries: vec![
                entry("repo:favetto/gpui/one", Some("favetto"), "one", vec![]),
                entry("repo:favetto/gpui/two", Some("favetto"), "two", vec![]),
                entry("repo:favetto/release", Some("favetto"), "ship", vec![]),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        assert_eq!(state.visible_names().len(), 3);
        state.toggle_collapsed("repo:favetto/gpui");
        assert_eq!(state.visible_names(), vec!["repo:favetto/release"]);
        assert_eq!(state.display_index_of("repo:favetto/release"), Some(3));
        assert_eq!(state.display_index_of("repo:favetto/gpui/one"), None);
    }

    #[test]
    fn required_vars_block_the_input_until_filled() {
        let mut form = LaunchForm::new(entry(
            "repo:favetto/plan",
            Some("favetto"),
            "plan",
            vec![var("issue", true), var("note", false)],
        ));
        assert!(!form.validate());
        assert_eq!(
            form.fields[0].error.as_deref(),
            Some("This value is required.")
        );
        assert!(form.input().is_none());

        form.fields[0].value = "  ".to_string();
        assert!(!form.validate());

        form.fields[0].value = "42".to_string();
        assert!(form.validate());
        assert_eq!(form.input().unwrap(), serde_json::json!({ "issue": "42" }));
    }

    #[test]
    fn declared_types_and_choices_are_validated_inline() {
        let mut int_var = var("count", false);
        int_var.var_type = VarType::Int;
        let mut choice_var = var("mode", false);
        choice_var.choices = Some(vec!["fast".to_string(), "slow".to_string()]);
        let mut form = LaunchForm::new(entry("plan", None, "plan", vec![int_var, choice_var]));
        form.fields[0].value = "many".to_string();
        form.fields[1].value = "medium".to_string();
        assert!(!form.validate());
        assert_eq!(form.fields[0].error.as_deref(), Some("must be an integer"));
        assert_eq!(
            form.fields[1].error.as_deref(),
            Some("must be one of: fast, slow")
        );

        form.fields[0].value = "3".to_string();
        form.fields[1].value = "fast".to_string();
        assert!(form.validate());
        assert_eq!(
            form.input().unwrap(),
            serde_json::json!({ "count": 3, "mode": "fast" })
        );
    }

    #[test]
    fn optional_blank_vars_are_omitted_from_input() {
        let mut form = LaunchForm::new(entry(
            "plan",
            None,
            "plan",
            vec![var("issue", true), var("note", false)],
        ));
        form.fields[0].value = "7".to_string();
        assert!(form.validate());
        assert_eq!(form.input().unwrap(), serde_json::json!({ "issue": "7" }));
    }

    #[test]
    fn the_default_agent_is_selected_for_a_one_shot() {
        let form = OneShotForm::new(vec![
            AgentChoice {
                name: "claude".to_string(),
                display_name: "Claude".to_string(),
                available: true,
                default: false,
            },
            AgentChoice {
                name: "opencode".to_string(),
                display_name: "OpenCode".to_string(),
                available: true,
                default: true,
            },
        ]);
        assert_eq!(form.agent.as_deref(), Some("opencode"));
        assert_eq!(form.focused, OneShotFocus::Cwd);
    }
}
