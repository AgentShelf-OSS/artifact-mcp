import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const root = new URL("../", import.meta.url);

// Server-rendered access and settings pages are covered by the Rust render suite. The trusted MCP
// review app is a static asset served verbatim, so its visual contract is checked here.
test("MCP review app keeps its locked-down policy while using the shared visual language", async () => {
  const review = await readFile(new URL("assets/mcp-review-app.html", root), "utf8");

  assert.match(review, /default-src 'none'/);
  assert.match(review, /PBI-087: align the trusted review app/);
  assert.match(review, /--blue: #b76d2e/);
  assert.match(review, /@media \(forced-colors:active\)/);
});
