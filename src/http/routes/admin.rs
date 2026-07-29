//! Owned by U18 (terra) — administrator settings, keys, orgs, and webhooks.

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Serialize;
use serde_json::{Map, Number, Value};

use crate::{
    AppDeps,
    config::{Clock, SystemClock},
    error::AppError,
    model::{
        ClientId, CreateOrganization, CreatePublisherKey, CreateWebhook, DeliveryResult,
        EmailAddress, OrgId, Viewer, WebhookEvent, WebhookId,
    },
    render::view_models::{SettingsOrganization, SettingsView},
    security::access::AccessPolicy,
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/settings", get(settings))
        .route("/settings/keys", post(create_key))
        .route("/settings/keys/{id}/revoke", post(revoke_key))
        .route("/settings/keys/{id}/owner", post(set_key_owner))
        .route(
            "/settings/keys/{id}/owner/backfill",
            post(backfill_key_owner),
        )
        .route("/settings/orgs", post(create_org))
        .route("/settings/orgs/{name}", delete(delete_org))
        .route("/settings/orgs/{name}/domains", post(add_domain))
        .route(
            "/settings/orgs/{name}/domains/{domain}",
            delete(remove_domain),
        )
        .route("/settings/orgs/{name}/emails", post(add_email_member))
        .route(
            "/settings/orgs/{name}/emails/{email}",
            delete(remove_email_member),
        )
        .route(
            "/settings/orgs/{name}/categories",
            post(add_category).delete(remove_category),
        )
        .route("/settings/orgs/{name}/color", post(set_color))
        .route("/settings/orgs/{name}/webhooks", post(create_webhook))
        .route(
            "/settings/orgs/{name}/webhooks/{id}",
            delete(remove_webhook).patch(update_webhook_events),
        )
        .route(
            "/settings/orgs/{name}/webhooks/{id}/test",
            post(test_webhook),
        )
}

async fn settings(State(deps): State<AppDeps>, headers: HeaderMap) -> Response {
    let viewer = match deps.viewer_identity.resolve(&headers).await {
        Ok(viewer) => viewer,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = AccessPolicy::admin_access(&viewer) {
        return (error.http_status(), error.to_string()).into_response();
    }

    let keys = match deps.admin.list_keys().await {
        Ok(keys) => keys,
        Err(error) => return error.into_response(),
    };
    let orgs = match deps.admin.list_orgs().await {
        Ok(orgs) => orgs,
        Err(error) => return error.into_response(),
    };
    let mut organizations = Vec::with_capacity(orgs.len());
    for organization in orgs {
        let webhooks = match deps.admin.list_webhooks(&organization.name).await {
            Ok(webhooks) => webhooks,
            Err(error) => return error.into_response(),
        };
        organizations.push(SettingsOrganization {
            organization,
            webhooks,
        });
    }

    let html = match deps.pages.settings(&SettingsView {
        viewer,
        keys,
        organizations,
    }) {
        Ok(html) => html,
        Err(error) => return error.into_response(),
    };
    ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

async fn create_key(State(deps): State<AppDeps>, request: Request) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
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
        .create_key(CreatePublisherKey {
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
        })
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

async fn revoke_key(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let revoked = match deps.admin.revoke_key(&ClientId(id.clone())).await {
        Ok(revoked) => revoked,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "id": id, "revoked": revoked })).into_response()
}

#[derive(Serialize)]
struct KeyOwnerResponse {
    #[serde(rename = "clientId")]
    client_id: ClientId,
    org: OrgId,
    #[serde(rename = "ownerEmail")]
    owner_email: Option<String>,
}

async fn set_key_owner(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let value = js_or_empty(body.get("ownerEmail"));
    match deps
        .admin
        .set_key_owner(ClientId(id), (!value.is_empty()).then_some(value))
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

async fn backfill_key_owner(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
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
        .backfill_key_owner(ClientId(id), owner_email, confirm)
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

async fn create_org(State(deps): State<AppDeps>, request: Request) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let domain = js_or_empty(body.get("domain"));
    let created = match deps
        .admin
        .create_org(CreateOrganization {
            name: OrgId(js_or_empty(body.get("name"))),
            label: js_or_empty(body.get("label")),
            domain: (!domain.is_empty()).then_some(domain),
        })
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

async fn delete_org(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let removed = match deps.admin.delete_org(&OrgId(name.clone())).await {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "name": name, "removed": removed })).into_response()
}

