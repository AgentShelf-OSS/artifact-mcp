//! MCP method and tool dispatch over the frozen service traits.

use serde_json::{Value, json};

use crate::{
    AppDeps,
    artifacts::{
        lifecycle::normalize_category,
        validation::{js_trim, sanitize_relative_path},
    },
    error::{AppError, JsonRpcError, McpError},
    model::{
        ArtifactContent, ArtifactId, ArtifactMeta, ArtifactRevision, ArtifactUpdate, CreateShare,
        EmailAddress, Feedback, OrgId, PublicShare, PublisherIdentity, ShareToken, SubmitFeedback,
        Timestamp, ViewerView,
    },
    security::{
        access::{AccessPolicy, OwnedArtifact, PUBLISH_PERMISSION_ERROR},
        audit::MutationAudit,
    },
};

use super::{
    apps::client_supports_apps,
    protocol::OrderedJson,
    tasks::{TASKS_EXTENSION, client_supports_tasks},
    tool_defs::{
        frozen_tool_definitions, modern_tool_definitions_for_client, tool_output_schema,
        validation_property_order,
    },
    validation::{apply_utf8_edits, validate_schema_input},
};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] = [MODERN_PROTOCOL_VERSION, PROTOCOL_VERSION];
pub const SERVER_NAME: &str = "artifact-mcp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire-era selection is explicit so modern per-request semantics never leak into legacy clients.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProtocolEra {
    #[default]
    Legacy,
    Modern,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedToolCall {
    pub name: String,
    pub arguments: OrderedJson,
}

macro_rules! object {
    ($($key:literal => $value:expr),* $(,)?) => {
        OrderedJson::Object(vec![$(($key.to_owned(), $value)),*])
    };
}

/// Dispatch one structurally-valid JSON-RPC message.
pub async fn dispatch(
    message: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    dispatch_for_era(message, auth, deps, ProtocolEra::Legacy).await
}

/// Dispatch one request according to its independently-selected protocol era.
pub async fn dispatch_for_era(
    message: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    era: ProtocolEra,
) -> Result<Value, McpError> {
    if let Some(result) = dispatch_protocol_for_era(message, era) {
        let mut result = result?;
        if era == ProtocolEra::Modern
            && message.get("method").and_then(OrderedJson::as_str) == Some("server/discover")
            && deps.config.oauth.enabled()
            && let Some(capabilities) = result
                .get_mut("capabilities")
                .and_then(Value::as_object_mut)
        {
            let extensions = capabilities
                .entry("extensions")
                .or_insert_with(|| json!({}));
            if let Some(extensions) = extensions.as_object_mut() {
                extensions.insert(
                    crate::security::oauth::OAUTH_EXTENSION.to_owned(),
                    json!({}),
                );
            }
        }
        return Ok(result);
    }
    match message.get("method").and_then(OrderedJson::as_str) {
        Some("tools/call") => {
            let allow_app_tools = era == ProtocolEra::Modern && client_supports_apps(message);
            let allow_tasks = era == ProtocolEra::Modern && client_supports_tasks(message);
            let result = call_tool(
                message.get("params"),
                auth,
                deps,
                era == ProtocolEra::Modern,
                allow_app_tools,
                allow_tasks,
            )
            .await?;
            if era == ProtocolEra::Modern {
                let result = add_artifact_resource_link(message, result);
                Ok(add_review_app_data(message, result, auth, deps).await)
            } else {
                Ok(result)
            }
        }
        Some("resources/list" | "resources/templates/list" | "resources/read")
            if era == ProtocolEra::Modern =>
        {
            super::resources::dispatch(message, auth, deps).await
        }
        Some("tasks/get" | "tasks/update" | "tasks/cancel") if era == ProtocolEra::Modern => {
            if !client_supports_tasks(message) {
                return Err(JsonRpcError::InvalidParams(format!(
                    "Missing required client capability: {TASKS_EXTENSION}"
                ))
                .into());
            }
            super::tasks::dispatch_task_method(message, auth, deps)
        }
        Some(method) => Err(JsonRpcError::MethodNotFound(method.to_owned()).into()),
        None => Err(JsonRpcError::InvalidRequest.into()),
    }
}

