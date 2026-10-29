use super::*;

/// The HTML a queued job will render.
///
/// `Deferred` is Node's `typeof job.html === "function"` ([lib/thumbnails.js:218]): the startup
/// backfill re-reads the body *at execution time* and returns `None` when a concurrent update has
/// moved the digest on, so a stale body can never overwrite the authoritative thumbnail
/// ([server.js:138-143]).
pub enum PreviewHtml {
    /// Already-loaded body.
    Ready(String),
    /// Body loaded when the job reaches the front of the lane.
    Deferred(Box<dyn FnOnce() -> BoxFuture<'static, Option<String>> + Send>),
}

impl std::fmt::Debug for PreviewHtml {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(html) => formatter
                .debug_struct("Ready")
                .field("bytes", &html.len())
                .finish(),
            Self::Deferred(_) => formatter.write_str("Deferred"),
        }
    }
}

impl From<String> for PreviewHtml {
    fn from(html: String) -> Self {
        Self::Ready(html)
    }
}

struct QueuedJob {
    meta: ArtifactMeta,
    html: PreviewHtml,
    respond: oneshot::Sender<Option<Vec<u8>>>,
    reserved_bytes: u64,
}

#[derive(Default)]
struct QueueState {
    high: VecDeque<QueuedJob>,
    low: VecDeque<QueuedJob>,
    running: bool,
    reserved_bytes: u64,
}

/// Pending job counts — `pending()` [lib/thumbnails.js:235]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueDepth {
    pub high: usize,
    pub low: usize,
    pub running: bool,
    pub reserved_bytes: u64,
}

/// `createThumbnailQueue({ thumbnails })` — [lib/thumbnails.js:207-236]
///
/// One serial lane: exactly one render is in flight at a time, which is what the sidecar's
/// single-render contract requires ([preview-renderer/src/server.rs]). Interactive mutation
/// events (`High`) are always selected ahead of startup backfill (`Low`), so publishing an
/// artifact does not queue behind thousands of backfill jobs.
#[derive(Debug)]
pub struct ThumbnailQueue {
    store: Arc<ThumbnailStore>,
    state: Mutex<QueueState>,
    max_pending: usize,
    max_pending_bytes: u64,
    rejections: Option<Arc<AtomicU64>>,
    pressure: Arc<Mutex<Option<RenderQueuePressure>>>,
}

impl std::fmt::Debug for QueueState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueueState")
            .field("high", &self.high.len())
            .field("low", &self.low.len())
            .field("running", &self.running)
            .finish()
    }
}

impl ThumbnailQueue {
    #[must_use]
    pub fn new(store: Arc<ThumbnailStore>) -> Arc<Self> {
        Self::new_with_limits_and_counter(store, usize::MAX, u64::MAX, None)
    }

    /// Construct a queue with a bounded number of waiting jobs. The running job is not counted;
    /// this keeps renderer backpressure independent from request admission permits.
    #[must_use]
    pub fn new_with_limit(store: Arc<ThumbnailStore>, max_pending: usize) -> Arc<Self> {
        Self::new_with_limits_and_counter(store, max_pending, u64::MAX, None)
    }

    /// Same bounded queue with a low-cardinality rejection counter owned by the ingress metrics.
    #[must_use]
    pub fn new_with_limit_and_counter(
        store: Arc<ThumbnailStore>,
        max_pending: usize,
        rejections: Option<Arc<AtomicU64>>,
    ) -> Arc<Self> {
        Self::new_with_limits_and_counter(store, max_pending, u64::MAX, rejections)
    }

