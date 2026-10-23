// Creates the throwaway org on the instance under test before any test runs.
import { request as pwRequest } from "@playwright/test";

const ADMIN = process.env.PW_ADMIN_EMAIL || "admin@example.test";
const BASE_URL = process.env.PW_RUST_URL || "http://127.0.0.1:3483";

export default async function globalSetup() {
  const runId = process.env.PW_RUN_ID || String(Date.now()).slice(-6);
  process.env.PW_RUN_ID = runId;
  const org = `pwtest-${runId}`;
  const ctx = await pwRequest.newContext({
    baseURL: BASE_URL,
    extraHTTPHeaders: {
      "Cf-Access-Authenticated-User-Email": ADMIN,
      "X-Artifact-Mutation": "1",
      "Sec-Fetch-Site": "same-origin"
    },
  });
  const res = await ctx.post("/settings/orgs", {
    headers: { "content-type": "application/json" },
    data: { name: org, label: "Playwright Test" },
  });
  if (!res.ok() && res.status() !== 400) {
    throw new Error(`could not create ${org} on ${BASE_URL}: ${res.status()} ${await res.text()}`);
  }
  await ctx.dispose();
  console.log(`[setup] throwaway org: ${org}`);
}
