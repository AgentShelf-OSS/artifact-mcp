import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const sdk = readFileSync(new URL("../assets/artifact-data-client.js", import.meta.url), "utf8");
const shell = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");
const brokerStart = shell.indexOf("  function createViewerDataBroker(options)");
const brokerEnd = shell.indexOf("\n  var dataBroker=", brokerStart);
let brokerFetch = () => Promise.reject(new Error("missing fetch"));
const brokerContext = { AbortController, Map, Promise, encodeURIComponent, JSON, Error, setTimeout, clearTimeout, fetch: (...args) => brokerFetch(...args), EventSource: class {} };
const createDataBroker = vm.runInNewContext(shell.slice(brokerStart, brokerEnd) + "\ncreateViewerDataBroker", brokerContext);

function harness() {
  const messages = [], handlers = [], timers = [];
  const parent = { postMessage: message => messages.push(message) };
  const window = { parent, addEventListener: (type, handler) => { if (type === "message") handlers.push(handler); }, setTimeout: (fn, ms) => { timers.push({ fn, ms }); return timers.length; } };
  vm.runInNewContext(sdk, { window, setTimeout: window.setTimeout, clearTimeout: () => {}, Map, Promise, Error, AbortController });
  return { window, parent, messages, handlers, timers, receive(data, source = parent) { handlers.forEach(handler => handler({ data, source })); } };
}

test("SDK performs source-checked handshake and query", async () => {
  const h = harness();
  assert.deepEqual(JSON.parse(JSON.stringify(h.messages)), [{ type: "data:hello" }]);
  const state = h.window.artifact.data.ready;
  h.receive({ type: "data:ready", enabled: true, bindings: { reviews: { operations: { status: {} } } } });
  assert.deepEqual(JSON.parse(JSON.stringify(await state)), { enabled: true, bindings: { reviews: { operations: { status: {} } } } });
  await Promise.resolve();
  const result = h.window.artifact.data.query("reviews", "status", {});
  await Promise.resolve();
  const query = h.messages.at(-1);
  assert.equal(query.type, "data:query");
  h.receive({ type: "data:result", requestId: query.requestId, data: { ok: true } });
  assert.deepEqual(JSON.parse(JSON.stringify(await result)), { ok: true });
  h.receive({ type: "data:ready", enabled: true }, {});
  assert.equal(h.messages.length, 2);
});

test("SDK subscriptions are synchronous and callbacks receive envelopes", async () => {
  const h = harness();
  h.receive({ type: "data:ready", enabled: true, bindings: {} });
  await h.window.artifact.data.ready;
  const received = [];
  const unsubscribe = h.window.artifact.data.subscribe("reviews", "events", value => received.push(value));
  await Promise.resolve();
  const subscribe = h.messages.at(-1);
  assert.equal(subscribe.type, "data:subscribe");
  h.receive({ type: "data:event", requestId: subscribe.requestId, binding: "reviews", subscription: "events", event: "dashboard-event", id: "7", data: { type: "provider_event" } });
  assert.deepEqual(JSON.parse(JSON.stringify(received)), [{ binding: "reviews", subscription: "events", event: "dashboard-event", id: "7", data: { type: "provider_event" } }]);
  unsubscribe();
  assert.equal(h.messages.at(-1).type, "data:unsubscribe");
});

test("SDK routes multiplexed events to exactly one callback per request", async () => {
  const h = harness();
  h.receive({ type: "data:ready", enabled: true, bindings: {} });
  await h.window.artifact.data.ready;
  const first = [], second = [];
  const stopFirst = h.window.artifact.data.subscribe("reviews", "events", value => first.push(value.id));
  const stopSecond = h.window.artifact.data.subscribe("reviews", "events", value => second.push(value.id));
  await Promise.resolve();
  const requests = h.messages.filter(message => message.type === "data:subscribe");
  h.receive({ type: "data:event", requestId: requests[0].requestId, binding: "reviews", subscription: "events", event: "dashboard-event", id: "a", data: {} });
  h.receive({ type: "data:event", requestId: requests[1].requestId, binding: "reviews", subscription: "events", event: "dashboard-event", id: "b", data: {} });
  assert.deepEqual(first, ["a"]);
  assert.deepEqual(second, ["b"]);
  stopFirst();
  h.receive({ type: "data:event", requestId: requests[1].requestId, binding: "reviews", subscription: "events", event: "dashboard-event", id: "c", data: {} });
  assert.deepEqual(first, ["a"]);
  assert.deepEqual(second, ["b", "c"]);
  stopSecond();
});

