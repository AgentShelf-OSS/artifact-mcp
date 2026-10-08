import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const shell = readFileSync(new URL("../assets/shell.js", import.meta.url), "utf8");
const start = shell.indexOf("  function createViewerReminderBroker(options)");
const end = shell.indexOf("  // End viewer reminder broker.", start);
assert.ok(start > 0 && end > start, "reminder broker is delimited in shell.js");
const createBroker = vm.runInNewContext(shell.slice(start, end) + "\ncreateViewerReminderBroker", {});
const settle = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); };
const plain = value => JSON.parse(JSON.stringify(value));
// Uncompressed P-256 point shape: 65 bytes starting with 0x04.
const VAPID_BYTES = Uint8Array.from({ length: 65 }, (_, i) => (i === 0 ? 4 : i));
const VAPID_KEY = Buffer.from(VAPID_BYTES).toString("base64url");

function harness({
  enabled = true,
  config = { enabled: true, vapid_public_key: VAPID_KEY },
  optedIn = false,
  supported = true,
  permission = "default",
  grant = "granted",
  needsInstall = false,
  routes = {},
  existingSubscription = null,
} = {}) {
  const calls = [], messages = [], statuses = [], prompts = [], events = [], frame = {};
  let currentPermission = permission;
  const table = {
    "GET /push/config": () => ({ status: 200, body: config }),
    "GET /artifact/push": () => ({ status: 200, body: { enabled: true, opted_in: optedIn, devices: optedIn ? 1 : 0 } }),
    "PUT /push/subscriptions": () => ({ status: 200, body: { id: "sub-1" } }),
    "PUT /artifact/push/optin": () => ({ status: 200, body: { opted_in: true } }),
    "DELETE /artifact/push/optin": () => ({ status: 200, body: { opted_in: false } }),
    ...routes,
  };
  const subscription = {
    options: { applicationServerKey: VAPID_BYTES.buffer },
    toJSON: () => ({ endpoint: "https://fcm.googleapis.com/fcm/send/abc", keys: { p256dh: "BPUB", auth: "AUTH" } }),
    unsubscribe: async () => { events.push("unsubscribe"); return true; },
  };
  const registration = {
    pushManager: {
      getSubscription: async () => existingSubscription,
      subscribe: async (opts) => { events.push(["subscribe", opts.userVisibleOnly, Array.from(opts.applicationServerKey)]); return subscription; },
    },
  };
  const platform = {
    supported: () => supported,
    permission: () => (supported ? currentPermission : "unsupported"),
    requestPermission: async () => { events.push("requestPermission"); currentPermission = grant; return grant; },
    register: async (url) => { events.push(["register", url]); return registration; },
    needsInstall: () => needsInstall,
    label: () => "iPhone",
    atob: (value) => Buffer.from(value, "base64").toString("binary"),
  };
  const broker = createBroker({
    enabled, artifactId: "artifact", frame: () => frame, platform,
    post: message => messages.push(plain(message)),
    onStatus: (status, busy) => statuses.push({ ...plain(status), busy }),
    showPrompt: (text, status) => prompts.push({ text, status: plain(status) }),
    fetch: async (url, init = {}) => {
      const method = init.method || "GET";
      calls.push({ url, method, headers: init.headers || {}, body: init.body === undefined ? undefined : JSON.parse(init.body) });
      events.push(`${method} ${url}`);
      const route = table[`${method} ${url}`];
      if (!route) throw new Error(`unexpected ${method} ${url}`);
      const result = route();
      if (result instanceof Error) throw result;
      return { status: result.status, ok: result.status < 400, json: async () => result.body ?? {} };
    },
  });
  return {
    broker, calls, messages, statuses, prompts, events, frame,
    send: (data, source = frame) => broker.handle({ data, source }),
  };
}

