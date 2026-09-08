import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const shell = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");
const start = shell.indexOf("  function createViewerStateBroker(options)");
const end = shell.indexOf("  // End viewer state broker.", start);
const createBroker = vm.runInNewContext(shell.slice(start, end) + "\ncreateViewerStateBroker", { TextEncoder });
const settle = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };

function harness({ enabled = true, entries = new Map(), viewerId = "", viewerName = "" } = {}) {
  let now = 0, timerId = 0;
  const timers = new Map(), calls = [], messages = [], frame = {};
  const storage = {
    getItem: key => entries.get(key) ?? null,
    setItem: (key, value) => entries.set(key, value),
    removeItem: key => entries.delete(key),
  };
  const broker = createBroker({ enabled, artifactId: "artifact", viewerId, viewerName, storage, frame: () => frame,
    post: message => messages.push(JSON.parse(JSON.stringify(message))),
    fetch: (url, init) => new Promise((resolve, reject) => calls.push({ url, init, resolve, reject })),
    setTimer: (callback, ms) => { const id = ++timerId; timers.set(id, { callback, due: now + ms }); return id; },
    clearTimer: id => timers.delete(id),
  });
  return {
    entries, calls, messages,
    send: (data, source = frame) => broker.handle({ data, source }),
    async advance(ms) { now += ms; for (const [id, timer] of timers) if (timer.due <= now) { timers.delete(id); timer.callback(); } await settle(); },
    async reply(index, status, body = {}) { calls[index].resolve({ status, ok: status < 400, json: async () => body }); await settle(); },
    async fail(index) { calls[index].reject(new Error("offline")); await settle(); },
    cache: (key, scope = "org") => JSON.parse(entries.get(scope === "viewer" ? "artifact-state:artifact:viewer:" + encodeURIComponent(viewerId) + ":" + key : "artifact-state:artifact:" + key) || "null"),
  };
}

test("viewer scope uses an isolated cache lane and echoes scope", async () => {
  const h = harness({ viewerId: "viewer-1", viewerName: "Viewer One" });
  h.send({ type: "state:set", scope: "viewer", key: "note", value: "private" });
  await h.advance(500);
  assert.equal(h.calls[0].url, "/artifact/state/note?scope=viewer");
  await h.reply(0, 200, { revision: 1 });
  assert.deepEqual(h.messages.at(-1), { type: "state:saved", scope: "viewer", key: "note", revision: 1 });
  assert.deepEqual(h.cache("note", "viewer"), { value: "private", revision: 1 });
  assert.equal(h.cache("note"), null);
});

function seed(value, revision) { return new Map([["artifact-state:artifact:note", JSON.stringify({ value, revision })]]); }

test("org and viewer mutations with the same key debounce and resolve independently", async () => {
  const h = harness({ viewerId: "9f2c1e0a4b7d3c55" });
  h.send({ type: "state:set", key: "note", value: "shared", ifRevision: 1 });
  h.send({ type: "state:set", scope: "viewer", key: "note", value: "private", ifRevision: 0 });
  await h.advance(500);
  assert.equal(h.calls.length, 2);
  await h.reply(1, 200, { revision: 1 });
  await h.reply(0, 409, { error: "conflict", value: "canonical shared", revision: 2 });
  assert.deepEqual(h.cache("note", "viewer"), { value: "private", revision: 1 });
  assert.deepEqual(h.cache("note"), { value: "canonical shared", revision: 2 });
  assert.deepEqual(h.messages, [
    { type: "state:saved", scope: "viewer", key: "note", revision: 1 },
    { type: "state:value", scope: "org", key: "note", value: "canonical shared", revision: 2, conflict: true },
    { type: "state:error", scope: "org", key: "note", reason: "conflict" },
  ]);
  h.send({ type: "state:delete", scope: "viewer", key: "note" });
  await h.reply(2, 204);
  assert.equal(h.cache("note", "viewer"), null);
  assert.equal(h.cache("note").value, "canonical shared");
  assert.deepEqual(h.messages.at(-1), { type: "state:saved", scope: "viewer", key: "note", revision: 0 });
});

test("another viewer on the same browser never reads or retries a private draft", async () => {
  const first = harness({ viewerId: "9f2c1e0a4b7d3c55" });
  first.send({ type: "state:set", scope: "viewer", key: "note", value: "private draft", ifRevision: 0 });
  await first.advance(500); await first.fail(0);
  assert.equal(first.entries.has("artifact-state:artifact:viewer:9f2c1e0a4b7d3c55:note"), true);
  const other = harness({ viewerId: "8e91af3205d2c700", entries: first.entries });
  other.send({ type: "state:get", scope: "viewer", key: "note" });
  assert.deepEqual(other.messages, []);
  await other.reply(0, 404); await other.advance(500);
  assert.equal(other.calls.length, 1);
  assert.deepEqual(other.messages, [{ type: "state:value", scope: "viewer", key: "note", value: null, revision: 0 }]);
  const reload = harness({ viewerId: "9f2c1e0a4b7d3c55", entries: first.entries });
  reload.send({ type: "state:get", scope: "viewer", key: "note" });
  assert.equal(reload.messages[0].value, "private draft");
  await reload.reply(0, 404); await reload.advance(500);
  await reload.reply(1, 200, { revision: 1 });
  assert.deepEqual(reload.cache("note", "viewer"), { value: "private draft", revision: 1 });
});

