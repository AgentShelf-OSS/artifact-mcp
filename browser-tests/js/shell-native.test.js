import { test, assert, readText, runInNewContext, readShell } from "./harness.js";

// ADR-0018: the shell relays consented native capabilities between the artifact frame and the app.
const shell = await readShell();
const start = shell.indexOf("  function createNativeBroker(options)");
const end = shell.indexOf("  // End native broker.", start);
const createBroker = runInNewContext(shell.slice(start, end) + "\ncreateNativeBroker", { URL, JSON, Date, Promise, Math, Number, Array, Object, String });
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
  assert.deepEqual(f.sent, [{ v: 1, type: "hello" }]);
  const pending = h.send({ type: "native:hello" });
  f.reply({ type: "ready", app: "0.1.0", capabilities: ["geo.watch", "audio.queue", "audio.geotrigger", "notify"] });
  await pending; await settle();
  assert.deepEqual(h.messages.at(-1), { type: "native:ready", available: true, capabilities: ["geo.watch", "audio.queue", "audio.geotrigger", "notify"] });
});

async function readyHarness(opts = {}) {
  const f = fakeHost();
  const h = harness({ host: f.host, ...opts });
  f.reply({ type: "ready", app: "0.1.0", capabilities: ["geo.watch", "audio.queue", "audio.geotrigger", "notify", "media.mirror", "library"] });
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

// Mirrors the field allowlists in artifact-mcp-android bridge/Protocol.kt (protocol v1), which
// rejects any unknown field. Update both together.
const APP_FIELDS = {
  hello: ["v", "type"],
  "geo.watch": ["v", "type", "artifactId", "requestId", "intervalMs", "highAccuracy"],
  "geo.unwatch": ["v", "type", "artifactId", "requestId"],
  "audio.queue": ["v", "type", "artifactId", "requestId", "item"],
  "audio.geotrigger": ["v", "type", "artifactId", "requestId", "items"],
  "audio.stop": ["v", "type", "artifactId"],
  "notify.show": ["v", "type", "artifactId", "requestId", "title", "body"],
  "played.ack": ["v", "type", "artifactId", "ids"],
  "media.session": ["v", "type", "artifactId", "state", "title", "artist", "album", "actions", "rate"],
};
const QUEUE_ITEM = ["id", "title", "text", "url", "voice"];
const TRIGGER_ITEM = [...QUEUE_ITEM, "lat", "lon", "radius_m"];
const subset = (obj, allowed) => Object.keys(obj).every(k => allowed.includes(k));

test("every host message matches the app's field allowlist", async () => {
  const h = await readyHarness();
  await h.send({ type: "geo:watch", requestId: "w1", intervalMs: 20000 });
  await h.send({ type: "geo:unwatch", requestId: "w1" });
  await h.send({ type: "audio:queue", requestId: "q1", item: { id: "s-2", title: "Luxor", text: "Glass pyramid.", voice: "andrea" } });
  await h.send({ type: "audio:geotrigger", requestId: "g1", items: [story] });
  await h.send({ type: "notify:show", requestId: "n1", title: "Hi", body: "There" });
  await h.send({ type: "audio:stop" });
  h.f.reply({ type: "audio.played", artifactId: "trip", id: "s-1", at: 1 });
  await h.send({ type: "native:hello" });
  h.broker.mirrorMedia({ state: "playing", title: "Chapter 1", artist: "Andrea", album: "Road trip", handlers: {} });
  const seen = new Set();
  for (const m of h.f.sent) {
    assert.ok(APP_FIELDS[m.type], "unknown host message " + m.type);
    assert.ok(subset(m, APP_FIELDS[m.type]), m.type + " has fields the app rejects: " + Object.keys(m));
    if (m.item) assert.ok(subset(m.item, QUEUE_ITEM));
    if (m.items) m.items.forEach(i => assert.ok(subset(i, TRIGGER_ITEM)));
    seen.add(m.type);
  }
  assert.deepEqual([...seen].sort(), Object.keys(APP_FIELDS).sort());
});

test("voices the app would reject are refused in the shell", async () => {
  const h = await readyHarness();
  await h.send({ type: "audio:queue", requestId: "q1", item: { id: "s-2", title: "Luxor", text: "x", voice: "qwen/custom voice" } });
  assert.deepEqual(h.messages.at(-1), { type: "native:error", requestId: "q1", reason: "bad_request" });
});

test("Listen state is mirrored to the app once per change and car buttons reach Listen", async () => {
  const h = await readyHarness();
  const calls = [];
  const handlers = { play: () => calls.push("play"), pause: () => calls.push("pause"), next: () => calls.push("next"), previous: () => calls.push("previous"), stop: () => calls.push("stop") };
  h.broker.mirrorMedia({ state: "playing", title: "T".repeat(300), artist: "Andrea", album: "Road trip", handlers });
  h.broker.mirrorMedia({ state: "playing", title: "T".repeat(300), artist: "Andrea", album: "Road trip", handlers });
  const sessions = h.f.sent.filter(m => m.type === "media.session");
  assert.equal(sessions.length, 1, "identical state is not resent");
  assert.deepEqual(sessions[0], { v: 1, artifactId: "trip", type: "media.session", state: "playing", title: "T".repeat(200), artist: "Andrea", album: "Road trip", actions: ["play", "pause", "next", "previous", "stop"] });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "pause" });
  h.f.reply({ type: "media.command", artifactId: "other", action: "next" });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "next" });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "rm -rf" });
  assert.deepEqual(calls, ["pause", "next"]);
  h.broker.mirrorMedia({ state: "none" });
  assert.deepEqual(h.f.sent.at(-1), { v: 1, artifactId: "trip", type: "media.session", state: "none", title: "", artist: "", album: "", actions: [] });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "play" });
  assert.deepEqual(calls, ["pause", "next"], "no handlers after Listen stops");
});

