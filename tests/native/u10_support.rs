//! U10 test support: a migrated engagement database and row-level fixture helpers.
//!
//! The helpers write rows with fully explicit values (timestamps included), so ordering and
//! aggregate assertions in `u10_engagement.rs` never depend on `datetime('now')` resolution.

use artifact_mcp::persistence::db::{self, Database, DbPool};
use artifact_mcp::persistence::migrations::MigrationContext;
use rusqlite::Connection;

use crate::u03_support::TempDataDir;

/// A migrated, pooled database in a throwaway data directory.
pub struct Fixture {
    pool: DbPool,
    /// Declared after `pool` so the connections close before the directory is removed.
    _dir: TempDataDir,
}

impl Fixture {
    /// Bootstraps an empty database at the latest schema.
    pub fn new(label: &str) -> Self {
        let dir = TempDataDir::new(label);
        let pool = Database::open_with(dir.path(), &MigrationContext::empty(), None)
            .expect("open rust database");
        Self { pool, _dir: dir }
    }

    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// A pooled connection (`foreign_keys = ON`, so the cascade assertions are real).
    pub fn conn(&self) -> db::DbConnection {
        db::checkout(&self.pool).expect("checkout connection")
    }
}

/// Inserts an artifact row owned by the `publisher` key.
pub fn insert_artifact(conn: &Connection, id: &str, org: &str, title: &str) {
    conn.execute(
        "INSERT INTO artifacts (id, client_id, org, title) VALUES (?, 'publisher', ?, ?)",
        (id, org, title),
    )
    .expect("insert artifact");
}

/// Inserts a feedback row with an explicit timestamp.
pub fn insert_feedback(
    conn: &Connection,
    id: &str,
    artifact_id: &str,
    org: &str,
    email: &str,
    body: &str,
    created_at: &str,
) {
    conn.execute(
        "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision, created_at)
         VALUES (?, ?, ?, ?, ?, 1, ?)",
        (id, artifact_id, org, email, body, created_at),
    )
    .expect("insert feedback");
}

/// Inserts an `artifact_views` row with fully explicit counters and timestamps, so ordering
/// assertions do not depend on `datetime('now')` resolution.
pub fn insert_view(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
    email: &str,
    count: i64,
    first_viewed_at: &str,
    last_viewed_at: &str,
) {
    conn.execute(
        "INSERT INTO artifact_views (artifact_id, org, email, count, first_viewed_at, last_viewed_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        (artifact_id, org, email, count, first_viewed_at, last_viewed_at),
    )
    .expect("insert artifact view");
}

/// Inserts a reaction row directly (bypassing the adapter, for read-side fixtures).
pub fn insert_reaction(
    conn: &Connection,
    email: &str,
    artifact_id: &str,
    favorite: i64,
    vote: i64,
) {
    conn.execute(
        "INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES (?, ?, ?, ?)",
        (email, artifact_id, favorite, vote),
    )
    .expect("insert reaction");
}

/// Scalar `SELECT` helper.
pub fn scalar<T: rusqlite::types::FromSql>(conn: &Connection, sql: &str) -> T {
    conn.query_row(sql, [], |row| row.get::<_, T>(0))
        .unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
}

/// Every row of a single-column query, in query order.
pub fn column(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).expect("prepare query");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("run query");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("read rows")
}
