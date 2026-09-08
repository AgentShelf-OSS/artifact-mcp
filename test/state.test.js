import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { mkdtempSync, renameSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { migrateDatabase } from "../lib/migrations.js";
const importDataDir = mkdtempSync(join(tmpdir(), "artifact-state-import-"));
process.env.DATA_DIR = importDataDir;
const { createStateStore } = await import("../lib/state.js");
const { default: defaultDb, openDatabase } = await import("../lib/db.js");
const { createArtifactStore } = await import("../lib/store.js");
test.after(() => {
  defaultDb.close();
  rmSync(importDataDir, { recursive: true, force: true });
});

function fixture() {
  const database = new Database(":memory:");
  database.pragma("foreign_keys = ON");
  migrateDatabase(database);
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('a', 'c', 'acme', 'A')").run();
  return database;
}

test("state store round trips values, revisions, conflicts, delete, and cascade", () => {
  const database = fixture();
  try {
    const state = createStateStore({ db: database, now: () => "2026-09-08T00:00:00.000Z" });
    assert.deepEqual(state.put("a", "theme.mode", { dark: true }, "v@acme.test"), { ok: true, key: "theme.mode", revision: 1, updated_at: "2026-09-08T00:00:00.000Z" });
    assert.deepEqual(state.get("a", "theme.mode").value, { dark: true });
    assert.equal(state.put("a", "theme.mode", "light", "v@acme.test", 1).revision, 2);
    assert.deepEqual(state.put("a", "theme.mode", "stale", "v@acme.test", 1), { ok: false, reason: "conflict", value: "light", revision: 2 });
    state.remove("a", "theme.mode");
    assert.equal(state.get("a", "theme.mode"), null);
    state.put("a", "x", 1, "v@acme.test");
    database.prepare("DELETE FROM artifacts WHERE id = 'a'").run();
    assert.equal(database.prepare("SELECT COUNT(*) AS n FROM artifact_state").get().n, 0);
  } finally { database.close(); }
});

test("state store keeps org and viewer namespaces independent", () => {
  const database = new Database(":memory:");
  database.pragma("foreign_keys = ON");
  migrateDatabase(database);
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('scoped', 'publisher', 'acme', 'Scoped')").run();
  const state = createStateStore({ db: database, now: () => "now" });
  assert.equal(state.put("scoped", "note", "org", "alice@example.test").ok, true);
  assert.equal(state.put("scoped", "note", "alice", "alice@example.test", null, "viewer", "alice@example.test").ok, true);
  assert.equal(state.get("scoped", "note").value, "org");
  assert.equal(state.get("scoped", "note", "viewer", "alice@example.test").value, "alice");
  assert.equal(state.get("scoped", "note", "viewer", "bob@example.test"), null);
  assert.equal(state.get("scoped", "note", "viewer", "alice@example.test").updated_by, "alice@example.test");
  database.close();
});

test("state store enforces key, value, and key-count limits", () => {
  const database = fixture();
  try {
    const state = createStateStore({ db: database });
    assert.equal(state.put("a", "bad key", 1, "v@acme.test").reason, "bad_key");
    for (const key of ["ends\n", "ends\r", "ends\u2028", "é", "", null, 123]) {
      assert.equal(state.put("a", key, 1, "v@acme.test").reason, "bad_key");
    }
    assert.equal(state.put("a", "huge", "x".repeat(256 * 1024), "v@acme.test").reason, "too_large");
    for (let i = 0; i < 64; i += 1) assert.equal(state.put("a", `k${i}`, i, "v@acme.test").ok, true);
    assert.equal(state.put("a", "overflow", 1, "v@acme.test").reason, "too_many_keys");
  } finally { database.close(); }
});

test("state supports every JSON type and revision zero means an absent key", () => {
  const database = fixture();
  try {
    const state = createStateStore({ db: database });
    assert.deepEqual(state.list("a"), []);
    assert.deepEqual(state.put("a", "value", false, "first@acme.test", 1),
      { ok: false, reason: "conflict", value: null, revision: 0 });
    for (const [index, value] of [null, false, 0, "", [1, true], { nested: { x: "é" } }].entries()) {
      const result = state.put("a", "value", value, "writer@acme.test", index);
      assert.equal(result.revision, index + 1);
      assert.deepEqual(state.get("a", "value").value, value);
      assert.equal(state.get("a", "value").updated_by, "writer@acme.test");
    }
    state.remove("a", "value");
    state.remove("a", "value");
    assert.equal(state.put("a", "value", "new", "writer@acme.test", 0).revision, 1);
  } finally { database.close(); }
});

test("serialized UTF-8 byte limit is exact and updates are allowed at the key cap", () => {
  const database = fixture();
  try {
    const state = createStateStore({ db: database });
    assert.equal(state.put("a", "limit", "x".repeat(262142), "v@acme.test").ok, true);
    assert.equal(state.put("a", "large", "x".repeat(262143), "v@acme.test").reason, "too_large");
    assert.equal(state.put("a", "utf8", "é".repeat(131071), "v@acme.test").ok, true);
    assert.equal(state.put("a", "utf8", "é".repeat(131072), "v@acme.test").reason, "too_large");
    for (let i = 0; i < 62; i++) state.put("a", `k${i}`, i, "v@acme.test");
    assert.equal(state.list("a").length, 64);
    assert.equal(state.put("a", "limit", "small now", "v@acme.test", 1).revision, 2);
    assert.equal(state.put("a", "new", true, "v@acme.test").reason, "too_many_keys");
    state.remove("a", "k0");
    assert.equal(state.put("a", "new", true, "v@acme.test").revision, 1);
  } finally { database.close(); }
});

test("state survives content updates, history restore, and a server restart", () => {
  const dataDir = mkdtempSync(join(tmpdir(), "artifact-state-lifecycle-"));
  let runtime = openDatabase({ dataDir });
  try {
    const artifacts = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });
    const artifact = artifacts.publish({ clientId: "publisher", org: "acme", html: "<h1>One</h1>" });
    const state = createStateStore({ db: runtime.db });
    state.put(artifact.id, "notes", ["keep"], "v@acme.test");
    const original = state.get(artifact.id, "notes");
    assert.equal(artifacts.update({ id: artifact.id, clientId: "publisher", org: "acme", html: "<h1>Two</h1>" }).ok, true);
    assert.deepEqual(state.get(artifact.id, "notes"), original);
    assert.equal(artifacts.restore({ id: artifact.id, clientId: "publisher", revision: 1 }).ok, true);
    assert.deepEqual(state.get(artifact.id, "notes"), original);
    runtime.db.close();
    runtime = openDatabase({ dataDir });
    assert.deepEqual(createStateStore({ db: runtime.db }).get(artifact.id, "notes"), original);
    const reopened = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });
    assert.equal(reopened.deleteArtifactById(artifact.id), true);
    assert.equal(createStateStore({ db: runtime.db }).get(artifact.id, "notes"), null);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("failed deletion and interrupted trash recovery keep state for the live artifact row", () => {
  const dataDir = mkdtempSync(join(tmpdir(), "artifact-state-trash-"));
  const runtime = openDatabase({ dataDir });
  try {
    const artifacts = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });
    const artifact = artifacts.publish({ clientId: "publisher", org: "acme", html: "<h1>Keep</h1>" });
    const state = createStateStore({ db: runtime.db });
    state.put(artifact.id, "notes", "keep", "v@acme.test");
    const original = state.get(artifact.id, "notes");
    runtime.db.exec("CREATE TRIGGER block_state_artifact_delete BEFORE DELETE ON artifacts BEGIN SELECT RAISE(ABORT, 'blocked'); END");
    assert.throws(() => artifacts.deleteArtifactById(artifact.id), /blocked/);
    assert.deepEqual(state.get(artifact.id, "notes"), original);
    assert.equal(artifacts.readArtifact(artifact.id).html, "<h1>Keep</h1>");
    runtime.db.exec("DROP TRIGGER block_state_artifact_delete");
    const trashName = `.${artifact.id}.trash-interrupted`;
    renameSync(join(runtime.artifactDir, `${artifact.id}.html`), join(runtime.artifactDir, trashName));
    const report = artifacts.auditStorage({ cleanTransient: true });
    assert.ok(report.recoveredPaths.includes(trashName));
    assert.deepEqual(state.get(artifact.id, "notes"), original);
    assert.equal(artifacts.readArtifact(artifact.id).html, "<h1>Keep</h1>");
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});
