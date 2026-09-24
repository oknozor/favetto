//! `favetto-wire` — platform-free shared domain and wire types.
//!
//! Pure serde/MessagePack value types extracted from `favetto-core` so browser
//! (wasm32) and native clients share one vocabulary. No filesystem, no tokio,
//! no config. `favetto-core` re-exports these modules to preserve every
//! existing import path.

pub mod agent_state;
pub mod model;
pub mod rpc;
pub mod task_var;
pub mod workflow;
