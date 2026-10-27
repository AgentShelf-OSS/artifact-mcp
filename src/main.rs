//! Production composition, observability, health, and listener startup.
//!
//! The port adapters live in [`artifact_mcp::adapters`] and the startup stage helpers in
//! [`artifact_mcp::startup`]. This file runs the stages in order, emits the startup observer
//! stages, and owns the HTTP/1 listener and process entry point.

use std::{future::Future, net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use artifact_mcp::{
    AppDeps, build_router,
    config::{AppConfig, IdSource, NanoIdSource},
    error::AppError,
    http::middleware::{
        AccessRetryState, access_session_retry, attach_audit_request_id,
        prevent_response_transforms,
    },
    ports::integrations::HealthReport,
    render::portal::AskamaPageRenderer,
    security::identity::assert_ready,
    startup::{self, BackgroundRuntimes, IntegrationStores},
};
use axum::{
    Router,
    http::{HeaderName, Request},
    middleware,
};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use thiserror::Error;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tower::ServiceExt as _;
use tower_http::{
    request_id::{MakeRequestUuid, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use tracing_subscriber::EnvFilter;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub(crate) enum RuntimeError {
    #[error(transparent)]
    Application(#[from] AppError),
    #[error("listener I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("healthcheck failed: {0}")]
    Healthcheck(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupStage {
    DatabaseReady,
    StorageReconciled,
    DeliveryWorkersStarted,
    ListenerBindRequested,
}

pub(crate) trait StartupObserver: Send + Sync {
    fn stage(&self, stage: StartupStage);
}

#[derive(Clone, Copy, Debug, Default)]
struct NoopStartupObserver;

impl StartupObserver for NoopStartupObserver {
    fn stage(&self, _stage: StartupStage) {}
}

struct Bootstrapped {
    router: Router,
    host: String,
    port: u16,
    runtimes: BackgroundRuntimes,
}

/// Run the startup stages in order. The observer receives `DatabaseReady` after the database
/// opens, `StorageReconciled` after storage reconciliation, and `DeliveryWorkersStarted` after
/// the background workers start.
#[tracing::instrument(skip_all)]
async fn bootstrap(
    config: AppConfig,
    observer: Arc<dyn StartupObserver>,
) -> Result<Bootstrapped, RuntimeError> {
    assert_ready(&config)?;
    let data_broker = artifact_mcp::data::DataBroker::from_env().map_err(AppError::Validation)?;
    let config = Arc::new(config);
    let protection = startup::webhook_protection(&config)?;

    // Stage 1: database, migrations, live data, and publisher keys.
    let pool = startup::open_database(&config, &protection).await?;
    observer.stage(StartupStage::DatabaseReady);
    startup::log_effective_pragmas(&pool).await?;
    let audit_key = startup::audit_ledger_key(&config)?;
    let data = Arc::new(
        data_broker
            .with_pool(pool.clone())
            .with_audit_key(audit_key),
    );
    data.validate_registry()
        .await
        .map_err(AppError::Validation)?;
    let key_store = startup::seed_publisher_keys(&pool, &config, audit_key).await?;

    // Stage 2: artifact storage reconciliation and the preview cache.
    let ids: Arc<dyn IdSource> = Arc::new(NanoIdSource::default());
    let content = startup::open_content(&config, &pool, &ids)?;
    startup::reconcile_storage(&content.artifacts, || {
        observer.stage(StartupStage::StorageReconciled);
    })
    .await?;
    startup::warm_previews(&content.artifacts, &content.previews).await?;

    // Stage 3: publisher authentication and viewer identity.
    let auth = startup::authentication(&config, &pool)?;

    // Stage 4: integration stores and background workers. Durable workers start only after
    // storage reconciliation, digest recovery, and preview cache reconciliation have finished.
    let stores = IntegrationStores::open(&config, &pool, &ids, &protection);
    let notifications = startup::notification_sink(&stores, &content)?;
    let workers = startup::start_background_workers(&config, &pool, &stores, &content, audit_key)?;
    observer.stage(StartupStage::DeliveryWorkersStarted);

    // Stage 5: port adapters.
    let services = startup::port_services(&config, pool, key_store, &stores, ids, audit_key)?;

    // Stage 6: router dependencies.
    let host = config.listen_host.clone();
    let port = config.port;
    let deps = AppDeps {
        data,
        publisher_auth: auth.publisher_auth,
        viewer_identity: auth.viewer_identity,
        artifacts: content.artifacts,
        admin: services.admin,
        discussions: services.discussions,
        engagement: services.engagement,
        viewer_state: services.viewer_state,
        shares: services.shares,
        pages: Arc::new(AskamaPageRenderer::from_config(&config)),
        previews: content.previews,
        notifications,
        health: services.health,
        ingress: content.ingress,
        preview_tasks: artifact_mcp::mcp::tasks::PreviewTaskStore::new(&config.data_dir),
        mcp_telemetry: artifact_mcp::observability::McpTelemetry::default(),
        delivery_telemetry: workers.delivery_telemetry,
        delivery_wake: workers.delivery_wake,
        audit_access: services.audit_access,
        push: workers.push,
        config,
    };
    artifact_mcp::mcp::tasks::resume_preview_tasks(deps.clone());
    let router = runtime_router(deps);
    Ok(Bootstrapped {
        router,
        host,
        port,
        runtimes: workers.runtimes,
    })
}

fn runtime_router(deps: AppDeps) -> Router {
    let access_retry =
        AccessRetryState::new(deps.config.access.identity_mode(), Arc::clone(&deps.pages));
    let trace = TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .and_then(|id| id.header_value().to_str().ok())
            .unwrap_or("unknown");
        tracing::info_span!(
            "http.request",
            request_id,
            method = %request.method()
        )
    });
    build_router(deps)
        .layer(middleware::from_fn_with_state(
            access_retry,
            access_session_retry,
        ))
        .layer(middleware::from_fn(attach_audit_request_id))
        .layer(trace)
        .layer(SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeRequestUuid))
        .layer(middleware::from_fn(prevent_response_transforms))
}

#[tracing::instrument(skip_all)]
#[allow(dead_code)] // exercised by the listener-free native runtime tests
pub(crate) async fn run_with_bind<B, F>(
    config: AppConfig,
    observer: Arc<dyn StartupObserver>,
    bind_and_serve: B,
) -> Result<(), RuntimeError>
where
    B: FnOnce(String, u16, Router) -> F,
    F: Future<Output = Result<(), RuntimeError>>,
{
    let bootstrapped = bootstrap(config, Arc::clone(&observer)).await?;
    observer.stage(StartupStage::ListenerBindRequested);
    let result = bind_and_serve(bootstrapped.host, bootstrapped.port, bootstrapped.router).await;
    // This path keeps its own stop order (push last), which differs from
    // `BackgroundRuntimes::shutdown` (inbound last).
    let runtimes = bootstrapped.runtimes;
    runtimes.delivery.shutdown().await;
    runtimes.recovery.shutdown().await;
    runtimes.inbound.shutdown().await;
    if let Some(push_runtime) = runtimes.push {
        push_runtime.shutdown().await;
    }
    result
}

#[tracing::instrument(skip_all)]
async fn serve(config: AppConfig) -> Result<(), RuntimeError> {
    let ingress = config.ingress.clone();
    let observer = Arc::new(NoopStartupObserver);
    let bootstrapped = bootstrap(config, observer.clone()).await?;
    observer.stage(StartupStage::ListenerBindRequested);
    serve_listener(
        bootstrapped.host,
        bootstrapped.port,
        bootstrapped.router,
        ingress,
        bootstrapped.runtimes,
    )
    .await
}

/// HTTP/1 listener boundary. Hyper owns header parsing here so slowloris/header limits apply
/// before Axum constructs a request; body deadlines remain in the JSON/MCP readers because a
/// blanket handler timeout would make durable mutations ambiguously succeed after a client-side
/// timeout.
async fn serve_listener(
    host: String,
    port: u16,
    router: Router,
    ingress: artifact_mcp::config::IngressConfig,
    runtimes: BackgroundRuntimes,
) -> Result<(), RuntimeError> {
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!(listen_host = %host, port, "listener ready");
    serve_listener_with_shutdown_inner(listener, router, ingress, Some(runtimes), shutdown_signal())
        .await
}

/// Serve an already-bound listener until the supplied shutdown future resolves. Keeping this
/// seam separate lets the runtime test exercise Hyper's actual header deadline and connection
/// permits without binding a process-wide signal handler.
#[allow(dead_code)] // exercised by the listener-free native runtime tests
pub(crate) async fn serve_listener_with_shutdown<F>(
    listener: TcpListener,
    router: Router,
    ingress: artifact_mcp::config::IngressConfig,
    shutdown: F,
) -> Result<(), RuntimeError>
where
    F: Future<Output = ()> + Send,
{
    serve_listener_with_shutdown_inner(listener, router, ingress, None, shutdown).await
}

async fn serve_listener_with_shutdown_inner<F>(
    listener: TcpListener,
    router: Router,
    ingress: artifact_mcp::config::IngressConfig,
    runtimes: Option<BackgroundRuntimes>,
    shutdown: F,
) -> Result<(), RuntimeError>
where
    F: Future<Output = ()> + Send,
{
    tokio::pin!(shutdown);
    let connection_permits = Arc::new(tokio::sync::Semaphore::new(
        usize::try_from(ingress.max_connections).unwrap_or(usize::MAX),
    ));
    let (stop_send, stop_receive) = watch::channel(());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined {
                    tracing::warn!(error = %error, "connection task ended unexpectedly");
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(error = %error, "listener accept failed");
                        continue;
                    }
                };
                let router = router.clone();
                let ingress = ingress.clone();
                let mut stop = stop_receive.clone();
                let Some(connection_permit) = connection_permits.clone().try_acquire_owned().ok() else {
                    tracing::warn!("connection admission limit reached; dropping socket");
                    drop(stream);
                    continue;
                };
                connections.spawn(async move {
                    let _connection_permit = connection_permit;
                    let service = match router
                        .into_make_service_with_connect_info::<SocketAddr>()
                        .oneshot(peer)
                        .await
                    {
                        Ok(service) => service,
                        Err(never) => match never {},
                    };
                    let mut builder = http1::Builder::new();
                    builder
                        .max_headers(usize::try_from(ingress.max_headers).unwrap_or(usize::MAX))
                        .max_buf_size(usize::try_from(ingress.max_header_bytes).unwrap_or(usize::MAX))
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_millis(ingress.read_timeout_ms));
                    let mut connection = Box::pin(
                        builder.serve_connection(TokioIo::new(stream), TowerToHyperService::new(service)),
                    );
                    let result = tokio::select! {
                        result = &mut connection => result,
                        _ = stop.changed() => {
                            connection.as_mut().graceful_shutdown();
                            connection.await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(error = %error, "HTTP connection ended");
                    }
                });
            }
        }
    }
    // Stop accepting new sockets and ask each HTTP/1 connection to leave keep-alive mode. We
    // join rather than abort: a mutation already admitted may complete its durable work, while
    // new requests are no longer accepted on that connection.
    if let Some(runtimes) = runtimes {
        runtimes.shutdown().await;
    }
    drop(listener);
    let _ = stop_send.send(());
    let grace = Duration::from_millis(ingress.shutdown_grace_ms);
    if tokio::time::timeout(grace, async {
        while let Some(joined) = connections.join_next().await {
            if let Err(error) = joined {
                tracing::warn!(error = %error, "connection task ended during graceful shutdown");
            }
        }
    })
    .await
    .is_err()
    {
        // This is process termination, not a success/failure response to a durable mutation.
        // The lifecycle's crash recovery path reconciles any interrupted staging work on restart.
        tracing::warn!(
            grace_ms = ingress.shutdown_grace_ms,
            "graceful shutdown deadline exceeded; terminating remaining connections"
        );
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    tracing::info!("listener stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %error, "Ctrl-C signal handler failed");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                let _ = signal.recv().await;
            }
            Err(error) => tracing::error!(error = %error, "SIGTERM handler failed"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

fn healthcheck_port() -> Result<u16, RuntimeError> {
    match std::env::var("PORT") {
        Ok(raw) => raw
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| {
                RuntimeError::Healthcheck("PORT must be between 1 and 65535".to_owned())
            }),
        Err(_) => Ok(artifact_mcp::config::DEFAULT_PORT),
    }
}

#[tracing::instrument(skip_all)]
async fn healthcheck() -> Result<(), RuntimeError> {
    let port = healthcheck_port()?;
    let url = format!("http://127.0.0.1:{port}/health");
    let client = reqwest::Client::builder()
        .timeout(HEALTHCHECK_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| RuntimeError::Healthcheck("HTTP client unavailable".to_owned()))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| RuntimeError::Healthcheck("server is unreachable".to_owned()))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(RuntimeError::Healthcheck(format!(
            "/health returned {}",
            response.status().as_u16()
        )));
    }
    let report = response
        .json::<HealthReport>()
        .await
        .map_err(|_| RuntimeError::Healthcheck("invalid /health response".to_owned()))?;
    if report != HealthReport::ok() {
        return Err(RuntimeError::Healthcheck(
            "/health did not report ok".to_owned(),
        ));
    }
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("artifact_mcp=info,tower_http=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .try_init();
}

