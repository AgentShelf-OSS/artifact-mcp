use std::sync::Arc;

use artifact_mcp::{
    config::Secret,
    error::AppError,
    model::{EmailAddress, OrgId, Viewer},
    persistence::{
        db::Database,
        discord_organization::{OrganizationDiscordStore, RecoveryDestination},
    },
    security::{audit::MutationAudit, crypto::WebhookUrlProtection},
};

#[tokio::test]
async fn batch_recovery_rejects_invalid_destination_before_audit_or_database_checks() {
    let dir = std::env::temp_dir().join(format!("artifact-mcp-p4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp data directory");
    let pool = Database::open_at(&dir).expect("migrate database");
    // The database check reports the same validation message as the preflight, so remove the
    // table it reads: if the preflight did not answer first, the result would be `Internal`.
    pool.get()
        .expect("checkout setup connection")
        .execute_batch("DROP TABLE org_discord_discussion_connections")
        .expect("make the database destination check fail");
    let store = OrganizationDiscordStore::new(
        pool,
        Arc::new(WebhookUrlProtection::Plaintext),
        None::<Secret>,
    );
    let audit = MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress::from("admin@example.test")),
        org: Some(OrgId::from("admin")),
        is_admin: true,
    })
    .expect("admin audit");

    let error = store
        .queue_recoveries_for_org_audited(
            OrgId::from("missing-org"),
            RecoveryDestination {
                connection_id: String::new(),
                notification_webhook_id: String::new(),
                provider_webhook_id: String::new(),
                guild_id: String::new(),
                channel_id: String::new(),
            },
            "not a base URL".to_owned(),
            audit,
            [0x44; 32],
        )
        .await
        .expect_err("preflight must reject before target lookup");
    assert_eq!(
        error,
        AppError::Validation(
            "Discord recovery destination does not match the organization connection.".to_owned()
        )
    );
    drop(store);
    std::fs::remove_dir_all(dir).expect("remove temp data directory");
}

#[tokio::test]
async fn batch_recovery_preflight_validation_wins_over_cross_org_authorization() {
    let dir =
        std::env::temp_dir().join(format!("artifact-mcp-p4-preflight-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp data directory");
    let pool = Database::open_at(&dir).expect("migrate database");
    let store = OrganizationDiscordStore::new(
        pool,
        Arc::new(WebhookUrlProtection::Plaintext),
        None::<Secret>,
    );
    let audit = MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress::from("member@example.test")),
        org: Some(OrgId::from("member-org")),
        is_admin: false,
    })
    .expect("member audit");

    let error = store
        .queue_recoveries_for_org_audited(
            OrgId::from("target-org"),
            RecoveryDestination {
                connection_id: String::new(),
                notification_webhook_id: String::new(),
                provider_webhook_id: String::new(),
                guild_id: String::new(),
                channel_id: String::new(),
            },
            "https://example.test".to_owned(),
            audit,
            [0x45; 32],
        )
        .await
        .expect_err("preflight must run before audit authorization");
    assert_eq!(
        error,
        AppError::Validation(
            "Discord recovery destination does not match the organization connection.".to_owned()
        )
    );
    drop(store);
    std::fs::remove_dir_all(dir).expect("remove temp data directory");
}

#[tokio::test]
async fn batch_recovery_authorization_wins_before_a_later_database_failure() {
    let dir = std::env::temp_dir().join(format!(
        "artifact-mcp-p4-authorization-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp data directory");
    let pool = Database::open_at(&dir).expect("migrate database");
    let store = OrganizationDiscordStore::new(
        pool.clone(),
        Arc::new(WebhookUrlProtection::Plaintext),
        None::<Secret>,
    );
    let connection_id = "connection-a";
    let webhook_id = "webhook-a";
    let provider_webhook_id = "123456789012345678";
    let guild_id = "223456789012345678";
    let channel_id = "323456789012345678";
    {
        let connection = pool.get().expect("checkout setup connection");
        connection
            .execute("INSERT INTO orgs (name) VALUES ('target-org')", [])
            .expect("insert target organization");
        connection
            .execute(
                "INSERT INTO org_webhooks (id, org, url, events) \
                 VALUES (?1, 'target-org', 'https://discord.com/api/webhooks/123/secret', 'published')",
                [webhook_id],
            )
            .expect("insert notification webhook");
        connection
            .execute(
                "INSERT INTO org_discord_discussion_connections \
                 (id, org, url, label, strategy, notification_webhook_id, guild_id, channel_id, notification_provider_webhook_id) \
                 VALUES (?1, 'target-org', '', 'Discord', 'notification_thread', ?2, ?4, ?5, ?3)",
                rusqlite::params![connection_id, webhook_id, provider_webhook_id, guild_id, channel_id],
            )
            .expect("insert matching connection");
        connection
            .execute_batch("DROP TABLE artifacts")
            .expect("make the later artifact query fail");
    }
    let audit = MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress::from("member@example.test")),
        org: Some(OrgId::from("member-org")),
        is_admin: false,
    })
    .expect("member audit");

    let error = store
        .queue_recoveries_for_org_audited(
            OrgId::from("target-org"),
            RecoveryDestination {
                connection_id: connection_id.to_owned(),
                notification_webhook_id: webhook_id.to_owned(),
                provider_webhook_id: provider_webhook_id.to_owned(),
                guild_id: guild_id.to_owned(),
                channel_id: channel_id.to_owned(),
            },
            "https://example.test".to_owned(),
            audit,
            [0x46; 32],
        )
        .await
        .expect_err("cross-org authorization must run before the artifact query");
    assert_eq!(
        error,
        AppError::Forbidden("administrator audit context required".to_owned())
    );
    drop(store);
    std::fs::remove_dir_all(dir).expect("remove temp data directory");
}
