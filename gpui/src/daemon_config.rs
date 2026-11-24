//! The daemon's global `config.toml` draft: form, revision and apply state.
//!
//! The default surface is the schema-backed form in [`crate::daemon_form`].
//! Every value is edited with a control appropriate to its type; the form
//! patches the values into the `toml_edit` document the daemon returned, so
//! comments, unknown keys, ordering and file mode survive. A raw TOML editor
//! remains available for repairs, comments and client-only `[tui]` settings.
//!
//! Saving always sends the whole draft to `config.set`, which keeps the
//! revision guard, validation, preflight, atomic write and secret restoration
//! in one place. This module is GPUI-free and unit-tested without a window.

use favetto_wire::rpc::{ConfigGetResult, ConfigOverride, ConfigSetResult};

use crate::daemon_form::{ConfigForm, RemoveAction};
use crate::text_input::TextInput;

/// What the daemon reported after the last load.
#[derive(Debug, Clone, Default)]
pub struct DaemonConfigState {
    form: Option<ConfigForm>,
    /// The raw TOML buffer used by the advanced/repair editor.
    raw: TextInput,
    /// Whether the advanced raw editor is on. A malformed file always opens
    /// here so the user can repair it without blocking Settings.
    advanced: bool,
    /// The raw TOML the daemon last returned, used by Reload and dirty checks.
    snapshot: Option<String>,
    path: Option<String>,
    revision: Option<String>,
    exists: bool,
    loading: bool,
    saving: bool,
    errors: Vec<String>,
    warnings: Vec<String>,
    overrides: Vec<ConfigOverride>,
    restart_required: Vec<String>,
    live_sections: Vec<String>,
    loaded: bool,
    load_error: Option<String>,
}

impl DaemonConfigState {
    pub fn form(&self) -> Option<&ConfigForm> {
        self.form.as_ref()
    }

    pub fn form_mut(&mut self) -> Option<&mut ConfigForm> {
        self.form.as_mut()
    }

    pub fn is_advanced(&self) -> bool {
        self.advanced || self.form.is_none()
    }

    pub fn set_advanced(&mut self, advanced: bool) {
        if advanced == self.advanced {
            return;
        }
        if advanced {
            // Carry the form's current document into the raw editor so the
            // user sees exactly what Save would send.
            if let Some(form) = &self.form {
                self.raw.set_value(form.raw());
            }
            self.advanced = true;
        } else {
            // Only leave the repair editor once the file parses.
            if let Some(form) = ConfigForm::parse(self.raw.value()) {
                self.form = Some(form);
                self.advanced = false;
            }
        }
    }

    /// The advanced raw editor's buffer. The form renders its own inputs from
    /// [`Self::form`].
    pub fn draft(&self) -> &TextInput {
        &self.raw
    }

    pub fn draft_mut(&mut self) -> &mut TextInput {
        &mut self.raw
    }

    pub fn insert_newline(&mut self) {
        self.raw.insert("\n");
        self.after_edit();
    }

