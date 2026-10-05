// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! MCP adapters for organization collections (folders in the library UI).

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use crate::{
    AppDeps,
    error::{AppError, McpError},
    model::{ArtifactId, PublisherIdentity},
    persistence::collections::{
        CollectionActor, CollectionPrincipal, CollectionStore, CollectionUpdate,
        MAX_COLLECTIONS_PER_ORG,
    },
    security::audit::MutationAudit,
};

use super::protocol::OrderedJson;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;

pub async fn call(
    name: &str,
    args: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    if let Some(scope) = crate::security::oauth::required_scope("tools/call", Some(name))
        && !auth.has_scope(scope)
    {
        return Err(AppError::Forbidden(format!("Missing required scope: {scope}")).into());
    }
    if auth.is_oauth() && deps.config.oauth.issuer.is_empty() {
        return Err(AppError::Validation("OAuth issuer unavailable".into()).into());
    }
    let actor = actor(auth, deps);
    let org = target_org(args, auth, deps).await?;
    let mut actor = actor;
    actor.org = org.clone();
    if auth.role == "reader"
        && matches!(
            name,
            "create_collection"
                | "update_collection"
                | "delete_collection"
                | "add_artifacts_to_collection"
                | "remove_artifacts_from_collection"
        )
    {
        return Err(AppError::Forbidden(
            "Permission denied: reader keys cannot manage collections".into(),
        )
        .into());
    }
    let store = store(deps)?;
    let result = match name {
        "list_collections" => list(args, auth, &store, actor, org).await,
        "get_collection" => get(args, auth, &store, actor, org, deps).await,
        "create_collection" => create(args, auth, &store, actor, org).await,
        "update_collection" => update(args, auth, &store, actor, org).await,
        "delete_collection" => delete(args, auth, &store, actor, org).await,
        "add_artifacts_to_collection" => membership(args, auth, &store, actor, org, true).await,
        "remove_artifacts_from_collection" => {
            membership(args, auth, &store, actor, org, false).await
        }
        _ => Err(AppError::Internal.into()),
    };
    result.map_err(collection_error)
}

fn store(deps: &AppDeps) -> Result<CollectionStore, McpError> {
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

fn actor(auth: &PublisherIdentity, deps: &AppDeps) -> CollectionActor {
    let principal = if auth.is_oauth() {
        CollectionPrincipal::OAuthClient {
            issuer: deps.config.oauth.issuer.clone(),
            client_id: auth.client_id.0.clone(),
        }
    } else {
        CollectionPrincipal::ApiKeyClient(auth.client_id.0.clone())
    };
    CollectionActor {
        email: String::new(),
        org: auth.org.0.clone(),
        is_admin: auth.is_admin(),
        principal,
        publisher: Some(auth.clone()),
    }
}

async fn target_org(
    args: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<String, McpError> {
    let requested = args
        .get("org")
        .and_then(OrderedJson::as_str)
        .map(|value| value.trim().to_lowercase());
    if auth.is_admin() {
        let org = requested.ok_or_else(|| AppError::Validation("org is required".into()))?;
        if org.is_empty() || org == "all" {
            return Err(AppError::Validation("org is required".into()).into());
        }
        if !deps
            .admin
            .org_exists(&crate::model::OrgId::from(org.clone()))
            .await?
        {
            return Err(AppError::ConcealedNotFound.into());
        }
        Ok(org)
    } else if requested.is_some_and(|value| value != auth.org.0) {
        Err(AppError::ConcealedNotFound.into())
    } else {
        Ok(auth.org.0.clone())
    }
}

fn limit(args: &OrderedJson) -> Result<usize, McpError> {
    let value = args
        .get("limit")
        .and_then(OrderedJson::as_number)
        .and_then(|n| n.as_u64())
        .unwrap_or(DEFAULT_LIMIT as u64);
    if !(1..=MAX_LIMIT as u64).contains(&value) {
        return Err(AppError::Validation("limit must be between 1 and 100".into()).into());
    }
    Ok(value as usize)
}

fn id(args: &OrderedJson, key: &str) -> Result<String, McpError> {
    let value = args
        .get(key)
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| AppError::Validation(format!("{key} is required")))?;
    if value.is_empty() || value.chars().count() > 128 {
        return Err(AppError::Validation(format!("{key} is invalid")).into());
    }
    Ok(value.to_owned())
}

fn cursor(
    args: &OrderedJson,
    name: &str,
    auth: &PublisherIdentity,
    org: &str,
    id: &str,
    limit: usize,
    principal: &CollectionPrincipal,
) -> Result<usize, McpError> {
    let Some(raw) = args
        .get("cursor")
        .and_then(OrderedJson::as_str)
        .filter(|v| !v.is_empty())
    else {
        return Ok(0);
    };
    if raw.len() > 1024 {
        return Err(AppError::Validation("Invalid collection cursor".into()).into());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| AppError::Validation("Invalid collection cursor".into()))?;
    let row: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::Validation("Invalid collection cursor".into()))?;
    let expected_prefix = json!([
        1,
        name,
        auth.org.0,
        org,
        if auth.is_oauth() { "oauth" } else { "api_key" },
        principal.kind_id().1,
        auth.role,
        id,
        limit
    ]);
    let Some(array) = row.as_array() else {
        return Err(AppError::Validation("Invalid collection cursor".into()).into());
    };
    if array.len() != 10 || array[..9] != expected_prefix.as_array().unwrap()[..] {
        return Err(AppError::Validation("Invalid collection cursor".into()).into());
    }
    let offset = array[9]
        .as_u64()
        .ok_or_else(|| AppError::Validation("Invalid collection cursor".into()))?;
    if offset
        > if name == "list_collections" {
            200
        } else {
            1000
        }
    {
        return Err(AppError::Validation("Invalid collection cursor".into()).into());
    }
    Ok(offset as usize)
}

