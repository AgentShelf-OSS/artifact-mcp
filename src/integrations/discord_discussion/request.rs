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
mod tests {
    use std::{net::SocketAddr, sync::Arc, time::Duration};

    use super::super::DiscordDiscussionTransport;
    use crate::config::Secret;

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
        sync::Mutex,
    };

    use super::*;

    #[derive(Clone)]
    struct FakeDiscord {
        origin: String,
        request: Arc<Mutex<Option<String>>>,
    }

    async fn fake_discord(response: &'static str, delay: Duration) -> FakeDiscord {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let address: SocketAddr = listener.local_addr().expect("address");
        let request = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&request);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = Vec::new();
            let mut scratch = [0_u8; 4096];
            loop {
                let read = socket.read(&mut scratch).await.expect("read request");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&scratch[..read]);
                let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                let content_length = String::from_utf8_lossy(&buffer)
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if header_end.is_some_and(|end| buffer.len() >= end + 4 + content_length) {
                    break;
                }
            }
            *recorded.lock().await = Some(String::from_utf8_lossy(&buffer).into_owned());
            tokio::time::sleep(delay).await;
            let _ = socket.write_all(response.as_bytes()).await;
        });
        FakeDiscord {
            origin: format!("http://{address}"),
            request,
        }
    }

    async fn fake_discord_sequence(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let address: SocketAddr = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buffer = Vec::new();
                let mut scratch = [0_u8; 4096];
                loop {
                    let read = socket.read(&mut scratch).await.expect("read request");
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&scratch[..read]);
                    let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                    let content_length = String::from_utf8_lossy(&buffer)
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if header_end.is_some_and(|end| buffer.len() >= end + 4 + content_length) {
                        break;
                    }
                }
                recorded
                    .lock()
                    .await
                    .push(String::from_utf8_lossy(&buffer).into_owned());
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        (format!("http://{address}"), requests)
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn local_request(
        origin: &str,
        operation: DiscussionOperation,
    ) -> Result<DiscussionRequest, TerminalReason> {
        local_request_with_ref(origin, "webhook:discussion-ref", operation)
    }

    fn local_request_with_ref(
        origin: &str,
        webhook_ref: &str,
        operation: DiscussionOperation,
    ) -> Result<DiscussionRequest, TerminalReason> {
        discussion_request_inner(
            &format!("{origin}/api/webhooks/123/ULTRA-SECRET?junk=true"),
            webhook_ref.to_owned(),
            operation,
            Some(origin),
        )
    }

    #[tokio::test]
    async fn create_thread_uses_wait_name_and_disabled_mentions_and_returns_ids() {
        let fake = fake_discord(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 61\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\",\"channel_id\":\"234567890123456789\"}",
            Duration::ZERO,
        )
        .await;
        let request = local_request(
            &fake.origin,
            DiscussionOperation::create_thread(
                "Artifact title".to_owned(),
                "@everyone hello".to_owned(),
            )
            .expect("operation"),
        )
        .expect("request");
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
            .expect("transport")
            .deliver(request)
            .await;
        assert!(matches!(
            result,
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt { message_id, thread_id: Some(thread_id) },
                ..
            } if message_id == "123456789012345678" && thread_id == "234567890123456789"
        ));
        let raw = fake.request.lock().await.clone().expect("request recorded");
        assert!(raw.starts_with("POST /api/webhooks/123/ULTRA-SECRET?wait=true HTTP/1.1"));
        assert!(!raw.contains("junk=true"));
        let body = raw.split("\r\n\r\n").nth(1).expect("body");
        let body: Value = serde_json::from_str(body).expect("json");
        assert_eq!(body["thread_name"], "Artifact title");
        assert_eq!(body["content"], "@everyone hello");
        assert_eq!(
            body["allowed_mentions"],
            json!({"parse": [], "users": [], "roles": [], "replied_user": false})
        );
    }

    #[tokio::test]
    async fn notification_thread_retry_probes_existing_thread_then_posts_comment() {
        let channel_id = "123456789012345678";
        let source_message_id = "223456789012345678";
        let comment_message_id = "323456789012345678";
        let thread_body = format!(
            "{{\"id\":\"{source_message_id}\",\"parent_id\":\"{channel_id}\",\"type\":11}}"
        );
        let comment_body = format!("{{\"id\":\"{comment_message_id}\"}}");
        let (origin, requests) = fake_discord_sequence(vec![
            json_response("400 Bad Request", "{}"),
            json_response("200 OK", &thread_body),
            json_response("200 OK", &comment_body),
        ])
        .await;
        let operation = DiscussionOperation::create_thread_from_message(
            channel_id.to_owned(),
            source_message_id.to_owned(),
            "Artifact discussion".to_owned(),
            "First comment".to_owned(),
        )
        .expect("operation");
        let request = discussion_request_inner(
            &format!("{origin}/api/webhooks/123/ULTRA-SECRET"),
            "discussion:connection-a".to_owned(),
            operation,
            Some(&origin),
        )
        .expect("request");
        let debug = format!("{request:?}");
        assert!(!debug.contains("ULTRA-SECRET"));
        assert!(!debug.contains("First comment"));
        let result = DiscordDiscussionTransport::with_timeout_and_bot_token(
            Duration::from_secs(1),
            Some(Secret::new("navi-test-token")),
        )
        .expect("transport")
        .deliver(request)
        .await;
        assert!(matches!(
            result,
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id,
                    thread_id: Some(thread_id),
                },
                ..
            } if message_id == comment_message_id && thread_id == source_message_id
        ));
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with(&format!(
            "POST /api/v10/channels/{channel_id}/messages/{source_message_id}/threads HTTP/1.1"
        )));
        assert!(requests[0].contains("authorization: Bot navi-test-token"));
        assert!(requests[1].starts_with(&format!(
            "GET /api/v10/channels/{source_message_id} HTTP/1.1"
        )));
        assert!(requests[2].starts_with(&format!(
            "POST /api/webhooks/123/ULTRA-SECRET?thread_id={source_message_id}&wait=true HTTP/1.1"
        )));
        assert!(!requests[2].contains("authorization: Bot "));
    }

    #[tokio::test]
    async fn notification_thread_creates_then_posts_the_first_comment_with_webhook_authorship() {
        let channel_id = "123456789012345678";
        let source_message_id = "223456789012345678";
        let comment_message_id = "323456789012345678";
        let thread_body = format!(
            "{{\"id\":\"{source_message_id}\",\"parent_id\":\"{channel_id}\",\"type\":11}}"
        );
        let comment_body = format!("{{\"id\":\"{comment_message_id}\"}}");
        let (origin, requests) = fake_discord_sequence(vec![
            json_response("201 Created", &thread_body),
            json_response("200 OK", &comment_body),
        ])
        .await;
        let request = discussion_request_inner(
            &format!("{origin}/api/webhooks/123/ULTRA-SECRET"),
            "discussion:connection-a".to_owned(),
            DiscussionOperation::create_thread_from_message(
                channel_id.to_owned(),
                source_message_id.to_owned(),
                "Artifact discussion".to_owned(),
                "First comment".to_owned(),
            )
            .expect("operation"),
            Some(&origin),
        )
        .expect("request");
        let result = DiscordDiscussionTransport::with_timeout_and_bot_token(
            Duration::from_secs(1),
            Some(Secret::new("navi-test-token")),
        )
        .expect("transport")
        .deliver(request)
        .await;
        assert!(matches!(
            result,
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id,
                    thread_id: Some(thread_id),
                },
                ..
            } if message_id == comment_message_id && thread_id == source_message_id
        ));
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with(&format!(
            "POST /api/v10/channels/{channel_id}/messages/{source_message_id}/threads HTTP/1.1"
        )));
        assert!(requests[0].contains("authorization: Bot navi-test-token"));
        let thread_payload: Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).expect("thread body"))
                .expect("thread JSON");
        assert_eq!(thread_payload["name"], "Artifact discussion");
        assert_eq!(thread_payload["auto_archive_duration"], 1440);
        assert!(requests[1].starts_with(&format!(
            "POST /api/webhooks/123/ULTRA-SECRET?thread_id={source_message_id}&wait=true HTTP/1.1"
        )));
        assert!(!requests[1].contains("authorization: Bot "));
    }

    #[tokio::test]
    async fn notification_thread_requires_bot_before_any_provider_call() {
        let fake = fake_discord("", Duration::ZERO).await;
        let operation = DiscussionOperation::create_thread_from_message(
            "123456789012345678".to_owned(),
            "223456789012345678".to_owned(),
            "Artifact discussion".to_owned(),
            "First comment".to_owned(),
        )
        .expect("operation");
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_millis(100))
            .expect("transport")
            .deliver(
                discussion_request_inner(
                    &format!("{}/api/webhooks/123/ULTRA-SECRET", fake.origin),
                    "discussion:connection-a".to_owned(),
                    operation,
                    Some(&fake.origin),
                )
                .expect("request"),
            )
            .await;
        assert_eq!(
            result,
            DiscussionResult::Terminal {
                reason: TerminalReason::InvalidSecret
            }
        );
        assert!(fake.request.lock().await.is_none());
    }

    #[tokio::test]
    async fn discussion_connection_reference_reaches_transport_without_becoming_a_url() {
        let fake = fake_discord(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\"}",
            Duration::ZERO,
        )
        .await;
        let request = local_request_with_ref(
            &fake.origin,
            "discussion:connection-a",
            DiscussionOperation::reply("234567890123456789".to_owned(), "reply".to_owned())
                .expect("operation"),
        )
        .expect("discussion ref request");
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
            .expect("transport")
            .deliver(request)
            .await;
        assert!(
            matches!(result, DiscussionResult::Accepted { .. }),
            "{result:?}"
        );
        let raw = fake.request.lock().await.clone().expect("request recorded");
        assert!(!raw.contains("discussion:connection-a"));
    }

    #[tokio::test]
    async fn root_success_without_a_bounded_thread_id_is_ambiguous() {
        let fake = fake_discord(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 44\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\",\"channel_id\":\"0\"}",
            Duration::ZERO,
        )
        .await;
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
            .expect("transport")
            .deliver(
                local_request(
                    &fake.origin,
                    DiscussionOperation::create_thread("Artifact".to_owned(), "comment".to_owned())
                        .expect("operation"),
                )
                .expect("request"),
            )
            .await;
        assert!(matches!(
            result,
            DiscussionResult::Retry {
                reason: RetryReason::Ambiguous,
                duplicate_risk: DuplicateRisk::Possible,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn replies_markers_and_tombstones_use_the_expected_discord_shapes() {
        let cases = [
            (
                DiscussionOperation::reply("234567890123456789".to_owned(), "reply".to_owned())
                    .expect("reply"),
                "POST /api/webhooks/123/ULTRA-SECRET?thread_id=234567890123456789&wait=true HTTP/1.1",
                "reply",
            ),
            (
                DiscussionOperation::resolved_marker("234567890123456789".to_owned())
                    .expect("marker"),
                "POST /api/webhooks/123/ULTRA-SECRET?thread_id=234567890123456789&wait=true HTTP/1.1",
                "[Artifact MCP] Discussion resolved.",
            ),
            (
                DiscussionOperation::reopened_marker("234567890123456789".to_owned())
                    .expect("marker"),
                "POST /api/webhooks/123/ULTRA-SECRET?thread_id=234567890123456789&wait=true HTTP/1.1",
                "[Artifact MCP] Discussion reopened.",
            ),
            (
                DiscussionOperation::tombstone(
                    "234567890123456789".to_owned(),
                    "345678901234567890".to_owned(),
                )
                .expect("tombstone"),
                "PATCH /api/webhooks/123/ULTRA-SECRET/messages/345678901234567890?thread_id=234567890123456789 HTTP/1.1",
                "[Artifact MCP] This Artifact MCP comment was deleted.",
            ),
        ];
        for (operation, expected_start, expected_content) in cases {
            let fake = fake_discord(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\"}",
                Duration::ZERO,
            )
            .await;
            let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
                .expect("transport")
                .deliver(local_request(&fake.origin, operation).expect("request"))
                .await;
            assert!(matches!(result, DiscussionResult::Accepted { .. }));
            let raw = fake.request.lock().await.clone().expect("recorded");
            assert!(raw.starts_with(expected_start), "unexpected request: {raw}");
            let body: Value =
                serde_json::from_str(raw.split("\r\n\r\n").nth(1).expect("body")).expect("json");
            assert_eq!(body["content"], expected_content);
            assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        }
    }

    #[tokio::test]
    async fn local_fake_server_covers_retry_terminal_ambiguous_and_redirect_outcomes() {
        let cases = [
            (
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"retry_after\":0.1}",
                "rate_limited",
            ),
            (
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "server_error",
            ),
            (
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "bad_request",
            ),
            (
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "redirect",
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                "ambiguous",
            ),
        ];
        for (response, expected) in cases {
            let fake = fake_discord(response, Duration::ZERO).await;
            let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
                .expect("transport")
                .deliver(
                    local_request(
                        &fake.origin,
                        DiscussionOperation::reply(
                            "234567890123456789".to_owned(),
                            "reply".to_owned(),
                        )
                        .expect("operation"),
                    )
                    .expect("request"),
                )
                .await;
            match (expected, result) {
                (
                    "rate_limited",
                    DiscussionResult::Retry {
                        reason: RetryReason::RateLimited,
                        ..
                    },
                )
                | (
                    "server_error",
                    DiscussionResult::Retry {
                        reason: RetryReason::ServerError,
                        ..
                    },
                )
                | (
                    "bad_request",
                    DiscussionResult::Terminal {
                        reason: TerminalReason::BadRequest,
                    },
                )
                | (
                    "redirect",
                    DiscussionResult::Terminal {
                        reason: TerminalReason::Redirect,
                    },
                )
                | (
                    "ambiguous",
                    DiscussionResult::Retry {
                        reason: RetryReason::Ambiguous,
                        ..
                    },
                ) => {}
                (_, unexpected) => panic!("unexpected discussion result: {unexpected:?}"),
            }
        }
    }

    #[tokio::test]
    async fn timeout_and_network_are_retryable_and_debug_is_secret_free() {
        let fake = fake_discord("", Duration::from_secs(1)).await;
        let request = local_request(
            &fake.origin,
            DiscussionOperation::reply(
                "234567890123456789".to_owned(),
                "never-log-comment".to_owned(),
            )
            .expect("operation"),
        )
        .expect("request");
        let debug = format!("{request:?}");
        assert!(!debug.contains("ULTRA-SECRET"));
        assert!(!debug.contains("never-log-comment"));
        let timeout = DiscordDiscussionTransport::with_timeout(Duration::from_millis(20))
            .expect("transport")
            .deliver(request)
            .await;
        assert!(matches!(
            timeout,
            DiscussionResult::Retry {
                reason: RetryReason::Timeout,
                ..
            }
        ));

        let network = DiscordDiscussionTransport::with_timeout(Duration::from_millis(100))
            .expect("transport")
            .deliver(
                discussion_request_inner(
                    "http://127.0.0.1:9/api/webhooks/123/ULTRA-SECRET",
                    "webhook:discussion-ref".to_owned(),
                    DiscussionOperation::reply("234567890123456789".to_owned(), "reply".to_owned())
                        .expect("operation"),
                    Some("http://127.0.0.1:9"),
                )
                .expect("request"),
            )
            .await;
        assert!(matches!(
            network,
            DiscussionResult::Retry {
                reason: RetryReason::Network,
                ..
            }
        ));
    }

    #[test]
    fn bounds_and_production_allowlist_are_enforced_without_leaking_inputs() {
        assert!(valid_webhook_ref("webhook:legacy-ref"));
        assert!(valid_webhook_ref("discussion:connection-a"));
        assert!(!valid_webhook_ref(
            "https://discord.com/api/webhooks/123/token"
        ));
        assert!(!valid_webhook_ref("discussion:"));
        assert!(!valid_webhook_ref("other:connection-a"));
        assert_eq!(
            DiscussionOperation::create_thread(String::new(), "body".to_owned()),
            Err(TerminalReason::BadRequest)
        );
        assert_eq!(
            DiscussionOperation::reply("00".to_owned(), "body".to_owned()),
            Err(TerminalReason::BadRequest)
        );
        assert_eq!(
            discussion_request(
                "http://127.0.0.1:9/api/webhooks/123/ULTRA-SECRET",
                "webhook:discussion-ref".to_owned(),
                DiscussionOperation::resolved_marker("234567890123456789".to_owned())
                    .expect("operation"),
            ),
            Err(TerminalReason::AllowlistRejected)
        );
        assert_eq!(
            discussion_request(
                "https://discord.com/api/webhooks/123/token",
                "webhook:discussion-ref".to_owned(),
                DiscussionOperation::CreateThread {
                    thread_name: "Artifact".to_owned(),
                    content: "x".repeat(MAX_DISCUSSION_CONTENT_CHARS + 1),
                },
            ),
            Err(TerminalReason::BadRequest)
        );
        assert_eq!(
            discussion_request(
                "https://discord.com/api/webhooks/123/token",
                "webhook:discussion-ref".to_owned(),
                DiscussionOperation::Reply {
                    thread_id: "234567890123456789".to_owned(),
                    content: "contains\0nul".to_owned(),
                },
            ),
            Err(TerminalReason::BadRequest)
        );
    }
}
