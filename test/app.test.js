import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createApp } from "../lib/app.js";
import { createMcpTelemetry } from "../lib/observability.js";
import { createArtifactPreviewNotifier } from "../lib/preview.js";
import { renderSettings } from "../lib/settings.js";

const identityDataDir = mkdtempSync(join(tmpdir(), "artifact-mcp-identity-"));
process.env.DATA_DIR = identityDataDir;
test.after(() => rmSync(identityDataDir, { recursive: true, force: true }));

let identityImportId = 0;
async function withIdentityEnv(values, fn) {
  const names = [
    "CF_ACCESS_TEAM_DOMAIN",
    "CF_ACCESS_AUD",
    "TRUST_ACCESS_HEADERS",
    "REQUIRE_ACCESS_JWT",
    "LISTEN_HOST",
    "HEADER_TRUST_ALLOW_INSECURE",
    "ADMIN_EMAILS",
    "ADMIN_EMAIL_DOMAINS",
    "ORG_EMAIL_DOMAINS"
  ];
  const previous = new Map(names.map((name) => [name, process.env[name]]));
  for (const name of names) delete process.env[name];
  Object.assign(process.env, values);
  try {
    const url = new URL(`../lib/identity.js?test=${++identityImportId}`, import.meta.url);
    return await fn(await import(url.href));
  } finally {
    for (const [name, value] of previous) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
  }
}

function dependencies(overrides = {}) {
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", is_bundle: 0 };
  return {
    checkPublisherKey: () => ({ ok: false }),
    handleMcp: async () => null,
    resolveViewer: async () => ({ email: "viewer@other.test", org: "other", isAdmin: false }),
    artifacts: {
      getArtifactMeta: () => artifact,
      readArtifact: () => ({ meta: artifact, html: "<h1>Artifact</h1>" }),
      readBundleFile: () => null,
      listOrgArtifacts: () => [],
      listAllGroupedByOrg: () => new Map(),
      listOrgIds: () => [artifact.id],
      deleteArtifactById: () => true,
      listRevisions: () => ({ current: 1, revisions: [] }),
      restoreArtifactRevision: () => ({ ok: true, id: artifact.id, revision: 2, restoredFrom: 1 }),
      readHistoryArtifact: () => null,
      readHistoryBundleFile: () => null
    },
    keys: { list: () => [], create: () => ({}), revoke: () => false },
    orgs: {
      list: () => [],
      names: () => [],
      has: () => true,
      create: () => ({}),
      remove: () => true,
      addDomain: () => ({}),
      removeDomain: () => true,
      addEmailMember: () => ({}),
      removeEmailMember: () => true,
      addCategory: () => ({}),
      removeCategory: () => true,
      categoriesFor: () => [],
      setColor: () => ({}),
      colorMap: () => ({})
    },
    reactions: {
      get: () => ({ favorite: 0, vote: 0 }),
      set: () => ({ favorite: 0, vote: 0 }),
      forViewer: () => new Map(),
      sentiment: () => new Map()
    },
    feedback: { listForArtifact: () => [] },
    pages: {
      gallery: () => "gallery",
      shell: () => "shell",
      notFound: () => "not found",
      notSignedIn: () => "not signed in",
      settings: () => "settings"
    },
    logger: { info() {}, error() {} },
    ...overrides
  };
}

test("gallery projects the registered category catalogue for each visible organization", async () => {
  let galleryArgs;
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    artifacts: {
      ...dependencies().artifacts,
      listAllGroupedByOrg: () => new Map([
        ["agentshelf", [{ id: "a1", org: "agentshelf", title: "A", client_id: "publisher", is_bundle: 0, category: "UI/UX" }]],
        ["homelab", [{ id: "h1", org: "homelab", title: "H", client_id: "publisher", is_bundle: 0, category: "Dashboards" }]],
      ]),
    },
    orgs: {
      ...dependencies().orgs,
      names: () => ["agentshelf", "homelab"],
      categoriesFor: (org) => org === "agentshelf" ? ["Specs", "UI/UX"] : ["Dashboards", "Runbooks"],
    },
    pages: {
      ...dependencies().pages,
      gallery: (...args) => { galleryArgs = args; return "gallery"; },
    },
  }));

  const response = await invokeRoute(app, "get", "/");
  assert.equal(response.status, 200);
  assert.equal(response.body, "gallery");
  assert.deepEqual(galleryArgs[8], {
    agentshelf: ["Specs", "UI/UX"],
    homelab: ["Dashboards", "Runbooks"],
  });
});

async function serve(app, fn) {
  const server = app.listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  const { port } = server.address();
  try {
    await fn(`http://127.0.0.1:${port}`);
  } finally {
    await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  }
}

// Exercise a createApp route without opening a socket. Route middleware is unnecessary for
// these cases because the request body is already decoded by the test harness.
async function invokeRoute(app, method, path, { headers = {}, params = {}, query = {}, body } = {}) {
  const route = app._router.stack.find((layer) => layer.route?.path === path && layer.route.methods[method]);
  assert.ok(route, `${method.toUpperCase()} ${path} route exists`);
  const handler = route.route.stack.at(-1).handle;
  const result = { status: 200, headers: {}, body: undefined };
  const res = {
    status(code) { result.status = code; return this; },
    set(name, value) {
      if (typeof name === "object") Object.assign(result.headers, name);
      else result.headers[String(name).toLowerCase()] = value;
      return this;
    },
    send(value) { result.body = value; return this; },
    json(value) { result.body = value; return this; },
    end() { return this; },
    redirect(code, location) { result.status = code; result.headers.location = location; return this; }
  };
  await handler({ headers, params, query, body }, res);
  return result;
}

test("cookie-authenticated portal mutations require the first-party header and same-origin metadata", async () => {
  const seen = [];
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "viewer@acme.test", org: "acme", isAdmin: false }),
    notifications: { recentForViewer: () => [], unreadCount: () => 0, markSeen: (email) => seen.push(email) }
  }));
  await serve(app, async (baseUrl) => {
    const denied = await fetch(`${baseUrl}/notifications/seen`, {
      method: "POST",
      headers: { cookie: "CF_Authorization=viewer", "sec-fetch-site": "cross-site" }
    });
    assert.equal(denied.status, 403);
    assert.deepEqual(seen, []);

    const allowed = await fetch(`${baseUrl}/notifications/seen`, {
      method: "POST",
      headers: {
        cookie: "CF_Authorization=viewer",
        "x-artifact-mutation": "1",
        "sec-fetch-site": "same-origin"
      }
    });
    assert.equal(allowed.status, 200);
    assert.deepEqual(seen, ["viewer@acme.test"]);

    const trustedHeaderDenied = await fetch(`${baseUrl}/notifications/seen`, {
      method: "POST",
      headers: {
        "cf-access-authenticated-user-email": "viewer@acme.test",
        "sec-fetch-site": "same-origin"
      }
    });
    assert.equal(trustedHeaderDenied.status, 403);

    const trustedHeaderAllowed = await fetch(`${baseUrl}/notifications/seen`, {
      method: "POST",
      headers: {
        "cf-access-authenticated-user-email": "viewer@acme.test",
        "x-artifact-mutation": "1",
        "sec-fetch-site": "same-origin"
      }
    });
    assert.equal(trustedHeaderAllowed.status, 200);
    assert.deepEqual(seen, ["viewer@acme.test", "viewer@acme.test"]);

    const bodylessCategory = await fetch(`${baseUrl}/abc123/category`, {
      method: "POST",
      headers: {
        cookie: "CF_Authorization=viewer",
        "x-artifact-mutation": "1",
        "sec-fetch-site": "same-origin"
      }
    });
    assert.equal(bodylessCategory.status, 400);
    assert.deepEqual(await bodylessCategory.json(), { error: "category is required" });

    const mcp = await fetch(`${baseUrl}/mcp`, { method: "POST" });
    assert.equal(mcp.status, 401, "MCP remains outside the portal CSRF gate");
  });
});

test("Access identity fails closed when JWT and explicit header trust are both off", async () => {
  await withIdentityEnv({ ADMIN_EMAILS: "admin@example.test" }, async (identity) => {
    assert.equal(identity.ACCESS_IDENTITY_MODE, "disabled");
    const headers = { "cf-access-authenticated-user-email": "admin@example.test" };
    assert.deepEqual(await identity.resolveViewer({ headers }), { email: null, org: null, isAdmin: false });

    const app = createApp(dependencies({ resolveViewer: identity.resolveViewer }));
    const gallery = await invokeRoute(app, "get", "/", { headers });
    assert.equal(gallery.status, 403);
    assert.equal(gallery.body, "not signed in");
    assert.equal((await invokeRoute(app, "get", "/settings", { headers })).status, 403);
  });
});

