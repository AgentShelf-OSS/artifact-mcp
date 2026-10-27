//! Modern-era decoration of tool results: review app data and artifact resource links.

use serde_json::{Value, json};

use crate::{
    AppDeps,
    model::{ArtifactId, ArtifactMeta, PublisherIdentity},
    security::access::AccessPolicy,
};

use super::{apps::client_supports_apps, protocol::OrderedJson, shape::artifact_url};

pub(crate) async fn add_review_app_data(
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

pub(crate) fn add_artifact_resource_link(message: &OrderedJson, mut result: Value) -> Value {
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
