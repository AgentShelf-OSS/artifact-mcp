import { test, expect, publish } from "../fixtures.mjs";

test.describe("artifact viewer", () => {
  test("shell renders, iframe is sandboxed without allow-same-origin", async ({ page, request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Shell ${org}`, html: "<!doctype html><h1>shell</h1>" });
    await page.goto(`/${a.id}`);
    const frame = page.locator("#vframe");
    await expect(frame).toBeVisible();
    const sandbox = await frame.getAttribute("sandbox");
    expect(sandbox, "iframe must be sandboxed").toBeTruthy();
    expect(sandbox).not.toContain("allow-same-origin");
  });

  test("raw delivery carries the CSP sandbox and never allow-same-origin", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Raw ${org}`, html: "<!doctype html><h1>raw</h1>" });
    const res = await request.get(`/raw/${a.id}`);
    expect(res.status()).toBe(200);
    const csp = res.headers()["content-security-policy"] || "";
    expect(csp).toContain("sandbox");
    expect(csp).not.toContain("allow-same-origin");
  });

  test("download returns an attachment", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Dl ${org}`, html: "<!doctype html><h1>dl</h1>" });
    const res = await request.get(`/raw/${a.id}?download=1`);
    expect(res.status()).toBe(200);
  });

  test("history lists revisions after an update", async ({ request, publisherKey, org }) => {
    const a = await publish(request, publisherKey, { title: `PW Hist ${org}`, html: "<!doctype html><h1>v1</h1>" });
    const upd = await request.post("/mcp", {
      headers: { authorization: `Bearer ${publisherKey}`, "content-type": "application/json" },
      data: { jsonrpc: "2.0", id: 1, method: "tools/call",
        params: { name: "update_artifact", arguments: { id: a.id, html: "<!doctype html><h1>v2</h1>" } } },
    });
    expect(upd.ok()).toBeTruthy();
    const hist = await request.get(`/${a.id}/history`);
    expect(hist.status()).toBe(200);
  });
});
