//! Narrow worker-side persistence and clock ports, with their production adapters.

use crate::{
    error::AppError,
    model::{ArtifactId, OrgId, WebhookDelivery, WebhookId},
    persistence::{
        discussions::{
            AcceptedDiscussionDelivery, AcceptedDiscussionMarker, ArtifactDiscussion,
            DiscussionConnectionDelivery, DiscussionMessageLink, DiscussionStore,
            TerminalDiscussionDelivery,
        },
        outbox::{DeadLetterTransition, DeliveryRecord, OutboxRepository, RetryTransition},
        webhooks::{WebhookDeliveryResolutionFailure, WebhookStore},
    },
    ports::BoxFuture,
};

/// Narrow worker-side persistence port. It makes the lease protocol testable without SQLite.
pub trait WorkerOutbox: Send + Sync {
    fn claim<'a>(
        &'a self,
        worker: String,
    ) -> BoxFuture<'a, Result<Option<DeliveryRecord>, AppError>>;
    fn accepted<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        message: String,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn retry<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        update: RetryTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn dead_letter<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        update: DeadLetterTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn persist_rate_limit<'a>(
        &'a self,
        scope: String,
        target: String,
        bucket: String,
        secret: String,
        blocked_until: i64,
    ) -> BoxFuture<'a, Result<(), AppError>>;
}
impl WorkerOutbox for OutboxRepository {
    fn claim<'a>(
        &'a self,
        worker: String,
    ) -> BoxFuture<'a, Result<Option<DeliveryRecord>, AppError>> {
        Box::pin(self.claim_next(worker))
    }
    fn accepted<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        message: String,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.accepted(
            record.id.clone(),
            worker,
            record.lease_token.clone().unwrap_or_default(),
            record.lease_version,
            message,
        ))
    }
    fn retry<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        update: RetryTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.retry(
            record.id.clone(),
            worker,
            record.lease_token.clone().unwrap_or_default(),
            record.lease_version,
            update,
        ))
    }
    fn dead_letter<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        update: DeadLetterTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.dead_letter(
            record.id.clone(),
            worker,
            record.lease_token.clone().unwrap_or_default(),
            record.lease_version,
            update,
        ))
    }
    fn persist_rate_limit<'a>(
        &'a self,
        scope: String,
        target: String,
        bucket: String,
        secret: String,
        blocked_until: i64,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(self.persist_rate_limit(scope, target, bucket, secret, blocked_until))
    }
}

/// Resolution failures keep an operational database lookup distinct from an unrecoverable
/// authenticated ciphertext failure. Neither variant carries secret material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookResolutionFailure {
    Retryable,
    InvalidReference,
    DecryptFailed,
}

/// Just-in-time decrypt/lookup port. It binds the reference to the queued tenant before exposing
/// a URL. Every failure is typed and data-free so a worker can classify it without logging a
/// bearer credential.
pub trait WorkerWebhooks: Send + Sync {
    fn delivery<'a>(
        &'a self,
        id: &'a WebhookId,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<WebhookDelivery, WebhookResolutionFailure>>;

    /// Advisory status only. It is called after the guarded outbox transition has committed, so
    /// bookkeeping failure can never re-open a sent row or introduce a duplicate delivery risk.
    fn record_result<'a>(
        &'a self,
        id: &'a WebhookId,
        outcome: Result<(), &'static str>,
    ) -> BoxFuture<'a, Result<(), AppError>>;
}
impl WorkerWebhooks for WebhookStore {
    fn delivery<'a>(
        &'a self,
        id: &'a WebhookId,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<WebhookDelivery, WebhookResolutionFailure>> {
        Box::pin(async move {
            self.resolve_delivery(id, org)
                .await
                .map_err(|failure| match failure {
                    WebhookDeliveryResolutionFailure::Retryable => {
                        WebhookResolutionFailure::Retryable
                    }
                    WebhookDeliveryResolutionFailure::InvalidReference => {
                        WebhookResolutionFailure::InvalidReference
                    }
                    WebhookDeliveryResolutionFailure::DecryptFailed => {
                        WebhookResolutionFailure::DecryptFailed
                    }
                })
        })
    }
    fn record_result<'a>(
        &'a self,
        id: &'a WebhookId,
        outcome: Result<(), &'static str>,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move { self.record_result(id, outcome.map_err(str::to_owned)).await })
    }
}

