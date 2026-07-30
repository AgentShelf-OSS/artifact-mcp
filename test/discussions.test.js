// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";

const importDataDir = mkdtempSync(path.join(tmpdir(), "artifact-discussions-import-"));
process.env.DATA_DIR = importDataDir;
const { createDiscussionStore } = await import("../lib/discussions.js");

test.after(() => rmSync(importDataDir, { recursive: true, force: true }));

function fixture({ failAudit = false } = {}) {
  const database = new Database(":memory:");
  database.pragma("foreign_keys = ON");
  migrateDatabase(database);
  database.prepare("INSERT INTO orgs (name, label) VALUES ('acme', 'Acme')").run();
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('artifact-1', 'publisher', 'acme', 'One')").run();
  const events = [];
  const contexts = [];
  const audit = { appendInTransaction(valueContext, value) {
    if (failAudit) throw new Error("ledger unavailable");
    contexts.push(valueContext);
    events.push(value);
  } };
  const context = { tenant: "acme", actorType: "viewer", actorId: "admin@acme.test", actorRole: "admin", source: "browser", requestId: "browser-correlation" };
  const requests = [];
  const store = createDiscussionStore({
    database,
    fetchImpl: async (url, options) => {
      requests.push({ url, body: JSON.parse(options.body) });
      return new Response(JSON.stringify({ id: "123456789012345678" }), { status: 200 });
    }
  });
  return { database, store, audit, context, events, contexts, requests, artifact: { id: "artifact-1", org: "acme" } };
}

test("discussion settings expose only fixed-safe summaries and audit real semantic changes", async () => {
  const { database, store, audit, context, events, contexts, requests, artifact } = fixture();
  try {
    assert.deepEqual(store.connection("acme"), { configured: false, label: "", destination: "", lastError: null });
    assert.deepEqual(store.status({ artifact }), {
      mode: "artifact_only", state: "local", enabled: false, connectionConfigured: false, lastError: null
    });

    const url = "https://discord.com/api/webhooks/123456/discussion-secret";
    const connection = store.configure({ org: "acme", url, label: "release forum", audit, context });
    assert.deepEqual(Object.keys(connection).sort(), ["configured", "destination", "label", "lastError"].sort());
    assert.equal(connection.configured, true);
    assert.equal(connection.label, "release forum");
    assert.doesNotMatch(JSON.stringify(connection), /discussion-secret|123456/);
    assert.equal(events.length, 1);
    assert.equal(events[0].operation, "discussion.connection.configure");
    assert.equal(store.configure({ org: "acme", url, label: "release forum", audit, context }).configured, true);
    assert.equal(events.length, 2, "the Node oracle preserves Rust's immutable replacement semantics");

    const enabled = store.setMode({ artifact, mode: "discord_mirror", actor: context.actorId, audit, context });
    assert.deepEqual(enabled, { mode: "discord_mirror", state: "pending", enabled: true, connectionConfigured: true, lastError: null });
    const generation = database.prepare("SELECT generation FROM artifact_discussions WHERE artifact_id = 'artifact-1'").pluck().get();
    assert.deepEqual(store.setMode({ artifact, mode: "discord_mirror", actor: context.actorId, audit, context }), enabled);
    assert.equal(database.prepare("SELECT generation FROM artifact_discussions WHERE artifact_id = 'artifact-1'").pluck().get(), generation);
    assert.equal(events.length, 3, "an already-enabled mirror is idempotent");

    assert.throws(
      () => store.configure({ org: "acme", url: "https://discord.com/api/webhooks/999999/other-secret", label: "new", audit, context }),
      (error) => error.status === 409
    );
    assert.throws(() => store.remove({ org: "acme", audit, context }), (error) => error.status === 409);

    const tested = await store.testConnection({ org: "acme", audit, context });
    assert.equal(tested, true);
    assert.equal(requests.length, 1);
    assert.equal(requests[0].body.thread_name, "Artifact MCP connection test");
    assert.match(requests[0].body.content, /visible post confirms delivery/);
    assert.match(requests[0].url, /\?wait=true$/);
    assert.deepEqual(events.slice(-2).map((value) => value.classification), ["external_delivery_requested", "external_delivery_succeeded"]);
    assert.deepEqual(events.slice(-2).map((value) => value.operation), ["discussion.connection.test.requested", "discussion.connection.test.completed"]);
    assert.ok(events.slice(-2).every((value) => value.targetType === "discussion_connection"));
    assert.deepEqual(contexts.slice(-2).map((value) => value.requestId), ["browser-correlation", "browser-correlation"]);
  } finally {
    database.close();
  }
});

