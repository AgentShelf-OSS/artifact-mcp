//! Migrations 1 through 10: the initial schema and the first org, feedback, and analytics steps.

use rusqlite::Transaction;

use super::{
    MigrationContext,
    schema_helpers::{ensure_column, js_trim},
};

pub(super) fn m001_initial_schema(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS api_keys (
          client_id   TEXT PRIMARY KEY,
          key_hash    TEXT NOT NULL UNIQUE,
          org         TEXT NOT NULL DEFAULT 'default',
          created_at  TEXT NOT NULL DEFAULT (datetime('now')),
          revoked_at  TEXT
        );

        CREATE TABLE IF NOT EXISTS artifacts (
          id          TEXT PRIMARY KEY,
          client_id   TEXT NOT NULL,
          org         TEXT NOT NULL DEFAULT 'default',
          title       TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          bytes       INTEGER NOT NULL DEFAULT 0,
          created_at  TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS reactions (
          email       TEXT NOT NULL,
          artifact_id TEXT NOT NULL,
          favorite    INTEGER NOT NULL DEFAULT 0,
          vote        INTEGER NOT NULL DEFAULT 0,
          updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (email, artifact_id)
        );
        CREATE INDEX IF NOT EXISTS reactions_artifact_idx ON reactions(artifact_id);
      ",
    )
}

