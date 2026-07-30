//! Provider-neutral PBI-080 Discord inbound synchronization core.
//!
//! This is intentionally a provider-neutral state machine with deterministic fakes, not a
//! running Gateway client.  It accepts only server-side, already-authorized organization/thread
//! bindings and therefore cannot turn browser-provided Discord identifiers into access control.
//! PBI-081 supplies the credential and effective policy before the guarded production runtime can
//! start an organization Gateway task.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Mutex,
};

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

/// Pure inbound application service.  It has no credential and no outbound enqueue capability,
/// which structurally prevents inbound content being echoed back to Discord.
#[derive(Clone, Debug)]
pub struct InboundProcessor {
    state: InboundIntegrationState,
}

impl InboundProcessor {
    #[must_use]
    pub const fn new(state: InboundIntegrationState) -> Self {
        Self { state }
    }

    pub fn set_state(&mut self, state: InboundIntegrationState) {
        self.state = state;
    }

    pub fn process(
        &self,
        authorization: Option<&TwoWayThreadAuthorization>,
        event: InboundEvent,
        inbox: &mut dyn InboundEventInbox,
        rest: &dyn DiscordRestPort,
    ) -> InboundResult {
        if inbox.has_event(&event) {
            return InboundResult::Duplicate;
        }
        let result = self.process_once(authorization, &event, inbox, rest);
        // A partial update that could not be hydrated is retryable, not a terminal inbox receipt.
        // Recording it would turn the retry into `Duplicate` and permanently lose the edit.
        if result != InboundResult::NeedsFetch {
            inbox.record_event(&event, result);
        }
        result
    }

    fn process_once(
        &self,
        authorization: Option<&TwoWayThreadAuthorization>,
        event: &InboundEvent,
        inbox: &mut dyn InboundEventInbox,
        rest: &dyn DiscordRestPort,
    ) -> InboundResult {
        if !self.state.enabled {
            return InboundResult::Ignored(IgnoreReason::Disabled);
        }
        let Some(binding) = authorization else {
            return InboundResult::Ignored(IgnoreReason::Unmapped);
        };
        if event.org != binding.org {
            return InboundResult::Rejected(RejectReason::CrossTenant);
        }
        if event.guild_id != binding.guild_id {
            return InboundResult::Rejected(RejectReason::WrongGuild);
        }
        if event.thread_id != binding.thread_id {
            return InboundResult::Rejected(RejectReason::WrongThread);
        }
        if !binding.enabled {
            return InboundResult::Ignored(IgnoreReason::Disabled);
        }

        match &event.kind {
            InboundEventKind::ThreadDelete => {
                inbox.mark_thread_degraded(ThreadDegradedReason::Deleted);
                InboundResult::Degraded(ThreadDegradedReason::Deleted)
            }
            InboundEventKind::ThreadUpdate { archived, locked } if *archived || *locked => {
                inbox.mark_thread_degraded(ThreadDegradedReason::ArchivedOrLocked);
                InboundResult::Degraded(ThreadDegradedReason::ArchivedOrLocked)
            }
            InboundEventKind::ThreadUpdate { .. } => {
                InboundResult::Ignored(IgnoreReason::Unsupported)
            }
            InboundEventKind::MessageCreate => self.create(binding, event.message.as_ref(), inbox),
            InboundEventKind::MessageUpdate => {
                self.update(binding, event.message.as_ref(), inbox, rest)
            }
            InboundEventKind::MessageDelete => self.delete(event.message.as_ref(), inbox),
        }
    }

