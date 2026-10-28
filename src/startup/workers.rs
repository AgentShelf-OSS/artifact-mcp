//! Worker stage: integration stores, the detached notifier, and the background runtimes.

use std::sync::Arc;

use crate::{
    config::{AppConfig, IdSource},
    error::AppError,
    integrations::{
        delivery_runtime::{DeliveryRuntime, DeliveryTelemetry, DeliveryWakeSignal},
        delivery_worker::{
            ArtifactDeliveryPreviewResolver, OrganizationDiscordDiscussionProvider,
            WorkerDiscussionProvider, WorkerDiscussions,
        },
        discord_delivery::DiscordProviderTransport,
        discord_gateway_runtime::DiscordInboundRuntime,
        discord_history_recovery::DiscordHistoryRest,
        discord_recovery_runtime::DiscordRecoveryRuntime,
        notify::{DiscordNotifier, HttpTransport},
        preview_notifier::ArtifactPreviewNotifier,
        push_runtime::{HttpPushTransport, PushRuntime, PushService},
    },
    persistence::{
        db::DbPool, discord_inbound::DiscordInboundStore,
        discord_organization::OrganizationDiscordStore, discussions::DiscussionStore,
        outbox::OutboxRepository, webhooks::WebhookStore,
    },
    ports::NotificationSink,
    security::crypto::WebhookUrlProtection,
};

use super::storage::ContentStores;

/// Webhook and Discord discussion stores shared by the workers and the port adapters.
pub struct IntegrationStores {
    pub webhooks: Arc<WebhookStore>,
    pub discussions: Arc<DiscussionStore>,
    pub organization_discord: OrganizationDiscordStore,
    pub inbound: DiscordInboundStore,
}

impl IntegrationStores {
    pub fn open(
        config: &AppConfig,
        pool: &DbPool,
        ids: &Arc<dyn IdSource>,
        protection: &Arc<WebhookUrlProtection>,
    ) -> Self {
        let webhooks = Arc::new(WebhookStore::new(
            pool.clone(),
            Arc::clone(ids),
            Arc::clone(protection),
        ));
        let discussions = Arc::new(DiscussionStore::new(pool.clone(), Arc::clone(protection)));
        let organization_discord = OrganizationDiscordStore::new(
            pool.clone(),
            Arc::clone(protection),
            config.discord_bot_token.clone(),
        );
        let inbound = DiscordInboundStore::new(pool.clone());
        Self {
            webhooks,
            discussions,
            organization_discord,
            inbound,
        }
    }
}

/// Build the detached publish notifier (Discord webhooks plus preview images).
///
/// # Errors
///
/// Returns an error when the HTTP transport cannot be built.
pub fn notification_sink(
    stores: &IntegrationStores,
    content: &ContentStores,
) -> Result<Arc<dyn NotificationSink>, AppError> {
    let discord = Arc::new(DiscordNotifier::new(
        Arc::clone(&stores.webhooks),
        Arc::new(HttpTransport::new()?),
    ));
    Ok(Arc::new(ArtifactPreviewNotifier::from_artifact_store(
        Arc::clone(&content.artifacts),
        Arc::clone(&content.previews),
        discord,
    )))
}

/// Background workers owned by a serving process, stopped together after the listener closes.
pub struct BackgroundRuntimes {
    pub delivery: DeliveryRuntime,
    pub recovery: DiscordRecoveryRuntime,
    pub inbound: DiscordInboundRuntime,
    pub push: Option<PushRuntime>,
}

impl BackgroundRuntimes {
    pub async fn shutdown(self) {
        self.delivery.shutdown().await;
        self.recovery.shutdown().await;
        if let Some(push) = self.push {
            push.shutdown().await;
        }
        self.inbound.shutdown().await;
    }
}

/// Started background runtimes and the handles the router needs from them.
pub struct StartedWorkers {
    pub runtimes: BackgroundRuntimes,
    pub delivery_telemetry: DeliveryTelemetry,
    pub delivery_wake: DeliveryWakeSignal,
    /// ADR-0012 reminders and Web Push. `None` when the feature is disabled.
    pub push: Option<Arc<PushService>>,
}

/// Start durable delivery, Discord history recovery, the Discord inbound Gateway, and (when
/// enabled) the Web Push reminder runtime.
///
/// Call this only after storage reconciliation, digest recovery, and preview cache
/// reconciliation have finished.
///
/// # Errors
///
/// Returns an error when a provider transport or the push service cannot be built.
pub fn start_background_workers(
    config: &AppConfig,
    pool: &DbPool,
    stores: &IntegrationStores,
    content: &ContentStores,
    audit_key: [u8; 32],
) -> Result<StartedWorkers, AppError> {
    let delivery_telemetry = DeliveryTelemetry::default();
    let worker_discussions: Arc<dyn WorkerDiscussions> = stores.discussions.clone();
    let worker_discussion_provider: Arc<dyn WorkerDiscussionProvider> = Arc::new(
        OrganizationDiscordDiscussionProvider::new(Arc::new(stores.organization_discord.clone())),
    );
    let (delivery_runtime, delivery_wake) = DeliveryRuntime::start(
        Arc::new(OutboxRepository::new(pool.clone())),
        Arc::clone(&stores.webhooks),
        Arc::new(DiscordProviderTransport::new()?),
        worker_discussions,
        worker_discussion_provider,
        Arc::new(ArtifactDeliveryPreviewResolver::new(
            Arc::clone(&content.artifacts),
            Arc::clone(&content.previews),
        )),
        delivery_telemetry.clone(),
    );
    let recovery_runtime = DiscordRecoveryRuntime::start(
        stores.organization_discord.clone(),
        Arc::new(DiscordHistoryRest::new()?),
        audit_key,
    );
    let inbound_runtime = DiscordInboundRuntime::start(
        config.discord_inbound_enabled,
        stores.inbound.clone(),
        Arc::new(stores.organization_discord.clone()),
    );
    // ADR-0012: the reminder sweeper and Web Push sender run only when the feature is enabled.
    let push_service = PushService::from_config(config, pool.clone())?;
    let push_runtime = match &push_service {
        Some(service) => {
            tracing::info!("web push reminders enabled");
            Some(PushRuntime::start(
                Arc::clone(service),
                Arc::new(HttpPushTransport::new()?),
            ))
        }
        None => {
            if config.web_push.vapid.is_some() {
                tracing::warn!(
                    "WEB_PUSH_VAPID_PRIVATE_KEY is set but WEBHOOK_ENC_KEY is not; web push reminders are disabled"
                );
            }
            None
        }
    };
    Ok(StartedWorkers {
        runtimes: BackgroundRuntimes {
            delivery: delivery_runtime,
            recovery: recovery_runtime,
            inbound: inbound_runtime,
            push: push_runtime,
        },
        delivery_telemetry,
        delivery_wake,
        push: push_service,
    })
}
