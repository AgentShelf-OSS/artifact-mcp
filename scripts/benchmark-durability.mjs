#!/usr/bin/env node
// Node-reference compatibility benchmark. This measures the frozen JS twin only; release
// performance evidence must use benchmark-durability-oci.mjs against the production Rust image.
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readdirSync, rmSync, statfsSync } from "node:fs";
import { tmpdir, cpus, release } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const number = (name, fallback) => {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < 0) throw new Error(`${name} must be a non-negative integer`);
  return value;
};
// Defaults are the release acceptance workload. Smaller values are intentionally supported for
// a fast smoke run, but the emitted record names them so it cannot be mistaken for evidence.
const WARMUPS = number("BENCH_WARMUPS", 20);
const MEASURED = number("BENCH_MEASURED", 200);
const payload = "x".repeat(64 * 1024);
const percentile = (values, p) => values[Math.min(values.length - 1, Math.ceil(values.length * p) - 1)];
const command = (program, args) => {
  try { return execFileSync(program, args, { encoding: "utf8" }).trim(); } catch { return null; }
};
const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
// Select a checkout, not a configuration toggle: the baseline must exercise the actual parent
// lifecycle implementation.  Example: BENCH_MODULE_ROOT=/srv/artifact-mcp-785897b.
const moduleRoot = path.resolve(process.env.BENCH_MODULE_ROOT || repoRoot);
const dataDir = process.env.BENCH_DATA_DIR || mkdtempSync(path.join(tmpdir(), "artifact-durability-bench-"));
const persistent = Boolean(process.env.BENCH_DATA_DIR);
mkdirSync(dataDir, { recursive: true });
if (readdirSync(dataDir).length) throw new Error(`BENCH_DATA_DIR must be an empty dedicated directory: ${dataDir}`);
// Both selected modules initialize a default store at import time. Point it at this isolated
// directory *before* importing, so a baseline checkout can never open a production DATA_DIR.
process.env.DATA_DIR = dataDir;
const { openDatabase } = await import(pathToFileURL(path.join(moduleRoot, "lib", "db.js")).href);
const { createArtifactStore } = await import(pathToFileURL(path.join(moduleRoot, "lib", "store.js")).href);
const fs = statfsSync(dataDir);
const environment = {
  moduleRoot, node: process.version, platform: process.platform, kernel: release(), cpu: cpus()[0]?.model || "unknown",
  cpus: cpus().length, dataDir, persistentVolume: persistent, statfs: { type: fs.type, blockSize: fs.bsize },
  checkoutCommit: command("git", ["-C", moduleRoot, "rev-parse", "HEAD"]),
  mount: command("findmnt", ["-no", "SOURCE,FSTYPE,TARGET", "--target", dataDir]),
  operatorMetadata: process.env.BENCH_OPERATOR_METADATA || null
};
if (!persistent) console.error("SMOKE ONLY: set BENCH_DATA_DIR to the VM310 local persistent artifact volume for release evidence.");
const runtime = openDatabase({ dataDir });
let id = 0;
const store = createArtifactStore({ db: runtime.db, artifactDir: runtime.artifactDir, idFactory: () => `bench${String(id++).padStart(7, "0")}` });
function measure(name, create, update) {
  for (let i = 0; i < WARMUPS; i++) { const artifact = create(i); update(artifact, i); }
  const publish = [], updates = [];
  for (let i = 0; i < MEASURED; i++) {
    let start = performance.now(); const artifact = create(i + WARMUPS); publish.push(performance.now() - start);
    start = performance.now(); update(artifact, i + WARMUPS); updates.push(performance.now() - start);
  }
  const summarize = (values) => { values.sort((a, b) => a - b); return { operations: values.length, p50Ms: percentile(values, .5), p95Ms: percentile(values, .95), maxMs: values.at(-1) }; };
  console.log(JSON.stringify({ scenario: name, warmups: WARMUPS, measured: MEASURED, environment, publish: summarize(publish), update: summarize(updates) }));
}
try {
  measure("single-64KiB", (i) => store.publish({ clientId: "bench", org: "default", html: `<main>${payload}${i}</main>` }), (artifact, i) => store.update({ id: artifact.id, clientId: "bench", org: "default", html: `<main>${payload}updated-${i}</main>` }));
  measure("bundle-10x64KiB", (i) => store.publishBundle({ clientId: "bench", org: "default", files: Object.fromEntries(Array.from({ length: 10 }, (_, n) => [`${n}.html`, `<main>${payload}${i}:${n}</main>`])) }), (artifact, i) => store.update({ id: artifact.id, clientId: "bench", org: "default", files: Object.fromEntries(Array.from({ length: 10 }, (_, n) => [`${n}.html`, `<main>${payload}updated-${i}:${n}</main>`])) }));
} finally {
  runtime.db.close();
  if (!persistent) rmSync(dataDir, { recursive: true, force: true });
}
