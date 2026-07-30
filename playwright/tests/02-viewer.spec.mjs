import { test, expect, publish, api } from "../fixtures.mjs";

test.describe("artifact viewer", () => {
  test("shell renders, iframe is sandboxed without allow-same-origin", async ({ page, request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Shell ${org}`, html: "<!doctype html><h1>shell</h1>" });
    await page.goto(`/${a.id}`);
    const frame = page.locator("#vframe");
    await expect(frame).toBeVisible();
    const sandbox = await frame.getAttribute("sandbox");
    expect(sandbox, "iframe must be sandboxed").toBeTruthy();
    expect(sandbox).not.toContain("allow-same-origin");
  });

  test("raw delivery carries the CSP sandbox and never allow-same-origin", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Raw ${org}`, html: "<!doctype html><h1>raw</h1>" });
    const res = await request.get(`/raw/${a.id}`);
    expect(res.status()).toBe(200);
    const csp = res.headers()["content-security-policy"] || "";
    expect(csp).toContain("sandbox");
    expect(csp).not.toContain("allow-same-origin");
  });

  test("download returns an attachment", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Dl ${org}`, html: "<!doctype html><h1>dl</h1>" });
    const res = await request.get(`/raw/${a.id}?download=1`);
    expect(res.status()).toBe(200);
  });

  test("history lists revisions after an update", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Hist ${org}`, html: "<!doctype html><h1>v1</h1>" });
    const upd = await request.post("/mcp", {
      headers: { authorization: `Bearer ${publisherKey}`, "content-type": "application/json" },
      data: { jsonrpc: "2.0", id: 1, method: "tools/call",
        params: { name: "update_artifact", arguments: { id: a.id, html: "<!doctype html><h1>v2</h1>" } } },
    });
    expect(upd.ok()).toBeTruthy();
    const hist = await request.get(`/${a.id}/history`);
    expect(hist.status()).toBe(200);
  });

  test("discussion details fail safely on Node and expose guarded Rust policy controls without an untrusted thread link", async ({ page, request, publisherKey, org }, testInfo) => {
    const artifact = await publish(request, publisherKey, { title: `PW Discussion ${org}`, html: "<!doctype html><h1>discussion</h1>" });
    await page.goto(`/${artifact.id}`);
    await page.locator("#vmore-toggle").click();
    await page.getByRole("button", { name: "Details" }).click();
    await expect(page.getByRole("link", { name: /Open Thread/i })).toHaveCount(0);

    if (testInfo.project.name === "node") {
      await expect(page.locator("#vdiscussion-state")).toHaveText("Status unavailable");
      await expect(page.locator("#vdiscussion-actions button")).toHaveCount(0);
      await expect(page.getByText(/Artifact content and feedback remain available/i)).toBeVisible();
      return;
    }

    await expect(page.locator("#vdiscussion-state")).toHaveText("Artifact MCP only");
    await expect(page.getByRole("button", { name: "Keep discussion in Artifact MCP" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Enable two-way Discord sync" })).toBeVisible();
    await expect(page.getByText(/Artifact MCP remains canonical/i)).toBeVisible();

    await page.getByRole("button", { name: "Keep discussion in Artifact MCP" }).click();
    await expect(page.locator("#vdiscussion-state")).toHaveText("Artifact MCP only");
    await expect(page.getByRole("button", { name: "Use organization default" })).toBeFocused();
  });

  test("an artifact owner can manage discussion status while a same-org non-owner cannot", async ({ browser, baseURL, request, org }) => {
    const owner = "discussion-owner@example.test";
    const member = "discussion-member@example.test";
    for (const email of [owner, member]) {
      const added = await api(request, "post", `/settings/orgs/${encodeURIComponent(org)}/emails`, { email });
      expect(added.status(), await added.text()).toBe(200);
    }
    const key = await api(request, "post", "/settings/keys", {
      clientId: `pw-discussion-owner-${Date.now()}`,
      org,
      label: "discussion owner",
      ownerEmail: owner,
    });
    expect(key.status(), await key.text()).toBe(200);
    const secret = (await key.json()).secret;
    const artifact = await publish(request, secret, { title: `PW Discussion owner ${org}`, html: "<!doctype html><h1>owner</h1>" });
    const ownerContext = await browser.newContext({ extraHTTPHeaders: { "Cf-Access-Authenticated-User-Email": owner } });
    const ownerPage = await ownerContext.newPage();
    await ownerPage.goto(`${baseURL}/${artifact.id}`);
    await ownerPage.locator("#vmore-toggle").click();
    await ownerPage.getByRole("button", { name: "Details" }).click();
    await expect(ownerPage.locator("#vdiscussion-actions")).toBeAttached();
    await ownerContext.close();

    const context = await browser.newContext({ extraHTTPHeaders: { "Cf-Access-Authenticated-User-Email": member } });
    const page = await context.newPage();
    await page.goto(`${baseURL}/${artifact.id}`);
    await page.locator("#vmore-toggle").click();
    await page.getByRole("button", { name: "Details" }).click();
    await expect(page.locator("#vdiscussion")).toBeVisible();
    await expect(page.locator("#vdiscussion-actions button")).toHaveCount(0);
    await context.close();
  });
});
