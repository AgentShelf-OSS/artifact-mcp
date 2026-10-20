// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// ADR-0023: stale-base guard for full-content updates.
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dataDir = mkdtempSync(join(tmpdir(), "artifact-mcp-stale-"));
process.env.DATA_DIR = dataDir;
delete process.env.STALE_GUARD;
delete process.env.STALE_GUARD_MIN_SEGMENTS;
delete process.env.STALE_GUARD_MIN_SHARE;
const { segmentDigests, evaluateStaleBase, staleMessage } = await import("../lib/stale-guard.js");
const config = await import("../lib/config.js");
const { handleMcp } = await import("../lib/mcp.js");
const store = await import("../lib/store.js");
const { default: db, ARTIFACT_DIR } = await import("../lib/db.js");

test.after(() => rmSync(dataDir, { recursive: true, force: true }));

const vectors = JSON.parse(readFileSync(new URL("../conformance/stale-guard-vectors.json", import.meta.url), "utf8"));
const asFiles = (list) => list.map((file) => ({ path: file.path, body: Buffer.from(file.body, "utf8") }));
const caseNamed = (name) => {
  const found = vectors.cases.find((entry) => entry.name === name);
  assert.ok(found, name);
  return found;
};

// ---------------------------------------------------------------- shared vectors (parity)

test("segment digests reproduce every shared digest example", () => {
  assert.ok(vectors.digest_examples.length > 0);
  for (const example of vectors.digest_examples) {
    const digests = [...segmentDigests([{ path: example.path, body: Buffer.from(example.body, "utf8") }])].sort();
    assert.deepEqual(digests, example.digests, JSON.stringify(example.body));
  }
});

test("evaluateStaleBase reproduces every shared vector case exactly", () => {
  assert.equal(vectors.version, 1);
  assert.ok(vectors.cases.length >= 10);
  for (const entry of vectors.cases) {
    const history = new Map(Object.entries(entry.history).map(([rev, files]) => [Number(rev), asFiles(files)]));
    const result = evaluateStaleBase({
      newFiles: asFiles(entry.new),
      currentFiles: asFiles(entry.current),
      currentRevision: entry.current_revision,
      history,
      minSegments: vectors.min_segments,
      minShare: vectors.min_share
    });
    assert.deepEqual({
      stale: result.stale,
      base: result.base,
      newer_segments: result.newerSegments,
      lost_segments: result.lostSegments,
      message: result.message
    }, entry.expect, entry.name);
  }
});

test("stale message names one revision or a range", () => {
  assert.equal(staleMessage(3, 4), "Update is based on revision 3, but the artifact is at revision 4. It would discard changes from revision 4. Read the current revision and apply your change to it, use restore_artifact to roll back on purpose, or pass discard_newer: true.");
  assert.match(staleMessage(13, 22), /It would discard changes from revisions 14-22\./);
});

test("segmentation works on bytes, trims, and ignores segments under 12 bytes", () => {
  // Multi-byte UTF-8 counts by byte, not by character: four 3-byte characters are 12 bytes.
  assert.equal(segmentDigests([{ path: null, body: Buffer.from("€€€€") }]).size, 1);
  assert.equal(segmentDigests([{ path: null, body: Buffer.from("€€€") }]).size, 0);
  assert.equal(segmentDigests([{ path: null, body: Buffer.from(" \t\r\f12345678901\n") }]).size, 0);
  const plain = segmentDigests([{ path: null, body: Buffer.from("<p>hello world</p>") }]);
  const trimmed = segmentDigests([{ path: null, body: Buffer.from("  <p>hello world</p>\r\n") }]);
  assert.deepEqual([...trimmed], [...plain]);
  const bundled = segmentDigests([{ path: "a.html", body: Buffer.from("<p>hello world</p>") }]);
  assert.notDeepEqual([...bundled], [...plain]);
  assert.equal(evaluateStaleBase({ newFiles: [], currentFiles: [{ path: null, body: Buffer.from("<p>hello world</p>") }], currentRevision: 2 }).stale, false);
});

