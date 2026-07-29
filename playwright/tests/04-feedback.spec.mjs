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
});
