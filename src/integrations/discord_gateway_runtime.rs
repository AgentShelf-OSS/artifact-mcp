//! Guarded production Discord Gateway runtime for PBI-080.
//!
//! The implementation is split by responsibility in the sibling module tree. This facade keeps
//! the established public integration paths stable.

mod runtime;

pub use runtime::{DiscordInboundReadinessRest, DiscordInboundRuntime, render_prometheus};
