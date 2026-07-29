import { test, expect, publish } from "../fixtures.mjs";

test.describe("reactions", () => {
  test("heart fires a request and flips pressed state", async ({ page, request, publisherKey, org }) => {
    // Regression guard for the truncated shell.js: the script was served faithfully but no listener
    // attached, so clicking did nothing. Assert the click has an EFFECT.
    const a = await publish(request, publisherKey, { title: `PW React ${org}`, html: "<!doctype html><h1>r</h1>" });
    await page.goto(`/${a.id}`);
    const fav = page.locator(".vreact.fav");
    await expect(fav).toHaveAttribute("aria-pressed", "false");
    const [res] = await Promise.all([
      page.waitForResponse((r) => r.url().includes(`/${a.id}/react`) && r.request().method() === "POST"),
      fav.click(),
    ]);
    expect(res.status()).toBe(200);
    await expect(fav).toHaveAttribute("aria-pressed", "true");
  });

  test("upvote and downvote toggle", async ({ page, request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Vote ${org}`, html: "<!doctype html><h1>v</h1>" });
    await page.goto(`/${a.id}`);
    const up = page.locator(".vreact.up");
    await Promise.all([
      page.waitForResponse((r) => r.url().includes(`/${a.id}/react`)),
      up.click(),
    ]);
    await expect(up).toHaveAttribute("aria-pressed", "true");
    const down = page.locator(".vreact.down");
    await Promise.all([
      page.waitForResponse((r) => r.url().includes(`/${a.id}/react`)),
      down.click(),
    ]);
    await expect(down).toHaveAttribute("aria-pressed", "true");
    await expect(up).toHaveAttribute("aria-pressed", "false");
  });

  test("reaction persists across reload", async ({ page, request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Persist ${org}`, html: "<!doctype html><h1>p</h1>" });
    await page.goto(`/${a.id}`);
    const fav = page.locator(".vreact.fav");
    await Promise.all([page.waitForResponse((r) => r.url().includes("/react")), fav.click()]);
    await page.reload();
    await expect(page.locator(".vreact.fav")).toHaveAttribute("aria-pressed", "true");
  });
});
