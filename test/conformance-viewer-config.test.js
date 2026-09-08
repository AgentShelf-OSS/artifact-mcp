import assert from "node:assert/strict";
import test from "node:test";
import { normalizeBody } from "../conformance/comparators.mjs";

test("viewer-handle conformance compares decoded shell attributes without unrelated markup", () => {
  const node = Buffer.from('<main>Node chrome</main><div id="shell-config" data-viewer-id="&quot;8e91af3205d2c700&quot;" data-viewer-name="&quot;Alex &amp; Jo&quot;"></div>');
  const rust = Buffer.from('<div hidden data-viewer-name="&#34;Alex &#38; Jo&#34;" id="shell-config" data-viewer-id="&#x22;8e91af3205d2c700&#x22;"></div>');
  const expected = { mode: "viewer-handle", viewer: { id: "8e91af3205d2c700", name: "Alex & Jo" } };
  assert.deepEqual(normalizeBody("viewer-handle", node), expected);
  assert.deepEqual(normalizeBody("viewer-handle", rust), expected);
});

test("viewer-handle conformance rejects missing or malformed identity instead of recording it", () => {
  for (const html of [
    '<div id="shell-config"></div>',
    '<div id="shell-config" data-viewer-id="&quot;bad&quot;" data-viewer-name="&quot;Alex&quot;"></div>',
    '<div id="shell-config" data-viewer-id="&quot;8e91af3205d2c700&quot;" data-viewer-name="null"></div>',
  ]) assert.throws(() => normalizeBody("viewer-handle", Buffer.from(html)));
});
