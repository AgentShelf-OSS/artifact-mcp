//! Owned by U03 (terra) — rusqlite bootstrap connection and r2d2 pool.
//!
//! Blueprint A4 startup order, implemented by [`Database::open`]:
//!
//! 1. open one exclusive bootstrap connection (the only connection that exists at this point);
//! 2. set and *verify* `journal_mode=WAL` by querying it back;
//! 3. set the remaining pragmas explicitly;
//! 4. apply migrations;
//! 5. run the webhook plaintext-to-encrypted conversion when a cipher is configured;
//! 6. build the r2d2 pool;
//! 7. apply per-connection pragmas through the connection initializer.
//!
//! Steps 8 and 9 (storage reconciliation, then the listener) belong to U08/U20.
//!
//! # Blocking and checkout contract
//!
//! Everything in this module is synchronous SQLite work and must never run on a Tokio worker
//! thread directly. The contract for callers is:
//!
//! * A database operation is *one* synchronous closure that checks a connection out of the pool,
//!   does all of its SQL (and any coordinated filesystem work), and drops the connection before
//!   returning. Use [`interact`], which wraps that closure in `tokio::task::spawn_blocking`.
//! * A pooled connection or an open `Transaction` must never be held across an `.await`. The
//!   borrow checker cannot enforce this on its own, so [`interact`] deliberately gives the closure
//!   a `&mut Connection` that cannot escape: it is created and dropped inside the blocking task.
//! * Checkout blocks for at most [`POOL_CHECKOUT_TIMEOUT`]. Exhausting the pool for that long is
//!   reported as [`AppError::Unavailable`], never as a hang.
//! * The pool holds at most [`POOL_MAX_SIZE`] connections. SQLite still has a single writer;
//!   `busy_timeout` (5 s) absorbs writer contention inside the driver.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::time::Duration;

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

use crate::config::AppConfig;
use crate::error::AppError;
use crate::persistence::migrations::{self, MigrationContext, WebhookUrlCipher};

/// Shared connection pool handed to every persistence adapter.
pub type DbPool = r2d2::Pool<SqliteConnectionManager>;

/// A connection checked out of [`DbPool`].
pub type DbConnection = r2d2::PooledConnection<SqliteConnectionManager>;

/// Maximum pooled connections (blueprint A4).
pub const POOL_MAX_SIZE: u32 = 4;

/// Maximum time a caller waits for a pooled connection (blueprint A4).
pub const POOL_CHECKOUT_TIMEOUT: Duration = Duration::from_secs(5);

/// Every pragma this process pins explicitly, with the value it must read back as.
///
/// These are asserted on the bootstrap connection *and* on every pooled connection; library
/// defaults are never inherited. `journal_mode` and `page_size` are properties of the database
/// file, the other four are per-connection state applied by the pool initializer.
pub const PINNED_PRAGMAS: &[(&str, PragmaValue)] = &[
    ("journal_mode", PragmaValue::Text("wal")),
    ("synchronous", PragmaValue::Int(2)), // FULL
    ("busy_timeout", PragmaValue::Int(5000)),
    ("wal_autocheckpoint", PragmaValue::Int(1000)),
    ("foreign_keys", PragmaValue::Int(1)), // ON
    ("page_size", PragmaValue::Int(4096)),
];

/// Expected value of a pinned pragma.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PragmaValue {
    /// Textual pragma value, compared case-insensitively as SQLite reports lowercase.
    Text(&'static str),
    /// Integer pragma value.
    Int(i64),
}

/// SQLite database file name inside the data directory (`lib/db.js`).
pub const DATABASE_FILE_NAME: &str = "artifacts.db";

/// Artifact body directory name inside the data directory (`lib/db.js`).
pub const ARTIFACT_DIR_NAME: &str = "artifacts";

/// Absolute path of the SQLite file for a data directory.
#[must_use]
pub fn database_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DATABASE_FILE_NAME)
}

/// Absolute path of the artifact body directory for a data directory.
#[must_use]
pub fn artifact_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(ARTIFACT_DIR_NAME)
}

/// Database bootstrap: opens, migrates, and pools the SQLite file under a data directory.
pub struct Database;

