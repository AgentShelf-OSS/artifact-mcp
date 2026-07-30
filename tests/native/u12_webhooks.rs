//! U12 webhook persistence: CRUD against the real pool in both at-rest modes.
//!
//! The two modes are not "supported" and "legacy" — `WEBHOOK_ENC_KEY` is currently **unset** in
//! production, so the plaintext column is what live rows use today, and the encrypted column is
//! what they will use after the key is set. Both are exercised end to end here.

use artifact_mcp::error::AppError;
use artifact_mcp::integrations::delivery_worker::{WebhookResolutionFailure, WorkerWebhooks};
use artifact_mcp::model::{CreateWebhook, OrgId, WebhookEvent, WebhookId};
use artifact_mcp::persistence::db;
use artifact_mcp::persistence::migrations::mask_webhook_url;
use artifact_mcp::persistence::webhooks::{
    INVALID_URL_MESSAGE, WebhookDeliveryResolutionFailure, mask_url,
};

use crate::u12_support::{fixture, raw_url_columns, seed_org, store_with, test_key};

/// A synthetic token whose last four characters (`wxyz`) are distinct from the rest, so a leak of
/// the whole secret is distinguishable from the four characters masking deliberately keeps.
const TOKEN: &str = "ULTRA-SECRET-WEBHOOK-TOKEN-wxyz";

fn discord_url() -> String {
    format!("https://discord.com/api/webhooks/123456789012345678/{TOKEN}")
}

fn request(org: &str, url: &str) -> CreateWebhook {
    CreateWebhook {
        org: OrgId(org.to_owned()),
        url: url.to_owned(),
        label: "Ops channel".to_owned(),
        events: None,
    }
}

// ---------------------------------------------------------------------------
// Create: the two at-rest layouts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_a_key_the_url_is_stored_verbatim_with_null_cipher_columns() {
    let (_dir, pool, store) = fixture("plaintext-create", "acme", None).await;
    let url = discord_url();

    let summary = store
        .create(request("acme", &url))
        .await
        .expect("create succeeds");

    let (stored, cipher, nonce, tag) = raw_url_columns(&pool, &summary.id.0).await;
    assert_eq!(
        stored, url,
        "the live plaintext configuration stores the URL as-is"
    );
    assert_eq!((cipher, nonce, tag), (None, None, None));

    // The returned shape is masked even though the column is not.
    assert_eq!(summary.url, "https://discord.com…wxyz");
    assert!(!summary.url.contains(TOKEN));
    assert_eq!(summary.url, mask_url(&url));
    assert_eq!(summary.label, "Ops channel");
    assert_eq!(summary.last_ok_at, None);
    assert_eq!(summary.last_error, None);
}

#[tokio::test]
async fn with_a_key_the_column_holds_the_mask_and_the_three_cipher_fields() {
    let (_dir, pool, store) = fixture("encrypted-create", "acme", Some(&test_key())).await;
    let url = discord_url();

    let summary = store
        .create(request("acme", &url))
        .await
        .expect("create succeeds");

    let (stored, cipher, nonce, tag) = raw_url_columns(&pool, &summary.id.0).await;
    assert_eq!(stored, mask_webhook_url(&url));
    assert_eq!(stored, "https://discord.com/…wxyz");
    assert!(!stored.contains(TOKEN));
    assert!(cipher.is_some() && nonce.is_some() && tag.is_some());

    // `publicRow` masks the already-masked column a second time; the slash disappears.
    assert_eq!(summary.url, "https://discord.com…wxyz");
    assert!(!summary.url.contains(TOKEN));

    // The original is still recoverable through the internal delivery path only.
    let delivery = store
        .delivery(&summary.id)
        .await
        .expect("delivery read")
        .expect("row exists");
    assert_eq!(delivery.url, url);
    assert_eq!(delivery.org, OrgId("acme".to_owned()));
}

#[tokio::test]
async fn delivery_returns_the_original_url_in_the_plaintext_configuration_too() {
    let (_dir, _pool, store) = fixture("plaintext-delivery", "acme", None).await;
    let url = discord_url();
    let summary = store.create(request("acme", &url)).await.expect("create");

    let delivery = store
        .delivery(&summary.id)
        .await
        .expect("delivery read")
        .expect("row exists");
    assert_eq!(delivery.url, url);
    assert_eq!(delivery.events.len(), 6);
    assert_eq!(
        store
            .delivery(&WebhookId("missing".to_owned()))
            .await
            .expect("missing"),
        None
    );
}

