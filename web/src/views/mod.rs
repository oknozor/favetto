//! Route components.
//!
//! `Pair` owns onboarding; the authenticated views (Inbox, Tasks, Catalog,
//! Schedules, Settings) each plug into the [`crate::app::Route`] enum and share
//! the [`crate::app`] navigation shell.

pub mod catalog;
pub mod pair;
pub mod schedules;
pub mod settings;
pub mod tasks;
pub mod workflow;
