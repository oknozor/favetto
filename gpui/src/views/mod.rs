//! Route views. Each surface is its own GPUI entity so later steps can add
//! local state (filters, tabs, scroll position) without touching the shell.
//!
//! Step 03 ships the navigation frame and intentional empty states; rows,
//! live feeds and forms arrive in the surfaces' own steps (docs §5).

pub mod agent;
pub mod catalog;
pub mod conversation;
pub mod detail;
pub mod inbox;
pub(crate) mod markdown;
pub mod schedules;
pub mod settings;
pub mod tasks;
pub mod workflows;

pub use agent::AgentView;
pub use catalog::CatalogView;
pub use detail::DetailView;
pub use inbox::InboxView;
pub use schedules::SchedulesView;
pub use settings::SettingsView;
pub use tasks::TasksView;
pub use workflows::WorkflowsView;
