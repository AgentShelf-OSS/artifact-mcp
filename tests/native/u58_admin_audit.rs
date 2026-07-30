//! PBI-058 production-store coverage for privileged configuration and public-share audit events.
//!
//! These tests deliberately call the pooled adapters used by `main`, not route doubles. They prove
//! target-tenant attribution, secret minimization, no-op suppression, and rollback when the
//! authenticated audit chain rejects an append.

use std::{path::PathBuf, process::Command, sync::Arc};

use artifact_mcp::config::{
    Clock, FixedClock, IdSource, SeededRandom, SequentialIdSource, is_valid_webhook_id,
};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{
    ArtifactId, ArtifactMeta, ClientId, CreateOrganization, CreatePublisherKey, CreateShare,
    CreateWebhook, EmailAddress, OrgId, Timestamp, Viewer, WebhookId,
};
use artifact_mcp::persistence::{
    keys::KeyStore,
    orgs::OrgStore,
    shares::{create_audited_pooled, revoke_audited_pooled},
    webhooks::{EVENTS, WebhookStore},
};
use artifact_mcp::security::{
    access::{AccessPolicy, AuthorizedArtifact},
    audit::{MutationAudit, initialize_head},
    crypto::WebhookUrlProtection,
};

use crate::u09_support::{TestDb, seed_org};

const AUDIT_KEY: [u8; 32] = [0x58; 32];
const WRONG_AUDIT_KEY: [u8; 32] = [0x59; 32];
const ACTOR: &str = "admin@example.test";

#[derive(Debug)]
struct AuditRow {
    tenant: String,
    actor_id: String,
    operation: String,
    target_type: String,
    target_id: String,
    result: String,
    request_id: String,
    canonical: Vec<u8>,
}

fn admin_audit() -> MutationAudit {
    MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress(ACTOR.to_owned())),
        org: Some(OrgId("admin".to_owned())),
        is_admin: true,
    })
    .expect("verified administrator audit context")
}

fn seed_and_seal(db: &TestDb, org: Option<&str>) {
    let mut conn = db.conn();
    if let Some(org) = org {
        seed_org(&mut conn, org);
    }
    initialize_head(&conn, &AUDIT_KEY).expect("seal audit head");
}

fn audit_rows(db: &TestDb) -> Vec<AuditRow> {
    let conn = db.conn();
    let mut statement = conn
        .prepare(
            "SELECT tenant, actor_id, operation, target_type, target_id, result, request_id, \
                    canonical \
             FROM security_audit_events ORDER BY sequence",
        )
        .expect("prepare audit rows");
    statement
        .query_map([], |row| {
            Ok(AuditRow {
                tenant: row.get(0)?,
                actor_id: row.get(1)?,
                operation: row.get(2)?,
                target_type: row.get(3)?,
                target_id: row.get(4)?,
                result: row.get(5)?,
                request_id: row.get(6)?,
                canonical: row.get(7)?,
            })
        })
        .expect("query audit rows")
        .map(|row| row.expect("read audit row"))
        .collect()
}

fn assert_not_in_canonical(rows: &[AuditRow], secret: &str) {
    assert!(
        rows.iter()
            .all(|row| !String::from_utf8_lossy(&row.canonical).contains(secret)),
        "audit canonical bytes retained secret material"
    );
}

fn key_request(client_id: &str) -> CreatePublisherKey {
    CreatePublisherKey {
        client_id: ClientId(client_id.to_owned()),
        org: OrgId("acme".to_owned()),
        label: "Production publisher".to_owned(),
        role: "author".to_owned(),
        owner_email: None,
    }
}

fn webhook_request(url: &str) -> CreateWebhook {
    CreateWebhook {
        org: OrgId("acme".to_owned()),
        url: url.to_owned(),
        label: "Audit delivery".to_owned(),
        events: None,
    }
}

fn webhook_store(db: &TestDb) -> WebhookStore {
    let ids: Arc<dyn IdSource> = Arc::new(SequentialIdSource::default());
    WebhookStore::new(
        db.pool().clone(),
        ids,
        Arc::new(WebhookUrlProtection::Plaintext),
    )
}

fn artifact_meta() -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId("auditart0001".to_owned()),
        client_id: ClientId("publisher-1".to_owned()),
        org: OrgId("acme".to_owned()),
        title: "Audited artifact".to_owned(),
        description: String::new(),
        bytes: 0,
        created_at: Timestamp("2026-01-01T00:00:00Z".to_owned()),
        updated_at: Timestamp("2026-01-01T00:00:00Z".to_owned()),
        uploader_label: String::new(),
        owner_email: None,
        is_bundle: false,
        entry: String::new(),
        revision: 1,
        category: String::new(),
        hidden: false,
        body_sha256: String::new(),
    }
}