test("reminder messages from other sources or families are ignored", async () => {
  const h = harness();
  assert.equal(h.send({ type: "reminder:hello" }, {}), false);
  assert.equal(h.send({ type: "state:hello" }), false);
  assert.equal(h.send(["reminder:hello"]), false);
  assert.equal(h.send(null), false);
  await settle();
  assert.deepEqual(h.calls, []);
  assert.deepEqual(h.messages, []);
});

test("hello loads server config and opt-in, then sends reminder:ready", async () => {
  const h = harness({ optedIn: true, permission: "granted" });
  assert.equal(h.send({ type: "reminder:hello" }), true);
  await settle();
  assert.deepEqual(h.calls.map(c => `${c.method} ${c.url}`), ["GET /push/config", "GET /artifact/push"]);
  assert.deepEqual(h.messages, [{ type: "reminder:ready", enabled: true, optedIn: true, permission: "granted", needsInstall: false }]);
  h.send({ type: "reminder:hello" });
  await settle();
  assert.equal(h.calls.length, 2, "config is fetched once per page");
  assert.equal(h.messages.length, 2);
});

test("server-disabled config reports disabled without reading opt-in", async () => {
  const h = harness({ config: { enabled: false, vapid_public_key: null } });
  h.send({ type: "reminder:hello" });
  await settle();
  assert.deepEqual(h.calls.map(c => c.url), ["/push/config"]);
  assert.deepEqual(h.messages, [{ type: "reminder:ready", enabled: false, optedIn: false, permission: "default", needsInstall: false }]);
});

test("unsupported browsers report permission unsupported; iOS Safari outside Home Screen reports needsInstall", async () => {
  const unsupported = harness({ supported: false });
  unsupported.send({ type: "reminder:hello" });
  await settle();
  assert.deepEqual(unsupported.messages.at(-1), { type: "reminder:ready", enabled: false, optedIn: false, permission: "unsupported", needsInstall: false });
  const ios = harness({ supported: false, needsInstall: true });
  ios.send({ type: "reminder:hello" });
  await settle();
  assert.deepEqual(ios.messages.at(-1), { type: "reminder:ready", enabled: false, optedIn: false, permission: "unsupported", needsInstall: true });
});

test("non-viewer shells (history, shares) answer hello as disabled and refuse mutations without requests", async () => {
  const h = harness({ enabled: false });
  h.send({ type: "reminder:hello" });
  h.send({ type: "reminder:set", key: "diaper", delaySeconds: 7200, title: "Change", requestId: "r1" });
  h.send({ type: "reminder:clear", key: "diaper" });
  h.send({ type: "reminder:prompt", reason: "Please" });
  await settle();
  assert.deepEqual(h.calls, []);
  assert.deepEqual(h.prompts, []);
  assert.deepEqual(h.messages, [
    { type: "reminder:error", key: "diaper", reason: "disabled", requestId: "r1" },
    { type: "reminder:error", key: "diaper", reason: "disabled" },
    { type: "reminder:ready", enabled: false, optedIn: false, permission: "default", needsInstall: false },
  ]);
});

test("reminder:set maps to PUT with mutation header, JSON body, and echoes requestId", async () => {
  const h = harness({ routes: {
    "PUT /artifact/reminders/diaper?scope=org": () => ({ status: 200, body: { key: "diaper", scope: "org", fire_at: 1791500000000, revision: 2 } }),
    "PUT /artifact/reminders/feed?scope=viewer": () => ({ status: 200, body: { key: "feed", scope: "viewer", fire_at: 1791500000001, revision: 1 } }),
  } });
  h.send({ type: "reminder:set", key: "diaper", delaySeconds: 7200, title: "Diaper change due", body: "Two hours", requestId: "req-1" });
  h.send({ type: "reminder:set", key: "feed", scope: "viewer", fireAt: 1791500000001, title: "Feed" });
  await settle();
  assert.equal(h.calls[0].method, "PUT");
  assert.equal(h.calls[0].headers["x-artifact-mutation"], "1");
  assert.equal(h.calls[0].headers["content-type"], "application/json");
  assert.deepEqual(h.calls[0].body, { delay_seconds: 7200, title: "Diaper change due", body: "Two hours" });
  assert.deepEqual(h.calls[1].body, { fire_at: 1791500000001, title: "Feed" });
  assert.deepEqual(h.messages, [
    { type: "reminder:saved", key: "diaper", scope: "org", fireAt: 1791500000000, requestId: "req-1" },
    { type: "reminder:saved", key: "feed", scope: "viewer", fireAt: 1791500000001 },
  ]);
});

