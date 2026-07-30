//! PBI-081 persistence contract: v30 credentials, inherited policy, and exact recovery evidence.

use std::sync::Arc;

use artifact_mcp::{
    config::Secret,
    error::AppError,
    model::{ArtifactId, EmailAddress, OrgId, Viewer},
    persistence::{
        db::{self, Database},
        discord_organization::{
            ArtifactDiscussionOverride, DiscordCredentialReadiness, OrganizationDiscordStore,
            RecoveryDestination, RecoveryState,
        },
        migrations,
    },
    ports::discussions::OrganizationDiscordCredentialService,
    security::audit::MutationAudit,
    security::crypto::WebhookUrlProtection,
};

use crate::u03_support::{TempDataDir, foreign_key_violations, scalar};

const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const AUDIT_KEY: [u8; 32] = [0x81; 32];

fn admin_audit() -> MutationAudit {
    MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress::from("admin@example.test")),
        org: Some(OrgId::from("admin")),
        is_admin: true,
    })
    .expect("admin audit")
}

fn store(
    label: &str,
    fallback: Option<Secret>,
) -> (TempDataDir, db::DbPool, OrganizationDiscordStore) {
    let dir = TempDataDir::new(label);
    let pool = Database::open_at(dir.path()).expect("migrate");
    let protection = Arc::new(WebhookUrlProtection::from_env_value(Some(KEY)).expect("key"));
    let store = OrganizationDiscordStore::new(pool.clone(), protection, fallback);
    (dir, pool, store)
}

async fn seed(pool: &db::DbPool, org: &str, artifact: &str) {
    let org = org.to_owned();
    let artifact = artifact.to_owned();
    db::interact(pool, move |conn| {
        conn.execute("INSERT INTO orgs (name) VALUES (?1)", [&org]).expect("org");
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title) VALUES (?1, 'publisher', ?2, 'Artifact')",
            [&artifact, &org],
        ).expect("artifact");
        Ok(())
    }).await.expect("seed");
}

async fn connection(
    pool: &db::DbPool,
    org: &str,
    id: &str,
    webhook: &str,
    guild: &str,
    channel: &str,
) {
    let (org, id, webhook, guild, channel) = (
        org.to_owned(),
        id.to_owned(),
        webhook.to_owned(),
        guild.to_owned(),
        channel.to_owned(),
    );
    db::interact(pool, move |conn| {
        conn.execute(
            "INSERT INTO org_webhooks (id, org, url, events) VALUES (?1, ?2, 'https://discord.com/api/webhooks/123/secret', 'published')",
            rusqlite::params![webhook, org],
        ).expect("webhook");
        conn.execute(
            "INSERT INTO org_discord_discussion_connections (id, org, url, label, strategy, notification_webhook_id, guild_id, channel_id, notification_provider_webhook_id) \
             VALUES (?1, ?2, '', 'Discord', 'notification_thread', ?3, ?4, ?5, ?4)",
            rusqlite::params![id, org, webhook, guild, channel],
        ).expect("connection");
        Ok(())
    }).await.expect("connection");
}

#[tokio::test]
async fn credential_is_encrypted_redacted_and_failed_rotation_preserves_active_value() {
    let (_dir, pool, store) = store("credential", None);
    seed(&pool, "acme", "artifact-a").await;
    let token = Secret::new("synthetic-token-a");
    assert_eq!(
        store
            .save_validated_credential(&OrgId::from("acme"), token, true)
            .await
            .expect("save"),
        DiscordCredentialReadiness::Configured
    );
    {
        let conn = db::checkout(&pool).expect("checkout");
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT COUNT(*) FROM org_discord_bot_credentials WHERE ciphertext = 'synthetic-token-a'"
            ),
            0
        );
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT COUNT(*) FROM pragma_table_info('org_discord_bot_credentials') WHERE name LIKE '%token%' OR name = 'plaintext'"
            ),
            0
        );
    }

    let error = store
        .save_validated_credential(&OrgId::from("acme"), Secret::new("replacement"), false)
        .await
        .expect_err("invalid rotation");
    assert_eq!(
        error,
        AppError::Validation("Discord bot credential validation failed.".to_owned())
    );
    let resolved = store
        .credential_for_provider(&OrgId::from("acme"))
        .await
        .expect("resolve")
        .expect("configured");
    assert_eq!(resolved.expose(), "synthetic-token-a");
    assert_eq!(format!("{resolved:?}"), "Secret(<redacted>)");
}