test("stale guard configuration falls back to defaults for invalid values", () => {
  assert.equal(config.STALE_GUARD, "warn");
  assert.equal(config.STALE_GUARD_MIN_SEGMENTS, 20);
  assert.equal(config.STALE_GUARD_MIN_SHARE, 0.1);
  assert.equal(config.staleGuardMode("enforce"), "enforce");
  assert.equal(config.staleGuardMode(" OFF "), "off");
  assert.equal(config.staleGuardMode("block"), "warn");
  assert.equal(config.staleGuardMode(undefined), "warn");
  assert.equal(config.staleGuardMinShare("0.5"), 0.5);
  assert.equal(config.staleGuardMinShare("1"), 1);
  for (const bad of ["0", "-0.1", "1.5", "abc", "", undefined, "NaN", "Infinity"]) {
    assert.equal(config.staleGuardMinShare(bad), 0.1, String(bad));
  }
});

// ---------------------------------------------------------------- MCP integration

const author = { clientId: "key-author", org: "acme", label: "Author" };
const otherAuthor = { clientId: "key-other", org: "acme", label: "Other" };
const WARN = { mode: "warn", minSegments: 20, minShare: 0.1 };
const ENFORCE = { mode: "enforce", minSegments: 20, minShare: 0.1 };
const OFF = { mode: "off", minSegments: 20, minShare: 0.1 };
const CONFLICT = "Artifact changed during update; fetch its current revision and retry";

let requestId = 0;
async function call(name, args, auth = author, staleGuard) {
  return handleMcp(
    { jsonrpc: "2.0", id: ++requestId, method: "tools/call", params: { name, arguments: args } },
    auth,
    staleGuard ? { staleGuard } : {}
  );
}
function ok(response) {
  assert.equal(response.error, undefined, JSON.stringify(response.error));
  assert.notEqual(response.result.isError, true, response.result?.content?.[0]?.text);
  return response.result.structuredContent;
}
function toolError(response) {
  assert.equal(response.result?.isError, true, JSON.stringify(response));
  return response.result.content[0].text;
}
function captureLog() {
  const lines = [];
  const original = console.warn;
  console.warn = (...parts) => {
    const line = parts.join(" ");
    if (line.includes("stale_base_detected")) lines.push(line);
    else original(...parts);
  };
  return { lines, restore: () => { console.warn = original; } };
}
async function withLog(fn) {
  const log = captureLog();
  try {
    return { value: await fn(), lines: log.lines };
  } finally {
    log.restore();
  }
}

// The "stale copy of r3, one newer revision" vector: r1..r3 retained, r4 current; the new body is
// an edit of r3. Builds the history through the public tools.
const STALE_R3 = caseNamed("stale copy of r3, one newer revision");
const singleRevs = [STALE_R3.history["1"], STALE_R3.history["2"], STALE_R3.history["3"], STALE_R3.current].map((files) => files[0].body);
const staleHtml = STALE_R3.new[0].body;
const STALE_TEXT = STALE_R3.expect.message;
const bundleCase = caseNamed("bundle with a stale file");
const bundleMap = (files) => Object.fromEntries(files.map((file) => [file.path, file.body]));

async function buildSingle({ key } = {}) {
  let id;
  for (const [index, html] of singleRevs.entries()) {
    if (key) {
      id = ok(await call("publish_artifact", { html, title: "Report", artifact_key: key })).id;
    } else if (index === 0) {
      id = ok(await call("publish_artifact", { html, title: "Report" })).id;
    } else {
      ok(await call("update_artifact", { id, html }));
    }
  }
  assert.equal(store.getArtifactMeta(id).revision, 4);
  return id;
}

async function buildBundle({ key } = {}) {
  // Revisions 1..4 where r1 matches the vector's old body and r4 its current body.
  const revs = [bundleCase.history["1"], bundleCase.history["3"], bundleCase.current];
  let id;
  for (const [index, files] of revs.entries()) {
    if (key) id = ok(await call("publish_bundle", { files: bundleMap(files), artifact_key: key })).id;
    else if (index === 0) id = ok(await call("publish_bundle", { files: bundleMap(files) })).id;
    else ok(await call("update_artifact", { id, files: bundleMap(files) }));
  }
  return id;
}

