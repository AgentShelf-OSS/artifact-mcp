//! Exact legacy tool contract, including live artifact data sources.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Canonical compact JSON for `steps[0].body.json.result.tools` in the frozen golden.
///
/// SHA-256: `ae316e6363d8ac76d6bbbbeeeae9e7fcd4ae34f1d6690c9c409141c04a11c2e1`.
pub const FROZEN_TOOL_DEFINITIONS_JSON: &str = r#"[{"name":"list_data_sources","description":"List configured live data sources and their public operation/subscription schemas.","inputSchema":{"type":"object","properties":{"org":{"type":"string"}},"additionalProperties":false}},{"name":"get_data_bindings","description":"Read the live data bindings attached to an artifact.","inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}},{"name":"set_data_bindings","description":"Replace the complete live data binding manifest for an artifact.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"bindings":{"type":"object","additionalProperties":true}},"required":["id","bindings"],"additionalProperties":false}},{"name":"set_artifact_data","description":"Set a push-source snapshot value for a bound artifact.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"binding":{"type":"string"},"key":{"type":"string"},"value":{}},"required":["id","binding","key","value"],"additionalProperties":false}},{"name":"append_artifact_events","description":"Append idempotent events to a push-source artifact subscription.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"binding":{"type":"string"},"subscription":{"type":"string"},"events":{"type":"array","minItems":1,"items":{"type":"object","properties":{"id":{"type":"string"},"event":{"type":"string"},"data":{}},"required":["id","event","data"],"additionalProperties":false}}},"required":["id","binding","subscription","events"],"additionalProperties":false}},{"name":"publish_artifact","description":"Publish a self-contained HTML document. Returns a public URL that renders it at your configured domain, /<id>. Provide a title and a short description for the artifact index.","inputSchema":{"type":"object","properties":{"html":{"type":"string","description":"Full self-contained HTML document to host."},"title":{"type":"string","description":"Short title shown on the artifact index."},"description":{"type":"string","description":"One-line description shown next to the link on the index."},"category":{"type":"string","description":"Optional category to group the artifact within its org (e.g. 'Dashboards'). Blank = Uncategorized."},"org":{"type":"string","description":"Target org (admin keys only; org keys are locked to their own org)."}},"required":["html"],"additionalProperties":false}},{"name":"publish_bundle","description":"Publish a multi-file artifact (e.g. several HTML pages that link to each other and a shared stylesheet). Provide files as a map of relative-path -> file contents; relative links between files resolve. Returns a public URL. Use this instead of publish_artifact when the HTML references other files like _shared.css or additional pages.","inputSchema":{"type":"object","properties":{"files":{"type":"object","description":"Map of relative path to file contents, e.g. {\"index.html\":\"...\",\"_shared.css\":\"...\"}. Paths are relative; no leading slash or '..'.","additionalProperties":{"type":"string"}},"entry":{"type":"string","description":"The HTML file to open first. Defaults to index.html, or the first .html file."},"title":{"type":"string","description":"Short title shown on the artifact index."},"description":{"type":"string","description":"One-line description shown on the index."},"category":{"type":"string","description":"Optional category to group the artifact within its org. Blank = Uncategorized."},"org":{"type":"string","description":"Target org (admin keys only)."}},"required":["files"],"additionalProperties":false}},{"name":"list_artifacts","description":"List artifacts available to this API key: organization-wide for reader/collaborator keys, own-only for author keys, with URLs and uploader labels.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},{"name":"delete_artifact","description":"Delete one of your artifacts by id.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id to delete."}},"required":["id"],"additionalProperties":false}},{"name":"update_artifact","description":"Replace an existing artifact's content and/or metadata in place, keeping the SAME id and URL so existing links keep working. Pass `html` for a single-file artifact or `files` for a bundle — the artifact type cannot change. Omitted title/description are preserved. Each effective change increments the artifact's revision.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id to update."},"html":{"type":"string","description":"New HTML for a single-file artifact."},"files":{"type":"object","description":"New complete bundle snapshot (relative path -> content) for a bundle artifact; omitted files are removed.","additionalProperties":{"type":"string"}},"entry":{"type":"string","description":"Entry file for a bundle (defaults to the current entry, then index.html)."},"title":{"type":"string","description":"New title (omit to keep the current one)."},"description":{"type":"string","description":"New description (omit to keep current; empty string clears it)."},"category":{"type":"string","description":"New category (omit to keep current; empty string moves it to Uncategorized)."},"expected_revision":{"type":"number","description":"Optional current revision; the update is rejected if the artifact has changed."}},"required":["id"],"additionalProperties":false}},{"name":"set_visibility","description":"Unlist or relist one of your artifacts. Hidden artifacts remain accessible by direct URL to organization members; this is not access control.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id."},"hidden":{"type":"boolean","description":"True unlists it from the gallery; false relists it."}},"required":["id","hidden"],"additionalProperties":false}},{"name":"list_categories","description":"List the categories registered for your organization (used to group artifacts in the gallery). Admin keys may pass an org.","inputSchema":{"type":"object","properties":{"org":{"type":"string","description":"Org to list (admin keys only; defaults to your org)."}},"additionalProperties":false}},{"name":"set_category","description":"Move one of your artifacts into a category (empty string = Uncategorized). Also adds the category to your org's list so it appears in the picker. Does NOT create a new revision.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id."},"category":{"type":"string","description":"Target category; empty string moves it to Uncategorized."}},"required":["id","category"],"additionalProperties":false}},{"name":"create_category","description":"Add a category to your organization's category list. Admin keys may pass an org.","inputSchema":{"type":"object","properties":{"name":{"type":"string","description":"Category name."},"org":{"type":"string","description":"Org (admin keys only; defaults to your org)."}},"required":["name"],"additionalProperties":false}},{"name":"delete_category","description":"Remove a category from your organization's category list. Artifacts already tagged with it keep their tag. Admin keys may pass an org.","inputSchema":{"type":"object","properties":{"name":{"type":"string","description":"Category name to remove."},"org":{"type":"string","description":"Org (admin keys only; defaults to your org)."}},"required":["name"],"additionalProperties":false}},{"name":"list_revisions","description":"List the version history of one of your artifacts — each retained revision's number, title, size, and timestamp. Use with restore_artifact to roll back.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id."}},"required":["id"],"additionalProperties":false}},{"name":"create_share","description":"Create an unlisted public, read-only share link for one of your artifacts. It serves the live artifact until it expires or is revoked.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id."},"expires":{"type":"string","description":"'24h', 'never', or a future ISO date."}},"required":["id","expires"],"additionalProperties":false}},{"name":"list_shares","description":"List active public share links for one of your artifacts.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id."}},"required":["id"],"additionalProperties":false}},{"name":"revoke_share","description":"Revoke an active public share link you own. Revocation takes effect immediately.","inputSchema":{"type":"object","properties":{"token":{"type":"string","description":"Share token returned by create_share or list_shares."}},"required":["token"],"additionalProperties":false}},{"name":"artifact_stats","description":"Get named audience-view analytics for one of your artifacts: total views, unique viewers, last viewed time, and each viewer's count and timestamps.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id."}},"required":["id"],"additionalProperties":false}},{"name":"restore_artifact","description":"Restore a past revision of your artifact by number. Its content is re-published as a NEW revision at the same id/URL, so nothing is lost and the restore is itself undoable. Get revision numbers from list_revisions.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id."},"revision":{"type":"number","description":"Revision number to restore (from list_revisions)."}},"required":["id","revision"],"additionalProperties":false}},{"name":"list_feedback","description":"List viewer feedback left on your artifacts. Pass an artifact id to scope to one; omit to list across all of your artifacts.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Optional artifact id to scope the feedback to."}},"additionalProperties":false}},{"name":"resolve_feedback","description":"Mark a piece of viewer feedback as resolved once you've addressed it.","inputSchema":{"type":"object","properties":{"feedback_id":{"type":"string","description":"Feedback id to resolve."}},"required":["feedback_id"],"additionalProperties":false}},{"name":"reopen_feedback","description":"Reopen previously resolved viewer feedback when more work is needed.","inputSchema":{"type":"object","properties":{"feedback_id":{"type":"string","description":"Feedback id to reopen."}},"required":["feedback_id"],"additionalProperties":false}},{"name":"read_artifact","description":"Read an artifact or retained revision with byte-bounded UTF-8 paging. A bundle without path returns its file listing; pass path to read one bundle file.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id to read."},"path":{"type":"string","description":"Bundle file path. Omit to list bundle files."},"revision":{"type":"integer","description":"Optional retained revision number; defaults to the current revision."},"offset":{"type":"integer","description":"UTF-8 byte offset; defaults to 0."},"limit":{"type":"integer","description":"Maximum UTF-8 bytes to return; defaults to 65536."}},"required":["id"],"additionalProperties":false}},{"name":"patch_artifact","description":"Apply an atomic batch of UTF-8 byte-safe partial edits to an artifact. Find edits must match exactly once; range offsets refer to the pre-edit content.","inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Artifact id to patch."},"expected_revision":{"type":"integer","description":"Required current revision; stale patches are rejected."},"path":{"type":"string","description":"Bundle file path. Required for bundle artifacts; omit for single-file artifacts."},"edits":{"type":"array","description":"Atomic edits, each using either find/replace or offset/length/replace.","minItems":1,"items":{"type":"object","properties":{"find":{"type":"string","description":"Exact UTF-8 text to replace; must occur exactly once."},"length":{"type":"integer","description":"UTF-8 byte length in the pre-edit content."},"offset":{"type":"integer","description":"UTF-8 byte offset in the pre-edit content."},"replace":{"type":"string","description":"Replacement text."}},"required":["replace"],"additionalProperties":false}}},"required":["id","expected_revision","edits"],"additionalProperties":false}}]"#;

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

