//! U03 bootstrap: startup order, pinned pragmas, pool behaviour, and integrity checks.

use artifact_mcp::persistence::db::{
    self, Database, POOL_CHECKOUT_TIMEOUT, POOL_MAX_SIZE, PragmaValue,
};
use artifact_mcp::persistence::migrations::{
    self, LATEST_SCHEMA_VERSION, MIGRATIONS, MigrationContext,
};
use rusqlite::Connection;

use crate::u03_support::{
    TempDataDir, column_names, foreign_key_violations, index_names, quick_check,
    recorded_migrations, scalar,
};

fn open(dir: &TempDataDir) -> db::DbPool {
    Database::open_with(dir.path(), &MigrationContext::empty(), None).expect("open database")
}

/// The exact `(version, name)` ledger frozen by the U01 contract.
fn expected_ledger() -> Vec<(i64, String)> {
    MIGRATIONS
        .iter()
        .map(|migration| (migration.version, migration.name.to_owned()))
        .collect()
}

#[test]
fn fresh_database_applies_the_extended_ledger_to_v24() {
    let dir = TempDataDir::new("fresh");
    let pool = open(&dir);
    let conn = db::checkout(&pool).expect("checkout");

    assert_eq!(recorded_migrations(&conn), expected_ledger());
    assert_eq!(
        migrations::current_version(&conn).expect("version"),
        LATEST_SCHEMA_VERSION
    );
    assert_eq!(
        migrations::applied_versions(&conn).expect("versions"),
        (1..=LATEST_SCHEMA_VERSION).collect::<Vec<_>>()
    );
}

#[test]
fn bootstrap_creates_the_artifact_directory_and_database_file() {
    let dir = TempDataDir::new("layout");
    let _pool = open(&dir);

    assert!(db::artifact_dir(dir.path()).is_dir());
    assert!(db::database_path(dir.path()).is_file());
    assert_eq!(db::DATABASE_FILE_NAME, "artifacts.db");
    assert_eq!(db::ARTIFACT_DIR_NAME, "artifacts");
}

#[test]
fn fresh_database_matches_the_node_schema_shape() {
    let dir = TempDataDir::new("shape");
    let pool = open(&dir);
    let conn = db::checkout(&pool).expect("checkout");

    // Mirrors the assertions in test/database.test.js.
    assert_eq!(
        column_names(&conn, "notification_reads"),
        ["viewer_email", "seen_at"]
    );
    assert_eq!(
        column_names(&conn, "org_email_members"),
        ["email", "org", "created_at"]
    );
    assert_eq!(
        column_names(&conn, "api_keys"),
        [
            "client_id",
            "key_hash",
            "org",
            "created_at",
            "revoked_at",
            "label",
            "role",
            "owner_email"
        ]
    );
    assert_eq!(
        column_names(&conn, "artifact_revisions"),
        [
            "artifact_id",
            "org",
            "revision",
            "title",
            "description",
            "category",
            "bytes",
            "is_bundle",
            "entry",
            "created_at",
            "body_sha256",
            "client_id"
        ]
    );
    assert_eq!(
        column_names(&conn, "feedback"),
        [
            "id",
            "artifact_id",
            "org",
            "viewer_email",
            "body",
            "artifact_revision",
            "created_at",
            "resolved_at",
            "resolved_by",
            "anchor_path",
            "anchor_x",
            "anchor_y",
            "anchor_approx",
            "parent_id",
            "anchor_w",
            "anchor_h",
            "anchor_page",
            "author_source",
            "external_author_id",
            "external_author_display",
            "external_created_at",
            "external_edited_at",
            "external_deleted_at",
            "anchor_kind",
            "anchor_node_id",
            "anchor_quote"
        ]
    );
    assert!(
        index_names(&conn, "org_email_members")
            .iter()
            .any(|name| name == "org_email_members_org_idx")
    );
    conn.execute(
        "INSERT INTO api_keys (client_id, key_hash) VALUES ('pre-v22-author', 'pre-v22-hash')",
        [],
    )
    .expect("insert legacy-shaped key");
    conn.execute(
        "INSERT INTO artifacts (id, client_id, org, title) \
         VALUES ('pre-v22-artifact', 'pre-v22-author', 'default', 'Existing revision')",
        [],
    )
    .expect("insert artifact");
    conn.execute(
        "INSERT INTO artifact_revisions (artifact_id, org, revision, title) \
         VALUES ('pre-v22-artifact', 'default', 1, 'Existing revision')",
        [],
    )
    .expect("insert unattributed revision");
    assert_eq!(
        conn.query_row(
            "SELECT client_id FROM artifact_revisions \
             WHERE artifact_id = 'pre-v22-artifact'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("read revision attribution"),
        None,
        "an unattributed pre-v22 revision remains readable as null"
    );

    let cascades: i64 = scalar(
        &conn,
        "SELECT COUNT(*) FROM pragma_foreign_key_list('reactions')
         WHERE \"table\" = 'artifacts' AND on_delete = 'CASCADE'",
    );
    assert_eq!(cascades, 1);
}

#[test]
fn reopening_a_migrated_database_applies_nothing_and_keeps_data() {
    let dir = TempDataDir::new("reopen");
    {
        let pool = open(&dir);
        let conn = db::checkout(&pool).expect("checkout");
        conn.execute("INSERT INTO orgs (name) VALUES (?1)", ["reopen-org"])
            .expect("insert org");
        conn.execute(
            "INSERT INTO org_email_members (email, org) VALUES (?1, ?2)",
            ["person@example.com", "reopen-org"],
        )
        .expect("insert member");
    }

    // A second bootstrap must be a no-op for the ledger.
    let pool = open(&dir);
    let mut conn = db::checkout(&pool).expect("checkout");
    assert_eq!(recorded_migrations(&conn), expected_ledger());
    let org: String = conn
        .query_row(
            "SELECT org FROM org_email_members WHERE email = ?1",
            ["person@example.com"],
            |row| row.get(0),
        )
        .expect("member survives reopen");
    assert_eq!(org, "reopen-org");

    let applied = migrations::apply(&mut conn, &MigrationContext::empty()).expect("re-apply");
    assert!(
        applied.is_empty(),
        "reopening applied {applied:?} instead of nothing"
    );
}

#[test]
fn integrity_and_foreign_keys_are_clean_after_migration() {
    let dir = TempDataDir::new("integrity");
    let pool = open(&dir);
    let conn = db::checkout(&pool).expect("checkout");

    assert_eq!(quick_check(&conn), "ok");
    assert_eq!(foreign_key_violations(&conn), 0);
}

/// Reads every pinned pragma back from one connection.
fn observed_pragmas(conn: &Connection) -> Vec<(&'static str, String)> {
    db::PINNED_PRAGMAS
        .iter()
        .map(|(name, expected)| {
            let value = match *expected {
                PragmaValue::Text(_) => conn
                    .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, String>(0))
                    .expect("read text pragma"),
                PragmaValue::Int(_) => conn
                    .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
                    .expect("read integer pragma")
                    .to_string(),
            };
            (*name, value)
        })
        .collect()
}

