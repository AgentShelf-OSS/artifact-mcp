//! Discord discussion transport for legacy Forum/Media posts and notification-anchored threads.
//!
//! The public operation and result types are deliberately provider-neutral: PBI-079's
//! orchestration can persist one artifact discussion contract without treating a Discord URL as
//! an identifier.  This adapter only converts the operation to Discord's webhook protocol.  It
//! neither owns an outbox row nor performs a database operation.
//!
//! Every request uses one redirect-refusing deadline and `allowed_mentions` with an empty parse
//! list.  `Debug` for all secret- or comment-bearing values is hand-written and redacted.

use std::fmt;

use reqwest::Method;
use serde_json::{Value, json};
use url::Url;

use crate::{
    integrations::discord_delivery::{
        DuplicateRisk, RateLimitMetadata, RetryReason, TerminalReason,
    },
    persistence::webhooks::is_discord_webhook_url,
};

/// Discord limits normal message content to 2,000 characters.
pub const MAX_DISCUSSION_CONTENT_CHARS: usize = 2_000;
/// Discord forum and media posts have a bounded thread name.
pub const MAX_THREAD_NAME_CHARS: usize = 100;
/// Discord snowflakes are decimal and this bound matches PBI-056 delivery IDs.
pub const MAX_DISCUSSION_ID_BYTES: usize = 32;
pub(super) const MAX_RESPONSE_BODY_BYTES: usize = 65_536;

/// Safe metadata returned by Discord's token-authenticated incoming-webhook lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordWebhookDestination {
    pub webhook_id: String,
    pub channel_id: String,
    pub guild_id: String,
}

/// A discussion action independently meaningful to a future provider adapter.
///
/// The text fields are intentionally private to this module's debug formatter.  Callers create
/// an action through the constructors so invalid/unbounded values cannot reach a webhook.
#[derive(Clone, PartialEq, Eq)]
pub enum DiscussionOperation {
    CreateThread {
        thread_name: String,
        content: String,
    },
    CreateThreadFromMessage {
        channel_id: String,
        message_id: String,
        thread_name: String,
        content: String,
    },
    Reply {
        thread_id: String,
        content: String,
    },
    ResolvedMarker {
        thread_id: String,
    },
    ReopenedMarker {
        thread_id: String,
    },
    Tombstone {
        thread_id: String,
        message_id: String,
    },
}

impl fmt::Debug for DiscussionOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateThread { .. } => formatter
                .debug_struct("CreateThread")
                .field("thread_name", &"<redacted>")
                .field("content", &"<redacted>")
                .finish(),
            Self::CreateThreadFromMessage {
                channel_id,
                message_id,
                ..
            } => formatter
                .debug_struct("CreateThreadFromMessage")
                .field("channel_id", channel_id)
                .field("message_id", message_id)
                .field("thread_name", &"<redacted>")
                .field("content", &"<redacted>")
                .finish(),
            Self::Reply { thread_id, .. } => formatter
                .debug_struct("Reply")
                .field("thread_id", thread_id)
                .field("content", &"<redacted>")
                .finish(),
            Self::ResolvedMarker { thread_id } => formatter
                .debug_struct("ResolvedMarker")
                .field("thread_id", thread_id)
                .finish(),
            Self::ReopenedMarker { thread_id } => formatter
                .debug_struct("ReopenedMarker")
                .field("thread_id", thread_id)
                .finish(),
            Self::Tombstone {
                thread_id,
                message_id,
            } => formatter
                .debug_struct("Tombstone")
                .field("thread_id", thread_id)
                .field("message_id", message_id)
                .finish(),
        }
    }
}

impl DiscussionOperation {
    /// Build the first mirrored comment and its Discord forum/media post name.
    pub fn create_thread(thread_name: String, content: String) -> Result<Self, TerminalReason> {
        validate_thread_name(&thread_name)?;
        validate_content(&content)?;
        Ok(Self::CreateThread {
            thread_name,
            content,
        })
    }

