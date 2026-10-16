//! ADR-0015: publish upsert by per-organization `artifact_key`, on production adapters.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::{
    artifacts::lifecycle::ARTIFACT_KEY_TAKEN,
    config::{AppConfig, Secret, SeedKeys},
    error::AppError,
    mcp::dispatch::{ARTIFACT_KEY_INVALID, is_valid_artifact_key},
    model::{ArtifactContent, OrgId},
    ports::ArtifactService as _,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::u08_support::{Fixture, TEST_ORG, mutation_audit, publish_request};
use super::u20_runtime::runtime;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u91-key-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create u91 temp directory");
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
    let defaults = AppConfig::defaults();
    // The race proof issues many publishes from one key; keep the ingress limiter out of it.
    let mut ingress = defaults.ingress.clone();
    ingress.uploads_per_window = 10_000;
    ingress.mcp_per_window = 10_000;
    AppConfig {
        ingress,
        data_dir: data_dir.to_path_buf(),
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse(
            "publisher:acme:owner-secret,second:acme:second-secret,foreign:other:foreign-secret",
        ),
        ..defaults
    }
}

async fn call(router: &Router, key: &str, id: u64, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let response = router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("MCP request"),
        )
        .await
        .expect("MCP response");
    assert_eq!(response.status(), 200);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read MCP response");
    serde_json::from_slice(&bytes).expect("MCP JSON response")
}

fn ok(response: &Value) -> &Value {
    assert!(
        response.get("error").is_none(),
        "unexpected error: {response}"
    );
    assert_ne!(
        response["result"]["isError"], true,
        "tool error: {response}"
    );
    &response["result"]["structuredContent"]
}

fn tool_error(response: &Value) -> &str {
    assert_eq!(
        response["result"]["isError"], true,
        "expected tool error: {response}"
    );
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool error text")
}

fn keyed_rows(data_dir: &Path, org: &str, key: &str) -> i64 {
    let conn = rusqlite::Connection::open(data_dir.join("artifacts.db")).expect("open database");
    conn.query_row(
        "SELECT count(*) FROM artifacts WHERE org = ?1 AND artifact_key = ?2",
        rusqlite::params![org, key],
        |row| row.get(0),
    )
    .expect("count keyed rows")
}

#[test]
fn artifact_key_grammar_matches_the_node_pattern() {
    for valid in [
        "a",
        "0",
        "proj/weekly-report",
        "a.b_c-d/e",
        &"x".repeat(128),
    ] {
        assert!(is_valid_artifact_key(valid), "{valid}");
    }
    for invalid in [
        "",
        "A",
        "-lead",
        ".lead",
        "/lead",
        "_lead",
        "has space",
        "Proj",
        &"x".repeat(129),
        "tab\t",
        "nl\n",
        "é",
        "a\\b",
        "a:b",
    ] {
        assert!(!is_valid_artifact_key(invalid), "{invalid:?}");
    }
}

#[tokio::test]
async fn store_records_resolves_and_guards_artifact_keys() {
    let fixture = Fixture::new("u91-store-keys");
    let mut request = publish_request(ArtifactContent::SingleHtml("<p>a</p>".to_owned()));
    request.artifact_key = Some("unit/race".to_owned());
    let first = fixture
        .try_publish_request(request.clone())
        .await
        .expect("keyed publish")
        .meta;
    assert_eq!(first.artifact_key.as_deref(), Some("unit/race"));
    let org = OrgId(TEST_ORG.to_owned());
    let found = fixture
        .store
        .find_meta_by_key(&org, "unit/race")
        .await
        .expect("lookup")
        .expect("key resolves");
    assert_eq!(found.id, first.id);
    assert!(
        fixture
            .store
            .find_meta_by_key(&OrgId("other".to_owned()), "unit/race")
            .await
            .expect("lookup")
            .is_none(),
        "keys are per organization"
    );

    // The unique index refuses a second live row, compensating staging and the intent.
    request.content = ArtifactContent::SingleHtml("<p>b</p>".to_owned());
    let refused = fixture
        .try_publish_request(request)
        .await
        .expect_err("duplicate key is refused");
    assert_eq!(refused, AppError::Conflict(ARTIFACT_KEY_TAKEN.to_owned()));
    assert_eq!(
        fixture.count("SELECT count(*) FROM artifacts WHERE artifact_key = 'unit/race'"),
        1
    );
    assert_eq!(
        fixture.count("SELECT count(*) FROM artifact_durability_intents"),
        0
    );
    assert!(fixture.staging_entries().is_empty());

    // A re-tenant clears the key so it cannot collide in the destination namespace.
    fixture.create_org("other");
    let mut destination = publish_request(ArtifactContent::SingleHtml("<p>d</p>".to_owned()));
    destination.target_org = OrgId("other".to_owned());
    destination.artifact_key = Some("unit/race".to_owned());
    fixture
        .try_publish_request(destination)
        .await
        .expect("same key in another org");
    let moved = fixture
        .store
        .move_to_org_for(&first, "other", None, mutation_audit())
        .await
        .expect("move succeeds");
    assert_eq!(moved.org.0, "other");
    assert_eq!(moved.artifact_key, None);
    assert_eq!(
        fixture.count(
            "SELECT count(*) FROM artifacts WHERE org = 'other' AND artifact_key = 'unit/race'"
        ),
        1
    );
}

