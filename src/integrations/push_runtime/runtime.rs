use std::{sync::Arc, time::Duration};

use tokio::{sync::watch, task::JoinSet};

use super::{PushService, PushTransport, SEND_INTERVAL, SWEEP_INTERVAL};
use std::sync::atomic::Ordering;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(11);

/// Owns the sweeper and sender tasks; consumed during graceful shutdown.
pub struct PushRuntime {
    shutdown: watch::Sender<bool>,
    service: Arc<PushService>,
    tasks: JoinSet<()>,
}

impl PushRuntime {
    /// Start both tasks. Call only when the feature is enabled.
    #[must_use]
    pub fn start(service: Arc<PushService>, transport: Arc<dyn PushTransport>) -> Self {
        Self::start_with_intervals(service, transport, SWEEP_INTERVAL, SEND_INTERVAL)
    }

    /// [`Self::start`] with explicit polling intervals (tests).
    #[must_use]
    pub fn start_with_intervals(
        service: Arc<PushService>,
        transport: Arc<dyn PushTransport>,
        sweep_interval: Duration,
        send_interval: Duration,
    ) -> Self {
        let (shutdown, receive) = watch::channel(false);
        let mut tasks = JoinSet::new();
        {
            let service = Arc::clone(&service);
            let mut shutdown = receive.clone();
            tasks.spawn(async move {
                loop {
                    if *shutdown.borrow() {
                        break;
                    }
                    if service.sweep_once().await.is_err() {
                        service
                            .telemetry
                            .counters
                            .sweeper_errors
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("push sweeper turn failed");
                    }
                    tokio::select! {
                        _ = shutdown.changed() => {}
                        () = tokio::time::sleep(sweep_interval) => {}
                        () = service.sweeper_wake.notified() => {}
                    }
                }
            });
        }
        {
            let service = Arc::clone(&service);
            let mut shutdown = receive;
            tasks.spawn(async move {
                loop {
                    if *shutdown.borrow() {
                        break;
                    }
                    if service.send_due(transport.as_ref()).await.is_err() {
                        service
                            .telemetry
                            .counters
                            .sweeper_errors
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("push sender turn failed");
                    }
                    tokio::select! {
                        _ = shutdown.changed() => {}
                        () = tokio::time::sleep(send_interval) => {}
                        () = service.sender_wake.notified() => {}
                    }
                }
            });
        }
        Self {
            shutdown,
            service,
            tasks,
        }
    }

    /// Stop both loops; an in-flight send may finish within its 10-second timeout.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        self.service.sweeper_wake.notify_one();
        self.service.sender_wake.notify_one();
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while let Some(joined) = self.tasks.join_next().await {
                if let Err(error) = joined {
                    tracing::warn!(error = %error, "push task ended unexpectedly");
                }
            }
        })
        .await;
        if drained.is_err() {
            self.tasks.abort_all();
        }
    }
}
