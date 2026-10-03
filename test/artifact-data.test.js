import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";
import { createArtifactData, parseDataSources } from "../lib/artifact-data.js";

function fixture(sources) {
  const db = new Database(":memory:"); db.pragma("foreign_keys=ON"); migrateDatabase(db);
  db.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('a','client','org','A')").run();
  return { db, data: createArtifactData({ db, sources }) };
}

test("data source configuration rejects missing secrets, duplicate ids, and invalid poll operations", () => {
  assert.throws(() => parseDataSources(JSON.stringify({ sources: [{ id: "a", org: "o", headers_env: { Authorization: "MISSING" } }] }), {}));
  assert.throws(() => parseDataSources(JSON.stringify({ sources: [{ id: "a", org: "o" }, { id: "a", org: "o" }] })));
  assert.throws(() => parseDataSources(JSON.stringify({ sources: [{ id: "a", org: "o", subscriptions: { events: { transport: "poll", operation: "missing", interval_ms: 1000 } } }] })));
  assert.equal(parseDataSources("ops/data-sources.pr-watch.example.json").length, 1);
});

test("source validation rejects unusable defaults and ambiguous transports before startup", () => {
  const source = { id: "api", org: "org", base_url: "https://example.test" };
  for (const change of [
    { operations: { list: { path: "/list", params: { limit: { type: "integer", minimum: 1, default: 0 } } } } },
    { operations: { list: { path: "/list", params: { kind: { type: "string", max_length: 3, enum: ["long"] } } } } },
    { operations: { item: { path: "/item/{missing}" } } },
    { operations: { item: { path: "/item/{" } } },
    { subscriptions: { events: { transport: "push" } } },
    { operations: { item: { path: "/item", params: { key: { type: "string", required: true } } } }, subscriptions: { events: { transport: "poll", operation: "item" } } },
    { headers_env: { Cookie: "QA_HEADER" } },
    { operations: [] }
  ]) assert.throws(() => parseDataSources(JSON.stringify({ sources: [{ ...source, ...change }] }), { QA_HEADER: "synthetic" }));
  const poll = parseDataSources(JSON.stringify({ sources: [{ ...source, operations: { list: { path: "/list" } }, subscriptions: { events: { transport: "poll", operation: "list" } } }] }));
  assert.equal(poll[0].subscriptions.events.interval_ms, 2000);
});

test("invalid event batches remain atomic and moved artifacts cannot expose prior bindings", async () => {
  const sources = parseDataSources(JSON.stringify({ sources: [{ id: "push", org: "org", kind: "push", operations: { latest: { key: "summary" } }, subscriptions: { events: { transport: "push" } } }] }));
  const { data, db } = fixture(sources);
  data.setBindings("a", "org", { reviews: { source: "push", operations: ["latest"], subscriptions: ["events"] } });
  assert.throws(() => data.appendEvents("a", "org", "reviews", "events", [{ id: "valid", event: "update", data: {} }, { id: "é".repeat(65), event: "update", data: {} }]));
  assert.equal(db.prepare("SELECT count(*) AS n FROM artifact_data_events").get().n, 0);
  assert.deepEqual(data.getBindings("a", "other-org"), { bindings: {} });
  assert.deepEqual(await data.query("a", "other-org", "reviews", "latest"), { error: "not_found" });
});

test("push snapshots and event batches are isolated and idempotent", async () => {
  const sources = parseDataSources(JSON.stringify({ sources: [{ id: "push", org: "org", kind: "push", operations: { status: { key: "status" } }, subscriptions: { events: { transport: "push" } } }] }));
  const { data } = fixture(sources);
  data.setBindings("a", "org", { bindings: { reviews: { source: "push", operations: ["status"], subscriptions: ["events"] } } });
  assert.deepEqual(data.setData("a", "org", "reviews", "status", { ok: true }), { revision: 1 });
  assert.deepEqual(await data.query("a", "org", "reviews", "status"), { data: { ok: true } });
  assert.deepEqual(data.appendEvents("a", "org", "reviews", "events", [{ id: "one", event: "dashboard-event", data: { n: 1 } }, { id: "one", event: "dashboard-event", data: { n: 1 } }]), { accepted: 1, duplicates: 1 });
});

test("HTTP query rejects redirects and bounds response bodies", async () => {
  const sources = parseDataSources(JSON.stringify({ sources: [{ id: "http", org: "org", base_url: "https://example.test", operations: { status: { path: "/status", max_bytes: 4 } } }] }));
  const { db } = fixture(sources);
  const data = createArtifactData({ db, sources, fetchImpl: async (url, options) => { calls.push({ url, options }); return new Response("12345", { status: 200 }); } });
  data.setBindings("a", "org", { bindings: { reviews: { source: "http", operations: ["status"], subscriptions: [] } } });
  const calls = [];
  const result = await data.query("a", "org", "reviews", "status");
  assert.deepEqual(result, { error: "too_large" });
  assert.equal(calls[0].options.redirect, "error");
});

