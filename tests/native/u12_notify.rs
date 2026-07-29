//! U12 Discord delivery: SSRF allowlist, redirect refusal, timeout, detachment, embed goldens.

use std::sync::Arc;
use std::time::{Duration, Instant};

use artifact_mcp::integrations::notify::{
    DELIVERY_TIMEOUT, HttpTransport, REDIRECT_REFUSED, UNKNOWN_WEBHOOK, URL_NOT_ALLOWED,
    WebhookTransport, build_embed, multipart_request,
};
use artifact_mcp::model::{
    CreateWebhook, DeliveryResult, OrgId, WebhookDelivery, WebhookEvent, WebhookId,
};
use artifact_mcp::ports::NotificationSink;

use crate::u12_support::{
    Behaviour, RecordingTransport, ServerBehaviour, fixture, notifier as make_notifier, payload,
    spawn_server, store_with, test_key,
};

const TOKEN: &str = "ULTRA-SECRET-WEBHOOK-TOKEN-wxyz";

fn discord_url() -> String {
    format!("https://discord.com/api/webhooks/123456789012345678/{TOKEN}")
}

fn delivery(url: &str) -> WebhookDelivery {
    WebhookDelivery {
        id: WebhookId("wh0000000001".to_owned()),
        org: OrgId("acme".to_owned()),
        url: url.to_owned(),
        label: "Ops channel".to_owned(),
        events: vec![WebhookEvent::Published],
    }
}

// ---------------------------------------------------------------------------
// The SSRF allowlist
// ---------------------------------------------------------------------------

/// Every case the guard must decide, with the expected outcome.
///
/// `true` means the request is allowed to leave the process. The lookalike, embedded-URL, and
/// userinfo cases are the ones a naive "does the URL contain discord.com" check would let through.
const ALLOWLIST_MATRIX: &[(&str, bool)] = &[
    ("https://discord.com/api/webhooks/1/token", true),
    ("https://discordapp.com/api/webhooks/1/token", true),
    ("HTTPS://DISCORD.COM/API/WEBHOOKS/1/token", true),
    // Wrong scheme.
    ("http://discord.com/api/webhooks/1/token", false),
    ("ftp://discord.com/api/webhooks/1/token", false),
    ("//discord.com/api/webhooks/1/token", false),
    // Wrong host.
    ("https://example.test/api/webhooks/1/token", false),
    ("https://hooks.slack.com/services/x/y/z", false),
    // Lookalikes.
    ("https://discord.com.evil.tld/api/webhooks/1/token", false),
    (
        "https://discordapp.com.evil.tld/api/webhooks/1/token",
        false,
    ),
    ("https://discord.company/api/webhooks/1/token", false),
    ("https://evil-discord.com/api/webhooks/1/token", false),
    ("https://sub.discord.com/api/webhooks/1/token", false),
    // Userinfo and embedded-URL smuggling.
    ("https://discord.com@evil.tld/api/webhooks/1/token", false),
    (
        "https://evil.tld/https://discord.com/api/webhooks/1/t",
        false,
    ),
    (
        "https://evil.tld/#https://discord.com/api/webhooks/1/t",
        false,
    ),
    // IP literals, including the cloud metadata endpoint and loopback.
    ("https://169.254.169.254/api/webhooks/1/token", false),
    ("https://127.0.0.1/api/webhooks/1/token", false),
    ("https://[::1]/api/webhooks/1/token", false),
    ("https://10.0.0.1/api/webhooks/1/token", false),
    // Other schemes that reach the local machine.
    ("file:///etc/passwd", false),
    ("http://localhost:3480/api/webhooks/1/token", false),
    // Leading whitespace: `create()` trims, but a stored row is matched literally.
    (" https://discord.com/api/webhooks/1/token", false),
    ("", false),
];

