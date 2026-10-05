//! Organization collection and gallery preference API.

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppDeps,
    error::AppError,
    model::{OrgId, Viewer},
    persistence::collections::{
        CollectionActor, CollectionStore, CollectionUpdate, MAX_COLLECTIONS_PER_ORG,
    },
    security::audit::MutationAudit,
};

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
    })
}
async fn viewer(deps: &AppDeps, headers: &HeaderMap) -> Result<Viewer, AppError> {
    deps.viewer_identity.resolve(headers).await
}
fn store(deps: &AppDeps) -> Result<CollectionStore, AppError> {
    let pool = deps
        .data
        .pool
        .clone()
        .ok_or_else(|| AppError::Unavailable("database unavailable".into()))?;
    Ok(match deps.audit_access.as_ref() {
        Some(audit) => CollectionStore::with_audit(pool, audit.mutation_key()),
        None => CollectionStore::new(pool),
    })
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

async fn projection(
    deps: &AppDeps,
    actor: &CollectionActor,
    orgs: &[String],
) -> Result<Value, AppError> {
    let registered = deps.admin.org_names().await?;
    if orgs
        .iter()
        .any(|org| !registered.iter().any(|entry| &entry.0 == org))
    {
        return Err(AppError::ConcealedNotFound);
    }
    let store = store(deps)?;
    let mut rows = Vec::new();
    for org in orgs {
        rows.extend(
            store
                .list_for_org(actor, org.clone(), MAX_COLLECTIONS_PER_ORG)
                .await?,
        );
    }
    let mut projected = Vec::new();
    let mut visible = std::collections::HashMap::new();
    for org in orgs {
        let items = deps
            .artifacts
            .list_org_artifacts(&OrgId(org.clone()), true)
            .await?;
        visible.extend(
            items
                .into_iter()
                .filter(|a| {
                    actor.is_admin
                        || !a.hidden
                        || a.owner_email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(&actor.email))
                })
                .map(|a| (a.id.0.clone(), a)),
        );
    }
    let mut collected = std::collections::HashSet::new();
    for row in rows {
        let mut row_actor = actor.clone();
        row_actor.org = row.org.clone();
        let ids = store
            .members(&row_actor, row.id.clone())
            .await?
            .into_iter()
            .filter(|id| visible.contains_key(id))
            .collect::<Vec<_>>();
        collected.extend(ids.iter().cloned());
        let cover = row.cover_artifact_id.filter(|id| visible.contains_key(id));
        let mut preview_ids = cover.iter().cloned().collect::<Vec<_>>();
        preview_ids.extend(
            ids.iter()
                .filter(|id| Some(*id) != cover.as_ref())
                .take(3usize.saturating_sub(preview_ids.len()))
                .cloned(),
        );
        let previews = preview_ids.iter().map(|id| json!({"id":id,"thumbnail":format!("/thumbnails/{}?v={}",id,visible.get(id).map(|a|a.body_sha256.clone()).unwrap_or_default())})).collect::<Vec<_>>();
        projected.push(json!({"id":row.id,"org":row.org,"name":row.name,"description":row.description,"color":row.color.filter(|color|!color.is_empty()),"createdBy":row.created_by,"editable":actor.is_admin || row.created_by.eq_ignore_ascii_case(&actor.email),"artifactCount":ids.len(),"artifactIds":ids,"coverArtifactId":cover,"previewArtifacts":previews}));
    }
    Ok(
        json!({"collections":projected,"uncollectedCount":visible.len().saturating_sub(collected.len()),"preferences":store.preferences(actor).await?}),
    )
}