// ---------------------------------------------------------------------------
// Fail closed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_encrypted_row_read_without_the_key_fails_closed() {
    let (_dir, pool, encrypted) = fixture("fail-closed", "acme", Some(&test_key())).await;
    let url = discord_url();
    let summary = encrypted
        .create(request("acme", &url))
        .await
        .expect("create");

    // Same database, same rows — but the process now has no key, as after a lost secret.
    let keyless = store_with(pool, None);

    let error = keyless
        .delivery(&summary.id)
        .await
        .expect_err("must not hand back the masked column as if it were a URL");
    assert_eq!(error, AppError::Internal);
    assert_eq!(error.to_string(), "internal error");

    let error = keyless
        .for_event(&OrgId("acme".to_owned()), &WebhookEvent::Published)
        .await
        .expect_err("the delivery fan-out fails closed as well");
    assert_eq!(error, AppError::Internal);

    // Listing still works: it never touches the ciphertext.
    let listed = keyless
        .list_for_org(&OrgId("acme".to_owned()))
        .await
        .expect("list without a key");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].url, "https://discord.com…wxyz");
    assert!(!listed[0].url.contains(TOKEN));
}

#[tokio::test]
async fn durable_resolution_is_typed_tenant_bound_and_redacted() {
    let (_dir, pool, encrypted) = fixture("durable-resolution", "acme", Some(&test_key())).await;
    let url = discord_url();
    let created = encrypted
        .create(request("acme", &url))
        .await
        .expect("create encrypted webhook");

    let resolved = encrypted
        .resolve_delivery(&created.id, &OrgId("acme".to_owned()))
        .await
        .expect("same-tenant reference resolves");
    assert_eq!(resolved.id, created.id);
    assert_eq!(resolved.url, url);

    for (id, org) in [
        (WebhookId("missing".to_owned()), OrgId("acme".to_owned())),
        (created.id.clone(), OrgId("other".to_owned())),
        (WebhookId("   ".to_owned()), OrgId("acme".to_owned())),
    ] {
        let failure = encrypted
            .resolve_delivery(&id, &org)
            .await
            .expect_err("invalid durable target is terminal");
        assert_eq!(failure, WebhookDeliveryResolutionFailure::InvalidReference);
        let diagnostic = format!("{failure:?}");
        assert!(!diagnostic.contains(TOKEN));
        assert!(!diagnostic.contains(&url));
    }

    // A lost key/authentication failure is terminal and its diagnostics never materialise the URL.
    let keyless = store_with(pool.clone(), None);
    let failure = keyless
        .resolve_delivery(&created.id, &OrgId("acme".to_owned()))
        .await
        .expect_err("encrypted target without key fails closed");
    assert_eq!(failure, WebhookDeliveryResolutionFailure::DecryptFailed);
    assert!(!format!("{failure:?}").contains(TOKEN));
    assert_eq!(
        WorkerWebhooks::delivery(keyless.as_ref(), &created.id, &OrgId("acme".to_owned()))
            .await
            .expect_err("worker adapter preserves decrypt classification"),
        WebhookResolutionFailure::DecryptFailed
    );

    // A database fault remains retryable instead of being misclassified as a decrypt failure.
    db::interact(&pool, |conn| {
        conn.execute_batch("DROP TABLE org_webhooks")
            .map_err(|_| AppError::Internal)
    })
    .await
    .expect("drop webhook table to force lookup fault");
    let failure = encrypted
        .resolve_delivery(&created.id, &OrgId("acme".to_owned()))
        .await
        .expect_err("database fault");
    assert_eq!(failure, WebhookDeliveryResolutionFailure::Retryable);
    assert!(!format!("{failure:?}").contains(TOKEN));
    assert_eq!(
        WorkerWebhooks::delivery(encrypted.as_ref(), &created.id, &OrgId("acme".to_owned()))
            .await
            .expect_err("worker adapter preserves database retry classification"),
        WebhookResolutionFailure::Retryable
    );
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_rejects_unknown_orgs_and_urls_outside_the_allowlist() {
    let (_dir, _pool, store) = fixture("validation", "acme", None).await;

    let error = store
        .create(request("nope", &discord_url()))
        .await
        .expect_err("unknown org");
    assert_eq!(
        error,
        AppError::Validation("Unknown organization \"nope\".".to_owned())
    );

    for rejected in [
        "http://discord.com/api/webhooks/1/t",
        "https://discord.com.evil.tld/api/webhooks/1/t",
        "https://evil.tld/api/webhooks/1/t",
        "https://169.254.169.254/api/webhooks/1/t",
        "https://discord.com/api/webhook/1/t",
        "https://discord.com@evil.tld/api/webhooks/1/t",
        "",
    ] {
        match store.create(request("acme", rejected)).await {
            Ok(summary) => panic!("{rejected:?} was accepted and stored as {}", summary.url),
            Err(error) => assert_eq!(
                error,
                AppError::Validation(INVALID_URL_MESSAGE.to_owned()),
                "wrong rejection for {rejected:?}"
            ),
        }
    }
}

#[tokio::test]
async fn the_unknown_org_check_runs_before_the_url_check() {
    // `lib/webhooks.js:86-87` checks the org first; a doubly-invalid request must report the org.
    let (_dir, _pool, store) = fixture("order", "acme", None).await;
    let error = store
        .create(request("nope", "http://evil.tld/"))
        .await
        .expect_err("rejected");
    assert_eq!(
        error,
        AppError::Validation("Unknown organization \"nope\".".to_owned())
    );
}

#[tokio::test]
async fn create_defaults_to_every_event_and_truncates_the_label() {
    let (_dir, _pool, store) = fixture("defaults", "acme", None).await;

    let summary = store
        .create(CreateWebhook {
            org: OrgId("  acme  ".to_owned()),
            url: format!("  {}  ", discord_url()),
            label: format!("  {}  ", "L".repeat(200)),
            events: None,
        })
        .await
        .expect("create");
    assert_eq!(summary.label, "L".repeat(80));
    assert_eq!(
        summary.events,
        [
            WebhookEvent::Published,
            WebhookEvent::Updated,
            WebhookEvent::Restored,
            WebhookEvent::Deleted,
            WebhookEvent::Feedback,
            WebhookEvent::Resolved,
        ]
    );

    // An explicit list is de-duplicated in caller order, not sorted.
    let summary = store
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: discord_url(),
            label: String::new(),
            events: Some(vec![
                WebhookEvent::Resolved,
                WebhookEvent::Published,
                WebhookEvent::Resolved,
            ]),
        })
        .await
        .expect("create");
    assert_eq!(
        summary.events,
        [WebhookEvent::Resolved, WebhookEvent::Published]
    );
    assert_eq!(summary.label, "");
}

