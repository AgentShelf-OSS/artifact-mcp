use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use artifact_mcp::{
    config::AppConfig,
    integrations::thumbnails::{PreviewArtifactRef, PreviewIntegration},
    model::{ArtifactId, ArtifactMeta, ClientId, OrgId, Timestamp},
    persistence::{
        db::{self, Database, PINNED_PRAGMAS},
        migrations::MigrationContext,
    },
    ports::{HealthProbe, PreviewService, integrations::PreviewPriority},
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
        listen_host: "invalid.invalid".to_owned(),
        port: 34_981,
        public_base_url: "http://127.0.0.1:34981".to_owned(),
        ..AppConfig::defaults()
    }
}

#[tokio::test]
async fn reconciliation_completes_before_listener_binding_is_requested() {
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
            runtime::StartupStage::ListenerBindRequested,
        ]
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
