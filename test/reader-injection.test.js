import test from "node:test";
import assert from "node:assert/strict";
import { READER_BRIDGE_MARKER, injectReaderBridge } from "../lib/artifact-http.js";

test("reader bridge injection is server-owned, single, and placed before the final body tag", () => {
  const html = "<body><p>one</p><script>const fake = '</body>';</script><!-- </body> --><p>two</p></body>";
  const output = injectReaderBridge(html);
  assert.equal((output.match(new RegExp(READER_BRIDGE_MARKER, "g")) || []).length, 1);
  const marker = output.indexOf(`id="${READER_BRIDGE_MARKER}"`);
  const finalBody = output.lastIndexOf("</body>");
  assert.ok(marker > output.indexOf("<!-- </body> -->"));
  assert.ok(marker < finalBody);
  assert.match(output, /reader:hello/);
});

test("reader bridge injection appends when the document has no body tag", () => {
  const output = injectReaderBridge("<main><p>read me</p></main>");
  assert.match(output, new RegExp(`<script id="${READER_BRIDGE_MARKER}">`));
  assert.ok(output.endsWith("</script>"));
});
