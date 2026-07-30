//! Authorized artifact resources for the modern stateless MCP surface.

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};

use crate::{
    AppDeps,
    artifacts::validation::sanitize_relative_path,
    error::{AppError, JsonRpcError, McpError},
    model::{ArtifactFile, ArtifactId, ArtifactMeta, PublisherIdentity},
    security::access::AccessPolicy,
};

use super::{
    apps::{REVIEW_APP_URI, client_supports_apps, resource_descriptor, resource_result},
    protocol::OrderedJson,
};

const RESOURCE_PAGE_SIZE: usize = 50;
const RESOURCE_MAX_BYTES: usize = 1_048_576;
const RESOURCE_LIST_TTL_MS: u64 = 60_000;
const RESOURCE_READ_TTL_MS: u64 = 30_000;
const RESOURCE_TEMPLATES_TTL_MS: u64 = 300_000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResourceTarget {
    id: String,
    revision: Option<u64>,
    path: Option<String>,
    thumbnail: bool,
}

pub async fn dispatch(
    message: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let method = message
        .get("method")
        .and_then(OrderedJson::as_str)
        .ok_or(JsonRpcError::InvalidRequest)?;
    match method {
        "resources/list" => list_resources(message, auth, deps).await,
        "resources/templates/list" => Ok(list_templates()),
        "resources/read" => read_resource(message, auth, deps).await,
        _ => Err(JsonRpcError::MethodNotFound(method.to_owned()).into()),
    }
}

async fn list_resources(
    message: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let params = message.get("params");
    let offset = params
        .and_then(|params| params.get("cursor"))
        .map(parse_cursor)
        .transpose()?
        .unwrap_or(0);
    let rows = if !auth.is_admin() && matches!(auth.role.as_str(), "reader" | "collaborator") {
        deps.artifacts
            .list_org_artifacts(&auth.org, true)
            .await
            .map_err(resource_app_error)?
    } else {
        deps.artifacts
            .list_for_publisher(auth)
            .await
            .map_err(resource_app_error)?
    };
    if offset > rows.len() {
        return Err(JsonRpcError::InvalidParams("Invalid resource cursor".to_owned()).into());
    }
    let end = offset.saturating_add(RESOURCE_PAGE_SIZE).min(rows.len());
    let mut resources = rows[offset..end]
        .iter()
        .map(resource_metadata)
        .collect::<Vec<_>>();
    if offset == 0 && client_supports_apps(message) {
        resources.insert(0, resource_descriptor());
    }
    let mut result = json!({
        "resources": resources,
        "ttlMs": RESOURCE_LIST_TTL_MS,
        "cacheScope": "private"
    });
    if end < rows.len() {
        result["nextCursor"] = Value::String(encode_cursor(end));
    }
    Ok(result)
}

pub fn list_templates() -> Value {
    json!({
        "resourceTemplates": [
            {
                "uriTemplate": "artifact://{id}",
                "name": "Artifact",
                "description": "Current authorized artifact content or bundle file listing."
            },
            {
                "uriTemplate": "artifact://{id}/revisions/{revision}",
                "name": "Artifact revision",
                "description": "One retained revision of an authorized artifact."
            },
            {
                "uriTemplate": "artifact://{id}/files/{+path}",
                "name": "Artifact file",
                "description": "One file from the current authorized bundle."
            },
            {
                "uriTemplate": "artifact://{id}/revisions/{revision}/files/{+path}",
                "name": "Artifact revision file",
                "description": "One file from a retained authorized bundle revision."
            },
            {
                "uriTemplate": "artifact://{id}/thumbnail",
                "name": "Artifact thumbnail",
                "description": "Authorized server-owned thumbnail or safe placeholder for an artifact."
            }
        ],
        "ttlMs": RESOURCE_TEMPLATES_TTL_MS,
        "cacheScope": "private"
    })
}

