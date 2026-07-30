//! U03 migrations: legacy upgrades, programmatic side effects, and constraint behaviour.

use artifact_mcp::error::AppError;
use artifact_mcp::persistence::db::{self, Database};
use artifact_mcp::persistence::migrations::{
    self, EncryptedUrl, LATEST_SCHEMA_VERSION, MigrationContext, WebhookUrlCipher, mask_webhook_url,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rusqlite::Connection;

use crate::u03_support::{
    TempDataDir, column_names, foreign_key_violations, quick_check, recorded_migrations, scalar,
};

fn write_legacy_database(dir: &TempDataDir, sql: &str) {
    let conn = Connection::open(db::database_path(dir.path())).expect("create legacy database");
    conn.execute_batch(sql).expect("apply legacy schema");
}

fn migrate(dir: &TempDataDir, ctx: &MigrationContext) -> db::DbPool {
    Database::open_with(dir.path(), ctx, None).expect("open database")
}

#[test]
fn legacy_databases_upgrade_without_losing_keys_artifacts_or_reactions() {
    // Byte-for-byte the pre-migration fixture used by test/database.test.js.
    let dir = TempDataDir::new("legacy");
    std::fs::create_dir_all(dir.path()).expect("data dir");
    write_legacy_database(
        &dir,
        "
        CREATE TABLE api_keys (
          client_id TEXT PRIMARY KEY,
          key_hash TEXT NOT NULL UNIQUE,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          revoked_at TEXT
        );
        CREATE TABLE artifacts (
          id TEXT PRIMARY KEY,
          client_id TEXT NOT NULL,
          title TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          bytes INTEGER NOT NULL DEFAULT 0,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE reactions (
          email TEXT NOT NULL,
          artifact_id TEXT NOT NULL,
          favorite INTEGER NOT NULL DEFAULT 0,
          vote INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (email, artifact_id)
        );
        INSERT INTO api_keys (client_id, key_hash) VALUES ('legacy-key', 'hash');
        INSERT INTO artifacts (id, client_id, title) VALUES ('abc123', 'legacy-key', 'Legacy artifact');
        INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('viewer@example.com', 'abc123', 1, 1);
        INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('orphan@example.com', 'missing1', 1, -1);
      ",
    );

    let pool = migrate(&dir, &MigrationContext::empty());
    let conn = db::checkout(&pool).expect("checkout");

    assert_eq!(
        migrations::current_version(&conn).expect("version"),
        LATEST_SCHEMA_VERSION
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT org FROM api_keys WHERE client_id = 'legacy-key'"
        ),
        "default"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT role FROM api_keys WHERE client_id = 'legacy-key'"
        ),
        "author"
    );
    assert_eq!(
        scalar::<String>(&conn, "SELECT title FROM artifacts WHERE id = 'abc123'"),
        "Legacy artifact"
    );
    // The orphaned reaction is dropped by the v3 table reconstruction.
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM reactions"), 1);
    assert_eq!(quick_check(&conn), "ok");
    assert_eq!(foreign_key_violations(&conn), 0);

    // The rebuilt table carries the cascading foreign key.
    conn.execute("DELETE FROM artifacts WHERE id = 'abc123'", [])
        .expect("delete artifact");
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM reactions"), 0);
}

#[test]
fn legacy_reaction_values_are_clamped_by_the_v3_reconstruction() {
    let dir = TempDataDir::new("legacy-clamp");
    write_legacy_database(
        &dir,
        "
        CREATE TABLE api_keys (client_id TEXT PRIMARY KEY, key_hash TEXT NOT NULL UNIQUE);
        CREATE TABLE artifacts (
          id TEXT PRIMARY KEY,
          client_id TEXT NOT NULL,
          title TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          bytes INTEGER NOT NULL DEFAULT 0,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE reactions (
          email TEXT NOT NULL,
          artifact_id TEXT NOT NULL,
          favorite INTEGER NOT NULL DEFAULT 0,
          vote INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (email, artifact_id)
        );
        INSERT INTO artifacts (id, client_id, title) VALUES ('art1', 'key', 'Artifact');
        INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('a@example.com', 'art1', 7, 9);
        INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('b@example.com', 'art1', 0, -9);
      ",
    );

    let pool = migrate(&dir, &MigrationContext::empty());
    let conn = db::checkout(&pool).expect("checkout");

    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT favorite FROM reactions WHERE email = 'a@example.com'"
        ),
        1
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT vote FROM reactions WHERE email = 'a@example.com'"
        ),
        1
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT vote FROM reactions WHERE email = 'b@example.com'"
        ),
        -1
    );

    // The rebuilt CHECK constraints now reject out-of-range values.
    assert!(
        conn.execute(
            "INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('c@example.com', 'art1', 0, 5)",
            [],
        )
        .is_err(),
        "vote CHECK constraint missing"
    );
}

