use axum::{
    Json,
    extract::{Extension, Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{
    AppDeps,
    error::AppError,
    model::{
        CreateOrganization, CreateWebhook, DeliveryResult, EmailAddress, OrgId, WebhookEvent,
        WebhookId,
    },
    security::audit::{AuditRequestId, MutationAudit},
};

use super::{admin_json_request, data_error, js_or_empty, js_trim, require_admin, target_audit};
pub(super) async fn create_org(State(deps): State<AppDeps>, request: Request) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let domain = js_or_empty(body.get("domain"));
    let created = match deps
        .admin
        .create_org(
            CreateOrganization {
                name: OrgId(js_or_empty(body.get("name"))),
                label: js_or_empty(body.get("label")),
                domain: (!domain.is_empty()).then_some(domain),
            },
            audit,
        )
        .await
    {
        Ok(created) => created,
        Err(error) => return error.into_response(),
    };
    Json(CreatedOrganizationResponse {
        name: created.name,
        label: created.label,
        domains: created.domains,
        emails: created.emails,
        categories: created.categories,
        key_count: created.key_count,
    })
    .into_response()
}

pub(super) async fn delete_org(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    headers: HeaderMap,
    request_id: Option<Extension<AuditRequestId>>,
) -> Response {
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let audit =
        match MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref().map(|id| &id.0))
            .and_then(|audit| audit.admin_for_tenant(&name))
        {
            Ok(audit) => audit,
            Err(error) => return error.into_response(),
        };
    if deps
        .data
        .source_views(Some(&name))
        .await
        .is_ok_and(|sources| sources.iter().any(|source| source.origin == "managed"))
    {
        return data_error("org_in_use");
    }
    let removed = match deps.admin.delete_org(&OrgId(name.clone()), audit).await {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "name": name, "removed": removed })).into_response()
}

pub(super) async fn add_domain(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let domain = js_or_empty(body.get("domain"));
    let audit = match target_audit(audit, &name) {
        Ok(audit) => audit,
        Err(error) => return error.into_response(),
    };
    let normalized = match deps
        .admin
        .add_domain(&OrgId(name.clone()), &domain, audit)
        .await
    {
        Ok(domain) => domain,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "domain": normalized })).into_response()
}

pub(super) async fn remove_domain(
    State(deps): State<AppDeps>,
    Path((name, domain)): Path<(String, String)>,
    headers: HeaderMap,
    request_id: Option<Extension<AuditRequestId>>,
) -> Response {
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let audit =
        match MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref().map(|id| &id.0))
            .and_then(|audit| audit.admin_for_tenant(&name))
        {
            Ok(audit) => audit,
            Err(error) => return error.into_response(),
        };
    let removed = match deps
        .admin
        .remove_domain(&OrgId(name.clone()), &domain, audit)
        .await
    {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "domain": domain, "removed": removed })).into_response()
}

pub(super) async fn add_email_member(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let email = EmailAddress(js_or_empty(body.get("email")));
    let display_name = match body.get("display_name") {
        Some(value) => {
            let Some(value) = value.as_str() else {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"display_name must be a single line of at most 40 characters."}))).into_response();
            };
            match crate::persistence::orgs::normalize_display_name(value) {
                Ok(name) => Some(name),
                Err(error) => return error.into_response(),
            }
        }
        None => None,
    };
    let normalized = match deps
        .admin
        .add_email_member_with_name(
            &OrgId(name.clone()),
            &email,
            display_name.clone(),
            match target_audit(audit, &name) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
        Ok(email) => email,
        Err(error) => return error.into_response(),
    };
    let mut response = serde_json::json!({ "org": name, "email": normalized });
    let sync = deps.admin.access_sync_status();
    if sync.state != "disabled" {
        response["access_sync"] = serde_json::json!(sync);
    }
    if let Some(display_name) = display_name {
        response["display_name"] = display_name.into();
    }
    Json(response).into_response()
}

pub(super) async fn remove_email_member(
    State(deps): State<AppDeps>,
    Path((name, email)): Path<(String, String)>,
    headers: HeaderMap,
    request_id: Option<Extension<AuditRequestId>>,
) -> Response {
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let audit =
        match MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref().map(|id| &id.0))
            .and_then(|audit| audit.admin_for_tenant(&name))
        {
            Ok(audit) => audit,
            Err(error) => return error.into_response(),
        };
    let email = EmailAddress(js_trim(&email).to_lowercase());
    let removed = match deps
        .admin
        .remove_email_member(&OrgId(name.clone()), &email, audit)
        .await
    {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    let mut response = serde_json::json!({ "org": name, "email": email, "removed": removed });
    let sync = deps.admin.access_sync_status();
    if sync.state != "disabled" {
        response["access_sync"] = serde_json::json!(sync);
    }
    Json(response).into_response()
}

pub(super) async fn add_category(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let category = js_or_empty(body.get("name"));
    let normalized = match deps
        .admin
        .add_category(
            &OrgId(name.clone()),
            &category,
            match target_audit(audit, &name) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
        Ok(category) => category,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "name": normalized })).into_response()
}