async fn read_resource(
    message: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let params = message.get("params");
    let uri = params
        .and_then(|params| params.get("uri"))
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| JsonRpcError::InvalidParams("uri is required".to_owned()))?;
    if uri == REVIEW_APP_URI {
        if !client_supports_apps(message) {
            return Err(JsonRpcError::InvalidParams(
                "MCP Apps support was not negotiated for this request".to_owned(),
            )
            .into());
        }
        return Ok(resource_result());
    }
    let target = parse_resource_uri(uri)?;
    let id = ArtifactId::from(target.id.as_str());
    let meta = deps
        .artifacts
        .find_meta(&id)
        .await
        .map_err(resource_app_error)?;
    let authorized = AccessPolicy::authorize_publisher_read(auth, meta, &target.id)
        .map_err(resource_app_error)?;
    let current = authorized.meta().clone();
    let artifact = authorized.into_authorized();

    if target.thumbnail {
        // Hold ArtifactStore's lifecycle read guard from readiness/digest recheck through the
        // preview read, so a concurrent update/delete cannot return a stale PNG after auth.
        let thumbnail = deps
            .artifacts
            .read_current_thumbnail(
                &artifact,
                &current.body_sha256,
                std::sync::Arc::clone(&deps.previews),
            )
            .await
            .map_err(resource_app_error)?;
        let (mime_type, bytes) = thumbnail.map_or_else(
            || ("image/svg+xml", deps.previews.placeholder(&current, None)),
            |png| ("image/png", png),
        );
        let byte_count = bytes.len();
        return Ok(json!({
            "contents": [{
                "uri": uri,
                "mimeType": mime_type,
                "blob": STANDARD.encode(bytes),
                "_meta": {
                    "com.agentshelf.artifact-mcp/org": current.org.0,
                    "com.agentshelf.artifact-mcp/revision": current.revision,
                    "com.agentshelf.artifact-mcp/bytes": byte_count,
                    "com.agentshelf.artifact-mcp/trustedThumbnail": true
                }
            }],
            "ttlMs": RESOURCE_READ_TTL_MS,
            "cacheScope": "private"
        }));
    }

    let historical = target
        .revision
        .filter(|revision| *revision != current.revision);
    let (revision, is_bundle, entry, bytes) = if let Some(revision) = historical {
        let history = deps
            .artifacts
            .list_revisions(&artifact)
            .await
            .map_err(resource_app_error)?;
        let selected = history
            .revisions
            .into_iter()
            .find(|candidate| candidate.revision == revision)
            .ok_or_else(|| {
                JsonRpcError::InvalidParams(format!("No such artifact revision: {revision}"))
            })?;
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

    let (mime_type, text, content_bytes) = if is_bundle {
        if let Some(path) = target.path.as_deref() {
            let file = if let Some(revision) = historical {
                deps.artifacts
                    .read_revision_body(&artifact, revision, Some(path))
                    .await
                    .map_err(resource_app_error)?
            } else {
                deps.artifacts
                    .read_bundle_file(&artifact, path)
                    .await
                    .map_err(resource_app_error)?
            }
            .ok_or_else(|| {
                JsonRpcError::InvalidParams(format!("Unknown artifact bundle file: {path}"))
            })?;
            bounded_text(file)?
        } else {
            let files = deps
                .artifacts
                .list_bundle_files(&artifact, historical)
                .await
                .map_err(resource_app_error)?
                .ok_or_else(|| {
                    JsonRpcError::InvalidParams(format!(
                        "Artifact revision content is unavailable: {revision}"
                    ))
                })?;
            let listing = json!({
                "id": current.id.0,
                "revision": revision,
                "entry": entry,
                "bytes": bytes,
                "files": files
                    .into_iter()
                    .map(|(path, bytes)| json!({ "path": path, "bytes": bytes }))
                    .collect::<Vec<_>>()
            });
            let text = serde_json::to_string(&listing).map_err(|error| {
                JsonRpcError::Internal(format!("failed to serialize bundle resource: {error}"))
            })?;
            (
                "application/vnd.artifact-mcp.bundle+json".to_owned(),
                text.clone(),
                text.len(),
            )
        }
    } else {
        if target.path.is_some() {
            return Err(JsonRpcError::InvalidParams(
                "Artifact file URIs apply only to bundle artifacts".to_owned(),
            )
            .into());
        }
        let file = if let Some(revision) = historical {
            deps.artifacts
                .read_revision_body(&artifact, revision, None)
                .await
                .map_err(resource_app_error)?
        } else {
            deps.artifacts
                .read_body(&artifact)
                .await
                .map_err(resource_app_error)?
        }
        .ok_or_else(|| {
            JsonRpcError::InvalidParams(format!(
                "Artifact revision content is unavailable: {revision}"
            ))
        })?;
        bounded_text(file)?
    };

    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": mime_type,
            "text": text,
            "_meta": {
                "com.agentshelf.artifact-mcp/org": current.org.0,
                "com.agentshelf.artifact-mcp/revision": revision,
                "com.agentshelf.artifact-mcp/bytes": content_bytes
            }
        }],
        "ttlMs": RESOURCE_READ_TTL_MS,
        "cacheScope": "private"
    }))
}

