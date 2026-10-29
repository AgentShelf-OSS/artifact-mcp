use super::*;

// ---------------------------------------------------------------------------
// Port implementation
// ---------------------------------------------------------------------------

/// The frozen [`PreviewService`] adapter: store + serial lane.
///
/// Every method returns `Ok(...)`. There is deliberately no `Err` path — see the module-level
/// "optional by design" note.
#[derive(Debug)]
pub struct PreviewIntegration {
    store: Arc<ThumbnailStore>,
    queue: Arc<ThumbnailQueue>,
}

impl PreviewIntegration {
    #[must_use]
    pub fn new(store: Arc<ThumbnailStore>) -> Self {
        let queue = ThumbnailQueue::new(Arc::clone(&store));
        Self { store, queue }
    }

    /// Compose the production preview adapter around a caller-managed serial queue.
    ///
    /// This is useful when composition needs a bounded queue with explicit admission limits.
    #[must_use]
    pub const fn from_parts(store: Arc<ThumbnailStore>, queue: Arc<ThumbnailQueue>) -> Self {
        Self { store, queue }
    }

    /// Build the whole preview stack from configuration. A missing `PREVIEW_RENDERER_URL` yields
    /// a fully inert but perfectly usable service.
    #[must_use]
    pub fn from_config(config: &AppConfig) -> Self {
        Self::from_config_with_queue_counter(config, None, None)
    }

    /// Production constructor that connects bounded renderer pressure to origin metrics.
    #[must_use]
    pub fn from_config_with_queue_counter(
        config: &AppConfig,
        queue_rejections: Option<Arc<AtomicU64>>,
        queue_pressure: Option<RenderQueuePressure>,
    ) -> Self {
        let renderer = Arc::new(PreviewRenderer::new(&config.preview));
        let max_pending = usize::try_from(config.ingress.render_queue_jobs).unwrap_or(usize::MAX);
        let store = Arc::new(ThumbnailStore::from_config(config, renderer));
        let queue = ThumbnailQueue::new_with_limits_and_counter(
            Arc::clone(&store),
            max_pending,
            config.ingress.render_queue_bytes,
            queue_rejections,
        );
        let queue = queue_pressure.map_or(queue.clone(), |pressure| queue.with_pressure(pressure));
        Self { store, queue }
    }

    #[must_use]
    pub fn store(&self) -> &Arc<ThumbnailStore> {
        &self.store
    }

    #[must_use]
    pub fn queue(&self) -> &Arc<ThumbnailQueue> {
        &self.queue
    }
}

/// Production-only scheduler for persistent artifact gallery thumbnails.
///
/// Queue admission occurs after the artifact mutation and its durable delivery fanout have
/// committed. It never emits a notification and its deferred body loader re-checks the current
/// digest before touching disk, so a fast update cannot render an obsolete body as current.
#[derive(Debug)]
pub struct PersistentThumbnailScheduler {
    previews: Arc<PreviewIntegration>,
}

impl PersistentThumbnailScheduler {
    #[must_use]
    pub const fn new(previews: Arc<PreviewIntegration>) -> Self {
        Self { previews }
    }
}

impl PostCommitPreviewScheduler for PersistentThumbnailScheduler {
    fn schedule(&self, artifacts: ArtifactStore, meta: ArtifactMeta) {
        if meta.is_bundle || BodyDigest::parse(&meta.body_sha256).is_none() {
            return;
        }
        let id = meta.id.clone();
        let expected_digest = meta.body_sha256.clone();
        let deferred = PreviewHtml::Deferred(Box::new(move || {
            Box::pin(async move {
                let current = artifacts.find_meta(&id).await.ok().flatten()?;
                if current.is_bundle || current.body_sha256 != expected_digest {
                    return None;
                }
                let file = artifacts.read_body_for(&current).await.ok().flatten()?;
                String::from_utf8(file.content).ok()
            })
        }));
        // Full queue pressure makes gallery preview preparation optional, never a successful
        // mutation failure. `try_enqueue` starts the lane itself and returns without awaiting
        // rendering; one post-commit call creates exactly one queued job.
        let _ = self
            .previews
            .queue()
            .try_enqueue(meta, deferred, PreviewPriority::High);
    }
}

impl PreviewService for PreviewIntegration {
    fn enabled(&self) -> bool {
        self.store.enabled()
    }

    fn read_thumbnail<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        digest: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move { Ok(self.store.read_thumbnail(artifact.meta(), digest).await) })
    }

    fn read_thumbnail_sync(
        &self,
        meta: &ArtifactMeta,
        digest: &str,
    ) -> Result<Option<Vec<u8>>, AppError> {
        Ok(self.store.read_thumbnail_sync(meta, digest))
    }

    fn placeholder(&self, meta: &ArtifactMeta, accent: Option<&str>) -> Vec<u8> {
        thumbnail_placeholder(meta, accent)
    }

    fn ensure_thumbnail<'a>(
        &'a self,
        meta: &'a ArtifactMeta,
        html: &'a str,
        priority: PreviewPriority,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move {
            // The lane exists to serialise *renders*. With no renderer there is nothing to
            // serialise, so the store is consulted directly — which still returns a thumbnail
            // left on disk by an earlier run that had the sidecar enabled, exactly as Node's
            // `generate` does before it checks `renderer?.enabled` ([lib/thumbnails.js:113-115]).
            if !self.store.enabled() {
                return Ok(self.store.ensure_thumbnail(meta, Some(html)).await);
            }
            let job = self.queue.enqueue_ready(meta.clone(), html, priority);
            Ok(job.await)
        })
    }

    fn remove_artifact<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            self.store.remove_artifact(id).await;
            Ok(())
        })
    }
}