impl Database {
    /// Frozen entry point: bootstrap the database described by the application configuration.
    ///
    /// The data directory comes from the already-validated [`AppConfig`].
    ///
    /// # Errors
    /// Returns [`AppError::Unavailable`] when the data directory, SQLite file, pragmas,
    /// migrations, or pool cannot be brought up.
    pub fn open(config: &AppConfig) -> Result<DbPool, AppError> {
        Self::open_at(&configured_data_dir(config))
    }

    /// Bootstrap an explicit data directory with environment-derived migration seeding and no
    /// webhook cipher (plaintext webhook rows are left untouched, as when no key is configured).
    ///
    /// # Errors
    /// See [`Database::open`].
    pub fn open_at(data_dir: &Path) -> Result<DbPool, AppError> {
        Self::open_with(data_dir, &MigrationContext::from_env(), None)
    }

    /// Full bootstrap seam: explicit data directory, migration context, and optional cipher.
    ///
    /// # Errors
    /// See [`Database::open`].
    pub fn open_with(
        data_dir: &Path,
        ctx: &MigrationContext,
        cipher: Option<&dyn WebhookUrlCipher>,
    ) -> Result<DbPool, AppError> {
        // `lib/db.js` creates <dataDir>/artifacts recursively, which also creates the data dir.
        std::fs::create_dir_all(artifact_dir(data_dir))
            .map_err(|error| unavailable("create data directory", &error))?;
        let db_path = database_path(data_dir);

        // Steps 1-3: one bootstrap connection with every pragma pinned and verified.
        let mut bootstrap = open_bootstrap_connection(&db_path)?;

        // Step 4: migrations, each in its own transaction.
        let applied = migrations::apply(&mut bootstrap, ctx)
            .map_err(|error| unavailable("apply migrations", &error))?;
        let version = migrations::current_version(&bootstrap)
            .map_err(|error| unavailable("read schema version", &error))?;
        tracing::info!(
            schema_version = version,
            applied = applied.len(),
            "database schema ready"
        );

        // Step 5: convert legacy plaintext webhook rows when a key is configured.
        if let Some(cipher) = cipher {
            migrations::encrypt_plaintext_webhook_urls(&mut bootstrap, cipher)?;
        }

        // The bootstrap connection is exclusive: it is closed before the pool exists, so no
        // caller can accidentally keep using an unpooled connection.
        drop(bootstrap);

        // Steps 6-7: the pool, with per-connection pragmas applied by the initializer.
        build_pool(&db_path)
    }
}

/// Resolves the validated data directory for the current configuration.
fn configured_data_dir(config: &AppConfig) -> PathBuf {
    config.data_dir.clone()
}

/// Opens the single bootstrap connection and pins/verifies all six pragmas (A4 steps 1-3).
///
/// "Exclusive" here means sole ownership for the duration of bootstrap — it is the only connection
/// open before the pool is built — not `PRAGMA locking_mode=EXCLUSIVE`, which would keep a WAL
/// write lock and lock out the pool that is created moments later.
///
/// # Errors
/// Returns [`AppError::Unavailable`] if the file cannot be opened or any pragma does not read
/// back with its pinned value.
pub fn open_bootstrap_connection(db_path: &Path) -> Result<Connection, AppError> {
    let conn =
        Connection::open(db_path).map_err(|error| unavailable("open sqlite database", &error))?;
    apply_file_pragmas(&conn).map_err(|error| unavailable("set database pragmas", &error))?;
    apply_connection_pragmas(&conn).map_err(|error| unavailable("set database pragmas", &error))?;
    verify_pragmas(&conn)?;
    Ok(conn)
}

/// Database-file-scoped pragmas. `page_size` must precede the WAL switch because SQLite can only
/// change the page size before the first write (and never in WAL mode without a VACUUM).
///
/// `journal_mode` is set and then *queried back*: a rejected WAL switch (network filesystem,
/// read-only file) is silent otherwise, so the value is verified rather than assumed.
fn apply_file_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    run_pragma(conn, "PRAGMA page_size = 4096")?;
    let journal_mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(())
}

/// Per-connection pragmas. These do not survive a connection, so the pool initializer applies
/// them to every checkout target — not just to the bootstrap connection.
fn apply_connection_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    run_pragma(conn, "PRAGMA synchronous = FULL")?;
    run_pragma(conn, "PRAGMA busy_timeout = 5000")?;
    run_pragma(conn, "PRAGMA wal_autocheckpoint = 1000")?;
    run_pragma(conn, "PRAGMA foreign_keys = ON")?;
    Ok(())
}