#[test]
fn migration_seven_seeds_orgs_domains_and_categories_from_existing_data() {
    let dir = TempDataDir::new("org-registry");
    write_legacy_database(
        &dir,
        "
        CREATE TABLE api_keys (
          client_id TEXT PRIMARY KEY,
          key_hash TEXT NOT NULL UNIQUE,
          org TEXT NOT NULL DEFAULT 'default',
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          revoked_at TEXT
        );
        CREATE TABLE artifacts (
          id TEXT PRIMARY KEY,
          client_id TEXT NOT NULL,
          org TEXT NOT NULL DEFAULT 'default',
          title TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          bytes INTEGER NOT NULL DEFAULT 0,
          category TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE reactions (
          email TEXT NOT NULL,
          artifact_id TEXT NOT NULL,
          favorite INTEGER NOT NULL DEFAULT 0,
          vote INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (email, artifact_id)
        );
        INSERT INTO api_keys (client_id, key_hash, org) VALUES ('k1', 'h1', 'acme');
        INSERT INTO api_keys (client_id, key_hash, org) VALUES ('k2', 'h2', 'admin');
        INSERT INTO artifacts (id, client_id, org, title, category)
          VALUES ('a1', 'k1', 'globex', 'One', 'Reports');
        INSERT INTO artifacts (id, client_id, org, title, category)
          VALUES ('a2', 'k1', 'acme', 'Two', '');
        INSERT INTO artifacts (id, client_id, org, title, category)
          VALUES ('a3', 'k2', 'admin', 'Three', 'Secret');
      ",
    );

    let ctx = MigrationContext {
        // Trailing/leading spaces, mixed case, the admin pseudo-org, and malformed entries all
        // follow the Node parsing rules exactly.
        org_email_domains: " Example.COM : acme , globex.io:globex ,skip.example:admin,broken,:orphan,nodomain.example: "
            .to_owned(),
    };
    let pool = migrate(&dir, &ctx);
    let conn = db::checkout(&pool).expect("checkout");

    let orgs = {
        let mut stmt = conn
            .prepare("SELECT name FROM orgs ORDER BY name")
            .expect("prepare");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.collect::<rusqlite::Result<Vec<_>>>().expect("orgs")
    };
    assert_eq!(
        orgs,
        ["acme", "globex"],
        "admin and blank orgs are excluded"
    );

    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT org FROM org_domains WHERE domain = 'example.com'"
        ),
        "acme",
        "domains are lowercased and trimmed"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT org FROM org_domains WHERE domain = 'globex.io'"
        ),
        "globex"
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM org_domains WHERE org = 'admin' OR domain IN ('broken', '')"
        ),
        0
    );
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM org_domains"), 2);

    // Category registry is seeded from non-blank categories of non-admin orgs.
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM org_categories"),
        1
    );
    assert_eq!(
        scalar::<String>(&conn, "SELECT org || '/' || name FROM org_categories"),
        "globex/Reports"
    );
}

#[test]
fn migration_seven_seeds_nothing_without_environment_domains() {
    let dir = TempDataDir::new("org-registry-empty");
    let pool = migrate(&dir, &MigrationContext::empty());
    let conn = db::checkout(&pool).expect("checkout");

    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM orgs"), 0);
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM org_domains"), 0);
}

