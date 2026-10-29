//! Durable persistence for PBI-079's optional Discord discussion mirror.
//!
//! This module owns only local state. It deliberately does not perform Discord I/O, and it keeps
//! the bearer URL behind the same [`WebhookUrlProtection`] boundary used by event webhooks.
//! Absent `artifact_discussions` rows mean local-only; migration v28 never backfills them.
//!
//! The store is split by responsibility: `types` and `inputs` hold the public records,
//! `connections`, `mirror`, `links`, and `delivery` hold the `DiscussionStore` methods, `planning`
//! holds the helpers that run inside a feedback transaction, and the `*_rows` modules hold the
//! row mapping and shared queries.

mod connection_rows;
mod connections;
mod delivery;
mod discussion_rows;
mod inputs;
mod link_rows;
mod links;
mod mirror;
mod planning;
mod support;
mod types;

use std::sync::Arc;

use crate::{error::AppError, persistence::db::DbPool, security::crypto::WebhookUrlProtection};

pub use inputs::{
    AcceptedDiscussionDelivery, AcceptedDiscussionMarker, BindDiscussionTombstone,
    CreateDiscussionConnection, CreateDiscussionMessageLink, CreateNotificationThreadConnection,
    TerminalDiscussionDelivery,
};
pub use planning::discussion_ordering_key;
pub(crate) use planning::{
    ActiveDiscussionPlan, active_plan_in_transaction, create_link_in_transaction,
    latest_link_for_feedback_in_transaction, link_for_feedback_generation_in_transaction,
    mark_deleted_and_bind_tombstone_in_transaction, retained_tombstone_plan_in_transaction,
    root_outbox_in_transaction,
};
pub use types::{
    ArtifactDiscussion, DiscussionConnectionDelivery, DiscussionConnectionStrategy,
    DiscussionConnectionSummary, DiscussionMessageLink, DiscussionMode, DiscussionState,
};

/// PBI-079 repository. The store is intentionally independent from `WebhookStore`: a general
/// event webhook must never become a discussion target by accident.
#[derive(Clone)]
pub struct DiscussionStore {
    pool: DbPool,
    protection: Arc<WebhookUrlProtection>,
}

impl DiscussionStore {
    #[must_use]
    pub const fn new(pool: DbPool, protection: Arc<WebhookUrlProtection>) -> Self {
        Self { pool, protection }
    }
}

fn internal(error: rusqlite::Error) -> AppError {
    // SQLite's error does not interpolate parameter values. Keep the operation broad anyway so a
    // bearer URL cannot become part of an operator log through a future query refactor.
    tracing::error!(error = %error, "discord discussion persistence failed");
    AppError::Internal
}

impl std::fmt::Debug for DiscussionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscussionStore")
            .field("protection", self.protection.as_ref())
            .finish_non_exhaustive()
    }
}
