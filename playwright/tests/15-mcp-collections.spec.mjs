// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { test, expect, api, publish } from "../fixtures.mjs";

async function call(request, key, name, args) {
  const response = await request.post("/mcp", {
    headers: { authorization: `Bearer ${key}` },
    data: { jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } }
  });
  expect(response.ok()).toBeTruthy();
  const body = await response.json();
  expect(body.error).toBeUndefined();
  expect(body.result.isError).not.toBe(true);
  return body.result.structuredContent;
}

test("MCP folders share all library views and preserve artifacts and preferences", async ({ page, request, org, publisherKey }) => {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  const artifacts = [];
  for (const title of ["MCP Alpha", "MCP Beta"]) artifacts.push(await publish(request, publisherKey, {
    title, html: `<!doctype html><h1>${title}</h1>`, category: "MCP QA"
  }));
  const prefs = await (await api(request, "get", `/gallery/preferences?org=${org}`)).json();
  const created = await call(request, publisherKey, "create_collection", { name: "MCP collection QA", artifact_ids: [artifacts[0].id], cover_artifact_id: artifacts[0].id });
  const id = created.collection.id;
  await call(request, publisherKey, "add_artifacts_to_collection", { id, artifact_ids: [artifacts[1].id] });
  expect((await call(request, publisherKey, "get_collection", { id })).artifacts.map((a) => a.id)).toEqual(expect.arrayContaining(artifacts.map((a) => a.id)));
  expect(await (await api(request, "get", `/gallery/preferences?org=${org}`)).json()).toEqual(prefs);
  for (const view of ["reel", "sheets", "ribbons"]) {
    await page.goto(`/?org=${org}&libraryView=${view}`, { waitUntil: "domcontentloaded" });
    await expect.poll(async () => page.evaluate(() => window.ArtifactCollections?.getState()?.collections?.map((c) => c.name) || [])).toContain("MCP collection QA");
    const state = await page.evaluate((id) => window.ArtifactCollections.getState().collections.find((c) => c.id === id), id);
    expect(state.artifactIds).toEqual(expect.arrayContaining(artifacts.map((a) => a.id)));
    await expect(page.locator("#collection-surface")).toContainText("MCP collection QA");
  }
  await call(request, publisherKey, "remove_artifacts_from_collection", { id, artifact_ids: [artifacts[0].id] });
  await page.reload({ waitUntil: "domcontentloaded" });
  await expect.poll(async () => page.evaluate((id) => window.ArtifactCollections?.getState()?.collections?.find((c) => c.id === id)?.artifactIds || [], id)).toEqual([artifacts[1].id]);
  await call(request, publisherKey, "delete_collection", { id });
  await page.reload({ waitUntil: "domcontentloaded" });
  await expect.poll(async () => page.evaluate(() => window.ArtifactCollections?.getState()?.collections?.map((c) => c.name) || [])).not.toContain("MCP collection QA");
  for (const artifact of artifacts) {
    const read = await call(request, publisherKey, "read_artifact", { id: artifact.id });
    expect(JSON.stringify(read)).toContain(artifact.id);
  }
  expect(errors).toEqual([]);
});