test("signed-in viewers can advance only their own notification watermark", async () => {
  const seen = [];
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "viewer@acme.test", org: "acme", isAdmin: false }),
    notifications: { recentForViewer: () => [], unreadCount: () => 0, markSeen: (email) => seen.push(email) }
  }));
  const response = await invokeRoute(app, "post", "/notifications/seen");
  assert.equal(response.status, 200);
  assert.deepEqual(response.body, { ok: true });
  assert.deepEqual(seen, ["viewer@acme.test"]);

  const unsigned = createApp(dependencies({
    resolveViewer: async () => ({ email: null, org: null, isAdmin: false }),
    notifications: { recentForViewer: () => [], unreadCount: () => 0, markSeen: (email) => seen.push(email) }
  }));
  const denied = await invokeRoute(unsigned, "post", "/notifications/seen");
  assert.equal(denied.status, 403);
  assert.deepEqual(seen, ["viewer@acme.test"]);
});

test("human artifact deletion is limited to administrators and recorded owners", async () => {
  async function attempt(viewer, ownerEmail) {
    const deleted = [];
    const artifact = {
      id: "abc123",
      org: "acme",
      title: "Artifact",
      client_id: "publisher",
      owner_email: ownerEmail,
      is_bundle: 0,
    };
    const base = dependencies({ resolveViewer: async () => viewer });
    base.artifacts = {
      ...base.artifacts,
      getArtifactMeta: () => artifact,
      deleteArtifactById: (id) => {
        deleted.push(id);
        return true;
      },
    };
    const response = await invokeRoute(createApp(base), "delete", "/:id", {
      params: { id: artifact.id },
    });
    return { response, deleted };
  }

  const owner = await attempt(
    { email: "OWNER@ACME.TEST", org: "acme", isAdmin: false },
    "owner@acme.test",
  );
  assert.equal(owner.response.status, 200);
  assert.deepEqual(owner.response.body, { id: "abc123", deleted: true });
  assert.deepEqual(owner.deleted, ["abc123"]);

  for (const ownerEmail of ["owner@acme.test", null]) {
    const member = await attempt(
      { email: "member@acme.test", org: "acme", isAdmin: false },
      ownerEmail,
    );
    assert.equal(member.response.status, 403);
    assert.deepEqual(member.response.body, { error: "Forbidden" });
    assert.deepEqual(member.deleted, []);
  }

  const admin = await attempt(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    null,
  );
  assert.equal(admin.response.status, 200);
  assert.deepEqual(admin.deleted, ["abc123"]);
});

test("organization deletion surfaces artifact refusal as an actionable 400", async () => {
  const message = 'Cannot delete organization "acme" while it owns 1 artifact. Move its artifacts to another organization first.';
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    orgs: {
      ...dependencies().orgs,
      remove: () => { throw new Error(message); }
    }
  }));

  const response = await invokeRoute(app, "delete", "/settings/orgs/:name", {
    params: { name: "acme" }
  });
  assert.equal(response.status, 400);
  assert.deepEqual(response.body, { error: message });
});

test("TRUST_ACCESS_HEADERS=1 explicitly restores local-development header identity", async () => {
  await withIdentityEnv(
    { TRUST_ACCESS_HEADERS: "1", ADMIN_EMAILS: "admin@example.test" },
    async (identity) => {
      assert.equal(identity.ACCESS_IDENTITY_MODE, "header-trust");
      const headers = { "cf-access-authenticated-user-email": "admin@example.test" };
      assert.deepEqual(
        await identity.resolveViewer({ headers }),
        { email: "admin@example.test", org: "admin", isAdmin: true }
      );
      const app = createApp(dependencies({ resolveViewer: identity.resolveViewer }));
      assert.equal((await invokeRoute(app, "get", "/", { headers })).status, 200);
      assert.equal((await invokeRoute(app, "get", "/settings", { headers })).status, 200);
    }
  );
});

test("MCP keys and share tokens remain identity-independent in every Access mode", async () => {
  const modes = [
    [{}, "disabled"],
    [{ TRUST_ACCESS_HEADERS: "1" }, "header-trust"],
    [{ CF_ACCESS_TEAM_DOMAIN: "team.cloudflareaccess.com", CF_ACCESS_AUD: "aud" }, "jwt"]
  ];
  for (const [env, expectedMode] of modes) {
    await withIdentityEnv(env, async (identity) => {
      assert.equal(identity.ACCESS_IDENTITY_MODE, expectedMode);
      const app = createApp(dependencies({
        checkPublisherKey: () => ({ ok: true, clientId: "publisher", org: "acme", label: "Agent", role: "author" }),
        handleMcp: async () => ({ jsonrpc: "2.0", id: 1, result: { ok: true } }),
        shares: { resolve: () => ({ artifact_id: "abc123", org: "acme" }) },
        resolveViewer: async () => { throw new Error("identity-independent route resolved viewer"); }
      }));
      const mcp = await invokeRoute(app, "post", "/mcp", {
        headers: { authorization: "Bearer secret" },
        body: { jsonrpc: "2.0", id: 1, method: "ping" }
      });
      assert.equal(mcp.status, 200);
      assert.deepEqual(mcp.body, { jsonrpc: "2.0", id: 1, result: { ok: true } });
      const share = await invokeRoute(app, "get", "/s/:token", { params: { token: "valid" } });
      assert.equal(share.status, 200);
      assert.equal(share.body, "<h1>Artifact</h1>");
    });
  }
});

test("MCP HTTP telemetry records success and failure paths with opaque correlation ids", async () => {
  const logs = [];
  const telemetry = createMcpTelemetry({
    logger: { info(message, fields) { logs.push({ message, fields }); } },
    createRequestId: (() => {
      let sequence = 0;
      return () => `mcp_test_${++sequence}`;
    })()
  });
  const successApp = createApp(dependencies({
    checkPublisherKey: () => ({ ok: true, clientId: "publisher", org: "acme", label: "Agent", role: "author" }),
    validateMcpHttpRequest: () => ({ ok: true, protocolVersion: null, modern: false }),
    handleMcp: async () => ({ jsonrpc: "2.0", id: 1, result: { ok: true } }),
    mcpTelemetry: telemetry
  }));
  await serve(successApp, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: {
        authorization: "Bearer must-not-be-logged",
        "content-type": "application/json"
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" })
    });
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("x-request-id"), "mcp_test_1");
  });

  const failureApp = createApp(dependencies({
    checkPublisherKey: () => ({ ok: false }),
    mcpTelemetry: telemetry
  }));
  await serve(failureApp, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: {
        authorization: "Bearer another-secret",
        "content-type": "application/json"
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list" })
    });
    assert.equal(response.status, 401);
    assert.equal(response.headers.get("x-request-id"), "mcp_test_2");
  });

  await serve(successApp, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/metrics`);
    const metrics = await response.text();
    assert.equal(response.status, 200);
    assert.match(metrics, /operation="listing",method="tools\/list",name="none",outcome="success"/);
    assert.match(metrics, /outcome="authentication_failure"/);
    assert.doesNotMatch(metrics, /must-not-be-logged|another-secret|publisher|acme/);
  });
  assert.equal(logs.length, 2);
  assert.doesNotMatch(JSON.stringify(logs), /must-not-be-logged|another-secret|publisher|acme/);
});

test("REQUIRE_ACCESS_JWT=1 rejects startup readiness without complete JWT configuration", async () => {
  await withIdentityEnv({ REQUIRE_ACCESS_JWT: "1" }, async (identity) => {
    assert.equal(identity.ACCESS_IDENTITY_MODE, "disabled");
    assert.throws(() => identity.assertReady(), /REQUIRE_ACCESS_JWT=1.*CF_ACCESS_TEAM_DOMAIN.*CF_ACCESS_AUD/);
  });
});

test("header-trust refuses a non-loopback bind unless explicitly acknowledged", async () => {
  await withIdentityEnv({ TRUST_ACCESS_HEADERS: "1", LISTEN_HOST: "0.0.0.0" }, async (identity) => {
    assert.equal(identity.ACCESS_IDENTITY_MODE, "header-trust");
    assert.throws(() => identity.assertReady(), /non-loopback bind/);
  });
  await withIdentityEnv({ TRUST_ACCESS_HEADERS: "1", LISTEN_HOST: "127.0.0.1" }, async (identity) => {
    assert.doesNotThrow(() => identity.assertReady());
  });
  await withIdentityEnv({ TRUST_ACCESS_HEADERS: "1", LISTEN_HOST: "0.0.0.0", HEADER_TRUST_ALLOW_INSECURE: "1" }, async (identity) => {
    assert.doesNotThrow(() => identity.assertReady());
  });
});

test("cross-organization artifact reads are concealed as not found", async () => {
  await serve(createApp(dependencies()), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123`);
    assert.equal(response.status, 404);
    assert.equal(await response.text(), "not found");
  });
});

