// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Node reference-server driver. Boots the REAL `node server.js` exactly the way `npm run
// dev` does — TRUST_ACCESS_HEADERS + env-seeded publisher keys — against a caller-supplied
// DATA_DIR and PORT, and returns a handle with a clean stop().
//
// Why an "app root": the worktree may not carry node_modules. We build a throwaway launch
// directory in the OS temp area containing a byte-for-byte copy of the worktree's Node runtime
// package (server.js, lib/, assets/, the MCP output schemas, and package.json) plus a node_modules
// SYMLINK to resolved built dependencies. The server we run is the current worktree source; only
// its dependency directory is borrowed.
// Nothing is written inside the worktree or the sibling checkout.

import { spawn } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  rmSync,
  symlinkSync
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { REPO_ROOT, resolveNodeModules } from "./testkit.mjs";

export function createNodeDriver() {
  let appRoot = null;

  function ensureAppRoot() {
    if (appRoot) return appRoot;
    const nm = resolveNodeModules();
    const root = mkdtempSync(join(tmpdir(), "conf-node-approot-"));
    // Copy the current worktree source (so we test today's code, not a stale snapshot).
    cpSync(join(REPO_ROOT, "server.js"), join(root, "server.js"));
    cpSync(join(REPO_ROOT, "package.json"), join(root, "package.json"));
    cpSync(join(REPO_ROOT, "lib"), join(root, "lib"), { recursive: true });
    cpSync(join(REPO_ROOT, "assets"), join(root, "assets"), { recursive: true });
    mkdirSync(join(root, "conformance"), { recursive: true });
    cpSync(
      join(REPO_ROOT, "conformance", "mcp.tool-output-schemas.json"),
      join(root, "conformance", "mcp.tool-output-schemas.json"),
      { recursive: true }
    );
    // Dependencies come from a real, built node_modules via symlink (native better-sqlite3
    // included). ESM bare-import resolution walks up from lib/ into this symlinked dir.
    symlinkSync(nm, join(root, "node_modules"), "dir");
    appRoot = root;
    return appRoot;
  }

  return {
    name: "node",

    // Spawn the server. Resolves once the process is spawned; health polling is the runner's
    // job (implementation-neutral). Returns a handle with an idempotent stop().
    async start({ dataDir, port, env = {} }) {
      const root = ensureAppRoot();
      const child = spawn(process.execPath, ["server.js"], {
        cwd: root,
        env: {
          // A clean env: only PATH-ish essentials plus what the case declares. This keeps a
          // developer's ambient CF_ACCESS_*/ADMIN_* from leaking into a conformance run.
          PATH: process.env.PATH,
          HOME: process.env.HOME,
          ...env,
          DATA_DIR: dataDir,
          PORT: String(port)
        },
        stdio: ["ignore", "pipe", "pipe"]
      });

      const logs = [];
      child.stdout.on("data", (d) => logs.push(d.toString()));
      child.stderr.on("data", (d) => logs.push(d.toString()));

      let exited = false;
      let exitInfo = null;
      child.on("exit", (code, signal) => {
        exited = true;
        exitInfo = { code, signal };
      });

      return {
        pid: child.pid,
        baseUrl: `http://127.0.0.1:${port}`,
        logs: () => logs.join(""),
        hasExited: () => exited,
        exitInfo: () => exitInfo,
        async stop() {
          if (exited) return;
          await new Promise((resolve) => {
            child.once("exit", () => resolve());
            child.kill("SIGTERM");
            // Escalate if it ignores SIGTERM.
            setTimeout(() => {
              if (!exited) child.kill("SIGKILL");
            }, 3000);
          });
        }
      };
    },

    dispose() {
      if (appRoot && existsSync(appRoot)) {
        rmSync(appRoot, { recursive: true, force: true });
        appRoot = null;
      }
    }
  };
}