fn seed_artifact(db: &TestDb) {
    let meta = artifact_meta();
    db.conn()
        .execute(
            "INSERT INTO artifacts (id, client_id, org, title) VALUES (?1, ?2, ?3, ?4)",
            (&meta.id.0, &meta.client_id.0, &meta.org.0, &meta.title),
        )
        .expect("seed artifact");
}

fn authorize_artifact() -> AuthorizedArtifact {
    AccessPolicy::authorize_viewer(
        &Viewer {
            email: Some(EmailAddress(ACTOR.to_owned())),
            org: Some(OrgId("admin".to_owned())),
            is_admin: true,
        },
        Some(artifact_meta()),
    )
    .expect("administrator may manage target artifact")
}

#[test]
fn node_and_rust_accept_only_the_persisted_webhook_id_shape() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cases = serde_json::json!([
        "wh0000000001",
        "000000000000",
        "a-real-webhook-token",
        "https://discord.com/api/webhooks/1/secret",
        "wh000000000l",
        "WH0000000001"
    ]);
    let driver = r#"
const root = process.argv[1];
const values = JSON.parse(process.argv[2]);
import(`file://${root}/lib/audit.js`)
  .then(({ isValidAuditWebhookId }) => {
    process.stdout.write(JSON.stringify(values.map((value) => isValidAuditWebhookId(value))));
  })
  .catch((error) => { console.error(error); process.exit(1); });
"#;
    let output = Command::new("node")
        .current_dir(&root)
        .arg("-e")
        .arg(driver)
        .arg(&root)
        .arg(cases.to_string())
        .output()
        .expect("run Node audit validator");
    assert!(
        output.status.success(),
        "Node audit validator failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let node: Vec<bool> =
        serde_json::from_slice(&output.stdout).expect("Node validator emitted JSON booleans");
    let rust = cases
        .as_array()
        .expect("case array")
        .iter()
        .map(|value| {
            is_valid_webhook_id(value.as_str().expect("string webhook identifier fixture"))
        })
        .collect::<Vec<_>>();
    assert_eq!(node, rust, "Node and Rust webhook-id guards drifted");
    assert_eq!(rust, [true, true, false, false, false, false]);
}

#[tokio::test]
async fn key_mutations_are_atomic_target_scoped_redacted_and_noop_safe() {
    let db = TestDb::new("u58-key-audit");
    seed_and_seal(&db, Some("acme"));
    let store = KeyStore::with_random(
        db.pool().clone(),
        Arc::new(SeededRandom::new(0x5800_0000_0000_0001)),
    );

    let created = store
        .create_key_audited(key_request("publisher-1"), admin_audit(), AUDIT_KEY)
        .await
        .expect("create audited key");
    let rows = audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].tenant.as_str(),
            rows[0].actor_id.as_str(),
            rows[0].operation.as_str(),
            rows[0].target_id.as_str(),
        ),
        ("acme", ACTOR, "key.create", "publisher-1")
    );
    assert_not_in_canonical(&rows, &created.secret);

    store
        .create_key_audited(key_request("publisher-1"), admin_audit(), AUDIT_KEY)
        .await
        .expect_err("duplicate is rejected");
    assert_eq!(audit_rows(&db).len(), 1, "rejection is not ledgered");

    assert!(
        store
            .revoke_key_audited(ClientId("publisher-1".to_owned()), admin_audit(), AUDIT_KEY,)
            .await
            .expect("first revoke")
    );
    assert!(
        !store
            .revoke_key_audited(ClientId("publisher-1".to_owned()), admin_audit(), AUDIT_KEY,)
            .await
            .expect("second revoke")
    );
    assert_eq!(audit_rows(&db).len(), 2, "no-op retry has no event");

    let rollback = TestDb::new("u58-key-audit-rollback");
    seed_and_seal(&rollback, Some("acme"));
    let rollback_store = KeyStore::with_random(
        rollback.pool().clone(),
        Arc::new(SeededRandom::new(0x5800_0000_0000_0002)),
    );
    assert_eq!(
        rollback_store
            .create_key_audited(
                key_request("publisher-rollback"),
                admin_audit(),
                WRONG_AUDIT_KEY,
            )
            .await,
        Err(AppError::Internal)
    );
    let conn = rollback.conn();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM api_keys WHERE client_id='publisher-rollback'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count keys"),
        0
    );
    assert!(audit_rows(&rollback).is_empty());
}