#[tokio::test]
async fn the_allowlist_decides_every_case_before_a_request_leaves_the_process() {
    let (_dir, _pool, store) = fixture("allowlist", "acme", None).await;

    for (url, allowed) in ALLOWLIST_MATRIX {
        let transport = RecordingTransport::ok();
        let notifier = make_notifier(&store, Arc::clone(&transport));
        let result = notifier
            .deliver(
                &delivery(url),
                &WebhookEvent::Published,
                &OrgId("acme".to_owned()),
                &payload(),
                None,
            )
            .await;

        if *allowed {
            assert_eq!(
                result,
                DeliveryResult {
                    ok: true,
                    error: None
                },
                "{url} should have been delivered"
            );
            assert_eq!(transport.calls().len(), 1, "{url} should have been sent");
        } else {
            assert_eq!(
                result,
                DeliveryResult {
                    ok: false,
                    error: Some(URL_NOT_ALLOWED.to_owned())
                },
                "{url} should have been refused"
            );
            assert_eq!(
                transport.calls().len(),
                0,
                "{url} reached the transport; the SSRF guard did not hold"
            );
        }
    }
}

#[tokio::test]
async fn a_refused_url_is_recorded_on_the_row_without_being_reproduced() {
    let (_dir, _pool, store) = fixture("refusal-record", "acme", None).await;
    let created = store
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: discord_url(),
            label: String::new(),
            events: None,
        })
        .await
        .expect("create");

    let transport = RecordingTransport::ok();
    let notifier = make_notifier(&store, Arc::clone(&transport));
    let mut tampered = delivery(&format!("https://evil.tld/api/webhooks/1/{TOKEN}"));
    tampered.id = created.id.clone();

    let result = notifier
        .deliver(
            &tampered,
            &WebhookEvent::Published,
            &OrgId("acme".to_owned()),
            &payload(),
            None,
        )
        .await;
    assert!(!result.ok);
    assert_eq!(transport.calls().len(), 0);

    let recorded = store
        .summary(&created.id)
        .await
        .expect("read")
        .expect("row")
        .last_error
        .expect("an error was recorded");
    assert_eq!(recorded, URL_NOT_ALLOWED);
    assert!(!recorded.contains(TOKEN));
}

// ---------------------------------------------------------------------------
// Transport policy: redirects and timeouts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_redirect_is_refused_rather_than_followed() {
    // A 302 to a foreign host is exactly the SSRF escape `redirect: "error"` exists to stop.
    let server = spawn_server(ServerBehaviour::Respond(
        "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n",
    ))
    .await;
    let transport = HttpTransport::with_timeout(Duration::from_secs(2)).expect("client");

    let error = transport
        .post(
            &format!("http://{}/api/webhooks/1/t", server.addr),
            artifact_mcp::integrations::notify::DeliveryRequest {
                content_type: "application/json".to_owned(),
                body: b"{}".to_vec(),
            },
        )
        .await
        .expect_err("a redirect must not be followed");
    assert_eq!(error, REDIRECT_REFUSED);
    assert_eq!(
        server.connections(),
        1,
        "exactly one connection: the redirect target was never contacted"
    );
}

#[tokio::test]
async fn a_hung_endpoint_is_abandoned_at_the_configured_timeout() {
    let server = spawn_server(ServerBehaviour::Hang).await;
    let transport = HttpTransport::with_timeout(Duration::from_millis(300)).expect("client");

    let started = Instant::now();
    let error = transport
        .post(
            &format!("http://{}/api/webhooks/1/t", server.addr),
            artifact_mcp::integrations::notify::DeliveryRequest {
                content_type: "application/json".to_owned(),
                body: b"{}".to_vec(),
            },
        )
        .await
        .expect_err("a hung endpoint must time out");
    let elapsed = started.elapsed();

    assert!(
        error.contains("timed out"),
        "unexpected timeout message: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the timeout did not bound the call: {elapsed:?}"
    );
}

