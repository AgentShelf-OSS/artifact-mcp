//! ADR-0012 Web Push subscriptions, per-artifact opt-ins, reminders, and the static service
//! worker, manifest, and icons.
//!
//! The artifact routes use viewer state's identity resolution, concealed `404`, and admin
//! handling ([`authorize`]). The request-authenticity gate applies to every PUT/DELETE through
//! the global middleware, and ingress classifies these paths into the state rate-limit budget.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    AppDeps,
    error::AppError,
    http::{
        ingress::ViewerCost,
        routes::{
            artifact::{authorize, parse_json_request},
            state::scope_query,
        },
    },
    integrations::push_runtime::{PushService, ReminderInput},
    mcp::protocol::OrderedJson,
    model::Viewer,
    persistence::push::{self, PushError, ReminderScope},
    render::portal::{artifact_install_name, artifact_install_short_name},
};

/// Push JSON bodies are small; the contract mirrors Node's `8kb` parser limit.
pub const PUSH_JSON_LIMIT: u64 = 8 * 1024;

const SERVICE_WORKER: &str = include_str!("../../../assets/push-sw.js");
const MANIFEST: &str = include_str!("../../../assets/manifest.webmanifest");
const ICON_APP_192: &[u8] = include_bytes!("../../../assets/icons/app-192.png");
const ICON_APP_512: &[u8] = include_bytes!("../../../assets/icons/app-512.png");
const ICON_MASKABLE_512: &[u8] = include_bytes!("../../../assets/icons/maskable-512.png");
const ICON_APPLE_TOUCH: &[u8] = include_bytes!("../../../assets/icons/apple-touch-icon.png");
const ICON_BADGE_72: &[u8] = include_bytes!("../../../assets/icons/badge-72.png");

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/sw.js", get(service_worker))
        .route("/manifest.webmanifest", get(manifest))
        .route("/{id}/manifest.webmanifest", get(artifact_manifest))
        .route("/icons/{name}", get(icon))
        .route("/push/config", get(config))
        .route(
            "/push/subscriptions",
            axum::routing::put(put_subscription).delete(delete_subscription),
        )
        .route("/{id}/push", get(status))
        .route(
            "/{id}/push/optin",
            axum::routing::put(opt_in).delete(opt_out),
        )
        .route("/{id}/reminders", get(list_reminders))
        .route(
            "/{id}/reminders/{key}",
            axum::routing::put(put_reminder).delete(delete_reminder),
        )
}

// ---------------------------------------------------------------------------
// Static files (no app-level auth; no private data)
// ---------------------------------------------------------------------------

fn static_file(body: impl Into<Body>, headers: &[(&'static str, &'static str)]) -> Response {
    let mut response = body.into().into_response();
    for (name, value) in headers {
        response
            .headers_mut()
            .insert(*name, HeaderValue::from_static(value));
    }
    response
}

async fn service_worker() -> Response {
    static_file(
        SERVICE_WORKER,
        &[
            ("content-type", "text/javascript; charset=utf-8"),
            ("cache-control", "no-cache"),
            ("service-worker-allowed", "/"),
            ("x-content-type-options", "nosniff"),
        ],
    )
}

async fn manifest() -> Response {
    static_file(
        MANIFEST,
        &[
            ("content-type", "application/manifest+json"),
            ("cache-control", "no-cache"),
            ("x-content-type-options", "nosniff"),
        ],
    )
}

/// Per-artifact install manifest: the site manifest with `id`, `start_url`, and `scope` set to
/// `/{id}` and the artifact's install names.
///
/// It names a private artifact, so it uses the viewer page's identity resolution, org
/// authorization, admin handling, and concealed `404` ([`authorize`]). Public shares get no
/// per-artifact manifest. Object keys are sorted recursively (`serde_json::Map` is ordered by
/// key, and `sort_all_objects` keeps that true under `preserve_order`) and the JSON is compact,
/// which is byte-identical to Node's `artifactWebManifest`.
async fn artifact_manifest(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (artifact, _viewer) = match authorize(&deps, &headers, &id).await {
        Ok(found) => found,
        Err(error) => return error.into_response(),
    };
    match artifact_manifest_json(&artifact.meta().id.0, &artifact.meta().title) {
        Some(body) => static_file(
            body,
            &[
                ("content-type", "application/manifest+json"),
                ("cache-control", "private, no-cache"),
                ("x-content-type-options", "nosniff"),
            ],
        ),
        None => AppError::Internal.into_response(),
    }
}

