//! Migrations 30 and 31: Discord organization threading policy and two-way inbound sync.

use rusqlite::Transaction;

use super::MigrationContext;

/// PBI-081 keeps bot credentials in a distinct encrypted-only table.  In particular it must not
/// reuse the URL display column or the global process token: a bot token has no safe masked form
/// and is never a queue, audit, or settings value.
pub(super) fn m030_discord_organization_threading_policy(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    tx.execute_batch(
        "
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
        -- Preserve the deployed PBI-079 explicit opt-outs. A paused row with a generation is an
        -- owner/admin exception (not the virtual local-only default). Existing mirror rows stay
        -- readable through their legacy mapping; they must not turn on the new organization-wide
        -- inherited policy without an administrator's explicit choice.
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
        ",
    )
}

pub(super) fn m031_discord_two_way_inbound_sync(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    let corrupt_feedback: bool = tx.query_row(
        "SELECT EXISTS( \
           SELECT 1 FROM feedback f \
           LEFT JOIN artifacts a ON a.id=f.artifact_id AND a.org=f.org \
           WHERE a.id IS NULL \
         )",
        [],
        |row| row.get(0),
    )?;
    if corrupt_feedback {
        return Err(rusqlite::Error::InvalidQuery);
    }
    tx.execute_batch(
        "
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
        ",
    )
}
