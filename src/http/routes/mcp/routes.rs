//! Authenticated POST/OPTIONS `/mcp` transport.

use axum::{
    Json, Router,
    body::HttpBody,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    AppDeps,
    security::{audit::with_mcp_request_id, oauth::SUPPORTED_SCOPES},
};

use super::admission;

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/mcp", post(mcp).options(mcp_options))
        .route("/audit/events", get(audit_events))
        .route("/audit/export", get(audit_export))
        .route("/metrics", get(metrics))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth_protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(oauth_protected_resource),
        )
}

#[derive(Deserialize)]
struct AuditEventsParams {
    tenant: Option<String>,
    cursor: Option<String>,
    limit: Option<u64>,
}

async fn audit_route_auth(
    deps: &AppDeps,
    headers: &HeaderMap,
) -> Result<crate::model::PublisherIdentity, Response> {
    deps.publisher_auth
        .authenticate(headers)
        .await
        .map_err(|error| {
            deps.mcp_telemetry.record_security_signal("auth_failure");
            error.into_response()
        })
}

/// A deliberately small, authenticated read-only operational surface. It accepts no actor fields
/// or secrets in query parameters; the verified publisher projection and configured audit key are
/// the only authority inputs. Legacy API keys are rejected by the audit capability gate.
async fn audit_events(
    State(deps): State<AppDeps>,
    Query(params): Query<AuditEventsParams>,
    request: Request,
) -> Response {
    let auth = match audit_route_auth(&deps, request.headers()).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let Some(audit) = deps.audit_access.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match audit
        .query(
            &auth,
            crate::security::audit::AuditQuery {
                tenant: params.tenant,
                cursor: params.cursor,
                limit: params.limit,
            },
        )
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn audit_export(
    State(deps): State<AppDeps>,
    Query(params): Query<AuditEventsParams>,
    request: Request,
) -> Response {
    let auth = match audit_route_auth(&deps, request.headers()).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let Some(audit) = deps.audit_access.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match audit
        .export(
            &auth,
            crate::security::audit::AuditExportQuery {
                tenant: params.tenant,
                cursor: params.cursor,
                limit: params.limit,
            },
        )
        .await
    {
        Ok(export) => {
            let mut response = (
                [
                    (
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
                    ),
                    (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                ],
                export.ndjson,
            )
                .into_response();
            if let Some(next) = export
                .next
                .and_then(|value| HeaderValue::from_str(&value).ok())
            {
                response.headers_mut().insert("x-audit-next", next);
            }
            if export.truncated {
                response
                    .headers_mut()
                    .insert("x-audit-truncated", HeaderValue::from_static("true"));
            }
            if let Some(reason) = export.reason {
                response
                    .headers_mut()
                    .insert("x-audit-export-reason", HeaderValue::from_static(reason));
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

async fn mcp(State(deps): State<AppDeps>, request: Request) -> Response {
    let mut observation = deps.mcp_telemetry.begin();
    let request_id = observation.request_id().to_owned();
    let (mut response, labels, outcome) =
        match with_mcp_request_id(request_id.clone(), admission::mcp_inner(&deps, request)).await {
            Ok(observed) => observed,
            Err(rejected) => *rejected,
        };
    observation.set_labels(labels);
    let result_bytes = response
        .body()
        .size_hint()
        .exact()
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or_default();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    observation.finish(outcome, result_bytes);
    response
}

async fn metrics(State(deps): State<AppDeps>) -> Response {
    let mut metrics = deps.mcp_telemetry.render_prometheus();
    metrics.push_str(&deps.ingress.render_prometheus());
    metrics.push_str(&deps.delivery_telemetry.render_prometheus());
    metrics.push_str(&crate::integrations::discord_gateway_runtime::render_prometheus());
    metrics.push_str(
        &deps
            .push
            .as_ref()
            .map(|push| push.telemetry().render_prometheus())
            .unwrap_or_else(|| {
                crate::integrations::push_runtime::PushTelemetry::default().render_prometheus()
            }),
    );
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        metrics,
    )
        .into_response()
}

async fn oauth_protected_resource(State(deps): State<AppDeps>) -> Response {
    if !deps.config.oauth.enabled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(json!({
        "resource": deps.config.mcp_resource_uri(),
        "authorization_servers": [deps.config.oauth.issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": SUPPORTED_SCOPES
    }))
    .into_response()
}

async fn mcp_options() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "authorization, content-type, accept, mcp-protocol-version, mcp-method, mcp-name",
        ),
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use std::time::Duration;

    use axum::body::to_bytes;

    use super::super::admission::{
        insufficient_scope_response, mcp_cost, modern_transport, validate_transport,
    };
    use crate::{
        config::AppConfig,
        http::ingress::{BodyReadError, McpCost, read_body_limited},
        security::oauth::{
            SCOPE_AUDIT_EXPORT, SCOPE_AUDIT_GLOBAL, SCOPE_AUDIT_READ, SCOPE_READ, SUPPORTED_SCOPES,
            required_scope,
        },
    };

    #[test]
    fn publish_tools_use_the_stricter_upload_budget() {
        assert_eq!(
            mcp_cost("tools/call", Some("publish_artifact")),
            McpCost::Upload
        );
        assert_eq!(
            mcp_cost("tools/call", Some("delete_artifact")),
            McpCost::Mutation
        );
        assert_eq!(mcp_cost("resources/read", None), McpCost::Read);
    }

    #[test]
    fn audit_scopes_are_advertised_for_the_live_audit_routes() {
        for scope in [SCOPE_AUDIT_READ, SCOPE_AUDIT_EXPORT, SCOPE_AUDIT_GLOBAL] {
            assert!(SUPPORTED_SCOPES.contains(&scope));
        }
    }

    #[test]
    fn every_state_changing_dispatch_path_uses_mutation_admission() {
        for tool in [
            "patch_artifact",
            "delete_artifact",
            "set_visibility",
            "set_category",
            "create_category",
            "delete_category",
            "create_share",
            "revoke_share",
            "restore_artifact",
            "resolve_feedback",
            "reopen_feedback",
            "submit_feedback",
            "regenerate_artifact_preview",
        ] {
            assert_eq!(
                mcp_cost("tools/call", Some(tool)),
                McpCost::Mutation,
                "{tool}"
            );
        }
        assert_eq!(mcp_cost("tasks/cancel", None), McpCost::Mutation);
        assert_eq!(mcp_cost("tasks/update", None), McpCost::Mutation);
        assert_eq!(mcp_cost("future/write", None), McpCost::Mutation);
    }

    #[tokio::test]
    async fn scoped_read_without_its_required_scope_returns_the_json_rpc_scope_envelope() {
        assert_eq!(required_scope("resources/read", None), Some(SCOPE_READ));
        let response = insufficient_scope_response(
            &AppConfig::default(),
            serde_json::json!("read-id"),
            SCOPE_READ,
        );
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
        let payload = serde_json::from_slice::<serde_json::Value>(
            &to_bytes(response.into_body(), 4_096)
                .await
                .expect("read scope envelope"),
        )
        .expect("JSON-RPC scope envelope");
        assert_eq!(payload["jsonrpc"], "2.0");
        assert_eq!(payload["id"], "read-id");
        assert_eq!(payload["error"]["message"], "insufficient_scope");
        assert_eq!(payload["error"]["data"]["requiredScope"], SCOPE_READ);
    }

    #[tokio::test]
    async fn request_body_limit_accepts_the_exact_boundary() {
        let body = read_body_limited(Body::from("1234"), 4, Duration::from_secs(1))
            .await
            .expect("read request body");
        assert_eq!(body, b"1234".to_vec());
    }

    #[tokio::test]
    async fn oversized_request_body_is_drained_and_reported_after_the_boundary() {
        let body = read_body_limited(Body::from("12345"), 4, Duration::from_secs(1)).await;
        assert_eq!(body, Err(BodyReadError::TooLarge));
    }

    #[test]
    fn modern_metadata_selects_modern_transport_without_a_version_header() {
        let payload = serde_json::json!({
            "method": "tools/list",
            "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2025-03-26"}}
        });
        let headers = axum::http::HeaderMap::new();

        assert!(modern_transport(&payload, &headers));
        assert_eq!(
            validate_transport(&payload, &headers),
            Ok(crate::mcp::dispatch::ProtocolEra::Modern)
        );
    }
}
