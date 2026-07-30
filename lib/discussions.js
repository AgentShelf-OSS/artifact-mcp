// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Node compatibility persistence for PBI-079. This is deliberately a status and
// configuration oracle only: Rust owns production discussion delivery workers.

import { customAlphabet } from "nanoid";
import db from "./db.js";
import { decrypt, encrypt, parseEncryptionKey } from "./crypto.js";
import { isDiscordWebhookUrl } from "./discord-webhook-url.js";
import { deliverDiscordWebhook } from "./discord-delivery.js";

const generateId = customAlphabet("0123456789abcdefghijkmnpqrstuvwxyz", 21);
const safeError = "delivery_failed";

function problem(message, status = 400) {
  const error = new Error(message);
  error.status = status;
  return error;
}

function text(value, max = 320) {
  return String(value || "").trim().slice(0, max);
}

function maskUrl(value) {
  try {
    const url = new URL(String(value || ""));
    return `${url.protocol}//${url.host}/…${String(value).slice(-4)}`;
  } catch {
    return `…${String(value || "").slice(-4)}`;
  }
}

function reveal(row) {
  return row?.url_cipher ? decrypt(row) : row?.url;
}

function connectionSummary(row) {
  return row
    ? { configured: true, label: row.label || "", destination: maskUrl(row.url), lastError: row.last_error ? safeError : null }
    : { configured: false, label: "", destination: "", lastError: null };
}

function discussionSummary(row, configured) {
  if (!row) {
    return { mode: "artifact_only", state: "local", enabled: false, connectionConfigured: configured, lastError: null };
  }
  return {
    mode: row.mode,
    state: row.state,
    enabled: row.mode === "discord_mirror",
    connectionConfigured: configured,
    lastError: row.last_error ? safeError : null
  };
}

function event(operation, targetType, targetId, classification, result = "success") {
  return { operation, targetType, targetId, result, classification };
}

/**
 * A dependency-injectable store keeps Node's compatibility behavior independently testable.
 * Every semantic mutation and its ledger row share one SQLite transaction.
 */
