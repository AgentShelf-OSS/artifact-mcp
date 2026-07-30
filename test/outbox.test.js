import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";
import { migrateDatabase } from "../lib/migrations.js";
import { createOutboxStore, MAX_OUTBOX_PAYLOAD_BYTES, OUTBOX_LEASE_MS } from "../lib/outbox.js";

function fixture({ id } = {}) { const db = new Database(":memory:"); db.pragma("foreign_keys = ON"); migrateDatabase(db); let time = 100; let next = 0; return { db, setTime(value) { time = value; }, store: createOutboxStore({ db, now: () => time, id: id || (() => `outbox-${++next}`) }) }; }
function event(id, target = "wh-a") { return { event_id: id, tenant: "acme", event_type: "published", target_key: target, secret_ref: "webhook:wh-secret-ref", payload: Buffer.from('{"content":"hello"}') }; }

test("v27 schema keeps secret references unlinked, bounded, intent-gated, and indexed", () => {
  const { db, store } = fixture(); const sql = db.prepare("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'provider_delivery_outbox'").pluck().get();
  assert.match(sql, /length\(payload\) <= 32768/); assert.match(sql, /ON DELETE RESTRICT/); assert.match(sql, /state = 'blocked'/); assert.doesNotMatch(sql, /org_webhooks/);
  const indexes = db.prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'provider_delivery_outbox'").pluck().all();
  for (const name of ["provider_delivery_outbox_ready_idx", "provider_delivery_outbox_tenant_idx", "provider_delivery_outbox_target_idx", "provider_delivery_outbox_intent_idx", "provider_delivery_outbox_bucket_idx"]) assert.ok(indexes.includes(name));
  const row = store.enqueue(event("lease-check"));
  assert.throws(
    () => db.prepare("UPDATE provider_delivery_outbox SET lease_owner='orphan' WHERE id=?").run(row.id),
    /CHECK constraint failed/,
    "every non-leased state must keep every lease field NULL"
  );
  db.close();
});

test("enqueue is stable, atomically capacity-checked, hash-sensitive, and redacts secrets", () => {
  const { db, store } = fixture(); const one = store.enqueue(event("event-1")); const twice = store.enqueue(event("event-1"));
  assert.equal(one.id, twice.id); assert.equal(one.payload_bytes, Buffer.byteLength('{"content":"hello"}')); assert.equal("secret_ref" in one, false); assert.equal("payload" in one, false);
  assert.throws(() => store.enqueue({ ...event("event-2"), payload_sha256: "0".repeat(64) }), /payload hash/);
  assert.throws(() => store.enqueue({ ...event("event-3"), payload: Buffer.alloc(MAX_OUTBOX_PAYLOAD_BYTES + 1) }), /32 KiB/);
  const atomic = db.transaction(() => store.enqueueInTransaction(event("event-atomic"))); assert.equal(atomic().event_id, "event-atomic"); db.close();
});

test("claims are ordered and two workers serialize targets and persisted buckets", () => {
  const { db, store, setTime } = fixture(); store.enqueue(event("one", "wh-a")); store.enqueue(event("two", "wh-a")); store.enqueue(event("three", "wh-b"));
  const first = store.claimNext("worker-a"); const other = store.claimNext("worker-b"); assert.equal(first.event_id, "one"); assert.equal(other.event_id, "three"); assert.equal(store.claimNext("worker-b"), undefined);
  store.applyRateLimit({ scope: "target", target_key: "wh-a", blocked_until: 5_000 }); setTime(100 + OUTBOX_LEASE_MS + 1);
  const recovered = store.claimNext("worker-c"); assert.equal(recovered.event_id, "one");
  assert.equal(db.prepare("SELECT state FROM provider_delivery_outbox WHERE id = ?").pluck().get(first.id), "leased"); assert.equal(db.prepare("SELECT state FROM provider_delivery_outbox WHERE id = ?").pluck().get(other.id), "retry"); assert.equal(recovered.duplicate_risk, 1); db.close();
});

test("accepted retry and dead-letter records require the live lease owner", () => {
  const { db, store, setTime } = fixture(); const ready = store.enqueue(event("one")); const leased = store.claimNext("worker"); assert.equal(store.accepted(leased.id, "other", leased.lease_token, leased.lease_version, "discord-1"), false); assert.equal(store.retry(leased.id, "worker", leased.lease_token, leased.lease_version, 500, "network_retry", true), true);
  setTime(500); const retry = store.claimNext("worker"); assert.equal(retry.attempts, 2); assert.equal(store.deadLetter(retry.id, "worker", retry.lease_token, retry.lease_version, "invalid_webhook", "https://discord.com/api/webhooks/secret"), true);
  const final = store.get(ready.id); assert.equal(final.state, "dead_letter"); assert.equal(final.terminal_error, "provider delivery failed"); assert.equal("secret_ref" in final, false); db.close();
});

