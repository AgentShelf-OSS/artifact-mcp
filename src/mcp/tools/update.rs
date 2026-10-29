//! Full-content update and patch tools, with the stale-base guard (ADR-0023).

use serde_json::Value;

use crate::{
    AppDeps,
    artifacts::{
        stale_guard::{self, DigestSet},
        validation::sanitize_relative_path,
    },
    config::StaleGuardMode,
    error::{AppError, JsonRpcError, McpError},
    mcp::{
        args::{
            bundle_files, nonempty_protocol_string, optional_string, positive_integer,
            publisher_write, safe_positive_integer,
        },
        protocol::OrderedJson,
        shape::{artifact_url, object, tool_result},
        validation::apply_utf8_edits,
    },
    model::{ArtifactContent, ArtifactUpdate, PublisherIdentity, UpdateArtifactResult},
    security::{
        access::{AuthorizedArtifact, OwnedArtifact},
        audit::MutationAudit,
    },
};

use super::publish::{UPDATE_CONFLICT_ERROR, UPDATE_UNAVAILABLE_ERROR};

/// ADR-0023 invalid-params text for a non-boolean `discard_newer`.
pub const DISCARD_NEWER_INVALID: &str = "Invalid arguments: discard_newer must be a boolean";

pub(crate) fn discard_newer_argument(arguments: &OrderedJson) -> Result<bool, McpError> {
    match arguments.get("discard_newer") {
        None => Ok(false),
        Some(OrderedJson::Bool(value)) => Ok(*value),
        Some(_) => Err(JsonRpcError::InvalidParams(DISCARD_NEWER_INVALID.to_owned()).into()),
    }
}

fn content_digests(content: &ArtifactContent) -> DigestSet {
    match content {
        ArtifactContent::SingleHtml(html) => stale_guard::digests([(None, html.as_bytes())]),
        ArtifactContent::Bundle { files, .. } => {
            // Digest new files under the same sanitized path the store will write them to.
            let mut set = DigestSet::new();
            for (path, body) in files {
                let path = sanitize_relative_path(path).unwrap_or_else(|| path.clone());
                stale_guard::add_file_digests(Some(&path), body.as_bytes(), &mut set);
            }
            set
        }
    }
}

/// Digest the current body (`revision` = `None`) or one retained revision. A body that is not
/// available yields `None`, so the caller skips that candidate.
async fn stored_digests(
    deps: &AppDeps,
    artifact: &AuthorizedArtifact,
    revision: Option<u64>,
    is_bundle: bool,
) -> Result<Option<DigestSet>, McpError> {
    if !is_bundle {
        let file = match revision {
            Some(revision) => {
                deps.artifacts
                    .read_revision_body(artifact, revision, None)
                    .await?
            }
            None => deps.artifacts.read_body(artifact).await?,
        };
        return Ok(file.map(|file| stale_guard::digests([(None, file.content.as_slice())])));
    }
    let Some(listing) = deps.artifacts.list_bundle_files(artifact, revision).await? else {
        return Ok(None);
    };
    let mut set = DigestSet::new();
    for (path, _bytes) in listing {
        let file = match revision {
            Some(revision) => {
                deps.artifacts
                    .read_revision_body(artifact, revision, Some(&path))
                    .await?
            }
            None => deps.artifacts.read_bundle_file(artifact, &path).await?,
        };
        if let Some(file) = file {
            stale_guard::add_file_digests(Some(&path), &file.content, &mut set);
        }
    }
    Ok(Some(set))
}

/// ADR-0023: decide whether a full-content update is based on an older revision. Runs only after
/// the caller is authorized to update the artifact. Returns the warning text to attach in `warn`
/// mode, refuses the update in `enforce` mode, and does nothing in `off` mode.
pub(crate) async fn stale_base_guard(
    owned: &OwnedArtifact,
    content: &ArtifactContent,
    discard_newer: bool,
    deps: &AppDeps,
) -> Result<Option<String>, McpError> {
    let settings = deps.config.stale_guard;
    if settings.mode == StaleGuardMode::Off {
        return Ok(None);
    }
    let new = content_digests(content);
    if new.is_empty() {
        return Ok(None);
    }
    let artifact = owned.clone().into_authorized();
    let current_meta = artifact.meta().clone();
    let current_revision = current_meta.revision;
    let Some(current) = stored_digests(deps, &artifact, None, current_meta.is_bundle).await? else {
        return Ok(None);
    };
    if current.is_empty() {
        return Ok(None);
    }
    let retained = deps.artifacts.list_revisions(&artifact).await?;
    let mut history = Vec::new();
    for row in retained.revisions {
        if row.revision >= current_revision {
            continue;
        }
        if let Some(set) =
            stored_digests(deps, &artifact, Some(row.revision), row.is_bundle).await?
        {
            history.push((row.revision, set));
        }
    }
    let evaluation = stale_guard::evaluate(
        &new,
        &current,
        current_revision,
        &history,
        stale_guard::Thresholds {
            min_segments: settings.min_segments,
            min_share: settings.min_share,
        },
    );
    let Some(message) = evaluation.message.filter(|_| evaluation.stale) else {
        return Ok(None);
    };
    tracing::warn!(
        "stale_base_detected id={} base={} current={} newer={} lost={} mode={} discard_newer={}",
        current_meta.id.0,
        evaluation.base,
        current_revision,
        evaluation.newer_segments,
        evaluation.lost_segments,
        settings.mode.as_str(),
        discard_newer
    );
    if discard_newer {
        return Ok(None);
    }
    match settings.mode {
        StaleGuardMode::Enforce => Err(AppError::Conflict(message).into()),
        StaleGuardMode::Warn => Ok(Some(message)),
        StaleGuardMode::Off => Ok(None),
    }
}

