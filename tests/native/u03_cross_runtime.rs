//! U03 cross-runtime proof: a Node-created database and a Rust-created database must have the
//! same schema. `lib/migrations.js` is authoritative; any divergence here is a bug in the port.
//!
//! The test drives the real Node reference (`lib/db.js`, which migrates on import), then opens
//! both files with rusqlite and compares `schema_migrations` plus every `sqlite_master` row.
//! It is skipped, not failed, when Node or its `node_modules` are unavailable, so `cargo test`
//! still runs in a Rust-only environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::persistence::db::{self, Database};
use artifact_mcp::persistence::migrations::MigrationContext;
use rusqlite::Connection;

use crate::u03_support::{TempDataDir, recorded_migrations, schema_objects};

const ORG_EMAIL_DOMAINS: &str =
    "\u{feff}example.com:acme,\u{85}nel.example:nel,globex.io:globex,skip.example:admin,broken";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Turns an unavailable Node reference into a hard failure instead of a silent skip.
///
/// Without this, the Node-vs-Rust schema proof — the strongest guarantee that the migration port
/// is faithful — green-passes in any environment lacking Node deps. CI must set
/// `REQUIRE_NODE_REFERENCE=1`. Mirrors the guard U04 introduced for the crypto parity proof.
fn require_node_reference() -> bool {
    matches!(std::env::var("REQUIRE_NODE_REFERENCE").as_deref(), Ok("1"))
}

/// Node reference availability; the suite degrades to a skip rather than a false failure,
/// unless `REQUIRE_NODE_REFERENCE=1` demands the proof actually run.
fn node_reference_available(root: &Path) -> bool {
    let missing = if !root.join("node_modules/better-sqlite3").is_dir() {
        Some("node_modules/better-sqlite3 is missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };

    match missing {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "REQUIRE_NODE_REFERENCE=1 but the Node reference is unavailable ({reason}); \
                 the Node/Rust schema proof did not run"
            );
            eprintln!("skipping cross-runtime schema proof: {reason}");
            false
        }
    }
}

/// Runs the Node reference bootstrap against `data_dir`, migrating it to the latest schema.
fn build_node_database(root: &Path, data_dir: &Path) {
    let entry = root.join("lib/db.js");
    let url = format!("file://{}", entry.display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg("import(process.argv[1]).catch((error) => { console.error(error); process.exit(1); })")
        .arg(&url)
        .env("DATA_DIR", data_dir)
        .env("ORG_EMAIL_DOMAINS", ORG_EMAIL_DOMAINS)
        .env_remove("WEBHOOK_ENC_KEY")
        .env_remove("ARTIFACT_API_KEYS")
        .output()
        .expect("run node reference");
    assert!(
        output.status.success(),
        "node reference bootstrap failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn open_readonly(data_dir: &Path) -> Connection {
    Connection::open(db::database_path(data_dir)).expect("open database for comparison")
}

fn rows(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).expect("prepare comparison query");
    let mapped = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("run comparison query");
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("read comparison rows")
}

#[test]
fn rust_and_node_produce_identical_schemas() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let node_dir = TempDataDir::new("xnode");
    let rust_dir = TempDataDir::new("xrust");
    build_node_database(&root, node_dir.path());
    let _pool = Database::open_with(
        rust_dir.path(),
        &MigrationContext {
            org_email_domains: ORG_EMAIL_DOMAINS.to_owned(),
        },
        None,
    )
    .expect("open rust database");

    let node_conn = open_readonly(node_dir.path());
    let rust_conn = open_readonly(rust_dir.path());

    // 1. Same ledger: identical (version, name) pairs, in order.
    assert_eq!(
        recorded_migrations(&rust_conn),
        recorded_migrations(&node_conn),
        "schema_migrations diverged between Rust and the Node reference"
    );

    // 2. Same objects: every table, index, view, and trigger, with normalised DDL text.
    let node_objects = schema_objects(&node_conn);
    let rust_objects = schema_objects(&rust_conn);
    assert!(
        !node_objects.is_empty(),
        "node reference produced no schema objects"
    );
    for (node_object, rust_object) in node_objects.iter().zip(rust_objects.iter()) {
        assert_eq!(
            rust_object, node_object,
            "sqlite_master entry diverged\n  node: {node_object:?}\n  rust: {rust_object:?}"
        );
    }
    assert_eq!(
        rust_objects.len(),
        node_objects.len(),
        "different number of schema objects\n  node: {:?}\n  rust: {:?}",
        node_objects.iter().map(|o| &o.1).collect::<Vec<_>>(),
        rust_objects.iter().map(|o| &o.1).collect::<Vec<_>>()
    );

    // 3. Same environment-seeded v7 data.
    let seeded_domains = "SELECT domain || '=' || org FROM org_domains ORDER BY domain";
    assert_eq!(
        rows(&rust_conn, seeded_domains),
        rows(&node_conn, seeded_domains),
        "ORG_EMAIL_DOMAINS seeding diverged"
    );
    assert_eq!(
        rows(&rust_conn, seeded_domains),
        [
            "example.com=acme",
            "globex.io=globex",
            "\u{85}nel.example=nel"
        ]
    );
    let seeded_orgs = "SELECT name FROM orgs ORDER BY name";
    assert_eq!(
        rows(&rust_conn, seeded_orgs),
        rows(&node_conn, seeded_orgs),
        "org seeding diverged"
    );

    // 4. Same column layout for every table, in declaration order.
    let tables = rows(
        &node_conn,
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    );
    for table in tables {
        let sql = format!("SELECT name || ' ' || type FROM pragma_table_info('{table}')");
        assert_eq!(
            rows(&rust_conn, &sql),
            rows(&node_conn, &sql),
            "columns of {table} diverged"
        );
    }
}

#[test]
fn rust_can_reopen_a_node_created_database_without_migrating() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let dir = TempDataDir::new("xreopen");
    build_node_database(&root, dir.path());

    // Same data directory, now bootstrapped by Rust: nothing new may be applied.
    let mut conn =
        db::open_bootstrap_connection(&db::database_path(dir.path())).expect("bootstrap");
    let applied =
        artifact_mcp::persistence::migrations::apply(&mut conn, &MigrationContext::empty())
            .expect("apply");
    assert!(
        applied.is_empty(),
        "Rust re-applied {applied:?} on a Node-created database"
    );

    let pool = Database::open_with(dir.path(), &MigrationContext::empty(), None).expect("open");
    let pooled = db::checkout(&pool).expect("checkout");
    assert_eq!(
        crate::u03_support::quick_check(&pooled),
        "ok",
        "Node-created database failed quick_check after Rust bootstrap"
    );
    assert_eq!(crate::u03_support::foreign_key_violations(&pooled), 0);
}