    /// Build the first mirrored comment in a public thread attached to an existing message.
    pub fn create_thread_from_message(
        channel_id: String,
        message_id: String,
        thread_name: String,
        content: String,
    ) -> Result<Self, TerminalReason> {
        validate_id(&channel_id)?;
        validate_id(&message_id)?;
        validate_thread_name(&thread_name)?;
        validate_content(&content)?;
        Ok(Self::CreateThreadFromMessage {
            channel_id,
            message_id,
            thread_name,
            content,
        })
    }

    /// Build a later comment or reply for the existing external thread.
    pub fn reply(thread_id: String, content: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        validate_content(&content)?;
        Ok(Self::Reply { thread_id, content })
    }

    /// Append the compact, system-authored resolved marker.
    pub fn resolved_marker(thread_id: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        Ok(Self::ResolvedMarker { thread_id })
    }

    /// Append the compact, system-authored reopened marker.
    pub fn reopened_marker(thread_id: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        Ok(Self::ReopenedMarker { thread_id })
    }

    /// Replace a webhook-authored comment with a compact deletion tombstone.
    pub fn tombstone(thread_id: String, message_id: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        validate_id(&message_id)?;
        Ok(Self::Tombstone {
            thread_id,
            message_id,
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Self::CreateThread { .. } => "create_thread",
            Self::CreateThreadFromMessage { .. } => "create_thread_from_message",
            Self::Reply { .. } => "reply",
            Self::ResolvedMarker { .. } => "resolved_marker",
            Self::ReopenedMarker { .. } => "reopened_marker",
            Self::Tombstone { .. } => "tombstone",
        }
    }

    pub(super) fn requires_thread_receipt(&self) -> bool {
        matches!(
            self,
            Self::CreateThread { .. } | Self::CreateThreadFromMessage { .. }
        )
    }
}

/// Provider-neutral acceptance receipt. `thread_id` is present only after creating a forum/media
/// post; later replies intentionally retain their already persisted thread mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionReceipt {
    pub message_id: String,
    pub thread_id: Option<String>,
}

/// Provider-neutral terminal/retry outcome, sharing PBI-056's locked semantics and rate-limit
/// metadata.  It contains no Discord URL, credential, or user-authored content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscussionResult {
    Accepted {
        receipt: DiscussionReceipt,
        rate_limit: RateLimitMetadata,
    },
    Retry {
        reason: RetryReason,
        duplicate_risk: DuplicateRisk,
        rate_limit: Option<RateLimitMetadata>,
    },
    Terminal {
        reason: TerminalReason,
    },
}

/// A redaction-safe executable request. The webhook URL and comment body remain private.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscussionRequest {
    pub(super) url: String,
    pub(super) webhook_ref: String,
    pub(super) method: Method,
    pub(super) payload: Vec<u8>,
    pub(super) operation: DiscussionOperation,
    pub(super) thread_anchor: Option<ThreadAnchorRequest>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ThreadAnchorRequest {
    pub(super) create_url: String,
    pub(super) probe_url: String,
    pub(super) payload: Vec<u8>,
    pub(super) thread_id: String,
    pub(super) parent_channel_id: String,
}

impl fmt::Debug for DiscussionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscussionRequest")
            .field("url", &"<redacted>")
            .field("webhook_ref", &"<redacted>")
            .field("method", &self.method)
            .field("payload", &"<redacted>")
            .field("operation", &self.operation.name())
            .field("thread_anchor", &self.thread_anchor.is_some())
            .finish()
    }
}

/// Build a Discord-backed request for a provider-neutral discussion operation.
pub fn discussion_request(
    webhook_url: &str,
    webhook_ref: String,
    operation: DiscussionOperation,
) -> Result<DiscussionRequest, TerminalReason> {
    discussion_request_inner(webhook_url, webhook_ref, operation, None)
}

