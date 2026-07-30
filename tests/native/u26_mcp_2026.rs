//! Production-backed transport coverage for PBI-069 / GitHub issue #4.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    sync::Mutex,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::config::{AppConfig, OAuthConfig, Secret, SeedKeys};
use artifact_mcp::model::{ArtifactId, ClientId, OrgId, PublisherIdentity};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::u20_runtime::runtime;

const MODERN_VERSION: &str = "2026-07-28";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u26-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create u26 temp directory");
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

struct Observer;

impl runtime::StartupObserver for Observer {
    fn stage(&self, _stage: runtime::StartupStage) {}
}

fn config_for(data_dir: &Path) -> AppConfig {
    AppConfig {
        data_dir: data_dir.to_path_buf(),
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse("publisher:acme:owner-secret,foreign:other:foreign-secret"),
        oauth: OAuthConfig {
            issuer: "https://auth.conformance.test".to_owned(),
            audience: "http://conformance.test/mcp".to_owned(),
            jwks_url: "https://auth.conformance.test/jwks".to_owned(),
            ..OAuthConfig::default()
        },
        ..AppConfig::defaults()
    }
}

fn meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {
            "name": "artifact-mcp-u26",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

fn apps_meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {
            "name": "artifact-mcp-u26-apps",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {
            "extensions": {
                "io.modelcontextprotocol/ui": {
                    "mimeTypes": ["text/html;profile=mcp-app"]
                }
            }
        }
    })
}

fn tasks_meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {
            "name": "artifact-mcp-u26-tasks",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {
            "extensions": {
                "io.modelcontextprotocol/tasks": {}
            }
        }
    })
}

async fn post(router: &Router, body: Value, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    post_as(router, "owner-secret", body, headers).await
}

async fn post_as(
    router: &Router,
    key: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut request = Request::post("/mcp")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(Body::from(body.to_string()))
                .expect("MCP request"),
        )
        .await
        .expect("MCP response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read MCP response");
    let value = serde_json::from_slice(&bytes).expect("MCP JSON response");
    (status, value)
}

