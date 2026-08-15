import test from "node:test";
import assert from "node:assert/strict";
import { createApp } from "../lib/app.js";

function dependencies(overrides = {}) {
  return {
    checkPublisherKey: () => ({ ok: false }),
    handleMcp: async () => null,
    resolveViewer: async () => ({ email: "admin@example.test", org: "admin", isAdmin: true }),
    artifacts: {},
    keys: { list: () => [], create: () => ({}), revoke: () => false },
    orgs: { list: () => [], names: () => [], has: () => true, colorMap: () => ({}) },
    reactions: { get: () => ({}), set: () => ({}), forViewer: () => new Map(), sentiment: () => new Map() },
    feedback: { listForArtifact: () => [] },
    pages: { settings: () => "settings" },
    logger: { info() {}, error() {} },
    ...overrides
  };
}

async function invokePatch(app, body) {
  const route = app._router.stack.find((layer) =>
    layer.route?.path === "/settings/keys/:id" && layer.route.methods.patch
  );
  assert.ok(route, "PATCH /settings/keys/:id route exists");
  const handler = route.route.stack.at(-1).handle;
  const result = { status: 200, body: undefined };
  const response = {
    status(code) { result.status = code; return this; },
    json(value) { result.body = value; return this; }
  };
  await handler({ params: { id: "existing-key" }, body }, response);
  return result;
}

test("publisher-key editing delegates only mutable metadata and preserves identity fields", async () => {
  const calls = [];
  const app = createApp(dependencies({
    keys: {
      list: () => [], create: () => ({}), revoke: () => false,
      update(id, input) {
        calls.push([id, input]);
        return { clientId: id, org: "acme", ...input, ownerEmail: input.ownerEmail.toLowerCase() };
      }
    }
  }));
  const result = await invokePatch(app, {
    clientId: "attempted-rename",
    org: "other",
    label: "Alex",
    role: "collaborator",
    ownerEmail: "ALEX@ACME.TEST"
  });
  assert.equal(result.status, 200);
  assert.deepEqual(result.body, {
    clientId: "existing-key",
    org: "acme",
    label: "Alex",
    role: "collaborator",
    ownerEmail: "alex@acme.test"
  });
  assert.deepEqual(calls, [["existing-key", {
    label: "Alex",
    role: "collaborator",
    ownerEmail: "ALEX@ACME.TEST"
  }]]);
});

test("publisher-key editing remains admin-only", async () => {
  let updates = 0;
  const app = createApp(dependencies({
    resolveViewer: async () => ({ email: "member@acme.test", org: "acme", isAdmin: false }),
    keys: {
      list: () => [], create: () => ({}), revoke: () => false,
      update() { updates += 1; return null; }
    }
  }));
  const result = await invokePatch(app, { label: "Nope", role: "author", ownerEmail: "" });
  assert.equal(result.status, 403);
  assert.equal(updates, 0);
});
