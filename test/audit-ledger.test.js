import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";
import {
  AUDIT_CAPABILITIES, assertAuditReady, auditContextFromAuth, createAuditLedger, createAuditMetrics,
  isValidAuditWebhookId, pendingReceiptsRoot
} from "../lib/audit.js";

const KEY = Buffer.alloc(32, 7).toString("base64");
function fixture({ timestamps = ["2026-01-01T00:00:00.000Z"], exportMaxBytes } = {}) {
  const db = new Database(":memory:");
  migrateDatabase(db);
  let next = 0;
  let clock = 0;
  return { db, ledger: createAuditLedger({ db, hmacKey: KEY, now: () => timestamps[Math.min(clock++, timestamps.length - 1)], id: () => `event-${++next}`, exportMaxBytes }) };
}
function context(caps = [AUDIT_CAPABILITIES.READ]) {
  return { ...auditContextFromAuth({ ok: true, clientId: "agent", org: "acme", role: "author" }, { source: "mcp", requestId: "request-1" }), capabilities: caps };
}

test("audit configuration is strict canonical base64 and fails closed", () => {
  assert.throws(() => assertAuditReady("not-base64-but-very-long-xxxxxxxxxxxxxxxx"));
  assert.doesNotThrow(() => assertAuditReady(KEY));
});

test("ledger atomically chains canonical bytes and detects tampering", () => {
  const { db, ledger } = fixture();
  ledger.append(context(), { operation: "artifact.publish", targetType: "artifact", targetId: "abc123", result: "success" });
  assert.deepEqual(ledger.verify().ok, true);
  db.prepare("UPDATE security_audit_events SET target_id = 'changed' WHERE sequence = 1").run();
  assert.deepEqual(ledger.verify(), { ok: false, sequence: 1 });
});

test("authenticated chain head detects tail deletion and head rollback", () => {
  const { db, ledger } = fixture();
  ledger.append(context(), { operation: "artifact.publish", targetType: "artifact", targetId: "one", result: "success" });
  ledger.append(context(), { operation: "artifact.update", targetType: "artifact", targetId: "two", result: "success" });
  const first = db.prepare("SELECT event_hash FROM security_audit_events WHERE sequence = 1").get();
  db.prepare("DELETE FROM security_audit_events WHERE sequence = 2").run();
  // An attacker can restore public tail columns but cannot forge the old authenticated head.
  db.prepare("UPDATE security_audit_chain_head SET sequence = 1, head_hash = ? WHERE singleton = 1").run(first.event_hash);
  assert.equal(ledger.verify().ok, false);
});

test("redaction permits only persisted webhook ids and rejects URLs, tokens, and bodies", () => {
  const { ledger } = fixture();
  assert.equal(isValidAuditWebhookId("wh0000000001"), true);
  assert.equal(isValidAuditWebhookId("https://discord.com/api/webhooks/1/secret"), false);
  assert.equal(isValidAuditWebhookId("a-real-webhook-token"), false);
  assert.doesNotThrow(() => ledger.append(context(), {
    operation: "webhook.test.completed", targetType: "webhook",
    targetId: "wh0000000001", result: "success"
  }));
  assert.throws(() => ledger.append(context(), { operation: "share.create", targetType: "share", targetId: "a-real-share-token", result: "success" }));
  assert.throws(() => ledger.append(context(), {
    operation: "webhook.test.completed", targetType: "webhook",
    targetId: "a-real-webhook-token", result: "failure"
  }));
  assert.throws(() => ledger.append(context(), {
    operation: "webhook.test.completed", targetType: "webhook",
    targetId: "https://discord.com/api/webhooks/1/secret", result: "failure"
  }));
  assert.throws(() => ledger.append(context(), { operation: "webhook.create", targetType: "org", targetId: "acme", classification: "https://discord.com/api/webhooks/1/secret", result: "success" }));
  assert.throws(() => ledger.append(context(), { operation: "artifact.publish", targetType: "artifact", targetId: "abc", body: "<html>secret</html>", result: "success" }));
});

test("tenant cursor cannot be replayed against another tenant and exports are bounded", () => {
  const { ledger } = fixture();
  ledger.append(context(), { operation: "share.create", targetType: "artifact", targetId: "abc123", result: "success" });
  ledger.append(context(), { operation: "share.revoke", targetType: "artifact", targetId: "abc123", result: "success" });
  const page = ledger.query(context(), { limit: 1 });
  assert.ok(page.next);
  const second = ledger.query(context(), { limit: 1, cursor: page.next });
  assert.equal(second.events[0].sequence, 2);
  assert.throws(() => ledger.query({ ...context(), tenant: "other" }, { tenant: "other", cursor: page.next }));
  assert.equal(ledger.export(context([AUDIT_CAPABILITIES.READ, AUDIT_CAPABILITIES.EXPORT])).rows, 2);
});

