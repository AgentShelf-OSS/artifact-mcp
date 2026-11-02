use super::*;

use crate::integrations::discord_delivery::TerminalReason;
use reqwest::Method;
use serde_json::{Value, json};

#[test]
fn invalid_secret_precedes_invalid_operation_and_url() {
    let operation = DiscussionOperation::Reply {
        thread_id: "0".into(),
        content: String::new(),
    };
    assert_eq!(
        discussion_request("not a url", "bad ref".into(), operation),
        Err(TerminalReason::InvalidSecret)
    );
}

#[test]
fn invalid_operation_precedes_webhook_allowlist() {
    let operation = DiscussionOperation::Reply {
        thread_id: "0".into(),
        content: String::new(),
    };
    assert_eq!(
        discussion_request("not a url", "discussion:valid".into(), operation),
        Err(TerminalReason::BadRequest)
    );
}

#[test]
fn valid_operation_rejects_unapproved_webhook() {
    let operation = DiscussionOperation::reply("123".into(), "hello".into()).unwrap();
    assert_eq!(
        discussion_request(
            "https://example.com/api/webhooks/123/token",
            "discussion:valid".into(),
            operation
        ),
        Err(TerminalReason::AllowlistRejected)
    );
}

#[test]
fn discussion_request_preserves_operation_specific_routes_and_payloads() {
    let webhook_url = "https://discord.com/api/webhooks/123/token?discard=this";
    let webhook_ref = "discussion:connection-a";
    let thread_id = "234567890123456789";
    let message_id = "345678901234567890";
    let cases = [
        (
            DiscussionOperation::create_thread("Forum post".to_owned(), "first".to_owned())
                .expect("create thread"),
            Method::POST,
            "https://discord.com/api/webhooks/123/token?wait=true",
            json!({
                "content": "first",
                "thread_name": "Forum post",
                "allowed_mentions": { "parse": [], "users": [], "roles": [], "replied_user": false },
            }),
        ),
        (
            DiscussionOperation::reply(thread_id.to_owned(), "reply".to_owned()).expect("reply"),
            Method::POST,
            "https://discord.com/api/webhooks/123/token?thread_id=234567890123456789&wait=true",
            json!({
                "content": "reply",
                "allowed_mentions": { "parse": [], "users": [], "roles": [], "replied_user": false },
            }),
        ),
        (
            DiscussionOperation::tombstone(thread_id.to_owned(), message_id.to_owned())
                .expect("tombstone"),
            Method::PATCH,
            "https://discord.com/api/webhooks/123/token/messages/345678901234567890?thread_id=234567890123456789",
            json!({
                "content": "[Artifact MCP] This Artifact MCP comment was deleted.",
                "allowed_mentions": { "parse": [], "users": [], "roles": [], "replied_user": false },
            }),
        ),
    ];

    for (operation, method, url, payload) in cases {
        let request =
            discussion_request(webhook_url, webhook_ref.to_owned(), operation).expect("request");
        assert_eq!(request.method, method);
        assert_eq!(request.url, url);
        assert_eq!(
            serde_json::from_slice::<Value>(&request.payload).expect("payload JSON"),
            payload
        );
    }
}
