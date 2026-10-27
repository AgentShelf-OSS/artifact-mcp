//! Publish tools, including keyed create-or-update (ADR-0015).

use serde_json::Value;

use crate::{
    AppDeps,
    artifacts::lifecycle::ARTIFACT_KEY_TAKEN,
    error::{AppError, JsonRpcError, McpError},
    mcp::{
        args::{bundle_files, ensure_can_publish, optional_string, required_string, target_org},
        protocol::OrderedJson,
        shape::{artifact_url, object, tool_result},
    },
    model::{ArtifactContent, ArtifactMeta, ArtifactUpdate, OrgId, PublisherIdentity},
    security::{access::AccessPolicy, audit::MutationAudit},
};

use super::update::{discard_newer_argument, run_update, stale_base_guard, with_warning};

pub(crate) async fn publish_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let artifact_key = artifact_key_argument(arguments)?;
    discard_newer_argument(arguments)?;
    ensure_can_publish(auth)?;
    let target_org = target_org(arguments, auth, deps).await?;
    if let Some(artifact_key) = artifact_key {
        return keyed_publish(
            PublishKind::Single,
            arguments,
            auth,
            deps,
            target_org,
            artifact_key,
        )
        .await;
    }
    let published = deps
        .artifacts
        .publish(
            crate::model::PublishArtifact {
                publisher: auth.clone(),
                target_org: target_org.clone(),
                title: optional_string(arguments, "title"),
                description: optional_string(arguments, "description"),
                category: optional_string(arguments, "category"),
                content: ArtifactContent::SingleHtml(
                    required_string(arguments, "html")?.to_owned(),
                ),
                artifact_key: None,
            },
            MutationAudit::publisher(auth)?,
        )
        .await?;
    deps.delivery_wake.wake();
    tool_result(object! {
        "id" => OrderedJson::string(published.meta.id.0.clone()),
        "url" => OrderedJson::string(artifact_url(deps, &published.meta.id)),
        "org" => OrderedJson::string(target_org.0),
        "bytes" => OrderedJson::number_u64(published.meta.bytes),
        "category" => OrderedJson::string(published.meta.category),
    })
}

pub(crate) async fn publish_bundle(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let artifact_key = artifact_key_argument(arguments)?;
    discard_newer_argument(arguments)?;
    ensure_can_publish(auth)?;
    let target_org = target_org(arguments, auth, deps).await?;
    if let Some(artifact_key) = artifact_key {
        return keyed_publish(
            PublishKind::Bundle,
            arguments,
            auth,
            deps,
            target_org,
            artifact_key,
        )
        .await;
    }
    let files = bundle_files(arguments.get("files"))?;
    let published = deps
        .artifacts
        .publish(
            crate::model::PublishArtifact {
                publisher: auth.clone(),
                target_org: target_org.clone(),
                title: optional_string(arguments, "title"),
                description: optional_string(arguments, "description"),
                category: optional_string(arguments, "category"),
                content: ArtifactContent::Bundle {
                    files,
                    entry: optional_string(arguments, "entry"),
                },
                artifact_key: None,
            },
            MutationAudit::publisher(auth)?,
        )
        .await?;
    deps.delivery_wake.wake();
    let file_count = u64::try_from(published.file_count.unwrap_or(0)).unwrap_or(u64::MAX);
    tool_result(object! {
        "id" => OrderedJson::string(published.meta.id.0.clone()),
        "url" => OrderedJson::string(artifact_url(deps, &published.meta.id)),
        "org" => OrderedJson::string(target_org.0),
        "entry" => OrderedJson::string(published.meta.entry.clone()),
        "files" => OrderedJson::number_u64(file_count),
        "bytes" => OrderedJson::number_u64(published.meta.bytes),
        "category" => OrderedJson::string(published.meta.category),
    })
}

/// ADR-0015 artifact key grammar: `^[a-z0-9][a-z0-9._/-]{0,127}$`, as in `lib/mcp.js`.
pub const ARTIFACT_KEY_INVALID: &str = "artifact_key must be 1-128 characters of a-z, 0-9, '.', '_', '/', or '-', starting with a letter or digit";
pub(crate) const UPDATE_CONFLICT_ERROR: &str =
    "Artifact changed during update; fetch its current revision and retry";
pub(crate) const UPDATE_UNAVAILABLE_ERROR: &str =
    "Artifact not found or you are not authorized to update it";