test("reminder:clear maps to DELETE and reports cleared on 204", async () => {
  const h = harness({ routes: { "DELETE /artifact/reminders/diaper?scope=org": () => ({ status: 204 }) } });
  h.send({ type: "reminder:clear", key: "diaper", requestId: "c1" });
  await settle();
  assert.equal(h.calls[0].method, "DELETE");
  assert.equal(h.calls[0].headers["x-artifact-mutation"], "1");
  assert.equal(h.calls[0].body, undefined);
  assert.deepEqual(h.messages, [{ type: "reminder:cleared", key: "diaper", scope: "org", requestId: "c1" }]);
});

test("local validation rejects bad keys, scopes, times, and text before any request", async () => {
  const h = harness();
  h.send({ type: "reminder:set", key: "bad key", delaySeconds: 60, title: "x" });
  h.send({ type: "reminder:set", delaySeconds: 60, title: "x" });
  h.send({ type: "reminder:set", key: "k", scope: "all", delaySeconds: 60, title: "x" });
  h.send({ type: "reminder:set", key: "k", title: "x" });
  h.send({ type: "reminder:set", key: "k", delaySeconds: 60, fireAt: 1, title: "x" });
  h.send({ type: "reminder:set", key: "k", delaySeconds: 1.5, title: "x" });
  h.send({ type: "reminder:set", key: "k", delaySeconds: 60, title: 7 });
  h.send({ type: "reminder:set", key: "k", delaySeconds: 60, title: "x", body: {} });
  h.send({ type: "reminder:set", key: "k", delaySeconds: 60, title: "x", requestId: "y".repeat(81) });
  await settle();
  assert.deepEqual(h.messages.slice(0, 8).map(m => m.reason), ["bad_key", "bad_key", "bad_scope", "bad_time", "bad_time", "bad_time", "bad_text", "bad_text"]);
  assert.equal(h.calls.length, 1, "only the final, valid message reaches the server");
  assert.equal("requestId" in h.messages.at(-1), false, "oversized request ids are not echoed");
});

test("server and network failures map to stable reasons", async () => {
  const cases = [
    [{ status: 400, body: { error: "bad_time" } }, "bad_time"],
    [{ status: 400, body: { error: "bad_text" } }, "bad_text"],
    [{ status: 409, body: { error: "reminder_limit" } }, "reminder_limit"],
    [{ status: 404, body: { error: "push_disabled" } }, "disabled"],
    [{ status: 404, body: { error: "not_found" } }, "forbidden"],
    [{ status: 403, body: {} }, "forbidden"],
    [{ status: 429, body: {} }, "rate_limited"],
    [{ status: 500, body: {} }, "network"],
    [new Error("offline"), "network"],
  ];
  for (const [result, reason] of cases) {
    const h = harness({ routes: { "PUT /artifact/reminders/k?scope=org": () => result } });
    h.send({ type: "reminder:set", key: "k", delaySeconds: 60, title: "x", requestId: "q" });
    await settle();
    assert.deepEqual(h.messages, [{ type: "reminder:error", key: "k", reason, requestId: "q" }], reason);
  }
});

