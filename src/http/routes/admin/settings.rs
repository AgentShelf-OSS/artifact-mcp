use axum::{
    Json,
    extract::State,
    http::{HeaderMap, header},
    response::{Html, IntoResponse, Response},
};

use crate::{
    AppDeps,
    error::AppError,
    http::ingress::ViewerCost,
    render::view_models::{SettingsOrganization, SettingsView},
    security::access::AccessPolicy,
};

use super::require_admin;

pub(super) async fn access_sync_status(
    State(deps): State<AppDeps>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let mut response = Json(deps.admin.access_sync_status()).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

pub(super) async fn settings(State(deps): State<AppDeps>, headers: HeaderMap) -> Response {
    let viewer = match deps.viewer_identity.resolve(&headers).await {
        Ok(viewer) => viewer,
        Err(error) => return error.into_response(),
    };
    if !deps
        .ingress
        .allow_verified_viewer(&headers, &viewer, ViewerCost::Admin)
    {
        return AppError::RateLimited.into_response();
    }
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
