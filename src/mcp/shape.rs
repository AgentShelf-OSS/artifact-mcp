//! JSON shaping shared by the MCP tool handlers.

use serde_json::{Value, json};

use crate::{
    AppDeps,
    error::{JsonRpcError, McpError},
    model::{ArtifactId, ArtifactRevision, Feedback, PublicShare, Timestamp, ViewerView},
};

use super::protocol::OrderedJson;

macro_rules! object {
    ($($key:literal => $value:expr),* $(,)?) => {
        OrderedJson::Object(vec![$(($key.to_owned(), $value)),*])
    };
}
pub(crate) use object;

/// Append `artifact_key` to a metadata object only when the artifact has one, so unkeyed
/// output stays byte-for-byte unchanged.
pub(crate) fn with_artifact_key(payload: OrderedJson, artifact_key: Option<&str>) -> OrderedJson {
    match (payload, artifact_key) {
        (OrderedJson::Object(mut entries), Some(key)) => {
            entries.push(("artifact_key".to_owned(), OrderedJson::string(key)));
            OrderedJson::Object(entries)
        }
        (payload, _) => payload,
    }
}

pub(crate) fn tool_result(payload: OrderedJson) -> Result<Value, McpError> {
    let text = payload.to_json_string().map_err(|error| {
        JsonRpcError::Internal(format!("failed to serialize tool result: {error}"))
    })?;
    let structured = payload.into_value();
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured
    }))
}

pub(crate) fn revision_json(revision: &ArtifactRevision) -> OrderedJson {
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

pub(crate) fn share_json(share: PublicShare) -> OrderedJson {
    object! {
        "token" => OrderedJson::string(share.token.0),
        "expires_at" => optional_timestamp(share.expires_at),
        "created_at" => optional_timestamp(share.created_at),
        "created_by" => optional_string_value(share.created_by),
    }
}

pub(crate) fn viewer_json(viewer: &ViewerView) -> OrderedJson {
    object! {
        "email" => OrderedJson::string(viewer.email.0.clone()),
        "count" => OrderedJson::number_u64(viewer.count),
        "first_viewed_at" => OrderedJson::string(viewer.first_viewed_at.0.clone()),
        "last_viewed_at" => OrderedJson::string(viewer.last_viewed_at.0.clone()),
    }
}

pub(crate) fn feedback_json(feedback: &Feedback) -> OrderedJson {
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
        "anchor_kind" => optional_id(feedback.anchor_kind.as_deref()),
        "anchor_node_id" => optional_id(feedback.anchor_node_id.as_deref()),
        "anchor_quote" => optional_string_value(feedback.anchor_quote.clone()),
        "anchor_version" => OrderedJson::number_u64(u64::from(feedback.anchor_version)),
        "created_at" => OrderedJson::string(feedback.created_at.0.clone()),
        "resolved_at" => optional_timestamp(feedback.resolved_at.clone()),
        "resolved_by" => optional_string_value(feedback.resolved_by.clone()),
    }
}

pub(crate) fn optional_timestamp(value: Option<Timestamp>) -> OrderedJson {
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

pub(crate) fn artifact_url(deps: &AppDeps, id: &ArtifactId) -> String {
    format!("{}/{id}", deps.config.public_base_url)
}

pub(crate) fn javascript_string(value: &OrderedJson) -> String {
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