#[tokio::test]
async fn the_production_client_uses_the_reference_four_second_bound() {
    assert_eq!(DELIVERY_TIMEOUT, Duration::from_millis(4000));

    let server = spawn_server(ServerBehaviour::Hang).await;
    let transport = HttpTransport::new().expect("production client");

    let started = Instant::now();
    let error = transport
        .post(
            &format!("http://{}/api/webhooks/1/t", server.addr),
            artifact_mcp::integrations::notify::DeliveryRequest {
                content_type: "application/json".to_owned(),
                body: b"{}".to_vec(),
            },
        )
        .await
        .expect_err("a hung endpoint must time out");
    let elapsed = started.elapsed();

    assert!(error.contains("timed out"), "unexpected message: {error}");
    assert!(
        elapsed >= Duration::from_millis(3_500) && elapsed < Duration::from_secs(8),
        "the default bound is not four seconds: {elapsed:?}"
    );
}

#[tokio::test]
async fn a_non_2xx_status_records_the_reference_message() {
    let (_dir, _pool, store) = fixture("status", "acme", None).await;
    let transport = RecordingTransport::new(Behaviour::Status(500));
    let notifier = make_notifier(&store, Arc::clone(&transport));

    let result = notifier
        .deliver(
            &delivery(&discord_url()),
            &WebhookEvent::Published,
            &OrgId("acme".to_owned()),
            &payload(),
            None,
        )
        .await;
    assert_eq!(
        result,
        DeliveryResult {
            ok: false,
            error: Some("Discord returned HTTP 500".to_owned())
        }
    );
}

// ---------------------------------------------------------------------------
// Detachment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn emit_returns_before_delivery_finishes_and_never_reports_an_error() {
    let (_dir, _pool, store) = fixture("detached", "acme", None).await;
    store
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: discord_url(),
            label: String::new(),
            events: None,
        })
        .await
        .expect("create");

    // A delivery that takes two seconds and then fails: the caller must see neither.
    let transport = RecordingTransport::new(Behaviour::SlowStatus(Duration::from_secs(2), 500));
    let notifier = make_notifier(&store, Arc::clone(&transport));

    let started = Instant::now();
    notifier
        .emit(WebhookEvent::Published, OrgId("acme".to_owned()), payload())
        .await
        .expect("emit never fails the triggering operation");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(500),
        "emit blocked on delivery for {elapsed:?}"
    );

    // The delivery really was started, just not awaited.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(transport.started(), 1);
}

#[tokio::test]
async fn emit_swallows_a_transport_failure_a_missing_org_and_an_unreadable_row() {
    let (_dir, pool, store) = fixture("isolation", "acme", Some(&test_key())).await;
    store
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: discord_url(),
            label: String::new(),
            events: None,
        })
        .await
        .expect("create");

    // 1. The transport fails outright.
    let failing = RecordingTransport::new(Behaviour::Failure("connection refused".to_owned()));
    make_notifier(&store, Arc::clone(&failing))
        .emit(WebhookEvent::Published, OrgId("acme".to_owned()), payload())
        .await
        .expect("a dead endpoint must not fail the publish");

    // 2. No such org: there is simply nothing to deliver.
    make_notifier(&store, RecordingTransport::ok())
        .emit(
            WebhookEvent::Published,
            OrgId("no-such-org".to_owned()),
            payload(),
        )
        .await
        .expect("an unknown org must not fail the publish");

    // 3. The rows cannot even be read — encrypted at rest, no key in this process.
    let keyless = store_with(pool, None);
    let transport = RecordingTransport::ok();
    make_notifier(&keyless, Arc::clone(&transport))
        .emit(WebhookEvent::Published, OrgId("acme".to_owned()), payload())
        .await
        .expect("an unreadable webhook must not fail the publish");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        transport.calls().len(),
        0,
        "a row that cannot be decrypted must never be delivered"
    );
}

