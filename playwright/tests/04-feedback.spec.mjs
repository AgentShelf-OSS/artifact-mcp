import { test, expect, publish, api } from "../fixtures.mjs";

test.describe("feedback", () => {
  test("add, list, reply, resolve, reopen, delete", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Fb ${org}`, html: "<!doctype html><h1>f</h1>" });

    const created = await api(request, "post", `/${a.id}/feedback`, { body: "first comment" });
    expect(created.status(), await created.text()).toBe(201);
    const fb = await created.json();
    expect(fb.id).toBeTruthy();

    const listed = await request.get(`/${a.id}/feedback`);
    expect(listed.status()).toBe(200);
    expect(await listed.text()).toContain("first comment");

    const reply = await api(request, "post", `/${a.id}/feedback`, { body: "a reply", parent_id: fb.id });
    expect([200, 201]).toContain(reply.status());

    const resolved = await api(request, "post", `/${a.id}/feedback/${fb.id}/resolve`, { resolved: true });
    expect(resolved.status()).toBe(200);

    const reopened = await api(request, "post", `/${a.id}/feedback/${fb.id}/resolve`, { resolved: false });
    expect(reopened.status()).toBe(200);

    const removed = await api(request, "delete", `/${a.id}/feedback/${fb.id}`);
    expect([200, 204]).toContain(removed.status());
  });

  test("empty comment is rejected", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW FbEmpty ${org}`, html: "<!doctype html><h1>e</h1>" });
    const res = await api(request, "post", `/${a.id}/feedback`, { body: "   " });
    expect(res.status()).toBe(400);
  });

  test("discussion modes are explicit and two-way fails closed without organization readiness", async ({ page, request, publisherKey, org }, testInfo) => {
    test.skip(testInfo.project.name === "node", "PBI-080 runtime activation exists only in the Rust production server.");
    const a = await publish(request, publisherKey, {
      title: `PW Discussion modes ${org}`,
      html: "<!doctype html><h1>discussion modes</h1>",
    });
    await page.goto(`/${a.id}`);
    await page.locator("#vtitle-toggle").click();
    await page.getByRole("menuitem", { name: "Details" }).click();

    await expect(page.getByRole("button", { name: "Keep discussion in Artifact MCP" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Enable two-way Discord sync" })).toBeVisible();
    await page.getByRole("button", { name: "Keep discussion in Artifact MCP" }).click();
    await expect(page.locator("#vdiscussion-state")).toHaveText("Artifact MCP only");
    await expect(page.getByRole("button", { name: "Use organization default" })).toBeVisible();

    await page.getByRole("button", { name: "Enable two-way Discord sync" }).click();
    const discussionStatus = page.locator("#vdiscussion-status");
    await expect(discussionStatus).toHaveClass(/error/);
    await expect(discussionStatus).toContainText(
      /organization Discord credential.*outbound threading policy.*ready/i,
    );

    const local = await api(request, "post", `/${a.id}/feedback`, {
      body: "local feedback survives unavailable Discord",
    });
    expect(local.status(), await local.text()).toBe(201);
  });
});