test("turnOn requests permission first, registers /sw.js, subscribes with the VAPID key, and saves subscription then opt-in", async () => {
  const h = harness();
  h.send({ type: "reminder:hello" });
  await settle();
  h.events.length = 0;
  const status = await h.broker.turnOn();
  assert.equal(h.events[0], "requestPermission", "permission is the first await so the click gesture is kept");
  assert.deepEqual(h.events.slice(1), [
    ["register", "/sw.js"],
    ["subscribe", true, Array.from(VAPID_BYTES)],
    "PUT /push/subscriptions",
    "PUT /artifact/push/optin",
  ]);
  const save = h.calls.find(c => c.url === "/push/subscriptions");
  assert.deepEqual(save.body, { endpoint: "https://fcm.googleapis.com/fcm/send/abc", keys: { p256dh: "BPUB", auth: "AUTH" }, label: "iPhone" });
  assert.equal(save.headers["x-artifact-mutation"], "1");
  assert.deepEqual(h.calls.find(c => c.url === "/artifact/push/optin").body, {});
  assert.deepEqual(plain(status), { enabled: true, optedIn: true, permission: "granted", needsInstall: false });
  assert.deepEqual(h.messages.at(-1), { type: "reminder:status", enabled: true, optedIn: true, permission: "granted", needsInstall: false });
  assert.equal(h.statuses.at(-1).busy, false);
});

test("turnOn replaces a subscription made with a different VAPID key", async () => {
  const unsubscribed = [];
  const stale = { options: { applicationServerKey: new Uint8Array([4, 9, 9]).buffer }, unsubscribe: async () => { unsubscribed.push(true); return true; } };
  const h = harness({ existingSubscription: stale });
  await h.broker.load();
  await h.broker.turnOn();
  assert.equal(unsubscribed.length, 1);
  assert.ok(h.events.some(e => Array.isArray(e) && e[0] === "subscribe"));
});

test("turnOn stops when permission is denied and reports the new permission", async () => {
  const h = harness({ grant: "denied" });
  h.send({ type: "reminder:hello" });
  await settle();
  await assert.rejects(h.broker.turnOn(), error => error.reason === "permission_denied");
  assert.equal(h.calls.some(c => c.method !== "GET"), false);
  assert.deepEqual(h.messages.at(-1), { type: "reminder:status", enabled: true, optedIn: false, permission: "denied", needsInstall: false });
});

test("turnOn refuses when the feature is unavailable", async () => {
  const h = harness({ config: { enabled: false, vapid_public_key: null } });
  await h.broker.load();
  await assert.rejects(h.broker.turnOn(), error => error.reason === "disabled");
  assert.equal(h.events.includes("requestPermission"), false);
});

test("turnOff removes only the artifact opt-in and keeps the device subscription", async () => {
  const h = harness({ optedIn: true, permission: "granted" });
  h.send({ type: "reminder:hello" });
  await settle();
  await h.broker.turnOff();
  const mutations = h.calls.filter(c => c.method !== "GET");
  assert.deepEqual(mutations.map(c => `${c.method} ${c.url}`), ["DELETE /artifact/push/optin"]);
  assert.equal(mutations[0].headers["x-artifact-mutation"], "1");
  assert.deepEqual(h.messages.at(-1), { type: "reminder:status", enabled: true, optedIn: false, permission: "granted", needsInstall: false });
});

test("reminder:prompt passes plain, length-limited text and is suppressed when already on", async () => {
  const h = harness();
  const reason = "<img src=x onerror=alert(1)>\u0007" + "a".repeat(200);
  h.send({ type: "reminder:prompt", reason });
  await settle();
  assert.equal(h.prompts.length, 1);
  assert.equal(h.prompts[0].text, ("<img src=x onerror=alert(1)> " + "a".repeat(200)).slice(0, 120));
  assert.equal(Array.from(h.prompts[0].text).length, 120);
  h.send({ type: "reminder:prompt", reason: 42 });
  await settle();
  assert.equal(h.prompts[1].text, "");

  const on = harness({ optedIn: true, permission: "granted" });
  on.send({ type: "reminder:prompt", reason: "Turn on" });
  await settle();
  assert.deepEqual(on.prompts, []);

  const off = harness({ config: { enabled: false, vapid_public_key: null } });
  off.send({ type: "reminder:prompt", reason: "Turn on" });
  await settle();
  assert.deepEqual(off.prompts, []);
});
