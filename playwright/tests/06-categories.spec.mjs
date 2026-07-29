import { test, expect, publish, api } from "../fixtures.mjs";

test.describe("categories", () => {
  test("a category assigned to an artifact appears in Settings", async ({ page, request, publisherKey, org }) => {
    // Regression guard: the web-UI route used to set the artifact column without registering the
    // category on the org, so it never reached the Settings picker.
    const a = await publish(request, publisherKey, { title: `PW Cat ${org}`, html: "<!doctype html><h1>c</h1>" });
    const category = `PWCat-${Date.now().toString().slice(-5)}`;

    const res = await api(request, "post", `/${a.id}/category`, { category });
    expect(res.status(), await res.text()).toBe(200);

    await page.goto("/settings");
    await page.locator(`[data-org-select="${org}"]`).click();
    await expect(page.getByText(category, { exact: false })).toBeVisible();
  });

  test("create and delete a category in Settings", async ({ request, org }) => {
    const name = `PWMade-${Date.now().toString().slice(-5)}`;
    const made = await api(request, "post", `/settings/orgs/${encodeURIComponent(org)}/categories`, { name });
    expect(made.status(), await made.text()).toBe(200);
    const gone = await api(request, "delete", `/settings/orgs/${encodeURIComponent(org)}/categories`, { name });
    expect([200, 204]).toContain(gone.status());
  });
});
