//! Authenticated POST/OPTIONS `/mcp` transport.

use std::time::Duration;

use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::{
    AppDeps,
    http::ingress::{
        BodyReadError, McpCost, McpRequestPermit, discard_body_bounded, peer_addr,
        read_body_limited, validate_json_complexity,
    },
    mcp::{
        dispatch::{MODERN_PROTOCOL_VERSION, PROTOCOL_VERSION, ProtocolEra},
        protocol::{self, OrderedJson},
    },
    observability::{McpMetricLabels, McpOutcome, labels_for},
    security::oauth::required_scope,
};

use super::transport;
type Observed = (Response, McpMetricLabels, McpOutcome);

/// An admission step's rejection. Boxed so every step's `Result` stays small; rejections are off
/// the success path.
type Rejected = Box<Observed>;

fn rejected(response: Response, labels: McpMetricLabels, outcome: McpOutcome) -> Rejected {
    Box::new((response, labels, outcome))
}

/// The `/mcp` admission pipeline. Every step returns its successor's input or the observed
/// rejection, and the order is a security contract: nothing reads the body before publisher
/// authentication, and nothing dispatches before every admission check has passed.
///
/// Guard lifetimes: `request_permit` lives in this frame until it moves into a read task, so a
/// rejected or mutating request holds it until this function returns. `_mutation_permit` is a
/// named binding (not `_`), so it is held across the mutation dispatch.
pub(super) async fn mcp_inner(deps: &AppDeps, request: Request) -> Result<Observed, Rejected> {
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
    let (auth, request) = authenticate_publisher(deps, request, initial_labels).await?;
    admit_verified_publisher(deps, &request, &auth, initial_labels)?;
    require_json_media_type(&request, initial_labels)?;
    let bytes = read_mcp_body(deps, request, initial_labels).await?;
    let payload = parse_mcp_json(&bytes, initial_labels)?;
    enforce_json_limits(deps, &payload, initial_labels)?;
    let era = validate_era(&payload, &transport_headers)?;
    let method = payload
        .get("method")
        .and_then(OrderedJson::as_str)
        .unwrap_or_default();
    let name = request_name(&payload);
    let labels = era_labels(era, method, name);
    if let Some(batch) = payload.as_array() {
        admit_legacy_batch(deps, batch, era, &auth, &transport_headers, peer, labels)?;
        return Ok(
            dispatch_read_with_deadline(deps, payload, auth, era, request_permit, labels).await,
        );
    }
    reject_invalid_request(&payload, era, labels)?;
    require_scope(deps, &auth, &payload, method, name, labels)?;
    let operation_cost = mcp_cost(method, name);
    admit_operation(
        deps,
        &transport_headers,
        peer,
        &auth,
        operation_cost,
        labels,
    )?;
    if operation_cost == McpCost::Read {
        return Ok(
            dispatch_read_with_deadline(deps, payload, auth, era, request_permit, labels).await,
        );
    }
    let _mutation_permit = acquire_mutation_permit(deps, labels)?;
    let response = protocol::handle_mcp_for_era(payload, &auth, deps, era).await;
    Ok(dispatch_response(response, era, labels))
}

/// Authenticate the publisher before the body is read or bounded. `/mcp` is Access-bypassed,
/// so the publisher key is the only gate protecting the multi-megabyte JSON parser.
async fn authenticate_publisher(
    deps: &AppDeps,
    request: Request,
    labels: McpMetricLabels,
) -> Result<(crate::model::PublisherIdentity, Request), Rejected> {
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
                return Err(rejected(
                    deps.ingress.mcp_rate_limited_response(),
                    labels,
                    McpOutcome::AuthenticationFailure,
                ));
            }
            return Err(rejected(
                transport::unauthorized_response(&deps.config),
                labels,
                McpOutcome::AuthenticationFailure,
            ));
        }
    };
    Ok((auth, request))
}

