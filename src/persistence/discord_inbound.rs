//! Durable PBI-080 Discord inbound event application.
//!
//! Provider frames are normalized before they reach this store.  The store then resolves the
//! tenant/thread binding from server-owned PBI-079/081 state and commits the inbox receipt,
//! canonical feedback mutation, external-message correlation, and health transition in one
//! immediate SQLite transaction.  It never calls Discord and never enqueues outbound delivery.

mod apply;
mod common;
mod store;
mod validation;

pub use store::{
    DiscordInboundMetricsSnapshot, DiscordInboundStore, GatewayOrganizationTarget, GatewayResume,
    InboundPolicyStatus,
};
