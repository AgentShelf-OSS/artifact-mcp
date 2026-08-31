import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const root = new URL("..", import.meta.url).pathname;
const manifestTool = join(root, "scripts/release/generate-manifest.mjs");
const verifyTool = join(root, "scripts/release/verify-manifest.mjs");
const sourceSbomTool = join(root, "scripts/release/generate-source-sbom.mjs");
const commit = "a".repeat(40);
const created = "2026-07-29T18:00:00Z";
const imageDigest = `sha256:${"b".repeat(64)}`;
const immutableImage = `ghcr.io/agentshelf-oss/artifact-mcp@${imageDigest}`;
const fixturePreflight = JSON.stringify({ schemaVersion: 1, image: immutableImage, fixtures: [{ status: "passed" }] });
const sourceCommit = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim();

function run(tool, args, cwd) {
  return execFileSync(process.execPath, [tool, ...args], { cwd, encoding: "utf8" });
}

test("release manifest is deterministic and self-verifying", async () => {
  const directory = await mkdtemp(join(tmpdir(), "artifact-mcp-release-manifest-"));
  await writeFile(join(directory, "artifact-mcp"), "native binary bytes\n");
  await writeFile(join(directory, "source.spdx.json"), "{\"spdxVersion\":\"SPDX-2.3\"}\n");
  await writeFile(join(directory, "image.spdx.json"), "{\"spdxVersion\":\"SPDX-2.3\"}\n");
  await writeFile(join(directory, "migration-notes.md"), "No schema migration.\n");
  await writeFile(join(directory, "historical-fixture-preflight.json"), `${fixturePreflight}\n`);
  const args = [
    "--out", "release-manifest.json", "--tag", "v1.6.0", "--version", "1.6.0", "--commit", commit,
    "--created", created, "--binary", "artifact-mcp", "--image-ref", "ghcr.io/agentshelf-oss/artifact-mcp:v1.6.0",
    "--image-digest", imageDigest, "--schema-min", "0", "--schema-current", "23", "--schema-max", "23",
    "--source-sbom", "source.spdx.json", "--image-sbom", "image.spdx.json",
    "--migration-notes", "migration-notes.md", "--fixture-preflight", "historical-fixture-preflight.json",
    "--repository", "AgentShelf-OSS/artifact-mcp",
    "--binary-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/1",
    "--image-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/2",
  ];
  run(manifestTool, args, directory);
  const first = await readFile(join(directory, "release-manifest.json"), "utf8");
  run(manifestTool, args, directory);
  const second = await readFile(join(directory, "release-manifest.json"), "utf8");
  assert.equal(second, first);
  assert.match(run(verifyTool, ["release-manifest.json"], directory), /verified v1\.6\.0/);
  const manifest = JSON.parse(first);
  assert.equal(manifest.image.immutableReference, `ghcr.io/agentshelf-oss/artifact-mcp@${imageDigest}`);
  assert.equal(manifest.schemaVersion, 2);
  assert.equal(manifest.validation.historicalFixturePreflight, "exact-oci-historical-fixtures/v1");
  assert.equal(manifest.validation.historicalFixturePreflightReport.path, "historical-fixture-preflight.json");
  assert.equal("recovery" in manifest, false);
});

