//! `favetto-wire` — platform-free shared domain and wire types.
//!
//! Pure serde/MessagePack value types extracted from `favetto-core` so native
//! clients share one vocabulary. No filesystem, no tokio, no config.
//! `favetto-core` re-exports these modules to preserve every existing import
//! path.

pub mod agent_state;
pub mod elicitation;
pub mod model;
pub mod rpc;
pub mod task_var;
pub mod timestamp;
pub mod workflow;

pub use timestamp::Timestamp;

/// Re-exported so every crate in the workspace shares one `time` version and
/// can name `Duration`/`OffsetDateTime` without declaring the dependency twice.
pub use time;
