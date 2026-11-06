use axum::{
    Json,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::{AppDeps, ports::data::DataError};

use super::{admin_json_request, require_admin};
pub(super) async fn list_data_sources(
    State(deps): State<AppDeps>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let org = url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
        .find(|(k, _)| k == "org")
        .map(|(_, v)| v.into_owned());
    match deps.data.source_views(org.as_deref()).await {
        Ok(connections) => Json(serde_json::json!({
            "connections": connections,
            "orgs": deps.admin.org_names().await.unwrap_or_default(),
            "credential_variables": crate::data::DataBroker::managed_credential_variables(),
        }))
        .into_response(),
        Err(error) => connection_error(&error),
    }
}
pub(super) async fn get_data_source(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    match deps.data.source_view_by_id(&id).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => connection_error(&error),
    }
}
pub(super) async fn create_data_source(State(deps): State<AppDeps>, request: Request) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !object_keys_allowed(&body, &["definition", "enabled"]) {
        return connection_error(&DataError::BadParams);
    }
    let Some(definition) = body.get("definition") else {
        return connection_error(&DataError::BadParams);
    };
    let Ok(source) = serde_json::from_value::<crate::data::SourceConfig>(definition.clone()) else {
        return connection_error(&DataError::InvalidSource);
    };
    let enabled = match body.get("enabled") {
        None => true,
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => return connection_error(&DataError::BadParams),
        },
    };
    match deps
        .data
        .create_managed_source(source, enabled, audit)
        .await
    {
        Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
        Err(error) => connection_error(&error),
    }
}
pub(super) async fn update_data_source(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !object_keys_allowed(&body, &["definition", "enabled", "expected_version"]) {
        return connection_error(&DataError::BadParams);
    }
    let Some(definition) = body.get("definition") else {
        return connection_error(&DataError::BadParams);
    };
    let Ok(source) = serde_json::from_value::<crate::data::SourceConfig>(definition.clone()) else {
        return connection_error(&DataError::InvalidSource);
    };
    if source.id != id {
        return connection_error(&DataError::InvalidSource);
    }
    let Some(expected) = body.get("expected_version").and_then(Value::as_u64) else {
        return connection_error(&DataError::BadParams);
    };
    let enabled = match body.get("enabled") {
        None => true,
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => return connection_error(&DataError::BadParams),
        },
    };
    match deps
        .data
        .set_managed_source(&id, source, enabled, expected, audit)
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => connection_error(&error),
    }
}
pub(super) async fn enable_data_source(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    set_data_source_enabled(deps, id, request, true).await
}
pub(super) async fn disable_data_source(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    set_data_source_enabled(deps, id, request, false).await
}
async fn set_data_source_enabled(
    deps: AppDeps,
    id: String,
    request: Request,
    enabled: bool,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !object_keys_allowed(&body, &["expected_version"]) {
        return connection_error(&DataError::BadParams);
    }
    let Some(expected) = body.get("expected_version").and_then(Value::as_u64) else {
        return connection_error(&DataError::BadParams);
    };
    match deps
        .data
        .set_managed_enabled(&id, enabled, expected, audit)
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => connection_error(&error),
    }
}
pub(super) async fn delete_data_source(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !object_keys_allowed(&body, &["expected_version"]) {
        return connection_error(&DataError::BadParams);
    }
    let Some(expected) = body.get("expected_version").and_then(Value::as_u64) else {
        return connection_error(&DataError::BadParams);
    };
    match deps.data.delete_managed(&id, expected, audit).await {
        Ok(()) => Json(serde_json::json!({"deleted":true,"id":id})).into_response(),
        Err(error) => connection_error(&error),
    }
}
pub(super) async fn data_source_impact(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    let source = match deps.data.source_view_by_id(&id).await {
        Ok(source) => source,
        Err(error) => return connection_error(&error),
    };
    match deps.data.impact(&id).await {
        Ok(artifacts) => {
            Json(serde_json::json!({"artifacts":artifacts,"can_delete":source.origin == "managed" && artifacts.is_empty()}))
                .into_response()
        }
        Err(error) => connection_error(&error),
    }
}
pub(super) async fn test_data_source(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !object_keys_allowed(&body, &["operation", "params"]) {
        return connection_error(&DataError::BadParams);
    }
    if body
        .get("operation")
        .is_some_and(|value| !value.is_string())
    {
        return connection_error(&DataError::BadParams);
    }
    let operation = body.get("operation").and_then(Value::as_str).unwrap_or("");
    let params = body
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match deps.data.test_source(&id, operation, params).await {
        Ok(result) => {
            let _ = deps
                .data
                .audit_mutation(
                    audit,
                    "data_source.test",
                    &id,
                    "",
                    if result.ok { "success" } else { "failure" },
                    None,
                )
                .await;
            Json(result).into_response()
        }
        Err(error) => connection_error(&error),
    }
}
// Compatibility for the organization route outside this change's owned scope.
pub(super) fn data_error(_reason: &str) -> Response {
    connection_error(&DataError::OrgInUse)
}
fn connection_error(error: &DataError) -> Response {
    let status = match error {
        DataError::NotFound => StatusCode::NOT_FOUND,
        DataError::SourceConflict
        | DataError::OperatorReadOnly
        | DataError::StaleVersion
        | DataError::SourceInUse
        | DataError::BindingInUse
        | DataError::OrgInUse => StatusCode::CONFLICT,
        DataError::Unavailable | DataError::Internal | DataError::TestUnavailable => {
            StatusCode::BAD_GATEWAY
        }
        DataError::BadParams
        | DataError::TooLarge
        | DataError::InvalidSource
        | DataError::InvalidDataSources
        | DataError::MissingReference
        | DataError::CredentialReferenceNotAllowed
        | DataError::UnknownOrg
        | DataError::ConfigError => StatusCode::BAD_REQUEST,
    };
    let message = match error {
        DataError::BadParams => "Request parameters are invalid.",
        DataError::InvalidSource => "The connection definition is invalid.",
        DataError::MissingReference => "A referenced credential is unavailable.",
        DataError::CredentialReferenceNotAllowed => {
            "Credential variables must be named ARTIFACT_DATA_CREDENTIAL_<NAME>, and cannot set \
             Host, Cookie, Connection, Content-Length, or Transfer-Encoding."
        }
        DataError::UnknownOrg => "The selected organization does not exist.",
        DataError::SourceConflict => "A connection with this id already exists.",
        DataError::StaleVersion => "Connection changed; reload before saving.",
        DataError::SourceInUse => "Remove all artifact bindings before deleting this connection.",
        DataError::BindingInUse => "Remove or migrate the affected bindings first.",
        DataError::OrgInUse => "Move or remove bindings before changing organization.",
        DataError::OperatorReadOnly => "Operator connections are read-only.",
        DataError::TestUnavailable => "The connection could not be tested.",
        DataError::Unavailable | DataError::Internal => "Connection data is unavailable.",
        DataError::NotFound => "Connection not found.",
        DataError::TooLarge | DataError::InvalidDataSources | DataError::ConfigError => {
            "The connection operation failed."
        }
    };
    (
        status,
        Json(serde_json::json!({"error":error.reason(),"message":message})),
    )
        .into_response()
}
pub(super) fn object_keys_allowed(value: &Value, allowed: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())))
}
pub(super) async fn get_data_bindings(
    State(deps): State<AppDeps>,
    Path(artifact_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_admin(&deps, &headers).await {
        return response;
    }
    if deps
        .artifacts
        .find_meta(&artifact_id.clone().into())
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not_found"})),
        )
            .into_response();
    }
    let value = deps.data.get_bindings(&artifact_id).await;
    Json(serde_json::json!({"id":artifact_id,"bindings":value.bindings})).into_response()
}
pub(super) async fn update_data_bindings(
    State(deps): State<AppDeps>,
    Path(artifact_id): Path<String>,
    request: Request,
) -> Response {
    let (_viewer, body, audit) = match admin_json_request(&deps, request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    if !object_keys_allowed(&body, &["bindings"]) {
        return connection_error(&DataError::BadParams);
    }
    let Some(raw) = body.get("bindings") else {
        return connection_error(&DataError::BadParams);
    };
    let Ok(manifest) = serde_json::from_value::<crate::data::BindingManifest>(
        serde_json::json!({"bindings": raw}),
    ) else {
        return connection_error(&DataError::BadParams);
    };
    let Some(meta) = deps
        .artifacts
        .find_meta(&artifact_id.into())
        .await
        .ok()
        .flatten()
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not_found"})),
        )
            .into_response();
    };
    match deps
        .data
        .set_bindings_audited(&meta.id.0, manifest, &meta.org.0, audit)
        .await
    {
        Ok(()) => {
            Json(serde_json::json!({"id":meta.id,"bindings":deps.data.get_bindings(&meta.id.0).await.bindings})).into_response()
        }
        Err(error) => connection_error(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn typed_errors_preserve_status_and_body() {
        let statuses = [
            404, 409, 409, 409, 409, 409, 409, 502, 502, 400, 400, 400, 400, 400, 400, 400, 400,
            502,
        ];
        for ((error, token), status) in crate::data::error_cases().into_iter().zip(statuses) {
            let response = connection_error(&error);
            assert_eq!(response.status().as_u16(), status, "{token}");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"], token);
            let message = match token {
                "bad_params" => "Request parameters are invalid.",
                "invalid_source" => "The connection definition is invalid.",
                "missing_reference" => "A referenced credential is unavailable.",
                "credential_reference_not_allowed" => {
                    "Credential variables must be named ARTIFACT_DATA_CREDENTIAL_<NAME>, and cannot set Host, Cookie, Connection, Content-Length, or Transfer-Encoding."
                }
                "unknown_org" => "The selected organization does not exist.",
                "source_conflict" => "A connection with this id already exists.",
                "stale_version" => "Connection changed; reload before saving.",
                "source_in_use" => "Remove all artifact bindings before deleting this connection.",
                "binding_in_use" => "Remove or migrate the affected bindings first.",
                "org_in_use" => "Move or remove bindings before changing organization.",
                "operator_read_only" => "Operator connections are read-only.",
                "test_unavailable" => "The connection could not be tested.",
                "data_unavailable" => "Connection data is unavailable.",
                "not_found" => "Connection not found.",
                _ => "The connection operation failed.",
            };
            assert_eq!(value, serde_json::json!({"error":token,"message":message}));
        }
    }
}
