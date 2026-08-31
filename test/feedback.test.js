import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const importDataDir = mkdtempSync(path.join(tmpdir(), "artifact-feedback-import-"));
process.env.DATA_DIR = importDataDir;
const { default: defaultDb, openDatabase } = await import("../lib/db.js");
const { createFeedbackStore } = await import("../lib/feedback.js");

test.after(() => {
  defaultDb.close();
  rmSync(importDataDir, { recursive: true, force: true });
});

function withFeedbackStore(fn) {
  const dataDir = mkdtempSync(path.join(tmpdir(), "artifact-feedback-"));
  const runtime = openDatabase({ dataDir });
  const store = createFeedbackStore({ db: runtime.db });
  runtime.db.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES (?, ?, ?, ?)").run("artifact-a", "owner", "acme", "A");
  runtime.db.prepare("INSERT INTO artifacts (id, client_id, org, title) VALUES (?, ?, ?, ?)").run("artifact-b", "owner", "acme", "B");
  try {
    fn({ db: runtime.db, feedback: store });
  } finally {
    runtime.db.close();
    rmSync(dataDir, { recursive: true, force: true });
  }
}

test("feedback replies require a same-artifact top-level parent and inherit no anchor", () => {
  withFeedbackStore(({ feedback }) => {
    const parent = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Pinned", artifactRevision: 7,
      anchor: { path: "body", x: 0.25, y: 0.75 }
    });
    const reply = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "reply@acme.test", body: "Reply", artifactRevision: 7,
      parentId: parent.id, anchor: { path: "ignored", x: 0.5, y: 0.5 }
    });
    assert.deepEqual(
      { parent_id: reply.parent_id, anchor_path: reply.anchor_path, anchor_x: reply.anchor_x, anchor_y: reply.anchor_y, anchor_w: reply.anchor_w, anchor_h: reply.anchor_h, anchor_approx: reply.anchor_approx },
      { parent_id: parent.id, anchor_path: null, anchor_x: null, anchor_y: null, anchor_w: null, anchor_h: null, anchor_approx: 0 }
    );
    assert.equal(reply.org, "acme");
    assert.equal(reply.artifact_revision, 7);
    assert.throws(() => feedback.addFeedback({ artifactId: "artifact-a", org: "acme", viewerEmail: "x", body: "No parent", artifactRevision: 7, parentId: "missing" }), /parent not found/);
    const other = feedback.addFeedback({ artifactId: "artifact-b", org: "acme", viewerEmail: "x", body: "Other", artifactRevision: 1 });
    assert.throws(() => feedback.addFeedback({ artifactId: "artifact-a", org: "acme", viewerEmail: "x", body: "Cross artifact", artifactRevision: 7, parentId: other.id }), /different artifact/);
    assert.throws(() => feedback.addFeedback({ artifactId: "artifact-a", org: "acme", viewerEmail: "x", body: "Too deep", artifactRevision: 7, parentId: reply.id }), /top-level/);
    assert.equal(feedback.listForArtifact("artifact-a").find((row) => row.id === reply.id).parent_id, parent.id);
  });
});

test("feedback box anchors persist bounded geometry while points and replies retain null dimensions", () => {
  withFeedbackStore(({ feedback }) => {
    const box = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Entire chart", artifactRevision: 7,
      anchor: { path: "main>section", x: 0.75, y: 0.5, w: 0.5, h: 0.75 }
    });
    assert.deepEqual(
      { x: box.anchor_x, y: box.anchor_y, w: box.anchor_w, h: box.anchor_h },
      { x: 0.75, y: 0.5, w: 0.25, h: 0.5 }
    );
    const listed = feedback.listForArtifact("artifact-a").find((row) => row.id === box.id);
    assert.deepEqual({ x: listed.anchor_x, y: listed.anchor_y, w: listed.anchor_w, h: listed.anchor_h }, { x: 0.75, y: 0.5, w: 0.25, h: 0.5 });

    const point = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Point", artifactRevision: 7,
      anchor: { x: 0.25, y: 0.25 }
    });
    assert.deepEqual({ w: point.anchor_w, h: point.anchor_h }, { w: null, h: null });

    const reply = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "reply@acme.test", body: "Reply", artifactRevision: 7, parentId: box.id,
      anchor: { x: 0.1, y: 0.1, w: 0.2, h: 0.2 }
    });
    assert.deepEqual(
      { path: reply.anchor_path, x: reply.anchor_x, y: reply.anchor_y, w: reply.anchor_w, h: reply.anchor_h },
      { path: null, x: null, y: null, w: null, h: null }
    );

    for (const anchor of [
      { x: 0.1, y: 0.1, w: -0.01, h: 0.2 },
      { x: 0.1, y: 0.1, w: 0.2, h: 1.01 },
      { x: 1, y: 0.1, w: 0.2, h: 0.2 },
      { x: 0.1, y: 0.1, w: 0.2 }
    ]) {
      assert.throws(() => feedback.addFeedback({ artifactId: "artifact-a", org: "acme", viewerEmail: "x", body: "Invalid", artifactRevision: 7, anchor }), /Anchor w and h|Box anchor/);
    }
  });
});

