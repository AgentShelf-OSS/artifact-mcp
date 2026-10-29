//! Migrations 25 through 27: the security audit ledger and the provider delivery outbox.

use rusqlite::Transaction;

use super::MigrationContext;

pub(super) fn m025_security_audit_ledger(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
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
      ",
    )
}

pub(super) fn m026_security_audit_protocol_hardening(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
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
      ",
    )
}

pub(super) fn m027_provider_delivery_outbox(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-056: delivery data is deliberately independent from `org_webhooks`.  A webhook can be
    // deleted or rotated after an intent was accepted; the worker resolves `secret_ref` through
    // the secret boundary at send time, and no URL/token is ever copied into this durable queue.
    tx.execute_batch(
        "
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
      ",
    )
}
