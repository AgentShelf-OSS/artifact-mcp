//! Artifact read, list, delete, visibility, revision, stats, and restore tools.

use serde_json::Value;

use crate::{
    AppDeps,
    error::{AppError, JsonRpcError, McpError},
    mcp::{
        args::{
            non_negative_integer, nonempty_protocol_string, positive_integer, publisher_delete,
            publisher_read, publisher_write, required_string, safe_positive_integer,
        },
        protocol::OrderedJson,
        shape::{
            artifact_url, object, optional_timestamp, revision_json, tool_result, viewer_json,
            with_artifact_key,
        },
    },
    model::PublisherIdentity,
    security::audit::MutationAudit,
};

pub(crate) async fn list_artifacts(
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let rows = if !auth.is_admin() && matches!(auth.role.as_str(), "reader" | "collaborator") {
        deps.artifacts.list_org_artifacts(&auth.org, true).await?
    } else {
        deps.artifacts.list_for_publisher(auth).await?
    };
    let count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let artifacts = rows
        .into_iter()
        .map(|row| {
            let artifact_key = row.artifact_key.clone();
            let entry = object! {
                "id" => OrderedJson::string(row.id.0.clone()),
                "url" => OrderedJson::string(artifact_url(deps, &row.id)),
                "title" => OrderedJson::string(row.title),
                "description" => OrderedJson::string(row.description),
                "created_at" => OrderedJson::string(row.created_at.0),
                "org" => OrderedJson::string(row.org.0),
                "category" => OrderedJson::string(row.category),
                "revision" => OrderedJson::number_u64(row.revision),
                "updated_at" => OrderedJson::string(row.updated_at.0),
                "bytes" => OrderedJson::number_u64(row.bytes),
                "is_bundle" => OrderedJson::number_i64(i64::from(row.is_bundle)),
                "entry" => OrderedJson::string(row.entry),
                "hidden" => OrderedJson::number_i64(i64::from(row.hidden)),
                "uploader_label" => OrderedJson::string(row.uploader_label),
            };
            with_artifact_key(entry, artifact_key.as_deref())
        })
        .collect();
    tool_result(object! {
        "count" => OrderedJson::number_u64(count),
        "artifacts" => OrderedJson::Array(artifacts),
    })
}

