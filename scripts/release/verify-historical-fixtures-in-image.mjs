#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
/**
 * Candidate-image historical-fixture preflight.
 *
 * Every case is copied and booted by the immutable OCI image, never by a host binary. Each boot
 * performs authenticated MCP reads; the richest recovery fixture also proves a new write, update,
 * and restore. The JSON report is a release artifact, so the manifest's passed marker has a
 * machine-verifiable execution record rather than a source-only assertion.
 */
import { createHash } from "node:crypto";
import { cpSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { execFileSync, spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const fixtureRoot = join(root, "conformance", "fixtures", "historical");
const image = process.argv[2];
const out = process.argv[3];
if (!image || !out || !image.includes("@sha256:")) {
  throw new Error("usage: verify-historical-fixtures-in-image.mjs IMAGE@sha256:DIGEST OUT.json");
}

const names = readdirSync(fixtureRoot, { withFileTypes: true })
  .filter((entry) => entry.isDirectory()).map((entry) => entry.name).sort();
const boundaries = names.filter((name) => /^boundary-v\d{2}$/.test(name));
const released = names.filter((name) => name.startsWith("release-v"));
if (released.length !== 5 || boundaries.length === 0 || names.length !== boundaries.length + released.length) {
  throw new Error(`expected every boundary plus five released rich fixtures, found ${names.length}`);
}
for (const [index, name] of boundaries.entries()) {
  if (name !== `boundary-v${String(index).padStart(2, "0")}`) {
    throw new Error(`historical fixture boundaries are not contiguous at ${name}`);
  }
}

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
function bodyManifest(root) {
  const base = join(root, "artifacts");
  const entries = [];
  function visit(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) visit(full);
      else if (entry.isFile()) {
        const bytes = readFileSync(full);
        entries.push({ path: full.slice(base.length + 1).replaceAll("\\", "/"), bytes: bytes.length, sha256: sha256(bytes) });
      }
    }
  }
  visit(base);
  return entries.sort((left, right) => left.path.localeCompare(right.path));
}
function immutableIdentity(source, manifest) {
  const db = readFileSync(join(source, manifest.database.path));
  if (sha256(db) !== manifest.database.sha256) throw new Error(`${manifest.origin.schemaVersion}: source database digest changed`);
  const bodies = bodyManifest(source);
  if (JSON.stringify(bodies) !== JSON.stringify(manifest.bodies)) throw new Error(`${manifest.origin.schemaVersion}: source body manifest changed`);
  return { databaseSha256: manifest.database.sha256, bodyManifestSha256: sha256(JSON.stringify(manifest.bodies)) };
}

function docker(args, options = {}) {
  return execFileSync("docker", args, { encoding: "utf8", ...options }).trim();
}
function containerDiagnostics(container) {
  const inspect = spawnSync("docker", ["inspect", container], { encoding: "utf8" }).stdout || "container no longer exists";
  const logs = spawnSync("docker", ["logs", container], { encoding: "utf8" }).stdout || "no container logs";
  return `inspect: ${inspect}\nlogs: ${logs}`;
}
async function waitForHealth(base, container) {
  let last = "not ready";
  for (let attempt = 0; attempt < 45; attempt += 1) {
    try {
      const response = await fetch(`${base}/health`);
      if (response.ok) return;
      last = `${response.status} ${await response.text()}`;
    } catch (error) { last = String(error); }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`${container}: health never became ready (${last})\n${containerDiagnostics(container)}`);
}
async function rpc(base, token, name, args, id) {
  const response = await fetch(`${base}/mcp`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } }),
  });
  const body = await response.json();
  if (!response.ok || body.error || body.result?.isError) throw new Error(`${name}: ${response.status} ${JSON.stringify(body)}`);
  return body.result.structuredContent;
}
async function expectConcealedRead(base, token, artifactId, id) {
  const response = await fetch(`${base}/mcp`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id,
      method: "tools/call",
      params: { name: "read_artifact", arguments: { id: artifactId } },
    }),
  });
  const body = await response.json();
  const text = body.result?.content?.find((item) => item?.type === "text")?.text;
  if (
    response.ok
    && !body.error
    && body.result?.isError === true
    && text === `Unknown artifact: ${artifactId}`
  ) {
    return;
  }
  throw new Error(
    `${artifactId}: expected concealed tool error, got ${response.status} ${JSON.stringify(body)}`,
  );
}