test("public shares serve sandboxed live artifacts while invalid states are the same 404", async () => {
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", is_bundle: 0 };
  let state = "valid";
  const app = createApp(dependencies({
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact },
    shares: { resolve: () => state === "valid" ? { artifact_id: "abc123", org: "acme" } : null, listForArtifact: () => [], revoke: () => false },
    resolveViewer: async () => { throw new Error("public share must not resolve viewer"); }
  }));
  await serve(app, async (baseUrl) => {
    const valid = await fetch(`${baseUrl}/s/token`);
    assert.equal(valid.status, 200);
    assert.match(valid.headers.get("content-security-policy"), /sandbox/);
    assert.equal(valid.headers.get("x-robots-tag"), "noindex");
    assert.doesNotMatch(await valid.text(), /artifact-anchor-bridge/);
    const statuses = [];
    for (const invalid of ["unknown", "expired", "revoked"]) {
      state = invalid;
      const response = await fetch(`${baseUrl}/s/token`);
      statuses.push([response.status, await response.text()]);
    }
    assert.deepEqual(statuses, [[404, "not found"], [404, "not found"], [404, "not found"]]);
  });
});

test("share management requires artifact access and bundle shares guard paths", async () => {
  const bundle = { id: "abc123", org: "acme", title: "Bundle", client_id: "publisher", is_bundle: 1, entry: "index.html" };
  const calls = [];
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    artifacts: {
      ...dependencies().artifacts,
      getArtifactMeta: () => bundle,
      readBundleFile(_id, rel) { return rel === "index.html" || !rel ? { content: "<h1>Entry</h1>", contentType: "text/html; charset=utf-8" } : rel === "assets/site.css" ? { content: "body{}", contentType: "text/css; charset=utf-8" } : null; }
    },
    shares: {
      resolve: () => ({ artifact_id: "abc123", org: "acme" }),
      create(input) { calls.push(input); return { token: "a".repeat(24), expires_at: null }; },
      listForArtifact: () => [],
      revoke: () => true
    },
    publicBase: "https://artifact.test"
  });
  await serve(createApp(base), async (baseUrl) => {
    const created = await fetch(`${baseUrl}/abc123/share`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ expires: "never" }) });
    assert.equal(created.status, 200);
    assert.equal((await created.json()).url, `https://artifact.test/s/${"a".repeat(24)}`);
    const entry = await fetch(`${baseUrl}/s/token/`, { redirect: "manual" });
    assert.equal(entry.status, 200);
    assert.match(entry.headers.get("cache-control") || "", /no-store/); // revocation must be immediate
    assert.equal((await fetch(`${baseUrl}/s/token/assets/site.css`)).status, 200);
    // A missing sub-file 404s. Path-traversal containment lives in the shared readBundleFile
    // (store-tested); the share route reuses it, so `..` escapes are rejected there.
    assert.equal((await fetch(`${baseUrl}/s/token/missing.js`)).status, 404);
  });
  assert.equal(calls[0].createdBy, "member@acme.test");
  // Invariant 3: a cross-org viewer and an unsigned viewer are told the artifact is not there,
  // never that it is there but forbidden — a 403 here would confirm the id exists elsewhere.
  await serve(createApp({ ...base, resolveViewer: async () => ({ email: "other@other.test", org: "other", isAdmin: false }) }), async (baseUrl) => {
    assert.equal((await fetch(`${baseUrl}/abc123/shares`)).status, 404);
  });
  await serve(createApp({ ...base, resolveViewer: async () => ({ email: "", org: "", isAdmin: false }) }), async (baseUrl) => {
    assert.equal((await fetch(`${baseUrl}/abc123/shares`)).status, 404);
  });
});

test("a foreign artifact is indistinguishable from a nonexistent one on every artifact route", async () => {
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", is_bundle: 0, revision: 1 };
  const subordinateReads = [];
  // Same dependencies twice; only whether getArtifactMeta finds a row differs. Every other
  // adapter records the reads that must NOT happen before the access decision.
  const build = (exists, viewer) => {
    const base = dependencies({
      resolveViewer: async () => viewer,
      shares: {
        resolve: () => null,
        create: () => { subordinateReads.push("share.create"); return {}; },
        listForArtifact: () => { subordinateReads.push("shares.list"); return []; },
        revoke: () => { subordinateReads.push("shares.revoke"); return true; }
      },
      feedback: { listForArtifact: () => { subordinateReads.push("feedback.list"); return []; } },
      views: {
        record: () => subordinateReads.push("views.record"),
        countsFor: () => { subordinateReads.push("views.counts"); return null; },
        countsForOrg: () => new Map(),
        viewersFor: () => [],
        topForOrg: () => []
      },
      thumbnails: {
        readThumbnail: async () => { subordinateReads.push("thumbnail.read"); return null; },
        removeArtifact: async () => {},
        placeholder: () => { subordinateReads.push("thumbnail.placeholder"); return Buffer.from("png"); }
      }
    });
    base.artifacts = {
      ...base.artifacts,
      getArtifactMeta: () => exists ? artifact : null,
      readArtifact: () => { subordinateReads.push("artifact.bytes"); return { meta: artifact, html: "<h1>Secret</h1>" }; },
      listRevisions: () => { subordinateReads.push("artifact.history"); return { current: 1, revisions: [] }; },
      deleteArtifactById: () => { subordinateReads.push("artifact.delete"); return true; }
    };
    return createApp(base);
  };

  const probes = [
    ["GET", "/:id"],
    ["GET", "/:id/shares"],
    ["GET", "/:id/history"],
    ["GET", "/:id/feedback"],
    ["GET", "/thumbnails/:id"],
    ["GET", "/raw/:id"],
    ["DELETE", "/:id"],
    ["POST", "/:id/react"],
    ["POST", "/:id/feedback"],
    ["POST", "/:id/category"],
    ["POST", "/:id/share"],
    ["POST", "/:id/visibility"],
    ["POST", "/:id/move"],
    ["POST", "/:id/restore"],
    ["DELETE", "/:id/shares/:token"],
    ["DELETE", "/:id/feedback/:fid"],
    ["POST", "/:id/feedback/:fid/resolve"]
  ];
  // Both unauthorized personas: the signed-out probe used to leak 401-vs-404 and the cross-org
  // probe 403-vs-404. Neither may distinguish an existing foreign id from a nonexistent one.
  const personas = {
    unsigned: { email: null, org: null, isAdmin: false },
    "cross-org": { email: "intruder@other.test", org: "other", isAdmin: false }
  };
  for (const [persona, viewer] of Object.entries(personas)) {
    const foreignApp = build(true, viewer);
    const missingApp = build(false, viewer);
    for (const [method, path] of probes) {
      const params = { id: "abc123", token: "t".repeat(24), fid: "feedback1" };
      const options = { params, body: { hidden: true, category: "x", expires: "never", revision: 1, body: "note" } };
      const foreign = await invokeRoute(foreignApp, method.toLowerCase(), path, options);
      const missing = await invokeRoute(missingApp, method.toLowerCase(), path, options);
      assert.equal(foreign.status, 404, `${persona} ${method} ${path} conceals a foreign artifact`);
      assert.deepEqual(
        { status: foreign.status, body: foreign.body, headers: foreign.headers },
        { status: missing.status, body: missing.body, headers: missing.headers },
        `${persona} ${method} ${path} answers a foreign artifact exactly like a nonexistent one`
      );
    }
  }
  // Concealment is decided before any subordinate read, so timing/side effects leak nothing.
  assert.deepEqual(subordinateReads, []);
});

