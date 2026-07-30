// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import test from "node:test";
import assert from "node:assert/strict";
import { cpSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import path from "node:path";
import { LATEST_SCHEMA_VERSION } from "../lib/migrations.js";

const root = path.resolve(import.meta.dirname, "..");
const fixtures = path.join(root, "conformance", "fixtures", "historical");
const names = readdirSync(fixtures, { withFileTypes: true }).filter((entry) => entry.isDirectory()).map((entry) => entry.name).sort();

test("frozen historical fixtures boot through the Node migration/recovery path without mutating source bytes", () => {
  assert.equal(names.length, LATEST_SCHEMA_VERSION + 6, "every migration boundary plus five released rich fixtures is checked in");
  for (const name of names) {
    const source = path.join(fixtures, name);
    const target = mkdtempSync(path.join(tmpdir(), `artifact-historical-node-${name}-`));
    try {
      cpSync(source, target, { recursive: true });
      const before = readFileSync(path.join(source, "fixture.json"));
      const manifest = JSON.parse(before);
      const result = spawnSync(process.execPath, ["scripts/run-historical-fixture-node.mjs"], {
        cwd: root,
        env: { ...process.env, DATA_DIR: target, WEBHOOK_ENC_KEY: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" },
        encoding: "utf8",
      });
      assert.equal(result.status, 0, `${name}: ${result.stderr || result.stdout}`);
      const output = JSON.parse(result.stdout.trim().split("\n").at(-1));
      assert.equal(output.encrypted > 0, manifest.webhookEncryption !== null, `${name}: encrypted webhook readability`);
      assert.deepEqual([...output.report.recoveredPaths].sort(), [...manifest.expectedRecovery.recoveredPaths].sort(), `${name}: recovered paths`);
      assert.deepEqual(output.report.divergentBodies, manifest.expectedRecovery.divergentBodies, `${name}: deterministic divergence`);
      assert.deepEqual(output.report.orphanBodies, manifest.expectedRecovery.orphanBodies, `${name}: orphan report`);
      assert.deepEqual(output.report.missingBodies, manifest.expectedRecovery.missingBodies, `${name}: missing report`);
      assert.deepEqual(output.report.transientPaths.filter((entry) => manifest.expectedRecovery.preservedTransientPaths.includes(entry)), manifest.expectedRecovery.preservedTransientPaths,
        `${name}: divergent staging remains recoverable`);
      assert.deepEqual(output.remainingIntentIds, manifest.expectedRecovery.remainingIntentIds || [], `${name}: only recoverable intents remain`);
      assert.deepEqual(
        output.preparedMetadataOnly,
        manifest.expectedRecovery.preparedMetadataOnly || null,
        `${name}: prepared metadata-only revision reconstructs durable history before becoming readable`
      );
      assert.deepEqual(readFileSync(path.join(source, "fixture.json")), before, `${name}: source manifest remains immutable`);
    } finally {
      rmSync(target, { recursive: true, force: true });
    }
  }
});
