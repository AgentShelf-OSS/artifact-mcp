//! Production-wiring regression for application-level organization offboarding.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::config::{AppConfig, Secret, SeedKeys};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::u20_runtime::runtime;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u24-org-delete-{}-{sequence}",
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
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

struct Observer;

impl runtime::StartupObserver for Observer {
    fn stage(&self, _stage: runtime::StartupStage) {}
}

fn config_for(data_dir: &Path) -> AppConfig {
    let mut config = AppConfig {
        data_dir: data_dir.to_path_buf(),
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse(
            "administrator:admin:admin-secret,owner:offboard:offboard-secret",
        ),
        ..AppConfig::defaults()
    };
    config.access.trust_headers = true;
    config.access.header_trust_allow_insecure = true;
    config.access.admin_emails = BTreeSet::from(["admin@example.test".to_owned()]);
    config
}

async fn json_body(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("response is JSON")
}

async fn mcp(
    router: &Router,
    key: &str,
    request_id: u64,
    name: &str,
    arguments: Value,
) -> Response {
    router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": "tools/call",
                        "params": { "name": name, "arguments": arguments }
                    })
                    .to_string(),
                ))
                .expect("MCP request"),
        )
        .await
        .expect("MCP response")
}

async fn delete_org(router: &Router) -> Response {
    router
        .clone()
        .oneshot(
            Request::delete("/settings/orgs/offboard")
                .header("cf-access-authenticated-user-email", "admin@example.test")
                .header("x-artifact-mutation", "1")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .expect("delete-org request"),
        )
        .await
        .expect("delete-org response")
}

#[tokio::test]
async fn deleted_org_key_cannot_authenticate_or_publish_against_the_real_database() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let created = router
                .clone()
                .oneshot(
                    Request::post("/settings/orgs")
                        .header("cf-access-authenticated-user-email", "admin@example.test")
                        .header("x-artifact-mutation", "1")
                        .header("sec-fetch-site", "same-origin")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({ "name": "offboard" }).to_string()))
                        .expect("create-org request"),
                )
                .await
                .expect("create-org response");
            assert_eq!(created.status(), StatusCode::OK);

            let published = mcp(
                &router,
                "offboard-secret",
                1,
                "publish_artifact",
                json!({ "html": "<h1>Retain me</h1>" }),
            )
            .await;
            assert_eq!(published.status(), StatusCode::OK);
            let published = json_body(published).await;
            let id = published["result"]["structuredContent"]["id"]
                .as_str()
                .expect("artifact id")
                .to_owned();

            let refused = delete_org(&router).await;
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                json_body(refused).await["error"],
                "Cannot delete organization \"offboard\" while it owns 1 artifact. Move its artifacts to another organization first."
            );

            let removed_artifact = mcp(
                &router,
                "offboard-secret",
                2,
                "delete_artifact",
                json!({ "id": id }),
            )
            .await;
            assert_eq!(removed_artifact.status(), StatusCode::OK);
            assert_eq!(
                json_body(removed_artifact).await["result"]["structuredContent"]["deleted"],
                true
            );

            let deleted = delete_org(&router).await;
            assert_eq!(deleted.status(), StatusCode::OK);
            assert_eq!(json_body(deleted).await["removed"], true);

            let rejected = mcp(
                &router,
                "offboard-secret",
                3,
                "publish_artifact",
                json!({ "html": "<h1>Must fail</h1>" }),
            )
            .await;
            assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(json_body(rejected).await["error"]["message"], "unauthorized");
            Ok(())
        },
    )
    .await
    .expect("bootstrap with fake listener");

    let conn = Connection::open(temp.path().join("artifacts.db")).expect("open real database");
    let revoked_at: Option<String> = conn
        .query_row(
            "SELECT revoked_at FROM api_keys WHERE client_id = 'owner'",
            [],
            |row| row.get(0),
        )
        .expect("read revoked key");
    assert!(revoked_at.is_some());
    let org_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM orgs WHERE name = 'offboard'",
            [],
            |row| row.get(0),
        )
        .expect("count orgs");
    assert_eq!(org_count, 0);
}
