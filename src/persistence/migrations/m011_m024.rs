//! Migrations 11 through 24: visibility, feedback anchors, shares, API key capabilities, and
//! artifact durability intents.

use rusqlite::Transaction;

use super::{MigrationContext, schema_helpers::ensure_column};

pub(super) fn m011_artifact_visibility(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Hidden means unlisted, not private: direct URLs remain tenant-accessible.
    ensure_column(tx, "artifacts", "hidden", "INTEGER NOT NULL DEFAULT 0")?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS artifacts_org_hidden_updated_idx ON artifacts(org, hidden, updated_at DESC)",
    )
}

pub(super) fn m012_feedback_anchors(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-013: a NULL anchor remains the original artifact-wide feedback behavior.
    ensure_column(tx, "feedback", "anchor_path", "TEXT")?;
    ensure_column(tx, "feedback", "anchor_x", "REAL")?;
    ensure_column(tx, "feedback", "anchor_y", "REAL")?;
    ensure_column(
        tx,
        "feedback",
        "anchor_approx",
        "INTEGER NOT NULL DEFAULT 0",
    )
}

pub(super) fn m013_feedback_threads(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Replies have one parent only; SQLite permits this nullable FK in ALTER TABLE.
    ensure_column(
        tx,
        "feedback",
        "parent_id",
        "TEXT REFERENCES feedback(id) ON DELETE CASCADE",
    )?;
    tx.execute_batch("CREATE INDEX IF NOT EXISTS feedback_parent_idx ON feedback(parent_id)")
}

pub(super) fn m014_feedback_anchor_boxes(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-013 extension: NULL dimensions retain the original point-anchor model.
    ensure_column(tx, "feedback", "anchor_w", "REAL")?;
    ensure_column(tx, "feedback", "anchor_h", "REAL")
}

pub(super) fn m015_artifact_public_shares(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS artifact_shares (
          token       TEXT PRIMARY KEY,
          artifact_id TEXT NOT NULL,
          org         TEXT NOT NULL,
          created_by  TEXT NOT NULL,
          created_at  TEXT NOT NULL DEFAULT (datetime('now')),
          expires_at  TEXT,
          revoked_at  TEXT,
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS artifact_shares_artifact_idx ON artifact_shares(artifact_id);
      ",
    )
}

pub(super) fn m016_org_color(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Optional per-org accent color (hex). NULL = derive a stable color from the org name.
    ensure_column(tx, "orgs", "color", "TEXT")
}

pub(super) fn m017_artifact_body_digest(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Empty is the explicit legacy/unknown value for rows whose bodies predate this
    // migration; every new publish/update records a SHA-256 commit marker.
    ensure_column(tx, "artifacts", "body_sha256", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(
        tx,
        "artifact_revisions",
        "body_sha256",
        "TEXT NOT NULL DEFAULT ''",
    )
}

pub(super) fn m018_webhook_url_encryption(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // The legacy url column remains required for zero-config plaintext fallback and
    // holds only the safe masked display value when the encrypted columns are set.
    ensure_column(tx, "org_webhooks", "url_cipher", "TEXT")?;
    ensure_column(tx, "org_webhooks", "url_nonce", "TEXT")?;
    ensure_column(tx, "org_webhooks", "url_tag", "TEXT")
}

pub(super) fn m019_feedback_anchor_page(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-024: NULL retains single-file and pre-page-identity anchor semantics.
    ensure_column(tx, "feedback", "anchor_page", "TEXT")
}

pub(super) fn m020_notification_read_watermarks(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-020: one durable read watermark per verified gallery viewer.
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS notification_reads (
          viewer_email TEXT PRIMARY KEY,
          seen_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS feedback_created_idx ON feedback(created_at DESC, id DESC);
      ",
    )
}

pub(super) fn m021_explicit_email_org_membership(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS org_email_members (
          email      TEXT PRIMARY KEY COLLATE NOCASE,
          org        TEXT NOT NULL,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          FOREIGN KEY (org) REFERENCES orgs(name) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS org_email_members_org_idx ON org_email_members(org, email);
      ",
    )
}

pub(super) fn m022_api_key_capabilities(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
        ALTER TABLE api_keys ADD COLUMN role TEXT NOT NULL DEFAULT 'author';
        ALTER TABLE artifact_revisions ADD COLUMN client_id TEXT;
      ",
    )
}

pub(super) fn m023_verified_artifact_owner(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Nullable by design: no automatic inference is allowed for legacy records or service keys.
    ensure_column(tx, "api_keys", "owner_email", "TEXT")?;
    ensure_column(tx, "artifacts", "owner_email", "TEXT")?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS artifacts_org_owner_visibility_idx \
          ON artifacts(org, owner_email, hidden, created_at DESC);",
    )
}

pub(super) fn m024_artifact_durability_intents(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS artifact_durability_intents (
          id              TEXT PRIMARY KEY,
          artifact_id     TEXT NOT NULL,
          operation       TEXT NOT NULL CHECK (operation IN ('publish', 'update', 'delete')),
          state           TEXT NOT NULL CHECK (state IN ('prepared', 'body_durable', 'metadata_committed')),
          expected_sha256 TEXT NOT NULL DEFAULT '',
          prior_sha256    TEXT NOT NULL DEFAULT '',
          staging_path    TEXT NOT NULL DEFAULT '',
          created_at      TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS artifact_durability_intents_artifact_idx
          ON artifact_durability_intents(artifact_id, created_at);
      ",
    )
}