    fn create(
        &self,
        binding: &TwoWayThreadAuthorization,
        message: Option<&DiscordMessage>,
        inbox: &mut dyn InboundEventInbox,
    ) -> InboundResult {
        let Some(message) = message else {
            return InboundResult::Rejected(RejectReason::InvalidEvent);
        };
        if let Some(result) = filter_author(binding, message) {
            return result;
        }
        if !message.supported_text {
            return InboundResult::Ignored(IgnoreReason::Unsupported);
        }
        let Some(body) = valid_body(message.content.as_deref()) else {
            return InboundResult::Ignored(IgnoreReason::EmptyContent);
        };
        if inbox.find_message(&message.id).is_some() {
            return InboundResult::Duplicate;
        }
        let parent_id = message.reply_to_message_id.as_deref().and_then(|id| {
            inbox.find_message(id).map(|feedback| {
                feedback
                    .parent_id
                    .clone()
                    .unwrap_or_else(|| feedback.id.clone())
            })
        });
        inbox.put_feedback(InboundFeedback {
            id: FeedbackId::from(format!("discord:{}", message.id)),
            artifact_id: binding.artifact_id.clone(),
            org: binding.org.clone(),
            parent_id,
            author: FeedbackAuthor::Discord {
                external_author_id: message.author.id.clone(),
                external_author_display: message.author.display.clone(),
            },
            body,
            external_message_id: message.id.clone(),
            provider_version: message.version,
            external_created_at: message.created_at.clone(),
            external_edited_at: message.edited_at.clone(),
            external_deleted_at: None,
        });
        InboundResult::Applied
    }

    fn update(
        &self,
        binding: &TwoWayThreadAuthorization,
        partial: Option<&DiscordMessage>,
        inbox: &mut dyn InboundEventInbox,
        rest: &dyn DiscordRestPort,
    ) -> InboundResult {
        let Some(partial) = partial else {
            return InboundResult::Rejected(RejectReason::InvalidEvent);
        };
        if let Some(result) = filter_author(binding, partial) {
            return result;
        }
        let message = match partial.content.as_ref() {
            Some(_) => partial.clone(),
            None => match rest.fetch_message(&partial.guild_id, &partial.thread_id, &partial.id) {
                Ok(Some(full)) => full,
                Ok(None) | Err(RestError::NotFound) => {
                    return InboundResult::Ignored(IgnoreReason::UnknownMessage);
                }
                Err(RestError::RateLimited | RestError::Unavailable | RestError::Forbidden) => {
                    return InboundResult::NeedsFetch;
                }
            },
        };
        let Some(existing) = inbox.find_message(&message.id).cloned() else {
            return InboundResult::Ignored(IgnoreReason::UnknownMessage);
        };
        if message.version <= existing.provider_version {
            return InboundResult::Ignored(IgnoreReason::Stale);
        }
        let Some(body) = valid_body(message.content.as_deref()) else {
            return InboundResult::Ignored(IgnoreReason::EmptyContent);
        };
        inbox.put_feedback(InboundFeedback {
            body,
            provider_version: message.version,
            external_edited_at: message.edited_at.clone(),
            ..existing
        });
        InboundResult::Applied
    }

    fn delete(
        &self,
        message: Option<&DiscordMessage>,
        inbox: &mut dyn InboundEventInbox,
    ) -> InboundResult {
        let Some(message) = message else {
            return InboundResult::Rejected(RejectReason::InvalidEvent);
        };
        let Some(existing) = inbox.find_message(&message.id).cloned() else {
            return InboundResult::Ignored(IgnoreReason::UnknownMessage);
        };
        if message.version <= existing.provider_version {
            return InboundResult::Ignored(IgnoreReason::Stale);
        }
        inbox.put_feedback(InboundFeedback {
            body: DELETION_TOMBSTONE.to_owned(),
            provider_version: message.version,
            external_deleted_at: message
                .edited_at
                .clone()
                .or_else(|| Some("provider-delete".into())),
            ..existing
        });
        InboundResult::Applied
    }
}

fn filter_author(
    binding: &TwoWayThreadAuthorization,
    message: &DiscordMessage,
) -> Option<InboundResult> {
    if message.author.is_bot || message.author.id == binding.configured_bot_user_id {
        return Some(InboundResult::Ignored(IgnoreReason::Bot));
    }
    if message.author.webhook_id.is_some() {
        return Some(InboundResult::Ignored(IgnoreReason::Webhook));
    }
    None
}