// ---------------------------------------------------------------------------
// List, update, delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_is_scoped_to_the_org_and_always_masked() {
    let (_dir, pool, store) = fixture("listing", "acme", None).await;
    seed_org(&pool, "other").await;

    let first = store
        .create(request("acme", &discord_url()))
        .await
        .expect("first");
    let second = store
        .create(request(
            "acme",
            "https://discordapp.com/api/webhooks/2/second-abcd",
        ))
        .await
        .expect("second");
    store
        .create(request(
            "other",
            "https://discord.com/api/webhooks/3/other-efgh",
        ))
        .await
        .expect("third");

    let listed = store
        .list_for_org(&OrgId("acme".to_owned()))
        .await
        .expect("list");
    assert_eq!(
        listed.iter().map(|row| row.id.clone()).collect::<Vec<_>>(),
        [first.id.clone(), second.id.clone()],
        "ordered by created_at then id"
    );
    assert_eq!(listed[1].url, "https://discordapp.com…abcd");
    for row in &listed {
        assert!(!row.url.contains(TOKEN));
        assert!(!row.url.contains("/api/webhooks/"));
    }

    assert_eq!(
        store
            .list_for_org(&OrgId("absent".to_owned()))
            .await
            .expect("empty")
            .len(),
        0
    );
}