/// Executes a pragma statement, tolerating both the row-returning and silent pragma forms.
fn run_pragma(conn: &Connection, sql: &str) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    while rows.next()?.is_some() {}
    Ok(())
}

/// Reads every pinned pragma back and fails startup if one did not take effect.
///
/// # Errors
/// Returns [`AppError::Unavailable`] naming the pragma that diverged.
pub fn verify_pragmas(conn: &Connection) -> Result<(), AppError> {
    for (name, expected) in PINNED_PRAGMAS {
        let sql = format!("PRAGMA {name}");
        match *expected {
            PragmaValue::Text(want) => {
                let got: String = conn
                    .query_row(&sql, [], |row| row.get(0))
                    .map_err(|error| unavailable("read pragma", &error))?;
                if !got.eq_ignore_ascii_case(want) {
                    return Err(AppError::Unavailable(format!(
                        "database pragma {name} is {got}, expected {want}"
                    )));
                }
            }
            PragmaValue::Int(want) => {
                let got: i64 = conn
                    .query_row(&sql, [], |row| row.get(0))
                    .map_err(|error| unavailable("read pragma", &error))?;
                if got != want {
                    return Err(AppError::Unavailable(format!(
                        "database pragma {name} is {got}, expected {want}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Builds the r2d2 pool (A4 steps 6-7).
fn build_pool(db_path: &Path) -> Result<DbPool, AppError> {
    let manager = SqliteConnectionManager::file(db_path).with_init(|conn| {
        // Runs on every physical connection r2d2 creates, so `foreign_keys`, `synchronous`,
        // `busy_timeout`, and `wal_autocheckpoint` hold for every checkout, not just bootstrap.
        apply_connection_pragmas(conn)
    });
    r2d2::Pool::builder()
        .max_size(POOL_MAX_SIZE)
        .connection_timeout(POOL_CHECKOUT_TIMEOUT)
        .build(manager)
        .map_err(|error| unavailable("build connection pool", &error))
}

/// Checks a connection out of the pool, blocking for at most [`POOL_CHECKOUT_TIMEOUT`].
///
/// Call only from a blocking context ([`interact`] or a synchronous bootstrap path).
///
/// # Errors
/// Returns [`AppError::Unavailable`] when the pool cannot hand out a connection in time. The
/// message is deliberately generic; the underlying cause is logged, never returned.
pub fn checkout(pool: &DbPool) -> Result<DbConnection, AppError> {
    pool.get().map_err(|error| {
        tracing::error!(error = %error, "database connection checkout failed");
        AppError::Unavailable("database unavailable".to_owned())
    })
}

/// Runs one synchronous database operation on the blocking pool.
///
/// This is the only sanctioned way for async code to touch SQLite. The closure receives an
/// exclusive connection that is checked out and dropped inside the blocking task, so no
/// connection or transaction can outlive the operation or be held across an `.await`.
///
/// # Errors
/// Propagates the closure's [`AppError`], returns [`AppError::Unavailable`] when no connection is
/// available, or [`AppError::Internal`] when the blocking task panicked or was cancelled.
pub async fn interact<T, F>(pool: &DbPool, work: F) -> Result<T, AppError>
where
    F: FnOnce(&mut Connection) -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    let pool = pool.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = checkout(&pool)?;
        work(&mut conn)
    })
    .await
    .map_err(|error| {
        tracing::error!(error = %error, "database blocking task failed");
        AppError::Internal
    })?
}

/// Startup faults are reported as unavailable with an operator-facing reason and are logged.
fn unavailable(operation: &str, error: &impl Display) -> AppError {
    tracing::error!(operation, error = %error, "database bootstrap failed");
    AppError::Unavailable(format!("database {operation} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_the_six_verified_pragmas() {
        let names: Vec<&str> = PINNED_PRAGMAS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "journal_mode",
                "synchronous",
                "busy_timeout",
                "wal_autocheckpoint",
                "foreign_keys",
                "page_size"
            ]
        );
    }

    #[test]
    fn uses_the_blueprint_pool_configuration() {
        assert_eq!(POOL_MAX_SIZE, 4);
        assert_eq!(POOL_CHECKOUT_TIMEOUT, Duration::from_secs(5));
    }
}