fn resource_metadata(meta: &ArtifactMeta) -> Value {
    let name = if meta.title.trim().is_empty() {
        meta.id.0.clone()
    } else {
        meta.title.clone()
    };
    json!({
        "uri": format!("artifact://{}", meta.id.0),
        "name": name,
        "title": meta.title,
        "description": meta.description,
        "mimeType": if meta.is_bundle {
            "application/vnd.artifact-mcp.bundle+json"
        } else {
            "text/html"
        },
        "_meta": {
            "com.agentshelf.artifact-mcp/org": meta.org.0,
            "com.agentshelf.artifact-mcp/revision": meta.revision,
            "com.agentshelf.artifact-mcp/hidden": meta.hidden,
            "com.agentshelf.artifact-mcp/updatedAt": meta.updated_at.0
        }
    })
}

fn bounded_text(file: ArtifactFile) -> Result<(String, String, usize), McpError> {
    if file.content.len() > RESOURCE_MAX_BYTES {
        return Err(JsonRpcError::InvalidParams(format!(
            "Resource exceeds the {RESOURCE_MAX_BYTES}-byte read limit; use the read_artifact tool for byte-paged access"
        ))
        .into());
    }
    let bytes = file.content.len();
    Ok((
        file.content_type,
        String::from_utf8_lossy(&file.content).into_owned(),
        bytes,
    ))
}

fn parse_resource_uri(uri: &str) -> Result<ResourceTarget, McpError> {
    let raw = uri
        .strip_prefix("artifact://")
        .ok_or_else(|| JsonRpcError::InvalidParams("Unsupported resource URI scheme".to_owned()))?;
    let mut segments = raw.split('/');
    let id = segments.next().unwrap_or_default();
    if id.is_empty() || id.contains(['?', '#']) {
        return Err(JsonRpcError::InvalidParams("Invalid artifact resource URI".to_owned()).into());
    }
    let remainder = segments.collect::<Vec<_>>();
    let (revision, path_segments, thumbnail): (Option<u64>, &[&str], bool) = if remainder.is_empty()
    {
        (None, &[], false)
    } else if remainder.len() == 1 && remainder[0] == "thumbnail" {
        (None, &[], true)
    } else if remainder[0] == "files" && remainder.len() > 1 {
        (None, &remainder[1..], false)
    } else if remainder[0] == "revisions" && remainder.len() == 2 {
        (Some(parse_revision(remainder[1])?), &[], false)
    } else if remainder[0] == "revisions" && remainder.len() > 3 && remainder[2] == "files" {
        (Some(parse_revision(remainder[1])?), &remainder[3..], false)
    } else {
        return Err(JsonRpcError::InvalidParams("Invalid artifact resource URI".to_owned()).into());
    };
    let path = if path_segments.is_empty() {
        None
    } else {
        let decoded = path_segments
            .iter()
            .map(|segment| percent_decode(segment))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                JsonRpcError::InvalidParams("Invalid percent-encoding in resource URI".to_owned())
            })?
            .join("/");
        Some(sanitize_relative_path(&decoded).ok_or_else(|| {
            JsonRpcError::InvalidParams("Invalid artifact bundle path".to_owned())
        })?)
    };
    Ok(ResourceTarget {
        id: id.to_owned(),
        revision,
        path,
        thumbnail,
    })
}