export function createDiscussionStore({ database = db, fetchImpl = globalThis.fetch } = {}) {
  const rowForOrg = database.prepare("SELECT * FROM org_discord_discussion_connections WHERE org = ?");
  const discussionFor = database.prepare("SELECT * FROM artifact_discussions WHERE artifact_id = ? AND org = ?");
  const orgExists = database.prepare("SELECT 1 FROM orgs WHERE name = ?");
  const hasAuthority = database.prepare(
    "SELECT EXISTS(SELECT 1 FROM artifact_discussions WHERE org = ? AND connection_id IS NOT NULL) OR EXISTS(SELECT 1 FROM discussion_message_links WHERE org = ?) AS bound"
  );

  function requireAudit(audit, context) {
    if (!audit?.appendInTransaction || !context) throw problem("audit_unavailable", 503);
  }
  function append(audit, context, value) { audit.appendInTransaction(context, value); }
  function configured(org) { return !!rowForOrg.get(org); }
  function authorityBound(org) { return !!hasAuthority.get(org, org)?.bound; }
  function current(org) { return connectionSummary(rowForOrg.get(org)); }

  function configure({ org, url, label, audit, context }) {
    org = text(org, 80); url = text(url, 4_096); label = text(label, 80);
    if (!isDiscordWebhookUrl(url)) throw problem("Webhook URL must be an HTTPS Discord webhook URL.");
    requireAudit(audit, context);
    const encryptionKey = parseEncryptionKey();
    const encrypted = encryptionKey ? encrypt(url) : null;
    return database.transaction(() => {
      if (!orgExists.get(org)) throw problem(`Unknown organization \"${org}\".`);
      const existing = rowForOrg.get(org);
      if (existing && authorityBound(org)) {
        throw problem("Discord discussion connection cannot be replaced after durable discussion authority is bound.", 409);
      }
      if (existing) database.prepare("DELETE FROM org_discord_discussion_connections WHERE org = ?").run(org);
      const row = {
        id: generateId(), org,
        url: encrypted ? maskUrl(url) : url,
        url_cipher: encrypted?.ciphertext ?? null,
        url_nonce: encrypted?.nonce ?? null,
        url_tag: encrypted?.tag ?? null,
        label
      };
      database.prepare("INSERT INTO org_discord_discussion_connections (id, org, url, url_cipher, url_nonce, url_tag, label) VALUES (@id, @org, @url, @url_cipher, @url_nonce, @url_tag, @label)").run(row);
      append(audit, context, event("discussion.connection.configure", "organization", org, "discussion_connection_configured"));
      return connectionSummary(rowForOrg.get(org));
    })();
  }

  function remove({ org, audit, context }) {
    org = text(org, 80); requireAudit(audit, context);
    return database.transaction(() => {
      // Absence is a no-op: do not mint an audit event or turn it into a conflict.
      if (!rowForOrg.get(org)) return false;
      if (authorityBound(org)) throw problem("Discord discussion connection cannot be removed after durable discussion authority is bound.", 409);
      database.prepare("DELETE FROM org_discord_discussion_connections WHERE org = ?").run(org);
      append(audit, context, event("discussion.connection.remove", "organization", org, "discussion_connection_removed"));
      return true;
    })();
  }

  function status({ artifact }) {
    const org = text(artifact?.org, 80);
    return discussionSummary(discussionFor.get(artifact?.id, org), configured(org));
  }

  function setMode({ artifact, mode, actor, audit, context }) {
    const id = text(artifact?.id, 240); const org = text(artifact?.org, 80);
    actor = text(actor, 320); requireAudit(audit, context);
    if (mode !== "artifact_only" && mode !== "discord_mirror") throw problem("invalid discussion mode request");
    return database.transaction(() => {
      const existing = discussionFor.get(id, org);
      const unchanged = (mode === "artifact_only" && (!existing || existing.mode === "artifact_only"))
        || (mode === "discord_mirror" && existing?.mode === "discord_mirror");
      if (unchanged) return discussionSummary(existing, configured(org));
      if (mode === "artifact_only") {
        database.prepare("UPDATE artifact_discussions SET mode='artifact_only', state='paused', disabled_at=datetime('now'), updated_at=datetime('now') WHERE artifact_id=? AND org=?").run(id, org);
      } else {
        const connection = rowForOrg.get(org);
        if (!connection) throw problem("Discord discussion connection is not configured.");
        database.prepare(`INSERT INTO artifact_discussions
          (artifact_id, org, provider, mode, connection_org, connection_id, state, generation, enabled_by, enabled_at)
          VALUES (?, ?, 'discord', 'discord_mirror', ?, ?, 'pending', 1, ?, datetime('now'))
          ON CONFLICT(artifact_id) DO UPDATE SET org=excluded.org, provider='discord', mode='discord_mirror',
            connection_org=excluded.connection_org, connection_id=excluded.connection_id, state='pending',
            generation=artifact_discussions.generation+1, enabled_by=excluded.enabled_by, enabled_at=datetime('now'),
            disabled_at=NULL, thread_id=NULL, root_message_id=NULL, last_synced_at=NULL, last_error=NULL, updated_at=datetime('now')`
        ).run(id, org, org, connection.id, actor);
      }
      append(audit, context, event("discussion.mode.set", "artifact", id, "discussion_mode_updated"));
      return discussionSummary(discussionFor.get(id, org), configured(org));
    })();
  }

  function retry({ artifact, actor, audit, context }) {
    const id = text(artifact?.id, 240); const org = text(artifact?.org, 80);
    actor = text(actor, 320); requireAudit(audit, context);
    return database.transaction(() => {
      const existing = discussionFor.get(id, org);
      if (!existing || existing.mode !== "discord_mirror") throw problem("Discord discussion mirroring is not enabled.", 409);
      // Retry is a mutation only from failed; an already pending/connected request is idempotent.
      if (existing.state !== "failed") return discussionSummary(existing, configured(org));
      const connection = rowForOrg.get(org);
      if (!connection) throw problem("Discord discussion connection is not configured.");
      database.prepare("UPDATE artifact_discussions SET connection_org=?, connection_id=?, state='pending', generation=generation+1, enabled_by=?, enabled_at=datetime('now'), thread_id=NULL, root_message_id=NULL, last_synced_at=NULL, last_error=NULL, updated_at=datetime('now') WHERE artifact_id=? AND org=? AND mode='discord_mirror' AND state='failed'")
        .run(org, connection.id, actor, id, org);
      append(audit, context, event("discussion.mode.retry", "artifact", id, "discussion_retry_new_generation"));
      return discussionSummary(discussionFor.get(id, org), true);
    })();
  }

  async function testConnection({ org, audit, context }) {
    org = text(org, 80); requireAudit(audit, context);
    // The pre-I/O event binds the attempt to the immutable credential id, not an org alias.
    const target = database.transaction(() => {
      const row = rowForOrg.get(org);
      if (!row) throw problem("Discord discussion connection is not configured.");
      append(audit, context, event("discussion.connection.test.requested", "discussion_connection", row.id, "external_delivery_requested"));
      // Keep the credential in a delivery-only closure. It is intentionally not projected into
      // audit records, API output, logger calls, or a returned value from this public method.
      return { id: row.id, row };
    })();
    let accepted = false;
    let detached = false;
    try {
      const url = reveal(target.row);
      const result = await deliverDiscordWebhook({
        webhookUrl: url,
        webhookRef: `webhook:${target.id}`,
        body: JSON.stringify({
          content: "Artifact MCP Discord discussion connection test. This visible post confirms delivery and is not linked to an artifact.",
          thread_name: "Artifact MCP connection test"
        }),
        headers: { "content-type": "application/json" },
        fetchImpl
      });
      accepted = result.state === "accepted";
    } catch {
      accepted = false;
    } finally {
      // This completion is attempted for every post-request outcome. Provider details never
      // cross this fixed classification boundary or become an API response.
      database.transaction(() => {
        const update = database.prepare("UPDATE org_discord_discussion_connections SET last_ok_at=CASE WHEN ? THEN datetime('now') ELSE last_ok_at END, last_error=CASE WHEN ? THEN NULL ELSE ? END, updated_at=datetime('now') WHERE org=? AND id=?")
          .run(accepted ? 1 : 0, accepted ? 1 : 0, safeError, org, target.id);
        if (update.changes !== 1) { accepted = false; detached = true; }
        append(audit, context, event(
          "discussion.connection.test.completed", "discussion_connection", target.id,
          accepted ? "external_delivery_succeeded" : "external_delivery_failed",
          accepted ? "success" : "failure"
        ));
      })();
    }
    if (detached) throw problem("Discord discussion connection changed during testing.", 409);
    if (!accepted) throw problem("Discord discussion unavailable.", 503);
    return accepted;
  }

  return { connection: current, configure, remove, status, setMode, retry, testConnection };
}

const discussions = createDiscussionStore();
export const connection = discussions.connection;
export const configure = discussions.configure;
export const remove = discussions.remove;
export const status = discussions.status;
export const setMode = discussions.setMode;
export const retry = discussions.retry;
export const testConnection = discussions.testConnection;
