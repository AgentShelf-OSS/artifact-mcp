//! U08 test support: a real pooled database, a real artifact directory, and the small
//! inspection helpers the lifecycle and crash-recovery suites share.
//!
//! Everything here drives the production adapter — `Database::open_at` applies the real
//! migrations and `ArtifactStore` is the same type `AppDeps` will hold — so the crash matrix
//! exercises real SQLite transactions and real filesystem renames, not a simulation.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use artifact_mcp::artifacts::lifecycle::{ArtifactStore, FaultInjector, NoFaults, ScriptedFaults};
use artifact_mcp::config::{SequentialIdSource, StorageLimits};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{
    ArtifactContent, ArtifactMeta, ArtifactUpdate, OrgId, PublishArtifact, PublishedArtifact,
    PublisherIdentity,
};
use artifact_mcp::persistence::db::{self, Database, DbPool};
use artifact_mcp::security::audit::MutationAudit;
use rusqlite::params;

use crate::u03_support::TempDataDir;

/// The tenant every fixture publishes into.
pub const TEST_ORG: &str = "acme";
/// The publisher key every fixture publishes with.
pub const TEST_CLIENT: &str = "client-1";
/// Explicit deterministic audit material for the direct lifecycle adapter tests.
pub const TEST_AUDIT_KEY: [u8; 32] = [0xA5; 32];

/// Explicit verified actor carrier for every direct-store mutation in the lifecycle corpus.
pub fn mutation_audit() -> MutationAudit {
    MutationAudit::publisher(&publisher()).expect("deterministic test audit context")
}

/// A throwaway store over a real database and a real artifact directory.
pub struct Fixture {
    dir: TempDataDir,
    pub pool: DbPool,
    pub store: ArtifactStore,
    pub artifact_dir: PathBuf,
}

impl Fixture {
    /// A store with no faults armed.
    pub fn new(label: &str) -> Self {
        Self::with_injector(label, Arc::new(NoFaults), StorageLimits::default())
    }

    /// A store with a scripted fault injector. The same `Arc` is returned so a test can inspect
    /// which failpoints were reached.
    pub fn with_faults(label: &str, faults: Arc<ScriptedFaults>) -> Self {
        Self::with_injector(label, faults, StorageLimits::default())
    }

    /// A store with a caller-controlled lifecycle injector. Concurrency proofs use this to
    /// hold one production mutation at an exact boundary while a second request races it.
    pub fn with_custom_injector(label: &str, faults: Arc<dyn FaultInjector>) -> Self {
        Self::with_injector(label, faults, StorageLimits::default())
    }

    /// A store with custom limits (used by the history-retention test).
    pub fn with_limits(label: &str, limits: StorageLimits) -> Self {
        Self::with_injector(label, Arc::new(NoFaults), limits)
    }

    fn with_injector(label: &str, faults: Arc<dyn FaultInjector>, limits: StorageLimits) -> Self {
        let dir = TempDataDir::new(label);
        let pool = Database::open_at(dir.path()).expect("bootstrap database");
        let artifact_dir = dir.path().join("artifacts");
        let store = ArtifactStore::with_faults_for_test(
            pool.clone(),
            artifact_dir.clone(),
            limits,
            Arc::new(SequentialIdSource::default()),
            faults,
            TEST_AUDIT_KEY,
        );
        let fixture = Self {
            dir,
            pool,
            store,
            artifact_dir,
        };
        fixture.create_org(TEST_ORG);
        fixture
    }

    pub fn data_dir(&self) -> &Path {
        self.dir.path()
    }

    /// `INSERT INTO orgs` so `moveArtifactToOrg`'s `orgExists` guard can pass.
    pub fn create_org(&self, name: &str) {
        let conn = db::checkout(&self.pool).expect("checkout");
        conn.execute(
            "INSERT OR IGNORE INTO orgs (name) VALUES (?1)",
            params![name],
        )
        .expect("insert org");
    }

