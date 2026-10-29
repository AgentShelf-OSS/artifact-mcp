//! Caller-supplied inputs for discussion connection, link, and delivery operations.

use crate::model::{ArtifactId, FeedbackId, OrgId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDiscussionConnection {
    pub org: OrgId,
    pub url: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateNotificationThreadConnection {
    pub org: OrgId,
    pub notification_webhook_id: String,
    pub notification_provider_webhook_id: String,
    pub channel_id: String,
    pub guild_id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDiscussionMessageLink {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub feedback_id: FeedbackId,
    pub delivery_event_id: String,
    pub outbox_id: String,
    pub external_thread_id: Option<String>,
    pub generation: u64,
}

/// A tombstone job attached to an already-posted feedback mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindDiscussionTombstone {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub feedback_id: FeedbackId,
    pub connection_id: String,
    pub generation: u64,
    pub outbox_id: String,
}

/// Inputs proven by the worker before one atomic accepted-result commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedDiscussionDelivery {
    pub outbox_id: String,
    pub worker: String,
    pub lease_token: String,
    pub lease_version: i64,
    pub external_thread_id: String,
    pub external_message_id: String,
    pub now_millis: i64,
}

/// Inputs proven by the worker before accepting a marker. Markers intentionally have no
/// message-link row; their durable receipt is the outbox row itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedDiscussionMarker {
    pub outbox_id: String,
    pub worker: String,
    pub lease_token: String,
    pub lease_version: i64,
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub generation: u64,
    pub message_id: String,
    pub now_millis: i64,
}

/// Guarded terminal update for a discussion delivery. The operation-specific durable state is
/// changed in the same SQLite transaction as the outbox terminal transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalDiscussionDelivery {
    pub outbox_id: String,
    pub worker: String,
    pub lease_token: String,
    pub lease_version: i64,
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub generation: u64,
    pub delivery_kind: String,
    pub feedback_id: Option<FeedbackId>,
    pub classification: String,
    pub duplicate_risk: bool,
    pub now_millis: i64,
}
