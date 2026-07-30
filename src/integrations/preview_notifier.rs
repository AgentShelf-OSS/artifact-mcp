//! Production notification adapter that adds an optional queued preview to artifact mutations.
//!
//! The queue admission happens before the deferred body loader can run. This keeps a saturated
//! renderer lane from creating body-reading work or copying artifact HTML merely to discover that
//! previews are temporarily unavailable.

use std::sync::Arc;

use tracing::{Instrument as _, info_span};

use crate::{
    artifacts::lifecycle::ArtifactStore,
    error::AppError,
    integrations::{
        notify::DiscordNotifier,
        thumbnails::{PreviewHtml, PreviewIntegration},
    },
    model::{
        ArtifactId, ArtifactMeta, DeliveryResult, NotificationPayload, OrgId, WebhookDelivery,
        WebhookEvent,
    },
    ports::{ArtifactService, BoxFuture, NotificationSink, integrations::PreviewPriority},
};

/// Source for the metadata needed for preview admission and the body loader used after admission.
///
/// Implementations must make [`Self::deferred_body`] lazy: the returned loader is evaluated only
/// by the queue worker after it has reserved the job's declared bytes.
pub trait PreviewArtifactSource: Send + Sync {
    /// Find an artifact suitable for an optional HTML preview.
    fn find_meta<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Option<ArtifactMeta>>;

    /// Build the deferred body reader for a previously admitted artifact.
    fn deferred_body(&self, meta: &ArtifactMeta) -> PreviewHtml;
}

/// The production [`PreviewArtifactSource`] backed by the authoritative artifact store.
#[derive(Clone)]
pub struct ArtifactStorePreviewSource {
    artifacts: Arc<ArtifactStore>,
}

impl ArtifactStorePreviewSource {
    #[must_use]
    pub const fn new(artifacts: Arc<ArtifactStore>) -> Self {
        Self { artifacts }
    }
}

impl PreviewArtifactSource for ArtifactStorePreviewSource {
    fn find_meta<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Option<ArtifactMeta>> {
        Box::pin(async move {
            self.artifacts
                .find_meta(id)
                .await
                .ok()
                .flatten()
                .filter(|meta| !meta.is_bundle)
        })
    }

    fn deferred_body(&self, meta: &ArtifactMeta) -> PreviewHtml {
        let id = meta.id.clone();
        let expected_digest = meta.body_sha256.clone();
        let artifacts = Arc::clone(&self.artifacts);
        PreviewHtml::Deferred(Box::new(move || {
            Box::pin(async move {
                let current = artifacts.find_meta(&id).await.ok().flatten()?;
                if current.is_bundle || current.body_sha256 != expected_digest {
                    return None;
                }
                let file = artifacts.read_body_for(&current).await.ok().flatten()?;
                String::from_utf8(file.content).ok()
            })
        }))
    }
}

/// [`NotificationSink`] used by production artifact mutations.
///
/// Preview generation is deliberately optional. Rejection, a missing artifact, a stale body, or
/// a renderer failure all deliver the webhook without an image and never fail the mutation.
#[derive(Clone)]
pub struct ArtifactPreviewNotifier {
    artifacts: Arc<dyn PreviewArtifactSource>,
    previews: Arc<PreviewIntegration>,
    discord: Arc<DiscordNotifier>,
}

impl ArtifactPreviewNotifier {
    #[must_use]
    pub fn new(
        artifacts: Arc<dyn PreviewArtifactSource>,
        previews: Arc<PreviewIntegration>,
        discord: Arc<DiscordNotifier>,
    ) -> Self {
        Self {
            artifacts,
            previews,
            discord,
        }
    }

    /// Construct the production adapter over the authoritative artifact store.
    #[must_use]
    pub fn from_artifact_store(
        artifacts: Arc<ArtifactStore>,
        previews: Arc<PreviewIntegration>,
        discord: Arc<DiscordNotifier>,
    ) -> Self {
        Self::new(
            Arc::new(ArtifactStorePreviewSource::new(artifacts)),
            previews,
            discord,
        )
    }

    async fn emit_direct(self, event: WebhookEvent, org: OrgId, payload: NotificationPayload) {
        if event == WebhookEvent::Deleted {
            self.previews
                .store()
                .remove_artifact(&payload.artifact_id)
                .await;
        }
        self.discord
            .emit_with_preview(event, org, payload, None)
            .await;
    }
}

impl NotificationSink for ArtifactPreviewNotifier {
    fn emit(
        &self,
        event: WebhookEvent,
        org: OrgId,
        payload: NotificationPayload,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        let notifier = self.clone();
        Box::pin(async move {
            if !matches!(
                event,
                WebhookEvent::Published | WebhookEvent::Updated | WebhookEvent::Restored
            ) {
                notifier.emit_direct(event, org, payload).await;
                return Ok(());
            }

            let Some(meta) = notifier.artifacts.find_meta(&payload.artifact_id).await else {
                notifier.emit_direct(event, org, payload).await;
                return Ok(());
            };
            let deferred = notifier.artifacts.deferred_body(&meta);
            let Some(job) =
                notifier
                    .previews
                    .queue()
                    .try_enqueue(meta, deferred, PreviewPriority::High)
            else {
                // Preview generation is optional. The webhook is still delivered without a
                // preview, and the queue's low-cardinality rejection counter records pressure.
                notifier.emit_direct(event, org, payload).await;
                return Ok(());
            };
            let discord = Arc::clone(&notifier.discord);
            tokio::spawn(
                async move {
                    let preview = job.await.map(Arc::new);
                    discord
                        .emit_with_preview(event, org, payload, preview)
                        .await;
                }
                .instrument(info_span!("notification.preview")),
            );
            Ok(())
        })
    }

    fn test<'a>(
        &'a self,
        webhook: &'a WebhookDelivery,
    ) -> BoxFuture<'a, Result<DeliveryResult, AppError>> {
        self.discord.test(webhook)
    }
}
