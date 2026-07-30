use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::{Router, routing::get};

use artifact_mcp::{
    config::{AppConfig, Secret, SeedKeys},
    integrations::thumbnails::{PreviewArtifactRef, PreviewIntegration},
    model::{ArtifactId, ArtifactMeta, ClientId, OrgId, Timestamp},
    persistence::{
        db::{self, Database, PINNED_PRAGMAS},
        migrations::MigrationContext,
    },
    ports::{HealthProbe, PreviewService, integrations::PreviewPriority},
    security::audit::{
        AuditContext, AuditEvent, append_in_transaction, reserve_receipt_in_transaction,
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::{Notify, oneshot},
    time::{Duration, Instant, timeout},
};

#[allow(dead_code)]
#[path = "../../src/main.rs"]
pub(super) mod runtime;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u20-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp data directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<runtime::StartupStage>>);

impl runtime::StartupObserver for Recorder {
    fn stage(&self, stage: runtime::StartupStage) {
        self.0.lock().expect("startup recorder lock").push(stage);
    }
}

fn config_for(data_dir: &Path) -> AppConfig {
    AppConfig {
        data_dir: data_dir.to_path_buf(),
        // Production bootstraps must fail closed without a ledger key.  Keep this fixture
        // explicit so runtime tests exercise the same startup path as a real deployment.
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        listen_host: "invalid.invalid".to_owned(),
        port: 34_981,
        public_base_url: "http://127.0.0.1:34981".to_owned(),
        ..AppConfig::defaults()
    }
}

async fn bootstrap_ledger(config: AppConfig) -> Result<(), runtime::RuntimeError> {
    runtime::run_with_bind(
        config,
        Arc::new(Recorder::default()),
        |_host, _port, _router| async { Ok(()) },
    )
    .await
}

#[tokio::test]
async fn startup_requires_a_ledger_key_and_accepts_a_clean_initialized_ledger() {
    let temp = TempDir::new("audit-key-required");
    let mut missing = config_for(temp.path());
    missing.audit_ledger_hmac_key = None;
    let error = bootstrap_ledger(missing)
        .await
        .expect_err("a production bootstrap must not silently disable audit integrity");
    assert!(error.to_string().contains("AUDIT_LEDGER_HMAC_KEY"));

    bootstrap_ledger(config_for(temp.path()))
        .await
        .expect("a clean ledger seals and starts with the configured key");
}

#[tokio::test]
async fn startup_rejects_a_wrong_ledger_key_or_tampered_head() {
    let temp = TempDir::new("audit-head-integrity");
    bootstrap_ledger(config_for(temp.path()))
        .await
        .expect("initialize ledger with fixture key");

    let mut wrong_key = config_for(temp.path());
    wrong_key.audit_ledger_hmac_key =
        Some(Secret::new("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="));
    assert!(
        bootstrap_ledger(wrong_key).await.is_err(),
        "wrong key must fail closed"
    );

    {
        let pool = Database::open_at(temp.path()).expect("open fixture database");
        let conn = db::checkout(&pool).expect("checkout fixture database");
        conn.execute(
            "UPDATE security_audit_chain_head SET head_mac='tampered' WHERE singleton=1",
            [],
        )
        .expect("tamper head for startup rejection proof");
    }
    assert!(
        bootstrap_ledger(config_for(temp.path())).await.is_err(),
        "a changed chain head must fail before the listener is bound"
    );
}

#[tokio::test]
async fn startup_rejects_a_tampered_persisted_audit_event() {
    let temp = TempDir::new("audit-event-integrity");
    bootstrap_ledger(config_for(temp.path()))
        .await
        .expect("initialize ledger with fixture key");
    {
        let pool = Database::open_at(temp.path()).expect("open fixture database");
        let mut conn = db::checkout(&pool).expect("checkout fixture database");
        let transaction = conn.transaction().expect("open audit fixture transaction");
        append_in_transaction(
            &transaction,
            &[0; 32],
            "audit-event-1",
            &AuditContext {
                tenant: "acme".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: "artifact-mcp".to_owned(),
                actor_role: String::new(),
                source: "maintenance".to_owned(),
                request_id: "fixture-1".to_owned(),
            },
            &AuditEvent {
                operation: "artifact.publish".to_owned(),
                target_type: "artifact".to_owned(),
                target_id: "abc123def456".to_owned(),
                result: "success".to_owned(),
                classification: String::new(),
                revision: Some(1),
            },
        )
        .expect("append valid fixture event");
        transaction.commit().expect("commit fixture event");
        conn.execute(
            "UPDATE security_audit_events SET target_id='mutated' WHERE event_id='audit-event-1'",
            [],
        )
        .expect("tamper immutable event projection");
    }

    assert!(
        bootstrap_ledger(config_for(temp.path())).await.is_err(),
        "the event canonical bytes and duplicated columns must be verified at startup"
    );
}