#[test]
fn composite_foreign_keys_pin_child_rows_to_the_artifact_tenant() {
    let dir = TempDataDir::new("composite-fk");
    let pool = migrate(&dir, &MigrationContext::empty());
    let conn = db::checkout(&pool).expect("checkout");

    conn.execute(
        "INSERT INTO artifacts (id, client_id, org, title) VALUES ('art1', 'k1', 'acme', 'One')",
        [],
    )
    .expect("insert artifact");

    // Same id, foreign org: the composite (artifact_id, org) foreign key rejects re-tenanting.
    let re_tenanted = conn.execute(
        "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision)
         VALUES ('f1', 'art1', 'evil', 'v@example.com', 'hi', 1)",
        [],
    );
    assert!(re_tenanted.is_err(), "feedback escaped its artifact's org");

    conn.execute(
        "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision)
         VALUES ('f2', 'art1', 'acme', 'v@example.com', 'hi', 1)",
        [],
    )
    .expect("insert feedback");

    for table in ["artifact_revisions", "artifact_views", "artifact_shares"] {
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM pragma_foreign_key_list('{table}') WHERE \"table\" = 'artifacts'"
                ),
                [],
                |row| row.get(0),
            )
            .expect("foreign key list");
        assert_eq!(count, 2, "{table} is missing the composite artifact key");
    }

    // Deleting the artifact cascades the whole child graph.
    conn.execute("DELETE FROM artifacts WHERE id = 'art1'", [])
        .expect("delete artifact");
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM feedback"), 0);
}

#[test]
fn explicit_email_membership_is_case_insensitive_and_org_scoped() {
    let dir = TempDataDir::new("email-members");
    let pool = migrate(&dir, &MigrationContext::empty());
    let conn = db::checkout(&pool).expect("checkout");

    conn.execute("INSERT INTO orgs (name) VALUES ('acme')", [])
        .expect("insert org");
    conn.execute(
        "INSERT INTO org_email_members (email, org) VALUES ('Person@Example.com', 'acme')",
        [],
    )
    .expect("insert member");

    assert!(
        conn.execute(
            "INSERT INTO org_email_members (email, org) VALUES ('person@example.com', 'acme')",
            [],
        )
        .is_err(),
        "email primary key is not COLLATE NOCASE"
    );
    assert!(
        conn.execute(
            "INSERT INTO org_email_members (email, org) VALUES ('other@example.com', 'ghost')",
            [],
        )
        .is_err(),
        "membership accepted an unknown org"
    );

    conn.execute("DELETE FROM orgs WHERE name = 'acme'", [])
        .expect("delete org");
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM org_email_members"),
        0,
        "membership rows did not cascade with the org"
    );
}

#[test]
fn webhook_encryption_columns_exist_after_migration_eighteen() {
    let dir = TempDataDir::new("webhook-columns");
    let pool = migrate(&dir, &MigrationContext::empty());
    let conn = db::checkout(&pool).expect("checkout");

    assert_eq!(
        column_names(&conn, "org_webhooks"),
        [
            "id",
            "org",
            "url",
            "label",
            "events",
            "created_at",
            "last_ok_at",
            "last_error",
            "url_cipher",
            "url_nonce",
            "url_tag"
        ]
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT dflt_value FROM pragma_table_info('org_webhooks') WHERE name = 'events'"
        ),
        "'published,updated,restored,deleted,feedback,resolved'"
    );
}

/// Stand-in for the U04 AEAD: reversible, but never leaves the plaintext readable in a row.
struct ReversingCipher;

impl WebhookUrlCipher for ReversingCipher {
    fn encrypt_url(&self, plaintext: &str) -> Result<EncryptedUrl, AppError> {
        let reversed: String = plaintext.chars().rev().collect();
        Ok(EncryptedUrl {
            ciphertext: BASE64.encode(reversed.as_bytes()),
            nonce: BASE64.encode([1_u8; 12]),
            tag: BASE64.encode([2_u8; 16]),
        })
    }
}