test("release manifest verification rejects a changed artifact", async () => {
  const directory = await mkdtemp(join(tmpdir(), "artifact-mcp-release-tamper-"));
  await writeFile(join(directory, "artifact-mcp"), "first\n");
  await writeFile(join(directory, "source.spdx.json"), "source\n");
  await writeFile(join(directory, "image.spdx.json"), "image\n");
  await writeFile(join(directory, "migration-notes.md"), "No schema migration.\n");
  await writeFile(join(directory, "historical-fixture-preflight.json"), `${fixturePreflight}\n`);
  run(manifestTool, [
    "--out", "release-manifest.json", "--tag", "v1.6.0", "--version", "1.6.0", "--commit", commit,
    "--created", created, "--binary", "artifact-mcp", "--image-ref", "ghcr.io/agentshelf-oss/artifact-mcp:v1.6.0",
    "--image-digest", imageDigest, "--schema-min", "0", "--schema-current", "23", "--schema-max", "23",
    "--source-sbom", "source.spdx.json", "--image-sbom", "image.spdx.json",
    "--migration-notes", "migration-notes.md", "--fixture-preflight", "historical-fixture-preflight.json",
    "--repository", "AgentShelf-OSS/artifact-mcp",
    "--binary-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/1",
    "--image-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/2",
  ], directory);
  await writeFile(join(directory, "artifact-mcp"), "changed\n");
  const result = spawnSync(process.execPath, [verifyTool, "release-manifest.json"], { cwd: directory, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /digest mismatch/);
});

test("release manifest verification rejects redirected image identity and changed migration notes", async () => {
  const directory = await mkdtemp(join(tmpdir(), "artifact-mcp-release-identity-"));
  await writeFile(join(directory, "artifact-mcp"), "native binary\n");
  await writeFile(join(directory, "source.spdx.json"), "source\n");
  await writeFile(join(directory, "image.spdx.json"), "image\n");
  await writeFile(join(directory, "migration-notes.md"), "No schema migration.\n");
  await writeFile(join(directory, "historical-fixture-preflight.json"), `${fixturePreflight}\n`);
  const args = [
    "--out", "release-manifest.json", "--tag", "v1.6.0", "--version", "1.6.0", "--commit", commit,
    "--created", created, "--binary", "artifact-mcp", "--image-ref", "ghcr.io/agentshelf-oss/artifact-mcp:v1.6.0",
    "--image-digest", imageDigest, "--schema-min", "0", "--schema-current", "23", "--schema-max", "23",
    "--source-sbom", "source.spdx.json", "--image-sbom", "image.spdx.json",
    "--migration-notes", "migration-notes.md", "--fixture-preflight", "historical-fixture-preflight.json",
    "--repository", "AgentShelf-OSS/artifact-mcp",
    "--binary-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/1",
    "--image-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/2",
  ];
  run(manifestTool, args, directory);
  const manifestPath = join(directory, "release-manifest.json");
  const redirected = JSON.parse(await readFile(manifestPath, "utf8"));
  redirected.image.immutableReference = `ghcr.io/agentshelf-oss/other@${imageDigest}`;
  await writeFile(manifestPath, `${JSON.stringify(redirected, null, 2)}\n`);
  let result = spawnSync(process.execPath, [verifyTool, "release-manifest.json"], { cwd: directory, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /immutable image reference/);

  run(manifestTool, args, directory);
  const crossRepository = JSON.parse(await readFile(manifestPath, "utf8"));
  for (const [index, attestation] of crossRepository.attestations.entries()) {
    attestation.repository = "evil/repository";
    attestation.reference = `https://github.com/evil/repository/attestations/${index + 1}`;
    attestation.verification = index === 0
      ? "gh attestation verify artifact-mcp --repo evil/repository"
      : `gh attestation verify oci://ghcr.io/agentshelf-oss/artifact-mcp@${imageDigest} --repo evil/repository`;
  }
  await writeFile(manifestPath, `${JSON.stringify(crossRepository, null, 2)}\n`);
  result = spawnSync(process.execPath, [verifyTool, "release-manifest.json"], { cwd: directory, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /attestation repository does not match image repository/);

  run(manifestTool, args, directory);
  await writeFile(join(directory, "migration-notes.md"), "Changed after manifest creation.\n");
  result = spawnSync(process.execPath, [verifyTool, "release-manifest.json"], { cwd: directory, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /migration notes/);
});

test("release manifest verification rejects missing or inconsistent validation evidence", async () => {
  const directory = await mkdtemp(join(tmpdir(), "artifact-mcp-release-validation-"));
  await writeFile(join(directory, "artifact-mcp"), "native binary\n");
  await writeFile(join(directory, "source.spdx.json"), "source\n");
  await writeFile(join(directory, "image.spdx.json"), "image\n");
  await writeFile(join(directory, "migration-notes.md"), "No schema migration.\n");
  await writeFile(join(directory, "historical-fixture-preflight.json"), `${fixturePreflight}\n`);
  const args = [
    "--out", "release-manifest.json", "--tag", "v1.6.0", "--version", "1.6.0", "--commit", commit,
    "--created", created, "--binary", "artifact-mcp", "--image-ref", "ghcr.io/agentshelf-oss/artifact-mcp:v1.6.0",
    "--image-digest", imageDigest, "--schema-min", "0", "--schema-current", "23", "--schema-max", "23",
    "--source-sbom", "source.spdx.json", "--image-sbom", "image.spdx.json",
    "--migration-notes", "migration-notes.md", "--fixture-preflight", "historical-fixture-preflight.json",
    "--repository", "AgentShelf-OSS/artifact-mcp",
    "--binary-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/1",
    "--image-attestation", "https://github.com/AgentShelf-OSS/artifact-mcp/attestations/2",
  ];
  const manifestPath = join(directory, "release-manifest.json");

  run(manifestTool, args, directory);
  const missingValidation = JSON.parse(await readFile(manifestPath, "utf8"));
  delete missingValidation.validation;
  await writeFile(manifestPath, `${JSON.stringify(missingValidation, null, 2)}\n`);
  let result = spawnSync(process.execPath, [verifyTool, "release-manifest.json"], { cwd: directory, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /invalid migration notes digest record/);

  run(manifestTool, args, directory);
  const missingFixtureReport = JSON.parse(await readFile(manifestPath, "utf8"));
  delete missingFixtureReport.validation.historicalFixturePreflightReport;
  await writeFile(manifestPath, `${JSON.stringify(missingFixtureReport, null, 2)}\n`);
  result = spawnSync(process.execPath, [verifyTool, "release-manifest.json"], { cwd: directory, encoding: "utf8" });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /invalid historical fixture preflight report digest record/);

  for (const fixtureReport of [
    { schemaVersion: 1, image: "ghcr.io/agentshelf-oss/other@sha256:" + "c".repeat(64), fixtures: [{ status: "passed" }] },
    { schemaVersion: 1, image: immutableImage, fixtures: [{ status: "failed" }] },
  ]) {
    run(manifestTool, args, directory);
    const reportBytes = `${JSON.stringify(fixtureReport)}\n`;
    await writeFile(join(directory, "historical-fixture-preflight.json"), reportBytes);
    const inconsistent = JSON.parse(await readFile(manifestPath, "utf8"));
    inconsistent.validation.historicalFixturePreflightReport.sha256 = createHash("sha256").update(reportBytes).digest("hex");
    await writeFile(manifestPath, `${JSON.stringify(inconsistent, null, 2)}\n`);
    result = spawnSync(process.execPath, [verifyTool, "release-manifest.json"], { cwd: directory, encoding: "utf8" });
    assert.equal(result.status, 1);
    assert.match(result.stderr, fixtureReport.image === immutableImage
      ? /historical fixture preflight has missing or failed cases/
      : /historical fixture preflight image does not match immutable release image/);
    await writeFile(join(directory, "historical-fixture-preflight.json"), `${fixturePreflight}\n`);
  }
});

test("source SBOM is stable for a fixed commit and timestamp", async () => {
  const directory = await mkdtemp(join(tmpdir(), "artifact-mcp-source-sbom-"));
  const output = join(directory, "source.spdx.json");
  const args = [
    "--out", output, "--commit", sourceCommit, "--version", "1.6.0", "--created", created,
    "--repository", "AgentShelf-OSS/artifact-mcp",
  ];
  run(sourceSbomTool, args, root);
  const first = await readFile(output, "utf8");
  run(sourceSbomTool, args, root);
  const second = await readFile(output, "utf8");
  assert.equal(second, first);
  const sbom = JSON.parse(first);
  assert.equal(sbom.spdxVersion, "SPDX-2.3");
  assert.equal(sbom.creationInfo.created, created);
  assert.ok(sbom.files.some((file) => file.fileName === "./Cargo.lock"));
});