fn valid_body(value: Option<&str>) -> Option<String> {
    let body = value?.trim();
    if body.is_empty() || body.len() > MAX_INBOUND_BODY_BYTES {
        None
    } else {
        Some(body.to_owned())
    }
}

/// Gateway commands capture official lifecycle constraints without a networking dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayCommand {
    Identify { intents: u64 },
    Resume { session_id: String, sequence: u64 },
    Heartbeat { sequence: Option<u64> },
    Reconnect,
    Shutdown,
}

/// Readiness is an optional integration signal. Missing intents or permissions block only inbound
/// activation; Artifact MCP's local feedback/liveness stays available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscordReadiness {
    Ready,
    MissingCredential,
    MissingMessageContentIntent,
    MissingGuildAccess,
    MissingThreadPermission,
    InvalidOrDisallowedIntents,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GatewayResumeState {
    pub session_id: Option<String>,
    pub resume_gateway_url: Option<String>,
    pub last_sequence: Option<u64>,
    pub awaiting_heartbeat_ack: bool,
}

/// Pure official-protocol lifecycle model. It has no socket, timer, credential, or app-liveness
/// ownership; deterministic tests use it to pin the behavior delegated to the maintained client.
#[derive(Clone, Debug)]
pub struct GatewayLifecycle {
    intents: u64,
    pub state: GatewayResumeState,
}

impl GatewayLifecycle {
    #[must_use]
    pub const fn new(intents: u64) -> Self {
        Self {
            intents,
            state: GatewayResumeState {
                session_id: None,
                resume_gateway_url: None,
                last_sequence: None,
                awaiting_heartbeat_ack: false,
            },
        }
    }

    #[must_use]
    pub const fn readiness(
        credential_present: bool,
        message_content_enabled: bool,
        guild_access: bool,
        thread_permission: bool,
        intent_valid: bool,
    ) -> DiscordReadiness {
        if !credential_present {
            DiscordReadiness::MissingCredential
        } else if !intent_valid {
            DiscordReadiness::InvalidOrDisallowedIntents
        } else if !message_content_enabled {
            DiscordReadiness::MissingMessageContentIntent
        } else if !guild_access {
            DiscordReadiness::MissingGuildAccess
        } else if !thread_permission {
            DiscordReadiness::MissingThreadPermission
        } else {
            DiscordReadiness::Ready
        }
    }

    /// READY supplies resume state; a non-resumable INVALID_SESSION clears it and re-identifies.
    #[must_use]
    pub fn on_frame(&mut self, frame: &GatewayFrame) -> Option<GatewayCommand> {
        match frame {
            GatewayFrame::Hello { .. } => {
                match (&self.state.session_id, self.state.last_sequence) {
                    (Some(session_id), Some(sequence)) => Some(GatewayCommand::Resume {
                        session_id: session_id.clone(),
                        sequence,
                    }),
                    _ => Some(GatewayCommand::Identify {
                        intents: self.intents,
                    }),
                }
            }
            GatewayFrame::Ready {
                session_id,
                resume_gateway_url,
            } => {
                self.state.session_id = Some(session_id.clone());
                self.state.resume_gateway_url = Some(resume_gateway_url.clone());
                None
            }
            GatewayFrame::Dispatch { sequence, .. } => {
                self.state.last_sequence = Some(*sequence);
                None
            }
            GatewayFrame::HeartbeatAck => {
                self.state.awaiting_heartbeat_ack = false;
                None
            }
            GatewayFrame::Reconnect => Some(GatewayCommand::Reconnect),
            GatewayFrame::InvalidSession { resumable: false } => {
                self.state = GatewayResumeState::default();
                Some(GatewayCommand::Identify {
                    intents: self.intents,
                })
            }
            GatewayFrame::InvalidSession { resumable: true } => Some(GatewayCommand::Reconnect),
            GatewayFrame::RateLimited { .. } | GatewayFrame::Closed => None,
        }
    }