pub(super) async fn remove_category(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let requested_name = body.get("name").cloned();
    let category = js_or_empty(requested_name.as_ref());
    let removed = match deps
        .admin
        .remove_category(
            &OrgId(name.clone()),
            &category,
            match target_audit(audit, &name) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    let mut response = Map::new();
    response.insert("org".to_owned(), Value::String(name));
    if let Some(requested_name) = requested_name {
        response.insert("name".to_owned(), requested_name);
    }
    response.insert("removed".to_owned(), Value::Bool(removed));
    Json(Value::Object(response)).into_response()
}

pub(super) async fn set_color(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let requested = body.get("color");
    let color = js_or_empty(requested);
    let stored = match deps
        .admin
        .set_color(
            &OrgId(name.clone()),
            requested.map(|_| color.as_str()),
            match target_audit(audit, &name) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
        Ok(color) => color,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "name": name, "color": stored })).into_response()
}

pub(super) async fn create_webhook(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let events = match parse_webhook_events(body.get("events")) {
        Ok(events) => events,
        Err(error) => return error.into_response(),
    };
    let webhook = match deps
        .admin
        .create_webhook(
            CreateWebhook {
                org: OrgId(name.clone()),
                url: js_or_empty(body.get("url")),
                label: js_or_empty(body.get("label")),
                events,
            },
            match target_audit(audit, &name) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
        Ok(webhook) => webhook,
        Err(error) => return error.into_response(),
    };
    Json(webhook).into_response()
}

pub(super) async fn remove_webhook(
    State(deps): State<AppDeps>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
    request_id: Option<Extension<AuditRequestId>>,
) -> Response {
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let audit =
        match MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref().map(|id| &id.0))
            .and_then(|audit| audit.admin_for_tenant(&name))
        {
            Ok(audit) => audit,
            Err(error) => return error.into_response(),
        };
    let webhook_id = id.clone().into();
    let removed = match deps
        .admin
        .remove_webhook(&OrgId(name.clone()), &webhook_id, audit)
        .await
    {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "id": id, "removed": removed })).into_response()
}

pub(super) async fn update_webhook_events(
    State(deps): State<AppDeps>,
    Path((name, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let events = match parse_webhook_events(body.get("events")) {
        Ok(events) => events.unwrap_or_default(),
        Err(error) => return error.into_response(),
    };
    match deps
        .admin
        .set_webhook_events(
            &OrgId(name.clone()),
            &id.into(),
            &events,
            match target_audit(audit, &name) {
                Ok(audit) => audit,
                Err(error) => return error.into_response(),
            },
        )
        .await
    {
        Ok(Some(webhook)) => Json(webhook).into_response(),
        Ok(None) => AppError::NotFound("Webhook not found".to_owned()).into_response(),
        Err(error) => error.into_response(),
    }
}

pub(super) async fn test_webhook(
    State(deps): State<AppDeps>,
    Path((name, id)): Path<(String, String)>,
    request_id: Option<Extension<AuditRequestId>>,
    headers: HeaderMap,
) -> Response {
    // External delivery cannot share a SQLite transaction. We therefore commit a requested
    // marker before I/O and a correlated terminal result afterward. A crash in the gap leaves an
    // actionable requested-only record; neither record contains the decrypted URL.
    let viewer = match require_admin(&deps, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let delivery = match deps.admin.webhook_delivery(&WebhookId(id)).await {
        Ok(Some(delivery)) if delivery.org.0 == name => delivery,
        Ok(Some(_) | None) => {
            return AppError::NotFound("Webhook not found".to_owned()).into_response();
        }
        Err(error) => return error.into_response(),
    };
    let audit =
        match MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref().map(|id| &id.0)) {
            Ok(audit) => audit,
            Err(error) => return error.into_response(),
        };
    if let Err(error) = deps
        .admin
        .audit_webhook_test(&delivery.org, &delivery.id, None, audit.clone())
        .await
    {
        return error.into_response();
    }
    let result = match deps.notifications.test(&delivery).await {
        Ok(result) => result,
        Err(delivery_error) => {
            if let Err(audit_error) = deps
                .admin
                .audit_webhook_test(&delivery.org, &delivery.id, Some(false), audit)
                .await
            {
                return audit_error.into_response();
            }
            return delivery_error.into_response();
        }
    };
    if let Err(error) = deps
        .admin
        .audit_webhook_test(&delivery.org, &delivery.id, Some(result.ok), audit)
        .await
    {
        return error.into_response();
    }
    Json(DeliveryResultResponse::from(result)).into_response()
}

pub(super) fn parse_webhook_events(
    value: Option<&Value>,
) -> Result<Option<Vec<WebhookEvent>>, AppError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Value::Array(values) = value else {
        return Err(AppError::Validation(
            "Webhook events must be an array.".to_owned(),
        ));
    };
    let mut events = Vec::with_capacity(values.len());
    for value in values {
        let name = js_trim(&js_or_empty(Some(value))).to_owned();
        let event = match name.as_str() {
            "published" => WebhookEvent::Published,
            "updated" => WebhookEvent::Updated,
            "restored" => WebhookEvent::Restored,
            "deleted" => WebhookEvent::Deleted,
            "feedback" => WebhookEvent::Feedback,
            "resolved" => WebhookEvent::Resolved,
            _ => {
                return Err(AppError::Validation(format!(
                    "Unknown webhook event: {name}"
                )));
            }
        };
        events.push(event);
    }
    Ok(Some(events))
}

#[derive(Serialize)]
struct CreatedOrganizationResponse {
    name: OrgId,
    label: String,
    domains: Vec<String>,
    emails: Vec<String>,
    categories: Vec<String>,
    #[serde(rename = "keyCount")]
    key_count: u64,
}

#[derive(Serialize)]
struct DeliveryResultResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<DeliveryResult> for DeliveryResultResponse {
    fn from(result: DeliveryResult) -> Self {
        Self {
            ok: result.ok,
            error: result.error,
        }
    }
}
