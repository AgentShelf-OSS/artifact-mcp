//! Artifact-scoped human actions.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::Request,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Serialize;

use crate::{
    AppDeps,
    error::AppError,
    mcp::protocol::OrderedJson,
    model::{
        ArtifactMeta, ArtifactRevision, CreateShare, NotificationPayload, OrgId, PublicShare,
        ReactionUpdate, ShareToken, Timestamp, Viewer, WebhookEvent,
    },
    security::access::{AccessPolicy, AuthorizedArtifact, FORBIDDEN_MESSAGE, resolve_for_viewer},
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/{id}", delete(delete_artifact))
        .route("/{id}/react", post(react))
        .route("/{id}/category", post(set_category))
        .route("/{id}/share", post(create_share))
        .route("/{id}/shares", get(list_shares))
        .route("/{id}/shares/{token}", delete(revoke_share))
        .route("/{id}/visibility", post(set_visibility))
        .route("/{id}/move", post(move_artifact))
        .route("/{id}/history", get(history))
        .route("/{id}/restore", post(restore))
}

pub(super) async fn authorize(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
) -> Result<(AuthorizedArtifact, Viewer), AppError> {
    let viewer = deps.viewer_identity.resolve(headers).await?;
    let artifact = resolve_for_viewer(deps.artifacts.as_ref(), &viewer, id).await?;
    Ok((artifact, viewer))
}

#[derive(Serialize)]
struct DeleteResponse {
    id: String,
    deleted: bool,
}

async fn delete_artifact(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DeleteResponse>, AppError> {
    let (artifact, viewer) = authorize(&deps, &headers, &id).await?;
    if !AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        return Err(AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()));
    }
    let meta = artifact.meta().clone();
    let deleted = deps.artifacts.delete(artifact).await?;
    if deleted {
        let previews = deps.previews.clone();
        let artifact_id = meta.id.clone();
        tokio::spawn(async move {
            let _ignored = previews.remove_artifact(&artifact_id).await;
        });
        notify_artifact(WebhookEvent::Deleted, &meta, None, &deps).await;
    }
    Ok(Json(DeleteResponse { id, deleted }))
}

async fn react(State(deps): State<AppDeps>, Path(id): Path<String>, request: Request) -> Response {
    let (headers, body) = match parse_json_request(request, deps.config.body.reaction_json).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    let update = match parse_reaction(&body) {
        Ok(update) => update,
        Err(error) => return error.into_response(),
    };
    match deps.engagement.set_reaction(artifact, viewer, update).await {
        Ok(reaction) => Json(reaction).into_response(),
        Err(error) => error.into_response(),
    }
}

fn parse_reaction(body: &OrderedJson) -> Result<ReactionUpdate, AppError> {
    let Some(entries) = body.as_object() else {
        return Err(AppError::Validation(
            "Reaction body must be a JSON object.".to_owned(),
        ));
    };
    if entries.is_empty() {
        return Err(AppError::Validation(
            "Reaction body must include favorite or vote.".to_owned(),
        ));
    }
    if let Some((unknown, _)) = body
        .object_entries()
        .into_iter()
        .find(|(key, _)| !matches!(*key, "favorite" | "vote"))
    {
        return Err(AppError::Validation(format!(
            "Unknown reaction field: {unknown}"
        )));
    }

    let favorite = match body.get("favorite") {
        None => None,
        Some(OrderedJson::Bool(value)) => Some(*value),
        Some(value) if json_number_is(value, 0.0) => Some(false),
        Some(value) if json_number_is(value, 1.0) => Some(true),
        Some(_) => {
            return Err(AppError::Validation(
                "favorite must be true, false, 0, or 1.".to_owned(),
            ));
        }
    };
    let vote = match body.get("vote") {
        None => None,
        Some(value) if json_number_is(value, -1.0) => Some(-1),
        Some(value) if json_number_is(value, 0.0) => Some(0),
        Some(value) if json_number_is(value, 1.0) => Some(1),
        Some(_) => {
            return Err(AppError::Validation("vote must be -1, 0, or 1.".to_owned()));
        }
    };
    Ok(ReactionUpdate { favorite, vote })
}

fn json_number_is(value: &OrderedJson, expected: f64) -> bool {
    value.as_number().and_then(serde_json::Number::as_f64) == Some(expected)
}

#[derive(Serialize)]
struct CategoryResponse {
    id: String,
    category: String,
}