    /// Run one read-only statement against the fixture database.
    pub fn scalar<T: rusqlite::types::FromSql>(&self, sql: &str) -> T {
        let conn = db::checkout(&self.pool).expect("checkout");
        conn.query_row(sql, [], |row| row.get(0))
            .unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.scalar(sql)
    }

    /// Rows reported by `PRAGMA foreign_key_check` — zero on a healthy database. A pragma with no
    /// violations returns no rows at all, so this cannot go through `scalar`.
    pub fn foreign_key_violations(&self) -> usize {
        let conn = db::checkout(&self.pool).expect("checkout");
        let mut statement = conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare foreign_key_check");
        let mut rows = statement.query([]).expect("run foreign_key_check");
        let mut violations = 0;
        while rows.next().expect("read foreign_key_check row").is_some() {
            violations += 1;
        }
        violations
    }

    pub fn execute(&self, sql: &str) {
        let conn = db::checkout(&self.pool).expect("checkout");
        conn.execute_batch(sql)
            .unwrap_or_else(|error| panic!("statement `{sql}` failed: {error}"));
    }

    /// Every entry directly under the artifact directory, sorted.
    pub fn entries(&self) -> Vec<String> {
        read_names(&self.artifact_dir)
    }

    /// Entries that are hidden staging/trash paths. [lib/store.js:694]
    pub fn transient_entries(&self) -> Vec<String> {
        self.entries()
            .into_iter()
            .filter(|name| name.starts_with('.') && name.contains('-'))
            .filter(|name| name.contains(".staging-") || name.contains(".trash-"))
            .collect()
    }

    pub fn staging_entries(&self) -> Vec<String> {
        self.entries()
            .into_iter()
            .filter(|name| name.contains(".staging-"))
            .collect()
    }

    pub fn trash_entries(&self) -> Vec<String> {
        self.entries()
            .into_iter()
            .filter(|name| name.contains(".trash-"))
            .collect()
    }

    /// Reload the row behind a published artifact, or `None` once it is deleted.
    pub fn reload(&self, meta: &ArtifactMeta) -> Option<ArtifactMeta> {
        let conn = db::checkout(&self.pool).expect("checkout");
        conn.query_row(
            "SELECT id, client_id, org, title, description, bytes, created_at, updated_at, \
             uploader_label, owner_email, is_bundle, entry, revision, category, hidden, body_sha256 \
             FROM artifacts WHERE id = ?1",
            params![meta.id.0],
            |row| {
                Ok(ArtifactMeta {
                    id: artifact_mcp::model::ArtifactId(row.get(0)?),
                    client_id: artifact_mcp::model::ClientId(row.get(1)?),
                    org: OrgId(row.get(2)?),
                    title: row.get(3)?,
                    description: row.get(4)?,
                    bytes: row.get::<_, i64>(5)?.unsigned_abs(),
                    created_at: artifact_mcp::model::Timestamp(row.get(6)?),
                    updated_at: artifact_mcp::model::Timestamp(row.get(7)?),
                    uploader_label: row.get(8)?,
                    owner_email: row.get(9)?,
                    is_bundle: row.get::<_, i64>(10)? != 0,
                    entry: row.get(11)?,
                    revision: row.get::<_, i64>(12)?.unsigned_abs(),
                    category: row.get(13)?,
                    hidden: row.get::<_, i64>(14)? != 0,
                    body_sha256: row.get(15)?,
                })
            },
        )
        .ok()
    }

    /// The single-file body currently installed at the final path.
    pub fn body_on_disk(&self, meta: &ArtifactMeta) -> Option<String> {
        std::fs::read_to_string(self.artifact_dir.join(format!("{}.html", meta.id.0))).ok()
    }