test("a detached connection test completes against the original credential id and fails closed", async () => {
  const { database, store, audit, context, events } = fixture();
  try {
    store.configure({ org: "acme", url: "https://discord.com/api/webhooks/123456/discussion-secret", label: "forum", audit, context });
    const racing = createDiscussionStore({
      database,
      fetchImpl: async () => {
        database.prepare("DELETE FROM org_discord_discussion_connections WHERE org='acme'").run();
        return new Response(JSON.stringify({ id: "123456789012345678" }), { status: 200 });
      }
    });
    await assert.rejects(() => racing.testConnection({ org: "acme", audit, context }), (error) => error.status === 409);
    assert.deepEqual(events.slice(-2).map((value) => [value.operation, value.targetType, value.result, value.classification]), [
      ["discussion.connection.test.requested", "discussion_connection", "success", "external_delivery_requested"],
      ["discussion.connection.test.completed", "discussion_connection", "failure", "external_delivery_failed"]
    ]);
  } finally { database.close(); }
});

test("a provider failure completes its audit record before returning fixed unavailable", async () => {
  const { database, store, audit, context, events } = fixture();
  try {
    store.configure({ org: "acme", url: "https://discord.com/api/webhooks/123456/discussion-secret", label: "forum", audit, context });
    const failing = createDiscussionStore({
      database,
      fetchImpl: async () => new Response("provider detail must not escape", { status: 500 })
    });
    await assert.rejects(() => failing.testConnection({ org: "acme", audit, context }), (error) => error.status === 503);
    assert.deepEqual(events.slice(-2).map((value) => [value.operation, value.result, value.classification]), [
      ["discussion.connection.test.requested", "success", "external_delivery_requested"],
      ["discussion.connection.test.completed", "failure", "external_delivery_failed"]
    ]);
  } finally { database.close(); }
});

test("discussion retry mutates only failed mirrors and ledger failure rolls back the mutation", () => {
  const { database, store, audit, context, events, artifact } = fixture();
  try {
    store.configure({ org: "acme", url: "https://discord.com/api/webhooks/123456/discussion-secret", label: "forum", audit, context });
    store.setMode({ artifact, mode: "discord_mirror", actor: context.actorId, audit, context });
    assert.deepEqual(store.retry({ artifact, actor: context.actorId, audit, context }), {
      mode: "discord_mirror", state: "pending", enabled: true, connectionConfigured: true, lastError: null
    });
    assert.equal(events.filter((value) => value.operation === "discussion.mode.retry").length, 0);
    database.prepare("UPDATE artifact_discussions SET state='failed', last_error='provider returned https://discord.com/api/webhooks/secret'").run();
    const retried = store.retry({ artifact, actor: context.actorId, audit, context });
    assert.deepEqual(retried, { mode: "discord_mirror", state: "pending", enabled: true, connectionConfigured: true, lastError: null });
    assert.equal(events.filter((value) => value.operation === "discussion.mode.retry").length, 1);
    assert.equal(database.prepare("SELECT generation FROM artifact_discussions WHERE artifact_id='artifact-1'").pluck().get(), 2);
  } finally { database.close(); }

  const failed = fixture({ failAudit: true });
  try {
    assert.throws(() => failed.store.configure({
      org: "acme", url: "https://discord.com/api/webhooks/123456/discussion-secret", label: "forum", audit: failed.audit, context: failed.context
    }), /ledger unavailable/);
    assert.equal(failed.database.prepare("SELECT COUNT(*) FROM org_discord_discussion_connections").pluck().get(), 0);
  } finally { failed.database.close(); }
});
