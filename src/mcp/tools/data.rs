//! Live data tools (ADR-0012): data sources, bindings, values, events, and reminders.

use serde_json::Value;

use crate::{
    AppDeps,
    error::{AppError, JsonRpcError, McpError},
    mcp::{
        args::{publisher_read, publisher_write, required_string},
        protocol::OrderedJson,
        shape::{object, tool_result},
    },
    model::PublisherIdentity,
};

pub(crate) async fn list_data_sources(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let requested = arguments.get("org").and_then(OrderedJson::as_str);
    let org = if auth.is_admin() {
        requested
    } else {
        Some(auth.org.0.as_str())
    };
    let value = serde_json::to_value(deps.data.public_sources(org).await)
        .map_err(|_| JsonRpcError::Internal("data serialization failed".into()))?;
    tool_result(
        object! { "sources" => serde_json::from_value(value).unwrap_or_else(|_| OrderedJson::Array(vec![])) },
    )
}

pub(crate) async fn data_bindings(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    write: bool,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let authorized = if write {
        publisher_write(id, auth, deps).await?.into_authorized()
    } else {
        publisher_read(id, auth, deps).await?.into_authorized()
    };
    if !write {
        let manifest = deps
            .data
            .try_bindings(id)
            .await
            .map_err(AppError::Validation)?;
        return tool_result(
            object! { "id" => OrderedJson::string(id), "bindings" => serde_json::from_value(serde_json::to_value(manifest.bindings).unwrap()).unwrap() },
        );
    }
    let raw = arguments
        .get("bindings")
        .cloned()
        .ok_or_else(|| JsonRpcError::InvalidParams("bindings is required".into()))?;
    let manifest: crate::data::BindingManifest =
        serde_json::from_value(serde_json::json!({"bindings": serde_json::to_value(raw).unwrap()}))
            .map_err(|_| AppError::Validation("bad_params".into()))?;
    deps.data
        .set_bindings(id, manifest.clone(), &authorized.meta().org.0)
        .await
        .map_err(AppError::Validation)?;
    tool_result(
        object! { "id" => OrderedJson::string(id), "bindings" => serde_json::from_value(serde_json::to_value(manifest.bindings).unwrap()).unwrap() },
    )
}

pub(crate) async fn set_artifact_data(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_write(id, auth, deps).await?;
    let binding = required_string(arguments, "binding")?;
    let key = required_string(arguments, "key")?;
    let value = arguments
        .get("value")
        .cloned()
        .ok_or_else(|| JsonRpcError::InvalidParams("value is required".into()))?;
    let revision = deps
        .data
        .set_snapshot(id, binding, key, serde_json::to_value(value).unwrap())
        .await
        .map_err(AppError::Validation)?;
    let _ = owned;
    tool_result(
        object! {"id"=>OrderedJson::string(id),"revision"=>OrderedJson::number_u64(revision)},
    )
}

pub(crate) async fn append_artifact_events(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_write(id, auth, deps).await?;
    let binding = required_string(arguments, "binding")?;
    let sub = required_string(arguments, "subscription")?;
    let raw = arguments
        .get("events")
        .cloned()
        .ok_or_else(|| JsonRpcError::InvalidParams("events is required".into()))?;
    let events: Vec<crate::data::Event> =
        serde_json::from_value(serde_json::to_value(raw).unwrap())
            .map_err(|_| AppError::Validation("bad_params".into()))?;
    let (accepted, duplicates) = deps
        .data
        .append_events(id, binding, sub, events)
        .await
        .map_err(AppError::Validation)?;
    let _ = owned;
    tool_result(
        object! {"id"=>OrderedJson::string(id),"accepted"=>OrderedJson::number_u64(accepted as u64),"duplicates"=>OrderedJson::number_u64(duplicates as u64)},
    )
}

const REMINDER_KEY_ERROR: &str =
    "Invalid reminder key: use 1-64 characters from A-Z, a-z, 0-9, '.', '_', or '-'.";
const REMINDER_TIME_ERROR: &str = "Invalid reminder time: give exactly one of fire_at or delay_seconds, resolving to 60 seconds to 30 days from now.";
const REMINDER_TEXT_ERROR: &str = "Invalid reminder text: title must be 1-80 characters after trimming, body at most 240, with no control characters other than newline.";
const REMINDER_LIMIT_ERROR: &str =
    "Reminder limit reached: an artifact can have at most 16 armed reminders.";

/// ADR-0012 `set_artifact_reminder`: org scope only, writer permission, `clear` deletes the key.
pub(crate) async fn set_artifact_reminder(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    use crate::{
        integrations::push_runtime::ReminderInput,
        persistence::push::{PUSH_DISABLED_MESSAGE, PushError, ReminderScope, valid_key},
    };
    let Some(push) = deps.push.clone() else {
        return Err(AppError::Validation(PUSH_DISABLED_MESSAGE.to_owned()).into());
    };
    let id = required_string(arguments, "id")?;
    let key = required_string(arguments, "key")?;
    let owned = publisher_write(id, auth, deps).await?;
    if !valid_key(key) {
        return Err(AppError::Validation(REMINDER_KEY_ERROR.to_owned()).into());
    }
    let meta = owned.into_authorized().into_meta();
    let clear = matches!(arguments.get("clear"), Some(OrderedJson::Bool(true)));
    if clear {
        push.clear_reminder(meta.id.0, ReminderScope::Org, String::new(), key.to_owned())
            .await
            .map_err(|_| AppError::Internal)?;
        return tool_result(object! {
            "id" => OrderedJson::string(id),
            "key" => OrderedJson::string(key),
            "cleared" => OrderedJson::Bool(true),
        });
    }
    let saved = push
        .set_reminder(
            meta.id.0,
            meta.org.0,
            ReminderScope::Org,
            String::new(),
            key.to_owned(),
            ReminderInput {
                fire_at: arguments.get("fire_at"),
                delay_seconds: arguments.get("delay_seconds"),
                title: arguments.get("title"),
                body: arguments.get("body"),
            },
            format!("publisher:{}", auth.client_id.0),
        )
        .await
        .map_err(|failure| match failure {
            PushError::BadKey => AppError::Validation(REMINDER_KEY_ERROR.to_owned()),
            PushError::BadTime => AppError::Validation(REMINDER_TIME_ERROR.to_owned()),
            PushError::BadText => AppError::Validation(REMINDER_TEXT_ERROR.to_owned()),
            PushError::ReminderLimit => AppError::Conflict(REMINDER_LIMIT_ERROR.to_owned()),
            _ => AppError::Internal,
        })?;
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "key" => OrderedJson::string(key),
        "scope" => OrderedJson::string("org"),
        "fire_at" => OrderedJson::number_i64(saved.fire_at),
        "revision" => OrderedJson::number_i64(saved.revision),
    })
}