const snapshot = (id) => ({
  meta: store.getArtifactMeta(id),
  body: store.readRevisionBodyFiles(id).map((file) => [file.path, file.body.toString("hex")]),
  revisions: db.prepare("SELECT count(*) FROM artifact_revisions WHERE artifact_id = ?").pluck().get(id)
});

test("default mode is warn: a stale update_artifact writes and returns the warning", async () => {
  const id = await buildSingle();
  const { value, lines } = await withLog(() => call("update_artifact", { id, html: staleHtml }));
  const result = ok(value);
  assert.equal(result.revision, 5);
  assert.deepEqual(result.warnings, [STALE_TEXT]);
  assert.deepEqual(Object.keys(result), ["id", "url", "revision", "bytes", "entry", "category", "warnings"]);
  assert.deepEqual(JSON.parse(value.result.content[0].text), result);
  assert.equal(store.readArtifact(id).html, staleHtml);
  assert.deepEqual(lines, [`[artifact-mcp] stale_base_detected id=${id} base=3 current=4 newer=26 lost=26 mode=warn discard_newer=false`]);
});

test("non-stale updates have no warnings key and keep their exact result", async () => {
  const id = await buildSingle();
  const edited = singleRevs[3].replace("intro line 3 value", "intro line 3 edited value");
  const { value, lines } = await withLog(() => call("update_artifact", { id, html: edited }, author, ENFORCE));
  const result = ok(value);
  assert.deepEqual(Object.keys(result), ["id", "url", "revision", "bytes", "entry", "category"]);
  assert.ok(!value.result.content[0].text.includes("warnings"));
  assert.deepEqual(lines, []);
  // An unchanged body is also not stale.
  const same = ok(await call("update_artifact", { id, html: edited }, author, ENFORCE));
  assert.equal(same.revision, result.revision);
  assert.equal(Object.hasOwn(same, "warnings"), false);
});

test("enforce refuses a stale update with the fixed text and changes nothing", async () => {
  const id = await buildSingle();
  const before = snapshot(id);
  const { value, lines } = await withLog(() => call("update_artifact", { id, html: staleHtml, title: "Changed" }, author, ENFORCE));
  assert.equal(toolError(value), STALE_TEXT);
  assert.deepEqual(snapshot(id), before);
  assert.deepEqual(lines, [`[artifact-mcp] stale_base_detected id=${id} base=3 current=4 newer=26 lost=26 mode=enforce discard_newer=false`]);
});

test("off mode skips the guard entirely", async () => {
  const id = await buildSingle();
  const { value, lines } = await withLog(() => call("update_artifact", { id, html: staleHtml }, author, OFF));
  const result = ok(value);
  assert.equal(result.revision, 5);
  assert.equal(Object.hasOwn(result, "warnings"), false);
  assert.deepEqual(lines, []);
});

test("discard_newer: true lets a stale update through with no warning, and is logged", async () => {
  for (const mode of [WARN, ENFORCE]) {
    const id = await buildSingle();
    const { value, lines } = await withLog(() => call("update_artifact", { id, html: staleHtml, discard_newer: true }, author, mode));
    const result = ok(value);
    assert.equal(result.revision, 5);
    assert.equal(Object.hasOwn(result, "warnings"), false);
    assert.deepEqual(lines, [`[artifact-mcp] stale_base_detected id=${id} base=3 current=4 newer=26 lost=26 mode=${mode.mode} discard_newer=true`]);
  }
  // discard_newer: false behaves like omission.
  const id = await buildSingle();
  assert.equal(toolError(await call("update_artifact", { id, html: staleHtml, discard_newer: false }, author, ENFORCE)), STALE_TEXT);
});

