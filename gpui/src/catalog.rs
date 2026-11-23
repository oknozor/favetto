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
use serde_json::{Map, Value};

use crate::repos::catalog_visible;
use crate::search::{self, SearchField, SearchQuery};
use crate::text_input::TextInput;

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

    /// The label painted under a folder header: the last path segment.
    ///
    /// The folder header already names the directory, so repeating the whole
    /// relative path on every child row (as `label` does) reads as a flat list
    /// rather than a tree.
    pub fn basename(&self) -> &str {
        self.label
            .rsplit_once('/')
            .map(|(_, name)| name)
            .unwrap_or(&self.label)
    }

    /// The one searchable field: the visible basename.
    ///
    /// The prompt, metadata, folder and fully-qualified `name` are deliberately
    /// absent. Matching the prompt buried the definition the user was looking
    /// for under unrelated rows, and the hidden `repo:<name>/` prefix would make
    /// every definition in a repo match its namespace.
    fn search_field(&self) -> SearchField<'_> {
        SearchField::new(self.basename(), search::weight::LABEL)
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
    pub value: TextInput,
    pub error: Option<String>,
}

impl VarField {
    fn new(var: &TaskVar) -> Self {
        Self {
            var: var.clone(),
            value: TextInput::new(var.default.clone().unwrap_or_default()),
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
    /// Selectable configured agents for a per-launch override.
    pub agents: Vec<AgentChoice>,
    /// The selected agent override. Seeded from the definition (or the daemon
    /// default), and changeable before launch.
    pub agent: Option<String>,
    /// The model override typed into the sheet: a bare id or `provider/model`.
    pub model: TextInput,
    /// The daemon's rejection, kept attached to the form (never a toast-only
    /// error: the user must be able to correct the input).
    pub server_error: Option<String>,
    pub submitting: bool,
}

impl LaunchForm {
    /// Build a launch form without an agent roster (used by unit tests).
    #[cfg(test)]
    pub fn new(entry: CatalogEntry) -> Self {
        Self::with_agents(entry, Vec::new())
    }

    /// Build a launch form with the daemon's configured agents.
    pub fn with_agents(entry: CatalogEntry, agents: Vec<AgentChoice>) -> Self {
        let fields: Vec<VarField> = entry.vars.iter().map(VarField::new).collect();
        let agent = if entry.kind == "action" {
            None
        } else {
            entry.agent.clone().or_else(|| {
                agents
                    .iter()
                    .find(|choice| choice.default && choice.available)
                    .or_else(|| agents.iter().find(|choice| choice.available))
                    .map(|choice| choice.name.clone())
            })
        };
        let model = if entry.kind == "action" {
            String::new()
        } else {
            match (entry.provider.as_deref(), entry.model.as_deref()) {
                (Some(provider), Some(model)) if !provider.is_empty() => {
                    format!("{provider}/{model}")
                }
                (_, Some(model)) => model.to_string(),
                _ => String::new(),
            }
        };
        let model = TextInput::new(model);
        Self {
            entry,
            fields,
            focused: 0,
            agents,
            agent,
            model,
            server_error: None,
            submitting: false,
        }
    }

    /// Whether this launch has agent/model overrides at all. Action tasks never
    /// launch an agent, so they hide the controls.
    pub fn show_overrides(&self) -> bool {
        self.entry.kind != "action"
    }

    /// Number of focusable controls: declared vars, then agent and model.
    fn field_count(&self) -> usize {
        self.fields.len() + if self.show_overrides() { 2 } else { 0 }
    }

    pub fn agent_index(&self) -> Option<usize> {
        self.show_overrides().then_some(self.fields.len())
    }

    pub fn model_index(&self) -> Option<usize> {
        self.show_overrides().then_some(self.fields.len() + 1)
    }

    /// Move the focus, wrapping at both ends (Tab / Shift+Tab).
    pub fn cycle_focus(&mut self, delta: isize) {
        let count = self.field_count();
        if count == 0 {
            self.focused = 0;
            return;
        }
        self.focused = (self.focused as isize + delta).rem_euclid(count as isize) as usize;
    }

    /// Cycle the focused field's fixed choice list (←/→). Returns whether the
    /// focused field actually offered choices. Typing an exact value still
    /// works; this is the discoverable keyboard path for a declared enum. The
    /// agent picker is also cycled with arrows.
    pub fn cycle_choice(&mut self, delta: isize) -> bool {
        if self.agent_index() == Some(self.focused) {
            self.cycle_agent(delta);
            return true;
        }
        let Some(field) = self.fields.get_mut(self.focused) else {
            return false;
        };
        let next_value = {
            let Some(choices) = field.var.choices.as_ref() else {
                return false;
            };
            if choices.is_empty() {
                return false;
            }
            let current = choices
                .iter()
                .position(|choice| choice == field.value.value());
            let next = match current {
                Some(current) => {
                    let last = choices.len() as isize - 1;
                    (current as isize + delta).clamp(0, last) as usize
                }
                None => 0,
            };
            choices[next].clone()
        };
        field.value = TextInput::new(next_value);
        field.error = None;
        true
    }

    /// Step the agent selection, wrapping. Only available agents are offered.
    pub fn cycle_agent(&mut self, delta: isize) {
        let available: Vec<&AgentChoice> = self
            .agents
            .iter()
            .filter(|choice| choice.available)
            .collect();
        if available.is_empty() {
            return;
        }
        let current = self
            .agent
            .as_ref()
            .and_then(|name| available.iter().position(|choice| &choice.name == name))
            .unwrap_or(0);
        let count = available.len() as isize;
        let next = (current as isize + delta).rem_euclid(count) as usize;
        self.agent = Some(available[next].name.clone());
        self.server_error = None;
    }

    /// Set the agent override from a click, clearing any stale error.
    pub fn set_agent(&mut self, name: &str) {
        self.agent = Some(name.to_string());
        self.server_error = None;
    }

    /// Append one typed character to the focused text control.
    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn push_char(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        let focused = self.focused;
        if self.model_index() == Some(focused) {
            self.model.push_char(character);
            self.server_error = None;
            return;
        }
        let Some(field) = self.fields.get_mut(focused) else {
            return;
        };
        field.value.push_char(character);
        field.error = None;
        self.server_error = None;
    }

    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn pop_char(&mut self) {
        let focused = self.focused;
        if self.model_index() == Some(focused) {
            self.model.backspace();
            self.server_error = None;
            return;
        }
        let Some(field) = self.fields.get_mut(focused) else {
            return;
        };
        field.value.backspace();
        field.error = None;
        self.server_error = None;
    }

    pub fn text_input(&self, target: crate::text_input::TextTarget) -> Option<&TextInput> {
        use crate::text_input::TextTarget;
        match target {
            TextTarget::CatalogLaunchModel if self.model_index().is_some() => Some(&self.model),
            TextTarget::CatalogLaunchVar(index) => self.fields.get(index).map(|f| &f.value),
            _ => None,
        }
    }

    pub fn text_input_for_target_mut(
        &mut self,
        target: crate::text_input::TextTarget,
    ) -> Option<&mut TextInput> {
        use crate::text_input::TextTarget;
        match target {
            TextTarget::CatalogLaunchModel if self.model_index().is_some() => Some(&mut self.model),
            TextTarget::CatalogLaunchVar(index) => {
                self.fields.get_mut(index).map(|field| &mut field.value)
            }
            _ => None,
        }
    }

    pub fn focused_text_target(&self) -> Option<crate::text_input::TextTarget> {
        use crate::text_input::TextTarget;
        if self.model_index() == Some(self.focused) {
            Some(TextTarget::CatalogLaunchModel)
        } else if self.focused < self.fields.len() {
            Some(TextTarget::CatalogLaunchVar(self.focused))
        } else {
            None
        }
    }

    /// Clear the inline error after an edit from the shared buffer.
    pub fn after_text_edit(&mut self) {
        if self.model_index() == Some(self.focused) {
            self.server_error = None;
            return;
        }
        if let Some(field) = self.fields.get_mut(self.focused) {
            field.error = None;
        }
        self.server_error = None;
    }

    /// The per-launch overrides to send. Empty values become `None`.
    pub fn overrides(&self) -> (Option<String>, Option<String>) {
        if !self.show_overrides() {
            return (None, None);
        }
        let model = self.model.value().trim();
        (
            self.agent.clone(),
            (!model.is_empty()).then(|| model.to_string()),
        )
    }

    /// Validate every field inline. Returns true when the form can be sent.
    pub fn validate(&mut self) -> bool {
        let mut valid = true;
        for field in &mut self.fields {
            field.error = None;
            if field.value.value().trim().is_empty() {
                if field.var.required {
                    field.error = Some("This value is required.".to_string());
                    valid = false;
                }
                continue;
            }
            if let Err(error) = field.var.coerce(field.value.value()) {
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
            if field.value.value().trim().is_empty() {
                if field.var.required {
                    return None;
                }
                continue;
            }
            let value = field.var.coerce(field.value.value()).ok()?;
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
    pub cwd: TextInput,
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
            cwd: TextInput::default(),
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
    query: TextInput,
    search_active: bool,
    /// Collapsed namespace and folder keys, remembered for the session.
    collapsed: HashSet<String>,
    selected: Option<String>,
    preview: Option<CatalogPreview>,
    launch: Option<LaunchForm>,
    oneshot: Option<OneShotForm>,
    /// The daemon-wide active repo. The Catalog shows its local entries plus
    /// global ones, matching the TUI's `catalog_visible`.
    active_repo: Option<String>,
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

    /// Apply the daemon-wide active repo. The selection is re-clamped and a
    /// cached preview for a now-hidden definition is dropped.
    pub fn set_active_repo(&mut self, active: Option<String>) {
        let changed = self.active_repo != active;
        self.active_repo = active;
        if changed {
            self.preview = None;
        }
        self.ensure_selection();
    }

    /// Whether an entry is in scope for the active repo (its own entries plus
    /// global ones; everything with no active repo).
    fn entry_in_scope(&self, entry: &CatalogEntry) -> bool {
        catalog_visible(self.active_repo.as_deref(), entry.repo.as_deref())
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
        self.query.value()
    }

    pub fn search_input(&self) -> &TextInput {
        &self.query
    }

    pub fn search_input_mut(&mut self) -> &mut TextInput {
        &mut self.query
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

    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn push_search_char(&mut self, character: char) -> Option<String> {
        if character.is_control() {
            return None;
        }
        self.query.push_char(character);
        self.ensure_selection();
        self.preview_needed()
    }

    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn backspace_search(&mut self) -> Option<String> {
        self.query.backspace();
        self.ensure_selection();
        self.preview_needed()
    }

    /// Re-run the filter and return the definition whose preview is needed
    /// after an in-place edit from the shared text element.
    pub fn after_search_edit(&mut self) -> Option<String> {
        self.ensure_selection();
        self.preview_needed()
    }

    /// The editable buffer for a launch-sheet target.
    pub fn launch_text_input(&self, target: crate::text_input::TextTarget) -> Option<&TextInput> {
        self.launch.as_ref()?.text_input(target)
    }

    pub fn launch_text_input_mut(
        &mut self,
        target: crate::text_input::TextTarget,
    ) -> Option<&mut TextInput> {
        self.launch.as_mut()?.text_input_for_target_mut(target)
    }

    pub fn oneshot_cwd_input(&self) -> Option<&TextInput> {
        self.oneshot.as_ref().map(|form| &form.cwd)
    }

    pub fn oneshot_cwd_input_mut(&mut self) -> Option<&mut TextInput> {
        self.oneshot.as_mut().map(|form| &mut form.cwd)
    }

    /// Clear the query and leave search mode. Returns whether anything changed.
    pub fn clear_search(&mut self) -> bool {
        let changed = self.search_active || !self.query.is_empty();
        self.search_active = false;
        self.query.set_value(String::new());
        self.ensure_selection();
        changed
    }

    // -- grouping ----------------------------------------------------------

    /// Whether the active filter has no terms (whitespace-only counts as empty).
    fn query_is_empty(&self) -> bool {
        SearchQuery::new(self.query.value()).is_empty()
    }

    /// The visible groups after the fuzzy filter. An empty query keeps the
    /// daemon-independent alphabetical grouping (`Global` last); with a query,
    /// entries, folders and namespaces are ordered by their best match so the
    /// most relevant definition is first.
    pub fn groups(&self) -> Vec<CatalogGroup> {
        let matcher = search::matcher();
        let query = SearchQuery::new(self.query.value());
        let mut scored: Vec<(i64, CatalogEntryView)> = self
            .entries
            .iter()
            .filter(|entry| self.entry_in_scope(entry))
            .filter_map(|entry| {
                let view = CatalogEntryView::new(entry);
                let score = if query.is_empty() {
                    0
                } else {
                    query.score(&matcher, &[view.search_field()])?.score
                };
                Some((score, view))
            })
            .collect();

        if query.is_empty() {
            scored.sort_by(|left, right| left.1.name.cmp(&right.1.name));
        } else {
            scored.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then_with(|| left.1.name.cmp(&right.1.name))
            });
        }

        let mut groups: Vec<CatalogGroup> = Vec::new();
        for (_, view) in scored {
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

        if query.is_empty() {
            groups.sort_by(|left, right| {
                let left_global = left.key == "global";
                let right_global = right.key == "global";
                left_global
                    .cmp(&right_global)
                    .then_with(|| left.key.cmp(&right.key))
            });
        }
        groups
    }

    fn visible_entries(&self) -> Vec<CatalogEntryView> {
        self.groups()
            .into_iter()
            .flat_map(|group| group.folders)
            .flat_map(|folder| folder.entries)
            .collect()
    }

    /// The number of definitions the current query matches.
    pub fn matched_count(&self) -> usize {
        self.visible_entries().len()
    }

    pub fn total_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| self.entry_in_scope(entry))
            .count()
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

    /// Whether `key` is collapsed for display. While a query is active every
    /// matching group/folder is auto-expanded so a hit inside a previously
    /// collapsed branch is always painted; the stored user state is untouched
    /// and comes back when the query is cleared.
    pub fn is_collapsed(&self, key: &str) -> bool {
        self.query_is_empty() && self.collapsed.contains(key)
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

    /// `h` on the Catalog list: collapse/expand the folder that contains the
    /// highlighted definition. With no visible row (every group collapsed) it
    /// reopens the first collapsed key, so the list is never keyboard-trapped.
    pub fn toggle_selected_folder(&mut self) -> bool {
        let key = self.selected.as_ref().and_then(|name| {
            self.entries
                .iter()
                .find(|entry| &entry.name == name)
                .map(|entry| {
                    let view = CatalogEntryView::new(entry);
                    if view.folder.is_empty() {
                        view.namespace
                    } else {
                        format!("{}/{}", view.namespace, view.folder)
                    }
                })
        });
        let key = key.or_else(|| self.collapsed.iter().next().cloned());
        let Some(key) = key else {
            return false;
        };
        self.toggle_collapsed(&key);
        self.ensure_selection();
        true
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

    pub fn open_launch(&mut self, name: &str, agents: Vec<AgentChoice>) -> bool {
        let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .cloned()
        else {
            return false;
        };
        self.launch = Some(LaunchForm::with_agents(entry, agents));
        true
    }

    pub fn close_launch(&mut self) {
        self.launch = None;
    }

    /// Append one typed character to the focused text control (a var or the
    /// model override). The agent picker is changed with arrows/clicks.
    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn push_launch_char(&mut self, character: char) {
        if let Some(form) = self.launch.as_mut() {
            form.push_char(character);
        }
    }

    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn backspace_launch(&mut self) {
        if let Some(form) = self.launch.as_mut() {
            form.pop_char();
        }
    }

    /// Enter inside a multiline var inserts a line break instead of submitting.
    #[allow(dead_code)] // Exercised by the model's tests.
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
        field.value.insert("\n");
        field.error = None;
        form.server_error = None;
        true
    }

    /// Focus the launch sheet's agent picker.
    pub fn focus_launch_agent(&mut self) {
        if let Some(form) = self.launch.as_mut() {
            if let Some(index) = form.agent_index() {
                form.focused = index;
            }
        }
    }

    /// Focus the launch sheet's model input.
    pub fn focus_launch_model(&mut self) {
        if let Some(form) = self.launch.as_mut() {
            if let Some(index) = form.model_index() {
                form.focused = index;
            }
        }
    }

    /// Set the launch agent override from a click.
    pub fn set_launch_agent(&mut self, name: &str) {
        if let Some(form) = self.launch.as_mut() {
            form.set_agent(name);
            if let Some(index) = form.agent_index() {
                form.focused = index;
            }
        }
    }

    pub fn cycle_launch_focus(&mut self, delta: isize) {
        if let Some(form) = self.launch.as_mut() {
            form.cycle_focus(delta);
        }
    }

    /// Cycle the focused launch field's fixed choices.
    pub fn cycle_launch_choice(&mut self, delta: isize) -> bool {
        self.launch
            .as_mut()
            .is_some_and(|form| form.cycle_choice(delta))
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

    #[allow(dead_code)] // Exercised by the model's tests.
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
        form.cwd.push_char(character);
        form.server_error = None;
    }

    #[allow(dead_code)] // Exercised by the model's tests.
    pub fn backspace_oneshot(&mut self) {
        let Some(form) = self.oneshot.as_mut() else {
            return;
        };
        form.focused = OneShotFocus::Cwd;
        form.cwd.backspace();
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

/// The three levels of the Catalog tree. Each level is indented one step
/// further from the list edge, so a folder owns the rows painted under it
/// (docs/design/gpui-client.md §2, one dimension at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogTreeLevel {
    /// A repo namespace (`repo:<name>`) or `Global`.
    Namespace,
    /// One directory inside a namespace.
    Folder,
    /// A task-definition or action row inside its folder.
    Entry,
}

impl CatalogTreeLevel {
    /// The zero-based indent step. The view multiplies this by its spacing
    /// step so the tree grows with the theme (and shrinks in compact mode).
    pub fn depth(self) -> usize {
        match self {
            CatalogTreeLevel::Namespace => 0,
            CatalogTreeLevel::Folder => 1,
            CatalogTreeLevel::Entry => 2,
        }
    }
}

/// The disclosure triangle for a collapsible namespace or folder.
pub fn disclosure_glyph(collapsed: bool) -> &'static str {
    if collapsed {
        "▸"
    } else {
        "▾"
    }
}

/// The icon that marks a folder row.
pub const FOLDER_GLYPH: &str = "▣";

/// The icon that marks a task-definition row.
pub const TASK_GLYPH: &str = "▤";

/// The icon that marks an action row. Deliberately not `⚙`, which is the
/// Settings route icon.
pub const ACTION_GLYPH: &str = "⚡";

/// The icon before a definition row: actions get a bolt, everything else is a
/// task definition.
pub fn entry_glyph(kind: &str) -> &'static str {
    if kind == "action" {
        ACTION_GLYPH
    } else {
        TASK_GLYPH
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
            mcp: None,
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
    fn the_active_repo_keeps_local_and_global_entries_and_drops_stale_selection() {
        let mut state = CatalogState {
            entries: vec![
                entry("repo:favetto/triage", Some("favetto"), "sort", vec![]),
                entry("repo:web/release", Some("web"), "ship", vec![]),
                entry("global/hello", None, "say hi", vec![]),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        state.select("repo:web/release");
        state.set_active_repo(Some("favetto".to_string()));

        let groups = state.groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key, "repo:favetto");
        assert_eq!(groups[1].key, "global");
        assert_eq!(state.total_count(), 2);
        assert_eq!(state.matched_count(), 2);
        assert_eq!(state.selected(), Some("repo:favetto/triage"));
        assert!(state.preview().is_none());

        state.select("repo:favetto/triage");
        state.set_active_repo(Some("web".to_string()));
        assert_eq!(state.groups().len(), 2);
        assert_eq!(
            state.selected(),
            Some("repo:web/release"),
            "a hidden selection is re-clamped after a switch"
        );

        state.set_active_repo(None);
        assert_eq!(state.groups().len(), 3);
        assert_eq!(state.total_count(), 3);
    }

    #[test]
    fn h_toggles_the_highlighted_folder_and_recovers_from_an_empty_list() {
        let mut state = CatalogState {
            entries: vec![
                entry(
                    "repo:favetto/gpui/01-scaffold",
                    Some("favetto"),
                    "shell",
                    vec![],
                ),
                entry("repo:favetto/release", Some("favetto"), "ship", vec![]),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        state.select("repo:favetto/gpui/01-scaffold");
        assert!(state.toggle_selected_folder());
        assert!(state.is_collapsed("repo:favetto/gpui"));
        assert_eq!(state.selected(), Some("repo:favetto/release"));

        // Collapse every remaining namespace/folder, then `h` reopens the
        // first one instead of trapping the highlight outside the list.
        state.toggle_collapsed("repo:favetto");
        assert!(state.visible_names().is_empty());
        assert!(state.toggle_selected_folder());
        assert!(!state.visible_names().is_empty());
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
    fn search_matches_only_the_definition_basename() {
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
        for character in "tri".chars() {
            state.push_search_char(character);
        }
        assert_eq!(state.matched_count(), 1);
        assert_eq!(state.selected(), Some("repo:favetto/triage"));

        state.clear_search();
        for character in "cha".chars() {
            state.push_search_char(character);
        }
        assert_eq!(state.matched_count(), 0, "a prompt-only hit must not match");
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn the_hidden_repo_prefix_and_folder_are_not_searchable() {
        let mut state = CatalogState {
            entries: vec![
                entry(
                    "repo:favetto/triage",
                    Some("favetto"),
                    "sort the inbox",
                    vec![],
                ),
                entry(
                    "repo:favetto/gpui/01-scaffold",
                    Some("favetto"),
                    "shell",
                    vec![],
                ),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        assert_eq!(state.total_count(), 2);

        for character in "favetto".chars() {
            state.push_search_char(character);
        }
        assert_eq!(
            state.matched_count(),
            0,
            "repo:favetto is hidden on the row"
        );

        state.clear_search();
        for character in "gpui".chars() {
            state.push_search_char(character);
        }
        assert_eq!(
            state.matched_count(),
            0,
            "the gpui folder header is not part of the row's title"
        );
    }

    #[test]
    fn prompts_and_metadata_are_not_searchable() {
        let mut triage = entry(
            "repo:favetto/triage",
            Some("favetto"),
            "sort the inbox",
            vec![],
        );
        triage.model = Some("opus".to_string());
        triage.agent = Some("claude".to_string());

        let mut state = CatalogState {
            entries: vec![
                triage,
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

        for query in ["inbox", "opus", "claude", "agent"] {
            state.clear_search();
            for character in query.chars() {
                state.push_search_char(character);
            }
            assert_eq!(state.matched_count(), 0, "{query} is not a title");
        }

        // Every term still has to land on the single title field.
        state.clear_search();
        for character in "triage release".chars() {
            state.push_search_char(character);
        }
        assert_eq!(state.matched_count(), 0);
    }

    #[test]
    fn prompt_and_metadata_hits_no_longer_outrank_a_title() {
        let mut state = CatalogState {
            entries: vec![
                entry(
                    "repo:favetto/release-notes",
                    Some("favetto"),
                    "publish a changelog",
                    vec![],
                ),
                entry(
                    "repo:favetto/changelog",
                    Some("favetto"),
                    "write the notes",
                    vec![],
                ),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        for character in "changelog".chars() {
            state.push_search_char(character);
        }
        assert_eq!(
            state.visible_names(),
            vec!["repo:favetto/changelog"],
            "the prompt-only hit is filtered out instead of ranked"
        );
    }

    #[test]
    fn searching_auto_expands_a_collapsed_folder_and_restores_it() {
        let mut state = CatalogState {
            entries: vec![
                entry(
                    "repo:favetto/gpui/01-scaffold",
                    Some("favetto"),
                    "shell",
                    vec![],
                ),
                entry("repo:favetto/release", Some("favetto"), "ship", vec![]),
            ],
            status: CatalogStatus::Loaded,
            ..CatalogState::default()
        };
        state.toggle_collapsed("repo:favetto/gpui");
        assert!(state.is_collapsed("repo:favetto/gpui"));
        assert_eq!(state.visible_names(), vec!["repo:favetto/release"]);

        for character in "scaffold".chars() {
            state.push_search_char(character);
        }
        assert_eq!(
            state.visible_names(),
            vec!["repo:favetto/gpui/01-scaffold"],
            "a hit inside a collapsed folder is visible while searching"
        );
        assert!(state
            .display_index_of("repo:favetto/gpui/01-scaffold")
            .is_some());

        state.clear_search();
        assert!(state.is_collapsed("repo:favetto/gpui"));
        assert_eq!(state.visible_names(), vec!["repo:favetto/release"]);
        assert_eq!(
            state.display_index_of("repo:favetto/gpui/01-scaffold"),
            None
        );
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

    fn agent(name: &str, available: bool, default: bool) -> AgentChoice {
        AgentChoice {
            name: name.to_string(),
            display_name: name.to_string(),
            available,
            default,
        }
    }

    #[test]
    fn launch_form_seeds_agent_and_model_from_the_definition() {
        let form = LaunchForm::with_agents(
            entry("plan", None, "plan", vec![]),
            vec![agent("opencode", true, true)],
        );
        assert!(form.show_overrides());
        assert_eq!(form.agent.as_deref(), Some("opencode"));
        assert_eq!(form.model, "opencode-go/deepseek");
        assert_eq!(
            form.overrides(),
            (
                Some("opencode".to_string()),
                Some("opencode-go/deepseek".to_string())
            )
        );
    }

    #[test]
    fn launch_form_falls_back_to_the_daemon_default_agent() {
        let mut e = entry("plan", None, "plan", vec![]);
        e.agent = None;
        e.provider = None;
        e.model = None;
        let form = LaunchForm::with_agents(
            e,
            vec![
                agent("unavailable", false, true),
                agent("fallback", true, true),
            ],
        );
        assert_eq!(form.agent.as_deref(), Some("fallback"));
        assert_eq!(form.model, "");
        assert_eq!(form.overrides(), (Some("fallback".to_string()), None));
    }

    #[test]
    fn launch_form_cycles_the_agent_and_types_a_model_override() {
        let mut form = LaunchForm::with_agents(
            entry("plan", None, "plan", vec![]),
            vec![agent("one", true, true), agent("two", true, false)],
        );
        assert_eq!(form.agent_index(), Some(0));
        assert_eq!(form.model_index(), Some(1));
        // With no vars the agent picker is focused first; arrows choose an
        // agent and Tab moves on to the model input.
        assert_eq!(form.focused, 0);
        assert!(form.cycle_choice(1));
        assert_eq!(form.agent.as_deref(), Some("two"));
        form.cycle_focus(1);
        assert_eq!(form.focused, 1);
        form.push_char('x');
        assert_eq!(form.model, "opencode-go/deepseekx");
        form.pop_char();
        assert_eq!(form.model, "opencode-go/deepseek");
        form.cycle_focus(1);
        assert_eq!(form.focused, 0);
    }

    #[test]
    fn action_launch_forms_hide_the_overrides() {
        let mut e = entry("gate", None, "gate", vec![]);
        e.kind = "action".to_string();
        let mut form = LaunchForm::with_agents(e, vec![agent("one", true, true)]);
        assert!(!form.show_overrides());
        assert_eq!(form.overrides(), (None, None));
        assert!(!form.cycle_choice(1));
        form.push_char('x');
        assert_eq!(form.focused, 0);
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

        form.fields[0].value = TextInput::new("  ".to_string());
        assert!(!form.validate());

        form.fields[0].value = TextInput::new("42".to_string());
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
        form.fields[0].value = TextInput::new("many".to_string());
        form.fields[1].value = TextInput::new("medium".to_string());
        assert!(!form.validate());
        assert_eq!(form.fields[0].error.as_deref(), Some("must be an integer"));
        assert_eq!(
            form.fields[1].error.as_deref(),
            Some("must be one of: fast, slow")
        );

        form.fields[0].value = TextInput::new("3".to_string());
        form.fields[1].value = TextInput::new("fast".to_string());
        assert!(form.validate());
        assert_eq!(
            form.input().unwrap(),
            serde_json::json!({ "count": 3, "mode": "fast" })
        );
    }

    #[test]
    fn a_launch_var_edits_in_the_middle_and_accepts_multiline_newlines() {
        let mut multiline = var("note", false);
        multiline.multiline = true;
        let mut form = LaunchForm::new(entry("plan", None, "plan", vec![multiline]));
        form.focused = 0;
        for character in "abcd".chars() {
            form.push_char(character);
        }
        form.fields[0].value.move_to(2);
        form.push_char('X');
        assert_eq!(form.fields[0].value, "abXcd");
        form.pop_char();
        assert_eq!(form.fields[0].value, "abcd");
        form.fields[0].value.move_to(2);
        form.fields[0].value.insert("\n");
        assert_eq!(form.fields[0].value, "ab\ncd");
    }

    #[test]
    fn the_launch_sheet_cycles_declared_choices_with_the_arrow_keys() {
        let mut choice_var = var("mode", false);
        choice_var.choices = Some(vec!["fast".to_string(), "slow".to_string()]);
        let mut form = LaunchForm::new(entry("plan", None, "plan", vec![choice_var]));
        form.focused = 0;
        assert!(form.cycle_choice(1));
        assert_eq!(form.fields[0].value, "fast");
        assert!(form.cycle_choice(1));
        assert_eq!(form.fields[0].value, "slow");
        assert!(form.cycle_choice(1));
        assert_eq!(form.fields[0].value, "slow", "clamped at the end");
        assert!(form.cycle_choice(-1));
        assert_eq!(form.fields[0].value, "fast");

        let mut plain = LaunchForm::new(entry("plan", None, "plan", vec![var("issue", false)]));
        assert!(!plain.cycle_choice(1));
    }

    #[test]
    fn optional_blank_vars_are_omitted_from_input() {
        let mut form = LaunchForm::new(entry(
            "plan",
            None,
            "plan",
            vec![var("issue", true), var("note", false)],
        ));
        form.fields[0].value = TextInput::new("7".to_string());
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

    #[test]
    fn tree_levels_indent_one_step_at_a_time() {
        assert_eq!(CatalogTreeLevel::Namespace.depth(), 0);
        assert_eq!(CatalogTreeLevel::Folder.depth(), 1);
        assert_eq!(CatalogTreeLevel::Entry.depth(), 2);
    }

    #[test]
    fn folder_and_definition_rows_use_distinct_glyphs() {
        assert_eq!(disclosure_glyph(false), "▾");
        assert_eq!(disclosure_glyph(true), "▸");
        assert_eq!(FOLDER_GLYPH, "▣");
        assert_eq!(entry_glyph("agent"), TASK_GLYPH);
        assert_eq!(entry_glyph(""), TASK_GLYPH);
        assert_eq!(entry_glyph("action"), ACTION_GLYPH);
        assert_eq!(TASK_GLYPH, "▤");
        assert_eq!(ACTION_GLYPH, "⚡");
        assert_ne!(
            entry_glyph("action"),
            "⚙",
            "the Settings route icon must not mean an action here"
        );
    }

    #[test]
    fn child_rows_show_the_basename_under_their_folder() {
        let nested = CatalogEntryView::new(&entry(
            "repo:favetto/gpui/01-scaffold",
            Some("favetto"),
            "shell",
            vec![],
        ));
        assert_eq!(nested.folder, "gpui");
        assert_eq!(nested.label, "gpui/01-scaffold");
        assert_eq!(nested.basename(), "01-scaffold");

        let root = CatalogEntryView::new(&entry(
            "repo:favetto/release",
            Some("favetto"),
            "ship",
            vec![],
        ));
        assert_eq!(root.folder, "");
        assert_eq!(root.basename(), "release");
    }
}