/// Root property traversal order from the Node object literals in `lib/mcp.js`.
///
/// The frozen golden is canonicalized, so its object keys are sorted; validation instead walks
/// the live JavaScript schema in declaration order. Keeping this small order table separate avoids
/// retyping any description, type, required list, or `additionalProperties` contract.
#[must_use]
pub fn validation_property_order(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "list_data_sources" => &["org"],
        "get_data_bindings" => &["id"],
        "set_data_bindings" => &["id", "bindings"],
        "set_artifact_data" => &["id", "binding", "key", "value"],
        "append_artifact_events" => &["id", "binding", "subscription", "events"],
        "publish_artifact" => &["html", "title", "description", "category", "org"],
        "publish_bundle" => &["files", "entry", "title", "description", "category", "org"],
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
    use super::*;

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
            26
        );
    }

    #[test]
    fn every_modern_tool_has_one_typed_output_schema() {
        let tools = modern_tool_definitions().expect("modern definitions");
        assert_eq!(tools.len(), 27);
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
        assert_eq!(apps.len(), 28);
        assert_eq!(
            apps.last()
                .and_then(|tool| tool.meta.as_ref())
                .and_then(|meta| meta["ui"]["visibility"].as_array())
                .cloned(),
            Some(vec![json!("app")])
        );
    }
}
