// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { encrypt, parseEncryptionKey, warnIfWebhookEncryptionDisabled } from "./crypto.js";

function hasColumn(db, table, column) {
  return db.prepare(`PRAGMA table_info(${table})`).all().some((entry) => entry.name === column);
}

function maskWebhookUrl(value) {
  const url = String(value || "");
  try {
    const parsed = new URL(url);
    return `${parsed.protocol}//${parsed.host}/…${url.slice(-4)}`;
  } catch {
    return `…${url.slice(-4)}`;
  }
}

export function encryptPlaintextWebhookUrls(db, keyValue = process.env.WEBHOOK_ENC_KEY) {
  const key = parseEncryptionKey(keyValue);
  if (!key) {
    warnIfWebhookEncryptionDisabled(keyValue);
    return 0;
  }

  const rows = db.prepare(`
    SELECT id, url FROM org_webhooks
    WHERE url_cipher IS NULL AND trim(url) <> ''
  `).all();
  if (!rows.length) return 0;

  const update = db.prepare(`
    UPDATE org_webhooks
    SET url = @url, url_cipher = @url_cipher, url_nonce = @url_nonce, url_tag = @url_tag
    WHERE id = @id AND url_cipher IS NULL
  `);
  const encryptedCount = db.transaction(() => {
    let changes = 0;
    for (const row of rows) {
      const encrypted = encrypt(row.url, keyValue);
      changes += update.run({
        id: row.id,
        url: maskWebhookUrl(row.url),
        url_cipher: encrypted.ciphertext,
        url_nonce: encrypted.nonce,
        url_tag: encrypted.tag
      }).changes;
    }
    return changes;
  })();

  console.log(`[artifact-mcp] encrypted ${encryptedCount} existing webhook URL(s) at rest`);
  return encryptedCount;
}

function ensureColumn(db, table, column, declaration) {
  if (!hasColumn(db, table, column)) {
    db.exec(`ALTER TABLE ${table} ADD COLUMN ${column} ${declaration}`);
  }
}

