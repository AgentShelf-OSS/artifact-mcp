import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { migrateDatabase } from "../lib/migrations.js";
import { createArtifactData } from "../lib/artifact-data.js";
import { createDataSourceRegistry } from "../lib/data-source-registry.js";

function fixture() {
  const db = new Database(":memory:");
  migrateDatabase(db);
  const data = createArtifactData({ db, sources: [] });
  const registry = createDataSourceRegistry({
    db, data, operatorSources: [], orgs: { has: (org) => org === "acme" },
    artifacts: { getArtifactMeta: (id) => id === "art-1" ? { id, org: "acme", title: "PR Watch" } : null }
  });
  return { db, registry };
}

const pushDefinition = (id = "pr-watch") => ({
  id, org: "acme", kind: "push", operations: { snapshot: {} }, subscriptions: { updates: { transport: "push" } }
});

test("managed connections expose versions and enforce optimistic updates", () => {
  const { db, registry } = fixture();
  const created = registry.create({ definition: pushDefinition(), enabled: true });
  assert.deepEqual({ id: created.id, origin: created.origin, version: created.version }, { id: "pr-watch", origin: "managed", version: 1 });
  assert.equal(registry.get("pr-watch").definition.kind, "push");
  assert.throws(() => registry.create({ definition: pushDefinition(), enabled: true }), /already exists/);
  assert.throws(() => registry.update("pr-watch", { definition: pushDefinition(), enabled: true, expected_version: 0 }), (error) => error.code === "stale_version");
  assert.equal(registry.update("pr-watch", { definition: pushDefinition(), enabled: false, expected_version: 1 }).health.state, "disabled");
  db.close();
});

test("operator sources are immutable and source ids cannot collide", () => {
  const db = new Database(":memory:"); migrateDatabase(db); const data = createArtifactData({ db, sources: [] });
  const operator = { id: "operator", org: "acme", kind: "push", operations: { state: {} }, subscriptions: { updates: { transport: "push" } } };
  const registry = createDataSourceRegistry({ db, data, operatorSources: [operator], orgs: { has: () => true }, artifacts: {} });
  assert.equal(registry.get("operator").origin, "operator");
  assert.throws(() => registry.update("operator", { definition: operator, enabled: false, expected_version: 1 }), (error) => error.code === "operator_read_only");
  assert.throws(() => registry.create({ definition: operator, enabled: true }), (error) => error.code === "source_conflict");
});

test("impact blocks deletion while a current-org artifact is bound", () => {
  const { db, registry } = fixture(); registry.create({ definition: pushDefinition(), enabled: true });
  db.prepare("INSERT INTO artifacts (id,org,title,client_id,created_at,updated_at) VALUES ('art-1','acme','PR Watch','client',datetime('now'),datetime('now'))").run();
  db.prepare("INSERT INTO artifact_data_bindings (artifact_id,org,bindings,updated_at) VALUES ('art-1','acme',?,datetime('now'))").run(JSON.stringify({ bindings: { card: { source: "pr-watch", operations: ["snapshot"], subscriptions: ["updates"] } } }));
  assert.equal(registry.impact("pr-watch").can_delete, false);
  assert.throws(() => registry.remove("pr-watch", 1), (error) => error.code === "source_in_use");
});

test("managed sources survive a real database reopen and missing refs remain config errors", () => {
  const dir = mkdtempSync(join(tmpdir(), "artifact-connections-"));
  const path = join(dir, "artifact.db");
  try {
    const first = new Database(path); migrateDatabase(first);
    const data = createArtifactData({ db: first, sources: [] });
    const registry = createDataSourceRegistry({ db: first, data, env: {}, orgs: { has: () => true }, artifacts: {} });
    const definition = { id: "credentialed", org: "acme", kind: "http", base_url: "https://example.test", headers_env: { authorization: "MISSING_TOKEN" }, operations: { status: { path: "/status" } }, subscriptions: {} };
    assert.equal(registry.create({ definition, enabled: true }).missing_references[0], "MISSING_TOKEN");
    first.close();
    const second = new Database(path); migrateDatabase(second);
    const reopened = createDataSourceRegistry({ db: second, data: createArtifactData({ db: second, sources: [] }), env: {}, orgs: { has: () => true }, artifacts: {} });
    const view = reopened.get("credentialed");
    assert.deepEqual(view.missing_references, ["MISSING_TOKEN"]);
    assert.equal(view.health.state, "config_error");
    second.close();
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("failed audit rolls back the source mutation and leaves runtime registry unchanged", () => {
  const db = new Database(":memory:"); migrateDatabase(db);
  const data = createArtifactData({ db, sources: [] });
  const registry = createDataSourceRegistry({ db, data, operatorSources: [], orgs: { has: () => true }, artifacts: {}, audit: { appendInTransaction() { throw new Error("audit unavailable"); } } });
  assert.throws(() => registry.create({ definition: pushDefinition("atomic"), enabled: true }), /audit unavailable/);
  assert.throws(() => registry.get("atomic"), (error) => error.code === "not_found");
  assert.equal(db.prepare("SELECT COUNT(*) AS count FROM data_sources").get().count, 0);
});

test("strict nested source validation rejects incompatible transport definitions", () => {
  const { registry } = fixture();
  assert.throws(() => registry.create({ definition: { ...pushDefinition("bad"), extra: true }, enabled: true }), (error) => error.code === "invalid_source");
  assert.throws(() => registry.create({ definition: { id: "bad-http", org: "acme", kind: "http", base_url: "https://example.test", operations: { x: { path: "/" } }, subscriptions: { push: { transport: "push" } } }, enabled: true }), (error) => error.code === "invalid_source");
  assert.throws(() => registry.create({ definition: { id: "bad-push", org: "acme", kind: "push", operations: { x: { path: "/not-allowed" } }, subscriptions: { push: { transport: "push" } } }, enabled: true }), (error) => error.code === "invalid_source");
});
