// SPDX-License-Identifier: Apache-2.0
import { spawn, spawnSync } from "node:child_process";
import { constants } from "node:fs";
import { access, mkdtemp, readdir, rm } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const directory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(directory, "..");
const tests = join(directory, "tests");
const cases = [
  ["reader-mixed-content", "chromium"],
  ["reader-scopes", "chromium"],
  ["reader-sections", "chromium"],
  ["reader-word-highlights", "chromium"],
  ["reader-controls", "both"],
  ["reader-player-scopes", "both"],
  ["reader-prefetch", "both"],
  ["reader-pronunciation", "both"],
  ["reader-wav", "both"],
];
const caseTimeoutMs = 90_000;
const runTimeoutMs = 10 * 60_000;
const outputLimit = 64 * 1024;
const abort = new AbortController();
process.once("SIGINT", () => abort.abort(new Error("Reader run interrupted by SIGINT")));
process.once("SIGTERM", () => abort.abort(new Error("Reader run interrupted by SIGTERM")));

function descendantGroups(pid) {
  // Chromium can start its own process group. Capture only this case's descendants so a
  // timeout also stops those groups, without touching other local test runs.
  const result = spawnSync("ps", ["-eo", "pid=,ppid=,pgid="], { encoding: "utf8" });
  if (result.status !== 0) throw new Error("Cannot inspect reader child processes for cleanup");
  const rows = result.stdout.trim().split("\n").map(line => line.trim().split(/\s+/).map(Number));
  const descendants = new Set([pid]);
  let changed;
  do {
    changed = false;
    for (const [child, parent] of rows) {
      if (descendants.has(parent) && !descendants.has(child)) {
        descendants.add(child);
        changed = true;
      }
    }
  } while (changed);
  return new Set([pid, ...rows.filter(([child]) => descendants.has(child)).map(([, , group]) => group)]);
}

function signalGroups(groups, signal) {
  for (const group of groups) {
    try { process.kill(-group, signal); }
    catch (error) { if (error.code !== "ESRCH") throw error; }
  }
}

async function execute(name, runtime, environment) {
  abort.signal.throwIfAborted();
  const label = `${name} [${runtime}]`;
  console.log(`RUN ${label}`);
  const child = spawn(process.execPath, [join(tests, `${name}.cjs`)], {
    cwd: repository,
    env: { ...environment, READER_RUNTIME: runtime },
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let output = "";
  let failure;
  let groups;
  let killTimer;
  const capture = chunk => { output = (output + chunk.toString()).slice(-outputLimit); };
  child.stdout.on("data", capture);
  child.stderr.on("data", capture);
  const stop = reason => {
    if (failure) return;
    failure = reason;
    try {
      groups = descendantGroups(child.pid);
      signalGroups(groups, "SIGTERM");
    } catch (error) {
      failure = new Error(`${reason.message}; cleanup failed: ${error.message}`);
      child.kill("SIGTERM");
    }
    killTimer = setTimeout(() => {
      if (groups) signalGroups(groups, "SIGKILL");
      else child.kill("SIGKILL");
    }, 5_000);
  };
  const interrupted = () => stop(abort.signal.reason);
  abort.signal.addEventListener("abort", interrupted, { once: true });
  const timeout = setTimeout(() => stop(new Error(`${label} exceeded ${caseTimeoutMs / 1_000}s`)), caseTimeoutMs);
  try {
    const result = await new Promise((resolveExit, reject) => {
      child.once("error", reject);
      child.once("close", (code, signal) => resolveExit({ code, signal }));
    });
    if (failure || result.code !== 0) {
      if (groups) signalGroups(groups, "SIGKILL");
      throw new Error(`${failure?.message || `${label} failed (${result.code ?? result.signal})`}\n${output.trim()}`);
    }
    if (!output.trim()) throw new Error(`${label} exited without its assertion report`);
    console.log(output.trim());
    console.log(`PASS ${label}`);
  } finally {
    clearTimeout(timeout);
    clearTimeout(killTimer);
    abort.signal.removeEventListener("abort", interrupted);
  }
}

async function main() {
  const args = process.argv.slice(2);
  const list = args.length === 1 && args[0] === "--list";
  const selected = args.length === 2 && args[0] === "--case" ? args[1] : undefined;
  if (args.length && !list && !selected) throw new Error("Usage: reader-runner.mjs [--list | --case reader-NAME]");
  if (process.platform === "win32") throw new Error("Reader process cleanup requires Linux or macOS");
  const inventory = (await readdir(tests)).filter(name => /^reader-.*\.cjs$/.test(name)).sort();
  const expected = cases.map(([name]) => `${name}.cjs`).sort();
  if (JSON.stringify(inventory) !== JSON.stringify(expected)) {
    throw new Error("Reader coverage manifest differs from reader-*.cjs; update the runner and coverage map");
  }
  const chosen = cases.filter(([name]) => !selected || name === selected);
  if (!chosen.length) throw new Error(`Unknown reader case: ${selected}`);
  const matrix = chosen.flatMap(([name, runtime]) => runtime === "both"
    ? [[name, "node"], [name, "rust"]] : [[name, runtime]]);
  if (list) {
    for (const [name, runtime] of matrix) console.log(`${name} [${runtime}]`);
    console.log(`${cases.length} scripts; ${matrix.length} planned executions`);
    return;
  }
  if (Number(process.versions.node.split(".")[0]) < 22) throw new Error("Reader checks require Node 22+");
  const require = createRequire(join(repository, "package.json"));
  require.resolve("better-sqlite3");
  const rustBinary = resolve(process.env.RUST_ARTIFACT_MCP_BIN || join(repository, "target/release/artifact-mcp"));
  // Both runtimes are prerequisites even for a focused run. A missing build is a failure,
  // never a skipped runtime that makes the complete command appear green.
  await access(rustBinary, constants.X_OK).catch(() => {
    throw new Error("Rust binary unavailable; run cargo build --release --locked or set RUST_ARTIFACT_MCP_BIN");
  });
  const root = await mkdtemp(join(tmpdir(), "artifact-reader-ci-"));
  const deadline = setTimeout(() => abort.abort(new Error("Reader run exceeded 10 minutes")), runTimeoutMs);
  try {
    for (const [name, runtime] of matrix) {
      await execute(name, runtime, {
        ...process.env,
        RUST_ARTIFACT_MCP_BIN: rustBinary,
        READER_TEST_ROOT: root,
        PW_USE_BUNDLED_CHROMIUM: "1",
      });
    }
    console.log(`Reader regressions: ${matrix.length} executions passed; 0 skipped`);
  } finally {
    clearTimeout(deadline);
    await rm(root, { recursive: true, force: true });
  }
}

main().catch(error => { console.error(error.message); process.exitCode = 1; });