const MIGRATIONS = [
  {
    version: 1,
    name: "initial-schema",
    up(db) {
      db.exec(`
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
      `);
    }
  },
  {
    version: 2,
    name: "org-label-and-bundles",
    up(db) {
      ensureColumn(db, "api_keys", "org", "TEXT NOT NULL DEFAULT 'default'");
      ensureColumn(db, "artifacts", "org", "TEXT NOT NULL DEFAULT 'default'");
      ensureColumn(db, "api_keys", "label", "TEXT NOT NULL DEFAULT ''");
      ensureColumn(db, "artifacts", "uploader_label", "TEXT NOT NULL DEFAULT ''");
      ensureColumn(db, "artifacts", "is_bundle", "INTEGER NOT NULL DEFAULT 0");
      ensureColumn(db, "artifacts", "entry", "TEXT NOT NULL DEFAULT ''");
      db.exec("CREATE INDEX IF NOT EXISTS artifacts_org_idx ON artifacts(org, client_id, created_at DESC)");
    }
  },
  {
    version: 3,
    name: "reaction-integrity",
    up(db) {
      db.exec(`
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
      `);
    }
  },
  {
    version: 4,
    name: "artifact-revision",
    up(db) {
      // PBI-009: stable-URL replace-in-place. Each successful update bumps revision.
      ensureColumn(db, "artifacts", "revision", "INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1)");
    }
  },
  {
    version: 5,
    name: "viewer-feedback",
    up(db) {
      // PBI-010: org-scoped viewer feedback threads. Composite FK ties feedback to the
      // artifact's immutable (id, org) so a viewer can never re-tenant a comment.
      db.exec(`
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
      `);
    }
  },
  {
    version: 6,
    name: "artifact-category",
    up(db) {
      // Category groups artifacts within an org (blank = "Uncategorized" bucket).
      ensureColumn(db, "artifacts", "category", "TEXT NOT NULL DEFAULT ''");
      db.exec("CREATE INDEX IF NOT EXISTS artifacts_org_category_idx ON artifacts(org, category, updated_at DESC)");
    }
  },
  {
    version: 7,
    name: "org-registry",
    up(db) {
      // Persist orgs, their email domains, and their category registry so tenancy is
      // managed in the admin UI instead of the ORG_EMAIL_DOMAINS env var. Domain->org
      // still falls back to the env map, then to "the domain is its own org".
      db.exec(`
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
      `);

      // Seed orgs from tenants already in use (issued keys + published artifacts), minus
      // the admin pseudo-org which is not a real tenant.
      db.exec(`
        INSERT OR IGNORE INTO orgs (name)
        SELECT DISTINCT org FROM (
          SELECT org FROM api_keys
          UNION
          SELECT org FROM artifacts
        ) WHERE org NOT IN ('admin', '') AND org IS NOT NULL;
      `);

      // Seed domain -> org from the ORG_EMAIL_DOMAINS env, creating any missing org.
      const insOrg = db.prepare("INSERT OR IGNORE INTO orgs (name) VALUES (?)");
      const insDom = db.prepare("INSERT OR IGNORE INTO org_domains (domain, org) VALUES (?, ?)");
      for (const pair of String(process.env.ORG_EMAIL_DOMAINS || "").split(",")) {
        const [domain, org] = pair.split(":").map((s) => s.trim());
        if (domain && org && org !== "admin") {
          insOrg.run(org);
          insDom.run(domain.toLowerCase(), org);
        }
      }

      // Seed the category registry from categories already applied to artifacts.
      db.exec(`
        INSERT OR IGNORE INTO org_categories (org, name)
        SELECT DISTINCT org, category FROM artifacts
        WHERE category <> '' AND org NOT IN ('admin', '');
      `);
    }
  },
  {
    version: 8,
    name: "artifact-history",
    up(db) {
      // Version history: each replace-in-place update snapshots the OUTGOING revision's
      // metadata here and its body under .history/<id>/<revision>. Restore replays a past
      // revision as a new one (append-only). Composite FK ties a snapshot to its artifact's
      // immutable (id, org) so cascade delete cleans the rows.
      db.exec(`
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
      `);
    }
  },
  {
    version: 9,
    name: "org-discord-webhooks",
    up(db) {
      db.exec(`
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
      `);
    }
  },
  {
    version: 10,
    name: "artifact-view-analytics",
    up(db) {
      db.exec(`
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
      `);
    }
  },
  {
    version: 11,
    name: "artifact-visibility",
    up(db) {
      // Hidden means unlisted, not private: direct URLs remain tenant-accessible.
      ensureColumn(db, "artifacts", "hidden", "INTEGER NOT NULL DEFAULT 0");
      db.exec("CREATE INDEX IF NOT EXISTS artifacts_org_hidden_updated_idx ON artifacts(org, hidden, updated_at DESC)");
    }
  },
  {
    version: 12,
    name: "feedback-anchors",
    up(db) {
      // PBI-013: a NULL anchor remains the original artifact-wide feedback behavior.
      ensureColumn(db, "feedback", "anchor_path", "TEXT");
      ensureColumn(db, "feedback", "anchor_x", "REAL");
      ensureColumn(db, "feedback", "anchor_y", "REAL");
      ensureColumn(db, "feedback", "anchor_approx", "INTEGER NOT NULL DEFAULT 0");
    }
  },
  {
    version: 13,
    name: "feedback-threads",
    up(db) {
      // Replies have one parent only; SQLite permits this nullable FK in ALTER TABLE.
      ensureColumn(db, "feedback", "parent_id", "TEXT REFERENCES feedback(id) ON DELETE CASCADE");
      db.exec("CREATE INDEX IF NOT EXISTS feedback_parent_idx ON feedback(parent_id)");
    }
  },
  {
    version: 14,
    name: "feedback-anchor-boxes",
    up(db) {
      // PBI-013 extension: NULL dimensions retain the original point-anchor model.
      ensureColumn(db, "feedback", "anchor_w", "REAL");
      ensureColumn(db, "feedback", "anchor_h", "REAL");
    }
  },
  {
    version: 15,
    name: "artifact-public-shares",
    up(db) {
      db.exec(`
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
      `);
    }
  },
  {
    version: 16,
    name: "org-color",
    up(db) {
      // Optional per-org accent color (hex). NULL = derive a stable color from the org name.
      ensureColumn(db, "orgs", "color", "TEXT");
    }
  },
  {
    version: 17,
    name: "artifact-body-digest",
    up(db) {
      // Empty is the explicit legacy/unknown value for rows whose bodies predate this
      // migration; every new publish/update records a SHA-256 commit marker.
      ensureColumn(db, "artifacts", "body_sha256", "TEXT NOT NULL DEFAULT ''");
      ensureColumn(db, "artifact_revisions", "body_sha256", "TEXT NOT NULL DEFAULT ''");
    }
  },
  {
    version: 18,
    name: "webhook-url-encryption",
    up(db) {
      // The legacy url column remains required for zero-config plaintext fallback and
      // holds only the safe masked display value when the encrypted columns are set.
      ensureColumn(db, "org_webhooks", "url_cipher", "TEXT");
      ensureColumn(db, "org_webhooks", "url_nonce", "TEXT");
      ensureColumn(db, "org_webhooks", "url_tag", "TEXT");
    }
  },
  {
    version: 19,
    name: "feedback-anchor-page",
    up(db) {
      // PBI-024: NULL retains single-file and pre-page-identity anchor semantics.
      ensureColumn(db, "feedback", "anchor_page", "TEXT");
    }
  },
  {
    version: 20,
    name: "notification-read-watermarks",
    up(db) {
      // PBI-020: one durable read watermark per verified gallery viewer.
      db.exec(`
        CREATE TABLE IF NOT EXISTS notification_reads (
          viewer_email TEXT PRIMARY KEY,
          seen_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS feedback_created_idx ON feedback(created_at DESC, id DESC);
      `);
    }
  },
  {
    version: 21,
    name: "explicit-email-org-membership",
    up(db) {
      db.exec(`
        CREATE TABLE IF NOT EXISTS org_email_members (
          email      TEXT PRIMARY KEY COLLATE NOCASE,
          org        TEXT NOT NULL,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          FOREIGN KEY (org) REFERENCES orgs(name) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS org_email_members_org_idx ON org_email_members(org, email);
      `);
    }
  },
  {
    version: 22,
    name: "api-key-capabilities",
    up(db) {
      db.exec(`
        ALTER TABLE api_keys ADD COLUMN role TEXT NOT NULL DEFAULT 'author';
        ALTER TABLE artifact_revisions ADD COLUMN client_id TEXT;
      `);
    }
  },
  {
    // Ownership is an authorization primitive.  It is intentionally nullable: existing
    // artifacts and service keys must not gain a guessed human owner during migration.
    version: 23,
    name: "verified-artifact-owner",
    up(db) {
      ensureColumn(db, "api_keys", "owner_email", "TEXT");
      ensureColumn(db, "artifacts", "owner_email", "TEXT");
      db.exec(`
        CREATE INDEX IF NOT EXISTS artifacts_org_owner_visibility_idx
          ON artifacts(org, owner_email, hidden, created_at DESC);
      `);
    }
  }
];

export function migrateDatabase(db) {
  db.exec(`
    CREATE TABLE IF NOT EXISTS schema_migrations (
      version    INTEGER PRIMARY KEY,
      name       TEXT NOT NULL,
      applied_at TEXT NOT NULL DEFAULT (datetime('now'))
    )
  `);

  const applied = new Set(db.prepare("SELECT version FROM schema_migrations").pluck().all());
  const record = db.prepare("INSERT INTO schema_migrations (version, name) VALUES (?, ?)");

  for (const migration of MIGRATIONS) {
    if (applied.has(migration.version)) continue;
    db.transaction(() => {
      migration.up(db);
      record.run(migration.version, migration.name);
    })();
  }

  encryptPlaintextWebhookUrls(db);
}

export const LATEST_SCHEMA_VERSION = MIGRATIONS.at(-1).version;
