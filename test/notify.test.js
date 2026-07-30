import test, { after } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { randomBytes } from "node:crypto";

const dir = mkdtempSync(path.join(tmpdir(), "artifact-notify-"));
process.env.DATA_DIR = dir;
process.env.WEBHOOK_ENC_KEY = randomBytes(32).toString("base64");
const { default: db } = await import("../lib/db.js");
const orgs = await import("../lib/orgs.js");
const webhooks = await import("../lib/webhooks.js");
const notify = await import("../lib/notify.js");
const provider = await import("../lib/discord-delivery.js");

after(() => {
  db.close();
  rmSync(dir, { recursive: true, force: true });
  delete process.env.WEBHOOK_ENC_KEY;
});

function tick() {
  return new Promise((resolve) => setTimeout(resolve, 15));
}

test("buildEmbed creates an org-branded, linked Discord embed", () => {
  const body = notify.buildEmbed("published", {
    org: "acme",
    title: "Release notes",
    url: "https://artifact.test/a1",
    uploaderLabel: "Deploy bot",
    category: "Releases",
    revision: 3,
    bytes: 2048
  });
  assert.equal(body.embeds.length, 1);
  assert.equal(body.embeds[0].author.name, "acme");
  assert.equal(body.embeds[0].title, "Release notes");
  assert.equal(body.embeds[0].url, "https://artifact.test/a1");
  assert.ok(body.embeds[0].fields.some((field) => field.name === "Revision"));
});

test("emit only posts to matching webhook events and never throws on delivery failure", async () => {
  orgs.createOrg({ name: "notify" });
  webhooks.create({ org: "notify", url: "https://discord.com/api/webhooks/1/published", events: ["published"] });
  webhooks.create({ org: "notify", url: "https://discord.com/api/webhooks/2/feedback", events: ["feedback"] });
  const calls = [];
  notify.emit("published", "notify", { title: "A", url: "https://artifact.test/a", revision: 1 }, {
    fetchImpl: async (url, init) => { calls.push({ url, init }); return { ok: true }; }
  });
  await tick();
  assert.equal(calls.length, 1);
  assert.match(calls[0].url, /\/1\/published$/);
  assert.deepEqual(calls[0].init.headers, { "content-type": "application/json" });
  assert.equal(calls[0].init.body, "{\"embeds\":[{\"color\":3120756,\"author\":{\"name\":\"notify\"},\"title\":\"A\",\"url\":\"https://artifact.test/a\",\"fields\":[{\"name\":\"Publisher\",\"value\":\"Unknown\",\"inline\":true},{\"name\":\"Category\",\"value\":\"Uncategorized\",\"inline\":true},{\"name\":\"Revision\",\"value\":\"1\",\"inline\":true},{\"name\":\"Size\",\"value\":\"—\",\"inline\":true}],\"description\":\"Published artifact\"}]}");
  assert.equal(JSON.parse(calls[0].init.body).embeds[0].author.name, "notify");

  assert.doesNotThrow(() => notify.emit("published", "notify", { title: "A" }, {
    fetchImpl: async () => { throw new Error("network down"); }
  }));
  await tick();
});

test("emit uses Discord multipart attachments when a preview buffer is present", async () => {
  orgs.createOrg({ name: "multipart" });
  webhooks.create({ org: "multipart", url: "https://discord.com/api/webhooks/3/multipart", events: ["updated"] });
  const calls = [];
  const preview = Buffer.from("png-binary");

  notify.emit("updated", "multipart", { title: "Previewed", revision: 2 }, {
    preview,
    fetchImpl: async (url, init) => { calls.push({ url, init }); return { ok: true }; }
  });
  await tick();

  assert.equal(calls.length, 1);
  assert.equal(calls[0].init.headers, undefined);
  assert.ok(calls[0].init.body instanceof FormData);
  const payload = JSON.parse(calls[0].init.body.get("payload_json"));
  assert.equal(payload.embeds[0].image.url, "attachment://preview.png");
  const file = calls[0].init.body.get("files[0]");
  assert.equal(file.name, "preview.png");
  assert.equal(file.type, "image/png");
  assert.deepEqual(Buffer.from(await file.arrayBuffer()), preview);
});