fn next_cursor(
    name: &str,
    auth: &PublisherIdentity,
    actor: &CollectionActor,
    id: &str,
    limit: usize,
    offset: usize,
    more: bool,
) -> String {
    if !more {
        return String::new();
    }
    URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!([
            1,
            name,
            auth.org.0,
            actor.org,
            if auth.is_oauth() { "oauth" } else { "api_key" },
            actor.principal.kind_id().1,
            auth.role,
            id,
            limit,
            offset + limit
        ]))
        .unwrap_or_default(),
    )
}

fn summary(row: &crate::persistence::collections::Collection, actor: &CollectionActor) -> Value {
    json!({"id":row.id,"org":row.org,"name":row.name,"description":row.description,"color":row.color.clone().unwrap_or_default(),"cover_artifact_id":row.cover_artifact_id.clone().unwrap_or_default(),"artifact_count":row.artifact_count,"editable":actor.publisher.as_ref().is_none_or(|p| p.role != "reader") && (actor.is_admin || (row.created_by_kind == actor.principal.kind_id().0 && row.created_by == actor.principal.kind_id().1))})
}

async fn list(
    args: &OrderedJson,
    auth: &PublisherIdentity,
    store: &CollectionStore,
    actor: CollectionActor,
    org: String,
) -> Result<Value, McpError> {
    let page = limit(args)?;
    let offset = cursor(
        args,
        "list_collections",
        auth,
        &org,
        "",
        page,
        &actor.principal,
    )?;
    let rows = store
        .list_for_org(&actor, org.clone(), MAX_COLLECTIONS_PER_ORG)
        .await?;
    let more = rows.len() > offset + page;
    let mut rows = rows;
    rows.sort_by(|a, b| {
        a.name
            .as_bytes()
            .cmp(b.name.as_bytes())
            .then_with(|| a.id.as_bytes().cmp(b.id.as_bytes()))
    });
    let mut collections = Vec::new();
    for row in rows.into_iter().skip(offset).take(page) {
        let members = store
            .readable_members(&actor, row.id.clone(), 0, 1000)
            .await?;
        let count = members.len();
        let mut value = summary(&row, &actor);
        value["artifact_count"] = json!(count);
        if row
            .cover_artifact_id
            .as_ref()
            .is_some_and(|id| !members.iter().any(|member| member == id))
        {
            value["cover_artifact_id"] = json!("");
        }
        collections.push(value);
    }
    Ok(
        json!({"collections":collections,"next_cursor":next_cursor("list_collections",auth,&actor,"",page,offset,more)}),
    )
}

