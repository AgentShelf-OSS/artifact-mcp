use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::{
    AppDeps,
    error::AppError,
    ports::collections::{CollectionUpdate, NewCollection},
};

use super::{
    CreateBody, OptionalText, OrgQuery, PatchBody, actor, collection_error, mutation_audit,
    parse_body, projection, requested_org, unique_ids, validate_query, viewer,
};

pub(super) async fn list(
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
pub(super) async fn create(
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
    let s = deps.collections.as_ref();
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
            NewCollection {
                id,
                name: body.name.unwrap_or_default(),
                description: body.description.unwrap_or_default(),
                color: body.color.filter(|color| !color.is_empty()),
                cover: body.cover_artifact_id,
                artifact_ids: unique_ids(body.artifact_ids.unwrap_or_default()),
            },
            audit,
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
pub(super) async fn update(
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
    let s = deps.collections.as_ref();
    let audit = match mutation_audit(&v) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    if let Err(e) = s.update_audited(&a, id.clone(), update, audit).await {
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
pub(super) async fn remove(
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
    let s = deps.collections.as_ref();
    let audit = match mutation_audit(&v) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    match s.delete_audited(&a, id.clone(), audit).await {
        Ok(()) => Json(json!({"id":id,"deleted":true})).into_response(),
        Err(e) => collection_error(e),
    }
}
