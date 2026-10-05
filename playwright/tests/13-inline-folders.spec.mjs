import { test, expect, api, publish } from "../fixtures.mjs";

async function fixture(page, request, org, publisherKey) {
  const artifact = await publish(request, publisherKey, { title: `Inline folders ${Date.now()}`, html: "<!doctype html><h1>Inline folders</h1>" });
  const created = await api(request, "post", "/collections", { org, name: `Inline destination ${Date.now()} with a longer name to verify picker wrapping` });
  expect(created.ok()).toBeTruthy();
  const folder = await created.json();
  await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
  const card = page.locator(`#artifact-grid .card[data-id="${artifact.id}"]`);
  // Folder controls are added after the initial collection projection loads.
  await expect(card.locator("[data-collection-folder-picker]")).toBeAttached();
  await card.locator('[data-action="more"]').click();
  await card.locator("[data-collection-folder-picker]").click();
  const panel = card.locator("[data-collection-folder-panel]");
  await expect(panel).toBeVisible();
  return { artifact, folder, card, panel };
}

test.describe("inline folder memberships", () => {
  test("dismissing More resets the nested folder control", async ({ page, request, org, publisherKey }) => {
    const { card, panel } = await fixture(page, request, org, publisherKey);
    await page.keyboard.press("Escape");
    await expect(card.locator(".card-menu")).toBeHidden();
    await expect(panel).toBeHidden();
    await expect(card.locator("[data-collection-folder-picker]")).toHaveAttribute("aria-expanded", "false");
    await card.locator('[data-action="more"]').click();
    await expect(card.locator(".card-menu")).toBeVisible();
    await expect(panel).toBeHidden();
  });
  test("the artifact popup adds and removes memberships immediately without a modal", async ({ page, request, org, publisherKey }) => {
    const { artifact, folder, card } = await fixture(page, request, org, publisherKey);
    const checkbox = card.locator(`[data-collection-folder-option="${folder.id}"]`);
    await expect(page.locator(".collection-picker-dialog")).toHaveCount(0);
    await checkbox.check();
    await expect.poll(async () => {
      const response = await api(request, "get", `/collections?org=${encodeURIComponent(org)}`);
      return (await response.json()).collections.find(row => row.id === folder.id).artifactIds;
    }).toContain(artifact.id);
    await expect(checkbox).toBeEnabled();
    await expect(checkbox).toBeChecked();
    await expect(card.locator("[data-collection-folder-picker]")).toContainText(folder.name);
    await checkbox.uncheck();
    await expect.poll(async () => {
      const response = await api(request, "get", `/collections?org=${encodeURIComponent(org)}`);
      return (await response.json()).collections.find(row => row.id === folder.id).artifactIds;
    }).not.toContain(artifact.id);
    await expect(checkbox).not.toBeChecked();
    await expect(card.locator("[data-collection-folder-panel]")).toBeVisible();
  });

  test("failed membership changes roll back the checkbox and show a retryable inline error", async ({ page, request, org, publisherKey }) => {
    const { folder, card, panel } = await fixture(page, request, org, publisherKey);
    const checkbox = card.locator(`[data-collection-folder-option="${folder.id}"]`);
    await page.route(`**/collections/${folder.id}/memberships*`, async route => {
      await route.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ message: "Membership unavailable" }) });
    });
    await checkbox.click();
    await expect(panel.locator(".category-error")).toHaveText("Membership unavailable");
    await expect(checkbox).not.toBeChecked();
    await expect(checkbox).toBeEnabled();
    await page.unroute(`**/collections/${folder.id}/memberships*`);
    await checkbox.check();
    await expect(checkbox).toBeEnabled();
    await expect(checkbox).toBeChecked();
  });

  for (const view of ["reel", "ribbons"]) {
    test(`the ${view} preview keeps its folder picker usable after membership writes`, async ({ page, request, org, publisherKey }) => {
      const artifact = await publish(request, publisherKey, { title: `Inline clone ${view}`, html: "<!doctype html><h1>Clone folders</h1>" });
      const source = await api(request, "post", "/collections", { org, name: `Inline clone source ${view}`, artifactIds: [artifact.id] });
      const destination = await api(request, "post", "/collections", { org, name: `Inline clone destination ${view}` });
      expect(source.ok() && destination.ok()).toBeTruthy();
      const sourceId = (await source.json()).id;
      const targetId = (await destination.json()).id;
      await page.goto(`/?libraryView=${view}&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
      if (view === "reel") await page.locator(`[data-collection-peek="${sourceId}"]`).click();
      const scope = view === "reel" ? ".collection-reel" : `[data-ribbon-id="${sourceId}"]`;
      const card = page.locator(`${scope} .card[data-id="${artifact.id}"]`);
      await card.locator('[data-action="more"]').click();
      await card.locator("[data-collection-folder-picker]").click();
      const checkbox = card.locator(`[data-collection-folder-option="${targetId}"]`);
      await checkbox.check();
      await expect.poll(async () => (await (await api(request, "get", `/collections?org=${encodeURIComponent(org)}`)).json()).collections.find(row => row.id === targetId).artifactIds).toContain(artifact.id);
      await expect(checkbox).toBeEnabled();
      await expect(checkbox).toBeVisible();
      await checkbox.uncheck();
      await expect.poll(async () => (await (await api(request, "get", `/collections?org=${encodeURIComponent(org)}`)).json()).collections.find(row => row.id === targetId).artifactIds).not.toContain(artifact.id);
      await expect(checkbox).toBeEnabled();
      await expect(checkbox).not.toBeChecked();
    });
  }

  for (const close of ["Cancel", "Close", "Escape"]) {
  test(`new-folder ${close} returns to the expanded artifact popup`, async ({ page, request, org, publisherKey }) => {
    const { card, panel } = await fixture(page, request, org, publisherKey);
    await panel.locator("[data-picker-new]").click();
    const dialog = page.getByRole("dialog", { name: "Create a folder", exact: true });
    await expect(dialog).toBeVisible();
    if (close === "Escape") await page.keyboard.press("Escape");
    else await dialog.getByRole("button", { name: close, exact: true }).click();
    await expect(dialog).toHaveCount(0);
    await expect(panel).toBeVisible();
    await expect(card.locator(".card-menu")).toBeVisible();
  });
  }

  test("expanded folder controls remain within the mobile viewport", async ({ page, request, org, publisherKey }) => {
    await page.setViewportSize({ width: 320, height: 568 });
    const { card, panel, folder } = await fixture(page, request, org, publisherKey);
    await expect(panel).toBeVisible();
    const selected = card.locator(`[data-collection-folder-option="${folder.id}"]`);
    await selected.check();
    await expect(selected).toBeEnabled();
    const box = await card.locator(".card-menu").boundingBox();
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(320);
    expect(box.y + box.height).toBeLessThanOrEqual(568);
    const overflow = await panel.evaluate(node => node.scrollWidth - node.clientWidth);
    expect(overflow).toBeLessThanOrEqual(1);
    expect(await card.locator(".card-menu").evaluate(node => node.scrollWidth - node.clientWidth)).toBeLessThanOrEqual(1);
  });
});
