import { test, expect, api, runId } from "../fixtures.mjs";

test.describe("settings administration", () => {
  test("settings page renders for an admin", async ({ page }) => {
    await page.goto("/settings");
    await expect(page.getByRole("heading", { name: /Operate the registry/i })).toBeVisible();
    await expect(page.getByRole("tab", { name: /Organizations/i })).toBeVisible();
  });

  test("org lifecycle: create, add domain and email, set colour, delete", async ({ request }) => {
    const name = `pwtmp-${runId()}`;
    const created = await api(request, "post", "/settings/orgs", { name, label: "Temp" });
    expect(created.status(), await created.text()).toBe(200);

    const dom = await api(request, "post", `/settings/orgs/${name}/domains`, { domain: `${name}.example` });
    expect(dom.status()).toBe(200);
    const undom = await api(request, "delete", `/settings/orgs/${name}/domains/${name}.example`);
    expect([200, 204]).toContain(undom.status());

    const mail = await api(request, "post", `/settings/orgs/${name}/emails`, { email: `a@${name}.example` });
    expect(mail.status()).toBe(200);
    const unmail = await api(request, "delete", `/settings/orgs/${name}/emails/${encodeURIComponent(`a@${name}.example`)}`);
    expect([200, 204]).toContain(unmail.status());

    const colour = await api(request, "post", `/settings/orgs/${name}/color`, { color: "#356B9F" });
    expect(colour.status()).toBe(200);

    const removed = await api(request, "delete", `/settings/orgs/${name}`);
    expect([200, 204]).toContain(removed.status());
  });

  test("publisher key: create shows the secret once, then revoke", async ({ request, org }) => {
    const clientId = `pwk-${runId()}-2`;
    const created = await api(request, "post", "/settings/keys", { clientId, org, label: "pw" });
    expect(created.status(), await created.text()).toBe(200);
    const body = await created.json();
    expect(body.secret || body.key, "secret must be returned once at creation").toBeTruthy();

    const revoked = await api(request, "post", `/settings/keys/${encodeURIComponent(clientId)}/revoke`, {});
    expect([200, 204]).toContain(revoked.status());
  });

  test("publisher owner assignment previews a null-only legacy backfill", async ({ page, request, org }) => {
    const clientId = `pwowner-${runId()}`;
    const ownerEmail = `owner-${runId()}@example.test`;
    const member = await api(
      request,
      "post",
      `/settings/orgs/${encodeURIComponent(org)}/emails`,
      { email: ownerEmail },
    );
    expect(member.status(), await member.text()).toBe(200);
    const created = await api(request, "post", "/settings/keys", { clientId, org, label: "owner-ui" });
    expect(created.status(), await created.text()).toBe(200);

    await page.goto("/settings");
    await page.getByRole("tab", { name: /Publisher keys/ }).click();
    const ownerCard = page.locator(`[data-key-owner-id="${clientId}"]`);
    await ownerCard.locator('input[type="email"]').fill(ownerEmail);
    await ownerCard.getByRole("button", { name: "Save owner" }).click();
    await expect(ownerCard.locator(".owner-current")).toHaveText(`Owner: ${ownerEmail}`);
    await expect(ownerCard.locator(".inline-status")).toContainText("future publishes only");

    await ownerCard.getByRole("button", { name: "Preview legacy backfill" }).click();
    await expect(ownerCard.locator(".backfill-count")).toContainText("0 null-owner artifacts");
    await expect(ownerCard.getByRole("button", { name: "Confirm backfill" })).toBeVisible();
  });

  test("webhook add and delete", async ({ request, org }) => {
    const add = await api(request, "post", `/settings/orgs/${encodeURIComponent(org)}/webhooks`, {
      url: "https://discord.com/api/webhooks/1/pwtest-token",
      label: "pw",
    });
    expect(add.status(), await add.text()).toBe(200);
    const hook = await add.json();
    const id = hook.id || hook.webhook?.id;
    if (id) {
      const del = await api(request, "delete", `/settings/orgs/${encodeURIComponent(org)}/webhooks/${id}`);
      expect([200, 204]).toContain(del.status());
    }
  });

  test("Discord threading settings keep the organization token write-only and explain inheritance", async ({ page, request, org }, testInfo) => {
    const discussionOrg = `${org}-discussion`;
    const created = await api(request, "post", "/settings/orgs", {
      name: discussionOrg,
      label: "Discussion Test",
    });
    expect(created.status(), await created.text()).toBe(200);
    // PBI-081's production credential service is Rust-owned. The legacy Node comparison server
    // deliberately exposes only the safe unavailable projection for this mutation.
    if (testInfo.project.name === "rust") {
      const disabled = await api(
        request,
        "put",
        `/settings/orgs/${encodeURIComponent(discussionOrg)}/discord-threading`,
        { botToken: "", enabled: false },
      );
      expect(disabled.status(), await disabled.text()).toBe(200);
      const disabledBody = await disabled.json();
      expect(disabledBody.enabled).toBe(false);
      expect(JSON.stringify(disabledBody)).not.toMatch(/botToken|ciphertext|nonce|tag/i);
    }

    await page.goto(`/settings#tab=notifications&org=${encodeURIComponent(discussionOrg)}`);
    const card = page.locator(`[data-discussion-org="${discussionOrg}"]`);
    const token = card.locator('input[name="botToken"]');
    await expect(card.getByRole("heading", { name: "Discord notification threads" })).toBeVisible();
    await expect(token).toHaveAttribute("type", "password");
    await expect(token).toHaveValue("");
    await expect(card.getByRole("checkbox", { name: /Enable Discord threads for this organization/i })).toBeVisible();
    await expect(card.getByText(/Eligible artifacts use this organization default/i)).toBeVisible();
    await expect(card.getByText(/exact canonical artifact URL/i)).toBeVisible();

    const removed = await api(request, "delete", `/settings/orgs/${encodeURIComponent(discussionOrg)}`);
    expect(removed.status(), await removed.text()).toBe(200);
  });

  test("a non-admin viewer cannot reach settings", async ({ browser, baseURL }) => {
    const ctx = await browser.newContext({
      extraHTTPHeaders: { "Cf-Access-Authenticated-User-Email": "nobody@outsider.example" },
    });
    const res = await ctx.request.get(baseURL + "/settings");
    expect(res.status()).toBe(403);
    await ctx.close();
  });
});
