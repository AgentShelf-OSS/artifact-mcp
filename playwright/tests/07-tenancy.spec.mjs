import { randomUUID } from "node:crypto";
import { test, expect, publish, api, adminHeaders, runId } from "../fixtures.mjs";

test.describe("visibility & tenancy", () => {
  test("hide and unhide an artifact", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Vis ${org}`, html: "<!doctype html><h1>v</h1>" });
    const hidden = await api(request, "post", `/${a.id}/visibility`, { hidden: true });
    expect(hidden.status(), await hidden.text()).toBe(200);
    const shown = await api(request, "post", `/${a.id}/visibility`, { hidden: false });
    expect(shown.status()).toBe(200);
  });

  test("a nonexistent artifact and a concealed one answer identically", async ({ request }) => {
    // invariant 3: existence must not leak. Both must be the same 404 shape.
    const missing = await request.get("/zzzznotreal99");
    expect(missing.status()).toBe(404);
    const missingApi = await request.get("/zzzznotreal99/shares");
    expect(missingApi.status()).toBe(404);
    expect(await missingApi.text()).toContain("Not found");
  });

  test("delete removes the artifact", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Del ${org}`, html: "<!doctype html><h1>d</h1>" });
    const del = await api(request, "delete", `/${a.id}`);
    expect([200, 204]).toContain(del.status());
    const after = await request.get(`/${a.id}`);
    expect(after.status()).toBe(404);
  });

  test("gallery eye and delete controls are admin-or-recorded-owner only", async ({
    browser,
    baseURL,
    request,
    org,
  }) => {
    const suffix = randomUUID().slice(0, 8);
    const clientId = `pwowned-${runId()}-${suffix}`;
    const ownerEmail = `owner-${suffix}@example.test`;
    const memberEmail = `member-${suffix}@example.test`;

    for (const email of [ownerEmail, memberEmail]) {
      const member = await api(
        request,
        "post",
        `/settings/orgs/${encodeURIComponent(org)}/emails`,
        { email },
      );
      expect(member.status(), await member.text()).toBe(200);
    }

    const created = await api(request, "post", "/settings/keys", {
      clientId,
      org,
      label: "owner-browser-qa",
    });
    expect(created.status(), await created.text()).toBe(200);
    const createdBody = await created.json();
    const key = createdBody.secret || createdBody.key;
    expect(key).toBeTruthy();

    const assigned = await api(
      request,
      "post",
      `/settings/keys/${encodeURIComponent(clientId)}/owner`,
      { ownerEmail },
    );
    expect(assigned.status(), await assigned.text()).toBe(200);

    const artifact = await publish(request, key, {
      title: `PW owner controls ${suffix}`,
      html: "<!doctype html><h1>owner controls</h1>",
    });

    async function memberPage(email) {
      const context = await browser.newContext({
        extraHTTPHeaders: { "Cf-Access-Authenticated-User-Email": email },
      });
      const page = await context.newPage();
      await page.goto(`${baseURL}/`);
      return { context, page, card: page.locator(`[data-id="${artifact.id}"]`) };
    }

    const owner = await memberPage(ownerEmail);
    await expect(owner.card).toHaveAttribute("data-owned", "1");
    await expect(owner.card.locator('[data-action="visibility"]')).toBeVisible();
    await owner.card.locator('[data-action="more"]').click();
    await expect(owner.card.getByRole("button", { name: "Delete artifact" })).toBeVisible();
    const hidden = await owner.context.request.post(`${baseURL}/${artifact.id}/visibility`, {
      headers: { "content-type": "application/json" },
      data: { hidden: true },
    });
    expect(hidden.status(), await hidden.text()).toBe(200);
    const shown = await owner.context.request.post(`${baseURL}/${artifact.id}/visibility`, {
      headers: { "content-type": "application/json" },
      data: { hidden: false },
    });
    expect(shown.status(), await shown.text()).toBe(200);
    await owner.context.close();

    const nonOwner = await memberPage(memberEmail);
    await expect(nonOwner.card).toHaveAttribute("data-owned", "0");
    await expect(nonOwner.card.locator('[data-action="visibility"]')).toHaveCount(0);
    await nonOwner.card.locator('[data-action="more"]').click();
    await expect(nonOwner.card.getByRole("button", { name: "Delete artifact" })).toHaveCount(0);
    const denied = await nonOwner.context.request.post(`${baseURL}/${artifact.id}/visibility`, {
      headers: { "content-type": "application/json" },
      data: { hidden: true },
    });
    expect(denied.status()).toBe(403);
    await nonOwner.context.close();

    const admin = await browser.newContext({ extraHTTPHeaders: adminHeaders });
    const adminPage = await admin.newPage();
    await adminPage.goto(`${baseURL}/`);
    const adminCard = adminPage.locator(`[data-id="${artifact.id}"]`);
    await expect(adminCard.locator('[data-action="visibility"]')).toBeVisible();
    await adminCard.locator('[data-action="more"]').click();
    await expect(adminCard.getByRole("button", { name: "Delete artifact" })).toBeVisible();
    await admin.close();
  });
});
