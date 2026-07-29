// Drives two RUNNING instances (prod's image and the Rust build) rather than booting servers, so
// the code under test is exactly what ships.
//
// SAFETY: every test runs inside a throwaway organization named `pwtest-<runid>`, created at setup
// and deleted at teardown. Artifacts, keys, categories, shares, and feedback all live under that
// organization. Point the harness only at isolated release-candidate instances.
import { test as base } from "@playwright/test";

const ADMIN = process.env.PW_ADMIN_EMAIL || "admin@example.test";

export const adminHeaders = { "Cf-Access-Authenticated-User-Email": ADMIN };

/** Unique per run so parallel/repeat runs never collide, and leftovers are identifiable. */
export function runId() {
  return process.env.PW_RUN_ID;
}

export async function api(request, method, path, body, headers = {}) {
  const opts = { headers: { ...adminHeaders, ...headers } };
  if (body !== undefined) {
    opts.headers["content-type"] = "application/json";
    opts.data = body;
  }
  return request[method](path, opts);
}

/** Publishes an artifact into the throwaway org using a publisher key minted for it. */
export async function publish(request, key, { title, html, category }) {
  const res = await request.post("/mcp", {
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    data: {
      jsonrpc: "2.0",
      id: 1,
      method: "tools/call",
      params: {
        name: "publish_artifact",
        arguments: { title, description: "playwright fixture", html, ...(category ? { category } : {}) },
      },
    },
  });
  const json = await res.json();
  const text = json?.result?.content?.[0]?.text ?? "{}";
  return JSON.parse(text);
}

import { randomUUID } from "node:crypto";

export const test = base.extend({
  org: async ({}, use) => { await use(`pwtest-${runId()}`); },

  // Test-scoped: worker fixtures cannot depend on the built-in `request`/`baseURL`, and minting a
  // key is cheap. Each gets a unique client_id so repeated creation never collides.
  publisherKey: async ({ request, org }, use) => {
    // A module-level counter resets per spec file, so every file asked for "-1" and got
    // "already exists". Randomise instead.
    const clientId = `pwkey-${runId()}-${randomUUID().slice(0, 8)}`;
    const res = await request.post("/settings/keys", {
      headers: { ...adminHeaders, "content-type": "application/json" },
      data: { clientId, org, label: "playwright" },
    });
    const body = await res.json().catch(() => ({}));
    if (!body.secret && !body.key) throw new Error(`publisher key not minted: ${res.status()} ${JSON.stringify(body).slice(0, 200)}`);
    await use(body.secret || body.key);
  },
});

export const expect = test.expect;