#[tokio::test]
async fn policy_inheritance_override_and_fallback_are_tenant_scoped() {
    let (_dir, pool, store) = store("policy", Some(Secret::new("synthetic-fallback")));
    seed(&pool, "acme", "artifact-a").await;
    seed(&pool, "cairn", "artifact-c").await;
    connection(
        &pool,
        "acme",
        "connection-a",
        "webhook-a",
        "123456789012345678",
        "223456789012345678",
    )
    .await;
    connection(
        &pool,
        "cairn",
        "connection-c",
        "webhook-c",
        "323456789012345678",
        "423456789012345678",
    )
    .await;

    store
        .set_outbound_enabled(&OrgId::from("acme"), true)
        .await
        .expect("enable");
    let inherited = store
        .effective_policy(&ArtifactId::from("artifact-a"), &OrgId::from("acme"))
        .await
        .expect("policy");
    assert!(inherited.effective_outbound);
    assert_eq!(
        inherited.credential_readiness,
        DiscordCredentialReadiness::LegacyFallback
    );
    store
        .set_artifact_override(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            ArtifactDiscussionOverride::ArtifactOnly,
        )
        .await
        .expect("opt out");
    assert!(
        !store
            .effective_policy(&ArtifactId::from("artifact-a"), &OrgId::from("acme"))
            .await
            .expect("opt out policy")
            .effective_outbound
    );
    store
        .set_outbound_enabled(&OrgId::from("acme"), false)
        .await
        .expect("disable");
    store
        .set_outbound_enabled(&OrgId::from("acme"), true)
        .await
        .expect("reenable");
    assert_eq!(
        store
            .effective_policy(&ArtifactId::from("artifact-a"), &OrgId::from("acme"))
            .await
            .expect("preserved override")
            .artifact_override,
        ArtifactDiscussionOverride::ArtifactOnly
    );
    store
        .set_artifact_override(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            ArtifactDiscussionOverride::Inherit,
        )
        .await
        .expect("reset");
    assert!(
        store
            .effective_policy(&ArtifactId::from("artifact-a"), &OrgId::from("acme"))
            .await
            .expect("reset policy")
            .effective_outbound
    );
    assert!(
        !store
            .effective_policy(&ArtifactId::from("artifact-c"), &OrgId::from("cairn"))
            .await
            .expect("other tenant")
            .effective_outbound
    );
}

#[tokio::test]
async fn disabling_outbound_policy_requires_neither_a_credential_nor_provider_availability() {
    let (_dir, pool, store) = store("disabled-without-credential", None);
    seed(&pool, "acme", "artifact-a").await;
    let status = store
        .save_validated_credential_and_policy_audited(
            OrgId::from("acme"),
            None,
            false,
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("local disable");
    assert!(!status.outbound_enabled);
    assert_eq!(
        status.credential_readiness,
        DiscordCredentialReadiness::Unconfigured
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT outbound_enabled FROM org_discord_threading_policies WHERE org='acme'"
        ),
        0
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM org_discord_bot_credentials WHERE org='acme'"
        ),
        0
    );
}

#[tokio::test]
async fn malformed_ciphertext_and_deactivation_fail_closed_without_erasing_history() {
    let (_dir, pool, store) = store("cipher", Some(Secret::new("synthetic-fallback")));
    seed(&pool, "acme", "artifact-a").await;
    store
        .save_validated_credential(&OrgId::from("acme"), Secret::new("synthetic-token-a"), true)
        .await
        .expect("save");
    db::interact(&pool, move |conn| {
        conn.execute(
            "UPDATE org_discord_bot_credentials SET ciphertext='not base64' WHERE org='acme'",
            [],
        )
        .expect("corrupt");
        Ok(())
    })
    .await
    .expect("corrupt");
    assert_eq!(
        store
            .credential_for_provider(&OrgId::from("acme"))
            .await
            .expect_err("corrupt fails closed"),
        AppError::Internal
    );
    assert!(
        store
            .deactivate_credential(&OrgId::from("acme"))
            .await
            .expect("deactivate")
    );
    assert_eq!(
        store
            .credential_readiness(&OrgId::from("acme"))
            .await
            .expect("status"),
        DiscordCredentialReadiness::Deactivated
    );
    assert_eq!(
        store
            .credential_for_provider(&OrgId::from("acme"))
            .await
            .expect("deactivated resolve"),
        None,
        "an explicit removal/deactivation must not fall through to the legacy process credential"
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM org_discord_bot_credentials WHERE org='acme'"
        ),
        1
    );
}