async fn get(
    args: &OrderedJson,
    auth: &PublisherIdentity,
    store: &CollectionStore,
    actor: CollectionActor,
    org: String,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let collection_id = id(args, "id")?;
    let page = limit(args)?;
    let offset = cursor(
        args,
        "get_collection",
        auth,
        &org,
        &collection_id,
        page,
        &actor.principal,
    )?;
    let row = store
        .list_for_org(&actor, org.clone(), MAX_COLLECTIONS_PER_ORG)
        .await?
        .into_iter()
        .find(|r| r.id == collection_id)
        .ok_or(AppError::ConcealedNotFound)?;
    let ids = store
        .readable_members(&actor, collection_id.clone(), 0, 1000)
        .await?;
    let mut artifacts = Vec::new();
    for artifact_id in ids {
        if let Some(meta) = deps
            .artifacts
            .find_meta(&ArtifactId::from(artifact_id.clone()))
            .await?
        {
            artifacts.push(json!({"id":artifact_id,"title":meta.title,"url":deps.config.artifact_url(&meta.id),"category":meta.category}));
        }
    }
    artifacts.sort_by(|a, b| {
        a["title"]
            .as_str()
            .unwrap_or_default()
            .as_bytes()
            .cmp(b["title"].as_str().unwrap_or_default().as_bytes())
            .then_with(|| {
                a["id"]
                    .as_str()
                    .unwrap_or_default()
                    .as_bytes()
                    .cmp(b["id"].as_str().unwrap_or_default().as_bytes())
            })
    });
    let cover_readable = row
        .cover_artifact_id
        .as_ref()
        .is_none_or(|id| artifacts.iter().any(|item| item["id"].as_str() == Some(id)));
    let readable_count = artifacts.len();
    let more = readable_count > offset + page;
    let artifacts = artifacts
        .into_iter()
        .skip(offset)
        .take(page)
        .collect::<Vec<_>>();
    let mut collection = summary(&row, &actor);
    collection["artifact_count"] = json!(readable_count);
    if !cover_readable {
        collection["cover_artifact_id"] = json!("");
    }
    Ok(
        json!({"collection":collection,"artifacts":artifacts,"next_cursor":next_cursor("get_collection",auth,&actor,&collection_id,page,offset,more)}),
    )
}

