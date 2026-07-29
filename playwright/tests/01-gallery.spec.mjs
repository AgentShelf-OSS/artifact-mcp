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
    }));
    expect(dimensions.scrollWidth).toBeLessThanOrEqual(dimensions.clientWidth + 1);
  });
});
