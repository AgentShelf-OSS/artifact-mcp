//! Organization and per-viewer state HTTP API.

use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

use crate::{
    AppDeps,
    error::AppError,
    http::routes::artifact::{authorize, json_error, parse_json_request},
    mcp::protocol::OrderedJson,
    persistence::state::{self, StateError, StateScope, StateValue},
    security::audit::{AuditRequestId, MutationAudit},
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new().route("/{id}/state", get(list_state)).route(
        "/{id}/state/{key}",
        get(get_state).put(put_state).delete(delete_state),
    )
}

#[derive(Serialize)]
struct KeyResponse {
    key: String,
    revision: u64,
    updated_at: String,
}

fn scope_query(uri: &axum::http::Uri) -> Result<StateScope, &'static str> {
    let mut found = None;
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
        if key == "scope" {
            if found.is_some() {
                return Err("bad_scope");
            }
            found = Some(value.into_owned());
        } else if key.starts_with("scope[") {
            return Err("bad_scope");
        }
    }
    match found.as_deref().unwrap_or("org") {
        "org" => Ok(StateScope::Org),
        "viewer" => Ok(StateScope::Viewer),
        _ => Err("bad_scope"),
    }
}

impl From<StateValue> for KeyResponse {
    fn from(value: StateValue) -> Self {
        Self {
            key: value.key,
            revision: value.revision,
            updated_at: value.updated_at.0,
        }
    }
}

#[derive(Serialize)]
struct ValueResponse {
    key: String,
    value: OrderedJson,
    revision: u64,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_by: Option<String>,
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn list_state(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, request.headers(), &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let scope = match scope_query(request.uri()) {
        Ok(s) => s,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error),
    };
    let owner = viewer.email.clone();
    if scope == StateScope::Viewer && owner.is_none() {
        return AppError::ConcealedNotFound.into_response();
    }
    match deps.viewer_state.list(artifact, scope, owner).await {
        Ok(keys) => no_store(
            Json(
                serde_json::json!({ "keys": keys.into_iter().map(|key| KeyResponse {
            key: key.key, revision: key.revision, updated_at: key.updated_at.0,
        }).collect::<Vec<_>>() }),
            )
            .into_response(),
        ),
        Err(error) => error.into_response(),
    }
}

async fn get_state(
    State(deps): State<AppDeps>,
    Path((id, key)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, request.headers(), &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let scope = match scope_query(request.uri()) {
        Ok(s) => s,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error),
    };
    if !state::valid_key(&key) {
        return json_error(StatusCode::BAD_REQUEST, "bad_key");
    }
    let owner = viewer.email.clone();
    if scope == StateScope::Viewer && owner.is_none() {
        return AppError::ConcealedNotFound.into_response();
    }
    match deps.viewer_state.get(artifact, key, scope, owner).await {
        Ok(Some(value)) => no_store(
            Json(ValueResponse {
                key: value.key,
                value: value.value,
                revision: value.revision,
                updated_at: value.updated_at.0,
                updated_by: (value.scope == StateScope::Org).then_some(value.updated_by.0),
            })
            .into_response(),
        ),
        Ok(None) => AppError::ConcealedNotFound.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn put_state(
    State(deps): State<AppDeps>,
    Path((id, key)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, request.headers(), &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let scope = match scope_query(request.uri()) {
        Ok(s) => s,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error),
    };
    if scope == StateScope::Viewer && viewer.email.is_none() {
        return AppError::ConcealedNotFound.into_response();
    }
    if !state::valid_key(&key) {
        return json_error(StatusCode::BAD_REQUEST, "bad_key");
    }
    let (_, body) = match parse_json_request(
        request,
        deps.config.body.state_json,
        &deps.config.ingress,
    )
    .await
    {
        Ok(value) => value,
        Err(response) => {
            return match response.status() {
                StatusCode::PAYLOAD_TOO_LARGE => {
                    json_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large")
                }
                StatusCode::BAD_REQUEST | StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                    json_error(StatusCode::BAD_REQUEST, "bad_body")
                }
                _ => response,
            };
        }
    };
    if !matches!(&body, OrderedJson::Object(fields) if fields.iter().all(|(name, _)| name == "value" || name == "if_revision") && fields.iter().any(|(name, _)| name == "value"))
    {
        return json_error(StatusCode::BAD_REQUEST, "bad_body");
    }
    let Some(value) = body.get("value") else {
        return json_error(StatusCode::BAD_REQUEST, "bad_body");
    };
    let if_revision = match body.get("if_revision") {
        None => None,
        Some(OrderedJson::Number(number)) => match number
            .as_f64()
            .filter(|value| (0.0..=9_007_199_254_740_991.0).contains(value) && value.fract() == 0.0)
        {
            Some(value) => Some(value as u64),
            None => return json_error(StatusCode::BAD_REQUEST, "bad_revision"),
        },
        _ => return json_error(StatusCode::BAD_REQUEST, "bad_revision"),
    };
    let Some(writer) = viewer.email else {
        return AppError::ConcealedNotFound.into_response();
    };
    match deps
        .viewer_state
        .put(artifact, key, value.clone(), if_revision, writer, scope)
        .await
    {
        Ok(value) => Json(KeyResponse::from(value)).into_response(),
        Err(StateError::Conflict(current)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "conflict", "value": current.value, "revision": current.revision,
            })),
        )
            .into_response(),
        Err(StateError::TooManyKeys) => json_error(StatusCode::CONFLICT, "too_many_keys"),
        Err(StateError::App(AppError::PayloadTooLarge)) => {
            json_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large")
        }
        Err(StateError::App(error)) => error.into_response(),
    }
}

async fn delete_state(
    State(deps): State<AppDeps>,
    Path((id, key)): Path<(String, String)>,
    request_id: Option<Extension<AuditRequestId>>,
    request: Request,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, request.headers(), &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let scope = match scope_query(request.uri()) {
        Ok(s) => s,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error),
    };
    if scope == StateScope::Viewer && viewer.email.is_none() {
        return AppError::ConcealedNotFound.into_response();
    }
    if !state::valid_key(&key) {
        return json_error(StatusCode::BAD_REQUEST, "bad_key");
    }
    let audit =
        match MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref().map(|id| &id.0)) {
            Ok(value) => value,
            Err(error) => return error.into_response(),
        };
    match deps
        .viewer_state
        .delete(artifact, key, scope, viewer.email, audit)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}
