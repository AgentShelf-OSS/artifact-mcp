//! Provider-neutral PBI-080 Discord inbound synchronization core.
//!
//! This is intentionally a provider-neutral state machine with deterministic fakes, not a
//! running Gateway client.  It accepts only server-side, already-authorized organization/thread
//! bindings and therefore cannot turn browser-provided Discord identifiers into access control.
//! PBI-081 supplies the credential and effective policy before the guarded production runtime can
//! start an organization Gateway task.

use crate::model::{ArtifactId, FeedbackAuthor, FeedbackId, OrgId};

/// Maximum imported plaintext body.  Content is bounded before it reaches the canonical store.
pub const MAX_INBOUND_BODY_BYTES: usize = 4_000;
/// The literal replaces a deleted remote body.  The original body is not retained in this core.
pub const DELETION_TOMBSTONE: &str = "This Discord message was deleted.";

/// PBI-081 plus artifact-level authorization resolved on the server, never from a Gateway frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TwoWayThreadAuthorization {
    pub org: OrgId,
    pub artifact_id: ArtifactId,
    pub guild_id: String,
    pub thread_id: String,
    pub enabled: bool,
    pub configured_bot_user_id: String,
    pub configured_webhook_id: Option<String>,
}

/// A normalized Discord author.  Its id/display are opaque provider identifiers, never emails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordAuthor {
    pub id: String,
    pub display: String,
    pub is_bot: bool,
    pub webhook_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordMessage {
    pub id: String,
    pub guild_id: String,
    pub thread_id: String,
    pub author: DiscordAuthor,
    pub content: Option<String>,
    /// Discord's `message_reference.message_id`, if present.
    pub reply_to_message_id: Option<String>,
    /// A monotonically comparable provider timestamp/version supplied by the adapter.
    pub version: i64,
    pub created_at: Option<String>,
    pub edited_at: Option<String>,
    pub supported_text: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboundEventKind {
    MessageCreate,
    MessageUpdate,
    MessageDelete,
    ThreadUpdate { archived: bool, locked: bool },
    ThreadDelete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundEvent {
    pub event_id: String,
    /// Gateway session namespace. Dispatch identifiers are not assumed globally unique across
    /// reconnects, organizations, or providers.
    pub gateway_session_id: String,
    /// The adapter's organization routing context.  It is checked against the stored binding.
    pub org: OrgId,
    pub kind: InboundEventKind,
    pub message: Option<DiscordMessage>,
    pub guild_id: String,
    pub thread_id: String,
    /// A hash/opaque fingerprint only.  Raw provider payload is intentionally not retained here.
    pub payload_fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundResult {
    Applied,
    Duplicate,
    Ignored(IgnoreReason),
    Rejected(RejectReason),
    NeedsFetch,
    Degraded(ThreadDegradedReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreReason {
    Disabled,
    Unmapped,
    Bot,
    Webhook,
    Unsupported,
    EmptyContent,
    Stale,
    UnknownMessage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    CrossTenant,
    WrongGuild,
    WrongThread,
    InvalidEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadDegradedReason {
    Deleted,
    ArchivedOrLocked,
}

/// Optional integration health, deliberately distinct from process liveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayHealth {
    Disabled,
    Connecting,
    Ready,
    Reconnecting,
    Degraded,
    Failed,
    Shutdown,
}

/// A kill switch stops only inbound consumption.  Local feedback and the outbound durable outbox
/// continue to function regardless of this state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboundIntegrationState {
    pub enabled: bool,
    pub health: GatewayHealth,
}

impl Default for InboundIntegrationState {
    fn default() -> Self {
        Self {
            enabled: false,
            health: GatewayHealth::Disabled,
        }
    }
}

/// Persisted/projection-safe inbound feedback state used by the repository adapter at integration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundFeedback {
    pub id: FeedbackId,
    pub artifact_id: ArtifactId,
    pub org: OrgId,
    pub parent_id: Option<FeedbackId>,
    pub author: FeedbackAuthor,
    pub body: String,
    pub external_message_id: String,
    pub provider_version: i64,
    pub external_created_at: Option<String>,
    pub external_edited_at: Option<String>,
    pub external_deleted_at: Option<String>,
}

/// Narrow REST port.  Only a partial `MESSAGE_UPDATE` asks for a complete current message.
pub trait DiscordRestPort: Send + Sync {
    fn fetch_message(
        &self,
        guild_id: &str,
        thread_id: &str,
        message_id: &str,
    ) -> Result<Option<DiscordMessage>, RestError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestError {
    RateLimited,
    Unavailable,
    Forbidden,
    NotFound,
}

/// Persistence adapter contract for the future SQL repository.  Every implementation must write
/// the inbox event and feedback/link mutation atomically; the in-memory implementation below is
/// only a deterministic test double.
pub trait InboundEventInbox: Send {
    fn has_event(&self, event: &InboundEvent) -> bool;
    fn record_event(&mut self, event: &InboundEvent, result: InboundResult);
    fn find_message(&self, external_message_id: &str) -> Option<&InboundFeedback>;
    fn put_feedback(&mut self, feedback: InboundFeedback);
    fn mark_thread_degraded(&mut self, reason: ThreadDegradedReason);
}

mod fakes;
mod gateway;
mod processor;

pub use fakes::*;
pub use gateway::*;
pub use processor::*;

#[cfg(test)]
mod tests;
