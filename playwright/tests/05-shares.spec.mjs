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
});
