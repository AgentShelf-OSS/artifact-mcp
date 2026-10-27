//! Storage stage: open the artifact store and preview cache, reconcile storage, and warm the
//! preview cache.

use std::sync::Arc;

use tracing::{Instrument as _, info_span};

use crate::{
    adapters::StartupPreviewIndex,
    artifacts::lifecycle::ArtifactStore,
    config::{AppConfig, IdSource},
    error::AppError,
    http::ingress::IngressState,
    integrations::thumbnails::{PersistentThumbnailScheduler, PreviewHtml, PreviewIntegration},
    persistence::db::DbPool,
    ports::{ArtifactService, PreviewService, integrations::PreviewPriority},
};

/// Artifact storage, preview cache, and listener ingress state shared by later stages.
pub struct ContentStores {
    pub ingress: Arc<IngressState>,
    pub previews: Arc<PreviewIntegration>,
    pub artifacts: Arc<ArtifactStore>,
}

/// Open the ingress state, the preview integration, and the artifact store.
///
/// # Errors
///
/// Returns an error when the artifact store configuration is invalid.
pub fn open_content(
    config: &AppConfig,
    pool: &DbPool,
    ids: &Arc<dyn IdSource>,
) -> Result<ContentStores, AppError> {
    let ingress = Arc::new(IngressState::from_config(config));
    let previews = Arc::new(PreviewIntegration::from_config_with_queue_counter(
        config,
        Some(ingress.preview_queue_rejection_counter()),
        Some(ingress.preview_queue_pressure()),
    ));
    let artifacts = Arc::new(
        ArtifactStore::from_config(pool.clone(), config, Arc::clone(ids))?
            .with_post_commit_preview_scheduler(Arc::new(PersistentThumbnailScheduler::new(
                Arc::clone(&previews),
            ))),
    );
    Ok(ContentStores {
        ingress,
        previews,
        artifacts,
    })
}

/// Reconcile artifact storage, call `reconciled` as soon as it completes, then backfill body
/// digests.
///
/// # Errors
///
/// Returns an error when reconciliation or the digest backfill fails; both record a
/// `reconciliation_failure` security signal first.
pub async fn reconcile_storage(
    artifacts: &ArtifactStore,
    reconciled: impl FnOnce(),
) -> Result<(), AppError> {
    let storage = artifacts.audit_storage(true).await.inspect_err(|_error| {
        crate::observability::record_global_security_signal("reconciliation_failure");
    })?;
    reconciled();
    tracing::info!(
        recovered = storage.recovered_paths.len(),
        transient = storage.transient_paths.len(),
        missing = storage.missing_bodies.len(),
        divergent = storage.divergent_bodies.len(),
        orphan_bodies = storage.orphan_bodies.len(),
        orphan_history = storage.orphan_history.len(),
        "storage reconciliation complete"
    );

    let digests = artifacts
        .backfill_body_digests()
        .await
        .inspect_err(|_error| {
            crate::observability::record_global_security_signal("reconciliation_failure");
        })?;
    tracing::info!(
        scanned = digests.scanned,
        updated = digests.updated,
        "artifact digest backfill complete"
    );
    Ok(())
}

/// Audit the preview cache against the current artifacts and queue missing thumbnails.
///
/// # Errors
///
/// Returns an error when the artifact listing fails.
pub async fn warm_previews(
    artifacts: &Arc<ArtifactStore>,
    previews: &Arc<PreviewIntegration>,
) -> Result<(), AppError> {
    let grouped = artifacts.list_all_grouped_by_org(true).await?;
    let all_artifacts = grouped
        .into_iter()
        .flat_map(|group| group.items)
        .collect::<Vec<_>>();
    let preview_index = StartupPreviewIndex::from_artifacts(&all_artifacts);
    let preview_audit = previews.store().audit(&preview_index).await;
    let queued =
        schedule_thumbnail_backfill(all_artifacts, Arc::clone(artifacts), Arc::clone(previews));
    tracing::info!(
        enabled = previews.enabled(),
        orphan_paths_removed = preview_audit.orphan_dirs.len(),
        stale_files_removed = preview_audit.partial_files.len(),
        invalid_pngs_removed = preview_audit.invalid_files.len(),
        backfill_candidates = queued,
        "preview startup status"
    );
    Ok(())
}

fn schedule_thumbnail_backfill(
    artifacts: Vec<crate::model::ArtifactMeta>,
    store: Arc<ArtifactStore>,
    previews: Arc<PreviewIntegration>,
) -> usize {
    if !previews.enabled() {
        return 0;
    }
    let candidates = artifacts
        .into_iter()
        .filter(|meta| !meta.is_bundle && !meta.body_sha256.is_empty())
        .collect::<Vec<_>>();
    let count = candidates.len();
    tokio::spawn(
        async move {
            for meta in candidates {
                if previews
                    .store()
                    .read_thumbnail(&meta, &meta.body_sha256)
                    .await
                    .is_some()
                {
                    continue;
                }
                let id = meta.id.clone();
                let expected_digest = meta.body_sha256.clone();
                let artifact_store = Arc::clone(&store);
                let deferred = PreviewHtml::Deferred(Box::new(move || {
                    Box::pin(async move {
                        let current = artifact_store.find_meta(&id).await.ok().flatten()?;
                        if current.body_sha256 != expected_digest || current.is_bundle {
                            return None;
                        }
                        let file = artifact_store
                            .read_body_for(&current)
                            .await
                            .ok()
                            .flatten()?;
                        String::from_utf8(file.content).ok()
                    })
                }));
                let _ = previews
                    .queue()
                    .enqueue(meta, deferred, PreviewPriority::Low)
                    .await;
            }
        }
        .instrument(info_span!("preview.backfill")),
    );
    count
}
