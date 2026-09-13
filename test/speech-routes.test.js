import test from "node:test";
import assert from "node:assert/strict";
import { IncomingMessage, ServerResponse } from "node:http";
import { Duplex } from "node:stream";
import { createApp } from "../lib/app.js";

function invoke(app, method, path, { params = {}, body, headers = {} } = {}) {
  const url = path.replace(/:([a-z]+)/g, (_match, key) => encodeURIComponent(params[key]));
  const payload = body === undefined ? "" : JSON.stringify(body);
  const socket = new Duplex({ read() {}, write(_chunk, _encoding, callback) { callback(); } });
  const req = new IncomingMessage(socket);
  req.method = method.toUpperCase(); req.url = url;
  req.headers = { cookie: "test-session", "content-type": "application/json", "content-length": String(Buffer.byteLength(payload)), "x-artifact-mutation": "1", "sec-fetch-site": "same-origin", ...headers };
  const res = new ServerResponse(req);
  const writes = [];
  return new Promise((resolve, reject) => {
    res.write = function write(chunk) { if (chunk != null) writes.push(Buffer.from(chunk)); return true; };
    res.end = function end(chunk) { if (chunk != null) writes.push(Buffer.from(chunk)); const raw = Buffer.concat(writes); const text = raw.toString(); let value; try { value = JSON.parse(text); } catch { value = text || undefined; } resolve({ status: res.statusCode, body: value, raw, headers: res.getHeaders() }); socket.destroy(); return res; };
    req.on("error", reject); req.push(payload || null); if (payload) req.push(null);
    app.handle(req, res, (error) => reject(error || new Error("unhandled test route")));
  });
}

function fixture(viewer = { email: "viewer@acme.test", org: "acme", isAdmin: false }) {
  const metadata = new Map([
    ["bundle01", { id: "bundle01", org: "acme", title: "Bundle", is_bundle: 1 }],
    ["foreign1", { id: "foreign1", org: "beta", title: "Foreign", is_bundle: 0 }]
  ]);
  return createApp({
    resolveViewer: async () => viewer,
    artifacts: { getArtifactMeta: (id) => metadata.get(id) || null, readArtifact: () => null, readBundleFile: () => null, listOrgArtifacts: () => [], listAllGroupedByOrg: () => new Map(), listOrgIds: () => [] },
    orgs: { list: () => [], names: () => [], has: () => true, colorMap: () => ({}) },
    keys: { list: () => [] }, reactions: { get: () => ({ favorite: 0, vote: 0 }) }, feedback: { listForArtifact: () => [] },
    pages: { notFound: () => "not found", notSignedIn: () => "not signed in", gallery: () => "", shell: () => "", settings: () => "" },
    speech: {
      voices: [{ id: "af_heart", name: "Heart · American" }],
      synthesize: async () => ({ status: 200, audio: Buffer.from("RIFF") }),
      streamTimedSynthesize: async (_text, voice) => {
        if (!voice.startsWith("pocket_")) return { status: 400, error: "bad_voice" };
        const bytes = new TextEncoder().encode("timed");
        let sent = false;
        return { status: 200, stream: { getReader: () => ({ read: async () => sent ? { done: true } : (sent = true, { done: false, value: bytes }), cancel: async () => {} }) }, release() {} };
      }
    },
    logger: { info() {}, error() {} }
  });
}

test("speech routes authorize bundles, conceal foreign artifacts, and require portal mutation proof", async () => {
  const previous = process.env.TTS_ENABLED;
  process.env.TTS_ENABLED = "1";
  try {
    const app = fixture();
    let result = await invoke(app, "get", "/:id/speech/voices", { params: { id: "bundle01" } });
    assert.equal(result.status, 200);
    assert.equal(result.body.enabled, true);
    result = await invoke(app, "post", "/:id/speech", { params: { id: "bundle01" }, body: { text: "Read this bundle.", voice: "af_heart" } });
    assert.equal(result.status, 200);
    assert.equal(result.body, "RIFF");
    result = await invoke(app, "post", "/:id/speech", { params: { id: "foreign1" }, body: { text: "secret", voice: "af_heart" } });
    assert.equal(result.status, 404);
    result = await invoke(app, "post", "/:id/speech", { params: { id: "bundle01" }, body: { text: "blocked", voice: "af_heart" }, headers: { "x-artifact-mutation": undefined, "sec-fetch-site": "same-origin" } });
    assert.equal(result.status, 403);
  } finally {
    if (previous === undefined) delete process.env.TTS_ENABLED; else process.env.TTS_ENABLED = previous;
  }
});

test("timed speech route restricts voices and proxies timed PCM", async () => {
  const previous = process.env.TTS_ENABLED;
  process.env.TTS_ENABLED = "1";
  try {
    const app = fixture();
    let result = await invoke(app, "post", "/:id/speech/stream-timed", { params: { id: "bundle01" }, body: { text: "Read this.", voice: "af_heart" } });
    assert.equal(result.status, 400);
    result = await invoke(app, "post", "/:id/speech/stream-timed", { params: { id: "bundle01" }, body: { text: "Read this.", voice: "pocket_alba" } });
    assert.equal(result.status, 200);
    assert.equal(result.body, "timed");
    assert.equal(result.headers["content-type"], "application/vnd.artifact.pcm-timed");
  } finally {
    if (previous === undefined) delete process.env.TTS_ENABLED; else process.env.TTS_ENABLED = previous;
  }
});


test("reader DSP assets are exact public code routes with source and license", async () => {
  const app = fixture(null);
  const worklet = await invoke(app, "get", "/reader-audio/pitch-v1.js");
  assert.equal(worklet.status, 200);
  assert.match(worklet.headers["content-type"], /application\/javascript/);
  assert.equal(worklet.headers["x-content-type-options"], "nosniff");
  assert.equal(worklet.headers["cache-control"], "no-cache");
  assert.ok(worklet.body.includes("registerProcessor('artifact-pitch'"));
  const map = await invoke(app, "get", "/reader-audio/soundtouch-processor.js.map");
  assert.equal(map.status, 200);
  assert.ok(map.body.sourcesContent.every(source => typeof source === "string" && source.length));
  const license = await invoke(app, "get", "/reader-audio/LICENSE");
  assert.equal(license.status, 200);
  assert.match(license.body, /Mozilla Public License/);
});