#[tokio::test]
async fn exact_recovery_is_bound_to_connection_destination_and_never_stores_bodies() {
    let (_dir, pool, store) = store("recovery", None);
    seed(&pool, "acme", "artifact-a").await;
    seed(&pool, "cairn", "artifact-c").await;
    connection(
        &pool,
        "acme",
        "connection-a",
        "webhook-a",
        "123456789012345678",
        "223456789012345678",
    )
    .await;
    connection(
        &pool,
        "cairn",
        "connection-c",
        "webhook-c",
        "323456789012345678",
        "423456789012345678",
    )
    .await;
    let destination = RecoveryDestination {
        connection_id: "connection-a".into(),
        notification_webhook_id: "webhook-a".into(),
        provider_webhook_id: "123456789012345678".into(),
        guild_id: "123456789012345678".into(),
        channel_id: "223456789012345678".into(),
    };
    store
        .schedule_recovery(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            destination.clone(),
            "https://artifact.example.test/a".into(),
        )
        .await
        .expect("schedule");
    let record = store
        .complete_recovery(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            RecoveryState::Recovered,
            Some("923456789012345678".into()),
        )
        .await
        .expect("exact match");
    assert_eq!(record.state, RecoveryState::Recovered);
    assert_eq!(
        record.recovered_message_id.as_deref(),
        Some("923456789012345678")
    );
    assert_eq!(
        store
            .recovered_anchor_message(
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &RecoveryDestination {
                    connection_id: "connection-a".into(),
                    notification_webhook_id: "webhook-a".into(),
                    provider_webhook_id: "123456789012345678".into(),
                    guild_id: "123456789012345678".into(),
                    channel_id: "223456789012345678".into()
                }
            )
            .await
            .expect("bound resolver")
            .as_deref(),
        Some("923456789012345678")
    );
    let cross_tenant = store
        .schedule_recovery(
            &ArtifactId::from("artifact-c"),
            &OrgId::from("cairn"),
            destination,
            "https://artifact.example.test/c".into(),
        )
        .await
        .expect_err("wrong destination");
    assert_eq!(
        cross_tenant,
        AppError::Validation(
            "Discord recovery destination does not match the organization connection.".to_owned()
        )
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM pragma_table_info('discord_notification_anchor_recoveries') WHERE name LIKE '%body%'"
        ),
        0
    );
    assert_eq!(foreign_key_violations(&conn), 0);
}

#[test]
fn frozen_v29_fixture_upgrades_and_reopens_without_mutating_source() {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance/fixtures/historical/boundary-v29/artifacts.db");
    let source_bytes = std::fs::read(&source).expect("read immutable v29 fixture");
    let dir = TempDataDir::new("frozen-v29-upgrade");
    std::fs::copy(&source, db::database_path(dir.path())).expect("copy immutable fixture");
    let pool = Database::open_at(dir.path()).expect("upgrade frozen v29 fixture");
    let conn = db::checkout(&pool).expect("checkout upgraded fixture");
    assert_eq!(migrations::current_version(&conn).expect("version"), 31);
    assert_eq!(scalar::<String>(&conn, "PRAGMA integrity_check"), "ok");
    drop(conn);
    drop(pool);
    let reopened = Database::open_at(dir.path()).expect("reopen upgraded v29 fixture");
    let conn = db::checkout(&reopened).expect("checkout reopened fixture");
    assert_eq!(migrations::current_version(&conn).expect("version"), 31);
    assert_eq!(foreign_key_violations(&conn), 0);
    assert_eq!(
        std::fs::read(source).expect("re-read immutable fixture"),
        source_bytes
    );
}

#[test]
fn frozen_v29_cross_tenant_corruption_fails_closed_before_v31_commit() {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance/fixtures/historical/boundary-v29/artifacts.db");
    let dir = TempDataDir::new("frozen-v29-corrupt");
    let destination = db::database_path(dir.path());
    std::fs::copy(source, &destination).expect("copy immutable v29 fixture");
    {
        let conn = rusqlite::Connection::open(&destination).expect("open raw v29 fixture");
        conn.pragma_update(None, "foreign_keys", "OFF")
            .expect("disable foreign keys for corruption fixture");
        conn.execute("INSERT INTO orgs (name) VALUES ('other')", [])
            .expect("other org");
        conn.execute(
            "INSERT INTO feedback \
             (id, artifact_id, org, viewer_email, body, artifact_revision) \
             VALUES ('v29-cross-tenant', 'singleb29', 'other', \
                     'foreign@example.test', 'corrupt', 1)",
            [],
        )
        .expect("seed cross-tenant corruption");
    }
    assert!(
        Database::open_at(dir.path()).is_err(),
        "v31 must reject v29 feedback whose artifact and organization do not match"
    );
    let conn = rusqlite::Connection::open(destination).expect("reopen rejected fixture");
    assert_eq!(
        migrations::current_version(&conn).expect("version"),
        30,
        "the append-only runner may commit the independent v30 credential boundary first"
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM feedback WHERE id='v29-cross-tenant'"
        ),
        1,
        "the failed migration must preserve corruption evidence"
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master \
              WHERE type='table' AND name IN \
                ('org_discord_bot_credentials','discord_inbound_events')"
        ),
        1,
        "v30 remains valid, while the rejecting v31 inbound schema is wholly absent"
    );
}

