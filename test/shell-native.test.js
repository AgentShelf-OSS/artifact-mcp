import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

// ADR-0018: the shell relays consented native capabilities between the artifact frame and the app.
const shell = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");
const start = shell.indexOf("  function createNativeBroker(options)");
const end = shell.indexOf("  // End native broker.", start);
const createBroker = vm.runInNewContext(shell.slice(start, end) + "\ncreateNativeBroker", { URL, JSON, Date, Promise, Math, Number, Array, Object, String });
const settle = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };

function fakeHost() {
  const sent = [];
  const host = { listener: null, postMessage: raw => sent.push(JSON.parse(raw)), addEventListener: (type, fn) => { if (type === "message") host.listener = fn; } };
  return { host, sent, reply: message => host.listener({ data: JSON.stringify({ v: 1, ...message }) }) };
}

function harness({ host = null, enabled = true, answer = "always", entries = new Map() } = {}) {
  const messages = [], asks = [], timers = [], frame = {};
  const broker = createBroker({
    enabled, artifactId: "trip", origin: "https://artifacts.example", host: () => host, frame: () => frame,
    storage: { getItem: k => entries.get(k) ?? null, setItem: (k, v) => entries.set(k, v), removeItem: k => entries.delete(k) },
    post: m => messages.push(JSON.parse(JSON.stringify(m))), setTimer: (fn) => timers.push(fn),
    ask: kind => { asks.push(kind); return answer; },
  });
  broker.start();
  return { broker, messages, asks, timers, entries,
    send: async (data, source = frame) => { const r = broker.handle({ data, source }); await settle(); return r; },
    fire: async () => { timers.splice(0).forEach(fn => fn()); await settle(); } };
}

const story = { id: "s-1", title: "The Mountain That Moved", text: "Out near Frisco Peak…", voice: "andrea", lat: 38.5, lon: -113.2, radius_m: 3000 };

test("without the app host the artifact learns native features are unavailable", async () => {
  const h = harness();
  await h.send({ type: "native:hello" });
  assert.deepEqual(h.messages, [{ type: "native:ready", available: false, capabilities: [] }]);
  await h.send({ type: "audio:geotrigger", requestId: "r1", items: [story] });
  assert.deepEqual(h.messages.at(-1), { type: "native:error", requestId: "r1", reason: "unsupported" });
  assert.equal(await h.send({ type: "state:get", key: "x" }), false, "other messages pass through");
  assert.equal(await h.send({ type: "native:hello" }, {}), false, "other frames are ignored");
});

test("hello handshake reports the host's capabilities", async () => {
  const f = fakeHost();
  const h = harness({ host: f.host });
  assert.deepEqual(f.sent, [{ v: 1, artifactId: "trip", type: "hello" }]);
  const pending = h.send({ type: "native:hello" });
  f.reply({ type: "ready", app: "0.1.0", capabilities: ["geo.watch", "audio.queue", "audio.geotrigger", "notify"] });
  await pending; await settle();
  assert.deepEqual(h.messages.at(-1), { type: "native:ready", available: true, capabilities: ["geo.watch", "audio.queue", "audio.geotrigger", "notify"] });
});

async function readyHarness(opts = {}) {
  const f = fakeHost();
  const h = harness({ host: f.host, ...opts });
  f.reply({ type: "ready", app: "0.1.0", capabilities: ["geo.watch", "audio.queue", "audio.geotrigger", "notify"] });
  await settle();
  return { ...h, f };
}

test("geotrigger items are validated, consented once, and relayed cleaned", async () => {
  const h = await readyHarness({ answer: "always" });
  await h.send({ type: "audio:geotrigger", requestId: "r1", items: [{ ...story, radius_m: 200 }] });
  assert.deepEqual(h.messages.at(-1), { type: "native:error", requestId: "r1", reason: "bad_request" });
  await h.send({ type: "audio:geotrigger", requestId: "r2", items: [{ ...story, text: undefined, url: "https://evil.example/a.wav" }] });
  assert.deepEqual(h.messages.at(-1), { type: "native:error", requestId: "r2", reason: "bad_request" });
  await h.send({ type: "audio:geotrigger", requestId: "r3", items: [{ ...story, extra: "dropped" }] });
  assert.deepEqual(h.f.sent.at(-1), { v: 1, artifactId: "trip", type: "audio.geotrigger", requestId: "r3", items: [{ id: "s-1", title: story.title, text: story.text, voice: "andrea", lat: 38.5, lon: -113.2, radius_m: 3000 }] });
  assert.deepEqual(h.asks, ["location"]);
  await h.send({ type: "audio:geotrigger", requestId: "r4", items: [story] });
  assert.deepEqual(h.asks, ["location"], "always-allow is remembered per artifact");
  assert.equal(h.entries.get("artifact-native:trip:location"), "allow");
});