async fn create(
    args: &OrderedJson,
    auth: &PublisherIdentity,
    store: &CollectionStore,
    mut actor: CollectionActor,
    org: String,
) -> Result<Value, McpError> {
    actor.org = org.clone();
    let name = id(args, "name")?;
    let ids = array_ids(args.get("artifact_ids"))?;
    let cover = empty_clear(args, "cover_artifact_id");
    let row = store
        .create_atomic(
            &actor,
            collection_id()?,
            name,
            args.get("description")
                .and_then(OrderedJson::as_str)
                .unwrap_or_default()
                .to_owned(),
            empty_clear(args, "color"),
            cover,
            ids,
            Some(MutationAudit::publisher(auth)?),
        )
        .await?;
    Ok(json!({"collection":project_summary(&row,&actor,store).await?}))
}
async fn update(
    args: &OrderedJson,
    auth: &PublisherIdentity,
    store: &CollectionStore,
    mut actor: CollectionActor,
    org: String,
) -> Result<Value, McpError> {
    actor.org = org;
    let collection_id = id(args, "id")?;
    let u = CollectionUpdate {
        name: args
            .get("name")
            .and_then(OrderedJson::as_str)
            .map(ToOwned::to_owned),
        description: args
            .get("description")
            .and_then(OrderedJson::as_str)
            .map(ToOwned::to_owned),
        color: args
            .get("color")
            .and_then(OrderedJson::as_str)
            .map(ToOwned::to_owned),
        cover_artifact_id: args
            .get("cover_artifact_id")
            .and_then(OrderedJson::as_str)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned),
        clear_cover: args.get("cover_artifact_id").and_then(OrderedJson::as_str) == Some(""),
        ..CollectionUpdate::default()
    };
    store
        .update_audited(
            &actor,
            collection_id.clone(),
            u,
            Some(MutationAudit::publisher(auth)?),
        )
        .await?;
    let row = store
        .list(&actor, 200)
        .await?
        .into_iter()
        .find(|r| r.id == collection_id)
        .ok_or(AppError::ConcealedNotFound)?;
    Ok(json!({"collection":project_summary(&row,&actor,store).await?}))
}
async fn delete(
    args: &OrderedJson,
    auth: &PublisherIdentity,
    store: &CollectionStore,
    actor: CollectionActor,
    org: String,
) -> Result<Value, McpError> {
    let collection_id = id(args, "id")?;
    let mut actor = actor;
    actor.org = org;
    store
        .delete_audited(
            &actor,
            collection_id.clone(),
            Some(MutationAudit::publisher(auth)?),
        )
        .await?;
    Ok(json!({"id":collection_id,"deleted":true}))
}
async fn membership(
    args: &OrderedJson,
    auth: &PublisherIdentity,
    store: &CollectionStore,
    actor: CollectionActor,
    org: String,
    add: bool,
) -> Result<Value, McpError> {
    let collection_id = id(args, "id")?;
    let ids = array_ids(args.get("artifact_ids"))?;
    let mut actor = actor;
    actor.org = org;
    if add {
        let (added, already_present) = store
            .add_memberships_diff_audited(
                &actor,
                collection_id.clone(),
                ids.clone(),
                Some(MutationAudit::publisher(auth)?),
            )
            .await?;
        Ok(json!({"collection_id":collection_id,"added":added,"already_present":already_present}))
    } else {
        let (removed, already_absent) = store
            .remove_memberships_diff_audited(
                &actor,
                collection_id.clone(),
                ids.clone(),
                Some(MutationAudit::publisher(auth)?),
            )
            .await?;
        Ok(json!({"collection_id":collection_id,"removed":removed,"already_absent":already_absent}))
    }
}
fn array_ids(value: Option<&OrderedJson>) -> Result<Vec<String>, McpError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(AppError::Validation("artifact_ids must be an array".into()).into());
    };
    if values.len() > 100 {
        return Err(AppError::Validation("too many artifact_ids".into()).into());
    };
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for value in values {
        let id = value
            .as_str()
            .ok_or_else(|| AppError::Validation("artifact_ids must contain strings".into()))?;
        if seen.insert(id) {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}
fn empty_clear(args: &OrderedJson, key: &str) -> Option<String> {
    args.get(key)
        .and_then(OrderedJson::as_str)
        .map(ToOwned::to_owned)
        .filter(|v| !v.is_empty())
}

fn collection_id() -> Result<String, AppError> {
    let mut bytes = [0_u8; 6];
    getrandom::fill(&mut bytes).map_err(|_| AppError::Internal)?;
    Ok(hex::encode(bytes))
}

async fn project_summary(
    row: &crate::persistence::collections::Collection,
    actor: &CollectionActor,
    store: &CollectionStore,
) -> Result<Value, McpError> {
    let ids = store
        .readable_members(actor, row.id.clone(), 0, 1000)
        .await?;
    let mut value = summary(row, actor);
    value["artifact_count"] = json!(ids.len());
    if row
        .cover_artifact_id
        .as_ref()
        .is_some_and(|id| !ids.contains(id))
    {
        value["cover_artifact_id"] = json!("");
    }
    Ok(value)
}

fn collection_error(error: McpError) -> McpError {
    match error {
        McpError::Tool(AppError::NotFound(_)) => AppError::ConcealedNotFound.into(),
        McpError::Tool(AppError::Validation(message)) if message == "invalid collection id" => {
            AppError::ConcealedNotFound.into()
        }
        McpError::Tool(AppError::Validation(message)) if message == "collection_limit" => {
            AppError::Validation("organization collection limit reached".into()).into()
        }
        McpError::Tool(AppError::Conflict(message))
            if message == "a collection with that name already exists" =>
        {
            AppError::Conflict("A collection with that name already exists".into()).into()
        }
        other => other,
    }
}

// Legacy MCP text content preserves the Node field order as well as the JSON value.
pub fn ordered_response(value: Value) -> OrderedJson {
    match value {
        Value::Object(mut fields) => {
            let order: &[&str] = if fields.contains_key("collections") {
                &["collections", "next_cursor"]
            } else if fields.contains_key("collection") {
                &["collection", "artifacts", "next_cursor"]
            } else if fields.contains_key("collection_id") {
                &[
                    "collection_id",
                    "added",
                    "already_present",
                    "removed",
                    "already_absent",
                ]
            } else if fields.contains_key("name") {
                &[
                    "id",
                    "org",
                    "name",
                    "description",
                    "color",
                    "cover_artifact_id",
                    "artifact_count",
                    "editable",
                ]
            } else if fields.contains_key("title") {
                &["id", "title", "url", "category"]
            } else {
                &["id", "deleted"]
            };
            let mut entries = Vec::new();
            for key in order {
                if let Some(value) = fields.remove(*key) {
                    entries.push(((*key).to_owned(), ordered_response(value)));
                }
            }
            entries.extend(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, ordered_response(value))),
            );
            OrderedJson::Object(entries)
        }
        Value::Array(values) => {
            OrderedJson::Array(values.into_iter().map(ordered_response).collect())
        }
        other => serde_json::from_value(other).expect("JSON primitive"),
    }
}
