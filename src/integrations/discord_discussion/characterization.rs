use super::*;

use reqwest::Method;
use serde_json::{Value, json};

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