async fn add_domain(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let domain = js_or_empty(body.get("domain"));
    let normalized = match deps.admin.add_domain(&OrgId(name.clone()), &domain).await {
        Ok(domain) => domain,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "domain": normalized })).into_response()
}

async fn remove_domain(
    State(deps): State<AppDeps>,
    Path((name, domain)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let removed = match deps
        .admin
        .remove_domain(&OrgId(name.clone()), &domain)
        .await
    {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "domain": domain, "removed": removed })).into_response()
}

async fn add_email_member(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let email = EmailAddress(js_or_empty(body.get("email")));
    let normalized = match deps
        .admin
        .add_email_member(&OrgId(name.clone()), &email)
        .await
    {
        Ok(email) => email,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "email": normalized })).into_response()
}

async fn remove_email_member(
    State(deps): State<AppDeps>,
    Path((name, email)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let email = EmailAddress(js_trim(&email).to_lowercase());
    let removed = match deps
        .admin
        .remove_email_member(&OrgId(name.clone()), &email)
        .await
    {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "email": email, "removed": removed })).into_response()
}

async fn add_category(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let category = js_or_empty(body.get("name"));
    let normalized = match deps
        .admin
        .add_category(&OrgId(name.clone()), &category)
        .await
    {
        Ok(category) => category,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "name": normalized })).into_response()
}

async fn remove_category(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let requested_name = body.get("name").cloned();
    let category = js_or_empty(requested_name.as_ref());
    let removed = match deps
        .admin
        .remove_category(&OrgId(name.clone()), &category)
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

async fn set_color(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let requested = body.get("color");
    let color = js_or_empty(requested);
    let stored = match deps
        .admin
        .set_color(&OrgId(name.clone()), requested.map(|_| color.as_str()))
        .await
    {
        Ok(color) => color,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "name": name, "color": stored })).into_response()
}

async fn create_webhook(
    State(deps): State<AppDeps>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let events = match parse_webhook_events(body.get("events")) {
        Ok(events) => events,
        Err(error) => return error.into_response(),
    };
    let webhook = match deps
        .admin
        .create_webhook(CreateWebhook {
            org: OrgId(name),
            url: js_or_empty(body.get("url")),
            label: js_or_empty(body.get("label")),
            events,
        })
        .await
    {
        Ok(webhook) => webhook,
        Err(error) => return error.into_response(),
    };
    Json(webhook).into_response()
}

async fn remove_webhook(
    State(deps): State<AppDeps>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let webhook_id = id.clone().into();
    let removed = match deps
        .admin
        .remove_webhook(&OrgId(name.clone()), &webhook_id)
        .await
    {
        Ok(removed) => removed,
        Err(error) => return error.into_response(),
    };
    Json(serde_json::json!({ "org": name, "id": id, "removed": removed })).into_response()
}

async fn update_webhook_events(
    State(deps): State<AppDeps>,
    Path((name, id)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (_viewer, body) = match admin_json_request(&deps, request).await {
        Ok(authorized) => authorized,
        Err(response) => return response,
    };
    let events = match parse_webhook_events(body.get("events")) {
        Ok(events) => events.unwrap_or_default(),
        Err(error) => return error.into_response(),
    };
    match deps
        .admin
        .set_webhook_events(&OrgId(name), &id.into(), &events)
        .await
    {
        Ok(Some(webhook)) => Json(webhook).into_response(),
        Ok(None) => AppError::NotFound("Webhook not found".to_owned()).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn test_webhook(
    State(deps): State<AppDeps>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let delivery = match deps.admin.webhook_delivery(&WebhookId(id)).await {
        Ok(Some(delivery)) if delivery.org.0 == name => delivery,
        Ok(Some(_) | None) => {
            return AppError::NotFound("Webhook not found".to_owned()).into_response();
        }
        Err(error) => return error.into_response(),
    };
    let result = match deps.notifications.test(&delivery).await {
        Ok(result) => result,
        Err(error) => return error.into_response(),
    };
    Json(DeliveryResultResponse::from(result)).into_response()
}

fn parse_webhook_events(value: Option<&Value>) -> Result<Option<Vec<WebhookEvent>>, AppError> {
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

async fn require_admin(deps: &AppDeps, headers: &HeaderMap) -> Result<Viewer, Response> {
    let viewer = deps
        .viewer_identity
        .resolve(headers)
        .await
        .map_err(IntoResponse::into_response)?;
    AccessPolicy::admin_access(&viewer).map_err(IntoResponse::into_response)?;
    Ok(viewer)
}

async fn admin_json_request(deps: &AppDeps, request: Request) -> Result<(Viewer, Value), Response> {
    let headers = request.headers().clone();
    let body = parse_json_body(request, deps.config.body.key_json).await?;
    let viewer = require_admin(deps, &headers).await?;
    Ok((viewer, body))
}

async fn parse_json_body(request: Request, limit: u64) -> Result<Value, Response> {
    if !request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json_content_type)
    {
        return Ok(Value::Object(Map::new()));
    }
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let bytes = to_bytes(request.into_body(), limit)
        .await
        .map_err(|_| AppError::PayloadTooLarge.into_response())?;
    if bytes.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => Ok(value),
        Ok(_) | Err(_) => Err(AppError::Validation("invalid JSON".to_owned()).into_response()),
    }
}

fn is_json_content_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
}

fn js_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        Some(Value::Bool(true)) => "true".to_owned(),
        Some(Value::Number(value)) if value.as_f64() != Some(0.0) => js_number(value),
        Some(Value::Array(values)) if !values.is_empty() => values
            .iter()
            .map(js_array_element_string)
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_owned(),
        _ => String::new(),
    }
}

