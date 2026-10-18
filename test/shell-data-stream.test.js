import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

// ADR-0017: the viewer shell relays streaming operations as data:chunk messages.
const shell = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");
const sdk = readFileSync(new URL("../assets/artifact-data-client.js", import.meta.url), "utf8");
const start = shell.indexOf("  function createViewerDataBroker(options)");
const end = shell.indexOf("\n  var dataBroker=", start);
let brokerFetch = () => Promise.reject(new Error("missing fetch"));
const context = { AbortController, Map, Promise, encodeURIComponent, JSON, Error, Object, TextDecoder, setTimeout, clearTimeout, fetch: (...args) => brokerFetch(...args), EventSource: class {} };
const createDataBroker = vm.runInNewContext(shell.slice(start, end) + "\ncreateViewerDataBroker", context);
const settle = () => new Promise(resolve => setImmediate(resolve));

function ndjsonResponse(lines, { status = 200 } = {}) {
  const encoder = new TextEncoder();
  const body = new ReadableStream({
    async pull(controller) {
      const next = lines.shift();
      if (next === undefined) { controller.close(); return; }
      await new Promise(resolve => setTimeout(resolve, 5));
      controller.enqueue(encoder.encode(next));
    },
  });
  return new Response(body, { status, headers: { "content-type": "application/x-ndjson" } });
}

async function ready(fetchImpl) {
  const calls = [], messages = [], frame = {};
  brokerFetch = (url, init) => {
    calls.push({ url, init });
    if (url.endsWith("/data")) return Promise.resolve({ ok: true, json: async () => ({ bindings: { concierge: { operations: ["chat", "status"] } } }) });
    return fetchImpl(url, init);
  };
  const broker = createDataBroker({ enabled: true, artifactId: "artifact", frame: () => frame, post: message => messages.push(JSON.parse(JSON.stringify(message))) });
  broker.handle({ source: frame, data: { type: "data:hello" } });
  await settle();
  return { calls, messages, send: data => broker.handle({ source: frame, data }) };
}

test("streaming query relays each chunk then the last chunk as the result", async () => {
  const h = await ready(() => Promise.resolve(ndjsonResponse([
    '{"chunk":{"type":"status","text":"Thinking"}}\n{"chunk":',
    '{"type":"final","blocks":[]}}\n',
    '{"done":true}\n',
  ])));
  h.send({ type: "data:query", requestId: "r1", binding: "concierge", operation: "chat", params: { msg: "hi" }, stream: true });
  for (let i = 0; i < 20 && !h.messages.some(m => m.type === "data:result"); i++) await new Promise(r => setTimeout(r, 10));
  assert.equal(h.calls.at(-1).url, "/artifact/data/stream");
  assert.deepEqual(JSON.parse(h.calls.at(-1).init.body), { binding: "concierge", operation: "chat", params: { msg: "hi" } });
  assert.deepEqual(h.messages.slice(1), [
    { type: "data:chunk", requestId: "r1", data: { type: "status", text: "Thinking" } },
    { type: "data:chunk", requestId: "r1", data: { type: "final", blocks: [] } },
    { type: "data:result", requestId: "r1", data: { type: "final", blocks: [] } },
  ]);
});

test("an error line or a missing done line ends with data:error", async () => {
  const h = await ready(() => Promise.resolve(ndjsonResponse(['{"chunk":1}\n{"error":"too_large"}\n'])));
  h.send({ type: "data:query", requestId: "r1", binding: "concierge", operation: "chat", params: {}, stream: true });
  for (let i = 0; i < 20 && !h.messages.some(m => m.type === "data:error"); i++) await new Promise(r => setTimeout(r, 10));
  assert.deepEqual(h.messages.slice(1), [
    { type: "data:chunk", requestId: "r1", data: 1 },
    { type: "data:error", requestId: "r1", reason: "too_large" },
  ]);
  const cut = await ready(() => Promise.resolve(ndjsonResponse(['{"chunk":1}\n'])));
  cut.send({ type: "data:query", requestId: "r2", binding: "concierge", operation: "chat", params: {}, stream: true });
  for (let i = 0; i < 20 && !cut.messages.some(m => m.type === "data:error"); i++) await new Promise(r => setTimeout(r, 10));
  assert.deepEqual(cut.messages.at(-1), { type: "data:error", requestId: "r2", reason: "data_unavailable" });
});

test("a refused stream reports the JSON error and plain queries keep using /data/query", async () => {
  const h = await ready((url) => url.endsWith("/data/stream")
    ? Promise.resolve(new Response(JSON.stringify({ error: "bad_params" }), { status: 400 }))
    : Promise.resolve({ ok: true, json: async () => ({ data: { ok: true } }) }));
  h.send({ type: "data:query", requestId: "r1", binding: "concierge", operation: "status", params: {}, stream: true });
  h.send({ type: "data:query", requestId: "r2", binding: "concierge", operation: "status", params: {} });
  await new Promise(r => setTimeout(r, 30));
  assert.deepEqual(h.messages.find(m => m.requestId === "r1"), { type: "data:error", requestId: "r1", reason: "bad_params" });
  assert.deepEqual(h.messages.find(m => m.requestId === "r2"), { type: "data:result", requestId: "r2", data: { ok: true } });
  assert.equal(h.calls.filter(c => c.url === "/artifact/data/query").length, 1);
});

test("SDK stream() forwards chunks to the callback and resolves with the last", async () => {
  const messages = [], handlers = [];
  const parent = { postMessage: message => messages.push(message) };
  const window = { parent, addEventListener: (type, handler) => { if (type === "message") handlers.push(handler); }, setTimeout: () => 1 };
  vm.runInNewContext(sdk, { window, setTimeout: window.setTimeout, clearTimeout: () => {}, Map, Set, Promise, Error, Object, AbortController });
  const receive = data => handlers.forEach(handler => handler({ data, source: parent }));
  receive({ type: "data:ready", enabled: true, bindings: { concierge: { operations: ["chat"] } } });
  const chunks = [];
  const done = window.artifact.data.stream("concierge", "chat", { msg: "hi" }, chunk => chunks.push(chunk));
  await new Promise(r => setTimeout(r, 5));
  const query = messages.at(-1);
  assert.equal(query.type, "data:query");
  assert.equal(query.stream, true);
  receive({ type: "data:chunk", requestId: query.requestId, data: { n: 1 } });
  receive({ type: "data:chunk", requestId: query.requestId, data: { n: 2 } });
  receive({ type: "data:result", requestId: query.requestId, data: { n: 2 } });
  assert.deepEqual(JSON.parse(JSON.stringify(await done)), { n: 2 });
  assert.deepEqual(JSON.parse(JSON.stringify(chunks)), [{ n: 1 }, { n: 2 }]);
});
