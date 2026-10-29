//! Durable Discord outbox worker. Startup wiring deliberately lives in a later slice.
//!
//! A turn has exactly two persistence phases: claim, then a guarded outcome update. Webhook
//! resolution and provider I/O occur between them, so an SQLite transaction is never held across
//! an `.await` to the network.
//!
//! The module is split by responsibility: `ports` holds the narrow persistence and clock
//! ports, `providers` and `preview` the provider-side adapters, `event` and `discussion` the
//! per-kind turn pipelines, `outcome` the guarded transitions, and `backoff` the retry policy.

mod backoff;
mod discussion;
mod event;
mod outcome;
mod ports;
mod preview;
mod providers;

#[cfg(test)]
mod tests;

use std::{sync::Arc, time::Duration};

use tokio::sync::watch;

use crate::{
    error::AppError,
    integrations::discord_delivery::DiscordProviderTransport,
    persistence::{
        outbox::{
            DELIVERY_KIND_DISCUSSION_MESSAGE, DELIVERY_KIND_DISCUSSION_THREAD,
            DELIVERY_KIND_DISCUSSION_TOMBSTONE, DeliveryRecord, OutboxRepository,
        },
        webhooks::WebhookStore,
    },
};

pub use backoff::RETRY_BACKOFF;
pub use ports::{
    DiscussionResolutionFailure, SystemWorkerClock, WebhookResolutionFailure, WorkerClock,
    WorkerDiscussions, WorkerOutbox, WorkerWebhooks,
};
use preview::DisabledPreviewResolver;
pub use preview::{ArtifactDeliveryPreviewResolver, WorkerPreviewResolver};
pub use providers::{
    OrganizationDiscordDiscussionProvider, WorkerDiscussionProvider, WorkerProvider,
};

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
}