test("administrators can open artifacts across organizations", async () => {
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true })
  }));
  await serve(app, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123`);
    assert.equal(response.status, 200);
    assert.equal(await response.text(), "shell");
    // Identity-dependent HTML must not be cached (else a stale pre-auth page can be served).
    assert.match(response.headers.get("cache-control") || "", /no-store/);
  });
});

test("hidden direct URLs still render, while visibility mutations require the recorded owner", async () => {
  const artifact = { id: "abc123", org: "acme", owner_email: "member@acme.test", title: "Hidden", client_id: "publisher", is_bundle: 0, hidden: 1 };
  let hidden;
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  });
  base.artifacts = { ...base.artifacts, getArtifactMeta: () => artifact, setHidden(_id, next) { hidden = next; return { ok: true, id: artifact.id, hidden: next }; } };
  await serve(createApp(base), async (baseUrl) => {
    assert.equal((await fetch(`${baseUrl}/abc123`)).status, 200);
    const allowed = await fetch(`${baseUrl}/abc123/visibility`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ hidden: false }) });
    assert.equal(allowed.status, 200);
    assert.equal(hidden, false);
  });

  base.resolveViewer = async () => ({ email: "member@other.test", org: "other", isAdmin: false });
  await serve(createApp(base), async (baseUrl) => {
    // Concealed, not forbidden: the mutation must not confirm the artifact exists elsewhere.
    const denied = await fetch(`${baseUrl}/abc123/visibility`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ hidden: false }) });
    assert.equal(denied.status, 404);
    assert.deepEqual(await denied.json(), { error: "Not found" });
  });
});

test("same-org non-owners cannot change visibility while owners can restore their hidden upload", async () => {
  const artifact = { id: "abc124", org: "acme", owner_email: "owner@acme.test", title: "Owned", client_id: "publisher", is_bundle: 0, hidden: 1 };
  let calls = 0;
  const base = dependencies({ resolveViewer: async () => ({ email: "peer@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = { ...base.artifacts, getArtifactMeta: () => artifact, setHidden() { calls += 1; return { hidden: false }; } };
  await serve(createApp(base), async (baseUrl) => {
    const denied = await fetch(`${baseUrl}/abc124/visibility`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ hidden: false }) });
    assert.equal(denied.status, 403);
    assert.equal(calls, 0);
  });
  base.resolveViewer = async () => ({ email: "OWNER@acme.test", org: "acme", isAdmin: false });
  await serve(createApp(base), async (baseUrl) => {
    const allowed = await fetch(`${baseUrl}/abc124/visibility`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ hidden: false }) });
    assert.equal(allowed.status, 200);
    assert.equal(calls, 1);
  });
});

test("non-admins cannot re-tenant artifacts", async () => {
  let moved = false;
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = { ...base.artifacts, moveArtifactToOrg() { moved = true; } };
  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123/move`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ org: "other" }) });
    assert.equal(response.status, 403);
    assert.equal(moved, false);
  });
});

test("artifact shell records named member views but never records admin views", async () => {
  const calls = [];
  let shellAnalytics;
  let shellViewer;
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    views: {
      record(...args) { calls.push(args); },
      countsFor: () => ({ views: 3, unique_viewers: 2, last_viewed_at: "2026-07-11 12:00:00" }),
      viewersFor: () => [{ email: "audience@acme.test" }],
      countsForOrg: () => new Map(),
      topForOrg: () => []
    },
    pages: {
      ...dependencies().pages,
      shell(_meta, _nav, _reaction, _feedback, analytics, viewer) {
        shellAnalytics = analytics;
        shellViewer = viewer;
        return "shell";
      }
    }
  });
  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123`);
    assert.equal(response.status, 200);
  });
  assert.deepEqual(calls, [["abc123", "acme", "member@acme.test"]]);
  assert.deepEqual(shellAnalytics.viewers, null);
  assert.deepEqual(shellViewer, { email: "member@acme.test", org: "acme", isAdmin: false });

  base.resolveViewer = async () => ({ email: "admin@example.test", org: "admin", isAdmin: true });
  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123`);
    assert.equal(response.status, 200);
  });
  assert.equal(calls.length, 1);
  assert.deepEqual(shellAnalytics.viewers, [{ email: "audience@acme.test" }]);
  assert.deepEqual(shellViewer, { email: "admin@example.test", org: "admin", isAdmin: true });
});

test("raw artifact fetches never record a view", async () => {
  let recorded = 0;
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    views: { record() { recorded += 1; }, countsFor: () => null, countsForOrg: () => new Map(), viewersFor: () => [], topForOrg: () => [] }
  }));
  await serve(app, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/raw/abc123`);
    assert.equal(response.status, 200);
  });
  assert.equal(recorded, 0);
});

test("unsigned artifact reads are concealed as not found", async () => {
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "", org: "", isAdmin: false })
  }));
  await serve(app, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/raw/abc123`);
    assert.equal(response.status, 404);
    assert.equal(await response.text(), "not found");
  });
});

test("publisher-key creation preserves its display label and selected role", async () => {
  let received;
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    keys: {
      list: () => [],
      revoke: () => false,
      create(input) {
        received = input;
        return { ...input, secret: "one-time-secret" };
      }
    }
  }));

  await serve(app, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/settings/keys`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        clientId: "agent-one",
        org: "acme",
        label: "Acme research agent",
        role: "collaborator"
      })
    });
    assert.equal(response.status, 200);
    assert.deepEqual(received, {
      clientId: "agent-one",
      org: "acme",
      label: "Acme research agent",
      role: "collaborator"
    });
    const created = await response.json();
    assert.equal(created.label, "Acme research agent");
    assert.equal(created.role, "collaborator");
  });
});

test("owner management requires an admin and makes backfill an explicit confirmation", async () => {
  const calls = [];
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    keys: {
      list: () => [], create: () => ({}), revoke: () => false,
      setOwner(id, ownerEmail) { calls.push(["set", id, ownerEmail]); return { clientId: id, org: "acme", ownerEmail }; },
      backfillOwner(id, ownerEmail, options) { calls.push(["backfill", id, ownerEmail, options.confirm]); return { clientId: id, org: "acme", ownerEmail, matched: 2, updated: options.confirm ? 2 : 0, confirmed: options.confirm }; }
    }
  }));
  await serve(app, async (baseUrl) => {
    const changed = await fetch(`${baseUrl}/settings/keys/acme-key/owner`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ ownerEmail: "owner@acme.test" }) });
    assert.equal(changed.status, 200);
    assert.deepEqual(await changed.json(), { clientId: "acme-key", org: "acme", ownerEmail: "owner@acme.test" });
    const preview = await fetch(`${baseUrl}/settings/keys/acme-key/owner/backfill`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ ownerEmail: "owner@acme.test" }) });
    assert.deepEqual(await preview.json(), { clientId: "acme-key", org: "acme", ownerEmail: "owner@acme.test", matched: 2, updated: 0, confirmed: false });
    const confirmed = await fetch(`${baseUrl}/settings/keys/acme-key/owner/backfill`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ ownerEmail: "owner@acme.test", confirm: true }) });
    assert.deepEqual(await confirmed.json(), { clientId: "acme-key", org: "acme", ownerEmail: "owner@acme.test", matched: 2, updated: 2, confirmed: true });
  });
  assert.deepEqual(calls, [["set", "acme-key", "owner@acme.test"], ["backfill", "acme-key", "owner@acme.test", false], ["backfill", "acme-key", "owner@acme.test", true]]);
});

test("non-admins cannot create organizations", async () => {
  let created = 0;
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  });
  base.orgs = { ...base.orgs, create() { created += 1; return {}; } };
  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/settings/orgs`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "sneaky" })
    });
    assert.equal(response.status, 403);
    assert.equal(created, 0);
  });
});

