//! Audited, redaction-safe HTTP surface for optional Discord discussion mirroring.

use axum::{
    Json, Router,
    extract::{Extension, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    AppDeps,
    error::AppError,
    http::routes::{
        admin::{admin_json_request, require_admin},
        artifact::{authorize, parse_json_request, viewer_audit},
    },
    mcp::protocol::OrderedJson,
    model::{OrgId, Viewer},
    ports::{
        ArtifactDiscussionOverrideView, ArtifactDiscussionView, DiscussionConnectionView,
        DiscussionModeRequest, DiscussionOverrideRequest, OrganizationThreadingView,
    },
    security::{
        access::{AccessPolicy, FORBIDDEN_MESSAGE},
        audit::{AuditRequestId, MutationAudit},
    },
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route(
            "/settings/orgs/{org}/discord-threading",
            get(organization_threading)
                .put(save_organization_threading)
                .delete(remove_organization_credential),
        )
        .route(
            "/settings/orgs/{org}/discord-threading/test",
            post(test_organization_credential),
        )
        .route(
            "/settings/orgs/{org}/discord-threading/recovery",
            post(queue_historical_recovery),
        )
        .route(
            "/settings/orgs/{org}/discord-discussion",
            get(connection)
                .put(configure_connection)
                .delete(remove_connection),
        )
        .route(
            "/{id}/discussion/override",
            get(artifact_override).put(set_artifact_override),
        )
        .route(
            "/settings/orgs/{org}/discord-discussion/test",
            post(test_connection),
        )
        .route("/{id}/discussion", get(status).put(set_mode))
        .route("/{id}/discussion/retry", post(retry))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionResponse {
    configured: bool,
    label: String,
    destination: String,
    strategy: String,
    webhook_id: Option<String>,
    bot_configured: bool,
    last_error: Option<String>,
}

impl From<DiscussionConnectionView> for ConnectionResponse {
    fn from(value: DiscussionConnectionView) -> Self {
        Self {
            configured: value.configured,
            label: value.label,
            destination: value.destination,
            strategy: value.strategy,
            webhook_id: value.webhook_id,
            bot_configured: value.bot_configured,
            last_error: value.last_error,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscussionResponse {
    mode: String,
    state: String,
    enabled: bool,
    connection_configured: bool,
    last_error: Option<String>,
}

impl From<ArtifactDiscussionView> for DiscussionResponse {
    fn from(value: ArtifactDiscussionView) -> Self {
        Self {
            mode: value.mode,
            state: value.state,
            enabled: value.enabled,
            connection_configured: value.connection_configured,
            last_error: value.last_error,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OrganizationThreadingResponse {
    credential: String,
    enabled: bool,
    degraded: bool,
    recovery: RecoveryResponse,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryResponse {
    state: String,
    pending: u64,
}

impl From<OrganizationThreadingView> for OrganizationThreadingResponse {
    fn from(value: OrganizationThreadingView) -> Self {
        Self {
            credential: safe_credential_state(&value.credential).to_owned(),
            enabled: value.enabled,
            degraded: value.degraded,
            recovery: RecoveryResponse {
                state: safe_recovery_state(&value.recovery_state).to_owned(),
                pending: value.recovery_pending,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactOverrideResponse {
    override_mode: String,
    effective_mode: String,
    state: String,
    actionable_error: Option<String>,
}

impl From<ArtifactDiscussionOverrideView> for ArtifactOverrideResponse {
    fn from(value: ArtifactDiscussionOverrideView) -> Self {
        Self {
            override_mode: safe_override_mode(&value.override_mode).to_owned(),
            effective_mode: safe_effective_mode(&value.effective_mode).to_owned(),
            state: safe_artifact_state(&value.state).to_owned(),
            actionable_error: value.actionable_error.and_then(safe_actionable_error),
        }
    }
}

fn safe_credential_state(value: &str) -> &str {
    match value {
        "configured" | "fallback" | "missing" => value,
        _ => "missing",
    }
}

fn safe_recovery_state(value: &str) -> &str {
    match value {
        "idle" | "recovering" | "degraded" | "ambiguous" | "not_found" | "permission_denied"
        | "rate_limited" | "unavailable" => value,
        _ => "unavailable",
    }
}

fn safe_override_mode(value: &str) -> &str {
    match value {
        "inherit" | "artifact_only" | "discord_two_way" => value,
        _ => "inherit",
    }
}

fn safe_effective_mode(value: &str) -> &str {
    match value {
        "artifact_only" | "discord_mirror" | "discord_two_way" => value,
        _ => "artifact_only",
    }
}

fn safe_artifact_state(value: &str) -> &str {
    match value {
        "local" | "recovering" | "pending" | "connected" | "connecting" | "ready" | "degraded"
        | "unavailable" | "failed" => value,
        _ => "unavailable",
    }
}

fn safe_actionable_error(value: String) -> Option<String> {
    match value.as_str() {
        "threading_unavailable" => Some(
            "Discord threading is unavailable. Check the organization credential and selected notification destination."
                .to_owned(),
        ),
        "recovery_not_found" => Some(
            "The original notification could not be recovered; discussion remains in Artifact MCP."
                .to_owned(),
        ),
        "recovery_ambiguous" => Some(
            "More than one exact historical notification matched, so no thread anchor was chosen."
                .to_owned(),
        ),
        "recovery_permission_denied" => Some(
            "Discord message history cannot be read for the selected destination.".to_owned(),
        ),
        "recovery_rate_limited" => Some(
            "Discord is rate limiting historical recovery; Artifact MCP will retry safely."
                .to_owned(),
        ),
        "missing_credential" => Some(
            "The organization Discord credential is unavailable; two-way sync remains off."
                .to_owned(),
        ),
        "message_content_intent" => Some(
            "Enable Discord's Message Content intent before using two-way sync.".to_owned(),
        ),
        "guild_access" => Some(
            "The Discord bot cannot access the selected organization guild.".to_owned(),
        ),
        "thread_permission" => Some(
            "The Discord bot cannot read the selected artifact thread.".to_owned(),
        ),
        "gateway_unavailable" => Some(
            "Discord inbound sync is temporarily unavailable; Artifact MCP remains canonical."
                .to_owned(),
        ),
        "thread_unavailable" => Some(
            "The mapped Discord thread is unavailable; Artifact MCP feedback remains available."
                .to_owned(),
        ),
        _ => None,
    }
}

async fn organization_threading(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    match deps.discussions.organization_threading(&OrgId(org)).await {
        Ok(view) => Json(OrganizationThreadingResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn save_organization_threading(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (bot_token, enabled) = match organization_threading_body(&body) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    match deps
        .discussions
        .save_organization_threading(OrgId(org), bot_token, enabled, audit)
        .await
    {
        Ok(view) => Json(OrganizationThreadingResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn test_organization_credential(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_empty_value_object(&body) {
        return AppError::Validation("invalid Discord threading request".to_owned())
            .into_response();
    }
    match deps
        .discussions
        .test_organization_credential(OrgId(org), audit)
        .await
    {
        Ok(tested) => Json(serde_json::json!({ "tested": tested })).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn remove_organization_credential(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    request_id: Option<Extension<AuditRequestId>>,
    headers: HeaderMap,
) -> Response {
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let audit = match audit_for(&viewer, request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    match deps
        .discussions
        .remove_organization_credential(OrgId(org), audit)
        .await
    {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn queue_historical_recovery(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_empty_value_object(&body) {
        return AppError::Validation("invalid Discord threading request".to_owned())
            .into_response();
    }
    match deps
        .discussions
        .queue_historical_recovery(OrgId(org), audit)
        .await
    {
        Ok(queued) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "queued": queued })),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn artifact_override(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    match deps.discussions.artifact_override(artifact.meta()).await {
        Ok(view) => Json(ArtifactOverrideResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn set_artifact_override(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let request_id = request.extensions().get::<AuditRequestId>().cloned();
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if !AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        return AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()).into_response();
    }
    let (_headers, body) =
        match parse_json_request(request, deps.config.body.key_json, &deps.config.ingress).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let override_mode = match override_body(&body) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let audit = match viewer_audit(&viewer, request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    let actor = viewer.email.map_or_else(String::new, |email| email.0);
    match deps
        .discussions
        .set_artifact_override(artifact.meta().clone(), override_mode, actor, audit)
        .await
    {
        Ok(view) => Json(ArtifactOverrideResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn connection(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    match deps.discussions.connection(&OrgId(org)).await {
        Ok(view) => Json(ConnectionResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn configure_connection(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let (webhook_id, label) = match connection_body(&body) {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    match deps
        .discussions
        .configure_connection(OrgId(org), webhook_id, label, audit)
        .await
    {
        Ok(view) => Json(ConnectionResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn remove_connection(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    request_id: Option<Extension<AuditRequestId>>,
    headers: HeaderMap,
) -> Response {
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let audit = match audit_for(&viewer, request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    match deps.discussions.remove_connection(OrgId(org), audit).await {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn test_connection(
    State(deps): State<AppDeps>,
    Path(org): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !is_empty_value_object(&body) {
        return AppError::Validation("invalid discussion test request".to_owned()).into_response();
    }
    match deps.discussions.test_connection(OrgId(org), audit).await {
        Ok(tested) => Json(serde_json::json!({ "tested": tested })).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn status(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    match deps.discussions.status(artifact.meta()).await {
        Ok(view) => Json(DiscussionResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn set_mode(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let request_id = request.extensions().get::<AuditRequestId>().cloned();
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if !AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        return AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()).into_response();
    }
    let (_headers, body) =
        match parse_json_request(request, deps.config.body.key_json, &deps.config.ingress).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let mode = match mode_body(&body) {
        Ok(mode) => mode,
        Err(error) => return error.into_response(),
    };
    let audit = match viewer_audit(&viewer, request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    let actor = viewer.email.map_or_else(String::new, |email| email.0);
    match deps
        .discussions
        .set_mode(artifact.meta().clone(), mode, actor, audit)
        .await
    {
        Ok(view) => Json(DiscussionResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn retry(State(deps): State<AppDeps>, Path(id): Path<String>, request: Request) -> Response {
    let headers = request.headers().clone();
    let request_id = request.extensions().get::<AuditRequestId>().cloned();
    let (artifact, viewer) = match authorize(&deps, &headers, &id).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    if !AccessPolicy::viewer_can_manage_artifact(&viewer, artifact.meta()) {
        return AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()).into_response();
    }
    let (_headers, body) =
        match parse_json_request(request, deps.config.body.key_json, &deps.config.ingress).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    if !is_empty_ordered_object(&body) {
        return AppError::Validation("invalid discussion retry request".to_owned()).into_response();
    }
    let audit = match viewer_audit(&viewer, request_id.as_ref()) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    let actor = viewer.email.map_or_else(String::new, |email| email.0);
    match deps
        .discussions
        .retry(artifact.meta().clone(), actor, audit)
        .await
    {
        Ok(view) => Json(DiscussionResponse::from(view)).into_response(),
        Err(error) => error.into_response(),
    }
}

fn audit_for(
    viewer: &Viewer,
    request_id: Option<&Extension<AuditRequestId>>,
) -> Result<MutationAudit, AppError> {
    MutationAudit::viewer_with_request_id(viewer, request_id.map(|id| &id.0))
}

fn connection_body(body: &serde_json::Value) -> Result<(String, String), AppError> {
    let Some(entries) = body.as_object() else {
        return Err(AppError::Validation(
            "invalid discussion connection request".to_owned(),
        ));
    };
    if entries.len() != 2
        || !entries
            .keys()
            .all(|key| matches!(key.as_str(), "webhookId" | "label"))
    {
        return Err(AppError::Validation(
            "invalid discussion connection request".to_owned(),
        ));
    }
    let webhook_id = body
        .get("webhookId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::Validation("invalid discussion connection request".to_owned()))?;
    let label = body
        .get("label")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::Validation("invalid discussion connection request".to_owned()))?;
    Ok((webhook_id.to_owned(), label.to_owned()))
}

/// The token is deliberately parsed only after admin authorization. It is handed directly to
/// the encrypted credential service and never included in an error, response, audit, or trace.
fn organization_threading_body(body: &serde_json::Value) -> Result<(String, bool), AppError> {
    let Some(entries) = body.as_object() else {
        return Err(AppError::Validation(
            "invalid Discord threading request".to_owned(),
        ));
    };
    if entries.len() != 2
        || !entries
            .keys()
            .all(|key| matches!(key.as_str(), "botToken" | "enabled"))
    {
        return Err(AppError::Validation(
            "invalid Discord threading request".to_owned(),
        ));
    }
    let token = body
        .get("botToken")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.len() <= 512)
        .ok_or_else(|| AppError::Validation("invalid Discord threading request".to_owned()))?;
    let enabled = body
        .get("enabled")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| AppError::Validation("invalid Discord threading request".to_owned()))?;
    Ok((token.to_owned(), enabled))
}

fn override_body(body: &OrderedJson) -> Result<DiscussionOverrideRequest, AppError> {
    let Some(entries) = body.as_object() else {
        return Err(AppError::Validation(
            "invalid discussion override request".to_owned(),
        ));
    };
    if entries.len() != 1 || entries[0].0 != "override" {
        return Err(AppError::Validation(
            "invalid discussion override request".to_owned(),
        ));
    }
    match body.get("override").and_then(OrderedJson::as_str) {
        Some("inherit") => Ok(DiscussionOverrideRequest::Inherit),
        Some("artifact_only") => Ok(DiscussionOverrideRequest::ArtifactOnly),
        Some("discord_two_way") => Ok(DiscussionOverrideRequest::DiscordTwoWay),
        _ => Err(AppError::Validation(
            "invalid discussion override request".to_owned(),
        )),
    }
}

fn mode_body(body: &OrderedJson) -> Result<DiscussionModeRequest, AppError> {
    let Some(entries) = body.as_object() else {
        return Err(AppError::Validation(
            "invalid discussion mode request".to_owned(),
        ));
    };
    if entries.len() != 1 || entries[0].0 != "mode" {
        return Err(AppError::Validation(
            "invalid discussion mode request".to_owned(),
        ));
    }
    match body.get("mode").and_then(OrderedJson::as_str) {
        Some("artifact_only") => Ok(DiscussionModeRequest::ArtifactOnly),
        Some("discord_mirror") => Ok(DiscussionModeRequest::DiscordMirror),
        _ => Err(AppError::Validation(
            "invalid discussion mode request".to_owned(),
        )),
    }
}

fn is_empty_value_object(body: &serde_json::Value) -> bool {
    body.as_object().is_some_and(serde_json::Map::is_empty)
}

fn is_empty_ordered_object(body: &OrderedJson) -> bool {
    body.as_object().is_some_and(<[_]>::is_empty)
}

#[cfg(test)]
mod tests {
    use super::{OrganizationThreadingResponse, organization_threading_body, override_body};
    use crate::ports::{DiscussionOverrideRequest, OrganizationThreadingView};

    #[test]
    fn organization_threading_body_accepts_only_write_only_token_and_policy() {
        let token = "synthetic-token-not-for-output";
        let body = serde_json::json!({ "botToken": token, "enabled": true });
        assert_eq!(
            organization_threading_body(&body).expect("valid request"),
            (token.to_owned(), true)
        );
        let error = organization_threading_body(&serde_json::json!({
            "botToken": token,
            "enabled": true,
            "unexpected": true
        }))
        .expect_err("unknown field must fail");
        assert!(!error.to_string().contains(token));
    }

    #[test]
    fn artifact_override_accepts_three_explicit_modes() {
        let inherit: crate::mcp::protocol::OrderedJson =
            serde_json::from_str(r#"{"override":"inherit"}"#).expect("ordered json");
        assert_eq!(
            override_body(&inherit).expect("inherit"),
            DiscussionOverrideRequest::Inherit
        );
        let two_way: crate::mcp::protocol::OrderedJson =
            serde_json::from_str(r#"{"override":"discord_two_way"}"#).expect("ordered json");
        assert_eq!(
            override_body(&two_way).expect("two-way"),
            DiscussionOverrideRequest::DiscordTwoWay
        );
        let invalid: crate::mcp::protocol::OrderedJson =
            serde_json::from_str(r#"{"override":"discord_mirror"}"#).expect("ordered json");
        assert!(override_body(&invalid).is_err());
    }

    #[test]
    fn organization_response_drops_unexpected_credential_text() {
        let response = OrganizationThreadingResponse::from(OrganizationThreadingView {
            credential: "synthetic-token-not-for-output".to_owned(),
            enabled: true,
            degraded: false,
            recovery_state: "idle".to_owned(),
            recovery_pending: 0,
        });
        let rendered = serde_json::to_string(&response).expect("serialize response");
        assert!(rendered.contains("\"credential\":\"missing\""));
        assert!(!rendered.contains("synthetic-token-not-for-output"));
    }
}
