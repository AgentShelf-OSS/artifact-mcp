import { test, expect, publish, api } from "../fixtures.mjs";

test.describe("portal request authenticity", () => {
  test("a real portal action adds the mutation header and succeeds", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, {
      title: `PW CSRF UI ${org}`,
      html: "<!doctype html><h1>trusted shell action</h1>"
    });

    await page.goto(`/${artifact.id}`);
    const favorite = page.locator(".vreact.fav");
    const [response] = await Promise.all([
      page.waitForResponse((candidate) => candidate.url().includes(`/${artifact.id}/react`) && candidate.request().method() === "POST"),
      favorite.click(),
    ]);

    expect(response.status()).toBe(200);
    expect(response.request().headers()["x-artifact-mutation"]).toBe("1");
    await expect(favorite).toHaveAttribute("aria-pressed", "true");
  });

  test("a sandboxed artifact cannot revoke a publisher key with a bodyless cross-site POST", async ({ page, request, publisherKey, org }) => {
    const clientId = `pwcsrf-${Date.now().toString().slice(-7)}`;
    const issued = await api(request, "post", "/settings/keys", {
      clientId,
      org,
      label: "CSRF target",
      role: "author"
    });
    expect(issued.status(), await issued.text()).toBe(200);
    const targetKey = await issued.json();
    expect(targetKey.secret).toBeTruthy();

    const attacker = await publish(request, publisherKey, {
      title: `PW CSRF attacker ${org}`,
      // The raw-artifact CSP correctly blocks `fetch` with `connect-src 'none'`, so exercise the
      // browser primitive that remains intentionally available to artifacts: a bodyless form POST.
      // The iframe is opaque-origin because its sandbox omits `allow-same-origin`.
      html: `<!doctype html><h1>untrusted</h1>
        <form id="csrf-form" action="/settings/keys/${clientId}/revoke" method="post"></form>
        <script>document.getElementById('csrf-form').submit();</script>`
    });

    const forgedResponse = page.waitForResponse((candidate) =>
      candidate.url().includes(`/settings/keys/${clientId}/revoke`)
      && candidate.request().method() === "POST"
    );
    await page.goto(`/${attacker.id}`);
    const frame = page.locator("#vframe");
    await expect(frame).toBeVisible();
    expect(await frame.getAttribute("sandbox")).not.toContain("allow-same-origin");
    const response = await forgedResponse;
    const headers = response.request().headers();
    expect(response.status()).toBe(403);
    expect(headers["x-artifact-mutation"]).toBeUndefined();
    // Chromium normally labels this opaque sandbox request with Origin: null. The hard assertion
    // above is portable; this accepts a browser omitting it while still rejecting a forged origin.
    if (headers.origin !== undefined) expect(headers.origin).toBe("null");
    if (headers["sec-fetch-site"] !== undefined) expect(headers["sec-fetch-site"]).not.toBe("same-origin");

    // A successful publish independently proves the target key was not revoked.
    const result = await publish(request, targetKey.secret, {
      title: `PW CSRF target still active ${org}`,
      html: "<!doctype html><h1>key remains active</h1>"
    });
    expect(result.id).toBeTruthy();
  });

  test("the discussion control uses the same CSRF mutation header", async ({ page, request, publisherKey, org }) => {
    const artifact = await publish(request, publisherKey, {
      title: `PW CSRF discussion ${org}`,
      html: "<!doctype html><h1>discussion action</h1>"
    });
    const current = await api(request, "get", `/settings/orgs/${encodeURIComponent(org)}/discord-discussion`);
    expect(current.status(), await current.text()).toBe(200);
    if (!(await current.json()).configured) {
      const configured = await api(request, "put", `/settings/orgs/${encodeURIComponent(org)}/discord-discussion`, {
        label: "csrf", url: "https://discord.com/api/webhooks/123456789012345678/pw-csrf-token"
      });
      expect(configured.status(), await configured.text()).toBe(200);
    }

    await page.goto(`/${artifact.id}`);
    await page.locator("#vmore-toggle").click();
    await page.getByRole("button", { name: "Details" }).click();
    const [response] = await Promise.all([
      page.waitForResponse((candidate) => candidate.url().includes(`/${artifact.id}/discussion`) && candidate.request().method() === "PUT"),
      page.getByRole("button", { name: "Enable mirroring" }).click(),
    ]);
    expect(response.status()).toBe(200);
    expect(response.request().headers()["x-artifact-mutation"]).toBe("1");
  });
});
