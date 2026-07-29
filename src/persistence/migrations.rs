//! Owned by U03 (terra) — append-only version/name-compatible SQLite migrations.
//!
//! The Node reference `lib/migrations.js` is the behavioural oracle for this module. Every
//! statement below is a verbatim copy of the SQL it executes so that `sqlite_master.sql` text,
//! column order, index names, check constraints, and composite foreign keys are identical
//! between a Node-created and a Rust-created database. Do not "tidy" the SQL: the cross-runtime
//! schema comparison in `tests/native/u03_cross_runtime.rs` compares normalised statement text,
//! and a reformatted statement is a real divergence in a shared production database.
//!
//! Blocking discipline: every function here is synchronous `rusqlite` work. Callers must run
//! them inside `tokio::task::spawn_blocking` (see [`crate::persistence::db::interact`]); no
//! function in this module may be awaited or called while a transaction is held across `.await`.

use std::fmt::Display;

use rusqlite::{Connection, Transaction};

use crate::error::AppError;

/// Latest schema version. The ledger is append-only and must match Node exactly.
pub const LATEST_SCHEMA_VERSION: i64 = 23;

/// `String.prototype.trim`'s character set, which is **not** Rust's `char::is_whitespace`.
///
/// ECMA-262 trims WhiteSpace ∪ LineTerminator: TAB, VT, FF, SP, NBSP, ZWNBSP (`U+FEFF`), every
/// `Zs`, LF, CR, LS, PS. Rust's White_Space property adds `U+0085` (NEL) and omits `U+FEFF`, so
/// both differences are corrected here.
#[must_use]
const fn is_js_whitespace(value: char) -> bool {
    matches!(value, '\u{feff}') || (value.is_whitespace() && !matches!(value, '\u{85}'))
}

/// `String.prototype.trim` — [lib/migrations.js:225]
#[must_use]
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// Environment-derived inputs consumed by migrations with programmatic side effects.
///
/// Migration 7 (`org-registry`) seeds `orgs`/`org_domains` from `ORG_EMAIL_DOMAINS`, exactly as
/// `lib/migrations.js` does. Capturing that value in an explicit context keeps migrations
/// deterministic and testable instead of reading process state deep inside the ladder.
#[derive(Clone, Debug, Default)]
pub struct MigrationContext {
    /// Raw `ORG_EMAIL_DOMAINS` value in `domain:org,domain:org` form.
    pub org_email_domains: String,
}

impl MigrationContext {
    /// Mirrors `String(process.env.ORG_EMAIL_DOMAINS || "")` in the Node reference.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            org_email_domains: std::env::var("ORG_EMAIL_DOMAINS").unwrap_or_default(),
        }
    }

    /// Context with no environment-derived seeding, used by fixtures and tests.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

type MigrationFn = fn(&Transaction<'_>, &MigrationContext) -> rusqlite::Result<()>;

/// One ordered, append-only schema step recorded in `schema_migrations`.
pub struct Migration {
    /// Frozen version number.
    pub version: i64,
    /// Frozen migration name.
    pub name: &'static str,
    up: MigrationFn,
}

impl Migration {
    /// Applies this migration's statements inside the caller's transaction.
    fn run(&self, tx: &Transaction<'_>, ctx: &MigrationContext) -> rusqlite::Result<()> {
        (self.up)(tx, ctx)
    }
}

/// A migration applied by this process (not one already recorded in the database).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    /// Applied version.
    pub version: i64,
    /// Applied name.
    pub name: &'static str,
}