/// Build the per-artifact manifest JSON from `assets/manifest.webmanifest`.
#[must_use]
pub fn artifact_manifest_json(id: &str, title: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(MANIFEST).ok()?;
    let fields = value.as_object_mut()?;
    let path = format!("/{id}");
    for key in ["id", "start_url", "scope"] {
        fields.insert(key.to_owned(), serde_json::Value::String(path.clone()));
    }
    fields.insert(
        "name".to_owned(),
        serde_json::Value::String(artifact_install_name(title)),
    );
    fields.insert(
        "short_name".to_owned(),
        serde_json::Value::String(artifact_install_short_name(title)),
    );
    fields.insert(
        "display".to_owned(),
        serde_json::Value::String("standalone".to_owned()),
    );
    value.sort_all_objects();
    serde_json::to_string(&value).ok()
}

async fn icon(Path(name): Path<String>) -> Response {
    let bytes = match name.as_str() {
        "app-192.png" => ICON_APP_192,
        "app-512.png" => ICON_APP_512,
        "maskable-512.png" => ICON_MASKABLE_512,
        "apple-touch-icon.png" => ICON_APPLE_TOUCH,
        "badge-72.png" => ICON_BADGE_72,
        _ => return AppError::ConcealedNotFound.into_response(),
    };
    static_file(
        bytes,
        &[
            ("content-type", "image/png"),
            ("cache-control", "public, max-age=3600"),
            ("x-content-type-options", "nosniff"),
        ],
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error(status: StatusCode, code: &str) -> Response {
    no_store((status, Json(serde_json::json!({ "error": code }))).into_response())
}

fn push_error(failure: PushError) -> Response {
    error(
        StatusCode::from_u16(failure.status()).unwrap_or(StatusCode::BAD_REQUEST),
        failure.code(),
    )
}

fn app_error(failure: AppError) -> Response {
    no_store(failure.into_response())
}

fn disabled() -> Response {
    error(StatusCode::NOT_FOUND, "push_disabled")
}

fn ok_json(value: serde_json::Value) -> Response {
    no_store(Json(value).into_response())
}

fn no_content() -> Response {
    no_store(StatusCode::NO_CONTENT.into_response())
}

/// Signed-in viewer for the non-artifact routes: a viewer without an email is concealed.
async fn signed_in_viewer(
    deps: &AppDeps,
    headers: &HeaderMap,
) -> Result<(Viewer, String), Response> {
    let viewer = deps
        .viewer_identity
        .resolve(headers)
        .await
        .map_err(app_error)?;
    if !deps
        .ingress
        .allow_verified_viewer(headers, &viewer, ViewerCost::Read)
    {
        return Err(app_error(AppError::RateLimited));
    }
    let Some(email) = viewer.email.as_ref().map(|email| email.0.clone()) else {
        return Err(app_error(AppError::ConcealedNotFound));
    };
    refresh_org(deps, &viewer).await;
    Ok((viewer, email))
}

/// Keep the viewer's subscriptions on their current organization (best effort).
async fn refresh_org(deps: &AppDeps, viewer: &Viewer) {
    if let (Some(service), Some(email)) = (deps.push.as_ref(), viewer.email.as_ref()) {
        let org = viewer
            .org
            .as_ref()
            .map(|org| org.0.clone())
            .unwrap_or_default();
        service.refresh_viewer_org(email.0.clone(), org).await;
    }
}

/// Mutations spend the viewer-state budget after concealment and the disabled check.
fn state_budget(deps: &AppDeps, headers: &HeaderMap) -> Result<(), Box<Response>> {
    if deps.ingress.allow_state_request(headers) {
        Ok(())
    } else {
        Err(Box::new(error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
        )))
    }
}

fn service(deps: &AppDeps) -> Result<Arc<PushService>, Box<Response>> {
    deps.push.clone().ok_or_else(|| Box::new(disabled()))
}

async fn json_body(deps: &AppDeps, request: Request) -> Result<OrderedJson, Response> {
    match parse_json_request(request, PUSH_JSON_LIMIT, &deps.config.ingress).await {
        Ok((_, body)) => Ok(body),
        Err(response) => Err(match response.status() {
            StatusCode::PAYLOAD_TOO_LARGE => error(StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            StatusCode::BAD_REQUEST | StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                error(StatusCode::BAD_REQUEST, "bad_body")
            }
            _ => no_store(response),
        }),
    }
}

fn object_fields(body: &OrderedJson) -> Option<&[(String, OrderedJson)]> {
    match body {
        OrderedJson::Object(fields) => Some(fields.as_slice()),
        _ => None,
    }
}

/// Authorized artifact + push service + viewer email, in the Node order: concealment, then
/// the disabled check.
struct ArtifactGrant {
    service: Arc<PushService>,
    artifact_id: String,
    org: String,
    viewer: Viewer,
}

async fn artifact_grant(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
    mutation: bool,
) -> Result<ArtifactGrant, Response> {
    let (artifact, viewer) = authorize(deps, headers, id).await.map_err(app_error)?;
    let service = service(deps).map_err(|response| *response)?;
    refresh_org(deps, &viewer).await;
    if mutation {
        state_budget(deps, headers).map_err(|response| *response)?;
    }
    let meta = artifact.into_meta();
    Ok(ArtifactGrant {
        service,
        artifact_id: meta.id.0,
        org: meta.org.0,
        viewer,
    })
}

fn email_of(viewer: &Viewer) -> Result<String, Box<Response>> {
    viewer
        .email
        .as_ref()
        .map(|email| email.0.clone())
        .ok_or_else(|| Box::new(app_error(AppError::ConcealedNotFound)))
}

/// `stateNamespace`: `bad_scope` for an invalid scope, concealed `404` for a viewer scope without
/// a viewer identity. Returns the scope and the `owner` column value.
fn namespace(
    request_uri: &axum::http::Uri,
    viewer: &Viewer,
) -> Result<(ReminderScope, String), Box<Response>> {
    let scope =
        scope_query(request_uri).map_err(|code| Box::new(error(StatusCode::BAD_REQUEST, code)))?;
    let email = viewer.email.as_ref().map(|email| email.0.as_str());
    if scope == ReminderScope::Viewer && email.is_none() {
        return Err(Box::new(app_error(AppError::ConcealedNotFound)));
    }
    Ok((scope, push::reminder_owner(scope, email)))
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

async fn config(State(deps): State<AppDeps>, headers: HeaderMap) -> Response {
    if let Err(response) = signed_in_viewer(&deps, &headers).await {
        return response;
    }
    ok_json(serde_json::json!({
        "enabled": deps.push.is_some(),
        "vapid_public_key": deps.push.as_ref().map(|service| service.public_key()),
    }))
}

async fn put_subscription(State(deps): State<AppDeps>, request: Request) -> Response {
    let (viewer, email) = match signed_in_viewer(&deps, request.headers()).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let service = match service(&deps) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    if let Err(response) = state_budget(&deps, request.headers()) {
        return *response;
    }
    let body = match json_body(&deps, request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(fields) = object_fields(&body) else {
        return error(StatusCode::BAD_REQUEST, "bad_body");
    };
    if fields.iter().any(|(name, _)| {
        !matches!(
            name.as_str(),
            "endpoint" | "keys" | "label" | "expirationTime"
        )
    }) {
        return error(StatusCode::BAD_REQUEST, "bad_body");
    }
    let label = match body.get("label") {
        None => String::new(),
        Some(OrderedJson::String(label)) if push::valid_label(label) => label.clone(),
        Some(_) => return error(StatusCode::BAD_REQUEST, "bad_body"),
    };
    let keys = body.get("keys");
    if let Some(OrderedJson::Object(key_fields)) = keys
        && key_fields
            .iter()
            .any(|(name, _)| name != "p256dh" && name != "auth")
    {
        return error(StatusCode::BAD_REQUEST, "bad_keys");
    }
    let Some(OrderedJson::String(endpoint)) = body.get("endpoint") else {
        return push_error(PushError::BadEndpoint);
    };
    let key = |name: &str| {
        keys.and_then(|keys| keys.get(name))
            .and_then(OrderedJson::as_str)
            .map(ToOwned::to_owned)
    };
    if !push::valid_endpoint(endpoint, &deps.config.web_push.endpoint_hosts) {
        return push_error(PushError::BadEndpoint);
    }
    let (Some(p256dh), Some(auth)) = (key("p256dh"), key("auth")) else {
        return push_error(PushError::BadKeys);
    };
    let org = viewer.org.map(|org| org.0).unwrap_or_default();
    match service
        .save_subscription(org, email, endpoint.clone(), p256dh, auth, label)
        .await
    {
        Ok(id) => ok_json(serde_json::json!({ "id": id })),
        Err(failure) => push_error(failure),
    }
}

async fn delete_subscription(State(deps): State<AppDeps>, request: Request) -> Response {
    let (_, email) = match signed_in_viewer(&deps, request.headers()).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let service = match service(&deps) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    if let Err(response) = state_budget(&deps, request.headers()) {
        return *response;
    }
    let body = match json_body(&deps, request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let valid_shape = object_fields(&body)
        .is_some_and(|fields| fields.iter().all(|(name, _)| name == "endpoint"));
    let Some(OrderedJson::String(endpoint)) = body.get("endpoint").filter(|_| valid_shape) else {
        return error(StatusCode::BAD_REQUEST, "bad_body");
    };
    match service.remove_subscription(email, endpoint.clone()).await {
        Ok(()) => no_content(),
        Err(failure) => push_error(failure),
    }
}

// ---------------------------------------------------------------------------
// Opt-ins
// ---------------------------------------------------------------------------

async fn status(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let grant = match artifact_grant(&deps, &headers, &id, false).await {
        Ok(grant) => grant,
        Err(response) => return response,
    };
    let email = match email_of(&grant.viewer) {
        Ok(email) => email,
        Err(response) => return *response,
    };
    match grant
        .service
        .status(grant.artifact_id, grant.org, email)
        .await
    {
        Ok((opted_in, devices)) => ok_json(serde_json::json!({
            "enabled": true, "opted_in": opted_in, "devices": devices,
        })),
        Err(failure) => push_error(failure),
    }
}

async fn set_opt_in(deps: AppDeps, id: String, headers: HeaderMap, value: bool) -> Response {
    let grant = match artifact_grant(&deps, &headers, &id, true).await {
        Ok(grant) => grant,
        Err(response) => return response,
    };
    let email = match email_of(&grant.viewer) {
        Ok(email) => email,
        Err(response) => return *response,
    };
    match grant
        .service
        .set_opt_in(grant.artifact_id, grant.org, email, value)
        .await
    {
        Ok(()) => ok_json(serde_json::json!({ "opted_in": value })),
        Err(failure) => push_error(failure),
    }
}

async fn opt_in(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    set_opt_in(deps, id, headers, true).await
}

async fn opt_out(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    set_opt_in(deps, id, headers, false).await
}

// ---------------------------------------------------------------------------
// Reminders
// ---------------------------------------------------------------------------

async fn list_reminders(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let grant = match artifact_grant(&deps, request.headers(), &id, false).await {
        Ok(grant) => grant,
        Err(response) => return response,
    };
    let (scope, owner) = match namespace(request.uri(), &grant.viewer) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    match grant
        .service
        .list_reminders(grant.artifact_id, scope, owner)
        .await
    {
        Ok(reminders) => ok_json(serde_json::json!({ "reminders": reminders })),
        Err(failure) => push_error(failure),
    }
}

async fn put_reminder(
    State(deps): State<AppDeps>,
    Path((id, key)): Path<(String, String)>,
    request: Request,
) -> Response {
    let grant = match artifact_grant(&deps, request.headers(), &id, true).await {
        Ok(grant) => grant,
        Err(response) => return response,
    };
    let (scope, owner) = match namespace(request.uri(), &grant.viewer) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    if !push::valid_key(&key) {
        return push_error(PushError::BadKey);
    }
    let body = match json_body(&deps, request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(fields) = object_fields(&body) else {
        return error(StatusCode::BAD_REQUEST, "bad_body");
    };
    if fields.iter().any(|(name, _)| {
        !matches!(
            name.as_str(),
            "fire_at" | "delay_seconds" | "title" | "body"
        )
    }) {
        return error(StatusCode::BAD_REQUEST, "bad_body");
    }
    let input = ReminderInput {
        fire_at: body.get("fire_at"),
        delay_seconds: body.get("delay_seconds"),
        title: body.get("title"),
        body: body.get("body"),
    };
    match grant
        .service
        .set_reminder(
            grant.artifact_id,
            grant.org,
            scope,
            owner,
            key.clone(),
            input,
            "viewer".to_owned(),
        )
        .await
    {
        Ok(saved) => ok_json(serde_json::json!({
            "key": key, "scope": scope.as_str(), "fire_at": saved.fire_at, "revision": saved.revision,
        })),
        Err(failure) => push_error(failure),
    }
}

async fn delete_reminder(
    State(deps): State<AppDeps>,
    Path((id, key)): Path<(String, String)>,
    request: Request,
) -> Response {
    let grant = match artifact_grant(&deps, request.headers(), &id, true).await {
        Ok(grant) => grant,
        Err(response) => return response,
    };
    let (scope, owner) = match namespace(request.uri(), &grant.viewer) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    if !push::valid_key(&key) {
        return push_error(PushError::BadKey);
    }
    match grant
        .service
        .clear_reminder(grant.artifact_id, scope, owner, key)
        .await
    {
        Ok(()) => no_content(),
        Err(failure) => push_error(failure),
    }
}
