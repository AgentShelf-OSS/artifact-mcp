//! Optional post-claim preview resolution. A preview never decides the delivery outcome.

use std::{sync::Arc, time::Duration};

use sha2::{Digest as _, Sha256};

use crate::{
    artifacts::lifecycle::ArtifactStore,
    error::AppError,
    integrations::{
        delivery_envelope::DeliveryPreviewReferenceV1,
        thumbnails::{PreviewHtml, PreviewIntegration},
    },
    model::{ArtifactId, OrgId},
    ports::{ArtifactService, BoxFuture, integrations::PreviewPriority},
};

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

pub(super) struct DisabledPreviewResolver;
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