/// Build a Discord-backed request for a provider-neutral discussion operation.
///
/// `webhook_ref` is the opaque durable secret reference, not a URL.  It is validated before a
/// request can be constructed, while the URL is checked against the shared HTTPS Discord
/// allowlist and normalised to discard caller-supplied query parameters.
fn discussion_request_inner(
    webhook_url: &str,
    webhook_ref: String,
    operation: DiscussionOperation,
    #[allow(unused_variables)] test_origin: Option<&str>,
) -> Result<DiscussionRequest, TerminalReason> {
    if !valid_webhook_ref(&webhook_ref) {
        return Err(TerminalReason::InvalidSecret);
    }
    validate_operation(&operation)?;
    let (method, url, payload, thread_anchor) = match &operation {
        DiscussionOperation::CreateThread {
            thread_name,
            content,
        } => (
            Method::POST,
            execute_url(webhook_url, None, test_origin)?,
            message_payload(content, Some(thread_name)),
            None,
        ),
        DiscussionOperation::CreateThreadFromMessage {
            channel_id,
            message_id,
            thread_name,
            content,
        } => (
            Method::POST,
            execute_url(webhook_url, Some(message_id), test_origin)?,
            message_payload(content, None),
            Some(thread_anchor_request(
                channel_id,
                message_id,
                thread_name,
                test_origin,
            )?),
        ),
        DiscussionOperation::Reply { thread_id, content } => (
            Method::POST,
            execute_url(webhook_url, Some(thread_id), test_origin)?,
            message_payload(content, None),
            None,
        ),
        DiscussionOperation::ResolvedMarker { thread_id } => (
            Method::POST,
            execute_url(webhook_url, Some(thread_id), test_origin)?,
            marker_payload("Discussion resolved."),
            None,
        ),
        DiscussionOperation::ReopenedMarker { thread_id } => (
            Method::POST,
            execute_url(webhook_url, Some(thread_id), test_origin)?,
            marker_payload("Discussion reopened."),
            None,
        ),
        DiscussionOperation::Tombstone {
            thread_id,
            message_id,
        } => (
            Method::PATCH,
            webhook_message_url(webhook_url, thread_id, message_id, test_origin)?,
            marker_payload("This Artifact MCP comment was deleted."),
            None,
        ),
    };
    Ok(DiscussionRequest {
        url,
        webhook_ref,
        method,
        payload,
        operation,
        thread_anchor,
    })
}

fn validate_operation(operation: &DiscussionOperation) -> Result<(), TerminalReason> {
    match operation {
        DiscussionOperation::CreateThread {
            thread_name,
            content,
        } => {
            validate_thread_name(thread_name)?;
            validate_content(content)
        }
        DiscussionOperation::CreateThreadFromMessage {
            channel_id,
            message_id,
            thread_name,
            content,
        } => {
            validate_id(channel_id)?;
            validate_id(message_id)?;
            validate_thread_name(thread_name)?;
            validate_content(content)
        }
        DiscussionOperation::Reply { thread_id, content } => {
            validate_id(thread_id)?;
            validate_content(content)
        }
        DiscussionOperation::ResolvedMarker { thread_id }
        | DiscussionOperation::ReopenedMarker { thread_id } => validate_id(thread_id),
        DiscussionOperation::Tombstone {
            thread_id,
            message_id,
        } => {
            validate_id(thread_id)?;
            validate_id(message_id)
        }
    }
}

fn thread_anchor_request(
    channel_id: &str,
    message_id: &str,
    thread_name: &str,
    test_origin: Option<&str>,
) -> Result<ThreadAnchorRequest, TerminalReason> {
    validate_id(channel_id)?;
    validate_id(message_id)?;
    validate_thread_name(thread_name)?;
    let api_origin = test_origin
        .map(str::trim_end)
        .unwrap_or("https://discord.com")
        .trim_end_matches('/');
    let create_url =
        format!("{api_origin}/api/v10/channels/{channel_id}/messages/{message_id}/threads");
    let probe_url = format!("{api_origin}/api/v10/channels/{message_id}");
    Ok(ThreadAnchorRequest {
        create_url,
        probe_url,
        payload: serde_json::to_vec(&json!({
            "name": thread_name,
            "auto_archive_duration": 1440
        }))
        .expect("fixed JSON thread payload serializes"),
        thread_id: message_id.to_owned(),
        parent_channel_id: channel_id.to_owned(),
    })
}