#[tokio::test]
async fn startup_rejects_a_deleted_pending_receipt_and_preserves_its_intent() {
    let temp = TempDir::new("audit-missing-pending-receipt");
    bootstrap_ledger(config_for(temp.path()))
        .await
        .expect("initialize ledger with fixture key");
    {
        let pool = Database::open_at(temp.path()).expect("open fixture database");
        let mut conn = db::checkout(&pool).expect("checkout fixture database");
        let transaction = conn
            .transaction()
            .expect("open receipt fixture transaction");
        transaction
            .execute(
                "INSERT INTO artifact_durability_intents \
                 (id,artifact_id,operation,state,expected_sha256,prior_sha256,staging_path) \
                 VALUES ('publish:abc123','abc123','publish','metadata_committed','','','')",
                [],
            )
            .expect("insert durability intent");
        reserve_receipt_in_transaction(
            &transaction,
            &[0; 32],
            "audit:artifact.publish:abc123:1:fixture",
            "publish:abc123",
            &AuditContext {
                tenant: "acme".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: "artifact-mcp".to_owned(),
                actor_role: String::new(),
                source: "maintenance".to_owned(),
                request_id: "fixture".to_owned(),
            },
            &AuditEvent {
                operation: "artifact.publish".to_owned(),
                target_type: "artifact".to_owned(),
                target_id: "abc123".to_owned(),
                result: "success".to_owned(),
                classification: String::new(),
                revision: Some(1),
            },
        )
        .expect("reserve authenticated pending receipt");
        transaction.commit().expect("commit receipt fixture");
        conn.execute(
            "DELETE FROM security_audit_receipts WHERE durability_intent_id='publish:abc123'",
            [],
        )
        .expect("simulate receipt deletion");
    }

    assert!(
        bootstrap_ledger(config_for(temp.path())).await.is_err(),
        "restart must fail before reconciliation can treat the missing receipt as legacy"
    );
    let pool = Database::open_at(temp.path()).expect("reopen fixture database");
    let conn = db::checkout(&pool).expect("checkout fixture database");
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM artifact_durability_intents WHERE id='publish:abc123'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count preserved intent"),
        1,
        "failed startup must leave the concealed intent for operator recovery"
    );
}

#[tokio::test]
async fn reconciliation_completes_before_delivery_workers_and_listener_binding() {
    let temp = TempDir::new("ordering");
    let artifact_dir = temp.path().join("artifacts");
    std::fs::create_dir_all(&artifact_dir).expect("artifact directory");
    let transient = artifact_dir.join(".abc123def456.staging-u20");
    std::fs::write(&transient, b"interrupted publish").expect("transient body");

    let recorder = Arc::new(Recorder::default());
    let observer: Arc<dyn runtime::StartupObserver> = recorder.clone();
    runtime::run_with_bind(
        config_for(temp.path()),
        observer,
        |_host, _port, _router| async { Ok(()) },
    )
    .await
    .expect("bootstrap with fake listener");

    assert!(
        !transient.exists(),
        "reconciliation must clean the transient"
    );
    assert_eq!(
        *recorder.0.lock().expect("startup stages"),
        [
            runtime::StartupStage::DatabaseReady,
            runtime::StartupStage::StorageReconciled,
            runtime::StartupStage::DeliveryWorkersStarted,
            runtime::StartupStage::ListenerBindRequested,
        ]
    );
}

#[tokio::test]
async fn listener_reclaims_a_slow_header_connection_for_a_later_client() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let mut ingress = AppConfig::default().ingress;
    ingress.max_connections = 1;
    ingress.read_timeout_ms = 20;
    let (shutdown_send, shutdown_receive) = oneshot::channel();
    let server = tokio::spawn(runtime::serve_listener_with_shutdown(
        listener,
        Router::new().route("/", get(|| async { "ok" })),
        ingress,
        async move {
            let _ = shutdown_receive.await;
        },
    ));

    let mut slow = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect slow client");
    slow.write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\n")
        .await
        .expect("send incomplete headers");

    let mut excess = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect excess client");
    excess
        .write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("write excess request");
    let mut closed = [0_u8; 1];
    assert!(matches!(
        timeout(Duration::from_millis(200), excess.read(&mut closed)).await,
        Ok(Ok(0)) | Ok(Err(_))
    ));

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut healthy = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect after slowloris timeout");
    healthy
        .write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("send complete request");
    let mut response = Vec::new();
    timeout(Duration::from_secs(1), healthy.read_to_end(&mut response))
        .await
        .expect("healthy request completed")
        .expect("read healthy response");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "expected a response after slow connection eviction: {response:?}"
    );
    let _ = shutdown_send.send(());
    timeout(Duration::from_secs(1), server)
        .await
        .expect("listener shutdown")
        .expect("listener task")
        .expect("listener result");
}