test("hosts without media.mirror get no session messages", async () => {
  const f = fakeHost();
  const h = harness({ host: f.host });
  f.reply({ type: "ready", app: "0.1.0", capabilities: ["geo.watch"] });
  await settle();
  h.broker.mirrorMedia({ state: "playing", title: "x", handlers: {} });
  assert.equal(f.sent.some(m => m.type === "media.session"), false);
  assert.equal(await h.broker.hostHas("media.mirror"), false);
});

test("speed is mirrored only to apps advertising media.rate; rewind and speed buttons reach Listen", async () => {
  const old = await readyHarness();
  old.broker.mirrorMedia({ state: "playing", title: "Ch 1", artist: "Andrea", album: "Trip", rate: "1.25", handlers: {} });
  assert.equal("rate" in old.f.sent.filter(m => m.type === "media.session").at(-1), false, "v0.2 apps never see rate");

  const f = fakeHost();
  const h = harness({ host: f.host });
  f.reply({ type: "ready", app: "0.3.0", capabilities: ["media.mirror", "media.rate"] });
  await settle();
  const calls = [];
  h.broker.mirrorMedia({ state: "playing", title: "Ch 1", artist: "Andrea", album: "Trip", rate: "1.25", handlers: { rewind: () => calls.push("rewind"), speed: () => calls.push("speed") } });
  assert.equal(f.sent.filter(m => m.type === "media.session").at(-1).rate, "1.25");
  assert.deepEqual(f.sent.filter(m => m.type === "media.session").at(-1).actions, ["play", "pause", "next", "previous", "stop", "rewind", "speed"]);
  assert.deepEqual(old.f.sent.filter(m => m.type === "media.session").at(-1).actions, ["play", "pause", "next", "previous", "stop"], "v0.2 apps keep the original five");
  h.broker.mirrorMedia({ state: "playing", title: "Ch 1", artist: "Andrea", album: "Trip", rate: "fast; drop", handlers: {} });
  assert.equal("rate" in f.sent.filter(m => m.type === "media.session").at(-1), false, "malformed rate dropped");
  h.broker.mirrorMedia({ state: "playing", title: "Ch 1", artist: "Andrea", album: "Trip", rate: "1.5", handlers: { rewind: () => calls.push("rewind"), speed: () => calls.push("speed") } });
  f.reply({ type: "media.command", artifactId: "trip", action: "rewind" });
  f.reply({ type: "media.command", artifactId: "trip", action: "speed" });
  assert.deepEqual(calls, ["rewind", "speed"]);
});

