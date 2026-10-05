import { test, expect, api, publish } from "../fixtures.mjs";

const html = (title) => `<!doctype html><meta name="viewport" content="width=device-width"><main><h1>${title}</h1></main>`;

async function openAdminShelf(page) {
  await page.goto("/?libraryView=reel&org=all", { waitUntil: "domcontentloaded" });
  await expect(page.locator("#new-folder")).toBeVisible();
  await page.locator("#new-folder").click();
  return page.getByRole("dialog", { name: "Create a folder", exact: true });
}

test.describe("create-folder wireframe", () => {
  test("shows a live folder preview while editing name, context, and color", async ({ page, request, org, publisherKey }) => {
    await publish(request, publisherKey, { title: "Preview source", html: html("Preview source") });
    const dialog = await openAdminShelf(page);

    await expect(dialog.getByLabel("Folder name")).toBeFocused();
    await expect(dialog.getByText(/your folder, as it will look/i)).toBeVisible();

    await dialog.getByLabel("Folder name").fill("Interface experiments");
    await dialog.getByLabel("A little context").fill("Pages that explore interaction patterns.");
    const preview = dialog.locator("#folder-live-preview, [data-folder-live-preview], .create-preview");
    await expect(preview).toContainText("Interface experiments");
    await expect(dialog.getByLabel("A little context")).toHaveValue("Pages that explore interaction patterns.");
    await dialog.getByLabel("Organization").selectOption(org);
    await expect(preview).toContainText(org);

    const swatches = dialog.locator('.collection-color-option input[type="radio"], [data-color-swatch], [data-folder-color]');
    await expect(swatches).toHaveCount(4);
    await dialog.getByText("Slate", { exact: true }).click();
    await expect(swatches.nth(2)).toBeChecked();
    await swatches.nth(2).focus();
    await page.keyboard.press("ArrowRight");
    await expect(swatches.nth(3)).toBeChecked();

    await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(dialog).toHaveCount(0);
  });

  test("admin organization picker contains concrete organizations only", async ({ page, request, org }) => {
    const dialog = await openAdminShelf(page);
    const organization = dialog.getByLabel("Organization");
    await expect(organization).toBeVisible();

    const values = await organization.locator("option").evaluateAll(options => options.map(option => option.value));
    expect(values).toContain(org);
    expect(values).not.toContain("admin");
    expect(values).not.toContain("all");
    expect(new Set(values).size).toBe(values.length);

    await organization.selectOption(org);
    const folderName = `Admin preview ${Date.now()}`;
    await dialog.getByLabel("Folder name").fill(folderName);
    await dialog.getByRole("button", { name: "Create folder", exact: true }).click();
    await expect(dialog).toHaveCount(0);

    await expect.poll(async () => {
      const response = await api(request, "get", `/collections?org=${encodeURIComponent(org)}&include=projection`);
      const body = await response.json();
      return body.collections?.find(collection => collection.name === folderName)?.org || "";
    }).toBe(org);
  });

  test("mobile create dialog keeps the preview and actions reachable", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 667 });
    const dialog = await openAdminShelf(page);
    await expect(dialog).toBeVisible();

    const box = await dialog.boundingBox();
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(375);
    await expect(dialog.getByRole("button", { name: "Cancel", exact: true })).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Create folder", exact: true })).toBeVisible();
    const create = await dialog.getByRole("button", { name: "Create folder", exact: true }).boundingBox();
    expect(create.y + create.height).toBeLessThanOrEqual(667);
    const preview = dialog.locator("[data-folder-live-preview]");
    await preview.scrollIntoViewIfNeeded();
    const previewBox = await preview.boundingBox();
    expect(previewBox.y).toBeGreaterThanOrEqual(0);
    expect(previewBox.y + previewBox.height).toBeLessThan(create.y);
    await expect(dialog.getByRole("button", { name: "Cancel", exact: true })).toBeInViewport();
  });
  test("editing preserves a custom color, explicit cover, and context", async ({ page, request, org, publisherKey }) => {
    const artifact = await publish(request, publisherKey, { title: "Custom cover", html: html("Custom cover") });
    const result = await api(request, "post", "/collections", { org, name: "Custom palette", description: "Saved context", color: "#102030", artifactIds: [artifact.id], coverArtifactId: artifact.id });
    expect(result.ok()).toBeTruthy();
    const folder = await result.json();
    await page.goto(`/?libraryView=reel&org=${encodeURIComponent(org)}`);
    await page.locator(`[data-collection-peek="${folder.id}"]`).hover();
    await page.locator(".collection-reel [data-collection-edit]").click();
    const dialog = page.getByRole("dialog", { name: "Edit folder", exact: true });
    await expect(dialog.getByRole("radio", { name: "Custom", exact: true })).toBeChecked();
    await expect(dialog.getByLabel("Cover preview")).toHaveValue(artifact.id);
    await expect(dialog.getByLabel("Description")).toHaveValue("Saved context");
    await expect(dialog.locator("[data-folder-live-preview]")).toContainText("1 artifact");
    await dialog.getByLabel("Folder name").fill("Renamed custom palette");
    await dialog.getByRole("button", { name: "Save changes" }).click();
    await expect(dialog).toHaveCount(0);
    const projection = await (await api(request, "get", `/collections?org=${encodeURIComponent(org)}`)).json();
    expect(projection.collections.find(row => row.id === folder.id)).toMatchObject({name:"Renamed custom palette", color:"#102030", description:"Saved context", coverArtifactId:artifact.id});
  });

  test("the pastel preview stays readable in dark theme", async ({ page }) => {
    await openAdminShelf(page);
    await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
    const face = page.locator(".collection-preview-face");
    await expect(face.locator("strong")).toHaveCSS("color", "rgb(23, 35, 45)");
    await page.getByRole("dialog", { name:"Create a folder", exact:true }).getByRole("button", { name:"Cancel", exact:true }).click();
  });

});