#[tokio::test]
async fn keyed_publish_creates_updates_and_frees_keys_over_mcp() {
    let temp = TempDir::new();
    let data_dir = temp.path().to_path_buf();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let created = call(
                &router,
                "owner-secret",
                1,
                "publish_artifact",
                json!({
                    "html": "<p>one</p>",
                    "title": "Report",
                    "description": "First",
                    "category": "Reports",
                    "artifact_key": "unit/report"
                }),
            )
            .await;
            let created = ok(&created).clone();
            assert_eq!(created["created"], true);
            assert_eq!(created["revision"], 1);
            assert_eq!(created["artifact_key"], "unit/report");
            let id = created["id"].as_str().expect("id").to_owned();

            let updated = call(
                &router,
                "owner-secret",
                2,
                "publish_artifact",
                json!({ "html": "<p>two</p>", "artifact_key": "unit/report" }),
            )
            .await;
            let updated = ok(&updated);
            assert_eq!(updated["created"], false);
            assert_eq!(updated["id"], id.as_str());
            assert_eq!(updated["url"], created["url"]);
            assert_eq!(updated["revision"], 2);
            assert_eq!(updated["category"], "Reports");

            let listed = call(&router, "owner-secret", 3, "list_artifacts", json!({})).await;
            let row = ok(&listed)["artifacts"]
                .as_array()
                .expect("rows")
                .iter()
                .find(|row| row["id"] == id.as_str())
                .expect("keyed row listed")
                .clone();
            assert_eq!(row["title"], "Report");
            assert_eq!(row["description"], "First");
            assert_eq!(row["artifact_key"], "unit/report");
            let read = call(&router, "owner-secret", 4, "read_artifact", json!({ "id": id })).await;
            assert_eq!(ok(&read)["content"], "<p>two</p>");
            assert_eq!(ok(&read)["artifact_key"], "unit/report");

            let invalid = call(
                &router,
                "owner-secret",
                5,
                "publish_artifact",
                json!({ "html": "<p>x</p>", "artifact_key": "Bad Key" }),
            )
            .await;
            assert_eq!(invalid["error"]["code"], -32602);
            assert_eq!(invalid["error"]["message"], ARTIFACT_KEY_INVALID);

            let mismatch = call(
                &router,
                "owner-secret",
                6,
                "publish_bundle",
                json!({ "files": { "index.html": "<p>b</p>" }, "artifact_key": "unit/report" }),
            )
            .await;
            assert_eq!(
                tool_error(&mismatch),
                "artifact_key \"unit/report\" belongs to a single-file artifact; use publish_artifact"
            );

            let denied = call(
                &router,
                "second-secret",
                7,
                "publish_artifact",
                json!({ "html": "<p>no</p>", "artifact_key": "unit/report" }),
            )
            .await;
            let denied_update = call(
                &router,
                "second-secret",
                8,
                "update_artifact",
                json!({ "id": id, "html": "<p>no</p>" }),
            )
            .await;
            assert_eq!(
                tool_error(&denied),
                "Permission denied: this API key cannot modify this artifact"
            );
            assert_eq!(tool_error(&denied), tool_error(&denied_update));

            let foreign = call(
                &router,
                "foreign-secret",
                9,
                "publish_artifact",
                json!({ "html": "<p>f</p>", "artifact_key": "unit/report" }),
            )
            .await;
            assert_eq!(ok(&foreign)["created"], true);
            assert_ne!(ok(&foreign)["id"], id.as_str());

            let unkeyed = call(
                &router,
                "owner-secret",
                10,
                "publish_artifact",
                json!({ "html": "<p>plain</p>" }),
            )
            .await;
            let unkeyed = ok(&unkeyed);
            assert!(unkeyed.get("artifact_key").is_none());
            assert!(unkeyed.get("created").is_none());
            assert!(unkeyed.get("revision").is_none());

            let deleted = call(&router, "owner-secret", 11, "delete_artifact", json!({ "id": id })).await;
            assert_eq!(ok(&deleted)["deleted"], true);
            let recreated = call(
                &router,
                "owner-secret",
                12,
                "publish_artifact",
                json!({ "html": "<p>three</p>", "artifact_key": "unit/report" }),
            )
            .await;
            let recreated = ok(&recreated);
            assert_eq!(recreated["created"], true);
            assert_eq!(recreated["revision"], 1);
            assert_ne!(recreated["id"], id.as_str());
            Ok(())
        },
    )
    .await
    .expect("runtime");
    assert_eq!(keyed_rows(&data_dir, "acme", "unit/report"), 1);
    assert_eq!(keyed_rows(&data_dir, "other", "unit/report"), 1);
}

