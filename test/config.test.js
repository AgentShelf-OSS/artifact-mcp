import test from "node:test";
import assert from "node:assert/strict";
import { mcpJsonLimitFor, parseArtifactCastGrants } from "../lib/config.js";

test("cast grants require exact artifact IDs and positive revisions", () => {
  assert.deepEqual([...parseArtifactCastGrants("7qgi2ehng52j@17,abcdef@2")], ["7qgi2ehng52j@17", "abcdef@2"]);
  for (const value of ["7qgi2ehng52j", "7qgi2ehng52j@0", "7qgi2ehng52j@01", "7qgi2ehng52j@9007199254740992", "7qgi2ehng52j@x", "7qgi2ehng52j@17,", "@2"]) {
    assert.throws(() => parseArtifactCastGrants(value), /ARTIFACT_CAST_IDS/);
  }
});

test("the largest valid worst-case bundle fits the MCP request limit", () => {
  const maxBundleBytes = 8 * 1024 * 1024;
  const largestValidBundle = "\u0000".repeat(maxBundleBytes);
  const request = {
    jsonrpc: "2.0",
    id: 1,
    method: "tools/call",
    params: {
      name: "publish_bundle",
      arguments: {
        files: { "index.html": largestValidBundle },
        entry: "index.html",
        title: "Largest valid bundle"
      }
    }
  };
  const limit = mcpJsonLimitFor(maxBundleBytes);

  assert.equal(Buffer.byteLength(largestValidBundle), maxBundleBytes);
  assert.ok(Buffer.byteLength(JSON.stringify(request)) <= limit);
});
