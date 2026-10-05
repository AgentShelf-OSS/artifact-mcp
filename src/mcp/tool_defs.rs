//! Exact legacy tool contract, including live artifact data sources.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Canonical compact JSON for `steps[0].body.json.result.tools` in the frozen golden.
///
/// SHA-256: `9c8fe17d72d1ebf2fbec24ad23301f1f430b0ebf29f6a68b99f682b44f81d3f9`.
pub const FROZEN_TOOL_DEFINITIONS_JSON: &str = r#"[{"description":"List configured live data sources and their public operation/subscription schemas.","inputSchema":{"additionalProperties":false,"properties":{"org":{"type":"string"}},"type":"object"},"name":"list_data_sources"},{"description":"Read the live data bindings attached to an artifact.","inputSchema":{"additionalProperties":false,"properties":{"id":{"type":"string"}},"required":["id"],"type":"object"},"name":"get_data_bindings"},{"description":"Replace the complete live data binding manifest for an artifact.","inputSchema":{"additionalProperties":false,"properties":{"bindings":{"additionalProperties":true,"type":"object"},"id":{"type":"string"}},"required":["id","bindings"],"type":"object"},"name":"set_data_bindings"},{"description":"Set a push-source snapshot value for a bound artifact.","inputSchema":{"additionalProperties":false,"properties":{"binding":{"type":"string"},"id":{"type":"string"},"key":{"type":"string"},"value":{}},"required":["id","binding","key","value"],"type":"object"},"name":"set_artifact_data"},{"description":"Append idempotent events to a push-source artifact subscription.","inputSchema":{"additionalProperties":false,"properties":{"binding":{"type":"string"},"events":{"items":{"additionalProperties":false,"properties":{"data":{},"event":{"type":"string"},"id":{"type":"string"}},"required":["id","event","data"],"type":"object"},"minItems":1,"type":"array"},"id":{"type":"string"},"subscription":{"type":"string"}},"required":["id","binding","subscription","events"],"type":"object"},"name":"append_artifact_events"},{"description":"Publish a self-contained HTML document. Returns a public URL that renders it at your configured domain, /<id>. Provide a title and a short description for the artifact index.","inputSchema":{"additionalProperties":false,"properties":{"category":{"description":"Optional category to group the artifact within its org (e.g. 'Dashboards'). Blank = Uncategorized.","type":"string"},"description":{"description":"One-line description shown next to the link on the index.","type":"string"},"html":{"description":"Full self-contained HTML document to host.","type":"string"},"org":{"description":"Target org (admin keys only; org keys are locked to their own org).","type":"string"},"title":{"description":"Short title shown on the artifact index.","type":"string"}},"required":["html"],"type":"object"},"name":"publish_artifact"},{"description":"Publish a multi-file artifact (e.g. several HTML pages that link to each other and a shared stylesheet). Provide files as a map of relative-path -> file contents; relative links between files resolve. Returns a public URL. Use this instead of publish_artifact when the HTML references other files like _shared.css or additional pages.","inputSchema":{"additionalProperties":false,"properties":{"category":{"description":"Optional category to group the artifact within its org. Blank = Uncategorized.","type":"string"},"description":{"description":"One-line description shown on the index.","type":"string"},"entry":{"description":"The HTML file to open first. Defaults to index.html, or the first .html file.","type":"string"},"files":{"additionalProperties":{"type":"string"},"description":"Map of relative path to file contents, e.g. {\"index.html\":\"...\",\"_shared.css\":\"...\"}. Paths are relative; no leading slash or '..'.","type":"object"},"org":{"description":"Target org (admin keys only).","type":"string"},"title":{"description":"Short title shown on the artifact index.","type":"string"}},"required":["files"],"type":"object"},"name":"publish_bundle"},{"description":"List artifacts available to this API key: organization-wide for reader/collaborator keys, own-only for author keys, with URLs and uploader labels.","inputSchema":{"additionalProperties":false,"properties":{},"type":"object"},"name":"list_artifacts"},{"description":"Delete one of your artifacts by id.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Artifact id to delete.","type":"string"}},"required":["id"],"type":"object"},"name":"delete_artifact"},{"description":"Replace an existing artifact's content and/or metadata in place, keeping the SAME id and URL so existing links keep working. Pass `html` for a single-file artifact or `files` for a bundle — the artifact type cannot change. Omitted title/description are preserved. Each effective change increments the artifact's revision.","inputSchema":{"additionalProperties":false,"properties":{"category":{"description":"New category (omit to keep current; empty string moves it to Uncategorized).","type":"string"},"description":{"description":"New description (omit to keep current; empty string clears it).","type":"string"},"entry":{"description":"Entry file for a bundle (defaults to the current entry, then index.html).","type":"string"},"expected_revision":{"description":"Optional current revision; the update is rejected if the artifact has changed.","type":"number"},"files":{"additionalProperties":{"type":"string"},"description":"New complete bundle snapshot (relative path -> content) for a bundle artifact; omitted files are removed.","type":"object"},"html":{"description":"New HTML for a single-file artifact.","type":"string"},"id":{"description":"Artifact id to update.","type":"string"},"title":{"description":"New title (omit to keep the current one).","type":"string"}},"required":["id"],"type":"object"},"name":"update_artifact"},{"description":"Unlist or relist one of your artifacts. Hidden artifacts remain accessible by direct URL to organization members; this is not access control.","inputSchema":{"additionalProperties":false,"properties":{"hidden":{"description":"True unlists it from the gallery; false relists it.","type":"boolean"},"id":{"description":"Artifact id.","type":"string"}},"required":["id","hidden"],"type":"object"},"name":"set_visibility"},{"description":"List the categories registered for your organization (used to group artifacts in the gallery). Admin keys may pass an org.","inputSchema":{"additionalProperties":false,"properties":{"org":{"description":"Org to list (admin keys only; defaults to your org).","type":"string"}},"type":"object"},"name":"list_categories"},{"description":"Move one of your artifacts into a category (empty string = Uncategorized). Also adds the category to your org's list so it appears in the picker. Does NOT create a new revision.","inputSchema":{"additionalProperties":false,"properties":{"category":{"description":"Target category; empty string moves it to Uncategorized.","type":"string"},"id":{"description":"Artifact id.","type":"string"}},"required":["id","category"],"type":"object"},"name":"set_category"},{"description":"Add a category to your organization's category list. Admin keys may pass an org.","inputSchema":{"additionalProperties":false,"properties":{"name":{"description":"Category name.","type":"string"},"org":{"description":"Org (admin keys only; defaults to your org).","type":"string"}},"required":["name"],"type":"object"},"name":"create_category"},{"description":"Remove a category from your organization's category list. Artifacts already tagged with it keep their tag. Admin keys may pass an org.","inputSchema":{"additionalProperties":false,"properties":{"name":{"description":"Category name to remove.","type":"string"},"org":{"description":"Org (admin keys only; defaults to your org).","type":"string"}},"required":["name"],"type":"object"},"name":"delete_category"},{"description":"List the version history of one of your artifacts — each retained revision's number, title, size, and timestamp. Use with restore_artifact to roll back.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Artifact id.","type":"string"}},"required":["id"],"type":"object"},"name":"list_revisions"},{"description":"Create an unlisted public, read-only share link for one of your artifacts. It serves the live artifact until it expires or is revoked.","inputSchema":{"additionalProperties":false,"properties":{"expires":{"description":"'24h', 'never', or a future ISO date.","type":"string"},"id":{"description":"Artifact id.","type":"string"}},"required":["id","expires"],"type":"object"},"name":"create_share"},{"description":"List active public share links for one of your artifacts.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Artifact id.","type":"string"}},"required":["id"],"type":"object"},"name":"list_shares"},{"description":"Revoke an active public share link you own. Revocation takes effect immediately.","inputSchema":{"additionalProperties":false,"properties":{"token":{"description":"Share token returned by create_share or list_shares.","type":"string"}},"required":["token"],"type":"object"},"name":"revoke_share"},{"description":"Get named audience-view analytics for one of your artifacts: total views, unique viewers, last viewed time, and each viewer's count and timestamps.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Artifact id.","type":"string"}},"required":["id"],"type":"object"},"name":"artifact_stats"},{"description":"Restore a past revision of your artifact by number. Its content is re-published as a NEW revision at the same id/URL, so nothing is lost and the restore is itself undoable. Get revision numbers from list_revisions.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Artifact id.","type":"string"},"revision":{"description":"Revision number to restore (from list_revisions).","type":"number"}},"required":["id","revision"],"type":"object"},"name":"restore_artifact"},{"description":"List viewer feedback left on your artifacts. Pass an artifact id to scope to one; omit to list across all of your artifacts.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Optional artifact id to scope the feedback to.","type":"string"}},"type":"object"},"name":"list_feedback"},{"description":"Mark a piece of viewer feedback as resolved once you've addressed it.","inputSchema":{"additionalProperties":false,"properties":{"feedback_id":{"description":"Feedback id to resolve.","type":"string"}},"required":["feedback_id"],"type":"object"},"name":"resolve_feedback"},{"description":"Reopen previously resolved viewer feedback when more work is needed.","inputSchema":{"additionalProperties":false,"properties":{"feedback_id":{"description":"Feedback id to reopen.","type":"string"}},"required":["feedback_id"],"type":"object"},"name":"reopen_feedback"},{"description":"Read an artifact or retained revision with byte-bounded UTF-8 paging. A bundle without path returns its file listing; pass path to read one bundle file.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Artifact id to read.","type":"string"},"limit":{"description":"Maximum UTF-8 bytes to return; defaults to 65536.","type":"integer"},"offset":{"description":"UTF-8 byte offset; defaults to 0.","type":"integer"},"path":{"description":"Bundle file path. Omit to list bundle files.","type":"string"},"revision":{"description":"Optional retained revision number; defaults to the current revision.","type":"integer"}},"required":["id"],"type":"object"},"name":"read_artifact"},{"description":"Apply an atomic batch of UTF-8 byte-safe partial edits to an artifact. Find edits must match exactly once; range offsets refer to the pre-edit content.","inputSchema":{"additionalProperties":false,"properties":{"edits":{"description":"Atomic edits, each using either find/replace or offset/length/replace.","items":{"additionalProperties":false,"properties":{"find":{"description":"Exact UTF-8 text to replace; must occur exactly once.","type":"string"},"length":{"description":"UTF-8 byte length in the pre-edit content.","type":"integer"},"offset":{"description":"UTF-8 byte offset in the pre-edit content.","type":"integer"},"replace":{"description":"Replacement text.","type":"string"}},"required":["replace"],"type":"object"},"minItems":1,"type":"array"},"expected_revision":{"description":"Required current revision; stale patches are rejected.","type":"integer"},"id":{"description":"Artifact id to patch.","type":"string"},"path":{"description":"Bundle file path. Required for bundle artifacts; omit for single-file artifacts.","type":"string"}},"required":["id","expected_revision","edits"],"type":"object"},"name":"patch_artifact"},{"annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true},"description":"List accessible organization collections, called folders in the library UI. Returns bounded summaries and a next-page cursor.","inputSchema":{"additionalProperties":false,"properties":{"cursor":{"description":"Opaque cursor from the previous page. Empty or omitted starts the first page.","maxLength":1024,"type":"string"},"limit":{"description":"Page size, default 25, maximum 100.","maximum":100,"minimum":1,"type":"integer"},"org":{"description":"Concrete registered organization. Required for admin credentials; other credentials are locked to their own organization.","type":"string"}},"required":[],"type":"object"},"name":"list_collections"},{"annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":true},"description":"Read a folder and a bounded page of readable artifact references. Folder membership never grants artifact access.","inputSchema":{"additionalProperties":false,"properties":{"cursor":{"description":"Opaque cursor from the previous page. Empty or omitted starts the first page.","maxLength":1024,"type":"string"},"id":{"description":"Collection ID returned by list_collections or create_collection.","maxLength":128,"minLength":1,"type":"string"},"limit":{"description":"Page size, default 25, maximum 100.","maximum":100,"minimum":1,"type":"integer"},"org":{"description":"Concrete registered organization. Required for admin credentials; other credentials are locked to their own organization.","type":"string"}},"required":["id"],"type":"object"},"name":"get_collection"},{"annotations":{"destructiveHint":false,"idempotentHint":false,"openWorldHint":false,"readOnlyHint":false},"description":"Create an organization folder referencing existing artifacts. Duplicate names return a conflict; creation never silently updates an existing folder.","inputSchema":{"additionalProperties":false,"properties":{"artifact_ids":{"description":"At most 100 readable artifact IDs from the same organization.","items":{"maxLength":128,"minLength":1,"type":"string"},"maxItems":100,"type":"array"},"color":{"description":"Six-digit hex color, or empty string to clear.","type":"string"},"cover_artifact_id":{"description":"Readable collection member ID, or empty string to clear the cover.","type":"string"},"description":{"description":"Empty string clears the description.","maxLength":500,"type":"string"},"name":{"maxLength":80,"minLength":1,"type":"string"},"org":{"description":"Concrete registered organization. Required for admin credentials; other credentials are locked to their own organization.","type":"string"}},"required":["name"],"type":"object"},"name":"create_collection"},{"annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Edit folder metadata. Omitted fields are preserved; empty description, color, or cover_artifact_id clears that field.","inputSchema":{"additionalProperties":false,"properties":{"color":{"description":"Six-digit hex color, or empty string to clear.","type":"string"},"cover_artifact_id":{"description":"Readable collection member ID, or empty string to clear the cover.","type":"string"},"description":{"description":"Empty string clears the description.","maxLength":500,"type":"string"},"id":{"description":"Collection ID returned by list_collections or create_collection.","maxLength":128,"minLength":1,"type":"string"},"name":{"maxLength":80,"minLength":1,"type":"string"},"org":{"description":"Concrete registered organization. Required for admin credentials; other credentials are locked to their own organization.","type":"string"}},"required":["id"],"type":"object"},"name":"update_collection"},{"annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Delete a folder and its membership links. All underlying artifacts, revisions, and links in other folders are preserved. Missing folders return Not found.","inputSchema":{"additionalProperties":false,"properties":{"id":{"description":"Collection ID returned by list_collections or create_collection.","maxLength":128,"minLength":1,"type":"string"},"org":{"description":"Concrete registered organization. Required for admin credentials; other credentials are locked to their own organization.","type":"string"}},"required":["id"],"type":"object"},"name":"delete_collection"},{"annotations":{"destructiveHint":false,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Add readable same-organization artifact references to a folder. Safe to retry; all batch targets validate before any write.","inputSchema":{"additionalProperties":false,"properties":{"artifact_ids":{"description":"At most 100 readable artifact IDs from the same organization.","items":{"maxLength":128,"minLength":1,"type":"string"},"maxItems":100,"type":"array"},"id":{"description":"Collection ID returned by list_collections or create_collection.","maxLength":128,"minLength":1,"type":"string"},"org":{"description":"Concrete registered organization. Required for admin credentials; other credentials are locked to their own organization.","type":"string"}},"required":["id","artifact_ids"],"type":"object"},"name":"add_artifacts_to_collection"},{"annotations":{"destructiveHint":true,"idempotentHint":true,"openWorldHint":false,"readOnlyHint":false},"description":"Remove artifact references from a folder, preserving artifacts and other folders. Safe to retry; reports already-absent references.","inputSchema":{"additionalProperties":false,"properties":{"artifact_ids":{"description":"At most 100 readable artifact IDs from the same organization.","items":{"maxLength":128,"minLength":1,"type":"string"},"maxItems":100,"type":"array"},"id":{"description":"Collection ID returned by list_collections or create_collection.","maxLength":128,"minLength":1,"type":"string"},"org":{"description":"Concrete registered organization. Required for admin credentials; other credentials are locked to their own organization.","type":"string"}},"required":["id","artifact_ids"],"type":"object"},"name":"remove_artifacts_from_collection"}]"#;

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

/// Root property traversal order from the Node object literals in `lib/mcp.js`.
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
            33
        );
    }

    #[test]
    fn every_modern_tool_has_one_typed_output_schema() {
        let tools = modern_tool_definitions().expect("modern definitions");
        assert_eq!(tools.len(), 34);
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
        assert_eq!(apps.len(), 35);
        assert_eq!(
            apps.last()
                .and_then(|tool| tool.meta.as_ref())
                .and_then(|meta| meta["ui"]["visibility"].as_array())
                .cloned(),
            Some(vec![json!("app")])
        );
    }
}
