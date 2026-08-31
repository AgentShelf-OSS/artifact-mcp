import { test, expect, publish, api } from "../fixtures.mjs";

test.describe("gallery", () => {
  test("renders and lists an artifact published into the throwaway org", async ({ page, request, publisherKey, org }) => {
    const title = `PW Gallery ${org}`;
    const a = await publish(request, publisherKey, { title, html: "<!doctype html><h1>g</h1>" });
    expect(a.id, "publish should return an id").toBeTruthy();
    await page.goto("/");
    await expect(page.getByText(title)).toBeVisible();
  });

  test("signed-out visitors get the sign-in page, not the gallery", async ({ browser, baseURL }) => {
    const ctx = await browser.newContext({ extraHTTPHeaders: {} }); // newContext INHERITS use.extraHTTPHeaders
    const res = await ctx.request.get(baseURL + "/");
    expect(res.status()).toBe(403);
    await ctx.close();
  });

  test("notifications seen endpoint accepts a post", async ({ request }) => {
    const res = await api(request, "post", "/notifications/seen", {});
    expect([200, 204]).toContain(res.status());
  });

  test("phone-width dark gallery has no horizontal document overflow", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.addInitScript(() => localStorage.setItem("artifact-theme", "dark"));
    await page.goto("/");

    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    await expect(page.getByRole("searchbox")).toBeVisible();
    const dimensions = await page.evaluate(() => ({
      clientWidth: document.documentElement.clientWidth,
      scrollWidth: document.documentElement.scrollWidth,
      targetHeights: Array.from(document.querySelectorAll(".filter-choice, .reset-filters, .layout-toggle button, .sort-control select"), (node) => node.getBoundingClientRect().height),
    }));
    expect(dimensions.scrollWidth).toBeLessThanOrEqual(dimensions.clientWidth + 1);
    expect(Math.min(...dimensions.targetHeights)).toBeGreaterThanOrEqual(44);
  });

  test("library keeps every filter in its top toolbar without a rail", async ({ page }) => {
    await page.goto("/");

    const toolbar = page.locator(".collection-tools");
    await expect(toolbar.getByRole("searchbox", { name: "Search artifacts" })).toBeVisible();
    await expect(toolbar.locator("[data-filter-view='all']")).toBeVisible();
    await expect(toolbar.getByLabel("Sort artifacts")).toBeVisible();
    await expect(toolbar.getByLabel("Collection layout")).toBeVisible();
    await expect(toolbar.getByRole("button", { name: "Reset", exact: true })).toBeVisible();
    await expect(page.locator(".filter-rail")).toHaveCount(0);
  });

  test("reset clears gallery filters, search, and sorting without a page error", async ({ page, request, publisherKey, org }) => {
    const title = `PW Reset ${org}`;
    const otherOrg = `${org}-reset-other`;
    const created = await api(request, "post", "/settings/orgs", { name: otherOrg, label: "Playwright reset organization" });
    expect([200, 201, 400]).toContain(created.status());
    const keyResponse = await api(request, "post", "/settings/keys", { clientId: `pw-reset-${Date.now()}`, org: otherOrg, label: "playwright reset" });
    const key = await keyResponse.json();
    await publish(request, key.secret || key.key, { title: `PW Reset ${otherOrg}`, category: "Specs", html: "<!doctype html><h1>other</h1>" });
    await publish(request, publisherKey, { title, category: "Reports", html: "<!doctype html><h1>reset</h1>" });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto("/");

    await page.locator(`[data-filter-org="${org}"]`).click();
    await page.locator('[data-filter-category="Reports"]').click();
    await page.getByRole("searchbox", { name: "Search artifacts" }).fill("PW Reset");
    await page.getByLabel("Sort artifacts").selectOption("title");
    await page.locator("[data-reset-filters]").click();

    await expect(page.getByRole("searchbox", { name: "Search artifacts" })).toHaveValue("");
    await expect(page.getByLabel("Sort artifacts")).toHaveValue("recent");
    await expect(page.locator('[data-filter-view="all"]')).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator('[data-filter-org="all"]')).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator('[data-filter-category="all"]')).toHaveAttribute("aria-pressed", "true");
    expect(errors).toEqual([]);
  });

  test("tablet toolbar keeps organization and category filters reachable", async ({ page, request, publisherKey, org }) => {
    const otherOrg = `${org}-other`;
    const created = await api(request, "post", "/settings/orgs", { name: otherOrg, label: "Playwright other organization" });
    expect([200, 201, 400]).toContain(created.status());
    const keyResponse = await api(request, "post", "/settings/keys", { clientId: `pw-tablet-${Date.now()}`, org: otherOrg, label: "playwright tablet" });
    const key = await keyResponse.json();
    await publish(request, key.secret || key.key, { title: `PW Tablet ${otherOrg}`, category: "Specs", html: "<!doctype html><h1>other</h1>" });
    await publish(request, publisherKey, { title: `PW Tablet ${org}`, category: "Reports", html: "<!doctype html><h1>tablet</h1>" });
    await page.setViewportSize({ width: 768, height: 1024 });
    await page.goto("/");

    for (const filter of [
      page.locator('[data-filter-org="all"]'),
      page.locator(`[data-filter-org="${org}"]`),
      page.locator('[data-filter-category="all"]'),
      page.locator('[data-filter-category="Reports"]'),
    ]) {
      await filter.scrollIntoViewIfNeeded();
      await expect(filter).toBeVisible();
    }
  });

});
