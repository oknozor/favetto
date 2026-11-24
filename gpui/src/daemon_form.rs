//! A schema-backed form over the daemon's global `config.toml`.
//!
//! The form is generated from the committed JSON schema
//! (`docs/public/favetto-schema.json`) so labels, descriptions, defaults and
//! enum choices cannot drift from [`FavettoConfig`]. Values are patched into
//! the `toml_edit` document the daemon returned, which keeps comments, unknown
//! keys, ordering and the raw repair path intact. Saving still sends the whole
//! draft to `config.set`, so validation, the revision guard, atomic writes and
//! secret restoration remain the daemon's job.
//!
//! This module is GPUI-free and unit-tested without a window.

use std::collections::BTreeSet;

use serde_json::Value as JsonValue;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::text_input::TextInput;

/// The generated schema shipped with the docs. It is the single source for
/// field docs, defaults and enum choices.
const SCHEMA_JSON: &str = include_str!("../../docs/public/favetto-schema.json");

/// One root section of `FavettoConfig`, in the order the form renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSection {
    Agent,
    Agents,
    Daemon,
    Executor,
    Git,
    Auth,
    Repos,
    Mcp,
    Tui,
}

impl ConfigSection {
    pub const ALL: [Self; 9] = [
        Self::Agent,
        Self::Agents,
        Self::Daemon,
        Self::Executor,
        Self::Git,
        Self::Auth,
        Self::Repos,
        Self::Mcp,
        Self::Tui,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Agents => "agents",
            Self::Daemon => "daemon",
            Self::Executor => "executor",
            Self::Git => "git",
            Self::Auth => "auth",
            Self::Repos => "repos",
            Self::Mcp => "mcp",
            Self::Tui => "tui",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "DEFAULT AGENT",
            Self::Agents => "AGENTS",
            Self::Daemon => "DAEMON",
            Self::Executor => "EXECUTOR",
            Self::Git => "GIT",
            Self::Auth => "AUTHENTICATION",
            Self::Repos => "REPOSITORIES (STARTUP SEED)",
            Self::Mcp => "MCP SERVERS (STARTUP SEED)",
            Self::Tui => "TUI (CLIENT-SIDE)",
        }
    }

    pub fn note(self) -> Option<&'static str> {
        match self {
            Self::Agent => Some(
                "The default agent for catalog tasks and new sessions. Choose one of the agents declared below, or leave it unset.",
            ),
            Self::Agents => Some(
                "External agents Favetto may launch. Each command must start the agent in its ACP mode.",
            ),
            Self::Daemon => Some(
                "Boot-only networking and storage. Changing listen, socket, tasks_dir or data_dir can make the running daemon unreachable after a restart.",
            ),
            Self::Executor => Some(
                "Task concurrency, isolation, output retention and automatic retries.",
            ),
            Self::Git => Some(
                "Identity and signing for commits made by agent processes. The operator's own git config is never modified.",
            ),
            Self::Auth => Some(
                "Username/password login policy. Login attempts are rate-limited per user and client; require_tls rejects non-loopback plaintext logins.",
            ),
            Self::Repos => Some(
                "This only seeds the known-repos registry at daemon startup. Use the live repos.* RPCs to add, remove or activate repositories now.",
            ),
            Self::Mcp => Some(
                "This only seeds the MCP registry at startup. Use the live mcp.* RPCs to change servers now. env/headers values are secrets; the daemon stores them outside this file.",
            ),
            Self::Tui => Some(
                "The daemon parses [tui] but only the TUI host reads it. Edit it in the advanced raw view; it is not a daemon setting.",
            ),
        }
    }
}

/// One selectable enum value. The description comes from the schema branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumChoice {
    pub value: String,
    pub description: Option<String>,
}

impl EnumChoice {
    pub fn label(&self) -> &str {
        &self.value
    }
}

/// A field's editable control.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text {
        input: TextInput,
        optional: bool,
        numeric: bool,
        min: Option<u64>,
    },
    Bool {
        value: bool,
        present: bool,
    },
    Enum {
        choices: Vec<EnumChoice>,
        selected: Option<String>,
        optional: bool,
    },
    List {
        rows: Vec<TextInput>,
        choices: Option<Vec<EnumChoice>>,
        optional: bool,
    },
    Map {
        rows: Vec<MapRow>,
        secret: bool,
    },
}

/// One key/value row of a map field (`env`, `headers`, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct MapRow {
    pub key: TextInput,
    pub value: TextInput,
    /// The key currently stored in the document (differs while the user types).
    pub original_key: String,
    /// A literal secret left untouched. The document keeps the redaction
    /// sentinel; the input starts empty and masked until the user types.
    pub unchanged: bool,
    pub key_error: Option<String>,
}

/// One form field, corresponding to a leaf property in the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigField {
    /// Dotted path used for errors and `FAVETTO_*` override badges.
    pub path: String,
    pub label: String,
    pub help: String,
    pub section: ConfigSection,
    pub group: Option<usize>,
    pub default: Option<String>,
    pub required: bool,
    pub boot_only: bool,
    pub secret: bool,
    pub kind: FieldKind,
    pub error: Option<String>,
}

impl ConfigField {
    pub fn is_present(&self) -> bool {
        match &self.kind {
            FieldKind::Text { .. } => !self.value_is_empty(),
            FieldKind::Bool { present, .. } => *present,
            FieldKind::Enum { selected, .. } => selected.is_some(),
            FieldKind::List { rows, .. } => !rows.is_empty(),
            FieldKind::Map { rows, .. } => !rows.is_empty(),
        }
    }

    fn value_is_empty(&self) -> bool {
        match &self.kind {
            FieldKind::Text { input, .. } => input.value().is_empty(),
            _ => false,
        }
    }
}

/// One removable/addable nested collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigGroup {
    pub title: String,
    pub section: ConfigSection,
    pub depth: u8,
    pub add: Option<AddAction>,
    pub remove: Option<RemoveAction>,
}

/// A structural action exposed beside a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAction {
    Agent,
    RepoSeed,
    McpServer,
    ListItem(usize),
    MapRow(usize),
}

impl AddAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Agent => "Add agent",
            Self::RepoSeed => "Add repository",
            Self::McpServer => "Add MCP server",
            Self::ListItem(_) => "Add entry",
            Self::MapRow(_) => "Add entry",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveAction {
    Agent(String),
    RepoSeed(usize),
    McpServer(usize),
}

/// The form's render order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutItem {
    Section(ConfigSection),
    Group(usize),
    Field(usize),
    EndGroup(usize),
}

/// One keyboard/caret stop. Text targets refer to these indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRef {
    Scalar(usize),
    ListItem(usize, usize),
    MapKey(usize, usize),
    MapValue(usize, usize),
    Toggle(usize),
}

/// The editable daemon config form.
#[derive(Debug, Clone)]
pub struct ConfigForm {
    doc: DocumentMut,
    fields: Vec<ConfigField>,
    groups: Vec<ConfigGroup>,
    layout: Vec<LayoutItem>,
    errors: Vec<String>,
}

impl ConfigForm {
    /// Parse `raw` into a form. Returns `None` when the document does not
    /// parse, so the caller can keep the raw repair editor instead.
    pub fn parse(raw: &str) -> Option<Self> {
        let doc = raw.parse::<DocumentMut>().ok()?;
        let schema: JsonValue = serde_json::from_str(SCHEMA_JSON).ok()?;
        let mut form = Self {
            doc,
            fields: Vec::new(),
            groups: Vec::new(),
            layout: Vec::new(),
            errors: Vec::new(),
        };
        form.build(&schema);
        Some(form)
    }

    pub fn raw(&self) -> String {
        self.doc.to_string()
    }

    pub fn fields(&self) -> &[ConfigField] {
        &self.fields
    }

    pub fn field(&self, index: usize) -> Option<&ConfigField> {
        self.fields.get(index)
    }

    pub fn groups(&self) -> &[ConfigGroup] {
        &self.groups
    }

