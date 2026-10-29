//! Guarded production Discord Gateway runtime for PBI-080.
//!
//! The implementation is split by responsibility in the sibling module tree. This facade keeps
//! the established public integration paths stable.

mod metrics;
mod normalize;
mod rest;
mod runtime;

pub use metrics::render_prometheus;
pub use rest::DiscordInboundReadinessRest;
pub use runtime::DiscordInboundRuntime;