test("feedback persists and lists anchor page identity while legacy and reply rows remain null", () => {
  withFeedbackStore(({ feedback }) => {
    const pinned = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Page two",
      artifactRevision: 7, anchorPage: "pages/two.html", anchor: { x: 0.25, y: 0.75 }
    });
    const legacy = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Legacy",
      artifactRevision: 7, anchor: { x: 0.1, y: 0.2 }
    });
    const reply = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Reply",
      artifactRevision: 7, parentId: pinned.id, anchorPage: "pages/two.html", anchor: { x: 0.2, y: 0.3 }
    });

    assert.equal(pinned.anchor_page, "pages/two.html");
    assert.equal(feedback.listForArtifact("artifact-a").find((row) => row.id === pinned.id).anchor_page, "pages/two.html");
    assert.equal(legacy.anchor_page, null);
    assert.equal(reply.anchor_page, null);
  });
});

test("feedback preserves a valid structured v2 anchor and rejects malformed v2 metadata", () => {
  withFeedbackStore(({ feedback }) => {
    const v2 = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Section",
      artifactRevision: 7,
      anchor: {
        version: 2, kind: "element", path: "main:nth-child(1)", nodeId: "revenue-table",
        quote: "Quarterly revenue", x: 0.25, y: 0.5, w: 0.25, h: 0.2, approx: false
      }
    });
    assert.deepEqual(
      {
        anchor_version: v2.anchor_version, anchor_kind: v2.anchor_kind, anchor_node_id: v2.anchor_node_id,
        anchor_quote: v2.anchor_quote, anchor_path: v2.anchor_path
      },
      {
        anchor_version: 2, anchor_kind: "element", anchor_node_id: "revenue-table",
        anchor_quote: "Quarterly revenue", anchor_path: "main:nth-child(1)"
      }
    );
    assert.equal(v2.artifact_revision, 7, "the anchor stays pinned to the exact revision that accepted it");
    const legacy = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Legacy",
      artifactRevision: 7, anchor: { path: "body", x: 0.1, y: 0.2 }
    });
    const plain = feedback.addFeedback({
      artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Plain", artifactRevision: 7
    });
    assert.equal(legacy.anchor_version, 1);
    assert.equal(plain.anchor_version, 0);
    for (const anchor of [
      { version: 1, kind: "element", path: "body", x: 0.1, y: 0.2 },
      { version: 2, kind: "unknown", path: "body", x: 0.1, y: 0.2 },
      { version: 2, kind: "element", path: 9, x: 0.1, y: 0.2 },
      { version: 2, kind: "element", path: "body", nodeId: 9, x: 0.1, y: 0.2 },
      { version: 2, kind: "element", path: "body", quote: 9, x: 0.1, y: 0.2 },
      { version: 2, kind: "element", path: "body", approx: "yes", x: 0.1, y: 0.2 },
      { version: 2, kind: "element", path: "body", approx: null, x: 0.1, y: 0.2 }
    ]) {
      assert.throws(
        () => feedback.addFeedback({ artifactId: "artifact-a", org: "acme", viewerEmail: "x", body: "Invalid", artifactRevision: 7, anchor }),
        /Anchor version|Anchor kind|Anchor path|Anchor nodeId|Anchor quote|Anchor approx/
      );
    }
  });
});

test("viewer delete and resolve enforce ownership, admin access, and parent cascade", () => {
  withFeedbackStore(({ feedback }) => {
    const parent = feedback.addFeedback({ artifactId: "artifact-a", org: "acme", viewerEmail: "owner@acme.test", body: "Parent", artifactRevision: 2 });
    const reply = feedback.addFeedback({ artifactId: "artifact-a", org: "acme", viewerEmail: "reply@acme.test", body: "Reply", artifactRevision: 2, parentId: parent.id });

    assert.deepEqual(feedback.deleteFeedback(reply.id, { viewerEmail: "other@acme.test", isAdmin: false }), { ok: false, reason: "forbidden" });
    assert.equal(feedback.getFeedback(reply.id).id, reply.id);
    assert.deepEqual(feedback.resolveByViewer(reply.id, { viewerEmail: "other@acme.test", isAdmin: false }), { ok: false, reason: "forbidden" });
    assert.deepEqual(feedback.resolveByViewer(reply.id, { viewerEmail: "reply@acme.test", isAdmin: false }), { ok: true, id: reply.id, changed: true });
    assert.equal(feedback.getFeedback(reply.id).resolved_by, "reply@acme.test");
    // A repeated resolve of an already-resolved item is a no-op transition (changed:false) so
    // callers can avoid re-emitting a "resolved" notification.
    assert.deepEqual(feedback.resolveByViewer(reply.id, { viewerEmail: "reply@acme.test", isAdmin: false }), { ok: true, id: reply.id, changed: false });
    assert.deepEqual(feedback.resolveByViewer(parent.id, { viewerEmail: "admin@acme.test", isAdmin: true }), { ok: true, id: parent.id, changed: true });
    assert.equal(feedback.getFeedback(parent.id).resolved_by, "admin:admin@acme.test");
    assert.deepEqual(feedback.deleteFeedback(parent.id, { viewerEmail: "admin@acme.test", isAdmin: true }), { ok: true, id: parent.id });
    assert.equal(feedback.getFeedback(parent.id), undefined);
    assert.equal(feedback.getFeedback(reply.id), undefined);
  });
});
