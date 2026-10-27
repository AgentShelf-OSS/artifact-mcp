//! MCP method and tool dispatch over the frozen service traits.

use serde_json::{Value, json};

use crate::{
    AppDeps,
    error::{JsonRpcError, McpError},
    model::PublisherIdentity,
};

use super::{
    apps::client_supports_apps,
    args::required_string,
    decorate::{add_artifact_resource_link, add_review_app_data},
    protocol::OrderedJson,
    shape::{javascript_string, tool_result},
    tasks::{TASKS_EXTENSION, client_supports_tasks},
    tool_defs::{
        frozen_tool_definitions, modern_tool_definitions_for_client, tool_output_schema,
        validation_property_order,
    },
    tools::{
        append_artifact_events, artifact_stats, create_category, create_share, data_bindings,
        delete_artifact, delete_category, list_artifacts, list_categories, list_data_sources,
        list_feedback, list_revisions, list_shares, patch_artifact, publish_artifact,
        publish_bundle, read_artifact, reopen_feedback, resolve_feedback, restore_artifact,
        revoke_share, set_artifact_data, set_artifact_reminder, set_category, set_visibility,
        submit_feedback, update_artifact,
    },
    validation::validate_schema_input,
};

pub use super::tools::{ARTIFACT_KEY_INVALID, DISCARD_NEWER_INVALID, is_valid_artifact_key};

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
                .and_then(|params| params.get("protocolVersion"));
            if requested.is_some_and(|version| version.as_str().is_none()) {
                return Some(Err(JsonRpcError::InvalidParams(
                    "protocolVersion must be a string".to_owned(),
                )
                .into()));
            }
            Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
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

async fn collection_call(
    name: &str,
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let value = super::collections::call(name, arguments, auth, deps).await?;
    tool_result(super::collections::ordered_response(value))
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
        "list_data_sources" => list_data_sources(arguments, auth, deps).await,
        "get_data_bindings" => data_bindings(arguments, auth, deps, false).await,
        "set_data_bindings" => data_bindings(arguments, auth, deps, true).await,
        "set_artifact_data" => set_artifact_data(arguments, auth, deps).await,
        "append_artifact_events" => append_artifact_events(arguments, auth, deps).await,
        "set_artifact_reminder" => set_artifact_reminder(arguments, auth, deps).await,
        "publish_artifact" => publish_artifact(arguments, auth, deps).await,
        "publish_bundle" => publish_bundle(arguments, auth, deps).await,
        "list_artifacts" => list_artifacts(auth, deps).await,
        "list_collections" => collection_call(name, arguments, auth, deps).await,
        "get_collection" => collection_call(name, arguments, auth, deps).await,
        "create_collection" => collection_call(name, arguments, auth, deps).await,
        "update_collection" => collection_call(name, arguments, auth, deps).await,
        "delete_collection" => collection_call(name, arguments, auth, deps).await,
        "add_artifacts_to_collection" => collection_call(name, arguments, auth, deps).await,
        "remove_artifacts_from_collection" => collection_call(name, arguments, auth, deps).await,
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