    /// Bounded job-count and declared-byte reservation used by the production notifier. Bytes
    /// remain reserved until the active render releases them, so queued work cannot race an
    /// in-flight body load and exceed the configured memory envelope.
    #[must_use]
    pub fn new_with_limits_and_counter(
        store: Arc<ThumbnailStore>,
        max_pending: usize,
        max_pending_bytes: u64,
        rejections: Option<Arc<AtomicU64>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            state: Mutex::new(QueueState::default()),
            max_pending: max_pending.max(1),
            max_pending_bytes: max_pending_bytes.max(1),
            rejections,
            pressure: Arc::new(Mutex::new(None)),
        })
    }

    #[must_use]
    pub fn with_pressure(self: Arc<Self>, pressure: RenderQueuePressure) -> Arc<Self> {
        *self
            .pressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pressure);
        self.update_pressure();
        self
    }

    /// `pending()` — [lib/thumbnails.js:235]
    #[must_use]
    pub fn depth(&self) -> QueueDepth {
        let state = self.lock_state();
        QueueDepth {
            high: state.high.len(),
            low: state.low.len(),
            running: state.running,
            reserved_bytes: state.reserved_bytes,
        }
    }

    /// `enqueue(meta, html, { priority })` — [lib/thumbnails.js:228-233]
    ///
    /// The job is registered **before** this returns, so a caller that enqueues `A` then `B`
    /// establishes their relative order regardless of when the returned futures are awaited.
    /// Dropping the returned future does not cancel the job.
    pub fn enqueue(
        self: &Arc<Self>,
        meta: ArtifactMeta,
        html: PreviewHtml,
        priority: PreviewPriority,
    ) -> BoxFuture<'static, Option<Vec<u8>>> {
        self.try_enqueue(meta, html, priority)
            .unwrap_or_else(|| Box::pin(async { None }))
    }

    /// Reserve count and byte capacity before scheduling work. `None` means no task may be
    /// spawned by the caller: the job was never accepted and no artifact body should be loaded.
    pub fn try_enqueue(
        self: &Arc<Self>,
        meta: ArtifactMeta,
        html: PreviewHtml,
        priority: PreviewPriority,
    ) -> Option<BoxFuture<'static, Option<Vec<u8>>>> {
        let (respond, receive) = oneshot::channel();
        let job = QueuedJob {
            reserved_bytes: meta.bytes,
            meta,
            html,
            respond,
        };
        let start = {
            let mut state = self.lock_state();
            if state.high.len().saturating_add(state.low.len()) >= self.max_pending
                || meta_bytes_would_exceed(
                    state.reserved_bytes,
                    job.reserved_bytes,
                    self.max_pending_bytes,
                )
            {
                if let Some(rejections) = &self.rejections {
                    rejections.fetch_add(1, Ordering::Relaxed);
                }
                return None;
            }
            state.reserved_bytes = state.reserved_bytes.saturating_add(job.reserved_bytes);
            match priority {
                PreviewPriority::High => state.high.push_back(job),
                PreviewPriority::Low => state.low.push_back(job),
            }
            let idle = !state.running;
            state.running = true;
            idle
        };
        if start {
            // `queueMicrotask(drain)` — [lib/thumbnails.js:231]
            let lane = Arc::clone(self);
            tokio::spawn(lane.drain());
        }
        self.update_pressure();
        // A sender dropped without a value (worker panic) resolves to "no thumbnail", never an
        // error: an unfinished preview must not fail the mutation that requested it.
        Some(Box::pin(async move { receive.await.unwrap_or(None) }))
    }

    /// Reserve queue capacity before cloning an HTML body from an HTTP mutation. The generic
    /// [`Self::enqueue`] API also supports deferred startup work, but the request path must not
    /// create an additional multi-megabyte `String` merely to discover that the renderer lane is
    /// full. A rejected preview is intentionally optional and resolves to `None`.
    pub fn enqueue_ready(
        self: &Arc<Self>,
        meta: ArtifactMeta,
        html: &str,
        priority: PreviewPriority,
    ) -> BoxFuture<'static, Option<Vec<u8>>> {
        let (respond, receive) = oneshot::channel();
        let reserved_bytes = meta.bytes;
        let start = {
            let mut state = self.lock_state();
            if state.high.len().saturating_add(state.low.len()) >= self.max_pending
                || meta_bytes_would_exceed(
                    state.reserved_bytes,
                    reserved_bytes,
                    self.max_pending_bytes,
                )
            {
                if let Some(rejections) = &self.rejections {
                    rejections.fetch_add(1, Ordering::Relaxed);
                }
                let _ = respond.send(None);
                return Box::pin(async { None });
            }
            state.reserved_bytes = state.reserved_bytes.saturating_add(reserved_bytes);
            let job = QueuedJob {
                meta,
                html: PreviewHtml::Ready(html.to_owned()),
                respond,
                reserved_bytes,
            };
            match priority {
                PreviewPriority::High => state.high.push_back(job),
                PreviewPriority::Low => state.low.push_back(job),
            }
            let idle = !state.running;
            state.running = true;
            idle
        };
        if start {
            let lane = Arc::clone(self);
            tokio::spawn(lane.drain());
        }
        self.update_pressure();
        Box::pin(async move { receive.await.unwrap_or(None) })
    }

    /// `drain()` — [lib/thumbnails.js:212-226]. `high.shift() || low.shift()`.
    async fn drain(self: Arc<Self>) {
        loop {
            let job = {
                let mut state = self.lock_state();
                match state.high.pop_front().or_else(|| state.low.pop_front()) {
                    Some(job) => job,
                    None => {
                        state.running = false;
                        // The previous job reported its completion while the lane was still
                        // marked running. Publish the final idle transition as well so the
                        // ingress gauge cannot remain stuck at one after a drained queue.
                        drop(state);
                        self.update_pressure();
                        return;
                    }
                }
            };
            self.update_pressure();
            let html = match job.html {
                PreviewHtml::Ready(html) => Some(html),
                PreviewHtml::Deferred(load) => load().await,
            };
            let png = self
                .store
                .ensure_thumbnail(&job.meta, html.as_deref())
                .await;
            {
                let mut state = self.lock_state();
                state.reserved_bytes = state.reserved_bytes.saturating_sub(job.reserved_bytes);
            }
            self.update_pressure();
            let _ = job.respond.send(png);
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update_pressure(&self) {
        let pressure = self
            .pressure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(pressure) = pressure {
            let state = self.lock_state();
            pressure.update(
                state.high.len().saturating_add(state.low.len()),
                state.running,
                state.reserved_bytes,
            );
        }
    }
}

fn meta_bytes_would_exceed(current: u64, requested: u64, limit: u64) -> bool {
    requested > limit || current.saturating_add(requested) > limit
}
