//! U11 test support: a migrated database with artifacts to hang shares and feedback off.
//!
//! Everything here goes through the real bootstrap ([`Database::open_at`]), so the six pinned
//! pragmas — `foreign_keys = ON` in particular, which is what makes the reply cascade work — are
//! the ones production uses, not a hand-rolled schema.

use artifact_mcp::config::{FixedClock, SequentialIdSource};
use artifact_mcp::model::{ArtifactId, ClientId, OrgId};
use artifact_mcp::persistence::db::{self, Database, DbConnection, DbPool};

use crate::u03_support::TempDataDir;

/// A migrated database plus the deterministic adapters U02 froze.
pub struct Fixture {
    #[allow(
        dead_code,
        reason = "kept alive so the temporary directory outlives the pool"
    )]
    data_dir: TempDataDir,
    pool: DbPool,
    /// Counter-backed ids: tokens and feedback ids are unique *and* ascending, so an `id`
    /// tiebreak in an `ORDER BY` is predictable instead of random.
    pub ids: SequentialIdSource,
    /// Pinned to 2026-01-01T00:00:00Z for the pure expiry arithmetic. Rows whose expiry is
    /// compared against SQLite's own `julianday('now')` are written relative to that clock
    /// instead, never to this one.
    pub clock: FixedClock,
}

impl Fixture {
    pub fn new(label: &str) -> Self {
        let data_dir = TempDataDir::new(label);
        let pool = Database::open_at(data_dir.path()).expect("bootstrap the u11 database");
        Self {
            data_dir,
            pool,
            ids: SequentialIdSource::default(),
            clock: FixedClock::default(),
        }
    }

    pub fn conn(&self) -> DbConnection {
        db::checkout(&self.pool).expect("check out a connection")
    }

    /// One artifact row, the parent both `feedback` and `artifact_shares` reference through the
    /// composite `(id, org)` foreign key.
    pub fn seed_artifact(&self, id: &str, org: &str, client_id: &str) -> ArtifactId {
        self.conn()
            .execute(
                "INSERT INTO artifacts (id, client_id, org, title) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, client_id, org, format!("Artifact {id}")],
            )
            .expect("seed artifact row");
        ArtifactId::from(id)
    }
}

pub fn org(name: &str) -> OrgId {
    OrgId::from(name)
}

pub fn client(name: &str) -> ClientId {
    ClientId::from(name)
}
