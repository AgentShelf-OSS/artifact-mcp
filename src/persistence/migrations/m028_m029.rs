//! Migrations 28 and 29: the Discord discussion mirror and notification threads.

use rusqlite::Transaction;

use super::MigrationContext;

pub(super) fn m028_discord_discussion_mirror(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // PBI-079: discussion mirroring is deliberately separate from event webhooks.  In
    // particular, a normal `org_webhooks` row is not an implicit discussion destination.
    // Artifact rows are intentionally absent until an owner/admin opts in (or retains a paused
    // historical mirror), so every existing artifact stays local-only without a backfill.
    tx.execute_batch(
        "
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
        -- `target_key` remains rate-limit identity. Discussion jobs receive their own kind,
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
      ",
    )
}

pub(super) fn m029_discord_notification_threads(
    tx: &Transaction<'_>,
    _ctx: &MigrationContext,
) -> rusqlite::Result<()> {
    // A notification-thread connection reuses one existing org webhook for message delivery and
    // gives the bot only the narrower thread-management job. The publication outbox row is the
    // durable anchor: a comment thread cannot race ahead of the notification it belongs to.
    tx.execute_batch(
        "
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
        ",
    )
}
