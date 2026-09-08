import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { migrateDatabase, migrateDatabaseThrough } from "../lib/migrations.js";
import { deriveViewerId, deriveViewerName } from "../lib/viewer-identity.js";
const stateDir = mkdtempSync(join(tmpdir(), "artifact-state-identity-"));
process.env.DATA_DIR = stateDir;
const { createStateStore } = await import("../lib/state.js");
const { default: db } = await import("../lib/db.js");
const orgs = await import("../lib/orgs.js");
const databases = new Set();
test.afterEach(() => { for (const database of databases) database.close(); databases.clear(); });

test("viewer identity nested HMAC is deterministic, case-folded, and key-bound", () => {
  const key = Buffer.alloc(32, 7).toString("base64");
  assert.equal(deriveViewerId("alice@acme.test", key), "d3d6aa9815b72050");
  assert.equal(deriveViewerId("ALICE@ACME.TEST", key), "d3d6aa9815b72050");
  assert.notEqual(deriveViewerId("alice@acme.test", Buffer.alloc(32, 8).toString("base64")), "d3d6aa9815b72050");
});

test("fallback viewer names use separators, Unicode code points, and no controls", () => {
  assert.equal(deriveViewerName("ada.lovelace+test@example.test"), "Ada Lovelace Test");
  assert.equal([...deriveViewerName("😀".repeat(50) + "@example.test")].length, 40);
  assert.doesNotMatch(deriveViewerName("a\u0000b\t\u007fc@example.test"), /[\u0000-\u001f\u007f-\u009f]/u);
});

test("migration 34 carries organization rows and revisions forward", () => {
  const database = new Database(":memory:");
  databases.add(database);
  migrateDatabaseThrough(database, 33);
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('a', 'p', 'acme', 'A')").run();
  database.prepare("INSERT INTO artifact_state (artifact_id,key,value,revision,updated_at,updated_by) VALUES ('a','note','2',7,'now','alice')").run();
  migrateDatabase(database);
  assert.deepEqual(database.prepare("SELECT scope,viewer,key,value,revision FROM artifact_state").get(), { scope: "org", viewer: "", key: "note", value: "2", revision: 7 });
});

test("viewer bucket cap is independent from other viewers and org state", () => {
  const database = new Database(":memory:");
  databases.add(database);
  migrateDatabase(database);
  database.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('a', 'p', 'acme', 'A')").run();
  const state = createStateStore({ db: database, now: () => "now" });
  for (let i = 0; i < 64; i += 1) assert.equal(state.put("a", `k${i}`, i, "alice@example.test", null, "viewer", "alice@example.test").ok, true);
  assert.equal(state.put("a", "overflow", 1, "alice@example.test", null, "viewer", "alice@example.test").reason, "too_many_keys");
  assert.equal(state.put("a", "note", 1, "bob@example.test", null, "viewer", "bob@example.test").ok, true);
  assert.equal(state.put("a", "k0", 1, "alice@example.test", 1, "viewer", "alice@example.test").revision, 2);
  assert.equal(state.put("a", "org", 1, "alice@example.test").ok, true);
});

test("email display names validate and upsert", () => {
  orgs.createOrg({ name: "names" });
  assert.deepEqual(orgs.addEmailMember("names", "ALICE@example.test", "  Élodie 😀  "), { org: "names", email: "alice@example.test", display_name: "Élodie 😀" });
  assert.equal(orgs.viewerDisplayName("ALICE@example.test"), "Élodie 😀");
  assert.deepEqual(orgs.addEmailMember("names", "alice@example.test", ""), { org: "names", email: "alice@example.test", display_name: "" });
  for (const value of [1, "a\tb", "a\u0000b", "a\u007fb", "a\u0085b", "x".repeat(41)]) assert.throws(() => orgs.addEmailMember("names", "alice@example.test", value));
  orgs.createOrg({ name: "other" });
  assert.throws(() => orgs.addEmailMember("other", "alice@example.test", "Moved"), /already mapped/);
  assert.equal(orgs.viewerDisplayName("alice@example.test"), "");
});

test.after(() => { db.close(); rmSync(stateDir, { recursive: true, force: true }); });