// ADR-0022: fields and messages that only apps advertising media.progress accept.
const PROGRESS_FIELDS = {
  ...APP_FIELDS,
  "media.session": [...APP_FIELDS["media.session"], "position", "duration", "chapterIndex", "error"],
  "media.chapters": ["v", "type", "artifactId", "chapters"],
};

async function progressHarness(capabilities = ["media.mirror", "media.rate", "media.progress"]) {
  const f = fakeHost();
  let clock = 1000;
  const messages = [], frame = {};
  const broker = createBroker({
    enabled: true, artifactId: "trip", origin: "https://artifacts.example", host: () => f.host, frame: () => frame,
    storage: { getItem: () => null, setItem: () => {}, removeItem: () => {} }, post: m => messages.push(m), setTimer: () => {}, ask: () => "deny",
    now: () => clock,
  });
  broker.start();
  f.reply({ type: "ready", app: "0.4.0", capabilities });
  await settle();
  return { broker, f, sessions: () => f.sent.filter(m => m.type === "media.session"), tick: ms => { clock += ms; } };
}

test("apps without media.progress see no progress fields, buffering as playing, and no chapter list", async () => {
  const old = await readyHarness();
  old.broker.mirrorChapters(["One", "Two"]);
  old.broker.mirrorMedia({ state: "buffering", title: "Ch 1", artist: "Andrea", album: "Trip", position: 12.34, duration: 99, chapterIndex: 1, error: "No connection", handlers: {} });
  const sent = old.f.sent.filter(m => m.type === "media.session").at(-1);
  assert.deepEqual(sent, { v: 1, artifactId: "trip", type: "media.session", state: "playing", title: "Ch 1", artist: "Andrea", album: "Trip", actions: ["play", "pause", "next", "previous", "stop"] });
  assert.equal(old.f.sent.some(m => m.type === "media.chapters"), false);
  old.broker.mirrorMedia({ state: "none", error: "No connection" });
  assert.deepEqual(old.f.sent.at(-1), { v: 1, artifactId: "trip", type: "media.session", state: "none", title: "", artist: "", album: "", actions: [] });
  const rateOnly = await progressHarness(["media.mirror", "media.rate"]);
  rateOnly.broker.mirrorMedia({ state: "paused", title: "Ch 1", rate: "1.75", position: 3, error: "x", handlers: {} });
  assert.deepEqual(Object.keys(rateOnly.sessions().at(-1)).sort(), ["actions", "album", "artifactId", "artist", "rate", "state", "title", "type", "v"]);
});

test("media.progress hosts get chapters, position, buffering, errors and new actions", async () => {
  const h = await progressHarness();
  h.broker.mirrorChapters(["Prologue", "", "x".repeat(300)]);
  assert.deepEqual(h.f.sent.at(-1), { v: 1, artifactId: "trip", type: "media.chapters", chapters: ["Prologue", "Chapter 2", "x".repeat(200)] });
  h.broker.mirrorChapters(["Prologue", "", "x".repeat(300)]);
  assert.equal(h.f.sent.filter(m => m.type === "media.chapters").length, 1, "an unchanged list is not resent");
  h.broker.mirrorMedia({ state: "buffering", title: "Prologue", artist: "Andrea", album: "Book", rate: "1.75", position: 12.345, duration: 100000, chapterIndex: 0, error: "ignored while buffering", handlers: {} });
  assert.deepEqual(h.sessions().at(-1), { v: 1, artifactId: "trip", type: "media.session", state: "buffering", title: "Prologue", artist: "Andrea", album: "Book", actions: ["play", "pause", "next", "previous", "stop", "rewind", "speed", "forward", "chapter"], rate: "1.75", position: 12.3, duration: 86400, chapterIndex: 0 });
  assert.ok(h.sessions().at(-1).actions.length <= 12);
  h.broker.mirrorMedia({ state: "paused", title: "Prologue", position: 13, chapterIndex: 7, error: "  Speech is unavailable  ", handlers: {} });
  const paused = h.sessions().at(-1);
  assert.equal(paused.error, "Speech is unavailable");
  assert.equal("chapterIndex" in paused, false, "an index outside the last chapter list is dropped");
  h.broker.mirrorMedia({ state: "none", error: "E".repeat(200) });
  assert.deepEqual(h.sessions().at(-1), { v: 1, artifactId: "trip", type: "media.session", state: "none", title: "", artist: "", album: "", actions: [], error: "E".repeat(120) });
  h.broker.mirrorChapters([]);
  h.broker.mirrorMedia({ state: "playing", title: "T", position: 1, handlers: {} });
  assert.deepEqual(h.sessions().at(-1).actions.slice(-1), ["forward"], "no chapter action without chapters");
});

