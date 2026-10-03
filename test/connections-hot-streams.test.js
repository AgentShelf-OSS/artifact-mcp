import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";
import { createArtifactData } from "../lib/artifact-data.js";
import { createDataSourceRegistry } from "../lib/data-source-registry.js";

function source(id) {
  return { id, org: "org", kind: "push", operations: { state: { key: "state" } }, subscriptions: { updates: { transport: "push" } } };
}

test("managed disable/re-enable resyncs only the affected live topic", () => {
  const db = new Database(":memory:"); db.pragma("foreign_keys=ON"); migrateDatabase(db);
  db.prepare("INSERT INTO artifacts (id,client_id,org,title) VALUES ('a','client','org','A')").run();
  const data = createArtifactData({ db, sources: [] });
  const registry = createDataSourceRegistry({ db, data, operatorSources: [], orgs: { has: () => true }, artifacts: { getArtifactMeta: () => ({ id: "a", org: "org", title: "A" }) } });
  registry.create({ definition: source("alpha"), enabled: true }); registry.create({ definition: source("beta"), enabled: true });
  data.setBindings("a", "org", { bindings: {
    alpha: { source: "alpha", operations: ["state"], subscriptions: ["updates"] },
    beta: { source: "beta", operations: ["state"], subscriptions: ["updates"] }
  } });
  const alpha = [], beta = [];
  const left = data.subscribe("a", "org", [{ binding: "alpha", subscription: "updates" }], (event) => alpha.push(event));
  const right = data.subscribe("a", "org", [{ binding: "beta", subscription: "updates" }], (event) => beta.push(event));
  const alphaVersion = registry.get("alpha").version;
  registry.disable("alpha", alphaVersion);
  assert.ok(alpha.some((event) => event.event === "data:resync"));
  assert.equal(beta.filter((event) => event.event === "data:resync").length, 0);
  const betaBefore = beta.length;
  registry.enable("alpha", registry.get("alpha").version);
  assert.ok(alpha.length > 1, "alpha listener should remain attached across disable/re-enable");
  assert.equal(beta.length, betaBefore, "unaffected beta topic must not reconnect or resync");
  left.close(); right.close(); db.close();
});

test("source test records bounded health without returning upstream headers or bodies", async () => {
  const db = new Database(":memory:"); migrateDatabase(db); const data = createArtifactData({ db, sources: [], fetchImpl: async () => new Response('{"privateBody":"never-in-summary"}', { headers: { "content-type": "application/json", "x-private-header": "never-in-summary" } }) });
  const registry = createDataSourceRegistry({ db, data, env: {}, orgs: { has: () => true }, artifacts: {} });
  registry.create({ definition: { id: "http", org: "org", kind: "http", base_url: "https://example.test", operations: { status: { path: "/status" } }, subscriptions: {} }, enabled: true });
  const result = await registry.test("http", "status", {});
  await assert.rejects(registry.test("http", "unknown", {}), error => error.code === "bad_params");
  await assert.rejects(registry.test("http", "status", { unknown: "x" }), error => error.code === "bad_params");
  assert.equal(result.source_id, "http");
  assert.equal(result.ok, true);
  assert.equal(result.summary.response_type, "object");
  assert.equal(JSON.stringify(result).includes("never-in-summary"), false);
  assert.equal(typeof result.elapsed_ms, "number");
  assert.equal(Object.hasOwn(result, "body"), false);
  assert.equal(Object.hasOwn(result, "headers"), false);
  assert.match(registry.get("http").health.last_query_at, /^\d{4}-/);
  db.close();
});


test("failed binding audit leaves producer data and live listeners unchanged", () => {
  const db = new Database(":memory:"); migrateDatabase(db);
  db.prepare("INSERT INTO artifacts (id,client_id,org,title) VALUES ('a','client','org','A')").run();
  const data = createArtifactData({ db, sources: [source("alpha")] });
  const grants = { live: { source: "alpha", operations: ["state"], subscriptions: ["updates"] } };
  data.setBindings("a", "org", grants); data.setData("a", "org", "live", "state", { preserved: true });
  const observed = [], connection = data.subscribe("a", "org", [{ binding: "live", subscription: "updates" }], event => observed.push(event));
  observed.length = 0;
  assert.throws(() => data.setBindings("a", "org", {}, () => { throw new Error("audit unavailable"); }), /audit unavailable/);
  assert.deepEqual(data.getBindings("a").bindings.live, grants.live);
  assert.equal(db.prepare("SELECT COUNT(*) AS n FROM artifact_data_snapshots").get().n, 1);
  assert.equal(observed.length, 0, "failed writes must emit no resync or status");
  data.appendEvents("a", "org", "live", "updates", [{ id: "still-live", event: "message", data: true }]);
  assert.equal(observed.at(-1).id, "still-live");
  connection.close(); db.close();
});


test("new subscriptions to disabled sources report disabled and do not replay data", () => {
  const db = new Database(":memory:"); migrateDatabase(db);
  db.prepare("INSERT INTO artifacts (id,client_id,org,title) VALUES ('a','client','org','A')").run();
  const data = createArtifactData({ db, sources: [] });
  const registry = createDataSourceRegistry({ db, data, orgs: { has: () => true }, artifacts: {} });
  registry.create({ definition: source("alpha"), enabled: true });
  data.setBindings("a", "org", { live: { source: "alpha", operations: ["state"], subscriptions: ["updates"] } });
  data.appendEvents("a", "org", "live", "updates", [{ id: "past", event: "message", data: true }]);
  registry.disable("alpha", 1);
  const received = [], connection = data.subscribe("a", "org", [{ binding: "live", subscription: "updates" }], event => received.push(event));
  assert.equal(received[0].data.state, "disabled");
  assert.deepEqual(data.replay("a", "org", connection.subscriptions).events, []);
  assert.equal(db.prepare("SELECT COUNT(*) AS n FROM artifact_data_events").get().n, 1, "disable retains producer data");
  connection.close(); db.close();
});

test("query health counts consecutive failures and resets after recovery", async () => {
  const db = new Database(":memory:"); migrateDatabase(db);
  db.prepare("INSERT INTO artifacts (id,client_id,org,title) VALUES ('a','client','org','A')").run();
  let failing = true;
  const data = createArtifactData({ db, sources: [], fetchImpl: async () => failing ? new Response("upstream-private-error", { status: 503 }) : new Response('{"ok":true}') });
  const registry = createDataSourceRegistry({ db, data, orgs: { has: () => true }, artifacts: {} });
  registry.create({ definition: { id: "http", org: "org", kind: "http", base_url: "https://example.test", operations: { status: { path: "/status" } }, subscriptions: {} } });
  data.setBindings("a", "org", { live: { source: "http", operations: ["status"], subscriptions: [] } });
  assert.equal((await data.query("a", "org", "live", "status")).error, "data_unavailable");
  assert.equal((await data.query("a", "org", "live", "status")).error, "data_unavailable");
  const failed = registry.get("http").health;
  assert.equal(failed.state, "unavailable"); assert.equal(failed.retry_count, 2);
  assert.equal(failed.error, "data_unavailable"); assert.equal(failed.last_success_at, null);
  assert.equal(failed.last_event_at, null);
  failing = false;
  assert.deepEqual((await data.query("a", "org", "live", "status")).data, { ok: true });
  const recovered = registry.get("http").health;
  assert.equal(recovered.state, "connected"); assert.equal(recovered.retry_count, 0);
  assert.equal(recovered.error, null); assert.ok(recovered.last_success_at);
  assert.equal(recovered.last_event_at, null);
  db.close();
});