test("a non-boolean discard_newer is an invalid-params error on all three tools", async () => {
  const id = await buildSingle();
  const before = snapshot(id);
  const attempts = [
    ["update_artifact", { id, html: staleHtml, discard_newer: "true" }],
    ["update_artifact", { id, html: staleHtml, discard_newer: 1 }],
    ["update_artifact", { id, title: "x", discard_newer: null }],
    ["publish_artifact", { html: "<p>fresh artifact body</p>", discard_newer: "yes" }],
    ["publish_bundle", { files: { "index.html": "<p>fresh bundle body</p>" }, discard_newer: 0 }]
  ];
  const artifacts = db.prepare("SELECT count(*) FROM artifacts").pluck().get();
  for (const [tool, args] of attempts) {
    const response = await call(tool, args, author, ENFORCE);
    assert.equal(response.error?.code, -32602, `${tool} ${JSON.stringify(args.discard_newer)}`);
    assert.match(response.error.message, /discard_newer must be a boolean/);
  }
  assert.deepEqual(snapshot(id), before);
  assert.equal(db.prepare("SELECT count(*) FROM artifacts").pluck().get(), artifacts);
});

test("discard_newer is accepted (and ignored) on a plain or creating publish", async () => {
  const single = ok(await call("publish_artifact", { html: "<p>plain create body</p>", discard_newer: true }));
  assert.equal(Object.hasOwn(single, "warnings"), false);
  const keyed = ok(await call("publish_bundle", { files: { "index.html": "<p>keyed create body</p>" }, artifact_key: "stale/create", discard_newer: false }));
  assert.equal(keyed.created, true);
  assert.equal(Object.hasOwn(keyed, "warnings"), false);
});

test("metadata-only and entry-only updates are not checked", async () => {
  const id = await buildSingle();
  const { value, lines } = await withLog(() => call("update_artifact", { id, title: "Renamed", category: "Ops" }, author, ENFORCE));
  const result = ok(value);
  assert.equal(result.revision, 5);
  assert.equal(Object.hasOwn(result, "warnings"), false);
  assert.deepEqual(lines, []);
});

test("patch_artifact and restore_artifact are unaffected", async () => {
  const id = await buildSingle();
  const { value: restored, lines } = await withLog(() => call("restore_artifact", { id, revision: 1 }, author, ENFORCE));
  const restoreResult = ok(restored);
  assert.equal(Object.hasOwn(restoreResult, "warnings"), false);
  assert.equal(store.readArtifact(id).html, singleRevs[0]);
  // Patching the restored r1 body back into an older shape is not checked either.
  const patched = ok(await call("patch_artifact", {
    id,
    expected_revision: store.getArtifactMeta(id).revision,
    edits: [{ find: "intro line 3 value", replace: "intro line 3 patched value" }]
  }, author, ENFORCE));
  assert.equal(Object.hasOwn(patched, "warnings"), false);
  assert.deepEqual(lines, []);
});

test("a mismatched expected_revision returns the existing conflict instead of the guard", async () => {
  const id = await buildSingle();
  const before = snapshot(id);
  const { value, lines } = await withLog(() => call("update_artifact", { id, html: staleHtml, expected_revision: 3 }, author, ENFORCE));
  assert.equal(toolError(value), CONFLICT);
  assert.deepEqual(lines, []);
  assert.deepEqual(snapshot(id), before);
  // A matching expected_revision still runs the guard.
  assert.equal(toolError(await call("update_artifact", { id, html: staleHtml, expected_revision: 4 }, author, ENFORCE)), STALE_TEXT);
});

test("an unauthorized caller gets the existing error, never the guard text", async () => {
  const id = await buildSingle();
  const before = snapshot(id);
  const existing = toolError(await call("update_artifact", { id, html: "<p>a body that is not stale</p>" }, otherAuthor, ENFORCE));
  const { value, lines } = await withLog(() => call("update_artifact", { id, html: staleHtml }, otherAuthor, ENFORCE));
  assert.equal(toolError(value), existing);
  assert.ok(!existing.includes("revision"));
  const keyed = await buildSingle({ key: "stale/owned" });
  assert.ok(keyed);
  const keyedError = toolError(await call("publish_artifact", { html: staleHtml, artifact_key: "stale/owned" }, otherAuthor, ENFORCE));
  assert.ok(!keyedError.includes("revision"), keyedError);
  assert.deepEqual(lines, []);
  assert.deepEqual(snapshot(id), before);
});