fn js_array_element_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => js_number(value),
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(js_array_element_string)
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

fn js_number(value: &Number) -> String {
    if let Some(integer) = value.as_i64() {
        return integer.to_string();
    }
    if let Some(integer) = value.as_u64() {
        return integer.to_string();
    }
    let Some(number) = value.as_f64() else {
        return "NaN".to_owned();
    };
    if number == 0.0 {
        return "0".to_owned();
    }
    let raw = serde_json::to_string(&number).unwrap_or_else(|_| "NaN".to_owned());
    let absolute = number.abs();
    if (1e-6..1e21).contains(&absolute) {
        decimal_notation(&raw)
    } else {
        exponent_notation(&raw)
    }
}

fn decimal_notation(raw: &str) -> String {
    let Some((mantissa, exponent)) = split_exponent(raw) else {
        return raw.strip_suffix(".0").unwrap_or(raw).to_owned();
    };
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.trim_start_matches('-');
    let mut digits = unsigned.replace('.', "");
    let decimal_at = i32::try_from(unsigned.find('.').unwrap_or(unsigned.len()))
        .unwrap_or(i32::MAX)
        .saturating_add(exponent);
    let rendered = if decimal_at <= 0 {
        format!(
            "0.{}{}",
            "0".repeat(usize::try_from(-decimal_at).unwrap_or(usize::MAX)),
            digits
        )
    } else if usize::try_from(decimal_at).map_or(true, |index| index >= digits.len()) {
        let decimal_at = usize::try_from(decimal_at).unwrap_or(usize::MAX);
        digits.push_str(&"0".repeat(decimal_at.saturating_sub(digits.len())));
        digits
    } else {
        let decimal_at = usize::try_from(decimal_at).unwrap_or_default();
        digits.insert(decimal_at, '.');
        digits
    };
    if negative {
        format!("-{rendered}")
    } else {
        rendered
    }
}

fn exponent_notation(raw: &str) -> String {
    let Some((mantissa, exponent)) = split_exponent(raw) else {
        return raw.strip_suffix(".0").unwrap_or(raw).to_owned();
    };
    let mantissa = mantissa.strip_suffix(".0").unwrap_or(mantissa);
    if exponent >= 0 {
        format!("{mantissa}e+{exponent}")
    } else {
        format!("{mantissa}e{exponent}")
    }
}

fn split_exponent(raw: &str) -> Option<(&str, i32)> {
    let position = raw.find(['e', 'E'])?;
    let exponent = raw.get(position + 1..)?.parse().ok()?;
    Some((&raw[..position], exponent))
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        matches!(character, '\u{feff}')
            || (character.is_whitespace() && !matches!(character, '\u{85}'))
    })
}

fn iso_timestamp(unix_millis: i64) -> String {
    let seconds = unix_millis.div_euclid(1000);
    let millis = unix_millis.rem_euclid(1000);
    let moment = time::OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second(),
        millis
    )
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
