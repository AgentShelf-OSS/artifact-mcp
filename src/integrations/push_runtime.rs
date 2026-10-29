//! ADR-0012 reminder service, sweeper, and Web Push sender.
//!
//! [`PushService`] is the single boundary the viewer routes and the MCP tool use. It exists only
//! when the feature is enabled (see [`crate::config::AppConfig::web_push_enabled`]).
//! [`PushRuntime`] owns the background sweeper and sender tasks.

mod runtime;
mod service;

pub use runtime::PushRuntime;
pub use service::*;
