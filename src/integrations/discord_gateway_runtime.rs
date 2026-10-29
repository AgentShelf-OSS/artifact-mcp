//! Guarded production Discord Gateway runtime for PBI-080.
//!
//! The implementation is split by responsibility in the sibling module tree. This facade keeps
//! the established public integration paths stable.

mod rest;
mod runtime;

pub use rest::DiscordInboundReadinessRest;
pub use runtime::{DiscordInboundRuntime, render_prometheus};