#[tokio::test]
async fn org_mutations_are_atomic_target_scoped_redacted_and_noop_safe() {
    let db = TestDb::new("u58-org-audit");
    seed_and_seal(&db, None);
    let store = OrgStore::new(db.pool().clone());
    let domain = "tenant-secret.example";

    store
        .create_org_audited(
            CreateOrganization {
                name: OrgId("acme".to_owned()),
                label: "Acme".to_owned(),
                domain: Some(domain.to_owned()),
            },
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("create audited org");
    let rows = audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].tenant.as_str(),
            rows[0].actor_id.as_str(),
            rows[0].operation.as_str(),
            rows[0].target_id.as_str(),
        ),
        ("acme", ACTOR, "org.create", "acme")
    );
    assert_not_in_canonical(&rows, domain);

    assert_eq!(
        store
            .add_category_audited(
                OrgId("acme".to_owned()),
                "Design".to_owned(),
                admin_audit(),
                AUDIT_KEY,
            )
            .await
            .expect("add category"),
        "Design"
    );
    assert_eq!(
        store
            .add_category_audited(
                OrgId("acme".to_owned()),
                "Design".to_owned(),
                admin_audit(),
                AUDIT_KEY,
            )
            .await
            .expect("repeat category"),
        "Design"
    );
    assert!(
        !store
            .delete_org_audited(OrgId("missing".to_owned()), admin_audit(), AUDIT_KEY,)
            .await
            .expect("missing org")
    );
    assert_eq!(audit_rows(&db).len(), 2, "no-op retries have no events");

    let rollback = TestDb::new("u58-org-audit-rollback");
    seed_and_seal(&rollback, None);
    let rollback_store = OrgStore::new(rollback.pool().clone());
    assert_eq!(
        rollback_store
            .create_org_audited(
                CreateOrganization {
                    name: OrgId("acme".to_owned()),
                    label: "Acme".to_owned(),
                    domain: Some(domain.to_owned()),
                },
                admin_audit(),
                WRONG_AUDIT_KEY,
            )
            .await,
        Err(AppError::Internal)
    );
    {
        let conn = rollback.conn();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM orgs WHERE name='acme'", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count orgs"),
            0
        );
    }
    assert!(audit_rows(&rollback).is_empty());

    let category_rollback = TestDb::new("u58-category-audit-rollback");
    seed_and_seal(&category_rollback, Some("acme"));
    let category_store = OrgStore::new(category_rollback.pool().clone());
    assert_eq!(
        category_store
            .add_category_audited(
                OrgId("acme".to_owned()),
                "Audit must commit".to_owned(),
                admin_audit(),
                WRONG_AUDIT_KEY,
            )
            .await,
        Err(AppError::Internal),
        "a bad audit key rejects the category registry write"
    );
    let conn = category_rollback.conn();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM org_categories WHERE org='acme' AND name='Audit must commit'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count rolled-back categories"),
        0,
        "the category registry write rolls back when its audit append fails"
    );
    assert!(audit_rows(&category_rollback).is_empty());
}

