import test from "node:test";
import assert from "node:assert/strict";
import { IncomingMessage, ServerResponse } from "node:http";
import { Duplex } from "node:stream";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createApp } from "../lib/app.js";

const html = '<!doctype html><h1>Artifact</h1><script>parent.postMessage({type:"state:hello"},"*");</script>';
const historical = '<!doctype html><h1>Earlier artifact</h1>';

function appForDelivery() {
  const metas = {
    single: { id: "single", org: "acme", title: "Artifact", is_bundle: false },
    bundle: { id: "bundle", org: "acme", title: "Bundle", is_bundle: true, entry: "index.html" },
  };
  const file = content => ({ content: Buffer.from(content), contentType: "text/html; charset=utf-8" });
  return createApp({
    artifacts: {
      getArtifactMeta: id => metas[id],
      readArtifact: () => ({ html }),
      readBundleFile: () => file(html),
      readHistoryArtifact: () => ({ html: historical }),
      readHistoryBundleFile: () => file(historical),
    },
    shares: { resolve: token => ({ artifact_id: token, org: "acme" }) },
    resolveViewer: async req => {
      assert.ok(!req.path.startsWith("/s/"), "public delivery must not resolve viewer identity");
      return { email: "viewer@acme.test", org: "acme", isAdmin: false };
    },
    pages: { notFound: () => "not found" },
  });
}

// Exercise Express delivery without opening a listening socket.
function get(app, url) {
  const socket = new Duplex({ read() {}, write(_chunk, _encoding, done) { done(); } });
  const req = new IncomingMessage(socket);
  req.method = "GET"; req.url = url; req.headers = {};
  const res = new ServerResponse(req);
  return new Promise((resolve, reject) => {
    res.end = function(chunk) {
      resolve({ status: res.statusCode, body: Buffer.from(chunk ?? "") });
      socket.destroy(); return res;
    };
    req.on("error", reject); req.push(null);
    app.handle(req, res, error => reject(error || new Error("unhandled delivery route")));
  });
}

for (const [path, expected] of [
  ["/raw/single", html],
  ["/raw/single?download", html],
  ["/raw/bundle/", html],
  ["/raw/single/rev/1", historical],
  ["/raw/bundle/rev/1/", historical],
  ["/s/single", html],
  ["/s/bundle/", html],
]) {
  test(`${path} preserves artifact bytes without a viewer-state responder`, async () => {
    const response = await get(appForDelivery(), path);
    assert.equal(response.status, 200);
    assert.deepEqual(response.body, Buffer.from(expected));
  });
}

function documentedExample() {
  const guide = readFileSync(new URL("../GETTING_STARTED.md", import.meta.url), "utf8");
  const section = guide.split("## Persisting state from an artifact")[1];
  const source = section.match(/<script>([\s\S]*?)<\/script>/)[1];
  const note = { value: "" }, status = { textContent: "Notes are temporary on this page." };
  const messages = [];
  let receive, timeout;
  const parent = { postMessage(message, origin) {
    assert.equal(origin, "*"); messages.push(JSON.parse(JSON.stringify(message)));
  } };
  vm.runInNewContext(source, {
    parent,
    document: { querySelector: selector => selector === "#note" ? note : status },
    addEventListener(type, handler) { assert.equal(type, "message"); receive = handler; },
    setTimeout(callback, delay) { assert.equal(delay, 1000); timeout = callback; },
  });
  assert.equal(typeof timeout, "function", "the example must set the fallback timeout");
  return { note, status, messages, timeout,
    receive: (data, source = parent) => receive({ data, source }),
    input(value) { note.value = value; note.oninput(); },
  };
}

test("the documented example stays temporary after one second without a shell, including a late ready", () => {
  const example = documentedExample();
  example.input("keep this note");
  example.timeout();
  example.receive({ type: "state:ready", enabled: true });
  example.input("still temporary");
  assert.deepEqual(example.messages, [{ type: "state:hello" }]);
  assert.equal(example.note.value, "still temporary");
  assert.equal(example.status.textContent, "Notes are temporary on this page.");
});

test("the documented example reads and saves when the shell answers before the timeout", () => {
  const example = documentedExample();
  example.receive({ type: "state:ready", enabled: true });
  assert.deepEqual(example.messages.at(-1), { type: "state:get", key: "note" });
  example.receive({ type: "state:value", key: "note", value: "shared", revision: 2 });
  assert.equal(example.note.value, "shared");
  example.timeout();
  example.input("edited");
  assert.deepEqual(example.messages.at(-1), { type: "state:set", key: "note", value: "edited", ifRevision: 2 });
  example.receive({ type: "state:saved", key: "note", revision: 3 });
  assert.equal(example.status.textContent, "Saved for the organization.");
});

test("the documented example ignores foreign windows and respects an explicitly disabled shell", () => {
  const example = documentedExample();
  example.receive({ type: "state:ready", enabled: true }, {});
  example.input("temporary");
  example.receive({ type: "state:ready", enabled: false });
  example.timeout();
  example.input("still temporary");
  assert.deepEqual(example.messages, [{ type: "state:hello" }]);
});