/// Spend the verified publisher's request budget before any body work.
fn admit_verified_publisher(
    deps: &AppDeps,
    request: &Request,
    auth: &crate::model::PublisherIdentity,
    labels: McpMetricLabels,
) -> Result<(), Rejected> {
    if !deps.ingress.allow_verified_publisher_request(request, auth) {
        deps.mcp_telemetry.record_security_signal("rate_limit");
        return Err(rejected(
            deps.ingress.mcp_rate_limited_response(),
            labels,
            McpOutcome::AuthenticationFailure,
        ));
    }
    Ok(())
}

fn require_json_media_type(request: &Request, labels: McpMetricLabels) -> Result<(), Rejected> {
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
        return Err(rejected(
            crate::http::ingress::mcp_admission_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, None),
            labels,
            McpOutcome::ValidationFailure,
        ));
    }
    Ok(())
}

/// Read the body under the configured MCP JSON limit and the ingress read deadline.
async fn read_mcp_body(
    deps: &AppDeps,
    request: Request,
    labels: McpMetricLabels,
) -> Result<Vec<u8>, Rejected> {
    let limit = usize::try_from(deps.config.body.mcp_json).unwrap_or(usize::MAX);
    read_body_limited(
        request.into_body(),
        limit,
        Duration::from_millis(deps.config.ingress.read_timeout_ms),
    )
    .await
    .map_err(|error| {
        let status = match error {
            BodyReadError::Timeout => StatusCode::REQUEST_TIMEOUT,
            BodyReadError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            BodyReadError::Invalid => StatusCode::BAD_REQUEST,
        };
        rejected(
            crate::http::ingress::mcp_admission_response(status, None),
            labels,
            McpOutcome::ValidationFailure,
        )
    })
}

fn parse_mcp_json(bytes: &[u8], labels: McpMetricLabels) -> Result<OrderedJson, Rejected> {
    serde_json::from_slice::<OrderedJson>(bytes).map_err(|_| {
        rejected(
            crate::http::ingress::mcp_admission_response(StatusCode::BAD_REQUEST, None),
            labels,
            McpOutcome::ValidationFailure,
        )
    })
}

/// Bound the parsed document's complexity, then the JSON-RPC batch length.
fn enforce_json_limits(
    deps: &AppDeps,
    payload: &OrderedJson,
    labels: McpMetricLabels,
) -> Result<(), Rejected> {
    let too_large = || {
        rejected(
            crate::http::ingress::mcp_admission_response(StatusCode::PAYLOAD_TOO_LARGE, None),
            labels,
            McpOutcome::ValidationFailure,
        )
    };
    if validate_json_complexity(payload, &deps.config.ingress).is_err() {
        return Err(too_large());
    }
    if payload.as_array().is_some_and(|batch| {
        batch.len() > usize::try_from(deps.config.ingress.json_max_batch).unwrap_or(usize::MAX)
    }) {
        return Err(too_large());
    }
    Ok(())
}

/// Validate the transport headers and request metadata, and select the protocol era.
fn validate_era(payload: &OrderedJson, headers: &HeaderMap) -> Result<ProtocolEra, Rejected> {
    transport::validate_transport(payload, headers).map_err(|error| {
        let labels = labels_for(
            protocol_dimension(headers),
            payload.get("method").and_then(OrderedJson::as_str),
            request_name(payload),
        );
        rejected(
            (
                StatusCode::BAD_REQUEST,
                Json(protocol::rpc_error_with_data_for_era(
                    transport::request_id(payload),
                    error.code,
                    &error.message,
                    error.data,
                    if transport::modern_transport(payload, headers) {
                        ProtocolEra::Modern
                    } else {
                        ProtocolEra::Legacy
                    },
                )),
            )
                .into_response(),
            labels,
            McpOutcome::ValidationFailure,
        )
    })
}

fn era_labels(era: ProtocolEra, method: &str, name: Option<&str>) -> McpMetricLabels {
    labels_for(
        match era {
            ProtocolEra::Legacy => PROTOCOL_VERSION,
            ProtocolEra::Modern => MODERN_PROTOCOL_VERSION,
        },
        Some(method),
        name,
    )
}

