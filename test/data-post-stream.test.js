// ADR-0017: broker POST and streaming operations (Node runtime).
import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { once } from "node:events";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";
import { createArtifactData, parseDataSources } from "../lib/artifact-data.js";
import { createDataSourceRegistry } from "../lib/data-source-registry.js";
import { createApp } from "../lib/app.js";

const BASE = { id: "api", org: "org", base_url: "https://example.test" };
const parse = (operations, extra = {}) => parseDataSources(JSON.stringify({ sources: [{ ...BASE, ...extra, operations }] }));
const strings = (count, schema = { type: "string" }) => Object.fromEntries(Array.from({ length: count }, (_, i) => [`p${i}`, { ...schema }]));

function database() {
  const db = new Database(":memory:"); db.pragma("foreign_keys=ON"); migrateDatabase(db);
  db.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES ('a','client','org','A')").run();
  return db;
}
function brokerWith(sources, options = {}) {
  const db = database();
  const data = createArtifactData({ db, sources, ...options });
  data.setBindings("a", "org", { bindings: { chat: { source: sources[0].id, operations: Object.keys(sources[0].operations), subscriptions: [] } } });
  return { db, data };
}

test("operation method and stream are validated with POST and streaming bounds", () => {
  const [source] = parse({ get: { path: "/get" }, post: { path: "/post", method: "POST" }, live: { path: "/live", method: "POST", stream: true, timeout_ms: 120000 } });
  assert.equal(source.operations.get.method, "GET"); assert.equal(source.operations.get.stream, false);
  assert.equal(source.operations.post.method, "POST"); assert.equal(source.operations.live.stream, true);
  // POST bounds: 32 params, max_length 16384.
  assert.doesNotThrow(() => parse({ op: { path: "/x", method: "POST", params: strings(32, { type: "string", max_length: 16384 }) } }));
  assert.throws(() => parse({ op: { path: "/x", method: "POST", params: strings(33) } }), /parameter limit/);
  assert.throws(() => parse({ op: { path: "/x", method: "POST", params: { q: { type: "string", max_length: 16385 } } } }), /max_length/);
  // GET bounds unchanged: 16 params, max_length 256.
  assert.doesNotThrow(() => parse({ op: { path: "/x", params: strings(16, { type: "string", max_length: 256 }) } }));
  assert.throws(() => parse({ op: { path: "/x", params: strings(17) } }), /parameter limit/);
  assert.throws(() => parse({ op: { path: "/x", method: "GET", params: { q: { type: "string", max_length: 257 } } } }), /max_length/);
  // timeout_ms: 100..30000, or 100..120000 for streaming operations.
  assert.throws(() => parse({ op: { path: "/x", timeout_ms: 30001 } }), /limits/);
  assert.throws(() => parse({ op: { path: "/x", method: "POST", timeout_ms: 30001 } }), /limits/);
  assert.doesNotThrow(() => parse({ op: { path: "/x", stream: true, timeout_ms: 120000 } }));
  assert.throws(() => parse({ op: { path: "/x", stream: true, timeout_ms: 120001 } }), /limits/);
  assert.throws(() => parse({ op: { path: "/x", stream: true, timeout_ms: 99 } }), /limits/);
  // Field types and unknown fields.
  for (const op of [{ method: "PUT" }, { method: "post" }, { method: 1 }, { stream: "true" }, { stream: 1 }, { verb: "POST" }]) {
    assert.throws(() => parse({ op: { path: "/x", ...op } }));
  }
  // Push sources accept neither field.
  for (const op of [{ method: "GET" }, { method: "POST" }, { stream: false }, { stream: true }]) {
    assert.throws(() => parseDataSources(JSON.stringify({ sources: [{ id: "push", org: "org", kind: "push", operations: { latest: { key: "k", ...op } } }] })), /push operation/);
  }
});