async fn set_category(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (headers, body) = match parse_json_request(request, deps.config.body.category_json).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    match deps
        .artifacts
        .set_category(artifact, javascript_or_empty(body.get("category")))
        .await
    {
        Ok(meta) => {
            // Best-effort: register the category on the org so it appears in the Settings picker,
            // exactly as the MCP set_category tool does. Without this a category assigned through
            // the web UI never reaches org_categories and stays invisible in Settings.
            if !meta.category.is_empty() {
                let _ignored = deps.admin.add_category(&meta.org, &meta.category).await;
            }
            Json(CategoryResponse {
                id,
                category: meta.category,
            })
            .into_response()
        }
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct VisibilityResponse {
    id: String,
    hidden: bool,
}

async fn set_visibility(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (headers, body) = match parse_json_request(request, deps.config.body.category_json).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    let Some(hidden) = body.get("hidden").and_then(OrderedJson::as_bool) else {
        return AppError::Validation("hidden must be a boolean".to_owned()).into_response();
    };
    // `authorize` retains the concealed cross-org result. Delete and visibility deliberately
    // share one administrator-or-immutable-owner policy.
    if !AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        return AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()).into_response();
    }
    match deps.artifacts.set_hidden(artifact, hidden).await {
        Ok(meta) => Json(VisibilityResponse {
            id,
            hidden: meta.hidden,
        })
        .into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct CreateShareResponse {
    token: ShareToken,
    expires_at: Option<Timestamp>,
    url: String,
}

async fn create_share(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (headers, body) = match parse_json_request(request, deps.config.body.category_json).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    let request = CreateShare {
        created_by: viewer.email.map_or_else(String::new, |email| email.0),
        expires: body
            .get("expires")
            .map_or_else(String::new, javascript_string),
    };
    match deps.shares.create(artifact, request).await {
        Ok(share) => {
            let url = format!("{}/s/{}", deps.config.public_base_url, share.token);
            Json(CreateShareResponse {
                token: share.token,
                expires_at: share.expires_at,
                url,
            })
            .into_response()
        }
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct SharesResponse {
    shares: Vec<PublicShare>,
}

async fn list_shares(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    match deps.shares.list(&artifact).await {
        Ok(shares) => Json(SharesResponse { shares }).into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct RevokeShareResponse {
    token: String,
    revoked: bool,
}

async fn revoke_share(
    State(deps): State<AppDeps>,
    Path((id, token)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    match deps
        .shares
        .revoke(artifact, ShareToken(token.clone()))
        .await
    {
        Ok(true) => Json(RevokeShareResponse {
            token,
            revoked: true,
        })
        .into_response(),
        Ok(false) => AppError::ConcealedNotFound.into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct HistoryResponse {
    current: u64,
    revisions: Vec<RevisionResponse>,
}

#[derive(Serialize)]
struct RevisionResponse {
    artifact_id: crate::model::ArtifactId,
    org: OrgId,
    revision: u64,
    title: String,
    description: String,
    category: String,
    bytes: u64,
    is_bundle: i64,
    entry: String,
    body_sha256: String,
    created_at: Timestamp,
}

impl From<ArtifactRevision> for RevisionResponse {
    fn from(revision: ArtifactRevision) -> Self {
        Self {
            artifact_id: revision.artifact_id,
            org: revision.org,
            revision: revision.revision,
            title: revision.title,
            description: revision.description,
            category: revision.category,
            bytes: revision.bytes,
            is_bundle: i64::from(revision.is_bundle),
            entry: revision.entry,
            body_sha256: revision.body_sha256,
            created_at: revision.created_at,
        }
    }
}

async fn history(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    match deps.artifacts.list_revisions(&artifact).await {
        Ok(history) => Json(HistoryResponse {
            current: history.current,
            revisions: history.revisions.into_iter().map(Into::into).collect(),
        })
        .into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct RestoreResponse {
    id: String,
    revision: u64,
    #[serde(rename = "restoredFrom")]
    restored_from: u64,
}

async fn restore(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (headers, body) = match parse_json_request(request, deps.config.body.category_json).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    let Some(revision) = body.get("revision").and_then(positive_integer) else {
        return AppError::Validation("revision must be a positive integer".to_owned())
            .into_response();
    };
    match deps.artifacts.restore(artifact, revision, None).await {
        Ok(result) => {
            notify_artifact(WebhookEvent::Restored, &result.meta, None, &deps).await;
            Json(RestoreResponse {
                id,
                revision: result.meta.revision,
                restored_from: result.restored_from,
            })
            .into_response()
        }
        Err(error) => error.into_response(),
    }
}

fn positive_integer(value: &OrderedJson) -> Option<u64> {
    let number = javascript_number(value)?;
    (number.is_finite() && number >= 1.0 && number.fract() == 0.0).then_some(number as u64)
}

fn javascript_number(value: &OrderedJson) -> Option<f64> {
    match value {
        OrderedJson::Null => Some(0.0),
        OrderedJson::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        OrderedJson::Number(value) => value.as_f64(),
        OrderedJson::String(value) => parse_javascript_number(value),
        OrderedJson::Array(_) => parse_javascript_number(&javascript_string(value)),
        OrderedJson::Object(_) => None,
    }
}

fn parse_javascript_number(value: &str) -> Option<f64> {
    let trimmed = value.trim_matches(|character: char| {
        character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
    });
    if trimmed.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix)
                .ok()
                .map(|value| value as f64);
        }
    }
    trimmed.parse::<f64>().ok()
}

#[derive(Serialize)]
struct MoveResponse {
    id: String,
    org: OrgId,
    category: String,
}

async fn move_artifact(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (headers, body) = match parse_json_request(request, deps.config.body.category_json).await {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = AccessPolicy::admin_access(&viewer) {
        return error.into_response();
    }

    let result = if body.contains_key("org") {
        let target_org = OrgId(javascript_or_empty(body.get("org")));
        let category = body
            .contains_key("category")
            .then(|| javascript_or_empty(body.get("category")));
        deps.artifacts
            .move_to_org(artifact, target_org, category)
            .await
    } else {
        deps.artifacts
            .set_category(artifact, javascript_or_empty(body.get("category")))
            .await
    };
    match result {
        Ok(meta) => {
            // Register the resulting category on the artifact's (possibly new) org, same as above.
            if !meta.category.is_empty() {
                let _ignored = deps.admin.add_category(&meta.org, &meta.category).await;
            }
            Json(MoveResponse {
                id,
                org: meta.org,
                category: meta.category,
            })
            .into_response()
        }
        Err(AppError::NotFound(reason)) if reason == "not_found" => {
            AppError::ConcealedNotFound.into_response()
        }
        Err(error) => error.into_response(),
    }
}

pub(super) async fn parse_json_request(
    request: Request,
    limit: u64,
) -> Result<(HeaderMap, OrderedJson), Response> {
    let headers = request.headers().clone();
    if !is_json_content_type(&headers) {
        return Ok((headers, OrderedJson::Null));
    }
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let bytes = to_bytes(request.into_body(), limit)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large"))?;
    if bytes.is_empty() {
        return Ok((headers, OrderedJson::Null));
    }
    let first = bytes
        .iter()
        .copied()
        .find(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));
    if !matches!(first, Some(b'{') | Some(b'[')) {
        return Err(json_error(StatusCode::BAD_REQUEST, "invalid JSON"));
    }
    let body = serde_json::from_slice(&bytes)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "invalid JSON"))?;
    Ok((headers, body))
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    error: &'a str,
}

pub(super) fn json_error(status: StatusCode, error: &str) -> Response {
    (status, Json(ErrorResponse { error })).into_response()
}

pub(super) fn javascript_truthy(value: &OrderedJson) -> bool {
    match value {
        OrderedJson::Null => false,
        OrderedJson::Bool(value) => *value,
        OrderedJson::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        OrderedJson::String(value) => !value.is_empty(),
        OrderedJson::Array(_) | OrderedJson::Object(_) => true,
    }
}

pub(super) fn javascript_string(value: &OrderedJson) -> String {
    match value {
        OrderedJson::Null => "null".to_owned(),
        OrderedJson::Bool(value) => value.to_string(),
        OrderedJson::Number(value) => OrderedJson::Number(value.clone())
            .to_json_string()
            .unwrap_or_else(|_| "NaN".to_owned()),
        OrderedJson::String(value) => value.clone(),
        OrderedJson::Array(values) => values
            .iter()
            .map(|value| match value {
                OrderedJson::Null => String::new(),
                other => javascript_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        OrderedJson::Object(_) => "[object Object]".to_owned(),
    }
}

pub(super) fn javascript_or_empty(value: Option<&OrderedJson>) -> String {
    value
        .filter(|value| javascript_truthy(value))
        .map_or_else(String::new, javascript_string)
}

async fn notify_artifact(
    event: WebhookEvent,
    meta: &ArtifactMeta,
    resolver: Option<String>,
    deps: &AppDeps,
) {
    let payload = NotificationPayload {
        artifact_id: meta.id.clone(),
        title: meta.title.clone(),
        url: format!("{}/{}", deps.config.public_base_url, meta.id),
        description: meta.description.clone(),
        uploader_label: meta.uploader_label.clone(),
        category: meta.category.clone(),
        revision: meta.revision,
        bytes: meta.bytes,
        viewer_email: None,
        body: None,
        resolver,
    };
    let _ignored = deps
        .notifications
        .emit(event, meta.org.clone(), payload)
        .await;
}