/// The frozen ledger. Append only; never renumber, rename, reorder, or edit an existing entry.
pub static MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial-schema",
        up: m001_initial_schema,
    },
    Migration {
        version: 2,
        name: "org-label-and-bundles",
        up: m002_org_label_and_bundles,
    },
    Migration {
        version: 3,
        name: "reaction-integrity",
        up: m003_reaction_integrity,
    },
    Migration {
        version: 4,
        name: "artifact-revision",
        up: m004_artifact_revision,
    },
    Migration {
        version: 5,
        name: "viewer-feedback",
        up: m005_viewer_feedback,
    },
    Migration {
        version: 6,
        name: "artifact-category",
        up: m006_artifact_category,
    },
    Migration {
        version: 7,
        name: "org-registry",
        up: m007_org_registry,
    },
    Migration {
        version: 8,
        name: "artifact-history",
        up: m008_artifact_history,
    },
    Migration {
        version: 9,
        name: "org-discord-webhooks",
        up: m009_org_discord_webhooks,
    },
    Migration {
        version: 10,
        name: "artifact-view-analytics",
        up: m010_artifact_view_analytics,
    },
    Migration {
        version: 11,
        name: "artifact-visibility",
        up: m011_artifact_visibility,
    },
    Migration {
        version: 12,
        name: "feedback-anchors",
        up: m012_feedback_anchors,
    },
    Migration {
        version: 13,
        name: "feedback-threads",
        up: m013_feedback_threads,
    },
    Migration {
        version: 14,
        name: "feedback-anchor-boxes",
        up: m014_feedback_anchor_boxes,
    },
    Migration {
        version: 15,
        name: "artifact-public-shares",
        up: m015_artifact_public_shares,
    },
    Migration {
        version: 16,
        name: "org-color",
        up: m016_org_color,
    },
    Migration {
        version: 17,
        name: "artifact-body-digest",
        up: m017_artifact_body_digest,
    },
    Migration {
        version: 18,
        name: "webhook-url-encryption",
        up: m018_webhook_url_encryption,
    },
    Migration {
        version: 19,
        name: "feedback-anchor-page",
        up: m019_feedback_anchor_page,
    },
    Migration {
        version: 20,
        name: "notification-read-watermarks",
        up: m020_notification_read_watermarks,
    },
    Migration {
        version: 21,
        name: "explicit-email-org-membership",
        up: m021_explicit_email_org_membership,
    },
    Migration {
        version: 22,
        name: "api-key-capabilities",
        up: m022_api_key_capabilities,
    },
    Migration {
        version: 23,
        name: "verified-artifact-owner",
        up: m023_verified_artifact_owner,
    },
];

/// Creates `schema_migrations` if needed and applies every unrecorded migration in order.
///
/// Each migration runs in its own transaction together with its `schema_migrations` row, exactly
/// like `db.transaction(...)` in `lib/migrations.js`: a failing migration leaves neither partial
/// DDL nor a bookkeeping row behind. Reopening an up-to-date database applies nothing and returns
/// an empty vector.
///
/// This is blocking work; call it from `spawn_blocking` or from a synchronous bootstrap path.
pub fn apply(
    conn: &mut Connection,
    ctx: &MigrationContext,
) -> rusqlite::Result<Vec<AppliedMigration>> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
      version    INTEGER PRIMARY KEY,
      name       TEXT NOT NULL,
      applied_at TEXT NOT NULL DEFAULT (datetime('now'))
    )",
    )?;

    let applied = applied_versions(conn)?;
    let mut newly_applied = Vec::new();

    for migration in MIGRATIONS {
        if applied.contains(&migration.version) {
            continue;
        }
        let tx = conn.transaction()?;
        migration.run(&tx, ctx)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
            (migration.version, migration.name),
        )?;
        tx.commit()?;
        newly_applied.push(AppliedMigration {
            version: migration.version,
            name: migration.name,
        });
    }

    Ok(newly_applied)
}

/// Versions already recorded in `schema_migrations`, ascending.
pub fn applied_versions(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    if !table_exists(conn, "schema_migrations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    rows.collect()
}

/// Highest recorded schema version, or `0` for an empty or pre-migration database.
pub fn current_version(conn: &Connection) -> rusqlite::Result<i64> {
    Ok(applied_versions(conn)?.last().copied().unwrap_or(0))
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Port of `hasColumn` in `lib/migrations.js`.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Port of `ensureColumn` in `lib/migrations.js`: additive, idempotent `ALTER TABLE`.
fn ensure_column(
    tx: &Transaction<'_>,
    table: &str,
    column: &str,
    declaration: &str,
) -> rusqlite::Result<()> {
    if !has_column(tx, table, column)? {
        tx.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))?;
    }
    Ok(())
}