test("receipt finalization is exactly once", () => {
  const { db, ledger } = fixture();
  assert.equal(ledger.reserveReceipt("durability:abc", "publish:abc:1", context(), { operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success" }), true);
  assert.match(db.prepare("SELECT receipt_mac FROM security_audit_receipts").pluck().get(), /^[a-f0-9]{64}$/);
  const first = ledger.finalizeReceipt("durability:abc", context(), { operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success" });
  const retry = ledger.finalizeReceipt("durability:abc", context(), { operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success" });
  assert.equal(first.duplicate, false);
  assert.equal(retry.duplicate, true);
  assert.equal(ledger.verify().events, 1);
  assert.throws(() => ledger.finalizeReceipt("durability:abc", { ...context(), actorId: "other-agent" }, { operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success" }), /context mismatch/);
  assert.throws(() => ledger.finalizeReceipt("durability:abc", context(), { operation: "artifact.publish", targetType: "artifact", targetId: "other", result: "success" }), /context mismatch/);
});

test("receipt MAC matches the Rust protocol vector", () => {
  const { db, ledger } = fixture();
  ledger.reserveReceipt("durability:abc", "publish:abc:1", {
    tenant: "acme", actorType: "api_key", actorId: "key-1", actorRole: "author",
    source: "mcp", requestId: "request-1"
  }, {
    operation: "artifact.publish", targetType: "artifact", targetId: "abc",
    result: "success", classification: "", revision: 1
  });
  assert.equal(
    db.prepare("SELECT receipt_mac FROM security_audit_receipts").pluck().get(),
    "c31530275592868cd7ed2070e9f8d0da3f827905bb555283ffe6d8b2ac6af9b3"
  );
});

test("pending receipt roots match the Rust protocol vectors", () => {
  assert.equal(pendingReceiptsRoot([]), "8826f8e30ad491deb7642729c14a19fde13144b8f1b1d15e4eca84585f18be53");
  assert.equal(
    pendingReceiptsRoot(["c31530275592868cd7ed2070e9f8d0da3f827905bb555283ffe6d8b2ac6af9b3"]),
    "faabbef514cca7b567de7b20dc04ee845749efa0ef1cbf76b86ad784ec984b14"
  );
});

test("receipt finalization rejects tampering of every authenticated snapshot field", async (t) => {
  const cases = [
    ["correlation_id", "durability:tampered"],
    ["durability_intent_id", "publish:other:1"],
    ["state", "finalized"],
    ["event_id", "event-other"],
    ["tenant", "other"],
    ["actor_type", "viewer"],
    ["actor_id", "other-agent"],
    ["actor_role", "admin"],
    ["source", "browser"],
    ["request_id", "request-2"],
    ["operation", "artifact.update"],
    ["target_type", "document"],
    ["target_id", "other"],
    ["result", "denied"],
    ["classification", "policy"],
    ["revision", 2],
    ["key_id", "v2"],
    ["canonical_version", 2],
    ["receipt_mac", "0".repeat(64)],
  ];
  for (const [field, value] of cases) {
    await t.test(field, () => {
      const { db, ledger } = fixture();
      const original = "durability:abc";
      ledger.reserveReceipt(original, "publish:abc:1", context(), {
        operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
      });
      db.prepare(`UPDATE security_audit_receipts SET ${field} = ? WHERE correlation_id = ?`).run(value, original);
      const correlation = field === "correlation_id" ? String(value) : original;
      assert.throws(() => ledger.finalizeReceipt(correlation, context(), {
        operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
      }), /integrity verification failed|pending receipt commitment verification failed/);
      assert.equal(db.prepare("SELECT COUNT(*) FROM security_audit_events").pluck().get(), 0);
    });
  }
});

test("pending receipt deletion and pending-to-finalized pointer tampering fail closed", () => {
  {
    const { db, ledger } = fixture();
    ledger.reserveReceipt("durability:abc", "publish:abc:1", context(), {
      operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
    });
    db.prepare("DELETE FROM security_audit_receipts WHERE correlation_id = ?").run("durability:abc");
    assert.equal(ledger.verify().ok, false);
    assert.throws(() => ledger.finalizeReceipt("durability:abc", context(), {
      operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
    }), /pending receipt commitment verification failed/);
  }
  {
    const { db, ledger } = fixture();
    ledger.reserveReceipt("durability:done", "publish:done:1", context(), {
      operation: "artifact.publish", targetType: "artifact", targetId: "done", result: "success"
    });
    const completed = ledger.finalizeReceipt("durability:done", context(), {
      operation: "artifact.publish", targetType: "artifact", targetId: "done", result: "success"
    });
    db.prepare("DELETE FROM security_audit_receipts WHERE correlation_id=?").run("durability:done");
    ledger.reserveReceipt("durability:abc", "publish:abc:1", context(), {
      operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
    });
    db.prepare("UPDATE security_audit_receipts SET state='finalized', event_id=? WHERE correlation_id=?")
      .run(completed.eventId, "durability:abc");
    assert.throws(() => ledger.finalizeReceipt("durability:abc", context(), {
      operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
    }), /pending receipt commitment verification failed/);
  }
});

test("finalized duplicate validates its referenced terminal event projection", () => {
  const { db, ledger } = fixture();
  ledger.reserveReceipt("durability:abc", "publish:abc:1", context(), {
    operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
  });
  const completed = ledger.finalizeReceipt("durability:abc", context(), {
    operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
  });
  db.prepare("UPDATE security_audit_events SET target_id='other' WHERE event_id=?").run(completed.eventId);
  assert.throws(() => ledger.finalizeReceipt("durability:abc", context(), {
    operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success"
  }), /event projection mismatch/);
});

test("retention only prunes a contiguous expired prefix", () => {
  const { db, ledger } = fixture({ timestamps: ["2025-01-01T00:00:00.000Z", "2026-01-01T00:00:00.000Z", "2025-01-02T00:00:00.000Z"] });
  for (const targetId of ["one", "two", "three"]) ledger.append(context(), { operation: "artifact.update", targetType: "artifact", targetId, result: "success" });
  assert.equal(ledger.prune("2025-06-01T00:00:00.000Z"), 1);
  assert.deepEqual(db.prepare("SELECT sequence FROM security_audit_events ORDER BY sequence").all().map((row) => row.sequence), [2, 3]);
  assert.equal(ledger.verify().ok, true);
});

test("exports use a signed continuation and never loop on a byte-exceeding first row", () => {
  const { ledger } = fixture({ exportMaxBytes: 1 });
  for (let n = 0; n < 501; n += 1) ledger.append(context(), { operation: "artifact.update", targetType: "artifact", targetId: `item-${n}`, result: "success" });
  const caps = context([AUDIT_CAPABILITIES.READ, AUDIT_CAPABILITIES.EXPORT]);
  const page = ledger.export(caps, { limit: 500, });
  // Recreate against the production byte cap for the continuation assertion.
  const { ledger: pageable } = fixture();
  for (let n = 0; n < 501; n += 1) pageable.append(context(), { operation: "artifact.update", targetType: "artifact", targetId: `item-${n}`, result: "success" });
  const normalPage = pageable.export(caps, { limit: 500 });
  assert.equal(normalPage.rows, 500);
  assert.ok(normalPage.next);
  assert.equal(pageable.export(caps, { cursor: normalPage.next }).rows, 1);
  const oversized = page;
  assert.deepEqual({ rows: oversized.rows, next: oversized.next, reason: oversized.reason }, { rows: 0, next: null, reason: "first_row_exceeds_export_cap" });
});

test("receipt reservation participates in the caller transaction", () => {
  const { db, ledger } = fixture();
  const work = db.transaction(() => {
    ledger.reserveReceiptInTransaction("rollback:abc", "publish:abc:1", context(), { operation: "artifact.publish", targetType: "artifact", targetId: "abc", result: "success" });
    throw new Error("simulate metadata rollback");
  });
  assert.throws(work);
  assert.equal(db.prepare("SELECT COUNT(*) AS n FROM security_audit_receipts").get().n, 0);
});

test("retention checkpoint bridges the retained suffix", () => {
  const { ledger } = fixture({ timestamps: ["2025-01-01T00:00:00.000Z", "2026-01-01T00:00:00.000Z"] });
  ledger.append(context(), { operation: "key.create", targetType: "key", targetId: "agent", result: "success" });
  ledger.append(context(), { operation: "key.revoke", targetType: "key", targetId: "agent", result: "success" });
  assert.equal(ledger.prune("2025-06-01T00:00:00.000Z"), 1);
  assert.equal(ledger.verify().ok, true);
});

test("security signals have only fixed low-cardinality labels", () => {
  const metrics = createAuditMetrics();
  metrics.record("auth_failure");
  metrics.record("rate_limit");
  metrics.record("untrusted-user-input");
  assert.match(metrics.renderPrometheus(), /signal="auth_failure"/);
  assert.match(metrics.renderPrometheus(), /signal="rate_limit"/);
  assert.doesNotMatch(metrics.renderPrometheus(), /untrusted-user-input/);
});
