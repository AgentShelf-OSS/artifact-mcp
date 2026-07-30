//! Authenticated POST/OPTIONS `/mcp` transport.

use std::time::Duration;

use axum::{
    Json, Router,
    body::HttpBody,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppDeps,
    http::ingress::{
        BodyReadError, McpCost, McpRequestPermit, discard_body_bounded, peer_addr,
        read_body_limited, validate_json_complexity,
    },
    mcp::{
        dispatch::{
            MODERN_PROTOCOL_VERSION, PROTOCOL_VERSION, ProtocolEra, SUPPORTED_PROTOCOL_VERSIONS,
        },
        protocol::{self, OrderedJson},
    },
    observability::{McpMetricLabels, McpOutcome, labels_for},
    security::{
        audit::with_mcp_request_id,
        oauth::{SUPPORTED_SCOPES, required_scope},
    },
};

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
        with_mcp_request_id(request_id.clone(), mcp_inner(&deps, request)).await;
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

async fn mcp_inner(deps: &AppDeps, request: Request) -> (Response, McpMetricLabels, McpOutcome) {
    // `admit` transfers this guard for MCP because only the parsed operation can distinguish a
    // cancellable read from a durable mutation. Retaining it in the spawned read task means a
    // client-facing 408 never turns into hidden concurrent work.
    let request_permit = request
        .extensions()
        .get::<McpRequestPermit>()
        .and_then(McpRequestPermit::take);
    let transport_headers = request.headers().clone();
    let peer = peer_addr(&request);
    let initial_labels = labels_for(protocol_dimension(&transport_headers), None, None);
    // This await happens before the body is read or bounded. `/mcp` is Access-bypassed, so the
    // publisher key is the only gate protecting the multi-megabyte JSON parser.
    let auth = match deps.publisher_auth.authenticate(request.headers()).await {
        Ok(auth) => auth,
        Err(_) => {
            deps.mcp_telemetry.record_security_signal("auth_failure");
            let rate_limited = !deps.ingress.allow_auth_failure_request(&request);
            // Authentication deliberately precedes parser work, but an HTTP/1 response must not
            // abandon an active upload. Consume and discard through the ingress deadline before
            // returning either fixed authentication rejection. This path intentionally performs
            // no parser buffering, regardless of MCP_JSON_LIMIT.
            let _ = discard_body_bounded(
                request.into_body(),
                Duration::from_millis(deps.config.ingress.read_timeout_ms),
            )
            .await;
            if rate_limited {
                deps.mcp_telemetry.record_security_signal("rate_limit");
                return observed(
                    deps.ingress.mcp_rate_limited_response(),
                    initial_labels,
                    McpOutcome::AuthenticationFailure,
                );
            }
            return observed(
                unauthorized_response(&deps.config),
                initial_labels,
                McpOutcome::AuthenticationFailure,
            );
        }
    };
    if !deps
        .ingress
        .allow_verified_publisher_request(&request, &auth)
    {
        deps.mcp_telemetry.record_security_signal("rate_limit");
        return observed(
            deps.ingress.mcp_rate_limited_response(),
            initial_labels,
            McpOutcome::AuthenticationFailure,
        );
    }
    let json_content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        });
    if !json_content_type {
        return observed(
            crate::http::ingress::mcp_admission_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, None),
            initial_labels,
            McpOutcome::ValidationFailure,
        );
    }
    let limit = usize::try_from(deps.config.body.mcp_json).unwrap_or(usize::MAX);
    let bytes = match read_body_limited(
        request.into_body(),
        limit,
        Duration::from_millis(deps.config.ingress.read_timeout_ms),
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(BodyReadError::Timeout) => {
            return observed(
                crate::http::ingress::mcp_admission_response(StatusCode::REQUEST_TIMEOUT, None),
                initial_labels,
                McpOutcome::ValidationFailure,
            );
        }
        Err(BodyReadError::TooLarge) => {
            return observed(
                crate::http::ingress::mcp_admission_response(StatusCode::PAYLOAD_TOO_LARGE, None),
                initial_labels,
                McpOutcome::ValidationFailure,
            );
        }
        Err(BodyReadError::Invalid) => {
            return observed(
                crate::http::ingress::mcp_admission_response(StatusCode::BAD_REQUEST, None),
                initial_labels,
                McpOutcome::ValidationFailure,
            );
        }
    };
    let payload = match serde_json::from_slice::<protocol::OrderedJson>(&bytes) {
        Ok(payload) => payload,
        Err(_) => {
            return observed(
                crate::http::ingress::mcp_admission_response(StatusCode::BAD_REQUEST, None),
                initial_labels,
                McpOutcome::ValidationFailure,
            );
        }
    };
    if validate_json_complexity(&payload, &deps.config.ingress).is_err() {
        return observed(
            crate::http::ingress::mcp_admission_response(StatusCode::PAYLOAD_TOO_LARGE, None),
            initial_labels,
            McpOutcome::ValidationFailure,
        );
    }
    if payload.as_array().is_some_and(|batch| {
        batch.len() > usize::try_from(deps.config.ingress.json_max_batch).unwrap_or(usize::MAX)
    }) {
        return observed(
            crate::http::ingress::mcp_admission_response(StatusCode::PAYLOAD_TOO_LARGE, None),
            initial_labels,
            McpOutcome::ValidationFailure,
        );
    }
    let era = match validate_transport(&payload, &transport_headers) {
        Ok(era) => era,
        Err(error) => {
            let labels = labels_for(
                protocol_dimension(&transport_headers),
                payload.get("method").and_then(OrderedJson::as_str),
                request_name(&payload),
            );
            return observed(
                (
                    StatusCode::BAD_REQUEST,
                    Json(protocol::rpc_error_with_data(
                        request_id(&payload),
                        error.code,
                        &error.message,
                        error.data,
                    )),
                )
                    .into_response(),
                labels,
                McpOutcome::ValidationFailure,
            );
        }
    };
    let method = payload
        .get("method")
        .and_then(OrderedJson::as_str)
        .unwrap_or_default();
    let name = request_name(&payload);
    let labels = labels_for(
        match era {
            ProtocolEra::Legacy => PROTOCOL_VERSION,
            ProtocolEra::Modern => MODERN_PROTOCOL_VERSION,
        },
        Some(method),
        name,
    );
    if let Some(batch) = payload.as_array() {
        // Legacy JSON-RPC retains read-only batches. Mixed or state-changing batches are
        // rejected atomically before dispatch so one cheap outer admission cannot buy writes.
        for message in batch {
            let method = message
                .get("method")
                .and_then(OrderedJson::as_str)
                .unwrap_or_default();
            let name = request_name(message);
            if mcp_cost(method, name) != McpCost::Read {
                return observed(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(protocol::rpc_error(
                            Value::Null,
                            -32_600,
                            "Legacy batches may contain read-only requests only",
                        )),
                    )
                        .into_response(),
                    labels,
                    McpOutcome::ValidationFailure,
                );
            }
            if let Some(scope) = required_scope(method, name)
                && !auth.has_scope(scope)
            {
                return observed(
                    insufficient_scope_response(&deps.config, request_id(message), scope),
                    labels,
                    McpOutcome::AuthorizationFailure,
                );
            }
        }
        if !deps.ingress.allow_verified_publisher_operation_weight(
            &transport_headers,
            peer,
            &auth,
            McpCost::Read,
            batch.len() as u64,
        ) {
            deps.mcp_telemetry.record_security_signal("rate_limit");
            return observed(
                deps.ingress.mcp_operation_rate_limited_response(),
                labels,
                McpOutcome::AuthenticationFailure,
            );
        }
        return dispatch_read_with_deadline(deps, payload, auth, era, request_permit, labels).await;
    }
    if let Some(scope) = required_scope(method, name)
        && !auth.has_scope(scope)
    {
        return observed(
            insufficient_scope_response(&deps.config, request_id(&payload), scope),
            labels,
            McpOutcome::AuthorizationFailure,
        );
    }
    let operation_cost = mcp_cost(method, name);
    if !deps.ingress.allow_verified_publisher_operation(
        &transport_headers,
        peer,
        &auth,
        operation_cost,
    ) {
        deps.mcp_telemetry.record_security_signal("rate_limit");
        return observed(
            deps.ingress.mcp_operation_rate_limited_response(),
            labels,
            McpOutcome::AuthenticationFailure,
        );
    }
    if operation_cost == McpCost::Read {
        return dispatch_read_with_deadline(deps, payload, auth, era, request_permit, labels).await;
    }
    let _mutation_permit = match deps.ingress.try_acquire_mutation() {
        Some(permit) => permit,
        None => {
            return observed(
                crate::http::ingress::mcp_admission_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    Some(1),
                ),
                labels,
                McpOutcome::ValidationFailure,
            );
        }
    };
    match protocol::handle_mcp_for_era(payload, &auth, deps, era).await {
        Some(response) => {
            let outcome = response_outcome(&response);
            let status = if era == ProtocolEra::Modern
                && response
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_i64)
                    == Some(-32_601)
            {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            };
            observed((status, Json(response)).into_response(), labels, outcome)
        }
        None => observed(
            StatusCode::ACCEPTED.into_response(),
            labels,
            McpOutcome::Success,
        ),
    }
}