test("null preview keeps the unchanged JSON delivery path", async () => {
  orgs.createOrg({ name: "preview-fallback" });
  webhooks.create({ org: "preview-fallback", url: "https://discord.com/api/webhooks/4/fallback", events: ["restored"] });
  const calls = [];

  notify.emit("restored", "preview-fallback", { title: "Fallback", revision: 4 }, {
    preview: null,
    fetchImpl: async (_url, init) => { calls.push(init); return { ok: true }; }
  });
  await tick();

  assert.deepEqual(calls[0].headers, { "content-type": "application/json" });
  assert.equal(JSON.parse(calls[0].body).embeds[0].image, undefined);
});

test("encrypted webhook delivery targets the decrypted Discord endpoint", async () => {
  orgs.createOrg({ name: "encrypted-delivery" });
  const secretUrl = "https://discord.com/api/webhooks/99/encrypted-delivery-token";
  const created = webhooks.create({ org: "encrypted-delivery", url: secretUrl, events: ["published"] });
  const stored = db.prepare("SELECT * FROM org_webhooks WHERE id = ?").get(created.id);
  const calls = [];

  notify.emit("published", "encrypted-delivery", { title: "Encrypted" }, {
    fetchImpl: async (url) => { calls.push(url); return { ok: true }; }
  });
  await tick();

  assert.doesNotMatch(JSON.stringify(stored), /encrypted-delivery-token/);
  assert.deepEqual(calls, [secretUrl]);
});

test("durable provider appends wait=true, preserves safe thread parameters, and drops unsafe query", () => {
  assert.equal(provider.DELIVERY_SEMANTICS, "bounded_at_least_once");
  assert.equal(provider.DISCORD_DELIVERY_TIMEOUT_MS, 4000);
  assert.equal(
    provider.executionUrl("https://discord.com/api/webhooks/1/token?thread_id=12&wait=false&junk=x&with_components=true", { threadId: "34" }),
    "https://discord.com/api/webhooks/1/token?thread_id=34&with_components=true&wait=true"
  );
  assert.throws(() => provider.executionUrl("https://evil.test/api/webhooks/1/secret"), /allowlist_rejected/);
});

