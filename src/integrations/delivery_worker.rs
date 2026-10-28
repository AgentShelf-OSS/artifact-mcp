//! Durable Discord outbox worker. Startup wiring deliberately lives in a later slice.
//!
//! A turn has exactly two persistence phases: claim, then a guarded outcome update. Webhook
//! resolution and provider I/O occur between them, so an SQLite transaction is never held across
//! an `.await` to the network.

use std::{sync::Arc, time::Duration};

use sha2::{Digest as _, Sha256};
use tokio::sync::watch;

use crate::{
    artifacts::lifecycle::ArtifactStore,
    error::AppError,
    integrations::discord_delivery::{
        DeliveryClassification, DiscordProviderTransport, DuplicateRisk, ProviderRequest,
        RateLimitMetadata, RateLimitScope, RetryReason, TerminalReason, provider_request,
    },
    integrations::{
        delivery_envelope::{DeliveryEnvelopeV1, DeliveryPreviewReferenceV1},
        discord_discussion::{
            DiscordDiscussionTransport, DiscussionOperation, DiscussionRequest, DiscussionResult,
            discussion_request,
        },
        discussion_envelope::{DiscordDiscussionEnvelopeV1, DiscordDiscussionOperationV1},
        thumbnails::{PreviewHtml, PreviewIntegration},
    },
    model::{ArtifactId, OrgId, WebhookDelivery, WebhookId},
    persistence::{
        discussions::{
            AcceptedDiscussionDelivery, AcceptedDiscussionMarker, ArtifactDiscussion,
            DiscussionConnectionDelivery, DiscussionConnectionStrategy, DiscussionMessageLink,
            DiscussionStore, TerminalDiscussionDelivery, discussion_ordering_key,
        },
        outbox::{
            DELIVERY_KIND_DISCUSSION_MESSAGE, DELIVERY_KIND_DISCUSSION_THREAD,
            DELIVERY_KIND_DISCUSSION_TOMBSTONE, DeadLetterTransition, DeliveryRecord, MAX_ATTEMPTS,
            OutboxRepository, RetryTransition, verify_payload_hash,
        },
        webhooks::{WebhookDeliveryResolutionFailure, WebhookStore, event_from_name},
    },
    ports::{
        ArtifactService, BoxFuture, discussions::OrganizationDiscordCredentialService,
        integrations::PreviewPriority,
    },
};

/// Bounded non-429 backoff, indexed by the already-incremented claim attempt.
pub const RETRY_BACKOFF: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
    Duration::from_secs(3600),
];

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

/// Provider port receives the redacted request envelope, not a URL or secret.
pub trait WorkerProvider: Send + Sync {
    fn deliver<'a>(&'a self, request: ProviderRequest) -> BoxFuture<'a, DeliveryClassification>;
}

/// Resolves an optional preview only after a durable row has been claimed.  Errors are isolated
/// from the business event: the worker always sends the canonical JSON body when no preview can
/// be obtained.
pub trait WorkerPreviewResolver: Send + Sync {
    fn preview<'a>(
        &'a self,
        tenant: &'a OrgId,
        reference: &'a DeliveryPreviewReferenceV1,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>>;
}

struct DisabledPreviewResolver;
impl WorkerPreviewResolver for DisabledPreviewResolver {
    fn preview<'a>(
        &'a self,
        _: &'a OrgId,
        _: &'a DeliveryPreviewReferenceV1,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async { Ok(None) })
    }
}

/// Production resolver over the authoritative store and bounded preview lane.
#[derive(Clone)]
pub struct ArtifactDeliveryPreviewResolver {
    artifacts: Arc<ArtifactStore>,
    previews: Arc<PreviewIntegration>,
}

impl ArtifactDeliveryPreviewResolver {
    #[must_use]
    pub const fn new(artifacts: Arc<ArtifactStore>, previews: Arc<PreviewIntegration>) -> Self {
        Self {
            artifacts,
            previews,
        }
    }
}

impl WorkerPreviewResolver for ArtifactDeliveryPreviewResolver {
    fn preview<'a>(
        &'a self,
        tenant: &'a OrgId,
        reference: &'a DeliveryPreviewReferenceV1,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move {
            let artifact_id = ArtifactId(reference.artifact_id().to_owned());
            let Some(meta) = self.artifacts.find_meta(&artifact_id).await? else {
                return Ok(None);
            };
            if !cache_eligible(&meta, tenant, reference) {
                return Ok(None);
            }
            if let Some(png) = self
                .previews
                .store()
                .read_delivery_thumbnail(&artifact_id, reference.body_sha256())
                .await
            {
                return Ok(Some(png));
            }

            if !render_eligible(&meta, tenant, reference) {
                return Ok(None);
            }

            // Rendering happens only in the bounded preview lane and only after the durable
            // delivery has been claimed. Re-hash the bytes so an update racing this lookup can
            // never attach a different artifact revision to the queued event.
            let Some(file) = self.artifacts.read_body_for(&meta).await? else {
                return Ok(None);
            };
            if hex::encode(Sha256::digest(&file.content)) != reference.body_sha256() {
                return Ok(None);
            }
            let Ok(html) = String::from_utf8(file.content) else {
                return Ok(None);
            };
            let Some(job) = self.previews.queue().try_enqueue(
                meta,
                PreviewHtml::Ready(html),
                PreviewPriority::High,
            ) else {
                return Ok(None);
            };
            // A preview is optional: waiting has the same upper bound as provider I/O, after
            // which the queued render may still warm the cache for a later delivery.
            Ok(tokio::time::timeout(Duration::from_secs(4), job)
                .await
                .ok()
                .flatten())
        })
    }
}

fn cache_eligible(
    meta: &crate::model::ArtifactMeta,
    tenant: &OrgId,
    reference: &DeliveryPreviewReferenceV1,
) -> bool {
    meta.org == *tenant && meta.id.0 == reference.artifact_id() && !meta.is_bundle
}

fn render_eligible(
    meta: &crate::model::ArtifactMeta,
    tenant: &OrgId,
    reference: &DeliveryPreviewReferenceV1,
) -> bool {
    cache_eligible(meta, tenant, reference)
        && meta.revision == reference.revision()
        && meta.body_sha256 == reference.body_sha256()
}
impl WorkerProvider for DiscordProviderTransport {
    fn deliver<'a>(&'a self, request: ProviderRequest) -> BoxFuture<'a, DeliveryClassification> {
        Box::pin(self.deliver(request))
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

/// Provider port for PBI-079's forum/media-thread operations.
pub trait WorkerDiscussionProvider: Send + Sync {
    fn deliver<'a>(
        &'a self,
        org: &'a OrgId,
        request: DiscussionRequest,
    ) -> BoxFuture<'a, DiscussionResult>;
}
impl WorkerDiscussionProvider for DiscordDiscussionTransport {
    fn deliver<'a>(
        &'a self,
        _org: &'a OrgId,
        request: DiscussionRequest,
    ) -> BoxFuture<'a, DiscussionResult> {
        Box::pin(self.deliver(request))
    }
}

/// Resolves the organization credential only after a tenant-bound delivery row is claimed.
/// Resolution errors occur before provider I/O and are therefore safely retryable without
/// duplicate risk. A missing token is still allowed through for legacy forum-webhook operations;
/// notification-thread operations reject it at the transport boundary.
pub struct OrganizationDiscordDiscussionProvider {
    credentials: Arc<dyn OrganizationDiscordCredentialService>,
}

impl OrganizationDiscordDiscussionProvider {
    #[must_use]
    pub fn new(credentials: Arc<dyn OrganizationDiscordCredentialService>) -> Self {
        Self { credentials }
    }
}

impl WorkerDiscussionProvider for OrganizationDiscordDiscussionProvider {
    fn deliver<'a>(
        &'a self,
        org: &'a OrgId,
        request: DiscussionRequest,
    ) -> BoxFuture<'a, DiscussionResult> {
        Box::pin(async move {
            let bot_token = match self.credentials.credential_for_provider(org).await {
                Ok(value) => value,
                Err(_) => {
                    return DiscussionResult::Retry {
                        reason: RetryReason::Network,
                        duplicate_risk: DuplicateRisk::None,
                        rate_limit: None,
                    };
                }
            };
            let transport = match DiscordDiscussionTransport::with_bot_token(bot_token) {
                Ok(value) => value,
                Err(_) => {
                    return DiscussionResult::Retry {
                        reason: RetryReason::Network,
                        duplicate_risk: DuplicateRisk::None,
                        rate_limit: None,
                    };
                }
            };
            transport.deliver(request).await
        })
    }
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

/// Observable result of a non-blocking single-turn worker invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerTurn {
    Idle,
    Processed,
    Shutdown,
}

/// One worker; lifecycle wiring later starts exactly two instances.
#[derive(Clone)]
pub struct DeliveryWorker {
    outbox: Arc<dyn WorkerOutbox>,
    webhooks: Arc<dyn WorkerWebhooks>,
    provider: Arc<dyn WorkerProvider>,
    previews: Arc<dyn WorkerPreviewResolver>,
    discussions: Option<Arc<dyn WorkerDiscussions>>,
    discussion_provider: Option<Arc<dyn WorkerDiscussionProvider>>,
    clock: Arc<dyn WorkerClock>,
    worker_id: String,
}
impl DeliveryWorker {
    #[must_use]
    pub fn new(
        outbox: Arc<OutboxRepository>,
        webhooks: Arc<WebhookStore>,
        provider: Arc<DiscordProviderTransport>,
        worker_id: String,
    ) -> Self {
        Self::with_adapters(
            outbox,
            webhooks,
            provider,
            Arc::new(SystemWorkerClock),
            worker_id,
        )
    }
    #[must_use]
    pub fn with_adapters(
        outbox: Arc<dyn WorkerOutbox>,
        webhooks: Arc<dyn WorkerWebhooks>,
        provider: Arc<dyn WorkerProvider>,
        clock: Arc<dyn WorkerClock>,
        worker_id: String,
    ) -> Self {
        Self {
            outbox,
            webhooks,
            provider,
            previews: Arc::new(DisabledPreviewResolver),
            discussions: None,
            discussion_provider: None,
            clock,
            worker_id,
        }
    }

