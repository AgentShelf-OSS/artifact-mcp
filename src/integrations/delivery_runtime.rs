//! Production lifecycle for the durable delivery worker pool.
//!
//! The coordinator is deliberately separate from producers. A producer may call
//! [`DeliveryWakeSignal::wake`] only *after* its transaction has committed; correctness never
//! depends on that best-effort hint because each worker also polls the durable SQLite queue.
//! Delivery remains bounded at-least-once: a process stop during an ambiguous provider attempt
//! can still create a duplicate, which is represented on the persisted outbox record.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    sync::{Notify, watch},
    task::JoinSet,
};

use crate::{
    error::AppError,
    integrations::{
        delivery_worker::{
            DeliveryWorker, WorkerDiscussionProvider, WorkerDiscussions, WorkerPreviewResolver,
            WorkerTurn,
        },
        discord_delivery::DiscordProviderTransport,
    },
    persistence::{
        outbox::{OutboxRepository, QueueStatus},
        webhooks::WebhookStore,
    },
    ports::BoxFuture,
};

/// Exactly two workers run in a production process. SQLite leases serialize competing claims.
pub const DELIVERY_WORKER_COUNT: usize = 2;
/// Polling remains the recovery path if a post-commit wake hint is lost across a crash.
pub const DELIVERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Provider attempts are capped at four seconds; shutdown never waits longer than this bound.
pub const DELIVERY_SHUTDOWN_GRACE: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DeliveryMetrics {
    active: u64,
    ready: u64,
    retrying: u64,
    dead_letter: u64,
    attempts: u64,
    ambiguous: u64,
    oldest_active_age_millis: u64,
    rate_limited_global: u64,
    rate_limited_target: u64,
    rate_limited_bucket: u64,
    max_rate_limit_delay_millis: u64,
    discussion_connected: u64,
    discussion_pending: u64,
    discussion_paused: u64,
    discussion_failed: u64,
    discussion_local_only: u64,
    discussion_pending_threads: u64,
    discussion_oldest_pending_thread_age_millis: u64,
    discussion_terminal_failures: u64,
    workers_healthy: u64,
    worker_errors: u64,
}

/// Thread-safe, low-cardinality delivery telemetry rendered on the existing `/metrics` route.
#[derive(Clone, Default)]
pub struct DeliveryTelemetry {
    metrics: Arc<Mutex<DeliveryMetrics>>,
}

impl DeliveryTelemetry {
    pub fn observe_queue(&self, status: QueueStatus) {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        metrics.active = status.active;
        metrics.ready = status.ready;
        metrics.retrying = status.retrying;
        metrics.dead_letter = status.dead_letter;
        metrics.attempts = status.attempts;
        metrics.ambiguous = status.ambiguous;
        metrics.oldest_active_age_millis = status.oldest_active_age_millis;
        metrics.rate_limited_global = status.rate_limited_global;
        metrics.rate_limited_target = status.rate_limited_target;
        metrics.rate_limited_bucket = status.rate_limited_bucket;
        metrics.max_rate_limit_delay_millis = status.max_rate_limit_delay_millis;
        metrics.discussion_connected = status.discussion_connected;
        metrics.discussion_pending = status.discussion_pending;
        metrics.discussion_paused = status.discussion_paused;
        metrics.discussion_failed = status.discussion_failed;
        metrics.discussion_local_only = status.discussion_local_only;
        metrics.discussion_pending_threads = status.discussion_pending_threads;
        metrics.discussion_oldest_pending_thread_age_millis =
            status.discussion_oldest_pending_thread_age_millis;
        metrics.discussion_terminal_failures = status.discussion_terminal_failures;
    }

    fn worker_started(&self) {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        metrics.workers_healthy = metrics.workers_healthy.saturating_add(1);
    }

    fn worker_stopped(&self) {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        metrics.workers_healthy = metrics.workers_healthy.saturating_sub(1);
    }

    fn worker_error(&self) {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        metrics.worker_errors = metrics.worker_errors.saturating_add(1);
    }

