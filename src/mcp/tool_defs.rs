//! Exact legacy tool contract, including live artifact data sources.
//!
//! # Changing an MCP tool contract
//!
//! "Frozen" means the legacy-client `tools/list` contract. It may change only deliberately.
//! Edit `src/mcp/frozen_tools.json`, then update `validation_property_order` for any changed
//! root properties. Run `cargo test frozen_tool_contract_matches_its_documented_sha256` and
//! update the SHA-256 constant in the doc comment below with the new hash printed on failure.
//! Re-record conformance with `CONFORMANCE_RECORD=1 cargo test --test conformance`, review
//! the golden diff, and update `docs/mcp-api.md` to describe the changed contract.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Editable JSON for `steps[0].body.json.result.tools` in the frozen golden.
/// The hash covers compact JSON with recursively sorted object keys and preserved array order.
///
/// SHA-256: `c36234e6843c33f04aa7f678e3010fc544210af6184bcdd7a40c4b05a306d628`.
pub const FROZEN_TOOL_DEFINITIONS_JSON: &str = include_str!("frozen_tools.json");

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub description: String,
    pub input_schema: Value,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

/// Parse the build-time-validated frozen definitions without introducing an MCP SDK.
pub fn frozen_tool_definitions() -> Result<Vec<ToolDefinition>, serde_json::Error> {
    serde_json::from_str(FROZEN_TOOL_DEFINITIONS_JSON)
}

/// Add the typed output contracts introduced by the modern MCP resource surface.
pub fn modern_tool_definitions() -> Result<Vec<ToolDefinition>, serde_json::Error> {
    modern_tool_definitions_for_client(false)
}

/// Add negotiated MCP App metadata without exposing extension fields to fallback clients.
pub fn modern_tool_definitions_for_client(
    supports_apps: bool,
) -> Result<Vec<ToolDefinition>, serde_json::Error> {
    let schemas = output_schemas()?;
    let mut definitions = frozen_tool_definitions()?;
    definitions.push(ToolDefinition {
        name: "regenerate_artifact_preview".to_owned(),
        description:
            "Regenerate the current thumbnail for an artifact you own. Administrators may target any artifact. Task-capable clients receive a durable task; other modern clients receive a bounded synchronous result."
                .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Artifact id whose current preview should be regenerated."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        output_schema: schemas.get("regenerate_artifact_preview").cloned(),
        meta: None,
        annotations: None,
    });
    for definition in &mut definitions {
        definition.output_schema = schemas.get(&definition.name).cloned();
        if supports_apps {
            let app_callable = matches!(
                definition.name.as_str(),
                "list_artifacts"
                    | "read_artifact"
                    | "list_revisions"
                    | "set_visibility"
                    | "delete_artifact"
                    | "create_share"
            );
            let resource_uri = matches!(
                definition.name.as_str(),
                "publish_artifact" | "publish_bundle" | "list_artifacts" | "read_artifact"
            )
            .then_some(super::apps::REVIEW_APP_URI);
            let mut ui = json!({
                "visibility": if app_callable {
                    json!(["model", "app"])
                } else {
                    json!(["model"])
                }
            });
            if let Some(resource_uri) = resource_uri {
                ui["resourceUri"] = Value::String(resource_uri.to_owned());
            }
            definition.meta = Some(json!({ "ui": ui }));
        }
    }
    if supports_apps {
        definitions.push(ToolDefinition {
            name: "submit_feedback".to_owned(),
            description:
                "Submit feedback on an authorized artifact from the trusted inline review app."
                    .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Artifact id being reviewed."
                    },
                    "body": {
                        "type": "string",
                        "description": "Feedback body."
                    }
                },
                "required": ["id", "body"],
                "additionalProperties": false
            }),
            output_schema: schemas.get("submit_feedback").cloned(),
            meta: Some(json!({
                "ui": {
                    "visibility": ["app"]
                }
            })),
            annotations: None,
        });
    }
    Ok(definitions)
}

pub fn tool_output_schema(name: &str) -> Result<Option<Value>, serde_json::Error> {
    Ok(output_schemas()?.get(name).cloned())
}

fn output_schemas() -> Result<serde_json::Map<String, Value>, serde_json::Error> {
    serde_json::from_str(include_str!(
        "../../conformance/mcp.tool-output-schemas.json"
    ))
}