async fn race(router: &Router, racers: u64, key: &str, base_id: u64) -> Vec<Value> {
    let mut tasks = Vec::new();
    for racer in 0..racers {
        let router = router.clone();
        let key = key.to_owned();
        tasks.push(tokio::spawn(async move {
            call(
                &router,
                "owner-secret",
                base_id + racer,
                "publish_artifact",
                json!({ "html": format!("<p>racer {racer}</p>"), "artifact_key": key }),
            )
            .await
        }));
    }
    let mut responses = Vec::new();
    for task in tasks {
        responses.push(task.await.expect("racer joins"));
    }
    responses
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_keyed_creates_converge_on_one_artifact() {
    let temp = TempDir::new();
    let data_dir = temp.path().to_path_buf();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            // Two concurrent creates: the unique index admits one row; the loser re-resolves the
            // key and updates the winner. Both calls succeed, deterministically.
            for round in 0..10_u64 {
                let key = format!("unit/pair-{round}");
                let responses = race(&router, 2, &key, 100 + round * 10).await;
                let payloads: Vec<&Value> = responses.iter().map(ok).collect();
                assert_eq!(payloads[0]["id"], payloads[1]["id"], "{key}");
                let mut created: Vec<bool> = payloads
                    .iter()
                    .map(|payload| payload["created"] == true)
                    .collect();
                created.sort_unstable();
                assert_eq!(created, vec![false, true], "{key}");
                let mut revisions: Vec<u64> = payloads
                    .iter()
                    .map(|payload| payload["revision"].as_u64().expect("revision"))
                    .collect();
                revisions.sort_unstable();
                assert_eq!(revisions, vec![1, 2], "{key}");
            }

            // A stampede still yields one artifact. Racers that lose the create become
            // concurrent updaters with `update_artifact` semantics, so a stale one fails with a
            // clean conflict error rather than creating or overwriting blindly.
            let responses = race(&router, 8, "unit/stampede", 1000).await;
            let mut ids = Vec::new();
            let mut created = 0;
            let mut successes = 0;
            for response in &responses {
                if response["result"]["isError"] == true {
                    // A stale updater gets the update_artifact conflict; a racer whose lookup
                    // lands while another keyed update holds its durability intent sees the row
                    // as in flight. Both are clean refusals, never a second artifact.
                    let message = tool_error(response);
                    assert!(
                        message
                            == "Artifact changed during update; fetch its current revision and retry"
                            || message
                                == "artifact_key is being published by another request; retry",
                        "unexpected stampede error: {message}"
                    );
                    continue;
                }
                let payload = ok(response);
                successes += 1;
                ids.push(payload["id"].as_str().expect("id").to_owned());
                if payload["created"] == true {
                    created += 1;
                }
            }
            ids.sort();
            ids.dedup();
            assert_eq!(
                ids.len(),
                1,
                "every successful racer resolved the same artifact"
            );
            assert_eq!(created, 1, "exactly one racer created the artifact");
            assert!(
                successes >= 2,
                "the creator and at least one updater succeed"
            );
            Ok(())
        },
    )
    .await
    .expect("runtime");
    for round in 0..10 {
        assert_eq!(
            keyed_rows(&data_dir, "acme", &format!("unit/pair-{round}")),
            1
        );
    }
    assert_eq!(keyed_rows(&data_dir, "acme", "unit/stampede"), 1);
}
