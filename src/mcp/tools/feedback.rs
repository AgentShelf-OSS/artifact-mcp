//! Viewer feedback tools.

use serde_json::Value;

use crate::{
    AppDeps,
    error::{AppError, McpError},
    mcp::{
        args::{nonempty_protocol_string, publisher_read, required_string},
        protocol::OrderedJson,
        shape::{feedback_json, object, tool_result},
    },
    model::{EmailAddress, PublisherIdentity, SubmitFeedback},
    security::access::AccessPolicy,
};

pub(crate) async fn list_feedback(
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

pub(crate) async fn submit_feedback(
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
                anchor_v2: None,
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

pub(crate) async fn resolve_feedback(
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

pub(crate) async fn reopen_feedback(
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
