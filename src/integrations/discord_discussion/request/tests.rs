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
    let thread_body =
        format!("{{\"id\":\"{source_message_id}\",\"parent_id\":\"{channel_id}\",\"type\":11}}");
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
    let thread_body =
        format!("{{\"id\":\"{source_message_id}\",\"parent_id\":\"{channel_id}\",\"type\":11}}");
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
            DiscussionOperation::resolved_marker("234567890123456789".to_owned()).expect("marker"),
            "POST /api/webhooks/123/ULTRA-SECRET?thread_id=234567890123456789&wait=true HTTP/1.1",
            "[Artifact MCP] Discussion resolved.",
        ),
        (
            DiscussionOperation::reopened_marker("234567890123456789".to_owned()).expect("marker"),
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
                    DiscussionOperation::reply("234567890123456789".to_owned(), "reply".to_owned())
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