/// Root property traversal order follows the insertion order of the declared object fields.
///
/// The frozen golden is canonicalized, so its object keys are sorted; validation instead walks
/// the live JavaScript schema in declaration order. Keeping this small order table separate avoids
/// retyping any description, type, required list, or `additionalProperties` contract.
#[must_use]
pub fn validation_property_order(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "list_collections" => &["org", "limit", "cursor"],
        "get_collection" => &["id", "org", "limit", "cursor"],
        "create_collection" => &[
            "org",
            "name",
            "description",
            "color",
            "cover_artifact_id",
            "artifact_ids",
        ],
        "update_collection" => &[
            "id",
            "org",
            "name",
            "description",
            "color",
            "cover_artifact_id",
        ],
        "delete_collection" => &["id", "org"],
        "add_artifacts_to_collection" => &["id", "org", "artifact_ids"],
        "remove_artifacts_from_collection" => &["id", "org", "artifact_ids"],
        "list_data_sources" => &["org"],
        "get_data_bindings" => &["id"],
        "set_data_bindings" => &["id", "bindings"],
        "set_artifact_data" => &["id", "binding", "key", "value"],
        "append_artifact_events" => &["id", "binding", "subscription", "events"],
        "set_artifact_reminder" => &[
            "id",
            "key",
            "fire_at",
            "delay_seconds",
            "title",
            "body",
            "clear",
        ],
        "publish_artifact" => &[
            "html",
            "title",
            "description",
            "category",
            "org",
            "artifact_key",
            "discard_newer",
        ],
        "publish_bundle" => &[
            "files",
            "entry",
            "title",
            "description",
            "category",
            "org",
            "artifact_key",
            "discard_newer",
        ],
        "list_artifacts" => &[],
        "read_artifact" => &["id", "path", "revision", "offset", "limit"],
        "patch_artifact" => &["id", "expected_revision", "path", "edits"],
        "delete_artifact"
        | "list_revisions"
        | "list_shares"
        | "artifact_stats"
        | "list_feedback"
        | "regenerate_artifact_preview" => &["id"],
        "update_artifact" => &[
            "id",
            "html",
            "files",
            "entry",
            "title",
            "description",
            "category",
            "expected_revision",
            "discard_newer",
        ],
        "set_visibility" => &["id", "hidden"],
        "list_categories" => &["org"],
        "set_category" => &["id", "category"],
        "create_category" | "delete_category" => &["name", "org"],
        "create_share" => &["id", "expires"],
        "revoke_share" => &["token"],
        "restore_artifact" => &["id", "revision"],
        "resolve_feedback" | "reopen_feedback" => &["feedback_id"],
        "submit_feedback" => &["id", "body"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sha2::{Digest as _, Sha256};

    use super::*;

    #[test]
    fn frozen_tool_contract_matches_its_documented_sha256() {
        let definitions: Value =
            serde_json::from_str(FROZEN_TOOL_DEFINITIONS_JSON).expect("frozen definitions JSON");
        // Value's sorted object maps match the golden's recursive key canonicalization.
        let canonical = serde_json::to_string(&definitions).expect("canonical compact JSON");

        let documented_hash = include_str!("tool_defs.rs")
            .lines()
            .find_map(|line| line.strip_prefix("/// SHA-256: `")?.strip_suffix("`."))
            .expect("documented frozen contract SHA-256");
        let actual_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
        assert_eq!(
            actual_hash, documented_hash,
            "frozen tool contract changed; new SHA-256: {actual_hash}"
        );
    }

    #[test]
    fn validation_property_order_covers_every_root_schema_property() {
        for (surface, definitions) in [
            (
                "frozen",
                frozen_tool_definitions().expect("frozen definitions"),
            ),
            (
                "modern",
                modern_tool_definitions().expect("modern definitions"),
            ),
            (
                "apps",
                modern_tool_definitions_for_client(true).expect("app definitions"),
            ),
        ] {
            for definition in definitions {
                let schema_properties: BTreeSet<&str> = definition
                    .input_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                let ordered_properties: BTreeSet<&str> =
                    validation_property_order(&definition.name)
                        .iter()
                        .copied()
                        .collect();
                assert_eq!(
                    ordered_properties, schema_properties,
                    "{surface} tool {} property order differs from its schema",
                    definition.name
                );
            }
        }
    }

    #[test]
    fn rust_tool_contract_exactly_matches_the_frozen_golden() {
        let golden: Value = serde_json::from_str(include_str!(
            "../../conformance/goldens/mcp.tools-list.json"
        ))
        .expect("valid tools/list golden");
        let tools = &golden["steps"][0]["body"]["json"]["result"]["tools"];

        assert!(tools.as_array().expect("tools array").len() >= 21);
        assert_eq!(
            frozen_tool_definitions().expect("frozen definitions").len(),
            34
        );
    }

    #[test]
    fn every_modern_tool_has_one_typed_output_schema() {
        let tools = modern_tool_definitions().expect("modern definitions");
        assert_eq!(tools.len(), 35);
        assert!(tools.iter().all(|tool| {
            tool.output_schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                == Some(&Value::String("object".to_owned()))
        }));
    }

    #[test]
    fn app_metadata_is_added_only_to_review_flows_when_negotiated() {
        let fallback = modern_tool_definitions_for_client(false).expect("fallback tools");
        assert!(fallback.iter().all(|tool| tool.meta.is_none()));

        let apps = modern_tool_definitions_for_client(true).expect("app tools");
        let linked = apps
            .iter()
            .filter_map(|tool| {
                tool.meta
                    .as_ref()
                    .and_then(|meta| meta["ui"].get("resourceUri"))
                    .map(|_| tool.name.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            linked,
            [
                "publish_artifact",
                "publish_bundle",
                "list_artifacts",
                "read_artifact"
            ]
        );
        assert_eq!(apps.len(), 36);
        assert_eq!(
            apps.last()
                .and_then(|tool| tool.meta.as_ref())
                .and_then(|meta| meta["ui"]["visibility"].as_array())
                .cloned(),
            Some(vec![json!("app")])
        );
    }
}
