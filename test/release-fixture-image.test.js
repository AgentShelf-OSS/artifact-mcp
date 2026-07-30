// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

test("candidate fixture runner uses API-key MCP auth without unsafe trusted identity headers", () => {
  const runner = readFileSync(join(import.meta.dirname, "../scripts/release/verify-historical-fixtures-in-image.mjs"), "utf8");
  assert.doesNotMatch(runner, /TRUST_ACCESS_HEADERS/);
  assert.match(runner, /ARTIFACT_API_KEYS=fixture-key:fixture:/);
  assert.match(runner, /containerDiagnostics/);
  assert.match(runner, /read\?\.id !== fixtureId/);
  assert.match(runner, /bytes_total/);
  assert.match(runner, /bytesTotal/);
});