/// Discussion credentials are deliberately resolved through a separate, immutable connection
/// identity. These failures are typed so neither a URL nor an encryption failure leaks into
/// worker logs or terminal state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscussionResolutionFailure {
    Retryable,
    InvalidReference,
    DecryptFailed,
}

/// The narrow discussion persistence surface used after a row is leased. It intentionally
/// excludes producer mutations, making the worker incapable of creating mirror work itself.
pub trait WorkerDiscussions: Send + Sync {
    fn connection<'a>(
        &'a self,
        id: &'a str,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<DiscussionConnectionDelivery>, DiscussionResolutionFailure>>;
    fn discussion<'a>(
        &'a self,
        artifact: &'a ArtifactId,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<ArtifactDiscussion>, AppError>>;
    fn feedback_link<'a>(
        &'a self,
        artifact: &'a ArtifactId,
        org: &'a OrgId,
        feedback: &'a crate::model::FeedbackId,
    ) -> BoxFuture<'a, Result<Option<DiscussionMessageLink>, AppError>>;
    fn notification_anchor<'a>(
        &'a self,
        outbox_id: &'a str,
        artifact: &'a ArtifactId,
        org: &'a OrgId,
        connection_id: &'a str,
        generation: u64,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>>;
    fn accept_post<'a>(
        &'a self,
        input: AcceptedDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn accept_tombstone<'a>(
        &'a self,
        input: AcceptedDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn accept_marker<'a>(
        &'a self,
        input: AcceptedDiscussionMarker,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
    fn terminal<'a>(
        &'a self,
        input: TerminalDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>>;
}
impl WorkerDiscussions for DiscussionStore {
    fn connection<'a>(
        &'a self,
        id: &'a str,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<DiscussionConnectionDelivery>, DiscussionResolutionFailure>>
    {
        Box::pin(async move {
            self.connection_for_delivery(id, org)
                .await
                .map_err(|error| match error {
                    AppError::Unavailable(_) | AppError::Internal => {
                        DiscussionResolutionFailure::Retryable
                    }
                    AppError::Validation(_) => DiscussionResolutionFailure::DecryptFailed,
                    _ => DiscussionResolutionFailure::InvalidReference,
                })
        })
    }
    fn discussion<'a>(
        &'a self,
        artifact: &'a ArtifactId,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<ArtifactDiscussion>, AppError>> {
        Box::pin(self.get_discussion(artifact, org))
    }
    fn feedback_link<'a>(
        &'a self,
        artifact: &'a ArtifactId,
        org: &'a OrgId,
        feedback: &'a crate::model::FeedbackId,
    ) -> BoxFuture<'a, Result<Option<DiscussionMessageLink>, AppError>> {
        Box::pin(self.message_link_for_feedback(artifact, org, feedback))
    }
    fn notification_anchor<'a>(
        &'a self,
        outbox_id: &'a str,
        artifact: &'a ArtifactId,
        org: &'a OrgId,
        connection_id: &'a str,
        generation: u64,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        Box::pin(self.notification_anchor_message(
            outbox_id,
            artifact,
            org,
            connection_id,
            generation,
        ))
    }
    fn accept_post<'a>(
        &'a self,
        input: AcceptedDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.accept_delivery_and_record_message(input))
    }
    fn accept_tombstone<'a>(
        &'a self,
        input: AcceptedDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.accept_tombstone_delivery(input))
    }
    fn accept_marker<'a>(
        &'a self,
        input: AcceptedDiscussionMarker,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.accept_marker_delivery(input))
    }
    fn terminal<'a>(
        &'a self,
        input: TerminalDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(self.terminal_delivery(input))
    }
}

/// Injectable epoch source for deterministic retry/rate-limit tests.
pub trait WorkerClock: Send + Sync {
    fn now_millis(&self) -> i64;
}
pub struct SystemWorkerClock;
impl WorkerClock for SystemWorkerClock {
    fn now_millis(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}