/// Run a parsed MCP read behind a client-facing deadline without cancelling the underlying
/// operation. The task owns the admission permit until the service call has actually completed.
/// Writes never call this helper: their request and mutation permits remain in the handler until
/// durable execution returns.
async fn dispatch_read_with_deadline(
    deps: &AppDeps,
    payload: protocol::OrderedJson,
    auth: crate::model::PublisherIdentity,
    era: ProtocolEra,
    request_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    labels: McpMetricLabels,
) -> (Response, McpMetricLabels, McpOutcome) {
    let deadline = Duration::from_millis(deps.config.ingress.read_handler_timeout_ms);
    let task_deps = deps.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // Keep this binding in the task rather than the waiter. If the client times out, the
        // task still holds its capacity until the read service returns.
        let _request_permit = request_permit;
        let response = protocol::handle_mcp_for_era(payload, &auth, &task_deps, era).await;
        let _ = send.send(response);
    });
    match tokio::time::timeout(deadline, receive).await {
        Ok(Ok(response)) => dispatch_response(response, era, labels),
        Ok(Err(_)) => observed(
            crate::http::ingress::mcp_admission_response(StatusCode::SERVICE_UNAVAILABLE, Some(1)),
            labels,
            McpOutcome::ServerFailure,
        ),
        Err(_) => observed(
            crate::http::ingress::mcp_admission_response(StatusCode::REQUEST_TIMEOUT, None),
            labels,
            McpOutcome::Cancelled,
        ),
    }
}