test("bucket metadata and global target effective-bucket throttles persist atomically", () => {
  const { db, store, setTime } = fixture(); const queued = store.enqueue(event("one", "wh-a"));
  store.persistRateLimit({ scope: "bucket", target_key: "wh-a", bucket_id: "bucket-1", top_level_secret_ref: "webhook:wh-secret-ref", blocked_until: 500 });
  store.persistRateLimit({ scope: "bucket", target_key: "wh-a", bucket_id: "bucket-1", top_level_secret_ref: "webhook:wh-secret-ref", blocked_until: 200 });
  assert.equal(db.prepare("SELECT blocked_until FROM provider_delivery_rate_limits").pluck().get(), 500);
  assert.equal(store.claimNext("worker"), undefined); setTime(500); const leased = store.claimNext("worker"); assert.equal(leased.bucket_id, "bucket-1");
  setTime(500 + OUTBOX_LEASE_MS + 1); assert.equal(store.accepted(leased.id, "worker", leased.lease_token, leased.lease_version, "late"), true);
  assert.equal(store.accepted(queued.id, "worker", leased.lease_token, leased.lease_version, "stale"), false); db.close();
});

test("rate-limit keys are scope-normalized, bounded, and never persist raw webhook secrets", () => {
  const { db, store } = fixture();
  const secret = "https://discord.com/api/webhooks/1/ULTRA_SECRET_TOKEN";
  for (const input of [
    { scope: "global", target_key: secret, blocked_until: 500 },
    { scope: "target", target_key: secret, blocked_until: 500 },
    { scope: "bucket", target_key: "wh-a", bucket_id: secret, top_level_secret_ref: "webhook:wh-secret-ref", blocked_until: 500 },
    { scope: "bucket", target_key: "wh-a", bucket_id: "bucket", top_level_secret_ref: secret, blocked_until: 500 },
    { scope: "bucket", target_key: "wh-a", bucket_id: "x".repeat(129), top_level_secret_ref: "webhook:wh-secret-ref", blocked_until: 500 }
  ]) {
    assert.throws(() => store.persistRateLimit(input), /invalid rate limit state/);
  }
  assert.equal(db.prepare("SELECT COUNT(*) FROM provider_delivery_rate_limits").pluck().get(), 0);
  assert.doesNotMatch(JSON.stringify(db.prepare("SELECT * FROM provider_delivery_rate_limits").all()), /ULTRA_SECRET_TOKEN/);
  store.persistRateLimit({ scope: "global", blocked_until: 500 });
  store.persistRateLimit({ scope: "target", target_key: "wh-a", blocked_until: 500 });
  store.persistRateLimit({ scope: "bucket", target_key: "wh-a", bucket_id: "bucket-a", top_level_secret_ref: "webhook:wh-secret-ref", blocked_until: 500 });
  assert.deepEqual(
    db.prepare("SELECT scope,target_key,bucket_id,top_level_secret_ref FROM provider_delivery_rate_limits ORDER BY scope").all(),
    [
      { scope: "bucket", target_key: "", bucket_id: "bucket-a", top_level_secret_ref: "webhook:wh-secret-ref" },
      { scope: "global", target_key: "", bucket_id: "", top_level_secret_ref: "" },
      { scope: "target", target_key: "wh-a", bucket_id: "", top_level_secret_ref: "" }
    ]
  );
  db.close();
});

test("terminal error storage rejects standalone secret-like provider text", () => {
  const { db, store } = fixture(); const queued = store.enqueue(event("one")); const leased = store.claimNext("worker");
  assert.equal(store.deadLetter(leased.id, "worker", leased.lease_token, leased.lease_version, "dead_letter", "ULTRA_SECRET_DISCORD_RESPONSE_TOKEN_4Jk72pXq"), true);
  assert.equal(store.get(queued.id).terminal_error, "provider delivery failed"); db.close();
});

test("lease version fences a reused owner/token after reclaim", () => {
  let calls = 0; const { db, store, setTime } = fixture({ id: () => ++calls === 1 ? "row-1" : "reused-token" });
  store.enqueue(event("one")); const first = store.claimNext("worker"); setTime(100 + OUTBOX_LEASE_MS + 1); const second = store.claimNext("worker");
  assert.equal(second.lease_version, first.lease_version + 1); assert.equal(store.accepted(first.id, "worker", "reused-token", first.lease_version, "late-v1"), false); assert.equal(store.accepted(second.id, "worker", "reused-token", second.lease_version, "current-v2"), true); db.close();
});