/// Append `warnings` only when the stale-base guard produced one, so other output is unchanged.
pub(crate) fn with_warning(payload: OrderedJson, warning: Option<String>) -> OrderedJson {
    match (payload, warning) {
        (OrderedJson::Object(mut entries), Some(warning)) => {
            entries.push((
                "warnings".to_owned(),
                OrderedJson::Array(vec![OrderedJson::string(warning)]),
            ));
            OrderedJson::Object(entries)
        }
        (payload, _) => payload,
    }
}

/// Shared `update_artifact` execution and error mapping, also used by keyed publish.
pub(crate) async fn run_update(
    owned: OwnedArtifact,
    update: ArtifactUpdate,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<crate::model::UpdateArtifactResult, McpError> {
    let result = match deps
        .artifacts
        .update(
            owned.into_authorized(),
            update,
            MutationAudit::publisher(auth)?,
        )
        .await
    {
        Ok(result) => result,
        Err(AppError::Conflict(_)) => {
            return Err(AppError::Conflict(UPDATE_CONFLICT_ERROR.to_owned()).into());
        }
        Err(AppError::NotFound(_) | AppError::Forbidden(_)) => {
            return Err(AppError::NotFound(UPDATE_UNAVAILABLE_ERROR.to_owned()).into());
        }
        Err(error) => return Err(error.into()),
    };
    if result.changed {
        deps.delivery_wake.wake();
    }
    Ok(result)
}

pub(crate) async fn patch_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let expected_revision = safe_positive_integer(
        arguments.get("expected_revision").ok_or_else(|| {
            JsonRpcError::InvalidParams("expected_revision is required".to_owned())
        })?,
        "expected_revision",
    )?;
    let edits = arguments
        .get("edits")
        .ok_or_else(|| JsonRpcError::InvalidParams("edits is required".to_owned()))?;
    let edit_count =
        u64::try_from(edits.as_array().map_or(0, <[OrderedJson]>::len)).unwrap_or(u64::MAX);
    let unavailable = "Artifact not found or you are not authorized to update it";
    let owned = publisher_write(id, auth, deps).await?;
    let pre = owned.meta().clone();
    if expected_revision != pre.revision {
        return Err(AppError::Conflict(
            "Artifact changed during update; fetch its current revision and retry".to_owned(),
        )
        .into());
    }
    let artifact = owned.into_authorized();

    let content = if pre.is_bundle {
        let path = arguments
            .get("path")
            .and_then(OrderedJson::as_str)
            .ok_or_else(|| {
                AppError::Validation("path is required for bundle artifacts".to_owned())
            })?;
        let target_path = if path.is_empty() {
            pre.entry.clone()
        } else {
            sanitize_relative_path(path)
                .ok_or_else(|| AppError::NotFound(format!("Unknown bundle file: {path}")))?
        };
        let target = deps
            .artifacts
            .read_bundle_file(&artifact, path)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Unknown bundle file: {path}")))?;
        let patched = apply_utf8_edits(&target.content, edits)?;
        let listing = deps
            .artifacts
            .list_bundle_files(&artifact, None)
            .await?
            .ok_or_else(|| AppError::Gone(format!("Artifact body is unavailable: {id}")))?;
        let mut files = Vec::with_capacity(listing.len());
        for (file_path, _) in listing {
            let file_content = if file_path == target_path {
                String::from_utf8_lossy(&patched).into_owned()
            } else {
                let file = deps
                    .artifacts
                    .read_bundle_file(&artifact, &file_path)
                    .await?
                    .ok_or_else(|| AppError::Gone(format!("Artifact body is unavailable: {id}")))?;
                String::from_utf8_lossy(&file.content).into_owned()
            };
            files.push((file_path, file_content));
        }
        ArtifactContent::Bundle {
            files,
            entry: Some(pre.entry.clone()),
        }
    } else {
        if arguments.get("path").is_some() {
            return Err(
                AppError::Validation("path only applies to bundle artifacts".to_owned()).into(),
            );
        }
        let file = deps
            .artifacts
            .read_body(&artifact)
            .await?
            .ok_or_else(|| AppError::Gone(format!("Artifact body is unavailable: {id}")))?;
        let patched = apply_utf8_edits(&file.content, edits)?;
        ArtifactContent::SingleHtml(String::from_utf8_lossy(&patched).into_owned())
    };

    let result = apply_patch_update(
        artifact,
        content,
        expected_revision,
        auth,
        deps,
        unavailable,
    )
    .await?;
    tool_result(object! {
        "id" => OrderedJson::string(result.meta.id.0.clone()),
        "revision" => OrderedJson::number_u64(result.meta.revision),
        "bytes_before" => OrderedJson::number_u64(pre.bytes),
        "bytes_after" => OrderedJson::number_u64(result.meta.bytes),
        "edits_applied" => OrderedJson::number_u64(edit_count),
    })
}

