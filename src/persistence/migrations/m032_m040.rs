//! Migrations 32 through 40: feedback anchor v2, artifact state, data bindings, collections,
//! web push reminders, and artifact keys.

use rusqlite::Transaction;

use super::{MigrationContext, schema_helpers::ensure_column};

/// Port of migration 32 (`feedback-anchor-v2`) in `lib/migrations.js`: the dense anchor-v2
/// metadata columns. Purely additive, idempotent `ALTER TABLE` statements, in Node's exact
/// order; a NULL value retains the existing anchor semantics.
pub(super) fn m32_feedback_anchor_v2(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    ensure_column(tx, "feedback", "anchor_kind", "TEXT")?;
    ensure_column(tx, "feedback", "anchor_node_id", "TEXT")?;
    ensure_column(tx, "feedback", "anchor_quote", "TEXT")?;
    Ok(())
}

/// Organization-scoped viewer state, keyed by artifact id and user-supplied key.
pub(super) fn m033_artifact_state(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE artifact_state (
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  key         TEXT NOT NULL,
  value       TEXT NOT NULL,
  revision    INTEGER NOT NULL DEFAULT 1,
  updated_at  TEXT NOT NULL,
  updated_by  TEXT NOT NULL,
  PRIMARY KEY (artifact_id, key)
);",
    )
}

/// Adds per-viewer state while carrying every v33 row into the explicit org bucket.
pub(super) fn m034_viewer_state_scope(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE artifact_state_v2 (
  artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
  scope       TEXT NOT NULL CHECK (scope IN ('org','viewer')),
  viewer      TEXT NOT NULL DEFAULT '' COLLATE NOCASE,
  key         TEXT NOT NULL,
  value       TEXT NOT NULL,
  revision    INTEGER NOT NULL DEFAULT 1,
  updated_at  TEXT NOT NULL,
  updated_by  TEXT NOT NULL,
  PRIMARY KEY (artifact_id, scope, viewer, key)
);
INSERT INTO artifact_state_v2 SELECT artifact_id, 'org', '', key, value, revision, updated_at, updated_by FROM artifact_state;
DROP TABLE artifact_state;
ALTER TABLE artifact_state_v2 RENAME TO artifact_state;
ALTER TABLE org_email_members ADD COLUMN display_name TEXT NOT NULL DEFAULT '';",
    )
}

pub(super) fn m035_artifact_data_bindings(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "\
      CREATE TABLE IF NOT EXISTS artifact_data_bindings (
        artifact_id TEXT PRIMARY KEY REFERENCES artifacts(id) ON DELETE CASCADE,
        org TEXT NOT NULL,
        bindings TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS artifact_data_snapshots (
        artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
        binding TEXT NOT NULL, key TEXT NOT NULL,
        value TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (artifact_id, binding, key)
      );
      CREATE TABLE IF NOT EXISTS artifact_data_events (
        artifact_id TEXT NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE, binding TEXT NOT NULL, subscription TEXT NOT NULL,
        event_id TEXT NOT NULL, event_name TEXT NOT NULL, data TEXT NOT NULL,
        created_at TEXT NOT NULL,
        PRIMARY KEY (artifact_id, binding, subscription, event_id)
      );
      ",
    )
}

/// Persistent administrator-managed source definitions. Operator-file sources remain outside
/// SQLite and are merged by the runtime registry. The definition contains references such as
/// environment-variable names only; resolved credential values never enter this table.
pub(super) fn m036_managed_data_sources(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
      CREATE TABLE IF NOT EXISTS data_sources (
        id         TEXT PRIMARY KEY,
        org        TEXT NOT NULL,
        definition TEXT NOT NULL,
        enabled    INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
        version    INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
      );
      CREATE INDEX IF NOT EXISTS data_sources_org_id_idx ON data_sources(org, id);
        ",
    )
}

/// Durable organization collections are references to existing artifacts.  The viewer
/// presentation table is deliberately keyed by email: it stores personal layout state and
/// never changes shared collection membership.
pub(super) fn m037_artifact_collections_and_viewer_preferences(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(include_str!(
        "../../../migrations/037-artifact-collections.sql"
    ))
}

pub(super) fn m038_collection_principals(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(include_str!(
        "../../../migrations/038-collection-principals.sql"
    ))
}

/// ADR-0012 scheduled reminders and Web Push subscriptions. Both runtimes execute the same shared
/// SQL file so `sqlite_master` stays identical.
pub(super) fn m039_web_push_reminders(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(include_str!(
        "../../../migrations/039-web-push-reminders.sql"
    ))
}

/// ADR-0015 per-organization artifact keys for publish upsert. Shared SQL keeps `sqlite_master`
/// identical across runtimes.
pub(super) fn m040_artifact_keys(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(include_str!("../../../migrations/040-artifact-keys.sql"))
}