    /// Prometheus exposition keeps every label-free value safe for public scraping.
    pub fn render_prometheus(&self) -> String {
        let metrics = *self
            .metrics
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        format!(
            "# HELP artifact_mcp_delivery_queue_ready Durable deliveries ready to claim.\n\
             # TYPE artifact_mcp_delivery_queue_ready gauge\n\
             artifact_mcp_delivery_queue_ready {}\n\
             # HELP artifact_mcp_delivery_queue_active Durable deliveries that have not reached a terminal outcome.\n\
             # TYPE artifact_mcp_delivery_queue_active gauge\n\
             artifact_mcp_delivery_queue_active {}\n\
             # HELP artifact_mcp_delivery_queue_retrying Durable deliveries waiting to retry.\n\
             # TYPE artifact_mcp_delivery_queue_retrying gauge\n\
             artifact_mcp_delivery_queue_retrying {}\n\
             # HELP artifact_mcp_delivery_queue_dead_letter Durable deliveries requiring operator review.\n\
             # TYPE artifact_mcp_delivery_queue_dead_letter gauge\n\
             artifact_mcp_delivery_queue_dead_letter {}\n\
             # HELP artifact_mcp_delivery_terminal_failures Terminal provider outcomes retained for operator review.\n\
             # TYPE artifact_mcp_delivery_terminal_failures gauge\n\
             artifact_mcp_delivery_terminal_failures {}\n\
             # HELP artifact_mcp_delivery_attempts Aggregate persisted delivery attempts across retained outbox rows.\n\
             # TYPE artifact_mcp_delivery_attempts gauge\n\
             artifact_mcp_delivery_attempts {}\n\
             # HELP artifact_mcp_delivery_ambiguous Durable rows carrying explicit duplicate-delivery risk.\n\
             # TYPE artifact_mcp_delivery_ambiguous gauge\n\
             artifact_mcp_delivery_ambiguous {}\n\
             # HELP artifact_mcp_delivery_oldest_active_age_seconds Age of the oldest active durable delivery.\n\
             # TYPE artifact_mcp_delivery_oldest_active_age_seconds gauge\n\
             artifact_mcp_delivery_oldest_active_age_seconds {:.3}\n\
             # HELP artifact_mcp_delivery_rate_limit_blocked Active Discord rate-limit state by safe scope.\n\
             # TYPE artifact_mcp_delivery_rate_limit_blocked gauge\n\
             artifact_mcp_delivery_rate_limit_blocked{{scope=\"global\"}} {}\n\
             artifact_mcp_delivery_rate_limit_blocked{{scope=\"target\"}} {}\n\
             artifact_mcp_delivery_rate_limit_blocked{{scope=\"bucket\"}} {}\n\
             # HELP artifact_mcp_delivery_rate_limit_max_delay_seconds Largest active provider-supplied rate-limit delay.\n\
             # TYPE artifact_mcp_delivery_rate_limit_max_delay_seconds gauge\n\
             artifact_mcp_delivery_rate_limit_max_delay_seconds {:.3}\n\
             # HELP artifact_mcp_discussion_mirrors Discussion mirrors by fixed, non-sensitive state.\n\
             # TYPE artifact_mcp_discussion_mirrors gauge\n\
             artifact_mcp_discussion_mirrors{{state=\"connected\"}} {}\n\
             artifact_mcp_discussion_mirrors{{state=\"pending\"}} {}\n\
             artifact_mcp_discussion_mirrors{{state=\"paused\"}} {}\n\
             artifact_mcp_discussion_mirrors{{state=\"failed\"}} {}\n\
             artifact_mcp_discussion_mirrors{{state=\"local_only\"}} {}\n\
             # HELP artifact_mcp_discussion_pending_threads Active durable discussion-root deliveries.\n\
             # TYPE artifact_mcp_discussion_pending_threads gauge\n\
             artifact_mcp_discussion_pending_threads {}\n\
             # HELP artifact_mcp_discussion_oldest_pending_thread_age_seconds Age of the oldest active durable discussion-root delivery.\n\
             # TYPE artifact_mcp_discussion_oldest_pending_thread_age_seconds gauge\n\
             artifact_mcp_discussion_oldest_pending_thread_age_seconds {:.3}\n\
             # HELP artifact_mcp_discussion_terminal_failures Terminal discussion-delivery outcomes retained for operator review.\n\
             # TYPE artifact_mcp_discussion_terminal_failures gauge\n\
             artifact_mcp_discussion_terminal_failures {}\n\
             # HELP artifact_mcp_delivery_workers_healthy Healthy delivery workers in this process.\n\
             # TYPE artifact_mcp_delivery_workers_healthy gauge\n\
             artifact_mcp_delivery_workers_healthy {}\n\
             # HELP artifact_mcp_delivery_workers_expected Configured delivery workers per process.\n\
             # TYPE artifact_mcp_delivery_workers_expected gauge\n\
             artifact_mcp_delivery_workers_expected {DELIVERY_WORKER_COUNT}\n\
             # HELP artifact_mcp_delivery_worker_errors_total Worker turns that failed before an outcome was persisted.\n\
             # TYPE artifact_mcp_delivery_worker_errors_total counter\n\
             artifact_mcp_delivery_worker_errors_total {}\n",
            metrics.ready,
            metrics.active,
            metrics.retrying,
            metrics.dead_letter,
            metrics.dead_letter,
            metrics.attempts,
            metrics.ambiguous,
            metrics.oldest_active_age_millis as f64 / 1_000.0,
            metrics.rate_limited_global,
            metrics.rate_limited_target,
            metrics.rate_limited_bucket,
            metrics.max_rate_limit_delay_millis as f64 / 1_000.0,
            metrics.discussion_connected,
            metrics.discussion_pending,
            metrics.discussion_paused,
            metrics.discussion_failed,
            metrics.discussion_local_only,
            metrics.discussion_pending_threads,
            metrics.discussion_oldest_pending_thread_age_millis as f64 / 1_000.0,
            metrics.discussion_terminal_failures,
            metrics.workers_healthy,
            metrics.worker_errors,
        )
    }
}

