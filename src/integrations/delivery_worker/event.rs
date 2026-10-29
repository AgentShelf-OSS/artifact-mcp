//! The webhook event turn: validate, resolve the webhook, build the request, send, settle.

use std::time::Duration;

use super::{
    DeliveryWorker, WebhookResolutionFailure,
    backoff::backoff,
    outcome::{retry_code, terminal_code},
};
use crate::{
    error::AppError,
    integrations::{
        delivery_envelope::DeliveryEnvelopeV1,
        discord_delivery::{
            DeliveryClassification, DuplicateRisk, ProviderRequest, RetryReason, provider_request,
        },
    },
    model::{OrgId, WebhookDelivery, WebhookId},
    persistence::{
        outbox::{DeliveryRecord, verify_payload_hash},
        webhooks::event_from_name,
    },
};

/// Why an event turn stops before the provider is called. Each variant maps to exactly one
/// guarded transition in `DeliveryWorker::settle_event`.
enum EventExit {
    /// Guarded dead-letter without duplicate risk.
    DeadLetter(&'static str),
    /// Guarded retry, or dead-letter once the attempt budget is spent.
    Retry {
        class: &'static str,
        duplicate_risk: bool,
        delay: Duration,
    },
}

impl DeliveryWorker {
    /// One event turn: decode → resolve webhook → build request → send → settle. Every stage
    /// before the send is free of provider I/O, and every early exit performs exactly one guarded
    /// transition.
    pub(super) async fn process_event(&self, record: DeliveryRecord) -> Result<(), AppError> {
        let (envelope, webhook_id) = match decode_event(&record) {
            Ok(decoded) => decoded,
            Err(class) => return self.dead_letter(&record, class, false).await,
        };
        let webhook = match self.resolve_webhook(&record, webhook_id).await {
            Ok(webhook) => webhook,
            Err(exit) => return self.settle_event(&record, exit).await,
        };
        let request = match self.build_event_request(&record, &envelope, &webhook).await {
            Ok(request) => request,
            Err(class) => return self.dead_letter(&record, class, false).await,
        };
        let outcome = self.provider.deliver(request).await;
        self.persist_rate_limit(&record, &outcome).await?;
        self.apply_event_outcome(&record, &webhook, outcome).await
    }

    /// Stage 2: resolve the referenced webhook for the queued tenant, then require that the row,
    /// the reference and the resolved webhook all name the same webhook and tenant.
    async fn resolve_webhook(
        &self,
        record: &DeliveryRecord,
        webhook_id: &str,
    ) -> Result<WebhookDelivery, EventExit> {
        let webhook = match self
            .webhooks
            .delivery(
                &WebhookId(webhook_id.to_owned()),
                &OrgId(record.tenant.clone()),
            )
            .await
        {
            Ok(row) => row,
            Err(WebhookResolutionFailure::Retryable) => {
                return Err(EventExit::Retry {
                    class: "network",
                    duplicate_risk: true,
                    delay: backoff(record.attempts),
                });
            }
            Err(WebhookResolutionFailure::InvalidReference) => {
                return Err(EventExit::DeadLetter("invalid_webhook"));
            }
            Err(WebhookResolutionFailure::DecryptFailed) => {
                return Err(EventExit::DeadLetter("decrypt_failed"));
            }
        };
        if webhook.id.0 != webhook_id
            || webhook.org.0 != record.tenant
            || record.target_key != webhook_id
        {
            return Err(EventExit::DeadLetter("invalid_webhook"));
        }
        Ok(webhook)
    }

    /// Stage 3: resolve the optional preview and build the provider request. A preview failure
    /// falls back to the JSON body; a request the transport rejects is a dead-letter class.
    async fn build_event_request(
        &self,
        record: &DeliveryRecord,
        envelope: &DeliveryEnvelopeV1,
        webhook: &WebhookDelivery,
    ) -> Result<ProviderRequest, &'static str> {
        let tenant = OrgId(record.tenant.clone());
        let preview = match envelope.preview() {
            Some(reference) => self
                .previews
                .preview(&tenant, reference)
                .await
                .unwrap_or_default(),
            None => None,
        };
        let (content_type, body) = match envelope.discord_request(preview.as_deref()) {
            Ok(request) => request,
            Err(_) => return Err("validation_failed"),
        };
        match provider_request(
            &webhook.url,
            None,
            record.secret_ref.clone(),
            content_type,
            body,
        ) {
            Ok(request) => Ok(request),
            Err(reason) => Err(terminal_code(reason)),
        }
    }

    /// Perform the single transition an early exit asks for.
    async fn settle_event(&self, record: &DeliveryRecord, exit: EventExit) -> Result<(), AppError> {
        match exit {
            EventExit::DeadLetter(class) => self.dead_letter(record, class, false).await,
            EventExit::Retry {
                class,
                duplicate_risk,
                delay,
            } => {
                self.retry_or_exhaust(record, class, duplicate_risk, delay)
                    .await
            }
        }
    }

    /// Stage 5: persist the provider outcome. Only an accepted or terminal send records the
    /// advisory webhook status, and only after its guarded transition has won.
    async fn apply_event_outcome(
        &self,
        record: &DeliveryRecord,
        webhook: &WebhookDelivery,
        outcome: DeliveryClassification,
    ) -> Result<(), AppError> {
        match outcome {
            DeliveryClassification::Accepted { message_id, .. } => {
                self.accept(record, message_id).await?;
                self.record_webhook_result(&webhook.id, Ok(())).await;
                Ok(())
            }
            DeliveryClassification::Retry {
                reason,
                duplicate_risk,
                rate_limit,
            } => {
                let delay = if reason == RetryReason::RateLimited {
                    rate_limit
                        .and_then(|rate| rate.retry_after_ms)
                        .map(Duration::from_millis)
                        .ok_or_else(|| {
                            AppError::Validation("rate_limited outcome missing retry_after".into())
                        })?
                } else {
                    backoff(record.attempts)
                };
                self.retry_or_exhaust(
                    record,
                    retry_code(reason),
                    duplicate_risk == DuplicateRisk::Possible,
                    delay,
                )
                .await
            }
            DeliveryClassification::Terminal { reason } => {
                let classification = terminal_code(reason);
                self.dead_letter(record, classification, false).await?;
                self.record_webhook_result(&webhook.id, Err(classification))
                    .await;
                Ok(())
            }
        }
    }
}

/// Stage 1: verify the payload hash, strictly decode the canonical envelope, and recover the
/// webhook id from the secret reference. Failures are plain dead-letter classes.
fn decode_event(record: &DeliveryRecord) -> Result<(DeliveryEnvelopeV1, &str), &'static str> {
    if !verify_payload_hash(record) {
        return Err("payload_hash_mismatch");
    }
    let Some(event) = event_from_name(&record.event_type) else {
        return Err("validation_failed");
    };
    let envelope = match DeliveryEnvelopeV1::decode_canonical(
        &record.payload,
        &OrgId(record.tenant.clone()),
        &event,
        &record.event_id,
        Some(&record.payload_sha256),
    ) {
        Ok(envelope) => envelope,
        Err(_) => return Err("validation_failed"),
    };
    let Some(webhook_id) = record
        .secret_ref
        .strip_prefix("webhook:")
        .filter(|id| !id.is_empty())
    else {
        return Err("invalid_secret");
    };
    Ok((envelope, webhook_id))
}
