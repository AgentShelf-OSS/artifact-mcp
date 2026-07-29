// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  createPreviewTaskStore,
  taskAccessibleTo,
  taskWire
} from "../lib/tasks.js";

function ids() {
  let sequence = 0;
  return () => `task_${String(++sequence).padStart(20, "0")}`;
}

test("working preview tasks resume from durable state after a store restart", async () => {
  const dataDir = mkdtempSync(join(tmpdir(), "artifact-mcp-task-resume-"));
  const first = createPreviewTaskStore({
    dataDir,
    logger: { error() {} },
    createId: ids()
  });
  const created = first.create({
    artifactId: "abc123",
    auth: { clientId: "owner", org: "acme", role: "author" }
  });
  assert.equal(taskWire(created, { creation: true }).resultType, "task");

  const restarted = createPreviewTaskStore({ dataDir, logger: { error() {} } });
  assert.equal(restarted.working().length, 1);
  restarted.resume(async () => ({
    content: [{ type: "text", text: "{\"regenerated\":true}" }],
    structuredContent: {
      id: "abc123",
      regenerated: true,
      digest: "a".repeat(64)
    }
  }));
  await new Promise((resolve) => setImmediate(resolve));
  const completed = restarted.get(created.taskId);
  assert.equal(completed.status, "completed");
  assert.equal(completed.progressCurrent, 2);
  assert.equal(completed.result.structuredContent.regenerated, true);

  rmSync(dataDir, { recursive: true, force: true });
});

test("task failure and authorization state persist without sensitive inputs", async () => {
  const dataDir = mkdtempSync(join(tmpdir(), "artifact-mcp-task-failure-"));
  const store = createPreviewTaskStore({
    dataDir,
    logger: { error() {} },
    createId: ids()
  });
  const created = store.create({
    artifactId: "abc123",
    auth: { clientId: "owner", org: "acme", role: "author" }
  });
  store.start(created.taskId, async () => {
    throw Object.assign(new Error("Bearer secret must not persist"), {
      publicMessage: "Preview regeneration failed safely"
    });
  });
  await new Promise((resolve) => setImmediate(resolve));
  const failed = store.get(created.taskId);
  assert.equal(failed.status, "failed");
  assert.equal(failed.error.code, -32603);
  assert.equal(failed.error.message, "Preview regeneration failed safely");
  assert.doesNotMatch(JSON.stringify(failed), /Bearer secret/);

  assert.equal(
    taskAccessibleTo(failed, { clientId: "owner", org: "acme" }),
    true
  );
  assert.equal(
    taskAccessibleTo(failed, { clientId: "other", org: "acme" }),
    false
  );
  assert.equal(
    taskAccessibleTo(failed, { clientId: "admin", org: "admin" }),
    true
  );

  rmSync(dataDir, { recursive: true, force: true });
});
