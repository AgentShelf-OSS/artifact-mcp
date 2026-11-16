use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::{AppDeps, error::AppError};

use super::{
    MembershipBody, OrgQuery, actor, collection_error, mutation_audit, parse_body, requested_org,
    unique_ids, validate_query, viewer,
};

pub(super) async fn add_memberships(
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
pub(super) async fn remove_memberships(
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
    let s = deps.collections.as_ref();
    let before = match s.members(&a, id.clone()).await {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    let audit = match mutation_audit(&v) {
        Ok(x) => x,
        Err(e) => return collection_error(e),
    };
    // HTTP reports the preflight view for compatibility, discarding the mutation diff.
    if adding {
        match s
            .add_memberships_audited(&a, id.clone(), ids.clone(), audit)
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
            .remove_memberships_audited(&a, id.clone(), ids.clone(), audit)
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