test("declined consent never reaches the host", async () => {
  const h = await readyHarness({ answer: "deny" });
  const before = h.f.sent.length;
  await h.send({ type: "geo:watch", requestId: "w1", intervalMs: 1000 });
  assert.deepEqual(h.messages.at(-1), { type: "geo:error", requestId: "w1", reason: "declined" });
  assert.equal(h.f.sent.length, before);
});

test("background watch relays positions for this artifact only and clamps the interval", async () => {
  const h = await readyHarness();
  await h.send({ type: "geo:watch", requestId: "w1", intervalMs: 1000, highAccuracy: true });
  assert.deepEqual(h.f.sent.at(-1), { v: 1, artifactId: "trip", type: "geo.watch", requestId: "w1", intervalMs: 15000, highAccuracy: true });
  h.f.reply({ type: "geo.position", artifactId: "other", requestId: "w1", lat: 1, lon: 2 });
  h.f.reply({ type: "geo.position", artifactId: "trip", requestId: "w1", lat: 36.1, lon: -115.2, accuracy: 9, timestamp: 5 });
  assert.deepEqual(h.messages.at(-1), { type: "geo:position", requestId: "w1", lat: 36.1, lon: -115.2, accuracy: 9, timestamp: 5 });
  assert.equal(h.messages.filter(m => m.type === "geo:position").length, 1);
  h.f.reply({ type: "geo.error", artifactId: "trip", requestId: "w1", reason: "denied" });
  assert.equal(h.entries.has("artifact-native:trip:location"), false, "a host denial forgets consent");
});

test("played events wait for the artifact, then are delivered and acknowledged", async () => {
  const h = await readyHarness();
  h.f.reply({ type: "audio.played", artifactId: "trip", id: "s-1", at: 42 });
  assert.equal(h.messages.some(m => m.type === "audio:played"), false);
  await h.send({ type: "native:hello" });
  assert.deepEqual(h.messages.filter(m => m.type === "audio:played"), [{ type: "audio:played", id: "s-1", at: 42 }]);
  assert.deepEqual(h.f.sent.at(-1), { v: 1, artifactId: "trip", type: "played.ack", ids: ["s-1"] });
});

test("queue, notify and stop are validated and relayed", async () => {
  const h = await readyHarness();
  await h.send({ type: "audio:queue", requestId: "q1", item: { id: "s-2", title: "Luxor", text: "A pyramid of glass." } });
  assert.equal(h.f.sent.at(-1).type, "audio.queue");
  await h.send({ type: "audio:queue", requestId: "q2", item: { id: "bad id", title: "x", text: "y" } });
  assert.deepEqual(h.messages.at(-1), { type: "native:error", requestId: "q2", reason: "bad_request" });
  await h.send({ type: "notify:show", requestId: "n1", title: "Today's surprise", body: "Ready when you are." });
  assert.deepEqual(h.f.sent.at(-1), { v: 1, artifactId: "trip", type: "notify.show", requestId: "n1", title: "Today's surprise", body: "Ready when you are." });
  assert.ok(h.asks.includes("notify"));
  await h.send({ type: "audio:stop" });
  assert.deepEqual(h.f.sent.at(-1), { v: 1, artifactId: "trip", type: "audio.stop" });
  h.f.reply({ type: "audio.state", artifactId: "trip", id: "s-2", state: "playing" });
  assert.deepEqual(h.messages.at(-1), { type: "audio:state", id: "s-2", state: "playing" });
});

test("historical views offer nothing even inside the app", async () => {
  const h = await readyHarness({ enabled: false });
  await h.send({ type: "native:hello" });
  assert.deepEqual(h.messages.at(-1), { type: "native:ready", available: false, capabilities: [] });
});
