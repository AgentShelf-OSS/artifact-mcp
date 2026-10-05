import { test, expect, api, publish } from "../fixtures.mjs";

const html = (title) => `<!doctype html><meta name="viewport" content="width=device-width"><main><h1>${title}</h1><p>Collection QA artifact.</p></main>`;
async function makeArtifacts(request, key) {
  return [
    await publish(request, key, { title: "Collection QA Alpha", category: "UI/UX", html: html("Alpha") }),
    await publish(request, key, { title: "Collection QA Beta", category: "Research", html: html("Beta") }),
  ];
}
async function makeMany(request, key, count) {
  const rows = [];
  for (let index = 0; index < count; index += 1) rows.push(await publish(request, key, { title: `Collection QA ${index + 1}`, category: "UI/UX", html: html(`Artifact ${index + 1}`) }));
  return rows;
}
async function getProjection(request, org) {
  const response = await api(request, "get", `/collections?org=${encodeURIComponent(org)}&include=projection`);
  expect(response.ok()).toBeTruthy();
  return response.json();
}
async function selectOrg(page, org) {
  const filter = page.locator("#org-filter");
  if (!(await filter.isVisible())) await page.locator("#library-filters-toggle").click();
  await filter.selectOption(org);
}

test.describe("collection organization", () => {
  test.beforeEach(async ({ request, org }) => {
    await api(request, "put", `/gallery/preferences?org=${encodeURIComponent(org)}`, { view: "reel", previewSize: "compact", artifactLayout: "grid", collectionOrderByOrg: { [org]: [] }, collapsedCollectionIdsByOrg: { [org]: [] } });
  });

  test("creates a folder, adds multiple artifacts atomically, and removes a membership", async ({ request, org, publisherKey }) => {
    const [alpha, beta] = await makeArtifacts(request, publisherKey);
    const created = await api(request, "post", "/collections", { org, name: "QA collection", artifactIds: [alpha.id] });
    expect(created.status()).toBe(201);
    const collection = await created.json();
    expect(collection.artifactIds).toContain(alpha.id);
    const add = await api(request, "post", `/collections/${collection.id}/memberships?org=${encodeURIComponent(org)}`, { artifactIds: [alpha.id, beta.id] });
    expect(add.ok()).toBeTruthy();
    expect((await add.json()).added).toContain(beta.id);
    const invalid = await api(request, "post", `/collections/${collection.id}/memberships?org=${encodeURIComponent(org)}`, { artifactIds: [beta.id, "missing-artifact"] });
    expect(invalid.status()).toBe(404);
    expect((await getProjection(request, org)).collections.find((row) => row.id === collection.id).artifactIds).toEqual(expect.arrayContaining([alpha.id, beta.id]));
    const remove = await api(request, "delete", `/collections/${collection.id}/memberships?org=${encodeURIComponent(org)}`, { artifactIds: [alpha.id] });
    expect(remove.ok()).toBeTruthy();
    expect((await getProjection(request, org)).collections.find((row) => row.id === collection.id).artifactIds).not.toContain(alpha.id);
  });

  test("uses the shared toolbar to switch views and preserves the selected organization", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    expect((await api(request, "post", "/collections", { org, name: "View QA", artifactIds: [alpha.id] })).ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await selectOrg(page, org);
    for (const [view, surface] of [["reel", ".collection-grid"], ["sheets", ".collection-sheet"], ["ribbons", ".collection-ribbons"]]) {
      await page.locator(`[data-library-view='${view}']`).click();
      await expect(page.locator(`[data-library-view='${view}']`)).toHaveAttribute("aria-pressed", "true");
      await expect(page.locator(surface)).toBeVisible();
    }
    await expect.poll(async () => (await (await api(request, "get", `/gallery/preferences?org=${encodeURIComponent(org)}`)).json()).view).toBe("ribbons");
  });

  test("restores the saved layout and reports a preference write failure", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    expect((await api(request, "post", "/collections", { org, name: "Preference recovery QA", artifactIds: [alpha.id] })).ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await selectOrg(page, org);
    let blocked = false;
    await page.route("**/gallery/preferences*", async (route) => {
      if (!blocked && route.request().method() === "PUT") {
        blocked = true;
        await route.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ message: "Preference write blocked for QA" }) });
        return;
      }
      await route.continue();
    });
    await page.locator("[data-library-view='ribbons']").click();
    await expect(page.locator("[data-library-view='reel']")).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator(".collection-status")).toContainText("Preference write blocked for QA");
    await page.unroute("**/gallery/preferences*");
    await page.locator("[data-library-view='ribbons']").click();
    await expect.poll(async () => (await (await api(request, "get", `/gallery/preferences?org=${encodeURIComponent(org)}`)).json()).view).toBe("ribbons");
  });

  test("recovers from an initial folder projection failure with Retry", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    expect((await api(request, "post", "/collections", { org, name: "Retry loading QA", artifactIds: [alpha.id] })).ok()).toBeTruthy();
    let failed = false;
    await page.route("**/collections*include=projection*", async (route) => {
      if (!failed && route.request().method() === "GET") {
        failed = true;
        await route.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ message: "Projection temporarily unavailable" }) });
        return;
      }
      await route.continue();
    });
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await expect(page.locator(".collection-retry")).toBeVisible();
    await expect(page.locator(".collection-status")).toContainText("Projection temporarily unavailable");
    await page.locator(".collection-retry").click();
    await expect(page.locator("[data-collection-peek]").filter({ hasText: "Retry loading QA" })).toBeVisible();
    expect(failed).toBe(true);
  });

  test("supports keyboard reel preview, pinning, paging, and action picker", async ({ page, request, org, publisherKey }) => {
    const artifacts = await makeArtifacts(request, publisherKey);
    expect((await api(request, "post", "/collections", { org, name: "Interaction QA", artifactIds: artifacts.map((row) => row.id) })).ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await selectOrg(page, org);
    await page.locator("[data-library-view='reel']").click();
    const folder = page.locator("[data-collection-peek]").filter({ hasText: "Interaction QA" });
    await folder.focus(); await folder.press("Enter");
    const reel = page.locator(".collection-reel:not([hidden])");
    await expect(reel).toBeVisible(); await expect(reel.locator(".collection-reel-card")).toHaveCount(2); await expect(reel).toHaveAttribute("data-placement", "below");
    const pin = reel.locator("[data-reel-pin]"); if ((await pin.getAttribute("aria-label")) !== "Unpin preview reel") await pin.click();
    await expect(reel).toHaveClass(/is-pinned/);
    const card = reel.locator(".collection-reel-card").first(); await card.locator("[data-action='more']").click();
    await expect(card.locator("[data-collection-folder-picker]")).toBeVisible(); await card.locator("[data-collection-folder-picker]").click();
    await expect(card.locator("[data-collection-folder-panel]")).toBeVisible(); await page.keyboard.press("Escape");
  });

  test("repositions the temporary reel around the shelf and dismisses on a second click", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    expect((await api(request, "post", "/collections", { org, name: "Placement QA", artifactIds: [alpha.id] })).ok()).toBeTruthy();
    await page.setViewportSize({ width: 1200, height: 680 });
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" }); await selectOrg(page, org); await page.locator("[data-library-view='reel']").click();
    const folder = page.locator("[data-collection-peek]").filter({ hasText: "Placement QA" });
    await folder.hover();
    const reel = page.locator(".collection-reel:not([hidden])"); await expect(reel).toBeVisible();
    await expect(reel).toHaveAttribute("data-placement", /above|below/);
    await page.evaluate(() => window.scrollTo(0, Math.max(0, window.scrollY + document.querySelector("[data-collection-peek]").getBoundingClientRect().top - 220))); await page.waitForTimeout(150);
    await folder.hover(); await expect(reel).toHaveAttribute("data-placement", /above|below/);
    const placementAfterScroll = await reel.getAttribute("data-placement");
    const geometry = await reel.evaluate((node) => ({ top: node.getBoundingClientRect().top, bottom: node.getBoundingClientRect().bottom }));
    expect(geometry.bottom).toBeGreaterThan(geometry.top);
    if (placementAfterScroll === "below") expect(geometry.top).toBeGreaterThanOrEqual(0);
    await page.evaluate(() => window.scrollTo(0, 0)); await page.waitForTimeout(100);
    await folder.click();
    await folder.click();
    await expect(page.locator(".collection-reel:not([hidden])")).toHaveCount(0);
  });

  test("collapses and reorders ribbons with persisted preferences", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    for (const name of ["Ribbon A", "Ribbon B"]) expect((await api(request, "post", "/collections", { org, name, artifactIds: [alpha.id] })).ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" }); await selectOrg(page, org); await page.locator("[data-library-view='ribbons']").click();
    const beforeOrder = await page.locator("[data-ribbon-id]").evaluateAll((nodes) => nodes.map((node) => node.dataset.ribbonId));
    const ribbon = page.locator("[data-ribbon-id]").first(); const id = await ribbon.getAttribute("data-ribbon-id"); const collapse = ribbon.locator("[data-ribbon-collapse]");
    await collapse.focus(); await collapse.press("Enter"); await expect(collapse).toHaveAttribute("aria-expanded", "false"); await expect(ribbon.locator(".collection-ribbon-fan")).toBeVisible(); await ribbon.locator("[data-ribbon-drag]").press("ArrowDown");
    await expect.poll(async () => page.locator("[data-ribbon-id]").first().getAttribute("data-ribbon-id")).not.toBe(beforeOrder[0]);
    await page.waitForTimeout(300); await page.reload({ waitUntil: "domcontentloaded" }); await selectOrg(page, org); await page.locator("[data-library-view='ribbons']").click();
    await expect(page.locator("[data-ribbon-id]").first()).not.toHaveAttribute("data-ribbon-id", beforeOrder[0]);
    await expect(page.locator(`[data-ribbon-id='${id}'] [data-ribbon-collapse]`)).toHaveAttribute("aria-expanded", "false");
    await expect.poll(async () => { const body = await (await api(request, "get", `/gallery/preferences?org=${encodeURIComponent(org)}`)).json(); return (body.preferences || body).collapsedCollectionIdsByOrg?.[org]?.includes(id); }).toBeTruthy();
  });

  test("conceals unknown and foreign collection targets", async ({ request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey); const created = await api(request, "post", "/collections", { org, name: "Concealment QA", artifactIds: [alpha.id] }); const collection = await created.json();
    expect((await api(request, "get", "/collections?org=unknown-tenant&include=projection")).status()).toBe(404);
    expect((await api(request, "get", `/collections/${collection.id}?org=unknown-tenant`)).status()).toBe(404);
  });

  test("creates a folder from the artifact folder picker and keeps the artifact reference", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    const base = await api(request, "post", "/collections", { org, name: "Picker source", artifactIds: [alpha.id] });
    expect(base.ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await selectOrg(page, org);
    await page.locator("[data-library-view='reel']").click();
    await page.locator("[data-collection-peek]").filter({ hasText: "Picker source" }).hover();
    const card = page.locator(".collection-reel-card .card").first();
    await card.locator("[data-action='more']").click();
    await card.locator("[data-collection-folder-picker]").click();
    await page.locator("[data-picker-new]").click();
    const dialog = page.locator(".collection-create-dialog");
    await dialog.locator("input[name=name]").fill("Created from picker");
    await dialog.getByRole("button", { name: "Create folder" }).click();
    await page.waitForTimeout(300);
    await page.evaluate(() => window.ArtifactCollections?.refresh?.());
    await expect.poll(async () => (await getProjection(request, org)).collections.some((row) => row.name === "Created from picker" && row.artifactIds.includes(alpha.id)), { timeout: 15000 }).toBeTruthy();
  });

  test("reaches every member through mobile reel paging and preserves an explicit cover", async ({ page, request, org, publisherKey }) => {
    const artifacts = await makeMany(request, publisherKey, 5);
    const created = await api(request, "post", "/collections", { org, name: "Mobile reel QA", artifactIds: artifacts.map((row) => row.id), coverArtifactId: artifacts[3].id });
    expect(created.ok()).toBeTruthy();
    const body = await created.json();
    expect(body.coverArtifactId).toBe(artifacts[3].id);
    await page.setViewportSize({ width: 390, height: 760 });
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await selectOrg(page, org);
    await page.locator("[data-library-view='reel']").click();
    await page.locator("[data-collection-peek]").filter({ hasText: "Mobile reel QA" }).focus();
    const folder = page.locator("[data-collection-peek]").filter({ hasText: "Mobile reel QA" });
    await folder.press("Enter");
    const reel = page.locator(".collection-reel:not([hidden])");
    const seen = new Set();
    await expect.poll(async () => reel.locator("img").first().evaluate((node) => node.complete && node.naturalWidth > 0)).toBe(true);
    const compactCard = await reel.locator(".collection-reel-card .card").first().boundingBox();
    expect(compactCard?.height).toBeLessThan(350);
    for (let index = 0; index < artifacts.length; index += 1) {
      for (const id of await reel.locator("[data-collection-artifact]").evaluateAll((nodes) => nodes.map((node) => node.dataset.collectionArtifact))) seen.add(id);
      const next = reel.locator("[data-reel-next]");
      if (await next.isDisabled()) break;
      await next.click();
    }
    expect([...seen].sort()).toEqual(artifacts.map((row) => row.id).sort());
    await page.locator("[data-library-view='sheets']").click();
    const sheet = page.locator(".collection-sheet-card").filter({ hasText: "Mobile reel QA" });
    await expect(sheet.locator(".collection-sheet-cover img").first()).toHaveAttribute("src", new RegExp(artifacts[3].id));
  });

  test("uses distinct contact sheet compositions for one through four previews", async ({ page, request, org, publisherKey }) => {
    const artifacts = await makeMany(request, publisherKey, 4);
    for (let count = 1; count <= 4; count += 1) {
      expect((await api(request, "post", "/collections", { org, name: `Composition ${count} QA`, artifactIds: artifacts.slice(0, count).map((row) => row.id) })).ok()).toBeTruthy();
    }
    await page.goto(`/?libraryView=sheets&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await selectOrg(page, org);
    await page.locator("[data-library-view='sheets']").click();
    for (let count = 1; count <= 4; count += 1) {
      const sheet = page.locator(".collection-sheet-card").filter({ hasText: `Composition ${count} QA` });
      await expect(sheet.locator(`.collection-sheet-cover[data-count='${count === 4 ? "more" : count}']`)).toBeVisible();
      await expect(sheet.locator(".sheet-shot")).toHaveCount(count);
    }
  });

  test("dragging an artifact onto a folder adds a reference without copying the artifact", async ({ page, request, org, publisherKey }) => {
    const [alpha, beta] = await makeArtifacts(request, publisherKey);
    const source = await api(request, "post", "/collections", { org, name: "Drag source", artifactIds: [alpha.id] });
    const target = await api(request, "post", "/collections", { org, name: "Drag target" });
    expect(source.ok() && target.ok()).toBeTruthy();
    const sourceBody = await source.json(); const targetBody = await target.json();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
    await selectOrg(page, org); await page.locator("[data-library-view='reel']").click();
    await page.locator("[data-collection-peek]").filter({ hasText: "Drag source" }).hover();
    const card = page.locator(".collection-reel-card .card").first();
    const targetFace = page.locator(`[data-collection-id='${targetBody.id}']`);
    await page.evaluate(({ artifactId, targetId }) => {
      const card = document.querySelector(`[data-collection-artifact='${artifactId}']`);
      const target = document.querySelector(`[data-collection-id='${targetId}']`);
      const data = new DataTransfer(); data.setData("text/plain", artifactId);
      card.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer: data }));
      target.dispatchEvent(new DragEvent("dragover", { bubbles: true, dataTransfer: data }));
      target.dispatchEvent(new DragEvent("drop", { bubbles: true, dataTransfer: data }));
    }, { artifactId: alpha.id, targetId: targetBody.id });
    await expect.poll(async () => (await getProjection(request, org)).collections.find((row) => row.id === targetBody.id).artifactIds.includes(alpha.id)).toBeTruthy();
    const projection = await getProjection(request, org);
    expect(projection.collections.find((row) => row.id === sourceBody.id)?.artifactIds).toContain(alpha.id);
  });

  test("favorite state stays synchronized between the canonical card and reel preview", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    expect((await api(request, "post", "/collections", { org, name: "Favorite QA", artifactIds: [alpha.id] })).ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" }); await selectOrg(page, org);
    await page.locator("[data-library-view='all']").click();
    const canonical = page.locator(`#artifact-grid .card[data-id='${alpha.id}']`);
    await expect(canonical).toHaveClass(/preview-ready/);
    await canonical.locator("[data-action='favorite']").click();
    await expect(canonical.locator("[data-action='favorite']")).toHaveAttribute("aria-pressed", "true");
    await page.locator("[data-library-view='reel']").click(); await page.evaluate(() => window.ArtifactCollections?.refresh?.()); await page.locator("[data-collection-peek]").filter({ hasText: "Favorite QA" }).hover();
    const preview = page.locator(`.collection-reel-card [data-collection-artifact='${alpha.id}']`);
    await expect(preview).toHaveClass(/preview-ready/);
    const previewBox = await preview.boundingBox();
    expect(previewBox?.height).toBeLessThan(350);
    await expect(preview.locator("[data-action='favorite']")).toHaveAttribute("aria-pressed", "true");
  });

  test("opens a folder into its focused artifact view and returns to the collection shelf", async ({ page, request, org, publisherKey }) => {
    const [alpha] = await makeArtifacts(request, publisherKey);
    expect((await api(request, "post", "/collections", { org, name: "Open folder QA", artifactIds: [alpha.id] })).ok()).toBeTruthy();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" }); await selectOrg(page, org);
    await page.locator("[data-library-view='reel']").click(); await page.locator("[data-collection-peek]").filter({ hasText: "Open folder QA" }).hover();
    await page.getByRole("button", { name: "Open folder ↗" }).click();
    await expect(page.locator("[data-collection-back]")).toBeVisible();
    await expect(page.locator(".collection-folder-heading h2")).toHaveText("Open folder QA");
    await expect(page.locator(`#artifact-grid .card[data-id='${alpha.id}']`)).toBeVisible();
    await page.locator("[data-collection-back]").click();
    await expect(page.locator("[data-library-view='reel']")).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator(".collection-grid")).toBeVisible();
    expect(new URL(page.url()).searchParams.has("collection")).toBe(false);
  });
});
