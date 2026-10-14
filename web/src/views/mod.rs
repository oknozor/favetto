//! Route components.
//!
//! `Pair` owns onboarding; the authenticated views (Inbox, Tasks, Catalog,
//! Workflow, Schedules, Settings) each plug into the [`crate::app::Route`] enum
//! and share the [`crate::app`] navigation shell. The Inbox is the default
//! landing view.

pub mod catalog;
pub mod inbox;
pub mod pair;
pub mod schedules;
pub mod settings;
pub mod tasks;
pub mod workflow;