#[tokio::test]
async fn webhook_mutations_and_external_test_results_are_minimal_and_correlated() {
    let db = TestDb::new("u58-webhook-audit");
    seed_and_seal(&db, Some("acme"));
    let store = webhook_store(&db);
    let token = "WEBHOOK-BEARER-SECRET";
    let url = format!("https://discord.com/api/webhooks/123456/{token}");

    let created = store
        .create_audited(webhook_request(&url), admin_audit(), AUDIT_KEY)
        .await
        .expect("create audited webhook");
    let rows = audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].tenant.as_str(),
            rows[0].actor_id.as_str(),
            rows[0].operation.as_str(),
        ),
        ("acme", ACTOR, "webhook.config.create")
    );
    assert_not_in_canonical(&rows, &url);
    assert_not_in_canonical(&rows, token);

    store
        .set_events_audited(
            OrgId("acme".to_owned()),
            created.id.clone(),
            EVENTS.to_vec(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("set same events")
        .expect("webhook exists");
    assert!(
        !store
            .remove_audited(
                OrgId("acme".to_owned()),
                WebhookId("missing-hook".to_owned()),
                admin_audit(),
                AUDIT_KEY,
            )
            .await
            .expect("remove missing")
    );
    assert_eq!(audit_rows(&db).len(), 1, "configuration no-ops are silent");

    let attempt = admin_audit();
    store
        .audit_test(
            OrgId("acme".to_owned()),
            created.id.clone(),
            None,
            attempt.clone(),
            AUDIT_KEY,
        )
        .await
        .expect("persist requested marker");
    store
        .audit_test(
            OrgId("acme".to_owned()),
            created.id.clone(),
            Some(false),
            attempt,
            AUDIT_KEY,
        )
        .await
        .expect("persist terminal failure");

    let rows = audit_rows(&db);
    assert_eq!(rows.len(), 3);
    let requested = &rows[1];
    let completed = &rows[2];
    assert_eq!(requested.operation, "webhook.test.requested");
    assert_eq!(completed.operation, "webhook.test.completed");
    assert_eq!(completed.result, "failure");
    assert_eq!(requested.tenant, "acme");
    assert_eq!(completed.tenant, "acme");
    assert_eq!(requested.target_type, "webhook");
    assert_eq!(completed.target_type, "webhook");
    assert_eq!(requested.target_id, created.id.0);
    assert_eq!(completed.target_id, created.id.0);
    assert_eq!(requested.actor_id, ACTOR);
    assert_eq!(completed.actor_id, ACTOR);
    assert_eq!(requested.request_id, completed.request_id);
    assert_not_in_canonical(&rows, &url);
    assert_not_in_canonical(&rows, token);

    let rollback = TestDb::new("u58-webhook-audit-rollback");
    seed_and_seal(&rollback, Some("acme"));
    let rollback_store = webhook_store(&rollback);
    assert_eq!(
        rollback_store
            .create_audited(webhook_request(&url), admin_audit(), WRONG_AUDIT_KEY)
            .await,
        Err(AppError::Internal)
    );
    let conn = rollback.conn();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM org_webhooks", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count webhooks"),
        0
    );
    assert!(audit_rows(&rollback).is_empty());
}

#[tokio::test]
async fn share_mutations_use_persisted_artifact_tenant_and_roll_back_with_audit() {
    let db = TestDb::new("u58-share-audit");
    seed_and_seal(&db, Some("acme"));
    seed_artifact(&db);
    let ids: Arc<dyn IdSource> = Arc::new(SequentialIdSource::default());
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::default());
    let created_by = "creator-secret@example.test";

    let share = create_audited_pooled(
        db.pool(),
        Arc::clone(&ids),
        Arc::clone(&clock),
        authorize_artifact(),
        CreateShare {
            created_by: created_by.to_owned(),
            expires: "never".to_owned(),
        },
        admin_audit(),
        AUDIT_KEY,
    )
    .await
    .expect("create audited share");
    let rows = audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].tenant.as_str(),
            rows[0].actor_id.as_str(),
            rows[0].operation.as_str(),
            rows[0].target_type.as_str(),
            rows[0].target_id.as_str(),
        ),
        ("acme", ACTOR, "share.create", "artifact", "auditart0001",)
    );
    assert_not_in_canonical(&rows, &share.token.0);
    assert_not_in_canonical(&rows, created_by);

    assert!(
        revoke_audited_pooled(
            db.pool(),
            authorize_artifact(),
            share.token.clone(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("first revoke")
    );
    assert!(
        !revoke_audited_pooled(
            db.pool(),
            authorize_artifact(),
            share.token,
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("second revoke")
    );
    assert_eq!(audit_rows(&db).len(), 2, "no-op retry has no event");

    let rollback = TestDb::new("u58-share-audit-rollback");
    seed_and_seal(&rollback, Some("acme"));
    seed_artifact(&rollback);
    let rollback_ids: Arc<dyn IdSource> = Arc::new(SequentialIdSource::default());
    let rollback_clock: Arc<dyn Clock> = Arc::new(FixedClock::default());
    assert_eq!(
        create_audited_pooled(
            rollback.pool(),
            rollback_ids,
            rollback_clock,
            authorize_artifact(),
            CreateShare {
                created_by: created_by.to_owned(),
                expires: "never".to_owned(),
            },
            admin_audit(),
            WRONG_AUDIT_KEY,
        )
        .await,
        Err(AppError::Internal)
    );
    let conn = rollback.conn();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM artifact_shares", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count shares"),
        0
    );
    assert!(audit_rows(&rollback).is_empty());
}