async fn list(
    State(deps): State<AppDeps>,
    query: Result<Query<OrgQuery>, QueryRejection>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return collection_error(AppError::Validation("invalid_query".into())),
    };
    if let Err(e) = validate_query(&uri, &["org", "include"]) {
        return collection_error(e);
    }
    if query
        .include
        .as_deref()
        .is_some_and(|value| value != "projection")
    {
        return collection_error(AppError::Validation("invalid_query".into()));
    }
    let v = match viewer(&deps, &headers).await {
        Ok(v) => v,
        Err(e) => return collection_error(e),
    };
    let aggregate = v.is_admin
        && (query.org.as_deref().is_none()
            || query.org.as_deref().is_some_and(|org| org.trim() == "all"));
    let org = if aggregate {
        "all".to_owned()
    } else {
        match requested_org(&v, query.org.as_deref()) {
            Ok(x) => x,
            Err(e) => return collection_error(e),
        }
    };
    let a = match actor(&v, &org) {
        Ok(a) => a,
        Err(e) => return collection_error(e),
    };
    let orgs = if aggregate {
        deps.admin
            .org_names()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|x| x.0)
            .collect()
    } else {
        vec![org]
    };
    match projection(&deps, &a, &orgs).await {
        Ok(x) => Json(x).into_response(),
        Err(e) => collection_error(e),
    }
}
async fn create(
    State(deps): State<AppDeps>,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body: CreateBody = match parse_body(body) {
        Ok(body) => body,
        Err(error) => return collection_error(error),
    };
    if let Err(e) = validate_query(&uri, &[]) {
        return collection_error(e);
    }
    let v = match viewer(&deps, &headers).await {
        Ok(v) => v,
        Err(e) => return collection_error(e),
    };
    let org = match requested_org(&v, body.org.as_deref()) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    let a = match actor(&v, &org) {
        Ok(a) => a,
        Err(e) => return collection_error(e),
    };
    let s = match store(&deps) {
        Ok(s) => s,
        Err(e) => return collection_error(e),
    };
    let id = nanoid::nanoid!(
        12,
        &"0123456789abcdefghijkmnpqrstuvwxyz"
            .chars()
            .collect::<Vec<_>>()
    );
    let audit = match mutation_audit(&v) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    let row = match s
        .create_atomic(
            &a,
            id,
            body.name.unwrap_or_default(),
            body.description.unwrap_or_default(),
            body.color.filter(|color| !color.is_empty()),
            body.cover_artifact_id,
            unique_ids(body.artifact_ids.unwrap_or_default()),
            Some(audit),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return collection_error(e),
    };
    match projection(&deps, &a, std::slice::from_ref(&row.org)).await {
        Ok(x) => {
            let item=x.get("collections").and_then(Value::as_array).and_then(|rows|rows.iter().find(|item|item.get("id").and_then(Value::as_str)==Some(row.id.as_str()))).cloned().unwrap_or_else(||json!({"id":row.id,"org":row.org,"name":row.name,"description":row.description,"color":row.color.filter(|color|!color.is_empty()),"createdBy":row.created_by,"artifactCount":row.artifact_count,"artifactIds":[]}));
            (StatusCode::CREATED, Json(item)).into_response()
        }
        Err(e) => collection_error(e),
    }
}
async fn update(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    query: Result<Query<OrgQuery>, QueryRejection>,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return collection_error(AppError::Validation("invalid_query".into())),
    };
    let body: PatchBody = match parse_body(body) {
        Ok(body) => body,
        Err(error) => return collection_error(error),
    };
    if let Err(error) = validate_query(&uri, &[]) {
        return collection_error(error);
    }
    let v = match viewer(&deps, &headers).await {
        Ok(v) => v,
        Err(e) => return collection_error(e),
    };
    let requested = body.org.as_deref().or(query.org.as_deref());
    let org = match requested_org(&v, requested) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    let a = match actor(&v, &org) {
        Ok(a) => a,
        Err(e) => return collection_error(e),
    };
    let cover = match body.cover_artifact_id {
        OptionalText::Missing => (None, false),
        OptionalText::Null => (None, true),
        OptionalText::Value(value) => (Some(value), false),
    };
    let update = CollectionUpdate {
        org: body.org,
        name: body.name,
        description: match body.description {
            OptionalText::Missing => None,
            OptionalText::Null => Some(String::new()),
            OptionalText::Value(value) => Some(value),
        },
        color: match body.color {
            OptionalText::Missing => None,
            OptionalText::Null => Some(String::new()),
            OptionalText::Value(value) => Some(value),
        },
        cover_artifact_id: cover.0,
        clear_cover: cover.1,
    };
    let s = match store(&deps) {
        Ok(s) => s,
        Err(e) => return collection_error(e),
    };
    let audit = match mutation_audit(&v) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    if let Err(e) = s.update_audited(&a, id.clone(), update, Some(audit)).await {
        return collection_error(e);
    };
    match projection(&deps, &a, &[org]).await {
        Ok(x) => {
            let item = x
                .get("collections")
                .and_then(Value::as_array)
                .and_then(|rows| {
                    rows.iter()
                        .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
                })
                .cloned()
                .unwrap_or(Value::Null);
            Json(item).into_response()
        }
        Err(e) => collection_error(e),
    }
}
async fn remove(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    query: Result<Query<OrgQuery>, QueryRejection>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return collection_error(AppError::Validation("invalid_query".into())),
    };
    if let Err(error) = validate_query(&uri, &["org"]) {
        return collection_error(error);
    }
    let v = match viewer(&deps, &headers).await {
        Ok(v) => v,
        Err(e) => return collection_error(e),
    };
    let org = match requested_org(&v, query.org.as_deref()) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    let a = match actor(&v, &org) {
        Ok(a) => a,
        Err(e) => return collection_error(e),
    };
    let s = match store(&deps) {
        Ok(s) => s,
        Err(e) => return collection_error(e),
    };
    let audit = match mutation_audit(&v) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    match s.delete_audited(&a, id.clone(), Some(audit)).await {
        Ok(()) => Json(json!({"id":id,"deleted":true})).into_response(),
        Err(e) => collection_error(e),
    }
}
async fn add_memberships(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    query: Result<Query<OrgQuery>, QueryRejection>,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return collection_error(AppError::Validation("invalid_query".into())),
    };
    let body: MembershipBody = match parse_body(body) {
        Ok(body) => body,
        Err(error) => return collection_error(error),
    };
    membership(deps, id, query, uri, headers, body, true).await
}
async fn remove_memberships(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    query: Result<Query<OrgQuery>, QueryRejection>,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return collection_error(AppError::Validation("invalid_query".into())),
    };
    let body: MembershipBody = match parse_body(body) {
        Ok(body) => body,
        Err(error) => return collection_error(error),
    };
    membership(deps, id, query, uri, headers, body, false).await
}
async fn membership(
    deps: AppDeps,
    id: String,
    query: OrgQuery,
    uri: Uri,
    headers: HeaderMap,
    body: MembershipBody,
    adding: bool,
) -> Response {
    if let Err(error) = validate_query(&uri, &["org"]) {
        return collection_error(error);
    }
    let v = match viewer(&deps, &headers).await {
        Ok(v) => v,
        Err(e) => return collection_error(e),
    };
    let org = match requested_org(&v, query.org.as_deref()) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    let a = match actor(&v, &org) {
        Ok(a) => a,
        Err(e) => return collection_error(e),
    };
    let ids = unique_ids(body.artifact_ids.unwrap_or_default());
    let s = match store(&deps) {
        Ok(s) => s,
        Err(e) => return collection_error(e),
    };
    let before = match s.members(&a, id.clone()).await {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    let audit = match mutation_audit(&v) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    if adding {
        match s
            .add_memberships_audited(&a, id.clone(), ids.clone(), Some(audit))
            .await
        {
            Ok(_) => {
                let prior = before.iter().collect::<std::collections::HashSet<_>>();
                Json(json!({"collectionId":id,"added":ids.iter().filter(|x|!prior.contains(x)).collect::<Vec<_>>(),"alreadyPresent":ids.iter().filter(|x|prior.contains(x)).collect::<Vec<_>>()})).into_response()
            }
            Err(e) => collection_error(e),
        }
    } else {
        match s
            .remove_memberships_audited(&a, id.clone(), ids.clone(), Some(audit))
            .await
        {
            Ok(_) => {
                let prior = before.iter().collect::<std::collections::HashSet<_>>();
                Json(json!({"collectionId":id,"removed":ids.iter().filter(|x|prior.contains(x)).collect::<Vec<_>>()})).into_response()
            }
            Err(e) => collection_error(e),
        }
    }
}
async fn get_preferences(
    State(deps): State<AppDeps>,
    query: Result<Query<OrgQuery>, QueryRejection>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return collection_error(AppError::Validation("invalid_query".into())),
    };
    if let Err(error) = validate_query(&uri, &["org"]) {
        return collection_error(error);
    }
    let v = match viewer(&deps, &headers).await {
        Ok(v) => v,
        Err(e) => return collection_error(e),
    };
    let org = if v.is_admin {
        query.org.unwrap_or_else(|| "all".into())
    } else {
        match requested_org(&v, query.org.as_deref()) {
            Ok(x) => x,
            Err(e) => return collection_error(e),
        }
    };
    let a = match actor(&v, &org) {
        Ok(a) => a,
        Err(e) => return collection_error(e),
    };
    if a.org != "all"
        && !deps
            .admin
            .org_names()
            .await
            .unwrap_or_default()
            .iter()
            .any(|org| org.0 == a.org)
    {
        return collection_error(AppError::ConcealedNotFound);
    }
    let s = match store(&deps) {
        Ok(store) => store,
        Err(error) => return collection_error(error),
    };
    match s.preferences(&a).await {
        Ok(x) => Json(x).into_response(),
        Err(e) => collection_error(e),
    }
}
async fn set_preferences(
    State(deps): State<AppDeps>,
    query: Result<Query<OrgQuery>, QueryRejection>,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return collection_error(AppError::Validation("invalid_query".into())),
    };
    let body: PreferenceBody = match parse_body(body) {
        Ok(body) => body,
        Err(error) => return collection_error(error),
    };
    if let Err(error) = validate_query(&uri, &["org"]) {
        return collection_error(error);
    }
    let v = match viewer(&deps, &headers).await {
        Ok(v) => v,
        Err(e) => return collection_error(e),
    };
    let org = if v.is_admin {
        query.org.unwrap_or_else(|| "all".into())
    } else {
        match requested_org(&v, query.org.as_deref()) {
            Ok(x) => x,
            Err(e) => return collection_error(e),
        }
    };
    let a = match actor(&v, &org) {
        Ok(a) => a,
        Err(e) => return collection_error(e),
    };
    if a.org != "all"
        && !deps
            .admin
            .org_names()
            .await
            .unwrap_or_default()
            .iter()
            .any(|org| org.0 == a.org)
    {
        return collection_error(AppError::ConcealedNotFound);
    }
    let s = match store(&deps) {
        Ok(s) => s,
        Err(e) => return collection_error(e),
    };
    let mut write_actor = a.clone();
    if write_actor.is_admin {
        write_actor.org = "all".into();
    }
    let mut p = match s.preferences(&write_actor).await {
        Ok(p) => p,
        Err(e) => return collection_error(e),
    };
    if let Some(x) = body.view {
        p.view = x
    };
    if let Some(x) = body.preview_size {
        p.preview_size = x
    };
    if let Some(x) = body.artifact_layout {
        p.artifact_layout = x
    };
    if let Some(x) = body.collection_order_by_org {
        p.collection_order_by_org = x
    };
    if let Some(x) = body.collapsed_collection_ids_by_org {
        p.collapsed_collection_ids_by_org = x
    };
    match s.save_preferences(&write_actor, p).await {
        Ok(x) => Json(x).into_response(),
        Err(e) => collection_error(e),
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