test("durable provider accepts only a bounded Discord message id and classifies retry/terminal results", () => {
  assert.deepEqual(
    provider.classifyDiscordResponse({ status: 200, webhookRef: "webhook:test", body: '{"id":"123456789012345678"}' }),
    { state: "accepted", messageId: "123456789012345678", rateLimit: { webhookRef: "webhook:test", retryAfterMs: null, bucket: null, remaining: null, resetAfterMs: null, scope: null } }
  );
  for (const body of ["{}", '{"id":1}', '{"id":"001"}', '{"id":"bad"}']) {
    assert.deepEqual(
      provider.classifyDiscordResponse({ status: 204, webhookRef: "webhook:test", body }),
      { state: "retry", reason: "ambiguous", duplicateRisk: "possible", rateLimit: null }
    );
  }
  assert.deepEqual(
    provider.classifyDiscordResponse({
      status: 429,
      webhookRef: "webhook:test",
      headers: { "X-RateLimit-Reset-After": "9", "X-RateLimit-Bucket": "bucket-a", "X-RateLimit-Scope": "shared" },
      body: '{"retry_after":0.25,"global":true}'
    }),
    { state: "retry", reason: "rate_limited", duplicateRisk: "none", rateLimit: { webhookRef: "webhook:test", retryAfterMs: 250, bucket: "bucket-a", remaining: null, resetAfterMs: 9000, scope: "global" } }
  );
  for (const status of [500, 502, 503, 504]) {
    assert.deepEqual(provider.classifyDiscordResponse({ status, body: "" }), { state: "retry", reason: "server_error", duplicateRisk: "possible", rateLimit: null });
  }
  assert.deepEqual(provider.classifyDiscordResponse({ status: 302, body: "" }), { state: "terminal", reason: "redirect" });
  assert.deepEqual(provider.classifyDiscordResponse({ status: 403, body: "" }), { state: "terminal", reason: "forbidden" });
  assert.deepEqual(provider.classifyDiscordFault("decrypt_failed"), { state: "terminal", reason: "decrypt_failed" });
  assert.deepEqual(
    provider.classifyDiscordResponse({ status: 429, webhookRef: "webhook:test", headers: {}, body: "{}" }),
    { state: "terminal", reason: "invalid_rate_limit_delay" },
    "a 429 without a supplied positive delay must dead-letter, never use generic backoff"
  );
  assert.deepEqual(
    provider.classifyDiscordResponse({
      status: 429,
      webhookRef: "webhook:test",
      headers: { "X-RateLimit-Reset-After": "1.5" },
      body: "{}"
    }),
    { state: "terminal", reason: "invalid_rate_limit_delay" },
    "Reset-After is metadata and must not become an authoritative 429 retry delay"
  );
  assert.deepEqual(
    provider.classifyDiscordResponse({
      status: 429,
      headers: { "Retry-After": "1.5" },
      body: "{}"
    }),
    { state: "terminal", reason: "invalid_secret" },
    "a schedulable rate limit must retain its opaque top-level webhook reference"
  );
  for (const response of [
    { status: 429, webhookRef: "webhook:test", headers: {}, body: '{"retry_after":true}' },
    { status: 429, webhookRef: "webhook:test", headers: {}, body: '{"retry_after":"1.5"}' },
    { status: 429, webhookRef: "webhook:test", headers: { "Retry-After": "1e300" }, body: "{}" },
    { status: 429, webhookRef: "webhook:test", headers: { "Retry-After": "86400.001" }, body: "{}" }
  ]) {
    assert.deepEqual(
      provider.classifyDiscordResponse(response),
      { state: "terminal", reason: "invalid_rate_limit_delay" },
      "malformed, coerced, or over-bound delays must never schedule a retry"
    );
  }
  assert.deepEqual(
    provider.classifyDiscordResponse({
      status: 429,
      webhookRef: "webhook:test",
      headers: { "Retry-After": "86400" },
      body: "{}"
    }),
    {
      state: "retry",
      reason: "rate_limited",
      duplicateRisk: "none",
      rateLimit: {
        webhookRef: "webhook:test",
        retryAfterMs: provider.MAX_RETRY_DELAY_MS,
        bucket: null,
        remaining: null,
        resetAfterMs: null,
        scope: null
      }
    }
  );
});

test("durable provider refuses redirects, bounds timeout, and does not leak endpoint or payload", async () => {
  const token = "never-log-this-token";
  const payload = '{"payload":"never-log-this-payload"}';
  const calls = [];
  const result = await provider.deliverDiscordWebhook({
    webhookUrl: `https://discord.com/api/webhooks/1/${token}`,
    webhookRef: "webhook:opaque-webhook-ref",
    body: payload,
    fetchImpl: async (url, init) => {
      calls.push({ url, init });
      return { status: 200, headers: {}, text: async () => '{"id":"123"}' };
    }
  });
  assert.deepEqual(result, { state: "accepted", messageId: "123", rateLimit: { webhookRef: "webhook:opaque-webhook-ref", retryAfterMs: null, bucket: null, remaining: null, resetAfterMs: null, scope: null } });
  assert.equal(calls[0].init.redirect, "manual");
  assert.equal(calls[0].init.signal.aborted, false);
  assert.doesNotMatch(JSON.stringify(result), /never-log-this-token|never-log-this-payload/);
  const redirect = await provider.deliverDiscordWebhook({
    webhookUrl: `https://discord.com/api/webhooks/1/${token}`,
    webhookRef: "webhook:opaque-webhook-ref",
    body: payload,
    fetchImpl: async () => ({
      status: 302,
      headers: {},
      text: async () => {
        throw new Error("redirect bodies must not be read");
      }
    })
  });
  assert.deepEqual(redirect, { state: "terminal", reason: "redirect" });
});
