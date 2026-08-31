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

  test("viewer chrome keeps menus, focus, and mobile targets predictable", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, { title: `PW Chrome ${org}`, html: "<!doctype html><h1>chrome</h1>" });
    const consoleErrors = [];
    page.on("console", (message) => { if (message.type() === "error") consoleErrors.push(message.text()); });
    page.on("pageerror", (error) => consoleErrors.push(error.message));

    for (const viewport of [
      { width: 1440, height: 900 },
      { width: 768, height: 1024 },
      { width: 390, height: 844 },
    ]) {
      await page.setViewportSize(viewport);
      await page.goto(`/${artifact.id}`);

      const title = page.locator("#vtitle-toggle");
      const titleMenu = page.getByRole("menu", { name: "Artifact overview" });
      const more = page.getByRole("button", { name: "More artifact actions" });
      const moreMenu = page.getByRole("menu", { name: "More artifact actions" });
      const comment = page.getByRole("button", { name: "Comment on a place" });

      await title.press("Enter");
      await expect(titleMenu).toBeVisible();
      await title.press("ArrowDown");
      await expect(page.getByRole("menuitem", { name: "Details" })).toBeFocused();
      await page.keyboard.press("End");
      await expect(page.getByRole("menuitemcheckbox", { name: "Mark as needing work" })).toBeFocused();
      await page.keyboard.press("Escape");
      await expect(title).toBeFocused();

      await more.press("Enter");
      await expect(moreMenu).toBeVisible();
      await more.press("ArrowDown");
      await expect(moreMenu.getByRole("menuitem", { name: "Open raw artifact" })).toBeFocused();
      await page.keyboard.press("Escape");
      await expect(more).toBeFocused();

      await title.press("Enter");
      await title.press("ArrowDown");
      await page.getByRole("menuitem", { name: "Details" }).click();
      await page.getByRole("tab", { name: "History" }).click();
      await page.getByRole("button", { name: "Close inspector" }).click();
      await expect(title).toBeFocused();

      await comment.click();
      await expect(comment).toHaveAttribute("aria-pressed", "true");
      await page.keyboard.press("Escape");
      await expect(comment).toHaveAttribute("aria-pressed", "false");
      await expect(comment).toBeFocused();

      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(viewport.width);
      if (viewport.width <= 760) {
        for (const target of [title, page.getByRole("link", { name: "Back to artifact library" }), page.getByRole("button", { name: "Save to favorites" }), comment, page.getByRole("button", { name: "Share" }), more]) {
          const box = await target.boundingBox();
          expect(box?.width).toBeGreaterThanOrEqual(44);
          expect(box?.height).toBeGreaterThanOrEqual(44);
        }
      }
    }

    expect(consoleErrors).toEqual([]);
  });

  test("anchored comment composer keeps a v2 draft separate from prompt copy", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, { title: `PW Anchor ${org}`, html: "<!doctype html><main><p>anchor target</p></main>" });
    const consoleErrors = [];
    page.on("console", (message) => { if (message.type() === "error") consoleErrors.push(message.text()); });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`/${artifact.id}`);
    const frame = page.frameLocator("#vframe");
    await frame.locator("body").evaluate(() => parent.postMessage({ type: "anchor:ready" }, "*"));
    await page.getByRole("button", { name: "Comment on a place" }).click();
    await frame.locator("body").evaluate(() => parent.postMessage({ type: "anchor:picked", version: 2, kind: "point", path: "main > p", nodeId: "target", quote: "anchor target", x: 0.2, y: 0.3, approx: false }, "*"));
    const composer = page.locator("#vanchor-composer");
    await expect(composer).toBeVisible();
    await composer.getByLabel("Anchored feedback").fill("Please revise the target copy.");
    await expect(composer.getByRole("button", { name: "Copy prompt" })).toBeVisible();
    for (const target of [composer.getByRole("button", { name: "Copy prompt" }), composer.getByRole("button", { name: "Add comment" }), composer.getByRole("button", { name: "Cancel anchored comment" })]) {
      const box = await target.boundingBox();
      expect(box?.width).toBeGreaterThanOrEqual(44);
      expect(box?.height).toBeGreaterThanOrEqual(44);
    }
    await composer.getByRole("button", { name: "Copy prompt" }).click();
    await expect(composer).toBeVisible();
    await expect(composer.getByLabel("Anchored feedback")).toHaveValue("Please revise the target copy.");
    await composer.getByRole("button", { name: "Add comment" }).click();
    await expect(composer.getByText(/Saved feedback/)).toBeVisible();
    await expect(composer.getByRole("button", { name: "Saved" })).toBeDisabled();
    await expect(composer.getByLabel("Anchored feedback")).toHaveValue("Please revise the target copy.");
    await page.keyboard.press("Escape");
    await expect(composer).toBeHidden();
    await expect(page.getByRole("button", { name: "Comment on a place" })).toBeFocused();
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);
    expect(consoleErrors).toEqual([]);
  });

  test("anchored composer uses bridge pixels and stays clamped at desktop, tablet, and mobile sizes", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, { title: `PW Anchor placement ${org}`, html: "<!doctype html><main><p>anchor target</p></main>" });
    const consoleErrors = [];
    page.on("console", (message) => { if (message.type() === "error") consoleErrors.push(message.text()); });
    for (const viewport of [{ width: 1440, height: 900 }, { width: 768, height: 1024 }, { width: 390, height: 844 }]) {
      await page.setViewportSize(viewport);
      await page.goto(`/${artifact.id}`);
      const frame = page.frameLocator("#vframe");
      await frame.locator("body").evaluate(() => parent.postMessage({ type: "anchor:ready" }, "*"));
      await page.getByRole("button", { name: "Comment on a place" }).click();
      await frame.locator("body").evaluate(() => parent.postMessage({ type: "anchor:picked", version: 2, kind: "region", path: "main", nodeId: "near-edge", quote: "anchor target", x: 0.94, y: 0.82, w: 0.04, h: 0.05 }, "*"));
      const composer = page.locator("#vanchor-composer");
      await expect(composer).toBeVisible();
      const box = await composer.boundingBox();
      expect(box?.x).toBeGreaterThanOrEqual(0);
      expect((box?.x || 0) + (box?.width || 0)).toBeLessThanOrEqual(viewport.width);
      expect((box?.y || 0) + (box?.height || 0)).toBeLessThanOrEqual(viewport.height);
      if (viewport.width <= 760) expect((box?.y || 0) + (box?.height || 0)).toBeGreaterThan(viewport.height - 6);
      await page.keyboard.press("Escape");
    }
    expect(consoleErrors).toEqual([]);
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
    await page.locator("#vtitle-toggle").click();
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
    await ownerPage.locator("#vtitle-toggle").click();
    await ownerPage.getByRole("button", { name: "Details" }).click();
    await expect(ownerPage.locator("#vdiscussion-actions")).toBeAttached();
    await ownerContext.close();

    const context = await browser.newContext({ extraHTTPHeaders: { "Cf-Access-Authenticated-User-Email": member } });
    const page = await context.newPage();
    await page.goto(`${baseURL}/${artifact.id}`);
    await page.locator("#vtitle-toggle").click();
    await page.getByRole("button", { name: "Details" }).click();
    await expect(page.locator("#vdiscussion")).toBeVisible();
    await expect(page.locator("#vdiscussion-actions button")).toHaveCount(0);
    await context.close();
  });
});
