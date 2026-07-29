//! U09 test support: a migrated throwaway database and error-shape helpers.

use artifact_mcp::error::AppError;
use artifact_mcp::persistence::db::{self, Database, DbConnection, DbPool};
use artifact_mcp::persistence::migrations::MigrationContext;
use artifact_mcp::persistence::orgs;

use crate::u03_support::TempDataDir;

/// A pool over a freshly migrated database in a temporary data directory.
///
/// `pool` is declared before `dir` so the pool is dropped (and its connections closed) before
/// the directory is removed.
pub struct TestDb {
    pool: DbPool,
    _dir: TempDataDir,
}

impl TestDb {
    pub fn new(label: &str) -> Self {
        let dir = TempDataDir::new(label);
        let pool = Database::open_with(dir.path(), &MigrationContext::empty(), None)
            .expect("bootstrap test database");
        Self { pool, _dir: dir }
    }

    pub fn conn(&self) -> DbConnection {
        db::checkout(&self.pool).expect("check a connection out of the test pool")
    }

    pub const fn pool(&self) -> &DbPool {
        &self.pool
    }
}

/// Unwraps the message of an expected [`AppError::Validation`] (the admin routes' 400 body).
pub fn validation_message<T: std::fmt::Debug>(result: Result<T, AppError>) -> String {
    match result {
        Err(AppError::Validation(message)) => message,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

/// Creates an org with no label and no domain.
pub fn seed_org(conn: &mut rusqlite::Connection, name: &str) {
    let request = artifact_mcp::model::CreateOrganization {
        name: artifact_mcp::model::OrgId(name.to_owned()),
        label: String::new(),
        domain: None,
    };
    orgs::create_org(conn, &request).unwrap_or_else(|error| panic!("seed org {name}: {error:?}"));
}
