//! Durable PBI-080 Discord inbound event application.
//!
//! Provider frames are normalized before they reach this store. The store resolves the
//! tenant/thread binding and commits the receipt and canonical mutation in one transaction.

mod apply;
mod common;
mod store;
mod validation;

pub use store::{
    DiscordInboundMetricsSnapshot, DiscordInboundStore, GatewayOrganizationTarget, GatewayResume,
    InboundPolicyStatus,
};