#[must_use]
pub fn is_valid_artifact_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    let allowed = |byte: &u8| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'.' | b'_' | b'/' | b'-')
    };
    matches!(bytes.first(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.len() <= 128
        && bytes.iter().all(allowed)
}

fn artifact_key_argument(arguments: &OrderedJson) -> Result<Option<String>, McpError> {
    let Some(value) = arguments.get("artifact_key") else {
        return Ok(None);
    };
    match value.as_str() {
        Some(key) if is_valid_artifact_key(key) => Ok(Some(key.to_owned())),
        _ => Err(JsonRpcError::InvalidParams(ARTIFACT_KEY_INVALID.to_owned()).into()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublishKind {
    Single,
    Bundle,
}

/// Create-or-update by `(org, artifact_key)`. An existing artifact is updated under exactly the
/// `update_artifact` policy and semantics. A concurrent create that wins the unique index makes
/// this call re-resolve the key once and update the winner instead of failing.
async fn keyed_publish(
    kind: PublishKind,
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    target_org: OrgId,
    artifact_key: String,
) -> Result<Value, McpError> {
    for _attempt in 0..2 {
        let existing = deps
            .artifacts
            .find_meta_by_key(&target_org, &artifact_key)
            .await?;
        let Some(existing) = existing else {
            match keyed_create(kind, arguments, auth, deps, &target_org, &artifact_key).await {
                Err(McpError::Tool(AppError::Conflict(message)))
                    if message == ARTIFACT_KEY_TAKEN =>
                {
                    continue;
                }
                other => return other,
            }
        };
        return keyed_update(kind, arguments, auth, deps, existing, &artifact_key).await;
    }
    Err(
        AppError::Conflict("artifact_key is being published by another request; retry".to_owned())
            .into(),
    )
}

async fn keyed_create(
    kind: PublishKind,
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    target_org: &OrgId,
    artifact_key: &str,
) -> Result<Value, McpError> {
    let content = match kind {
        PublishKind::Single => {
            ArtifactContent::SingleHtml(required_string(arguments, "html")?.to_owned())
        }
        PublishKind::Bundle => ArtifactContent::Bundle {
            files: bundle_files(arguments.get("files"))?,
            entry: optional_string(arguments, "entry"),
        },
    };
    let published = deps
        .artifacts
        .publish(
            crate::model::PublishArtifact {
                publisher: auth.clone(),
                target_org: target_org.clone(),
                title: optional_string(arguments, "title"),
                description: optional_string(arguments, "description"),
                category: optional_string(arguments, "category"),
                content,
                artifact_key: Some(artifact_key.to_owned()),
            },
            MutationAudit::publisher(auth)?,
        )
        .await?;
    deps.delivery_wake.wake();
    let meta = published.meta;
    let mut payload = vec![
        ("id".to_owned(), OrderedJson::string(meta.id.0.clone())),
        (
            "url".to_owned(),
            OrderedJson::string(artifact_url(deps, &meta.id)),
        ),
        ("org".to_owned(), OrderedJson::string(target_org.0.clone())),
    ];
    if kind == PublishKind::Bundle {
        let file_count = u64::try_from(published.file_count.unwrap_or(0)).unwrap_or(u64::MAX);
        payload.push(("entry".to_owned(), OrderedJson::string(meta.entry.clone())));
        payload.push(("files".to_owned(), OrderedJson::number_u64(file_count)));
    }
    payload.extend([
        ("bytes".to_owned(), OrderedJson::number_u64(meta.bytes)),
        ("category".to_owned(), OrderedJson::string(meta.category)),
        (
            "revision".to_owned(),
            OrderedJson::number_u64(meta.revision),
        ),
        ("artifact_key".to_owned(), OrderedJson::string(artifact_key)),
        ("created".to_owned(), OrderedJson::Bool(true)),
    ]);
    tool_result(OrderedJson::Object(payload))
}

async fn keyed_update(
    kind: PublishKind,
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    existing: ArtifactMeta,
    artifact_key: &str,
) -> Result<Value, McpError> {
    let id = existing.id.0.clone();
    let owned = AccessPolicy::authorize_publisher_write(auth, Some(existing), &id, "")?;
    let pre = owned.meta().clone();
    if pre.is_bundle != (kind == PublishKind::Bundle) {
        let message = if pre.is_bundle {
            format!(
                "artifact_key \"{artifact_key}\" belongs to a bundle artifact; use publish_bundle"
            )
        } else {
            format!(
                "artifact_key \"{artifact_key}\" belongs to a single-file artifact; use publish_artifact"
            )
        };
        return Err(AppError::Validation(message).into());
    }
    let (content, file_count) = match kind {
        PublishKind::Single => (
            ArtifactContent::SingleHtml(required_string(arguments, "html")?.to_owned()),
            None,
        ),
        PublishKind::Bundle => {
            let files = bundle_files(arguments.get("files"))?;
            // An empty bundle is the entry-only update sentinel; Node rejects an empty snapshot.
            if files.is_empty() {
                return Err(AppError::Validation("files is empty".to_owned()).into());
            }
            let count = u64::try_from(files.len()).unwrap_or(u64::MAX);
            (
                ArtifactContent::Bundle {
                    files,
                    entry: optional_string(arguments, "entry"),
                },
                Some(count),
            )
        }
    };
    let warning =
        stale_base_guard(&owned, &content, discard_newer_argument(arguments)?, deps).await?;
    let update = ArtifactUpdate {
        expected_revision: pre.revision,
        acting_client_id: Some(auth.client_id.clone()),
        title: optional_string(arguments, "title"),
        description: optional_string(arguments, "description"),
        category: optional_string(arguments, "category"),
        content: Some(content),
    };
    let result = run_update(owned, update, auth, deps).await?;
    let meta = result.meta;
    let mut payload = vec![
        ("id".to_owned(), OrderedJson::string(meta.id.0.clone())),
        (
            "url".to_owned(),
            OrderedJson::string(artifact_url(deps, &meta.id)),
        ),
        ("org".to_owned(), OrderedJson::string(pre.org.0)),
    ];
    if let Some(file_count) = file_count {
        payload.push(("entry".to_owned(), OrderedJson::string(meta.entry.clone())));
        payload.push(("files".to_owned(), OrderedJson::number_u64(file_count)));
    }
    payload.extend([
        ("bytes".to_owned(), OrderedJson::number_u64(meta.bytes)),
        ("category".to_owned(), OrderedJson::string(meta.category)),
        (
            "revision".to_owned(),
            OrderedJson::number_u64(meta.revision),
        ),
        ("artifact_key".to_owned(), OrderedJson::string(artifact_key)),
        ("created".to_owned(), OrderedJson::Bool(false)),
    ]);
    tool_result(with_warning(OrderedJson::Object(payload), warning))
}
