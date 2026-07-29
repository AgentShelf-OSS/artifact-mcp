//! U12 test support: throwaway webhook databases, scripted transports, and raw TCP servers.
//!
//! Everything here is shared by `u12_webhooks.rs`, `u12_notify.rs`, and `u12_node_parity.rs`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use artifact_mcp::config::{IdSource, Secret, SequentialIdSource};
use artifact_mcp::error::AppError;
use artifact_mcp::integrations::notify::{
    DeliveryRequest, DeliveryResponse, DiscordNotifier, WebhookTransport,
};
use artifact_mcp::model::{ArtifactId, EmailAddress, NotificationPayload};
use artifact_mcp::persistence::db::{self, DbPool};
use artifact_mcp::persistence::webhooks::WebhookStore;
use artifact_mcp::ports::BoxFuture;
use artifact_mcp::security::crypto::WebhookUrlProtection;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temporary data directory removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u12-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp data directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The canonical 32-byte parity key (standard Base64 of the bytes `0..32`), shared with U04.
pub fn test_key() -> String {
    BASE64.encode((0..32_u8).collect::<Vec<u8>>())
}

/// A pool over a fresh migrated database in `dir`.
pub fn open_pool(dir: &Path) -> DbPool {
    artifact_mcp::persistence::db::Database::open_at(dir).expect("bootstrap database")
}

/// Insert an organization so `create()` passes its `orgExists` check.
pub async fn seed_org(pool: &DbPool, org: &str) {
    let org = org.to_owned();
    db::interact(pool, move |conn| {
        conn.execute(
            "INSERT OR IGNORE INTO orgs (name, label) VALUES (?1, ?2)",
            (&org, "Test Org"),
        )
        .map_err(|_| AppError::Internal)?;
        Ok(())
    })
    .await
    .expect("seed org");
}

/// A store bound to `pool`, with deterministic ids and the requested protection mode.
pub fn store_with(pool: DbPool, key: Option<&str>) -> Arc<WebhookStore> {
    let protection = match key {
        None => WebhookUrlProtection::Plaintext,
        Some(value) => WebhookUrlProtection::from_config_key(Some(&Secret::new(value.to_owned())))
            .expect("valid key"),
    };
    let ids: Arc<dyn IdSource> = Arc::new(SequentialIdSource::starting_at(
        COUNTER.fetch_add(1_000, Ordering::Relaxed),
    ));
    Arc::new(WebhookStore::new(pool, ids, Arc::new(protection)))
}

/// A migrated database plus a store, in the requested protection mode.
pub async fn fixture(
    label: &str,
    org: &str,
    key: Option<&str>,
) -> (TempDir, DbPool, Arc<WebhookStore>) {
    let dir = TempDir::new(label);
    let pool = open_pool(dir.path());
    seed_org(&pool, org).await;
    let store = store_with(pool.clone(), key);
    (dir, pool, store)
}

/// The raw at-rest columns of one row: `(url, url_cipher, url_nonce, url_tag)`.
pub async fn raw_url_columns(
    pool: &DbPool,
    id: &str,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let id = id.to_owned();
    db::interact(pool, move |conn| {
        conn.query_row(
            "SELECT url, url_cipher, url_nonce, url_tag FROM org_webhooks WHERE id = ?1",
            (&id,),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|_| AppError::Internal)
    })
    .await
    .expect("read raw columns")
}

/// A representative artifact-event payload.
pub fn payload() -> NotificationPayload {
    NotificationPayload {
        artifact_id: ArtifactId("abc123def456".to_owned()),
        title: "Quarterly report".to_owned(),
        url: "https://example.test/abc123def456".to_owned(),
        description: "The numbers are in.".to_owned(),
        uploader_label: "Ada Lovelace".to_owned(),
        category: "Reports".to_owned(),
        revision: 3,
        bytes: 2048,
        viewer_email: Some(EmailAddress("viewer@example.test".to_owned())),
        body: Some("Looks good to me".to_owned()),
        resolver: Some("Grace Hopper".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Scripted transport
// ---------------------------------------------------------------------------

/// What a [`RecordingTransport`] should do with the next request.
#[derive(Clone, Debug)]
pub enum Behaviour {
    /// Answer with this status code.
    Status(u16),
    /// Answer after a delay, so detachment is observable.
    SlowStatus(Duration, u16),
    /// Fail with this message, as a network error would.
    Failure(String),
}

/// A `fetchImpl` stand-in that records every call instead of touching the network.
pub struct RecordingTransport {
    behaviour: Behaviour,
    calls: std::sync::Mutex<Vec<(String, DeliveryRequest)>>,
    started: AtomicUsize,
}

impl RecordingTransport {
    pub fn new(behaviour: Behaviour) -> Arc<Self> {
        Arc::new(Self {
            behaviour,
            calls: std::sync::Mutex::new(Vec::new()),
            started: AtomicUsize::new(0),
        })
    }

    pub fn ok() -> Arc<Self> {
        Self::new(Behaviour::Status(204))
    }

    /// Every `(url, request)` pair the notifier attempted.
    pub fn calls(&self) -> Vec<(String, DeliveryRequest)> {
        self.calls.lock().expect("transport lock").clone()
    }

    /// Number of requests that reached the transport, including ones still in flight.
    pub fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for RecordingTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordingTransport")
            .field("behaviour", &self.behaviour)
            .finish_non_exhaustive()
    }
}

impl WebhookTransport for RecordingTransport {
    fn post<'a>(
        &'a self,
        url: &'a str,
        request: DeliveryRequest,
    ) -> BoxFuture<'a, Result<DeliveryResponse, String>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.calls
                .lock()
                .expect("transport lock")
                .push((url.to_owned(), request));
            match self.behaviour.clone() {
                Behaviour::Status(status) => Ok(DeliveryResponse { status }),
                Behaviour::SlowStatus(delay, status) => {
                    tokio::time::sleep(delay).await;
                    Ok(DeliveryResponse { status })
                }
                Behaviour::Failure(message) => Err(message),
            }
        })
    }
}

/// A notifier over a store and a scripted transport.
pub fn notifier(store: &Arc<WebhookStore>, transport: Arc<RecordingTransport>) -> DiscordNotifier {
    DiscordNotifier::new(Arc::clone(store), transport)
}

// ---------------------------------------------------------------------------
// Raw TCP server
// ---------------------------------------------------------------------------

/// How a [`spawn_server`] connection is answered.
#[derive(Clone, Copy, Debug)]
pub enum ServerBehaviour {
    /// Read the request and never write a byte, so only a timeout ends the call.
    Hang,
    /// Reply with a raw HTTP response.
    Respond(&'static str),
}

/// A local HTTP-ish server used to exercise the real [`HttpTransport`] policies.
///
/// [`HttpTransport`]: artifact_mcp::integrations::notify::HttpTransport
pub struct TestServer {
    /// Address to POST to.
    pub addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

impl TestServer {
    /// Number of TCP connections accepted so far — a followed redirect would show up as two.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Bind a local listener that answers every connection with `behaviour`.
pub async fn spawn_server(behaviour: ServerBehaviour) -> TestServer {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut scratch = [0_u8; 4096];
                // One read is enough to know the request arrived; the bodies here are small.
                let _ = socket.read(&mut scratch).await;
                match behaviour {
                    ServerBehaviour::Hang => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    ServerBehaviour::Respond(response) => {
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.flush().await;
                    }
                }
            });
        }
    });
    TestServer { addr, connections }
}
