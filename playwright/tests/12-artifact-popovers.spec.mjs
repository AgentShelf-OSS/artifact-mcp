import { test, expect, publish, api } from "../fixtures.mjs";

async function assertInsideViewport(page, menu) {
  const box = await menu.boundingBox();
  const viewport = page.viewportSize();
  expect(box).toBeTruthy();
  expect(box.x).toBeGreaterThanOrEqual(0);
  expect(box.y).toBeGreaterThanOrEqual(0);
  expect(box.x + box.width).toBeLessThanOrEqual(viewport.width);
  expect(box.y + box.height).toBeLessThanOrEqual(viewport.height);
  expect(await menu.evaluate((node) => node.scrollHeight > node.clientHeight || getComputedStyle(node).overflowY === "auto")).toBeTruthy();
}

async function openMenu(page, card) {
  const trigger = card.locator('[data-action="more"]');
  await trigger.click();
  const menu = card.locator('.card-menu:not([hidden])');
  await expect(menu).toBeVisible();
  return { menu, trigger };
}

test.describe("artifact action popovers", () => {
  test("keeps the main artifact action menu inside the viewport and closes with Escape", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, { title: `Popover main ${org}`, html: "<!doctype html><h1>Popover</h1>" });
    await page.setViewportSize({ width: 390, height: 520 });
    await page.goto(`/?libraryView=all&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    const card = page.locator(`#artifact-grid .card[data-id="${artifact.id}"]`);
    const { menu, trigger } = await openMenu(page, card);
    await assertInsideViewport(page, menu);
    await expect(menu.locator('option[value="__create_category__"]')).toHaveCount(1);
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect(trigger).toBeFocused();
  });

  for (const view of ["reel", "ribbons"]) {
    test(`keeps the ${view} clone action menu inside the viewport`, async ({ page, request, publisherKey, org }) => {
      const artifact = await publish(request, publisherKey, { title: `Popover ${view} ${org}`, html: "<!doctype html><h1>Popover</h1>" });
      const created = await api(request, "post", "/collections", { org, name: `Popover ${view} folder`, artifactIds: [artifact.id] });
      expect(created.ok()).toBeTruthy();
      await page.setViewportSize({ width: 390, height: 520 });
      await page.goto(`/?libraryView=${view}&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
      if (view === "reel") {
        const folder = page.locator("[data-collection-peek]").filter({ hasText: `Popover ${view} folder` });
        await folder.hover();
      }
      const card = page.locator(".collection-reel-card .card, .collection-ribbon-card .card").filter({ hasText: `Popover ${view} ${org}` }).first();
      const { menu, trigger } = await openMenu(page, card);
      await assertInsideViewport(page, menu);
      await expect(menu.locator('option[value="__create_category__"]')).toHaveCount(1);
      await page.keyboard.press("Escape");
      await expect(menu).toBeHidden();
      await expect(trigger).toBeFocused();
    });
  }
});