test("position-only changes are throttled to one update every 5 s", async () => {
  const h = await progressHarness();
  const at = position => h.broker.mirrorMedia({ state: "playing", title: "T", position, duration: 60, handlers: {} });
  at(1); at(2); h.tick(4000); at(3);
  assert.deepEqual(h.sessions().map(m => m.position), [1]);
  h.tick(1000); at(4);
  assert.deepEqual(h.sessions().map(m => m.position), [1, 4]);
  h.broker.mirrorMedia({ state: "paused", title: "T", position: 4.2, duration: 60, handlers: {} });
  assert.equal(h.sessions().at(-1).state, "paused", "state changes are sent at once");
  h.broker.mirrorMedia({ state: "paused", title: "Next chapter", position: 0, duration: 60, handlers: {} });
  assert.equal(h.sessions().at(-1).title, "Next chapter", "title changes are sent at once");
});

test("forward and chapter commands reach Listen only from media.progress apps", async () => {
  const calls = [];
  const handlers = { forward: () => calls.push("forward"), chapter: index => calls.push("chapter:" + index) };
  const old = await readyHarness();
  old.broker.mirrorChapters(["A", "B"]);
  old.broker.mirrorMedia({ state: "playing", title: "T", handlers });
  old.f.reply({ type: "media.command", artifactId: "trip", action: "forward" });
  old.f.reply({ type: "media.command", artifactId: "trip", action: "chapter", index: 1 });
  assert.deepEqual(calls, []);
  const h = await progressHarness();
  h.broker.mirrorChapters(["A", "B"]);
  h.broker.mirrorMedia({ state: "playing", title: "T", handlers });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "forward" });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "chapter", index: 1 });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "chapter", index: 2 });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "chapter", index: "1" });
  h.f.reply({ type: "media.command", artifactId: "trip", action: "chapter" });
  assert.deepEqual(calls, ["forward", "chapter:1"]);
});

test("chapters set before the app is ready are sent after ready, and every message fits the progress allowlist", async () => {
  const f = fakeHost();
  const frame = {};
  const broker = createBroker({ enabled: true, artifactId: "trip", origin: "https://artifacts.example", host: () => f.host, frame: () => frame,
    storage: { getItem: () => null, setItem: () => {}, removeItem: () => {} }, post: () => {}, setTimer: () => {}, ask: () => "deny" });
  broker.start();
  broker.mirrorChapters(["One"]);
  assert.equal(f.sent.some(m => m.type === "media.chapters"), false);
  f.reply({ type: "ready", app: "0.4.0", capabilities: ["media.mirror", "media.rate", "media.progress"] });
  await settle();
  assert.deepEqual(f.sent.at(-1), { v: 1, artifactId: "trip", type: "media.chapters", chapters: ["One"] });
  broker.mirrorMedia({ state: "paused", title: "One", position: 2, duration: 30, chapterIndex: 0, error: "No connection", rate: "2.5", handlers: {} });
  for (const m of f.sent) {
    assert.ok(PROGRESS_FIELDS[m.type], "unknown host message " + m.type);
    assert.ok(subset(m, PROGRESS_FIELDS[m.type]), m.type + " has fields the app rejects: " + Object.keys(m));
  }
});