fn m001_initial_schema(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
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

fn m002_org_label_and_bundles(
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

fn m003_reaction_integrity(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
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

fn m004_artifact_revision(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
    // PBI-009: stable-URL replace-in-place. Each successful update bumps revision.
    ensure_column(
        tx,
        "artifacts",
        "revision",
        "INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1)",
    )
}

fn m005_viewer_feedback(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
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

fn m006_artifact_category(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
    // Category groups artifacts within an org (blank = "Uncategorized" bucket).
    ensure_column(tx, "artifacts", "category", "TEXT NOT NULL DEFAULT ''")?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS artifacts_org_category_idx ON artifacts(org, category, updated_at DESC)",
    )
}

fn m007_org_registry(tx: &Transaction<'_>, ctx: &MigrationContext) -> rusqlite::Result<()> {
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

fn m008_artifact_history(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
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

fn m009_org_discord_webhooks(
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

fn m010_artifact_view_analytics(
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

fn m011_artifact_visibility(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
    // Hidden means unlisted, not private: direct URLs remain tenant-accessible.
    ensure_column(tx, "artifacts", "hidden", "INTEGER NOT NULL DEFAULT 0")?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS artifacts_org_hidden_updated_idx ON artifacts(org, hidden, updated_at DESC)",
    )
}

fn m012_feedback_anchors(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
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

fn m013_feedback_threads(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
    // Replies have one parent only; SQLite permits this nullable FK in ALTER TABLE.
    ensure_column(
        tx,
        "feedback",
        "parent_id",
        "TEXT REFERENCES feedback(id) ON DELETE CASCADE",
    )?;
    tx.execute_batch("CREATE INDEX IF NOT EXISTS feedback_parent_idx ON feedback(parent_id)")
}

fn m014_feedback_anchor_boxes(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-013 extension: NULL dimensions retain the original point-anchor model.
    ensure_column(tx, "feedback", "anchor_w", "REAL")?;
    ensure_column(tx, "feedback", "anchor_h", "REAL")
}

fn m015_artifact_public_shares(
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

fn m016_org_color(tx: &Transaction<'_>, _ctx: &MigrationContext) -> rusqlite::Result<()> {
    // Optional per-org accent color (hex). NULL = derive a stable color from the org name.
    ensure_column(tx, "orgs", "color", "TEXT")
}

fn m017_artifact_body_digest(
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

fn m018_webhook_url_encryption(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // The legacy url column remains required for zero-config plaintext fallback and
    // holds only the safe masked display value when the encrypted columns are set.
    ensure_column(tx, "org_webhooks", "url_cipher", "TEXT")?;
    ensure_column(tx, "org_webhooks", "url_nonce", "TEXT")?;
    ensure_column(tx, "org_webhooks", "url_tag", "TEXT")
}

fn m019_feedback_anchor_page(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-024: NULL retains single-file and pre-page-identity anchor semantics.
    ensure_column(tx, "feedback", "anchor_page", "TEXT")
}

fn m020_notification_read_watermarks(
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

fn m021_explicit_email_org_membership(
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

fn m022_api_key_capabilities(
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

fn m023_verified_artifact_owner(
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

/// Encrypted webhook URL record produced by U04's cipher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedUrl {
    /// Base64 ciphertext.
    pub ciphertext: String,
    /// Base64 nonce.
    pub nonce: String,
    /// Base64 authentication tag.
    pub tag: String,
}

/// Injection seam for the U04 webhook cipher.
///
/// U03 owns the at-rest conversion of legacy plaintext webhook rows (blueprint A4 startup step 5)
/// but must not own the AEAD itself. `src/security/crypto.rs` (U04) provides the implementation;
/// when no key is configured the caller passes `None` and rows stay plaintext, matching
/// `encryptPlaintextWebhookUrls` returning `0` in the Node reference.
pub trait WebhookUrlCipher: Send + Sync {
    /// Encrypts one webhook URL, returning the three at-rest columns.
    ///
    /// # Errors
    /// Returns an [`AppError`] when the configured key cannot encrypt the value.
    fn encrypt_url(&self, plaintext: &str) -> Result<EncryptedUrl, AppError>;
}

/// Port of `maskWebhookUrl` in `lib/migrations.js`.
///
/// The masked value is what remains in the plaintext `url` column once the row is encrypted, so it
/// must never contain the token segment. Node builds `${protocol}//${host}/…${url.slice(-4)}` and
/// falls back to `…${url.slice(-4)}` for unparsable values.
#[must_use]
pub fn mask_webhook_url(value: &str) -> String {
    let suffix = last_four(value);
    url::Url::parse(value).map_or_else(
        |_| format!("…{suffix}"),
        |parsed| {
            let mut host = parsed.host_str().unwrap_or_default().to_owned();
            if let Some(port) = parsed.port() {
                host.push(':');
                host.push_str(&port.to_string());
            }
            format!("{}://{host}/…{suffix}", parsed.scheme())
        },
    )
}

/// Equivalent of JavaScript `value.slice(-4)` for the ASCII URLs stored in this column.
/// Port of JavaScript `url.slice(-4)` — the last four **UTF-16 code units**, not characters.
///
/// `String::chars()` diverges for astral characters: for `https://example.test/🎉🎉🎉`, Node yields
/// `🎉🎉` (each emoji is two UTF-16 units) while a `chars()`-based slice yields `/🎉🎉🎉`. This value
/// is written into the `url` column during plaintext→encrypted conversion, so the divergence would
/// persist at rest. Flagged by U12, which verified the Node behaviour empirically.
///
/// If the 4-unit window splits a surrogate pair, JavaScript emits a lone surrogate; Rust cannot
/// represent one, so the straddling character is dropped (same policy U09 adopted for UTF-16
/// truncation).
fn last_four(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let start = units.len().saturating_sub(4);
    let mut window = &units[start..];
    // A leading trailing-surrogate is the orphaned half of a pair straddling the boundary.
    if window
        .first()
        .is_some_and(|u| (0xDC00..=0xDFFF).contains(u))
    {
        window = &window[1..];
    }
    String::from_utf16_lossy(window)
}

/// Port of `encryptPlaintextWebhookUrls`: converts legacy plaintext rows at rest.
///
/// Selects every row whose ciphertext column is still NULL and whose URL is non-blank, then
/// rewrites `url` to the masked display value and fills the three encrypted columns in one
/// transaction. The UPDATE keeps the `url_cipher IS NULL` guard so a concurrent conversion cannot
/// double-encrypt. Returns the number of converted rows.
///
/// This is blocking work; run it inside `spawn_blocking` or the synchronous bootstrap path.
///
/// # Errors
/// Returns [`AppError::Internal`] if the database rejects the read or write, or propagates the
/// cipher's error. No SQL, path, or URL detail is ever placed in the returned error.
pub fn encrypt_plaintext_webhook_urls(
    conn: &mut Connection,
    cipher: &dyn WebhookUrlCipher,
) -> Result<usize, AppError> {
    let rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, url FROM org_webhooks
                 WHERE url_cipher IS NULL AND trim(url) <> ''",
            )
            .map_err(internal)?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(internal)?;
        mapped
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(internal)?
    };
    if rows.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction().map_err(internal)?;
    let mut changed = 0_usize;
    {
        let mut update = tx
            .prepare(
                "UPDATE org_webhooks
                 SET url = ?1, url_cipher = ?2, url_nonce = ?3, url_tag = ?4
                 WHERE id = ?5 AND url_cipher IS NULL",
            )
            .map_err(internal)?;
        for (id, url) in rows {
            let encrypted = cipher.encrypt_url(&url)?;
            changed += update
                .execute((
                    mask_webhook_url(&url),
                    encrypted.ciphertext,
                    encrypted.nonce,
                    encrypted.tag,
                    id,
                ))
                .map_err(internal)?;
        }
    }
    tx.commit().map_err(internal)?;

    tracing::info!(
        converted = changed,
        "encrypted existing webhook URL(s) at rest"
    );
    Ok(changed)
}

/// Database faults never leak SQL, paths, or row contents into an HTTP body.
fn internal(error: impl Display) -> AppError {
    tracing::error!(error = %error, "webhook url conversion failed");
    AppError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_is_ordered_dense_and_matches_the_frozen_version() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version,
                i64::try_from(index).expect("index fits i64") + 1,
                "migration {} is out of order",
                migration.name
            );
            assert!(!migration.name.is_empty());
        }
        assert_eq!(MIGRATIONS.len(), 23);
        assert_eq!(
            MIGRATIONS.last().map(|migration| migration.version),
            Some(LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    fn masks_webhook_urls_like_the_node_reference() {
        assert_eq!(
            mask_webhook_url("https://discord.com/api/webhooks/123/existing-plaintext-token"),
            "https://discord.com/…oken"
        );
        assert_eq!(
            mask_webhook_url("https://hooks.example.com:8443/abc/secret"),
            "https://hooks.example.com:8443/…cret"
        );
        assert_eq!(mask_webhook_url("not a url"), "… url");
        assert_eq!(mask_webhook_url("abc"), "…abc");
        assert_eq!(mask_webhook_url(""), "…");
    }
}