// ---------------------------------------------------------------------------
// The admin Test button
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_awaits_the_delivery_and_reports_the_outcome() {
    let (_dir, _pool, store) = fixture("test-button", "acme", None).await;
    let created = store
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: discord_url(),
            label: String::new(),
            events: None,
        })
        .await
        .expect("create");
    let row = store
        .delivery(&created.id)
        .await
        .expect("read")
        .expect("row");

    let transport = RecordingTransport::ok();
    let notifier = make_notifier(&store, Arc::clone(&transport));
    let result = notifier.test(&row).await.expect("test");
    assert_eq!(
        result,
        DeliveryResult {
            ok: true,
            error: None
        }
    );

    let (url, request) = transport.calls().into_iter().next().expect("one request");
    assert_eq!(url, discord_url());
    assert_eq!(request.content_type, "application/json");
    let body = String::from_utf8(request.body).expect("utf-8 body");
    assert_eq!(
        body,
        concat!(
            r#"{"embeds":[{"color":3120756,"author":{"name":"acme"},"title":"Webhook test","#,
            r#""url":"http://localhost:3480","fields":["#,
            r#"{"name":"Publisher","value":"Artifact Index","inline":true},"#,
            r#"{"name":"Category","value":"Notifications","inline":true},"#,
            r#"{"name":"Revision","value":"1","inline":true},"#,
            r#"{"name":"Size","value":"0 B","inline":true}],"#,
            r#""description":"Published artifact"}]}"#
        )
    );

    assert!(
        store
            .summary(&created.id)
            .await
            .expect("read")
            .expect("row")
            .last_ok_at
            .is_some()
    );
}

#[tokio::test]
async fn test_reports_a_webhook_with_no_url_without_sending_anything() {
    let (_dir, _pool, store) = fixture("test-empty", "acme", None).await;
    let transport = RecordingTransport::ok();
    let notifier = make_notifier(&store, Arc::clone(&transport));

    let result = notifier.test(&delivery("")).await.expect("test");
    assert_eq!(
        result,
        DeliveryResult {
            ok: false,
            error: Some(UNKNOWN_WEBHOOK.to_owned())
        }
    );
    assert_eq!(transport.calls().len(), 0);
}

// ---------------------------------------------------------------------------
// Embed goldens
// ---------------------------------------------------------------------------

fn embed_json(event: &WebhookEvent) -> String {
    serde_json::to_string(&build_embed(event, &OrgId("acme".to_owned()), &payload()))
        .expect("serialize")
}

#[test]
fn published_embed_is_byte_exact() {
    assert_eq!(
        embed_json(&WebhookEvent::Published),
        concat!(
            r#"{"embeds":[{"color":3120756,"author":{"name":"acme"},"title":"Quarterly report","#,
            r#""url":"https://example.test/abc123def456","fields":["#,
            r#"{"name":"Publisher","value":"Ada Lovelace","inline":true},"#,
            r#"{"name":"Category","value":"Reports","inline":true},"#,
            r#"{"name":"Revision","value":"3","inline":true},"#,
            r#"{"name":"Size","value":"2.0 KB","inline":true}],"#,
            r#""description":"The numbers are in."}]}"#
        )
    );
}

