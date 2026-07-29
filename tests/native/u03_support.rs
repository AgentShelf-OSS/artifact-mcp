//! U03 test support: throwaway data directories and small SQLite query helpers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temporary data directory removed on drop.
pub struct TempDataDir {
    path: PathBuf,
}

impl TempDataDir {
    pub fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u03-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp data directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Every `(version, name)` pair recorded in `schema_migrations`, ascending.
pub fn recorded_migrations(conn: &Connection) -> Vec<(i64, String)> {
    let mut stmt = conn
        .prepare("SELECT version, name FROM schema_migrations ORDER BY version")
        .expect("prepare schema_migrations query");
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query schema_migrations");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("read schema_migrations")
}

/// Column names of a table in declaration order.
pub fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("read table_info")
}

/// Index names defined on a table.
pub fn index_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA index_list({table})"))
        .expect("prepare index_list");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query index_list");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("read index_list")
}

/// Single scalar helper for the assertions below.
pub fn scalar<T: rusqlite::types::FromSql>(conn: &Connection, sql: &str) -> T {
    conn.query_row(sql, [], |row| row.get::<_, T>(0))
        .unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
}

/// `PRAGMA quick_check` result (`ok` on a healthy database).
pub fn quick_check(conn: &Connection) -> String {
    scalar(conn, "PRAGMA quick_check")
}

/// Number of rows reported by `PRAGMA foreign_key_check`.
pub fn foreign_key_violations(conn: &Connection) -> usize {
    let mut stmt = conn
        .prepare("PRAGMA foreign_key_check")
        .expect("prepare foreign_key_check");
    let mut rows = stmt.query([]).expect("run foreign_key_check");
    let mut count = 0;
    while rows.next().expect("read foreign_key_check row").is_some() {
        count += 1;
    }
    count
}

/// Normalised `sqlite_master` entries: `(type, name, tbl_name, whitespace-collapsed sql)`.
///
/// Auto-created indexes (`sqlite_autoindex_*`) have a NULL `sql`, which normalises to an empty
/// string so both runtimes compare equal.
pub fn schema_objects(conn: &Connection) -> Vec<(String, String, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT type, name, tbl_name, COALESCE(sql, '')
             FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_stat%'
             ORDER BY type, name",
        )
        .expect("prepare sqlite_master query");
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                normalize_sql(&row.get::<_, String>(3)?),
            ))
        })
        .expect("query sqlite_master");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("read sqlite_master")
}

/// Collapses runs of whitespace so indentation differences are not reported as divergence.
pub fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}