test("managed sources accept the same fields and discovery reports method and stream", () => {
  const db = database(); const data = createArtifactData({ db, sources: [] });
  const registry = createDataSourceRegistry({ db, data, env: {}, orgs: { has: () => true }, artifacts: {} });
  const definition = { id: "managed", org: "org", kind: "http", base_url: "https://example.test", operations: { chat: { path: "/chat", method: "POST", stream: true, timeout_ms: 60000, params: { prompt: { type: "string", max_length: 16384 } } }, list: { path: "/list" } }, subscriptions: {} };
  const view = registry.create({ definition, enabled: true });
  assert.equal(view.definition.operations.chat.method, "POST");
  assert.equal(view.definition.operations.chat.stream, true);
  assert.equal(view.definition.operations.list.method, "GET");
  assert.equal(view.definition.operations.list.stream, false);
  assert.throws(() => registry.create({ definition: { ...definition, id: "bad", operations: { chat: { path: "/chat", method: "PATCH" } } }, enabled: true }), (error) => error.code === "invalid_source");
  assert.throws(() => registry.create({ definition: { ...definition, id: "bad2", operations: { chat: { path: "/chat", stream: true, timeout_ms: 120001 } } }, enabled: true }), (error) => error.code === "invalid_source");
  assert.throws(() => registry.create({ definition: { id: "bad-push", org: "org", kind: "push", operations: { latest: { stream: true } }, subscriptions: {} }, enabled: true }), (error) => error.code === "invalid_source");
  // Re-saving the admin view's definition round-trips.
  assert.equal(registry.disable("managed", view.version).enabled, false);
  const listed = data.listSources("org");
  assert.equal(listed.length, 0);
  registry.enable("managed", registry.get("managed").version);
  const [publicView] = data.listSources("org");
  assert.deepEqual(publicView.operations.chat, { method: "POST", params: { prompt: { max_length: 16384, type: "string" } }, stream: true });
  assert.deepEqual(publicView.operations.list, { method: "GET", params: {}, stream: false });
  const push = parseDataSources(JSON.stringify({ sources: [{ id: "push", org: "org", kind: "push", operations: { latest: { key: "k" } } }] }));
  assert.deepEqual(createArtifactData({ db, sources: push }).listSources("org")[0].operations.latest, { method: "GET", params: {}, stream: false });
  db.close();
});

test("POST sends typed supplied and defaulted parameters as a JSON body with path placeholders", async () => {
  const operations = { send: { path: "/rooms/{room}/send", method: "POST", params: {
    room: { type: "string", required: true }, text: { type: "string", max_length: 16384 }, count: { type: "integer", default: 3 }, loud: { type: "boolean" }, mode: { type: "string", default: "fast" }, unused: { type: "string" }
  } } };
  const withEnv = parseDataSources(JSON.stringify({ sources: [{ ...BASE, headers_env: { "Content-Type": "QA_CT", Authorization: "QA_AUTH" }, operations }] }), { QA_CT: "text/plain", QA_AUTH: "Bearer synthetic" });
  const calls = [];
  const { data, db } = brokerWith(withEnv, { fetchImpl: async (url, init) => { calls.push({ url, init }); return Response.json({ ok: true }); } });
  const result = await data.query("a", "org", "chat", "send", { room: "r 1", text: "hello", loud: false });
  assert.deepEqual(result, { data: { ok: true } });
  assert.equal(calls[0].url, "https://example.test/rooms/r%201/send");
  assert.equal(calls[0].init.method, "POST");
  assert.equal(calls[0].init.redirect, "error");
  assert.deepEqual(JSON.parse(calls[0].init.body), { text: "hello", count: 3, loud: false, mode: "fast" });
  assert.equal(calls[0].init.headers["content-type"], "application/json");
  assert.equal(calls[0].init.headers["Content-Type"], undefined);
  assert.equal(calls[0].init.headers.Authorization, "Bearer synthetic");
  assert.deepEqual(await data.query("a", "org", "chat", "send", { room: "../x" }), { error: "bad_params" });
  assert.deepEqual(await data.query("a", "org", "chat", "send", { room: "r", count: "3" }), { error: "bad_params" });
  assert.equal(calls.length, 1);
  db.close();
});

test("POST bodies over 64 KiB are rejected before any upstream call", async () => {
  const params = Object.fromEntries(Array.from({ length: 5 }, (_, i) => [`p${i}`, { type: "string", max_length: 16384 }]));
  const sources = parse({ big: { path: "/big", method: "POST", params } });
  let calls = 0;
  const { data, db } = brokerWith(sources, { fetchImpl: async () => { calls += 1; return Response.json({}); } });
  const values = Object.fromEntries(Object.keys(params).map((name) => [name, "x".repeat(16000)]));
  assert.deepEqual(await data.query("a", "org", "chat", "big", values), { error: "too_large" });
  assert.equal(calls, 0);
  const fits = Object.fromEntries(Object.keys(params).slice(0, 4).map((name) => [name, "x".repeat(16000)]));
  assert.deepEqual(await data.query("a", "org", "chat", "big", fits), { data: {} });
  assert.equal(calls, 1);
  db.close();
});

// --- streaming over real HTTP: upstream server -> broker app -> client fetch ---

async function listen(server) { server.listen(0, "127.0.0.1"); await once(server, "listening"); return `http://127.0.0.1:${server.address().port}`; }