    /// Mark the raw draft edited so a subsequent load cannot clobber it.
    pub fn after_edit(&mut self) {
        self.advanced = true;
        self.loaded = true;
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn begin_load(&mut self) {
        self.loading = true;
        self.load_error = None;
    }

    pub fn fail_load(&mut self, message: String) {
        self.loading = false;
        self.load_error = Some(message);
    }

    /// Fold a `config.get` result. A dirty draft is never overwritten; the path,
    /// revision and diagnostics still refresh.
    pub fn apply_get(&mut self, result: &ConfigGetResult) {
        // Decide whether the draft may be replaced before the new snapshot is
        // recorded; comparing against it afterwards would always look dirty.
        let clean = !self.is_dirty();
        self.loading = false;
        self.load_error = None;
        self.loaded = true;
        self.path = Some(result.path.clone());
        self.revision = Some(result.revision.clone());
        self.exists = result.exists;
        let parsed = ConfigForm::parse(&result.raw);
        if clean {
            self.raw.set_value(result.raw.clone());
            self.form = parsed;
            // A malformed file is not authoritative: keep the repair editor
            // visible until it parses.
            self.advanced = !result.parse_errors.is_empty() || self.form.is_none();
        } else if self.form.is_none() && parsed.is_some() {
            self.form = parsed;
        }
        if let Some(form) = &mut self.form {
            form.set_daemon_errors(&result.parse_errors);
        }
        self.snapshot = Some(result.raw.clone());
        self.errors = result.parse_errors.clone();
        self.warnings = result.warnings.clone();
        self.overrides = result.overrides.clone();
        self.restart_required = result.restart_required.clone();
        self.live_sections.clear();
    }

    pub fn begin_save(&mut self) {
        self.saving = true;
        self.errors.clear();
    }

    pub fn fail_save(&mut self, message: String) {
        self.saving = false;
        self.errors = vec![message];
    }

    /// Fold a `config.set` result. A rejected save keeps the draft and the
    /// daemon's current revision so the next attempt is guarded correctly.
    pub fn apply_set(&mut self, result: &ConfigSetResult) {
        self.saving = false;
        self.path = Some(result.path.clone());
        self.revision = Some(result.revision.clone());
        self.errors = result.errors.clone();
        self.warnings = result.warnings.clone();
        self.restart_required = result.restart_required.clone();
        self.live_sections = result.live_sections.clone();
        if let Some(form) = &mut self.form {
            form.set_daemon_errors(&result.errors);
        }
        if result.saved {
            self.snapshot = Some(self.draft_toml());
            self.exists = true;
        }
    }

    /// Discard edits and restore the last loaded snapshot.
    pub fn reset(&mut self) {
        if let Some(snapshot) = &self.snapshot {
            self.raw.set_value(snapshot.clone());
            self.form = ConfigForm::parse(snapshot);
            self.advanced = self.form.is_none();
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    /// The exact draft Save sends.
    pub fn draft_toml(&self) -> String {
        if self.is_advanced() {
            self.raw.value().to_string()
        } else {
            self.form
                .as_ref()
                .map(ConfigForm::raw)
                .unwrap_or_else(|| self.raw.value().to_string())
        }
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn revision(&self) -> Option<&str> {
        self.revision.as_deref()
    }

    pub fn exists(&self) -> bool {
        self.exists
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn is_saving(&self) -> bool {
        self.saving
    }

    pub fn has_loaded(&self) -> bool {
        self.loaded
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn overrides(&self) -> &[ConfigOverride] {
        &self.overrides
    }

    pub fn restart_required(&self) -> &[String] {
        &self.restart_required
    }

    pub fn live_sections(&self) -> &[String] {
        &self.live_sections
    }

    /// Dirty against the last snapshot the daemon returned.
    pub fn is_dirty(&self) -> bool {
        let current = self.draft_toml();
        self.snapshot
            .as_deref()
            .is_some_and(|snapshot| snapshot != current)
    }

    /// The save action is available for a loaded, edited, idle draft with no
    /// client-side validation errors.
    pub fn can_save(&self) -> bool {
        self.loaded
            && !self.saving
            && !self.loading
            && self.is_dirty()
            && !self.form.as_ref().is_some_and(ConfigForm::has_errors)
    }

    /// The number of form/keyboard stops, used to clamp Tab focus.
    pub fn control_count(&self) -> usize {
        self.form
            .as_ref()
            .map(ConfigForm::control_count)
            .unwrap_or(0)
    }

    /// Apply a text edit for one form control.
    pub fn after_control_edit(&mut self, control: usize) {
        if let Some(form) = &mut self.form {
            form.after_control_edit(control);
            self.errors = form.errors().to_vec();
        }
        self.live_sections.clear();
    }

    pub fn toggle_control(&mut self, control: usize) -> bool {
        let handled = self.form.as_mut().is_some_and(|form| form.toggle(control));
        if handled {
            self.errors.clear();
            self.live_sections.clear();
        }
        handled
    }

    pub fn reset_field(&mut self, field: usize) {
        if let Some(form) = &mut self.form {
            form.reset_field(field);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn add(&mut self, action: &crate::daemon_form::AddAction) {
        if let Some(form) = &mut self.form {
            form.add(action);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn remove(&mut self, action: &RemoveAction) {
        if let Some(form) = &mut self.form {
            form.remove(action);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn add_list_item(&mut self, field: usize) {
        self.add(&crate::daemon_form::AddAction::ListItem(field));
    }

    pub fn set_list_choice(&mut self, field: usize, row: usize, value: String) {
        if let Some(form) = &mut self.form {
            form.set_list_choice(field, row, value);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn remove_list_item(&mut self, field: usize, row: usize) {
        if let Some(form) = &mut self.form {
            form.remove_list_item(field, row);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn add_map_row(&mut self, field: usize) {
        self.add(&crate::daemon_form::AddAction::MapRow(field));
    }

    pub fn remove_map_row(&mut self, field: usize, row: usize) {
        if let Some(form) = &mut self.form {
            form.remove_map_row(field, row);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn clear_secret(&mut self, field: usize, row: usize) {
        if let Some(form) = &mut self.form {
            form.clear_secret(field, row);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn set_enum(&mut self, field: usize, value: Option<String>) {
        if let Some(form) = &mut self.form {
            form.set_enum(field, value);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn set_bool(&mut self, field: usize, value: bool) {
        if let Some(form) = &mut self.form {
            form.set_bool(field, value);
        }
        self.errors.clear();
        self.live_sections.clear();
    }

    pub fn status(&self) -> DaemonConfigStatus {
        if self.saving {
            DaemonConfigStatus::Saving
        } else if self.loading {
            DaemonConfigStatus::Loading
        } else if !self.errors.is_empty() || self.form.as_ref().is_some_and(ConfigForm::has_errors)
        {
            DaemonConfigStatus::Invalid
        } else if self.is_dirty() {
            DaemonConfigStatus::Dirty
        } else if self.loaded {
            DaemonConfigStatus::Saved
        } else {
            DaemonConfigStatus::Idle
        }
    }
}

/// The one-line status shown beside the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonConfigStatus {
    Idle,
    Loading,
    Saving,
    Dirty,
    Invalid,
    Saved,
}

impl DaemonConfigStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Not loaded",
            Self::Loading => "Loading…",
            Self::Saving => "Saving…",
            Self::Dirty => "Unsaved changes",
            Self::Invalid => "Invalid draft",
            Self::Saved => "Valid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(raw: &str, revision: &str) -> ConfigGetResult {
        ConfigGetResult {
            path: "/home/u/.config/favetto/config.toml".to_string(),
            raw: raw.to_string(),
            revision: revision.to_string(),
            exists: true,
            parse_errors: Vec::new(),
            warnings: Vec::new(),
            overrides: Vec::new(),
            restart_required: Vec::new(),
            effective: None,
        }
    }

    #[test]
    fn a_fresh_snapshot_seeds_the_form_and_stays_clean() {
        let mut state = DaemonConfigState::default();
        state.begin_load();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        assert_eq!(state.draft_toml(), "[daemon]\n");
        assert_eq!(state.revision(), Some("r1"));
        assert!(!state.is_dirty());
        assert_eq!(state.status(), DaemonConfigStatus::Saved);
        assert!(state.form().is_some());
        assert!(!state.is_advanced());
    }

    #[test]
    fn edits_are_dirty_and_are_not_clobbered_by_a_background_load() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        let control = state
            .form()
            .and_then(|form| {
                form.fields()
                    .iter()
                    .position(|field| field.path == "daemon.listen")
            })
            .and_then(|field| {
                state
                    .form()
                    .and_then(|form| form.field_controls(field).first().map(|(index, _)| *index))
            })
            .unwrap();
        state
            .form_mut()
            .unwrap()
            .text_input_mut(control)
            .unwrap()
            .set_value("127.0.0.1:1".to_string());
        state.after_control_edit(control);
        assert!(state.is_dirty());
        assert_eq!(state.status(), DaemonConfigStatus::Dirty);

        state.apply_get(&snapshot("[daemon]\nlisten = \"old\"\n", "r2"));
        assert!(state.draft_toml().contains("127.0.0.1:1"));
        // The revision still tracks the daemon for the save guard.
        assert_eq!(state.revision(), Some("r2"));
    }

    #[test]
    fn a_rejected_save_keeps_the_draft_and_reports_errors() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.set_advanced(true);
        state.raw.set_value("[daemon]\nlisten = 42\n".to_string());
        state.begin_save();
        assert_eq!(state.status(), DaemonConfigStatus::Saving);
        state.apply_set(&ConfigSetResult {
            saved: false,
            path: "/home/u/.config/favetto/config.toml".to_string(),
            revision: "r1".to_string(),
            errors: vec!["invalid type: integer".to_string()],
            warnings: Vec::new(),
            changed_sections: Vec::new(),
            restart_required: Vec::new(),
            live_sections: Vec::new(),
        });
        assert!(state.is_dirty());
        assert_eq!(state.errors().len(), 1);
        assert_eq!(state.status(), DaemonConfigStatus::Invalid);
        assert_eq!(state.revision(), Some("r1"));
    }

    #[test]
    fn a_successful_save_clears_dirty_and_records_restart_sections() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.set_advanced(true);
        state
            .raw
            .set_value("[daemon]\nlisten = \"127.0.0.1:9999\"\n".to_string());
        state.apply_set(&ConfigSetResult {
            saved: true,
            path: "/home/u/.config/favetto/config.toml".to_string(),
            revision: "r2".to_string(),
            errors: Vec::new(),
            warnings: Vec::new(),
            changed_sections: vec!["daemon".to_string()],
            restart_required: vec!["daemon".to_string()],
            live_sections: Vec::new(),
        });
        assert!(!state.is_dirty());
        assert_eq!(state.revision(), Some("r2"));
        assert_eq!(state.restart_required(), ["daemon"]);
        assert_eq!(state.status(), DaemonConfigStatus::Saved);
    }

    #[test]
    fn reset_restores_the_last_snapshot() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.set_advanced(true);
        state.raw.set_value("junk".to_string());
        state.reset();
        assert_eq!(state.draft_toml(), "[daemon]\n");
        assert!(!state.is_dirty());
    }

    #[test]
    fn newlines_are_inserted_in_the_repair_editor_and_count_as_an_edit() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[daemon]\n", "r1"));
        state.set_advanced(true);
        state.insert_newline();
        assert!(state.draft_toml().ends_with("\n\n"));
        assert!(state.is_dirty());
    }

    #[test]
    fn a_malformed_file_opens_in_the_repair_editor() {
        let mut state = DaemonConfigState::default();
        let mut result = snapshot("[daemon\n", "r1");
        result.parse_errors = vec!["expected a table header".to_string()];
        state.apply_get(&result);
        assert!(state.is_advanced());
        assert!(state.form().is_none());
        assert_eq!(state.draft_toml(), "[daemon\n");
    }

    #[test]
    fn leaving_the_repair_editor_requires_a_parsable_document() {
        let mut state = DaemonConfigState::default();
        let mut result = snapshot("[daemon\n", "r1");
        result.parse_errors = vec!["expected a table header".to_string()];
        state.apply_get(&result);
        state.set_advanced(false);
        assert!(state.is_advanced(), "still invalid");
        state.raw.set_value("[daemon]\n".to_string());
        state.set_advanced(false);
        assert!(!state.is_advanced());
        assert!(state.form().is_some());
    }

    #[test]
    fn required_fields_block_save_until_filled() {
        let mut state = DaemonConfigState::default();
        state.apply_get(&snapshot("[agents.broken]\ncommand = \"\"\n", "r1"));
        assert!(state.status() == DaemonConfigStatus::Invalid);
        assert!(!state.can_save());
    }
}
