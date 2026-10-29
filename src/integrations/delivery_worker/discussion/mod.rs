//! The discussion turn: the worker-side settle and accept steps, and the types its stages share.

mod stages;

use std::time::Duration;

use self::stages::{decode_discussion, prepare_discussion};
use super::{
    DeliveryWorker, WorkerDiscussions,
    backoff::backoff,
    outcome::{guarded, retry_code, terminal_code},
};
use crate::{
    error::AppError,
    integrations::{
        discord_delivery::{DeliveryClassification, DuplicateRisk, RetryReason},
        discord_discussion::DiscussionResult,
        discussion_envelope::{DiscordDiscussionEnvelopeV1, DiscordDiscussionOperationV1},
    },
    model::{ArtifactId, OrgId},
    persistence::{
        discussions::{
            AcceptedDiscussionDelivery, AcceptedDiscussionMarker, TerminalDiscussionDelivery,
        },
        outbox::{DELIVERY_KIND_DISCUSSION_MESSAGE, DeliveryRecord, MAX_ATTEMPTS, RetryTransition},
    },
};

impl DeliveryWorker {
    /// One discussion turn: decode → authorize → resolve → build → send → settle. Every stage
    /// before the send is free of provider I/O, and every early exit performs exactly one guarded
    /// transition (or returns a storage error with none).
    pub(super) async fn process_discussion(&self, record: DeliveryRecord) -> Result<(), AppError> {
        let (Some(discussions), Some(provider)) = (&self.discussions, &self.discussion_provider)
        else {
            return self.dead_letter(&record, "validation_failed", false).await;
        };
        let (envelope, binding) = match decode_discussion(&record) {
            Ok(decoded) => decoded,
            Err(class) => return self.dead_letter(&record, class, false).await,
        };
        let kind = DiscussionKind::of(envelope.operation());
        let (request, targets) =
            match prepare_discussion(discussions.as_ref(), &envelope, kind, &binding).await {
                Ok(prepared) => prepared,
                Err(exit) => return self.settle_discussion(&binding, exit).await,
            };
        let outcome = provider.deliver(&binding.tenant, request).await;
        self.persist_discussion_rate_limit(&record, &outcome)
            .await?;
        self.apply_discussion_outcome(discussions.as_ref(), kind, &binding, targets, outcome)
            .await
    }

    /// Classify the provider outcome and persist it: accepted (with the thread recorded), retry,
    /// or terminal.
    async fn apply_discussion_outcome(
        &self,
        discussions: &dyn WorkerDiscussions,
        kind: DiscussionKind,
        binding: &DiscussionBinding<'_>,
        targets: DiscussionTargets,
        outcome: DiscussionResult,
    ) -> Result<(), AppError> {
        match outcome {
            DiscussionResult::Accepted { receipt, .. } => {
                let thread = match kind {
                    DiscussionKind::Thread => match receipt.thread_id {
                        Some(value) => value,
                        None => {
                            let exit = DiscussionExit::Retry {
                                class: "ambiguous",
                                duplicate_risk: true,
                                delay: backoff(binding.record.attempts),
                            };
                            return self.settle_discussion(binding, exit).await;
                        }
                    },
                    _ => targets.thread_id.unwrap_or_default(),
                };
                self.accept_discussion(discussions, kind, binding, thread, receipt.message_id)
                    .await
            }
            DiscussionResult::Retry {
                reason,
                duplicate_risk,
                rate_limit,
            } => {
                let delay = if reason == RetryReason::RateLimited {
                    rate_limit
                        .and_then(|value| value.retry_after_ms)
                        .map(Duration::from_millis)
                        .ok_or_else(|| {
                            AppError::Validation("rate_limited outcome missing retry_after".into())
                        })?
                } else {
                    backoff(binding.record.attempts)
                };
                let exit = DiscussionExit::Retry {
                    class: retry_code(reason),
                    duplicate_risk: duplicate_risk == DuplicateRisk::Possible,
                    delay,
                };
                self.settle_discussion(binding, exit).await
            }
            DiscussionResult::Terminal { reason } => {
                self.settle_discussion(binding, DiscussionExit::Terminal(terminal_code(reason)))
                    .await
            }
        }
    }