test("invalid scopes fail locally and echo the requested scope", () => {
  const h = harness();
  for (const type of ["state:get", "state:set", "state:delete"]) {
    h.send({ type, scope: "team", key: "note", value: "no" });
    assert.deepEqual(h.messages.at(-1), { type: "state:error", scope: "team", key: "note", reason: "bad_scope" });
  }
  assert.equal(h.calls.length, 0);
  assert.equal(h.entries.size, 0);
});

test("source validation and disabled mode never access the network or cache", () => {
  const h = harness({ enabled: false, entries: seed("secret", 1) });
  assert.equal(h.send({ type: "state:hello" }, {}), false);
  assert.equal(h.send([{ type: "state:hello" }]), false);
  assert.equal(h.send({ type: 3 }), false);
  h.send({ type: "state:hello" });
  for (const type of ["get", "set", "delete", "unexpected"]) h.send({ type: "state:" + type, key: "note", value: "no" });
  assert.deepEqual(h.messages[0], { type: "state:ready", enabled: false, scope: "org", keys: [] });
  assert.ok(h.messages.slice(1).every(m => m.reason === "disabled"));
  assert.equal(h.calls.length, 0);
  assert.deepEqual(h.cache("note"), { value: "secret", revision: 1 });
});

test("hello strips timestamps and writer identities from bridge metadata", async () => {
  const h = harness({ viewerId: "9f2c1e0a4b7d3c55", viewerName: "Neil" }); h.send({ type: "state:hello" });
  await h.reply(0, 200, { keys: [{ key: "note", revision: 2, updated_at: "now", updated_by: "private@example.test" }] });
  await h.reply(1, 200, { keys: [{ key: "diary", revision: 3, updated_by: "private@example.test" }] });
  assert.deepEqual(h.calls.map(call => call.url), ["/artifact/state", "/artifact/state?scope=viewer"]);
  assert.deepEqual(h.messages, [{ type: "state:ready", enabled: true, scope: "org", scopes: ["org", "viewer"], viewer: { id: "9f2c1e0a4b7d3c55", name: "Neil" }, keys: [{ key: "note", revision: 2 }], viewerKeys: [{ key: "diary", revision: 3 }] }]);
});

test("a failed private list preserves org metadata and reports the private scope", async () => {
  const h = harness({ viewerId: "9f2c1e0a4b7d3c55", viewerName: "Neil" });
  h.send({ type: "state:hello" });
  await h.reply(0, 200, { keys: [{ key: "shared", revision: 2 }] });
  await h.fail(1);
  assert.deepEqual(h.messages[0].keys, [{ key: "shared", revision: 2 }]);
  assert.deepEqual(h.messages[0].viewerKeys, []);
  assert.deepEqual(h.messages[1], { type: "state:error", scope: "viewer", key: "", reason: "network" });
});

test("sets debounce per key for 500ms and acknowledge only after the server", async () => {
  const h = harness();
  h.send({ type: "state:set", key: "__proto__", value: "first" });
  await h.advance(300);
  h.send({ type: "state:set", key: "__proto__", value: "last" });
  await h.advance(499); assert.equal(h.calls.length, 0); assert.equal(h.messages.length, 0);
  await h.advance(1); assert.equal(h.calls.length, 1);
  assert.deepEqual(JSON.parse(h.calls[0].init.body), { value: "last" });
  assert.equal(h.messages.length, 0);
  await h.reply(0, 200, { key: "__proto__", revision: 1 });
  assert.deepEqual(h.messages, [{ type: "state:saved", scope: "org", key: "__proto__", revision: 1 }]);
  assert.deepEqual(h.cache("__proto__"), { value: "last", revision: 1 });
});

test("a later set waits for both the debounce deadline and an in-flight write", async () => {
  const h = harness(); h.send({ type: "state:set", key: "note", value: 1 }); await h.advance(500);
  h.send({ type: "state:set", key: "note", value: 2 }); await h.advance(100);
  await h.reply(0, 200, { revision: 1 }); assert.equal(h.calls.length, 1);
  await h.advance(399); assert.equal(h.calls.length, 1);
  await h.advance(1); assert.equal(h.calls.length, 2);
  await h.reply(1, 200, { revision: 2 }); await h.advance(500);
  assert.equal(h.calls.length, 2); assert.deepEqual(h.cache("note"), { value: 2, revision: 2 });
});