    /// A missed ACK requires reconnect; otherwise heartbeat carries the last dispatch sequence.
    #[must_use]
    pub fn heartbeat_due(&mut self) -> GatewayCommand {
        if self.state.awaiting_heartbeat_ack {
            GatewayCommand::Reconnect
        } else {
            self.state.awaiting_heartbeat_ack = true;
            GatewayCommand::Heartbeat {
                sequence: self.state.last_sequence,
            }
        }
    }
}

/// Deterministic frame subset for testing a future maintained client adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayFrame {
    Hello {
        heartbeat_interval_ms: u64,
    },
    Ready {
        session_id: String,
        resume_gateway_url: String,
    },
    Dispatch {
        sequence: u64,
        event: Box<InboundEvent>,
    },
    HeartbeatAck,
    Reconnect,
    InvalidSession {
        resumable: bool,
    },
    RateLimited {
        retry_after_ms: u64,
    },
    Closed,
}

/// Provider-neutral Gateway test port. Production uses `twilight-gateway`; this contract keeps
/// deterministic lifecycle scenarios free of sockets, credentials, and process ownership.
pub trait DiscordGatewayPort: Send + Sync {
    fn next_frame(&self) -> Option<GatewayFrame>;
    fn send(&self, command: GatewayCommand);
}

/// Scriptable fake Gateway: records Identify/Resume/Heartbeat/Reconnect/Shutdown in exact order.
#[derive(Default)]
pub struct FakeDiscordGateway {
    frames: Mutex<VecDeque<GatewayFrame>>,
    commands: Mutex<Vec<GatewayCommand>>,
}

impl FakeDiscordGateway {
    #[must_use]
    pub fn scripted(frames: impl IntoIterator<Item = GatewayFrame>) -> Self {
        Self {
            frames: Mutex::new(frames.into_iter().collect()),
            commands: Mutex::new(Vec::new()),
        }
    }
    #[must_use]
    pub fn commands(&self) -> Vec<GatewayCommand> {
        self.commands.lock().expect("fake gateway mutex").clone()
    }
}
impl DiscordGatewayPort for FakeDiscordGateway {
    fn next_frame(&self) -> Option<GatewayFrame> {
        self.frames.lock().expect("fake gateway mutex").pop_front()
    }
    fn send(&self, command: GatewayCommand) {
        self.commands
            .lock()
            .expect("fake gateway mutex")
            .push(command);
    }
}

/// Deterministic fake REST source used by partial-update and rate-limit tests.
type DiscordMessageKey = (String, String, String);
type FakeRestReply = Result<DiscordMessage, RestError>;

#[derive(Default)]
pub struct FakeDiscordRest {
    messages: Mutex<BTreeMap<DiscordMessageKey, FakeRestReply>>,
}
impl FakeDiscordRest {
    pub fn put(&self, message: DiscordMessage) {
        self.messages.lock().expect("fake rest mutex").insert(
            (
                message.guild_id.clone(),
                message.thread_id.clone(),
                message.id.clone(),
            ),
            Ok(message),
        );
    }
    pub fn fail(&self, guild: &str, thread: &str, message: &str, error: RestError) {
        self.messages
            .lock()
            .expect("fake rest mutex")
            .insert((guild.into(), thread.into(), message.into()), Err(error));
    }
}
impl DiscordRestPort for FakeDiscordRest {
    fn fetch_message(
        &self,
        guild: &str,
        thread: &str,
        message: &str,
    ) -> Result<Option<DiscordMessage>, RestError> {
        match self
            .messages
            .lock()
            .expect("fake rest mutex")
            .get(&(guild.into(), thread.into(), message.into()))
            .cloned()
        {
            Some(result) => result.map(Some),
            None => Ok(None),
        }
    }
}