pub(crate) async fn read_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let requested_revision = arguments
        .get("revision")
        .map(|value| safe_positive_integer(value, "revision"))
        .transpose()?;
    let requested_offset = arguments
        .get("offset")
        .map(|value| non_negative_integer(value, "offset"))
        .transpose()?
        .unwrap_or(0);
    let limit = arguments
        .get("limit")
        .map(|value| safe_positive_integer(value, "limit"))
        .transpose()?
        .unwrap_or(65_536);

    // Invariant 3: authorize the metadata before any body, history, or bundle-directory read.
    let owned = publisher_read(id, auth, deps).await?;
    let current = owned.meta().clone();
    let artifact_key = current.artifact_key.clone();
    let artifact = owned.into_authorized();

    let historical = requested_revision.filter(|revision| *revision != current.revision);
    let (revision, is_bundle, entry, aggregate_bytes) = if let Some(wanted) = historical {
        let history = deps.artifacts.list_revisions(&artifact).await?;
        let selected = history
            .revisions
            .into_iter()
            .find(|row| row.revision == wanted)
            .ok_or_else(|| AppError::NotFound(format!("No such revision: {wanted}")))?;
        (
            selected.revision,
            selected.is_bundle,
            selected.entry,
            selected.bytes,
        )
    } else {
        (
            current.revision,
            current.is_bundle,
            current.entry.clone(),
            current.bytes,
        )
    };

    if is_bundle {
        if arguments.get("path").is_none() {
            let files = deps
                .artifacts
                .list_bundle_files(&artifact, historical)
                .await?
                .ok_or_else(|| {
                    if historical.is_some() {
                        AppError::Gone(format!("Revision {revision} is no longer retained"))
                    } else {
                        AppError::Gone(format!("Artifact body is unavailable: {id}"))
                    }
                })?;
            let files = files
                .into_iter()
                .map(|(path, bytes)| {
                    let is_entry = path == entry;
                    object! {
                        "path" => OrderedJson::string(path),
                        "bytes" => OrderedJson::number_u64(bytes),
                        "entry" => OrderedJson::Bool(is_entry),
                    }
                })
                .collect();
            return tool_result(with_artifact_key(
                object! {
                    "id" => OrderedJson::string(id),
                    "org" => OrderedJson::string(current.org.0),
                    "is_bundle" => OrderedJson::Bool(true),
                    "entry" => OrderedJson::string(entry),
                    "revision" => OrderedJson::number_u64(revision),
                    "content_type" => OrderedJson::string("application/json"),
                    "bytes_total" => OrderedJson::number_u64(aggregate_bytes),
                    "offset" => OrderedJson::number_u64(0),
                    "bytes_returned" => OrderedJson::number_u64(0),
                    "truncated" => OrderedJson::Bool(false),
                    "files" => OrderedJson::Array(files),
                },
                artifact_key.as_deref(),
            ));
        }

        let path = required_string(arguments, "path")?;
        let file = if let Some(historical_revision) = historical {
            deps.artifacts
                .read_revision_body(&artifact, historical_revision, Some(path))
                .await?
        } else {
            deps.artifacts.read_bundle_file(&artifact, path).await?
        }
        .ok_or_else(|| AppError::NotFound(format!("Unknown bundle file: {path}")))?;
        let page = page_utf8(&file.content, requested_offset, limit);
        return tool_result(with_artifact_key(
            object! {
                "id" => OrderedJson::string(id),
                "org" => OrderedJson::string(current.org.0),
                "is_bundle" => OrderedJson::Bool(true),
                "entry" => OrderedJson::string(entry),
                "revision" => OrderedJson::number_u64(revision),
                "content_type" => OrderedJson::string(file.content_type),
                "bytes_total" => OrderedJson::number_u64(page.bytes_total),
                "offset" => OrderedJson::number_u64(page.offset),
                "bytes_returned" => OrderedJson::number_u64(page.bytes_returned),
                "truncated" => OrderedJson::Bool(page.truncated),
                "content" => OrderedJson::string(page.content),
            },
            artifact_key.as_deref(),
        ));
    }

    if arguments.get("path").is_some() {
        return Err(
            AppError::Validation("path only applies to bundle artifacts".to_owned()).into(),
        );
    }
    let file = if let Some(historical_revision) = historical {
        deps.artifacts
            .read_revision_body(&artifact, historical_revision, None)
            .await?
    } else {
        deps.artifacts.read_body(&artifact).await?
    }
    .ok_or_else(|| {
        if historical.is_some() {
            AppError::Gone(format!("Revision {revision} is no longer retained"))
        } else {
            AppError::Gone(format!("Artifact body is unavailable: {id}"))
        }
    })?;
    let page = page_utf8(&file.content, requested_offset, limit);
    tool_result(with_artifact_key(
        object! {
            "id" => OrderedJson::string(id),
            "org" => OrderedJson::string(current.org.0),
            "is_bundle" => OrderedJson::Bool(false),
            "revision" => OrderedJson::number_u64(revision),
            "content_type" => OrderedJson::string(file.content_type),
            "bytes_total" => OrderedJson::number_u64(page.bytes_total),
            "offset" => OrderedJson::number_u64(page.offset),
            "bytes_returned" => OrderedJson::number_u64(page.bytes_returned),
            "truncated" => OrderedJson::Bool(page.truncated),
            "content" => OrderedJson::string(page.content),
        },
        artifact_key.as_deref(),
    ))
}

pub(crate) async fn delete_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let owned = publisher_delete(id, auth, deps).await?;
    let deleted = deps
        .artifacts
        .delete(owned.into_authorized(), MutationAudit::publisher(auth)?)
        .await?;
    if deleted {
        deps.delivery_wake.wake();
    }
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "deleted" => OrderedJson::Bool(deleted),
    })
}