    /// Record an accepted send through the acceptor that owns this operation's durable state.
    async fn accept_discussion(
        &self,
        discussions: &dyn WorkerDiscussions,
        kind: DiscussionKind,
        binding: &DiscussionBinding<'_>,
        thread: String,
        message: String,
    ) -> Result<(), AppError> {
        let record = binding.record;
        let input = AcceptedDiscussionDelivery {
            outbox_id: record.id.clone(),
            worker: self.worker_id.clone(),
            lease_token: record.lease_token.clone().unwrap_or_default(),
            lease_version: record.lease_version,
            external_thread_id: thread,
            external_message_id: message,
            now_millis: self.clock.now_millis(),
        };
        let accepted = match kind {
            DiscussionKind::Thread | DiscussionKind::Reply => {
                discussions.accept_post(input).await?
            }
            DiscussionKind::Tombstone => discussions.accept_tombstone(input).await?,
            DiscussionKind::Marker => {
                discussions
                    .accept_marker(AcceptedDiscussionMarker {
                        outbox_id: input.outbox_id,
                        worker: input.worker,
                        lease_token: input.lease_token,
                        lease_version: input.lease_version,
                        artifact_id: binding.artifact.clone(),
                        org: binding.tenant.clone(),
                        connection_id: binding.connection.clone(),
                        generation: binding.generation,
                        message_id: input.external_message_id,
                        now_millis: input.now_millis,
                    })
                    .await?
            }
        };
        guarded(Ok(accepted), "discussion accepted")
    }

    /// Perform the single transition an early exit asks for.
    async fn settle_discussion(
        &self,
        binding: &DiscussionBinding<'_>,
        exit: DiscussionExit,
    ) -> Result<(), AppError> {
        match exit {
            DiscussionExit::Terminal(class) => {
                self.discussion_terminal(binding, class, false).await
            }
            DiscussionExit::Retry {
                class,
                duplicate_risk,
                delay,
            } => {
                self.retry_or_exhaust_discussion(binding, class, duplicate_risk, delay)
                    .await
            }
            DiscussionExit::Failed(error) => Err(error),
        }
    }

    async fn retry_or_exhaust_discussion(
        &self,
        binding: &DiscussionBinding<'_>,
        class: &str,
        duplicate_risk: bool,
        delay: Duration,
    ) -> Result<(), AppError> {
        let record = binding.record;
        if record.attempts >= MAX_ATTEMPTS {
            return self
                .discussion_terminal(binding, "attempts_exhausted", duplicate_risk)
                .await;
        }
        let next_attempt_at = self
            .clock
            .now_millis()
            .saturating_add(i64::try_from(delay.as_millis()).unwrap_or(i64::MAX));
        guarded(
            self.outbox
                .retry(
                    record,
                    self.worker_id.clone(),
                    RetryTransition {
                        next_attempt_at,
                        classification: class.to_owned(),
                        duplicate_risk,
                    },
                )
                .await,
            "retry",
        )
    }