fn dispatch_response(
    response: Option<Value>,
    era: ProtocolEra,
    labels: McpMetricLabels,
) -> (Response, McpMetricLabels, McpOutcome) {
    match response {
        Some(response) => {
            let outcome = response_outcome(&response);
            let status = if era == ProtocolEra::Modern
                && response
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_i64)
                    == Some(-32_601)
            {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            };
            observed((status, Json(response)).into_response(), labels, outcome)
        }
        None => observed(
            StatusCode::ACCEPTED.into_response(),
            labels,
            McpOutcome::Success,
        ),
    }
}

fn mcp_cost(method: &str, name: Option<&str>) -> McpCost {
    match method {
        "tools/call" => match name {
            Some("publish_artifact" | "publish_bundle" | "update_artifact") => McpCost::Upload,
            Some(
                "patch_artifact"
                | "delete_artifact"
                | "set_visibility"
                | "set_category"
                | "create_category"
                | "delete_category"
                | "create_share"
                | "revoke_share"
                | "restore_artifact"
                | "resolve_feedback"
                | "reopen_feedback"
                | "submit_feedback"
                | "regenerate_artifact_preview",
            ) => McpCost::Mutation,
            Some(
                "list_artifacts" | "read_artifact" | "list_categories" | "list_revisions"
                | "list_shares" | "artifact_stats" | "list_feedback",
            ) => McpCost::Read,
            // An unrecognised tool may be added by a newer server. Reserve mutation capacity
            // until its semantics are explicitly classified rather than silently admitting a
            // future write as a read.
            _ => McpCost::Mutation,
        },
        "tasks/cancel" | "tasks/update" => McpCost::Mutation,
        "initialize"
        | "ping"
        | "notifications/initialized"
        | "server/discover"
        | "tools/list"
        | "resources/list"
        | "resources/templates/list"
        | "resources/read"
        | "tasks/get" => McpCost::Read,
        // Unknown protocol methods receive the conservative class too. They are rejected by
        // dispatch today; this keeps a later state-changing addition from bypassing permits.
        _ => McpCost::Mutation,
    }
}