    /// Adds the production post-claim preview resolver without widening the outbox/webhook ports.
    #[must_use]
    pub fn with_preview_resolver(mut self, previews: Arc<dyn WorkerPreviewResolver>) -> Self {
        self.previews = previews;
        self
    }

    /// Adds the generation-scoped PBI-079 worker adapters. Existing PBI-056 constructors keep
    /// discussion rows terminal rather than silently attempting an unconfigured destination.
    #[must_use]
    pub fn with_discussion_adapters(
        mut self,
        discussions: Arc<dyn WorkerDiscussions>,
        provider: Arc<dyn WorkerDiscussionProvider>,
    ) -> Self {
        self.discussions = Some(discussions);
        self.discussion_provider = Some(provider);
        self
    }

    /// Claim/process at most one row; storage and lease-guard failures are returned, never hidden.
    pub async fn run_once(
        &self,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<WorkerTurn, AppError> {
        if *shutdown.borrow() {
            return Ok(WorkerTurn::Shutdown);
        }
        let Some(record) = self.outbox.claim(self.worker_id.clone()).await? else {
            return Ok(WorkerTurn::Idle);
        };
        if *shutdown.borrow() {
            self.retry_or_exhaust(&record, "network", false, Duration::from_secs(1))
                .await?;
            return Ok(WorkerTurn::Shutdown);
        }
        self.process(record).await?;
        Ok(WorkerTurn::Processed)
    }

    async fn process(&self, record: DeliveryRecord) -> Result<(), AppError> {
        match record.delivery_kind.as_str() {
            "event" => self.process_event(record).await,
            DELIVERY_KIND_DISCUSSION_THREAD
            | DELIVERY_KIND_DISCUSSION_MESSAGE
            | DELIVERY_KIND_DISCUSSION_TOMBSTONE => self.process_discussion(record).await,
            _ => self.dead_letter(&record, "validation_failed", false).await,
        }
    }

    async fn process_event(&self, record: DeliveryRecord) -> Result<(), AppError> {
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
    /// One discussion turn: decode → authorize → resolve → build → send → settle. Every stage
    /// before the send is free of provider I/O, and every early exit performs exactly one guarded
    /// transition (or returns a storage error with none).
    async fn process_discussion(&self, record: DeliveryRecord) -> Result<(), AppError> {
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

    async fn accept(&self, record: &DeliveryRecord, message: String) -> Result<(), AppError> {
        guarded(
            self.outbox
                .accepted(record, self.worker_id.clone(), message)
                .await,
            "accepted",
        )
    }
    async fn record_webhook_result(&self, id: &WebhookId, outcome: Result<(), &'static str>) {
        // This happens strictly after `accepted`/`dead_letter` won its lease guard. It is
        // intentionally advisory: a separate status write is not atomic with the outbox state,
        // and retrying it through the delivery path could duplicate a provider send.
        if let Err(error) = self.webhooks.record_result(id, outcome).await {
            tracing::warn!(webhook = %id, error = %error, "durable webhook outcome could not be recorded");
        }
    }
    async fn retry_or_exhaust(
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
    async fn dead_letter(
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
    async fn persist_rate_limit(
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

fn guarded(result: Result<bool, AppError>, operation: &str) -> Result<(), AppError> {
    match result? {
        true => Ok(()),
        false => Err(AppError::Conflict(format!(
            "outbox guarded {operation} lost lease"
        ))),
    }
}
fn backoff(attempt: i64) -> Duration {
    RETRY_BACKOFF[usize::try_from(attempt.saturating_sub(1))
        .unwrap_or(usize::MAX)
        .min(RETRY_BACKOFF.len() - 1)]
}
fn retry_code(reason: RetryReason) -> &'static str {
    match reason {
        RetryReason::RateLimited => "rate_limited",
        RetryReason::Network => "network",
        RetryReason::Timeout => "timeout",
        RetryReason::Ambiguous => "ambiguous",
        RetryReason::ServerError => "server_error",
    }
}
fn terminal_code(reason: TerminalReason) -> &'static str {
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

/// Stage 1: verify the payload hash and strictly decode the canonical envelope. Failures here
/// cannot yet be bound to operation state, so they are plain outbox dead-letter classes.
fn decode_discussion(
    record: &DeliveryRecord,
) -> Result<(DiscordDiscussionEnvelopeV1, DiscussionBinding<'_>), &'static str> {
    if !verify_payload_hash(record) {
        return Err("payload_hash_mismatch");
    }
    let tenant = OrgId(record.tenant.clone());
    // The artifact/connection/generation values are first recovered from the canonical
    // envelope. Only then can failures update operation-specific state.
    let parsed: DiscordDiscussionEnvelopeV1 =
        serde_json::from_slice(&record.payload).map_err(|_| "validation_failed")?;
    let artifact = ArtifactId(parsed.artifact_id().to_owned());
    let connection = parsed.connection_id().to_owned();
    let generation = parsed.generation();
    let envelope = DiscordDiscussionEnvelopeV1::decode_canonical(
        &record.payload,
        &tenant,
        &record.event_id,
        parsed.artifact_id(),
        &connection,
        generation,
        Some(&record.payload_sha256),
    )
    .map_err(|_| "validation_failed")?;
    let feedback = crate::model::FeedbackId(envelope.operation().feedback_id().to_owned());
    Ok((
        envelope,
        DiscussionBinding {
            record,
            tenant,
            artifact,
            connection,
            generation,
            feedback,
        },
    ))
}

/// Stages 2-6: everything between a trusted envelope and an executable provider request. No
/// stage performs provider I/O.
async fn prepare_discussion(
    discussions: &dyn WorkerDiscussions,
    envelope: &DiscordDiscussionEnvelopeV1,
    kind: DiscussionKind,
    binding: &DiscussionBinding<'_>,
) -> Result<(DiscussionRequest, DiscussionTargets), DiscussionExit> {
    authorize_claim(envelope, binding)?;
    let discussion = authorize_generation(discussions, binding).await?;
    let link = verified_link(discussions, envelope, binding).await?;
    let targets = resolve_targets(kind, &discussion, &link)?;
    let delivery = resolve_connection(discussions, binding).await?;
    let operation =
        build_operation(discussions, envelope, kind, binding, &delivery, &targets).await?;
    let request = discussion_request(&delivery.url, binding.record.secret_ref.clone(), operation)
        .map_err(|reason| DiscussionExit::Terminal(terminal_code(reason)))?;
    Ok((request, targets))
}

/// Stage 2: the claimed row's kind, target, secret reference, and ordering key must all match
/// the envelope.
fn authorize_claim(
    envelope: &DiscordDiscussionEnvelopeV1,
    binding: &DiscussionBinding<'_>,
) -> Result<(), DiscussionExit> {
    let record = binding.record;
    if !discussion_kind_matches(&record.delivery_kind, envelope.operation())
        || record.target_key != binding.connection
        || record.secret_ref != format!("discussion:{}", binding.connection)
        || record.ordering_key != discussion_ordering_key(&binding.artifact, binding.generation)?
    {
        return Err(DiscussionExit::INVALID);
    }
    Ok(())
}

/// Stage 3: the artifact's current discussion must still be bound to this connection and
/// generation.
async fn authorize_generation(
    discussions: &dyn WorkerDiscussions,
    binding: &DiscussionBinding<'_>,
) -> Result<ArtifactDiscussion, DiscussionExit> {
    let Some(discussion) = discussions
        .discussion(&binding.artifact, &binding.tenant)
        .await?
    else {
        return Err(DiscussionExit::INVALID);
    };
    // Paused is intentionally accepted here: disabling only stops new producer work. Any
    // re-enable increments generation and is rejected by this exact authority check.
    if discussion.connection_id.as_deref() != Some(binding.connection.as_str())
        || discussion.generation != binding.generation
    {
        return Err(DiscussionExit::INVALID);
    }
    Ok(discussion)
}

/// Stage 3b: the feedback mapping must exist and correlate with this exact row.
async fn verified_link(
    discussions: &dyn WorkerDiscussions,
    envelope: &DiscordDiscussionEnvelopeV1,
    binding: &DiscussionBinding<'_>,
) -> Result<DiscussionMessageLink, DiscussionExit> {
    let link = discussions
        .feedback_link(&binding.artifact, &binding.tenant, &binding.feedback)
        .await?;
    match link {
        Some(link) if valid_discussion_link(binding.record, envelope, Some(&link)) => Ok(link),
        _ => Err(DiscussionExit::INVALID),
    }
}

/// Stage 4: resolve the external thread/message ids each operation kind needs.
fn resolve_targets(
    kind: DiscussionKind,
    discussion: &ArtifactDiscussion,
    link: &DiscussionMessageLink,
) -> Result<DiscussionTargets, DiscussionExit> {
    match kind {
        DiscussionKind::Thread => Ok(DiscussionTargets {
            thread_id: None,
            message_id: None,
        }),
        DiscussionKind::Reply | DiscussionKind::Marker => {
            let thread = discussion
                .thread_id
                .clone()
                .ok_or(DiscussionExit::INVALID)?;
            Ok(DiscussionTargets {
                thread_id: Some(thread),
                message_id: None,
            })
        }
        DiscussionKind::Tombstone => {
            let (Some(thread), Some(message)) = (
                link.external_thread_id.clone(),
                link.external_message_id.clone(),
            ) else {
                return Err(DiscussionExit::INVALID);
            };
            Ok(DiscussionTargets {
                thread_id: Some(thread),
                message_id: Some(message),
            })
        }
    }
}

/// Stage 5: resolve the tenant-bound connection credential. Only an operational lookup failure
/// is retryable; it happens before provider I/O, so it carries no duplicate risk.
async fn resolve_connection(
    discussions: &dyn WorkerDiscussions,
    binding: &DiscussionBinding<'_>,
) -> Result<DiscussionConnectionDelivery, DiscussionExit> {
    match discussions
        .connection(&binding.connection, &binding.tenant)
        .await
    {
        Ok(Some(value)) if value.org == binding.tenant => Ok(value),
        Ok(Some(_) | None) | Err(DiscussionResolutionFailure::InvalidReference) => {
            Err(DiscussionExit::INVALID)
        }
        Err(DiscussionResolutionFailure::Retryable) => Err(DiscussionExit::Retry {
            class: "network",
            duplicate_risk: false,
            delay: backoff(binding.record.attempts),
        }),
        Err(DiscussionResolutionFailure::DecryptFailed) => {
            Err(DiscussionExit::Terminal("decrypt_failed"))
        }
    }
}

/// Stage 6: build the transport operation. A notification-thread connection creates the first
/// thread from the already delivered anchor message instead of a forum post.
async fn build_operation(
    discussions: &dyn WorkerDiscussions,
    envelope: &DiscordDiscussionEnvelopeV1,
    kind: DiscussionKind,
    binding: &DiscussionBinding<'_>,
    delivery: &DiscussionConnectionDelivery,
    targets: &DiscussionTargets,
) -> Result<DiscussionOperation, DiscussionExit> {
    let base_operation = envelope
        .to_transport_operation(targets.thread_id.as_deref(), targets.message_id.as_deref())
        .map_err(|_| DiscussionExit::INVALID)?;
    if kind != DiscussionKind::Thread
        || delivery.strategy != DiscussionConnectionStrategy::NotificationThread
    {
        return Ok(base_operation);
    }
    let anchor_message_id = discussions
        .notification_anchor(
            binding
                .record
                .depends_on_outbox_id
                .as_deref()
                .unwrap_or_default(),
            &binding.artifact,
            &binding.tenant,
            &binding.connection,
            binding.generation,
        )
        .await?
        .ok_or(DiscussionExit::INVALID)?;
    let channel_id = delivery.channel_id.clone().ok_or(DiscussionExit::INVALID)?;
    let DiscussionOperation::CreateThread {
        thread_name,
        content,
    } = base_operation
    else {
        return Err(DiscussionExit::INVALID);
    };
    DiscussionOperation::create_thread_from_message(
        channel_id,
        anchor_message_id,
        thread_name,
        content,
    )
    .map_err(|reason| DiscussionExit::Terminal(terminal_code(reason)))
}

fn discussion_kind_matches(kind: &str, operation: &DiscordDiscussionOperationV1) -> bool {
    matches!(
        (kind, operation),
        (
            DELIVERY_KIND_DISCUSSION_THREAD,
            DiscordDiscussionOperationV1::Thread { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            DiscordDiscussionOperationV1::Reply { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            DiscordDiscussionOperationV1::Resolved { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            DiscordDiscussionOperationV1::Reopened { .. }
        ) | (
            DELIVERY_KIND_DISCUSSION_TOMBSTONE,
            DiscordDiscussionOperationV1::Tombstone { .. }
        )
    )
}

fn valid_discussion_link(
    record: &DeliveryRecord,
    envelope: &DiscordDiscussionEnvelopeV1,
    link: Option<&DiscussionMessageLink>,
) -> bool {
    let Some(link) = link else {
        return false;
    };
    if link.artifact_id.0 != envelope.artifact_id()
        || link.org.0 != envelope.tenant()
        || link.connection_id != envelope.connection_id()
        || link.generation != envelope.generation()
        || link.feedback_id.0 != envelope.operation().feedback_id()
    {
        return false;
    }
    match envelope.operation() {
        DiscordDiscussionOperationV1::Thread { .. }
        | DiscordDiscussionOperationV1::Reply { .. } => link.outbox_id == record.id,
        DiscordDiscussionOperationV1::Tombstone { .. } => {
            link.tombstone_outbox_id.as_deref() == Some(&record.id)
        }
        // A marker is correlated by its canonical receipt, but must refer to a retained original
        // feedback mapping in this exact generation.
        DiscordDiscussionOperationV1::Resolved { .. }
        | DiscordDiscussionOperationV1::Reopened { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use sha2::Digest;

    use super::*;
    use crate::{
        integrations::discord_discussion::DiscussionReceipt,
        model::{ArtifactId, FeedbackId, NotificationPayload, OrgId, Timestamp, WebhookEvent},
        persistence::discussions::{DiscussionMode, DiscussionState},
    };

    #[derive(Debug, PartialEq, Eq)]
    enum Action {
        Accepted(String),
        Retry(RetryTransition),
        Dead(String),
        Rate {
            scope: String,
            bucket: String,
            until: i64,
        },
        /// A discussion-store transition, journaled only when a test opts into the shared ledger.
        Discussion(&'static str),
    }
    struct FakeOutbox {
        record: Mutex<Option<DeliveryRecord>>,
        actions: Mutex<Vec<Action>>,
        dead_risks: Mutex<Vec<bool>>,
        guarded: bool,
        on_claim: Mutex<Option<watch::Sender<bool>>>,
    }
    impl FakeOutbox {
        fn one(record: DeliveryRecord) -> Self {
            Self {
                record: Mutex::new(Some(record)),
                actions: Mutex::new(Vec::new()),
                dead_risks: Mutex::new(Vec::new()),
                guarded: true,
                on_claim: Mutex::new(None),
            }
        }
        fn actions(&self) -> Vec<Action> {
            std::mem::take(&mut *self.actions.lock().expect("lock"))
        }
        fn dead_risks(&self) -> Vec<bool> {
            std::mem::take(&mut *self.dead_risks.lock().expect("lock"))
        }
    }
    impl WorkerOutbox for FakeOutbox {
        fn claim<'a>(
            &'a self,
            _: String,
        ) -> BoxFuture<'a, Result<Option<DeliveryRecord>, AppError>> {
            Box::pin(async move {
                let row = self.record.lock().expect("lock").take();
                if let Some(sender) = self.on_claim.lock().expect("lock").take() {
                    sender.send(true).expect("receiver live");
                }
                Ok(row)
            })
        }
        fn accepted<'a>(
            &'a self,
            _: &'a DeliveryRecord,
            _: String,
            message: String,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            Box::pin(async move {
                self.actions
                    .lock()
                    .expect("lock")
                    .push(Action::Accepted(message));
                Ok(self.guarded)
            })
        }
        fn retry<'a>(
            &'a self,
            _: &'a DeliveryRecord,
            _: String,
            update: RetryTransition,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            Box::pin(async move {
                self.actions
                    .lock()
                    .expect("lock")
                    .push(Action::Retry(update));
                Ok(self.guarded)
            })
        }
        fn dead_letter<'a>(
            &'a self,
            _: &'a DeliveryRecord,
            _: String,
            update: DeadLetterTransition,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            Box::pin(async move {
                self.actions
                    .lock()
                    .expect("lock")
                    .push(Action::Dead(update.classification));
                self.dead_risks
                    .lock()
                    .expect("lock")
                    .push(update.duplicate_risk);
                Ok(self.guarded)
            })
        }
        fn persist_rate_limit<'a>(
            &'a self,
            scope: String,
            _: String,
            bucket: String,
            _: String,
            until: i64,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            Box::pin(async move {
                self.actions.lock().expect("lock").push(Action::Rate {
                    scope,
                    bucket,
                    until,
                });
                Ok(())
            })
        }
    }
    struct FakeWebhooks {
        response: Mutex<Result<WebhookDelivery, WebhookResolutionFailure>>,
        results: Mutex<Vec<Result<(), String>>>,
    }
    impl WorkerWebhooks for FakeWebhooks {
        fn delivery<'a>(
            &'a self,
            _: &'a WebhookId,
            _: &'a OrgId,
        ) -> BoxFuture<'a, Result<WebhookDelivery, WebhookResolutionFailure>> {
            Box::pin(async move { self.response.lock().expect("lock").clone() })
        }
        fn record_result<'a>(
            &'a self,
            _: &'a WebhookId,
            outcome: Result<(), &'static str>,
        ) -> BoxFuture<'a, Result<(), AppError>> {
            Box::pin(async move {
                self.results
                    .lock()
                    .expect("lock")
                    .push(outcome.map_err(str::to_owned));
                Ok(())
            })
        }
    }
    struct FakeProvider {
        result: Mutex<DeliveryClassification>,
        calls: AtomicUsize,
    }
    struct FakePreview {
        outcome: Result<Option<Vec<u8>>, AppError>,
        calls: AtomicUsize,
    }
    impl WorkerPreviewResolver for FakePreview {
        fn preview<'a>(
            &'a self,
            _: &'a OrgId,
            _: &'a DeliveryPreviewReferenceV1,
        ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.outcome.clone()
            })
        }
    }
    impl WorkerProvider for FakeProvider {
        fn deliver<'a>(&'a self, _: ProviderRequest) -> BoxFuture<'a, DeliveryClassification> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.result.lock().expect("lock").clone()
            })
        }
    }
    struct FakeDiscussionProvider {
        result: Mutex<DiscussionResult>,
        calls: AtomicUsize,
        requests: Mutex<Vec<DiscussionRequest>>,
    }
    impl WorkerDiscussionProvider for FakeDiscussionProvider {
        fn deliver<'a>(
            &'a self,
            _: &'a OrgId,
            request: DiscussionRequest,
        ) -> BoxFuture<'a, DiscussionResult> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.requests.lock().expect("lock").push(request);
                self.result.lock().expect("lock").clone()
            })
        }
    }
    struct FakeDiscussions {
        connection:
            Mutex<Result<Option<DiscussionConnectionDelivery>, DiscussionResolutionFailure>>,
        discussion: Mutex<Option<ArtifactDiscussion>>,
        link: Mutex<Option<DiscussionMessageLink>>,
        posts: Mutex<Vec<AcceptedDiscussionDelivery>>,
        tombstones: Mutex<Vec<AcceptedDiscussionDelivery>>,
        markers: Mutex<Vec<AcceptedDiscussionMarker>>,
        terminals: Mutex<Vec<TerminalDiscussionDelivery>>,
        anchor: Mutex<Option<String>>,
        anchor_calls: Mutex<Vec<String>>,
        accept_guard: Mutex<bool>,
        ledger: Mutex<Option<Arc<FakeOutbox>>>,
    }
    impl FakeDiscussions {
        fn journal(&self, transition: &'static str) {
            if let Some(outbox) = self.ledger.lock().expect("lock").as_ref() {
                outbox
                    .actions
                    .lock()
                    .expect("lock")
                    .push(Action::Discussion(transition));
            }
        }
    }
    impl WorkerDiscussions for FakeDiscussions {
        fn connection<'a>(
            &'a self,
            _: &'a str,
            _: &'a OrgId,
        ) -> BoxFuture<'a, Result<Option<DiscussionConnectionDelivery>, DiscussionResolutionFailure>>
        {
            Box::pin(async move { self.connection.lock().expect("lock").clone() })
        }
        fn discussion<'a>(
            &'a self,
            _: &'a ArtifactId,
            _: &'a OrgId,
        ) -> BoxFuture<'a, Result<Option<ArtifactDiscussion>, AppError>> {
            Box::pin(async move { Ok(self.discussion.lock().expect("lock").clone()) })
        }
        fn feedback_link<'a>(
            &'a self,
            _: &'a ArtifactId,
            _: &'a OrgId,
            _: &'a FeedbackId,
        ) -> BoxFuture<'a, Result<Option<DiscussionMessageLink>, AppError>> {
            Box::pin(async move { Ok(self.link.lock().expect("lock").clone()) })
        }
        fn notification_anchor<'a>(
            &'a self,
            outbox_id: &'a str,
            _: &'a ArtifactId,
            _: &'a OrgId,
            _: &'a str,
            _: u64,
        ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
            Box::pin(async move {
                self.anchor_calls
                    .lock()
                    .expect("lock")
                    .push(outbox_id.to_owned());
                Ok(self.anchor.lock().expect("lock").clone())
            })
        }
        fn accept_post<'a>(
            &'a self,
            input: AcceptedDiscussionDelivery,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            Box::pin(async move {
                self.journal("accept_post");
                self.posts.lock().expect("lock").push(input);
                Ok(*self.accept_guard.lock().expect("lock"))
            })
        }
        fn accept_tombstone<'a>(
            &'a self,
            input: AcceptedDiscussionDelivery,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            Box::pin(async move {
                self.journal("accept_tombstone");
                self.tombstones.lock().expect("lock").push(input);
                Ok(*self.accept_guard.lock().expect("lock"))
            })
        }
        fn accept_marker<'a>(
            &'a self,
            input: AcceptedDiscussionMarker,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            Box::pin(async move {
                self.journal("accept_marker");
                self.markers.lock().expect("lock").push(input);
                Ok(*self.accept_guard.lock().expect("lock"))
            })
        }
        fn terminal<'a>(
            &'a self,
            input: TerminalDiscussionDelivery,
        ) -> BoxFuture<'a, Result<bool, AppError>> {
            Box::pin(async move {
                self.journal("terminal");
                self.terminals.lock().expect("lock").push(input);
                Ok(true)
            })
        }
    }
    struct FixedClock(i64);
    impl WorkerClock for FixedClock {
        fn now_millis(&self) -> i64 {
            self.0
        }
    }

    fn record() -> DeliveryRecord {
        let envelope = DeliveryEnvelopeV1::build(
            "event".into(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            &NotificationPayload {
                artifact_id: ArtifactId("artifact".into()),
                title: "title".into(),
                url: "https://example.test/a".into(),
                description: "description".into(),
                uploader_label: "uploader".into(),
                category: "category".into(),
                revision: 1,
                bytes: 1,
                viewer_email: None,
                body: None,
                resolver: None,
            },
        )
        .expect("valid envelope");
        let payload = envelope.canonical_bytes().expect("canonical envelope");
        DeliveryRecord {
            id: "row".into(),
            event_id: "event".into(),
            tenant: "acme".into(),
            event_type: "published".into(),
            target_key: "hook".into(),
            bucket_id: "hook".into(),
            secret_ref: "webhook:hook".into(),
            payload: payload.clone(),
            payload_sha256: envelope.payload_sha256().expect("envelope hash"),
            durability_intent_id: None,
            delivery_kind: "event".into(),
            ordering_key: "hook".into(),
            depends_on_outbox_id: None,
            state: "leased".into(),
            attempts: 1,
            next_attempt_at: 0,
            lease_owner: Some("test".into()),
            lease_expires_at: Some(1),
            lease_token: Some("lease".into()),
            lease_version: 1,
            result_classification: String::new(),
            duplicate_risk: false,
            discord_message_id: None,
            terminal_error: String::new(),
            created_at: 0,
            updated_at: 0,
            completed_at: None,
        }
    }

    fn record_with_preview() -> DeliveryRecord {
        let envelope = DeliveryEnvelopeV1::build_with_preview(
            "event".into(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            &NotificationPayload {
                artifact_id: ArtifactId("abc123".into()),
                title: "title".into(),
                url: "https://example.test/abc123".into(),
                description: "description".into(),
                uploader_label: "uploader".into(),
                category: "category".into(),
                revision: 1,
                bytes: 1,
                viewer_email: None,
                body: None,
                resolver: None,
            },
            Some(DeliveryPreviewReferenceV1::new("abc123", 1, &"a".repeat(64)).expect("reference")),
        )
        .expect("valid envelope");
        let payload = envelope.canonical_bytes().expect("canonical envelope");
        DeliveryRecord {
            payload,
            payload_sha256: envelope.payload_sha256().expect("envelope hash"),
            ..record()
        }
    }
    fn webhook() -> WebhookDelivery {
        WebhookDelivery {
            id: WebhookId("hook".into()),
            org: OrgId("acme".into()),
            url: "https://discord.com/api/webhooks/1/token".into(),
            label: String::new(),
            events: vec![WebhookEvent::Published],
        }
    }
    fn rate() -> RateLimitMetadata {
        RateLimitMetadata {
            webhook_ref: Some("webhook:hook".into()),
            bucket: Some("bucket".into()),
            remaining: None,
            reset_after_ms: None,
            retry_after_ms: None,
            scope: None,
        }
    }
    fn make_worker(
        outbox: Arc<FakeOutbox>,
        hooks: Result<Option<WebhookDelivery>, WebhookResolutionFailure>,
        result: DeliveryClassification,
    ) -> (DeliveryWorker, Arc<FakeProvider>) {
        let (worker, provider, _) = make_worker_with_hooks(outbox, hooks, result);
        (worker, provider)
    }
    fn make_worker_with_hooks(
        outbox: Arc<FakeOutbox>,
        hooks: Result<Option<WebhookDelivery>, WebhookResolutionFailure>,
        result: DeliveryClassification,
    ) -> (DeliveryWorker, Arc<FakeProvider>, Arc<FakeWebhooks>) {
        let provider = Arc::new(FakeProvider {
            result: Mutex::new(result),
            calls: AtomicUsize::new(0),
        });
        let hooks = Arc::new(FakeWebhooks {
            response: Mutex::new(
                hooks.and_then(|row| row.ok_or(WebhookResolutionFailure::InvalidReference)),
            ),
            results: Mutex::new(Vec::new()),
        });
        (
            DeliveryWorker::with_adapters(
                outbox,
                hooks.clone(),
                provider.clone(),
                Arc::new(FixedClock(10_000)),
                "test".into(),
            ),
            provider,
            hooks,
        )
    }

    fn make_worker_with_preview(
        outbox: Arc<FakeOutbox>,
        previews: Arc<FakePreview>,
        result: DeliveryClassification,
    ) -> (DeliveryWorker, Arc<FakeProvider>) {
        let provider = Arc::new(FakeProvider {
            result: Mutex::new(result),
            calls: AtomicUsize::new(0),
        });
        (
            DeliveryWorker::with_adapters(
                outbox,
                Arc::new(FakeWebhooks {
                    response: Mutex::new(Ok(webhook())),
                    results: Mutex::new(Vec::new()),
                }),
                provider.clone(),
                Arc::new(FixedClock(10_000)),
                "test".into(),
            )
            .with_preview_resolver(previews),
            provider,
        )
    }
    async fn turn(worker: &DeliveryWorker) -> Result<WorkerTurn, AppError> {
        let (_, mut shutdown) = watch::channel(false);
        worker.run_once(&mut shutdown).await
    }

    fn discussion_record(operation: DiscordDiscussionOperationV1, kind: &str) -> DeliveryRecord {
        let envelope = DiscordDiscussionEnvelopeV1::build(
            "discussion-event".into(),
            &OrgId("acme".into()),
            "artifact-a".into(),
            "connection-a".into(),
            1,
            operation,
        )
        .expect("valid discussion envelope");
        let payload = envelope
            .canonical_bytes()
            .expect("canonical discussion envelope");
        DeliveryRecord {
            id: "discussion-row".into(),
            event_id: "discussion-event".into(),
            tenant: "acme".into(),
            event_type: "discussion".into(),
            target_key: "connection-a".into(),
            bucket_id: "connection-a".into(),
            secret_ref: "discussion:connection-a".into(),
            payload,
            payload_sha256: envelope.payload_sha256().expect("discussion hash"),
            delivery_kind: kind.into(),
            ordering_key: discussion_ordering_key(&ArtifactId("artifact-a".into()), 1)
                .expect("ordering"),
            ..record()
        }
    }

    fn discussion_state(thread_id: Option<&str>) -> ArtifactDiscussion {
        ArtifactDiscussion {
            artifact_id: ArtifactId("artifact-a".into()),
            org: OrgId("acme".into()),
            mode: DiscussionMode::DiscordMirror,
            connection_org: Some(OrgId("acme".into())),
            connection_id: Some("connection-a".into()),
            thread_id: thread_id.map(str::to_owned),
            root_message_id: None,
            state: DiscussionState::Connected,
            generation: 1,
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

    fn discussion_link(record: &DeliveryRecord, tombstone: bool) -> DiscussionMessageLink {
        DiscussionMessageLink {
            artifact_id: ArtifactId("artifact-a".into()),
            org: OrgId("acme".into()),
            connection_id: "connection-a".into(),
            feedback_id: FeedbackId("feedback-a".into()),
            delivery_event_id: "discussion-event".into(),
            outbox_id: if tombstone {
                "prior-row".into()
            } else {
                record.id.clone()
            },
            tombstone_outbox_id: tombstone.then(|| record.id.clone()),
            external_thread_id: Some("123456789012345678".into()),
            external_message_id: Some("223456789012345678".into()),
            generation: 1,
            state: "posted".into(),
            last_error: None,
            local_deleted_at: None,
            created_at: Timestamp("1".into()),
            updated_at: Timestamp("1".into()),
            posted_at: Some(Timestamp("1".into())),
        }
    }

    fn fake_discussions(record: &DeliveryRecord, thread_id: Option<&str>) -> Arc<FakeDiscussions> {
        Arc::new(FakeDiscussions {
            connection: Mutex::new(Ok(Some(DiscussionConnectionDelivery {
                org: OrgId("acme".into()),
                label: "discussion".into(),
                url: "https://discord.com/api/webhooks/123/token".into(),
                strategy: DiscussionConnectionStrategy::ForumWebhook,
                notification_webhook_id: None,
                notification_provider_webhook_id: None,
                channel_id: None,
                guild_id: None,
            }))),
            discussion: Mutex::new(Some(discussion_state(thread_id))),
            link: Mutex::new(Some(discussion_link(record, false))),
            posts: Mutex::new(Vec::new()),
            tombstones: Mutex::new(Vec::new()),
            markers: Mutex::new(Vec::new()),
            terminals: Mutex::new(Vec::new()),
            anchor: Mutex::new(None),
            anchor_calls: Mutex::new(Vec::new()),
            accept_guard: Mutex::new(true),
            ledger: Mutex::new(None),
        })
    }

    fn make_discussion_worker(
        outbox: Arc<FakeOutbox>,
        discussions: Arc<FakeDiscussions>,
        result: DiscussionResult,
    ) -> (DeliveryWorker, Arc<FakeDiscussionProvider>) {
        let provider = Arc::new(FakeDiscussionProvider {
            result: Mutex::new(result),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let worker = DeliveryWorker::with_adapters(
            outbox,
            Arc::new(FakeWebhooks {
                response: Mutex::new(Ok(webhook())),
                results: Mutex::new(Vec::new()),
            }),
            Arc::new(FakeProvider {
                result: Mutex::new(DeliveryClassification::Terminal {
                    reason: TerminalReason::ClientError,
                }),
                calls: AtomicUsize::new(0),
            }),
            Arc::new(FixedClock(10_000)),
            "test".into(),
        )
        .with_discussion_adapters(discussions, provider.clone());
        (worker, provider)
    }

    #[tokio::test]
    async fn accepted_persists_message_id_and_bucket_discovery_without_blocking() {
        let outbox = Arc::new(FakeOutbox::one(record()));
        let metadata = RateLimitMetadata {
            remaining: Some(1),
            ..rate()
        };
        let (worker, _) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "123".into(),
                rate_limit: metadata,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "bucket".into(),
                    bucket: "bucket".into(),
                    until: 10_000
                },
                Action::Accepted("123".into())
            ]
        );
    }
    #[tokio::test]
    async fn rate_limit_uses_exact_delay_and_persists_block() {
        let outbox = Arc::new(FakeOutbox::one(record()));
        let metadata = RateLimitMetadata {
            retry_after_ms: Some(250),
            ..rate()
        };
        let (worker, _) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Retry {
                reason: RetryReason::RateLimited,
                duplicate_risk: DuplicateRisk::None,
                rate_limit: Some(metadata),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "bucket".into(),
                    bucket: "bucket".into(),
                    until: 10_250
                },
                Action::Retry(RetryTransition {
                    next_attempt_at: 10_250,
                    classification: "rate_limited".into(),
                    duplicate_risk: false
                })
            ]
        );
    }
    #[tokio::test]
    async fn global_rate_metadata_is_normalized_and_does_not_strand_the_lease() {
        let outbox = Arc::new(FakeOutbox::one(record()));
        let metadata = RateLimitMetadata {
            scope: Some(RateLimitScope::Global),
            remaining: Some(0),
            reset_after_ms: Some(500),
            ..rate()
        };
        let (worker, _) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "123".into(),
                rate_limit: metadata,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "global".into(),
                    bucket: String::new(),
                    until: 10_500
                },
                Action::Accepted("123".into())
            ]
        );
    }
    #[tokio::test]
    async fn global_429_uses_exact_retry_delay_with_normalized_state() {
        let outbox = Arc::new(FakeOutbox::one(record()));
        let metadata = RateLimitMetadata {
            scope: Some(RateLimitScope::Global),
            retry_after_ms: Some(250),
            ..rate()
        };
        let (worker, _) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Retry {
                reason: RetryReason::RateLimited,
                duplicate_risk: DuplicateRisk::None,
                rate_limit: Some(metadata),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "global".into(),
                    bucket: String::new(),
                    until: 10_250
                },
                Action::Retry(RetryTransition {
                    next_attempt_at: 10_250,
                    classification: "rate_limited".into(),
                    duplicate_risk: false
                })
            ]
        );
    }
    #[tokio::test]
    async fn accepted_and_terminal_outcomes_record_advisory_webhook_status_after_guarding() {
        let accepted_outbox = Arc::new(FakeOutbox::one(record()));
        let (accepted, _, accepted_hooks) = make_worker_with_hooks(
            accepted_outbox,
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "123".into(),
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&accepted).await, Ok(WorkerTurn::Processed));
        assert_eq!(*accepted_hooks.results.lock().expect("lock"), vec![Ok(())]);

        let terminal_outbox = Arc::new(FakeOutbox::one(record()));
        let (terminal, _, terminal_hooks) = make_worker_with_hooks(
            terminal_outbox,
            Ok(Some(webhook())),
            DeliveryClassification::Terminal {
                reason: TerminalReason::BadRequest,
            },
        );
        assert_eq!(turn(&terminal).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            *terminal_hooks.results.lock().expect("lock"),
            vec![Err("bad_request".into())]
        );
    }
    #[tokio::test]
    async fn lookup_failure_retries_but_missing_or_cross_tenant_is_terminal_without_send() {
        let outbox = Arc::new(FakeOutbox::one(record()));
        let (worker, provider) = make_worker(
            outbox.clone(),
            Err(WebhookResolutionFailure::Retryable),
            DeliveryClassification::Terminal {
                reason: TerminalReason::BadRequest,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Retry(RetryTransition {
                next_attempt_at: 11_000,
                classification: "network".into(),
                duplicate_risk: true
            })]
        );
        let outbox = Arc::new(FakeOutbox::one(record()));
        let mut wrong = webhook();
        wrong.org = OrgId("other".into());
        let (worker, provider) = make_worker(
            outbox.clone(),
            Ok(Some(wrong)),
            DeliveryClassification::Terminal {
                reason: TerminalReason::BadRequest,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("invalid_webhook".into())]
        );
        let outbox = Arc::new(FakeOutbox::one(record()));
        let (worker, provider) = make_worker(
            outbox.clone(),
            Err(WebhookResolutionFailure::DecryptFailed),
            DeliveryClassification::Terminal {
                reason: TerminalReason::BadRequest,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("decrypt_failed".into())]
        );
    }
    #[tokio::test]
    async fn hash_mismatch_and_attempt_exhaustion_dead_letter_without_send() {
        let mut bad = record();
        bad.payload_sha256 = "bad".into();
        let outbox = Arc::new(FakeOutbox::one(bad));
        let (worker, provider) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "1".into(),
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("payload_hash_mismatch".into())]
        );
        let mut exhausted = record();
        exhausted.attempts = MAX_ATTEMPTS;
        let outbox = Arc::new(FakeOutbox::one(exhausted));
        let (worker, _) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Retry {
                reason: RetryReason::Network,
                duplicate_risk: DuplicateRisk::Possible,
                rate_limit: None,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("attempts_exhausted".into())]
        );
        assert_eq!(outbox.dead_risks(), vec![true]);
    }
    #[tokio::test]
    async fn envelope_tamper_or_binding_mismatch_never_reaches_provider() {
        let mut tampered = record();
        let mut value: serde_json::Value =
            serde_json::from_slice(&tampered.payload).expect("envelope JSON");
        value["tenant"] = serde_json::Value::String("other".into());
        tampered.payload = serde_json::to_vec(&value).expect("tampered JSON");
        tampered.payload_sha256 = hex::encode(sha2::Sha256::digest(&tampered.payload));
        let outbox = Arc::new(FakeOutbox::one(tampered));
        let (worker, provider) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "1".into(),
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("validation_failed".into())]
        );
    }
    #[tokio::test]
    async fn stale_guard_is_surfaced() {
        let outbox = Arc::new(FakeOutbox {
            record: Mutex::new(Some(record())),
            actions: Mutex::new(Vec::new()),
            dead_risks: Mutex::new(Vec::new()),
            guarded: false,
            on_claim: Mutex::new(None),
        });
        let (worker, _) = make_worker(
            outbox,
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "1".into(),
                rate_limit: rate(),
            },
        );
        assert!(matches!(turn(&worker).await, Err(AppError::Conflict(_))));
    }

    #[tokio::test]
    async fn shutdown_before_send_releases_lease_as_a_safe_network_retry() {
        let outbox = Arc::new(FakeOutbox::one(record()));
        let (worker, provider) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "1".into(),
                rate_limit: rate(),
            },
        );
        let (shutdown_tx, mut shutdown) = watch::channel(false);
        *outbox.on_claim.lock().expect("lock") = Some(shutdown_tx);
        assert_eq!(
            worker.run_once(&mut shutdown).await,
            Ok(WorkerTurn::Shutdown)
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Retry(RetryTransition {
                next_attempt_at: 11_000,
                classification: "network".into(),
                duplicate_risk: false,
            })]
        );
    }

    #[tokio::test]
    async fn two_workers_cannot_claim_the_same_ready_row() {
        let outbox = Arc::new(FakeOutbox::one(record()));
        let (first, _) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "1".into(),
                rate_limit: rate(),
            },
        );
        let (second, _) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Accepted {
                message_id: "2".into(),
                rate_limit: rate(),
            },
        );
        let (_, mut first_shutdown) = watch::channel(false);
        let (_, mut second_shutdown) = watch::channel(false);
        let (first_turn, second_turn) = tokio::join!(
            first.run_once(&mut first_shutdown),
            second.run_once(&mut second_shutdown)
        );
        assert!(matches!(first_turn, Ok(WorkerTurn::Processed)));
        assert!(matches!(second_turn, Ok(WorkerTurn::Idle)));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "bucket".into(),
                    bucket: "bucket".into(),
                    until: 10_000,
                },
                Action::Accepted("1".into())
            ]
        );
    }

    #[tokio::test]
    async fn missing_or_failed_preview_falls_back_to_json_delivery() {
        for outcome in [
            Ok(None),
            Err(AppError::Unavailable("preview renderer unavailable".into())),
        ] {
            let outbox = Arc::new(FakeOutbox::one(record_with_preview()));
            let previews = Arc::new(FakePreview {
                outcome,
                calls: AtomicUsize::new(0),
            });
            let (worker, provider) = make_worker_with_preview(
                outbox.clone(),
                previews.clone(),
                DeliveryClassification::Accepted {
                    message_id: "1".into(),
                    rate_limit: rate(),
                },
            );
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
            assert_eq!(previews.calls.load(Ordering::SeqCst), 1);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert!(
                outbox
                    .actions()
                    .iter()
                    .any(|action| matches!(action, Action::Accepted(message) if message == "1"))
            );
        }
    }

    #[tokio::test]
    async fn discussion_thread_accepts_provider_thread_and_message_receipt() {
        let record = discussion_record(
            DiscordDiscussionOperationV1::thread(
                "feedback-a".into(),
                "Thread".into(),
                "Body".into(),
            )
            .expect("thread"),
            DELIVERY_KIND_DISCUSSION_THREAD,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, None);
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id: "223456789012345678".into(),
                    thread_id: Some("123456789012345678".into()),
                },
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            discussions.posts.lock().expect("lock")[0].external_thread_id,
            "123456789012345678"
        );
        assert_eq!(
            discussions.posts.lock().expect("lock")[0].external_message_id,
            "223456789012345678"
        );
    }

    #[tokio::test]
    async fn discussion_reply_uses_persisted_thread_and_accepts_post() {
        let record = discussion_record(
            DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some("123456789012345678"));
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id: "323456789012345678".into(),
                    thread_id: None,
                },
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            discussions.posts.lock().expect("lock")[0].external_thread_id,
            "123456789012345678"
        );
    }

    #[tokio::test]
    async fn discussion_markers_use_persisted_thread_and_marker_acceptor() {
        for operation in [
            DiscordDiscussionOperationV1::resolved("feedback-a".into()).expect("resolved"),
            DiscordDiscussionOperationV1::reopened("feedback-a".into()).expect("reopened"),
        ] {
            let record = discussion_record(operation, DELIVERY_KIND_DISCUSSION_MESSAGE);
            let outbox = Arc::new(FakeOutbox::one(record.clone()));
            let discussions = fake_discussions(&record, Some("123456789012345678"));
            let (worker, provider) = make_discussion_worker(
                outbox,
                discussions.clone(),
                DiscussionResult::Accepted {
                    receipt: DiscussionReceipt {
                        message_id: "423456789012345678".into(),
                        thread_id: None,
                    },
                    rate_limit: rate(),
                },
            );
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
            assert_eq!(discussions.markers.lock().expect("lock")[0].generation, 1);
            assert!(discussions.posts.lock().expect("lock").is_empty());
        }
    }

    #[tokio::test]
    async fn discussion_tombstone_uses_retained_link_and_tombstone_acceptor() {
        let record = discussion_record(
            DiscordDiscussionOperationV1::tombstone("feedback-a".into()).expect("tombstone"),
            DELIVERY_KIND_DISCUSSION_TOMBSTONE,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some("123456789012345678"));
        *discussions.link.lock().expect("lock") = Some(discussion_link(&record, true));
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id: "223456789012345678".into(),
                    thread_id: None,
                },
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            discussions.tombstones.lock().expect("lock")[0].external_thread_id,
            "123456789012345678"
        );
        assert_eq!(
            discussions.tombstones.lock().expect("lock")[0].external_message_id,
            "223456789012345678"
        );
    }

    #[tokio::test]
    async fn malformed_discussion_payload_dead_letters_without_provider_call() {
        let mut record = discussion_record(
            DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        );
        record.payload = b"{".to_vec();
        record.payload_sha256 = format!("{:x}", sha2::Sha256::digest(&record.payload));
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some("123456789012345678"));
        let (worker, provider) = make_discussion_worker(
            outbox.clone(),
            discussions,
            DiscussionResult::Terminal {
                reason: TerminalReason::ClientError,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("validation_failed".into())]
        );
    }

    #[tokio::test]
    async fn discussion_claim_binding_mismatches_never_reach_provider() {
        let base = discussion_record(
            DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        );
        let mut records = Vec::new();
        let mut wrong_tenant = base.clone();
        wrong_tenant.tenant = "other".into();
        records.push(wrong_tenant);
        let mut wrong_target = base.clone();
        wrong_target.target_key = "other-connection".into();
        records.push(wrong_target);
        let mut wrong_secret = base.clone();
        wrong_secret.secret_ref = "discussion:other-connection".into();
        records.push(wrong_secret);
        let mut wrong_ordering = base.clone();
        wrong_ordering.ordering_key = "discussion:artifact-b:1".into();
        records.push(wrong_ordering);
        let mut wrong_kind = base.clone();
        wrong_kind.delivery_kind = DELIVERY_KIND_DISCUSSION_THREAD.into();
        records.push(wrong_kind);

        for record in records {
            let outbox = Arc::new(FakeOutbox::one(record.clone()));
            let discussions = fake_discussions(&record, Some("123456789012345678"));
            let (worker, provider) = make_discussion_worker(
                outbox.clone(),
                discussions.clone(),
                DiscussionResult::Terminal {
                    reason: TerminalReason::ClientError,
                },
            );
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            assert!(
                !outbox.actions().is_empty()
                    || !discussions.terminals.lock().expect("lock").is_empty()
            );
        }

        for mismatch in ["generation", "link"] {
            let record = base.clone();
            let outbox = Arc::new(FakeOutbox::one(record.clone()));
            let discussions = fake_discussions(&record, Some("123456789012345678"));
            if mismatch == "generation" {
                discussions
                    .discussion
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("discussion")
                    .generation = 2;
            } else {
                discussions
                    .link
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("link")
                    .outbox_id = "other-row".into();
            }
            let (worker, provider) = make_discussion_worker(
                outbox,
                discussions.clone(),
                DiscussionResult::Terminal {
                    reason: TerminalReason::ClientError,
                },
            );
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            assert_eq!(discussions.terminals.lock().expect("lock").len(), 1);
        }
    }

    #[tokio::test]
    async fn discussion_credential_retry_is_retried_without_provider_call() {
        let record = discussion_record(
            DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some("123456789012345678"));
        *discussions.connection.lock().expect("lock") = Err(DiscussionResolutionFailure::Retryable);
        let (worker, provider) = make_discussion_worker(
            outbox.clone(),
            discussions,
            DiscussionResult::Terminal {
                reason: TerminalReason::ClientError,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(
            matches!(outbox.actions().as_slice(), [Action::Retry(update)] if update.classification == "network")
        );
    }

    // --- Characterization of `process_discussion` branch outcomes -----------------------------
    //
    // These lock the persistence call, classification, feedback correlation, and provider-call
    // count for every early exit and outcome so the pipeline can be restructured safely.

    const THREAD: &str = "123456789012345678";
    const ANCHOR: &str = "523456789012345678";
    const CHANNEL: &str = "623456789012345678";

    fn reply_record() -> DeliveryRecord {
        discussion_record(
            DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        )
    }

    fn thread_record() -> DeliveryRecord {
        discussion_record(
            DiscordDiscussionOperationV1::thread(
                "feedback-a".into(),
                "Thread".into(),
                "Body".into(),
            )
            .expect("thread"),
            DELIVERY_KIND_DISCUSSION_THREAD,
        )
    }

    fn accepted(message: &str, thread: Option<&str>) -> DiscussionResult {
        DiscussionResult::Accepted {
            receipt: DiscussionReceipt {
                message_id: message.into(),
                thread_id: thread.map(str::to_owned),
            },
            rate_limit: rate(),
        }
    }

    fn client_error() -> DiscussionResult {
        DiscussionResult::Terminal {
            reason: TerminalReason::ClientError,
        }
    }

    /// `(classification, feedback_id, duplicate_risk)` for each guarded terminal update.
    fn terminal_summary(discussions: &FakeDiscussions) -> Vec<(String, Option<String>, bool)> {
        discussions
            .terminals
            .lock()
            .expect("lock")
            .iter()
            .map(|row| {
                (
                    row.classification.clone(),
                    row.feedback_id.as_ref().map(|id| id.0.clone()),
                    row.duplicate_risk,
                )
            })
            .collect()
    }

    fn terminal(class: &str, feedback: Option<&str>, risk: bool) -> (String, Option<String>, bool) {
        (class.to_owned(), feedback.map(str::to_owned), risk)
    }

    fn notification_thread(discussions: &FakeDiscussions) {
        if let Ok(Some(connection)) = discussions.connection.lock().expect("lock").as_mut() {
            connection.strategy = DiscussionConnectionStrategy::NotificationThread;
            connection.channel_id = Some(CHANNEL.into());
        }
        *discussions.anchor.lock().expect("lock") = Some(ANCHOR.into());
    }

    #[tokio::test]
    async fn discussion_rows_without_discussion_adapters_dead_letter_as_validation_failed() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record));
        let (worker, provider) = make_worker(
            outbox.clone(),
            Ok(Some(webhook())),
            DeliveryClassification::Terminal {
                reason: TerminalReason::ClientError,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("validation_failed".into())]
        );
    }

    #[tokio::test]
    async fn discussion_hash_mismatch_dead_letters_before_discussion_state_is_read() {
        let mut record = reply_record();
        record.payload_sha256 = "0".repeat(64);
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, provider) =
            make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("payload_hash_mismatch".into())]
        );
        assert!(terminal_summary(&discussions).is_empty());
    }

    #[tokio::test]
    async fn non_canonical_or_cross_tenant_envelopes_dead_letter_without_discussion_state() {
        let mut non_canonical = reply_record();
        let value: serde_json::Value =
            serde_json::from_slice(&non_canonical.payload).expect("canonical JSON");
        non_canonical.payload = serde_json::to_vec_pretty(&value).expect("pretty JSON");
        non_canonical.payload_sha256 = hex::encode(Sha256::digest(&non_canonical.payload));
        let mut cross_tenant = reply_record();
        cross_tenant.tenant = "other".into();

        for record in [non_canonical, cross_tenant] {
            let outbox = Arc::new(FakeOutbox::one(record.clone()));
            let discussions = fake_discussions(&record, Some(THREAD));
            let (worker, provider) =
                make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                outbox.actions(),
                vec![Action::Dead("validation_failed".into())]
            );
            assert!(terminal_summary(&discussions).is_empty());
        }
    }

    #[tokio::test]
    async fn discussion_binding_and_authority_failures_are_terminal_before_provider_io() {
        type RecordEdit = fn(&mut DeliveryRecord);
        type StateEdit = fn(&FakeDiscussions);
        let keep_record: RecordEdit = |_| {};
        let keep_state: StateEdit = |_| {};
        let cases: &[(&str, RecordEdit, StateEdit, &str)] = &[
            (
                "target",
                |record| record.target_key = "connection-b".into(),
                keep_state,
                "validation_failed",
            ),
            (
                "secret",
                |record| record.secret_ref = "discussion:connection-b".into(),
                keep_state,
                "validation_failed",
            ),
            (
                "ordering",
                |record| record.ordering_key = "discussion:artifact-b:1".into(),
                keep_state,
                "validation_failed",
            ),
            (
                "kind",
                |record| record.delivery_kind = DELIVERY_KIND_DISCUSSION_TOMBSTONE.into(),
                keep_state,
                "validation_failed",
            ),
            (
                "discussion missing",
                keep_record,
                |state| *state.discussion.lock().expect("lock") = None,
                "validation_failed",
            ),
            (
                "foreign connection",
                keep_record,
                |state| {
                    state
                        .discussion
                        .lock()
                        .expect("lock")
                        .as_mut()
                        .expect("discussion")
                        .connection_id = Some("connection-b".into());
                },
                "validation_failed",
            ),
            (
                "disconnected",
                keep_record,
                |state| {
                    state
                        .discussion
                        .lock()
                        .expect("lock")
                        .as_mut()
                        .expect("discussion")
                        .connection_id = None;
                },
                "validation_failed",
            ),
            (
                "stale generation",
                keep_record,
                |state| {
                    state
                        .discussion
                        .lock()
                        .expect("lock")
                        .as_mut()
                        .expect("discussion")
                        .generation = 2;
                },
                "validation_failed",
            ),
            (
                "link missing",
                keep_record,
                |state| *state.link.lock().expect("lock") = None,
                "validation_failed",
            ),
            (
                "link generation",
                keep_record,
                |state| {
                    state
                        .link
                        .lock()
                        .expect("lock")
                        .as_mut()
                        .expect("link")
                        .generation = 2;
                },
                "validation_failed",
            ),
            (
                "reply without thread",
                keep_record,
                |state| {
                    state
                        .discussion
                        .lock()
                        .expect("lock")
                        .as_mut()
                        .expect("discussion")
                        .thread_id = None;
                },
                "validation_failed",
            ),
            (
                "connection missing",
                keep_record,
                |state| *state.connection.lock().expect("lock") = Ok(None),
                "validation_failed",
            ),
            (
                "connection cross-tenant",
                keep_record,
                |state| {
                    if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                        connection.org = OrgId("other".into());
                    }
                },
                "validation_failed",
            ),
            (
                "connection invalid reference",
                keep_record,
                |state| {
                    *state.connection.lock().expect("lock") =
                        Err(DiscussionResolutionFailure::InvalidReference);
                },
                "validation_failed",
            ),
            (
                "connection decrypt",
                keep_record,
                |state| {
                    *state.connection.lock().expect("lock") =
                        Err(DiscussionResolutionFailure::DecryptFailed);
                },
                "decrypt_failed",
            ),
            (
                "persisted thread is not a snowflake",
                keep_record,
                |state| {
                    state
                        .discussion
                        .lock()
                        .expect("lock")
                        .as_mut()
                        .expect("discussion")
                        .thread_id = Some("not-a-snowflake".into());
                },
                "validation_failed",
            ),
            (
                "connection url outside allowlist",
                keep_record,
                |state| {
                    if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                        connection.url = "https://example.test/api/webhooks/123/token".into();
                    }
                },
                "allowlist_rejected",
            ),
        ];

        for &(name, edit_record, edit_state, class) in cases {
            let mut record = reply_record();
            edit_record(&mut record);
            let outbox = Arc::new(FakeOutbox::one(record.clone()));
            let discussions = fake_discussions(&record, Some(THREAD));
            edit_state(&discussions);
            let (worker, provider) =
                make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed), "{name}");
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{name}");
            assert!(outbox.actions().is_empty(), "{name}");
            assert_eq!(
                terminal_summary(&discussions),
                vec![terminal(class, Some("feedback-a"), false)],
                "{name}"
            );
            let terminals = discussions.terminals.lock().expect("lock");
            let row = &terminals[0];
            assert_eq!(
                (
                    row.outbox_id.as_str(),
                    row.worker.as_str(),
                    row.lease_token.as_str(),
                    row.lease_version,
                    row.artifact_id.0.as_str(),
                    row.org.0.as_str(),
                    row.connection_id.as_str(),
                    row.generation,
                    row.delivery_kind.as_str(),
                    row.now_millis,
                ),
                (
                    "discussion-row",
                    "test",
                    "lease",
                    1,
                    "artifact-a",
                    "acme",
                    "connection-a",
                    1,
                    record.delivery_kind.as_str(),
                    10_000,
                ),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn paused_discussion_in_the_current_generation_still_delivers() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        discussions
            .discussion
            .lock()
            .expect("lock")
            .as_mut()
            .expect("discussion")
            .state = DiscussionState::Paused;
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            accepted("323456789012345678", None),
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(discussions.posts.lock().expect("lock").len(), 1);
        assert!(terminal_summary(&discussions).is_empty());
    }

    #[tokio::test]
    async fn tombstone_without_retained_external_ids_is_terminal_before_provider_io() {
        for missing in ["link", "thread", "message"] {
            let record = discussion_record(
                DiscordDiscussionOperationV1::tombstone("feedback-a".into()).expect("tombstone"),
                DELIVERY_KIND_DISCUSSION_TOMBSTONE,
            );
            let outbox = Arc::new(FakeOutbox::one(record.clone()));
            let discussions = fake_discussions(&record, Some(THREAD));
            let mut link = discussion_link(&record, true);
            match missing {
                "thread" => link.external_thread_id = None,
                "message" => link.external_message_id = None,
                _ => {}
            }
            *discussions.link.lock().expect("lock") = (missing != "link").then_some(link);
            let (worker, provider) =
                make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed), "{missing}");
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{missing}");
            assert!(outbox.actions().is_empty(), "{missing}");
            assert_eq!(
                terminal_summary(&discussions),
                vec![terminal("validation_failed", Some("feedback-a"), false)],
                "{missing}"
            );
        }
    }

    #[tokio::test]
    async fn marker_failures_clear_the_feedback_correlation() {
        // Early validation exit: a marker needs a persisted thread.
        let record = discussion_record(
            DiscordDiscussionOperationV1::resolved("feedback-a".into()).expect("resolved"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, None);
        let (worker, provider) =
            make_discussion_worker(outbox, discussions.clone(), client_error());
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            terminal_summary(&discussions),
            vec![terminal("validation_failed", None, false)]
        );

        // Provider terminal outcome for a reopened marker.
        let record = discussion_record(
            DiscordDiscussionOperationV1::reopened("feedback-a".into()).expect("reopened"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            DiscussionResult::Terminal {
                reason: TerminalReason::NotFound,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            terminal_summary(&discussions),
            vec![terminal("not_found", None, false)]
        );
        assert!(discussions.markers.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn notification_thread_creates_the_thread_from_the_anchor_message() {
        let mut record = thread_record();
        record.depends_on_outbox_id = Some("anchor-row".into());
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, None);
        notification_thread(&discussions);
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            accepted("223456789012345678", Some(THREAD)),
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            *discussions.anchor_calls.lock().expect("lock"),
            vec!["anchor-row".to_owned()]
        );
        let expected = discussion_request(
            "https://discord.com/api/webhooks/123/token",
            "discussion:connection-a".into(),
            DiscussionOperation::create_thread_from_message(
                CHANNEL.into(),
                ANCHOR.into(),
                "Thread".into(),
                "Body".into(),
            )
            .expect("anchored thread"),
        )
        .expect("anchored request");
        assert_eq!(*provider.requests.lock().expect("lock"), vec![expected]);
        assert_eq!(
            discussions.posts.lock().expect("lock")[0].external_thread_id,
            THREAD
        );
    }

    #[tokio::test]
    async fn notification_thread_without_anchor_or_channel_is_terminal_before_provider_io() {
        type StateEdit = fn(&FakeDiscussions);
        let cases: &[(&str, StateEdit, &str)] = &[
            (
                "anchor missing",
                |state| *state.anchor.lock().expect("lock") = None,
                "validation_failed",
            ),
            (
                "channel missing",
                |state| {
                    if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                        connection.channel_id = None;
                    }
                },
                "validation_failed",
            ),
            (
                "channel invalid",
                |state| {
                    if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                        connection.channel_id = Some("channel".into());
                    }
                },
                "bad_request",
            ),
        ];
        for &(name, edit, class) in cases {
            let record = thread_record();
            let outbox = Arc::new(FakeOutbox::one(record.clone()));
            let discussions = fake_discussions(&record, None);
            notification_thread(&discussions);
            edit(&discussions);
            let (worker, provider) =
                make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
            assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed), "{name}");
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{name}");
            assert!(outbox.actions().is_empty(), "{name}");
            assert_eq!(
                terminal_summary(&discussions),
                vec![terminal(class, Some("feedback-a"), false)],
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn notification_thread_replies_use_the_persisted_thread_without_an_anchor() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        notification_thread(&discussions);
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            accepted("323456789012345678", None),
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert!(discussions.anchor_calls.lock().expect("lock").is_empty());
        let expected = discussion_request(
            "https://discord.com/api/webhooks/123/token",
            "discussion:connection-a".into(),
            DiscussionOperation::reply(THREAD.into(), "Body".into()).expect("reply"),
        )
        .expect("reply request");
        assert_eq!(*provider.requests.lock().expect("lock"), vec![expected]);
    }

    #[tokio::test]
    async fn thread_acceptance_without_a_thread_receipt_retries_as_ambiguous() {
        let record = thread_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, None);
        let (worker, _) = make_discussion_worker(
            outbox.clone(),
            discussions.clone(),
            accepted("223456789012345678", None),
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "bucket".into(),
                    bucket: "bucket".into(),
                    until: 10_000,
                },
                Action::Retry(RetryTransition {
                    next_attempt_at: 11_000,
                    classification: "ambiguous".into(),
                    duplicate_risk: true,
                }),
            ]
        );
        assert!(discussions.posts.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn discussion_rate_limit_is_persisted_before_the_exact_retry() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, _) = make_discussion_worker(
            outbox.clone(),
            discussions,
            DiscussionResult::Retry {
                reason: RetryReason::RateLimited,
                duplicate_risk: DuplicateRisk::None,
                rate_limit: Some(RateLimitMetadata {
                    retry_after_ms: Some(2_500),
                    ..rate()
                }),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "bucket".into(),
                    bucket: "bucket".into(),
                    until: 12_500,
                },
                Action::Retry(RetryTransition {
                    next_attempt_at: 12_500,
                    classification: "rate_limited".into(),
                    duplicate_risk: false,
                }),
            ]
        );
    }

    #[tokio::test]
    async fn discussion_rate_limit_without_retry_after_is_an_error_without_transition() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, _) = make_discussion_worker(
            outbox.clone(),
            discussions.clone(),
            DiscussionResult::Retry {
                reason: RetryReason::RateLimited,
                duplicate_risk: DuplicateRisk::None,
                rate_limit: None,
            },
        );
        assert_eq!(
            turn(&worker).await,
            Err(AppError::Validation(
                "rate_limited outcome missing retry_after".into()
            ))
        );
        assert!(outbox.actions().is_empty());
        assert!(terminal_summary(&discussions).is_empty());
    }

    #[tokio::test]
    async fn discussion_retry_backs_off_by_attempt_and_carries_duplicate_risk() {
        let mut record = reply_record();
        record.attempts = 3;
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, _) = make_discussion_worker(
            outbox.clone(),
            discussions,
            DiscussionResult::Retry {
                reason: RetryReason::Timeout,
                duplicate_risk: DuplicateRisk::Possible,
                rate_limit: None,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![Action::Retry(RetryTransition {
                next_attempt_at: 40_000,
                classification: "timeout".into(),
                duplicate_risk: true,
            })]
        );
    }

    #[tokio::test]
    async fn exhausted_discussion_retry_is_terminal_with_its_duplicate_risk() {
        let mut record = reply_record();
        record.attempts = MAX_ATTEMPTS;
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, _) = make_discussion_worker(
            outbox.clone(),
            discussions.clone(),
            DiscussionResult::Retry {
                reason: RetryReason::ServerError,
                duplicate_risk: DuplicateRisk::Possible,
                rate_limit: None,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert!(outbox.actions().is_empty());
        assert_eq!(
            terminal_summary(&discussions),
            vec![terminal("attempts_exhausted", Some("feedback-a"), true)]
        );
    }

    #[tokio::test]
    async fn discussion_provider_terminal_maps_its_reason_code() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, provider) = make_discussion_worker(
            outbox.clone(),
            discussions.clone(),
            DiscussionResult::Terminal {
                reason: TerminalReason::Forbidden,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(outbox.actions().is_empty());
        assert_eq!(
            terminal_summary(&discussions),
            vec![terminal("forbidden", Some("feedback-a"), false)]
        );
    }

    #[tokio::test]
    async fn discussion_acceptance_follows_rate_persistence_and_binds_the_lease() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        *discussions.ledger.lock().expect("lock") = Some(outbox.clone());
        let (worker, _) = make_discussion_worker(
            outbox.clone(),
            discussions.clone(),
            accepted("323456789012345678", None),
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            outbox.actions(),
            vec![
                Action::Rate {
                    scope: "bucket".into(),
                    bucket: "bucket".into(),
                    until: 10_000,
                },
                Action::Discussion("accept_post"),
            ]
        );
        assert_eq!(
            *discussions.posts.lock().expect("lock"),
            vec![AcceptedDiscussionDelivery {
                outbox_id: "discussion-row".into(),
                worker: "test".into(),
                lease_token: "lease".into(),
                lease_version: 1,
                external_thread_id: THREAD.into(),
                external_message_id: "323456789012345678".into(),
                now_millis: 10_000,
            }]
        );
        *discussions.ledger.lock().expect("lock") = None;

        // Marker acceptance carries the full generation-scoped identity instead of a link.
        let record = discussion_record(
            DiscordDiscussionOperationV1::resolved("feedback-a".into()).expect("resolved"),
            DELIVERY_KIND_DISCUSSION_MESSAGE,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, _) = make_discussion_worker(
            outbox,
            discussions.clone(),
            accepted("423456789012345678", None),
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            *discussions.markers.lock().expect("lock"),
            vec![AcceptedDiscussionMarker {
                outbox_id: "discussion-row".into(),
                worker: "test".into(),
                lease_token: "lease".into(),
                lease_version: 1,
                artifact_id: ArtifactId("artifact-a".into()),
                org: OrgId("acme".into()),
                connection_id: "connection-a".into(),
                generation: 1,
                message_id: "423456789012345678".into(),
                now_millis: 10_000,
            }]
        );
    }

    #[tokio::test]
    async fn lost_discussion_acceptance_guard_is_surfaced() {
        let record = reply_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        *discussions.accept_guard.lock().expect("lock") = false;
        let (worker, _) =
            make_discussion_worker(outbox, discussions, accepted("323456789012345678", None));
        assert_eq!(
            turn(&worker).await,
            Err(AppError::Conflict(
                "outbox guarded discussion accepted lost lease".into()
            ))
        );
    }
}