test("SSE multiplexer isolates source failure, preserves events, resumes, and aborts on disconnect", async () => {
  const sources = parseDataSources(JSON.stringify({ sources: [
    { id: "down", org: "org", base_url: "https://down.test", operations: { status: { path: "/status" } }, subscriptions: { events: { transport: "sse", path: "/events", events: ["dashboard-event", "dashboard-resync"] } } },
    { id: "up", org: "org", base_url: "https://up.test", operations: { status: { path: "/status" } }, subscriptions: { events: { transport: "sse", path: "/events", events: ["dashboard-event", "dashboard-resync"] } } }
  ] }));
  const { db } = fixture(sources); const frames = []; let aborted = false; let attempt = 0;
  const fetchImpl = async (url, options) => {
    if (url.startsWith("https://down")) throw new Error("offline");
    attempt += 1;
    const stream = new ReadableStream({ start(controller) { controller.enqueue(new TextEncoder().encode(`id: evt-${attempt}\nevent: dashboard-event\ndata: {"attempt":${attempt}}\n\n`)); }, cancel() { aborted = true; } });
    options.signal.addEventListener("abort", () => { aborted = true; });
    return new Response(stream, { status: 200, headers: { "content-type": "text/event-stream" } });
  };
  const data = createArtifactData({ db, sources, fetchImpl });
  data.setBindings("a", "org", { bindings: { first: { source: "down", operations: ["status"], subscriptions: ["events"] }, second: { source: "up", operations: ["status"], subscriptions: ["events"] } } });
  const connection = data.subscribe("a", "org", [{ binding: "first", subscription: "events" }, { binding: "second", subscription: "events" }], (event) => frames.push(event));
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.ok(frames.some((event) => event.binding === "first" && event.event === "data:status" && event.data.state === "unavailable"));
  assert.ok(frames.some((event) => event.binding === "second" && event.event === "dashboard-event"));
  connection.close(); await new Promise((resolve) => setTimeout(resolve, 10)); assert.equal(aborted, true);
});

test("replay reports a gap and binding replacement stops its upstream", async () => {
  const sources = parseDataSources(JSON.stringify({ sources: [{ id: "up", org: "org", base_url: "https://up.test", operations: { status: { path: "/status" } }, subscriptions: { events: { transport: "sse", path: "/events" } } }] }));
  const { db } = fixture(sources); let aborted = false;
  const data = createArtifactData({ db, sources, fetchImpl: async (_url, options) => { const stream = new ReadableStream({ start(controller) { controller.enqueue(new TextEncoder().encode("id: one\ndata: {\"ok\":true}\n\n")); } }); options.signal.addEventListener("abort", () => { aborted = true; }); return new Response(stream, { status: 200, headers: { "content-type": "text/event-stream" } }); } });
  data.setBindings("a", "org", { bindings: { reviews: { source: "up", operations: ["status"], subscriptions: ["events"] } } });
  const connection = data.subscribe("a", "org", [{ binding: "reviews", subscription: "events" }], () => {});
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(data.replay("a", "org", [{ binding: "reviews", subscription: "events" }], { "reviews:events": "missing" }).gaps.length, 1);
  data.setBindings("a", "org", { bindings: {} }); connection.close(); await new Promise((resolve) => setTimeout(resolve, 10)); assert.equal(aborted, true);
});

test("poll subscriptions emit message frames and stop independently", async () => {
  const sources = parseDataSources(JSON.stringify({ sources: [{ id: "poll", org: "org", base_url: "https://poll.test", operations: { status: { path: "/status" } }, subscriptions: { events: { transport: "poll", operation: "status", interval_ms: 1000 } } }] }));
  const { db } = fixture(sources); let calls = 0; const data = createArtifactData({ db, sources, fetchImpl: async () => { calls += 1; return new Response(JSON.stringify({ calls }), { status: 200 }); } });
  data.setBindings("a", "org", { bindings: { reviews: { source: "poll", operations: [], subscriptions: ["events"] } } });
  assert.deepEqual(await data.query("a", "org", "reviews", "status"), { error: "not_found" });
  const events = []; const connection = data.subscribe("a", "org", [{ binding: "reviews", subscription: "events" }], (event) => events.push(event));
  await new Promise((resolve) => setTimeout(resolve, 1030)); connection.close(); assert.ok(events.some((event) => event.event === "message")); assert.ok(calls >= 1);
});
