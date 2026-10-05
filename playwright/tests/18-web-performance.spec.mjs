import { test, expect, api, publish } from "../fixtures.mjs";

test("gallery and viewer start without separate application asset requests", async ({ page, request, publisherKey }) => {
  const row = await publish(request, publisherKey, { title: "Inline application fixture", html: "<!doctype html><main><p>Inline fixture.</p></main>" });
  const assetRequests = [];
  page.on("request", request => {
    if (new URL(request.url()).pathname.startsWith("/assets/")) assetRequests.push(request.url());
  });
  await page.goto("/");
  await expect(page.getByRole("searchbox", { name: "Search artifacts" })).toBeVisible();
  await expect.poll(() => page.evaluate(() => Boolean(window.ArtifactPortal && window.ArtifactCollections))).toBe(true);
  await page.goto(`/${row.id}`);
  await expect(page.locator("#vtitle-toggle")).toBeVisible();
  await expect(page.frameLocator("#vframe").locator("main")).toHaveText("Inline fixture.");
  expect(assetRequests).toEqual([]);
});

test("search preserves sort and skips inactive collection cards", async ({ page, request, publisherKey, org }) => {
  const rows = [];
  for (const title of ["Perf Gamma", "Perf Alpha", "Perf Beta"]) {
    rows.push(await publish(request, publisherKey, { title, html: "<!doctype html><main><p>Performance regression fixture.</p></main>" }));
  }
  const created = await api(request, "post", "/collections", { org, name: "Performance folder", artifactIds: rows.map(row => row.id) });
  expect(created.ok()).toBeTruthy();
  await page.goto(`/?libraryView=all&org=${encodeURIComponent(org)}`);
  await expect.poll(() => page.evaluate(() => window.ArtifactCollections?.getState().collections.length)).toBeGreaterThan(0);
  await expect(page.locator("#collection-surface .collection-artifact-card")).toHaveCount(0);
  const before = await page.evaluate(() => ({
    sort: window.ArtifactPortal.getPerformanceStats().sortRuns,
    enhanced: window.ArtifactCollections.getPerformanceStats().canonicalEnhancements,
  }));
  const search = page.getByRole("searchbox", { name: "Search artifacts" });
  for (const title of ["Perf Alpha", "Perf Beta", "Perf Gamma"]) {
    await search.fill(title);
    await expect(page.locator("#artifact-grid > .card:visible .card-title")).toHaveText([title]);
  }
  await search.fill("");
  const after = await page.evaluate(() => ({
    sort: window.ArtifactPortal.getPerformanceStats().sortRuns,
    enhanced: window.ArtifactCollections.getPerformanceStats().canonicalEnhancements,
  }));
  expect(after).toEqual(before);
  await page.getByLabel("Sort artifacts").selectOption("title");
  await search.fill("Perf ");
  await expect(page.locator("#artifact-grid > .card:visible .card-title")).toHaveText(["Perf Alpha", "Perf Beta", "Perf Gamma"]);
  await page.locator('[data-library-view="reel"]').click();
  await expect(page.locator(".collection-grid")).toBeVisible();
  await expect(page.locator("[data-collection-peek]").filter({ hasText: "Performance folder" })).toBeVisible();
});

test("Details fetches discussion status once on first open", async ({ page, request, publisherKey }) => {
  const row = await publish(request, publisherKey, { title: "Deferred discussion fixture", html: "<!doctype html><main><p>Viewer fixture.</p></main>" });
  const requests = [];
  await page.route(`**/${row.id}/discussion/override`, route => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({ state: "local", overrideMode: "inherit" }),
  }));
  page.on("request", request => {
    if (new URL(request.url()).pathname === `/${row.id}/discussion/override`) requests.push(request);
  });
  await page.goto(`/${row.id}`);
  await page.frameLocator("#vframe").locator("main").waitFor();
  expect(requests).toHaveLength(0);
  await page.locator("#vtitle-toggle").click();
  await page.locator('[data-inspector-open="details"]').click();
  await expect.poll(() => requests.length).toBe(1);
  await expect(page.locator("#vdiscussion-state")).not.toHaveText("Checking status…");
  await page.locator("#vinspector-close").click();
  await page.locator("#vtitle-toggle").click();
  await page.locator('[data-inspector-open="details"]').click();
  expect(requests).toHaveLength(1);
});

