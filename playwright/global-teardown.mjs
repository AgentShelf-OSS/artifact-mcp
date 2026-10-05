// Removes the throwaway org from BOTH instances. Deleting an org cascades its domains, email
// members, categories and webhooks; artifacts published into it are removed first.
import { request as pwRequest } from "@playwright/test";

const ADMIN = process.env.PW_ADMIN_EMAIL || "admin@example.test";

export default async function globalTeardown() {
  const org = `pwtest-${process.env.PW_RUN_ID}`;
  const errors = [];
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
      // Read canonical card markup, not the embedded scripts or preview clones.
      const listed = await ctx.get("/");
      if (!listed.ok()) throw new Error(`could not list QA artifacts: ${listed.status()}`);
      const html = await listed.text();
      const orgOptions = html.match(/<select id="org-filter"[^>]*>([\s\S]*?)<\/select>/)?.[1] || "";
      const existingOrgs = new Set([...orgOptions.matchAll(/<option value="([^"]+)"/g)].map((match) => match[1]));
      const ownedOrgs = new Set([org, `${org}-other`, `${org}-reset-other`, org.replace(/^pwtest-/, "pwcat-")]);
      const cards = [...html.split("</main>")[0].matchAll(/<article\b[^>]*class="card\b[^\"]*"[^>]*>/g)];
      const ids = cards.filter(([tag]) => ownedOrgs.has(tag.match(/\bdata-org="([^"]+)"/)?.[1]))
        .map(([tag]) => tag.match(/\bdata-id="([^"]+)"/)?.[1]).filter(Boolean);
      for (const id of ids) {
        const removed = await ctx.delete(`/${id}`);
        if (!removed.ok()) throw new Error(`could not delete QA artifact: ${removed.status()}`);
      }
      for (const name of ownedOrgs) {
        if (!existingOrgs.has(name)) continue;
        const removed = await ctx.delete(`/settings/orgs/${encodeURIComponent(name)}`);
        if (!removed.ok() && removed.status() !== 404) throw new Error(`could not delete ${name}: ${removed.status()}`);
      }
      console.log(`[teardown] removed ${org} and companion organizations (${ids.length} artifact(s)) on ${base}`);
    } catch (error) {
      console.log(`[teardown] WARNING on ${base}: ${error.message}`);
      errors.push(error.message);
    }
    await ctx.dispose();
  }
  if (errors.length) throw new Error(`QA fixture cleanup failed: ${errors.join("; ")}`);
}