test("keyed publish_artifact detects a stale update of an existing artifact", async () => {
  const id = await buildSingle({ key: "stale/report" });
  const { value, lines } = await withLog(() => call("publish_artifact", { html: staleHtml, artifact_key: "stale/report" }));
  const result = ok(value);
  assert.equal(result.id, id);
  assert.equal(result.created, false);
  assert.equal(result.revision, 5);
  assert.deepEqual(result.warnings, [STALE_TEXT]);
  assert.deepEqual(Object.keys(result), ["id", "url", "org", "bytes", "category", "revision", "artifact_key", "created", "warnings"]);
  assert.equal(lines.length, 1);

  const id2 = await buildSingle({ key: "stale/report-enforced" });
  const before = snapshot(id2);
  assert.equal(toolError(await call("publish_artifact", { html: staleHtml, artifact_key: "stale/report-enforced" }, author, ENFORCE)), STALE_TEXT);
  assert.deepEqual(snapshot(id2), before);
  const forced = ok(await call("publish_artifact", { html: staleHtml, artifact_key: "stale/report-enforced", discard_newer: true }, author, ENFORCE));
  assert.equal(forced.revision, 5);
  assert.equal(Object.hasOwn(forced, "warnings"), false);

  // A non-stale keyed update keeps the exact ADR-0015 shape.
  const fresh = ok(await call("publish_artifact", { html: staleHtml.replace("edited", "re-edited"), artifact_key: "stale/report-enforced" }, author, ENFORCE));
  assert.deepEqual(Object.keys(fresh), ["id", "url", "org", "bytes", "category", "revision", "artifact_key", "created"]);
});

test("keyed publish_bundle and bundle update_artifact detect a stale bundle", async () => {
  const staleFiles = bundleMap(bundleCase.new);
  const expected = bundleCase.expect;
  // The test history has r1 (vector r1), r2 (vector r3), r3 (vector current): base 1, current 3.
  const message = staleMessage(1, 3);
  const keyedId = await buildBundle({ key: "stale/site" });
  const keyed = ok(await call("publish_bundle", { files: staleFiles, artifact_key: "stale/site" }));
  assert.equal(keyed.id, keyedId);
  assert.equal(keyed.created, false);
  assert.deepEqual(keyed.warnings, [message]);
  assert.deepEqual(Object.keys(keyed), ["id", "url", "org", "entry", "files", "bytes", "category", "revision", "artifact_key", "created", "warnings"]);
  assert.equal(expected.base, 1);

  const id = await buildBundle();
  const before = snapshot(id);
  const { value, lines } = await withLog(() => call("update_artifact", { id, files: staleFiles }, author, ENFORCE));
  assert.equal(toolError(value), message);
  assert.deepEqual(snapshot(id), before);
  assert.equal(lines.length, 1);
  assert.match(lines[0], new RegExp(`id=${id} base=1 current=3 newer=\\d+ lost=\\d+ mode=enforce discard_newer=false$`));
  // Equivalent normalized paths digest the same as the stored relative paths.
  const dotted = Object.fromEntries(Object.entries(staleFiles).map(([path, body]) => [`./${path}`, body]));
  assert.equal(toolError(await call("update_artifact", { id, files: dotted }, author, ENFORCE)), message);
});

test("retained revisions with missing bodies are skipped", async () => {
  const id = await buildSingle();
  // Remove r3's body: the best remaining base for an edit of r3 is then r4 (current) or r2.
  rmSync(join(ARTIFACT_DIR, ".history", id, "3.html"), { force: true });
  assert.equal(store.readRevisionBodyFiles(id, 3), null);
  const result = ok(await call("update_artifact", { id, html: staleHtml }, author, ENFORCE));
  assert.equal(result.revision, 5);
});
