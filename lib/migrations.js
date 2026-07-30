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

  const encryptedCount = db.transaction(() => {
    let changes = 0;
    // PBI-079's dedicated discussion destination shares the same encrypted-record lifecycle as
    // ordinary event webhooks. These names are constants, never configuration input.
    for (const [table, key] of [["org_webhooks", "id"], ["org_discord_discussion_connections", "org"]]) {
      const rows = db.prepare(`SELECT ${key}, url FROM ${table} WHERE url_cipher IS NULL AND trim(url) <> ''`).all();
      const update = db.prepare(`UPDATE ${table} SET url = @url, url_cipher = @url_cipher, url_nonce = @url_nonce, url_tag = @url_tag WHERE ${key} = @id AND url_cipher IS NULL`);
      for (const row of rows) {
        const encrypted = encrypt(row.url, keyValue);
        changes += update.run({
          id: row[key],
          url: maskWebhookUrl(row.url),
          url_cipher: encrypted.ciphertext,
          url_nonce: encrypted.nonce,
          url_tag: encrypted.tag
        }).changes;
      }
    }
    return changes;
  })();

  // Preserve the existing quiet startup contract when there was nothing to migrate. Besides
  // avoiding noise, cross-runtime callers can continue treating stdout as their response channel.
  if (encryptedCount > 0) {
    console.log(`[artifact-mcp] encrypted ${encryptedCount} existing webhook URL(s) at rest`);
  }
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
  },
  {
    // PBI-051: durable cross-resource mutations are journaled before their filesystem phase.
    // The table intentionally retains completed rows until the operation has committed; startup
    // can then distinguish a safe orphan from an interrupted publish/update/delete.
    version: 24,
    name: "artifact-durability-intents",
    up(db) {
      db.exec(`
        CREATE TABLE IF NOT EXISTS artifact_durability_intents (
          id            TEXT PRIMARY KEY,
          artifact_id   TEXT NOT NULL,
          operation     TEXT NOT NULL CHECK (operation IN ('publish', 'update', 'delete')),
          state         TEXT NOT NULL CHECK (state IN ('prepared', 'body_durable', 'metadata_committed')),
          expected_sha256 TEXT NOT NULL DEFAULT '',
          prior_sha256  TEXT NOT NULL DEFAULT '',
          staging_path  TEXT NOT NULL DEFAULT '',
          created_at    TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS artifact_durability_intents_artifact_idx
          ON artifact_durability_intents(artifact_id, created_at);
      `);
    }
  },
  {
    // PBI-058: immutable, tenant-scoped security events. Hashes are written by lib/audit.js;
    // the schema deliberately holds only already-redacted identifiers/classifications.
    version: 25,
    name: "security-audit-ledger",
    up(db) {
      db.exec(`
        CREATE TABLE IF NOT EXISTS security_audit_events (
          sequence       INTEGER PRIMARY KEY,
          event_id       TEXT NOT NULL UNIQUE,
          key_id         TEXT NOT NULL DEFAULT 'v1',
          tenant         TEXT NOT NULL,
          actor_type     TEXT NOT NULL CHECK (actor_type IN ('api_key', 'viewer', 'system')),
          actor_id       TEXT NOT NULL,
          actor_role     TEXT NOT NULL DEFAULT '',
          operation      TEXT NOT NULL,
          target_type    TEXT NOT NULL,
          target_id      TEXT NOT NULL DEFAULT '',
          result         TEXT NOT NULL CHECK (result IN ('success', 'denied', 'failure', 'recovered')),
          classification TEXT NOT NULL DEFAULT '',
          source         TEXT NOT NULL CHECK (source IN ('mcp', 'browser', 'maintenance', 'reconciliation')),
          request_id     TEXT NOT NULL DEFAULT '',
          revision       INTEGER,
          occurred_at    TEXT NOT NULL DEFAULT (datetime('now')),
          canonical_version INTEGER NOT NULL DEFAULT 1,
          canonical      BLOB NOT NULL,
          prev_hash      TEXT NOT NULL,
          event_hash     TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS security_audit_events_tenant_sequence_idx
          ON security_audit_events(tenant, sequence DESC);
        CREATE INDEX IF NOT EXISTS security_audit_events_operation_sequence_idx
          ON security_audit_events(operation, sequence DESC);
        CREATE TABLE IF NOT EXISTS security_audit_checkpoints (
          checkpoint_id  INTEGER PRIMARY KEY AUTOINCREMENT,
          first_sequence INTEGER NOT NULL,
          last_sequence  INTEGER NOT NULL,
          key_id         TEXT NOT NULL,
          canonical_version INTEGER NOT NULL,
          bridge_hash    TEXT NOT NULL,
          prev_checkpoint_hash TEXT NOT NULL DEFAULT '',
          checkpoint_hash TEXT NOT NULL,
          pruned_at      TEXT NOT NULL DEFAULT (datetime('now')),
          CHECK (first_sequence <= last_sequence)
        );
        CREATE TABLE IF NOT EXISTS security_audit_chain_head (
          singleton      INTEGER PRIMARY KEY CHECK (singleton = 1),
          sequence       INTEGER NOT NULL DEFAULT 0,
          key_id         TEXT NOT NULL,
          head_hash      TEXT NOT NULL DEFAULT '',
          canonical_version INTEGER NOT NULL DEFAULT 1,
          updated_at     TEXT NOT NULL DEFAULT (datetime('now'))
        );
        INSERT OR IGNORE INTO security_audit_chain_head (singleton, key_id) VALUES (1, 'v1');
        CREATE TABLE IF NOT EXISTS security_audit_receipts (
          correlation_id TEXT PRIMARY KEY,
          durability_intent_id TEXT UNIQUE,
          event_id       TEXT UNIQUE,
          state          TEXT NOT NULL CHECK (state IN ('pending', 'finalized', 'ambiguous')),
          operation      TEXT NOT NULL,
          target_type    TEXT NOT NULL,
          target_id      TEXT NOT NULL DEFAULT '',
          tenant         TEXT NOT NULL,
          created_at     TEXT NOT NULL DEFAULT (datetime('now')),
          finalized_at   TEXT
        );
      `);
    }
  },
  {
    // PBI-058 follow-up: v25 shipped without authenticated tail state/receipt snapshots. Keep
    // historical v25 immutable and upgrade it explicitly rather than changing its DDL in place.
    version: 26,
    name: "security-audit-protocol-hardening",
    up(db) {
      db.exec(`
        ALTER TABLE security_audit_chain_head ADD COLUMN head_mac TEXT NOT NULL DEFAULT '';
        ALTER TABLE security_audit_chain_head ADD COLUMN pending_receipts_root TEXT NOT NULL DEFAULT '';
        ALTER TABLE security_audit_receipts ADD COLUMN result TEXT NOT NULL DEFAULT 'success';
        ALTER TABLE security_audit_receipts ADD COLUMN actor_type TEXT NOT NULL DEFAULT 'system';
        ALTER TABLE security_audit_receipts ADD COLUMN actor_id TEXT NOT NULL DEFAULT 'artifact-mcp';
        ALTER TABLE security_audit_receipts ADD COLUMN actor_role TEXT NOT NULL DEFAULT '';
        ALTER TABLE security_audit_receipts ADD COLUMN source TEXT NOT NULL DEFAULT 'maintenance';
        ALTER TABLE security_audit_receipts ADD COLUMN request_id TEXT NOT NULL DEFAULT '';
        ALTER TABLE security_audit_receipts ADD COLUMN revision INTEGER;
        ALTER TABLE security_audit_receipts ADD COLUMN classification TEXT NOT NULL DEFAULT '';
        ALTER TABLE security_audit_receipts ADD COLUMN key_id TEXT NOT NULL DEFAULT 'v1';
        ALTER TABLE security_audit_receipts ADD COLUMN canonical_version INTEGER NOT NULL DEFAULT 1;
        ALTER TABLE security_audit_receipts ADD COLUMN receipt_mac TEXT NOT NULL DEFAULT '';
      `);
    }
  },
  {
    // PBI-056: no URL/token FK to org_webhooks.  Queued work must survive webhook rotation and
    // resolve its secret reference only inside a future delivery worker.
    version: 27,
    name: "provider-delivery-outbox",
    up(db) {
      db.exec(`
        CREATE TABLE IF NOT EXISTS provider_delivery_outbox (
          id                    TEXT PRIMARY KEY,
          provider              TEXT NOT NULL CHECK (provider = 'discord'),
          event_id              TEXT NOT NULL,
          tenant                TEXT NOT NULL,
          event_type            TEXT NOT NULL,
          target_key            TEXT NOT NULL,
          bucket_id             TEXT NOT NULL,
          secret_ref            TEXT NOT NULL,
          payload               BLOB NOT NULL CHECK (length(payload) <= 32768),
          payload_sha256        TEXT NOT NULL CHECK (length(payload_sha256) = 64),
          durability_intent_id  TEXT,
          state                 TEXT NOT NULL CHECK (state IN ('blocked', 'ready', 'leased', 'retry', 'accepted', 'dead_letter')),
          attempts              INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
          next_attempt_at       INTEGER NOT NULL,
          lease_owner           TEXT,
          lease_expires_at      INTEGER,
          lease_token           TEXT,
          lease_version         INTEGER NOT NULL DEFAULT 0 CHECK (lease_version >= 0),
          result_classification TEXT NOT NULL DEFAULT '',
          duplicate_risk        INTEGER NOT NULL DEFAULT 0 CHECK (duplicate_risk IN (0, 1)),
          discord_message_id    TEXT,
          terminal_error        TEXT NOT NULL DEFAULT '',
          created_at            INTEGER NOT NULL,
          updated_at            INTEGER NOT NULL,
          completed_at          INTEGER,
          UNIQUE (provider, tenant, target_key, event_id),
          FOREIGN KEY (durability_intent_id) REFERENCES artifact_durability_intents(id) ON DELETE RESTRICT,
          CHECK ((state = 'blocked') = (durability_intent_id IS NOT NULL)),
          CHECK (
            (state = 'leased' AND lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL AND lease_token IS NOT NULL)
            OR
            (state <> 'leased' AND lease_owner IS NULL AND lease_expires_at IS NULL AND lease_token IS NULL)
          )
        );
        CREATE INDEX IF NOT EXISTS provider_delivery_outbox_ready_idx
          ON provider_delivery_outbox(state, next_attempt_at, created_at, id);
        CREATE INDEX IF NOT EXISTS provider_delivery_outbox_tenant_idx
          ON provider_delivery_outbox(tenant, state, created_at, id);
        CREATE INDEX IF NOT EXISTS provider_delivery_outbox_target_idx
          ON provider_delivery_outbox(provider, target_key, state, next_attempt_at, id);
        CREATE INDEX IF NOT EXISTS provider_delivery_outbox_intent_idx
          ON provider_delivery_outbox(durability_intent_id, created_at, id);
        CREATE INDEX IF NOT EXISTS provider_delivery_outbox_bucket_idx
          ON provider_delivery_outbox(provider, bucket_id, secret_ref, state, next_attempt_at, id);
        CREATE TABLE IF NOT EXISTS provider_delivery_rate_limits (
          provider      TEXT NOT NULL CHECK (provider = 'discord'),
          scope         TEXT NOT NULL CHECK (scope IN ('global', 'target', 'bucket')),
          target_key    TEXT NOT NULL DEFAULT '',
          bucket_id     TEXT NOT NULL DEFAULT '',
          top_level_secret_ref TEXT NOT NULL DEFAULT '',
          blocked_until INTEGER NOT NULL DEFAULT 0,
          updated_at    INTEGER NOT NULL,
          PRIMARY KEY (provider, scope, target_key, bucket_id, top_level_secret_ref),
          CHECK (
            (scope = 'global' AND target_key = '' AND bucket_id = '' AND top_level_secret_ref = '')
            OR
            (scope = 'target' AND target_key <> '' AND bucket_id = '' AND top_level_secret_ref = '')
            OR
            (scope = 'bucket' AND target_key = '' AND bucket_id <> '' AND top_level_secret_ref <> '')
          )
        );
        CREATE INDEX IF NOT EXISTS provider_delivery_rate_limits_bucket_idx
          ON provider_delivery_rate_limits(provider, scope, bucket_id, top_level_secret_ref, blocked_until);
      `);
    }
  },
  {
    // PBI-079: optional artifact discussion mirroring has a dedicated destination. Existing
    // artifacts get no rows, preserving the local-only default without a migration backfill.
    version: 28,
    name: "discord-discussion-mirror",
    up(db) {
      db.exec(`
        CREATE TABLE IF NOT EXISTS org_discord_discussion_connections (
          id           TEXT PRIMARY KEY,
          org          TEXT NOT NULL UNIQUE REFERENCES orgs(name) ON DELETE CASCADE,
          url          TEXT NOT NULL,
          url_cipher   TEXT,
          url_nonce    TEXT,
          url_tag      TEXT,
          label        TEXT NOT NULL DEFAULT '',
          created_at   TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
          last_ok_at   TEXT,
          last_error   TEXT
        );
        CREATE TABLE IF NOT EXISTS artifact_discussions (
          artifact_id    TEXT PRIMARY KEY,
          org            TEXT NOT NULL,
          provider       TEXT NOT NULL CHECK (provider = 'discord'),
          mode           TEXT NOT NULL CHECK (mode IN ('artifact_only', 'discord_mirror')),
          connection_org TEXT,
          connection_id  TEXT,
          thread_id      TEXT,
          root_message_id TEXT,
          state          TEXT NOT NULL CHECK (state IN ('local', 'pending', 'connected', 'paused', 'failed')),
          generation     INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
          enabled_by     TEXT,
          enabled_at     TEXT,
          disabled_at    TEXT,
          last_synced_at TEXT,
          last_error     TEXT,
          created_at     TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at     TEXT NOT NULL DEFAULT (datetime('now')),
          CHECK (connection_org IS NULL OR connection_org = org),
          CHECK (
            (mode = 'artifact_only' AND state IN ('local', 'paused', 'failed'))
            OR
            (mode = 'discord_mirror' AND state IN ('pending', 'connected', 'failed'))
          ),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE,
          FOREIGN KEY (connection_org) REFERENCES orgs(name) ON DELETE SET NULL,
          FOREIGN KEY (connection_id) REFERENCES org_discord_discussion_connections(id) ON DELETE RESTRICT
        );
        CREATE INDEX IF NOT EXISTS artifact_discussions_org_state_idx
          ON artifact_discussions(org, state, updated_at DESC, artifact_id);
        -- target_key remains rate-limit identity. Discussion work carries its own kind,
        -- aggregate ordering key, and one nullable predecessor without a second source of truth.
        ALTER TABLE provider_delivery_outbox ADD COLUMN delivery_kind TEXT NOT NULL DEFAULT 'event'
          CHECK (delivery_kind IN ('event', 'discussion_thread', 'discussion_message', 'discussion_tombstone'));
        ALTER TABLE provider_delivery_outbox ADD COLUMN ordering_key TEXT NOT NULL DEFAULT '';
        ALTER TABLE provider_delivery_outbox ADD COLUMN depends_on_outbox_id TEXT
          REFERENCES provider_delivery_outbox(id) ON DELETE RESTRICT;
        UPDATE provider_delivery_outbox SET ordering_key = target_key WHERE ordering_key = '';
        CREATE TRIGGER IF NOT EXISTS provider_delivery_outbox_legacy_ordering_key
        AFTER INSERT ON provider_delivery_outbox
        FOR EACH ROW WHEN NEW.ordering_key = ''
        BEGIN
          UPDATE provider_delivery_outbox SET ordering_key = NEW.target_key WHERE id = NEW.id;
        END;
        CREATE INDEX IF NOT EXISTS provider_delivery_outbox_ordering_idx
          ON provider_delivery_outbox(ordering_key, state, created_at, id);
        CREATE INDEX IF NOT EXISTS provider_delivery_outbox_dependency_idx
          ON provider_delivery_outbox(depends_on_outbox_id, state, created_at, id);
        CREATE TABLE IF NOT EXISTS discussion_message_links (
          provider          TEXT NOT NULL CHECK (provider = 'discord'),
          artifact_id       TEXT NOT NULL,
          org               TEXT NOT NULL,
          connection_id     TEXT NOT NULL REFERENCES org_discord_discussion_connections(id) ON DELETE RESTRICT,
          feedback_id       TEXT NOT NULL,
          delivery_event_id TEXT NOT NULL,
          outbox_id         TEXT NOT NULL REFERENCES provider_delivery_outbox(id) ON DELETE RESTRICT,
          tombstone_outbox_id TEXT UNIQUE REFERENCES provider_delivery_outbox(id) ON DELETE RESTRICT,
          external_thread_id TEXT,
          external_message_id TEXT,
          source            TEXT NOT NULL DEFAULT 'artifact' CHECK (source = 'artifact'),
          generation        INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1),
          state             TEXT NOT NULL CHECK (state IN ('pending', 'posted', 'local_deleted', 'tombstone_pending', 'tombstoned', 'failed')),
          last_error        TEXT,
          local_deleted_at  TEXT,
          created_at        TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at        TEXT NOT NULL DEFAULT (datetime('now')),
          posted_at         TEXT,
          PRIMARY KEY (provider, feedback_id, generation),
          UNIQUE (provider, outbox_id),
          UNIQUE (provider, external_message_id),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS discussion_message_links_artifact_idx
          ON discussion_message_links(artifact_id, org, created_at, feedback_id);
        CREATE INDEX IF NOT EXISTS discussion_message_links_outbox_idx
          ON discussion_message_links(outbox_id);
        CREATE TRIGGER IF NOT EXISTS artifact_discussions_connection_tenant_insert
        BEFORE INSERT ON artifact_discussions
        FOR EACH ROW WHEN NEW.connection_id IS NOT NULL AND NOT EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion connection must belong to artifact organization');
        END;
        CREATE TRIGGER IF NOT EXISTS artifact_discussions_connection_tenant_update
        BEFORE UPDATE OF org, connection_id ON artifact_discussions
        FOR EACH ROW WHEN NEW.connection_id IS NOT NULL AND NOT EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion connection must belong to artifact organization');
        END;
        CREATE TRIGGER IF NOT EXISTS discussion_message_links_feedback_tenant_insert
        BEFORE INSERT ON discussion_message_links
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM feedback
           WHERE id = NEW.feedback_id
             AND artifact_id = NEW.artifact_id
             AND org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion message link must match feedback artifact');
        END;
        CREATE TRIGGER IF NOT EXISTS discussion_message_links_connection_tenant_insert
        BEFORE INSERT ON discussion_message_links
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion connection must belong to feedback organization');
        END;
        CREATE TRIGGER IF NOT EXISTS discussion_message_links_connection_tenant_update
        BEFORE UPDATE OF connection_id, org ON discussion_message_links
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion connection must belong to feedback organization');
        END;
        CREATE TRIGGER IF NOT EXISTS discussion_message_links_feedback_tenant_update
        BEFORE UPDATE OF feedback_id, artifact_id, org ON discussion_message_links
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM feedback
           WHERE id = NEW.feedback_id
             AND artifact_id = NEW.artifact_id
             AND org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion message link must match feedback artifact');
        END;
      `);
    }
  },
  {
    version: 29,
    name: "discord-notification-threads",
    up(db) {
      db.exec(`
        ALTER TABLE org_discord_discussion_connections
          ADD COLUMN strategy TEXT NOT NULL DEFAULT 'forum_webhook'
          CHECK (strategy IN ('forum_webhook', 'notification_thread'));
        ALTER TABLE org_discord_discussion_connections
          ADD COLUMN notification_webhook_id TEXT
          REFERENCES org_webhooks(id) ON DELETE RESTRICT;
        ALTER TABLE org_discord_discussion_connections ADD COLUMN channel_id TEXT;
        ALTER TABLE org_discord_discussion_connections ADD COLUMN guild_id TEXT;
        ALTER TABLE artifact_discussions
          ADD COLUMN anchor_outbox_id TEXT
          REFERENCES provider_delivery_outbox(id) ON DELETE RESTRICT;

        CREATE INDEX IF NOT EXISTS artifact_discussions_anchor_idx
          ON artifact_discussions(anchor_outbox_id);
        CREATE INDEX IF NOT EXISTS discussion_connections_notification_webhook_idx
          ON org_discord_discussion_connections(notification_webhook_id);

        CREATE TRIGGER IF NOT EXISTS discussion_connection_notification_insert
        BEFORE INSERT ON org_discord_discussion_connections
        FOR EACH ROW WHEN NEW.strategy = 'notification_thread' AND (
          NEW.notification_webhook_id IS NULL OR NEW.channel_id IS NULL OR NEW.channel_id = ''
          OR NEW.guild_id IS NULL OR NEW.guild_id = ''
          OR NOT EXISTS (
            SELECT 1 FROM org_webhooks w
             WHERE w.id = NEW.notification_webhook_id
               AND w.org = NEW.org
               AND instr(',' || w.events || ',', ',published,') > 0
          )
        )
        BEGIN
          SELECT RAISE(ABORT, 'notification thread webhook must belong to organization and subscribe to published');
        END;
        CREATE TRIGGER IF NOT EXISTS discussion_connection_notification_update
        BEFORE UPDATE OF strategy, notification_webhook_id, channel_id, guild_id, org
          ON org_discord_discussion_connections
        FOR EACH ROW WHEN NEW.strategy = 'notification_thread' AND (
          NEW.notification_webhook_id IS NULL OR NEW.channel_id IS NULL OR NEW.channel_id = ''
          OR NEW.guild_id IS NULL OR NEW.guild_id = ''
          OR NOT EXISTS (
            SELECT 1 FROM org_webhooks w
             WHERE w.id = NEW.notification_webhook_id
               AND w.org = NEW.org
               AND instr(',' || w.events || ',', ',published,') > 0
          )
        )
        BEGIN
          SELECT RAISE(ABORT, 'notification thread webhook must belong to organization and subscribe to published');
        END;
        CREATE TRIGGER IF NOT EXISTS discussion_notification_webhook_events_update
        BEFORE UPDATE OF events ON org_webhooks
        FOR EACH ROW WHEN instr(',' || NEW.events || ',', ',published,') = 0 AND EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.notification_webhook_id = OLD.id
             AND c.org = OLD.org
             AND c.strategy = 'notification_thread'
        )
        BEGIN
          SELECT RAISE(ABORT, 'published event is required by Discord notification threading');
        END;

        CREATE TRIGGER IF NOT EXISTS artifact_discussions_notification_anchor_insert
        BEFORE INSERT ON artifact_discussions
        FOR EACH ROW WHEN NEW.mode = 'discord_mirror' AND EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.strategy = 'notification_thread'
        ) AND (
          NEW.anchor_outbox_id IS NULL OR NOT EXISTS (
            SELECT 1
              FROM provider_delivery_outbox o
              JOIN org_discord_discussion_connections c ON c.id = NEW.connection_id
             WHERE o.id = NEW.anchor_outbox_id
               AND o.provider = 'discord'
               AND o.delivery_kind = 'event'
               AND o.event_type = 'published'
               AND o.tenant = NEW.org
               AND o.target_key = c.notification_webhook_id
          )
        )
        BEGIN
          SELECT RAISE(ABORT, 'notification discussion must reference its publication outbox');
        END;
        CREATE TRIGGER IF NOT EXISTS artifact_discussions_notification_anchor_update
        BEFORE UPDATE OF mode, connection_id, org, anchor_outbox_id ON artifact_discussions
        FOR EACH ROW WHEN NEW.mode = 'discord_mirror' AND EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.strategy = 'notification_thread'
        ) AND (
          NEW.anchor_outbox_id IS NULL OR NOT EXISTS (
            SELECT 1
              FROM provider_delivery_outbox o
              JOIN org_discord_discussion_connections c ON c.id = NEW.connection_id
             WHERE o.id = NEW.anchor_outbox_id
               AND o.provider = 'discord'
               AND o.delivery_kind = 'event'
               AND o.event_type = 'published'
               AND o.tenant = NEW.org
               AND o.target_key = c.notification_webhook_id
          )
        )
        BEGIN
          SELECT RAISE(ABORT, 'notification discussion must reference its publication outbox');
        END;
      `);
    }
  },
  {
    version: 30,
    name: "discord-organization-threading-policy",
    up(db) {
      db.exec(`
        CREATE TABLE org_discord_bot_credentials (
          org        TEXT PRIMARY KEY REFERENCES orgs(name) ON DELETE CASCADE,
          ciphertext TEXT NOT NULL,
          nonce      TEXT NOT NULL,
          tag        TEXT NOT NULL,
          version    INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
          active     INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          deactivated_at TEXT
        );
        CREATE TABLE org_discord_threading_policies (
          org              TEXT PRIMARY KEY REFERENCES orgs(name) ON DELETE CASCADE,
          outbound_enabled INTEGER NOT NULL DEFAULT 0 CHECK (outbound_enabled IN (0, 1)),
          recovery_state   TEXT NOT NULL DEFAULT 'idle'
            CHECK (recovery_state IN ('idle', 'pending', 'recovering', 'degraded', 'complete')),
          recovery_error   TEXT NOT NULL DEFAULT '',
          updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE artifact_discussion_overrides (
          artifact_id TEXT NOT NULL,
          org         TEXT NOT NULL,
          mode        TEXT NOT NULL CHECK (mode = 'artifact_only'),
          created_at  TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (artifact_id, org),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        ALTER TABLE org_discord_discussion_connections
          ADD COLUMN notification_provider_webhook_id TEXT;
        CREATE TABLE discord_notification_anchor_recoveries (
          artifact_id          TEXT NOT NULL,
          org                  TEXT NOT NULL,
          connection_id        TEXT NOT NULL REFERENCES org_discord_discussion_connections(id) ON DELETE RESTRICT,
          notification_webhook_id TEXT NOT NULL,
          provider_webhook_id  TEXT NOT NULL,
          guild_id             TEXT NOT NULL,
          channel_id           TEXT NOT NULL,
          canonical_artifact_url TEXT NOT NULL,
          state                TEXT NOT NULL
            CHECK (state IN ('pending', 'recovering', 'recovered', 'not_found', 'ambiguous', 'permission_denied', 'rate_limited', 'retryable', 'invalid')),
          recovered_message_id TEXT,
          provenance           TEXT NOT NULL DEFAULT ''
            CHECK (provenance IN ('', 'exact_selected_webhook_canonical_url')),
          attempts             INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
          last_error           TEXT NOT NULL DEFAULT '',
          created_at           TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at           TEXT NOT NULL DEFAULT (datetime('now')),
          completed_at         TEXT,
          PRIMARY KEY (artifact_id, org),
          UNIQUE (connection_id, recovered_message_id),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE,
          CHECK (
            (state = 'recovered' AND recovered_message_id IS NOT NULL AND provenance = 'exact_selected_webhook_canonical_url')
            OR (state <> 'recovered' AND recovered_message_id IS NULL AND provenance = '')
          )
        );
        DROP TRIGGER artifact_discussions_notification_anchor_insert;
        DROP TRIGGER artifact_discussions_notification_anchor_update;
        CREATE TRIGGER artifact_discussions_notification_anchor_insert
        BEFORE INSERT ON artifact_discussions
        FOR EACH ROW WHEN NEW.mode = 'discord_mirror' AND EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.strategy = 'notification_thread'
        ) AND NOT (
          (NEW.anchor_outbox_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM provider_delivery_outbox o
            JOIN org_discord_discussion_connections c ON c.id = NEW.connection_id
            WHERE o.id = NEW.anchor_outbox_id AND o.provider = 'discord'
              AND o.delivery_kind = 'event' AND o.event_type = 'published'
              AND o.tenant = NEW.org AND o.target_key = c.notification_webhook_id
          )) OR (NEW.anchor_outbox_id IS NULL AND EXISTS (
            SELECT 1 FROM discord_notification_anchor_recoveries r
            JOIN org_discord_discussion_connections c ON c.id = NEW.connection_id
            WHERE r.artifact_id = NEW.artifact_id AND r.org = NEW.org
              AND r.connection_id = NEW.connection_id AND r.state = 'recovered'
              AND r.provenance = 'exact_selected_webhook_canonical_url'
              AND r.notification_webhook_id = c.notification_webhook_id
              AND r.provider_webhook_id = c.notification_provider_webhook_id
              AND r.guild_id = c.guild_id AND r.channel_id = c.channel_id
          ))
        )
        BEGIN
          SELECT RAISE(ABORT, 'notification discussion must reference exact publication evidence');
        END;
        CREATE TRIGGER artifact_discussions_notification_anchor_update
        BEFORE UPDATE OF mode, connection_id, org, anchor_outbox_id ON artifact_discussions
        FOR EACH ROW WHEN NEW.mode = 'discord_mirror' AND EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.strategy = 'notification_thread'
        ) AND NOT (
          (NEW.anchor_outbox_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM provider_delivery_outbox o
            JOIN org_discord_discussion_connections c ON c.id = NEW.connection_id
            WHERE o.id = NEW.anchor_outbox_id AND o.provider = 'discord'
              AND o.delivery_kind = 'event' AND o.event_type = 'published'
              AND o.tenant = NEW.org AND o.target_key = c.notification_webhook_id
          )) OR (NEW.anchor_outbox_id IS NULL AND EXISTS (
            SELECT 1 FROM discord_notification_anchor_recoveries r
            JOIN org_discord_discussion_connections c ON c.id = NEW.connection_id
            WHERE r.artifact_id = NEW.artifact_id AND r.org = NEW.org
              AND r.connection_id = NEW.connection_id AND r.state = 'recovered'
              AND r.provenance = 'exact_selected_webhook_canonical_url'
              AND r.notification_webhook_id = c.notification_webhook_id
              AND r.provider_webhook_id = c.notification_provider_webhook_id
              AND r.guild_id = c.guild_id AND r.channel_id = c.channel_id
          ))
        )
        BEGIN
          SELECT RAISE(ABORT, 'notification discussion must reference exact publication evidence');
        END;
        INSERT OR IGNORE INTO artifact_discussion_overrides (artifact_id, org, mode)
        SELECT artifact_id, org, 'artifact_only'
          FROM artifact_discussions
         WHERE mode = 'artifact_only' AND generation > 0;
        CREATE INDEX artifact_discussion_overrides_org_idx
          ON artifact_discussion_overrides(org, artifact_id);
        CREATE INDEX discord_notification_anchor_recoveries_work_idx
          ON discord_notification_anchor_recoveries(org, state, updated_at, artifact_id);
        CREATE TRIGGER discord_notification_anchor_recoveries_tenant_insert
        BEFORE INSERT ON discord_notification_anchor_recoveries
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.org = NEW.org
             AND c.strategy = 'notification_thread'
             AND c.notification_webhook_id = NEW.notification_webhook_id
             AND c.notification_provider_webhook_id = NEW.provider_webhook_id
             AND c.guild_id = NEW.guild_id AND c.channel_id = NEW.channel_id
        )
        BEGIN
          SELECT RAISE(ABORT, 'recovery destination must belong to artifact organization');
        END;
        CREATE TRIGGER discord_notification_anchor_recoveries_tenant_update
        BEFORE UPDATE OF org, connection_id, notification_webhook_id, provider_webhook_id, guild_id, channel_id
          ON discord_notification_anchor_recoveries
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM org_discord_discussion_connections c
           WHERE c.id = NEW.connection_id AND c.org = NEW.org
             AND c.strategy = 'notification_thread'
             AND c.notification_webhook_id = NEW.notification_webhook_id
             AND c.notification_provider_webhook_id = NEW.provider_webhook_id
             AND c.guild_id = NEW.guild_id AND c.channel_id = NEW.channel_id
        )
        BEGIN
          SELECT RAISE(ABORT, 'recovery destination must belong to artifact organization');
        END;
      `);
    }
  },
  {
    version: 31,
    name: "discord-two-way-inbound-sync",
    up(db) {
      const corruptFeedback = db.prepare(`
        SELECT EXISTS(
          SELECT 1 FROM feedback f
          LEFT JOIN artifacts a ON a.id=f.artifact_id AND a.org=f.org
          WHERE a.id IS NULL
        ) AS present
      `).get().present;
      if (corruptFeedback) throw new Error("feedback tenant corruption blocks schema v31 migration");
      db.exec(`
        PRAGMA defer_foreign_keys=ON;
        DROP TRIGGER discussion_message_links_feedback_tenant_insert;
        DROP TRIGGER discussion_message_links_feedback_tenant_update;
        ALTER TABLE feedback RENAME TO feedback_v30;
        CREATE TABLE feedback (
          id               TEXT PRIMARY KEY,
          artifact_id      TEXT NOT NULL,
          org              TEXT NOT NULL,
          viewer_email     TEXT,
          body             TEXT NOT NULL CHECK (length(trim(body)) BETWEEN 1 AND 4000),
          artifact_revision INTEGER NOT NULL CHECK (artifact_revision >= 1),
          created_at       TEXT NOT NULL DEFAULT (datetime('now')),
          resolved_at      TEXT,
          resolved_by      TEXT,
          anchor_path      TEXT,
          anchor_x         REAL,
          anchor_y         REAL,
          anchor_approx    INTEGER NOT NULL DEFAULT 0,
          parent_id        TEXT REFERENCES feedback(id) ON DELETE CASCADE,
          anchor_w         REAL,
          anchor_h         REAL,
          anchor_page      TEXT,
          author_source    TEXT NOT NULL DEFAULT 'artifact'
            CHECK (author_source IN ('artifact', 'discord')),
          external_author_id TEXT,
          external_author_display TEXT,
          external_created_at TEXT,
          external_edited_at TEXT,
          external_deleted_at TEXT,
          CHECK ((resolved_at IS NULL) = (resolved_by IS NULL)),
          CHECK (
            (author_source='artifact' AND viewer_email IS NOT NULL
              AND external_author_id IS NULL AND external_author_display IS NULL)
            OR
            (author_source='discord' AND viewer_email IS NULL
              AND length(external_author_id) BETWEEN 1 AND 32
              AND length(trim(external_author_display)) BETWEEN 1 AND 160)
          ),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        INSERT INTO feedback (
          id, artifact_id, org, viewer_email, body, artifact_revision, created_at,
          resolved_at, resolved_by, anchor_path, anchor_x, anchor_y, anchor_approx,
          parent_id, anchor_w, anchor_h, anchor_page, author_source
        )
        SELECT id, artifact_id, org, viewer_email, body, artifact_revision, created_at,
          resolved_at, resolved_by, anchor_path, anchor_x, anchor_y, anchor_approx,
          parent_id, anchor_w, anchor_h, anchor_page, 'artifact'
        FROM feedback_v30;
        DROP TABLE feedback_v30;
        CREATE INDEX feedback_thread_idx ON feedback(artifact_id, resolved_at, created_at, id);
        CREATE INDEX feedback_org_idx ON feedback(org, resolved_at, created_at DESC, id DESC);
        CREATE INDEX feedback_parent_idx ON feedback(parent_id);
        CREATE INDEX feedback_created_idx ON feedback(created_at DESC, id DESC);
        CREATE TRIGGER discussion_message_links_feedback_tenant_insert
        BEFORE INSERT ON discussion_message_links
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM feedback
           WHERE id = NEW.feedback_id
             AND artifact_id = NEW.artifact_id
             AND org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion message link must match feedback artifact');
        END;
        CREATE TRIGGER discussion_message_links_feedback_tenant_update
        BEFORE UPDATE OF feedback_id, artifact_id, org ON discussion_message_links
        FOR EACH ROW WHEN NOT EXISTS (
          SELECT 1 FROM feedback
           WHERE id = NEW.feedback_id
             AND artifact_id = NEW.artifact_id
             AND org = NEW.org
        )
        BEGIN
          SELECT RAISE(ABORT, 'discussion message link must match feedback artifact');
        END;
        CREATE TABLE artifact_discord_inbound_policies (
          artifact_id TEXT NOT NULL,
          org         TEXT NOT NULL,
          enabled     INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
          health      TEXT NOT NULL DEFAULT 'disabled'
            CHECK (health IN ('disabled','connecting','ready','reconnecting','degraded','failed')),
          safe_error  TEXT NOT NULL DEFAULT ''
            CHECK (safe_error IN ('','missing_credential','message_content_intent','guild_access','thread_permission','gateway_unavailable','thread_unavailable')),
          updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (artifact_id, org),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE
        );
        CREATE INDEX artifact_discord_inbound_policies_org_idx
          ON artifact_discord_inbound_policies(org, enabled, health, artifact_id);
        CREATE TABLE discord_inbound_events (
          provider TEXT NOT NULL CHECK (provider = 'discord'),
          event_id TEXT NOT NULL,
          org TEXT NOT NULL REFERENCES orgs(name) ON DELETE CASCADE,
          gateway_session_id TEXT NOT NULL,
          guild_id TEXT NOT NULL,
          thread_id TEXT NOT NULL,
          message_id TEXT,
          event_type TEXT NOT NULL
            CHECK (event_type IN ('message_create','message_update','message_delete','thread_update','thread_delete')),
          provider_version INTEGER,
          payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
          received_at TEXT NOT NULL DEFAULT (datetime('now')),
          processed_at TEXT,
          next_attempt_at TEXT,
          attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 20),
          result TEXT NOT NULL
            CHECK (result IN ('received','applied','duplicate','ignored','rejected','needs_fetch','degraded','failed')),
          safe_error TEXT NOT NULL DEFAULT '',
          PRIMARY KEY (provider, org, gateway_session_id, event_id)
        );
        CREATE INDEX discord_inbound_events_retention_idx
          ON discord_inbound_events(processed_at, received_at);
        CREATE INDEX discord_inbound_events_org_thread_idx
          ON discord_inbound_events(org, guild_id, thread_id, received_at);
        CREATE TABLE discord_inbound_message_state (
          provider TEXT NOT NULL CHECK (provider = 'discord'),
          external_message_id TEXT NOT NULL,
          org TEXT NOT NULL REFERENCES orgs(name) ON DELETE CASCADE,
          artifact_id TEXT NOT NULL,
          feedback_id TEXT,
          external_thread_id TEXT NOT NULL,
          external_author_id TEXT NOT NULL,
          external_author_display TEXT NOT NULL,
          provider_version INTEGER NOT NULL DEFAULT 0,
          external_created_at TEXT,
          external_edited_at TEXT,
          external_deleted_at TEXT,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (provider, org, external_message_id),
          UNIQUE (provider, org, feedback_id),
          FOREIGN KEY (artifact_id, org) REFERENCES artifacts(id, org) ON DELETE CASCADE,
          FOREIGN KEY (feedback_id) REFERENCES feedback(id) ON DELETE SET NULL
        );
        CREATE INDEX discord_inbound_message_state_artifact_idx
          ON discord_inbound_message_state(artifact_id, org, external_thread_id);
        CREATE TRIGGER discord_inbound_message_state_feedback_insert
        BEFORE INSERT ON discord_inbound_message_state
        FOR EACH ROW WHEN NEW.feedback_id IS NOT NULL AND NOT EXISTS (
          SELECT 1 FROM feedback
           WHERE id=NEW.feedback_id AND artifact_id=NEW.artifact_id AND org=NEW.org
             AND author_source='discord'
        )
        BEGIN
          SELECT RAISE(ABORT, 'inbound message must match Discord feedback tenant');
        END;
        CREATE TRIGGER discord_inbound_message_state_feedback_update
        BEFORE UPDATE OF feedback_id, artifact_id, org ON discord_inbound_message_state
        FOR EACH ROW WHEN NEW.feedback_id IS NOT NULL AND NOT EXISTS (
          SELECT 1 FROM feedback
           WHERE id=NEW.feedback_id AND artifact_id=NEW.artifact_id AND org=NEW.org
             AND author_source='discord'
        )
        BEGIN
          SELECT RAISE(ABORT, 'inbound message must match Discord feedback tenant');
        END;
        CREATE TABLE discord_gateway_sessions (
          org TEXT PRIMARY KEY REFERENCES orgs(name) ON DELETE CASCADE,
          credential_version INTEGER NOT NULL DEFAULT 0 CHECK (credential_version >= 0),
          session_id TEXT,
          resume_gateway_url TEXT,
          last_sequence INTEGER,
          health TEXT NOT NULL DEFAULT 'disabled'
            CHECK (health IN ('disabled','connecting','ready','reconnecting','degraded','failed')),
          safe_error TEXT NOT NULL DEFAULT '',
          updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
      `);
    }
  }
];

/**
 * Apply the append-only ledger through a historical boundary.
 *
 * `migrateDatabase` is the production entry point and always uses the latest version.  This
 * explicit boundary seam exists solely to materialize immutable, SQLite-aware recovery fixtures:
 * it runs the real migration functions and records the real ledger rows, never manufactures an
 * old database by deleting rows from a newer one.
 */
export function migrateDatabaseThrough(db, targetVersion = MIGRATIONS.at(-1).version) {
  if (!Number.isInteger(targetVersion) || targetVersion < 0 || targetVersion > MIGRATIONS.at(-1).version) {
    throw new RangeError(`Unsupported schema version: ${targetVersion}`);
  }
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
    if (migration.version > targetVersion) break;
    if (applied.has(migration.version)) continue;
    db.transaction(() => {
      migration.up(db);
      record.run(migration.version, migration.name);
    })();
  }

  if (targetVersion === MIGRATIONS.at(-1).version) encryptPlaintextWebhookUrls(db);
}

export function migrateDatabase(db) {
  migrateDatabaseThrough(db);
}

export const LATEST_SCHEMA_VERSION = MIGRATIONS.at(-1).version;