#[test]
fn every_event_uses_its_own_color_and_shape() {
    assert!(embed_json(&WebhookEvent::Updated).starts_with(r#"{"embeds":[{"color":3900150,"#));
    assert!(embed_json(&WebhookEvent::Restored).starts_with(r#"{"embeds":[{"color":9133302,"#));
    assert!(embed_json(&WebhookEvent::Deleted).starts_with(r#"{"embeds":[{"color":14427686,"#));

    assert_eq!(
        embed_json(&WebhookEvent::Feedback),
        concat!(
            r#"{"embeds":[{"color":16096779,"author":{"name":"acme"},"title":"Quarterly report","#,
            r#""url":"https://example.test/abc123def456","fields":["#,
            r#"{"name":"Viewer","value":"viewer@example.test","inline":true},"#,
            r#"{"name":"Revision","value":"3","inline":true}],"#,
            r#""description":"Looks good to me"}]}"#
        )
    );
    assert_eq!(
        embed_json(&WebhookEvent::Resolved),
        concat!(
            r#"{"embeds":[{"color":1483594,"author":{"name":"acme"},"title":"Quarterly report","#,
            r#""url":"https://example.test/abc123def456","fields":["#,
            r#"{"name":"Resolver","value":"Grace Hopper","inline":true}],"#,
            r#""description":"Feedback resolved"}]}"#
        )
    );
}

#[tokio::test]
async fn a_preview_switches_to_multipart_and_appends_the_image_key() {
    let (_dir, _pool, store) = fixture("preview", "acme", None).await;
    let transport = RecordingTransport::ok();
    let notifier = make_notifier(&store, Arc::clone(&transport));
    let png = vec![0x89, 0x50, 0x4E, 0x47];

    let result = notifier
        .deliver(
            &delivery(&discord_url()),
            &WebhookEvent::Published,
            &OrgId("acme".to_owned()),
            &payload(),
            Some(&png),
        )
        .await;
    assert!(result.ok);

    let (_, request) = transport.calls().into_iter().next().expect("one request");
    assert!(
        request
            .content_type
            .starts_with("multipart/form-data; boundary="),
        "unexpected content type: {}",
        request.content_type
    );
    let body = String::from_utf8_lossy(&request.body).into_owned();
    assert!(
        body.contains(
            r#""description":"The numbers are in.","image":{"url":"attachment://preview.png"}}]}"#
        ),
        "the image key must be appended after description: {body}"
    );
    assert!(body.contains(r#"name="payload_json""#));
    assert!(body.contains(r#"name="files[0]"; filename="preview.png""#));
    assert!(body.contains("Content-Type: image/png"));

    // An empty preview, or an event that does not take one, stays on the JSON path.
    for (event, preview) in [
        (WebhookEvent::Published, Vec::new()),
        (WebhookEvent::Deleted, png.clone()),
        (WebhookEvent::Feedback, png.clone()),
    ] {
        let transport = RecordingTransport::ok();
        let sink = make_notifier(&store, Arc::clone(&transport));
        sink.deliver(
            &delivery(&discord_url()),
            &event,
            &OrgId("acme".to_owned()),
            &payload(),
            Some(&preview),
        )
        .await;
        let (_, request) = transport.calls().into_iter().next().expect("one request");
        assert_eq!(request.content_type, "application/json", "{event:?}");
    }
}

#[test]
fn the_multipart_framing_is_stable() {
    let request = multipart_request(r#"{"embeds":[]}"#, &[1, 2, 3], "TESTBOUNDARY");
    assert_eq!(
        String::from_utf8_lossy(&request.body),
        concat!(
            "--TESTBOUNDARY\r\n",
            "Content-Disposition: form-data; name=\"payload_json\"\r\n\r\n",
            "{\"embeds\":[]}\r\n",
            "--TESTBOUNDARY\r\n",
            "Content-Disposition: form-data; name=\"files[0]\"; filename=\"preview.png\"\r\n",
            "Content-Type: image/png\r\n\r\n",
            "\u{1}\u{2}\u{3}\r\n",
            "--TESTBOUNDARY--\r\n"
        )
    );
}

#[tokio::test]
async fn no_delivery_surface_reproduces_the_webhook_url() {
    let (_dir, _pool, store) = fixture("notify-leakage", "acme", None).await;
    let transport = RecordingTransport::new(Behaviour::Failure("connection refused".to_owned()));
    let notifier = make_notifier(&store, Arc::clone(&transport));
    let row = delivery(&discord_url());

    let result = notifier
        .deliver(
            &row,
            &WebhookEvent::Published,
            &OrgId("acme".to_owned()),
            &payload(),
            None,
        )
        .await;

    let rendered = format!(
        "{:?} | {:?} | {:?} | {:?}",
        notifier,
        transport,
        result,
        HttpTransport::new().expect("client")
    );
    assert!(
        !rendered.contains(TOKEN),
        "a delivery surface leaked the webhook token: {rendered}"
    );
    assert!(
        !rendered.contains("/api/webhooks/"),
        "a delivery surface leaked the webhook path: {rendered}"
    );
}
