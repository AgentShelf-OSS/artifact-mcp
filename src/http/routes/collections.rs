//! Organization collection and gallery preference API.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppDeps,
    error::AppError,
    model::Viewer,
    ports::collections::{CollectionActor, CollectionPrincipal},
    security::audit::MutationAudit,
};

mod handlers;
mod memberships;
mod preferences;
mod projection;

use handlers::{create, list, remove, update};
use memberships::{add_memberships, remove_memberships};
use preferences::{get_preferences, set_preferences};
use projection::projection;

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/collections", get(list).post(create))
        .route(
            "/collections/{id}",
            get(collection_not_found).patch(update).delete(remove),
        )
        .route(
            "/collections/{id}/memberships",
            post(add_memberships).delete(remove_memberships),
        )
        .route(
            "/gallery/preferences",
            get(get_preferences).put(set_preferences),
        )
        .layer(DefaultBodyLimit::max(128 * 1024))
}

#[derive(Deserialize)]
struct OrgQuery {
    org: Option<String>,
    include: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    org: Option<String>,
    name: Option<String>,
    description: Option<String>,
    color: Option<String>,
    #[serde(rename = "coverArtifactId")]
    cover_artifact_id: Option<String>,
    #[serde(rename = "artifactIds")]
    artifact_ids: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MembershipBody {
    #[serde(rename = "artifactIds")]
    artifact_ids: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferenceBody {
    view: Option<String>,
    #[serde(rename = "previewSize")]
    preview_size: Option<String>,
    #[serde(rename = "artifactLayout")]
    artifact_layout: Option<String>,
    #[serde(rename = "collectionOrderByOrg")]
    collection_order_by_org: Option<std::collections::BTreeMap<String, Vec<String>>>,
    #[serde(rename = "collapsedCollectionIdsByOrg")]
    collapsed_collection_ids_by_org: Option<std::collections::BTreeMap<String, Vec<String>>>,
}
#[derive(Default)]
enum OptionalText {
    #[default]
    Missing,
    Null,
    Value(String),
}
fn deserialize_optional_text<'de, D>(deserializer: D) -> Result<OptionalText, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None => OptionalText::Null,
        Some(Value::String(value)) => OptionalText::Value(value),
        Some(Value::Null) => OptionalText::Null,
        Some(_) => {
            return Err(serde::de::Error::custom(
                "coverArtifactId must be an artifact ID or null",
            ));
        }
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchBody {
    org: Option<String>,
    name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    description: OptionalText,
    #[serde(default, deserialize_with = "deserialize_optional_text")]
    color: OptionalText,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_text",
        rename = "coverArtifactId"
    )]
    cover_artifact_id: OptionalText,
}

fn actor(viewer: &Viewer, org: &str) -> Result<CollectionActor, AppError> {
    let email = viewer
        .email
        .as_ref()
        .filter(|e| !e.0.is_empty())
        .ok_or_else(|| AppError::Unauthorized("Not signed in".into()))?;
    if !viewer.is_admin && viewer.org.as_ref().map(|o| o.0.as_str()) != Some(org) {
        return Err(AppError::ConcealedNotFound);
    }
    Ok(CollectionActor {
        email: email.0.clone(),
        org: org.to_owned(),
        is_admin: viewer.is_admin,
        principal: CollectionPrincipal::HumanEmail(email.0.clone()),
        publisher: None,
    })
}
async fn viewer(deps: &AppDeps, headers: &HeaderMap) -> Result<Viewer, AppError> {
    deps.viewer_identity.resolve(headers).await
}
fn mutation_audit(viewer: &Viewer) -> Result<MutationAudit, AppError> {
    MutationAudit::viewer(viewer)
}
fn validate_query(uri: &Uri, allowed: &[&str]) -> Result<(), AppError> {
    let mut seen = std::collections::HashSet::new();
    for (key, _) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        if !allowed.iter().any(|candidate| *candidate == key) || !seen.insert(key.to_string()) {
            return Err(AppError::Validation("invalid_query".into()));
        }
    }
    Ok(())
}