test("admins create organizations through the registry", async () => {
  let received;
  const base = dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true })
  });
  base.orgs = {
    ...base.orgs,
    create(input) { received = input; return { name: input.name, label: "", domains: [], categories: [], keyCount: 0 }; }
  };
  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/settings/orgs`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "newco", domain: "newco.test" })
    });
    assert.equal(response.status, 200);
    assert.deepEqual(received, { name: "newco", domain: "newco.test", label: undefined });
    assert.equal((await response.json()).name, "newco");
  });
});

test("explicit email membership routes are admin-only and return normalized values", async () => {
  const calls = [];
  const base = dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true })
  });
  base.orgs = {
    ...base.orgs,
    addEmailMember(org, email) {
      calls.push(["add", org, email]);
      return { org, email: String(email).trim().toLowerCase() };
    },
    removeEmailMember(org, email) {
      calls.push(["remove", org, email]);
      return email === "person@example.com";
    }
  };
  const app = createApp(base);

  const added = await invokeRoute(app, "post", "/settings/orgs/:name/emails", {
    params: { name: "acme" }, body: { email: " Person@Example.com " }
  });
  assert.equal(added.status, 200);
  assert.deepEqual(added.body, { org: "acme", email: "person@example.com" });

  const removed = await invokeRoute(app, "delete", "/settings/orgs/:name/emails/:email", {
    params: { name: "acme", email: "person@example.com" }
  });
  assert.equal(removed.status, 200);
  assert.deepEqual(removed.body, { org: "acme", email: "person@example.com", removed: true });
  assert.deepEqual(calls, [
    ["add", "acme", " Person@Example.com "],
    ["remove", "acme", "person@example.com"]
  ]);
});

test("explicit email membership conflicts return the current owner for remediation", async () => {
  const base = dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true })
  });
  base.orgs = {
    ...base.orgs,
    addEmailMember() { throw new Error('Email "person@example.com" is already mapped to "other".'); }
  };
  const response = await invokeRoute(createApp(base), "post", "/settings/orgs/:name/emails", {
    params: { name: "acme" }, body: { email: "person@example.com" }
  });
  assert.equal(response.status, 400);
  assert.match(response.body.error, /already mapped to "other"/);
});

test("non-admin and cross-org viewers cannot manage explicit email memberships", async () => {
  for (const viewer of [
    { email: "member@acme.test", org: "acme", isAdmin: false },
    { email: "member@other.test", org: "other", isAdmin: false }
  ]) {
    let mutations = 0;
    const base = dependencies({ resolveViewer: async () => viewer });
    base.orgs = {
      ...base.orgs,
      addEmailMember() { mutations += 1; },
      removeEmailMember() { mutations += 1; }
    };
    const app = createApp(base);
    const add = await invokeRoute(app, "post", "/settings/orgs/:name/emails", {
      params: { name: "acme" }, body: { email: "person@example.com" }
    });
    const remove = await invokeRoute(app, "delete", "/settings/orgs/:name/emails/:email", {
      params: { name: "acme", email: "person@example.com" }
    });
    assert.equal(add.status, 403);
    assert.equal(remove.status, 403);
    assert.equal(mutations, 0);
  }
});

test("Settings renders escaped explicit email chips and explains Access policy", () => {
  const html = renderSettings(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [],
    [{ name: "legacy.example", label: "", color: null, domains: [], emails: ["person+tag@example.com", '"><script>alert(1)</script>@example.com'], categories: [], keyCount: 0 }]
  );
  assert.match(html, /Specific emails/);
  assert.match(html, /person\+tag@example\.com/);
  assert.doesNotMatch(html, /<script>alert\(1\)<\/script>/);
  assert.match(html, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
  assert.match(html, /override domain routing/i);
  assert.match(html, /Cloudflare Access Allow policy/i);
  assert.match(html, /Legacy domain-shaped organization/);
  assert.match(html, /data-ui="app-frame"/);
  assert.match(html, /data-ui="nav-artifacts"/);
  assert.match(html, /data-ui="nav-administration"/);
  assert.match(html, /aria-current="page"/);
  assert.match(html, /data-discussion-org="legacy\.example"/);
  assert.match(html, /Discord notification threads/);
  assert.match(html, /type="password"/);
  assert.match(html, /name="botToken"/);
  assert.match(html, /Enable Discord threads for this organization/);
  assert.match(html, /exact canonical artifact URL/);
  assert.doesNotMatch(html, /<span>Gallery<\/span>/);
});

test("legacy same-name domain removal fails loudly through the admin route", async () => {
  const message = 'Cannot remove domain "legacy.example" from organization "legacy.example": implicit domain access would remain. Migrate to a non-domain organization first.';
  const base = dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true })
  });
  base.orgs = {
    ...base.orgs,
    removeDomain() { throw new Error(message); }
  };

  const response = await invokeRoute(
    createApp(base),
    "delete",
    "/settings/orgs/:name/domains/:domain",
    { params: { name: "legacy.example", domain: "legacy.example" } }
  );
  assert.equal(response.status, 400);
  assert.deepEqual(response.body, { error: message });
});

test("issuing a key to an unregistered org is refused", async () => {
  const base = dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true })
  });
  base.orgs = { ...base.orgs, has: () => false, create: () => ({}) };
  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/settings/keys`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ clientId: "x", org: "ghost", label: "" })
    });
    assert.equal(response.status, 400);
    assert.match((await response.json()).error, /Unknown organization/);
  });
});

test("reaction updates reject invalid values without writing them", async () => {
  let writes = 0;
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  });
  base.reactions = {
    ...base.reactions,
    set() {
      writes += 1;
      return { favorite: 0, vote: 0 };
    }
  };

  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123/react`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ favorite: "yes", vote: 4 })
    });
    assert.equal(response.status, 400);
    assert.match((await response.json()).error, /favorite|vote/i);
    assert.equal(writes, 0);
  });
});

test("same-org raw HTML is served with an opaque-origin sandbox", async () => {
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  }));

  await serve(app, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/raw/abc123`);
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-security-policy"), /sandbox/);
    assert.doesNotMatch(response.headers.get("content-security-policy"), /allow-same-origin/);
    assert.equal(await response.text(), "<h1>Artifact</h1>");
  });
});

test("raw and download HTML stay byte-for-byte original while the anchor variant adds only the bridge", async () => {
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  }));

  await serve(app, async (baseUrl) => {
    const raw = await fetch(`${baseUrl}/raw/abc123`);
    const download = await fetch(`${baseUrl}/raw/abc123?download`);
    const anchored = await fetch(`${baseUrl}/raw/abc123?anchor=1`);
    assert.equal(await raw.text(), "<h1>Artifact</h1>");
    assert.equal(await download.text(), "<h1>Artifact</h1>");
    assert.match(await anchored.text(), /artifact-anchor-bridge/);
  });
});

test("every anchored bundle HTML page receives a page-aware bridge", async () => {
  const artifact = { id: "abc123", org: "acme", title: "Bundle", client_id: "publisher", is_bundle: 1, entry: "index.html", revision: 1 };
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = {
    ...base.artifacts,
    getArtifactMeta: () => artifact,
    readBundleFile: (_id, page) => ({ content: `<h1>${page}</h1>`, contentType: "text/html; charset=utf-8" })
  };

  const response = await invokeRoute(createApp(base), "get", "/raw/:id/*", {
    params: { id: "abc123", 0: "pages/two.html" }, query: { anchor: "1" }
  });

  assert.equal(response.status, 200);
  assert.match(String(response.body), /artifact-anchor-bridge/);
  assert.match(String(response.body), /pages\/two\.html/);
});

test("same-org viewers can fetch the current feedback list", async () => {
  const rows = [{
    id: "feedback1", artifact_id: "abc123", body: "Review this", anchor_version: 0,
    anchor_kind: null, anchor_node_id: null, anchor_quote: null
  }];
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    feedback: { listForArtifact: (id) => id === "abc123" ? rows : [] }
  }));

  const response = await invokeRoute(app, "get", "/:id/feedback", { params: { id: "abc123" } });

  assert.equal(response.status, 200);
  assert.equal(response.headers["cache-control"], "no-store");
  assert.deepEqual(response.body, rows);
});

test("feedback lists conceal artifacts from cross-org viewers", async () => {
  let reads = 0;
  const app = createApp(dependencies({
    feedback: { listForArtifact: () => { reads += 1; return []; } }
  }));

  const response = await invokeRoute(app, "get", "/:id/feedback", { params: { id: "abc123" } });

  assert.equal(response.status, 404);
  assert.equal(response.headers["cache-control"], "no-store");
  assert.deepEqual(response.body, { error: "Not found" });
  assert.equal(reads, 0);
});

test("feedback rejects unknown and server-owned request fields without writing", async () => {
  let writes = 0;
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", is_bundle: 0, revision: 7 };
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = { ...base.artifacts, getArtifactMeta: () => artifact };
  base.feedback = {
    listForArtifact: () => [],
    add() { writes += 1; }
  };

  const app = createApp(base);
  for (const [field, value] of Object.entries({
    id: "client-id",
    createdAt: "2026-07-14T12:00:00Z",
    viewer_email: "spoofed@acme.test",
    resolved: true,
    org: "other",
    artifactRevision: 999,
    surprise: "unknown"
  })) {
    const response = await invokeRoute(app, "post", "/:id/feedback", {
      params: { id: "abc123" },
      body: { body: "Pinned", [field]: value }
    });
    assert.equal(response.status, 400, field);
    assert.equal(response.headers["cache-control"], "no-store");
    assert.match(response.body.error, new RegExp(field, "i"));
  }
  assert.equal(writes, 0);
});

