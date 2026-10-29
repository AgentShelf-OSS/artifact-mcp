//! Guarded outbox transitions, rate-limit persistence, and outcome classification codes.

use std::time::Duration;

use super::DeliveryWorker;
use crate::{
    error::AppError,
    integrations::discord_delivery::{
        DeliveryClassification, RateLimitMetadata, RateLimitScope, RetryReason, TerminalReason,
    },
    model::WebhookId,
    persistence::outbox::{DeadLetterTransition, DeliveryRecord, MAX_ATTEMPTS, RetryTransition},
};

impl DeliveryWorker {
    pub(super) async fn accept(
        &self,
        record: &DeliveryRecord,
        message: String,
    ) -> Result<(), AppError> {
        guarded(
            self.outbox
                .accepted(record, self.worker_id.clone(), message)
                .await,
            "accepted",
        )
    }
    pub(super) async fn record_webhook_result(
        &self,
        id: &WebhookId,
        outcome: Result<(), &'static str>,
    ) {
        // This happens strictly after `accepted`/`dead_letter` won its lease guard. It is
        // intentionally advisory: a separate status write is not atomic with the outbox state,
        // and retrying it through the delivery path could duplicate a provider send.
        if let Err(error) = self.webhooks.record_result(id, outcome).await {
            tracing::warn!(webhook = %id, error = %error, "durable webhook outcome could not be recorded");
        }
    }
    pub(super) async fn retry_or_exhaust(
        &self,
        record: &DeliveryRecord,
        class: &str,
        duplicate_risk: bool,
        delay: Duration,
    ) -> Result<(), AppError> {
        if record.attempts >= MAX_ATTEMPTS {
            return self
                .dead_letter(record, "attempts_exhausted", duplicate_risk)
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
    pub(super) async fn dead_letter(
        &self,
        record: &DeliveryRecord,
        class: &str,
        duplicate_risk: bool,
    ) -> Result<(), AppError> {
        guarded(
            self.outbox
                .dead_letter(
                    record,
                    self.worker_id.clone(),
                    DeadLetterTransition {
                        classification: class.to_owned(),
                        error: class.to_owned(),
                        duplicate_risk,
                    },
                )
                .await,
            "dead-letter",
        )
    }
    pub(super) async fn persist_rate_limit(
        &self,
        record: &DeliveryRecord,
        outcome: &DeliveryClassification,
    ) -> Result<(), AppError> {
        let Some(rate) = rate_metadata(outcome) else {
            return Ok(());
        };
        let delay = match outcome {
            DeliveryClassification::Retry {
                reason: RetryReason::RateLimited,
                ..
            } => rate.retry_after_ms,
            DeliveryClassification::Accepted { .. } if rate.remaining == Some(0) => {
                rate.reset_after_ms
            }
            DeliveryClassification::Accepted { .. } => Some(0),
            _ => None,
        };
        let Some(delay) = delay else {
            return Ok(());
        };
        let (scope, target, bucket, secret) = match rate.scope {
            // Global state must be normalized so it can block every target.
            Some(RateLimitScope::Global) => ("global", String::new(), String::new(), String::new()),
            // Discord's user scope is local to this target but has no shareable bucket identity.
            Some(RateLimitScope::User) => (
                "target",
                record.target_key.clone(),
                String::new(),
                String::new(),
            ),
            // Bucket state uses target only to discover/update its current bucket; storage
            // normalizes the persisted target to empty so the bucket is shared correctly.
            Some(RateLimitScope::Shared) | None => (
                "bucket",
                record.target_key.clone(),
                rate.bucket
                    .clone()
                    .unwrap_or_else(|| record.bucket_id.clone()),
                record.secret_ref.clone(),
            ),
        };
        self.outbox
            .persist_rate_limit(
                scope.to_owned(),
                target,
                bucket,
                secret,
                self.clock
                    .now_millis()
                    .saturating_add(i64::try_from(delay).unwrap_or(i64::MAX)),
            )
            .await
    }
}

pub(super) fn guarded(result: Result<bool, AppError>, operation: &str) -> Result<(), AppError> {
    match result? {
        true => Ok(()),
        false => Err(AppError::Conflict(format!(
            "outbox guarded {operation} lost lease"
        ))),
    }
}
pub(super) fn retry_code(reason: RetryReason) -> &'static str {
    match reason {
        RetryReason::RateLimited => "rate_limited",
        RetryReason::Network => "network",
        RetryReason::Timeout => "timeout",
        RetryReason::Ambiguous => "ambiguous",
        RetryReason::ServerError => "server_error",
    }
}
pub(super) fn terminal_code(reason: TerminalReason) -> &'static str {
    match reason {
        TerminalReason::InvalidSecret => "invalid_secret",
        TerminalReason::DecryptFailed => "decrypt_failed",
        TerminalReason::AllowlistRejected => "allowlist_rejected",
        TerminalReason::Redirect => "redirect",
        TerminalReason::BadRequest => "bad_request",
        TerminalReason::Unauthorized => "unauthorized",
        TerminalReason::Forbidden => "forbidden",
        TerminalReason::NotFound => "not_found",
        TerminalReason::InvalidRateLimitDelay => "invalid_rate_limit_delay",
        TerminalReason::ClientError => "client_error",
        TerminalReason::ServerError => "server_error",
    }
}
fn rate_metadata(outcome: &DeliveryClassification) -> Option<&RateLimitMetadata> {
    match outcome {
        DeliveryClassification::Accepted { rate_limit, .. } => Some(rate_limit),
        DeliveryClassification::Retry { rate_limit, .. } => rate_limit.as_ref(),
        DeliveryClassification::Terminal { .. } => None,
    }
}