fn parse_revision(value: &str) -> Result<u64, McpError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|revision| *revision > 0)
        .ok_or_else(|| JsonRpcError::InvalidParams("Invalid artifact revision".to_owned()).into())
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = hex(*bytes.get(index + 1)?)?;
        let low = hex(*bytes.get(index + 2)?)?;
        decoded.push(high << 4 | low);
        index += 3;
    }
    String::from_utf8(decoded).ok()
}

const fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(offset.to_string())
}

fn parse_cursor(value: &OrderedJson) -> Result<usize, McpError> {
    let cursor = value
        .as_str()
        .ok_or_else(|| JsonRpcError::InvalidParams("Invalid resource cursor".to_owned()))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|offset| offset.parse::<usize>().ok())
        .ok_or_else(|| JsonRpcError::InvalidParams("Invalid resource cursor".to_owned()))?;
    if encode_cursor(decoded) != cursor {
        return Err(JsonRpcError::InvalidParams("Invalid resource cursor".to_owned()).into());
    }
    Ok(decoded)
}

fn resource_app_error(error: AppError) -> McpError {
    match error {
        AppError::Internal | AppError::Unavailable(_) => {
            JsonRpcError::Internal("resource operation failed".to_owned()).into()
        }
        other => JsonRpcError::InvalidParams(other.to_string()).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_uri_parser_accepts_the_four_stable_shapes() {
        assert_eq!(
            parse_resource_uri("artifact://abc123").expect("artifact URI"),
            ResourceTarget {
                id: "abc123".to_owned(),
                revision: None,
                path: None,
                thumbnail: false
            }
        );
        assert_eq!(
            parse_resource_uri("artifact://abc123/revisions/7").expect("revision URI"),
            ResourceTarget {
                id: "abc123".to_owned(),
                revision: Some(7),
                path: None,
                thumbnail: false
            }
        );
        assert_eq!(
            parse_resource_uri("artifact://abc123/files/docs/My%20File.html").expect("file URI"),
            ResourceTarget {
                id: "abc123".to_owned(),
                revision: None,
                path: Some("docs/My File.html".to_owned()),
                thumbnail: false
            }
        );
        assert_eq!(
            parse_resource_uri("artifact://abc123/revisions/2/files/index.html")
                .expect("revision file URI"),
            ResourceTarget {
                id: "abc123".to_owned(),
                revision: Some(2),
                path: Some("index.html".to_owned()),
                thumbnail: false
            }
        );
        assert_eq!(
            parse_resource_uri("artifact://abc123/thumbnail").expect("thumbnail URI"),
            ResourceTarget {
                id: "abc123".to_owned(),
                revision: None,
                path: None,
                thumbnail: true
            }
        );
    }

    #[test]
    fn resource_uri_parser_rejects_traversal_and_noncanonical_cursors() {
        assert!(parse_resource_uri("artifact://abc123/files/../secret").is_err());
        assert!(parse_resource_uri("https://example.test/abc123").is_err());
        assert!(parse_cursor(&OrderedJson::string("MQ==")).is_err());
        assert_eq!(
            parse_cursor(&OrderedJson::string(encode_cursor(50))).expect("cursor"),
            50
        );
    }

    #[test]
    fn resource_reads_enforce_the_dedicated_payload_limit() {
        let exact = ArtifactFile {
            content: vec![b'a'; RESOURCE_MAX_BYTES],
            content_type: "text/plain".to_owned(),
        };
        assert_eq!(
            bounded_text(exact).expect("exact boundary").2,
            RESOURCE_MAX_BYTES
        );
        let oversized = ArtifactFile {
            content: vec![b'a'; RESOURCE_MAX_BYTES + 1],
            content_type: "text/plain".to_owned(),
        };
        assert!(bounded_text(oversized).is_err());
    }
}
