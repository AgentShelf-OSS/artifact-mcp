import test, { after } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import * as fs from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const importDataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-import-"));
process.env.DATA_DIR = importDataDir;
const { default: defaultDb, openDatabase } = await import("../lib/db.js");
const { createArtifactStore } = await import("../lib/store.js");

const sha256 = (value) => createHash("sha256").update(value, "utf8").digest("hex");
const bundleSha256 = (bundleFiles) => {
  const manifest = Object.entries(bundleFiles)
    .map(([rel, content]) => [rel, sha256(content)])
    .sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
  return sha256(JSON.stringify(manifest));
};

after(() => {
  defaultDb.close();
  rmSync(importDataDir, { recursive: true, force: true });
});

test("single-file publication exposes metadata and body without staging residue", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-single-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });

  try {
    const created = store.publish({
      clientId: "publisher",
      org: "acme",
      uploaderLabel: "Agent",
      html: "<!doctype html><h1>Hello</h1>",
      title: "Hello"
    });

    assert.ok(existsSync(path.join(runtime.artifactDir, `${created.id}.html`)));
    assert.equal(store.readArtifact(created.id).html, "<!doctype html><h1>Hello</h1>");
    assert.equal(store.getArtifactMeta(created.id).body_sha256, sha256("<!doctype html><h1>Hello</h1>"));
    assert.deepEqual(readdirSync(runtime.artifactDir).filter((name) => name.includes(".staging-")), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("hidden artifacts stay discoverable only to their recorded owner and administrators", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-visibility-"));
  const runtime = openDatabase({ dataDir });
  const ids = ["hidden1", "hidden2"];
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => ids.shift() });

  try {
    store.publish({ clientId: "publisher", org: "acme", ownerEmail: "owner@acme.test", html: "<h1>Hidden</h1>" });
    store.publish({ clientId: "publisher-2", org: "acme", ownerEmail: "other@acme.test", html: "<h1>Other</h1>" });
    store.setHidden("hidden1", true);
    store.setHidden("hidden2", true);
    assert.deepEqual(store.listOrgArtifacts("acme").map((row) => row.id), []);
    assert.deepEqual(store.listOrgIds("acme"), []);
    assert.deepEqual(store.listOrgArtifacts("acme", { ownerEmail: "owner@acme.test" }).map((row) => row.id), ["hidden1"]);
    assert.deepEqual(store.listOrgIds("acme", { ownerEmail: "owner@acme.test" }), ["hidden1"]);
    assert.deepEqual(store.listOrgArtifacts("acme", { includeHidden: true }).map((row) => row.id), ["hidden1", "hidden2"]);
    assert.deepEqual(store.listOrgIds("acme", { includeHidden: true }), ["hidden2", "hidden1"]);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("moving an artifact re-tenants every composite-FK child atomically", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-move-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({
    db: runtime.db,
    artifactDir: runtime.artifactDir,
    idFactory: () => "moved1",
    orgExists: (org) => org === "acme" || org === "beta"
  });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>One</h1>", category: "Reports" });
    store.update({ id: "moved1", clientId: "publisher", org: "acme", html: "<h1>Two</h1>" }); // creates a revision row
    runtime.db.prepare("INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision) VALUES (?, ?, ?, ?, ?, ?)")
      .run("feedback1", "moved1", "acme", "viewer@acme.test", "Looks good", 2);
    runtime.db.prepare("INSERT INTO artifact_views (artifact_id, org, email) VALUES (?, ?, ?)")
      .run("moved1", "acme", "viewer@acme.test");
    runtime.db.prepare("INSERT INTO artifact_shares (token, artifact_id, org, created_by) VALUES (?, ?, ?, ?)")
      .run("tok_move", "moved1", "acme", "viewer@acme.test");

    assert.deepEqual(store.moveArtifactToOrg("moved1", "beta"), { ok: true, id: "moved1", org: "beta", category: "Reports" });
    for (const table of ["artifacts", "feedback", "artifact_revisions", "artifact_views"]) {
      const idColumn = table === "artifacts" ? "id" : "artifact_id";
      assert.equal(runtime.db.prepare(`SELECT COUNT(*) AS n FROM ${table} WHERE ${idColumn} = ? AND org = 'acme'`).get("moved1").n, 0, `${table} has no old-org rows`);
      assert.ok(runtime.db.prepare(`SELECT COUNT(*) AS n FROM ${table} WHERE ${idColumn} = ? AND org = 'beta'`).get("moved1").n > 0, `${table} moved to beta`);
    }
    // An org move revokes existing public share links rather than carrying them over.
    assert.equal(runtime.db.prepare("SELECT COUNT(*) AS n FROM artifact_shares WHERE artifact_id = 'moved1'").get().n, 0, "shares dropped on move");
    assert.equal(runtime.db.pragma("foreign_key_check").length, 0);
    assert.throws(() => store.moveArtifactToOrg("moved1", "ghost"), /Unknown organization/);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("publisher artifact listing is org-scoped so an org move does not leak to the old key", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-leak-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({
    db: runtime.db,
    artifactDir: runtime.artifactDir,
    idFactory: () => "leak1",
    orgExists: (org) => org === "acme" || org === "beta"
  });
  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>One</h1>" });
    store.moveArtifactToOrg("leak1", "beta");
    // The move preserves client_id; the original-org key must NOT keep listing the artifact.
    assert.equal(store.listForClient("publisher", "acme").length, 0, "old-org key sees nothing");
    assert.equal(store.listForClient("publisher", "beta").length, 1, "new-org key sees it");
    assert.equal(store.listForClient("publisher").length, 1, "admin (no org scope) still sees it");
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("bundle publication exposes the selected entry and linked assets atomically", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-bundle-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });

  try {
    const bundleFiles = {
      "index.html": '<link rel="stylesheet" href="styles/site.css"><h1>Bundle</h1>',
      "styles/site.css": "h1{color:navy}"
    };
    const created = store.publishBundle({
      clientId: "publisher",
      org: "acme",
      files: bundleFiles,
      title: "Bundle"
    });

    assert.ok(existsSync(path.join(runtime.artifactDir, created.id, "index.html")));
    assert.equal(store.readBundleFile(created.id, "").content.toString(), '<link rel="stylesheet" href="styles/site.css"><h1>Bundle</h1>');
    assert.equal(store.readBundleFile(created.id, "styles/site.css").contentType, "text/css; charset=utf-8");
    assert.equal(store.getArtifactMeta(created.id).body_sha256, bundleSha256(bundleFiles));
    assert.deepEqual(store.auditStorage().divergentBodies, []);
    assert.deepEqual(readdirSync(runtime.artifactDir).filter((name) => name.includes(".staging-")), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("identical single-file and bundle updates do not create revisions or replacement bodies", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-noop-update-"));
  const runtime = openDatabase({ dataDir });
  const ids = ["noop01", "noop02"];
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => ids.shift() });

  try {
    const html = "<h1>Unchanged</h1>";
    const bundleFiles = { "index.html": "<h1>Index</h1>", "other.html": "<h1>Other</h1>" };
    store.publish({ clientId: "publisher", org: "acme", html, title: "Single", description: "Same", category: "Reports" });
    store.publishBundle({ clientId: "publisher", org: "acme", files: bundleFiles, entry: "index.html", title: "Bundle", description: "Same", category: "Reports" });

    const single = store.update({
      id: "noop01", clientId: "publisher", org: "acme", expectedRevision: 1,
      html, title: "Single", description: "Same", category: "Reports"
    });
    const bundle = store.update({
      id: "noop02", clientId: "publisher", org: "acme", expectedRevision: 1,
      files: bundleFiles, entry: "index.html", title: "Bundle", description: "Same", category: "Reports"
    });

    assert.equal(single.revision, 1);
    assert.equal(bundle.revision, 1);
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM artifact_revisions").pluck().get(), 2);
    assert.equal(store.listRevisions("noop01").revisions.length, 0);
    assert.equal(store.listRevisions("noop02").revisions.length, 0);
    assert.equal(existsSync(path.join(runtime.artifactDir, ".history")), false);
    assert.equal(store.readArtifact("noop01").html, html);
    assert.equal(store.readBundleFile("noop02", "other.html").content.toString(), bundleFiles["other.html"]);
    assert.deepEqual(readdirSync(runtime.artifactDir).filter((name) => name.includes(".staging-")), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("changing only a bundle entry creates a revision and preserves its files", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-entry-update-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "entry1" });

  try {
    const bundleFiles = { "index.html": "<h1>Index</h1>", "other.html": "<h1>Other</h1>" };
    store.publishBundle({ clientId: "publisher", org: "acme", files: bundleFiles, entry: "index.html" });

    const result = store.update({
      id: "entry1", clientId: "publisher", org: "acme", expectedRevision: 1, entry: "other.html"
    });

    assert.equal(result.ok, true);
    assert.equal(result.revision, 2);
    assert.equal(result.entry, "other.html");
    assert.equal(store.readBundleFile("entry1", "").content.toString(), bundleFiles["other.html"]);
    assert.equal(store.readBundleFile("entry1", "index.html").content.toString(), bundleFiles["index.html"]);
    assert.equal(store.readHistoryBundleFile("entry1", 1, "").content.toString(), bundleFiles["index.html"]);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("updates reject the wrong tenant and a stale expected revision", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-update-guard-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "guard1" });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>V1</h1>" });

    assert.deepEqual(
      store.update({ id: "guard1", clientId: "publisher", org: "beta", expectedRevision: 1, title: "Wrong tenant" }),
      { ok: false, reason: "forbidden" }
    );
    assert.deepEqual(
      store.update({ id: "guard1", clientId: "other", org: "acme", expectedRevision: 1, title: "Wrong owner" }),
      { ok: false, reason: "forbidden" }
    );

    const updated = store.update({
      id: "guard1", clientId: "publisher", org: "acme", expectedRevision: 1, html: "<h1>V2</h1>"
    });
    assert.equal(updated.revision, 2);
    assert.deepEqual(
      store.update({ id: "guard1", clientId: "publisher", org: "acme", expectedRevision: 1, html: "<h1>Stale</h1>" }),
      { ok: false, reason: "conflict" }
    );
    assert.equal(store.getArtifactMeta("guard1").revision, 2);
    assert.equal(store.readArtifact("guard1").html, "<h1>V2</h1>");
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("failed finalization compensates metadata and staged files", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-failure-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({
    db: runtime.db,
    artifactDir: runtime.artifactDir,
    idFactory: () => "fail123",
    files: { ...fs, renameSync: () => { throw new Error("simulated rename failure"); } }
  });

  try {
    assert.throws(
      () => store.publish({ clientId: "publisher", org: "acme", html: "<h1>Failure</h1>" }),
      /simulated rename failure/
    );
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM artifacts WHERE id = 'fail123'").pluck().get(), 0);
    assert.deepEqual(readdirSync(runtime.artifactDir), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("a failed in-place update reverts metadata and preserves the old body", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-update-fail-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "upd123" });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>V1</h1>", title: "V1" });

    // A second store over the same data whose rename always fails simulates a crash during
    // the body swap. Metadata is committed first, so the swap failure must compensate.
    const failing = createArtifactStore({
      db: runtime.db,
      artifactDir: runtime.artifactDir,
      files: { ...fs, renameSync: () => { throw new Error("simulated rename failure"); } }
    });
    assert.throws(
      () => failing.update({ id: "upd123", clientId: "publisher", org: "acme", html: "<h1>V2</h1>", title: "V2" }),
      /simulated rename failure/
    );

    const row = runtime.db.prepare("SELECT title, revision FROM artifacts WHERE id = 'upd123'").get();
    assert.equal(row.title, "V1");
    assert.equal(row.revision, 1);
    assert.equal(store.readArtifact("upd123").html, "<h1>V1</h1>");
    assert.deepEqual(readdirSync(runtime.artifactDir).filter((n) => n.includes(".staging-") || n.includes(".trash-")), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("updates capture history and a past revision can be restored", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-history-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "hist123" });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>V1</h1>", title: "One" });
    store.update({ id: "hist123", clientId: "publisher", org: "acme", html: "<h1>V2</h1>", title: "Two" });
    store.update({ id: "hist123", clientId: "publisher", org: "acme", html: "<h1>V3</h1>", title: "Three" });

    // Live is revision 3; history holds the outgoing revisions 1 and 2.
    const h = store.listRevisions("hist123");
    assert.equal(h.current, 3);
    assert.deepEqual(h.revisions.map((r) => r.revision), [2, 1]);
    assert.equal(store.readArtifact("hist123").html, "<h1>V3</h1>");
    assert.equal(store.readHistoryArtifact("hist123", 1).html, "<h1>V1</h1>");

    // Restore v1 -> becomes a NEW revision 4 with v1's content; history now has 1,2,3.
    const result = store.restore({ id: "hist123", clientId: "publisher", revision: 1 });
    assert.equal(result.ok, true);
    assert.equal(result.revision, 4);
    assert.equal(result.restoredFrom, 1);
    assert.equal(store.readArtifact("hist123").html, "<h1>V1</h1>");
    assert.deepEqual(store.listRevisions("hist123").revisions.map((r) => r.revision), [3, 2, 1]);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("history is capped at maxHistory and pruned oldest-first", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-hcap-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "cap123", maxHistory: 2 });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>r1</h1>" });
    for (let i = 2; i <= 6; i++) store.update({ id: "cap123", clientId: "publisher", org: "acme", html: `<h1>r${i}</h1>` });
    // Live is r6; only the newest 2 outgoing revisions (5, 4) are retained.
    const revs = store.listRevisions("cap123").revisions.map((r) => r.revision);
    assert.deepEqual(revs, [5, 4]);
    assert.equal(store.readHistoryArtifact("cap123", 3), null); // pruned
    assert.equal(store.readHistoryArtifact("cap123", 5).html, "<h1>r5</h1>");
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("deleting an artifact removes its history rows and bodies", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-hdel-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "del123" });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>a</h1>" });
    store.update({ id: "del123", clientId: "publisher", org: "acme", html: "<h1>b</h1>" });
    assert.equal(runtime.db.prepare("SELECT COUNT(*) c FROM artifact_revisions WHERE artifact_id='del123'").get().c, 2);

    assert.equal(store.deleteArtifactById("del123"), true);
    assert.equal(runtime.db.prepare("SELECT COUNT(*) c FROM artifact_revisions WHERE artifact_id='del123'").get().c, 0);
    assert.equal(existsSync(path.join(runtime.artifactDir, ".history", "del123")), false);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("audit recovers a committed-but-uninstalled staged body after a mid-update crash", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-auditswap-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "aud123" });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>V1</h1>" });
    // Simulate a crash after the metadata commit (bytes and digest now reflect V2) but before the body
    // swap: the V2 body is stranded in a staging file while the final still holds V1.
    const v2 = "<h1>V2 body is a different length</h1>";
    runtime.db.prepare("UPDATE artifacts SET bytes = ?, body_sha256 = ?, revision = 2 WHERE id = 'aud123'")
      .run(Buffer.byteLength(v2), sha256(v2));
    fs.writeFileSync(path.join(runtime.artifactDir, ".aud123.staging-crash"), v2);

    const report = store.auditStorage({ cleanTransient: true });
    assert.ok(report.recoveredPaths.includes(".aud123.staging-crash"));
    assert.equal(store.readArtifact("aud123").html, v2); // committed body is now served, not lost
    assert.deepEqual(readdirSync(runtime.artifactDir).filter((n) => n.includes(".staging-")), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("audit recovers a committed same-length body while preserving outgoing history", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-auditdigest-"));
  let runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "digest1" });

  try {
    const oldBody = "<h1>OLD</h1>";
    const newBody = "<h1>NEW</h1>";
    assert.equal(Buffer.byteLength(oldBody), Buffer.byteLength(newBody), "fixture bodies must have equal byte length");
    store.publish({ clientId: "publisher", org: "acme", html: oldBody, title: "Old" });

    // Reproduce the durable state after update() commits metadata and its outgoing revision
    // row, but before it moves the old body to history and installs the staged new body.
    runtime.db.exec(`
      INSERT OR REPLACE INTO artifact_revisions
        (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256)
      SELECT id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256
      FROM artifacts WHERE id = 'digest1';
    `);
    runtime.db.prepare(`
      UPDATE artifacts
      SET title = 'New', bytes = ?, body_sha256 = ?, revision = 2
      WHERE id = 'digest1'
    `).run(Buffer.byteLength(newBody), sha256(newBody));
    fs.writeFileSync(path.join(runtime.artifactDir, ".digest1.staging-crash"), newBody);

    runtime.db.close();
    runtime = openDatabase({ dataDir });
    const recovered = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });
    const report = recovered.auditStorage({ cleanTransient: true });

    assert.ok(report.recoveredPaths.includes(".digest1.staging-crash"));
    assert.equal(recovered.readArtifact("digest1").html, newBody);
    const outgoing = recovered.readHistoryArtifact("digest1", 1);
    assert.equal(outgoing.html, oldBody, "recovery snapshots revision 1 before installing revision 2");
    assert.equal(outgoing.meta.body_sha256, sha256(oldBody));

    const restored = recovered.restore({ id: "digest1", revision: 1, clientId: "publisher" });
    assert.equal(restored.ok, true);
    assert.equal(restored.revision, 3);
    assert.equal(restored.restoredFrom, 1);
    assert.equal(recovered.readArtifact("digest1").html, oldBody);

    const recoveredHistory = recovered.readHistoryArtifact("digest1", 2);
    assert.equal(recoveredHistory.html, newBody, "restore snapshots the recovered revision 2 body");
    assert.equal(recoveredHistory.meta.body_sha256, sha256(newBody));
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("audit preserves and reports zero-length staging without replacing an intact final body", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-audittorn-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "torn123" });

  try {
    const oldBody = "<h1>OLD</h1>";
    const newBody = "<h1>NEW</h1>";
    store.publish({ clientId: "publisher", org: "acme", html: oldBody, title: "Old" });
    runtime.db.exec(`
      INSERT OR REPLACE INTO artifact_revisions
        (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256)
      SELECT id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256
      FROM artifacts WHERE id = 'torn123';
    `);
    runtime.db.prepare(`
      UPDATE artifacts
      SET title = 'New', bytes = ?, body_sha256 = ?, revision = 2
      WHERE id = 'torn123'
    `).run(Buffer.byteLength(newBody), sha256(newBody));

    const stagingName = ".torn123.staging-crash";
    const stagingPath = path.join(runtime.artifactDir, stagingName);
    fs.writeFileSync(stagingPath, "");

    const report = store.auditStorage({ cleanTransient: true });

    assert.equal(store.readArtifact("torn123").html, oldBody);
    assert.equal(fs.readFileSync(stagingPath, "utf8"), "", "zero-length staging is preserved");
    assert.deepEqual(report.transientPaths, [stagingName]);
    assert.deepEqual(report.recoveredPaths, []);
    assert.deepEqual(report.divergentBodies, ["torn123"]);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("audit reclaims history bodies for artifacts that no longer exist", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-orphanhist-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "real99" });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>a</h1>" });
    store.update({ id: "real99", clientId: "publisher", org: "acme", html: "<h1>b</h1>" }); // creates .history/real99
    // Orphaned history for a since-deleted artifact (crash between DB delete and removeHistory).
    const ghost = path.join(runtime.artifactDir, ".history", "ghost77");
    fs.mkdirSync(ghost, { recursive: true });
    fs.writeFileSync(path.join(ghost, "1.html"), "leaked");

    const report = store.auditStorage({ cleanTransient: true });
    assert.ok(report.orphanHistory.includes("ghost77"));
    assert.equal(existsSync(ghost), false);
    assert.equal(existsSync(path.join(runtime.artifactDir, ".history", "real99")), true); // live one kept
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("only admin keys may delete an artifact owned by a different key", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-admin-delete-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "own123" });

  try {
    store.publish({ clientId: "owner", org: "acme", html: "<h1>Owned</h1>" });

    assert.equal(store.remove({ id: "own123", clientId: "intruder" }), false);
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM artifacts WHERE id = 'own123'").pluck().get(), 1);

    assert.equal(store.remove({ id: "own123", clientId: "intruder", isAdmin: true }), true);
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM artifacts WHERE id = 'own123'").pluck().get(), 0);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("artifact deletion removes bodies, metadata, and reactions together", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-delete-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });

  try {
    const created = store.publish({ clientId: "publisher", org: "acme", html: "<h1>Delete me</h1>" });
    runtime.db.prepare("INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES (?, ?, 1, 1)")
      .run("viewer@example.com", created.id);

    assert.equal(store.deleteArtifactById(created.id), true);
    assert.equal(existsSync(path.join(runtime.artifactDir, `${created.id}.html`)), false);
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM artifacts WHERE id = ?").pluck().get(created.id), 0);
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM reactions WHERE artifact_id = ?").pluck().get(created.id), 0);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("failed metadata deletion restores the artifact body", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-delete-failure-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });

  try {
    const created = store.publish({ clientId: "publisher", org: "acme", html: "<h1>Keep me</h1>" });
    runtime.db.exec("CREATE TRIGGER block_artifact_delete BEFORE DELETE ON artifacts BEGIN SELECT RAISE(ABORT, 'blocked'); END");

    assert.throws(() => store.deleteArtifactById(created.id), /blocked/);
    assert.ok(existsSync(path.join(runtime.artifactDir, `${created.id}.html`)));
    assert.equal(runtime.db.prepare("SELECT COUNT(*) FROM artifacts WHERE id = ?").pluck().get(created.id), 1);
    assert.deepEqual(readdirSync(runtime.artifactDir).filter((name) => name.includes(".trash-")), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("storage audit reports divergence and cleans only transient paths", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-audit-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });

  try {
    runtime.db.prepare(`
      INSERT INTO artifacts (id, client_id, org, title, description, bytes, uploader_label, is_bundle, entry)
      VALUES ('missing1', 'publisher', 'acme', 'Missing', '', 0, '', 0, '')
    `).run();
    runtime.db.prepare(`
      INSERT INTO artifacts (id, client_id, org, title, description, bytes, uploader_label, is_bundle, entry, body_sha256)
      VALUES ('drift1', 'publisher', 'acme', 'Drifted', '', 9, '', 0, '', ?)
    `).run(sha256("committed"));
    fs.writeFileSync(path.join(runtime.artifactDir, "drift1.html"), "tampered!");
    fs.writeFileSync(path.join(runtime.artifactDir, "orphan1.html"), "<h1>Orphan</h1>");
    fs.writeFileSync(path.join(runtime.artifactDir, ".temp123.staging-dead"), "partial");

    const report = store.auditStorage({ cleanTransient: true });
    assert.deepEqual(report.missingBodies, ["missing1"]);
    assert.deepEqual(report.divergentBodies, ["drift1"]);
    assert.deepEqual(report.orphanBodies, ["orphan1.html"]);
    assert.deepEqual(report.transientPaths, [".temp123.staging-dead"]);
    assert.ok(existsSync(path.join(runtime.artifactDir, "orphan1.html")));
    assert.equal(existsSync(path.join(runtime.artifactDir, ".temp123.staging-dead")), false);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("storage audit recovers interrupted staging and trash moves for live metadata", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-recover-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir });

  try {
    const insert = runtime.db.prepare(`
      INSERT INTO artifacts (id, client_id, org, title, description, bytes, uploader_label, is_bundle, entry)
      VALUES (?, 'publisher', 'acme', 'Interrupted', '', 16, '', 0, '')
    `);
    insert.run("stage123");
    insert.run("trash123");
    fs.writeFileSync(path.join(runtime.artifactDir, ".stage123.staging-dead"), "<h1>Staged</h1>");
    fs.writeFileSync(path.join(runtime.artifactDir, ".trash123.trash-dead"), "<h1>Trashed</h1>");

    const report = store.auditStorage({ cleanTransient: true });

    assert.equal(store.readArtifact("stage123").html, "<h1>Staged</h1>");
    assert.equal(store.readArtifact("trash123").html, "<h1>Trashed</h1>");
    assert.deepEqual(report.missingBodies, []);
    assert.deepEqual(report.recoveredPaths.sort(), [".stage123.staging-dead", ".trash123.trash-dead"]);
    assert.deepEqual(readdirSync(runtime.artifactDir).filter((name) => name.startsWith(".")), []);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("backfillBodyDigests restores blank digests from disk without bumping revision or timestamp, idempotently", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-digest-backfill-"));
  const runtime = openDatabase({ dataDir });
  let n = 0;
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => `legacy${++n}` });

  try {
    const single = store.publish({ clientId: "publisher", org: "acme", html: "<h1>Legacy</h1>", title: "Legacy" });
    const bundle = store.publishBundle({
      clientId: "publisher",
      org: "acme",
      files: { "index.html": "<h1>Bundle</h1>", "a.css": "body{}" },
      entry: "index.html",
      title: "Bundle"
    });
    // Simulate rows created before body_sha256 existed: blank the digest on both the artifact and
    // its current revision, and bump updated_at to a sentinel so we can prove it is preserved.
    const blank = runtime.db.prepare("UPDATE artifacts SET body_sha256 = '', updated_at = '2000-01-01 00:00:00' WHERE id = ?");
    blank.run(single.id);
    blank.run(bundle.id);
    const before = runtime.db.prepare("SELECT revision, updated_at FROM artifacts WHERE id = ?").get(single.id);

    const first = store.backfillBodyDigests();
    assert.equal(first.updated, 2);

    const singleMeta = store.getArtifactMeta(single.id);
    assert.equal(singleMeta.body_sha256, sha256("<h1>Legacy</h1>"));
    assert.equal(store.getArtifactMeta(bundle.id).body_sha256, bundleSha256({ "index.html": "<h1>Bundle</h1>", "a.css": "body{}" }));
    // Revision and updated_at are untouched — a digest backfill is not a content mutation.
    assert.equal(singleMeta.revision, before.revision);
    assert.equal(singleMeta.updated_at, "2000-01-01 00:00:00");

    // Idempotent: a second pass finds nothing blank to fix.
    assert.equal(store.backfillBodyDigests().updated, 0);
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("backfillBodyDigests skips rows whose body is missing from disk", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-store-digest-missing-"));
  const runtime = openDatabase({ dataDir });
  const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => "goneid1" });

  try {
    store.publish({ clientId: "publisher", org: "acme", html: "<h1>Gone</h1>" });
    runtime.db.prepare("UPDATE artifacts SET body_sha256 = '' WHERE id = 'goneid1'").run();
    rmSync(path.join(runtime.artifactDir, "goneid1.html"), { force: true });

    const result = store.backfillBodyDigests();
    assert.equal(result.scanned, 1);
    assert.equal(result.updated, 0);
    assert.equal(store.getArtifactMeta("goneid1").body_sha256, "");
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
});