fn observed(
    response: Response,
    labels: McpMetricLabels,
    outcome: McpOutcome,
) -> (Response, McpMetricLabels, McpOutcome) {
    (response, labels, outcome)
}

fn protocol_dimension(headers: &HeaderMap) -> &'static str {
    match headers
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
    {
        Some(MODERN_PROTOCOL_VERSION) => MODERN_PROTOCOL_VERSION,
        Some(PROTOCOL_VERSION) | None => PROTOCOL_VERSION,
        Some(_) => "unsupported",
    }
}

fn request_name(payload: &OrderedJson) -> Option<&str> {
    let method = payload.get("method").and_then(OrderedJson::as_str);
    payload
        .get("params")
        .and_then(|params| {
            if method == Some("resources/read") {
                params.get("uri")
            } else if method.is_some_and(|method| method.starts_with("tasks/")) {
                params.get("taskId")
            } else {
                params.get("name")
            }
        })
        .and_then(OrderedJson::as_str)
}

fn response_outcome(response: &Value) -> McpOutcome {
    let Some(error) = response.get("error") else {
        return McpOutcome::Success;
    };
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match code {
        Some(-32_603)
            if message.contains("output failed validation")
                || message.contains("output schema")
                || message.contains("structured content") =>
        {
            McpOutcome::OutputValidationFailure
        }
        Some(-32_603) => McpOutcome::ServerFailure,
        Some(-32_602) | Some(-32_600) | Some(-32_700) => McpOutcome::ValidationFailure,
        _ => McpOutcome::ProtocolError,
    }
}

async fn metrics(State(deps): State<AppDeps>) -> Response {
    let mut metrics = deps.mcp_telemetry.render_prometheus();
    metrics.push_str(&deps.ingress.render_prometheus());
    metrics.push_str(&deps.delivery_telemetry.render_prometheus());
    metrics.push_str(&crate::integrations::discord_gateway_runtime::render_prometheus());
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

#[derive(Debug, PartialEq)]
struct TransportValidationError {
    code: i32,
    message: String,
    data: Option<Value>,
}

impl TransportValidationError {
    fn header(message: impl Into<String>) -> Self {
        Self {
            code: -32_020,
            message: format!("Header mismatch: {}", message.into()),
            data: None,
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32_602,
            message: message.into(),
            data: None,
        }
    }

    fn unsupported(requested: &str) -> Self {
        Self {
            code: -32_022,
            message: "Unsupported protocol version".to_owned(),
            data: Some(json!({
                "supported": SUPPORTED_PROTOCOL_VERSIONS,
                "requested": requested
            })),
        }
    }

    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: -32_600,
            message: message.into(),
            data: None,
        }
    }
}

