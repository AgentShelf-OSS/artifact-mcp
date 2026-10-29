//! Public discussion records and the closed enums stored in their text columns.

use crate::model::{ArtifactId, FeedbackId, OrgId, Timestamp};

/// The persisted mode. No row is the same as [`Self::ArtifactMcpOnly`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionMode {
    ArtifactMcpOnly,
    DiscordMirror,
}

impl DiscussionMode {
    pub(super) fn parse(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "artifact_only" => Ok(Self::ArtifactMcpOnly),
            "discord_mirror" => Ok(Self::DiscordMirror),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// Operator-visible state of an artifact discussion mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionState {
    Local,
    Pending,
    Connected,
    Paused,
    Failed,
}

impl DiscussionState {
    pub(super) fn parse(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "local" => Ok(Self::Local),
            "pending" => Ok(Self::Pending),
            "connected" => Ok(Self::Connected),
            "paused" => Ok(Self::Paused),
            "failed" => Ok(Self::Failed),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// How Discord discussion delivery is rooted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionConnectionStrategy {
    /// Legacy PBI-079 destination that creates a standalone Forum/Media post.
    ForumWebhook,
    /// A public thread attached to the existing artifact-published notification.
    NotificationThread,
}

impl DiscussionConnectionStrategy {
    pub(super) fn parse(value: String) -> rusqlite::Result<Self> {
        match value.as_str() {
            "forum_webhook" => Ok(Self::ForumWebhook),
            "notification_thread" => Ok(Self::NotificationThread),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// A settings-safe discussion connection representation. `destination` is always masked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionConnectionSummary {
    /// Opaque, immutable credential identity used by durable delivery correlations.
    pub id: String,
    pub org: OrgId,
    pub label: String,
    pub destination: String,
    pub strategy: DiscussionConnectionStrategy,
    pub notification_webhook_id: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub last_ok_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

/// The narrowly-scoped delivery capability. It must never be rendered or logged.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscussionConnectionDelivery {
    pub org: OrgId,
    pub label: String,
    pub url: String,
    pub strategy: DiscussionConnectionStrategy,
    pub notification_webhook_id: Option<String>,
    pub notification_provider_webhook_id: Option<String>,
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
}

impl std::fmt::Debug for DiscussionConnectionDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscussionConnectionDelivery")
            .field("org", &self.org)
            .field("label", &self.label)
            .field("url", &"<redacted>")
            .field("strategy", &self.strategy)
            .field("notification_webhook_id", &self.notification_webhook_id)
            .field(
                "notification_provider_webhook_id",
                &self.notification_provider_webhook_id,
            )
            .field("channel_id", &self.channel_id)
            .field("guild_id", &self.guild_id)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDiscussion {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub mode: DiscussionMode,
    pub connection_org: Option<OrgId>,
    pub connection_id: Option<String>,
    pub thread_id: Option<String>,
    pub root_message_id: Option<String>,
    pub state: DiscussionState,
    pub generation: u64,
    pub enabled_by: Option<String>,
    pub enabled_at: Option<Timestamp>,
    pub disabled_at: Option<Timestamp>,
    pub last_synced_at: Option<Timestamp>,
    pub last_error: Option<String>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub anchor_outbox_id: Option<String>,
}

impl ArtifactDiscussion {
    /// Construct the virtual state represented by a missing database row.
    #[must_use]
    pub fn local_only(artifact_id: ArtifactId, org: OrgId) -> Self {
        Self {
            artifact_id,
            org,
            mode: DiscussionMode::ArtifactMcpOnly,
            connection_org: None,
            connection_id: None,
            thread_id: None,
            root_message_id: None,
            state: DiscussionState::Local,
            generation: 0,
            enabled_by: None,
            enabled_at: None,
            disabled_at: None,
            last_synced_at: None,
            last_error: None,
            created_at: None,
            updated_at: None,
            anchor_outbox_id: None,
        }
    }
}

/// One persistent correlation between canonical feedback and one provider delivery job/message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionMessageLink {
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub connection_id: String,
    pub feedback_id: FeedbackId,
    pub delivery_event_id: String,
    pub outbox_id: String,
    pub tombstone_outbox_id: Option<String>,
    pub external_thread_id: Option<String>,
    pub external_message_id: Option<String>,
    pub generation: u64,
    pub state: String,
    pub last_error: Option<String>,
    pub local_deleted_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub posted_at: Option<Timestamp>,
}