test("bundle feedback records a validated anchor page and exposes it on create", async () => {
  let received;
  const artifact = { id: "abc123", org: "acme", title: "Bundle", client_id: "publisher", is_bundle: 1, entry: "index.html", revision: 7 };
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = {
    ...base.artifacts,
    getArtifactMeta: () => artifact,
    readBundleFile: (_id, page) => page === "pages/two.html" ? { content: "<h1>Two</h1>", contentType: "text/html; charset=utf-8" } : null
  };
  base.feedback = {
    listForArtifact: () => [],
    add(input) {
      received = input;
      return {
        id: "feedback-page-2", viewer_email: input.viewerEmail, body: input.body,
        created_at: "2026-07-14", artifact_revision: input.artifactRevision,
        anchor_path: input.anchor.path, anchor_x: input.anchor.x, anchor_y: input.anchor.y,
        anchor_w: null, anchor_h: null, anchor_approx: 0, anchor_page: input.anchorPage,
        parent_id: null
      };
    }
  };

  const response = await invokeRoute(createApp(base), "post", "/:id/feedback", {
    params: { id: "abc123" },
    body: { body: "Pinned on page two", anchor: { path: "body", x: 0.5, y: 0.5 }, anchor_page: "pages/two.html" }
  });

  assert.equal(response.status, 201);
  assert.equal(response.headers["cache-control"], "no-store");
  assert.equal(received.anchorPage, "pages/two.html");
  assert.equal(response.body.anchor_page, "pages/two.html");
});

test("feedback HTTP creates and projects structured v2 anchors without degrading them to legacy", async () => {
  let received;
  const artifact = { id: "abc123", org: "acme", title: "Single", client_id: "publisher", is_bundle: 0, revision: 7 };
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = { ...base.artifacts, getArtifactMeta: () => artifact };
  base.feedback = {
    listForArtifact: () => [],
    add(input) {
      received = input;
      return {
        id: "feedback-v2", viewer_email: input.viewerEmail, body: input.body, created_at: "2026-07-14",
        artifact_revision: input.artifactRevision, parent_id: null,
        anchor_path: input.anchor.path, anchor_x: input.anchor.x, anchor_y: input.anchor.y,
        anchor_w: input.anchor.w, anchor_h: input.anchor.h, anchor_approx: 0, anchor_page: null,
        anchor_kind: input.anchor.kind, anchor_node_id: input.anchor.nodeId, anchor_quote: input.anchor.quote,
        anchor_version: 2
      };
    }
  };
  const anchor = {
    version: 2, kind: "element", path: "main:nth-child(1)", nodeId: "revenue-table",
    quote: "Quarterly revenue", x: 0.25, y: 0.5, w: 0.25, h: 0.2, approx: false
  };
  const response = await invokeRoute(createApp(base), "post", "/:id/feedback", {
    params: { id: "abc123" }, body: { body: "Pinned", anchor }
  });

  assert.equal(response.status, 201);
  assert.deepEqual(received.anchor, anchor);
  assert.deepEqual(
    {
      anchor_version: response.body.anchor_version, anchor_kind: response.body.anchor_kind,
      anchor_node_id: response.body.anchor_node_id, anchor_quote: response.body.anchor_quote
    },
    { anchor_version: 2, anchor_kind: "element", anchor_node_id: "revenue-table", anchor_quote: "Quarterly revenue" }
  );
});

test("bundle anchor pages reject traversal, absolute, missing, and non-HTML paths", async () => {
  const artifact = { id: "abc123", org: "acme", title: "Bundle", client_id: "publisher", is_bundle: 1, entry: "index.html", revision: 7 };
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = {
    ...base.artifacts,
    getArtifactMeta: () => artifact,
    readBundleFile: (_id, page) => page === "styles/site.css" ? { content: "", contentType: "text/css; charset=utf-8" } : null
  };
  base.feedback = { listForArtifact: () => [], add() { throw new Error("invalid page reached feedback store"); } };
  const app = createApp(base);

  for (const anchorPage of [undefined, "../two.html", "pages/../two.html", "/pages/two.html", "C:\\pages\\two.html", "pages/missing.html", "styles/site.css"]) {
    const response = await invokeRoute(app, "post", "/:id/feedback", {
      params: { id: "abc123" },
      body: { body: "Invalid", anchor: { x: 0.5, y: 0.5 }, anchor_page: anchorPage }
    });
    assert.equal(response.status, 400, anchorPage);
    assert.match(response.body.error, /anchor_page/);
  }
});

test("bundle shell marks anchors stale when their recorded page no longer exists", async () => {
  let shellFeedback;
  const artifact = { id: "abc123", org: "acme", title: "Bundle", client_id: "publisher", is_bundle: 1, entry: "index.html", revision: 7 };
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.artifacts = { ...base.artifacts, getArtifactMeta: () => artifact, readBundleFile: () => null };
  base.feedback = { listForArtifact: () => [{ id: "missing-page", anchor_page: "removed.html", anchor_x: 0.5, anchor_y: 0.5 }] };
  base.pages = {
    ...base.pages,
    shell(_meta, _nav, _reaction, rows) { shellFeedback = rows; return "shell"; }
  };

  const response = await invokeRoute(createApp(base), "get", "/:id", { params: { id: "abc123" } });

  assert.equal(response.status, 200);
  assert.equal(shellFeedback[0].anchor_page_stale, true);
});

test("viewer feedback management routes scope feedback to the artifact and enforce own-or-admin results", async () => {
  const calls = [];
  const base = dependencies({ resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }) });
  base.feedback = {
    listForArtifact: () => [],
    getFeedback(id) {
      return id === "foreign" ? { id, artifact_id: "other", org: "acme", viewer_email: "member@acme.test" }
        : { id, artifact_id: "abc123", org: "acme", viewer_email: "member@acme.test" };
    },
    deleteFeedback(id, actor) { calls.push(["delete", id, actor]); return { ok: id !== "blocked", id, reason: id === "blocked" ? "forbidden" : undefined }; },
    resolveByViewer(id, actor) { calls.push(["resolve", id, actor]); return { ok: id !== "blocked", id, reason: id === "blocked" ? "forbidden" : undefined }; }
  };
  const app = createApp(base);
  const deleted = await invokeRoute(app, "delete", "/:id/feedback/:fid", { params: { id: "abc123", fid: "owned" } });
  const foreign = await invokeRoute(app, "delete", "/:id/feedback/:fid", { params: { id: "abc123", fid: "foreign" } });
  const blocked = await invokeRoute(app, "post", "/:id/feedback/:fid/resolve", { params: { id: "abc123", fid: "blocked" } });

  assert.equal(deleted.status, 200);
  assert.equal(foreign.status, 404);
  assert.equal(blocked.status, 403);
  for (const response of [deleted, foreign, blocked]) {
    assert.equal(response.headers["cache-control"], "no-store");
  }
  assert.deepEqual(calls, [
    ["delete", "owned", { viewerEmail: "member@acme.test", isAdmin: false }],
    ["resolve", "blocked", { viewerEmail: "member@acme.test", isAdmin: false }]
  ]);
});

test("bundle assets keep their content type but still receive the opaque-origin sandbox", async () => {
  // An uploaded .svg/.xml executes scripts on direct navigation, so EVERY raw response —
  // not just text/html — must carry the sandbox CSP. The content type is still preserved.
  const artifact = { id: "abc123", org: "acme", title: "Bundle", client_id: "publisher", is_bundle: 1, entry: "index.html" };
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  });
  base.artifacts = {
    ...base.artifacts,
    getArtifactMeta: () => artifact,
    readBundleFile: () => ({ content: Buffer.from("<svg xmlns='http://www.w3.org/2000/svg'><script>0</script></svg>"), contentType: "image/svg+xml" })
  };

  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/raw/abc123/logo.svg`);
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-security-policy"), /sandbox/);
    assert.doesNotMatch(response.headers.get("content-security-policy"), /allow-same-origin/);
    assert.match(response.headers.get("content-type"), /^image\/svg\+xml/);
  });
});

test("bundle HTML responses receive the same opaque-origin sandbox", async () => {
  const artifact = { id: "abc123", org: "acme", title: "Bundle", client_id: "publisher", is_bundle: 1, entry: "index.html" };
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  });
  base.artifacts = {
    ...base.artifacts,
    getArtifactMeta: () => artifact,
    readBundleFile: () => ({ content: Buffer.from("<h1>Bundle</h1>"), contentType: "text/html; charset=utf-8" })
  };

  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/raw/abc123/index.html`);
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-security-policy"), /sandbox/);
    assert.doesNotMatch(response.headers.get("content-security-policy"), /allow-same-origin/);
  });
});

