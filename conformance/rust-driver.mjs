// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Rust production-server driver. It mirrors node-driver.mjs: start the real server against a
// caller-owned DATA_DIR and PORT, wait for /health, and return an idempotently stoppable handle.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { isAbsolute, join, resolve } from "node:path";
import { REPO_ROOT, waitForHealth } from "./testkit.mjs";

const NOT_BUILT_MESSAGE =
  "Rust artifact-mcp binary is absent. Build it with `cargo build --release`, " +
  "or set RUST_ARTIFACT_MCP_BIN to the executable. `--impl rust` is a hard failure; " +
  "`--impl both` will run Node and report this explicit skip.";

function resolveBinary() {
  const configured = process.env.RUST_ARTIFACT_MCP_BIN;
  const candidate = configured
    ? (isAbsolute(configured) ? configured : resolve(REPO_ROOT, configured))
    : join(REPO_ROOT, "target", "release", "artifact-mcp");
  return existsSync(candidate) ? candidate : null;
}

export function createRustDriver() {
  return {
    name: "rust",

    available() {
      return resolveBinary() !== null;
    },

    unavailableReason() {
      return NOT_BUILT_MESSAGE;
    },

    async start({ dataDir, port, env = {} }) {
      const bin = resolveBinary();
      if (!bin) {
        const error = new Error(NOT_BUILT_MESSAGE);
        error.code = "RUST_NOT_BUILT";
        throw error;
      }

      const child = spawn(bin, [], {
        cwd: REPO_ROOT,
        env: {
          PATH: process.env.PATH,
          HOME: process.env.HOME,
          ...env,
          DATA_DIR: dataDir,
          PORT: String(port)
        },
        stdio: ["ignore", "pipe", "pipe"]
      });

      const logs = [];
      child.stdout.on("data", (data) => logs.push(data.toString()));
      child.stderr.on("data", (data) => logs.push(data.toString()));

      let exited = false;
      let exitInfo = null;
      child.on("exit", (code, signal) => {
        exited = true;
        exitInfo = { code, signal };
      });

      const handle = {
        pid: child.pid,
        baseUrl: `http://127.0.0.1:${port}`,
        logs: () => logs.join(""),
        hasExited: () => exited,
        exitInfo: () => exitInfo,
        async stop() {
          if (exited) return;
          await new Promise((resolveStop) => {
            child.once("exit", () => resolveStop());
            child.kill("SIGTERM");
            setTimeout(() => {
              if (!exited) child.kill("SIGKILL");
            }, 3000);
          });
        }
      };

      const earlyExit = new Promise((_, reject) => {
        child.once("error", (error) => reject(error));
        child.once("exit", (code, signal) => {
          reject(new Error(`Rust server exited before health: code=${code} signal=${signal}`));
        });
      });

      try {
        await Promise.race([waitForHealth(handle.baseUrl), earlyExit]);
      } catch (error) {
        await handle.stop();
        const output = handle.logs().trim();
        const detail = output ? `\nRust server output:\n${output}` : "";
        throw new Error(`${error.message}${detail}`, { cause: error });
      }

      return handle;
    },

    dispose() {}
  };
}
