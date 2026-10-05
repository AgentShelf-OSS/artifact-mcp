import { test, expect, publish, api } from "../fixtures.mjs";

const html = (title) => `<!doctype html><meta charset="utf-8"><main><h1>${title}</h1><p>Gesture QA artifact.</p></main>`;

async function projection(request, org) {
  const response = await api(request, "get", `/collections?org=${encodeURIComponent(org)}&include=projection`);
  expect(response.ok()).toBeTruthy();
  return response.json();
}

test.describe("collection gestures", () => {
  test("real pointer drag adds a reference and preserves the artifact body", async ({ page, request, publisherKey, org }, testInfo) => {
    const artifact = await publish(request, publisherKey, { title: `Gesture drag ${org}`, html: html("Drag source") });
    const before = await (await request.get(`/raw/${artifact.id}`)).text();
    const source = await api(request, "post", "/collections", { org, name: `Gesture source ${org}`, artifactIds: [artifact.id] });
    const target = await api(request, "post", "/collections", { org, name: `Gesture target ${org}` });
    expect(source.ok() && target.ok()).toBeTruthy();
    const sourceBody = await source.json();
    const targetBody = await target.json();
    const preferences = await api(request, "put", `/gallery/preferences?org=${encodeURIComponent(org)}`, {
      view: "reel", previewSize: "compact", artifactLayout: "grid",
      collectionOrderByOrg: { [org]: [sourceBody.id, targetBody.id] }, collapsedCollectionIdsByOrg: {},
    });
    expect(preferences.ok()).toBeTruthy();
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    const sourceFace = page.locator("[data-collection-peek]").filter({ hasText: `Gesture source ${org}` });
    await sourceFace.hover();
    const card = page.locator(`.collection-reel-card [data-collection-artifact="${artifact.id}"]`);
    const targetFace = page.locator(`[data-collection-id="${targetBody.id}"]`);
    await expect(card).toBeVisible();
    const preview = card.locator('.preview a[aria-label^="Open "]');
    const sourceBox = await preview.boundingBox();
    const destinationBox = await targetFace.boundingBox();
    expect(destinationBox.x).toBeGreaterThanOrEqual(0);
    expect(destinationBox.x + destinationBox.width).toBeLessThanOrEqual(1440);
    expect(await targetFace.evaluate(node => {
      const box = node.getBoundingClientRect();
      return document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2)?.closest("[data-collection-id]")?.dataset.collectionId;
    })).toBe(targetBody.id);
    await page.mouse.move(sourceBox.x + sourceBox.width / 2, sourceBox.y + sourceBox.height / 2);
    await page.mouse.down();
    await page.mouse.move(sourceBox.x + sourceBox.width / 2 + 10, sourceBox.y + sourceBox.height / 2 + 10, {steps: 3});
    await page.mouse.move(destinationBox.x + destinationBox.width / 2, destinationBox.y + destinationBox.height / 2, {steps: 12});
    await page.mouse.up();
    await expect.poll(async () => (await projection(request, org)).collections.find((row) => row.id === targetBody.id)?.artifactIds.includes(artifact.id)).toBeTruthy();
    const after = await (await request.get(`/raw/${artifact.id}`)).text();
    expect(after).toBe(before);
    expect((await projection(request, org)).collections.find((row) => row.id === sourceBody.id)?.artifactIds).toContain(artifact.id);
    await page.screenshot({ path: testInfo.outputPath("pointer-drag.png"), fullPage: true });
  });

  test("real pointer drag reorders ribbons above and below and persists after reload", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, { title: `Ribbon gesture ${org}`, html: html("Ribbon") });
    const folders = [];
    for (const name of ["Gesture ribbon A", "Gesture ribbon B", "Gesture ribbon C"]) {
      const response = await api(request, "post", "/collections", { org, name, artifactIds: [artifact.id] });
      expect(response.ok()).toBeTruthy(); folders.push(await response.json());
    }
    const folderIds = folders.map(folder => folder.id);
    const preferences = await api(request, "put", `/gallery/preferences?org=${encodeURIComponent(org)}`, {
      view: "ribbons", previewSize: "compact", artifactLayout: "grid",
      collectionOrderByOrg: { [org]: folderIds }, collapsedCollectionIdsByOrg: { [org]: folderIds },
    });
    expect(preferences.ok()).toBeTruthy();
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(`/?libraryView=ribbons&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    const first = page.locator(`[data-ribbon-id="${folders[0].id}"]`);
    const second = page.locator(`[data-ribbon-id="${folders[1].id}"]`);
    const third = page.locator(`[data-ribbon-id="${folders[2].id}"]`);
    await expect(first).toHaveCount(1); await expect(second).toHaveCount(1); await expect(third).toHaveCount(1);
    const scopedOrder = () => page.evaluate((ids) => [...document.querySelectorAll("[data-ribbon-id]")].map((item) => item.dataset.ribbonId).filter((id) => ids.includes(id)), folders.map((folder) => folder.id));
    await first.scrollIntoViewIfNeeded();
    await third.scrollIntoViewIfNeeded();
    const thirdBox = await third.boundingBox();
    const gripBox = await first.locator("[data-ribbon-drag]").boundingBox();
    await page.mouse.move(gripBox.x + gripBox.width / 2, gripBox.y + gripBox.height / 2);
    await page.mouse.down();
    await page.mouse.move(gripBox.x + gripBox.width / 2 + 8, gripBox.y + gripBox.height / 2 + 8);
    await page.mouse.move(thirdBox.x + thirdBox.width / 2, thirdBox.y + thirdBox.height - 2, { steps: 12 });
    await page.mouse.up();
    await expect.poll(scopedOrder).toEqual([folderIds[1], folderIds[2], folderIds[0]]);
    const firstRow = page.locator(`[data-ribbon-id="${folders[1].id}"]`);
    const moved = page.locator(`[data-ribbon-id="${folders[0].id}"]`);
    await moved.scrollIntoViewIfNeeded();
    await firstRow.scrollIntoViewIfNeeded();
    const targetBox = await firstRow.boundingBox();
    const movedGrip = await moved.locator("[data-ribbon-drag]").boundingBox();
    await page.mouse.move(movedGrip.x + movedGrip.width / 2, movedGrip.y + movedGrip.height / 2);
    await page.mouse.down();
    await page.mouse.move(movedGrip.x + movedGrip.width / 2 + 8, movedGrip.y + movedGrip.height / 2 + 8);
    await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + 2, { steps: 12 });
    await page.mouse.up();
    await expect.poll(scopedOrder).toEqual(folderIds);
    const finalOrder = await scopedOrder();
    await expect.poll(async () => ((await (await api(request, "get", `/gallery/preferences?org=${encodeURIComponent(org)}`)).json()).collectionOrderByOrg[org] || []).filter(id => folderIds.includes(id))).toEqual(finalOrder);
    await page.reload({ waitUntil: "domcontentloaded" });
    await expect.poll(scopedOrder).toEqual(finalOrder);
  });

  test.describe("touch input", () => {
    test.use({ hasTouch: true, viewport: { width: 390, height: 760 } });

    test("touch tap pins then dismisses the Reel Shelf and keeps touch targets usable", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, { title: `Touch gesture ${org}`, html: html("Touch") });
    const folderResponse = await api(request, "post", "/collections", { org, name: `Touch folder ${org}`, artifactIds: [artifact.id] });
    expect(folderResponse.ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    const folder = page.locator("[data-collection-peek]").filter({ hasText: `Touch folder ${org}` });
    await folder.tap();
    await expect(page.locator(".collection-reel.is-pinned")).toBeVisible();
    await folder.tap();
    await expect(page.locator(".collection-reel:not([hidden])")).toHaveCount(0);
    const undersized = await page.locator(".collection-surface[data-view='reel'] button").evaluateAll((buttons) => buttons.map((button) => ({ label: button.getAttribute("aria-label") || button.textContent.trim(), box: button.getBoundingClientRect().toJSON() })).filter((item) => item.box.width > 0 && item.box.height > 0 && (item.box.width < 44 || item.box.height < 44)));
    expect(undersized, JSON.stringify(undersized)).toEqual([]);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow).toBe(0);
    });
  });

  test("real hover places the Reel Shelf above or below after scrolling", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, { title: `Scroll gesture ${org}`, html: html("Scroll") });
    expect((await api(request, "post", "/collections", { org, name: `Scroll folder ${org}`, artifactIds: [artifact.id] })).ok()).toBeTruthy();
    await page.setViewportSize({ width: 900, height: 520 });
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    const folder = page.locator("[data-collection-peek]").filter({ hasText: `Scroll folder ${org}` });
    await folder.hover();
    const reel = page.locator(".collection-reel:not([hidden])");
    await expect(reel).toBeVisible();
    for (const y of [0, 450, 900]) {
      await page.evaluate((scrollY) => window.scrollTo(0, scrollY), y);
      await folder.hover();
      const box = await reel.boundingBox();
      expect(box).toBeTruthy();
      expect(box.y).toBeGreaterThanOrEqual(0);
      expect(box.y + box.height).toBeLessThanOrEqual(520);
      expect(await reel.getAttribute("data-placement")).toMatch(/above|below/);
    }
  });
});