fn validate_transport(
    payload: &OrderedJson,
    headers: &HeaderMap,
) -> Result<ProtocolEra, TransportValidationError> {
    let header_version = optional_header(headers, "mcp-protocol-version")?;
    let has_modern_metadata = contains_modern_request_metadata(payload);
    let method = payload.get("method").and_then(OrderedJson::as_str);
    let modern_intent = header_version
        .as_deref()
        .is_some_and(|version| version == MODERN_PROTOCOL_VERSION)
        || has_modern_metadata
        || method == Some("server/discover");

    if !modern_intent {
        return Ok(ProtocolEra::Legacy);
    }
    if payload.as_array().is_some() {
        return Err(TransportValidationError::invalid_request(
            "Batch requests are not supported by MCP 2026-07-28",
        ));
    }
    let method = method.ok_or_else(|| {
        TransportValidationError::invalid_request("Invalid Request: method must be a string")
    })?;
    let header_version = header_version.ok_or_else(|| {
        TransportValidationError::header("required MCP-Protocol-Version header is missing")
    })?;
    let body_version = request_meta(payload)
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| {
            TransportValidationError::invalid_params(
                "Missing required request metadata: io.modelcontextprotocol/protocolVersion",
            )
        })?;
    if header_version != body_version {
        return Err(TransportValidationError::header(format!(
            "MCP-Protocol-Version header value '{header_version}' does not match body value '{body_version}'"
        )));
    }
    if body_version != MODERN_PROTOCOL_VERSION {
        return Err(TransportValidationError::unsupported(body_version));
    }
    if request_meta(payload)
        .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
        .and_then(OrderedJson::as_object)
        .is_none()
    {
        return Err(TransportValidationError::invalid_params(
            "Missing required request metadata: io.modelcontextprotocol/clientCapabilities",
        ));
    }

    let header_method = optional_header(headers, "mcp-method")?
        .ok_or_else(|| TransportValidationError::header("required Mcp-Method header is missing"))?;
    if header_method != method {
        return Err(TransportValidationError::header(format!(
            "Mcp-Method header value '{header_method}' does not match body value '{method}'"
        )));
    }

    if matches!(
        method,
        "tools/call"
            | "resources/read"
            | "prompts/get"
            | "tasks/get"
            | "tasks/update"
            | "tasks/cancel"
    ) {
        let body_name = payload
            .get("params")
            .and_then(|params| {
                if method == "resources/read" {
                    params.get("uri")
                } else if method.starts_with("tasks/") {
                    params.get("taskId")
                } else {
                    params.get("name")
                }
            })
            .and_then(OrderedJson::as_str)
            .ok_or_else(|| {
                TransportValidationError::invalid_params(format!(
                    "{method} requires a string params.{}",
                    if method == "resources/read" {
                        "uri"
                    } else if method.starts_with("tasks/") {
                        "taskId"
                    } else {
                        "name"
                    }
                ))
            })?;
        let raw_header_name = optional_header(headers, "mcp-name")?.ok_or_else(|| {
            TransportValidationError::header("required Mcp-Name header is missing")
        })?;
        let header_name = decode_header_value(&raw_header_name)
            .ok_or_else(|| TransportValidationError::header("Mcp-Name header is malformed"))?;
        if header_name != body_name {
            return Err(TransportValidationError::header(format!(
                "Mcp-Name header value '{header_name}' does not match body value '{body_name}'"
            )));
        }
    }

    Ok(ProtocolEra::Modern)
}

fn optional_header(
    headers: &HeaderMap,
    name: &'static str,
) -> Result<Option<String>, TransportValidationError> {
    headers
        .get(name)
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                TransportValidationError::header(format!("{name} header is malformed"))
            })
        })
        .transpose()
}

fn contains_modern_request_metadata(payload: &OrderedJson) -> bool {
    match payload {
        OrderedJson::Array(messages) => messages.iter().any(contains_modern_request_metadata),
        _ => request_meta(payload).is_some_and(|meta| {
            meta.get("io.modelcontextprotocol/protocolVersion")
                .is_some()
        }),
    }
}

fn request_meta(payload: &OrderedJson) -> Option<&OrderedJson> {
    payload.get("params").and_then(|params| params.get("_meta"))
}

fn decode_header_value(value: &str) -> Option<String> {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|value| value.strip_suffix("?="))
    else {
        return Some(value.to_owned());
    };
    let decoded = STANDARD.decode(encoded).ok()?;
    String::from_utf8(decoded).ok()
}

fn request_id(payload: &OrderedJson) -> Value {
    payload
        .get("id")
        .cloned()
        .map_or(Value::Null, OrderedJson::into_value)
}

fn unauthorized_response(config: &crate::config::AppConfig) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(protocol::rpc_error(
            serde_json::Value::Null,
            -32_001,
            "unauthorized",
        )),
    )
        .into_response();
    if config.oauth.enabled()
        && let Ok(challenge) = HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}\", scope=\"{}\"",
            config.oauth_resource_metadata_uri(),
            SUPPORTED_SCOPES.join(" ")
        ))
    {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, challenge);
    }
    response
}

fn insufficient_scope_response(
    config: &crate::config::AppConfig,
    id: Value,
    scope: &str,
) -> Response {
    let mut response = (
        StatusCode::FORBIDDEN,
        Json(protocol::rpc_error_with_data(
            id,
            -32_003,
            "insufficient_scope",
            Some(json!({ "requiredScope": scope })),
        )),
    )
        .into_response();
    if let Ok(challenge) = HeaderValue::from_str(&format!(
        "Bearer error=\"insufficient_scope\", scope=\"{scope}\", resource_metadata=\"{}\"",
        config.oauth_resource_metadata_uri()
    )) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, challenge);
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use std::time::Duration;

    use axum::body::to_bytes;

    use super::{insufficient_scope_response, mcp_cost};
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
}