    pub fn layout(&self) -> &[LayoutItem] {
        &self.layout
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty() || self.fields.iter().any(|field| field.error.is_some())
    }

    /// Every focus/caret stop in render order.
    pub fn controls(&self) -> Vec<ControlRef> {
        let mut controls = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            match &field.kind {
                FieldKind::Text { .. } => controls.push(ControlRef::Scalar(index)),
                FieldKind::Bool { .. } | FieldKind::Enum { .. } => {
                    controls.push(ControlRef::Toggle(index))
                }
                FieldKind::List { rows, .. } => {
                    controls.extend((0..rows.len()).map(|row| ControlRef::ListItem(index, row)));
                }
                FieldKind::Map { rows, .. } => {
                    for row in 0..rows.len() {
                        controls.push(ControlRef::MapKey(index, row));
                        controls.push(ControlRef::MapValue(index, row));
                    }
                }
            }
        }
        controls
    }

    pub fn control_count(&self) -> usize {
        self.controls().len()
    }

    pub fn control(&self, index: usize) -> Option<ControlRef> {
        self.controls().get(index).copied()
    }

    /// The keyboard stops belonging to one field, with their global indices.
    pub fn field_controls(&self, field: usize) -> Vec<(usize, ControlRef)> {
        self.controls()
            .into_iter()
            .enumerate()
            .filter(|(_, control)| control_field(*control) == field)
            .collect()
    }

    pub fn text_input(&self, control: usize) -> Option<&TextInput> {
        let control = self.control(control)?;
        match control {
            ControlRef::Scalar(field) => match &self.fields.get(field)?.kind {
                FieldKind::Text { input, .. } => Some(input),
                _ => None,
            },
            ControlRef::ListItem(field, row) => match &self.fields.get(field)?.kind {
                FieldKind::List { rows, .. } => rows.get(row),
                _ => None,
            },
            ControlRef::MapKey(field, row) => match &self.fields.get(field)?.kind {
                FieldKind::Map { rows, .. } => rows.get(row).map(|row| &row.key),
                _ => None,
            },
            ControlRef::MapValue(field, row) => match &self.fields.get(field)?.kind {
                FieldKind::Map { rows, .. } => rows.get(row).map(|row| &row.value),
                _ => None,
            },
            ControlRef::Toggle(_) => None,
        }
    }

    pub fn text_input_mut(&mut self, control: usize) -> Option<&mut TextInput> {
        let control = self.control(control)?;
        match control {
            ControlRef::Scalar(field) => match &mut self.fields.get_mut(field)?.kind {
                FieldKind::Text { input, .. } => Some(input),
                _ => None,
            },
            ControlRef::ListItem(field, row) => match &mut self.fields.get_mut(field)?.kind {
                FieldKind::List { rows, .. } => rows.get_mut(row),
                _ => None,
            },
            ControlRef::MapKey(field, row) => match &mut self.fields.get_mut(field)?.kind {
                FieldKind::Map { rows, .. } => rows.get_mut(row).map(|row| &mut row.key),
                _ => None,
            },
            ControlRef::MapValue(field, row) => match &mut self.fields.get_mut(field)?.kind {
                FieldKind::Map { rows, .. } => rows.get_mut(row).map(|row| &mut row.value),
                _ => None,
            },
            ControlRef::Toggle(_) => None,
        }
    }

    /// Apply a text edit to the document. Invalid numbers/keys leave the
    /// document untouched and attach an error to the field.
    pub fn after_control_edit(&mut self, control: usize) {
        let Some(control_ref) = self.control(control) else {
            return;
        };
        match control_ref {
            ControlRef::Scalar(field) => self.apply_scalar(field),
            ControlRef::ListItem(field, row) => self.apply_list_item(field, row),
            ControlRef::MapKey(field, row) => self.apply_map_key(field, row),
            ControlRef::MapValue(field, row) => self.apply_map_value(field, row),
            ControlRef::Toggle(_) => {}
        }
        self.revalidate();
    }

    /// Enter/Space on a toggle. Returns whether the control was handled.
    pub fn toggle(&mut self, control: usize) -> bool {
        let Some(control_ref) = self.control(control) else {
            return false;
        };
        let ControlRef::Toggle(field) = control_ref else {
            return false;
        };
        let path = self.fields[field].path.clone();
        match &self.fields[field].kind {
            FieldKind::Bool { present, value } => {
                let next = if *present {
                    !*value
                } else {
                    !default_bool(&path)
                };
                set_value(&mut self.doc, &self.fields[field].path, Value::from(next));
                self.fields[field].kind = FieldKind::Bool {
                    value: next,
                    present: true,
                };
            }
            FieldKind::Enum {
                choices,
                selected,
                optional,
            } => {
                let choices: Vec<String> =
                    choices.iter().map(|choice| choice.value.clone()).collect();
                let next = next_enum(&choices, selected.as_deref(), *optional);
                match next {
                    Some(value) => {
                        set_value(
                            &mut self.doc,
                            &self.fields[field].path,
                            Value::from(value.clone()),
                        );
                        if let FieldKind::Enum { selected, .. } = &mut self.fields[field].kind {
                            *selected = Some(value);
                        }
                    }
                    None => {
                        remove_path(&mut self.doc, &self.fields[field].path);
                        if let FieldKind::Enum { selected, .. } = &mut self.fields[field].kind {
                            *selected = None;
                        }
                    }
                }
            }
            _ => return false,
        }
        self.revalidate();
        true
    }

    pub fn set_enum(&mut self, field: usize, value: Option<String>) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        match value {
            Some(value) => {
                set_value(&mut self.doc, &path, Value::from(value.clone()));
                if let FieldKind::Enum { selected, .. } = &mut self.fields[field].kind {
                    *selected = Some(value);
                }
            }
            None => {
                remove_path(&mut self.doc, &path);
                if let FieldKind::Enum { selected, .. } = &mut self.fields[field].kind {
                    *selected = None;
                }
            }
        }
        self.revalidate();
    }

    pub fn set_bool(&mut self, field: usize, value: bool) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        set_value(&mut self.doc, &path, Value::from(value));
        if let FieldKind::Bool {
            value: current,
            present,
        } = &mut self.fields[field].kind
        {
            *current = value;
            *present = true;
        }
        self.revalidate();
    }

    /// Remove a key so the schema default applies again.
    pub fn reset_field(&mut self, field: usize) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        remove_path(&mut self.doc, &path);
        self.rebuild();
    }

    pub fn remove(&mut self, action: &RemoveAction) {
        match action {
            RemoveAction::Agent(name) => {
                if let Some(table) = self.doc.get_mut("agents").and_then(Item::as_table_mut) {
                    table.remove(name);
                    if table.is_empty() {
                        self.doc.as_table_mut().remove("agents");
                    }
                }
            }
            RemoveAction::RepoSeed(index) => self.remove_array_table("repos.known", *index),
            RemoveAction::McpServer(index) => self.remove_array_table("mcp.servers", *index),
        }
        self.rebuild();
    }

    pub fn add(&mut self, action: &AddAction) {
        match action {
            AddAction::Agent => {
                let name = unique_key(self.agent_names(), "agent");
                let table = self.doc.as_table_mut();
                if !table.contains_key("agents") {
                    table.insert("agents", Item::Table(Table::new()));
                }
                if let Some(agents) = table.get_mut("agents").and_then(item_as_table_mut) {
                    let mut agent = Table::new();
                    agent.insert("command", Item::Value(Value::from("")));
                    agents.insert(&name, Item::Table(agent));
                }
            }
            AddAction::RepoSeed => self.push_array_table("repos.known", repo_seed_table()),
            AddAction::McpServer => self.push_array_table("mcp.servers", mcp_server_table("")),
            AddAction::ListItem(field) => {
                let Some(config_field) = self.fields.get(*field) else {
                    return;
                };
                let path = config_field.path.clone();
                let value = match &config_field.kind {
                    FieldKind::List {
                        choices: Some(choices),
                        ..
                    } => choices
                        .first()
                        .map(|choice| choice.value.clone())
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                push_array_value(&mut self.doc, &path, value);
                self.rebuild();
                return;
            }
            AddAction::MapRow(field) => {
                let Some(config_field) = self.fields.get(*field) else {
                    return;
                };
                let path = config_field.path.clone();
                let existing: BTreeSet<String> = match &config_field.kind {
                    FieldKind::Map { rows, .. } => {
                        rows.iter().map(|row| row.original_key.clone()).collect()
                    }
                    _ => BTreeSet::new(),
                };
                let key = unique_key(existing.into_iter().collect(), "KEY");
                set_string_map_entry(&mut self.doc, &path, &key, "");
                self.rebuild();
                return;
            }
        }
        self.rebuild();
    }

    /// Replace one row of an enum list (`retry_on`).
    pub fn set_list_choice(&mut self, field: usize, row: usize, value: String) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        set_array_value(&mut self.doc, &path, row, value);
        if let FieldKind::List { rows, .. } = &mut self.fields[field].kind {
            if let Some(input) = rows.get_mut(row) {
                input.set_value(String::new());
            }
        }
        self.rebuild();
    }

    pub fn remove_list_item(&mut self, field: usize, row: usize) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        remove_array_value(&mut self.doc, &path, row);
        self.rebuild();
    }

    pub fn remove_map_row(&mut self, field: usize, row: usize) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        let key = match &config_field.kind {
            FieldKind::Map { rows, .. } => rows.get(row).map(|row| row.original_key.clone()),
            _ => None,
        };
        if let Some(key) = key {
            remove_string_map_entry(&mut self.doc, &path, &key);
        }
        self.rebuild();
    }

    /// Explicitly clear a secret row, removing the key rather than sending the
    /// sentinel back to the daemon.
    pub fn clear_secret(&mut self, field: usize, row: usize) {
        self.remove_map_row(field, row);
    }

    /// Map daemon validation messages onto fields when they name a dotted path.
    pub fn set_daemon_errors(&mut self, errors: &[String]) {
        self.errors = errors.to_vec();
        for field in &mut self.fields {
            field.error = None;
        }
        self.revalidate();
        for message in errors {
            let Some(key) = daemon_error_key(message) else {
                continue;
            };
            for field in &mut self.fields {
                if field.error.is_none()
                    && (field.path == key
                        || field.path.starts_with(&format!("{key}."))
                        || key.starts_with(&format!("{}.", field.path)))
                {
                    field.error = Some(message.clone());
                }
            }
        }
    }

    fn build(&mut self, schema: &JsonValue) {
        let Some(root) = schema.get("properties") else {
            return;
        };
        for section in ConfigSection::ALL {
            self.layout.push(LayoutItem::Section(section));
            match section {
                ConfigSection::Agents => self.build_agents(schema, root),
                ConfigSection::Repos => self.build_table_list(
                    schema,
                    root.get("repos"),
                    "repos",
                    "known",
                    ConfigSection::Repos,
                    AddAction::RepoSeed,
                    "Repository",
                ),
                ConfigSection::Mcp => self.build_table_list(
                    schema,
                    root.get("mcp"),
                    "mcp",
                    "servers",
                    ConfigSection::Mcp,
                    AddAction::McpServer,
                    "MCP server",
                ),
                ConfigSection::Tui => {}
                _ => {
                    if let Some(node) = root.get(section.key()) {
                        self.collect_table(
                            schema,
                            node,
                            vec![key(section.key())],
                            section,
                            None,
                            0,
                            None,
                        );
                    }
                }
            }
        }
        self.revalidate();
    }

    fn build_agents(&mut self, schema: &JsonValue, root: &JsonValue) {
        let Some(section_schema) = root.get("agents").and_then(|node| effective(schema, node))
        else {
            return;
        };
        let entries = section_schema
            .get("additionalProperties")
            .and_then(|node| effective(schema, node));
        let Some(entries) = entries else {
            return;
        };
        let names = self.agent_names();
        let mut agent_names: Vec<String> = self
            .doc
            .get("agents")
            .and_then(Item::as_table)
            .map(|table| table.iter().map(|(key, _)| key.to_string()).collect())
            .unwrap_or_default();
        agent_names.sort();
        for name in agent_names {
            let group = self.groups.len();
            self.groups.push(ConfigGroup {
                title: format!("Agent: {name}"),
                section: ConfigSection::Agents,
                depth: 0,
                add: None,
                remove: Some(RemoveAction::Agent(name.clone())),
            });
            self.layout.push(LayoutItem::Group(group));
            self.collect_table(
                schema,
                entries,
                vec![key("agents"), key(&name)],
                ConfigSection::Agents,
                Some(group),
                0,
                None,
            );
            self.layout.push(LayoutItem::EndGroup(group));
        }
        if names.is_empty() {
            self.groups.push(ConfigGroup {
                title: "No agents declared".to_string(),
                section: ConfigSection::Agents,
                depth: 0,
                add: Some(AddAction::Agent),
                remove: None,
            });
        } else {
            self.groups.push(ConfigGroup {
                title: String::new(),
                section: ConfigSection::Agents,
                depth: 0,
                add: Some(AddAction::Agent),
                remove: None,
            });
        }
        let group = self.groups.len() - 1;
        self.layout.push(LayoutItem::Group(group));
        self.layout.push(LayoutItem::EndGroup(group));
    }

    #[allow(clippy::too_many_arguments)]
    fn build_table_list(
        &mut self,
        schema: &JsonValue,
        section_node: Option<&JsonValue>,
        section_key: &str,
        array_key: &str,
        section: ConfigSection,
        add: AddAction,
        title: &str,
    ) {
        let Some(section_node) = section_node else {
            return;
        };
        let Some(section_node) = effective(schema, section_node) else {
            return;
        };
        let Some(array_schema) = section_node
            .get("properties")
            .and_then(|props| props.get(array_key))
        else {
            return;
        };
        let Some(array_schema) = effective(schema, array_schema) else {
            return;
        };
        let Some(items) = array_schema
            .get("items")
            .and_then(|node| effective(schema, node))
        else {
            return;
        };
        let count = self.array_len(&[key(section_key), key(array_key)]);
        for index in 0..count {
            let label = self
                .read_string(&[
                    key(section_key),
                    key(array_key),
                    PathSeg::Index(index),
                    key("name"),
                ])
                .or_else(|| {
                    self.read_string(&[
                        key(section_key),
                        key(array_key),
                        PathSeg::Index(index),
                        key("path"),
                    ])
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("{title} {}", index + 1));
            let group = self.groups.len();
            self.groups.push(ConfigGroup {
                title: format!("{title}: {label}"),
                section,
                depth: 0,
                add: None,
                remove: Some(match add {
                    AddAction::RepoSeed => RemoveAction::RepoSeed(index),
                    AddAction::McpServer => RemoveAction::McpServer(index),
                    _ => continue,
                }),
            });
            self.layout.push(LayoutItem::Group(group));
            self.collect_table(
                schema,
                items,
                vec![key(section_key), key(array_key), PathSeg::Index(index)],
                section,
                Some(group),
                0,
                None,
            );
            self.layout.push(LayoutItem::EndGroup(group));
        }
        let group = self.groups.len();
        self.groups.push(ConfigGroup {
            title: if count == 0 {
                "No entries".to_string()
            } else {
                String::new()
            },
            section,
            depth: 0,
            add: Some(add),
            remove: None,
        });
        self.layout.push(LayoutItem::Group(group));
        self.layout.push(LayoutItem::EndGroup(group));
        self.collect_table(
            schema,
            section_node,
            vec![key(section_key)],
            section,
            None,
            0,
            Some(array_key),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_table(
        &mut self,
        schema: &JsonValue,
        node: &JsonValue,
        base: Vec<PathSeg>,
        section: ConfigSection,
        group: Option<usize>,
        depth: u8,
        skip: Option<&str>,
    ) {
        let Some(node) = effective(schema, node) else {
            return;
        };
        let Some(properties) = node.get("properties").and_then(JsonValue::as_object) else {
            return;
        };
        let required: BTreeSet<&str> = node
            .get("required")
            .and_then(JsonValue::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        for (property, raw) in properties {
            if skip == Some(property.as_str()) {
                continue;
            }
            let info = self.info(schema, raw, required.contains(property.as_str()));
            let mut path = base.clone();
            path.push(key(property));
            let dotted = dotted_path(&path);
            if dotted == "agent.default" {
                let choices = self.agent_choices();
                self.add_enum_field(&dotted, property, info, choices, &path, section, group);
                continue;
            }
            match info.shape {
                Shape::Table => {
                    if group.is_some() && depth >= 2 {
                        // Keep deeply nested tables flat; the field labels still
                        // carry the full path.
                        self.collect_table(
                            schema,
                            info.node,
                            path,
                            section,
                            group,
                            depth + 1,
                            None,
                        );
                        continue;
                    }
                    let nested = self.groups.len();
                    self.groups.push(ConfigGroup {
                        title: label_for(property),
                        section,
                        depth: depth + 1,
                        add: None,
                        remove: None,
                    });
                    self.layout.push(LayoutItem::Group(nested));
                    self.collect_table(
                        schema,
                        info.node,
                        path,
                        section,
                        Some(nested),
                        depth + 1,
                        None,
                    );
                    self.layout.push(LayoutItem::EndGroup(nested));
                }
                Shape::Map => {
                    self.add_map_field(&dotted, property, info, &path, section, group);
                }
                Shape::List => {
                    self.add_list_field(&dotted, property, info, &path, section, group);
                }
                Shape::Enum => {
                    let choices = if dotted == "agent.default" {
                        self.agent_choices()
                    } else {
                        info.choices.clone()
                    };
                    self.add_enum_field(&dotted, property, info, choices, &path, section, group);
                }
                Shape::Bool => {
                    let present = self.read_value(&path).is_some();
                    let value = self
                        .read_bool(&path)
                        .or_else(|| info.default.as_ref().and_then(JsonValue::as_bool))
                        .unwrap_or(false);
                    self.push_field(ConfigField {
                        path: dotted,
                        label: label_for(property),
                        help: info.description,
                        section,
                        group,
                        default: info.default.map(|value| display_json(&value)),
                        required: info.required,
                        boot_only: is_boot_only(&path),
                        secret: false,
                        kind: FieldKind::Bool { value, present },
                        error: None,
                    });
                }
                Shape::Number => {
                    let current = self.read_u64(&path).map(|value| value.to_string());
                    let default = info.default.as_ref().map(display_json);
                    let input = TextInput::new(current.unwrap_or_default());
                    self.push_field(ConfigField {
                        path: dotted,
                        label: label_for(property),
                        help: info.description,
                        section,
                        group,
                        default,
                        required: info.required,
                        boot_only: is_boot_only(&path),
                        secret: false,
                        kind: FieldKind::Text {
                            input,
                            optional: info.optional,
                            numeric: true,
                            min: info.minimum,
                        },
                        error: None,
                    });
                }
                Shape::Text => {
                    let current = self.read_string(&path).unwrap_or_default();
                    let input = TextInput::new(current);
                    self.push_field(ConfigField {
                        path: dotted,
                        label: label_for(property),
                        help: info.description,
                        section,
                        group,
                        default: info.default.as_ref().map(display_json),
                        required: info.required,
                        boot_only: is_boot_only(&path),
                        secret: false,
                        kind: FieldKind::Text {
                            input,
                            optional: info.optional,
                            numeric: false,
                            min: None,
                        },
                        error: None,
                    });
                }
                Shape::Unknown => {}
            }
        }
    }

    fn push_field(&mut self, field: ConfigField) {
        let index = self.fields.len();
        self.fields.push(field);
        self.layout.push(LayoutItem::Field(index));
    }

    #[allow(clippy::too_many_arguments)]
    fn add_enum_field(
        &mut self,
        dotted: &str,
        property: &str,
        info: Info,
        choices: Vec<EnumChoice>,
        path: &[PathSeg],
        section: ConfigSection,
        group: Option<usize>,
    ) {
        let selected = self.read_string(path);
        self.push_field(ConfigField {
            path: dotted.to_string(),
            label: label_for(property),
            help: info.description,
            section,
            group,
            default: info.default.as_ref().map(display_json),
            required: info.required,
            boot_only: is_boot_only(path),
            secret: false,
            kind: FieldKind::Enum {
                choices,
                selected,
                optional: info.optional,
            },
            error: None,
        });
    }

    fn add_list_field(
        &mut self,
        dotted: &str,
        property: &str,
        info: Info,
        path: &[PathSeg],
        section: ConfigSection,
        group: Option<usize>,
    ) {
        let values = self.read_array(path);
        let choices = info.list_choices;
        let rows = values
            .iter()
            .map(|value| TextInput::new(value.clone()))
            .collect();
        self.push_field(ConfigField {
            path: dotted.to_string(),
            label: label_for(property),
            help: info.description,
            section,
            group,
            default: info.default.as_ref().map(display_json),
            required: info.required,
            boot_only: false,
            secret: false,
            kind: FieldKind::List {
                rows,
                choices,
                optional: info.optional,
            },
            error: None,
        });
    }

    fn add_map_field(
        &mut self,
        dotted: &str,
        property: &str,
        info: Info,
        path: &[PathSeg],
        section: ConfigSection,
        group: Option<usize>,
    ) {
        let secret = is_secret_map(dotted);
        let rows = self
            .read_map(path)
            .into_iter()
            .map(|(key, value)| {
                let unchanged = secret && value == REDACTED_SECRET;
                MapRow {
                    key: TextInput::new(key.clone()),
                    value: TextInput::new(if unchanged { String::new() } else { value }),
                    original_key: key,
                    unchanged,
                    key_error: None,
                }
            })
            .collect();
        self.push_field(ConfigField {
            path: dotted.to_string(),
            label: label_for(property),
            help: info.description,
            section,
            group,
            default: info.default.as_ref().map(display_json),
            required: info.required,
            boot_only: false,
            secret,
            kind: FieldKind::Map { rows, secret },
            error: None,
        });
    }

    fn apply_scalar(&mut self, field: usize) {
        let Some(config_field) = self.fields.get_mut(field) else {
            return;
        };
        let FieldKind::Text {
            input,
            optional,
            numeric,
            min,
        } = &config_field.kind
        else {
            return;
        };
        let value = input.value().to_string();
        if *numeric {
            if value.trim().is_empty() {
                if *optional {
                    config_field.error = None;
                    remove_path(&mut self.doc, &config_field.path);
                } else {
                    config_field.error = Some("A number is required.".to_string());
                }
                return;
            }
            match value.trim().parse::<u64>() {
                Ok(number) => {
                    if min.is_some_and(|min| number < min) {
                        config_field.error =
                            Some(format!("Must be at least {}.", min.unwrap_or(0)));
                        return;
                    }
                    config_field.error = None;
                    set_value(
                        &mut self.doc,
                        &config_field.path,
                        Value::from(number as i64),
                    );
                }
                Err(_) => {
                    config_field.error = Some("Enter a whole number.".to_string());
                }
            }
            return;
        }
        if value.is_empty() && config_field.required {
            config_field.error = Some("This field is required.".to_string());
            return;
        }
        config_field.error = None;
        if value.is_empty() && *optional {
            remove_path(&mut self.doc, &config_field.path);
        } else {
            set_value(&mut self.doc, &config_field.path, Value::from(value));
        }
    }

    fn apply_list_item(&mut self, field: usize, row: usize) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        let Some(value) = (match &config_field.kind {
            FieldKind::List { rows, .. } => rows.get(row).map(|input| input.value().to_string()),
            _ => None,
        }) else {
            return;
        };
        set_array_value(&mut self.doc, &path, row, value);
    }

    fn apply_map_key(&mut self, field: usize, row: usize) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        let Some((new_key, old_key, value)) = (match &config_field.kind {
            FieldKind::Map { rows, .. } => rows.get(row).map(|row| {
                (
                    row.key.value().to_string(),
                    row.original_key.clone(),
                    row.value.value().to_string(),
                )
            }),
            _ => None,
        }) else {
            return;
        };
        let error = match &self.fields[field].kind {
            FieldKind::Map { rows, secret } => {
                if new_key.is_empty() {
                    Some("A key is required.".to_string())
                } else if new_key != old_key
                    && rows
                        .iter()
                        .enumerate()
                        .any(|(index, existing)| index != row && existing.original_key == new_key)
                {
                    Some("Keys must be unique.".to_string())
                } else {
                    let _ = secret;
                    None
                }
            }
            _ => None,
        };
        if error.is_some() {
            if let FieldKind::Map { rows, .. } = &mut self.fields[field].kind {
                if let Some(row) = rows.get_mut(row) {
                    row.key_error = error;
                }
            }
            return;
        }
        if new_key != old_key {
            remove_string_map_entry(&mut self.doc, &path, &old_key);
            set_string_map_entry_raw(&mut self.doc, &path, &new_key, &value);
            if let FieldKind::Map { rows, .. } = &mut self.fields[field].kind {
                if let Some(row) = rows.get_mut(row) {
                    row.original_key = new_key;
                    row.key_error = None;
                }
            }
        }
    }

    fn apply_map_value(&mut self, field: usize, row: usize) {
        let Some(config_field) = self.fields.get(field) else {
            return;
        };
        let path = config_field.path.clone();
        let Some((key, value, secret, unchanged)) = (match &config_field.kind {
            FieldKind::Map { rows, secret } => rows.get(row).map(|row| {
                (
                    row.original_key.clone(),
                    row.value.value().to_string(),
                    *secret,
                    row.unchanged,
                )
            }),
            _ => None,
        }) else {
            return;
        };
        if secret && unchanged && value.is_empty() {
            return;
        }
        set_string_map_entry_raw(&mut self.doc, &path, &key, &value);
        if let FieldKind::Map { rows, .. } = &mut self.fields[field].kind {
            if let Some(row) = rows.get_mut(row) {
                row.unchanged = false;
            }
        }
    }

    fn revalidate(&mut self) {
        self.errors.clear();
        for field in &mut self.fields {
            match &field.kind {
                FieldKind::Map { rows, secret } => {
                    let mut seen = BTreeSet::new();
                    for row in rows {
                        let key = row.original_key.clone();
                        if key.is_empty() {
                            field.error = Some("A key is required.".to_string());
                        } else if !seen.insert(key.clone()) {
                            field.error = Some(format!("Duplicate key: {key}."));
                        }
                    }
                    let _ = secret;
                }
                FieldKind::Text {
                    input,
                    numeric: true,
                    min,
                    ..
                } => {
                    if !input.value().trim().is_empty() {
                        match input.value().trim().parse::<u64>() {
                            Ok(number) if min.is_some_and(|min| number < min) => {
                                field.error =
                                    Some(format!("Must be at least {}.", min.unwrap_or_default()));
                            }
                            Ok(_) => {}
                            Err(_) => field.error = Some("Enter a whole number.".to_string()),
                        }
                    }
                }
                FieldKind::Text {
                    input,
                    optional: false,
                    numeric: false,
                    ..
                } if field.required && input.value().is_empty() => {
                    field.error = Some("This field is required.".to_string());
                }
                _ => {}
            }
        }
        self.errors = self
            .fields
            .iter()
            .filter_map(|field| {
                field
                    .error
                    .as_ref()
                    .map(|error| format!("{}: {error}", field.path))
            })
            .collect();
    }

    fn rebuild(&mut self) {
        let raw = self.doc.to_string();
        if let Some(mut form) = Self::parse(&raw) {
            form.errors = std::mem::take(&mut self.errors);
            *self = form;
        }
    }

    fn info<'a>(&self, schema: &'a JsonValue, raw: &'a JsonValue, required: bool) -> Info<'a> {
        let Some(node) = effective(schema, raw) else {
            return Info {
                node: raw,
                description: String::new(),
                default: None,
                optional: true,
                required,
                minimum: None,
                shape: Shape::Unknown,
                choices: Vec::new(),
                list_choices: None,
            };
        };
        let description = raw
            .get("description")
            .and_then(JsonValue::as_str)
            .or_else(|| node.get("description").and_then(JsonValue::as_str))
            .unwrap_or_default()
            .to_string();
        let default = raw.get("default").or_else(|| node.get("default")).cloned();
        let optional = is_optional(raw) || is_optional(node);
        let mut choices = enum_choices(node);
        let mut list_choices = None;
        let shape = shape_of(node);
        if shape == Shape::List {
            if let Some(items) = node.get("items").and_then(|items| effective(schema, items)) {
                let item_choices = enum_choices(items);
                if !item_choices.is_empty() {
                    list_choices = Some(item_choices);
                }
            }
        }
        if choices.is_empty() {
            choices = Vec::new();
        }
        Info {
            node,
            description,
            default,
            optional,
            required,
            minimum: node.get("minimum").and_then(JsonValue::as_u64),
            shape,
            choices,
            list_choices,
        }
    }

    fn agent_names(&self) -> Vec<String> {
        self.toml_value()
            .get("agents")
            .and_then(toml::Value::as_table)
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn agent_choices(&self) -> Vec<EnumChoice> {
        self.agent_names()
            .into_iter()
            .map(|name| EnumChoice {
                value: name,
                description: None,
            })
            .collect()
    }

    fn toml_value(&self) -> toml::Value {
        toml::from_str(&self.doc.to_string())
            .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()))
    }

    fn read_value(&self, path: &[PathSeg]) -> Option<toml::Value> {
        let value = self.toml_value();
        value_at(&value, path).cloned()
    }

    fn read_string(&self, path: &[PathSeg]) -> Option<String> {
        self.read_value(path)?.as_str().map(str::to_string)
    }

    fn read_bool(&self, path: &[PathSeg]) -> Option<bool> {
        self.read_value(path)?.as_bool()
    }

    fn read_u64(&self, path: &[PathSeg]) -> Option<u64> {
        self.read_value(path)?
            .as_integer()
            .and_then(|value| u64::try_from(value).ok())
    }

    fn read_array(&self, path: &[PathSeg]) -> Vec<String> {
        self.read_value(path)
            .and_then(|value| value.as_array().cloned())
            .map(|array| {
                array
                    .iter()
                    .map(|value| value.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn read_map(&self, path: &[PathSeg]) -> Vec<(String, String)> {
        self.read_value(path)
            .and_then(|value| value.as_table().cloned())
            .map(|table| {
                table
                    .iter()
                    .map(|(key, value)| {
                        (key.clone(), value.as_str().unwrap_or_default().to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn array_len(&self, path: &[PathSeg]) -> usize {
        self.read_value(path)
            .and_then(|value| value.as_array().map(Vec::len))
            .unwrap_or(0)
    }

    fn remove_array_table(&mut self, path: &str, index: usize) {
        let segments = split_path(path);
        let Some(parent) = parent_table_mut(&mut self.doc, &segments) else {
            return;
        };
        let Some(last) = segments.last() else {
            return;
        };
        let PathSeg::Key(last) = last else {
            return;
        };
        if let Some(tables) = parent.get_mut(last).and_then(Item::as_array_of_tables_mut) {
            if index < tables.len() {
                tables.remove(index);
            }
        }
        if parent
            .get(last)
            .and_then(Item::as_array_of_tables)
            .is_some_and(|tables| tables.is_empty())
        {
            parent.remove(last);
        }
    }

    fn push_array_table(&mut self, path: &str, table: Table) {
        let segments = split_path(path);
        let Some(parent) = parent_table_mut(&mut self.doc, &segments) else {
            return;
        };
        let Some(PathSeg::Key(last)) = segments.last() else {
            return;
        };
        let entry = parent
            .entry(last)
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
        if entry.as_array_of_tables().is_none() {
            *entry = Item::ArrayOfTables(ArrayOfTables::new());
        }
        if let Some(tables) = entry.as_array_of_tables_mut() {
            tables.push(table);
        }
    }
}

#[derive(Debug, Clone)]
struct Info<'a> {
    node: &'a JsonValue,
    description: String,
    default: Option<JsonValue>,
    optional: bool,
    required: bool,
    minimum: Option<u64>,
    shape: Shape,
    choices: Vec<EnumChoice>,
    list_choices: Option<Vec<EnumChoice>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Text,
    Number,
    Bool,
    Enum,
    List,
    Map,
    Table,
    Unknown,
}

fn control_field(control: ControlRef) -> usize {
    match control {
        ControlRef::Scalar(field)
        | ControlRef::ListItem(field, _)
        | ControlRef::MapKey(field, _)
        | ControlRef::MapValue(field, _)
        | ControlRef::Toggle(field) => field,
    }
}

fn shape_of(node: &JsonValue) -> Shape {
    if !enum_choices(node).is_empty() {
        return Shape::Enum;
    }
    let types: Vec<&str> = match node.get("type") {
        Some(JsonValue::String(value)) => vec![value.as_str()],
        Some(JsonValue::Array(values)) => values.iter().filter_map(JsonValue::as_str).collect(),
        _ => Vec::new(),
    };
    if types.contains(&"object") {
        if node.get("properties").is_some() {
            return Shape::Table;
        }
        if node.get("additionalProperties").is_some() {
            return Shape::Map;
        }
        return Shape::Unknown;
    }
    if types.contains(&"array") {
        return Shape::List;
    }
    if types.contains(&"boolean") {
        return Shape::Bool;
    }
    if types.contains(&"integer") || types.contains(&"number") {
        return Shape::Number;
    }
    if types.contains(&"string") {
        return Shape::Text;
    }
    Shape::Unknown
}

fn enum_choices(node: &JsonValue) -> Vec<EnumChoice> {
    if let Some(values) = node.get("enum").and_then(JsonValue::as_array) {
        return values
            .iter()
            .filter_map(JsonValue::as_str)
            .map(|value| EnumChoice {
                value: value.to_string(),
                description: None,
            })
            .collect();
    }
    let Some(branches) = node.get("oneOf").and_then(JsonValue::as_array) else {
        return Vec::new();
    };
    let mut choices = Vec::new();
    for branch in branches {
        if is_null_schema(branch) {
            continue;
        }
        if let Some(value) = branch.get("const").and_then(JsonValue::as_str).or_else(|| {
            branch
                .get("enum")
                .and_then(JsonValue::as_array)
                .and_then(|values| values.first())
                .and_then(JsonValue::as_str)
        }) {
            choices.push(EnumChoice {
                value: value.to_string(),
                description: branch
                    .get("description")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
            });
        }
    }
    choices
}

fn is_null_schema(node: &JsonValue) -> bool {
    node.get("type").and_then(JsonValue::as_str) == Some("null")
        || node
            .get("type")
            .and_then(JsonValue::as_array)
            .is_some_and(|types| types.len() == 1 && types[0] == "null")
}

fn label_for(key: &str) -> String {
    let mut label = String::with_capacity(key.len());
    for (index, part) in key.split('_').enumerate() {
        if index > 0 {
            label.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            label.push(first.to_ascii_uppercase());
            label.extend(chars);
        }
    }
    label
}

fn display_json(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => value.clone(),
        other => other.to_string(),
    }
}

/// Extract the field path a daemon message names, if any.
///
/// Preflight messages use `[section]`/`[agents.name.git]` prefixes; serde
/// messages use `field.path: message`. The form only needs a best-effort hint,
/// and the full message always remains in the card summary.
fn daemon_error_key(message: &str) -> Option<String> {
    if let Some(rest) = message.strip_prefix('[') {
        let (section, after) = rest.split_once(']')?;
        let suffix: String = after
            .trim_start_matches('.')
            .chars()
            .take_while(|character| {
                character.is_alphanumeric() || *character == '_' || *character == '.'
            })
            .collect();
        return Some(if suffix.is_empty() {
            section.to_string()
        } else {
            format!("{section}.{suffix}")
        });
    }
    let (key, _) = message
        .split_once(':')
        .or_else(|| message.split_once(" is "))?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn is_boot_only(path: &[PathSeg]) -> bool {
    let dotted = dotted_path(path);
    matches!(
        dotted.as_str(),
        "daemon.listen"
            | "daemon.socket"
            | "daemon.tasks_dir"
            | "daemon.data_dir"
            | "auth.require_tls"
    ) || dotted.ends_with(".command")
}

fn is_secret_map(path: &str) -> bool {
    path.starts_with("agents.") && path.ends_with(".env")
        || path.starts_with("mcp.servers.")
            && (path.ends_with(".env") || path.ends_with(".headers"))
}

fn next_enum(choices: &[String], selected: Option<&str>, optional: bool) -> Option<String> {
    let index = selected.and_then(|selected| choices.iter().position(|choice| choice == selected));
    match (index, optional) {
        (Some(index), _) if index + 1 < choices.len() => Some(choices[index + 1].clone()),
        (Some(_), true) => None,
        (Some(_), false) => choices.first().cloned(),
        (None, _) => choices.first().cloned(),
    }
}

fn default_bool(path: &str) -> bool {
    matches!(
        path,
        "executor.worktree"
            | "executor.detect_awaiting_input"
            | "daemon.retention.vacuum"
            | "tui.show_header"
            | "tui.show_footer"
            | "tui.sound.enabled"
    )
}

fn repo_seed_table() -> Table {
    let mut table = Table::new();
    table.insert("path", Item::Value(Value::from("")));
    table
}

fn mcp_server_table(name: &str) -> Table {
    let mut table = Table::new();
    table.insert("name", Item::Value(Value::from(name)));
    table.insert("transport", Item::Value(Value::from("stdio")));
    table
}

fn unique_key(existing: Vec<String>, base: &str) -> String {
    let existing: BTreeSet<String> = existing.into_iter().collect();
    if !existing.contains(base) {
        return base.to_string();
    }
    for index in 2.. {
        let candidate = format!("{base}-{index}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

const REDACTED_SECRET: &str = "__FAVETTO_REDACTED__";

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSeg {
    Key(String),
    Index(usize),
}

fn key(value: &str) -> PathSeg {
    PathSeg::Key(value.to_string())
}

fn split_path(path: &str) -> Vec<PathSeg> {
    if path.is_empty() {
        return Vec::new();
    }
    path.split('.').map(key).collect()
}

fn dotted_path(path: &[PathSeg]) -> String {
    let mut out = String::new();
    for segment in path {
        match segment {
            PathSeg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            PathSeg::Index(index) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(&index.to_string());
            }
        }
    }
    out
}

fn value_at<'a>(value: &'a toml::Value, path: &[PathSeg]) -> Option<&'a toml::Value> {
    let mut current = value;
    for segment in path {
        current = match segment {
            PathSeg::Key(key) => current.get(key)?,
            PathSeg::Index(index) => current.get(*index)?,
        };
    }
    Some(current)
}

fn effective<'a>(schema: &'a JsonValue, raw: &'a JsonValue) -> Option<&'a JsonValue> {
    let mut node = raw;
    if let Some(any) = raw.get("anyOf").or_else(|| raw.get("oneOf")) {
        if let Some(branches) = any.as_array() {
            let mut non_null = branches.iter().filter(|branch| !is_null_schema(branch));
            let first = non_null.next();
            if non_null.next().is_none() {
                node = first?;
            }
        }
    }
    let mut current = node;
    for _ in 0..16 {
        let Some(reference) = current.get("$ref").and_then(JsonValue::as_str) else {
            return Some(current);
        };
        let name = reference.rsplit('/').next()?;
        current = schema.get("$defs").and_then(|defs| defs.get(name))?;
    }
    None
}

fn is_optional(node: &JsonValue) -> bool {
    node.get("type")
        .and_then(JsonValue::as_array)
        .is_some_and(|types| types.iter().any(|value| value == "null"))
        || node
            .get("anyOf")
            .or_else(|| node.get("oneOf"))
            .and_then(JsonValue::as_array)
            .is_some_and(|branches| branches.iter().any(is_null_schema))
}

fn set_value(doc: &mut DocumentMut, path: &str, value: Value) {
    let segments = split_path(path);
    let Some(parent) = parent_table_mut(doc, &segments) else {
        return;
    };
    let Some(PathSeg::Key(last)) = segments.last() else {
        return;
    };
    parent.insert(last, Item::Value(value));
}

fn set_string_map_entry(doc: &mut DocumentMut, path: &str, key: &str, value: &str) {
    set_string_map_entry_raw(doc, path, key, value);
}

fn set_string_map_entry_raw(doc: &mut DocumentMut, path: &str, key: &str, value: &str) {
    let segments = split_path(path);
    let Some(parent) = parent_table_mut(doc, &segments) else {
        return;
    };
    let Some(PathSeg::Key(last)) = segments.last() else {
        return;
    };
    if !parent.contains_key(last) {
        parent.insert(last, Item::Table(Table::new()));
    }
    if let Some(table) = parent.get_mut(last).and_then(item_as_table_mut) {
        table.insert(key, Item::Value(Value::from(value)));
    }
}

fn remove_string_map_entry(doc: &mut DocumentMut, path: &str, key: &str) {
    let segments = split_path(path);
    let Some(parent) = parent_table_mut(doc, &segments) else {
        return;
    };
    let Some(PathSeg::Key(last)) = segments.last() else {
        return;
    };
    if let Some(table) = parent.get_mut(last).and_then(item_as_table_mut) {
        table.remove(key);
    }
}

fn push_array_value(doc: &mut DocumentMut, path: &str, value: String) {
    let segments = split_path(path);
    let Some(parent) = parent_table_mut(doc, &segments) else {
        return;
    };
    let Some(PathSeg::Key(last)) = segments.last() else {
        return;
    };
    if !parent.contains_key(last) {
        parent.insert(last, Item::Value(Value::Array(Array::new())));
    }
    if let Some(array) = parent
        .get_mut(last)
        .and_then(Item::as_value_mut)
        .and_then(Value::as_array_mut)
    {
        array.push(value);
    }
}

fn set_array_value(doc: &mut DocumentMut, path: &str, index: usize, value: String) {
    let segments = split_path(path);
    let Some(parent) = parent_table_mut(doc, &segments) else {
        return;
    };
    let Some(PathSeg::Key(last)) = segments.last() else {
        return;
    };
    if !parent.contains_key(last) {
        parent.insert(last, Item::Value(Value::Array(Array::new())));
    }
    if let Some(array) = parent
        .get_mut(last)
        .and_then(Item::as_value_mut)
        .and_then(Value::as_array_mut)
    {
        while array.len() <= index {
            array.push("");
        }
        if let Some(slot) = array.get_mut(index) {
            *slot = Value::from(value);
        }
    }
}

fn remove_array_value(doc: &mut DocumentMut, path: &str, index: usize) {
    let segments = split_path(path);
    let Some(parent) = parent_table_mut(doc, &segments) else {
        return;
    };
    let Some(PathSeg::Key(last)) = segments.last() else {
        return;
    };
    if let Some(array) = parent
        .get_mut(last)
        .and_then(Item::as_value_mut)
        .and_then(Value::as_array_mut)
    {
        if index < array.len() {
            array.remove(index);
        }
        if array.is_empty() {
            parent.remove(last);
        }
    }
}

fn remove_path(doc: &mut DocumentMut, path: &str) {
    let segments = split_path(path);
    let Some(parent) = parent_table_mut(doc, &segments) else {
        return;
    };
    let Some(PathSeg::Key(last)) = segments.last() else {
        return;
    };
    parent.remove(last);
}

fn parent_table_mut<'a>(doc: &'a mut DocumentMut, path: &[PathSeg]) -> Option<&'a mut Table> {
    let (_, parent) = path.split_last()?;
    table_at(doc.as_table_mut(), parent)
}

fn table_at<'a>(table: &'a mut Table, path: &[PathSeg]) -> Option<&'a mut Table> {
    if path.is_empty() {
        return Some(table);
    }
    if let [PathSeg::Key(key), PathSeg::Index(index), rest @ ..] = path {
        if !table.contains_key(key) {
            table.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let item = table.get_mut(key)?;
        let tables = ensure_array_of_tables(item)?;
        while tables.len() <= *index {
            tables.push(Table::new());
        }
        return table_at(tables.get_mut(*index)?, rest);
    }
    if let [PathSeg::Key(key), rest @ ..] = path {
        if !table.contains_key(key) {
            table.insert(key, Item::Table(Table::new()));
        }
        let item = table.get_mut(key)?;
        let child = item_as_table_mut(item)?;
        return table_at(child, rest);
    }
    None
}

fn ensure_array_of_tables(item: &mut Item) -> Option<&mut ArrayOfTables> {
    if item.as_array_of_tables().is_none() {
        if let Some(array) = item.as_value().and_then(Value::as_array) {
            let mut tables = ArrayOfTables::new();
            for value in array.iter() {
                let inline = value.as_inline_table()?;
                let mut table = Table::new();
                for (key, value) in inline.iter() {
                    table.insert(key, Item::Value(value.clone()));
                }
                tables.push(table);
            }
            *item = Item::ArrayOfTables(tables);
        } else {
            *item = Item::ArrayOfTables(ArrayOfTables::new());
        }
    }
    item.as_array_of_tables_mut()
}

fn item_as_table_mut(item: &mut Item) -> Option<&mut Table> {
    if item.as_table().is_none() {
        if let Some(inline) = item.as_value().and_then(Value::as_inline_table) {
            let mut table = Table::new();
            for (key, value) in inline.iter() {
                table.insert(key, Item::Value(value.clone()));
            }
            *item = Item::Table(table);
        }
    }
    item.as_table_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_form(raw: &str) -> ConfigForm {
        ConfigForm::parse(raw).expect("valid TOML")
    }

    fn field<'a>(form: &'a ConfigForm, path: &str) -> &'a ConfigField {
        form.fields()
            .iter()
            .find(|field| field.path == path)
            .unwrap_or_else(|| panic!("missing field {path}"))
    }

    #[test]
    fn the_schema_produces_a_labeled_field_for_every_documented_root_section() {
        let form = parse_form("");
        for path in [
            "daemon.listen",
            "daemon.retention.days",
            "executor.retry.max_attempts",
            "git.signing",
            "auth.login_rate_limit",
            "repos.depth",
        ] {
            let field = field(&form, path);
            assert!(!field.label.is_empty(), "{path} has a label");
        }
        // [tui] is client-side and intentionally has no daemon form field.
        assert!(form
            .fields()
            .iter()
            .all(|field| field.section != ConfigSection::Tui || !field.path.starts_with("tui.")));
    }

    #[test]
    fn defaults_and_enum_choices_come_from_the_schema() {
        let form = parse_form("");
        assert_eq!(
            field(&form, "daemon.retention.days").default.as_deref(),
            Some("30")
        );
        let signing = field(&form, "git.signing");
        let FieldKind::Enum {
            choices, selected, ..
        } = &signing.kind
        else {
            panic!("git.signing must be an enum select");
        };
        let values: Vec<&str> = choices.iter().map(|choice| choice.value.as_str()).collect();
        assert_eq!(values, ["off", "gpg", "ssh"]);
        assert_eq!(selected, &None);
    }

    #[test]
    fn editing_a_text_field_patches_only_that_value_and_preserves_comments() {
        let mut form =
            parse_form("# keep me\n[daemon]\nlisten = \"127.0.0.1:7878\" # inline\nunknown = 42\n");
        let listen = form
            .fields()
            .iter()
            .position(|field| field.path == "daemon.listen")
            .unwrap();
        form.fields[listen].kind = interpolate_text(&form.fields[listen], "127.0.0.1:9999");
        let control = form.field_controls(listen)[0].0;
        form.after_control_edit(control);
        let raw = form.raw();
        assert!(raw.contains("# keep me"));
        assert!(raw.contains("unknown = 42"));
        assert!(raw.contains("listen = \"127.0.0.1:9999\""), "{raw}");
    }

    #[test]
    fn an_empty_optional_value_removes_the_key_instead_of_writing_an_empty_string() {
        let mut form = parse_form("[daemon]\nlisten = \"127.0.0.1:7878\"\n");
        let listen = form
            .fields()
            .iter()
            .position(|field| field.path == "daemon.listen")
            .unwrap();
        form.fields[listen].kind = interpolate_text(&form.fields[listen], "");
        let control = form.field_controls(listen)[0].0;
        form.after_control_edit(control);
        assert!(!form.raw().contains("listen"));
    }

    #[test]
    fn unset_and_false_are_distinct() {
        let mut form = parse_form("");
        let worktree = form
            .fields()
            .iter()
            .position(|field| field.path == "executor.worktree")
            .unwrap();
        assert!(!form.raw().contains("worktree"));
        form.set_bool(worktree, false);
        assert!(form.raw().contains("worktree = false"));
        form.reset_field(worktree);
        assert!(!form.raw().contains("worktree"));
    }

    #[test]
    fn numeric_fields_reject_non_numbers_without_touching_the_document() {
        let mut form = parse_form("");
        let concurrency = form
            .fields()
            .iter()
            .position(|field| field.path == "executor.max_concurrency")
            .unwrap();
        let control = form.field_controls(concurrency)[0].0;
        form.text_input_mut(control)
            .unwrap()
            .set_value("many".to_string());
        form.after_control_edit(control);
        assert!(form.raw().is_empty());
        assert!(form.fields[concurrency].error.is_some());
        assert!(form.has_errors());
    }

    #[test]
    fn secret_maps_never_expose_the_sentinel_and_preserve_untouched_values() {
        let mut form = parse_form(
            "[agents.opencode]\ncommand = \"opencode\"\n[agents.opencode.env]\nTOKEN = \"__FAVETTO_REDACTED__\"\n",
        );
        let env = form
            .fields()
            .iter()
            .position(|field| field.path == "agents.opencode.env")
            .unwrap();
        let FieldKind::Map { rows, secret } = &form.fields[env].kind else {
            panic!("env is a map");
        };
        assert!(secret);
        assert_eq!(rows[0].value.value(), "");
        assert!(rows[0].unchanged);
        assert!(form.raw().contains(REDACTED_SECRET));

        let control = form
            .controls()
            .iter()
            .position(|control| matches!(*control, ControlRef::MapValue(field, _) if field == env))
            .unwrap();
        form.text_input_mut(control)
            .unwrap()
            .set_value("new-secret".to_string());
        form.after_control_edit(control);
        assert!(!form.raw().contains(REDACTED_SECRET));
        assert!(form.raw().contains("new-secret"));
    }

    #[test]
    fn adding_and_removing_agents_round_trips_through_toml() {
        let mut form = parse_form("");
        form.add(&AddAction::Agent);
        assert!(form.raw().contains("[agents.agent]"));
        assert!(form
            .fields()
            .iter()
            .any(|field| field.path == "agents.agent.command"));
        form.remove(&RemoveAction::Agent("agent".to_string()));
        assert!(!form.raw().contains("agents"));
    }

    #[test]
    fn adding_repo_and_mcp_entries_creates_array_of_tables() {
        let mut form = parse_form("");
        form.add(&AddAction::RepoSeed);
        assert!(form.raw().contains("[[repos.known]]"));
        form.add(&AddAction::McpServer);
        assert!(form.raw().contains("[[mcp.servers]]"));
        form.remove(&RemoveAction::RepoSeed(0));
        assert!(!form.raw().contains("[[repos.known]]"));
    }

    #[test]
    fn list_rows_add_remove_and_replace_values() {
        let mut form = parse_form("[agents.opencode]\ncommand = \"x\"\n");
        let args = form
            .fields()
            .iter()
            .position(|field| field.path == "agents.opencode.args")
            .unwrap();
        form.add(&AddAction::ListItem(args));
        let args = form
            .fields()
            .iter()
            .position(|field| field.path == "agents.opencode.args")
            .unwrap();
        let control = form.field_controls(args)[0].0;
        form.text_input_mut(control)
            .unwrap()
            .set_value("acp".to_string());
        form.after_control_edit(control);
        assert!(form.raw().contains("args = [\"acp\"]"));
        form.remove_list_item(args, 0);
        assert!(!form.raw().contains("args"));
    }

    #[test]
    fn map_rows_can_be_added_and_removed() {
        let mut form = parse_form("[agents.opencode]\ncommand = \"x\"\n");
        let env = form
            .fields()
            .iter()
            .position(|field| field.path == "agents.opencode.env")
            .unwrap();
        form.add(&AddAction::MapRow(env));
        let env = form
            .fields()
            .iter()
            .position(|field| field.path == "agents.opencode.env")
            .unwrap();
        assert!(form.raw().contains("KEY = \"\""));
        form.remove_map_row(env, 0);
        assert!(!form.raw().contains("KEY"));
    }

    #[test]
    fn an_optional_enum_cycles_through_choices_and_back_to_unset() {
        let mut form = parse_form("");
        let signing = form
            .fields()
            .iter()
            .position(|field| field.path == "git.signing")
            .unwrap();
        let control = form.field_controls(signing)[0].0;
        assert!(form.toggle(control));
        assert!(form.raw().contains("signing = \"off\""));
        assert!(form.toggle(control));
        assert!(form.raw().contains("signing = \"gpg\""));
        assert!(form.toggle(control));
        assert!(form.raw().contains("signing = \"ssh\""));
        assert!(form.toggle(control));
        assert!(!form.raw().contains("signing"));
    }

    #[test]
    fn the_default_agent_select_is_populated_from_the_declared_agents() {
        let mut form = parse_form("[agents.alpha]\ncommand = \"alpha\"\n");
        form.add(&AddAction::Agent);
        let default = form
            .fields()
            .iter()
            .find(|field| field.path == "agent.default")
            .expect("agent.default");
        let FieldKind::Enum { choices, .. } = &default.kind else {
            panic!("agent.default is a select");
        };
        let values: Vec<&str> = choices.iter().map(|choice| choice.value.as_str()).collect();
        assert!(values.contains(&"alpha"));
        assert!(values.contains(&"agent"));
    }

    #[test]
    fn daemon_errors_map_onto_the_named_field() {
        let mut form = parse_form("");
        form.set_daemon_errors(&["executor.retry.max_attempts: must be >= 1".to_string()]);
        let max_attempts = field(&form, "executor.retry.max_attempts");
        assert!(max_attempts
            .error
            .as_deref()
            .is_some_and(|error| error.contains("must be >= 1")));

        let mut form = parse_form("");
        form.set_daemon_errors(&["[git]: signing key not found".to_string()]);
        assert!(field(&form, "git.signing_key").error.is_some());

        let mut form = parse_form("");
        form.set_daemon_errors(&[
            "[agent].default = \"missing\" does not name a configured agent".to_string(),
        ]);
        assert!(field(&form, "agent.default").error.is_some());
    }

    fn interpolate_text(field: &ConfigField, value: &str) -> FieldKind {
        match &field.kind {
            FieldKind::Text {
                optional,
                numeric,
                min,
                ..
            } => FieldKind::Text {
                input: TextInput::new(value.to_string()),
                optional: *optional,
                numeric: *numeric,
                min: *min,
            },
            other => other.clone(),
        }
    }
}
