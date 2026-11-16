use axum::{
    Json,
    extract::{
        Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::{AppDeps, error::AppError};

use super::{
    OrgQuery, PreferenceBody, actor, collection_error, parse_body, requested_org, validate_query,
    viewer,
};

pub(super) async fn get_preferences(
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
    let s = deps.collections.as_ref();
    match s.preferences(&a).await {
        Ok(x) => Json(x).into_response(),
        Err(e) => collection_error(e),
    }
}
pub(super) async fn set_preferences(
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
    let s = deps.collections.as_ref();
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
