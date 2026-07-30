//! Artifact-scoped human actions.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::Request,
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Serialize;

use crate::{
    AppDeps,
    artifacts::lifecycle::normalize_category,
    error::AppError,
    http::ingress::{
        BodyReadError, ViewerCost, complexity_response, read_body_limited, validate_json_complexity,
    },
    mcp::protocol::OrderedJson,
    model::{
        ArtifactRevision, CreateShare, OrgId, PublicShare, ReactionUpdate, ShareToken, Timestamp,
        Viewer,
    },
    security::{
        access::{AccessPolicy, AuthorizedArtifact, FORBIDDEN_MESSAGE, resolve_for_viewer},
        audit::{AuditRequestId, MutationAudit},
    },
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

pub(crate) async fn authorize(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
) -> Result<(AuthorizedArtifact, Viewer), AppError> {
    let viewer = deps.viewer_identity.resolve(headers).await?;
    if !deps
        .ingress
        .allow_verified_viewer(headers, &viewer, ViewerCost::Read)
    {
        return Err(AppError::RateLimited);
    }
    let artifact = resolve_for_viewer(deps.artifacts.as_ref(), &viewer, id).await?;
    Ok((artifact, viewer))
}

fn request_id_from_request(request: &Request) -> Option<AuditRequestId> {
    request.extensions().get::<AuditRequestId>().cloned()
}

pub(crate) fn viewer_audit(
    viewer: &Viewer,
    request_id: Option<&AuditRequestId>,
) -> Result<MutationAudit, AppError> {
    MutationAudit::viewer_with_request_id(viewer, request_id)
}

#[derive(Serialize)]
struct DeleteResponse {
    id: String,
    deleted: bool,
}

async fn delete_artifact(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request_id: Option<Extension<AuditRequestId>>,
    headers: HeaderMap,
) -> Result<Json<DeleteResponse>, AppError> {
    let (artifact, viewer) = authorize(&deps, &headers, &id).await?;
    if !AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        return Err(AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()));
    }
    let meta = artifact.meta().clone();
    let deleted = deps
        .artifacts
        .delete(
            artifact,
            viewer_audit(&viewer, request_id.as_ref().map(|id| &id.0))?,
        )
        .await?;
    if deleted {
        deps.delivery_wake.wake();
        let previews = deps.previews.clone();
        let artifact_id = meta.id.clone();
        tokio::spawn(async move {
            let _ignored = previews.remove_artifact(&artifact_id).await;
        });
    }
    Ok(Json(DeleteResponse { id, deleted }))
}