/// A safe, lossy acceleration hint. It carries neither provider metadata nor delivery payload.
#[derive(Clone, Default)]
pub struct DeliveryWakeSignal {
    notify: Arc<Notify>,
}

impl DeliveryWakeSignal {
    /// Wake sleeping workers after a producer transaction commits. Polling remains mandatory.
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }
}

trait DeliveryTurnRunner: Send + Sync {
    fn run_once<'a>(
        &'a self,
        shutdown: &'a mut watch::Receiver<bool>,
    ) -> BoxFuture<'a, Result<WorkerTurn, AppError>>;
}

impl DeliveryTurnRunner for DeliveryWorker {
    fn run_once<'a>(
        &'a self,
        shutdown: &'a mut watch::Receiver<bool>,
    ) -> BoxFuture<'a, Result<WorkerTurn, AppError>> {
        Box::pin(self.run_once(shutdown))
    }
}

trait QueueStatusReader: Send + Sync {
    fn status<'a>(&'a self) -> BoxFuture<'a, Result<QueueStatus, AppError>>;
}

impl QueueStatusReader for OutboxRepository {
    fn status<'a>(&'a self) -> BoxFuture<'a, Result<QueueStatus, AppError>> {
        Box::pin(self.status())
    }
}

/// Owns worker tasks and is consumed during graceful process shutdown.
pub struct DeliveryRuntime {
    shutdown: watch::Sender<bool>,
    wake: DeliveryWakeSignal,
    tasks: JoinSet<()>,
}

impl DeliveryRuntime {
    /// Start the two production workers after storage reconciliation has completed.
    #[must_use]
    pub fn start(
        outbox: Arc<OutboxRepository>,
        webhooks: Arc<WebhookStore>,
        provider: Arc<DiscordProviderTransport>,
        discussions: Arc<dyn WorkerDiscussions>,
        discussion_provider: Arc<dyn WorkerDiscussionProvider>,
        previews: Arc<dyn WorkerPreviewResolver>,
        telemetry: DeliveryTelemetry,
    ) -> (Self, DeliveryWakeSignal) {
        let workers = (0..DELIVERY_WORKER_COUNT)
            .map(|index| {
                Arc::new(
                    DeliveryWorker::new(
                        Arc::clone(&outbox),
                        Arc::clone(&webhooks),
                        Arc::clone(&provider),
                        format!("delivery-{}", index + 1),
                    )
                    .with_discussion_adapters(
                        Arc::clone(&discussions),
                        Arc::clone(&discussion_provider),
                    )
                    .with_preview_resolver(Arc::clone(&previews)),
                ) as Arc<dyn DeliveryTurnRunner>
            })
            .collect();
        Self::start_with_runners(workers, Some(outbox), telemetry, DELIVERY_POLL_INTERVAL)
    }