fn expected_pragmas() -> Vec<(&'static str, String)> {
    vec![
        ("journal_mode", "wal".to_owned()),
        ("synchronous", "2".to_owned()),
        ("busy_timeout", "5000".to_owned()),
        ("wal_autocheckpoint", "1000".to_owned()),
        ("foreign_keys", "1".to_owned()),
        ("page_size", "4096".to_owned()),
    ]
}

#[test]
fn bootstrap_connection_pins_all_six_pragmas() {
    let dir = TempDataDir::new("pragma-bootstrap");
    let _pool = open(&dir);

    let conn = db::open_bootstrap_connection(&db::database_path(dir.path()))
        .expect("open bootstrap connection");
    assert_eq!(observed_pragmas(&conn), expected_pragmas());
    db::verify_pragmas(&conn).expect("bootstrap pragmas verified");
}

#[test]
fn every_pooled_connection_pins_all_six_pragmas() {
    let dir = TempDataDir::new("pragma-pool");
    let pool = open(&dir);

    // Hold the whole pool at once so each assertion runs against a distinct physical
    // connection created by the r2d2 initializer, not a single reused one.
    let connections: Vec<_> = (0..POOL_MAX_SIZE)
        .map(|_| db::checkout(&pool).expect("checkout"))
        .collect();
    assert_eq!(connections.len(), POOL_MAX_SIZE as usize);
    for conn in &connections {
        assert_eq!(observed_pragmas(conn), expected_pragmas());
        db::verify_pragmas(conn).expect("pooled pragmas verified");
    }
    drop(connections);

    // A connection recycled back into the pool still has them.
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(observed_pragmas(&conn), expected_pragmas());
}

#[test]
fn pooled_connections_enforce_foreign_keys() {
    let dir = TempDataDir::new("fk-enforced");
    let pool = open(&dir);
    let conn = db::checkout(&pool).expect("checkout");

    let error = conn.execute(
        "INSERT INTO artifact_shares (token, artifact_id, org, created_by) VALUES (?1, ?2, ?3, ?4)",
        ["tok", "missing", "acme", "publisher@example.com"],
    );
    assert!(
        error.is_err(),
        "composite foreign key was not enforced on a pooled connection"
    );
}

#[test]
fn pool_uses_the_blueprint_configuration() {
    assert_eq!(POOL_MAX_SIZE, 4);
    assert_eq!(POOL_CHECKOUT_TIMEOUT, std::time::Duration::from_secs(5));

    let dir = TempDataDir::new("pool-config");
    let pool = open(&dir);
    assert_eq!(pool.max_size(), POOL_MAX_SIZE);
}

#[tokio::test]
async fn interact_runs_sync_work_on_the_blocking_pool() {
    let dir = TempDataDir::new("interact");
    let pool = open(&dir);

    // The closure owns its connection for its whole lifetime and drops it before returning,
    // which is the checkout contract every persistence adapter must follow.
    let version = db::interact(&pool, |conn| {
        let tx = conn
            .transaction()
            .map_err(|_| artifact_mcp::error::AppError::Internal)?;
        tx.execute("INSERT INTO orgs (name) VALUES (?1)", ["async-org"])
            .map_err(|_| artifact_mcp::error::AppError::Internal)?;
        tx.commit()
            .map_err(|_| artifact_mcp::error::AppError::Internal)?;
        migrations::current_version(conn).map_err(|_| artifact_mcp::error::AppError::Internal)
    })
    .await
    .expect("interact");
    assert_eq!(version, LATEST_SCHEMA_VERSION);

    let count = db::interact(&pool, |conn| {
        Ok(scalar::<i64>(
            conn,
            "SELECT COUNT(*) FROM orgs WHERE name = 'async-org'",
        ))
    })
    .await
    .expect("interact");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn interact_propagates_domain_errors() {
    let dir = TempDataDir::new("interact-error");
    let pool = open(&dir);

    let error = db::interact(&pool, |_conn| {
        Err::<(), _>(artifact_mcp::error::AppError::Conflict("boom".to_owned()))
    })
    .await
    .expect_err("closure error propagates");
    assert_eq!(
        error,
        artifact_mcp::error::AppError::Conflict("boom".to_owned())
    );
}