async function streamHarness(handler, { operations, streamIdleMs } = {}) {
  const upstream = http.createServer(handler);
  const upstreamUrl = await listen(upstream);
  const db = database();
  const sources = parseDataSources(JSON.stringify({ sources: [{ id: "llm", org: "org", base_url: upstreamUrl, operations: operations || {
    chat: { path: "/chat", method: "POST", stream: true, max_bytes: 4096, timeout_ms: 5000, params: { prompt: { type: "string", max_length: 16384 } } },
    plain: { path: "/plain" }
  } }] }));
  const data = createArtifactData({ db, sources, ...(streamIdleMs ? { streamIdleMs } : {}) });
  data.setBindings("a", "org", { bindings: { chat: { source: "llm", operations: Object.keys(sources[0].operations), subscriptions: [] } } });
  const app = createApp({
    resolveViewer: async () => ({ email: "viewer@org.test", org: "org", isAdmin: false }),
    artifacts: { getArtifactMeta: (id) => id === "a" ? { id, org: "org", title: "A" } : null, readArtifact: () => null, readBundleFile: () => null, listOrgArtifacts: () => [], listAllGroupedByOrg: () => new Map(), listOrgIds: () => [] },
    orgs: { list: () => [], names: () => [], has: () => true, colorMap: () => ({}) },
    keys: { list: () => [] }, reactions: { get: () => ({ favorite: 0, vote: 0 }) }, feedback: { listForArtifact: () => [] },
    pages: { notFound: () => "not found", notSignedIn: () => "not signed in", gallery: () => "", shell: () => "", settings: () => "" },
    data, limits: { dataPerWindow: 1000 },
    logger: { info() {}, warn() {}, error() {} }
  });
  const server = http.createServer(app);
  const appUrl = await listen(server);
  const post = (route, body, init = {}) => fetch(`${appUrl}/a/data/${route}`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body), ...init });
  const close = async () => { server.closeAllConnections?.(); upstream.closeAllConnections?.(); server.close(); upstream.close(); db.close(); };
  return { post, close, data };
}

async function* lines(response) {
  const reader = response.body.getReader(); const decoder = new TextDecoder(); let buffer = "";
  while (true) {
    const part = await reader.read(); if (part.done) break;
    buffer += decoder.decode(part.value, { stream: true });
    let split; while ((split = buffer.indexOf("\n")) >= 0) { const line = buffer.slice(0, split); buffer = buffer.slice(split + 1); yield JSON.parse(line); }
  }
  if (buffer.trim()) yield JSON.parse(buffer);
}

test("stream route relays NDJSON chunks in order and incrementally, then done", async () => {
  let release, upstreamBody, upstreamHeaders;
  const gate = new Promise((resolve) => { release = resolve; });
  const h = await streamHarness(async (req, res) => {
    let raw = ""; for await (const part of req) raw += part; upstreamBody = JSON.parse(raw); upstreamHeaders = req.headers;
    res.writeHead(200, { "content-type": "application/x-ndjson; charset=utf-8" });
    res.write('{"delta":"Hel"}\n');
    await gate; // the upstream has not finished until the client saw the first chunk
    res.write('\n{"delta":"lo"}\r\n');
    res.end('{"done":"upstream","text":"Hello"}');
  });
  try {
    const response = await h.post("stream", { binding: "chat", operation: "chat", params: { prompt: "hi" } });
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("content-type"), "application/x-ndjson; charset=utf-8");
    assert.equal(response.headers.get("cache-control"), "no-store");
    assert.equal(response.headers.get("x-accel-buffering"), "no");
    const seen = [];
    for await (const line of lines(response)) { seen.push(line); if (seen.length === 1) release(); }
    assert.deepEqual(seen, [{ chunk: { delta: "Hel" } }, { chunk: { delta: "lo" } }, { chunk: { done: "upstream", text: "Hello" } }, { done: true }]);
    assert.deepEqual(upstreamBody, { prompt: "hi" });
    assert.equal(upstreamHeaders["content-type"], "application/json");
    assert.equal(upstreamHeaders.accept, "application/x-ndjson");
    // The same operation through /data/query is collected and returns the last line.
    const again = await h.post("query", { binding: "chat", operation: "chat", params: { prompt: "hi" } });
    assert.equal(again.status, 200);
    assert.deepEqual(await again.json(), { data: { done: "upstream", text: "Hello" } });
  } finally { release(); await h.close(); }
});

