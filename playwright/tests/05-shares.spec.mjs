import { test, expect, publish, api } from "../fixtures.mjs";

test.describe("sharing", () => {
  test("create, list, use publicly, revoke, then 404", async ({ request, browser, baseURL, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Share ${org}`, html: "<!doctype html><h1>s</h1>" });

    const created = await api(request, "post", `/${a.id}/share`, { expires: "never" });
    expect(created.status(), await created.text()).toBe(200);
    const share = await created.json();
    const token = share.token || share.share?.token;
    expect(token).toBeTruthy();

    const listed = await request.get(`/${a.id}/shares`);
    expect(listed.status()).toBe(200);

    // The public page must work with NO identity at all.
    const anon = await browser.newContext({ extraHTTPHeaders: {} });
    const pub = await anon.request.get(`${baseURL}/s/${token}`);
    expect(pub.status(), "public share must load unauthenticated").toBe(200);
    expect(pub.headers()["cache-control"] || "").toContain("no-store");

    const revoked = await api(request, "delete", `/${a.id}/shares/${token}`);
    expect([200, 204]).toContain(revoked.status());

    const after = await anon.request.get(`${baseURL}/s/${token}`);
    expect(after.status(), "revoked share must 404").toBe(404);

    // An invented token must be indistinguishable from a revoked one.
    const bogus = await anon.request.get(`${baseURL}/s/${"z".repeat(24)}`);
    expect(bogus.status()).toBe(404);
    await anon.close();
  });

  test("rejects an invalid expiry", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW ShareBad ${org}`, html: "<!doctype html><h1>x</h1>" });
    const res = await api(request, "post", `/${a.id}/share`, { expires: "not-a-date" });
    expect(res.status()).toBe(400);
  });

  test("public shares preserve HTML and do not enable state for unsigned or signed-in visitors", async ({ page, browser, request, baseURL, publisherKey, org }) => {
    const html = '<!doctype html><h1>share</h1>';
    const a = await publish(request, publisherKey, { title: `PW Share State ${org}`, html });
    const created = await api(request, "post", `/${a.id}/share`, { expires: "never" });
    expect(created.status()).toBe(200); const token = (await created.json()).token;
    const anon = await browser.newContext({ extraHTTPHeaders: {} });
    try {
      for (const visitor of [page, await anon.newPage()]) {
        const response = await visitor.goto(`${baseURL}/s/${token}`);
        expect(response.status()).toBe(200);
        expect(await response.text()).toBe(html);
        // If public delivery ever gains a shell, it must keep state disabled.
        await expect(visitor.locator('#shell-config[data-state-enabled="1"]')).toHaveCount(0);
      }
      expect((await anon.request.get(`${baseURL}/${a.id}/state/private`)).status()).toBe(404);
      const updated = await request.post('/mcp', { headers: { authorization: `Bearer ${publisherKey}` }, data: { jsonrpc:'2.0',id:2,method:'tools/call',params:{name:'update_artifact',arguments:{id:a.id,html:'<h1>updated</h1>'}} } });
      expect((await updated.json()).result.isError).not.toBe(true);
      const history = await page.goto(`${baseURL}/raw/${a.id}/rev/1`);
      expect(history.status()).toBe(200);
      expect(await history.text()).toBe(html);
    } finally { await anon.close(); }
  });
});
