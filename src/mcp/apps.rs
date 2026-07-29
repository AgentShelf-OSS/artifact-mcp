//! Negotiated MCP App support for the trusted artifact review surface.

use serde_json::{Value, json};

use super::protocol::OrderedJson;

pub const MCP_APPS_EXTENSION: &str = "io.modelcontextprotocol/ui";
pub const MCP_APP_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub const REVIEW_APP_URI: &str = "ui://artifact-mcp/review";
pub const REVIEW_APP_VERSION: &str = "1.0.0";

const REVIEW_APP_HTML: &str = include_str!("../../assets/mcp-review-app.html");

#[must_use]
pub fn client_supports_apps(message: &OrderedJson) -> bool {
    message
        .get("params")
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
        .and_then(|capabilities| capabilities.get("extensions"))
        .and_then(|extensions| extensions.get(MCP_APPS_EXTENSION))
        .and_then(|extension| extension.get("mimeTypes"))
        .and_then(OrderedJson::as_array)
        .is_some_and(|mime_types| {
            mime_types
                .iter()
                .any(|mime_type| mime_type.as_str() == Some(MCP_APP_MIME_TYPE))
        })
}

#[must_use]
pub fn resource_descriptor() -> Value {
    json!({
        "uri": REVIEW_APP_URI,
        "name": "Artifact review",
        "title": "Artifact review",
        "description": "Trusted inline metadata and thumbnail review for authorized artifacts.",
        "mimeType": MCP_APP_MIME_TYPE,
        "_meta": {
            "com.agentshelf.artifact-mcp/appVersion": REVIEW_APP_VERSION,
            "ui": {
                "csp": {
                    "connectDomains": [],
                    "resourceDomains": [],
                    "frameDomains": [],
                    "baseUriDomains": []
                },
                "prefersBorder": true
            }
        }
    })
}

#[must_use]
pub fn resource_result() -> Value {
    json!({
        "contents": [{
            "uri": REVIEW_APP_URI,
            "mimeType": MCP_APP_MIME_TYPE,
            "text": REVIEW_APP_HTML,
            "_meta": {
                "com.agentshelf.artifact-mcp/appVersion": REVIEW_APP_VERSION,
                "ui": {
                    "csp": {
                        "connectDomains": [],
                        "resourceDomains": [],
                        "frameDomains": [],
                        "baseUriDomains": []
                    },
                    "prefersBorder": true
                }
            }
        }],
        "ttlMs": 3_600_000,
        "cacheScope": "public"
    })
}

#[must_use]
pub fn tool_ui_meta() -> Value {
    json!({
        "ui": {
            "resourceUri": REVIEW_APP_URI,
            "visibility": ["model", "app"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_resource_is_self_contained_and_deny_by_default() {
        let result = resource_result();
        let content = &result["contents"][0];
        assert_eq!(content["mimeType"], MCP_APP_MIME_TYPE);
        assert!(content["text"].as_str().is_some_and(|html| {
            html.starts_with("<!doctype html>")
                && !html.contains("<iframe")
                && !html.contains(".innerHTML")
        }));
        assert_eq!(
            content["_meta"]["ui"]["csp"],
            json!({
                "connectDomains": [],
                "resourceDomains": [],
                "frameDomains": [],
                "baseUriDomains": []
            })
        );
        assert!(content["_meta"]["ui"].get("permissions").is_none());
    }
}