test("cached reads refresh changed revisions and emit absence after server deletion", async () => {
  const h = harness({ entries: seed("old", 3) }); h.send({ type: "state:get", key: "note" });
  assert.deepEqual(h.messages[0], { type: "state:value", scope: "org", key: "note", value: "old", revision: 3 });
  await h.reply(0, 200, { value: "fresh", revision: 4, updated_by: "private@example.test" });
  assert.deepEqual(h.messages[1], { type: "state:value", scope: "org", key: "note", value: "fresh", revision: 4 });
  h.send({ type: "state:get", key: "note" }); await h.reply(1, 200, { value: "fresh", revision: 4 });
  assert.equal(h.messages.length, 3);
  h.send({ type: "state:get", key: "note" }); await h.reply(2, 404, { error: "Not found" });
  assert.deepEqual(h.messages.at(-1), { type: "state:value", scope: "org", key: "note", value: null, revision: 0 });
  assert.equal(h.cache("note"), null);
});

test("old reads cannot overwrite a newer acknowledged mutation", async () => {
  const h = harness(); h.send({ type: "state:get", key: "note" });
  h.send({ type: "state:set", key: "note", value: "new" }); await h.advance(500);
  await h.reply(1, 200, { revision: 2 }); await h.reply(0, 200, { value: "old", revision: 1 });
  assert.deepEqual(h.cache("note"), { value: "new", revision: 2 });
  assert.deepEqual(h.messages, [{ type: "state:saved", scope: "org", key: "note", revision: 2 }]);
});

test("conflicts refresh cache then return canonical value before the error", async () => {
  const h = harness({ entries: seed("old", 1) });
  h.send({ type: "state:set", key: "note", value: "mine", ifRevision: 1 }); await h.advance(500);
  await h.reply(0, 409, { error: "conflict", value: "theirs", revision: 2, updated_by: "private" });
  assert.deepEqual(h.messages, [
    { type: "state:value", scope: "org", key: "note", value: "theirs", revision: 2, conflict: true },
    { type: "state:error", scope: "org", key: "note", reason: "conflict" },
  ]);
  assert.deepEqual(h.cache("note"), { value: "theirs", revision: 2 });
});

test("network failure retains a draft through reload and retries after a get", async () => {
  const h = harness(); h.send({ type: "state:set", key: "note", value: "keep", ifRevision: 0 }); await h.advance(500);
  await h.fail(0); assert.equal(h.messages[0].reason, "network"); assert.equal(h.cache("note").draft.value, "keep");
  const reload = harness({ entries: h.entries }); reload.send({ type: "state:get", key: "note" });
  assert.equal(reload.messages[0].value, "keep"); await reload.reply(0, 404);
  await reload.advance(500); assert.equal(reload.calls.length, 2); await reload.reply(1, 200, { revision: 1 });
  assert.deepEqual(reload.cache("note"), { value: "keep", revision: 1 });
});

test("delete cancels debounced sets and serializes all mutations for the key", async () => {
  const h = harness(); h.send({ type: "state:set", key: "note", value: 1 });
  h.send({ type: "state:delete", key: "note" }); assert.equal(h.calls[0].init.method, "DELETE");
  h.send({ type: "state:set", key: "note", value: 2 }); await h.advance(500); assert.equal(h.calls.length, 1);
  await h.reply(0, 204); assert.equal(h.calls[1].init.method, "PUT");
  h.send({ type: "state:delete", key: "note" }); assert.equal(h.calls.length, 2);
  await h.reply(1, 200, { revision: 1 }); assert.equal(h.calls[2].init.method, "DELETE");
  await h.reply(2, 204); await h.advance(1000); assert.equal(h.calls.length, 3); assert.equal(h.cache("note"), null);
});

test("invalid JSON, keys, revisions and size fail locally; capacity errors stay in the protocol", async () => {
  const h = harness(); const cyclic = {}; cyclic.self = cyclic;
  for (const ifRevision of [null, NaN, Infinity, -1, 0.5, 2 ** 53, "1"]) h.send({ type: "state:set", key: "note", value: 1, ifRevision });
  for (const value of [cyclic, undefined, 1n]) h.send({ type: "state:set", key: "note", value });
  for (const key of ["", "bad key", "x".repeat(65), "ends\n", "ends\r", "ends\u2028"]) h.send({ type: "state:get", key });
  h.send({ type: "state:set", key: "note", value: "é".repeat(131072) });
  assert.equal(h.calls.length, 0); assert.ok(h.messages.every(m => m.type === "state:error"));
  assert.equal(h.messages.at(-1).reason, "too_large");
  h.send({ type: "state:set", key: "note", value: null }); await h.advance(500);
  await h.reply(0, 409, { error: "too_many_keys" });
  assert.deepEqual(h.messages.at(-1), { type: "state:error", scope: "org", key: "note", reason: "too_large" });
});