async fn apply_patch_update(
    artifact: AuthorizedArtifact,
    content: ArtifactContent,
    expected_revision: u64,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    unavailable: &str,
) -> Result<UpdateArtifactResult, McpError> {
    let result = match deps
        .artifacts
        .update(
            artifact,
            ArtifactUpdate {
                expected_revision,
                acting_client_id: Some(auth.client_id.clone()),
                title: None,
                description: None,
                category: None,
                content: Some(content),
            },
            MutationAudit::publisher(auth)?,
        )
        .await
    {
        Ok(result) => result,
        Err(AppError::Conflict(_)) => {
            return Err(AppError::Conflict(
                "Artifact changed during update; fetch its current revision and retry".to_owned(),
            )
            .into());
        }
        Err(AppError::NotFound(_) | AppError::Forbidden(_)) => {
            return Err(AppError::NotFound(unavailable.to_owned()).into());
        }
        Err(error) => return Err(error.into()),
    };
    if result.changed {
        deps.delivery_wake.wake();
    }
    Ok(result)
}

pub(crate) async fn update_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let discard_newer = discard_newer_argument(arguments)?;
    let owned = publisher_write(id, auth, deps).await?;
    let pre = owned.meta().clone();
    let expected_revision = match arguments.get("expected_revision") {
        Some(value) => positive_integer(value, "expected_revision")?,
        None => pre.revision,
    };
    let html = arguments.get("html").and_then(OrderedJson::as_str);
    let files_value = arguments.get("files");
    let entry = arguments.get("entry").and_then(OrderedJson::as_str);
    if html.is_some() && files_value.is_some() {
        return Err(
            AppError::Validation("provide either html or files, not both".to_owned()).into(),
        );
    }
    if html.is_some() && entry.is_some() {
        let message = if pre.is_bundle {
            "artifact is a bundle; pass files, not html"
        } else {
            "artifact is single-file; entry only applies to bundles"
        };
        return Err(AppError::Validation(message.to_owned()).into());
    }
    let content = if let Some(files_value) = files_value {
        let files = bundle_files(Some(files_value))?;
        // U08 uses an empty bundle plus `Some(entry)` as the sentinel for an entry-only update.
        // A caller-provided empty `files` object is not that sentinel: Node rejects it even when
        // `entry` is also present, so intercept it before constructing `ArtifactUpdate`.
        if files.is_empty() {
            return Err(AppError::Validation("files is empty".to_owned()).into());
        }
        Some(ArtifactContent::Bundle {
            files,
            entry: entry.map(ToOwned::to_owned),
        })
    } else if let Some(html) = html {
        Some(ArtifactContent::SingleHtml(html.to_owned()))
    } else {
        entry.map(|entry| ArtifactContent::Bundle {
            files: Vec::new(),
            entry: Some(entry.to_owned()),
        })
    };
    // ADR-0023: only full-content updates are checked, and a mismatched expected_revision keeps
    // the existing conflict error.
    let full_content = html.is_some() || files_value.is_some();
    let warning = match &content {
        // A body of the wrong type is skipped so the existing type error wins.
        Some(content)
            if full_content
                && matches!(content, ArtifactContent::Bundle { .. }) == pre.is_bundle
                && expected_revision == pre.revision =>
        {
            stale_base_guard(&owned, content, discard_newer, deps).await?
        }
        _ => None,
    };
    let update = ArtifactUpdate {
        expected_revision,
        acting_client_id: Some(auth.client_id.clone()),
        title: optional_string(arguments, "title"),
        description: optional_string(arguments, "description"),
        category: optional_string(arguments, "category"),
        content,
    };
    let result = run_update(owned, update, auth, deps).await?;
    tool_result(with_warning(
        object! {
            "id" => OrderedJson::string(result.meta.id.0.clone()),
            "url" => OrderedJson::string(artifact_url(deps, &result.meta.id)),
            "revision" => OrderedJson::number_u64(result.meta.revision),
            "bytes" => OrderedJson::number_u64(result.meta.bytes),
            "entry" => OrderedJson::string(result.meta.entry),
            "category" => OrderedJson::string(result.meta.category),
        },
        warning,
    ))
}