    /// One file inside a bundle's live directory.
    pub fn bundle_file_on_disk(&self, meta: &ArtifactMeta, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.artifact_dir.join(&meta.id.0).join(relative)).ok()
    }

    /// One retained history body.
    pub fn history_body(&self, meta: &ArtifactMeta, revision: u64) -> Option<String> {
        std::fs::read_to_string(
            self.artifact_dir
                .join(".history")
                .join(&meta.id.0)
                .join(format!("{revision}.html")),
        )
        .ok()
    }

    pub fn history_entries(&self, meta: &ArtifactMeta) -> Vec<String> {
        read_names(&self.artifact_dir.join(".history").join(&meta.id.0))
    }

    /// Publish a single-file artifact and return its committed metadata.
    pub async fn publish_single(&self, html: &str) -> ArtifactMeta {
        self.try_publish(ArtifactContent::SingleHtml(html.to_owned()))
            .await
            .expect("publish succeeds")
            .meta
    }

    /// Publish a bundle in the caller's order, optionally with an explicit entry.
    pub async fn publish_bundle(
        &self,
        files: &[(&str, &str)],
        entry: Option<&str>,
    ) -> PublishedArtifact {
        self.try_publish(bundle_content(files, entry))
            .await
            .expect("publish succeeds")
    }

    pub async fn try_publish(
        &self,
        content: ArtifactContent,
    ) -> Result<PublishedArtifact, AppError> {
        self.try_publish_request(publish_request(content)).await
    }

    /// Publish an explicitly built request — used by the metadata-normalization parity proof.
    pub async fn try_publish_request(
        &self,
        request: PublishArtifact,
    ) -> Result<PublishedArtifact, AppError> {
        use artifact_mcp::ports::ArtifactService as _;
        let audit = artifact_mcp::security::audit::MutationAudit::publisher(&request.publisher)?;
        self.store.publish(request, audit).await
    }

    /// The current body digest recorded in the row, for divergence assertions.
    pub fn recorded_digest(&self, meta: &ArtifactMeta) -> String {
        self.reload(meta)
            .map(|row| row.body_sha256)
            .unwrap_or_default()
    }
}

/// Ordered bundle content — the caller's order is load-bearing (contract delta 4).
pub fn bundle_content(files: &[(&str, &str)], entry: Option<&str>) -> ArtifactContent {
    ArtifactContent::Bundle {
        files: files
            .iter()
            .map(|(name, body)| ((*name).to_owned(), (*body).to_owned()))
            .collect(),
        entry: entry.map(ToOwned::to_owned),
    }
}

pub fn publisher() -> PublisherIdentity {
    PublisherIdentity {
        client_id: TEST_CLIENT.into(),
        org: TEST_ORG.into(),
        label: "Fixture publisher".to_owned(),
        role: "author".to_owned(),
        scopes: None,
    }
}

pub fn publish_request(content: ArtifactContent) -> PublishArtifact {
    PublishArtifact {
        publisher: publisher(),
        target_org: OrgId(TEST_ORG.to_owned()),
        title: Some("Fixture".to_owned()),
        description: Some("Fixture artifact".to_owned()),
        category: Some("docs".to_owned()),
        content,
    }
}

/// An update that only replaces the single-file body.
pub fn html_update(revision: u64, html: &str) -> ArtifactUpdate {
    ArtifactUpdate {
        expected_revision: revision,
        content: Some(ArtifactContent::SingleHtml(html.to_owned())),
        ..ArtifactUpdate::default()
    }
}

/// An update that only replaces the bundle snapshot.
pub fn bundle_update(revision: u64, files: &[(&str, &str)], entry: Option<&str>) -> ArtifactUpdate {
    ArtifactUpdate {
        expected_revision: revision,
        content: Some(bundle_content(files, entry)),
        ..ArtifactUpdate::default()
    }
}

/// Node's `sha256(content)` for a single body. [lib/store.js:77-79]
pub fn sha256_hex(content: &str) -> String {
    artifact_mcp::artifacts::digest::sha256_hex(content.as_bytes())
}

pub fn read_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