test("voice capability states control reader availability", async ({ page, request, publisherKey }) => {
  const row = await publish(request, publisherKey, { title: "Voice capability fixture", html: "<!doctype html><main><p>Read this fixture.</p></main>" });
  let mode = "disabled";
  await page.route(`**/${row.id}/speech/voices`, route => {
    if (mode === "error") return route.fulfill({ status: 503, body: "unavailable" });
    if (mode === "disabled") return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ enabled: false, voices: [] }) });
    return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ enabled: true, voices: [{ id: "fixture_voice", name: "Fixture Voice", provider: "Fixture" }] }) });
  });
  const voicesResponse = () => page.waitForResponse(response => new URL(response.url()).pathname === `/${row.id}/speech/voices`);
  await Promise.all([voicesResponse(), page.goto(`/${row.id}`)]);
  await expect(page.locator(".vreader")).toHaveCount(1);
  await expect(page.locator(".vreader")).toBeHidden();
  mode = "enabled";
  await page.reload();
  await expect(page.locator("#vreader-toggle")).toBeEnabled();
  mode = "error";
  await Promise.all([voicesResponse(), page.reload()]);
  await expect(page.locator(".vreader")).toHaveCount(1);
  await expect(page.locator(".vreader")).toBeHidden();
});

test("late inline shell still paints existing anchors and accepts early state readiness", async ({ page, request, publisherKey }) => {
  const row = await publish(request, publisherKey, { title: "Late shell anchor fixture", html: `<!doctype html><main><p>anchor target</p><output id="state-ready">Waiting</output></main><script>
    addEventListener('message', event => { if (event.source === parent && event.data?.type === 'state:ready') document.querySelector('#state-ready').textContent = String(event.data.enabled); });
    parent.postMessage({type:'state:hello'}, '*');
  </script>` });
  const created = await api(request, "post", `/${row.id}/feedback`, {
    body: "boot marker",
    anchor: { version: 2, kind: "element", path: "main > p", nodeId: "boot", quote: "anchor target", x: 0.25, y: 0.3, approx: false },
  });
  expect(created.ok()).toBeTruthy();
  let held = false;
  let releaseShell;
  const shellReleased = new Promise(resolve => { releaseShell = resolve; });
  await page.route(`**/${row.id}`, async route => {
    const response = await route.fetch();
    const html = await response.text();
    expect(html).toContain('<div id="shell-config"');
    await route.fulfill({ response, body: html.replace('<div id="shell-config"', '<script src="/__qa_hold_shell"></script><div id="shell-config"') });
  });
  await page.route("**/__qa_hold_shell", async route => {
    if (!held) {
      held = true;
      await shellReleased;
    }
    await route.fulfill({ status: 200, contentType: "application/javascript", body: "" });
  });
  const rawLoaded = new Promise(resolve => {
    const check = frame => {
      if (frame.url().includes(`/raw/${row.id}`)) resolve();
    };
    page.on("framenavigated", check);
  });
  try {
    await page.goto(`/${row.id}`, { waitUntil: "commit" });
    await page.locator("#vframe").waitFor({ state: "attached" });
    await rawLoaded;
    await expect.poll(() => held).toBe(true);
    await expect(page.frameLocator("#vframe").locator("#state-ready")).toHaveText("Waiting");
    releaseShell();
    await expect(page.locator(`#vanchor-overlay [id^="vanchor-"]`)).toBeVisible();
    await expect(page.frameLocator("#vframe").locator("#state-ready")).toHaveText("true");
    expect(await page.locator(`#vanchor-overlay [id^="vanchor-"]`).count()).toBe(1);
  } finally {
    releaseShell();
    await page.unroute("**/__qa_hold_shell");
    await page.unroute(`**/${row.id}`);
  }
});