/// Legacy JSON-RPC retains read-only batches. Mixed or state-changing batches are rejected
/// atomically before dispatch so one cheap outer admission cannot buy writes. The whole batch
/// then spends one weighted read budget.
fn admit_legacy_batch(
    deps: &AppDeps,
    batch: &[OrderedJson],
    era: ProtocolEra,
    auth: &crate::model::PublisherIdentity,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    labels: McpMetricLabels,
) -> Result<(), Rejected> {
    for message in batch {
        if protocol::invalid_request(message, era).is_some() {
            continue;
        }
        let method = message
            .get("method")
            .and_then(OrderedJson::as_str)
            .unwrap_or_default();
        let name = request_name(message);
        if mcp_cost(method, name) != McpCost::Read {
            return Err(rejected(
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
            ));
        }
        require_scope(deps, auth, message, method, name, labels)?;
    }
    if !deps.ingress.allow_verified_publisher_operation_weight(
        headers,
        peer,
        auth,
        McpCost::Read,
        batch.len() as u64,
    ) {
        deps.mcp_telemetry.record_security_signal("rate_limit");
        return Err(rejected(
            deps.ingress.mcp_operation_rate_limited_response(),
            labels,
            McpOutcome::AuthenticationFailure,
        ));
    }
    Ok(())
}

fn reject_invalid_request(
    payload: &OrderedJson,
    era: ProtocolEra,
    labels: McpMetricLabels,
) -> Result<(), Rejected> {
    if let Some(error) = protocol::invalid_request(payload, era) {
        return Err(rejected(
            (StatusCode::OK, Json(error)).into_response(),
            labels,
            McpOutcome::ValidationFailure,
        ));
    }
    Ok(())
}

/// Require the scope this method needs; the rejection echoes `message`'s JSON-RPC id.
fn require_scope(
    deps: &AppDeps,
    auth: &crate::model::PublisherIdentity,
    message: &OrderedJson,
    method: &str,
    name: Option<&str>,
    labels: McpMetricLabels,
) -> Result<(), Rejected> {
    if let Some(scope) = required_scope(method, name)
        && !auth.has_scope(scope)
    {
        return Err(rejected(
            transport::insufficient_scope_response(
                &deps.config,
                transport::request_id(message),
                scope,
            ),
            labels,
            McpOutcome::AuthorizationFailure,
        ));
    }
    Ok(())
}

/// Spend the verified publisher's budget for this operation class.
fn admit_operation(
    deps: &AppDeps,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    auth: &crate::model::PublisherIdentity,
    cost: McpCost,
    labels: McpMetricLabels,
) -> Result<(), Rejected> {
    if !deps
        .ingress
        .allow_verified_publisher_operation(headers, peer, auth, cost)
    {
        deps.mcp_telemetry.record_security_signal("rate_limit");
        return Err(rejected(
            deps.ingress.mcp_operation_rate_limited_response(),
            labels,
            McpOutcome::AuthenticationFailure,
        ));
    }
    Ok(())
}

/// A durable mutation needs one of the bounded mutation permits; the caller must hold the
/// returned permit until dispatch completes.
fn acquire_mutation_permit(
    deps: &AppDeps,
    labels: McpMetricLabels,
) -> Result<tokio::sync::OwnedSemaphorePermit, Rejected> {
    deps.ingress.try_acquire_mutation().ok_or_else(|| {
        rejected(
            crate::http::ingress::mcp_admission_response(StatusCode::SERVICE_UNAVAILABLE, Some(1)),
            labels,
            McpOutcome::ValidationFailure,
        )
    })
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

pub(super) fn mcp_cost(method: &str, name: Option<&str>) -> McpCost {
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
                "list_data_sources" | "get_data_bindings" | "list_artifacts" | "read_artifact"
                | "list_categories" | "list_revisions" | "list_shares" | "artifact_stats"
                | "list_feedback",
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