pub(super) fn m002_org_label_and_bundles(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    ensure_column(tx, "api_keys", "org", "TEXT NOT NULL DEFAULT 'default'")?;
    ensure_column(tx, "artifacts", "org", "TEXT NOT NULL DEFAULT 'default'")?;
    ensure_column(tx, "api_keys", "label", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(
        tx,
        "artifacts",
        "uploader_label",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    ensure_column(tx, "artifacts", "is_bundle", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(tx, "artifacts", "entry", "TEXT NOT NULL DEFAULT ''")?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS artifacts_org_idx ON artifacts(org, client_id, created_at DESC)",
    )
}

pub(super) fn m003_reaction_integrity(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Legacy reaction-table reconstruction: rebuild with CHECK constraints and a cascading
    // foreign key, keeping only rows whose artifact still exists (orphan rows are dropped).
    tx.execute_batch(
        "
        DROP TABLE IF EXISTS reactions_next;
        CREATE TABLE reactions_next (
          email       TEXT NOT NULL,
          artifact_id TEXT NOT NULL,
          favorite    INTEGER NOT NULL DEFAULT 0 CHECK (favorite IN (0, 1)),
          vote        INTEGER NOT NULL DEFAULT 0 CHECK (vote IN (-1, 0, 1)),
          updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (email, artifact_id),
          FOREIGN KEY (artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE
        );
        INSERT INTO reactions_next (email, artifact_id, favorite, vote, updated_at)
        SELECT r.email,
               r.artifact_id,
               CASE WHEN r.favorite <> 0 THEN 1 ELSE 0 END,
               CASE WHEN r.vote > 0 THEN 1 WHEN r.vote < 0 THEN -1 ELSE 0 END,
               r.updated_at
        FROM reactions r
        INNER JOIN artifacts a ON a.id = r.artifact_id;
        DROP TABLE reactions;
        ALTER TABLE reactions_next RENAME TO reactions;
        CREATE INDEX reactions_artifact_idx ON reactions(artifact_id);
      ",
    )
}

pub(super) fn m004_artifact_revision(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-009: stable-URL replace-in-place. Each successful update bumps revision.
    ensure_column(
        tx,
        "artifacts",
        "revision",
        "INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1)",
    )
}

pub(super) fn m005_viewer_feedback(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-010: org-scoped viewer feedback threads. Composite FK ties feedback to the
    // artifact's immutable (id, org) so a viewer can never re-tenant a comment.
    tx.execute_batch(
        "
        CREATE UNIQUE INDEX IF NOT EXISTS artifacts_id_org_uidx ON artifacts(id, org);
        CREATE TABLE IF NOT EXISTS feedback (
          id               TEXT PRIMARY KEY,
          artifact_id      TEXT NOT NULL,
          org              TEXT NOT NULL,
          viewer_email     TEXT NOT NULL,
          body             TEXT NOT NULL CHECK (length(trim(body)) BETWEEN 1 AND 4000),
          artifact_revision INTEGER NOT NULL CHECK (artifact_revision >= 1),
          created_at       TEXT NOT NULL DEFAULT (datetime('now')),
          resolved_at      TEXT,
          resolved_by      TEXT,
          CHECK ((resolved_at IS NULL) = (resolved_by IS NULL)),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS feedback_thread_idx ON feedback(artifact_id, resolved_at, created_at, id);
        CREATE INDEX IF NOT EXISTS feedback_org_idx ON feedback(org, resolved_at, created_at DESC, id DESC);
      ",
    )
}

pub(super) fn m006_artifact_category(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Category groups artifacts within an org (blank = "Uncategorized" bucket).
    ensure_column(tx, "artifacts", "category", "TEXT NOT NULL DEFAULT ''")?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS artifacts_org_category_idx ON artifacts(org, category, updated_at DESC)",
    )
}

pub(super) fn m007_org_registry(
    tx: &Transaction<'_>,
    ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Persist orgs, their email domains, and their category registry so tenancy is
    // managed in the admin UI instead of the ORG_EMAIL_DOMAINS env var. Domain->org
    // still falls back to the env map, then to "the domain is its own org".
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS orgs (
          name       TEXT PRIMARY KEY,
          label      TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS org_domains (
          domain     TEXT PRIMARY KEY,
          org        TEXT NOT NULL REFERENCES orgs(name) ON DELETE CASCADE,
          created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS org_domains_org_idx ON org_domains(org);
        CREATE TABLE IF NOT EXISTS org_categories (
          org        TEXT NOT NULL REFERENCES orgs(name) ON DELETE CASCADE,
          name       TEXT NOT NULL,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (org, name)
        );
      ",
    )?;

    // Seed orgs from tenants already in use (issued keys + published artifacts), minus
    // the admin pseudo-org which is not a real tenant.
    tx.execute_batch(
        "
        INSERT OR IGNORE INTO orgs (name)
        SELECT DISTINCT org FROM (
          SELECT org FROM api_keys
          UNION
          SELECT org FROM artifacts
        ) WHERE org NOT IN ('admin', '') AND org IS NOT NULL;
      ",
    )?;

    // Seed domain -> org from the ORG_EMAIL_DOMAINS env, creating any missing org.
    {
        let mut ins_org = tx.prepare("INSERT OR IGNORE INTO orgs (name) VALUES (?)")?;
        let mut ins_dom =
            tx.prepare("INSERT OR IGNORE INTO org_domains (domain, org) VALUES (?, ?)")?;
        for pair in ctx.org_email_domains.split(',') {
            let mut parts = pair.split(':').map(js_trim);
            let domain = parts.next().unwrap_or_default();
            let org = parts.next().unwrap_or_default();
            if !domain.is_empty() && !org.is_empty() && org != "admin" {
                ins_org.execute([org])?;
                ins_dom.execute((domain.to_lowercase(), org))?;
            }
        }
    }

    // Seed the category registry from categories already applied to artifacts.
    tx.execute_batch(
        "
        INSERT OR IGNORE INTO org_categories (org, name)
        SELECT DISTINCT org, category FROM artifacts
        WHERE category <> '' AND org NOT IN ('admin', '');
      ",
    )
}

pub(super) fn m008_artifact_history(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // Version history: each replace-in-place update snapshots the OUTGOING revision's
    // metadata here and its body under .history/<id>/<revision>. Restore replays a past
    // revision as a new one (append-only). Composite FK ties a snapshot to its artifact's
    // immutable (id, org) so cascade delete cleans the rows.
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS artifact_revisions (
          artifact_id TEXT NOT NULL,
          org         TEXT NOT NULL,
          revision    INTEGER NOT NULL CHECK (revision >= 1),
          title       TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          category    TEXT NOT NULL DEFAULT '',
          bytes       INTEGER NOT NULL DEFAULT 0,
          is_bundle   INTEGER NOT NULL DEFAULT 0,
          entry       TEXT NOT NULL DEFAULT '',
          created_at  TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (artifact_id, revision),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS artifact_revisions_idx ON artifact_revisions(artifact_id, revision DESC);
      ",
    )
}

pub(super) fn m009_org_discord_webhooks(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS org_webhooks (
          id            TEXT PRIMARY KEY,
          org           TEXT NOT NULL REFERENCES orgs(name) ON DELETE CASCADE,
          url           TEXT NOT NULL,
          label         TEXT NOT NULL DEFAULT '',
          events        TEXT NOT NULL DEFAULT 'published,updated,restored,deleted,feedback,resolved',
          created_at    TEXT NOT NULL DEFAULT (datetime('now')),
          last_ok_at    TEXT,
          last_error    TEXT
        );
        CREATE INDEX IF NOT EXISTS org_webhooks_org_idx ON org_webhooks(org);
      ",
    )
}

pub(super) fn m010_artifact_view_analytics(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS artifact_views (
          artifact_id     TEXT NOT NULL,
          org             TEXT NOT NULL,
          email           TEXT NOT NULL,
          count           INTEGER NOT NULL DEFAULT 1,
          first_viewed_at TEXT NOT NULL DEFAULT (datetime('now')),
          last_viewed_at  TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (artifact_id, email),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS artifact_views_artifact_last_viewed_idx ON artifact_views(artifact_id, last_viewed_at DESC);
        CREATE INDEX IF NOT EXISTS artifact_views_org_artifact_idx ON artifact_views(org, artifact_id);
      ",
    )
}
