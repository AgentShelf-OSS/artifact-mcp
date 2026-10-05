import { test, expect, api, publish } from "../fixtures.mjs";

const html = (title) => `<!doctype html><meta name="viewport" content="width=device-width"><main><h1>${title}</h1></main>`;

async function openLibrary(page, org) {
  await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`, { waitUntil: "domcontentloaded" });
  await expect(page.locator("#new-folder")).toBeVisible();
}

async function expectNoMutation(page, action) {
  const mutations = [];
  const onRequest = (request) => {
    if (["POST", "PUT", "PATCH", "DELETE"].includes(request.method())) mutations.push(`${request.method()} ${new URL(request.url()).pathname}`);
  };
  page.on("request", onRequest);
  await action();
  await page.waitForTimeout(100);
  page.off("request", onRequest);
  expect(mutations).toEqual([]);
}

async function expectNoBrowserErrors(page, action) {
  const errors = [];
  const onPageError = (error) => errors.push(`pageerror: ${error.message}`);
  const onConsole = (message) => { if (message.type() === "error") errors.push(`console: ${message.text()}`); };
  page.on("pageerror", onPageError);
  page.on("console", onConsole);
  await action();
  await page.waitForTimeout(100);
  page.off("pageerror", onPageError);
  page.off("console", onConsole);
  expect(errors).toEqual([]);
}

test.describe("collection dialog dismissal and validation", () => {
  test.beforeEach(async ({ page, request, org }) => {
    await api(request, "put", `/gallery/preferences?org=${encodeURIComponent(org)}`, {
      view: "reel",
      previewSize: "compact",
      artifactLayout: "grid",
      collectionOrderByOrg: { [org]: [] },
      collapsedCollectionIdsByOrg: { [org]: [] },
    });
    await openLibrary(page, org);
  });

  test("Cancel closes New folder without validation, network mutation, or browser errors", async ({ page }) => {
    await page.locator("#new-folder").click();
    const dialog = page.locator(".collection-create-dialog");
    await expect(dialog).toBeVisible();
    const name = dialog.locator("input[name=name]");
    await expect(name).toBeFocused();
    await expectNoBrowserErrors(page, async () => {
      await expectNoMutation(page, () => dialog.getByRole("button", { name: "Cancel" }).click());
    });
    await expect(dialog).toHaveCount(0);
    await expect(page.locator("#new-folder")).toBeFocused();
  });

  test("X closes New folder and restores focus to its launcher", async ({ page }) => {
    await page.locator("#new-folder").click();
    const dialog = page.locator(".collection-create-dialog");
    await expect(dialog).toBeVisible();
    await expectNoBrowserErrors(page, async () => {
      await expectNoMutation(page, () => dialog.getByRole("button", { name: "Close" }).click());
    });
    await expect(dialog).toHaveCount(0);
    await expect(page.locator("#new-folder")).toBeFocused();
  });

  test("the shelf New folder tile opens and cancels the same accessible dialog", async ({ page }) => {
    const tile = page.locator(".collection-new-tile");
    await tile.click();
    const dialog = page.getByRole("dialog", { name: "Create a folder", exact: true });
    await expect(dialog.getByLabel("Folder name")).toBeFocused();
    await expectNoMutation(page, () => dialog.getByRole("button", { name: "Cancel", exact: true }).click());
    await expect(dialog).toHaveCount(0);
    await expect(tile).toBeFocused();
  });

  test("Escape closes New folder without a mutation and restores focus", async ({ page }) => {
    await page.locator("#new-folder").click();
    const dialog = page.locator(".collection-create-dialog");
    await expect(dialog).toBeVisible();
    await expectNoBrowserErrors(page, async () => {
      await expectNoMutation(page, () => page.keyboard.press("Escape"));
    });
    await expect(dialog).toHaveCount(0);
    await expect(page.locator("#new-folder")).toBeFocused();
  });

  test("required folder name blocks invalid submission and valid submission creates one folder", async ({ page, request, org }) => {
    await page.locator("#new-folder").click();
    const dialog = page.locator(".collection-create-dialog");
    const name = dialog.locator("input[name=name]");
    await dialog.getByRole("button", { name: "Create folder" }).click();
    await expect(dialog).toBeVisible();
    expect(await name.evaluate((input) => input.validationMessage)).toMatch(/fill out|complete/i);

    const folderName = `Dialog valid ${Date.now()}`;
    await name.fill(folderName);
    await expectNoBrowserErrors(page, async () => {
      await dialog.getByRole("button", { name: "Create folder" }).click();
      await expect(dialog).toHaveCount(0);
    });
    await expect.poll(async () => {
      const response = await api(request, "get", `/collections?org=${encodeURIComponent(org)}&include=projection`);
      const body = await response.json();
      return body.collections?.some((collection) => collection.name === folderName) || false;
    }).toBe(true);
  });

  test("cancelling nested New folder returns to the folder picker without a mutation", async ({ page, request, org, publisherKey }) => {
    const artifact = await publish(request, publisherKey, { title: "Dialog picker source", category: "UI/UX", html: html("Picker source") });
    const folder = await api(request, "post", "/collections", { org, name: "Pending picker selection" });
    expect(folder.ok()).toBeTruthy();
    const folderId = (await folder.json()).id;
    await page.reload({ waitUntil: "domcontentloaded" });
    const card = page.locator(`#artifact-grid .card[data-id="${artifact.id}"]`);
    await expect(card).toBeVisible();
    await card.locator("[data-collection-select]").check();
    await page.locator("[data-bulk-collect]").click();
    const picker = page.locator(".collection-picker-dialog");
    await expect(picker).toBeVisible();
    const pending = picker.locator(`input[value="${folderId}"]`);
    await pending.check();
    await picker.locator("[data-picker-new]").click();
    const create = page.locator(".collection-create-dialog");
    await expect(create).toBeVisible();
    await expectNoBrowserErrors(page, async () => {
      await expectNoMutation(page, () => create.getByRole("button", { name: "Cancel" }).click());
    });
    await expect(create).toHaveCount(0);
    await expect(picker).toBeVisible();
    await expect(pending).toBeChecked();
    await expect(picker.locator("[data-picker-new]")).toBeFocused();
    await expectNoMutation(page, () => picker.getByRole("button", { name: "Cancel", exact: true }).click());
    await expect(picker).toHaveCount(0);
  });

  for (const close of ["Cancel", "Close", "Escape"]) {
    test(`edit folder ${close} exits after clearing its required name`, async ({ page, request, org }) => {
      const name = `Edit cancellation ${close}`;
      const created = await api(request, "post", "/collections", { org, name });
      expect(created.ok()).toBeTruthy();
      const folder = await created.json();
      await page.reload({ waitUntil: "domcontentloaded" });
      await page.locator(`[data-collection-peek="${folder.id}"]`).hover();
      await page.locator(".collection-reel [data-collection-edit]").click();
      const dialog = page.getByRole("dialog", { name: "Edit folder", exact: true });
      await dialog.getByLabel("Folder name").fill("");
      await expectNoBrowserErrors(page, () => expectNoMutation(page, () => close === "Escape"
        ? page.keyboard.press("Escape") : dialog.getByRole("button", { name: close, exact: true }).click()));
      await expect(dialog).toHaveCount(0);
      const response = await api(request, "get", `/collections?org=${encodeURIComponent(org)}`);
      expect((await response.json()).collections.find(row => row.id === folder.id).name).toBe(name);
    });
  }

  test("Enter submits the primary create action once", async ({ page, request, org }) => {
    const writes = [];
    page.on("request", request => {
      if (request.method() === "POST" && new URL(request.url()).pathname === "/collections") writes.push(request);
    });
    await page.locator("#new-folder").click();
    const dialog = page.getByRole("dialog", { name: "Create a folder", exact: true });
    const name = `Keyboard create ${Date.now()}`;
    await dialog.getByLabel("Folder name").fill(name);
    await dialog.getByLabel("Folder name").press("Enter");
    await expect(dialog).toHaveCount(0);
    expect(writes).toHaveLength(1);
    const projection = await api(request, "get", `/collections?org=${encodeURIComponent(org)}`);
    expect((await projection.json()).collections.filter(row => row.name === name)).toHaveLength(1);
  });

  test("failed creation retains the draft and can be cancelled without another write", async ({ page }) => {
    await page.locator("#new-folder").click();
    const dialog = page.getByRole("dialog", { name: "Create a folder", exact: true });
    await dialog.getByLabel("Folder name").fill("Keep this draft");
    await page.route("**/collections", async route => {
      if (route.request().method() === "POST") await route.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ message: "Folder write unavailable" }) });
      else await route.continue();
    });
    await dialog.getByRole("button", { name: "Create folder", exact: true }).click();
    await expect(dialog.locator(".category-error")).toHaveText("Folder write unavailable");
    await expect(dialog.getByLabel("Folder name")).toHaveValue("Keep this draft");
    await expect(dialog.getByRole("button", { name: "Create folder", exact: true })).toBeEnabled();
    await expectNoMutation(page, () => dialog.getByRole("button", { name: "Cancel", exact: true }).click());
    await expect(dialog).toHaveCount(0);
  });

  for (const method of ["Cancel", "Close"]) test(`delete confirmation ${method} retains the edit dialog and its draft`, async ({ page, request, org }) => {
    const created = await api(request, "post", "/collections", { org, name: `Delete confirmation draft ${method}` });
    const folder = await created.json();
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.locator(`[data-collection-peek="${folder.id}"]`).hover();
    await page.locator(".collection-reel [data-collection-edit]").click();
    const dialog = page.getByRole("dialog", { name: "Edit folder", exact: true });
    await dialog.getByLabel("Description").fill("Unsaved description");
    await dialog.getByRole("button", { name: "Delete folder", exact: true }).click();
    const confirmation = page.getByRole("dialog", { name: "Delete folder", exact: true });
    await expect(confirmation).toBeVisible();
    await expectNoMutation(page, () => confirmation.getByRole("button", { name: method, exact: true }).click());
    await expect(confirmation).toHaveCount(0);
    await expect(dialog).toBeVisible();
    await expect(dialog.getByLabel("Description")).toHaveValue("Unsaved description");
    await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  });

  test("folder deletion confirms in an accessible dialog and preserves its artifacts", async ({ page, request, org, publisherKey }) => {
    const artifact = await publish(request, publisherKey, { title: "Folder deletion source", html: html("Deletion source") });
    const created = await api(request, "post", "/collections", { org, name: "Folder to delete", artifactIds: [artifact.id] });
    const sibling = await api(request, "post", "/collections", { org, name: "Folder to keep", artifactIds: [artifact.id] });
    expect(created.ok() && sibling.ok()).toBeTruthy();
    const folder = await created.json();
    const kept = await sibling.json();
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.locator(`[data-collection-peek="${folder.id}"]`).hover();
    await page.locator(".collection-reel [data-collection-edit]").click();
    const edit = page.getByRole("dialog", { name: "Edit folder", exact: true });
    await edit.getByRole("button", { name: "Delete folder", exact: true }).click();
    const confirmation = page.getByRole("dialog", { name: "Delete folder", exact: true });
    await expect(confirmation).toContainText(folder.name);
    await confirmation.getByRole("button", { name: "Delete folder", exact: true }).click();
    await expect(confirmation).toHaveCount(0);
    await expect(page.locator(`[data-collection-peek="${folder.id}"]`)).toHaveCount(0);
    await expect(page.locator(`[data-collection-peek="${kept.id}"]`)).toBeVisible();
    await expect(page.locator(`#artifact-grid .card[data-id="${artifact.id}"]`)).toBeVisible();
    const projection = await api(request, "get", `/collections?org=${encodeURIComponent(org)}`);
    const rows = (await projection.json()).collections;
    expect(rows.some(row => row.id === folder.id)).toBe(false);
    expect(rows.some(row => row.id === kept.id && row.artifactIds.includes(artifact.id))).toBe(true);
  });

  test("Escape cancels folder deletion without a request", async ({ page, request, org }) => {
    const created = await api(request, "post", "/collections", { org, name: "Escape deletion" });
    const folder = await created.json();
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.locator(`[data-collection-peek="${folder.id}"]`).hover();
    await page.locator(".collection-reel [data-collection-edit]").click();
    const edit = page.getByRole("dialog", { name: "Edit folder", exact: true });
    await edit.getByRole("button", { name: "Delete folder", exact: true }).click();
    const confirmation = page.getByRole("dialog", { name: "Delete folder", exact: true });
    await expectNoMutation(page, () => page.keyboard.press("Escape"));
    await expect(confirmation).toHaveCount(0);
    await expect(edit).toBeVisible();
    const projection = await api(request, "get", `/collections?org=${encodeURIComponent(org)}`);
    expect((await projection.json()).collections.some(row => row.id === folder.id)).toBe(true);
  });

  test("failed folder deletion keeps the confirmation open and retryable", async ({ page, request, org }) => {
    const created = await api(request, "post", "/collections", { org, name: "Failed deletion" });
    const folder = await created.json();
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.locator(`[data-collection-peek="${folder.id}"]`).hover();
    await page.locator(".collection-reel [data-collection-edit]").click();
    const edit = page.getByRole("dialog", { name: "Edit folder", exact: true });
    await edit.getByRole("button", { name: "Delete folder", exact: true }).click();
    const confirmation = page.getByRole("dialog", { name: "Delete folder", exact: true });
    await page.route(`**/collections/${folder.id}**`, async route => {
      if (route.request().method() === "DELETE") await route.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ message: "Folder delete unavailable" }) });
      else await route.continue();
    });
    await confirmation.getByRole("button", { name: "Delete folder", exact: true }).click();
    await expect(confirmation).toBeVisible();
    await expect(confirmation).toContainText("Folder delete unavailable");
    await expect(confirmation.getByRole("button", { name: "Delete folder", exact: true })).toBeEnabled();
    await page.unroute(`**/collections/${folder.id}**`);
    await confirmation.getByRole("button", { name: "Delete folder", exact: true }).click();
    await expect(confirmation).toHaveCount(0);
    await expect(edit).toHaveCount(0);
  });
});