#[test]
fn v30_records_cascade_from_their_existing_owners() {
    let dir = TempDataDir::new("v29-upgrade");
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance/fixtures/historical/boundary-v29/artifacts.db");
    std::fs::copy(source, db::database_path(dir.path())).expect("copy immutable v29 fixture");
    let reopened = Database::open_at(dir.path()).expect("upgrade v29 fixture");
    let conn = db::checkout(&reopened).expect("checkout");
    assert_eq!(migrations::current_version(&conn).expect("version"), 31);
    conn.execute("INSERT INTO orgs (name) VALUES ('acme')", [])
        .expect("org");
    conn.execute("INSERT INTO artifacts (id, client_id, org, title) VALUES ('artifact-a', 'publisher', 'acme', 'Artifact')", []).expect("artifact");
    conn.execute("INSERT INTO artifact_discussion_overrides (artifact_id, org, mode) VALUES ('artifact-a', 'acme', 'artifact_only')", []).expect("override");
    conn.execute("INSERT INTO org_discord_bot_credentials (org, ciphertext, nonce, tag) VALUES ('acme', 'a', 'b', 'c')", []).expect("credential");
    // Artifacts predate the organization registry foreign key, so their normal lifecycle owns
    // override cleanup; organization deletion owns credential cleanup.
    conn.execute("DELETE FROM artifacts WHERE id='artifact-a'", [])
        .expect("artifact cascade");
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM artifact_discussion_overrides"),
        0
    );
    conn.execute("DELETE FROM orgs WHERE name='acme'", [])
        .expect("organization cascade");
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM org_discord_bot_credentials"),
        0
    );
    assert_eq!(foreign_key_violations(&conn), 0);
}

#[test]
fn v30_backfills_deployed_pbi079_opt_out_without_widening_legacy_mirror_policy() {
    let dir = TempDataDir::new("v29-pbi079-intent");
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance/fixtures/historical/boundary-v29/artifacts.db");
    let destination = db::database_path(dir.path());
    std::fs::copy(source, &destination).expect("copy immutable v29 fixture");
    {
        let conn = rusqlite::Connection::open(&destination).expect("open raw v29 fixture");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("enable foreign keys");
        conn.execute_batch(
            "INSERT INTO orgs (name) VALUES ('acme');
             INSERT INTO artifacts (id, client_id, org, title) VALUES
               ('artifact-opt-out', 'publisher', 'acme', 'Opt out'),
               ('artifact-mirror', 'publisher', 'acme', 'Mirror');
             INSERT INTO org_discord_discussion_connections (id, org, url, label)
               VALUES ('legacy-connection', 'acme', 'https://discord.com/api/webhooks/123/secret', 'Legacy');
             INSERT INTO artifact_discussions
               (artifact_id, org, provider, mode, connection_org, connection_id, state, generation)
               VALUES
                 ('artifact-opt-out', 'acme', 'discord', 'artifact_only', 'acme', 'legacy-connection', 'paused', 1),
                 ('artifact-mirror', 'acme', 'discord', 'discord_mirror', 'acme', 'legacy-connection', 'pending', 1);",
        )
        .expect("seed deployed v29 intent");
    }
    let upgraded = Database::open_at(dir.path()).expect("upgrade v29 intent");
    let conn = db::checkout(&upgraded).expect("checkout upgraded intent");
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT mode FROM artifact_discussion_overrides WHERE artifact_id='artifact-opt-out' AND org='acme'"
        ),
        "artifact_only"
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM org_discord_threading_policies WHERE org='acme'"
        ),
        0,
        "a legacy per-artifact mirror must not become a new inherited org policy"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT mode FROM artifact_discussions WHERE artifact_id='artifact-mirror'"
        ),
        "discord_mirror",
        "the deployed explicit PBI-079 mirror remains available for primary's legacy resolver"
    );
}
