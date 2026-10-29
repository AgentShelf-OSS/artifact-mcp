use std::sync::Arc;

use artifact_mcp::{
    config::Secret,
    error::AppError,
    model::{EmailAddress, OrgId, Viewer},
    persistence::{db::Database, discord_organization::{OrganizationDiscordStore, RecoveryDestination}},
    security::{audit::MutationAudit, crypto::WebhookUrlProtection},
};

#[tokio::test]
async fn batch_recovery_rejects_invalid_destination_before_audit_or_database_checks() {
    let dir = std::env::temp_dir().join(format!("artifact-mcp-p4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp data directory");
    let pool = Database::open_at(&dir).expect("migrate database");
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
