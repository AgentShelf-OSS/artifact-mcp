//! Viewer actions are separate from read-only Connections queries.
use super::artifact::authorize;
use crate::{
    AppDeps,
    actions::{ActionGrant, dispatch, valid_name},
    security::access::AccessPolicy,
};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::json;

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/{id}/actions", get(manifest))
        .route("/{id}/actions/{action}", get(status).post(start))
}
fn response(code: StatusCode, value: serde_json::Value) -> Response {
    (code, [(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}
async fn grant(
    deps: &AppDeps,
    headers: &axum::http::HeaderMap,
    id: &str,
    action: &str,
) -> Result<(ActionGrant, crate::model::Viewer), Response> {
    let (artifact, viewer) = authorize(deps, headers, id)
        .await
        .map_err(IntoResponse::into_response)?;
    if !AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        return Err(response(
            StatusCode::FORBIDDEN,
            json!({"error":"forbidden"}),
        ));
    }
    deps.config
        .action_grants
        .iter()
        .find(|g| {
            g.artifact_id == id
                && g.action == action
                && g.org == artifact.meta().org.0
                && g.revision == artifact.meta().revision
        })
        .cloned()
        .map(|g| (g, viewer))
        .ok_or_else(|| response(StatusCode::NOT_FOUND, json!({"error":"action_unavailable"})))
}
async fn manifest(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, request.headers(), &id).await {
        Ok(pair) => pair,
        Err(e) => return e.into_response(),
    };
    let actions: Vec<&str> = if AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        deps.config
            .action_grants
            .iter()
            .filter(|g| {
                g.artifact_id == id
                    && g.org == artifact.meta().org.0
                    && g.revision == artifact.meta().revision
            })
            .map(|g| g.action.as_str())
            .collect()
    } else {
        vec![]
    };
    response(
        StatusCode::OK,
        json!({"enabled":!actions.is_empty(),"actions":actions}),
    )
}
async fn status(
    State(deps): State<AppDeps>,
    Path((id, action)): Path<(String, String)>,
    request: Request,
) -> Response {
    let g = match grant(&deps, request.headers(), &id, &action).await {
        Ok((g, _)) => g,
        Err(r) => return r,
    };
    match dispatch(&g, None).await {
        Ok(v) => response(StatusCode::OK, v),
        Err(_) => response(
            StatusCode::BAD_GATEWAY,
            json!({"error":"action_unavailable"}),
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    request_id: String,
}
async fn start(
    State(deps): State<AppDeps>,
    Path((id, action)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (g, viewer) = match grant(&deps, request.headers(), &id, &action).await {
        Ok(pair) => pair,
        Err(r) => return r,
    };
    let audit_id = request
        .extensions()
        .get::<crate::security::audit::AuditRequestId>()
        .cloned();
    let Ok(bytes) = axum::body::to_bytes(request.into_body(), 1024).await else {
        return response(StatusCode::BAD_REQUEST, json!({"error":"bad_params"}));
    };
    let Ok(input) = serde_json::from_slice::<Start>(&bytes) else {
        return response(StatusCode::BAD_REQUEST, json!({"error":"bad_params"}));
    };
    if !valid_name(&input.request_id) {
        return response(StatusCode::BAD_REQUEST, json!({"error":"bad_params"}));
    }
    let audit = match crate::security::audit::MutationAudit::viewer_with_request_id(
        &viewer,
        audit_id.as_ref(),
    )
    .and_then(|a| a.for_target_tenant(&g.org))
    {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    if let Some(ledger) = &deps.audit_access
        && ledger
            .record_live_action(
                audit.clone(),
                id.clone(),
                g.revision,
                "success",
                &format!("{}.requested", g.action),
            )
            .await
            .is_err()
    {
        return response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"action_unavailable"}),
        );
    }
    let result = dispatch(&g, Some(&input.request_id)).await;
    if let Some(ledger) = &deps.audit_access
        && ledger
            .record_live_action(
                audit,
                id,
                g.revision,
                if result.is_ok() { "success" } else { "failure" },
                &format!("{}.dispatched", g.action),
            )
            .await
            .is_err()
    {
        return response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"action_unavailable"}),
        );
    }
    match result {
        Ok(v) => response(StatusCode::ACCEPTED, v),
        Err(_) => response(
            StatusCode::BAD_GATEWAY,
            json!({"error":"action_unavailable"}),
        ),
    }
}
