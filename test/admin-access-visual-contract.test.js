import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { accessSessionRetryPage, notFoundPage, notSignedInPage } from "../lib/portal.js";
import { renderSettings } from "../lib/settings.js";

const root = new URL("../", import.meta.url);

test("standalone access pages use the saved-theme slate/paper foundation without changing routes", () => {
  const pages = [
    [notFoundPage("Missing"), 'class="nf"'],
    [notSignedInPage(), 'class="si"'],
    [accessSessionRetryPage("/artifact?cf_access_retry=1"), 'class="retry"'],
  ];

  for (const [html, landmark] of pages) {
    assert.match(html, /localStorage\.getItem\('artifact-theme'\)/);
    assert.match(html, /:root\[data-theme="dark"\]/);
    assert.match(html, /min-height:100dvh/);
    assert.match(html, /@media\(forced-colors:active\)/);
    assert.match(html, new RegExp(landmark));
  }
  assert.match(pages[2][0], /http-equiv="refresh" content="0;url=\/artifact\?cf_access_retry=1"/);
  assert.match(pages[2][0], /href="\/artifact\?cf_access_retry=1"/);
});

test("settings and MCP review preserve their operational contracts while using the shared visual language", async () => {
  const html = renderSettings(
    { email: "admin@example.test", org: "admin", isAdmin: true },
    [],
    [{ name: "acme", label: "Acme", color: null, domains: [], emails: [], categories: [], keyCount: 0, webhooks: [] }],
  );
  const review = await readFile(new URL("assets/mcp-review-app.html", root), "utf8");

  assert.match(html, /data-ui="app-frame"/);
  assert.match(html, /data-owner-contract="pending"/);
  assert.match(html, /class="chip-value"/);
  assert.match(html, /type="password"/);
  assert.match(html, /PBI-087 administration/);
  assert.match(html, /@media\(forced-colors:active\)/);
  assert.match(review, /default-src 'none'/);
  assert.match(review, /PBI-087: align the trusted review app/);
  assert.match(review, /--blue: #b76d2e/);
  assert.match(review, /@media \(forced-colors:active\)/);
});
