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
        discord_delivery::{DeliveryClassification, DuplicateRisk, RetryReason, provider_request},
    },
    model::{OrgId, WebhookId},
    persistence::{
        outbox::{DeliveryRecord, verify_payload_hash},
        webhooks::event_from_name,
    },
};

impl DeliveryWorker {
    pub(super) async fn process_event(&self, record: DeliveryRecord) -> Result<(), AppError> {
        if !verify_payload_hash(&record) {
            return self
                .dead_letter(&record, "payload_hash_mismatch", false)
                .await;
        }
        let Some(event) = event_from_name(&record.event_type) else {
            return self.dead_letter(&record, "validation_failed", false).await;
        };
        let envelope = match DeliveryEnvelopeV1::decode_canonical(
            &record.payload,
            &OrgId(record.tenant.clone()),
            &event,
            &record.event_id,
            Some(&record.payload_sha256),
        ) {
            Ok(envelope) => envelope,
            Err(_) => return self.dead_letter(&record, "validation_failed", false).await,
        };
        let Some(webhook_id) = record
            .secret_ref
            .strip_prefix("webhook:")
            .filter(|id| !id.is_empty())
        else {
            return self.dead_letter(&record, "invalid_secret", false).await;
        };
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
                return self
                    .retry_or_exhaust(&record, "network", true, backoff(record.attempts))
                    .await;
            }
            Err(WebhookResolutionFailure::InvalidReference) => {
                return self.dead_letter(&record, "invalid_webhook", false).await;
            }
            Err(WebhookResolutionFailure::DecryptFailed) => {
                return self.dead_letter(&record, "decrypt_failed", false).await;
            }
        };
        if webhook.id.0 != webhook_id
            || webhook.org.0 != record.tenant
            || record.target_key != webhook_id
        {
            return self.dead_letter(&record, "invalid_webhook", false).await;
        }
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
            Err(_) => return self.dead_letter(&record, "validation_failed", false).await,
        };
        let request = match provider_request(
            &webhook.url,
            None,
            record.secret_ref.clone(),
            content_type,
            body,
        ) {
            Ok(request) => request,
            Err(reason) => {
                return self
                    .dead_letter(&record, terminal_code(reason), false)
                    .await;
            }
        };
        let outcome = self.provider.deliver(request).await;
        self.persist_rate_limit(&record, &outcome).await?;
        match outcome {
            DeliveryClassification::Accepted { message_id, .. } => {
                self.accept(&record, message_id).await?;
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
                    &record,
                    retry_code(reason),
                    duplicate_risk == DuplicateRisk::Possible,
                    delay,
                )
                .await
            }
            DeliveryClassification::Terminal { reason } => {
                let classification = terminal_code(reason);
                self.dead_letter(&record, classification, false).await?;
                self.record_webhook_result(&webhook.id, Err(classification))
                    .await;
                Ok(())
            }
        }
    }
}