#[tokio::test]
async fn set_events_updates_within_the_org_only_and_defaults_to_empty() {
    let (_dir, pool, store) = fixture("set-events", "acme", None).await;
    seed_org(&pool, "other").await;
    let created = store
        .create(request("acme", &discord_url()))
        .await
        .expect("create");

    let updated = store
        .set_events(
            &OrgId("acme".to_owned()),
            &created.id,
            &[WebhookEvent::Feedback, WebhookEvent::Feedback],
        )
        .await
        .expect("set events")
        .expect("row matched");
    assert_eq!(updated.events, [WebhookEvent::Feedback]);

    // Clearing every event is legal and leaves the row subscribed to nothing.
    let cleared = store
        .set_events(&OrgId("acme".to_owned()), &created.id, &[])
        .await
        .expect("set events")
        .expect("row matched");
    assert_eq!(cleared.events, Vec::<WebhookEvent>::new());
    assert_eq!(
        store
            .for_event(&OrgId("acme".to_owned()), &WebhookEvent::Published)
            .await
            .expect("fan-out")
            .len(),
        0
    );

    // A foreign org cannot touch the row.
    assert_eq!(
        store
            .set_events(
                &OrgId("other".to_owned()),
                &created.id,
                &[WebhookEvent::Published]
            )
            .await
            .expect("no match"),
        None
    );
    assert_eq!(
        store
            .set_events(
                &OrgId("acme".to_owned()),
                &WebhookId("missing".to_owned()),
                &[WebhookEvent::Published]
            )
            .await
            .expect("no match"),
        None
    );
}

#[tokio::test]
async fn remove_is_scoped_to_the_org() {
    let (_dir, pool, store) = fixture("remove", "acme", None).await;
    seed_org(&pool, "other").await;
    let created = store
        .create(request("acme", &discord_url()))
        .await
        .expect("create");

    assert!(
        !store
            .remove(&OrgId("other".to_owned()), &created.id)
            .await
            .expect("cross-org delete"),
        "a foreign org must not be able to delete the row"
    );
    assert!(
        store
            .remove(&OrgId("acme".to_owned()), &created.id)
            .await
            .expect("delete")
    );
    assert!(
        !store
            .remove(&OrgId("acme".to_owned()), &created.id)
            .await
            .expect("second delete")
    );
}

#[tokio::test]
async fn notification_thread_anchor_prevents_webhook_deletion_or_published_unsubscribe() {
    let (_dir, pool, store) = fixture("notification-thread-anchor", "acme", None).await;
    let created = store
        .create(request("acme", &discord_url()))
        .await
        .expect("create");
    let webhook_id = created.id.0.clone();
    db::interact(&pool, move |conn| {
        conn.execute(
            "INSERT INTO org_discord_discussion_connections \
             (id, org, url, label, strategy, notification_webhook_id, channel_id, guild_id) \
             VALUES ('connection-a', 'acme', '', 'Artifact threads', 'notification_thread', \
                     ?1, '123456789012345678', '323456789012345678')",
            [webhook_id],
        )
        .expect("notification thread connection");
        Ok(())
    })
    .await
    .expect("seed connection");

    assert!(matches!(
        store
            .set_events(
                &OrgId("acme".to_owned()),
                &created.id,
                &[WebhookEvent::Feedback],
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        store.remove(&OrgId("acme".to_owned()), &created.id).await,
        Err(AppError::Conflict(_))
    ));

    let retained = store
        .list_for_org(&OrgId("acme".to_owned()))
        .await
        .expect("retained webhook");
    assert_eq!(retained.len(), 1);
    assert!(retained[0].events.contains(&WebhookEvent::Published));
}

#[tokio::test]
async fn for_event_returns_only_subscribed_rows() {
    let (_dir, _pool, store) = fixture("fan-out", "acme", None).await;
    let subscribed = store
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: discord_url(),
            label: String::new(),
            events: Some(vec![WebhookEvent::Published]),
        })
        .await
        .expect("create");
    store
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: "https://discord.com/api/webhooks/9/other-abcd".to_owned(),
            label: String::new(),
            events: Some(vec![WebhookEvent::Feedback]),
        })
        .await
        .expect("create");

    let matched = store
        .for_event(&OrgId("acme".to_owned()), &WebhookEvent::Published)
        .await
        .expect("fan-out");
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].id, subscribed.id);
    assert_eq!(matched[0].url, discord_url());

    assert_eq!(
        store
            .for_event(&OrgId("acme".to_owned()), &WebhookEvent::Deleted)
            .await
            .expect("fan-out")
            .len(),
        0
    );
}