#[cfg(unix)]
const USAGE: &str = artifact_mcp::ops::cli::USAGE;
#[cfg(not(unix))]
const USAGE: &str = "usage: artifact-mcp [healthcheck | check-action-grants FILE]";

fn is_healthcheck(args: &[String]) -> Result<bool, RuntimeError> {
    match args {
        [] => Ok(false),
        [command] if command == "healthcheck" => Ok(true),
        _ => Err(RuntimeError::Healthcheck(USAGE.to_owned())),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Operator subcommands (backup, verify-backup, web-push-keys) print plain text for a person
    // or a cron job, so they run before the JSON log subscriber is installed. Paths may be
    // non-UTF-8, so they see the raw arguments.
    #[cfg(unix)]
    {
        let raw_args = std::env::args_os().skip(1).collect::<Vec<_>>();
        if let Some(code) = artifact_mcp::ops::cli::dispatch(&raw_args).await {
            return code;
        }
    }
    init_tracing();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    // Validate the exact reviewed grants before a deployment stops the live server.
    // This path does not load application config, open storage, or contact a worker.
    if let [command, path] = args.as_slice()
        && command == "check-action-grants"
    {
        return match artifact_mcp::actions::load_grants(Some(path)) {
            Ok(_) => {
                println!("{{\"status\":\"ok\",\"check\":\"action-grants\"}}");
                ExitCode::SUCCESS
            }
            Err(_) => {
                eprintln!("Invalid action grants");
                ExitCode::FAILURE
            }
        };
    }
    let result = match is_healthcheck(&args) {
        Ok(true) => healthcheck().await,
        Ok(false) => match AppConfig::from_env() {
            Ok(config) if config.audit_ledger_hmac_key.is_none() => Err(RuntimeError::Application(
                AppError::Validation("AUDIT_LEDGER_HMAC_KEY is required; refusing to start without a tamper-evident audit ledger".to_owned()),
            )),
            Ok(config) => serve(config).await,
            Err(error) => Err(RuntimeError::from(error)),
        },
        Err(error) => Err(error),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "artifact-mcp stopped with an error");
            ExitCode::FAILURE
        }
    }
}
