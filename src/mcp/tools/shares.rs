//! Public share tools.

use serde_json::Value;

use crate::{
    AppDeps,
    error::{AppError, McpError},
    mcp::{
        args::{publisher_delete, publisher_read, required_string},
        protocol::OrderedJson,
        shape::{object, optional_timestamp, share_json, tool_result},
    },
    model::{CreateShare, PublisherIdentity, ShareToken},
    security::{access::AccessPolicy, audit::MutationAudit},
};

pub(crate) async fn create_share(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_delete(id, auth, deps).await?;
    let share = deps
        .shares
        .create(
            owned.into_authorized(),
            CreateShare {
                created_by: format!("agent:{}", auth.client_id),
                expires: required_string(arguments, "expires")?.to_owned(),
            },
            MutationAudit::publisher(auth)?,
        )
        .await?;
    let url = format!("{}/s/{}", deps.config.public_base_url, share.token);
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "token" => OrderedJson::string(share.token.0),
        "expires_at" => optional_timestamp(share.expires_at),
        "url" => OrderedJson::string(url),
    })
}

pub(crate) async fn list_shares(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_read(id, auth, deps).await?;
    let shares = deps.shares.list(&owned.into_authorized()).await?;
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "shares" => OrderedJson::Array(shares.into_iter().map(share_json).collect()),
    })
}

pub(crate) async fn revoke_share(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let token = ShareToken::from(required_string(arguments, "token")?);
    let Some(grant) = deps.shares.resolve(&token).await? else {
        return Err(AppError::NotFound("Unknown share".to_owned()).into());
    };
    let Some(meta) = deps.artifacts.find_meta(&grant.artifact_id).await? else {
        return Err(AppError::NotFound("Unknown share".to_owned()).into());
    };
    if !AccessPolicy::share_matches(&grant, &meta) {
        return Err(AppError::NotFound("Unknown share".to_owned()).into());
    }
    let owned = AccessPolicy::authorize_publisher_write(auth, Some(meta), &grant.artifact_id.0, "")
        .map_err(|denied| match denied {
            AppError::NotFound(_) => AppError::NotFound("Unknown share".to_owned()),
            other => other,
        })?;
    let revoked = deps
        .shares
        .revoke(
            owned.into_authorized(),
            token.clone(),
            MutationAudit::publisher(auth)?,
        )
        .await?;
    tool_result(object! {
        "token" => OrderedJson::string(token.0),
        "revoked" => OrderedJson::Bool(revoked),
    })
}