// ---------------------------------------------------------------------------
// Result recording
// ---------------------------------------------------------------------------

#[tokio::test]
async fn record_result_stamps_success_and_truncates_failures() {
    let (_dir, _pool, store) = fixture("record", "acme", None).await;
    let created = store
        .create(request("acme", &discord_url()))
        .await
        .expect("create");

    store
        .record_result(&created.id, Err("Discord returned HTTP 500".to_owned()))
        .await
        .expect("record failure");
    let after_failure = store
        .summary(&created.id)
        .await
        .expect("read")
        .expect("row exists");
    assert_eq!(
        after_failure.last_error.as_deref(),
        Some("Discord returned HTTP 500")
    );
    assert_eq!(after_failure.last_ok_at, None);

    // An empty message becomes the Node default, and long messages are cut at 500 units.
    store
        .record_result(&created.id, Err(String::new()))
        .await
        .expect("record empty failure");
    assert_eq!(
        store
            .summary(&created.id)
            .await
            .expect("read")
            .expect("row")
            .last_error
            .as_deref(),
        Some("Webhook delivery failed.")
    );
    store
        .record_result(&created.id, Err("E".repeat(900)))
        .await
        .expect("record long failure");
    assert_eq!(
        store
            .summary(&created.id)
            .await
            .expect("read")
            .expect("row")
            .last_error,
        Some("E".repeat(500))
    );

    store
        .record_result(&created.id, Ok(()))
        .await
        .expect("record success");
    let after_success = store
        .summary(&created.id)
        .await
        .expect("read")
        .expect("row exists");
    assert_eq!(after_success.last_error, None);
    assert!(
        after_success.last_ok_at.is_some(),
        "success stamps last_ok_at"
    );

    // Recording against an unknown id is a no-op, not an error.
    store
        .record_result(&WebhookId("missing".to_owned()), Ok(()))
        .await
        .expect("no-op");
}

// ---------------------------------------------------------------------------
// Leakage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_public_surface_or_diagnostic_ever_carries_the_full_url() {
    for key in [None, Some(test_key())] {
        let (_dir, pool, store) = fixture("leakage", "acme", key.as_deref()).await;
        let url = discord_url();
        let summary = store.create(request("acme", &url)).await.expect("create");

        let keyless = store_with(pool, None);
        let reveal_error = keyless.delivery(&summary.id).await.err();

        let rendered = format!(
            "{:?} | {:?} | {} | {:?} | {:?} | {:?} | {}",
            store,
            summary,
            summary.url,
            store
                .list_for_org(&OrgId("acme".to_owned()))
                .await
                .expect("list"),
            reveal_error,
            store
                .create(request("acme", "https://evil.tld/x"))
                .await
                .expect_err("rejected"),
            serde_json::to_string(&summary).expect("serialize the API shape"),
        );

        assert!(
            !rendered.contains(TOKEN),
            "a rendered surface leaked the webhook token: {rendered}"
        );
        assert!(
            !rendered.contains("/api/webhooks/"),
            "a rendered surface leaked the webhook path: {rendered}"
        );
        // The masked form is still useful: it keeps the host and the last four characters.
        assert!(summary.url.starts_with("https://discord.com"));
        assert!(summary.url.ends_with("wxyz"));
    }
}

#[tokio::test]
async fn create_never_names_the_rejected_url_in_its_error() {
    let (_dir, _pool, store) = fixture("error-text", "acme", None).await;
    let secret_looking = format!("https://evil.tld/api/webhooks/1/{TOKEN}");
    let error = store
        .create(request("acme", &secret_looking))
        .await
        .expect_err("rejected");
    assert_eq!(error, AppError::Validation(INVALID_URL_MESSAGE.to_owned()));
    assert!(!error.to_string().contains(TOKEN));
}