test("stream route maps pre-start failures to JSON statuses and rejects non-stream operations", async () => {
  let mode = "json";
  const h = await streamHarness((req, res) => {
    req.resume();
    if (mode === "json") { res.writeHead(200, { "content-type": "application/json" }); res.end('{"a":1}\n'); }
    else if (mode === "missing") { res.writeHead(404); res.end(); }
    else { res.writeHead(500, { "content-type": "application/x-ndjson" }); res.end('{"a":1}\n'); }
  });
  try {
    let response = await h.post("stream", { binding: "chat", operation: "plain" });
    assert.equal(response.status, 400); assert.deepEqual(await response.json(), { error: "bad_params" });
    response = await h.post("stream", { binding: "chat", operation: "chat" });
    assert.equal(response.status, 502); assert.deepEqual(await response.json(), { error: "data_unavailable" });
    mode = "status";
    response = await h.post("stream", { binding: "chat", operation: "chat" });
    assert.equal(response.status, 502);
    mode = "missing";
    response = await h.post("stream", { binding: "chat", operation: "chat" });
    assert.equal(response.status, 404);
    response = await h.post("stream", { binding: "nope", operation: "chat" });
    assert.equal(response.status, 404);
    response = await h.post("stream", { binding: "chat", operation: "chat", extra: 1 });
    assert.equal(response.status, 400);
    response = await h.post("stream", { binding: "chat", operation: "chat", params: { prompt: 1 } });
    assert.equal(response.status, 400);
    response = await fetch(response.url, { method: "POST", headers: { "content-type": "application/json" }, body: "{" });
    assert.equal(response.status, 400); assert.deepEqual(await response.json(), { error: "bad_params" });
    response = await fetch(response.url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ binding: "chat", operation: "chat", params: { prompt: "x".repeat(70 * 1024) } }) });
    assert.equal(response.status, 413); assert.deepEqual(await response.json(), { error: "too_large" });
  } finally { await h.close(); }
});

test("stream failures after start end with one error line: max_bytes, malformed line", async () => {
  let mode = "large";
  const h = await streamHarness((req, res) => {
    req.resume();
    res.writeHead(200, { "content-type": "application/x-ndjson" });
    res.write('{"n":1}\n');
    const rest = mode === "large" ? `${JSON.stringify({ pad: "x".repeat(5000) })}\n` : "not json\n{\"n\":2}\n";
    setTimeout(() => res.end(rest), 25);
  });
  try {
    let response = await h.post("stream", { binding: "chat", operation: "chat" });
    let seen = []; for await (const line of lines(response)) seen.push(line);
    assert.deepEqual(seen, [{ chunk: { n: 1 } }, { error: "too_large" }]);
    mode = "malformed";
    response = await h.post("stream", { binding: "chat", operation: "chat" });
    seen = []; for await (const line of lines(response)) seen.push(line);
    assert.deepEqual(seen, [{ chunk: { n: 1 } }, { error: "data_unavailable" }]);
    response = await h.post("query", { binding: "chat", operation: "chat" });
    assert.equal(response.status, 502);
    mode = "large";
    response = await h.post("query", { binding: "chat", operation: "chat" });
    assert.equal(response.status, 413);
  } finally { await h.close(); }
});

test("an idle gap between lines ends the stream with data_unavailable", async () => {
  let upstreamClosed;
  const closed = new Promise((resolve) => { upstreamClosed = resolve; });
  const h = await streamHarness((req, res) => {
    req.resume(); res.on("close", upstreamClosed);
    res.writeHead(200, { "content-type": "application/x-ndjson" });
    res.write('{"n":1}\n'); // then stall beyond the idle window
  }, { streamIdleMs: 60 });
  try {
    const started = Date.now();
    const response = await h.post("stream", { binding: "chat", operation: "chat" });
    const seen = []; for await (const line of lines(response)) seen.push(line);
    assert.deepEqual(seen, [{ chunk: { n: 1 } }, { error: "data_unavailable" }]);
    assert.ok(Date.now() - started < 3000);
    await closed;
  } finally { await h.close(); }
});

test("client disconnect aborts the upstream stream request", async () => {
  let upstreamClosed;
  const closed = new Promise((resolve) => { upstreamClosed = resolve; });
  const h = await streamHarness((req, res) => {
    req.resume(); res.on("close", upstreamClosed);
    res.writeHead(200, { "content-type": "application/x-ndjson" });
    res.write('{"n":1}\n');
    const timer = setInterval(() => res.write("\n"), 20); res.on("close", () => clearInterval(timer));
  });
  try {
    const controller = new AbortController();
    const response = await h.post("stream", { binding: "chat", operation: "chat" }, { signal: controller.signal });
    const reader = response.body.getReader();
    const first = await reader.read();
    assert.match(new TextDecoder().decode(first.value), /\{"chunk":\{"n":1\}\}/);
    controller.abort();
    await Promise.race([closed, new Promise((_, reject) => setTimeout(() => reject(new Error("upstream was not aborted")), 3000))]);
  } finally { await h.close(); }
});