async fn react(State(deps): State<AppDeps>, Path(id): Path<String>, request: Request) -> Response {
    let (headers, body) = match parse_json_request(
        request,
        deps.config.body.reaction_json,
        &deps.config.ingress,
    )
    .await
    {
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
    let audit_request_id = request_id_from_request(&request);
    let (headers, body) = match parse_json_request(
        request,
        deps.config.body.category_json,
        &deps.config.ingress,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    if !body.contains_key("category") {
        return AppError::Validation("category is required".to_owned()).into_response();
    }
    let category = javascript_or_empty(body.get("category"));
    let registered_category = normalize_category(&category);
    let audit = match viewer_audit(&viewer, audit_request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    // The two services cannot share a transaction through their public ports. Registering first
    // makes its audited write a required precondition: an audit/registry failure leaves the
    // artifact untouched rather than returning success with an invisible category.
    if !registered_category.is_empty()
        && let Err(error) = deps
            .admin
            .add_category(&artifact.meta().org, &registered_category, audit.clone())
            .await
    {
        return error.into_response();
    }
    match deps.artifacts.set_category(artifact, category, audit).await {
        Ok(meta) => Json(CategoryResponse {
            id,
            category: meta.category,
        })
        .into_response(),
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
    let audit_request_id = request_id_from_request(&request);
    let (headers, body) = match parse_json_request(
        request,
        deps.config.body.category_json,
        &deps.config.ingress,
    )
    .await
    {
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
    match deps
        .artifacts
        .set_hidden(
            artifact,
            hidden,
            match viewer_audit(&viewer, audit_request_id.as_ref()) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
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
    let audit_request_id = request_id_from_request(&request);
    let (headers, body) = match parse_json_request(
        request,
        deps.config.body.category_json,
        &deps.config.ingress,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    let request = CreateShare {
        created_by: viewer
            .email
            .as_ref()
            .map_or_else(String::new, |email| email.0.clone()),
        expires: body
            .get("expires")
            .map_or_else(String::new, javascript_string),
    };
    match deps
        .shares
        .create(
            artifact,
            request,
            match viewer_audit(&viewer, audit_request_id.as_ref()) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
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
    request_id: Option<Extension<AuditRequestId>>,
    headers: HeaderMap,
) -> Response {
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    match deps
        .shares
        .revoke(
            artifact,
            ShareToken(token.clone()),
            match viewer_audit(&viewer, request_id.as_ref().map(|id| &id.0)) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
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
    let audit_request_id = request_id_from_request(&request);
    let (headers, body) = match parse_json_request(
        request,
        deps.config.body.category_json,
        &deps.config.ingress,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(allowed) => allowed,
        Err(error) => return error.into_response(),
    };
    let Some(revision) = body.get("revision").and_then(positive_integer) else {
        return AppError::Validation("revision must be a positive integer".to_owned())
            .into_response();
    };
    let audit = match viewer_audit(&viewer, audit_request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    match deps
        .artifacts
        .restore(artifact, revision, None, audit)
        .await
    {
        Ok(result) => {
            deps.delivery_wake.wake();
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
    let audit_request_id = request_id_from_request(&request);
    let (headers, body) = match parse_json_request(
        request,
        deps.config.body.category_json,
        &deps.config.ingress,
    )
    .await
    {
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

    if !body.contains_key("org") && !body.contains_key("category") {
        return AppError::Validation("org or category is required".to_owned()).into_response();
    }

    let audit = match viewer_audit(&viewer, audit_request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    let target_category = if body.contains_key("org") {
        if body.contains_key("category") {
            javascript_or_empty(body.get("category"))
        } else {
            artifact.meta().category.clone()
        }
    } else {
        javascript_or_empty(body.get("category"))
    };
    let registered_category = normalize_category(&target_category);
    let target_org = if body.contains_key("org") {
        OrgId(javascript_or_empty(body.get("org")))
    } else {
        artifact.meta().org.clone()
    };
    // As above, category registration is a prerequisite because the ports do not expose a
    // cross-service transaction. Do not move or retag when its ledger write fails.
    if !registered_category.is_empty()
        && let Err(error) = deps
            .admin
            .add_category(&target_org, &registered_category, audit.clone())
            .await
    {
        return error.into_response();
    }

    let result = if body.contains_key("org") {
        let target_org = OrgId(javascript_or_empty(body.get("org")));
        let category = body
            .contains_key("category")
            .then(|| javascript_or_empty(body.get("category")));
        deps.artifacts
            .move_to_org(artifact, target_org, category, audit)
            .await
    } else {
        deps.artifacts
            .set_category(artifact, target_category, audit)
            .await
    };
    match result {
        Ok(meta) => Json(MoveResponse {
            id,
            org: meta.org,
            category: meta.category,
        })
        .into_response(),
        Err(AppError::NotFound(reason)) if reason == "not_found" => {
            AppError::ConcealedNotFound.into_response()
        }
        Err(error) => error.into_response(),
    }
}

pub(crate) async fn parse_json_request(
    request: Request,
    limit: u64,
    ingress: &crate::config::IngressConfig,
) -> Result<(HeaderMap, OrderedJson), Response> {
    let headers = request.headers().clone();
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let bytes = read_body_limited(
        request.into_body(),
        limit,
        Duration::from_millis(ingress.read_timeout_ms),
    )
    .await
    .map_err(|error| match error {
        BodyReadError::Timeout => json_error(StatusCode::REQUEST_TIMEOUT, "request timeout"),
        BodyReadError::TooLarge => json_error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large"),
        BodyReadError::Invalid => json_error(StatusCode::BAD_REQUEST, "invalid JSON"),
    })?;
    // Bodyless portal actions retain Node's `req.body = {}`-style behaviour. Any actual bytes
    // with an unsupported content type, including chunked bodies without Content-Length, are
    // rejected before authorization or mutation instead of silently executing as `Null`.
    if !is_json_content_type(&headers) {
        return if bytes.is_empty() {
            Ok((headers, OrderedJson::Null))
        } else {
            Err(json_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported media type",
            ))
        };
    }
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
    validate_json_complexity(&body, ingress).map_err(complexity_response)?;
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