#[test]
fn existing_plaintext_webhook_rows_are_encrypted_in_place() {
    let dir = TempDataDir::new("webhook-encrypt");
    let secret_url = "https://discord.com/api/webhooks/123/existing-plaintext-token";
    {
        let pool = migrate(&dir, &MigrationContext::empty());
        let conn = db::checkout(&pool).expect("checkout");
        conn.execute("INSERT INTO orgs (name) VALUES ('migration-test')", [])
            .expect("insert org");
        conn.execute(
            "INSERT INTO org_webhooks (id, org, url) VALUES (?1, ?2, ?3)",
            ["legacy-webhook", "migration-test", secret_url],
        )
        .expect("insert webhook");
        conn.execute(
            "INSERT INTO org_webhooks (id, org, url) VALUES ('blank-webhook', 'migration-test', '   ')",
            [],
        )
        .expect("insert blank webhook");
        conn.execute(
            "INSERT INTO org_discord_discussion_connections (id, org, url) VALUES (?1, ?2, ?3)",
            ["legacy-discussion", "migration-test", secret_url],
        )
        .expect("insert discussion connection");
    }

    let mut conn = db::open_bootstrap_connection(&db::database_path(dir.path()))
        .expect("open bootstrap connection");
    let converted =
        migrations::encrypt_plaintext_webhook_urls(&mut conn, &ReversingCipher).expect("convert");
    assert_eq!(converted, 2, "blank URLs must be left alone");

    let (url, cipher, nonce, tag): (String, String, String, String) = conn
        .query_row(
            "SELECT url, url_cipher, url_nonce, url_tag FROM org_webhooks WHERE id = 'legacy-webhook'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read converted row");
    assert_eq!(url, "https://discord.com/…oken");
    assert_eq!(url, mask_webhook_url(secret_url));
    assert!(!url.contains("existing-plaintext-token"));
    assert!(!cipher.contains("existing-plaintext-token"));
    assert!(!nonce.is_empty() && !tag.is_empty());
    let decoded = String::from_utf8(BASE64.decode(&cipher).expect("base64")).expect("utf8");
    assert_eq!(decoded.chars().rev().collect::<String>(), secret_url);

    let (discussion_url, discussion_cipher): (String, String) = conn
        .query_row(
            "SELECT url, url_cipher FROM org_discord_discussion_connections WHERE id = 'legacy-discussion'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read encrypted discussion connection");
    assert_eq!(discussion_url, mask_webhook_url(secret_url));
    assert!(!discussion_url.contains("existing-plaintext-token"));
    assert!(!discussion_cipher.contains("existing-plaintext-token"));

    // A second bootstrap must not double-encrypt.
    assert_eq!(
        migrations::encrypt_plaintext_webhook_urls(&mut conn, &ReversingCipher).expect("convert"),
        0
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT url FROM org_webhooks WHERE id = 'legacy-webhook'"
        ),
        "https://discord.com/…oken"
    );
    // Without a configured cipher the bootstrap leaves plaintext rows untouched (Node returns 0).
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT url FROM org_webhooks WHERE id = 'blank-webhook'"
        ),
        "   "
    );
}

#[test]
fn migration_ledger_records_the_frozen_versions_and_names() {
    let dir = TempDataDir::new("ledger");
    let pool = migrate(&dir, &MigrationContext::empty());
    let conn = db::checkout(&pool).expect("checkout");

    let expected: Vec<(i64, String)> = vec![
        (1, "initial-schema".to_owned()),
        (2, "org-label-and-bundles".to_owned()),
        (3, "reaction-integrity".to_owned()),
        (4, "artifact-revision".to_owned()),
        (5, "viewer-feedback".to_owned()),
        (6, "artifact-category".to_owned()),
        (7, "org-registry".to_owned()),
        (8, "artifact-history".to_owned()),
        (9, "org-discord-webhooks".to_owned()),
        (10, "artifact-view-analytics".to_owned()),
        (11, "artifact-visibility".to_owned()),
        (12, "feedback-anchors".to_owned()),
        (13, "feedback-threads".to_owned()),
        (14, "feedback-anchor-boxes".to_owned()),
        (15, "artifact-public-shares".to_owned()),
        (16, "org-color".to_owned()),
        (17, "artifact-body-digest".to_owned()),
        (18, "webhook-url-encryption".to_owned()),
        (19, "feedback-anchor-page".to_owned()),
        (20, "notification-read-watermarks".to_owned()),
        (21, "explicit-email-org-membership".to_owned()),
        (22, "api-key-capabilities".to_owned()),
        (23, "verified-artifact-owner".to_owned()),
        (24, "artifact-durability-intents".to_owned()),
        (25, "security-audit-ledger".to_owned()),
        (26, "security-audit-protocol-hardening".to_owned()),
        (27, "provider-delivery-outbox".to_owned()),
        (28, "discord-discussion-mirror".to_owned()),
        (29, "discord-notification-threads".to_owned()),
        (30, "discord-organization-threading-policy".to_owned()),
        (31, "discord-two-way-inbound-sync".to_owned()),
    ];
    assert_eq!(recorded_migrations(&conn), expected);
    assert!(
        column_names(&conn, "security_audit_receipts")
            .iter()
            .any(|column| column == "receipt_mac"),
        "v26 authenticates pending receipt snapshots"
    );
    assert!(
        column_names(&conn, "security_audit_chain_head")
            .iter()
            .any(|column| column == "pending_receipts_root"),
        "v26 commits the complete pending receipt set"
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM schema_migrations WHERE applied_at IS NULL OR applied_at = ''"
        ),
        0,
        "every ledger row records an applied_at timestamp"
    );
}
