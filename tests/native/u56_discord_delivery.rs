//! PBI-056 provider-contract parity: the future Rust worker and Node reference classify the same
//! Discord response without leaking webhook credentials.

use std::{collections::BTreeMap, path::PathBuf, process::Command};

use artifact_mcp::integrations::discord_delivery::{
    DeliveryClassification, DiscordHttpResponse, DuplicateRisk, ProviderFault, RateLimitScope,
    RetryReason, TerminalReason, classify_fault, classify_http_response, execution_url,
};
use serde_json::{Value, json};

const NODE_DRIVER: &str = r#"
const root = process.argv[1];
const input = JSON.parse(process.argv[2]);
import(`file://${root}/lib/discord-delivery.js`).then((provider) => {
  process.stdout.write(JSON.stringify({
    urls: input.urls.map((item) => { try { return { ok: true, url: provider.executionUrl(item.url, { threadId: item.threadId }) }; } catch (error) { return { ok: false, error: error.message }; } }),
    outcomes: input.responses.map((item) => provider.classifyDiscordResponse(item)),
    faults: input.faults.map((fault) => provider.classifyDiscordFault(fault))
  }));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn node_available() -> bool {
    root().join("lib/discord-delivery.js").is_file()
        && Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
}

fn node(input: &Value) -> Value {
    let output = Command::new("node")
        .current_dir(root())
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(root().to_string_lossy().as_ref())
        .arg(input.to_string())
        .output()
        .expect("run node provider contract");
    assert!(
        output.status.success(),
        "node provider contract failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("node emitted provider contract JSON")
}

fn response(value: &Value) -> DiscordHttpResponse {
    DiscordHttpResponse {
        status: value["status"].as_u64().expect("status") as u16,
        webhook_ref: value["webhookRef"].as_str().map(ToOwned::to_owned),
        headers: value["headers"]
            .as_object()
            .map(|headers| {
                headers
                    .iter()
                    .map(|(key, value)| {
                        (key.clone(), value.as_str().unwrap_or_default().to_owned())
                    })
                    .collect()
            })
            .unwrap_or_default(),
        body: value["body"]
            .as_str()
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
    }
}

fn rust_outcome(outcome: DeliveryClassification) -> Value {
    match outcome {
        DeliveryClassification::Accepted {
            message_id,
            rate_limit,
        } => {
            json!({ "state": "accepted", "messageId": message_id, "rateLimit": rate_limit_json(rate_limit) })
        }
        DeliveryClassification::Retry {
            reason,
            duplicate_risk,
            rate_limit,
        } => json!({
            "state": "retry",
            "reason": match reason {
                RetryReason::RateLimited => "rate_limited",
                RetryReason::Network => "network",
                RetryReason::Timeout => "timeout",
                RetryReason::Ambiguous => "ambiguous",
                RetryReason::ServerError => "server_error",
            },
            "duplicateRisk": match duplicate_risk { DuplicateRisk::None => "none", DuplicateRisk::Possible => "possible" },
            "rateLimit": rate_limit.map(rate_limit_json),
        }),
        DeliveryClassification::Terminal { reason } => json!({
            "state": "terminal",
            "reason": match reason {
                TerminalReason::InvalidSecret => "invalid_secret",
                TerminalReason::DecryptFailed => "decrypt_failed",
                TerminalReason::AllowlistRejected => "allowlist_rejected",
                TerminalReason::Redirect => "redirect",
                TerminalReason::BadRequest => "bad_request",
                TerminalReason::Unauthorized => "unauthorized",
                TerminalReason::Forbidden => "forbidden",
                TerminalReason::NotFound => "not_found",
                TerminalReason::InvalidRateLimitDelay => "invalid_rate_limit_delay",
                TerminalReason::ClientError => "client_error",
                TerminalReason::ServerError => "server_error",
            }
        }),
    }
}

fn rate_limit_json(
    metadata: artifact_mcp::integrations::discord_delivery::RateLimitMetadata,
) -> Value {
    json!({
        "webhookRef": metadata.webhook_ref,
        "retryAfterMs": metadata.retry_after_ms,
        "bucket": metadata.bucket,
        "remaining": metadata.remaining,
        "resetAfterMs": metadata.reset_after_ms,
        "scope": metadata.scope.map(|scope| match scope {
            RateLimitScope::Global => "global", RateLimitScope::Shared => "shared", RateLimitScope::User => "user",
        }),
    })
}

#[test]
fn rust_and_node_provider_contracts_match_for_urls_responses_and_faults() {
    if !node_available() {
        eprintln!("skipping PBI-056 Node parity: node is unavailable");
        return;
    }
    let input = json!({
        "urls": [
            { "url": "https://discord.com/api/webhooks/1/token?thread_id=12&wait=false&junk=x&with_components=true", "threadId": "34" },
            { "url": "https://discordapp.com/api/webhooks/1/token?applied_tags=7,8", "threadId": null },
            { "url": "https://evil.test/api/webhooks/1/secret", "threadId": null }
        ],
        "responses": [
            { "status": 200, "webhookRef": "webhook:wh-secret-ref-1", "headers": { "X-RateLimit-Bucket": "bucket-a", "X-RateLimit-Remaining": "0", "X-RateLimit-Reset-After": "0.25", "X-RateLimit-Scope": "shared" }, "body": "{\"id\":\"123456789012345678\"}" },
            { "status": 204, "headers": {}, "body": "{\"id\":\"bad\"}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": { "Retry-After": "0.251", "X-RateLimit-Reset-After": "9", "X-RateLimit-Bucket": "bucket-a", "X-RateLimit-Scope": "shared" }, "body": "{\"retry_after\":0.25,\"global\":true}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": { "Retry-After": "1.5", "X-RateLimit-Scope": "user", "X-RateLimit-Global": "true" }, "body": "{}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": { "X-RateLimit-Reset-After": "1.5" }, "body": "{}" },
            { "status": 429, "headers": { "Retry-After": "1.5" }, "body": "{}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": {}, "body": "{\"retry_after\":true}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": {}, "body": "{\"retry_after\":\"1.5\"}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": { "Retry-After": "1e300" }, "body": "{}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": { "Retry-After": "86400.001" }, "body": "{}" },
            { "status": 429, "webhookRef": "webhook:wh-secret-ref-1", "headers": { "Retry-After": "86400" }, "body": "{}" },
            { "status": 302, "headers": {}, "body": "" },
            { "status": 400, "headers": {}, "body": "" },
            { "status": 401, "headers": {}, "body": "" },
            { "status": 403, "headers": {}, "body": "" },
            { "status": 404, "headers": {}, "body": "" },
            { "status": 418, "headers": {}, "body": "" },
            { "status": 500, "headers": {}, "body": "" },
            { "status": 502, "headers": {}, "body": "" },
            { "status": 503, "headers": {}, "body": "" },
            { "status": 504, "headers": {}, "body": "" }
        ],
        "faults": ["invalid_secret", "decrypt_failed", "allowlist_rejected", "network", "timeout", "ambiguous"]
    });
    let actual = node(&input);

    let rust_urls: Vec<Value> = input["urls"]
        .as_array()
        .expect("urls")
        .iter()
        .map(|item| {
            match execution_url(
                item["url"].as_str().expect("url"),
                item["threadId"].as_str(),
            ) {
                Ok(url) => json!({ "ok": true, "url": url }),
                Err(reason) => json!({ "ok": false, "error": match reason {
                    TerminalReason::AllowlistRejected => "allowlist_rejected",
                    TerminalReason::BadRequest => "bad_request",
                    _ => "unexpected",
                }}),
            }
        })
        .collect();
    assert_eq!(actual["urls"], json!(rust_urls));

    let rust_responses: Vec<Value> = input["responses"]
        .as_array()
        .expect("responses")
        .iter()
        .map(|item| rust_outcome(classify_http_response(&response(item))))
        .collect();
    assert_eq!(actual["outcomes"], json!(rust_responses));

    let rust_faults: Vec<Value> = [
        ProviderFault::InvalidSecret,
        ProviderFault::DecryptFailed,
        ProviderFault::AllowlistRejected,
        ProviderFault::Network,
        ProviderFault::Timeout,
        ProviderFault::Ambiguous,
    ]
    .into_iter()
    .map(|fault| rust_outcome(classify_fault(fault)))
    .collect();
    assert_eq!(actual["faults"], json!(rust_faults));
}

#[test]
fn classification_debug_never_contains_the_secret_response_body() {
    let secret = "never-log-this-token-or-payload";
    let outcome = classify_http_response(&DiscordHttpResponse {
        status: 200,
        webhook_ref: Some("webhook:wh-ref".to_owned()),
        headers: BTreeMap::new(),
        body: format!(r#"{{"error":"{secret}"}}"#).into_bytes(),
    });
    let debug = format!("{outcome:?}");
    assert!(!debug.contains(secret));
    assert_eq!(outcome, classify_fault(ProviderFault::Ambiguous));
}