#[tokio::test]
async fn modern_and_legacy_mcp_share_one_endpoint_without_contract_leakage() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let (status, discover) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "discover",
                    "method": "server/discover",
                    "params": { "_meta": meta(MODERN_VERSION) }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "server/discover"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(discover["result"]["resultType"], "complete");
            assert_eq!(
                discover["result"]["supportedVersions"],
                json!(["2026-07-28", "2025-06-18"])
            );
            assert_eq!(discover["result"]["cacheScope"], "private");
            assert_eq!(
                discover["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
                "artifact-mcp"
            );
            assert_eq!(
                discover["result"]["capabilities"]["extensions"]
                    ["io.modelcontextprotocol/oauth-client-credentials"],
                json!({})
            );
            assert_eq!(
                discover["result"]["capabilities"]["extensions"]
                    ["io.modelcontextprotocol/tasks"],
                json!({})
            );
            assert!(
                discover["result"]["capabilities"]["extensions"]
                    .get("io.modelcontextprotocol/skills")
                    .is_none(),
                "draft Skills over MCP must remain absent until ADR-0004's gate passes"
            );

            let metadata_response = router
                .clone()
                .oneshot(
                    Request::get("/.well-known/oauth-protected-resource")
                        .body(Body::empty())
                        .expect("OAuth resource metadata request"),
                )
                .await
                .expect("OAuth resource metadata response");
            assert_eq!(metadata_response.status(), StatusCode::OK);
            let metadata = serde_json::from_slice::<Value>(
                &to_bytes(metadata_response.into_body(), usize::MAX)
                    .await
                    .expect("OAuth resource metadata body"),
            )
            .expect("OAuth resource metadata JSON");
            assert_eq!(metadata["resource"], "http://conformance.test/mcp");
            assert_eq!(
                metadata["authorization_servers"],
                json!(["https://auth.conformance.test"])
            );
            assert_eq!(
                metadata["scopes_supported"],
                json!([
                    "artifacts:read",
                    "artifacts:publish",
                    "artifacts:review",
                    "artifacts:visibility",
                    "artifacts:delete",
                    "audit:read",
                    "audit:export",
                    "audit:global"
                ])
            );

            let unauthorized = router
                .clone()
                .oneshot(
                    Request::post("/mcp")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "jsonrpc": "2.0",
                                "id": "unauthorized",
                                "method": "tools/list"
                            })
                            .to_string(),
                        ))
                        .expect("unauthorized MCP request"),
                )
                .await
                .expect("unauthorized MCP response");
            assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
            assert!(
                unauthorized
                    .headers()
                    .get("www-authenticate")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.contains(
                        "resource_metadata=\"http://conformance.test/.well-known/oauth-protected-resource\""
                    ))
            );

            let (status, listed) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "tools",
                    "method": "tools/list",
                    "params": { "_meta": meta(MODERN_VERSION) }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/list"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(listed["result"]["resultType"], "complete");
            assert_eq!(listed["result"]["cacheScope"], "private");
            assert_eq!(listed["result"]["tools"].as_array().map(Vec::len), Some(22));
            assert!(
                listed["result"]["tools"]
                    .as_array()
                    .expect("modern tools")
                    .iter()
                    .all(|tool| tool["outputSchema"]["type"] == "object")
            );
            assert!(
                listed["result"]["tools"]
                    .as_array()
                    .expect("fallback modern tools")
                    .iter()
                    .all(|tool| tool.get("_meta").is_none())
            );

            let (status, app_tools) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "app-tools",
                    "method": "tools/list",
                    "params": { "_meta": apps_meta(MODERN_VERSION) }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/list"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                app_tools["result"]["tools"].as_array().map(Vec::len),
                Some(23)
            );
            let linked_tools = app_tools["result"]["tools"]
                .as_array()
                .expect("app tools")
                .iter()
                .filter(|tool| tool["_meta"]["ui"].get("resourceUri").is_some())
                .map(|tool| tool["name"].as_str().expect("tool name"))
                .collect::<Vec<_>>();
            assert_eq!(
                linked_tools,
                [
                    "publish_artifact",
                    "publish_bundle",
                    "list_artifacts",
                    "read_artifact"
                ]
            );
            let submit_feedback = app_tools["result"]["tools"]
                .as_array()
                .expect("app tools")
                .iter()
                .find(|tool| tool["name"] == "submit_feedback")
                .expect("app-only feedback tool");
            assert_eq!(
                submit_feedback["_meta"]["ui"]["visibility"],
                json!(["app"])
            );
            assert!(
                app_tools["result"]["tools"]
                    .as_array()
                    .expect("app tools")
                    .iter()
                    .filter(|tool| tool["name"] != "submit_feedback")
                    .all(|tool| tool["_meta"]["ui"]["visibility"]
                        .as_array()
                        .is_some_and(|visibility| visibility.contains(&json!("model"))))
            );

            let (status, called) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "call",
                    "method": "tools/call",
                    "params": {
                        "name": "list_artifacts",
                        "arguments": {},
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "list_artifacts"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(called["result"]["resultType"], "complete");
            assert_eq!(called["result"]["structuredContent"]["count"], 0);

            let (status, published) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "publish-resource",
                    "method": "tools/call",
                    "params": {
                        "name": "publish_bundle",
                        "arguments": {
                            "title": "Resource bundle",
                            "files": {
                                "index.html": "<h1>Revision one</h1>",
                                "docs/guide.html": "<p>Guide one</p>"
                            }
                        },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "publish_bundle"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let artifact_id = published["result"]["structuredContent"]["id"]
                .as_str()
                .expect("published artifact id")
                .to_owned();
            let artifact_uri = format!("artifact://{artifact_id}");
            assert_eq!(
                published["result"]["content"]
                    .as_array()
                    .and_then(|content| content.last())
                    .and_then(|item| item.get("uri")),
                Some(&Value::String(artifact_uri.clone()))
            );
            assert_eq!(
                published["result"]["_meta"]["com.agentshelf.artifact-mcp/review"]["artifacts"][0]
                    ["title"],
                "Resource bundle"
            );
            assert_eq!(
                published["result"]["_meta"]["com.agentshelf.artifact-mcp/review"]["artifacts"][0]
                    ["canManage"],
                true
            );

            let (status, direct_feedback) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "direct-feedback",
                    "method": "tools/call",
                    "params": {
                        "name": "submit_feedback",
                        "arguments": {
                            "id": artifact_id,
                            "body": "Direct model calls must not see this app-only tool."
                        },
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "submit_feedback"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(direct_feedback["error"]["code"], -32_602);

            let (status, submitted_feedback) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "app-feedback",
                    "method": "tools/call",
                    "params": {
                        "name": "submit_feedback",
                        "arguments": {
                            "id": artifact_id,
                            "body": "The inline review action works."
                        },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "submit_feedback"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                submitted_feedback["result"]["structuredContent"]["artifact_id"],
                artifact_id
            );
            assert_eq!(
                submitted_feedback["result"]["structuredContent"]["submitted"],
                true
            );

            let (status, templates) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "resource-templates",
                    "method": "resources/templates/list",
                    "params": { "_meta": apps_meta(MODERN_VERSION) }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/templates/list"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                templates["result"]["resourceTemplates"]
                    .as_array()
                    .expect("resource templates")
                    .iter()
                    .any(|template| template["uriTemplate"] == "artifact://{id}")
            );

            let (status, resources) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "resources",
                    "method": "resources/list",
                    "params": { "_meta": apps_meta(MODERN_VERSION) }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/list"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(resources["result"]["cacheScope"], "private");
            assert!(
                resources["result"]["resources"]
                    .as_array()
                    .expect("resource rows")
                    .iter()
                    .any(|resource| resource["uri"] == artifact_uri)
            );
            assert_eq!(
                resources["result"]["resources"][0]["uri"],
                "ui://artifact-mcp/review"
            );

            let app_uri = "ui://artifact-mcp/review";
            let (status, app_resource) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "app-resource",
                    "method": "resources/read",
                    "params": {
                        "uri": app_uri,
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/read"),
                    ("mcp-name", app_uri),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                app_resource["result"]["contents"][0]["mimeType"],
                "text/html;profile=mcp-app"
            );
            assert_eq!(
                app_resource["result"]["contents"][0]["_meta"]["ui"]["csp"],
                json!({
                    "connectDomains": [],
                    "resourceDomains": [],
                    "frameDomains": [],
                    "baseUriDomains": []
                })
            );
            let app_html = app_resource["result"]["contents"][0]["text"]
                .as_str()
                .expect("review app HTML");
            assert!(!app_html.contains("<iframe"));
            assert!(!app_html.contains(".innerHTML"));

            let thumbnail_uri = format!("{artifact_uri}/thumbnail");
            let (status, thumbnail) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "thumbnail-resource",
                    "method": "resources/read",
                    "params": {
                        "uri": thumbnail_uri.clone(),
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/read"),
                    ("mcp-name", thumbnail_uri.as_str()),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                thumbnail["result"]["contents"][0]["mimeType"],
                "image/svg+xml"
            );
            assert!(
                thumbnail["result"]["contents"][0]["blob"]
                    .as_str()
                    .is_some_and(|blob| !blob.is_empty())
            );
            assert_eq!(
                thumbnail["result"]["contents"][0]["_meta"]
                    ["com.agentshelf.artifact-mcp/trustedThumbnail"],
                true
            );

            let (status, fallback_app_read) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "fallback-app-resource",
                    "method": "resources/read",
                    "params": {
                        "uri": app_uri,
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/read"),
                    ("mcp-name", app_uri),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(fallback_app_read["error"]["code"], -32_602);

            let current_uri = format!("{artifact_uri}/files/docs/guide.html");
            let (status, current_file) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "read-current-file",
                    "method": "resources/read",
                    "params": {
                        "uri": current_uri.clone(),
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/read"),
                    ("mcp-name", current_uri.as_str()),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                current_file["result"]["contents"][0]["text"],
                "<p>Guide one</p>"
            );

            let (status, updated) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "update-resource",
                    "method": "tools/call",
                    "params": {
                        "name": "update_artifact",
                        "arguments": {
                            "id": artifact_id,
                            "expected_revision": 1,
                            "files": {
                                "index.html": "<h1>Revision two</h1>",
                                "docs/guide.html": "<p>Guide two</p>"
                            }
                        },
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "update_artifact"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(updated["result"]["structuredContent"]["revision"], 2);

            let (status, revisions) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "list-revisions",
                    "method": "tools/call",
                    "params": {
                        "name": "list_revisions",
                        "arguments": { "id": artifact_id },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "list_revisions"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(revisions["result"]["structuredContent"]["current"], 2);
            assert!(
                revisions["result"]["structuredContent"]["revisions"]
                    .as_array()
                    .is_some_and(|items| items
                        .iter()
                        .any(|revision| revision["revision"] == 1))
            );

            let historical_uri =
                format!("{artifact_uri}/revisions/1/files/docs/guide.html");
            let (status, historical_file) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "read-historical-file",
                    "method": "resources/read",
                    "params": {
                        "uri": historical_uri.clone(),
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/read"),
                    ("mcp-name", historical_uri.as_str()),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                historical_file["result"]["contents"][0]["text"],
                "<p>Guide one</p>"
            );
            assert_eq!(
                historical_file["result"]["contents"][0]["_meta"]
                    ["com.agentshelf.artifact-mcp/revision"],
                1
            );

            let (status, hidden) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "hide-artifact",
                    "method": "tools/call",
                    "params": {
                        "name": "set_visibility",
                        "arguments": { "id": artifact_id, "hidden": true },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "set_visibility"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(hidden["result"]["structuredContent"]["hidden"], true);
            assert_eq!(
                hidden["result"]["_meta"]["com.agentshelf.artifact-mcp/review"]["artifacts"][0]
                    ["hidden"],
                true
            );

            let (status, shared) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "share-artifact",
                    "method": "tools/call",
                    "params": {
                        "name": "create_share",
                        "arguments": { "id": artifact_id, "expires": "24h" },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "create_share"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(shared["result"]["structuredContent"]["id"], artifact_id);
            assert!(
                shared["result"]["structuredContent"]["url"]
                    .as_str()
                    .is_some_and(|url| url.contains("/s/"))
            );

            let (status, concealed) = post_as(
                &router,
                "foreign-secret",
                json!({
                    "jsonrpc": "2.0",
                    "id": "foreign-resource",
                    "method": "resources/read",
                    "params": {
                        "uri": artifact_uri.clone(),
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "resources/read"),
                    ("mcp-name", artifact_uri.as_str()),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(concealed["error"]["code"], -32_602);

            let (status, concealed_feedback) = post_as(
                &router,
                "foreign-secret",
                json!({
                    "jsonrpc": "2.0",
                    "id": "foreign-feedback",
                    "method": "tools/call",
                    "params": {
                        "name": "submit_feedback",
                        "arguments": {
                            "id": artifact_id,
                            "body": "Cross-org probe"
                        },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "submit_feedback"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(concealed_feedback["result"]["isError"], true);
            assert_eq!(
                concealed_feedback["result"]["content"][0]["text"],
                format!("Unknown artifact: {artifact_id}")
            );

            let (status, delete_target) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "publish-delete-target",
                    "method": "tools/call",
                    "params": {
                        "name": "publish_artifact",
                        "arguments": {
                            "html": "<h1>Delete target</h1>",
                            "title": "Delete target"
                        },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "publish_artifact"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let delete_id = delete_target["result"]["structuredContent"]["id"]
                .as_str()
                .expect("delete target id");
            let (status, deleted) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "delete-target",
                    "method": "tools/call",
                    "params": {
                        "name": "delete_artifact",
                        "arguments": { "id": delete_id },
                        "_meta": apps_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "delete_artifact"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(deleted["result"]["structuredContent"]["deleted"], true);
            assert_eq!(
                deleted["result"]["_meta"]["com.agentshelf.artifact-mcp/audit"],
                json!({
                    "action": "delete",
                    "artifactId": delete_id,
                    "actor": "agent:publisher",
                    "outcome": "deleted"
                })
            );

            let (status, initialized) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "legacy-initialize",
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {
                            "name": "legacy-client",
                            "version": "1.0.0"
                        }
                    }
                }),
                &[("mcp-protocol-version", "2025-11-25")],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
            assert!(initialized["result"].get("resultType").is_none());

            let (status, legacy_with_version_header) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "legacy-version-header",
                    "method": "tools/list",
                    "params": {
                        "_meta": { "progressToken": "legacy-progress" }
                    }
                }),
                &[("mcp-protocol-version", "2025-11-25")],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                legacy_with_version_header["result"]
                    .get("resultType")
                    .is_none()
            );
            assert!(
                legacy_with_version_header["result"]["tools"]
                    .as_array()
                    .is_some_and(|tools| tools.len() == 21)
            );

            let (status, legacy) = post(
                &router,
                json!({ "jsonrpc": "2.0", "id": "legacy", "method": "tools/list" }),
                &[],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(legacy["result"].get("resultType").is_none());
            assert!(legacy["result"].get("ttlMs").is_none());

            let (status, mismatch) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "mismatch",
                    "method": "tools/list",
                    "params": { "_meta": meta(MODERN_VERSION) }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(mismatch["error"]["code"], -32_020);

            let (status, name_mismatch) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "name-mismatch",
                    "method": "tools/call",
                    "params": {
                        "name": "list_artifacts",
                        "arguments": {},
                        "_meta": meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "read_artifact"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(name_mismatch["error"]["code"], -32_020);

            let (status, unsupported) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "unsupported",
                    "method": "tools/list",
                    "params": { "_meta": meta("2099-01-01") }
                }),
                &[
                    ("mcp-protocol-version", "2099-01-01"),
                    ("mcp-method", "tools/list"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(unsupported["error"]["code"], -32_022);
            assert_eq!(
                unsupported["error"]["data"]["supported"],
                json!(["2026-07-28", "2025-06-18"])
            );

            let (status, unknown) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "unknown",
                    "method": "example/unknown",
                    "params": { "_meta": meta(MODERN_VERSION) }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "example/unknown"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(unknown["error"]["code"], -32_601);

            let (status, modern_batch) = post(
                &router,
                json!([{
                    "jsonrpc": "2.0",
                    "id": "batch",
                    "method": "tools/list",
                    "params": { "_meta": meta(MODERN_VERSION) }
                }]),
                &[("mcp-protocol-version", MODERN_VERSION)],
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(modern_batch["error"]["code"], -32_600);

            Ok(())
        },
    )
    .await
    .expect("run production MCP transport");
}

#[tokio::test]
async fn mcp_preflight_advertises_modern_request_headers() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let response = router
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri("/mcp")
                        .body(Body::empty())
                        .expect("MCP preflight"),
                )
                .await
                .expect("MCP preflight response");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let allowed = response
                .headers()
                .get("access-control-allow-headers")
                .and_then(|value| value.to_str().ok())
                .expect("allowed request headers");
            for expected in ["mcp-protocol-version", "mcp-method", "mcp-name", "accept"] {
                assert!(
                    allowed.contains(expected),
                    "missing {expected} in {allowed}"
                );
            }
            Ok(())
        },
    )
    .await
    .expect("run production MCP preflight");
}

#[tokio::test]
async fn mcp_rejects_non_json_media_types_with_a_json_rpc_415_envelope() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let response = router
                .oneshot(
                    Request::post("/mcp")
                        .header("authorization", "Bearer owner-secret")
                        .header("content-type", "text/plain")
                        .body(Body::from(
                            json!({
                                "jsonrpc": "2.0",
                                "id": "wrong-media",
                                "method": "tools/list"
                            })
                            .to_string(),
                        ))
                        .expect("MCP request"),
                )
                .await
                .expect("MCP response");
            assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            let body = serde_json::from_slice::<Value>(
                &to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("MCP media rejection body"),
            )
            .expect("JSON-RPC media rejection");
            assert_eq!(body["jsonrpc"], "2.0");
            assert!(body["id"].is_null());
            assert_eq!(body["error"]["data"]["reason"], "unsupported_media_type");
            Ok(())
        },
    )
    .await
    .expect("run production MCP media-type rejection");
}

#[tokio::test]
async fn legacy_mcp_batches_are_read_only_and_reject_mixed_work_before_dispatch() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let (status, read_only) = post(
                &router,
                json!([
                    { "jsonrpc": "2.0", "id": "list", "method": "tools/list" },
                    { "jsonrpc": "2.0", "id": "ping", "method": "ping" }
                ]),
                &[],
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert!(read_only.is_array());

            let (status, mixed) = post(
                &router,
                json!([
                    { "jsonrpc": "2.0", "id": "list", "method": "tools/list" },
                    {
                        "jsonrpc": "2.0",
                        "id": "write",
                        "method": "tools/call",
                        "params": { "name": "submit_feedback", "arguments": {} }
                    }
                ]),
                &[],
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(mixed["error"]["code"], -32_600);
            Ok(())
        },
    )
    .await
    .expect("run legacy batch admission");
}

#[tokio::test]
async fn mcp_metrics_emit_safe_success_failure_and_correlation_signals() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let response = router
                .clone()
                .oneshot(
                    Request::post("/mcp")
                        .header("authorization", "Bearer owner-secret")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "jsonrpc": "2.0",
                                "id": "legacy-list",
                                "method": "tools/list"
                            })
                            .to_string(),
                        ))
                        .expect("successful MCP request"),
                )
                .await
                .expect("successful MCP response");
            assert_eq!(response.status(), StatusCode::OK);
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .expect("opaque request id");
            assert!(request_id.starts_with("mcp_"));

            let unauthorized = router
                .clone()
                .oneshot(
                    Request::post("/mcp")
                        .header("authorization", "Bearer must-never-appear")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "jsonrpc": "2.0",
                                "id": "denied",
                                "method": "tools/list"
                            })
                            .to_string(),
                        ))
                        .expect("unauthorized MCP request"),
                )
                .await
                .expect("unauthorized MCP response");
            assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
            assert!(unauthorized.headers().contains_key("x-request-id"));

            let metrics = router
                .oneshot(
                    Request::get("/metrics")
                        .body(Body::empty())
                        .expect("metrics request"),
                )
                .await
                .expect("metrics response");
            assert_eq!(metrics.status(), StatusCode::OK);
            assert!(
                metrics
                    .headers()
                    .get("cache-control")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.contains("no-store"))
            );
            let body = String::from_utf8(
                to_bytes(metrics.into_body(), usize::MAX)
                    .await
                    .expect("metrics body")
                    .to_vec(),
            )
            .expect("UTF-8 metrics");
            assert!(body.contains(
                "operation=\"listing\",method=\"tools/list\",name=\"none\",outcome=\"success\""
            ));
            assert!(body.contains("outcome=\"authentication_failure\""));
            assert!(!body.contains("must-never-appear"));
            assert!(!body.contains("owner-secret"));
            assert!(!body.contains("legacy-list"));
            Ok(())
        },
    )
    .await
    .expect("run production MCP metrics");
}

