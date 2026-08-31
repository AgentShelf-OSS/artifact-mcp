import test, { after } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import Database from "better-sqlite3";
import { LATEST_SCHEMA_VERSION, migrateDatabaseThrough } from "../lib/migrations.js";
import { decrypt } from "../lib/crypto.js";

const ALL_VERSIONS = Array.from({ length: LATEST_SCHEMA_VERSION }, (_, i) => i + 1);

const importDataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-import-"));
process.env.DATA_DIR = importDataDir;
const { default: defaultDb, openDatabase } = await import("../lib/db.js");
const keys = await import("../lib/keys.js");

after(() => {
  defaultDb.close();
  rmSync(importDataDir, { recursive: true, force: true });
});

test("fresh databases apply ordered migrations with foreign keys enabled", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-fresh-"));
  const runtime = openDatabase({ dataDir });

  try {
    const versions = runtime.db.prepare("SELECT version FROM schema_migrations ORDER BY version").pluck().all();
    assert.deepEqual(versions, ALL_VERSIONS);
    assert.equal(runtime.db.pragma("foreign_keys", { simple: true }), 1);
    const foreignKeys = runtime.db.prepare("PRAGMA foreign_key_list(reactions)").all();
    assert.ok(foreignKeys.some((fk) => fk.table === "artifacts" && fk.on_delete === "CASCADE"));
    assert.deepEqual(
      runtime.db.prepare("PRAGMA table_info(notification_reads)").all().map((column) => column.name),
      ["viewer_email", "seen_at"]
    );
    assert.deepEqual(
      runtime.db.prepare("PRAGMA table_info(org_email_members)").all().map((column) => column.name),
      ["email", "org", "created_at"]
    );
    assert.deepEqual(
      runtime.db.prepare("PRAGMA table_info(api_keys)").all().map((column) => column.name),
      ["client_id", "key_hash", "org", "created_at", "revoked_at", "label", "role", "owner_email"]
    );
    assert.deepEqual(
      runtime.db.prepare("PRAGMA table_info(artifact_revisions)").all().map((column) => column.name),
      [
        "artifact_id", "org", "revision", "title", "description", "category", "bytes",
        "is_bundle", "entry", "created_at", "body_sha256", "client_id"
      ]
    );
    assert.deepEqual(
      runtime.db.prepare("PRAGMA table_info(feedback)").all().map((column) => column.name),
      [
        "id", "artifact_id", "org", "viewer_email", "body", "artifact_revision", "created_at",
        "resolved_at", "resolved_by", "anchor_path", "anchor_x", "anchor_y", "anchor_approx",
        "parent_id", "anchor_w", "anchor_h", "anchor_page", "author_source",
        "external_author_id", "external_author_display", "external_created_at",
        "external_edited_at", "external_deleted_at", "anchor_kind", "anchor_node_id", "anchor_quote"
      ]
    );
    runtime.db.prepare("INSERT INTO api_keys (client_id, key_hash) VALUES (?, ?)")
      .run("pre-v22-author", "pre-v22-hash");
    runtime.db.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES (?, ?, ?, ?)")
      .run("pre-v22-artifact", "pre-v22-author", "default", "Existing revision");
    runtime.db.prepare(
      "INSERT INTO artifact_revisions (artifact_id, org, revision, title) VALUES (?, ?, ?, ?)"
    ).run("pre-v22-artifact", "default", 1, "Existing revision");
    assert.equal(
      runtime.db.prepare(
        "SELECT client_id FROM artifact_revisions WHERE artifact_id = 'pre-v22-artifact'"
      ).pluck().get(),
      null,
      "an unattributed pre-v22 revision remains readable as null"
    );
    assert.ok(
      runtime.db.prepare("PRAGMA index_list(org_email_members)").all()
        .some((index) => index.name === "org_email_members_org_idx")
    );
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("historical migration boundary seam executes real ledger steps without manufacturing rows", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-boundary-"));
  const boundary = new Database(path.join(dataDir, "artifacts.db"));
  try {
    migrateDatabaseThrough(boundary, 8);
    assert.deepEqual(boundary.prepare("SELECT version FROM schema_migrations ORDER BY version").pluck().all(),
      [1, 2, 3, 4, 5, 6, 7, 8]);
    assert.equal(boundary.prepare("SELECT COUNT(*) FROM sqlite_master WHERE name = 'artifact_revisions'").pluck().get(), 1);
    assert.equal(boundary.prepare("SELECT COUNT(*) FROM sqlite_master WHERE name = 'artifact_shares'").pluck().get(), 0);
    assert.throws(() => migrateDatabaseThrough(boundary, LATEST_SCHEMA_VERSION + 1), /Unsupported schema version/);
  } finally {
    boundary.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("a frozen v25 audit database upgrades through v26 without rewriting its history", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-v25-audit-"));
  const db = new Database(path.join(dataDir, "artifacts.db"));
  try {
    migrateDatabaseThrough(db, 25);
    assert.equal(db.prepare("PRAGMA table_info(security_audit_chain_head)").all().some((column) => column.name === "head_mac"), false);
    migrateDatabaseThrough(db);
    assert.equal(db.prepare("PRAGMA table_info(security_audit_chain_head)").all().some((column) => column.name === "head_mac"), true);
    assert.equal(db.prepare("PRAGMA table_info(security_audit_chain_head)").all().some((column) => column.name === "pending_receipts_root"), true);
    assert.deepEqual(db.prepare("PRAGMA table_info(security_audit_receipts)").all().filter((column) => ["actor_id", "result", "canonical_version", "receipt_mac"].includes(column.name)).map((column) => column.name), ["result", "actor_id", "canonical_version", "receipt_mac"]);
  } finally {
    db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("a populated schema-31 database upgrades to the latest version and reopens without further migrations", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-v31-anchor-"));
  const boundary = new Database(path.join(dataDir, "artifacts.db"));
  boundary.pragma("foreign_keys = ON");
  try {
    migrateDatabaseThrough(boundary, 31);
    assert.deepEqual(
      boundary.prepare("SELECT version FROM schema_migrations ORDER BY version").pluck().all(),
      Array.from({ length: 31 }, (_, i) => i + 1),
      "the boundary database starts exactly at schema 31"
    );
    boundary.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES (?, ?, ?, ?)")
      .run("v31-artifact", "v31-key", "default", "Schema 31 artifact");
    boundary.prepare(
      "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)"
    ).run(
      "v31-feedback", "v31-artifact", "default", "v31-viewer@example.test",
      "Feedback written before anchor v2", 2, "2026-07-30 19:32:23"
    );
    boundary.prepare(
      "UPDATE feedback SET anchor_path = 'index.html', anchor_x = 12.5, anchor_y = 30, anchor_approx = 1, anchor_page = '2' WHERE id = 'v31-feedback'"
    ).run();
  } finally {
    boundary.close();
  }

  const upgraded = openDatabase({ dataDir });
  try {
    assert.deepEqual(
      upgraded.db.prepare("SELECT version FROM schema_migrations ORDER BY version").pluck().all(),
      ALL_VERSIONS,
      "the upgrade applies only the unapplied migration 32"
    );
    const row = upgraded.db.prepare("SELECT * FROM feedback WHERE id = 'v31-feedback'").get();
    assert.equal(row.artifact_id, "v31-artifact");
    assert.equal(row.org, "default");
    assert.equal(row.viewer_email, "v31-viewer@example.test");
    assert.equal(row.body, "Feedback written before anchor v2");
    assert.equal(row.artifact_revision, 2);
    assert.equal(row.created_at, "2026-07-30 19:32:23");
    assert.equal(row.anchor_path, "index.html");
    assert.equal(row.anchor_x, 12.5);
    assert.equal(row.anchor_y, 30);
    assert.equal(row.anchor_approx, 1);
    assert.equal(row.anchor_page, "2");
    assert.equal(row.anchor_kind, null);
    assert.equal(row.anchor_node_id, null);
    assert.equal(row.anchor_quote, null);
  } finally {
    upgraded.db.close();
  }

  // A second open must be a no-op for the ledger and keep the new columns null.
  const reopened = openDatabase({ dataDir });
  try {
    assert.deepEqual(
      reopened.db.prepare("SELECT version FROM schema_migrations ORDER BY version").pluck().all(),
      ALL_VERSIONS,
      "reopening applies no further migration"
    );
    const row = reopened.db.prepare("SELECT * FROM feedback WHERE id = 'v31-feedback'").get();
    assert.equal(row.body, "Feedback written before anchor v2");
    assert.equal(row.created_at, "2026-07-30 19:32:23");
    assert.equal(row.anchor_kind, null);
    assert.equal(row.anchor_node_id, null);
    assert.equal(row.anchor_quote, null);
  } finally {
    reopened.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("reopening a migrated database is idempotent", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-reopen-"));
  const first = openDatabase({ dataDir });
  first.db.prepare("INSERT INTO orgs (name) VALUES (?)").run("reopen-org");
  first.db.prepare("INSERT INTO org_email_members (email, org) VALUES (?, ?)")
    .run("person@example.com", "reopen-org");
  first.db.close();
  const second = openDatabase({ dataDir });

  try {
    const versions = second.db.prepare("SELECT version FROM schema_migrations ORDER BY version").pluck().all();
    assert.deepEqual(versions, ALL_VERSIONS);
    assert.equal(
      second.db.prepare("SELECT org FROM org_email_members WHERE email = ?").pluck().get("person@example.com"),
      "reopen-org"
    );
  } finally {
    second.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("legacy databases upgrade without losing valid keys, artifacts, or reactions", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-legacy-"));
  const legacy = new Database(path.join(dataDir, "artifacts.db"));
  legacy.exec(`
    CREATE TABLE api_keys (
      client_id TEXT PRIMARY KEY,
      key_hash TEXT NOT NULL UNIQUE,
      created_at TEXT NOT NULL DEFAULT (datetime('now')),
      revoked_at TEXT
    );
    CREATE TABLE artifacts (
      id TEXT PRIMARY KEY,
      client_id TEXT NOT NULL,
      title TEXT NOT NULL,
      description TEXT NOT NULL DEFAULT '',
      bytes INTEGER NOT NULL DEFAULT 0,
      created_at TEXT NOT NULL DEFAULT (datetime('now')),
      updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE TABLE reactions (
      email TEXT NOT NULL,
      artifact_id TEXT NOT NULL,
      favorite INTEGER NOT NULL DEFAULT 0,
      vote INTEGER NOT NULL DEFAULT 0,
      updated_at TEXT NOT NULL DEFAULT (datetime('now')),
      PRIMARY KEY (email, artifact_id)
    );
    INSERT INTO api_keys (client_id, key_hash) VALUES ('legacy-key', 'hash');
    INSERT INTO artifacts (id, client_id, title) VALUES ('abc123', 'legacy-key', 'Legacy artifact');
    INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('viewer@example.com', 'abc123', 1, 1);
    INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('orphan@example.com', 'missing1', 1, -1);
  `);
  legacy.close();

  const runtime = openDatabase({ dataDir });
  try {
    assert.equal(runtime.db.prepare("SELECT org FROM api_keys WHERE client_id = 'legacy-key'").pluck().get(), "default");
    assert.equal(runtime.db.prepare("SELECT role FROM api_keys WHERE client_id = 'legacy-key'").pluck().get(), "author");
    assert.equal(runtime.db.prepare("SELECT title FROM artifacts WHERE id = 'abc123'").pluck().get(), "Legacy artifact");
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM reactions").pluck().get(), 1);
    runtime.db.prepare("DELETE FROM artifacts WHERE id = 'abc123'").run();
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM reactions").pluck().get(), 0);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("key creation validates, persists, lists, and defaults capability roles", () => {
  const reader = keys.createKey({ clientId: "db-role-reader", org: "acme", label: "Reader", role: "reader" });
  const collaborator = keys.createKey({
    clientId: "db-role-collaborator",
    org: "acme",
    label: "Collaborator",
    role: "collaborator"
  });
  const defaulted = keys.createKey({ clientId: "db-role-default", org: "acme", label: "Default" });

  assert.equal(reader.role, "reader");
  assert.equal(collaborator.role, "collaborator");
  assert.equal(defaulted.role, "author");
  assert.throws(
    () => keys.createKey({ clientId: "db-role-invalid", org: "acme", role: "owner" }),
    { message: keys.INVALID_KEY_ROLE_MESSAGE }
  );
  const listed = keys.listKeys();
  assert.equal(listed.find((key) => key.client_id === reader.clientId).role, "reader");
  assert.equal(
    listed.find((key) => key.client_id === collaborator.clientId).role,
    "collaborator"
  );
});

test("existing plaintext webhook rows are encrypted in place when a key is configured", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-db-webhook-encryption-"));
  const previousKey = process.env.WEBHOOK_ENC_KEY;
  delete process.env.WEBHOOK_ENC_KEY;
  const first = openDatabase({ dataDir });
  const secretUrl = "https://discord.com/api/webhooks/123/existing-plaintext-token";
  first.db.prepare("INSERT INTO orgs (name) VALUES (?)").run("migration-test");
  first.db.prepare("INSERT INTO org_webhooks (id, org, url) VALUES (?, ?, ?)")
    .run("legacy-webhook", "migration-test", secretUrl);
  first.db.close();

  const key = Buffer.alloc(32, 7).toString("base64");
  process.env.WEBHOOK_ENC_KEY = key;
  const second = openDatabase({ dataDir });
  try {
    const stored = second.db.prepare("SELECT * FROM org_webhooks WHERE id = ?").get("legacy-webhook");
    assert.doesNotMatch(JSON.stringify(stored), /existing-plaintext-token/);
    assert.match(stored.url, /^https:\/\/discord\.com\/…oken$/);
    assert.equal(decrypt(stored, key), secretUrl);
  } finally {
    second.db.close();
    if (previousKey === undefined) delete process.env.WEBHOOK_ENC_KEY;
    else process.env.WEBHOOK_ENC_KEY = previousKey;
    rmSync(dataDir, { recursive: true, force: true });
  }
});
