// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// ADR-0015: publish upsert by per-organization artifact_key.
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dataDir = mkdtempSync(join(tmpdir(), "artifact-mcp-key-"));
process.env.DATA_DIR = dataDir;
const { handleMcp, ARTIFACT_KEY_RE, ARTIFACT_KEY_INVALID } = await import("../lib/mcp.js");
const store = await import("../lib/store.js");
const { default: db, ARTIFACT_DIR } = await import("../lib/db.js");
const { createOrg } = await import("../lib/orgs.js");

test.after(() => rmSync(dataDir, { recursive: true, force: true }));

const author = { clientId: "key-author", org: "acme", label: "Author" };
const otherAuthor = { clientId: "key-other", org: "acme", label: "Other" };
const collaborator = { clientId: "key-collab", org: "acme", label: "Collab", role: "collaborator" };
const reader = { clientId: "key-reader", org: "acme", label: "Reader", role: "reader" };
const admin = { clientId: "key-admin", org: "admin", label: "Admin" };

let requestId = 0;
async function call(name, args, auth = author) {
  return handleMcp({ jsonrpc: "2.0", id: ++requestId, method: "tools/call", params: { name, arguments: args } }, auth);
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
const keyedCount = (org, key) => db.prepare("SELECT count(*) FROM artifacts WHERE org = ? AND artifact_key = ?").pluck().get(org, key);

test("artifact_key grammar matches the documented pattern", () => {
  for (const valid of ["a", "0", "proj/weekly-report", "a.b_c-d/e", "x".repeat(128)]) {
    assert.ok(ARTIFACT_KEY_RE.test(valid), valid);
  }
  for (const invalid of ["", "A", "-lead", ".lead", "/lead", "_lead", "has space", "Proj", "x".repeat(129), "tab\t", "nl\n", "é", "a\\b", "a:b"]) {
    assert.ok(!ARTIFACT_KEY_RE.test(invalid), JSON.stringify(invalid));
  }
});

test("invalid artifact_key is a JSON-RPC invalid-params error and creates nothing", async () => {
  const before = db.prepare("SELECT count(*) FROM artifacts").pluck().get();
  for (const key of ["Bad Key", "-x", "x".repeat(129), "nl\n"]) {
    for (const tool of ["publish_artifact", "publish_bundle"]) {
      const args = tool === "publish_artifact" ? { html: "<p>x</p>", artifact_key: key } : { files: { "index.html": "<p>x</p>" }, artifact_key: key };
      const response = await call(tool, args);
      assert.equal(response.error?.code, -32602, `${tool} ${JSON.stringify(key)}`);
      assert.equal(response.error.message, ARTIFACT_KEY_INVALID);
    }
  }
  const wrongType = await call("publish_artifact", { html: "<p>x</p>", artifact_key: 7 });
  assert.equal(wrongType.error?.code, -32602);
  assert.equal(db.prepare("SELECT count(*) FROM artifacts").pluck().get(), before);
});

test("keyed publish creates once, then updates in place with update_artifact semantics", async () => {
  const created = ok(await call("publish_artifact", {
    html: "<p>one</p>", title: "Report", description: "First", category: "Reports", artifact_key: "unit/report"
  }));
  assert.equal(created.created, true);
  assert.equal(created.revision, 1);
  assert.equal(created.artifact_key, "unit/report");
  assert.deepEqual(Object.keys(created), ["id", "url", "org", "bytes", "category", "revision", "artifact_key", "created"]);

  const updated = ok(await call("publish_artifact", { html: "<p>two!</p>", artifact_key: "unit/report" }));
  assert.equal(updated.created, false);
  assert.equal(updated.id, created.id);
  assert.equal(updated.url, created.url);
  assert.equal(updated.revision, 2);
  assert.equal(updated.bytes, Buffer.byteLength("<p>two!</p>"));
  assert.equal(updated.category, "Reports");

  const meta = store.getArtifactMeta(created.id);
  assert.equal(meta.title, "Report");
  assert.equal(meta.description, "First");
  assert.equal(meta.artifact_key, "unit/report");
  assert.equal(store.readArtifact(created.id).html, "<p>two!</p>");
  assert.equal(keyedCount("acme", "unit/report"), 1);

  // Supplied metadata replaces; an identical republish is a no-op that keeps the revision.
  const retitled = ok(await call("publish_artifact", { html: "<p>two!</p>", title: "Renamed", artifact_key: "unit/report" }));
  assert.equal(retitled.revision, 3);
  const noop = ok(await call("publish_artifact", { html: "<p>two!</p>", artifact_key: "unit/report" }));
  assert.equal(noop.revision, 3);
  assert.equal(noop.created, false);
  assert.equal(store.getArtifactMeta(created.id).title, "Renamed");
  assert.equal(store.listRevisions(created.id).revisions.length, 2);

  const read = ok(await call("read_artifact", { id: created.id }));
  assert.equal(read.artifact_key, "unit/report");
  const listed = ok(await call("list_artifacts", {}));
  assert.equal(listed.artifacts.find((row) => row.id === created.id).artifact_key, "unit/report");
});

test("unkeyed publish, list, and read output carries no artifact_key field", async () => {
  const plain = ok(await call("publish_artifact", { html: "<p>plain</p>" }));
  assert.deepEqual(Object.keys(plain), ["id", "url", "org", "bytes", "category"]);
  const read = ok(await call("read_artifact", { id: plain.id }));
  assert.equal(Object.hasOwn(read, "artifact_key"), false);
  const row = ok(await call("list_artifacts", {})).artifacts.find((entry) => entry.id === plain.id);
  assert.equal(Object.hasOwn(row, "artifact_key"), false);
  assert.equal(store.getArtifactMeta(plain.id).artifact_key, null);
});

test("keyed bundle publish replaces the whole snapshot and keeps the entry", async () => {
  const created = ok(await call("publish_bundle", {
    files: { "index.html": "<p>v1</p>", "a.css": "p{}" }, title: "Site", artifact_key: "unit/site"
  }));
  assert.equal(created.created, true);
  assert.equal(created.files, 2);
  const updated = ok(await call("publish_bundle", { files: { "index.html": "<p>v2</p>" }, artifact_key: "unit/site" }));
  assert.equal(updated.created, false);
  assert.equal(updated.id, created.id);
  assert.equal(updated.revision, 2);
  assert.equal(updated.files, 1);
  assert.equal(updated.entry, "index.html");
  assert.deepEqual(readdirSync(join(ARTIFACT_DIR, created.id)), ["index.html"]);
  assert.equal(store.getArtifactMeta(created.id).title, "Site");
  const empty = await call("publish_bundle", { files: {}, artifact_key: "unit/site" });
  assert.equal(toolError(empty), "files is empty");
  assert.equal(store.getArtifactMeta(created.id).revision, 2);
});

test("type mismatch between key and tool is refused without change", async () => {
  const single = ok(await call("publish_artifact", { html: "<p>s</p>", artifact_key: "unit/kind-single" }));
  const bundle = ok(await call("publish_bundle", { files: { "index.html": "<p>b</p>" }, artifact_key: "unit/kind-bundle" }));
  assert.equal(
    toolError(await call("publish_bundle", { files: { "index.html": "<p>x</p>" }, artifact_key: "unit/kind-single" })),
    'artifact_key "unit/kind-single" belongs to a single-file artifact; use publish_artifact'
  );
  assert.equal(
    toolError(await call("publish_artifact", { html: "<p>x</p>", artifact_key: "unit/kind-bundle" })),
    'artifact_key "unit/kind-bundle" belongs to a bundle artifact; use publish_bundle'
  );
  assert.equal(store.getArtifactMeta(single.id).revision, 1);
  assert.equal(store.getArtifactMeta(bundle.id).revision, 1);
});

test("keyed update follows the update_artifact policy for every role", async () => {
  const owned = ok(await call("publish_artifact", { html: "<p>own</p>", artifact_key: "unit/policy" }));
  // Another author in the same org: the same refusal update_artifact gives, no id disclosed.
  const denied = toolError(await call("publish_artifact", { html: "<p>no</p>", artifact_key: "unit/policy" }, otherAuthor));
  assert.equal(denied, "Permission denied: this API key cannot modify this artifact");
  assert.equal(denied.includes(owned.id), false);
  const deniedUpdate = toolError(await call("update_artifact", { id: owned.id, html: "<p>no</p>" }, otherAuthor));
  assert.equal(denied, deniedUpdate);
  // A reader cannot publish at all, keyed or not.
  assert.match(toolError(await call("publish_artifact", { html: "<p>no</p>", artifact_key: "unit/policy" }, reader)), /Permission denied/);
  assert.equal(store.getArtifactMeta(owned.id).revision, 1);
  // A collaborator may update any same-org artifact, exactly as update_artifact allows.
  const collab = ok(await call("publish_artifact", { html: "<p>collab</p>", artifact_key: "unit/policy" }, collaborator));
  assert.equal(collab.id, owned.id);
  assert.equal(collab.created, false);
  assert.equal(collab.revision, 2);
  // Keys are per organization; another org gets its own artifact.
  createOrg({ name: "keyorg" });
  const foreign = ok(await call("publish_artifact", { html: "<p>foreign</p>", artifact_key: "unit/policy" }, { clientId: "key-foreign", org: "keyorg" }));
  assert.equal(foreign.created, true);
  assert.notEqual(foreign.id, owned.id);
  // An admin key resolves the key inside its explicit target org.
  try { createOrg({ name: "acme" }); } catch {}
  const viaAdmin = ok(await call("publish_artifact", { html: "<p>admin</p>", org: "acme", artifact_key: "unit/policy" }, admin));
  assert.equal(viaAdmin.id, owned.id);
  assert.equal(viaAdmin.revision, 3);
});

test("deleting a keyed artifact frees the key", async () => {
  const first = ok(await call("publish_artifact", { html: "<p>1</p>", artifact_key: "unit/delete" }));
  assert.equal(ok(await call("delete_artifact", { id: first.id })).deleted, true);
  assert.equal(keyedCount("acme", "unit/delete"), 0);
  const second = ok(await call("publish_artifact", { html: "<p>2</p>", artifact_key: "unit/delete" }));
  assert.equal(second.created, true);
  assert.notEqual(second.id, first.id);
  assert.equal(second.revision, 1);
});

test("moving an artifact to another org clears its key", async () => {
  createOrg({ name: "keymove" });
  const moved = ok(await call("publish_artifact", { html: "<p>m</p>", artifact_key: "unit/move" }));
  ok(await call("publish_artifact", { html: "<p>m2</p>", artifact_key: "unit/move" }, { clientId: "mover", org: "keymove" }));
  // The destination already uses the key, so carrying it would violate the unique index.
  assert.equal(store.moveArtifactToOrg(moved.id, "keymove").ok, true);
  assert.equal(store.getArtifactMeta(moved.id).artifact_key, null);
  assert.equal(keyedCount("keymove", "unit/move"), 1);
  assert.equal(keyedCount("acme", "unit/move"), 0);
});

test("the unique index rejects a second live row and the store reports a typed conflict", () => {
  store.publish({ clientId: "key-author", org: "acme", html: "<p>a</p>", artifactKey: "unit/race" });
  const before = readdirSync(ARTIFACT_DIR).length;
  assert.throws(
    () => store.publish({ clientId: "key-author", org: "acme", html: "<p>b</p>", artifactKey: "unit/race" }),
    (error) => error.code === "artifact_key_taken"
  );
  assert.throws(
    () => store.publishBundle({ clientId: "key-author", org: "acme", files: { "index.html": "<p>b</p>" }, artifactKey: "unit/race" }),
    (error) => error.code === "artifact_key_taken"
  );
  assert.equal(keyedCount("acme", "unit/race"), 1);
  // No staged body or durability intent survives the refused insert.
  assert.equal(readdirSync(ARTIFACT_DIR).length, before);
  assert.equal(db.prepare("SELECT count(*) FROM artifact_durability_intents").pluck().get(), 0);
});

test("a concurrent in-flight create of the same key makes the loser fail cleanly", async () => {
  // Model another process's publish between metadata commit and body durability: the row holds
  // the key but stays invisible behind its durability intent.
  db.prepare(`INSERT INTO artifacts (id, client_id, org, title, description, bytes, is_bundle, entry, category, body_sha256, artifact_key)
              VALUES ('inflight0001', 'key-author', 'acme', 't', '', 1, 0, '', '', 'x', 'unit/inflight')`).run();
  db.prepare(`INSERT INTO artifact_durability_intents (id, artifact_id, operation, state, expected_sha256, prior_sha256, staging_path)
              VALUES ('publish:inflight0001', 'inflight0001', 'publish', 'metadata_committed', 'x', '', '.inflight0001.staging-abc')`).run();
  try {
    const before = db.prepare("SELECT count(*) FROM artifacts").pluck().get();
    const loser = await call("publish_artifact", { html: "<p>late</p>", artifact_key: "unit/inflight" });
    assert.equal(toolError(loser), "artifact_key is being published by another request; retry");
    assert.equal(db.prepare("SELECT count(*) FROM artifacts").pluck().get(), before);
    assert.equal(db.prepare("SELECT count(*) FROM artifact_durability_intents").pluck().get(), 1);
  } finally {
    db.prepare("DELETE FROM artifact_durability_intents WHERE id = 'publish:inflight0001'").run();
    db.prepare("DELETE FROM artifacts WHERE id = 'inflight0001'").run();
  }
  // Once the winner is visible, the same call updates it instead of creating a duplicate.
  const winner = ok(await call("publish_artifact", { html: "<p>w</p>", artifact_key: "unit/inflight" }));
  const again = ok(await call("publish_artifact", { html: "<p>w2</p>", artifact_key: "unit/inflight" }));
  assert.equal(again.id, winner.id);
  assert.equal(again.created, false);
});
