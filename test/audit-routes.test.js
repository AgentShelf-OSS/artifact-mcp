// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import test from "node:test";
import assert from "node:assert/strict";
import Database from "better-sqlite3";

import { createApp } from "../lib/app.js";
import { AUDIT_CAPABILITIES, createAuditLedger, createAuditMetrics } from "../lib/audit.js";
import { migrateDatabase } from "../lib/migrations.js";

const KEY = Buffer.alloc(32, 58).toString("base64");

function unusedDependencies(overrides) {
  return {
    handleMcp: async () => null,
    resolveViewer: async () => ({ email: null, org: null, isAdmin: false }),
    artifacts: {},
    keys: {},
    orgs: {},
    reactions: {},
    feedback: {},
    pages: {},
    ...overrides
  };
}

async function serve(app, fn) {
  const server = app.listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  try {
    await fn(`http://127.0.0.1:${server.address().port}`);
  } finally {
    await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
}

function responseFor(request) {
  const bearer = request.headers.authorization;
  if (request.headers["x-api-key"] === "legacy") {
    return { ok: true, authType: "api_key", clientId: "legacy", org: "acme", scopes: null };
  }
  const scopes = {
    "Bearer read": [AUDIT_CAPABILITIES.READ],
    "Bearer global": [AUDIT_CAPABILITIES.READ, AUDIT_CAPABILITIES.GLOBAL],
    "Bearer export": [AUDIT_CAPABILITIES.READ, AUDIT_CAPABILITIES.EXPORT],
    "Bearer global-export": [AUDIT_CAPABILITIES.READ, AUDIT_CAPABILITIES.EXPORT, AUDIT_CAPABILITIES.GLOBAL],
    "Bearer insufficient": []
  }[bearer];
  return scopes === undefined
    ? { ok: false }
    : { ok: true, authType: "oauth", clientId: "audit-reader", org: "acme", scopes: new Set(scopes) };
}

function append(ledger, tenant, targetId) {
  ledger.append({
    tenant,
    actorType: "api_key",
    actorId: "credential-secret-must-not-leak",
    actorRole: "author",
    source: "mcp",
    requestId: "request-must-not-leak"
  }, {
    operation: "artifact.update",
    targetType: "artifact",
    targetId,
    result: "success",
    classification: "internal"
  });
}

test("audit HTTP routes are OAuth-only, tenant-safe, paginated, and bounded", async () => {
  const db = new Database(":memory:");
  migrateDatabase(db);
  let next = 0;
  const ledger = createAuditLedger({
    db,
    hmacKey: KEY,
    id: () => `event-${++next}`,
    now: () => "2026-07-29T00:00:00.000Z"
  });
  // 10,001 events prove both the 500 interactive-page cap and the 10k export cap through the
  // real ledger implementation; no route-local pagination logic is trusted.
  for (let index = 0; index < 10_001; index += 1) append(ledger, "acme", `artifact-acme-${index}`);
  append(ledger, "other", "artifact-other-1");

  const securityMetrics = createAuditMetrics();
  const app = createApp(unusedDependencies({
    checkPublisherKey: responseFor,
    audit: ledger,
    securityMetrics,
    oauth: { enabled: true, issuer: "https://issuer.example.test" }
  }));

  await serve(app, async (baseUrl) => {
    for (const path of ["/audit/events", "/audit/export"]) {
      const response = await fetch(`${baseUrl}${path}`);
      assert.equal(response.status, 401, `${path} requires authentication`);
    }
    const metrics = await (await fetch(`${baseUrl}/metrics`)).text();
    assert.match(metrics, /signal="auth_failure"} 2/);

    const apiKey = await fetch(`${baseUrl}/audit/events`, { headers: { "x-api-key": "legacy" } });
    assert.equal(apiKey.status, 403, "legacy API keys never inherit audit access");
    const insufficient = await fetch(`${baseUrl}/audit/events`, { headers: { authorization: "Bearer insufficient" } });
    assert.equal(insufficient.status, 403);

    const defaultPage = await fetch(`${baseUrl}/audit/events`, { headers: { authorization: "Bearer read" } });
    assert.equal((await defaultPage.json()).events.length, 100, "interactive reads default to 100 rows");
    const page = await fetch(`${baseUrl}/audit/events?limit=10000`, { headers: { authorization: "Bearer read" } });
    assert.equal(page.status, 200);
    assert.equal(page.headers.get("cache-control"), "no-store");
    const pageJson = await page.json();
    assert.equal(pageJson.events.length, 500, "interactive reads are clamped to 500 rows");
    assert.ok(pageJson.next, "interactive reads have a signed continuation");
    assert.doesNotMatch(JSON.stringify(pageJson), /credential-secret-must-not-leak|request-must-not-leak/);

    const replay = await fetch(`${baseUrl}/audit/events?cursor=${encodeURIComponent(`${pageJson.next}x`)}`, {
      headers: { authorization: "Bearer read" }
    });
    assert.equal(replay.status, 400, "modified continuations fail closed");
    const crossTenant = await fetch(`${baseUrl}/audit/events?tenant=other`, { headers: { authorization: "Bearer read" } });
    assert.equal(crossTenant.status, 403);
    const global = await fetch(`${baseUrl}/audit/events?tenant=other`, { headers: { authorization: "Bearer global" } });
    assert.equal(global.status, 200);
    assert.equal((await global.json()).events[0].tenant, "other");
    const crossTenantCursor = await fetch(`${baseUrl}/audit/events?tenant=other&cursor=${encodeURIComponent(pageJson.next)}`, {
      headers: { authorization: "Bearer global" }
    });
    assert.equal(crossTenantCursor.status, 400, "a signed cursor remains tenant-bound for global readers");

    const exportDenied = await fetch(`${baseUrl}/audit/export`, { headers: { authorization: "Bearer read" } });
    assert.equal(exportDenied.status, 403, "export has a distinct scope");
    const exportCrossTenant = await fetch(`${baseUrl}/audit/export?tenant=other`, { headers: { authorization: "Bearer export" } });
    assert.equal(exportCrossTenant.status, 403);
    const oneRow = await fetch(`${baseUrl}/audit/export?limit=1`, { headers: { authorization: "Bearer export" } });
    assert.equal(oneRow.status, 200);
    assert.equal(oneRow.headers.get("content-type"), "application/x-ndjson; charset=utf-8");
    assert.equal(oneRow.headers.get("cache-control"), "no-store");
    assert.ok(oneRow.headers.get("x-audit-next"));
    assert.equal(oneRow.headers.get("x-audit-truncated"), "true");
    assert.match(await oneRow.text(), /\n$/);

    const boundedExport = await fetch(`${baseUrl}/audit/export?limit=100000`, { headers: { authorization: "Bearer export" } });
    assert.equal(boundedExport.status, 200);
    const ndjson = await boundedExport.text();
    assert.ok(ndjson.split("\n").filter(Boolean).length <= 10_000, "exports never exceed 10k rows");
    assert.ok(Buffer.byteLength(ndjson) <= 5 * 1024 * 1024, "exports never exceed 5 MiB");
    assert.equal(boundedExport.headers.get("x-audit-truncated"), "true");

    const globalExport = await fetch(`${baseUrl}/audit/export?tenant=other&limit=1`, {
      headers: { authorization: "Bearer global-export" }
    });
    assert.equal(globalExport.status, 200, "audit:global explicitly permits cross-tenant export");
  });
});