    fn start_with_runners(
        workers: Vec<Arc<dyn DeliveryTurnRunner>>,
        status: Option<Arc<dyn QueueStatusReader>>,
        telemetry: DeliveryTelemetry,
        poll_interval: Duration,
    ) -> (Self, DeliveryWakeSignal) {
        debug_assert_eq!(workers.len(), DELIVERY_WORKER_COUNT);
        let (shutdown, receive) = watch::channel(false);
        let wake = DeliveryWakeSignal::default();
        let mut tasks = JoinSet::new();
        for worker in workers {
            tasks.spawn(run_worker(
                worker,
                receive.clone(),
                wake.clone(),
                status.clone(),
                telemetry.clone(),
                poll_interval,
            ));
        }
        (
            Self {
                shutdown,
                wake: wake.clone(),
                tasks,
            },
            wake,
        )
    }

    /// Stop new claims immediately, then permit bounded in-flight delivery work to finish.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        self.wake.wake();
        let drained = tokio::time::timeout(DELIVERY_SHUTDOWN_GRACE, async {
            while let Some(joined) = self.tasks.join_next().await {
                if let Err(error) = joined {
                    tracing::warn!(error = %error, "delivery worker task ended unexpectedly");
                }
            }
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                grace_ms = DELIVERY_SHUTDOWN_GRACE.as_millis(),
                "delivery shutdown deadline exceeded; aborting workers"
            );
            self.tasks.abort_all();
            while self.tasks.join_next().await.is_some() {}
        }
    }
}

async fn run_worker(
    worker: Arc<dyn DeliveryTurnRunner>,
    mut shutdown: watch::Receiver<bool>,
    wake: DeliveryWakeSignal,
    status: Option<Arc<dyn QueueStatusReader>>,
    telemetry: DeliveryTelemetry,
    poll_interval: Duration,
) {
    telemetry.worker_started();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let should_wait = match worker.run_once(&mut shutdown).await {
            Ok(WorkerTurn::Processed) => {
                if let Err(error) = refresh(&status, &telemetry).await {
                    tracing::warn!(error = %error, "delivery queue status refresh failed");
                }
                false
            }
            Ok(WorkerTurn::Shutdown) => break,
            Ok(WorkerTurn::Idle) => true,
            Err(error) => {
                telemetry.worker_error();
                tracing::warn!(error = %error, "delivery worker turn failed");
                true
            }
        };
        if should_wait {
            if let Err(error) = refresh(&status, &telemetry).await {
                tracing::warn!(error = %error, "delivery queue status refresh failed");
            }
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                _ = wake.notify.notified() => {}
                _ = tokio::time::sleep(poll_interval) => {}
            }
        }
    }
    telemetry.worker_stopped();
}