#[tokio::test]
async fn mcp_tcp_drain_preserves_oversize_and_parse_error_wire_contracts() {
    let temp = TempDir::new("mcp-tcp-drain");
    let mut config = config_for(temp.path());
    config.seed_keys = SeedKeys::parse("publisher:acme:owner-secret");
    config.body.mcp_json = 128;
    config.ingress.read_timeout_ms = 1_000;
    let ingress = config.ingress.clone();

    runtime::run_with_bind(config, Arc::new(Recorder::default()), move |_host, _port, router| async move {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind MCP listener");
        let address = listener.local_addr().expect("MCP listener address");
        let (shutdown_send, shutdown_receive) = oneshot::channel();
        let server = tokio::spawn(runtime::serve_listener_with_shutdown(
            listener,
            router,
            ingress,
            async move {
                let _ = shutdown_receive.await;
            },
        ));

        // Write the first over-limit frame, then continue slowly. The old admission preflight
        // returned before the remaining upload was read, making one of these writes fail with
        // EPIPE. The production listener must instead discard through the bounded deadline.
        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect oversized MCP client");
        let oversized = vec![b'x'; 256 * 1024];
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: example.test\r\nAuthorization: Bearer owner-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            oversized.len()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write oversized MCP headers");
        client
            .write_all(&oversized[..129])
            .await
            .expect("write first over-limit MCP frame");
        tokio::time::sleep(Duration::from_millis(20)).await;
        for chunk in oversized[129..].chunks(4_096) {
            client
                .write_all(chunk)
                .await
                .expect("drained oversized MCP upload must not EPIPE");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .expect("oversized MCP response deadline")
            .expect("read oversized MCP response");
        assert!(
            response.starts_with(b"HTTP/1.1 413"),
            "oversized MCP response: {response:?}"
        );
        let body = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| &response[index + 4..])
            .expect("oversized MCP response body");
        assert_eq!(body, br#"{"error":"payload too large"}"#);

        let mut malformed = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect malformed MCP client");
        let invalid_json = b"{ this is not json";
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: example.test\r\nAuthorization: Bearer owner-secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            invalid_json.len()
        );
        malformed
            .write_all(request.as_bytes())
            .await
            .expect("write malformed MCP headers");
        malformed
            .write_all(invalid_json)
            .await
            .expect("write malformed MCP body");
        let mut response = Vec::new();
        timeout(Duration::from_secs(1), malformed.read_to_end(&mut response))
            .await
            .expect("malformed MCP response deadline")
            .expect("read malformed MCP response");
        assert!(
            response.starts_with(b"HTTP/1.1 400"),
            "malformed MCP response: {response:?}"
        );
        let body = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| &response[index + 4..])
            .expect("malformed MCP response body");
        assert_eq!(body, br#"{"error":"invalid JSON"}"#);

        let _ = shutdown_send.send(());
        timeout(Duration::from_secs(1), server)
            .await
            .expect("MCP listener shutdown")
            .expect("MCP listener task")
            .expect("MCP listener result");
        Ok(())
    })
    .await
    .expect("bootstrap production MCP router");
}

#[tokio::test]
async fn shutdown_drains_an_in_flight_handler_that_completes_within_grace() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let router = Router::new().route(
        "/drain",
        get({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move || {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    entered.notify_one();
                    release.notified().await;
                    "drained"
                }
            }
        }),
    );
    let mut ingress = AppConfig::default().ingress;
    ingress.shutdown_grace_ms = 200;
    let (shutdown_send, shutdown_receive) = oneshot::channel();
    let server = tokio::spawn(runtime::serve_listener_with_shutdown(
        listener,
        router,
        ingress,
        async move {
            let _ = shutdown_receive.await;
        },
    ));

    let mut client = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect client");
    client
        .write_all(b"GET /drain HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("start request");
    entered.notified().await;

    let _ = shutdown_send.send(());
    release.notify_one();
    let mut response = Vec::new();
    timeout(Duration::from_secs(1), client.read_to_end(&mut response))
        .await
        .expect("drained response completes")
        .expect("read drained response");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "response: {response:?}"
    );
    assert!(response.ends_with(b"drained"), "response: {response:?}");
    timeout(Duration::from_secs(1), server)
        .await
        .expect("listener drains within grace")
        .expect("listener task")
        .expect("listener result");
}