fn requested_org(viewer: &Viewer, requested: Option<&str>) -> Result<String, AppError> {
    if viewer.is_admin {
        let org = requested
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "all")
            .ok_or_else(|| AppError::Validation("org is required".into()))?;
        Ok(org.to_owned())
    } else {
        let org = viewer
            .org
            .as_ref()
            .map(|org| org.0.clone())
            .filter(|org| !org.is_empty())
            .ok_or_else(|| AppError::Unauthorized("Not signed in".into()))?;
        if requested.is_some_and(|value| !value.is_empty() && value.trim() != org) {
            return Err(AppError::ConcealedNotFound);
        }
        Ok(org)
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(
    input: Result<Json<Value>, JsonRejection>,
) -> Result<T, AppError> {
    let Json(mut value) = input.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge
        } else {
            AppError::Validation("invalid_body".into())
        }
    })?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| AppError::Validation("invalid_body".into()))?;
    if let Some(ids) = object.get("artifactIds")
        && (!ids.is_array()
            || ids
                .as_array()
                .is_some_and(|ids| ids.len() > 100 || ids.iter().any(|id| !id.is_string())))
    {
        return Err(AppError::Validation("invalid_ids".into()));
    }
    for field in ["name", "view", "previewSize", "artifactLayout"] {
        if object.get(field).is_some_and(|value| !value.is_string()) {
            return Err(AppError::Validation(
                if field == "name" {
                    "invalid_body"
                } else {
                    "invalid_preferences"
                }
                .into(),
            ));
        }
    }
    for field in ["coverArtifactId", "color", "org"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return Err(AppError::Validation(
                match field {
                    "coverArtifactId" => "invalid_cover",
                    "color" => "invalid_color",
                    _ => "invalid_query",
                }
                .into(),
            ));
        }
    }
    for field in ["collectionOrderByOrg", "collapsedCollectionIdsByOrg"] {
        if let Some(value) = object.get_mut(field) {
            if value.is_null() {
                *value = json!({});
            }
            let map = value
                .as_object()
                .ok_or_else(|| AppError::Validation("invalid_preferences".into()))?;
            if map.values().any(|ids| {
                !ids.is_array()
                    || ids
                        .as_array()
                        .is_some_and(|ids| ids.len() > 2000 || ids.iter().any(|id| !id.is_string()))
            }) {
                return Err(AppError::Validation("invalid_preferences".into()));
            }
        }
    }
    serde_json::from_value(value).map_err(|_| AppError::Validation("invalid_body".into()))
}

fn collection_error(error: AppError) -> Response {
    let status = error.http_status();
    let code = match &error {
        AppError::Unauthorized(_) => "unauthorized",
        AppError::Forbidden(_) => "forbidden",
        AppError::NotFound(_) | AppError::ConcealedNotFound => "not_found",
        AppError::Conflict(_) => "duplicate_name",
        AppError::PayloadTooLarge => "body_too_large",
        AppError::Validation(message) => {
            if message == "invalid_query" {
                "invalid_query"
            } else if message.contains("org is required") {
                "org_required"
            } else if message.contains("cover") {
                "invalid_cover"
            } else if message.contains("color") {
                "invalid_color"
            } else if message.contains("preferences") || message.contains("preference") {
                "invalid_preferences"
            } else if message == "collection_limit" {
                "collection_limit"
            } else if message.contains("member limit") {
                "membership_limit"
            } else if message == "invalid_ids" || message.contains("artifact id") {
                "invalid_ids"
            } else if message == "name is required" {
                "invalid_name"
            } else {
                "invalid_body"
            }
        }
        _ => "internal_error",
    };
    (
        status,
        Json(json!({"error":code,"message":error.to_string()})),
    )
        .into_response()
}

fn unique_ids(ids: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

async fn collection_not_found() -> Response {
    collection_error(AppError::ConcealedNotFound)
}
