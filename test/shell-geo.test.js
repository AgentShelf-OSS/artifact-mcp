import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const shell = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");
const start = shell.indexOf("  function createViewerGeoBroker(options)");
const end = shell.indexOf("  // End viewer geolocation broker.", start);
const createBroker = vm.runInNewContext(shell.slice(start, end) + "\ncreateViewerGeoBroker", {});
const settle = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };

function harness({ enabled = true, supported = true, answer = "once", entries = new Map() } = {}) {
  let now = 10_000;
  const messages = [], asks = [], lookups = [], frame = {};
  const geolocation = supported ? {
    getCurrentPosition: (ok, fail, opts) => lookups.push({ ok, fail, opts }),
  } : null;
  const broker = createBroker({
    enabled, artifactId: "artifact", geolocation, now: () => now, frame: () => frame,
    storage: { getItem: k => entries.get(k) ?? null, setItem: (k, v) => entries.set(k, v), removeItem: k => entries.delete(k) },
    post: message => messages.push(JSON.parse(JSON.stringify(message))),
    ask: () => { asks.push(1); return answer; },
  });
  return {
    messages, asks, lookups, entries,
    send: async (data, source = frame) => { const handled = broker.handle({ data, source }); await settle(); return handled; },
    tick: ms => { now += ms; },
    fix: (i, lat, lon, accuracy = 12) => lookups[i].ok({ coords: { latitude: lat, longitude: lon, accuracy }, timestamp: 5 }),
    deny: (i, code = 1) => lookups[i].fail({ code }),
  };
}

test("ignores other frames and non-geo messages", async () => {
  const h = harness();
  assert.equal(await h.send({ type: "geo:request", requestId: "a" }, {}), false);
  assert.equal(await h.send({ type: "state:get", key: "x" }), false);
  assert.equal(h.asks.length, 0);
});

test("hello reports support and remembered consent", async () => {
  const h = harness({ entries: new Map([["artifact-geo:artifact", "allow"]]) });
  await h.send({ type: "geo:hello" });
  assert.deepEqual(h.messages, [{ type: "geo:ready", supported: true, remembered: true }]);
});

test("asks once, then returns the position to the frame", async () => {
  const h = harness({ answer: "once" });
  await h.send({ type: "geo:request", requestId: "r1", highAccuracy: true });
  assert.equal(h.asks.length, 1);
  assert.equal(h.lookups[0].opts.enableHighAccuracy, true);
  h.fix(0, 36.1, -115.17);
  assert.deepEqual(h.messages.at(-1), { type: "geo:position", requestId: "r1", lat: 36.1, lon: -115.17, accuracy: 12, timestamp: 5 });
  assert.equal(h.entries.has("artifact-geo:artifact"), false);
  h.tick(2000);
  await h.send({ type: "geo:request", requestId: "r2" });
  assert.equal(h.asks.length, 2, "allow once does not persist");
});

test("always allow persists per artifact and skips the prompt", async () => {
  const h = harness({ answer: "always" });
  await h.send({ type: "geo:request", requestId: "r1" });
  h.fix(0, 1, 2);
  assert.equal(h.entries.get("artifact-geo:artifact"), "allow");
  h.tick(2000);
  await h.send({ type: "geo:request", requestId: "r2" });
  assert.equal(h.asks.length, 1);
  assert.equal(h.lookups.length, 2);
});

test("declining never touches the browser API", async () => {
  const h = harness({ answer: "deny" });
  await h.send({ type: "geo:request", requestId: "r1" });
  assert.equal(h.lookups.length, 0);
  assert.deepEqual(h.messages.at(-1), { type: "geo:error", requestId: "r1", reason: "declined" });
});

test("browser denial forgets stored consent", async () => {
  const h = harness({ entries: new Map([["artifact-geo:artifact", "allow"]]) });
  await h.send({ type: "geo:request", requestId: "r1" });
  h.deny(0, 1);
  assert.deepEqual(h.messages.at(-1), { type: "geo:error", requestId: "r1", reason: "denied" });
  assert.equal(h.entries.has("artifact-geo:artifact"), false);
});

test("rejects concurrent, rapid, malformed and unsupported requests", async () => {
  const h = harness({ entries: new Map([["artifact-geo:artifact", "allow"]]) });
  await h.send({ type: "geo:request", requestId: "r1" });
  await h.send({ type: "geo:request", requestId: "r2" });
  assert.deepEqual(h.messages.at(-1), { type: "geo:error", requestId: "r2", reason: "busy" });
  h.fix(0, 1, 2);
  await h.send({ type: "geo:request", requestId: "r3" });
  assert.deepEqual(h.messages.at(-1), { type: "geo:error", requestId: "r3", reason: "busy" });
  await h.send({ type: "geo:request", requestId: "bad id!" });
  assert.deepEqual(h.messages.at(-1), { type: "geo:error", requestId: "", reason: "bad_request" });
  const off = harness({ supported: false });
  await off.send({ type: "geo:request", requestId: "r1" });
  assert.deepEqual(off.messages.at(-1), { type: "geo:error", requestId: "r1", reason: "unsupported" });
  const historical = harness({ enabled: false });
  await historical.send({ type: "geo:request", requestId: "r1" });
  assert.equal(historical.asks.length, 0);
});

test("forget clears consent", async () => {
  const h = harness({ entries: new Map([["artifact-geo:artifact", "allow"]]) });
  await h.send({ type: "geo:forget" });
  assert.deepEqual(h.messages.at(-1), { type: "geo:ready", supported: true, remembered: false });
});