    async fn discussion_terminal(
        &self,
        binding: &DiscussionBinding<'_>,
        class: &str,
        duplicate_risk: bool,
    ) -> Result<(), AppError> {
        let record = binding.record;
        let Some(discussions) = &self.discussions else {
            return self.dead_letter(record, class, duplicate_risk).await;
        };
        // Marker rows intentionally have no message-link row. Re-derive that operation class
        // from the canonical payload so a marker failure updates only the discussion's safe
        // status, while reply failures update their immutable feedback mapping.
        let feedback = if record.delivery_kind == DELIVERY_KIND_DISCUSSION_MESSAGE
            && serde_json::from_slice::<DiscordDiscussionEnvelopeV1>(&record.payload)
                .ok()
                .is_some_and(|envelope| {
                    matches!(
                        envelope.operation(),
                        DiscordDiscussionOperationV1::Resolved { .. }
                            | DiscordDiscussionOperationV1::Reopened { .. }
                    )
                }) {
            None
        } else {
            Some(binding.feedback.clone())
        };
        guarded(
            discussions
                .terminal(TerminalDiscussionDelivery {
                    outbox_id: record.id.clone(),
                    worker: self.worker_id.clone(),
                    lease_token: record.lease_token.clone().unwrap_or_default(),
                    lease_version: record.lease_version,
                    artifact_id: binding.artifact.clone(),
                    org: OrgId(record.tenant.clone()),
                    connection_id: binding.connection.clone(),
                    generation: binding.generation,
                    delivery_kind: record.delivery_kind.clone(),
                    feedback_id: feedback,
                    classification: class.to_owned(),
                    duplicate_risk,
                    now_millis: self.clock.now_millis(),
                })
                .await,
            "discussion dead-letter",
        )
    }

    async fn persist_discussion_rate_limit(
        &self,
        record: &DeliveryRecord,
        outcome: &DiscussionResult,
    ) -> Result<(), AppError> {
        let translated = match outcome {
            DiscussionResult::Accepted {
                receipt,
                rate_limit,
            } => DeliveryClassification::Accepted {
                message_id: receipt.message_id.clone(),
                rate_limit: rate_limit.clone(),
            },
            DiscussionResult::Retry {
                reason,
                duplicate_risk,
                rate_limit,
            } => DeliveryClassification::Retry {
                reason: *reason,
                duplicate_risk: *duplicate_risk,
                rate_limit: rate_limit.clone(),
            },
            DiscussionResult::Terminal { reason } => {
                DeliveryClassification::Terminal { reason: *reason }
            }
        };
        self.persist_rate_limit(record, &translated).await
    }
}

/// Discussion identity recovered from the canonical envelope. Every operation-scoped terminal or
/// retry update after decoding is bound to exactly these values.
struct DiscussionBinding<'r> {
    record: &'r DeliveryRecord,
    tenant: OrgId,
    artifact: ArtifactId,
    connection: String,
    generation: u64,
    feedback: crate::model::FeedbackId,
}

/// Why a discussion turn stops before the provider is called. Each variant maps to exactly one
/// outcome in `DeliveryWorker::settle_discussion`.
enum DiscussionExit {
    /// Guarded operation-scoped terminal update.
    Terminal(&'static str),
    /// Guarded retry, or terminal exhaustion once the attempt budget is spent.
    Retry {
        class: &'static str,
        duplicate_risk: bool,
        delay: Duration,
    },
    /// Storage failure, returned unchanged without a transition.
    Failed(AppError),
}

impl DiscussionExit {
    const INVALID: Self = Self::Terminal("validation_failed");
}

impl From<AppError> for DiscussionExit {
    fn from(error: AppError) -> Self {
        Self::Failed(error)
    }
}

/// The per-operation behaviour the worker needs: which external ids to resolve and which
/// acceptor records a successful send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscussionKind {
    /// Creates the external thread; accepted through the post acceptor.
    Thread,
    /// Posts into the persisted thread; accepted through the post acceptor.
    Reply,
    /// Resolved/reopened marker in the persisted thread; accepted through the marker acceptor.
    Marker,
    /// Edits the retained message; accepted through the tombstone acceptor.
    Tombstone,
}

impl DiscussionKind {
    const fn of(operation: &DiscordDiscussionOperationV1) -> Self {
        match operation {
            DiscordDiscussionOperationV1::Thread { .. } => Self::Thread,
            DiscordDiscussionOperationV1::Reply { .. } => Self::Reply,
            DiscordDiscussionOperationV1::Resolved { .. }
            | DiscordDiscussionOperationV1::Reopened { .. } => Self::Marker,
            DiscordDiscussionOperationV1::Tombstone { .. } => Self::Tombstone,
        }
    }
}

/// Authoritative external ids for the transport operation.
struct DiscussionTargets {
    thread_id: Option<String>,
    message_id: Option<String>,
}
