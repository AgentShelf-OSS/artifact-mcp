// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Durable operational state for the MCP Tasks extension. Task records never contain credentials,
// artifact content, request arguments, or OAuth claims.
import { randomBytes } from "node:crypto";
import {
  closeSync,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  writeFileSync
} from "node:fs";
import path from "node:path";

export const TASKS_EXTENSION = "io.modelcontextprotocol/tasks";
export const TASK_TTL_MS = 86_400_000;
export const TASK_POLL_INTERVAL_MS = 1_000;

const TASK_ID = /^task_[A-Za-z0-9_-]{20}$/;
const TERMINAL = new Set(["completed", "failed", "cancelled"]);

function now() {
  return new Date().toISOString();
}

function progressMeta(task) {
  return {
    "com.agentshelf.artifact-mcp/progress": {
      current: task.progressCurrent,
      total: task.progressTotal
    }
  };
}

export function taskWire(task, { creation = false } = {}) {
  return {
    resultType: creation ? "task" : "complete",
    taskId: task.taskId,
    status: task.status,
    statusMessage: task.statusMessage,
    createdAt: task.createdAt,
    lastUpdatedAt: task.lastUpdatedAt,
    ttlMs: task.ttlMs,
    pollIntervalMs: task.pollIntervalMs,
    _meta: progressMeta(task),
    ...(task.result === undefined ? {} : { result: task.result }),
    ...(task.error === undefined ? {} : { error: task.error })
  };
}

export function createPreviewTaskStore({
  dataDir = process.env.DATA_DIR || "/data",
  logger = console,
  createId = () => `task_${randomBytes(15).toString("base64url").slice(0, 20)}`
} = {}) {
  const root = path.join(dataDir, "tasks");
  const active = new Set();

  function taskPath(taskId) {
    return path.join(root, `${taskId}.json`);
  }

  function read(taskId) {
    if (!TASK_ID.test(String(taskId || ""))) return null;
    try {
      return JSON.parse(readFileSync(taskPath(taskId), "utf8"));
    } catch {
      return null;
    }
  }

  function write(task) {
    mkdirSync(root, { recursive: true });
    const temporary = path.join(
      root,
      `.${task.taskId}.${randomBytes(8).toString("hex")}.tmp`
    );
    let descriptor;
    try {
      descriptor = openSync(temporary, "wx");
      writeFileSync(descriptor, JSON.stringify(task));
      fsyncSync(descriptor);
      closeSync(descriptor);
      descriptor = undefined;
      renameSync(temporary, taskPath(task.taskId));
    } catch (error) {
      if (descriptor !== undefined) {
        try { closeSync(descriptor); } catch {}
      }
      try { rmSync(temporary, { force: true }); } catch {}
      logger.error?.("[artifact-mcp] durable preview task persistence failed");
      throw error;
    }
  }

  function create({ artifactId, auth }) {
    mkdirSync(root, { recursive: true });
    for (let attempt = 0; attempt < 16; attempt += 1) {
      const taskId = createId();
      if (!TASK_ID.test(taskId) || read(taskId)) continue;
      const timestamp = now();
      const task = {
        taskId,
        artifactId,
        clientId: auth.clientId,
        org: auth.org,
        role: auth.role,
        status: "working",
        statusMessage: "Preview regeneration queued",
        createdAt: timestamp,
        lastUpdatedAt: timestamp,
        ttlMs: TASK_TTL_MS,
        pollIntervalMs: TASK_POLL_INTERVAL_MS,
        progressCurrent: 0,
        progressTotal: 2
      };
      write(task);
      return task;
    }
    throw new Error("Could not allocate a durable preview task");
  }

  function transition(taskId, mutate) {
    const task = read(taskId);
    if (!task) return null;
    mutate(task);
    task.lastUpdatedAt = now();
    write(task);
    return task;
  }

  function cancel(taskId) {
    return transition(taskId, (task) => {
      if (TERMINAL.has(task.status)) return;
      task.status = "cancelled";
      task.statusMessage = "Preview regeneration cancelled";
      delete task.result;
      delete task.error;
    });
  }

  function working() {
    let names;
    try {
      names = readdirSync(root);
    } catch {
      return [];
    }
    return names
      .filter((name) => name.endsWith(".json"))
      .map((name) => read(name.slice(0, -5)))
      .filter((task) => task && !TERMINAL.has(task.status))
      .sort((left, right) => left.createdAt.localeCompare(right.createdAt));
  }

  function start(taskId, executor) {
    if (active.has(taskId)) return;
    active.add(taskId);
    queueMicrotask(async () => {
      try {
        const task = transition(taskId, (current) => {
          if (TERMINAL.has(current.status)) return;
          current.statusMessage = "Rendering artifact preview";
          current.progressCurrent = 1;
        });
        if (!task || TERMINAL.has(task.status)) return;
        try {
          const result = await executor(task);
          transition(taskId, (current) => {
            if (TERMINAL.has(current.status)) return;
            current.status = "completed";
            current.statusMessage = "Preview regeneration completed";
            current.progressCurrent = current.progressTotal;
            current.result = result;
            delete current.error;
          });
        } catch (error) {
          const message = String(error?.publicMessage || "Preview regeneration failed");
          transition(taskId, (current) => {
            if (TERMINAL.has(current.status)) return;
            current.status = "failed";
            current.statusMessage = message;
            current.error = { code: -32603, message };
            delete current.result;
          });
        }
      } finally {
        active.delete(taskId);
      }
    });
  }

  function resume(executor) {
    for (const task of working()) start(task.taskId, executor);
  }

  return { create, get: read, cancel, working, start, resume };
}

export function taskAccessibleTo(task, auth) {
  return auth?.org === "admin"
    || (task?.org === auth?.org && task?.clientId === auth?.clientId);
}
