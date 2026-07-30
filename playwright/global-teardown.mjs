// Removes the throwaway org from BOTH instances. Deleting an org cascades its domains, email
// members, categories and webhooks; artifacts published into it are removed first.
import { request as pwRequest } from "@playwright/test";

const ADMIN = process.env.PW_ADMIN_EMAIL || "admin@example.test";

export default async function globalTeardown() {
  const org = `pwtest-${process.env.PW_RUN_ID}`;
  for (const base of [process.env.PW_NODE_URL, process.env.PW_RUST_URL].filter(Boolean)) {
    const ctx = await pwRequest.newContext({
      baseURL: base,
      extraHTTPHeaders: {
        "Cf-Access-Authenticated-User-Email": ADMIN,
        "X-Artifact-Mutation": "1",
        "Sec-Fetch-Site": "same-origin"
      },
    });
    try {
      // Delete artifacts belonging to the throwaway org, then the org itself.
      const listed = await ctx.get("/");
      const html = listed.ok() ? await listed.text() : "";
      const ids = [...html.matchAll(/data-artifact-id="([^"]+)"[^>]*data-org="([^"]+)"/g)]
        .filter((m) => m[2] === org)
        .map((m) => m[1]);
      for (const id of ids) await ctx.delete(`/${id}`);
      await ctx.delete(`/settings/orgs/${encodeURIComponent(org)}`);
      console.log(`[teardown] removed ${org} (${ids.length} artifact(s)) on ${base}`);
    } catch (error) {
      console.log(`[teardown] WARNING on ${base}: ${error.message}`);
    }
    await ctx.dispose();
  }
}