test("valid reaction updates are normalized and persisted", async () => {
  let received;
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false })
  });
  base.reactions = {
    ...base.reactions,
    set(_email, _id, update) {
      received = update;
      return update;
    }
  };

  await serve(createApp(base), async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123/react`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ favorite: true, vote: -1 })
    });
    assert.equal(response.status, 200);
    assert.deepEqual(received, { favorite: 1, vote: -1 });
    assert.deepEqual(await response.json(), received);
  });
});

test("authorized MCP requests retain their JSON-RPC response contract", async () => {
  let context;
  const app = createApp(dependencies({
    checkPublisherKey: () => ({
      ok: true,
      clientId: "publisher",
      org: "acme",
      label: "Agent",
      role: "author",
      ownerEmail: "owner@acme.test",
    }),
    async handleMcp(_payload, auth) {
      context = auth;
      return { jsonrpc: "2.0", id: 7, result: { accepted: true } };
    }
  }));

  await serve(app, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer secret" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 7, method: "ping" })
    });
    assert.equal(response.status, 200);
    // Role and verified owner must survive the handleMcp handoff. The latter is the immutable,
    // server-authenticated snapshot used for member visibility ownership.
    assert.deepEqual(context, {
      clientId: "publisher",
      org: "acme",
      label: "Agent",
      role: "author",
      ownerEmail: "owner@acme.test",
    });
    assert.deepEqual(await response.json(), { jsonrpc: "2.0", id: 7, result: { accepted: true } });
  });
});

test("MCP parser failures preserve the frozen compact HTTP envelope", async () => {
  const app = createApp(dependencies({
    limits: { mcpJson: "1kb" },
    checkPublisherKey: () => ({
      ok: true,
      clientId: "publisher",
      org: "acme",
      label: "Agent",
      role: "author",
      ownerEmail: null,
    }),
  }));

  await serve(app, async (baseUrl) => {
    const malformed = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer secret" },
      body: "{",
    });
    assert.equal(malformed.status, 400);
    assert.deepEqual(await malformed.json(), { error: "invalid JSON" });

    const oversized = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer secret" },
      body: JSON.stringify({ payload: "x".repeat(2048) }),
    });
    assert.equal(oversized.status, 413);
    assert.deepEqual(await oversized.json(), { error: "payload too large" });
  });
});

test("OAuth MCP routes publish resource metadata and return a scoped 403 challenge", async () => {
  const oauth = {
    enabled: true,
    issuer: "https://auth.example.test"
  };
  let handled = 0;
  const app = createApp(dependencies({
    publicBase: "https://artifacts.example.test",
    oauth,
    checkPublisherKey: async () => ({
      ok: true,
      clientId: "reader-service",
      org: "acme",
      label: "Reader service",
      role: "reader",
      scopes: new Set(["artifacts:read"]),
      authType: "oauth"
    }),
    async handleMcp(payload) {
      handled += 1;
      return { jsonrpc: "2.0", id: payload.id, result: { accepted: true } };
    }
  }));

  await serve(app, async (baseUrl) => {
    const metadata = await fetch(`${baseUrl}/.well-known/oauth-protected-resource`);
    assert.equal(metadata.status, 200);
    assert.deepEqual(await metadata.json(), {
      resource: "https://artifacts.example.test/mcp",
      authorization_servers: ["https://auth.example.test"],
      bearer_methods_supported: ["header"],
      scopes_supported: [
        "artifacts:read",
        "artifacts:publish",
        "artifacts:review",
        "artifacts:visibility",
        "artifacts:delete",
        "audit:read",
        "audit:export",
        "audit:global"
      ]
    });

    const denied = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer scoped-token" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 73,
        method: "tools/call",
        params: { name: "publish_artifact", arguments: { html: "<h1>No</h1>" } }
      })
    });
    assert.equal(denied.status, 403);
    assert.match(denied.headers.get("www-authenticate"), /error="insufficient_scope"/);
    assert.match(denied.headers.get("www-authenticate"), /scope="artifacts:publish"/);
    assert.deepEqual(await denied.json(), {
      jsonrpc: "2.0",
      id: 73,
      error: {
        code: -32003,
        message: "insufficient_scope",
        data: { requiredScope: "artifacts:publish" }
      }
    });
    assert.equal(handled, 0);

    const allowed = await fetch(`${baseUrl}/mcp`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer scoped-token" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 74,
        method: "tools/call",
        params: { name: "read_artifact", arguments: { id: "abc123" } }
      })
    });
    assert.equal(allowed.status, 200);
    assert.equal(handled, 1);
  });
});

test("viewer restores pass the updated artifact revision to the notifier seam", async () => {
  const emitted = [];
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", is_bundle: 0, revision: 2 };
  const base = dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    notify: { emit: (...args) => emitted.push(args), test: async () => ({ ok: true }) }
  });
  base.artifacts = {
    ...base.artifacts,
    getArtifactMeta: () => artifact,
    restoreArtifactRevision: () => ({ ok: true, id: artifact.id, revision: 2, restoredFrom: 1, bytes: 10 })
  };

  const response = await invokeRoute(createApp(base), "post", "/:id/restore", {
    params: { id: artifact.id },
    body: { revision: 1 }
  });
  assert.equal(response.status, 200);
  assert.equal(emitted[0][0], "restored");
  assert.deepEqual(emitted[0][3].artifactMeta, artifact);
});

test("preview notifier preserves the admin webhook test action", async () => {
  const webhook = { id: "wh1", org: "acme", url: "https://discord.com/api/webhooks/1/test" };
  const notify = createArtifactPreviewNotifier({
    artifacts: dependencies().artifacts,
    renderer: { enabled: false },
    notify: { emit() {}, test: async (row) => ({ ok: row === webhook }) }
  });
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    webhooks: { get: () => webhook },
    notify
  }));

  const response = await invokeRoute(app, "post", "/settings/orgs/:name/webhooks/:id/test", {
    params: { name: "acme", id: "wh1" }
  });
  assert.equal(response.status, 200);
  assert.deepEqual(response.body, { ok: true });
});

test("discussion parity routes keep strict bodies, safe projections, and owner/admin mutations", async () => {
  const calls = [];
  const safeConnection = { configured: true, label: "Forum", destination: "https://discord.com/…test", lastError: null };
  const safeDiscussion = { mode: "artifact_only", state: "local", enabled: false, connectionConfigured: true, lastError: null };
  const discussions = {
    connection(org) { calls.push(["connection", org]); return safeConnection; },
    configure(input) { calls.push(["configure", input]); return safeConnection; },
    remove(input) { calls.push(["remove", input]); return true; },
    async testConnection(input) { calls.push(["test", input]); return true; },
    status(input) { calls.push(["status", input]); return safeDiscussion; },
    setMode(input) { calls.push(["mode", input]); return { ...safeDiscussion, mode: input.mode, enabled: input.mode === "discord_mirror" }; },
    retry(input) { calls.push(["retry", input]); return safeDiscussion; }
  };
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher" };
  const app = createApp(dependencies({
    audit: {}, discussions,
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact }
  }));

  const connection = await invokeRoute(app, "get", "/settings/orgs/:org/discord-discussion", { params: { org: "acme" } });
  assert.deepEqual(connection.body, safeConnection);
  const malformed = await invokeRoute(app, "put", "/settings/orgs/:org/discord-discussion", {
    params: { org: "acme" }, body: { url: "https://discord.com/api/webhooks/x/y" }
  });
  assert.equal(malformed.status, 400);
  const configured = await invokeRoute(app, "put", "/settings/orgs/:org/discord-discussion", {
    params: { org: "acme" }, body: { url: "https://discord.com/api/webhooks/x/y", label: "Forum" }
  });
  assert.deepEqual(configured.body, safeConnection);
  const status = await invokeRoute(app, "get", "/:id/discussion", { params: { id: artifact.id } });
  assert.deepEqual(status.body, safeDiscussion);
  const mode = await invokeRoute(app, "put", "/:id/discussion", { params: { id: artifact.id }, body: { mode: "discord_mirror" } });
  assert.equal(mode.body.mode, "discord_mirror");
  const retry = await invokeRoute(app, "post", "/:id/discussion/retry", { params: { id: artifact.id }, body: {} });
  assert.deepEqual(retry.body, safeDiscussion);
  assert.ok(calls.some(([name]) => name === "configure"));
  assert.ok(calls.some(([name]) => name === "mode"));
  assert.ok(calls.some(([name]) => name === "retry"));
  const configureCall = calls.find(([name]) => name === "configure")[1];
  assert.match(configureCall.context.requestId, /^[0-9a-f-]{36}$/i, "browser audit correlation is server-generated");
});

test("discussion mutations authorize before malformed JSON is parsed", async () => {
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", owner_email: "owner@acme.test" };
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact }
  }));
  await serve(app, async (baseUrl) => {
    const response = await fetch(`${baseUrl}/abc123/discussion`, {
      method: "PUT", headers: { "content-type": "application/json" }, body: "{ not json"
    });
    assert.equal(response.status, 403, "a non-owner is denied before the malformed body can win");
    assert.deepEqual(await response.json(), { error: "Forbidden" });
  });
});

test("discussion mutation routes reject duplicate JSON members before schema normalization", async () => {
  const calls = [];
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher" };
  const discussions = {
    configure() { calls.push("configure"); return {}; },
    async testConnection() { calls.push("test"); return true; },
    setMode() { calls.push("mode"); return {}; },
    retry() { calls.push("retry"); return {}; }
  };
  const app = createApp(dependencies({
    audit: {},
    discussions,
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact }
  }));
  const requests = [
    [
      "/settings/orgs/acme/discord-discussion",
      "PUT",
      "{\"url\":\"https://discord.com/api/webhooks/x/first\",\"url\":\"https://discord.com/api/webhooks/x/second\",\"label\":\"Forum\"}"
    ],
    ["/settings/orgs/acme/discord-discussion/test", "POST", "{\"probe\":1,\"probe\":2}"],
    ["/abc123/discussion", "PUT", "{\"mode\":\"artifact_only\",\"mo\\u0064e\":\"discord_mirror\"}"],
    ["/abc123/discussion/retry", "POST", "{\"probe\":1,\"probe\":2}"]
  ];

  await serve(app, async (baseUrl) => {
    for (const [path, method, body] of requests) {
      const response = await fetch(`${baseUrl}${path}`, {
        method,
        headers: { "content-type": "application/json" },
        body
      });
      assert.equal(response.status, 400, `${method} ${path} rejects duplicate members`);
    }
  });
  assert.deepEqual(calls, [], "no privileged discussion operation runs after duplicate JSON");
});

test("organization threading routes keep credentials write-only and artifact overrides owner/admin-only", async () => {
  const calls = [];
  const secret = "synthetic-bot-token-never-returned";
  const threading = {
    status(org) {
      calls.push(["status", org]);
      return { configured: true, credential: "configured", fallback: false, enabled: true, recovery: { state: "idle", pending: 0 } };
    },
    save(input) { calls.push(["save", input]); return this.status(input.org); },
    test(input) { calls.push(["test", input]); return { tested: true }; },
    remove(input) { calls.push(["remove", input]); return { removed: true }; },
    queueRecovery(input) { calls.push(["recover", input]); return { queued: true }; },
    artifactStatus(input) { calls.push(["artifactStatus", input]); return { override: "inherit", effectiveMode: "discord_mirror", state: "recovering", actionableError: null }; },
    setArtifactOverride(input) { calls.push(["override", input]); return { override: input.override, effectiveMode: input.override === "artifact_only" ? "artifact_only" : "discord_mirror", state: "local", actionableError: null }; }
  };
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", owner_email: "owner@acme.test" };
  const app = createApp(dependencies({
    audit: {}, organizationThreading: threading,
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact }
  }));

  const saved = await invokeRoute(app, "put", "/settings/orgs/:org/discord-threading", {
    params: { org: "acme" }, body: { botToken: secret, enabled: true }
  });
  assert.equal(saved.status, 200);
  assert.doesNotMatch(JSON.stringify(saved.body), new RegExp(secret));
  assert.equal(calls.find(([name]) => name === "save")[1].botToken, secret);
  const override = await invokeRoute(app, "put", "/:id/discussion/override", {
    params: { id: artifact.id }, body: { override: "artifact_only" }
  });
  assert.deepEqual(override.body, { override: "artifact_only", effectiveMode: "artifact_only", state: "local", actionableError: null });
  assert.ok(calls.some(([name]) => name === "override"));
});

test("threading mutations authorize before body parsing and reject duplicate/unknown fields", async () => {
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", owner_email: "owner@acme.test" };
  const calls = [];
  const threading = { save() { calls.push("save"); }, setArtifactOverride() { calls.push("override"); } };
  const app = createApp(dependencies({
    organizationThreading: threading,
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact }
  }));
  await serve(app, async (baseUrl) => {
    const unauthorized = await fetch(`${baseUrl}/settings/orgs/acme/discord-threading`, {
      method: "PUT", headers: { "content-type": "application/json" }, body: "{ bad json"
    });
    assert.equal(unauthorized.status, 403);
  });
  const admin = createApp(dependencies({
    organizationThreading: threading,
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact }
  }));
  await serve(admin, async (baseUrl) => {
    for (const [path, body] of [
      ["/settings/orgs/acme/discord-threading", '{"botToken":"one","botToken":"two","enabled":true}'],
      ["/settings/orgs/acme/discord-threading", '{"botToken":"one","enabled":true,"extra":true}'],
      ["/abc123/discussion/override", '{"override":"inherit","override":"artifact_only"}']
    ]) {
      const response = await fetch(`${baseUrl}${path}`, { method: "PUT", headers: { "content-type": "application/json" }, body });
      assert.equal(response.status, 400);
    }
  });
  assert.deepEqual(calls, []);
});

test("thumbnail delivery is digest-bound, authenticated, and uses no-store placeholders", async () => {
  const digest = "a".repeat(64);
  const artifact = { id: "abc123", org: "acme", title: "Artifact", client_id: "publisher", is_bundle: 0, body_sha256: digest };
  const image = Buffer.from("persisted png");
  const thumbnails = {
    readThumbnail: async (_meta, requested) => requested === digest ? image : null,
    placeholder: (item) => Buffer.from(item.is_bundle ? "bundle placeholder" : "html placeholder"),
    removeArtifact: async () => {}
  };
  const makeApp = (viewer) => createApp(dependencies({
    artifacts: { ...dependencies().artifacts, getArtifactMeta: () => artifact },
    resolveViewer: async () => viewer,
    thumbnails,
    orgs: { ...dependencies().orgs, colorMap: () => ({ acme: "#123456" }) }
  }));

  for (const viewer of [
    { email: "member@acme.test", org: "acme", isAdmin: false },
    { email: "admin@example.test", org: "admin", isAdmin: true }
  ]) {
    const response = await invokeRoute(makeApp(viewer), "get", "/thumbnails/:id", {
      params: { id: "abc123" }, query: { v: digest }
    });
    assert.equal(response.status, 200);
    assert.equal(response.headers["content-type"], "image/png");
    assert.equal(response.headers["x-content-type-options"], "nosniff");
    assert.equal(response.headers["cache-control"], "private, max-age=31536000, immutable");
    assert.deepEqual(response.body, image);
  }

  const stale = await invokeRoute(makeApp({ email: "member@acme.test", org: "acme", isAdmin: false }), "get", "/thumbnails/:id", {
    params: { id: "abc123" }, query: { v: "b".repeat(64) }
  });
  assert.equal(stale.headers["content-type"], "image/svg+xml; charset=utf-8");
  assert.equal(stale.headers["cache-control"], "no-store");
  assert.deepEqual(stale.body, Buffer.from("html placeholder"));

  for (const viewer of [
    { email: null, org: null, isAdmin: false },
    { email: "other@example.test", org: "other", isAdmin: false }
  ]) {
    const concealed = await invokeRoute(makeApp(viewer), "get", "/thumbnails/:id", {
      params: { id: "abc123" }, query: { v: digest }
    });
    assert.equal(concealed.status, 404);
    assert.equal(concealed.body, "not found");
  }
});

test("artifact deletion triggers best-effort thumbnail cleanup", async () => {
  const removed = [];
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    thumbnails: {
      readThumbnail: async () => null,
      placeholder: () => Buffer.from("placeholder"),
      removeArtifact: async (id) => { removed.push(id); }
    }
  }));
  const response = await invokeRoute(app, "delete", "/:id", { params: { id: "abc123" } });
  assert.equal(response.status, 200);
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(removed, ["abc123"]);
});