#[tokio::test]
async fn durable_preview_tasks_recover_after_restart_and_enforce_ownership() {
    let temp = TempDir::new();
    let artifact_id = Arc::new(Mutex::new(None::<String>));
    let captured = Arc::clone(&artifact_id);
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        move |_host, _port, router| async move {
            let (_, published) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "publish-for-task",
                    "method": "tools/call",
                    "params": {
                        "name": "publish_artifact",
                        "arguments": { "html": "<h1>Durable preview</h1>" }
                    }
                }),
                &[],
            )
            .await;
            *captured.lock().expect("artifact id lock") = published["result"]["structuredContent"]
                ["id"]
                .as_str()
                .map(ToOwned::to_owned);
            Ok(())
        },
    )
    .await
    .expect("publish before restart");

    let artifact_id = artifact_id
        .lock()
        .expect("artifact id lock")
        .clone()
        .expect("published artifact id");
    let store = artifact_mcp::mcp::tasks::PreviewTaskStore::new(temp.path());
    let task = store
        .create(
            ArtifactId::from(artifact_id.as_str()),
            &PublisherIdentity {
                client_id: ClientId::from("publisher"),
                org: OrgId::from("acme"),
                label: "Publisher".to_owned(),
                role: "author".to_owned(),
                scopes: None,
            },
        )
        .expect("durable task before restart");
    drop(store);

    let renderer = super::u16_support::StubRenderer::rendering(super::u16_support::sample_png());
    let mut config = config_for(temp.path());
    config.preview = renderer.config();
    let task_id = task.task_id.clone();
    runtime::run_with_bind(
        config,
        Arc::new(Observer),
        move |_host, _port, router| async move {
            let mut completed = None;
            for sequence in 0..200_u64 {
                let (status, response) = post(
                    &router,
                    json!({
                        "jsonrpc": "2.0",
                        "id": format!("poll-{sequence}"),
                        "method": "tasks/get",
                        "params": {
                            "taskId": task_id,
                            "_meta": tasks_meta(MODERN_VERSION)
                        }
                    }),
                    &[
                        ("mcp-protocol-version", MODERN_VERSION),
                        ("mcp-method", "tasks/get"),
                        ("mcp-name", task_id.as_str()),
                    ],
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                if response["result"]["status"] == "completed" {
                    completed = Some(response);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let completed = completed.expect("recovered task completed");
            assert_eq!(
                completed["result"]["result"]["structuredContent"]["regenerated"],
                true
            );
            assert_eq!(
                completed["result"]["_meta"]["com.agentshelf.artifact-mcp/progress"],
                json!({ "current": 2, "total": 2 })
            );
            let (_, updated) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "update-task",
                    "method": "tasks/update",
                    "params": {
                        "taskId": task_id,
                        "inputResponses": {},
                        "_meta": tasks_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tasks/update"),
                    ("mcp-name", task_id.as_str()),
                ],
            )
            .await;
            assert_eq!(updated["result"]["resultType"], "complete");

            let (_, created) = post(
                &router,
                json!({
                    "jsonrpc": "2.0",
                    "id": "create-task",
                    "method": "tools/call",
                    "params": {
                        "name": "regenerate_artifact_preview",
                        "arguments": { "id": artifact_id },
                        "_meta": tasks_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "regenerate_artifact_preview"),
                ],
            )
            .await;
            assert_eq!(created["result"]["resultType"], "task");
            assert_eq!(created["result"]["status"], "working");

            let (_, foreign) = post_as(
                &router,
                "foreign-secret",
                json!({
                    "jsonrpc": "2.0",
                    "id": "foreign-cancel",
                    "method": "tasks/cancel",
                    "params": {
                        "taskId": task_id,
                        "_meta": tasks_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tasks/cancel"),
                    ("mcp-name", task_id.as_str()),
                ],
            )
            .await;
            assert_eq!(foreign["error"]["code"], -32_602);
            assert!(
                foreign["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("Unknown task"))
            );

            let (_, foreign_regeneration) = post_as(
                &router,
                "foreign-secret",
                json!({
                    "jsonrpc": "2.0",
                    "id": "foreign-regenerate",
                    "method": "tools/call",
                    "params": {
                        "name": "regenerate_artifact_preview",
                        "arguments": { "id": artifact_id },
                        "_meta": tasks_meta(MODERN_VERSION)
                    }
                }),
                &[
                    ("mcp-protocol-version", MODERN_VERSION),
                    ("mcp-method", "tools/call"),
                    ("mcp-name", "regenerate_artifact_preview"),
                ],
            )
            .await;
            assert_eq!(foreign_regeneration["result"]["isError"], true);
            assert_eq!(
                foreign_regeneration["result"]["content"][0]["text"],
                format!("Unknown artifact: {artifact_id}")
            );
            Ok(())
        },
    )
    .await
    .expect("recover durable preview task after restart");
}