async fn add_review_app_data(
    message: &OrderedJson,
    mut result: Value,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Value {
    if !client_supports_apps(message) {
        return result;
    }
    let tool_name = message
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(OrderedJson::as_str);
    if !matches!(
        tool_name,
        Some(
            "publish_artifact"
                | "publish_bundle"
                | "list_artifacts"
                | "read_artifact"
                | "list_revisions"
                | "set_visibility"
                | "delete_artifact"
                | "create_share"
                | "submit_feedback"
        )
    ) {
        return result;
    }

    if tool_name == Some("delete_artifact") {
        let id = result
            .get("structuredContent")
            .and_then(|content| content.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let deleted = result
            .get("structuredContent")
            .and_then(|content| content.get("deleted"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        insert_result_meta(
            &mut result,
            "com.agentshelf.artifact-mcp/audit",
            json!({
                "action": "delete",
                "artifactId": id,
                "actor": format!("agent:{}", auth.client_id),
                "outcome": if deleted { "deleted" } else { "unchanged" }
            }),
        );
        return result;
    }

    let reviews = if tool_name == Some("list_artifacts") {
        let ids = result
            .get("structuredContent")
            .and_then(|content| content.get("artifacts"))
            .and_then(Value::as_array)
            .map(|artifacts| {
                artifacts
                    .iter()
                    .filter_map(|artifact| artifact.get("id").and_then(Value::as_str))
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut reviews = Vec::with_capacity(ids.len());
        for id in ids {
            if let Ok(Some(meta)) = deps
                .artifacts
                .find_meta(&ArtifactId::from(id.as_str()))
                .await
                && AccessPolicy::authorize_publisher_read(auth, Some(meta.clone()), &id).is_ok()
            {
                reviews.push(review_from_meta(&meta, auth, deps));
            }
        }
        reviews
    } else {
        let id = result
            .get("structuredContent")
            .and_then(|content| content.get("id"))
            .and_then(Value::as_str);
        if let Some(id) = id {
            match deps.artifacts.find_meta(&ArtifactId::from(id)).await {
                Ok(Some(meta))
                    if AccessPolicy::authorize_publisher_read(auth, Some(meta.clone()), id)
                        .is_ok() =>
                {
                    vec![review_from_meta(&meta, auth, deps)]
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        }
    };
    if reviews.is_empty() {
        return result;
    }

    insert_result_meta(
        &mut result,
        "com.agentshelf.artifact-mcp/review",
        json!({ "artifacts": reviews }),
    );
    result
}

fn insert_result_meta(result: &mut Value, key: &str, value: Value) {
    let meta = result
        .as_object_mut()
        .expect("MCP tool results are objects")
        .entry("_meta")
        .or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    meta.as_object_mut()
        .expect("tool result metadata object")
        .insert(key.to_owned(), value);
}

fn review_from_meta(meta: &ArtifactMeta, auth: &PublisherIdentity, deps: &AppDeps) -> Value {
    json!({
        "id": meta.id.0,
        "url": artifact_url(deps, &meta.id),
        "title": if meta.title.trim().is_empty() { &meta.id.0 } else { &meta.title },
        "description": meta.description,
        "org": meta.org.0,
        "category": meta.category,
        "publisher": meta.uploader_label,
        "revision": meta.revision,
        "hidden": meta.hidden,
        "isBundle": meta.is_bundle,
        "canManage": AccessPolicy::publisher_can_delete(auth, meta),
        "canFeedback": AccessPolicy::publisher_can_read(auth, meta),
        "thumbnailResourceUri": format!("artifact://{}/thumbnail", meta.id.0)
    })
}

fn add_artifact_resource_link(message: &OrderedJson, mut result: Value) -> Value {
    let tool_name = message
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(OrderedJson::as_str);
    if !matches!(
        tool_name,
        Some(
            "publish_artifact"
                | "publish_bundle"
                | "update_artifact"
                | "patch_artifact"
                | "restore_artifact"
        )
    ) {
        return result;
    }
    let Some(id) = result
        .get("structuredContent")
        .and_then(|content| content.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return result;
    };
    let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return result;
    };
    content.push(json!({
        "type": "resource_link",
        "uri": format!("artifact://{id}"),
        "name": id,
        "description": "Authorized artifact resource"
    }));
    result
}

/// Dispatch methods that do not execute a tool. `None` means `tools/call`.
pub fn dispatch_protocol(message: &OrderedJson) -> Option<Result<Value, McpError>> {
    dispatch_protocol_for_era(message, ProtocolEra::Legacy)
}

/// Dispatch protocol methods without executing a tool.
pub fn dispatch_protocol_for_era(
    message: &OrderedJson,
    era: ProtocolEra,
) -> Option<Result<Value, McpError>> {
    let method = match message.get("method").and_then(OrderedJson::as_str) {
        Some(method) => method,
        None => return Some(Err(JsonRpcError::InvalidRequest.into())),
    };

    if era == ProtocolEra::Modern {
        if let Err(error) = validate_modern_request_metadata(message) {
            return Some(Err(error));
        }
        return Some(match method {
            "server/discover" => Ok(json!({
                "supportedVersions": SUPPORTED_PROTOCOL_VERSIONS,
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "listChanged": false, "subscribe": false },
                    "extensions": {
                        TASKS_EXTENSION: {}
                    }
                },
                "instructions": "Publish, organize, review, and manage authorized HTML artifacts.",
                "ttlMs": 3_600_000,
                "cacheScope": "private"
            })),
            "tools/list" => modern_tool_definitions_for_client(client_supports_apps(message))
                .map(|tools| {
                    json!({
                        "tools": tools,
                        "ttlMs": 300_000,
                        "cacheScope": "private"
                    })
                })
                .map_err(|error| {
                    JsonRpcError::Internal(format!("failed to load tool definitions: {error}"))
                        .into()
                }),
            "resources/templates/list" => Ok(super::resources::list_templates()),
            "tools/call" | "resources/list" | "resources/read" | "tasks/get" | "tasks/update"
            | "tasks/cancel" => {
                return None;
            }
            _ => Err(JsonRpcError::MethodNotFound(method.to_owned()).into()),
        });
    }

    Some(match method {
        "initialize" => {
            let requested = message
                .get("params")
                .and_then(|params| params.get("protocolVersion"))
                .filter(|value| javascript_truthy(value))
                .cloned()
                .map_or_else(
                    || Value::String(PROTOCOL_VERSION.to_owned()),
                    OrderedJson::into_value,
                );
            Ok(json!({
                "protocolVersion": requested,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            }))
        }
        "ping" | "notifications/initialized" => Ok(json!({})),
        "tools/list" => frozen_tool_definitions()
            .map(|tools| json!({ "tools": tools }))
            .map_err(|error| {
                JsonRpcError::Internal(format!("failed to load tool definitions: {error}")).into()
            }),
        "tools/call" => return None,
        _ => Err(JsonRpcError::MethodNotFound(method.to_owned()).into()),
    })
}

fn validate_modern_request_metadata(message: &OrderedJson) -> Result<(), McpError> {
    let meta = message
        .get("params")
        .and_then(|params| params.get("_meta"))
        .and_then(OrderedJson::as_object)
        .ok_or_else(|| {
            JsonRpcError::InvalidParams(
                "Missing required request metadata: params._meta".to_owned(),
            )
        })?;
    let protocol_version = meta
        .iter()
        .find_map(|(key, value)| {
            (key == "io.modelcontextprotocol/protocolVersion").then_some(value)
        })
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| {
            JsonRpcError::InvalidParams(
                "Missing required request metadata: io.modelcontextprotocol/protocolVersion"
                    .to_owned(),
            )
        })?;
    if protocol_version != MODERN_PROTOCOL_VERSION {
        return Err(JsonRpcError::UnsupportedProtocolVersion {
            requested: protocol_version.to_owned(),
        }
        .into());
    }
    let client_capabilities = meta.iter().find_map(|(key, value)| {
        (key == "io.modelcontextprotocol/clientCapabilities").then_some(value)
    });
    if client_capabilities
        .and_then(OrderedJson::as_object)
        .is_none()
    {
        return Err(JsonRpcError::InvalidParams(
            "Missing required request metadata: io.modelcontextprotocol/clientCapabilities"
                .to_owned(),
        )
        .into());
    }
    Ok(())
}

async fn call_tool(
    params: Option<&OrderedJson>,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    modern: bool,
    allow_app_tools: bool,
    allow_tasks: bool,
) -> Result<Value, McpError> {
    let call = validate_tool_call_for_client(params, modern, allow_app_tools)?;
    let name = call.name.as_str();
    let arguments = &call.arguments;

    let result = match name {
        "publish_artifact" => publish_artifact(arguments, auth, deps).await,
        "publish_bundle" => publish_bundle(arguments, auth, deps).await,
        "list_artifacts" => list_artifacts(auth, deps).await,
        "read_artifact" => read_artifact(arguments, auth, deps).await,
        "patch_artifact" => patch_artifact(arguments, auth, deps).await,
        "delete_artifact" => delete_artifact(arguments, auth, deps).await,
        "update_artifact" => update_artifact(arguments, auth, deps).await,
        "set_visibility" => set_visibility(arguments, auth, deps).await,
        "list_categories" => list_categories(arguments, auth, deps).await,
        "set_category" => set_category(arguments, auth, deps).await,
        "create_category" => create_category(arguments, auth, deps).await,
        "delete_category" => delete_category(arguments, auth, deps).await,
        "list_revisions" => list_revisions(arguments, auth, deps).await,
        "create_share" => create_share(arguments, auth, deps).await,
        "list_shares" => list_shares(arguments, auth, deps).await,
        "revoke_share" => revoke_share(arguments, auth, deps).await,
        "artifact_stats" => artifact_stats(arguments, auth, deps).await,
        "restore_artifact" => restore_artifact(arguments, auth, deps).await,
        "list_feedback" => list_feedback(arguments, auth, deps).await,
        "resolve_feedback" => resolve_feedback(arguments, auth, deps).await,
        "reopen_feedback" => reopen_feedback(arguments, auth, deps).await,
        "submit_feedback" if allow_app_tools => submit_feedback(arguments, auth, deps).await,
        "regenerate_artifact_preview" if modern && allow_tasks => {
            super::tasks::create_preview_task(required_string(arguments, "id")?, auth, deps).await
        }
        "regenerate_artifact_preview" if modern => {
            super::tasks::regenerate_synchronously(required_string(arguments, "id")?, auth, deps)
                .await
        }
        _ => Err(JsonRpcError::Internal(format!("Tool is not implemented: {name}")).into()),
    }?;
    if result.get("resultType").and_then(Value::as_str) == Some("task") {
        return Ok(result);
    }
    validate_tool_output(name, &result)?;
    Ok(result)
}

fn validate_tool_output(name: &str, result: &Value) -> Result<(), McpError> {
    let schema = tool_output_schema(name).map_err(|error| {
        JsonRpcError::Internal(format!("failed to load output schema for {name}: {error}"))
    })?;
    let Some(schema) = schema else {
        return Err(
            JsonRpcError::Internal(format!("missing output schema for tool: {name}")).into(),
        );
    };
    let structured = result.get("structuredContent").ok_or_else(|| {
        JsonRpcError::Internal(format!("tool {name} returned no structured content"))
    })?;
    let ordered: OrderedJson = serde_json::from_value(structured.clone()).map_err(|error| {
        JsonRpcError::Internal(format!(
            "tool {name} returned invalid structured content: {error}"
        ))
    })?;
    let errors = validate_schema_input(&schema, &ordered, &[]);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(JsonRpcError::Internal(format!(
            "tool {name} output failed validation: {}",
            errors
                .into_iter()
                .map(|error| {
                    error.strip_prefix("arguments").map_or_else(
                        || format!("structuredContent.{error}"),
                        |suffix| format!("structuredContent{suffix}"),
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        ))
        .into())
    }
}

/// Resolve a frozen definition and run the exact contract validator without executing a tool.
pub fn validate_tool_call(params: Option<&OrderedJson>) -> Result<ValidatedToolCall, McpError> {
    validate_tool_call_for_client(params, false, false)
}

fn validate_tool_call_for_client(
    params: Option<&OrderedJson>,
    modern: bool,
    allow_app_tools: bool,
) -> Result<ValidatedToolCall, McpError> {
    let name_value = params.and_then(|value| value.get("name"));
    let name = name_value.and_then(OrderedJson::as_str).ok_or_else(|| {
        JsonRpcError::InvalidParams(format!(
            "Unknown tool: {}",
            name_value.map_or_else(|| "undefined".to_owned(), javascript_string)
        ))
    })?;
    let definitions = if modern {
        modern_tool_definitions_for_client(allow_app_tools)
    } else {
        frozen_tool_definitions()
    }
    .map_err(|error| JsonRpcError::Internal(format!("failed to load tool definitions: {error}")))?;
    let definition = definitions
        .iter()
        .find(|definition| definition.name == name)
        .ok_or_else(|| JsonRpcError::InvalidParams(format!("Unknown tool: {name}")))?;
    let arguments = params
        .and_then(|value| value.get("arguments"))
        .cloned()
        .unwrap_or_else(|| OrderedJson::Object(Vec::new()));
    let errors = validate_schema_input(
        &definition.input_schema,
        &arguments,
        validation_property_order(name),
    );
    if !errors.is_empty() {
        return Err(JsonRpcError::InvalidParams(format!(
            "Invalid arguments: {}",
            errors.join("; ")
        ))
        .into());
    }

    Ok(ValidatedToolCall {
        name: name.to_owned(),
        arguments,
    })
}

async fn publish_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    ensure_can_publish(auth)?;
    let target_org = target_org(arguments, auth, deps).await?;
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

async fn publish_bundle(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    ensure_can_publish(auth)?;
    let target_org = target_org(arguments, auth, deps).await?;
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

async fn list_artifacts(auth: &PublisherIdentity, deps: &AppDeps) -> Result<Value, McpError> {
    let rows = if !auth.is_admin() && matches!(auth.role.as_str(), "reader" | "collaborator") {
        deps.artifacts.list_org_artifacts(&auth.org, true).await?
    } else {
        deps.artifacts.list_for_publisher(auth).await?
    };
    let count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let artifacts = rows
        .into_iter()
        .map(|row| {
            object! {
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
            }
        })
        .collect();
    tool_result(object! {
        "count" => OrderedJson::number_u64(count),
        "artifacts" => OrderedJson::Array(artifacts),
    })
}

async fn read_artifact(
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
            return tool_result(object! {
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
            });
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
        return tool_result(object! {
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
        });
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
    tool_result(object! {
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
    })
}

async fn delete_artifact(
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

async fn patch_artifact(
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
    tool_result(object! {
        "id" => OrderedJson::string(result.meta.id.0.clone()),
        "revision" => OrderedJson::number_u64(result.meta.revision),
        "bytes_before" => OrderedJson::number_u64(pre.bytes),
        "bytes_after" => OrderedJson::number_u64(result.meta.bytes),
        "edits_applied" => OrderedJson::number_u64(edit_count),
    })
}

async fn update_artifact(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let unavailable = "Artifact not found or you are not authorized to update it";
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
    let update = ArtifactUpdate {
        expected_revision,
        acting_client_id: Some(auth.client_id.clone()),
        title: optional_string(arguments, "title"),
        description: optional_string(arguments, "description"),
        category: optional_string(arguments, "category"),
        content,
    };
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
    tool_result(object! {
        "id" => OrderedJson::string(result.meta.id.0.clone()),
        "url" => OrderedJson::string(artifact_url(deps, &result.meta.id)),
        "revision" => OrderedJson::number_u64(result.meta.revision),
        "bytes" => OrderedJson::number_u64(result.meta.bytes),
        "entry" => OrderedJson::string(result.meta.entry),
        "category" => OrderedJson::string(result.meta.category),
    })
}

async fn set_visibility(
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

async fn list_categories(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let org = category_org(arguments, auth)?;
    let categories = deps.admin.categories(&org).await?;
    tool_result(object! {
        "org" => OrderedJson::string(org.0),
        "categories" => OrderedJson::Array(categories.into_iter().map(OrderedJson::string).collect()),
    })
}

async fn set_category(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_write(id, auth, deps).await?;
    let org = owned.meta().org.clone();
    let category = required_string(arguments, "category")?.to_owned();
    let registered_category = normalize_category(&category);
    let audit = MutationAudit::publisher(auth)?;
    // The artifact and organization ports own separate transactions. Make the audited registry
    // write a prerequisite so a failed audit key/write cannot report a successful category
    // mutation while leaving Settings without the category.
    if !registered_category.is_empty() {
        deps.admin
            .add_category(&org, &registered_category, audit.clone())
            .await?;
    }
    let result = deps
        .artifacts
        .set_category(owned.into_authorized(), category, audit)
        .await?;
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "category" => OrderedJson::string(result.category),
    })
}

async fn create_category(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let org = category_org(arguments, auth)?;
    let name = deps
        .admin
        .add_category(
            &org,
            required_string(arguments, "name")?,
            MutationAudit::publisher(auth)?,
        )
        .await?;
    tool_result(object! {
        "org" => OrderedJson::string(org.0),
        "name" => OrderedJson::string(name),
    })
}

async fn delete_category(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let org = category_org(arguments, auth)?;
    let name = required_string(arguments, "name")?;
    let removed = deps
        .admin
        .remove_category(&org, name, MutationAudit::publisher(auth)?)
        .await?;
    tool_result(object! {
        "org" => OrderedJson::string(org.0),
        "name" => OrderedJson::string(name),
        "removed" => OrderedJson::Bool(removed),
    })
}

async fn list_revisions(
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

async fn create_share(
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

async fn list_shares(
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

async fn revoke_share(
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

async fn artifact_stats(
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

async fn restore_artifact(
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

async fn list_feedback(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    if let Some(id) = arguments.get("id").and_then(OrderedJson::as_str)
        && !id.is_empty()
    {
        let owned = publisher_read(id, auth, deps).await?;
        let items = deps
            .engagement
            .list_feedback(&owned.into_authorized())
            .await?;
        let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
        return tool_result(object! {
            "artifact_id" => OrderedJson::string(id),
            "count" => OrderedJson::number_u64(count),
            "feedback" => OrderedJson::Array(items.iter().map(feedback_json).collect()),
        });
    }
    let items = deps
        .engagement
        .list_feedback_for_publisher(auth, None)
        .await?;
    let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
    tool_result(object! {
        "count" => OrderedJson::number_u64(count),
        "feedback" => OrderedJson::Array(items.iter().map(feedback_json).collect()),
    })
}

async fn submit_feedback(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "id")?;
    let body = required_string(arguments, "body")?;
    let owned = publisher_read(id, auth, deps).await?;
    let viewer_email = EmailAddress::from(format!("agent:{}", auth.client_id));
    let created = deps
        .engagement
        .submit_feedback(
            owned.into_authorized(),
            SubmitFeedback {
                viewer_email,
                body: body.to_owned(),
                parent_id: None,
                anchor: None,
                anchor_path: None,
                anchor_page: None,
            },
        )
        .await?;
    deps.delivery_wake.wake();
    tool_result(object! {
        "feedback_id" => OrderedJson::string(created.id.0),
        "artifact_id" => OrderedJson::string(id),
        "revision" => OrderedJson::number_u64(created.artifact_revision),
        "submitted" => OrderedJson::Bool(true),
    })
}

async fn resolve_feedback(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "feedback_id")?;
    let feedback_id = crate::model::FeedbackId::from(id);
    let Some(reference) = deps.engagement.feedback_ref(&feedback_id).await? else {
        return Err(AppError::NotFound(format!("Unknown feedback: {id}")).into());
    };
    let meta = deps.artifacts.find_meta(&reference.artifact_id).await?;
    let owned = AccessPolicy::authorize_publisher_write(auth, meta, &reference.artifact_id.0, "")
        .map_err(|denied| match denied {
        AppError::NotFound(_) => AppError::NotFound(format!("Unknown feedback: {id}")),
        other => other,
    })?;
    let resolver = format!("agent:{}", auth.client_id);
    let resolved = deps
        .engagement
        .resolve_feedback_as_publisher(owned, feedback_id, resolver)
        .await?;
    if resolved {
        deps.delivery_wake.wake();
    }
    tool_result(object! {
        "feedback_id" => OrderedJson::string(id),
        "resolved" => OrderedJson::Bool(resolved),
    })
}

async fn reopen_feedback(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = nonempty_protocol_string(arguments, "feedback_id")?;
    let feedback_id = crate::model::FeedbackId::from(id);
    let Some(reference) = deps.engagement.feedback_ref(&feedback_id).await? else {
        return Err(AppError::NotFound(format!("Unknown feedback: {id}")).into());
    };
    let meta = deps.artifacts.find_meta(&reference.artifact_id).await?;
    let owned = AccessPolicy::authorize_publisher_write(auth, meta, &reference.artifact_id.0, "")
        .map_err(|denied| match denied {
        AppError::NotFound(_) => AppError::NotFound(format!("Unknown feedback: {id}")),
        other => other,
    })?;
    let reopened = deps
        .engagement
        .reopen_feedback_as_publisher(owned, feedback_id)
        .await?;
    tool_result(object! {
        "feedback_id" => OrderedJson::string(id),
        "reopened" => OrderedJson::Bool(reopened),
    })
}

async fn publisher_read(
    id: &str,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OwnedArtifact, McpError> {
    let meta = deps.artifacts.find_meta(&ArtifactId::from(id)).await?;
    Ok(AccessPolicy::authorize_publisher_read(auth, meta, id)?)
}

async fn publisher_write(
    id: &str,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OwnedArtifact, McpError> {
    let meta = deps.artifacts.find_meta(&ArtifactId::from(id)).await?;
    Ok(AccessPolicy::authorize_publisher_write(auth, meta, id, "")?)
}

async fn publisher_delete(
    id: &str,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OwnedArtifact, McpError> {
    let meta = deps.artifacts.find_meta(&ArtifactId::from(id)).await?;
    Ok(AccessPolicy::authorize_publisher_delete(auth, meta, id)?)
}

fn ensure_can_publish(auth: &PublisherIdentity) -> Result<(), McpError> {
    if !auth.is_admin() && auth.role == "reader" {
        Err(AppError::Forbidden(PUBLISH_PERMISSION_ERROR.to_owned()).into())
    } else {
        Ok(())
    }
}

fn required_string<'a>(arguments: &'a OrderedJson, key: &str) -> Result<&'a str, McpError> {
    arguments
        .get(key)
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| JsonRpcError::InvalidParams(format!("{key} is required")).into())
}

fn nonempty_protocol_string<'a>(
    arguments: &'a OrderedJson,
    key: &str,
) -> Result<&'a str, McpError> {
    let value = required_string(arguments, key)?;
    if value.is_empty() {
        Err(JsonRpcError::InvalidParams(format!("{key} is required")).into())
    } else {
        Ok(value)
    }
}

fn optional_string(arguments: &OrderedJson, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(OrderedJson::as_str)
        .map(ToOwned::to_owned)
}

fn bundle_files(value: Option<&OrderedJson>) -> Result<Vec<(String, String)>, McpError> {
    let Some(value) = value else {
        return Err(JsonRpcError::InvalidParams("files is required".to_owned()).into());
    };
    let mut files = Vec::new();
    for (path, content) in value.object_entries() {
        let Some(content) = content.as_str() else {
            return Err(
                JsonRpcError::InvalidParams(format!("files.{path} must be a string")).into(),
            );
        };
        files.push((path.to_owned(), content.to_owned()));
    }
    Ok(files)
}

async fn target_org(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OrgId, McpError> {
    if AccessPolicy::publisher_is_admin(auth)
        && let Some(org) = arguments.get("org").and_then(OrderedJson::as_str)
    {
        let normalized = js_trim(org).to_lowercase();
        if !normalized.is_empty() {
            let target = OrgId::from(normalized.clone());
            if !is_valid_publish_org(&normalized) || !deps.admin.org_exists(&target).await? {
                return Err(AppError::Validation(format!(
                    "Unknown organization \"{normalized}\". Create it in the Organizations section first."
                ))
                .into());
            }
            return Ok(target);
        }
    }
    Ok(auth.org.clone())
}

fn is_valid_publish_org(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.clone().count() < 41
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn category_org(arguments: &OrderedJson, auth: &PublisherIdentity) -> Result<OrgId, McpError> {
    if !AccessPolicy::publisher_is_admin(auth) {
        return Ok(auth.org.clone());
    }
    let org = arguments
        .get("org")
        .and_then(OrderedJson::as_str)
        .map(js_trim)
        .unwrap_or_default();
    if org.is_empty() {
        Err(AppError::Validation("org is required for admin keys".to_owned()).into())
    } else {
        Ok(OrgId::from(org))
    }
}

fn positive_integer(value: &OrderedJson, name: &str) -> Result<u64, McpError> {
    let valid = value
        .as_number()
        .and_then(serde_json::Number::as_f64)
        .filter(|number| number.is_finite() && *number >= 1.0 && number.fract() == 0.0);
    let Some(number) = valid else {
        return Err(
            JsonRpcError::InvalidParams(format!("{name} must be a positive integer")).into(),
        );
    };
    Ok(if number >= u64::MAX as f64 {
        u64::MAX
    } else {
        number as u64
    })
}

fn non_negative_integer(value: &OrderedJson, name: &str) -> Result<u64, McpError> {
    let valid = value
        .as_number()
        .and_then(serde_json::Number::as_f64)
        .filter(|number| {
            number.is_finite()
                && *number >= 0.0
                && *number <= 9_007_199_254_740_991.0
                && number.fract() == 0.0
        });
    let Some(number) = valid else {
        return Err(
            JsonRpcError::InvalidParams(format!("{name} must be a non-negative integer")).into(),
        );
    };
    Ok(if number >= u64::MAX as f64 {
        u64::MAX
    } else {
        number as u64
    })
}

fn safe_positive_integer(value: &OrderedJson, name: &str) -> Result<u64, McpError> {
    let valid = value
        .as_number()
        .and_then(serde_json::Number::as_f64)
        .filter(|number| {
            number.is_finite()
                && *number >= 1.0
                && *number <= 9_007_199_254_740_991.0
                && number.fract() == 0.0
        });
    let Some(number) = valid else {
        return Err(
            JsonRpcError::InvalidParams(format!("{name} must be a positive integer")).into(),
        );
    };
    Ok(number as u64)
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

fn tool_result(payload: OrderedJson) -> Result<Value, McpError> {
    let text = payload.to_json_string().map_err(|error| {
        JsonRpcError::Internal(format!("failed to serialize tool result: {error}"))
    })?;
    let structured = payload.into_value();
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured
    }))
}

fn revision_json(revision: &ArtifactRevision) -> OrderedJson {
    object! {
        "revision" => OrderedJson::number_u64(revision.revision),
        "title" => OrderedJson::string(revision.title.clone()),
        "description" => OrderedJson::string(revision.description.clone()),
        "category" => OrderedJson::string(revision.category.clone()),
        "bytes" => OrderedJson::number_u64(revision.bytes),
        "is_bundle" => OrderedJson::number_i64(i64::from(revision.is_bundle)),
        "entry" => OrderedJson::string(revision.entry.clone()),
        "body_sha256" => OrderedJson::string(revision.body_sha256.clone()),
        "created_at" => OrderedJson::string(revision.created_at.0.clone()),
        "client_id" => optional_string_value(revision.client_id.as_ref().map(|id| id.0.clone())),
    }
}

fn share_json(share: PublicShare) -> OrderedJson {
    object! {
        "token" => OrderedJson::string(share.token.0),
        "expires_at" => optional_timestamp(share.expires_at),
        "created_at" => optional_timestamp(share.created_at),
        "created_by" => optional_string_value(share.created_by),
    }
}

fn viewer_json(viewer: &ViewerView) -> OrderedJson {
    object! {
        "email" => OrderedJson::string(viewer.email.0.clone()),
        "count" => OrderedJson::number_u64(viewer.count),
        "first_viewed_at" => OrderedJson::string(viewer.first_viewed_at.0.clone()),
        "last_viewed_at" => OrderedJson::string(viewer.last_viewed_at.0.clone()),
    }
}

fn feedback_json(feedback: &Feedback) -> OrderedJson {
    let author = match &feedback.author {
        crate::model::FeedbackAuthor::Artifact { viewer_email } => object! {
            "source" => OrderedJson::string("artifact"),
            "viewer_email" => OrderedJson::string(viewer_email.0.clone()),
        },
        crate::model::FeedbackAuthor::Discord {
            external_author_id,
            external_author_display,
        } => object! {
            "source" => OrderedJson::string("discord"),
            "external_author_id" => OrderedJson::string(external_author_id.clone()),
            "external_author_display" => OrderedJson::string(external_author_display.clone()),
        },
    };
    object! {
        "id" => OrderedJson::string(feedback.id.0.clone()),
        "artifact_id" => OrderedJson::string(feedback.artifact_id.0.clone()),
        "parent_id" => optional_id(feedback.parent_id.as_ref().map(|id| id.0.as_str())),
        "viewer_email" => optional_string_value(
            feedback.viewer_email.as_ref().map(|email| email.0.clone())
        ),
        "author" => author,
        "body" => OrderedJson::string(feedback.body.clone()),
        "artifact_revision" => OrderedJson::number_u64(feedback.artifact_revision),
        "anchor_path" => optional_id(feedback.anchor_path.as_deref()),
        "anchor_x" => optional_number(feedback.anchor_x),
        "anchor_y" => optional_number(feedback.anchor_y),
        "anchor_w" => optional_number(feedback.anchor_w),
        "anchor_h" => optional_number(feedback.anchor_h),
        "anchor_approx" => OrderedJson::number_i64(i64::from(feedback.anchor_approx)),
        "anchor_page" => optional_id(feedback.anchor_page.as_deref()),
        "created_at" => OrderedJson::string(feedback.created_at.0.clone()),
        "resolved_at" => optional_timestamp(feedback.resolved_at.clone()),
        "resolved_by" => optional_string_value(feedback.resolved_by.clone()),
    }
}

fn optional_timestamp(value: Option<Timestamp>) -> OrderedJson {
    value.map_or(OrderedJson::Null, |timestamp| {
        OrderedJson::string(timestamp.0)
    })
}

fn optional_string_value(value: Option<String>) -> OrderedJson {
    value.map_or(OrderedJson::Null, OrderedJson::string)
}

fn optional_id(value: Option<&str>) -> OrderedJson {
    value.map_or(OrderedJson::Null, OrderedJson::string)
}

fn optional_number(value: Option<f64>) -> OrderedJson {
    value.map_or(OrderedJson::Null, OrderedJson::number_f64)
}

fn artifact_url(deps: &AppDeps, id: &ArtifactId) -> String {
    format!("{}/{id}", deps.config.public_base_url)
}

fn javascript_truthy(value: &OrderedJson) -> bool {
    match value {
        OrderedJson::Null => false,
        OrderedJson::Bool(value) => *value,
        OrderedJson::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        OrderedJson::String(value) => !value.is_empty(),
        OrderedJson::Array(_) | OrderedJson::Object(_) => true,
    }
}

fn javascript_string(value: &OrderedJson) -> String {
    match value {
        OrderedJson::Null => "null".to_owned(),
        OrderedJson::Bool(value) => value.to_string(),
        OrderedJson::Number(value) => OrderedJson::Number(value.clone())
            .to_json_string()
            .unwrap_or_else(|_| "NaN".to_owned()),
        OrderedJson::String(value) => value.clone(),
        OrderedJson::Array(values) => values
            .iter()
            .map(|value| match value {
                OrderedJson::Null => String::new(),
                other => javascript_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        OrderedJson::Object(_) => "[object Object]".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_validation_rejects_a_malformed_structured_result() {
        let valid = json!({
            "content": [{ "type": "text", "text": "{}" }],
            "structuredContent": { "count": 0, "artifacts": [] }
        });
        validate_tool_output("list_artifacts", &valid).expect("valid output");

        let invalid = json!({
            "content": [{ "type": "text", "text": "{}" }],
            "structuredContent": { "count": "zero", "artifacts": [] }
        });
        let error = validate_tool_output("list_artifacts", &invalid)
            .expect_err("invalid output must fail closed");
        assert!(matches!(
            error,
            McpError::Protocol(JsonRpcError::Internal(message))
                if message.contains("structuredContent.count must be an integer")
        ));
    }
}