async fn refresh(
    status: &Option<Arc<dyn QueueStatusReader>>,
    telemetry: &DeliveryTelemetry,
) -> Result<(), AppError> {
    if let Some(status) = status {
        telemetry.observe_queue(status.status().await?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct IdleRunner(AtomicUsize);

    impl DeliveryTurnRunner for IdleRunner {
        fn run_once<'a>(
            &'a self,
            _shutdown: &'a mut watch::Receiver<bool>,
        ) -> BoxFuture<'a, Result<WorkerTurn, AppError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(WorkerTurn::Idle) })
        }
    }

    async fn wait_for(count: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_millis(250), async {
            while count.load(Ordering::SeqCst) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker activity before test deadline");
    }

    #[tokio::test]
    async fn starts_exactly_two_workers_and_stops_claims_on_shutdown() {
        let first = Arc::new(IdleRunner(AtomicUsize::new(0)));
        let second = Arc::new(IdleRunner(AtomicUsize::new(0)));
        let telemetry = DeliveryTelemetry::default();
        let (runtime, _) = DeliveryRuntime::start_with_runners(
            vec![first.clone(), second.clone()],
            None,
            telemetry.clone(),
            Duration::from_secs(60),
        );
        wait_for(&first.0, 1).await;
        wait_for(&second.0, 1).await;
        runtime.shutdown().await;
        let first_claims = first.0.load(Ordering::SeqCst);
        let second_claims = second.0.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(first.0.load(Ordering::SeqCst), first_claims);
        assert_eq!(second.0.load(Ordering::SeqCst), second_claims);
        assert!(
            telemetry
                .render_prometheus()
                .contains("artifact_mcp_delivery_workers_healthy 0")
        );
    }

    #[tokio::test]
    async fn wake_accelerates_idle_workers_and_polling_remains_a_fallback() {
        let first = Arc::new(IdleRunner(AtomicUsize::new(0)));
        let second = Arc::new(IdleRunner(AtomicUsize::new(0)));
        let (runtime, wake) = DeliveryRuntime::start_with_runners(
            vec![first.clone(), second.clone()],
            None,
            DeliveryTelemetry::default(),
            Duration::from_millis(20),
        );
        wait_for(&first.0, 1).await;
        wait_for(&second.0, 1).await;
        wake.wake();
        wait_for(&first.0, 2).await;
        wait_for(&second.0, 2).await;
        wait_for(&first.0, 3).await;
        wait_for(&second.0, 3).await;
        runtime.shutdown().await;
    }

    #[test]
    fn metrics_are_aggregate_and_include_queue_and_worker_health() {
        let telemetry = DeliveryTelemetry::default();
        telemetry.observe_queue(QueueStatus {
            active: 5,
            ready: 3,
            retrying: 2,
            dead_letter: 1,
            attempts: 7,
            ambiguous: 1,
            oldest_active_age_millis: 1_250,
            rate_limited_global: 1,
            rate_limited_target: 2,
            rate_limited_bucket: 3,
            max_rate_limit_delay_millis: 2_500,
            discussion_connected: 2,
            discussion_pending: 3,
            discussion_paused: 4,
            discussion_failed: 5,
            discussion_local_only: 6,
            discussion_pending_threads: 7,
            discussion_oldest_pending_thread_age_millis: 1_500,
            discussion_terminal_failures: 8,
        });
        let output = telemetry.render_prometheus();
        assert!(output.contains("artifact_mcp_delivery_queue_ready 3"));
        assert!(output.contains("artifact_mcp_delivery_queue_active 5"));
        assert!(output.contains("artifact_mcp_delivery_queue_retrying 2"));
        assert!(output.contains("artifact_mcp_delivery_queue_dead_letter 1"));
        assert!(output.contains("artifact_mcp_delivery_terminal_failures 1"));
        assert!(output.contains("artifact_mcp_delivery_attempts 7"));
        assert!(output.contains("artifact_mcp_delivery_ambiguous 1"));
        assert!(output.contains("artifact_mcp_delivery_oldest_active_age_seconds 1.250"));
        assert!(output.contains("artifact_mcp_delivery_workers_expected 2"));
        assert!(output.contains("artifact_mcp_delivery_rate_limit_blocked{scope=\"global\"} 1"));
        let discussion_series = output
            .lines()
            .filter(|line| line.starts_with("artifact_mcp_discussion_"))
            .collect::<Vec<_>>();
        assert_eq!(
            discussion_series,
            [
                "artifact_mcp_discussion_mirrors{state=\"connected\"} 2",
                "artifact_mcp_discussion_mirrors{state=\"pending\"} 3",
                "artifact_mcp_discussion_mirrors{state=\"paused\"} 4",
                "artifact_mcp_discussion_mirrors{state=\"failed\"} 5",
                "artifact_mcp_discussion_mirrors{state=\"local_only\"} 6",
                "artifact_mcp_discussion_pending_threads 7",
                "artifact_mcp_discussion_oldest_pending_thread_age_seconds 1.500",
                "artifact_mcp_discussion_terminal_failures 8",
            ]
        );
        assert!(!output.contains("org-secret"));
        assert!(!output.contains("artifact-secret"));
        assert!(!output.contains("connection-secret"));
        assert!(!output.contains("thread-secret"));
        assert!(!output.contains("webhook-secret"));
    }
}
