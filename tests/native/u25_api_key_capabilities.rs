//! PBI-043 production-backed API key role matrix and revision attribution.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::{
    config::{AppConfig, Secret, SeedKeys},
    persistence::db,
    security::access::{
        DELETE_PERMISSION_ERROR, PUBLISH_PERMISSION_ERROR, READ_PERMISSION_ERROR,
        WRITE_PERMISSION_ERROR,
    },
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
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
            "artifact-mcp-u25-capabilities-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create U25 temp directory");
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
    AppConfig {
        data_dir: data_dir.to_path_buf(),
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse(
            "role-reader:acme:reader-secret,\
             role-author:acme:author-secret,\
             role-collaborator:acme:collaborator-secret,\
             role-colleague:acme:colleague-secret,\
             role-foreign:beta:foreign-secret",
        ),
        ..AppConfig::defaults()
    }
}

async fn call(router: &Router, key: &str, request_id: u64, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": request_id,
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

fn structured(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

fn error_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool error text")
}

async fn publish(router: &Router, key: &str, request_id: u64, title: &str) -> String {
    let response = call(
        router,
        key,
        request_id,
        "publish_artifact",
        json!({ "html": "<p>seed</p>", "title": title }),
    )
    .await;
    structured(&response)["id"]
        .as_str()
        .expect("published artifact id")
        .to_owned()
}

#[derive(Clone, Copy)]
enum Outcome {
    Allowed,
    Refused(&'static str),
    Concealed,
}

#[tokio::test]
async fn role_matrix_and_revision_attribution_work_on_production_adapters() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            // Seeded keys default to author. Publish every "own" fixture first, then deliberately
            // assign the reader/collaborator roles in the same database production auth queries.
            let reader_own = publish(&router, "reader-secret", 1, "reader own").await;
            let author_own = publish(&router, "author-secret", 2, "author own").await;
            let collaborator_own =
                publish(&router, "collaborator-secret", 3, "collaborator own").await;

            let reader_same = publish(&router, "colleague-secret", 4, "reader same").await;
            let author_same = publish(&router, "colleague-secret", 5, "author same").await;
            let collaborator_same =
                publish(&router, "colleague-secret", 6, "collaborator same").await;

            let reader_cross = publish(&router, "foreign-secret", 7, "reader cross").await;
            let author_cross = publish(&router, "foreign-secret", 8, "author cross").await;
            let collaborator_cross =
                publish(&router, "foreign-secret", 9, "collaborator cross").await;

            {
                let conn = Connection::open(db::database_path(temp.path()))
                    .expect("open production database");
                conn.execute(
                    "UPDATE api_keys SET role = 'reader' WHERE client_id = 'role-reader'",
                    [],
                )
                .expect("assign reader role");
                conn.execute(
                    "UPDATE api_keys SET role = 'collaborator' \
                     WHERE client_id = 'role-collaborator'",
                    [],
                )
                .expect("assign collaborator role");
            }

            let reader_publish = call(
                &router,
                "reader-secret",
                10,
                "publish_artifact",
                json!({ "html": "<p>refused</p>" }),
            )
            .await;
            assert_eq!(error_text(&reader_publish), PUBLISH_PERMISSION_ERROR);

            let author_list = call(&router, "author-secret", 11, "list_artifacts", json!({})).await;
            let author_ids: Vec<&str> = structured(&author_list)["artifacts"]
                .as_array()
                .expect("author listing")
                .iter()
                .filter_map(|row| row["id"].as_str())
                .collect();
            assert_eq!(author_ids, vec![author_own.as_str()]);

            for (key, expected_id) in [
                ("reader-secret", reader_same.as_str()),
                ("collaborator-secret", collaborator_same.as_str()),
            ] {
                let listed = call(&router, key, 12, "list_artifacts", json!({})).await;
                let rows = structured(&listed)["artifacts"]
                    .as_array()
                    .expect("organization-wide listing");
                assert!(rows.iter().any(|row| row["id"] == expected_id));
                assert!(rows.iter().all(|row| row.get("client_id").is_none()));
                assert!(rows.iter().all(|row| row.get("uploader_label").is_some()));
            }

            let cases = [
                (
                    "reader",
                    "reader-secret",
                    [
                        (
                            "own",
                            reader_own.as_str(),
                            [
                                Outcome::Allowed,
                                Outcome::Refused(WRITE_PERMISSION_ERROR),
                                Outcome::Refused(DELETE_PERMISSION_ERROR),
                            ],
                        ),
                        (
                            "same",
                            reader_same.as_str(),
                            [
                                Outcome::Allowed,
                                Outcome::Refused(WRITE_PERMISSION_ERROR),
                                Outcome::Refused(DELETE_PERMISSION_ERROR),
                            ],
                        ),
                        (
                            "cross",
                            reader_cross.as_str(),
                            [Outcome::Concealed, Outcome::Concealed, Outcome::Concealed],
                        ),
                    ],
                ),
                (
                    "author",
                    "author-secret",
                    [
                        (
                            "own",
                            author_own.as_str(),
                            [Outcome::Allowed, Outcome::Allowed, Outcome::Allowed],
                        ),
                        (
                            "same",
                            author_same.as_str(),
                            [
                                Outcome::Refused(READ_PERMISSION_ERROR),
                                Outcome::Refused(WRITE_PERMISSION_ERROR),
                                Outcome::Refused(DELETE_PERMISSION_ERROR),
                            ],
                        ),
                        (
                            "cross",
                            author_cross.as_str(),
                            [Outcome::Concealed, Outcome::Concealed, Outcome::Concealed],
                        ),
                    ],
                ),
                (
                    "collaborator",
                    "collaborator-secret",
                    [
                        (
                            "own",
                            collaborator_own.as_str(),
                            [Outcome::Allowed, Outcome::Allowed, Outcome::Allowed],
                        ),
                        (
                            "same",
                            collaborator_same.as_str(),
                            [
                                Outcome::Allowed,
                                Outcome::Allowed,
                                Outcome::Refused(DELETE_PERMISSION_ERROR),
                            ],
                        ),
                        (
                            "cross",
                            collaborator_cross.as_str(),
                            [Outcome::Concealed, Outcome::Concealed, Outcome::Concealed],
                        ),
                    ],
                ),
            ];

            let mut request_id = 20;
            for (role, key, targets) in cases {
                for (scope, id, outcomes) in targets {
                    request_id += 1;
                    let read = call(
                        &router,
                        key,
                        request_id,
                        "read_artifact",
                        json!({ "id": id }),
                    )
                    .await;
                    request_id += 1;
                    let write = call(
                        &router,
                        key,
                        request_id,
                        "patch_artifact",
                        json!({
                            "id": id,
                            "expected_revision": 1,
                            "edits": [{ "find": "seed", "replace": format!("{role}-{scope}") }]
                        }),
                    )
                    .await;
                    request_id += 1;
                    let delete = call(
                        &router,
                        key,
                        request_id,
                        "delete_artifact",
                        json!({ "id": id }),
                    )
                    .await;

                    for (operation, response, outcome) in [
                        ("read", &read, outcomes[0]),
                        ("write", &write, outcomes[1]),
                        ("delete", &delete, outcomes[2]),
                    ] {
                        match outcome {
                            Outcome::Allowed => assert_ne!(
                                response["result"]["isError"],
                                Value::Bool(true),
                                "{role} {scope} {operation}"
                            ),
                            Outcome::Refused(message) => assert_eq!(
                                error_text(response),
                                message,
                                "{role} {scope} {operation}"
                            ),
                            Outcome::Concealed => assert_eq!(
                                error_text(response),
                                format!("Unknown artifact: {id}"),
                                "{role} {scope} {operation}"
                            ),
                        }
                    }
                }
            }

            // Patch the colleague artifact again so revision 2 becomes retained history. Its
            // attribution must name the collaborator, while revision 1 names the colleague.
            let second_patch = call(
                &router,
                "collaborator-secret",
                60,
                "patch_artifact",
                json!({
                    "id": collaborator_same,
                    "expected_revision": 2,
                    "edits": [{ "find": "collaborator-same", "replace": "second-edit" }]
                }),
            )
            .await;
            assert_eq!(structured(&second_patch)["revision"], 3);
            let revisions = call(
                &router,
                "collaborator-secret",
                61,
                "list_revisions",
                json!({ "id": collaborator_same }),
            )
            .await;
            let rows = structured(&revisions)["revisions"]
                .as_array()
                .expect("revision listing");
            assert_eq!(rows[0]["revision"], 2);
            assert_eq!(rows[0]["client_id"], "role-collaborator");
            assert_eq!(rows[1]["revision"], 1);
            assert_eq!(rows[1]["client_id"], "role-colleague");

            let conn = Connection::open(db::database_path(temp.path()))
                .expect("reopen production database");
            let live_actor: String = conn
                .query_row(
                    "SELECT client_id FROM artifact_revisions \
                     WHERE artifact_id = ?1 AND revision = 3",
                    [&collaborator_same],
                    |row| row.get(0),
                )
                .expect("live revision attribution");
            assert_eq!(live_actor, "role-collaborator");
            Ok(())
        },
    )
    .await
    .expect("run U25 capability server");
}