test("URL-like identifiers, unsafe classifications, and mismatched idempotency are rejected", () => {
  const { db, store } = fixture(); assert.throws(() => store.enqueue({ ...event("bad"), target_key: "https://discord.com/api/webhooks/1/token" }), /identity fields/); assert.throws(() => store.enqueue({ ...event("raw"), secret_ref: "raw-token-must-never-be-here" }), /identity fields/);
  store.enqueue(event("one")); assert.throws(() => store.enqueue({ ...event("one"), payload: Buffer.from("different") }), /idempotency conflict/);
  const leased = store.claimNext("worker");
  assert.throws(() => store.retry(leased.id, "worker", leased.lease_token, leased.lease_version, 200, "invalid_secret"), /classification/);
  assert.throws(() => store.deadLetter(leased.id, "worker", leased.lease_token, leased.lease_version, "network", "network"), /classification/);
  assert.throws(() => store.deadLetter(leased.id, "worker", leased.lease_token, leased.lease_version, "invalid_secret", "invalid_secret", true), /duplicate risk/);
  assert.throws(() => store.retry(leased.id, "worker", leased.lease_token, leased.lease_version, 200, "https://discord.com/api/webhooks/token"), /classification/);
  assert.equal(store.accepted(leased.id, "worker", leased.lease_token, leased.lease_version, "123", "network"), true);
  assert.equal(store.get(leased.id).result_classification, "accepted");
  db.close();
});

test("fixed provider outcomes permit representative retry and terminal classifications", () => {
  const { db, store } = fixture(); store.enqueue(event("retry", "wh-a")); const retry = store.claimNext("worker"); assert.equal(store.retry(retry.id, "worker", retry.lease_token, retry.lease_version, 200, "network"), true); assert.equal(store.get(retry.id).result_classification, "network");
  store.enqueue(event("terminal", "wh-b")); const terminal = store.claimNext("worker"); assert.equal(store.deadLetter(terminal.id, "worker", terminal.lease_token, terminal.lease_version, "unknown_webhook", "unknown_webhook"), true); const result = store.get(terminal.id); assert.equal(result.result_classification, "unknown_webhook"); assert.equal(result.terminal_error, "unknown_webhook"); db.close();
});

test("direct exhausted dead letters preserve explicit duplicate risk", () => {
  const { db, store } = fixture();
  store.enqueue(event("exhausted"));
  let leased = store.claimNext("worker");
  assert.equal(store.deadLetter(leased.id, "worker", leased.lease_token, leased.lease_version, "attempts_exhausted", "attempts_exhausted", true), false);
  assert.equal(store.retry(leased.id, "worker", leased.lease_token, leased.lease_version, 100, "network", true), true);
  for (let attempt = 2; attempt < 8; attempt += 1) {
    leased = store.claimNext("worker");
    assert.equal(store.retry(leased.id, "worker", leased.lease_token, leased.lease_version, 100, "network", true), true);
  }
  leased = store.claimNext("worker");
  assert.equal(leased.attempts, 8);
  assert.equal(store.deadLetter(leased.id, "worker", leased.lease_token, leased.lease_version, "attempts_exhausted", "attempts_exhausted", true), true);
  assert.deepEqual(
    db.prepare("SELECT state,result_classification,duplicate_risk FROM provider_delivery_outbox WHERE id=?").get(leased.id),
    { state: "dead_letter", result_classification: "attempts_exhausted", duplicate_risk: 1 }
  );
  db.close();
});

test("the eighth retry atomically dead-letters, frees capacity, and unblocks target FIFO", () => {
  const { db, store } = fixture();
  store.enqueue(event("exhaust", "wh-a"));
  store.enqueue(event("follower", "wh-a"));
  for (let attempt = 1; attempt <= 8; attempt += 1) {
    const leased = store.claimNext("worker");
    assert.equal(leased.event_id, "exhaust");
    assert.equal(leased.attempts, attempt);
    assert.equal(store.retry(leased.id, "worker", leased.lease_token, leased.lease_version, 100, "network", true), true);
  }
  const exhausted = db.prepare("SELECT state,attempts,result_classification,terminal_error,duplicate_risk FROM provider_delivery_outbox WHERE event_id='exhaust'").get();
  assert.deepEqual(exhausted, { state: "dead_letter", attempts: 8, result_classification: "attempts_exhausted", terminal_error: "attempts_exhausted", duplicate_risk: 1 });
  assert.equal(store.depth(), 1, "dead letters do not consume active queue capacity");
  const follower = store.claimNext("worker");
  assert.equal(follower.event_id, "follower", "the exhausted predecessor no longer blocks target FIFO");
  db.close();
});

test("an expired eighth lease dead-letters on restart and releases the FIFO follower", () => {
  const { db, store, setTime } = fixture();
  store.enqueue(event("exhaust", "wh-a"));
  store.enqueue(event("follower", "wh-a"));
  for (let attempt = 1; attempt < 8; attempt += 1) {
    const leased = store.claimNext("worker");
    store.retry(leased.id, "worker", leased.lease_token, leased.lease_version, 100, "network", true);
  }
  const eighth = store.claimNext("worker");
  assert.equal(eighth.attempts, 8);
  setTime(100 + OUTBOX_LEASE_MS + 1);
  const follower = store.claimNext("replacement");
  assert.equal(follower.event_id, "follower");
  assert.deepEqual(
    db.prepare("SELECT state,result_classification,duplicate_risk FROM provider_delivery_outbox WHERE event_id='exhaust'").get(),
    { state: "dead_letter", result_classification: "attempts_exhausted_after_worker_restart", duplicate_risk: 1 }
  );
  db.close();
});
