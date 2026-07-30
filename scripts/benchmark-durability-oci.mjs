#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
/**
 * Bench the production Rust OCI image over its real MCP HTTP transport.
 *
 * Usage: BENCH_DATA_DIR=/var/lib/artifact-mcp-bench node scripts/benchmark-durability-oci.mjs \
 *   artifact-mcp-bench:candidate durability-oci-benchmark.json
 *
 * BENCH_DATA_DIR must be an empty, dedicated directory on the intended persistent volume. Set
 * BENCH_REQUIRE_PERSISTENT=1 to fail unless that directory has a discoverable mount; use an OCI
 * digest for release evidence, while a local tag is supported for before/after comparison. The
 * report is machine-readable and never prints the ephemeral benchmark credential.
 */
import { execFileSync, spawnSync } from "node:child_process";
import { cpus, release, tmpdir } from "node:os";
import { mkdirSync, mkdtempSync, readdirSync, rmSync, statfsSync, writeFileSync } from "node:fs";
import path from "node:path";

const [image, out] = process.argv.slice(2);
if (!image || !out) {
  throw new Error("usage: benchmark-durability-oci.mjs IMAGE_OR_LOCAL_TAG OUT.json");
}
const integer = (name, fallback) => {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < 0) throw new Error(`${name} must be a non-negative integer`);
  return value;
};
const warmups = integer("BENCH_WARMUPS", 20);
const measured = integer("BENCH_MEASURED", 200);
const dataDir = process.env.BENCH_DATA_DIR || mkdtempSync(path.join(tmpdir(), "artifact-mcp-oci-bench-"));
const requirePersistent = process.env.BENCH_REQUIRE_PERSISTENT === "1";
mkdirSync(dataDir, { recursive: true });
if (readdirSync(dataDir).length) throw new Error(`BENCH_DATA_DIR must be an empty dedicated directory: ${dataDir}`);
if (!requirePersistent) console.error("SMOKE ONLY: set BENCH_DATA_DIR and BENCH_REQUIRE_PERSISTENT=1 for persistent-volume evidence.");

const run = (program, args) => {
  try { return execFileSync(program, args, { encoding: "utf8" }).trim(); } catch { return null; }
};
const percentile = (values, fraction) => values[Math.min(values.length - 1, Math.ceil(values.length * fraction) - 1)];
const summarize = (values) => {
  const sorted = [...values].sort((left, right) => left - right);
  return { count: sorted.length, p50Ms: percentile(sorted, .5), p95Ms: percentile(sorted, .95), maxMs: sorted.at(-1) };
};
const mount = run("findmnt", ["--json", "--target", dataDir]);
if (requirePersistent && (!process.env.BENCH_DATA_DIR || !mount)) {
  throw new Error("persistent evidence requires BENCH_DATA_DIR and a discoverable findmnt target");
}
const environment = (() => {
  const fs = statfsSync(dataDir);
  return {
    host: run("hostname", []), kernel: release(), cpu: cpus()[0]?.model || "unknown", cpus: cpus().length,
    dataDir, persistentEvidence: requirePersistent, mount,
    statfs: { type: fs.type, blockSize: fs.bsize },
  };
})();
const container = `artifact-mcp-durability-bench-${process.pid}`;
const token = "artifact-mcp-benchmark-token";
const payload = "x".repeat(64 * 1024);
let requestId = 0;
let imageIdentity;

async function waitForHealth(base) {
  let last = "not ready";
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const response = await fetch(`${base}/health`);
      if (response.ok) return;
      last = `${response.status} ${await response.text()}`;
    } catch (error) { last = String(error); }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`production OCI benchmark container was not healthy: ${last}`);
}
async function rpc(base, name, args) {
  const response = await fetch(`${base}/mcp`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: ++requestId, method: "tools/call", params: { name, arguments: args } }),
  });
  const body = await response.json();
  if (!response.ok || body.error || body.result?.isError) throw new Error(`${name}: ${response.status} ${JSON.stringify(body)}`);
  return body.result.structuredContent;
}
function files(iteration, phase) {
  return Object.fromEntries(Array.from({ length: 10 }, (_, index) => [
    `${index}.html`, `<main>${payload}${phase}-${iteration}-${index}</main>`,
  ]));
}
async function measure(base, scenario, publish, update) {
  for (let index = 0; index < warmups; index += 1) {
    const artifact = await publish(index, "warmup");
    await update(artifact, index, "warmup");
  }
  const publishTimes = [], updateTimes = [];
  for (let index = 0; index < measured; index += 1) {
    let start = performance.now();
    const artifact = await publish(index, "measured");
    publishTimes.push(performance.now() - start);
    start = performance.now();
    await update(artifact, index, "measured");
    updateTimes.push(performance.now() - start);
  }
  return { scenario, warmups, measured, publish: summarize(publishTimes), update: summarize(updateTimes) };
}

const report = { schemaVersion: 1, kind: "artifact-mcp-production-oci-durability-benchmark", requestedImage: image, environment, scenarios: [] };
try {
  const inspected = JSON.parse(execFileSync("docker", ["image", "inspect", image], { encoding: "utf8" }))[0];
  if (!inspected?.Id) throw new Error("docker image inspect returned no image identity");
  imageIdentity = {
    id: inspected.Id,
    repoDigests: inspected.RepoDigests || [],
    labels: Object.fromEntries(Object.entries(inspected.Config?.Labels || {}).filter(([key]) => key.startsWith("org.opencontainers.image."))),
  };
  report.image = imageIdentity;
  execFileSync("docker", ["run", "--detach", "--rm", "--name", container, "--publish", "127.0.0.1::3480",
    "--volume", `${dataDir}:/data-rust`, "--env", `ARTIFACT_API_KEYS=bench:default:${token}`,
    "--env", "WEBHOOK_ENC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", image], { stdio: "pipe" });
  const port = run("docker", ["port", container, "3480/tcp"])?.match(/127\.0\.0\.1:(\d+)/)?.[1];
  if (!port) throw new Error("unable to discover production OCI benchmark port");
  const base = `http://127.0.0.1:${port}`;
  await waitForHealth(base);
  report.scenarios.push(await measure(
    base,
    "single-64KiB",
    async (index, phase) => rpc(base, "publish_artifact", { html: `<main>${payload}${phase}-${index}</main>` }),
    async (artifact, index, phase) => rpc(base, "update_artifact", { id: artifact.id, html: `<main>${payload}updated-${phase}-${index}</main>` }),
  ));
  report.scenarios.push(await measure(
    base,
    "bundle-10x64KiB",
    async (index, phase) => rpc(base, "publish_bundle", { files: files(index, phase) }),
    async (artifact, index, phase) => rpc(base, "update_artifact", { id: artifact.id, files: files(index, `updated-${phase}`) }),
  ));
  report.completedAt = new Date().toISOString();
  writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
  console.log(`production OCI durability benchmark passed: ${report.scenarios.length} scenarios`);
} catch (error) {
  report.error = String(error);
  writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
  throw error;
} finally {
  spawnSync("docker", ["rm", "--force", container], { stdio: "ignore" });
  if (!process.env.BENCH_DATA_DIR) rmSync(dataDir, { recursive: true, force: true });
}
