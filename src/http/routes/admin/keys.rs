use axum::{
    Json,
    extract::{Extension, Path, Request, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    AppDeps,
    config::{Clock, SystemClock},
    error::AppError,
    model::{ClientId, CreatePublisherKey, OrgId, UpdatePublisherKey},
    security::audit::{AuditRequestId, MutationAudit},
};

use super::{admin_json_request, iso_timestamp, js_or_empty, js_trim, require_admin};
pub(super) async fn create_key(State(deps): State<AppDeps>, request: Request) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let target_org = js_trim(&js_or_empty(body.get("org"))).to_owned();
    match deps.admin.org_exists(&OrgId(target_org.clone())).await {
        Ok(true) => {}
        Ok(false) => {
            return AppError::Validation(format!(
                "Unknown organization \"{target_org}\". Create it in the Organizations section first."
            ))
            .into_response();
        }
        Err(error) => return error.into_response(),
    }

    let created = match deps
        .admin
        .create_key(
            CreatePublisherKey {
                client_id: ClientId(js_or_empty(body.get("clientId"))),
                org: OrgId(js_or_empty(body.get("org"))),
                label: js_or_empty(body.get("label")),
                role: match js_or_empty(body.get("role")) {
                    value if value.is_empty() => "author".to_owned(),
                    value => value,
                },
                owner_email: {
                    let value = js_or_empty(body.get("ownerEmail"));
                    (!value.is_empty()).then_some(value)
                },
            },
            audit,
        )
        .await
    {
        Ok(created) => created,
        Err(error) => return error.into_response(),
    };
    Json(CreatedKeyResponse {
        client_id: created.client_id,
        org: created.org,
        label: created.label,
        role: created.role,
        secret: created.secret,
        created_at: iso_timestamp(SystemClock.now_unix_millis()),
    })
    .into_response()
}

pub(super) async fn revoke_key(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request_id: Option<Extension<AuditRequestId>>,
) -> Response {
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let audit =
        match MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref().map(|id| &id.0)) {
            Ok(audit) => audit,
            Err(error) => return error.into_response(),
        };
    let revoked = match deps.admin.revoke_key(&ClientId(id.clone()), audit).await {
        Ok(revoked) => revoked,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "id": id, "revoked": revoked })).into_response()
}

#[derive(Serialize)]
struct UpdatedKeyResponse {
    #[serde(rename = "clientId")]
    client_id: ClientId,
    org: OrgId,
    label: String,
    role: String,
    #[serde(rename = "ownerEmail")]
    owner_email: Option<String>,
}

pub(super) async fn update_key(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let owner_email = js_trim(&js_or_empty(body.get("ownerEmail"))).to_owned();
    match deps
        .admin
        .update_key(
            ClientId(id),
            UpdatePublisherKey {
                label: js_or_empty(body.get("label")),
                role: js_or_empty(body.get("role")),
                owner_email: (!owner_email.is_empty()).then_some(owner_email),
            },
            audit,
        )
        .await
    {
        Ok(Some(updated)) => Json(UpdatedKeyResponse {
            client_id: updated.client_id,
            org: updated.org,
            label: updated.label,
            role: updated.role,
            owner_email: updated.owner_email,
        })
        .into_response(),
        Ok(None) => AppError::ConcealedNotFound.into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct KeyOwnerResponse {
    #[serde(rename = "clientId")]
    client_id: ClientId,
    org: OrgId,
    #[serde(rename = "ownerEmail")]
    owner_email: Option<String>,
}

pub(super) async fn set_key_owner(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let value = js_or_empty(body.get("ownerEmail"));
    match deps
        .admin
        .set_key_owner(ClientId(id), (!value.is_empty()).then_some(value), audit)
        .await
    {
        Ok(Some(updated)) => Json(KeyOwnerResponse {
            client_id: updated.client_id,
            org: updated.org,
            owner_email: updated.owner_email,
        })
        .into_response(),
        Ok(None) => AppError::ConcealedNotFound.into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct OwnerBackfillResponse {
    #[serde(rename = "clientId")]
    client_id: ClientId,
    org: OrgId,
    #[serde(rename = "ownerEmail")]
    owner_email: String,
    matched: u64,
    updated: u64,
    confirmed: bool,
}

pub(super) async fn backfill_key_owner(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let owner_email = js_or_empty(body.get("ownerEmail"));
    if owner_email.is_empty() {
        return AppError::Validation("Owner is required for backfill.".to_owned()).into_response();
    }
    let confirm = matches!(body.get("confirm"), Some(Value::Bool(true)));
    match deps
        .admin
        .backfill_key_owner(ClientId(id), owner_email, confirm, audit)
        .await
    {
        Ok(Some(result)) => Json(OwnerBackfillResponse {
            client_id: result.client_id,
            org: result.org,
            owner_email: result.owner_email,
            matched: result.matched,
            updated: result.updated,
            confirmed: result.confirmed,
        })
        .into_response(),
        Ok(None) => AppError::ConcealedNotFound.into_response(),
        Err(error) => error.into_response(),
    }
}

#[derive(Serialize)]
struct CreatedKeyResponse {
    #[serde(rename = "clientId")]
    client_id: ClientId,
    org: OrgId,
    label: String,
    role: String,
    secret: String,
    created_at: String,
}