/// In-memory transactional-shaped fake.  It retains fingerprints/results only, not raw content.
#[derive(Default)]
pub struct MemoryInbox {
    events: BTreeSet<(String, String, String)>,
    pub results: Vec<(String, InboundResult)>,
    pub feedback: BTreeMap<String, InboundFeedback>,
    pub degraded: Vec<ThreadDegradedReason>,
}
impl InboundEventInbox for MemoryInbox {
    fn has_event(&self, event: &InboundEvent) -> bool {
        self.events.contains(&(
            event.org.0.clone(),
            event.gateway_session_id.clone(),
            event.event_id.clone(),
        ))
    }
    fn record_event(&mut self, event: &InboundEvent, result: InboundResult) {
        self.events.insert((
            event.org.0.clone(),
            event.gateway_session_id.clone(),
            event.event_id.clone(),
        ));
        self.results
            .push((event.payload_fingerprint.clone(), result));
    }
    fn find_message(&self, external_message_id: &str) -> Option<&InboundFeedback> {
        self.feedback.get(external_message_id)
    }
    fn put_feedback(&mut self, feedback: InboundFeedback) {
        self.feedback
            .insert(feedback.external_message_id.clone(), feedback);
    }
    fn mark_thread_degraded(&mut self, reason: ThreadDegradedReason) {
        self.degraded.push(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> TwoWayThreadAuthorization {
        TwoWayThreadAuthorization {
            org: OrgId::from("acme"),
            artifact_id: ArtifactId::from("artifact-a"),
            guild_id: "guild-a".into(),
            thread_id: "thread-a".into(),
            enabled: true,
            configured_bot_user_id: "bot-a".into(),
            configured_webhook_id: Some("hook-a".into()),
        }
    }
    fn message(id: &str, body: Option<&str>, version: i64) -> DiscordMessage {
        DiscordMessage {
            id: id.into(),
            guild_id: "guild-a".into(),
            thread_id: "thread-a".into(),
            author: DiscordAuthor {
                id: "human-a".into(),
                display: "A Person".into(),
                is_bot: false,
                webhook_id: None,
            },
            content: body.map(str::to_owned),
            reply_to_message_id: None,
            version,
            created_at: Some("2026-07-30T00:00:00Z".into()),
            edited_at: None,
            supported_text: true,
        }
    }
    fn event(id: &str, kind: InboundEventKind, message: Option<DiscordMessage>) -> InboundEvent {
        InboundEvent {
            event_id: id.into(),
            gateway_session_id: "session-a".into(),
            org: OrgId::from("acme"),
            kind,
            message,
            guild_id: "guild-a".into(),
            thread_id: "thread-a".into(),
            payload_fingerprint: format!("hash-{id}"),
        }
    }
    fn processor() -> InboundProcessor {
        InboundProcessor::new(InboundIntegrationState {
            enabled: true,
            health: GatewayHealth::Ready,
        })
    }

    #[test]
    fn human_create_is_external_identity_once_and_never_has_an_email_projection() {
        let mut inbox = MemoryInbox::default();
        let rest = FakeDiscordRest::default();
        let create = event(
            "create-1",
            InboundEventKind::MessageCreate,
            Some(message("message-1", Some("hello"), 1)),
        );
        assert_eq!(
            processor().process(Some(&binding()), create.clone(), &mut inbox, &rest),
            InboundResult::Applied
        );
        assert_eq!(
            processor().process(Some(&binding()), create, &mut inbox, &rest),
            InboundResult::Duplicate
        );
        let feedback = inbox.feedback.get("message-1").unwrap();
        assert_eq!(feedback.author.verified_viewer_email(), None);
        assert_eq!(feedback.parent_id, None);
        assert_eq!(inbox.feedback.len(), 1);
    }

    #[test]
    fn reply_to_a_reply_normalizes_to_the_top_level_parent() {
        let mut inbox = MemoryInbox::default();
        let rest = FakeDiscordRest::default();
        let p = processor();
        p.process(
            Some(&binding()),
            event(
                "root",
                InboundEventKind::MessageCreate,
                Some(message("root", Some("root"), 1)),
            ),
            &mut inbox,
            &rest,
        );
        let mut reply = message("reply", Some("reply"), 1);
        reply.reply_to_message_id = Some("root".into());
        p.process(
            Some(&binding()),
            event("reply", InboundEventKind::MessageCreate, Some(reply)),
            &mut inbox,
            &rest,
        );
        let mut nested = message("nested", Some("nested"), 1);
        nested.reply_to_message_id = Some("reply".into());
        assert_eq!(
            p.process(
                Some(&binding()),
                event("nested", InboundEventKind::MessageCreate, Some(nested)),
                &mut inbox,
                &rest
            ),
            InboundResult::Applied
        );
        assert_eq!(
            inbox.feedback["nested"].parent_id,
            Some(FeedbackId::from("discord:root"))
        );
    }

    #[test]
    fn filters_automated_unmapped_disabled_and_cross_tenant_events_without_content_retention() {
        let rest = FakeDiscordRest::default();
        let mut inbox = MemoryInbox::default();
        let p = processor();
        let mut bot = message("bot", Some("secret text"), 1);
        bot.author.is_bot = true;
        assert_eq!(
            p.process(
                Some(&binding()),
                event("bot", InboundEventKind::MessageCreate, Some(bot)),
                &mut inbox,
                &rest
            ),
            InboundResult::Ignored(IgnoreReason::Bot)
        );
        assert_eq!(
            p.process(
                None,
                event(
                    "none",
                    InboundEventKind::MessageCreate,
                    Some(message("m", Some("secret text"), 1))
                ),
                &mut inbox,
                &rest
            ),
            InboundResult::Ignored(IgnoreReason::Unmapped)
        );
        let mut foreign = event(
            "foreign",
            InboundEventKind::MessageCreate,
            Some(message("f", Some("secret text"), 1)),
        );
        foreign.org = OrgId::from("other");
        assert_eq!(
            p.process(Some(&binding()), foreign, &mut inbox, &rest),
            InboundResult::Rejected(RejectReason::CrossTenant)
        );
        assert!(inbox.feedback.is_empty());
    }

    #[test]
    fn configured_webhook_messages_are_ignored_and_never_loop_back_as_feedback() {
        let rest = FakeDiscordRest::default();
        let mut inbox = MemoryInbox::default();
        let mut webhook = message("webhook", Some("outbound echo"), 1);
        webhook.author.webhook_id = Some("hook-a".into());
        assert_eq!(
            processor().process(
                Some(&binding()),
                event("webhook", InboundEventKind::MessageCreate, Some(webhook)),
                &mut inbox,
                &rest
            ),
            InboundResult::Ignored(IgnoreReason::Webhook)
        );
        assert!(inbox.feedback.is_empty());
    }

    #[test]
    fn crash_between_feedback_and_inbox_commit_recovers_without_losing_or_duplicating_create() {
        let rest = FakeDiscordRest::default();
        let mut inbox = MemoryInbox::default();
        // Simulate a pre-transaction legacy failure: feedback/link committed but the event receipt
        // did not. The real SQL adapter is required to make these one transaction; this proves a
        // replay also converges safely while an operator recovers such a partial state.
        let message = message("m", Some("survives replay"), 1);
        let feedback = InboundFeedback {
            id: FeedbackId::from("discord:m"),
            artifact_id: ArtifactId::from("artifact-a"),
            org: OrgId::from("acme"),
            parent_id: None,
            author: FeedbackAuthor::Discord {
                external_author_id: "human-a".into(),
                external_author_display: "A Person".into(),
            },
            body: "survives replay".into(),
            external_message_id: "m".into(),
            provider_version: 1,
            external_created_at: None,
            external_edited_at: None,
            external_deleted_at: None,
        };
        inbox.put_feedback(feedback);
        assert_eq!(
            processor().process(
                Some(&binding()),
                event(
                    "replayed-create",
                    InboundEventKind::MessageCreate,
                    Some(message)
                ),
                &mut inbox,
                &rest
            ),
            InboundResult::Duplicate
        );
        assert_eq!(inbox.feedback.len(), 1);
        assert_eq!(inbox.results.len(), 1);
    }

    #[test]
    fn partial_update_fetches_current_message_and_out_of_order_update_cannot_regress_body() {
        let mut inbox = MemoryInbox::default();
        let rest = FakeDiscordRest::default();
        let p = processor();
        p.process(
            Some(&binding()),
            event(
                "create",
                InboundEventKind::MessageCreate,
                Some(message("m", Some("old"), 1)),
            ),
            &mut inbox,
            &rest,
        );
        let mut full = message("m", Some("new"), 3);
        full.edited_at = Some("2026-07-30T01:00:00Z".into());
        rest.put(full);
        assert_eq!(
            p.process(
                Some(&binding()),
                event(
                    "update-new",
                    InboundEventKind::MessageUpdate,
                    Some(message("m", None, 3))
                ),
                &mut inbox,
                &rest
            ),
            InboundResult::Applied
        );
        assert_eq!(
            p.process(
                Some(&binding()),
                event(
                    "update-old",
                    InboundEventKind::MessageUpdate,
                    Some(message("m", Some("stale"), 2))
                ),
                &mut inbox,
                &rest
            ),
            InboundResult::Ignored(IgnoreReason::Stale)
        );
        assert_eq!(inbox.feedback["m"].body, "new");
    }

    #[test]
    fn delete_tombstones_without_retaining_body_and_thread_loss_only_degrades_sync() {
        let mut inbox = MemoryInbox::default();
        let rest = FakeDiscordRest::default();
        let p = processor();
        p.process(
            Some(&binding()),
            event(
                "create",
                InboundEventKind::MessageCreate,
                Some(message("m", Some("remove me"), 1)),
            ),
            &mut inbox,
            &rest,
        );
        let mut deleted = message("m", None, 2);
        deleted.edited_at = Some("2026-07-30T02:00:00Z".into());
        assert_eq!(
            p.process(
                Some(&binding()),
                event("delete", InboundEventKind::MessageDelete, Some(deleted)),
                &mut inbox,
                &rest
            ),
            InboundResult::Applied
        );
        assert_eq!(inbox.feedback["m"].body, DELETION_TOMBSTONE);
        assert_eq!(
            p.process(
                Some(&binding()),
                event("thread-delete", InboundEventKind::ThreadDelete, None),
                &mut inbox,
                &rest
            ),
            InboundResult::Degraded(ThreadDegradedReason::Deleted)
        );
        assert_eq!(inbox.feedback.len(), 1);
    }

    #[test]
    fn locked_or_archived_thread_degrades_but_preserves_canonical_feedback() {
        let mut inbox = MemoryInbox::default();
        let rest = FakeDiscordRest::default();
        let p = processor();
        p.process(
            Some(&binding()),
            event(
                "create",
                InboundEventKind::MessageCreate,
                Some(message("m", Some("persist"), 1)),
            ),
            &mut inbox,
            &rest,
        );
        assert_eq!(
            p.process(
                Some(&binding()),
                event(
                    "locked",
                    InboundEventKind::ThreadUpdate {
                        archived: false,
                        locked: true
                    },
                    None
                ),
                &mut inbox,
                &rest
            ),
            InboundResult::Degraded(ThreadDegradedReason::ArchivedOrLocked)
        );
        assert_eq!(inbox.feedback["m"].body, "persist");
    }

    #[test]
    fn fake_gateway_records_identify_resume_reconnect_invalid_session_and_shutdown_contract() {
        let gateway = FakeDiscordGateway::scripted([
            GatewayFrame::Hello {
                heartbeat_interval_ms: 45_000,
            },
            GatewayFrame::Ready {
                session_id: "session".into(),
                resume_gateway_url: "wss://resume".into(),
            },
            GatewayFrame::Reconnect,
            GatewayFrame::InvalidSession { resumable: false },
            GatewayFrame::Closed,
        ]);
        gateway.send(GatewayCommand::Identify {
            intents: 1 | 1 << 9 | 1 << 15,
        }); // GUILDS, GUILD_MESSAGES, MESSAGE_CONTENT
        gateway.send(GatewayCommand::Heartbeat { sequence: Some(9) });
        gateway.send(GatewayCommand::Resume {
            session_id: "session".into(),
            sequence: 9,
        });
        gateway.send(GatewayCommand::Reconnect);
        gateway.send(GatewayCommand::Identify {
            intents: 1 | 1 << 9 | 1 << 15,
        }); // invalid-session false re-identifies
        gateway.send(GatewayCommand::Shutdown);
        assert!(matches!(
            gateway.next_frame(),
            Some(GatewayFrame::Hello { .. })
        ));
        assert_eq!(gateway.commands().len(), 6);
    }

    #[test]
    fn gateway_lifecycle_caches_sequence_resumes_reconnects_on_missed_ack_and_reidentifies() {
        let mut lifecycle = GatewayLifecycle::new(1 | 1 << 9 | 1 << 15);
        assert_eq!(
            lifecycle.on_frame(&GatewayFrame::Hello {
                heartbeat_interval_ms: 45_000
            }),
            Some(GatewayCommand::Identify {
                intents: 1 | 1 << 9 | 1 << 15
            })
        );
        assert_eq!(
            lifecycle.on_frame(&GatewayFrame::Ready {
                session_id: "session".into(),
                resume_gateway_url: "wss://resume".into()
            }),
            None
        );
        let dispatch = event(
            "dispatch",
            InboundEventKind::MessageCreate,
            Some(message("m", Some("body"), 1)),
        );
        let _ = lifecycle.on_frame(&GatewayFrame::Dispatch {
            sequence: 9,
            event: Box::new(dispatch),
        });
        assert_eq!(
            lifecycle.heartbeat_due(),
            GatewayCommand::Heartbeat { sequence: Some(9) }
        );
        assert_eq!(lifecycle.heartbeat_due(), GatewayCommand::Reconnect);
        assert_eq!(
            lifecycle.on_frame(&GatewayFrame::Hello {
                heartbeat_interval_ms: 45_000
            }),
            Some(GatewayCommand::Resume {
                session_id: "session".into(),
                sequence: 9
            })
        );
        assert_eq!(
            lifecycle.on_frame(&GatewayFrame::InvalidSession { resumable: false }),
            Some(GatewayCommand::Identify {
                intents: 1 | 1 << 9 | 1 << 15
            })
        );
        assert_eq!(lifecycle.state, GatewayResumeState::default());
    }

    #[test]
    fn readiness_distinguishes_intents_and_permissions_from_application_liveness() {
        assert_eq!(
            GatewayLifecycle::readiness(false, true, true, true, true),
            DiscordReadiness::MissingCredential
        );
        assert_eq!(
            GatewayLifecycle::readiness(true, false, true, true, true),
            DiscordReadiness::MissingMessageContentIntent
        );
        assert_eq!(
            GatewayLifecycle::readiness(true, true, true, true, false),
            DiscordReadiness::InvalidOrDisallowedIntents
        );
        assert_eq!(
            GatewayLifecycle::readiness(true, true, true, true, true),
            DiscordReadiness::Ready
        );
    }

    #[test]
    fn rate_limited_partial_update_is_deferred_and_kill_switch_keeps_inbox_empty() {
        let mut inbox = MemoryInbox::default();
        let rest = FakeDiscordRest::default();
        rest.fail("guild-a", "thread-a", "m", RestError::RateLimited);
        let p = processor();
        p.process(
            Some(&binding()),
            event(
                "create",
                InboundEventKind::MessageCreate,
                Some(message("m", Some("persist"), 1)),
            ),
            &mut inbox,
            &rest,
        );
        assert_eq!(
            p.process(
                Some(&binding()),
                event(
                    "update",
                    InboundEventKind::MessageUpdate,
                    Some(message("m", None, 3))
                ),
                &mut inbox,
                &rest
            ),
            InboundResult::NeedsFetch
        );
        let disabled = InboundProcessor::new(InboundIntegrationState::default());
        assert_eq!(
            disabled.process(
                Some(&binding()),
                event(
                    "disabled",
                    InboundEventKind::MessageCreate,
                    Some(message("new", Some("local survives"), 1))
                ),
                &mut inbox,
                &rest
            ),
            InboundResult::Ignored(IgnoreReason::Disabled)
        );
        assert_eq!(inbox.feedback.len(), 1);
    }
}