const report = { schemaVersion: 1, image, command: process.argv.slice(1), fixtures: [] };
try {
  for (const name of names) {
    const source = join(fixtureRoot, name);
    const manifest = JSON.parse(readFileSync(join(source, "fixture.json"), "utf8"));
    const sourceBefore = immutableIdentity(source, manifest);
    const data = mkdtempSync(join(tmpdir(), `artifact-mcp-candidate-${name}-`));
    const container = `artifact-mcp-fixture-${process.pid}-${name}`;
    try {
      cpSync(source, data, { recursive: true });
      execFileSync("chmod", ["-R", "a+rwX", data]);
      // The distroless image runs as nonroot. This temporary, synthetic copy is deliberately
      // writable while the committed source fixture remains untouched.
      docker(["run", "--detach", "--rm", "--name", container, "--publish", "127.0.0.1::3480",
        "--volume", `${data}:/data-rust`,
        "--env", `ARTIFACT_API_KEYS=fixture-key:fixture:${manifest.authentication.token}`,
        "--env", "WEBHOOK_ENC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", image]);
      const portResult = spawnSync("docker", ["port", container, "3480/tcp"], { encoding: "utf8" });
      const port = portResult.status === 0 ? portResult.stdout.match(/127\.0\.0\.1:(\d+)/)?.[1] : null;
      if (!port) throw new Error(`${name}: unable to discover candidate port\n${containerDiagnostics(container)}`);
      const base = `http://127.0.0.1:${port}`;
      await waitForHealth(base, container);
      const fixtureId = name.startsWith("boundary-v") ? `singleb${name.slice(-2)}` : "single16";
      const read = await rpc(base, manifest.authentication.token, "read_artifact", { id: fixtureId }, 1);
      if (read?.id !== fixtureId) throw new Error(`${name}: authenticated read returned ${String(read?.id)} instead of ${fixtureId}`);
      if (!Number.isSafeInteger(read?.bytes_total) || read.bytes_total < 0) {
        throw new Error(`${name}: authenticated read has invalid bytes_total`);
      }
      const evidence = {
        name,
        originSchema: manifest.origin.schemaVersion,
        source: sourceBefore,
        authenticatedRead: { artifactId: fixtureId, returnedId: read.id, bytesTotal: read.bytes_total },
      };
      if (name === "release-v23-recovery") {
        const published = await rpc(base, manifest.authentication.token, "publish_artifact", { html: "<main>candidate write</main>", title: "Candidate fixture write" }, 2);
        const id = published?.id;
        if (typeof id !== "string") throw new Error("candidate publish did not return artifact id");
        await rpc(base, manifest.authentication.token, "update_artifact", { id, html: "<main>candidate update</main>" }, 3);
        await rpc(base, manifest.authentication.token, "restore_artifact", { id, revision: 1 }, 4);
        evidence.recoveryMutation = { publishedId: id, update: "passed", restoreRevision: 1 };
      }
      if (name === "release-v24-durability-recovery") {
        const recovered = [];
        for (const artifactId of ["intentup24", "prepared24", "intentpub24", "intentdel24"]) {
          const value = await rpc(base, manifest.authentication.token, "read_artifact", { id: artifactId }, 10 + recovered.length);
          if (value?.id !== artifactId) throw new Error(`${name}: ${artifactId} was not recovered and readable`);
          recovered.push(artifactId);
        }
        for (const artifactId of ["ambigup24", "norow24", "gone24"]) {
          await expectConcealedRead(base, manifest.authentication.token, artifactId, 20 + recovered.length);
        }
        const preparedHistory = await rpc(base, manifest.authentication.token, "read_artifact", { id: "prepared24", revision: 1 }, 30);
        if (preparedHistory?.id !== "prepared24") throw new Error(`${name}: prepared24 history was not reconstructed`);
        evidence.durabilityRecovery = { recovered, preparedMetadataOnly: { revision: 2, historyRevision: 1 }, ambiguousStaging: "concealed", uncommittedPublish: "not_found", completedDelete: "not_found" };
      }
      if (JSON.stringify(immutableIdentity(source, manifest)) !== JSON.stringify(sourceBefore)) {
        throw new Error(`${name}: candidate preflight mutated the frozen source fixture`);
      }
      report.fixtures.push({ ...evidence, status: "passed" });
    } finally {
      spawnSync("docker", ["rm", "--force", container], { stdio: "ignore" });
      rmSync(data, { recursive: true, force: true });
    }
  }
} catch (error) {
  report.error = String(error);
  writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
  throw error;
}
report.completedAt = new Date().toISOString();
writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`);
console.log(`candidate fixture preflight passed: ${report.fixtures.length} cases`);
