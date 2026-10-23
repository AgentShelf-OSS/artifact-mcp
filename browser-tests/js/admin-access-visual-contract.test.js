import { test, assert, readText } from "./harness.js";

// Server-rendered access and settings pages are covered by the Rust render suite. The trusted MCP
// review app is a static asset served verbatim, so its visual contract is checked here.
test("MCP review app keeps its locked-down policy while using the shared visual language", async () => {
  const review = await readText("assets/mcp-review-app.html");

  assert.match(review, /default-src 'none'/);
  assert.match(review, /PBI-087: align the trusted review app/);
  assert.match(review, /--blue: #b76d2e/);
  assert.match(review, /@media \(forced-colors:active\)/);
});