#[tokio::test]
async fn shutdown_aborts_a_non_completing_connection_after_its_grace_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let entered = Arc::new(Notify::new());
    let router = Router::new().route(
        "/stuck",
        get({
            let entered = Arc::clone(&entered);
            move || {
                let entered = Arc::clone(&entered);
                async move {
                    entered.notify_one();
                    std::future::pending::<()>().await;
                    "unreachable"
                }
            }
        }),
    );
    let mut ingress = AppConfig::default().ingress;
    ingress.shutdown_grace_ms = 25;
    let (shutdown_send, shutdown_receive) = oneshot::channel();
    let server = tokio::spawn(runtime::serve_listener_with_shutdown(
        listener,
        router,
        ingress,
        async move {
            let _ = shutdown_receive.await;
        },
    ));

    let mut client = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect client");
    client
        .write_all(b"GET /stuck HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("start request");
    entered.notified().await;

    let started = Instant::now();
    let _ = shutdown_send.send(());
    timeout(Duration::from_millis(500), server)
        .await
        .expect("listener enforces shutdown grace")
        .expect("listener task")
        .expect("listener result");
    assert!(
        started.elapsed() >= Duration::from_millis(20),
        "the listener must give the connection its configured grace"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the listener must abort work after the bounded grace"
    );

    let mut byte = [0_u8; 1];
    assert!(matches!(
        timeout(Duration::from_millis(200), client.read(&mut byte)).await,
        Ok(Ok(0)) | Ok(Err(_))
    ));
    assert!(
        tokio::net::TcpStream::connect(address).await.is_err(),
        "shutdown drops the listener before remaining connection tasks are joined"
    );
}

#[test]
fn bootstrap_and_every_pooled_connection_verify_the_pinned_pragmas() {
    let temp = TempDir::new("pragmas");
    let config = config_for(temp.path());
    let pool = Database::open_with(&config.data_dir, &MigrationContext::empty(), None)
        .expect("database bootstrap");

    let bootstrap = db::open_bootstrap_connection(&config.database_path())
        .expect("second bootstrap connection");
    db::verify_pragmas(&bootstrap).expect("bootstrap pragmas");
    drop(bootstrap);

    let connections = (0..4)
        .map(|_| pool.get().expect("pooled connection"))
        .collect::<Vec<_>>();
    assert_eq!(connections.len(), 4);
    assert_eq!(PINNED_PRAGMAS.len(), 6);
    for connection in &connections {
        db::verify_pragmas(connection).expect("pooled pragmas");
    }
}

#[tokio::test]
async fn production_health_checks_sqlite_and_directory_and_keeps_node_shape() {
    let temp = TempDir::new("health");
    let config = config_for(temp.path());
    let pool = Database::open_with(&config.data_dir, &MigrationContext::empty(), None)
        .expect("database bootstrap");
    let probe = runtime::ProductionHealth::new(pool, config.artifact_dir());

    let report = probe
        .check()
        .await
        .expect("healthy production dependencies");
    assert_eq!(
        serde_json::to_vec(&report).expect("serialize health"),
        br#"{"status":"ok"}"#
    );

    std::fs::remove_dir(config.artifact_dir()).expect("remove empty artifact directory");
    assert!(
        probe.check().await.is_err(),
        "an unreadable/unwritable artifact directory must make health fail"
    );
    assert_eq!(
        serde_json::to_vec(&artifact_mcp::ports::integrations::HealthReport::error())
            .expect("serialize failed health"),
        br#"{"status":"error"}"#
    );
}

#[tokio::test]
async fn disabled_preview_renderer_is_a_clean_startup_and_render_noop() {
    let temp = TempDir::new("preview-disabled");
    let config = config_for(temp.path());
    let previews = PreviewIntegration::from_config(&config);
    assert!(!previews.enabled());

    let index = HashMap::<String, PreviewArtifactRef>::new();
    let audit = previews.store().audit(&index).await;
    assert!(audit.is_empty());

    let meta = ArtifactMeta {
        id: ArtifactId("abc123def456".to_owned()),
        client_id: ClientId("publisher".to_owned()),
        org: OrgId("acme".to_owned()),
        title: "No renderer".to_owned(),
        description: String::new(),
        bytes: 13,
        created_at: Timestamp("2026-07-21 00:00:00".to_owned()),
        updated_at: Timestamp("2026-07-21 00:00:00".to_owned()),
        uploader_label: String::new(),
        owner_email: None,
        is_bundle: false,
        entry: String::new(),
        revision: 1,
        category: String::new(),
        hidden: false,
        body_sha256: "0".repeat(64),
    };
    assert_eq!(
        previews
            .ensure_thumbnail(&meta, "<h1>noop</h1>", PreviewPriority::Low)
            .await
            .expect("disabled renderer remains optional"),
        None
    );
}