test("historical broker capability stays disabled and does not fetch", () => {
  const calls = [], messages = [], frame = {};
  const broker = createDataBroker({ enabled: false, artifactId: "old", frame: () => frame, post: message => messages.push(message) });
  assert.equal(broker.handle({ source: frame, data: { type: "data:hello" } }), true);
  assert.deepEqual(JSON.parse(JSON.stringify(messages)), [{ type: "data:ready", enabled: false, bindings: {} }]);
  assert.equal(calls.length, 0);
});

test("a viewer without live bindings reports a disabled capability", async () => {
  const messages = [], frame = {};
  brokerFetch = () => Promise.resolve({ ok: true, json: async () => ({ bindings: {} }) });
  const broker = createDataBroker({ enabled: true, artifactId: "unbound", frame: () => frame, post: message => messages.push(message) });
  broker.handle({ source: frame, data: { type: "data:hello" } });
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(JSON.parse(JSON.stringify(messages)), [{ type: "data:ready", enabled: false, bindings: {} }]);
});

test("real shell broker multiplexes callbacks, cursors, status, and source checks", async () => {
  const calls = [], messages = [], frame = {}, events = [];
  class FakeEventSource {
    constructor(url) { this.url = url; this.handlers = {}; this.closed = false; events.push(this); }
    addEventListener(type, callback) { (this.handlers[type] ||= []).push(callback); }
    close() { this.closed = true; }
    emit(type, value, lastEventId = "") { (this.handlers[type] || []).forEach(callback => callback({ data: JSON.stringify(value), lastEventId })); }
  }
  brokerFetch = (url, init) => { calls.push({ url, init }); return Promise.resolve({ ok: true, json: async () => ({ bindings: { reviews: { operations: ["status"], subscriptions: ["events"] } } }) }); };
  const contextBroker = createDataBroker;
  brokerContext.EventSource = FakeEventSource;
  const broker = createDataBroker({ enabled: true, artifactId: "artifact", frame: () => frame, post: message => messages.push(JSON.parse(JSON.stringify(message))) });
  assert.equal(broker.handle({ source: {}, data: { type: "data:hello" } }), false);
  broker.handle({ source: frame, data: { type: "data:hello" } });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(calls.length, 1);
  broker.handle({ source: frame, data: { type: "data:subscribe", requestId: "one", binding: "reviews", subscription: "events" } });
  broker.handle({ source: frame, data: { type: "data:subscribe", requestId: "two", binding: "reviews", subscription: "events" } });
  assert.equal(events.length, 2);
  events[1].emit("artifact-data", { binding: "reviews", subscription: "events", event: "dashboard-event", id: "7", data: { n: 1 } }, "7");
  assert.deepEqual(messages.filter(message => message.type === "data:event").map(message => message.requestId), ["one", "two"]);
  broker.handle({ source: frame, data: { type: "data:unsubscribe", requestId: "one", binding: "reviews", subscription: "events" } });
  events[1].emit("data:status", { state: "reconnecting" });
  events[1].emit("data:resync", { reason: "gap" });
  const tail = messages.slice(-2);
  assert.deepEqual(tail.map(message => [message.requestId, message.event, message.data]), [["two", "data:status", { state: "reconnecting" }], ["two", "data:resync", { reason: "gap" }]]);
  broker.handle({ source: frame, data: { type: "data:subscribe", requestId: "three", binding: "reviews", subscription: "events" } });
  assert.match(events.at(-1).url, /cursor=/);
  broker.close();
  assert.equal(events.at(-1).closed, true);
  void contextBroker;
});