pub(crate) async fn set_visibility(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_delete(id, auth, deps).await?;
    let hidden = arguments
        .get("hidden")
        .and_then(OrderedJson::as_bool)
        .ok_or_else(|| JsonRpcError::InvalidParams("hidden is required".to_owned()))?;
    let result = deps
        .artifacts
        .set_hidden(
            owned.into_authorized(),
            hidden,
            MutationAudit::publisher(auth)?,
        )
        .await?;
    tool_result(object! {
        "id" => OrderedJson::string(result.id.0),
        "hidden" => OrderedJson::Bool(result.hidden),
    })
}

pub(crate) async fn list_revisions(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let owned = publisher_read(id, auth, deps).await?;
    let history = deps
        .artifacts
        .list_revisions(&owned.into_authorized())
        .await?;
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "current" => OrderedJson::number_u64(history.current),
        "revisions" => OrderedJson::Array(history.revisions.iter().map(revision_json).collect()),
    })
}

pub(crate) async fn artifact_stats(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_read(id, auth, deps).await?;
    let authorized = owned.into_authorized();
    let counts = deps.engagement.view_counts(&authorized).await?;
    let viewers = deps.engagement.viewers(&authorized).await?;
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "views" => OrderedJson::number_u64(counts.views),
        "unique_viewers" => OrderedJson::number_u64(counts.unique_viewers),
        "last_viewed_at" => optional_timestamp(counts.last_viewed_at),
        "viewers" => OrderedJson::Array(viewers.iter().map(viewer_json).collect()),
    })
}

pub(crate) async fn restore_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let revision = positive_integer(
        arguments
            .get("revision")
            .ok_or_else(|| JsonRpcError::InvalidParams("revision is required".to_owned()))?,
        "revision",
    )?;
    let owned = publisher_write(id, auth, deps).await?;
    let result = match deps
        .artifacts
        .restore(
            owned.into_authorized(),
            revision,
            Some(auth.client_id.clone()),
            MutationAudit::publisher(auth)?,
        )
        .await
    {
        Ok(result) => result,
        Err(AppError::NotFound(reason)) if reason == "not_found" => {
            return Err(AppError::NotFound(format!("Unknown artifact: {id}")).into());
        }
        Err(AppError::Forbidden(_)) => {
            return Err(AppError::NotFound(format!("Unknown artifact: {id}")).into());
        }
        Err(AppError::NotFound(reason)) if reason == "revision_not_found" => {
            return Err(AppError::NotFound(format!("No such revision: {revision}")).into());
        }
        Err(AppError::Gone(reason)) if reason == "body_missing" => {
            return Err(
                AppError::Gone(format!("Revision {revision} is no longer retained")).into(),
            );
        }
        Err(AppError::Conflict(reason)) if reason == "type_mismatch" => {
            return Err(AppError::Conflict(
                "Revision type does not match the current artifact".to_owned(),
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    deps.delivery_wake.wake();
    tool_result(object! {
        "id" => OrderedJson::string(result.meta.id.0.clone()),
        "url" => OrderedJson::string(artifact_url(deps, &result.meta.id)),
        "revision" => OrderedJson::number_u64(result.meta.revision),
        "restoredFrom" => OrderedJson::number_u64(result.restored_from),
        "bytes" => OrderedJson::number_u64(result.meta.bytes),
    })
}

#[derive(Debug, PartialEq, Eq)]
struct BytePage {
    bytes_total: u64,
    offset: u64,
    bytes_returned: u64,
    truncated: bool,
    content: String,
}

fn page_utf8(content: &[u8], requested_offset: u64, limit: u64) -> BytePage {
    let total = content.len();
    let requested = usize::try_from(requested_offset).unwrap_or(usize::MAX);
    let mut offset = requested.min(total);
    while offset < total && content[offset] & 0xc0 == 0x80 {
        offset += 1;
    }

    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut end = offset.saturating_add(limit).min(total);
    while end > offset && end < total && content[end] & 0xc0 == 0x80 {
        end -= 1;
    }

    BytePage {
        bytes_total: u64::try_from(total).unwrap_or(u64::MAX),
        offset: u64::try_from(offset).unwrap_or(u64::MAX),
        bytes_returned: u64::try_from(end - offset).unwrap_or(u64::MAX),
        truncated: end < total,
        content: String::from_utf8_lossy(&content[offset..end]).into_owned(),
    }
}