pub(super) fn execute_url(
    webhook_url: &str,
    thread_id: Option<&str>,
    test_origin: Option<&str>,
) -> Result<String, TerminalReason> {
    let mut url = parse_allowed_webhook(webhook_url, test_origin)?;
    let mut query = url.query_pairs_mut();
    query.clear();
    if let Some(thread_id) = thread_id {
        validate_id(thread_id)?;
        query.append_pair("thread_id", thread_id);
    }
    query.append_pair("wait", "true");
    drop(query);
    Ok(url.into())
}

fn webhook_message_url(
    webhook_url: &str,
    thread_id: &str,
    message_id: &str,
    test_origin: Option<&str>,
) -> Result<String, TerminalReason> {
    validate_id(thread_id)?;
    validate_id(message_id)?;
    let mut url = parse_allowed_webhook(webhook_url, test_origin)?;
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/messages/{message_id}"));
    let mut query = url.query_pairs_mut();
    query.clear();
    query.append_pair("thread_id", thread_id);
    drop(query);
    Ok(url.into())
}

pub(super) fn parse_allowed_webhook(
    value: &str,
    test_origin: Option<&str>,
) -> Result<Url, TerminalReason> {
    let url = Url::parse(value).map_err(|_| TerminalReason::AllowlistRejected)?;
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(TerminalReason::AllowlistRejected);
    }
    if is_discord_webhook_url(value) {
        return Ok(url);
    }
    // Kept private to this module's unit tests: production callers cannot opt a local endpoint
    // into the allowlist. It lets tests exercise actual request shapes and redirect policy.
    if test_origin.is_some_and(|origin| url.as_str().starts_with(origin)) {
        return Ok(url);
    }
    Err(TerminalReason::AllowlistRejected)
}

pub(super) fn message_payload(content: &str, thread_name: Option<&str>) -> Vec<u8> {
    let mut payload = json!({
        "content": content,
        "allowed_mentions": { "parse": [], "users": [], "roles": [], "replied_user": false },
    });
    if let Some(thread_name) = thread_name {
        payload["thread_name"] = Value::String(thread_name.to_owned());
    }
    serde_json::to_vec(&payload).expect("fixed JSON discussion payload serializes")
}

fn marker_payload(marker: &str) -> Vec<u8> {
    message_payload(&format!("[Artifact MCP] {marker}"), None)
}

fn validate_content(content: &str) -> Result<(), TerminalReason> {
    (!content.is_empty()
        && content.chars().count() <= MAX_DISCUSSION_CONTENT_CHARS
        && !content.contains('\0'))
    .then_some(())
    .ok_or(TerminalReason::BadRequest)
}

fn validate_thread_name(name: &str) -> Result<(), TerminalReason> {
    (!name.is_empty() && name.chars().count() <= MAX_THREAD_NAME_CHARS && !name.contains('\0'))
        .then_some(())
        .ok_or(TerminalReason::BadRequest)
}

pub(super) fn validate_id(value: &str) -> Result<(), TerminalReason> {
    valid_snowflake(value)
        .then_some(())
        .ok_or(TerminalReason::BadRequest)
}

pub(super) fn valid_snowflake(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DISCUSSION_ID_BYTES
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(super) fn valid_webhook_ref(value: &str) -> bool {
    let Some(suffix) = value
        .strip_prefix("webhook:")
        .or_else(|| value.strip_prefix("discussion:"))
    else {
        return false;
    };
    !suffix.is_empty()
        && value.len() <= 128
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests;
