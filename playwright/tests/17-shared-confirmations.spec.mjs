import { test, expect, api, publish } from "../fixtures.mjs";

for (const dismissal of ["Cancel", "Close", "Escape"]) {
  test(`organization deletion ${dismissal} preserves the registry`, async ({ page, org }) => {
    const writes = [], nativeDialogs = [];
    page.on("dialog", async dialog => { nativeDialogs.push(dialog.type()); await dialog.dismiss(); });
    await page.goto(`/settings#tab=organizations&org=${encodeURIComponent(org)}`);
    const trigger = page.locator(`[data-org-detail="${org}"] .org-del`);
    await trigger.click();
    const dialog = page.getByRole("dialog", { name: "Delete organization", exact: true });
    await expect(dialog).toContainText(org);
    await expect(dialog.getByRole("button", { name: "Cancel" })).toBeFocused();
    page.on("request", req => { if(req.method() === "DELETE") writes.push(req.url()); });
    if(dismissal === "Escape") await page.keyboard.press("Escape");
    else await dialog.getByRole("button", { name: dismissal, exact: true }).click();
    await expect(dialog).toHaveCount(0);
    await expect(trigger).toBeFocused();
    expect(writes).toEqual([]);
    expect(nativeDialogs).toEqual([]);
  });
}

test("publisher-key revocation uses the in-app confirmation and preserves CSRF protection", async ({ page, request, org }) => {
  const clientId = `confirm-key-${Date.now()}`;
  const response = await api(request, "post", "/settings/keys", {clientId, org, label:"Confirmation QA"});
  expect(response.ok()).toBeTruthy();
  await page.goto("/settings#tab=keys");
  const row = page.locator(`tr[data-id="${clientId}"]`);
  await row.locator(".revoke").click();
  const dialog = page.getByRole("dialog", {name:"Revoke publisher key",exact:true});
  await expect(dialog).toContainText(clientId);
  const mutation = page.waitForRequest(req => req.url().endsWith(`/settings/keys/${clientId}/revoke`) && req.method() === "POST");
  await dialog.getByRole("button", {name:"Revoke key",exact:true}).click();
  expect((await mutation).headers()["x-artifact-mutation"]).toBe("1");
  await expect(row).toHaveClass(/revoked/);
  await expect(dialog).toHaveCount(0);
});

test("restoring a revision confirms its effect and keeps revision history", async ({ page, request, publisherKey }) => {
  const artifact = await publish(request,publisherKey,{title:"Restore confirmation",html:"<!doctype html><h1>Version one</h1>"});
  await request.post("/mcp", {headers:{authorization:`Bearer ${publisherKey}`},data:{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:"update_artifact",arguments:{id:artifact.id,html:"<!doctype html><h1>Version two</h1>"}}}});
  await page.goto(`/${artifact.id}`);
  await page.locator("#vtitle-toggle").click();
  await page.locator('[data-inspector-open="history"]').click();
  await page.locator('.vh-restore[data-rev="1"]').click();
  const dialog = page.getByRole("dialog", {name:"Restore revision",exact:true});
  await expect(dialog).toContainText("All existing revisions will remain");
  await dialog.getByRole("button", {name:"Cancel",exact:true}).click();
  const before = await (await request.get(`/${artifact.id}/history`)).json();
  expect(before.current).toBe(2);
  await page.locator('.vh-restore[data-rev="1"]').click();
  await dialog.getByRole("button", {name:"Restore revision",exact:true}).click();
  await expect.poll(async () => (await (await request.get(`/${artifact.id}/history`)).json()).current).toBe(3);
});
